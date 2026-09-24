#![no_std]
//! # Treasury
//!
//! Soroban port of `microcrop-contracts/microcrop/src/Treasury.sol` (v3.1.0: per-org
//! treasury + audit batch 1-2).
//!
//! ## EVM -> Soroban mapping
//! - `IERC20 usdc` + SafeERC20 -> `soroban_sdk::token::TokenClient` (USDC is a Stellar Asset
//!   Contract; the Treasury holds USDC as a SAC contract balance, no trustline).
//! - `receivePremium` `safeTransferFrom(msg.sender,..)` -> `token.transfer(&payer,
//!   &current_contract, &amount)` (the payer's `require_auth` propagates to the SAC sub-call).
//! - `requestPayout`/`withdraw*` `safeTransfer(..)` -> `token.transfer(&current_contract, &to,
//!   &amount)` (a contract auto-authorizes moves of its own balance).
//! - `Pausable` -> instance `DataKey::Paused` + a `when_not_paused` guard.
//! - per-org reserve mappings -> persistent `DataKey::OrgReserve/OrgReserveRatioBps/...`.
//! - `withdrawOrgSurplus` uses `org.require_auth()` — the org's own wallet key is the authority
//!   (exact replacement for Solidity's `msg.sender == org`).
//!
//! ## Solvency invariant to preserve
//! `usdc.balance(this) >= total_org_reserves + accumulated_fees` at all times.
//!
//! ## Notes on faithful-but-idiomatic divergences from the Solidity
//! - Solidity `address(0)` guards (`ZeroAddress`, `OrgNotResolved`) have no Soroban analogue:
//!   `Address` is always a valid host object and the callee `PolicyManager` raises its own error
//!   when a policy has no org. Those checks are therefore omitted (documented, not silently
//!   dropped).
//! - `Pausable`'s `EnforcedPause` / `ExpectedPause` reverts map to a crate-local `PausedError`
//!   (the shared `TreasuryError` enum carries no paused code — flagged in the port report).

use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, panic_with_error,
    token::TokenClient, Address, BytesN, Env,
};

use microcrop_shared::{errors::TreasuryError, interfaces::PolicyManagerClient, roles, types::Role};

// ============ Constants (ported 1:1) ============
pub const MIN_RESERVE_PERCENT: u32 = 20;
pub const TARGET_RESERVE_PERCENT: u32 = 30;
pub const MAX_PLATFORM_FEE_PERCENT: u32 = 20;
/// Legacy percent denominator (100).
pub const BASIS_POINTS: i128 = 100;
/// True bps denominator (10000) for per-org reserve/fee math.
pub const BPS_DENOMINATOR: i128 = 10_000;
pub const DEFAULT_RESERVE_RATIO_BPS: u32 = 2_000;
pub const MAX_RESERVE_RATIO_BPS: u32 = 10_000;
pub const MAX_FEE_BPS: u32 = 3_000;
/// Default global platform fee percent set at construction (10%).
pub const DEFAULT_PLATFORM_FEE_PERCENT: u32 = 10;

// ============ TTL bump parameters ============
// Soroban rents storage; entries expire unless their TTL is extended. We bump on every
// state-changing entrypoint so live config + per-org reserves never lapse under regular use.
const DAY_IN_LEDGERS: u32 = 17_280; // ~5s ledgers => ~1 day
const INSTANCE_BUMP_AMOUNT: u32 = 30 * DAY_IN_LEDGERS;
const INSTANCE_LIFETIME_THRESHOLD: u32 = INSTANCE_BUMP_AMOUNT - DAY_IN_LEDGERS;
const PERSISTENT_BUMP_AMOUNT: u32 = 90 * DAY_IN_LEDGERS;
const PERSISTENT_LIFETIME_THRESHOLD: u32 = PERSISTENT_BUMP_AMOUNT - DAY_IN_LEDGERS;

/// Crate-local paused guard errors. The shared `TreasuryError` enum (range 200..=214) has no
/// paused code; `Pausable`'s two reverts map here. Codes 215/216 stay inside the Treasury
/// domain range (200..=299) so logs remain unambiguous.
#[contracterror]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum PausedError {
    /// `whenNotPaused` violated (EnforcedPause).
    Paused = 215,
    /// `whenPaused` violated (ExpectedPause).
    NotPaused = 216,
}

// ============ Events (ported 1:1 from the Solidity `event` declarations) ============
// SDK 27 idiom: `#[contractevent]` structs published via `.publish(&env)`. `#[topic]` fields are
// indexed (the Solidity `indexed` params); the rest form the data map.

/// A premium was received (net of the platform fee).
#[contractevent]
#[derive(Clone)]
pub struct PremiumReceived {
    #[topic]
    pub policy_id: u64,
    #[topic]
    pub from: Address,
    pub gross_amount: i128,
    pub platform_fee: i128,
    pub net_amount: i128,
}

/// An org's reserve was credited from a premium.
#[contractevent]
#[derive(Clone)]
pub struct OrgReserveCredited {
    #[topic]
    pub org: Address,
    #[topic]
    pub policy_id: u64,
    pub net_amount: i128,
}

/// A payout was sent to the backend wallet.
#[contractevent]
#[derive(Clone)]
pub struct PayoutSent {
    #[topic]
    pub policy_id: u64,
    #[topic]
    pub recipient: Address,
    pub amount: i128,
}

/// An org's reserve was debited for a payout.
#[contractevent]
#[derive(Clone)]
pub struct OrgReserveDebited {
    #[topic]
    pub org: Address,
    #[topic]
    pub policy_id: u64,
    pub amount: i128,
}

/// Non-reverting solvency alert: after a payout an org's reserve fell below its requirement.
#[contractevent]
#[derive(Clone)]
pub struct ReserveBelowRequirement {
    #[topic]
    pub org: Address,
    pub remaining: i128,
    pub required: i128,
}

/// Reserve capital was deposited into an org's reserve.
#[contractevent]
#[derive(Clone)]
pub struct OrgReserveDeposited {
    #[topic]
    pub org: Address,
    #[topic]
    pub from: Address,
    pub amount: i128,
}

/// An org withdrew surplus reserve.
#[contractevent]
#[derive(Clone)]
pub struct OrgSurplusWithdrawn {
    #[topic]
    pub org: Address,
    #[topic]
    pub to: Address,
    pub amount: i128,
}

/// The platform admin set an org's reserve ratio.
#[contractevent]
#[derive(Clone)]
pub struct OrgReserveRatioSet {
    #[topic]
    pub org: Address,
    pub ratio_bps: u32,
}

/// The platform admin set an org's fee rate.
#[contractevent]
#[derive(Clone)]
pub struct OrgFeeBpsSet {
    #[topic]
    pub org: Address,
    pub fee_bps: u32,
}

/// The global platform fee percent was updated.
#[contractevent]
#[derive(Clone)]
pub struct PlatformFeeUpdated {
    pub old_fee: u32,
    pub new_fee: u32,
}

/// Accumulated platform fees were withdrawn.
#[contractevent]
#[derive(Clone)]
pub struct FeesWithdrawn {
    #[topic]
    pub recipient: Address,
    pub amount: i128,
}

/// An emergency (unbacked-surplus) withdrawal was executed while paused.
#[contractevent]
#[derive(Clone)]
pub struct EmergencyWithdrawal {
    #[topic]
    pub recipient: Address,
    pub amount: i128,
}

/// The backend payout wallet was updated.
#[contractevent]
#[derive(Clone)]
pub struct BackendWalletUpdated {
    #[topic]
    pub old_wallet: Address,
    #[topic]
    pub new_wallet: Address,
}

/// The PolicyManager reference was set.
#[contractevent]
#[derive(Clone)]
pub struct PolicyManagerSet {
    #[topic]
    pub policy_manager: Address,
}

/// The authorized PayoutReceiver reference was set.
#[contractevent]
#[derive(Clone)]
pub struct PayoutReceiverSet {
    #[topic]
    pub payout_receiver: Address,
}

/// Storage keys. Config + global scalars are instance; per-policy/per-org maps are persistent.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    // ----- instance (config + global scalars) -----
    /// USDC Stellar Asset Contract address.
    Usdc,
    /// Backend wallet that receives payouts for M-Pesa offramp.
    BackendWallet,
    /// PolicyManager (resolves a policy's org + org outstanding exposure).
    PolicyManager,
    /// Authorized PayoutReceiver (defense-in-depth on `request_payout`).
    PayoutReceiver,
    /// Global platform fee percent (fallback when an org fee is unset).
    PlatformFeePercent,
    /// Lifetime net premiums.
    TotalPremiums,
    /// Lifetime payouts.
    TotalPayouts,
    /// Accumulated platform fees available for withdrawal.
    AccumulatedFees,
    /// Running sum of all org reserves.
    TotalOrgReserves,
    /// Paused flag.
    Paused,
    // ----- persistent (per-policy / per-org) -----
    /// policy_id -> premium received?
    PremiumReceived(u64),
    /// policy_id -> payout processed?
    PayoutProcessed(u64),
    /// org -> reserve balance (USDC 6dp).
    OrgReserve(Address),
    /// org -> reserve ratio in true bps.
    OrgReserveRatioBps(Address),
    /// org -> reserve-ratio presence flag (honor explicit 0).
    OrgRatioSet(Address),
    /// org -> platform fee in true bps.
    OrgFeeBps(Address),
    /// org -> fee presence flag (honor explicit 0).
    OrgFeeSet(Address),
}

#[contract]
pub struct Treasury;

#[contractimpl]
impl Treasury {
    /// Port of `initialize(usdc, backendWallet, admin)`. Grants ADMIN + UPGRADER, stores config,
    /// sets default platform fee (10%), zeroes global scalars, unpaused.
    pub fn __constructor(env: Env, usdc: Address, backend_wallet: Address, admin: Address) {
        roles::grant_role(&env, Role::Admin, &admin);
        roles::grant_role(&env, Role::Upgrader, &admin);
        let s = env.storage().instance();
        s.set(&DataKey::Usdc, &usdc);
        s.set(&DataKey::BackendWallet, &backend_wallet);
        s.set(&DataKey::PlatformFeePercent, &DEFAULT_PLATFORM_FEE_PERCENT);
        s.set(&DataKey::TotalPremiums, &0i128);
        s.set(&DataKey::TotalPayouts, &0i128);
        s.set(&DataKey::AccumulatedFees, &0i128);
        s.set(&DataKey::TotalOrgReserves, &0i128);
        s.set(&DataKey::Paused, &false);
    }

    // ---- admin wiring ----

    /// Port of `setPolicyManager` (ADMIN).
    pub fn set_policy_manager(env: Env, caller: Address, policy_manager: Address) {
        roles::require_role(&env, &caller, Role::Admin);
        env.storage()
            .instance()
            .set(&DataKey::PolicyManager, &policy_manager);
        PolicyManagerSet { policy_manager }.publish(&env);
    }

    /// Port of `setPayoutReceiver` (ADMIN). Once set, `request_payout` also requires
    /// `caller == payout_receiver`.
    pub fn set_payout_receiver(env: Env, caller: Address, payout_receiver: Address) {
        roles::require_role(&env, &caller, Role::Admin);
        env.storage()
            .instance()
            .set(&DataKey::PayoutReceiver, &payout_receiver);
        PayoutReceiverSet { payout_receiver }.publish(&env);
    }

    /// Port of `setBackendWallet` (ADMIN).
    pub fn set_backend_wallet(env: Env, caller: Address, backend_wallet: Address) {
        roles::require_role(&env, &caller, Role::Admin);
        let old: Address = env
            .storage()
            .instance()
            .get(&DataKey::BackendWallet)
            .unwrap();
        env.storage()
            .instance()
            .set(&DataKey::BackendWallet, &backend_wallet);
        BackendWalletUpdated {
            old_wallet: old,
            new_wallet: backend_wallet,
        }
        .publish(&env);
    }

    /// Grant a role (ADMIN only).
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

    // ---- domain logic ----

    /// Port of `receivePremium` (BACKEND, when-not-paused). Splits per-org fee, credits
    /// `org_reserve` + `accumulated_fees` + `total_premiums`, pulls USDC from `caller`.
    pub fn receive_premium(env: Env, caller: Address, policy_id: u64, amount: i128) {
        roles::require_role(&env, &caller, Role::Backend);
        require_not_paused(&env);
        instance_bump(&env);

        if amount == 0 {
            panic_with_error!(&env, TreasuryError::ZeroAmount);
        }
        if is_premium_received(&env, policy_id) {
            panic_with_error!(&env, TreasuryError::PremiumAlreadyReceived);
        }

        // Resolve the backing org and split the per-org platform fee.
        let org = resolve_org(&env, policy_id);
        let platform_fee = amount * fee_bps(&env, &org) / BPS_DENOMINATOR;
        let net_premium = amount - platform_fee;

        // Mark received BEFORE the external transfer (CEI).
        set_premium_received(&env, policy_id);
        set_scalar(&env, &DataKey::AccumulatedFees, get_accumulated_fees(&env) + platform_fee);
        set_scalar(&env, &DataKey::TotalPremiums, get_total_premiums(&env) + net_premium);
        set_org_reserve(&env, &org, org_reserve_of(&env, &org) + net_premium);
        set_scalar(&env, &DataKey::TotalOrgReserves, get_total_org_reserves(&env) + net_premium);

        // Pull USDC from the caller (payer's require_auth propagates to the SAC sub-call).
        usdc(&env).transfer(&caller, &env.current_contract_address(), &amount);

        PremiumReceived {
            policy_id,
            from: caller,
            gross_amount: amount,
            platform_fee,
            net_amount: net_premium,
        }
        .publish(&env);
        OrgReserveCredited {
            org,
            policy_id,
            net_amount: net_premium,
        }
        .publish(&env);
    }

    /// Port of `requestPayout` (PAYOUT + caller == payout_receiver, when-not-paused). Debits
    /// ONLY the policy's org reserve (revert `InsufficientOrgReserve` if short); transfers to
    /// backend wallet; emits a solvency signal if remaining < required.
    pub fn request_payout(env: Env, caller: Address, policy_id: u64, amount: i128) {
        roles::require_role(&env, &caller, Role::Payout);
        require_not_paused(&env);
        instance_bump(&env);

        if amount == 0 {
            panic_with_error!(&env, TreasuryError::ZeroAmount);
        }
        // Defense-in-depth: when the PayoutReceiver is wired, restrict callers to it.
        if let Some(pr) = env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::PayoutReceiver)
        {
            if caller != pr {
                panic_with_error!(&env, TreasuryError::NotPayoutReceiver);
            }
        }
        if is_payout_processed(&env, policy_id) {
            panic_with_error!(&env, TreasuryError::PayoutAlreadyProcessed);
        }
        // Never pay out a policy whose premium was never collected.
        if !is_premium_received(&env, policy_id) {
            panic_with_error!(&env, TreasuryError::PremiumNotReceived);
        }

        // Per-org solvency: the payout is funded ONLY by the policy's org's own reserve.
        let org = resolve_org(&env, policy_id);
        let available = org_reserve_of(&env, &org);
        if available < amount {
            // LOUD: the farmer is owed money and the backend must alert the org + admin.
            panic_with_error!(&env, TreasuryError::InsufficientOrgReserve);
        }

        // Update state BEFORE the external transfer (CEI).
        set_payout_processed(&env, policy_id);
        set_scalar(&env, &DataKey::TotalPayouts, get_total_payouts(&env) + amount);
        let remaining = available - amount;
        set_org_reserve(&env, &org, remaining);
        set_scalar(&env, &DataKey::TotalOrgReserves, get_total_org_reserves(&env) - amount);

        // Transfer USDC to backend wallet (M-Pesa offramp). A contract auto-authorizes its own
        // balance moves.
        let backend: Address = env
            .storage()
            .instance()
            .get(&DataKey::BackendWallet)
            .unwrap();
        usdc(&env).transfer(&env.current_contract_address(), &backend, &amount);

        PayoutSent {
            policy_id,
            recipient: backend,
            amount,
        }
        .publish(&env);
        OrgReserveDebited {
            org: org.clone(),
            policy_id,
            amount,
        }
        .publish(&env);

        // Non-reverting solvency signal.
        let required = Self::reserve_required(env.clone(), org.clone());
        if remaining < required {
            ReserveBelowRequirement {
                org,
                remaining,
                required,
            }
            .publish(&env);
        }
    }

    /// Port of `depositReserve` (`from.require_auth()`, when-not-paused). Credits an org's
    /// reserve; pulls USDC from `from`.
    pub fn deposit_reserve(env: Env, from: Address, org: Address, amount: i128) {
        from.require_auth();
        require_not_paused(&env);
        instance_bump(&env);

        if amount == 0 {
            panic_with_error!(&env, TreasuryError::ZeroAmount);
        }
        set_org_reserve(&env, &org, org_reserve_of(&env, &org) + amount);
        set_scalar(&env, &DataKey::TotalOrgReserves, get_total_org_reserves(&env) + amount);
        usdc(&env).transfer(&from, &env.current_contract_address(), &amount);
        OrgReserveDeposited { org, from, amount }.publish(&env);
    }

    /// Port of `withdrawOrgSurplus` (`org.require_auth()`, when-not-paused). Surplus only —
    /// reverts `WouldBreachReserve` below the org's required reserve.
    pub fn withdraw_org_surplus(env: Env, org: Address, amount: i128, to: Address) {
        // The org's own wallet is the per-org key AND the authority (== Solidity msg.sender == org).
        org.require_auth();
        require_not_paused(&env);
        instance_bump(&env);

        if amount == 0 {
            panic_with_error!(&env, TreasuryError::ZeroAmount);
        }
        let available = org_reserve_of(&env, &org);
        if available < amount {
            panic_with_error!(&env, TreasuryError::InsufficientOrgReserve);
        }
        let remaining = available - amount;
        let required = Self::reserve_required(env.clone(), org.clone());
        if remaining < required {
            panic_with_error!(&env, TreasuryError::WouldBreachReserve);
        }

        set_org_reserve(&env, &org, remaining);
        set_scalar(&env, &DataKey::TotalOrgReserves, get_total_org_reserves(&env) - amount);
        usdc(&env).transfer(&env.current_contract_address(), &to, &amount);
        OrgSurplusWithdrawn { org, to, amount }.publish(&env);
    }

    /// Port of `setOrgReserveRatioBps` (ADMIN). <= `MAX_RESERVE_RATIO_BPS`; sets presence flag.
    pub fn set_org_reserve_ratio_bps(env: Env, caller: Address, org: Address, bps: u32) {
        roles::require_role(&env, &caller, Role::Admin);
        instance_bump(&env);
        if bps > MAX_RESERVE_RATIO_BPS {
            panic_with_error!(&env, TreasuryError::BpsTooHigh);
        }
        set_persistent_u32(&env, DataKey::OrgReserveRatioBps(org.clone()), bps);
        set_persistent_bool(&env, DataKey::OrgRatioSet(org.clone()), true);
        OrgReserveRatioSet { org, ratio_bps: bps }.publish(&env);
    }

    /// Port of `setOrgFeeBps` (ADMIN). <= `MAX_FEE_BPS`; sets presence flag.
    pub fn set_org_fee_bps(env: Env, caller: Address, org: Address, bps: u32) {
        roles::require_role(&env, &caller, Role::Admin);
        instance_bump(&env);
        if bps > MAX_FEE_BPS {
            panic_with_error!(&env, TreasuryError::BpsTooHigh);
        }
        set_persistent_u32(&env, DataKey::OrgFeeBps(org.clone()), bps);
        set_persistent_bool(&env, DataKey::OrgFeeSet(org.clone()), true);
        OrgFeeBpsSet { org, fee_bps: bps }.publish(&env);
    }

    /// Port of `setPlatformFee` (ADMIN). <= `MAX_PLATFORM_FEE_PERCENT`.
    pub fn set_platform_fee(env: Env, caller: Address, percent: u32) {
        roles::require_role(&env, &caller, Role::Admin);
        instance_bump(&env);
        if percent > MAX_PLATFORM_FEE_PERCENT {
            panic_with_error!(&env, TreasuryError::FeeTooHigh);
        }
        let old: u32 = env
            .storage()
            .instance()
            .get(&DataKey::PlatformFeePercent)
            .unwrap_or(DEFAULT_PLATFORM_FEE_PERCENT);
        env.storage()
            .instance()
            .set(&DataKey::PlatformFeePercent, &percent);
        PlatformFeeUpdated {
            old_fee: old,
            new_fee: percent,
        }
        .publish(&env);
    }

    /// Port of `withdrawFees` (ADMIN). Transfers exactly `accumulated_fees` (never org reserves).
    pub fn withdraw_fees(env: Env, caller: Address, recipient: Address) {
        roles::require_role(&env, &caller, Role::Admin);
        instance_bump(&env);
        let fees = get_accumulated_fees(&env);
        if fees == 0 {
            panic_with_error!(&env, TreasuryError::NoFeesToWithdraw);
        }
        // Invariant balance == sum(orgReserve) + accumulatedFees means withdrawing exactly the
        // fees never touches an org's reserve.
        set_scalar(&env, &DataKey::AccumulatedFees, 0);
        usdc(&env).transfer(&env.current_contract_address(), &recipient, &fees);
        FeesWithdrawn {
            recipient,
            amount: fees,
        }
        .publish(&env);
    }

    /// Port of `emergencyWithdraw` (ADMIN, when-paused). Only UNBACKED surplus
    /// (`balance - total_org_reserves - accumulated_fees`).
    pub fn emergency_withdraw(env: Env, caller: Address, recipient: Address, amount: i128) {
        roles::require_role(&env, &caller, Role::Admin);
        require_paused(&env);
        instance_bump(&env);

        // Only UNBACKED surplus may be recovered — never funds that back org reserves or fees.
        let balance = usdc(&env).balance(&env.current_contract_address());
        let backed = get_total_org_reserves(&env) + get_accumulated_fees(&env);
        let recoverable = if balance > backed { balance - backed } else { 0 };
        if amount > recoverable {
            panic_with_error!(&env, TreasuryError::ExceedsRecoverableSurplus);
        }
        usdc(&env).transfer(&env.current_contract_address(), &recipient, &amount);
        EmergencyWithdrawal { recipient, amount }.publish(&env);
    }

    // ---- views ----

    /// Port of `reserveRequired` = `outstanding * ratio_bps / 10000`.
    pub fn reserve_required(env: Env, org: Address) -> i128 {
        let outstanding = PolicyManagerClient::new(&env, &require_policy_manager(&env))
            .org_outstanding_sum_insured(&org);
        outstanding * reserve_ratio_bps(&env, &org) / BPS_DENOMINATOR
    }

    /// Port of `getBalance` (USDC balance of this contract).
    pub fn get_balance(env: Env) -> i128 {
        usdc(&env).balance(&env.current_contract_address())
    }

    /// Port of `getAvailableForPayouts(org)` — the org's reserve balance.
    pub fn get_available_for_payouts(env: Env, org: Address) -> i128 {
        org_reserve_of(&env, &org)
    }

    /// Port of `meetsReserveRequirements(org)`.
    pub fn meets_reserve_requirements(env: Env, org: Address) -> bool {
        org_reserve_of(&env, &org) >= Self::reserve_required(env.clone(), org)
    }

    /// Port of `orgReserve(org)`.
    pub fn org_reserve(env: Env, org: Address) -> i128 {
        org_reserve_of(&env, &org)
    }

    /// Port of `totalOrgReserves`.
    pub fn total_org_reserves(env: Env) -> i128 {
        get_total_org_reserves(&env)
    }

    /// Port of `accumulatedFees`.
    pub fn accumulated_fees(env: Env) -> i128 {
        get_accumulated_fees(&env)
    }

    /// Port of `isPremiumReceived(policyId)`.
    pub fn is_premium_received(env: Env, policy_id: u64) -> bool {
        is_premium_received(&env, policy_id)
    }

    /// Port of `isPayoutProcessed(policyId)`.
    pub fn is_payout_processed(env: Env, policy_id: u64) -> bool {
        is_payout_processed(&env, policy_id)
    }

    /// Port of `calculatePlatformFee(premium, org)` = `premium * fee_bps(org) / 10000`.
    pub fn calculate_platform_fee(env: Env, premium: i128, org: Address) -> i128 {
        premium * fee_bps(&env, &org) / BPS_DENOMINATOR
    }

    /// Port of `getTotalPremiums`.
    pub fn get_total_premiums(env: Env) -> i128 {
        get_total_premiums(&env)
    }

    /// Port of `getTotalPayouts`.
    pub fn get_total_payouts(env: Env) -> i128 {
        get_total_payouts(&env)
    }
}

// ============ Free helpers (not exported as contract methods) ============

/// USDC token client bound to the stored SAC address.
fn usdc(env: &Env) -> TokenClient<'_> {
    let addr: Address = env.storage().instance().get(&DataKey::Usdc).unwrap();
    TokenClient::new(env, &addr)
}

fn instance_bump(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
}

fn is_paused(env: &Env) -> bool {
    env.storage()
        .instance()
        .get(&DataKey::Paused)
        .unwrap_or(false)
}

fn require_not_paused(env: &Env) {
    if is_paused(env) {
        panic_with_error!(env, PausedError::Paused);
    }
}

fn require_paused(env: &Env) {
    if !is_paused(env) {
        panic_with_error!(env, PausedError::NotPaused);
    }
}

fn get_scalar(env: &Env, key: &DataKey) -> i128 {
    env.storage().instance().get(key).unwrap_or(0)
}

fn set_scalar(env: &Env, key: &DataKey, v: i128) {
    env.storage().instance().set(key, &v);
}

fn get_total_premiums(env: &Env) -> i128 {
    get_scalar(env, &DataKey::TotalPremiums)
}
fn get_total_payouts(env: &Env) -> i128 {
    get_scalar(env, &DataKey::TotalPayouts)
}
fn get_accumulated_fees(env: &Env) -> i128 {
    get_scalar(env, &DataKey::AccumulatedFees)
}
fn get_total_org_reserves(env: &Env) -> i128 {
    get_scalar(env, &DataKey::TotalOrgReserves)
}

fn require_policy_manager(env: &Env) -> Address {
    match env
        .storage()
        .instance()
        .get::<DataKey, Address>(&DataKey::PolicyManager)
    {
        Some(a) => a,
        None => panic_with_error!(env, TreasuryError::PolicyManagerNotSet),
    }
}

/// Resolve a policy's backing org via PolicyManager (revert if PolicyManager unset). Soroban has
/// no zero address, so `OrgNotResolved` is unreachable here — the callee raises on a missing org.
fn resolve_org(env: &Env, policy_id: u64) -> Address {
    PolicyManagerClient::new(env, &require_policy_manager(env)).policy_org(&policy_id)
}

// ----- persistent per-org / per-policy accessors (with TTL bump on write) -----

fn org_reserve_of(env: &Env, org: &Address) -> i128 {
    env.storage()
        .persistent()
        .get(&DataKey::OrgReserve(org.clone()))
        .unwrap_or(0)
}

fn set_org_reserve(env: &Env, org: &Address, v: i128) {
    let key = DataKey::OrgReserve(org.clone());
    env.storage().persistent().set(&key, &v);
    env.storage()
        .persistent()
        .extend_ttl(&key, PERSISTENT_LIFETIME_THRESHOLD, PERSISTENT_BUMP_AMOUNT);
}

fn is_premium_received(env: &Env, policy_id: u64) -> bool {
    env.storage()
        .persistent()
        .get(&DataKey::PremiumReceived(policy_id))
        .unwrap_or(false)
}

fn set_premium_received(env: &Env, policy_id: u64) {
    let key = DataKey::PremiumReceived(policy_id);
    env.storage().persistent().set(&key, &true);
    env.storage()
        .persistent()
        .extend_ttl(&key, PERSISTENT_LIFETIME_THRESHOLD, PERSISTENT_BUMP_AMOUNT);
}

fn is_payout_processed(env: &Env, policy_id: u64) -> bool {
    env.storage()
        .persistent()
        .get(&DataKey::PayoutProcessed(policy_id))
        .unwrap_or(false)
}

fn set_payout_processed(env: &Env, policy_id: u64) {
    let key = DataKey::PayoutProcessed(policy_id);
    env.storage().persistent().set(&key, &true);
    env.storage()
        .persistent()
        .extend_ttl(&key, PERSISTENT_LIFETIME_THRESHOLD, PERSISTENT_BUMP_AMOUNT);
}

fn set_persistent_u32(env: &Env, key: DataKey, v: u32) {
    env.storage().persistent().set(&key, &v);
    env.storage()
        .persistent()
        .extend_ttl(&key, PERSISTENT_LIFETIME_THRESHOLD, PERSISTENT_BUMP_AMOUNT);
}

fn set_persistent_bool(env: &Env, key: DataKey, v: bool) {
    env.storage().persistent().set(&key, &v);
    env.storage()
        .persistent()
        .extend_ttl(&key, PERSISTENT_LIFETIME_THRESHOLD, PERSISTENT_BUMP_AMOUNT);
}

/// Effective reserve ratio for an org (explicit per-org value if set — including 0 — else the
/// default). Returned as `i128` for the reserve math.
fn reserve_ratio_bps(env: &Env, org: &Address) -> i128 {
    let set: bool = env
        .storage()
        .persistent()
        .get(&DataKey::OrgRatioSet(org.clone()))
        .unwrap_or(false);
    if set {
        let r: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::OrgReserveRatioBps(org.clone()))
            .unwrap_or(0);
        r as i128
    } else {
        DEFAULT_RESERVE_RATIO_BPS as i128
    }
}

/// Effective fee bps for an org (explicit per-org value if set — including 0 — else the global
/// `platform_fee_percent * 100`). Returned as `i128` for the fee math.
fn fee_bps(env: &Env, org: &Address) -> i128 {
    let set: bool = env
        .storage()
        .persistent()
        .get(&DataKey::OrgFeeSet(org.clone()))
        .unwrap_or(false);
    if set {
        let f: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::OrgFeeBps(org.clone()))
            .unwrap_or(0);
        f as i128
    } else {
        let percent: u32 = env
            .storage()
            .instance()
            .get(&DataKey::PlatformFeePercent)
            .unwrap_or(DEFAULT_PLATFORM_FEE_PERCENT);
        percent as i128 * 100
    }
}

#[cfg(test)]
mod test;
