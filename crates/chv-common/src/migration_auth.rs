//! Short-lived, control-plane-issued authorization for stord disk
//! migrations (issue #663).
//!
//! mTLS authenticates *a* node certificate, but not *which* migration the
//! control plane actually asked for. Without a second authorization layer
//! any CA-certified peer could open a `StreamBlocks` stream and make the
//! destination provision an arbitrary peer-supplied volume id/size/format
//! (unlimited volume creation). This module defines the token that closes
//! that gap:
//!
//! * HMAC-SHA256 over a canonical JSON claim set, using a **dedicated**
//!   migration-signing secret (never the user/JWT or enrollment secret).
//! * The MAC key is derived with a domain-separation label so the same
//!   secret can never be confused with another protocol's HMAC use.
//! * Claims bind source node, destination node, volume id, size, issue
//!   time, expiry, and a unique nonce. The destination verifies every
//!   bound field before it creates a receiving volume; the nonce is
//!   consumed once by the receiver's replay cache.
//!
//! Token wire form: `v1.<hex(payload-json)>.<hex(hmac-sha256)>`. Hex keeps
//! the token printable/TOML-safe without adding a base64 dependency. The
//! payload is only trusted after the HMAC verifies in constant time using
//! the `hmac` crate's `verify_slice`.

use hmac::{Hmac, Mac};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;

/// Token format version. Bump on incompatible claim/wire changes; a
/// receiver rejects unknown versions rather than guessing.
pub const MIGRATION_AUTH_TOKEN_VERSION: u32 = 1;

/// Minimum signing-secret length. A short secret makes the MAC brute
/// forceable; 32 bytes matches the rest of CHV's secret floor.
pub const MIN_MIGRATION_SIGNING_SECRET_BYTES: usize = 32;

/// Default lifetime of a migration authorization. The token only needs to
/// cover the gap between the CP dispatching `MigrateVm` and the destination
/// stord opening the stream — not the whole transfer.
pub const DEFAULT_MIGRATION_TOKEN_TTL: Duration = Duration::from_secs(300);

/// Hard ceiling accepted by the verifier. A token valid for longer than
/// this is rejected even if correctly signed (defense in depth against a
/// compromised or buggy minting path).
pub const MAX_MIGRATION_TOKEN_TTL: Duration = Duration::from_secs(900);

/// Tolerated clock skew for `issued_at`.
pub const MIGRATION_TOKEN_CLOCK_SKEW: Duration = Duration::from_secs(60);

const KEY_DERIVATION_LABEL: &[u8] = b"chv/stord/migration-authorization/key/v1";
const SIGNING_DOMAIN: &[u8] = b"chv/stord/migration-authorization/token/v1\0";
const TOKEN_PREFIX: &str = "v1";

/// Claims bound into a migration authorization token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationAuthorizationClaims {
    pub version: u32,
    pub source_node: String,
    pub destination_node: String,
    pub volume_id: String,
    pub size_bytes: u64,
    pub issued_at_unix_ms: i64,
    pub expires_at_unix_ms: i64,
    /// Unique per mint; the receiver consumes it once.
    pub nonce: String,
}

/// The values the destination expects the token to authorize.
#[derive(Debug, Clone, Copy)]
pub struct MigrationAuthorizationExpectation<'a> {
    pub destination_node: &'a str,
    pub volume_id: &'a str,
    pub size_bytes: u64,
}

/// Why a token was rejected. Each variant maps to a closed, operator-
/// diagnosable error; none of them ever permits volume creation.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum MigrationAuthError {
    #[error("migration authorization token is missing")]
    Missing,
    #[error("migration authorization token is malformed")]
    Malformed,
    #[error("migration authorization token has an unsupported version")]
    UnsupportedVersion,
    #[error("migration authorization signature is invalid")]
    BadSignature,
    #[error("migration authorization token has expired")]
    Expired,
    #[error("migration authorization token is not valid yet (clock skew)")]
    NotYetValid,
    #[error("migration authorization lifetime is invalid or too long")]
    InvalidLifetime,
    #[error("migration authorization is for a different destination node")]
    WrongDestination,
    #[error("migration authorization is for a different volume")]
    WrongVolume,
    #[error("migration authorization is for a different volume size")]
    WrongSize,
    #[error("migration signing secret is too short (minimum {MIN_MIGRATION_SIGNING_SECRET_BYTES} bytes)")]
    SecretTooShort,
    #[error("migration authorization is not configured on this receiver")]
    NotConfigured,
    #[error("migration authorization token has already been used (replay rejected)")]
    Replay,
    #[error("migration authorization claims are invalid: {0}")]
    InvalidClaims(String),
}

/// A domain-separated HMAC codec for migration authorizations.
#[derive(Clone)]
pub struct MigrationTokenCodec {
    key: [u8; 32],
}

impl std::fmt::Debug for MigrationTokenCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MigrationTokenCodec")
            .field("key", &"<redacted>")
            .finish()
    }
}

impl MigrationTokenCodec {
    /// Build a codec from a dedicated migration-signing secret.
    pub fn new(secret: &[u8]) -> Result<Self, MigrationAuthError> {
        if secret.len() < MIN_MIGRATION_SIGNING_SECRET_BYTES {
            return Err(MigrationAuthError::SecretTooShort);
        }
        // Domain-separate the signing key from every other use of the same
        // configured secret (JWT, enrollment, webhooks, ...).
        let mut mac =
            HmacSha256::new_from_slice(secret).map_err(|_| MigrationAuthError::SecretTooShort)?;
        mac.update(KEY_DERIVATION_LABEL);
        let derived = mac.finalize().into_bytes();
        let mut key = [0u8; 32];
        key.copy_from_slice(&derived);
        Ok(Self { key })
    }

    /// Mint a token valid for `ttl` from now.
    pub fn mint(
        &self,
        source_node: &str,
        destination_node: &str,
        volume_id: &str,
        size_bytes: u64,
        ttl: Duration,
    ) -> Result<String, MigrationAuthError> {
        self.mint_at(
            source_node,
            destination_node,
            volume_id,
            size_bytes,
            now_unix_ms(),
            ttl,
        )
    }

    /// Mint with an explicit issue time. Kept public for deterministic
    /// negative tests (expiry/skew) without sleeping.
    pub fn mint_at(
        &self,
        source_node: &str,
        destination_node: &str,
        volume_id: &str,
        size_bytes: u64,
        issued_at_unix_ms: i64,
        ttl: Duration,
    ) -> Result<String, MigrationAuthError> {
        if source_node.is_empty() || destination_node.is_empty() || volume_id.is_empty() {
            return Err(MigrationAuthError::InvalidClaims(
                "source node, destination node, and volume id must not be empty".to_string(),
            ));
        }
        let ttl_ms = duration_millis(ttl)?;
        let expires_at_unix_ms = issued_at_unix_ms
            .checked_add(ttl_ms)
            .ok_or(MigrationAuthError::InvalidLifetime)?;
        let nonce_bytes: [u8; 16] = rand::rng().random();
        let claims = MigrationAuthorizationClaims {
            version: MIGRATION_AUTH_TOKEN_VERSION,
            source_node: source_node.to_string(),
            destination_node: destination_node.to_string(),
            volume_id: volume_id.to_string(),
            size_bytes,
            issued_at_unix_ms,
            expires_at_unix_ms,
            nonce: hex::encode(nonce_bytes),
        };
        self.sign_claims(&claims)
    }

    /// Mint an explicit claim set. Validates the same invariants as
    /// [`Self::mint_at`] so a caller cannot accidentally issue a
    /// long-lived or backwards-in-time token.
    pub fn sign_claims(
        &self,
        claims: &MigrationAuthorizationClaims,
    ) -> Result<String, MigrationAuthError> {
        self.validate_claims_for_mint(claims)?;
        let payload = serde_json::to_vec(claims)
            .map_err(|e| MigrationAuthError::InvalidClaims(e.to_string()))?;
        let payload_hex = hex::encode(&payload);
        let signature = self.sign_bytes(&payload);
        Ok(format!(
            "{TOKEN_PREFIX}.{payload_hex}.{}",
            hex::encode(signature)
        ))
    }

    /// Verify a token against the current wall clock.
    pub fn verify(
        &self,
        token: &str,
        expected: MigrationAuthorizationExpectation<'_>,
    ) -> Result<MigrationAuthorizationClaims, MigrationAuthError> {
        self.verify_at(token, expected, now_unix_ms())
    }

    /// Verify at an explicit time (deterministic tests).
    pub fn verify_at(
        &self,
        token: &str,
        expected: MigrationAuthorizationExpectation<'_>,
        now_unix_ms: i64,
    ) -> Result<MigrationAuthorizationClaims, MigrationAuthError> {
        let claims = self.verify_signature_and_parse(token)?;
        if claims.version != MIGRATION_AUTH_TOKEN_VERSION {
            return Err(MigrationAuthError::UnsupportedVersion);
        }
        if claims.destination_node != expected.destination_node {
            return Err(MigrationAuthError::WrongDestination);
        }
        if claims.volume_id != expected.volume_id {
            return Err(MigrationAuthError::WrongVolume);
        }
        if claims.size_bytes != expected.size_bytes {
            return Err(MigrationAuthError::WrongSize);
        }

        let now = now_unix_ms;
        if claims.expires_at_unix_ms <= now {
            return Err(MigrationAuthError::Expired);
        }
        let skew_ms = duration_millis(MIGRATION_TOKEN_CLOCK_SKEW)?;
        if claims.issued_at_unix_ms > now.saturating_add(skew_ms) {
            return Err(MigrationAuthError::NotYetValid);
        }
        let lifetime_ms = claims
            .expires_at_unix_ms
            .checked_sub(claims.issued_at_unix_ms)
            .ok_or(MigrationAuthError::InvalidLifetime)?;
        let max_ms = duration_millis(MAX_MIGRATION_TOKEN_TTL)?;
        if lifetime_ms <= 0 || lifetime_ms > max_ms {
            return Err(MigrationAuthError::InvalidLifetime);
        }
        Ok(claims)
    }

    fn validate_claims_for_mint(
        &self,
        claims: &MigrationAuthorizationClaims,
    ) -> Result<(), MigrationAuthError> {
        if claims.version != MIGRATION_AUTH_TOKEN_VERSION {
            return Err(MigrationAuthError::UnsupportedVersion);
        }
        if claims.source_node.is_empty()
            || claims.destination_node.is_empty()
            || claims.volume_id.is_empty()
            || claims.nonce.is_empty()
        {
            return Err(MigrationAuthError::InvalidClaims(
                "source node, destination node, volume id, and nonce must not be empty".to_string(),
            ));
        }
        let lifetime_ms = claims
            .expires_at_unix_ms
            .checked_sub(claims.issued_at_unix_ms)
            .ok_or(MigrationAuthError::InvalidLifetime)?;
        let max_ms = duration_millis(MAX_MIGRATION_TOKEN_TTL)?;
        if lifetime_ms <= 0 || lifetime_ms > max_ms {
            return Err(MigrationAuthError::InvalidLifetime);
        }
        Ok(())
    }

    fn verify_signature_and_parse(
        &self,
        token: &str,
    ) -> Result<MigrationAuthorizationClaims, MigrationAuthError> {
        if token.is_empty() {
            return Err(MigrationAuthError::Missing);
        }
        let mut parts = token.split('.');
        let prefix = parts.next().ok_or(MigrationAuthError::Malformed)?;
        let payload_hex = parts.next().ok_or(MigrationAuthError::Malformed)?;
        let signature_hex = parts.next().ok_or(MigrationAuthError::Malformed)?;
        if parts.next().is_some() || prefix != TOKEN_PREFIX {
            return Err(MigrationAuthError::Malformed);
        }
        let payload = hex::decode(payload_hex).map_err(|_| MigrationAuthError::Malformed)?;
        let signature = hex::decode(signature_hex).map_err(|_| MigrationAuthError::Malformed)?;

        let mut mac = HmacSha256::new_from_slice(&self.key)
            .map_err(|_| MigrationAuthError::SecretTooShort)?;
        mac.update(SIGNING_DOMAIN);
        mac.update(&payload);
        // Constant-time comparison; `BadSignature` for both a wrong MAC and
        // a wrong length, so a caller cannot distinguish them.
        mac.verify_slice(&signature)
            .map_err(|_| MigrationAuthError::BadSignature)?;

        serde_json::from_slice(&payload).map_err(|_| MigrationAuthError::Malformed)
    }

    fn sign_bytes(&self, payload: &[u8]) -> Vec<u8> {
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC accepts a 32-byte key");
        mac.update(SIGNING_DOMAIN);
        mac.update(payload);
        mac.finalize().into_bytes().to_vec()
    }
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as i64
}

fn duration_millis(duration: Duration) -> Result<i64, MigrationAuthError> {
    let millis = duration.as_millis();
    if millis == 0 || millis > i64::MAX as u128 {
        return Err(MigrationAuthError::InvalidLifetime);
    }
    Ok(millis as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"test-migration-signing-secret-0123456789abcdef";

    fn codec() -> MigrationTokenCodec {
        MigrationTokenCodec::new(SECRET).unwrap()
    }

    fn expectation<'a>(
        dest: &'a str,
        volume: &'a str,
        size: u64,
    ) -> MigrationAuthorizationExpectation<'a> {
        MigrationAuthorizationExpectation {
            destination_node: dest,
            volume_id: volume,
            size_bytes: size,
        }
    }

    #[test]
    fn minted_token_verifies_and_binds_all_claims() {
        let codec = codec();
        let now = 1_700_000_000_000;
        let token = codec
            .mint_at(
                "node-src",
                "node-dst",
                "vol-1",
                4096,
                now,
                Duration::from_secs(60),
            )
            .unwrap();
        let claims = codec
            .verify_at(&token, expectation("node-dst", "vol-1", 4096), now + 1)
            .unwrap();
        assert_eq!(claims.source_node, "node-src");
        assert_eq!(claims.destination_node, "node-dst");
        assert_eq!(claims.volume_id, "vol-1");
        assert_eq!(claims.size_bytes, 4096);
        assert_eq!(claims.issued_at_unix_ms, now);
        assert!(!claims.nonce.is_empty());
    }

    #[test]
    fn missing_and_malformed_tokens_are_rejected() {
        let codec = codec();
        let exp = expectation("node-dst", "vol-1", 4096);
        assert_eq!(
            codec.verify_at("", exp, 0).unwrap_err(),
            MigrationAuthError::Missing
        );
        for token in ["not-a-token", "v2.00.00", "v1.zz.00", "v1.00.zz", "v1.00"] {
            assert_eq!(
                codec.verify_at(token, exp, 0).unwrap_err(),
                MigrationAuthError::Malformed,
                "token {token:?} must be malformed"
            );
        }
    }

    #[test]
    fn tampered_payload_and_signature_are_rejected() {
        let codec = codec();
        let now = 1_700_000_000_000;
        let token = codec
            .mint_at(
                "node-src",
                "node-dst",
                "vol-1",
                4096,
                now,
                Duration::from_secs(60),
            )
            .unwrap();
        let mut parts: Vec<String> = token.split('.').map(str::to_string).collect();

        // Flip a payload byte without re-signing.
        let mut payload = hex::decode(&parts[1]).unwrap();
        let last = payload.len() - 1;
        payload[last] ^= 0x01;
        parts[1] = hex::encode(payload);
        let tampered_payload = parts.join(".");
        assert_eq!(
            codec
                .verify_at(
                    &tampered_payload,
                    expectation("node-dst", "vol-1", 4096),
                    now
                )
                .unwrap_err(),
            MigrationAuthError::BadSignature
        );

        // Flip a signature byte.
        let mut parts: Vec<String> = token.split('.').map(str::to_string).collect();
        let mut signature = hex::decode(&parts[2]).unwrap();
        signature[0] ^= 0x80;
        parts[2] = hex::encode(signature);
        let tampered_signature = parts.join(".");
        assert_eq!(
            codec
                .verify_at(
                    &tampered_signature,
                    expectation("node-dst", "vol-1", 4096),
                    now
                )
                .unwrap_err(),
            MigrationAuthError::BadSignature
        );
    }

    #[test]
    fn expiration_and_future_issue_time_are_rejected() {
        let codec = codec();
        let now = 1_700_000_000_000;
        let token = codec
            .mint_at(
                "node-src",
                "node-dst",
                "vol-1",
                4096,
                now,
                Duration::from_secs(60),
            )
            .unwrap();
        assert_eq!(
            codec
                .verify_at(&token, expectation("node-dst", "vol-1", 4096), now + 60_000)
                .unwrap_err(),
            MigrationAuthError::Expired
        );
        assert_eq!(
            codec
                .verify_at(
                    &token,
                    expectation("node-dst", "vol-1", 4096),
                    now - 120_000
                )
                .unwrap_err(),
            MigrationAuthError::NotYetValid
        );
    }

    #[test]
    fn wrong_destination_volume_and_size_are_rejected() {
        let codec = codec();
        let now = 1_700_000_000_000;
        let token = codec
            .mint_at(
                "node-src",
                "node-dst",
                "vol-1",
                4096,
                now,
                Duration::from_secs(60),
            )
            .unwrap();
        assert_eq!(
            codec
                .verify_at(&token, expectation("node-other", "vol-1", 4096), now)
                .unwrap_err(),
            MigrationAuthError::WrongDestination
        );
        assert_eq!(
            codec
                .verify_at(&token, expectation("node-dst", "vol-2", 4096), now)
                .unwrap_err(),
            MigrationAuthError::WrongVolume
        );
        assert_eq!(
            codec
                .verify_at(&token, expectation("node-dst", "vol-1", 8192), now)
                .unwrap_err(),
            MigrationAuthError::WrongSize
        );
    }

    #[test]
    fn short_secrets_and_overlong_lifetimes_are_rejected() {
        assert_eq!(
            MigrationTokenCodec::new(b"too-short").unwrap_err(),
            MigrationAuthError::SecretTooShort
        );
        let codec = codec();
        let now = 1_700_000_000_000;
        assert_eq!(
            codec
                .mint_at(
                    "node-src",
                    "node-dst",
                    "vol-1",
                    4096,
                    now,
                    MAX_MIGRATION_TOKEN_TTL + Duration::from_secs(1),
                )
                .unwrap_err(),
            MigrationAuthError::InvalidLifetime
        );
    }

    #[test]
    fn token_is_domain_separated_from_the_raw_secret() {
        // A MAC computed with the raw configured secret (no domain
        // derivation/label) must not verify.
        let codec = codec();
        let now = 1_700_000_000_000;
        let token = codec
            .mint_at(
                "node-src",
                "node-dst",
                "vol-1",
                4096,
                now,
                Duration::from_secs(60),
            )
            .unwrap();
        let parts: Vec<&str> = token.split('.').collect();
        let payload = hex::decode(parts[1]).unwrap();
        let mut raw = HmacSha256::new_from_slice(SECRET).unwrap();
        raw.update(&payload);
        let forged = format!(
            "v1.{}.{}",
            parts[1],
            hex::encode(raw.finalize().into_bytes())
        );
        assert_eq!(
            codec
                .verify_at(&forged, expectation("node-dst", "vol-1", 4096), now)
                .unwrap_err(),
            MigrationAuthError::BadSignature
        );
    }
}
