#[allow(unused_imports)]
use soroban_sdk::{contracttype, Env};

use stellar_lend_common::BPS_DENOM;

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RateParams {
    /// Base borrow rate in basis points applied at zero utilization.
    pub base_rate_bps: i128,
    /// Utilization kink in basis points where the jump multiplier starts.
    pub kink_utilization_bps: i128,
    /// Pre-kink slope in basis points per 100% utilization.
    pub multiplier_bps: i128,
    /// Post-kink slope in basis points per 100% excess utilization.
    pub jump_multiplier_bps: i128,
    /// Minimum borrow rate clamp in basis points.
    pub rate_floor_bps: i128,
    /// Maximum borrow rate clamp in basis points.
    pub rate_ceiling_bps: i128,
    /// Maximum borrow-rate change allowed per ledger when smoothing is enabled.
    pub max_rate_change_per_ledger_bps: i128,
    /// Dead-zone around the current rate, in basis points, that suppresses small target moves.
    pub hysteresis_bps: i128,
}

impl Default for RateParams {
    fn default() -> Self {
        Self {
            base_rate_bps: 100,
            kink_utilization_bps: 8_000,
            multiplier_bps: 2_000,
            jump_multiplier_bps: 10_000,
            rate_floor_bps: 50,
            rate_ceiling_bps: 10_000,
            max_rate_change_per_ledger_bps: i128::MAX,
            hysteresis_bps: 0,
        }
    }
}

pub fn compute_borrow_rate(utilization_bps: i128, params: &RateParams) -> i128 {
    let pre_kink_rate = params
        .base_rate_bps
        .checked_add(
            utilization_bps
                .min(params.kink_utilization_bps)
                .checked_mul(params.multiplier_bps)
                .unwrap()
                .checked_div(BPS_DENOM)
                .unwrap(),
        )
        .unwrap();

    let raw_rate = if utilization_bps > params.kink_utilization_bps {
        let excess = utilization_bps
            .checked_sub(params.kink_utilization_bps)
            .unwrap();
        let jump = excess
            .checked_mul(params.jump_multiplier_bps)
            .unwrap()
            .checked_div(BPS_DENOM)
            .unwrap();
        pre_kink_rate.checked_add(jump).unwrap()
    } else {
        pre_kink_rate
    };

    raw_rate
        .max(params.rate_floor_bps)
        .min(params.rate_ceiling_bps)
}

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RateModelKey {
    LastRate,
    LastRateLedger,
}

/// Applies a symmetric hysteresis band around `current` and returns an adjusted target.
///
/// When `target` lies inside the dead-zone `current ± band`, the current rate is held flat.
/// When `target` lies outside the band, the returned value is shifted to the nearest band edge
/// so subsequent smoothing continues from the edge without a discontinuous jump.
pub fn apply_hysteresis(current: i128, target: i128, band: i128) -> i128 {
    let band = band.max(0);
    let diff = match target.checked_sub(current) {
        Some(value) => value,
        None => {
            if target >= current {
                i128::MAX
            } else {
                i128::MIN
            }
        }
    };

    if diff >= 0 {
        if diff <= band {
            return current;
        }
        target.checked_sub(band).unwrap_or(target)
    } else {
        let abs_diff = diff.checked_abs().unwrap_or(i128::MAX);
        if abs_diff <= band {
            return current;
        }
        target.checked_add(band).unwrap_or(target)
    }
}

/// Computes the smoothed borrow rate bounded by a max per-ledger change.
///
/// # Arguments
/// * `last_rate` - The rate applied in the previous ledger update.
/// * `target_rate` - The instantaneous target rate computed from current utilization.
/// * `max_step` - The maximum allowed rate change per ledger (in basis points).
/// * `elapsed` - The number of ledgers elapsed since the last update.
/// * `hysteresis_bps` - Dead-zone around `last_rate` that suppresses small target moves.
///
/// # Returns
/// The smoothed borrow rate.
pub fn compute_smoothed_rate(
    last_rate: i128,
    target_rate: i128,
    max_step: i128,
    elapsed: u32,
    hysteresis_bps: i128,
) -> i128 {
    let adjusted_target = apply_hysteresis(last_rate, target_rate, hysteresis_bps);
    if elapsed == 0 || max_step == i128::MAX {
        return adjusted_target;
    }
    let max_change = max_step.saturating_mul(elapsed as i128);
   let diff = adjusted_target.checked_sub(last_rate).unwrap_or(
    if adjusted_target >= last_rate {
        i128::MAX
    } else {
        i128::MIN
    }
);
    if diff > 0 {
        last_rate
            .checked_add(diff.min(max_change))
            .unwrap_or(adjusted_target)
    } else {
        let decrease = diff.checked_abs().unwrap_or(i128::MAX).min(max_change);
        last_rate.checked_sub(decrease).unwrap_or(adjusted_target)
    }
}

/// Hook to update the persisted borrow rate state and return the new effective rate.
///
/// Bounded by the configurable step limit per ledger, and clamped to the rate params floor/ceiling.
pub fn update_and_get_rate(env: &Env, target_rate: i128, params: &RateParams) -> i128 {
    let current_ledger = env.ledger().sequence();
    let last_ledger = env
        .storage()
        .instance()
        .get(&RateModelKey::LastRateLedger)
        .unwrap_or(0);

    let last_rate = if last_ledger == 0 {
        target_rate
    } else {
        env.storage()
            .instance()
            .get(&RateModelKey::LastRate)
            .unwrap_or(target_rate)
    };

    let elapsed = if last_ledger == 0 {
        0
    } else {
        current_ledger.saturating_sub(last_ledger)
    };

    let new_rate = compute_smoothed_rate(
        last_rate,
        target_rate,
        params.max_rate_change_per_ledger_bps,
        elapsed,
        params.hysteresis_bps,
    );
    let clamped_rate = new_rate
        .max(params.rate_floor_bps)
        .min(params.rate_ceiling_bps);

    env.storage()
        .instance()
        .set(&RateModelKey::LastRate, &clamped_rate);
    env.storage()
        .instance()
        .set(&RateModelKey::LastRateLedger, &current_ledger);

    clamped_rate
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_scval_conversion() {
        use soroban_sdk::xdr::ScVal;
        let params = RateParams::default();
        let _scval = ScVal::try_from(&params).unwrap();
    }

    fn default_params() -> RateParams {
        RateParams::default()
    }

    #[test]
    fn test_zero_utilization_returns_base_rate() {
        let p = default_params();
        let rate = compute_borrow_rate(0, &p);
        assert_eq!(rate, 100);
    }

    #[test]
    fn test_utilization_at_kink() {
        let p = default_params();
        let rate = compute_borrow_rate(8_000, &p);
        // base + (kink * multiplier) / 10000 = 100 + (8000 * 2000) / 10000 = 100 + 1600 = 1700
        assert_eq!(rate, 1_700);
    }

    #[test]
    fn test_utilization_below_kink_is_linear() {
        let p = default_params();
        let rate = compute_borrow_rate(4_000, &p);
        // base + (4000 * 2000) / 10000 = 100 + 800 = 900
        assert_eq!(rate, 900);
    }

    #[test]
    fn test_utilization_above_kink_jumps() {
        let p = default_params();
        let rate = compute_borrow_rate(10_000, &p);
        // base + (kink * mult) / 10000 + ((util - kink) * jump) / 10000
        // = 100 + (8000 * 2000) / 10000 + (2000 * 10000) / 10000
        // = 100 + 1600 + 2000 = 3700
        assert_eq!(rate, 3_700);
    }

    #[test]
    fn test_rate_floor_clamps_low_rates() {
        let p = RateParams {
            base_rate_bps: 0,
            multiplier_bps: 100,
            rate_floor_bps: 200,
            ..Default::default()
        };
        let rate = compute_borrow_rate(1_000, &p);
        assert_eq!(rate, 200);
    }

    #[test]
    fn test_rate_ceiling_clamps_high_rates() {
        let p = RateParams {
            jump_multiplier_bps: 500_000,
            rate_ceiling_bps: 10_000,
            ..Default::default()
        };
        let rate = compute_borrow_rate(10_000, &p);
        assert_eq!(rate, 10_000);
    }

    #[test]
    fn test_full_utilization_clamped_to_ceiling() {
        let p = RateParams {
            rate_ceiling_bps: 5_000,
            ..Default::default()
        };
        // At util=40_000 the raw rate far exceeds 5_000; ceiling must clamp it.
        // raw: base(100) + kink_slope(1600) + jump((40000-8000)*10000/10000=32000) = 33700
        let rate = compute_borrow_rate(40_000, &p);
        assert_eq!(rate, 5_000);
    }

    #[test]
    fn test_monotonic_non_decreasing_at_kink() {
        let p = default_params();
        let before = compute_borrow_rate(7_999, &p);
        let at = compute_borrow_rate(8_000, &p);
        let after = compute_borrow_rate(8_001, &p);
        assert!(before <= at, "rate dropped at kink approach");
        assert!(at <= after, "rate dropped after kink");
    }

    #[test]
    fn test_utilization_above_supply_still_works() {
        let p = default_params();
        let rate = compute_borrow_rate(20_000, &p);
        assert!(rate >= p.rate_floor_bps);
        assert!(rate <= p.rate_ceiling_bps);
    }

    #[test]
    fn test_default_params_matches_init_sh() {
        let p = RateParams::default();
        assert_eq!(p.base_rate_bps, 100);
        assert_eq!(p.kink_utilization_bps, 8_000);
        assert_eq!(p.multiplier_bps, 2_000);
        assert_eq!(p.jump_multiplier_bps, 10_000);
        assert_eq!(p.rate_floor_bps, 50);
        assert_eq!(p.rate_ceiling_bps, 10_000);
        assert_eq!(p.max_rate_change_per_ledger_bps, i128::MAX);
        assert_eq!(p.hysteresis_bps, 0);
    }

    #[test]
    fn test_apply_hysteresis_returns_current_inside_band() {
        assert_eq!(apply_hysteresis(1_000, 1_020, 25), 1_000);
        assert_eq!(apply_hysteresis(1_000, 980, 25), 1_000);
        assert_eq!(apply_hysteresis(1_000, 975, 25), 1_000);
        assert_eq!(apply_hysteresis(1_000, 1_025, 25), 1_000);
    }

    #[test]
    fn test_apply_hysteresis_measures_from_band_edge() {
        assert_eq!(apply_hysteresis(1_000, 1_040, 25), 1_015);
        assert_eq!(apply_hysteresis(1_000, 960, 25), 985);
    }

    #[test]
    fn test_apply_hysteresis_overflow_safe() {
        assert_eq!(apply_hysteresis(i128::MIN, i128::MAX, i128::MAX), 0);
        assert_eq!(apply_hysteresis(i128::MAX, i128::MIN, i128::MAX), -1);
    }

    #[test]
    fn test_smoothing_disabled_returns_target_rate() {
        let last_rate = 100;
        let target_rate = 500;
        let new_rate = compute_smoothed_rate(last_rate, target_rate, i128::MAX, 10, 0);
        assert_eq!(new_rate, target_rate);
    }

    #[test]
    fn test_smoothing_moves_toward_target() {
        let last_rate = 100;
        let target_rate = 500;
        let step = 10;
        let elapsed = 5;
        // max change = 10 * 5 = 50
        let new_rate = compute_smoothed_rate(last_rate, target_rate, step, elapsed, 0);
        assert_eq!(new_rate, 150);
    }

    #[test]
    fn test_smoothing_holds_rate_flat_inside_hysteresis_band() {
        let new_rate = compute_smoothed_rate(1_000, 1_020, 10, 5, 25);
        assert_eq!(new_rate, 1_000);
    }

    #[test]
    fn test_smoothing_starts_from_hysteresis_band_edge() {
        let new_rate = compute_smoothed_rate(1_000, 1_040, 10, 1, 25);
        assert_eq!(new_rate, 1_010);
    }

    #[test]
    fn test_smoothing_converges_without_overshoot() {
        let last_rate = 100;
        let target_rate = 120;
        let step = 10;
        let elapsed = 5;
        // max change = 10 * 5 = 50. Since diff = 20 < 50, it should converge to target_rate.
        let new_rate = compute_smoothed_rate(last_rate, target_rate, step, elapsed, 0);
        assert_eq!(new_rate, target_rate);
    }

    #[test]
    fn test_smoothing_direction_down() {
        let last_rate = 500;
        let target_rate = 100;
        let step = 10;
        let elapsed = 5;
        // max change = 10 * 5 = 50.
        let new_rate = compute_smoothed_rate(last_rate, target_rate, step, elapsed, 0);
        assert_eq!(new_rate, 450);
    }

    #[test]
    fn test_smoothing_saturation_check() {
        let last_rate = 100;
        let target_rate = i128::MAX;
        let step = i128::MAX - 100;
        let elapsed = 2; // overflow multiplication
        let new_rate = compute_smoothed_rate(last_rate, target_rate, step, elapsed, 0);
        assert_eq!(new_rate, target_rate);
    }

    mod monotonicity {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(proptest::test_runner::Config::with_cases(256))]

            #[test]
            fn borrow_rate_monotonic_in_utilization(
                util_a in 0i128..=20_000i128,
                util_b in 0i128..=20_000i128,
            ) {
                let p = RateParams::default();
                let rate_a = compute_borrow_rate(util_a, &p);
                let rate_b = compute_borrow_rate(util_b, &p);
                if util_a <= util_b {
                    assert!(
                        rate_a <= rate_b,
                        "rate decreased: util {} -> {} gave rate {} -> {}",
                        util_a, util_b, rate_a, rate_b
                    );
                }
            }
        }

        proptest! {
            #![proptest_config(proptest::test_runner::Config::with_cases(256))]

            #[test]
            fn borrow_rate_always_between_floor_and_ceiling(
                util in 0i128..=50_000i128,
            ) {
                let p = RateParams::default();
                let rate = compute_borrow_rate(util, &p);
                assert!(
                    rate >= p.rate_floor_bps,
                    "rate {} below floor {}",
                    rate,
                    p.rate_floor_bps
                );
                assert!(
                    rate <= p.rate_ceiling_bps,
                    "rate {} above ceiling {}",
                    rate,
                    p.rate_ceiling_bps
                );
            }
        }

        proptest! {
            #![proptest_config(proptest::test_runner::Config::with_cases(256))]

            #[test]
            fn borrow_rate_non_negative(
                util in 0i128..=50_000i128,
            ) {
                let p = RateParams::default();
                let rate = compute_borrow_rate(util, &p);
                assert!(rate >= 0, "negative rate {}", rate);
            }
        }

        proptest! {
            #![proptest_config(proptest::test_runner::Config::with_cases(256))]

            #[test]
            fn borrow_rate_value_stable_across_same_utilization(
                util in 0i128..=50_000i128,
            ) {
                let p = RateParams::default();
                let rate_1 = compute_borrow_rate(util, &p);
                let rate_2 = compute_borrow_rate(util, &p);
                assert_eq!(rate_1, rate_2, "non-deterministic rate");
            }
        }
    }
}
