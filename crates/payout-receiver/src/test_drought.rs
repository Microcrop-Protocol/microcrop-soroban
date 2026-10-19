//! Security suite for `PayoutReceiver::submit_drought_determination` (CROP_DROUGHT,
//! `crop-drought-rdi-1.0`): one test per check, in the order the contract runs them, plus
//! replay across both methodologies, the pending path, archived-entry safety and the view.
//!
//! The signature is a real secp256k1 round-trip: `k256` signs the digest the contract
//! rebuilds and the host's `secp256k1_recover` recovers it.

extern crate std;

use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    Address, BytesN, Env,
};

use k256::ecdsa::{Signature, SigningKey};

use microcrop_shared::{
    encoding::{determination_digest, drought_determination_digest, DROUGHT_PARAMS_HASH_RDI_1_0},
    errors::{CommonError, PayoutError},
    types::{
        CoverageType, CropDetermination, DroughtDetermination, MethodologyKind, Policy,
        PolicyStatus, Role,
    },
};

use crate::test::{MockPolicyManager, MockPolicyManagerClient, MockTreasury, MockTreasuryClient};
use crate::{PausedError, PayoutReceiver, PayoutReceiverClient};

// ============ Season fixture ============

const DAY: u64 = 86_400;
/// Planting day 2025-04-01 00:00 UTC (`plantDays * 86400`).
const PLANT: u64 = 1_743_465_600;
/// Cover activated at 06:00 UTC on planting day.
const START: u64 = PLANT + 6 * 3_600;
/// 150 days of cover.
const END: u64 = START + 150 * DAY;
/// Assessed 117 days after planting (110-day window + 7-day archive lag).
const ASSESSED: u64 = PLANT + 117 * DAY;
/// The relayer submits a minute after the assessment.
const NOW: u64 = ASSESSED + 60;
const SUM_INSURED: i128 = 10_000_000_000; // 1,000 USDC at 7dp
const PLOT_LAT_E6: i64 = -1_286_389;
const PLOT_LON_E6: i64 = 36_817_223;

struct F {
    env: Env,
    client: PayoutReceiverClient<'static>,
    pr_addr: Address,
    admin: Address,
    pm: MockPolicyManagerClient<'static>,
    treasury: MockTreasuryClient<'static>,
    relayer: Address,
    sk: SigningKey,
    domain: BytesN<32>,
}

fn policy(env: &Env) -> Policy {
    Policy {
        id: 1,
        farmer: Address::generate(env),
        plot_id: 7,
        lat_e6: PLOT_LAT_E6,
        lon_e6: PLOT_LON_E6,
        sum_insured: SUM_INSURED,
        premium: 500_000_000,
        start_date: START,
        end_date: END,
        coverage_type: CoverageType::Drought,
        status: PolicyStatus::Active,
        created_at: START,
        org: Address::generate(env),
    }
}

fn sk_from(byte: u8) -> SigningKey {
    SigningKey::from_slice(&[byte; 32]).unwrap()
}

fn pubkey65(env: &Env, sk: &SigningKey) -> BytesN<65> {
    let ep = sk.verifying_key().to_encoded_point(false);
    let mut arr = [0u8; 65];
    arr.copy_from_slice(ep.as_bytes());
    BytesN::from_array(env, &arr)
}

/// Deploy with the signer and domain set. `drought_on` enables CropDroughtRdi10.
fn deploy(drought_on: bool, signer: bool) -> F {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(NOW);
    let admin = Address::generate(&env);
    let relayer = Address::generate(&env);
    let p = policy(&env);
    let treasury_id = env.register(MockTreasury, ());
    let pm_id = env.register(MockPolicyManager, (p,));
    let pr_addr = env.register(
        PayoutReceiver,
        (treasury_id.clone(), pm_id.clone(), admin.clone()),
    );
    let client = PayoutReceiverClient::new(&env, &pr_addr);
    client.grant_role(&admin, &Role::Relayer, &relayer);
    let sk = sk_from(0x11);
    if signer {
        client.set_authorized_signer(&admin, &pubkey65(&env, &sk));
    }
    let domain = BytesN::from_array(&env, &[0xABu8; 32]);
    client.set_network_domain(&admin, &domain);
    if drought_on {
        client.set_methodology_enabled(&admin, &MethodologyKind::CropDroughtRdi10, &true);
    }
    F {
        pm: MockPolicyManagerClient::new(&env, &pm_id),
        treasury: MockTreasuryClient::new(&env, &treasury_id),
        env,
        client,
        pr_addr,
        admin,
        relayer,
        sk,
        domain,
    }
}

fn setup() -> F {
    deploy(true, true)
}

/// V1: W1 at its normal, W2 at 70% of normal -> drought 45 -> 4,500 bp -> 450 USDC.
fn det() -> DroughtDetermination {
    let env = Env::default();
    DroughtDetermination {
        on_chain_policy_id: 1,
        damage_percent_bp: 4_500,
        drought_damage: 45,
        payout_amount: 4_500_000_000,
        assessed_at: ASSESSED,
        latitude_e6: PLOT_LAT_E6 as i128,
        longitude_e6: PLOT_LON_E6 as i128,
        sum_insured: SUM_INSURED,
        season_start_epoch: PLANT,
        lgp_days: 180,
        rain_w1_e2: 20_000,
        rain_ref_w1_e2: 20_000,
        rain_w2_e2: 12_600,
        rain_ref_w2_e2: 18_000,
        cdd_w2_days: 21,
        source_hash: BytesN::from_array(&env, &[0x09; 32]),
        methodology_params_hash: BytesN::from_array(&env, &DROUGHT_PARAMS_HASH_RDI_1_0),
    }
}

/// Re-home a fixture value into `env` (BytesN fields are env-bound).
fn on(env: &Env, d: &DroughtDetermination) -> DroughtDetermination {
    let mut x = d.clone();
    x.source_hash = BytesN::from_array(env, &d.source_hash.to_array());
    x.methodology_params_hash = BytesN::from_array(env, &d.methodology_params_hash.to_array());
    x
}

fn sign_with(
    env: &Env,
    sk: &SigningKey,
    contract: &Address,
    domain: &BytesN<32>,
    d: &DroughtDetermination,
) -> (BytesN<64>, u32) {
    let digest = drought_determination_digest(env, d, contract, domain);
    let (sig, recid) = sk.sign_prehash_recoverable(&digest.to_array()).unwrap();
    let mut arr = [0u8; 64];
    arr.copy_from_slice(&sig.to_bytes());
    (BytesN::from_array(env, &arr), recid.to_byte() as u32)
}

impl F {
    fn submit(&self, d: &DroughtDetermination) -> Result<(), PayoutError> {
        let d = on(&self.env, d);
        let (sig, recid) = sign_with(&self.env, &self.sk, &self.pr_addr, &self.domain, &d);
        self.raw(&d, &sig, recid)
    }

    fn raw(
        &self,
        d: &DroughtDetermination,
        sig: &BytesN<64>,
        recid: u32,
    ) -> Result<(), PayoutError> {
        match self
            .client
            .try_submit_drought_determination(&self.relayer, d, sig, &recid)
        {
            Ok(Ok(())) => Ok(()),
            Err(Ok(e)) => Err(PayoutError::try_from(e).unwrap_or_else(|_| {
                panic!("not a PayoutError: {:?}", e);
            })),
            other => panic!("unexpected result {:?}", other),
        }
    }

    fn expect(&self, d: &DroughtDetermination, e: PayoutError) {
        assert_eq!(self.submit(d), Err(e));
        assert_eq!(self.treasury.calls(), 0, "nothing may be paid on {:?}", e);
        assert!(!self.client.is_policy_paid(&1));
    }

    fn set_policy(&self, f: impl FnOnce(&mut Policy)) {
        let mut p = self.pm.get_policy(&1);
        f(&mut p);
        self.pm.set_policy(&p);
    }
}

/// A determination with drought `dd` derived from W2 alone (W1 at normal): `w2 / 18000`.
fn with_w2(w2: u128, dd: u32) -> DroughtDetermination {
    let mut d = det();
    d.rain_w2_e2 = w2;
    d.drought_damage = dd;
    d.damage_percent_bp = dd * 100;
    d.payout_amount = SUM_INSURED * (dd as i128 * 100) / 10_000;
    d
}

// ============ Happy path ============

#[test]
fn happy_path_verifies_and_pays_out() {
    let f = setup();
    f.submit(&det()).unwrap();
    assert_eq!(f.treasury.calls(), 1);
    assert_eq!(f.treasury.last_pid(), 1);
    assert_eq!(f.treasury.last_amt(), 4_500_000_000);
    assert_eq!(f.pm.claimed_calls(), 1);
    assert_eq!(f.pm.inc_calls(), 1);
    assert!(f.client.is_policy_paid(&1));
    let r = f.client.get_report(&1);
    assert_eq!(r.damage_percentage, 4_500);
    assert_eq!(r.payout_amount, 4_500_000_000);
    assert_eq!(r.weather_damage, 0);
    assert_eq!(r.satellite_damage, 0);
    assert_eq!(r.assessed_at, ASSESSED);
}

#[test]
fn view_digest_is_the_signed_digest() {
    let f = setup();
    let d = on(&f.env, &det());
    let want = drought_determination_digest(&f.env, &d, &f.pr_addr, &f.domain).to_bytes();
    assert_eq!(f.client.drought_determination_digest(&d), want);
}

// ============ 0-2: access, pause, signer, methodology ============

#[test]
fn unauthorized_relayer_rejected() {
    let f = setup();
    let d = on(&f.env, &det());
    let (sig, recid) = sign_with(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    let stranger = Address::generate(&f.env);
    assert_eq!(
        f.client
            .try_submit_drought_determination(&stranger, &d, &sig, &recid),
        Err(Ok(CommonError::Unauthorized.into()))
    );
}

#[test]
fn paused_blocks_submit() {
    let f = setup();
    f.client.pause(&f.admin);
    let d = on(&f.env, &det());
    let (sig, recid) = sign_with(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    assert_eq!(
        f.client
            .try_submit_drought_determination(&f.relayer, &d, &sig, &recid),
        Err(Ok(PausedError::ContractPaused.into()))
    );
}

#[test]
fn signer_not_configured_rejected() {
    let f = deploy(true, false);
    f.expect(&det(), PayoutError::SignerNotConfigured);
}

#[test]
fn methodology_disabled_rejected() {
    let f = deploy(false, true);
    f.expect(&det(), PayoutError::MethodologyDisabled);
    // Enabling it is all it takes; disabling it again stops it.
    f.client
        .set_methodology_enabled(&f.admin, &MethodologyKind::CropDroughtRdi10, &true);
    f.client
        .set_methodology_enabled(&f.admin, &MethodologyKind::CropDroughtRdi10, &false);
    f.expect(&det(), PayoutError::MethodologyDisabled);
}

// ============ 3: bounds and evidence ============

#[test]
fn damage_bp_above_maximum_rejected() {
    let f = setup();
    let mut d = det();
    d.damage_percent_bp = 10_100;
    d.drought_damage = 101;
    f.expect(&d, PayoutError::DamageExceedsMaximum);
}

#[test]
fn drought_score_above_100_rejected() {
    let f = setup();
    let mut d = det();
    d.drought_damage = 101;
    d.damage_percent_bp = 10_000;
    f.expect(&d, PayoutError::DroughtScoreOutOfRange);
}

#[test]
fn lgp_other_than_180_rejected() {
    let f = setup();
    let mut d = det();
    d.lgp_days = 150;
    f.expect(&d, PayoutError::DroughtEvidenceInvalid);
}

#[test]
fn zero_climatological_normal_rejected() {
    for w in [1, 2] {
        let f = setup();
        let mut d = det();
        if w == 1 {
            d.rain_ref_w1_e2 = 0;
        } else {
            d.rain_ref_w2_e2 = 0;
        }
        f.expect(&d, PayoutError::DroughtEvidenceInvalid);
    }
}

#[test]
fn params_hash_other_than_rdi_1_0_rejected() {
    let f = setup();
    let mut d = det();
    // The EVM fixture's arbitrary params hash.
    let mut h = DROUGHT_PARAMS_HASH_RDI_1_0;
    h[0] ^= 1;
    d.methodology_params_hash = BytesN::from_array(&f.env, &h);
    f.expect(&d, PayoutError::DroughtEvidenceInvalid);
}

#[test]
fn dry_spell_longer_than_w2_rejected() {
    let f = setup();
    let mut d = det();
    d.cdd_w2_days = 41;
    f.expect(&d, PayoutError::DroughtEvidenceInvalid);
    // 40 (every W2 day dry) is valid evidence.
    let f = setup();
    let mut d = det();
    d.cdd_w2_days = 40;
    f.submit(&d).unwrap();
}

#[test]
fn season_start_not_at_utc_midnight_rejected() {
    let f = setup();
    let mut d = det();
    d.season_start_epoch = PLANT + 3_600;
    f.expect(&d, PayoutError::DroughtEvidenceInvalid);
}

#[test]
fn rainfall_above_the_bound_rejected() {
    let f = setup();
    let mut d = det();
    d.rain_w1_e2 = 1_000_000_001;
    d.rain_ref_w1_e2 = 1_000_000_001;
    f.expect(&d, PayoutError::DroughtEvidenceInvalid);
}

// ============ 4-6: unit invariant, recompute, threshold ============

#[test]
fn bp_not_drought_damage_times_100_rejected() {
    let f = setup();
    let mut d = det();
    d.damage_percent_bp = 4_501;
    d.payout_amount = SUM_INSURED * 4_501 / 10_000;
    f.expect(&d, PayoutError::InvalidDroughtDamage);
}

#[test]
fn drought_damage_not_what_the_evidence_gives_rejected() {
    let f = setup();
    // The evidence gives 45; the signer claims 50.
    let mut d = det();
    d.drought_damage = 50;
    d.damage_percent_bp = 5_000;
    d.payout_amount = SUM_INSURED / 2;
    f.expect(&d, PayoutError::InvalidDroughtDamage);
    // The EVM fixture's rains (12500/20000, 3000/18000) give 100, not its 45.
    let f = setup();
    let mut d = det();
    d.rain_w1_e2 = 12_500;
    d.rain_w2_e2 = 3_000;
    f.expect(&d, PayoutError::InvalidDroughtDamage);
}

#[test]
fn below_threshold_rejected() {
    let f = setup();
    // W2 at 90% of normal: d2 = 10, g2 = 15 -> 15%, a valid but sub-threshold result.
    f.expect(&with_w2(16_200, 15), PayoutError::DamageBelowThreshold);
}

#[test]
fn threshold_is_inclusive() {
    let f = setup();
    // W2 at 80% of normal: d2 = 20, g2 = 30 -> exactly 3,000 bp.
    f.submit(&with_w2(14_400, 30)).unwrap();
    assert_eq!(f.treasury.last_amt(), SUM_INSURED * 3_000 / 10_000);
}

// ============ 7: policy state ============

#[test]
fn policy_missing_rejected() {
    let f = setup();
    let mut d = det();
    d.on_chain_policy_id = 2;
    f.expect(&d, PayoutError::PolicyDoesNotExist);
}

#[test]
fn policy_not_active_rejected() {
    for st in [
        PolicyStatus::Pending,
        PolicyStatus::Expired,
        PolicyStatus::Cancelled,
        PolicyStatus::Claimed,
        PolicyStatus::ClaimPending,
    ] {
        let f = setup();
        f.set_policy(|p| p.status = st);
        f.expect(&det(), PayoutError::PolicyNotActive);
    }
}

#[test]
fn policy_expired_rejected() {
    let f = setup();
    // Cover ended a second before now (the window still fits, so only expiry fires).
    f.set_policy(|p| p.end_date = NOW - 1);
    f.expect(&det(), PayoutError::PolicyExpired);
}

#[test]
fn policy_already_paid_rejected() {
    let f = setup();
    f.submit(&det()).unwrap();
    // A second, different, validly signed determination later in the window.
    f.env.ledger().set_timestamp(NOW + 120);
    let mut d = with_w2(0, 100);
    d.assessed_at = NOW + 100;
    assert_eq!(f.submit(&d), Err(PayoutError::PolicyAlreadyPaid));
    assert_eq!(f.treasury.calls(), 1);
}

// ============ 8: coverage ============

#[test]
fn only_drought_cover_settles() {
    for c in [
        CoverageType::Comprehensive,
        CoverageType::Flood,
        CoverageType::Both,
        CoverageType::ExcessRain,
    ] {
        let f = setup();
        f.set_policy(|p| p.coverage_type = c);
        f.expect(&det(), PayoutError::CoverageNotSettledByMethodology);
    }
    let f = setup();
    f.submit(&det()).unwrap();
}

// ============ 9-10: sum insured and location ============

#[test]
fn sum_insured_mismatch_rejected() {
    let f = setup();
    let mut d = det();
    d.sum_insured = SUM_INSURED + 1;
    d.payout_amount = (SUM_INSURED + 1) * 4_500 / 10_000;
    f.expect(&d, PayoutError::SumInsuredMismatch);
    // A 6dp amount (the EVM scale) is 10x too small.
    let f = setup();
    let mut d = det();
    d.sum_insured = SUM_INSURED / 10;
    d.payout_amount = SUM_INSURED / 10 * 4_500 / 10_000;
    f.expect(&d, PayoutError::SumInsuredMismatch);
}

#[test]
fn location_tolerance_is_one_micro_degree_per_axis() {
    // Plot with BOTH coordinates negative, to exercise the signed comparison.
    let lat: i64 = -3_217_611;
    let lon: i64 = -39_668_206;
    for (dlat, dlon, ok) in [
        (1i128, 0i128, true),
        (-1, 0, true),
        (0, 1, true),
        (0, -1, true),
        (1, -1, true),
        (2, 0, false),
        (-2, 0, false),
        (0, 2, false),
        (0, -2, false),
    ] {
        let f = setup();
        f.set_policy(|p| {
            p.lat_e6 = lat;
            p.lon_e6 = lon;
        });
        let mut d = det();
        d.latitude_e6 = lat as i128 + dlat;
        d.longitude_e6 = lon as i128 + dlon;
        if ok {
            f.submit(&d).unwrap();
        } else {
            f.expect(&d, PayoutError::PlotLocationMismatch);
        }
    }
}

// ============ 11: evidence window ============

#[test]
fn planting_before_the_cover_start_day_rejected() {
    let f = setup();
    let mut d = det();
    d.season_start_epoch = PLANT - DAY;
    f.expect(&d, PayoutError::EvidenceOutsideCoverWindow);
}

#[test]
fn planting_on_the_cover_start_day_accepted() {
    // Cover activated at 23:59:59 UTC on planting day: still the same UTC day.
    let f = setup();
    f.set_policy(|p| p.start_date = PLANT + DAY - 1);
    f.submit(&det()).unwrap();
    // A day later it is not.
    let f = setup();
    f.set_policy(|p| p.start_date = PLANT + DAY);
    f.expect(&det(), PayoutError::EvidenceOutsideCoverWindow);
}

#[test]
fn window_ending_after_cover_rejected_and_exact_end_accepted() {
    let window_end = PLANT + 110 * DAY;
    // One second short: rejected.
    let f = setup();
    f.set_policy(|p| p.end_date = window_end - 1);
    f.env.ledger().set_timestamp(window_end - 1);
    let mut d = det();
    d.assessed_at = window_end - 1;
    f.expect(&d, PayoutError::EvidenceOutsideCoverWindow);
    // Exactly at the window end, assessed then: accepted.
    let f = setup();
    f.set_policy(|p| p.end_date = window_end);
    f.env.ledger().set_timestamp(window_end);
    let mut d = det();
    d.assessed_at = window_end;
    f.submit(&d).unwrap();
}

#[test]
fn season_not_complete_rejected() {
    let f = setup();
    let t = PLANT + 110 * DAY - 1;
    f.env.ledger().set_timestamp(t);
    let mut d = det();
    d.assessed_at = t;
    f.expect(&d, PayoutError::SeasonNotComplete);
}

// ============ 12: payout ============

#[test]
fn payout_rounds_down_and_an_extra_base_unit_is_rejected() {
    // 1,000,000,001 * 3,300 / 10,000 = 330,000,000.33 -> 330,000,000.
    // (rdi-1.0 is whole percent, so 3,300 bp rather than 3,333.) W2 at 78% of normal:
    // d2 = 22, g2 = 33.
    let si: i128 = 1_000_000_001;
    let base = || {
        let mut d = with_w2(14_040, 33);
        d.sum_insured = si;
        d
    };
    let f = setup();
    f.set_policy(|p| p.sum_insured = si);
    let mut up = base();
    up.payout_amount = 330_000_001;
    f.expect(&up, PayoutError::InvalidPayoutCalculation);
    let mut d = base();
    d.payout_amount = 330_000_000;
    f.submit(&d).unwrap();
    assert_eq!(f.treasury.last_amt(), 330_000_000);
}

// ============ 13: freshness ============

#[test]
fn future_assessment_rejected() {
    let f = setup();
    let mut d = det();
    d.assessed_at = NOW + 1;
    f.expect(&d, PayoutError::ReportInFuture);
}

#[test]
fn stale_assessment_rejected_at_3601_and_accepted_at_3600() {
    let f = setup();
    f.env.ledger().set_timestamp(ASSESSED + 3_601);
    f.expect(&det(), PayoutError::ReportTooOld);
    let f = setup();
    f.env.ledger().set_timestamp(ASSESSED + 3_600);
    f.submit(&det()).unwrap();
}

// ============ 14: farmer claim limit ============

#[test]
fn farmer_over_claim_limit_rejected() {
    let f = setup();
    f.pm.set_can_claim(&false);
    f.expect(&det(), PayoutError::FarmerClaimLimitExceeded);
}

// ============ 15-17: digest binding, replay, signature ============

#[test]
fn wrong_signer_domain_or_contract_rejected() {
    let f = setup();
    let d = on(&f.env, &det());
    let attacker = sk_from(0x22);
    let (sig, recid) = sign_with(&f.env, &attacker, &f.pr_addr, &f.domain, &d);
    assert_eq!(f.raw(&d, &sig, recid), Err(PayoutError::InvalidSignature));
    let other_domain = BytesN::from_array(&f.env, &[0x01; 32]);
    let (sig, recid) = sign_with(&f.env, &f.sk, &f.pr_addr, &other_domain, &d);
    assert_eq!(f.raw(&d, &sig, recid), Err(PayoutError::InvalidSignature));
    let other_contract = Address::generate(&f.env);
    let (sig, recid) = sign_with(&f.env, &f.sk, &other_contract, &f.domain, &d);
    assert_eq!(f.raw(&d, &sig, recid), Err(PayoutError::InvalidSignature));
    // Any changed evidence field under the same signature: the digest differs.
    let (sig, recid) = sign_with(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    let mut moved = d.clone();
    moved.cdd_w2_days = 22;
    assert_eq!(
        f.raw(&moved, &sig, recid),
        Err(PayoutError::InvalidSignature)
    );
    assert_eq!(f.treasury.calls(), 0);
}

#[test]
fn high_s_and_bad_recovery_id_never_pay() {
    let f = setup();
    let d = on(&f.env, &det());
    let digest = drought_determination_digest(&f.env, &d, &f.pr_addr, &f.domain);
    let (sig, recid) = f.sk.sign_prehash_recoverable(&digest.to_array()).unwrap();
    // The same signature with s -> n - s (and the recovery id flipped).
    let (r, s) = sig.split_scalars();
    let neg_s: k256::Scalar = -*s.as_ref();
    let high = Signature::from_scalars(r.to_bytes(), neg_s.to_bytes()).unwrap();
    let mut arr = [0u8; 64];
    arr.copy_from_slice(&high.to_bytes());
    let flipped = (recid.to_byte() ^ 1) as u32;
    let res = f.client.try_submit_drought_determination(
        &f.relayer,
        &d,
        &BytesN::from_array(&f.env, &arr),
        &flipped,
    );
    assert!(res.is_err(), "a high-S signature must not verify");
    // Recovery id 2 is refused.
    let mut arr = [0u8; 64];
    arr.copy_from_slice(&sig.to_bytes());
    let res = f.client.try_submit_drought_determination(
        &f.relayer,
        &d,
        &BytesN::from_array(&f.env, &arr),
        &2u32,
    );
    assert!(res.is_err(), "recovery id 2 must not verify");
    assert_eq!(f.treasury.calls(), 0);
    assert!(!f.client.is_policy_paid(&1));
}

#[test]
fn replay_of_the_same_determination_rejected() {
    let f = setup();
    let d = on(&f.env, &det());
    let (sig, recid) = sign_with(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    assert_eq!(f.raw(&d, &sig, recid), Ok(()));
    assert_eq!(f.raw(&d, &sig, recid), Err(PayoutError::PolicyAlreadyPaid));
    assert_eq!(f.treasury.calls(), 1);
}

fn crop_det(env: &Env) -> CropDetermination {
    let _ = env;
    CropDetermination {
        on_chain_policy_id: 1,
        damage_percent_bp: 5_000,
        weather_damage: 50,
        satellite_damage: 50,
        payout_amount: SUM_INSURED / 2,
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

fn sign_crop(f: &F, d: &CropDetermination) -> (BytesN<64>, u32) {
    let digest = determination_digest(&f.env, d, &f.pr_addr, &f.domain);
    let (sig, recid) = f.sk.sign_prehash_recoverable(&digest.to_array()).unwrap();
    let mut arr = [0u8; 64];
    arr.copy_from_slice(&sig.to_bytes());
    (BytesN::from_array(&f.env, &arr), recid.to_byte() as u32)
}

#[test]
fn a_policy_settles_at_most_once_across_both_methodologies() {
    // Crop first, then drought.
    let f = setup();
    f.client
        .set_methodology_enabled(&f.admin, &MethodologyKind::CropDualIndex10, &true);
    f.set_policy(|p| p.coverage_type = CoverageType::Comprehensive);
    let c = crop_det(&f.env);
    let (sig, recid) = sign_crop(&f, &c);
    f.client.submit_determination(&f.relayer, &c, &sig, &recid);
    f.set_policy(|p| p.coverage_type = CoverageType::Drought);
    assert_eq!(f.submit(&det()), Err(PayoutError::PolicyAlreadyPaid));
    assert_eq!(f.treasury.calls(), 1);

    // Drought first, then crop.
    let f = setup();
    f.client
        .set_methodology_enabled(&f.admin, &MethodologyKind::CropDualIndex10, &true);
    f.submit(&det()).unwrap();
    f.set_policy(|p| p.coverage_type = CoverageType::Comprehensive);
    let c = crop_det(&f.env);
    let (sig, recid) = sign_crop(&f, &c);
    assert_eq!(
        f.client
            .try_submit_determination(&f.relayer, &c, &sig, &recid),
        Err(Ok(PayoutError::PolicyAlreadyPaid.into()))
    );
    assert_eq!(f.treasury.calls(), 1);
}

#[test]
fn a_crop_signature_does_not_verify_on_the_drought_path() {
    let f = setup();
    let d = on(&f.env, &det());
    // A valid CROP_DAMAGE signature from the same key, contract and domain.
    let (sig, recid) = sign_crop(&f, &crop_det(&f.env));
    assert_eq!(f.raw(&d, &sig, recid), Err(PayoutError::InvalidSignature));
}

// ============ Pending path ============

#[test]
fn short_reserve_records_then_settles_and_blocks_other_determinations() {
    let f = setup();
    f.treasury.set_short(&true);
    f.submit(&det()).unwrap();
    assert_eq!(f.pm.pending_calls(), 1);
    assert_eq!(f.pm.claimed_calls(), 0);
    assert!(f.client.is_claim_pending(&1));
    assert!(f.client.is_policy_paid(&1));

    // Another valid determination cannot settle it.
    f.env.ledger().set_timestamp(NOW + 120);
    let mut d2 = with_w2(0, 100);
    d2.assessed_at = NOW + 100;
    assert_eq!(f.submit(&d2), Err(PayoutError::PolicyAlreadyPaid));

    // Topped up long after the policy ended: settles in full, no re-verification.
    f.env.ledger().set_timestamp(END + 30 * DAY);
    f.treasury.set_short(&false);
    f.env.set_auths(&[]);
    f.client.settle_pending_claim(&1);
    assert_eq!(f.treasury.settled_calls(), 1);
    assert_eq!(f.pm.claimed_calls(), 1);
    assert!(!f.client.is_claim_pending(&1));
}

// ============ Archived-entry safety ============

/// An archived `PolicyPaid` never reads as "not paid". Its TTL is allowed to run out (ledger
/// sequence only; the clock stays inside the cover), the temporary replay marker is gone,
/// and a fresh, validly signed determination for the same policy is submitted. The host
/// either refuses the archived entry or restores it with its true value, so the call reverts
/// and nothing is paid.
#[test]
fn an_archived_policy_paid_flag_never_pays_out() {
    use microcrop_shared::ttl::DAY_LEDGERS;
    use soroban_sdk::testutils::storage::Persistent as _;

    let f = setup();
    f.submit(&det()).unwrap();
    let paid = crate::DataKey::PolicyPaid(1);
    let ttl = f
        .env
        .as_contract(&f.pr_addr, || f.env.storage().persistent().get_ttl(&paid));
    let seq = f.env.ledger().sequence();
    f.env.ledger().set_sequence_number(seq + ttl + DAY_LEDGERS);
    // The replay marker (temporary, 7 days) has expired too.
    let digest = drought_determination_digest(&f.env, &on(&f.env, &det()), &f.pr_addr, &f.domain);
    let consumed = crate::DataKey::ConsumedDetermination(digest.to_bytes());
    assert!(!f
        .env
        .as_contract(&f.pr_addr, || f.env.storage().temporary().has(&consumed)));

    // A different determination, signed now.
    f.env.ledger().set_timestamp(NOW + 120);
    let mut d = with_w2(0, 100);
    d.assessed_at = NOW + 100;
    let d = on(&f.env, &d);
    let (sig, recid) = sign_with(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    let res = f
        .client
        .try_submit_drought_determination(&f.relayer, &d, &sig, &recid);
    // The test host emulates protocol-23 automatic restoration: the archived entry comes back
    // with its true value, `true`, and the call reverts PolicyAlreadyPaid. On the network a
    // transaction that does not restore it fails on the archived read instead.
    assert_eq!(res, Err(Ok(PayoutError::PolicyAlreadyPaid.into())));
    assert_eq!(f.treasury.calls(), 1, "paid exactly once");
}

// ============ Rent cap on the drought path ============

#[test]
fn the_drought_path_never_extends_the_instance_above_three_days() {
    use microcrop_shared::ttl::{DAY_LEDGERS, KEEPER_TARGET_LEDGERS, USER_TARGET_LEDGERS};
    use soroban_sdk::testutils::storage::Instance as _;
    let f = setup();
    let inst = || {
        f.env
            .as_contract(&f.pr_addr, || f.env.storage().instance().get_ttl())
    };
    f.client.extend_contract_ttl();
    assert_eq!(inst(), KEEPER_TARGET_LEDGERS);
    f.treasury.set_short(&true);
    f.submit(&det()).unwrap();
    assert_eq!(inst(), KEEPER_TARGET_LEDGERS);
    // Below 3 days, a settlement attempt tops it up to 10 days only.
    let seq = f.env.ledger().sequence();
    f.env.ledger().set_sequence_number(seq + 28 * DAY_LEDGERS);
    f.treasury.set_short(&false);
    f.client.settle_pending_claim(&1);
    assert_eq!(inst(), USER_TARGET_LEDGERS);
}

// ============ Signatures, not just roles ============
// The rest of this suite runs under `mock_all_auths`, which proves the role checks but not
// that the caller's signature is demanded. These run with no auths at all.

/// `set_methodology_enabled` needs the admin's own signature, not just the Admin role.
#[test]
fn set_methodology_enabled_requires_admin_signature() {
    let f = deploy(false, true);
    f.env.set_auths(&[]);
    let r =
        f.client
            .try_set_methodology_enabled(&f.admin, &MethodologyKind::CropDroughtRdi10, &true);
    assert!(r.is_err(), "admin call without admin auth must fail: {r:?}");
    assert!(!f
        .client
        .is_methodology_enabled(&MethodologyKind::CropDroughtRdi10));
}

/// `submit_drought_determination` needs the relayer's signature even when the determination
/// itself is validly signed by the oracle key.
#[test]
fn submit_drought_determination_requires_relayer_signature() {
    let f = setup();
    let d = on(&f.env, &det());
    let (sig, recid) = sign_with(&f.env, &f.sk, &f.pr_addr, &f.domain, &d);
    f.env.set_auths(&[]);
    let r = f
        .client
        .try_submit_drought_determination(&f.relayer, &d, &sig, &recid);
    assert!(r.is_err(), "relayer without auth must fail: {r:?}");
    assert_eq!(f.treasury.calls(), 0);
}

/// The keeper and the digest view are permissionless by design.
#[test]
fn keeper_and_digest_view_need_no_auth() {
    let f = setup();
    f.env.set_auths(&[]);
    f.client.extend_contract_ttl();
    let d = on(&f.env, &det());
    let _ = f.client.drought_determination_digest(&d);
}
