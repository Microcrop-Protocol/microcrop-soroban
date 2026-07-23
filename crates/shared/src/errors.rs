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
}

/// `Treasury` domain errors. Range 200..=299.
#[contracterror]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum TreasuryError {
    ZeroAddress = 200,
    ZeroAmount = 201,
    PremiumAlreadyReceived = 202,
    PayoutAlreadyProcessed = 203,
    InsufficientOrgReserve = 204,
    FeeTooHigh = 205,
    InsufficientBalance = 206,
    PremiumNotReceived = 207,
    ExceedsRecoverableSurplus = 208,
    OrgNotResolved = 209,
    PolicyManagerNotSet = 210,
    NotPayoutReceiver = 211,
    BpsTooHigh = 212,
    WouldBreachReserve = 213,
    NoFeesToWithdraw = 214,
}

/// `PayoutReceiver` domain errors. Range 300..=399. Order mirrors the 12 validation steps.
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
