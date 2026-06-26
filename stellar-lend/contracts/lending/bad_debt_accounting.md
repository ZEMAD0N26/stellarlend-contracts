# Bad Debt Accounting Specification

The protocol guards against system insolvency using an on-chain bad-debt accumulator tracker.

## Shortfall Accumulator Mechanism
When a liquidation event triggers, the liquidator is entitled to an incentivized collateral amount (`seized_collateral`). If the user's positions drop below safe limits such that `seized_collateral > available_collateral`, the system clamps the payout and tracks the shortfall directly.

$$\text{Shortfall} = \text{Seized Collateral} - \text{Available Collateral}$$

This value is stored via `DataKey::BadDebt` using state instance storage.

### Worked Example
- **Incentivized Seizure Target:** 150 Collateral Tokens
- **Available Borrower Collateral:** 100 Collateral Tokens
- **Shortfall Recorded:** 50 Tokens added monotonically to the BadDebt ledger.

### Invariants
- **Monotonic Increase:** Bad debt values can never decrease except via explicit, future administrative `write_off_bad_debt` entrypoints (currently left as a TODO).
- **Checked Arithmetic:** All computations run inside safe `checked_sub` and `checked_add` parameters to rule out integer overflow vulnerabilities.