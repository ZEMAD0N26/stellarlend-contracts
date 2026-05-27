#![no_std]

mod debt;
mod rounding_strategy;

use debt::{
    borrow_amount, effective_debt, load_debt, repay_amount, save_debt, DEFAULT_APR_BPS,
};

pub use debt::DebtPosition;
use soroban_sdk::{contract, contractimpl, contracttype, Address, Env};

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PositionSummary {
    pub collateral: i128,
    pub debt: i128,
}

#[contract]
pub struct LendingContract;

#[contractimpl]
impl LendingContract {
    pub fn initialize(env: Env, admin: Address) {
        env.storage().instance().set(&"admin", &admin);
    }

    pub fn get_admin(env: Env) -> Address {
        env.storage().instance().get(&"admin").unwrap()
    }

    pub fn deposit(env: Env, user: Address, amount: i128) -> i128 {
        user.require_auth();
        let key = ("col", user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let new_balance = current + amount;
        env.storage().persistent().set(&key, &new_balance);
        new_balance
    }

    pub fn withdraw(env: Env, user: Address, amount: i128) -> i128 {
        user.require_auth();
        let key = ("col", user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let new_balance = current - amount;
        env.storage().persistent().set(&key, &new_balance);
        new_balance
    }

    pub fn borrow(env: Env, user: Address, amount: i128) -> i128 {
        user.require_auth();
        let now = env.ledger().timestamp();
        let position = load_debt(&env, &user);
        let updated = borrow_amount(position, now, amount, DEFAULT_APR_BPS)
            .unwrap_or_else(|_| panic_with_debt_error());
        save_debt(&env, &user, &updated);
        updated.principal
    }

    pub fn repay(env: Env, user: Address, amount: i128) -> i128 {
        user.require_auth();
        let now = env.ledger().timestamp();
        let position = load_debt(&env, &user);
        let updated = repay_amount(position, now, amount, DEFAULT_APR_BPS)
            .unwrap_or_else(|_| panic_with_debt_error());
        save_debt(&env, &user, &updated);
        updated.principal
    }

    pub fn get_debt_position(env: Env, user: Address) -> DebtPosition {
        load_debt(&env, &user)
    }

    pub fn get_position(env: Env, user: Address) -> PositionSummary {
        let col: i128 = env
            .storage()
            .persistent()
            .get(&("col", user.clone()))
            .unwrap_or(0);
        let position = load_debt(&env, &user);
        let debt = effective_debt(&position, env.ledger().timestamp(), DEFAULT_APR_BPS)
            .unwrap_or(position.principal);
        PositionSummary {
            collateral: col,
            debt,
        }
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
        li.sequence_number = li.sequence_number.saturating_add(1);
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
    fn test_position_summary_default_zero() {
        let (_env, client, _admin, user) = setup();
        let pos = client.get_position(&user);
        assert_eq!(pos.collateral, 0);
        assert_eq!(pos.debt, 0);
    }

    #[test]
    fn test_one_year_interest_accrual() {
        let (env, client, _admin, user) = setup();
        client.borrow(&user, &100);
        advance_time(&env, rounding_strategy::SECONDS_PER_YEAR);
        let pos = client.get_position(&user);
        assert_eq!(pos.debt, 105);
    }

    #[test]
    fn test_multi_period_drift_bounded() {
        let (env, client, _admin, user) = setup();
        client.borrow(&user, &1000);
        let monthly = rounding_strategy::SECONDS_PER_YEAR / 12;
        let mut last_debt = 1000i128;

        for _ in 0..12 {
            advance_time(&env, monthly);
            let pos = client.get_position(&user);
            assert!(pos.debt >= last_debt);
            last_debt = pos.debt;
        }

        assert!(last_debt >= 1045 && last_debt <= 1055);
    }

    #[test]
    fn test_accrual_on_repay_orders_before_reduction() {
        let (env, client, _admin, user) = setup();
        client.borrow(&user, &100);
        advance_time(&env, rounding_strategy::SECONDS_PER_YEAR);
        let before_repay = client.get_position(&user).debt;
        assert_eq!(before_repay, 105);
        let after_repay = client.repay(&user, &5);
        assert_eq!(after_repay, 100);
        let stored = client.get_debt_position(&user);
        assert_eq!(stored.principal, 100);
    }

    #[test]
    fn test_borrow_accrues_before_increasing_principal() {
        let (env, client, _admin, user) = setup();
        client.borrow(&user, &100);
        advance_time(&env, rounding_strategy::SECONDS_PER_YEAR / 2);
        let half_year = client.get_position(&user).debt;
        assert!(half_year >= 102);
        client.borrow(&user, &10);
        let after_second_borrow = client.get_position(&user).debt;
        assert!(after_second_borrow >= half_year + 10);
    }
}

#[cfg(test)]
mod interest_drift_regression_test;
