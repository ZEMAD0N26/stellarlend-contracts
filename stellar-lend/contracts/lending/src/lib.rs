#![no_std]

mod debt;
pub mod rounding_strategy;

#[cfg(test)]
mod interest_drift_regression_test;

use soroban_sdk::{contract, contractimpl, contracttype, contracterror, Address, Bytes, Env, Symbol, symbol_short};
use debt::{borrow_amount, load_debt, save_debt, DebtPosition, DEFAULT_APR_BPS, repay_amount, effective_debt};

const PERSISTENT_TTL_LEDGERS: u32 = 1_000_000;
const DEFAULT_DEPOSIT_CAP: i128 = 100_000_000_000_000; // 100 Trillion
const DEFAULT_DEBT_CEILING: i128 = 50_000_000_000_000; // 50 Trillion

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataKey {
    Admin,
    PendingAdmin,
    EmergencyState,
    Guardian,
    Collateral(Address),
    Oracle,
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
    InvalidAmount = 2004,
}

#[contract]
pub struct LendingContract;

fn acquire_reentrancy_lock(env: &Env) {
    let reentrancy_lock_key = Symbol::new(env, "reent_l");
    let locked: bool = env
        .storage()
        .temporary()
        .get(&reentrancy_lock_key)
        .unwrap_or(false);
    if locked {
        panic!("reentrant call");
    }
    env.storage().temporary().set(&reentrancy_lock_key, &true);
}

fn release_reentrancy_lock(env: &Env) {
    let reentrancy_lock_key = Symbol::new(env, "reent_l");
    env.storage().temporary().remove(&reentrancy_lock_key);
}

fn with_reentrancy_lock<T>(env: &Env, f: impl FnOnce() -> T) -> T {
    acquire_reentrancy_lock(env);
    let result = f();
    release_reentrancy_lock(env);
    result
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
    let key = ("col", user.clone());
    let ttl = env.storage().max_ttl().min(PERSISTENT_TTL_LEDGERS);
    env.storage().persistent().extend_ttl(&key, ttl, ttl);
}

fn extend_debt_ttl(env: &Env, user: &Address) {
    let key = ("debt", user.clone());
    let ttl = env.storage().max_ttl().min(PERSISTENT_TTL_LEDGERS);
    env.storage().persistent().extend_ttl(&key, ttl, ttl);
}

#[contractimpl]
impl LendingContract {
    pub fn initialize(env: Env, admin: Address) -> Result<(), LendingError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(LendingError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        set_emergency_state_internal(&env, EmergencyState::Normal);
        Ok(())
    }

    pub fn get_admin(env: Env) -> Result<Address, LendingError> {
        match env.storage().instance().get(&DataKey::Admin) {
            Some(a) => Ok(a),
            None => Err(LendingError::NotInitialized),
        }
    }

    pub fn propose_admin(env: Env, new_admin: Address) -> Result<(), LendingError> {
        let current_admin = Self::get_admin(env.clone())?;
        current_admin.require_auth();
        env.storage().instance().set(&DataKey::PendingAdmin, &new_admin);
        Ok(())
    }

    pub fn accept_admin(env: Env) -> Result<(), LendingError> {
        let pending_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .ok_or(LendingError::NotInitialized)?;
        pending_admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &pending_admin);
        env.storage().instance().remove(&DataKey::PendingAdmin);
        Ok(())
    }

    pub fn set_min_borrow(env: Env, min_borrow: i128) -> Result<(), LendingError> {
        let admin = Self::get_admin(env.clone())?;
        admin.require_auth();
        env.storage().instance().set(&DataKey::BorrowMinAmount, &min_borrow);
        Ok(())
    }

    pub fn get_min_borrow(env: Env) -> i128 {
        env.storage().instance().get(&DataKey::BorrowMinAmount).unwrap_or(0)
    }

    pub fn deposit(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
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
        
        let total_deposits: i128 = env.storage().persistent().get(&DataKey::TotalDeposits).unwrap_or(0);
        let deposit_cap: i128 = env.storage().persistent().get(&DataKey::DepositCap).unwrap_or(DEFAULT_DEPOSIT_CAP);
        
        let new_total = total_deposits.checked_add(amount)
            .ok_or(LendingError::Overflow)?;
        
        if new_total > deposit_cap {
            return Err(LendingError::DepositCapExceeded);
        }
        env.storage().persistent().set(&DataKey::TotalDeposits, &new_total);
        
        let key = ("col", user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let new_balance = current.checked_add(amount).ok_or(LendingError::Overflow)?;
        env.storage().persistent().set(&key, &new_balance);
        extend_collateral_ttl(&env, &user);
        Ok(new_balance)
    }

    pub fn deposit_collateral(
        env: Env,
        user: Address,
        asset: Address,
        amount: i128,
    ) -> Result<i128, LendingError> {
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

        if amount <= 0 {
            return Err(LendingError::InvalidAmount);
        }

        let total_deposits: i128 = env.storage().persistent().get(&DataKey::TotalDeposits).unwrap_or(0);
        let deposit_cap: i128 = env.storage().persistent().get(&DataKey::DepositCap).unwrap_or(DEFAULT_DEPOSIT_CAP);

        let new_total = total_deposits
            .checked_add(amount)
            .ok_or(LendingError::Overflow)?;

        if new_total > deposit_cap {
            return Err(LendingError::DepositCapExceeded);
        }
        env.storage().persistent().set(&DataKey::TotalDeposits, &new_total);

        let col_key = ("col", user.clone());
        let current: i128 = env.storage().persistent().get(&col_key).unwrap_or(0);
        let new_balance = current.checked_add(amount).ok_or(LendingError::Overflow)?;
        env.storage().persistent().set(&col_key, &new_balance);

        env.storage().persistent().set(&("col_asset", user.clone()), &asset);

        extend_collateral_ttl(&env, &user);
        
        let asset_key = ("col_asset", user.clone());
        let ttl = env.storage().max_ttl().min(PERSISTENT_TTL_LEDGERS);
        env.storage().persistent().extend_ttl(&asset_key, ttl, ttl);

        Ok(new_balance)
    }

    pub fn withdraw(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
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
        let key = ("col", user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        if amount > current {
            panic!("insufficient collateral");
        }
        let new_balance = current.checked_sub(amount).ok_or(LendingError::Overflow)?;
        env.storage().persistent().set(&key, &new_balance);
        
        let total_deposits: i128 = env.storage().persistent().get(&DataKey::TotalDeposits).unwrap_or(0);
        let new_total = total_deposits.checked_sub(amount).ok_or(LendingError::Overflow)?;
        env.storage().persistent().set(&DataKey::TotalDeposits, &new_total);

        extend_collateral_ttl(&env, &user);
        Ok(new_balance)
    }

    pub fn borrow(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
        check_emergency_status(&env, ProtocolAction::Borrow);

        user.require_auth();
        let min_borrow = Self::get_min_borrow(env.clone());
        if amount < min_borrow {
            panic!("BelowMinimumBorrow");
        }

        let total_debt: i128 = env.storage().persistent().get(&DataKey::TotalDebt).unwrap_or(0);
        let debt_ceiling: i128 = env.storage().persistent().get(&DataKey::DebtCeiling).unwrap_or(DEFAULT_DEBT_CEILING);
        
        let new_total = total_debt.checked_add(amount)
            .ok_or(LendingError::Overflow)?;
        
        if new_total > debt_ceiling {
            return Err(LendingError::DebtCeilingExceeded);
        }
        env.storage().persistent().set(&DataKey::TotalDebt, &new_total);

        let now = env.ledger().timestamp();
        let position = load_debt(&env, &user);
        let updated = borrow_amount(position, now, amount, DEFAULT_APR_BPS)
            .map_err(|_| LendingError::Overflow)?;
        save_debt(&env, &user, &updated);
        extend_debt_ttl(&env, &user);
        Ok(updated.principal)
    }

    pub fn repay(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
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
            .map_err(|_| LendingError::Overflow)?;
        save_debt(&env, &user, &updated);

        let total_debt: i128 = env.storage().persistent().get(&DataKey::TotalDebt).unwrap_or(0);
        let new_total = total_debt.checked_sub(amount).ok_or(LendingError::Overflow)?;
        env.storage().persistent().set(&DataKey::TotalDebt, &new_total);

        extend_debt_ttl(&env, &user);
        Ok(updated.principal)
    }

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

        const LIQUIDATION_THRESHOLD: i128 = 8000;
        let hf = (collateral * LIQUIDATION_THRESHOLD) / debt;

        if hf >= 10000 {
            return Err(Error::PositionHealthy);
        }

        const CLOSE_FACTOR: i128 = 5000;
        let max_repay = (debt * CLOSE_FACTOR) / 10000;
        let actual_repay = if amount > max_repay { max_repay } else { amount };

        const INCENTIVE_BPS: i128 = 1000;
        let seized_collateral = (actual_repay * (10000 + INCENTIVE_BPS)) / 10000;
        
        let final_seized = if seized_collateral > collateral { collateral } else { seized_collateral };

        let new_debt = debt - actual_repay;
        let new_col = collateral - final_seized;

        env.storage().persistent().set(&debt_key, &new_debt);
        env.storage().persistent().set(&col_key, &new_col);

        Ok(actual_repay)
    }

    pub fn get_debt_position(env: Env, user: Address) -> DebtPosition {
        let position = load_debt(&env, &user);
        if position.principal != 0 {
            extend_debt_ttl(&env, &user);
        }
        position
    }

    pub fn set_debt_ceiling(env: Env, ceiling: i128) -> Result<(), LendingError> {
        let admin = Self::get_admin(env.clone())?;
        admin.require_auth();
        if ceiling <= 0 {
            panic!("InvalidDebtCeiling");
        }
        env.storage().persistent().set(&DataKey::DebtCeiling, &ceiling);
        Ok(())
    }

    pub fn get_debt_ceiling(env: Env) -> i128 {
        env.storage().persistent().get(&DataKey::DebtCeiling).unwrap_or(DEFAULT_DEBT_CEILING)
    }

    pub fn set_deposit_cap(env: Env, cap: i128) -> Result<(), LendingError> {
        let admin = Self::get_admin(env.clone())?;
        admin.require_auth();
        if cap <= 0 {
            panic!("InvalidDepositCap");
        }
        env.storage().persistent().set(&DataKey::DepositCap, &cap);
        Ok(())
    }

    pub fn get_deposit_cap(env: Env) -> i128 {
        env.storage().persistent().get(&DataKey::DepositCap).unwrap_or(DEFAULT_DEPOSIT_CAP)
    }

    pub fn get_total_debt(env: Env) -> i128 {
        env.storage().persistent().get(&DataKey::TotalDebt).unwrap_or(0)
    }

    pub fn get_total_deposits(env: Env) -> i128 {
        env.storage().persistent().get(&DataKey::TotalDeposits).unwrap_or(0)
    }

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
        env.storage().instance().set(&DataKey::FlashFeeBps, &fee_bps);
        Ok(())
    }

    pub fn set_oracle(env: Env, admin: Address, oracle: Address) -> Result<(), LendingError> {
        admin.require_auth();
        let stored_admin = Self::get_admin(env.clone())?;
        if stored_admin != admin {
            panic!("Unauthorized");
        }
        env.storage().instance().set(&DataKey::Oracle, &oracle);
        Ok(())
    }

    pub fn get_oracle(env: Env) -> Result<Address, LendingError> {
        env.storage()
            .instance()
            .get(&DataKey::Oracle)
            .ok_or(LendingError::NotInitialized)
    }

    pub fn get_collateral_value(env: Env, user: Address) -> i128 {
        let col_key = ("col", user.clone());
        let collateral: i128 = env.storage().persistent().get(&col_key).unwrap_or(0);
        if collateral == 0 {
            return 0;
        }

        let oracle_res = env.storage().instance().get::<_, Address>(&DataKey::Oracle);
        let oracle = match oracle_res {
            Some(o) => o,
            None => return 0,
        };

        let asset_res = env.storage().persistent().get::<_, Address>(&("col_asset", user.clone()));
        let asset = match asset_res {
            Some(a) => a,
            None => return 0,
        };

        let price_res = env.try_invoke_contract::<i128>(
            &oracle,
            &Symbol::new(&env, "price"),
            (asset,).into_val(&env),
        );

        let price = match price_res {
            Ok(Ok(p)) => p,
            _ => return 0,
        };

        if price <= 0 {
            return 0;
        }

        const PRICE_SCALE: i128 = 100_000_000;
        collateral.checked_mul(price)
            .map(|v| v / PRICE_SCALE)
            .unwrap_or(0)
    }

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

    pub fn repay_flash_loan(env: Env, asset: Address, amount: i128) {
        let payer = Env::invoker(&env);
        payer.require_auth();
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
        let tre_key = DataKey::Treasury(asset.clone());
        let tre_bal: i128 = env.storage().persistent().get(&tre_key).unwrap_or(0);
        let new_tre_bal = tre_bal.checked_add(amount)
            .expect("repay_flash_loan: treasury balance overflow");
        env.storage()
            .persistent()
            .set(&tre_key, &new_tre_bal);
    }

    pub fn flash_loan(env: Env, receiver: Address, asset: Address, amount: i128, params: Bytes) {
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

        env.storage().instance().set(&DataKey::FlashActive, &true);

        let method = Symbol::new(&env, "on_flash_loan");
        let initiator = Env::invoker(&env);
        env.invoke_contract::<()>(
            &receiver,
            &method,
            (initiator.clone(), asset.clone(), amount, fee, params).into_val(&env),
        );

        env.storage().instance().set(&DataKey::FlashActive, &false);

        let final_tre: i128 = env.storage().persistent().get(&tre_key).unwrap_or(0);
        let required_balance = tre_bal.checked_add(fee)
            .expect("flash_loan: fee addition overflow");
        if final_tre < required_balance {
            panic!("InsufficientRepayment");
        }
    }

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
                .unwrap_or(i128::MAX)
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

    #[contract]
    pub struct MockOracleContract;

    #[contractimpl]
    impl MockOracleContract {
        pub fn price(env: Env, asset: Address) -> i128 {
            100_000_000 // 1.0 USD
        }
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
        let (env, client, _admin, user) = setup();
        client.borrow(&user, &100).unwrap();

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
    fn test_set_min_borrow_admin_only() {
        let (_env, client, _admin, _user) = setup();
        assert_eq!(client.get_min_borrow(), 0);
        client.set_min_borrow(&100).unwrap();
        assert_eq!(client.get_min_borrow(), 100);
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
        let (_env, client, admin, user) = setup();
        client.set_emergency_state(&EmergencyState::Shutdown);
        client.deposit(&user, &10).unwrap();
    }

    #[test]
    #[should_panic(expected = "OperationDisabledDuringShutdown")]
    fn test_shutdown_blocks_borrow() {
        let (_env, client, admin, user) = setup();
        client.set_emergency_state(&EmergencyState::Shutdown);
        client.borrow(&user, &5).unwrap();
    }

    #[test]
    #[should_panic(expected = "OperationDisabledDuringShutdown")]
    fn test_shutdown_blocks_withdraw() {
        let (_env, client, admin, user) = setup();
        client.deposit(&user, &100).unwrap();
        client.set_emergency_state(&EmergencyState::Shutdown);
        client.withdraw(&user, &10).unwrap();
    }

    #[test]
    #[should_panic(expected = "OperationDisabledDuringShutdown")]
    fn test_shutdown_blocks_repay() {
        let (_env, client, admin, user) = setup();
        client.borrow(&user, &100).unwrap();
        client.set_emergency_state(&EmergencyState::Shutdown);
        client.repay(&user, &10).unwrap();
    }

    #[test]
    #[should_panic(expected = "ActionBlockedInRecovery")]
    fn test_recovery_blocks_deposit() {
        let (_env, client, admin, user) = setup();
        client.set_emergency_state(&EmergencyState::Recovery);
        client.deposit(&user, &10).unwrap();
    }

    #[test]
    #[should_panic(expected = "ActionBlockedInRecovery")]
    fn test_recovery_blocks_borrow() {
        let (_env, client, admin, user) = setup();
        client.set_emergency_state(&EmergencyState::Recovery);
        client.borrow(&user, &10).unwrap();
    }

    #[test]
    fn test_recovery_allows_repay_and_withdraw() {
        let (_env, client, admin, user) = setup();
        client.deposit(&user, &200).unwrap();
        client.borrow(&user, &50).unwrap();
        client.set_emergency_state(&EmergencyState::Recovery);
        let repay_result = client.repay(&user, &10).unwrap();
        assert_eq!(repay_result, 40);
        let withdraw_result = client.withdraw(&user, &10).unwrap();
        assert_eq!(withdraw_result, 190);
    }

    // ============ DEPOSIT COLLATERAL AND ORACLE TESTS ============

    #[test]
    fn test_deposit_collateral_increases_balance_and_stores_asset() {
        let (env, client, _admin, user) = setup();
        let asset = Address::generate(&env);
        let result = client.deposit_collateral(&user, &asset, &250).unwrap();
        assert_eq!(result, 250);

        let pos = client.get_position(&user);
        assert_eq!(pos.collateral, 250);
    }

    #[test]
    fn test_get_collateral_value_calculates_usd_value() {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(LendingContract, ());
        let client = LendingContractClient::new(&env, &id);
        let admin = Address::generate(&env);
        let user = Address::generate(&env);
        client.initialize(&admin);

        let asset = Address::generate(&env);
        let oracle = env.register(MockOracleContract, ());

        client.set_oracle(&admin, &oracle).unwrap();
        client.deposit_collateral(&user, &asset, &1000).unwrap();

        let val = client.get_collateral_value(&user);
        assert_eq!(val, 1000);
    }

    #[test]
    fn test_get_collateral_value_returns_zero_when_no_oracle() {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(LendingContract, ());
        let client = LendingContractClient::new(&env, &id);
        let admin = Address::generate(&env);
        let user = Address::generate(&env);
        client.initialize(&admin);

        let asset = Address::generate(&env);
        client.deposit_collateral(&user, &asset, &1000).unwrap();

        let val = client.get_collateral_value(&user);
        assert_eq!(val, 0);
    }

    #[test]
    fn test_set_oracle_unauthorized_panics() {
        let (env, client, _admin, user) = setup();
        let oracle = Address::generate(&env);
        let res = client.try_set_oracle(&user, &oracle);
        assert!(res.is_err());
    }
}
