#![no_std]
//! # PayoutReceiver
//!
//! Soroban port of `microcrop-contracts/microcrop/src/PayoutReceiver.sol` (v2.1.0, plus the
//! v2.2.0 drought path). This is the crux contract: it verifies a PKP-signed determination
//! and, iff valid, triggers the payout. The **signature is the sole payout authority**;
//! `Role::Relayer` is an anti-spam gate on WHO may relay and does NOT authorize a payout.
//!
//! Two methodologies, each behind an admin switch (`set_methodology_enabled`; absent means
//! disabled):
//! - CROP_DROUGHT, `crop-drought-rdi-1.0` — [`PayoutReceiver::submit_drought_determination`].
//!   The live methodology; settles `Drought` cover only. Its 17 checks are listed on the
//!   function.
//! - CROP_DAMAGE, `crop-dualindex-1.0` — [`PayoutReceiver::submit_determination`]. Retired
//!   (the oracle no longer signs it); settles `Comprehensive` cover only. Leave it disabled.
//!
//! ## EVM -> Soroban mapping
//! - `ECDSA.recover` (secp256k1) -> `env.crypto().secp256k1_recover(&digest, &sig64, recovery_id)`.
//! - `keccak256` -> `env.crypto().keccak256(&Bytes)`.
//! - `abi.encode`/`abi.encodePacked` -> [`microcrop_shared::encoding`] FROZEN canonical layouts.
//! - `authorizedSigner` (20-byte EVM addr) -> stored 65-byte SEC-1 pubkey (`BytesN<65>`),
//!   byte-compared to the recovered key.
//! - `block.chainid` -> configured `DataKey::NetworkDomain` (`BytesN<32>`), see `set_network_domain`.
//! - `address(this)` -> `env.current_contract_address()`.
//! - Cross-contract effects via [`microcrop_shared::interfaces`] clients: `TreasuryClient::request_payout`,
//!   `PolicyManagerClient::mark_as_claimed` + `increment_claim_count`.
//! - `nonReentrant` dropped (Soroban is single-frame re-entrancy safe); CEI ordering preserved.
//!
//! ## submit_determination (CROP_DAMAGE) — validation order (ALL must pass)
//!  1. signer configured; methodology `CropDualIndex10` enabled (322)
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
//! Both paths share their effects (`settle_verified`) and their replay guards: `PolicyPaid`
//! is one flag per policy, so a policy settles at most once across both methodologies.
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
    contract, contracterror, contractevent, contractimpl, contracttype, crypto::Hash,
    panic_with_error, Address, BytesN, Env,
};

use microcrop_shared::{
    drought,
    encoding::{determination_digest, drought_determination_digest, DROUGHT_PARAMS_HASH_RDI_1_0},
    errors::{CommonError, PayoutError},
    interfaces::{PolicyManagerClient, TreasuryClient},
    roles, ttl,
    types::{
        CoverageType, CropDetermination, DamageReport, DroughtDetermination, MethodologyKind,
        Policy, PolicyStatus, Role,
    },
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

/// The rainfall evidence behind a verified CROP_DROUGHT determination.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DroughtEvidenceRecorded {
    #[topic]
    pub policy_id: u64,
    pub drought_damage: u32,
    pub season_start_epoch: u64,
    pub source_hash: BytesN<32>,
}

/// A methodology was enabled or disabled.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MethodologyEnabledSet {
    #[topic]
    pub kind: MethodologyKind,
    pub enabled: bool,
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
    /// methodology -> accepted? Instance storage; absent means disabled.
    MethodologyEnabled(MethodologyKind),
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

    // ---- methodology allow-list ----

    /// Enable or disable a determination methodology (ADMIN). Absent means disabled, so a
    /// fresh deployment or an upgrade accepts nothing until the admin enables a methodology.
    /// CROP_DAMAGE (`CropDualIndex10`) is retired: leave it disabled. A claim already recorded
    /// as pending still settles, because `settle_pending_claim` does not re-verify.
    pub fn set_methodology_enabled(
        env: Env,
        caller: Address,
        kind: MethodologyKind,
        enabled: bool,
    ) {
        roles::require_role(&env, &caller, Role::Admin);
        env.storage()
            .instance()
            .set(&DataKey::MethodologyEnabled(kind), &enabled);
        MethodologyEnabledSet { kind, enabled }.publish(&env);
    }

    /// Whether `kind` is enabled.
    pub fn is_methodology_enabled(env: Env, kind: MethodologyKind) -> bool {
        Self::methodology_enabled(&env, kind)
    }

    // ---- crux ----

    /// Port of `submitDetermination` (RELAYER, when-not-paused) for the retired CROP_DAMAGE
    /// methodology (`crop-dualindex-1.0`). Runs the validation in the module docs, rebuilds
    /// the digest via [`microcrop_shared::encoding::determination_digest`] using
    /// `env.current_contract_address()` + the stored `NetworkDomain`, recovers the signer with
    /// `env.crypto().secp256k1_recover(&digest, &signature, recovery_id)`, byte-compares it to
    /// the stored `AuthorizedSigner`, then settles exactly as the drought path does.
    ///
    /// Requires `MethodologyEnabled(CropDualIndex10)` (`MethodologyDisabled`, 322, otherwise).
    /// It is disabled after the drought upgrade, and no new `Comprehensive` policy can be sold.
    ///
    /// `signature` is the 64-byte `r‖s`; `recovery_id` is `v - 27` (0 or 1).
    pub fn submit_determination(
        env: Env,
        caller: Address,
        d: CropDetermination,
        signature: BytesN<64>,
        recovery_id: u32,
    ) {
        let authorized_signer =
            Self::begin_submission(&env, &caller, MethodologyKind::CropDualIndex10);

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

        // 6. policy state: exists, ACTIVE, not expired, not already paid.
        let (pm, treasury) = Self::clients(&env);
        let policy = Self::open_policy(&env, &pm, d.on_chain_policy_id);

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
        Self::require_plot(&env, &policy, d.latitude_e6, d.longitude_e6);

        // 7. payout re-derived on-chain (basis points; single unit on the money path).
        Self::require_payout(&env, &policy, d.damage_percent_bp, d.payout_amount);

        // 8. freshness + 9. farmer claim limit.
        Self::require_fresh_and_claimable(&env, &pm, &policy, d.assessed_at);

        // 10. reconstruct the settlement digest ON-CHAIN from raw fields (never trust a
        //     passed-in hash). NetworkDomain + this contract's address are bound in, so a
        //     determination signed for another network/contract cannot verify here.
        let digest = determination_digest(
            &env,
            &d,
            &env.current_contract_address(),
            &Self::network_domain(&env),
        );

        // 11. replay guard + 12. signature (the sole authority).
        Self::require_unconsumed_and_signed(
            &env,
            &digest,
            &signature,
            recovery_id,
            &authorized_signer,
        );

        let report = DamageReport {
            policy_id: d.on_chain_policy_id,
            damage_percentage: d.damage_percent_bp,
            weather_damage: d.weather_damage,
            satellite_damage: d.satellite_damage,
            payout_amount: d.payout_amount,
            assessed_at: d.assessed_at,
        };
        Self::settle_verified(&env, &pm, &treasury, &policy, report, digest.to_bytes());
    }

    /// Verify a PKP-signed CROP_DROUGHT determination (`crop-drought-rdi-1.0`) and, iff
    /// valid, pay it (RELAYER, when-not-paused). The signature is the sole payout authority;
    /// it is over [`microcrop_shared::encoding::drought_determination_digest`], which binds
    /// the on-chain policy id, the USDC amounts, this contract and the network domain.
    ///
    /// Checks, in order (all must pass):
    ///  1. signer configured (300); 2. `CropDroughtRdi10` enabled (322);
    ///  3. bounds: `bp <= 10000` (301), `drought_damage <= 100` (323), evidence within the
    ///     methodology (325: `lgp_days == 180`, `cdd_w2_days <= 40`, non-zero reference rain,
    ///     every rain value <= 1e9, season start at 00:00 UTC, the rdi-1.0 params hash);
    ///  4. `bp == drought_damage * 100` (324); 5. the index recomputed from the signed rain
    ///     equals `drought_damage` (324); 6. `bp >= 3000` (306);
    ///  7. policy exists (307), ACTIVE (308), `now <= end_date` (309), not paid (310);
    ///  8. `coverage_type == Drought` (320); 9. `sum_insured` matches (311);
    ///  10. coordinates within 1e-6 degree (321);
    ///  11. evidence window: planting on or after the UTC day cover started and the 110-day
    ///      window inside cover (326); `assessed_at` after the window (327);
    ///  12. `payout == sum_insured * bp / 10000` (312); 13. `assessed_at <= now` (313) and
    ///      `now <= assessed_at + 3600` (314); 14. farmer claim limit (315);
    ///  15. digest rebuilt on-chain; 16. not consumed (316); 17. signer recovered (317).
    ///
    /// Effects and interactions are those of `submit_determination`; `PolicyPaid` is shared,
    /// so a policy settles at most once across both methodologies. The stored report has
    /// `weather_damage = satellite_damage = 0`.
    pub fn submit_drought_determination(
        env: Env,
        caller: Address,
        d: DroughtDetermination,
        signature: BytesN<64>,
        recovery_id: u32,
    ) {
        let authorized_signer =
            Self::begin_submission(&env, &caller, MethodologyKind::CropDroughtRdi10);

        // 3. bounds.
        if d.damage_percent_bp > MAX_DAMAGE_PERCENTAGE {
            panic_with_error!(&env, PayoutError::DamageExceedsMaximum);
        }
        if d.drought_damage > 100 {
            panic_with_error!(&env, PayoutError::DroughtScoreOutOfRange);
        }
        if d.lgp_days != drought::LGP_DAYS
            || d.cdd_w2_days > drought::MAX_CDD_W2_DAYS
            || d.rain_ref_w1_e2 == 0
            || d.rain_ref_w2_e2 == 0
            || d.rain_w1_e2 > drought::MAX_RAIN_E2
            || d.rain_ref_w1_e2 > drought::MAX_RAIN_E2
            || d.rain_w2_e2 > drought::MAX_RAIN_E2
            || d.rain_ref_w2_e2 > drought::MAX_RAIN_E2
            || !d
                .season_start_epoch
                .is_multiple_of(drought::SECONDS_PER_DAY)
            || d.methodology_params_hash.to_array() != DROUGHT_PARAMS_HASH_RDI_1_0
        {
            panic_with_error!(&env, PayoutError::DroughtEvidenceInvalid);
        }

        // 4. unit invariant: rdi-1.0 emits whole percent; the money path is basis points.
        if d.damage_percent_bp != d.drought_damage * 100 {
            panic_with_error!(&env, PayoutError::InvalidDroughtDamage);
        }

        // 5. the result follows from the signed evidence.
        match drought::rdi_v1_drought_damage(
            d.rain_w1_e2,
            d.rain_ref_w1_e2,
            d.rain_w2_e2,
            d.rain_ref_w2_e2,
        ) {
            Some(dd) if dd == d.drought_damage => {}
            _ => panic_with_error!(&env, PayoutError::InvalidDroughtDamage),
        }

        // 6. threshold.
        if d.damage_percent_bp < MIN_DAMAGE_THRESHOLD {
            panic_with_error!(&env, PayoutError::DamageBelowThreshold);
        }

        // 7. policy: exists, ACTIVE, not expired, not already paid.
        let (pm, treasury) = Self::clients(&env);
        let policy = Self::open_policy(&env, &pm, d.on_chain_policy_id);

        // 8. drought cover only (the index indemnifies drought and nothing else).
        if policy.coverage_type != CoverageType::Drought {
            panic_with_error!(&env, PayoutError::CoverageNotSettledByMethodology);
        }

        // 9. the evidence binds the sold sum insured (USDC, 7dp).
        if d.sum_insured != policy.sum_insured {
            panic_with_error!(&env, PayoutError::SumInsuredMismatch);
        }

        // 10. the evidence is about the insured plot.
        Self::require_plot(&env, &policy, d.latitude_e6, d.longitude_e6);

        // 11. the measured season lies inside the cover. Planting must not precede the UTC day
        //     cover started (`activate_policy` resets `start_date` to now), so rainfall already
        //     observed at purchase cannot be insured; and the whole W1..W2 window (110 days)
        //     must end by `end_date`. Then the season must be over when it was assessed.
        let start_day = policy.start_date - policy.start_date % drought::SECONDS_PER_DAY;
        let window_end = d
            .season_start_epoch
            .checked_add(drought::SEASON_WINDOW_SECS)
            .unwrap_or_else(|| panic_with_error!(&env, PayoutError::EvidenceOutsideCoverWindow));
        if d.season_start_epoch < start_day || window_end > policy.end_date {
            panic_with_error!(&env, PayoutError::EvidenceOutsideCoverWindow);
        }
        if d.assessed_at < window_end {
            panic_with_error!(&env, PayoutError::SeasonNotComplete);
        }

        // 12. payout re-derived on-chain (floor; never overpays a base unit).
        Self::require_payout(&env, &policy, d.damage_percent_bp, d.payout_amount);

        // 13. freshness + 14. farmer claim limit.
        Self::require_fresh_and_claimable(&env, &pm, &policy, d.assessed_at);

        // 15. rebuild the digest on-chain from the stored domain and this contract's address.
        let digest = drought_determination_digest(
            &env,
            &d,
            &env.current_contract_address(),
            &Self::network_domain(&env),
        );

        // 16. replay guard + 17. signature (the sole authority).
        Self::require_unconsumed_and_signed(
            &env,
            &digest,
            &signature,
            recovery_id,
            &authorized_signer,
        );

        let report = DamageReport {
            policy_id: d.on_chain_policy_id,
            damage_percentage: d.damage_percent_bp,
            weather_damage: 0,
            satellite_damage: 0,
            payout_amount: d.payout_amount,
            assessed_at: d.assessed_at,
        };
        DroughtEvidenceRecorded {
            policy_id: d.on_chain_policy_id,
            drought_damage: d.drought_damage,
            season_start_epoch: d.season_start_epoch,
            source_hash: d.source_hash.clone(),
        }
        .publish(&env);
        Self::settle_verified(&env, &pm, &treasury, &policy, report, digest.to_bytes());
    }

    /// The digest `submit_drought_determination` would verify for `d` on this contract and
    /// network. Read-only: the oracle and the backend simulate it against the live contract
    /// to confirm their encoder. Reverts `NotConfigured` until the network domain is set.
    pub fn drought_determination_digest(env: Env, d: DroughtDetermination) -> BytesN<32> {
        drought_determination_digest(
            &env,
            &d,
            &env.current_contract_address(),
            &Self::network_domain(&env),
        )
        .to_bytes()
    }

    // ---- shared verification steps (private) ----

    /// Steps 0-2 of both paths: relayer role + auth, not paused, user-path TTL, signer
    /// configured, methodology enabled. Returns the authorized signer.
    fn begin_submission(env: &Env, caller: &Address, kind: MethodologyKind) -> BytesN<65> {
        // RELAYER gate (anti-spam only; not the payout authority) + caller.require_auth().
        roles::require_role(env, caller, Role::Relayer);
        // whenNotPaused.
        if env
            .storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
        {
            panic_with_error!(env, PausedError::ContractPaused);
        }
        ttl::touch_contract(env);
        // 1. authority root must be configured.
        let signer: BytesN<65> = match env.storage().instance().get(&DataKey::AuthorizedSigner) {
            Some(pk) => pk,
            None => panic_with_error!(env, PayoutError::SignerNotConfigured),
        };
        // 2. the methodology must be enabled.
        if !Self::methodology_enabled(env, kind) {
            panic_with_error!(env, PayoutError::MethodologyDisabled);
        }
        signer
    }

    fn methodology_enabled(env: &Env, kind: MethodologyKind) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::MethodologyEnabled(kind))
            .unwrap_or(false)
    }

    fn clients(env: &Env) -> (PolicyManagerClient<'_>, TreasuryClient<'_>) {
        let (treasury, policy_manager) = Self::siblings(env);
        (
            PolicyManagerClient::new(env, &policy_manager),
            TreasuryClient::new(env, &treasury),
        )
    }

    fn network_domain(env: &Env) -> BytesN<32> {
        env.storage()
            .instance()
            .get(&DataKey::NetworkDomain)
            .unwrap_or_else(|| panic_with_error!(env, CommonError::NotConfigured))
    }

    /// The policy exists (307), is ACTIVE (308), within its cover (309) and unpaid (310).
    fn open_policy(env: &Env, pm: &PolicyManagerClient, policy_id: u64) -> Policy {
        if !pm.policy_exists(&policy_id) {
            panic_with_error!(env, PayoutError::PolicyDoesNotExist);
        }
        let policy = pm.get_policy(&policy_id);
        if policy.status != PolicyStatus::Active {
            panic_with_error!(env, PayoutError::PolicyNotActive);
        }
        if env.ledger().timestamp() > policy.end_date {
            panic_with_error!(env, PayoutError::PolicyExpired);
        }
        if env
            .storage()
            .persistent()
            .get(&DataKey::PolicyPaid(policy_id))
            .unwrap_or(false)
        {
            panic_with_error!(env, PayoutError::PolicyAlreadyPaid);
        }
        policy
    }

    /// The signed coordinates are the insured plot's, within one micro-degree per axis (321).
    fn require_plot(env: &Env, policy: &Policy, lat_e6: i128, lon_e6: i128) {
        if (lat_e6 - policy.lat_e6 as i128).abs() > PLOT_COORD_TOLERANCE_E6
            || (lon_e6 - policy.lon_e6 as i128).abs() > PLOT_COORD_TOLERANCE_E6
        {
            panic_with_error!(env, PayoutError::PlotLocationMismatch);
        }
    }

    /// `payout == sum_insured * bp / 10000`, floor division (312).
    fn require_payout(env: &Env, policy: &Policy, bp: u32, payout: i128) {
        if payout != policy.sum_insured * (bp as i128) / BASIS_POINTS {
            panic_with_error!(env, PayoutError::InvalidPayoutCalculation);
        }
    }

    /// Freshness (313 future, 314 stale) and the farmer's claim limit (315).
    fn require_fresh_and_claimable(
        env: &Env,
        pm: &PolicyManagerClient,
        policy: &Policy,
        assessed_at: u64,
    ) {
        let now = env.ledger().timestamp();
        if assessed_at > now {
            panic_with_error!(env, PayoutError::ReportInFuture);
        }
        if now > assessed_at.saturating_add(MAX_REPORT_AGE) {
            panic_with_error!(env, PayoutError::ReportTooOld);
        }
        if !pm.can_farmer_claim(&policy.farmer) {
            panic_with_error!(env, PayoutError::FarmerClaimLimitExceeded);
        }
    }

    /// The digest was not consumed (316) and recovers to the authorized signer (317).
    fn require_unconsumed_and_signed(
        env: &Env,
        digest: &Hash<32>,
        signature: &BytesN<64>,
        recovery_id: u32,
        authorized_signer: &BytesN<65>,
    ) {
        if env
            .storage()
            .temporary()
            .has(&DataKey::ConsumedDetermination(digest.to_bytes()))
        {
            panic_with_error!(env, PayoutError::DeterminationAlreadyConsumed);
        }
        let recovered = env
            .crypto()
            .secp256k1_recover(digest, signature, recovery_id);
        if recovered != *authorized_signer {
            panic_with_error!(env, PayoutError::InvalidSignature);
        }
    }

    /// Effects then interactions for a verified determination, identical for both
    /// methodologies: consumed marker, `PolicyPaid`, `Report`; `Treasury::request_payout`;
    /// `mark_as_claimed`, or on a short reserve `mark_claim_pending` + `ClaimPending`;
    /// `increment_claim_count`; events.
    fn settle_verified(
        env: &Env,
        pm: &PolicyManagerClient,
        treasury: &TreasuryClient,
        policy: &Policy,
        report: DamageReport,
        preimage_hash: BytesN<32>,
    ) {
        let s = env.storage();
        let policy_id = report.policy_id;
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

        s.persistent().set(&DataKey::PolicyPaid(policy_id), &true);
        ttl::extend_for_policy(env, &DataKey::PolicyPaid(policy_id), policy.end_date);

        s.persistent().set(&DataKey::Report(policy_id), &report);
        ttl::extend_for_policy(env, &DataKey::Report(policy_id), policy.end_date);

        // Interactions. `this` is passed as the acting caller so Treasury/PolicyManager can
        // enforce `caller == payout_receiver` / role checks; a contract auto-authorizes calls
        // it makes under its own address. Treasury.request_payout is the third replay layer
        // (its `PayoutProcessed(policy_id)` guard).
        //
        // A short org reserve does not revert the determination (audit findings 9/11):
        // Treasury records a pending payout for the full amount and returns false, the policy
        // moves to CLAIM_PENDING (so it cannot expire, be cancelled, or be settled by another
        // determination), and `settle_pending_claim` pays it once the reserve covers it. The
        // replay guards above are already set, so the determination is consumed either way.
        let this = env.current_contract_address();
        let paid_now = treasury.request_payout(&this, &policy_id, &report.payout_amount);
        if paid_now {
            pm.mark_as_claimed(&this, &policy_id);
        } else {
            pm.mark_claim_pending(&this, &policy_id);
            s.persistent().set(&DataKey::ClaimPending(policy_id), &true);
            ttl::extend_pending(env, &DataKey::ClaimPending(policy_id));
        }
        // The claim counts against the farmer's window when it is approved, paid or not.
        pm.increment_claim_count(&this, &policy.farmer);

        // Events (mirror Solidity DeterminationVerified / DamageReportReceived / PayoutInitiated).
        DeterminationVerified {
            policy_id,
            preimage_hash,
        }
        .publish(env);
        DamageReportReceived {
            policy_id,
            farmer: policy.farmer.clone(),
            damage_percentage: report.damage_percentage,
            payout_amount: report.payout_amount,
        }
        .publish(env);
        if paid_now {
            PayoutInitiated {
                policy_id,
                amount: report.payout_amount,
            }
            .publish(env);
        } else {
            ClaimAwaitingFunding {
                policy_id,
                amount: report.payout_amount,
            }
            .publish(env);
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
#[cfg(test)]
mod test_drought;
