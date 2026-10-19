#![cfg(test)]
//! Security suite for `PayoutReceiver::submit_determination`.
//!
//! The signature is the SOLE payout authority, so the crux is a real secp256k1 round-trip:
//! we sign the on-chain-reconstructed digest with `k256` (host side, dev-only) and let the
//! contract recover it via `env.crypto().secp256k1_recover`. Mock SettlementAdapter /
//! PolicyManager contracts stand in for the sibling crates and record the cross-calls.
//!
//! Coverage: happy path (payout re-derived, obligation recorded with exact fields, report
//! stored); every guard — unauthorized relayer, paused, bad signature (wrong signer), wrong
//! network domain, sub-score/flag/weighted-invariant, threshold, sum-insured mismatch, wrong
//! payout, stale & future report, farmer claim limit, and replay (persistent `PolicyPaid`
//! guard); the adapter boundary — return-value check (329), unbound adapter (500, atomic),
//! `set_settlement_adapter` handshake (328) and replay across an adapter swap; upgrade gate.

extern crate std;

use soroban_sdk::{
    contract, contractimpl, panic_with_error, symbol_short,
    testutils::{Address as _, Events as _, Ledger as _},
    Address, BytesN, Env, Error, Event as _,
};

use k256::ecdsa::SigningKey;

use microcrop_shared::{
    encoding::determination_digest,
    errors::{AdapterError, CommonError, PayoutError},
    types::{
        CoverageType, CropDetermination, MethodologyKind, Obligation, ObligationStatus, Policy,
        PolicyStatus, Role,
    },
};

use crate::{
    DamageReportReceived, DeterminationVerified, ObligationRequested, PausedError, PayoutReceiver,
    PayoutReceiverClient, SettlementAdapterUpdated,
};

// ============ Mock sibling contracts ============

#[contract]
pub struct MockPolicyManager;

#[contractimpl]
impl MockPolicyManager {
    pub fn __constructor(env: Env, policy: Policy) {
        env.storage()
            .instance()
            .set(&symbol_short!("policy"), &policy);
        env.storage()
            .instance()
            .set(&symbol_short!("canclaim"), &true);
    }
    pub fn set_can_claim(env: Env, v: bool) {
        env.storage().instance().set(&symbol_short!("canclaim"), &v);
    }
    pub fn set_policy(env: Env, policy: Policy) {
        env.storage()
            .instance()
            .set(&symbol_short!("policy"), &policy);
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
        env.storage()
            .instance()
            .get(&symbol_short!("policy"))
            .unwrap()
    }
    pub fn can_farmer_claim(env: Env, _farmer: Address) -> bool {
        env.storage()
            .instance()
            .get(&symbol_short!("canclaim"))
            .unwrap_or(true)
    }
    pub fn mark_as_claimed(env: Env, _caller: Address, _policy_id: u64) {
        let n: u32 = env
            .storage()
            .instance()
            .get(&symbol_short!("claimed"))
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&symbol_short!("claimed"), &(n + 1));
    }
    pub fn increment_claim_count(env: Env, _caller: Address, _farmer: Address) {
        let n: u32 = env
            .storage()
            .instance()
            .get(&symbol_short!("inc"))
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&symbol_short!("inc"), &(n + 1));
    }
    pub fn claimed_calls(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&symbol_short!("claimed"))
            .unwrap_or(0)
    }
    pub fn extend_policy_ttl(env: Env, _policy_id: u64) {
        env.storage().instance().set(&symbol_short!("ttl"), &true);
    }
    pub fn ttl_called(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&symbol_short!("ttl"))
            .unwrap_or(false)
    }
    pub fn extend_contract_ttl(env: Env) {
        env.storage().instance().set(&symbol_short!("keep"), &true);
    }
    pub fn keep_called(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&symbol_short!("keep"))
            .unwrap_or(false)
    }
    pub fn inc_calls(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&symbol_short!("inc"))
            .unwrap_or(0)
    }
}

/// Stand-in for the SettlementAdapter. Behaves like the real one at the boundary the
/// PayoutReceiver relies on (only the bound receiver may record, with its own auth) and
/// records every call. `set_liar(true)` returns an obligation that differs from the request;
/// `set_trap(true)` makes the keeper entrypoints trap.
#[contract]
pub struct MockSettlementAdapter;

#[contractimpl]
impl MockSettlementAdapter {
    pub fn __constructor(_env: Env) {}
    pub fn set_pr(env: Env, pr: Option<Address>) {
        env.storage().instance().set(&symbol_short!("pr"), &pr);
    }
    pub fn payout_receiver(env: Env) -> Option<Address> {
        env.storage()
            .instance()
            .get(&symbol_short!("pr"))
            .unwrap_or(None)
    }
    pub fn set_liar(env: Env, v: bool) {
        env.storage().instance().set(&symbol_short!("liar"), &v);
    }
    pub fn set_trap(env: Env, v: bool) {
        env.storage().instance().set(&symbol_short!("trap"), &v);
    }
    pub fn record_obligation(
        env: Env,
        caller: Address,
        policy_id: u64,
        org: Address,
        amount: i128,
        determination_digest: BytesN<32>,
    ) -> Obligation {
        if Self::payout_receiver(env.clone()) != Some(caller.clone()) {
            panic_with_error!(&env, AdapterError::NotPayoutReceiver);
        }
        caller.require_auth();
        let s = env.storage().instance();
        let n: u64 = s.get(&symbol_short!("n")).unwrap_or(0) + 1;
        s.set(&symbol_short!("n"), &n);
        s.set(&symbol_short!("pid"), &policy_id);
        s.set(&symbol_short!("amt"), &amount);
        s.set(&symbol_short!("org"), &org);
        s.set(&symbol_short!("dig"), &determination_digest);
        s.set(&(symbol_short!("of"), policy_id), &n);
        let liar: bool = s.get(&symbol_short!("liar")).unwrap_or(false);
        Obligation {
            id: n,
            payout_receiver: caller,
            policy_id,
            org,
            amount: if liar { amount + 1 } else { amount },
            determination_digest,
            status: ObligationStatus::Owed,
            recorded_at: env.ledger().timestamp(),
            org_index: 0,
            funded_by: None,
            funded_at: 0,
            funding_reference: None,
            settled_by: None,
            settled_at: 0,
            settlement_reference: None,
        }
    }
    pub fn find_obligation(env: Env, _payout_receiver: Address, policy_id: u64) -> Option<u64> {
        if env
            .storage()
            .instance()
            .get(&symbol_short!("trap"))
            .unwrap_or(false)
        {
            panic!("adapter trap");
        }
        env.storage()
            .instance()
            .get(&(symbol_short!("of"), policy_id))
    }
    pub fn extend_obligation_ttl(env: Env, _obligation_id: u64) {
        let s = env.storage().instance();
        let n: u32 = s.get(&symbol_short!("ext")).unwrap_or(0);
        s.set(&symbol_short!("ext"), &(n + 1));
    }
    pub fn ext_calls(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&symbol_short!("ext"))
            .unwrap_or(0)
    }
    pub fn last_pid(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&symbol_short!("pid"))
            .unwrap_or(0)
    }
    pub fn last_amt(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&symbol_short!("amt"))
            .unwrap_or(0)
    }
    pub fn last_org(env: Env) -> Option<Address> {
        env.storage().instance().get(&symbol_short!("org"))
    }
    pub fn last_digest(env: Env) -> Option<BytesN<32>> {
        env.storage().instance().get(&symbol_short!("dig"))
    }
    pub fn calls(env: Env) -> u32 {
        let n: u64 = env
            .storage()
            .instance()
            .get(&symbol_short!("n"))
            .unwrap_or(0);
        n as u32
    }
}

// ============ Test fixture ============

const NOW: u64 = 1_700_000_000;
const SUM_INSURED: i128 = 10_000_000_000; // 1,000 USDC at 7dp (Stellar SAC)
/// The insured plot (Nairobi), degrees x 1e6.
const PLOT_LAT_E6: i64 = -1_286_389;
const PLOT_LON_E6: i64 = 36_817_223;

struct Fixture {
    env: Env,
    client: PayoutReceiverClient<'static>,
    pr_addr: Address,
    admin: Address,
    pm: MockPolicyManagerClient<'static>,
    adapter: MockSettlementAdapterClient<'static>,
    relayer: Address,
    sk: SigningKey,
    domain: BytesN<32>,
}

/// A canonical, fully-consistent determination: weather present, 50/50 sub-scores ->
/// bp = 60*50 + 40*50 = 5000; payout = SUM_INSURED * 5000 / 10000 = 5_000_000_000.
fn base_det() -> CropDetermination {
    CropDetermination {
        on_chain_policy_id: 1,
        damage_percent_bp: 5_000,
        weather_damage: 50,
        satellite_damage: 50,
        payout_amount: 5_000_000_000,
        assessed_at: NOW,
        latitude_e6: PLOT_LAT_E6 as i128,
        longitude_e6: PLOT_LON_E6 as i128,
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
        lat_e6: PLOT_LAT_E6,
        lon_e6: PLOT_LON_E6,
        sum_insured: SUM_INSURED,
        premium: 500_000_000,
        start_date: NOW - 10,
        end_date: NOW + 1_000_000,
        coverage_type: CoverageType::Comprehensive,
        status: PolicyStatus::Active,
        created_at: NOW - 20,
        org,
    };

    let adapter_id = env.register(MockSettlementAdapter, ());
    let pm_id = env.register(MockPolicyManager, (policy,));
    let pr_addr = env.register(
        PayoutReceiver,
        (adapter_id.clone(), pm_id.clone(), admin.clone()),
    );

    let client = PayoutReceiverClient::new(&env, &pr_addr);
    client.grant_role(&admin, &Role::Relayer, &relayer);

    let sk = sk_from(0x11);
    client.set_authorized_signer(&admin, &pubkey65(&env, &sk));

    let domain = BytesN::from_array(&env, &[0xABu8; 32]);
    client.set_network_domain(&admin, &domain);
    // The crop methodology is retired and disabled by default; these tests exercise it.
    client.set_methodology_enabled(&admin, &MethodologyKind::CropDualIndex10, &true);

    let adapter = MockSettlementAdapterClient::new(&env, &adapter_id);
    // Bind the adapter to this PayoutReceiver, as deploy.sh does.
    adapter.set_pr(&Some(pr_addr.clone()));

    let pm = MockPolicyManagerClient::new(&env, &pm_id);

    Fixture {
        env,
        client,
        pr_addr,
        admin,
        pm,
        adapter,
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

    // Payout re-derived on-chain and recorded as an obligation, then the policy claimed.
    assert_eq!(f.adapter.calls(), 1);
    assert_eq!(f.adapter.last_pid(), 1);
    assert_eq!(f.adapter.last_amt(), 5_000_000_000);
    assert_eq!(f.pm.claimed_calls(), 1);
    assert_eq!(f.pm.inc_calls(), 1);

    // Replay guards set + report stored.
    assert!(f.client.is_policy_paid(&1));
    let report = f.client.get_report(&1);
    assert_eq!(report.policy_id, 1);
    assert_eq!(report.damage_percentage, 5_000);
    assert_eq!(report.payout_amount, 5_000_000_000);
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
    d.payout_amount = SUM_INSURED * 4_000 / 10_000; // 4_000_000_000
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);

    f.client.submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(f.adapter.last_amt(), 4_000_000_000);
}

// ============ Access + lifecycle guards ============

#[test]
fn unauthorized_relayer_rejected() {
    let f = setup();
    let stranger = Address::generate(&f.env); // holds no Relayer role
    let d = base_det();
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);

    let res = f
        .client
        .try_submit_determination(&stranger, &d, &sig, &recid);
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
        lat_e6: PLOT_LAT_E6,
        lon_e6: PLOT_LON_E6,
        sum_insured: SUM_INSURED,
        premium: 500_000_000,
        start_date: NOW - 10,
        end_date: NOW + 1_000_000,
        coverage_type: CoverageType::Comprehensive,
        status: PolicyStatus::Active,
        created_at: NOW - 20,
        org,
    };
    let adapter_id = env.register(MockSettlementAdapter, ());
    let pm_id = env.register(MockPolicyManager, (policy,));
    let pr_addr = env.register(PayoutReceiver, (adapter_id, pm_id, admin.clone()));
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

    let res = f
        .client
        .try_submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(PayoutError::InvalidSignature.into())));
}

#[test]
fn wrong_network_domain_rejected() {
    let f = setup();
    let d = base_det();
    // Correct key, but signature is over a digest bound to a DIFFERENT domain than stored.
    let other_domain = BytesN::from_array(&f.env, &[0x01u8; 32]);
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &other_domain, &d);

    let res = f
        .client
        .try_submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(PayoutError::InvalidSignature.into())));
}

#[test]
fn wrong_contract_address_rejected() {
    let f = setup();
    let d = base_det();
    // Correct key + domain, but signed for a different contract address.
    let other_addr = Address::generate(&f.env);
    let (sig, recid) = sign(&f.env, &f.sk, &other_addr, &f.domain, &d);

    let res = f
        .client
        .try_submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(PayoutError::InvalidSignature.into())));
}

// ============ Bounds / invariant guards ============

#[test]
fn damage_exceeds_maximum_rejected() {
    let f = setup();
    let mut d = base_det();
    d.damage_percent_bp = 10_001;
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    let res = f
        .client
        .try_submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(PayoutError::DamageExceedsMaximum.into())));
}

#[test]
fn subscore_out_of_range_rejected() {
    let f = setup();
    let mut d = base_det();
    d.weather_damage = 101;
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    let res = f
        .client
        .try_submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(PayoutError::SubScoreOutOfRange.into())));
}

#[test]
fn invalid_weather_flag_rejected() {
    let f = setup();
    let mut d = base_det();
    d.weather_present = 2;
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    let res = f
        .client
        .try_submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(PayoutError::InvalidWeatherFlag.into())));
}

#[test]
fn weather_flag_damage_mismatch_rejected() {
    let f = setup();
    let mut d = base_det();
    d.weather_present = 0; // satellite-only ...
    d.weather_damage = 10; // ... but nonzero weather damage
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    let res = f
        .client
        .try_submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(PayoutError::WeatherFlagDamageMismatch.into())));
}

#[test]
fn invalid_weighted_damage_rejected() {
    let f = setup();
    let mut d = base_det();
    // weather present: expected bp = 60*50 + 40*50 = 5000; claim 6000 instead.
    d.damage_percent_bp = 6_000;
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    let res = f
        .client
        .try_submit_determination(&f.relayer, &d, &sig, &recid);
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
    let res = f
        .client
        .try_submit_determination(&f.relayer, &d, &sig, &recid);
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
    let res = f
        .client
        .try_submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(PayoutError::SumInsuredMismatch.into())));
}

#[test]
fn wrong_payout_amount_rejected() {
    let f = setup();
    let mut d = base_det();
    d.payout_amount = 4_999_999_999; // != SUM_INSURED * 5000 / 10000
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    let res = f
        .client
        .try_submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(PayoutError::InvalidPayoutCalculation.into())));
}

// ============ Freshness ============

#[test]
fn future_report_rejected() {
    let f = setup();
    let mut d = base_det();
    d.assessed_at = NOW + 10;
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    let res = f
        .client
        .try_submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(PayoutError::ReportInFuture.into())));
}

#[test]
fn stale_report_rejected() {
    let f = setup();
    let mut d = base_det();
    d.assessed_at = NOW - 3_601; // older than MAX_REPORT_AGE (3600)
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    let res = f
        .client
        .try_submit_determination(&f.relayer, &d, &sig, &recid);
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
        lat_e6: PLOT_LAT_E6,
        lon_e6: PLOT_LON_E6,
        sum_insured: SUM_INSURED,
        premium: 500_000_000,
        start_date: NOW - 10,
        end_date: NOW + 1_000_000,
        coverage_type: CoverageType::Comprehensive,
        status: PolicyStatus::Active,
        created_at: NOW - 20,
        org,
    };
    let adapter_id = env.register(MockSettlementAdapter, ());
    let pm_id = env.register(MockPolicyManager, (policy,));
    let pmc = MockPolicyManagerClient::new(&env, &pm_id);
    pmc.set_can_claim(&false);
    let pr_addr = env.register(PayoutReceiver, (adapter_id, pm_id, admin.clone()));
    let client = PayoutReceiverClient::new(&env, &pr_addr);
    client.grant_role(&admin, &Role::Relayer, &relayer);
    let sk = sk_from(0x11);
    client.set_authorized_signer(&admin, &pubkey65(&env, &sk));
    let domain = BytesN::from_array(&env, &[0xABu8; 32]);
    client.set_network_domain(&admin, &domain);
    client.set_methodology_enabled(&admin, &MethodologyKind::CropDualIndex10, &true);

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
    // triple replay guard fires first (before the ConsumedDetermination / adapter layers).
    let res = f
        .client
        .try_submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(res, Err(Ok(PayoutError::PolicyAlreadyPaid.into())));
    // Only one obligation was ever requested.
    assert_eq!(f.adapter.calls(), 1);
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
        lat_e6: PLOT_LAT_E6,
        lon_e6: PLOT_LON_E6,
        sum_insured: SUM_INSURED,
        premium: 500_000_000,
        start_date: NOW - 10,
        end_date: NOW + 1_000_000,
        coverage_type: CoverageType::Comprehensive,
        status: PolicyStatus::Active,
        created_at: NOW - 20,
        org,
    };
    let adapter_id = env.register(MockSettlementAdapter, ());
    let pm_id = env.register(MockPolicyManager, (policy,));
    let pr_addr = env.register(PayoutReceiver, (adapter_id, pm_id, admin.clone()));
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

// ============ Coverage-type binding (step 6c) ============

#[test]
fn crop_damage_rejects_every_single_peril_coverage() {
    // CROP_DAMAGE is cause-agnostic: it settles Comprehensive cover only.
    for coverage in [
        CoverageType::Drought,
        CoverageType::Flood,
        CoverageType::Both,
        CoverageType::ExcessRain,
    ] {
        let f = setup();
        let mut p = f.pm.get_policy(&1);
        p.coverage_type = coverage;
        f.pm.set_policy(&p);
        let d = base_det();
        let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
        let res = f
            .client
            .try_submit_determination(&f.relayer, &d, &sig, &recid);
        assert_eq!(
            res,
            Err(Ok(PayoutError::CoverageNotSettledByMethodology.into())),
            "{:?} must not settle on a crop-damage determination",
            coverage
        );
        assert_eq!(f.adapter.calls(), 0);
        assert!(!f.client.is_policy_paid(&1));
    }
}

// ============ Plot binding (step 6d) ============

#[test]
fn determination_for_another_location_is_rejected() {
    let f = setup();
    // Latitude 2 micro-degrees off (beyond the 1-unit tolerance).
    let mut d = base_det();
    d.latitude_e6 += 2;
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    assert_eq!(
        f.client
            .try_submit_determination(&f.relayer, &d, &sig, &recid),
        Err(Ok(PayoutError::PlotLocationMismatch.into()))
    );
    // Longitude of a different farm entirely.
    let mut d = base_det();
    d.longitude_e6 = 36_900_000;
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    assert_eq!(
        f.client
            .try_submit_determination(&f.relayer, &d, &sig, &recid),
        Err(Ok(PayoutError::PlotLocationMismatch.into()))
    );
    assert_eq!(f.adapter.calls(), 0);
}

#[test]
fn one_micro_degree_rounding_difference_is_accepted() {
    let f = setup();
    let mut d = base_det();
    d.latitude_e6 -= 1;
    d.longitude_e6 += 1;
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    f.client.submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(f.adapter.calls(), 1);
}

// ============ TTL keep-alive ============

#[test]
fn extend_policy_ttl_bumps_own_entries_the_obligation_and_the_policy_manager() {
    use microcrop_shared::ttl::NETWORK_MAX_ENTRY_TTL_LEDGERS;
    use soroban_sdk::testutils::storage::Persistent as _;

    let f = setup();
    f.env
        .ledger()
        .set_max_entry_ttl(NETWORK_MAX_ENTRY_TTL_LEDGERS);
    let d = base_det();
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    f.client.submit_determination(&f.relayer, &d, &sig, &recid);

    let ttl = |k: &crate::DataKey| {
        f.env
            .as_contract(&f.pr_addr, || f.env.storage().persistent().get_ttl(k))
    };
    // The policy ends 1,000,000 s after NOW: PolicyPaid and Report live end + 30 days.
    let need = (1_000_000u32 + 30 * 86_400).div_ceil(5);
    assert_eq!(ttl(&crate::DataKey::PolicyPaid(1)), need);
    assert_eq!(ttl(&crate::DataKey::Report(1)), need);

    // Permissionless; finds the obligation recorded by THIS receiver and extends it.
    f.env.set_auths(&[]);
    f.client.extend_policy_ttl(&1);
    assert_eq!(f.adapter.ext_calls(), 1);
    assert!(f.pm.ttl_called());
    // Need-based, never the network maximum.
    assert!(ttl(&crate::DataKey::PolicyPaid(1)) < NETWORK_MAX_ENTRY_TTL_LEDGERS);
}

#[test]
fn extend_policy_ttl_survives_an_adapter_that_traps() {
    let f = setup();
    let d = base_det();
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    f.client.submit_determination(&f.relayer, &d, &sig, &recid);
    f.adapter.set_trap(&true);
    f.env.set_auths(&[]);
    // A broken or swapped adapter never breaks the keeper: PolicyManager is still extended.
    f.client.extend_policy_ttl(&1);
    assert_eq!(f.adapter.ext_calls(), 0);
    assert!(f.pm.ttl_called());
}

#[test]
fn extend_contract_ttl_keeps_all_four_contracts_alive() {
    use microcrop_shared::ttl::{KEEPER_TARGET_LEDGERS, USER_TARGET_LEDGERS};
    use soroban_sdk::testutils::storage::Instance as _;

    let f = setup();
    let inst = |a: &Address| {
        f.env
            .as_contract(a, || f.env.storage().instance().get_ttl())
    };
    // An ordinary call extends the instance to 10 days, never the network maximum.
    let d = base_det();
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    f.client.submit_determination(&f.relayer, &d, &sig, &recid);
    assert_eq!(inst(&f.pr_addr), USER_TARGET_LEDGERS);

    f.env.set_auths(&[]);
    f.client.extend_contract_ttl();
    assert_eq!(inst(&f.pr_addr), KEEPER_TARGET_LEDGERS);
    // The SettlementAdapter through the deployer; PolicyManager (and through it PolicyNft)
    // by its own keeper.
    assert_eq!(inst(&f.adapter.address), KEEPER_TARGET_LEDGERS);
    assert!(f.pm.keep_called());
}

// ============ Methodology allow-list ============

#[test]
fn crop_methodology_disabled_is_rejected() {
    let f = setup();
    f.client
        .set_methodology_enabled(&f.admin, &MethodologyKind::CropDualIndex10, &false);
    assert!(!f
        .client
        .is_methodology_enabled(&MethodologyKind::CropDualIndex10));
    let d = base_det();
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    assert_eq!(
        f.client
            .try_submit_determination(&f.relayer, &d, &sig, &recid),
        Err(Ok(PayoutError::MethodologyDisabled.into()))
    );
    assert_eq!(f.adapter.calls(), 0);
}

#[test]
fn only_the_admin_sets_a_methodology() {
    let f = setup();
    let stranger = Address::generate(&f.env);
    assert_eq!(
        f.client
            .try_set_methodology_enabled(&stranger, &MethodologyKind::CropDroughtRdi10, &true),
        Err(Ok(CommonError::Unauthorized.into()))
    );
    assert!(!f
        .client
        .is_methodology_enabled(&MethodologyKind::CropDroughtRdi10));
}

// ============ SettlementAdapter boundary ============

fn submit_ok(f: &Fixture) -> CropDetermination {
    let d = base_det();
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    f.client.submit_determination(&f.relayer, &d, &sig, &recid);
    d
}

#[test]
fn the_obligation_carries_exactly_the_verified_claim_and_the_events_say_so() {
    let f = setup();
    let d = base_det();
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    f.client.submit_determination(&f.relayer, &d, &sig, &recid);
    // Events reset on every call: take them before any read.
    let events = f.env.events().all().filter_by_contract(&f.pr_addr);

    let policy = f.pm.get_policy(&1);
    let digest = determination_digest(&f.env, &d, &f.pr_addr, &f.domain).to_bytes();
    assert_eq!(f.adapter.last_pid(), 1);
    assert_eq!(f.adapter.last_org(), Some(policy.org.clone()));
    assert_eq!(f.adapter.last_amt(), d.payout_amount);
    assert_eq!(f.adapter.last_digest(), Some(digest.clone()));

    let expected = std::vec![
        DeterminationVerified {
            policy_id: 1,
            preimage_hash: digest,
        }
        .to_xdr(&f.env, &f.pr_addr),
        DamageReportReceived {
            policy_id: 1,
            farmer: policy.farmer.clone(),
            damage_percentage: 5_000,
            payout_amount: 5_000_000_000,
        }
        .to_xdr(&f.env, &f.pr_addr),
        ObligationRequested {
            policy_id: 1,
            obligation_id: 1,
            adapter: f.adapter.address.clone(),
            amount: 5_000_000_000,
        }
        .to_xdr(&f.env, &f.pr_addr),
    ];
    assert_eq!(events, expected);
}

#[test]
fn only_the_relayer_signs_a_submission() {
    let f = setup();
    let d = base_det();
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    f.client.submit_determination(&f.relayer, &d, &sig, &recid);
    // The PayoutReceiver's own authority over the adapter and PolicyManager calls comes from
    // the direct contract call, not from any signature: the relayer is the only root.
    let auths = f.env.auths();
    assert_eq!(auths.len(), 1, "{:?}", auths);
    assert_eq!(auths[0].0, f.relayer);
}

#[test]
fn a_lying_adapter_is_refused_and_nothing_persists() {
    let f = setup();
    f.adapter.set_liar(&true);
    let d = base_det();
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    assert_eq!(
        f.client
            .try_submit_determination(&f.relayer, &d, &sig, &recid),
        Err(Ok(PayoutError::ObligationNotRecorded.into()))
    );
    assert!(!f.client.is_policy_paid(&1));
    assert_eq!(f.client.get_report(&1).policy_id, 0);
    assert_eq!(f.adapter.calls(), 0, "the adapter's write reverted too");
    assert_eq!(f.pm.claimed_calls(), 0);
    assert_eq!(f.pm.inc_calls(), 0);
}

#[test]
fn an_unbound_adapter_reverts_the_whole_submission() {
    let f = setup();
    f.adapter.set_pr(&None);
    let d = base_det();
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    assert_eq!(
        f.client
            .try_submit_determination(&f.relayer, &d, &sig, &recid),
        Err(Ok(Error::from_contract_error(
            AdapterError::NotPayoutReceiver as u32
        )))
    );
    assert!(!f.client.is_policy_paid(&1));
    assert_eq!(f.pm.claimed_calls(), 0);

    // Once bound, the same determination settles (it was never consumed).
    f.adapter.set_pr(&Some(f.pr_addr.clone()));
    f.client.submit_determination(&f.relayer, &d, &sig, &recid);
    assert!(f.client.is_policy_paid(&1));
}

fn new_adapter(f: &Fixture, bound_to: Option<Address>) -> MockSettlementAdapterClient<'static> {
    let id = f.env.register(MockSettlementAdapter, ());
    let a = MockSettlementAdapterClient::new(&f.env, &id);
    a.set_pr(&bound_to);
    a
}

#[test]
fn set_settlement_adapter_requires_a_bound_adapter_and_the_admin() {
    let f = setup();
    assert_eq!(f.client.settlement_adapter(), f.adapter.address);
    let bound = new_adapter(&f, Some(f.pr_addr.clone()));

    // Role, then signature.
    let stranger = Address::generate(&f.env);
    assert_eq!(
        f.client
            .try_set_settlement_adapter(&stranger, &bound.address),
        Err(Ok(CommonError::Unauthorized.into()))
    );
    f.env.set_auths(&[]);
    assert!(f
        .client
        .try_set_settlement_adapter(&f.admin, &bound.address)
        .is_err());
    f.env.mock_all_auths();

    // Unbound, bound elsewhere, this contract, and a contract that is not an adapter.
    let unbound = new_adapter(&f, None);
    let elsewhere = new_adapter(&f, Some(Address::generate(&f.env)));
    for bad in [
        unbound.address.clone(),
        elsewhere.address.clone(),
        f.pr_addr.clone(),
        f.pm.address.clone(),
    ] {
        assert_eq!(
            f.client.try_set_settlement_adapter(&f.admin, &bad),
            Err(Ok(PayoutError::SettlementAdapterNotBound.into()))
        );
    }
    assert_eq!(f.client.settlement_adapter(), f.adapter.address);

    // A bound adapter is accepted, with an event, and used from the next submission on.
    f.client.set_settlement_adapter(&f.admin, &bound.address);
    assert_eq!(
        f.env.events().all().filter_by_contract(&f.pr_addr),
        std::vec![SettlementAdapterUpdated {
            previous: f.adapter.address.clone(),
            adapter: bound.address.clone(),
        }
        .to_xdr(&f.env, &f.pr_addr)]
    );
    assert_eq!(f.client.settlement_adapter(), bound.address);
    submit_ok(&f);
    assert_eq!(bound.calls(), 1);
    assert_eq!(f.adapter.calls(), 0);
}

#[test]
fn set_settlement_adapter_works_while_paused() {
    let f = setup();
    f.client.pause(&f.admin);
    let bound = new_adapter(&f, Some(f.pr_addr.clone()));
    f.client.set_settlement_adapter(&f.admin, &bound.address);
    assert_eq!(f.client.settlement_adapter(), bound.address);
}

/// The key security case: `PolicyPaid` lives in the PayoutReceiver, so a fresh adapter (with
/// no record of the policy) cannot be used to settle it a second time.
#[test]
fn replay_after_an_adapter_swap_is_still_already_paid() {
    let f = setup();
    let d = submit_ok(&f);
    let bound = new_adapter(&f, Some(f.pr_addr.clone()));
    f.client.set_settlement_adapter(&f.admin, &bound.address);

    // The same determination...
    let (sig, recid) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    assert_eq!(
        f.client
            .try_submit_determination(&f.relayer, &d, &sig, &recid),
        Err(Ok(PayoutError::PolicyAlreadyPaid.into()))
    );
    // ...and a different, validly signed one.
    f.env.ledger().set_timestamp(NOW + 60);
    let mut d2 = base_det();
    d2.weather_damage = 100;
    d2.satellite_damage = 100;
    d2.damage_percent_bp = 10_000;
    d2.payout_amount = SUM_INSURED;
    d2.assessed_at = NOW + 60;
    let (sig2, recid2) = sign(&f.env, &f.sk, &f.pr_addr, &f.domain, &d2);
    assert_eq!(
        f.client
            .try_submit_determination(&f.relayer, &d2, &sig2, &recid2),
        Err(Ok(PayoutError::PolicyAlreadyPaid.into()))
    );
    assert_eq!(bound.calls(), 0);
    assert_eq!(f.adapter.calls(), 1);
}

// ============ Upgrade gate ============

#[test]
fn upgrade_is_refused_to_non_upgraders_and_without_a_signature() {
    let f = setup();
    let hash = BytesN::from_array(&f.env, &[7u8; 32]);
    assert_eq!(
        f.client.try_upgrade(&f.relayer, &hash),
        Err(Ok(CommonError::Unauthorized.into()))
    );
    f.env.set_auths(&[]);
    match f.client.try_upgrade(&f.admin, &hash) {
        Err(Ok(e)) => assert!(
            !e.is_type(soroban_sdk::xdr::ScErrorType::Contract),
            "expected a missing-auth host error, got {:?}",
            e
        ),
        other => panic!("expected a missing-auth host error, got {:?}", other),
    }
}
