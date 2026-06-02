#![no_std]

mod debt;
pub mod rounding_strategy;

use crate::debt::{borrow_amount, DebtPosition, effective_debt, load_debt, repay_amount, save_debt, DEFAULT_APR_BPS};
use soroban_sdk::{contract, contracterror, contractevent, contractimpl, contracttype, Address, Bytes, Env, IntoVal, Symbol, Val};

/// Maximum desired persistent TTL for position entries, in ledgers.
/// We bound the extension by the network's `max_ttl` to remain compatible
/// with runtime limits while keeping active positions alive for a long window.
const PERSISTENT_TTL_LEDGERS: u32 = 1_000_000;
const DEFAULT_DEPOSIT_CAP: i128 = 1_000_000_000_000;

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataKey {
    Collateral(Address),
    Debt(Address),
    Balance(Address, Address),
    Treasury(Address),
    TotalDebt,
    TotalDeposits,
    DebtCeiling,
    DepositCap,
    FlashActive,
    FlashFeeBps,
    BorrowMinAmount,
    Admin,
    PendingAdmin,
    EmergencyState,
    Guardian,
    PauseState(PauseType),
}

#[contractevent]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmergencyStateChangedEvent {
    pub old_state: EmergencyState,
    pub new_state: EmergencyState,
}

#[contractevent]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PauseStateChangedEvent {
    pub operation: PauseType,
    pub old_state: PauseState,
    pub new_state: PauseState,
}

#[contracttype]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmergencyState {
    Normal,
    Shutdown,
    Recovery,
}

#[contracttype]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PauseType {
    All,
    Deposit,
    Withdraw,
    Borrow,
    Repay,
    Liquidation,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PauseState {
    pub paused: bool,
    pub expires_at_ledger: u32,
}

/// Labels used by `check_pause_status` to decide which operations are
/// allowed under each circuit-breaker state.
pub enum ProtocolAction {
    Deposit,
    Withdraw,
    Borrow,
    Repay,
    Liquidate,
}

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum LendingError {
    InvalidAmount = 1004,
    BelowMinimumBorrow = 1008,
    NotInitialized = 1009,
    AlreadyInitialized = 1010,
    DebtCeilingExceeded = 2001,
    DepositCapExceeded = 2002,
    Overflow = 2003,
    Unauthorized = 2004,
    InvalidFeeBps = 2005,
    PositionHealthy = 2006,
    InsufficientCollateral = 2007,
    InvalidPauseExpiry = 2008,
    PauseNotActive = 2009,
}

// ---------------------------------------------------------------------------
// Shared view structs
// ---------------------------------------------------------------------------

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PositionSummary {
    pub collateral: i128,
    pub debt: i128,
    pub health_factor: i128,
}

#[contract]
pub struct LendingContract;

#[contractimpl]
impl LendingContract {
    pub fn initialize(env: Env, admin: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("AlreadyInitialized");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        set_emergency_state_internal(&env, EmergencyState::Normal);
    }

    pub fn get_admin(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Admin).unwrap()
    }

    /// Propose a new admin (current admin only)
    pub fn propose_admin(env: Env, new_admin: Address) {
        let current_admin = Self::get_admin(env.clone());
        current_admin.require_auth();
        env.storage().instance().set(&DataKey::PendingAdmin, &new_admin);
    }

    /// Accept the proposed admin role (proposed admin only)
    pub fn accept_admin(env: Env) {
        let pending_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .expect("no pending admin");
        pending_admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &pending_admin);
        env.storage().instance().remove(&DataKey::PendingAdmin);
    }

    /// Set the minimum borrow amount (admin-only).
    pub fn set_min_borrow(env: Env, min_borrow: i128) -> Result<(), LendingError> {
        let admin = Self::get_admin(env.clone());
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::BorrowMinAmount, &min_borrow);
        Ok(())
    }

    /// Get the minimum borrow amount.
    pub fn get_min_borrow(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::BorrowMinAmount)
            .unwrap_or(0)
    }

    pub fn get_pause_state(env: Env, operation: PauseType) -> bool {
        pause_is_active(&env, operation)
    }

    pub fn set_pause(env: Env, operation: PauseType, paused: bool, expires_at_ledger: u32) -> Result<(), LendingError> {
        let admin = Self::get_admin(env.clone());
        admin.require_auth();
        let current_seq = env.ledger().sequence();
        let old_state = get_pause_data(&env, operation);
        let new_state = if paused {
            if expires_at_ledger <= current_seq {
                return Err(LendingError::InvalidPauseExpiry);
            }
            PauseState {
                paused: true,
                expires_at_ledger,
            }
        } else {
            PauseState {
                paused: false,
                expires_at_ledger: 0,
            }
        };
        save_pause_data(&env, operation, &new_state);
        PauseStateChangedEvent {
            operation,
            old_state,
            new_state: new_state.clone(),
        }
        .publish(&env);
        Ok(())
    }

    pub fn extend_pause(env: Env, operation: PauseType, new_expiry: u32) -> Result<(), LendingError> {
        let admin = Self::get_admin(env.clone());
        admin.require_auth();
        if !pause_is_active(&env, operation) {
            return Err(LendingError::PauseNotActive);
        }
        let current_seq = env.ledger().sequence();
        if new_expiry <= current_seq {
            return Err(LendingError::InvalidPauseExpiry);
        }
        let old_state = get_pause_data(&env, operation);
        let new_state = PauseState {
            paused: true,
            expires_at_ledger: new_expiry,
        };
        save_pause_data(&env, operation, &new_state);
        PauseStateChangedEvent {
            operation,
            old_state,
            new_state: new_state.clone(),
        }
        .publish(&env);
        Ok(())
    }

    /// Set the protocol-level debt ceiling (admin-only).
    pub fn set_debt_ceiling(env: Env, ceiling: i128) -> Result<(), LendingError> {
        let admin = Self::get_admin(env.clone());
        admin.require_auth();
        if ceiling <= 0 {
            return Err(LendingError::Overflow);
        }
        env.storage().instance().set(&DataKey::DebtCeiling, &ceiling);
        Ok(())
    }

    /// Privileged function to update the global emergency state.
    /// If a guardian is configured, either the guardian or the admin may call this.
    /// If no guardian is configured, this operation is unauthorized.
    pub fn set_emergency_state(env: Env, new_state: EmergencyState) {
        let guardian_opt: Option<Address> = env.storage().instance().get(&DataKey::Guardian);
        let admin = Self::get_admin(env.clone());

        match guardian_opt {
            Some(guardian) => {
                let auths = env.auths();
                let is_admin_authorized = auths.iter().any(|(address, _)| address == &admin);
                let is_guardian_authorized = auths.iter().any(|(address, _)| address == &guardian);
                if !is_admin_authorized && !is_guardian_authorized {
                    panic!("Unauthorized");
                }
            }
            None => panic!("Unauthorized"),
        }

        let old_state = get_emergency_state(&env);
        set_emergency_state_internal(&env, new_state);

        EmergencyStateChangedEvent {
            old_state,
            new_state,
        }
        .publish(&env);
    }

    /// Set the flash loan fee in basis points.
    pub fn set_flash_fee(env: Env, bps: i128) -> Result<(), LendingError> {
        let admin = Self::get_admin(env.clone());
        admin.require_auth();
        if bps < 0 || bps > 1000 {
            return Err(LendingError::InvalidFeeBps);
        }
        env.storage().instance().set(&DataKey::FlashFeeBps, &bps);
        Ok(())
    }

    /// Deposit collateral for a user.
    pub fn deposit(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
        check_pause_status(&env, ProtocolAction::Deposit);
        check_emergency_status(&env, ProtocolAction::Deposit);

        if amount <= 0 {
            return Err(LendingError::InvalidAmount);
        }

        // Prevent mutating during an active flash loan callback
        let active: bool = env.storage().instance().get(&DataKey::FlashActive).unwrap_or(false);
        if active {
            panic!("FlashLoanReentrancy");
        }
        user.require_auth();

        // Check deposit cap with overflow protection
        let total_deposits: i128 = env.storage().persistent().get(&DataKey::TotalDeposits).unwrap_or(0);
        let deposit_cap: i128 = env.storage().persistent().get(&DataKey::DepositCap).unwrap_or(DEFAULT_DEPOSIT_CAP);

        let new_total = total_deposits.checked_add(amount).ok_or(LendingError::Overflow)?;

        if new_total > deposit_cap {
            return Err(LendingError::DepositCapExceeded);
        }

        // Update user collateral with overflow protection
        let key = DataKey::Collateral(user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let new_balance = current.checked_add(amount).ok_or(LendingError::Overflow)?;
        env.storage().persistent().set(&key, &new_balance);
        env.storage().persistent().set(&DataKey::TotalDeposits, &new_total);
        // Extend TTL to prevent archival of collateral entry
        extend_collateral_ttl(&env, &user);
        Ok(new_balance)
    }

    pub fn withdraw(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
        check_pause_status(&env, ProtocolAction::Withdraw);
        check_emergency_status(&env, ProtocolAction::Withdraw);

        if amount <= 0 {
            return Err(LendingError::InvalidAmount);
        }

        // Prevent mutating during an active flash loan callback
        let active: bool = env.storage().instance().get(&DataKey::FlashActive).unwrap_or(false);
        if active {
            panic!("FlashLoanReentrancy");
        }
        user.require_auth();
        let key = DataKey::Collateral(user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        if amount > current {
            return Err(LendingError::InsufficientCollateral);
        }
        let new_balance = current.checked_sub(amount).ok_or(LendingError::Overflow)?;
        env.storage().persistent().set(&key, &new_balance);
        // Extend TTL to prevent archival of collateral entry
        extend_collateral_ttl(&env, &user);
        Ok(new_balance)
    }

    /// Borrow against deposited collateral. Enforces protocol-level debt ceiling.
    pub fn borrow(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
        check_pause_status(&env, ProtocolAction::Borrow);
        check_emergency_status(&env, ProtocolAction::Borrow);

        if amount <= 0 {
            return Err(LendingError::InvalidAmount);
        }

        user.require_auth();
        let min_borrow = Self::get_min_borrow(env.clone());
        if amount < min_borrow {
            return Err(LendingError::BelowMinimumBorrow);
        }

        let now = env.ledger().timestamp();
        let position = load_debt(&env, &user);
        let updated = borrow_amount(position, now, amount, DEFAULT_APR_BPS).map_err(|e| match e {
            debt::DebtError::InvalidAmount => LendingError::InvalidAmount,
            debt::DebtError::Overflow => LendingError::Overflow,
        })?;
        save_debt(&env, &user, &updated);
        Ok(updated.principal)
    }

    /// Liquidate an undercollateralized position.
    pub fn liquidate(
        env: Env,
        liquidator: Address,
        borrower: Address,
        amount: i128,
    ) -> Result<i128, LendingError> {
        check_pause_status(&env, ProtocolAction::Liquidate);
        check_emergency_status(&env, ProtocolAction::Liquidate);
        liquidator.require_auth();

        let col_key = DataKey::Collateral(borrower.clone());
        let debt_key = DataKey::Debt(borrower.clone());

        let collateral: i128 = env.storage().persistent().get(&col_key).unwrap_or(0);
        let debt: i128 = env.storage().persistent().get(&debt_key).unwrap_or(0);

        if debt == 0 {
            return Err(LendingError::PositionHealthy);
        }

        const LIQUIDATION_THRESHOLD: i128 = 8000;
        let hf = (collateral * LIQUIDATION_THRESHOLD) / debt;

        if hf >= 10000 {
            return Err(LendingError::PositionHealthy);
        }

        const CLOSE_FACTOR: i128 = 5000;
        let max_repay = (debt * CLOSE_FACTOR) / 10000;
        let actual_repay = if amount > max_repay { max_repay } else { amount };

        const INCENTIVE_BPS: i128 = 1000;
        let seized_collateral = (actual_repay * (10000 + INCENTIVE_BPS)) / 10000;

        let final_seized = if seized_collateral > collateral {
            collateral
        } else {
            seized_collateral
        };

        let new_debt = debt - actual_repay;
        let new_col = collateral - final_seized;

        env.storage().persistent().set(&debt_key, &new_debt);
        env.storage().persistent().set(&col_key, &new_col);

        Ok(actual_repay)
    }

    /// Repay user debt, clamping overpayment to zero.
    pub fn repay(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
        check_pause_status(&env, ProtocolAction::Repay);
        check_emergency_status(&env, ProtocolAction::Repay);

        if amount <= 0 {
            return Err(LendingError::InvalidAmount);
        }

        let active: bool = env.storage().instance().get(&DataKey::FlashActive).unwrap_or(false);
        if active {
            panic!("FlashLoanReentrancy");
        }
        user.require_auth();
        let now = env.ledger().timestamp();
        let position = load_debt(&env, &user);
        let updated = repay_amount(position, now, amount, DEFAULT_APR_BPS).map_err(|e| match e {
            debt::DebtError::InvalidAmount => LendingError::InvalidAmount,
            debt::DebtError::Overflow => LendingError::Overflow,
        })?;
        save_debt(&env, &user, &updated);
        extend_debt_ttl(&env, &user);
        Ok(updated.principal)
    }

    pub fn get_debt_position(env: Env, user: Address) -> DebtPosition {
        let position = load_debt(&env, &user);
        if position.principal != 0 {
            extend_debt_ttl(&env, &user);
        }
        position
    }

    pub fn get_position(env: Env, user: Address) -> PositionSummary {
        let col_key = DataKey::Collateral(user.clone());
        let col: i128 = env.storage().persistent().get(&col_key).unwrap_or(0);
        if col != 0 {
            extend_collateral_ttl(&env, &user);
        }
        let position = load_debt(&env, &user);
        if position.principal != 0 {
            extend_debt_ttl(&env, &user);
        }
        let debt = effective_debt(&position, env.ledger().timestamp(), DEFAULT_APR_BPS)
            .unwrap_or(position.principal);
        let health_factor = if debt > 0 {
            col.checked_mul(8000).map(|v| v / debt).unwrap_or(i128::MAX)
        } else {
            1000000
        };

        PositionSummary {
            collateral: col,
            debt,
            health_factor,
        }
    }
}

fn acquire_reentrancy_lock(env: &Env) {
    let reentrancy_lock_key = Symbol::new(env, "reent_l");
    env.storage().temporary().set(&reentrancy_lock_key, &true);
}

fn get_pause_data(env: &Env, operation: PauseType) -> PauseState {
    env.storage()
        .instance()
        .get(&DataKey::PauseState(operation))
        .unwrap_or(PauseState {
            paused: false,
            expires_at_ledger: 0,
        })
}

fn save_pause_data(env: &Env, operation: PauseType, state: &PauseState) {
    env.storage().instance().set(&DataKey::PauseState(operation), state);
}

fn pause_is_active(env: &Env, operation: PauseType) -> bool {
    let state = get_pause_data(env, operation);
    if !state.paused {
        return false;
    }
    env.ledger().sequence() <= state.expires_at_ledger
}

fn check_pause_status(env: &Env, action: ProtocolAction) {
    if pause_is_active(env, PauseType::All) {
        panic!("OperationPaused");
    }
    let operation = match action {
        ProtocolAction::Deposit => PauseType::Deposit,
        ProtocolAction::Withdraw => PauseType::Withdraw,
        ProtocolAction::Borrow => PauseType::Borrow,
        ProtocolAction::Repay => PauseType::Repay,
        ProtocolAction::Liquidate => PauseType::Liquidation,
    };
    if pause_is_active(env, operation) {
        panic!("OperationPaused");
    }
}

fn get_flash_fee_bps(env: &Env) -> i128 {
    env.storage()
        .instance()
        .get(&DataKey::FlashFeeBps)
        .unwrap_or(5)
}

fn set_emergency_state_internal(env: &Env, state: EmergencyState) {
    env.storage().instance().set(&DataKey::EmergencyState, &state);
}

fn get_emergency_state(env: &Env) -> EmergencyState {
    env.storage()
        .instance()
        .get(&DataKey::EmergencyState)
        .unwrap_or(EmergencyState::Normal)
}

fn check_emergency_status(env: &Env, action: ProtocolAction) {
    let state = get_emergency_state(env);
    match state {
        EmergencyState::Normal => {}
        EmergencyState::Shutdown => {
            panic!("OperationDisabledDuringShutdown");
        }
        EmergencyState::Recovery => match action {
            ProtocolAction::Repay | ProtocolAction::Withdraw => {}
            _ => {
                panic!("ActionBlockedInRecovery");
            }
        },
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger};

    fn setup() -> (Env, LendingContractClient<'static>, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(LendingContract, ());
        let client = LendingContractClient::new(&env, &id);
        let admin = Address::generate(&env);
        let user = Address::generate(&env);
        client.initialize(&admin);
        (env, client, admin, user)
    }

    fn advance_time(env: &Env, seconds: u64) {
        let mut li: soroban_sdk::testutils::LedgerInfo = env.ledger().get();
        li.timestamp = li.timestamp.saturating_add(seconds);
        li.sequence_number = li.sequence_number.saturating_add(seconds as u32);
        env.ledger().set(li);
    }

    #[test]
    fn test_initialize_and_get_admin() {
        let (_env, client, admin, _user) = setup();
        assert_eq!(client.get_admin(), admin);
    }

    #[test]
    #[should_panic]
    fn test_unauthorized_set_min_borrow_rejected() {
        let (env, client, _admin, _user) = setup();
        let attacker = Address::generate(&env);
        let env2 = Env::default();
        let id2 = env2.register(LendingContract, ());
        let client2 = LendingContractClient::new(&env2, &id2);
        let admin2 = Address::generate(&env2);
        env2.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &admin2,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &id2,
                fn_name: "initialize",
                args: (admin2.clone(),).into_val(&env2),
                sub_invokes: &[],
            },
        }]);
        client2.initialize(&admin2).unwrap();
        client2.set_min_borrow(&100).unwrap();
    }

    #[test]
    fn test_set_min_borrow_admin_only() {
        let (_env, client, _admin, _user) = setup();
        assert_eq!(client.get_min_borrow(), 0);
        client.set_min_borrow(&100).unwrap();
        assert_eq!(client.get_min_borrow(), 100);
    }

    #[test]
    fn test_set_debt_ceiling_admin_only() {
        let (_env, client, _admin, _user) = setup();
        client.set_debt_ceiling(&1_000_000).unwrap();
    }

    #[test]
    fn test_set_flash_fee_valid_range() {
        let (_env, client, _admin, _user) = setup();
        client.set_flash_fee(&50).unwrap();
    }

    #[test]
    fn test_set_flash_fee_rejects_out_of_range() {
        let (_env, client, _admin, _user) = setup();
        let res = client.try_set_flash_fee(&1_001);
        assert!(matches!(res, Err(Ok(LendingError::InvalidFeeBps))), "expected InvalidFeeBps, got {:?}", res);
    }

    #[test]
    fn test_propose_and_accept_admin() {
        let (env, client, _admin, _user) = setup();
        let new_admin = Address::generate(&env);
        client.propose_admin(&new_admin).unwrap();
        client.accept_admin().unwrap();
        assert_eq!(client.get_admin().unwrap(), new_admin);
    }

    #[test]
    fn test_deposit_increases_balance() {
        let (_env, client, _admin, user) = setup();
        let result = client.deposit(&user, &100).unwrap();
        assert_eq!(result, 100);
        let again = client.deposit(&user, &50).unwrap();
        assert_eq!(again, 150);
    }

    #[test]
    fn test_withdraw_decreases_balance() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &100).unwrap();
        let result = client.withdraw(&user, &40).unwrap();
        assert_eq!(result, 60);
    }

    #[test]
    fn test_withdraw_fails_when_over_withdrawing() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &50).unwrap();
        let result = client.try_withdraw(&user, &75);
        assert!(result.is_err());
    }

    #[test]
    fn test_borrow_increases_debt() {
        let (_env, client, _admin, user) = setup();
        let result = client.borrow(&user, &50).unwrap();
        assert_eq!(result, 50);
    }

    #[test]
    fn test_repay_decreases_debt() {
        let (_env, client, _admin, user) = setup();
        client.borrow(&user, &100).unwrap();
        let result = client.repay(&user, &30).unwrap();
        assert_eq!(result, 70);
    }

    #[test]
    fn test_position_summary_reflects_state() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &200).unwrap();
        client.borrow(&user, &75).unwrap();
        let pos = client.get_position(&user);
        assert_eq!(pos.collateral, 200);
        assert_eq!(pos.debt, 75);
    }

    #[test]
    fn test_pause_auto_expires_after_ledger() {
        let (env, client, _admin, user) = setup();
        let current_seq = env.ledger().sequence();
        client.set_pause(&PauseType::Borrow, &true, &(current_seq + 2)).unwrap();
        assert!(client.get_pause_state(&PauseType::Borrow));
        assert!(client.try_borrow(&user, &10).is_err());

        advance_time(&env, 3);
        assert!(!client.get_pause_state(&PauseType::Borrow));
        client.borrow(&user, &10).unwrap();
    }

    #[test]
    fn test_extend_pause_keeps_pause_active() {
        let (env, client, _admin, user) = setup();
        let current_seq = env.ledger().sequence();
        client.set_pause(&PauseType::Borrow, &true, &(current_seq + 2)).unwrap();
        client.extend_pause(&PauseType::Borrow, &(current_seq + 10)).unwrap();

        advance_time(&env, 5);
        assert!(client.get_pause_state(&PauseType::Borrow));
        assert!(client.try_borrow(&user, &10).is_err());
    }

    #[test]
    fn test_expired_pause_is_treated_as_unpaused() {
        let (env, client, _admin, user) = setup();
        let current_seq = env.ledger().sequence();
        client.set_pause(&PauseType::Borrow, &true, &(current_seq + 1)).unwrap();

        advance_time(&env, 2);
        assert!(!client.get_pause_state(&PauseType::Borrow));
        client.borrow(&user, &10).unwrap();
    }

    #[test]
    fn test_borrow_below_minimum_rejected() {
        let (_env, client, _admin, user) = setup();
        client.set_min_borrow(&50).unwrap();
        let res = client.try_borrow(&user, &40);
        assert!(res.is_err());
    }

    #[test]
    fn test_borrow_exactly_minimum_accepted() {
        let (_env, client, _admin, user) = setup();
        client.set_min_borrow(&50).unwrap();
        let res = client.borrow(&user, &50).unwrap();
        assert_eq!(res, 50);
    }

    #[test]
    #[should_panic(expected = "Unauthorized")]
    fn test_non_guardian_cannot_set_state() {
        let (_env, client, _admin, _user) = setup();
        client.set_emergency_state(&EmergencyState::Shutdown);
    }

    #[test]
    #[should_panic(expected = "OperationDisabledDuringShutdown")]
    fn test_shutdown_blocks_deposit() {
        let (_env, client, _admin, user) = setup();
        client.set_emergency_state(&EmergencyState::Shutdown);
        client.deposit(&user, &10).unwrap();
    }

    #[test]
    #[should_panic(expected = "OperationDisabledDuringShutdown")]
    fn test_shutdown_blocks_borrow() {
        let (_env, client, _admin, user) = setup();
        client.set_emergency_state(&EmergencyState::Shutdown);
        client.borrow(&user, &5).unwrap();
    }

    #[test]
    #[should_panic(expected = "OperationDisabledDuringShutdown")]
    fn test_shutdown_blocks_withdraw() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &100).unwrap();
        client.set_emergency_state(&EmergencyState::Shutdown);
        client.withdraw(&user, &10).unwrap();
    }

    #[test]
    #[should_panic(expected = "OperationDisabledDuringShutdown")]
    fn test_shutdown_blocks_repay() {
        let (_env, client, _admin, user) = setup();
        client.borrow(&user, &100).unwrap();
        client.set_emergency_state(&EmergencyState::Shutdown);
        client.repay(&user, &10).unwrap();
    }

    #[test]
    fn test_ttl_keeps_position_live_across_reads() {
        let (env, client, _admin, user) = setup();
        client.deposit(&user, &200).unwrap();
        client.borrow(&user, &75).unwrap();

        advance_time(&env, (PERSISTENT_TTL_LEDGERS / 2) as u64);
        let pos_mid = client.get_position(&user);
        assert_eq!(pos_mid.collateral, 200);
        assert_eq!(pos_mid.debt, 75);

        advance_time(&env, (PERSISTENT_TTL_LEDGERS / 2 + 1) as u64);
        let pos_after = client.get_position(&user);
        assert_eq!(pos_after.collateral, 200);
        assert_eq!(pos_after.debt, 75);
    }

    #[test]
    fn test_get_debt_position_extends_debt_ttl() {
        let (env, client, _admin, _user) = setup();
        client.borrow(&_user, &100).unwrap();

        advance_time(&env, (PERSISTENT_TTL_LEDGERS / 2) as u64);
        let debt_mid = client.get_debt_position(&_user);
        assert_eq!(debt_mid.principal, 100);

        advance_time(&env, (PERSISTENT_TTL_LEDGERS / 2 + 1) as u64);
        let debt_after = client.get_debt_position(&_user);
        assert_eq!(debt_after.principal, 100);
    }

    #[test]
    fn test_position_summary_default_zero() {
        let (_env, client, _admin, user) = setup();
        let pos = client.get_position(&user);
        assert_eq!(pos.collateral, 0);
        assert_eq!(pos.debt, 0);
    }
}


// Flash Loan Reservation Accounting

// When a flash loan moves asset out and back, the in-flight balance temporarily
// drops while the deposit cap counter does not. We maintain a reserved counter
// per asset that is debited at the call boundary and credited on repayment.
// The deposit-cap check uses: current_balance + reserved_for_flash_loan.

use soroban_sdk::{Address, Env, Symbol};

/// DataKey extension for flash loan reservation counter.
/// Added to the existing DataKey enum:
/// ReservedForFlashLoan(Address) -> Temporary storage, ledger-scoped
///
/// Invariant: reserved_for_flash_loan(asset) <= total_deposits(asset)
///            at all times. Violation indicates a bug or attack.

/// Get the current reserved amount for flash loans on a given asset.
fn get_reserved_for_flash_loan(env: &Env, asset: &Address) -> i128 {
    env.storage()
        .temporary()
        .get(&DataKey::ReservedForFlashLoan(asset.clone()))
        .unwrap_or(0i128)
}

/// Set the reserved amount for flash loans on a given asset.
fn set_reserved_for_flash_loan(env: &Env, asset: &Address, amount: i128) {
    if amount > 0 {
        env.storage()
            .temporary()
            .set(&DataKey::ReservedForFlashLoan(asset.clone()), &amount);
    } else {
        env.storage()
            .temporary()
            .remove(&DataKey::ReservedForFlashLoan(asset.clone()));
    }
}

/// Debit the reservation counter when a flash loan is initiated.
fn reserve_flash_loan(env: &Env, asset: &Address, amount: i128) {
    let current = get_reserved_for_flash_loan(env, asset);
    let new_reserved = current.checked_add(amount)
        .expect("flash loan reservation overflow");
    
    // Invariant: reserved cannot exceed total deposits
    let total_deposits = get_total_deposits(env, asset);
    assert!(
        new_reserved <= total_deposits,
        "reserved flash loan amount exceeds total deposits"
    );
    
    set_reserved_for_flash_loan(env, asset, new_reserved);
    
    env.events().publish(
        (Symbol::new(env, "flash_loan_reserved"), asset.clone()),
        (amount, new_reserved),
    );
}

/// Credit the reservation counter when a flash loan is repaid.
fn release_flash_loan_reservation(env: &Env, asset: &Address, amount: i128) {
    let current = get_reserved_for_flash_loan(env, asset);
    assert!(
        current >= amount,
        "flash loan release exceeds reservation"
    );
    
    let new_reserved = current - amount;
    set_reserved_for_flash_loan(env, asset, new_reserved);
    
    env.events().publish(
        (Symbol::new(env, "flash_loan_released"), asset.clone()),
        (amount, new_reserved),
    );
}

/// Compute effective available deposits for cap checking.
/// This includes in-flight flash loan reservations.
fn get_effective_deposits(env: &Env, asset: &Address) -> i128 {
    let raw_deposits = get_total_deposits(env, asset);
    let reserved = get_reserved_for_flash_loan(env, asset);
    raw_deposits + reserved
}

/// Updated deposit-cap check that accounts for flash loan reservations.
fn check_deposit_cap(env: &Env, asset: &Address, additional_amount: i128) {
    let asset_params: AssetParams = env
        .storage()
        .persistent()
        .get(&DataKey::AssetParams(asset.clone()))
        .expect("asset params not set");
    
    let deposit_cap = asset_params.deposit_cap;
    if deposit_cap == 0 {
        return; // No cap configured
    }
    
    // Use effective deposits (raw + reserved) for cap calculation
    let effective_deposits = get_effective_deposits(env, asset);
    let new_total = effective_deposits
        .checked_add(additional_amount)
        .expect("deposit cap check overflow");
    
    assert!(
        new_total <= deposit_cap,
        "deposit cap exceeded: {} + {} > {}",
        effective_deposits,
        additional_amount,
        deposit_cap
    );
}

// Placeholder: get_total_deposits would be defined elsewhere in the contract
fn get_total_deposits(env: &Env, asset: &Address) -> i128 {
    env.storage()
        .persistent()
        .get(&DataKey::TotalDeposits(asset.clone()))
        .unwrap_or(0i128)
}

// Flash Loan Entrypoint (Updated)

/// Execute a flash loan with reservation accounting.
/// 
/// # Arguments
/// * `asset` - The asset to flash loan
/// * `amount` - The amount to loan
/// * `callback` - Contract to call with the loaned amount
/// * `callback_data` - Data passed to the callback contract
/// 
/// # Invariants
/// 1. reserved_for_flash_loan is debited before transfer
/// 2. Callback is invoked with loaned amount
/// 3. Repayment + fee is verified
/// 4. Reservation is credited back after repayment
pub fn flash_loan(
    env: Env,
    asset: Address,
    amount: i128,
    callback: Address,
    callback_data: soroban_sdk::Vec<Val>,
) {
    // Auth: caller must be authorized
    let caller = env.current_contract_address();
    
    // Reserve the flash loan amount against deposit cap
    reserve_flash_loan(&env, &asset, amount);
    
    // Transfer asset to callback contract
    let token_client = token::Client::new(&env, &asset);
    token_client.transfer(&caller, &callback, &amount);
    
    // Invoke callback contract
    let callback_client = FlashLoanReceiverClient::new(&env, &callback);
    callback_client.on_flash_loan(
        &caller,
        &asset,
        &amount,
        &calculate_flash_loan_fee(&env, &asset, amount),
        &callback_data,
    );
    
    // Verify repayment (amount + fee)
    let fee = calculate_flash_loan_fee(&env, &asset, amount);
    let expected_repayment = amount.checked_add(fee)
        .expect("flash loan fee overflow");
    
    let balance_after = token_client.balance(&caller);
    let balance_before = get_contract_balance(&env, &asset);
    
    assert!(
        balance_after >= balance_before + expected_repayment,
        "flash loan not repaid: expected {} + fee, got {}",
        amount,
        balance_after - balance_before
    );
    
    // Release the reservation
    release_flash_loan_reservation(&env, &asset, amount);
    
    // Emit event
    env.events().publish(
        (Symbol::new(&env, "flash_loan"), asset.clone()),
        (amount, fee, caller),
    );
}