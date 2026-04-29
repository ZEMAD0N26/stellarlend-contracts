//! # Per-Asset Oracle Staleness Tests — Issue #645
//!
//! Covers `set_asset_max_staleness`, `clear_asset_max_staleness`, and
//! `get_asset_max_staleness` with:
//!
//! - Admin-only authorization on set / clear
//! - Zero-value rejection
//! - Per-asset override takes precedence over global config
//! - Clear reverts to global config
//! - Fresh vs stale at exact boundary (per-asset threshold)
//! - One second past per-asset threshold is stale
//! - Per-asset config does not bleed into other assets
//! - Fallback oracle also respects per-asset staleness
//! - Cross-asset flow: two assets with different per-asset thresholds
//! - Admin can update per-asset threshold after initial set
//! - Borrow / liquidation path blocked when price is stale (staleness → no price)
//!
//! ## Security Notes
//! - Stale prices are never silently accepted regardless of which staleness
//!   limit (global or per-asset) is in effect.
//! - Only the admin can set or clear per-asset overrides.
//! - `require_auth()` is called on every state-changing path.

use super::*;
use oracle::{OracleConfig, OracleError, OracleKey, PriceFeed};
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    Address, Env,
};

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn setup(env: &Env) -> (LendingContractClient<'_>, Address, Address, Address) {
    let contract_id = env.register(LendingContract, ());
    let client = LendingContractClient::new(env, &contract_id);
    let admin = Address::generate(env);
    let asset = Address::generate(env);
    client.initialize(&admin, &1_000_000_000, &1000);
    (client, admin, asset, contract_id)
}

/// Write a price feed directly into storage to simulate staleness.
fn write_feed_at(
    env: &Env,
    contract_id: &Address,
    key: OracleKey,
    price: i128,
    timestamp: u64,
    oracle: &Address,
) {
    env.as_contract(contract_id, || {
        let feed = PriceFeed {
            price,
            last_updated: timestamp,
            oracle: oracle.clone(),
        };
        env.storage().persistent().set(&key, &feed);
    });
}

// ─────────────────────────────────────────────────────────────────────────────
// set_asset_max_staleness — authorization
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_set_asset_max_staleness_admin_succeeds() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, asset, _cid) = setup(&env);

    client.set_asset_max_staleness(&admin, &asset, &120);
    assert_eq!(client.get_asset_max_staleness(&asset), 120);
}

#[test]
fn test_set_asset_max_staleness_unauthorized() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, asset, _cid) = setup(&env);
    let stranger = Address::generate(&env);

    assert_eq!(
        client.try_set_asset_max_staleness(&stranger, &asset, &120),
        Err(Ok(OracleError::Unauthorized))
    );
}

#[test]
fn test_set_asset_max_staleness_zero_rejected() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, asset, _cid) = setup(&env);

    assert_eq!(
        client.try_set_asset_max_staleness(&admin, &asset, &0),
        Err(Ok(OracleError::InvalidPrice))
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// clear_asset_max_staleness — authorization
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_clear_asset_max_staleness_admin_succeeds() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, asset, _cid) = setup(&env);

    client.set_asset_max_staleness(&admin, &asset, &120);
    client.clear_asset_max_staleness(&admin, &asset);

    // After clear, should fall back to global default (3600)
    assert_eq!(client.get_asset_max_staleness(&asset), 3600);
}

#[test]
fn test_clear_asset_max_staleness_unauthorized() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, asset, _cid) = setup(&env);
    let stranger = Address::generate(&env);

    client.set_asset_max_staleness(&admin, &asset, &120);

    assert_eq!(
        client.try_clear_asset_max_staleness(&stranger, &asset),
        Err(Ok(OracleError::Unauthorized))
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// get_asset_max_staleness — resolution order
// ─────────────────────────────────────────────────────────────────────────────

/// No per-asset config and no global config → hard-coded default (3600).
#[test]
fn test_get_asset_max_staleness_defaults_to_global_default() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, asset, _cid) = setup(&env);

    assert_eq!(client.get_asset_max_staleness(&asset), 3600);
}

/// No per-asset config but global config set → returns global value.
#[test]
fn test_get_asset_max_staleness_uses_global_when_no_per_asset() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, asset, _cid) = setup(&env);

    client.configure_oracle(&admin, &OracleConfig { max_staleness_seconds: 900 });

    assert_eq!(client.get_asset_max_staleness(&asset), 900);
}

/// Per-asset config overrides global config.
#[test]
fn test_get_asset_max_staleness_per_asset_overrides_global() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, asset, _cid) = setup(&env);

    client.configure_oracle(&admin, &OracleConfig { max_staleness_seconds: 900 });
    client.set_asset_max_staleness(&admin, &asset, &60);

    assert_eq!(client.get_asset_max_staleness(&asset), 60);
}

/// After clear, per-asset reverts to global config (not hard-coded default).
#[test]
fn test_get_asset_max_staleness_reverts_to_global_after_clear() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, asset, _cid) = setup(&env);

    client.configure_oracle(&admin, &OracleConfig { max_staleness_seconds: 1800 });
    client.set_asset_max_staleness(&admin, &asset, &60);
    client.clear_asset_max_staleness(&admin, &asset);

    assert_eq!(client.get_asset_max_staleness(&asset), 1800);
}

// ─────────────────────────────────────────────────────────────────────────────
// get_price — per-asset staleness boundary
// ─────────────────────────────────────────────────────────────────────────────

/// Price at exactly the per-asset threshold is still valid.
#[test]
fn test_get_price_per_asset_exact_boundary_valid() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, asset, _cid) = setup(&env);

    client.set_asset_max_staleness(&admin, &asset, &120);

    env.ledger().with_mut(|li| li.timestamp = 0);
    client.update_price_feed(&admin, &asset, &100_000_000, &8);

    env.ledger().with_mut(|li| li.timestamp = 120);
    assert_eq!(client.get_price(&asset), 100_000_000);
}

/// Price one second past the per-asset threshold is stale.
#[test]
fn test_get_price_per_asset_one_second_past_threshold_stale() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, asset, _cid) = setup(&env);

    client.set_asset_max_staleness(&admin, &asset, &120);

    env.ledger().with_mut(|li| li.timestamp = 0);
    client.update_price_feed(&admin, &asset, &100_000_000, &8);

    env.ledger().with_mut(|li| li.timestamp = 121);
    assert_eq!(
        client.try_get_price(&asset),
        Err(Ok(OracleError::StalePrice))
    );
}

/// Per-asset threshold tighter than global: global would pass but per-asset rejects.
#[test]
fn test_get_price_per_asset_tighter_than_global() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, asset, _cid) = setup(&env);

    // Global = 3600s, per-asset = 60s
    client.set_asset_max_staleness(&admin, &asset, &60);

    env.ledger().with_mut(|li| li.timestamp = 0);
    client.update_price_feed(&admin, &asset, &100_000_000, &8);

    // t=200: stale under per-asset (60s) but would be fresh under global (3600s)
    env.ledger().with_mut(|li| li.timestamp = 200);
    assert_eq!(
        client.try_get_price(&asset),
        Err(Ok(OracleError::StalePrice))
    );
}

/// Per-asset threshold looser than global: price accepted beyond global limit.
#[test]
fn test_get_price_per_asset_looser_than_global() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, asset, _cid) = setup(&env);

    // Global = 3600s, per-asset = 7200s
    client.set_asset_max_staleness(&admin, &asset, &7200);

    env.ledger().with_mut(|li| li.timestamp = 0);
    client.update_price_feed(&admin, &asset, &100_000_000, &8);

    // t=5000: stale under global (3600s) but fresh under per-asset (7200s)
    env.ledger().with_mut(|li| li.timestamp = 5000);
    assert_eq!(client.get_price(&asset), 100_000_000);
}

/// After clearing per-asset override, global threshold is enforced again.
#[test]
fn test_get_price_reverts_to_global_after_clear() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, asset, _cid) = setup(&env);

    // Set per-asset to 7200s (looser than global 3600s)
    client.set_asset_max_staleness(&admin, &asset, &7200);

    env.ledger().with_mut(|li| li.timestamp = 0);
    client.update_price_feed(&admin, &asset, &100_000_000, &8);

    // t=5000: fresh under per-asset (7200s)
    env.ledger().with_mut(|li| li.timestamp = 5000);
    assert_eq!(client.get_price(&asset), 100_000_000);

    // Clear per-asset override
    client.clear_asset_max_staleness(&admin, &asset);

    // t=5000: now stale under global (3600s)
    assert_eq!(
        client.try_get_price(&asset),
        Err(Ok(OracleError::StalePrice))
    );
}

/// Admin can update per-asset threshold; new value takes effect immediately.
#[test]
fn test_set_asset_max_staleness_update_takes_effect_immediately() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, asset, _cid) = setup(&env);

    client.set_asset_max_staleness(&admin, &asset, &7200);

    env.ledger().with_mut(|li| li.timestamp = 0);
    client.update_price_feed(&admin, &asset, &100_000_000, &8);

    // t=5000: fresh under 7200s
    env.ledger().with_mut(|li| li.timestamp = 5000);
    assert_eq!(client.get_price(&asset), 100_000_000);

    // Tighten to 60s
    client.set_asset_max_staleness(&admin, &asset, &60);

    // t=5000: now stale under 60s (age = 5000s)
    assert_eq!(
        client.try_get_price(&asset),
        Err(Ok(OracleError::StalePrice))
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Per-asset staleness isolation — does not bleed into other assets
// ─────────────────────────────────────────────────────────────────────────────

/// Per-asset config on asset1 does not affect asset2.
#[test]
fn test_per_asset_staleness_isolated_from_other_assets() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, asset1, _cid) = setup(&env);
    let asset2 = Address::generate(&env);

    // asset1 gets a tight 60s window; asset2 uses global default (3600s)
    client.set_asset_max_staleness(&admin, &asset1, &60);

    env.ledger().with_mut(|li| li.timestamp = 0);
    client.update_price_feed(&admin, &asset1, &100_000_000, &8);
    client.update_price_feed(&admin, &asset2, &200_000_000, &8);

    // t=200: asset1 stale (60s), asset2 fresh (3600s)
    env.ledger().with_mut(|li| li.timestamp = 200);

    assert_eq!(
        client.try_get_price(&asset1),
        Err(Ok(OracleError::StalePrice))
    );
    assert_eq!(client.get_price(&asset2), 200_000_000);
}

// ─────────────────────────────────────────────────────────────────────────────
// Cross-asset flow — two assets with different per-asset thresholds
// ─────────────────────────────────────────────────────────────────────────────

/// Stablecoin (60s) and volatile asset (3600s) coexist correctly.
#[test]
fn test_cross_asset_different_per_asset_thresholds() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, stablecoin, _cid) = setup(&env);
    let volatile = Address::generate(&env);

    // Stablecoin oracle updates every 60s; volatile every 3600s
    client.set_asset_max_staleness(&admin, &stablecoin, &60);
    client.set_asset_max_staleness(&admin, &volatile, &3600);

    env.ledger().with_mut(|li| li.timestamp = 0);
    client.update_price_feed(&admin, &stablecoin, &100_000_000, &8);
    client.update_price_feed(&admin, &volatile, &50_000_000_000i128, &8);

    // t=30: both fresh
    env.ledger().with_mut(|li| li.timestamp = 30);
    assert_eq!(client.get_price(&stablecoin), 100_000_000);
    assert_eq!(client.get_price(&volatile), 50_000_000_000i128);

    // t=90: stablecoin stale (60s), volatile still fresh (3600s)
    env.ledger().with_mut(|li| li.timestamp = 90);
    assert_eq!(
        client.try_get_price(&stablecoin),
        Err(Ok(OracleError::StalePrice))
    );
    assert_eq!(client.get_price(&volatile), 50_000_000_000i128);

    // t=4000: both stale
    env.ledger().with_mut(|li| li.timestamp = 4000);
    assert_eq!(
        client.try_get_price(&stablecoin),
        Err(Ok(OracleError::StalePrice))
    );
    assert_eq!(
        client.try_get_price(&volatile),
        Err(Ok(OracleError::StalePrice))
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Fallback oracle respects per-asset staleness
// ─────────────────────────────────────────────────────────────────────────────

/// Fallback feed is accepted when within per-asset threshold.
#[test]
fn test_fallback_respects_per_asset_staleness_fresh() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, asset, _cid) = setup(&env);
    let fallback = Address::generate(&env);

    client.set_asset_max_staleness(&admin, &asset, &120);
    client.set_fallback_oracle(&admin, &asset, &fallback);

    // Primary written at t=0, fallback at t=50
    env.ledger().with_mut(|li| li.timestamp = 0);
    client.update_price_feed(&admin, &asset, &100_000_000, &8);

    env.ledger().with_mut(|li| li.timestamp = 50);
    client.update_price_feed(&fallback, &asset, &101_000_000, &8);

    // t=130: primary stale (age=130 > 120), fallback fresh (age=80 ≤ 120)
    env.ledger().with_mut(|li| li.timestamp = 130);
    assert_eq!(client.get_price(&asset), 101_000_000);
}

/// Fallback feed is rejected when outside per-asset threshold.
#[test]
fn test_fallback_respects_per_asset_staleness_stale() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, asset, contract_id) = setup(&env);
    let fallback = Address::generate(&env);

    client.set_asset_max_staleness(&admin, &asset, &120);
    client.set_fallback_oracle(&admin, &asset, &fallback);

    // Write both feeds at t=0 directly (simulating old data)
    write_feed_at(
        &env,
        &contract_id,
        OracleKey::PrimaryFeed(asset.clone()),
        100_000_000,
        0,
        &admin,
    );
    write_feed_at(
        &env,
        &contract_id,
        OracleKey::FallbackFeed(asset.clone()),
        101_000_000,
        0,
        &fallback,
    );

    // t=200: both stale under per-asset threshold (120s)
    env.ledger().with_mut(|li| li.timestamp = 200);
    assert_eq!(
        client.try_get_price(&asset),
        Err(Ok(OracleError::StalePrice))
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Borrow / liquidation path blocked when price is stale
// ─────────────────────────────────────────────────────────────────────────────

/// Collateral value is 0 when per-asset staleness is exceeded (stale price → no value).
#[test]
fn test_collateral_value_zero_when_per_asset_staleness_exceeded() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, _asset, _cid) = setup(&env);
    let user = Address::generate(&env);
    let borrow_asset = Address::generate(&env);
    let collateral_asset = Address::generate(&env);

    // Tight 60s window for collateral asset
    client.set_asset_max_staleness(&admin, &collateral_asset, &60);

    env.ledger().with_mut(|li| li.timestamp = 0);
    client.update_price_feed(&admin, &collateral_asset, &100_000_000, &8);
    client.borrow(&user, &borrow_asset, &10_000, &collateral_asset, &20_000);

    // t=100: price stale under per-asset (60s) → collateral value = 0
    env.ledger().with_mut(|li| li.timestamp = 100);
    assert_eq!(client.get_collateral_value(&user), 0);
}

/// Collateral value is non-zero when within per-asset threshold.
#[test]
fn test_collateral_value_nonzero_within_per_asset_threshold() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, _asset, _cid) = setup(&env);
    let user = Address::generate(&env);
    let borrow_asset = Address::generate(&env);
    let collateral_asset = Address::generate(&env);

    client.set_asset_max_staleness(&admin, &collateral_asset, &300);

    env.ledger().with_mut(|li| li.timestamp = 0);
    client.update_price_feed(&admin, &collateral_asset, &100_000_000, &8);
    client.borrow(&user, &borrow_asset, &10_000, &collateral_asset, &20_000);

    // t=200: within 300s threshold → value = 20_000
    env.ledger().with_mut(|li| li.timestamp = 200);
    assert_eq!(client.get_collateral_value(&user), 20_000);
}
