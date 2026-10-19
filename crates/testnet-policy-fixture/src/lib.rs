#![no_std]
//! # TestnetPolicyFixture (TESTNET ONLY)
//!
//! A stand-in for `PolicyManager` that lets an admin INJECT policy state. It exists only for
//! the SettlementAdapter evidence lane (`scripts/testnet-evidence.sh`, Lane E): the real
//! PolicyManager sets `start_date = now` at activation, so a drought determination can only
//! settle 110+ days after a sale. With this fixture the production PayoutReceiver and
//! SettlementAdapter WASM (same hashes as Lane P) can verify a real, historical season today.
//!
//! **A policy set here is not a sale.** No premium, no certificate, no exposure accounting,
//! no lifecycle. Nothing it records means cover was bought.
//!
//! It implements exactly the `IPolicyManager` methods the PayoutReceiver calls:
//! `policy_exists`, `get_policy`, `can_farmer_claim`, `mark_as_claimed`,
//! `increment_claim_count`, `extend_policy_ttl`, `extend_contract_ttl`. The two writes are
//! gated by `Role::Oracle`, as on PolicyManager.
//!
//! The constructor panics on mainnet (network id = sha256 of the public passphrase), so the
//! fixture cannot be deployed there. It is not in the workspace `default-members`, so the
//! deploy-time wasm build never produces it.

use soroban_sdk::{contract, contractimpl, contracttype, panic_with_error, Address, Env};

use microcrop_shared::{
    errors::PolicyError,
    roles, ttl,
    types::{Policy, PolicyStatus, Role},
};

/// `sha256("Public Global Stellar Network ; September 2015")`, the mainnet network id.
pub const MAINNET_NETWORK_ID: [u8; 32] = [
    0x7a, 0xc3, 0x39, 0x97, 0x54, 0x4e, 0x31, 0x75, 0xd2, 0x66, 0xbd, 0x02, 0x24, 0x39, 0xb2, 0x2c,
    0xdb, 0x16, 0x50, 0x8c, 0x01, 0x16, 0x3f, 0x26, 0xe5, 0xcb, 0x2a, 0x3e, 0x10, 0x45, 0xa9, 0x79,
];

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Policy(u64),
    ClaimCount(Address),
}

#[contract]
pub struct TestnetPolicyFixture;

#[contractimpl]
impl TestnetPolicyFixture {
    /// Grants `Role::Admin` to `admin`. Panics on mainnet.
    pub fn __constructor(env: Env, admin: Address) {
        if env.ledger().network_id().to_array() == MAINNET_NETWORK_ID {
            panic!("testnet-policy-fixture must never be deployed on mainnet");
        }
        roles::grant_role(&env, Role::Admin, &admin);
    }

    /// Grant a role (ADMIN only), e.g. `Role::Oracle` to the evidence-lane PayoutReceiver.
    pub fn grant_role(env: Env, caller: Address, role: Role, who: Address) {
        roles::require_role(&env, &caller, Role::Admin);
        roles::grant_role(&env, role, &who);
    }

    /// True if `who` holds `role`.
    pub fn has_role(env: Env, role: Role, who: Address) -> bool {
        roles::has_role(&env, role, &who)
    }

    /// INJECT a policy (ADMIN only). Not a sale.
    pub fn set_policy(env: Env, caller: Address, policy: Policy) {
        roles::require_role(&env, &caller, Role::Admin);
        ttl::touch_contract(&env);
        let key = DataKey::Policy(policy.id);
        env.storage().persistent().set(&key, &policy);
        ttl::extend_shared(&env, &key, None);
    }

    pub fn policy_exists(env: Env, policy_id: u64) -> bool {
        env.storage().persistent().has(&DataKey::Policy(policy_id))
    }

    pub fn get_policy(env: Env, policy_id: u64) -> Policy {
        env.storage()
            .persistent()
            .get(&DataKey::Policy(policy_id))
            .unwrap_or_else(|| panic_with_error!(&env, PolicyError::PolicyDoesNotExist))
    }

    /// Always true: the fixture keeps no claim window.
    pub fn can_farmer_claim(_env: Env, _farmer: Address) -> bool {
        true
    }

    /// ORACLE: set the stored status to `Claimed`.
    pub fn mark_as_claimed(env: Env, caller: Address, policy_id: u64) {
        roles::require_role(&env, &caller, Role::Oracle);
        let mut policy = Self::get_policy(env.clone(), policy_id);
        policy.status = PolicyStatus::Claimed;
        env.storage()
            .persistent()
            .set(&DataKey::Policy(policy_id), &policy);
    }

    /// ORACLE: count the claim (for inspection only; never enforced).
    pub fn increment_claim_count(env: Env, caller: Address, farmer: Address) {
        roles::require_role(&env, &caller, Role::Oracle);
        let key = DataKey::ClaimCount(farmer);
        let n: u32 = env.storage().persistent().get(&key).unwrap_or(0);
        env.storage().persistent().set(&key, &(n + 1));
        ttl::extend_shared(&env, &key, None);
    }

    pub fn claim_count(env: Env, farmer: Address) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::ClaimCount(farmer))
            .unwrap_or(0)
    }

    pub fn extend_policy_ttl(env: Env, policy_id: u64) {
        let key = DataKey::Policy(policy_id);
        if env.storage().persistent().has(&key) {
            ttl::extend_shared(&env, &key, None);
        }
    }

    pub fn extend_contract_ttl(env: Env) {
        ttl::keep_contract_alive(&env);
    }
}

#[cfg(test)]
mod test {
    extern crate std;
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger as _},
        BytesN,
    };

    #[test]
    fn the_mainnet_network_id_constant_is_sha256_of_the_public_passphrase() {
        let env = Env::default();
        let h = env.crypto().sha256(&soroban_sdk::Bytes::from_slice(
            &env,
            b"Public Global Stellar Network ; September 2015",
        ));
        assert_eq!(h.to_array(), MAINNET_NETWORK_ID);
    }

    #[test]
    #[should_panic]
    fn refuses_to_deploy_on_mainnet() {
        let env = Env::default();
        env.ledger()
            .set_network_id(BytesN::from_array(&env, &MAINNET_NETWORK_ID).to_array());
        let admin = Address::generate(&env);
        env.register(TestnetPolicyFixture, (admin,));
    }

    #[test]
    fn deploys_elsewhere_and_only_the_oracle_marks_claimed() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let id = env.register(TestnetPolicyFixture, (admin.clone(),));
        let c = TestnetPolicyFixtureClient::new(&env, &id);
        assert!(c.has_role(&Role::Admin, &admin));
        assert!(!c.policy_exists(&1));
        let stranger = Address::generate(&env);
        assert!(c.try_mark_as_claimed(&stranger, &1).is_err());
    }
}
