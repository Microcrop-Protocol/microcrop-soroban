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
//! - `distributorPolicies` -> persistent `DataKey::DistributorPolicies(Address)`.
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
    roles,
    types::{CoverageType, PolicyCertificate, Role},
};

// ============ Storage TTL policy ============
// ~5s ledgers => 17,280 ledgers/day. Certificates are long-lived registry records, so
// persistent entries are bumped ~30 days on every touch; the instance (name/symbol/wiring)
// ~7 days. Thresholds sit one day below the bump so a bump only happens when actually needed.
const DAY_IN_LEDGERS: u32 = 17_280;
const INSTANCE_BUMP_AMOUNT: u32 = 7 * DAY_IN_LEDGERS;
const INSTANCE_LIFETIME_THRESHOLD: u32 = INSTANCE_BUMP_AMOUNT - DAY_IN_LEDGERS;
const PERSISTENT_BUMP_AMOUNT: u32 = 30 * DAY_IN_LEDGERS;
const PERSISTENT_LIFETIME_THRESHOLD: u32 = PERSISTENT_BUMP_AMOUNT - DAY_IN_LEDGERS;

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
    /// distributor -> list of their token ids.
    DistributorPolicies(Address),
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

        // distributorPolicies[distributor].push(tokenId).
        let mut list: Vec<u64> = p
            .get(&DataKey::DistributorPolicies(distributor.clone()))
            .unwrap_or_else(|| Vec::new(&env));
        list.push_back(token_id);
        p.set(&DataKey::DistributorPolicies(distributor.clone()), &list);

        // TTL bumps.
        Self::bump_persistent(&env, &DataKey::Certificate(token_id));
        Self::bump_persistent(&env, &DataKey::PolicyToToken(policy_id));
        Self::bump_persistent(&env, &DataKey::Owner(token_id));
        Self::bump_persistent(&env, &DataKey::DistributorPolicies(distributor.clone()));
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
        Self::bump_persistent(&env, &DataKey::Certificate(token_id));

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
        Self::bump_persistent(&env, &DataKey::Owner(token_id));

        Transfer {
            from,
            to,
            token_id,
        }
        .publish(&env);
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

    /// Port of `getDistributorPolicies`.
    pub fn get_distributor_policies(env: Env, distributor: Address) -> Vec<u64> {
        env.storage()
            .persistent()
            .get(&DataKey::DistributorPolicies(distributor))
            .unwrap_or_else(|| Vec::new(&env))
    }

    // ---- internal helpers ----

    fn bump_instance(env: &Env) {
        env.storage()
            .instance()
            .extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
    }

    fn bump_persistent(env: &Env, key: &DataKey) {
        env.storage()
            .persistent()
            .extend_ttl(key, PERSISTENT_LIFETIME_THRESHOLD, PERSISTENT_BUMP_AMOUNT);
    }
}

#[cfg(test)]
mod test;
