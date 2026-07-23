//! Ported data types. Field order and integer widths are load-bearing:
//! - money (USDC, 6dp) -> `i128` (the token-native amount type)
//! - ids / timestamps  -> `u64`
//! - basis points / percents / flags -> `u32`
//! - geo / signed evidence (Solidity `int256`) -> `i128`
//!
//! Enum ordinals are pinned with explicit discriminants so they match the Solidity
//! `enum` ordinals and any off-chain system that speaks the same wire values.

use soroban_sdk::{contracttype, Address, String};

/// Coverage kinds — ordinals 0..4 preserved from `PolicyManager.CoverageType` /
/// `PolicyNFT.CoverageType`.
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

/// Policy lifecycle states — ordinals 0..4 preserved from `PolicyManager.PolicyStatus`.
#[contracttype]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum PolicyStatus {
    Pending = 0,
    Active = 1,
    Expired = 2,
    Cancelled = 3,
    Claimed = 4,
}

/// Access-control roles. Replaces the `keccak256("..._ROLE")` `bytes32` role ids of
/// OpenZeppelin `AccessControl`. Stored per (role, holder) — see [`crate::roles`].
#[contracttype]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Role {
    /// `DEFAULT_ADMIN_ROLE` + `ADMIN_ROLE` — grants/revokes roles, config, pause.
    Admin = 0,
    /// `BACKEND_ROLE` — create/activate/cancel/expire policies; receive premiums.
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

/// Stored damage report — mirrors `PayoutReceiver.DamageReport`.
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
    /// USDC base units (6dp)
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
