//! Storage TTL policy shared by all four contracts.
//!
//! Soroban rents storage: a persistent or instance entry whose TTL runs out is archived and
//! has to be restored before it can be read again. The host refuses (clamps) any extension
//! past the network's `max_entry_ttl`, so asking for "400 days" never bought 400 days.
//!
//! ## Measured network limit (2026-10-13, `getLedgerEntries` on `ConfigSettingStateArchival`)
//!
//! | network | `max_entry_ttl` (ledgers) | at ~5 s/ledger | `min_persistent_ttl` |
//! |---|---|---|---|
//! | mainnet (`mainnet.sorobanrpc.com`, protocol 29) | 3,110,400 | ~180 days | 2,073,600 (~120 d) |
//! | testnet (`soroban-testnet.stellar.org`, protocol 29) | 3,110,400 | ~180 days | 120,960 (~7 d) |
//!
//! A 365-day policy therefore OUTLIVES one TTL. The rule here: every touch of a long-lived
//! entry extends it to the network maximum ([`soroban_sdk::storage::Storage::max_ttl`], which
//! reads the live limit, so a future network change needs no code change). An entry that is
//! not touched for ~180 days archives anyway; `extend_policy_ttl(policy_id)` on
//! PayoutReceiver bumps every entry of one policy across the four contracts, and the
//! deployment runbook tells the backend to call it at least every ~150 days for open policies.

use soroban_sdk::{Env, IntoVal, Val};

/// The network `max_entry_ttl` measured on mainnet and testnet (2026-10-13). Documentation
/// and tests only: the contracts read the live value through `max_ttl()`.
pub const NETWORK_MAX_ENTRY_TTL_LEDGERS: u32 = 3_110_400;

/// Extend this contract's instance (and its code entry) to the network maximum.
pub fn extend_instance(env: &Env) {
    let max = env.storage().max_ttl();
    env.storage().instance().extend_ttl(max, max);
}

/// Extend an existing persistent entry to the network maximum. The entry must exist.
pub fn extend_persistent<K: IntoVal<Env, Val>>(env: &Env, key: &K) {
    let max = env.storage().max_ttl();
    env.storage().persistent().extend_ttl(key, max, max);
}

/// Extend a persistent entry to the network maximum if it exists. Returns whether it did.
pub fn extend_persistent_if_present<K: IntoVal<Env, Val>>(env: &Env, key: &K) -> bool {
    if env.storage().persistent().has(key) {
        extend_persistent(env, key);
        true
    } else {
        false
    }
}
