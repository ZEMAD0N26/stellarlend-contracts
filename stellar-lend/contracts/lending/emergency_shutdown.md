# Emergency Shutdown and Recovery Flow

This document describes the contracts-only emergency lifecycle implemented in the lending contract.

## State Machine

`Normal -> Shutdown -> Recovery -> Normal`

- `Normal`: regular operation.
- `Shutdown`: hard stop for high-risk operations.
- `Recovery`: controlled unwind mode where users can reduce risk.

## Roles

- `admin`: governance-controlled address. Can configure guardian and manage recovery lifecycle.
- `guardian`: optional fast-response address set by admin. Can trigger `emergency_shutdown`.

## Authorized Calls

- `set_guardian(admin, guardian)` -> admin only.
- `emergency_shutdown(caller)` -> admin or guardian.
- `start_recovery(admin)` -> admin only, only valid from `Shutdown`.
- `complete_recovery(admin)` -> admin only.

## Operation Policy by State

- `Normal`:
  - All operations follow existing granular pause rules.
  - **Liquidation Policy**: See pause.md for detailed liquidation-pause matrix.
- `Shutdown`:
  - Block: `deposit`, `deposit_collateral`, `borrow`, `liquidate`, `flash_loan`, `repay`, `withdraw`.
  - Allow: view/read methods and admin recovery actions.
  - **Liquidation Policy**: **BLOCKED** - Emergency stop prevents cascading failures.
- `Recovery`:
  - Allow: `repay`, `withdraw` (subject to granular pause and collateral checks).
  - Block: `deposit`, `deposit_collateral`, `borrow`, `liquidate`, `flash_loan`.
  - **Liquidation Policy**: **BLOCKED** - Unwind-only mode for safe position closure.

## Security Notes

- Emergency checks are enforced in both contract entrypoints and core borrow logic, including token-receiver deposit/repay paths.
- Recovery mode does not allow users to create new protocol exposure.
- Granular pauses still apply during recovery (for partial shutdown handling).
- All key transitions emit contract events (`guardian_set_event`, `emergency_state_event`, existing pause events).

## Operation Policy Matrix

| Operation              | Normal | Shutdown | Recovery | Notes                           |
| ---------------------- | ------ | -------- | -------- | ------------------------------- |
| `deposit`              | ✅\*   | ❌       | ❌       | Subject to granular pause rules |
| `deposit_collateral`   | ✅\*   | ❌       | ❌       | Subject to granular pause rules |
| `borrow`               | ✅\*   | ❌       | ❌       | Subject to granular pause rules |
| `repay`                | ✅\*   | ❌       | ✅\*     | Subject to granular pause rules |
| `withdraw`             | ✅\*   | ❌       | ✅\*     | Subject to granular pause rules |
| `liquidate`            | ✅\*   | ❌       | ❌       | Subject to granular pause rules |
| `flash_loan`           | ✅\*   | ❌       | ❌       | Subject to granular pause rules |
| View methods           | ✅     | ✅       | ✅       | Always available                |
| Admin recovery actions | ✅     | ✅       | ✅       | Admin only                      |

\*Subject to granular pause controls

## State Transition Authorization Matrix

| Transition          | Authorized Roles | Preconditions       |
| ------------------- | ---------------- | ------------------- |
| Normal → Shutdown   | Admin, Guardian  | None                |
| Shutdown → Recovery | Admin only       | Must be in Shutdown |
| Recovery → Normal   | Admin only       | Must be in Recovery |
| Normal → Recovery   | None             | Forbidden           |
| Shutdown → Normal   | None             | Forbidden           |
| Recovery → Shutdown | Admin, Guardian  | Emergency override  |

## Test Coverage

`src/emergency_shutdown_test.rs` covers basic emergency functionality:

- Authorization validation for shutdown triggers
- State transition flow testing
- Operation blocking in emergency states
- Recovery mode unwind operations
- Edge cases and partial pause interactions

`src/emergency_lifecycle_conformance_test.rs` provides comprehensive conformance validation:

- Complete state machine flow (Normal → Shutdown → Recovery → Normal)
- Authorization matrix enforcement (admin vs guardian roles)
- Operation permission validation per state
- Forbidden transition testing
- Role-based access control validation
- Multiple emergency cycle testing
- Granular pause interaction with emergency states

## Security Invariants

1. **State Machine Integrity**: Emergency transitions follow strict order and authorization
2. **Operation Boundaries**: High-risk operations blocked in Shutdown and Recovery states
3. **Role Separation**: Guardian can shutdown, only admin can manage recovery
4. **Recovery Safety**: Recovery mode allows unwind operations only
5. **Pause Layering**: Granular controls remain effective during emergency states
6. **Event Auditing**: All state transitions emit events for monitoring
