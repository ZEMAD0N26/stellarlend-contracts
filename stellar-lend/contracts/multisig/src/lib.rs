#![no_std]

use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, Address, Env, Symbol,
};

/// Minimum threshold delay in ledgers (7 days = ~604,800 seconds / 5 sec per ledger = ~120,960 ledgers)
/// Using conservative estimate: 600,000 ledgers for 7 days
const MIN_THRESHOLD_DELAY_LEDGERS: u32 = 600_000;

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataKey {
    /// Current multisig threshold
    Threshold,
    /// Admin who can queue and apply threshold changes
    Admin,
    /// Pending threshold change (new_threshold, eta_ledger)
    PendingThresholdChange,
    /// Ledger number when contract was initialized
    InitializedLedger,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum MultisigError {
    /// Caller is not the admin
    Unauthorized = 1001,
    /// No pending threshold change queued
    NoQueuedChange = 1002,
    /// Threshold change delay period not yet elapsed
    DelayNotElapsed = 1003,
    /// Invalid threshold (must be > 0)
    InvalidThreshold = 1004,
    /// Contract has not been initialized
    NotInitialized = 1005,
    /// Contract already initialized
    AlreadyInitialized = 1006,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThresholdChange {
    pub new_threshold: u32,
    pub eta_ledger: u32,
}

#[contractevent]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThresholdChangeQueuedEvent {
    pub admin: Address,
    pub new_threshold: u32,
    pub eta_ledger: u32,
}

#[contractevent]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThresholdChangeAppliedEvent {
    pub admin: Address,
    pub old_threshold: u32,
    pub new_threshold: u32,
    pub ledger: u32,
}

#[contract]
pub struct MultisigContract;

#[contractimpl]
impl MultisigContract {
    /// Initialize the multisig contract with an admin and initial threshold.
    /// Can only be called once.
    ///
    /// # Arguments
    /// * `env` - Soroban environment
    /// * `admin` - Address of the multisig administrator
    /// * `initial_threshold` - Initial signing threshold (must be > 0)
    ///
    /// # Errors
    /// * `AlreadyInitialized` - Contract already initialized
    /// * `InvalidThreshold` - Initial threshold is 0 or negative
    pub fn initialize(
        env: Env,
        admin: Address,
        initial_threshold: u32,
    ) -> Result<(), MultisigError> {
        if env.storage().instance().has(&DataKey::Threshold) {
            return Err(MultisigError::AlreadyInitialized);
        }

        if initial_threshold == 0 {
            return Err(MultisigError::InvalidThreshold);
        }

        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::Threshold, &initial_threshold);
        env.storage()
            .instance()
            .set(&DataKey::InitializedLedger, &env.ledger().sequence());

        Ok(())
    }

    /// Get the current multisig threshold.
    ///
    /// # Arguments
    /// * `env` - Soroban environment
    ///
    /// # Returns
    /// Current threshold value
    ///
    /// # Errors
    /// * `NotInitialized` - Contract not yet initialized
    pub fn get_threshold(env: Env) -> Result<u32, MultisigError> {
        env.storage()
            .instance()
            .get(&DataKey::Threshold)
            .ok_or(MultisigError::NotInitialized)
    }

    /// Get the current admin.
    ///
    /// # Arguments
    /// * `env` - Soroban environment
    ///
    /// # Returns
    /// Current admin address
    ///
    /// # Errors
    /// * `NotInitialized` - Contract not yet initialized
    pub fn get_admin(env: Env) -> Result<Address, MultisigError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(MultisigError::NotInitialized)
    }

    /// Get any pending threshold change.
    ///
    /// # Arguments
    /// * `env` - Soroban environment
    ///
    /// # Returns
    /// Option containing pending change (new_threshold, eta_ledger) or None
    pub fn get_pending_threshold_change(env: Env) -> Option<ThresholdChange> {
        env.storage()
            .instance()
            .get(&DataKey::PendingThresholdChange)
    }

    /// Queue a new threshold change with a minimum delay of MIN_THRESHOLD_DELAY_LEDGERS.
    /// This prevents same-ledger takeover by requiring a delayed execution step.
    ///
    /// # Arguments
    /// * `env` - Soroban environment
    /// * `new_threshold` - New threshold value (must be > 0)
    ///
    /// # Security Invariant
    /// The threshold change is not applied immediately. It must be executed separately
    /// via `apply_threshold_change()` after the delay period has elapsed. This ensures
    /// that a compromised quorum cannot lower the threshold and pass a malicious proposal
    /// in the same transaction.
    ///
    /// # Errors
    /// * `Unauthorized` - Caller is not the admin
    /// * `NotInitialized` - Contract not initialized
    /// * `InvalidThreshold` - New threshold is 0
    pub fn queue_threshold_change(env: Env, new_threshold: u32) -> Result<(), MultisigError> {
        let admin = Self::get_admin(env.clone())?;
        admin.require_auth();

        if new_threshold == 0 {
            return Err(MultisigError::InvalidThreshold);
        }

        let current_ledger = env.ledger().sequence();
        let eta_ledger = current_ledger + MIN_THRESHOLD_DELAY_LEDGERS;

        let change = ThresholdChange {
            new_threshold,
            eta_ledger,
        };

        env.storage()
            .instance()
            .set(&DataKey::PendingThresholdChange, &change);

        ThresholdChangeQueuedEvent {
            admin: admin.clone(),
            new_threshold,
            eta_ledger,
        }
        .publish(&env);

        Ok(())
    }

    /// Apply a previously queued threshold change.
    /// The delay period (MIN_THRESHOLD_DELAY_LEDGERS) must have elapsed since queueing.
    ///
    /// # Arguments
    /// * `env` - Soroban environment
    ///
    /// # Security Invariant
    /// * Prevents same-ledger takeover by enforcing a time delay
    /// * Only applies if the current ledger >= eta_ledger from queue operation
    /// * Clears the pending change on successful application
    ///
    /// # Errors
    /// * `Unauthorized` - Caller is not the admin
    /// * `NotInitialized` - Contract not initialized
    /// * `NoQueuedChange` - No threshold change is queued
    /// * `DelayNotElapsed` - Current ledger < eta_ledger
    pub fn apply_threshold_change(env: Env) -> Result<(), MultisigError> {
        let admin = Self::get_admin(env.clone())?;
        admin.require_auth();

        let change = env
            .storage()
            .instance()
            .get(&DataKey::PendingThresholdChange)
            .ok_or(MultisigError::NoQueuedChange)?;

        let current_ledger = env.ledger().sequence();
        if current_ledger < change.eta_ledger {
            return Err(MultisigError::DelayNotElapsed);
        }

        let old_threshold = Self::get_threshold(env.clone())?;
        env.storage()
            .instance()
            .set(&DataKey::Threshold, &change.new_threshold);
        env.storage()
            .instance()
            .remove(&DataKey::PendingThresholdChange);

        ThresholdChangeAppliedEvent {
            admin: admin.clone(),
            old_threshold,
            new_threshold: change.new_threshold,
            ledger: current_ledger,
        }
        .publish(&env);

        Ok(())
    }

    /// Get the minimum threshold delay in ledgers.
    ///
    /// # Arguments
    /// * `env` - Soroban environment (unused, kept for consistency)
    ///
    /// # Returns
    /// Minimum delay in ledgers
    pub fn get_min_threshold_delay_ledgers(_env: Env) -> u32 {
        MIN_THRESHOLD_DELAY_LEDGERS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::testutils::Address as AddressTestUtils;
    use soroban_sdk::testutils::Ledger;

    fn setup() -> (Env, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let contract_id = env.register_contract(None, MultisigContract);
        (env, admin, contract_id)
    }

    fn setup_initialized(threshold: u32) -> (Env, Address, Address) {
        let (env, admin, contract_id) = setup();
        let client = MultisigContractClient::new(&env, &contract_id);
        client.initialize(&admin, &threshold).unwrap();
        (env, admin, contract_id)
    }

    #[test]
    fn test_initialize_success() {
        let (env, admin, contract_id) = setup();
        let client = MultisigContractClient::new(&env, &contract_id);
        let result = client.initialize(&admin, &5);
        assert!(result.is_ok());
        assert_eq!(client.get_threshold().unwrap(), 5);
        assert_eq!(client.get_admin().unwrap(), admin);
    }

    #[test]
    fn test_initialize_with_zero_threshold() {
        let (env, admin, contract_id) = setup();
        let client = MultisigContractClient::new(&env, &contract_id);
        let result = client.initialize(&admin, &0);
        assert_eq!(result, Err(Ok(MultisigError::InvalidThreshold)));
    }

    #[test]
    fn test_initialize_already_initialized() {
        let (env, admin, contract_id) = setup();
        let client = MultisigContractClient::new(&env, &contract_id);
        client.initialize(&admin, &3).unwrap();
        let result = client.initialize(&admin, &5);
        assert_eq!(result, Err(Ok(MultisigError::AlreadyInitialized)));
    }

    #[test]
    fn test_get_threshold_not_initialized() {
        let (env, _admin, contract_id) = setup();
        let client = MultisigContractClient::new(&env, &contract_id);
        let result = client.get_threshold();
        assert_eq!(result, Err(Ok(MultisigError::NotInitialized)));
    }

    #[test]
    fn test_get_admin_not_initialized() {
        let (env, _admin, contract_id) = setup();
        let client = MultisigContractClient::new(&env, &contract_id);
        let result = client.get_admin();
        assert_eq!(result, Err(Ok(MultisigError::NotInitialized)));
    }

    #[test]
    fn test_queue_threshold_change_success() {
        let (env, admin, contract_id) = setup_initialized(3);
        let client = MultisigContractClient::new(&env, &contract_id);

        let initial_ledger = env.ledger().sequence();
        let result = client.queue_threshold_change(&5);
        assert!(result.is_ok());

        let pending = client.get_pending_threshold_change();
        assert!(pending.is_some());
        let change = pending.unwrap();
        assert_eq!(change.new_threshold, 5);
        assert_eq!(
            change.eta_ledger,
            initial_ledger + MIN_THRESHOLD_DELAY_LEDGERS
        );
    }

    #[test]
    fn test_queue_threshold_change_not_initialized() {
        let (env, _admin, contract_id) = setup();
        let client = MultisigContractClient::new(&env, &contract_id);
        let result = client.queue_threshold_change(&5);
        assert_eq!(result, Err(Ok(MultisigError::NotInitialized)));
    }

    #[test]
    fn test_queue_threshold_change_zero_threshold() {
        let (env, admin, contract_id) = setup_initialized(3);
        let client = MultisigContractClient::new(&env, &contract_id);
        let result = client.queue_threshold_change(&0);
        assert_eq!(result, Err(Ok(MultisigError::InvalidThreshold)));
    }

    #[test]
    fn test_queue_threshold_change_unauthorized() {
        let (env, _admin, contract_id) = setup_initialized(3);
        let other = Address::generate(&env);
        let client = MultisigContractClient::new(&env, &contract_id);

        // Disable mock_all_auths to enforce auth check
        env.mock_all_auths_allow_last();
        let result = client.queue_threshold_change(&5);
        assert_eq!(result, Err(Ok(MultisigError::Unauthorized)));
    }

    #[test]
    fn test_apply_threshold_change_before_delay() {
        let (env, admin, contract_id) = setup_initialized(3);
        let client = MultisigContractClient::new(&env, &contract_id);

        client.queue_threshold_change(&5).unwrap();

        // Try to apply before delay elapsed
        let result = client.apply_threshold_change();
        assert_eq!(result, Err(Ok(MultisigError::DelayNotElapsed)));

        // Threshold should not have changed
        assert_eq!(client.get_threshold().unwrap(), 3);
    }

    #[test]
    fn test_apply_threshold_change_after_delay() {
        let (env, admin, contract_id) = setup_initialized(3);
        let client = MultisigContractClient::new(&env, &contract_id);

        client.queue_threshold_change(&5).unwrap();
        let initial_ledger = env.ledger().sequence();

        // Jump past the delay
        env.ledger()
            .set_sequence_number(initial_ledger + MIN_THRESHOLD_DELAY_LEDGERS);

        let result = client.apply_threshold_change();
        assert!(result.is_ok());

        // Threshold should have changed
        assert_eq!(client.get_threshold().unwrap(), 5);

        // Pending change should be cleared
        assert!(client.get_pending_threshold_change().is_none());
    }

    #[test]
    fn test_apply_threshold_change_no_queued_change() {
        let (env, admin, contract_id) = setup_initialized(3);
        let client = MultisigContractClient::new(&env, &contract_id);

        let result = client.apply_threshold_change();
        assert_eq!(result, Err(Ok(MultisigError::NoQueuedChange)));
    }

    #[test]
    fn test_apply_threshold_change_unauthorized() {
        let (env, admin, contract_id) = setup_initialized(3);
        let client = MultisigContractClient::new(&env, &contract_id);

        client.queue_threshold_change(&5).unwrap();
        let initial_ledger = env.ledger().sequence();
        env.ledger()
            .set_sequence_number(initial_ledger + MIN_THRESHOLD_DELAY_LEDGERS);

        // Disable mock_all_auths to enforce auth check
        env.mock_all_auths_allow_last();
        let other = Address::generate(&env);
        let result = client.apply_threshold_change();
        assert_eq!(result, Err(Ok(MultisigError::Unauthorized)));
    }

    #[test]
    fn test_multiple_threshold_changes() {
        let (env, admin, contract_id) = setup_initialized(3);
        let client = MultisigContractClient::new(&env, &contract_id);

        // Queue first change
        client.queue_threshold_change(&5).unwrap();
        let initial_ledger = env.ledger().sequence();

        // Jump to after delay
        env.ledger()
            .set_sequence_number(initial_ledger + MIN_THRESHOLD_DELAY_LEDGERS);

        // Apply first change
        client.apply_threshold_change().unwrap();
        assert_eq!(client.get_threshold().unwrap(), 5);

        // Queue second change
        client.queue_threshold_change(&7).unwrap();
        let second_ledger = env.ledger().sequence();

        // Jump to after delay
        env.ledger()
            .set_sequence_number(second_ledger + MIN_THRESHOLD_DELAY_LEDGERS);

        // Apply second change
        client.apply_threshold_change().unwrap();
        assert_eq!(client.get_threshold().unwrap(), 7);
    }

    #[test]
    fn test_overwrite_pending_change() {
        let (env, admin, contract_id) = setup_initialized(3);
        let client = MultisigContractClient::new(&env, &contract_id);

        // Queue first change
        client.queue_threshold_change(&5).unwrap();
        let pending1 = client.get_pending_threshold_change().unwrap();
        assert_eq!(pending1.new_threshold, 5);

        // Queue second change (overwrites first)
        client.queue_threshold_change(&7).unwrap();
        let pending2 = client.get_pending_threshold_change().unwrap();
        assert_eq!(pending2.new_threshold, 7);

        // Apply should use the second change
        let initial_ledger = env.ledger().sequence();
        env.ledger()
            .set_sequence_number(initial_ledger + MIN_THRESHOLD_DELAY_LEDGERS);
        client.apply_threshold_change().unwrap();
        assert_eq!(client.get_threshold().unwrap(), 7);
    }

    #[test]
    fn test_same_ledger_protection() {
        let (env, admin, contract_id) = setup_initialized(3);
        let client = MultisigContractClient::new(&env, &contract_id);

        // Queue a threshold change
        client.queue_threshold_change(&1).unwrap();

        // Try to apply on the same ledger
        let result = client.apply_threshold_change();
        assert_eq!(result, Err(Ok(MultisigError::DelayNotElapsed)));

        // Verify threshold unchanged
        assert_eq!(client.get_threshold().unwrap(), 3);
    }

    #[test]
    fn test_get_min_threshold_delay_ledgers() {
        let (env, admin, contract_id) = setup_initialized(3);
        let client = MultisigContractClient::new(&env, &contract_id);
        assert_eq!(
            client.get_min_threshold_delay_ledgers(),
            MIN_THRESHOLD_DELAY_LEDGERS
        );
    }

    #[test]
    fn test_queue_then_apply_reduces_takeover_window() {
        // This test demonstrates that once threshold is lowered, it cannot be used
        // in the same transaction where it was queued, preventing immediate takeover
        let (env, admin, contract_id) = setup_initialized(3);
        let client = MultisigContractClient::new(&env, &contract_id);

        let initial_ledger = env.ledger().sequence();

        // Queue threshold change to 1 (hypothetical takeover attempt)
        client.queue_threshold_change(&1).unwrap();

        // On same ledger, threshold is still 3
        assert_eq!(client.get_threshold().unwrap(), 3);

        // Cannot apply on same ledger
        assert_eq!(
            client.apply_threshold_change(),
            Err(Ok(MultisigError::DelayNotElapsed))
        );
    }

    #[test]
    fn test_apply_at_exact_eta() {
        let (env, admin, contract_id) = setup_initialized(3);
        let client = MultisigContractClient::new(&env, &contract_id);

        let initial_ledger = env.ledger().sequence();
        client.queue_threshold_change(&5).unwrap();

        // Get the exact ETA
        let pending = client.get_pending_threshold_change().unwrap();
        let eta = pending.eta_ledger;

        // Jump to exactly the ETA ledger
        env.ledger().set_sequence_number(eta);

        // Should succeed at exact ETA
        let result = client.apply_threshold_change();
        assert!(result.is_ok());
        assert_eq!(client.get_threshold().unwrap(), 5);
    }

    #[test]
    fn test_apply_after_eta() {
        let (env, admin, contract_id) = setup_initialized(3);
        let client = MultisigContractClient::new(&env, &contract_id);

        let initial_ledger = env.ledger().sequence();
        client.queue_threshold_change(&5).unwrap();

        // Jump far past the delay
        env.ledger()
            .set_sequence_number(initial_ledger + MIN_THRESHOLD_DELAY_LEDGERS * 2);

        // Should still succeed
        let result = client.apply_threshold_change();
        assert!(result.is_ok());
        assert_eq!(client.get_threshold().unwrap(), 5);
    }

    #[test]
    fn test_large_threshold_values() {
        let (env, admin, contract_id) = setup_initialized(1);
        let client = MultisigContractClient::new(&env, &contract_id);

        let large_threshold = 1_000_000u32;
        client.queue_threshold_change(&large_threshold).unwrap();

        let initial_ledger = env.ledger().sequence();
        env.ledger()
            .set_sequence_number(initial_ledger + MIN_THRESHOLD_DELAY_LEDGERS);

        client.apply_threshold_change().unwrap();
        assert_eq!(client.get_threshold().unwrap(), large_threshold);
    }
}
