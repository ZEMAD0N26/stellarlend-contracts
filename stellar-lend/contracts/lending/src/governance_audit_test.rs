//! # Governance Audit Log Tests
//!
//! Comprehensive test suite for the governance audit log functionality.
//! Tests event emission, storage, and view functions to ensure
//! complete audit coverage and compliance monitoring.

#[cfg(test)]
mod tests {
    extern crate std;
    use soroban_sdk::{Address, Env, String, Vec};
    use std::vec;
    use crate::governance_audit::{
        log_governance_action, get_recent_audit_entries, get_audit_count,
        GovernanceAction, GovernancePayload, MAX_AUDIT_ENTRIES,
    };

    #[test]
    fn test_audit_log_basic_functionality() {
        let env = Env::default();
        let admin = Address::generate(&env);
        let user = Address::generate(&env);

        // Initially no audit entries
        assert_eq!(get_audit_count(&env), 0);
        let entries = get_recent_audit_entries(&env, 10);
        assert_eq!(entries.len(), 0);

        // Log a simple action
        let payload = crate::governance_audit::payload_empty(&env);
        log_governance_action(&env, GovernanceAction::EmergencyShutdown, admin.clone(), payload);

        // Verify audit entry was created
        assert_eq!(get_audit_count(&env), 1);
        let entries = get_recent_audit_entries(&env, 10);
        assert_eq!(entries.len(), 1);

        let entry = &entries.get_unchecked(0);
        assert_eq!(entry.id, 1);
        assert_eq!(entry.action, GovernanceAction::EmergencyShutdown);
        assert_eq!(entry.caller, admin);
        assert!(entry.timestamp > 0);
        assert_eq!(entry.payload.data.len(), 0);
    }

    #[test]
    fn test_audit_log_with_payload() {
        let env = Env::default();
        let admin = Address::generate(&env);
        let asset = Address::generate(&env);
        let amount = 1000i128;

        // Log action with payload
        let payload = crate::governance_audit::payload_address_asset_i128(&env, admin.clone(), asset, amount);
        log_governance_action(&env, GovernanceAction::CreditInsuranceFund, admin.clone(), payload);

        // Verify audit entry with payload
        let entries = get_recent_audit_entries(&env, 10);
        assert_eq!(entries.len(), 1);

        let entry = &entries.get_unchecked(0);
        assert_eq!(entry.id, 1);
        assert_eq!(entry.action, GovernanceAction::CreditInsuranceFund);
        assert_eq!(entry.caller, admin);
        assert_eq!(entry.payload.data.len(), 3); // admin, asset, amount
    }

    #[test]
    fn test_audit_log_multiple_entries() {
        let env = Env::default();
        let admin = Address::generate(&env);
        let user = Address::generate(&env);

        // Log multiple actions
        for i in 0..5 {
            let payload = crate::governance_audit::payload_i128(&env, i as i128);
            log_governance_action(&env, GovernanceAction::SetLiquidationThreshold, admin.clone(), payload);
        }

        // Verify all entries are logged
        assert_eq!(get_audit_count(&env), 5);
        let entries = get_recent_audit_entries(&env, 10);
        assert_eq!(entries.len(), 5);

        // Entries should be in reverse chronological order (newest first)
        for (i, entry) in entries.iter().enumerate() {
            assert_eq!(entry.id, (5 - i) as u64);
            assert_eq!(entry.action, GovernanceAction::SetLiquidationThreshold);
            assert_eq!(entry.caller, admin);
        }
    }

    #[test]
    fn test_audit_log_limit_enforcement() {
        let env = Env::default();
        let admin = Address::generate(&env);

        // Test limit > 100 returns empty
        let entries = get_recent_audit_entries(&env, 101);
        assert_eq!(entries.len(), 0);

        // Test limit = 0 returns empty
        let entries = get_recent_audit_entries(&env, 0);
        assert_eq!(entries.len(), 0);

        // Test valid limit
        for i in 0..10 {
            let payload = crate::governance_audit::payload_empty(&env);
            log_governance_action(&env, GovernanceAction::SetPause, admin.clone(), payload);
        }

        let entries = get_recent_audit_entries(&env, 5);
        assert_eq!(entries.len(), 5);

        let entries = get_recent_audit_entries(&env, 10);
        assert_eq!(entries.len(), 10);

        let entries = get_recent_audit_entries(&env, 15);
        assert_eq!(entries.len(), 10); // Only 10 entries exist
    }

    #[test]
    fn test_audit_log_circular_buffer() {
        let env = Env::default();
        let admin = Address::generate(&env);

        // Fill the circular buffer
        for i in 0..MAX_AUDIT_ENTRIES {
            let payload = crate::governance_audit::payload_u64(&env, i);
            log_governance_action(&env, GovernanceAction::SetFlashLoanFee, admin.clone(), payload);
        }

        assert_eq!(get_audit_count(&env), MAX_AUDIT_ENTRIES);

        // Add one more entry to overwrite the oldest
        let payload = crate::governance_audit::payload_u64(&env, MAX_AUDIT_ENTRIES);
        log_governance_action(&env, GovernanceAction::SetFlashLoanFee, admin.clone(), payload);

        assert_eq!(get_audit_count(&env), MAX_AUDIT_ENTRIES + 1);

        // Get recent entries - should still work correctly
        let entries = get_recent_audit_entries(&env, 10);
        assert_eq!(entries.len(), 10);

        // The newest entry should have ID MAX_AUDIT_ENTRIES + 1
        let newest_entry = &entries.get_unchecked(0);
        assert_eq!(newest_entry.id, MAX_AUDIT_ENTRIES + 1);
    }

    #[test]
    fn test_audit_log_all_action_types() {
        let env = Env::default();
        let admin = Address::generate(&env);
        let asset = Address::generate(&env);
        let oracle = Address::generate(&env);
        let guardian = Address::generate(&env);

        // Test all major action types
        let test_cases = vec![
            (GovernanceAction::Initialize, crate::governance_audit::payload_empty(&env)),
            (GovernanceAction::SetAdmin, crate::governance_audit::payload_address(&env, admin.clone())),
            (GovernanceAction::SetPause, crate::governance_audit::payload_address_bool(&env, admin.clone(), true)),
            (GovernanceAction::SetGuardian, crate::governance_audit::payload_address(&env, guardian)),
            (GovernanceAction::EmergencyShutdown, crate::governance_audit::payload_empty(&env)),
            (GovernanceAction::StartRecovery, crate::governance_audit::payload_empty(&env)),
            (GovernanceAction::CompleteRecovery, crate::governance_audit::payload_empty(&env)),
            (GovernanceAction::SetOracle, crate::governance_audit::payload_address(&env, oracle)),
            (GovernanceAction::ConfigureOracle, crate::governance_audit::payload_u64(&env, 3600)),
            (GovernanceAction::SetPrimaryOracle, crate::governance_audit::payload_two_addresses(&env, asset, oracle)),
            (GovernanceAction::SetFallbackOracle, crate::governance_audit::payload_two_addresses(&env, asset, oracle)),
            (GovernanceAction::SetOraclePaused, crate::governance_audit::payload_address_bool(&env, admin.clone(), false)),
            (GovernanceAction::UpdatePriceFeed, crate::governance_audit::payload_address_asset_i128(&env, asset, asset, 1000000)),
            (GovernanceAction::SetLiquidationThreshold, crate::governance_audit::payload_i128(&env, 8000)),
            (GovernanceAction::SetCloseFactor, crate::governance_audit::payload_i128(&env, 5000)),
            (GovernanceAction::SetLiquidationIncentive, crate::governance_audit::payload_i128(&env, 1000)),
            (GovernanceAction::InitializeBorrowSettings, crate::governance_audit::payload_two_u64(&env, 1000000, 100)),
            (GovernanceAction::InitializeDepositSettings, crate::governance_audit::payload_two_u64(&env, 10000000, 100)),
            (GovernanceAction::InitializeWithdrawSettings, crate::governance_audit::payload_i128(&env, 100)),
            (GovernanceAction::SetFlashLoanFee, crate::governance_audit::payload_i128(&env, 50)),
            (GovernanceAction::InitializeCrossAssetAdmin, crate::governance_audit::payload_address(&env, admin.clone())),
            (GovernanceAction::SetAssetParams, crate::governance_audit::payload_address(&env, asset)),
            (GovernanceAction::UpgradeInit, crate::governance_audit::payload_two_u64(&env, 1, 2)),
            (GovernanceAction::UpgradeAddApprover, crate::governance_audit::payload_two_addresses(&env, admin.clone(), admin.clone())),
            (GovernanceAction::UpgradeRemoveApprover, crate::governance_audit::payload_two_addresses(&env, admin.clone(), admin.clone())),
            (GovernanceAction::UpgradePropose, crate::governance_audit::payload_two_u64(&env, 1, 2)),
            (GovernanceAction::UpgradeApprove, crate::governance_audit::payload_two_u64(&env, 1, 1)),
            (GovernanceAction::UpgradeExecute, crate::governance_audit::payload_u64(&env, 1)),
            (GovernanceAction::UpgradeRollback, crate::governance_audit::payload_u64(&env, 1)),
            (GovernanceAction::CreditInsuranceFund, crate::governance_audit::payload_address_asset_i128(&env, admin.clone(), asset, 1000)),
            (GovernanceAction::OffsetBadDebt, crate::governance_audit::payload_address_asset_i128(&env, admin.clone(), asset, 500)),
            (GovernanceAction::GrantDataWriter, crate::governance_audit::payload_two_addresses(&env, admin.clone(), admin.clone())),
            (GovernanceAction::RevokeDataWriter, crate::governance_audit::payload_two_addresses(&env, admin.clone(), admin.clone())),
            (GovernanceAction::DataBackup, crate::governance_audit::payload_string(&env, String::from_str(&env, "backup1"))),
            (GovernanceAction::DataRestore, crate::governance_audit::payload_string(&env, String::from_str(&env, "backup1"))),
            (GovernanceAction::DataMigrate, crate::governance_audit::payload_two_u64(&env, 2, 3)),
        ];

        for (action, payload) in test_cases {
            log_governance_action(&env, action, admin.clone(), payload);
        }

        assert_eq!(get_audit_count(&env), test_cases.len() as u64);

        // Verify all entries are properly stored
        let entries = get_recent_audit_entries(&env, 100);
        assert_eq!(entries.len(), test_cases.len());

        // Verify each action type is correctly stored
        for (i, (expected_action, _)) in test_cases.iter().enumerate() {
            let entry = &entries.get_unchecked(test_cases.len() - 1 - i); // Reverse order
            assert_eq!(entry.action, *expected_action);
            assert_eq!(entry.caller, admin);
            assert!(entry.timestamp > 0);
        }
    }

    #[test]
    fn test_payload_helper_functions() {
        let env = Env::default();
        let admin = Address::generate(&env);
        let asset = Address::generate(&env);
        let amount = 1000i128;
        let bool_val = true;
        let u64_val = 42u64;
        let string_val = String::from_str(&env, "test");

        // Test all payload helper functions
        let payload1 = crate::governance_audit::payload_empty(&env);
        assert_eq!(payload1.data.len(), 0);

        let payload2 = crate::governance_audit::payload_address(&env, admin.clone());
        assert_eq!(payload2.data.len(), 1);

        let payload3 = crate::governance_audit::payload_address_bool(&env, admin.clone(), bool_val);
        assert_eq!(payload3.data.len(), 2);

        let payload4 = crate::governance_audit::payload_address_u64(&env, admin.clone(), u64_val);
        assert_eq!(payload4.data.len(), 2);

        let payload5 = crate::governance_audit::payload_address_i128(&env, admin.clone(), amount);
        assert_eq!(payload5.data.len(), 2);

        let payload6 = crate::governance_audit::payload_two_addresses(&env, admin.clone(), asset);
        assert_eq!(payload6.data.len(), 2);

        let payload7 = crate::governance_audit::payload_address_asset_i128(&env, admin.clone(), asset, amount);
        assert_eq!(payload7.data.len(), 3);

        let payload8 = crate::governance_audit::payload_i128(&env, amount);
        assert_eq!(payload8.data.len(), 1);

        let payload9 = crate::governance_audit::payload_u64(&env, u64_val);
        assert_eq!(payload9.data.len(), 1);

        let payload10 = crate::governance_audit::payload_two_u64(&env, u64_val, u64_val + 1);
        assert_eq!(payload10.data.len(), 2);

        let payload11 = crate::governance_audit::payload_string(&env, string_val.clone());
        assert_eq!(payload11.data.len(), 1);
    }

    #[test]
    fn test_audit_event_emission() {
        let env = Env::default();
        let admin = Address::generate(&env);

        // Log an action and verify event is emitted
        let payload = crate::governance_audit::payload_empty(&env);
        
        // Capture events
        let mut events = Vec::new(&env);
        env.events().all(&mut events);

        log_governance_action(&env, GovernanceAction::SetPause, admin.clone(), payload);

        // Check that an event was emitted
        let mut new_events = Vec::new(&env);
        env.events().all(&mut new_events);
        assert_eq!(new_events.len(), events.len() + 1);

        // The new event should be a GovernanceAuditEvent
        let new_event = &new_events.get_unchecked(new_events.len() - 1);
        // Note: In a real test environment, you would verify the event structure
        // This is a basic test to ensure events are being emitted
    }

    #[test]
    fn test_audit_storage_persistence() {
        let env = Env::default();
        let admin = Address::generate(&env);

        // Log some actions
        for i in 0..5 {
            let payload = crate::governance_audit::payload_u64(&env, i);
            log_governance_action(&env, GovernanceAction::SetLiquidationThreshold, admin.clone(), payload);
        }

        // Verify persistence
        assert_eq!(get_audit_count(&env), 5);
        let entries = get_recent_audit_entries(&env, 10);
        assert_eq!(entries.len(), 5);

        // Simulate ledger close and reopen (in real Stellar environment)
        // In test, we just verify the data is still accessible
        let entries_after = get_recent_audit_entries(&env, 10);
        assert_eq!(entries_after.len(), 5);
        assert_eq!(entries_after.get_unchecked(0).id, 5);
    }

    #[test]
    fn test_audit_pagination() {
        let env = Env::default();
        let admin = Address::generate(&env);

        // Create 25 entries
        for i in 0..25 {
            let payload = crate::governance_audit::payload_u64(&env, i);
            log_governance_action(&env, GovernanceAction::SetFlashLoanFee, admin.clone(), payload);
        }

        // Test pagination with different limits
        let page1 = get_recent_audit_entries(&env, 10);
        assert_eq!(page1.len(), 10);
        assert_eq!(page1.get_unchecked(0).id, 25);
        assert_eq!(page1.get_unchecked(9).id, 16);

        let page2 = get_recent_audit_entries(&env, 15);
        assert_eq!(page2.len(), 15);
        assert_eq!(page2.get_unchecked(0).id, 25);
        assert_eq!(page2.get_unchecked(14).id, 11);

        let all = get_recent_audit_entries(&env, 30);
        assert_eq!(all.len(), 25);
        assert_eq!(all.get_unchecked(0).id, 25);
        assert_eq!(all.get_unchecked(24).id, 1);
    }
}
