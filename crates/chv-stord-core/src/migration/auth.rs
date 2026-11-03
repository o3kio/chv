//! Receiver-side migration authorization, replay protection, and
//! per-node admission limits (issue #663).
//!
//! The token itself is defined in [`chv_common::migration_auth`] so the
//! control plane and stord share one implementation. This module adds the
//! receiver policy that must run **before** `create_receiving_volume`:
//!
//! 1. verify the control-plane signature and every claim (destination,
//!    volume, size, expiry),
//! 2. reserve one in-flight slot and the token's byte count against the
//!    configured per-node limits,
//! 3. consume the unique nonce exactly once.
//!
//! Only then may the backend provision a receiving volume. The reservation
//! is an RAII guard released on success, failure, and stream close.

use chv_common::migration_auth::{
    MigrationAuthError, MigrationAuthorizationClaims, MigrationAuthorizationExpectation,
    MigrationTokenCodec,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// How inbound migration streams are authorized.
#[derive(Clone)]
pub enum MigrationReceiverAuthorization {
    /// Production mode: every stream must carry a valid, unexpired,
    /// single-use control-plane token naming this node/volume/size.
    Required {
        codec: MigrationTokenCodec,
        destination_node_id: String,
    },
    /// Explicit development-only bypass, selected by
    /// `migration.allow_insecure_unauthenticated = true`. No token is
    /// required. Never reachable from a default config.
    DisabledInsecure,
    /// Fail closed: no valid authorization material is configured, so every
    /// inbound migration is rejected before volume creation. This is the
    /// default for a stord that never configured a signing secret.
    DenyAll,
}

impl std::fmt::Debug for MigrationReceiverAuthorization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Required {
                destination_node_id,
                ..
            } => f
                .debug_struct("MigrationReceiverAuthorization::Required")
                .field("destination_node_id", destination_node_id)
                .field("codec", &"<redacted>")
                .finish(),
            Self::DisabledInsecure => {
                f.write_str("MigrationReceiverAuthorization::DisabledInsecure")
            }
            Self::DenyAll => f.write_str("MigrationReceiverAuthorization::DenyAll"),
        }
    }
}

impl MigrationReceiverAuthorization {
    /// Build the production verifier. Rejects short secrets and empty node
    /// ids at construction (fail closed at startup, not per-stream).
    pub fn required(
        secret: &[u8],
        destination_node_id: impl Into<String>,
    ) -> Result<Self, MigrationAuthError> {
        let destination_node_id = destination_node_id.into();
        if destination_node_id.is_empty() {
            return Err(MigrationAuthError::InvalidClaims(
                "migration destination node id must not be empty".to_string(),
            ));
        }
        Ok(Self::Required {
            codec: MigrationTokenCodec::new(secret)?,
            destination_node_id,
        })
    }

    /// Verify a token for `volume_id`/`size_bytes` on this node.
    pub fn authorize(
        &self,
        token: &str,
        volume_id: &str,
        size_bytes: u64,
    ) -> Result<MigrationAuthorizationClaims, MigrationAuthError> {
        match self {
            Self::Required {
                codec,
                destination_node_id,
            } => codec.verify(
                token,
                MigrationAuthorizationExpectation {
                    destination_node: destination_node_id,
                    volume_id,
                    size_bytes,
                },
            ),
            Self::DisabledInsecure => {
                // No claims exist on the insecure path. Return a synthetic
                // claim set so callers keep one shape; the nonce is unique
                // per stream, so replay protection cannot block a second
                // insecure migration on a dev box.
                let nonce = chv_common::gen_short_id();
                Ok(MigrationAuthorizationClaims {
                    version: chv_common::migration_auth::MIGRATION_AUTH_TOKEN_VERSION,
                    source_node: String::new(),
                    destination_node: String::new(),
                    volume_id: volume_id.to_string(),
                    size_bytes,
                    issued_at_unix_ms: chv_common::now_unix_ms(),
                    expires_at_unix_ms: chv_common::now_unix_ms() + 60_000,
                    nonce,
                })
            }
            Self::DenyAll => Err(MigrationAuthError::NotConfigured),
        }
    }
}

/// Per-node inbound migration admission policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationLimits {
    pub max_inflight: usize,
    pub max_inflight_bytes: u64,
}

impl Default for MigrationLimits {
    fn default() -> Self {
        // Keep in sync with `StordMigrationConfig` defaults. These are
        // per-destination-node aggregate ceilings, not per-VM: 16 concurrent
        // receivers (enough for a VM with up to 16 attached volumes) and
        // 64 TiB of summed token-authorized bytes in flight. Both remain a
        // hard ceiling against an authorized peer opening unbounded or
        // arbitrarily large receiving volumes.
        Self {
            max_inflight: 16,
            max_inflight_bytes: 64 * 1024 * 1024 * 1024 * 1024,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum MigrationAdmissionError {
    ConcurrencyLimit { limit: usize },
    ByteLimit { limit: u64, requested: u64 },
    InvalidLimits,
}

impl std::fmt::Display for MigrationAdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConcurrencyLimit { limit } => write!(
                f,
                "destination already has {limit} concurrent receiving migration(s)"
            ),
            Self::ByteLimit { limit, requested } => write!(
                f,
                "destination in-flight migration byte limit reached (limit {limit}, requested {requested})"
            ),
            Self::InvalidLimits => write!(
                f,
                "migration admission limits are invalid (max_inflight and max_inflight_receiving_bytes must be > 0)"
            ),
        }
    }
}

#[derive(Debug, Default)]
struct AdmissionState {
    inflight: usize,
    bytes: u64,
}

/// Shared per-node admission state. Cheap to clone via `Arc` at the
/// service level; reservations hold an `Arc` back to it.
#[derive(Debug)]
pub struct MigrationAdmission {
    limits: MigrationLimits,
    state: Mutex<AdmissionState>,
}

impl MigrationAdmission {
    pub fn new(limits: MigrationLimits) -> Result<Arc<Self>, MigrationAdmissionError> {
        if limits.max_inflight == 0 || limits.max_inflight_bytes == 0 {
            return Err(MigrationAdmissionError::InvalidLimits);
        }
        Ok(Arc::new(Self {
            limits,
            state: Mutex::new(AdmissionState::default()),
        }))
    }

    /// Fail-closed admission for an invalid configuration: every reservation
    /// is rejected rather than silently allowing an unbounded receiver.
    pub fn unavailable() -> Arc<Self> {
        Arc::new(Self {
            limits: MigrationLimits {
                max_inflight: 0,
                max_inflight_bytes: 0,
            },
            state: Mutex::new(AdmissionState::default()),
        })
    }

    pub fn limits(&self) -> MigrationLimits {
        self.limits
    }

    /// Reserve one slot and `bytes` for an inbound migration. The returned
    /// guard releases both on drop (success, failure, or stream close).
    pub fn reserve(
        self: &Arc<Self>,
        bytes: u64,
    ) -> Result<MigrationReservation, MigrationAdmissionError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.inflight >= self.limits.max_inflight {
            return Err(MigrationAdmissionError::ConcurrencyLimit {
                limit: self.limits.max_inflight,
            });
        }
        let requested = state.bytes.saturating_add(bytes);
        if requested > self.limits.max_inflight_bytes {
            return Err(MigrationAdmissionError::ByteLimit {
                limit: self.limits.max_inflight_bytes,
                requested: bytes,
            });
        }
        state.inflight += 1;
        state.bytes = requested;
        Ok(MigrationReservation {
            admission: Arc::clone(self),
            bytes,
        })
    }

    fn release(&self, bytes: u64) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.inflight = state.inflight.saturating_sub(1);
        state.bytes = state.bytes.saturating_sub(bytes);
    }
}

/// RAII reservation released on every exit path.
#[derive(Debug)]
pub struct MigrationReservation {
    admission: Arc<MigrationAdmission>,
    bytes: u64,
}

impl Drop for MigrationReservation {
    fn drop(&mut self) {
        self.admission.release(self.bytes);
    }
}

/// Single-use nonce cache. Entries are kept only until their token expires;
/// a replay during the token's lifetime is rejected.
///
/// The cache is **memory-only** (not persisted across a stord restart) and
/// its size is bounded by the number of distinct unexpired tokens that can
/// be minted: each successful admission inserts at most one entry, purged
/// lazily on the next `consume` once `expires_at_unix_ms` passes. It is not
/// an unbounded history of every nonce ever seen.
#[derive(Debug, Default)]
pub struct MigrationReplayCache {
    used: Mutex<HashMap<String, i64>>,
}

impl MigrationReplayCache {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Consume `nonce` exactly once. Returns [`MigrationAuthError::Replay`]
    /// if it was already used. `expires_at_unix_ms` bounds retention.
    pub fn consume(
        &self,
        nonce: &str,
        expires_at_unix_ms: i64,
        now_unix_ms: i64,
    ) -> Result<(), MigrationAuthError> {
        let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        used.retain(|_, expires| *expires > now_unix_ms);
        if used.contains_key(nonce) {
            return Err(MigrationAuthError::Replay);
        }
        used.insert(nonce.to_string(), expires_at_unix_ms);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_enforces_concurrency_and_releases_on_drop() {
        let admission = MigrationAdmission::new(MigrationLimits {
            max_inflight: 1,
            max_inflight_bytes: 1024,
        })
        .unwrap();

        let first = admission.reserve(100).unwrap();
        assert_eq!(
            admission.reserve(100).unwrap_err(),
            MigrationAdmissionError::ConcurrencyLimit { limit: 1 }
        );
        drop(first);
        let second = admission.reserve(100).expect("released slot reusable");
        drop(second);
    }

    #[test]
    fn admission_enforces_total_bytes_and_releases_on_drop() {
        let admission = MigrationAdmission::new(MigrationLimits {
            max_inflight: 4,
            max_inflight_bytes: 150,
        })
        .unwrap();
        let first = admission.reserve(100).unwrap();
        assert_eq!(
            admission.reserve(51).unwrap_err(),
            MigrationAdmissionError::ByteLimit {
                limit: 150,
                requested: 51
            }
        );
        // Exactly the remaining room is allowed.
        let second = admission.reserve(50).unwrap();
        drop(first);
        drop(second);
        // All bytes released.
        admission
            .reserve(150)
            .expect("all reservation bytes released");
    }

    #[test]
    fn invalid_limits_fail_closed() {
        assert_eq!(
            MigrationAdmission::new(MigrationLimits {
                max_inflight: 0,
                max_inflight_bytes: 1
            })
            .unwrap_err(),
            MigrationAdmissionError::InvalidLimits
        );
        let unavailable = MigrationAdmission::unavailable();
        assert_eq!(
            unavailable.reserve(1).unwrap_err(),
            MigrationAdmissionError::ConcurrencyLimit { limit: 0 }
        );
    }

    #[test]
    fn replay_cache_rejects_second_use_until_expiry() {
        let cache = MigrationReplayCache::default();
        cache.consume("nonce-1", 2_000, 1_000).unwrap();
        assert_eq!(
            cache.consume("nonce-1", 2_000, 1_001).unwrap_err(),
            MigrationAuthError::Replay
        );
        // After expiry the entry is purged; a fresh token may reuse the
        // nonce string (mint uses 128-bit random nonces in practice).
        cache.consume("nonce-1", 3000, 2_001).unwrap();
    }

    #[test]
    fn deny_all_authorization_rejects_every_token() {
        let auth = MigrationReceiverAuthorization::DenyAll;
        assert_eq!(
            auth.authorize("anything", "vol-1", 1).unwrap_err(),
            MigrationAuthError::NotConfigured
        );
    }
}
