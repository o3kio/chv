//! Startup load + validation of the storage-migration mTLS identity.
//!
//! This is the daemon-wiring half of issue #232: production configuration must
//! be able to supply the existing [`MigrationTlsConfig`] capability (previously
//! hard-coded as `None` in `cmd/chv-stord`).
//!
//! It also hosts the *server half* of migration mTLS (issue #390):
//! [`load_migration_server_tls`] loads the material for the migration
//! receiver's TLS TCP listener.
//!
//! # Fail-closed semantics
//!
//! - `enabled = false` → `Ok(None)`: the daemon starts without migration
//!   credentials, and migration actions fail as *unavailable* — the sender
//!   already refuses to run without `MigrationTlsConfig` (no plaintext
//!   fallback, see `migration/sender.rs`).
//! - `enabled = false` with **any** client identity field set → startup
//!   **error**: an operator who believes migration is disabled must not
//!   have a half-configured identity silently ignored (fail-closed,
//!   symmetric with the receiver half, issue #395).
//! - `enabled = true` → the client half is all-or-nothing: with **no**
//!   client fields set it is simply absent → `Ok(None)` (destination-only
//!   stord, issue #401: this stord never initiates migrations, and
//!   outbound migration actions fail with the sender's
//!   `failed_precondition` error); with **any**
//!   client field set, all four identity inputs are required, files must be
//!   readable, the certificate/key pair must match, and the CA bundle must
//!   parse. Any problem is a **startup error** (fail-closed).
//! - Receiver (server) TLS fields are all-or-nothing: none set → `Ok(None)`
//!   (source-only stord, a legitimate deployment); *partially* set, an
//!   unreadable file, a mismatched keypair, an invalid/empty client CA
//!   bundle, or an unparseable `listen_addr` → **startup error**. With
//!   `enabled = false`, any receiver TLS field set is a startup error;
//!   authorization/admission keys (`node_id`, `signing_secret*`,
//!   `max_inflight_*`, `allow_insecure_unauthenticated`) are not listener
//!   fields and stay inert in that case.
//! - `enabled = true` with **neither** half configured → startup **error**
//!   (see [`ensure_migration_half_configured`], issue #401): an enabled
//!   migration section that configures nothing is a misconfiguration — the
//!   daemon would run with migrations unavailable in both directions while
//!   the operator believes migration is on.
//!
//! # Security rules honored here
//!
//! - No plaintext fallback and no "skip verify" option.
//! - No PEM/private-key material in logs or errors — only file paths and
//!   generic failure reasons.

use crate::migration::sender::MigrationTlsConfig;
use std::fmt;
use std::net::SocketAddr;
use std::path::Path;

/// Errors produced while loading/validating the migration TLS identity.
///
/// Deliberately does not carry PEM/private-key material (see module docs).
#[derive(Debug)]
pub enum MigrationTlsLoadError {
    /// A required identity field is missing while migration is enabled.
    Missing(String),
    /// A configured identity file could not be read. `field` is the config
    /// key the path came from (e.g. `migration.client_cert_path`), so an
    /// operator who set a key to `""` can see which key it was.
    Unreadable {
        field: &'static str,
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    /// The identity material failed to parse or the key does not match the
    /// certificate.
    Invalid(String),
}

impl fmt::Display for MigrationTlsLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MigrationTlsLoadError::Missing(msg) => write!(f, "migration TLS config missing: {msg}"),
            MigrationTlsLoadError::Unreadable {
                field,
                path,
                source,
            } => {
                // An empty path renders as nothing at all ("cannot read
                // migration TLS file  : No such file or directory"),
                // hiding which config key was set to "". Name the key and
                // say the path is empty so the operator sees both.
                if path.as_os_str().is_empty() {
                    write!(
                        f,
                        "cannot read migration TLS file for {field}: the configured path \
                         is empty — an empty string counts as set but names no file; \
                         remove the key or set a real path"
                    )
                } else {
                    write!(
                        f,
                        "cannot read migration TLS file for {field} at {}: {source}",
                        path.display()
                    )
                }
            }
            MigrationTlsLoadError::Invalid(msg) => {
                write!(f, "invalid migration TLS material: {msg}")
            }
        }
    }
}

impl std::error::Error for MigrationTlsLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MigrationTlsLoadError::Unreadable { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Load and validate the migration mTLS identity (client/source half) from
/// configuration fields.
///
/// Returns `Ok(None)` when `enabled` is `false` **and** no client identity
/// field is configured. Setting any client field (`client_cert_path`,
/// `client_key_path`, `ca_cert_path`, `dest_server_name`) while
/// `enabled = false` is a startup **error**: an operator who believes
/// migration is disabled must not have a half-configured identity silently
/// ignored (fail-closed, symmetric with the receiver half — issue #395).
///
/// When `enabled` is `true`, the client half is all-or-nothing (issue #401):
///
/// - with **no** client field set → `Ok(None)`: this stord is
///   destination-only — it never initiates migrations, and outbound
///   migration actions fail with the sender's `failed_precondition` error
///   (no plaintext fallback, exactly as when migration is disabled);
/// - with **any** client field set → all four inputs are required, files
///   must be readable, the certificate/key pair must match, and the CA
///   bundle must parse, returning a fully validated
///   [`MigrationTlsConfig`] on success. A partially configured client half
///   is a startup **error** (fail-closed).
///
/// The `enabled = true`-but-nothing-at-all case (neither this half nor the
/// receiver half configured) is rejected by
/// [`ensure_migration_half_configured`], which sees both halves.
pub fn load_migration_tls(
    enabled: bool,
    client_cert_path: Option<&Path>,
    client_key_path: Option<&Path>,
    ca_cert_path: Option<&Path>,
    dest_server_name: Option<&str>,
) -> Result<Option<MigrationTlsConfig>, MigrationTlsLoadError> {
    if !enabled {
        let any_set = client_cert_path.is_some()
            || client_key_path.is_some()
            || ca_cert_path.is_some()
            || dest_server_name.is_some();
        if any_set {
            return Err(MigrationTlsLoadError::Invalid(
                "migration client fields (client_cert_path, client_key_path, ca_cert_path, \
                 dest_server_name) are configured but migration.enabled = false — set \
                 migration.enabled = true or remove the client fields"
                    .into(),
            ));
        }
        // No startup log here: the disabled-migration confirmation line is
        // emitted by the daemon wiring (`load_migration_materials` in
        // `cmd/chv-stord`), so it fires exactly once per startup. This
        // loader used to emit its own copy, which duplicated the wiring's
        // line before #483 removed the latter.
        return Ok(None);
    }

    // Client half absent under `enabled = true` (issue #401): a stord may be
    // destination-only. `Ok(None)` means "this stord never initiates
    // migrations" — the sender refuses to run without `MigrationTlsConfig`,
    // so outbound migration actions fail with a `failed_precondition`
    // error, never as plaintext.
    let any_client_set = client_cert_path.is_some()
        || client_key_path.is_some()
        || ca_cert_path.is_some()
        || dest_server_name.is_some();
    if !any_client_set {
        tracing::info!(
            "storage migration client identity not configured: this stord will not initiate storage migrations"
        );
        return Ok(None);
    }

    let cert_path = client_cert_path.ok_or_else(|| {
        MigrationTlsLoadError::Missing(
            "migration.client_cert_path is required when any migration client field is set \
             (migration.enabled = true)"
                .into(),
        )
    })?;
    let key_path = client_key_path.ok_or_else(|| {
        MigrationTlsLoadError::Missing(
            "migration.client_key_path is required when any migration client field is set \
             (migration.enabled = true)"
                .into(),
        )
    })?;
    let ca_path = ca_cert_path.ok_or_else(|| {
        MigrationTlsLoadError::Missing(
            "migration.ca_cert_path is required when any migration client field is set \
             (migration.enabled = true)"
                .into(),
        )
    })?;
    let dest = dest_server_name
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            MigrationTlsLoadError::Missing(
                "migration.dest_server_name is required when any migration client field is set \
                 (migration.enabled = true)"
                    .into(),
            )
        })?;

    let cert_pem = read_identity_file(cert_path, "migration.client_cert_path")?;
    let key_pem = read_identity_file(key_path, "migration.client_key_path")?;
    let ca_pem = read_identity_file(ca_path, "migration.ca_cert_path")?;

    validate_keypair(&cert_pem, &key_pem)?;
    validate_ca_bundle(&ca_pem)?;

    Ok(Some(MigrationTlsConfig {
        cert_pem,
        key_pem,
        ca_pem,
        dest_domain: dest.to_string(),
    }))
}

/// Validated server-side mTLS material for the migration receiver listener.
///
/// Only produced by [`load_migration_server_tls`], which enforces the
/// fail-closed invariants (all-or-nothing presence, readable files, matching
/// keypair, non-empty CA bundle, parseable listen address).
#[derive(Debug, Clone)]
pub struct MigrationServerTls {
    /// TCP address the migration receiver listener binds.
    pub listen_addr: SocketAddr,
    /// Server certificate PEM presented to migration peers.
    pub cert_pem: Vec<u8>,
    /// Server private key PEM.
    pub key_pem: Vec<u8>,
    /// CA bundle PEM used to authenticate migration peer client
    /// certificates. Client-certificate authentication is mandatory on the
    /// listener built from this material (see `server.rs`).
    pub client_ca_pem: Vec<u8>,
}

/// Load and validate the migration *receiver* mTLS material (server half of
/// migration TLS, issue #390).
///
/// `migration.enabled` is the master switch, exactly as for the client
/// half: the receiver fields take effect **only** when it is `true`.
///
/// - `enabled = false` (the default) with **no** receiver fields → `Ok(None)`:
///   no listener, migrations unavailable in both directions;
/// - `enabled = false` with **any** receiver field set → startup **error** —
///   an operator who believes migration is disabled must not silently get an
///   inbound TCP listener (fail-closed, explicit);
/// - `enabled = true` with no receiver fields → `Ok(None)`: this stord is
///   migration-source-only and never accepts inbound migrations (a legitimate
///   deployment);
/// - `enabled = true` with any receiver field set → all four are required,
///   files must be readable, the server certificate/key pair must match, the
///   client CA bundle must parse and be non-empty, and `listen_addr` must be a
///   valid socket address. Any problem is a **startup error** (fail-closed) —
///   there is no plaintext listener and no client-auth-optional mode.
///
/// Note: the client half (`load_migration_tls`) is symmetric: client fields
/// set with `enabled = false` are likewise a startup error (issue #395).
pub fn load_migration_server_tls(
    enabled: bool,
    listen_addr: Option<&str>,
    server_cert_path: Option<&Path>,
    server_key_path: Option<&Path>,
    client_ca_path: Option<&Path>,
) -> Result<Option<MigrationServerTls>, MigrationTlsLoadError> {
    let any_set = listen_addr.is_some()
        || server_cert_path.is_some()
        || server_key_path.is_some()
        || client_ca_path.is_some();
    if !any_set {
        if enabled {
            tracing::info!(
                "storage migration receiver listener not configured: this stord will not accept inbound migrations"
            );
        }
        return Ok(None);
    }
    if !enabled {
        return Err(MigrationTlsLoadError::Invalid(
            "migration receiver fields (listen_addr, server_cert_path, server_key_path, \
             client_ca_path) are configured but migration.enabled = false — set \
             migration.enabled = true or remove the receiver fields"
                .into(),
        ));
    }

    let addr_str = listen_addr.ok_or_else(|| {
        MigrationTlsLoadError::Missing(
            "migration.listen_addr is required when any migration receiver field is set".into(),
        )
    })?;
    let cert_path = server_cert_path.ok_or_else(|| {
        MigrationTlsLoadError::Missing(
            "migration.server_cert_path is required when any migration receiver field is set"
                .into(),
        )
    })?;
    let key_path = server_key_path.ok_or_else(|| {
        MigrationTlsLoadError::Missing(
            "migration.server_key_path is required when any migration receiver field is set".into(),
        )
    })?;
    let ca_path = client_ca_path.ok_or_else(|| {
        MigrationTlsLoadError::Missing(
            "migration.client_ca_path is required when any migration receiver field is set".into(),
        )
    })?;

    let listen_addr = addr_str.trim().parse::<SocketAddr>().map_err(|_| {
        MigrationTlsLoadError::Invalid(format!(
            "migration.listen_addr {addr_str:?} is not a valid socket address \
                 (expected \"host:port\", e.g. \"127.0.0.1:50052\")"
        ))
    })?;

    let cert_pem = read_identity_file(cert_path, "migration.server_cert_path")?;
    let key_pem = read_identity_file(key_path, "migration.server_key_path")?;
    let client_ca_pem = read_identity_file(ca_path, "migration.client_ca_path")?;

    validate_keypair(&cert_pem, &key_pem)?;
    validate_ca_bundle(&client_ca_pem)?;

    Ok(Some(MigrationServerTls {
        listen_addr,
        cert_pem,
        key_pem,
        client_ca_pem,
    }))
}

/// Fail-closed cross-half check (issue #401): `migration.enabled = true`
/// must configure at least one of the two migration halves.
///
/// The halves are independently optional under `enabled = true` — a stord
/// may be source-only (client identity, no receiver), destination-only
/// (receiver, no client identity), or both — but an *enabled* migration
/// section that configures **nothing** is a misconfiguration: the daemon
/// would run with migrations unavailable in both directions while the
/// operator believes migration is on. That case is a startup **error**.
///
/// Call this after both [`load_migration_tls`] and
/// [`load_migration_server_tls`] have succeeded, passing their results and
/// the same `enabled` flag. With `enabled = false` it is always `Ok(())`
/// (each loader already rejects half-configured fields on its own).
pub fn ensure_migration_half_configured(
    enabled: bool,
    client_tls: Option<&MigrationTlsConfig>,
    server_tls: Option<&MigrationServerTls>,
) -> Result<(), MigrationTlsLoadError> {
    if !enabled || client_tls.is_some() || server_tls.is_some() {
        return Ok(());
    }
    Err(MigrationTlsLoadError::Invalid(
        "migration.enabled = true but neither the client half (client_cert_path, \
         client_key_path, ca_cert_path, dest_server_name) nor the receiver half \
         (listen_addr, server_cert_path, server_key_path, client_ca_path) is configured — \
         configure at least one half or set migration.enabled = false"
            .into(),
    ))
}

/// Read a PEM identity file. Errors are surfaced without file contents.
/// `field` is the config key the path came from; it is carried in the
/// error so the message can name the key (see
/// [`MigrationTlsLoadError::Unreadable`]).
fn read_identity_file(path: &Path, field: &'static str) -> Result<Vec<u8>, MigrationTlsLoadError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(bytes),
        Err(source) => {
            let err = MigrationTlsLoadError::Unreadable {
                field,
                path: path.to_path_buf(),
                source,
            };
            tracing::error!(field, path = %path.display(), "cannot read migration TLS file");
            Err(err)
        }
    }
}

/// Validate that `key_pem` is a parseable private key and that it matches the
/// public key in `cert_pem` (SPKI comparison).
fn validate_keypair(cert_pem: &[u8], key_pem: &[u8]) -> Result<(), MigrationTlsLoadError> {
    let cert_str = String::from_utf8_lossy(cert_pem);
    let key_str = String::from_utf8_lossy(key_pem);

    // Parsing the key validates it is a well-formed private key.
    let key = rcgen::KeyPair::from_pem(&key_str)
        .map_err(|e| MigrationTlsLoadError::Invalid(format!("invalid private key: {e}")))?;

    // Parsing the certificate validates it is a well-formed X.509 cert.
    // Bind the `Pem` so the borrowed `X509Certificate` does not dangle.
    let (_, pem) = x509_parser::pem::parse_x509_pem(cert_str.as_bytes())
        .map_err(|e| MigrationTlsLoadError::Invalid(format!("invalid certificate: {e}")))?;
    let cert = pem
        .parse_x509()
        .map_err(|e| MigrationTlsLoadError::Invalid(format!("invalid certificate: {e}")))?;

    // Canonical SPKI comparison: both sides go through rcgen's
    // `SubjectPublicKeyInfo` (normalized key encoding), avoiding any
    // platform-specific raw-key representation differences.
    let cert_spki = rcgen::SubjectPublicKeyInfo::from_der(cert.public_key().raw)
        .map_err(|e| MigrationTlsLoadError::Invalid(format!("invalid certificate: {e}")))?;
    let key_spki = rcgen::SubjectPublicKeyInfo::from_pem(&key.public_key_pem())
        .map_err(|e| MigrationTlsLoadError::Invalid(format!("invalid private key: {e}")))?;

    if cert_spki != key_spki {
        return Err(MigrationTlsLoadError::Invalid(
            "certificate does not match private key".into(),
        ));
    }

    Ok(())
}

/// Validate that `ca_pem` contains at least one parseable X.509 certificate.
fn validate_ca_bundle(ca_pem: &[u8]) -> Result<(), MigrationTlsLoadError> {
    let mut rest: &[u8] = ca_pem;
    let mut count = 0u32;
    loop {
        let trimmed = trim_ascii_whitespace(rest);
        if trimmed.is_empty() {
            break;
        }
        let (remaining, pem) = x509_parser::pem::parse_x509_pem(trimmed)
            .map_err(|e| MigrationTlsLoadError::Invalid(format!("invalid CA bundle: {e}")))?;
        pem.parse_x509()
            .map_err(|e| MigrationTlsLoadError::Invalid(format!("invalid CA bundle: {e}")))?;
        count += 1;
        rest = remaining;
    }
    if count == 0 {
        return Err(MigrationTlsLoadError::Invalid(
            "CA bundle contains no certificates".into(),
        ));
    }
    Ok(())
}

fn trim_ascii_whitespace(mut b: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = b {
        if first.is_ascii_whitespace() {
            b = rest;
        } else {
            break;
        }
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal `tracing` subscriber that records the message text of
    /// INFO- and WARN-level events, so tests can assert which startup log
    /// line the loaders emitted (round-2 review of #401). Installed
    /// per-thread with `tracing::subscriber::set_default` — the same
    /// convention as `chv-controlplane-store`'s credential key-source log
    /// capture (#336).
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

    /// Generate a self-signed leaf cert for `cn` plus its key, as a matching pair.
    fn matching_pair(cn: &str) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        use rcgen::string::Ia5String;
        use rcgen::{CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, SanType};

        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, cn);
        let dns = Ia5String::try_from(cn.to_string()).unwrap();
        params.subject_alt_names.push(SanType::DnsName(dns));
        params.is_ca = IsCa::NoCa;
        let cert = params.self_signed(&key).unwrap();
        let ca = crate::migration::tests::test_ca_pem();

        (
            cert.pem().into_bytes(),
            key.serialize_pem().into_bytes(),
            ca,
        )
    }

    fn mismatched_pair(cn: &str) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        use rcgen::{CertificateParams, DistinguishedName, DnType, IsCa, KeyPair};

        let (_, _, ca) = matching_pair("unused");
        // Cert signed by one key, but we attach a *different* key.
        let cert_key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, cn);
        params.is_ca = IsCa::NoCa;
        let cert = params.self_signed(&cert_key).unwrap();
        let wrong_key = KeyPair::generate().unwrap();
        (
            cert.pem().into_bytes(),
            wrong_key.serialize_pem().into_bytes(),
            ca,
        )
    }

    fn tmp_file(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut f, bytes).unwrap();
        f
    }

    /// Write cert/key/CA to temp files. Returns the temp files (kept alive so
    /// their paths remain readable for the test's duration) plus their paths.
    #[allow(clippy::type_complexity)]
    fn write_all(
        cert: &[u8],
        key: &[u8],
        ca: &[u8],
    ) -> (
        (
            tempfile::NamedTempFile,
            tempfile::NamedTempFile,
            tempfile::NamedTempFile,
        ),
        (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf),
    ) {
        let c = tmp_file(cert);
        let k = tmp_file(key);
        let a = tmp_file(ca);
        let paths = (
            c.path().to_path_buf(),
            k.path().to_path_buf(),
            a.path().to_path_buf(),
        );
        ((c, k, a), paths)
    }

    #[test]
    fn disabled_returns_none_without_any_inputs() {
        match load_migration_tls(false, None, None, None, None).expect("no error when disabled") {
            None => {}
            Some(_) => panic!("migration disabled must not produce a TLS config"),
        }
    }

    // -----------------------------------------------------------------
    // Client half enabled gating (issue #395): symmetric with the server
    // half — client fields set while migration is disabled must be a
    // startup error, not a silently ignored half-configuration.
    // -----------------------------------------------------------------

    #[test]
    fn disabled_with_client_fields_is_an_error() {
        let (c, k, a) = matching_pair("node-a");
        let ((_c, _k, _a), (cp, kp, ap)) = write_all(&c, &k, &a);
        match load_migration_tls(false, Some(&cp), Some(&kp), Some(&ap), Some("stord-peer")) {
            Err(MigrationTlsLoadError::Invalid(msg)) => {
                assert!(
                    msg.contains("migration.enabled = false"),
                    "error must name the contradiction: {msg}"
                );
                assert!(
                    msg.contains("client_cert_path"),
                    "error must name the client fields: {msg}"
                );
            }
            Ok(_) => panic!("disabled migration with client fields must not load"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn disabled_with_only_dest_server_name_is_an_error() {
        match load_migration_tls(false, None, None, None, Some("stord-peer")) {
            Err(MigrationTlsLoadError::Invalid(msg)) => {
                assert!(
                    msg.contains("dest_server_name"),
                    "error must name the client fields: {msg}"
                );
            }
            Ok(_) => panic!("disabled migration with dest_server_name must not load"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn disabled_with_only_ca_bundle_is_an_error() {
        let (c, k, a) = matching_pair("node-a");
        let ((_c, _k, _a), (_, _, ap)) = write_all(&c, &k, &a);
        match load_migration_tls(false, None, None, Some(&ap), None) {
            Err(MigrationTlsLoadError::Invalid(msg)) => {
                assert!(
                    msg.contains("ca_cert_path"),
                    "error must name the client fields: {msg}"
                );
            }
            Ok(_) => panic!("disabled migration with a CA bundle must not load"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Client half independently optional under enabled = true (issue #401):
    // no client fields = destination-only stord (Ok(None)); any client
    // field set = all four required (all-or-nothing within the half).
    // -----------------------------------------------------------------

    #[test]
    fn enabled_with_no_client_fields_returns_none() {
        // Destination-only stord: `enabled = true` with no client identity
        // must load as Ok(None) (previously a startup error — issue #401).
        match load_migration_tls(true, None, None, None, None).expect("no error when enabled") {
            None => {}
            Some(_) => panic!("unconfigured client half must not produce a TLS config"),
        }
    }

    #[test]
    fn enabled_with_only_client_cert_path_fails() {
        match load_migration_tls(true, Some(Path::new("/tmp/unused.crt")), None, None, None) {
            Err(MigrationTlsLoadError::Missing(msg)) => {
                assert!(
                    msg.contains("client_key_path"),
                    "error must name the missing field: {msg}"
                );
            }
            Ok(_) => panic!("partial client half must not load"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn enabled_with_only_client_key_path_fails() {
        match load_migration_tls(true, None, Some(Path::new("/tmp/unused.key")), None, None) {
            Err(MigrationTlsLoadError::Missing(msg)) => {
                assert!(
                    msg.contains("client_cert_path"),
                    "error must name the missing field: {msg}"
                );
            }
            Ok(_) => panic!("partial client half must not load"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn enabled_with_only_ca_bundle_fails() {
        match load_migration_tls(true, None, None, Some(Path::new("/tmp/unused.ca")), None) {
            Err(MigrationTlsLoadError::Missing(msg)) => {
                assert!(
                    msg.contains("client_cert_path"),
                    "error must name the missing field: {msg}"
                );
            }
            Ok(_) => panic!("partial client half must not load"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn enabled_with_only_dest_server_name_fails() {
        match load_migration_tls(true, None, None, None, Some("stord-peer")) {
            Err(MigrationTlsLoadError::Missing(msg)) => {
                assert!(
                    msg.contains("client_cert_path"),
                    "error must name the missing field: {msg}"
                );
            }
            Ok(_) => panic!("partial client half must not load"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Empty-string fields (round-2 review of #401): an empty value is NOT
    // the same as an absent key. `Some("")` counts as "set" everywhere
    // (absent ≠ empty is existing, intentional semantics — changing it
    // would be a behavior change beyond this PR's scope), so these tests
    // pin what each "" row does today.
    // -----------------------------------------------------------------

    #[test]
    fn disabled_with_empty_string_client_field_is_an_error() {
        // (a) `enabled = false` + any field = "": the field counts as set,
        // so the #395 contradiction fires — "" does not silently disable.
        match load_migration_tls(false, Some(Path::new("")), None, None, None) {
            Err(MigrationTlsLoadError::Invalid(msg)) => {
                assert!(
                    msg.contains("migration.enabled = false"),
                    "empty string must count as set: {msg}"
                );
            }
            Ok(_) => panic!("disabled migration with an empty-string field must not load"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
        // Same for a non-path field.
        match load_migration_tls(false, None, None, None, Some("")) {
            Err(MigrationTlsLoadError::Invalid(msg)) => {
                assert!(
                    msg.contains("migration.enabled = false"),
                    "empty dest_server_name must count as set: {msg}"
                );
            }
            Ok(_) => panic!("disabled migration with an empty dest_server_name must not load"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn enabled_with_empty_client_cert_path_fails_naming_the_field() {
        // (b) `enabled = true` with the other three client fields valid and
        // `client_cert_path = ""`: the loader reaches the file read and
        // fails on the empty path. The diagnostic must name the config key
        // and say the path is empty — a bare "cannot read migration TLS
        // file  : No such file or directory" renders the empty path
        // invisibly and hides which key was set to "".
        let (c, k, a) = matching_pair("node-a");
        let ((_c, _k, _a), (_, kp, ap)) = write_all(&c, &k, &a);
        match load_migration_tls(
            true,
            Some(Path::new("")),
            Some(&kp),
            Some(&ap),
            Some("stord-peer"),
        ) {
            Err(
                ref err @ MigrationTlsLoadError::Unreadable {
                    field, ref path, ..
                },
            ) => {
                assert_eq!(
                    field, "migration.client_cert_path",
                    "error must name the config key the empty path came from"
                );
                assert!(path.as_os_str().is_empty());
                let msg = err.to_string();
                assert!(
                    msg.contains("migration.client_cert_path"),
                    "message must name the config key: {msg}"
                );
                assert!(
                    msg.contains("path is empty"),
                    "message must say the path is empty: {msg}"
                );
            }
            Ok(_) => panic!("an empty client_cert_path must not load"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn enabled_with_empty_dest_server_name_alone_fails_missing_client_cert() {
        // (c) `enabled = true` + `dest_server_name = ""` alone: the field
        // counts as set, so the client half is partial and the
        // all-or-nothing rule demands the other three.
        match load_migration_tls(true, None, None, None, Some("")) {
            Err(MigrationTlsLoadError::Missing(msg)) => {
                assert!(
                    msg.contains("client_cert_path"),
                    "error must name the missing field: {msg}"
                );
            }
            Ok(_) => panic!("an empty dest_server_name alone must not load"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn destination_only_with_stray_empty_dest_server_name_errors() {
        // (d) The "cleared a field by emptying it" trap: a destination-only
        // stord (valid receiver half) whose operator emptied
        // `dest_server_name` instead of deleting the line. The empty string
        // counts as a set client field, so the client half is partial and
        // the daemon fails at startup with Missing(client_cert_path) — it
        // does NOT silently become destination-only. Pin the message so
        // the operator can find the stray key.
        let (c, k, a) = matching_pair("node-a");
        let ((_c, _k, _a), (cp, kp, ap)) = write_all(&c, &k, &a);
        let receiver = load_server(Some("127.0.0.1:50052"), Some(&cp), Some(&kp), Some(&ap))
            .expect("valid receiver half must load");
        assert!(receiver.is_some(), "the receiver half by itself is valid");
        match load_migration_tls(true, None, None, None, Some("")) {
            Err(MigrationTlsLoadError::Missing(msg)) => {
                assert!(
                    msg.contains("client_cert_path"),
                    "error must name the missing field: {msg}"
                );
            }
            Ok(_) => panic!("a stray empty dest_server_name must not be ignored"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn enabled_with_empty_dest_server_name_and_valid_others_fails() {
        // (e) `enabled = true` + `dest_server_name = ""` with the other
        // three client fields valid: the empty value is filtered out and
        // then reported missing (existing filtered-empty behavior, pinned
        // here so a future semantics change is a deliberate act).
        let (c, k, a) = matching_pair("node-a");
        let ((_c, _k, _a), (cp, kp, ap)) = write_all(&c, &k, &a);
        match load_migration_tls(true, Some(&cp), Some(&kp), Some(&ap), Some("")) {
            Err(MigrationTlsLoadError::Missing(msg)) => {
                assert!(
                    msg.contains("dest_server_name"),
                    "error must name the missing field: {msg}"
                );
            }
            Ok(_) => panic!("an empty dest_server_name must not load"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn server_with_empty_listen_addr_fails() {
        // (f) Receiver side, `enabled = true` + `listen_addr = ""`: with
        // the other receiver fields valid the empty address fails to parse
        // (the message renders the empty value explicitly); alone, it
        // counts as a set field and the all-or-nothing rule demands the
        // rest.
        let (c, k, a) = matching_pair("node-a");
        let ((_c, _k, _a), (cp, kp, ap)) = write_all(&c, &k, &a);
        match load_server(Some(""), Some(&cp), Some(&kp), Some(&ap)) {
            Err(MigrationTlsLoadError::Invalid(msg)) => {
                assert!(
                    msg.contains("listen_addr") && msg.contains("not a valid socket address"),
                    "unexpected message: {msg}"
                );
                assert!(
                    msg.contains("\"\""),
                    "message must render the empty address explicitly: {msg}"
                );
            }
            Ok(_) => panic!("an empty listen_addr must not load"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
        match load_server(Some(""), None, None, None) {
            Err(MigrationTlsLoadError::Missing(msg)) => {
                assert!(
                    msg.contains("server_cert_path"),
                    "unexpected message: {msg}"
                );
            }
            Ok(_) => panic!("an empty listen_addr alone must not load"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Startup log lines (round-2 review of #401): the destination-only
    // path must emit its info! line, and the non-destination-only paths
    // must not emit it.
    // -----------------------------------------------------------------

    #[test]
    fn destination_only_path_logs_client_identity_absent() {
        let logs = log_capture::LogCollector::default();
        let _subscriber = tracing::subscriber::set_default(logs.clone());
        let result = load_migration_tls(true, None, None, None, None).expect("must load");
        assert!(result.is_none());
        assert!(
            logs.messages_at(tracing::Level::INFO)
                .iter()
                .any(|m| m.contains("client identity not configured")),
            "destination-only startup must log the absent-client-identity info line"
        );
    }

    #[test]
    fn source_only_path_does_not_log_destination_only_line() {
        let logs = log_capture::LogCollector::default();
        let _subscriber = tracing::subscriber::set_default(logs.clone());
        let (c, k, a) = matching_pair("node-a");
        let ((_c, _k, _a), (cp, kp, ap)) = write_all(&c, &k, &a);
        let result = load_migration_tls(true, Some(&cp), Some(&kp), Some(&ap), Some("stord-peer"))
            .expect("valid client identity must load");
        assert!(result.is_some());
        assert!(
            !logs
                .messages_at(tracing::Level::INFO)
                .iter()
                .any(|m| m.contains("client identity not configured")),
            "source-only startup must not log the destination-only info line"
        );
    }

    #[test]
    fn disabled_path_does_not_log_destination_only_line() {
        let logs = log_capture::LogCollector::default();
        let _subscriber = tracing::subscriber::set_default(logs.clone());
        load_migration_tls(false, None, None, None, None).expect("must load");
        assert!(
            !logs
                .messages_at(tracing::Level::INFO)
                .iter()
                .any(|m| m.contains("client identity not configured")),
            "disabled startup must not log the destination-only info line"
        );
    }

    #[test]
    fn enabled_missing_dest_server_name_fails() {
        let ((_c, _k, _a), (cp, kp, ap)) = {
            let (c, k, a) = matching_pair("node-a");
            write_all(&c, &k, &a)
        };
        match load_migration_tls(true, Some(&cp), Some(&kp), Some(&ap), None) {
            Err(MigrationTlsLoadError::Missing(_)) => {}
            Ok(_) => panic!("expected a Missing error, but a TLS config was produced"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn enabled_valid_pair_loads_config_without_plaintext() {
        let (c, k, a) = matching_pair("node-a");
        let ((_c, _k, _a), (cp, kp, ap)) = write_all(&c, &k, &a);
        let opt = load_migration_tls(true, Some(&cp), Some(&kp), Some(&ap), Some("stord-peer"))
            .expect("valid pair should load");
        let cfg = match opt {
            Some(cfg) => cfg,
            None => panic!("enabled migration must produce a TLS config"),
        };
        assert_eq!(cfg.dest_domain, "stord-peer");
        assert!(cfg.cert_pem == c, "cert PEM must be preserved");
        assert!(cfg.key_pem == k, "key PEM must be preserved");
        assert!(cfg.ca_pem == a, "CA PEM must be preserved");
    }

    /// x509-parser 0.18 behavior lock: the PEM reader ignores lines that are
    /// not valid UTF-8 in the comment section before BEGIN (some provisioning
    /// tools emit them). A certificate PEM carrying such a comment must still
    /// load and validate.
    #[test]
    fn enabled_cert_pem_with_non_utf8_comment_loads() {
        let (cert, key, ca) = matching_pair("node-a");
        let mut commented = b"# provisioning note: \xFF\xFE\xf0\x28\x8c\x28\n".to_vec();
        commented.extend_from_slice(&cert);
        let ((_c, _k, _a), (cp, kp, ap)) = write_all(&commented, &key, &ca);
        load_migration_tls(true, Some(&cp), Some(&kp), Some(&ap), Some("stord-peer"))
            .expect("PEM with a non-UTF-8 comment line must load");
    }

    #[test]
    fn enabled_mismatched_keypair_fails() {
        let (c, k, a) = mismatched_pair("node-a");
        let ((_c, _k, _a), (cp, kp, ap)) = write_all(&c, &k, &a);
        match load_migration_tls(true, Some(&cp), Some(&kp), Some(&ap), Some("stord-peer")) {
            Err(MigrationTlsLoadError::Invalid(msg)) => {
                assert!(
                    msg.contains("does not match"),
                    "expected mismatch message, got: {msg}"
                );
            }
            Ok(_) => panic!("expected an Invalid error, but a TLS config was produced"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn enabled_unreadable_file_fails() {
        let (c, k, a) = matching_pair("node-a");
        let ((_c, _k, _a), (_, kp, ap)) = write_all(&c, &k, &a);
        let missing = std::path::PathBuf::from("/nonexistent/chv-migration-cert.pem");
        match load_migration_tls(
            true,
            Some(&missing),
            Some(&kp),
            Some(&ap),
            Some("stord-peer"),
        ) {
            Err(MigrationTlsLoadError::Unreadable { path, .. }) => {
                assert_eq!(path, missing);
            }
            Ok(_) => panic!("expected an Unreadable error, but a TLS config was produced"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn enabled_garbage_pem_rejected() {
        let (c, k, a) = matching_pair("node-a");
        let ((_c, _k, _a), (_, kp, ap)) = write_all(&c, &k, &a);
        let bad = tmp_file(b"not a pem at all\n");
        match load_migration_tls(
            true,
            Some(bad.path()),
            Some(&kp),
            Some(&ap),
            Some("stord-peer"),
        ) {
            Err(MigrationTlsLoadError::Invalid(_)) => {}
            Ok(_) => panic!("expected an Invalid error, but a TLS config was produced"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Server half (migration receiver listener, issue #390)
    // -----------------------------------------------------------------

    fn load_server(
        addr: Option<&str>,
        cert: Option<&std::path::Path>,
        key: Option<&std::path::Path>,
        ca: Option<&std::path::Path>,
    ) -> Result<Option<MigrationServerTls>, MigrationTlsLoadError> {
        load_migration_server_tls(true, addr, cert, key, ca)
    }

    #[test]
    fn server_no_fields_returns_none() {
        match load_server(None, None, None, None).expect("no error when unset") {
            None => {}
            Some(_) => panic!("unconfigured receiver must not produce server TLS material"),
        }
    }

    #[test]
    fn server_disabled_with_no_fields_returns_none() {
        match load_migration_server_tls(false, None, None, None, None).expect("no error when unset")
        {
            None => {}
            Some(_) => panic!("disabled + unconfigured receiver must not produce material"),
        }
    }

    #[test]
    fn server_disabled_with_fields_is_an_error() {
        // `migration.enabled = false` must not silently ignore receiver
        // fields (an operator who believes migration is off must not get an
        // inbound TCP listener), nor silently open one.
        match load_migration_server_tls(false, Some("127.0.0.1:50052"), None, None, None) {
            Err(MigrationTlsLoadError::Invalid(msg)) => {
                assert!(
                    msg.contains("migration.enabled = false"),
                    "error must name the contradiction: {msg}"
                );
            }
            other => panic!("expected Invalid, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn server_only_listen_addr_fails() {
        match load_server(Some("127.0.0.1:50052"), None, None, None) {
            Err(MigrationTlsLoadError::Missing(msg)) => {
                assert!(
                    msg.contains("server_cert_path"),
                    "unexpected message: {msg}"
                );
            }
            Ok(_) => panic!("partial receiver config must not load"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn server_missing_listen_addr_fails() {
        let (c, k, a) = matching_pair("node-a");
        let ((_c, _k, _a), (cp, kp, ap)) = write_all(&c, &k, &a);
        match load_server(None, Some(&cp), Some(&kp), Some(&ap)) {
            Err(MigrationTlsLoadError::Missing(msg)) => {
                assert!(msg.contains("listen_addr"), "unexpected message: {msg}");
            }
            Ok(_) => panic!("partial receiver config must not load"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn server_missing_client_ca_fails() {
        let (c, k, _a) = matching_pair("node-a");
        let ((_c, _k, _a), (cp, kp, _)) = write_all(&c, &k, &[]);
        match load_server(Some("127.0.0.1:50052"), Some(&cp), Some(&kp), None) {
            Err(MigrationTlsLoadError::Missing(msg)) => {
                assert!(msg.contains("client_ca_path"), "unexpected message: {msg}");
            }
            Ok(_) => panic!("partial receiver config must not load"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn server_missing_server_key_fails() {
        // Receiver cert without key: the receiver half is all-or-nothing.
        let (c, k, a) = matching_pair("node-a");
        let ((_c, _k, _a), (cp, _, ap)) = write_all(&c, &k, &a);
        match load_server(Some("127.0.0.1:50052"), Some(&cp), None, Some(&ap)) {
            Err(MigrationTlsLoadError::Missing(msg)) => {
                assert!(msg.contains("server_key_path"), "unexpected message: {msg}");
            }
            Ok(_) => panic!("partial receiver config must not load"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn server_invalid_listen_addr_fails() {
        let (c, k, a) = matching_pair("node-a");
        let ((_c, _k, _a), (cp, kp, ap)) = write_all(&c, &k, &a);
        match load_server(
            Some("not a socket address"),
            Some(&cp),
            Some(&kp),
            Some(&ap),
        ) {
            Err(MigrationTlsLoadError::Invalid(msg)) => {
                assert!(msg.contains("listen_addr"), "unexpected message: {msg}");
            }
            Ok(_) => panic!("invalid listen_addr must not load"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn server_unreadable_file_fails() {
        let (c, k, a) = matching_pair("node-a");
        let ((_c, _k, _a), (_, kp, ap)) = write_all(&c, &k, &a);
        let missing = std::path::PathBuf::from("/nonexistent/chv-migration-server-cert.pem");
        match load_server(
            Some("127.0.0.1:50052"),
            Some(&missing),
            Some(&kp),
            Some(&ap),
        ) {
            Err(MigrationTlsLoadError::Unreadable { path, .. }) => {
                assert_eq!(path, missing);
            }
            Ok(_) => panic!("expected an Unreadable error, but server TLS material was produced"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn server_mismatched_keypair_fails() {
        let (c, k, a) = mismatched_pair("node-a");
        let ((_c, _k, _a), (cp, kp, ap)) = write_all(&c, &k, &a);
        match load_server(Some("127.0.0.1:50052"), Some(&cp), Some(&kp), Some(&ap)) {
            Err(MigrationTlsLoadError::Invalid(msg)) => {
                assert!(
                    msg.contains("does not match"),
                    "expected mismatch message, got: {msg}"
                );
            }
            Ok(_) => panic!("expected an Invalid error, but server TLS material was produced"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn server_garbage_client_ca_fails() {
        let (c, k, _a) = matching_pair("node-a");
        let bad_ca = tmp_file(b"garbage not a pem\n");
        let ((_c, _k, _a), (cp, kp, _)) = write_all(&c, &k, &[]);
        match load_server(
            Some("127.0.0.1:50052"),
            Some(&cp),
            Some(&kp),
            Some(bad_ca.path()),
        ) {
            Err(MigrationTlsLoadError::Invalid(msg)) => {
                assert!(msg.contains("CA bundle"), "unexpected message: {msg}");
            }
            Ok(_) => panic!("expected an Invalid error, but server TLS material was produced"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn server_empty_client_ca_fails() {
        let (c, k, _a) = matching_pair("node-a");
        let empty_ca = tmp_file(b"");
        let ((_c, _k, _a), (cp, kp, _)) = write_all(&c, &k, &[]);
        match load_server(
            Some("127.0.0.1:50052"),
            Some(&cp),
            Some(&kp),
            Some(empty_ca.path()),
        ) {
            Err(MigrationTlsLoadError::Invalid(msg)) => {
                assert!(msg.contains("no certificates"), "unexpected message: {msg}");
            }
            Ok(_) => panic!("expected an Invalid error, but server TLS material was produced"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn server_valid_full_set_loads() {
        let (c, k, a) = matching_pair("node-a");
        let ((_c, _k, _a), (cp, kp, ap)) = write_all(&c, &k, &a);
        let mat = load_server(Some("127.0.0.1:50052"), Some(&cp), Some(&kp), Some(&ap))
            .expect("valid receiver material should load")
            .expect("configured receiver must produce server TLS material");
        assert_eq!(
            mat.listen_addr,
            "127.0.0.1:50052".parse::<std::net::SocketAddr>().unwrap()
        );
        assert!(mat.cert_pem == c, "server cert PEM must be preserved");
        assert!(mat.key_pem == k, "server key PEM must be preserved");
        assert!(mat.client_ca_pem == a, "client CA PEM must be preserved");
    }

    // -----------------------------------------------------------------
    // Cross-half gating (issue #401): the two halves are independently
    // optional under enabled = true, but enabled = true with NEITHER half
    // configured is a startup error (misconfiguration).
    // -----------------------------------------------------------------

    #[test]
    fn enabled_with_neither_half_configured_is_an_error() {
        match ensure_migration_half_configured(true, None, None) {
            Err(MigrationTlsLoadError::Invalid(msg)) => {
                assert!(
                    msg.contains("migration.enabled = true"),
                    "error must name the contradiction: {msg}"
                );
                assert!(
                    msg.contains("client_cert_path"),
                    "error must name the client fields: {msg}"
                );
                assert!(
                    msg.contains("listen_addr"),
                    "error must name the receiver fields: {msg}"
                );
            }
            Ok(()) => panic!("enabled migration with neither half must not pass"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn enabled_destination_only_loads() {
        // The shape issue #401 makes expressible: `enabled = true` with only
        // the receiver half configured. The client loader must yield None
        // (no client identity required), the server loader must yield the
        // receiver material, and the cross-half check must pass.
        let (c, k, a) = matching_pair("node-a");
        let ((_c, _k, _a), (cp, kp, ap)) = write_all(&c, &k, &a);

        let client_tls = load_migration_tls(true, None, None, None, None)
            .expect("destination-only client half must load");
        assert!(
            client_tls.is_none(),
            "destination-only stord must not carry a client identity"
        );

        let server_tls = load_server(Some("127.0.0.1:50052"), Some(&cp), Some(&kp), Some(&ap))
            .expect("valid receiver material should load");
        assert!(server_tls.is_some(), "receiver half must be configured");

        ensure_migration_half_configured(true, client_tls.as_ref(), server_tls.as_ref())
            .expect("destination-only stord must pass the cross-half check");
    }

    #[test]
    fn enabled_source_only_passes_half_check() {
        // Source-only stord (works today, must keep working): client
        // identity configured, no receiver fields.
        let (c, k, a) = matching_pair("node-a");
        let ((_c, _k, _a), (cp, kp, ap)) = write_all(&c, &k, &a);
        let client_tls =
            load_migration_tls(true, Some(&cp), Some(&kp), Some(&ap), Some("stord-peer"))
                .expect("valid client identity should load");
        let server_tls = load_server(None, None, None, None).expect("no error when unset");
        assert!(server_tls.is_none(), "unconfigured receiver must be None");
        ensure_migration_half_configured(true, client_tls.as_ref(), server_tls.as_ref())
            .expect("source-only stord must pass the cross-half check");
    }

    #[test]
    fn enabled_both_halves_pass_half_check() {
        let (c, k, a) = matching_pair("node-a");
        let ((_c, _k, _a), (cp, kp, ap)) = write_all(&c, &k, &a);
        let client_tls =
            load_migration_tls(true, Some(&cp), Some(&kp), Some(&ap), Some("stord-peer"))
                .expect("valid client identity should load");
        let server_tls = load_server(Some("127.0.0.1:50052"), Some(&cp), Some(&kp), Some(&ap))
            .expect("valid receiver material should load");
        ensure_migration_half_configured(true, client_tls.as_ref(), server_tls.as_ref())
            .expect("both halves configured must pass the cross-half check");
    }

    #[test]
    fn disabled_passes_half_check_without_any_halves() {
        ensure_migration_half_configured(false, None, None)
            .expect("disabled migration must pass the cross-half check");
    }
}
