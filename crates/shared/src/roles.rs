//! Role registry — the Soroban equivalent of OpenZeppelin `AccessControlUpgradeable`.
//!
//! In the EVM contracts each state-changing function is guarded by `onlyRole(X)` against
//! `msg.sender`. Here every state-changing function takes an explicit acting `caller:
//! Address`, calls `caller.require_auth()` (the host verifies the signature + replay), and
//! then checks this registry. Multiple holders per role are supported, matching
//! AccessControl's N-holders semantics.
//!
//! Each holder is keyed `(Role, Address) -> bool` in the calling contract's own
//! **persistent** storage (roles must outlive the instance TTL and are looked up on writes).

use soroban_sdk::{contracttype, panic_with_error, Address, Env};

use crate::errors::CommonError;
use crate::types::Role;

/// Storage key for a role grant. Lives in each contract's own storage namespace, so the
/// same key type is reused across contracts without collision.
#[contracttype]
#[derive(Clone)]
pub enum RoleKey {
    Role(Role, Address),
}

/// True if `who` holds `role` in the current contract.
pub fn has_role(env: &Env, role: Role, who: &Address) -> bool {
    env.storage()
        .persistent()
        .get(&RoleKey::Role(role, who.clone()))
        .unwrap_or(false)
}

/// Grant `role` to `who` (no authorization check — callers must gate this, e.g. via
/// `require_role(env, caller, Role::Admin)` or during `__constructor`).
pub fn grant_role(env: &Env, role: Role, who: &Address) {
    env.storage()
        .persistent()
        .set(&RoleKey::Role(role, who.clone()), &true);
}

/// Revoke `role` from `who`.
pub fn revoke_role(env: &Env, role: Role, who: &Address) {
    env.storage()
        .persistent()
        .remove(&RoleKey::Role(role, who.clone()));
}

/// Require that `caller` authorized this invocation AND holds `role`; otherwise panic
/// with [`CommonError::Unauthorized`]. This is the standard guard at the top of every
/// role-gated entrypoint.
pub fn require_role(env: &Env, caller: &Address, role: Role) {
    caller.require_auth();
    if !has_role(env, role, caller) {
        panic_with_error!(env, CommonError::Unauthorized);
    }
}
