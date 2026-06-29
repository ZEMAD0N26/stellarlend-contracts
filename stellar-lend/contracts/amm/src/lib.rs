//! # StellarLend AMM Contract
//!
//! A minimal constant-product automated market maker (AMM) with a
//! configurable minimum-liquidity floor that prevents reserves from
//! being drained to dust.
//!
//! ## Constant-Product Invariant
//!
//! All swaps and liquidity operations preserve `reserve_a * reserve_b >= k`
//! after applying the minimum-liquidity floor check.
//!
//! ## Minimum-Liquidity Floor
//!
//! A per-pool floor (default `0`) ensures that `remove_liquidity` and
//! `swap_*` cannot reduce either reserve below the configured threshold.
//! This protects the constant-product math from becoming numerically
//! fragile at near-zero reserve levels, where a single swap could move
//! the price wildly.
//!
//! ## Admin Controls
//!
//! The admin can set the minimum-liquidity floor via [`AmmContract::set_min_liquidity`].
//! All admin-gated functions require the admin address to sign the invocation.

#![no_std]

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, Address, Env};

// ---------------------------------------------------------------------------
// Storage keys
// ---------------------------------------------------------------------------

/// Persistent storage keys used by the AMM contract.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataKey {
    /// The reserve balance of token A in the pool.
    ReserveA,
    /// The reserve balance of token B in the pool.
    ReserveB,
    /// The address of token A.
    TokenA,
    /// The address of token B.
    TokenB,
    /// The minimum-liquidity floor — no operation may leave either
    /// reserve below this value.
    MinLiquidity,
    /// The contract admin address.
    Admin,
    /// Flag indicating whether the contract has been initialized.
    Initialized,
}

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Errors returned by the AMM contract.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum AmmError {
    /// The contract has not been initialized yet.
    NotInitialized = 1,
    /// `initialize` was called a second time.
    AlreadyInitialized = 2,
    /// Caller is not the admin.
    Unauthorized = 3,
    /// Amount must be positive.
    InvalidAmount = 4,
    /// Insufficient liquidity in the pool.
    InsufficientLiquidity = 5,
    /// The operation would leave a reserve below the minimum-liquidity floor.
    BelowMinLiquidity = 6,
    /// Swap would exceed the configured slippage tolerance.
    SlippageExceeded = 7,
    /// Checked arithmetic would overflow or underflow.
    Overflow = 8,
    /// The supplied token address does not match either pool token.
    InvalidToken = 9,
}

// ---------------------------------------------------------------------------
// View types
// ---------------------------------------------------------------------------

/// Snapshot of the current pool reserves.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReserveInfo {
    /// Current reserve of token A.
    pub reserve_a: i128,
    /// Current reserve of token B.
    pub reserve_b: i128,
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

/// The StellarLend constant-product AMM contract.
#[contract]
pub struct AmmContract;

#[contractimpl]
impl AmmContract {
    // -----------------------------------------------------------------------
    // Initialization
    // -----------------------------------------------------------------------

    /// Initialize the AMM pool with an admin and two token addresses.
    ///
    /// Can only be called once. The pool starts with zero reserves.
    ///
    /// # Parameters
    /// - `env`: Soroban environment.
    /// - `admin`: Address of the contract admin.
    /// - `token_a`: Address of the first token in the pool.
    /// - `token_b`: Address of the second token in the pool.
    ///
    /// # Panics
    /// - [`AlreadyInitialized`](AmmError::AlreadyInitialized) if called a second time.
    pub fn initialize(env: Env, admin: Address, token_a: Address, token_b: Address) {
        if env.storage().instance().has(&DataKey::Initialized) {
            panic_with_error(&env, AmmError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::TokenA, &token_a);
        env.storage().instance().set(&DataKey::TokenB, &token_b);
        env.storage().instance().set(&DataKey::ReserveA, &0_i128);
        env.storage().instance().set(&DataKey::ReserveB, &0_i128);
        // Default minimum-liquidity floor is 0 (preserves current behaviour).
        env.storage().instance().set(&DataKey::MinLiquidity, &0_i128);
        env.storage().instance().set(&DataKey::Initialized, &true);
    }

    // -----------------------------------------------------------------------
    // Views
    // -----------------------------------------------------------------------

    /// Return the admin address.
    ///
    /// # Panics
    /// - [`NotInitialized`](AmmError::NotInitialized) if the contract has not been initialized.
    pub fn get_admin(env: Env) -> Address {
        require_initialized(&env);
        env.storage().instance().get(&DataKey::Admin).unwrap()
    }

    /// Return the current minimum-liquidity floor.
    ///
    /// Defaults to `0` when the contract is not initialized, preserving
    /// backward-compatible behaviour.
    pub fn get_min_liquidity(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::MinLiquidity)
            .unwrap_or(0)
    }

    /// Return the current pool reserves for both tokens.
    ///
    /// # Panics
    /// - [`NotInitialized`](AmmError::NotInitialized) if the contract has not been initialized.
    pub fn get_reserves(env: Env) -> ReserveInfo {
        require_initialized(&env);
        let a: i128 = env.storage().instance().get(&DataKey::ReserveA).unwrap();
        let b: i128 = env.storage().instance().get(&DataKey::ReserveB).unwrap();
        ReserveInfo {
            reserve_a: a,
            reserve_b: b,
        }
    }

    /// Return the addresses of the two pool tokens.
    ///
    /// # Panics
    /// - [`NotInitialized`](AmmError::NotInitialized) if the contract has not been initialized.
    pub fn get_tokens(env: Env) -> (Address, Address) {
        require_initialized(&env);
        let a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();
        (a, b)
    }

    // -----------------------------------------------------------------------
    // Admin-gated setter
    // -----------------------------------------------------------------------

    /// Set the minimum-liquidity floor.
    ///
    /// After this call, [`remove_liquidity`](AmmContract::remove_liquidity) and
    /// [`swap_exact_a_for_b`](AmmContract::swap_exact_a_for_b) /
    /// [`swap_exact_b_for_a`](AmmContract::swap_exact_b_for_a) will reject
    /// operations that would leave either reserve below `floor`.
    ///
    /// A floor of `0` (the default) preserves the current behaviour with no
    /// restriction.
    ///
    /// # Parameters
    /// - `env`: Soroban environment.
    /// - `floor`: The new minimum-liquidity floor. Must be non-negative.
    ///
    /// # Errors
    /// - [`Unauthorized`](AmmError::Unauthorized) if the caller is not the admin.
    /// - [`NotInitialized`](AmmError::NotInitialized) if the contract has not been initialized.
    /// - [`InvalidAmount`](AmmError::InvalidAmount) if `floor` is negative.
    pub fn set_min_liquidity(env: Env, floor: i128) -> Result<(), AmmError> {
        require_initialized(&env);
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        if floor < 0 {
            return Err(AmmError::InvalidAmount);
        }
        env.storage()
            .instance()
            .set(&DataKey::MinLiquidity, &floor);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Liquidity management
    // -----------------------------------------------------------------------

    /// Add liquidity to the pool.
    ///
    /// The caller provides `amount_a` of token A. The required amount of
    /// token B is computed to maintain the current price ratio. If the pool
    /// is empty (first deposit), any ratio is accepted.
    ///
    /// # Parameters
    /// - `env`: Soroban environment.
    /// - `to`: Address that receives LP tokens (not yet implemented —
    ///   liquidity is tracked proportionally for this MVP).
    /// - `amount_a`: Amount of token A to add (must be positive).
    /// - `amount_b_min`: Minimum amount of token B to accept (slippage protection).
    ///
    /// # Errors
    /// - [`NotInitialized`](AmmError::NotInitialized) if the contract has not been initialized.
    /// - [`InvalidAmount`](AmmError::InvalidAmount) if `amount_a` is not positive.
    /// - [`SlippageExceeded`](AmmError::SlippageExceeded) if the computed B amount
    ///   is below `amount_b_min`.
    /// - [`Overflow`](AmmError::Overflow) if checked arithmetic fails.
    ///
    /// # Returns
    /// The computed amount of token B that must also be provided.
    pub fn add_liquidity(
        env: Env,
        to: Address,
        amount_a: i128,
        amount_b_min: i128,
    ) -> Result<i128, AmmError> {
        require_initialized(&env);
        to.require_auth();

        if amount_a <= 0 {
            return Err(AmmError::InvalidAmount);
        }

        let reserve_a: i128 = env.storage().instance().get(&DataKey::ReserveA).unwrap();
        let reserve_b: i128 = env.storage().instance().get(&DataKey::ReserveB).unwrap();

        let amount_b = if reserve_a == 0 && reserve_b == 0 {
            // First deposit: accept any ratio provided. The caller must supply
            // at least `amount_b_min`, which defaults to 0.
            amount_b_min.max(0)
        } else {
            // Price-constant addition: amount_b = (reserve_b * amount_a) / reserve_a
            let product = reserve_b.checked_mul(amount_a).ok_or(AmmError::Overflow)?;
            let required_b = product
                .checked_div(reserve_a)
                .ok_or(AmmError::Overflow)?;
            if required_b < amount_b_min {
                return Err(AmmError::SlippageExceeded);
            }
            required_b
        };

        // Update reserves with checked arithmetic
        let new_reserve_a = reserve_a.checked_add(amount_a).ok_or(AmmError::Overflow)?;
        let new_reserve_b = reserve_b.checked_add(amount_b).ok_or(AmmError::Overflow)?;

        env.storage()
            .instance()
            .set(&DataKey::ReserveA, &new_reserve_a);
        env.storage()
            .instance()
            .set(&DataKey::ReserveB, &new_reserve_b);

        Ok(amount_b)
    }

    /// Remove liquidity from the pool.
    ///
    /// The caller withdraws `amount_a` of token A and `amount_b` of token B.
    /// Both reserves must remain at or above the configured minimum-liquidity
    /// floor after removal.
    ///
    /// # Parameters
    /// - `env`: Soroban environment.
    /// - `to`: Address that receives the withdrawn tokens.
    /// - `amount_a`: Amount of token A to withdraw (must be positive).
    /// - `amount_b`: Amount of token B to withdraw (must be positive).
    ///
    /// # Errors
    /// - [`NotInitialized`](AmmError::NotInitialized) if the contract has not been initialized.
    /// - [`InvalidAmount`](AmmError::InvalidAmount) if either amount is not positive.
    /// - [`InsufficientLiquidity`](AmmError::InsufficientLiquidity) if either requested
    ///   amount exceeds the current reserve.
    /// - [`BelowMinLiquidity`](AmmError::BelowMinLiquidity) if removing the requested
    ///   amounts would drop a reserve below the minimum-liquidity floor.
    /// - [`Overflow`](AmmError::Overflow) if checked arithmetic fails.
    ///
    /// # Returns
    /// The actual amounts withdrawn as a `(amount_a, amount_b)` tuple.
    pub fn remove_liquidity(
        env: Env,
        to: Address,
        amount_a: i128,
        amount_b: i128,
    ) -> Result<(i128, i128), AmmError> {
        require_initialized(&env);
        to.require_auth();

        if amount_a <= 0 || amount_b <= 0 {
            return Err(AmmError::InvalidAmount);
        }

        let reserve_a: i128 = env.storage().instance().get(&DataKey::ReserveA).unwrap();
        let reserve_b: i128 = env.storage().instance().get(&DataKey::ReserveB).unwrap();

        if amount_a > reserve_a || amount_b > reserve_b {
            return Err(AmmError::InsufficientLiquidity);
        }

        let new_reserve_a = reserve_a.checked_sub(amount_a).ok_or(AmmError::Overflow)?;
        let new_reserve_b = reserve_b.checked_sub(amount_b).ok_or(AmmError::Overflow)?;

        // Enforce minimum-liquidity floor
        let floor: i128 = env
            .storage()
            .instance()
            .get(&DataKey::MinLiquidity)
            .unwrap_or(0);
        if new_reserve_a < floor {
            return Err(AmmError::BelowMinLiquidity);
        }
        if new_reserve_b < floor {
            return Err(AmmError::BelowMinLiquidity);
        }

        env.storage()
            .instance()
            .set(&DataKey::ReserveA, &new_reserve_a);
        env.storage()
            .instance()
            .set(&DataKey::ReserveB, &new_reserve_b);

        Ok((amount_a, amount_b))
    }

    // -----------------------------------------------------------------------
    // Swaps
    // -----------------------------------------------------------------------

    /// Swap an exact amount of token A for token B.
    ///
    /// The constant-product invariant `reserve_a * reserve_b` must not
    /// decrease. The output is computed via:
    ///
    /// ```text
    /// amount_b_out = (reserve_b * amount_a_in) / (reserve_a + amount_a_in)
    /// ```
    ///
    /// After the swap, the remaining reserve of token B must be at or above
    /// the minimum-liquidity floor.
    ///
    /// # Parameters
    /// - `env`: Soroban environment.
    /// - `to`: Address that receives token B.
    /// - `amount_a_in`: Amount of token A to swap in (must be positive).
    /// - `amount_b_out_min`: Minimum amount of token B to receive (slippage protection).
    ///
    /// # Errors
    /// - [`NotInitialized`](AmmError::NotInitialized) if the contract has not been initialized.
    /// - [`InvalidAmount`](AmmError::InvalidAmount) if `amount_a_in` is not positive.
    /// - [`InsufficientLiquidity`](AmmError::InsufficientLiquidity) if the pool has
    ///   no liquidity.
    /// - [`SlippageExceeded`](AmmError::SlippageExceeded) if the computed output is
    ///   below `amount_b_out_min`.
    /// - [`BelowMinLiquidity`](AmmError::BelowMinLiquidity) if the swap would leave
    ///   reserve B below the minimum-liquidity floor.
    /// - [`Overflow`](AmmError::Overflow) if checked arithmetic fails.
    ///
    /// # Returns
    /// The actual amount of token B received.
    pub fn swap_exact_a_for_b(
        env: Env,
        to: Address,
        amount_a_in: i128,
        amount_b_out_min: i128,
    ) -> Result<i128, AmmError> {
        require_initialized(&env);
        to.require_auth();

        if amount_a_in <= 0 {
            return Err(AmmError::InvalidAmount);
        }

        let reserve_a: i128 = env.storage().instance().get(&DataKey::ReserveA).unwrap();
        let reserve_b: i128 = env.storage().instance().get(&DataKey::ReserveB).unwrap();

        if reserve_a == 0 || reserve_b == 0 {
            return Err(AmmError::InsufficientLiquidity);
        }

        // amount_b_out = (reserve_b * amount_a_in) / (reserve_a + amount_a_in)
        let numerator = reserve_b.checked_mul(amount_a_in).ok_or(AmmError::Overflow)?;
        let denominator = reserve_a.checked_add(amount_a_in).ok_or(AmmError::Overflow)?;
        let amount_b_out = numerator.checked_div(denominator).ok_or(AmmError::Overflow)?;

        if amount_b_out <= 0 {
            return Err(AmmError::InvalidAmount);
        }
        if amount_b_out < amount_b_out_min {
            return Err(AmmError::SlippageExceeded);
        }

        let new_reserve_a = reserve_a.checked_add(amount_a_in).ok_or(AmmError::Overflow)?;
        let new_reserve_b = reserve_b.checked_sub(amount_b_out).ok_or(AmmError::Overflow)?;

        // Enforce minimum-liquidity floor on the outgoing reserve
        let floor: i128 = env
            .storage()
            .instance()
            .get(&DataKey::MinLiquidity)
            .unwrap_or(0);
        if new_reserve_b < floor {
            return Err(AmmError::BelowMinLiquidity);
        }

        env.storage()
            .instance()
            .set(&DataKey::ReserveA, &new_reserve_a);
        env.storage()
            .instance()
            .set(&DataKey::ReserveB, &new_reserve_b);

        Ok(amount_b_out)
    }

    /// Swap an exact amount of token B for token A.
    ///
    /// Symmetric to [`swap_exact_a_for_b`](AmmContract::swap_exact_a_for_b) with
    /// the roles of A and B reversed.
    ///
    /// After the swap, the remaining reserve of token A must be at or above
    /// the minimum-liquidity floor.
    ///
    /// # Parameters
    /// - `env`: Soroban environment.
    /// - `to`: Address that receives token A.
    /// - `amount_b_in`: Amount of token B to swap in (must be positive).
    /// - `amount_a_out_min`: Minimum amount of token A to receive (slippage protection).
    ///
    /// # Errors
    /// - [`NotInitialized`](AmmError::NotInitialized) if the contract has not been initialized.
    /// - [`InvalidAmount`](AmmError::InvalidAmount) if `amount_b_in` is not positive.
    /// - [`InsufficientLiquidity`](AmmError::InsufficientLiquidity) if the pool has
    ///   no liquidity.
    /// - [`SlippageExceeded`](AmmError::SlippageExceeded) if the computed output is
    ///   below `amount_a_out_min`.
    /// - [`BelowMinLiquidity`](AmmError::BelowMinLiquidity) if the swap would leave
    ///   reserve A below the minimum-liquidity floor.
    /// - [`Overflow`](AmmError::Overflow) if checked arithmetic fails.
    ///
    /// # Returns
    /// The actual amount of token A received.
    pub fn swap_exact_b_for_a(
        env: Env,
        to: Address,
        amount_b_in: i128,
        amount_a_out_min: i128,
    ) -> Result<i128, AmmError> {
        require_initialized(&env);
        to.require_auth();

        if amount_b_in <= 0 {
            return Err(AmmError::InvalidAmount);
        }

        let reserve_a: i128 = env.storage().instance().get(&DataKey::ReserveA).unwrap();
        let reserve_b: i128 = env.storage().instance().get(&DataKey::ReserveB).unwrap();

        if reserve_a == 0 || reserve_b == 0 {
            return Err(AmmError::InsufficientLiquidity);
        }

        // amount_a_out = (reserve_a * amount_b_in) / (reserve_b + amount_b_in)
        let numerator = reserve_a.checked_mul(amount_b_in).ok_or(AmmError::Overflow)?;
        let denominator = reserve_b.checked_add(amount_b_in).ok_or(AmmError::Overflow)?;
        let amount_a_out = numerator.checked_div(denominator).ok_or(AmmError::Overflow)?;

        if amount_a_out <= 0 {
            return Err(AmmError::InvalidAmount);
        }
        if amount_a_out < amount_a_out_min {
            return Err(AmmError::SlippageExceeded);
        }

        let new_reserve_b = reserve_b.checked_add(amount_b_in).ok_or(AmmError::Overflow)?;
        let new_reserve_a = reserve_a.checked_sub(amount_a_out).ok_or(AmmError::Overflow)?;

        // Enforce minimum-liquidity floor on the outgoing reserve
        let floor: i128 = env
            .storage()
            .instance()
            .get(&DataKey::MinLiquidity)
            .unwrap_or(0);
        if new_reserve_a < floor {
            return Err(AmmError::BelowMinLiquidity);
        }

        env.storage()
            .instance()
            .set(&DataKey::ReserveA, &new_reserve_a);
        env.storage()
            .instance()
            .set(&DataKey::ReserveB, &new_reserve_b);

        Ok(amount_a_out)
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Require that the contract has been initialized, panicking with
/// [`NotInitialized`](AmmError::NotInitialized) if not.
fn require_initialized(env: &Env) {
    if !env.storage().instance().has(&DataKey::Initialized) {
        panic_with_error(env, AmmError::NotInitialized);
    }
}

/// Panic with a given error value.
fn panic_with_error(env: &Env, error: AmmError) -> ! {
    panic!("{}", error as u32)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod test;
