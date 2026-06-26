#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, Env,
};

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub grantee: Address,
    pub total: u128,
    pub claimed: u128,
    pub start_seconds: u64,
    pub duration_seconds: u64,
    pub cliff_seconds: u64,
    pub revoked: bool,
}

impl Grant {
    pub fn vested_at(&self, now: u64) -> u128 {
        if now < self.start_seconds.saturating_add(self.cliff_seconds) {
            return 0;
        }
        if self.duration_seconds == 0 {
            return self.total;
        }
        let end = self.start_seconds.saturating_add(self.duration_seconds);
        let effective = if now >= end { end } else { now };
        if effective <= self.start_seconds {
            return 0;
        }
        let elapsed = effective - self.start_seconds;
        // linear vesting proportion: total * elapsed / duration
        (self.total * elapsed as u128) / self.duration_seconds as u128
    }

    pub fn claimable_at(&self, now: u64) -> u128 {
        if self.revoked {
            return 0;
        }
        let vested = self.vested_at(now);
        if vested <= self.claimed {
            0
        } else {
            vested - self.claimed
        }
    }
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum VestingError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    NoGrantFound = 4,
    AlreadyRevoked = 5,
    InvalidParameters = 6,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataKey {
    Admin,
    Treasury,
    Token,
    Grant(Address),
}

const PERSISTENT_TTL_LEDGERS: u32 = 1_000_000;

fn extend_grant_ttl(env: &Env, grantee: &Address) {
    let key = DataKey::Grant(grantee.clone());
    let extend_to = env.storage().max_ttl().min(PERSISTENT_TTL_LEDGERS);
    let threshold = extend_to / 2 + 1;
    if env.storage().persistent().has(&key) {
        env.storage()
            .persistent()
            .extend_ttl(&key, threshold, extend_to);
    }
}

#[contract]
pub struct VestingContract;

#[contractimpl]
impl VestingContract {
    /// Initialize the vesting contract with an admin, a treasury address, and the token to be vested.
    /// Can only be called once.
    pub fn initialize(
        env: Env,
        admin: Address,
        treasury: Address,
        token: Address,
    ) -> Result<(), VestingError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(VestingError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Treasury, &treasury);
        env.storage().instance().set(&DataKey::Token, &token);
        Ok(())
    }

    /// Retrieve the admin address of the contract.
    pub fn get_admin(env: Env) -> Result<Address, VestingError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(VestingError::NotInitialized)
    }

    /// Retrieve the treasury address of the contract.
    pub fn get_treasury(env: Env) -> Result<Address, VestingError> {
        env.storage()
            .instance()
            .get(&DataKey::Treasury)
            .ok_or(VestingError::NotInitialized)
    }

    /// Retrieve the token address used by the contract.
    pub fn get_token(env: Env) -> Result<Address, VestingError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(VestingError::NotInitialized)
    }

    /// Add a new vesting grant for a grantee.
    ///
    /// Gates on admin authorization. Escrows the tokens by transferring the
    /// `total` amount from the admin to the contract.
    pub fn add_grant(
        env: Env,
        grantee: Address,
        total: u128,
        start_seconds: u64,
        duration_seconds: u64,
        cliff_seconds: u64,
    ) -> Result<(), VestingError> {
        let admin = Self::get_admin(env.clone())?;
        admin.require_auth();

        if total == 0 {
            return Err(VestingError::InvalidParameters);
        }

        let token = Self::get_token(env.clone())?;
        let token_client = soroban_sdk::token::Client::new(&env, &token);
        
        // Transfer tokens from admin to the contract to escrow them.
        token_client.transfer(&admin, &env.current_contract_address(), &(total as i128));

        let grant = Grant {
            grantee: grantee.clone(),
            total,
            claimed: 0,
            start_seconds,
            duration_seconds,
            cliff_seconds,
            revoked: false,
        };

        let key = DataKey::Grant(grantee.clone());
        env.storage().persistent().set(&key, &grant);
        extend_grant_ttl(&env, &grantee);

        Ok(())
    }

    /// Claim the vested tokens for the caller (grantee).
    ///
    /// Gates on grantee authorization. Transfers claimable tokens from the
    /// contract to the grantee.
    pub fn claim(env: Env, grantee: Address) -> Result<u128, VestingError> {
        grantee.require_auth();

        let key = DataKey::Grant(grantee.clone());
        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(VestingError::NoGrantFound)?;

        let now = env.ledger().timestamp();
        let amount = grant.claimable_at(now);
        if amount == 0 {
            extend_grant_ttl(&env, &grantee);
            return Ok(0);
        }

        grant.claimed = grant.claimed.saturating_add(amount);
        env.storage().persistent().set(&key, &grant);
        extend_grant_ttl(&env, &grantee);

        let token = Self::get_token(env.clone())?;
        let token_client = soroban_sdk::token::Client::new(&env, &token);
        token_client.transfer(&env.current_contract_address(), &grantee, &(amount as i128));

        Ok(amount)
    }

    /// Revoke a vesting grant.
    ///
    /// Gates on admin authorization. Transferred the unvested tokens back
    /// to the treasury address. Marks the grant as revoked and reduces the
    /// total grant amount to the vested amount.
    pub fn revoke(env: Env, grantee: Address) -> Result<u128, VestingError> {
        let admin = Self::get_admin(env.clone())?;
        admin.require_auth();

        let key = DataKey::Grant(grantee.clone());
        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(VestingError::NoGrantFound)?;

        if grant.revoked {
            return Err(VestingError::AlreadyRevoked);
        }

        let now = env.ledger().timestamp();
        let vested = grant.vested_at(now);
        let unvested = if grant.total > vested {
            grant.total - vested
        } else {
            0
        };

        let treasury = Self::get_treasury(env.clone())?;
        if unvested > 0 {
            let token = Self::get_token(env.clone())?;
            let token_client = soroban_sdk::token::Client::new(&env, &token);
            token_client.transfer(&env.current_contract_address(), &treasury, &(unvested as i128));
        }

        grant.revoked = true;
        grant.total = vested;

        env.storage().persistent().set(&key, &grant);
        extend_grant_ttl(&env, &grantee);

        Ok(unvested)
    }

    /// Get a grant for a grantee.
    pub fn get_grant(env: Env, grantee: Address) -> Result<Grant, VestingError> {
        let key = DataKey::Grant(grantee.clone());
        let grant: Grant = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(VestingError::NoGrantFound)?;
        extend_grant_ttl(&env, &grantee);
        Ok(grant)
    }
}

#[cfg(test)]
mod vesting_contract_test;

