use chv_config::{load_stord_config, resolve_migration_signing_secret, StordMigrationConfig};
use chv_observability::init_logger;
use chv_observability::metrics_server::{
    resolve_metrics_bind, start_metrics_server, DaemonMetricsInfo,
};
use chv_stord_backends::{
    CephRbdBackend, IscsiBackend, LVMBackend, LocalFileBackend, StorageBackend,
};
use chv_stord_core::migration::auth::{MigrationLimits, MigrationReceiverAuthorization};
use chv_stord_core::migration::tls_config::{
    ensure_migration_half_configured, load_migration_server_tls, load_migration_tls,
};
use chv_stord_core::store::SessionStore;
use chv_stord_core::StorageServer;
use std::path::PathBuf;
use tokio::signal::unix::{signal, SignalKind};
use tracing::info;

/// Default `/metrics` listener when `metrics_bind` is unset. Loopback, never
/// `0.0.0.0`: an omitted key must not expose storage telemetry on every
/// interface. Operators scraping remotely set `metrics_bind` explicitly and
/// restrict access at the network layer (issue #657).
const DEFAULT_METRICS_BIND: &str = "127.0.0.1:9101";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Capture the true process start before any work so the metrics listener
    // reports a truthful uptime / `process_start_time_seconds` (issue #657).
    let process_started = std::time::Instant::now();

    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!(
            "{} {} (commit {}, build {}, channel {})",
            env!("CARGO_PKG_NAME"),
            env!("CHV_VERSION"),
            env!("CHV_GIT_SHA"),
            env!("CHV_BUILD_DATE"),
            env!("CHV_RELEASE_CHANNEL"),
        );
        return Ok(());
    }

    // Install the rustls ring crypto provider. Required before any tonic/rustls
    // TLS connection can be established — in particular the mTLS storage
    // migration sender (issue #232).
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls ring crypto provider");

    let config_path = std::env::args().nth(1).map(PathBuf::from);
    let config = load_stord_config(config_path.as_deref())?;

    init_logger(&config.log_level)?;

    info!(
        "{} starting (version {}, commit {}, channel {})",
        env!("CARGO_PKG_NAME"),
        env!("CHV_VERSION"),
        env!("CHV_GIT_SHA"),
        env!("CHV_RELEASE_CHANNEL"),
    );

    // Bind the Prometheus listener before opening the session store or any
    // backend: a bad `metrics_bind` or an occupied port aborts startup loudly
    // instead of running a daemon with no observability surface (#657).
    start_metrics_endpoint(&config, process_started).await?;

    let db_path = config.runtime_dir.join("stord.db");
    let store = SessionStore::new(&db_path)?;

    // Select backend based on configuration.
    //
    // #379 DP2 fail-closed guards (design §5.1 decision 2), extracted
    // into [`validate_backend_type`] / [`verify_volume_group`] below:
    //
    // - A PRESENT but unrecognized `backend_type` ABORTS at startup. The
    //   pre-#379 fallback arm silently served local-file for a typo'd
    //   value — a fail-open footgun on exactly the path this issue
    //   enables, because the agent now reports the config's class
    //   verbatim as the node's advertised storage class (DP4), so a
    //   typo'd value would be advertised as a class nothing serves.
    //   An ABSENT key still means local (B1) — unchanged.
    // - `backend_type = "lvm"` verifies the volume group exists (`vgs`)
    //   before serving: every LVM open would otherwise fail at runtime
    //   with an opaque `lvcreate` error, and the DP2 create-on-open
    //   provisioning path would strand half-created VMs. The operator
    //   pre-provisions the VG (see docs/OPERATIONS.md, "LVM nodes").
    validate_backend_type(config.backend_type.as_deref())?;
    let backend: Box<dyn StorageBackend> = match config.backend_type.as_deref().unwrap_or("local") {
        "iscsi" => {
            let iscsi_cfg = config
                .iscsi
                .as_ref()
                .ok_or("backend_type is 'iscsi' but [iscsi] config section is missing")?;
            let backend_cfg = chv_stord_backends::iscsi::IscsiConfig {
                portal: iscsi_cfg.portal.clone(),
                target_iqn: iscsi_cfg.target_iqn.clone(),
                initiator_name: iscsi_cfg.initiator_name.clone(),
                chap_username: iscsi_cfg.chap_username.clone(),
                chap_secret: iscsi_cfg.chap_secret.clone(),
            };
            Box::new(IscsiBackend::new(backend_cfg)?)
        }
        "ceph" => {
            let ceph_cfg = config
                .ceph
                .as_ref()
                .ok_or("backend_type is 'ceph' but [ceph] config section is missing")?;
            let backend_cfg = chv_stord_backends::ceph::CephRbdConfig {
                cluster_name: ceph_cfg.cluster_name.clone(),
                pool_name: ceph_cfg.pool_name.clone(),
                user: ceph_cfg.user.clone(),
                keyring_path: ceph_cfg.keyring_path.clone(),
                monitors: ceph_cfg.monitors.clone(),
            };
            Box::new(CephRbdBackend::new(backend_cfg)?)
        }
        "lvm" => {
            let vg_name = config.lvm_volume_group.as_deref().unwrap_or("chv-vg");
            // Fail closed on a missing VG: refuse to start instead of
            // serving a backend whose every open fails at runtime.
            verify_volume_group(vg_name).await?;
            Box::new(LVMBackend::new(vg_name.to_string())?)
        }
        // Only None/"local" reach here: validate_backend_type above has
        // already aborted on any other present value.
        _ => Box::new(LocalFileBackend::new(config.runtime_dir.clone())),
    };

    info!(
        backend_type = config.backend_type.as_deref().unwrap_or("local"),
        "storage backend initialized"
    );

    // Load + validate the storage-migration mTLS identity at startup (issue #232).
    // Fail-closed: a partially configured client half, unreadable material or
    // a mismatched keypair is a startup error (never a runtime downgrade).
    // `migration.enabled = false` (default) starts without credentials and
    // migration actions fail with a failed_precondition error in the sender;
    // the wiring logs a single `storage migration is disabled` confirmation
    // line at startup (see `load_migration_materials`).
    // Under `enabled = true` the client half is independently optional
    // (issue #401): no client fields = destination-only stord that never
    // initiates migrations (outbound migration actions fail with the same
    // failed_precondition error).
    //
    // Server half (issue #390): load + validate the migration receiver's mTLS
    // material. `migration.enabled` is the master switch here too: receiver
    // fields with enabled = false are a startup error (an operator who
    // believes migration is off must not get an inbound TCP listener), and
    // the four receiver fields are all-or-nothing — a partially configured
    // receiver, unreadable files, a mismatched keypair, an invalid/empty
    // client CA bundle, or a bad listen address is a startup error
    // (fail-closed). No receiver fields = source-only stord: no TCP listener
    // is opened. Client-certificate authentication is mandatory on the
    // listener; there is no plaintext fallback. The "listening" log line is
    // emitted only after the TCP socket is actually bound (see server.rs).
    //
    // Cross-half check (issue #401): the two halves are independently
    // optional under `enabled = true` (source-only, destination-only, or
    // both), but an enabled migration section that configures NEITHER half
    // is a misconfiguration — the daemon would run with migrations
    // unavailable in both directions while the operator believes migration
    // is on. Fail-closed at startup.
    let (migration_tls, migration_server_tls) = load_migration_materials(&config.migration)?;
    if migration_tls.is_some() {
        info!("storage migration mTLS enabled (credentials validated at startup)");
    }
    if migration_server_tls.is_some() {
        info!(
            "storage migration receiver mTLS material validated (listener binds in server startup)"
        );
    }
    let migration_authorization =
        load_migration_authorization(&config.migration, migration_server_tls.is_some())?;
    let migration_limits = load_migration_limits(&config.migration)?;
    if matches!(
        migration_authorization,
        MigrationReceiverAuthorization::Required { .. }
    ) {
        info!(
            "storage migration authorizations enabled (control-plane HMAC tokens required before \
             receiving-volume creation)"
        );
    }

    let server = StorageServer::new(
        backend,
        config.runtime_dir.clone(),
        chv_observability::Metrics::new(),
        config.backend_allowlist,
        config.path_allowlist,
        config.device_allowlist,
        config.migration_dest_allowlist,
        migration_tls,
        migration_server_tls,
        migration_authorization,
        migration_limits,
        Some(store),
    );

    let socket_path = config.socket_path.clone();
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    tokio::select! {
        result = server.serve(&config.socket_path, Some(&db_path)) => {
            result?;
        }
        _ = sigterm.recv() => {
            info!("received SIGTERM, shutting down");
        }
        _ = sigint.recv() => {
            info!("received SIGINT, shutting down");
        }
    }

    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

/// Build the process identity exposed by the `/metrics` endpoint.
fn daemon_metrics_info(process_started: std::time::Instant) -> DaemonMetricsInfo {
    DaemonMetricsInfo::new(
        env!("CARGO_PKG_NAME"),
        env!("CHV_VERSION"),
        env!("CHV_GIT_SHA"),
        env!("CHV_RELEASE_CHANNEL"),
    )
    .with_started_at(process_started)
}

/// Resolve and bind the `/metrics` listener.
///
/// The bind is synchronous with startup (the listener is acquired before this
/// returns), so `metrics_bind` typos and port conflicts are fatal. The serving
/// task itself is process-lifetime and logs if it ever exits.
async fn start_metrics_endpoint(
    config: &chv_config::StordConfig,
    process_started: std::time::Instant,
) -> Result<(), String> {
    let bind = resolve_metrics_bind(config.metrics_bind.as_deref(), DEFAULT_METRICS_BIND);
    if config.metrics_bind.is_none() {
        tracing::warn!(
            bind = %bind,
            "metrics_bind is not set; binding /metrics on loopback — set metrics_bind \
             explicitly to serve a different address"
        );
    }
    start_metrics_server(&bind, daemon_metrics_info(process_started))
        .await
        .map_err(|e| {
            format!(
                "failed to bind metrics listener on {bind}: {e}; set metrics_bind to a valid \
                 bind address (loopback by default)"
            )
        })
}

/// #379 DP2 fail-closed vocabulary guard (B2): a PRESENT but
/// unrecognized `backend_type` aborts stord at startup. The pre-#379
/// fallback arm silently served local-file for a typo'd value — a
/// fail-open footgun on exactly the path #379 enables, because the
/// agent now reports the config's class verbatim as the node's
/// advertised storage class (DP4), so a typo'd value would be
/// advertised as a class nothing serves. An ABSENT key still means
/// local (B1) — the historical default, unchanged.
fn validate_backend_type(backend_type: Option<&str>) -> Result<(), String> {
    match backend_type {
        // B1: absent key = local.
        None => Ok(()),
        Some(known) if ["local", "iscsi", "ceph", "lvm"].contains(&known) => Ok(()),
        Some(unknown) => Err(format!(
            "backend_type '{unknown}' is not a recognized stord backend (recognized: local, \
             iscsi, ceph, lvm); refusing to start. Remove the key to select the local \
             backend — an absent key still means local (#379 DP2 fail-closed guard)"
        )),
    }
}

/// #379 DP2 fail-closed VG guard: `backend_type = "lvm"` verifies the
/// volume group is visible (`vgs`) before serving. Every LVM open would
/// otherwise fail at runtime with an opaque `lvcreate` error, and the
/// DP2 create-on-open provisioning path would strand half-created VMs;
/// the operator pre-provisions the VG instead (see
/// docs/OPERATIONS.md, "LVM nodes").
async fn verify_volume_group(vg_name: &str) -> Result<(), String> {
    let vgs = tokio::process::Command::new("vgs")
        .args(["--noheadings", "--options", "vg_name", vg_name])
        .output()
        .await
        .map_err(|e| {
            format!(
                "backend_type is 'lvm' but vgs could not be run to verify the volume group: {e}"
            )
        })?;
    if !vgs.status.success() {
        return Err(format!(
            "backend_type is 'lvm' but volume group '{vg_name}' does not exist or is not \
             visible (vgs: {}); pre-provision the VG before starting stord \
             (#379 DP2 fail-closed guard)",
            String::from_utf8_lossy(&vgs.stderr).trim()
        ));
    }
    Ok(())
}

/// Load and validate both migration mTLS halves from the `[migration]`
/// config section, in the daemon's startup order: client (source) half,
/// receiver (destination) half, then the cross-half check (issue #401).
///
/// Extracted from `main` so the wiring sequence — in particular the
/// `ensure_migration_half_configured` call — is unit-testable in this
/// crate: a regression that drops or bypasses a step fails the tests
/// below.
///
/// This function also owns the disabled-migration startup confirmation
/// line: `migration.enabled = false` (the default) logs one info line so
/// a stord that starts without migration capability says so. #483's
/// extraction dropped `main`'s `else` branch for this case; the line now
/// lives here — and only here, the loaders no longer emit their own
/// copies — so it fires exactly once for a disabled config and never for
/// the three enabled shapes (source-only, destination-only, both), whose
/// startup lines come from the loaders and the `is_some` branches in
/// `main`.
fn load_migration_materials(
    migration: &chv_config::StordMigrationConfig,
) -> Result<
    (
        Option<chv_stord_core::migration::sender::MigrationTlsConfig>,
        Option<chv_stord_core::migration::tls_config::MigrationServerTls>,
    ),
    chv_stord_core::migration::tls_config::MigrationTlsLoadError,
> {
    let client_tls = load_migration_tls(
        migration.enabled,
        migration.client_cert_path.as_deref(),
        migration.client_key_path.as_deref(),
        migration.ca_cert_path.as_deref(),
        migration.dest_server_name.as_deref(),
    )?;
    let server_tls = load_migration_server_tls(
        migration.enabled,
        migration.listen_addr.as_deref(),
        migration.server_cert_path.as_deref(),
        migration.server_key_path.as_deref(),
        migration.client_ca_path.as_deref(),
    )?;
    ensure_migration_half_configured(migration.enabled, client_tls.as_ref(), server_tls.as_ref())?;
    // Only reachable for a *clean* disabled section: `enabled = false`
    // with any field set anywhere in the section already failed above.
    if !migration.enabled {
        info!(
            "storage migration is disabled (migration.enabled = false): migration actions will be unavailable"
        );
    }
    Ok((client_tls, server_tls))
}

/// Build the receiver authorization policy (issue #663).
///
/// Fail-closed rules:
/// * A receiver half with no signing secret, or no `node_id` to compare the
///   token's destination claim against, aborts startup. An operator who
///   exposed an inbound listener must not silently get an unauthenticated
///   receiver.
/// * `allow_insecure_unauthenticated = true` is the only bypass and logs a
///   loud warning. It is never the default.
/// * Without a receiver half the service still exists on the local Unix
///   socket; absent auth material it uses `DenyAll` rather than accepting
///   local streams without a token.
fn load_migration_authorization(
    migration: &StordMigrationConfig,
    receiver_enabled: bool,
) -> Result<MigrationReceiverAuthorization, String> {
    if migration.allow_insecure_unauthenticated {
        tracing::warn!(
            "SECURITY: migration.allow_insecure_unauthenticated = true — inbound disk \
             migrations are NOT authorized by the control plane. This is a development-only \
             mode; never run it in production"
        );
        return Ok(MigrationReceiverAuthorization::DisabledInsecure);
    }

    let secret = resolve_migration_signing_secret(
        migration.signing_secret.as_deref(),
        migration.signing_secret_file.as_deref(),
        "CHV_MIGRATION_SIGNING_SECRET",
    )
    .map_err(|e| e.to_string())?;

    match secret {
        Some(secret) => {
            let node_id = migration
                .node_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    "migration.signing_secret is configured but migration.node_id is missing; \
                     stord needs its own node id to reject tokens minted for another destination"
                        .to_string()
                })?;
            MigrationReceiverAuthorization::required(&secret, node_id)
                .map_err(|e| format!("invalid migration authorization configuration: {e}"))
        }
        None if receiver_enabled => Err(
            "migration receiver is enabled but no migration signing secret is configured; \
             set migration.signing_secret_file (preferred) or CHV_MIGRATION_SIGNING_SECRET, \
             or explicitly set migration.allow_insecure_unauthenticated = true for a \
             development-only deployment"
                .to_string(),
        ),
        None => Ok(MigrationReceiverAuthorization::DenyAll),
    }
}

/// Per-node receiving-migration admission limits (issue #663). Zero is a
/// startup error, not "unlimited": an accidental zero must never turn into
/// an unbounded receiver.
fn load_migration_limits(migration: &StordMigrationConfig) -> Result<MigrationLimits, String> {
    let limits = MigrationLimits {
        max_inflight: migration.max_inflight_receiving,
        max_inflight_bytes: migration.max_inflight_receiving_bytes,
    };
    if limits.max_inflight == 0 || limits.max_inflight_bytes == 0 {
        return Err(
            "migration.max_inflight_receiving and migration.max_inflight_receiving_bytes \
             must both be greater than zero"
                .to_string(),
        );
    }
    Ok(limits)
}

#[cfg(test)]
mod tests {
    use super::{
        load_migration_authorization, load_migration_limits, load_migration_materials,
        start_metrics_endpoint, validate_backend_type, verify_volume_group, DEFAULT_METRICS_BIND,
    };
    use chv_config::StordMigrationConfig;
    use chv_stord_core::migration::auth::MigrationReceiverAuthorization;

    /// Minimal `tracing` subscriber that records the message text of
    /// INFO- and WARN-level events, so tests can assert which startup log
    /// line the wiring emitted. Installed per-thread with
    /// `tracing::subscriber::set_default` — the same convention as the
    /// loader tests in `chv-stord-core`'s `tls_config` (round-2 review of
    /// #401) and `chv-controlplane-store`'s credential key-source log
    /// capture (#336). Duplicated here because this is a bin crate.
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

    /// Write a self-signed cert/key pair plus a CA bundle (the cert
    /// itself — the loaders only require the bundle to parse) and return
    /// the material paths. Valid for either half: the receiver fields
    /// (`server_cert_path`, `server_key_path`, `client_ca_path`) and the
    /// client fields (`client_cert_path`, `client_key_path`,
    /// `ca_cert_path`) accept the same shape. `keep` must stay alive for
    /// the paths to remain readable.
    #[allow(clippy::type_complexity)]
    fn self_signed_material() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        use rcgen::{CertificateParams, KeyPair};

        let key = KeyPair::generate().unwrap();
        let cert = CertificateParams::default().self_signed(&key).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("server.crt");
        let key_path = dir.path().join("server.key");
        let ca_path = dir.path().join("client-ca.crt");
        std::fs::write(&cert_path, cert.pem()).unwrap();
        std::fs::write(&key_path, key.serialize_pem()).unwrap();
        std::fs::write(&ca_path, cert.pem()).unwrap();
        (dir, cert_path, key_path, ca_path)
    }

    /// Round-2 review of #401 (daemon-wiring regression test): the wiring
    /// must keep calling `ensure_migration_half_configured` — deleting
    /// that call would let an enabled-but-empty `[migration]` section
    /// start the daemon with migrations off in both directions while the
    /// operator believes migration is on.
    #[test]
    fn enabled_with_neither_half_is_a_startup_error() {
        let config = StordMigrationConfig {
            enabled: true,
            ..Default::default()
        };
        assert!(
            load_migration_materials(&config).is_err(),
            "enabled = true with neither half configured must be a startup error"
        );
    }

    /// The shape issue #401 makes expressible, exercised at the daemon
    /// wiring seam: `enabled = true` + receiver half only → no client
    /// identity, receiver material present.
    #[test]
    fn destination_only_config_loads() {
        let (_keep, cert, key, ca) = self_signed_material();
        let config = StordMigrationConfig {
            enabled: true,
            listen_addr: Some("127.0.0.1:50052".to_string()),
            server_cert_path: Some(cert),
            server_key_path: Some(key),
            client_ca_path: Some(ca),
            ..Default::default()
        };
        let (client, server) =
            load_migration_materials(&config).expect("destination-only config must load");
        assert!(
            client.is_none(),
            "destination-only stord carries no client identity"
        );
        assert!(server.is_some(), "receiver half must be configured");
    }

    /// The wiring must keep passing the config's client fields to the
    /// client loader: a stray field under `enabled = false` is the #395
    /// contradiction, not a silently ignored key.
    #[test]
    fn disabled_with_stray_client_field_is_a_startup_error() {
        let config = StordMigrationConfig {
            enabled: false,
            dest_server_name: Some("stord-peer".to_string()),
            ..Default::default()
        };
        assert!(
            load_migration_materials(&config).is_err(),
            "enabled = false with a stray client field must be a startup error"
        );
    }

    // -----------------------------------------------------------------
    // Startup log lines (second-pass review of #483): the
    // disabled-migration confirmation line that #483's extraction dropped
    // is restored at the wiring seam, fires exactly once (the loaders no
    // longer emit their own copies), and never fires for the enabled
    // shapes.
    // -----------------------------------------------------------------

    #[test]
    fn disabled_config_logs_disabled_startup_line_exactly_once() {
        let logs = log_capture::LogCollector::default();
        let _subscriber = tracing::subscriber::set_default(logs.clone());
        let config = StordMigrationConfig::default();
        let (client, server) =
            load_migration_materials(&config).expect("disabled config must load");
        assert!(client.is_none() && server.is_none());
        let hits: Vec<String> = logs
            .messages_at(tracing::Level::INFO)
            .into_iter()
            .filter(|m| m.contains("storage migration is disabled"))
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "the disabled-migration line must fire exactly once, got: {hits:?}"
        );
    }

    #[test]
    fn source_only_config_does_not_log_disabled_line() {
        let logs = log_capture::LogCollector::default();
        let _subscriber = tracing::subscriber::set_default(logs.clone());
        let (_keep, cert, key, ca) = self_signed_material();
        let config = StordMigrationConfig {
            enabled: true,
            client_cert_path: Some(cert),
            client_key_path: Some(key),
            ca_cert_path: Some(ca),
            dest_server_name: Some("stord-peer".to_string()),
            ..Default::default()
        };
        let (client, server) =
            load_migration_materials(&config).expect("source-only config must load");
        assert!(client.is_some() && server.is_none());
        assert!(
            !logs
                .messages_at(tracing::Level::INFO)
                .iter()
                .any(|m| m.contains("storage migration is disabled")),
            "source-only startup must not log the disabled-migration line"
        );
    }

    #[test]
    fn destination_only_config_does_not_log_disabled_line() {
        let logs = log_capture::LogCollector::default();
        let _subscriber = tracing::subscriber::set_default(logs.clone());
        let (_keep, cert, key, ca) = self_signed_material();
        let config = StordMigrationConfig {
            enabled: true,
            listen_addr: Some("127.0.0.1:50052".to_string()),
            server_cert_path: Some(cert),
            server_key_path: Some(key),
            client_ca_path: Some(ca),
            ..Default::default()
        };
        let (client, server) =
            load_migration_materials(&config).expect("destination-only config must load");
        assert!(client.is_none() && server.is_some());
        assert!(
            !logs
                .messages_at(tracing::Level::INFO)
                .iter()
                .any(|m| m.contains("storage migration is disabled")),
            "destination-only startup must not log the disabled-migration line"
        );
        assert!(
            logs.messages_at(tracing::Level::INFO)
                .iter()
                .any(|m| m.contains("client identity not configured")),
            "destination-only startup keeps the loader's own info line"
        );
    }

    #[test]
    fn both_halves_config_does_not_log_disabled_line() {
        let logs = log_capture::LogCollector::default();
        let _subscriber = tracing::subscriber::set_default(logs.clone());
        let (_keep_client, c_cert, c_key, c_ca) = self_signed_material();
        let (_keep_server, s_cert, s_key, s_ca) = self_signed_material();
        let config = StordMigrationConfig {
            enabled: true,
            client_cert_path: Some(c_cert),
            client_key_path: Some(c_key),
            ca_cert_path: Some(c_ca),
            dest_server_name: Some("stord-peer".to_string()),
            listen_addr: Some("127.0.0.1:50052".to_string()),
            server_cert_path: Some(s_cert),
            server_key_path: Some(s_key),
            client_ca_path: Some(s_ca),
            ..StordMigrationConfig::default()
        };
        let (client, server) =
            load_migration_materials(&config).expect("both-halves config must load");
        assert!(client.is_some() && server.is_some());
        assert!(
            !logs
                .messages_at(tracing::Level::INFO)
                .iter()
                .any(|m| m.contains("storage migration is disabled")),
            "both-halves startup must not log the disabled-migration line"
        );
    }

    // -----------------------------------------------------------------
    // #657: the metrics listener is configured like the agent's — loopback
    // default, explicit `metrics_bind` honored, invalid bind fatal.
    // -----------------------------------------------------------------

    #[test]
    fn metrics_bind_defaults_to_loopback() {
        assert_eq!(DEFAULT_METRICS_BIND, "127.0.0.1:9101");
        assert_eq!(
            chv_observability::metrics_server::resolve_metrics_bind(None, DEFAULT_METRICS_BIND),
            "127.0.0.1:9101"
        );
        assert_eq!(
            chv_observability::metrics_server::resolve_metrics_bind(
                Some("192.0.2.10:9101"),
                DEFAULT_METRICS_BIND
            ),
            "192.0.2.10:9101"
        );
    }

    #[tokio::test]
    async fn invalid_metrics_bind_fails_startup() {
        let config = chv_config::StordConfig {
            metrics_bind: Some("not-a-socket-address".to_string()),
            ..Default::default()
        };
        let err = start_metrics_endpoint(&config, std::time::Instant::now())
            .await
            .expect_err("an invalid metrics_bind must abort startup");
        assert!(
            err.contains("failed to bind metrics listener"),
            "startup error must name the metrics bind failure, got: {err}"
        );
    }

    /// #379 DP2 vocabulary guard: every recognized value — and the
    /// ABSENT key (B1: absent = local) — passes; only a PRESENT but
    /// unknown value aborts. The pre-#379 fallback arm would have
    /// silently served local-file for the typo'd values.
    #[test]
    fn backend_type_vocabulary_fails_closed_on_unknown_values() {
        assert!(
            validate_backend_type(None).is_ok(),
            "an absent backend_type still means local (B1)"
        );
        for known in ["local", "iscsi", "ceph", "lvm"] {
            assert!(
                validate_backend_type(Some(known)).is_ok(),
                "{known} is a recognized backend_type"
            );
        }
        for typo in ["loca", "LVM", "lvm2", "zfs", ""] {
            match validate_backend_type(Some(typo)) {
                Err(msg) => assert!(
                    msg.contains("is not a recognized stord backend"),
                    "got: {msg}"
                ),
                Ok(()) => panic!("a present-but-unknown backend_type '{typo}' must abort"),
            }
        }
    }

    /// #379 DP2 VG guard: a volume group that does not exist fails the
    /// startup check. Non-root-safe by construction — `vgs` reports a
    /// nonexistent VG as an error regardless of privileges — so this
    /// pins the fail-closed wiring without provisioning anything. (The
    /// positive leg is exercised end-to-end by the m4.5 Leg G
    /// qualification on the loopback VG.)
    #[tokio::test]
    async fn volume_group_guard_fails_on_missing_vg() {
        // Either fail-closed shape satisfies the guard: a host without
        // the lvm2 tools reports "vgs could not be run" (the runner
        // image is not guaranteed to carry `vgs` — CI only installs
        // protobuf-compiler), a host with them reports the missing VG.
        // Both abort startup; neither degrades to serving.
        match verify_volume_group("chv-no-such-vg-379").await {
            Err(msg) => assert!(
                msg.contains("does not exist or is not visible")
                    || msg.contains("vgs could not be run"),
                "got: {msg}"
            ),
            Ok(()) => panic!("a missing VG must fail the startup guard"),
        }
    }

    // -----------------------------------------------------------------
    // Issue #663: migration authorization / admission startup wiring.
    // -----------------------------------------------------------------

    #[test]
    fn migration_limits_reject_zero_and_accept_configured_values() {
        let mut migration = StordMigrationConfig::default();
        let defaults = load_migration_limits(&migration).expect("defaults must be valid");
        assert!(defaults.max_inflight > 0);
        assert!(defaults.max_inflight_bytes > 0);

        migration.max_inflight_receiving = 0;
        assert!(load_migration_limits(&migration)
            .expect_err("zero concurrency must abort startup")
            .contains("max_inflight_receiving"));

        migration.max_inflight_receiving = 4;
        migration.max_inflight_receiving_bytes = 0;
        assert!(load_migration_limits(&migration)
            .expect_err("zero bytes must abort startup")
            .contains("max_inflight_receiving_bytes"));

        migration.max_inflight_receiving_bytes = 8 * 1024 * 1024 * 1024;
        let configured = load_migration_limits(&migration).unwrap();
        assert_eq!(configured.max_inflight, 4);
        assert_eq!(configured.max_inflight_bytes, 8 * 1024 * 1024 * 1024);
    }

    #[test]
    fn migration_authorization_insecure_bypass_is_explicit_and_logged() {
        let logs = log_capture::LogCollector::default();
        let _subscriber = tracing::subscriber::set_default(logs.clone());
        let migration = StordMigrationConfig {
            allow_insecure_unauthenticated: true,
            ..StordMigrationConfig::default()
        };
        let authorization =
            load_migration_authorization(&migration, true).expect("explicit bypass must construct");
        assert!(matches!(
            authorization,
            MigrationReceiverAuthorization::DisabledInsecure
        ));
        assert!(
            logs.messages_at(tracing::Level::WARN)
                .iter()
                .any(|m| m.contains("allow_insecure_unauthenticated")),
            "the insecure bypass must be loudly logged"
        );
    }

    #[test]
    fn migration_authorization_receiver_without_secret_fails_closed() {
        // Isolate the env fallback so a developer shell cannot change the
        // outcome of this fail-closed assertion.
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("CHV_MIGRATION_SIGNING_SECRET");

        let migration = StordMigrationConfig::default();
        let error = load_migration_authorization(&migration, true)
            .expect_err("an exposed receiver without a signing secret must abort startup");
        assert!(
            error.contains("no migration signing secret"),
            "got: {error}"
        );

        // Without an exposed TCP receiver the service stays fail-closed
        // instead of aborting the whole daemon.
        assert!(matches!(
            load_migration_authorization(&migration, false).unwrap(),
            MigrationReceiverAuthorization::DenyAll
        ));
    }

    #[test]
    fn migration_authorization_requires_node_id_with_a_secret() {
        let mut migration = StordMigrationConfig {
            signing_secret: Some("dedicated-migration-signing-secret-0123456789abcdef".to_string()),
            ..StordMigrationConfig::default()
        };
        let error = load_migration_authorization(&migration, true)
            .expect_err("a secret without node_id must abort startup");
        assert!(error.contains("node_id is missing"), "got: {error}");

        migration.node_id = Some("node-a".to_string());
        assert!(matches!(
            load_migration_authorization(&migration, true).unwrap(),
            MigrationReceiverAuthorization::Required { .. }
        ));
    }

    #[test]
    fn migration_authorization_rejects_short_or_ambiguous_secrets() {
        let short = StordMigrationConfig {
            node_id: Some("node-a".to_string()),
            signing_secret: Some("too-short".to_string()),
            ..StordMigrationConfig::default()
        };
        assert!(load_migration_authorization(&short, true)
            .expect_err("short secret must abort")
            .contains("shorter than 32 bytes"));

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("migration-secret");
        std::fs::write(&file, b"file-migration-signing-secret-0123456789").unwrap();
        let ambiguous = StordMigrationConfig {
            node_id: Some("node-a".to_string()),
            signing_secret: Some("inline-migration-signing-secret-0123456789".to_string()),
            signing_secret_file: Some(file),
            ..StordMigrationConfig::default()
        };
        assert!(load_migration_authorization(&ambiguous, true)
            .expect_err("inline+file must abort")
            .contains("mutually exclusive"));
    }
}
