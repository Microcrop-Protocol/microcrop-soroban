//! Cross-contract client traits.
//!
//! We use `#[contractclient]` on plain trait definitions rather than `contractimport!`:
//! `contractimport!` needs the callee's compiled `.wasm` at build time, which would create
//! a build-order + circular dependency between PayoutReceiver <-> PolicyManager <-> Treasury.
//! `#[contractclient]` generates a typed `Client` from the trait alone (no WASM coupling);
//! each real contract exposes matching inherent methods, so the generated client is correct
//! at call time.
//!
//! IMPORTANT: these signatures are the cross-contract ABI contract. They MUST stay in exact
//! lockstep (name, arg order, types) with the inherent methods on the real contracts.

use soroban_sdk::{contractclient, Address, Env};

use crate::types::Policy;

/// Client for `PolicyManager` — used by `PayoutReceiver` (reads + claim effects) and by
/// `Treasury` (per-org solvency reads).
#[contractclient(name = "PolicyManagerClient")]
pub trait IPolicyManager {
    fn get_policy(env: Env, policy_id: u64) -> Policy;
    fn policy_exists(env: Env, policy_id: u64) -> bool;
    fn can_farmer_claim(env: Env, farmer: Address) -> bool;
    fn policy_org(env: Env, policy_id: u64) -> Address;
    fn org_outstanding_sum_insured(env: Env, org: Address) -> i128;
    fn mark_as_claimed(env: Env, caller: Address, policy_id: u64);
    fn increment_claim_count(env: Env, caller: Address, farmer: Address);
}

/// Client for `Treasury` — used by `PayoutReceiver` to disburse an authorized payout.
#[contractclient(name = "TreasuryClient")]
pub trait ITreasury {
    fn request_payout(env: Env, caller: Address, policy_id: u64, amount: i128);
}

/// Client for `PolicyNft` — used by `PolicyManager` on activation (mint) and lifecycle
/// transitions (deactivate).
#[contractclient(name = "PolicyNftClient")]
pub trait IPolicyNft {
    fn policy_nft_exists(env: Env, policy_id: u64) -> bool;
    fn update_policy_status(env: Env, caller: Address, policy_id: u64, is_active: bool);
    #[allow(clippy::too_many_arguments)]
    fn mint_policy(
        env: Env,
        caller: Address,
        farmer: Address,
        policy_id: u64,
        distributor: Address,
        distributor_name: soroban_sdk::String,
        sum_insured: i128,
        premium: i128,
        start_date: u64,
        end_date: u64,
        coverage_type: crate::types::CoverageType,
        region: soroban_sdk::String,
        plot_id: u64,
    ) -> u64;
}
