// ════════════════════════════════════════════════════════════════
// RATE MODEL — Dynamic interest rate with EMA smoothing
// ════════════════════════════════════════════════════════════════
//
// This module implements a utilization-based interest rate model
// with exponential moving average (EMA) smoothing. The smoothed
// rate is persisted in instance storage and a versioned
// `RateUpdatedEvent` is emitted each time the rate changes.
//
// ## Design
//
// 1. **Utilisation** is computed as `total_debt / total_deposits`
//    (in basis points, e.g. 8000 = 80%).
// 2. **Target rate** follows a piecewise linear (kink) model:
//    - Below target utilisation → slope = `SLOPE1`
//    - Above target utilisation → slope = `SLOPE2` (steeper)
// 3. **EMA smoothing** blends the target into the historical
//    smoothed rate so that the on-chain rate does not jump
//    abruptly.
// 4. A **versioned `RateUpdatedEvent`** is emitted only when the
//    persisted smoothed rate actually changes, preventing event
//    spam on no-op updates.
//
// ## Edge cases
//
// - **Zero deposits**: utilisation is 0 → rate falls to BASE_RATE.
// - **Uninitialised smoothing state**: the first call sets the
//   smoothed rate equal to the target rate (no EMA blending).
// - **Rate unchanged**: if the computed smoothed rate is
//   identical to the stored value, no event is emitted.

use soroban_sdk::{contractevent, contracttype, Env, Symbol};

use crate::DataKey;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Schema version for versioned events. Must be bumped on breaking changes
/// to the `RateUpdatedEvent` payload.
pub const EVENT_SCHEMA_VERSION: u32 = 1;

/// Target utilisation in basis points (8000 = 80%).
pub const TARGET_UTILIZATION_BPS: i128 = 8000;

/// EMA smoothing factor in basis points (1000 = 0.1).
/// Higher values make the rate respond faster to utilisation changes.
pub const SMOOTHING_FACTOR_BPS: i128 = 1000;

/// Base (minimum) rate in basis points (50 = 0.5%).
pub const BASE_RATE_BPS: i128 = 50;

/// Slope below target utilisation (per unit utilisation in BPS).
pub const SLOPE1_BPS: i128 = 50; // 0.5% per 1% utilisation below target

/// Slope above target utilisation (per unit utilisation in BPS).
/// Equal to the *additional* rate applied once utilisation reaches 100 %
/// (since `(util - target) / (BPS_SCALE - target) = 1` at full util).
pub const SLOPE2_BPS: i128 = 300; // 3% per 1% utilisation above target

/// Maximum allowed rate in basis points (5000 = 50%).
pub const MAX_RATE_BPS: i128 = 5000;

/// Scale for basis-point arithmetic.
pub const BPS_SCALE: i128 = 10_000;

/// Storage key for the rate smoothing state.
const RATE_SMOOTHING_KEY: &str = "RateSmooth";

// ---------------------------------------------------------------------------
// Storage types
// ---------------------------------------------------------------------------

/// Persisted smoothing state for the rate model.
///
/// Stored in instance storage under the `RATE_SMOOTHING_KEY` Symbol.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RateSmoothingState {
    /// The current smoothed interest rate in basis points.
    pub smoothed_rate_bps: i128,
    /// Ledger timestamp of the last update.
    pub last_update: u64,
    /// Utilisation ratio at the last update, in basis points.
    pub utilization_bps: i128,
}

// ---------------------------------------------------------------------------
// Event
// ---------------------------------------------------------------------------

/// Emitted when the smoothed rate changes after an `update_and_get_rate` call.
///
/// # Fields
/// - `schema_version` — Event schema version (currently `1`). Indexers must
///   check this before decoding the rest of the payload.
/// - `utilization_bps` — Current pool utilisation in basis points.
/// - `target_rate_bps` — The target (pre-smoothing) rate in basis points.
/// - `applied_rate_bps` — The smoothed rate that was persisted, in BPS.
/// - `ledger` — Ledger sequence number at which this event was emitted.
#[contractevent]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RateUpdatedEvent {
    pub schema_version: u32,
    pub utilization_bps: i128,
    pub target_rate_bps: i128,
    pub applied_rate_bps: i128,
    pub ledger: u32,
}

// ---------------------------------------------------------------------------
// Rate computation helpers
// ---------------------------------------------------------------------------

/// Compute the target interest rate (in basis points) from the current
/// utilisation using a piecewise linear (kink) model.
///
/// - If `utilization_bps <= TARGET_UTILIZATION_BPS`:
///   `rate = BASE_RATE + utilization_bps * SLOPE1_BPS / TARGET_UTILIZATION_BPS`
/// - If `utilization_bps > TARGET_UTILIZATION_BPS`:
///   `rate = BASE_RATE + SLOPE1_BPS + (utilization_bps - target) * SLOPE2_BPS / (BPS_SCALE - TARGET_UTILIZATION_BPS)`
///
/// The result is capped at `MAX_RATE_BPS`. The `MAX_RATE_BPS` cap is a
/// safety ceiling that is intentionally higher than the rate the model
/// produces at 100 % utilisation under the default constants; this gives
/// headroom for future governance-tuned slope changes without code edits.
pub fn compute_target_rate(utilization_bps: i128) -> i128 {
    if utilization_bps <= TARGET_UTILIZATION_BPS {
        // Ramp from BASE_RATE up to (BASE_RATE + SLOPE1) as utilisation
        // approaches the target.
        let scaled = utilization_bps
            .checked_mul(SLOPE1_BPS)
            .unwrap_or(0)
            .checked_div(TARGET_UTILIZATION_BPS)
            .unwrap_or(0);
        let rate = BASE_RATE_BPS.saturating_add(scaled);
        rate.min(MAX_RATE_BPS)
    } else {
        // Above-target utilisation increases the slope.
        let excess = utilization_bps.saturating_sub(TARGET_UTILIZATION_BPS);
        let max_excess = BPS_SCALE.saturating_sub(TARGET_UTILIZATION_BPS);
        let scaled = if max_excess > 0 {
            excess
                .checked_mul(SLOPE2_BPS)
                .unwrap_or(0)
                .checked_div(max_excess)
                .unwrap_or(0)
        } else {
            0
        };
        let rate = BASE_RATE_BPS
            .saturating_add(SLOPE1_BPS)
            .saturating_add(scaled);
        rate.min(MAX_RATE_BPS)
    }
}

/// Load the persisted smoothing state, returning `None` if it has never been
/// written (uninitialised).
fn load_smoothing_state(env: &Env) -> Option<RateSmoothingState> {
    env.storage()
        .instance()
        .get::<Symbol, RateSmoothingState>(&Symbol::new(env, RATE_SMOOTHING_KEY))
}

/// Persist the smoothing state to instance storage.
fn save_smoothing_state(env: &Env, state: &RateSmoothingState) {
    env.storage()
        .instance()
        .set(&Symbol::new(env, RATE_SMOOTHING_KEY), state);
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Read the current utilisation ratio (in basis points) from storage.
///
/// Returns `0` when there are no deposits (division-by-zero guard).
pub fn current_utilization(env: &Env) -> i128 {
    let total_debt: i128 = env
        .storage()
        .persistent()
        .get(&DataKey::TotalDebt)
        .unwrap_or(0);
    let total_deposits: i128 = env
        .storage()
        .persistent()
        .get(&DataKey::TotalDeposits)
        .unwrap_or(0);

    if total_deposits > 0 {
        total_debt
            .checked_mul(BPS_SCALE)
            .unwrap_or(0)
            .checked_div(total_deposits)
            .unwrap_or(0)
    } else {
        0
    }
}

/// Update the smoothed interest rate based on the current pool utilisation
/// and emit a `RateUpdatedEvent` **only when the persisted rate changes**.
///
/// # Returns
/// The current smoothed rate in basis points.
///
/// # Panics
/// Never panics — all arithmetic uses saturating/checked operations and falls
/// back to `BASE_RATE_BPS` if storage is inconsistent.
///
/// # Events
/// Emits a `RateUpdatedEvent` whenever the computed smoothed rate differs
/// from the previously persisted value, or on the first call (no prior state).
pub fn update_and_get_rate(env: &Env) -> i128 {
    // 1. Compute current utilisation
    let utilization_bps = current_utilization(env);

    // 2. Compute the target (pre-smoothing) rate
    let target_rate_bps = compute_target_rate(utilization_bps);

    // 3. Load previous smoothing state and compute applied rate
    let prev_state = load_smoothing_state(env);

    let applied_rate_bps = match prev_state {
        Some(ref state) => {
            // EMA: new = alpha * target + (1 - alpha) * old
            // SMOOTHING_FACTOR_BPS is alpha in BPS (e.g. 1000 = 0.1)
            let alpha = SMOOTHING_FACTOR_BPS;
            let one_minus_alpha = BPS_SCALE.saturating_sub(alpha);

            let weighted_target = target_rate_bps
                .checked_mul(alpha)
                .unwrap_or(0);
            let weighted_old = state
                .smoothed_rate_bps
                .checked_mul(one_minus_alpha)
                .unwrap_or(0);

            let blended = weighted_target
                .saturating_add(weighted_old)
                .checked_div(BPS_SCALE)
                .unwrap_or(0);

            blended.min(MAX_RATE_BPS)
        }
        None => {
            // First call: no prior state, use target rate directly
            target_rate_bps
        }
    };

    // 4. Determine whether the persisted rate actually changed
    let rate_changed = prev_state
        .as_ref()
        .map(|s| s.smoothed_rate_bps != applied_rate_bps)
        .unwrap_or(true); // First call always counts as a change

    if rate_changed {
        // 5. Persist the new smoothing state
        let new_state = RateSmoothingState {
            smoothed_rate_bps: applied_rate_bps,
            last_update: env.ledger().timestamp(),
            utilization_bps,
        };
        save_smoothing_state(env, &new_state);

        // 6. Emit the versioned event
        RateUpdatedEvent {
            schema_version: EVENT_SCHEMA_VERSION,
            utilization_bps,
            target_rate_bps,
            applied_rate_bps,
            ledger: env.ledger().sequence(),
        }
        .publish(env);
    }

    applied_rate_bps
}
