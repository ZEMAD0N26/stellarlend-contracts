use crate::rounding_strategy::{
    calculate_interest_with_rounding, reconcile_debt_with_drift_correction, RoundingMode,
    RoundingError, SECONDS_PER_YEAR,
};

#[test]
fn test_24_month_long_horizon_drift_bounded() {
    let borrowed = 100_000i128;
    let monthly_seconds = SECONDS_PER_YEAR / 12;
    let mut total_interest = 0i128;

    for _ in 0..24 {
        let result = calculate_interest_with_rounding(
            borrowed,
            monthly_seconds,
            500,
            RoundingMode::Bankers,
        )
        .expect("should not overflow");
        total_interest += result.interest;
    }

    let expected = 10_000i128;
    let drift = (total_interest - expected).abs();
    assert!(drift <= 20, "Drift too large: {drift}");
}

#[test]
fn test_long_horizon_100_months_drift_tracking() {
    let borrowed = 50_000i128;
    let monthly_seconds = SECONDS_PER_YEAR / 12;
    let mut total_interest = 0i128;

    for _ in 0..100 {
        let result = calculate_interest_with_rounding(
            borrowed,
            monthly_seconds,
            500,
            RoundingMode::Bankers,
        )
        .expect("should not overflow");
        total_interest += result.interest;
    }

    let expected_approx = 20_825i128;
    let drift = (total_interest - expected_approx).abs();
    assert!(drift <= 50, "Long-horizon drift too large: {drift}");
}

#[test]
fn test_interest_monotonic_over_long_horizon() {
    let borrowed = 1_000_000i128;
    let mut previous_total = 0i128;

    for seconds_elapsed in [0, 100, 1000, 10000, 100000, 1000000, 10000000, 100000000] {
        let result = calculate_interest_with_rounding(
            borrowed,
            seconds_elapsed,
            500,
            RoundingMode::Bankers,
        )
        .expect("should not overflow");

        assert!(
            result.interest >= previous_total,
            "Interest decreased at {seconds_elapsed} seconds"
        );
        previous_total = result.interest;
    }
}

#[test]
fn test_rounding_modes_drift_comparison() {
    let borrowed = 1000i128;
    let one_month = SECONDS_PER_YEAR / 12;

    for mode in [
        RoundingMode::Floor,
        RoundingMode::Ceil,
        RoundingMode::Bankers,
    ] {
        let mut total = 0i128;
        for _ in 0..12 {
            let result =
                calculate_interest_with_rounding(borrowed, one_month, 500, mode).unwrap();
            total += result.interest;
        }
        let drift = (total - 50).abs();
        assert!(drift <= 10, "Excessive drift for {mode:?}: {drift}");
    }
}

#[test]
fn test_debt_reconciliation_within_tolerance() {
    let stored = 100i128;
    let fresh = 100i128;
    let accumulated_drift = 0i128;
    let max_allowed_drift_bps = 100i128;

    let result = reconcile_debt_with_drift_correction(
        stored,
        fresh,
        accumulated_drift,
        max_allowed_drift_bps,
    )
    .expect("reconciliation should succeed");

    assert_eq!(result, (100, 0));
}

#[test]
fn test_debt_reconciliation_rejects_excessive_drift() {
    let result = reconcile_debt_with_drift_correction(100, 200, 0, 100);
    assert_eq!(result, Err(RoundingError::InvalidParameters));
}

#[test]
fn test_extreme_horizon_overflow_protection() {
    let result = calculate_interest_with_rounding(
        i128::MAX / 2,
        u64::MAX,
        500,
        RoundingMode::Bankers,
    );
    assert!(result.is_err());
}

#[test]
fn test_small_amounts_precision() {
    let result = calculate_interest_with_rounding(1, SECONDS_PER_YEAR, 500, RoundingMode::Bankers)
        .expect("should not overflow");
    assert_eq!(result.interest, 0);
}

#[test]
fn test_high_rate_long_horizon() {
    let borrowed = 100_000i128;
    let one_month = SECONDS_PER_YEAR / 12;
    let mut total = 0i128;

    for _ in 0..12 {
        let result = calculate_interest_with_rounding(
            borrowed,
            one_month,
            10000,
            RoundingMode::Bankers,
        )
        .expect("should not overflow");
        total += result.interest;
    }

    assert!(total >= 95_000 && total <= 105_000, "total: {total}");
}
