//! Contract error codes. Solidity custom errors carry rich params; Soroban
//! `#[contracterror]` variants are bare `u32` codes, so any values a Solidity error
//! carried (e.g. `InsufficientOrgReserve(org, required, available)`) are surfaced via
//! an event at the raise site instead. One enum per contract domain, plus a common one
//! for the shared role/auth layer. Code ranges are namespaced per domain to keep them
//! unambiguous in logs.

use soroban_sdk::contracterror;

/// Shared auth/config errors (raised from [`crate::roles`] and constructors). Range 1..=9.
#[contracterror]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum CommonError {
    /// Caller lacks the required role (replaces AccessControl's revert).
    Unauthorized = 1,
    /// A zero / unset address was supplied where a real one is required.
    ZeroAddress = 2,
    /// A required config address (e.g. PolicyManager) has not been wired yet.
    NotConfigured = 3,
    /// A required singleton (admin, counter, config) was missing from storage.
    NotInitialized = 4,
}

/// `PolicyManager` domain errors. Range 100..=199.
#[contracterror]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum PolicyError {
    ZeroAddressFarmer = 100,
    ZeroAddressOrg = 101,
    ZeroAddressDistributor = 102,
    SumInsuredTooLow = 103,
    SumInsuredTooHigh = 104,
    ZeroPremium = 105,
    InvalidDuration = 106,
    TooManyActivePolicies = 107,
    TooManyClaimsThisYear = 108,
    PolicyDoesNotExist = 109,
    InvalidPolicyStatus = 110,
    PolicyExpired = 111,
    PolicyNotYetExpired = 112,
    PolicyNftNotSet = 113,
    OrgNotSetForPolicy = 114,
    OrgAlreadySet = 115,
    /// Only `Role::Admin` may cancel an ACTIVE policy; `Role::Backend` may cancel PENDING
    /// ones only. Stops cover being cancelled after a loss to dodge the payout.
    CancelActiveRequiresAdmin = 116,
    /// `create_policy` with `|lat_e6| > 90_000_000` or `|lon_e6| > 180_000_000`.
    InvalidCoordinates = 117,
    /// `create_policy` with a coverage type other than `Drought`. The only methodology the
    /// signer can sign (CROP_DROUGHT, `crop-drought-rdi-1.0`) indemnifies drought only, so any
    /// other cover could take a premium and never pay. Rejected at sale.
    CoverageNotOffered = 118,
}

// 200..=299 were the `Treasury` domain errors (and 215/216 its crate-local pause codes).
// The Treasury was removed by Option B (SettlementAdapter, 2026-10-19). The range is retired
// with it: never reuse any code in 200..=299, so a log line from a Treasury-era deployment
// can never be misread.

/// `PayoutReceiver` domain errors. Range 300..=399. Order mirrors the validation steps.
#[contracterror]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum PayoutError {
    SignerNotConfigured = 300,
    DamageExceedsMaximum = 301,
    SubScoreOutOfRange = 302,
    InvalidWeatherFlag = 303,
    WeatherFlagDamageMismatch = 304,
    InvalidWeightedDamage = 305,
    DamageBelowThreshold = 306,
    PolicyDoesNotExist = 307,
    PolicyNotActive = 308,
    PolicyExpired = 309,
    PolicyAlreadyPaid = 310,
    SumInsuredMismatch = 311,
    InvalidPayoutCalculation = 312,
    ReportInFuture = 313,
    ReportTooOld = 314,
    FarmerClaimLimitExceeded = 315,
    DeterminationAlreadyConsumed = 316,
    InvalidSignature = 317,
    // 318 is the crate-local `payout_receiver::PausedError::ContractPaused`.
    /// Retired with the Treasury (was `settle_pending_claim` with nothing pending). Kept so
    /// the code stays reserved; never raised, never reuse 319.
    NoPendingClaim = 319,
    /// The policy's coverage type is not one the submitted methodology settles: CROP_DAMAGE
    /// settles `Comprehensive` only, CROP_DROUGHT settles `Drought` only (see `CoverageType`).
    CoverageNotSettledByMethodology = 320,
    /// The signed `latitude_e6` / `longitude_e6` are not the insured plot's coordinates.
    PlotLocationMismatch = 321,
    /// The determination's methodology is not enabled (`set_methodology_enabled`). Absent
    /// means disabled.
    MethodologyDisabled = 322,
    /// `drought_damage > 100`.
    DroughtScoreOutOfRange = 323,
    /// `damage_percent_bp != drought_damage * 100`, or `drought_damage` is not what the
    /// rdi-1.0 index gives for the signed rainfall evidence.
    InvalidDroughtDamage = 324,
    /// The drought evidence is outside the methodology: `lgp_days != 180`, `cdd_w2_days > 40`,
    /// a zero reference rainfall, a rainfall value above 1e9 centi-mm, a season start that is
    /// not 00:00 UTC, or a methodology params hash other than rdi-1.0's.
    DroughtEvidenceInvalid = 325,
    /// The measured season is not inside the cover: planting before the UTC day cover
    /// started, or the W1..W2 window (110 days) ending after `end_date`.
    EvidenceOutsideCoverWindow = 326,
    /// `assessed_at` is before the end of the 110-day measurement window.
    SeasonNotComplete = 327,
    /// `set_settlement_adapter` with an address that is not a SettlementAdapter bound to this
    /// PayoutReceiver (`adapter.payout_receiver() != this`), or with this contract itself.
    SettlementAdapterNotBound = 328,
    /// The SettlementAdapter returned an obligation that is not exactly the one requested
    /// (receiver, policy, org, amount, digest, status `Owed`). Return values are checked,
    /// never trusted.
    ObligationNotRecorded = 329,
}

/// `PolicyNft` domain errors. Range 400..=499.
#[contracterror]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum NftError {
    ZeroAddress = 400,
    InvalidPolicyId = 401,
    PolicyAlreadyMinted = 402,
    TransferWhileActive = 403,
    PolicyNotFound = 404,
    NotPolicyManager = 405,
    TokenNotFound = 406,
}

/// `SettlementAdapter` domain errors. Range 500..=599.
#[contracterror]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum AdapterError {
    /// `record_obligation` from a caller other than the configured PayoutReceiver.
    NotPayoutReceiver = 500,
    /// `record_obligation` with `amount <= 0`.
    InvalidAmount = 501,
    /// (PayoutReceiver, policy) already has an obligation with a different org, amount or
    /// determination digest.
    ObligationConflict = 502,
    /// No obligation with that id.
    ObligationNotFound = 503,
    /// `mark_funded` on a Funded obligation with a different reference.
    ObligationAlreadyFunded = 504,
    /// Any transition out of Settled, except repeating the identical `mark_settled`.
    ObligationAlreadySettled = 505,
    /// An all-zero reference hash.
    InvalidReference = 506,
    /// `set_payout_receiver` with the adapter's own address.
    InvalidPayoutReceiver = 507,
    /// The obligation id or an org's obligation count would overflow (unreachable in
    /// practice; replaces a bare panic).
    CounterOverflow = 508,
}
