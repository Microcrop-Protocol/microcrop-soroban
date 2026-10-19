//! SettlementAdapter unit tests.
//!
//! Auth is tested with `set_auths(&[])` and exact `mock_auths`, not only `mock_all_auths`,
//! and the security-critical trees are asserted with `env.auths()` (testing.md,
//! "Authorization"). Events are compared with `to_xdr` (testing.md, "Events"), TTLs with
//! `get_ttl` (testing.md, "Storage TTL"), and the state machine is driven by a property test
//! (testing.md, "Property-based testing").

extern crate std;

use soroban_sdk::{
    testutils::{
        storage::{Instance as _, Persistent as _},
        Address as _, AuthorizedFunction, AuthorizedInvocation, Events as _, Ledger as _, MockAuth,
        MockAuthInvoke,
    },
    Address, BytesN, Env, Error, Event as _, IntoVal, Symbol,
};

use microcrop_shared::{
    errors::CommonError,
    ttl::{
        DAY_LEDGERS, KEEPER_TARGET_LEDGERS, NETWORK_MAX_ENTRY_TTL_LEDGERS, PENDING_TARGET_LEDGERS,
        SHARED_FLOOR_LEDGERS, USER_TARGET_LEDGERS,
    },
    types::{Obligation, ObligationStatus, Role},
};

use crate::{
    DataKey, ObligationFunded, ObligationRecorded, ObligationSettled, PayoutReceiverSet,
    SettlementAdapter, SettlementAdapterClient, MAX_PAGE_SIZE,
};

const NOW: u64 = 1_700_000_000;
const AMOUNT: i128 = 5_000_000_000; // 500 USDC at 7dp

struct F {
    env: Env,
    id: Address,
    c: SettlementAdapterClient<'static>,
    admin: Address,
    pr: Address,
    org: Address,
    funder: Address,
}

fn fresh_env() -> Env {
    env_with(Env::default())
}

fn env_with(env: Env) -> Env {
    env.ledger().set_timestamp(NOW);
    env.ledger()
        .set_max_entry_ttl(NETWORK_MAX_ENTRY_TTL_LEDGERS);
    env
}

/// An adapter bound to `pr` (a plain address standing in for the PayoutReceiver) with
/// `funder` holding `Role::Funder`. Leaves `mock_all_auths` on.
fn setup() -> F {
    setup_in(fresh_env())
}

fn setup_in(env: Env) -> F {
    let admin = Address::generate(&env);
    let id = env.register(SettlementAdapter, (admin.clone(),));
    let c = SettlementAdapterClient::new(&env, &id);
    let pr = Address::generate(&env);
    let org = Address::generate(&env);
    let funder = Address::generate(&env);
    env.mock_all_auths();
    c.set_payout_receiver(&admin, &pr);
    c.grant_role(&admin, &Role::Funder, &funder);
    F {
        env,
        id,
        c,
        admin,
        pr,
        org,
        funder,
    }
}

fn digest(env: &Env, b: u8) -> BytesN<32> {
    BytesN::from_array(env, &[b; 32])
}

fn reference(env: &Env, b: u8) -> BytesN<32> {
    BytesN::from_array(env, &[b; 32])
}

fn record(f: &F, policy_id: u64) -> Obligation {
    f.c.record_obligation(&f.pr, &policy_id, &f.org, &AMOUNT, &digest(&f.env, 0xD1))
}

fn contract_err<T: core::fmt::Debug>(
    res: Result<T, Result<Error, soroban_sdk::InvokeError>>,
    code: u32,
) {
    match res {
        Err(Ok(e)) => assert_eq!(e, Error::from_contract_error(code), "wrong error"),
        other => panic!("expected contract error #{code}, got {:?}", other),
    }
}

/// A missing signature surfaces as a host (auth) error, never as a contract error.
fn host_auth_err<T: core::fmt::Debug>(res: Result<T, Result<Error, soroban_sdk::InvokeError>>) {
    match res {
        Err(Ok(e)) => assert!(
            !e.is_type(soroban_sdk::xdr::ScErrorType::Contract),
            "expected a host auth error, got {:?}",
            e
        ),
        other => panic!("expected a host auth error, got {:?}", other),
    }
}

fn ttl(f: &F, k: &DataKey) -> u32 {
    f.env
        .as_contract(&f.id, || f.env.storage().persistent().get_ttl(k))
}

fn inst_ttl(f: &F) -> u32 {
    f.env
        .as_contract(&f.id, || f.env.storage().instance().get_ttl())
}

fn advance_days(env: &Env, days: u32) {
    let seq = env.ledger().sequence();
    env.ledger().set_sequence_number(seq + days * DAY_LEDGERS);
    let ts = env.ledger().timestamp();
    env.ledger().set_timestamp(ts + days as u64 * 86_400);
}

// ============ 1-2. constructor and binding ============

#[test]
fn constructor_grants_admin_and_upgrader_and_leaves_receiver_unset() {
    let env = fresh_env();
    let admin = Address::generate(&env);
    let id = env.register(SettlementAdapter, (admin.clone(),));
    let c = SettlementAdapterClient::new(&env, &id);
    assert!(c.has_role(&Role::Admin, &admin));
    assert!(c.has_role(&Role::Upgrader, &admin));
    assert!(!c.has_role(&Role::Funder, &admin));
    assert_eq!(c.payout_receiver(), None);
    assert_eq!(c.obligation_count(), 0);
    let next: u64 = env.as_contract(&id, || {
        env.storage()
            .instance()
            .get(&DataKey::NextObligationId)
            .unwrap()
    });
    assert_eq!(next, 1);
}

#[test]
fn record_before_the_receiver_is_set_is_not_configured() {
    let env = fresh_env();
    let admin = Address::generate(&env);
    let id = env.register(SettlementAdapter, (admin,));
    let c = SettlementAdapterClient::new(&env, &id);
    env.mock_all_auths();
    let pr = Address::generate(&env);
    let org = Address::generate(&env);
    contract_err(
        c.try_record_obligation(&pr, &1, &org, &AMOUNT, &digest(&env, 1)),
        CommonError::NotConfigured as u32,
    );
    assert_eq!(c.obligation_count(), 0);
}

// ============ 3-4. record ============

#[test]
fn record_by_the_receiver_creates_an_owed_obligation_with_exact_fields() {
    let f = setup();
    let ob = record(&f, 42);
    let expected = Obligation {
        id: 1,
        payout_receiver: f.pr.clone(),
        policy_id: 42,
        org: f.org.clone(),
        amount: AMOUNT,
        determination_digest: digest(&f.env, 0xD1),
        status: ObligationStatus::Owed,
        recorded_at: NOW,
        org_index: 0,
        funded_by: None,
        funded_at: 0,
        funding_reference: None,
        settled_by: None,
        settled_at: 0,
        settlement_reference: None,
    };
    assert_eq!(ob, expected);
    assert_eq!(f.c.get_obligation(&1), Some(expected));
    assert_eq!(f.c.find_obligation(&f.pr, &42), Some(1));
    assert_eq!(f.c.find_obligation(&f.pr, &43), None);
    assert_eq!(f.c.obligation_count(), 1);
    assert_eq!(f.c.org_obligation_count(&f.org), 1);
    assert_eq!(
        f.c.get_org_obligations(&f.org, &0, &10),
        soroban_sdk::vec![&f.env, 1u64]
    );

    // A second policy of the same org gets the next id and index.
    let ob2 = record(&f, 43);
    assert_eq!((ob2.id, ob2.org_index), (2, 1));
    assert_eq!(f.c.org_obligation_count(&f.org), 2);
}

#[test]
fn record_emits_obligation_recorded() {
    let f = setup();
    record(&f, 7);
    let expected = ObligationRecorded {
        obligation_id: 1,
        policy_id: 7,
        org: f.org.clone(),
        payout_receiver: f.pr.clone(),
        amount: AMOUNT,
        determination_digest: digest(&f.env, 0xD1),
    };
    assert_eq!(
        f.env.events().all(),
        std::vec![expected.to_xdr(&f.env, &f.id)]
    );
}

// ============ 5-6. who may record ============

#[test]
fn a_caller_other_than_the_receiver_is_refused_even_with_every_auth_mocked() {
    let f = setup();
    let stranger = Address::generate(&f.env);
    contract_err(
        f.c.try_record_obligation(&stranger, &1, &f.org, &AMOUNT, &digest(&f.env, 1)),
        500,
    );
    // The org itself cannot record its own obligations either.
    contract_err(
        f.c.try_record_obligation(&f.org, &1, &f.org, &AMOUNT, &digest(&f.env, 1)),
        500,
    );
    assert_eq!(f.c.obligation_count(), 0);
}

#[test]
fn record_needs_the_receivers_own_authorization() {
    let f = setup();
    let d = digest(&f.env, 1);

    // Naming the receiver as the caller without its signature fails.
    f.env.set_auths(&[]);
    host_auth_err(f.c.try_record_obligation(&f.pr, &1, &f.org, &AMOUNT, &d));
    assert_eq!(f.c.obligation_count(), 0);

    // A signature by someone else (here the admin) does not help either.
    host_auth_err(
        f.c.mock_auths(&[MockAuth {
            address: &f.admin,
            invoke: &MockAuthInvoke {
                contract: &f.id,
                fn_name: "record_obligation",
                args: (f.pr.clone(), 1u64, f.org.clone(), AMOUNT, d.clone()).into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .try_record_obligation(&f.pr, &1, &f.org, &AMOUNT, &d),
    );

    // With exactly the receiver's authorization for exactly these arguments it succeeds,
    // and the auth tree is exactly that one invocation.
    let args = (f.pr.clone(), 1u64, f.org.clone(), AMOUNT, d.clone()).into_val(&f.env);
    f.c.mock_auths(&[MockAuth {
        address: &f.pr,
        invoke: &MockAuthInvoke {
            contract: &f.id,
            fn_name: "record_obligation",
            args: (f.pr.clone(), 1u64, f.org.clone(), AMOUNT, d.clone()).into_val(&f.env),
            sub_invokes: &[],
        },
    }])
    .record_obligation(&f.pr, &1, &f.org, &AMOUNT, &d);
    assert_eq!(
        f.env.auths(),
        std::vec![(
            f.pr.clone(),
            AuthorizedInvocation {
                function: AuthorizedFunction::Contract((
                    f.id.clone(),
                    Symbol::new(&f.env, "record_obligation"),
                    args,
                )),
                sub_invocations: std::vec![],
            }
        )]
    );
    assert_eq!(f.c.obligation_count(), 1);
}

// ============ 7-10. amounts, idempotency, conflicts, namespaces ============

#[test]
fn non_positive_amounts_are_refused() {
    let f = setup();
    for amount in [0i128, -1, i128::MIN] {
        contract_err(
            f.c.try_record_obligation(&f.pr, &1, &f.org, &amount, &digest(&f.env, 1)),
            501,
        );
    }
    // The smallest and largest positive amounts are accepted (no token, no arithmetic).
    f.c.record_obligation(&f.pr, &1, &f.org, &1, &digest(&f.env, 1));
    f.c.record_obligation(&f.pr, &2, &f.org, &i128::MAX, &digest(&f.env, 2));
    assert_eq!(f.c.obligation_count(), 2);
}

#[test]
fn an_identical_replay_is_a_no_op() {
    let f = setup();
    let first = record(&f, 9);
    let again = record(&f, 9);
    assert_eq!(again, first);
    // No event, no new id, no new index item.
    assert_eq!(f.env.events().all().events().len(), 0);
    assert_eq!(f.c.obligation_count(), 1);
    assert_eq!(f.c.org_obligation_count(&f.org), 1);
}

#[test]
fn a_conflicting_replay_is_refused() {
    let f = setup();
    record(&f, 9);
    let other_org = Address::generate(&f.env);
    contract_err(
        f.c.try_record_obligation(&f.pr, &9, &f.org, &(AMOUNT + 1), &digest(&f.env, 0xD1)),
        502,
    );
    contract_err(
        f.c.try_record_obligation(&f.pr, &9, &f.org, &AMOUNT, &digest(&f.env, 0xD2)),
        502,
    );
    contract_err(
        f.c.try_record_obligation(&f.pr, &9, &other_org, &AMOUNT, &digest(&f.env, 0xD1)),
        502,
    );
    assert_eq!(f.c.obligation_count(), 1);
}

#[test]
fn the_same_policy_id_from_a_rotated_receiver_is_a_distinct_obligation() {
    let f = setup();
    record(&f, 5);
    let pr2 = Address::generate(&f.env);
    f.c.set_payout_receiver(&f.admin, &pr2);
    let ob =
        f.c.record_obligation(&pr2, &5, &f.org, &AMOUNT, &digest(&f.env, 0xD1));
    assert_eq!(ob.id, 2);
    assert_eq!(ob.payout_receiver, pr2);
    assert_eq!(f.c.find_obligation(&f.pr, &5), Some(1));
    assert_eq!(f.c.find_obligation(&pr2, &5), Some(2));
}

// ============ 11-16. marks ============

#[test]
fn the_org_and_the_funder_may_mark_and_nobody_else() {
    let f = setup();
    record(&f, 1);
    record(&f, 2);
    let r = reference(&f.env, 0xF1);

    // Strangers, the PayoutReceiver and another org are refused.
    let stranger = Address::generate(&f.env);
    let other_org = Address::generate(&f.env);
    for who in [&stranger, &f.pr, &other_org, &f.admin] {
        contract_err(
            f.c.try_mark_funded(who, &1, &r),
            CommonError::Unauthorized as u32,
        );
        contract_err(
            f.c.try_mark_settled(who, &1, &r),
            CommonError::Unauthorized as u32,
        );
    }

    // The org needs its own signature.
    host_auth_err(
        f.c.mock_auths(&[MockAuth {
            address: &f.funder,
            invoke: &MockAuthInvoke {
                contract: &f.id,
                fn_name: "mark_funded",
                args: (f.org.clone(), 1u64, r.clone()).into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .try_mark_funded(&f.org, &1, &r),
    );

    // The org, signing for exactly this call.
    let args = (f.org.clone(), 1u64, r.clone()).into_val(&f.env);
    let ob =
        f.c.mock_auths(&[MockAuth {
            address: &f.org,
            invoke: &MockAuthInvoke {
                contract: &f.id,
                fn_name: "mark_funded",
                args: (f.org.clone(), 1u64, r.clone()).into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .mark_funded(&f.org, &1, &r);
    assert_eq!(
        f.env.auths(),
        std::vec![(
            f.org.clone(),
            AuthorizedInvocation {
                function: AuthorizedFunction::Contract((
                    f.id.clone(),
                    Symbol::new(&f.env, "mark_funded"),
                    args
                )),
                sub_invocations: std::vec![],
            }
        )]
    );
    assert_eq!(ob.status, ObligationStatus::Funded);
    assert_eq!(ob.funded_by, Some(f.org.clone()));
    assert_eq!(ob.funded_at, NOW);
    assert_eq!(ob.funding_reference, Some(r.clone()));

    // The Funder, for any org's obligation.
    let ob2 = f.c.mark_funded(&f.funder, &2, &r);
    assert_eq!(ob2.funded_by, Some(f.funder.clone()));
    let expected = ObligationFunded {
        obligation_id: 2,
        policy_id: 2,
        org: f.org.clone(),
        by: f.funder.clone(),
        reference_hash: r.clone(),
    };
    assert_eq!(
        f.env.events().all(),
        std::vec![expected.to_xdr(&f.env, &f.id)]
    );
}

#[test]
fn a_zero_reference_is_refused() {
    let f = setup();
    record(&f, 1);
    let zero = BytesN::from_array(&f.env, &[0u8; 32]);
    contract_err(f.c.try_mark_funded(&f.funder, &1, &zero), 506);
    contract_err(f.c.try_mark_settled(&f.funder, &1, &zero), 506);
    assert_eq!(
        f.c.get_obligation(&1).unwrap().status,
        ObligationStatus::Owed
    );
}

#[test]
fn funding_twice_is_a_no_op_with_the_same_reference_and_refused_with_another() {
    let f = setup();
    record(&f, 1);
    let r = reference(&f.env, 0xF1);
    let first = f.c.mark_funded(&f.funder, &1, &r);
    let again = f.c.mark_funded(&f.funder, &1, &r);
    assert_eq!(again, first);
    assert_eq!(f.env.events().all().events().len(), 0);
    // Even the org cannot overwrite the funder's reference.
    contract_err(
        f.c.try_mark_funded(&f.org, &1, &reference(&f.env, 0xF2)),
        504,
    );
    assert_eq!(f.c.get_obligation(&1).unwrap(), first);
}

#[test]
fn settlement_from_owed_and_from_funded_and_its_immutability() {
    let f = setup();
    record(&f, 1);
    record(&f, 2);
    let rf = reference(&f.env, 0xF1);
    let rs = reference(&f.env, 0x51);

    // Owed -> Settled directly (funding implicit).
    let s1 = f.c.mark_settled(&f.funder, &1, &rs);
    assert_eq!(s1.status, ObligationStatus::Settled);
    assert_eq!(s1.funded_by, None);
    assert_eq!(s1.settled_by, Some(f.funder.clone()));
    assert_eq!(s1.settled_at, NOW);
    assert_eq!(s1.settlement_reference, Some(rs.clone()));
    let expected = ObligationSettled {
        obligation_id: 1,
        policy_id: 1,
        org: f.org.clone(),
        by: f.funder.clone(),
        reference_hash: rs.clone(),
        amount: AMOUNT,
    };
    assert_eq!(
        f.env.events().all(),
        std::vec![expected.to_xdr(&f.env, &f.id)]
    );

    // Owed -> Funded -> Settled, by the org.
    f.c.mark_funded(&f.funder, &2, &rf);
    f.env.ledger().set_timestamp(NOW + 100);
    let s2 = f.c.mark_settled(&f.org, &2, &rs);
    assert_eq!(s2.status, ObligationStatus::Settled);
    assert_eq!(s2.funding_reference, Some(rf.clone()));
    assert_eq!(s2.funded_at, NOW);
    assert_eq!(s2.settled_at, NOW + 100);

    // Same reference: no-op. Another reference: refused. Funding after settlement: refused.
    assert_eq!(f.c.mark_settled(&f.funder, &1, &rs), s1);
    assert_eq!(f.env.events().all().events().len(), 0);
    contract_err(f.c.try_mark_settled(&f.funder, &1, &rf), 505);
    contract_err(f.c.try_mark_funded(&f.funder, &1, &rf), 505);
    contract_err(f.c.try_mark_funded(&f.funder, &1, &rs), 505);
    assert_eq!(f.c.get_obligation(&1).unwrap(), s1);
}

#[test]
fn an_unknown_obligation_is_not_found() {
    let f = setup();
    let r = reference(&f.env, 1);
    contract_err(f.c.try_mark_funded(&f.funder, &1, &r), 503);
    contract_err(f.c.try_mark_settled(&f.funder, &77, &r), 503);
    assert_eq!(f.c.get_obligation(&1), None);
}

#[test]
fn a_revoked_funder_is_refused() {
    let f = setup();
    record(&f, 1);
    f.c.revoke_role(&f.admin, &Role::Funder, &f.funder);
    assert!(!f.c.has_role(&Role::Funder, &f.funder));
    contract_err(
        f.c.try_mark_funded(&f.funder, &1, &reference(&f.env, 1)),
        CommonError::Unauthorized as u32,
    );
    // Only the admin manages roles.
    let stranger = Address::generate(&f.env);
    contract_err(
        f.c.try_grant_role(&stranger, &Role::Funder, &stranger),
        CommonError::Unauthorized as u32,
    );
    contract_err(
        f.c.try_revoke_role(&f.funder, &Role::Admin, &f.admin),
        CommonError::Unauthorized as u32,
    );
}

// ============ 17. receiver binding ============

#[test]
fn set_payout_receiver_is_admin_only_signed_and_rotates_fail_closed() {
    let f = setup();
    record(&f, 1);

    let stranger = Address::generate(&f.env);
    let pr2 = Address::generate(&f.env);
    contract_err(
        f.c.try_set_payout_receiver(&stranger, &pr2),
        CommonError::Unauthorized as u32,
    );
    contract_err(f.c.try_set_payout_receiver(&f.admin, &f.id), 507);

    // The admin needs to sign.
    f.env.set_auths(&[]);
    host_auth_err(f.c.try_set_payout_receiver(&f.admin, &pr2));
    assert_eq!(f.c.payout_receiver(), Some(f.pr.clone()));

    f.env.mock_all_auths();
    f.c.set_payout_receiver(&f.admin, &pr2);
    let expected = PayoutReceiverSet {
        previous: Some(f.pr.clone()),
        payout_receiver: pr2.clone(),
    };
    assert_eq!(
        f.env.events().all(),
        std::vec![expected.to_xdr(&f.env, &f.id)]
    );
    assert_eq!(f.c.payout_receiver(), Some(pr2.clone()));

    // The old receiver can no longer record; its obligations stay markable.
    contract_err(
        f.c.try_record_obligation(&f.pr, &2, &f.org, &AMOUNT, &digest(&f.env, 2)),
        500,
    );
    let ob = f.c.mark_settled(&f.funder, &1, &reference(&f.env, 9));
    assert_eq!(ob.status, ObligationStatus::Settled);
    assert_eq!(ob.payout_receiver, f.pr);
}

#[test]
fn the_first_binding_has_no_previous_receiver() {
    let env = fresh_env();
    let admin = Address::generate(&env);
    let id = env.register(SettlementAdapter, (admin.clone(),));
    let c = SettlementAdapterClient::new(&env, &id);
    let pr = Address::generate(&env);
    env.mock_all_auths();
    c.set_payout_receiver(&admin, &pr);
    let expected = PayoutReceiverSet {
        previous: None,
        payout_receiver: pr,
    };
    assert_eq!(env.events().all(), std::vec![expected.to_xdr(&env, &id)]);
}

// ============ 18. org index ============

#[test]
fn org_index_paginates_with_a_page_cap() {
    let f = setup();
    f.env.cost_estimate().budget().reset_unlimited();
    for p in 1..=120u64 {
        record(&f, p);
    }
    // Interleave another org's obligation: it must not appear in this org's index.
    let other = Address::generate(&f.env);
    f.c.record_obligation(&f.pr, &999, &other, &AMOUNT, &digest(&f.env, 3));

    assert_eq!(f.c.org_obligation_count(&f.org), 120);
    assert_eq!(f.c.org_obligation_count(&other), 1);
    let page = f.c.get_org_obligations(&f.org, &0, &1_000);
    assert_eq!(page.len(), MAX_PAGE_SIZE);
    assert_eq!(page.get(0), Some(1));
    assert_eq!(page.get(49), Some(50));
    let page = f.c.get_org_obligations(&f.org, &100, &50);
    assert_eq!(page.len(), 20);
    assert_eq!(page.get(19), Some(120));
    assert_eq!(f.c.get_org_obligations(&f.org, &120, &10).len(), 0);
    assert_eq!(
        f.c.get_org_obligations(&f.org, &u32::MAX, &u32::MAX).len(),
        0
    );
    assert_eq!(f.c.get_org_obligations(&f.org, &5, &0).len(), 0);
    assert_eq!(
        f.c.get_org_obligations(&other, &0, &10),
        soroban_sdk::vec![&f.env, 121u64]
    );
}

// ============ 19-20. TTL ============

#[test]
fn ttls_follow_the_rent_cap() {
    let f = setup();
    let ob = record(&f, 1);
    let keys = [
        DataKey::Obligation(1),
        DataKey::ObligationOf(f.pr.clone(), 1),
        DataKey::OrgObligationAt(f.org.clone(), ob.org_index),
    ];
    for k in &keys {
        assert_eq!(ttl(&f, k), PENDING_TARGET_LEDGERS);
    }
    assert_eq!(
        ttl(&f, &DataKey::OrgObligationCount(f.org.clone())),
        SHARED_FLOOR_LEDGERS
    );
    // The user path never extends the instance past 10 days.
    assert_eq!(inst_ttl(&f), USER_TARGET_LEDGERS);

    // The keeper takes the instance to 30 days.
    f.env.set_auths(&[]);
    f.c.extend_contract_ttl();
    assert_eq!(inst_ttl(&f), KEEPER_TARGET_LEDGERS);
    f.env.mock_all_auths();

    // 35 days on (keeping the instance alive), a mark re-extends to 60 days.
    advance_days(&f.env, 20);
    f.c.extend_contract_ttl();
    advance_days(&f.env, 15);
    for k in &keys {
        assert_eq!(ttl(&f, k), PENDING_TARGET_LEDGERS - 35 * DAY_LEDGERS);
    }
    f.c.mark_funded(&f.funder, &1, &reference(&f.env, 1));
    for k in &keys {
        assert_eq!(ttl(&f, k), PENDING_TARGET_LEDGERS);
    }
    // A user call below the keeper threshold leaves the instance where the keeper put it.
    assert!(inst_ttl(&f) <= KEEPER_TARGET_LEDGERS);

    // The permissionless keeper extends an open obligation...
    advance_days(&f.env, 15);
    f.c.extend_contract_ttl();
    advance_days(&f.env, 20);
    f.env.set_auths(&[]);
    f.c.extend_obligation_ttl(&1);
    for k in &keys {
        assert_eq!(ttl(&f, k), PENDING_TARGET_LEDGERS);
    }
    // ...ignores an unknown id...
    f.c.extend_obligation_ttl(&999);

    // ...and leaves a Settled one to archive.
    f.env.mock_all_auths();
    f.c.mark_settled(&f.funder, &1, &reference(&f.env, 2));
    advance_days(&f.env, 10);
    let before = ttl(&f, &DataKey::Obligation(1));
    f.c.extend_obligation_ttl(&1);
    assert_eq!(ttl(&f, &DataKey::Obligation(1)), before);
    assert_eq!(before, PENDING_TARGET_LEDGERS - 10 * DAY_LEDGERS);
}

/// security.md class 8. An archived persistent entry never reads as absent: the host either
/// refuses the read or (protocol 23+ automatic restoration, which the test host emulates)
/// restores it with its TRUE value. Either way a lapsed obligation is never recreated fresh,
/// a conflicting record is still refused, and a mark acts on the real obligation.
#[test]
fn an_archived_obligation_is_never_recreated_fresh() {
    let f = setup();
    record(&f, 1);
    // Keep the instance alive while the 60-day obligation entries lapse (75 days).
    for _ in 0..3 {
        f.c.extend_contract_ttl();
        advance_days(&f.env, 25);
    }
    f.c.extend_contract_ttl();

    // The identical record returns obligation 1, not a new obligation 2.
    let again =
        f.c.try_record_obligation(&f.pr, &1, &f.org, &AMOUNT, &digest(&f.env, 0xD1));
    match again {
        Ok(Ok(ob)) => assert_eq!(ob.id, 1),
        Err(_) => {} // a host that refuses the archived read also fails closed
        other => panic!("unexpected {:?}", other),
    }
    // A conflicting record is still a conflict, not a fresh obligation.
    assert!(f
        .c
        .try_record_obligation(&f.pr, &1, &f.org, &(AMOUNT + 1), &digest(&f.env, 0xD1))
        .is_err());
    // A mark acts on the real obligation (or traps), never on an empty one.
    if let Ok(Ok(ob)) = f.c.try_mark_funded(&f.funder, &1, &reference(&f.env, 1)) {
        assert_eq!((ob.id, ob.policy_id, ob.amount), (1, 1, AMOUNT));
    }
    assert_eq!(f.c.obligation_count(), 1);
    assert_eq!(f.c.org_obligation_count(&f.org), 1);
}

// ============ 21. upgrade ============

#[test]
fn upgrade_is_refused_to_non_upgraders_and_without_a_signature() {
    let f = setup();
    let hash = BytesN::from_array(&f.env, &[7u8; 32]);
    contract_err(
        f.c.try_upgrade(&f.funder, &hash),
        CommonError::Unauthorized as u32,
    );
    f.env.set_auths(&[]);
    host_auth_err(f.c.try_upgrade(&f.admin, &hash));
}

// ============ 22. structural: no custody ============

#[test]
fn the_adapter_never_touches_a_token_or_calls_another_contract() {
    let src = include_str!("lib.rs");
    for needle in ["token::", "TokenClient", "invoke_contract", "Client::new"] {
        assert!(!src.contains(needle), "lib.rs must not contain `{needle}`");
    }
}

// ============ 23. property test of the state machine ============

mod props {
    use super::*;
    use proptest::prelude::*;
    use std::collections::BTreeMap;
    use std::vec::Vec as StdVec;

    #[derive(Clone, Debug)]
    enum Op {
        Record {
            policy: u64,
            org: usize,
            amount: usize,
            digest: u8,
        },
        Fund {
            id: u64,
            caller: usize,
            reference: usize,
        },
        Settle {
            id: u64,
            caller: usize,
            reference: usize,
        },
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            (1u64..5, 0usize..2, 0usize..3, 1u8..3).prop_map(|(policy, org, amount, digest)| {
                Op::Record {
                    policy,
                    org,
                    amount,
                    digest,
                }
            }),
            (1u64..6, 0usize..5, 0usize..3).prop_map(|(id, caller, reference)| Op::Fund {
                id,
                caller,
                reference
            }),
            (1u64..6, 0usize..5, 0usize..3).prop_map(|(id, caller, reference)| Op::Settle {
                id,
                caller,
                reference
            }),
        ]
    }

    fn rank(s: ObligationStatus) -> u32 {
        s as u32
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]
        #[test]
        fn status_is_monotone_and_one_obligation_per_policy(ops in proptest::collection::vec(op(), 1..24)) {
            // Random inputs: no snapshot (it would differ on every run).
            let f = setup_in(env_with(Env::new_with_config(soroban_sdk::testutils::EnvTestConfig {
                capture_snapshot_at_drop: false,
            })));
            let org_b = Address::generate(&f.env);
            let stranger = Address::generate(&f.env);
            let orgs = [f.org.clone(), org_b.clone()];
            // callers: org A, org B, funder, stranger, the PayoutReceiver
            let callers = [f.org.clone(), org_b, f.funder.clone(), stranger, f.pr.clone()];
            let amounts = [AMOUNT, 1i128, 0i128];
            let refs = [reference(&f.env, 0xA1), reference(&f.env, 0xA2), BytesN::from_array(&f.env, &[0u8; 32])];

            let mut seen: BTreeMap<u64, Obligation> = BTreeMap::new();
            let mut recorded_policies: StdVec<u64> = StdVec::new();
            for op in ops {
                match op {
                    Op::Record { policy, org, amount, digest } => {
                        let r = f.c.try_record_obligation(&f.pr, &policy, &orgs[org], &amounts[amount], &super::digest(&f.env, digest));
                        if r.is_ok() && !recorded_policies.contains(&policy) {
                            recorded_policies.push(policy);
                        }
                    }
                    Op::Fund { id, caller, reference } => {
                        let _ = f.c.try_mark_funded(&callers[caller], &id, &refs[reference]);
                    }
                    Op::Settle { id, caller, reference } => {
                        let _ = f.c.try_mark_settled(&callers[caller], &id, &refs[reference]);
                    }
                }
                // Invariants after every step.
                prop_assert_eq!(f.c.obligation_count(), recorded_policies.len() as u64);
                for id in 1..=f.c.obligation_count() {
                    let now = f.c.get_obligation(&id).unwrap();
                    prop_assert_eq!(f.c.find_obligation(&f.pr, &now.policy_id), Some(id));
                    prop_assert!(now.amount > 0);
                    if let Some(prev) = seen.get(&id) {
                        prop_assert!(rank(now.status) >= rank(prev.status), "status went backwards");
                        if prev.status == ObligationStatus::Settled {
                            prop_assert_eq!(&now, prev, "a settled obligation changed");
                        }
                        if prev.funding_reference.is_some() {
                            prop_assert_eq!(&now.funding_reference, &prev.funding_reference);
                        }
                        prop_assert_eq!(&now.determination_digest, &prev.determination_digest);
                        prop_assert_eq!(now.amount, prev.amount);
                    }
                    if now.status != ObligationStatus::Owed {
                        let by = if now.status == ObligationStatus::Settled { now.settled_by.clone() } else { now.funded_by.clone() };
                        let by = by.unwrap();
                        prop_assert!(by == now.org || by == f.funder, "marked by an unauthorized caller");
                    }
                    seen.insert(id, now);
                }
                // One obligation per (receiver, policy): the ids found for distinct policies
                // are distinct.
                let mut ids: StdVec<u64> = recorded_policies.iter().map(|p| f.c.find_obligation(&f.pr, p).unwrap()).collect();
                ids.sort();
                ids.dedup();
                prop_assert_eq!(ids.len(), recorded_policies.len());
            }
        }
    }
}
