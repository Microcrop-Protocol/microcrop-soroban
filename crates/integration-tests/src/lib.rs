#![cfg(test)]
//! # Cross-contract integration tests
//!
//! Every other test crate in this workspace exercises ONE contract with in-file `#[contract]`
//! mocks standing in for its siblings. This crate does the opposite: it registers all FOUR
//! **real** contracts — [`PolicyManager`], [`Treasury`], [`PayoutReceiver`], [`PolicyNft`] — on a
//! single shared [`Env`], wires them the way a real deployment would, and runs the entire money
//! flow end-to-end:
//!
//! ```text
//!   create_policy ─▶ activate_policy (mints the real NFT)
//!                 ─▶ mint USDC + receive_premium (real SAC token transfer, fee split)
//!                 ─▶ sign a DroughtDetermination with a real k256 key
//!                 ─▶ submit_drought_determination (real secp256k1_recover)
//!                 ─▶ Treasury.request_payout (real USDC leaves the Treasury)
//! ```
//!
//! The payout authority is a genuine secp256k1 signature: we sign the exact digest the
//! `PayoutReceiver` reconstructs on-chain (via [`microcrop_shared::encoding::determination_digest`],
//! bound to the PayoutReceiver's own address + the configured network domain) and let the contract
//! recover it with `env.crypto().secp256k1_recover`. No assertion is weakened and no contract is
//! stubbed.
//!
//! ## Cross-contract role wiring discovered from the sources (the whole point of this test)
//! | grant / setter                                   | on contract     | so that…                                          |
//! |--------------------------------------------------|-----------------|---------------------------------------------------|
//! | `grant_role(Backend, backend)`                   | PolicyManager   | backend may `create_policy` / `activate_policy`   |
//! | `grant_role(Oracle,  payout_receiver_addr)`      | PolicyManager   | PR (as itself) may `mark_as_claimed` + `increment_claim_count` |
//! | `set_policy_nft(nft_addr)`                        | PolicyManager   | activation can mint the certificate               |
//! | `grant_role(Minter, policy_manager_addr)`        | PolicyNft       | PM (as itself) may `mint_policy` on activation    |
//! | `set_policy_manager(policy_manager_addr)`        | PolicyNft       | PM may flip the cert active-flag on claim         |
//! | `set_policy_manager(policy_manager_addr)`        | Treasury        | Treasury can resolve a policy's backing org       |
//! | `set_payout_receiver(payout_receiver_addr)`      | Treasury        | `request_payout` restricts caller == PR           |
//! | `grant_role(Backend, backend)`                   | Treasury        | backend may `receive_premium`                     |
//! | `grant_role(Payout,  payout_receiver_addr)`      | Treasury        | PR (as itself) may `request_payout`               |
//! | `grant_role(Relayer, relayer)`                   | PayoutReceiver  | relayer may submit determinations (anti-spam)     |
//! | `set_authorized_signer(pubkey65)`                | PayoutReceiver  | the sole payout authority (the signature)         |
//! | `set_network_domain(domain)`                     | PayoutReceiver  | binds the signed digest to this network           |
//! | `set_methodology_enabled(CropDroughtRdi10)`       | PayoutReceiver  | drought determinations are accepted at all        |
//!
//! ### Payout recipient note
//! In this v3 port `Treasury::request_payout` transfers USDC to the **backend offramp wallet**
//! (M-Pesa cash-out), NOT directly to the farmer's on-chain address — the farmer is paid off-chain.
//! So the REAL on-chain balance movement asserted here is `Treasury -> backend_wallet` for exactly
//! `sum_insured * damage_bp / 10000`, while the farmer is verified as the on-chain NFT owner and the
//! subject of the claimed policy. (Documented rather than hacked around.)

extern crate std;

use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    token::{StellarAssetClient, TokenClient},
    Address, BytesN, Env, String,
};

use k256::ecdsa::SigningKey;

use microcrop_shared::{
    encoding::{determination_digest, drought_determination_digest, DROUGHT_PARAMS_HASH_RDI_1_0},
    errors::{PayoutError, PolicyError, TreasuryError},
    types::{
        CoverageType, CropDetermination, DroughtDetermination, MethodologyKind, PolicyStatus, Role,
    },
};

use microcrop_payout_receiver::{
    PausedError as PrPausedError, PayoutReceiver, PayoutReceiverClient,
};
use microcrop_policy_manager::{PolicyManager, PolicyManagerClient};
use microcrop_policy_nft::{PolicyNft, PolicyNftClient};
use microcrop_treasury::{PausedError as TreasuryPausedError, Treasury, TreasuryClient};

// ============ Fixed test parameters ============

/// Planting day, 2025-04-01 00:00 UTC (the drought evidence's `season_start_epoch`).
const PLANT: u64 = 1_743_465_600;
/// Sale and activation: 06:00 UTC on planting day (the cover must start by planting).
const NOW: u64 = PLANT + 6 * 3_600;
/// 1,000 USDC (7dp, Stellar SAC) — within PolicyManager's [MIN_SUM_INSURED, MAX_SUM_INSURED].
const SUM_INSURED: i128 = 10_000_000_000;
/// 50% drought damage => 5000 bp (W1 at half its normal, W2 at 75% of its normal).
const DAMAGE_BP: u32 = 5_000;
/// SUM_INSURED * 5000 / 10000.
const PAYOUT: i128 = 5_000_000_000;
/// 150 days of cover: the 110-day measurement window, the 7-day archive lag and a margin.
const DURATION_DAYS: u32 = 150;
/// The oracle assesses 117 days after planting; the relayer submits a minute later.
const ASSESSED: u64 = PLANT + 117 * 86_400;
/// The insured plot (Nairobi), degrees x 1e6 — what create_policy stores and the signer signs.
const PLOT_LAT_E6: i64 = -1_286_389;
const PLOT_LON_E6: i64 = 36_817_223;

// ============ secp256k1 signing helpers (as in payout-receiver/src/test.rs) ============

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

fn sign_digest(env: &Env, sk: &SigningKey, digest: [u8; 32]) -> (BytesN<64>, u32) {
    let (sig, recid) = sk.sign_prehash_recoverable(&digest).unwrap();
    let mut arr = [0u8; 64];
    arr.copy_from_slice(&sig.to_bytes());
    (BytesN::from_array(env, &arr), recid.to_byte() as u32)
}

/// Sign the drought digest the contract will reconstruct for `d`, bound to `pr_addr` +
/// `domain`.
fn sign(
    env: &Env,
    sk: &SigningKey,
    pr_addr: &Address,
    domain: &BytesN<32>,
    d: &DroughtDetermination,
) -> (BytesN<64>, u32) {
    let digest = drought_determination_digest(env, d, pr_addr, domain);
    sign_digest(env, sk, digest.to_array())
}

/// A fully consistent drought determination for `policy_id`: W1 10000/20000 (d1 50, g1 20),
/// W2 13500/18000 (d2 25, g2 37) -> 100 - (80 * 63) / 100 = 50% => 5000 bp, assessed at
/// `ASSESSED`. `payout` must equal `sum_insured * DAMAGE_BP / 10000`.
fn determination(
    env: &Env,
    policy_id: u64,
    sum_insured: i128,
    payout: i128,
) -> DroughtDetermination {
    DroughtDetermination {
        on_chain_policy_id: policy_id,
        damage_percent_bp: DAMAGE_BP,
        drought_damage: DAMAGE_BP / 100,
        payout_amount: payout,
        assessed_at: ASSESSED,
        latitude_e6: PLOT_LAT_E6 as i128,
        longitude_e6: PLOT_LON_E6 as i128,
        sum_insured,
        season_start_epoch: PLANT,
        lgp_days: 180,
        rain_w1_e2: 10_000,
        rain_ref_w1_e2: 20_000,
        rain_w2_e2: 13_500,
        rain_ref_w2_e2: 18_000,
        cdd_w2_days: 12,
        source_hash: BytesN::from_array(env, &[0x5c; 32]),
        methodology_params_hash: BytesN::from_array(env, &DROUGHT_PARAMS_HASH_RDI_1_0),
    }
}

// ============ The wired-up world (all four REAL contracts on one Env) ============

struct World<'a> {
    env: Env,
    // the deploying admin — holds Admin + Upgrader on all four contracts
    admin: Address,
    // clients
    pm: PolicyManagerClient<'a>,
    nft: PolicyNftClient<'a>,
    treasury: TreasuryClient<'a>,
    pr: PayoutReceiverClient<'a>,
    token: TokenClient<'a>,
    token_admin: StellarAssetClient<'a>,
    // addresses
    treasury_id: Address,
    pr_addr: Address,
    // actors
    backend: Address,
    backend_wallet: Address,
    farmer: Address,
    distributor: Address,
    org: Address,
    relayer: Address,
    // signing
    sk: SigningKey,
    domain: BytesN<32>,
    // the USDC Stellar Asset Contract (issuer flags for the freeze test)
    sac: soroban_sdk::testutils::StellarAssetContract,
}

/// Deploy + wire all four contracts exactly as a real deployment would. `mock_all_auths` covers
/// every `require_auth` (payer transfers + the contract-as-caller cross-calls) on the happy path.
fn deploy<'a>() -> World<'a> {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(NOW);

    let admin = Address::generate(&env);
    let backend = Address::generate(&env);
    let backend_wallet = Address::generate(&env);
    let farmer = Address::generate(&env);
    let distributor = Address::generate(&env);
    let org = Address::generate(&env);
    let relayer = Address::generate(&env);

    // USDC as a real Stellar Asset Contract.
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let usdc_addr = sac.address();
    let token = TokenClient::new(&env, &usdc_addr);
    let token_admin = StellarAssetClient::new(&env, &usdc_addr);

    // ---- register the four real contracts ----
    let nft_addr = env.register(
        PolicyNft,
        (
            admin.clone(),
            String::from_str(&env, "MicroCrop Policy"),
            String::from_str(&env, "MCP"),
        ),
    );
    let pm_addr = env.register(PolicyManager, (admin.clone(),));
    let treasury_id = env.register(
        Treasury,
        (usdc_addr.clone(), backend_wallet.clone(), admin.clone()),
    );
    let pr_addr = env.register(
        PayoutReceiver,
        (treasury_id.clone(), pm_addr.clone(), admin.clone()),
    );

    let nft = PolicyNftClient::new(&env, &nft_addr);
    let pm = PolicyManagerClient::new(&env, &pm_addr);
    let treasury = TreasuryClient::new(&env, &treasury_id);
    let pr = PayoutReceiverClient::new(&env, &pr_addr);

    // ---- wire cross-contract references ----
    pm.set_policy_nft(&admin, &nft_addr);
    nft.set_policy_manager(&admin, &pm_addr);
    treasury.set_policy_manager(&admin, &pm_addr);
    treasury.set_payout_receiver(&admin, &pr_addr);

    // ---- grant the roles the cross-calls require ----
    // backend: create/activate policies + receive premiums.
    pm.grant_role(&admin, &Role::Backend, &backend);
    treasury.grant_role(&admin, &Role::Backend, &backend);
    // PolicyManager (acting as itself) mints certificates.
    nft.grant_role(&admin, &Role::Minter, &pm_addr);
    // PayoutReceiver (acting as itself) marks claimed / increments count / requests payout.
    pm.grant_role(&admin, &Role::Oracle, &pr_addr);
    treasury.grant_role(&admin, &Role::Payout, &pr_addr);
    // relayer: anti-spam gate on submit_determination.
    pr.grant_role(&admin, &Role::Relayer, &relayer);

    // ---- configure the payout authority (the signature) ----
    let sk = sk_from(0x11);
    pr.set_authorized_signer(&admin, &pubkey65(&env, &sk));
    let domain = BytesN::from_array(&env, &[0xABu8; 32]);
    pr.set_network_domain(&admin, &domain);
    // Accept CROP_DROUGHT; CROP_DAMAGE stays disabled (retired).
    pr.set_methodology_enabled(&admin, &MethodologyKind::CropDroughtRdi10, &true);

    World {
        env,
        admin: admin.clone(),
        pm,
        nft,
        treasury,
        pr,
        token,
        token_admin,
        treasury_id,
        pr_addr,
        backend,
        backend_wallet,
        farmer,
        distributor,
        org,
        relayer,
        sk,
        domain,
        sac,
    }
}

impl<'a> World<'a> {
    /// create_policy (PENDING) then activate_policy (ACTIVE + real NFT mint). Returns policy_id.
    fn create_and_activate(&self, premium: i128) -> u64 {
        let policy_id = self.pm.create_policy(
            &self.backend,
            &self.farmer,
            &7u64, // plot_id
            &PLOT_LAT_E6,
            &PLOT_LON_E6,
            &SUM_INSURED,
            &premium,
            &DURATION_DAYS,
            &CoverageType::Drought,
            &self.org,
        );
        self.pm.activate_policy(
            &self.backend,
            &policy_id,
            &self.distributor,
            &String::from_str(&self.env, "Acme Distributor"),
            &String::from_str(&self.env, "Rift Valley"),
        );
        policy_id
    }

    /// Fund the backend payer and pay the premium into the Treasury (real SAC transfer).
    fn pay_premium(&self, policy_id: u64, premium: i128) {
        self.token_admin.mint(&self.backend, &premium);
        self.treasury
            .receive_premium(&self.backend, &policy_id, &premium);
    }

    /// Move the ledger to the relayer's submission time (a minute after the assessment) if it
    /// is earlier: a drought determination can only be assessed once the season is over.
    fn at_assessment(&self) {
        if self.env.ledger().timestamp() < ASSESSED + 60 {
            self.env.ledger().set_timestamp(ASSESSED + 60);
        }
    }

    /// Sign `d` with the authorized key and submit it at assessment time.
    fn submit(&self, d: &DroughtDetermination) {
        self.at_assessment();
        let (sig, recid) = sign(&self.env, &self.sk, &self.pr_addr, &self.domain, d);
        self.pr
            .submit_drought_determination(&self.relayer, d, &sig, &recid);
    }

    /// `try_` form of [`Self::submit`] with an explicit signature.
    fn try_submit(
        &self,
        d: &DroughtDetermination,
        sig: &BytesN<64>,
        recid: u32,
    ) -> Result<
        Result<(), soroban_sdk::ConversionError>,
        Result<soroban_sdk::Error, soroban_sdk::InvokeError>,
    > {
        self.at_assessment();
        self.pr
            .try_submit_drought_determination(&self.relayer, d, sig, &recid)
    }

    /// Solvency invariant that must hold after every Treasury state change.
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

// ============ 1. Full lifecycle: premium -> signed determination -> payout ============

#[test]
fn full_lifecycle_premium_to_payout() {
    let w = deploy();
    let premium: i128 = 10_000_000_000; // 1,000 USDC; 10% fee => 900 USDC net to org reserve.

    // --- create + activate; the REAL NFT is minted to the farmer ---
    let policy_id = w.create_and_activate(premium);
    assert_eq!(policy_id, 1);
    assert!(w.nft.policy_nft_exists(&policy_id));
    assert_eq!(w.nft.owner_of(&policy_id), w.farmer);
    assert_eq!(w.pm.get_policy(&policy_id).status, PolicyStatus::Active);
    // Activation booked the org's outstanding exposure.
    assert_eq!(w.pm.org_outstanding_sum_insured(&w.org), SUM_INSURED);

    // --- premium in: fee split, org reserve credited ---
    w.pay_premium(policy_id, premium);
    let platform_fee = premium / 10; // default global fee = 10%
    let net = premium - platform_fee; // 9_000_000_000
    assert_eq!(w.treasury.accumulated_fees(), platform_fee);
    assert_eq!(w.treasury.org_reserve(&w.org), net);
    assert_eq!(w.treasury.get_available_for_payouts(&w.org), net);
    assert_eq!(w.token.balance(&w.treasury_id), premium);
    w.assert_solvent();

    // --- after the season: sign the determination with the real k256 key and submit it ---
    let d = determination(&w.env, policy_id, SUM_INSURED, PAYOUT);
    let backend_wallet_before = w.token.balance(&w.backend_wallet);
    w.submit(&d);

    // --- REAL USDC moved: Treasury -> backend offramp wallet, exactly the derived payout ---
    assert_eq!(
        w.token.balance(&w.backend_wallet) - backend_wallet_before,
        PAYOUT
    );
    assert_eq!(w.token.balance(&w.treasury_id), premium - PAYOUT);
    // Org reserve decreased by exactly the payout.
    assert_eq!(w.treasury.org_reserve(&w.org), net - PAYOUT);
    // Replay + processing guards set across contracts.
    assert!(w.treasury.is_payout_processed(&policy_id));
    assert!(w.pr.is_policy_paid(&policy_id));
    // Policy transitioned ACTIVE -> CLAIMED, and the stored report matches.
    assert_eq!(w.pm.get_policy(&policy_id).status, PolicyStatus::Claimed);
    let report = w.pr.get_report(&policy_id);
    assert_eq!(report.payout_amount, PAYOUT);
    assert_eq!(report.damage_percentage, DAMAGE_BP);
    assert_eq!(report.weather_damage, 0);
    assert_eq!(report.satellite_damage, 0);
    // Farmer is still the on-chain certificate owner.
    assert_eq!(w.nft.owner_of(&policy_id), w.farmer);
    w.assert_solvent();
}

// ============ 2. Replay of an identical determination is rejected, no double payout ============

#[test]
fn determination_replay_rejected() {
    let w = deploy();
    let premium: i128 = 10_000_000_000;
    let policy_id = w.create_and_activate(premium);
    w.pay_premium(policy_id, premium);

    let d = determination(&w.env, policy_id, SUM_INSURED, PAYOUT);
    let (sig, recid) = sign(&w.env, &w.sk, &w.pr_addr, &w.domain, &d);

    // First submission pays out.
    w.try_submit(&d, &sig, recid).unwrap().unwrap();
    let backend_wallet_after_first = w.token.balance(&w.backend_wallet);
    let reserve_after_first = w.treasury.org_reserve(&w.org);
    assert_eq!(backend_wallet_after_first, PAYOUT);

    // Second (identical) submission must be rejected. NOTE an emergent property only visible with
    // the REAL cross-contract wiring: the first payout drove PolicyManager ACTIVE -> CLAIMED, so on
    // replay the policy-status guard (`PolicyNotActive`, #308) fires *before* PayoutReceiver's own
    // persistent `PolicyAlreadyPaid` guard (#310) would. (The isolated unit test — whose mock keeps
    // the policy Active — sees #310 instead.) Either way the replay is blocked and no second payout
    // occurs; that difference is exactly what an integration test exists to surface.
    let res = w.try_submit(&d, &sig, recid);
    assert_eq!(res, Err(Ok(PayoutError::PolicyNotActive.into())));

    // No second payout: balances + reserve unchanged, Treasury still solvent.
    assert_eq!(
        w.token.balance(&w.backend_wallet),
        backend_wallet_after_first
    );
    assert_eq!(w.treasury.org_reserve(&w.org), reserve_after_first);
    assert_eq!(w.token.balance(&w.treasury_id), premium - PAYOUT);
    w.assert_solvent();
}

// ============ 3. A short org reserve records the claim; it settles in full later ============
// (audit findings 9 and 11 — previously the determination reverted with no record, and the
// claim was lost once the signed report aged out or the policy reached end_date.)

/// Sell (create + activate) a policy on planting day and pay a 100 USDC premium (90 USDC
/// net reserve). Returns the policy id.
fn sell_short(w: &World) -> u64 {
    let premium: i128 = 1_000_000_000; // 100 USDC; net after 10% fee = 90 USDC.
    let policy_id = w.create_and_activate(premium);
    w.pay_premium(policy_id, premium);
    assert!(premium - premium / 10 < PAYOUT);
    policy_id
}

/// [`sell_short`], then after the season submit a valid determination for a 500 USDC payout
/// the reserve cannot cover. Returns the policy id.
fn record_short_claim(w: &World) -> u64 {
    let policy_id = sell_short(w);
    w.submit(&determination(&w.env, policy_id, SUM_INSURED, PAYOUT));
    policy_id
}

#[test]
fn short_reserve_records_pending_claim_then_settles_in_full() {
    let w = deploy();
    let policy_id = record_short_claim(&w);
    let net: i128 = 900_000_000;

    // Nothing paid yet; the full amount is recorded and the policy is CLAIM_PENDING.
    assert_eq!(w.token.balance(&w.backend_wallet), 0);
    assert_eq!(w.treasury.org_reserve(&w.org), net);
    assert!(w.pr.is_claim_pending(&policy_id));
    assert!(w.pr.is_policy_paid(&policy_id));
    assert!(!w.treasury.is_payout_processed(&policy_id));
    assert_eq!(
        w.treasury.get_pending_payout(&policy_id).unwrap().amount,
        PAYOUT
    );
    assert_eq!(w.treasury.org_pending_payouts(&w.org), PAYOUT);
    assert_eq!(
        w.pm.get_policy(&policy_id).status,
        PolicyStatus::ClaimPending
    );
    assert_eq!(w.pm.get_farmer_claim_count(&w.farmer), 1);
    // Counted once each: the claim holds the farmer's slot (as a claim in the window) and the
    // org's reserve (as the full pending payout), so the active slot and the 30% exposure on
    // the policy are released when it goes CLAIM_PENDING.
    assert_eq!(w.pm.get_farmer_active_count(&w.farmer), 0);
    assert_eq!(w.pm.org_outstanding_sum_insured(&w.org), 0);
    assert!(w.nft.get_certificate(&policy_id).is_active);

    // Reserve lock: the requirement is the whole pending payout, and nothing on top of it.
    assert_eq!(w.treasury.reserve_required(&w.org), PAYOUT);
    let to = Address::generate(&w.env);
    assert!(w
        .treasury
        .try_withdraw_org_surplus(&w.org, &1, &to)
        .is_err());

    // Settling while still short reverts InsufficientOrgReserve and keeps the claim.
    assert_eq!(
        w.pr.try_settle_pending_claim(&policy_id),
        Err(Ok(TreasuryError::InsufficientOrgReserve.into()))
    );
    assert!(w.pr.is_claim_pending(&policy_id));

    // Time passes: far beyond MAX_REPORT_AGE and the policy's end_date. The claim does not
    // lapse — the policy cannot be expired or cancelled while the claim is pending.
    let end = w.pm.get_policy(&policy_id).end_date;
    w.env.ledger().set_timestamp(end + 30 * 86_400);
    assert!(w.pm.try_expire_policy(&policy_id).is_err());
    assert!(w.pm.try_cancel_policy(&w.admin, &policy_id).is_err());

    // The org tops up exactly the claim. Before settlement it still cannot withdraw the
    // claim money.
    let top_up = PAYOUT;
    w.token_admin.mint(&w.org, &top_up);
    w.treasury.deposit_reserve(&w.org, &w.org, &top_up);
    assert!(w
        .treasury
        .try_withdraw_org_surplus(&w.org, &(net + 1), &to)
        .is_err());

    // Anyone settles — no authorization at all.
    w.env.set_auths(&[]);
    w.pr.settle_pending_claim(&policy_id);

    // Exactly the signed payout moved, once.
    assert_eq!(w.token.balance(&w.backend_wallet), PAYOUT);
    assert_eq!(w.treasury.org_reserve(&w.org), net + top_up - PAYOUT);
    assert!(w.treasury.is_payout_processed(&policy_id));
    assert!(w.treasury.get_pending_payout(&policy_id).is_none());
    assert_eq!(w.treasury.org_pending_payouts(&w.org), 0);
    assert!(!w.pr.is_claim_pending(&policy_id));
    // Marked claimed exactly as the normal path: status, slot, exposure, certificate.
    assert_eq!(w.pm.get_policy(&policy_id).status, PolicyStatus::Claimed);
    assert_eq!(w.pm.get_farmer_active_count(&w.farmer), 0);
    assert_eq!(w.pm.org_outstanding_sum_insured(&w.org), 0);
    assert!(!w.nft.get_certificate(&policy_id).is_active);
    // Counted once, at approval.
    assert_eq!(w.pm.get_farmer_claim_count(&w.farmer), 1);
    w.assert_solvent();

    // No double pay: a second settle reverts, and the original determination cannot replay.
    let res = w.pr.try_settle_pending_claim(&policy_id);
    assert_eq!(res, Err(Ok(PayoutError::NoPendingClaim.into())));
    assert_eq!(w.token.balance(&w.backend_wallet), PAYOUT);
}

#[test]
fn pending_claim_blocks_replay_and_other_determinations() {
    let w = deploy();
    let policy_id = record_short_claim(&w);

    // The same determination again: rejected, and no second pending record.
    let d = determination(&w.env, policy_id, SUM_INSURED, PAYOUT);
    let (sig, recid) = sign(&w.env, &w.sk, &w.pr_addr, &w.domain, &d);
    assert!(w.try_submit(&d, &sig, recid).is_err());

    // A different, smaller determination (signed later) cannot settle the policy either.
    // W1 at normal, W2 at 80% of normal: d2 = 20, g2 = 30 -> 30%.
    w.env.ledger().set_timestamp(ASSESSED + 120);
    let mut d2 = determination(&w.env, policy_id, SUM_INSURED, SUM_INSURED * 3_000 / 10_000);
    d2.rain_w1_e2 = 20_000;
    d2.rain_w2_e2 = 14_400;
    d2.drought_damage = 30;
    d2.damage_percent_bp = 3_000;
    d2.assessed_at = ASSESSED + 100;
    let (sig2, recid2) = sign(&w.env, &w.sk, &w.pr_addr, &w.domain, &d2);
    let res = w.try_submit(&d2, &sig2, recid2);
    assert_eq!(res, Err(Ok(PayoutError::PolicyNotActive.into())));

    // The recorded amount is still the original, full one.
    assert_eq!(
        w.treasury.get_pending_payout(&policy_id).unwrap().amount,
        PAYOUT
    );
    assert_eq!(w.token.balance(&w.backend_wallet), 0);
}

#[test]
fn new_claim_queues_behind_an_older_pending_claim() {
    let w = deploy();
    // Two policies of the same farmer, both sold on planting day.
    let first = sell_short(&w);
    let premium: i128 = 1_000_000_000;
    let second = w.create_and_activate(premium);
    w.pay_premium(second, premium);
    // After the season the first claim finds 180 USDC of reserve: 500 USDC pending.
    w.submit(&determination(&w.env, first, SUM_INSURED, PAYOUT));
    // Reserve 600 USDC: the second 500 USDC claim fits the reserve, but only 100 USDC of it
    // is not already owed to the first claim.
    let top_up: i128 = 4_200_000_000;
    w.token_admin.mint(&w.org, &top_up);
    w.treasury.deposit_reserve(&w.org, &w.org, &top_up);
    assert_eq!(w.treasury.org_reserve(&w.org), 6_000_000_000);

    w.submit(&determination(&w.env, second, SUM_INSURED, PAYOUT));

    // Queued, not paid ahead of the first claim.
    assert_eq!(w.token.balance(&w.backend_wallet), 0);
    assert_eq!(w.pm.get_policy(&second).status, PolicyStatus::ClaimPending);
    assert_eq!(w.treasury.org_pending_payouts(&w.org), 2 * PAYOUT);

    // The first claim settles from the reserve it was waiting on.
    w.env.set_auths(&[]);
    w.pr.settle_pending_claim(&first);
    assert_eq!(w.token.balance(&w.backend_wallet), PAYOUT);
    assert_eq!(w.pm.get_policy(&first).status, PolicyStatus::Claimed);
    assert!(w.pr.is_claim_pending(&second));
    w.assert_solvent();
}

#[test]
fn pending_claim_settlement_respects_both_pauses() {
    let w = deploy();
    let policy_id = record_short_claim(&w);
    w.token_admin.mint(&w.org, &PAYOUT);
    w.treasury.deposit_reserve(&w.org, &w.org, &PAYOUT);

    // PayoutReceiver paused: ContractPaused (318).
    w.pr.pause(&w.admin);
    assert_eq!(
        w.pr.try_settle_pending_claim(&policy_id),
        Err(Ok(PrPausedError::ContractPaused.into()))
    );
    w.pr.unpause(&w.admin);

    // Treasury paused: its Paused (215) propagates; the claim stays recorded.
    w.treasury.pause(&w.admin);
    assert_eq!(
        w.pr.try_settle_pending_claim(&policy_id),
        Err(Ok(TreasuryPausedError::Paused.into()))
    );
    assert!(w.pr.is_claim_pending(&policy_id));
    assert_eq!(w.token.balance(&w.backend_wallet), 0);

    // Unpaused — even long after the policy's end_date — it settles in full.
    w.treasury.unpause(&w.admin);
    let end = w.pm.get_policy(&policy_id).end_date;
    w.env.ledger().set_timestamp(end + 1);
    w.pr.settle_pending_claim(&policy_id);
    assert_eq!(w.token.balance(&w.backend_wallet), PAYOUT);
    assert_eq!(w.pm.get_policy(&policy_id).status, PolicyStatus::Claimed);
    w.assert_solvent();
}

// ============ Bonus: a wrong-signer determination is rejected end-to-end ============

#[test]
fn wrong_signer_determination_rejected() {
    let w = deploy();
    let premium: i128 = 10_000_000_000;
    let policy_id = w.create_and_activate(premium);
    w.pay_premium(policy_id, premium);

    // Correct payload, but signed by a key that is NOT the configured authorized signer.
    let attacker = sk_from(0x22);
    let d = determination(&w.env, policy_id, SUM_INSURED, PAYOUT);
    let (sig, recid) = sign(&w.env, &attacker, &w.pr_addr, &w.domain, &d);

    let res = w.try_submit(&d, &sig, recid);
    assert_eq!(res, Err(Ok(PayoutError::InvalidSignature.into())));

    // Nothing moved.
    assert_eq!(w.token.balance(&w.backend_wallet), 0);
    assert_eq!(w.treasury.org_reserve(&w.org), premium - premium / 10);
    assert!(!w.pr.is_policy_paid(&policy_id));
    assert_eq!(w.pm.get_policy(&policy_id).status, PolicyStatus::Active);
}

// ============ 5. has_role reports the wiring a deployment depends on ============
//
// These are the grants that produce a SILENTLY BROKEN deployment when missed: none of them
// fails at deploy time, and three of them go to CONTRACTS rather than people. Before
// has_role() existed there was no way to ask a deployed contract whether they had been made,
// so scripts/verify-deployment.sh had to read raw storage and match on addresses — indirect,
// and able to false-pass. This pins the exact queries that verifier now relies on.
#[test]
fn has_role_reports_the_money_path_grants() {
    let w = deploy();

    // Granted to CONTRACTS. Missing #1 -> activation cannot mint a certificate; #2 ->
    // determinations cannot mark a policy claimed; #3 -> NO PAYOUT CAN EVER BE DRAWN.
    assert!(
        w.nft.has_role(&Role::Minter, &w.pm.address),
        "PolicyManager must hold Minter on PolicyNft"
    );
    assert!(
        w.pm.has_role(&Role::Oracle, &w.pr_addr),
        "PayoutReceiver must hold Oracle on PolicyManager"
    );
    assert!(
        w.treasury.has_role(&Role::Payout, &w.pr_addr),
        "PayoutReceiver must hold Payout on Treasury"
    );

    // Granted to accounts.
    assert!(w.pm.has_role(&Role::Backend, &w.backend));
    assert!(w.treasury.has_role(&Role::Backend, &w.backend));
    assert!(w.pr.has_role(&Role::Relayer, &w.relayer));
}

#[test]
fn has_role_is_false_for_non_holders_and_is_per_contract() {
    let w = deploy();

    // An address holding a role in one contract does not hold it in another. The registry is
    // namespaced per contract, and a verifier that assumed otherwise would false-pass.
    assert!(w.pm.has_role(&Role::Backend, &w.backend));
    assert!(
        !w.nft.has_role(&Role::Backend, &w.backend),
        "Backend on PolicyManager must not leak into PolicyNft"
    );

    // Holding one role does not imply another.
    assert!(!w.pm.has_role(&Role::Oracle, &w.backend));
    assert!(!w.treasury.has_role(&Role::Payout, &w.backend));

    // A never-granted address holds nothing.
    assert!(!w.treasury.has_role(&Role::Payout, &w.farmer));
    assert!(!w.pr.has_role(&Role::Relayer, &w.farmer));
}

#[test]
fn revoking_a_role_is_visible_through_has_role() {
    let w = deploy();
    let admin = w.pm.has_role(&Role::Backend, &w.backend);
    assert!(admin);

    // Revocation must be observable, or an operator cannot confirm an emergency lockout
    // actually took effect.
    w.treasury.revoke_role(&w.admin, &Role::Payout, &w.pr_addr);
    assert!(
        !w.treasury.has_role(&Role::Payout, &w.pr_addr),
        "revoke must be visible via has_role"
    );
}

// ============ Finding 4: a PENDING policy's premium backs its own exposure ============

#[test]
fn pending_policy_premium_is_reserve_not_surplus() {
    let w = deploy();
    let premium: i128 = 10_000_000_000; // 1,000 USDC; 900 USDC net to the org reserve.
    let policy_id = w.pm.create_policy(
        &w.backend,
        &w.farmer,
        &7u64,
        &PLOT_LAT_E6,
        &PLOT_LON_E6,
        &SUM_INSURED,
        &premium,
        &DURATION_DAYS,
        &CoverageType::Drought,
        &w.org,
    );
    assert_eq!(w.pm.get_policy(&policy_id).status, PolicyStatus::Pending);
    // Exposure is counted while PENDING.
    assert_eq!(w.pm.org_outstanding_sum_insured(&w.org), SUM_INSURED);

    w.pay_premium(policy_id, premium);
    let net = premium - premium / 10;
    let required = w.treasury.reserve_required(&w.org);
    assert_eq!(required, SUM_INSURED * 3_000 / 10_000);

    // The org cannot take the reserve below what the PENDING policy needs.
    let to = Address::generate(&w.env);
    assert!(w
        .treasury
        .try_withdraw_org_surplus(&w.org, &(net - required + 1), &to)
        .is_err());
    w.treasury
        .withdraw_org_surplus(&w.org, &(net - required), &to);
    assert_eq!(w.treasury.org_reserve(&w.org), required);

    // Cancelling the PENDING policy releases the exposure, and the requirement with it.
    w.pm.cancel_policy(&w.backend, &policy_id);
    assert_eq!(w.pm.org_outstanding_sum_insured(&w.org), 0);
    assert_eq!(w.treasury.reserve_required(&w.org), 0);
    w.assert_solvent();
}

// ============ Pending claims settle first-in-first-out per org ============

#[test]
fn pending_claims_of_one_org_settle_in_arrival_order() {
    let w = deploy();
    // Both sold on planting day; both claims recorded after the season.
    let first = sell_short(&w);
    let second = sell_short(&w);
    w.submit(&determination(&w.env, first, SUM_INSURED, PAYOUT)); // 500 pending, reserve 180
    w.submit(&determination(&w.env, second, SUM_INSURED, PAYOUT)); // 500 pending
    assert_eq!(w.treasury.pending_payout_head(&w.org), Some(first));
    assert_eq!(w.treasury.pending_payout_count(&w.org), 2);

    // Fund exactly one claim. Anyone may settle, but only the head: the second claim
    // cannot take the money the first has been waiting for.
    w.token_admin.mint(&w.org, &(PAYOUT - 1_800_000_000));
    w.treasury
        .deposit_reserve(&w.org, &w.org, &(PAYOUT - 1_800_000_000));
    w.env.set_auths(&[]);
    assert_eq!(
        w.pr.try_settle_pending_claim(&second),
        Err(Ok(TreasuryError::NotPendingQueueHead.into()))
    );
    assert!(w.pr.is_claim_pending(&second));
    w.pr.settle_pending_claim(&first);
    assert_eq!(w.token.balance(&w.backend_wallet), PAYOUT);
    assert_eq!(w.treasury.pending_payout_head(&w.org), Some(second));

    // The second settles once funded.
    w.env.mock_all_auths();
    w.token_admin.mint(&w.org, &PAYOUT);
    w.treasury.deposit_reserve(&w.org, &w.org, &PAYOUT);
    w.env.set_auths(&[]);
    w.pr.settle_pending_claim(&second);
    assert_eq!(w.token.balance(&w.backend_wallet), 2 * PAYOUT);
    assert_eq!(w.pm.get_policy(&second).status, PolicyStatus::Claimed);
    assert_eq!(w.treasury.pending_payout_head(&w.org), None);
    w.assert_solvent();
}

// ============ An issuer freeze on the backend wallet does not lose the claim ============

#[test]
fn frozen_backend_wallet_queues_the_claim_until_the_wallet_is_rotated() {
    let w = deploy();
    let premium: i128 = 10_000_000_000; // reserve 900 USDC covers the 500 USDC claim.
    let policy_id = w.create_and_activate(premium);
    w.pay_premium(policy_id, premium);

    // Circle's issuer has AUTH_REVOCABLE: it deauthorizes the backend wallet.
    w.sac
        .issuer()
        .set_flag(soroban_sdk::testutils::IssuerFlags::RevocableFlag);
    w.token_admin.set_authorized(&w.backend_wallet, &false);

    // The determination is accepted, not reverted: consumed, counted, and queued.
    w.submit(&determination(&w.env, policy_id, SUM_INSURED, PAYOUT));
    assert!(w.pr.is_policy_paid(&policy_id));
    assert!(w.pr.is_claim_pending(&policy_id));
    assert_eq!(
        w.pm.get_policy(&policy_id).status,
        PolicyStatus::ClaimPending
    );
    assert_eq!(w.token.balance(&w.backend_wallet), 0);
    assert_eq!(w.treasury.org_pending_payouts(&w.org), PAYOUT);

    // Settling into the frozen wallet fails and keeps the claim.
    assert_eq!(
        w.pr.try_settle_pending_claim(&policy_id),
        Err(Ok(TreasuryError::PayoutTransferFailed.into()))
    );
    assert!(w.pr.is_claim_pending(&policy_id));

    // The admin rotates the wallet; anyone settles; the farmer's claim is paid in full,
    // long after MAX_REPORT_AGE.
    let new_wallet = Address::generate(&w.env);
    w.treasury.set_backend_wallet(&w.admin, &new_wallet);
    w.env.ledger().set_timestamp(ASSESSED + 7 * 86_400);
    w.env.set_auths(&[]);
    w.pr.settle_pending_claim(&policy_id);
    assert_eq!(w.token.balance(&new_wallet), PAYOUT);
    assert_eq!(w.pm.get_policy(&policy_id).status, PolicyStatus::Claimed);
    w.assert_solvent();
}

// ============ Coverage type and plot binding, end to end ============

#[test]
fn only_drought_cover_can_be_sold() {
    // The one signable methodology (crop-drought-rdi-1.0) settles drought cover only, so
    // every other cover is refused at sale: no policy, no exposure, no farmer slot, no
    // premium.
    let w = deploy();
    let premium: i128 = 10_000_000_000;
    for coverage in [
        CoverageType::Comprehensive,
        CoverageType::Flood,
        CoverageType::Both,
        CoverageType::ExcessRain,
    ] {
        assert_eq!(
            w.pm.try_create_policy(
                &w.backend,
                &w.farmer,
                &7u64,
                &PLOT_LAT_E6,
                &PLOT_LON_E6,
                &SUM_INSURED,
                &premium,
                &DURATION_DAYS,
                &coverage,
                &w.org,
            ),
            Err(Ok(PolicyError::CoverageNotOffered.into()))
        );
    }
    assert_eq!(w.pm.get_total_policies(), 0);
    assert_eq!(w.pm.org_outstanding_sum_insured(&w.org), 0);
    assert_eq!(w.pm.get_farmer_policy_count(&w.farmer), 0);
}

#[test]
fn evidence_from_another_plot_does_not_settle() {
    let w = deploy();
    let premium: i128 = 10_000_000_000;
    let policy_id = w.create_and_activate(premium);
    w.pay_premium(policy_id, premium);

    let mut d = determination(&w.env, policy_id, SUM_INSURED, PAYOUT);
    d.latitude_e6 = -1_300_000; // ~1.5 km south
    let (sig, recid) = sign(&w.env, &w.sk, &w.pr_addr, &w.domain, &d);
    assert_eq!(
        w.try_submit(&d, &sig, recid),
        Err(Ok(PayoutError::PlotLocationMismatch.into()))
    );
    assert_eq!(w.pm.get_policy(&policy_id).status, PolicyStatus::Active);
}

// ============ Rent cap: keepers reach every contract ============

const DAY: u32 = 17_280;

/// Advance ledger time and sequence together by `days`, running the contract keeper first
/// (as the backend's daily job would) so no instance lapses.
fn advance_with_keeper(w: &World, days: u32) {
    w.pr.extend_contract_ttl();
    let seq = w.env.ledger().sequence();
    w.env.ledger().set_sequence_number(seq + days * DAY);
    let ts = w.env.ledger().timestamp();
    w.env.ledger().set_timestamp(ts + days as u64 * 86_400);
}

#[test]
fn extend_contract_ttl_reaches_every_contract() {
    use microcrop_shared::ttl::{KEEPER_TARGET_LEDGERS, USER_TARGET_LEDGERS};
    use soroban_sdk::testutils::storage::Instance as _;

    let w = deploy();
    let premium: i128 = 10_000_000_000;
    let policy_id = w.create_and_activate(premium);
    w.pay_premium(policy_id, premium);
    let inst = |a: &Address| {
        w.env
            .as_contract(a, || w.env.storage().instance().get_ttl())
    };
    let all = [
        w.pm.address.clone(),
        w.nft.address.clone(),
        w.treasury_id.clone(),
        w.pr_addr.clone(),
    ];
    // The whole money path so far extended code and instances to 10 days at most. (Admin
    // and config calls extend nothing, so PayoutReceiver is still at its deploy TTL: run the
    // keeper right after a deploy.)
    for a in all[..3].iter() {
        assert_eq!(inst(a), USER_TARGET_LEDGERS);
    }
    assert!(inst(&w.pr_addr) < USER_TARGET_LEDGERS);
    // One permissionless call extends all four to 30 days.
    w.env.set_auths(&[]);
    w.pr.extend_contract_ttl();
    for a in all.iter() {
        assert_eq!(inst(a), KEEPER_TARGET_LEDGERS);
    }
}

#[test]
fn extend_policy_ttl_reaches_every_contract() {
    use microcrop_shared::ttl::{NETWORK_MAX_ENTRY_TTL_LEDGERS, PENDING_TARGET_LEDGERS};
    use soroban_sdk::testutils::storage::Persistent as _;
    use soroban_sdk::IntoVal;

    let w = deploy();
    w.env
        .ledger()
        .set_max_entry_ttl(NETWORK_MAX_ENTRY_TTL_LEDGERS);
    let policy_id = record_short_claim(&w);

    let ttl_in = |contract: &Address, k: soroban_sdk::Val| {
        w.env
            .as_contract(contract, || w.env.storage().persistent().get_ttl(&k))
    };
    let pm_policy = || microcrop_policy_manager::DataKey::Policy(policy_id).into_val(&w.env);
    let tr_premium = || microcrop_treasury::DataKey::PremiumReceived(policy_id).into_val(&w.env);
    let tr_pending = || microcrop_treasury::DataKey::PendingPayout(policy_id).into_val(&w.env);
    let pr_paid = || microcrop_payout_receiver::DataKey::PolicyPaid(policy_id).into_val(&w.env);
    let pr_pending =
        || microcrop_payout_receiver::DataKey::ClaimPending(policy_id).into_val(&w.env);
    let nft_cert = || microcrop_policy_nft::DataKey::Certificate(policy_id).into_val(&w.env);

    // Per-policy entries live until end + 30 days, counted from when they were written:
    // the premium flag, certificate and policy at sale (NOW), PolicyPaid at the claim.
    let until = w.pm.get_policy(&policy_id).end_date + 30 * 86_400;
    let need_at = |t: u64| ((until - t).div_ceil(5)) as u32;
    assert_eq!(ttl_in(&w.pr_addr, pr_paid()), need_at(ASSESSED + 60));
    assert_eq!(ttl_in(&w.treasury_id, tr_premium()), need_at(NOW));
    assert_eq!(ttl_in(&w.nft.address, nft_cert()), need_at(NOW));
    // The policy entry already lived until end + 30 days; going CLAIM_PENDING never
    // shortens it. Pending-claim entries: 60 days.
    assert_eq!(ttl_in(&w.pm.address, pm_policy()), need_at(NOW));
    let before = [
        ttl_in(&w.pm.address, pm_policy()),
        ttl_in(&w.pr_addr, pr_paid()),
        ttl_in(&w.treasury_id, tr_premium()),
        ttl_in(&w.nft.address, nft_cert()),
    ];
    assert_eq!(ttl_in(&w.treasury_id, tr_pending()), PENDING_TARGET_LEDGERS);
    assert_eq!(ttl_in(&w.pr_addr, pr_pending()), PENDING_TARGET_LEDGERS);

    // 40 days with only the contract keeper running: the pending entries are under 30 days.
    advance_with_keeper(&w, 40);
    assert_eq!(ttl_in(&w.pr_addr, pr_pending()), 20 * DAY);

    // One permissionless call.
    w.env.set_auths(&[]);
    w.pr.extend_policy_ttl(&policy_id);
    for (c, k) in [(&w.treasury_id, tr_pending()), (&w.pr_addr, pr_pending())] {
        assert_eq!(ttl_in(c, k), PENDING_TARGET_LEDGERS);
    }
    // The per-policy entries already live until end + 30 days: left as they are.
    for (i, (c, k)) in [
        (&w.pm.address, pm_policy()),
        (&w.pr_addr, pr_paid()),
        (&w.treasury_id, tr_premium()),
        (&w.nft.address, nft_cert()),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(ttl_in(c, k), before[i] - 40 * DAY);
    }
}

#[test]
fn extend_policy_ttl_keeps_org_and_farmer_entries_alive() {
    use microcrop_shared::ttl::{
        NETWORK_MAX_ENTRY_TTL_LEDGERS, PENDING_TARGET_LEDGERS, SHARED_FLOOR_LEDGERS,
    };
    use soroban_sdk::testutils::storage::Persistent as _;
    use soroban_sdk::IntoVal;

    let w = deploy();
    w.env
        .ledger()
        .set_max_entry_ttl(NETWORK_MAX_ENTRY_TTL_LEDGERS);
    // Admin-set org terms (the same values as the defaults, so the claim math is unchanged).
    w.treasury.set_org_fee_bps(&w.admin, &w.org, &1_000u32);
    w.treasury
        .set_org_reserve_ratio_bps(&w.admin, &w.org, &3_000u32);
    let policy_id = record_short_claim(&w);

    let ttl_in = |contract: &Address, k: soroban_sdk::Val| {
        w.env
            .as_contract(contract, || w.env.storage().persistent().get_ttl(&k))
    };
    use microcrop_policy_manager::DataKey as Pm;
    use microcrop_treasury::DataKey as Tr;
    let until = w.pm.get_policy(&policy_id).end_date + 30 * 86_400;
    // Written for the policy at sale (NOW): its need then.
    let need = ((until - NOW).div_ceil(5)) as u32;
    // Per-org terms and reserve: the policy's need (they were read or written for it).
    let org_keys = [
        Tr::OrgReserve(w.org.clone()),
        Tr::OrgFeeSet(w.org.clone()),
        Tr::OrgFeeBps(w.org.clone()),
        Tr::OrgRatioSet(w.org.clone()),
        Tr::OrgReserveRatioBps(w.org.clone()),
    ];
    // (`OrgQueueHead` is first written when the head advances, so it is absent here.)
    let pending_keys = [
        Tr::OrgQueueTail(w.org.clone()),
        Tr::OrgPendingPayouts(w.org.clone()),
    ];
    let pm_keys = [
        Pm::OrgOutstanding(w.org.clone()),
        Pm::FarmerPolicyCount(w.farmer.clone()),
    ];
    // The fee terms and the reserve were used for this policy: its need (from the premium
    // at sale). The ratio terms were only read by `reserve_required`, which has no policy at
    // hand: the 60-day floor, from when the admin set them.
    for k in org_keys[..3].iter() {
        assert_eq!(ttl_in(&w.treasury_id, k.clone().into_val(&w.env)), need);
    }
    for k in org_keys[3..].iter() {
        assert_eq!(
            ttl_in(&w.treasury_id, k.clone().into_val(&w.env)),
            SHARED_FLOOR_LEDGERS
        );
    }

    advance_with_keeper(&w, 40);
    w.env.set_auths(&[]);
    w.pr.extend_policy_ttl(&policy_id);
    for k in pending_keys.iter() {
        assert_eq!(
            ttl_in(&w.treasury_id, k.clone().into_val(&w.env)),
            PENDING_TARGET_LEDGERS
        );
    }
    // Fee terms and reserve: left as they are (they already outlive the policy). Ratio
    // terms: under the floor, so topped up to at least 60 days.
    for k in org_keys[..3].iter() {
        assert_eq!(
            ttl_in(&w.treasury_id, k.clone().into_val(&w.env)),
            need - 40 * DAY
        );
    }
    for k in org_keys[3..].iter() {
        assert_eq!(
            ttl_in(&w.treasury_id, k.clone().into_val(&w.env)),
            SHARED_FLOOR_LEDGERS
        );
    }
    for k in pm_keys.iter() {
        assert_eq!(
            ttl_in(&w.pm.address, k.clone().into_val(&w.env)),
            need - 40 * DAY
        );
    }
}

// ============ CROP_DROUGHT end to end ============

#[test]
fn crop_determination_is_rejected_after_the_upgrade() {
    // CROP_DAMAGE is retired: disabled, so even a validly signed one is refused.
    let w = deploy();
    let premium: i128 = 10_000_000_000;
    let policy_id = w.create_and_activate(premium);
    w.pay_premium(policy_id, premium);
    let c = CropDetermination {
        on_chain_policy_id: policy_id,
        damage_percent_bp: 5_000,
        weather_damage: 50,
        satellite_damage: 50,
        payout_amount: PAYOUT,
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
    };
    let digest = determination_digest(&w.env, &c, &w.pr_addr, &w.domain);
    let (sig, recid) = sign_digest(&w.env, &w.sk, digest.to_array());
    assert!(!w
        .pr
        .is_methodology_enabled(&MethodologyKind::CropDualIndex10));
    assert_eq!(
        w.pr.try_submit_determination(&w.relayer, &c, &sig, &recid),
        Err(Ok(PayoutError::MethodologyDisabled.into()))
    );
    assert_eq!(w.token.balance(&w.backend_wallet), 0);
}

#[test]
fn cover_bought_after_planting_does_not_settle_that_season() {
    // Activated the day after planting: the buyer has seen the start of W1. The season's
    // evidence cannot settle the policy (anti-adverse-selection rule), however dry it was.
    let w = deploy();
    w.env.ledger().set_timestamp(PLANT + 86_400 + 60);
    let premium: i128 = 10_000_000_000;
    let policy_id = w.create_and_activate(premium);
    w.pay_premium(policy_id, premium);
    let d = determination(&w.env, policy_id, SUM_INSURED, PAYOUT);
    let (sig, recid) = sign(&w.env, &w.sk, &w.pr_addr, &w.domain, &d);
    assert_eq!(
        w.try_submit(&d, &sig, recid),
        Err(Ok(PayoutError::EvidenceOutsideCoverWindow.into()))
    );
    assert_eq!(w.pm.get_policy(&policy_id).status, PolicyStatus::Active);
}

/// Positive end-to-end with REAL evidence: the 2022 long-rains season at the oracle's pilot
/// pixel (ERA5 cell 1.0 N / 35.0 E, Trans Nzoia), planted 2022-04-01. The values below are
/// what the oracle's own index (`calculateDroughtDamage`, microcrop-lit-oracle 1bc2d9d
/// `src/scoring/drought.shared.js`) returns for the live Open-Meteo archive response of
///   https://archive-api.open-meteo.com/v1/archive?latitude=1.016700&longitude=35.006200&start_date=1991-01-01&end_date=2022-07-19&daily=precipitation_sum&timezone=Africa%2FNairobi&models=era5
/// fetched 2026-10-18 (209,608 bytes, sha256 213bdcc7…b7c117); `source_hash` is keccak256 of
/// that run's canonical projection. Signed here with the test key; the ledger clock is set
/// inside the window.
#[test]
fn real_era5_season_settles_end_to_end() {
    const PLANT_2022: u64 = 1_648_771_200; // 2022-04-01
    const LAT: i64 = 1_016_700;
    const LON: i64 = 35_006_200;
    let w = deploy();
    w.env.ledger().set_timestamp(PLANT_2022 + 6 * 3_600);
    let premium: i128 = 1_000_000_000;
    let policy_id = w.pm.create_policy(
        &w.backend,
        &w.farmer,
        &7u64,
        &LAT,
        &LON,
        &SUM_INSURED,
        &premium,
        &DURATION_DAYS,
        &CoverageType::Drought,
        &w.org,
    );
    w.pm.activate_policy(
        &w.backend,
        &policy_id,
        &w.distributor,
        &String::from_str(&w.env, "Acme Distributor"),
        &String::from_str(&w.env, "Trans Nzoia"),
    );
    w.token_admin.mint(&w.org, &SUM_INSURED);
    w.treasury.deposit_reserve(&w.org, &w.org, &SUM_INSURED);
    w.pay_premium(policy_id, premium);

    // W1 31130 vs normal 27260 (satisfied); W2 16280 vs 22305: s2 = 729, d2 = 28, g2 = 42.
    let source_hash: [u8; 32] = [
        0x20, 0x7c, 0xa2, 0x6b, 0x24, 0x54, 0xf0, 0xd0, 0x8e, 0x04, 0x96, 0x2f, 0x42, 0x00, 0x8b,
        0x73, 0xba, 0x0a, 0x51, 0x74, 0x4f, 0x26, 0x73, 0xe9, 0x9a, 0x0e, 0xe9, 0xf5, 0xf5, 0x6a,
        0xf8, 0x42,
    ];
    let assessed = PLANT_2022 + 117 * 86_400;
    let d = DroughtDetermination {
        on_chain_policy_id: policy_id,
        damage_percent_bp: 4_200,
        drought_damage: 42,
        payout_amount: SUM_INSURED * 4_200 / 10_000,
        assessed_at: assessed,
        latitude_e6: LAT as i128,
        longitude_e6: LON as i128,
        sum_insured: SUM_INSURED,
        season_start_epoch: PLANT_2022,
        lgp_days: 180,
        rain_w1_e2: 31_130,
        rain_ref_w1_e2: 27_260,
        rain_w2_e2: 16_280,
        rain_ref_w2_e2: 22_305,
        cdd_w2_days: 7,
        source_hash: BytesN::from_array(&w.env, &source_hash),
        methodology_params_hash: BytesN::from_array(&w.env, &DROUGHT_PARAMS_HASH_RDI_1_0),
    };
    w.env.ledger().set_timestamp(assessed + 600);
    let (sig, recid) = sign(&w.env, &w.sk, &w.pr_addr, &w.domain, &d);
    // The view returns the digest that was signed.
    assert_eq!(
        w.pr.drought_determination_digest(&d),
        drought_determination_digest(&w.env, &d, &w.pr_addr, &w.domain).to_bytes()
    );
    w.pr.submit_drought_determination(&w.relayer, &d, &sig, &recid);

    assert_eq!(
        w.token.balance(&w.backend_wallet),
        SUM_INSURED * 4_200 / 10_000
    );
    assert_eq!(w.pm.get_policy(&policy_id).status, PolicyStatus::Claimed);
    assert!(w.treasury.is_payout_processed(&policy_id));
    assert_eq!(w.pr.get_report(&policy_id).damage_percentage, 4_200);
    w.assert_solvent();
}
