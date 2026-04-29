//! # Oracle Migration and Swap Tests — Issue #536
//!
//! Deterministic tests for changing oracle addresses while positions are open.
//! Covers:
//! - Legacy to Legacy Swap
//! - Hardened to Hardened Swap
//! - Legacy to Hardened Migration (precedence logic)
//! - Liquidation Safety during Swap
//! - Misconfiguration Failure Modes (Safe Rejection)
//! - Event Emission Auditability

use super::*;
use soroban_sdk::{
    contract, contractimpl, testutils::{Address as _, Ledger, Events},
    xdr, Address, Env, Symbol, TryFromVal, Val, Vec,
};
use crate::borrow::BorrowError;
use views::HEALTH_FACTOR_SCALE;

#[contract]
pub struct MockLegacyOracle;

#[contractimpl]
impl MockLegacyOracle {
    pub fn price(env: Env, _asset: Address) -> i128 {
        env.storage().instance().get(&Symbol::new(&env, "price")).unwrap_or(0)
    }

    pub fn set_price(env: Env, price: i128) {
        env.storage().instance().set(&Symbol::new(&env, "price"), &price);
    }
}

fn setup(env: &Env) -> (LendingContractClient<'_>, Address, Address, Address, Address) {
    let contract_id = env.register(LendingContract, ());
    let client = LendingContractClient::new(env, &contract_id);
    let admin = Address::generate(env);
    let user = Address::generate(env);
    let asset = Address::generate(env);
    let collateral_asset = Address::generate(env);
    
    // Diagnostic event
    env.events().publish(Vec::from_array(env, [Symbol::new(env, "diagnostic")]), Symbol::new(env, "setup"));
    
    client.initialize(&admin, &1_000_000_000, &1000);
    (client, admin, user, asset, collateral_asset)
}

fn get_last_event_topics(env: &Env) -> Vec<Val> {
    let all = env.events().all();
    let events = all.events();
    if events.is_empty() {
        panic!("no events emitted at all");
    }
    let last = events.last().unwrap();
    match &last.body {
        xdr::ContractEventBody::V0(body) => {
            let mut topics = Vec::new(env);
            for topic in body.topics.iter() {
                topics.push_back(Val::try_from_val(env, topic).unwrap());
            }
            topics
        }
        _ => panic!("unexpected event body variant"),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Legacy Oracle Swaps
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_legacy_to_legacy_swap_updates_valuation() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, user, asset, collateral_asset) = setup(&env);

    let oracle_a = env.register(MockLegacyOracle, ());
    let oracle_b = env.register(MockLegacyOracle, ());
    let oracle_a_client = MockLegacyOracleClient::new(&env, &oracle_a);
    let oracle_b_client = MockLegacyOracleClient::new(&env, &oracle_b);

    oracle_a_client.set_price(&100_000_000); // 1.0
    oracle_b_client.set_price(&200_000_000); // 2.0

    client.set_oracle(&admin, &oracle_a);
    client.borrow(&user, &asset, &10_000, &collateral_asset, &20_000);

    // Initial valuation with Oracle A
    assert_eq!(client.get_collateral_value(&user), 20_000);
    assert_eq!(client.get_debt_value(&user), 10_000);

    // Swap to Oracle B
    client.set_oracle(&admin, &oracle_b);

    // Updated valuation with Oracle B
    assert_eq!(client.get_collateral_value(&user), 40_000);
    assert_eq!(client.get_debt_value(&user), 20_000);

    // Verify event emission
    let topics = get_last_event_topics(&env);
    assert_eq!(Symbol::try_from_val(&env, &topics.get(0).unwrap()).unwrap(), Symbol::new(&env, "OracleSetEvent"));
    assert_eq!(Address::try_from_val(&env, &topics.get(1).unwrap()).unwrap(), admin);
    assert_eq!(Address::try_from_val(&env, &topics.get(2).unwrap()).unwrap(), oracle_b);
}

// ─────────────────────────────────────────────────────────────────────────────
// Hardened Oracle Swaps
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_hardened_primary_swap_updates_valuation() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, user, asset, collateral_asset) = setup(&env);

    let oracle_a = Address::generate(&env);
    let oracle_b = Address::generate(&env);

    client.set_primary_oracle(&admin, &asset, &oracle_a);
    client.set_primary_oracle(&admin, &collateral_asset, &oracle_a);

    env.ledger().with_mut(|li| li.timestamp = 100);
    client.update_price_feed(&oracle_a, &asset, &100_000_000, &8);
    client.update_price_feed(&oracle_a, &collateral_asset, &100_000_000, &8);

    client.borrow(&user, &asset, &10_000, &collateral_asset, &20_000);

    assert_eq!(client.get_collateral_value(&user), 20_000);

    // Swap Primary Oracle
    client.set_primary_oracle(&admin, &collateral_asset, &oracle_b);
    client.update_price_feed(&oracle_b, &collateral_asset, &50_000_000, &8); // Price drops to 0.5

    // Value should reflect new primary oracle
    assert_eq!(client.get_collateral_value(&user), 10_000);

    // Verify event emission
    let topics = get_last_event_topics(&env);
    assert_eq!(Symbol::try_from_val(&env, &topics.get(0).unwrap()).unwrap(), Symbol::new(&env, "OracleConfigEvent"));
}

// ─────────────────────────────────────────────────────────────────────────────
// Migration: Legacy to Hardened Precedence
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_hardened_oracle_preempts_legacy_oracle() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, user, asset, collateral_asset) = setup(&env);

    let legacy_oracle = env.register(MockLegacyOracle, ());
    let legacy_client = MockLegacyOracleClient::new(&env, &legacy_oracle);
    legacy_client.set_price(&100_000_000);

    client.set_oracle(&admin, &legacy_oracle);
    client.borrow(&user, &asset, &10_000, &collateral_asset, &20_000);

    assert_eq!(client.get_collateral_value(&user), 20_000);

    // Add Hardened Oracle with different price
    let hardened_oracle = Address::generate(&env);
    client.set_primary_oracle(&admin, &collateral_asset, &hardened_oracle);
    client.update_price_feed(&hardened_oracle, &collateral_asset, &150_000_000, &8); // 1.5

    // Should use 1.5 instead of 1.0
    assert_eq!(client.get_collateral_value(&user), 30_000);
}

// ─────────────────────────────────────────────────────────────────────────────
// Liquidation Safety during Swap
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_liquidation_safety_triggered_by_oracle_swap() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, user, asset, collateral_asset) = setup(&env);

    let oracle_a = Address::generate(&env);
    client.set_primary_oracle(&admin, &asset, &oracle_a);
    client.set_primary_oracle(&admin, &collateral_asset, &oracle_a);

    env.ledger().with_mut(|li| li.timestamp = 100);
    client.update_price_feed(&oracle_a, &asset, &100_000_000, &8);
    client.update_price_feed(&oracle_a, &collateral_asset, &100_000_000, &8);

    // Collateral 20k, Debt 10k. LT 80% -> Weighted 16k. HF 1.6 (Healthy)
    client.borrow(&user, &asset, &10_000, &collateral_asset, &20_000);
    assert_eq!(client.get_health_factor(&user), 16_000);

    // Swap to Oracle B with crash price for collateral
    let oracle_b = Address::generate(&env);
    client.set_primary_oracle(&admin, &collateral_asset, &oracle_b);
    client.update_price_feed(&oracle_b, &collateral_asset, &50_000_000, &8); // Crash to 0.5

    // Weighted = 20k * 0.5 * 0.8 = 8k. Debt 10k. HF = 0.8 (Liquidatable)
    assert_eq!(client.get_health_factor(&user), 8000);

    // Liquidate
    let liquidator = Address::generate(&env);
    client.liquidate(&liquidator, &user, &asset, &collateral_asset, &5000);

    // Verify position was reduced
    let pos = client.get_user_position(&user);
    assert!(pos.debt_balance < 10_000);
}

// ─────────────────────────────────────────────────────────────────────────────
// Misconfiguration Failure Modes
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_misconfigured_legacy_oracle_fails_safely() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, user, asset, collateral_asset) = setup(&env);

    // Use a non-contract address as oracle
    let fake_oracle = Address::generate(&env);
    client.set_oracle(&admin, &fake_oracle);

    // Borrow should fail because valuation returns 0 (missing oracle)
    let result = client.try_borrow(&user, &asset, &10_000, &collateral_asset, &20_000);
    assert!(result.is_err());

    // If we already had a position and then swap to fake oracle
    let real_oracle = env.register(MockLegacyOracle, ());
    MockLegacyOracleClient::new(&env, &real_oracle).set_price(&100_000_000);
    client.set_oracle(&admin, &real_oracle);
    client.borrow(&user, &asset, &10_000, &collateral_asset, &20_000);

    client.set_oracle(&admin, &fake_oracle);

    // Valuation returns 0
    assert_eq!(client.get_health_factor(&user), 0);

    // Withdraw should fail
    let result = client.try_withdraw(&user, &collateral_asset, &1000);
    assert!(result.is_err());

    // Liquidation should fail (reverts because HF=0 means price unavailable)
    let liquidator = Address::generate(&env);
    let result = client.try_liquidate(&liquidator, &user, &asset, &collateral_asset, &1000);
    assert_eq!(result, Err(Ok(BorrowError::InsufficientCollateral)));
}

#[test]
fn test_stale_hardened_oracle_reverts_to_legacy_or_fails() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, user, asset, collateral_asset) = setup(&env);

    let legacy_oracle = env.register(MockLegacyOracle, ());
    MockLegacyOracleClient::new(&env, &legacy_oracle).set_price(&100_000_000);
    client.set_oracle(&admin, &legacy_oracle);

    let hardened_oracle = Address::generate(&env);
    client.set_primary_oracle(&admin, &collateral_asset, &hardened_oracle);

    env.ledger().with_mut(|li| li.timestamp = 100);
    client.update_price_feed(&hardened_oracle, &collateral_asset, &200_000_000, &8);

    client.borrow(&user, &asset, &10_000, &collateral_asset, &20_000);
    assert_eq!(client.get_collateral_value(&user), 40_000); // 20k * 2.0

    // Make hardened oracle stale
    env.ledger().with_mut(|li| li.timestamp = 100 + 4000); // Default staleness 3600

    // Should fall back to legacy oracle (1.0)
    assert_eq!(client.get_collateral_value(&user), 20_000); // 20k * 1.0
}
