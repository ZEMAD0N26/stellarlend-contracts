#![no_std]

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, Address, Env};

/// Default minimum collateral ratio: 150% expressed in basis points.
const DEFAULT_COL_RATIO: i128 = 15_000;

/// Errors returned by the borrow entrypoint.
///
/// # Collateral-ratio invariant
/// After a successful borrow the following must hold:
///
/// ```text
/// collateral * 10_000 / (existing_debt + amount) >= col_ratio
/// ```
///
/// where `col_ratio` is stored under the `"col_ratio"` key (default 15 000 bps = 150 %).
#[contracterror]
#[derive(Clone, Debug, PartialEq)]
pub enum BorrowError {
    /// The borrow would push the collateral ratio below the configured minimum.
    InsufficientCollateral = 1,
    /// Arithmetic overflow during collateral-ratio check.
    Overflow = 2,
}

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

    /// Set the minimum collateral ratio (in basis points, e.g. 15 000 = 150 %).
    ///
    /// Only the admin may call this function.
    pub fn set_collateral_ratio(env: Env, ratio: i128) {
        let admin: Address = env.storage().instance().get(&"admin").unwrap();
        admin.require_auth();
        env.storage().instance().set(&"col_ratio", &ratio);
    }

    /// Return the current minimum collateral ratio in basis points.
    pub fn get_collateral_ratio(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&"col_ratio")
            .unwrap_or(DEFAULT_COL_RATIO)
    }

    /// Deposit collateral for a user.
    pub fn deposit(env: Env, user: Address, amount: i128) -> i128 {
        user.require_auth();
        let key = ("col", user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let new_balance = current + amount;
        env.storage().persistent().set(&key, &new_balance);
        new_balance
    }

    /// Withdraw collateral for a user.
    pub fn withdraw(env: Env, user: Address, amount: i128) -> i128 {
        user.require_auth();
        let key = ("col", user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let new_balance = current - amount;
        env.storage().persistent().set(&key, &new_balance);
        new_balance
    }

    /// Borrow against deposited collateral.
    ///
    /// Enforces the collateral-ratio invariant:
    ///
    /// ```text
    /// collateral * 10_000 / (existing_debt + amount) >= col_ratio
    /// ```
    ///
    /// Returns `BorrowError::InsufficientCollateral` when the ratio would be
    /// violated, and `BorrowError::Overflow` on arithmetic overflow.
    pub fn borrow(env: Env, user: Address, amount: i128) -> Result<i128, BorrowError> {
        user.require_auth();

        let collateral: i128 = env
            .storage()
            .persistent()
            .get(&("col", user.clone()))
            .unwrap_or(0);

        let current_debt: i128 = env
            .storage()
            .persistent()
            .get(&("debt", user.clone()))
            .unwrap_or(0);

        let new_debt = current_debt
            .checked_add(amount)
            .ok_or(BorrowError::Overflow)?;

        // Reject zero-collateral borrows immediately (avoids division by zero
        // in the ratio check and is always under-collateralised).
        if collateral <= 0 {
            return Err(BorrowError::InsufficientCollateral);
        }

        let ratio: i128 = env
            .storage()
            .instance()
            .get(&"col_ratio")
            .unwrap_or(DEFAULT_COL_RATIO);

        // collateral * 10_000 / new_debt >= ratio
        // ⟺  collateral * 10_000 >= ratio * new_debt
        let lhs = collateral
            .checked_mul(10_000)
            .ok_or(BorrowError::Overflow)?;
        let rhs = ratio
            .checked_mul(new_debt)
            .ok_or(BorrowError::Overflow)?;

        if lhs < rhs {
            return Err(BorrowError::InsufficientCollateral);
        }

        env.storage()
            .persistent()
            .set(&("debt", user.clone()), &new_debt);
        Ok(new_debt)
    }

    /// Repay debt.
    pub fn repay(env: Env, user: Address, amount: i128) -> i128 {
        user.require_auth();
        let key = ("debt", user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let new_debt = current - amount;
        env.storage().persistent().set(&key, &new_debt);
        new_debt
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

    // ── existing tests ────────────────────────────────────────────────────────

    #[test]
    fn test_initialize_and_get_admin() {
        let (_env, client, admin, _user) = setup();
        assert_eq!(client.get_admin(), admin);
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
    fn test_repay_decreases_debt() {
        let (_env, client, _admin, user) = setup();
        // Deposit enough collateral first (150 % of 100 = 150).
        client.deposit(&user, &150);
        client.borrow(&user, &100).unwrap();
        assert_eq!(client.repay(&user, &30), 70);
    }

    #[test]
    fn test_position_summary_reflects_state() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &300);
        client.borrow(&user, &100).unwrap(); // 300/100 = 300 % ≥ 150 %
        let pos = client.get_position(&user);
        assert_eq!(pos.collateral, 300);
        assert_eq!(pos.debt, 100);
    }

    #[test]
    fn test_position_summary_default_zero() {
        let (_env, client, _admin, user) = setup();
        let pos = client.get_position(&user);
        assert_eq!(pos.collateral, 0);
        assert_eq!(pos.debt, 0);
    }

    // ── collateral-ratio tests ────────────────────────────────────────────────

    /// Borrow well within the 150 % limit (300 % ratio).
    #[test]
    fn test_borrow_within_limit() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &300);
        // 300 * 10_000 / 100 = 30_000 bps = 300 % ≥ 150 %
        assert_eq!(client.borrow(&user, &100).unwrap(), 100);
    }

    /// Borrow exactly at the 150 % boundary (collateral = 1.5 × amount).
    #[test]
    fn test_borrow_at_boundary() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &150);
        // 150 * 10_000 / 100 = 15_000 bps = 150 % — exactly at limit.
        assert_eq!(client.borrow(&user, &100).unwrap(), 100);
    }

    /// Borrow one unit above the boundary must be rejected.
    #[test]
    fn test_borrow_exceeding_limit() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &149);
        // 149 * 10_000 / 100 = 14_900 bps < 15_000 bps → rejected.
        assert_eq!(
            client.try_borrow(&user, &100),
            Err(Ok(BorrowError::InsufficientCollateral))
        );
    }

    /// Borrow with zero collateral must be rejected.
    #[test]
    fn test_borrow_zero_collateral() {
        let (_env, client, _admin, user) = setup();
        assert_eq!(
            client.try_borrow(&user, &1),
            Err(Ok(BorrowError::InsufficientCollateral))
        );
    }

    /// Cumulative borrows must respect the ratio against total debt.
    #[test]
    fn test_borrow_cumulative_exceeds_limit() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &150);
        // First borrow: 150/100 = 150 % — OK.
        client.borrow(&user, &100).unwrap();
        // Second borrow: 150/(100+1) < 150 % — rejected.
        assert_eq!(
            client.try_borrow(&user, &1),
            Err(Ok(BorrowError::InsufficientCollateral))
        );
    }

    /// Admin can lower the ratio and a previously-rejected borrow succeeds.
    #[test]
    fn test_set_collateral_ratio_allows_lower_ratio() {
        let (_env, client, admin, user) = setup();
        client.deposit(&user, &120);
        // Default 150 % → 120/100 = 120 % < 150 % → rejected.
        assert_eq!(
            client.try_borrow(&user, &100),
            Err(Ok(BorrowError::InsufficientCollateral))
        );
        // Admin lowers ratio to 110 % (11 000 bps).
        client.set_collateral_ratio(&admin, &11_000);
        // 120 * 10_000 / 100 = 12_000 bps ≥ 11_000 bps → accepted.
        assert_eq!(client.borrow(&user, &100).unwrap(), 100);
    }

    /// Default collateral ratio is 15 000 bps (150 %).
    #[test]
    fn test_default_collateral_ratio() {
        let (_env, client, _admin, _user) = setup();
        assert_eq!(client.get_collateral_ratio(), 15_000);
    }
}
