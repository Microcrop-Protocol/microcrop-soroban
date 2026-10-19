#![cfg(test)]
//! # Cross-contract integration tests
//!
//! Every other test crate in this workspace exercises ONE contract with in-file `#[contract]`
//! mocks standing in for its siblings. This crate does the opposite: it registers all FOUR
//! **real** contracts — [`PolicyManager`], [`SettlementAdapter`], [`PayoutReceiver`],
//! [`PolicyNft`] — on a single shared [`Env`], wires them the way `scripts/deploy.sh` does, and
//! runs the whole Option B flow end-to-end:
//!
//! ```text
//!   create_policy ─▶ activate_policy (mints the real NFT)       (no premium on-chain)
//!                 ─▶ sign a DroughtDetermination with a real k256 key
//!                 ─▶ submit_drought_determination (real secp256k1_recover)
//!                 ─▶ SettlementAdapter.record_obligation  (Owed; no token anywhere)
//!                 ─▶ mark_funded (Funder) ─▶ mark_settled (org)  (attestations)
//! ```
//!
//! The payout authority is a genuine secp256k1 signature over the exact digest the
//! `PayoutReceiver` rebuilds on-chain. No assertion is weakened and no contract is stubbed.
//! There is no USDC Stellar Asset Contract in this world: nothing on-chain holds money.
//!
//! The `#[ignore]`d [`wasm_lane_full_flow`] repeats the core flow against the optimized WASM
//! files in `target/` (testing.md: "cross-contract interactions tested against real WASM").
//!
//! ## Cross-contract wiring (what deploy.sh does and verify-deployment.sh checks)
//! | grant / setter                                   | on contract       | so that…                                          |
//! |--------------------------------------------------|-------------------|---------------------------------------------------|
//! | `grant_role(Backend, backend)`                   | PolicyManager     | backend may `create_policy` / `activate_policy`   |
//! | `grant_role(Oracle,  payout_receiver_addr)`      | PolicyManager     | PR (as itself) may `mark_as_claimed` + `increment_claim_count` |
//! | `set_policy_nft(nft_addr)`                        | PolicyManager     | activation can mint the certificate               |
//! | `grant_role(Minter, policy_manager_addr)`        | PolicyNft         | PM (as itself) may `mint_policy` on activation    |
//! | `set_policy_manager(policy_manager_addr)`        | PolicyNft         | PM may flip the cert active-flag on claim         |
//! | `set_payout_receiver(payout_receiver_addr)`      | SettlementAdapter | only the PR may `record_obligation`               |
//! | `grant_role(Funder, backend)`                    | SettlementAdapter | backend may mark any obligation funded / settled  |
//! | `grant_role(Relayer, relayer)`                   | PayoutReceiver    | relayer may submit determinations (anti-spam)     |
//! | `set_authorized_signer(pubkey65)`                | PayoutReceiver    | the sole payout authority (the signature)         |
//! | `set_network_domain(domain)`                     | PayoutReceiver    | binds the signed digest to this network           |
//! | `set_methodology_enabled(CropDroughtRdi10)`       | PayoutReceiver    | drought determinations are accepted at all        |

extern crate std;

use soroban_sdk::{
    testutils::{Address as _, Ledger as _, MockAuth, MockAuthInvoke},
    Address, BytesN, Env, IntoVal, String,
};

use k256::ecdsa::SigningKey;

use microcrop_shared::{
    encoding::{determination_digest, drought_determination_digest, DROUGHT_PARAMS_HASH_RDI_1_0},
    errors::{AdapterError, PayoutError, PolicyError},
    types::{
        CoverageType, CropDetermination, DroughtDetermination, MethodologyKind, ObligationStatus,
        PolicyStatus, Role,
    },
};

use microcrop_payout_receiver::{PayoutReceiver, PayoutReceiverClient};
use microcrop_policy_manager::{PolicyManager, PolicyManagerClient};
use microcrop_policy_nft::{PolicyNft, PolicyNftClient};
use microcrop_settlement_adapter::{SettlementAdapter, SettlementAdapterClient};

// ============ Fixed test parameters ============

/// Planting day, 2025-04-01 00:00 UTC (the drought evidence's `season_start_epoch`).
const PLANT: u64 = 1_743_465_600;
/// Sale and activation: 06:00 UTC on planting day (the cover must start by planting).
const NOW: u64 = PLANT + 6 * 3_600;
/// 1,000 USDC (7dp) — within PolicyManager's [MIN_SUM_INSURED, MAX_SUM_INSURED].
const SUM_INSURED: i128 = 10_000_000_000;
/// 100 USDC. Recorded on the policy; paid off-chain (M-Pesa), never on-chain.
const PREMIUM: i128 = 1_000_000_000;
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
    pm: PolicyManagerClient<'a>,
    nft: PolicyNftClient<'a>,
    adapter: SettlementAdapterClient<'a>,
    pr: PayoutReceiverClient<'a>,
    pr_addr: Address,
    // actors
    backend: Address,
    farmer: Address,
    distributor: Address,
    org: Address,
    relayer: Address,
    // signing
    sk: SigningKey,
    domain: BytesN<32>,
}

/// Deploy + wire all four contracts exactly as deploy.sh does. `mock_all_auths` covers every
/// `require_auth` on the happy path; the auth-specific tests switch it off.
fn deploy<'a>() -> World<'a> {
    deploy_with(true)
}

/// `bind == false` skips `adapter.set_payout_receiver(pr)`, the one step that has to follow
/// the PayoutReceiver's deployment.
fn deploy_with<'a>(bind: bool) -> World<'a> {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(NOW);

    let admin = Address::generate(&env);
    let backend = Address::generate(&env);
    let farmer = Address::generate(&env);
    let distributor = Address::generate(&env);
    let org = Address::generate(&env);
    let relayer = Address::generate(&env);

    let nft_addr = env.register(
        PolicyNft,
        (
            admin.clone(),
            String::from_str(&env, "MicroCrop Policy"),
            String::from_str(&env, "MCP"),
        ),
    );
    let pm_addr = env.register(PolicyManager, (admin.clone(),));
    let adapter_addr = env.register(SettlementAdapter, (admin.clone(),));
    let pr_addr = env.register(
        PayoutReceiver,
        (adapter_addr.clone(), pm_addr.clone(), admin.clone()),
    );

    let nft = PolicyNftClient::new(&env, &nft_addr);
    let pm = PolicyManagerClient::new(&env, &pm_addr);
    let adapter = SettlementAdapterClient::new(&env, &adapter_addr);
    let pr = PayoutReceiverClient::new(&env, &pr_addr);

    pm.set_policy_nft(&admin, &nft_addr);
    nft.set_policy_manager(&admin, &pm_addr);
    if bind {
        adapter.set_payout_receiver(&admin, &pr_addr);
    }

    pm.grant_role(&admin, &Role::Backend, &backend);
    nft.grant_role(&admin, &Role::Minter, &pm_addr);
    pm.grant_role(&admin, &Role::Oracle, &pr_addr);
    adapter.grant_role(&admin, &Role::Funder, &backend);
    pr.grant_role(&admin, &Role::Relayer, &relayer);

    let sk = sk_from(0x11);
    pr.set_authorized_signer(&admin, &pubkey65(&env, &sk));
    let domain = BytesN::from_array(&env, &[0xABu8; 32]);
    pr.set_network_domain(&admin, &domain);
    pr.set_methodology_enabled(&admin, &MethodologyKind::CropDroughtRdi10, &true);

    World {
        env,
        admin,
        pm,
        nft,
        adapter,
        pr,
        pr_addr,
        backend,
        farmer,
        distributor,
        org,
        relayer,
        sk,
        domain,
    }
}

impl<'a> World<'a> {
    /// create_policy (PENDING) then activate_policy (ACTIVE + real NFT mint). Returns policy_id.
    /// The premium is recorded on the policy but never paid on-chain (it stays off-chain, with
    /// its M-Pesa reference).
    fn create_and_activate(&self) -> u64 {
        let policy_id = self.pm.create_policy(
            &self.backend,
            &self.farmer,
            &7u64,
            &PLOT_LAT_E6,
            &PLOT_LON_E6,
            &SUM_INSURED,
            &PREMIUM,
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

    /// Move the ledger to the relayer's submission time (a minute after the assessment).
    fn at_assessment(&self) {
        if self.env.ledger().timestamp() < ASSESSED + 60 {
            self.env.ledger().set_timestamp(ASSESSED + 60);
        }
    }

    fn submit(&self, d: &DroughtDetermination) {
        self.at_assessment();
        let (sig, recid) = sign(&self.env, &self.sk, &self.pr_addr, &self.domain, d);
        self.pr
            .submit_drought_determination(&self.relayer, d, &sig, &recid);
    }

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

    /// Nothing anywhere recorded a claim for `policy_id`.
    fn assert_nothing_owed(&self, policy_id: u64) {
        assert!(!self.pr.is_policy_paid(&policy_id));
        assert_eq!(
            self.adapter.find_obligation(&self.pr_addr, &policy_id),
            None
        );
        assert_eq!(self.adapter.obligation_count(), 0);
        assert_eq!(self.pm.get_policy(&policy_id).status, PolicyStatus::Active);
    }
}

fn reference(env: &Env, b: u8) -> BytesN<32> {
    BytesN::from_array(env, &[b; 32])
}

// ============ 1. Full lifecycle: determination -> obligation -> funded -> settled ============

#[test]
fn full_lifecycle_determination_to_obligation_to_settlement() {
    let w = deploy();

    let policy_id = w.create_and_activate();
    assert_eq!(policy_id, 1);
    assert_eq!(w.nft.owner_of(&policy_id), w.farmer);
    assert_eq!(w.pm.get_policy(&policy_id).status, PolicyStatus::Active);
    assert_eq!(w.pm.org_outstanding_sum_insured(&w.org), SUM_INSURED);

    // After the season: sign with the real k256 key and submit.
    let d = determination(&w.env, policy_id, SUM_INSURED, PAYOUT);
    w.submit(&d);
    let digest = drought_determination_digest(&w.env, &d, &w.pr_addr, &w.domain).to_bytes();

    // Owed, exactly the verified claim.
    let id = w.adapter.find_obligation(&w.pr_addr, &policy_id).unwrap();
    let ob = w.adapter.get_obligation(&id).unwrap();
    assert_eq!(ob.id, 1);
    assert_eq!(ob.payout_receiver, w.pr_addr);
    assert_eq!(ob.policy_id, policy_id);
    assert_eq!(ob.org, w.org);
    assert_eq!(ob.amount, PAYOUT);
    assert_eq!(ob.determination_digest, digest);
    assert_eq!(ob.status, ObligationStatus::Owed);
    assert_eq!(w.adapter.org_obligation_count(&w.org), 1);
    // Policy CLAIMED, exposure released, report stored; the farmer still owns the certificate.
    assert!(w.pr.is_policy_paid(&policy_id));
    assert_eq!(w.pm.get_policy(&policy_id).status, PolicyStatus::Claimed);
    assert_eq!(w.pm.org_outstanding_sum_insured(&w.org), 0);
    assert_eq!(w.pm.get_farmer_claim_count(&w.farmer), 1);
    let report = w.pr.get_report(&policy_id);
    assert_eq!(report.payout_amount, PAYOUT);
    assert_eq!(report.damage_percentage, DAMAGE_BP);
    assert_eq!(w.nft.owner_of(&policy_id), w.farmer);
    assert!(!w.nft.get_certificate(&policy_id).is_active);

    // Funded: the backend (Funder) attests the off-ramp order was accepted.
    let rf = reference(&w.env, 0xF1);
    let ob = w.adapter.mark_funded(&w.backend, &id, &rf);
    assert_eq!(ob.status, ObligationStatus::Funded);
    assert_eq!(ob.funded_by, Some(w.backend.clone()));

    // Settled: the org itself attests, with its own signature and nothing else.
    let rs = reference(&w.env, 0x51);
    let ob = w
        .adapter
        .mock_auths(&[MockAuth {
            address: &w.org,
            invoke: &MockAuthInvoke {
                contract: &w.adapter.address,
                fn_name: "mark_settled",
                args: (w.org.clone(), id, rs.clone()).into_val(&w.env),
                sub_invokes: &[],
            },
        }])
        .mark_settled(&w.org, &id, &rs);
    assert_eq!(ob.status, ObligationStatus::Settled);
    assert_eq!(ob.settled_by, Some(w.org.clone()));
    assert_eq!(ob.funding_reference, Some(rf));
    assert_eq!(ob.settlement_reference, Some(rs));
    assert_eq!(w.pm.get_policy(&policy_id).status, PolicyStatus::Claimed);
    assert_eq!(w.nft.owner_of(&policy_id), w.farmer);
}

// ============ 2. Replay ============

#[test]
fn determination_replay_rejected() {
    let w = deploy();
    let policy_id = w.create_and_activate();
    let d = determination(&w.env, policy_id, SUM_INSURED, PAYOUT);
    let (sig, recid) = sign(&w.env, &w.sk, &w.pr_addr, &w.domain, &d);
    w.try_submit(&d, &sig, recid).unwrap().unwrap();

    // With the REAL wiring the first claim drove PolicyManager ACTIVE -> CLAIMED, so on replay
    // the status guard (#308) fires before PayoutReceiver's own PolicyPaid guard (#310).
    assert_eq!(
        w.try_submit(&d, &sig, recid),
        Err(Ok(PayoutError::PolicyNotActive.into()))
    );
    assert_eq!(w.adapter.obligation_count(), 1);
}

/// Swapping the adapter cannot reopen a settled policy: neither the same determination nor a
/// new one records a second obligation in the new adapter.
#[test]
fn replay_after_adapter_swap_still_rejected() {
    let w = deploy();
    let policy_id = w.create_and_activate();
    let d = determination(&w.env, policy_id, SUM_INSURED, PAYOUT);
    let (sig, recid) = sign(&w.env, &w.sk, &w.pr_addr, &w.domain, &d);
    w.try_submit(&d, &sig, recid).unwrap().unwrap();

    let adapter2_addr = w.env.register(SettlementAdapter, (w.admin.clone(),));
    let adapter2 = SettlementAdapterClient::new(&w.env, &adapter2_addr);
    // Not yet bound: refused.
    assert_eq!(
        w.pr.try_set_settlement_adapter(&w.admin, &adapter2_addr),
        Err(Ok(PayoutError::SettlementAdapterNotBound.into()))
    );
    adapter2.set_payout_receiver(&w.admin, &w.pr_addr);
    w.pr.set_settlement_adapter(&w.admin, &adapter2_addr);
    assert_eq!(w.pr.settlement_adapter(), adapter2_addr);

    assert_eq!(
        w.try_submit(&d, &sig, recid),
        Err(Ok(PayoutError::PolicyNotActive.into()))
    );
    w.env.ledger().set_timestamp(ASSESSED + 120);
    let mut d2 = determination(&w.env, policy_id, SUM_INSURED, PAYOUT);
    d2.assessed_at = ASSESSED + 100;
    let (sig2, recid2) = sign(&w.env, &w.sk, &w.pr_addr, &w.domain, &d2);
    assert!(w.try_submit(&d2, &sig2, recid2).is_err());
    assert_eq!(adapter2.obligation_count(), 0);
    // The original obligation stays in the original adapter, still markable.
    assert_eq!(w.adapter.obligation_count(), 1);
    w.adapter
        .mark_settled(&w.backend, &1, &reference(&w.env, 0x51));
}

#[test]
fn adapter_not_bound_reverts_atomically_then_succeeds_after_binding() {
    let w = deploy_with(false);
    let policy_id = w.create_and_activate();
    let d = determination(&w.env, policy_id, SUM_INSURED, PAYOUT);
    let (sig, recid) = sign(&w.env, &w.sk, &w.pr_addr, &w.domain, &d);
    assert_eq!(
        w.try_submit(&d, &sig, recid),
        Err(Ok(soroban_sdk::Error::from_contract_error(
            microcrop_shared::errors::CommonError::NotConfigured as u32
        )))
    );
    w.assert_nothing_owed(policy_id);

    // Bound to another receiver: still refused, still nothing persists.
    let other = Address::generate(&w.env);
    w.adapter.set_payout_receiver(&w.admin, &other);
    assert_eq!(
        w.try_submit(&d, &sig, recid),
        Err(Ok(soroban_sdk::Error::from_contract_error(
            AdapterError::NotPayoutReceiver as u32
        )))
    );
    w.assert_nothing_owed(policy_id);

    // Bound to this one: the same signed determination settles.
    w.adapter.set_payout_receiver(&w.admin, &w.pr_addr);
    w.try_submit(&d, &sig, recid).unwrap().unwrap();
    assert_eq!(w.adapter.obligation_count(), 1);
    assert_eq!(w.pm.get_policy(&policy_id).status, PolicyStatus::Claimed);
}

#[test]
fn wrong_signer_determination_rejected() {
    let w = deploy();
    let policy_id = w.create_and_activate();
    let attacker = sk_from(0x22);
    let d = determination(&w.env, policy_id, SUM_INSURED, PAYOUT);
    let (sig, recid) = sign(&w.env, &attacker, &w.pr_addr, &w.domain, &d);
    assert_eq!(
        w.try_submit(&d, &sig, recid),
        Err(Ok(PayoutError::InvalidSignature.into()))
    );
    w.assert_nothing_owed(policy_id);
}

// ============ has_role reports the wiring a deployment depends on ============
//
// These grants produce a SILENTLY BROKEN deployment when missed: none fails at deploy time,
// two go to CONTRACTS rather than people. scripts/verify-deployment.sh relies on exactly these
// queries.

#[test]
fn has_role_reports_the_money_path_grants() {
    let w = deploy();
    assert!(
        w.nft.has_role(&Role::Minter, &w.pm.address),
        "PolicyManager must hold Minter on PolicyNft"
    );
    assert!(
        w.pm.has_role(&Role::Oracle, &w.pr_addr),
        "PayoutReceiver must hold Oracle on PolicyManager"
    );
    assert!(
        w.adapter.has_role(&Role::Funder, &w.backend),
        "the backend must hold Funder on the SettlementAdapter"
    );
    assert_eq!(w.adapter.payout_receiver(), Some(w.pr_addr.clone()));
    assert_eq!(w.pr.settlement_adapter(), w.adapter.address);
    assert!(w.pm.has_role(&Role::Backend, &w.backend));
    assert!(w.pr.has_role(&Role::Relayer, &w.relayer));
}

#[test]
fn has_role_is_false_for_non_holders_and_is_per_contract() {
    let w = deploy();
    assert!(w.pm.has_role(&Role::Backend, &w.backend));
    assert!(
        !w.nft.has_role(&Role::Backend, &w.backend),
        "Backend on PolicyManager must not leak into PolicyNft"
    );
    assert!(
        !w.pm.has_role(&Role::Funder, &w.backend),
        "Funder on the adapter must not leak into PolicyManager"
    );
    assert!(!w.pm.has_role(&Role::Oracle, &w.backend));
    assert!(!w.adapter.has_role(&Role::Funder, &w.farmer));
    assert!(!w.adapter.has_role(&Role::Funder, &w.pr_addr));
    assert!(!w.pr.has_role(&Role::Relayer, &w.farmer));
}

#[test]
fn revoking_a_role_is_visible_through_has_role() {
    let w = deploy();
    let policy_id = w.create_and_activate();
    w.submit(&determination(&w.env, policy_id, SUM_INSURED, PAYOUT));
    // An emergency lockout of the Funder must be observable, and effective.
    w.adapter.revoke_role(&w.admin, &Role::Funder, &w.backend);
    assert!(
        !w.adapter.has_role(&Role::Funder, &w.backend),
        "revoke must be visible via has_role"
    );
    assert!(w
        .adapter
        .try_mark_funded(&w.backend, &1, &reference(&w.env, 1))
        .is_err());
}

// ============ Coverage type and plot binding, end to end ============

#[test]
fn only_drought_cover_can_be_sold() {
    let w = deploy();
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
                &PREMIUM,
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
    let policy_id = w.create_and_activate();
    let mut d = determination(&w.env, policy_id, SUM_INSURED, PAYOUT);
    d.latitude_e6 = -1_300_000; // ~1.5 km south
    let (sig, recid) = sign(&w.env, &w.sk, &w.pr_addr, &w.domain, &d);
    assert_eq!(
        w.try_submit(&d, &sig, recid),
        Err(Ok(PayoutError::PlotLocationMismatch.into()))
    );
    w.assert_nothing_owed(policy_id);
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
    w.create_and_activate();
    let inst = |a: &Address| {
        w.env
            .as_contract(a, || w.env.storage().instance().get_ttl())
    };
    let all = [
        w.pm.address.clone(),
        w.nft.address.clone(),
        w.adapter.address.clone(),
        w.pr_addr.clone(),
    ];
    // Every call so far extended code and instances to 10 days at most.
    for a in all.iter() {
        assert!(inst(a) <= USER_TARGET_LEDGERS);
    }
    // One permissionless call extends all four to 30 days.
    w.env.set_auths(&[]);
    w.pr.extend_contract_ttl();
    for a in all.iter() {
        assert_eq!(inst(a), KEEPER_TARGET_LEDGERS);
    }
}

#[test]
fn extend_policy_ttl_reaches_every_contract() {
    use microcrop_settlement_adapter::DataKey as Ad;
    use microcrop_shared::ttl::{NETWORK_MAX_ENTRY_TTL_LEDGERS, PENDING_TARGET_LEDGERS};
    use soroban_sdk::testutils::storage::Persistent as _;

    let w = deploy();
    w.env
        .ledger()
        .set_max_entry_ttl(NETWORK_MAX_ENTRY_TTL_LEDGERS);
    let policy_id = w.create_and_activate();
    w.submit(&determination(&w.env, policy_id, SUM_INSURED, PAYOUT));

    let ttl_in = |contract: &Address, k: soroban_sdk::Val| {
        w.env
            .as_contract(contract, || w.env.storage().persistent().get_ttl(&k))
    };
    let pm_policy = || microcrop_policy_manager::DataKey::Policy(policy_id).into_val(&w.env);
    let pr_paid = || microcrop_payout_receiver::DataKey::PolicyPaid(policy_id).into_val(&w.env);
    let nft_cert = || microcrop_policy_nft::DataKey::Certificate(policy_id).into_val(&w.env);
    let ob_keys = || {
        [
            Ad::Obligation(1).into_val(&w.env),
            Ad::ObligationOf(w.pr_addr.clone(), policy_id).into_val(&w.env),
            Ad::OrgObligationAt(w.org.clone(), 0).into_val(&w.env),
        ]
    };

    // Per-policy entries live until end + 30 days, counted from when they were written.
    let until = w.pm.get_policy(&policy_id).end_date + 30 * 86_400;
    let need_at = |t: u64| ((until - t).div_ceil(5)) as u32;
    assert_eq!(ttl_in(&w.pr_addr, pr_paid()), need_at(ASSESSED + 60));
    assert_eq!(ttl_in(&w.nft.address, nft_cert()), need_at(NOW));
    assert_eq!(ttl_in(&w.pm.address, pm_policy()), need_at(NOW));
    // The open obligation and its lookup/index items: 60 days.
    for k in ob_keys() {
        assert_eq!(ttl_in(&w.adapter.address, k), PENDING_TARGET_LEDGERS);
    }
    let before = [
        ttl_in(&w.pm.address, pm_policy()),
        ttl_in(&w.pr_addr, pr_paid()),
        ttl_in(&w.nft.address, nft_cert()),
    ];

    // 40 days with only the contract keeper running: the obligation is under 30 days.
    advance_with_keeper(&w, 40);
    for k in ob_keys() {
        assert_eq!(ttl_in(&w.adapter.address, k), 20 * DAY);
    }

    // One permissionless call reaches the adapter through the PayoutReceiver.
    w.env.set_auths(&[]);
    w.pr.extend_policy_ttl(&policy_id);
    for k in ob_keys() {
        assert_eq!(ttl_in(&w.adapter.address, k), PENDING_TARGET_LEDGERS);
    }
    // Closed per-policy entries already live until end + 30 days: left as they are.
    for (i, (c, k)) in [
        (&w.pm.address, pm_policy()),
        (&w.pr_addr, pr_paid()),
        (&w.nft.address, nft_cert()),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(ttl_in(c, k), before[i] - 40 * DAY);
    }

    // Once Settled, the keeper leaves the obligation to archive.
    w.env.mock_all_auths();
    w.adapter
        .mark_settled(&w.backend, &1, &reference(&w.env, 0x51));
    advance_with_keeper(&w, 10);
    let settled = ttl_in(&w.adapter.address, Ad::Obligation(1).into_val(&w.env));
    w.pr.extend_policy_ttl(&policy_id);
    assert_eq!(
        ttl_in(&w.adapter.address, Ad::Obligation(1).into_val(&w.env)),
        settled
    );
}

#[test]
fn extend_policy_ttl_keeps_org_and_farmer_entries_alive() {
    use microcrop_settlement_adapter::DataKey as Ad;
    use microcrop_shared::ttl::{NETWORK_MAX_ENTRY_TTL_LEDGERS, SHARED_FLOOR_LEDGERS};
    use soroban_sdk::testutils::storage::Persistent as _;

    let w = deploy();
    w.env
        .ledger()
        .set_max_entry_ttl(NETWORK_MAX_ENTRY_TTL_LEDGERS);
    let policy_id = w.create_and_activate();
    w.submit(&determination(&w.env, policy_id, SUM_INSURED, PAYOUT));

    let ttl_in = |contract: &Address, k: soroban_sdk::Val| {
        w.env
            .as_contract(contract, || w.env.storage().persistent().get_ttl(&k))
    };
    use microcrop_policy_manager::DataKey as Pm;
    let until = w.pm.get_policy(&policy_id).end_date + 30 * 86_400;
    let need = ((until - NOW).div_ceil(5)) as u32;
    let pm_keys = [
        Pm::OrgOutstanding(w.org.clone()),
        Pm::FarmerPolicyCount(w.farmer.clone()),
    ];
    let org_count = || Ad::OrgObligationCount(w.org.clone()).into_val(&w.env);
    assert_eq!(
        ttl_in(&w.adapter.address, org_count()),
        SHARED_FLOOR_LEDGERS
    );

    advance_with_keeper(&w, 40);
    w.env.set_auths(&[]);
    w.pr.extend_policy_ttl(&policy_id);
    for k in pm_keys.iter() {
        assert_eq!(
            ttl_in(&w.pm.address, k.clone().into_val(&w.env)),
            need - 40 * DAY
        );
    }
    // The org's obligation count: back to at least the 60-day floor.
    assert_eq!(
        ttl_in(&w.adapter.address, org_count()),
        SHARED_FLOOR_LEDGERS
    );
}

// ============ CROP_DROUGHT end to end ============

#[test]
fn crop_determination_is_rejected_after_the_upgrade() {
    // CROP_DAMAGE is retired: disabled, so even a validly signed one is refused.
    let w = deploy();
    let policy_id = w.create_and_activate();
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
    w.assert_nothing_owed(policy_id);
}

#[test]
fn cover_bought_after_planting_does_not_settle_that_season() {
    // Activated the day after planting: the buyer has seen the start of W1. The season's
    // evidence cannot settle the policy (anti-adverse-selection rule), however dry it was.
    let w = deploy();
    w.env.ledger().set_timestamp(PLANT + 86_400 + 60);
    let policy_id = w.create_and_activate();
    let d = determination(&w.env, policy_id, SUM_INSURED, PAYOUT);
    let (sig, recid) = sign(&w.env, &w.sk, &w.pr_addr, &w.domain, &d);
    assert_eq!(
        w.try_submit(&d, &sig, recid),
        Err(Ok(PayoutError::EvidenceOutsideCoverWindow.into()))
    );
    w.assert_nothing_owed(policy_id);
}

/// The real 2022 long-rains season at the oracle's pilot pixel (ERA5 cell 1.0 N / 35.0 E,
/// Trans Nzoia), planted 2022-04-01, exactly as `scripts/testnet-evidence.sh` Lane E signs
/// it. The values are what the oracle's own index (`calculateDroughtDamage`,
/// microcrop-lit-oracle 1bc2d9d `src/scoring/drought.shared.js`) returns for the live
/// Open-Meteo archive response of
///   https://archive-api.open-meteo.com/v1/archive?latitude=1.016700&longitude=35.006200&start_date=1991-01-01&end_date=2022-07-19&daily=precipitation_sum&timezone=Africa%2FNairobi&models=era5
/// fetched 2026-10-18 (209,608 bytes, sha256 213bdcc7…b7c117); `source_hash` is keccak256 of
/// that run's canonical projection. Signed here with the test key; the ledger clock is set
/// inside the window.
#[test]
fn real_era5_season_records_an_obligation_end_to_end() {
    let w = deploy();
    w.env.ledger().set_timestamp(PLANT_2022 + 6 * 3_600);
    let policy_id = w.pm.create_policy(
        &w.backend,
        &w.farmer,
        &7u64,
        &ERA5_LAT,
        &ERA5_LON,
        &SUM_INSURED,
        &PREMIUM,
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
    let d = era5_determination(&w.env, policy_id);
    w.env.ledger().set_timestamp(d.assessed_at + 600);
    let (sig, recid) = sign(&w.env, &w.sk, &w.pr_addr, &w.domain, &d);
    // The view returns the digest that was signed.
    let digest = drought_determination_digest(&w.env, &d, &w.pr_addr, &w.domain).to_bytes();
    assert_eq!(w.pr.drought_determination_digest(&d), digest);
    w.pr.submit_drought_determination(&w.relayer, &d, &sig, &recid);

    let ob = w.adapter.get_obligation(&1).unwrap();
    assert_eq!(ob.amount, SUM_INSURED * 4_200 / 10_000);
    assert_eq!(ob.determination_digest, digest);
    assert_eq!(ob.status, ObligationStatus::Owed);
    assert_eq!(w.pm.get_policy(&policy_id).status, PolicyStatus::Claimed);
    assert_eq!(w.pr.get_report(&policy_id).damage_percentage, 4_200);
}

const PLANT_2022: u64 = 1_648_771_200; // 2022-04-01
const ERA5_LAT: i64 = 1_016_700;
const ERA5_LON: i64 = 35_006_200;

/// The 2022 ERA5 determination (4,200 bp) for `policy_id`, assessed 117 days after planting.
fn era5_determination(env: &Env, policy_id: u64) -> DroughtDetermination {
    // W1 31130 vs normal 27260 (satisfied); W2 16280 vs 22305: s2 = 729, d2 = 28, g2 = 42.
    let source_hash: [u8; 32] = [
        0x20, 0x7c, 0xa2, 0x6b, 0x24, 0x54, 0xf0, 0xd0, 0x8e, 0x04, 0x96, 0x2f, 0x42, 0x00, 0x8b,
        0x73, 0xba, 0x0a, 0x51, 0x74, 0x4f, 0x26, 0x73, 0xe9, 0x9a, 0x0e, 0xe9, 0xf5, 0xf5, 0x6a,
        0xf8, 0x42,
    ];
    DroughtDetermination {
        on_chain_policy_id: policy_id,
        damage_percent_bp: 4_200,
        drought_damage: 42,
        payout_amount: SUM_INSURED * 4_200 / 10_000,
        assessed_at: PLANT_2022 + 117 * 86_400,
        latitude_e6: ERA5_LAT as i128,
        longitude_e6: ERA5_LON as i128,
        sum_insured: SUM_INSURED,
        season_start_epoch: PLANT_2022,
        lgp_days: 180,
        rain_w1_e2: 31_130,
        rain_ref_w1_e2: 27_260,
        rain_w2_e2: 16_280,
        rain_ref_w2_e2: 22_305,
        cdd_w2_days: 7,
        source_hash: BytesN::from_array(env, &source_hash),
        methodology_params_hash: BytesN::from_array(env, &DROUGHT_PARAMS_HASH_RDI_1_0),
    }
}

// ============ Real-WASM lane ============

mod wasm {
    //! The four optimized contracts as built by `stellar contract build` +
    //! `stellar contract optimize` (or plain `cargo build --release --target wasm32v1-none`).
    use std::path::PathBuf;

    pub fn bytes(name: &str) -> std::vec::Vec<u8> {
        let dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/wasm32v1-none/release");
        let optimized = dir.join(std::format!("{name}.optimized.wasm"));
        let plain = dir.join(std::format!("{name}.wasm"));
        let path = if optimized.exists() { optimized } else { plain };
        std::fs::read(&path).unwrap_or_else(|e| {
            panic!(
                "{}: {e}. Build first: cargo build --target wasm32v1-none --release",
                path.display()
            )
        })
    }
}

/// The core flow against the compiled WASM (testing.md, "Cross-contract interactions tested
/// against real WASM"). Ignored by default because it needs the wasm build in `target/`:
/// `cargo build --target wasm32v1-none --release && cargo test -p microcrop-integration-tests -- --ignored`.
#[test]
#[ignore]
fn wasm_lane_full_flow() {
    let env = Env::default();
    env.mock_all_auths();
    env.cost_estimate().budget().reset_unlimited();
    env.ledger().set_timestamp(PLANT_2022 + 6 * 3_600);
    let admin = Address::generate(&env);
    let backend = Address::generate(&env);
    let relayer = Address::generate(&env);
    let farmer = Address::generate(&env);
    let org = Address::generate(&env);

    let nft_wasm = wasm::bytes("microcrop_policy_nft");
    let pm_wasm = wasm::bytes("microcrop_policy_manager");
    let ad_wasm = wasm::bytes("microcrop_settlement_adapter");
    let pr_wasm = wasm::bytes("microcrop_payout_receiver");
    let nft_addr = env.register(
        nft_wasm.as_slice(),
        (
            admin.clone(),
            String::from_str(&env, "MicroCrop Policy"),
            String::from_str(&env, "MCP"),
        ),
    );
    let pm_addr = env.register(pm_wasm.as_slice(), (admin.clone(),));
    let ad_addr = env.register(ad_wasm.as_slice(), (admin.clone(),));
    let pr_addr = env.register(
        pr_wasm.as_slice(),
        (ad_addr.clone(), pm_addr.clone(), admin.clone()),
    );
    let nft = PolicyNftClient::new(&env, &nft_addr);
    let pm = PolicyManagerClient::new(&env, &pm_addr);
    let adapter = SettlementAdapterClient::new(&env, &ad_addr);
    let pr = PayoutReceiverClient::new(&env, &pr_addr);

    pm.set_policy_nft(&admin, &nft_addr);
    nft.set_policy_manager(&admin, &pm_addr);
    adapter.set_payout_receiver(&admin, &pr_addr);
    pm.grant_role(&admin, &Role::Backend, &backend);
    nft.grant_role(&admin, &Role::Minter, &pm_addr);
    pm.grant_role(&admin, &Role::Oracle, &pr_addr);
    adapter.grant_role(&admin, &Role::Funder, &backend);
    pr.grant_role(&admin, &Role::Relayer, &relayer);
    let sk = sk_from(0x11);
    pr.set_authorized_signer(&admin, &pubkey65(&env, &sk));
    let domain = BytesN::from_array(&env, &[0xABu8; 32]);
    pr.set_network_domain(&admin, &domain);
    pr.set_methodology_enabled(&admin, &MethodologyKind::CropDroughtRdi10, &true);

    let policy_id = pm.create_policy(
        &backend,
        &farmer,
        &7u64,
        &ERA5_LAT,
        &ERA5_LON,
        &SUM_INSURED,
        &PREMIUM,
        &DURATION_DAYS,
        &CoverageType::Drought,
        &org,
    );
    pm.activate_policy(
        &backend,
        &policy_id,
        &Address::generate(&env),
        &String::from_str(&env, "Acme Distributor"),
        &String::from_str(&env, "Trans Nzoia"),
    );
    let d = era5_determination(&env, policy_id);
    env.ledger().set_timestamp(d.assessed_at + 600);
    let (sig, recid) = sign(&env, &sk, &pr_addr, &domain, &d);
    pr.submit_drought_determination(&relayer, &d, &sig, &recid);

    let ob = adapter.get_obligation(&1).unwrap();
    assert_eq!(ob.status, ObligationStatus::Owed);
    assert_eq!(ob.amount, SUM_INSURED * 4_200 / 10_000);
    assert_eq!(pm.get_policy(&policy_id).status, PolicyStatus::Claimed);
    adapter.mark_funded(&backend, &1, &reference(&env, 0xF1));
    let ob = adapter.mark_settled(&org, &1, &reference(&env, 0x51));
    assert_eq!(ob.status, ObligationStatus::Settled);
    // Replay is refused by the compiled contract too.
    assert!(pr
        .try_submit_drought_determination(&relayer, &d, &sig, &recid)
        .is_err());
}
