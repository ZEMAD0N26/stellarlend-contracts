# Contract Interface Quick Reference

This document provides a single source of truth for frontend integrators
interacting with the StellarLend smart contracts.

---

## 1. Unit Scales & Precisions

| Parameter | Scale | Description |
|---|---|---|
| Amounts | 10^7 | All asset amounts (XLM, USDC, etc.) use 7 decimal places. |
| Health Factor | 10^4 | 1.0 = 10000. Values < 10000 are subject to liquidation. |
| BPS (Basis Points) | 10^4 | 1% = 100 BPS. Used for interest rates and fees. |
| Timestamps | Seconds | Unix epoch timestamps. |

---

## 2. Core View Functions

### `get_position(user: Address) -> PositionSummary`
Returns `{ collateral, debt, health_factor }` for the user.  
**Use case:** Dashboard health check, borrow eligibility.

### `get_debt_position(user: Address) -> DebtPosition`
Returns raw `{ principal, last_update }` for the user's debt entry.  
**Use case:** Interest accrual calculations, repay amount estimation.

### `get_min_borrow() -> i128`
Returns the minimum borrow amount set by the admin (0 if unset).

### `get_admin() -> Result<Address, LendingError>`
Returns the admin address. Returns `LendingError::NotInitialized` if the
contract has not yet been initialized — use `try_get_admin()` on the client
and handle the error explicitly.

---

## 3. Admin Entrypoints (all require admin auth)

| Entrypoint | Returns | Notes |
|---|---|---|
| `initialize(admin)` | `Result<(), LendingError>` | Single-call only. Rejects with `AlreadyInitialized` after first call. |
| `propose_admin(new_admin)` | `Result<(), LendingError>` | Begins two-step rotation. |
| `accept_admin()` | `Result<(), LendingError>` | Pending admin signs to complete. |
| `set_min_borrow(amount)` | `Result<(), LendingError>` | Sets minimum borrow floor. |
| `set_debt_ceiling(ceiling)` | `Result<(), LendingError>` | Protocol-level borrow cap. |
| `set_flash_fee(fee_bps)` | `Result<(), LendingError>` | Must be in `[0, 1000]`. |
| `set_guardian(guardian)` | `Result<(), LendingError>` | Grants emergency pause rights. |
| `set_emergency_state(state)` | `Result<(), LendingError>` | Admin **or** guardian. |

---

## 4. Error Mapping Guidance

| Error code | Name | Suggested UI message |
|---|---|---|
| 1008 | `BelowMinimumBorrow` | "Amount is too small. Minimum borrow required." |
| 1009 | `NotInitialized` | "Contract not ready. Contact support." |
| 1010 | `AlreadyInitialized` | (internal — should never reach UI) |
| 2001 | `DebtCeilingExceeded` | "Protocol borrow limit reached. Try a smaller amount." |
| 2002 | `DepositCapExceeded` | "Deposit limit reached. Try a smaller amount." |
| 2003 | `Overflow` | "Amount too large. Please enter a smaller value." |
| 2004 | `Unauthorized` | "You are not authorised to perform this action." |
| 2005 | `InvalidFeeBps` | "Fee must be between 0 and 10%." |
| 2006 | `PositionHealthy` | "This position cannot be liquidated yet." |

---

## 5. Events to Subscribe

| Event | Emitted when |
|---|---|
| `EmergencyStateChanged` | Emergency state transitions |

---

## 6. Integration Checklist

- [ ] Call `try_initialize` and handle `AlreadyInitialized` gracefully on
  re-deploys.
- [ ] Use `try_get_admin()` instead of `get_admin()` before the contract is
  guaranteed to be initialized.
- [ ] Convert UI inputs to 10^7 scale before sending to contract.
- [ ] Check `health_factor` from `get_position` before allowing further
  borrows (liquidation threshold is 10000).
- [ ] Verify `user.require_auth()` is handled by the wallet connector for
  all user operations (deposit, withdraw, borrow, repay).
- [ ] Admin operations require the admin key to sign — never expose the admin
  private key in a browser context.
