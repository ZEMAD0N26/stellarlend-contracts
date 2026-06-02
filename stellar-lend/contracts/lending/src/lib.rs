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
    contract, contractevent, contractimpl, contracttype, Address, Bytes, Env, IntoVal, Symbol, Val,
};

// Re-export common types so callers only need one import.
pub use stellar_lend_common::{BPS_DENOM, LendingError};

/// Maximum desired persistent TTL for position entries, in ledgers.
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

    /// Propose a new admin (current admin only).
    pub fn propose_admin(env: Env, new_admin: Address) {
        let current_admin = Self::get_admin(env.clone());
        current_admin.require_auth();
        env.storage().instance().set(&DataKey::PendingAdmin, &new_admin);
    }

    /// Accept the proposed admin role (proposed admin only).
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

    /// Set the guardian address (admin-only).
    /// The guardian's sole capability is calling `set_emergency_state(Shutdown)`.
    pub fn set_guardian(env: Env, guardian: Address) {
        let admin = Self::get_admin(env.clone());
        admin.require_auth();
        env.storage().instance().set(&DataKey::Guardian, &guardian);
    }

    /// Get the current guardian, if one is set.
    pub fn get_guardian(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Guardian)
    }

    /// Update the protocol emergency state.
    ///
    /// Authorization rules:
    /// - `Shutdown`  → guardian OR admin
    /// - `Recovery`  → admin only
    /// - `Normal`    → admin only
    pub fn set_emergency_state(env: Env, new_state: EmergencyState) {
        let admin = Self::get_admin(env.clone());

        match new_state {
            EmergencyState::Shutdown => {
                // Guardian OR admin may trigger a shutdown.
                let guardian_opt: Option<Address> =
                    env.storage().instance().get(&DataKey::Guardian);
                let authorized = match guardian_opt {
                    Some(ref guardian) => {
                        // Try guardian first; fall back to admin.
                        // require_auth panics if neither signed.
                        let try_guardian = env.auths().iter().any(|(a, _)| a == guardian);
                        let try_admin = env.auths().iter().any(|(a, _)| a == &admin);
                        if try_guardian {
                            guardian.require_auth();
                            true
                        } else if try_admin {
                            admin.require_auth();
                            true
                        } else {
                            false
                        }
                    }
                    None => {
                        // No guardian configured — admin only.
                        admin.require_auth();
                        true
                    }
                };
                if !authorized {
                    panic!("Unauthorized");
                }
            }
            EmergencyState::Recovery | EmergencyState::Normal => {
                // Only admin may lift an emergency.
                admin.require_auth();
            }
        }

        let old_state = get_emergency_state(&env);
        set_emergency_state_internal(&env, new_state);
        EmergencyStateChangedEvent { old_state, new_state }.publish(&env);
    }

    /// Set the minimum borrow amount (admin-only).
    pub fn set_min_borrow(env: Env, min_borrow: i128) -> Result<(), LendingError> {
        let admin = Self::get_admin(env.clone());
        admin.require_auth();
        env.storage().instance().set(&DataKey::BorrowMinAmount, &min_borrow);
        Ok(())
    }

    /// Get the minimum borrow amount.
    pub fn get_min_borrow(env: Env) -> i128 {
        env.storage().instance().get(&DataKey::BorrowMinAmount).unwrap_or(0)
    }

    /// Set the flash loan fee in basis points (admin-only, max 1000 bps = 10%).
    pub fn set_flash_fee(env: Env, fee_bps: i128) -> Result<(), LendingError> {
        let admin = Self::get_admin(env.clone());
        admin.require_auth();
        if fee_bps < 0 || fee_bps > 1000 {
            return Err(LendingError::InvalidFeeBps);
        }
        env.storage().instance().set(&DataKey::FlashFeeBps, &fee_bps);
        Ok(())
    }

    fn get_flash_fee_bps(env: &Env) -> i128 {
        env.storage().instance().get(&DataKey::FlashFeeBps).unwrap_or(5)
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

    /// Set the flash-loan fee in basis points (admin-only, max 1000 BPS = 10 %).
    pub fn set_flash_fee(env: Env, fee_bps: i128) -> Result<(), LendingError> {
        let admin = Self::get_admin(env.clone());
        admin.require_auth();
        if fee_bps < 0 || fee_bps > 1_000 {
            return Err(LendingError::InvalidFeeBps);
        }
        env.storage()
            .instance()
            .set(&DataKey::FlashFeeBps, &fee_bps);
        Ok(())
    }

    /// Deposit collateral for a user.
    pub fn deposit(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
        check_emergency_status(&env, ProtocolAction::Deposit);
        if amount <= 0 {
            return Err(LendingError::InvalidAmount);
        }

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
            .storage().persistent().get(&DataKey::TotalDeposits).unwrap_or(0);
        let deposit_cap: i128 = env
            .storage().persistent().get(&DataKey::DepositCap).unwrap_or(DEFAULT_DEPOSIT_CAP);
        let new_total = total_deposits.checked_add(amount).ok_or(LendingError::Overflow)?;
        if new_total > deposit_cap {
            return Err(LendingError::DepositCapExceeded);
        }

        let key = DataKey::Collateral(user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let new_balance = current
            .checked_add(amount)
            .ok_or(LendingError::Overflow)?;
        env.storage().persistent().set(&key, &new_balance);
        env.storage()
            .persistent()
            .set(&DataKey::TotalDeposits, &new_total);
        extend_collateral_ttl(&env, &user);
        Ok(new_balance)
    }

    pub fn withdraw(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
        check_emergency_status(&env, ProtocolAction::Withdraw);
        if amount <= 0 {
            return Err(LendingError::InvalidAmount);
        }

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
            return Err(LendingError::InsufficientCollateral);
        }
        let new_balance = current
            .checked_sub(amount)
            .ok_or(LendingError::Overflow)?;
        env.storage().persistent().set(&key, &new_balance);
        extend_collateral_ttl(&env, &user);
        Ok(new_balance)
    }

    /// Borrow against deposited collateral. Enforces protocol-level debt ceiling.
    ///
    /// # Errors
    /// * `LendingError::InvalidAmount` — non-positive amount
    /// * `LendingError::BelowMinimumBorrow` — amount < protocol minimum
    /// * `LendingError::DebtCeilingExceeded` — would exceed debt ceiling
    /// * `LendingError::Overflow` — arithmetic overflow
    pub fn borrow(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
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
        liquidator.require_auth();
        let col_key = DataKey::Collateral(borrower.clone());
        let debt_key = DataKey::Debt(borrower.clone());
        let collateral: i128 = env.storage().persistent().get(&col_key).unwrap_or(0);
        let debt: i128 = env.storage().persistent().get(&debt_key).unwrap_or(0);
        if debt == 0 {
            return Err(LendingError::PositionHealthy);
        }

        // HF = (collateral * 8000 BPS) / debt  (80 % liquidation threshold)
        const LIQUIDATION_THRESHOLD: i128 = 8_000;
        let hf = (collateral * LIQUIDATION_THRESHOLD) / debt;

        if hf >= BPS_DENOM {
            return Err(LendingError::PositionHealthy);
        }

        // Close factor: 50 %
        const CLOSE_FACTOR: i128 = 5_000;
        let max_repay = (debt * CLOSE_FACTOR) / BPS_DENOM;
        let actual_repay = amount.min(max_repay);

        // Liquidation incentive: 10 %
        const INCENTIVE_BPS: i128 = 1_000;
        let seized_collateral = (actual_repay * (BPS_DENOM + INCENTIVE_BPS)) / BPS_DENOM;
        let final_seized = seized_collateral.min(collateral);

        env.storage()
            .persistent()
            .set(&debt_key, &(debt - actual_repay));
        env.storage()
            .persistent()
            .set(&col_key, &(collateral - final_seized));

        Ok(actual_repay)
    }

    /// Repay user debt, clamping overpayment to zero.
    pub fn repay(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
        check_pause_status(&env, ProtocolAction::Repay);
        check_emergency_status(&env, ProtocolAction::Repay);

        if amount <= 0 {
            return Err(LendingError::InvalidAmount);
        }

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

    /// Set the protocol-level debt ceiling (admin-only).
    pub fn set_debt_ceiling(env: Env, ceiling: i128) -> Result<(), LendingError> {
        let admin = Self::get_admin(env.clone());
        admin.require_auth();
        if ceiling <= 0 {
            return Err(LendingError::Overflow);
        }
        env.storage()
            .instance()
            .set(&DataKey::DebtCeiling, &ceiling);
        Ok(())
    }

    /// Update the global emergency state.
    /// Either the guardian or the admin may call this; panics if no guardian is set.
    pub fn set_emergency_state(env: Env, new_state: EmergencyState) {
        let guardian_opt: Option<Address> = env.storage().instance().get(&DataKey::Guardian);
        let admin = Self::get_admin(env.clone());

        match guardian_opt {
            Some(guardian) => {
                let auths = env.auths();
                let is_admin_authorized = auths.iter().any(|(address, _)| address == &admin);
                let is_guardian_authorized =
                    auths.iter().any(|(address, _)| address == &guardian);
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

    fn get_flash_fee_bps(env: &Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::FlashFeeBps)
            .unwrap_or(5)
    }

    /// Return principal + fee to the contract treasury after a flash-loan callback.
    pub fn repay_flash_loan(env: Env, payer: Address, asset: Address, amount: i128) {
        payer.require_auth();
        let payer_key = DataKey::Balance(asset.clone(), payer.clone());
        let payer_bal: i128 = env.storage().persistent().get(&payer_key).unwrap_or(0);
        if payer_bal < amount {
            panic!("InsufficientBalance");
        }
        let new_payer_bal = payer_bal
            .checked_sub(amount)
            .expect("repay_flash_loan: payer balance underflow");
        env.storage().persistent().set(&payer_key, &new_payer_bal);

        let tre_key = DataKey::Treasury(asset.clone());
        let tre_bal: i128 = env.storage().persistent().get(&tre_key).unwrap_or(0);
        let new_tre_bal = tre_bal
            .checked_add(amount)
            .expect("repay_flash_loan: treasury balance overflow");
        env.storage().persistent().set(&tre_key, &new_tre_bal);
    }

    /// Execute a flash loan: transfer assets to `receiver`, invoke its callback,
    /// and verify repayment of principal + fee.
    pub fn flash_loan(
        env: Env,
        initiator: Address,
        receiver: Address,
        asset: Address,
        amount: i128,
        params: Bytes,
    ) {
        let tre_key = DataKey::Treasury(asset.clone());
        let tre_bal: i128 = env.storage().persistent().get(&tre_key).unwrap_or(0);
        if amount > tre_bal {
            panic!("InsufficientLiquidity");
        }

        initiator.require_auth();

        let fee_bps = Self::get_flash_fee_bps(&env);
        let fee = amount
            .checked_mul(fee_bps)
            .map(|v| v / BPS_DENOM)
            .expect("flash_loan: fee calculation overflow");

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

        env.storage().instance().set(&DataKey::FlashActive, &true);

        let method = Symbol::new(&env, "on_flash_loan");
        env.invoke_contract::<Val>(
            &receiver,
            &method,
            soroban_sdk::vec![
                &env,
                initiator.into_val(&env),
                asset.into_val(&env),
                amount.into_val(&env),
                fee.into_val(&env),
                params.into_val(&env)
            ],
        );

        env.storage().instance().set(&DataKey::FlashActive, &false);

        let final_tre: i128 = env.storage().persistent().get(&tre_key).unwrap_or(0);
        let required_balance = tre_bal
            .checked_add(fee)
            .expect("flash_loan: fee addition overflow");
        if final_tre < required_balance {
            panic!("InsufficientRepayment");
        }
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
            col.checked_mul(8_000)
                .map(|v| v / debt)
                .unwrap_or(i128::MAX)
        } else {
            1_000_000 // sentinel for healthy (no debt)
        };

        PositionSummary {
            collateral: col,
            debt,
            health_factor,
        }
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn extend_collateral_ttl(env: &Env, user: &Address) {
    let key = DataKey::Collateral(user.clone());
    let extend_to = env.storage().max_ttl().min(PERSISTENT_TTL_LEDGERS);
    let threshold = extend_to / 2 + 1;
    if env.storage().persistent().has(&key) {
        env.storage()
            .persistent()
            .extend_ttl(&key, threshold, extend_to);
    }
}

fn extend_debt_ttl(env: &Env, user: &Address) {
    let key = DataKey::Debt(user.clone());
    let extend_to = env.storage().max_ttl().min(PERSISTENT_TTL_LEDGERS);
    let threshold = extend_to / 2 + 1;
    if env.storage().persistent().has(&key) {
        env.storage()
            .persistent()
            .extend_ttl(&key, threshold, extend_to);
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

fn set_emergency_state_internal(env: &Env, state: EmergencyState) {
    env.storage().instance().set(&DataKey::EmergencyState, &state);
}

fn check_emergency_status(env: &Env, action: ProtocolAction) {
    match get_emergency_state(env) {
        EmergencyState::Normal => {}
        EmergencyState::Shutdown => panic!("OperationDisabledDuringShutdown"),
        EmergencyState::Recovery => match action {
            ProtocolAction::Repay | ProtocolAction::Withdraw => {}
            _ => panic!("ActionBlockedInRecovery"),
        },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger, LedgerInfo};

    fn setup() -> (
        Env,
        LendingContractClient<'static>,
        soroban_sdk::Address,
        soroban_sdk::Address,
    ) {
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

    // -----------------------------------------------------------------------
    // Basic admin / init
    // -----------------------------------------------------------------------

    #[test]
    fn test_initialize_and_get_admin() {
        let (_env, client, admin, _user) = setup();
        assert_eq!(client.get_admin(), admin);
    }

    #[test]
    fn test_set_min_borrow_admin_only() {
        let (_env, client, _admin, _user) = setup();
        assert_eq!(client.get_min_borrow(), 0);
        client.set_min_borrow(&100);
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
        assert!(
            matches!(res, Err(Ok(LendingError::InvalidFeeBps))),
            "expected InvalidFeeBps, got {:?}",
            res
        );
    }

    #[test]
    fn test_deposit_increases_balance() {
        let (_env, client, _admin, user) = setup();
        assert_eq!(client.deposit(&user, &100), 100);
        assert_eq!(client.deposit(&user, &50), 150);
    }

    #[test]
    fn test_withdraw_decreases_balance() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &100);
        assert_eq!(client.withdraw(&user, &40), 60);
    }

    #[test]
    fn test_withdraw_fails_when_over_withdrawing() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &50);
        assert!(client.try_withdraw(&user, &75).is_err());
    }

    #[test]
    fn test_borrow_increases_debt() {
        let (_env, client, _admin, user) = setup();
        assert_eq!(client.borrow(&user, &50), 50);
    }

    #[test]
    fn test_repay_decreases_debt() {
        let (_env, client, _admin, user) = setup();
        client.borrow(&user, &100);
        assert_eq!(client.repay(&user, &30), 70);
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
        assert!(client.try_borrow(&user, &40).is_err());
    }

    #[test]
    fn test_borrow_exactly_minimum_accepted() {
        let (_env, client, _admin, user) = setup();
        client.set_min_borrow(&50).unwrap();
        assert_eq!(client.borrow(&user, &50), 50);
    }

    #[test]
    fn test_ttl_keeps_position_live_across_reads() {
        let (env, client, _admin, user) = setup();
        client.deposit(&user, &200);
        client.borrow(&user, &75);

        advance_time(&env, (PERSISTENT_TTL_LEDGERS / 2) as u64);
        let pos_mid = client.get_position(&user);
        assert_eq!(pos_mid.collateral, 200);

        advance_time(&env, (PERSISTENT_TTL_LEDGERS / 2 + 1) as u64);
        let pos_after = client.get_position(&user);
        assert_eq!(pos_after.collateral, 200);
    }

    #[test]
    fn test_get_debt_position_extends_debt_ttl() {
        let (env, client, _admin, user) = setup();
        client.borrow(&user, &100);
        advance_time(&env, (PERSISTENT_TTL_LEDGERS / 2) as u64);
        assert_eq!(client.get_debt_position(&user).principal, 100);
        advance_time(&env, (PERSISTENT_TTL_LEDGERS / 2 + 1) as u64);
        assert_eq!(client.get_debt_position(&user).principal, 100);
    }

    /// Guardian can trigger Shutdown.
    #[test]
    fn test_guardian_can_shutdown() {
        let (env, client, _admin, _user) = setup();
        let guardian = Address::generate(&env);
        client.set_guardian(&guardian);
        client.set_emergency_state(&EmergencyState::Shutdown);
        // Verify Shutdown blocks deposits.
        let user = Address::generate(&env);
        let result = client.try_deposit(&user, &10);
        assert!(result.is_err());
    }

    /// Guardian cannot set Recovery (admin-only).
    #[test]
    #[should_panic(expected = "Unauthorized")]
    fn test_guardian_cannot_set_recovery() {
        let (env, client, admin, _user) = setup();
        let guardian = Address::generate(&env);
        client.set_guardian(&guardian);

        // Re-create env that mocks ONLY guardian auth (not admin).
        let env2 = Env::default();
        let id2 = env2.register(LendingContract, ());
        let client2 = LendingContractClient::new(&env2, &id2);
        let admin2 = Address::generate(&env2);
        let guardian2 = Address::generate(&env2);
        env2.mock_all_auths();
        client2.initialize(&admin2);
        client2.set_guardian(&guardian2);

        // Now only mock guardian2 auth, not admin2.
        env2.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &guardian2,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &id2,
                fn_name: "set_emergency_state",
                args: (EmergencyState::Recovery,).into_val(&env2),
                sub_invokes: &[],
            },
        }]);
        client2.set_emergency_state(&EmergencyState::Recovery);
        let _ = (env, admin, guardian);
    }

    /// Guardian cannot set Normal (admin-only).
    #[test]
    #[should_panic(expected = "Unauthorized")]
    fn test_guardian_cannot_set_normal() {
        let env2 = Env::default();
        let id2 = env2.register(LendingContract, ());
        let client2 = LendingContractClient::new(&env2, &id2);
        let admin2 = Address::generate(&env2);
        let guardian2 = Address::generate(&env2);
        env2.mock_all_auths();
        client2.initialize(&admin2);
        client2.set_guardian(&guardian2);

        // Only mock guardian auth.
        env2.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &guardian2,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &id2,
                fn_name: "set_emergency_state",
                args: (EmergencyState::Normal,).into_val(&env2),
                sub_invokes: &[],
            },
        }]);
        client2.set_emergency_state(&EmergencyState::Normal);
    }

    // -----------------------------------------------------------------------
    // set_emergency_state — admin capabilities
    // -----------------------------------------------------------------------

    /// Admin can set Shutdown even when no guardian is configured.
    #[test]
    fn test_admin_can_shutdown_without_guardian() {
        let (_env, client, _admin, _user) = setup();
        assert_eq!(client.get_guardian(), None);
        client.set_emergency_state(&EmergencyState::Shutdown);
    }

    /// Admin can set Recovery.
    #[test]
    fn test_admin_can_set_recovery() {
        let (_env, client, _admin, _user) = setup();
        client.set_emergency_state(&EmergencyState::Recovery);
    }

    /// Admin can set Normal.
    #[test]
    fn test_admin_can_set_normal() {
        let (_env, client, _admin, _user) = setup();
        client.set_emergency_state(&EmergencyState::Shutdown);
        client.set_emergency_state(&EmergencyState::Normal);
    }

    /// Admin can lift Shutdown back to Normal.
    #[test]
    fn test_admin_lifts_shutdown_to_normal() {
        let (env, client, _admin, _user) = setup();
        let guardian = Address::generate(&env);
        client.set_guardian(&guardian);
        client.set_emergency_state(&EmergencyState::Shutdown);
        client.set_emergency_state(&EmergencyState::Normal);
        // Deposits should work again.
        let user = Address::generate(&env);
        let result = client.deposit(&user, &10);
        assert_eq!(result, 10);
    }

    // -----------------------------------------------------------------------
    // set_emergency_state — unauthenticated / random caller
    // -----------------------------------------------------------------------

    /// A random address (neither admin nor guardian) cannot set any state.
    #[test]
    #[should_panic(expected = "Unauthorized")]
    fn test_random_caller_cannot_set_emergency_state() {
        let env2 = Env::default();
        let id2 = env2.register(LendingContract, ());
        let client2 = LendingContractClient::new(&env2, &id2);
        let admin2 = Address::generate(&env2);
        let attacker = Address::generate(&env2);
        env2.mock_all_auths();
        client2.initialize(&admin2);
        // Only mock attacker auth.
        env2.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &attacker,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &id2,
                fn_name: "set_emergency_state",
                args: (EmergencyState::Shutdown,).into_val(&env2),
                sub_invokes: &[],
            },
        }]);
        client2.set_emergency_state(&EmergencyState::Shutdown);
    }

    // -----------------------------------------------------------------------
    // EmergencyState effects on protocol actions
    // -----------------------------------------------------------------------

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
        assert_eq!(client.repay(&user, &10), 40);
        assert_eq!(client.withdraw(&user, &10), 190);
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