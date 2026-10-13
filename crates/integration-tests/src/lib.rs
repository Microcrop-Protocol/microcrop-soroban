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
//!                 ─▶ sign a CropDetermination with a real k256 key
//!                 ─▶ submit_determination (real secp256k1_recover)
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
//! | `grant_role(Relayer, relayer)`                   | PayoutReceiver  | relayer may `submit_determination` (anti-spam)    |
//! | `set_authorized_signer(pubkey65)`                | PayoutReceiver  | the sole payout authority (the signature)         |
//! | `set_network_domain(domain)`                     | PayoutReceiver  | binds the signed digest to this network           |
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
    encoding::determination_digest,
    errors::{PayoutError, TreasuryError},
    types::{CoverageType, CropDetermination, PolicyStatus, Role},
};

use microcrop_payout_receiver::{
    PausedError as PrPausedError, PayoutReceiver, PayoutReceiverClient,
};
use microcrop_policy_manager::{PolicyManager, PolicyManagerClient};
use microcrop_policy_nft::{PolicyNft, PolicyNftClient};
use microcrop_treasury::{PausedError as TreasuryPausedError, Treasury, TreasuryClient};

// ============ Fixed test parameters ============

const NOW: u64 = 1_700_000_000;
/// 1,000 USDC (7dp, Stellar SAC) — within PolicyManager's [MIN_SUM_INSURED, MAX_SUM_INSURED].
const SUM_INSURED: i128 = 10_000_000_000;
/// weather 50% + satellite 50% => 60*50 + 40*50 = 5000 bp (the weighted invariant).
const DAMAGE_BP: u32 = 5_000;
/// SUM_INSURED * 5000 / 10000.
const PAYOUT: i128 = 5_000_000_000;
const DURATION_DAYS: u32 = 90;

// ============ secp256k1 signing helpers (copied verbatim from payout-receiver/src/test.rs) ============

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
/// Reuses the exact on-chain preimage/digest so `secp256k1_recover` round-trips.
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

/// A fully-consistent determination for `policy_id`, sum insured `sum_insured`, 50/50 sub-scores
/// (=> 5000 bp) assessed at `NOW`. `payout` must equal `sum_insured * DAMAGE_BP / 10000`.
fn determination(policy_id: u64, sum_insured: i128, payout: i128) -> CropDetermination {
    CropDetermination {
        on_chain_policy_id: policy_id,
        damage_percent_bp: DAMAGE_BP,
        weather_damage: 50,
        satellite_damage: 50,
        payout_amount: payout,
        assessed_at: NOW,
        latitude_e6: -1_286_389,
        longitude_e6: 36_817_223,
        sum_insured,
        ndvi_scaled: 4_200,
        weather_present: 1,
        weather_temp_c_e2: 2_750,
        weather_precip_e2: 15,
        weather_humidity: 80,
        weather_wind_e2: 350,
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
    }
}

impl<'a> World<'a> {
    /// create_policy (PENDING) then activate_policy (ACTIVE + real NFT mint). Returns policy_id.
    fn create_and_activate(&self, premium: i128) -> u64 {
        let policy_id = self.pm.create_policy(
            &self.backend,
            &self.farmer,
            &7u64, // plot_id
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

    // --- sign the determination with the real k256 key and submit it ---
    let d = determination(policy_id, SUM_INSURED, PAYOUT);
    let (sig, recid) = sign(&w.env, &w.sk, &w.pr_addr, &w.domain, &d);

    let backend_wallet_before = w.token.balance(&w.backend_wallet);
    w.pr.submit_determination(&w.relayer, &d, &sig, &recid);

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

    let d = determination(policy_id, SUM_INSURED, PAYOUT);
    let (sig, recid) = sign(&w.env, &w.sk, &w.pr_addr, &w.domain, &d);

    // First submission pays out.
    w.pr.submit_determination(&w.relayer, &d, &sig, &recid);
    let backend_wallet_after_first = w.token.balance(&w.backend_wallet);
    let reserve_after_first = w.treasury.org_reserve(&w.org);
    assert_eq!(backend_wallet_after_first, PAYOUT);

    // Second (identical) submission must be rejected. NOTE an emergent property only visible with
    // the REAL cross-contract wiring: the first payout drove PolicyManager ACTIVE -> CLAIMED, so on
    // replay the policy-status guard (`PolicyNotActive`, #308) fires *before* PayoutReceiver's own
    // persistent `PolicyAlreadyPaid` guard (#310) would. (The isolated unit test — whose mock keeps
    // the policy Active — sees #310 instead.) Either way the replay is blocked and no second payout
    // occurs; that difference is exactly what an integration test exists to surface.
    let res = w.pr.try_submit_determination(&w.relayer, &d, &sig, &recid);
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

/// Create + activate + pay a 100 USDC premium (90 USDC net reserve), then submit a valid
/// determination for a 500 USDC payout the reserve cannot cover. Returns the policy id.
fn record_short_claim(w: &World) -> u64 {
    let premium: i128 = 1_000_000_000; // 100 USDC; net after 10% fee = 90 USDC.
    let policy_id = w.create_and_activate(premium);
    w.pay_premium(policy_id, premium);
    let net = premium - premium / 10;
    assert!(net < PAYOUT);

    let d = determination(policy_id, SUM_INSURED, PAYOUT);
    let (sig, recid) = sign(&w.env, &w.sk, &w.pr_addr, &w.domain, &d);
    w.pr.submit_determination(&w.relayer, &d, &sig, &recid);
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
    let d = determination(policy_id, SUM_INSURED, PAYOUT);
    let (sig, recid) = sign(&w.env, &w.sk, &w.pr_addr, &w.domain, &d);
    assert!(w
        .pr
        .try_submit_determination(&w.relayer, &d, &sig, &recid)
        .is_err());

    // A different, smaller determination (signed later) cannot settle the policy either.
    w.env.ledger().set_timestamp(NOW + 60);
    let mut d2 = determination(policy_id, SUM_INSURED, SUM_INSURED * 3_000 / 10_000);
    d2.weather_damage = 30;
    d2.satellite_damage = 30;
    d2.damage_percent_bp = 3_000;
    d2.assessed_at = NOW + 60;
    let (sig2, recid2) = sign(&w.env, &w.sk, &w.pr_addr, &w.domain, &d2);
    let res =
        w.pr.try_submit_determination(&w.relayer, &d2, &sig2, &recid2);
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
    let first = record_short_claim(&w); // reserve 90 USDC, 500 USDC pending.

    // Same farmer (one claim in the window + one open policy is under the cap of 3).
    let premium: i128 = 1_000_000_000;
    let second = w.create_and_activate(premium);
    w.pay_premium(second, premium);
    // Reserve 600 USDC: the second 500 USDC claim fits the reserve, but only 100 USDC of it
    // is not already owed to the first claim.
    let top_up: i128 = 4_200_000_000;
    w.token_admin.mint(&w.org, &top_up);
    w.treasury.deposit_reserve(&w.org, &w.org, &top_up);
    assert_eq!(w.treasury.org_reserve(&w.org), 6_000_000_000);

    let d = determination(second, SUM_INSURED, PAYOUT);
    let (sig, recid) = sign(&w.env, &w.sk, &w.pr_addr, &w.domain, &d);
    w.pr.submit_determination(&w.relayer, &d, &sig, &recid);

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
    let d = determination(policy_id, SUM_INSURED, PAYOUT);
    let (sig, recid) = sign(&w.env, &attacker, &w.pr_addr, &w.domain, &d);

    let res = w.pr.try_submit_determination(&w.relayer, &d, &sig, &recid);
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
