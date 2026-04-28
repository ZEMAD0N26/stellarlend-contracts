# Timelock Governance Operations Guide

## Overview

This document provides operational guidance for using the timelock governance system in StellarLend. The timelock introduces delayed execution for high-risk parameter changes while maintaining emergency response capabilities.

## Quick Reference

### Operation Classifications

| Risk Level | Delay Required | Examples |
|------------|----------------|----------|
| **Immediate** | 0 seconds | `get_admin`, `get_price`, view functions |
| **High Risk** | 7 days | `set_liquidation_threshold_bps`, `set_pause`, `set_guardian` |
| **Critical** | 14 days | `set_oracle`, `upgrade_execute`, `complete_recovery` |

### Key Addresses

- **Admin**: Can queue/execute all operations, bound by delay rules
- **Guardian**: Can execute emergency operations immediately (`emergency_shutdown`, `set_pause`)
- **Timelock Contract**: Acts as the admin for the lending contract

## Common Operations

### 1. Standard Parameter Changes (7-day delay)

**Example: Updating Liquidation Threshold**

```bash
# Step 1: Queue the change
stellar contract invoke \
  --id $TIMELOCK_CONTRACT \
  --source $ADMIN_KEY \
  --network testnet \
  -- queue \
  --caller $ADMIN_ADDRESS \
  --target $LENDING_CONTRACT \
  --func "set_liquidation_threshold_bps" \
  --args '["8000"]' \
  --eta $(($(date +%s) + 604800))  # 7 days from now

# Step 2: Wait 7 days and monitor community feedback

# Step 3: Execute the change
stellar contract invoke \
  --id $TIMELOCK_CONTRACT \
  --source $ADMIN_KEY \
  --network testnet \
  -- execute \
  --caller $ADMIN_ADDRESS \
  --target $LENDING_CONTRACT \
  --func "set_liquidation_threshold_bps" \
  --args '["8000"]' \
  --eta $ETA_FROM_STEP1
```

### 2. Critical Operations (14-day delay)

**Example: Oracle Update**

```bash
# Step 1: Queue the oracle change (requires 14-day delay)
stellar contract invoke \
  --id $TIMELOCK_CONTRACT \
  --source $ADMIN_KEY \
  --network testnet \
  -- queue \
  --caller $ADMIN_ADDRESS \
  --target $LENDING_CONTRACT \
  --func "set_oracle" \
  --args '["$NEW_ORACLE_ADDRESS"]' \
  --eta $(($(date +%s) + 1209600))  # 14 days from now

# Step 2: Wait 14 days with extended community review

# Step 3: Execute after delay
stellar contract invoke \
  --id $TIMELOCK_CONTRACT \
  --source $ADMIN_KEY \
  --network testnet \
  -- execute \
  --caller $ADMIN_ADDRESS \
  --target $LENDING_CONTRACT \
  --func "set_oracle" \
  --args '["$NEW_ORACLE_ADDRESS"]' \
  --eta $ETA_FROM_STEP1
```

### 3. Immediate Operations

**Example: Reading Protocol State**

```bash
# Can be executed immediately without queueing
stellar contract invoke \
  --id $TIMELOCK_CONTRACT \
  --source $ADMIN_KEY \
  --network testnet \
  -- execute_immediate \
  --caller $ADMIN_ADDRESS \
  --target $LENDING_CONTRACT \
  --func "get_admin" \
  --args '[]'
```

### 4. Emergency Operations

**Example: Guardian Emergency Shutdown**

```bash
# Guardian can execute immediately
stellar contract invoke \
  --id $TIMELOCK_CONTRACT \
  --source $GUARDIAN_KEY \
  --network testnet \
  -- execute_immediate \
  --caller $GUARDIAN_ADDRESS \
  --target $LENDING_CONTRACT \
  --func "emergency_shutdown" \
  --args '[]'
```

## Emergency Response Procedures

### 1. Immediate Threat Response

When an immediate threat is detected:

1. **Guardian Action**: Trigger emergency shutdown
   ```bash
   stellar contract invoke --id $TIMELOCK_CONTRACT --source $GUARDIAN_KEY \
     -- execute_immediate --caller $GUARDIAN_ADDRESS \
     --target $LENDING_CONTRACT --func "emergency_shutdown" --args '[]'
   ```

2. **Verify State**: Check emergency state
   ```bash
   stellar contract invoke --id $TIMELOCK_CONTRACT \
     -- get_emergency_state
   ```

### 2. Recovery Process

After threat mitigation:

1. **Start Recovery** (Admin only):
   ```bash
   stellar contract invoke --id $TIMELOCK_CONTRACT --source $ADMIN_KEY \
     -- start_recovery --caller $ADMIN_ADDRESS
   ```

2. **Allow User Withdrawals**: During recovery, users can repay debts and withdraw collateral

3. **Complete Recovery** (14-day delay required):
   ```bash
   # Queue recovery completion
   stellar contract invoke --id $TIMELOCK_CONTRACT --source $ADMIN_KEY \
     -- queue --caller $ADMIN_ADDRESS --target $LENDING_CONTRACT \
     --func "complete_recovery" --args '[]' \
     --eta $(($(date +%s) + 1209600))
   
   # Execute after 14 days
   stellar contract invoke --id $TIMELOCK_CONTRACT --source $ADMIN_KEY \
     -- execute --caller $ADMIN_ADDRESS --target $LENDING_CONTRACT \
     --func "complete_recovery" --args '[]' --eta $ETA
   ```

## Monitoring and Alerting

### Event Monitoring

Set up monitoring for these critical events:

```javascript
// Monitor queued actions
contract.events.filter({
  topics: ["timelock", "queue"]
}).on('data', (event) => {
  console.log('Action queued:', event.data);
  // Alert community about pending change
});

// Monitor executions
contract.events.filter({
  topics: ["timelock", "execute"]
}).on('data', (event) => {
  console.log('Action executed:', event.data);
  // Log successful execution
});

// Monitor emergency events
contract.events.filter({
  topics: ["timelock", "emergency_shutdown"]
}).on('data', (event) => {
  console.log('EMERGENCY: Protocol shutdown triggered');
  // Send immediate alerts
});
```

### Community Notification

For all queued actions:

1. **Immediate Notification**: Post to governance forum/Discord
2. **Technical Details**: Include function name, parameters, execution time
3. **Impact Assessment**: Explain what the change does and why
4. **Objection Period**: Provide clear process for community feedback

## Security Best Practices

### Admin Key Management

1. **Multi-signature**: Use multi-sig wallet for admin operations
2. **Cold Storage**: Keep admin keys in hardware wallets
3. **Rotation**: Regularly rotate admin keys
4. **Backup**: Maintain secure backup procedures

### Guardian Key Management

1. **Hot Wallet**: Guardian keys should be readily accessible for emergencies
2. **Monitoring**: 24/7 monitoring for threat detection
3. **Response Time**: Aim for <1 hour emergency response
4. **Limited Scope**: Guardian can only execute emergency operations

### Operational Security

1. **Verification**: Always verify queued actions before execution
2. **Community Review**: Allow full delay period for community input
3. **Cancellation**: Be prepared to cancel malicious or erroneous actions
4. **Documentation**: Document all parameter changes and rationale

## Troubleshooting

### Common Errors

**`DelayTooShort`**: Operation requires longer delay
- Solution: Use correct delay for operation risk level
- High-risk: 7 days minimum
- Critical: 14 days minimum

**`ActionNotQueued`**: Trying to execute non-existent action
- Solution: Verify action was queued successfully
- Check action ID matches exactly

**`TimelockNotReady`**: Trying to execute before delay expires
- Solution: Wait until ETA timestamp has passed

**`TimelockExpired`**: Action expired after grace period
- Solution: Re-queue the action with new ETA

**`EmergencyActive`**: Non-emergency operation during emergency state
- Solution: Complete recovery process first, or use emergency-allowed operations only

**`NotGuardian`**: Guardian trying to execute non-emergency operation
- Solution: Use admin account, or limit to emergency operations

### Recovery Scenarios

**Compromised Admin Key**:
1. Guardian triggers emergency shutdown immediately
2. Deploy new timelock with new admin key
3. Update lending contract admin to new timelock
4. Resume operations with new governance structure

**Lost Guardian Key**:
1. Admin can still manage all operations (with delays)
2. Set new guardian address via standard governance process
3. Emergency response capability restored

**Malicious Queued Action**:
1. Admin cancels the queued action immediately
2. Investigate how malicious action was queued
3. Implement additional security measures
4. Consider emergency shutdown if compromise suspected

## Integration Checklist

### Pre-deployment

- [ ] Deploy timelock contract with correct parameters
- [ ] Set guardian address
- [ ] Configure governance delays (7 days default, 14 days critical)
- [ ] Test all operation classifications
- [ ] Verify emergency procedures

### Post-deployment

- [ ] Update lending contract admin to timelock address
- [ ] Test parameter change flow end-to-end
- [ ] Verify emergency shutdown works
- [ ] Set up event monitoring
- [ ] Train operations team on procedures
- [ ] Document all addresses and keys

### Ongoing Operations

- [ ] Monitor queued actions daily
- [ ] Review community feedback on proposals
- [ ] Maintain guardian key accessibility
- [ ] Regular security audits of procedures
- [ ] Update documentation as needed

## Contact Information

For operational questions or emergency situations:

- **Technical Issues**: [Technical Support Channel]
- **Security Incidents**: [Security Team Contact]
- **Governance Questions**: [Governance Forum]
- **Emergency Contact**: [24/7 Emergency Line]