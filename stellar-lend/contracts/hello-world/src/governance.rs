//! # Governance proposal payload binding (issue #1120)
//!
//! Binds the exact action a proposal authorizes to a cryptographic hash at
//! **creation** time, and verifies it at **execution** time. This closes the
//! "execute-time substitution" gap: a privileged caller cannot queue one action
//! and then execute a different one, because execution recomputes the hash of
//! the action it is about to run and rejects anything that does not match what
//! voters approved ("what you voted for is what runs").
//!
//! ## Canonical encoding
//!
//! The hash is taken over the canonical encoding of the action:
//!
//! ```text
//!   preimage = be32(len(target)) ‖ target
//!            ‖ be32(len(params)) ‖ params
//!            ‖ be64(proposal_id)
//!   payload_hash = keccak256(preimage)
//! ```
//!
//! * Each variable-length field (`target`, `params`) is **length-prefixed** with
//!   a 4-byte big-endian length. Without this, distinct actions could share a
//!   preimage by shifting bytes across the field boundary
//!   (e.g. `target="ab",params=""` vs `target="a",params="b"`) — a classic
//!   concatenation/aliasing attack. Length-prefixing makes the encoding
//!   injective.
//! * `proposal_id` is folded into the preimage as an 8-byte big-endian integer,
//!   so an identical action bound to a different proposal hashes differently.
//!   This resists cross-proposal **replay** (re-using an approved action's hash
//!   under a new id) and aliasing.
//!
//! ## Wiring into governance
//!
//! `gov_create_proposal` should call [`bind_payload`] and persist the returned
//! hash alongside the proposal. `gov_execute_proposal` should reconstruct the
//! [`ProposalPayload`] for the action it is about to perform and call
//! [`verify_payload`] with the stored hash before doing anything else; on
//! [`PayloadBindingError::PayloadMismatch`] it must abort.
//!
//! The full proposal/vote/queue lifecycle currently lives behind stubbed
//! modules in this crate; this module provides the cryptographic binding
//! primitive so it can be dropped into the lifecycle once restored.

use soroban_sdk::{contracterror, contracttype, Bytes, BytesN, Env};

/// The canonical action a proposal authorizes.
///
/// The pair (`target`, `params`) fully describes *what runs*; `proposal_id`
/// scopes the binding to a single proposal so an approved action cannot be
/// replayed under a different id.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProposalPayload {
    /// Opaque action target — e.g. the encoded contract address and/or function
    /// selector the proposal will invoke.
    pub target: Bytes,
    /// Canonical-encoded action parameters.
    pub params: Bytes,
    /// Id of the proposal this payload is bound to (anti-replay / anti-alias).
    pub proposal_id: u64,
}

/// Errors returned when verifying a proposal payload at execution time.
#[contracterror]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum PayloadBindingError {
    /// The execution-time payload does not match the hash bound at creation.
    PayloadMismatch = 1,
}

/// Appends a 4-byte big-endian length prefix to `buf`.
fn append_be32(buf: &mut Bytes, value: u32) {
    for byte in value.to_be_bytes() {
        buf.push_back(byte);
    }
}

/// Builds the canonical, injective preimage for a payload (see module docs).
fn encode_payload(env: &Env, payload: &ProposalPayload) -> Bytes {
    let mut buf = Bytes::new(env);

    append_be32(&mut buf, payload.target.len());
    buf.append(&payload.target);

    append_be32(&mut buf, payload.params.len());
    buf.append(&payload.params);

    for byte in payload.proposal_id.to_be_bytes() {
        buf.push_back(byte);
    }

    buf
}

/// Computes the `keccak256` payload hash binding `target`, `params`, and
/// `proposal_id` (see module docs for the exact encoding).
///
/// The same inputs always produce the same hash; any change to the target, the
/// params, or the proposal id produces a different hash.
pub fn compute_payload_hash(env: &Env, payload: &ProposalPayload) -> BytesN<32> {
    let preimage = encode_payload(env, payload);
    env.crypto().keccak256(&preimage).to_bytes()
}

/// Hash to record at proposal **creation** time.
///
/// Call from `gov_create_proposal` and persist the result with the proposal.
/// Alias of [`compute_payload_hash`] kept for call-site clarity.
pub fn bind_payload(env: &Env, payload: &ProposalPayload) -> BytesN<32> {
    compute_payload_hash(env, payload)
}

/// Verifies an execution-time payload against the hash bound at creation.
///
/// Call from `gov_execute_proposal` before performing any action. Returns
/// [`PayloadBindingError::PayloadMismatch`] if the recomputed hash differs from
/// `bound_hash`, which the caller must treat as a hard failure (abort
/// execution).
pub fn verify_payload(
    env: &Env,
    bound_hash: &BytesN<32>,
    payload: &ProposalPayload,
) -> Result<(), PayloadBindingError> {
    let recomputed = compute_payload_hash(env, payload);
    if recomputed == *bound_hash {
        Ok(())
    } else {
        Err(PayloadBindingError::PayloadMismatch)
    }
}

#[cfg(test)]
#[path = "gov_payload_hash_test.rs"]
mod gov_payload_hash_test;
