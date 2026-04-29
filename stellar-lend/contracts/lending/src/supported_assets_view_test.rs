//! Tests for `get_supported_assets` view function (#698).
//!
//! Covers:
//! - Empty registry returns empty vec
//! - Returned assets match registered assets exactly (core registry invariant)
//! - Pagination (offset + page_size) is correct and bounded
//! - page_size cap at 20 is enforced
//! - Deregistered assets are absent
//! - Assets registered without AssetParams return zeroed fields (not a panic)
//! - Assets registered with AssetParams return correct fields
//! - Registry cap (MAX_REGISTERED_ASSETS = 64) prevents overflow
//! - View is read-only (no state change across repeated calls)
//! - Order is stable across repeated reads

use crate::asset_registry::{self, MAX_REGISTERED_ASSETS};
use crate::views::{get_supported_assets, SupportedAssetInfo};
use crate::cross_asset::AssetParams;
use soroban_sdk::{
    testutils::Address as _,
    Address, Env,
};

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn fresh_env() -> Env {
    Env::default()
}

/// Register `n` fresh addresses and return them in registration order.
fn register_n(env: &Env, n: usize) -> Vec<Address> {
    (0..n)
        .map(|_| {
            let a = Address::generate(env);
            asset_registry::register(env, &a).expect("register failed");
            a
        })
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// Empty registry
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_empty_registry_returns_empty_vec() {
    let env = fresh_env();
    let result = get_supported_assets(&env, 0, 20);
    assert_eq!(result.len(), 0);
}

#[test]
fn test_empty_registry_nonzero_offset_returns_empty() {
    let env = fresh_env();
    let result = get_supported_assets(&env, 5, 20);
    assert_eq!(result.len(), 0);
}

// ─────────────────────────────────────────────────────────────────────────────
// Core invariant: returned set == registered set
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_returned_assets_match_registered_assets_exactly() {
    let env = fresh_env();
    let registered = register_n(&env, 5);
    let page = get_supported_assets(&env, 0, 20);
    assert_eq!(page.len() as usize, registered.len());
    for (i, info) in page.iter().enumerate() {
        assert_eq!(
            info.asset, registered[i],
            "asset at index {i} does not match"
        );
        // Each address must also pass is_registered
        assert!(
            asset_registry::is_registered(&env, &info.asset),
            "returned asset not in registry"
        );
    }
}

#[test]
fn test_single_asset_registered_appears_in_view() {
    let env = fresh_env();
    let asset = Address::generate(&env);
    asset_registry::register(&env, &asset).unwrap();
    let page = get_supported_assets(&env, 0, 20);
    assert_eq!(page.len(), 1);
    assert_eq!(page.get(0).unwrap().asset, asset);
}

// ─────────────────────────────────────────────────────────────────────────────
// Deregistration
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_deregistered_asset_absent_from_view() {
    let env = fresh_env();
    let assets = register_n(&env, 3);
    // Deregister the middle one
    asset_registry::deregister(&env, &assets[1]).unwrap();
    let page = get_supported_assets(&env, 0, 20);
    assert_eq!(page.len(), 2);
    let returned_addrs: std::vec::Vec<Address> = page.iter().map(|i| i.asset.clone()).collect();
    assert!(returned_addrs.contains(&assets[0]));
    assert!(!returned_addrs.contains(&assets[1]), "deregistered asset must be absent");
    assert!(returned_addrs.contains(&assets[2]));
}

#[test]
fn test_deregister_all_returns_empty_view() {
    let env = fresh_env();
    let assets = register_n(&env, 4);
    for a in &assets {
        asset_registry::deregister(&env, a).unwrap();
    }
    let page = get_supported_assets(&env, 0, 20);
    assert_eq!(page.len(), 0);
}

// ─────────────────────────────────────────────────────────────────────────────
// Pagination
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_pagination_first_page() {
    let env = fresh_env();
    let assets = register_n(&env, 10);
    let page = get_supported_assets(&env, 0, 5);
    assert_eq!(page.len(), 5);
    for (i, info) in page.iter().enumerate() {
        assert_eq!(info.asset, assets[i]);
    }
}

#[test]
fn test_pagination_second_page() {
    let env = fresh_env();
    let assets = register_n(&env, 10);
    let page = get_supported_assets(&env, 5, 5);
    assert_eq!(page.len(), 5);
    for (i, info) in page.iter().enumerate() {
        assert_eq!(info.asset, assets[5 + i]);
    }
}

#[test]
fn test_pagination_partial_last_page() {
    let env = fresh_env();
    let assets = register_n(&env, 7);
    let page = get_supported_assets(&env, 5, 10);
    assert_eq!(page.len(), 2);
    assert_eq!(page.get(0).unwrap().asset, assets[5]);
    assert_eq!(page.get(1).unwrap().asset, assets[6]);
}

#[test]
fn test_pagination_offset_beyond_end_returns_empty() {
    let env = fresh_env();
    register_n(&env, 3);
    let page = get_supported_assets(&env, 10, 5);
    assert_eq!(page.len(), 0);
}

#[test]
fn test_pagination_all_pages_cover_full_registry() {
    let env = fresh_env();
    let assets = register_n(&env, 15);
    let mut collected: std::vec::Vec<Address> = std::vec::Vec::new();
    let mut offset = 0u32;
    loop {
        let page = get_supported_assets(&env, offset, 5);
        if page.len() == 0 {
            break;
        }
        collected.extend(page.iter().map(|i| i.asset.clone()));
        offset += 5;
    }
    assert_eq!(collected.len(), 15);
    for (i, a) in assets.iter().enumerate() {
        assert_eq!(&collected[i], a, "mismatch at index {i}");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// page_size cap at 20
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_page_size_capped_at_20() {
    let env = fresh_env();
    register_n(&env, 30);
    // Requesting 100 must return at most 20
    let page = get_supported_assets(&env, 0, 100);
    assert_eq!(page.len(), 20, "page_size must be capped at 20");
}

#[test]
fn test_page_size_zero_returns_empty() {
    let env = fresh_env();
    register_n(&env, 5);
    let page = get_supported_assets(&env, 0, 0);
    assert_eq!(page.len(), 0);
}

// ─────────────────────────────────────────────────────────────────────────────
// AssetParams fields
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_asset_with_params_returns_correct_fields() {
    let env = fresh_env();
    let asset = Address::generate(&env);
    let price_feed = Address::generate(&env);
    asset_registry::register(&env, &asset).unwrap();
    // Write AssetParams directly into persistent storage (mirrors set_asset_params)
    let params = AssetParams {
        ltv: 7500,
        liquidation_threshold: 8000,
        price_feed: price_feed.clone(),
        debt_ceiling: 1_000_000,
        is_active: true,
    };
    env.storage().persistent().set(
        &crate::cross_asset::CrossAssetKey::AssetParams(asset.clone()),
        &params,
    );
    let page = get_supported_assets(&env, 0, 20);
    assert_eq!(page.len(), 1);
    let info = page.get(0).unwrap();
    assert_eq!(info.asset, asset);
    assert_eq!(info.ltv_bps, 7500);
    assert_eq!(info.liquidation_threshold_bps, 8000);
    assert_eq!(info.price_feed, price_feed);
    assert_eq!(info.debt_ceiling, 1_000_000);
    assert!(info.is_active);
}

#[test]
fn test_asset_without_params_returns_zero_fields_not_panic() {
    let env = fresh_env();
    let asset = Address::generate(&env);
    // Register via boolean path only — no AssetParams written
    asset_registry::register(&env, &asset).unwrap();
    let page = get_supported_assets(&env, 0, 20);
    assert_eq!(page.len(), 1);
    let info = page.get(0).unwrap();
    assert_eq!(info.asset, asset);
    assert_eq!(info.ltv_bps, 0);
    assert_eq!(info.liquidation_threshold_bps, 0);
    assert_eq!(info.debt_ceiling, 0);
    assert!(!info.is_active);
}

#[test]
fn test_mixed_assets_with_and_without_params() {
    let env = fresh_env();
    let a1 = Address::generate(&env);
    let a2 = Address::generate(&env);
    let price_feed = Address::generate(&env);
    asset_registry::register(&env, &a1).unwrap();
    asset_registry::register(&env, &a2).unwrap();
    // Only a2 gets params
    env.storage().persistent().set(
        &crate::cross_asset::CrossAssetKey::AssetParams(a2.clone()),
        &AssetParams {
            ltv: 6000,
            liquidation_threshold: 7500,
            price_feed: price_feed.clone(),
            debt_ceiling: 500_000,
            is_active: true,
        },
    );
    let page = get_supported_assets(&env, 0, 20);
    assert_eq!(page.len(), 2);
    let i0 = page.get(0).unwrap();
    let i1 = page.get(1).unwrap();
    // a1: no params → zeroed
    assert_eq!(i0.ltv_bps, 0);
    assert!(!i0.is_active);
    // a2: has params → populated
    assert_eq!(i1.ltv_bps, 6000);
    assert_eq!(i1.liquidation_threshold_bps, 7500);
    assert!(i1.is_active);
}

// ─────────────────────────────────────────────────────────────────────────────
// Registry cap
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_registry_cap_prevents_overflow() {
    let env = fresh_env();
    // Fill to cap
    for _ in 0..MAX_REGISTERED_ASSETS {
        let a = Address::generate(&env);
        asset_registry::register(&env, &a).expect("should register up to cap");
    }
    // One more must fail
    let overflow = Address::generate(&env);
    let result = asset_registry::register(&env, &overflow);
    assert!(result.is_err(), "registration beyond cap must fail");
}

#[test]
fn test_registry_cap_view_never_exceeds_max() {
    let env = fresh_env();
    for _ in 0..MAX_REGISTERED_ASSETS {
        let a = Address::generate(&env);
        asset_registry::register(&env, &a).unwrap();
    }
    // Page everything — total must equal cap, never exceed it
    let mut total = 0u32;
    let mut offset = 0u32;
    loop {
        let page = get_supported_assets(&env, offset, 20);
        if page.len() == 0 {
            break;
        }
        total += page.len() as u32;
        offset += 20;
    }
    assert_eq!(total, MAX_REGISTERED_ASSETS);
}

// ─────────────────────────────────────────────────────────────────────────────
// Read-only invariant
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_view_is_read_only_no_state_change() {
    let env = fresh_env();
    let assets = register_n(&env, 4);
    // Call the view multiple times
    let p1 = get_supported_assets(&env, 0, 20);
    let _ = get_supported_assets(&env, 0, 20);
    let _ = get_supported_assets(&env, 0, 5);
    let p2 = get_supported_assets(&env, 0, 20);
    // Registry must be unchanged
    assert_eq!(p1.len(), p2.len());
    for (a, b) in p1.iter().zip(p2.iter()) {
        assert_eq!(a, b);
    }
    // All original assets still registered
    for a in &assets {
        assert!(asset_registry::is_registered(&env, a));
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Stable ordering
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_order_is_stable_across_repeated_reads() {
    let env = fresh_env();
    register_n(&env, 8);
    let first = get_supported_assets(&env, 0, 20);
    for _ in 0..10 {
        let again = get_supported_assets(&env, 0, 20);
        for (a, b) in first.iter().zip(again.iter()) {
            assert_eq!(a.asset, b.asset, "order drifted across repeated reads");
        }
    }
}

#[test]
fn test_order_is_insertion_order() {
    let env = fresh_env();
    let assets = register_n(&env, 6);
    let page = get_supported_assets(&env, 0, 20);
    for (i, info) in page.iter().enumerate() {
        assert_eq!(
            info.asset, assets[i],
            "index {i}: expected insertion order"
        );
    }
}