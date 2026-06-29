#![cfg(test)]
use proptest::prelude::*;
use crate::math::compute_max_borrow;
use crate::math::BPS_SCALE;
#[cfg(test)]
use proptest::prelude::*;
proptest! {
    #[test]
    fn max_borrow_monotonic_in_collateral(
        ltv in 0u32..BPS_SCALE,
        c1 in 1i128..1_000_000_000,
        c2 in 1_000_000_001i128..2_000_000_000,
    ) {
        let b1 = compute_max_borrow(c1, ltv).unwrap();
        let b2 = compute_max_borrow(c2, ltv).unwrap();

        prop_assert!(b2 >= b1);
    }

    #[test]
    fn max_borrow_monotonic_in_ltv(
        collateral in 1i128..1_000_000_000,
        ltv1 in 0u32..5000,
        ltv2 in 5001u32..BPS_SCALE,
    ) {
        let b1 = compute_max_borrow(collateral, ltv1).unwrap();
        let b2 = compute_max_borrow(collateral, ltv2).unwrap();

        prop_assert!(b2 >= b1);
    }

    #[test]
    fn zero_ltv_gives_zero_or_minimal_borrow(
        collateral in 1i128..1_000_000_000,
    ) {
        let b = compute_max_borrow(collateral, 0).unwrap();
        prop_assert!(b == 0);
    }
}
