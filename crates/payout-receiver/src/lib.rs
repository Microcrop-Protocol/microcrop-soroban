#![no_std]
//! # PayoutReceiver
//!
//! Soroban port of `microcrop-contracts/microcrop/src/PayoutReceiver.sol` (v2.1.0). This is
//! the crux contract: it verifies a PKP-signed CROP_DAMAGE determination and, iff valid,
//! triggers the payout. The **signature is the sole payout authority**; `Role::Relayer` is an
//! anti-spam gate on WHO may relay and does NOT authorize a payout.
//!
//! ## EVM -> Soroban mapping
//! - `ECDSA.recover` (secp256k1) -> `env.crypto().secp256k1_recover(&digest, &sig64, recovery_id)`.
//! - `keccak256` -> `env.crypto().keccak256(&Bytes)`.
//! - `abi.encode`/`abi.encodePacked` -> [`microcrop_shared::encoding`] FROZEN canonical layout.
//! - `authorizedSigner` (20-byte EVM addr) -> stored 65-byte SEC-1 pubkey (`BytesN<65>`),
//!   byte-compared to the recovered key.
//! - `block.chainid` -> configured `DataKey::NetworkDomain` (`BytesN<32>`), see `set_network_domain`.
//! - `address(this)` -> `env.current_contract_address()`.
//! - Cross-contract effects via [`microcrop_shared::interfaces`] clients: `TreasuryClient::request_payout`,
//!   `PolicyManagerClient::mark_as_claimed` + `increment_claim_count`.
//! - `nonReentrant` dropped (Soroban is single-frame re-entrancy safe); CEI ordering preserved.
//!
//! ## submit_determination — validation order (ALL must pass; preserve verbatim)
//!  1. signer configured
//!  2. bounds: `damage_bp <= 10000`, sub-scores `<= 100`, `weather_present in {0,1}`
//!  3. evidence consistency: `weather_present == 0 => weather_damage == 0`
//!  4. weighted invariant (NO division): present -> `60*w + 40*s == bp`; satellite-only -> `100*s == bp`
//!  5. threshold: `bp >= 3000`
//!  6. policy: exists, ACTIVE, not expired, not already paid; `d.sum_insured == policy.sum_insured`;
//!     6c: `policy.coverage_type == Comprehensive` (the only cover a cause-agnostic crop-damage
//!     index settles); 6d: signed `latitude_e6`/`longitude_e6` within `PLOT_COORD_TOLERANCE_E6`
//!     of the policy's plot coordinates
//!  7. payout: `payout == sum_insured * bp / 10000`
//!  8. freshness: `assessed_at` within `MAX_REPORT_AGE`, not in the future
//!  9. farmer within yearly claim limit
//! 10. reconstruct inputs_hash + preimage on-chain (never trust a passed-in hash)
//! 11. replay: preimage not consumed (temporary `ConsumedDetermination` + persistent `PolicyPaid`
//!     + Treasury `payout_processed` = triple guard)
//! 12. signature: `secp256k1_recover(digest, sig, recovery_id) == authorized_signer`
//!
//! Effects before interactions (CEI): mark consumed + paid + store report, THEN
//! `treasury.request_payout`, then `pm.mark_as_claimed` (paid now) or `pm.mark_claim_pending`
//! (org reserve short: Treasury recorded a pending payout), then `pm.increment_claim_count`.
//!
//! ## Pending claims (audit findings 9 and 11)
//! A valid determination is never lost to a short org reserve. Treasury records the full
//! amount as a pending payout (counted in the org's `reserve_required`, so the org cannot
//! withdraw it), the policy becomes CLAIM_PENDING, and the permissionless
//! [`PayoutReceiver::settle_pending_claim`] pays it once the reserve is topped up. Pending
//! claims of one org settle strictly first-in-first-out (Treasury keeps a per-org queue). A
//! failed USDC transfer to the backend wallet (issuer-deauthorized or missing trustline) is
//! recorded the same way instead of reverting the determination.
//!
//! ## One determination per policy (signer-side rule)
//! Two different determinations signed for the same policy are both valid for
//! `MAX_REPORT_AGE`; whichever the relayer submits first settles the policy and the other
//! reverts `PolicyAlreadyPaid`. The contract cannot tell which one the signer "meant" without
//! a dispute window, so the rule lives in the signer: it issues at most ONE determination per
//! policy per assessment window (no new signature while an earlier one for the same policy is
//! unexpired or the policy is paid). See `docs/MIGRATION_NOTES.md`.

use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, panic_with_error, Address,
    BytesN, Env,
};

use microcrop_shared::{
    encoding::determination_digest,
    errors::{CommonError, PayoutError},
    interfaces::{PolicyManagerClient, TreasuryClient},
    roles, ttl,
    types::{CoverageType, CropDetermination, DamageReport, PolicyStatus, Role},
};

// ============ Constants (ported 1:1) ============
/// 30% = 3000 bps.
pub const MIN_DAMAGE_THRESHOLD: u32 = 3_000;
/// 100% = 10000 bps.
pub const MAX_DAMAGE_PERCENTAGE: u32 = 10_000;
pub const WEATHER_WEIGHT: u32 = 60;
pub const SATELLITE_WEIGHT: u32 = 40;
/// 1 hour, in seconds.
pub const MAX_REPORT_AGE: u64 = 3_600;
pub const BASIS_POINTS: i128 = 10_000;
/// Weight denominator (100%); the weighted invariant deliberately does NOT divide by it.
pub const WEIGHT_DENOMINATOR: u32 = 100;
/// Largest allowed difference, per axis, between a signed coordinate and the policy's plot
/// coordinate, in degrees x 1e6. One unit (1e-6 degree, ~0.11 m) absorbs a rounding-mode
/// difference between the backend's and the Lit signer's float-to-integer conversion of the
/// same plot record; it is far below any plot's size, so it cannot move evidence to another
/// farm.
pub const PLOT_COORD_TOLERANCE_E6: i128 = 1;

// ---- Storage TTL (rent cap, `microcrop_shared::ttl`) ----
// Instance + code: every call extends below 3 days to 10 days; `extend_contract_ttl()` (the
// keeper) extends all four contracts below 23 days to 30 days. `PolicyPaid` and `Report` live
// until the policy's `end_date + 30 d`; `ClaimPending` below 30 days to 60 days. The replay
// marker is temporary and only has to outlive the 1-hour freshness window; `PolicyPaid` is
// the durable guard.
const DAY_IN_LEDGERS: u32 = 17_280;
const TEMP_BUMP: u32 = 7 * DAY_IN_LEDGERS;
const TEMP_THRESHOLD: u32 = TEMP_BUMP - DAY_IN_LEDGERS;

/// Local emergency-stop error. Solidity's `PausableUpgradeable` reverts `EnforcedPause`; the
/// shared `PayoutError` enum (300..=317) has no paused variant and the shared crate is frozen,
/// so this in-crate error carries code 318 (contiguous with the PayoutReceiver domain range).
/// NOTE FOR SHARED-CRATE OWNER: consider adding a `Paused` variant to `CommonError` or
/// `PayoutError` so all four contracts share one code.
#[contracterror]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum PausedError {
    ContractPaused = 318,
}

// ============ Events (ported from the Solidity `event` declarations) ============

/// Emitted when the authorized signer (PKP) is (re)configured.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizedSignerUpdated {
    pub signer: BytesN<65>,
}

/// Emitted when a signed determination is verified and consumed (Solidity `DeterminationVerified`).
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeterminationVerified {
    #[topic]
    pub policy_id: u64,
    pub preimage_hash: BytesN<32>,
}

/// Emitted when a valid damage report is processed (Solidity `DamageReportReceived`).
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DamageReportReceived {
    #[topic]
    pub policy_id: u64,
    #[topic]
    pub farmer: Address,
    pub damage_percentage: u32,
    pub payout_amount: i128,
}

/// Emitted when a payout is initiated (Solidity `PayoutInitiated`).
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PayoutInitiated {
    #[topic]
    pub policy_id: u64,
    pub amount: i128,
}

/// A verified claim could not be paid from the org reserve; it is recorded and settles via
/// `settle_pending_claim` once the reserve covers it.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaimAwaitingFunding {
    #[topic]
    pub policy_id: u64,
    pub amount: i128,
}

/// A pending claim was paid in full.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingClaimSettled {
    #[topic]
    pub policy_id: u64,
    pub amount: i128,
}

/// Storage keys. Config + signer + domain + paused are instance; report/paid persistent;
/// the replay guard is temporary (freshness window makes long-lived persistence unnecessary).
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// Treasury contract address.
    Treasury,
    /// PolicyManager contract address.
    PolicyManager,
    /// Authorized PKP signer as a 65-byte SEC-1 uncompressed pubkey.
    AuthorizedSigner,
    /// Network domain separator (replaces `block.chainid`).
    NetworkDomain,
    /// Paused flag.
    Paused,
    /// policy_id -> DamageReport.
    Report(u64),
    /// policy_id -> settled? Set when a determination is accepted, whether the payout went
    /// out at once or was recorded as pending, so no second determination can settle the
    /// policy. (`is_policy_paid` reads this; `is_claim_pending` tells the two apart.)
    PolicyPaid(u64),
    /// policy_id -> verified claim waiting for the org reserve (see `settle_pending_claim`).
    ClaimPending(u64),
    /// reconstructed preimage hash -> consumed? (temporary storage; guards distinct-preimage
    /// replay within the unpaid freshness window).
    ConsumedDetermination(BytesN<32>),
}

#[contract]
pub struct PayoutReceiver;

#[contractimpl]
impl PayoutReceiver {
    /// Port of `initialize(treasury, policyManager, admin)`. Grants ADMIN + UPGRADER, stores
    /// the two contract references, unpaused. NOTE: `authorized_signer` and `network_domain`
    /// must be configured post-deploy via `set_authorized_signer` / `set_network_domain`
    /// before any determination can verify.
    pub fn __constructor(env: Env, treasury: Address, policy_manager: Address, admin: Address) {
        roles::grant_role(&env, Role::Admin, &admin);
        roles::grant_role(&env, Role::Upgrader, &admin);
        let s = env.storage().instance();
        s.set(&DataKey::Treasury, &treasury);
        s.set(&DataKey::PolicyManager, &policy_manager);
        s.set(&DataKey::Paused, &false);
    }

    /// Port of `setAuthorizedSigner` (ADMIN). Authority root for payouts. Stored as the 65-byte
    /// SEC-1 uncompressed pubkey and byte-compared to the `secp256k1_recover` result.
    pub fn set_authorized_signer(env: Env, caller: Address, pubkey: BytesN<65>) {
        roles::require_role(&env, &caller, Role::Admin);
        env.storage()
            .instance()
            .set(&DataKey::AuthorizedSigner, &pubkey);
        AuthorizedSignerUpdated { signer: pubkey }.publish(&env);
    }

    /// Necessary port addition (no EVM equivalent): the domain separator that replaces
    /// `block.chainid` in the signed preimage. Bind the Stellar network passphrase. ADMIN only.
    /// The off-chain Lit PKP signer MUST use the same 32-byte value.
    pub fn set_network_domain(env: Env, caller: Address, domain: BytesN<32>) {
        roles::require_role(&env, &caller, Role::Admin);
        env.storage()
            .instance()
            .set(&DataKey::NetworkDomain, &domain);
    }

    /// Grant a role (ADMIN only) — e.g. `Role::Relayer` to the relayer wallet.
    pub fn grant_role(env: Env, caller: Address, role: Role, who: Address) {
        roles::require_role(&env, &caller, Role::Admin);
        roles::grant_role(&env, role, &who);
    }

    /// Revoke a role (ADMIN only).
    pub fn revoke_role(env: Env, caller: Address, role: Role, who: Address) {
        roles::require_role(&env, &caller, Role::Admin);
        roles::revoke_role(&env, role, &who);
    }

    /// True if `who` holds `role` in this contract. READ-ONLY view, no auth required.
    ///
    /// Role membership is not a secret: every grant is already a public persistent-storage
    /// entry on a public ledger. What was missing was a way to ASK. Without this, a deployment
    /// could not be verified through the interface — a missed cross-contract grant (the ones
    /// that make PolicyManager a Minter, or PayoutReceiver a Payout drawer) produces a
    /// deployment that looks complete and is dead on the money path, and the failure surfaces
    /// on the first real claim rather than at deploy time.
    pub fn has_role(env: Env, role: Role, who: Address) -> bool {
        roles::has_role(&env, role, &who)
    }

    /// Port of the UUPS `_authorizeUpgrade` path (UPGRADER only).
    pub fn upgrade(env: Env, caller: Address, new_wasm_hash: BytesN<32>) {
        roles::require_role(&env, &caller, Role::Upgrader);
        env.deployer().update_current_contract_wasm(new_wasm_hash);
    }

    /// Port of `pause` (ADMIN).
    pub fn pause(env: Env, caller: Address) {
        roles::require_role(&env, &caller, Role::Admin);
        env.storage().instance().set(&DataKey::Paused, &true);
    }

    /// Port of `unpause` (ADMIN).
    pub fn unpause(env: Env, caller: Address) {
        roles::require_role(&env, &caller, Role::Admin);
        env.storage().instance().set(&DataKey::Paused, &false);
    }

    // ---- crux ----

    /// Port of `submitDetermination` (RELAYER, when-not-paused). Runs the 12-step validation,
    /// reconstructs the digest via [`microcrop_shared::encoding::determination_digest`] using
    /// `env.current_contract_address()` + the stored `NetworkDomain`, recovers the signer with
    /// `env.crypto().secp256k1_recover(&digest, &signature, recovery_id)`, byte-compares to the
    /// stored `AuthorizedSigner`, then (CEI) marks consumed/paid, stores the report, and calls
    /// `TreasuryClient::request_payout` + `PolicyManagerClient::mark_as_claimed` +
    /// `increment_claim_count`.
    ///
    /// `signature` is the 64-byte `r‖s`; `recovery_id` is `v - 27` (0 or 1).
    pub fn submit_determination(
        env: Env,
        caller: Address,
        d: CropDetermination,
        signature: BytesN<64>,
        recovery_id: u32,
    ) {
        // RELAYER gate (anti-spam only; not the payout authority) + caller.require_auth().
        roles::require_role(&env, &caller, Role::Relayer);

        // whenNotPaused.
        if env
            .storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
        {
            panic_with_error!(&env, PausedError::ContractPaused);
        }

        let s = env.storage();
        ttl::touch_contract(&env);

        // 1. authority root must be configured.
        let authorized_signer: BytesN<65> = match s.instance().get(&DataKey::AuthorizedSigner) {
            Some(pk) => pk,
            None => panic_with_error!(&env, PayoutError::SignerNotConfigured),
        };

        // 2. bounds.
        if d.damage_percent_bp > MAX_DAMAGE_PERCENTAGE {
            panic_with_error!(&env, PayoutError::DamageExceedsMaximum);
        }
        if d.weather_damage > 100 || d.satellite_damage > 100 {
            panic_with_error!(&env, PayoutError::SubScoreOutOfRange);
        }
        if d.weather_present > 1 {
            panic_with_error!(&env, PayoutError::InvalidWeatherFlag);
        }

        // 3. evidence consistency: satellite-only determinations MUST carry weather_damage == 0,
        //    so the flag cannot be gamed against the renormalized invariant below.
        if d.weather_present == 0 && d.weather_damage != 0 {
            panic_with_error!(&env, PayoutError::WeatherFlagDamageMismatch);
        }

        // 4. weighted-damage invariant (basis points; NO /WEIGHT_DENOMINATOR). Sub-scores are
        //    bounded <= 100 by step 2, so these products cannot overflow u32.
        let expected_bp: u32 = if d.weather_present == 0 {
            d.satellite_damage * WEIGHT_DENOMINATOR // satellite-only: pct -> bp at 100% weight
        } else {
            (WEATHER_WEIGHT * d.weather_damage) + (SATELLITE_WEIGHT * d.satellite_damage)
        };
        if expected_bp != d.damage_percent_bp {
            panic_with_error!(&env, PayoutError::InvalidWeightedDamage);
        }

        // 5. threshold.
        if d.damage_percent_bp < MIN_DAMAGE_THRESHOLD {
            panic_with_error!(&env, PayoutError::DamageBelowThreshold);
        }

        // Cross-contract references (configured in the constructor).
        let policy_manager: Address = s
            .instance()
            .get(&DataKey::PolicyManager)
            .unwrap_or_else(|| panic_with_error!(&env, CommonError::NotConfigured));
        let treasury: Address = s
            .instance()
            .get(&DataKey::Treasury)
            .unwrap_or_else(|| panic_with_error!(&env, CommonError::NotConfigured));
        let pm = PolicyManagerClient::new(&env, &policy_manager);

        // 6. policy state: exists, ACTIVE, not expired, not already paid.
        if !pm.policy_exists(&d.on_chain_policy_id) {
            panic_with_error!(&env, PayoutError::PolicyDoesNotExist);
        }
        let policy = pm.get_policy(&d.on_chain_policy_id);
        if policy.status != PolicyStatus::Active {
            panic_with_error!(&env, PayoutError::PolicyNotActive);
        }
        let now = env.ledger().timestamp();
        if now > policy.end_date {
            panic_with_error!(&env, PayoutError::PolicyExpired);
        }
        if s.persistent()
            .get(&DataKey::PolicyPaid(d.on_chain_policy_id))
            .unwrap_or(false)
        {
            panic_with_error!(&env, PayoutError::PolicyAlreadyPaid);
        }

        // 6b. evidence must bind the real policy's sum insured.
        if d.sum_insured != policy.sum_insured {
            panic_with_error!(&env, PayoutError::SumInsuredMismatch);
        }

        // 6c. the methodology must settle this cover. CROP_DAMAGE measures loss, not its
        //     cause, so it settles only all-cause cover (see `CoverageType`).
        if policy.coverage_type != CoverageType::Comprehensive {
            panic_with_error!(&env, PayoutError::CoverageNotSettledByMethodology);
        }

        // 6d. the evidence must be about the insured plot.
        if (d.latitude_e6 - policy.lat_e6 as i128).abs() > PLOT_COORD_TOLERANCE_E6
            || (d.longitude_e6 - policy.lon_e6 as i128).abs() > PLOT_COORD_TOLERANCE_E6
        {
            panic_with_error!(&env, PayoutError::PlotLocationMismatch);
        }

        // 7. payout re-derived on-chain (basis points; single unit on the money path).
        let expected_payout = policy.sum_insured * (d.damage_percent_bp as i128) / BASIS_POINTS;
        if d.payout_amount != expected_payout {
            panic_with_error!(&env, PayoutError::InvalidPayoutCalculation);
        }

        // 8. freshness: future-dated and stale are distinct failure modes.
        if d.assessed_at > now {
            panic_with_error!(&env, PayoutError::ReportInFuture);
        }
        if now > d.assessed_at.saturating_add(MAX_REPORT_AGE) {
            panic_with_error!(&env, PayoutError::ReportTooOld);
        }

        // 9. farmer claim limit.
        if !pm.can_farmer_claim(&policy.farmer) {
            panic_with_error!(&env, PayoutError::FarmerClaimLimitExceeded);
        }

        // 10. reconstruct the settlement digest ON-CHAIN from raw fields (never trust a
        //     passed-in hash). NetworkDomain + this contract's address are bound in, so a
        //     determination signed for another network/contract cannot verify here.
        let network_domain: BytesN<32> = s
            .instance()
            .get(&DataKey::NetworkDomain)
            .unwrap_or_else(|| panic_with_error!(&env, CommonError::NotConfigured));
        let this = env.current_contract_address();
        let digest = determination_digest(&env, &d, &this, &network_domain);
        let preimage_hash = digest.to_bytes();

        // 11. replay guard (temporary layer of the triple guard).
        if s.temporary()
            .has(&DataKey::ConsumedDetermination(preimage_hash.clone()))
        {
            panic_with_error!(&env, PayoutError::DeterminationAlreadyConsumed);
        }

        // 12. signature is the sole authority: recover the SEC-1 pubkey and byte-compare.
        let recovered = env
            .crypto()
            .secp256k1_recover(&digest, &signature, recovery_id);
        if recovered != authorized_signer {
            panic_with_error!(&env, PayoutError::InvalidSignature);
        }

        // ============ All checks passed — effects before interactions (CEI) ============
        s.temporary().set(
            &DataKey::ConsumedDetermination(preimage_hash.clone()),
            &true,
        );
        s.temporary().extend_ttl(
            &DataKey::ConsumedDetermination(preimage_hash.clone()),
            TEMP_THRESHOLD,
            TEMP_BUMP,
        );

        s.persistent()
            .set(&DataKey::PolicyPaid(d.on_chain_policy_id), &true);
        ttl::extend_for_policy(
            &env,
            &DataKey::PolicyPaid(d.on_chain_policy_id),
            policy.end_date,
        );

        let report = DamageReport {
            policy_id: d.on_chain_policy_id,
            damage_percentage: d.damage_percent_bp,
            weather_damage: d.weather_damage,
            satellite_damage: d.satellite_damage,
            payout_amount: d.payout_amount,
            assessed_at: d.assessed_at,
        };
        s.persistent()
            .set(&DataKey::Report(d.on_chain_policy_id), &report);
        ttl::extend_for_policy(
            &env,
            &DataKey::Report(d.on_chain_policy_id),
            policy.end_date,
        );

        // Interactions. `this` is passed as the acting caller so Treasury/PolicyManager can
        // enforce `caller == payout_receiver` / role checks; a contract auto-authorizes calls
        // it makes under its own address. Treasury.request_payout is the third replay layer
        // (its `PayoutProcessed(policy_id)` guard).
        //
        // A short org reserve no longer reverts the determination (audit findings 9/11):
        // Treasury records a pending payout for the full amount and returns false, the policy
        // moves to CLAIM_PENDING (so it cannot expire, be cancelled, or be settled by another
        // determination), and `settle_pending_claim` pays it once the reserve covers it. The
        // replay guards above are already set, so the determination is consumed either way.
        let paid_now = TreasuryClient::new(&env, &treasury).request_payout(
            &this,
            &d.on_chain_policy_id,
            &d.payout_amount,
        );
        if paid_now {
            pm.mark_as_claimed(&this, &d.on_chain_policy_id);
        } else {
            pm.mark_claim_pending(&this, &d.on_chain_policy_id);
            s.persistent()
                .set(&DataKey::ClaimPending(d.on_chain_policy_id), &true);
            ttl::extend_pending(&env, &DataKey::ClaimPending(d.on_chain_policy_id));
        }
        // The claim counts against the farmer's window when it is approved, paid or not.
        pm.increment_claim_count(&this, &policy.farmer);

        // Events (mirror Solidity DeterminationVerified / DamageReportReceived / PayoutInitiated).
        DeterminationVerified {
            policy_id: d.on_chain_policy_id,
            preimage_hash,
        }
        .publish(&env);
        DamageReportReceived {
            policy_id: d.on_chain_policy_id,
            farmer: policy.farmer,
            damage_percentage: d.damage_percent_bp,
            payout_amount: d.payout_amount,
        }
        .publish(&env);
        if paid_now {
            PayoutInitiated {
                policy_id: d.on_chain_policy_id,
                amount: d.payout_amount,
            }
            .publish(&env);
        } else {
            ClaimAwaitingFunding {
                policy_id: d.on_chain_policy_id,
                amount: d.payout_amount,
            }
            .publish(&env);
        }
    }

    /// Settle a claim that `submit_determination` verified but the org reserve could not pay.
    /// PERMISSIONLESS: anyone (the org, a keeper, the farmer's agent) may call it once the org
    /// reserve covers the claim. Pays the FULL recorded amount through
    /// `Treasury::settle_pending_payout`, then marks the policy CLAIMED exactly as the normal
    /// path does. Not re-verified: the signature, freshness and coverage window were checked
    /// when the claim was recorded, so neither `MAX_REPORT_AGE` nor `end_date` can lapse it.
    ///
    /// Pending claims of one org settle strictly first-in-first-out: only the org's oldest
    /// unsettled claim (`Treasury::pending_payout_head(org)`) is accepted.
    ///
    /// Reverts `NoPendingClaim` if nothing is pending for `policy_id` (never recorded, or
    /// already settled), `ContractPaused` while paused, and Treasury's `NotPendingQueueHead`
    /// (an older claim of the same org is still unsettled), `InsufficientOrgReserve` (the
    /// reserve is still short) or `PayoutTransferFailed` (the backend wallet cannot receive
    /// USDC; rotate it). Every revert leaves the claim recorded.
    pub fn settle_pending_claim(env: Env, policy_id: u64) {
        if env
            .storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
        {
            panic_with_error!(&env, PausedError::ContractPaused);
        }
        let s = env.storage();
        ttl::touch_contract(&env);

        let key = DataKey::ClaimPending(policy_id);
        if !s.persistent().get(&key).unwrap_or(false) {
            panic_with_error!(&env, PayoutError::NoPendingClaim);
        }
        // Effects before interactions: clear the marker first.
        s.persistent().remove(&key);

        let policy_manager: Address = s
            .instance()
            .get(&DataKey::PolicyManager)
            .unwrap_or_else(|| panic_with_error!(&env, CommonError::NotConfigured));
        let treasury: Address = s
            .instance()
            .get(&DataKey::Treasury)
            .unwrap_or_else(|| panic_with_error!(&env, CommonError::NotConfigured));
        let this = env.current_contract_address();

        let amount = TreasuryClient::new(&env, &treasury).settle_pending_payout(&this, &policy_id);
        PolicyManagerClient::new(&env, &policy_manager).mark_as_claimed(&this, &policy_id);

        PendingClaimSettled { policy_id, amount }.publish(&env);
        PayoutInitiated { policy_id, amount }.publish(&env);
    }

    /// PERMISSIONLESS keep-alive for one policy across all four contracts: extends this
    /// contract's entries for `policy_id` to their need (`PolicyPaid` / `Report` to the
    /// policy's `end_date + 30 d`, `ClaimPending` below 30 days to 60 days), then calls
    /// `Treasury::extend_policy_ttl` and `PolicyManager::extend_policy_ttl` (which extends the
    /// PolicyNft certificate, best effort). Uses the keeper targets, never the network
    /// maximum. The backend keeper calls it for every open or pending policy whose shortest
    /// TTL is under 30 days (see `docs/DEPLOYMENT_RUNBOOK.md`). Not paused-gated: keeping state
    /// alive is always safe. Reverts `PolicyDoesNotExist` (PolicyManager) for an unknown
    /// policy.
    pub fn extend_policy_ttl(env: Env, policy_id: u64) {
        ttl::touch_contract(&env);
        let (treasury, policy_manager) = Self::siblings(&env);
        let pm = PolicyManagerClient::new(&env, &policy_manager);
        let policy = pm.get_policy(&policy_id);
        let p = env.storage().persistent();
        for key in [DataKey::PolicyPaid(policy_id), DataKey::Report(policy_id)] {
            if p.has(&key) {
                ttl::extend_for_policy(&env, &key, policy.end_date);
            }
        }
        if p.has(&DataKey::ClaimPending(policy_id)) {
            ttl::extend_pending(&env, &DataKey::ClaimPending(policy_id));
        }
        TreasuryClient::new(&env, &treasury).extend_policy_ttl(&policy_id);
        pm.extend_policy_ttl(&policy_id);
    }

    /// PERMISSIONLESS keeper for all four contracts: extends the instance and code of this
    /// contract and of Treasury below 23 days to 30 days, then calls
    /// `PolicyManager::extend_contract_ttl`, which does the same for PolicyManager and
    /// PolicyNft. A no-op above 23 days, so the backend calls it daily. Ordinary calls extend
    /// only below 3 days to 10 days; this keeper, not user traffic, pays for keeping the code
    /// alive. Not paused-gated.
    pub fn extend_contract_ttl(env: Env) {
        ttl::keep_contract_alive(&env);
        let (treasury, policy_manager) = Self::siblings(&env);
        ttl::keep_other_contract_alive(&env, &treasury);
        PolicyManagerClient::new(&env, &policy_manager).extend_contract_ttl();
    }

    /// The Treasury and PolicyManager addresses (set in the constructor).
    fn siblings(env: &Env) -> (Address, Address) {
        let s = env.storage().instance();
        let treasury: Address = s
            .get(&DataKey::Treasury)
            .unwrap_or_else(|| panic_with_error!(env, CommonError::NotConfigured));
        let policy_manager: Address = s
            .get(&DataKey::PolicyManager)
            .unwrap_or_else(|| panic_with_error!(env, CommonError::NotConfigured));
        (treasury, policy_manager)
    }

    // ---- views ----

    /// True while a verified claim for `policy_id` is waiting for the org reserve.
    pub fn is_claim_pending(env: Env, policy_id: u64) -> bool {
        env.storage()
            .persistent()
            .get(&DataKey::ClaimPending(policy_id))
            .unwrap_or(false)
    }

    /// Port of `getReport`. Returns a zeroed report for an unknown policy (Solidity mapping
    /// default-value semantics).
    pub fn get_report(env: Env, policy_id: u64) -> DamageReport {
        env.storage()
            .persistent()
            .get(&DataKey::Report(policy_id))
            .unwrap_or(DamageReport {
                policy_id: 0,
                damage_percentage: 0,
                weather_damage: 0,
                satellite_damage: 0,
                payout_amount: 0,
                assessed_at: 0,
            })
    }

    /// Port of `isPolicyPaid`.
    pub fn is_policy_paid(env: Env, policy_id: u64) -> bool {
        env.storage()
            .persistent()
            .get(&DataKey::PolicyPaid(policy_id))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod test;
