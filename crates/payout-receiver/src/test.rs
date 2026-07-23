#![cfg(test)]
//! Security suite for `PayoutReceiver::submit_determination`.
//!
//! The signature is the SOLE payout authority, so the crux is a real secp256k1 round-trip:
//! we sign the on-chain-reconstructed digest with `k256` (host side, dev-only) and let the
//! contract recover it via `env.crypto().secp256k1_recover`. Mock Treasury / PolicyManager
//! contracts stand in for the (still-stubbed) sibling crates and record the cross-calls.
//!
//! Coverage: happy path (premium math re-derived, payout dispatched, report stored); every
//! guard — unauthorized relayer, paused, bad signature (wrong signer), wrong network domain,
//! sub-score/flag/weighted-invariant, threshold, sum-insured mismatch, wrong payout, stale &
//! future report, farmer claim limit, and replay (persistent `PolicyPaid` guard).

extern crate std;

use soroban_sdk::{
    contract, contractimpl, symbol_short,
    testutils::{Address as _, Ledger as _},
    Address, BytesN, Env,
};

use k256::ecdsa::SigningKey;

use microcrop_shared::{
    encoding::determination_digest,
    errors::{CommonError, PayoutError},
    types::{CoverageType, CropDetermination, Policy, PolicyStatus, Role},
};

use crate::{PausedError, PayoutReceiver, PayoutReceiverClient};

// ============ Mock sibling contracts ============

#[contract]
pub struct MockPolicyManager;

#[contractimpl]
impl MockPolicyManager {
    pub fn __constructor(env: Env, policy: Policy) {
        env.storage().instance().set(&symbol_short!("policy"), &policy);
        env.storage().instance().set(&symbol_short!("canclaim"), &true);
    }
    pub fn set_can_claim(env: Env, v: bool) {
        env.storage().instance().set(&symbol_short!("canclaim"), &v);
    }
    pub fn set_policy(env: Env, policy: Policy) {
        env.storage().instance().set(&symbol_short!("policy"), &policy);
    }
    pub fn policy_exists(env: Env, policy_id: u64) -> bool {
        match env
            .storage()
            .instance()
            .get::<_, Policy>(&symbol_short!("policy"))
        {
            Some(p) => p.id == policy_id,
            None => false,
        }
    }
    pub fn get_policy(env: Env, _policy_id: u64) -> Policy {
        env.storage().instance().get(&symbol_short!("policy")).unwrap()
    }
    pub fn can_farmer_claim(env: Env, _farmer: Address) -> bool {
        env.storage()
            .instance()
            .get(&symbol_short!("canclaim"))
            .unwrap_or(true)
    }
    pub fn mark_as_claimed(env: Env, _caller: Address, _policy_id: u64) {
        let n: u32 = env.storage().instance().get(&symbol_short!("claimed")).unwrap_or(0);
        env.storage().instance().set(&symbol_short!("claimed"), &(n + 1));
    }
    pub fn increment_claim_count(env: Env, _caller: Address, _farmer: Address) {
        let n: u32 = env.storage().instance().get(&symbol_short!("inc")).unwrap_or(0);
        env.storage().instance().set(&symbol_short!("inc"), &(n + 1));
    }
    pub fn claimed_calls(env: Env) -> u32 {
        env.storage().instance().get(&symbol_short!("claimed")).unwrap_or(0)
    }
    pub fn inc_calls(env: Env) -> u32 {
        env.storage().instance().get(&symbol_short!("inc")).unwrap_or(0)
    }
}

#[contract]
pub struct MockTreasury;

#[contractimpl]
impl MockTreasury {
    pub fn __constructor(_env: Env) {}
    pub fn request_payout(env: Env, _caller: Address, policy_id: u64, amount: i128) {
        env.storage().instance().set(&symbol_short!("pid"), &policy_id);
        env.storage().instance().set(&symbol_short!("amt"), &amount);
        let n: u32 = env.storage().instance().get(&symbol_short!("n")).unwrap_or(0);
        env.storage().instance().set(&symbol_short!("n"), &(n + 1));
    }
    pub fn last_pid(env: Env) -> u64 {
        env.storage().instance().get(&symbol_short!("pid")).unwrap_or(0)
    }
    pub fn last_amt(env: Env) -> i128 {
        env.storage().instance().get(&symbol_short!("amt")).unwrap_or(0)
    }
    pub fn calls(env: Env) -> u32 {
        env.storage().instance().get(&symbol_short!("n")).unwrap_or(0)
    }
}

// ============ Test fixture ============

const NOW: u64 = 1_700_000_000;
const SUM_INSURED: i128 = 1_000_000; // $1 at 6dp

struct Fixture {
    env: Env,
    client: PayoutReceiverClient<'static>,
    pr_addr: Address,
    treasury: MockTreasuryClient<'static>,
    relayer: Address,
    sk: SigningKey,
    domain: BytesN<32>,
}

/// A canonical, fully-consistent determination: weather present, 50/50 sub-scores ->
/// bp = 60*50 + 40*50 = 5000; payout = SUM_INSURED * 5000 / 10000 = 500_000.
fn base_det() -> CropDetermination {
    CropDetermination {
        on_chain_policy_id: 1,
        damage_percent_bp: 5_000,
        weather_damage: 50,
        satellite_damage: 50,
        payout_amount: 500_000,
        assessed_at: NOW,
        latitude_e6: -1_286_389,
        longitude_e6: 36_817_223,
        sum_insured: SUM_INSURED,
        ndvi_scaled: 4_200,
        weather_present: 1,
        weather_temp_c_e2: 2_750,
        weather_precip_e2: 15,
        weather_humidity: 80,
        weather_wind_e2: 350,
    }
}

fn sk_from(byte: u8) -> SigningKey {
    SigningKey::from_slice(&[byte; 32]).unwrap()
}

fn pubkey65(env: &Env, sk: &SigningKey) -> BytesN<65> {
    let vk = sk.verifying_key();
    let ep = vk.to_encoded_point(false);
    let mut arr = [0u8; 65];
    arr.copy_from_slice(ep.as_bytes());
    BytesN::from_array(env, &arr)
}

/// Sign the digest the contract will reconstruct for `d`, bound to `pr_addr` + `domain`.
fn sign(
    env: &Env,
    sk: &SigningKey,
    pr_addr: &Address,
    domain: &BytesN<32>,
    d: &CropDetermination,
) -> (BytesN<64>, u32) {
    let digest = determination_digest(env, d, pr_addr, domain);
    let (sig, recid) = sk.sign_prehash_recoverable(&digest.to_array()).unwrap();
    let mut arr = [0u8; 64];
    arr.copy_from_slice(&sig.to_bytes());
    (BytesN::from_array(env, &arr), recid.to_byte() as u32)
}

fn setup() -> Fixture {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(NOW);

    let admin = Address::generate(&env);
    let relayer = Address::generate(&env);
    let farmer = Address::generate(&env);
    let org = Address::generate(&env);

    let policy = Policy {
        id: 1,
        farmer,
        plot_id: 7,
        sum_insured: SUM_INSURED,
        premium: 50_000,
        start_date: NOW - 10,
        end_date: NOW + 1_000_000,
        coverage_type: CoverageType::Drought,
        status: PolicyStatus::Active,
        created_at: NOW - 20,
        org,
    };

    let treasury_id = env.register(MockTreasury, ());
    let pm_id = env.register(MockPolicyManager, (policy,));
    let pr_addr = env.register(PayoutReceiver, (treasury_id.clone(), pm_id.clone(), admin.clone()));

    let client = PayoutReceiverClient::new(&env, &pr_addr);
    client.grant_role(&admin, &Role::Relayer, &relayer);

    let sk = sk_from(0x11);
    client.set_authorized_signer(&admin, &pubkey65(&env, &sk));

    let domain = BytesN::from_array(&env, &[0xABu8; 32]);
    client.set_network_domain(&admin, &domain);

    let treasury = MockTreasuryClient::new(&env, &treasury_id);

    Fixture {
        env,
        client,
        pr_addr,
        treasury,
        relayer,
        sk,
        domain,
    }
}

// ============ Happy path ============

#[test]
fn happy_path_verifies_and_pays_out() {
    let f = setup();
    let d = base_det();
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);

    f.client.submit_determination(&f.relayer, &d, &sig, &recid);

    // Payout re-derived on-chain and dispatched to Treasury.
    assert_eq!(f.treasury.calls(), 1);
    assert_eq!(f.treasury.last_pid(), 1);
    assert_eq!(f.treasury.last_amt(), 500_000);

    // Replay guards set + report stored.
    assert!(f.client.is_policy_paid(&1));
    let report = f.client.get_report(&1);
    assert_eq!(report.policy_id, 1);
    assert_eq!(report.damage_percentage, 5_000);
    assert_eq!(report.payout_amount, 500_000);
    assert_eq!(report.weather_damage, 50);
    assert_eq!(report.satellite_damage, 50);
}

#[test]
fn satellite_only_renormalizes_to_full_weight() {
    // weather absent -> bp = 100 * satellite (renormalized), weather_damage must be 0.
    let f = setup();
    let mut d = base_det();
    d.weather_present = 0;
    d.weather_damage = 0;
    d.satellite_damage = 40;
    d.damage_percent_bp = 4_000; // 100 * 40
    d.payout_amount = SUM_INSURED * 4_000 / 10_000; // 400_000
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);

    f.client.submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(f.treasury.last_amt(), 400_000);
}

// ============ Access + lifecycle guards ============

#[test]
fn unauthorized_relayer_rejected() {
    let f = setup();
    let stranger = Address::generate(&f.env); // holds no Relayer role
    let d = base_det();
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);

    let res = f.client.try_submit_determination(&stranger, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(CommonError::Unauthorized.into())));
}

#[test]
fn paused_blocks_submit() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(NOW);

    let admin = Address::generate(&env);
    let relayer = Address::generate(&env);
    let farmer = Address::generate(&env);
    let org = Address::generate(&env);
    let policy = Policy {
        id: 1,
        farmer: farmer.clone(),
        plot_id: 7,
        sum_insured: SUM_INSURED,
        premium: 50_000,
        start_date: NOW - 10,
        end_date: NOW + 1_000_000,
        coverage_type: CoverageType::Drought,
        status: PolicyStatus::Active,
        created_at: NOW - 20,
        org,
    };
    let treasury_id = env.register(MockTreasury, ());
    let pm_id = env.register(MockPolicyManager, (policy,));
    let pr_addr = env.register(PayoutReceiver, (treasury_id, pm_id, admin.clone()));
    let client = PayoutReceiverClient::new(&env, &pr_addr);
    client.grant_role(&admin, &Role::Relayer, &relayer);
    let sk = sk_from(0x11);
    client.set_authorized_signer(&admin, &pubkey65(&env, &sk));
    let domain = BytesN::from_array(&env, &[0xABu8; 32]);
    client.set_network_domain(&admin, &domain);

    client.pause(&admin);
    let d = base_det();
    let (sig, recid) = sign(&env, &sk, &pr_addr, &domain, &d);
    let res = client.try_submit_determination(&relayer, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(PausedError::ContractPaused.into())));
}

// ============ Signature / domain binding ============

#[test]
fn wrong_signer_rejected() {
    let f = setup();
    let d = base_det();
    // Sign with a DIFFERENT key than the configured authorized signer.
    let attacker = sk_from(0x22);
    let (sig, recid) = sign(&f.env, &attacker, &f.pr_addr, &f.domain, &d);

    let res = f.client.try_submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(PayoutError::InvalidSignature.into())));
}

#[test]
fn wrong_network_domain_rejected() {
    let f = setup();
    let d = base_det();
    // Correct key, but signature is over a digest bound to a DIFFERENT domain than stored.
    let other_domain = BytesN::from_array(&f.env, &[0x01u8; 32]);
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &other_domain, &d);

    let res = f.client.try_submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(PayoutError::InvalidSignature.into())));
}

#[test]
fn wrong_contract_address_rejected() {
    let f = setup();
    let d = base_det();
    // Correct key + domain, but signed for a different contract address.
    let other_addr = Address::generate(&f.env);
    let (sig, recid) = sign(&f.env, &f.sk, &other_addr, &f.domain, &d);

    let res = f.client.try_submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(PayoutError::InvalidSignature.into())));
}

// ============ Bounds / invariant guards ============

#[test]
fn damage_exceeds_maximum_rejected() {
    let f = setup();
    let mut d = base_det();
    d.damage_percent_bp = 10_001;
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    let res = f.client.try_submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(PayoutError::DamageExceedsMaximum.into())));
}

#[test]
fn subscore_out_of_range_rejected() {
    let f = setup();
    let mut d = base_det();
    d.weather_damage = 101;
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    let res = f.client.try_submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(PayoutError::SubScoreOutOfRange.into())));
}

#[test]
fn invalid_weather_flag_rejected() {
    let f = setup();
    let mut d = base_det();
    d.weather_present = 2;
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    let res = f.client.try_submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(PayoutError::InvalidWeatherFlag.into())));
}

#[test]
fn weather_flag_damage_mismatch_rejected() {
    let f = setup();
    let mut d = base_det();
    d.weather_present = 0; // satellite-only ...
    d.weather_damage = 10; // ... but nonzero weather damage
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    let res = f.client.try_submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(PayoutError::WeatherFlagDamageMismatch.into())));
}

#[test]
fn invalid_weighted_damage_rejected() {
    let f = setup();
    let mut d = base_det();
    // weather present: expected bp = 60*50 + 40*50 = 5000; claim 6000 instead.
    d.damage_percent_bp = 6_000;
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    let res = f.client.try_submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(PayoutError::InvalidWeightedDamage.into())));
}

#[test]
fn below_threshold_rejected() {
    let f = setup();
    let mut d = base_det();
    // 20/20 -> bp = 60*20 + 40*20 = 2000 (< 3000) but a valid weighted invariant.
    d.weather_damage = 20;
    d.satellite_damage = 20;
    d.damage_percent_bp = 2_000;
    d.payout_amount = SUM_INSURED * 2_000 / 10_000;
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    let res = f.client.try_submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(PayoutError::DamageBelowThreshold.into())));
}

// ============ Policy binding / payout math ============

#[test]
fn sum_insured_mismatch_rejected() {
    let f = setup();
    let mut d = base_det();
    d.sum_insured = SUM_INSURED + 1; // does not match the on-chain policy
    // keep payout consistent with the FALSE sum so only the sum check can fire
    d.payout_amount = (SUM_INSURED + 1) * 5_000 / 10_000;
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    let res = f.client.try_submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(PayoutError::SumInsuredMismatch.into())));
}

#[test]
fn wrong_payout_amount_rejected() {
    let f = setup();
    let mut d = base_det();
    d.payout_amount = 499_999; // != SUM_INSURED * 5000 / 10000
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    let res = f.client.try_submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(PayoutError::InvalidPayoutCalculation.into())));
}

// ============ Freshness ============

#[test]
fn future_report_rejected() {
    let f = setup();
    let mut d = base_det();
    d.assessed_at = NOW + 10;
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    let res = f.client.try_submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(PayoutError::ReportInFuture.into())));
}

#[test]
fn stale_report_rejected() {
    let f = setup();
    let mut d = base_det();
    d.assessed_at = NOW - 3_601; // older than MAX_REPORT_AGE (3600)
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    let res = f.client.try_submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(PayoutError::ReportTooOld.into())));
}

// ============ Farmer limit ============

#[test]
fn farmer_over_claim_limit_rejected() {
    // Dedicated setup so we can flip the mock PolicyManager's canFarmerClaim -> false.
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(NOW);
    let admin = Address::generate(&env);
    let relayer = Address::generate(&env);
    let farmer = Address::generate(&env);
    let org = Address::generate(&env);
    let policy = Policy {
        id: 1,
        farmer: farmer.clone(),
        plot_id: 7,
        sum_insured: SUM_INSURED,
        premium: 50_000,
        start_date: NOW - 10,
        end_date: NOW + 1_000_000,
        coverage_type: CoverageType::Drought,
        status: PolicyStatus::Active,
        created_at: NOW - 20,
        org,
    };
    let treasury_id = env.register(MockTreasury, ());
    let pm_id = env.register(MockPolicyManager, (policy,));
    let pmc = MockPolicyManagerClient::new(&env, &pm_id);
    pmc.set_can_claim(&false);
    let pr_addr = env.register(PayoutReceiver, (treasury_id, pm_id, admin.clone()));
    let client = PayoutReceiverClient::new(&env, &pr_addr);
    client.grant_role(&admin, &Role::Relayer, &relayer);
    let sk = sk_from(0x11);
    client.set_authorized_signer(&admin, &pubkey65(&env, &sk));
    let domain = BytesN::from_array(&env, &[0xABu8; 32]);
    client.set_network_domain(&admin, &domain);

    let d = base_det();
    let (sig, recid) = sign(&env, &sk, &pr_addr, &domain, &d);
    let res = client.try_submit_determination(&relayer, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(PayoutError::FarmerClaimLimitExceeded.into())));
}

// ============ Replay ============

#[test]
fn replay_rejected_by_policy_paid_guard() {
    let f = setup();
    let d = base_det();
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);

    // First submission succeeds.
    f.client.submit_determination(&f.relayer, &d, &sig, &recid);
    assert!(f.client.is_policy_paid(&1));

    // Second (identical) submission is rejected — the persistent PolicyPaid layer of the
    // triple replay guard fires first (before the ConsumedDetermination/Treasury layers).
    let res = f.client.try_submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(PayoutError::PolicyAlreadyPaid.into())));
    // Treasury was only paid once.
    assert_eq!(f.treasury.calls(), 1);
}

// ============ Signer-not-configured ============

#[test]
fn signer_not_configured_rejected() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(NOW);
    let admin = Address::generate(&env);
    let relayer = Address::generate(&env);
    let farmer = Address::generate(&env);
    let org = Address::generate(&env);
    let policy = Policy {
        id: 1,
        farmer,
        plot_id: 7,
        sum_insured: SUM_INSURED,
        premium: 50_000,
        start_date: NOW - 10,
        end_date: NOW + 1_000_000,
        coverage_type: CoverageType::Drought,
        status: PolicyStatus::Active,
        created_at: NOW - 20,
        org,
    };
    let treasury_id = env.register(MockTreasury, ());
    let pm_id = env.register(MockPolicyManager, (policy,));
    let pr_addr = env.register(PayoutReceiver, (treasury_id, pm_id, admin.clone()));
    let client = PayoutReceiverClient::new(&env, &pr_addr);
    client.grant_role(&admin, &Role::Relayer, &relayer);
    // NOTE: authorized signer intentionally NOT configured.
    let domain = BytesN::from_array(&env, &[0xABu8; 32]);
    client.set_network_domain(&admin, &domain);

    let sk = sk_from(0x11);
    let d = base_det();
    let (sig, recid) = sign(&env, &sk, &pr_addr, &domain, &d);
    let res = client.try_submit_determination(&relayer, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(PayoutError::SignerNotConfigured.into())));
}
