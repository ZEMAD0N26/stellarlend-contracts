# Views — Health Factor and Read-Only Position Queries

This document describes the view functions for user collateral value, debt value, health factor, and position summary. These are **read-only**, **gas-efficient** entry points for frontends and liquidation logic.

## Overview

| Function | Description |
|----------|-------------|
| `get_position` | Returns full position summary (collateral, effective debt, health factor). |
| `get_health_factor` | Health factor (scaled 10000 = 1.0). |
| `get_debt_position` | Returns debt position struct (principal + last_update). |

---

## 1. `get_position(user: Address) -> PositionSummary`

- **Purpose:** Returns the user's position summary: raw collateral balance, effective debt (principal + accrued interest), and health factor.
- **Read-only:** Yes. Extends TTL for active positions.
- **Returns:** `PositionSummary { collateral: i128, debt: i128, health_factor: i128 }`. Zero-valued fields if the user has no position.
- **Health factor formula:** `(collateral * LIQUIDATION_THRESHOLD_BPS) / debt` with `LIQUIDATION_THRESHOLD_BPS = 8000` (80%).
- **Special health factor values:**
  - No debt: returns `HEALTH_FACTOR_NO_DEBT` (100_000_000), meaning "healthy".
  - Overflow in calculation: returns `i128::MAX`.

---

## 2. `get_health_factor(user: Address) -> i128`

- **Purpose:** Health factor for liquidations and UI. Computed from raw collateral, effective debt, and the hardcoded liquidation threshold.
- **Read-only:** Yes. Extends TTL for active positions.
- **Formula:**  
  `health_factor = (collateral * LIQUIDATION_THRESHOLD_BPS) / debt`  
  with `LIQUIDATION_THRESHOLD_BPS = 8000` (80%) and implicit `HEALTH_FACTOR_SCALE = 10000`, so **10000 = 1.0**.
- **Interpretation:**
  - **> 10000:** Healthy (above liquidation threshold).
  - **< 10000:** Liquidatable.
  - **= 10000:** Boundary (at liquidation threshold).
- **Special values:**
  - No debt: returns `HEALTH_FACTOR_NO_DEBT` (100_000_000), meaning "healthy".
  - Overflow in calculation: returns `i128::MAX`.

---

## 3. `get_debt_position(user: Address) -> DebtPosition`

- **Purpose:** Returns the user's debt tracking struct containing principal and last_update timestamp.
- **Read-only:** Yes. Extends TTL for active positions.
- **Returns:** `DebtPosition { principal: i128, last_update: u64 }`.

---

## Security Assumptions

1. **No state change:** All view functions only read storage. They do not modify protocol or user state beyond TTL extension.
2. **Liquidation threshold:** Currently hardcoded at 8000 BPS (80%) as `LIQUIDATION_THRESHOLD_BPS`.
3. **Overflow:** Health factor calculations use checked arithmetic where applicable; edge cases (e.g. zero debt) are handled explicitly.

---

## Gas and Usage

- Views are designed to be callable without authorization and with minimal state changes (TTL extension only), so they are suitable for read-only RPC calls and UIs.
- `get_position` aggregates one read of collateral and one of debt, returning both raw values and the computed health factor in a single call.

---

## View Guarantees

The view layer is a load-bearing surface for liquidation bots, frontends, and
downstream contracts. The following guarantees are pinned by the invariant
suite in `stellar-lend/contracts/lending/src/views_test.rs` and must never be
weakened without an explicit, audited change.

### G1. Summary–getter consistency

`get_user_position(user)` must return field-for-field exactly what the
individual getters return for the same `user` at the same ledger height:

- `summary.collateral_balance == get_collateral_balance(user)`
- `summary.debt_balance == get_debt_balance(user)`
- `summary.collateral_value == get_collateral_value(user)`
- `summary.debt_value == get_debt_value(user)`
- `summary.health_factor == get_health_factor(user)`

### G2. Stable serialization (idempotence)

The view output is a pure function of `(storage, oracle, ledger height)`.
Repeated calls in any order must yield bit-identical results — no view path
may mutate state, cache stale derived values, or depend on call order.

### G3. Threshold isolation

Changing `liquidation_threshold_bps` may move `health_factor` but must not
move any of `collateral_balance`, `collateral_value`, `debt_balance`, or
`debt_value`. Those four are functions of raw state and oracle output only.

### G4. Missing-asset and missing-oracle behaviour

- A user with no recorded position returns a default summary: zero balances,
  zero values, and `health_factor == HEALTH_FACTOR_NO_DEBT`.
- When the oracle is unconfigured, every value-bearing field reads as `0`
  consistently. Raw balance fields remain exact and non-zero. The contract
  refuses to emit a non-zero `health_factor` without price data so liquidators
  cannot act on stale assumptions.

### G5. Rounding semantics

Health-factor division truncates toward zero. The boundary case
`health_factor == HEALTH_FACTOR_SCALE` (exactly 1.0) is treated as healthy:
`get_max_liquidatable_amount` returns `0` here. Any refactor that switches to
ceiling rounding or float math will break the invariant suite.

### G6. Liquidation-incentive monotonicity

`get_liquidation_incentive_amount(repay)` is monotonic non-decreasing in
`repay`. Negative or zero `repay` always yields `0`. This forbids a future
incentive curve that liquidators could game by splitting repayments.

### G7. Independence across users

Each user's summary depends only on that user's positions and the global
risk parameters. There is no cross-user contamination — pinned by the
"independent users" invariant test.

### Security: no view-based exploitation assumptions

- Views never mutate state, never charge fees, and never trigger external
  contract calls beyond the read-only oracle lookup. Callers may safely
  invoke them off-chain.
- Integrators MUST NOT rely on a view's value beyond the ledger height at
  which it was observed. Oracle prices and risk parameters can change.

---

## Example Commit Message

```
feat: implement health factor and view functions with tests and docs
```
