#![no_std]

pub mod storage;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod governance_tests;

#[cfg(test)]
mod integration_tests;

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, xdr::ToXdr, Address, BytesN, Env, Symbol, Vec,
};
use storage::{Config, StorageKey};

mod governance {
    use soroban_sdk::{contracttype, Symbol, Vec, Env};

    #[derive(Clone)]
    #[contracttype]
    pub struct GovernanceConfig {
        pub immediate_operations: Vec<Symbol>,
        pub default_delay: u64,
        pub critical_delay: u64,
    }

    #[derive(Clone)]
    #[contracttype]
    pub enum OperationRisk {
        Immediate,
        High,
        Critical,
    }

    pub struct GovernancePolicy;

    impl GovernancePolicy {
        pub fn get_operation_risk(func: &Symbol) -> OperationRisk {
            let func_str = func.to_string();
            
            match func_str.as_str() {
                "get_admin" | "get_guardian" | "get_emergency_state" | "get_pause_state" |
                "get_price" | "get_health_factor" | "get_user_position" | "get_collateral_balance" |
                "get_debt_balance" | "get_performance_stats" | "data_load" | "data_key_exists" |
                "data_schema_version" | "data_entry_count" | "current_version" | "current_wasm_hash" |
                "upgrade_status" => OperationRisk::Immediate,
                
                "set_oracle" | "configure_oracle" | "set_primary_oracle" | "set_fallback_oracle" |
                "upgrade_execute" | "complete_recovery" | "data_migrate_bump_version" => OperationRisk::Critical,
                
                _ => OperationRisk::High,
            }
        }
        
        pub fn get_required_delay(func: &Symbol, config: &GovernanceConfig) -> u64 {
            match Self::get_operation_risk(func) {
                OperationRisk::Immediate => 0,
                OperationRisk::High => config.default_delay,
                OperationRisk::Critical => config.critical_delay,
            }
        }
        
        pub fn is_immediate_operation(func: &Symbol, config: &GovernanceConfig) -> bool {
            config.immediate_operations.contains(func) || 
            matches!(Self::get_operation_risk(func), OperationRisk::Immediate)
        }
    }

    pub struct EmergencyPolicy;

    impl EmergencyPolicy {
        pub fn is_emergency_allowed(func: &Symbol) -> bool {
            let func_str = func.to_string();
            
            match func_str.as_str() {
                "emergency_shutdown" | "start_recovery" | "complete_recovery" |
                "get_emergency_state" | "get_guardian" | "repay" | "withdraw" |
                "liquidate" | "get_user_position" | "get_health_factor" => true,
                _ => false,
            }
        }
        
        pub fn is_guardian_bypass_allowed(func: &Symbol) -> bool {
            let func_str = func.to_string();
            
            match func_str.as_str() {
                "emergency_shutdown" | "set_pause" => true,
                _ => false,
            }
        }
    }
}

use governance::{GovernanceConfig, GovernancePolicy, EmergencyPolicy};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TimelockError {
    NotAdmin = 1,
    DelayTooShort = 2,
    ActionAlreadyQueued = 3,
    ActionNotQueued = 4,
    TimelockNotReady = 5,
    TimelockExpired = 6,
    NotInitialized = 7,
    NotGuardian = 8,
    OperationNotAllowed = 9,
    EmergencyActive = 10,
}

#[derive(Clone)]
#[contracttype]
pub struct ActionPayload {
    pub target: Address,
    pub func: Symbol,
    pub args: Vec<soroban_sdk::Val>,
    pub eta: u64,
}

#[contract]
pub struct TimelockContract;

fn get_action_id(env: &Env, target: &Address, func: &Symbol, args: &Vec<soroban_sdk::Val>, eta: u64) -> BytesN<32> {
    let payload = ActionPayload {
        target: target.clone(),
        func: func.clone(),
        args: args.clone(),
        eta,
    };
    env.crypto().keccak256(&payload.to_xdr(env)).into()
}

#[contractimpl]
impl TimelockContract {
    /// Initialize the timelock with admin, minimum delay, grace period, and governance config
    pub fn initialize(
        env: Env, 
        admin: Address, 
        min_delay: u64, 
        grace_period: u64,
        default_delay: u64,
        critical_delay: u64,
    ) -> Result<(), TimelockError> {
        if env.storage().instance().has(&StorageKey::Admin) {
            return Err(TimelockError::NotAdmin); // Already initialized
        }
        
        env.storage().instance().set(&StorageKey::Admin, &admin);
        env.storage().instance().set(&StorageKey::Config, &Config { min_delay, grace_period });
        
        // Initialize governance configuration
        let governance_config = GovernanceConfig {
            immediate_operations: Vec::new(&env),
            default_delay,
            critical_delay,
        };
        env.storage().instance().set(&StorageKey::GovernanceConfig, &governance_config);
        env.storage().instance().set(&StorageKey::EmergencyState, &storage::EmergencyState::Normal);
        
        Ok(())
    }

    /// Set the guardian address (admin only)
    pub fn set_guardian(
        env: Env,
        caller: Address,
        guardian: Address,
    ) -> Result<(), TimelockError> {
        caller.require_auth();
        
        let admin: Address = env.storage().instance().get(&StorageKey::Admin).ok_or(TimelockError::NotInitialized)?;
        if caller != admin {
            return Err(TimelockError::NotAdmin);
        }

        env.storage().instance().set(&StorageKey::Guardian, &guardian);
        Ok(())
    }

    /// Get the current guardian address
    pub fn get_guardian(env: Env) -> Option<Address> {
        env.storage().instance().get(&StorageKey::Guardian)
    }

    /// Update governance configuration (admin only)
    pub fn update_governance_config(
        env: Env,
        caller: Address,
        config: GovernanceConfig,
    ) -> Result<(), TimelockError> {
        caller.require_auth();
        
        let admin: Address = env.storage().instance().get(&StorageKey::Admin).ok_or(TimelockError::NotInitialized)?;
        if caller != admin {
            return Err(TimelockError::NotAdmin);
        }

        env.storage().instance().set(&StorageKey::GovernanceConfig, &config);
        Ok(())
    }

    /// Queue a delayed action with governance policy validation
    pub fn queue(
        env: Env,
        caller: Address,
        target: Address,
        func: Symbol,
        args: Vec<soroban_sdk::Val>,
        eta: u64,
    ) -> Result<BytesN<32>, TimelockError> {
        caller.require_auth();
        
        let admin: Address = env.storage().instance().get(&StorageKey::Admin).ok_or(TimelockError::NotInitialized)?;
        let governance_config: GovernanceConfig = env.storage().instance().get(&StorageKey::GovernanceConfig).ok_or(TimelockError::NotInitialized)?;
        let emergency_state: storage::EmergencyState = env.storage().instance().get(&StorageKey::EmergencyState).unwrap_or(storage::EmergencyState::Normal);
        
        // Check authorization
        if caller != admin {
            return Err(TimelockError::NotAdmin);
        }

        // Check if operation is allowed during emergency
        if !matches!(emergency_state, storage::EmergencyState::Normal) {
            if !EmergencyPolicy::is_emergency_allowed(&func) {
                return Err(TimelockError::EmergencyActive);
            }
        }

        // Check if this operation can be executed immediately
        if GovernancePolicy::is_immediate_operation(&func, &governance_config) {
            return Err(TimelockError::OperationNotAllowed); // Should use execute_immediate instead
        }

        let config: Config = env.storage().instance().get(&StorageKey::Config).ok_or(TimelockError::NotInitialized)?;
        let current_time = env.ledger().timestamp();

        // Validate delay meets governance requirements
        let required_delay = GovernancePolicy::get_required_delay(&func, &governance_config);
        let actual_delay = eta.saturating_sub(current_time);
        
        if actual_delay < required_delay.max(config.min_delay) {
            return Err(TimelockError::DelayTooShort);
        }

        let action_id = get_action_id(&env, &target, &func, &args, eta);
        let key = StorageKey::QueuedAction(action_id.clone());

        if env.storage().persistent().has(&key) {
            return Err(TimelockError::ActionAlreadyQueued);
        }

        env.storage().persistent().set(&key, &true);

        env.events().publish((Symbol::new(&env, "timelock"), Symbol::new(&env, "queue")), action_id.clone());

        Ok(action_id)
    }

    /// Execute immediate operations that don't require timelock delay
    pub fn execute_immediate(
        env: Env,
        caller: Address,
        target: Address,
        func: Symbol,
        args: Vec<soroban_sdk::Val>,
    ) -> Result<soroban_sdk::Val, TimelockError> {
        caller.require_auth();

        let admin: Address = env.storage().instance().get(&StorageKey::Admin).ok_or(TimelockError::NotInitialized)?;
        let governance_config: GovernanceConfig = env.storage().instance().get(&StorageKey::GovernanceConfig).ok_or(TimelockError::NotInitialized)?;
        let emergency_state: storage::EmergencyState = env.storage().instance().get(&StorageKey::EmergencyState).unwrap_or(storage::EmergencyState::Normal);

        // Check authorization - admin or guardian for emergency operations
        let is_admin = caller == admin;
        let is_guardian = env.storage().instance().get::<StorageKey, Address>(&StorageKey::Guardian)
            .map(|g| caller == g).unwrap_or(false);

        if !is_admin && !is_guardian {
            return Err(TimelockError::NotAdmin);
        }

        // Guardian can only execute emergency bypass operations
        if !is_admin && !EmergencyPolicy::is_guardian_bypass_allowed(&func) {
            return Err(TimelockError::NotGuardian);
        }

        // Check if operation is allowed during emergency
        if !matches!(emergency_state, storage::EmergencyState::Normal) {
            if !EmergencyPolicy::is_emergency_allowed(&func) {
                return Err(TimelockError::EmergencyActive);
            }
        }

        // Verify this operation is allowed for immediate execution
        if !GovernancePolicy::is_immediate_operation(&func, &governance_config) && 
           !EmergencyPolicy::is_guardian_bypass_allowed(&func) {
            return Err(TimelockError::OperationNotAllowed);
        }

        let result = env.invoke_contract(&target, &func, args);
        
        env.events().publish((Symbol::new(&env, "timelock"), Symbol::new(&env, "execute_immediate")), func);

        Ok(result)
    }

    /// Execute a previously queued action with governance validation
    pub fn execute(
        env: Env,
        caller: Address,
        target: Address,
        func: Symbol,
        args: Vec<soroban_sdk::Val>,
        eta: u64,
    ) -> Result<soroban_sdk::Val, TimelockError> {
        caller.require_auth();

        let admin: Address = env.storage().instance().get(&StorageKey::Admin).ok_or(TimelockError::NotInitialized)?;
        let emergency_state: storage::EmergencyState = env.storage().instance().get(&StorageKey::EmergencyState).unwrap_or(storage::EmergencyState::Normal);

        // Only admin can execute queued actions
        if caller != admin {
            return Err(TimelockError::NotAdmin);
        }

        // Check if operation is allowed during emergency
        if !matches!(emergency_state, storage::EmergencyState::Normal) {
            if !EmergencyPolicy::is_emergency_allowed(&func) {
                return Err(TimelockError::EmergencyActive);
            }
        }

        let config: Config = env.storage().instance().get(&StorageKey::Config).ok_or(TimelockError::NotInitialized)?;
        let current_time = env.ledger().timestamp();

        let action_id = get_action_id(&env, &target, &func, &args, eta);
        let key = StorageKey::QueuedAction(action_id.clone());

        if !env.storage().persistent().has(&key) {
            return Err(TimelockError::ActionNotQueued);
        }

        if current_time < eta {
            return Err(TimelockError::TimelockNotReady);
        }

        if current_time > eta + config.grace_period {
            return Err(TimelockError::TimelockExpired);
        }

        // Remove from storage before execution to prevent reentrancy
        env.storage().persistent().remove(&key);

        let result = env.invoke_contract(&target, &func, args);
        
        env.events().publish((Symbol::new(&env, "timelock"), Symbol::new(&env, "execute")), action_id);

        Ok(result)
    }

    /// Cancel a queued action before it executes
    pub fn cancel(
        env: Env,
        caller: Address,
        target: Address,
        func: Symbol,
        args: Vec<soroban_sdk::Val>,
        eta: u64,
    ) -> Result<(), TimelockError> {
        caller.require_auth();
        
        let admin: Address = env.storage().instance().get(&StorageKey::Admin).ok_or(TimelockError::NotInitialized)?;
        if caller != admin {
            return Err(TimelockError::NotAdmin);
        }

        let action_id = get_action_id(&env, &target, &func, &args, eta);
        let key = StorageKey::QueuedAction(action_id.clone());

        if !env.storage().persistent().has(&key) {
            return Err(TimelockError::ActionNotQueued);
        }

        env.storage().persistent().remove(&key);

        env.events().publish((Symbol::new(&env, "timelock"), Symbol::new(&env, "cancel")), action_id);

        Ok(())
    }

    /// Trigger emergency shutdown (admin or guardian only)
    pub fn emergency_shutdown(
        env: Env,
        caller: Address,
    ) -> Result<(), TimelockError> {
        caller.require_auth();

        let admin: Address = env.storage().instance().get(&StorageKey::Admin).ok_or(TimelockError::NotInitialized)?;
        let is_admin = caller == admin;
        let is_guardian = env.storage().instance().get::<StorageKey, Address>(&StorageKey::Guardian)
            .map(|g| caller == g).unwrap_or(false);

        if !is_admin && !is_guardian {
            return Err(TimelockError::NotGuardian);
        }

        env.storage().instance().set(&StorageKey::EmergencyState, &storage::EmergencyState::Shutdown);
        
        env.events().publish((Symbol::new(&env, "timelock"), Symbol::new(&env, "emergency_shutdown")), caller);

        Ok(())
    }

    /// Start recovery mode (admin only)
    pub fn start_recovery(
        env: Env,
        caller: Address,
    ) -> Result<(), TimelockError> {
        caller.require_auth();

        let admin: Address = env.storage().instance().get(&StorageKey::Admin).ok_or(TimelockError::NotInitialized)?;
        if caller != admin {
            return Err(TimelockError::NotAdmin);
        }

        env.storage().instance().set(&StorageKey::EmergencyState, &storage::EmergencyState::Recovery);
        
        env.events().publish((Symbol::new(&env, "timelock"), Symbol::new(&env, "start_recovery")), caller);

        Ok(())
    }

    /// Complete recovery and return to normal operation (admin only)
    pub fn complete_recovery(
        env: Env,
        caller: Address,
    ) -> Result<(), TimelockError> {
        caller.require_auth();

        let admin: Address = env.storage().instance().get(&StorageKey::Admin).ok_or(TimelockError::NotInitialized)?;
        if caller != admin {
            return Err(TimelockError::NotAdmin);
        }

        env.storage().instance().set(&StorageKey::EmergencyState, &storage::EmergencyState::Normal);
        
        env.events().publish((Symbol::new(&env, "timelock"), Symbol::new(&env, "complete_recovery")), caller);

        Ok(())
    }

    /// Get current emergency state
    pub fn get_emergency_state(env: Env) -> storage::EmergencyState {
        env.storage().instance().get(&StorageKey::EmergencyState).unwrap_or(storage::EmergencyState::Normal)
    }

    /// Get governance configuration
    pub fn get_governance_config(env: Env) -> Option<GovernanceConfig> {
        env.storage().instance().get(&StorageKey::GovernanceConfig)
    }
}
