//! Integration tests for the native monitoring read API (query/alerts
//! contract v1, #602 PR-2): `GET /v1/monitoring/catalog`, `POST
//! /v1/monitoring/{overview,current,history}`, `GET
//! /v1/monitoring/health`.
//!
//! These boot the real `bff_router` with a real file-backed monitoring
//! store and pin the contract's honesty rules end-to-end:
//!
//! - authentication is required (anonymous → 401) and the viewer role
//!   suffices (reads are viewer-tier; the role check happens before
//!   any target is read);
//! - a degraded/absent store answers `503 MONITORING_UNAVAILABLE` —
//!   which never means nodes or VMs are unhealthy — while `catalog`
//!   (static registry data) and `health` still answer;
//! - a valid point carries a value, a non-valid point carries only its
//!   quality — missing data is never zero;
//! - counter values serialize as decimal strings;
//! - a missing series carries a reason from the absence vocabulary;
//! - typed 400s for unknown metrics and excessive ranges.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chv_common::SystemClock;
use chv_controlplane_store::{
    AlertRepository, AlertRuleRepository, ApplyRunRepository, BackupRepository,
    DesiredStateRepository, DriftReportRepository, EventRepository, ImageRepository,
    NetworkRepository, NodeRepository, NotificationOutboxRepository, ObservedStateRepository,
    OperationRepository, TopologyRepository,
};
use chv_monitoring_core::model::{
    CheckRecord, CheckStatus, SampleBuilder, SampleQuality, SampleValue, Source, TargetKind,
};
use chv_monitoring_store::{
    IngestOutcome, MonitoringHealth, MonitoringStore, MonitoringStoreConfig, NodeBatch,
};
use chv_webui_bff::mutations::MutationService;
use chv_webui_bff::{AppState, BffError};
use serde_json::{json, Value};
use sqlx::sqlite::SqlitePoolOptions;
use tower::ServiceExt;

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
        unreachable!()
    }
    async fn migrate_vm(
        &self,
        _vm_id: String,
        _target_node_id: String,
        _pause_first: bool,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        unreachable!()
    }
    async fn snapshot_vm(
        &self,
        _vm_id: String,
        _destination: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        unreachable!()
    }
    async fn restore_snapshot(
        &self,
        _vm_id: String,
        _destination: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        unreachable!()
    }
    async fn mutate_node(
        &self,
        _node_id: String,
        _action: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateNodeResponse, BffError> {
        unreachable!()
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
        unreachable!()
    }
    async fn snapshot_volume(
        &self,
        _volume_id: String,
        _snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!()
    }
    async fn restore_volume_snapshot(
        &self,
        _volume_id: String,
        _snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!()
    }
    async fn delete_volume_snapshot(
        &self,
        _volume_id: String,
        _snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!()
    }
    async fn clone_volume(
        &self,
        _source_volume_id: String,
        _target_volume_id: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!()
    }
    async fn mutate_network(
        &self,
        _network_id: String,
        _action: String,
        _force: bool,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateNetworkResponse, BffError> {
        unreachable!()
    }
}

async fn build_state(monitoring: Option<Arc<MonitoringStore>>) -> AppState {
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
        agent_runtime_dir: std::path::PathBuf::from("/var/lib/chv/agent"),
        cache: chv_webui_bff::BffCache::new(5),
        clock: Arc::new(SystemClock),
        monitoring,
        monitoring_health: MonitoringHealth::new(),
    }
}

/// Seed a user with the given role and return a usable JWT bearer
/// token — the viewer role proves the routes are viewer-tier.
async fn seed_jwt_as(state: &AppState, role: &str) -> String {
    sqlx::query(
        "INSERT INTO users (user_id, username, password_hash, role, must_change_password) \
         VALUES (?, ?, 'x', ?, 0)",
    )
    .bind(format!("u-{role}"))
    .bind(role)
    .bind(role)
    .execute(&state.pool)
    .await
    .expect("seed user");

    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let claims = chv_webui_bff::auth::Claims {
        sub: format!("u-{role}"),
        username: role.to_string(),
        role: role.to_string(),
        exp,
        must_change_password: false,
        aud: None,
        token_version: None,
    };
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
    jsonwebtoken::encode(
        &header,
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(state.jwt_secret.as_bytes()),
    )
    .expect("encode test token")
}

async fn request(
    state: &AppState,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let req = match body {
        Some(b) => builder
            .header("content-type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = chv_webui_bff::bff_router(state.clone())
        .with_state(state.clone())
        .oneshot(req)
        .await
        .expect("route request");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, json)
}

/// A real file-backed monitoring store in a tempdir, pre-seeded with
/// one node's honest mixed-quality samples:
/// - node.cpu.capacity_ratio: one valid gauge point (0.42);
/// - node.memory.available_bytes: one insufficient_samples point
///   (no value — missing data is never zero);
/// - node.net.rx_bytes_total: two valid counter points (1000 → 3000,
///   same epoch) so history computes a real same-epoch delta.
async fn seeded_store() -> (tempfile::TempDir, Arc<MonitoringStore>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        MonitoringStore::connect(MonitoringStoreConfig {
            database_url: format!("sqlite://{}/monitoring.db", dir.path().display()),
            migrations_dir: std::path::PathBuf::from(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../cmd/chv-controlplane/monitoring-migrations"
            )),
            ..MonitoringStoreConfig::default()
        })
        .await
        .expect("connect monitoring store"),
    );

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let gauge = SampleBuilder::new(
        TargetKind::Node,
        "node-1",
        "node.cpu.capacity_ratio",
        Source::NodeOs,
        now - 30_000,
    )
    .unwrap()
    .value(SampleValue::Float(0.42))
    .build()
    .unwrap();
    let hole = SampleBuilder::new(
        TargetKind::Node,
        "node-1",
        "node.memory.available_bytes",
        Source::NodeOs,
        now - 30_000,
    )
    .unwrap()
    .quality(SampleQuality::InsufficientSamples)
    .build()
    .unwrap();
    let counter = |at_ms: u64, value: u64| {
        SampleBuilder::new(
            TargetKind::Node,
            "node-1",
            "node.net.rx_bytes_total",
            Source::NodeOs,
            at_ms,
        )
        .unwrap()
        .dimension("interface_id", "eth0")
        .unwrap()
        .epoch("boot-1", "iface-eth0")
        .value(SampleValue::Integer(value))
        .build()
        .unwrap()
    };
    let batch = NodeBatch {
        boot_id: "agent-boot-1".to_string(),
        sequence: 0,
        sent_at_ms: now,
        samples: vec![
            gauge,
            hole,
            counter(now - 30_000, 1000),
            counter(now - 15_000, 3000),
        ],
    };
    let outcome = store
        .ingest_node_batch("node-1", &batch, now)
        .await
        .expect("ingest");
    assert!(matches!(outcome, IngestOutcome::Accepted { samples: 4 }));
    (dir, store)
}

#[tokio::test]
async fn monitoring_routes_require_authentication() {
    let state = build_state(None).await;
    let (status, _) = request(&state, "GET", "/v1/monitoring/catalog", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = request(
        &state,
        "POST",
        "/v1/monitoring/history",
        None,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = request(
        &state,
        "POST",
        "/v1/monitoring/checks",
        None,
        Some(json!({"target_kind": "vm", "target_id": "vm-1"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn viewer_role_suffices_for_monitoring_reads() {
    let (_dir, store) = seeded_store().await;
    let state = build_state(Some(store)).await;
    let token = seed_jwt_as(&state, "viewer").await;
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/current",
        Some(&token),
        Some(json!({
            "target_kind": "node",
            "target_id": "node-1",
            "metric_ids": ["node.cpu.capacity_ratio"]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
}

#[tokio::test]
async fn degraded_monitoring_is_a_typed_503_never_node_failure() {
    let state = build_state(None).await;
    let token = seed_jwt_as(&state, "viewer").await;

    // Reads against a degraded store: 503 with the contract's
    // monitoring_unavailable code.
    for (path, body) in [
        (
            "/v1/monitoring/current",
            json!({"target_kind": "node", "target_id": "node-1"}),
        ),
        (
            "/v1/monitoring/history",
            json!({
                "target_kind": "node",
                "target_id": "node-1",
                "metric_ids": ["node.cpu.capacity_ratio"],
                "from_ms": 0,
                "to_ms": 1
            }),
        ),
        (
            "/v1/monitoring/overview",
            json!({"target_kind": "node", "target_ids": ["node-1"]}),
        ),
        (
            "/v1/monitoring/checks",
            json!({"target_kind": "vm", "target_id": "vm-1"}),
        ),
    ] {
        let (status, body_out) = request(&state, "POST", path, Some(&token), Some(body)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "path: {path}");
        assert_eq!(body_out["code"], "MONITORING_UNAVAILABLE");
    }

    // Catalog is static registry data and still answers.
    let (status, body) = request(&state, "GET", "/v1/monitoring/catalog", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["schema_version"], 1);
    let metrics = body["metrics"].as_array().unwrap();
    assert!(metrics
        .iter()
        .any(|m| m["metric_id"] == "node.cpu.capacity_ratio"));

    // Health answers with availability: false and the reason —
    // degraded monitoring is reportable, not a crash.
    let (status, body) = request(&state, "GET", "/v1/monitoring/health", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["available"], false);
}

#[tokio::test]
async fn current_and_history_keep_missing_data_honest() {
    let (_dir, store) = seeded_store().await;
    let state = build_state(Some(store)).await;
    let token = seed_jwt_as(&state, "viewer").await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    // /current: the valid gauge carries a value; the insufficient
    // sample carries only its quality — never zero.
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/current",
        Some(&token),
        Some(json!({
            "target_kind": "node",
            "target_id": "node-1",
            "metric_ids": ["node.cpu.capacity_ratio", "node.memory.available_bytes"]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let samples = body["samples"].as_array().unwrap();
    let cpu = samples
        .iter()
        .find(|s| s["metric_id"] == "node.cpu.capacity_ratio")
        .unwrap();
    assert_eq!(cpu["quality"], "valid");
    assert_eq!(cpu["value"], 0.42);
    let memory = samples
        .iter()
        .find(|s| s["metric_id"] == "node.memory.available_bytes")
        .unwrap();
    assert_eq!(memory["quality"], "insufficient_samples");
    assert!(memory.get("value").is_none() || memory["value"].is_null());

    // /history: counter deltas are decimal strings; the unqueried
    // metric is absent; the response carries the contract envelope.
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/history",
        Some(&token),
        Some(json!({
            "target_kind": "node",
            "target_id": "node-1",
            "metric_ids": ["node.net.rx_bytes_total", "node.cpu.capacity_ratio"],
            "from_ms": now - 60_000,
            "to_ms": now
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["schema_version"], 1);
    assert_eq!(body["truncated"], false);
    let series = body["series"].as_array().unwrap();
    let net = series
        .iter()
        .find(|s| s["metric_id"] == "node.net.rx_bytes_total")
        .unwrap();
    assert_eq!(net["source"], "node_os");
    assert_eq!(net["dimensions"]["interface_id"], "eth0");
    // Same-epoch delta over the window: 3000 - 1000 = 2000, as a
    // decimal string.
    let counter_points: Vec<&Value> = net["points"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["quality"] == "valid")
        .collect();
    assert!(!counter_points.is_empty());
    for point in &counter_points {
        assert!(
            point["integer_value"].is_string(),
            "counter values are decimal strings: {point}"
        );
    }
    assert!(
        counter_points.iter().any(|p| point_delta(p) == 2000),
        "same-epoch counter delta: {counter_points:?}"
    );

    // A series with no stored data for the target returns the
    // contract's missing-series envelope: empty points, a reason, no
    // fabricated source or values.
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/history",
        Some(&token),
        Some(json!({
            "target_kind": "node",
            "target_id": "node-2",
            "metric_ids": ["node.cpu.capacity_ratio"],
            "from_ms": now - 60_000,
            "to_ms": now
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let series = body["series"].as_array().unwrap();
    let missing = series
        .iter()
        .find(|s| s["metric_id"] == "node.cpu.capacity_ratio")
        .unwrap();
    assert_eq!(missing["points"].as_array().unwrap().len(), 0);
    assert_eq!(missing["reason"], "not_collected");
    assert!(missing["source"].is_null());
    assert_eq!(missing["coverage_ratio"], 0.0);
}

fn point_delta(point: &Value) -> i64 {
    point["integer_value"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or(-1)
}

#[tokio::test]
async fn typed_errors_for_unknown_metrics_and_excessive_ranges() {
    let (_dir, store) = seeded_store().await;
    let state = build_state(Some(store)).await;
    let token = seed_jwt_as(&state, "viewer").await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/history",
        Some(&token),
        Some(json!({
            "target_kind": "node",
            "target_id": "node-1",
            "metric_ids": ["node.cpu.not_a_metric"],
            "from_ms": now - 60_000,
            "to_ms": now
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "UNKNOWN_METRIC");

    // 181 days exceeds the aggregated ceiling.
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/history",
        Some(&token),
        Some(json!({
            "target_kind": "node",
            "target_id": "node-1",
            "metric_ids": ["node.cpu.capacity_ratio"],
            "from_ms": now - 181 * 24 * 60 * 60 * 1000,
            "to_ms": now
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "QUERY_TOO_LARGE");

    // Disallowed source for the metric.
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/history",
        Some(&token),
        Some(json!({
            "target_kind": "node",
            "target_id": "node-1",
            "metric_ids": ["node.cpu.capacity_ratio"],
            "sources": ["guest_agent"],
            "from_ms": now - 60_000,
            "to_ms": now
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "UNSUPPORTED_SOURCE");
}

#[tokio::test]
async fn overview_enumerates_targets_and_enforces_the_cap() {
    let (_dir, store) = seeded_store().await;
    let state = build_state(Some(store)).await;
    let token = seed_jwt_as(&state, "viewer").await;

    // No target_ids ⇒ the server enumerates stored targets (bounded).
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/overview",
        Some(&token),
        Some(json!({"target_kind": "node"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let targets = body["targets"].as_array().unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0]["target_id"], "node-1");

    // The contract's 100-target cap is a typed 400.
    let many: Vec<String> = (0..101).map(|i| format!("node-{i}")).collect();
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/overview",
        Some(&token),
        Some(json!({"target_kind": "node", "target_ids": many})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "QUERY_TOO_LARGE");
}

/// The checks inventory route: viewer-tier read of the
/// latest-record-per-check inventory, seeded through the store's own
/// `record_checks` (the guest ingest path that writes it is covered
/// by the controlplane-service tests). The typed status serializes as
/// its string form and staleness is decided server-side.
#[tokio::test]
async fn checks_route_serves_the_latest_record_per_check() {
    let (_dir, store) = seeded_store().await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    store
        .record_checks(
            "agent:a-1",
            &TargetKind::Vm,
            "vm-1",
            &[
                CheckRecord {
                    check_id: "service:nginx.service".to_string(),
                    service_key: Some("nginx.service".to_string()),
                    status: CheckStatus::Warning,
                    summary: Some("active (running)".to_string()),
                    observed_at_ms: now - 30_000,
                },
                CheckRecord {
                    check_id: "http:local:8080".to_string(),
                    service_key: None,
                    status: CheckStatus::Ok,
                    summary: None,
                    observed_at_ms: now - 30_000,
                },
            ],
            now,
        )
        .await
        .expect("seed check inventory");

    let state = build_state(Some(store)).await;
    let token = seed_jwt_as(&state, "viewer").await;

    // Viewer role suffices (the role check happens before the target
    // is read); the response carries the full inventory shape.
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/checks",
        Some(&token),
        Some(json!({"target_kind": "vm", "target_id": "vm-1"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["schema_version"], 1);
    assert_eq!(body["target_kind"], "vm");
    assert_eq!(body["target_id"], "vm-1");
    let checks = body["checks"].as_array().unwrap();
    assert_eq!(checks.len(), 2, "{checks:?}");
    let nginx = checks
        .iter()
        .find(|c| c["check_id"] == "service:nginx.service")
        .unwrap();
    assert_eq!(nginx["status"], "warning", "typed state as its string form");
    assert_eq!(nginx["service_key"], "nginx.service");
    assert_eq!(nginx["summary"], "active (running)");
    assert_eq!(nginx["agent_id"], "agent:a-1");
    assert_eq!(nginx["received_at_ms"], json!(now));
    assert_eq!(
        nginx["stale"], false,
        "30s-old check is fresh (180s window)"
    );

    // A target with no recorded checks: honest absence, empty list.
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/checks",
        Some(&token),
        Some(json!({"target_kind": "vm", "target_id": "vm-404"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["checks"].as_array().unwrap().len(), 0);

    // Only vm and node carry check inventory in v1 — other parseable
    // kinds are a typed 400, as is a non-parsing kind.
    for kind in ["volume", "datacenter"] {
        let (status, body) = request(
            &state,
            "POST",
            "/v1/monitoring/checks",
            Some(&token),
            Some(json!({"target_kind": kind, "target_id": "x-1"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "kind: {kind}");
        assert_eq!(body["code"], "INVALID_TARGET_KIND", "kind: {kind}");
    }
}
