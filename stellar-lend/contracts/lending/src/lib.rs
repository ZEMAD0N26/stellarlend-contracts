#![no_std]

mod debt;
pub mod rounding_strategy;
pub mod debt;

mod debt;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod interest_drift_regression_test;

use soroban_sdk::{contract, contractimpl, contracttype, contracterror, Address, Env, Symbol, Bytes};
use crate::debt::{load_debt, save_debt, repay_amount, DEFAULT_APR_BPS, DebtPosition, effective_debt};

const REENTRANCY_LOCK_KEY: &str = "reentrancy_lock";

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

#[contract]
pub struct LendingContract;

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]  // Add Eq here
pub enum EmergencyState {
    Normal,
    Shutdown,
    Recovery,
}

impl EmergencyState {
    fn as_u32(&self) -> u32 {
        match self {
            EmergencyState::Normal => 0,
            EmergencyState::Shutdown => 1,
            EmergencyState::Recovery => 2,
        }
    }
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

    /// Deposit collateral for a user.
    pub fn deposit(env: Env, user: Address, amount: i128) -> i128 {
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
        let key = DataKey::Collateral(user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let new_balance = current.checked_add(amount).expect("collateral overflow");
        env.storage().persistent().set(&key, &new_balance);
        Ok(new_balance)
    }

    pub fn withdraw(env: Env, user: Address, amount: i128) -> i128 {
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
        let new_balance = current.checked_sub(amount).expect("collateral underflow");
        env.storage().persistent().set(&key, &new_balance);
        Ok(new_balance)
    }

    /// Borrow against deposited collateral.
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
        Ok(updated.principal)
    }

    pub fn repay(env: Env, user: Address, amount: i128) -> i128 {
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
            .unwrap_or_else(|_| panic!("repay failed"));
        save_debt(&env, &user, &updated);
        Ok(updated.principal)
    }

    pub fn get_debt_position(env: Env, user: Address) -> DebtPosition {
        load_debt(&env, &user)
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

    // Repay function used by receiver during callback to return funds to the contract.
    pub fn repay_flash_loan(env: Env, asset: Address, amount: i128) {
        // Payer must be the invoker (caller contract/account)
        let payer = Env::invoker(&env);
        payer.require_auth();
        // subtract from payer balance
        let payer_key = DataKey::Balance(asset.clone(), payer.clone());
        let payer_bal: i128 = env.storage().persistent().get(&payer_key).unwrap_or(0);
        if payer_bal < amount {
            panic!("InsufficientBalance");
        }
        env.storage()
            .persistent()
            .set(&payer_key, &(payer_bal - amount));
        // add to contract treasury
        let tre_key = DataKey::Treasury(asset.clone());
        let tre_bal: i128 = env.storage().persistent().get(&tre_key).unwrap_or(0);
        env.storage()
            .persistent()
            .set(&tre_key, &(tre_bal + amount));
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

        initiator.require_auth();
        receiver.require_auth();

        let fee_bps = Self::get_flash_fee_bps(&env);
        let fee = amount * fee_bps / 10_000;

        // transfer out: treasury -= amount; receiver balance += amount
        env.storage()
            .persistent()
            .set(&tre_key, &(tre_bal - amount));
        let rec_key = DataKey::Balance(asset.clone(), receiver.clone());
        let rec_bal: i128 = env.storage().persistent().get(&rec_key).unwrap_or(0);
        env.storage()
            .persistent()
            .set(&rec_key, &(rec_bal + amount));

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
        if final_tre < tre_bal + fee {
            panic!("InsufficientRepayment");
        }
    }

    pub fn get_position(env: Env, user: Address) -> PositionSummary {
        let col: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Collateral(user.clone()))
            .unwrap_or(0);
        let position = load_debt(&env, &user);
        let debt = effective_debt(&position, env.ledger().timestamp(), DEFAULT_APR_BPS)
            .unwrap_or(position.principal);
        
        let health_factor = if debt > 0 {
            (col * 8000) / debt
        } else {
            1000000 // Sentinel for healthy
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

    fn setup() -> (Env, LendingContractClient<'static>, Address, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(LendingContract, ());
        let client = LendingContractClient::new(&env, &id);
        let admin = Address::generate(&env);
        let user = Address::generate(&env);
        client.initialize(&admin);
        (env, client, id, admin, user)
    }

    fn setup_without_mocked_auth(
    ) -> (Env, LendingContractClient<'static>, Address, Address, Address) {
        let env = Env::default();
        let id = env.register(LendingContract, ());
        let client = LendingContractClient::new(&env, &id);
        let admin = Address::generate(&env);
        let user = Address::generate(&env);
        client.initialize(&admin);
        (env, client, id, admin, user)
    }

    #[test]
    fn test_initialize_and_get_admin() {
        let (_env, client, _id, admin, _user) = setup();
        assert_eq!(client.get_admin(), admin);
    }

    #[test]
    fn test_propose_and_accept_admin() {
        let (env, client, admin, _user) = setup();
        let new_admin = Address::generate(&env);
        
        client.propose_admin(&new_admin);
        client.accept_admin();
        
        assert_eq!(client.get_admin(), new_admin);
    }

    #[test]
    #[should_panic(expected = "no pending admin")]
    fn test_accept_without_propose() {
        let (_env, client, _admin, _user) = setup();
        client.accept_admin();
    }

    #[test]
    fn test_deposit_increases_balance() {
        let (_env, client, _id, _admin, user) = setup();
        let result = client.deposit(&user, &100);
        assert_eq!(result, 100);
        let again = client.deposit(&user, &50);
        assert_eq!(again, 150);
    }

    #[test]
    fn test_withdraw_decreases_balance() {
        let (_env, client, _id, _admin, user) = setup();
        client.deposit(&user, &100);
        let result = client.withdraw(&user, &40);
        assert_eq!(result, 60);
    }

    #[test]
    fn test_borrow_increases_debt() {
        let (_env, client, _id, _admin, user) = setup();
        let result = client.borrow(&user, &50);
        assert_eq!(result, 50);
    }

    #[test]
    fn test_repay_decreases_debt() {
        let (_env, client, _id, _admin, user) = setup();
        client.borrow(&user, &100);
        let result = client.repay(&user, &30);
        assert_eq!(result, 70);
    }

    #[test]
    fn test_position_summary_reflects_state() {
        let (_env, client, _id, _admin, user) = setup();
        client.deposit(&user, &200);
        client.borrow(&user, &75);
        let pos = client.get_position(&user);
        assert_eq!(pos.collateral, 200);
        assert_eq!(pos.debt, 75);
        assert!(pos.health_factor > 10000);
    }

    #[test]
    fn test_position_summary_default_zero() {
        let (_env, client, _id, _admin, user) = setup();
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

    #[test]
    fn test_get_admin_before_init() {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(LendingContract, ());
        let client = LendingContractClient::new(&env, &id);
        // contract not initialized, try_get_admin should return an error
        let res = client.try_get_admin();
        assert!(res.is_err());
    }

    #[test]
    fn test_double_initialize_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(LendingContract, ());
        let client = LendingContractClient::new(&env, &id);
        let admin = Address::generate(&env);
        // first initialize should succeed
        client.initialize(&admin);
        // second initialize should return an error
        let res = client.try_initialize(&admin);
        assert!(res.is_err());
    }
}


