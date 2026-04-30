//! Deterministic Multi-Collateral Valuation Tests
//!
//! This suite verifies the correctness and stability of collateral valuation
//! when multiple assets are involved, focusing on oracle interactions,
//! staleness handling, and monotonicity invariants.

use crate::cross_asset::{AssetParams, CrossAssetError};
use crate::{LendingContract, LendingContractClient};
use crate::oracle::OracleConfig;
use soroban_sdk::{testutils::{Address as _, Ledger}, Address, Env};

const BPS_SCALE: i128 = 10_000;
const HF_NO_DEBT: i128 = 1_000_000;
const PRICE_DIVISOR: i128 = 10_000_000; // 7 decimals as used in cross_asset.rs

fn setup(env: &Env) -> (LendingContractClient<'_>, Address) {
    let contract_id = env.register(LendingContract, ());
    let client = LendingContractClient::new(env, &contract_id);
    let admin = Address::generate(env);
    client.initialize(&admin, &1_000_000_000, &1_000);
    client.initialize_admin(&admin);
    
    // Initialize oracle config
    client.configure_oracle(&admin, &OracleConfig { max_staleness_seconds: 3600 });
    
    (client, admin)
}

fn register_asset(env: &Env, client: &LendingContractClient<'_>, admin: &Address, ltv: i128) -> Address {
    let asset = Address::generate(env);
    client.register_asset(admin, &asset);
    client.set_asset_params(&asset, &AssetParams {
        ltv,
        liquidation_threshold: (ltv + 500).min(BPS_SCALE),
        price_feed: Address::generate(env),
        debt_ceiling: 1_000_000_000_000,
        deposit_cap: 1_000_000_000_000,
        is_active: true,
        borrow_cap: 0,
    });
    client.set_asset_decimals(admin, &asset, &7);
    client.set_primary_oracle(admin, &asset, admin); // Admin will act as oracle
    asset
}

fn set_price(client: &LendingContractClient<'_>, admin: &Address, asset: &Address, price: i128) {
    client.update_price_feed(admin, asset, &price, &7);
}

#[test]
fn test_multi_collateral_valuation_standard() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin) = setup(&env);
    let user = Address::generate(&env);

    let ltv_a = 8_000i128; // 80%
    let ltv_b = 6_000i128; // 60%
    let asset_a = register_asset(&env, &client, &admin, ltv_a);
    let asset_b = register_asset(&env, &client, &admin, ltv_b);

    set_price(&client, &admin, &asset_a, 20_000_000); // $2.0
    set_price(&client, &admin, &asset_b, 10_000_000); // $1.0

    client.deposit_collateral_asset(&user, &asset_a, &100); // Value = 100 * 2 = 200
    client.deposit_collateral_asset(&user, &asset_b, &200); // Value = 200 * 1 = 200

    let summary = client.get_cross_position_summary(&user);
    assert_eq!(summary.total_collateral_usd, 400);
    assert_eq!(summary.total_debt_usd, 0);
    assert_eq!(summary.health_factor, HF_NO_DEBT);

    // Borrow 100 of asset A (Value = 100 * 2 = 200)
    client.borrow_asset(&user, &asset_a, &100);

    let summary = client.get_cross_position_summary(&user);
    assert_eq!(summary.total_debt_usd, 200);
    
    // Expected weighted collateral:
    // A: 200 * 80% = 160
    // B: 200 * 60% = 120
    // Total Weighted = 280
    // HF = 280 * 10000 / 200 = 14000
    assert_eq!(summary.health_factor, 14_000);
}

#[test]
fn test_multi_collateral_stale_price() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin) = setup(&env);
    let user = Address::generate(&env);

    let asset_a = register_asset(&env, &client, &admin, 8_000);
    let asset_b = register_asset(&env, &client, &admin, 6_000);

    set_price(&client, &admin, &asset_a, 10_000_000);
    set_price(&client, &admin, &asset_b, 10_000_000);

    client.deposit_collateral_asset(&user, &asset_a, &100);
    client.deposit_collateral_asset(&user, &asset_b, &100);

    // Prices are fresh
    let summary = client.get_cross_position_summary(&user);
    assert_eq!(summary.total_collateral_usd, 200);

    // Advance time beyond staleness (1 hour = 3600s)
    env.ledger().set_timestamp(3601);

    // Now price for A is stale, B is also stale (since they were set at t=0)
    // CrossAssetError::PriceUnavailable is expected
    let result = client.try_get_cross_position_summary(&user);
    assert!(result.is_err());

    // Update only A
    set_price(&client, &admin, &asset_a, 10_000_000);
    
    // Still errors because B is stale
    let result = client.try_get_cross_position_summary(&user);
    assert!(result.is_err());

    // Update B
    set_price(&client, &admin, &asset_b, 10_000_000);
    let result = client.try_get_cross_position_summary(&user);
    assert!(result.is_ok());
    assert_eq!(result.unwrap().unwrap().total_collateral_usd, 200);
}

#[test]
fn test_valuation_monotonicity_with_price() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin) = setup(&env);
    let user = Address::generate(&env);

    let asset = register_asset(&env, &client, &admin, 8_000);
    client.deposit_collateral_asset(&user, &asset, &1000);
    client.borrow_asset(&user, &asset, &400);

    let mut prev_hf = 0;
    let mut prev_val = 0;

    for p in [10_000_000, 11_000_000, 12_000_000, 15_000_000] {
        set_price(&client, &admin, &asset, p);
        let summary = client.get_cross_position_summary(&user);
        
        assert!(summary.total_collateral_usd >= prev_val, "Collateral value must be monotonic with price");
        // HF should remain constant if price of both collateral and debt change together?
        // Wait, in this case it's the same asset.
        // HF = (amount * price * LTV / Scale) * Scale / (debt * price / Scale)
        // HF = amount * LTV / debt
        // Price cancels out.
        
        prev_val = summary.total_collateral_usd;
        prev_hf = summary.health_factor;
    }

    // Different asset for debt to see HF change
    let asset_debt = register_asset(&env, &client, &admin, 8_000);
    set_price(&client, &admin, &asset_debt, 10_000_000);
    client.deposit_collateral_asset(&user, &asset_debt, &1000);
    client.borrow_asset(&user, &asset_debt, &400);

    for p in [10_000_000, 11_000_000, 12_000_000, 15_000_000] {
        set_price(&client, &admin, &asset, p); // Increase collateral price
        let summary = client.get_cross_position_summary(&user);
        assert!(summary.health_factor >= prev_hf, "HF must be monotonic with collateral price increase");
        prev_hf = summary.health_factor;
    }
}

#[test]
fn test_valuation_non_negative() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin) = setup(&env);
    let user = Address::generate(&env);

    let asset = register_asset(&env, &client, &admin, 8_000);
    set_price(&client, &admin, &asset, 10_000_000);
    client.deposit_collateral_asset(&user, &asset, &1000);

    let summary = client.get_cross_position_summary(&user);
    assert!(summary.total_collateral_usd >= 0);
    assert!(summary.total_debt_usd >= 0);
    assert!(summary.health_factor >= 0);
}

#[test]
fn test_rounding_edge_cases() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin) = setup(&env);
    let user = Address::generate(&env);

    let asset = register_asset(&env, &client, &admin, 7_500); // 75%
    
    // Very small amount where price * amount < Scale
    set_price(&client, &admin, &asset, 10_000); // $0.001
    client.deposit_collateral_asset(&user, &asset, &100); 
    // Value = 100 * 10,000 / 10,000,000 = 100,000 / 10,000,000 = 0.1 -> 0
    
    let summary = client.get_cross_position_summary(&user);
    assert_eq!(summary.total_collateral_usd, 0);

    // Slightly larger
    client.deposit_collateral_asset(&user, &asset, &900); 
    // Total amount = 1000. Value = 1000 * 10,000 / 10,000,000 = 10,000,000 / 10,000,000 = 1
    let summary = client.get_cross_position_summary(&user);
    assert_eq!(summary.total_collateral_usd, 1);
}
