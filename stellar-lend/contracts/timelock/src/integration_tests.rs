use crate::{TimelockContract, TimelockContractClient, TimelockError};
use soroban_sdk::{testutils::Address as _, Address, Env, Symbol, Vec};

// Mock lending contract for integration testing
use soroban_sdk::{contract, contractimpl};

#[contract]
pub struct MockLendingContract;

#[contractimpl]
impl MockLendingContract {
    pub fn initialize(env: Env, admin: Address) {
        env.storage().instance().set(&Symbol::new(&env, "admin"), &admin);
    }
    
    pub fn set_liquidation_threshold_bps(env: Env, caller: Address, bps: i128) -> Result<(), u32> {
        caller.require_auth();
        let admin: Address = env.storage().instance().get(&Symbol::new(&env, "admin")).unwrap();
        if caller != admin {
            return Err(1); // Unauthorized
        }
        env.storage().instance().set(&Symbol::new(&env, "liquidation_threshold_bps"), &bps);
        Ok(())
    }
    
    pub fn get_liquidation_threshold_bps(env: Env) -> i128 {
        env.storage().instance().get(&Symbol::new(&env, "liquidation_threshold_bps")).unwrap_or(7500)
    }
    
    pub fn set_oracle(env: Env, caller: Address, oracle: Address) -> Result<(), u32> {
        caller.require_auth();
        let admin: Address = env.storage().instance().get(&Symbol::new(&env, "admin")).unwrap();
        if caller != admin {
            return Err(1); // Unauthorized
        }
        env.storage().instance().set(&Symbol::new(&env, "oracle"), &oracle);
        Ok(())
    }
    
    pub fn get_oracle(env: Env) -> Option<Address> {
        env.storage().instance().get(&Symbol::new(&env, "oracle"))
    }
    
    pub fn emergency_shutdown(env: Env, caller: Address) -> Result<(), u32> {
        caller.require_auth();
        env.storage().instance().set(&Symbol::new(&env, "emergency_state"), &true);
        Ok(())
    }
    
    pub fn get_admin(env: Env) -> Address {
        env.storage().instance().get(&Symbol::new(&env, "admin")).unwrap()
    }
}

type MockLendingContractClient<'a> = soroban_sdk::contractclient::Client<'a, MockLendingContract>;

fn setup_integration_test(env: &Env) -> (TimelockContractClient, MockLendingContractClient, Address, Address) {
    // Deploy timelock
    let timelock_id = env.register(TimelockContract, ());
    let timelock_client = TimelockContractClient::new(env, &timelock_id);
    
    // Deploy mock lending contract
    let lending_id = env.register(MockLendingContract, ());
    let lending_client = MockLendingContractClient::new(env, &lending_id);
    
    let admin = Address::generate(env);
    let guardian = Address::generate(env);
    
    // Initialize timelock with governance parameters
    timelock_client.initialize(
        &admin, 
        &(24 * 3600),        // 1 day min delay
        &(7 * 24 * 3600),    // 7 day grace period
        &(7 * 24 * 3600),    // 7 day default delay
        &(14 * 24 * 3600)    // 14 day critical delay
    );
    timelock_client.set_guardian(&admin, &guardian);
    
    // Initialize lending contract with timelock as admin
    lending_client.initialize(&timelock_id);
    
    (timelock_client, lending_client, admin, guardian)
}

#[test]
fn test_end_to_end_parameter_change() {
    let env = Env::default();
    env.mock_all_auths();
    
    let (timelock, lending, admin, _) = setup_integration_test(&env);
    
    // Verify initial state
    assert_eq!(lending.get_liquidation_threshold_bps(), 7500); // Default value
    
    // Queue a parameter change (high-risk operation, 7-day delay)
    let new_threshold = 8000i128;
    let func = Symbol::new(&env, "set_liquidation_threshold_bps");
    let args = Vec::from_array(&env, [new_threshold.into_val(&env)]);
    let eta = env.ledger().timestamp() + (7 * 24 * 3600);
    
    let action_id = timelock.queue(&admin, &lending.address, &func, &args, &eta);
    assert!(action_id.is_ok());
    
    // Verify parameter hasn't changed yet
    assert_eq!(lending.get_liquidation_threshold_bps(), 7500);
    
    // Try to execute before delay (should fail)
    let result = timelock.try_execute(&admin, &lending.address, &func, &args, &eta);
    assert_eq!(result, Err(Ok(TimelockError::TimelockNotReady)));
    
    // Advance time past the delay
    env.ledger().with_mut(|li| li.timestamp = eta + 1);
    
    // Execute the change
    let result = timelock.execute(&admin, &lending.address, &func, &args, &eta);
    assert!(result.is_ok());
    
    // Verify parameter has changed
    assert_eq!(lending.get_liquidation_threshold_bps(), 8000);
}

#[test]
fn test_critical_operation_requires_longer_delay() {
    let env = Env::default();
    env.mock_all_auths();
    
    let (timelock, lending, admin, _) = setup_integration_test(&env);
    let new_oracle = Address::generate(&env);
    
    // Try to queue oracle change with 7-day delay (should fail)
    let func = Symbol::new(&env, "set_oracle");
    let args = Vec::from_array(&env, [new_oracle.into_val(&env)]);
    let eta_short = env.ledger().timestamp() + (7 * 24 * 3600);
    
    let result = timelock.try_queue(&admin, &lending.address, &func, &args, &eta_short);
    assert_eq!(result, Err(Ok(TimelockError::DelayTooShort)));
    
    // Queue with 14-day delay (should succeed)
    let eta_long = env.ledger().timestamp() + (14 * 24 * 3600);
    let action_id = timelock.queue(&admin, &lending.address, &func, &args, &eta_long);
    assert!(action_id.is_ok());
    
    // Execute after delay
    env.ledger().with_mut(|li| li.timestamp = eta_long + 1);
    let result = timelock.execute(&admin, &lending.address, &func, &args, &eta_long);
    assert!(result.is_ok());
    
    // Verify oracle was set
    assert_eq!(lending.get_oracle(), Some(new_oracle));
}

#[test]
fn test_immediate_operation_execution() {
    let env = Env::default();
    env.mock_all_auths();
    
    let (timelock, lending, admin, _) = setup_integration_test(&env);
    
    // Execute immediate operation (get_admin is classified as immediate)
    let func = Symbol::new(&env, "get_admin");
    let args = Vec::new(&env);
    
    let result = timelock.execute_immediate(&admin, &lending.address, &func, &args);
    assert!(result.is_ok());
    
    // Verify we got the expected result (timelock address as admin)
    let returned_admin = result.unwrap();
    assert_eq!(Address::try_from_val(&env, &returned_admin).unwrap(), timelock.address);
}

#[test]
fn test_guardian_emergency_bypass() {
    let env = Env::default();
    env.mock_all_auths();
    
    let (timelock, lending, admin, guardian) = setup_integration_test(&env);
    
    // Guardian can execute emergency shutdown immediately
    let func = Symbol::new(&env, "emergency_shutdown");
    let args = Vec::new(&env);
    
    let result = timelock.execute_immediate(&guardian, &lending.address, &func, &args);
    assert!(result.is_ok());
}

#[test]
fn test_cancel_queued_action() {
    let env = Env::default();
    env.mock_all_auths();
    
    let (timelock, lending, admin, _) = setup_integration_test(&env);
    
    // Queue a parameter change
    let new_threshold = 8000i128;
    let func = Symbol::new(&env, "set_liquidation_threshold_bps");
    let args = Vec::from_array(&env, [new_threshold.into_val(&env)]);
    let eta = env.ledger().timestamp() + (7 * 24 * 3600);
    
    let action_id = timelock.queue(&admin, &lending.address, &func, &args, &eta);
    assert!(action_id.is_ok());
    
    // Cancel the action
    timelock.cancel(&admin, &lending.address, &func, &args, &eta);
    
    // Advance time and try to execute (should fail)
    env.ledger().with_mut(|li| li.timestamp = eta + 1);
    let result = timelock.try_execute(&admin, &lending.address, &func, &args, &eta);
    assert_eq!(result, Err(Ok(TimelockError::ActionNotQueued)));
    
    // Verify parameter didn't change
    assert_eq!(lending.get_liquidation_threshold_bps(), 7500);
}

#[test]
fn test_grace_period_expiration() {
    let env = Env::default();
    env.mock_all_auths();
    
    let (timelock, lending, admin, _) = setup_integration_test(&env);
    
    // Queue a parameter change
    let new_threshold = 8000i128;
    let func = Symbol::new(&env, "set_liquidation_threshold_bps");
    let args = Vec::from_array(&env, [new_threshold.into_val(&env)]);
    let eta = env.ledger().timestamp() + (7 * 24 * 3600);
    
    let action_id = timelock.queue(&admin, &lending.address, &func, &args, &eta);
    assert!(action_id.is_ok());
    
    // Advance time past grace period (7 days after eta)
    env.ledger().with_mut(|li| li.timestamp = eta + (7 * 24 * 3600) + 1);
    
    // Try to execute (should fail due to expiration)
    let result = timelock.try_execute(&admin, &lending.address, &func, &args, &eta);
    assert_eq!(result, Err(Ok(TimelockError::TimelockExpired)));
}

#[test]
fn test_emergency_state_restrictions() {
    let env = Env::default();
    env.mock_all_auths();
    
    let (timelock, lending, admin, guardian) = setup_integration_test(&env);
    
    // Trigger emergency shutdown
    timelock.emergency_shutdown(&guardian);
    
    // Try to queue a non-emergency operation (should fail)
    let new_threshold = 8000i128;
    let func = Symbol::new(&env, "set_liquidation_threshold_bps");
    let args = Vec::from_array(&env, [new_threshold.into_val(&env)]);
    let eta = env.ledger().timestamp() + (7 * 24 * 3600);
    
    let result = timelock.try_queue(&admin, &lending.address, &func, &args, &eta);
    assert_eq!(result, Err(Ok(TimelockError::EmergencyActive)));
    
    // Emergency operations should still work
    let emergency_func = Symbol::new(&env, "emergency_shutdown");
    let emergency_args = Vec::new(&env);
    let result = timelock.execute_immediate(&guardian, &lending.address, &emergency_func, &emergency_args);
    assert!(result.is_ok());
}