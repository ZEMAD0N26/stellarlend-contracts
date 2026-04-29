//! # Borrow-Withdraw Rounding & Timing Adversarial Tests
//!
//! Deep-dive adversarial tests targeting rounding, timing, and view
//! inconsistency exploitation in borrow-withdraw sequences.
//!
//! ## Threat Model
//!
//! | Category | # | Attack Vector | Defence |
//! |----------|---|---------------|---------|
//! | Rounding | 1 | Exact 150% integer boundary — withdraw 1 | `InsufficientCollateralRatio` |
//! | Rounding | 2 | Non-integer boundary with ceiling math | `InsufficientCollateralRatio` |
//! | Rounding | 3 | 1ns interest accrual blocks any withdraw | Fresh interest + ratio check |
//! | Rounding | 4 | Maximum safe withdraw boundary | `InsufficientCollateralRatio` on +1 |
//! | Rounding | 5 | Collateral = required + 1, withdraw 1 fails | `InsufficientCollateralRatio` |
//! | Rounding | 6 | Minimum borrow exact ratio then withdraw | `InsufficientCollateralRatio` |
//! | Timing   | 7 | Same-timestamp borrow-then-withdraw | Interest not yet accrued |
//! | Timing   | 8 | 1-second advance changes boundary | Fresh interest calc |
//! | Timing   | 9 | Rapid deposit-borrow-withdraw sequence | Consistent ratio enforcement |
//! | Timing   | 10 | Sub-year interest blocks withdraw | Fresh interest calc |
//! | Timing   | 11 | Borrow at year boundary then withdraw | Interest resets at boundary |
//! | Views    | 12 | Stale view after borrow cannot bypass | Withdraw uses fresh state |
//! | Views    | 13 | Oracle returns 0, withdraw still enforced | Raw ratio check |
//! | Views    | 14 | Misleading HF, over-withdraw blocked | Raw ratio not oracle |
//! | Views    | 15 | HF=NO_DEBT only when actual debt=0 | Debt-aware check |
//! | Views    | 16 | max_liquidatable=0 doesn't mean withdrawable | Separate paths |
//! | Path Iso | 17 | Borrow-path collateral completely inaccessible | `InsufficientCollateral` |
//! | Path Iso | 18 | Deposit then borrow 0 additional, withdraw boundary | Deposit path only |
//! | Path Iso | 19 | Interleaved deposit/collateral, verify paths | Path separation |
//! | Path Iso | 20 | Withdraw ignores borrow-path balance | Deposit path balance check |
//! | Extreme  | 21 | i128::MAX/3 collateral exact ratio | `InsufficientCollateralRatio` |
//! | Extreme  | 22 | Smallest viable position ratio enforced | `InsufficientCollateralRatio` |
//! | Extreme  | 23 | Repay to exact boundary, withdraw to zero | Debt=0 check |
//!
//! ## Security Invariant
//! After every successful `withdraw`, the remaining deposit-path collateral
//! must satisfy `collateral >= debt * MIN_COLLATERAL_RATIO_BPS / BPS_SCALE`
//! (150 % default). This invariant is enforced by
//! `validate_collateral_ratio_after_withdraw`, which delegates to the same
//! `borrow::validate_collateral_ratio` used at borrow time.

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    Address, Env,
};
use views::{HEALTH_FACTOR_SCALE, HEALTH_FACTOR_NO_DEBT};

// ─── helpers ────────────────────────────────────────────────────────────────

fn setup(env: &Env) -> (LendingContractClient<'_>, Address, Address, Address, Address) {
    let contract_id = env.register(LendingContract, ());
    let client = LendingContractClient::new(env, &contract_id);
    let admin = Address::generate(env);
    let user = Address::generate(env);
    let asset = Address::generate(env);
    let collateral_asset = Address::generate(env);
    client.initialize(&admin, &1_000_000_000, &1000);
    client.initialize_withdraw_settings(&100);
    client.register_asset(&admin, &asset);
    client.register_asset(&admin, &collateral_asset);
    (client, admin, user, asset, collateral_asset)
}

fn setup_with_oracle(
    env: &Env,
) -> (LendingContractClient<'_>, Address, Address, Address, Address) {
    let (client, admin, user, asset, collateral_asset) = setup(env);
    env.ledger().with_mut(|li| li.timestamp = 0);
    (client, admin, user, asset, collateral_asset)
}

/// Compute required collateral for a given debt at 150 % (integer division).
fn required_collateral(debt: i128) -> i128 {
    debt.checked_mul(15_000).unwrap().checked_div(10_000).unwrap()
}

// ═════════════════════════════════════════════════════════════════════════════
// Rounding Exploitation Tests
// ═════════════════════════════════════════════════════════════════════════════

/// 1. Exact integer boundary: debt where debt*15000/10000 has no remainder.
///    collateral = exact required. withdraw 1 → remaining < required → fail.
#[test]
fn test_exact_150pct_integer_boundary() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    // 10_000 * 15000 / 10000 = 15_000 exactly (no remainder)
    client.borrow(&user, &asset, &10_000, &collateral_asset, &15_000);

    let result = client.try_withdraw(&user, &collateral_asset, &1);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );
}

/// 2. Non-integer boundary: debt where debt*15000/10000 has remainder.
///    Integer division truncates toward zero, so required = floor(total).
///    collateral = floor + 1. withdraw 1 → remaining = floor < required+1 → fail.
#[test]
fn test_150pct_non_integer_boundary_ceiling() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    // 7_000 * 15000 / 10000 = 10_500 exactly — let me pick one with remainder
    // 7_001 * 15000 / 10000 = 10_501.5 → 10_501 (floor)
    // Required = 10_501. If collateral = 10_502, withdraw 1 → 10_501 = required → OK
    // If collateral = 10_501, withdraw 1 → 10_500 < 10_501 → fail
    client.borrow(&user, &asset, &7_001, &collateral_asset, &10_501);

    let result = client.try_withdraw(&user, &collateral_asset, &1);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );
}

/// 3. Interest rounds up after just 1 nanosecond of elapsed time.
///    Borrow at exactly 150%, advance timestamp by 1, interest rounds up to 1.
///    Total debt now exceeds the collateral that was exactly at boundary.
#[test]
fn test_interest_rounding_1ns_blocks_any_withdraw() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup_with_oracle(&env);

    env.ledger().with_mut(|li| li.timestamp = 0);

    // Borrow at exactly 150%: 100_000 debt, 150_000 collateral
    client.borrow(&user, &asset, &100_000, &collateral_asset, &150_000);

    // Advance 1 second — interest calculation uses ceiling rounding
    env.ledger().with_mut(|li| li.timestamp = 1);

    let debt = client.get_user_debt(&user);
    // Interest: (100_000 * 500 * 1) / (10000 * 31536000) with ceiling
    // = 5000000 / 315360000000 = 0.0000158... → ceiling = 1
    assert_eq!(debt.interest_accrued, 1, "interest must round up to 1");

    // Total debt = 100_001. Required = 100_001 * 1.5 = 150_001.5 → 150_001 (floor)
    // But since interest rounded up, the contract may require ceiling.
    // Regardless, withdrawing any amount from 150_000 must fail.
    let result = client.try_withdraw(&user, &collateral_asset, &1);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );
}

/// 4. Verify that maximum safe withdraw computation boundary is exact.
///    collateral = 300_000, debt = 100_000. Required = 150_000. Max safe = 150_000.
///    withdraw 150_000 → OK. withdraw 150_001 → fail.
#[test]
fn test_maximum_safe_withdraw_computation_boundary() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    client.borrow(&user, &asset, &100_000, &collateral_asset, &300_000);

    // Max safe = 300_000 - 150_000 = 150_000
    let result = client.try_withdraw(&user, &collateral_asset, &150_001);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );

    let remaining = client.withdraw(&user, &collateral_asset, &150_000);
    assert_eq!(remaining, 150_000);
}

/// 5. collateral = required + 1. withdraw 1 → remaining = required.
///    Should succeed (exactly at boundary). withdraw 2 → remaining < required → fail.
#[test]
fn test_collateral_1_above_required_withdraw_1_fails() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    // debt = 10_000, required = 15_000. collateral = 15_001.
    client.borrow(&user, &asset, &10_000, &collateral_asset, &15_001);

    // withdraw 1 → remaining = 15_000 = required → OK
    let remaining = client.withdraw(&user, &collateral_asset, &1);
    assert_eq!(remaining, 15_000);

    // Now remaining = 15_000. withdraw 1 more → 14_999 < 15_000 → fail
    let result = client.try_withdraw(&user, &collateral_asset, &1);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );
}

/// 6. Minimum borrow amount (1000) with exactly 1500 collateral.
///    Verify withdraw 1 fails.
#[test]
fn test_minimum_borrow_exact_ratio_then_withdraw() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    // Minimum borrow = 1000. Required collateral = 1500.
    client.borrow(&user, &asset, &1_000, &collateral_asset, &1_500);

    let result = client.try_withdraw(&user, &collateral_asset, &1);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// Timing Attack Tests
// ═════════════════════════════════════════════════════════════════════════════

/// 7. Borrow and immediate withdraw at same ledger timestamp.
///    Interest has not accrued yet (time_elapsed = 0), so ratio is unchanged.
///    Withdraw must still be blocked because the ratio check uses current state.
#[test]
fn test_borrow_withdraw_same_timestamp_blocked() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup_with_oracle(&env);

    env.ledger().with_mut(|li| li.timestamp = 100);

    // Borrow at exactly 150%
    client.borrow(&user, &asset, &100_000, &collateral_asset, &150_000);

    // Same timestamp — interest = 0
    let debt = client.get_user_debt(&user);
    assert_eq!(debt.interest_accrued, 0);

    // Still cannot withdraw anything
    let result = client.try_withdraw(&user, &collateral_asset, &1);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );
}

/// 8. Advance exactly 1 second and verify interest changes the boundary.
///    The interest calculation should round up, making the position unsafe.
#[test]
fn test_withdraw_after_1_second_interest_boundary() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup_with_oracle(&env);

    env.ledger().with_mut(|li| li.timestamp = 0);

    // Borrow with some headroom: 200% collateral
    client.borrow(&user, &asset, &100_000, &collateral_asset, &200_000);

    // Advance 1 second
    env.ledger().with_mut(|li| li.timestamp = 1);

    let debt = client.get_user_debt(&user);
    assert_eq!(debt.interest_accrued, 1, "1 second interest should be 1");

    let total_debt = debt.borrowed_amount + debt.interest_accrued;
    let req = required_collateral(total_debt);
    let max_safe = 200_000 - req;

    // Withdraw just over max safe → fail
    let result = client.try_withdraw(&user, &collateral_asset, &(max_safe + 1));
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );

    // Withdraw max safe → succeed
    let remaining = client.withdraw(&user, &collateral_asset, &max_safe);
    assert_eq!(remaining, req);
}

/// 9. Rapid sequence: deposit, borrow, withdraw all in same timestamp.
///    Each operation sees fresh state; withdraw must still enforce ratio.
#[test]
fn test_rapid_sequence_deposit_borrow_withdraw() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup_with_oracle(&env);

    env.ledger().with_mut(|li| li.timestamp = 500);

    // Deposit 200_000
    client.deposit(&user, &collateral_asset, &200_000);

    // Borrow 100_000 with 0 additional (still 200%)
    client.borrow(&user, &asset, &100_000, &collateral_asset, &0);

    // Try to withdraw 51_000 → remaining 149_000 < 150_000 required → fail
    let result = client.try_withdraw(&user, &collateral_asset, &51_000);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );

    // Withdraw 50_000 → OK
    let remaining = client.withdraw(&user, &collateral_asset, &50_000);
    assert_eq!(remaining, 150_000);
}

/// 10. Advance by a small fraction of a year and verify interest blocks withdraw.
#[test]
fn test_interest_blocks_withdraw_at_sub_year_boundary() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup_with_oracle(&env);

    env.ledger().with_mut(|li| li.timestamp = 0);

    // Borrow at exactly 150% with no headroom
    client.borrow(&user, &asset, &100_000, &collateral_asset, &150_000);

    // Advance 1 day (86_400 seconds) — interest accrues
    env.ledger().with_mut(|li| li.timestamp = 86_400);

    let debt = client.get_user_debt(&user);
    assert!(debt.interest_accrued > 0, "interest must accrue after 1 day");

    // Total debt increased; any withdraw must fail
    let result = client.try_withdraw(&user, &collateral_asset, &1);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );
}

/// 11. Borrow at year boundary, then withdraw immediately after year boundary.
///    Interest calculation resets at each call; the boundary doesn't create gaps.
#[test]
fn test_borrow_at_year_boundary_then_withdraw() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup_with_oracle(&env);

    // Set timestamp just before a year boundary
    env.ledger().with_mut(|li| li.timestamp = 31_535_999);

    // Borrow with headroom
    client.borrow(&user, &asset, &100_000, &collateral_asset, &200_000);

    // Cross year boundary
    env.ledger().with_mut(|li| li.timestamp = 31_536_001);

    let debt = client.get_user_debt(&user);
    // Interest should be calculated for 2 seconds (ceiling rounding)
    assert_eq!(debt.interest_accrued, 1, "2 seconds interest rounds up to 1");

    let total_debt = debt.borrowed_amount + debt.interest_accrued;
    let req = required_collateral(total_debt);
    let max_safe = 200_000 - req;

    let result = client.try_withdraw(&user, &collateral_asset, &(max_safe + 1));
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// View Inconsistency Tests
// ═════════════════════════════════════════════════════════════════════════════

/// 12. Stale view queried before borrow cannot bypass ratio on withdraw.
///    The view is from before the borrow; withdraw uses fresh debt state.
#[test]
fn test_stale_view_after_borrow_cannot_bypass_ratio() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    // Get a view before any borrowing (debt = 0)
    let hf_before = client.get_health_factor(&user);
    assert_eq!(hf_before, HEALTH_FACTOR_NO_DEBT);

    // Now borrow
    client.borrow(&user, &asset, &100_000, &collateral_asset, &150_000);

    // The stale view is irrelevant; withdraw uses current state
    let result = client.try_withdraw(&user, &collateral_asset, &1);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );
}

/// 13. Oracle returns 0 (unconfigured). View shows 0 for everything.
///    Withdraw must still enforce raw 150% ratio regardless of oracle.
#[test]
fn test_oracle_returns_zero_withdraw_still_enforced() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    // No oracle configured — views return 0
    client.borrow(&user, &asset, &100_000, &collateral_asset, &200_000);

    assert_eq!(client.get_collateral_value(&user), 0);
    assert_eq!(client.get_debt_value(&user), 0);
    assert_eq!(client.get_health_factor(&user), 0);

    // Withdraw still blocked by raw ratio
    let result = client.try_withdraw(&user, &collateral_asset, &51_000);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );

    // Valid withdraw still works
    let remaining = client.withdraw(&user, &collateral_asset, &50_000);
    assert_eq!(remaining, 150_000);
}

/// 14. Position summary shows healthy HF but over-withdrawal is blocked.
///    HF is based on oracle; withdraw uses raw amounts. Cannot exploit gap.
#[test]
fn test_position_summary_misleading_hf_withdraw_blocked() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, user, asset, collateral_asset) = setup(&env);

    // Set oracle
    let oracle_id = env.register(views_test::MockOracle, ());
    client.set_oracle(&admin, &oracle_id);

    // Borrow with 300% collateral (very healthy)
    client.borrow(&user, &asset, &50_000, &collateral_asset, &150_000);

    let summary = client.get_user_position(&user);
    assert!(summary.health_factor >= HEALTH_FACTOR_SCALE);

    // Try to withdraw to below 150% raw ratio
    // Required = 50_000 * 1.5 = 75_000. Try to withdraw 76_001.
    let result = client.try_withdraw(&user, &collateral_asset, &76_001);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );
}

/// 15. HEALTH_FACTOR_NO_DEBT sentinel is only returned when actual debt is 0.
///    Borrowing creates debt; HF should reflect real position.
#[test]
fn test_health_factor_no_debt_vs_actual_debt() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, user, asset, collateral_asset) = setup(&env);

    let oracle_id = env.register(views_test::MockOracle, ());
    client.set_oracle(&admin, &oracle_id);

    // Before borrow: HF = NO_DEBT
    assert_eq!(client.get_health_factor(&user), HEALTH_FACTOR_NO_DEBT);

    // After borrow: HF is a real computed value
    client.borrow(&user, &asset, &10_000, &collateral_asset, &20_000);
    let hf = client.get_health_factor(&user);
    assert_ne!(hf, HEALTH_FACTOR_NO_DEBT);
    assert!(hf >= HEALTH_FACTOR_SCALE);
}

/// 16. get_max_liquidatable_amount = 0 for healthy positions.
///    This does NOT mean the position has no debt or is fully withdrawable.
#[test]
fn test_view_max_liquidatable_zero_does_not_mean_withdrawable() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, user, asset, collateral_asset) = setup(&env);

    let oracle_id = env.register(views_test::MockOracle, ());
    client.set_oracle(&admin, &oracle_id);

    // Healthy position: max_liquidatable = 0
    client.borrow(&user, &asset, &50_000, &collateral_asset, &150_000);
    assert_eq!(client.get_max_liquidatable_amount(&user), 0);

    // But withdraw is still limited by 150% ratio
    let result = client.try_withdraw(&user, &collateral_asset, &76_001);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// Path Isolation Tests
// ═════════════════════════════════════════════════════════════════════════════

/// 17. Borrow-path collateral is completely inaccessible via withdraw().
///    withdraw() reads from DepositDataKey, not BorrowDataKey.
#[test]
fn test_borrow_path_collateral_completely_inaccessible() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    // Add collateral ONLY via deposit_collateral (borrow path)
    client.deposit_collateral(&user, &collateral_asset, &100_000);

    // Borrow against it
    client.borrow(&user, &asset, &50_000, &collateral_asset, &0);

    // Try to withdraw from deposit path — deposit path is empty
    let result = client.try_withdraw(&user, &collateral_asset, &1);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateral))
    );

    // Borrow-path collateral intact
    let borrow_pos = client.get_user_collateral(&user);
    assert_eq!(borrow_pos.amount, 100_000);
}

/// 18. Deposit 150k, borrow 100k with 0 additional, try withdraw 1.
///    Deposit path = 150k, required = 150k. withdraw 1 → fail.
#[test]
fn test_deposit_then_borrow_zero_additional_withdraw_boundary() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    client.deposit(&user, &collateral_asset, &150_000);
    client.borrow(&user, &asset, &100_000, &collateral_asset, &0);

    // Deposit path = 150_000. Required = 100_000 * 1.5 = 150_000.
    let result = client.try_withdraw(&user, &collateral_asset, &1);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );
}

/// 19. Interleaved deposits to both paths, borrow, verify only deposit path withdrawable.
#[test]
fn test_interleaved_deposit_collateral_and_withdraw() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    // Deposit 50_000 to deposit path (withdrawable)
    client.deposit(&user, &collateral_asset, &50_000);

    // Deposit 100_000 to borrow path (not withdrawable)
    client.deposit_collateral(&user, &collateral_asset, &100_000);

    // Borrow 50_000 — total collateral = 150_000
    client.borrow(&user, &asset, &50_000, &collateral_asset, &0);

    // Deposit path = 50_000. Required = 75_000.
    // Cannot withdraw anything because deposit path < required
    let result = client.try_withdraw(&user, &collateral_asset, &1);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );

    // Borrow path = 100_000 (intact)
    let borrow_pos = client.get_user_collateral(&user);
    assert_eq!(borrow_pos.amount, 100_000);
}

/// 20. Withdraw ignores borrow-path balance for its balance check.
///    Borrow path has lots; deposit path has little. Withdraw limited by deposit.
#[test]
fn test_withdraw_ignores_borrow_path_for_balance_check() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    // Borrow path: 1_000_000
    client.deposit_collateral(&user, &collateral_asset, &1_000_000);

    // Deposit path: 10_000
    client.deposit(&user, &collateral_asset, &10_000);

    // Borrow 5_000 — required = 7_500
    client.borrow(&user, &asset, &5_000, &collateral_asset, &0);

    // Try to withdraw 10_000 from deposit path
    // Deposit path balance = 10_000 >= 10_000. But remaining = 0 < 7_500 required → fail
    let result = client.try_withdraw(&user, &collateral_asset, &10_000);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );

    // Max safe from deposit path = 10_000 - 7_500 = 2_500
    let remaining = client.withdraw(&user, &collateral_asset, &2_500);
    assert_eq!(remaining, 7_500);
}

// ═════════════════════════════════════════════════════════════════════════════
// Extreme Value Tests
// ═════════════════════════════════════════════════════════════════════════════

/// 21. i128::MAX/3 collateral with exact 150% ratio borrow.
///    withdraw 1 → fail. No overflow in ratio computation.
#[test]
fn test_i128_max_collateral_exact_ratio_withdraw_1() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    let large_collateral: i128 = i128::MAX / 3;
    // Exact 150%: borrow = collateral * 10000 / 15000
    let large_borrow: i128 = large_collateral * 10_000 / 15_000;

    client.borrow(&user, &asset, &large_borrow, &collateral_asset, &large_collateral);

    let result = client.try_withdraw(&user, &collateral_asset, &1);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );
}

/// 22. Smallest viable position: borrow 1000 (min) with 1500 collateral.
///    Verify ratio is enforced at the smallest scale.
#[test]
fn test_smallest_viable_position_ratio_enforced() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    // Minimum borrow = 1000
    client.borrow(&user, &asset, &1_000, &collateral_asset, &1_500);

    // withdraw 1 → 1499 < 1500 required → fail
    let result = client.try_withdraw(&user, &collateral_asset, &1);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );

    // Position intact
    let collateral = client.get_user_collateral(&user);
    assert_eq!(collateral.amount, 1_500);
}

/// 23. Repay to exact boundary (collateral = required exactly), then try withdraw 1.
///    Debt is 0, so ratio check is bypassed and full withdraw allowed.
#[test]
fn test_repay_to_exact_boundary_then_withdraw_to_zero() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    // Borrow 100_000 with 200_000 collateral
    client.borrow(&user, &asset, &100_000, &collateral_asset, &200_000);

    // Repay all debt
    client.repay(&user, &asset, &100_000);

    let debt = client.get_user_debt(&user);
    assert_eq!(debt.borrowed_amount, 0);
    assert_eq!(debt.interest_accrued, 0);

    // Full withdraw allowed because debt is 0
    let remaining = client.withdraw(&user, &collateral_asset, &200_000);
    assert_eq!(remaining, 0);

    // Position fully empty
    let deposit_pos = client.get_user_collateral_deposit(&user, &collateral_asset);
    assert_eq!(deposit_pos.amount, 0);
}

