#![no_std]
//! # SettlementAdapter
//!
//! Option B (2026-10-19): the non-custodial replacement for the per-org Treasury. After the
//! PayoutReceiver verifies a signed determination it records here what is owed: one
//! obligation per (PayoutReceiver, policy). The `Role::Funder` holder (the MicroCrop backend)
//! then attests that the obligation was funded and settled off-chain (mobile money), each step
//! identified by a 32-byte reference hash of the off-chain payment record.
//!
//! This is the "minimal FundingFacility/SettlementAdapter stub" of the Stellar Instaward SOW
//! (deliverable 2): it tracks verified obligations and their funding status without pooling
//! or custodying farmer premiums. A real FundingFacility can replace it later through
//! `PayoutReceiver::set_settlement_adapter`, without redeploying the PayoutReceiver.
//!
//! ## What it is not
//! - It holds no tokens, imports no token interface and makes no cross-contract calls. The
//!   `amount` of an obligation is a unit of account (USDC, 7dp base units), not a balance.
//! - `Funded` and `Settled` are ATTESTATIONS, not proof of payment. Only the `Funder` role
//!   makes them. The org is the party that owes the money, so letting it mark its own
//!   obligation settled would be a self-waive (and, once an org held a real key, it could race
//!   the Funder's real settlement into a 505). The reference hash, the revocable role and the
//!   events are the audit trail; nothing here proves money moved.
//! - It has no pause. It is not value-bearing: the worst a misuse can do is write a wrong
//!   attestation, which a pause would not prevent and the events expose. The PayoutReceiver
//!   keeps its pause, which stops new obligations.
//!
//! ## State machine
//! `Owed -> Funded -> Settled`, or `Owed -> Settled` (funding may be implicit). Status never
//! moves backwards. Repeating a transition with the same reference is a no-op (webhooks
//! arrive twice); with a different reference it is refused (504 / 505).
//!
//! ## Authorisation (security.md classes 1 and 2)
//! - `record_obligation`: `caller` must equal the PayoutReceiver stored here AND
//!   `caller.require_auth()` (a direct call from that contract satisfies it).
//! - `mark_funded` / `mark_settled`: `caller.require_auth()`, then `caller` must hold
//!   `Role::Funder` (the org cannot mark its own obligation).
//! - `set_payout_receiver`, `grant_role`, `revoke_role`: `Role::Admin`; `upgrade`:
//!   `Role::Upgrader`. Role grants and revocations emit `RoleGranted` / `RoleRevoked`.
//!
//! ## Keying
//! Policy ids are unique only per PolicyManager. Obligations are looked up by
//! `ObligationOf(payout_receiver, policy_id)`, so rotating to a new PayoutReceiver can never
//! collide with an old one's obligations. Within one PayoutReceiver there is at most one
//! obligation per policy; across PayoutReceivers sharing a PolicyManager, the policy's
//! `Claimed` status stops a second claim.
//!
//! ## Storage and TTL (rent cap, `microcrop_shared::ttl`)
//! Instance: `PayoutReceiver`, `NextObligationId`; every state-changing entrypoint extends
//! instance + code below 3 d to 10 d, the keeper `extend_contract_ttl` below 23 d to 30 d
//! (and `PayoutReceiver::extend_contract_ttl` keeps this contract alive too). Persistent:
//! `Obligation`, `ObligationOf`, `OrgObligationAt` below 30 d to 60 d on every write and by
//! `extend_obligation_ttl` while not Settled; `OrgObligationCount` at least 60 d. A Settled
//! obligation is left to archive: it stays restorable, and an archived entry fails closed
//! (the host refuses the read), so it can never be recreated as fresh.

use soroban_sdk::{
    contract, contractevent, contractimpl, contracttype, panic_with_error, Address, BytesN, Env,
    Vec,
};

use microcrop_shared::{
    errors::{AdapterError, CommonError},
    roles, ttl,
    types::{Obligation, ObligationStatus, Role},
};

/// Largest page `get_org_obligations` returns. A page of ids plus the count and the instance
/// stays well under the per-transaction footprint limit (security.md class 10).
pub const MAX_PAGE_SIZE: u32 = 50;

// ============ Events ============

/// `by` granted `role` to `who` (e.g. `Role::Funder`, the adapter's most powerful grant).
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoleGranted {
    #[topic]
    pub role: Role,
    #[topic]
    pub who: Address,
    pub by: Address,
}

/// `by` revoked `role` from `who`.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoleRevoked {
    #[topic]
    pub role: Role,
    #[topic]
    pub who: Address,
    pub by: Address,
}

/// The PayoutReceiver this adapter accepts obligations from was set or rotated.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PayoutReceiverSet {
    pub previous: Option<Address>,
    pub payout_receiver: Address,
}

/// A verified payout obligation was recorded (status `Owed`).
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObligationRecorded {
    #[topic]
    pub obligation_id: u64,
    #[topic]
    pub policy_id: u64,
    #[topic]
    pub org: Address,
    pub payout_receiver: Address,
    pub amount: i128,
    pub determination_digest: BytesN<32>,
}

/// `by` attested that the obligation was funded (an attestation, not proof of payment).
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObligationFunded {
    #[topic]
    pub obligation_id: u64,
    #[topic]
    pub policy_id: u64,
    #[topic]
    pub org: Address,
    pub by: Address,
    pub reference_hash: BytesN<32>,
}

/// `by` attested that the obligation was settled (an attestation, not proof of payment).
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObligationSettled {
    #[topic]
    pub obligation_id: u64,
    #[topic]
    pub policy_id: u64,
    #[topic]
    pub org: Address,
    pub by: Address,
    pub reference_hash: BytesN<32>,
    pub amount: i128,
}

/// Storage keys (typed, security.md class 6).
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// Instance: the only PayoutReceiver allowed to record obligations.
    PayoutReceiver,
    /// Instance: the next obligation id (starts at 1).
    NextObligationId,
    /// Persistent: obligation id -> `Obligation`.
    Obligation(u64),
    /// Persistent: (payout_receiver, policy_id) -> obligation id.
    ObligationOf(Address, u64),
    /// Persistent: org -> number of obligations recorded for it.
    OrgObligationCount(Address),
    /// Persistent: (org, index) -> obligation id.
    OrgObligationAt(Address, u32),
}

#[contract]
pub struct SettlementAdapter;

#[contractimpl]
impl SettlementAdapter {
    /// Grants `Role::Admin` and `Role::Upgrader` to `admin`. The PayoutReceiver is unset, so
    /// `record_obligation` fails `NotConfigured` until `set_payout_receiver` binds one.
    pub fn __constructor(env: Env, admin: Address) {
        roles::grant_role(&env, Role::Admin, &admin);
        roles::grant_role(&env, Role::Upgrader, &admin);
        env.storage()
            .instance()
            .set(&DataKey::NextObligationId, &1u64);
    }

    // ---- admin ----

    /// Bind (or rotate) the PayoutReceiver (ADMIN). Rotation is allowed at any time:
    /// existing obligations keep their own `payout_receiver` and stay markable, and the old
    /// PayoutReceiver's next `record_obligation` fails `NotPayoutReceiver` (fail closed).
    pub fn set_payout_receiver(env: Env, caller: Address, payout_receiver: Address) {
        ttl::touch_contract(&env);
        roles::require_role(&env, &caller, Role::Admin);
        if payout_receiver == env.current_contract_address() {
            panic_with_error!(&env, AdapterError::InvalidPayoutReceiver);
        }
        let s = env.storage().instance();
        let previous: Option<Address> = s.get(&DataKey::PayoutReceiver);
        s.set(&DataKey::PayoutReceiver, &payout_receiver);
        PayoutReceiverSet {
            previous,
            payout_receiver,
        }
        .publish(&env);
    }

    /// The PayoutReceiver this adapter accepts obligations from (`None` until bound).
    pub fn payout_receiver(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::PayoutReceiver)
    }

    /// Grant a role (ADMIN only), e.g. `Role::Funder` to the backend account.
    pub fn grant_role(env: Env, caller: Address, role: Role, who: Address) {
        ttl::touch_contract(&env);
        roles::require_role(&env, &caller, Role::Admin);
        roles::grant_role(&env, role, &who);
        RoleGranted {
            role,
            who,
            by: caller,
        }
        .publish(&env);
    }

    /// Revoke a role (ADMIN only).
    pub fn revoke_role(env: Env, caller: Address, role: Role, who: Address) {
        ttl::touch_contract(&env);
        roles::require_role(&env, &caller, Role::Admin);
        roles::revoke_role(&env, role, &who);
        RoleRevoked {
            role,
            who,
            by: caller,
        }
        .publish(&env);
    }

    /// True if `who` holds `role` here. Read-only, no auth.
    pub fn has_role(env: Env, role: Role, who: Address) -> bool {
        roles::has_role(&env, role, &who)
    }

    /// Replace this contract's WASM (UPGRADER only).
    pub fn upgrade(env: Env, caller: Address, new_wasm_hash: BytesN<32>) {
        roles::require_role(&env, &caller, Role::Upgrader);
        env.deployer().update_current_contract_wasm(new_wasm_hash);
    }

    // ---- obligations ----

    /// Record the obligation for (`caller`, `policy_id`). Only the configured PayoutReceiver
    /// may call it, passing its own address as `caller`.
    ///
    /// Order: PayoutReceiver configured (3); `caller == payout_receiver` (500);
    /// `caller.require_auth()`; `amount > 0` (501). If (`caller`, `policy_id`) already has an
    /// obligation with the same org, amount and digest, it is returned unchanged (no write, no
    /// event); with any other field it is refused (502). Otherwise a new `Owed` obligation is
    /// written, indexed under the org and returned.
    pub fn record_obligation(
        env: Env,
        caller: Address,
        policy_id: u64,
        org: Address,
        amount: i128,
        determination_digest: BytesN<32>,
    ) -> Obligation {
        ttl::touch_contract(&env);
        let pr: Address = env
            .storage()
            .instance()
            .get(&DataKey::PayoutReceiver)
            .unwrap_or_else(|| panic_with_error!(&env, CommonError::NotConfigured));
        if caller != pr {
            panic_with_error!(&env, AdapterError::NotPayoutReceiver);
        }
        caller.require_auth();
        if amount <= 0 {
            panic_with_error!(&env, AdapterError::InvalidAmount);
        }

        let p = env.storage().persistent();
        let of_key = DataKey::ObligationOf(caller.clone(), policy_id);
        if let Some(existing_id) = p.get::<_, u64>(&of_key) {
            let ob = Self::load(&env, existing_id);
            if ob.org == org
                && ob.amount == amount
                && ob.determination_digest == determination_digest
            {
                return ob;
            }
            panic_with_error!(&env, AdapterError::ObligationConflict);
        }

        let s = env.storage().instance();
        let id: u64 = s
            .get(&DataKey::NextObligationId)
            .unwrap_or_else(|| panic_with_error!(&env, CommonError::NotInitialized));
        // Checked arithmetic (security.md class 5): traps rather than wrapping.
        let next = id
            .checked_add(1)
            .unwrap_or_else(|| panic_with_error!(&env, AdapterError::CounterOverflow));
        s.set(&DataKey::NextObligationId, &next);

        let count_key = DataKey::OrgObligationCount(org.clone());
        let org_index: u32 = p.get(&count_key).unwrap_or(0);
        let new_count = org_index
            .checked_add(1)
            .unwrap_or_else(|| panic_with_error!(&env, AdapterError::CounterOverflow));

        let ob = Obligation {
            id,
            payout_receiver: caller.clone(),
            policy_id,
            org: org.clone(),
            amount,
            determination_digest: determination_digest.clone(),
            status: ObligationStatus::Owed,
            recorded_at: env.ledger().timestamp(),
            org_index,
            funded_by: None,
            funded_at: 0,
            funding_reference: None,
            settled_by: None,
            settled_at: 0,
            settlement_reference: None,
        };
        p.set(&DataKey::Obligation(id), &ob);
        p.set(&of_key, &id);
        p.set(&DataKey::OrgObligationAt(org.clone(), org_index), &id);
        p.set(&count_key, &new_count);
        Self::extend_entries(&env, &ob);

        ObligationRecorded {
            obligation_id: id,
            policy_id,
            org,
            payout_receiver: caller,
            amount,
            determination_digest,
        }
        .publish(&env);
        ob
    }

    /// Attest that obligation `obligation_id` was funded. `caller` must sign and hold
    /// `Role::Funder`. `Owed -> Funded`; repeating it with the same
    /// reference is a no-op; a different reference is refused (504); a Settled obligation is
    /// refused (505). `reference_hash` must not be all zero (506).
    pub fn mark_funded(
        env: Env,
        caller: Address,
        obligation_id: u64,
        reference_hash: BytesN<32>,
    ) -> Obligation {
        let mut ob = Self::begin_mark(&env, &caller, obligation_id, &reference_hash);
        match ob.status {
            ObligationStatus::Owed => {}
            ObligationStatus::Funded => {
                if ob.funding_reference.as_ref() == Some(&reference_hash) {
                    return ob;
                }
                panic_with_error!(&env, AdapterError::ObligationAlreadyFunded);
            }
            ObligationStatus::Settled => {
                panic_with_error!(&env, AdapterError::ObligationAlreadySettled)
            }
        }
        ob.status = ObligationStatus::Funded;
        ob.funded_by = Some(caller.clone());
        ob.funded_at = env.ledger().timestamp();
        ob.funding_reference = Some(reference_hash.clone());
        env.storage()
            .persistent()
            .set(&DataKey::Obligation(obligation_id), &ob);
        Self::extend_entries(&env, &ob);
        ObligationFunded {
            obligation_id,
            policy_id: ob.policy_id,
            org: ob.org.clone(),
            by: caller,
            reference_hash,
        }
        .publish(&env);
        ob
    }

    /// Attest that obligation `obligation_id` was settled. `caller` must sign and hold
    /// `Role::Funder`. `Owed | Funded -> Settled` (funding may be
    /// implicit); repeating it with the same reference is a no-op; a different reference is
    /// refused (505). `reference_hash` must not be all zero (506).
    pub fn mark_settled(
        env: Env,
        caller: Address,
        obligation_id: u64,
        reference_hash: BytesN<32>,
    ) -> Obligation {
        let mut ob = Self::begin_mark(&env, &caller, obligation_id, &reference_hash);
        if ob.status == ObligationStatus::Settled {
            if ob.settlement_reference.as_ref() == Some(&reference_hash) {
                return ob;
            }
            panic_with_error!(&env, AdapterError::ObligationAlreadySettled);
        }
        ob.status = ObligationStatus::Settled;
        ob.settled_by = Some(caller.clone());
        ob.settled_at = env.ledger().timestamp();
        ob.settlement_reference = Some(reference_hash.clone());
        env.storage()
            .persistent()
            .set(&DataKey::Obligation(obligation_id), &ob);
        Self::extend_entries(&env, &ob);
        ObligationSettled {
            obligation_id,
            policy_id: ob.policy_id,
            org: ob.org.clone(),
            by: caller,
            reference_hash,
            amount: ob.amount,
        }
        .publish(&env);
        ob
    }

    // ---- views ----

    /// The obligation with id `obligation_id`, if any.
    pub fn get_obligation(env: Env, obligation_id: u64) -> Option<Obligation> {
        env.storage()
            .persistent()
            .get(&DataKey::Obligation(obligation_id))
    }

    /// The obligation id `payout_receiver` recorded for `policy_id`, if any.
    pub fn find_obligation(env: Env, payout_receiver: Address, policy_id: u64) -> Option<u64> {
        env.storage()
            .persistent()
            .get(&DataKey::ObligationOf(payout_receiver, policy_id))
    }

    /// How many obligations were ever recorded.
    pub fn obligation_count(env: Env) -> u64 {
        let next: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextObligationId)
            .unwrap_or(1);
        next - 1
    }

    /// How many obligations were recorded for `org`.
    pub fn org_obligation_count(env: Env, org: Address) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::OrgObligationCount(org))
            .unwrap_or(0)
    }

    /// The ids of `org`'s obligations at indexes `start..start + limit`, oldest first.
    /// `limit` is capped at [`MAX_PAGE_SIZE`]; a `start` at or past the end returns an empty
    /// list. Index items of Settled obligations are left to archive: a page that reaches one
    /// needs it restored first (the host refuses the read, it never returns a wrong id).
    pub fn get_org_obligations(env: Env, org: Address, start: u32, limit: u32) -> Vec<u64> {
        let p = env.storage().persistent();
        let count: u32 = p
            .get(&DataKey::OrgObligationCount(org.clone()))
            .unwrap_or(0);
        let mut out = Vec::new(&env);
        if start >= count {
            return out;
        }
        let end = start.saturating_add(limit.min(MAX_PAGE_SIZE)).min(count);
        for i in start..end {
            if let Some(id) = p.get::<_, u64>(&DataKey::OrgObligationAt(org.clone(), i)) {
                out.push_back(id);
            }
        }
        out
    }

    // ---- keepers ----

    /// PERMISSIONLESS: extend an open (Owed / Funded) obligation's entries below 30 d to
    /// 60 d: the obligation, its `ObligationOf` lookup and its org index item, plus the org's
    /// count (at least 60 d). A no-op for an unknown id or a Settled obligation.
    pub fn extend_obligation_ttl(env: Env, obligation_id: u64) {
        ttl::touch_contract(&env);
        let ob: Option<Obligation> = env
            .storage()
            .persistent()
            .get(&DataKey::Obligation(obligation_id));
        if let Some(ob) = ob {
            if ob.status != ObligationStatus::Settled {
                Self::extend_entries(&env, &ob);
            }
        }
    }

    /// PERMISSIONLESS keeper: extend this contract's instance and code below 23 d to 30 d.
    pub fn extend_contract_ttl(env: Env) {
        ttl::keep_contract_alive(&env);
    }

    // ---- private ----

    /// Common prefix of both marks: user-path TTL, the caller's signature, a non-zero
    /// reference, the caller holds `Role::Funder` (1), and the obligation exists (503).
    fn begin_mark(
        env: &Env,
        caller: &Address,
        obligation_id: u64,
        reference_hash: &BytesN<32>,
    ) -> Obligation {
        ttl::touch_contract(env);
        caller.require_auth();
        if reference_hash.to_array() == [0u8; 32] {
            panic_with_error!(env, AdapterError::InvalidReference);
        }
        // Funder only: the org owes the obligation, so it may not attest its own settlement.
        if !roles::has_role(env, Role::Funder, caller) {
            panic_with_error!(env, CommonError::Unauthorized);
        }
        Self::load(env, obligation_id)
    }

    fn load(env: &Env, obligation_id: u64) -> Obligation {
        env.storage()
            .persistent()
            .get(&DataKey::Obligation(obligation_id))
            .unwrap_or_else(|| panic_with_error!(env, AdapterError::ObligationNotFound))
    }

    /// Extend every entry of `ob`: obligation, lookup and index item below 30 d to 60 d; the
    /// org's count to at least 60 d.
    fn extend_entries(env: &Env, ob: &Obligation) {
        ttl::extend_pending(env, &DataKey::Obligation(ob.id));
        ttl::extend_pending(
            env,
            &DataKey::ObligationOf(ob.payout_receiver.clone(), ob.policy_id),
        );
        ttl::extend_pending(env, &DataKey::OrgObligationAt(ob.org.clone(), ob.org_index));
        ttl::extend_shared(env, &DataKey::OrgObligationCount(ob.org.clone()), None);
    }
}

#[cfg(test)]
mod test;
