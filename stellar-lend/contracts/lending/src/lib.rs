#![no_std]

mod debt;
pub mod rounding_strategy;

#[cfg(test)]
mod interest_drift_regression_test;

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, Address, Bytes, Env, Symbol, IntoVal};
use debt::{borrow_amount, load_debt, save_debt, DebtPosition, DEFAULT_APR_BPS, repay_amount, effective_debt};

/// Maximum desired persistent TTL for position entries, in ledgers.
const PERSISTENT_TTL_LEDGERS: u32 = 1_000_000;

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
    /// The super-admin address.
    Admin,
    /// Pending admin during a two-step rotation.
    PendingAdmin,
    /// Global circuit-breaker state.
    EmergencyState,
    /// Optional guardian address (defaults to admin when unset).
    Guardian,
    /// Minimum borrow amount enforced by `borrow`.
    BorrowMinAmount,
    /// Reentrancy guard flag for flash-loan callbacks.
    FlashActive,
    /// Flash-loan fee in basis points.
    FlashFeeBps,
    /// Per-user collateral balance.
    Collateral(Address),
    /// Per-user debt position key (managed by `debt` module).
    Debt(Address),
    /// Per-asset per-account balance (used by flash-loan book-keeping).
    Balance(Address, Address),
    /// Per-asset treasury balance.
    Treasury(Address),
    /// Total outstanding debt across all users.
    TotalDebt,
    /// Total deposited collateral across all users.
    TotalDeposits,
    /// Protocol-level debt ceiling.
    DebtCeiling,
    /// Protocol-level deposit cap.
    DepositCap,
}

// ---------------------------------------------------------------------------
// Emergency circuit-breaker
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Load the stored admin address.
///
/// Returns `LendingError::NotInitialized` if the contract has never been
/// initialized.
fn get_admin_internal(env: &Env) -> Result<Address, LendingError> {
    env.storage()
        .instance()
        .get(&DataKey::Admin)
        .ok_or(LendingError::NotInitialized)
}

/// **Auth boundary**: load the stored admin and call `require_auth()` on it.
///
/// Every privileged setter MUST call this helper before touching protocol state.
/// Returning `Err` here means the contract is uninitialized; Soroban will
/// surface an auth failure if `require_auth` is not satisfied.
fn require_admin(env: &Env) -> Result<Address, LendingError> {
    let admin = get_admin_internal(env)?;
    admin.require_auth();
    Ok(admin)
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
        EmergencyState::Shutdown => {
            panic!("OperationDisabledDuringShutdown");
        }
        EmergencyState::Recovery => match action {
            ProtocolAction::Repay | ProtocolAction::Withdraw => {}
            _ => panic!("ActionBlockedInRecovery"),
        },
    }
}

fn panic_with_debt_error() -> ! {
    panic!("debt operation failed");
}

/// Extend the TTL of a user's collateral entry to prevent archival.
fn extend_collateral_ttl(env: &Env, user: &Address) {
    let key = DataKey::Collateral(user.clone());
    let max_ttl = env.storage().max_ttl();
    let threshold = max_ttl.saturating_sub(PERSISTENT_TTL_LEDGERS / 10);
    env.storage()
        .persistent()
        .extend_ttl(&key, threshold, max_ttl);
}

/// Extend the TTL of a user's debt entry to prevent archival.
fn extend_debt_ttl(env: &Env, user: &Address) {
    let key = DataKey::Debt(user.clone());
    let max_ttl = env.storage().max_ttl();
    let threshold = max_ttl.saturating_sub(PERSISTENT_TTL_LEDGERS / 10);
    env.storage()
        .persistent()
        .extend_ttl(&key, threshold, max_ttl);
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[contract]
pub struct LendingContract;

#[contractimpl]
impl LendingContract {
    // -----------------------------------------------------------------------
    // Initialization
    // -----------------------------------------------------------------------

    /// Initialize the contract with a super-admin address.
    ///
    /// # Security
    /// This function can only succeed **once**. A second call is rejected with
    /// `LendingError::AlreadyInitialized`, preventing any party from seizing
    /// admin rights after deployment.
    ///
    /// Note: `initialize` intentionally does **not** call `require_auth` on
    /// `admin` — the deployer is trusted at construction time (mirrors the
    /// conventional pattern used across Soroban protocol contracts).
    pub fn initialize(env: Env, admin: Address) -> Result<(), LendingError> {
        // Already-initialized guard — the single most important auth boundary
        // in this contract.  Without this check anyone could call initialize
        // again and overwrite the stored admin.
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(LendingError::AlreadyInitialized);
        }

        env.storage().instance().set(&DataKey::Admin, &admin);
        set_emergency_state_internal(&env, EmergencyState::Normal);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Admin queries
    // -----------------------------------------------------------------------

    /// Return the current admin address.
    ///
    /// Returns `LendingError::NotInitialized` if the contract has not been
    /// initialized yet; callers should use `try_get_admin` and handle the
    /// error explicitly.
    pub fn get_admin(env: Env) -> Result<Address, LendingError> {
        get_admin_internal(&env)
    }

    // -----------------------------------------------------------------------
    // Two-step admin rotation
    // -----------------------------------------------------------------------

    /// Propose a new admin (current admin only).
    ///
    /// # Auth boundary
    /// Only the current admin is permitted to nominate a successor.
    pub fn propose_admin(env: Env, new_admin: Address) -> Result<(), LendingError> {
        require_admin(&env)?;
        env.storage().instance().set(&DataKey::PendingAdmin, &new_admin);
        Ok(())
    }

    /// Accept the proposed admin role (proposed admin only).
    ///
    /// The pending admin must `require_auth` themselves, completing the
    /// two-step handover and preventing accidental transfers.
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

    // -----------------------------------------------------------------------
    // Privileged configuration setters
    // -----------------------------------------------------------------------

    /// Set the minimum borrow amount.
    ///
    /// # Auth boundary — admin only
    pub fn set_min_borrow(env: Env, min_borrow: i128) -> Result<(), LendingError> {
        require_admin(&env)?;
        env.storage().instance().set(&DataKey::BorrowMinAmount, &min_borrow);
        Ok(())
    }

    /// Get the minimum borrow amount (no auth required).
    pub fn get_min_borrow(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::BorrowMinAmount)
            .unwrap_or(0)
    }

    /// Set the protocol-level debt ceiling.
    ///
    /// # Auth boundary — admin only
    pub fn set_debt_ceiling(env: Env, ceiling: i128) -> Result<(), LendingError> {
        require_admin(&env)?;
        env.storage().persistent().set(&DataKey::DebtCeiling, &ceiling);
        Ok(())
    }

    /// Set the flash-loan fee in basis points (0–1000).
    ///
    /// # Auth boundary — admin only
    pub fn set_flash_fee(env: Env, fee_bps: i128) -> Result<(), LendingError> {
        require_admin(&env)?;
        const MAX_FEE: i128 = 1_000;
        if !(0..=MAX_FEE).contains(&fee_bps) {
            return Err(LendingError::InvalidFeeBps);
        }
        env.storage().instance().set(&DataKey::FlashFeeBps, &fee_bps);
        Ok(())
    }

    /// Privileged function to update the global emergency state.
    ///
    /// # Auth boundary — admin or guardian
    /// The guardian address (if set) is authorised alongside the admin so that
    /// an emergency operator can pause the protocol without requiring the admin
    /// key.
    pub fn set_emergency_state(env: Env, new_state: EmergencyState) -> Result<(), LendingError> {
        // Allow either the guardian (if configured) or the admin.
        let admin = get_admin_internal(&env)?;
        let guardian: Address = env
            .storage()
            .instance()
            .get(&DataKey::Guardian)
            .unwrap_or_else(|| admin.clone());
        guardian.require_auth();

        let old_state = get_emergency_state(&env);
        set_emergency_state_internal(&env, new_state);

        env.events().publish(
            (Symbol::new(&env, "EmergencyStateChanged"),),
            (old_state, new_state),
        );
        Ok(())
    }

    /// Set a dedicated guardian address.
    ///
    /// # Auth boundary — admin only
    pub fn set_guardian(env: Env, guardian: Address) -> Result<(), LendingError> {
        require_admin(&env)?;
        env.storage().instance().set(&DataKey::Guardian, &guardian);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Core lending operations
    // -----------------------------------------------------------------------

    /// Deposit collateral for `user`.
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
            .unwrap_or_else(|| panic!("Overflow"));
        if new_total > deposit_cap {
            panic!("DepositCapExceeded");
        }
        env.storage().persistent().set(&DataKey::TotalDeposits, &new_total);

        let key = DataKey::Collateral(user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let new_balance = current
            .checked_add(amount)
            .unwrap_or_else(|| panic!("Overflow"));
        env.storage().persistent().set(&key, &new_balance);
        extend_collateral_ttl(&env, &user);
        new_balance
    }

    /// Withdraw collateral for `user`.
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
            panic!("InsufficientCollateral");
        }
        let new_balance = current
            .checked_sub(amount)
            .unwrap_or_else(|| panic!("Overflow"));
        env.storage().persistent().set(&key, &new_balance);
        extend_collateral_ttl(&env, &user);
        new_balance
    }

    /// Borrow against deposited collateral. Enforces the protocol-level debt ceiling.
    pub fn borrow(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
        check_emergency_status(&env, ProtocolAction::Borrow);

        user.require_auth();
        let min_borrow = Self::get_min_borrow(env.clone());
        if amount < min_borrow {
            panic!("BelowMinimumBorrow");
        }

        // Debt ceiling check
        let total_debt: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::TotalDebt)
            .unwrap_or(0);
        let debt_ceiling: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::DebtCeiling)
            .unwrap_or(i128::MAX);
        let new_total_debt = total_debt
            .checked_add(amount)
            .ok_or(LendingError::Overflow)?;
        if new_total_debt > debt_ceiling {
            return Err(LendingError::DebtCeilingExceeded);
        }
        env.storage().persistent().set(&DataKey::TotalDebt, &new_total_debt);

        let now = env.ledger().timestamp();
        let position = load_debt(&env, &user);
        let updated = borrow_amount(position, now, amount, DEFAULT_APR_BPS)
            .unwrap_or_else(|_| panic_with_debt_error());
        save_debt(&env, &user, &updated);
        extend_debt_ttl(&env, &user);
        Ok(updated.principal)
    }

    /// Repay outstanding debt for `user`.
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
            .unwrap_or_else(|_| panic_with_debt_error());
        save_debt(&env, &user, &updated);
        extend_debt_ttl(&env, &user);
        updated.principal
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

        const LIQUIDATION_THRESHOLD: i128 = 8_000;
        let hf = (collateral * LIQUIDATION_THRESHOLD) / debt;
        if hf >= 10_000 {
            return Err(LendingError::PositionHealthy);
        }

        const CLOSE_FACTOR: i128 = 5_000;
        let max_repay = (debt * CLOSE_FACTOR) / 10_000;
        let actual_repay = amount.min(max_repay);

        const INCENTIVE_BPS: i128 = 1_000;
        let seized_collateral = (actual_repay * (10_000 + INCENTIVE_BPS)) / 10_000;
        let final_seized = seized_collateral.min(collateral);

        let new_debt = debt - actual_repay;
        let new_col  = collateral - final_seized;

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

    /// Get a user's collateral, effective debt, and health factor.
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
            1_000_000
        };

        PositionSummary { collateral: col, debt, health_factor }
    }

    // -----------------------------------------------------------------------
    // Flash-loan
    // -----------------------------------------------------------------------

    fn get_flash_fee_bps(env: &Env) -> i128 {
        env.storage().instance().get(&DataKey::FlashFeeBps).unwrap_or(5)
    }

    /// Execute a flash loan.
    pub fn flash_loan(
        env: Env,
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

        receiver.require_auth();

        let fee_bps = Self::get_flash_fee_bps(&env);
        let fee = amount
            .checked_mul(fee_bps)
            .map(|v| v / 10_000)
            .expect("fee overflow");

        let new_tre_bal = tre_bal.checked_sub(amount).expect("treasury underflow");
        env.storage().persistent().set(&tre_key, &new_tre_bal);

        let rec_key = DataKey::Balance(asset.clone(), receiver.clone());
        let rec_bal: i128 = env.storage().persistent().get(&rec_key).unwrap_or(0);
        let new_rec_bal = rec_bal.checked_add(amount).expect("receiver overflow");
        env.storage().persistent().set(&rec_key, &new_rec_bal);

        env.storage().instance().set(&DataKey::FlashActive, &true);

        let method = Symbol::new(&env, "on_flash_loan");
        env.invoke_contract::<()>(
            &receiver,
            &method,
            (receiver.clone(), asset.clone(), amount, fee, params).into_val(&env),
        );

        env.storage().instance().set(&DataKey::FlashActive, &false);

        let final_tre: i128 = env.storage().persistent().get(&tre_key).unwrap_or(0);
        let required_balance = tre_bal.checked_add(fee).expect("required balance overflow");
        if final_tre < required_balance {
            panic!("InsufficientRepayment");
        }
    }

    /// Repay a flash loan from within the callback.
    pub fn repay_flash_loan(env: Env, payer: Address, asset: Address, amount: i128) {
        payer.require_auth();

        let payer_key = DataKey::Balance(asset.clone(), payer.clone());
        let payer_bal: i128 = env.storage().persistent().get(&payer_key).unwrap_or(0);
        if payer_bal < amount {
            panic!("InsufficientBalance");
        }
        let new_payer_bal = payer_bal.checked_sub(amount).expect("payer underflow");
        env.storage().persistent().set(&payer_key, &new_payer_bal);

        let tre_key = DataKey::Treasury(asset.clone());
        let tre_bal: i128 = env.storage().persistent().get(&tre_key).unwrap_or(0);
        let new_tre_bal = tre_bal.checked_add(amount).expect("treasury overflow");
        env.storage().persistent().set(&tre_key, &new_tre_bal);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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
        assert_eq!(client.get_admin().unwrap(), admin);
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
        let result = client.borrow(&user, &50).unwrap();
        assert_eq!(result, 50);
    }

    #[test]
    fn test_repay_decreases_debt() {
        let (_env, client, _admin, user) = setup();
        client.borrow(&user, &100).unwrap();
        let result = client.repay(&user, &30);
        assert_eq!(result, 70);
    }

    #[test]
    fn test_position_summary_reflects_state() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &200);
        client.borrow(&user, &75).unwrap();
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
    #[should_panic(expected = "ActionBlockedInRecovery")]
    fn test_recovery_blocks_deposit() {
        let (_env, client, _admin, user) = setup();
        client.set_emergency_state(&EmergencyState::Recovery).unwrap();
        client.deposit(&user, &10);
    }

    #[test]
    #[should_panic(expected = "ActionBlockedInRecovery")]
    fn test_recovery_blocks_borrow() {
        let (_env, client, _admin, user) = setup();
        client.set_emergency_state(&EmergencyState::Recovery).unwrap();
        client.borrow(&user, &10).unwrap();
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
}
