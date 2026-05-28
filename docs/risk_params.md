# Risk Parameters

This document provides a consolidated view of all protocol risk parameters, including their purpose, constraints, and how they are configured.

> Verified against contract constants and admin setter constraints where applicable.


## Risk Parameters Table

| Parameter | Meaning | Default | Bounds | Setter Function | Rationale |
|----------|--------|--------|--------|----------------|-----------|
| **Minimum Collateral Ratio** | Minimum ratio of collateral to total debt required for a borrow to succeed, in basis points | **15 000 bps (150 %)** | > 0 | `set_collateral_ratio(admin, ratio)` | Prevents under-collateralised borrowing and protects protocol solvency |
| Close Factor | Maximum portion of a borrow position that can be liquidated in a single transaction | Defined in code | 0% – 100% | Admin-controlled setter | Prevents full liquidation at once, reducing market shock and cascading failures |
| Liquidation Threshold | Collateral ratio below which a position becomes eligible for liquidation | Defined in code | Protocol-defined bounds | Admin-controlled setter | Ensures positions remain sufficiently collateralized and protects lenders |
| Reserve Factor | Percentage of interest allocated to protocol reserves | Defined in code | 0% – 100% | Admin-controlled setter | Builds reserves for protocol stability and risk mitigation |
| Supply Cap | Maximum total supply allowed for a specific asset | Defined in code | ≥ 0 | Admin-controlled setter | Limits exposure to any single asset and reduces systemic risk |
| Borrow Cap | Maximum total borrow allowed for a specific asset | Defined in code | ≥ 0 | Admin-controlled setter | Prevents excessive leverage and liquidity stress |
| Minimum Borrow | Minimum borrowable amount | Defined in code | ≥ 0 | Admin-controlled setter | Avoids inefficient micro-loans and reduces spam |
| Rate Limits | Constraints on how quickly parameters or balances can change | Defined in code | Protocol-defined bounds | Admin-controlled setter | Prevents sudden parameter manipulation and extreme volatility |

### Collateral-Ratio Formula

The borrow entrypoint (`stellar-lend/contracts/lending/src/lib.rs`) enforces:

```
collateral * 10_000 >= col_ratio * (existing_debt + borrow_amount)
```

where `col_ratio` is stored under the `"col_ratio"` instance-storage key (default **15 000 bps = 150 %**).

- `collateral` — value stored at `("col", user)` persistent key
- `existing_debt` — value stored at `("debt", user)` persistent key before the borrow
- `borrow_amount` — the requested borrow amount
- Arithmetic is performed with `checked_mul` / `checked_add`; overflow returns `BorrowError::Overflow` (code 2)
- Zero or negative collateral always returns `BorrowError::InsufficientCollateral` (code 1)


## Implementation Notes

- All parameters are enforced at the smart contract level.
- Validation is applied through:
  - Constant definitions
  - Admin setter functions
- Any parameter updates must pass bounds checks before being applied.


## Verification

To verify correctness, refer to:

- Contract constants (e.g., `constants.rs`)
- Admin setter implementations in lending modules

Developers should ensure that:
- Documented bounds match enforced ranges
- Default values align with deployed configuration


## Design Considerations

These parameters are designed to balance:

- Protocol safety  
- Capital efficiency  
- Market stability  

Changes to these values should be governed carefully to avoid unintended economic consequences.
