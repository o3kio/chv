use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tracing::{info, warn};

/// Errors returned by [`CredentialEncryption::decrypt`].
///
/// Decrypt fails closed: callers MUST handle these errors and never substitute
/// the ciphertext back into a credential field. Returning the literal
/// `enc:hex...` to a downstream consumer (e.g. the S3 client) makes auth
/// failures look like phantom AWS errors and silently breaks backups.
#[derive(Debug, Error)]
pub enum DecryptError {
    /// The input had the `enc:` prefix but the payload could not be hex-decoded
    /// or was shorter than the AES-GCM nonce. The stored value is corrupt.
    #[error("malformed encrypted credential")]
    Malformed,
    /// AES-GCM authentication failed. The configured key does not match the
    /// key used to encrypt this value, or the ciphertext was tampered with.
    #[error("credential authentication failed (wrong key or tampered ciphertext)")]
    AuthFailed,
    /// The decrypted bytes were not valid UTF-8. Authentic per AES-GCM, but
    /// not a recoverable string credential.
    #[error("decrypted credential is not valid UTF-8")]
    InvalidUtf8,
    /// No encryption key is configured. Callers that have an `enc:`-prefixed
    /// value in the database hit this when the operator forgot to set
    /// `CHV_ENCRYPTION_KEY` (or rotated it away) — the value cannot be
    /// recovered and the credential field MUST be treated as missing.
    #[error("no encryption key configured; cannot decrypt encrypted credential")]
    KeyUnavailable,
}

/// Errors returned by [`CredentialEncryption::encrypt`].
///
/// Encrypt fails closed: callers MUST handle these errors and MUST NOT write
/// the credential. The previous contract returned the plaintext on every
/// failure path (no key, bad nonce, cipher error), silently defeating
/// encryption-at-rest.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum EncryptError {
    /// No usable key is configured. Writing the credential would store it in
    /// plaintext at rest, so the write is rejected.
    #[error(
        "no credential encryption key is configured: refusing to store the \
         credential in plaintext. Set CHV_ENCRYPTION_KEY (or the legacy \
         CHV_JWT_SECRET fallback) and retry"
    )]
    KeyUnavailable,
    /// The generated nonce was not a valid AES-GCM nonce length. This should
    /// be impossible (the nonce is a fixed 12-byte array); treat it as a
    /// cryptographic configuration failure rather than downgrading to
    /// plaintext.
    #[error(
        "invalid AES-256-GCM nonce: refusing to store the credential in \
         plaintext; check the credential encryption configuration and retry"
    )]
    InvalidNonce,
    /// AES-256-GCM encryption failed. The credential is not persisted.
    #[error(
        "credential encryption failed ({0}): refusing to store the credential \
         in plaintext; retry, and contact the operator if the failure persists"
    )]
    EncryptionFailed(String),
}

#[derive(Clone)]
pub struct CredentialEncryption {
    cipher: Option<Aes256Gcm>,
}

impl Default for CredentialEncryption {
    /// Equivalent to [`CredentialEncryption::new`]: reads the process
    /// environment for the configured key.
    fn default() -> Self {
        Self::new()
    }
}

impl CredentialEncryption {
    pub fn new() -> Self {
        // NOTE (#336): the CHV_JWT_SECRET fallback below is a legacy path.
        // Both install surfaces have minted a dedicated CHV_ENCRYPTION_KEY
        // since #334/#335, so only pre-#335 hosts and env-driven deployments
        // reach it. It shares the JWT signing secret (chv-config): rotating
        // CHV_JWT_SECRET invalidates credentials encrypted under it — the
        // warn! below makes that hazard visible at startup instead of
        // surfacing later as opaque S3 AuthFailed errors. Dropping the
        // fallback entirely is a maintainer decision tracked in #336;
        // do not add new consumers.
        let (key_str, jwt_fallback) = match std::env::var("CHV_ENCRYPTION_KEY") {
            Ok(key) => (key, false),
            Err(_) => match std::env::var("CHV_JWT_SECRET") {
                Ok(jwt) => (jwt, true),
                Err(_) => (String::new(), false),
            },
        };

        // Unset AND present-but-empty must both warn: an empty value (e.g. a
        // truncated /etc/chv/encryption.env) otherwise disables encryption
        // silently — the install-time guards warn, but the daemon must not
        // trust them.
        if key_str.is_empty() {
            warn!(
                "CHV_ENCRYPTION_KEY is unset or empty; credential writes \
                 (backup S3 credentials, NetBox API tokens) will be rejected \
                 (fail-closed) until a key is configured. Existing plaintext \
                 rows remain readable, and previously encrypted rows require \
                 the original key."
            );
            return Self { cipher: None };
        }

        // Key-source observability (#336): the operator must be able to tell
        // from the logs which secret the credential cipher was keyed from.
        if jwt_fallback {
            warn!(
                "CHV_ENCRYPTION_KEY is unset; using the legacy CHV_JWT_SECRET \
                 fallback: S3 credentials are being encrypted under the JWT \
                 signing secret. Rotating CHV_JWT_SECRET will invalidate all \
                 stored S3 credentials — decryption fails closed (AuthFailed) \
                 and the credentials must be re-entered. See \
                 docs/runbooks/control-plane-dr.md for how to mint a \
                 dedicated CHV_ENCRYPTION_KEY and migrate off this fallback."
            );
        } else {
            info!(
                "CHV_ENCRYPTION_KEY is set; \
                 S3 credentials will be encrypted with the dedicated key"
            );
        }

        // SHA-256 always produces 32 bytes, which is the exact key size for
        // AES-256-GCM, so this constructor cannot fail in practice. We still
        // avoid `.expect()` in production code (ADR-008): on the impossible
        // failure path we fall back to a no-cipher state and log a clear
        // operator-visible warning instead of crashing the process. The
        // no-cipher state rejects credential writes (fail-closed).
        Self::from_key(&key_str)
    }

    /// Builds a cipher from an explicit key secret, bypassing environment
    /// lookup. Used by [`Self::new`] after key selection, by the
    /// control-plane bootstrap when `[secrets] credential_encryption` is
    /// configured (#662), and by the test-support constructors.
    pub fn from_key(key_str: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(key_str.as_bytes());
        let key_bytes = hasher.finalize();

        match Aes256Gcm::new_from_slice(&key_bytes) {
            Ok(cipher) => Self {
                cipher: Some(cipher),
            },
            Err(e) => {
                warn!(
                    error = %e,
                    "failed to construct AES-256-GCM cipher from key; credential \
                     writes will be rejected (fail-closed) until this is fixed"
                );
                Self { cipher: None }
            }
        }
    }

    /// Test-support constructor: a no-cipher instance that exercises the
    /// fail-closed write path without mutating process-global environment.
    #[cfg(any(test, feature = "test-util"))]
    pub fn without_key() -> Self {
        Self { cipher: None }
    }

    /// Test-support constructor: a cipher keyed from an explicit secret,
    /// without mutating process-global environment.
    #[cfg(any(test, feature = "test-util"))]
    pub fn with_key(key: &str) -> Self {
        Self::from_key(key)
    }

    /// Encrypts a credential for storage.
    ///
    /// Fail-closed contract: any failure (no key, invalid nonce, cipher error)
    /// returns an [`EncryptError`]. The plaintext is never returned as a
    /// successful result, so a caller that propagates the error cannot
    /// persist the credential in the clear.
    pub fn encrypt(&self, plaintext: &str) -> Result<String, EncryptError> {
        let Some(cipher) = &self.cipher else {
            metrics::counter!(
                "chv_credential_encrypt_total",
                "outcome" => "err_key_unavailable"
            )
            .increment(1);
            return Err(EncryptError::KeyUnavailable);
        };

        let nonce_bytes: [u8; 12] = rand::random();
        let nonce = match Nonce::try_from(nonce_bytes.as_slice()) {
            Ok(nonce) => nonce,
            Err(_) => {
                metrics::counter!(
                    "chv_credential_encrypt_total",
                    "outcome" => "err_invalid_nonce"
                )
                .increment(1);
                return Err(EncryptError::InvalidNonce);
            }
        };
        let ciphertext = match cipher.encrypt(&nonce, plaintext.as_bytes()) {
            Ok(ct) => ct,
            Err(e) => {
                metrics::counter!(
                    "chv_credential_encrypt_total",
                    "outcome" => "err_encrypt"
                )
                .increment(1);
                return Err(EncryptError::EncryptionFailed(e.to_string()));
            }
        };

        let mut combined = Vec::with_capacity(nonce_bytes.len() + ciphertext.len());
        combined.extend_from_slice(&nonce_bytes);
        combined.extend_from_slice(&ciphertext);

        metrics::counter!("chv_credential_encrypt_total", "outcome" => "ok").increment(1);
        Ok(format!("enc:{}", hex::encode(combined)))
    }

    /// Decrypts an `enc:`-prefixed credential.
    ///
    /// Fail-closed contract: any failure mode (wrong key, tampered ciphertext,
    /// malformed payload, missing key) returns a [`DecryptError`]. Callers
    /// MUST NOT fall back to the input string — doing so leaks ciphertext
    /// into downstream consumers as if it were a credential, which silently
    /// breaks S3 backups with opaque auth errors.
    ///
    /// Plaintext values without the `enc:` prefix are returned as-is to
    /// preserve backward compatibility with rows written before encryption
    /// was enabled.
    pub fn decrypt(&self, ciphertext: &str) -> Result<String, DecryptError> {
        // Plaintext (no `enc:` prefix) is a backward-compatibility case: the
        // row was written before encryption was enabled. Pass through unchanged.
        let Some(payload) = ciphertext.strip_prefix("enc:") else {
            metrics::counter!("chv_credential_decrypt_total", "outcome" => "ok_plaintext")
                .increment(1);
            return Ok(ciphertext.to_string());
        };

        // From here on, the value is supposed to be encrypted. Any failure is
        // a hard error — we never return the literal `enc:hex...` to a caller.

        let Some(cipher) = &self.cipher else {
            metrics::counter!("chv_credential_decrypt_total", "outcome" => "err_key_unavailable")
                .increment(1);
            return Err(DecryptError::KeyUnavailable);
        };

        let combined = match hex::decode(payload) {
            Ok(v) => v,
            Err(_) => {
                metrics::counter!("chv_credential_decrypt_total", "outcome" => "err_malformed")
                    .increment(1);
                return Err(DecryptError::Malformed);
            }
        };

        if combined.len() < 12 {
            metrics::counter!("chv_credential_decrypt_total", "outcome" => "err_malformed")
                .increment(1);
            return Err(DecryptError::Malformed);
        }

        let (nonce_bytes, encrypted) = combined.split_at(12);
        let nonce = match Nonce::try_from(nonce_bytes) {
            Ok(nonce) => nonce,
            Err(_) => {
                metrics::counter!("chv_credential_decrypt_total", "outcome" => "err_malformed")
                    .increment(1);
                return Err(DecryptError::Malformed);
            }
        };
        let plain_bytes = match cipher.decrypt(&nonce, encrypted) {
            Ok(v) => v,
            Err(_) => {
                metrics::counter!("chv_credential_decrypt_total", "outcome" => "err_auth")
                    .increment(1);
                return Err(DecryptError::AuthFailed);
            }
        };

        match String::from_utf8(plain_bytes) {
            Ok(s) => {
                metrics::counter!("chv_credential_decrypt_total", "outcome" => "ok").increment(1);
                Ok(s)
            }
            Err(_) => {
                metrics::counter!("chv_credential_decrypt_total", "outcome" => "err_invalid_utf8")
                    .increment(1);
                Err(DecryptError::InvalidUtf8)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Minimal `tracing` subscriber that records the message text of
    /// INFO- and WARN-level events, so tests can assert which key-source
    /// log line `CredentialEncryption::new` emitted (#336). Installed
    /// per-thread with `tracing::subscriber::set_default`, mirroring the
    /// warn-capture convention in `chv-nwd-core`'s fabric tests.
    mod log_capture {
        use std::sync::{Arc, Mutex as StdMutex};
        use tracing::field::Visit;
        use tracing::span::{Attributes, Id};
        use tracing::{Event, Level, Metadata};

        #[derive(Clone, Default)]
        pub struct LogCollector {
            events: Arc<StdMutex<Vec<(Level, String)>>>,
        }

        impl LogCollector {
            pub fn messages_at(&self, level: Level) -> Vec<String> {
                self.events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(l, _)| *l == level)
                    .map(|(_, m)| m.clone())
                    .collect()
            }
        }

        struct MessageVisitor(Option<String>);

        impl Visit for MessageVisitor {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = Some(format!("{:?}", value));
                }
            }
        }

        impl tracing::Subscriber for LogCollector {
            fn enabled(&self, metadata: &Metadata<'_>) -> bool {
                matches!(*metadata.level(), Level::INFO | Level::WARN)
            }

            fn new_span(&self, _span: &Attributes<'_>) -> Id {
                Id::from_u64(1)
            }

            fn record(&self, _span: &Id, _values: &tracing::span::Record<'_>) {}

            fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

            fn event(&self, event: &Event<'_>) {
                let mut visitor = MessageVisitor(None);
                event.record(&mut visitor);
                if let Some(message) = visitor.0 {
                    self.events
                        .lock()
                        .unwrap()
                        .push((*event.metadata().level(), message));
                }
            }

            fn enter(&self, _span: &Id) {}

            fn exit(&self, _span: &Id) {}
        }
    }

    /// Serializes all tests in this module so they don't race on the
    /// process-global `CHV_ENCRYPTION_KEY` and `CHV_JWT_SECRET` env vars.
    /// Tests that mutate these env vars MUST acquire this lock.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// RAII guard that snapshots and restores both env vars across a test.
    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev_enc: Option<String>,
        prev_jwt: Option<String>,
    }

    impl EnvGuard {
        fn lock() -> Self {
            // poisoned mutex is fine — we only use it for serialization
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let prev_enc = std::env::var("CHV_ENCRYPTION_KEY").ok();
            let prev_jwt = std::env::var("CHV_JWT_SECRET").ok();
            Self {
                _lock: lock,
                prev_enc,
                prev_jwt,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev_enc {
                Some(v) => std::env::set_var("CHV_ENCRYPTION_KEY", v),
                None => std::env::remove_var("CHV_ENCRYPTION_KEY"),
            }
            match &self.prev_jwt {
                Some(v) => std::env::set_var("CHV_JWT_SECRET", v),
                None => std::env::remove_var("CHV_JWT_SECRET"),
            }
        }
    }

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let _g = EnvGuard::lock();
        std::env::set_var("CHV_ENCRYPTION_KEY", "test-key-for-unit-tests-12345");

        let crypto = CredentialEncryption::new();
        let plaintext = "my-super-secret-key";

        let encrypted = crypto.encrypt(plaintext).expect("encryption succeeds");
        assert!(encrypted.starts_with("enc:"));
        assert_ne!(encrypted, plaintext);

        let decrypted = crypto.decrypt(&encrypted).expect("roundtrip succeeds");
        assert_eq!(decrypted, plaintext);
    }

    /// #336: the dedicated-key path must log an info line (so the key
    /// source is visible at startup) and must NOT warn — a warning here
    /// would train operators to ignore the real fallback warning.
    #[test]
    fn dedicated_key_logs_info_and_does_not_warn() {
        let _g = EnvGuard::lock();
        std::env::set_var("CHV_ENCRYPTION_KEY", "dedicated-key-observability");
        std::env::remove_var("CHV_JWT_SECRET");

        let logs = log_capture::LogCollector::default();
        let _subscriber = tracing::subscriber::set_default(logs.clone());
        let crypto = CredentialEncryption::new();

        let warnings = logs.messages_at(tracing::Level::WARN);
        assert!(
            warnings.is_empty(),
            "dedicated key must not warn, got {:?}",
            warnings
        );
        assert!(
            logs.messages_at(tracing::Level::INFO)
                .iter()
                .any(|m| m.contains("CHV_ENCRYPTION_KEY")),
            "dedicated key must log an info line naming CHV_ENCRYPTION_KEY"
        );

        // The key-source logging is observability only: the cipher still
        // round-trips exactly as before.
        let encrypted = crypto
            .encrypt("dedicated-key-plaintext")
            .expect("encryption succeeds");
        assert!(encrypted.starts_with("enc:"));
        assert_eq!(
            crypto.decrypt(&encrypted).expect("roundtrip succeeds"),
            "dedicated-key-plaintext"
        );
    }

    /// #336: the legacy CHV_JWT_SECRET fallback must log a warn-level
    /// startup warning that states the rotation hazard plainly (S3
    /// credentials encrypted under the JWT signing secret; rotating
    /// CHV_JWT_SECRET invalidates them, fail-closed) and points at the
    /// runbook for migrating to a dedicated key.
    #[test]
    fn jwt_fallback_logs_rotation_hazard_warning() {
        let _g = EnvGuard::lock();
        std::env::remove_var("CHV_ENCRYPTION_KEY");
        std::env::set_var("CHV_JWT_SECRET", "jwt-secret-legacy-fallback");

        let logs = log_capture::LogCollector::default();
        let _subscriber = tracing::subscriber::set_default(logs.clone());
        let crypto = CredentialEncryption::new();

        let warnings = logs.messages_at(tracing::Level::WARN);
        let warning = warnings
            .iter()
            .find(|m| m.contains("CHV_JWT_SECRET"))
            .expect("fallback must warn, got no warning");
        assert!(
            warning.contains("JWT signing secret"),
            "warning must say the credentials ride the JWT signing secret: {}",
            warning
        );
        assert!(
            warning.contains("Rotating CHV_JWT_SECRET will invalidate"),
            "warning must state the rotation hazard: {}",
            warning
        );
        assert!(
            warning.contains("AuthFailed"),
            "warning must name the fail-closed outcome: {}",
            warning
        );
        assert!(
            warning.contains("control-plane-dr.md"),
            "warning must point at the migration runbook: {}",
            warning
        );

        // Behavior is unchanged: the fallback key still encrypts and
        // decrypts round-trip exactly as before.
        let encrypted = crypto
            .encrypt("jwt-fallback-plaintext")
            .expect("encryption succeeds");
        assert!(encrypted.starts_with("enc:"));
        assert_eq!(
            crypto.decrypt(&encrypted).expect("roundtrip succeeds"),
            "jwt-fallback-plaintext"
        );
    }

    #[test]
    fn test_decrypt_plaintext_backward_compatible() {
        let _g = EnvGuard::lock();
        std::env::set_var("CHV_ENCRYPTION_KEY", "test-key-for-unit-tests-12345");

        let crypto = CredentialEncryption::new();
        let plaintext = "plain-old-value";

        // Decrypting a non-prefixed string returns it as-is for back-compat
        // with rows written before encryption was enabled.
        let decrypted = crypto
            .decrypt(plaintext)
            .expect("plaintext passthrough succeeds");
        assert_eq!(decrypted, plaintext);
    }

    /// Fail-closed (#655): with no key, `encrypt` MUST return
    /// `KeyUnavailable` and MUST NOT return the input as a successful
    /// ciphertext. The previous contract returned the plaintext verbatim,
    /// silently defeating encryption at rest.
    #[test]
    fn test_no_key_encrypt_fails_closed() {
        let _g = EnvGuard::lock();
        std::env::remove_var("CHV_ENCRYPTION_KEY");
        std::env::remove_var("CHV_JWT_SECRET");

        let crypto = CredentialEncryption::new();
        let plaintext = "no-encryption-active";

        let result = crypto.encrypt(plaintext);
        assert!(
            matches!(result, Err(EncryptError::KeyUnavailable)),
            "expected KeyUnavailable with no key, got {:?}",
            result
        );
        assert_ne!(result, Ok(plaintext.to_string()));

        // Compatibility: legacy plaintext rows remain readable without a key.
        assert_eq!(
            crypto
                .decrypt(plaintext)
                .expect("plaintext passthrough still works"),
            plaintext
        );
    }

    #[test]
    fn test_without_key_constructor_encrypt_fails_closed() {
        let crypto = CredentialEncryption::without_key();
        let plaintext = "injected-no-key";

        let result = crypto.encrypt(plaintext);
        assert!(
            matches!(result, Err(EncryptError::KeyUnavailable)),
            "expected KeyUnavailable, got {:?}",
            result
        );
        assert_ne!(result, Ok(plaintext.to_string()));
    }

    /// The no-key error must tell the operator exactly which setting to fix.
    #[test]
    fn encrypt_error_message_names_encryption_key() {
        let err = CredentialEncryption::without_key()
            .encrypt("secret")
            .expect_err("no-key encryption must fail");
        let message = err.to_string();
        assert!(
            message.contains("CHV_ENCRYPTION_KEY"),
            "operator-facing error must name CHV_ENCRYPTION_KEY: {message}"
        );
        assert!(
            message.contains("plaintext"),
            "operator-facing error must state the plaintext hazard: {message}"
        );
    }

    /// A present-but-empty CHV_ENCRYPTION_KEY (e.g. a truncated
    /// /etc/chv/encryption.env) must behave exactly like no key — and, as
    /// of #335 round 4, emit the same startup warning instead of silently
    /// disabling encryption. Writes are now rejected (#655).
    #[test]
    fn test_empty_key_encrypt_fails_closed() {
        let _g = EnvGuard::lock();
        std::env::set_var("CHV_ENCRYPTION_KEY", "");
        std::env::remove_var("CHV_JWT_SECRET");

        let crypto = CredentialEncryption::new();
        let plaintext = "empty-key-no-encryption";

        let result = crypto.encrypt(plaintext);
        assert!(
            matches!(result, Err(EncryptError::KeyUnavailable)),
            "expected KeyUnavailable with an empty key, got {:?}",
            result
        );
        assert_ne!(result, Ok(plaintext.to_string()));
    }

    /// Fail-closed: tampered ciphertext MUST surface AuthFailed, not be
    /// returned verbatim. Returning the literal `enc:hex...` would let the
    /// caller pass the ciphertext into the S3 client as if it were a
    /// credential.
    #[test]
    fn tampered_ciphertext_returns_auth_failed() {
        let _g = EnvGuard::lock();
        std::env::set_var("CHV_ENCRYPTION_KEY", "tamper-test-key");

        let crypto = CredentialEncryption::new();
        let plaintext = "sensitive-credential";
        let encrypted = crypto.encrypt(plaintext).expect("encryption succeeds");
        assert!(encrypted.starts_with("enc:"));

        // Flip one byte near the tail (auth tag region).
        let mut bytes: Vec<u8> = encrypted.as_bytes().to_vec();
        let tail = bytes.len() - 1;
        bytes[tail] = if bytes[tail] == b'a' { b'b' } else { b'a' };
        let tampered = String::from_utf8(bytes).expect("ASCII hex");

        let result = crypto.decrypt(&tampered);
        assert!(
            matches!(result, Err(DecryptError::AuthFailed)),
            "expected AuthFailed for tampered ciphertext, got {:?}",
            result
        );
    }

    /// C4 regression test: ciphertext encrypted with key A MUST surface
    /// AuthFailed when decrypted with key B. The previous fail-soft contract
    /// returned the ciphertext literal here, which then ended up written
    /// into `s3_access_key`/`s3_secret_key` and passed to the S3 client as
    /// a credential — silently breaking backups with opaque auth errors.
    #[test]
    fn wrong_key_returns_auth_failed() {
        let _g = EnvGuard::lock();

        // Encrypt with key A.
        std::env::set_var("CHV_ENCRYPTION_KEY", "key-A-for-encryption");
        let crypto_a = CredentialEncryption::new();
        let plaintext = "secret-under-key-A";
        let encrypted = crypto_a.encrypt(plaintext).expect("encryption succeeds");
        assert!(encrypted.starts_with("enc:"));

        // Decrypt with key B.
        std::env::set_var("CHV_ENCRYPTION_KEY", "key-B-totally-different");
        let crypto_b = CredentialEncryption::new();
        let result = crypto_b.decrypt(&encrypted);

        assert!(
            matches!(result, Err(DecryptError::AuthFailed)),
            "expected AuthFailed under wrong key, got {:?}",
            result
        );
    }

    #[test]
    fn malformed_hex_after_enc_prefix_returns_malformed() {
        let _g = EnvGuard::lock();
        std::env::set_var("CHV_ENCRYPTION_KEY", "any-key");

        let crypto = CredentialEncryption::new();
        let result = crypto.decrypt("enc:not-valid-hex-zzz");

        assert!(
            matches!(result, Err(DecryptError::Malformed)),
            "expected Malformed for non-hex payload, got {:?}",
            result
        );
    }

    #[test]
    fn empty_ciphertext_after_prefix_returns_malformed() {
        let _g = EnvGuard::lock();
        std::env::set_var("CHV_ENCRYPTION_KEY", "any-key");

        let crypto = CredentialEncryption::new();
        // hex-decode of "" is Ok([]); length 0 is below the 12-byte nonce floor.
        let result = crypto.decrypt("enc:");

        assert!(
            matches!(result, Err(DecryptError::Malformed)),
            "expected Malformed for empty payload, got {:?}",
            result
        );
    }

    #[test]
    fn short_ciphertext_below_nonce_size_returns_malformed() {
        let _g = EnvGuard::lock();
        std::env::set_var("CHV_ENCRYPTION_KEY", "any-key");

        let crypto = CredentialEncryption::new();
        let result = crypto.decrypt("enc:0011");

        assert!(
            matches!(result, Err(DecryptError::Malformed)),
            "expected Malformed for under-nonce payload, got {:?}",
            result
        );
    }

    /// An `enc:`-prefixed value with no configured key cannot be recovered.
    /// We must return KeyUnavailable, not the ciphertext literal.
    #[test]
    fn enc_prefixed_with_no_key_returns_key_unavailable() {
        let _g = EnvGuard::lock();

        // Encrypt under a key…
        std::env::set_var("CHV_ENCRYPTION_KEY", "key-that-will-be-removed");
        let crypto_with_key = CredentialEncryption::new();
        let encrypted = crypto_with_key
            .encrypt("plaintext-under-key")
            .expect("encryption succeeds");
        assert!(encrypted.starts_with("enc:"));

        // …then drop the key (operator misconfig / rotation gap).
        std::env::remove_var("CHV_ENCRYPTION_KEY");
        std::env::remove_var("CHV_JWT_SECRET");
        let crypto_no_key = CredentialEncryption::new();
        let result = crypto_no_key.decrypt(&encrypted);

        assert!(
            matches!(result, Err(DecryptError::KeyUnavailable)),
            "expected KeyUnavailable when key is missing, got {:?}",
            result
        );
    }

    #[test]
    fn encrypt_then_decrypt_roundtrip_with_unicode() {
        let _g = EnvGuard::lock();
        std::env::set_var("CHV_ENCRYPTION_KEY", "unicode-roundtrip-key");

        let crypto = CredentialEncryption::new();
        let plaintext = "hello 🌍 — naïve café 日本語 🚀";

        let encrypted = crypto.encrypt(plaintext).expect("encryption succeeds");
        assert!(encrypted.starts_with("enc:"));

        let decrypted = crypto.decrypt(&encrypted).expect("unicode roundtrip");
        assert_eq!(decrypted, plaintext);
    }
}
