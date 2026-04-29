//! # Borrow-Withdraw Adversarial Tests — Issue #472
//!
//! Threat scenarios covered:
//!
//! | # | Threat | Defence |
//! |---|--------|---------|
//! | 1 | Borrow then immediately withdraw all collateral | `InsufficientCollateralRatio` |
//! | 2 | Borrow at exact 150 % boundary then withdraw 1 unit | `InsufficientCollateralRatio` |
//! | 3 | Rounding manipulation: borrow small, withdraw to sub-150 % | `InsufficientCollateralRatio` |
//! | 4 | Interest accrual timing attack: borrow, wait, withdraw before interest updates | `validate_collateral_ratio_after_withdraw` |
//! | 5 | Partial repay then attempt over-withdraw | `InsufficientCollateralRatio` |
//! | 6 | View inconsistency: health factor says liquidatable but withdraw still blocked | ratio check |
//! | 7 | Rapid borrow-withdraw cycles to drain collateral | `InsufficientCollateralRatio` on each step |
//! | 8 | Deposit, borrow, withdraw original deposit leaving only borrowed collateral | `InsufficientCollateral` |
//! | 9 | Withdraw exactly to 150 % line after interest rounds up | `InsufficientCollateralRatio` |
//! | 10 | Cross-module consistency: deposit collateral then borrow against it, withdraw via deposit path | `InsufficientCollateralRatio` |
//! | 11 | Oracle price drop makes position undercollateralised; withdraw must still be blocked | ratio check |
//! | 12 | Zero-amount withdraw after borrow (bypass auth / minimum checks) | `InvalidAmount` |
//! | 13 | Negative withdraw after borrow | `InvalidAmount` |
//! | 14 | Max i128 borrow, max i128 collateral, then withdraw anything | `InsufficientCollateralRatio` |
//! | 15 | Borrow with existing deposit, withdraw deposit portion leaving borrowed collateral | ratio check |
//!
//! ## Security Invariant
//! After every successful `withdraw`, the remaining collateral must satisfy
//! `collateral >= debt * MIN_COLLATERAL_RATIO_BPS / BPS_SCALE` (150 % default).
//! This invariant is enforced by `validate_collateral_ratio_after_withdraw`,
//! which delegates to the same `borrow::validate_collateral_ratio` used at
//! borrow time, ensuring the two paths cannot diverge.

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    Address, Env,
};
use views::HEALTH_FACTOR_SCALE;

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

/// Assert health factor is ≥ 1.0 (healthy) or exactly `HEALTH_FACTOR_NO_DEBT`.
fn assert_healthy(env: &Env, client: &LendingContractClient<'_>, user: &Address) {
    let hf = client.get_health_factor(user);
    // When oracle is absent, HF = 0 (unknown); we skip the assertion in that case.
    // When oracle is present, HF must be >= HEALTH_FACTOR_SCALE or HEALTH_FACTOR_NO_DEBT.
    if hf != 0 && hf != views::HEALTH_FACTOR_NO_DEBT {
        assert!(
            hf >= HEALTH_FACTOR_SCALE,
            "health factor must be >= 1.0; got {}",
            hf
        );
    }
}

// ─── 1. Borrow then immediately withdraw all collateral ─────────────────────

#[test]
fn test_borrow_then_withdraw_all_collateral_rejected() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    // Borrow 10_000 with 20_000 collateral (200 %)
    client.borrow(&user, &asset, &10_000, &collateral_asset, &20_000);

    // Attempt to withdraw all 20_000 — would leave 0 collateral vs 10_000 debt
    let result = client.try_withdraw(&user, &collateral_asset, &20_000);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );
}

// ─── 2. Borrow at exact 150 % boundary then withdraw 1 unit ─────────────────

#[test]
fn test_borrow_exact_150pct_then_withdraw_one_unit_rejected() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    // Borrow 10_000 with exactly 15_000 collateral (150 %)
    client.borrow(&user, &asset, &10_000, &collateral_asset, &15_000);

    // Withdrawing even 1 unit would leave 14_999 collateral, which is < 15_000 required
    let result = client.try_withdraw(&user, &collateral_asset, &1);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );
}

// ─── 3. Rounding manipulation: small borrow, attempt sub-150 % withdraw ─────

#[test]
fn test_small_borrow_rounding_cannot_bypass_ratio() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    // Minimum borrow is 1_000. Use smallest collateral that passes: 1_500
    client.borrow(&user, &asset, &1_000, &collateral_asset, &1_500);

    // Attempt to withdraw 1 unit — 1_499 < 1_500 required
    let result = client.try_withdraw(&user, &collateral_asset, &1);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );

    // Verify position is still valid
    let collateral = client.get_user_collateral(&user);
    assert_eq!(collateral.amount, 1_500);
}

// ─── 4. Interest accrual timing attack ──────────────────────────────────────

#[test]
fn test_interest_accrual_blocks_withdraw_timing_attack() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    env.ledger().with_mut(|li| li.timestamp = 0);

    // Borrow with exactly 150 % collateral
    client.borrow(&user, &asset, &100_000, &collateral_asset, &150_000);

    // Advance time so interest accrues; now debt > 100_000
    env.ledger().with_mut(|li| li.timestamp = 31_536_000); // 1 year

    let debt = client.get_user_debt(&user);
    assert!(debt.interest_accrued > 0, "interest should have accrued");

    // Withdrawing any amount should fail because the ratio already uses
    // the updated debt (interest is calculated fresh on every call).
    // Max safe withdrawal = collateral - required_for_debt
    let required_collateral = (debt.borrowed_amount + debt.interest_accrued) * 15_000 / 10_000;
    let max_safe_withdrawal = 150_000 - required_collateral;

    // Attempt to withdraw just over the safe amount
    let result = client.try_withdraw(&user, &collateral_asset, &(max_safe_withdrawal + 1));
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );
}

// ─── 5. Partial repay then attempt over-withdraw ────────────────────────────

#[test]
fn test_partial_repay_then_over_withdraw_rejected() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    // Initial borrow: 200 % collateral
    client.borrow(&user, &asset, &100_000, &collateral_asset, &200_000);

    // Repay 50_000 — debt now 50_000
    client.repay(&user, &asset, &50_000);
    let debt_after = client.get_user_debt(&user);
    assert_eq!(debt_after.borrowed_amount, 50_000);

    // Required collateral = 50_000 * 1.5 = 75_000
    // Attempt to withdraw 126_000 → remaining 74_000 < 75_000
    let result = client.try_withdraw(&user, &collateral_asset, &126_000);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );

    // Withdraw exactly 125_000 → remaining 75_000 = required → should succeed
    let remaining = client.withdraw(&user, &collateral_asset, &125_000);
    assert_eq!(remaining, 75_000);
}

// ─── 6. View inconsistency: health factor liquidatable, withdraw blocked ────

#[test]
fn test_view_shows_liquidatable_withdraw_still_blocked() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, user, asset, collateral_asset) = setup(&env);

    // Set up oracle so health factor is meaningful
    let oracle_id = env.register(views_test::MockOracle, ());
    client.set_oracle(&admin, &oracle_id);

    // Borrow with tight collateral: 150 % ratio (passes borrow check)
    client.borrow(&user, &asset, &100_000, &collateral_asset, &150_000);

    // Lower liquidation threshold so HF < 1.0 (would be liquidatable)
    // With default threshold 80%: weighted = 150_000 * 0.8 = 120_000
    // HF = 120_000 * 10000 / 100_000 = 12_000 > 10_000 (healthy)
    // We need HF < 10_000. Use threshold 60%: weighted = 150_000 * 0.6 = 90_000
    // HF = 90_000 * 10000 / 100_000 = 9_000 < 10_000
    client.set_liquidation_threshold_bps(&admin, &6000);

    let hf = client.get_health_factor(&user);
    assert!(
        hf < HEALTH_FACTOR_SCALE,
        "position should be liquidatable; hf={}",
        hf
    );

    // Even though view says liquidatable, withdraw must still enforce 150 % ratio
    // Required collateral = 100_000 * 1.5 = 150_000
    // Withdrawing 1 unit → 149_999 < 150_000 → must fail
    let result = client.try_withdraw(&user, &collateral_asset, &1);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );
}

// ─── 7. Rapid borrow-withdraw cycles cannot drain collateral ────────────────

#[test]
fn test_rapid_borrow_withdraw_cycles_blocked() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    // Start with a large deposit
    client.deposit_collateral(&user, &collateral_asset, &1_000_000);

    // Attempt multiple borrow-withdraw cycles
    for _ in 0..5 {
        // Borrow small amount with minimal additional collateral
        let result = client.try_borrow(&user, &asset, &10_000, &collateral_asset, &15_000);
        if result.is_ok() {
            // Try to withdraw the newly added collateral
            let withdraw_result =
                client.try_withdraw(&user, &collateral_asset, &15_000);
            assert!(
                withdraw_result.is_err(),
                "withdraw in rapid cycle should fail"
            );
        }
    }

    // Final collateral must still cover debt at 150 %
    let debt = client.get_user_debt(&user);
    let collateral = client.get_user_collateral(&user);
    if debt.borrowed_amount > 0 {
        let required = debt.borrowed_amount * 15_000 / 10_000;
        assert!(
            collateral.amount >= required,
            "final collateral {} < required {} for debt {}",
            collateral.amount,
            required,
            debt.borrowed_amount
        );
    }
}

// ─── 8. Deposit, borrow, withdraw original deposit ──────────────────────────

#[test]
fn test_withdraw_original_deposit_after_borrow_rejected() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    // Deposit 100_000 collateral first
    client.deposit_collateral(&user, &collateral_asset, &100_000);

    // Borrow 50_000 — required collateral = 75_000
    client.borrow(&user, &asset, &50_000, &collateral_asset, &0);

    // Attempt to withdraw the original 100_000 deposit
    // Remaining would be 0 < 75_000 required → must fail
    let result = client.try_withdraw(&user, &collateral_asset, &100_000);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );
}

// ─── 9. Withdraw exactly to 150 % line after interest rounds up ─────────────

#[test]
fn test_withdraw_to_exact_150pct_after_interest_rounds_up() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    env.ledger().with_mut(|li| li.timestamp = 0);

    // Borrow 100_000 with 150_000 collateral (exactly 150 %)
    client.borrow(&user, &asset, &100_000, &collateral_asset, &150_000);

    // Advance 1 second — interest rounds up to 1 unit
    env.ledger().with_mut(|li| li.timestamp = 1);

    let debt = client.get_user_debt(&user);
    assert_eq!(
        debt.interest_accrued, 1,
        "interest should round up to 1"
    );

    // Total debt = 100_001. Required collateral = 100_001 * 1.5 = 150_001.5 → 150_002 (ceil)
    // Current collateral = 150_000. Cannot withdraw anything safely.
    let result = client.try_withdraw(&user, &collateral_asset, &1);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );
}

// ─── 10. Cross-module consistency: deposit path vs borrow path collateral ───

#[test]
fn test_cross_module_collateral_consistency() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    // Deposit via deposit path
    client.deposit(&user, &collateral_asset, &100_000);

    // Borrow using the same asset as collateral (0 additional)
    client.borrow(&user, &asset, &50_000, &collateral_asset, &0);

    // get_user_collateral (borrow module) should reflect deposit
    let borrow_collateral = client.get_user_collateral(&user);
    assert_eq!(borrow_collateral.amount, 100_000);

    // get_user_collateral_deposit (deposit module) should match
    let deposit_collateral = client.get_user_collateral_deposit(&user, &collateral_asset);
    assert_eq!(deposit_collateral.amount, 100_000);

    // Withdraw via withdraw path should see the debt and enforce ratio
    // Required = 50_000 * 1.5 = 75_000. Try to withdraw 26_000 → remaining 74_000
    let result = client.try_withdraw(&user, &collateral_asset, &26_000);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );

    // Withdraw 25_000 → remaining 75_000 = required → should succeed
    let remaining = client.withdraw(&user, &collateral_asset, &25_000);
    assert_eq!(remaining, 75_000);
}

// ─── 11. Oracle price drop: withdraw still blocked by ratio ─────────────────

#[test]
fn test_oracle_price_drop_withdraw_still_enforced() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, user, asset, collateral_asset) = setup(&env);

    // Register a mock oracle that returns price = 1
    let oracle_id = env.register(MockOraclePriceOne, ());
    client.set_oracle(&admin, &oracle_id);

    // Borrow 100_000 with 150_000 collateral (150 %)
    client.borrow(&user, &asset, &100_000, &collateral_asset, &150_000);

    // Health factor should be healthy with default threshold
    let hf_before = client.get_health_factor(&user);
    assert!(hf_before >= HEALTH_FACTOR_SCALE || hf_before == views::HEALTH_FACTOR_NO_DEBT);

    // Even if oracle price drops (simulated by lowering liquidation threshold),
    // the borrow-side 150 % ratio check on withdraw must still block over-withdrawal
    client.set_liquidation_threshold_bps(&admin, &5000); // 50 %

    let hf_after = client.get_health_factor(&user);
    assert!(
        hf_after < HEALTH_FACTOR_SCALE,
        "HF should now be < 1.0; got {}",
        hf_after
    );

    // Withdraw attempt must still fail on the 150 % ratio, not on health factor
    let result = client.try_withdraw(&user, &collateral_asset, &1);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );
}

/// Mock oracle returning price = 1 (100_000_000 with 8 decimals)
#[contract]
pub struct MockOraclePriceOne;

#[contractimpl]
impl MockOraclePriceOne {
    pub fn price(_env: Env, _asset: Address) -> i128 {
        100_000_000
    }
}

// ─── 12. Zero-amount withdraw after borrow ──────────────────────────────────

#[test]
fn test_zero_withdraw_after_borrow_rejected() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    client.borrow(&user, &asset, &10_000, &collateral_asset, &20_000);

    let result = client.try_withdraw(&user, &collateral_asset, &0);
    assert_eq!(result, Err(Ok(WithdrawError::InvalidAmount)));
}

// ─── 13. Negative withdraw after borrow ─────────────────────────────────────

#[test]
fn test_negative_withdraw_after_borrow_rejected() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    client.borrow(&user, &asset, &10_000, &collateral_asset, &20_000);

    let result = client.try_withdraw(&user, &collateral_asset, &-1);
    assert_eq!(result, Err(Ok(WithdrawError::InvalidAmount)));
}

// ─── 14. Extreme i128 values: cannot bypass ratio ───────────────────────────

#[test]
fn test_extreme_i128_borrow_withdraw_ratio_enforced() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    // Use very large but safe values
    let large_collateral: i128 = i128::MAX / 3;
    let large_borrow: i128 = large_collateral * 10_000 / 15_000; // exactly 150 %

    client.borrow(&user, &asset, &large_borrow, &collateral_asset, &large_collateral);

    // Withdrawing any positive amount should fail
    let result = client.try_withdraw(&user, &collateral_asset, &1);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );
}

// ─── 15. Borrow with existing deposit, withdraw deposit portion ─────────────

#[test]
fn test_borrow_with_existing_deposit_withdraw_portion_rejected() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    // Deposit 200_000 first
    client.deposit_collateral(&user, &collateral_asset, &200_000);

    // Borrow 100_000 — required collateral = 150_000
    client.borrow(&user, &asset, &100_000, &collateral_asset, &0);

    // Total collateral = 200_000. Max safe withdraw = 200_000 - 150_000 = 50_000
    // Attempt 50_001
    let result = client.try_withdraw(&user, &collateral_asset, &50_001);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );

    // Exactly 50_000 should succeed
    let remaining = client.withdraw(&user, &collateral_asset, &50_000);
    assert_eq!(remaining, 150_000);
}

// ─── 16. Interest-bearing debt: partial withdraw after long time ────────────

#[test]
fn test_long_duration_interest_blocks_partial_withdraw() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    env.ledger().with_mut(|li| li.timestamp = 0);

    // Borrow with generous collateral: 300 %
    client.borrow(&user, &asset, &100_000, &collateral_asset, &300_000);

    // Wait 10 years
    env.ledger().with_mut(|li| li.timestamp = 10 * 31_536_000);

    let debt = client.get_user_debt(&user);
    let total_debt = debt.borrowed_amount + debt.interest_accrued;
    assert!(total_debt > 100_000, "interest should have accrued");

    // Required collateral after 10 years
    let required = total_debt * 15_000 / 10_000;
    let max_safe = 300_000 - required;

    // Attempt to withdraw just over max safe
    let result = client.try_withdraw(&user, &collateral_asset, &(max_safe + 1));
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );

    // Withdraw exactly max safe should succeed
    if max_safe >= 100 {
        let remaining = client.withdraw(&user, &collateral_asset, &max_safe);
        assert_eq!(remaining, required);
    }
}

// ─── 17. Multiple borrows, then attempt aggregate over-withdraw ─────────────

#[test]
fn test_multiple_borrows_then_over_withdraw_rejected() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    // First borrow: 50_000 with 100_000
    client.borrow(&user, &asset, &50_000, &collateral_asset, &100_000);

    // Second borrow: 30_000 with 50_000
    // Total: 80_000 debt, 150_000 collateral = 187.5 %
    client.borrow(&user, &asset, &30_000, &collateral_asset, &50_000);

    let debt = client.get_user_debt(&user);
    assert_eq!(debt.borrowed_amount, 80_000);

    let collateral = client.get_user_collateral(&user);
    assert_eq!(collateral.amount, 150_000);

    // Required = 80_000 * 1.5 = 120_000. Max safe withdraw = 30_000
    let result = client.try_withdraw(&user, &collateral_asset, &30_001);
    assert_eq!(
        result,
        Err(Ok(WithdrawError::InsufficientCollateralRatio))
    );

    let remaining = client.withdraw(&user, &collateral_asset, &30_000);
    assert_eq!(remaining, 120_000);
}

// ─── 18. Repay all debt, then full withdraw allowed ─────────────────────────

#[test]
fn test_repay_all_then_full_withdraw_allowed() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    client.borrow(&user, &asset, &100_000, &collateral_asset, &200_000);

    // Repay all debt
    client.repay(&user, &asset, &100_000);

    let debt = client.get_user_debt(&user);
    assert_eq!(debt.borrowed_amount, 0);
    assert_eq!(debt.interest_accrued, 0);

    // Full withdraw should now be allowed
    let remaining = client.withdraw(&user, &collateral_asset, &200_000);
    assert_eq!(remaining, 0);
}

// ─── 19. View functions do not mutate state during withdraw check ───────────

#[test]
fn test_views_read_only_during_withdraw_validation() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    client.borrow(&user, &asset, &100_000, &collateral_asset, &200_000);

    // Capture state before view calls
    let debt_before = client.get_user_debt(&user);
    let collateral_before = client.get_user_collateral(&user);

    // Call multiple views
    let _ = client.get_health_factor(&user);
    let _ = client.get_user_position(&user);
    let _ = client.get_collateral_balance(&user);
    let _ = client.get_debt_balance(&user);

    // State must be unchanged
    let debt_after = client.get_user_debt(&user);
    let collateral_after = client.get_user_collateral(&user);

    assert_eq!(debt_before.borrowed_amount, debt_after.borrowed_amount);
    assert_eq!(debt_before.interest_accrued, debt_after.interest_accrued);
    assert_eq!(collateral_before.amount, collateral_after.amount);
}

// ─── 20. Minimum withdraw amount respected even with debt ───────────────────

#[test]
fn test_min_withdraw_respected_with_active_debt() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, user, asset, collateral_asset) = setup(&env);

    // Re-initialize with higher minimum
    client.initialize_withdraw_settings(&5000);

    client.borrow(&user, &asset, &100_000, &collateral_asset, &200_000);

    // Attempt to withdraw below minimum
    let result = client.try_withdraw(&user, &collateral_asset, &1000);
    assert_eq!(result, Err(Ok(WithdrawError::InvalidAmount)));

    // Withdraw at minimum should succeed (and pass ratio check)
    // Remaining = 195_000, required = 100_000 * 1.5 = 150_000 → OK
    let remaining = client.withdraw(&user, &collateral_asset, &5_000);
    assert_eq!(remaining, 195_000);
}

