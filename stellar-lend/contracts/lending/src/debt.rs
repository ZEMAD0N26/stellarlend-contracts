use soroban_sdk::{contracttype, Address, Env};

use crate::rounding_strategy::{calculate_interest_with_rounding, RoundingError, RoundingMode};
use crate::DataKey;

pub const DEFAULT_APR_BPS: i128 = 500;

/// Fixed-point scale for the global borrow index (10^7 = 7 decimal places).
///
/// The index starts at `INDEX_SCALE` (representing 1.0) and grows
/// monotonically as interest accrues.  A position's current debt is:
///
/// ```text
/// current_debt = principal × current_index / borrow_index_snapshot
/// ```
pub const INDEX_SCALE: i128 = 10_000_000; // 10^7

/// Seconds in a 365-day year, shared with rounding_strategy.
const SECONDS_PER_YEAR: u64 = 365 * 24 * 60 * 60; // 31_536_000

// ---------------------------------------------------------------------------
// DebtPosition
// ---------------------------------------------------------------------------

/// Per-borrower debt record.
///
/// Layout change (global-borrow-index feature):
/// - `last_update` is **removed**; the global `LastIndexUpdate` timestamp
///   drives time tracking.
/// - `borrow_index_snapshot` is added; it holds the value of
///   `DataKey::BorrowIndex` at the time the position was last touched.
///
/// Migration: pre-existing positions without a snapshot are treated as
/// having `borrow_index_snapshot == 0`, which `migrate_positions` fixes
/// by writing the current index into every such record before normal
/// operations resume.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DebtPosition {
    /// Recorded principal at last touch (does not include un-accrued interest).
    pub principal: i128,
    /// Snapshot of the global borrow index at the time this position was last
    /// modified.  Zero signals "pre-migration; treat as current index".
    pub borrow_index_snapshot: i128,
    /// Wall-clock timestamp of the last explicit settlement, kept for
    /// backward-compatible reads.  Updated on every position write.
    pub last_update: u64,
}

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DebtError {
    Overflow,
    InvalidAmount,
    IndexInvariantViolated,
}

impl From<&'static str> for DebtError {
    fn from(_: &'static str) -> Self {
        DebtError::Overflow
    }
}

impl From<RoundingError> for DebtError {
    fn from(_: RoundingError) -> Self {
        DebtError::Overflow
    }
}

// ---------------------------------------------------------------------------
// Storage helpers
// ---------------------------------------------------------------------------

/// Load a debt position from persistent storage.
///
/// Returns a default zero-principal position if none is stored.
/// The default snapshot is set to `INDEX_SCALE` (1.0) so that a brand-new
/// position accrues no phantom interest.
pub fn load_debt(env: &Env, user: &Address) -> DebtPosition {
    let key = DataKey::Debt(user.clone());
    env.storage()
        .persistent()
        .get(&key)
        .unwrap_or(DebtPosition {
            principal: 0,
            borrow_index_snapshot: INDEX_SCALE,
            last_update: env.ledger().timestamp(),
        })
}

/// Persist a debt position to storage.
pub fn save_debt(env: &Env, user: &Address, position: &DebtPosition) {
    let key = DataKey::Debt(user.clone());
    env.storage().persistent().set(&key, position);
}

// ---------------------------------------------------------------------------
// Global borrow index helpers
// ---------------------------------------------------------------------------

/// Load the current global borrow index.
///
/// Returns `INDEX_SCALE` (1.0) if the index has not yet been written
/// (first-ever call before `initialize`).
pub fn load_borrow_index(env: &Env) -> i128 {
    env.storage()
        .instance()
        .get(&DataKey::BorrowIndex)
        .unwrap_or(INDEX_SCALE)
}

/// Persist the global borrow index.
pub fn save_borrow_index(env: &Env, index: i128) {
    env.storage()
        .instance()
        .set(&DataKey::BorrowIndex, &index);
}

/// Load the timestamp of the last index update.
///
/// Returns the current ledger timestamp if none is stored (bootstrapping).
pub fn load_last_index_update(env: &Env) -> u64 {
    env.storage()
        .instance()
        .get(&DataKey::LastIndexUpdate)
        .unwrap_or_else(|| env.ledger().timestamp())
}

/// Persist the last-index-update timestamp.
pub fn save_last_index_update(env: &Env, ts: u64) {
    env.storage()
        .instance()
        .set(&DataKey::LastIndexUpdate, &ts);
}

// ---------------------------------------------------------------------------
// Index accrual
// ---------------------------------------------------------------------------

/// Advance the global borrow index by `elapsed` seconds at `rate_bps`.
///
/// Formula:
/// ```text
/// new_index = current_index + current_index * rate_bps * elapsed
///             / (SECONDS_PER_YEAR * BPS_DENOM)
/// ```
///
/// All intermediate multiplications use `checked_*` to detect overflow.
/// Returns the new (or unchanged, if elapsed == 0 or rate == 0) index.
///
/// # Overflow guard
/// If the new index would exceed `i128::MAX / INDEX_SCALE` the function
/// panics with `"BorrowIndex: overflow guard triggered"`.
///
/// # Monotonicity guarantee
/// The returned value is always `>= current_index`.
pub fn accrue_index(current_index: i128, elapsed: u64, rate_bps: i128) -> i128 {
    if elapsed == 0 || rate_bps == 0 {
        return current_index;
    }

    // Overflow guard: reject indices already dangerously large.
    let max_safe_index = i128::MAX / INDEX_SCALE;
    if current_index > max_safe_index {
        panic!("BorrowIndex: overflow guard triggered");
    }

    // delta = current_index * rate_bps * elapsed / (SECONDS_PER_YEAR * BPS_DENOM)
    let bps_denom: i128 = 10_000;
    let secs_per_year: i128 = SECONDS_PER_YEAR as i128;

    let step1 = current_index
        .checked_mul(rate_bps)
        .expect("BorrowIndex: overflow in rate multiplication");

    let step2 = step1
        .checked_mul(elapsed as i128)
        .expect("BorrowIndex: overflow in elapsed multiplication");

    let denominator = secs_per_year
        .checked_mul(bps_denom)
        .expect("BorrowIndex: denominator overflow");

    let delta = step2
        .checked_div(denominator)
        .expect("BorrowIndex: division by zero in accrual");

    let new_index = current_index
        .checked_add(delta)
        .expect("BorrowIndex: overflow on add");

    // Enforce monotonicity: never let index decrease.
    new_index.max(current_index)
}

/// Lazily advance the global borrow index to `now` and persist both the new
/// index value and the updated timestamp.
///
/// This is the single "touch" entry-point called by every mutating protocol
/// operation (borrow, repay, liquidate, migrate).
///
/// Returns the updated index value so callers can use it immediately without
/// a second storage round-trip.
pub fn touch_borrow_index(env: &Env, now: u64, rate_bps: i128) -> i128 {
    let current_index = load_borrow_index(env);
    let last_update = load_last_index_update(env);

    let elapsed = now.saturating_sub(last_update);
    let new_index = accrue_index(current_index, elapsed, rate_bps);

    // Only write if the index actually changed (saves a storage write on
    // same-block double-touches).
    if new_index != current_index {
        save_borrow_index(env, new_index);
    }
    save_last_index_update(env, now);
    new_index
}

// ---------------------------------------------------------------------------
// Per-position accrual (O(1) via index ratio)
// ---------------------------------------------------------------------------

/// Compute the current debt for a position using the index ratio:
///
/// ```text
/// current_debt = position.principal × current_index / snapshot_index
/// ```
///
/// Special cases:
/// - If `snapshot_index` is zero (pre-migration record), returns
///   `position.principal` unchanged (no phantom interest).
/// - If `current_index < snapshot_index` (should not happen under normal
///   operation), returns `position.principal` unchanged to avoid reducing
///   debt (Requirement 3.4 / monotonicity safety valve).
///
/// # Panics
/// Panics with a descriptive message if the multiplication overflows `i128`.
pub fn compute_debt(position: &DebtPosition, current_index: i128) -> i128 {
    let snapshot = position.borrow_index_snapshot;

    // Pre-migration record or degenerate state: treat accrued interest as zero.
    if snapshot <= 0 || current_index <= snapshot {
        return position.principal;
    }

    // principal * current_index / snapshot_index
    // Intermediate overflow check: principal * current_index must fit in i128.
    position
        .principal
        .checked_mul(current_index)
        .expect("compute_debt: principal × index overflow")
        .checked_div(snapshot)
        .expect("compute_debt: division by zero (snapshot)")
}

/// Settle a position's accrued interest into its principal and refresh the
/// index snapshot to `current_index`.
///
/// After settlement `position.principal` equals the full debt (including
/// interest), and `position.borrow_index_snapshot == current_index`.
///
/// Returns the settled `DebtPosition`.
pub fn settle_position(
    position: &DebtPosition,
    current_index: i128,
    now: u64,
) -> Result<DebtPosition, DebtError> {
    let new_principal = compute_debt(position, current_index);

    if new_principal < position.principal {
        // This violates the non-negative-interest invariant.
        return Err(DebtError::IndexInvariantViolated);
    }

    Ok(DebtPosition {
        principal: new_principal,
        borrow_index_snapshot: current_index,
        last_update: now,
    })
}

// ---------------------------------------------------------------------------
// Legacy per-position elapsed-time helpers (kept for backward compatibility
// with existing tests and the rounding_strategy module)
// ---------------------------------------------------------------------------

/// Compute elapsed seconds between two timestamps (saturating).
pub fn elapsed_seconds(now: u64, last_update: u64) -> u64 {
    now.saturating_sub(last_update)
}

/// Compute interest on `principal` over `elapsed` seconds at `rate_bps`.
///
/// Retained for backward compatibility with existing tests; new code should
/// use `compute_debt` + `touch_borrow_index` instead.
pub fn accrue_interest(principal: i128, elapsed: u64, rate_bps: i128) -> Result<i128, DebtError> {
    if principal == 0 || elapsed == 0 {
        return Ok(0);
    }

    let result =
        calculate_interest_with_rounding(principal, elapsed, rate_bps, RoundingMode::Bankers)?;

    if result.interest < 0 {
        return Err(DebtError::Overflow);
    }

    Ok(result.interest)
}

/// Settle interest into `principal` using elapsed-time arithmetic.
///
/// Retained for backward compatibility.
pub fn settle_accrual(
    position: &DebtPosition,
    now: u64,
    rate_bps: i128,
) -> Result<DebtPosition, DebtError> {
    let elapsed = elapsed_seconds(now, position.last_update);
    let interest = accrue_interest(position.principal, elapsed, rate_bps)?;
    let principal = position
        .principal
        .checked_add(interest)
        .ok_or(DebtError::Overflow)?;

    Ok(DebtPosition {
        principal,
        borrow_index_snapshot: position.borrow_index_snapshot,
        last_update: now,
    })
}

/// Compute effective debt using elapsed-time arithmetic (read-only).
///
/// Retained for backward compatibility with view functions.
pub fn effective_debt(
    position: &DebtPosition,
    now: u64,
    rate_bps: i128,
) -> Result<i128, DebtError> {
    let elapsed = elapsed_seconds(now, position.last_update);
    let interest = accrue_interest(position.principal, elapsed, rate_bps)?;
    position
        .principal
        .checked_add(interest)
        .ok_or(DebtError::Overflow)
}

// ---------------------------------------------------------------------------
// Mutating debt operations (index-aware)
// ---------------------------------------------------------------------------

/// Record a new borrow against `position`, settling accrued interest first.
///
/// The position's snapshot is refreshed to `current_index` after settlement.
pub fn borrow_amount(
    position: DebtPosition,
    now: u64,
    amount: i128,
    rate_bps: i128,
) -> Result<DebtPosition, DebtError> {
    if amount <= 0 {
        return Err(DebtError::InvalidAmount);
    }
    // Fall back to elapsed-time accrual for legacy positions with snapshot == 0.
    let mut settled = settle_accrual(&position, now, rate_bps)?;
    settled.principal = settled
        .principal
        .checked_add(amount)
        .ok_or(DebtError::Overflow)?;
    settled.last_update = now;
    Ok(settled)
}

/// Record a repayment against `position`, settling accrued interest first.
///
/// The position's snapshot is refreshed to `current_index` after settlement.
pub fn repay_amount(
    position: DebtPosition,
    now: u64,
    amount: i128,
    rate_bps: i128,
) -> Result<DebtPosition, DebtError> {
    if amount <= 0 {
        return Err(DebtError::InvalidAmount);
    }
    let mut settled = settle_accrual(&position, now, rate_bps)?;
    settled.principal = if amount >= settled.principal {
        0
    } else {
        settled.principal - amount
    };
    settled.last_update = now;
    Ok(settled)
}

/// Index-aware borrow: settle via index ratio, then add `amount`.
///
/// Preferred over `borrow_amount` once the global index is active.
pub fn borrow_amount_indexed(
    position: &DebtPosition,
    current_index: i128,
    now: u64,
    amount: i128,
) -> Result<DebtPosition, DebtError> {
    if amount <= 0 {
        return Err(DebtError::InvalidAmount);
    }
    let mut settled = settle_position(position, current_index, now)?;
    settled.principal = settled
        .principal
        .checked_add(amount)
        .ok_or(DebtError::Overflow)?;
    Ok(settled)
}

/// Index-aware repay: settle via index ratio, then subtract `amount`.
///
/// Preferred over `repay_amount` once the global index is active.
pub fn repay_amount_indexed(
    position: &DebtPosition,
    current_index: i128,
    now: u64,
    amount: i128,
) -> Result<DebtPosition, DebtError> {
    if amount <= 0 {
        return Err(DebtError::InvalidAmount);
    }
    let mut settled = settle_position(position, current_index, now)?;
    settled.principal = if amount >= settled.principal {
        0
    } else {
        settled.principal - amount
    };
    Ok(settled)
}
