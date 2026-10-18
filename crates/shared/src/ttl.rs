//! Storage TTL policy shared by all four contracts: a rent cap.
//!
//! Soroban rents storage. Extending an entry's TTL charges, up front, the rent for every
//! ledger it is extended by. The previous policy extended every entry, the instance and the
//! contract CODE to the network maximum (~180 days) on every touch. Because a fresh upload
//! starts at the network minimum (7 days on testnet), the first ordinary call to each
//! contract paid ~173 days of code rent: 362–833 XLM per contract on testnet, charged to
//! whichever user or money-path call happened to touch it first (`receive_premium`,
//! `submit_determination`). The per-day cost of keeping code alive is a property of the
//! network and the wasm size; this module only decides **who pays it, and when**.
//!
//! ## Policy per entry class (5-second ledgers, 1 day = 17,280 ledgers)
//!
//! | class | rule | kept alive by |
//! |---|---|---|
//! | instance + code, user path | below 3 d -> 10 d (safety net only) | any call ([`touch_contract`]) |
//! | instance + code, keeper | below 23 d -> 30 d | permissionless `extend_contract_ttl()` ([`keep_contract_alive`]) |
//! | role grants | below 30 d -> 60 d | grant and every use ([`extend_role`]) |
//! | per-policy entries | need = `end_date + 30 d`, clamped to the max | the lifecycle calls ([`extend_for_policy`]) |
//! | pending-claim entries | below 30 d -> 60 d | settlement attempts, `extend_policy_ttl` ([`extend_pending`]) |
//! | shared per-farmer / per-org entries | `max(current, need, 60 d)` | every touch ([`extend_shared`]) |
//!
//! ## Archived entries fail closed
//! The host refuses to read an expired persistent entry ("accessing no-longer-live entry");
//! the transaction has to restore it first. So an archived `PolicyPaid`, `PayoutProcessed`,
//! `PremiumReceived`, role grant or reserve never reads as absent or `false`: the call traps,
//! or the entry is restored and reads its true value. Instance flags (paused, signer, network
//! domain, enabled methodologies) archive with the whole instance, which makes the contract
//! unusable rather than unpaused. Shortening a TTL therefore changes cost and availability,
//! never correctness. Rules that keep it so: no security flag lives in temporary storage
//! except the redundant replay marker, and no positive guard (`PolicyPaid`,
//! `PayoutProcessed`, `PremiumReceived`) is ever `remove()`d. Entries that are removed
//! (revoked role grants, `ClaimPending`, Treasury's `PendingPayout` and `OrgQueueAt`,
//! `PendingCounted`) are removed only when their absence is the correct state.
//!
//! ## Measured network limits (2026-10-13, `getLedgerEntries` on `ConfigSettingStateArchival`)
//!
//! | network | `max_entry_ttl` (ledgers) | at ~5 s/ledger | `min_persistent_ttl` |
//! |---|---|---|---|
//! | mainnet (protocol 29) | 3,110,400 | ~180 days | 2,073,600 (~120 d) |
//! | testnet (protocol 29) | 3,110,400 | ~180 days | 120,960 (~7 d) |

use soroban_sdk::{Address, Env, IntoVal, Val};

/// The network `max_entry_ttl` measured on mainnet and testnet (2026-10-13). Documentation
/// and tests only: the contracts read the live value through `max_ttl()`.
pub const NETWORK_MAX_ENTRY_TTL_LEDGERS: u32 = 3_110_400;

/// Target ledger close time used to convert seconds into ledgers.
pub const LEDGER_SECONDS: u64 = 5;
/// One day of ledgers at [`LEDGER_SECONDS`].
pub const DAY_LEDGERS: u32 = 17_280;

/// User path: instance + code are extended only when below this many ledgers (3 days)...
pub const USER_THRESHOLD_LEDGERS: u32 = 3 * DAY_LEDGERS;
/// ...and then only to this many (10 days). A safety net, not the keep-alive.
pub const USER_TARGET_LEDGERS: u32 = 10 * DAY_LEDGERS;
/// Keeper: `extend_contract_ttl` extends instance + code when below 23 days...
pub const KEEPER_THRESHOLD_LEDGERS: u32 = 23 * DAY_LEDGERS;
/// ...to 30 days. Run daily: a no-op until the TTL falls under the threshold.
pub const KEEPER_TARGET_LEDGERS: u32 = 30 * DAY_LEDGERS;
/// Role grants: extended on grant and on use when below 30 days, to 60 days.
pub const ROLE_THRESHOLD_LEDGERS: u32 = 30 * DAY_LEDGERS;
/// See [`ROLE_THRESHOLD_LEDGERS`].
pub const ROLE_TARGET_LEDGERS: u32 = 60 * DAY_LEDGERS;
/// Pending-claim entries: extended when below 30 days, to 60 days.
pub const PENDING_THRESHOLD_LEDGERS: u32 = 30 * DAY_LEDGERS;
/// See [`PENDING_THRESHOLD_LEDGERS`].
pub const PENDING_TARGET_LEDGERS: u32 = 60 * DAY_LEDGERS;
/// Floor for shared per-farmer / per-org entries touched by any call (60 days).
pub const SHARED_FLOOR_LEDGERS: u32 = 60 * DAY_LEDGERS;
/// A per-policy entry must outlive the policy's `end_date` by this much (30 days).
pub const POLICY_GRACE_SECS: u64 = 30 * 86_400;
/// `FarmerClaimTimes` must outlive the rolling 365-day claim window.
pub const CLAIM_HISTORY_SECS: u64 = 365 * 86_400;

/// User path: extend this contract's instance and code below 3 days to 10 days. Every
/// entrypoint calls it, so an unattended contract never archives, but no ordinary call pays
/// more than ~7 days of code rent even if the keeper lapses.
pub fn touch_contract(env: &Env) {
    let max = env.storage().max_ttl();
    env.storage().instance().extend_ttl(
        USER_THRESHOLD_LEDGERS.min(max),
        USER_TARGET_LEDGERS.min(max),
    );
}

/// Keeper path: extend this contract's instance and code below 23 days to 30 days.
pub fn keep_contract_alive(env: &Env) {
    let max = env.storage().max_ttl();
    env.storage().instance().extend_ttl(
        KEEPER_THRESHOLD_LEDGERS.min(max),
        KEEPER_TARGET_LEDGERS.min(max),
    );
}

/// Keeper path for a sibling contract: extend its instance and code below 23 days to 30
/// days. The host allows any contract to extend any other contract's TTL.
pub fn keep_other_contract_alive(env: &Env, contract: &Address) {
    let max = env.storage().max_ttl();
    env.deployer().extend_ttl(
        contract.clone(),
        KEEPER_THRESHOLD_LEDGERS.min(max),
        KEEPER_TARGET_LEDGERS.min(max),
    );
}

/// Ledgers from now until unix time `t`: `ceil((t - now) / 5)`, clamped to the network
/// maximum TTL. Zero when `t` is not in the future.
pub fn ledgers_until(env: &Env, t: u64) -> u32 {
    let now = env.ledger().timestamp();
    if t <= now {
        return 0;
    }
    let ledgers = (t - now).div_ceil(LEDGER_SECONDS);
    let max = env.storage().max_ttl() as u64;
    if ledgers > max {
        max as u32
    } else {
        ledgers as u32
    }
}

/// Extend an existing persistent entry so it lives at least `n` ledgers (clamped to the
/// network maximum). The threshold is `n - 1 day`, so a touch within a day of the last
/// extension is free, and an entry that already lives longer is never shortened. `n == 0`
/// is a no-op. The entry must exist.
pub fn extend_to<K: IntoVal<Env, Val>>(env: &Env, key: &K, n: u32) {
    let n = n.min(env.storage().max_ttl());
    if n == 0 {
        return;
    }
    let threshold = n.saturating_sub(DAY_LEDGERS);
    env.storage().persistent().extend_ttl(key, threshold, n);
}

/// Per-policy entry: live until the policy's `end_date` plus [`POLICY_GRACE_SECS`] (clamped).
/// Re-applied at activation, when `end_date` moves. A no-op once that time has passed, so a
/// closed policy's entries keep the TTL they already have.
pub fn extend_for_policy<K: IntoVal<Env, Val>>(env: &Env, key: &K, end_date: u64) {
    let n = ledgers_until(env, end_date.saturating_add(POLICY_GRACE_SECS));
    extend_to(env, key, n);
}

/// Pending-claim entry: extend below 30 days to 60 days.
pub fn extend_pending<K: IntoVal<Env, Val>>(env: &Env, key: &K) {
    let max = env.storage().max_ttl();
    env.storage().persistent().extend_ttl(
        key,
        PENDING_THRESHOLD_LEDGERS.min(max),
        PENDING_TARGET_LEDGERS.min(max),
    );
}

/// Role grant: extend below 30 days to 60 days.
pub fn extend_role<K: IntoVal<Env, Val>>(env: &Env, key: &K) {
    let max = env.storage().max_ttl();
    env.storage().persistent().extend_ttl(
        key,
        ROLE_THRESHOLD_LEDGERS.min(max),
        ROLE_TARGET_LEDGERS.min(max),
    );
}

/// Shared per-farmer / per-org entry: at least the need of the policy touching it (when one
/// is known) and at least [`SHARED_FLOOR_LEDGERS`]. Extension only ever lengthens a TTL, so
/// the entry keeps the longest need of any policy that touched it.
pub fn extend_shared<K: IntoVal<Env, Val>>(env: &Env, key: &K, end_date: Option<u64>) {
    let need = match end_date {
        Some(end) => ledgers_until(env, end.saturating_add(POLICY_GRACE_SECS)),
        None => 0,
    };
    extend_to(env, key, need.max(SHARED_FLOOR_LEDGERS));
}

/// Shared entry with a need of `secs` from now (e.g. `FarmerClaimTimes`, 365 days).
pub fn extend_for_secs<K: IntoVal<Env, Val>>(env: &Env, key: &K, secs: u64) {
    let until = env.ledger().timestamp().saturating_add(secs);
    extend_to(
        env,
        key,
        ledgers_until(env, until).max(SHARED_FLOOR_LEDGERS),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::testutils::storage::{Instance as _, Persistent as _};
    use soroban_sdk::testutils::Ledger as _;
    use soroban_sdk::{contract, contractimpl, symbol_short};

    #[contract]
    struct Probe;

    #[contractimpl]
    impl Probe {}

    const NOW: u64 = 1_700_000_000;

    fn env() -> (Env, Address) {
        let env = Env::default();
        env.ledger().set_timestamp(NOW);
        env.ledger()
            .set_max_entry_ttl(NETWORK_MAX_ENTRY_TTL_LEDGERS);
        let id = env.register(Probe, ());
        (env, id)
    }

    #[test]
    fn ledgers_until_rounds_up_and_clamps() {
        let (env, id) = env();
        env.as_contract(&id, || {
            assert_eq!(ledgers_until(&env, NOW), 0);
            assert_eq!(ledgers_until(&env, NOW - 1), 0);
            assert_eq!(ledgers_until(&env, NOW + 1), 1);
            assert_eq!(ledgers_until(&env, NOW + 5), 1);
            assert_eq!(ledgers_until(&env, NOW + 6), 2);
            assert_eq!(ledgers_until(&env, NOW + 86_400), DAY_LEDGERS);
            // 400 days clamps to the network maximum.
            assert_eq!(
                ledgers_until(&env, NOW + 400 * 86_400),
                env.storage().max_ttl()
            );
            assert_eq!(ledgers_until(&env, u64::MAX), env.storage().max_ttl());
        });
    }

    #[test]
    fn extend_to_clamps_never_shortens_and_skips_within_a_day() {
        let (env, id) = env();
        let k = symbol_short!("k");
        env.as_contract(&id, || {
            env.storage().persistent().set(&k, &1u32);
            let max = env.storage().max_ttl();
            extend_to(&env, &k, 40 * DAY_LEDGERS);
            assert_eq!(env.storage().persistent().get_ttl(&k), 40 * DAY_LEDGERS);
            // A shorter request never shortens it.
            extend_to(&env, &k, 10 * DAY_LEDGERS);
            assert_eq!(env.storage().persistent().get_ttl(&k), 40 * DAY_LEDGERS);
            // A request within one day of the current TTL is a no-op (threshold = n - 1 d).
            extend_to(&env, &k, 40 * DAY_LEDGERS + 100);
            assert_eq!(env.storage().persistent().get_ttl(&k), 40 * DAY_LEDGERS);
            // Past the maximum it clamps.
            extend_to(&env, &k, u32::MAX);
            assert_eq!(env.storage().persistent().get_ttl(&k), max);
            // Zero is a no-op.
            extend_to(&env, &k, 0);
            assert_eq!(env.storage().persistent().get_ttl(&k), max);
        });
        // The thresholds of every fixed class are below their targets.
        const _: () = assert!(USER_THRESHOLD_LEDGERS <= USER_TARGET_LEDGERS);
        const _: () = assert!(KEEPER_THRESHOLD_LEDGERS <= KEEPER_TARGET_LEDGERS);
        const _: () = assert!(ROLE_THRESHOLD_LEDGERS <= ROLE_TARGET_LEDGERS);
        const _: () = assert!(PENDING_THRESHOLD_LEDGERS <= PENDING_TARGET_LEDGERS);
        const _: () = assert!(USER_TARGET_LEDGERS < KEEPER_THRESHOLD_LEDGERS);
    }

    #[test]
    fn policy_need_is_end_date_plus_grace() {
        let (env, id) = env();
        let k = symbol_short!("p");
        env.as_contract(&id, || {
            env.storage().persistent().set(&k, &1u32);
            let end = NOW + 140 * 86_400;
            extend_for_policy(&env, &k, end);
            assert_eq!(
                env.storage().persistent().get_ttl(&k),
                (140 + 30) * DAY_LEDGERS
            );
            // A policy that ended long ago extends nothing.
            let k2 = symbol_short!("q");
            env.storage().persistent().set(&k2, &1u32);
            let before = env.storage().persistent().get_ttl(&k2);
            extend_for_policy(&env, &k2, NOW - 365 * 86_400);
            assert_eq!(env.storage().persistent().get_ttl(&k2), before);
        });
    }

    #[test]
    fn shared_entries_take_the_longest_need_and_the_floor() {
        let (env, id) = env();
        let k = symbol_short!("s");
        env.as_contract(&id, || {
            env.storage().persistent().set(&k, &1u32);
            extend_shared(&env, &k, None);
            assert_eq!(env.storage().persistent().get_ttl(&k), SHARED_FLOOR_LEDGERS);
            extend_shared(&env, &k, Some(NOW + 100 * 86_400));
            assert_eq!(env.storage().persistent().get_ttl(&k), 130 * DAY_LEDGERS);
            // A shorter policy does not shorten it.
            extend_shared(&env, &k, Some(NOW + 31 * 86_400));
            assert_eq!(env.storage().persistent().get_ttl(&k), 130 * DAY_LEDGERS);
        });
    }

    #[test]
    fn instance_user_and_keeper_targets() {
        let (env, id) = env();
        let ttl = || env.as_contract(&id, || env.storage().instance().get_ttl());
        // A fresh instance starts at the network minimum (4096 ledgers in the test env).
        env.as_contract(&id, || touch_contract(&env));
        assert_eq!(ttl(), USER_TARGET_LEDGERS);
        env.as_contract(&id, || keep_contract_alive(&env));
        assert_eq!(ttl(), KEEPER_TARGET_LEDGERS);
        // The user path never extends above the 3-day threshold.
        env.as_contract(&id, || touch_contract(&env));
        assert_eq!(ttl(), KEEPER_TARGET_LEDGERS);
        // The keeper is a no-op above 23 days.
        let seq = env.ledger().sequence();
        env.ledger().set_sequence_number(seq + 6 * DAY_LEDGERS);
        env.as_contract(&id, || keep_contract_alive(&env));
        assert_eq!(ttl(), 24 * DAY_LEDGERS);
    }
}
