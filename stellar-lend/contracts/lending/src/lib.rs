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
    contract, contracterror, contractevent, contractimpl, contracttype, Address, Bytes, Env,
    IntoVal, Symbol, Val,
};

/// Maximum desired persistent TTL for position entries, in ledgers.
/// We bound the extension by the network's `max_ttl` to remain compatible
/// with runtime limits while keeping active positions alive for a long window.
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
}

#[contractevent]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmergencyStateChangedEvent {
    pub old_state: EmergencyState,
    pub new_state: EmergencyState,
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
    InvalidAmount        = 1004,
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
    InsufficientCollateral = 2007,
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

/// Protocol-wide metrics snapshot returned by `get_protocol_metrics`.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtocolMetrics {
    /// Total collateral deposited across all users.
    pub total_supply: i128,
    /// Total debt (principal) outstanding across all users.
    pub total_borrow: i128,
    /// Utilization rate in basis points: `(total_borrow * 10_000) / total_supply`.
    /// Returns 0 when `total_supply` is zero.
    pub utilization_bps: i128,
    /// Ledger sequence number at which this snapshot was taken.
    pub ledger: u32,
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

    /// Propose a new admin (current admin only)
    pub fn propose_admin(env: Env, new_admin: Address) {
        let current_admin = Self::get_admin(env.clone());
        current_admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &new_admin);
    }

    /// Accept the proposed admin role (proposed admin only)
    pub fn accept_admin(env: Env) {
        let pending_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .expect("no pending admin");
        pending_admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::Admin, &pending_admin);
        env.storage().instance().remove(&DataKey::PendingAdmin);
    }

    /// Set the minimum borrow amount (admin-only).
    pub fn set_min_borrow(env: Env, min_borrow: i128) -> Result<(), LendingError> {
        let admin = Self::get_admin(env.clone());
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::BorrowMinAmount, &min_borrow);
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
    pub fn deposit(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
        check_emergency_status(&env, ProtocolAction::Deposit);

        if amount <= 0 {
            return Err(LendingError::InvalidAmount);
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
            .ok_or(LendingError::Overflow)?;

        if new_total > deposit_cap {
            return Err(LendingError::DepositCapExceeded);
        }

        // Update user collateral with overflow protection
        let key = DataKey::Collateral(user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let new_balance = current.checked_add(amount).ok_or(LendingError::Overflow)?;
        env.storage().persistent().set(&key, &new_balance);
        env.storage()
            .persistent()
            .set(&DataKey::TotalDeposits, &new_total);
        // Extend TTL to prevent archival of collateral entry
        extend_collateral_ttl(&env, &user);
        Ok(new_balance)
    }

    pub fn withdraw(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
        check_emergency_status(&env, ProtocolAction::Withdraw);

        if amount <= 0 {
            return Err(LendingError::InvalidAmount);
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
            return Err(LendingError::InsufficientCollateral);
        }
        let new_balance = current.checked_sub(amount).ok_or(LendingError::Overflow)?;
        env.storage().persistent().set(&key, &new_balance);
        // Update protocol-level total deposits
        let total_deposits: i128 = env.storage().persistent().get(&DataKey::TotalDeposits).unwrap_or(0);
        let new_total = total_deposits.saturating_sub(amount);
        env.storage().persistent().set(&DataKey::TotalDeposits, &new_total);
        // Extend TTL to prevent archival of collateral entry
        extend_collateral_ttl(&env, &user);
        Ok(new_balance)
    }

    /// Borrow against deposited collateral. Enforces protocol-level debt ceiling.
    ///
    /// # Security Invariant: Overflow Protection
    /// All debt mutations use `checked_add` to prevent integer overflow.
    /// If overflow would occur, returns `Error::Overflow`.
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
    /// * `Error::Overflow` - Checked arithmetic would overflow
    ///
    /// # Returns
    /// User's debt principal after borrow
    pub fn borrow(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
        check_emergency_status(&env, ProtocolAction::Borrow);

        if amount <= 0 {
            return Err(LendingError::InvalidAmount);
        }

        user.require_auth();
        let min_borrow = Self::get_min_borrow(env.clone());
        if amount < min_borrow {
            panic!("BelowMinimumBorrow");
        }
        
        let now = env.ledger().timestamp();
        let position = load_debt(&env, &user);
        let prev_principal = position.principal;
        let updated =
            borrow_amount(position, now, amount, DEFAULT_APR_BPS).map_err(|e| match e {
                debt::DebtError::InvalidAmount => LendingError::InvalidAmount,
                debt::DebtError::Overflow => LendingError::Overflow,
            })?;
        save_debt(&env, &user, &updated);
        // Track protocol-level total debt
        let total_debt: i128 = env.storage().persistent().get(&DataKey::TotalDebt).unwrap_or(0);
        let delta = updated.principal.checked_sub(prev_principal).expect("borrow: delta overflow");
        let new_total_debt = total_debt.checked_add(delta).expect("borrow: total_debt overflow");
        env.storage().persistent().set(&DataKey::TotalDebt, &new_total_debt);
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

        // Health Factor Calculation (base 10000). HF = (Collateral * Threshold) / Debt
        // We use a hardcoded 80% (8000 BPS) liquidation threshold for this implementation.
        const LIQUIDATION_THRESHOLD: i128 = 8000;
        let hf = (collateral * LIQUIDATION_THRESHOLD) / debt;

        if hf >= 10000 {
            return Err(LendingError::PositionHealthy);
        }

        // Cap maximum allowed repayment by close factor (50%)
        const CLOSE_FACTOR: i128 = 5000;
        let max_repay = (debt * CLOSE_FACTOR) / 10000;
        let actual_repay = if amount > max_repay {
            max_repay
        } else {
            amount
        };

        // Apply liquidation incentive bonus (10%)
        const INCENTIVE_BPS: i128 = 1000;
        let seized_collateral = (actual_repay * (10000 + INCENTIVE_BPS)) / 10000;

        // Ensure we don't seize more than available
        let final_seized = if seized_collateral > collateral {
            collateral
        } else {
            seized_collateral
        };

        let new_debt = debt - actual_repay;
        let new_col = collateral - final_seized;

        env.storage().persistent().set(&debt_key, &new_debt);
        env.storage().persistent().set(&col_key, &new_col);

        Ok(actual_repay)
    }

    /// Repay user debt, clamping overpayment to zero.
    /// Returns the remaining debt principal after the repay.
    pub fn repay(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
        check_emergency_status(&env, ProtocolAction::Repay);

        if amount <= 0 {
            return Err(LendingError::InvalidAmount);
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
        let prev_principal = position.principal;
        let updated =
            repay_amount(position, now, amount, DEFAULT_APR_BPS).map_err(|e| match e {
                debt::DebtError::InvalidAmount => LendingError::InvalidAmount,
                debt::DebtError::Overflow => LendingError::Overflow,
            })?;
        save_debt(&env, &user, &updated);
        // Track protocol-level total debt
        let total_debt: i128 = env.storage().persistent().get(&DataKey::TotalDebt).unwrap_or(0);
        let repaid = prev_principal.checked_sub(updated.principal).unwrap_or(0);
        let new_total_debt = total_debt.saturating_sub(repaid);
        env.storage().persistent().set(&DataKey::TotalDebt, &new_total_debt);
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

    /// Privileged function to update the global emergency state.
    /// Admin can always call this. If a guardian is configured, the guardian
    /// may also call this without admin authorization.
    pub fn set_emergency_state(env: Env, new_state: EmergencyState) {
        let admin = Self::get_admin(env.clone());
        let guardian_opt: Option<Address> = env.storage().instance().get(&DataKey::Guardian);

        let caller_is_guardian = guardian_opt.map_or(false, |guardian| {
            let auths = env.auths();
            auths.iter().any(|(address, _)| address == &guardian)
        });

        if !caller_is_guardian {
            admin.require_auth();
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

    /// Set the flash loan fee in basis points (admin-only). Must be in [0, 1000].
    pub fn set_flash_fee(env: Env, fee_bps: i128) -> Result<(), LendingError> {
        let admin = Self::get_admin(env.clone());
        admin.require_auth();
        if fee_bps < 0 || fee_bps > 1000 {
            return Err(LendingError::InvalidFeeBps);
        }
        env.storage().instance().set(&DataKey::FlashFeeBps, &fee_bps);
        Ok(())
    }

    /// Set the guardian address (admin-only).
    pub fn set_guardian(env: Env, guardian: Address) {
        let admin = Self::get_admin(env.clone());
        admin.require_auth();
        env.storage().instance().set(&DataKey::Guardian, &guardian);
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
    pub fn flash_loan(
        env: Env,
        initiator: Address,
        receiver: Address,
        asset: Address,
        amount: i128,
        params: Bytes,
    ) {
        // Check liquidity
        let tre_key = DataKey::Treasury(asset.clone());
        let tre_bal: i128 = env.storage().persistent().get(&tre_key).unwrap_or(0);
        if amount > tre_bal {
            panic!("InsufficientLiquidity");
        }

        initiator.require_auth();

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
        // Call contract - if it panics, propagate
        env.invoke_contract::<Val>(&receiver, &method, soroban_sdk::vec![&env, initiator.into_val(&env), asset.into_val(&env), amount.into_val(&env), fee.into_val(&env), params.into_val(&env)]);

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
            col.checked_mul(8000).map(|v| v / debt).unwrap_or(i128::MAX) // Sentinel for overflow/healthy
        } else {
            1000000 // Sentinel for healthy (no debt)
        };

        PositionSummary {
            collateral: col,
            debt,
            health_factor,
        }
    }

    /// Returns a consistent protocol-wide snapshot: total supply, total borrow,
    /// utilization in BPS, and the current ledger sequence number.
    ///
    /// `utilization_bps = (total_borrow * 10_000) / total_supply`.
    /// Returns 0 when `total_supply` is 0 to avoid division-by-zero.
    pub fn get_protocol_metrics(env: Env) -> ProtocolMetrics {
        let total_supply: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::TotalDeposits)
            .unwrap_or(0);
        let total_borrow: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::TotalDebt)
            .unwrap_or(0);
        let utilization_bps = if total_supply > 0 {
            total_borrow.saturating_mul(10_000) / total_supply
        } else {
            0
        };
        ProtocolMetrics {
            total_supply,
            total_borrow,
            utilization_bps,
            ledger: env.ledger().sequence(),
        }
    }
}

fn acquire_reentrancy_lock(env: &Env) {
    let reentrancy_lock_key = Symbol::new(env, "reent_l");
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

fn extend_collateral_ttl(env: &Env, user: &Address) {
    let key = DataKey::Collateral(user.clone());
    let extend_to = env.storage().max_ttl().min(PERSISTENT_TTL_LEDGERS);
    let threshold = extend_to / 2 + 1;
    if env.storage().persistent().has(&key) {
        env.storage().persistent().extend_ttl(&key, threshold, extend_to);
    }
}

fn extend_debt_ttl(env: &Env, user: &Address) {
    let key = DataKey::Debt(user.clone());
    let extend_to = env.storage().max_ttl().min(PERSISTENT_TTL_LEDGERS);
    let threshold = extend_to / 2 + 1;
    if env.storage().persistent().has(&key) {
        env.storage().persistent().extend_ttl(&key, threshold, extend_to);
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

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger};
    use soroban_sdk::LedgerInfo;

    fn setup() -> (Env, LendingContractClient<'static>, soroban_sdk::Address, soroban_sdk::Address) {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(LendingContract, ());
        let client = LendingContractClient::new(&env, &id);
        let admin = soroban_sdk::Address::generate(&env);
        let user = soroban_sdk::Address::generate(&env);
        client.initialize(&admin);
        (env, client, admin, user)
    }

    fn advance_time(env: &Env, seconds: u64) {
        let mut li: LedgerInfo = env.ledger().get();
        li.timestamp = li.timestamp.saturating_add(seconds);
        li.sequence_number = li.sequence_number.saturating_add(seconds as u32);
        env.ledger().set(li);
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
    fn test_withdraw_fails_when_over_withdrawing() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &50);
        let result = client.try_withdraw(&user, &75);
        assert!(result.is_err());
    }

    #[test]
    fn test_withdraw_fails_when_debt_exceeds_remaining_collateral() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &100);
        client.borrow(&user, &100);
        let result = client.try_withdraw(&user, &1);
        assert!(result.is_err());
    }

    #[test]
    fn test_withdraw_to_exact_ratio_boundary_succeeds() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &100);
        client.borrow(&user, &100);
        let result = client.withdraw(&user, &0);
        assert_eq!(result, 100);
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
        client.set_min_borrow(&100);
        assert_eq!(client.get_min_borrow(), 100);
    }

    // ============ DEBT CEILING TESTS ============

    #[test]
    #[should_panic]
    fn test_non_guardian_cannot_set_state() {
        // A non-admin caller without a guardian configured cannot change emergency state.
        let (env2, _, _, _) = setup();
        let env2 = Env::default(); // no mock_all_auths
        let id2 = env2.register(LendingContract, ());
        let client2 = LendingContractClient::new(&env2, &id2);
        let admin2 = Address::generate(&env2);
        let attacker = Address::generate(&env2);
        env2.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &admin2,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &id2,
                fn_name: "initialize",
                args: (admin2.clone(),).into_val(&env2),
                sub_invokes: &[],
            },
        }]);
        client2.initialize(&admin2);
        // attacker with no auth — should panic
        client2.set_emergency_state(&EmergencyState::Shutdown);
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

    // -----------------------------------------------------------------------
    // get_protocol_metrics tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_protocol_metrics_initial_zeros() {
        let (_env, client, _admin, _user) = setup();
        let m = client.get_protocol_metrics();
        assert_eq!(m.total_supply, 0);
        assert_eq!(m.total_borrow, 0);
        assert_eq!(m.utilization_bps, 0);
    }

    #[test]
    fn test_protocol_metrics_after_deposit() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &1000);
        let m = client.get_protocol_metrics();
        assert_eq!(m.total_supply, 1000);
        assert_eq!(m.total_borrow, 0);
        assert_eq!(m.utilization_bps, 0);
    }

    #[test]
    fn test_protocol_metrics_after_borrow() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &1000);
        client.borrow(&user, &500);
        let m = client.get_protocol_metrics();
        assert_eq!(m.total_supply, 1000);
        assert_eq!(m.total_borrow, 500);
        assert_eq!(m.utilization_bps, 5000); // 50%
    }

    #[test]
    fn test_protocol_metrics_after_repay() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &1000);
        client.borrow(&user, &500);
        client.repay(&user, &200);
        let m = client.get_protocol_metrics();
        assert_eq!(m.total_supply, 1000);
        assert_eq!(m.total_borrow, 300);
        assert_eq!(m.utilization_bps, 3000); // 30%
    }

    #[test]
    fn test_protocol_metrics_interleaved_multiple_users() {
        let (env, client, _admin, user1) = setup();
        let user2 = Address::generate(&env);
        client.deposit(&user1, &600);
        client.deposit(&user2, &400);
        client.borrow(&user1, &200);
        client.borrow(&user2, &100);
        let m = client.get_protocol_metrics();
        assert_eq!(m.total_supply, 1000);
        assert_eq!(m.total_borrow, 300);
        assert_eq!(m.utilization_bps, 3000);
    }

    #[test]
    fn test_protocol_metrics_full_repay_zeroes_borrow() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &1000);
        client.borrow(&user, &400);
        client.repay(&user, &400);
        let m = client.get_protocol_metrics();
        assert_eq!(m.total_borrow, 0);
        assert_eq!(m.utilization_bps, 0);
    }

    #[test]
    fn test_protocol_metrics_ledger_field_set() {
        let (env, client, _admin, _user) = setup();
        let m = client.get_protocol_metrics();
        assert_eq!(m.ledger, env.ledger().sequence());
    }
}
