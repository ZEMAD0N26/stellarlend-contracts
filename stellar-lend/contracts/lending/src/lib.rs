#![no_std]

use soroban_sdk::{contract, contractimpl, contracttype, Address, Env, Symbol, Bytes};

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
    /// Initialize the lending contract with an admin.
    pub fn initialize(env: Env, admin: Address) {
        env.storage().instance().set(&"admin", &admin);
    }

    /// Get the configured admin (or panic if uninitialized).
    pub fn get_admin(env: Env) -> Address {
        env.storage().instance().get(&"admin").unwrap()
    }

    /// Deposit collateral for a user.
    pub fn deposit(env: Env, user: Address, amount: i128) -> i128 {
        // Prevent mutating during an active flash loan callback
        let active: bool = env.storage().instance().get(&"flash_active").unwrap_or(false);
        if active {
            panic!("FlashLoanReentrancy");
        }
        user.require_auth();
        let key = ("col", user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let new_balance = current + amount;
        env.storage().persistent().set(&key, &new_balance);
        new_balance
    }

    /// Withdraw collateral for a user.
    pub fn withdraw(env: Env, user: Address, amount: i128) -> i128 {
        // Prevent mutating during an active flash loan callback
        let active: bool = env.storage().instance().get(&"flash_active").unwrap_or(false);
        if active {
            panic!("FlashLoanReentrancy");
        }
        user.require_auth();
        let key = ("col", user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let new_balance = current - amount;
        env.storage().persistent().set(&key, &new_balance);
        new_balance
    }

    /// Borrow against deposited collateral.
    pub fn borrow(env: Env, user: Address, amount: i128) -> i128 {
        // Prevent mutating during an active flash loan callback
        let active: bool = env.storage().instance().get(&"flash_active").unwrap_or(false);
        if active {
            panic!("FlashLoanReentrancy");
        }
        user.require_auth();
        let key = ("debt", user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let new_debt = current + amount;
        env.storage().persistent().set(&key, &new_debt);
        new_debt
    }

    /// Repay debt.
    pub fn repay(env: Env, user: Address, amount: i128) -> i128 {
        // Prevent mutating during an active flash loan callback
        let active: bool = env.storage().instance().get(&"flash_active").unwrap_or(false);
        if active {
            panic!("FlashLoanReentrancy");
        }
        user.require_auth();
        let key = ("debt", user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let new_debt = current - amount;
        env.storage().persistent().set(&key, &new_debt);
        new_debt
    }

    // Flash loan fee setter (bps). Only admin may call.
    pub fn set_flash_loan_fee_bps(env: Env, admin: Address, fee_bps: i128) {
        admin.require_auth();
        let stored_admin: Address = env.storage().instance().get(&"admin").unwrap();
        if stored_admin != admin {
            panic!("Unauthorized");
        }
        const MAX_FEE: i128 = 1000;
        if fee_bps < 0 || fee_bps > MAX_FEE {
            panic!("InvalidFeeBps");
        }
        env.storage().instance().set(&"flash_fee_bps", &fee_bps);
    }

    fn get_flash_fee_bps(env: &Env) -> i128 {
        env.storage().instance().get(&"flash_fee_bps").unwrap_or(5)
    }

    // Repay function used by receiver during callback to return funds to the contract.
    pub fn repay_flash_loan(env: Env, asset: Address, amount: i128) {
        // Payer must be the invoker (caller contract/account)
        let payer = env.invoker();
        payer.require_auth();
        // subtract from payer balance
        let payer_key = ("bal", asset.clone(), payer.clone());
        let payer_bal: i128 = env.storage().persistent().get(&payer_key).unwrap_or(0);
        if payer_bal < amount {
            panic!("InsufficientBalance");
        }
        env.storage().persistent().set(&payer_key, &(payer_bal - amount));
        // add to contract treasury
        let tre_key = ("treasury", asset.clone());
        let tre_bal: i128 = env.storage().persistent().get(&tre_key).unwrap_or(0);
        env.storage().persistent().set(&tre_key, &(tre_bal + amount));
    }

    /// Execute a flash loan: transfer assets to `receiver`, call its `on_flash_loan` callback,
    /// and ensure repayment of principal + fee before returning.
    pub fn flash_loan(
        env: Env,
        receiver: Address,
        asset: Address,
        amount: i128,
        params: Bytes,
    ) {
        // Check liquidity
        let tre_key = ("treasury", asset.clone());
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
        env.storage().persistent().set(&tre_key, &(tre_bal - amount));
        let rec_key = ("bal", asset.clone(), receiver.clone());
        let rec_bal: i128 = env.storage().persistent().get(&rec_key).unwrap_or(0);
        env.storage().persistent().set(&rec_key, &(rec_bal + amount));

        // set reentrancy guard
        env.storage().instance().set(&"flash_active", &true);

        // invoke receiver callback: on_flash_loan(initiator, asset, amount, fee, params)
        let method = Symbol::new(&env, "on_flash_loan");
        // Prepare arguments: initiator = caller (invoker)
        let initiator = env.invoker();
        // Call contract - if it panics, propagate
        env.invoke_contract(&receiver, &method, (initiator.clone(), asset.clone(), amount, fee, params));

        // clear reentrancy guard before checks to ensure state is readable
        env.storage().instance().set(&"flash_active", &false);

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
            .get(&("col", user.clone()))
            .unwrap_or(0);
        let debt: i128 = env
            .storage()
            .persistent()
            .get(&("debt", user.clone()))
            .unwrap_or(0);
        PositionSummary {
            collateral: col,
            debt,
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

    use soroban_sdk::{BytesN, IntoVal};

    #[contract]
    pub struct MockFlashLoanReceiver;

    #[contractimpl]
    impl MockFlashLoanReceiver {
        // on_flash_loan will attempt to repay principal + fee by calling back into the lender
        pub fn on_flash_loan(
            env: Env,
            _initiator: Address,
            asset: Address,
            amount: i128,
            fee: i128,
            _params: Bytes,
        ) -> bool {
            // call repay_flash_loan on the invoker (the lending contract)
            let lender = env.invoker();
            let method = Symbol::new(&env, "repay_flash_loan");
            // repay principal + fee
            let to_repay = amount + fee;
            env.invoke_contract(&lender, &method, (asset.clone(), to_repay));
            true
        }
    }

    #[test]
    fn test_flash_loan_success() {
        let env = Env::default();
        env.mock_all_auths();
        // register lending contract
        let lending_id = env.register(LendingContract, ());
        let client = LendingContractClient::new(&env, &lending_id);
        // init admin
        let admin = Address::generate(&env);
        client.initialize(&admin);

        // register receiver contract
        let recv_id = env.register(MockFlashLoanReceiver, ());
        let receiver = Address::Contract(recv_id.clone());

        // choose an asset address
        let asset = Address::generate(&env);

        // seed treasury with liquidity
        let tre_key = ("treasury", asset.clone());
        env.storage().persistent().set(&tre_key, &1000i128);

        // ensure receiver has zero balance initially
        let rec_key = ("bal", asset.clone(), receiver.clone());
        env.storage().persistent().set(&rec_key, &0i128);

        // perform flash loan of 100
        client.flash_loan(&receiver, &asset, &100, &Bytes::new(&env, vec![]));

        // treasury should be decreased by 100 then increased by 100 + fee (default 5 bps => 0)
        let final_tre: i128 = env.storage().persistent().get(&tre_key).unwrap_or(0);
        // fee is amount * 5 / 10000 = 0 for small amounts in integer arithmetic
        assert!(final_tre >= 1000);
    }
}
