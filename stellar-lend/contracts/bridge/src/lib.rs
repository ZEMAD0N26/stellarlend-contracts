#![no_std]
use soroban_sdk::{contract, contractimpl, contracterror, contracttype, Env, Map};

/// Error codes for Bridge contract operations.
#[contracterror]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BridgeError {
    /// Nonce overflow: destination nonce has reached u64::MAX and cannot be incremented.
    NonceOverflow = 1,
}

/// Ledger storage key for the outbound nonce map.
#[contracttype]
pub enum BridgeDataKey {
    /// Maps destination network ID (u32) to its next outbound nonce (u64).
    OutboundNonces,
}

/// Emitted when an outbound bridge message is created.
/// Carries the destination network and the nonce assigned to this message,
/// giving relayers and the destination chain a unique, ordered identity.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutboundMessageEvent {
    /// Destination network identifier.
    pub dest: u32,
    /// Monotonically increasing nonce for this destination.
    pub nonce: u64,
}

/// Bridge contract with per-destination outbound nonce sequencing.
///
/// Each outbound transfer is assigned a strictly-increasing nonce keyed by
/// destination network. This gives relayers and downstream chains a
/// replay-resistant, deterministically ordered message identity.
#[contract]
pub struct Bridge;

#[contractimpl]
impl Bridge {
    /// Retrieve the current outbound nonce map from storage, or return an empty map.
    fn load_nonces(env: &Env) -> Map<u32, u64> {
        env.storage()
            .persistent()
            .get::<BridgeDataKey, Map<u32, u64>>(&BridgeDataKey::OutboundNonces)
            .unwrap_or_else(|| Map::new(env))
    }

    /// Persist the outbound nonce map to storage.
    fn save_nonces(env: &Env, nonces: &Map<u32, u64>) {
        env.storage()
            .persistent()
            .set(&BridgeDataKey::OutboundNonces, nonces);
    }

    /// Return the next outbound nonce for `dest`, then increment it.
    ///
    /// The first call for a fresh destination returns `0`.
    /// Subsequent calls return strictly increasing values.
    /// Panics with `BridgeError::NonceOverflow` if the nonce would exceed `u64::MAX`.
    ///
    /// # Arguments
    /// * `dest` - Destination network identifier (u32).
    ///
    /// # Returns
    /// The nonce assigned to this outbound message.
    pub fn next_outbound_nonce(env: Env, dest: u32) -> Result<u64, BridgeError> {
        let mut nonces = Self::load_nonces(&env);
        let current = nonces.get(dest).unwrap_or(0u64);
        let next = current.checked_add(1).ok_or(BridgeError::NonceOverflow)?;
        nonces.set(dest, next);
        Self::save_nonces(&env, &nonces);

        // Emit outbound event so relayers can track the message identity.
        env.events().publish(
            (soroban_sdk::symbol_short!("outbound"),),
            OutboundMessageEvent {
                dest,
                nonce: current,
            },
        );

        Ok(current)
    }

    /// Return the next nonce that will be assigned for `dest` without incrementing.
    ///
    /// Returns `0` if no messages have been sent to `dest` yet.
    ///
    /// # Arguments
    /// * `dest` - Destination network identifier (u32).
    ///
    /// # Returns
    /// The nonce that the next `next_outbound_nonce` call will return for this destination.
    pub fn peek_outbound_nonce(env: Env, dest: u32) -> u64 {
        let nonces = Self::load_nonces(&env);
        nonces.get(dest).unwrap_or(0u64)
    }
}

#[cfg(test)]
mod outbound_nonce_test;
