#![cfg(test)]
//! PolicyManager unit tests (Soroban test env).
//!
//! Covers: constructor wiring, the create -> activate -> claim happy path, every create-time
//! validation revert, the combined open-policy cap, role enforcement, per-org exposure
//! accounting, cancel/expire transitions, the claim-limit guard, and year rollover.

use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    Address, Env, Error, String,
};

use microcrop_shared::{
    errors::{CommonError, PolicyError},
    types::{CoverageType, PolicyStatus, Role},
};

use crate::{
    PolicyManager, PolicyManagerClient, CLAIM_WINDOW_SECS, MAX_ACTIVE_POLICIES_PER_FARMER,
    MAX_PAGE_SIZE, MAX_SUM_INSURED, MIN_SUM_INSURED, SECONDS_PER_DAY,
};

// ---- minimal PolicyNFT mock matching the IPolicyNft cross-contract interface ----
mod nft_mock {
    use soroban_sdk::{contract, contractimpl, contracttype, Address, Env, String};

    use microcrop_shared::types::CoverageType;

    #[contracttype]
    #[derive(Clone)]
    pub enum K {
        Minted(u64),
    }

    #[contract]
    pub struct NftMock;

    #[contractimpl]
    impl NftMock {
        pub fn policy_nft_exists(env: Env, policy_id: u64) -> bool {
            env.storage()
                .persistent()
                .get(&K::Minted(policy_id))
                .unwrap_or(false)
        }

        pub fn update_policy_status(
            _env: Env,
            _caller: Address,
            _policy_id: u64,
            _is_active: bool,
        ) {
            // no-op: exercises the best-effort deactivation path without reverting.
        }

        #[allow(clippy::too_many_arguments)]
        pub fn mint_policy(
            env: Env,
            _caller: Address,
            _farmer: Address,
            policy_id: u64,
            _distributor: Address,
            _distributor_name: String,
            _sum_insured: i128,
            _premium: i128,
            _start_date: u64,
            _end_date: u64,
            _coverage_type: CoverageType,
            _region: String,
            _plot_id: u64,
        ) -> u64 {
            env.storage().persistent().set(&K::Minted(policy_id), &true);
            policy_id
        }
    }
}

struct Ctx {
    env: Env,
    client: PolicyManagerClient<'static>,
    admin: Address,
    backend: Address,
    oracle: Address,
    farmer: Address,
    org: Address,
    distributor: Address,
}

fn setup() -> Ctx {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let pm_id = env.register(PolicyManager, (admin.clone(),));
    let client = PolicyManagerClient::new(&env, &pm_id);

    // Wire the NFT mock + grant roles.
    let nft_id = env.register(nft_mock::NftMock, ());
    client.set_policy_nft(&admin, &nft_id);

    let backend = Address::generate(&env);
    let oracle = Address::generate(&env);
    client.grant_role(&admin, &Role::Backend, &backend);
    client.grant_role(&admin, &Role::Oracle, &oracle);

    let farmer = Address::generate(&env);
    let org = Address::generate(&env);
    let distributor = Address::generate(&env);

    Ctx {
        env,
        client,
        admin,
        backend,
        oracle,
        farmer,
        org,
        distributor,
    }
}

fn create_default(c: &Ctx) -> u64 {
    c.client.create_policy(
        &c.backend,
        &c.farmer,
        &1u64,            // plot_id
        &MIN_SUM_INSURED, // sum_insured (100 USDC)
        &100_000_000i128, // premium (10 USDC)
        &30u32,           // duration_days
        &CoverageType::Drought,
        &c.org,
    )
}

fn activate_default(c: &Ctx, id: u64) {
    c.client.activate_policy(
        &c.backend,
        &id,
        &c.distributor,
        &String::from_str(&c.env, "Acme Distributor"),
        &String::from_str(&c.env, "Nakuru"),
    );
}

/// Assert a cross-contract call reverted with the given contract-error code, regardless of the
/// method's success return type.
fn assert_err<T: core::fmt::Debug, E: core::fmt::Debug>(
    res: Result<Result<T, E>, Result<Error, soroban_sdk::InvokeError>>,
    code: u32,
) {
    match res {
        Err(Ok(e)) => assert_eq!(e, Error::from_contract_error(code)),
        other => panic!("expected contract error {}, got {:?}", code, other),
    }
}

#[test]
fn constructor_registers() {
    let env = Env::default();
    let admin = Address::generate(&env);
    let id = env.register(PolicyManager, (admin,));
    let _client = PolicyManagerClient::new(&env, &id);
}

#[test]
fn create_activate_claim_happy_path() {
    let c = setup();

    let id = create_default(&c);
    assert_eq!(id, 1);
    assert_eq!(c.client.get_total_policies(), 1);
    assert!(c.client.policy_exists(&id));

    let p = c.client.get_policy(&id);
    assert_eq!(p.status, PolicyStatus::Pending);
    assert_eq!(p.sum_insured, MIN_SUM_INSURED);
    assert_eq!(p.org, c.org);
    // Coverage window: end == start + 30 days.
    assert_eq!(p.end_date, p.start_date + 30 * SECONDS_PER_DAY);
    // PENDING policy is not yet active, but its org exposure is counted from creation.
    assert_eq!(c.client.get_farmer_active_count(&c.farmer), 0);
    assert_eq!(
        c.client.org_outstanding_sum_insured(&c.org),
        MIN_SUM_INSURED
    );
    assert!(!c.client.is_policy_active(&id));

    // Activate.
    activate_default(&c, id);
    let p = c.client.get_policy(&id);
    assert_eq!(p.status, PolicyStatus::Active);
    assert_eq!(c.client.get_farmer_active_count(&c.farmer), 1);
    assert_eq!(
        c.client.org_outstanding_sum_insured(&c.org),
        MIN_SUM_INSURED
    );
    assert!(c.client.is_policy_active(&id));
    assert_eq!(
        c.client.get_farmer_policies(&c.farmer, &0, &10),
        soroban_sdk::vec![&c.env, 1u64]
    );

    // Claim (ORACLE).
    c.client.mark_as_claimed(&c.oracle, &id);
    let p = c.client.get_policy(&id);
    assert_eq!(p.status, PolicyStatus::Claimed);
    assert_eq!(c.client.get_farmer_active_count(&c.farmer), 0);
    // Exposure released on claim.
    assert_eq!(c.client.org_outstanding_sum_insured(&c.org), 0);
    assert!(!c.client.is_policy_active(&id));
}

#[test]
fn create_rejects_sum_too_low() {
    let c = setup();
    let res = c.client.try_create_policy(
        &c.backend,
        &c.farmer,
        &1u64,
        &(MIN_SUM_INSURED - 1),
        &100_000_000i128,
        &30u32,
        &CoverageType::Drought,
        &c.org,
    );
    assert_err(res, PolicyError::SumInsuredTooLow as u32);
}

#[test]
fn create_rejects_sum_too_high() {
    let c = setup();
    let res = c.client.try_create_policy(
        &c.backend,
        &c.farmer,
        &1u64,
        &(MAX_SUM_INSURED + 1),
        &100_000_000i128,
        &30u32,
        &CoverageType::Drought,
        &c.org,
    );
    assert_err(res, PolicyError::SumInsuredTooHigh as u32);
}

#[test]
fn create_rejects_zero_premium() {
    let c = setup();
    let res = c.client.try_create_policy(
        &c.backend,
        &c.farmer,
        &1u64,
        &MIN_SUM_INSURED,
        &0i128,
        &30u32,
        &CoverageType::Drought,
        &c.org,
    );
    assert_err(res, PolicyError::ZeroPremium as u32);
}

#[test]
fn create_rejects_bad_duration() {
    let c = setup();
    // Too short.
    let res = c.client.try_create_policy(
        &c.backend,
        &c.farmer,
        &1u64,
        &MIN_SUM_INSURED,
        &100_000_000i128,
        &29u32,
        &CoverageType::Drought,
        &c.org,
    );
    assert_err(res, PolicyError::InvalidDuration as u32);
    // Too long.
    let res = c.client.try_create_policy(
        &c.backend,
        &c.farmer,
        &1u64,
        &MIN_SUM_INSURED,
        &100_000_000i128,
        &366u32,
        &CoverageType::Drought,
        &c.org,
    );
    assert_err(res, PolicyError::InvalidDuration as u32);
}

#[test]
fn create_enforces_open_policy_cap() {
    let c = setup();
    // 3 open (PENDING) policies allowed.
    assert_eq!(MAX_ACTIVE_POLICIES_PER_FARMER, 3);
    for _ in 0..3 {
        create_default(&c);
    }
    // 6th exceeds the ACTIVE+PENDING cap.
    let res = c.client.try_create_policy(
        &c.backend,
        &c.farmer,
        &1u64,
        &MIN_SUM_INSURED,
        &100_000_000i128,
        &30u32,
        &CoverageType::Drought,
        &c.org,
    );
    assert_err(res, PolicyError::TooManyActivePolicies as u32);
}

#[test]
fn create_requires_backend_role() {
    let c = setup();
    let stranger = Address::generate(&c.env);
    let res = c.client.try_create_policy(
        &stranger,
        &c.farmer,
        &1u64,
        &MIN_SUM_INSURED,
        &100_000_000i128,
        &30u32,
        &CoverageType::Drought,
        &c.org,
    );
    assert_err(res, CommonError::Unauthorized as u32);
}

#[test]
fn mark_as_claimed_requires_oracle_role() {
    let c = setup();
    let id = create_default(&c);
    activate_default(&c, id);
    // Backend cannot mark claimed (needs ORACLE).
    let res = c.client.try_mark_as_claimed(&c.backend, &id);
    assert_err(res, CommonError::Unauthorized as u32);
}

#[test]
fn activate_rejects_non_pending() {
    let c = setup();
    let id = create_default(&c);
    activate_default(&c, id);
    // Second activation: policy no longer PENDING.
    let res = c.client.try_activate_policy(
        &c.backend,
        &id,
        &c.distributor,
        &String::from_str(&c.env, "d"),
        &String::from_str(&c.env, "r"),
    );
    assert_err(res, PolicyError::InvalidPolicyStatus as u32);
}

#[test]
fn get_policy_unknown_reverts() {
    let c = setup();
    let res = c.client.try_get_policy(&999u64);
    assert_err(res, PolicyError::PolicyDoesNotExist as u32);
}

#[test]
fn cancel_pending_releases_slot() {
    let c = setup();
    let id = create_default(&c);
    // Cancel a PENDING policy: no active count, pending slot released so farmer can create again.
    c.client.cancel_policy(&c.backend, &id);
    assert_eq!(c.client.get_policy(&id).status, PolicyStatus::Cancelled);

    // Fill the cap again to prove the PENDING slot was freed (3 more creations succeed).
    for _ in 0..3 {
        create_default(&c);
    }
    let res = c.client.try_create_policy(
        &c.backend,
        &c.farmer,
        &1u64,
        &MIN_SUM_INSURED,
        &100_000_000i128,
        &30u32,
        &CoverageType::Drought,
        &c.org,
    );
    assert_err(res, PolicyError::TooManyActivePolicies as u32);
}

#[test]
fn cancel_active_releases_exposure() {
    let c = setup();
    let id = create_default(&c);
    activate_default(&c, id);
    assert_eq!(
        c.client.org_outstanding_sum_insured(&c.org),
        MIN_SUM_INSURED
    );

    // ACTIVE cover: Admin only (finding 6).
    c.client.cancel_policy(&c.admin, &id);
    assert_eq!(c.client.get_policy(&id).status, PolicyStatus::Cancelled);
    assert_eq!(c.client.get_farmer_active_count(&c.farmer), 0);
    assert_eq!(c.client.org_outstanding_sum_insured(&c.org), 0);
}

#[test]
fn expire_before_end_reverts_then_succeeds_after() {
    let c = setup();
    let id = create_default(&c);
    activate_default(&c, id);
    let p = c.client.get_policy(&id);

    // Not yet past end date.
    let res = c.client.try_expire_policy(&id);
    assert_err(res, PolicyError::PolicyNotYetExpired as u32);
    // Exactly at end_date is still covered.
    c.env.ledger().set_timestamp(p.end_date);
    let res = c.client.try_expire_policy(&id);
    assert_err(res, PolicyError::PolicyNotYetExpired as u32);

    // Advance beyond end date.
    c.env.ledger().set_timestamp(p.end_date + 1);
    c.client.expire_policy(&id);
    assert_eq!(c.client.get_policy(&id).status, PolicyStatus::Expired);
    assert_eq!(c.client.get_farmer_active_count(&c.farmer), 0);
    assert_eq!(c.client.org_outstanding_sum_insured(&c.org), 0);
}

#[test]
fn mark_as_claimed_rejects_expired_coverage() {
    let c = setup();
    let id = create_default(&c);
    activate_default(&c, id);
    let p = c.client.get_policy(&id);
    c.env.ledger().set_timestamp(p.end_date + 1);
    let res = c.client.try_mark_as_claimed(&c.oracle, &id);
    assert_err(res, PolicyError::PolicyExpired as u32);
}

#[test]
fn claim_cap_is_a_rolling_365_day_window() {
    let c = setup();
    let t0: u64 = 1_800_000_000;
    c.env.ledger().set_timestamp(t0);

    assert!(c.client.can_farmer_claim(&c.farmer));
    for i in 0..3u64 {
        c.env.ledger().set_timestamp(t0 + i);
        c.client.increment_claim_count(&c.oracle, &c.farmer);
    }
    assert_eq!(c.client.get_farmer_claim_count(&c.farmer), 3);
    assert!(!c.client.can_farmer_claim(&c.farmer));
    let res = c.client.try_increment_claim_count(&c.oracle, &c.farmer);
    assert_err(res, PolicyError::TooManyClaimsThisYear as u32);

    // One second before the oldest claim is 365 days old: still blocked, and the view agrees.
    c.env.ledger().set_timestamp(t0 + CLAIM_WINDOW_SECS - 1);
    assert_eq!(c.client.get_farmer_claim_count(&c.farmer), 3);
    assert!(!c.client.can_farmer_claim(&c.farmer));
    let res = c.client.try_increment_claim_count(&c.oracle, &c.farmer);
    assert_err(res, PolicyError::TooManyClaimsThisYear as u32);

    // Exactly 365 days after the oldest claim: it drops out and one more claim is allowed.
    c.env.ledger().set_timestamp(t0 + CLAIM_WINDOW_SECS);
    assert_eq!(c.client.get_farmer_claim_count(&c.farmer), 2);
    assert!(c.client.can_farmer_claim(&c.farmer));
    c.client.increment_claim_count(&c.oracle, &c.farmer);
    assert_eq!(c.client.get_farmer_claim_count(&c.farmer), 3);
    assert!(!c.client.can_farmer_claim(&c.farmer));
    // The stored list stays bounded at 3, oldest first.
    let times = c.client.get_farmer_claim_times(&c.farmer);
    assert_eq!(times.len(), 3);
    assert_eq!(times.get(0).unwrap(), t0 + 1);
    assert_eq!(times.get(2).unwrap(), t0 + CLAIM_WINDOW_SECS);
}

#[test]
fn no_six_claims_across_a_calendar_bucket_edge() {
    // The old fixed bucket (timestamp / 365d) reset at 1_797_552_000: three claims just
    // before and three just after all paid. The rolling window stops the fourth.
    let c = setup();
    let edge: u64 = 1_797_552_000;
    for i in 0..3u64 {
        c.env.ledger().set_timestamp(edge - 10 + i);
        c.client.increment_claim_count(&c.oracle, &c.farmer);
    }
    c.env.ledger().set_timestamp(edge + 10);
    assert!(!c.client.can_farmer_claim(&c.farmer));
    let res = c.client.try_increment_claim_count(&c.oracle, &c.farmer);
    assert_err(res, PolicyError::TooManyClaimsThisYear as u32);
}

#[test]
fn create_counts_recent_claims_against_the_open_cap() {
    // Open policies + claims in the window <= 3, so every premium paid can still pay out.
    let c = setup();
    let t0: u64 = 1_800_000_000;
    c.env.ledger().set_timestamp(t0);

    // One policy claimed: 0 open + 1 claim.
    let a = create_default(&c);
    activate_default(&c, a);
    c.client.mark_as_claimed(&c.oracle, &a);
    c.client.increment_claim_count(&c.oracle, &c.farmer);

    // Two more fit (2 open + 1 claim = 3); a third does not.
    create_default(&c);
    create_default(&c);
    let res = c.client.try_create_policy(
        &c.backend,
        &c.farmer,
        &1u64,
        &MIN_SUM_INSURED,
        &100_000_000i128,
        &30u32,
        &CoverageType::Drought,
        &c.org,
    );
    assert_err(res, PolicyError::TooManyActivePolicies as u32);

    // Once the claim is 365 days old it no longer counts, and the slot frees up.
    c.env.ledger().set_timestamp(t0 + CLAIM_WINDOW_SECS);
    create_default(&c);
}

#[test]
fn activate_without_nft_set_reverts() {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let pm_id = env.register(PolicyManager, (admin.clone(),));
    let client = PolicyManagerClient::new(&env, &pm_id);

    let backend = Address::generate(&env);
    client.grant_role(&admin, &Role::Backend, &backend);
    let farmer = Address::generate(&env);
    let org = Address::generate(&env);
    let dist = Address::generate(&env);

    let id = client.create_policy(
        &backend,
        &farmer,
        &1u64,
        &MIN_SUM_INSURED,
        &100_000_000i128,
        &30u32,
        &CoverageType::Drought,
        &org,
    );
    // PolicyNFT never wired -> activation reverts PolicyNftNotSet.
    let res = client.try_activate_policy(
        &backend,
        &id,
        &dist,
        &String::from_str(&env, "d"),
        &String::from_str(&env, "r"),
    );
    assert_err(res, PolicyError::PolicyNftNotSet as u32);
}

#[test]
fn sum_insured_bounds_are_seven_decimal_usdc() {
    // Stellar USDC has 7 decimals: 1 USDC = 10_000_000 base units.
    const ONE_USDC: i128 = 10_000_000;
    assert_eq!(MIN_SUM_INSURED, 100 * ONE_USDC);
    assert_eq!(MAX_SUM_INSURED, 1_000_000 * ONE_USDC);

    let c = setup();
    let create = |sum: i128| {
        c.client.try_create_policy(
            &c.backend,
            &Address::generate(&c.env),
            &1u64,
            &sum,
            &100_000_000i128,
            &30u32,
            &CoverageType::Drought,
            &c.org,
        )
    };
    // Exactly 100 USDC and exactly 1,000,000 USDC are accepted.
    assert!(create(MIN_SUM_INSURED).is_ok());
    assert!(create(MAX_SUM_INSURED).is_ok());
    // The old 6dp minimum (100_000_000) is only 10 USDC at 7dp and is rejected.
    assert_err(create(100_000_000), PolicyError::SumInsuredTooLow as u32);
    assert_err(
        create(MAX_SUM_INSURED + 1),
        PolicyError::SumInsuredTooHigh as u32,
    );
}

#[test]
fn farmer_index_is_per_entry_and_paginated_past_100() {
    // Finding 10: the farmer index used to be one Vec entry rewritten on every create. It is
    // now one entry per policy, read in pages of at most MAX_PAGE_SIZE (50). Each policy is
    // cancelled while PENDING so the open-policy cap does not stop the loop.
    let c = setup();
    for _ in 0..150 {
        let id = create_default(&c);
        c.client.cancel_policy(&c.backend, &id);
    }
    assert_eq!(c.client.get_farmer_policy_count(&c.farmer), 150);
    assert_eq!(MAX_PAGE_SIZE, 50);

    let mut seen = 0u64;
    let mut start = 0u32;
    while start < 150 {
        let page = c.client.get_farmer_policies(&c.farmer, &start, &50);
        assert_eq!(page.len(), 50);
        for id in page.iter() {
            seen += 1;
            assert_eq!(id, seen);
        }
        start += 50;
    }
    assert_eq!(seen, 150);

    assert_eq!(c.client.get_farmer_policies(&c.farmer, &0, &100).len(), 50);
    assert_eq!(
        c.client
            .get_farmer_policies(&c.farmer, &140, &u32::MAX)
            .len(),
        10
    );
    assert_eq!(c.client.get_farmer_policies(&c.farmer, &150, &10).len(), 0);
}

// ---------------- Finding 4: exposure counted at create ----------------

#[test]
fn cancel_pending_releases_exposure_exactly_once() {
    let c = setup();
    let a = create_default(&c);
    let b = create_default(&c);
    assert_eq!(
        c.client.org_outstanding_sum_insured(&c.org),
        2 * MIN_SUM_INSURED
    );

    c.client.cancel_policy(&c.backend, &a);
    assert_eq!(
        c.client.org_outstanding_sum_insured(&c.org),
        MIN_SUM_INSURED
    );
    // A second cancel of the same policy reverts and releases nothing more.
    let res = c.client.try_cancel_policy(&c.backend, &a);
    assert_err(res, PolicyError::InvalidPolicyStatus as u32);
    assert_eq!(
        c.client.org_outstanding_sum_insured(&c.org),
        MIN_SUM_INSURED
    );

    // Activation does not add the exposure a second time.
    activate_default(&c, b);
    assert_eq!(
        c.client.org_outstanding_sum_insured(&c.org),
        MIN_SUM_INSURED
    );
}

// ---------------- Finding 6: Backend cancels PENDING only ----------------

#[test]
fn backend_cannot_cancel_active_policy() {
    let c = setup();
    let id = create_default(&c);
    activate_default(&c, id);
    let res = c.client.try_cancel_policy(&c.backend, &id);
    assert_err(res, PolicyError::CancelActiveRequiresAdmin as u32);
    // Nothing changed.
    assert_eq!(c.client.get_policy(&id).status, PolicyStatus::Active);
    assert_eq!(c.client.get_farmer_active_count(&c.farmer), 1);
    assert_eq!(
        c.client.org_outstanding_sum_insured(&c.org),
        MIN_SUM_INSURED
    );
}

#[test]
fn admin_can_cancel_pending_and_active() {
    let c = setup();
    let pending = create_default(&c);
    let active = create_default(&c);
    activate_default(&c, active);
    c.client.cancel_policy(&c.admin, &pending);
    c.client.cancel_policy(&c.admin, &active);
    assert_eq!(
        c.client.get_policy(&pending).status,
        PolicyStatus::Cancelled
    );
    assert_eq!(c.client.get_policy(&active).status, PolicyStatus::Cancelled);
    assert_eq!(c.client.org_outstanding_sum_insured(&c.org), 0);
}

#[test]
fn stranger_cannot_cancel() {
    let c = setup();
    let id = create_default(&c);
    let res = c.client.try_cancel_policy(&Address::generate(&c.env), &id);
    assert_err(res, CommonError::Unauthorized as u32);
}

// ---------------- Finding 7: expiry is permissionless + idempotent ----------------

#[test]
fn expire_needs_no_role_and_no_auth() {
    let c = setup();
    let id = create_default(&c);
    activate_default(&c, id);
    let p = c.client.get_policy(&id);
    c.env.ledger().set_timestamp(p.end_date + 1);

    // Drop every mocked authorization: if expire_policy called require_auth anywhere, this
    // would fail.
    c.env.set_auths(&[]);
    c.client.expire_policy(&id);
    assert_eq!(c.client.get_policy(&id).status, PolicyStatus::Expired);
    assert_eq!(c.client.org_outstanding_sum_insured(&c.org), 0);
}

#[test]
fn expire_is_idempotent() {
    let c = setup();
    let a = create_default(&c);
    let b = create_default(&c);
    activate_default(&c, a);
    activate_default(&c, b);
    let p = c.client.get_policy(&a);
    c.env.ledger().set_timestamp(p.end_date + 1);

    c.client.expire_policy(&a);
    let outstanding = c.client.org_outstanding_sum_insured(&c.org);
    let active = c.client.get_farmer_active_count(&c.farmer);
    assert_eq!(outstanding, MIN_SUM_INSURED); // b is still counted
    assert_eq!(active, 1);

    // Second call: no error, no second release.
    c.client.expire_policy(&a);
    assert_eq!(c.client.get_policy(&a).status, PolicyStatus::Expired);
    assert_eq!(c.client.org_outstanding_sum_insured(&c.org), outstanding);
    assert_eq!(c.client.get_farmer_active_count(&c.farmer), active);
}

#[test]
fn expire_rejects_pending_and_closed_policies() {
    let c = setup();
    let pending = create_default(&c);
    let p = c.client.get_policy(&pending);
    c.env.ledger().set_timestamp(p.end_date + 1);
    assert_err(
        c.client.try_expire_policy(&pending),
        PolicyError::InvalidPolicyStatus as u32,
    );
    c.client.cancel_policy(&c.backend, &pending);
    assert_err(
        c.client.try_expire_policy(&pending),
        PolicyError::InvalidPolicyStatus as u32,
    );
}

// ---------------- Findings 9/11: CLAIM_PENDING ----------------

#[test]
fn claim_pending_policy_cannot_lapse_or_be_cancelled_and_settles_after_end_date() {
    let c = setup();
    let id = create_default(&c);
    activate_default(&c, id);
    let p = c.client.get_policy(&id);

    c.client.mark_claim_pending(&c.oracle, &id);
    assert_eq!(c.client.get_policy(&id).status, PolicyStatus::ClaimPending);
    // Still holds its slot and its exposure until settled.
    assert_eq!(c.client.get_farmer_active_count(&c.farmer), 1);
    assert_eq!(
        c.client.org_outstanding_sum_insured(&c.org),
        MIN_SUM_INSURED
    );

    // Past end_date: cannot be expired, cannot be cancelled (not even by Admin).
    c.env.ledger().set_timestamp(p.end_date + 1_000);
    assert_err(
        c.client.try_expire_policy(&id),
        PolicyError::InvalidPolicyStatus as u32,
    );
    assert_err(
        c.client.try_cancel_policy(&c.admin, &id),
        PolicyError::InvalidPolicyStatus as u32,
    );
    // A second mark_claim_pending is refused.
    assert_err(
        c.client.try_mark_claim_pending(&c.oracle, &id),
        PolicyError::InvalidPolicyStatus as u32,
    );

    // Settlement after end_date still works and releases slot + exposure exactly once.
    c.client.mark_as_claimed(&c.oracle, &id);
    assert_eq!(c.client.get_policy(&id).status, PolicyStatus::Claimed);
    assert_eq!(c.client.get_farmer_active_count(&c.farmer), 0);
    assert_eq!(c.client.org_outstanding_sum_insured(&c.org), 0);
    assert_err(
        c.client.try_mark_as_claimed(&c.oracle, &id),
        PolicyError::InvalidPolicyStatus as u32,
    );
}

#[test]
fn mark_claim_pending_requires_oracle_and_live_active_cover() {
    let c = setup();
    let id = create_default(&c);
    // PENDING (not activated) cannot become claim-pending.
    assert_err(
        c.client.try_mark_claim_pending(&c.oracle, &id),
        PolicyError::InvalidPolicyStatus as u32,
    );
    activate_default(&c, id);
    assert_err(
        c.client.try_mark_claim_pending(&c.backend, &id),
        CommonError::Unauthorized as u32,
    );
    let p = c.client.get_policy(&id);
    c.env.ledger().set_timestamp(p.end_date + 1);
    assert_err(
        c.client.try_mark_claim_pending(&c.oracle, &id),
        PolicyError::PolicyExpired as u32,
    );
}
