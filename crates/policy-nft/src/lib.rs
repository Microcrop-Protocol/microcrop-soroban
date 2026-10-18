#![no_std]
//! # PolicyNft
//!
//! Soroban port of `microcrop-contracts/microcrop/src/PolicyNFT.sol`.
//!
//! Solidity is a full ERC721 (+ Enumerable, URIStorage) with on-chain SVG/JSON `tokenURI`.
//! Soroban has no ERC-721 standard and no marketplace expecting `tokenURI`, so this is a
//! **minimal on-chain certificate + ownership registry**: the soulbound-while-active rule is
//! preserved; the SVG/JSON rendering moves off-chain (keyed by the on-chain certificate).
//! `tokenId == policyId` (as in Solidity).
//!
//! ## Interface / Solidity mapping
//! | Soroban fn                 | Solidity                              |
//! |----------------------------|---------------------------------------|
//! | `__constructor`            | `constructor(name_, symbol_)`         |
//! | `set_policy_manager`       | `setPolicyManager`                    |
//! | `mint_policy`              | `mintPolicy` (MINTER_ROLE)            |
//! | `update_policy_status`     | `updatePolicyStatus` (policyManager)  |
//! | `transfer`                 | ERC721 transfer + soulbound `_update` |
//! | `owner_of`                 | `ownerOf`                             |
//! | `get_certificate`          | `getCertificate`                      |
//! | `policy_nft_exists`        | `policyNFTExists`                     |
//! | `get_distributor_policies` | `getDistributorPolicies`              |
//!
//! ### EVM -> Soroban notes
//! - `AccessControl` -> [`microcrop_shared::roles`] registry + explicit `caller` + `require_auth`.
//! - `mapping(uint256 => PolicyCertificate) certificates` -> persistent `DataKey::Certificate(u64)`.
//! - ERC721 `_owners` -> persistent `DataKey::Owner(u64)`.
//! - `policyToToken` -> persistent `DataKey::PolicyToToken(u64)` (presence == minted; the
//!   Solidity `!= 0` sentinel becomes a `has(..)` check, so `policy_id == 0` is still rejected).
//! - `distributorPolicies` -> persistent `DataKey::DistributorCount(Address)` +
//!   `DataKey::DistributorPolicy(Address, u32)` (one entry per token; read in pages).
//! - The on-chain `tokenURI` (SVG/JSON) + `baseExternalURI` are DROPPED (rendered off-chain).
//! - Solidity zero-address guards on `farmer` collapse to the fact that Soroban `Address`
//!   values are always well-formed; the load-bearing `policy_id == 0` guard is kept.
//! - PolicyNFT.sol is NOT upgradeable (plain AccessControl, no UUPS) — no `upgrade`.

use soroban_sdk::{
    contract, contractevent, contractimpl, contracttype, panic_with_error, Address, Env, String,
    Vec,
};

use microcrop_shared::{
    errors::{CommonError, NftError},
    roles, ttl,
    types::{CoverageType, PolicyCertificate, Role},
};

// ============ Storage TTL policy (rent cap, `microcrop_shared::ttl`) ============
// - Instance + code: every call extends below 3 days to 10 days; the keeper
//   (`extend_contract_ttl`, or PolicyManager's, which also extends this contract) extends
//   below 23 days to 30 days.
// - Certificate entries (`Certificate`, `Owner`, `PolicyToToken`, `DistributorPolicy`) live
//   until the policy's `end_date + 30 d` (clamped to the network maximum); the distributor
//   count takes the longest need of any certificate and at least 60 days.

/// Hard maximum page size for the paginated index views. Each id is its own ledger entry,
/// and a Soroban transaction (simulation included) may touch at most 100 footprint entries;
/// a page of 100 plus the count and the instance overflows that, so pages stop at 50.
pub const MAX_PAGE_SIZE: u32 = 50;

/// Storage keys. `PolicyManager`/`Name`/`Symbol` are instance; the rest are persistent.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// Authorized PolicyManager (only address allowed to flip a certificate's active flag).
    PolicyManager,
    /// Collection name.
    Name,
    /// Collection symbol.
    Symbol,
    /// token_id -> PolicyCertificate.
    Certificate(u64),
    /// token_id -> owner Address.
    Owner(u64),
    /// policy_id -> token_id (presence == minted; `tokenId == policyId`).
    PolicyToToken(u64),
    /// distributor -> number of tokens minted for them (length of the index below).
    DistributorCount(Address),
    /// (distributor, index) -> token id. One entry per token, so the index never grows a
    /// single ledger entry: a single `Vec` entry rewritten on every mint hit the entry-size
    /// cap after ~5,450 policies and then blocked `activate_policy` (audit finding 1).
    DistributorPolicy(Address, u32),
}

// ============ Events (ported 1:1 from the Solidity events) ============

/// `PolicyNFTMinted(tokenId indexed, policyId indexed, farmer indexed, distributor, sumInsured)`.
#[contractevent(topics = ["policy_nft_minted"], data_format = "map")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyNftMinted {
    #[topic]
    pub token_id: u64,
    #[topic]
    pub policy_id: u64,
    #[topic]
    pub farmer: Address,
    pub distributor: Address,
    pub sum_insured: i128,
}

/// `PolicyStatusUpdated(tokenId indexed, isActive)`.
#[contractevent(topics = ["policy_status_updated"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyStatusUpdated {
    #[topic]
    pub token_id: u64,
    pub is_active: bool,
}

/// `PolicyManagerUpdated(previousPolicyManager indexed, newPolicyManager indexed)`.
/// Soroban has no zero address, so an unset previous manager is simply omitted from the
/// event (only emitted when one existed).
#[contractevent(topics = ["policy_manager_updated"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyManagerUpdated {
    #[topic]
    pub new_policy_manager: Address,
}

/// ERC721-style `Transfer(from indexed, to indexed, tokenId indexed)` for post-expiry moves.
#[contractevent(topics = ["transfer"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Transfer {
    #[topic]
    pub from: Address,
    #[topic]
    pub to: Address,
    pub token_id: u64,
}

#[contract]
pub struct PolicyNft;

#[contractimpl]
impl PolicyNft {
    /// Port of `constructor`. Grants ADMIN to `admin`; stores collection name/symbol.
    pub fn __constructor(env: Env, admin: Address, name: String, symbol: String) {
        roles::grant_role(&env, Role::Admin, &admin);
        let s = env.storage().instance();
        s.set(&DataKey::Name, &name);
        s.set(&DataKey::Symbol, &symbol);
    }

    /// Port of `setPolicyManager` (ADMIN). The stored PolicyManager is the ONLY caller
    /// permitted to `update_policy_status`.
    pub fn set_policy_manager(env: Env, caller: Address, policy_manager: Address) {
        roles::require_role(&env, &caller, Role::Admin);
        env.storage()
            .instance()
            .set(&DataKey::PolicyManager, &policy_manager);
        Self::bump_instance(&env);
        PolicyManagerUpdated {
            new_policy_manager: policy_manager,
        }
        .publish(&env);
    }

    /// PERMISSIONLESS keeper: extend this contract's instance and code below 23 days to 30
    /// days (a no-op above). `PolicyManager::extend_contract_ttl` does the same for it.
    pub fn extend_contract_ttl(env: Env) {
        ttl::keep_contract_alive(&env);
    }

    /// Grant a role (ADMIN only). Mirrors AccessControl `grantRole`.
    pub fn grant_role(env: Env, caller: Address, role: Role, who: Address) {
        roles::require_role(&env, &caller, Role::Admin);
        roles::grant_role(&env, role, &who);
    }

    /// Revoke a role (ADMIN only). Mirrors AccessControl `revokeRole`.
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

    // ---- domain logic ----

    /// Port of `mintPolicy` (MINTER_ROLE). `tokenId = policyId`; certificate starts active
    /// (soulbound); records distributor policy; sets owner = farmer. Reverts
    /// `InvalidPolicyId` on `policy_id == 0` and `PolicyAlreadyMinted` if already minted.
    #[allow(clippy::too_many_arguments)]
    pub fn mint_policy(
        env: Env,
        caller: Address,
        farmer: Address,
        policy_id: u64,
        distributor: Address,
        distributor_name: String,
        sum_insured: i128,
        premium: i128,
        start_date: u64,
        end_date: u64,
        coverage_type: CoverageType,
        region: String,
        plot_id: u64,
    ) -> u64 {
        // onlyRole(MINTER_ROLE)
        roles::require_role(&env, &caller, Role::Minter);

        // `policyId == 0` is the reserved sentinel (`policyToToken[policyId] != 0`) — reject.
        if policy_id == 0 {
            panic_with_error!(&env, NftError::InvalidPolicyId);
        }
        // `if (policyToToken[policyId] != 0) revert PolicyAlreadyMinted`.
        let p = env.storage().persistent();
        if p.has(&DataKey::PolicyToToken(policy_id)) {
            panic_with_error!(&env, NftError::PolicyAlreadyMinted);
        }

        // tokenId == policyId.
        let token_id = policy_id;

        let cert = PolicyCertificate {
            policy_id,
            farmer: farmer.clone(),
            distributor: distributor.clone(),
            distributor_name,
            sum_insured,
            premium,
            start_date,
            end_date,
            coverage_type,
            region,
            plot_id,
            is_active: true,
        };

        // Persist certificate, policy->token index, and ownership (`_safeMint` -> owner = farmer).
        p.set(&DataKey::Certificate(token_id), &cert);
        p.set(&DataKey::PolicyToToken(policy_id), &token_id);
        p.set(&DataKey::Owner(token_id), &farmer);

        // distributorPolicies[distributor].push(tokenId), as one entry per token + a count.
        let count_key = DataKey::DistributorCount(distributor.clone());
        let n: u32 = p.get(&count_key).unwrap_or(0);
        let item_key = DataKey::DistributorPolicy(distributor.clone(), n);
        p.set(&item_key, &token_id);
        p.set(&count_key, &(n + 1));

        // TTL: every certificate entry lives as long as the policy needs it.
        ttl::extend_for_policy(&env, &DataKey::Certificate(token_id), end_date);
        ttl::extend_for_policy(&env, &DataKey::PolicyToToken(policy_id), end_date);
        ttl::extend_for_policy(&env, &DataKey::Owner(token_id), end_date);
        ttl::extend_for_policy(&env, &item_key, end_date);
        ttl::extend_shared(&env, &count_key, Some(end_date));
        Self::bump_instance(&env);

        PolicyNftMinted {
            token_id,
            policy_id,
            farmer,
            distributor,
            sum_insured,
        }
        .publish(&env);

        token_id
    }

    /// Port of `updatePolicyStatus` — ONLY the wired PolicyManager may call (NotPolicyManager
    /// otherwise). Flips the certificate active flag (governs the soulbound restriction).
    pub fn update_policy_status(env: Env, caller: Address, policy_id: u64, is_active: bool) {
        // `if (msg.sender != policyManager) revert NotPolicyManager()`.
        let pm: Option<Address> = env.storage().instance().get(&DataKey::PolicyManager);
        match pm {
            Some(pm_addr) if pm_addr == caller => {}
            _ => panic_with_error!(&env, NftError::NotPolicyManager),
        }
        // The wired PolicyManager still authorizes this invocation.
        caller.require_auth();

        // tokenId = policyToToken[policyId]; if 0 revert PolicyNotFound.
        let p = env.storage().persistent();
        let token_id: u64 = match p.get(&DataKey::PolicyToToken(policy_id)) {
            Some(t) => t,
            None => panic_with_error!(&env, NftError::PolicyNotFound),
        };

        let mut cert: PolicyCertificate = p
            .get(&DataKey::Certificate(token_id))
            .unwrap_or_else(|| panic_with_error!(&env, NftError::PolicyNotFound));
        cert.is_active = is_active;
        p.set(&DataKey::Certificate(token_id), &cert);
        ttl::extend_for_policy(&env, &DataKey::Certificate(token_id), cert.end_date);
        Self::bump_instance(&env);

        PolicyStatusUpdated {
            token_id,
            is_active,
        }
        .publish(&env);
    }

    /// Soulbound transfer: reverts `TransferWhileActive` if the certificate is still active;
    /// otherwise moves ownership. `from.require_auth()`. Mirrors ERC721 `_update`: transfers
    /// between two real holders are blocked while `certificates[tokenId].isActive`.
    pub fn transfer(env: Env, from: Address, to: Address, token_id: u64) {
        from.require_auth();

        let p = env.storage().persistent();
        let owner: Address = match p.get(&DataKey::Owner(token_id)) {
            Some(o) => o,
            None => panic_with_error!(&env, NftError::TokenNotFound),
        };
        // ERC721: `from` must be the current owner.
        if owner != from {
            panic_with_error!(&env, CommonError::Unauthorized);
        }

        let cert: PolicyCertificate = p
            .get(&DataKey::Certificate(token_id))
            .unwrap_or_else(|| panic_with_error!(&env, NftError::TokenNotFound));
        // Soulbound while active (`_update`).
        if cert.is_active {
            panic_with_error!(&env, NftError::TransferWhileActive);
        }

        p.set(&DataKey::Owner(token_id), &to);
        ttl::extend_for_policy(&env, &DataKey::Owner(token_id), cert.end_date);

        Transfer { from, to, token_id }.publish(&env);
    }

    /// PERMISSIONLESS TTL helper: extend the certificate entries of `policy_id`
    /// (`PolicyToToken`, `Certificate`, `Owner`) to the policy's need (`end_date + 30 d`,
    /// clamped to the network maximum). A no-op for a policy that was never minted. Called
    /// best effort by `PolicyManager::extend_policy_ttl`.
    pub fn extend_policy_ttl(env: Env, policy_id: u64) {
        Self::bump_instance(&env);
        let p = env.storage().persistent();
        let token: Option<u64> = p.get(&DataKey::PolicyToToken(policy_id));
        if let Some(token_id) = token {
            let cert: Option<PolicyCertificate> = p.get(&DataKey::Certificate(token_id));
            if let Some(cert) = cert {
                let end = cert.end_date;
                ttl::extend_for_policy(&env, &DataKey::PolicyToToken(policy_id), end);
                ttl::extend_for_policy(&env, &DataKey::Certificate(token_id), end);
                if p.has(&DataKey::Owner(token_id)) {
                    ttl::extend_for_policy(&env, &DataKey::Owner(token_id), end);
                }
            }
        }
    }

    // ---- views ----

    /// Port of `ownerOf`. Reverts `TokenNotFound` for a nonexistent token.
    pub fn owner_of(env: Env, token_id: u64) -> Address {
        env.storage()
            .persistent()
            .get(&DataKey::Owner(token_id))
            .unwrap_or_else(|| panic_with_error!(&env, NftError::TokenNotFound))
    }

    /// Port of `getCertificate`. Reverts `TokenNotFound` for a nonexistent token (Solidity
    /// returned a zeroed struct; Soroban `Address` has no zero value, so we revert).
    pub fn get_certificate(env: Env, token_id: u64) -> PolicyCertificate {
        env.storage()
            .persistent()
            .get(&DataKey::Certificate(token_id))
            .unwrap_or_else(|| panic_with_error!(&env, NftError::TokenNotFound))
    }

    /// Port of `policyNFTExists` (`policyToToken[policyId] != 0`).
    pub fn policy_nft_exists(env: Env, policy_id: u64) -> bool {
        env.storage()
            .persistent()
            .has(&DataKey::PolicyToToken(policy_id))
    }

    /// Port of `getDistributorPolicies`, paginated: up to `limit` token ids starting at index
    /// `start`, in mint order. `limit` is capped at [`MAX_PAGE_SIZE`]; a `start` past the end
    /// returns an empty list. Use [`Self::get_distributor_policy_count`] for the total.
    pub fn get_distributor_policies(
        env: Env,
        distributor: Address,
        start: u32,
        limit: u32,
    ) -> Vec<u64> {
        let p = env.storage().persistent();
        let count: u32 = p
            .get(&DataKey::DistributorCount(distributor.clone()))
            .unwrap_or(0);
        let end = start.saturating_add(limit.min(MAX_PAGE_SIZE)).min(count);
        let mut out = Vec::new(&env);
        let mut i = start;
        while i < end {
            let key = DataKey::DistributorPolicy(distributor.clone(), i);
            let id: u64 = p
                .get(&key)
                .unwrap_or_else(|| panic_with_error!(&env, NftError::TokenNotFound));
            // Index items live as long as their certificate (`end_date + 30 d`); paging an
            // older item needs it restored first (the RPC simulation adds the restore).
            out.push_back(id);
            i += 1;
        }
        out
    }

    /// Number of certificates minted for `distributor` (the length of the paged index).
    pub fn get_distributor_policy_count(env: Env, distributor: Address) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::DistributorCount(distributor))
            .unwrap_or(0)
    }

    // ---- internal helpers ----

    /// User-path instance + code extension: below 3 days to 10 days (see `ttl`).
    fn bump_instance(env: &Env) {
        ttl::touch_contract(env);
    }
}

#[cfg(test)]
mod test;
