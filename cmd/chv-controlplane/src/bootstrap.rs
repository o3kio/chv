mod instance_lock;

use chv_config::{ControlPlaneConfig, ControlPlaneTlsConfig};
use chv_controlplane_service::{
    alert_evaluator::{AlertEvaluatorWorker, EvaluatorChannels},
    compat::{CompatibilityMatrix, Component},
    monitoring_exporter::{MonitoringExporter, ReqwestExportTransport},
    notification_dispatcher::{DispatcherSettings, NotificationDispatcher},
    validate_security_mode, ControlPlaneComponents, ControlPlaneMutationService,
    ControlPlaneRuntime, ControlPlaneService, ControlPlaneServiceError,
    EnrollmentServiceImplementation, InventoryServiceImplementation,
    LifecycleServiceImplementation, MonitoringIngestImplementation, NodeClientPool, Orchestrator,
    ReconcileServiceImplementation, TelemetryServiceImplementation,
};
use chv_controlplane_store::{
    connect_pool, run_migrations, AlertRepository, AlertRuleRepository, AuditLogRepository,
    BackupRepository, BootstrapTokenRepository, ControlPlaneStoreConfig, DesiredStateRepository,
    EventRepository, NodeRepository, NotificationOutboxRepository, ObservedStateRepository,
    OperationRepository, VtepRepository,
};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// Process-lifetime guard for the single-instance advisory lock.
///
/// `build_service` is expected to run once per process, but the lock must be
/// tied to the process, not to a function frame: keeping it in a `OnceLock`
/// means it cannot be dropped accidentally when bootstrap returns while the
/// service is still running. The OS releases the underlying flock on process
/// exit (graceful, SIGTERM, or crash).
static INSTANCE_LOCK: std::sync::OnceLock<instance_lock::InstanceLock> = std::sync::OnceLock::new();

/// Default dedicated Prometheus listener when `metrics_bind` is unset.
/// Loopback, never `0.0.0.0`: the scrape surface has no authentication of its
/// own, so an omitted key must not expose it on every interface (issue #657).
const DEFAULT_METRICS_BIND: &str = "127.0.0.1:9103";

/// Validate the TLS configuration for the control plane.
///
/// Returns `Ok(())` when:
/// - Both `server_cert_path` and `server_key_path` are set, `client_ca_path` is set
///   (production mTLS), OR
/// - `allow_insecure` is `true` (dev / `CHV_ALLOW_INSECURE=1`).
///
/// Returns `Err(ControlPlaneServiceError::Internal)` with a stable, operator-greppable
/// message when the configuration is incomplete. The message strings here are part of
/// the operator contract — changing them will break runbooks/log searches.
///
/// This is extracted as a pure function (no I/O, no env reads) so the polarity of the
/// gate is exercised by unit tests. See ADR-008 (error handling) and ADR-009 (logging).
pub fn validate_tls(
    tls: &ControlPlaneTlsConfig,
    allow_insecure: bool,
) -> Result<(), ControlPlaneServiceError> {
    if tls.server_cert_path.is_some() != tls.server_key_path.is_some() {
        return Err(ControlPlaneServiceError::Internal(
            "both server_cert_path and server_key_path must be set to enable TLS".into(),
        ));
    }

    if !allow_insecure {
        if tls.server_cert_path.is_none() || tls.server_key_path.is_none() {
            return Err(ControlPlaneServiceError::Internal(
                "TLS required: set server_cert_path + server_key_path, or CHV_ALLOW_INSECURE=1 for dev".into(),
            ));
        }
        if tls.client_ca_path.is_none() {
            return Err(ControlPlaneServiceError::Internal(
                "mTLS required: set client_ca_path for client certificate verification, or CHV_ALLOW_INSECURE=1 for dev".into(),
            ));
        }
    }

    Ok(())
}

/// Resolve and bind the dedicated `/metrics` listener (issue #657).
///
/// The bind happens before this returns, so an invalid `metrics_bind` or an
/// occupied port is a startup error. The admin-gated `/metrics` on `http_bind`
/// stays for compatibility, but scrapers use this listener and therefore never
/// need a rotating admin JWT.
async fn start_metrics_endpoint(
    config: &ControlPlaneConfig,
    process_started: std::time::Instant,
) -> Result<(), ControlPlaneServiceError> {
    let bind = chv_observability::metrics_server::resolve_metrics_bind(
        config.metrics_bind.as_deref(),
        DEFAULT_METRICS_BIND,
    );
    if config.metrics_bind.is_none() {
        tracing::warn!(
            bind = %bind,
            "metrics_bind is not set; binding /metrics on loopback — set metrics_bind \
             explicitly to serve a different address"
        );
    }
    let info = chv_observability::DaemonMetricsInfo::new(
        env!("CARGO_PKG_NAME"),
        env!("CHV_VERSION"),
        env!("CHV_GIT_SHA"),
        env!("CHV_RELEASE_CHANNEL"),
    )
    .with_started_at(process_started);
    chv_observability::metrics_server::start_metrics_server(&bind, info)
        .await
        .map_err(|e| {
            ControlPlaneServiceError::Internal(format!(
                "failed to bind metrics listener on {bind}: {e}; set metrics_bind to a valid \
                 bind address (loopback by default)"
            ))
        })
}

/// Connect the isolated monitoring store (ADR-027, #602 PR-2).
///
/// Failure-tolerant by design: the monitoring database is disposable
/// telemetry, so a disabled `[monitoring]` section or any connection
/// failure returns `(None, degraded health)` and the control plane
/// starts normally — VM lifecycle must work with this file deleted.
/// Ingestion answers `ingestion_unavailable` until the operator
/// fixes the store and restarts.
async fn connect_monitoring_store(
    config: &chv_config::MonitoringConfig,
) -> (
    Option<Arc<chv_monitoring_store::MonitoringStore>>,
    chv_monitoring_store::MonitoringHealth,
) {
    let health = chv_monitoring_store::MonitoringHealth::new();
    if !config.enabled {
        health.degrade("monitoring disabled by configuration".to_string());
        return (None, health);
    }

    let store_config = chv_monitoring_store::MonitoringStoreConfig {
        database_url: config.database_url.clone(),
        migrations_dir: config.migrations_dir.clone(),
        max_connections: config.max_connections,
        raw_retention_ms: config.raw_retention_hours * 60 * 60 * 1000,
        rollup_5m_retention_ms: config.rollup_5m_retention_days * 24 * 60 * 60 * 1000,
        rollup_1h_retention_ms: config.rollup_1h_retention_days * 24 * 60 * 60 * 1000,
        max_series_per_target: config.max_series_per_target,
        max_db_bytes: config.max_db_gib * 1024 * 1024 * 1024,
        dedup_retention_ms: chv_monitoring_store::MonitoringStoreConfig::default()
            .dedup_retention_ms,
    };

    // Create the parent directory first (a missing /var/lib/chv/monitoring
    // on a fresh install is normal, not an error).
    if let Some(dir) = store_config
        .database_url
        .strip_prefix("sqlite://")
        .and_then(|p| std::path::Path::new(p).parent())
        .map(|p| p.to_path_buf())
    {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(
                error = %e,
                path = %dir.display(),
                "cannot create monitoring database directory; monitoring degraded"
            );
            health.degrade(format!("cannot create monitoring directory: {e}"));
            return (None, health);
        }
    }

    match chv_monitoring_store::MonitoringStore::connect(store_config).await {
        Ok(store) => (Some(Arc::new(store)), health),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "monitoring store unavailable; starting control plane with monitoring degraded \
                 (VM lifecycle is unaffected — the monitoring database is disposable telemetry)"
            );
            health.degrade(format!("monitoring store unavailable: {e}"));
            (None, health)
        }
    }
}

/// Run the compatibility-matrix gate against enrolled node versions.
///
/// The matrix is operator-opt-in via `CHV_COMPAT_MATRIX_PATH`. Once opted in, the gate
/// is **fail-closed**: a DB query failure at boot is treated as "cannot verify, refuse
/// to start" rather than silently bypassed. A `tracing::warn!` here would let an
/// incompatible-version node reconcile during a transient DB glitch — see H8 in the
/// security review.
///
/// Behavior:
/// - File missing at `matrix_path`: log warn and continue (configured-but-not-yet-deployed
///   is acceptable; the operator may be staging the file).
/// - File present, parse error: log warn and continue (parse errors should surface during
///   matrix authoring, not block prod boot for an isolated control plane).
/// - File present, parsed OK, query fails: **return Err** (fail-closed).
/// - Incompatible versions detected: return Err with the violation summary.
///
/// See ADR-008 (error handling) and ADR-009 (logging/observability).
pub async fn check_compat_matrix(
    pool: &SqlitePool,
    matrix_path: &Path,
) -> Result<(), ControlPlaneServiceError> {
    if !matrix_path.exists() {
        tracing::warn!(
            path = %matrix_path.display(),
            "CHV_COMPAT_MATRIX_PATH set but file does not exist, skipping compatibility check"
        );
        return Ok(());
    }

    let matrix = match CompatibilityMatrix::load_from_file(matrix_path) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                path = %matrix_path.display(),
                error = %e,
                "failed to load compatibility matrix file"
            );
            return Ok(());
        }
    };

    let rows = sqlx::query_as::<_, (String, Option<String>, Option<String>, Option<String>)>(
        "SELECT node_id, chv_agent_version, chv_stord_version, chv_nwd_version \
         FROM node_inventory \
         WHERE chv_agent_version IS NOT NULL \
            OR chv_stord_version IS NOT NULL \
            OR chv_nwd_version IS NOT NULL",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| {
        // FAIL-CLOSED: H8. When the operator has opted into the compat matrix
        // (CHV_COMPAT_MATRIX_PATH is set) and we cannot query node versions,
        // refuse to start. The previous behavior (warn + continue) silently
        // bypassed the gate during a DB glitch, letting an incompatible node
        // reconcile.
        tracing::error!(
            error = %e,
            path = %matrix_path.display(),
            "compatibility matrix gate: failed to query node_inventory; refusing to start (fail-closed)"
        );
        ControlPlaneServiceError::Internal(format!("compat matrix query failed: {e}"))
    })?;

    let mut all_reports = Vec::new();
    for (node_id, agent_ver, stord_ver, nwd_ver) in &rows {
        let mut versions: HashMap<Component, String> = HashMap::new();
        if let Some(v) = agent_ver {
            versions.insert(Component::Agent, v.clone());
        }
        if let Some(v) = stord_ver {
            versions.insert(Component::Stord, v.clone());
        }
        if let Some(v) = nwd_ver {
            versions.insert(Component::Nwd, v.clone());
        }

        let reports = matrix.check_all(&versions);
        for report in &reports {
            tracing::error!(
                node_id = %node_id,
                component = %report.component,
                current_version = %report.current_version,
                min_allowed = %report.min_allowed,
                max_allowed = %report.max_allowed,
                "version incompatibility detected: {}",
                report.message
            );
        }
        all_reports.extend(reports);
    }

    if !all_reports.is_empty() {
        let summary: Vec<String> = all_reports.iter().map(|r| r.message.clone()).collect();
        return Err(ControlPlaneServiceError::Internal(format!(
            "boot blocked: {} incompatible component version(s) detected: {}",
            all_reports.len(),
            summary.join("; ")
        )));
    }

    tracing::info!(
        nodes_checked = rows.len(),
        "compatibility matrix check passed — all versions compatible"
    );
    Ok(())
}

pub async fn build_service(
    config: &ControlPlaneConfig,
    process_started: std::time::Instant,
) -> Result<ControlPlaneService, ControlPlaneServiceError> {
    tokio::fs::create_dir_all(&config.runtime_dir).await?;

    let allow_insecure = std::env::var("CHV_ALLOW_INSECURE")
        .map(|v| v == "1")
        .unwrap_or(false);
    // Security-mode gate (issue #233): typed, non-panicking startup validation
    // BEFORE any listener/interceptor is constructed. A build without the `dev`
    // feature must never start with insecure peer-identity enforcement; the
    // interceptor constructor repeats this check as defense-in-depth.
    validate_security_mode(allow_insecure)?;
    validate_tls(&config.tls, allow_insecure)?;

    // Single-instance guard (#653): take the exclusive lock before the SQLite
    // database is opened or migrated. Two control planes sharing one database
    // would race migrations, seeding, and every lifecycle worker. The guard is
    // stored process-wide so it survives `build_service` returning; the kernel
    // releases it when this process exits, including on SIGTERM/crash.
    let instance_lock = instance_lock::InstanceLock::acquire_for_database(
        &config.runtime_dir,
        &config.database.url,
    )
    .map_err(|e| ControlPlaneServiceError::Internal(e.to_string()))?;
    tracing::info!(
        path = %instance_lock.path().display(),
        "acquired control-plane single-instance lock"
    );
    if INSTANCE_LOCK.set(instance_lock).is_err() {
        return Err(ControlPlaneServiceError::Internal(
            instance_lock::ALREADY_RUNNING_MESSAGE.to_string(),
        ));
    }

    // Dedicated Prometheus listener (#657). Bind after the single-instance
    // guard so a second control plane reports the instance-lock error instead
    // of a misleading metrics-port conflict, and before the database is opened
    // so an invalid `metrics_bind` or an occupied port still aborts startup
    // instead of running without a scrape surface. This listener is separate
    // from the admin-gated `/metrics` route on `http_bind`; it is loopback by
    // default and needs no JWT.
    start_metrics_endpoint(config, process_started).await?;

    let store_config = ControlPlaneStoreConfig {
        database_url: config.database.url.clone(),
        migrations_dir: config.database.migrations_dir.clone(),
        max_connections: config.database.max_connections,
        acquire_timeout_secs: config.database.acquire_timeout_secs,
    };

    let pool = connect_pool(&store_config).await?;
    run_migrations(&pool, Some(&store_config)).await?;

    // Restart reconciliation (#656): the migration state machine is an
    // in-process future, so any non-terminal row at this point belongs to a
    // previous control-plane process. Record those rows as interrupted before
    // any worker (including the reaper) starts, so operators see an inspectable
    // terminal state instead of a row that silently sits non-terminal for the
    // stall timeout. Fail closed: a database error here aborts startup rather
    // than leaving ambiguous migration state behind.
    let recovery_report = chv_controlplane_service::MigrationRegistry::new(pool.clone())
        .recover_after_restart()
        .await
        .map_err(|e| {
            ControlPlaneServiceError::Internal(format!("migration restart recovery failed: {e}"))
        })?;
    if recovery_report.is_empty() {
        tracing::info!("migration restart recovery: no interrupted migrations");
    } else {
        tracing::warn!(
            reconciled = recovery_report.reconciled.len(),
            already_terminal = recovery_report.already_terminal,
            unresolved = recovery_report.unresolved,
            "migration restart recovery completed"
        );
    }

    // Monitoring store (ADR-027, campaign #602 PR-2): disposable
    // telemetry in its own SQLite file and pool. A failure here —
    // missing directory, corrupt file, full disk — DEGRADES monitoring
    // and never aborts control-plane startup: VM lifecycle must work
    // with this file deleted.
    let (monitoring_store, monitoring_health) = connect_monitoring_store(&config.monitoring).await;

    // Seed the six canonical starter topologies on first deployment so the
    // operator lands on a populated /architectures dashboard. The seeder is
    // idempotent — once `system_settings.seed_starters_completed = '1'` it
    // is a cheap no-op on every subsequent boot. Per-fixture failures are
    // logged and skipped (fail-open) so a malformed starter cannot block
    // the control plane from coming up; only a failure to read or update
    // the sentinel row itself propagates as a boot error.
    //
    // See `docs/plans/2026-06-16-starter-topologies-and-auto-seed.md` §4
    // for the full design and `crates/chv-controlplane-seed` for the
    // implementation.
    let seed_topology_repo = chv_controlplane_store::TopologyRepository::new(pool.clone());
    match chv_controlplane_seed::seed_if_first_deployment(&seed_topology_repo).await {
        Ok(chv_controlplane_seed::SeedOutcome::Seeded { count }) => {
            tracing::info!(count, "starter topologies seeded on first deployment");
        }
        Ok(chv_controlplane_seed::SeedOutcome::Skipped) => {
            tracing::debug!("starter topology seeding already completed; skipping");
        }
        Err(err) => {
            // Preserve structured fields for ops dashboards via the dedicated
            // Seed(SeedError) variant — `?err` keeps thiserror's source chain
            // intact for downstream `error = ?` consumers.
            tracing::error!(?err, "starter topology seed fatal");
            return Err(ControlPlaneServiceError::Seed(err));
        }
    }

    // --- Compatibility matrix check (hard gate on incompatibilities) ---
    // Operator-opt-in: only runs when CHV_COMPAT_MATRIX_PATH is set. Once opted
    // in, the gate is fail-closed (see check_compat_matrix docs and H8 review).
    if let Ok(matrix_path) = std::env::var("CHV_COMPAT_MATRIX_PATH") {
        check_compat_matrix(&pool, Path::new(&matrix_path)).await?;
    }

    // Warn if the bootstrap admin password has not been changed
    let default_hash = "$2b$12$JbNLkka47ajSOyzKo8fKI.CBvQav06.Vrnh4pbZf4VSaLwS7yI71m";
    if let Ok(Some(hash)) =
        sqlx::query_scalar::<_, String>("SELECT password_hash FROM users WHERE username = 'admin'")
            .fetch_optional(&pool)
            .await
    {
        if hash == default_hash {
            tracing::warn!(
                "SECURITY: the default 'admin' user still has the bootstrap password. \
                 Change it immediately via the UI or API."
            );
        }
    }

    let node_repo = NodeRepository::new(pool.clone());
    let token_repo = BootstrapTokenRepository::new(pool.clone());
    let observed_state_repo = ObservedStateRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let alert_repo = AlertRepository::new(pool.clone());
    let desired_state_repo = DesiredStateRepository::new(pool.clone());
    let operation_repo = OperationRepository::new(pool.clone());
    // Explicit credential-encryption key from `[secrets] credential_encryption`
    // (#662). `None` keeps the store's documented CHV_ENCRYPTION_KEY /
    // legacy CHV_JWT_SECRET lookup unchanged for existing installs.
    let credential_encryption_key = config.effective_credential_encryption_key();
    let backup_repo = match credential_encryption_key.as_deref() {
        Some(key) => BackupRepository::new_with_key(pool.clone(), key),
        None => BackupRepository::new(pool.clone()),
    };
    let topology_repo = chv_controlplane_store::TopologyRepository::new(pool.clone());
    let vtep_repo = VtepRepository::new(pool.clone());

    // Created before the service implementations: shared by the lifecycle
    // service's operator relays (resolve-inspect-required), the overlay
    // manager, and the orchestrator.
    let node_client_pool =
        NodeClientPool::new().with_auth_token(config.effective_agent_auth_token());

    let lifecycle_service = Arc::new(
        LifecycleServiceImplementation::new(
            node_repo.clone(),
            operation_repo.clone(),
            event_repo.clone(),
            desired_state_repo.clone(),
        )
        .with_node_egress(
            node_client_pool.clone(),
            config.agent_socket_pattern.clone(),
        ),
    );

    let bff_state = chv_webui_bff::AppState {
        pool: pool.clone(),
        node_repo: node_repo.clone(),
        operation_repo: operation_repo.clone(),
        event_repo: event_repo.clone(),
        alert_repo: alert_repo.clone(),
        // Native alerting surface (ADR-027, #602 PR-6): repositories
        // plus config-derived scalars. Booleans only — the signing
        // secret never crosses into the BFF.
        alert_rules: Arc::new(AlertRuleRepository::new(pool.clone())),
        notification_outbox: Arc::new(NotificationOutboxRepository::new(pool.clone())),
        alerting_max_rules: config.monitoring.alerting.max_rules as i64,
        notification_channels: chv_webui_bff::NotificationChannels {
            webhook: config.monitoring.notifications.webhook_url.is_some(),
            slack: config.monitoring.notifications.slack_webhook_url.is_some(),
        },
        desired_state_repo: desired_state_repo.clone(),
        observed_state_repo: observed_state_repo.clone(),
        backup_repo: backup_repo.clone(),
        topology_repo: topology_repo.clone(),
        network_repo: chv_controlplane_store::NetworkRepository::new(pool.clone()),
        image_repo: chv_controlplane_store::ImageRepository::new(pool.clone()),
        apply_runs: Arc::new(chv_controlplane_store::ApplyRunRepository::new(
            pool.clone(),
        )),
        drift_reports: Arc::new(chv_controlplane_store::DriftReportRepository::new(
            pool.clone(),
        )),
        netbox_config: Arc::new(match credential_encryption_key.as_deref() {
            Some(key) => chv_controlplane_store::NetboxProjectionConfigRepository::new_with_key(
                pool.clone(),
                key,
            ),
            None => chv_controlplane_store::NetboxProjectionConfigRepository::new(pool.clone()),
        }),
        netbox_runs: Arc::new(chv_controlplane_store::NetboxProjectionRunRepository::new(
            pool.clone(),
        )),
        mutations: Arc::new(ControlPlaneMutationService::new(
            pool.clone(),
            lifecycle_service.clone(),
        )),
        monitoring: monitoring_store.clone(),
        monitoring_health: monitoring_health.clone(),
        jwt_secret: config.effective_user_jwt_secret(),
        console_jwt_secret: config.effective_console_jwt_secret(),
        agent_runtime_dir: config.agent_runtime_dir.clone(),
        cache: chv_webui_bff::BffCache::new(5),
        clock: Arc::new(chv_common::SystemClock),
    };

    let convergence_metrics = chv_controlplane_service::convergence_metrics::new_shared();

    // Native alerting (ADR-027, campaign #602 PR-6): validation is
    // fail-loud at boot, the same contract as guest ingestion — a
    // misconfigured alerting/notifications section is an operator
    // error the operator must see at startup, never a silent
    // never-firing (or never-delivering) degradation discovered
    // mid-incident.
    if let Err(reason) = config.monitoring.alerting.validate() {
        return Err(ControlPlaneServiceError::Internal(format!(
            "invalid [monitoring.alerting]: {reason}"
        )));
    }
    if let Err(reason) = config.monitoring.notifications.validate() {
        return Err(ControlPlaneServiceError::Internal(format!(
            "invalid [monitoring.notifications]: {reason}"
        )));
    }
    // Optional external export (ADR-027 decision 9, campaign #602
    // PR-8): same fail-loud-at-boot contract — an enabled export with
    // no metric selection, or an unsafe destination, is an operator
    // error to fix in the config, not a silent no-export degradation.
    if let Err(reason) = config.monitoring.export.validate() {
        return Err(ControlPlaneServiceError::Internal(format!(
            "invalid [monitoring.export]: {reason}"
        )));
    }

    // Guest monitoring agent ingestion (ADR-026, campaign #602 G3):
    // an explicit deployment decision, never a side effect. Enabling
    // it requires `[http_tls]` — the guest route must never ride a
    // plain-HTTP listener exposed to the VM network — and the
    // dedicated agent CA. Anything missing is a boot error, not a
    // silent degradation the operator discovers mid-enrollment.
    let guest_ingest = &config.monitoring.guest_ingestion;
    if guest_ingest.enabled {
        if let Err(reason) = guest_ingest.validate() {
            return Err(ControlPlaneServiceError::Internal(reason));
        }
        if config.http_tls.is_none() {
            return Err(ControlPlaneServiceError::Internal(
                "monitoring.guest_ingestion.enabled requires [http_tls]: guest ingestion must \
                 be served over TLS, never the plain-HTTP listener"
                    .to_string(),
            ));
        }
        if guest_ingest.agent_ca_cert_path.is_none() || guest_ingest.agent_ca_key_path.is_none() {
            return Err(ControlPlaneServiceError::Internal(
                "monitoring.guest_ingestion.enabled requires agent_ca_cert_path and \
                 agent_ca_key_path (a CA dedicated to guest monitoring agents)"
                    .to_string(),
            ));
        }
    }

    let guest_agent_service = if guest_ingest.is_ready(config.http_tls.is_some()) {
        let ca_cert_pem = tokio::fs::read_to_string(
            guest_ingest
                .agent_ca_cert_path
                .as_ref()
                .expect("checked above"),
        )
        .await
        .map_err(|e| {
            ControlPlaneServiceError::Internal(format!(
                "failed to read guest agent CA certificate: {e}"
            ))
        })?;
        let ca_key_pem = tokio::fs::read_to_string(
            guest_ingest
                .agent_ca_key_path
                .as_ref()
                .expect("checked above"),
        )
        .await
        .map_err(|e| {
            ControlPlaneServiceError::Internal(format!("failed to read guest agent CA key: {e}"))
        })?;
        let issuer = Arc::new(
            chv_controlplane_service::monitoring_agent::AgentCertificateIssuer::new(
                &ca_cert_pem,
                &ca_key_pem,
            )?,
        );
        let limits = chv_controlplane_service::monitoring_agent::GuestIngestionLimits {
            claim_ttl_ms: guest_ingest.claim_ttl_seconds as i64 * 1000,
            enroll_attempts_per_minute: guest_ingest.enroll_attempts_per_minute,
            agent_batches_per_minute: guest_ingest.agent_batches_per_minute,
            rotation_grace_ms: guest_ingest.rotation_grace_minutes as i64 * 60_000,
            credential_ttl_ms: guest_ingest.credential_ttl_days as i64 * 24 * 3600 * 1000,
            renewal_window_ms: guest_ingest.renewal_window_days as i64 * 24 * 3600 * 1000,
        };
        let agent_repo = chv_controlplane_store::MonitoringAgentRepository::new(pool.clone());
        Some(Arc::new(
            chv_controlplane_service::monitoring_agent::MonitoringAgentService::new(
                agent_repo,
                issuer,
                monitoring_store.clone(),
                monitoring_health.clone(),
                event_repo.clone(),
                limits,
                guest_ingest.public_base_url.clone(),
            ),
        ))
    } else {
        None
    };

    let (agent_browser, agent_ingest_router) = match &guest_agent_service {
        Some(service) => {
            let admin_state = Arc::new(
                chv_controlplane_service::api::agent_admin::AgentAdminState {
                    service: service.clone(),
                    offline_after_ms: guest_ingest.offline_after_seconds as i64 * 1000,
                    enrollment_grace_ms: guest_ingest.enrollment_grace_seconds as i64 * 1000,
                    renewal_window_ms: guest_ingest.renewal_window_days as i64 * 24 * 3600 * 1000,
                },
            );
            let viewer = chv_controlplane_service::api::agent_admin::agent_viewer_router(
                admin_state.clone(),
            );
            let operator =
                chv_controlplane_service::api::agent_admin::agent_operator_router(admin_state);
            let ingest = chv_controlplane_service::api::agent_routes::agent_router(service.clone());
            (Some((viewer, operator)), Some(ingest))
        }
        None => {
            // Guest ingestion not configured: the browser routes still
            // exist and answer the typed `guest_ingestion_disabled`
            // error (honest "off", never a 404); the agent-auth
            // ingest routes stay unmounted entirely — nothing for a
            // guest to talk to.
            let (viewer, operator) =
                chv_controlplane_service::api::agent_admin::agent_admin_disabled_routers();
            (Some((viewer, operator)), None)
        }
    };

    let router = chv_controlplane_service::api::router::admin_router_with_guest_agents(
        bff_state,
        convergence_metrics.clone(),
        config.webui.clone(),
        agent_browser,
        agent_ingest_router,
    );

    // HTTPS on the HTTP listener when [http_tls] is configured. The
    // client CA for optional agent-certificate verification is the
    // guest agent CA (only present when guest ingestion is enabled);
    // without it this is an ordinary TLS listener for browser traffic.
    let https_config = match &config.http_tls {
        Some(tls_conf) => {
            let server_cert_pem = tokio::fs::read_to_string(&tls_conf.server_cert_path)
                .await
                .map_err(|e| {
                    ControlPlaneServiceError::Internal(format!(
                        "failed to read http_tls server certificate: {e}"
                    ))
                })?;
            let server_key_pem = tokio::fs::read_to_string(&tls_conf.server_key_path)
                .await
                .map_err(|e| {
                    ControlPlaneServiceError::Internal(format!(
                        "failed to read http_tls server key: {e}"
                    ))
                })?;
            let client_ca_pem = guest_agent_service
                .as_ref()
                .map(|s| s.issuer().ca_pem().to_string());
            Some(
                chv_controlplane_service::api::tls::build_https_config(
                    &server_cert_pem,
                    &server_key_pem,
                    client_ca_pem.as_deref(),
                )
                .map_err(ControlPlaneServiceError::Internal)?,
            )
        }
        None => None,
    };

    let http_listener = tokio::net::TcpListener::bind(config.http_bind)
        .await
        .map_err(|e| {
            ControlPlaneServiceError::Internal(format!("failed to bind HTTP listener: {}", e))
        })?;
    let (http_shutdown_tx, mut http_shutdown_rx) = tokio::sync::watch::channel(());
    let http_join_handle = tokio::spawn(async move {
        match https_config {
            Some(tls) => {
                chv_controlplane_service::api::tls::serve_tls(
                    http_listener,
                    tls,
                    router,
                    http_shutdown_rx,
                )
                .await
            }
            None => {
                axum::serve(
                    http_listener,
                    router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
                )
                .with_graceful_shutdown(async move {
                    let _ = http_shutdown_rx.changed().await;
                })
                .await
            }
        }
    });

    let cert_issuer = if let (Some(ca_cert_path), Some(ca_key_path)) =
        (&config.tls.ca_cert_path, &config.tls.ca_key_path)
    {
        let ca_cert_pem = tokio::fs::read_to_string(ca_cert_path).await.map_err(|e| {
            ControlPlaneServiceError::Internal(format!("failed to read CA certificate: {}", e))
        })?;
        let ca_key_pem = tokio::fs::read_to_string(ca_key_path).await.map_err(|e| {
            ControlPlaneServiceError::Internal(format!("failed to read CA key: {}", e))
        })?;

        Some(
            Arc::new(chv_controlplane_service::CaBackedCertificateIssuer::new(
                &ca_cert_pem,
                &ca_key_pem,
            )?) as Arc<dyn chv_controlplane_service::CertificateIssuer>,
        )
    } else {
        None
    };

    let enrollment_service = EnrollmentServiceImplementation::new(
        node_repo.clone(),
        token_repo.clone(),
        cert_issuer,
        vtep_repo.clone(),
    )
    .with_certificate_revocations(
        chv_controlplane_store::CertificateRevocationRepository::new(pool.clone()),
    );
    let inventory_service =
        InventoryServiceImplementation::new(node_repo.clone(), vtep_repo.clone());
    let telemetry_service = TelemetryServiceImplementation::new(
        node_repo.clone(),
        observed_state_repo.clone(),
        event_repo.clone(),
        alert_repo.clone(),
    );
    let monitoring_ingest_service = MonitoringIngestImplementation::new(
        monitoring_store.clone(),
        node_repo.clone(),
        observed_state_repo.clone(),
        monitoring_health.clone(),
        config.monitoring.batches_per_minute,
        config.monitoring.min_headroom_mib * 1024 * 1024,
    );
    let reconcile_service = ReconcileServiceImplementation::new(
        node_repo.clone(),
        desired_state_repo.clone(),
        event_repo.clone(),
        observed_state_repo.clone(),
        operation_repo.clone(),
    );

    let mut tls_config = None;
    if let (Some(cert_path), Some(key_path)) =
        (&config.tls.server_cert_path, &config.tls.server_key_path)
    {
        let cert_pem = tokio::fs::read(cert_path).await.map_err(|e| {
            ControlPlaneServiceError::Internal(format!("failed to read TLS certificate: {}", e))
        })?;
        let key_pem = tokio::fs::read(key_path).await.map_err(|e| {
            ControlPlaneServiceError::Internal(format!("failed to read TLS key: {}", e))
        })?;
        let identity = tonic::transport::Identity::from_pem(cert_pem, key_pem);
        let mut server_tls = tonic::transport::ServerTlsConfig::new().identity(identity);
        if let Some(client_ca_path) = &config.tls.client_ca_path {
            let client_ca_pem = tokio::fs::read(client_ca_path).await.map_err(|e| {
                ControlPlaneServiceError::Internal(format!(
                    "failed to read client CA certificate: {}",
                    e
                ))
            })?;
            server_tls =
                server_tls.client_ca_root(tonic::transport::Certificate::from_pem(client_ca_pem));
        }
        tls_config = Some(server_tls);
    }

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());

    let runtime = ControlPlaneRuntime::new(
        config.grpc_bind,
        config.runtime_dir.clone(),
        tls_config,
        allow_insecure,
        http_shutdown_tx,
        http_join_handle,
        shutdown_rx.clone(),
    );

    let overlay_manager = chv_controlplane_service::OverlayManager::new(
        node_client_pool.clone(),
        config.agent_socket_pattern.clone(),
    );

    // Dedicated migration-signing secret (issue #663). Deliberately a
    // separate key from jwt_secret: rotating user auth must not invalidate
    // in-flight migration authorizations, and a leak of one domain must not
    // compromise the other. Missing secret is allowed at startup so a
    // deployment that never migrates still boots; a migration dispatch then
    // fails closed before contacting the agent.
    let migration_signing_secret = chv_config::resolve_migration_signing_secret(
        config.migration.signing_secret.as_deref(),
        config.migration.signing_secret_file.as_deref(),
        "CHV_MIGRATION_SIGNING_SECRET",
    )
    .map_err(|e| {
        ControlPlaneServiceError::InvalidArgument(format!(
            "invalid migration signing-secret configuration: {e}"
        ))
    })?;
    if migration_signing_secret.is_some() {
        tracing::info!("migration authorization minting enabled (dedicated signing secret loaded)");
    } else if config.migration.allow_insecure_unauthenticated {
        tracing::warn!(
            "SECURITY: migration.allow_insecure_unauthenticated = true — disk migrations will \
             be dispatched without control-plane authorizations. Development-only; never run \
             this in production"
        );
    } else {
        tracing::warn!(
            "no migration signing secret configured: disk migration dispatch will fail closed \
             until [migration].signing_secret_file (or CHV_MIGRATION_SIGNING_SECRET) is set"
        );
    }

    let orchestrator = Orchestrator::new(
        pool.clone(),
        operation_repo.clone(),
        config.agent_socket_pattern.clone(),
        config.kernel_path.clone(),
        config.firmware_path.clone(),
        node_client_pool.clone(),
        convergence_metrics,
    )
    .with_overlay_manager(overlay_manager)
    .with_migration_authorization(
        migration_signing_secret,
        std::time::Duration::from_secs(config.migration.token_ttl_seconds),
        config.migration.allow_insecure_unauthenticated,
    );
    let orchestrator_handle = tokio::spawn(orchestrator.run(shutdown_rx.clone()));

    let backup_staging_dir = config.runtime_dir.join("backups");
    if let Err(e) = std::fs::create_dir_all(&backup_staging_dir) {
        tracing::warn!(
            error = %e,
            path = %backup_staging_dir.display(),
            "failed to create backup staging directory"
        );
    }
    let backup_worker = chv_controlplane_service::BackupWorker::new(
        pool.clone(),
        backup_repo.clone(),
        config.agent_socket_pattern.clone(),
        node_client_pool.clone(),
        backup_staging_dir,
    );
    let backup_worker_handle = tokio::spawn(backup_worker.run(shutdown_rx.clone()));

    // Monitoring maintenance worker (#602 PR-2): rollups, retention,
    // size budget, WAL checkpoint — one bounded pass per interval. A
    // failed pass degrades monitoring health and retries; it can never
    // block or crash the control plane. Absent store ⇒ idle handle.
    let monitoring_worker_handle = match monitoring_store.clone() {
        Some(store) => tokio::spawn(chv_controlplane_service::run_monitoring_maintenance(
            store,
            monitoring_health.clone(),
            std::time::Duration::from_secs(config.monitoring.maintenance_interval_secs.max(1)),
            shutdown_rx.clone(),
        )),
        None => tokio::spawn(async {}),
    };

    // NetBox projection worker (issue #239): claims queued projection
    // runs and executes them against each architecture's configured
    // NetBox instance. A NetBox outage can never propagate here — every
    // run failure is marked on the run row and the loop continues.
    let netbox_projection_worker = chv_controlplane_service::NetboxProjectionWorker::new(
        chv_controlplane_store::NetboxProjectionRunRepository::new(pool.clone()),
        match credential_encryption_key.as_deref() {
            Some(key) => chv_controlplane_store::NetboxProjectionConfigRepository::new_with_key(
                pool.clone(),
                key,
            ),
            None => chv_controlplane_store::NetboxProjectionConfigRepository::new(pool.clone()),
        },
        event_repo.clone(),
        chv_controlplane_store::ApplyRunRepository::new(pool.clone()),
        chv_controlplane_store::VersionRepository::new(pool.clone()),
    );
    // Demo-mode override (issue #586 / ADR-024 decision 5): with the
    // `netbox-demo` feature compiled in, swap the worker's HTTPS-only
    // client factory for the double-gated plain-HTTP one so the
    // controlplane can project into the local `chv-netbox-sim`
    // simulator. The factory itself re-checks
    // `CHV_NETBOX_ALLOW_HTTP=1` at runtime and fails closed without
    // it — both gates must be open. Default-feature builds never
    // compile this block: the worker keeps `NetBoxClient::new`
    // (HTTPS-only) and the binary's behavior is byte-identical.
    #[cfg(feature = "netbox-demo")]
    let netbox_projection_worker = {
        // Same exact-match check the factory closure enforces
        // (`netbox_demo::allow_http_env`) — one source of truth, so
        // the log can never claim a gate state the code does not
        // enforce.
        if chv_controlplane_service::netbox_demo::allow_http_env() {
            tracing::warn!(
                "NETBOX DEMO MODE: plain-HTTP NetBox client enabled \
                 (feature netbox-demo + CHV_NETBOX_ALLOW_HTTP=1) — NOT FOR PRODUCTION"
            );
        } else {
            tracing::warn!(
                "netbox-demo feature is compiled in but CHV_NETBOX_ALLOW_HTTP is \
                 not \"1\" — the projection worker stays HTTPS-only (the demo \
                 plain-HTTP client factory fails closed)"
            );
        }
        netbox_projection_worker
            .with_client_factory(chv_controlplane_service::netbox_demo::plain_http_client_factory())
    };
    let netbox_projection_worker_handle =
        tokio::spawn(netbox_projection_worker.run(shutdown_rx.clone()));

    // Native alerting workers (ADR-027, #602 PR-6).
    //
    // The evaluator runs only when alerting is enabled AND the
    // monitoring store is up: without stored samples every condition
    // reads as Missing, and an availability rule would fire on the
    // degradation itself. Degrading alerting (never the control
    // plane) is the honest behavior; the dispatcher below still runs
    // so events already enqueued before the degradation deliver.
    let alerting_config = &config.monitoring.alerting;
    let notifications_config = &config.monitoring.notifications;
    let alert_evaluator_handle = if alerting_config.enabled {
        match monitoring_store.as_ref() {
            Some(store) => {
                let evaluator = AlertEvaluatorWorker::new(
                    AlertRuleRepository::new(pool.clone()),
                    AlertRepository::new(pool.clone()),
                    store.clone(),
                    EvaluatorChannels {
                        webhook: notifications_config.webhook_url.is_some(),
                        slack: notifications_config.slack_webhook_url.is_some(),
                    },
                );
                let interval =
                    std::time::Duration::from_secs(alerting_config.evaluation_interval_secs);
                let shutdown = shutdown_rx.clone();
                tokio::spawn(async move { evaluator.run(interval, shutdown).await })
            }
            None => {
                tracing::warn!(
                    "monitoring.alerting is enabled but the monitoring store is unavailable; \
                     alert evaluation is degraded until the control plane is restarted with the \
                     monitoring store reachable (VM lifecycle and incident reads are unaffected)"
                );
                tokio::spawn(async {})
            }
        }
    } else {
        tracing::debug!("monitoring.alerting disabled by configuration; evaluator not started");
        tokio::spawn(async {})
    };

    // The dispatcher runs whenever a destination is configured —
    // independently of the evaluator, so pending outbox events from
    // before a monitoring degradation still deliver.
    let notification_dispatcher_handle = if notifications_config.has_destination() {
        // A configured CA bundle is an operator contract: unreadable
        // or invalid PEM is a boot error, not a silent system-roots
        // fallback that would fail every TLS handshake later.
        let ca_pem = match &notifications_config.webhook_ca_path {
            Some(path) => match std::fs::read_to_string(path) {
                Ok(pem) => Some(pem),
                Err(e) => {
                    return Err(ControlPlaneServiceError::Internal(format!(
                        "cannot read monitoring.notifications.webhook_ca_path {}: {e}",
                        path.display()
                    )));
                }
            },
            None => None,
        };
        let dispatcher = NotificationDispatcher::new(
            NotificationOutboxRepository::new(pool.clone()),
            event_repo.clone(),
            DispatcherSettings {
                webhook_url: notifications_config.webhook_url.clone(),
                signing_secret: notifications_config
                    .webhook_signing_secret
                    .as_str()
                    .to_string(),
                slack_webhook_url: notifications_config.slack_webhook_url.clone(),
                max_attempts: notifications_config.max_attempts,
                max_batch: notifications_config.max_batch as i64,
            },
            ca_pem.as_deref(),
        )
        .map_err(ControlPlaneServiceError::Internal)?;
        let interval = std::time::Duration::from_secs(notifications_config.dispatch_interval_secs);
        let shutdown = shutdown_rx.clone();
        tokio::spawn(async move { dispatcher.run(interval, shutdown).await })
    } else {
        tracing::debug!(
            "no notification destination configured; dispatcher not started (the UI is \
             always a channel)"
        );
        tokio::spawn(async {})
    };

    // Optional external export worker (ADR-027 decision 9, #602 PR-8).
    //
    // Runs only when explicitly enabled with a metric selection, and
    // only with the monitoring store up: without stored samples there
    // is nothing to export — degrading export (never the control
    // plane, never the native graphs) is the honest behavior. The
    // exporter degrades on destination outages by design; see the
    // worker docs for the bounded-backfill contract.
    let export_config = &config.monitoring.export;
    let monitoring_exporter_handle =
        if export_config.enabled && !export_config.metric_ids.is_empty() {
            match monitoring_store.as_ref() {
                Some(store) => {
                    // Bearer token and CA are operator contracts:
                    // unreadable or invalid material is a boot error,
                    // never a silent unauthenticated or
                    // system-roots fallback that would fail (or
                    // weaken) every push later.
                    let bearer_token = match &export_config.bearer_token_path {
                        Some(path) => match std::fs::read_to_string(path) {
                            Ok(raw) => {
                                let token = raw.trim();
                                if token.is_empty() {
                                    return Err(ControlPlaneServiceError::Internal(format!(
                                        "monitoring.export.bearer_token_path {} is empty \
                                         after trimming surrounding whitespace",
                                        path.display()
                                    )));
                                }
                                Some(token.to_string())
                            }
                            Err(e) => {
                                return Err(ControlPlaneServiceError::Internal(format!(
                                    "cannot read monitoring.export.bearer_token_path {}: {e}",
                                    path.display()
                                )));
                            }
                        },
                        None => None,
                    };
                    let ca_pem = match &export_config.ca_cert_path {
                        Some(path) => match std::fs::read_to_string(path) {
                            Ok(pem) => Some(pem),
                            Err(e) => {
                                return Err(ControlPlaneServiceError::Internal(format!(
                                    "cannot read monitoring.export.ca_cert_path {}: {e}",
                                    path.display()
                                )));
                            }
                        },
                        None => None,
                    };
                    // destination_url is guaranteed by the config
                    // validation above; the defensive fallback fails
                    // loud rather than pushing anywhere.
                    let destination = export_config.destination_url.as_deref().unwrap_or("");
                    let transport =
                        ReqwestExportTransport::new(destination, bearer_token, ca_pem.as_deref())
                            .map_err(ControlPlaneServiceError::Internal)?;
                    let exporter = MonitoringExporter::new(
                        export_config,
                        store.clone(),
                        std::sync::Arc::new(transport),
                    )
                    .map_err(ControlPlaneServiceError::Internal)?;
                    let shutdown = shutdown_rx.clone();
                    tokio::spawn(async move { exporter.run(shutdown).await })
                }
                None => {
                    tracing::warn!(
                        "monitoring.export is enabled but the monitoring store is unavailable; \
                         export is degraded until the control plane is restarted with the \
                         monitoring store reachable (local monitoring, alerting and VM \
                         lifecycle are unaffected)"
                    );
                    tokio::spawn(async {})
                }
            }
        } else {
            // Info, not debug: export is an opt-in egress surface, so
            // the default install must positively confirm at the
            // default log level that export was evaluated and
            // declined — silence must never be ambiguous between
            // "disabled by config" and "silently never wired".
            tracing::info!("monitoring.export not enabled; exporter not started");
            tokio::spawn(async {})
        };

    let mut migration_reaper = chv_controlplane_service::MigrationReaper::new(
        pool.clone(),
        node_client_pool.clone(),
        config.agent_socket_pattern.clone(),
    );
    // Operator override for the no-progress (stall) timeout. This is not a
    // total-runtime cap: a migration may run for hours as long as it keeps
    // reporting progress. Invalid or non-positive values fall back to the
    // service default rather than disabling the reaper.
    if let Ok(raw) = std::env::var("CHV_MIGRATION_STALL_TIMEOUT_SECS") {
        match raw.parse::<i64>() {
            Ok(secs) if secs > 0 => {
                tracing::info!(
                    stall_timeout_secs = secs,
                    "migration reaper stall timeout overridden by CHV_MIGRATION_STALL_TIMEOUT_SECS"
                );
                migration_reaper = migration_reaper.with_timeout_secs(secs);
            }
            Ok(secs) => tracing::warn!(
                value = secs,
                "CHV_MIGRATION_STALL_TIMEOUT_SECS must be positive; using default"
            ),
            Err(e) => tracing::warn!(
                value = %raw,
                error = %e,
                "CHV_MIGRATION_STALL_TIMEOUT_SECS is not an integer; using default"
            ),
        }
    }
    // Audit-log retention (#658): bounded periodic sweep, plus an
    // immediate pass on startup so an upgraded host does not wait a
    // whole interval to enforce the configured retention. Failures are
    // logged but never block startup or the request path.
    let audit_retention_repo = AuditLogRepository::new(pool.clone());
    let audit_retention_ms = (config.audit.retention_days as i64).saturating_mul(86_400_000);
    // Row-count ceiling (#658 review M2): bounds the table even when
    // rows arrive faster than they age out (anonymous-denial floods).
    // Clamped into i64 range defensively; the config validator already
    // rejects absurd values.
    let audit_max_rows = config.audit.max_rows.min(i64::MAX as u64) as i64;
    let audit_sweep_interval =
        std::time::Duration::from_secs(config.audit.sweep_interval_secs.max(60));
    let mut audit_shutdown = shutdown_rx.clone();
    let audit_retention_handle = tokio::spawn(async move {
        loop {
            let cutoff = chv_common::now_unix_ms().saturating_sub(audit_retention_ms);
            match audit_retention_repo.delete_older_than(cutoff).await {
                Ok(deleted) if deleted > 0 => {
                    tracing::info!(
                        target: "audit",
                        deleted,
                        retention_days = audit_retention_ms / 86_400_000,
                        "audit log retention sweep removed expired rows"
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::error!(
                        target: "audit",
                        error = %error,
                        "audit log retention sweep failed; previous rows remain"
                    );
                }
            }
            // Cap the table at max_rows newest rows. A warn, not an
            // info: hitting the ceiling means evidence is being
            // discarded ahead of schedule and the operator should
            // investigate the row flood (or raise the cap).
            match audit_retention_repo.prune_to_max_rows(audit_max_rows).await {
                Ok(pruned) if pruned > 0 => {
                    tracing::warn!(
                        target: "audit",
                        pruned,
                        max_rows = audit_max_rows,
                        "audit log row cap pruned oldest rows"
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::error!(
                        target: "audit",
                        error = %error,
                        "audit log row-cap prune failed; previous rows remain"
                    );
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(audit_sweep_interval) => {}
                _ = audit_shutdown.changed() => break,
            }
        }
    });

    let reaper_handle = tokio::spawn(migration_reaper.run(shutdown_rx));

    Ok(ControlPlaneService::new(
        runtime,
        ControlPlaneComponents::new(
            pool,
            enrollment_service,
            inventory_service,
            telemetry_service,
            reconcile_service,
            (*lifecycle_service).clone(),
            monitoring_ingest_service,
        ),
        shutdown_tx,
        vec![
            orchestrator_handle,
            backup_worker_handle,
            netbox_projection_worker_handle,
            reaper_handle,
            monitoring_worker_handle,
            alert_evaluator_handle,
            notification_dispatcher_handle,
            monitoring_exporter_handle,
            audit_retention_handle,
        ],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ---------------------------------------------------------------------
    // H7: validate_tls — pure-function unit tests for the TLS/mTLS gate.
    //
    // These tests pin the polarity of the gate. A future refactor that
    // accidentally flips a condition (e.g., && → ||) will fail here long
    // before a production binary ships with mTLS disabled.
    // ---------------------------------------------------------------------

    fn p(s: &str) -> Option<PathBuf> {
        Some(PathBuf::from(s))
    }

    fn err_msg(r: Result<(), ControlPlaneServiceError>) -> String {
        match r {
            Err(ControlPlaneServiceError::Internal(m)) => m,
            other => panic!("expected Err(Internal), got {other:?}"),
        }
    }

    // ---------------------------------------------------------------------
    // #657: dedicated metrics listener — loopback default, explicit bind
    // honored, invalid bind fatal.
    // ---------------------------------------------------------------------

    #[test]
    fn metrics_bind_defaults_to_loopback() {
        assert_eq!(DEFAULT_METRICS_BIND, "127.0.0.1:9103");
        assert_eq!(
            chv_observability::metrics_server::resolve_metrics_bind(None, DEFAULT_METRICS_BIND),
            "127.0.0.1:9103"
        );
        assert_eq!(
            chv_observability::metrics_server::resolve_metrics_bind(
                Some("192.0.2.10:9103"),
                DEFAULT_METRICS_BIND
            ),
            "192.0.2.10:9103"
        );
    }

    #[tokio::test]
    async fn invalid_metrics_bind_fails_startup() {
        let config = ControlPlaneConfig {
            metrics_bind: Some("not-a-socket-address".to_string()),
            ..Default::default()
        };
        let err = err_msg(start_metrics_endpoint(&config, std::time::Instant::now()).await);
        assert!(
            err.contains("failed to bind metrics listener"),
            "startup error must name the metrics bind failure, got: {err}"
        );
    }

    #[test]
    fn validate_tls_full_mtls_ok() {
        let tls = ControlPlaneTlsConfig {
            server_cert_path: p("/tls/server.crt"),
            server_key_path: p("/tls/server.key"),
            client_ca_path: p("/tls/ca.crt"),
            ca_cert_path: None,
            ca_key_path: None,
        };
        assert!(validate_tls(&tls, false).is_ok());
    }

    #[test]
    fn validate_tls_cert_only_rejects_with_pair_message() {
        let tls = ControlPlaneTlsConfig {
            server_cert_path: p("/tls/server.crt"),
            server_key_path: None,
            client_ca_path: None,
            ca_cert_path: None,
            ca_key_path: None,
        };
        let msg = err_msg(validate_tls(&tls, false));
        assert!(
            msg.contains("both server_cert_path and server_key_path"),
            "expected pair-required message, got: {msg}"
        );
    }

    #[test]
    fn validate_tls_no_certs_rejects_with_tls_required() {
        let tls = ControlPlaneTlsConfig::default();
        let msg = err_msg(validate_tls(&tls, false));
        assert!(
            msg.contains("TLS required"),
            "expected TLS-required message, got: {msg}"
        );
    }

    #[test]
    fn validate_tls_certs_without_client_ca_rejects_with_mtls_required() {
        let tls = ControlPlaneTlsConfig {
            server_cert_path: p("/tls/server.crt"),
            server_key_path: p("/tls/server.key"),
            client_ca_path: None,
            ca_cert_path: None,
            ca_key_path: None,
        };
        let msg = err_msg(validate_tls(&tls, false));
        assert!(
            msg.contains("mTLS required"),
            "expected mTLS-required message, got: {msg}"
        );
    }

    #[test]
    fn validate_tls_no_certs_with_allow_insecure_ok() {
        let tls = ControlPlaneTlsConfig::default();
        assert!(validate_tls(&tls, true).is_ok());
    }

    // ---------------------------------------------------------------------
    // H8: check_compat_matrix — fail-closed on DB query failure when the
    // operator has opted into the matrix gate.
    //
    // We exercise the query-failure path by pointing check_compat_matrix at
    // an in-memory SQLite pool that has no `node_inventory` table. Sqlx will
    // return a "no such table" error from fetch_all; we want that error to
    // propagate as Err, not be swallowed as warn-and-continue.
    // ---------------------------------------------------------------------

    async fn empty_pool() -> SqlitePool {
        SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite")
    }

    fn write_minimal_matrix() -> tempfile::NamedTempFile {
        // A minimal-but-valid matrix file. Format mirrors crates/chv-controlplane-service/src/compat.rs.
        let f = tempfile::Builder::new()
            .suffix(".toml")
            .tempfile()
            .expect("tempfile");
        std::fs::write(
            f.path(),
            r#"[compatibility]
[[compatibility.entry]]
component = "agent"
min_version = "0.0.0"
max_version = "999.0.0"
"#,
        )
        .expect("write matrix");
        f
    }

    #[tokio::test]
    async fn compat_matrix_query_failure_when_opted_in_returns_err() {
        // node_inventory does not exist in this pool → fetch_all errors.
        // Operator HAS opted in (we're calling check_compat_matrix with a
        // valid path), so the gate must fail-closed.
        let pool = empty_pool().await;
        let matrix = write_minimal_matrix();

        let result = check_compat_matrix(&pool, matrix.path()).await;

        match result {
            Err(ControlPlaneServiceError::Internal(msg)) => {
                assert!(
                    msg.contains("compat matrix query failed"),
                    "expected fail-closed message, got: {msg}"
                );
            }
            other => panic!("expected Err(Internal) from fail-closed gate, got {other:?}"),
        }
    }

    /// Pin the startup invariant that restart recovery runs before anything
    /// that can serve traffic or start a background worker. `build_service` is
    /// too heavy to drive end-to-end in a unit test, so this asserts the
    /// source order directly.
    #[test]
    fn restart_recovery_precedes_listener_and_workers() {
        let source = include_str!("bootstrap.rs");

        // Anchor on the real recovery statement, not any earlier textual
        // mention. The call site is a single registry construction followed
        // closely by `recover_after_restart()`.
        // Split literals so `include_str!` does not count this test's own
        // assertions as a second occurrence.
        let recovery_anchor = concat!(
            "let recovery_report = chv_controlplane_service::",
            "MigrationRegistry::new(pool.clone())"
        );
        assert_eq!(
            source.matches(recovery_anchor).count(),
            1,
            "the restart recovery statement must appear exactly once"
        );
        let recovery = source
            .find(recovery_anchor)
            .expect("restart recovery statement must exist");
        let recover_call = concat!(".recover_after_", "restart()");
        let recovery_call = recovery
            + source[recovery..]
                .find(recover_call)
                .expect("recovery statement must call recover_after_restart()");
        assert!(
            recovery_call - recovery < 200,
            "recover_after_restart() must be part of the anchored statement"
        );

        for marker in [
            concat!(
                "let http_listener = tokio::net::TcpListener::bind(",
                "config.http_bind)"
            ),
            concat!("let orchestrator = ", "Orchestrator::new("),
            concat!(
                "let mut migration_reaper = chv_controlplane_service::",
                "MigrationReaper::new("
            ),
        ] {
            assert_eq!(
                source.matches(marker).count(),
                1,
                "startup marker {marker:?} must appear exactly once"
            );
            let position = source
                .find(marker)
                .unwrap_or_else(|| panic!("startup marker {marker:?} must exist"));
            assert!(
                recovery < position,
                "restart recovery must run before {marker:?}"
            );
        }
    }

    #[tokio::test]
    async fn compat_matrix_query_failure_when_opt_out_continues() {
        // Operator has NOT opted in: in build_service that means the env var
        // CHV_COMPAT_MATRIX_PATH is unset, so check_compat_matrix is never
        // called. We model "opt-out" by simply not invoking the function and
        // asserting that build_service's surrounding flow is unaffected.
        //
        // The pure equivalent we can test directly is: when the path does
        // not exist (e.g., CHV_COMPAT_MATRIX_PATH points at a not-yet-deployed
        // file), we warn-and-continue rather than fail. This matches the
        // documented "configured-but-not-yet-deployed is OK" behavior and
        // preserves the contract that an unset env var means no gate.
        let pool = empty_pool().await;
        let missing_path =
            std::path::PathBuf::from("/nonexistent/compat-matrix-do-not-create.toml");

        let result = check_compat_matrix(&pool, &missing_path).await;
        assert!(
            result.is_ok(),
            "missing matrix file must NOT fail boot (opt-out / not-yet-deployed path), got: {result:?}"
        );
    }
}
