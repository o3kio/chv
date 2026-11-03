//! Integration tests for the Phase 6 Architecture Designer drift handler
//! (`POST /v1/architectures/drift`).
//!
//! Mirrors the harness in `tests/architectures_apply.rs` (in-memory SQLite,
//! direct handler invocation, no HTTP layer) but exercises the drift
//! detection path end-to-end against an injected `ManualClock`. Tests
//! cover:
//!
//! * happy clean path (baseline matches snapshot → NoDrift)
//! * drifted path (baseline expects bridge but live has no bridge)
//! * cache TTL — within 5 min returns cache_hit, beyond 5 min recomputes
//! * force_refresh skips cache
//! * check_failed when baseline yaml is unparsable
//! * 404 on unknown architecture
//! * findings_json round-trip through the persisted row
//! * metrics emission contract
//! * tracing emission contract

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use axum::extract::State;
use axum::Json;
use chrono::{Duration, TimeZone, Utc};
use chv_common::ManualClock;
use chv_controlplane_store::{
    AlertRepository, AlertRuleRepository, ApplyRunRepository, BackupRepository,
    DesiredStateRepository, DriftReportRepository, EventRepository, ImageRepository,
    NetworkRepository, NodeRepository, NotificationOutboxRepository, ObservedStateRepository,
    OperationRepository, TopologyRepository,
};
use chv_controlplane_types::architecture::{ArchitectureId, DriftStatus};
use chv_webui_bff::auth::{BearerToken, Claims};
use chv_webui_bff::handlers::architectures::{
    create_architecture, get_architecture_drift, CreateArchitectureRequest, DriftRequest,
    DRIFT_CACHE_TTL_SECS,
};
use chv_webui_bff::mutations::MutationService;
use chv_webui_bff::{AppState, BffError};
use sqlx::sqlite::SqlitePoolOptions;

// ---------------------------------------------------------------------------
// Test scaffolding
// ---------------------------------------------------------------------------

/// MutationService stub — drift handler does not touch mutations, but the
/// AppState carries an `Arc<dyn MutationService>` that has to be satisfied.
struct NoopMutations;

#[async_trait]
impl MutationService for NoopMutations {
    async fn mutate_vm(
        &self,
        _vm_id: String,
        _action: String,
        _force: bool,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        unreachable!("mutate_vm not used")
    }
    async fn migrate_vm(
        &self,
        _vm_id: String,
        _target_node_id: String,
        _pause_first: bool,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        unreachable!("migrate_vm not used")
    }
    async fn snapshot_vm(
        &self,
        _vm_id: String,
        _destination: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        unreachable!("snapshot_vm not used")
    }
    async fn restore_snapshot(
        &self,
        _vm_id: String,
        _source: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        unreachable!("restore_snapshot not used")
    }
    async fn mutate_node(
        &self,
        _node_id: String,
        _action: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateNodeResponse, BffError> {
        unreachable!("mutate_node not used")
    }
    async fn mutate_volume(
        &self,
        _volume_id: String,
        _action: String,
        _force: bool,
        _resize_bytes: Option<u64>,
        _vm_id: Option<String>,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!("mutate_volume not used")
    }
    async fn snapshot_volume(
        &self,
        _volume_id: String,
        _snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!("snapshot_volume not used")
    }
    async fn restore_volume_snapshot(
        &self,
        _volume_id: String,
        _snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!("restore_volume_snapshot not used")
    }
    async fn delete_volume_snapshot(
        &self,
        _volume_id: String,
        _snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!("delete_volume_snapshot not used")
    }
    async fn mutate_network(
        &self,
        _network_id: String,
        _action: String,
        _force: bool,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateNetworkResponse, BffError> {
        unreachable!("mutate_network not used")
    }
    async fn clone_volume(
        &self,
        _volume_id: String,
        _new_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!("clone_volume not used")
    }
}

fn t0() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 15, 10, 0, 0).unwrap()
}

async fn build_state_with_clock(clock: ManualClock) -> AppState {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    chv_controlplane_store::run_migrations(&pool, None)
        .await
        .expect("run migrations");

    AppState {
        pool: pool.clone(),
        node_repo: NodeRepository::new(pool.clone()),
        operation_repo: OperationRepository::new(pool.clone()),
        event_repo: EventRepository::new(pool.clone()),
        alert_repo: AlertRepository::new(pool.clone()),
        alert_rules: std::sync::Arc::new(AlertRuleRepository::new(pool.clone())),
        notification_outbox: std::sync::Arc::new(NotificationOutboxRepository::new(pool.clone())),
        alerting_max_rules: 200,
        notification_channels: chv_webui_bff::NotificationChannels {
            webhook: false,
            slack: false,
        },
        desired_state_repo: DesiredStateRepository::new(pool.clone()),
        observed_state_repo: ObservedStateRepository::new(pool.clone()),
        backup_repo: BackupRepository::new(pool.clone()),
        topology_repo: TopologyRepository::new(pool.clone()),
        network_repo: NetworkRepository::new(pool.clone()),
        image_repo: ImageRepository::new(pool.clone()),
        apply_runs: Arc::new(ApplyRunRepository::new(pool.clone())),
        drift_reports: Arc::new(DriftReportRepository::new(pool.clone())),
        netbox_config: Arc::new(
            chv_controlplane_store::NetboxProjectionConfigRepository::new(pool.clone()),
        ),
        netbox_runs: Arc::new(chv_controlplane_store::NetboxProjectionRunRepository::new(
            pool.clone(),
        )),
        mutations: Arc::new(NoopMutations),
        jwt_secret: "test-secret".to_string(),
        console_jwt_secret: "test-secret".to_string(),
        monitoring: None,
        monitoring_health: chv_webui_bff::MonitoringHealth::new(),
        agent_runtime_dir: std::path::PathBuf::from("/var/lib/chv/agent"),
        cache: chv_webui_bff::BffCache::new(5),
        clock: Arc::new(clock),
    }
}

fn claims_for(role: &str) -> Claims {
    Claims {
        sub: "u-tester".to_string(),
        username: "tester".to_string(),
        role: role.to_string(),
        exp: u64::MAX / 2,
        must_change_password: false,
        aud: None,
        token_version: None,
    }
}

fn err_status(e: &BffError) -> u16 {
    match e {
        BffError::BadRequest(_) => 400,
        BffError::Unauthorized(_) => 401,
        BffError::Forbidden(_) => 403,
        BffError::NotFound(_) => 404,
        BffError::Conflict(_) => 409,
        BffError::TooManyRequests(_) => 429,
        BffError::MonitoringUnavailable(_) => 503,
        BffError::MonitoringQuery { .. } => 400,
        BffError::Internal(_) => 500,
        BffError::NotImplemented(_) => 501,
        BffError::QuotaExceeded { .. } => 422,
        BffError::GraphEmpty => 422,
        BffError::PlanExpired { .. } => 409,
        BffError::PlanNotDiscardable { .. } => 409,
        BffError::MissingConfirmation { .. } => 400,
        BffError::WarningsNotAcknowledged { .. } => 400,
        BffError::PlanNotApplicable { .. } => 409,
        BffError::ProductionRequiresAdmin { .. } => 403,
        BffError::PlanModeMismatch { .. } => 400,
        BffError::InvalidResourceName { .. } => 400,
        BffError::DriftCheckFailed { .. } => 502,
        BffError::NetboxNotConfigured { .. } => 404,
        BffError::NetboxNotConfiguredPrecondition { .. } => 400,
        BffError::NetboxNotApplied { .. } => 400,
        BffError::NetboxHttpsRequired => 400,
        BffError::NetboxTokenMissing { .. } => 400,
        BffError::NetboxRunActive { .. } => 409,
        BffError::ProjectionRunNotRetryable { .. } => 409,
        BffError::NetboxStaleVersion { .. } => 409,
        BffError::NetboxUnreachable { .. } => 502,
        BffError::NetboxAuthFailed { .. } => 502,
    }
}

/// Baseline YAML that declares neither networks nor datastores nor servers
/// — with no live nodes / networks / datastores either, the drift compute
/// returns a clean `NoDrift` report.
const CLEAN_YAML: &str = r#"apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: clean
"#;

/// Baseline YAML that declares one network with a `bridge`. The live
/// `NetworkRepository` returns `bridge: None` for any seeded network row,
/// so naming the same network in live snapshot triggers a single
/// `DRIFT_NETWORK_CHANGED{field=bridge}` finding.
const NETWORK_BRIDGE_YAML: &str = r#"apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: net-bridge
networks:
  - name: edge-1
    type: bridge
    bridge: br0
"#;

async fn create_arch_with_yaml(state: &AppState, name: &str, yaml: &str) -> String {
    create_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(CreateArchitectureRequest {
            name: name.to_string(),
            description: None,
            environment: None,
            display_name: None,
            design_graph_json: None,
            latest_yaml: Some(yaml.to_string()),
        }),
    )
    .await
    .expect("create")
    .0
    .architecture
    .id
}

async fn seed_live_network(state: &AppState, name: &str) {
    sqlx::query(
        r#"INSERT INTO networks (network_id, display_name, network_class)
           VALUES (?1, ?1, 'bridge')"#,
    )
    .bind(name)
    .execute(&state.pool)
    .await
    .unwrap();
}

// ---------------------------------------------------------------------------
// 1. happy clean path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn drift_returns_no_drift_when_baseline_matches_snapshot() {
    let clock = ManualClock::new(t0());
    let state = build_state_with_clock(clock).await;
    let arch_id = create_arch_with_yaml(&state, "clean-1", CLEAN_YAML).await;

    let resp = get_architecture_drift(
        BearerToken(claims_for("viewer")),
        State(state),
        Json(DriftRequest {
            id: arch_id.clone(),
            force_refresh: false,
        }),
    )
    .await
    .expect("drift should compute")
    .0;

    assert_eq!(resp.status, DriftStatus::NoDrift);
    assert!(
        resp.findings.is_empty(),
        "expected empty findings for clean baseline"
    );
    assert_eq!(resp.summary.total, 0);
    assert!(!resp.cache_hit, "first call must not be cache_hit");
    assert!(resp.error_message.is_none());
    assert!(
        resp.drift_report_id.is_some(),
        "fresh compute persists a row"
    );
}

// ---------------------------------------------------------------------------
// 2. drifted: bridge declared in baseline but live has none
// ---------------------------------------------------------------------------
//
// `NETWORK_BRIDGE_YAML` declares `bridge: br0` for `edge-1`. The live
// `NetworkRepository` row seeded by `seed_live_network` has no `bridge`
// column populated, so `live.bridge` is `None`. This is the single most
// reliable trigger for a `DRIFT_NETWORK_CHANGED{field=bridge}` finding in
// this test harness — the test name reflects that exact scenario.

#[tokio::test]
async fn drift_returns_drifted_when_bridge_declared_but_live_has_none() {
    let clock = ManualClock::new(t0());
    let state = build_state_with_clock(clock).await;
    let arch_id = create_arch_with_yaml(&state, "net-bridge-1", NETWORK_BRIDGE_YAML).await;
    seed_live_network(&state, "edge-1").await;

    let resp = get_architecture_drift(
        BearerToken(claims_for("viewer")),
        State(state),
        Json(DriftRequest {
            id: arch_id.clone(),
            force_refresh: false,
        }),
    )
    .await
    .expect("drift should compute")
    .0;

    assert_eq!(resp.status, DriftStatus::Drifted);
    assert_eq!(resp.summary.total, resp.findings.len() as i64);

    // Pin the exact NetworkChanged{field=bridge} finding rather than
    // settling for "any code containing DRIFT_NETWORK_CHANGED" — the
    // baseline declares `bridge: br0` while the live row has no bridge
    // column populated, so the wire shape must surface
    // expected=Some("br0") and actual=None.
    use chv_architecture_reconcile::drift::DriftFinding;
    let net_finding = resp
        .findings
        .iter()
        .find_map(|f| match f {
            DriftFinding::NetworkChanged {
                field,
                expected,
                actual,
                ..
            } if field == "bridge" => Some((expected.clone(), actual.clone())),
            _ => None,
        })
        .expect("expected NetworkChanged on the bridge field");
    assert_eq!(net_finding.0.as_deref(), Some("br0"));
    assert_eq!(net_finding.1, None);
}

// ---------------------------------------------------------------------------
// 3. force_refresh skips cache
// ---------------------------------------------------------------------------

#[tokio::test]
async fn drift_force_refresh_skips_cache() {
    // Anchor the manual clock to wall-clock — the cache hit branch on the
    // second call needs `clock.now()` to be within TTL of the SQLite
    // `created_at` stamp. See `drift_cache_returns_cached_within_5_minutes`
    // for the full rationale.
    let clock = ManualClock::new(Utc::now());
    let state = build_state_with_clock(clock.clone()).await;
    let arch_id = create_arch_with_yaml(&state, "force-1", CLEAN_YAML).await;

    let first = get_architecture_drift(
        BearerToken(claims_for("viewer")),
        State(state.clone()),
        Json(DriftRequest {
            id: arch_id.clone(),
            force_refresh: false,
        }),
    )
    .await
    .expect("first compute")
    .0;
    assert!(!first.cache_hit, "first call computes fresh");

    // Second call without force_refresh inside TTL: cache hit.
    let cached = get_architecture_drift(
        BearerToken(claims_for("viewer")),
        State(state.clone()),
        Json(DriftRequest {
            id: arch_id.clone(),
            force_refresh: false,
        }),
    )
    .await
    .expect("cached read")
    .0;
    assert!(cached.cache_hit, "second call within TTL must be cache_hit");
    assert_eq!(cached.drift_report_id, first.drift_report_id);

    // Third call with force_refresh: fresh compute despite TTL.
    let fresh = get_architecture_drift(
        BearerToken(claims_for("viewer")),
        State(state),
        Json(DriftRequest {
            id: arch_id,
            force_refresh: true,
        }),
    )
    .await
    .expect("forced refresh")
    .0;
    assert!(!fresh.cache_hit, "force_refresh must bypass cache");
    assert_ne!(
        fresh.drift_report_id, cached.drift_report_id,
        "force_refresh persists a new row"
    );
}

// ---------------------------------------------------------------------------
// 4. cache returns cached within TTL
// ---------------------------------------------------------------------------

#[tokio::test]
async fn drift_cache_returns_cached_within_5_minutes() {
    // Anchor the manual clock to the runner's wall clock so it agrees
    // with SQLite's `strftime('now')`-stamped created_at column. The
    // drift cache is a wall-clock TTL; using a fixed `t0()` would race
    // with the runner's actual clock and the comparison would be
    // dominated by that skew, not the test-driven `clock.advance`.
    let clock = ManualClock::new(Utc::now());
    let state = build_state_with_clock(clock.clone()).await;
    let arch_id = create_arch_with_yaml(&state, "cache-1", CLEAN_YAML).await;

    let first = get_architecture_drift(
        BearerToken(claims_for("viewer")),
        State(state.clone()),
        Json(DriftRequest {
            id: arch_id.clone(),
            force_refresh: false,
        }),
    )
    .await
    .expect("first compute")
    .0;
    let first_id = first.drift_report_id.clone().expect("fresh row id");

    // Advance the clock to just inside the TTL window.
    clock.advance(Duration::seconds(60));

    let cached = get_architecture_drift(
        BearerToken(claims_for("viewer")),
        State(state),
        Json(DriftRequest {
            id: arch_id,
            force_refresh: false,
        }),
    )
    .await
    .expect("cached read")
    .0;
    assert!(cached.cache_hit, "within-TTL must be cache_hit");
    assert_eq!(cached.drift_report_id.as_deref(), Some(first_id.as_str()));
}

// ---------------------------------------------------------------------------
// 5. cache expires after TTL
// ---------------------------------------------------------------------------

#[tokio::test]
async fn drift_cache_expires_after_5_minutes() {
    // Anchor the manual clock to the runner's wall clock — see the
    // companion `drift_cache_returns_cached_within_5_minutes` test for
    // the rationale. The expire-after-TTL behavior depends on the BFF
    // clock advancing past `created_at + DRIFT_CACHE_TTL_SECS`.
    let clock = ManualClock::new(Utc::now());
    let state = build_state_with_clock(clock.clone()).await;
    let arch_id = create_arch_with_yaml(&state, "expire-1", CLEAN_YAML).await;

    let first = get_architecture_drift(
        BearerToken(claims_for("viewer")),
        State(state.clone()),
        Json(DriftRequest {
            id: arch_id.clone(),
            force_refresh: false,
        }),
    )
    .await
    .expect("first compute")
    .0;
    let first_id = first.drift_report_id.clone().expect("fresh row id");

    // Advance well past the 5-minute TTL window. Using 301 seconds is a
    // tight margin that races SQLite's seconds-truncated `created_at`
    // when the runner is heavily loaded, so we pick a comfortable
    // multiple of TTL.
    clock.advance(Duration::seconds(900));

    let after = get_architecture_drift(
        BearerToken(claims_for("viewer")),
        State(state),
        Json(DriftRequest {
            id: arch_id,
            force_refresh: false,
        }),
    )
    .await
    .expect("post-TTL compute")
    .0;
    assert!(!after.cache_hit, "post-TTL call must recompute");
    assert_ne!(
        after.drift_report_id.as_deref(),
        Some(first_id.as_str()),
        "post-TTL persist creates a new row"
    );
}

// ---------------------------------------------------------------------------
// 6. check_failed when baseline yaml is unparsable
// ---------------------------------------------------------------------------
//
// The simplest reliable trigger for the `check_failed` path is a baseline
// whose YAML cannot be parsed. The handler then persists a `check_failed`
// drift report and returns 200 with `error_message` populated, exactly
// matching the spec: "if snapshot capture or YAML parse fails, persist a
// `DriftReport { status: CheckFailed, error_message: Some(err) }` and
// return 200 with that report".

#[tokio::test]
async fn drift_persists_check_failed_when_baseline_yaml_unparsable() {
    let clock = ManualClock::new(t0());
    let state = build_state_with_clock(clock).await;
    let bad_yaml = "this::is:not:valid::yaml: [unclosed";
    let arch_id = create_arch_with_yaml(&state, "bad-yaml-1", bad_yaml).await;

    let resp = get_architecture_drift(
        BearerToken(claims_for("viewer")),
        State(state.clone()),
        Json(DriftRequest {
            id: arch_id.clone(),
            force_refresh: false,
        }),
    )
    .await
    .expect("check_failed must return 200, not error")
    .0;

    assert_eq!(resp.status, DriftStatus::CheckFailed);
    assert!(resp.findings.is_empty(), "check_failed has no findings");
    assert!(
        resp.error_message.is_some(),
        "check_failed must surface error_message"
    );
    assert!(
        resp.drift_report_id.is_some(),
        "check_failed row is persisted"
    );
    assert!(!resp.cache_hit);

    // Verify the row is in the DB and round-trips.
    let arch = ArchitectureId::new(arch_id).unwrap();
    let history = state
        .drift_reports
        .list_for_architecture(&arch)
        .await
        .expect("list history");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].status, DriftStatus::CheckFailed);
}

// ---------------------------------------------------------------------------
// 7. unknown architecture → 404
// ---------------------------------------------------------------------------

#[tokio::test]
async fn drift_unknown_architecture_returns_404() {
    let clock = ManualClock::new(t0());
    let state = build_state_with_clock(clock).await;

    let err = get_architecture_drift(
        BearerToken(claims_for("viewer")),
        State(state),
        Json(DriftRequest {
            id: "does-not-exist".to_string(),
            force_refresh: false,
        }),
    )
    .await
    .expect_err("unknown id must 404");
    assert_eq!(err_status(&err), 404);
}

// ---------------------------------------------------------------------------
// 8. findings_json round-trips through the persisted row
// ---------------------------------------------------------------------------

#[tokio::test]
async fn drift_persists_findings_json_round_trips() {
    let clock = ManualClock::new(t0());
    let state = build_state_with_clock(clock).await;
    let arch_id = create_arch_with_yaml(&state, "round-1", NETWORK_BRIDGE_YAML).await;
    seed_live_network(&state, "edge-1").await;

    let resp = get_architecture_drift(
        BearerToken(claims_for("viewer")),
        State(state.clone()),
        Json(DriftRequest {
            id: arch_id.clone(),
            force_refresh: false,
        }),
    )
    .await
    .expect("compute")
    .0;
    let report_id = resp
        .drift_report_id
        .clone()
        .expect("persisted row required");

    // Read the row directly from the store and deserialize the findings JSON.
    let arch = ArchitectureId::new(arch_id).unwrap();
    let rows = state
        .drift_reports
        .list_for_architecture(&arch)
        .await
        .expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id.as_str(), report_id);
    let stored_findings: Vec<chv_architecture_reconcile::drift::DriftFinding> =
        serde_json::from_str(rows[0].findings_json.as_deref().expect("findings_json"))
            .expect("findings_json round-trip");
    assert_eq!(
        stored_findings, resp.findings,
        "persisted findings must equal the response findings"
    );
}

// ---------------------------------------------------------------------------
// 9. metrics: chv_architecture_drift_total{status=...} >= 1
// ---------------------------------------------------------------------------

static METRICS_HANDLE: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();

fn ensure_metrics_installed() -> &'static metrics_exporter_prometheus::PrometheusHandle {
    METRICS_HANDLE.get_or_init(|| {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _ = metrics::set_global_recorder(recorder);
        handle
    })
}

#[tokio::test]
async fn drift_increments_metrics_per_status() {
    let handle = ensure_metrics_installed();
    // Anchor to wall clock so the second call hits the cache (see
    // `drift_cache_returns_cached_within_5_minutes`).
    let clock = ManualClock::new(Utc::now());
    let state = build_state_with_clock(clock.clone()).await;
    let arch_id = create_arch_with_yaml(&state, "metrics-1", CLEAN_YAML).await;

    // Fresh compute → no_drift counter.
    let _ = get_architecture_drift(
        BearerToken(claims_for("viewer")),
        State(state.clone()),
        Json(DriftRequest {
            id: arch_id.clone(),
            force_refresh: false,
        }),
    )
    .await
    .expect("first compute");

    // Second call within TTL → cache_hit counter.
    let _ = get_architecture_drift(
        BearerToken(claims_for("viewer")),
        State(state),
        Json(DriftRequest {
            id: arch_id,
            force_refresh: false,
        }),
    )
    .await
    .expect("cached read");

    let scrape = handle.render();
    assert!(
        scrape.contains("chv_architecture_drift_total"),
        "metrics scrape missing chv_architecture_drift_total. Output:\n{scrape}"
    );
    assert!(
        scrape.contains("status=\"no_drift\"") || scrape.contains("status=\"unknown\""),
        "metrics scrape missing no_drift label. Output:\n{scrape}"
    );
    assert!(
        scrape.contains("status=\"cache_hit\""),
        "metrics scrape missing cache_hit label. Output:\n{scrape}"
    );
}

// ---------------------------------------------------------------------------
// 10. tracing emission contract
// ---------------------------------------------------------------------------

static TRACE_BUF: OnceLock<Arc<std::sync::Mutex<Vec<u8>>>> = OnceLock::new();
static TRACE_INSTALLED: OnceLock<()> = OnceLock::new();

#[derive(Clone)]
struct MakeBuf(Arc<std::sync::Mutex<Vec<u8>>>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for MakeBuf {
    type Writer = BufWriter;
    fn make_writer(&'a self) -> Self::Writer {
        BufWriter(self.0.clone())
    }
}

struct BufWriter(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for BufWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn install_tracing_subscriber() -> Arc<std::sync::Mutex<Vec<u8>>> {
    let buf = TRACE_BUF
        .get_or_init(|| Arc::new(std::sync::Mutex::new(Vec::new())))
        .clone();
    TRACE_INSTALLED.get_or_init(|| {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        let writer = MakeBuf(buf.clone());
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .with_target(true)
            .with_ansi(false);
        let _ = tracing_subscriber::Registry::default()
            .with(layer)
            .try_init();
    });
    buf
}

#[tokio::test]
async fn drift_emits_structured_tracing() {
    let buf = install_tracing_subscriber();
    let baseline = buf.lock().unwrap().len();

    let clock = ManualClock::new(t0());
    let state = build_state_with_clock(clock).await;
    let arch_id = create_arch_with_yaml(&state, "trace-1", CLEAN_YAML).await;
    let _ = get_architecture_drift(
        BearerToken(claims_for("viewer")),
        State(state),
        Json(DriftRequest {
            id: arch_id,
            force_refresh: false,
        }),
    )
    .await
    .expect("compute");

    let captured = {
        let guard = buf.lock().unwrap();
        String::from_utf8(guard[baseline..].to_vec()).expect("utf8")
    };
    assert!(
        captured.contains("architecture.drift"),
        "expected architecture.drift target in trace output, got:\n{captured}"
    );
    assert!(
        captured.contains("architecture.drift.invoked"),
        "expected drift.invoked event:\n{captured}"
    );
    assert!(
        captured.contains("architecture.drift.computed")
            || captured.contains("architecture.drift.cache_hit")
            || captured.contains("architecture.drift.failed"),
        "expected one of the drift outcome events:\n{captured}"
    );
}

// ---------------------------------------------------------------------------
// 11. M6: at-the-boundary cache TTL tests
// ---------------------------------------------------------------------------
//
// The cache check in `get_architecture_drift` is a strict `<` comparison
// (`age_secs < DRIFT_CACHE_TTL_SECS`), so the boundary semantics are:
//
//     age == TTL - 1 → cache_hit
//     age == TTL     → recompute (NOT cache_hit)
//
// SQLite stamps `created_at` at wall-clock at row insert (truncated to
// whole seconds). We anchor the clock with a wall-clock `Utc::now()` so the
// signed duration uses comparable timestamps, then read the persisted row
// to get its actual `created_at` and call `clock.set(created_at + delta)`
// to land on a precise age. This sidesteps the millisecond skew between
// `ManualClock::new(Utc::now())` and the SQLite-stamped insert time.

async fn read_latest_created_at(state: &AppState, arch: &ArchitectureId) -> chrono::DateTime<Utc> {
    let history = state
        .drift_reports
        .list_for_architecture(arch)
        .await
        .expect("list");
    history
        .first()
        .expect("at least one row after fresh compute")
        .created_at
}

#[tokio::test]
async fn drift_cache_returns_cached_at_ttl_minus_one_sec() {
    let clock = ManualClock::new(Utc::now());
    let state = build_state_with_clock(clock.clone()).await;
    let arch_id = create_arch_with_yaml(&state, "boundary-in-1", CLEAN_YAML).await;

    let first = get_architecture_drift(
        BearerToken(claims_for("viewer")),
        State(state.clone()),
        Json(DriftRequest {
            id: arch_id.clone(),
            force_refresh: false,
        }),
    )
    .await
    .expect("first compute")
    .0;
    let first_id = first.drift_report_id.clone().expect("fresh row id");

    // Pin the clock to exactly `created_at + (TTL - 1)` seconds — strictly
    // inside the TTL window. The strict `<` check must accept this.
    let arch = ArchitectureId::new(arch_id.clone()).unwrap();
    let created_at = read_latest_created_at(&state, &arch).await;
    clock.set(created_at + Duration::seconds(DRIFT_CACHE_TTL_SECS - 1));

    let cached = get_architecture_drift(
        BearerToken(claims_for("viewer")),
        State(state),
        Json(DriftRequest {
            id: arch_id,
            force_refresh: false,
        }),
    )
    .await
    .expect("cached read")
    .0;
    assert!(
        cached.cache_hit,
        "age == TTL - 1 must be served from cache (strict < bound)"
    );
    assert_eq!(cached.drift_report_id.as_deref(), Some(first_id.as_str()));
}

#[tokio::test]
async fn drift_cache_expires_at_exactly_ttl() {
    let clock = ManualClock::new(Utc::now());
    let state = build_state_with_clock(clock.clone()).await;
    let arch_id = create_arch_with_yaml(&state, "boundary-out-1", CLEAN_YAML).await;

    let first = get_architecture_drift(
        BearerToken(claims_for("viewer")),
        State(state.clone()),
        Json(DriftRequest {
            id: arch_id.clone(),
            force_refresh: false,
        }),
    )
    .await
    .expect("first compute")
    .0;
    let first_id = first.drift_report_id.clone().expect("fresh row id");

    // Pin the clock to exactly `created_at + TTL` seconds — at the boundary
    // the strict `<` check must reject and force a recompute.
    let arch = ArchitectureId::new(arch_id.clone()).unwrap();
    let created_at = read_latest_created_at(&state, &arch).await;
    clock.set(created_at + Duration::seconds(DRIFT_CACHE_TTL_SECS));

    let after = get_architecture_drift(
        BearerToken(claims_for("viewer")),
        State(state),
        Json(DriftRequest {
            id: arch_id,
            force_refresh: false,
        }),
    )
    .await
    .expect("post-TTL compute")
    .0;
    assert!(
        !after.cache_hit,
        "age == TTL must recompute (strict < bound)"
    );
    assert_ne!(
        after.drift_report_id.as_deref(),
        Some(first_id.as_str()),
        "boundary recompute must persist a new row"
    );
}

// ---------------------------------------------------------------------------
// 12. M8: drift persists check_failed when latest_yaml is empty/None
// ---------------------------------------------------------------------------

#[tokio::test]
async fn drift_persists_check_failed_when_latest_yaml_is_empty() {
    let clock = ManualClock::new(t0());
    let state = build_state_with_clock(clock).await;

    // Create an architecture with `latest_yaml: None`. The handler's empty-
    // yaml branch reports a `check_failed` row with an `error_message`
    // mentioning "no latest_yaml".
    let arch_id = create_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(CreateArchitectureRequest {
            name: "no-yaml-1".to_string(),
            description: None,
            environment: None,
            display_name: None,
            design_graph_json: None,
            latest_yaml: None,
        }),
    )
    .await
    .expect("create")
    .0
    .architecture
    .id;

    let resp = get_architecture_drift(
        BearerToken(claims_for("viewer")),
        State(state),
        Json(DriftRequest {
            id: arch_id,
            force_refresh: false,
        }),
    )
    .await
    .expect("check_failed must return 200, not error")
    .0;

    assert_eq!(resp.status, DriftStatus::CheckFailed);
    let msg = resp
        .error_message
        .as_deref()
        .expect("check_failed must surface error_message");
    assert!(
        msg.contains("no latest_yaml") || msg.contains("nothing to drift-check"),
        "error_message should mention missing latest_yaml; got: {msg}"
    );
    assert!(resp.findings.is_empty());
    assert!(resp.drift_report_id.is_some(), "row must be persisted");
}

// ---------------------------------------------------------------------------
// 13. C3 regression: PermissionChanged fires for viewer, not admin
// ---------------------------------------------------------------------------
//
// Before this fix, all three production drift call sites in
// `handlers::architectures` hardcoded `deploy_allowed_for_caller: true`,
// which made the `DRIFT_PERMISSION_CHANGED` heuristic in
// `chv-architecture-reconcile::drift::compute` dead code outside unit tests.
// The fix plumbs the caller's role through `caller_can_apply(&claims)` so
// the heuristic reflects the live caller. These two tests pin the contract:
//
//   * viewer hitting drift on a baseline that declares roles must see
//     a `DRIFT_PERMISSION_CHANGED` finding
//   * admin/operator must NOT see one
//
// `PERMISSION_BASELINE_YAML` declares an `operator` role with the
// `architecture:apply` permission and binds it to a user — exactly the
// shape `compute_drift` keys off via `baseline_expects_permissions`.

const PERMISSION_BASELINE_YAML: &str = r#"apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: perm-1
roles:
  - name: operator
    permissions:
      - architecture:apply
users:
  - name: alice
    roles:
      - operator
"#;

#[tokio::test]
async fn drift_emits_permission_changed_for_viewer_when_baseline_declares_roles() {
    use chv_architecture_reconcile::drift::DriftFinding;

    let clock = ManualClock::new(t0());
    let state = build_state_with_clock(clock).await;
    let arch_id = create_arch_with_yaml(&state, "perm-viewer-1", PERMISSION_BASELINE_YAML).await;

    let resp = get_architecture_drift(
        BearerToken(claims_for("viewer")),
        State(state),
        Json(DriftRequest {
            id: arch_id,
            force_refresh: false,
        }),
    )
    .await
    .expect("drift should compute")
    .0;

    assert_eq!(
        resp.status,
        DriftStatus::Drifted,
        "viewer with role-bearing baseline must drift on permission gap"
    );
    assert!(
        resp.findings
            .iter()
            .any(|f| matches!(f, DriftFinding::PermissionChanged { .. })),
        "viewer drift findings must include DRIFT_PERMISSION_CHANGED; got {:?}",
        resp.findings.iter().map(|f| f.code()).collect::<Vec<_>>()
    );
    assert_eq!(
        resp.summary
            .by_type
            .get("DRIFT_PERMISSION_CHANGED")
            .copied(),
        Some(1),
        "summary.by_type must count exactly one PermissionChanged finding"
    );
}

#[tokio::test]
async fn drift_does_not_emit_permission_changed_for_admin() {
    use chv_architecture_reconcile::drift::DriftFinding;

    let clock = ManualClock::new(t0());
    let state = build_state_with_clock(clock).await;
    let arch_id = create_arch_with_yaml(&state, "perm-admin-1", PERMISSION_BASELINE_YAML).await;

    let resp = get_architecture_drift(
        BearerToken(claims_for("admin")),
        State(state),
        Json(DriftRequest {
            id: arch_id,
            force_refresh: false,
        }),
    )
    .await
    .expect("drift should compute")
    .0;

    assert!(
        resp.findings
            .iter()
            .all(|f| !matches!(f, DriftFinding::PermissionChanged { .. })),
        "admin drift findings must NOT include DRIFT_PERMISSION_CHANGED; got {:?}",
        resp.findings.iter().map(|f| f.code()).collect::<Vec<_>>()
    );
    assert!(
        !resp
            .summary
            .by_type
            .contains_key("DRIFT_PERMISSION_CHANGED"),
        "summary.by_type must not record PermissionChanged for an admin caller"
    );
}
