#![no_std]

use soroban_sdk::{contract, contracterror, contractevent, contractimpl, contracttype, Address, Env, Vec};

// ===========================================================================
// Error types
// ===========================================================================

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum VestingError {
    /// Contract has not been initialized yet.
    NotInitialized = 2000,
    /// `initialize` was called a second time.
    AlreadyInitialized = 2001,
    /// Caller is not the admin.
    Unauthorized = 2002,
    /// Amount must be strictly positive.
    InvalidAmount = 2003,
    /// The provided vesting schedule fails validation.
    InvalidSchedule = 2004,
    /// Claim amount exceeds currently vested less already claimed.
    InsufficientVested = 2005,
    /// A checked arithmetic operation overflowed.
    Overflow = 2006,
    /// No grant exists for the requested recipient.
    GrantNotFound = 2007,
    /// A grant already exists for this recipient.
    GrantAlreadyExists = 2008,
    /// Milestones must be strictly increasing in timestamp.
    InvalidMilestoneOrder = 2009,
    /// Final milestone cumulative must equal the grant principal.
    InvalidMilestoneCumulative = 2010,
    /// Milestone schedule must contain at least one milestone.
    EmptyMilestones = 2011,
}

// ===========================================================================
// Data keys
// ===========================================================================

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataKey {
    Admin,
    Grant(Address),
}

// ===========================================================================
// Vesting schedule types
// ===========================================================================

/// Variants representing the two supported vesting strategies.
///
/// ## Linear
/// Tokens vest continuously from `start` to `end`, with nothing vested
/// before the `cliff` timestamp.  At `cliff` a lump sum proportional to
/// `(cliff - start) / (end - start)` unlocks, then the remaining fraction
/// vests linearly from `cliff` to `end`.
///
/// * `Linear.0` – `start` timestamp (seconds since epoch).  
/// * `Linear.1` – `cliff` timestamp: no tokens vest before this time.  
/// * `Linear.2` – `end` timestamp: 100 % vested at/after this time.  
///
/// ## Milestone
/// Tokens vest in discrete tranches at fixed timestamps.  At a given
/// time the vested amount equals the `cumulative_amount` of the latest
/// milestone whose timestamp has passed.  If no milestone has passed the
/// vested amount is zero.
///
/// * `Milestone.0` – Ordered `Vec` of `(timestamp, cumulative_amount)` pairs.  
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VestingSchedule {
    Linear(u64, u64, u64),
    Milestone(Vec<(u64, i128)>),
}

// ===========================================================================
// Grant
// ===========================================================================

/// Per-recipient grant stored in persistent storage.
///
/// ## Fields
/// * `principal` – Total tokens allocated to this grant.  
/// * `claimed`   – Tokens the recipient has already claimed.  
/// * `schedule`  – The vesting curve ([`Linear`](VestingSchedule::Linear) or
///   [`Milestone`](VestingSchedule::Milestone)).  
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub principal: i128,
    pub claimed: i128,
    pub schedule: VestingSchedule,
}

// ===========================================================================
// Events
// ===========================================================================

/// Emitted when a new grant is created via `add_grant`.
#[contractevent]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantCreatedEvent {
    pub recipient: Address,
    pub principal: i128,
}

/// Emitted when tokens are claimed via `claim`.
#[contractevent]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimedEvent {
    pub recipient: Address,
    pub amount: i128,
    pub new_claimed: i128,
}

// ===========================================================================
// Contract
// ===========================================================================

#[contract]
pub struct VestingContract;

#[contractimpl]
impl VestingContract {
    // ------------------------------------------------------------------
    // Admin
    // ------------------------------------------------------------------

    /// One-time initialisation.  Stores the admin address.
    ///
    /// # Panics
    /// If called more than once.
    pub fn initialize(env: Env, admin: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("AlreadyInitialized");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
    }

    /// Return the stored admin address.
    ///
    /// # Panics
    /// If the contract has not been initialised.
    pub fn get_admin(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("NotInitialized")
    }

    // ------------------------------------------------------------------
    // Grant management
    // ------------------------------------------------------------------

    /// Create a new vesting grant (admin-only).
    ///
    /// Validates the schedule before persisting:
    ///
    /// * `principal` must be > 0.
    /// * A grant must not already exist for `recipient`.
    ///
    /// **Linear schedule validation:**
    /// * `start <= cliff < end` (strictly increasing).  
    ///
    /// **Milestone schedule validation:**
    /// * At least one milestone.  
    /// * Timestamps and cumulative amounts are strictly increasing.  
    /// * Final cumulative amount equals `principal`.  
    ///
    /// # Returns
    /// The newly created [`Grant`].
    ///
    /// # Errors
    /// Returns [`VestingError`] variants for validation failures.
    pub fn add_grant(
        env: Env,
        admin: Address,
        recipient: Address,
        principal: i128,
        schedule: VestingSchedule,
    ) -> Result<Grant, VestingError> {
        admin.require_auth();
        Self::require_admin(&env, &admin)?;

        if principal <= 0 {
            return Err(VestingError::InvalidAmount);
        }

        let key = DataKey::Grant(recipient.clone());
        if env.storage().persistent().has(&key) {
            return Err(VestingError::GrantAlreadyExists);
        }

        // Validate the schedule variant
        validate_schedule(&schedule, principal)?;

        let grant = Grant {
            principal,
            claimed: 0,
            schedule,
        };

        env.storage().persistent().set(&key, &grant);

        GrantCreatedEvent {
            recipient,
            principal,
        }
        .publish(&env);

        Ok(grant)
    }

    /// Read the stored grant for a recipient.
    ///
    /// # Errors
    /// [`VestingError::GrantNotFound`] if no grant exists.
    pub fn get_grant(env: Env, recipient: Address) -> Result<Grant, VestingError> {
        let key = DataKey::Grant(recipient);
        env.storage()
            .persistent()
            .get(&key)
            .ok_or(VestingError::GrantNotFound)
    }

    // ------------------------------------------------------------------
    // Vesting computation
    // ------------------------------------------------------------------

    /// Compute the vested token amount for a recipient at an arbitrary
    /// `timestamp` (seconds since epoch).
    ///
    /// This is a **pure view** – it does not mutate storage.
    ///
    /// * Linear: `0` before cliff; linear interpolation between cliff and end;  
    ///   `principal` at/after end.  
    /// * Milestone: cumulative amount of the latest milestone whose timestamp
    ///   is ≤ `timestamp`; `0` if no milestone has passed.  
    ///
    /// # Errors
    /// [`VestingError::GrantNotFound`] if no grant exists.
    pub fn vested_at(
        env: Env,
        recipient: Address,
        timestamp: u64,
    ) -> Result<i128, VestingError> {
        let grant = Self::get_grant(env, recipient)?;
        Ok(compute_vested(&grant, timestamp))
    }

    /// Convenience view: vested amount right now minus already claimed.
    ///
    /// Equivalent to `vested_at(now) - grant.claimed`, clamped to 0.
    ///
    /// # Errors
    /// [`VestingError::GrantNotFound`] if no grant exists.
    pub fn claimable(env: Env, recipient: Address) -> Result<i128, VestingError> {
        let grant = Self::get_grant(env, recipient)?;
        let now = env.ledger().timestamp();
        let vested = compute_vested(&grant, now);
        let claimable = vested
            .checked_sub(grant.claimed)
            .unwrap_or(0);
        Ok(claimable)
    }

    // ------------------------------------------------------------------
    // Claiming
    // ------------------------------------------------------------------

    /// Claim `amount` of vested tokens (recipient must authorise).
    ///
    /// `amount` must not exceed the currently claimable balance.
    /// The caller (`recipient`) must invoke `require_auth`.
    ///
    /// # Returns
    /// The updated [`Grant`] with the incremented `claimed` field.
    ///
    /// # Errors
    /// * [`VestingError::GrantNotFound`] – no grant for this recipient.  
    /// * [`VestingError::InvalidAmount`] – `amount` ≤ 0.  
    /// * [`VestingError::InsufficientVested`] – `amount > claimable`.  
    pub fn claim(
        env: Env,
        recipient: Address,
        amount: i128,
    ) -> Result<Grant, VestingError> {
        recipient.require_auth();

        if amount <= 0 {
            return Err(VestingError::InvalidAmount);
        }

        let key = DataKey::Grant(recipient.clone());
        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(VestingError::GrantNotFound)?;

        let now = env.ledger().timestamp();
        let vested = compute_vested(&grant, now);
        let claimable = vested
            .checked_sub(grant.claimed)
            .unwrap_or(0);

        if amount > claimable {
            return Err(VestingError::InsufficientVested);
        }

        grant.claimed = grant
            .claimed
            .checked_add(amount)
            .ok_or(VestingError::Overflow)?;

        env.storage().persistent().set(&key, &grant);

        ClaimedEvent {
            recipient,
            amount,
            new_claimed: grant.claimed,
        }
        .publish(&env);

        Ok(grant)
    }

    /// Synchronise the recipient's storage state.  In the current model
    /// vested is always computed on-the-fly from the schedule, so this
    /// is a no-op that simply returns the current claimable amount.
    ///
    /// # Errors
    /// [`VestingError::GrantNotFound`] if no grant exists.
    pub fn sync(env: Env, recipient: Address) -> Result<i128, VestingError> {
        Self::claimable(env, recipient)
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    fn require_admin(env: &Env, caller: &Address) -> Result<(), VestingError> {
        let stored: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(VestingError::NotInitialized)?;
        if caller != &stored {
            return Err(VestingError::Unauthorized);
        }
        Ok(())
    }
}

// ===========================================================================
// Pure functions (no Env needed)
// ===========================================================================

/// Panic-free vested-amount computation for an in-memory [`Grant`].
///
/// Uses checked arithmetic throughout; overflow returns 0 as a safe
/// sentinel (should be unreachable with realistic principal values).
fn compute_vested(grant: &Grant, timestamp: u64) -> i128 {
    match &grant.schedule {
        VestingSchedule::Linear(start, cliff, end) => {
            let start = *start;
            let cliff = *cliff;
            let end = *end;

            if timestamp < cliff {
                return 0;
            }
            if timestamp >= end {
                return grant.principal;
            }

            let elapsed = timestamp.saturating_sub(start);
            let total = end.saturating_sub(start);
            if total == 0 {
                return grant.principal;
            }

            grant
                .principal
                .checked_mul(elapsed as i128)
                .and_then(|v| v.checked_div(total as i128))
                .unwrap_or(0)
        }
        VestingSchedule::Milestone(milestones) => {
            let mut vested: i128 = 0;
            for milestone in milestones.iter() {
                let (milestone_ts, cumulative) = milestone;
                if timestamp >= milestone_ts {
                    vested = cumulative;
                } else {
                    // Milestones are ordered – stop early.
                    break;
                }
            }
            vested
        }
    }
}

// ===========================================================================
// Schedule validation
// ===========================================================================

/// Validate a [`VestingSchedule`] against the grant `principal`.
///
/// ## Linear
/// * `start <= cliff` – cliff cannot precede start.  
/// * `cliff < end`   – vesting must complete after the cliff.  
/// * `start < end`   – overall duration must be positive.  
///
/// ## Milestone
/// * At least one milestone.  
/// * Timestamps strictly increasing.  
/// * Cumulative amounts strictly increasing.  
/// * Final cumulative amount == `principal`.  
fn validate_schedule(schedule: &VestingSchedule, principal: i128) -> Result<(), VestingError> {
    match schedule {
        VestingSchedule::Linear(start, cliff, end) => {
            let s = *start;
            let c = *cliff;
            let e = *end;
            if s > c {
                return Err(VestingError::InvalidSchedule);
            }
            if c >= e {
                return Err(VestingError::InvalidSchedule);
            }
            Ok(())
        }
        VestingSchedule::Milestone(milestones) => {
            if milestones.is_empty() {
                return Err(VestingError::EmptyMilestones);
            }

            let mut prev_ts: u64 = 0;
            let mut prev_cum: i128 = 0;

            for milestone in milestones.iter() {
                let (ts, cum) = milestone;

                if ts <= prev_ts {
                    return Err(VestingError::InvalidMilestoneOrder);
                }
                if cum <= prev_cum {
                    return Err(VestingError::InvalidMilestoneOrder);
                }
                if cum > principal {
                    return Err(VestingError::InvalidMilestoneCumulative);
                }

                prev_ts = ts;
                prev_cum = cum;
            }

            if prev_cum != principal {
                return Err(VestingError::InvalidMilestoneCumulative);
            }

            Ok(())
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod milestone_schedule_test;

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger, LedgerInfo};

    fn setup() -> (Env, VestingContractClient<'static>, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(VestingContract, ());
        let client = VestingContractClient::new(&env, &id);
        let admin = Address::generate(&env);
        let user = Address::generate(&env);
        client.initialize(&admin);
        (env, client, admin, user)
    }

    fn advance_time(env: &Env, seconds: u64) {
        let mut li: LedgerInfo = env.ledger().get();
        li.timestamp = li.timestamp.saturating_add(seconds);
        li.sequence_number = li.sequence_number.saturating_add(seconds as u32);
        env.ledger().set(li);
    }

    // ------------------------------------------------------------------
    // Initialization
    // ------------------------------------------------------------------

    #[test]
    fn test_initialize_and_get_admin() {
        let (_env, client, admin, _user) = setup();
        assert_eq!(client.get_admin(), admin);
    }

    #[test]
    #[should_panic(expected = "AlreadyInitialized")]
    fn test_double_initialize_rejected() {
        let (env, client, admin, _user) = setup();
        client.initialize(&admin);
    }

    // ------------------------------------------------------------------
    // Linear vesting – basic computations
    // ------------------------------------------------------------------

    #[test]
    fn test_linear_vested_before_cliff_zero() {
        let (env, client, admin, user) = setup();
        let now = env.ledger().timestamp();
        let schedule = VestingSchedule::Linear(now, now + 100, now + 1000);
        client
            .add_grant(&admin, &user, &1000, &schedule)
            .unwrap();
        let v = client.vested_at(&user, &(now + 50)).unwrap();
        assert_eq!(v, 0);
    }

    #[test]
    fn test_linear_vested_at_cliff_boundary() {
        let (env, client, admin, user) = setup();
        let now = env.ledger().timestamp();
        let schedule = VestingSchedule::Linear(now, now + 100, now + 1000);
        client
            .add_grant(&admin, &user, &1000, &schedule)
            .unwrap();
        // At exactly cliff, vesting starts
        let v = client.vested_at(&user, &(now + 100)).unwrap();
        assert!(v > 0);
    }

    #[test]
    fn test_linear_vested_partial() {
        let (env, client, admin, user) = setup();
        let now = env.ledger().timestamp();
        let schedule = VestingSchedule::Linear(now, now + 100, now + 1100);
        client
            .add_grant(&admin, &user, &1000, &schedule)
            .unwrap();
        // Halfway through vesting window (from start)
        let v = client.vested_at(&user, &(now + 600)).unwrap();
        // 500 / 1000 * 1000 = 500
        assert_eq!(v, 500);
    }

    #[test]
    fn test_linear_vested_at_end_full() {
        let (env, client, admin, user) = setup();
        let now = env.ledger().timestamp();
        let schedule = VestingSchedule::Linear(now, now + 100, now + 1000);
        client
            .add_grant(&admin, &user, &1000, &schedule)
            .unwrap();
        let v = client.vested_at(&user, &(now + 1000)).unwrap();
        assert_eq!(v, 1000);
    }

    #[test]
    fn test_linear_vested_after_end_capped_at_principal() {
        let (env, client, admin, user) = setup();
        let now = env.ledger().timestamp();
        let schedule = VestingSchedule::Linear(now, now + 100, now + 1000);
        client
            .add_grant(&admin, &user, &1000, &schedule)
            .unwrap();
        let v = client.vested_at(&user, &(now + 2000)).unwrap();
        assert_eq!(v, 1000);
    }

    // ------------------------------------------------------------------
    // Linear vesting – validation
    // ------------------------------------------------------------------

    #[test]
    fn test_linear_cliff_before_start_rejected() {
        let (env, client, admin, user) = setup();
        let now = env.ledger().timestamp();
        let schedule = VestingSchedule::Linear(now + 100, now, now + 1000);
        let r = client.try_add_grant(&admin, &user, &1000, &schedule);
        assert!(r.is_err());
    }

    #[test]
    fn test_linear_zero_duration_rejected() {
        let (env, client, admin, user) = setup();
        let now = env.ledger().timestamp();
        let schedule = VestingSchedule::Linear(now, now, now);
        let r = client.try_add_grant(&admin, &user, &1000, &schedule);
        assert!(r.is_err());
    }

    // ------------------------------------------------------------------
    // Note: Comprehensive milestone-specific tests live in
    // milestone_schedule_test.rs (before, at, between, after milestones;
    // single-milestone; validation rejections; claiming; stress; cross-schedule).
    // ------------------------------------------------------------------

    #[test]
    fn test_claim_full_vested() {
        let (env, client, admin, user) = setup();
        let now = env.ledger().timestamp();
        let schedule = VestingSchedule::Linear(now, now, now + 1000);
        client
            .add_grant(&admin, &user, &1000, &schedule)
            .unwrap();
        advance_time(&env, 2000);
        let grant = client.claim(&user, &500).unwrap();
        assert_eq!(grant.claimed, 500);
        let claimable = client.claimable(&user).unwrap();
        assert_eq!(claimable, 500);
    }

    #[test]
    fn test_claim_more_than_vested_rejected() {
        let (env, client, admin, user) = setup();
        let now = env.ledger().timestamp();
        let schedule = VestingSchedule::Linear(now, now + 100, now + 1000);
        client
            .add_grant(&admin, &user, &1000, &schedule)
            .unwrap();
        // No time passed – nothing vested
        let r = client.try_claim(&user, &100);
        assert!(r.is_err());
    }

    #[test]
    fn test_claim_zero_amount_rejected() {
        let (env, client, admin, user) = setup();
        let now = env.ledger().timestamp();
        let schedule = VestingSchedule::Linear(now, now, now + 1000);
        client
            .add_grant(&admin, &user, &1000, &schedule)
            .unwrap();
        advance_time(&env, 2000);
        let r = client.try_claim(&user, &0);
        assert!(r.is_err());
    }

    #[test]
    fn test_claim_negative_amount_rejected() {
        let (env, client, admin, user) = setup();
        let now = env.ledger().timestamp();
        let schedule = VestingSchedule::Linear(now, now, now + 1000);
        client
            .add_grant(&admin, &user, &1000, &schedule)
            .unwrap();
        advance_time(&env, 2000);
        let r = client.try_claim(&user, &-1);
        assert!(r.is_err());
    }

    #[test]
    fn test_multiple_claims_accumulate() {
        let (env, client, admin, user) = setup();
        let now = env.ledger().timestamp();
        let schedule = VestingSchedule::Linear(now, now, now + 1000);
        client
            .add_grant(&admin, &user, &1000, &schedule)
            .unwrap();
        advance_time(&env, 2000);
        client.claim(&user, &200).unwrap();
        client.claim(&user, &300).unwrap();
        let claimable = client.claimable(&user).unwrap();
        assert_eq!(claimable, 500);
        let grant = client.get_grant(&user).unwrap();
        assert_eq!(grant.claimed, 500);
    }

    // ------------------------------------------------------------------
    // Sync
    // ------------------------------------------------------------------

    #[test]
    fn test_sync_returns_claimable() {
        let (env, client, admin, user) = setup();
        let now = env.ledger().timestamp();
        let schedule = VestingSchedule::Linear(now, now, now + 1000);
        client
            .add_grant(&admin, &user, &1000, &schedule)
            .unwrap();
        advance_time(&env, 2000);
        let s = client.sync(&user).unwrap();
        assert_eq!(s, 1000);
    }

    // ------------------------------------------------------------------
    // Unauthorized access
    // ------------------------------------------------------------------

    #[test]
    fn test_non_admin_cannot_add_grant() {
        let (env, client, admin, user) = setup();
        let now = env.ledger().timestamp();
        let schedule = VestingSchedule::Linear(now, now, now + 1000);
        // user (not admin) tries to add grant
        let r = client.try_add_grant(&user, &user, &1000, &schedule);
        assert!(r.is_err());
    }

    // ------------------------------------------------------------------
    // Linear preserves existing behaviour
    // ------------------------------------------------------------------

    #[test]
    fn test_linear_zero_cliff_full_vesting() {
        let (env, client, admin, user) = setup();
        let now = env.ledger().timestamp();
        let schedule = VestingSchedule::Linear(now, now, now + 1000);
        client
            .add_grant(&admin, &user, &1000, &schedule)
            .unwrap();
        advance_time(&env, 500);
        let v = client.vested_at(&user, &(now + 500)).unwrap();
        assert_eq!(v, 500);
    }

    #[test]
    fn test_linear_cliff_equals_start() {
        let (env, client, admin, user) = setup();
        let now = env.ledger().timestamp();
        let schedule = VestingSchedule::Linear(now, now, now + 1000);
        client
            .add_grant(&admin, &user, &1000, &schedule)
            .unwrap();
        // Before start+cliff (same time), nothing
        let v = client.vested_at(&user, &(now - 1)).unwrap();
        assert_eq!(v, 0);
    }

    #[test]
    fn test_principal_zero_rejected() {
        let (env, client, admin, user) = setup();
        let now = env.ledger().timestamp();
        let schedule = VestingSchedule::Linear(now, now, now + 1000);
        let r = client.try_add_grant(&admin, &user, &0, &schedule);
        assert!(r.is_err());
    }

    #[test]
    fn test_negative_principal_rejected() {
        let (env, client, admin, user) = setup();
        let now = env.ledger().timestamp();
        let schedule = VestingSchedule::Linear(now, now, now + 1000);
        let r = client.try_add_grant(&admin, &user, &-100, &schedule);
        assert!(r.is_err());
    }

    #[test]
    fn test_get_grant_not_found() {
        let (_env, client, _admin, user) = setup();
        let r = client.try_get_grant(&user);
        assert!(r.is_err());
    }

    #[test]
    fn test_grant_already_exists_rejected() {
        let (env, client, admin, user) = setup();
        let now = env.ledger().timestamp();
        let schedule = VestingSchedule::Linear(now, now, now + 1000);
        client
            .add_grant(&admin, &user, &1000, &schedule)
            .unwrap();
        let schedule2 = VestingSchedule::Linear(now, now, now + 2000);
        let r = client.try_add_grant(&admin, &user, &500, &schedule2);
        assert!(r.is_err());
    }
}
