#![cfg(test)]
//! Treasury unit tests (Soroban test env).
//!
//! Coverage:
//! - premium in: per-org fee split + net credit to the org's reserve + accumulated fees
//! - payout out: debits ONLY that org's reserve, transfers to the backend wallet
//! - guards: `InsufficientOrgReserve`, `PremiumAlreadyReceived`, `PayoutAlreadyProcessed`,
//!   `PremiumNotReceived`, `NotPayoutReceiver`, `ZeroAmount`
//! - surplus-only withdrawal + `WouldBreachReserve`
//! - emergency-withdraw recovers only UNBACKED surplus
//! - the solvency invariant `balance >= total_org_reserves + accumulated_fees` after each op

use soroban_sdk::{
    contract, contractimpl, contracttype,
    testutils::{Address as _, Ledger as _},
    token::{StellarAssetClient, TokenClient},
    Address, Env,
};

use microcrop_shared::types::{CoverageType, Policy, PolicyStatus, Role};

use crate::{Treasury, TreasuryClient};

// ---------------- Mock PolicyManager ----------------
// The Treasury reads a policy (status, end date, org) and an org's outstanding exposure. This
// mock exposes exactly the `PolicyManagerClient` methods Treasury calls: `get_policy`,
// `policy_org` and `org_outstanding_sum_insured`. `set_org` stores an ACTIVE policy with a
// far end date; `set_status` / `set_end_date` move it for the status-gate tests.

#[contracttype]
#[derive(Clone)]
enum MockKey {
    Policy(u64),
    Outstanding(Address),
}

#[contract]
struct MockPm;

#[contractimpl]
impl MockPm {
    pub fn set_org(env: Env, policy_id: u64, org: Address) {
        let p = Policy {
            id: policy_id,
            farmer: org.clone(),
            plot_id: 1,
            sum_insured: 1_000_000_000,
            premium: 1_000_000,
            start_date: 0,
            end_date: u64::MAX,
            coverage_type: CoverageType::Drought,
            status: PolicyStatus::Active,
            created_at: 0,
            org,
        };
        env.storage()
            .persistent()
            .set(&MockKey::Policy(policy_id), &p);
    }

    pub fn set_status(env: Env, policy_id: u64, status: PolicyStatus) {
        let mut p = Self::get_policy(env.clone(), policy_id);
        p.status = status;
        env.storage()
            .persistent()
            .set(&MockKey::Policy(policy_id), &p);
    }

    pub fn set_end_date(env: Env, policy_id: u64, end_date: u64) {
        let mut p = Self::get_policy(env.clone(), policy_id);
        p.end_date = end_date;
        env.storage()
            .persistent()
            .set(&MockKey::Policy(policy_id), &p);
    }

    pub fn set_outstanding(env: Env, org: Address, amount: i128) {
        env.storage()
            .persistent()
            .set(&MockKey::Outstanding(org), &amount);
    }

    // --- methods invoked by Treasury via PolicyManagerClient ---
    pub fn get_policy(env: Env, policy_id: u64) -> Policy {
        env.storage()
            .persistent()
            .get(&MockKey::Policy(policy_id))
            .unwrap()
    }

    pub fn policy_org(env: Env, policy_id: u64) -> Address {
        Self::get_policy(env, policy_id).org
    }

    pub fn org_outstanding_sum_insured(env: Env, org: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&MockKey::Outstanding(org))
            .unwrap_or(0)
    }
}

// ---------------- Mock 6-decimal token ----------------
// Stands in for a non-Stellar USDC (e.g. a bridged 6dp token). Only `decimals()` is called by
// the Treasury constructor.

#[contract]
struct SixDecimalToken;

#[contractimpl]
impl SixDecimalToken {
    pub fn decimals(_env: Env) -> u32 {
        6
    }
}

// ---------------- Test harness ----------------

struct Ctx<'a> {
    env: Env,
    treasury: TreasuryClient<'a>,
    treasury_id: Address,
    token: TokenClient<'a>,
    token_admin: StellarAssetClient<'a>,
    admin: Address,
    backend: Address,        // BACKEND_ROLE holder + USDC payer
    backend_wallet: Address, // receives payouts
    payout_receiver: Address,
    org: Address,
    pm_id: Address,
}

fn setup<'a>() -> Ctx<'a> {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let backend = Address::generate(&env);
    let backend_wallet = Address::generate(&env);
    let payout_receiver = Address::generate(&env);
    let org = Address::generate(&env);

    // USDC as a Stellar Asset Contract.
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let usdc_addr = sac.address();
    let token = TokenClient::new(&env, &usdc_addr);
    let token_admin = StellarAssetClient::new(&env, &usdc_addr);

    // Treasury.
    let treasury_id = env.register(
        Treasury,
        (usdc_addr.clone(), backend_wallet.clone(), admin.clone()),
    );
    let treasury = TreasuryClient::new(&env, &treasury_id);

    // Mock PolicyManager + wiring.
    let pm_id = env.register(MockPm, ());
    treasury.set_policy_manager(&admin, &pm_id);
    treasury.set_payout_receiver(&admin, &payout_receiver);

    // Roles: backend receives premiums; payout_receiver requests payouts.
    treasury.grant_role(&admin, &Role::Backend, &backend);
    treasury.grant_role(&admin, &Role::Payout, &payout_receiver);

    Ctx {
        env,
        treasury,
        treasury_id,
        token,
        token_admin,
        admin,
        backend,
        backend_wallet,
        payout_receiver,
        org,
        pm_id,
    }
}

impl<'a> Ctx<'a> {
    fn pm(&self) -> MockPmClient<'a> {
        MockPmClient::new(&self.env, &self.pm_id)
    }

    /// The solvency invariant that must hold after every state change.
    fn assert_solvent(&self) {
        let balance = self.token.balance(&self.treasury_id);
        let backed = self.treasury.total_org_reserves() + self.treasury.accumulated_fees();
        assert!(
            balance >= backed,
            "solvency invariant breached: balance {} < backed {}",
            balance,
            backed
        );
    }
}

// ---------------- Happy path ----------------

#[test]
fn premium_split_and_payout_happy_path() {
    let c = setup();
    // policy 1 backs `org`.
    c.pm().set_org(&1u64, &c.org);

    // Fund the backend payer with USDC and let it pay a 1_000_000 base-unit premium
    // (0.1 USDC: the SAC has 7 decimals).
    c.token_admin.mint(&c.backend, &1_000_000);
    c.treasury.receive_premium(&c.backend, &1u64, &1_000_000);

    // Default global fee = 10% => 1000 bps => fee 100_000, net 900_000.
    assert_eq!(c.treasury.accumulated_fees(), 100_000);
    assert_eq!(c.treasury.org_reserve(&c.org), 900_000);
    assert_eq!(c.treasury.total_org_reserves(), 900_000);
    assert_eq!(c.treasury.get_total_premiums(), 900_000);
    assert_eq!(c.treasury.get_balance(), 1_000_000);
    c.assert_solvent();

    // Pay out 500_000 from the org's reserve.
    c.treasury
        .request_payout(&c.payout_receiver, &1u64, &500_000);

    assert_eq!(c.treasury.org_reserve(&c.org), 400_000);
    assert_eq!(c.treasury.total_org_reserves(), 400_000);
    assert_eq!(c.treasury.get_total_payouts(), 500_000);
    assert!(c.treasury.is_payout_processed(&1u64));
    // Backend wallet received the payout.
    assert_eq!(c.token.balance(&c.backend_wallet), 500_000);
    // Treasury balance = 900_000 - 500_000 net + 100_000 fees left = 500_000.
    assert_eq!(c.treasury.get_balance(), 500_000);
    c.assert_solvent();
}

#[test]
fn per_org_fee_override_is_honored() {
    let c = setup();
    c.pm().set_org(&7u64, &c.org);
    // Explicit 500 bps (5%) fee for this org.
    c.treasury.set_org_fee_bps(&c.admin, &c.org, &500u32);

    c.token_admin.mint(&c.backend, &2_000_000);
    c.treasury.receive_premium(&c.backend, &7u64, &2_000_000);

    // 5% of 2_000_000 = 100_000 fee; net 1_900_000.
    assert_eq!(c.treasury.accumulated_fees(), 100_000);
    assert_eq!(c.treasury.org_reserve(&c.org), 1_900_000);
    assert_eq!(
        c.treasury.calculate_platform_fee(&2_000_000, &c.org),
        100_000
    );
    c.assert_solvent();
}

#[test]
fn explicit_zero_fee_bps_is_honored_over_global() {
    let c = setup();
    c.pm().set_org(&9u64, &c.org);
    c.treasury.set_org_fee_bps(&c.admin, &c.org, &0u32);

    c.token_admin.mint(&c.backend, &1_000_000);
    c.treasury.receive_premium(&c.backend, &9u64, &1_000_000);

    // 0 bps => no fee; full amount credited to the org.
    assert_eq!(c.treasury.accumulated_fees(), 0);
    assert_eq!(c.treasury.org_reserve(&c.org), 1_000_000);
    c.assert_solvent();
}

// ---------------- Guards ----------------

#[test]
fn payout_exceeding_org_reserve_is_recorded_pending_not_paid() {
    let c = setup();
    c.pm().set_org(&1u64, &c.org);
    c.token_admin.mint(&c.backend, &1_000_000);
    c.treasury.receive_premium(&c.backend, &1u64, &1_000_000); // org reserve = 900_000

    // Ask for more than the org holds: no revert, nothing paid, the full amount recorded.
    let paid_now = c
        .treasury
        .request_payout(&c.payout_receiver, &1u64, &900_001);
    assert!(!paid_now);
    assert_eq!(c.token.balance(&c.backend_wallet), 0);
    assert_eq!(c.treasury.org_reserve(&c.org), 900_000);
    assert!(!c.treasury.is_payout_processed(&1u64));
    let pending = c.treasury.get_pending_payout(&1u64).unwrap();
    assert_eq!(pending.amount, 900_001);
    assert_eq!(pending.org, c.org);
    assert_eq!(c.treasury.org_pending_payouts(&c.org), 900_001);
    c.assert_solvent();
}

#[test]
#[should_panic(expected = "#202")] // PremiumAlreadyReceived
fn premium_replay_reverts() {
    let c = setup();
    c.pm().set_org(&1u64, &c.org);
    c.token_admin.mint(&c.backend, &2_000_000);
    c.treasury.receive_premium(&c.backend, &1u64, &1_000_000);
    c.treasury.receive_premium(&c.backend, &1u64, &1_000_000);
}

#[test]
#[should_panic(expected = "#203")] // PayoutAlreadyProcessed
fn payout_replay_reverts() {
    let c = setup();
    c.pm().set_org(&1u64, &c.org);
    c.token_admin.mint(&c.backend, &1_000_000);
    c.treasury.receive_premium(&c.backend, &1u64, &1_000_000);
    c.treasury
        .request_payout(&c.payout_receiver, &1u64, &100_000);
    c.treasury
        .request_payout(&c.payout_receiver, &1u64, &100_000);
}

#[test]
#[should_panic(expected = "#207")] // PremiumNotReceived
fn payout_without_premium_reverts() {
    let c = setup();
    c.pm().set_org(&5u64, &c.org);
    // No premium received for policy 5, but seed the org reserve via a direct deposit so the
    // failure is specifically PremiumNotReceived (not InsufficientOrgReserve).
    c.token_admin.mint(&c.org, &1_000_000);
    c.treasury.deposit_reserve(&c.org, &c.org, &1_000_000);
    c.treasury
        .request_payout(&c.payout_receiver, &5u64, &100_000);
}

#[test]
#[should_panic(expected = "#211")] // NotPayoutReceiver
fn payout_from_non_payout_receiver_reverts() {
    let c = setup();
    c.pm().set_org(&1u64, &c.org);
    c.token_admin.mint(&c.backend, &1_000_000);
    c.treasury.receive_premium(&c.backend, &1u64, &1_000_000);

    // Grant PAYOUT_ROLE to a rogue address; it still is not the wired payout_receiver.
    let rogue = Address::generate(&c.env);
    c.treasury.grant_role(&c.admin, &Role::Payout, &rogue);
    c.treasury.request_payout(&rogue, &1u64, &100_000);
}

#[test]
#[should_panic(expected = "#201")] // ZeroAmount
fn zero_premium_reverts() {
    let c = setup();
    c.pm().set_org(&1u64, &c.org);
    c.treasury.receive_premium(&c.backend, &1u64, &0);
}

// ---------------- Reserve / surplus ----------------

#[test]
fn deposit_and_withdraw_surplus() {
    let c = setup();
    // Org has 1000 outstanding sum insured at the default 3000 bps (30%) ratio => required 300.
    c.pm().set_outstanding(&c.org, &1_000);

    c.token_admin.mint(&c.org, &1_000);
    c.treasury.deposit_reserve(&c.org, &c.org, &1_000);
    assert_eq!(c.treasury.org_reserve(&c.org), 1_000);
    assert_eq!(c.treasury.reserve_required(&c.org), 300);
    assert!(c.treasury.meets_reserve_requirements(&c.org));
    c.assert_solvent();

    // Withdraw surplus down to exactly the required reserve (1000 - 700 = 300).
    let to = Address::generate(&c.env);
    c.treasury.withdraw_org_surplus(&c.org, &700, &to);
    assert_eq!(c.treasury.org_reserve(&c.org), 300);
    assert_eq!(c.token.balance(&to), 700);
    c.assert_solvent();
}

#[test]
#[should_panic(expected = "#213")] // WouldBreachReserve
fn withdraw_below_required_reserve_reverts() {
    let c = setup();
    c.pm().set_outstanding(&c.org, &1_000); // required = 300
    c.token_admin.mint(&c.org, &1_000);
    c.treasury.deposit_reserve(&c.org, &c.org, &1_000);
    let to = Address::generate(&c.env);
    // Leaving 299 < required 300.
    c.treasury.withdraw_org_surplus(&c.org, &701, &to);
}

// ---------------- Fees / emergency ----------------

#[test]
fn withdraw_fees_moves_only_platform_revenue() {
    let c = setup();
    c.pm().set_org(&1u64, &c.org);
    c.token_admin.mint(&c.backend, &1_000_000);
    c.treasury.receive_premium(&c.backend, &1u64, &1_000_000); // fee 100_000

    let recipient = Address::generate(&c.env);
    c.treasury.withdraw_fees(&c.admin, &recipient);
    assert_eq!(c.token.balance(&recipient), 100_000);
    assert_eq!(c.treasury.accumulated_fees(), 0);
    // Org reserve untouched.
    assert_eq!(c.treasury.org_reserve(&c.org), 900_000);
    c.assert_solvent();
}

#[test]
fn emergency_withdraw_recovers_only_unbacked_surplus() {
    let c = setup();
    c.pm().set_org(&1u64, &c.org);
    c.token_admin.mint(&c.backend, &1_000_000);
    c.treasury.receive_premium(&c.backend, &1u64, &1_000_000);

    // Someone accidentally sends 250_000 USDC straight to the treasury (unbacked surplus).
    let stray = Address::generate(&c.env);
    c.token_admin.mint(&stray, &250_000);
    c.token.transfer(&stray, &c.treasury_id, &250_000);

    // Must be paused to emergency-withdraw.
    c.treasury.pause(&c.admin);

    let rescuer = Address::generate(&c.env);
    // Only 250_000 is recoverable (balance - reserves - fees).
    c.treasury.emergency_withdraw(&c.admin, &rescuer, &250_000);
    assert_eq!(c.token.balance(&rescuer), 250_000);
    c.assert_solvent();
}

#[test]
#[should_panic(expected = "#208")] // ExceedsRecoverableSurplus
fn emergency_withdraw_cannot_touch_backed_funds() {
    let c = setup();
    c.pm().set_org(&1u64, &c.org);
    c.token_admin.mint(&c.backend, &1_000_000);
    c.treasury.receive_premium(&c.backend, &1u64, &1_000_000);
    c.treasury.pause(&c.admin);
    let rescuer = Address::generate(&c.env);
    // Nothing unbacked => 1 exceeds recoverable (0).
    c.treasury.emergency_withdraw(&c.admin, &rescuer, &1);
}

#[test]
#[should_panic(expected = "#216")] // NotPaused
fn emergency_withdraw_requires_paused() {
    let c = setup();
    let rescuer = Address::generate(&c.env);
    c.treasury.emergency_withdraw(&c.admin, &rescuer, &0);
}

// ---------------- Access control ----------------

#[test]
#[should_panic(expected = "#1")] // CommonError::Unauthorized
fn receive_premium_requires_backend_role() {
    let c = setup();
    c.pm().set_org(&1u64, &c.org);
    let stranger = Address::generate(&c.env);
    c.token_admin.mint(&stranger, &1_000_000);
    c.treasury.receive_premium(&stranger, &1u64, &1_000_000);
}

#[test]
#[should_panic(expected = "#205")] // FeeTooHigh
fn set_platform_fee_above_max_reverts() {
    let c = setup();
    c.treasury.set_platform_fee(&c.admin, &21u32);
}

#[test]
#[should_panic(expected = "#212")] // BpsTooHigh
fn set_org_fee_above_max_reverts() {
    let c = setup();
    c.treasury.set_org_fee_bps(&c.admin, &c.org, &3_001u32);
}

// ---------------- Token precision (finding 5) ----------------

#[test]
fn usdc_sac_reports_seven_decimals() {
    let c = setup();
    // The SAC the harness registers is what Stellar USDC is: 7 decimals.
    assert_eq!(c.token.decimals(), crate::USDC_DECIMALS);
    assert_eq!(crate::USDC_DECIMALS, 7);
}

#[test]
#[should_panic(expected = "#217")] // UsdcDecimalsMismatch
fn constructor_rejects_a_non_seven_decimal_token() {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let backend_wallet = Address::generate(&env);
    let six_dp = env.register(SixDecimalToken, ());
    env.register(Treasury, (six_dp, backend_wallet, admin));
}

// ---------------- Reserve floor (finding 2) ----------------

#[test]
fn default_reserve_ratio_is_thirty_percent() {
    assert_eq!(crate::DEFAULT_RESERVE_RATIO_BPS, 3_000);
    assert_eq!(crate::MIN_RESERVE_RATIO_BPS, 3_000);
}

#[test]
#[should_panic(expected = "#218")] // ReserveRatioTooLow
fn set_org_reserve_ratio_below_floor_reverts() {
    let c = setup();
    c.treasury
        .set_org_reserve_ratio_bps(&c.admin, &c.org, &2_999u32);
}

#[test]
fn set_org_reserve_ratio_at_floor_and_above_is_accepted() {
    let c = setup();
    c.pm().set_outstanding(&c.org, &10_000);
    c.treasury
        .set_org_reserve_ratio_bps(&c.admin, &c.org, &3_000u32);
    assert_eq!(c.treasury.reserve_required(&c.org), 3_000);
    c.treasury
        .set_org_reserve_ratio_bps(&c.admin, &c.org, &5_000u32);
    assert_eq!(c.treasury.reserve_required(&c.org), 5_000);
}

#[test]
fn smallest_valid_payout_survives_a_maximal_surplus_withdrawal() {
    // The org withdraws every unit it is allowed to; a 30% claim (the minimum a
    // determination can carry) must still be payable from what is left.
    let c = setup();
    let sum_insured: i128 = 1_000_000;
    c.pm().set_org(&1u64, &c.org);
    c.pm().set_outstanding(&c.org, &sum_insured);
    c.token_admin.mint(&c.org, &sum_insured);
    c.treasury.deposit_reserve(&c.org, &c.org, &sum_insured);
    c.token_admin.mint(&c.backend, &100_000);
    c.treasury.receive_premium(&c.backend, &1u64, &100_000); // +90_000 net

    let required = c.treasury.reserve_required(&c.org);
    assert_eq!(required, sum_insured * 3_000 / 10_000);
    let surplus = c.treasury.org_reserve(&c.org) - required;
    let to = Address::generate(&c.env);
    c.treasury.withdraw_org_surplus(&c.org, &surplus, &to);
    assert_eq!(c.treasury.org_reserve(&c.org), required);

    // 30% payout drains the floor exactly; it does not revert.
    c.treasury
        .request_payout(&c.payout_receiver, &1u64, &(sum_insured * 3_000 / 10_000));
    assert_eq!(c.treasury.org_reserve(&c.org), 0);
    c.assert_solvent();
}

// ---------------- Premium only for open policies (finding 8) ----------------

fn premium_on_status(status: PolicyStatus) {
    let c = setup();
    c.pm().set_org(&1u64, &c.org);
    c.pm().set_status(&1u64, &status);
    c.token_admin.mint(&c.backend, &1_000_000);
    c.treasury.receive_premium(&c.backend, &1u64, &1_000_000);
}

#[test]
fn premium_accepted_for_pending_policy() {
    premium_on_status(PolicyStatus::Pending);
}

#[test]
#[should_panic(expected = "#219")] // PolicyNotOpenForPremium
fn premium_rejected_for_cancelled_policy() {
    premium_on_status(PolicyStatus::Cancelled);
}

#[test]
#[should_panic(expected = "#219")]
fn premium_rejected_for_expired_policy() {
    premium_on_status(PolicyStatus::Expired);
}

#[test]
#[should_panic(expected = "#219")]
fn premium_rejected_for_claimed_policy() {
    premium_on_status(PolicyStatus::Claimed);
}

#[test]
#[should_panic(expected = "#220")] // PolicyCoverageEnded
fn premium_rejected_for_active_policy_past_end_date() {
    let c = setup();
    c.env.ledger().set_timestamp(10_000);
    c.pm().set_org(&1u64, &c.org);
    c.pm().set_end_date(&1u64, &9_999u64);
    c.token_admin.mint(&c.backend, &1_000_000);
    c.treasury.receive_premium(&c.backend, &1u64, &1_000_000);
}

#[test]
fn premium_accepted_for_active_policy_on_its_end_date() {
    let c = setup();
    c.env.ledger().set_timestamp(10_000);
    c.pm().set_org(&1u64, &c.org);
    c.pm().set_end_date(&1u64, &10_000u64);
    c.token_admin.mint(&c.backend, &1_000_000);
    c.treasury.receive_premium(&c.backend, &1u64, &1_000_000);
    assert!(c.treasury.is_premium_received(&1u64));
}

// ---------------- Pending payouts (findings 9 and 11) ----------------

/// Premium of 1_000_000 for policy 1 (net 900_000 reserve), then a 2_000_000 payout that
/// the reserve cannot cover, so it is recorded as pending.
fn setup_with_pending<'a>() -> Ctx<'a> {
    let c = setup();
    c.pm().set_org(&1u64, &c.org);
    c.token_admin.mint(&c.backend, &1_000_000);
    c.treasury.receive_premium(&c.backend, &1u64, &1_000_000);
    assert!(!c
        .treasury
        .request_payout(&c.payout_receiver, &1u64, &2_000_000));
    c
}

#[test]
fn pending_payout_is_part_of_reserve_required() {
    let c = setup_with_pending();
    c.pm().set_outstanding(&c.org, &1_000_000);
    // 30% of outstanding + the full pending payout.
    assert_eq!(c.treasury.reserve_required(&c.org), 300_000 + 2_000_000);
    assert!(!c.treasury.meets_reserve_requirements(&c.org));

    // Top up to exactly the requirement: still nothing withdrawable.
    c.token_admin.mint(&c.org, &1_400_000);
    c.treasury.deposit_reserve(&c.org, &c.org, &1_400_000);
    assert_eq!(c.treasury.org_reserve(&c.org), 2_300_000);
    let to = Address::generate(&c.env);
    assert!(c
        .treasury
        .try_withdraw_org_surplus(&c.org, &1, &to)
        .is_err());
}

#[test]
fn settle_pending_payout_pays_the_full_amount_once() {
    let c = setup_with_pending();

    // Still short: reverts and keeps the record.
    assert!(c
        .treasury
        .try_settle_pending_payout(&c.payout_receiver, &1u64)
        .is_err());
    assert!(c.treasury.get_pending_payout(&1u64).is_some());

    // Top up and settle: exactly the recorded amount leaves, to the backend wallet.
    c.token_admin.mint(&c.org, &1_100_000);
    c.treasury.deposit_reserve(&c.org, &c.org, &1_100_000);
    let paid = c.treasury.settle_pending_payout(&c.payout_receiver, &1u64);
    assert_eq!(paid, 2_000_000);
    assert_eq!(c.token.balance(&c.backend_wallet), 2_000_000);
    assert_eq!(c.treasury.org_reserve(&c.org), 0);
    assert!(c.treasury.is_payout_processed(&1u64));
    assert!(c.treasury.get_pending_payout(&1u64).is_none());
    assert_eq!(c.treasury.org_pending_payouts(&c.org), 0);
    assert_eq!(c.treasury.get_total_payouts(), 2_000_000);
    c.assert_solvent();
}

#[test]
#[should_panic(expected = "#221")] // NoPendingPayout
fn settle_pending_payout_twice_reverts() {
    let c = setup_with_pending();
    c.token_admin.mint(&c.org, &2_000_000);
    c.treasury.deposit_reserve(&c.org, &c.org, &2_000_000);
    c.treasury.settle_pending_payout(&c.payout_receiver, &1u64);
    c.treasury.settle_pending_payout(&c.payout_receiver, &1u64);
}

#[test]
#[should_panic(expected = "#222")] // PayoutAlreadyPending
fn second_request_while_pending_reverts() {
    let c = setup_with_pending();
    c.treasury.request_payout(&c.payout_receiver, &1u64, &100);
}

#[test]
#[should_panic(expected = "#203")] // PayoutAlreadyProcessed
fn request_after_settlement_reverts() {
    let c = setup_with_pending();
    c.token_admin.mint(&c.org, &2_000_000);
    c.treasury.deposit_reserve(&c.org, &c.org, &2_000_000);
    c.treasury.settle_pending_payout(&c.payout_receiver, &1u64);
    c.treasury.request_payout(&c.payout_receiver, &1u64, &100);
}

#[test]
#[should_panic(expected = "#211")] // NotPayoutReceiver
fn settle_pending_payout_only_from_payout_receiver() {
    let c = setup_with_pending();
    let rogue = Address::generate(&c.env);
    c.treasury.grant_role(&c.admin, &Role::Payout, &rogue);
    c.treasury.settle_pending_payout(&rogue, &1u64);
}

#[test]
#[should_panic(expected = "#215")] // Paused
fn settle_pending_payout_blocked_while_paused() {
    let c = setup_with_pending();
    c.token_admin.mint(&c.org, &2_000_000);
    c.treasury.deposit_reserve(&c.org, &c.org, &2_000_000);
    c.treasury.pause(&c.admin);
    c.treasury.settle_pending_payout(&c.payout_receiver, &1u64);
}

// ---------------- Per-policy flags outlive a 365-day policy ----------------

#[test]
fn premium_flag_ttl_covers_the_longest_policy() {
    use soroban_sdk::testutils::storage::Persistent as _;

    let c = setup();
    c.pm().set_org(&1u64, &c.org);
    c.token_admin.mint(&c.backend, &1_000_000);
    c.treasury.receive_premium(&c.backend, &1u64, &1_000_000);
    let ttl = c.env.as_contract(&c.treasury_id, || {
        c.env
            .storage()
            .persistent()
            .get_ttl(&crate::DataKey::PremiumReceived(1))
    });
    // MAX_DURATION_DAYS = 365 at 17,280 ledgers/day. (The test host's max_entry_ttl is
    // ~365.3 days, so the 400-day request is clamped to it.)
    assert!(
        ttl >= 365 * 17_280,
        "PremiumReceived TTL {} < 365 days",
        ttl
    );
}

// ---------------- New claims queue behind pending ones ----------------

#[test]
fn new_claim_cannot_be_paid_from_money_owed_to_a_pending_claim() {
    // Policy 1: 900_000 reserve, 2_000_000 pending.
    let c = setup_with_pending();
    // Policy 2 (same org): another 900_000 net, then the org tops up 1_200_000.
    c.pm().set_org(&2u64, &c.org);
    c.token_admin.mint(&c.backend, &1_000_000);
    c.treasury.receive_premium(&c.backend, &2u64, &1_000_000);
    c.token_admin.mint(&c.org, &1_200_000);
    c.treasury.deposit_reserve(&c.org, &c.org, &1_200_000);
    assert_eq!(c.treasury.org_reserve(&c.org), 3_000_000);

    // 1_500_000 fits the reserve (3_000_000) but not the part not owed to policy 1
    // (1_000_000), so it is recorded pending instead of being paid ahead of policy 1.
    assert!(!c
        .treasury
        .request_payout(&c.payout_receiver, &2u64, &1_500_000));
    assert_eq!(c.token.balance(&c.backend_wallet), 0);
    assert_eq!(c.treasury.org_pending_payouts(&c.org), 3_500_000);

    // A claim that fits the unowed part is still paid at once.
    c.pm().set_org(&3u64, &c.org);
    c.token_admin.mint(&c.backend, &1_000_000);
    c.treasury.receive_premium(&c.backend, &3u64, &1_000_000);
    c.token_admin.mint(&c.org, &3_600_000);
    c.treasury.deposit_reserve(&c.org, &c.org, &3_600_000);
    // reserve 7_500_000, pending 3_500_000, unowed 4_000_000.
    assert!(c
        .treasury
        .request_payout(&c.payout_receiver, &3u64, &4_000_000));
    assert_eq!(c.token.balance(&c.backend_wallet), 4_000_000);

    // The two pending claims are both still fully covered.
    assert_eq!(c.treasury.org_reserve(&c.org), 3_500_000);
    c.treasury.settle_pending_payout(&c.payout_receiver, &1u64);
    c.treasury.settle_pending_payout(&c.payout_receiver, &2u64);
    assert_eq!(c.token.balance(&c.backend_wallet), 7_500_000);
    assert_eq!(c.treasury.org_reserve(&c.org), 0);
    assert_eq!(c.treasury.org_pending_payouts(&c.org), 0);
    c.assert_solvent();
}

#[test]
#[should_panic(expected = "Error(Contract, #3)")] // CommonError::NotConfigured
fn settle_pending_payout_requires_a_wired_payout_receiver() {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let backend = Address::generate(&env);
    let payout = Address::generate(&env);
    let org = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let token_admin = StellarAssetClient::new(&env, &sac.address());
    let treasury_id = env.register(
        Treasury,
        (sac.address(), Address::generate(&env), admin.clone()),
    );
    let treasury = TreasuryClient::new(&env, &treasury_id);
    let pm_id = env.register(MockPm, ());
    treasury.set_policy_manager(&admin, &pm_id);
    // No set_payout_receiver.
    treasury.grant_role(&admin, &Role::Backend, &backend);
    treasury.grant_role(&admin, &Role::Payout, &payout);
    MockPmClient::new(&env, &pm_id).set_org(&1u64, &org);
    token_admin.mint(&backend, &1_000_000);
    treasury.receive_premium(&backend, &1u64, &1_000_000);
    assert!(!treasury.request_payout(&payout, &1u64, &2_000_000));
    token_admin.mint(&org, &2_000_000);
    treasury.deposit_reserve(&org, &org, &2_000_000);

    // Settling from a bare Payout-role holder would clear the Treasury record while the
    // PayoutReceiver marker and the CLAIM_PENDING status stay behind; refuse it.
    treasury.settle_pending_payout(&payout, &1u64);
}
