//! # Liquidation-Pause Policy Tests
//!
//! This module implements comprehensive tests for liquidation behavior under all
//! pause and emergency states, explicitly defining the policy for whether liquidation
//! is allowed while other operations are paused.
//!
//! ## Policy Definition
//!
//! ### Normal State + Granular Pauses
//! - **Liquidation Paused**: Liquidations BLOCKED (solvency protection)
//! - **Other Operations Paused**: Liquidations ALLOWED (market health)
//!
//! ### Emergency States
//! - **Shutdown**: Liquidations BLOCKED (hard stop)
//! - **Recovery**: Liquidations BLOCKED (unwind-only mode)
//! - **ReadOnly**: Liquidations BLOCKED (incident freeze)
//!
//! ### Global Pause (All)
//! - **All Pause Active**: Liquidations BLOCKED (protocol-wide halt)
//!
//! ## Rationale
//!
//! 1. **Solvency Protection**: When liquidations are explicitly paused, we protect
//!    potentially solvent positions from being liquidated during oracle issues or
//!    market volatility.
//!
//! 2. **Market Health**: When other operations are paused but liquidations remain
//!    available, we allow the market to self-correct unhealthy positions.
//!
//! 3. **Emergency Safety**: During emergency states, liquidations are blocked to
//!    prevent cascading failures and allow controlled recovery.

use super::*;
use crate::deposit::DepositError;
use crate::withdraw::WithdrawError;
use soroban_sdk::{testutils::Address as _, Address, Env};

// Mock oracle for liquidation tests
#[contract]
pub struct LiquidationPolicyOracle;

#[contractimpl]
impl LiquidationPolicyOracle {
    pub fn price(_env: Env, _asset: Address) -> i128 {
        100_000_000 // 1.0 USD with 8 decimals
    }
}

// Helper setup function
fn setup_liquidation_policy_test(
    env: &Env,
) -> (
    LendingContractClient<'_>,
    Address, // admin
    Address, // guardian
    Address, // borrower
    Address, // liquidator
    Address, // asset
    Address, // collateral_asset
) {
    let contract_id = env.register(LendingContract, ());
    let client = LendingContractClient::new(env, &contract_id);

    let admin = Address::generate(env);
    let guardian = Address::generate(env);
    let borrower = Address::generate(env);
    let liquidator = Address::generate(env);
    let asset = Address::generate(env);
    let collateral_asset = Address::generate(env);

    client.initialize(&admin, &1_000_000_000, &1000);
    
    // Set up oracle
    let oracle_id = env.register(LiquidationPolicyOracle, ());
    client.set_oracle(&admin, &oracle_id);
    
    // Configure for liquidation (40% threshold = 0.4x collateral)
    client.set_liquidation_threshold_bps(&admin, &4000);
    client.set_guardian(&admin, &guardian);
    client.initialize_deposit_settings(&1_000_000_000, &100);
    client.initialize_withdraw_settings(&100);

    (client, admin, guardian, borrower, liquidator, asset, collateral_asset)
}

// Create an underwater position for testing
fn create_underwater_position(
    client: &LendingContractClient<'_>,
    borrower: &Address,
    asset: &Address,
    collateral_asset: &Address,
) {
    // Borrow 10,000 with 15,000 collateral
    // Health factor = (15,000 * 0.4 * 10,000) / 10,000 = 6,000 < 10,000 (underwater)
    client.borrow(borrower, asset, &10_000, collateral_asset, &15_000);
}

// ============================================================================
// SECTION 1: Normal State + Granular Pause Tests
// ============================================================================

/// Test 1.1: Liquidation explicitly paused blocks liquidations only
#[test]
fn test_liquidation_explicitly_paused_blocks_only_liquidations() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, guardian, borrower, liquidator, asset, collateral_asset) = 
        setup_liquidation_policy_test(&env);

    create_underwater_position(&client, &borrower, &asset, &collateral_asset);

    // Pause liquidations explicitly
    client.set_pause(&admin, &PauseType::Liquidation, &true);
    assert!(client.get_pause_state(&PauseType::Liquidation));

    // Liquidation should be blocked
    assert_eq!(
        client.try_liquidate(&liquidator, &borrower, &asset, &collateral_asset, &5_000),
        Err(Ok(BorrowError::ProtocolPaused))
    );

    // Other operations should work
    client.deposit(&borrower, &asset, &1_000);
    client.borrow(&borrower, &asset, &1_000, &collateral_asset, &2_000);
    client.repay(&borrower, &asset, &1_000);
    client.withdraw(&borrower, &asset, &500);

    // Verify emergency state is still Normal
    assert_eq!(client.get_emergency_state(), EmergencyState::Normal);
}

/// Test 1.2: Other operations paused allows liquidations (market health)
#[test]
fn test_other_operations_paused_allows_liquidations() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, guardian, borrower, liquidator, asset, collateral_asset) = 
        setup_liquidation_policy_test(&env);

    create_underwater_position(&client, &borrower, &asset, &collateral_asset);

    // Pause other operations but NOT liquidations
    client.set_pause(&admin, &PauseType::Deposit, &true);
    client.set_pause(&admin, &PauseType::Borrow, &true);
    client.set_pause(&admin, &PauseType::Repay, &true);
    client.set_pause(&admin, &PauseType::Withdraw, &true);

    // Verify liquidations are NOT paused
    assert!(!client.get_pause_state(&PauseType::Liquidation));

    // Liquidation should work (market health protection)
    client.liquidate(&liquidator, &borrower, &asset, &collateral_asset, &5_000);

    // Verify debt was reduced
    let debt = client.get_user_debt(&borrower);
    assert!(debt.borrowed_amount < 10_000);

    // Other operations should be blocked
    assert_eq!(
        client.try_deposit(&borrower, &asset, &1_000),
        Err(Ok(DepositError::DepositPaused))
    );
    assert_eq!(
        client.try_borrow(&borrower, &asset, &1_000, &collateral_asset, &2_000),
        Err(Ok(BorrowError::ProtocolPaused))
    );
    assert_eq!(
        client.try_repay(&borrower, &asset, &1_000),
        Err(Ok(BorrowError::ProtocolPaused))
    );
    assert_eq!(
        client.try_withdraw(&borrower, &asset, &1_000),
        Err(Ok(WithdrawError::WithdrawPaused))
    );
}

/// Test 1.3: Global pause blocks liquidations (protocol-wide halt)
#[test]
fn test_global_pause_blocks_liquidations() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, guardian, borrower, liquidator, asset, collateral_asset) = 
        setup_liquidation_policy_test(&env);

    create_underwater_position(&client, &borrower, &asset, &collateral_asset);

    // Set global pause
    client.set_pause(&admin, &PauseType::All, &true);
    assert!(client.get_pause_state(&PauseType::All));

    // Liquidation should be blocked despite individual liquidation flag being false
    assert!(!client.get_pause_state(&PauseType::Liquidation)); // Individual flag false
    assert!(client.get_pause_state(&PauseType::Liquidation)); // But appears paused due to All

    assert_eq!(
        client.try_liquidate(&liquidator, &borrower, &asset, &collateral_asset, &5_000),
        Err(Ok(BorrowError::ProtocolPaused))
    );
}

// ============================================================================
// SECTION 2: Emergency State Tests
// ============================================================================

/// Test 2.1: Emergency shutdown blocks all liquidations
#[test]
fn test_emergency_shutdown_blocks_liquidations() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, guardian, borrower, liquidator, asset, collateral_asset) = 
        setup_liquidation_policy_test(&env);

    create_underwater_position(&client, &borrower, &asset, &collateral_asset);

    // Trigger emergency shutdown
    client.emergency_shutdown(&guardian);
    assert_eq!(client.get_emergency_state(), EmergencyState::Shutdown);

    // Liquidation should be blocked regardless of pause flags
    assert_eq!(
        client.try_liquidate(&liquidator, &borrower, &asset, &collateral_asset, &5_000),
        Err(Ok(BorrowError::ProtocolPaused))
    );

    // Even unpausing liquidations shouldn't work in shutdown
    client.set_pause(&admin, &PauseType::Liquidation, &false);
    assert_eq!(
        client.try_liquidate(&liquidator, &borrower, &asset, &collateral_asset, &5_000),
        Err(Ok(BorrowError::ProtocolPaused))
    );
}

/// Test 2.2: Recovery mode blocks liquidations (unwind-only)
#[test]
fn test_recovery_mode_blocks_liquidations() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, guardian, borrower, liquidator, asset, collateral_asset) = 
        setup_liquidation_policy_test(&env);

    create_underwater_position(&client, &borrower, &asset, &collateral_asset);

    // Transition to recovery
    client.emergency_shutdown(&guardian);
    client.start_recovery(&admin);
    assert_eq!(client.get_emergency_state(), EmergencyState::Recovery);

    // Liquidation should be blocked in recovery mode
    assert_eq!(
        client.try_liquidate(&liquidator, &borrower, &asset, &collateral_asset, &5_000),
        Err(Ok(BorrowError::ProtocolPaused))
    );

    // Verify unwind operations work
    client.repay(&borrower, &asset, &5_000);
    client.withdraw(&borrower, &asset, &1_000);
}

/// Test 2.3: ReadOnly mode blocks liquidations (incident freeze)
#[test]
fn test_read_only_mode_blocks_liquidations() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, guardian, borrower, liquidator, asset, collateral_asset) = 
        setup_liquidation_policy_test(&env);

    create_underwater_position(&client, &borrower, &asset, &collateral_asset);

    // Enable read-only mode
    client.set_read_only(&admin, &true);
    assert!(client.is_read_only());

    // Liquidation should be blocked in read-only mode
    assert_eq!(
        client.try_liquidate(&liquidator, &borrower, &asset, &collateral_asset, &5_000),
        Err(Ok(BorrowError::ProtocolPaused))
    );

    // Disable read-only mode
    client.set_read_only(&admin, &false);
    assert!(!client.is_read_only());

    // Liquidation should work again
    client.liquidate(&liquidator, &borrower, &asset, &collateral_asset, &5_000);
}

// ============================================================================
// SECTION 3: Precedence and Interaction Tests
// ============================================================================

/// Test 3.1: Emergency state overrides granular pause flags
#[test]
fn test_emergency_state_overrides_granular_pause_flags() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, guardian, borrower, liquidator, asset, collateral_asset) = 
        setup_liquidation_policy_test(&env);

    create_underwater_position(&client, &borrower, &asset, &collateral_asset);

    // Explicitly enable liquidations
    client.set_pause(&admin, &PauseType::Liquidation, &false);
    assert!(!client.get_pause_state(&PauseType::Liquidation));

    // Enter emergency shutdown
    client.emergency_shutdown(&guardian);
    assert_eq!(client.get_emergency_state(), EmergencyState::Shutdown);

    // Emergency state should block despite liquidation being unpaused
    assert_eq!(
        client.try_liquidate(&liquidator, &borrower, &asset, &collateral_asset, &5_000),
        Err(Ok(BorrowError::ProtocolPaused))
    );
}

/// Test 3.2: ReadOnly mode has highest precedence
#[test]
fn test_read_only_mode_highest_precedence() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, guardian, borrower, liquidator, asset, collateral_asset) = 
        setup_liquidation_policy_test(&env);

    create_underwater_position(&client, &borrower, &asset, &collateral_asset);

    // Enable read-only mode
    client.set_read_only(&admin, &true);

    // Even with all other flags allowing liquidations, read-only should block
    client.set_pause(&admin, &PauseType::Liquidation, &false);
    client.set_pause(&admin, &PauseType::All, &false);

    assert_eq!(
        client.try_liquidate(&liquidator, &borrower, &asset, &collateral_asset, &5_000),
        Err(Ok(BorrowError::ProtocolPaused))
    );
}

/// Test 3.3: Complex interaction: multiple pauses + emergency states
#[test]
fn test_complex_pause_emergency_interactions() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, guardian, borrower, liquidator, asset, collateral_asset) = 
        setup_liquidation_policy_test(&env);

    create_underwater_position(&client, &borrower, &asset, &collateral_asset);

    // Scenario 1: Normal with liquidation paused
    client.set_pause(&admin, &PauseType::Liquidation, &true);
    assert_eq!(
        client.try_liquidate(&liquidator, &borrower, &asset, &collateral_asset, &5_000),
        Err(Ok(BorrowError::ProtocolPaused))
    );

    // Scenario 2: Add global pause (redundant but should still block)
    client.set_pause(&admin, &PauseType::All, &true);
    assert_eq!(
        client.try_liquidate(&liquidator, &borrower, &asset, &collateral_asset, &5_000),
        Err(Ok(BorrowError::ProtocolPaused))
    );

    // Scenario 3: Emergency shutdown (highest precedence)
    client.emergency_shutdown(&guardian);
    assert_eq!(
        client.try_liquidate(&liquidator, &borrower, &asset, &collateral_asset, &5_000),
        Err(Ok(BorrowError::ProtocolPaused))
    );

    // Scenario 4: Recovery mode
    client.start_recovery(&admin);
    assert_eq!(
        client.try_liquidate(&liquidator, &borrower, &asset, &collateral_asset, &5_000),
        Err(Ok(BorrowError::ProtocolPaused))
    );

    // Scenario 5: Return to normal, remove all pauses
    client.complete_recovery(&admin);
    client.set_pause(&admin, &PauseType::All, &false);
    client.set_pause(&admin, &PauseType::Liquidation, &false);

    // Should work now
    client.liquidate(&liquidator, &borrower, &asset, &collateral_asset, &5_000);
}

// ============================================================================
// SECTION 4: Policy Compliance Tests
// ============================================================================

/// Test 4.1: Verify policy matrix compliance
#[test]
fn test_policy_matrix_compliance() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, guardian, borrower, liquidator, asset, collateral_asset) = 
        setup_liquidation_policy_test(&env);

    create_underwater_position(&client, &borrower, &asset, &collateral_asset);

    // Test matrix: [State] [Liquidation Pause] [Expected Result]
    
    // Normal, Liquidation Not Paused -> ALLOWED
    assert!(!client.get_pause_state(&PauseType::Liquidation));
    client.liquidate(&liquidator, &borrower, &asset, &collateral_asset, &1_000);
    
    // Reset position
    create_underwater_position(&client, &borrower, &asset, &collateral_asset);
    
    // Normal, Liquidation Paused -> BLOCKED
    client.set_pause(&admin, &PauseType::Liquidation, &true);
    assert_eq!(
        client.try_liquidate(&liquidator, &borrower, &asset, &collateral_asset, &1_000),
        Err(Ok(BorrowError::ProtocolPaused))
    );
    
    // Reset
    client.set_pause(&admin, &PauseType::Liquidation, &false);
    create_underwater_position(&client, &borrower, &asset, &collateral_asset);
    
    // Global Pause -> BLOCKED
    client.set_pause(&admin, &PauseType::All, &true);
    assert_eq!(
        client.try_liquidate(&liquidator, &borrower, &asset, &collateral_asset, &1_000),
        Err(Ok(BorrowError::ProtocolPaused))
    );
    
    // Reset
    client.set_pause(&admin, &PauseType::All, &false);
    create_underwater_position(&client, &borrower, &asset, &collateral_asset);
    
    // Emergency Shutdown -> BLOCKED
    client.emergency_shutdown(&guardian);
    assert_eq!(
        client.try_liquidate(&liquidator, &borrower, &asset, &collateral_asset, &1_000),
        Err(Ok(BorrowError::ProtocolPaused))
    );
    
    // Reset
    client.complete_recovery(&admin);
    create_underwater_position(&client, &borrower, &asset, &collateral_asset);
    
    // Recovery -> BLOCKED
    client.emergency_shutdown(&guardian);
    client.start_recovery(&admin);
    assert_eq!(
        client.try_liquidate(&liquidator, &borrower, &asset, &collateral_asset, &1_000),
        Err(Ok(BorrowError::ProtocolPaused))
    );
}

/// Test 4.2: Market health scenario - liquidations allowed when others paused
#[test]
fn test_market_health_scenario() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, guardian, borrower, liquidator, asset, collateral_asset) = 
        setup_liquidation_policy_test(&env);

    create_underwater_position(&client, &borrower, &asset, &collateral_asset);

    // Simulate market stress: pause new risk operations but allow liquidations
    client.set_pause(&admin, &PauseType::Deposit, &true);
    client.set_pause(&admin, &PauseType::Borrow, &true);
    // Deliberately NOT pausing liquidations

    // Verify market health protection works
    let debt_before = client.get_user_debt(&borrower);
    client.liquidate(&liquidator, &borrower, &asset, &collateral_asset, &5_000);
    let debt_after = client.get_user_debt(&borrower);
    
    assert!(debt_after.borrowed_amount < debt_before.borrowed_amount);
    
    // Verify new risk operations are blocked
    assert_eq!(
        client.try_deposit(&borrower, &asset, &1_000),
        Err(Ok(DepositError::DepositPaused))
    );
    assert_eq!(
        client.try_borrow(&borrower, &asset, &1_000, &collateral_asset, &2_000),
        Err(Ok(BorrowError::ProtocolPaused))
    );
}

/// Test 4.3: Solvency protection scenario - liquidations paused during oracle issues
#[test]
fn test_solvency_protection_scenario() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, guardian, borrower, liquidator, asset, collateral_asset) = 
        setup_liquidation_policy_test(&env);

    create_underwater_position(&client, &borrower, &asset, &collateral_asset);

    // Simulate oracle issues: pause liquidations to protect potentially solvent positions
    client.set_pause(&admin, &PauseType::Liquidation, &true);

    // Verify solvency protection works
    assert_eq!(
        client.try_liquidate(&liquidator, &borrower, &asset, &collateral_asset, &5_000),
        Err(Ok(BorrowError::ProtocolPaused))
    );

    // Other operations can continue (if not paused)
    client.repay(&borrower, &asset, &1_000);
    client.withdraw(&borrower, &asset, &500);
}
