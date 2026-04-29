//! # Views — Read-only position and health factor queries
//!
//! Provides gas-efficient, read-only view functions for frontends and liquidations:
//! collateral value, debt value, health factor, and position summary.
//! All functions perform **no state changes** and use the admin-configured oracle for pricing.
//!
//! ## Security
//! - View functions do not modify contract or user state.
//! - Collateral and debt values depend on the oracle; ensure the oracle is correct and trusted.
//! - Health factor uses the admin-set liquidation threshold consistently.
//!
//! ## Serialization Stability
//! Public getter structs are treated as view schema `v1`.
//! Soroban `#[contracttype]` structs serialize as XDR maps keyed by field name, and the generated
//! conversion code sorts those keys lexicographically. Existing getter return structs must keep
//! their current field names and types stable; any additive or breaking change should ship as a
//! new versioned getter/type instead of mutating the existing schema in place.

use crate::borrow::{
    get_close_factor_bps, get_liquidation_incentive_bps, get_liquidation_threshold_bps, get_oracle,
    get_user_collateral, get_user_debt, BorrowCollateral, DebtPosition,
};
use crate::constants::BPS_SCALE;
use crate::oracle;
use soroban_sdk::{contracttype, Address, Env, IntoVal, Symbol, I256};

/// Scale for oracle price (1e8 = one unit). Value = amount * price / PRICE_SCALE.
const PRICE_SCALE: i128 = 100_000_000;

/// Health factor scale: 10000 = 1.0 (healthy). Below 10000 = liquidatable.
pub const HEALTH_FACTOR_SCALE: i128 = BPS_SCALE;

/// Sentinel health factor when user has no debt (position is healthy).
pub const HEALTH_FACTOR_NO_DEBT: i128 = 100_000_000;

/// Current schema version for public getter structs documented in this contract.
pub const VIEW_SCHEMA_VERSION: u32 = 1;

/// Summary of a user's borrow position for frontends and liquidations.
///
/// All value fields use a common unit (e.g. USD with 8 decimals) when oracle is set.
/// When oracle is not set, `collateral_value` and `debt_value` are 0 and `health_factor` is 0.
/// Serialization contract: this struct is exposed as view schema `v1`. Preserve the current field
/// names and types for `get_user_position`; ship a new versioned getter/type for any schema change.
#[contracttype]
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UserPositionSummary {
    /// User's collateral balance (raw amount)
    pub collateral_balance: i128,
    /// Collateral value in common unit (e.g. USD 8 decimals). 0 if oracle not set.
    pub collateral_value: i128,
    /// User's debt balance (principal + accrued interest)
    pub debt_balance: i128,
    /// Debt value in common unit. 0 if oracle not set.
    pub debt_value: i128,
    /// Health factor scaled by 10000 (10000 = 1.0). 0 if oracle not set or unconfigured.
    pub health_factor: i128,
}

/// Fetches price for `asset`.
///
/// Resolution order:
/// 1. Oracle module (`update_price_feed` path) — primary then fallback, with staleness checks.
/// 2. Legacy oracle contract (`set_oracle` path) — for backward compatibility.
///
/// Returns `None` if no fresh price is available.
#[inline]
fn get_asset_price(env: &Env, asset: &Address) -> Option<i128> {
    // Prefer the hardened oracle module
    if let Ok(price) = oracle::get_price(env, asset) {
        return Some(price);
    }
    // Fall back to legacy oracle contract if configured
    let oracle_addr = get_oracle(env)?;
    let price: i128 = env.invoke_contract(
        &oracle_addr,
        &Symbol::new(env, "price"),
        (asset.clone(),).into_val(env),
    );
    if price > 0 {
        Some(price)
    } else {
        None
    }
}

/// Computes collateral value in common unit (amount * price / PRICE_SCALE).
/// Returns 0 if no fresh price is available or amount is zero.
#[inline]
pub(crate) fn collateral_value(env: &Env, collateral: &BorrowCollateral) -> i128 {
    if collateral.amount <= 0 {
        return 0;
    }
    let Some(price) = get_asset_price(env, &collateral.asset) else {
        return 0;
    };
    if price <= 0 {
        return 0;
    }
    let amount_256 = I256::from_i128(env, collateral.amount);
    let price_256 = I256::from_i128(env, price);
    let scale_256 = I256::from_i128(env, PRICE_SCALE);
    let val_256 = amount_256.mul(&price_256).div(&scale_256);
    val_256.to_i128().unwrap_or(0)
}

/// Computes debt value in common unit (total debt * price / PRICE_SCALE).
/// Returns 0 if no fresh price is available or debt is zero.
#[inline]
pub(crate) fn debt_value(env: &Env, position: &DebtPosition) -> i128 {
    let total_debt = position
        .borrowed_amount
        .checked_add(position.interest_accrued)
        .unwrap_or(0);
    if total_debt <= 0 {
        return 0;
    }
    let Some(price) = get_asset_price(env, &position.asset) else {
        return 0;
    };
    if price <= 0 {
        return 0;
    }
    let debt_256 = I256::from_i128(env, total_debt);
    let price_256 = I256::from_i128(env, price);
    let scale_256 = I256::from_i128(env, PRICE_SCALE);
    let val_256 = debt_256.mul(&price_256).div(&scale_256);
    val_256.to_i128().unwrap_or(0)
}

/// Computes health factor from collateral value, debt value, and liquidation threshold.
///
/// Formula: `health_factor = (collateral_value * liquidation_threshold_bps / 10000) * HEALTH_FACTOR_SCALE / debt_value`
/// So 10000 = 1.0; above 10000 is healthy, below is liquidatable.
///
/// Returns `HEALTH_FACTOR_NO_DEBT` when debt is zero (position is healthy).
/// Returns 0 when no fresh price is available but user has debt (cannot compute).
#[inline]
pub(crate) fn compute_health_factor(
    env: &Env,
    collateral_value: i128,
    debt_value: i128,
    has_debt: bool,
) -> i128 {
    if debt_value <= 0 {
        if has_debt {
            return 0; // No price available; cannot compute
        }
        return HEALTH_FACTOR_NO_DEBT;
    }
    let bps = get_liquidation_threshold_bps(env);
    let collat_256 = I256::from_i128(env, collateral_value);
    let bps_256 = I256::from_i128(env, bps);
    let hf_scale_256 = I256::from_i128(env, HEALTH_FACTOR_SCALE);
    let debt_256 = I256::from_i128(env, debt_value);

    let weighted_collateral = collat_256
        .mul(&bps_256)
        .div(&I256::from_i128(env, BPS_SCALE));

    let hf_256 = weighted_collateral.mul(&hf_scale_256).div(&debt_256);
    hf_256.to_i128().unwrap_or(0)
}

// ═══════════════════════════════════════════════════════════════════════════
// Public view functions (read-only; no state changes)
// ═══════════════════════════════════════════════════════════════════════════

/// Returns the user's collateral balance (raw amount and asset from borrow position).
///
/// # Arguments
/// * `env` - Contract environment
/// * `user` - User address
///
/// # Returns
/// The stored collateral amount. 0 if user has no collateral.
///
/// # Security
/// Read-only; no state change. Uses existing borrow storage.
pub fn get_collateral_balance(env: &Env, user: &Address) -> i128 {
    let collateral = get_user_collateral(env, user);
    collateral.amount
}

/// Returns the user's debt balance (principal + accrued interest).
///
/// # Arguments
/// * `env` - Contract environment
/// * `user` - User address
///
/// # Returns
/// Total debt in raw units. 0 if user has no debt.
///
/// # Security
/// Read-only; no state change. Uses existing borrow storage and interest accrual.
pub fn get_debt_balance(env: &Env, user: &Address) -> i128 {
    let position = get_user_debt(env, user);
    position
        .borrowed_amount
        .checked_add(position.interest_accrued)
        .unwrap_or(0)
}

/// Returns the user's collateral value in the common unit (e.g. USD 8 decimals).
///
/// Uses the admin-configured oracle. Returns 0 if oracle is not set or price unavailable.
///
/// # Security
/// Read-only; no state change. Oracle is trusted (admin-configured).
pub fn get_collateral_value(env: &Env, user: &Address) -> i128 {
    let collateral = get_user_collateral(env, user);
    collateral_value(env, &collateral)
}

/// Returns the user's debt value in the common unit (e.g. USD 8 decimals).
///
/// Uses the admin-configured oracle. Returns 0 if oracle is not set or price unavailable.
///
/// # Security
/// Read-only; no state change. Oracle is trusted (admin-configured).
pub fn get_debt_value(env: &Env, user: &Address) -> i128 {
    let position = get_user_debt(env, user);
    debt_value(env, &position)
}

/// Returns the user's health factor (scaled by 10000; 10000 = 1.0).
///
/// Computed from collateral value, debt value, and liquidation threshold.
/// - Above 10000: healthy
/// - Below 10000: liquidatable
/// - Returns `HEALTH_FACTOR_NO_DEBT` when user has no debt
/// - Returns 0 when oracle is not set or values cannot be computed
///
/// # Security
/// Read-only; no state change. Correct oracle and liquidation threshold usage.
pub fn get_health_factor(env: &Env, user: &Address) -> i128 {
    let collateral = get_user_collateral(env, user);
    let position = get_user_debt(env, user);
    let debt_balance = position
        .borrowed_amount
        .checked_add(position.interest_accrued)
        .unwrap_or(0);
    let cv = collateral_value(env, &collateral);
    let dv = debt_value(env, &position);
    compute_health_factor(env, cv, dv, debt_balance > 0)
}

/// Returns the maximum debt amount that can be liquidated for `user` in one call.
///
/// This is the primary view function for liquidation bots and frontends to determine
/// how much of a borrower's debt can be repaid in a single `liquidate` call.
/// The value is consistent with the close-factor cap enforced inside `liquidate_position`.
///
/// ## Returns 0 when
/// - User has no outstanding debt (`borrowed_amount + interest_accrued == 0`)
/// - Position is healthy (health factor ≥ `HEALTH_FACTOR_SCALE`, i.e. ≥ 1.0)
/// - Oracle is not configured or returns no fresh price (health factor cannot be computed)
///
/// ## Formula
/// ```text
/// total_debt  = borrowed_amount + interest_accrued
/// max_liq     = floor(total_debt * close_factor_bps / 10_000)
/// ```
///
/// ## Rounding and unit scales
/// - All amounts are in raw token units (no decimal assumption by the contract).
/// - Oracle price uses 8-decimal fixed-point: `100_000_000 = 1.0`.
/// - BPS scale: `10_000 = 100%`.
/// - Division is integer floor: `floor(10_001 * 5_000 / 10_000) = 5_000`, not 5_000.5.
/// - Interest accrual in `borrow.rs` uses ceiling-up rounding; the stored
///   `interest_accrued` field already reflects that rounding before this view reads it.
///
/// ## Consistency with `liquidate`
/// `liquidate_position` calls this function internally to derive the close-factor cap,
/// so the value returned here is exactly the amount that will be repaid when
/// `liquidate` is called with an amount larger than the cap.
///
/// ## Cross-asset positions
/// This function reads only the simplified single-asset borrow position
/// (`get_user_debt` / `get_user_collateral`). Cross-asset positions tracked by
/// the `cross_asset` module are not reflected here; use `get_cross_position_summary`
/// for those.
///
/// # Security
/// Read-only; no state change. Relies on oracle for health factor; 0 is returned
/// if oracle is absent so the caller cannot liquidate without price data.
pub fn get_max_liquidatable_amount(env: &Env, user: &Address) -> i128 {
    let position = get_user_debt(env, user);
    let total_debt = position
        .borrowed_amount
        .checked_add(position.interest_accrued)
        .unwrap_or(0);
    if total_debt <= 0 {
        return 0;
    }
    let collateral = get_user_collateral(env, user);
    let cv = collateral_value(env, &collateral);
    let dv = debt_value(env, &position);
    let hf = compute_health_factor(env, cv, dv, true);
    // hf == 0 means oracle is missing; healthy or unknown → not liquidatable
    if hf == 0 || hf >= HEALTH_FACTOR_SCALE {
        return 0;
    }
    let close_factor = get_close_factor_bps(env);
    let debt_256 = I256::from_i128(env, total_debt);
    let cf_256 = I256::from_i128(env, close_factor);
    let result = debt_256.mul(&cf_256).div(&I256::from_i128(env, BPS_SCALE));
    result.to_i128().unwrap_or(0)
}

/// Returns the collateral bonus amount a liquidator receives for repaying `repay_amount` of debt.
///
/// Formula: `repay_amount * (10000 + incentive_bps) / 10000`
///
/// Returns 0 for zero or negative `repay_amount`.
/// Uses saturating semantics: returns `i128::MAX` on overflow instead of panicking.
///
/// # Security
/// Read-only; no state change. Incentive bounds are enforced by admin setter (0–10000 bps).
pub fn get_liquidation_incentive_amount(env: &Env, repay_amount: i128) -> i128 {
    if repay_amount <= 0 {
        return 0;
    }
    let incentive_bps = get_liquidation_incentive_bps(env);
    let amount_256 = I256::from_i128(env, repay_amount);
    let scale_256 = I256::from_i128(env, BPS_SCALE + incentive_bps);
    let result = amount_256
        .mul(&scale_256)
        .div(&I256::from_i128(env, BPS_SCALE));
    result.to_i128().unwrap_or(i128::MAX)
}

/// Returns a full position summary for the user (collateral balance/value, debt balance/value, health factor).
///
/// Single read-only call for frontends and liquidation bots.
///
/// # Security
/// Read-only; no state change. Correct oracle and liquidation threshold usage.
pub fn get_user_position(env: &Env, user: &Address) -> UserPositionSummary {
    let collateral = get_user_collateral(env, user);
    let position = get_user_debt(env, user);
    let debt_balance = position
        .borrowed_amount
        .checked_add(position.interest_accrued)
        .unwrap_or(0);
    let collateral_value_usd = collateral_value(env, &collateral);
    let debt_value_usd = debt_value(env, &position);
    let health_factor =
        compute_health_factor(env, collateral_value_usd, debt_value_usd, debt_balance > 0);

    UserPositionSummary {
        collateral_balance: collateral.amount,
        collateral_value: collateral_value_usd,
        debt_balance,
        debt_value: debt_value_usd,
        health_factor,
    }
}

+pub struct SupportedAssetInfo {
    +    /// The asset's contract address.
    +    pub asset: Address,
    +    /// Loan-to-value ratio in basis points (e.g. 7500 = 75%).
    +    pub ltv_bps: i128,
    +    /// Liquidation threshold in basis points (e.g. 8000 = 80%).
    +    pub liquidation_threshold_bps: i128,
    +    /// Oracle / price-feed address configured for this asset.
    +    pub price_feed: Address,
    +    /// Maximum debt allowed for this asset (protocol debt ceiling).
    +    pub debt_ceiling: i128,
    +    /// Whether the asset is currently active / accepting new positions.
    +    pub is_active: bool,
    +}
    +
    +/// Returns up to `page_size` supported assets starting at `offset`.
    +///
    +/// Assets that have no `AssetParams` recorded (registered via the simple boolean
    +/// path only) are included with zeroed numeric fields so the list stays consistent
    +/// with `is_asset_registered`. Frontends should treat `is_active = false` as
    +/// read-only.
    +///
    +/// # Pagination
    +/// - `offset` — zero-based index into the full registered-asset list.
    +/// - `page_size` — max entries to return; capped internally at 20 to bound
    +///   ledger I/O per call. Pass `page_size = 20` and increment `offset` by 20
    +///   to paginate through large registries.
    +///
    +/// Returns an empty `Vec` when `offset >= total registered assets`.
    +///
    +/// # Security
    +/// Read-only; no state change. Bounded by `MAX_REGISTERED_ASSETS` on the
    +/// registry side, and by the `page_size` cap here, so gas usage is deterministic.
    +pub fn get_supported_assets(env: &Env, offset: u32, page_size: u32) -> Vec<SupportedAssetInfo> {
    +    const MAX_PAGE: u32 = 20;
    +    let capped = page_size.min(MAX_PAGE);
    +    let all = asset_registry::list_registered(env);
    +    let total = all.len();
    +    let mut result: Vec<SupportedAssetInfo> = Vec::new(env);
    +    if offset >= total {
    +        return result;
    +    }
    +    let end = (offset + capped).min(total);
    +    for i in offset..end {
    +        let asset = all.get(i).unwrap();
    +        // Try to load cross-asset params; fall back to zeroed entry so the
    +        // list is always consistent with the boolean registry.
    +        let params_opt: Option<AssetParams> = env
    +            .storage()
    +            .persistent()
    +            .get(&crate::cross_asset::CrossAssetKey::AssetParams(asset.clone()));
    +        let info = match params_opt {
    +            Some(p) => SupportedAssetInfo {
    +                asset: asset.clone(),
    +                ltv_bps: p.ltv,
    +                liquidation_threshold_bps: p.liquidation_threshold,
    +                price_feed: p.price_feed,
    +                debt_ceiling: p.debt_ceiling,
    +                is_active: p.is_active,
    +            },
    +            None => SupportedAssetInfo {
    +                asset: asset.clone(),
    +                ltv_bps: 0,
    +                liquidation_threshold_bps: 0,
    +                price_feed: asset.clone(), // placeholder; no oracle configured
    +                debt_ceiling: 0,
    +                is_active: false,
    +            },
    +        };
    +        result.push_back(info);
    +    }
    +    result
    +}
    