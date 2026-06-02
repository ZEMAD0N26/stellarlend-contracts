# StellarLend Storage Tier Documentation

## Soroban Storage Tiers Overview

Soroban provides three storage tiers with different persistence guarantees and cost characteristics:

| Tier | Persistence | TTL (Time-To-Live) | Cost | Use Case |
|------|------------|-------------------|------|----------|
| **Persistent** | Survives contract deletion | Requires explicit bump | Higher rent | Protocol config, user positions, critical state |
| **Temporary** | Dropped when TTL expires | Auto-expires | Lower rent | Cached prices, session data, transient state |
| **Instance** | Bound to contract instance | Bumped with instance | Medium | Small config, flags, counters |

&gt; **Reference**: [Soroban Storage Documentation](https://soroban.stellar.org/docs/fundamentals/state-expiration)

---

## DataKey Storage Tier Decision Table

### Lending Contract (`stellar-lend/contracts/lending/src/lib.rs`)

| DataKey Variant | Storage Tier | Lifetime | Bump Frequency | Rationale |
|-----------------|-------------|----------|----------------|-----------|
| `Admin` | **Persistent** | Indefinite | On admin change | Critical protocol governance; must survive all conditions |
| `Collateral(Address)` | **Persistent** | Indefinite | On deposit/withdraw/liquidate | User funds; loss of data = loss of deposits |
| `Debt(Address)` | **Persistent** | Indefinite | On borrow/repay/liquidate | User debt tracking; must persist for liquidation |
| `Paused` | **Instance** | Bound to instance | On toggle | Small bool (1 byte); changes frequently; cheap to bump with instance |
| `AssetParams(Address)` | **Persistent** | Indefinite | On admin update | Risk parameters; must persist across upgrades |
| `DepositCap(Address)` | **Persistent** | Indefinite | On deposit/withdraw | Protocol safety limit; must survive for invariant checks |
| `ReservedForFlashLoan(Address)` | **Temporary** | 1 ledger | Auto-expire | In-flight flash loan balance; ledger-scoped by design |
| `InterestIndex` | **Persistent** | Indefinite | On borrow/repay | Cumulative interest; critical for debt calculation |
| `LastAccrualTime` | **Instance** | Bound to instance | On accrual | Small u64; frequent updates; instance bump is sufficient |
| `TotalBorrows` | **Persistent** | Indefinite | On borrow/repay | Protocol TVL metric; required for interest model |
| `TotalReserves` | **Persistent** | Indefinite | On borrow/repay/withdraw | Protocol revenue; must persist for accounting |
| `OracleAddress` | **Instance** | Bound to instance | On admin update | Small Address; changes rarely; instance storage sufficient |
| `RiskConfig` | **Instance** | Bound to instance | On admin update | Small struct (close_factor + liquidation_incentive); instance OK |
| `RateModelParams` | **Instance** | Bound to instance | On admin update | Small config struct; instance storage sufficient |
| `AMMHookAddress` | **Instance** | Bound to instance | On admin update | Small Address; optional feature flag |
| `FlashLoanFee` | **Instance** | Bound to instance | On admin update | Small u32 (BPS); instance storage sufficient |
| `UserNonce(Address)` | **Persistent** | Indefinite | On increment | Replay protection; must persist permanently |

### Hello-World Contract (`stellar-lend/contracts/hello-world/src/lib.rs`)

| DataKey Variant | Storage Tier | Lifetime | Bump Frequency | Rationale |
|-----------------|-------------|----------|----------------|-----------|
| `Admin` | **Instance** | Bound to instance | On init | Demo contract; minimal state; instance is sufficient |
| `Message` | **Temporary** | Short TTL | Auto-expire | Demo greeting; ephemeral by design |
| `Counter` | **Instance** | Bound to instance | On increment | Demo state; small u64; instance bump is cheap |

---

## TTL Bump Cadence

| Tier | Bump Trigger | Recommended TTL | Notes |
|------|-------------|-----------------|-------|
| **Persistent** | Every state-mutating entrypoint | 31 days (default) | Bump on every write to prevent expiration |
| **Instance** | Every entrypoint (auto-bumped) | 31 days (default) | Soroban auto-bumps instance storage on invocation |
| **Temporary** | N/A (auto-expire) | 1 ledger | Designed for single-ledger scope; no bump needed |

### Cadence Rationale

- **Persistent keys** are bumped explicitly in every state-mutating function via `env.storage().persistent().extend_ttl()`. This ensures user funds and protocol state never expire unexpectedly.
- **Instance keys** rely on Soroban's automatic instance bump on every contract invocation. Since instance storage is small and bounded, this is cost-effective.
- **Temporary keys** are never bumped. They are designed to expire automatically at ledger close, making them ideal for in-flight operations like flash loans where the state only matters within a single transaction.

---

## Cross-References

- **Typed Keys Refactor**: See `stellar-lend/contracts/lending/src/typed_keys.rs` (introduces strongly-typed `DataKey` enum)
- **Keys Audit**: See `KEYS_AUDIT.md` for security review of key derivation and collision resistance
- **Interest Numeric Assumptions**: See `docs/INTEREST_NUMERIC_ASSUMPTIONS.md` for precision and rounding rules affecting stored values

---

## Security Considerations

1. **Never store user funds in Temporary storage** — TTL expiration = permanent loss.
2. **Instance storage is capped** — Keep instance data under 64KB to avoid eviction.
3. **Bump Persistent storage on every write** — Missing bumps cause state expiration.
4. **Validate TTL before critical operations** — Check `env.storage().persistent().has()` before reads.
5. **Reserved flash loan counters** — Must be Temporary to avoid double-counting across ledgers.

---

## Migration Notes

When upgrading the contract:
- Persistent storage is preserved (new contract reads old state).
- Instance storage is reset (must re-initialize).
- Temporary storage is lost (expected; re-created per-ledger).