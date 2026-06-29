use anyhow::{anyhow, Result};
use bincode;
use ed25519_dalek::{PublicKey, Signature, Verifier};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Minimum number of validators required for a secure validator set.
///
/// A set with fewer than this many validators has an unacceptably low
/// supermajority threshold — a single compromised key or node outage can
/// halt or subvert the bridge.  The value `3` ensures the supermajority
/// threshold is at least 3, matching the BFT assumption that fewer than
/// 1/3 of validators may be Byzantine.
pub const MIN_VALIDATORS: usize = 3;

/// Maximum number of validators permitted in a single set.
///
/// This limit bounds the proof‑verification cost of a quorum check and
/// prevents unbounded storage growth.  The value `32` is a generous
/// upper bound that accommodates most real‑world bridge deployments
/// while keeping per‑rotation verification within reasonable limits.
pub const MAX_VALIDATORS: usize = 32;

/// Typed contract errors to represent specific domain violations.
#[derive(Debug, PartialEq, Eq)]
pub enum BridgeError {
    /// Emitted when attempting to configure a rolling window of length 0.
    InvalidWindowSize,
    /// Emitted when `rotate_validators` receives a `new_set` whose effective
    /// (deduplicated) validator count is below [`MIN_VALIDATORS`].
    ValidatorSetTooSmall,
    /// Emitted when `rotate_validators` receives a `new_set` whose effective
    /// (deduplicated) validator count exceeds [`MAX_VALIDATORS`].
    ValidatorSetTooLarge,
    /// Emitted when `rotate_validators` receives a `new_set` containing
    /// duplicate public keys.
    DuplicateValidatorKey,
    /// No guardian key has been configured for this bridge. Pause / unpause
    /// requires a guardian to be set via [`Bridge::set_guardian`].
    NoGuardianConfigured,
    /// The signature supplied to authorise a guardian action (pause, unpause,
    /// or future guardian-protected operations) did not verify against the
    /// configured guardian key over the expected action-bound payload.
    InvalidGuardianSignature,
    /// The caller asked us to pause / unpause a validator whose public key is
    /// not in the current validator set.
    UnknownValidator,
    /// Pausing this validator would drop the active validator count below the
    /// effective supermajority quorum threshold, so quorum would become
    /// unreachable. Rejected (fail-closed) — the bridge prefers to remain
    /// live with a known-compromised key over freezing itself.
    PauseWouldBreakQuorum,
    /// The validator requested for pause was already in the paused set.
    AlreadyPaused,
    /// The validator requested for unpause was not in the paused set.
    NotPaused,
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BridgeError::InvalidWindowSize => {
                write!(f, "InvalidWindowSize: window_size must be > 0")
            }
            BridgeError::ValidatorSetTooSmall => {
                write!(
                    f,
                    "ValidatorSetTooSmall: validator set must have at least {MIN_VALIDATORS} unique validators"
                )
            }
            BridgeError::ValidatorSetTooLarge => {
                write!(
                    f,
                    "ValidatorSetTooLarge: validator set must have at most {MAX_VALIDATORS} unique validators"
                )
            }
            BridgeError::DuplicateValidatorKey => {
                write!(
                    f,
                    "DuplicateValidatorKey: new_set contains duplicate public keys"
                )
            }
            BridgeError::NoGuardianConfigured => {
                write!(f, "NoGuardianConfigured: bridge has no guardian key set")
            }
            BridgeError::InvalidGuardianSignature => {
                write!(f, "InvalidGuardianSignature: guardian signature did not verify")
            }
            BridgeError::UnknownValidator => {
                write!(f, "UnknownValidator: target validator not in current validator set")
            }
            BridgeError::PauseWouldBreakQuorum => write!(
                f,
                "PauseWouldBreakQuorum: pausing this validator would leave active count below the effective quorum threshold"
            ),
            BridgeError::AlreadyPaused => write!(f, "AlreadyPaused: validator is already paused"),
            BridgeError::NotPaused => write!(f, "NotPaused: validator is not currently paused"),
        }
    }
}

impl std::error::Error for BridgeError {}
/// Events emitted by guardian-gated validator-pause operations. Callers (e.g.
/// off-chain tooling, audit pipelines, or a Soroban host adapter) are expected
/// to serialize or log these events so downstream consumers can react (alert,
/// rotate keys, fan out to other nodes, etc.).
///
/// In this off-chain Rust crate we do not have a host-managed event log; we
/// return the typed event from the operation so callers can persist it
/// however they store the bridge. The on-the-wire shape is determined by the
/// caller's chosen encoder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ValidatorEvent {
    /// A validator has been paused by the guardian. Signatures from this key
    /// are now ignored in `verify_quorum_proof` and the effective quorum
    /// threshold is recomputed against the remaining active validators.
    Paused {
        /// Raw byte encoding of the paused validator's public key, so the
        /// event is self-describing for layers that don't have direct access
        /// to the `ValidatorSet`.
        validator: Vec<u8>,
        /// Bridge epoch at the time the pause became effective.
        epoch: u64,
    },
    /// A previously paused validator has been resumed. The key is counted
    /// toward quorum again from this point forward.
    Unpaused {
        validator: Vec<u8>,
        epoch: u64,
    },
}

impl std::fmt::Display for ValidatorEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidatorEvent::Paused { validator, epoch } => write!(
                f,
                "ValidatorPaused(epoch={epoch}, pk=0x{})",
                lowercase_hex(validator)
            ),
            ValidatorEvent::Unpaused { validator, epoch } => write!(
                f,
                "ValidatorUnpaused(epoch={epoch}, pk=0x{})",
                lowercase_hex(validator)
            ),
        }
    }
}

/// Store validator public keys as raw bytes so the struct remains serde-friendly
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValidatorSet {
    pub validators: Vec<Vec<u8>>, // each is PublicKey::to_bytes()
}

impl ValidatorSet {
    /// Returns the effective validator count used for quorum decisions.
    ///
    /// Duplicate byte-encoded keys collapse to a single logical validator so a
    /// malformed set cannot silently raise the quorum threshold by repeating the
    /// same public key multiple times.
    pub fn len(&self) -> usize {
        self.validators
            .iter()
            .map(|validator| validator.as_slice())
            .collect::<HashSet<_>>()
            .len()
    }

    /// Returns the strict supermajority quorum threshold for this set.
    ///
    /// The threshold is computed from the deduplicated validator count exposed
    /// by [`ValidatorSet::len`], so repeated keys never inflate the required
    /// number of unique signatures.
    pub fn threshold(&self) -> usize {
        // Supermajority: > 2/3 of validators
        let n = self.len();
        (n * 2) / 3 + 1
    }

    /// Returns `true` when `pk` is present anywhere in the raw validator list.
    pub fn contains_pk(&self, pk: &PublicKey) -> bool {
        let b = pk.to_bytes();
        self.validators.iter().any(|v| v.as_slice() == b.as_ref())
    }

    /// Returns the raw byte-encoded validator list in storage order.
    pub fn to_bytes_vec(&self) -> Vec<Vec<u8>> {
        self.validators.clone()
    }
}

#[derive(Clone, Debug)]
pub struct Bridge {
    pub epoch: u64,
    pub validators: ValidatorSet,
    /// Maximum cumulative inbound value (in the bridge's native token unit,
    /// matching the `i128` amount convention used elsewhere in this workspace)
    /// that may be admitted within a single rolling window.
    ///
    /// A value of `0` means "no inbound" (fail-closed) — not "unlimited".
    /// Defaults to `0` so a freshly constructed `Bridge` admits no inbound
    /// value until an operator explicitly opts in via [`Bridge::set_inbound_cap`].
    pub max_per_window: i128,
    /// Length of the rolling inbound-value window, in ledger-time seconds
    /// (e.g. `86_400` for a calendar-day window). Must be > 0 once configured.
    pub window_size: u64,
    /// Ledger time at which the current window began.
    pub window_start: u64,
    /// Cumulative inbound value admitted so far within `[window_start, window_start + window_size)`.
    pub window_inbound_total: i128,
    /// Set of byte-encoded public keys of validators that the guardian has
    /// paused. Signatures from paused validators are silently skipped
    /// (not counted toward quorum, not verified) in
    /// [`Bridge::verify_quorum_proof`], and the effective quorum threshold is
    /// recomputed against the active (non-paused) subset.
    ///
    /// Pauses are scoped to the current validator set: a successful call to
    /// [`Bridge::rotate_validators`] clears this set, since the new validator
    /// set implies fresh key material and stale pause flags are meaningless.
    /// See [`VALIDATOR_PAUSE.md`](https://example.invalid/VALIDATOR_PAUSE.md)
    /// for the full rationale.
    pub paused_validators: HashSet<Vec<u8>>,
    /// Guardian public key authorised to pause / unpause individual
    /// validators. `None` means the bridge has no guardian configured;
    /// [`Bridge::pause_validator`] and [`Bridge::unpause_validator`] are both
    /// rejected with [`BridgeError::NoGuardianConfigured`] until a guardian is
    /// configured via [`Bridge::set_guardian`].
    pub guardian: Option<PublicKey>,
}

/// Default rolling window length: one day, in seconds.
pub const DEFAULT_INBOUND_WINDOW_SECS: u64 = 86_400;

/// Domain separator tags prepended to guardian-signed payloads for
/// pause / unpause authorisations. Binding the tag into the signed payload
/// prevents replay of a `pause_validator` signature against
/// `unpause_validator` (or vice versa) and prevents cross-action confusion.
const PAUSE_PAYLOAD_TAG: &[u8] = b"BRIDGE_PAUSE:";
const UNPAUSE_PAYLOAD_TAG: &[u8] = b"BRIDGE_UNPAUSE:";

impl Bridge {
    /// Construct a new bridge. Inbound value transfer is **fail-closed by
    /// default**: `max_per_window` starts at `0`, so [`Bridge::admit_inbound`]
    /// rejects everything until [`Bridge::set_inbound_cap`] is called with a
    /// non-zero cap.
    ///
    /// A freshly constructed `Bridge` has no guardian (so pause / unpause
    /// calls are rejected) and an empty paused set. Operators must opt in
    /// to guardian-gated operations via [`Bridge::set_guardian`].
    pub fn new(initial: ValidatorSet) -> Self {
        Bridge {
            epoch: 0,
            validators: initial,
            max_per_window: 0,
            window_size: DEFAULT_INBOUND_WINDOW_SECS,
            window_start: 0,
            window_inbound_total: 0,
            paused_validators: HashSet::new(),
            guardian: None,
        }
    }

    /// Configure the guardian public key authorised to pause / unpause
    /// individual validators.
    ///
    /// Although this method has no signature check (the bridge is a
    /// pure-Rust data structure with no built-in notion of a privileged
    /// host), operational guidance is to call it exactly once, on a trusted
    /// host, immediately after [`Bridge::new`]. Replacing the guardian
    /// later must only be done on the same trusted path; there is no
    /// built-in two-step handover — if you need one, build it on top of
    /// `set_guardian`.
    pub fn set_guardian(&mut self, guardian: PublicKey) {
        self.guardian = Some(guardian);
    }

    /// Returns `Some(&guardian_public_key)` if a guardian has been
    /// configured, otherwise `None`.
    pub fn guardian(&self) -> Option<&PublicKey> {
        self.guardian.as_ref()
    }

    /// Returns the number of active (non-paused) validators.
    ///
    /// "Active" excludes any validator whose byte-encoded public key is in
    /// [`Bridge::paused_validators`]. Duplicate keys in the raw validator
    /// list still collapse to one logical validator, matching
    /// [`ValidatorSet::len`] semantics.
    pub fn active_validator_count(&self) -> usize {
        self.validators
            .validators
            .iter()
            .filter(|v| !self.paused_validators.contains(*v))
            .map(|v| v.as_slice())
            .collect::<HashSet<_>>()
            .len()
    }

    /// Effective supermajority quorum threshold computed from the active
    /// (non-paused) validator count.
    ///
    /// If every validator is paused, this returns `1` — the same value
    /// [`ValidatorSet::threshold`] returns for an empty set. This is a
    /// documented edge case: a fully-paused bridge is mathematically
    /// unreachable (no active signer can ever meet any threshold > 0) and
    /// pause / unpause calls will reject based on the fail-closed
    /// arithmetic before this returns in any realistic configuration. See
    /// [`BridgeError::PauseWouldBreakQuorum`] for the guard that prevents
    /// the bridge from getting into this state in the first place.
    pub fn effective_threshold(&self) -> usize {
        let n = self.active_validator_count();
        (n * 2) / 3 + 1
    }

    /// Returns `true` iff `pk`'s byte encoding is in the paused set.
    ///
    /// This is a pure membership check; it does **not** validate that the
    /// validator is also part of the current validator set. To check both
    /// conditions, see [`Bridge::is_active_validator`].
    pub fn is_paused(&self, pk: &PublicKey) -> bool {
        self.paused_validators.contains(&pk.to_bytes().to_vec())
    }

    /// Returns `true` iff `pk` is currently part of the validator set
    /// **and** is not paused — i.e. its signature counts toward quorum.
    pub fn is_active_validator(&self, pk: &PublicKey) -> bool {
        self.validators.contains_pk(pk) && !self.is_paused(pk)
    }

    /// Returns the raw byte-encoding of every currently-paused validator,
    /// in arbitrary set-iteration order. Useful for audit / introspection
    /// tooling.
    pub fn paused_list(&self) -> Vec<Vec<u8>> {
        self.paused_validators.iter().cloned().collect()
    }

    /// Verify a quorum proof from the current validator set over the (new_set, epoch) payload.
    ///
    /// Paused validator signatures are *silently skipped* — they are neither
    /// verified nor counted toward the quorum, and they do not cause the
    /// overall proof to fail. Skipping (rather than rejecting on sight) is a
    /// deliberate choice: a compromised key may still be present in the
    /// relay-network gossip, so silently ignoring it lets a bridge keep
    /// operating under quorum with a known-compromised signer excluded. The
    /// effective quorum threshold is recomputed from the active (non-paused)
    /// validator subset.
    fn verify_quorum_proof(
        &self,
        new_set: &ValidatorSet,
        epoch: u64,
        proofs: &[(PublicKey, Signature)],
    ) -> Result<()> {
        if proofs.is_empty() {
            return Err(anyhow!("empty proofs"));
        }

        // payload to be signed: bincode(new_set_bytes_vec, epoch)
        let payload = bincode::serialize(&(new_set.to_bytes_vec(), epoch))?;

        let mut unique_active_signers: HashSet<Vec<u8>> = HashSet::new();
        for (pk, sig) in proofs.iter() {
            // Signer must be part of the current validator set. This applies
            // to paused validators, too — paused keys must still be in the
            // current set; otherwise they should have been rotated out.
            if !self.validators.contains_pk(pk) {
                return Err(anyhow!("proof contains signer not in current validator set"));
            }

            // Paused validators are silently skipped. They do not count
            // toward the quorum, and we do not verify their signature
            // (the key is presumed compromised, so its signature carries no
            // trust weight; verifying it is wasted work, and a malformed
            // signature from a compromised-but-paused key should not bring
            // down the rest of the proof).
            let key_bytes = pk.to_bytes().to_vec();
            if self.paused_validators.contains(&key_bytes) {
                continue;
            }

            // Deduplicate within the active subset.
            if unique_active_signers.contains(&key_bytes) {
                continue;
            }

            pk.verify(&payload, sig).map_err(|e| anyhow!(e.to_string()))?;
            unique_active_signers.insert(key_bytes);
        }

        if unique_active_signers.len() < self.effective_threshold() {
            return Err(anyhow!("insufficient quorum in proofs"));
        }

        Ok(())
    }

    /// Rotate validators to `new_set` at `next_epoch` if `proofs` from current set form a quorum.
    /// The `epoch` must be exactly current_epoch + 1.
    ///
    /// # Security validation
    ///
    /// Before verifying the quorum proof, this function validates the incoming
    /// `new_set`:
    ///
    /// 1. **Size bounds** — the deduplicated validator count must lie within
    ///    [`MIN_VALIDATORS`, `MAX_VALIDATORS`].  Rejects empty or single-validator
    ///    sets that would collapse the supermajority into a single point of
    ///    failure, and oversized sets that would make quorum verification
    ///    prohibitively expensive.
    /// 2. **Duplicate keys** — the raw `new_set` must not contain duplicate
    ///    public-key byte representations.  While the [`ValidatorSet::len`] and
    ///    [`ValidatorSet::threshold`] methods themselves deduplicate for quorum
    ///    counting, a set that *relies* on dedup to meet its size bound is a
    ///    bug waiting to happen — the extra duplicate entries serve no purpose
    ///    and may mask an operator error during key collection.
    ///
    /// The paused-validator set is cleared on rotation: pauses are scoped to
    /// the compromised key material in the *current* set, and the *new* set
    /// implies fresh, unpaused keys by default. If a key from the old set
    /// happens to also be present in the new set, that's a configuration
    /// choice the operator must make explicitly via a subsequent
    /// [`Bridge::pause_validator`] call.
    pub fn rotate_validators(
        &mut self,
        new_set: ValidatorSet,
        epoch: u64,
        proofs: Vec<(PublicKey, Signature)>,
    ) -> Result<()> {
        if epoch != self.epoch + 1 {
            return Err(anyhow!("invalid epoch: must be current_epoch + 1"));
        }

        // ── Validate new_set size bounds ──────────────────────────────────
        let unique_count = new_set.len();
        if unique_count < MIN_VALIDATORS {
            return Err(anyhow!("{}", BridgeError::ValidatorSetTooSmall));
        }
        if unique_count > MAX_VALIDATORS {
            return Err(anyhow!("{}", BridgeError::ValidatorSetTooLarge));
        }

        // ── Validate no duplicate keys ────────────────────────────────────
        // We check the *raw* (pre-dedup) list.  The `len()` method deduplicates
        // internally, but we also want to reject sets that contain any duplicate
        // entries at all — they are never legitimate and always indicate an
        // operator error.
        {
            let mut seen = std::collections::HashSet::new();
            for key_bytes in &new_set.validators {
                if !seen.insert(key_bytes.as_slice()) {
                    return Err(anyhow!("{}", BridgeError::DuplicateValidatorKey));
                }
            }
        }

        self.verify_quorum_proof(&new_set, epoch, &proofs)?;

        // swap atomically
        self.validators = new_set;
        self.epoch = epoch;
        // stale pause flags belong to the old (rotated-out) key material; clear.
        self.paused_validators.clear();
        Ok(())
    }

    /// Guardian-gated pause of a single validator.
    ///
    /// On success the validator is added to [`Bridge::paused_validators`] and
    /// a [`ValidatorEvent::Paused`] event is returned for the caller to log
    /// or persist. The validator's signature is ignored in subsequent
    /// `verify_quorum_proof` calls, and the effective quorum threshold is
    /// recomputed against the remaining active validators.
    ///
    /// ### Fail-closed guard
    ///
    /// The *supplied* signature is verified against the configured
    /// [`Bridge::guardian`] (not against `validator`) over the action-bound
    /// payload `"BRIDGE_PAUSE:" || validator.to_bytes()`. This binds the
    /// authorisation to a specific (action, target_validator) pair so a
    /// pause signature cannot be replayed as an unpause signature, and vice
    /// versa (the inverse tag `"BRIDGE_UNPAUSE:"` is used for unpauses).
    ///
    /// Pausing is rejected with [`BridgeError::PauseWouldBreakQuorum`] if it
    /// would leave the active validator count below the new effective
    /// supermajority threshold (so a quorum-proof could never reach the new
    /// threshold). This protects the bridge from being frozen by an overly
    /// aggressive guardian and is enforced upstream of the signature check
    /// so a malicious caller cannot burn the guardian's signature on a
    /// request that would have been rejected anyway.
    pub fn pause_validator(
        &mut self,
        validator: &PublicKey,
        signature: &Signature,
    ) -> Result<ValidatorEvent> {
        // 1. Guardian must be configured.
        let guardian = self.guardian.ok_or(BridgeError::NoGuardianConfigured)?;

        let v_bytes = validator.to_bytes().to_vec();

        // 2. Target must be part of the current validator set.
        if !self.validators.contains_pk(validator) {
            return Err(BridgeError::UnknownValidator.into());
        }

        // 3. Reject double-pause explicitly *before* the fail-closed math,
        //    so a re-pause attempt returns the precise `AlreadyPaused`
        //    diagnostic instead of `PauseWouldBreakQuorum` (whose math is
        //    only meaningful when the target is currently active).
        if self.paused_validators.contains(&v_bytes) {
            return Err(BridgeError::AlreadyPaused.into());
        }

        // 4. Fail-closed: refuse to pause if it would make the active count
        //    drop below the new effective quorum threshold. We have just
        //    confirmed `validator` is in the current validator set *and* is
        //    not yet paused, so subtracting 1 from the active count is
        //    exact.
        let current_active = self.active_validator_count();
        let new_active = current_active.checked_sub(1).unwrap_or(0);
        let new_threshold = (new_active * 2) / 3 + 1;
        if new_active < new_threshold {
            return Err(BridgeError::PauseWouldBreakQuorum.into());
        }

        // 5. Verify guardian signature over the action-bound payload.
        let payload = concat_prefixed(PAUSE_PAYLOAD_TAG, &v_bytes);
        guardian
            .verify(&payload, signature)
            .map_err(|_| BridgeError::InvalidGuardianSignature)?;

        // 6. Commit: mark the validator paused and return the event.
        self.paused_validators.insert(v_bytes.clone());
        Ok(ValidatorEvent::Paused {
            validator: v_bytes,
            epoch: self.epoch,
        })
    }

    /// Guardian-gated unpause of a single validator.
    ///
    /// The signature is verified against the configured [`Bridge::guardian`]
    /// over the action-bound payload `"BRIDGE_UNPAUSE:" || pk_bytes`, which
    /// is the dual of the pause payload so signatures cannot be replayed
    /// across actions.
    pub fn unpause_validator(
        &mut self,
        validator: &PublicKey,
        signature: &Signature,
    ) -> Result<ValidatorEvent> {
        let guardian = self.guardian.ok_or(BridgeError::NoGuardianConfigured)?;

        let v_bytes = validator.to_bytes().to_vec();

        if !self.validators.contains_pk(validator) {
            return Err(BridgeError::UnknownValidator.into());
        }
        if !self.paused_validators.contains(&v_bytes) {
            return Err(BridgeError::NotPaused.into());
        }

        let payload = concat_prefixed(UNPAUSE_PAYLOAD_TAG, &v_bytes);
        guardian
            .verify(&payload, signature)
            .map_err(|_| BridgeError::InvalidGuardianSignature)?;

        self.paused_validators.remove(&v_bytes);
        Ok(ValidatorEvent::Unpaused {
            validator: v_bytes,
            epoch: self.epoch,
        })
    }

    /// Verify inbound message signature epoch. Messages signed with an epoch < current epoch are rejected.
    pub fn validate_inbound_epoch(&self, signed_epoch: u64) -> Result<()> {
        if signed_epoch < self.epoch {
            return Err(anyhow!("message signed by retired validator set (epoch too old)"));
        }
        Ok(())
    }

    /// Reconfigure the per-window inbound value cap and (re)start the
    /// window fresh at `current_time`.
    ///
    /// `max_per_window == 0` is a valid, intentional configuration meaning
    /// "no inbound" (fail-closed) — use a positive value to actually permit
    /// inbound transfers. `window_size` must be greater than zero.
    pub fn set_inbound_cap(&mut self, max_per_window: i128, window_size: u64, current_time: u64) -> Result<()> {
        if max_per_window < 0 {
            return Err(anyhow!("max_per_window must be >= 0"));
        }
        if window_size == 0 {
            return Err(BridgeError::InvalidWindowSize.into());
        }

        self.max_per_window = max_per_window;
        self.window_size = window_size;
        self.window_start = current_time;
        self.window_inbound_total = 0;
        Ok(())
    }

    /// Roll the inbound-value window forward if `current_time` has moved
    /// past the end of the current window. Resetting realigns the window to
    /// start at `current_time` rather than stepping forward in fixed
    /// `window_size` increments, so a bridge that sat idle for a long time
    /// doesn't pay for that idle period with a stale, partially-consumed
    /// window (see SECURITY_NOTES.md for the rationale).
    fn roll_window_if_expired(&mut self, current_time: u64) {
        if current_time < self.window_start {
            // Guard against non-monotonic clock adjustments (time moving backwards).
            return;
        }

        if let Some(window_end) = self.window_start.checked_add(self.window_size) {
            if current_time >= window_end {
                self.window_start = current_time;
                self.window_inbound_total = 0;
            }
        }
    }

    /// Admit an inbound transfer of `amount` against the per-window inbound
    /// value cap, tracked on rolling ledger time (not block count).
    ///
    /// Rejects (without mutating any state) if:
    /// - `amount` is negative,
    /// - the cap is configured as `0` (fail-closed — no inbound permitted
    ///   regardless of amount), or
    /// - admitting `amount` would push the window's cumulative inbound value
    ///   above `max_per_window`.
    ///
    /// On success, `amount` is added to the current window's running total
    /// and `Ok(())` is returned. Callers are expected to have already
    /// validated validator quorum and inbound epoch separately — this check
    /// is purely the value-cap defense-in-depth layer.
    pub fn admit_inbound(&mut self, amount: i128, current_time: u64) -> Result<()> {
        if amount < 0 {
            return Err(anyhow!("inbound amount must be >= 0"));
        }

        if self.max_per_window == 0 {
            return Err(anyhow!("inbound cap is zero (fail-closed): no inbound transfers permitted"));
        }

        self.roll_window_if_expired(current_time);

        let new_total = self
            .window_inbound_total
            .checked_add(amount)
            .ok_or_else(|| anyhow!("inbound window total overflow"))?;

        if new_total > self.max_per_window {
            return Err(anyhow!("inbound cap exceeded for current window"));
        }

        self.window_inbound_total = new_total;
        Ok(())
    }
}

/// Helper: build a payload of the form `prefix || suffix` without an
/// intermediate allocation beyond the result vector.
fn concat_prefixed(prefix: &[u8], suffix: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(prefix.len() + suffix.len());
    out.extend_from_slice(prefix);
    out.extend_from_slice(suffix);
    out
}

/// Lowercase hex encoder for the `Display` impl of `ValidatorEvent`. Inlined
/// here (rather than pulling in the `hex` crate as a runtime dependency)
/// because event formatting is the only consumer and the format is trivial.
fn lowercase_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod rotation_test;

#[cfg(test)]
mod inbound_cap_test;

#[cfg(test)]
mod validator_bounds_test;

#[cfg(test)]
mod epoch_monotonicity_proptest;

#[cfg(test)]
mod window_guard_test;

#[cfg(test)]
mod window_tuning_doc_test;

#[cfg(test)]
mod validatorset_proptest;

#[cfg(test)]
mod validator_pause_test;

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
        let payload = bincode::serialize(&(new_set.to_bytes_vec(), epoch)).unwrap();

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
        let payload = bincode::serialize(&(new_set.to_bytes_vec(), epoch)).unwrap();

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
        let payload = bincode::serialize(&(new_set.to_bytes_vec(), epoch)).unwrap();

        let mut proofs = vec![];
        for i in 0..2 {
            let sig = kp_a[i].sign(&payload);
            proofs.push((kp_a[i].public, sig));
        }

        assert!(bridge.rotate_validators(new_set, epoch, proofs).is_err());
    }
}
