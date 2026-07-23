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
    PolicyManager, PolicyManagerClient, MAX_SUM_INSURED, MIN_SUM_INSURED, SECONDS_PER_DAY,
    SECONDS_PER_YEAR,
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
        &1u64,             // plot_id
        &MIN_SUM_INSURED,  // sum_insured (100 USDC)
        &10_000_000i128,   // premium (10 USDC)
        &30u32,            // duration_days
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
    // PENDING policy is not yet active; org exposure not counted.
    assert_eq!(c.client.get_farmer_active_count(&c.farmer), 0);
    assert_eq!(c.client.org_outstanding_sum_insured(&c.org), 0);
    assert!(!c.client.is_policy_active(&id));

    // Activate.
    activate_default(&c, id);
    let p = c.client.get_policy(&id);
    assert_eq!(p.status, PolicyStatus::Active);
    assert_eq!(c.client.get_farmer_active_count(&c.farmer), 1);
    assert_eq!(c.client.org_outstanding_sum_insured(&c.org), MIN_SUM_INSURED);
    assert!(c.client.is_policy_active(&id));
    assert_eq!(c.client.get_farmer_policies(&c.farmer), soroban_sdk::vec![&c.env, 1u64]);

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
        &10_000_000i128,
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
        &10_000_000i128,
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
        &10_000_000i128,
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
        &10_000_000i128,
        &366u32,
        &CoverageType::Drought,
        &c.org,
    );
    assert_err(res, PolicyError::InvalidDuration as u32);
}

#[test]
fn create_enforces_open_policy_cap() {
    let c = setup();
    // 5 open (PENDING) policies allowed.
    for _ in 0..5 {
        create_default(&c);
    }
    // 6th exceeds the ACTIVE+PENDING cap.
    let res = c.client.try_create_policy(
        &c.backend,
        &c.farmer,
        &1u64,
        &MIN_SUM_INSURED,
        &10_000_000i128,
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
        &10_000_000i128,
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

    // Fill the cap again to prove the PENDING slot was freed (5 more creations succeed).
    for _ in 0..5 {
        create_default(&c);
    }
    let res = c.client.try_create_policy(
        &c.backend,
        &c.farmer,
        &1u64,
        &MIN_SUM_INSURED,
        &10_000_000i128,
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
    assert_eq!(c.client.org_outstanding_sum_insured(&c.org), MIN_SUM_INSURED);

    c.client.cancel_policy(&c.backend, &id);
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
    let res = c.client.try_expire_policy(&c.backend, &id);
    assert_err(res, PolicyError::PolicyNotYetExpired as u32);

    // Advance beyond end date.
    c.env.ledger().set_timestamp(p.end_date + 1);
    c.client.expire_policy(&c.backend, &id);
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
fn claim_count_limit_and_year_rollover() {
    let c = setup();
    // Year 2.
    c.env.ledger().set_timestamp(2 * SECONDS_PER_YEAR + 100);
    let year2 = c.client.get_current_year();
    assert_eq!(year2, 2);

    assert!(c.client.can_farmer_claim(&c.farmer));
    for _ in 0..3 {
        c.client.increment_claim_count(&c.oracle, &c.farmer);
    }
    assert_eq!(c.client.get_farmer_claim_count(&c.farmer, &year2), 3);
    assert!(!c.client.can_farmer_claim(&c.farmer));

    // 4th within the same year exceeds the limit.
    let res = c.client.try_increment_claim_count(&c.oracle, &c.farmer);
    assert_err(res, PolicyError::TooManyClaimsThisYear as u32);

    // Roll to year 3 — counter resets.
    c.env.ledger().set_timestamp(3 * SECONDS_PER_YEAR + 100);
    assert_eq!(c.client.get_current_year(), 3);
    assert!(c.client.can_farmer_claim(&c.farmer));
    assert_eq!(c.client.get_farmer_claim_count(&c.farmer, &3u64), 0);
    c.client.increment_claim_count(&c.oracle, &c.farmer);
    assert_eq!(c.client.get_farmer_claim_count(&c.farmer, &3u64), 1);
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
        &10_000_000i128,
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
