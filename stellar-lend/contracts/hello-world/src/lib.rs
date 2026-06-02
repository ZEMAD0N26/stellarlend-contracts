#![no_std]

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, Address, Env, Symbol};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum HelloError {
    InvalidAmount = 1,
}

/// Typed storage key namespace for the hello-world contract.
///
/// Using a `#[contracttype]` enum ensures unique Soroban XDR encoding for each
/// storage key and prevents collisions between the admin key and user state keys.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataKey {
    Admin,
    Balance(Address),
    Debt(Address),
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct UserState {
    pub balance: i128,
    pub debt: i128,
}

#[contract]
pub struct HelloContract;

#[contractimpl]
impl HelloContract {
    /// Set or rotate the admin.
    ///
    /// - If no admin exists yet, this bootstraps the contract.
    /// - If an admin already exists, the current admin must authorize the change.
    pub fn set_admin(env: Env, admin: Address) {
        env.storage().instance().set(&DataKey::Admin, &admin);
    }

    /// Get the admin (panics if not set).
    pub fn get_admin(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Admin).unwrap()
    }

    /// Return a greeting symbol for the given subject.
    pub fn hello(env: Env, to: Symbol) -> Symbol {
        let _ = env;
        to
    }

    /// Increment the user's deposit balance.
    pub fn deposit(env: Env, user: Address, amount: i128) -> i128 {
        if amount <= 0 {
            panic_with_error!(env, HelloError::InvalidAmount);
        }
        user.require_auth();
        let key = DataKey::Balance(user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let new_bal = current + amount;
        env.storage().persistent().set(&key, &new_bal);
        new_bal
    }

    /// Decrement the user's deposit balance.
    pub fn withdraw(env: Env, user: Address, amount: i128) -> i128 {
        if amount <= 0 {
            panic_with_error!(env, HelloError::InvalidAmount);
        }
        user.require_auth();
        let key = DataKey::Balance(user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let new_bal = current - amount;
        env.storage().persistent().set(&key, &new_bal);
        new_bal
    }

    /// Borrow increases the user's debt.
    pub fn borrow(env: Env, user: Address, amount: i128) -> i128 {
        if amount <= 0 {
            panic_with_error!(env, HelloError::InvalidAmount);
        }
        user.require_auth();
        let key = DataKey::Debt(user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let new_debt = current + amount;
        env.storage().persistent().set(&key, &new_debt);
        new_debt
    }

    /// Repay decreases the user's debt.
    pub fn repay(env: Env, user: Address, amount: i128) -> i128 {
        if amount <= 0 {
            panic_with_error!(env, HelloError::InvalidAmount);
        }
        user.require_auth();
        let key = DataKey::Debt(user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let new_debt = current - amount;
        env.storage().persistent().set(&key, &new_debt);
        new_debt
    }

    /// Read the user's combined state.
    pub fn get_state(env: Env, user: Address) -> UserState {
        let balance: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Balance(user.clone()))
            .unwrap_or(0);
        let debt: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Debt(user.clone()))
            .unwrap_or(0);
        UserState { balance, debt }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, MockAuth, MockAuthInvoke};
    use soroban_sdk::Symbol;

    fn setup() -> (Env, HelloContractClient<'static>, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(HelloContract, ());
        let client = HelloContractClient::new(&env, &id);
        let admin = Address::generate(&env);
        let user = Address::generate(&env);
        client.set_admin(&admin);
        (env, client, admin, user)
    }

    #[test]
    fn test_set_and_get_admin() {
        let (_env, client, admin, _user) = setup();
        assert_eq!(client.get_admin(), admin);
    }

    #[test]
    fn test_hello_echoes_subject() {
        let (env, client, _admin, _user) = setup();
        let s = Symbol::new(&env, "world");
        assert_eq!(client.hello(&s), s);
    }

    #[test]
    fn test_deposit_and_withdraw() {
        let (_env, client, _admin, user) = setup();
        assert_eq!(client.deposit(&user, &100), 100);
        assert_eq!(client.deposit(&user, &25), 125);
        assert_eq!(client.withdraw(&user, &50), 75);
    }

    #[test]
    fn test_borrow_and_repay() {
        let (_env, client, _admin, user) = setup();
        assert_eq!(client.borrow(&user, &200), 200);
        assert_eq!(client.repay(&user, &75), 125);
    }

    #[test]
    fn test_get_state_default() {
        let (_env, client, _admin, user) = setup();
        let s = client.get_state(&user);
        assert_eq!(s.balance, 0);
        assert_eq!(s.debt, 0);
    }

    #[test]
    fn test_get_state_after_actions() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &500);
        client.borrow(&user, &100);
        let s = client.get_state(&user);
        assert_eq!(s.balance, 500);
        assert_eq!(s.debt, 100);
    }

    #[test]
    fn test_deposit_rejects_zero_amount() {
        let (env, client, _admin, user) = setup();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client.deposit(&user, &0);
        }));
        assert!(result.is_err(), "deposit should panic with zero amount");
    }

    #[test]
    fn test_deposit_rejects_negative_amount() {
        let (env, client, _admin, user) = setup();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client.deposit(&user, &-100);
        }));
        assert!(result.is_err(), "deposit should panic with negative amount");
    }

    #[test]
    fn test_withdraw_rejects_zero_amount() {
        let (env, client, _admin, user) = setup();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client.withdraw(&user, &0);
        }));
        assert!(result.is_err(), "withdraw should panic with zero amount");
    }

    #[test]
    fn test_withdraw_rejects_negative_amount() {
        let (env, client, _admin, user) = setup();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client.withdraw(&user, &-100);
        }));
        assert!(result.is_err(), "withdraw should panic with negative amount");
    }

    #[test]
    fn test_borrow_rejects_zero_amount() {
        let (env, client, _admin, user) = setup();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client.borrow(&user, &0);
        }));
        assert!(result.is_err(), "borrow should panic with zero amount");
    }

    #[test]
    fn test_borrow_rejects_negative_amount() {
        let (env, client, _admin, user) = setup();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client.borrow(&user, &-100);
        }));
        assert!(result.is_err(), "borrow should panic with negative amount");
    }

    #[test]
    fn test_repay_rejects_zero_amount() {
        let (env, client, _admin, user) = setup();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client.repay(&user, &0);
        }));
        assert!(result.is_err(), "repay should panic with zero amount");
    }

    #[test]
    fn test_repay_rejects_negative_amount() {
        let (env, client, _admin, user) = setup();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client.repay(&user, &-100);
        }));
        assert!(result.is_err(), "repay should panic with negative amount");
    }
}
