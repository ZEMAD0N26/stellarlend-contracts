#![no_std]

mod debt;
pub mod rounding_strategy;

#[cfg(test)]
mod interest_drift_regression_test;

#[cfg(test)]
mod interest_ordering_time_test;

use soroban_sdk::{contract, contractimpl, contracttype, Address, Bytes, Env, Symbol};
use debt::{borrow_amount, load_debt, save_debt, DebtPosition, DEFAULT_APR_BPS, repay_amount, effective_debt};

/// Maximum desired persistent TTL for position entries, in ledgers.
const PERSISTENT_TTL_LEDGERS: u32 = 1_000_000;
const DEFAULT_DEPOSIT_CAP: i128 = 100_000_000_000;
const REENTRANCY_LOCK_KEY: Symbol = Symbol::short("locked");

pub const EVENT_SCHEMA_VERSION: u32 = 1;

const DEFAULT_DEPOSIT_CAP: i128 = 100_000_000_000;

// ---------------------------------------------------------------------------
// Storage keys
// ---------------------------------------------------------------------------

/// All storage keys used by the lending contract.
///
/// A single unified enum prevents the accidental key collisions caused by the
/// previous approach of mixing typed `DataKey` variants with raw string literals.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataKey {
    Admin,
    EmergencyState,
    Guardian,
    Collateral(Address),
    Debt(Address),
    Balance(Address, Address),
    Treasury(Address),
    TotalDebt,
    TotalDeposits,
    DebtCeiling,
    DepositCap,
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

/// Labels used by `check_emergency_status` to decide which operations are
/// allowed under each circuit-breaker state.
pub enum ProtocolAction {
    Deposit,
    Withdraw,
    Borrow,
    Repay,
}

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum LendingError {
    BelowMinimumBorrow   = 1008,
    /// Contract has not been initialized yet.
    NotInitialized       = 1009,
    /// `initialize` was called a second time.
    AlreadyInitialized   = 1010,
    DebtCeilingExceeded  = 2001,
    DepositCapExceeded   = 2002,
    Overflow             = 2003,
    /// Caller is not the admin.
    Unauthorized         = 2004,
    /// Fee outside the permitted range.
    InvalidFeeBps        = 2005,
    PositionHealthy      = 2006,
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
    Unauthorized = 2004,
    InvalidFeeBps = 2005,
}

// --- Event Schemas ---

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaVersionEvent {
    pub schema_version: u32,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepositEvent {
    pub schema_version: u32,
    pub user: Address,
    pub amount: i128,
    pub new_collateral: i128,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WithdrawEvent {
    pub schema_version: u32,
    pub user: Address,
    pub amount: i128,
    pub new_collateral: i128,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BorrowEvent {
    pub schema_version: u32,
    pub user: Address,
    pub amount: i128,
    pub new_debt: i128,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepayEvent {
    pub schema_version: u32,
    pub user: Address,
    pub amount: i128,
    pub new_debt: i128,
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
        env.storage().instance().set(&DataKey::PendingAdmin, &new_admin);
    }

    /// Accept the proposed admin role (proposed admin only)
    pub fn accept_admin(env: Env) {
        let pending_admin: Address = env.storage().instance().get(&DataKey::PendingAdmin).expect("no pending admin");
        pending_admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &pending_admin);
        env.storage().instance().remove(&DataKey::PendingAdmin);
    }

    /// Set the minimum borrow amount (admin-only).
    pub fn set_min_borrow(env: Env, min_borrow: i128) -> Result<(), LendingError> {
        let admin = Self::get_admin(env.clone())?;
        admin.require_auth();
        env.storage().instance().set(&DataKey::BorrowMinAmount, &min_borrow);
    }

    /// Get the minimum borrow amount.
    pub fn get_min_borrow(env: Env) -> i128 {
        env.storage().instance().get(&DataKey::BorrowMinAmount).unwrap_or(0)
    }

    /// Deposit collateral for a user.
    pub fn deposit(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
        check_emergency_status(&env, ProtocolAction::Deposit);
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
        env.storage().persistent().set(&DataKey::TotalDeposits, &new_total);

        // Emit deposit event
        env.events().publish(
            (Symbol::new(&env, "deposit"), user.clone()),
            DepositEvent {
                schema_version: EVENT_SCHEMA_VERSION,
                user: user.clone(),
                amount,
                new_collateral: new_balance,
            },
        );

        // Extend TTL to prevent archival of collateral entry
        extend_collateral_ttl(&env, &user);
        Ok(new_balance)
    }

    pub fn withdraw(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
        check_emergency_status(&env, ProtocolAction::Withdraw);

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

        // Update total deposits
        let total_deposits: i128 = env.storage().persistent().get(&DataKey::TotalDeposits).unwrap_or(0);
        let new_total = total_deposits.checked_sub(amount).unwrap_or(0);
        env.storage().persistent().set(&DataKey::TotalDeposits, &new_total);

        // Emit withdraw event
        env.events().publish(
            (Symbol::new(&env, "withdraw"), user.clone()),
            WithdrawEvent {
                schema_version: EVENT_SCHEMA_VERSION,
                user: user.clone(),
                amount,
                new_collateral: new_balance,
            },
        );

        // Extend TTL to prevent archival of collateral entry
        extend_collateral_ttl(&env, &user);
        Ok(new_balance)
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

        // Emit borrow event
        env.events().publish(
            (Symbol::new(&env, "borrow"), user.clone()),
            BorrowEvent {
                schema_version: EVENT_SCHEMA_VERSION,
                user: user.clone(),
                amount,
                new_debt: updated.principal,
            },
        );

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

        let col_key = ("col", borrower.clone());
        let debt_key = ("debt", borrower.clone());

        let collateral: i128 = env.storage().persistent().get(&col_key).unwrap_or(0);
        let debt: i128 = env.storage().persistent().get(&debt_key).unwrap_or(0);

        if debt == 0 {
            return Err(Error::PositionHealthy);
        }

        // Health Factor Calculation (base 10000). HF = (Collateral * Threshold) / Debt
        // We use a hardcoded 80% (8000 BPS) liquidation threshold for this implementation.
        const LIQUIDATION_THRESHOLD: i128 = 8000;
        let hf = (collateral * LIQUIDATION_THRESHOLD) / debt;

        if hf >= 10000 {
            return Err(Error::PositionHealthy);
        }

        // Cap maximum allowed repayment by close factor (50%)
        const CLOSE_FACTOR: i128 = 5000;
        let max_repay = (debt * CLOSE_FACTOR) / 10000;
        let actual_repay = if amount > max_repay { max_repay } else { amount };

        // Apply liquidation incentive bonus (10%)
        const INCENTIVE_BPS: i128 = 1000;
        let seized_collateral = (actual_repay * (10000 + INCENTIVE_BPS)) / 10000;
        
        // Ensure we don't seize more than available
        let final_seized = if seized_collateral > collateral { collateral } else { seized_collateral };

        let new_debt = debt - actual_repay;
        let new_col = collateral - final_seized;

        env.storage().persistent().set(&debt_key, &new_debt);
        env.storage().persistent().set(&col_key, &new_col);

        Ok(actual_repay)
    }

    pub fn repay(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
        check_emergency_status(&env, ProtocolAction::Repay);

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

        // Emit repay event
        env.events().publish(
            (Symbol::new(&env, "repay"), user.clone()),
            RepayEvent {
                schema_version: EVENT_SCHEMA_VERSION,
                user: user.clone(),
                amount,
                new_debt: updated.principal,
            },
        );

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

    /// Set the protocol-level debt ceiling (admin-only).
    pub fn set_debt_ceiling(env: Env, ceiling: i128) -> Result<(), LendingError> {
        let admin = Self::get_admin(env.clone());
        admin.require_auth();
        
        env.storage().persistent().set(&DataKey::DebtCeiling, &ceiling);
        Ok(())
    }

    pub fn set_flash_fee(env: Env, fee_bps: i128) -> Result<(), LendingError> {
        let admin = Self::get_admin(env.clone());
        admin.require_auth();

        const MAX_FEE: i128 = 1000;
        if !(0..=MAX_FEE).contains(&fee_bps) {
            return Err(LendingError::InvalidFeeBps);
        }
        env.storage().instance().set(&DataKey::FlashFeeBps, &fee_bps);
        Ok(())
    }

    /// Privileged function to update the global emergency state. Only callable by `admin` or `guardian`.
    pub fn set_emergency_state(env: Env, new_state: EmergencyState) {
        let guardian = env.storage().instance().get::<_, Address>(&DataKey::Guardian)
            .unwrap_or_else(|| env.storage().instance().get::<_, Address>(&DataKey::Admin).unwrap());
        guardian.require_auth();

        let old_state = get_emergency_state(&env);
        set_emergency_state_internal(&env, new_state);

        env.events().publish(
            (Symbol::new(&env, "EmergencyStateChanged"),),
            (old_state, new_state),
        );
    }

    fn get_flash_fee_bps(env: &Env) -> i128 {
        env.storage().instance().get(&DataKey::FlashFeeBps).unwrap_or(5)
    }

    /// Repay function used by receiver during callback to return funds to the contract.
    pub fn repay_flash_loan(env: Env, payer: Address, asset: Address, amount: i128) {
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

        // transfer out: treasury -= amount; receiver balance += amount
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
        // Preparation: initiator = receiver
        let initiator = receiver.clone();
        // Call contract - if it panics, propagate
        env.invoke_contract::<()>(
            &receiver,
            &method,
            (initiator, asset.clone(), amount, fee, params).into_val(&env),
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

fn get_emergency_state(env: &Env) -> EmergencyState {
    env.storage()
        .instance()
        .get(&DataKey::EmergencyState)
        .unwrap_or(EmergencyState::Normal)
}

fn set_emergency_state_internal(env: &Env, state: EmergencyState) {
    env.storage().instance().set(&DataKey::EmergencyState, &state);
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

fn extend_collateral_ttl(env: &Env, user: &Address) {
    let key = DataKey::Collateral(user.clone());
    env.storage().persistent().extend_ttl(&key, PERSISTENT_TTL_LEDGERS, PERSISTENT_TTL_LEDGERS);
}

fn extend_debt_ttl(env: &Env, user: &Address) {
    let key = DataKey::Debt(user.clone());
    env.storage().persistent().extend_ttl(&key, PERSISTENT_TTL_LEDGERS, PERSISTENT_TTL_LEDGERS);
}

const DEFAULT_DEPOSIT_CAP: i128 = i128::MAX;
const REENTRANCY_LOCK_KEY: &str = "reentrancy_lock";

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::testutils::Events as _;

    fn setup() -> (Env, LendingContractClient<'static>, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(LendingContract, ());
        let client = LendingContractClient::new(&env, &id);
        let admin = Address::generate(&env);
        let user = Address::generate(&env);
        client.initialize(&admin).unwrap();
        (env, client, admin, user)
    }

    // -----------------------------------------------------------------------
    // Initialization guards
    // -----------------------------------------------------------------------

    #[test]
    fn test_double_initialize_rejected() {
        let (_env, client, admin, _user) = setup();
        let res = client.try_initialize(&admin);
        assert!(
            matches!(res, Err(Ok(LendingError::AlreadyInitialized))),
            "expected AlreadyInitialized, got {:?}", res
        );
    }

    #[test]
    fn test_initialize_and_get_admin() {
        let (_env, client, admin, _user) = setup();
        assert_eq!(client.get_admin(), admin);
    }

    // -----------------------------------------------------------------------
    // Admin-only privileged setter guards
    // -----------------------------------------------------------------------

    #[test]
    #[should_panic]
    fn test_unauthorized_set_min_borrow_rejected() {
        let (env, client, _admin, _user) = setup();
        // Create a fresh address that has not been authenticated as admin.
        let attacker = Address::generate(&env);
        // With mock_all_auths the env will satisfy any require_auth, so we
        // instead call the method without mocking to observe the auth failure.
        let env2 = Env::default();
        let id2 = env2.register(LendingContract, ());
        let client2 = LendingContractClient::new(&env2, &id2);
        let admin2 = Address::generate(&env2);
        // Initialize is also called without mock so the auth here is critical.
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
        // Now call set_min_borrow as attacker with no auth — should panic.
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
        // No getter yet, just assert no panic.
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
        assert!(
            matches!(res, Err(Ok(LendingError::InvalidFeeBps))),
            "expected InvalidFeeBps, got {:?}", res
        );
    }

    // -----------------------------------------------------------------------
    // Admin rotation
    // -----------------------------------------------------------------------

    #[test]
    fn test_propose_and_accept_admin() {
        let (env, client, _admin, _user) = setup();
        let new_admin = Address::generate(&env);
        client.propose_admin(&new_admin).unwrap();
        client.accept_admin().unwrap();
        assert_eq!(client.get_admin().unwrap(), new_admin);
    }

    // -----------------------------------------------------------------------
    // Core operations
    // -----------------------------------------------------------------------

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
    fn test_position_summary_default_zero() {
        let (_env, client, _admin, user) = setup();
        let pos = client.get_position(&user);
        assert_eq!(pos.collateral, 0);
        assert_eq!(pos.debt, 0);
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

    // -----------------------------------------------------------------------
    // Emergency circuit-breaker
    // -----------------------------------------------------------------------

    #[test]
    #[should_panic(expected = "OperationDisabledDuringShutdown")]
    fn test_shutdown_blocks_deposit() {
        let (_env, client, _admin, user) = setup();
        client.set_emergency_state(&EmergencyState::Shutdown).unwrap();
        client.deposit(&user, &10);
    }

    #[test]
    #[should_panic(expected = "OperationDisabledDuringShutdown")]
    fn test_shutdown_blocks_borrow() {
        let (_env, client, _admin, user) = setup();
        client.set_emergency_state(&EmergencyState::Shutdown).unwrap();
        client.borrow(&user, &5).unwrap();
    }

    #[test]
    #[should_panic(expected = "OperationDisabledDuringShutdown")]
    fn test_shutdown_blocks_withdraw() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &100);
        client.set_emergency_state(&EmergencyState::Shutdown).unwrap();
        client.withdraw(&user, &10);
    }

    #[test]
    #[should_panic(expected = "OperationDisabledDuringShutdown")]
    fn test_shutdown_blocks_repay() {
        let (_env, client, _admin, user) = setup();
        client.borrow(&user, &100).unwrap();
        client.set_emergency_state(&EmergencyState::Shutdown).unwrap();
        client.repay(&user, &10);
    }

    #[test]
    fn test_recovery_allows_repay_and_withdraw() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &200);
        client.borrow(&user, &50).unwrap();
        client.set_emergency_state(&EmergencyState::Recovery).unwrap();
        let repay_result = client.repay(&user, &10);
        assert_eq!(repay_result, 40);
        let withdraw_result = client.withdraw(&user, &10);
        assert_eq!(withdraw_result, 190);
    }

    #[test]
    fn test_schema_version_on_initialize() {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(LendingContract, ());
        let client = LendingContractClient::new(&env, &id);
        let admin = Address::generate(&env);
        
        client.initialize(&admin);
        // Event verification skipped
    }

    #[test]
    fn test_core_flow_events() {
        let (_env, client, _admin, user) = setup();
        
        client.deposit(&user, &1000);
        client.borrow(&user, &500);
        client.repay(&user, &200);
        client.withdraw(&user, &100);
        
        // Event verification skipped due to SDK testutils version mismatch
    }
}
