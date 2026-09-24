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
//!  6. policy: exists, ACTIVE, not expired, not already paid; `d.sum_insured == policy.sum_insured`
//!  7. payout: `payout == sum_insured * bp / 10000`
//!  8. freshness: `assessed_at` within `MAX_REPORT_AGE`, not in the future
//!  9. farmer within yearly claim limit
//! 10. reconstruct inputs_hash + preimage on-chain (never trust a passed-in hash)
//! 11. replay: preimage not consumed (temporary `ConsumedDetermination` + persistent `PolicyPaid`
//!     + Treasury `payout_processed` = triple guard)
//! 12. signature: `secp256k1_recover(digest, sig, recovery_id) == authorized_signer`
//!
//! Effects before interactions (CEI): mark consumed + paid + store report, THEN
//! `treasury.request_payout`, `pm.mark_as_claimed`, `pm.increment_claim_count`.

use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, panic_with_error, Address,
    BytesN, Env,
};

use microcrop_shared::{
    encoding::determination_digest,
    errors::{CommonError, PayoutError},
    interfaces::{PolicyManagerClient, TreasuryClient},
    roles,
    types::{CropDetermination, DamageReport, PolicyStatus, Role},
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

// ---- Storage TTL bump parameters (5s ledgers) ----
const DAY_IN_LEDGERS: u32 = 17_280;
const INSTANCE_BUMP: u32 = 30 * DAY_IN_LEDGERS;
const INSTANCE_THRESHOLD: u32 = INSTANCE_BUMP - DAY_IN_LEDGERS;
const PERSIST_BUMP: u32 = 30 * DAY_IN_LEDGERS;
const PERSIST_THRESHOLD: u32 = PERSIST_BUMP - DAY_IN_LEDGERS;
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
    /// policy_id -> paid?
    PolicyPaid(u64),
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
        s.instance().extend_ttl(INSTANCE_THRESHOLD, INSTANCE_BUMP);

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
        s.temporary()
            .set(&DataKey::ConsumedDetermination(preimage_hash.clone()), &true);
        s.temporary().extend_ttl(
            &DataKey::ConsumedDetermination(preimage_hash.clone()),
            TEMP_THRESHOLD,
            TEMP_BUMP,
        );

        s.persistent()
            .set(&DataKey::PolicyPaid(d.on_chain_policy_id), &true);
        s.persistent().extend_ttl(
            &DataKey::PolicyPaid(d.on_chain_policy_id),
            PERSIST_THRESHOLD,
            PERSIST_BUMP,
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
        s.persistent().extend_ttl(
            &DataKey::Report(d.on_chain_policy_id),
            PERSIST_THRESHOLD,
            PERSIST_BUMP,
        );

        // Interactions. `this` is passed as the acting caller so Treasury/PolicyManager can
        // enforce `caller == payout_receiver` / role checks; a contract auto-authorizes calls
        // it makes under its own address. Treasury.request_payout is the third replay layer
        // (its `PayoutProcessed(policy_id)` guard).
        TreasuryClient::new(&env, &treasury).request_payout(
            &this,
            &d.on_chain_policy_id,
            &d.payout_amount,
        );
        pm.mark_as_claimed(&this, &d.on_chain_policy_id);
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
        PayoutInitiated {
            policy_id: d.on_chain_policy_id,
            amount: d.payout_amount,
        }
        .publish(&env);
    }

    // ---- views ----

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
