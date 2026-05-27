#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoundingMode {
    Truncate,
    Floor,
    Bankers,
    Ceil,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoundingError {
    InvalidParameters,
    Overflow,
}

pub const INTEREST_PRECISION: i128 = 1_000_000;
pub const SECONDS_PER_YEAR: u64 = 365 * 24 * 60 * 60;
pub const BASIS_POINTS_SCALE: i128 = 10_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterestCalcResult {
    pub interest: i128,
    pub remainder: i128,
    pub total_drift: i128,
    pub mode: RoundingMode,
}

impl InterestCalcResult {
    pub fn new(interest: i128, remainder: i128, mode: RoundingMode) -> Self {
        Self {
            interest,
            remainder,
            total_drift: remainder,
            mode,
        }
    }
}

pub fn calculate_interest_with_rounding(
    borrowed_amount: i128,
    elapsed_seconds: u64,
    rate_bps: i128,
    mode: RoundingMode,
) -> Result<InterestCalcResult, RoundingError> {
    if borrowed_amount < 0 || rate_bps < 0 {
        return Err(RoundingError::InvalidParameters);
    }

    if borrowed_amount == 0 {
        return Ok(InterestCalcResult::new(0, 0, mode));
    }

    let amount_times_seconds = borrowed_amount
        .checked_mul(elapsed_seconds as i128)
        .ok_or(RoundingError::Overflow)?;

    let amount_times_seconds_times_rate = amount_times_seconds
        .checked_mul(rate_bps)
        .ok_or(RoundingError::Overflow)?;

    let with_precision = amount_times_seconds_times_rate
        .checked_mul(INTEREST_PRECISION)
        .ok_or(RoundingError::Overflow)?;

    let denominator = (SECONDS_PER_YEAR as i128)
        .checked_mul(BASIS_POINTS_SCALE)
        .ok_or(RoundingError::Overflow)?;

    let full_division = with_precision / denominator;
    let remainder = with_precision % denominator;

    let (rounded_interest, _) = apply_rounding(full_division, remainder, denominator, mode);

    let final_interest = rounded_interest / INTEREST_PRECISION;
    let final_remainder = rounded_interest % INTEREST_PRECISION;

    Ok(InterestCalcResult::new(
        final_interest,
        final_remainder,
        mode,
    ))
}

fn apply_rounding(
    quotient: i128,
    remainder: i128,
    divisor: i128,
    mode: RoundingMode,
) -> (i128, i128) {
    let half_divisor = divisor / 2;

    match mode {
        RoundingMode::Truncate | RoundingMode::Floor => (quotient, remainder),
        RoundingMode::Bankers => {
            if remainder < half_divisor {
                (quotient, remainder)
            } else if remainder > half_divisor {
                (quotient + 1, remainder - divisor)
            } else if quotient % 2 == 0 {
                (quotient, remainder)
            } else {
                (quotient + 1, remainder - divisor)
            }
        }
        RoundingMode::Ceil => {
            if remainder == 0 {
                (quotient, 0)
            } else {
                (quotient + 1, remainder - divisor)
            }
        }
    }
}

pub fn reconcile_debt_with_drift_correction(
    stored_debt: i128,
    freshly_calculated_debt: i128,
    accumulated_drift: i128,
    max_allowed_drift_bps: i128,
) -> Result<(i128, i128), RoundingError> {
    let debt_basis = if stored_debt > 0 {
        (freshly_calculated_debt - stored_debt)
            .checked_mul(10_000)
            .ok_or(RoundingError::Overflow)?
            / stored_debt
    } else {
        0
    };

    if debt_basis.abs() > max_allowed_drift_bps {
        return Err(RoundingError::InvalidParameters);
    }

    Ok((
        freshly_calculated_debt,
        accumulated_drift + (freshly_calculated_debt - stored_debt),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_zero_borrowed_returns_zero_interest() {
        let result =
            calculate_interest_with_rounding(0, SECONDS_PER_YEAR, 500, RoundingMode::Floor);
        assert_eq!(result.unwrap().interest, 0);
    }

    #[test]
    fn test_simple_one_year_accrual() {
        let result = calculate_interest_with_rounding(
            100,
            SECONDS_PER_YEAR,
            500,
            RoundingMode::Floor,
        )
        .unwrap();
        assert_eq!(result.interest, 5);
    }

    #[test]
    fn test_rounding_modes_differ_on_fractions() {
        let result_floor = calculate_interest_with_rounding(
            1000,
            SECONDS_PER_YEAR / 12,
            500,
            RoundingMode::Floor,
        )
        .unwrap();

        let result_ceil = calculate_interest_with_rounding(
            1000,
            SECONDS_PER_YEAR / 12,
            500,
            RoundingMode::Ceil,
        )
        .unwrap();

        assert!(result_ceil.interest >= result_floor.interest);
    }

    #[test]
    fn test_long_horizon_no_drift_with_bankers() {
        let mut total_interest = 0i128;
        let borrowed = 1000i128;
        let monthly_seconds = SECONDS_PER_YEAR / 12;

        for _ in 0..24 {
            let result = calculate_interest_with_rounding(
                borrowed,
                monthly_seconds,
                500,
                RoundingMode::Bankers,
            )
            .unwrap();
            total_interest += result.interest;
        }

        assert!(
            total_interest >= 95 && total_interest <= 105,
            "total_interest: {total_interest}"
        );
    }
}
