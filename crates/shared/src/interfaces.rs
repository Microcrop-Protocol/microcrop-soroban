//! Cross-contract client traits.
//!
//! We use `#[contractclient]` on plain trait definitions rather than `contractimport!`:
//! `contractimport!` needs the callee's compiled `.wasm` at build time, which would create
//! a build-order + circular dependency between PayoutReceiver <-> PolicyManager <-> SettlementAdapter.
//! `#[contractclient]` generates a typed `Client` from the trait alone (no WASM coupling);
//! each real contract exposes matching inherent methods, so the generated client is correct
//! at call time.
//!
//! IMPORTANT: these signatures are the cross-contract ABI contract. They MUST stay in exact
//! lockstep (name, arg order, types) with the inherent methods on the real contracts.

use soroban_sdk::{contractclient, Address, BytesN, Env};

use crate::types::{Obligation, Policy};

/// Client for `PolicyManager` — used by `PayoutReceiver` (reads + claim effects). The views
/// `policy_org` and `org_outstanding_sum_insured` stay on PolicyManager but are no longer
/// called cross-contract, so they are not part of this ABI.
#[contractclient(name = "PolicyManagerClient")]
pub trait IPolicyManager {
    fn get_policy(env: Env, policy_id: u64) -> Policy;
    fn policy_exists(env: Env, policy_id: u64) -> bool;
    fn can_farmer_claim(env: Env, farmer: Address) -> bool;
    fn mark_as_claimed(env: Env, caller: Address, policy_id: u64);
    fn increment_claim_count(env: Env, caller: Address, farmer: Address);
    /// Permissionless: extend every PolicyManager (and, best effort, PolicyNft) entry of
    /// `policy_id` to its need (`end_date + 30 d`).
    fn extend_policy_ttl(env: Env, policy_id: u64);
    /// Permissionless keeper: extend PolicyManager's and PolicyNft's instance and code below
    /// 23 days to 30 days.
    fn extend_contract_ttl(env: Env);
}

/// Client for `SettlementAdapter` — used by `PayoutReceiver` to record a verified payout
/// obligation. The adapter holds no tokens and makes no calls; it is a ledger of what is
/// owed and of attestations that it was funded and settled.
#[contractclient(name = "SettlementAdapterClient")]
pub trait ISettlementAdapter {
    /// Record the obligation for (`caller`, `policy_id`). Only the configured PayoutReceiver,
    /// passing its own address as `caller`, may call it. Idempotent for identical fields.
    fn record_obligation(
        env: Env,
        caller: Address,
        policy_id: u64,
        org: Address,
        amount: i128,
        determination_digest: BytesN<32>,
    ) -> Obligation;
    /// The PayoutReceiver this adapter accepts obligations from (`None` until bound).
    fn payout_receiver(env: Env) -> Option<Address>;
    /// The obligation id recorded by `payout_receiver` for `policy_id`, if any.
    fn find_obligation(env: Env, payout_receiver: Address, policy_id: u64) -> Option<u64>;
    /// Permissionless: extend an open obligation's entries (no-op once Settled or absent).
    fn extend_obligation_ttl(env: Env, obligation_id: u64);
}

/// Client for `PolicyNft` — used by `PolicyManager` on activation (mint) and lifecycle
/// transitions (deactivate).
#[contractclient(name = "PolicyNftClient")]
pub trait IPolicyNft {
    fn policy_nft_exists(env: Env, policy_id: u64) -> bool;
    fn update_policy_status(env: Env, caller: Address, policy_id: u64, is_active: bool);
    /// Permissionless: extend the certificate entries of `policy_id` (if minted) to the
    /// policy's need (`end_date + 30 d`).
    fn extend_policy_ttl(env: Env, policy_id: u64);
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
