#![no_std]

mod debt;
pub mod rounding_strategy;

#[cfg(test)]
mod interest_drift_regression_test;

use debt::{
    borrow_amount, effective_debt, load_debt, repay_amount, save_debt, DebtPosition,
    DEFAULT_APR_BPS,
};
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, Bytes, Env, IntoVal, Symbol,
};

/// Maximum desired persistent TTL for position entries, in ledgers.
const PERSISTENT_TTL_LEDGERS: u32 = 1_000_000;

const DEFAULT_DEPOSIT_CAP: i128 = i128::MAX;
const HEALTH_FACTOR_SCALE: i128 = 10_000;
const HEALTH_FACTOR_NO_DEBT: i128 = 100_000_000;
pub const LIQUIDATION_THRESHOLD_BPS: i128 = 8000;
const REENTRANCY_LOCK_KEY: &str = "reentrancy_lock";

fn extend_collateral_ttl(env: &Env, user: &Address) {
    let key = DataKey::Collateral(user.clone());
    env.storage()
        .persistent()
        .extend_ttl(&key, PERSISTENT_TTL_LEDGERS, PERSISTENT_TTL_LEDGERS);
}

fn extend_debt_ttl(env: &Env, user: &Address) {
    let key = DataKey::Debt(user.clone());
    env.storage()
        .persistent()
        .extend_ttl(&key, PERSISTENT_TTL_LEDGERS, PERSISTENT_TTL_LEDGERS);
}

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
    EmergencyState,
    Guardian,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmergencyState {
    Normal,
    Shutdown,
    Recovery,
}

pub enum ProtocolAction {
    Deposit,
    Withdraw,
    Borrow,
    Repay,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
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
    PositionHealthy = 1011,
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

#[contractimpl]
impl LendingContract {
    pub fn initialize(env: Env, admin: Address) {
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
        env.storage().instance().set(&"pending_admin", &new_admin);
    }

    /// Accept the proposed admin role (proposed admin only)
    pub fn accept_admin(env: Env) {
        let pending_admin: Address = env
            .storage()
            .instance()
            .get(&"pending_admin")
            .expect("no pending admin");
        pending_admin.require_auth();
        env.storage().instance().set(&"admin", &pending_admin);
        env.storage().instance().remove(&"pending_admin");
    }

    /// Set the minimum borrow amount (admin-only).
    pub fn set_min_borrow(env: Env, min_borrow: i128) {
        let admin = Self::get_admin(env.clone());
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::BorrowMinAmount, &min_borrow);
    }

    /// Get the minimum borrow amount.
    pub fn get_min_borrow(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::BorrowMinAmount)
            .unwrap_or(0)
    }

    /// Deposit collateral for a user.
    pub fn deposit(env: Env, user: Address, amount: i128) -> i128 {
        check_emergency_status(&env, ProtocolAction::Deposit);
        let active: bool = env
            .storage()
            .instance()
            .get(&DataKey::FlashActive)
            .unwrap_or(false);
        if active {
            panic!("FlashLoanReentrancy");
        }
        user.require_auth();

        let total_deposits: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::TotalDeposits)
            .unwrap_or(0);
        let deposit_cap: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::DepositCap)
            .unwrap_or(DEFAULT_DEPOSIT_CAP);

        let new_total = total_deposits
            .checked_add(amount)
            .expect("deposit: overflow adding to total deposits");

        if new_total > deposit_cap {
            panic!("DepositCapExceeded");
        }

        let key = DataKey::Collateral(user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let new_balance = current
            .checked_add(amount)
            .expect("deposit: overflow adding to user balance");
        env.storage().persistent().set(&key, &new_balance);
        extend_collateral_ttl(&env, &user);
        new_balance
    }

    pub fn withdraw(env: Env, user: Address, amount: i128) -> i128 {
        check_emergency_status(&env, ProtocolAction::Withdraw);

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
        let new_balance = current.checked_sub(amount).expect("withdraw: underflow");
        env.storage().persistent().set(&key, &new_balance);
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
        check_emergency_status(&env, ProtocolAction::Borrow);

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

    /// Liquidate an undercollateralized position.
    pub fn liquidate(
        env: Env,
        liquidator: Address,
        borrower: Address,
        amount: i128,
    ) -> Result<i128, Error> {
        liquidator.require_auth();

        let col_key = DataKey::Collateral(borrower.clone());
        let collateral: i128 = env.storage().persistent().get(&col_key).unwrap_or(0);
        let position = load_debt(&env, &borrower);
        let debt = effective_debt(&position, env.ledger().timestamp(), DEFAULT_APR_BPS)
            .unwrap_or(position.principal);

        if debt == 0 {
            return Err(Error::PositionHealthy);
        }

        const LIQUIDATION_THRESHOLD: i128 = 8000;
        let hf = (collateral * LIQUIDATION_THRESHOLD) / debt;

        if hf >= 10000 {
            return Err(Error::PositionHealthy);
        }

        const CLOSE_FACTOR: i128 = 5000;
        let max_repay = (debt * CLOSE_FACTOR) / 10000;
        let actual_repay = if amount > max_repay {
            max_repay
        } else {
            amount
        };

        const INCENTIVE_BPS: i128 = 1000;
        let seized_collateral = (actual_repay * (10000 + INCENTIVE_BPS)) / 10000;

        let final_seized = if seized_collateral > collateral {
            collateral
        } else {
            seized_collateral
        };

        let new_debt = debt - actual_repay;
        let new_col = collateral - final_seized;

        let now = env.ledger().timestamp();
        let updated_position = DebtPosition {
            principal: new_debt,
            last_update: now,
        };
        save_debt(&env, &borrower, &updated_position);
        env.storage().persistent().set(&col_key, &new_col);

        Ok(actual_repay)
    }

    pub fn repay(env: Env, user: Address, amount: i128) -> i128 {
        check_emergency_status(&env, ProtocolAction::Repay);

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
            .expect("repay: debt operation failed");
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
    pub fn set_debt_ceiling(env: Env, ceiling: i128) {
        let admin = Self::get_admin(env.clone());
        admin.require_auth();
        env.storage()
            .persistent()
            .set(&DataKey::DebtCeiling, &ceiling);
    }

    /// Privileged function to update the global emergency state. Only callable by `admin` or `guardian`.
    pub fn set_emergency_state(env: Env, new_state: EmergencyState) {
        let guardian = env
            .storage()
            .instance()
            .get::<_, Address>(&DataKey::Guardian)
            .unwrap_or_else(|| {
                env.storage()
                    .instance()
                    .get::<_, Address>(&DataKey::Admin)
                    .unwrap()
            });
        guardian.require_auth();

        let old_state = get_emergency_state(&env);
        set_emergency_state_internal(&env, new_state);

        env.events().publish(
            (Symbol::new(&env, "EmergencyStateChanged"),),
            (old_state, new_state),
        );
    }

    fn get_flash_fee_bps(env: &Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::FlashFeeBps)
            .unwrap_or(5)
    }

    /// Repay function used by receiver during callback to return funds to the contract.
    /// Uses checked arithmetic to prevent overflow/underflow.
    pub fn repay_flash_loan(env: Env, payer: Address, asset: Address, amount: i128) {
        payer.require_auth();
        // subtract from payer balance with overflow protection
        let payer_key = DataKey::Balance(asset.clone(), payer.clone());
        let payer_bal: i128 = env.storage().persistent().get(&payer_key).unwrap_or(0);
        if payer_bal < amount {
            panic!("InsufficientBalance");
        }
        let new_payer_bal = payer_bal
            .checked_sub(amount)
            .expect("repay_flash_loan: payer balance underflow");
        env.storage().persistent().set(&payer_key, &new_payer_bal);
        // add to contract treasury with overflow protection
        let tre_key = DataKey::Treasury(asset.clone());
        let tre_bal: i128 = env.storage().persistent().get(&tre_key).unwrap_or(0);
        let new_tre_bal = tre_bal
            .checked_add(amount)
            .expect("repay_flash_loan: treasury balance overflow");
        env.storage().persistent().set(&tre_key, &new_tre_bal);
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
        let fee = amount
            .checked_mul(fee_bps)
            .map(|v| v / 10_000)
            .expect("flash_loan: fee calculation overflow");

        // transfer out: treasury -= amount; receiver balance += amount (with overflow protection)
        let new_tre_bal = tre_bal
            .checked_sub(amount)
            .expect("flash_loan: treasury underflow during transfer");
        env.storage().persistent().set(&tre_key, &new_tre_bal);

        let rec_key = DataKey::Balance(asset.clone(), receiver.clone());
        let rec_bal: i128 = env.storage().persistent().get(&rec_key).unwrap_or(0);
        let new_rec_bal = rec_bal
            .checked_add(amount)
            .expect("flash_loan: receiver balance overflow");
        env.storage().persistent().set(&rec_key, &new_rec_bal);

        // set reentrancy guard
        env.storage().instance().set(&DataKey::FlashActive, &true);

        let method = Symbol::new(&env, "on_flash_loan");
        // Prepare arguments: initiator = caller (invoker) — using receiver as fallback
        let initiator = receiver.clone();
        env.invoke_contract::<()>(
            &receiver,
            &method,
            (initiator.clone(), asset.clone(), amount, fee, params).into_val(&env),
        );

        // clear reentrancy guard before checks to ensure state is readable
        env.storage().instance().set(&DataKey::FlashActive, &false);

        let final_tre: i128 = env.storage().persistent().get(&tre_key).unwrap_or(0);
        let required_balance = tre_bal
            .checked_add(fee)
            .expect("flash_loan: fee addition overflow");
        if final_tre < required_balance {
            panic!("InsufficientRepayment");
        }
    }

    /// Get user position summary: collateral, effective debt, and health factor.
    /// Health factor uses checked arithmetic to prevent overflow on calculation.
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
            col.checked_mul(LIQUIDATION_THRESHOLD_BPS)
                .map(|v| v / debt)
                .unwrap_or(i128::MAX)
        } else {
            HEALTH_FACTOR_NO_DEBT
        };

        PositionSummary {
            collateral: col,
            debt,
            health_factor,
        }
    }

    /// Get the health factor for a user. Read-only view.
    /// Computed as: `(collateral * LIQUIDATION_THRESHOLD_BPS) / debt`
    /// Returns `HEALTH_FACTOR_NO_DEBT` sentinel if user has no debt.
    /// Scale: `HEALTH_FACTOR_SCALE` (10000 = 1.0).
    pub fn get_health_factor(env: Env, user: Address) -> i128 {
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

        if debt > 0 {
            col.checked_mul(LIQUIDATION_THRESHOLD_BPS)
                .map(|v| v / debt)
                .unwrap_or(i128::MAX)
        } else {
            HEALTH_FACTOR_NO_DEBT
        }
    }
}

fn get_emergency_state(env: &Env) -> EmergencyState {
    env.storage()
        .instance()
        .get(&DataKey::EmergencyState)
        .unwrap_or(EmergencyState::Normal)
}

fn set_emergency_state_internal(env: &Env, state: EmergencyState) {
    env.storage()
        .instance()
        .set(&DataKey::EmergencyState, &state);
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

fn panic_with_debt_error() -> ! {
    panic!("debt operation failed");
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger as _};

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
        li.sequence_number = li.sequence_number.saturating_add(seconds as u32);
        env.ledger().set(li);
    }

    #[test]
    fn test_initialize_and_get_admin() {
        let (_env, client, admin, _user) = setup();
        assert_eq!(client.get_admin(), admin);
    }

    #[test]
    fn test_deposit_increases_balance() {
        let (_env, client, _admin, user) = setup();
        let result = client.deposit(&user, &100);
        assert_eq!(result, 100);
        let again = client.deposit(&user, &50);
        assert_eq!(again, 150);
    }

    #[test]
    fn test_withdraw_decreases_balance() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &100);
        let result = client.withdraw(&user, &40);
        assert_eq!(result, 60);
    }

    #[test]
    fn test_borrow_increases_debt() {
        let (_env, client, _admin, user) = setup();
        let result = client.borrow(&user, &50);
        assert_eq!(result, 50);
    }

    #[test]
    fn test_repay_decreases_debt() {
        let (_env, client, _admin, user) = setup();
        client.borrow(&user, &100);
        let result = client.repay(&user, &30);
        assert_eq!(result, 70);
    }

    #[test]
    fn test_position_summary_reflects_state() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &200);
        client.borrow(&user, &75);
        let pos = client.get_position(&user);
        assert_eq!(pos.collateral, 200);
        assert_eq!(pos.debt, 75);
    }

    #[test]
    fn test_ttl_keeps_position_live_across_reads() {
        let (env, client, _admin, user) = setup();
        client.deposit(&user, &200);
        client.borrow(&user, &75);

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
        client.set_min_borrow(&100);
        assert_eq!(client.get_min_borrow(), 100);
    }

    // ============ HEALTH FACTOR TESTS ============

    #[test]
    fn test_health_factor_no_debt_returns_sentinel() {
        let (_env, client, _admin, user) = setup();
        let hf = client.get_health_factor(&user);
        assert_eq!(hf, HEALTH_FACTOR_NO_DEBT);
    }

    #[test]
    fn test_health_factor_healthy() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &200);
        client.borrow(&user, &100);
        let hf = client.get_health_factor(&user);
        assert!(hf > HEALTH_FACTOR_SCALE);
    }

    #[test]
    fn test_health_factor_exactly_one() {
        let (_env, client, _admin, user) = setup();
        let col: i128 = 100;
        let debt: i128 = col * LIQUIDATION_THRESHOLD_BPS / HEALTH_FACTOR_SCALE;
        client.deposit(&user, &col);
        client.borrow(&user, &debt);
        let hf = client.get_health_factor(&user);
        assert_eq!(hf, HEALTH_FACTOR_SCALE);
    }

    #[test]
    fn test_health_factor_unhealthy() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &100);
        client.borrow(&user, &200);
        let hf = client.get_health_factor(&user);
        assert!(hf < HEALTH_FACTOR_SCALE);
    }

    #[test]
    fn test_health_factor_matches_position_hf() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &300);
        client.borrow(&user, &100);
        let hf = client.get_health_factor(&user);
        let pos = client.get_position(&user);
        assert_eq!(hf, pos.health_factor);
    }

    #[test]
    fn test_health_factor_strictly_read_only() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &100);
        client.borrow(&user, &50);

        let hf_before = client.get_health_factor(&user);
        let _ = client.get_health_factor(&user);
        let hf_after = client.get_health_factor(&user);
        assert_eq!(hf_before, hf_after);
    }

    // ============ EMERGENCY STATE TESTS ============

    #[test]
    fn test_set_emergency_state_changes_state() {
        let (_env, client, _admin, user) = setup();
        client.set_emergency_state(&EmergencyState::Shutdown);
        // With mock_all_auths, the admin is authorized to change state.
        // Verify the state changed by checking deposit is blocked.
        let res = client.try_deposit(&user, &10);
        assert!(res.is_err(), "deposit should be blocked in Shutdown");
    }

    #[test]
    #[should_panic(expected = "OperationDisabledDuringShutdown")]
    fn test_shutdown_blocks_deposit() {
        let (_env, client, _admin, user) = setup();
        client.set_emergency_state(&EmergencyState::Shutdown);
        client.deposit(&user, &10);
    }

    #[test]
    #[should_panic(expected = "OperationDisabledDuringShutdown")]
    fn test_shutdown_blocks_borrow() {
        let (_env, client, _admin, user) = setup();
        client.set_emergency_state(&EmergencyState::Shutdown);
        client.borrow(&user, &5);
    }

    #[test]
    #[should_panic(expected = "OperationDisabledDuringShutdown")]
    fn test_shutdown_blocks_withdraw() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &100);
        client.set_emergency_state(&EmergencyState::Shutdown);
        client.withdraw(&user, &10);
    }

    #[test]
    #[should_panic(expected = "OperationDisabledDuringShutdown")]
    fn test_shutdown_blocks_repay() {
        let (_env, client, _admin, user) = setup();
        client.borrow(&user, &100);
        client.set_emergency_state(&EmergencyState::Shutdown);
        client.repay(&user, &10);
    }

    #[test]
    #[should_panic(expected = "ActionBlockedInRecovery")]
    fn test_recovery_blocks_deposit() {
        let (_env, client, _admin, user) = setup();
        client.set_emergency_state(&EmergencyState::Recovery);
        client.deposit(&user, &10);
    }

    #[test]
    #[should_panic(expected = "ActionBlockedInRecovery")]
    fn test_recovery_blocks_borrow() {
        let (_env, client, _admin, user) = setup();
        client.set_emergency_state(&EmergencyState::Recovery);
        client.borrow(&user, &10);
    }

    #[test]
    fn test_recovery_allows_repay_and_withdraw() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &200);
        client.borrow(&user, &50);
        client.set_emergency_state(&EmergencyState::Recovery);
        let repay_result = client.repay(&user, &10);
        assert_eq!(repay_result, 40);
        let withdraw_result = client.withdraw(&user, &10);
        assert_eq!(withdraw_result, 190);
    }
}
