# Multisig Module

## Overview

The **multisig** module (`src/multisig.rs`) implements a proposal–approve–execute governance pattern for critical StellarLend protocol parameters. It is a thin, focused layer on top of `governance.rs` that adds admin-set management (`ms_set_admins`) and a clean public API for the multisig flow.

---

## Flow

```
ms_set_admins([A1, A2, A3], threshold=2)
          │
A1 calls ms_propose_set_min_cr(new_ratio=20000)
          │  ← A1 auto-approves
          │
A2 calls ms_approve(proposal_id)
          │  ← threshold (2) met
          │
[wait for execution timelock — default 2 days]
          │
A3 calls ms_execute(proposal_id)
          │
Protocol parameter updated; proposal marked Executed
```

---

## Storage Layout

Shares all storage with `governance.rs` via `GovernanceDataKey`:

| Key | Type | Description |
|-----|------|-------------|
| `GovernanceDataKey::MultisigAdmins` | `Vec<Address>` | Current admin set |
| `GovernanceDataKey::MultisigThreshold` | `u32` | Approval quorum |
| `GovernanceDataKey::ProposalCounter` | `u64` | Monotonic proposal ID counter |
| `GovernanceDataKey::Proposal(id)` | `Proposal` | Proposal data, including `expires_at_ledger: u32` |
| `GovernanceDataKey::ProposalApprovals(id)` | `Vec<Address>` | Per-proposal approvals; removed with its proposal during expiry cleanup |

---

## Functions

### `ms_set_admins(env, caller, admins, threshold)`

> **Auth:** Existing admin (or any caller at first bootstrap)

Replaces the multisig admin set and threshold atomically.

| Param | Type | Constraint |
|-------|------|-----------|
| `admins` | `Vec<Address>` | Non-empty, no duplicates |
| `threshold` | `u32` | `1 ≤ threshold ≤ len(admins)` |

**Errors:** `Unauthorized`, `InvalidMultisigConfig`

Rotation guidance:

- Use `ms_set_admins(...)` to replace the full signer set in one governance action when rotating
  the humans or devices behind a multisig-controlled admin role.
- Avoid "remove one signer now, add the replacement later" workflows for governance signers. The
  multisig contract already supports atomic replacement, which is safer and easier to audit.
- If this multisig is the stored upgrade `admin`, finish multisig signer rotation here before
  rotating any separate upgrade approver keys in `docs/UPGRADE_AUTHORIZATION.md`.

---

### `ms_propose_set_min_cr(env, proposer, new_ratio)`

> **Auth:** Registered multisig admin

Creates a `MinCollateralRatio` proposal. The proposer automatically approves.

| Param | Type | Constraint |
|-------|------|-----------|
| `new_ratio` | `i128` | > 10,000 bps (> 100%) |

**Returns:** `u64` proposal ID

**Errors:** `Unauthorized`, `InvalidProposal`

**Events:** `proposal_created(proposal_id, proposer)` + `proposal_approved(proposal_id, proposer)`

---

### `ms_approve(env, approver, proposal_id)`

> **Auth:** Registered multisig admin

Adds one approval to a proposal. Duplicate approvals rejected.

**Errors:** `Unauthorized`, `ProposalNotFound`, `AlreadyVoted`

**Events:** `proposal_approved(proposal_id, approver)`

---

### `ms_execute(env, executor, proposal_id)`

> **Auth:** Registered multisig admin

Executes the proposal after the approval threshold is met **and** the execution timelock has elapsed. Execution also checks `env.ledger().sequence() <= proposal.expires_at_ledger`; stale proposals whose expiry ledger is in the past are rejected and must be recreated so approvals reflect current signer intent.

**Errors:** `Unauthorized`, `InsufficientApprovals`, `ProposalNotReady`, `ProposalAlreadyExecuted`, `ProposalExpired`

**Events:** `proposal_executed(proposal_id, executor)`

---

### `cleanup_expired(env, ids)`

> **Auth:** Multisig admin / cleanup administrator

Removes expired, unexecuted proposal records and their approval vectors from contract storage. The call is deliberately batched by explicit proposal IDs so operators can bound transaction cost and review exactly which records will be deleted. Fresh proposals and executed proposals are retained for auditability.

| Param | Type | Constraint |
|-------|------|-----------|
| `ids` | `Vec<u64>` | Proposal IDs to inspect and clean |

**Returns:** number of proposals removed.

**Errors:** `Unauthorized`, `NotInitialized`

---

## View Functions

| Function | Returns | Description |
|----------|---------|-------------|
| `get_ms_admins(env)` | `Option<Vec<Address>>` | Current admin list |
| `get_ms_threshold(env)` | `u32` | Approval threshold (default `1`) |
| `get_ms_proposal(env, id)` | `Option<Proposal>` | Proposal by ID |
| `get_ms_approvals(env, id)` | `Option<Vec<Address>>` | Approvals for a proposal |
| `get_default_proposal_expiry_ledgers(env)` | `u32` | Default proposal lifetime used by the multisig crate |

---

## Security Model

| Threat | Mitigation |
|--------|-----------|
| Single admin key compromise | t-of-n threshold before any parameter changes |
| Replay of executed proposals | `ProposalStatus::Executed` checked; `ProposalAlreadyExecuted` returned on second attempt |
| Old proposal ID reuse | Monotonic counter in `governance.rs` — IDs never decrease |
| Front-running a proposal | Proposer auto-approves in the same call, so no window between creation and first approval |
| Rushed execution | Execution timelock (default 2 days) gives time to detect malicious proposals |
| Stale approval execution | `expires_at_ledger` is stored on every proposal and `ms_execute` / `execute_proposal` rejects `current_ledger > expires_at_ledger` |
| Storage bloat from expired proposals | `cleanup_expired(ids)` removes expired unexecuted proposals and their approvals in bounded batches |

---

## Extending with New Actions

To add a new governable parameter (e.g. `SetReserveFactor`):

1. Add a variant to `ProposalType` in `governance.rs`:
   ```rust
   SetReserveFactor(i128),
   ```
2. Add a new propose function in `multisig.rs`:
   ```rust
   pub fn ms_propose_set_reserve_factor(env: &Env, proposer: Address, factor: i128)
       -> Result<u64, GovernanceError> { ... }
   ```
3. Add execution logic inside `execute_proposal` in `governance.rs`:
   ```rust
   ProposalType::SetReserveFactor(f) => { /* persist */ }
   ```
4. Add tests in `multisig_test.rs`.
5. Expose the entrypoint in `lib.rs`.

---

## Integration — `lib.rs` changes needed

Add to `lib.rs`:

```rust
pub mod multisig;

use multisig::{ms_set_admins, ms_propose_set_min_cr, ms_approve, ms_execute};
```

Then expose on `HelloContract`:

```rust
pub fn ms_set_admins(env: Env, caller: Address, admins: Vec<Address>, threshold: u32)
    -> Result<(), GovernanceError> { multisig::ms_set_admins(&env, caller, admins, threshold) }

pub fn ms_propose_set_min_cr(env: Env, proposer: Address, new_ratio: i128)
    -> Result<u64, GovernanceError> { multisig::ms_propose_set_min_cr(&env, proposer, new_ratio) }

pub fn ms_approve(env: Env, approver: Address, proposal_id: u64)
    -> Result<(), GovernanceError> { multisig::ms_approve(&env, approver, proposal_id) }

pub fn ms_execute(env: Env, executor: Address, proposal_id: u64)
    -> Result<(), GovernanceError> { multisig::ms_execute(&env, executor, proposal_id) }
```

---

## Events Reference

All events emitted via helpers in `governance.rs`:

| Event | Topics | Payload |
|-------|--------|---------|
| `proposal_created` | `(proposal_id, proposer)` | — |
| `proposal_approved` | `(proposal_id, approver)` | — |
| `proposal_executed` | `(proposal_id, executor)` | — |
| `proposal_failed` | `(proposal_id)` | — |

---

## Safe Threshold and Signer-Set Change Workflow

Changing the multisig threshold or signer set is a high-risk operation. An
incorrect sequence can create a window where protocol actions are executable
with weaker security than intended, or leave governance permanently deadlocked.

### Recommended Sequences

#### Raising security (adding signers or increasing threshold)

Always use `ms_set_admins` to atomically replace both the signer list and the
threshold in a single call. This eliminates any intermediate state.

```
# Safe: atomic replace — new threshold applies to the new set immediately
ms_set_admins([A1, A2, A3], threshold=2)
```

If you must use two steps, raise the threshold **before** adding the new signer:

```
# Step 1: raise threshold while signer count is still the same
ms_set_admins([A1, A2], threshold=2)   # was threshold=1

# Step 2: add A3 — threshold is already at the desired level
ms_set_admins([A1, A2, A3], threshold=2)
```

#### Lowering security (removing signers or decreasing threshold)

Lowering the threshold or removing a signer should be done with extra caution.
Prefer the atomic form:

```
# Safe: atomic replace
ms_set_admins([A1, A2], threshold=2)   # removes A3, keeps threshold
```

If you must lower the threshold separately, do it **after** removing the signer:

```
# Step 1: remove A3 first (threshold stays at 2-of-2, still valid)
ms_set_admins([A1, A2], threshold=2)

# Step 2: lower threshold only if intentional
set_ms_threshold(threshold=1)
```

Never lower the threshold before removing a signer — this creates a window
where fewer approvals than intended can execute proposals.

#### Replacing the entire signer set

Use a single `ms_set_admins` call. The old set is replaced atomically; there
is no window where the old threshold applies to the new set or vice versa.

```
ms_set_admins([NewA1, NewA2, NewA3], threshold=2)
```

---

## Security Notes: Preventing Downgrade Attacks

### Threshold is captured at proposal creation time

When a proposal is created via `ms_propose_set_min_cr` (or any propose
function), the **current multisig threshold is stored on the proposal** in the
`multisig_threshold` field. This stored value is the binding quorum for that
proposal — it cannot be retroactively changed.

This prevents the following attack:

1. Attacker creates a proposal when threshold = 3 (needs 3 approvals).
2. Attacker lowers threshold to 1.
3. Attacker tries to execute with only 1 approval.

Step 3 fails because `ms_execute` checks `proposal.multisig_threshold` (= 3),
not the current global threshold (= 1).

### Constraints enforced on every threshold/signer change

| Constraint | Enforced by | Error |
|---|---|---|
| Threshold ≥ 1 | `ms_set_admins`, `set_ms_threshold` | `InvalidMultisigConfig` / `InvalidThreshold` |
| Threshold ≤ signer count | `ms_set_admins`, `set_ms_threshold` | `InvalidMultisigConfig` / `InvalidThreshold` |
| No duplicate signers | `ms_set_admins` | `InvalidMultisigConfig` |
| Non-empty signer set | `ms_set_admins` | `InvalidMultisigConfig` |
| Caller must be existing admin | `ms_set_admins` (post-bootstrap), `set_ms_threshold` | `Unauthorized` |

### Execution timelock

`ms_execute` enforces a 24-hour delay from proposal creation before any
proposal can be executed, regardless of how many approvals it has. This gives
the remaining admins time to detect and respond to a malicious proposal before
it takes effect.

### Expiry window

Each proposal stores an explicit `expires_at_ledger: u32`. Proposal creation must set this value far enough in the future to cover the execution timelock, review period, and expected operational delay. The multisig crate default is 14 days of ledgers.

Execution is valid through the exact expiry ledger (`current_ledger == expires_at_ledger`) and rejected once the ledger sequence advances past it (`current_ledger > expires_at_ledger`). An expired proposal cannot be executed; a new proposal must be created and approved so the quorum reflects current intent and current protocol state.

### Operational cleanup cadence

Run `cleanup_expired(ids)` as part of normal governance operations, ideally after each governance meeting and at least weekly for active deployments. Suggested procedure:

1. Enumerate pending proposal IDs from governance monitoring.
2. Filter to proposals where `env.ledger().sequence() > expires_at_ledger` and `status != Executed`.
3. Submit bounded cleanup batches sized to fit Soroban transaction limits.
4. Archive proposal metadata off-chain before cleanup if long-term audit records are required.

Cleanup is not a substitute for the execution guard. Even if cleanup is delayed, expired proposals still cannot execute because execution checks `expires_at_ledger` on-chain.

### Bootstrap vs. post-bootstrap

`ms_set_admins` accepts any caller during the initial bootstrap (when no
multisig config exists yet). After bootstrap, only an existing admin can call
it. Ensure the bootstrap call is made in the same transaction as contract
initialization to avoid a front-running window.

---

## Failure Recovery

### Scenario: threshold accidentally set too high (governance deadlocked)

If the threshold is set higher than the number of available signers (e.g. a
key is lost), governance is deadlocked. Recovery options:

1. **Social recovery** — if guardians are configured, use `start_recovery` /
   `approve_recovery` / `execute_recovery` to rotate the admin key, then
   reconfigure the multisig.
2. **Key recovery** — recover the lost signing key from secure backup.

Prevention: always keep at least one more signer than the threshold (n-of-m
where m > n) so a single key loss does not deadlock governance.

### Scenario: malicious proposal approved before detection

If a malicious proposal reaches the approval threshold:

1. The 24-hour execution timelock gives a response window.
2. Any existing admin can call `cancel_proposal` before execution.
3. After cancellation, rotate the compromised key via `ms_set_admins`.

### Scenario: signer key compromised

1. Immediately call `ms_set_admins` with the compromised key removed and a
   replacement key added, keeping the threshold the same or higher.
2. Review all pending proposals for approvals from the compromised key.
3. Cancel any proposals that were approved by the compromised key if their
   legitimacy is in doubt.
