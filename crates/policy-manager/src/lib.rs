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
//! ## Storage TTL (rent cap, `microcrop_shared::ttl`)
//! - Instance + code: every call extends below 3 days to 10 days; the permissionless keeper
//!   `extend_contract_ttl()` extends below 23 days to 30 days (and PolicyNft's too).
//! - Per-policy entries (`Policy`, `PendingCounted`, the farmer index item) live until
//!   `end_date + 30 d` (clamped to the network maximum). Activation moves the end and
//!   re-applies it to `Policy` and the shared per-farmer / per-org entries (counts included);
//!   the farmer index item (`FarmerPolicyAt`) keeps its create-time need, because the policy
//!   does not record its index position. The new end is later by the time the policy spent
//!   PENDING, so the item can archive up to that long before the policy's new need; paging
//!   it then needs a restore (it fails closed, never reads as absent). A closed policy keeps the TTL
//!   it has. `Policy` lives 60 days past every touch while CLAIM_PENDING.
//! - Shared per-farmer / per-org entries take the longest need of any policy that touches
//!   them, and at least 60 days; `FarmerClaimTimes` lives 365 days past the last claim.
//! - `extend_policy_ttl(policy_id)` is the permissionless keep-alive for one policy.

use soroban_sdk::{
    contract, contractevent, contractimpl, contracttype, panic_with_error, Address, BytesN, Env,
    String, Vec,
};

use microcrop_shared::{
    errors::{CommonError, PolicyError},
    interfaces::PolicyNftClient,
    roles, ttl,
    types::{CoverageType, Policy, PolicyStatus, Role},
};

// ============ Constants (ported 1:1) ============
/// 100 USDC in base units. Stellar USDC (a Stellar Asset Contract) has **7 decimals**, so
/// 1 USDC = 10_000_000. The EVM source used 6dp (`100_000_000`); that value is 10 USDC here.
pub const MIN_SUM_INSURED: i128 = 1_000_000_000;
/// 1,000,000 USDC in base units (7dp).
pub const MAX_SUM_INSURED: i128 = 10_000_000_000_000;
pub const MIN_DURATION_DAYS: u32 = 30;
/// Shortest Drought cover that can ever pay. A drought determination settles only if the
/// season's 110-day window (`drought::SEASON_WINDOW_SECS`) ends by `end_date`, the season
/// starts no earlier than the UTC day cover started, and the assessment (which needs the
/// rainfall archive, about 7 days behind real time) lands by `end_date`. So 110 + 7 = 117
/// days is the floor below which a Drought policy takes a premium and can never pay. It is
/// a floor, not the recommended sale: sell ~140 days (operations margin) and activate by
/// planting.
pub const MIN_DROUGHT_DURATION_DAYS: u32 = 117;
pub const MAX_DURATION_DAYS: u32 = 365;
/// Cap on a farmer's open policies (PENDING + ACTIVE) PLUS claims paid in the rolling claim
/// window. Equal to the claim cap, so every policy a farmer pays a premium on can still pay
/// out (audit finding 3; was 5 open against 3 claims).
pub const MAX_ACTIVE_POLICIES_PER_FARMER: u32 = 3;
/// Claims per farmer in any rolling [`CLAIM_WINDOW_SECS`] window.
pub const MAX_CLAIMS_PER_FARMER_PER_YEAR: u32 = 3;
pub const SECONDS_PER_DAY: u64 = 86_400;
/// Hard maximum page size for the paginated index views. Each id is its own ledger entry,
/// and a Soroban transaction (simulation included) may touch at most 100 footprint entries;
/// a page of 100 plus the count and the instance overflows that, so pages stop at 50.
pub const MAX_PAGE_SIZE: u32 = 50;
/// Largest valid `|lat_e6|` (90 degrees x 1e6).
pub const MAX_LAT_E6: i64 = 90_000_000;
/// Largest valid `|lon_e6|` (180 degrees x 1e6).
pub const MAX_LON_E6: i64 = 180_000_000;
/// Rolling claim window: 365 days. A claim counts against the cap until it is exactly this
/// old. (The EVM's fixed `timestamp / 365d` bucket let six claims pay across a bucket edge.)
pub const CLAIM_WINDOW_SECS: u64 = 365 * SECONDS_PER_DAY;

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
    /// farmer -> number of policies ever created for them (length of the index below).
    FarmerPolicyCount(Address),
    /// (farmer, index) -> policy id. One entry per policy: a single `Vec` entry rewritten on
    /// every create hit the entry-size cap after ~5,450 policies and then blocked
    /// `create_policy` for a shared farmer address (audit finding 10).
    FarmerPolicyAt(Address, u32),
    /// farmer -> count of ACTIVE policies.
    FarmerActiveCount(Address),
    /// farmer -> count of PENDING (created, not yet activated) policies.
    FarmerPendingCount(Address),
    /// farmer -> timestamps of their most recent claims, oldest first, at most
    /// `MAX_CLAIMS_PER_FARMER_PER_YEAR` long (bounded; the oldest is dropped on overflow).
    FarmerClaimTimes(Address),
    /// policy_id -> whether it was counted in the pending counter at creation.
    PendingCounted(u64),
    /// org -> outstanding (PENDING + ACTIVE, unpaid) sum insured aggregated across products.
    /// Counted from `create_policy` so a premium paid on a PENDING policy is reserve, not
    /// withdrawable surplus (audit finding 4); released exactly once when the policy leaves
    /// PENDING/ACTIVE (cancel, expire, claim).
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

/// A verified claim is waiting for the org reserve (ACTIVE -> CLAIM_PENDING).
#[contractevent]
#[derive(Clone)]
pub struct PolicyClaimPending {
    #[topic]
    pub policy_id: u64,
    pub recorded_at: u64,
}

/// `PolicyCancelled(policyId, cancelledAt)`.
#[contractevent]
#[derive(Clone)]
pub struct PolicyCancelled {
    #[topic]
    pub policy_id: u64,
    pub cancelled_at: u64,
}

/// `PolicyExpired(policyId, expiredAt)` — anyone may expire an ended policy.
#[contractevent]
#[derive(Clone)]
pub struct PolicyExpired {
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

/// `ClaimCountIncremented(farmer, claimedAt, claimsInWindow)` — `claims_in_window` counts
/// this claim and every other one in the rolling 365-day window ending now.
#[contractevent]
#[derive(Clone)]
pub struct ClaimCountIncremented {
    #[topic]
    pub farmer: Address,
    pub claimed_at: u64,
    pub claims_in_window: u32,
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

    /// PERMISSIONLESS keeper: extend this contract's instance and code, and PolicyNft's,
    /// below 23 days to 30 days (a no-op above). Ordinary calls only extend below 3 days to
    /// 10 days, so this is what keeps the contracts alive; the backend runs it daily
    /// (through `PayoutReceiver::extend_contract_ttl`). Never paused-gated.
    pub fn extend_contract_ttl(env: Env) {
        ttl::keep_contract_alive(&env);
        let policy_nft: Option<Address> = env.storage().instance().get(&DataKey::PolicyNft);
        if let Some(addr) = policy_nft {
            ttl::keep_other_contract_alive(&env, &addr);
        }
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

    /// Port of `createPolicy` (BACKEND). Validates farmer/org/sum/premium/duration, the plot
    /// coordinates and the combined open-policy cap; creates PENDING policy; records backing
    /// `org`; increments pending counter. Returns the new policy id.
    ///
    /// `lat_e6` / `lon_e6` are the insured plot's coordinates in degrees x 1e6, rounded the
    /// way the Lit signer rounds them (`Math.round(deg * 1e6)`). PayoutReceiver only accepts
    /// a determination whose signed coordinates match them (no EVM equivalent).
    #[allow(clippy::too_many_arguments)]
    pub fn create_policy(
        env: Env,
        caller: Address,
        farmer: Address,
        plot_id: u64,
        lat_e6: i64,
        lon_e6: i64,
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

        // Validate the plot coordinates (degrees x 1e6).
        if !(-MAX_LAT_E6..=MAX_LAT_E6).contains(&lat_e6)
            || !(-MAX_LON_E6..=MAX_LON_E6).contains(&lon_e6)
        {
            panic_with_error!(&env, PolicyError::InvalidCoordinates);
        }

        // Only drought cover is sold: the one methodology the signer can sign
        // (`crop-drought-rdi-1.0`) indemnifies drought and nothing else, so any other coverage
        // type could take a premium and never pay (PayoutReceiver keeps the same check as
        // defence in depth). Re-opening `Comprehensive` needs an audited upgrade once a
        // cause-agnostic methodology is live again.
        //
        // The drought path settles only if the season's 110-day window lies inside the cover
        // and is assessed by `end_date` (rainfall archive ~7 days behind), so Drought cover
        // shorter than MIN_DROUGHT_DURATION_DAYS (117) could take a premium and never pay:
        // refused here. Sale-side rule (not enforced): sell ~140 days of cover (operations
        // margin) and activate on or before the planting day.
        if coverage_type != CoverageType::Drought {
            panic_with_error!(&env, PolicyError::CoverageNotOffered);
        }
        if duration_days < MIN_DROUGHT_DURATION_DAYS {
            panic_with_error!(&env, PolicyError::InvalidDuration);
        }

        // Cap open policies (ACTIVE + PENDING) PLUS claims already paid in the rolling window.
        // Each open policy can become at most one claim, so keeping this sum <= the claim cap
        // means every policy the farmer pays for can still pay out (audit finding 3).
        let current_open =
            Self::farmer_active_count(&env, &farmer) + Self::farmer_pending_count(&env, &farmer);
        let recent_claims = Self::claims_in_window(&env, &farmer);
        if current_open + recent_claims >= MAX_ACTIVE_POLICIES_PER_FARMER {
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
            lat_e6,
            lon_e6,
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

        // Append to the farmer's policy index: one entry per policy + a count.
        let count_key = DataKey::FarmerPolicyCount(farmer.clone());
        let n: u32 = env.storage().persistent().get(&count_key).unwrap_or(0);
        let item_key = DataKey::FarmerPolicyAt(farmer.clone(), n);
        env.storage().persistent().set(&item_key, &policy_id);
        env.storage().persistent().set(&count_key, &(n + 1));
        ttl::extend_for_policy(&env, &item_key, end_date);
        ttl::extend_shared(&env, &count_key, Some(end_date));

        // The org's exposure starts now, not at activation: the premium can arrive while the
        // policy is PENDING, and until it is counted here the org could withdraw it as surplus
        // and the policy would activate with no reserve behind it (audit finding 4).
        let outstanding = Self::org_outstanding(&env, &org);
        Self::set_org_outstanding(&env, &org, outstanding + sum_insured, end_date);

        // Increment the pending counter and mark the policy as counted (Finding 8).
        Self::set_farmer_pending_count(
            &env,
            &farmer,
            current_open_pending_inc(&env, &farmer),
            end_date,
        );
        env.storage()
            .persistent()
            .set(&DataKey::PendingCounted(policy_id), &true);
        ttl::extend_for_policy(&env, &DataKey::PendingCounted(policy_id), end_date);

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
    /// moves the farmer slot pending->active, mints the NFT. Org exposure is unchanged: it was
    /// counted at `create_policy`.
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
        // A PENDING policy past its end date is expirable by anyone; activating it instead
        // would revive cover the org's exposure may already have been released for.
        if env.ledger().timestamp() > policy.end_date {
            panic_with_error!(&env, PolicyError::PolicyExpired);
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
        Self::set_farmer_active_count(&env, &policy.farmer, current_active + 1, policy.end_date);
        Self::release_pending_slot(&env, policy_id, &policy.farmer, policy.end_date);
        // The end moved: the org's exposure entry and the farmer's index count must outlive
        // the new end too.
        for key in [
            DataKey::OrgOutstanding(policy.org.clone()),
            DataKey::FarmerPolicyCount(policy.farmer.clone()),
        ] {
            if env.storage().persistent().has(&key) {
                ttl::extend_shared(&env, &key, Some(policy.end_date));
            }
        }

        // (The org's outstanding exposure was already counted at create_policy.)

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

    /// Port of `markAsClaimed` (ORACLE = PayoutReceiver). ACTIVE or CLAIM_PENDING -> CLAIMED,
    /// deactivates NFT (best-effort). From ACTIVE it also decrements the active count and
    /// releases the org exposure; from CLAIM_PENDING those were already released by
    /// `mark_claim_pending`, so they are not released twice.
    ///
    /// An ACTIVE policy must still be within its coverage window. A CLAIM_PENDING policy is
    /// not checked against `end_date`: its determination was verified inside the window and
    /// the claim is only waiting for the org reserve, so it must not lapse.
    pub fn mark_as_claimed(env: Env, caller: Address, policy_id: u64) {
        roles::require_role(&env, &caller, Role::Oracle);

        let mut policy = Self::load_policy(&env, policy_id);

        let was_active = match policy.status {
            PolicyStatus::Active => {
                if env.ledger().timestamp() > policy.end_date {
                    panic_with_error!(&env, PolicyError::PolicyExpired);
                }
                true
            }
            PolicyStatus::ClaimPending => false,
            _ => panic_with_error!(&env, PolicyError::InvalidPolicyStatus),
        };

        policy.status = PolicyStatus::Claimed;
        Self::save_policy(&env, &policy);

        if was_active {
            Self::decrement_active(&env, &policy.farmer, policy.end_date);
            Self::release_org_exposure(&env, &policy.org, policy.sum_insured, policy.end_date);
        }
        Self::deactivate_policy_nft(&env, policy_id);

        Self::bump_instance(&env);
        PolicyClaimed {
            policy_id,
            claimed_at: env.ledger().timestamp(),
        }
        .publish(&env);
    }

    /// ORACLE (PayoutReceiver). ACTIVE -> CLAIM_PENDING: a valid determination was verified
    /// but the org reserve cannot pay it yet. The policy keeps its soulbound certificate until
    /// `mark_as_claimed` settles it, and it cannot be expired or cancelled meanwhile. No port
    /// equivalent in Solidity (audit findings 9/11).
    ///
    /// The active slot and the org exposure are released HERE, not at settlement. The claim
    /// itself already counts against the farmer's cap (as a claim in the window) and against
    /// the org's required reserve (as a pending payout of the full amount, in Treasury), so
    /// keeping the slot and the exposure too would count the same policy twice.
    pub fn mark_claim_pending(env: Env, caller: Address, policy_id: u64) {
        roles::require_role(&env, &caller, Role::Oracle);

        let mut policy = Self::load_policy(&env, policy_id);
        if policy.status != PolicyStatus::Active {
            panic_with_error!(&env, PolicyError::InvalidPolicyStatus);
        }
        if env.ledger().timestamp() > policy.end_date {
            panic_with_error!(&env, PolicyError::PolicyExpired);
        }

        policy.status = PolicyStatus::ClaimPending;
        Self::save_policy(&env, &policy);

        Self::decrement_active(&env, &policy.farmer, policy.end_date);
        Self::release_org_exposure(&env, &policy.org, policy.sum_insured, policy.end_date);

        Self::bump_instance(&env);
        PolicyClaimPending {
            policy_id,
            recorded_at: env.ledger().timestamp(),
        }
        .publish(&env);
    }

    /// Port of `incrementClaimCount` (ORACLE). Enforces <= 3 claims per farmer in any
    /// rolling 365-day window: rejects while the oldest of the farmer's last three claims is
    /// less than [`CLAIM_WINDOW_SECS`] old.
    pub fn increment_claim_count(env: Env, caller: Address, farmer: Address) {
        roles::require_role(&env, &caller, Role::Oracle);

        let now = env.ledger().timestamp();
        let in_window = Self::claims_in_window(&env, &farmer);
        if in_window >= MAX_CLAIMS_PER_FARMER_PER_YEAR {
            panic_with_error!(&env, PolicyError::TooManyClaimsThisYear);
        }

        let key = DataKey::FarmerClaimTimes(farmer.clone());
        let mut times = Self::claim_times(&env, &farmer);
        times.push_back(now);
        while times.len() > MAX_CLAIMS_PER_FARMER_PER_YEAR {
            times.pop_front();
        }
        env.storage().persistent().set(&key, &times);
        ttl::extend_for_secs(&env, &key, ttl::CLAIM_HISTORY_SECS);

        ClaimCountIncremented {
            farmer,
            claimed_at: now,
            claims_in_window: in_window + 1,
        }
        .publish(&env);
    }

    /// Port of `cancelPolicy`. PENDING/ACTIVE -> CANCELLED with the matching
    /// counter/exposure/NFT bookkeeping.
    ///
    /// `Role::Backend` may cancel a PENDING policy only. Cancelling ACTIVE cover needs
    /// `Role::Admin` (an operational escape hatch): otherwise the org's staff could cancel a
    /// policy through the backend after a loss and keep the payout (audit finding 6).
    pub fn cancel_policy(env: Env, caller: Address, policy_id: u64) {
        caller.require_auth();
        let is_admin = roles::has_role(&env, Role::Admin, &caller);
        if !is_admin && !roles::has_role(&env, Role::Backend, &caller) {
            panic_with_error!(&env, CommonError::Unauthorized);
        }

        let mut policy = Self::load_policy(&env, policy_id);

        if policy.status != PolicyStatus::Pending && policy.status != PolicyStatus::Active {
            panic_with_error!(&env, PolicyError::InvalidPolicyStatus);
        }
        if policy.status == PolicyStatus::Active && !is_admin {
            panic_with_error!(&env, PolicyError::CancelActiveRequiresAdmin);
        }

        // Both PENDING and ACTIVE policies carry org exposure (counted at create_policy).
        Self::release_org_exposure(&env, &policy.org, policy.sum_insured, policy.end_date);

        if policy.status == PolicyStatus::Active {
            Self::decrement_active(&env, &policy.farmer, policy.end_date);
            Self::deactivate_policy_nft(&env, policy_id);
        } else {
            // PENDING: release its reserved open-policy slot (only if it was counted — Finding 8).
            Self::release_pending_slot(&env, policy_id, &policy.farmer, policy.end_date);
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

    /// Port of `expirePolicy`. ACTIVE or PENDING -> EXPIRED once past end date.
    ///
    /// Permissionless: the `now > end_date` check is the only gate, so the org's reserve and
    /// the farmer's slot are not held hostage to the backend acting (audit finding 7).
    /// A PENDING policy's `end_date` is its creation time plus its duration, so one that was
    /// never activated also lapses: its org exposure (counted at `create_policy`) and its
    /// pending slot are released, and it can no longer be activated.
    /// Idempotent: expiring an already-EXPIRED policy is a no-op (no second release, no
    /// event); any other status reverts `InvalidPolicyStatus`.
    pub fn expire_policy(env: Env, policy_id: u64) {
        let mut policy = Self::load_policy(&env, policy_id);

        if policy.status == PolicyStatus::Expired {
            return;
        }
        if policy.status != PolicyStatus::Active && policy.status != PolicyStatus::Pending {
            panic_with_error!(&env, PolicyError::InvalidPolicyStatus);
        }
        if env.ledger().timestamp() <= policy.end_date {
            panic_with_error!(&env, PolicyError::PolicyNotYetExpired);
        }

        if policy.status == PolicyStatus::Active {
            Self::decrement_active(&env, &policy.farmer, policy.end_date);
            Self::deactivate_policy_nft(&env, policy_id);
        } else {
            Self::release_pending_slot(&env, policy_id, &policy.farmer, policy.end_date);
        }
        Self::release_org_exposure(&env, &policy.org, policy.sum_insured, policy.end_date);

        policy.status = PolicyStatus::Expired;
        Self::save_policy(&env, &policy);

        Self::bump_instance(&env);
        PolicyExpired {
            policy_id,
            expired_at: env.ledger().timestamp(),
        }
        .publish(&env);
    }

    /// PERMISSIONLESS keep-alive: extend every PolicyManager entry of `policy_id` to its
    /// need — `Policy` and `PendingCounted` to `end_date + 30 d` (clamped to the network
    /// maximum), or `Policy` 60 days while CLAIM_PENDING — plus the farmer and org entries the
    /// policy depends on, then, best effort, the policy's certificate in PolicyNft. A policy
    /// whose need is past the ~180-day network maximum (or a pending claim) needs this call
    /// again before its entries' TTL runs out; the backend keeper calls it (through
    /// `PayoutReceiver::extend_policy_ttl`) for open or pending policies whose shortest TTL is
    /// under 30 days. Reverts `PolicyDoesNotExist` for an unknown policy.
    pub fn extend_policy_ttl(env: Env, policy_id: u64) {
        let policy = Self::load_policy(&env, policy_id);
        let key = DataKey::Policy(policy_id);
        if policy.status == PolicyStatus::ClaimPending {
            ttl::extend_pending(&env, &key);
        } else {
            ttl::extend_for_policy(&env, &key, policy.end_date);
        }
        let p = env.storage().persistent();
        if p.has(&DataKey::PendingCounted(policy_id)) {
            ttl::extend_for_policy(&env, &DataKey::PendingCounted(policy_id), policy.end_date);
        }
        // The per-farmer and per-org entries this policy's create, claim and release paths read.
        for key in [
            DataKey::FarmerActiveCount(policy.farmer.clone()),
            DataKey::FarmerPendingCount(policy.farmer.clone()),
            DataKey::FarmerPolicyCount(policy.farmer.clone()),
            DataKey::OrgOutstanding(policy.org.clone()),
        ] {
            if p.has(&key) {
                ttl::extend_shared(&env, &key, Some(policy.end_date));
            }
        }
        let claims = DataKey::FarmerClaimTimes(policy.farmer.clone());
        if p.has(&claims) {
            ttl::extend_for_secs(&env, &claims, ttl::CLAIM_HISTORY_SECS);
        }
        Self::bump_instance(&env);
        let policy_nft: Option<Address> = env.storage().instance().get(&DataKey::PolicyNft);
        if let Some(addr) = policy_nft {
            // Best effort: a failing NFT call must not block the policy's own keep-alive.
            let _ = PolicyNftClient::new(&env, &addr).try_extend_policy_ttl(&policy_id);
        }
    }

    // ---- views ----

    /// Port of `getPolicy`. Reverts `PolicyDoesNotExist` if unknown.
    pub fn get_policy(env: Env, policy_id: u64) -> Policy {
        Self::load_policy(&env, policy_id)
    }

    /// Port of `getFarmerPolicies`, paginated: up to `limit` policy ids starting at index
    /// `start`, in creation order. `limit` is capped at [`MAX_PAGE_SIZE`]; a `start` past the
    /// end returns an empty list. Use [`Self::get_farmer_policy_count`] for the total.
    pub fn get_farmer_policies(env: Env, farmer: Address, start: u32, limit: u32) -> Vec<u64> {
        let p = env.storage().persistent();
        let count: u32 = p
            .get(&DataKey::FarmerPolicyCount(farmer.clone()))
            .unwrap_or(0);
        let end = start.saturating_add(limit.min(MAX_PAGE_SIZE)).min(count);
        let mut out = Vec::new(&env);
        let mut i = start;
        while i < end {
            let key = DataKey::FarmerPolicyAt(farmer.clone(), i);
            let id: u64 = p
                .get(&key)
                .unwrap_or_else(|| panic_with_error!(&env, PolicyError::PolicyDoesNotExist));
            // Index items live as long as their policy's entries (`end_date + 30 d`); paging
            // an older item needs it restored first (the RPC simulation adds the restore).
            out.push_back(id);
            i += 1;
        }
        out
    }

    /// Number of policies ever created for `farmer` (the length of the paged index).
    pub fn get_farmer_policy_count(env: Env, farmer: Address) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::FarmerPolicyCount(farmer))
            .unwrap_or(0)
    }

    /// Port of `getFarmerActiveCount`.
    pub fn get_farmer_active_count(env: Env, farmer: Address) -> u32 {
        Self::farmer_active_count(&env, &farmer)
    }

    /// Claims by `farmer` in the rolling 365-day window ending now. (The EVM took a `year`
    /// bucket; the window replaces it.)
    pub fn get_farmer_claim_count(env: Env, farmer: Address) -> u32 {
        Self::claims_in_window(&env, &farmer)
    }

    /// Timestamps of the farmer's most recent claims, oldest first (at most 3).
    pub fn get_farmer_claim_times(env: Env, farmer: Address) -> Vec<u64> {
        Self::claim_times(&env, &farmer)
    }

    /// Port of `canFarmerClaim`. Uses the same rolling-window predicate as
    /// `increment_claim_count`, so the two always agree.
    pub fn can_farmer_claim(env: Env, farmer: Address) -> bool {
        Self::claims_in_window(&env, &farmer) < MAX_CLAIMS_PER_FARMER_PER_YEAR
    }

    /// Port of `isPolicyActive`.
    pub fn is_policy_active(env: Env, policy_id: u64) -> bool {
        match env
            .storage()
            .persistent()
            .get::<_, Policy>(&DataKey::Policy(policy_id))
        {
            Some(p) => p.status == PolicyStatus::Active && env.ledger().timestamp() <= p.end_date,
            None => false,
        }
    }

    /// Port of `policyExists`.
    pub fn policy_exists(env: Env, policy_id: u64) -> bool {
        env.storage().persistent().has(&DataKey::Policy(policy_id))
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
        Self::bump_policy(env, policy);
    }

    fn load_policy(env: &Env, policy_id: u64) -> Policy {
        match env
            .storage()
            .persistent()
            .get::<_, Policy>(&DataKey::Policy(policy_id))
        {
            Some(p) => {
                Self::bump_policy(env, &p);
                p
            }
            None => panic_with_error!(env, PolicyError::PolicyDoesNotExist),
        }
    }

    /// The `Policy` entry's TTL rule: open (PENDING / ACTIVE) -> `end_date + 30 d`;
    /// CLAIM_PENDING -> below 30 d to 60 d; closed -> left as it is.
    fn bump_policy(env: &Env, policy: &Policy) {
        let key = DataKey::Policy(policy.id);
        match policy.status {
            PolicyStatus::Pending | PolicyStatus::Active => {
                ttl::extend_for_policy(env, &key, policy.end_date)
            }
            PolicyStatus::ClaimPending => ttl::extend_pending(env, &key),
            PolicyStatus::Expired | PolicyStatus::Cancelled | PolicyStatus::Claimed => {}
        }
    }

    fn farmer_active_count(env: &Env, farmer: &Address) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::FarmerActiveCount(farmer.clone()))
            .unwrap_or(0)
    }

    fn set_farmer_active_count(env: &Env, farmer: &Address, v: u32, end_date: u64) {
        let key = DataKey::FarmerActiveCount(farmer.clone());
        env.storage().persistent().set(&key, &v);
        ttl::extend_shared(env, &key, Some(end_date));
    }

    fn farmer_pending_count(env: &Env, farmer: &Address) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::FarmerPendingCount(farmer.clone()))
            .unwrap_or(0)
    }

    fn set_farmer_pending_count(env: &Env, farmer: &Address, v: u32, end_date: u64) {
        let key = DataKey::FarmerPendingCount(farmer.clone());
        env.storage().persistent().set(&key, &v);
        ttl::extend_shared(env, &key, Some(end_date));
    }

    fn claim_times(env: &Env, farmer: &Address) -> Vec<u64> {
        env.storage()
            .persistent()
            .get(&DataKey::FarmerClaimTimes(farmer.clone()))
            .unwrap_or_else(|| Vec::new(env))
    }

    /// Claims younger than [`CLAIM_WINDOW_SECS`]: a claim at `t` counts while
    /// `now < t + CLAIM_WINDOW_SECS` and stops counting at exactly 365 days.
    fn claims_in_window(env: &Env, farmer: &Address) -> u32 {
        let now = env.ledger().timestamp();
        let mut n = 0u32;
        for t in Self::claim_times(env, farmer).iter() {
            if now < t.saturating_add(CLAIM_WINDOW_SECS) {
                n += 1;
            }
        }
        n
    }

    fn org_outstanding(env: &Env, org: &Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::OrgOutstanding(org.clone()))
            .unwrap_or(0)
    }

    fn set_org_outstanding(env: &Env, org: &Address, v: i128, end_date: u64) {
        let key = DataKey::OrgOutstanding(org.clone());
        env.storage().persistent().set(&key, &v);
        ttl::extend_shared(env, &key, Some(end_date));
    }

    /// Decrement the farmer's ACTIVE count, guarding underflow (matches the Solidity `> 0` guard).
    fn decrement_active(env: &Env, farmer: &Address, end_date: u64) {
        let c = Self::farmer_active_count(env, farmer);
        if c > 0 {
            Self::set_farmer_active_count(env, farmer, c - 1, end_date);
        }
    }

    /// Release a PENDING policy's slot in the farmer's pending count, only if `create_policy`
    /// counted it (Finding 8). Removes the marker so the slot is released at most once.
    fn release_pending_slot(env: &Env, policy_id: u64, farmer: &Address, end_date: u64) {
        let key = DataKey::PendingCounted(policy_id);
        if env.storage().persistent().get(&key).unwrap_or(false) {
            env.storage().persistent().remove(&key);
            let pending = Self::farmer_pending_count(env, farmer);
            if pending > 0 {
                Self::set_farmer_pending_count(env, farmer, pending - 1, end_date);
            }
        }
    }

    /// Release an org's outstanding exposure when a policy leaves ACTIVE coverage. Saturating at 0.
    fn release_org_exposure(env: &Env, org: &Address, sum_insured: i128, end_date: u64) {
        let outstanding = Self::org_outstanding(env, org);
        let next = if outstanding > sum_insured {
            outstanding - sum_insured
        } else {
            0
        };
        Self::set_org_outstanding(env, org, next, end_date);
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

    /// User-path instance + code extension: below 3 days to 10 days (see `ttl`).
    fn bump_instance(env: &Env) {
        ttl::touch_contract(env);
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
