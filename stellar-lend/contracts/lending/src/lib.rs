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

/// Canonical storage namespace for this contract.
///
/// Using a single typed enum prevents silent collisions between ad-hoc string
/// keys and tuple layouts as the contract grows.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Admin,
    BorrowMinAmount,
    FlashActive,
    FlashFeeBps,
    Collateral(Address),
    Debt(Address),
    Balance(Address, Address),
    Treasury(Address),
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PositionSummary {
    pub collateral: i128,
    pub debt: i128,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum LendingError {
    BelowMinimumBorrow = 1008,
}

#[contract]
pub struct LendingContract;

#[contractimpl]
impl LendingContract {
    pub fn initialize(env: Env, admin: Address) {
        env.storage().instance().set(&DataKey::Admin, &admin);
    }

    pub fn get_admin(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Admin).unwrap()
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
        let new_balance = current + amount;
        env.storage().persistent().set(&key, &new_balance);
        new_balance
    }

    pub fn withdraw(env: Env, user: Address, amount: i128) -> i128 {
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
        let new_balance = current - amount;
        env.storage().persistent().set(&key, &new_balance);
        new_balance
    }

    /// Borrow against deposited collateral.
    pub fn borrow(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
        user.require_auth();
        let min_borrow = Self::get_min_borrow(env.clone());
        if amount < min_borrow {
            return Err(LendingError::BelowMinimumBorrow);
        }
        let now = env.ledger().timestamp();
        let position = load_debt(&env, &user);
        let updated = borrow_amount(position, now, amount, DEFAULT_APR_BPS)
            .unwrap_or_else(|_| panic_with_debt_error());
        save_debt(&env, &user, &updated);
        Ok(updated.principal)
    }

    pub fn repay(env: Env, user: Address, amount: i128) -> i128 {
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
            .unwrap_or_else(|_| panic_with_debt_error());
        save_debt(&env, &user, &updated);
        updated.principal
    }

    pub fn get_debt_position(env: Env, user: Address) -> DebtPosition {
        load_debt(&env, &user)
    }

    // Flash loan fee setter (bps). Only admin may call.
    pub fn set_flash_loan_fee_bps(env: Env, admin: Address, fee_bps: i128) {
        admin.require_auth();
        let stored_admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        if stored_admin != admin {
            panic!("Unauthorized");
        }
        const MAX_FEE: i128 = 1000;
        if fee_bps < 0 || fee_bps > MAX_FEE {
            panic!("InvalidFeeBps");
        }
        env.storage()
            .instance()
            .set(&DataKey::FlashFeeBps, &fee_bps);
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
        // Soroban SDK v25 exposes the current contract address here, not the
        // external invoker identity.
        let payer = env.current_contract_address();
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

        // Ensure receiver consent
        receiver.require_auth();

        // compute fee
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

        // invoke receiver callback: on_flash_loan(initiator, asset, amount, fee, params)
        let method = Symbol::new(&env, "on_flash_loan");
        // Prepare arguments: initiator = caller (invoker)
        let initiator = env.current_contract_address();
        // Call contract - if it panics, propagate
        env.invoke_contract::<()>(
            &receiver,
            &method,
            (initiator.clone(), asset.clone(), amount, fee, params).into_val(&env),
        );

        // clear reentrancy guard before checks to ensure state is readable
        env.storage().instance().set(&DataKey::FlashActive, &false);

        // verify repayment: treasury balance must be >= previous tre_bal + fee
        let final_tre: i128 = env.storage().persistent().get(&tre_key).unwrap_or(0);
        if final_tre < tre_bal + fee {
            panic!("InsufficientRepayment");
        }
    }

    /// Get the user's current position summary.
    pub fn get_position(env: Env, user: Address) -> PositionSummary {
        let col: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Collateral(user.clone()))
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
    fn test_typed_storage_keys_keep_namespaces_isolated() {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(LendingContract, ());
        let user = Address::generate(&env);
        let asset = Address::generate(&env);

        env.as_contract(&id, || {
            env.storage().instance().set(&DataKey::Admin, &user);
            env.storage().instance().set(&DataKey::FlashFeeBps, &7i128);
            env.storage()
                .persistent()
                .set(&DataKey::Collateral(user.clone()), &25i128);
            env.storage()
                .persistent()
                .set(&DataKey::Debt(user.clone()), &40i128);
            env.storage()
                .persistent()
                .set(&DataKey::Balance(asset.clone(), user.clone()), &55i128);
            env.storage()
                .persistent()
                .set(&DataKey::Treasury(asset.clone()), &70i128);

            assert_eq!(
                env.storage().instance().get(&DataKey::Admin),
                Some(user.clone())
            );
            assert_eq!(
                env.storage().instance().get(&DataKey::FlashFeeBps),
                Some(7i128)
            );
            assert_eq!(
                env.storage()
                    .persistent()
                    .get(&DataKey::Collateral(user.clone())),
                Some(25i128)
            );
            assert_eq!(
                env.storage().persistent().get(&DataKey::Debt(user.clone())),
                Some(40i128)
            );
            assert_eq!(
                env.storage()
                    .persistent()
                    .get(&DataKey::Balance(asset.clone(), user.clone())),
                Some(55i128)
            );
            assert_eq!(
                env.storage().persistent().get(&DataKey::Treasury(asset)),
                Some(70i128)
            );
        });
    }
}
