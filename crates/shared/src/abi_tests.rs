//! ABI pin: every error code number and every enum ordinal that leaves the contracts (logs,
//! events, the backend's decoders). security.md checklist: "error codes never renumbered".
//! A failure here means a wire value changed; that is a breaking change, not a refactor.

use crate::errors::{AdapterError, CommonError, NftError, PayoutError, PolicyError};
use crate::types::{CoverageType, MethodologyKind, ObligationStatus, PolicyStatus, Role};

#[test]
fn common_error_codes() {
    assert_eq!(CommonError::Unauthorized as u32, 1);
    assert_eq!(CommonError::ZeroAddress as u32, 2);
    assert_eq!(CommonError::NotConfigured as u32, 3);
    assert_eq!(CommonError::NotInitialized as u32, 4);
}

#[test]
fn policy_error_codes() {
    let codes = [
        (PolicyError::ZeroAddressFarmer, 100),
        (PolicyError::ZeroAddressOrg, 101),
        (PolicyError::ZeroAddressDistributor, 102),
        (PolicyError::SumInsuredTooLow, 103),
        (PolicyError::SumInsuredTooHigh, 104),
        (PolicyError::ZeroPremium, 105),
        (PolicyError::InvalidDuration, 106),
        (PolicyError::TooManyActivePolicies, 107),
        (PolicyError::TooManyClaimsThisYear, 108),
        (PolicyError::PolicyDoesNotExist, 109),
        (PolicyError::InvalidPolicyStatus, 110),
        (PolicyError::PolicyExpired, 111),
        (PolicyError::PolicyNotYetExpired, 112),
        (PolicyError::PolicyNftNotSet, 113),
        (PolicyError::OrgNotSetForPolicy, 114),
        (PolicyError::OrgAlreadySet, 115),
        (PolicyError::CancelActiveRequiresAdmin, 116),
        (PolicyError::InvalidCoordinates, 117),
        (PolicyError::CoverageNotOffered, 118),
    ];
    for (e, n) in codes {
        assert_eq!(e as u32, n, "{:?}", e);
    }
}

#[test]
fn payout_error_codes() {
    let codes = [
        (PayoutError::SignerNotConfigured, 300),
        (PayoutError::DamageExceedsMaximum, 301),
        (PayoutError::SubScoreOutOfRange, 302),
        (PayoutError::InvalidWeatherFlag, 303),
        (PayoutError::WeatherFlagDamageMismatch, 304),
        (PayoutError::InvalidWeightedDamage, 305),
        (PayoutError::DamageBelowThreshold, 306),
        (PayoutError::PolicyDoesNotExist, 307),
        (PayoutError::PolicyNotActive, 308),
        (PayoutError::PolicyExpired, 309),
        (PayoutError::PolicyAlreadyPaid, 310),
        (PayoutError::SumInsuredMismatch, 311),
        (PayoutError::InvalidPayoutCalculation, 312),
        (PayoutError::ReportInFuture, 313),
        (PayoutError::ReportTooOld, 314),
        (PayoutError::FarmerClaimLimitExceeded, 315),
        (PayoutError::DeterminationAlreadyConsumed, 316),
        (PayoutError::InvalidSignature, 317),
        // 318 is payout_receiver::PausedError::ContractPaused (pinned in that crate).
        (PayoutError::NoPendingClaim, 319), // retired, reserved
        (PayoutError::CoverageNotSettledByMethodology, 320),
        (PayoutError::PlotLocationMismatch, 321),
        (PayoutError::MethodologyDisabled, 322),
        (PayoutError::DroughtScoreOutOfRange, 323),
        (PayoutError::InvalidDroughtDamage, 324),
        (PayoutError::DroughtEvidenceInvalid, 325),
        (PayoutError::EvidenceOutsideCoverWindow, 326),
        (PayoutError::SeasonNotComplete, 327),
        (PayoutError::SettlementAdapterNotBound, 328),
        (PayoutError::ObligationNotRecorded, 329),
    ];
    for (e, n) in codes {
        assert_eq!(e as u32, n, "{:?}", e);
    }
}

#[test]
fn nft_error_codes() {
    let codes = [
        (NftError::ZeroAddress, 400),
        (NftError::InvalidPolicyId, 401),
        (NftError::PolicyAlreadyMinted, 402),
        (NftError::TransferWhileActive, 403),
        (NftError::PolicyNotFound, 404),
        (NftError::NotPolicyManager, 405),
        (NftError::TokenNotFound, 406),
    ];
    for (e, n) in codes {
        assert_eq!(e as u32, n, "{:?}", e);
    }
}

#[test]
fn adapter_error_codes() {
    let codes = [
        (AdapterError::NotPayoutReceiver, 500),
        (AdapterError::InvalidAmount, 501),
        (AdapterError::ObligationConflict, 502),
        (AdapterError::ObligationNotFound, 503),
        (AdapterError::ObligationAlreadyFunded, 504),
        (AdapterError::ObligationAlreadySettled, 505),
        (AdapterError::InvalidReference, 506),
        (AdapterError::InvalidPayoutReceiver, 507),
    ];
    for (e, n) in codes {
        assert_eq!(e as u32, n, "{:?}", e);
    }
}

#[test]
fn enum_ordinals() {
    assert_eq!(Role::Admin as u32, 0);
    assert_eq!(Role::Backend as u32, 1);
    assert_eq!(Role::Oracle as u32, 2);
    assert_eq!(Role::Payout as u32, 3); // retired, reserved
    assert_eq!(Role::Relayer as u32, 4);
    assert_eq!(Role::Upgrader as u32, 5);
    assert_eq!(Role::Minter as u32, 6);
    assert_eq!(Role::Funder as u32, 7);

    assert_eq!(ObligationStatus::Owed as u32, 0);
    assert_eq!(ObligationStatus::Funded as u32, 1);
    assert_eq!(ObligationStatus::Settled as u32, 2);

    assert_eq!(PolicyStatus::Pending as u32, 0);
    assert_eq!(PolicyStatus::Active as u32, 1);
    assert_eq!(PolicyStatus::Expired as u32, 2);
    assert_eq!(PolicyStatus::Cancelled as u32, 3);
    assert_eq!(PolicyStatus::Claimed as u32, 4);
    // 5 retired (ClaimPending); never reuse.

    assert_eq!(CoverageType::Drought as u32, 0);
    assert_eq!(CoverageType::Flood as u32, 1);
    assert_eq!(CoverageType::Both as u32, 2);
    assert_eq!(CoverageType::ExcessRain as u32, 3);
    assert_eq!(CoverageType::Comprehensive as u32, 4);

    assert_eq!(MethodologyKind::CropDualIndex10 as u32, 0);
    assert_eq!(MethodologyKind::CropDroughtRdi10 as u32, 1);
}
