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
- `set_emergency_state(caller, new_state)` -> admin or guardian. Caller must present auth. Emits `EmergencyStateChanged(old_state, new_state)`.
- Recovery lifecycle transitions (`Shutdown -> Recovery -> Normal`) are admin-only when moving out of `Shutdown`.

## Operation Policy by State

- `Normal`:
  - All user-facing mutation entrypoints operate normally (subject to existing granular pause rules).
- `Shutdown`:
  - Block: `deposit`, `borrow`, `withdraw`, `repay`, `flash_loan`, `liquidate`, and other mutating entrypoints. (Contract is effectively read-only for users.)
  - Allow: view/read methods and admin recovery actions.
- `Recovery`:
  - Allow: `repay`, `withdraw` (users can reduce exposure and exit positions).
  - Block: `deposit`, `borrow`, `flash_loan`, `liquidate`, and other operations that create new exposure.

## Security Notes

- Emergency checks are enforced in both contract entrypoints and core borrow logic, including token-receiver deposit/repay paths.
- Recovery mode does not allow users to create new protocol exposure.
- Granular pauses still apply during recovery (for partial shutdown handling).
- All key transitions emit contract events (`guardian_set_event`, `emergency_state_event`, existing pause events).

## Operation Policy Matrix

| Operation              | Normal | Shutdown | Recovery | Notes                           |
| ---------------------- | ------ | -------- | -------- | ------------------------------- |
| `deposit`              | ✅\*   | ❌       | ❌       | Blocked outside `Normal`        |
| `borrow`               | ✅\*   | ❌       | ❌       | Blocked outside `Normal`        |
| `repay`                | ✅\*   | ❌       | ✅\*     | Blocked in `Shutdown` only      |
| `withdraw`             | ✅\*   | ❌       | ✅\*     | Blocked in `Shutdown` only      |
| `flash_loan`           | ✅\*   | ❌       | ❌       | Blocked outside `Normal`        |
| `liquidate`            | ✅\*   | ❌       | ❌       | Blocked outside `Normal`        |
| View methods           | ✅     | ✅       | ✅       | Always available                |
| Admin recovery actions | ✅     | ✅       | ✅       | Admin only to progress recovery |

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
