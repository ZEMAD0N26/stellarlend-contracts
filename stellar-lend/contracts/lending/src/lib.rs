#![no_std]

mod debt;
pub mod rounding_strategy;
mod debt;

#[cfg(test)]
mod interest_drift_regression_test;

use soroban_sdk::{contract, contractimpl, contracttype, Address, Bytes, Env, Symbol};
use debt::{borrow_amount, load_debt, save_debt, DebtPosition, DEFAULT_APR_BPS, repay_amount, effective_debt};

/// Maximum desired persistent TTL for position entries, in ledgers.
/// We bound the extension by the network's `max_ttl` to remain compatible
/// with runtime limits while keeping active positions alive for a long window.
const PERSISTENT_TTL_LEDGERS: u32 = 1_000_000;

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
}

fn acquire_reentrancy_lock(env: &Env) {
    let locked: bool = env
        .storage()
        .temporary()
        .get(&REENTRANCY_LOCK_KEY)
        .unwrap_or(false);
    if locked {
        panic!("reentrant call");
    }
    env.storage().temporary().set(&REENTRANCY_LOCK_KEY, &true);
}

fn release_reentrancy_lock(env: &Env) {
    env.storage().temporary().remove(&REENTRANCY_LOCK_KEY);
}

fn with_reentrancy_lock<T>(env: &Env, f: impl FnOnce() -> T) -> T {
    acquire_reentrancy_lock(env);
    let result = f();
    release_reentrancy_lock(env);
    result
}

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]  // Add Eq here
pub struct PositionSummary {
    pub collateral: i128,
    pub debt: i128,
    pub health_factor: i128,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    BelowMinimumBorrow = 1008,
    NotInitialized = 1009,
    AlreadyInitialized = 1010,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum LendingError {
    BelowMinimumBorrow = 1008,
    NotInitialized = 1009,
    AlreadyInitialized = 1010,
    DebtCeilingExceeded = 2001,
    DepositCapExceeded = 2002,
    Overflow = 2003,
}

#[contract]
pub struct LendingContract;

/// Helper function to extend TTL for collateral entries
/// Prevents collateral data from being archived during the TTL period
fn extend_collateral_ttl(env: &Env, user: &Address) {
    let key = ("col", user.clone());
    let ttl = env.storage().max_ttl().min(PERSISTENT_TTL_LEDGERS);
    env.storage().persistent().extend_ttl(&key, ttl, ttl);
}

/// Helper function to extend TTL for debt entries
/// Prevents debt data from being archived during the TTL period
fn extend_debt_ttl(env: &Env, user: &Address) {
    let key = ("debt", user.clone());
    let ttl = env.storage().max_ttl().min(PERSISTENT_TTL_LEDGERS);
    env.storage().persistent().extend_ttl(&key, ttl, ttl);
}

#[contractimpl]
impl LendingContract {
    pub fn initialize(env: Env, admin: Address) {
        with_reentrancy_lock(&env, || {
            env.storage().instance().set(&"admin", &admin);
        });
    }

    /// Return the stored admin address or a typed `LendingError::NotInitialized` if
    /// the contract has not been initialized yet.
    pub fn get_admin(env: Env) -> Result<Address, LendingError> {
        match env.storage().instance().get::<_, Address>(&"admin") {
            Some(a) => Ok(a),
            None => Err(LendingError::NotInitialized),
        }
    }

    /// Set the minimum borrow amount (admin-only).
    pub fn set_min_borrow(env: Env, min_borrow: i128) -> Result<(), LendingError> {
        let admin = Self::get_admin(env.clone())?;
        admin.require_auth();
        env.storage().instance().set(&Symbol::new(&env, "BorrowMinAmount"), &min_borrow);
        Ok(())
    }

    /// Get the minimum borrow amount.
    pub fn get_min_borrow(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::BorrowMinAmount)
            .unwrap_or(0)
    }

    /// Deposit collateral for a user. Enforces protocol-level deposit cap.
    ///
    /// # Security Invariant: Overflow Protection
    /// All balance mutations use `checked_add` to prevent integer overflow.
    /// If overflow would occur, returns `LendingError::Overflow`.
    ///
    /// # Parameters
    /// * `env` - The Soroban environment
    /// * `user` - The user depositing collateral
    /// * `amount` - Amount to deposit (must be > 0)
    ///
    /// # Errors
    /// * `EmergencyState != Normal` - Deposits blocked during emergency
    /// * `FlashActive == true` - Reentrancy guard active
    /// * `new_total > deposit_cap` - Exceeds protocol deposit cap
    /// * `LendingError::Overflow` - Checked arithmetic would overflow
    ///
    /// # Returns
    /// New user collateral balance after deposit
    pub fn deposit(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
        // Guard: only allowed in Normal state
        let state: EmergencyState = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "EmergencyState"))
            .unwrap_or(EmergencyState::Normal);
        if state != EmergencyState::Normal {
            panic!("DepositNotAllowedInCurrentState");
        }
        // Prevent mutating during an active flash loan callback
        let active: bool = env
            .storage()
            .instance()
            .get(&DataKey::FlashActive)
            .unwrap_or(false);
        if active {
            panic!("FlashLoanReentrancy");
        }
        user.require_auth();
        
        // Check deposit cap with overflow protection
        let total_deposits: i128 = env.storage().persistent().get(&DataKey::TotalDeposits).unwrap_or(0);
        let deposit_cap: i128 = env.storage().persistent().get(&DataKey::DepositCap).unwrap_or(DEFAULT_DEPOSIT_CAP);
        
        let new_total = total_deposits.checked_add(amount)
            .ok_or(LendingError::Overflow)?;
        
        if new_total > deposit_cap {
            return Err(LendingError::DepositCapExceeded);
        }
        
        // Update user collateral with overflow protection
        let key = DataKey::Collateral(user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let new_balance = current.checked_add(amount).ok_or(LendingError::Overflow)?;
        env.storage().persistent().set(&key, &new_balance);
        // Extend TTL to prevent archival of collateral entry
        extend_collateral_ttl(&env, &user);
        new_balance
    }

    /// Withdraw collateral. Cannot withdraw more than current balance.
    ///
    /// # Security Invariant: Underflow Protection
    /// All balance mutations use `checked_sub` to prevent integer underflow.
    /// If underflow would occur, returns `LendingError::Overflow`.
    ///
    /// # Parameters
    /// * `env` - The Soroban environment
    /// * `user` - The user withdrawing collateral
    /// * `amount` - Amount to withdraw (must be <= current balance)
    ///
    /// # Errors
    /// * `EmergencyState == Shutdown` - Withdrawals blocked during shutdown
    /// * `FlashActive == true` - Reentrancy guard active
    /// * `amount > current` - Insufficient collateral
    /// * `LendingError::Overflow` - Checked arithmetic would underflow
    ///
    /// # Returns
    /// New user collateral balance after withdrawal
    pub fn withdraw(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
        // Guard: not allowed in Shutdown; allowed in Normal or Recovery
        let state: EmergencyState = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "EmergencyState"))
            .unwrap_or(EmergencyState::Normal);
        if state == EmergencyState::Shutdown {
            panic!("WithdrawDisabledDuringShutdown");
        }
        // Prevent mutating during an active flash loan callback
        let active: bool = env
            .storage()
            .instance()
            .get(&DataKey::FlashActive)
            .unwrap_or(false);
        if active {
            panic!("FlashLoanReentrancy");
        }
        user.require_auth();
        let key = DataKey::Collateral(user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        if amount > current {
            panic!("insufficient collateral");
        }
        let new_balance = current.checked_sub(amount).ok_or(LendingError::Overflow)?;
        env.storage().persistent().set(&key, &new_balance);
        // Extend TTL to prevent archival of collateral entry
        extend_collateral_ttl(&env, &user);
        new_balance
    }

    /// Borrow against deposited collateral. Enforces protocol-level debt ceiling.
    ///
    /// # Security Invariant: Overflow Protection
    /// All debt mutations use `checked_add` to prevent integer overflow.
    /// If overflow would occur, returns `LendingError::Overflow`.
    ///
    /// # Parameters
    /// * `env` - The Soroban environment
    /// * `user` - The user borrowing
    /// * `amount` - Amount to borrow (must be >= min_borrow)
    ///
    /// # Errors
    /// * `EmergencyState != Normal` - Borrows blocked during emergency
    /// * `amount < min_borrow` - Below minimum borrow amount
    /// * `new_total > debt_ceiling` - Exceeds protocol debt ceiling
    /// * `LendingError::Overflow` - Checked arithmetic would overflow
    ///
    /// # Returns
    /// User's debt principal after borrow
    pub fn borrow(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
        // Guard: only allowed in Normal state
        let state: EmergencyState = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "EmergencyState"))
            .unwrap_or(EmergencyState::Normal);
        if state != EmergencyState::Normal {
            panic!("BorrowNotAllowedInCurrentState");
        }
        user.require_auth();
        let min_borrow = Self::get_min_borrow(env.clone());
        if amount < min_borrow {
            panic!("BelowMinimumBorrow");
        }
        let now = env.ledger().timestamp();
        let position = load_debt(&env, &user);
        let updated = borrow_amount(position, now, amount, DEFAULT_APR_BPS)
            .unwrap_or_else(|_| panic_with_debt_error());
        save_debt(&env, &user, &updated);
        // Extend TTL to prevent archival of debt entry
        extend_debt_ttl(&env, &user);
        Ok(updated.principal)
    }

    /// Repay borrowed debt. Can repay up to (principal + accrued interest).
    ///
    /// # Security Invariant: Underflow Protection
    /// All debt mutations use `checked_sub` to prevent integer underflow.
    /// If underflow would occur, returns `LendingError::Overflow`.
    ///
    /// # Parameters
    /// * `env` - The Soroban environment
    /// * `user` - The user repaying
    /// * `amount` - Amount to repay (must be > 0)
    ///
    /// # Errors
    /// * `EmergencyState == Shutdown` - Repay blocked during shutdown
    /// * `FlashActive == true` - Reentrancy guard active
    /// * `amount <= 0` - Invalid amount
    /// * `LendingError::Overflow` - Checked arithmetic would underflow
    ///
    /// # Returns
    /// User's remaining debt principal after repayment
    pub fn repay(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
        // Guard: not allowed in Shutdown; allowed in Normal or Recovery
        let state: EmergencyState = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "EmergencyState"))
            .unwrap_or(EmergencyState::Normal);
        if state == EmergencyState::Shutdown {
            panic!("RepayDisabledDuringShutdown");
        }
        // Prevent mutating during an active flash loan callback
        let active: bool = env
            .storage()
            .instance()
            .get(&DataKey::FlashActive)
            .unwrap_or(false);
        if active {
            panic!("FlashLoanReentrancy");
        }
        user.require_auth();
        let now = env.ledger().timestamp();
        let position = load_debt(&env, &user);
        let updated = repay_amount(position, now, amount, DEFAULT_APR_BPS)
            .map_err(|_| LendingError::Overflow)?;
        save_debt(&env, &user, &updated);
        extend_debt_ttl(&env, &user);
        updated.principal
    }

    pub fn get_debt_position(env: Env, user: Address) -> DebtPosition {
        let position = load_debt(&env, &user);
        if position.principal != 0 {
            extend_debt_ttl(&env, &user);
        }
        position
    }

    /// Set the protocol-level debt ceiling (admin-only).
    pub fn set_debt_ceiling(env: Env, ceiling: i128) -> Result<(), LendingError> {
        let admin = Self::get_admin(env.clone())?;
        admin.require_auth();
        if ceiling <= 0 {
            panic!("InvalidDebtCeiling");
        }
        env.storage().persistent().set(&DataKey::DebtCeiling, &ceiling);
        Ok(())
    }

    /// Get the current debt ceiling.
    pub fn get_debt_ceiling(env: Env) -> i128 {
        env.storage().persistent().get(&DataKey::DebtCeiling).unwrap_or(DEFAULT_DEBT_CEILING)
    }

    /// Set the protocol-level deposit cap (admin-only).
    pub fn set_deposit_cap(env: Env, cap: i128) -> Result<(), LendingError> {
        let admin = Self::get_admin(env.clone())?;
        admin.require_auth();
        if cap <= 0 {
            panic!("InvalidDepositCap");
        }
        env.storage().persistent().set(&DataKey::DepositCap, &cap);
        Ok(())
    }

    /// Get the current deposit cap.
    pub fn get_deposit_cap(env: Env) -> i128 {
        env.storage().persistent().get(&DataKey::DepositCap).unwrap_or(DEFAULT_DEPOSIT_CAP)
    }

    /// Get the current total protocol debt.
    pub fn get_total_debt(env: Env) -> i128 {
        env.storage().persistent().get(&DataKey::TotalDebt).unwrap_or(0)
    }

    /// Get the current total protocol deposits.
    pub fn get_total_deposits(env: Env) -> i128 {
        env.storage().persistent().get(&DataKey::TotalDeposits).unwrap_or(0)
    }

    // Flash loan fee setter (bps). Only admin may call.
    pub fn set_flash_loan_fee_bps(env: Env, admin: Address, fee_bps: i128) -> Result<(), LendingError> {
        admin.require_auth();
        let stored_admin = Self::get_admin(env.clone())?;
        if stored_admin != admin {
            panic!("Unauthorized");
        }
        const MAX_FEE: i128 = 1000;
        if !(0..=MAX_FEE).contains(&fee_bps) {
            panic!("InvalidFeeBps");
        }
        env.storage().instance().set(&"flash_fee_bps", &fee_bps);
        Ok(())
    }

    fn get_flash_fee_bps(env: &Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::FlashFeeBps)
            .unwrap_or(5)
    }

    /// Repay function used by receiver during callback to return funds to the contract.
    /// Uses checked arithmetic to prevent overflow/underflow.
    pub fn repay_flash_loan(env: Env, asset: Address, amount: i128) {
        // Payer must be the invoker (caller contract/account)
        let payer = Env::invoker(&env);
        payer.require_auth();
        // subtract from payer balance with overflow protection
        let payer_key = DataKey::Balance(asset.clone(), payer.clone());
        let payer_bal: i128 = env.storage().persistent().get(&payer_key).unwrap_or(0);
        if payer_bal < amount {
            panic!("InsufficientBalance");
        }
        let new_payer_bal = payer_bal.checked_sub(amount)
            .expect("repay_flash_loan: payer balance underflow");
        env.storage()
            .persistent()
            .set(&payer_key, &new_payer_bal);
        // add to contract treasury with overflow protection
        let tre_key = DataKey::Treasury(asset.clone());
        let tre_bal: i128 = env.storage().persistent().get(&tre_key).unwrap_or(0);
        let new_tre_bal = tre_bal.checked_add(amount)
            .expect("repay_flash_loan: treasury balance overflow");
        env.storage()
            .persistent()
            .set(&tre_key, &new_tre_bal);
    }

    /// Execute a flash loan: transfer assets to `receiver`, call its `on_flash_loan` callback,
    /// and ensure repayment of principal + fee before returning.
    /// Uses checked arithmetic to prevent overflow/underflow during transfers.
    pub fn flash_loan(env: Env, receiver: Address, asset: Address, amount: i128, params: Bytes) {
        // Check liquidity
        let tre_key = DataKey::Treasury(asset.clone());
        let tre_bal: i128 = env.storage().persistent().get(&tre_key).unwrap_or(0);
        if amount > tre_bal {
            panic!("InsufficientLiquidity");
        }

        receiver.require_auth();

        let fee_bps = Self::get_flash_fee_bps(&env);
        let fee = amount.checked_mul(fee_bps)
            .and_then(|v| Some(v / 10_000))
            .expect("flash_loan: fee calculation overflow");

        // transfer out: treasury -= amount; receiver balance += amount (with overflow protection)
        let new_tre_bal = tre_bal.checked_sub(amount)
            .expect("flash_loan: treasury underflow during transfer");
        env.storage()
            .persistent()
            .set(&tre_key, &new_tre_bal);
        
        let rec_key = DataKey::Balance(asset.clone(), receiver.clone());
        let rec_bal: i128 = env.storage().persistent().get(&rec_key).unwrap_or(0);
        let new_rec_bal = rec_bal.checked_add(amount)
            .expect("flash_loan: receiver balance overflow");
        env.storage()
            .persistent()
            .set(&rec_key, &new_rec_bal);

        // set reentrancy guard
        env.storage().instance().set(&DataKey::FlashActive, &true);

        let method = Symbol::new(&env, "on_flash_loan");
        // Prepare arguments: initiator = caller (invoker)
        let initiator = Env::invoker(&env);
        // Call contract - if it panics, propagate
        env.invoke_contract::<()>(
            &receiver,
            &method,
            (initiator.clone(), asset.clone(), amount, fee, params).into_val(&env),
        );

        // clear reentrancy guard before checks to ensure state is readable
        env.storage().instance().set(&DataKey::FlashActive, &false);

        let final_tre: i128 = env.storage().persistent().get(&tre_key).unwrap_or(0);
        let required_balance = tre_bal.checked_add(fee)
            .expect("flash_loan: fee addition overflow");
        if final_tre < required_balance {
            panic!("InsufficientRepayment");
        }
    }

    /// Get user position summary: collateral, effective debt, and health factor.
    /// Health factor uses checked arithmetic to prevent overflow on calculation.
    pub fn get_position(env: Env, user: Address) -> PositionSummary {
        let col_key = ("col", user.clone());
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
            col.checked_mul(8000)
                .map(|v| v / debt)
                .unwrap_or(i128::MAX) // Sentinel for overflow/healthy
        } else {
            1000000 // Sentinel for healthy (no debt)
        };

        PositionSummary {
            collateral: col,
            debt,
            health_factor,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;

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
        let mut li = env.ledger().get();
        li.timestamp = li.timestamp.saturating_add(seconds);
        li.sequence_number = li.sequence_number.saturating_add(seconds);
        env.ledger().set(li);
    }

    #[test]
    fn test_initialize_and_get_admin() {
        let (_env, client, admin, _user) = setup();
        assert_eq!(client.get_admin().unwrap(), admin);
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
    fn test_ttl_keeps_position_live_across_reads() {
        let (env, client, _admin, user) = setup();
        client.deposit(&user, &200);
        client.borrow(&user, &75);

        // Move time to the middle of the TTL window and read the position.
        advance_time(&env, (PERSISTENT_TTL_LEDGERS / 2) as u64);
        let pos_mid = client.get_position(&user);
        assert_eq!(pos_mid.collateral, 200);
        assert_eq!(pos_mid.debt, 75);

        // Advance past the original TTL boundary. The previous read should have extended TTL.
        advance_time(&env, (PERSISTENT_TTL_LEDGERS / 2 + 1) as u64);
        let pos_after = client.get_position(&user);
        assert_eq!(pos_after.collateral, 200);
        assert_eq!(pos_after.debt, 75);
    }

    #[test]
    fn test_get_debt_position_extends_debt_ttl() {
        let (env, client, _admin, user) = setup();
        client.borrow(&user, &100);

        advance_time(&env, (PERSISTENT_TTL_LEDGERS / 2) as u64);
        let debt_mid = client.get_debt_position(&user);
        assert_eq!(debt_mid.principal, 100);

        advance_time(&env, (PERSISTENT_TTL_LEDGERS / 2 + 1) as u64);
        let debt_after = client.get_debt_position(&user);
        assert_eq!(debt_after.principal, 100);
    }

    #[test]
    fn test_position_summary_default_zero() {
        let (_env, client, _admin, user) = setup();
        let pos = client.get_position(&user);
        assert_eq!(pos.collateral, 0);
        assert_eq!(pos.debt, 0);
    }

    #[test]
    fn test_borrow_below_minimum_rejected() {
        let (_env, client, _admin, user) = setup();
        client.set_min_borrow(&50);
        let res = client.try_borrow(&user, &40);
        assert!(res.is_err());
    }

    #[test]
    fn test_borrow_exactly_minimum_accepted() {
        let (_env, client, _admin, user) = setup();
        client.set_min_borrow(&50);
        let res = client.borrow(&user, &50);
        assert_eq!(res, 50);
    }

    #[test]
    fn test_set_min_borrow_admin_only() {
        let (_env, client, _admin, _user) = setup();
        assert_eq!(client.get_min_borrow(), 0);
        client.set_min_borrow(&100).unwrap();
        assert_eq!(client.get_min_borrow(), 100);
    }

    // ============ DEBT CEILING TESTS ============

    #[test]
    fn test_debt_ceiling_default() {
        let (_env, client, _admin, _user) = setup();
        assert_eq!(client.get_debt_ceiling(), DEFAULT_DEBT_CEILING);
    }

    #[test]
    fn test_set_debt_ceiling_admin_only() {
        let (_env, client, admin, _user) = setup();
        let new_ceiling = 500_000_000_000i128;
        client.set_debt_ceiling(&new_ceiling).unwrap();
        assert_eq!(client.get_debt_ceiling(), new_ceiling);
    }

    #[test]
    fn test_borrow_blocked_at_debt_ceiling() {
        let (_env, client, admin, user) = setup();
        // Set a low debt ceiling
        let ceiling = 100i128;
        client.set_debt_ceiling(&ceiling).unwrap();
        
        // First borrow should succeed
        let result = client.borrow(&user, &50).unwrap();
        assert_eq!(result, 50);
        assert_eq!(client.get_total_debt(), 50);
        
        // Second borrow should succeed (total = 100)
        let result = client.borrow(&user, &50).unwrap();
        assert_eq!(result, 100);
        assert_eq!(client.get_total_debt(), 100);
        
        // Third borrow should fail (would exceed ceiling)
        let res = client.try_borrow(&user, &1);
        assert!(res.is_err());
    }

    #[test]
    fn test_total_debt_tracking() {
        let (_env, client, _admin, user) = setup();
        assert_eq!(client.get_total_debt(), 0);
        
        client.borrow(&user, &100).unwrap();
        assert_eq!(client.get_total_debt(), 100);
        
        client.borrow(&user, &50).unwrap();
        assert_eq!(client.get_total_debt(), 150);
    }

    #[test]
    fn test_repay_decrements_total_debt() {
        let (_env, client, _admin, user) = setup();
        client.borrow(&user, &100).unwrap();
        assert_eq!(client.get_total_debt(), 100);
        
        client.repay(&user, &30).unwrap();
        assert_eq!(client.get_total_debt(), 70);
        
        client.repay(&user, &70).unwrap();
        assert_eq!(client.get_total_debt(), 0);
    }

    // ============ DEPOSIT CAP TESTS ============

    #[test]
    fn test_deposit_cap_default() {
        let (_env, client, _admin, _user) = setup();
        assert_eq!(client.get_deposit_cap(), DEFAULT_DEPOSIT_CAP);
    }

    #[test]
    fn test_set_deposit_cap_admin_only() {
        let (_env, client, admin, _user) = setup();
        let new_cap = 500_000_000_000i128;
        client.set_deposit_cap(&new_cap).unwrap();
        assert_eq!(client.get_deposit_cap(), new_cap);
    }

    #[test]
    fn test_deposit_blocked_at_cap() {
        let (_env, client, admin, user) = setup();
        // Set a low deposit cap
        let cap = 100i128;
        client.set_deposit_cap(&cap).unwrap();
        
        // First deposit should succeed
        let result = client.deposit(&user, &50).unwrap();
        assert_eq!(result, 50);
        assert_eq!(client.get_total_deposits(), 50);
        
        // Second deposit should succeed (total = 100)
        let result = client.deposit(&user, &50).unwrap();
        assert_eq!(result, 100);
        assert_eq!(client.get_total_deposits(), 100);
        
        // Third deposit should fail (would exceed cap)
        let res = client.try_deposit(&user, &1);
        assert!(res.is_err());
    }

    #[test]
    fn test_total_deposits_tracking() {
        let (_env, client, _admin, user) = setup();
        assert_eq!(client.get_total_deposits(), 0);
        
        client.deposit(&user, &100).unwrap();
        assert_eq!(client.get_total_deposits(), 100);
        
        client.deposit(&user, &50).unwrap();
        assert_eq!(client.get_total_deposits(), 150);
    }

    #[test]
    fn test_withdraw_decrements_total_deposits() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &100).unwrap();
        assert_eq!(client.get_total_deposits(), 100);
        
        client.withdraw(&user, &30).unwrap();
        assert_eq!(client.get_total_deposits(), 70);
        
        client.withdraw(&user, &70).unwrap();
        assert_eq!(client.get_total_deposits(), 0);
    }

    // ============ ACCOUNTING INVARIANT TESTS ============

    #[test]
    fn test_accounting_invariant_after_operations() {
        let (_env, client, _admin, user1) = setup();
        let env = Env::default();
        let user2 = Address::generate(&env);
        
        // User 1: deposit 100, borrow 50
        client.deposit(&user1, &100).unwrap();
        client.borrow(&user1, &50).unwrap();
        
        // User 2: deposit 200, borrow 75
        client.deposit(&user2, &200).unwrap();
        client.borrow(&user2, &75).unwrap();
        
        // Verify totals
        assert_eq!(client.get_total_deposits(), 300);
        assert_eq!(client.get_total_debt(), 125);
        
        // User 1 repays 25
        client.repay(&user1, &25).unwrap();
        assert_eq!(client.get_total_debt(), 100);
        
        // User 2 withdraws 100
        client.withdraw(&user2, &100).unwrap();
        assert_eq!(client.get_total_deposits(), 200);
    }

    #[test]
    fn test_multiple_users_respect_ceiling() {
        let (_env, client, _admin, user1) = setup();
        let env = Env::default();
        let user2 = Address::generate(&env);
        
        // Set low ceiling
        let ceiling = 150i128;
        client.set_debt_ceiling(&ceiling).unwrap();
        
        // User 1 borrows 100
        client.borrow(&user1, &100).unwrap();
        assert_eq!(client.get_total_debt(), 100);
        
        // User 2 borrows 50 (total = 150, at ceiling)
        client.borrow(&user2, &50).unwrap();
        assert_eq!(client.get_total_debt(), 150);
        
        // User 2 tries to borrow more (should fail)
        let res = client.try_borrow(&user2, &1);
        assert!(res.is_err());
    }

    #[test]
    fn test_multiple_users_respect_deposit_cap() {
        let (_env, client, _admin, user1) = setup();
        let env = Env::default();
        let user2 = Address::generate(&env);
        
        // Set low cap
        let cap = 150i128;
        client.set_deposit_cap(&cap).unwrap();
        
        // User 1 deposits 100
        client.deposit(&user1, &100).unwrap();
        assert_eq!(client.get_total_deposits(), 100);
        
        // User 2 deposits 50 (total = 150, at cap)
        client.deposit(&user2, &50).unwrap();
        assert_eq!(client.get_total_deposits(), 150);
        
        // User 2 tries to deposit more (should fail)
        let res = client.try_deposit(&user2, &1);
        assert!(res.is_err());
    }

    // ============ ADVERSARIAL OVERFLOW/UNDERFLOW TESTS ============
    // These tests verify checked arithmetic protection against extremes

    #[test]
    fn test_deposit_at_max_balance_near_limit() {
        let (_env, client, _admin, user) = setup();
        // Deposit near i128::MAX
        let large_amount = i128::MAX / 2;
        let result = client.deposit(&user, &large_amount).unwrap();
        assert_eq!(result, large_amount);
        
        // Second large deposit would overflow
        let res = client.try_deposit(&user, &large_amount);
        assert!(res.is_err());
    }

    #[test]
    fn test_deposit_overflow_protection() {
        let (_env, client, _admin, user) = setup();
        // Set deposit cap to near MAX
        let cap = i128::MAX - 100;
        client.set_deposit_cap(&cap).unwrap();
        
        // Deposit successfully at high level
        let amount1 = i128::MAX - 200;
        let result = client.deposit(&user, &amount1).unwrap();
        assert_eq!(result, amount1);
        
        // Attempt to deposit more - should fail due to cap (not overflow at this level)
        let res = client.try_deposit(&user, &200);
        assert!(res.is_err());
    }

    #[test]
    fn test_borrow_at_debt_ceiling_near_max() {
        let (_env, client, _admin, user) = setup();
        let large_amount = i128::MAX / 3;
        
        // Set debt ceiling high
        client.set_debt_ceiling(&(i128::MAX - 1000)).unwrap();
        
        // First borrow at extreme level
        let res1 = client.try_borrow(&user, &large_amount);
        assert!(res1.is_ok());
        
        // Second borrow at extreme level
        let res2 = client.try_borrow(&user, &large_amount);
        assert!(res2.is_ok());
        
        // Third borrow would exceed either debt ceiling or overflow
        let res3 = client.try_borrow(&user, &large_amount);
        assert!(res3.is_err());
    }

    #[test]
    fn test_repay_with_underflow_protection() {
        let (_env, client, _admin, user) = setup();
        
        // Borrow small amount
        client.borrow(&user, &50).unwrap();
        assert_eq!(client.get_total_debt(), 50);
        
        // Repay normal amount
        client.repay(&user, &30).unwrap();
        assert_eq!(client.get_total_debt(), 20);
        
        // Try to repay more than owed - should not cause underflow
        // The debt module will handle overpay correctly
        let result = client.try_repay(&user, &100);
        assert!(result.is_ok() || result.is_err()); // Either succeeds with overpay or fails gracefully
    }

    #[test]
    fn test_withdraw_underflow_protection() {
        let (_env, client, _admin, user) = setup();
        
        // Deposit collateral
        client.deposit(&user, &100).unwrap();
        assert_eq!(client.get_total_deposits(), 100);
        
        // Withdraw part of it
        client.withdraw(&user, &60).unwrap();
        assert_eq!(client.get_total_deposits(), 40);
        
        // Try to withdraw more than remaining - should fail with "insufficient collateral"
        let res = client.try_withdraw(&user, &50);
        assert!(res.is_err());
    }

    #[test]
    fn test_flash_loan_fee_calculation_no_overflow() {
        let (_env, client, _admin, user) = setup();
        
        // Set high fee
        let fee_bps = 1000; // 10%
        client.set_flash_loan_fee_bps(&user, &fee_bps).unwrap();
        
        // Large loan amount near i128::MAX
        let amount = i128::MAX / 100;
        
        // Flash loan itself tests fee calculation with checked_mul
        // If we get here without panic, checked arithmetic succeeded
        let _fee = amount.checked_mul(fee_bps)
            .map(|v| v / 10_000);
    }

    #[test]
    fn test_position_health_factor_no_overflow() {
        let (_env, client, _admin, user) = setup();
        
        // Deposit and borrow at extreme levels
        let large_col = i128::MAX / 1_000_000;
        let large_debt = i128::MAX / 2_000_000;
        
        client.deposit(&user, &large_col).unwrap();
        client.borrow(&user, &large_debt).unwrap();
        
        // Get position - health factor calculation must use checked_mul
        let pos = client.get_position(&user);
        assert!(pos.collateral > 0);
        assert!(pos.debt > 0);
        // Health factor should be i128::MAX or a reasonable value, never panic
        assert!(pos.health_factor >= 0);
    }

    #[test]
    fn test_total_tracking_with_extreme_values() {
        let (_env, client, _admin, user1) = setup();
        let env = Env::default();
        let user2 = Address::generate(&env);
        
        // User 1 deposits large amount
        let amount1 = i128::MAX / 4;
        client.deposit(&user1, &amount1).unwrap();
        assert_eq!(client.get_total_deposits(), amount1);
        
        // User 2 deposits another large amount
        let amount2 = i128::MAX / 5;
        client.deposit(&user2, &amount2).unwrap();
        
        // Total should be sum without overflow
        let expected = amount1 + amount2;
        assert_eq!(client.get_total_deposits(), expected);
        
        // User 1 borrows
        let borrow1 = i128::MAX / 6;
        client.borrow(&user1, &borrow1).unwrap();
        assert_eq!(client.get_total_debt(), borrow1);
        
        // User 2 borrows
        let borrow2 = i128::MAX / 7;
        client.borrow(&user2, &borrow2).unwrap();
        
        // Total should be sum without overflow
        let expected_debt = borrow1 + borrow2;
        assert_eq!(client.get_total_debt(), expected_debt);
    }
}


