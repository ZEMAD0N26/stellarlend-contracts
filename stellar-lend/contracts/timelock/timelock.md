# Soroban Timelock Module

## Overview

The `soroban-timelock` crate implements a secure, delayed execution pattern for privileged administrative actions in the StellarLend protocol. It acts as a safety buffer between the proposal of an operational change and its actual implementation, with intelligent governance policies that classify operations by risk level.

## Governance Integration

The timelock now includes sophisticated governance policies that automatically classify operations and enforce appropriate delays:

### Operation Risk Classification

**Immediate Operations** (no delay required):
- Read-only operations: `get_admin`, `get_guardian`, `get_emergency_state`, `get_pause_state`
- View functions: `get_price`, `get_health_factor`, `get_user_position`, `get_collateral_balance`
- Data queries: `data_load`, `data_key_exists`, `data_schema_version`, `current_version`

**High-Risk Operations** (7-day default delay):
- Risk parameter changes: `set_liquidation_threshold_bps`, `set_close_factor_bps`, `set_liquidation_incentive_bps`
- Protocol settings: `set_flash_loan_fee_bps`, `set_pause`, `set_guardian`
- Asset configuration: `set_asset_params`, `initialize_deposit_settings`, `initialize_withdraw_settings`
- Governance setup: `upgrade_init`, `upgrade_propose`, `data_store_init`

**Critical Operations** (14-day extended delay):
- Oracle configuration: `set_oracle`, `configure_oracle`, `set_primary_oracle`, `set_fallback_oracle`
- Contract upgrades: `upgrade_execute`
- Recovery completion: `complete_recovery`
- Data migration: `data_migrate_bump_version`

### Emergency Bypass System

**Guardian Powers:**
- Can execute `emergency_shutdown` and `set_pause` immediately (bypass timelock)
- Limited to emergency operations only
- Cannot execute routine admin operations

**Emergency State Management:**
- Normal → Shutdown (guardian or admin)
- Shutdown → Recovery (admin only)
- Recovery → Normal (admin only)

During emergency states, only essential operations are allowed:
- Emergency: `emergency_shutdown`, `start_recovery`, `complete_recovery`
- Recovery: `repay`, `withdraw`, `liquidate`, view functions

## Security Context

### Trust Boundaries

**Admin**: 
- Authorized for queueing and executing all operations
- Bound by governance delay rules (cannot bypass except for immediate operations)
- Must actively monitor queued actions during delay periods
- Can cancel queued actions before execution

**Guardian**: 
- Limited emergency powers only
- Can trigger immediate `emergency_shutdown` and `set_pause`
- Cannot queue or execute routine admin operations
- Designed for fast-response emergency scenarios

**Grace Period**: 
- Prevents forgotten actions from becoming permanently valid
- Actions expire after `grace_period` if not executed
- Provides bounded execution window after delay completion

### Governance Policy Enforcement

The timelock automatically enforces appropriate delays based on operation risk:
- Prevents immediate execution of high-risk operations
- Requires extended delays for critical infrastructure changes
- Allows immediate execution only for safe operations or emergency bypasses
- Validates all delays meet minimum governance requirements

## Integration

### Initial Setup

1. Deploy and initialize `soroban-timelock` with governance parameters:
   ```rust
   timelock.initialize(
       admin_address,
       min_delay: 24 * 3600,        // 1 day minimum
       grace_period: 7 * 24 * 3600, // 7 day execution window
       default_delay: 7 * 24 * 3600, // 7 days for high-risk ops
       critical_delay: 14 * 24 * 3600 // 14 days for critical ops
   );
   ```

2. Set the guardian for emergency operations:
   ```rust
   timelock.set_guardian(admin, guardian_address);
   ```

3. Configure the lending contract to use timelock as admin:
   ```rust
   lending.set_admin(timelock_address);
   ```

### Operation Flow

**For High-Risk/Critical Operations:**
1. **Queue**: `queue(target_addr, function_symbol, args, eta)` 
   - Validates delay meets governance requirements
   - Generates action ID and stores in persistent storage
   - Emits `(timelock, queue)` event for monitoring

2. **Wait**: Community monitoring period during delay
   - Off-chain systems can inspect queued actions
   - Admin can cancel if issues discovered
   - Automatic expiration after grace period

3. **Execute**: `execute(target_addr, function_symbol, args, eta)`
   - Validates time bounds and authorization
   - Removes action from storage (reentrancy protection)
   - Invokes target contract function
   - Emits `(timelock, execute)` event

**For Immediate Operations:**
1. **Execute**: `execute_immediate(target_addr, function_symbol, args)`
   - Validates operation is classified as immediate
   - Checks emergency state restrictions
   - Invokes target contract immediately
   - Emits `(timelock, execute_immediate)` event

**For Emergency Operations:**
1. **Guardian Bypass**: Guardian can execute `emergency_shutdown` and `set_pause` immediately
2. **Emergency State**: Restricts available operations during crisis
3. **Recovery Flow**: Admin-controlled transition back to normal operations

## Flow Examples

### Standard Parameter Change
```rust
// Queue a liquidation threshold change (7-day delay)
let eta = env.ledger().timestamp() + (7 * 24 * 3600);
let action_id = timelock.queue(
    admin,
    lending_contract,
    Symbol::new(&env, "set_liquidation_threshold_bps"),
    vec![&env, 8000i128.into_val(&env)], // 80%
    eta
);

// Wait 7 days...

// Execute the change
timelock.execute(
    admin,
    lending_contract,
    Symbol::new(&env, "set_liquidation_threshold_bps"),
    vec![&env, 8000i128.into_val(&env)],
    eta
);
```

### Critical Oracle Update
```rust
// Queue oracle change (14-day delay required)
let eta = env.ledger().timestamp() + (14 * 24 * 3600);
let action_id = timelock.queue(
    admin,
    lending_contract,
    Symbol::new(&env, "set_oracle"),
    vec![&env, new_oracle_address.into_val(&env)],
    eta
);

// Wait 14 days...

// Execute the change
timelock.execute(admin, lending_contract, /* ... */);
```

### Emergency Response
```rust
// Guardian triggers immediate shutdown
timelock.execute_immediate(
    guardian,
    lending_contract,
    Symbol::new(&env, "emergency_shutdown"),
    vec![]
);

// Admin starts recovery process
timelock.start_recovery(admin);

// Admin completes recovery (requires 14-day delay)
let eta = env.ledger().timestamp() + (14 * 24 * 3600);
timelock.queue(
    admin,
    lending_contract,
    Symbol::new(&env, "complete_recovery"),
    vec![],
    eta
);
```

## Events

The timelock emits structured events for off-chain monitoring:

- `(timelock, queue)` → `action_id`: Action queued for delayed execution
- `(timelock, execute)` → `action_id`: Queued action executed successfully  
- `(timelock, execute_immediate)` → `function_symbol`: Immediate operation executed
- `(timelock, cancel)` → `action_id`: Queued action cancelled
- `(timelock, emergency_shutdown)` → `caller`: Emergency shutdown triggered
- `(timelock, start_recovery)` → `caller`: Recovery mode initiated
- `(timelock, complete_recovery)` → `caller`: Normal operations restored

## Security Considerations

### Reduced Admin Key Compromise Impact

The timelock significantly reduces the impact of admin key compromise:

1. **Delayed Execution**: Attackers cannot immediately execute critical changes
2. **Community Monitoring**: 7-14 day delays allow community to detect malicious actions
3. **Cancellation Window**: Legitimate admin can cancel malicious queued actions
4. **Guardian Separation**: Emergency powers separated from routine admin powers
5. **Operation Classification**: Automatic risk assessment prevents bypass attempts

### Operational Security

1. **Monitor Queued Actions**: Set up off-chain monitoring for queue events
2. **Guardian Key Security**: Keep guardian keys in fast-access but secure storage
3. **Admin Key Security**: Admin keys can be in slower but more secure storage
4. **Community Alerts**: Notify community of all queued critical operations
5. **Cancellation Procedures**: Have clear procedures for cancelling malicious actions

### Attack Mitigation

- **Immediate Compromise Response**: Guardian can trigger emergency shutdown
- **Malicious Queue Detection**: Community has 7-14 days to detect bad actions
- **Reentrancy Protection**: Actions removed from storage before execution
- **Time Validation**: Strict ETA and grace period enforcement
- **Authorization Checks**: Multi-layer permission validation
