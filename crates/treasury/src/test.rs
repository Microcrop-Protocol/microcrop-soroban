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
    testutils::Address as _,
    token::{StellarAssetClient, TokenClient},
    Address, Env,
};

use microcrop_shared::types::Role;

use crate::{Treasury, TreasuryClient};

// ---------------- Mock PolicyManager ----------------
// The real PolicyManager is a separate crate (still a stub); Treasury only needs the two v3
// per-org reads. This mock exposes exactly the `PolicyManagerClient` methods Treasury calls:
// `policy_org(policy_id)` and `org_outstanding_sum_insured(org)`.

#[contracttype]
#[derive(Clone)]
enum MockKey {
    Org(u64),
    Outstanding(Address),
}

#[contract]
struct MockPm;

#[contractimpl]
impl MockPm {
    pub fn set_org(env: Env, policy_id: u64, org: Address) {
        env.storage().persistent().set(&MockKey::Org(policy_id), &org);
    }

    pub fn set_outstanding(env: Env, org: Address, amount: i128) {
        env.storage()
            .persistent()
            .set(&MockKey::Outstanding(org), &amount);
    }

    // --- methods invoked by Treasury via PolicyManagerClient ---
    pub fn policy_org(env: Env, policy_id: u64) -> Address {
        env.storage()
            .persistent()
            .get(&MockKey::Org(policy_id))
            .unwrap()
    }

    pub fn org_outstanding_sum_insured(env: Env, org: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&MockKey::Outstanding(org))
            .unwrap_or(0)
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

    // Fund the backend payer with USDC and let it pay a 1_000_000 (1 USDC, 6dp) premium.
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
    assert_eq!(c.treasury.calculate_platform_fee(&2_000_000, &c.org), 100_000);
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
#[should_panic(expected = "#204")] // InsufficientOrgReserve
fn payout_exceeding_org_reserve_reverts() {
    let c = setup();
    c.pm().set_org(&1u64, &c.org);
    c.token_admin.mint(&c.backend, &1_000_000);
    c.treasury.receive_premium(&c.backend, &1u64, &1_000_000); // org reserve = 900_000
    // Ask for more than the org holds.
    c.treasury
        .request_payout(&c.payout_receiver, &1u64, &900_001);
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
    c.treasury.request_payout(&c.payout_receiver, &5u64, &100_000);
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
    // Org has 1000 outstanding sum insured at the default 2000 bps (20%) ratio => required 200.
    c.pm().set_outstanding(&c.org, &1_000);

    c.token_admin.mint(&c.org, &1_000);
    c.treasury.deposit_reserve(&c.org, &c.org, &1_000);
    assert_eq!(c.treasury.org_reserve(&c.org), 1_000);
    assert_eq!(c.treasury.reserve_required(&c.org), 200);
    assert!(c.treasury.meets_reserve_requirements(&c.org));
    c.assert_solvent();

    // Withdraw surplus down to exactly the required reserve (1000 - 800 = 200).
    let to = Address::generate(&c.env);
    c.treasury.withdraw_org_surplus(&c.org, &800, &to);
    assert_eq!(c.treasury.org_reserve(&c.org), 200);
    assert_eq!(c.token.balance(&to), 800);
    c.assert_solvent();
}

#[test]
#[should_panic(expected = "#213")] // WouldBreachReserve
fn withdraw_below_required_reserve_reverts() {
    let c = setup();
    c.pm().set_outstanding(&c.org, &1_000); // required = 200
    c.token_admin.mint(&c.org, &1_000);
    c.treasury.deposit_reserve(&c.org, &c.org, &1_000);
    let to = Address::generate(&c.env);
    // Leaving 199 < required 200.
    c.treasury.withdraw_org_surplus(&c.org, &801, &to);
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
