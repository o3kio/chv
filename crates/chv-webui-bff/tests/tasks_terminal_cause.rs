//! Integration tests for the #502 terminal-failure-cause surfacing —
//! `error_code`/`error_message` on every BFF response that carries
//! operation status.
//!
//! The fast-fail work (#498, #500) journals a genuine diagnostic to
//! `operations` on terminal failure (`error_code` ∈
//! {UNSUPPORTED_BY_AGENT, DISPATCH_FAILED, AGENT_REJECTED,
//! MIGRATION_FAILED, ...} plus the agents' verbatim refusal text in
//! `error_message`), but no BFF endpoint surfaced those columns — an
//! operator watching a failed op saw a bare `Failed` everywhere and
//! needed DB access or a log grep for the cause. The maintainer
//! adopted Option 1 (issue #502, 2026-10-06): surface the fields.
//!
//! These tests pin the contract on every operation-carrying arm:
//!
//! - `POST /v1/tasks` (list items) and `POST /v1/tasks/get` (detail) —
//!   the tasks endpoints;
//! - `GET /v1/tasks/stream` (the SSE items);
//! - the per-resource recent-task listings (`volumes/get`, `nodes/get`,
//!   `vms/get`) and `POST /v1/overview`'s recent-tasks panel.
//!
//! The pass-through discipline, pinned per arm: a FAILED op with a
//! recorded cause surfaces `error_code`/`error_message` verbatim; a
//! SUCCEEDED (or RUNNING) op surfaces both as NULL; a terminal op with
//! NO recorded cause surfaces NULL — never a fabricated placeholder
//! like `"UNKNOWN"`.

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
use chv_webui_bff::mutations::MutationService;
use chv_webui_bff::{AppState, BffError};
use sqlx::sqlite::SqlitePoolOptions;
use tokio_stream::StreamExt as _;
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
        _source: String,
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

async fn build_state() -> AppState {
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
        monitoring: None,
        monitoring_health: chv_webui_bff::MonitoringHealth::new(),
        agent_runtime_dir: std::path::PathBuf::from("/var/lib/chv/agent"),
        cache: chv_webui_bff::BffCache::new(5),
        clock: Arc::new(SystemClock),
    }
}

/// Seed an operator and return a usable JWT bearer token.
async fn seed_jwt(state: &AppState) -> String {
    sqlx::query(
        "INSERT INTO users (user_id, username, password_hash, role, must_change_password) \
         VALUES ('u-ops', 'ops', 'x', 'operator', 0)",
    )
    .execute(&state.pool)
    .await
    .expect("seed user");

    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let claims = chv_webui_bff::auth::Claims {
        sub: "u-ops".to_string(),
        username: "ops".to_string(),
        role: "operator".to_string(),
        exp,
        must_change_password: false,
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

/// Seed one operation row. `cause` is the `(error_code, error_message)`
/// pair bound verbatim (None journals NULL) — the pass-through under
/// test.
async fn seed_operation(
    state: &AppState,
    operation_id: &str,
    resource_kind: &str,
    resource_id: &str,
    operation_type: &str,
    status: &str,
    cause: Option<(&str, &str)>,
) {
    sqlx::query(
        "INSERT INTO operations \
         (operation_id, idempotency_key, resource_kind, resource_id, operation_type, status, \
          requested_by, error_code, error_message, requested_at, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, 'u-ops', ?, ?, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
    )
    .bind(operation_id)
    .bind(format!("terminal-cause-{operation_id}"))
    .bind(resource_kind)
    .bind(resource_id)
    .bind(operation_type)
    .bind(status)
    .bind(cause.map(|(code, _)| code))
    .bind(cause.map(|(_, message)| message))
    .execute(&state.pool)
    .await
    .expect("seed operation");
}

/// The #498/#500 fast-fail shape the surfacing exists for: a terminal
/// `Failed` row carrying `UNSUPPORTED_BY_AGENT` plus the agent's own
/// refusal text.
const REFUSAL_TEXT: &str = "snapshot_volume is unsupported in core-managed mode";

async fn post_json(
    state: AppState,
    path: &str,
    token: Option<&str>,
    body: &str,
) -> (StatusCode, serde_json::Value) {
    let app = chv_webui_bff::bff_router(state.clone()).with_state(state);
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    let req = builder.body(Body::from(body.to_string())).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, body)
}

/// The shared per-row assertions: a failed op with a recorded cause
/// surfaces it verbatim; non-terminal/succeeded ops and cause-less
/// terminal ops surface NULL — never a fabricated placeholder.
fn assert_cause_surfaced(item: &serde_json::Value) {
    assert_eq!(item["error_code"].as_str(), Some("UNSUPPORTED_BY_AGENT"));
    assert_eq!(item["error_message"].as_str(), Some(REFUSAL_TEXT));
}

fn assert_no_cause(item: &serde_json::Value, operation_id: &str) {
    assert!(
        item.get("error_code").is_some() && item["error_code"].is_null(),
        "{operation_id}: error_code key must be present and NULL, not absent or fabricated"
    );
    assert!(
        item.get("error_message").is_some() && item["error_message"].is_null(),
        "{operation_id}: error_message key must be present and NULL, not absent or fabricated"
    );
}

#[tokio::test]
async fn tasks_list_surfaces_the_terminal_cause() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    // The three shapes the pass-through must distinguish:
    // - a terminal Failed row WITH the recorded fast-fail diagnostic;
    // - a Succeeded row (the columns are NULL);
    // - a terminal Failed row with NO recorded cause (older writer) —
    //   the fields surface NULL, never "UNKNOWN" or any placeholder.
    seed_operation(
        &state,
        "op-failed",
        "vm",
        "vm-1",
        "SnapshotVm",
        "Failed",
        Some(("UNSUPPORTED_BY_AGENT", REFUSAL_TEXT)),
    )
    .await;
    seed_operation(&state, "op-ok", "vm", "vm-1", "CreateVm", "Succeeded", None).await;
    seed_operation(
        &state,
        "op-failed-no-cause",
        "vm",
        "vm-1",
        "DeleteVm",
        "Failed",
        None,
    )
    .await;

    let (status, body) = post_json(state, "/v1/tasks", Some(&token), "{}").await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 3);

    let by_id = |id: &str| {
        items
            .iter()
            .find(|i| i["task_id"].as_str() == Some(id))
            .unwrap_or_else(|| panic!("item {id} missing"))
    };
    assert_cause_surfaced(by_id("op-failed"));
    assert_no_cause(by_id("op-ok"), "op-ok");
    assert_no_cause(by_id("op-failed-no-cause"), "op-failed-no-cause");
    // A non-terminal row (the list's `active` window member) surfaces
    // neither field either — the running op has nothing recorded.
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_operation(
        &state,
        "op-running",
        "vm",
        "vm-1",
        "CreateVm",
        "Running",
        None,
    )
    .await;
    let (status, body) = post_json(state, "/v1/tasks", Some(&token), "{}").await;
    assert_eq!(status, StatusCode::OK);
    let item = &body["items"][0];
    assert_eq!(item["task_id"].as_str(), Some("op-running"));
    assert_no_cause(item, "op-running");
}

#[tokio::test]
async fn tasks_get_surfaces_the_terminal_cause() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_operation(
        &state,
        "op-failed",
        "vm",
        "vm-1",
        "SnapshotVm",
        "Failed",
        Some(("AGENT_REJECTED", "agent refused: volume is attached")),
    )
    .await;
    seed_operation(&state, "op-ok", "vm", "vm-1", "CreateVm", "Succeeded", None).await;

    let (status, body) = post_json(
        state,
        "/v1/tasks/get",
        Some(&token),
        r#"{"task_id":"op-failed"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let detail = &body["detail"];
    assert_eq!(detail["status"], "Failed");
    assert_eq!(detail["error_code"].as_str(), Some("AGENT_REJECTED"));
    assert_eq!(
        detail["error_message"].as_str(),
        Some("agent refused: volume is attached")
    );

    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_operation(&state, "op-ok", "vm", "vm-1", "CreateVm", "Succeeded", None).await;
    let (status, body) = post_json(
        state,
        "/v1/tasks/get",
        Some(&token),
        r#"{"task_id":"op-ok"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_no_cause(&body["detail"], "op-ok");
}

/// `GET /v1/tasks/stream` (SSE) — the first data frame carries the
/// same two keys on the same pass-through discipline. The stream's
/// window is the last 30 seconds, so the seeded row uses `now`.
#[tokio::test]
async fn tasks_stream_surfaces_the_terminal_cause() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    sqlx::query(
        "INSERT INTO operations \
         (operation_id, idempotency_key, resource_kind, resource_id, operation_type, status, \
          requested_by, error_code, error_message, requested_at, created_at, updated_at) \
         VALUES ('op-sse', 'terminal-cause-op-sse', 'vm', 'vm-1', 'SnapshotVm', 'Failed', 'u-ops', \
                 'UNSUPPORTED_BY_AGENT', ?, \
                 strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'))",
    )
    .bind(REFUSAL_TEXT)
    .execute(&state.pool)
    .await
    .expect("seed sse operation");

    let app = chv_webui_bff::bff_router(state.clone()).with_state(state);
    let req = Request::builder()
        .method("GET")
        .uri("/v1/tasks/stream")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The tick interval's first fire is immediate, so the first data
    // frame is available without waiting a full tick.
    let mut stream = resp.into_body().into_data_stream();
    let frame = tokio::time::timeout(std::time::Duration::from_secs(15), stream.next())
        .await
        .expect("first SSE frame within 15s")
        .expect("stream produced a frame")
        .expect("frame is bytes");
    let text = String::from_utf8(frame.to_vec()).expect("utf8 frame");
    let payload = text
        .trim_start_matches("data:")
        .trim()
        .split('\n')
        .find_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .expect("first frame parses as a data payload");
    let item = &payload["items"][0];
    assert_eq!(item["task_id"].as_str(), Some("op-sse"));
    assert_cause_surfaced(item);
}

#[tokio::test]
async fn volume_detail_recent_tasks_surface_the_terminal_cause() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('n-1', 'h-1', 'Node 1')",
    )
    .execute(&state.pool)
    .await
    .expect("seed node");
    sqlx::query(
        "INSERT INTO volumes (volume_id, node_id, display_name, owner_id, capacity_bytes, updated_at) \
         VALUES ('vol-1', 'n-1', 'disk-1', 'u-ops', 10737418240, '2026-01-01T00:00:00Z')",
    )
    .execute(&state.pool)
    .await
    .expect("seed volume");
    seed_operation(
        &state,
        "op-vol-failed",
        "volume",
        "vol-1",
        "SnapshotVolume",
        "Failed",
        Some(("UNSUPPORTED_BY_AGENT", REFUSAL_TEXT)),
    )
    .await;
    seed_operation(
        &state,
        "op-vol-ok",
        "volume",
        "vol-1",
        "CreateVolume",
        "Succeeded",
        None,
    )
    .await;

    let (status, body) = post_json(
        state,
        "/v1/volumes/get",
        Some(&token),
        r#"{"volume_id":"vol-1"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tasks = body["summary"]["recent_tasks"]
        .as_array()
        .expect("recent_tasks array");
    assert_eq!(tasks.len(), 2);
    let by_id = |id: &str| {
        tasks
            .iter()
            .find(|t| t["task_id"].as_str() == Some(id))
            .unwrap_or_else(|| panic!("recent task {id} missing"))
    };
    assert_cause_surfaced(by_id("op-vol-failed"));
    assert_no_cause(by_id("op-vol-ok"), "op-vol-ok");
}

#[tokio::test]
async fn node_detail_recent_tasks_surface_the_terminal_cause() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('n-1', 'h-1', 'Node 1')",
    )
    .execute(&state.pool)
    .await
    .expect("seed node");
    seed_operation(
        &state,
        "op-node-failed",
        "node",
        "n-1",
        "UpdateOverlay",
        "Failed",
        Some((
            "DISPATCH_FAILED",
            "permanently failed after 3 retries: node unreachable",
        )),
    )
    .await;

    let (status, body) =
        post_json(state, "/v1/nodes/get", Some(&token), r#"{"node_id":"n-1"}"#).await;
    assert_eq!(status, StatusCode::OK);
    let tasks = body["recentTasks"].as_array().expect("recentTasks array");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["error_code"].as_str(), Some("DISPATCH_FAILED"));
    assert_eq!(
        tasks[0]["error_message"].as_str(),
        Some("permanently failed after 3 retries: node unreachable")
    );
}

#[tokio::test]
async fn vm_detail_recent_tasks_surface_the_terminal_cause() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('n-1', 'h-1', 'Node 1')",
    )
    .execute(&state.pool)
    .await
    .expect("seed node");
    sqlx::query(
        "INSERT INTO vms (vm_id, node_id, display_name, owner_id) VALUES ('vm-1', 'n-1', 'VM 1', 'u-ops')",
    )
    .execute(&state.pool)
    .await
    .expect("seed vm");
    seed_operation(
        &state,
        "op-vm-failed",
        "vm",
        "vm-1",
        "MigrateVm",
        "Failed",
        Some((
            "MIGRATION_FAILED",
            "migration aborted: dirty block rate never converged",
        )),
    )
    .await;

    let (status, body) = post_json(state, "/v1/vms/get", Some(&token), r#"{"vm_id":"vm-1"}"#).await;
    assert_eq!(status, StatusCode::OK);
    let tasks = body["summary"]["recent_tasks"]
        .as_array()
        .expect("recent_tasks array");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["error_code"].as_str(), Some("MIGRATION_FAILED"));
    assert_eq!(
        tasks[0]["error_message"].as_str(),
        Some("migration aborted: dirty block rate never converged")
    );
}

#[tokio::test]
async fn overview_recent_tasks_surface_the_terminal_cause() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_operation(
        &state,
        "op-ov-failed",
        "vm",
        "vm-1",
        "SnapshotVm",
        "Failed",
        Some(("UNSUPPORTED_BY_AGENT", REFUSAL_TEXT)),
    )
    .await;

    let (status, body) = post_json(state, "/v1/overview", Some(&token), "{}").await;
    assert_eq!(status, StatusCode::OK);
    let tasks = body["recent_tasks"].as_array().expect("recent_tasks array");
    assert_eq!(tasks.len(), 1);
    assert_cause_surfaced(&tasks[0]);
}
