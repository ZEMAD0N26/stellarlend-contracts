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
mod rotation_test;

#[cfg(test)]
mod domain_separation_test;

#[cfg(test)]
mod inbound_cap_test;

#[cfg(test)]
mod window_rollover_test;

#[cfg(test)]
mod validator_bounds_test;

#[cfg(test)]
mod epoch_monotonicity_proptest;

#[cfg(test)]
mod window_guard_test;

#[cfg(test)]
mod window_tuning_doc_test;

#[cfg(test)]
mod outbound_cap_test;

#[cfg(test)]
mod validatorset_proptest;

#[cfg(test)]
mod validator_pause_test;

#[cfg(test)]
mod rotation_churn_test;

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Keypair, Signer};
    use rand::rngs::OsRng;

    fn make_keypairs(n: usize) -> Vec<Keypair> {
        let mut rng = OsRng;
        (0..n).map(|_| Keypair::generate(&mut rng)).collect()
    }

    #[test]
    fn test_rotate_success_and_epoch_boundary() {
        // initial set A: 4 validators
        let kp_a = make_keypairs(4);
        let a_pks: Vec<PublicKey> = kp_a.iter().map(|k| k.public).collect();
        let initial = ValidatorSet { validators: a_pks.iter().map(|p| p.to_bytes().to_vec()).collect() };
        let mut bridge = Bridge::new(initial);

        // new set B: 3 validators
        let kp_b = make_keypairs(3);
        let b_pks: Vec<PublicKey> = kp_b.iter().map(|k| k.public).collect();
        let new_set = ValidatorSet { validators: b_pks.iter().map(|p| p.to_bytes().to_vec()).collect() };

        // proofs: have >2/3 of A sign the (new_set, epoch=1) payload
        let epoch = 1u64;
        let payload = Bridge::quorum_proof_payload(&bridge.bridge_id, &new_set, epoch).unwrap();

        // need threshold of A: (4*2)/3+1 = 3
        let mut proofs = vec![];
        for i in 0..3 {
            let sig = kp_a[i].sign(&payload);
            proofs.push((kp_a[i].public, sig));
        }

        // rotate should succeed
        bridge.rotate_validators(new_set.clone(), epoch, proofs).expect("rotation failed");
        assert_eq!(bridge.epoch, 1);

        // messages signed with epoch 0 should be rejected
        assert!(bridge.validate_inbound_epoch(0).is_err());
        // messages signed with epoch 1 are accepted
        assert!(bridge.validate_inbound_epoch(1).is_ok());
        assert!(bridge.validate_inbound_epoch(2).is_ok(), "future epochs allowed by this check (policy dependent)");
    }

    #[test]
    fn test_rotate_reject_insufficient_quorum() {
        let kp_a = make_keypairs(5);
        let a_pks: Vec<PublicKey> = kp_a.iter().map(|k| k.public).collect();
        let initial = ValidatorSet { validators: a_pks.iter().map(|p| p.to_bytes().to_vec()).collect() };
        let mut bridge = Bridge::new(initial);

        let kp_b = make_keypairs(3);
        let b_pks: Vec<PublicKey> = kp_b.iter().map(|k| k.public).collect();
        let new_set = ValidatorSet { validators: b_pks.iter().map(|p| p.to_bytes().to_vec()).collect() };

        let epoch = 1u64;
        let payload = Bridge::quorum_proof_payload(&bridge.bridge_id, &new_set, epoch).unwrap();

        // need threshold of A: (5*2)/3+1 = 4. Provide only 3 signatures => fail
        let mut proofs = vec![];
        for i in 0..3 {
            let sig = kp_a[i].sign(&payload);
            proofs.push((kp_a[i].public, sig));
        }

        assert!(bridge.rotate_validators(new_set, epoch, proofs).is_err());
    }

    #[test]
    fn test_rotate_reject_wrong_epoch() {
        let kp_a = make_keypairs(3);
        let a_pks: Vec<PublicKey> = kp_a.iter().map(|k| k.public).collect();
        let initial = ValidatorSet { validators: a_pks.iter().map(|p| p.to_bytes().to_vec()).collect() };
        let mut bridge = Bridge::new(initial);

        let kp_b = make_keypairs(2);
        let b_pks: Vec<PublicKey> = kp_b.iter().map(|k| k.public).collect();
        let new_set = ValidatorSet { validators: b_pks.iter().map(|p| p.to_bytes().to_vec()).collect() };

        // wrong epoch (must be 1)
        let epoch = 2u64;
        let payload = Bridge::quorum_proof_payload(&bridge.bridge_id, &new_set, epoch).unwrap();

        let mut proofs = vec![];
        for i in 0..2 {
            let sig = kp_a[i].sign(&payload);
            proofs.push((kp_a[i].public, sig));
        }

        assert!(bridge.rotate_validators(new_set, epoch, proofs).is_err());
    }
}
