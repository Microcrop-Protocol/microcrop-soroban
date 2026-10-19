//! Ported data types. Field order and integer widths are load-bearing:
//! - money (USDC, **7dp** — the Stellar Asset Contract's `decimals()`; 1 USDC = 10_000_000)
//!   -> `i128` (the token-native amount type). The EVM contracts used 6dp; do not copy
//!   their literals.
//! - ids / timestamps  -> `u64`
//! - basis points / percents / flags -> `u32`
//! - geo / signed evidence (Solidity `int256`) -> `i128`
//!
//! Enum ordinals are pinned with explicit discriminants so they match the Solidity
//! `enum` ordinals and any off-chain system that speaks the same wire values.

use soroban_sdk::{contracttype, Address, BytesN, String};

/// Coverage kinds — ordinals 0..4 preserved from `PolicyManager.CoverageType` /
/// `PolicyNFT.CoverageType`.
///
/// Settlement mapping. The only methodology the authorized signer can sign today is
/// CROP_DROUGHT (`crop-drought-rdi-1.0`), a rainfall-deficit index that indemnifies drought
/// and nothing else, so it settles `Drought` cover only. `create_policy` sells `Drought` only
/// (`CoverageNotOffered` otherwise) and `submit_drought_determination` rejects every other
/// type (`CoverageNotSettledByMethodology`).
///
/// The retired CROP_DAMAGE methodology (`crop-dualindex-1.0`) is cause-agnostic and settles
/// `Comprehensive` only; it is disabled by default (`MethodologyKind::CropDualIndex10`), and
/// no new `Comprehensive` policy can be sold. Re-opening `Comprehensive` needs an audited
/// upgrade once a cause-agnostic methodology is live again. `Flood`, `Both` and `ExcessRain`
/// are settled by no methodology.
#[contracttype]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum CoverageType {
    Drought = 0,
    Flood = 1,
    Both = 2,
    ExcessRain = 3,
    Comprehensive = 4,
}

/// Policy lifecycle states — ordinals 0..4 preserved from `PolicyManager.PolicyStatus`;
/// `ClaimPending` (5) is Soroban-only.
#[contracttype]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum PolicyStatus {
    Pending = 0,
    Active = 1,
    Expired = 2,
    Cancelled = 3,
    Claimed = 4,
    /// A valid determination was verified but the org reserve could not pay it in full. The
    /// claim is recorded in Treasury and settles via `PayoutReceiver::settle_pending_claim`
    /// once the reserve covers it. The policy cannot expire, be cancelled, or be settled by
    /// another determination while in this state; it moves to `Claimed` on settlement.
    ClaimPending = 5,
}

/// Access-control roles. Replaces the `keccak256("..._ROLE")` `bytes32` role ids of
/// OpenZeppelin `AccessControl`. Stored per (role, holder) — see [`crate::roles`].
#[contracttype]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Role {
    /// `DEFAULT_ADMIN_ROLE` + `ADMIN_ROLE` — grants/revokes roles, config, pause, and the
    /// escape hatch of cancelling an ACTIVE policy.
    Admin = 0,
    /// `BACKEND_ROLE` — create/activate policies, cancel PENDING ones; receive premiums.
    /// (Cancelling ACTIVE cover is Admin-only; expiry is permissionless.)
    Backend = 1,
    /// `ORACLE_ROLE` — mark claimed / increment claim count (PayoutReceiver only).
    Oracle = 2,
    /// `PAYOUT_ROLE` — request payouts (PayoutReceiver only).
    Payout = 3,
    /// `RELAYER_ROLE` — anti-spam gate on who may submit a determination.
    Relayer = 4,
    /// `UPGRADER_ROLE` — authorise WASM upgrades.
    Upgrader = 5,
    /// `MINTER_ROLE` — mint policy certificate NFTs (PolicyManager only).
    Minter = 6,
}

/// Insurance policy — mirrors `PolicyManager.Policy`, plus the v3 backing `org`
/// (which in Solidity lives in the separate `_policyOrg` mapping).
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    pub id: u64,
    pub farmer: Address,
    pub plot_id: u64,
    /// Insured plot latitude, degrees x 1e6 (signed). Set by the backend at `create_policy`;
    /// a determination's signed `latitude_e6` must match it (PayoutReceiver step 6d).
    pub lat_e6: i64,
    /// Insured plot longitude, degrees x 1e6 (signed). See `lat_e6`.
    pub lon_e6: i64,
    pub sum_insured: i128,
    pub premium: i128,
    pub start_date: u64,
    pub end_date: u64,
    pub coverage_type: CoverageType,
    pub status: PolicyStatus,
    pub created_at: u64,
    /// v3 backing org (the org's wallet address whose reserve funds any payout).
    pub org: Address,
}

/// Stored damage report — mirrors `PayoutReceiver.DamageReport`. Unchanged so existing
/// `Report` entries still decode; a drought report stores `weather_damage = 0` and
/// `satellite_damage = 0`, as on EVM.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DamageReport {
    pub policy_id: u64,
    /// Total damage in basis points (0..10000).
    pub damage_percentage: u32,
    /// Weather sub-score in whole percent (0..100).
    pub weather_damage: u32,
    /// Satellite sub-score in whole percent (0..100).
    pub satellite_damage: u32,
    pub payout_amount: i128,
    pub assessed_at: u64,
}

/// CROP_DAMAGE signed-determination input — mirrors `PayoutReceiver.CropDetermination`.
/// The contract reconstructs BOTH hashes from these raw values and never trusts a
/// passed-in hash. Units are FROZEN (see the Solidity NatSpec and [`crate::encoding`]).
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CropDetermination {
    // --- settlement preimage (result / subject) ---
    pub on_chain_policy_id: u64,
    /// basis points 0..10000
    pub damage_percent_bp: u32,
    /// whole percent 0..100
    pub weather_damage: u32,
    /// whole percent 0..100
    pub satellite_damage: u32,
    /// USDC base units (7dp, Stellar SAC: 1 USDC = 10_000_000)
    pub payout_amount: i128,
    /// unix seconds
    pub assessed_at: u64,
    // --- evidence (inputsHash) ---
    /// degrees x 1e6, signed
    pub latitude_e6: i128,
    /// degrees x 1e6, signed
    pub longitude_e6: i128,
    /// must equal policy.sum_insured
    pub sum_insured: i128,
    /// NDVI x 1e4, signed
    pub ndvi_scaled: i128,
    /// 0 = satellite-only, 1 = weather present
    pub weather_present: u32,
    /// deg C x 1e2, signed
    pub weather_temp_c_e2: i128,
    /// precipitation x 1e2, unsigned
    pub weather_precip_e2: u128,
    /// humidity, unsigned
    pub weather_humidity: u128,
    /// wind x 1e2, unsigned
    pub weather_wind_e2: u128,
}

/// Determination methodologies the PayoutReceiver can verify. Each one's methodology string is
/// a FROZEN constant inside its signed preimage, not a parameter; adding one is an audited
/// upgrade. Which are accepted is an admin switch (`set_methodology_enabled`); absent means
/// disabled.
#[contracttype]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum MethodologyKind {
    /// CROP_DAMAGE, `crop-dualindex-1.0` (`submit_determination`). Retired: leave disabled.
    CropDualIndex10 = 0,
    /// CROP_DROUGHT, `crop-drought-rdi-1.0` (`submit_drought_determination`).
    CropDroughtRdi10 = 1,
}

/// CROP_DROUGHT signed determination (methodology `crop-drought-rdi-1.0`). Field order is
/// the encoding order ([`crate::encoding::encode_drought_inputs`] /
/// [`crate::encoding::encode_drought_preimage`]); units are FROZEN. Mirrors
/// `PayoutReceiver.sol` v2.2.0 `DroughtDetermination`, except that `sum_insured` and
/// `payout_amount` are 7-decimal Stellar USDC.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DroughtDetermination {
    // --- settlement preimage (result / subject) ---
    /// The Soroban PolicyManager policy id (not the backend's database id).
    pub on_chain_policy_id: u64,
    /// Basis points 0..=10000 — the ONLY money-path unit. Must equal `drought_damage * 100`.
    pub damage_percent_bp: u32,
    /// Whole percent 0..=100 (the rdi-1.0 index output).
    pub drought_damage: u32,
    /// USDC base units, 7dp. Must equal `sum_insured * damage_percent_bp / 10000`.
    pub payout_amount: i128,
    /// Unix seconds, chosen once by the orchestrator.
    pub assessed_at: u64,
    // --- evidence (inputs hash, 13 words including the policy id) ---
    /// Degrees x 1e6, signed.
    pub latitude_e6: i128,
    /// Degrees x 1e6, signed.
    pub longitude_e6: i128,
    /// USDC 7dp; must equal `policy.sum_insured`.
    pub sum_insured: i128,
    /// Planting civil date at 00:00 UTC (`plantDays * 86400`).
    pub season_start_epoch: u64,
    /// Length of growing period; must be 180.
    pub lgp_days: u32,
    /// Rainfall in centi-mm over DAP 1..40 of the insured season.
    pub rain_w1_e2: u128,
    /// The pixel's 1991-2020 median of W1 rainfall.
    pub rain_ref_w1_e2: u128,
    /// Rainfall in centi-mm over DAP 71..110 of the insured season.
    pub rain_w2_e2: u128,
    /// The pixel's 1991-2020 median of W2 rainfall.
    pub rain_ref_w2_e2: u128,
    /// Longest dry spell inside W2, days (evidence only, weight 0), 0..=40.
    pub cdd_w2_days: u32,
    /// keccak256 of the canonical archive projection.
    pub source_hash: BytesN<32>,
    /// keccak256 of `DROUGHT_PARAMS_JSON`; must equal
    /// [`crate::encoding::DROUGHT_PARAMS_HASH_RDI_1_0`].
    pub methodology_params_hash: BytesN<32>,
}

/// On-chain policy certificate — mirrors `PolicyNFT.PolicyCertificate`. The Solidity
/// on-chain SVG/JSON `tokenURI` is intentionally dropped in the port (rendered off-chain);
/// this struct is the lightweight registry record.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyCertificate {
    pub policy_id: u64,
    pub farmer: Address,
    pub distributor: Address,
    pub distributor_name: String,
    pub sum_insured: i128,
    pub premium: i128,
    pub start_date: u64,
    pub end_date: u64,
    pub coverage_type: CoverageType,
    pub region: String,
    pub plot_id: u64,
    pub is_active: bool,
}
