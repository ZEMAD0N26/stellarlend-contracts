use soroban_sdk::{contracttype, Address, Env};

use crate::rounding_strategy::{calculate_interest_with_rounding, RoundingError, RoundingMode};
use crate::{rate_model, DataKey};
use stellar_lend_common::BPS_DENOM;

pub const DEFAULT_APR_BPS: i128 = 500;

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DebtPosition {
    pub principal: i128,
    pub last_update: u64,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RateSnapshot {
    pub total_debt: i128,
    pub total_supply: i128,
    pub params: Option<rate_model::RateParams>,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BorrowRateCache {
    pub ledger_sequence: u32,
    pub rate_bps: i128,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DebtError {
    Overflow,
    InvalidAmount,
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

pub fn load_debt(env: &Env, user: &Address) -> DebtPosition {
    let key = DataKey::Debt(user.clone());
    env.storage()
        .persistent()
        .get(&key)
        .unwrap_or(DebtPosition {
            principal: 0,
            last_update: env.ledger().timestamp(),
        })
}

pub fn save_debt(env: &Env, user: &Address, position: &DebtPosition) {
    let key = DataKey::Debt(user.clone());
    env.storage().persistent().set(&key, position);
}

/// Loads the aggregate values needed to compute the global borrow rate once.
pub fn load_rate_snapshot(env: &Env) -> RateSnapshot {
    let storage = env.storage();
    let persistent = storage.persistent();
    let instance = storage.instance();

    RateSnapshot {
        total_debt: persistent.get(&DataKey::TotalDebt).unwrap_or(0),
        total_supply: persistent.get(&DataKey::TotalDeposits).unwrap_or(0),
        params: instance.get(&DataKey::RateParams),
    }
}

/// Computes the global borrow rate directly from current aggregate storage.
pub fn uncached_borrow_rate(env: &Env) -> i128 {
    let snapshot = load_rate_snapshot(env);

    match snapshot.params {
        Some(p) => {
            let utilization_bps = if snapshot.total_supply > 0 {
                snapshot.total_debt.saturating_mul(BPS_DENOM) / snapshot.total_supply
            } else {
                0
            };
            rate_model::compute_borrow_rate(utilization_bps, &p)
        }
        None => DEFAULT_APR_BPS,
    }
}

/// Returns the global borrow rate, computing it at most once per ledger.
///
/// The temporary-storage key includes `env.ledger().sequence()`, so advancing
/// the ledger naturally misses the previous cache entry and recomputes from a
/// fresh `RateSnapshot`.
pub fn cached_borrow_rate(env: &Env) -> i128 {
    let ledger_sequence = env.ledger().sequence();
    let key = DataKey::BorrowRateCache(ledger_sequence);

    if let Some(cache) = env
        .storage()
        .temporary()
        .get::<DataKey, BorrowRateCache>(&key)
    {
        if cache.ledger_sequence == ledger_sequence {
            return cache.rate_bps;
        }
    }

    let rate_bps = uncached_borrow_rate(env);
    let cache = BorrowRateCache {
        ledger_sequence,
        rate_bps,
    };
    env.storage().temporary().set(&key, &cache);
    rate_bps
}

pub fn elapsed_seconds(now: u64, last_update: u64) -> u64 {
    now.saturating_sub(last_update)
}

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
        last_update: now,
    })
}

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

pub fn borrow_amount(
    position: DebtPosition,
    now: u64,
    amount: i128,
    rate_bps: i128,
) -> Result<DebtPosition, DebtError> {
    if amount <= 0 {
        return Err(DebtError::InvalidAmount);
    }

    let mut settled = settle_accrual(&position, now, rate_bps)?;
    settled.principal = settled
        .principal
        .checked_add(amount)
        .ok_or(DebtError::Overflow)?;
    settled.last_update = now;
    Ok(settled)
}

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
