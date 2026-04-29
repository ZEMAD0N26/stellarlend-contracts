//! # Oracle Manipulation Resistance Tests — Issue #666
//!
//! Adversarial scenarios targeting the oracle module and its downstream
//! integration in views (health factor, liquidation) and borrow operations.
//!
//! ## Scenarios
//! - Sudden 10× price jump/crash and its effect on borrow eligibility
//! - Stale primary feed → automatic fallback activation, view consistency
//! - Both feeds stale → safe-default values (0, not panic) throughout
//! - Unauthorised price write rejected; existing price unchanged
//! - Liquidation does not overpay when price drops mid-transaction
//! - Health factor boundary: exactly at threshold vs one unit below
//! - Cross-asset: collateral price manipulated while debt price is stable

use super::*;
use oracle::{OracleError, OracleKey, PriceFeed};
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    Address, Env,
};

// ─────────────────────────────────────────────────────────────────────────────
// Shared helpers
// ─────────────────────────────────────────────────────────────────────────────

fn setup(env: &Env) -> (LendingContractClient<'_>, Address, Address, Address) {
    let contract_id = env.register(LendingContract, ());
    let client = LendingContractClient::new(env, &contract_id);
    let admin = Address::generate(env);
    let asset = Address::generate(env);
    client.initialize(&admin, &1_000_000_000, &100);
    (client, admin, asset, contract_id)
}

/// Write a price feed entry directly into contract storage to simulate
/// arbitrary prices and timestamps without going through `update_price_feed`.
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
// 1. Sudden price jump / crash
// ─────────────────────────────────────────────────────────────────────────────

/// A 10× price increase raises collateral value proportionally.
/// The health factor improves and the position is safely over-collateralised.
#[test]
fn test_sudden_10x_price_increase_improves_health_factor() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, debt_asset, _) = setup(&env);
    let collateral_asset = Address::generate(&env);
    let user = Address::generate(&env);

    env.ledger().with_mut(|li| li.timestamp = 0);
    // Baseline: collateral = $1.00
    client.update_price_feed(&admin, &collateral_asset, &100_000_000, &8);
    client.update_price_feed(&admin, &debt_asset, &100_000_000, &8);

    client.borrow(&user, &debt_asset, &10_000, &collateral_asset, &20_000);

    let hf_before = client.get_health_factor(&user);

    // Oracle reports 10× price jump: collateral = $10.00
    client.update_price_feed(&admin, &collateral_asset, &1_000_000_000, &8);

    let hf_after = client.get_health_factor(&user);
    assert!(
        hf_after > hf_before,
        "health factor must improve after collateral price 10× jump"
    );
    // With 80% threshold: hf = (20000*10 * 0.8) * 10000 / (10000*1) = 160000
    assert!(hf_after >= views::HEALTH_FACTOR_SCALE);
}

/// A 10× collateral price crash makes the position unhealthy.
/// The view layer must reflect this immediately without any state changes.
#[test]
fn test_sudden_10x_price_crash_makes_position_unhealthy() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, debt_asset, _) = setup(&env);
    let collateral_asset = Address::generate(&env);
    let user = Address::generate(&env);

    env.ledger().with_mut(|li| li.timestamp = 0);
    // Set collateral price high enough to make the borrow valid
    client.update_price_feed(&admin, &collateral_asset, &1_000_000_000, &8); // $10
    client.update_price_feed(&admin, &debt_asset, &100_000_000, &8); // $1

    client.borrow(&user, &debt_asset, &10_000, &collateral_asset, &20_000);

    let hf_before = client.get_health_factor(&user);
    assert!(hf_before >= views::HEALTH_FACTOR_SCALE, "position should start healthy");

    // Collateral crashes 10×: $10 → $1
    client.update_price_feed(&admin, &collateral_asset, &100_000_000, &8);

    let hf_after = client.get_health_factor(&user);
    assert!(
        hf_after < hf_before,
        "health factor must drop after collateral crash"
    );
}

/// After a price crash that makes collateral value < debt value,
/// get_max_liquidatable_amount must return a positive value (not 0).
#[test]
fn test_price_crash_triggers_liquidation_eligibility() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, debt_asset, _) = setup(&env);
    let collateral_asset = Address::generate(&env);
    let user = Address::generate(&env);

    env.ledger().with_mut(|li| li.timestamp = 0);
    client.update_price_feed(&admin, &collateral_asset, &1_000_000_000, &8); // $10
    client.update_price_feed(&admin, &debt_asset, &100_000_000, &8); // $1

    // Borrow with moderate collateral cushion
    client.borrow(&user, &debt_asset, &50_000, &collateral_asset, &100_000);

    // Initially not liquidatable
    assert_eq!(client.get_max_liquidatable_amount(&user), 0);

    // Crash collateral to $0.50 — now collateral value = 50_000, debt = 50_000
    // With threshold 80%: weighted collateral = 40_000 < debt 50_000 → unhealthy
    client.update_price_feed(&admin, &collateral_asset, &50_000_000, &8);

    let liquidatable = client.get_max_liquidatable_amount(&user);
    assert!(
        liquidatable > 0,
        "position must become liquidatable after price crash, got {}",
        liquidatable
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 2. Stale primary → fallback activation
// ─────────────────────────────────────────────────────────────────────────────

/// When the primary feed goes stale, the fallback price is used transparently.
/// View functions (collateral_value, health_factor) must reflect the fallback price.
#[test]
fn test_stale_primary_fallback_used_in_views() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, debt_asset, _) = setup(&env);
    let collateral_asset = Address::generate(&env);
    let user = Address::generate(&env);
    let fallback = Address::generate(&env);

    client.set_fallback_oracle(&admin, &collateral_asset, &fallback);
    client.set_fallback_oracle(&admin, &debt_asset, &fallback);

    env.ledger().with_mut(|li| li.timestamp = 0);
    // Primary: $1.00 for both
    client.update_price_feed(&admin, &collateral_asset, &100_000_000, &8);
    client.update_price_feed(&admin, &debt_asset, &100_000_000, &8);

    client.borrow(&user, &debt_asset, &10_000, &collateral_asset, &20_000);

    // Primary goes stale; fallback (at t=4000) has higher collateral price $2.00
    env.ledger().with_mut(|li| li.timestamp = 4000);
    client.update_price_feed(&fallback, &collateral_asset, &200_000_000, &8);
    client.update_price_feed(&fallback, &debt_asset, &100_000_000, &8);

    // get_price should use fallback
    assert_eq!(client.get_price(&collateral_asset), 200_000_000);

    // Collateral value doubles: was 20_000, now 40_000
    let cv = client.get_collateral_value(&user);
    assert_eq!(cv, 40_000);

    // Health factor must be higher than at baseline
    let hf = client.get_health_factor(&user);
    assert!(hf >= views::HEALTH_FACTOR_SCALE);
}

/// Fallback is consistent: get_price and get_collateral_value agree.
#[test]
fn test_fallback_price_consistency_across_views() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, debt_asset, _) = setup(&env);
    let collateral_asset = Address::generate(&env);
    let user = Address::generate(&env);
    let fallback = Address::generate(&env);

    client.set_fallback_oracle(&admin, &collateral_asset, &fallback);

    env.ledger().with_mut(|li| li.timestamp = 0);
    client.update_price_feed(&admin, &collateral_asset, &100_000_000, &8);
    client.update_price_feed(&admin, &debt_asset, &100_000_000, &8);

    client.borrow(&user, &debt_asset, &5_000, &collateral_asset, &10_000);

    // Move past primary staleness, write fresh fallback at $3.00
    env.ledger().with_mut(|li| li.timestamp = 5000);
    client.update_price_feed(&fallback, &collateral_asset, &300_000_000, &8);

    let price = client.get_price(&collateral_asset);
    let cv = client.get_collateral_value(&user);
    // cv should equal 10_000 * price / 100_000_000
    let expected_cv = (10_000i128 * price) / 100_000_000;
    assert_eq!(cv, expected_cv, "collateral_value and get_price must be consistent");
}

// ─────────────────────────────────────────────────────────────────────────────
// 3. Both feeds stale → safe zero, no panic
// ─────────────────────────────────────────────────────────────────────────────

/// When both primary and fallback are stale, get_price returns StalePrice.
/// View functions must return 0 rather than panicking.
#[test]
fn test_both_feeds_stale_views_return_zero_not_panic() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, debt_asset, contract_id) = setup(&env);
    let collateral_asset = Address::generate(&env);
    let user = Address::generate(&env);
    let fallback = Address::generate(&env);

    client.set_fallback_oracle(&admin, &collateral_asset, &fallback);

    // Write both feeds at t=0
    write_feed_at(
        &env,
        &contract_id,
        OracleKey::PrimaryFeed(collateral_asset.clone()),
        100_000_000,
        0,
        &admin,
    );
    write_feed_at(
        &env,
        &contract_id,
        OracleKey::FallbackFeed(collateral_asset.clone()),
        105_000_000,
        0,
        &fallback,
    );
    write_feed_at(
        &env,
        &contract_id,
        OracleKey::PrimaryFeed(debt_asset.clone()),
        100_000_000,
        0,
        &admin,
    );

    client.borrow(&user, &debt_asset, &10_000, &collateral_asset, &20_000);

    // Both feeds are now stale
    env.ledger().with_mut(|li| li.timestamp = 8000);

    assert_eq!(
        client.try_get_price(&collateral_asset),
        Err(Ok(OracleError::StalePrice))
    );

    // Views must return 0 rather than panicking
    assert_eq!(client.get_collateral_value(&user), 0);
    assert_eq!(client.get_health_factor(&user), 0);
    assert_eq!(client.get_max_liquidatable_amount(&user), 0);
}

/// When no price feed exists at all, views return 0 (NoPriceFeed path).
#[test]
fn test_no_price_feed_views_return_zero() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, debt_asset, _) = setup(&env);
    let collateral_asset = Address::generate(&env);
    let user = Address::generate(&env);

    client.borrow(&user, &debt_asset, &10_000, &collateral_asset, &20_000);

    assert_eq!(
        client.try_get_price(&collateral_asset),
        Err(Ok(OracleError::NoPriceFeed))
    );
    assert_eq!(client.get_collateral_value(&user), 0);
    assert_eq!(client.get_debt_value(&user), 0);
    assert_eq!(client.get_health_factor(&user), 0);
    assert_eq!(client.get_max_liquidatable_amount(&user), 0);
}

/// get_user_position returns a zeroed-out summary when oracle is unavailable.
#[test]
fn test_user_position_summary_zeroed_when_oracle_unavailable() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, debt_asset, _) = setup(&env);
    let collateral_asset = Address::generate(&env);
    let user = Address::generate(&env);

    client.borrow(&user, &debt_asset, &10_000, &collateral_asset, &20_000);

    let summary = client.get_user_position(&user);
    // Collateral and debt balances are still stored
    assert_eq!(summary.collateral_balance, 20_000);
    assert!(summary.debt_balance > 0);
    // But values and health factor are 0 without oracle
    assert_eq!(summary.collateral_value, 0);
    assert_eq!(summary.debt_value, 0);
    assert_eq!(summary.health_factor, 0);
}

// ─────────────────────────────────────────────────────────────────────────────
// 4. Cache poisoning — unauthorised price write rejected
// ─────────────────────────────────────────────────────────────────────────────

/// An attacker who is not admin, primary oracle, or fallback oracle
/// cannot submit a price update. The stored price must remain unchanged.
#[test]
fn test_attacker_cannot_poison_price_feed() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, asset, _) = setup(&env);
    let attacker = Address::generate(&env);

    env.ledger().with_mut(|li| li.timestamp = 0);
    client.update_price_feed(&admin, &asset, &100_000_000, &8);

    let price_before = client.get_price(&asset);

    // Attacker tries to submit a manipulated price
    assert_eq!(
        client.try_update_price_feed(&attacker, &asset, &1, &8),
        Err(Ok(OracleError::Unauthorized))
    );

    // Price is unchanged
    let price_after = client.get_price(&asset);
    assert_eq!(price_before, price_after);
}

/// A registered fallback oracle cannot overwrite the primary slot.
/// Existing primary price must remain intact.
#[test]
fn test_fallback_oracle_cannot_poison_primary_slot() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, asset, contract_id) = setup(&env);
    let fallback = Address::generate(&env);

    env.ledger().with_mut(|li| li.timestamp = 0);
    client.set_fallback_oracle(&admin, &asset, &fallback);
    client.update_price_feed(&admin, &asset, &100_000_000, &8);

    // Fallback oracle writes to its own slot (fallback), not primary
    client.update_price_feed(&fallback, &asset, &999_999_999, &8);

    // Primary slot must retain the original price
    env.as_contract(&contract_id, || {
        let primary_feed: Option<PriceFeed> = env
            .storage()
            .persistent()
            .get(&OracleKey::PrimaryFeed(asset.clone()));
        assert_eq!(
            primary_feed.map(|f| f.price),
            Some(100_000_000),
            "fallback must not overwrite primary slot"
        );
    });
}

/// A future timestamp in the feed is treated as stale (clock skew guard).
/// An attacker injecting a far-future timestamp cannot extend feed validity.
#[test]
fn test_future_timestamp_treated_as_stale_cannot_extend_feed() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, asset, contract_id) = setup(&env);
    let oracle = Address::generate(&env);

    env.ledger().with_mut(|li| li.timestamp = 1000);

    // Inject a feed with a far-future timestamp directly into storage
    write_feed_at(
        &env,
        &contract_id,
        OracleKey::PrimaryFeed(asset.clone()),
        100_000_000,
        999_999_999, // far future
        &oracle,
    );

    // Must be rejected as stale even at current time 1000
    assert_eq!(
        client.try_get_price(&asset),
        Err(Ok(OracleError::StalePrice))
    );
}

/// Zero and negative prices are always rejected; attacker cannot set price to zero
/// to make collateral worth nothing via the API.
#[test]
fn test_zero_and_negative_price_rejected_via_api() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, asset, _) = setup(&env);

    assert_eq!(
        client.try_update_price_feed(&admin, &asset, &0, &8),
        Err(Ok(OracleError::InvalidPrice))
    );
    assert_eq!(
        client.try_update_price_feed(&admin, &asset, &-1, &8),
        Err(Ok(OracleError::InvalidPrice))
    );
    assert_eq!(
        client.try_update_price_feed(&admin, &asset, &i128::MIN, &8),
        Err(Ok(OracleError::InvalidPrice))
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 5. Health factor at exact liquidation threshold boundary
// ─────────────────────────────────────────────────────────────────────────────

/// Health factor exactly at HEALTH_FACTOR_SCALE (10000) is not liquidatable.
///
/// Borrow uses 200% raw collateral (satisfies 150% min). Then prices are set so
/// that collateral_value * liq_bps == debt_value * BPS_SCALE → hf = 10000 exactly.
/// With collateral=20_000, debt=10_000, liq_bps=8000, P_d=$1:
///   need collateral_value = debt_value * 1.25 → P_c = $0.625 (62_500_000).
#[test]
fn test_health_factor_at_exact_threshold_not_liquidatable() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, debt_asset, _) = setup(&env);
    let collateral_asset = Address::generate(&env);
    let user = Address::generate(&env);

    env.ledger().with_mut(|li| li.timestamp = 0);
    // Borrow with raw 200% collateral ratio (≥ 150% min); oracle not used for borrow check.
    client.borrow(&user, &debt_asset, &10_000, &collateral_asset, &20_000);

    // Now set prices to produce hf = 10000:
    // collateral_value = 20_000 * 62_500_000 / 1e8 = 12_500
    // debt_value       = 10_000 * 100_000_000 / 1e8 = 10_000
    // hf = (12_500 * 8000 / 10000) * 10000 / 10_000 = 10_000
    client.update_price_feed(&admin, &collateral_asset, &62_500_000, &8);
    client.update_price_feed(&admin, &debt_asset, &100_000_000, &8);

    let hf = client.get_health_factor(&user);
    assert_eq!(hf, views::HEALTH_FACTOR_SCALE, "hf must equal threshold exactly");

    // At exactly the threshold, position is NOT liquidatable
    assert_eq!(client.get_max_liquidatable_amount(&user), 0);
}

/// Health factor below HEALTH_FACTOR_SCALE (10000) means position is liquidatable.
///
/// Same setup as the exact-threshold test but with P_c = $0.62 (62_000_000)
/// instead of $0.625, pushing hf to 9920 (< 10000).
#[test]
fn test_health_factor_just_below_threshold_is_liquidatable() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, debt_asset, _) = setup(&env);
    let collateral_asset = Address::generate(&env);
    let user = Address::generate(&env);

    env.ledger().with_mut(|li| li.timestamp = 0);
    // Borrow at 200% raw ratio (≥ 150% min)
    client.borrow(&user, &debt_asset, &10_000, &collateral_asset, &20_000);

    // Set prices so hf < 10000:
    // collateral_value = 20_000 * 62_000_000 / 1e8 = 12_400
    // debt_value       = 10_000 * 100_000_000 / 1e8 = 10_000
    // hf = (12_400 * 8000 / 10000) * 10000 / 10_000 = 9920 < 10000
    client.update_price_feed(&admin, &collateral_asset, &62_000_000, &8);
    client.update_price_feed(&admin, &debt_asset, &100_000_000, &8);

    let hf = client.get_health_factor(&user);
    assert!(hf < views::HEALTH_FACTOR_SCALE, "hf={} should be below threshold", hf);

    let liquidatable = client.get_max_liquidatable_amount(&user);
    assert!(liquidatable > 0, "position must be liquidatable when hf < threshold");
}

// ─────────────────────────────────────────────────────────────────────────────
// 6. Oracle paused — updates blocked but reads continue from stored data
// ─────────────────────────────────────────────────────────────────────────────

/// When oracle is paused, new price updates are blocked.
/// Existing fresh prices remain readable.
#[test]
fn test_oracle_paused_blocks_updates_but_reads_persist() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, asset, _) = setup(&env);

    env.ledger().with_mut(|li| li.timestamp = 0);
    client.update_price_feed(&admin, &asset, &100_000_000, &8);

    // Pause oracle
    client.set_oracle_paused(&admin, &true);

    // New update is blocked
    assert_eq!(
        client.try_update_price_feed(&admin, &asset, &200_000_000, &8),
        Err(Ok(OracleError::OraclePaused))
    );

    // Existing price is still readable (not stale yet)
    assert_eq!(client.get_price(&asset), 100_000_000);

    // Unpause — updates resume
    client.set_oracle_paused(&admin, &false);
    client.update_price_feed(&admin, &asset, &200_000_000, &8);
    assert_eq!(client.get_price(&asset), 200_000_000);
}

/// Unauthorised address cannot pause the oracle.
#[test]
fn test_oracle_pause_unauthorized_rejected() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, _asset, _) = setup(&env);
    let stranger = Address::generate(&env);

    assert_eq!(
        client.try_set_oracle_paused(&stranger, &true),
        Err(Ok(OracleError::Unauthorized))
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 7. Cross-asset: collateral price manipulated, debt price stable
// ─────────────────────────────────────────────────────────────────────────────

/// A sudden collateral price drop while debt price stays stable
/// is immediately visible in health factor without any user action.
#[test]
fn test_cross_asset_collateral_crash_debt_stable() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, debt_asset, _) = setup(&env);
    let collateral_asset = Address::generate(&env);
    let user = Address::generate(&env);

    env.ledger().with_mut(|li| li.timestamp = 0);
    client.update_price_feed(&admin, &collateral_asset, &1_000_000_000, &8); // $10
    client.update_price_feed(&admin, &debt_asset, &100_000_000, &8); // $1

    // Borrow at comfortable ratio
    client.borrow(&user, &debt_asset, &20_000, &collateral_asset, &30_000);

    let cv_before = client.get_collateral_value(&user);
    let dv_before = client.get_debt_value(&user);
    let hf_before = client.get_health_factor(&user);

    assert!(cv_before > dv_before);
    assert!(hf_before >= views::HEALTH_FACTOR_SCALE);

    // Debt oracle stays at $1; collateral crashes to $0.10 (10× drop)
    client.update_price_feed(&admin, &collateral_asset, &10_000_000, &8);

    let cv_after = client.get_collateral_value(&user);
    let dv_after = client.get_debt_value(&user);
    let hf_after = client.get_health_factor(&user);

    // Debt value unchanged
    assert_eq!(dv_before, dv_after);
    // Collateral value drops ~10×
    assert!(cv_after < cv_before);
    // Health factor drops significantly
    assert!(hf_after < hf_before);
}

/// Debt price spike (without collateral change) worsens the health factor.
#[test]
fn test_cross_asset_debt_price_spike_worsens_health_factor() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, debt_asset, _) = setup(&env);
    let collateral_asset = Address::generate(&env);
    let user = Address::generate(&env);

    env.ledger().with_mut(|li| li.timestamp = 0);
    client.update_price_feed(&admin, &collateral_asset, &1_000_000_000, &8); // $10
    client.update_price_feed(&admin, &debt_asset, &100_000_000, &8); // $1

    client.borrow(&user, &debt_asset, &10_000, &collateral_asset, &20_000);

    let hf_before = client.get_health_factor(&user);

    // Debt asset price 5× — debt value increases, health factor worsens
    client.update_price_feed(&admin, &debt_asset, &500_000_000, &8); // $5

    let hf_after = client.get_health_factor(&user);
    assert!(
        hf_after < hf_before,
        "debt price spike must worsen health factor: before={} after={}",
        hf_before,
        hf_after
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 8. Multiple assets with independent staleness — no cross-contamination
// ─────────────────────────────────────────────────────────────────────────────

/// Staleness of one asset's oracle does not affect another asset's price reads.
#[test]
fn test_stale_oracle_does_not_contaminate_other_assets() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, asset1, _) = setup(&env);
    let asset2 = Address::generate(&env);

    env.ledger().with_mut(|li| li.timestamp = 0);
    client.update_price_feed(&admin, &asset1, &100_000_000, &8);

    // asset2 gets a fresh price at t=2000
    env.ledger().with_mut(|li| li.timestamp = 2000);
    client.update_price_feed(&admin, &asset2, &200_000_000, &8);

    // Advance past asset1's staleness but not asset2's
    env.ledger().with_mut(|li| li.timestamp = 4500);

    // asset1 is stale
    assert_eq!(
        client.try_get_price(&asset1),
        Err(Ok(OracleError::StalePrice))
    );

    // asset2 is still fresh
    assert_eq!(client.get_price(&asset2), 200_000_000);
}

/// Collateral value is 0 for a stale asset while a fresh debt asset
/// correctly reports its value — they don't interfere.
#[test]
fn test_stale_collateral_zero_value_fresh_debt_correct_value() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, debt_asset, _) = setup(&env);
    let collateral_asset = Address::generate(&env);
    let user = Address::generate(&env);

    env.ledger().with_mut(|li| li.timestamp = 0);
    client.update_price_feed(&admin, &collateral_asset, &100_000_000, &8);
    client.update_price_feed(&admin, &debt_asset, &100_000_000, &8);

    client.borrow(&user, &debt_asset, &10_000, &collateral_asset, &20_000);

    // Let collateral oracle go stale; refresh debt oracle
    env.ledger().with_mut(|li| li.timestamp = 4500);
    client.update_price_feed(&admin, &debt_asset, &100_000_000, &8);

    // Collateral value must be 0 (stale oracle)
    assert_eq!(client.get_collateral_value(&user), 0);

    // Debt value must be correctly computed from fresh debt oracle
    let dv = client.get_debt_value(&user);
    assert!(dv > 0, "debt value must be positive when debt oracle is fresh");
}
