#![no_std]
//! # PolicyManager
//!
//! Soroban port of `microcrop-contracts/microcrop/src/PolicyManager.sol` (v3.1.0:
//! per-org treasury + audit batch 1-2).
//!
//! ## EVM -> Soroban mapping
//! - `AccessControl` roles -> [`microcrop_shared::roles`] registry + explicit `caller` +
//!   `require_auth()`.
//! - `initialize` / UUPS -> `__constructor` + [`PolicyManager::upgrade`]
//!   (`env.deployer().update_current_contract_wasm`).
//! - `mapping(uint256 => Policy)` -> persistent `DataKey::Policy(u64)`.
//! - `_policyCounter` -> instance `DataKey::PolicyCounter`.
//! - `_policyOrg` -> folded into `Policy.org`; `_orgOutstandingSumInsured` ->
//!   persistent `DataKey::OrgOutstanding(Address)`.
//! - `nonReentrant` -> dropped (no reentrancy in Soroban); CEI ordering kept anyway.
//! - `setLegacyPolicyOrg` and all `__gap` / `__deprecated_*` -> OMITTED (fresh chain, no legacy state).
//! - Solidity `try/catch` around the NFT status update -> the generated client's fallible
//!   `try_update_policy_status` (best-effort; never bricks the lifecycle transition — Finding 5).
//!
//! ## Storage TTL
//! Instance + persistent entries are bumped on access so live policies never expire. The
//! bump amounts are well under the test-env `max_entry_ttl` so `#[cfg(test)]` runs cleanly.

use soroban_sdk::{
    contract, contractevent, contractimpl, contracttype, panic_with_error, Address, BytesN, Env,
    String, Vec,
};

use microcrop_shared::{
    errors::PolicyError,
    interfaces::PolicyNftClient,
    roles,
    types::{CoverageType, Policy, PolicyStatus, Role},
};

// ============ Constants (ported 1:1) ============
/// 100 USDC (6dp).
pub const MIN_SUM_INSURED: i128 = 100_000_000;
/// 1,000,000 USDC (6dp).
pub const MAX_SUM_INSURED: i128 = 1_000_000_000_000;
pub const MIN_DURATION_DAYS: u32 = 30;
pub const MAX_DURATION_DAYS: u32 = 365;
pub const MAX_ACTIVE_POLICIES_PER_FARMER: u32 = 5;
pub const MAX_CLAIMS_PER_FARMER_PER_YEAR: u32 = 3;
pub const SECONDS_PER_DAY: u64 = 86_400;
/// `getCurrentYear = timestamp / (365 * 86400)`.
pub const SECONDS_PER_YEAR: u64 = 365 * 86_400;

// ---- TTL bump amounts (ledgers). ~17280 ledgers/day at 5s close. ----
const DAY_IN_LEDGERS: u32 = 17_280;
/// Bump when remaining TTL drops below ~30 days...
const BUMP_THRESHOLD: u32 = 30 * DAY_IN_LEDGERS;
/// ...extending the entry to ~90 days out (< test-env `max_entry_ttl`).
const BUMP_EXTEND_TO: u32 = 90 * DAY_IN_LEDGERS;

/// Storage keys. `PolicyNft`/`PolicyCounter` are instance; the rest are persistent.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// PolicyNft contract address (certificate minting).
    PolicyNft,
    /// Auto-increment policy id counter (u64).
    PolicyCounter,
    /// policy_id -> Policy.
    Policy(u64),
    /// farmer -> list of their policy ids.
    FarmerPolicies(Address),
    /// farmer -> count of ACTIVE policies.
    FarmerActiveCount(Address),
    /// farmer -> count of PENDING (created, not yet activated) policies.
    FarmerPendingCount(Address),
    /// (farmer, year) -> claim count.
    FarmerClaimCount(Address, u64),
    /// policy_id -> whether it was counted in the pending counter at creation.
    PendingCounted(u64),
    /// org -> outstanding (ACTIVE, unpaid) sum insured aggregated across products.
    OrgOutstanding(Address),
}

// ============ Events (ported 1:1 from the Solidity `event` declarations) ============

/// `PolicyCreated(policyId, farmer, plotId, sumInsured, premium, startDate, endDate, coverageType)`.
#[contractevent]
#[derive(Clone)]
pub struct PolicyCreated {
    #[topic]
    pub policy_id: u64,
    #[topic]
    pub farmer: Address,
    #[topic]
    pub plot_id: u64,
    pub sum_insured: i128,
    pub premium: i128,
    pub start_date: u64,
    pub end_date: u64,
    pub coverage_type: CoverageType,
}

/// `PolicyActivated(policyId, activatedAt)`.
#[contractevent]
#[derive(Clone)]
pub struct PolicyActivated {
    #[topic]
    pub policy_id: u64,
    pub activated_at: u64,
}

/// `PolicyClaimed(policyId, claimedAt)`.
#[contractevent]
#[derive(Clone)]
pub struct PolicyClaimed {
    #[topic]
    pub policy_id: u64,
    pub claimed_at: u64,
}

/// `PolicyCancelled(policyId, cancelledAt)`.
#[contractevent]
#[derive(Clone)]
pub struct PolicyCancelled {
    #[topic]
    pub policy_id: u64,
    pub cancelled_at: u64,
}

/// `PolicyExpiredByBackend(policyId, expiredAt)`.
#[contractevent]
#[derive(Clone)]
pub struct PolicyExpiredByBackend {
    #[topic]
    pub policy_id: u64,
    pub expired_at: u64,
}

/// `PolicyNFTSet(policyNFT)`.
#[contractevent]
#[derive(Clone)]
pub struct PolicyNftSet {
    #[topic]
    pub policy_nft: Address,
}

/// `PolicyNFTUpdateSkipped(policyId)` — best-effort NFT deactivation was a no-op (Finding 5).
#[contractevent]
#[derive(Clone)]
pub struct PolicyNftUpdateSkipped {
    #[topic]
    pub policy_id: u64,
}

/// `ClaimCountIncremented(farmer, year, newCount)`.
#[contractevent]
#[derive(Clone)]
pub struct ClaimCountIncremented {
    #[topic]
    pub farmer: Address,
    #[topic]
    pub year: u64,
    pub new_count: u32,
}

#[contract]
pub struct PolicyManager;

#[contractimpl]
impl PolicyManager {
    /// Port of `initialize(admin)`. Grants ADMIN + UPGRADER; zeroes the policy counter.
    pub fn __constructor(env: Env, admin: Address) {
        roles::grant_role(&env, Role::Admin, &admin);
        roles::grant_role(&env, Role::Upgrader, &admin);
        env.storage().instance().set(&DataKey::PolicyCounter, &0u64);
    }

    /// Port of `setPolicyNFT` (ADMIN). Must be set before activating policies.
    pub fn set_policy_nft(env: Env, caller: Address, policy_nft: Address) {
        roles::require_role(&env, &caller, Role::Admin);
        env.storage()
            .instance()
            .set(&DataKey::PolicyNft, &policy_nft);
        Self::bump_instance(&env);
        PolicyNftSet {
            policy_nft: policy_nft.clone(),
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

    // ---- domain logic ----

    /// Port of `createPolicy` (BACKEND). Validates farmer/org/sum/premium/duration and the
    /// combined open-policy cap (ACTIVE + PENDING <= 5); creates PENDING policy; records
    /// backing `org`; increments pending counter. Returns the new policy id.
    #[allow(clippy::too_many_arguments)]
    pub fn create_policy(
        env: Env,
        caller: Address,
        farmer: Address,
        plot_id: u64,
        sum_insured: i128,
        premium: i128,
        duration_days: u32,
        coverage_type: CoverageType,
        org: Address,
    ) -> u64 {
        roles::require_role(&env, &caller, Role::Backend);

        // NOTE: `farmer` / `org` are typed `Address`; Soroban has no zero address, so the
        // Solidity `ZeroAddressFarmer` / `ZeroAddressOrg` guards are structurally unreachable
        // (a caller can never construct address(0)). The error codes are retained in the shared
        // enum for parity, but there is nothing to check here.

        // Validate sum insured bounds.
        if sum_insured < MIN_SUM_INSURED {
            panic_with_error!(&env, PolicyError::SumInsuredTooLow);
        }
        if sum_insured > MAX_SUM_INSURED {
            panic_with_error!(&env, PolicyError::SumInsuredTooHigh);
        }

        // Validate premium.
        if premium <= 0 {
            panic_with_error!(&env, PolicyError::ZeroPremium);
        }

        // Validate duration.
        if duration_days < MIN_DURATION_DAYS || duration_days > MAX_DURATION_DAYS {
            panic_with_error!(&env, PolicyError::InvalidDuration);
        }

        // Cap TOTAL open policies (ACTIVE + PENDING) at creation time (Finding 5).
        let current_open =
            Self::farmer_active_count(&env, &farmer) + Self::farmer_pending_count(&env, &farmer);
        if current_open >= MAX_ACTIVE_POLICIES_PER_FARMER {
            panic_with_error!(&env, PolicyError::TooManyActivePolicies);
        }

        // Generate unique policy id (starts at 1; matches `++_policyCounter`).
        let policy_id: u64 = Self::policy_counter(&env) + 1;
        env.storage()
            .instance()
            .set(&DataKey::PolicyCounter, &policy_id);

        // Calculate timestamps.
        let start_date = env.ledger().timestamp();
        let end_date = start_date + (duration_days as u64) * SECONDS_PER_DAY;

        // Create and store the policy (org folded into the struct).
        let policy = Policy {
            id: policy_id,
            farmer: farmer.clone(),
            plot_id,
            sum_insured,
            premium,
            start_date,
            end_date,
            coverage_type,
            status: PolicyStatus::Pending,
            created_at: start_date,
            org: org.clone(),
        };
        Self::save_policy(&env, &policy);

        // Update the farmer's policy list.
        let mut list = Self::farmer_policies_vec(&env, &farmer);
        list.push_back(policy_id);
        env.storage()
            .persistent()
            .set(&DataKey::FarmerPolicies(farmer.clone()), &list);
        Self::bump_persistent(&env, &DataKey::FarmerPolicies(farmer.clone()));

        // Increment the pending counter and mark the policy as counted (Finding 8).
        Self::set_farmer_pending_count(&env, &farmer, current_open_pending_inc(&env, &farmer));
        env.storage()
            .persistent()
            .set(&DataKey::PendingCounted(policy_id), &true);
        Self::bump_persistent(&env, &DataKey::PendingCounted(policy_id));

        Self::bump_instance(&env);

        PolicyCreated {
            policy_id,
            farmer,
            plot_id,
            sum_insured,
            premium,
            start_date,
            end_date,
            coverage_type,
        }
        .publish(&env);

        policy_id
    }

    /// Port of `activatePolicy` (BACKEND). PENDING->ACTIVE, resets coverage window to now,
    /// moves the farmer slot pending->active, adds sum insured to org outstanding, mints the NFT.
    pub fn activate_policy(
        env: Env,
        caller: Address,
        policy_id: u64,
        distributor: Address,
        distributor_name: String,
        region: String,
    ) {
        roles::require_role(&env, &caller, Role::Backend);

        // PolicyNFT must be wired before activation.
        let policy_nft: Address = env
            .storage()
            .instance()
            .get(&DataKey::PolicyNft)
            .unwrap_or_else(|| panic_with_error!(&env, PolicyError::PolicyNftNotSet));

        // (`distributor` is `Address`; no zero-address check possible/needed — see create_policy.)

        let mut policy = Self::load_policy(&env, policy_id);

        // Must be PENDING.
        if policy.status != PolicyStatus::Pending {
            panic_with_error!(&env, PolicyError::InvalidPolicyStatus);
        }

        // The backing org is always set by create_policy on a fresh chain; the guard is kept for
        // parity (Finding 7). `Policy.org` can never be address(0) in this port.

        // Enforce the active-policy cap at activation time.
        let current_active = Self::farmer_active_count(&env, &policy.farmer);
        if current_active >= MAX_ACTIVE_POLICIES_PER_FARMER {
            panic_with_error!(&env, PolicyError::TooManyActivePolicies);
        }

        // Activate — reset coverage window to start now, preserving the chosen duration.
        let now = env.ledger().timestamp();
        let duration = policy.end_date - policy.start_date;
        policy.status = PolicyStatus::Active;
        policy.start_date = now;
        policy.end_date = now + duration;
        Self::save_policy(&env, &policy);

        // Move the farmer's slot PENDING -> ACTIVE.
        Self::set_farmer_active_count(&env, &policy.farmer, current_active + 1);
        if env
            .storage()
            .persistent()
            .get(&DataKey::PendingCounted(policy_id))
            .unwrap_or(false)
        {
            env.storage()
                .persistent()
                .remove(&DataKey::PendingCounted(policy_id));
            let pending = Self::farmer_pending_count(&env, &policy.farmer);
            if pending > 0 {
                Self::set_farmer_pending_count(&env, &policy.farmer, pending - 1);
            }
        }

        // ACTIVE coverage adds to the org's outstanding exposure (drives Treasury solvency).
        let outstanding = Self::org_outstanding(&env, &policy.org);
        Self::set_org_outstanding(&env, &policy.org, outstanding + policy.sum_insured);

        // Mint the NFT certificate to the farmer. PolicyManager acts as itself (Minter).
        let nft = PolicyNftClient::new(&env, &policy_nft);
        nft.mint_policy(
            &env.current_contract_address(),
            &policy.farmer,
            &policy_id,
            &distributor,
            &distributor_name,
            &policy.sum_insured,
            &policy.premium,
            &policy.start_date,
            &policy.end_date,
            &policy.coverage_type,
            &region,
            &policy.plot_id,
        );

        Self::bump_instance(&env);

        PolicyActivated {
            policy_id,
            activated_at: now,
        }
        .publish(&env);
    }

    /// Port of `markAsClaimed` (ORACLE = PayoutReceiver). ACTIVE->CLAIMED, decrements active
    /// count, releases org exposure, deactivates NFT (best-effort).
    pub fn mark_as_claimed(env: Env, caller: Address, policy_id: u64) {
        roles::require_role(&env, &caller, Role::Oracle);

        let mut policy = Self::load_policy(&env, policy_id);

        if policy.status != PolicyStatus::Active {
            panic_with_error!(&env, PolicyError::InvalidPolicyStatus);
        }
        if env.ledger().timestamp() > policy.end_date {
            panic_with_error!(&env, PolicyError::PolicyExpired);
        }

        policy.status = PolicyStatus::Claimed;
        Self::save_policy(&env, &policy);

        Self::decrement_active(&env, &policy.farmer);
        Self::release_org_exposure(&env, &policy.org, policy.sum_insured);
        Self::deactivate_policy_nft(&env, policy_id);

        Self::bump_instance(&env);
        PolicyClaimed {
            policy_id,
            claimed_at: env.ledger().timestamp(),
        }
        .publish(&env);
    }

    /// Port of `incrementClaimCount` (ORACLE). Enforces <= 3 claims/farmer/year.
    pub fn increment_claim_count(env: Env, caller: Address, farmer: Address) {
        roles::require_role(&env, &caller, Role::Oracle);

        let year = env.ledger().timestamp() / SECONDS_PER_YEAR;
        let current = Self::farmer_claim_count(&env, &farmer, year);
        let new_count = current + 1;
        if new_count > MAX_CLAIMS_PER_FARMER_PER_YEAR {
            panic_with_error!(&env, PolicyError::TooManyClaimsThisYear);
        }
        env.storage()
            .persistent()
            .set(&DataKey::FarmerClaimCount(farmer.clone(), year), &new_count);
        Self::bump_persistent(&env, &DataKey::FarmerClaimCount(farmer.clone(), year));

        ClaimCountIncremented {
            farmer,
            year,
            new_count,
        }
        .publish(&env);
    }

    /// Port of `cancelPolicy` (BACKEND). PENDING/ACTIVE -> CANCELLED with the matching
    /// counter/exposure/NFT bookkeeping.
    pub fn cancel_policy(env: Env, caller: Address, policy_id: u64) {
        roles::require_role(&env, &caller, Role::Backend);

        let mut policy = Self::load_policy(&env, policy_id);

        if policy.status != PolicyStatus::Pending && policy.status != PolicyStatus::Active {
            panic_with_error!(&env, PolicyError::InvalidPolicyStatus);
        }

        if policy.status == PolicyStatus::Active {
            Self::decrement_active(&env, &policy.farmer);
            Self::release_org_exposure(&env, &policy.org, policy.sum_insured);
            Self::deactivate_policy_nft(&env, policy_id);
        } else {
            // PENDING: release its reserved open-policy slot (only if it was counted — Finding 8).
            if env
                .storage()
                .persistent()
                .get(&DataKey::PendingCounted(policy_id))
                .unwrap_or(false)
            {
                env.storage()
                    .persistent()
                    .remove(&DataKey::PendingCounted(policy_id));
                let pending = Self::farmer_pending_count(&env, &policy.farmer);
                if pending > 0 {
                    Self::set_farmer_pending_count(&env, &policy.farmer, pending - 1);
                }
            }
        }

        policy.status = PolicyStatus::Cancelled;
        Self::save_policy(&env, &policy);

        Self::bump_instance(&env);
        PolicyCancelled {
            policy_id,
            cancelled_at: env.ledger().timestamp(),
        }
        .publish(&env);
    }

    /// Port of `expirePolicy` (BACKEND). ACTIVE -> EXPIRED once past end date.
    pub fn expire_policy(env: Env, caller: Address, policy_id: u64) {
        roles::require_role(&env, &caller, Role::Backend);

        let mut policy = Self::load_policy(&env, policy_id);

        if policy.status != PolicyStatus::Active {
            panic_with_error!(&env, PolicyError::InvalidPolicyStatus);
        }
        if env.ledger().timestamp() <= policy.end_date {
            panic_with_error!(&env, PolicyError::PolicyNotYetExpired);
        }

        Self::decrement_active(&env, &policy.farmer);
        Self::release_org_exposure(&env, &policy.org, policy.sum_insured);
        Self::deactivate_policy_nft(&env, policy_id);

        policy.status = PolicyStatus::Expired;
        Self::save_policy(&env, &policy);

        Self::bump_instance(&env);
        PolicyExpiredByBackend {
            policy_id,
            expired_at: env.ledger().timestamp(),
        }
        .publish(&env);
    }

    // ---- views ----

    /// Port of `getPolicy`. Reverts `PolicyDoesNotExist` if unknown.
    pub fn get_policy(env: Env, policy_id: u64) -> Policy {
        Self::load_policy(&env, policy_id)
    }

    /// Port of `getFarmerPolicies`.
    pub fn get_farmer_policies(env: Env, farmer: Address) -> Vec<u64> {
        Self::farmer_policies_vec(&env, &farmer)
    }

    /// Port of `getFarmerActiveCount`.
    pub fn get_farmer_active_count(env: Env, farmer: Address) -> u32 {
        Self::farmer_active_count(&env, &farmer)
    }

    /// Port of `getFarmerClaimCount`.
    pub fn get_farmer_claim_count(env: Env, farmer: Address, year: u64) -> u32 {
        Self::farmer_claim_count(&env, &farmer, year)
    }

    /// Port of `canFarmerClaim`.
    pub fn can_farmer_claim(env: Env, farmer: Address) -> bool {
        let year = env.ledger().timestamp() / SECONDS_PER_YEAR;
        Self::farmer_claim_count(&env, &farmer, year) < MAX_CLAIMS_PER_FARMER_PER_YEAR
    }

    /// Port of `isPolicyActive`.
    pub fn is_policy_active(env: Env, policy_id: u64) -> bool {
        match env
            .storage()
            .persistent()
            .get::<_, Policy>(&DataKey::Policy(policy_id))
        {
            Some(p) => {
                p.status == PolicyStatus::Active && env.ledger().timestamp() <= p.end_date
            }
            None => false,
        }
    }

    /// Port of `policyExists`.
    pub fn policy_exists(env: Env, policy_id: u64) -> bool {
        env.storage()
            .persistent()
            .has(&DataKey::Policy(policy_id))
    }

    /// Port of `policyOrg`. Reverts `PolicyDoesNotExist` for an unknown policy (Soroban has no
    /// zero address to return in the not-found case — see report note).
    pub fn policy_org(env: Env, policy_id: u64) -> Address {
        Self::load_policy(&env, policy_id).org
    }

    /// Port of `orgOutstandingSumInsured`.
    pub fn org_outstanding_sum_insured(env: Env, org: Address) -> i128 {
        Self::org_outstanding(&env, &org)
    }

    /// Port of `getTotalPolicies` (the counter).
    pub fn get_total_policies(env: Env) -> u64 {
        Self::policy_counter(&env)
    }

    /// Port of `getCurrentYear` = `timestamp / (365 * 86400)`.
    pub fn get_current_year(env: Env) -> u64 {
        env.ledger().timestamp() / SECONDS_PER_YEAR
    }

    // ============ internal helpers ============

    fn policy_counter(env: &Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::PolicyCounter)
            .unwrap_or(0)
    }

    fn save_policy(env: &Env, policy: &Policy) {
        env.storage()
            .persistent()
            .set(&DataKey::Policy(policy.id), policy);
        Self::bump_persistent(env, &DataKey::Policy(policy.id));
    }

    fn load_policy(env: &Env, policy_id: u64) -> Policy {
        match env.storage().persistent().get(&DataKey::Policy(policy_id)) {
            Some(p) => {
                Self::bump_persistent(env, &DataKey::Policy(policy_id));
                p
            }
            None => panic_with_error!(env, PolicyError::PolicyDoesNotExist),
        }
    }

    fn farmer_policies_vec(env: &Env, farmer: &Address) -> Vec<u64> {
        env.storage()
            .persistent()
            .get(&DataKey::FarmerPolicies(farmer.clone()))
            .unwrap_or_else(|| Vec::new(env))
    }

    fn farmer_active_count(env: &Env, farmer: &Address) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::FarmerActiveCount(farmer.clone()))
            .unwrap_or(0)
    }

    fn set_farmer_active_count(env: &Env, farmer: &Address, v: u32) {
        env.storage()
            .persistent()
            .set(&DataKey::FarmerActiveCount(farmer.clone()), &v);
        Self::bump_persistent(env, &DataKey::FarmerActiveCount(farmer.clone()));
    }

    fn farmer_pending_count(env: &Env, farmer: &Address) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::FarmerPendingCount(farmer.clone()))
            .unwrap_or(0)
    }

    fn set_farmer_pending_count(env: &Env, farmer: &Address, v: u32) {
        env.storage()
            .persistent()
            .set(&DataKey::FarmerPendingCount(farmer.clone()), &v);
        Self::bump_persistent(env, &DataKey::FarmerPendingCount(farmer.clone()));
    }

    fn farmer_claim_count(env: &Env, farmer: &Address, year: u64) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::FarmerClaimCount(farmer.clone(), year))
            .unwrap_or(0)
    }

    fn org_outstanding(env: &Env, org: &Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::OrgOutstanding(org.clone()))
            .unwrap_or(0)
    }

    fn set_org_outstanding(env: &Env, org: &Address, v: i128) {
        env.storage()
            .persistent()
            .set(&DataKey::OrgOutstanding(org.clone()), &v);
        Self::bump_persistent(env, &DataKey::OrgOutstanding(org.clone()));
    }

    /// Decrement the farmer's ACTIVE count, guarding underflow (matches the Solidity `> 0` guard).
    fn decrement_active(env: &Env, farmer: &Address) {
        let c = Self::farmer_active_count(env, farmer);
        if c > 0 {
            Self::set_farmer_active_count(env, farmer, c - 1);
        }
    }

    /// Release an org's outstanding exposure when a policy leaves ACTIVE coverage. Saturating at 0.
    fn release_org_exposure(env: &Env, org: &Address, sum_insured: i128) {
        let outstanding = Self::org_outstanding(env, org);
        let next = if outstanding > sum_insured {
            outstanding - sum_insured
        } else {
            0
        };
        Self::set_org_outstanding(env, org, next);
    }

    /// Best-effort NFT deactivation (Finding 5). Any sub-call failure is swallowed and surfaced
    /// as `PolicyNftUpdateSkipped` rather than reverting the lifecycle transition. Uses the
    /// generated client's fallible `try_*` variant (the Soroban equivalent of Solidity try/catch).
    fn deactivate_policy_nft(env: &Env, policy_id: u64) {
        let policy_nft: Option<Address> = env.storage().instance().get(&DataKey::PolicyNft);
        match policy_nft {
            Some(addr) => {
                let nft = PolicyNftClient::new(env, &addr);
                if nft.policy_nft_exists(&policy_id) {
                    let res = nft.try_update_policy_status(
                        &env.current_contract_address(),
                        &policy_id,
                        &false,
                    );
                    if res.is_err() {
                        PolicyNftUpdateSkipped { policy_id }.publish(env);
                    }
                } else {
                    PolicyNftUpdateSkipped { policy_id }.publish(env);
                }
            }
            None => {
                PolicyNftUpdateSkipped { policy_id }.publish(env);
            }
        }
    }

    fn bump_instance(env: &Env) {
        env.storage()
            .instance()
            .extend_ttl(BUMP_THRESHOLD, BUMP_EXTEND_TO);
    }

    fn bump_persistent(env: &Env, key: &DataKey) {
        env.storage()
            .persistent()
            .extend_ttl(key, BUMP_THRESHOLD, BUMP_EXTEND_TO);
    }
}

/// Compute the incremented pending count for a farmer (read current + 1). Kept as a free fn so the
/// borrow of `env` in `create_policy` stays clear.
fn current_open_pending_inc(env: &Env, farmer: &Address) -> u32 {
    env.storage()
        .persistent()
        .get(&DataKey::FarmerPendingCount(farmer.clone()))
        .unwrap_or(0u32)
        + 1
}

#[cfg(test)]
mod test;
