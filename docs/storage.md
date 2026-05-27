# StellarLend Storage Layout and Migration Guide

This document describes the persistent storage structure of the StellarLend protocol on Soroban.

## Overview

StellarLend uses Soroban's `persistent()` storage for all long-term data.
All keys are defined using `contracttype` enums.

> [!IMPORTANT]
> **Namespace Isolation**: To prevent collisions between modules, all storage key enum variants MUST be unique across the entire contract. Even different enum types will collide if their variants share the same name (as they serialize to the same `Symbol`).

## Storage Map

### 1. Lending Contract (`stellar-lend/contracts/lending/src/lib.rs`)

The lending contract centralizes its storage namespace in a single
`#[contracttype] enum DataKey`. This is the canonical key list for that module.

| Key (`DataKey`) | Storage Class | Value Type | Description |
|-----------------|---------------|------------|-------------|
| `Admin` | `instance()` | `Address` | Contract admin authorized to update admin-controlled settings. |
| `BorrowMinAmount` | `instance()` | `i128` | Minimum borrow amount enforced by `borrow`. |
| `FlashActive` | `instance()` | `bool` | Reentrancy guard set during flash-loan callbacks. |
| `FlashFeeBps` | `instance()` | `i128` | Flash-loan fee in basis points. |
| `Collateral(Address)` | `persistent()` | `i128` | Per-user collateral balance. |
| `Debt(Address)` | `persistent()` | `DebtPosition` | Per-user debt principal and last accrual timestamp. |
| `Balance(Address, Address)` | `persistent()` | `i128` | Per-asset balance tracked for a specific user or callback invoker. |
| `Treasury(Address)` | `persistent()` | `i128` | Per-asset treasury liquidity available for flash loans. |

Notes:
- `Address` payload order for `Balance(asset, user)` is asset first, user second.
- New lending keys must be appended to `DataKey`; never reuse an existing variant for a different value type.

### 2. Cross-Asset Core (`cross_asset.rs`)

| Key (Symbol/Type) | Value Type | Description |
|-------------------|------------|-------------|
| `admin` | `Address` | Protocol admin address authorized to manage assets. |
| `configs` | `Map<AssetKey, AssetConfig>` | Configuration for each supported asset (factors, caps, prices). |
| `positions` | `Map<UserAssetKey, AssetPosition>` | Per-user, per-asset collateral and debt balances. |
| `supplies` | `Map<AssetKey, i128>` | Total supply (deposits) for each asset. |
| `borrows` | `Map<AssetKey, i128>` | Total borrows (debt) for each asset. |
| `assets` | `Vec<AssetKey>` | List of all registered assets in the protocol. |

### 3. Risk Management (`risk_management.rs`)

| Key (`RiskDataKey`) | Value Type | Description |
|---------------------|------------|-------------|
| `RiskConfig` | `RiskConfig` | Global risk parameters (MCR, liquidation threshold, close factor). |
| `Admin` | `Address` | Admin address for risk management operations. |
| `EmergencyPause` | `bool` | Global flag to halt all protocol operations. |

### 4. Deposit Module (`deposit.rs`)

| Key (`DepositDataKey`) | Value Type | Description |
|------------------------|------------|-------------|
| `CollateralBalance(Address)` | `i128` | Per-user cumulative collateral balance (deprecated in favor of `cross_asset` positions). |
| `AssetParams(Address)` | `AssetParams` | Legacy asset parameters. |
| `Position(Address)` | `Position` | User's unified position (legacy module). |
| `ProtocolAnalytics` | `ProtocolAnalytics` | Aggregate protocol metrics (deposits, borrows, TVL). |
| `UserAnalytics(Address)` | `UserAnalytics` | Detailed per-user activity and risk metrics. |

### 5. Interest Rate Module (`interest_rate.rs`)

| Key (`InterestRateDataKey`) | Value Type | Description |
|-----------------------------|------------|-------------|
| `InterestRateConfig` | `InterestRateConfig` | Kink-based model parameters (base rate, kink, multipliers). |
| `Admin` | `Address` | Admin address for interest rate adjustments. |

### 6. Oracle Module (`oracle.rs`)

| Key (`OracleDataKey`) | Value Type | Description |
|-----------------------|------------|-------------|
| `PriceFeed(Address)` | `PriceFeed` | Latest price, timestamp, and provider for an asset. |
| `FallbackOracle(Address)` | `Address` | Designated fallback price provider for an asset. |
| `PriceCache(Address)` | `CachedPrice` | TTL-bounded price cache for gas efficiency. |
| `OracleConfig` | `OracleConfig` | Global oracle safety parameters (deviation, staleness). |

### 7. Flash Loan Module (`flash_loan.rs`)

| Key (`FlashLoanDataKey`) | Value Type | Description |
|--------------------------|------------|-------------|
| `FlashLoanConfig` | `FlashLoanConfig` | Fee basis points and amount limits. |
| `ActiveFlashLoan(Addr, Addr)` | `FlashLoanRecord` | Reentrancy guard and transient loan record. |

### 8. Analytics Module (`analytics.rs`)

| Key (`AnalyticsDataKey`) | Value Type | Description |
|--------------------------|------------|-------------|
| `ProtocolMetrics` | `ProtocolMetrics` | Cached protocol-wide stats snapshot. |
| `UserMetrics(Address)` | `UserMetrics` | Cached per-user stats snapshot. |
| `ActivityLog` | `Vec<ActivityEntry>` | Global activity history (max 10,000 entries). |
| `TotalUsers` | `u64` | Total number of unique users. |
| `TotalTransactions` | `u64` | Global transaction counter. |

---

## Type Definitions

### Core Structs

#### `AssetPosition`
```rust
pub struct AssetPosition {
    pub collateral: i128,        // Asset's native units
    pub debt_principal: i128,    // Principal borrowed
    pub accrued_interest: i128,  // Accumulated interest
    pub last_updated: u64,       // Timestamp of last update
}
```

#### `RiskConfig`
```rust
pub struct RiskConfig {
    pub min_collateral_ratio: i128,  // Basis points (11000 = 110%)
    pub liquidation_threshold: i128, // Basis points
    pub close_factor: i128,          // Basis points
    pub liquidation_incentive: i128, // Basis points
    pub pause_switches: Map<Symbol, bool>,
    pub last_update: u64,
}
```

---

## Upgrade and Migration Strategy

### Wasm Upgrades
Soroban supports contract upgrades via `env.deployer().update_current_contract_wasm(new_wasm_hash)`. This replaces the contract code while preserving existing storage.

### Compatibility Guidelines
1.  **Append Only**: Always add new variants to the end of `contracttype` enums to preserve discriminant mapping.
2.  **Structural Stability**: Avoid deleting or reordering fields in structs. If a field is deprecated, keep it but ignore its value.
3.  **Key Consistency**: Ensure that `contracttype` definitions used for storage keys are identical across versions.

### Data Migration Patterns
If a storage layout change is unavoidable (e.g., merging two maps into one), follow this process:
1.  **Deployment**: Deploy the new contract code.
2.  **Migration Transaction**: Execute a one-time admin function that reads old data, transforms it, and writes it to new keys.
3.  **Cleanup**: Remove the old keys to reclaim rent/storage costs.
4.  **Verification**: Execute a test suite against the migrated state.

---

## Security Assumptions and Validation

- **No Overwrites**: Storage keys are designed to be unique. Map-based keys use composite structures like `UserAssetKey(Address, AssetKey)` to prevent users from affecting each other's data.
- **Persistent Only**: All critical protocol state is stored in `persistent()` storage to prevent expiration (subject to rent payments).
- **Admin Isolation**: Admin addresses are stored in module-specific keys, allowing for granular permission management or a unified global admin.

### Validation Checklist
- [ ] All `contracttype` enums have unique variants.
- [ ] No `temporary()` or `instance()` storage is used for critical state.
- [ ] `AssetKey` correctly handles both Native (XLM) and Token assets.
- [ ] Key collisions between modules are avoided by using unique Enum types for keys.

---

## Migration Checklist — User Position Preservation

When introducing a new storage field or key (a "layout addition"), follow this
checklist to guarantee user positions (collateral, debt, rates, timestamps)
survive the upgrade unchanged. The safety tests in
`stellar-lend/contracts/lending/src/upgrade_migration_safety_test.rs` enforce
the same invariants programmatically.

### Pre-upgrade

- [ ] **Snapshot rich fixture**: confirm seed data covers multiple users and
  multiple assets, with collateral, debt, rate, and timestamp fields populated.
- [ ] **Backup**: call `data_backup` and store the snapshot name. The
  `test_view_consistency_after_upgrade` test models this flow.
- [ ] **Schema version recorded**: capture `data_schema_version()` for use as
  the strict-greater-than check in the new bump.

### During the upgrade

- [ ] **Append-only**: new storage keys MUST live under fresh, non-overlapping
  namespaces. Never reuse a legacy key for a different value type. The
  `test_new_storage_fields_coexist_with_preserved_positions` test asserts the
  new keys never alias the old ones.
- [ ] **No in-place rewrites of legacy entries**: the migration may *read*
  legacy entries to derive new ones, but must never overwrite them with a
  different encoding during the same migration.
- [ ] **Bump schema version**: call `data_migrate_bump_version` with the new
  version and a memo describing the layout addition.

### Post-upgrade verification

- [ ] **Per-entry round-trip**: every legacy `(key, value)` pair must read back
  identically. `test_positions_preserved_across_upgrade_layout_addition` and
  `test_position_decoding_after_upgrade_round_trip` pin this at both the
  byte-level and the decoded-field level.
- [ ] **Aggregate count**: `data_entry_count()` for legacy keys must remain
  unchanged; the count for new keys must equal exactly what the migration
  wrote.
- [ ] **Sequential safety**: if multiple migrations are chained, each step
  must independently preserve all preceding entries. See
  `test_positions_preserved_across_sequential_layout_additions`.
- [ ] **Rollback semantics documented**: storage writes are not transactional
  with upgrade execution. Document any keys the migration wrote so operators
  understand they will persist even if the upgrade is rolled back. See
  `test_migration_preserves_positions_under_rollback`.

### Security notes

- A migration that silently mutates or drops user positions can socialise
  losses across the borrower set. Treat any test failure in
  `upgrade_migration_safety_test.rs` as a release-blocker.
- New storage namespaces must not collide with legacy namespaces by symbol or
  by enum discriminant. Add a regression test alongside any new storage key.
