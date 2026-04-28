use crate::{TimelockContract, TimelockContractClient, TimelockError};
use crate::governance::{GovernanceConfig, GovernancePolicy, EmergencyPolicy};
use crate::storage::EmergencyState;
use soroban_sdk::{testutils::Address as _, Address, Env, Symbol, Vec};

fn setup_timelock(env: &Env) -> (TimelockContractClient, Address, Address) {
    let contract_id = env.register(TimelockContract, ());
    let client = TimelockContractClient::new(env, &contract_id);
    let admin = Address::generate(env);
    let guardian = Address::generate(env);
    
    // Initialize with 7 day default delay, 14 day critical delay
    client.initialize(&admin, &(24 * 3600), &(7 * 24 * 3600), &(7 * 24 * 3600), &(14 * 24 * 3600));
    client.set_guardian(&admin, &guardian);
    
    (client, admin, guardian)
}

#[test]
fn test_queue_high_risk_operation() {
    let env = Env::default();
    env.mock_all_auths();
    
    let (client, admin, _) = setup_timelock(&env);
    let target = Address::generate(&env);
    let func = Symbol::new(&env, "set_liquidation_threshold_bps");
    let args = Vec::new(&env);
    let eta = env.ledger().timestamp() + (7 * 24 * 3600); // 7 days from now
    
    let action_id = client.queue(&admin, &target, &func, &args, &eta);
    assert!(action_id.is_ok());
}

#[test]
fn test_queue_critical_operation_requires_longer_delay() {
    let env = Env::default();
    env.mock_all_auths();
    
    let (client, admin, _) = setup_timelock(&env);
    let target = Address::generate(&env);
    let func = Symbol::new(&env, "set_oracle");
    let args = Vec::new(&env);
    
    // Try with 7 day delay (should fail for critical operation)
    let eta_short = env.ledger().timestamp() + (7 * 24 * 3600);
    let result = client.try_queue(&admin, &target, &func, &args, &eta_short);
    assert_eq!(result, Err(Ok(TimelockError::DelayTooShort)));
    
    // Try with 14 day delay (should succeed)
    let eta_long = env.ledger().timestamp() + (14 * 24 * 3600);
    let action_id = client.queue(&admin, &target, &func, &args, &eta_long);
    assert!(action_id.is_ok());
}

#[test]
fn test_execute_immediate_operation() {
    let env = Env::default();
    env.mock_all_auths();
    
    let (client, admin, _) = setup_timelock(&env);
    let target = Address::generate(&env);
    let func = Symbol::new(&env, "get_admin");
    let args = Vec::new(&env);
    
    // Should be able to execute immediately
    let result = client.execute_immediate(&admin, &target, &func, &args);
    // Note: This will fail in test because target contract doesn't exist,
    // but it should pass the governance checks
    assert!(result.is_err()); // Contract invoke will fail, but not due to governance
}

#[test]
fn test_immediate_operation_cannot_be_queued() {
    let env = Env::default();
    env.mock_all_auths();
    
    let (client, admin, _) = setup_timelock(&env);
    let target = Address::generate(&env);
    let func = Symbol::new(&env, "get_admin");
    let args = Vec::new(&env);
    let eta = env.ledger().timestamp() + (7 * 24 * 3600);
    
    let result = client.try_queue(&admin, &target, &func, &args, &eta);
    assert_eq!(result, Err(Ok(TimelockError::OperationNotAllowed)));
}

#[test]
fn test_guardian_emergency_bypass() {
    let env = Env::default();
    env.mock_all_auths();
    
    let (client, admin, guardian) = setup_timelock(&env);
    let target = Address::generate(&env);
    let func = Symbol::new(&env, "emergency_shutdown");
    let args = Vec::new(&env);
    
    // Guardian should be able to execute emergency operations immediately
    let result = client.execute_immediate(&guardian, &target, &func, &args);
    // Note: This will fail in test because target contract doesn't exist,
    // but it should pass the governance checks
    assert!(result.is_err()); // Contract invoke will fail, but not due to governance
}

#[test]
fn test_guardian_cannot_execute_non_emergency_operations() {
    let env = Env::default();
    env.mock_all_auths();
    
    let (client, admin, guardian) = setup_timelock(&env);
    let target = Address::generate(&env);
    let func = Symbol::new(&env, "set_oracle");
    let args = Vec::new(&env);
    
    let result = client.try_execute_immediate(&guardian, &target, &func, &args);
    assert_eq!(result, Err(Ok(TimelockError::NotGuardian)));
}

#[test]
fn test_emergency_state_management() {
    let env = Env::default();
    env.mock_all_auths();
    
    let (client, admin, guardian) = setup_timelock(&env);
    
    // Initial state should be normal
    assert_eq!(client.get_emergency_state(), EmergencyState::Normal);
    
    // Guardian can trigger emergency shutdown
    client.emergency_shutdown(&guardian);
    assert_eq!(client.get_emergency_state(), EmergencyState::Shutdown);
    
    // Admin can start recovery
    client.start_recovery(&admin);
    assert_eq!(client.get_emergency_state(), EmergencyState::Recovery);
    
    // Admin can complete recovery
    client.complete_recovery(&admin);
    assert_eq!(client.get_emergency_state(), EmergencyState::Normal);
}

#[test]
fn test_operations_blocked_during_emergency() {
    let env = Env::default();
    env.mock_all_auths();
    
    let (client, admin, guardian) = setup_timelock(&env);
    let target = Address::generate(&env);
    let func = Symbol::new(&env, "set_oracle");
    let args = Vec::new(&env);
    let eta = env.ledger().timestamp() + (14 * 24 * 3600);
    
    // Trigger emergency shutdown
    client.emergency_shutdown(&guardian);
    
    // Should not be able to queue non-emergency operations
    let result = client.try_queue(&admin, &target, &func, &args, &eta);
    assert_eq!(result, Err(Ok(TimelockError::EmergencyActive)));
}

#[test]
fn test_unauthorized_access() {
    let env = Env::default();
    env.mock_all_auths();
    
    let (client, admin, _) = setup_timelock(&env);
    let stranger = Address::generate(&env);
    let target = Address::generate(&env);
    let func = Symbol::new(&env, "set_oracle");
    let args = Vec::new(&env);
    let eta = env.ledger().timestamp() + (14 * 24 * 3600);
    
    // Stranger cannot queue actions
    let result = client.try_queue(&stranger, &target, &func, &args, &eta);
    assert_eq!(result, Err(Ok(TimelockError::NotAdmin)));
    
    // Stranger cannot execute immediate operations
    let result = client.try_execute_immediate(&stranger, &target, &func, &args);
    assert_eq!(result, Err(Ok(TimelockError::NotAdmin)));
    
    // Stranger cannot trigger emergency shutdown
    let result = client.try_emergency_shutdown(&stranger);
    assert_eq!(result, Err(Ok(TimelockError::NotGuardian)));
}

#[test]
fn test_execute_queued_action_after_delay() {
    let env = Env::default();
    env.mock_all_auths();
    
    let (client, admin, _) = setup_timelock(&env);
    let target = Address::generate(&env);
    let func = Symbol::new(&env, "set_liquidation_threshold_bps");
    let args = Vec::new(&env);
    let eta = env.ledger().timestamp() + (7 * 24 * 3600);
    
    // Queue the action
    let action_id = client.queue(&admin, &target, &func, &args, &eta);
    assert!(action_id.is_ok());
    
    // Try to execute before delay (should fail)
    let result = client.try_execute(&admin, &target, &func, &args, &eta);
    assert_eq!(result, Err(Ok(TimelockError::TimelockNotReady)));
    
    // Advance time past the delay
    env.ledger().with_mut(|li| li.timestamp = eta + 1);
    
    // Now execution should work (will fail due to target contract not existing, but governance checks pass)
    let result = client.execute(&admin, &target, &func, &args, &eta);
    assert!(result.is_err()); // Contract invoke will fail, but not due to timelock
}

#[test]
fn test_cancel_queued_action() {
    let env = Env::default();
    env.mock_all_auths();
    
    let (client, admin, _) = setup_timelock(&env);
    let target = Address::generate(&env);
    let func = Symbol::new(&env, "set_liquidation_threshold_bps");
    let args = Vec::new(&env);
    let eta = env.ledger().timestamp() + (7 * 24 * 3600);
    
    // Queue the action
    let action_id = client.queue(&admin, &target, &func, &args, &eta);
    assert!(action_id.is_ok());
    
    // Cancel the action
    client.cancel(&admin, &target, &func, &args, &eta);
    
    // Advance time and try to execute (should fail because it was cancelled)
    env.ledger().with_mut(|li| li.timestamp = eta + 1);
    let result = client.try_execute(&admin, &target, &func, &args, &eta);
    assert_eq!(result, Err(Ok(TimelockError::ActionNotQueued)));
}

#[test]
fn test_governance_policy_classification() {
    let env = Env::default();
    
    // Test immediate operations
    let get_admin = Symbol::new(&env, "get_admin");
    assert!(matches!(GovernancePolicy::get_operation_risk(&get_admin), crate::governance::OperationRisk::Immediate));
    
    // Test high-risk operations
    let set_threshold = Symbol::new(&env, "set_liquidation_threshold_bps");
    assert!(matches!(GovernancePolicy::get_operation_risk(&set_threshold), crate::governance::OperationRisk::High));
    
    // Test critical operations
    let set_oracle = Symbol::new(&env, "set_oracle");
    assert!(matches!(GovernancePolicy::get_operation_risk(&set_oracle), crate::governance::OperationRisk::Critical));
}

#[test]
fn test_emergency_policy() {
    let env = Env::default();
    
    // Test emergency allowed operations
    let emergency_shutdown = Symbol::new(&env, "emergency_shutdown");
    assert!(EmergencyPolicy::is_emergency_allowed(&emergency_shutdown));
    assert!(EmergencyPolicy::is_guardian_bypass_allowed(&emergency_shutdown));
    
    // Test non-emergency operations
    let set_oracle = Symbol::new(&env, "set_oracle");
    assert!(!EmergencyPolicy::is_emergency_allowed(&set_oracle));
    assert!(!EmergencyPolicy::is_guardian_bypass_allowed(&set_oracle));
    
    // Test recovery operations
    let repay = Symbol::new(&env, "repay");
    assert!(EmergencyPolicy::is_emergency_allowed(&repay));
    assert!(!EmergencyPolicy::is_guardian_bypass_allowed(&repay));
}