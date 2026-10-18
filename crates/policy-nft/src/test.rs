#![cfg(test)]
//! Unit tests for the PolicyNft port: mint happy-path + registry views, soulbound transfer
//! blocked while active / allowed after deactivation, and the access guards
//! (MINTER_ROLE on mint, NotPolicyManager on status update, InvalidPolicyId,
//! PolicyAlreadyMinted, PolicyNotFound).

use soroban_sdk::{testutils::Address as _, Address, Env, String};

use microcrop_shared::{
    errors::{CommonError, NftError},
    types::{CoverageType, Role},
};

use crate::{PolicyNft, PolicyNftClient};

struct Fixture<'a> {
    env: Env,
    client: PolicyNftClient<'a>,
    admin: Address,
    minter: Address,
    policy_manager: Address,
}

fn setup<'a>() -> Fixture<'a> {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let name = String::from_str(&env, "MicroCrop Insurance Certificate");
    let symbol = String::from_str(&env, "mcINS");

    let id = env.register(PolicyNft, (admin.clone(), name, symbol));
    let client = PolicyNftClient::new(&env, &id);

    // Grant MINTER to a backend/minter address and wire a PolicyManager address.
    let minter = Address::generate(&env);
    client.grant_role(&admin, &Role::Minter, &minter);

    let policy_manager = Address::generate(&env);
    client.set_policy_manager(&admin, &policy_manager);

    Fixture {
        env,
        client,
        admin,
        minter,
        policy_manager,
    }
}

/// Mint a policy #`policy_id` to `farmer` from `distributor` named "Acre".
fn mint(f: &Fixture, farmer: &Address, distributor: &Address, policy_id: u64) -> u64 {
    f.client.mint_policy(
        &f.minter,
        farmer,
        &policy_id,
        distributor,
        &String::from_str(&f.env, "Acre Africa"),
        &10_000_000_000i128, // sum insured (1000 USDC, 7dp)
        &50_000_000i128,     // premium
        &1_700_000_000u64,   // start
        &1_730_000_000u64,   // end
        &CoverageType::Drought,
        &String::from_str(&f.env, "Nakuru"),
        &42u64, // plot id
    )
}

#[test]
fn constructor_registers() {
    let env = Env::default();
    let admin = Address::generate(&env);
    let name = String::from_str(&env, "MicroCrop Insurance Certificate");
    let symbol = String::from_str(&env, "mcINS");

    let id = env.register(PolicyNft, (admin, name, symbol));
    let _client = PolicyNftClient::new(&env, &id);
}

#[test]
fn mint_happy_path_sets_owner_certificate_and_indexes() {
    let f = setup();
    let farmer = Address::generate(&f.env);
    let distributor = Address::generate(&f.env);

    let token_id = mint(&f, &farmer, &distributor, 7);

    // tokenId == policyId.
    assert_eq!(token_id, 7);
    // Ownership == farmer.
    assert_eq!(f.client.owner_of(&7), farmer);
    // Registry existence.
    assert!(f.client.policy_nft_exists(&7));
    assert!(!f.client.policy_nft_exists(&8));

    // Certificate stored faithfully and starts ACTIVE (soulbound).
    let cert = f.client.get_certificate(&7);
    assert_eq!(cert.policy_id, 7);
    assert_eq!(cert.farmer, farmer);
    assert_eq!(cert.distributor, distributor);
    assert_eq!(cert.sum_insured, 10_000_000_000i128);
    assert_eq!(cert.plot_id, 42);
    assert_eq!(cert.coverage_type, CoverageType::Drought);
    assert!(cert.is_active);

    // distributorPolicies[distributor] == [7].
    let list = f.client.get_distributor_policies(&distributor, &0, &10);
    assert_eq!(f.client.get_distributor_policy_count(&distributor), 1);
    assert_eq!(list.len(), 1);
    assert_eq!(list.get(0).unwrap(), 7);
}

#[test]
fn mint_requires_minter_role() {
    let f = setup();
    let outsider = Address::generate(&f.env);
    let farmer = Address::generate(&f.env);
    let distributor = Address::generate(&f.env);

    // `outsider` holds no role -> Unauthorized (auth is mocked, role check fails).
    let res = f.client.try_mint_policy(
        &outsider,
        &farmer,
        &1u64,
        &distributor,
        &String::from_str(&f.env, "Acre"),
        &1_000_000_000i128,
        &50_000_000i128,
        &1u64,
        &2u64,
        &CoverageType::Drought,
        &String::from_str(&f.env, "Nakuru"),
        &1u64,
    );
    assert_eq!(res, Err(Ok(CommonError::Unauthorized.into())));
}

#[test]
fn mint_rejects_zero_policy_id() {
    let f = setup();
    let farmer = Address::generate(&f.env);
    let distributor = Address::generate(&f.env);

    let res = f.client.try_mint_policy(
        &f.minter,
        &farmer,
        &0u64, // reserved sentinel
        &distributor,
        &String::from_str(&f.env, "Acre"),
        &1_000_000_000i128,
        &50_000_000i128,
        &1u64,
        &2u64,
        &CoverageType::Drought,
        &String::from_str(&f.env, "Nakuru"),
        &1u64,
    );
    assert_eq!(res, Err(Ok(NftError::InvalidPolicyId.into())));
}

#[test]
fn mint_rejects_duplicate_policy() {
    let f = setup();
    let farmer = Address::generate(&f.env);
    let distributor = Address::generate(&f.env);

    mint(&f, &farmer, &distributor, 5);

    let res = f.client.try_mint_policy(
        &f.minter,
        &farmer,
        &5u64, // already minted
        &distributor,
        &String::from_str(&f.env, "Acre"),
        &1_000_000_000i128,
        &50_000_000i128,
        &1u64,
        &2u64,
        &CoverageType::Drought,
        &String::from_str(&f.env, "Nakuru"),
        &1u64,
    );
    assert_eq!(res, Err(Ok(NftError::PolicyAlreadyMinted.into())));
}

#[test]
fn transfer_blocked_while_active() {
    let f = setup();
    let farmer = Address::generate(&f.env);
    let distributor = Address::generate(&f.env);
    let buyer = Address::generate(&f.env);

    mint(&f, &farmer, &distributor, 9);

    // Certificate is active -> soulbound.
    let res = f.client.try_transfer(&farmer, &buyer, &9);
    assert_eq!(res, Err(Ok(NftError::TransferWhileActive.into())));
    // Ownership unchanged.
    assert_eq!(f.client.owner_of(&9), farmer);
}

#[test]
fn transfer_allowed_after_deactivation() {
    let f = setup();
    let farmer = Address::generate(&f.env);
    let distributor = Address::generate(&f.env);
    let buyer = Address::generate(&f.env);

    mint(&f, &farmer, &distributor, 11);

    // PolicyManager deactivates (expiry / claim).
    f.client
        .update_policy_status(&f.policy_manager, &11u64, &false);
    assert!(!f.client.get_certificate(&11).is_active);

    // Now transfer succeeds and moves ownership.
    f.client.transfer(&farmer, &buyer, &11);
    assert_eq!(f.client.owner_of(&11), buyer);
}

#[test]
fn transfer_rejects_wrong_owner() {
    let f = setup();
    let farmer = Address::generate(&f.env);
    let distributor = Address::generate(&f.env);
    let attacker = Address::generate(&f.env);
    let buyer = Address::generate(&f.env);

    mint(&f, &farmer, &distributor, 13);
    f.client
        .update_policy_status(&f.policy_manager, &13u64, &false);

    // `from` is not the owner.
    let res = f.client.try_transfer(&attacker, &buyer, &13);
    assert_eq!(res, Err(Ok(CommonError::Unauthorized.into())));
}

#[test]
fn update_policy_status_only_policy_manager() {
    let f = setup();
    let farmer = Address::generate(&f.env);
    let distributor = Address::generate(&f.env);

    mint(&f, &farmer, &distributor, 3);

    // Neither the admin nor a random minter may flip the active flag — only the wired PM.
    let res = f.client.try_update_policy_status(&f.admin, &3u64, &false);
    assert_eq!(res, Err(Ok(NftError::NotPolicyManager.into())));

    let res2 = f.client.try_update_policy_status(&f.minter, &3u64, &false);
    assert_eq!(res2, Err(Ok(NftError::NotPolicyManager.into())));

    // The wired PolicyManager can.
    f.client
        .update_policy_status(&f.policy_manager, &3u64, &false);
    assert!(!f.client.get_certificate(&3).is_active);
}

#[test]
fn update_policy_status_unknown_policy_reverts() {
    let f = setup();
    let res = f
        .client
        .try_update_policy_status(&f.policy_manager, &999u64, &false);
    assert_eq!(res, Err(Ok(NftError::PolicyNotFound.into())));
}

#[test]
fn owner_of_unknown_token_reverts() {
    let f = setup();
    let res = f.client.try_owner_of(&404u64);
    assert_eq!(res, Err(Ok(NftError::TokenNotFound.into())));
}

#[test]
fn distributor_index_is_per_entry_and_paginated_past_100() {
    // Finding 1: the index used to be one Vec entry rewritten on every mint. It is now one
    // entry per token, read in pages of at most MAX_PAGE_SIZE (50: a transaction may touch
    // at most 100 ledger entries, so a page of 100 would itself fail).
    let f = setup();
    let farmer = Address::generate(&f.env);
    let distributor = Address::generate(&f.env);
    for id in 1..=150u64 {
        mint(&f, &farmer, &distributor, id);
    }
    assert_eq!(f.client.get_distributor_policy_count(&distributor), 150);
    assert_eq!(crate::MAX_PAGE_SIZE, 50);

    // Walk all 150 in three full pages.
    let mut seen = 0u64;
    let mut start = 0u32;
    while start < 150 {
        let page = f.client.get_distributor_policies(&distributor, &start, &50);
        assert_eq!(page.len(), 50);
        for id in page.iter() {
            seen += 1;
            assert_eq!(id, seen);
        }
        start += 50;
    }
    assert_eq!(seen, 150);

    // limit is capped at MAX_PAGE_SIZE, even when the caller asks for 100 or u32::MAX.
    assert_eq!(
        f.client
            .get_distributor_policies(&distributor, &0, &100)
            .len(),
        50
    );
    assert_eq!(
        f.client
            .get_distributor_policies(&distributor, &120, &u32::MAX)
            .len(),
        30
    );
    // Past the end is empty, not a revert; so is an unknown distributor.
    assert_eq!(
        f.client
            .get_distributor_policies(&distributor, &150, &10)
            .len(),
        0
    );
    assert_eq!(
        f.client
            .get_distributor_policies(&Address::generate(&f.env), &0, &10)
            .len(),
        0
    );
}

// ---------------- Rent cap (storage TTL) ----------------

mod rent {
    use super::*;
    use crate::DataKey;
    use microcrop_shared::ttl::{
        DAY_LEDGERS, KEEPER_TARGET_LEDGERS, NETWORK_MAX_ENTRY_TTL_LEDGERS, USER_TARGET_LEDGERS,
    };
    use soroban_sdk::testutils::storage::{Instance as _, Persistent as _};
    use soroban_sdk::testutils::Ledger as _;

    const NOW: u64 = 1_700_000_000;

    fn mint_ending(f: &Fixture, policy_id: u64, end: u64) -> Address {
        let farmer = Address::generate(&f.env);
        f.client.mint_policy(
            &f.minter,
            &farmer,
            &policy_id,
            &Address::generate(&f.env),
            &String::from_str(&f.env, "Acre Africa"),
            &10_000_000_000i128,
            &50_000_000i128,
            &NOW,
            &end,
            &CoverageType::Drought,
            &String::from_str(&f.env, "Nakuru"),
            &42u64,
        );
        farmer
    }

    #[test]
    fn certificate_entries_live_until_end_date_plus_grace() {
        let f = setup();
        f.env.ledger().set_timestamp(NOW);
        f.env
            .ledger()
            .set_max_entry_ttl(NETWORK_MAX_ENTRY_TTL_LEDGERS);
        mint_ending(&f, 7, NOW + 100 * 86_400);
        let ttl = |k: &DataKey| {
            f.env.as_contract(&f.client.address, || {
                f.env.storage().persistent().get_ttl(k)
            })
        };
        for k in [
            DataKey::Certificate(7),
            DataKey::Owner(7),
            DataKey::PolicyToToken(7),
        ] {
            assert_eq!(ttl(&k), 130 * DAY_LEDGERS);
        }
        // A 400-day need clamps to the network maximum, never more.
        mint_ending(&f, 8, NOW + 400 * 86_400);
        assert_eq!(ttl(&DataKey::Certificate(8)), NETWORK_MAX_ENTRY_TTL_LEDGERS);
    }

    #[test]
    fn calls_no_longer_extend_the_instance_above_the_threshold() {
        let f = setup();
        f.env.ledger().set_timestamp(NOW);
        let inst = || {
            f.env
                .as_contract(&f.client.address, || f.env.storage().instance().get_ttl())
        };
        // Setup touched it: user target, not the network maximum.
        assert_eq!(inst(), USER_TARGET_LEDGERS);
        f.client.extend_contract_ttl();
        assert_eq!(inst(), KEEPER_TARGET_LEDGERS);
        // A mint is a user call: it leaves a 30-day TTL alone.
        mint_ending(&f, 9, NOW + 100 * 86_400);
        assert_eq!(inst(), KEEPER_TARGET_LEDGERS);
        // Below 3 days a user call tops it up to 10 days, no more.
        let seq = f.env.ledger().sequence();
        f.env.ledger().set_sequence_number(seq + 28 * DAY_LEDGERS);
        f.client.extend_policy_ttl(&9);
        assert_eq!(inst(), USER_TARGET_LEDGERS);
    }
}
