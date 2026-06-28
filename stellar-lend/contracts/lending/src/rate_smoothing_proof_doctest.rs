#[cfg(test)]
mod rate_smoothing_proof_doctest {
    use crate::rate_model::compute_smoothed_rate;

    #[test]
    fn test_upward_convergence() {
        let mut current_rate = 100;
        let target_rate = 110;
        let max_step = 2;
        let elapsed = 1;
        let hysteresis_bps = 0;

        let expected_trace = [102, 104, 106, 108, 110, 110, 110, 110, 110, 110];

        for expected in expected_trace {
            current_rate =
                compute_smoothed_rate(current_rate, target_rate, max_step, elapsed, hysteresis_bps);
            assert_eq!(current_rate, expected);
        }
    }

    #[test]
    fn test_downward_convergence() {
        let mut current_rate = 210;
        let target_rate = 200;
        let max_step = 2;
        let elapsed = 1;
        let hysteresis_bps = 0;

        let expected_trace = [208, 206, 204, 202, 200, 200, 200, 200, 200, 200];

        for expected in expected_trace {
            current_rate =
                compute_smoothed_rate(current_rate, target_rate, max_step, elapsed, hysteresis_bps);
            assert_eq!(current_rate, expected);
        }
    }
}
