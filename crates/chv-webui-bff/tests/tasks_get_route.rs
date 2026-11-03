//! Integration tests for the single-task get route `POST /v1/tasks/get`
//! (#372 DP6 — the route `chvctl task watch` polls).
//!
//! The CLI's watch command used to poll the LIST route `POST /v1/tasks`
//! with a `task_id` key the handler silently ignored; the response has
//! no top-level `status`, so the loop printed `Status: unknown` forever
//! (issue #372 §2.5). The fix adds this route following the house `/get`
//! convention (`vms/get`, `nodes/get`, `volumes/get`, `networks/get`):
//! 400 on a missing `task_id`, 404 on an unknown one, and the payload
//! nested under a single top-level `detail` key whose item shape is
//! byte-identical to `list_tasks`' items. These tests boot the real
//! `bff_router` and pin that contract:
//!
//! - authentication is required (anonymous request -> 401);
//! - a seeded operation returns its single row under `detail`, with the
//!   list-view field names (`task_id`, `status`, `operation`,
//!   `resource_kind`, `resource_id`, `actor`, `started_unix_ms`,
//!   `finished_unix_ms`);
//! - a missing `task_id` is a 400;
//! - an unknown `task_id` is a 404.

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

/// Seed one operation row (the `list_tasks` seeding shape) — the row
/// `task watch` polls for.
async fn seed_operation(state: &AppState, operation_id: &str, status: &str) {
    sqlx::query(
        "INSERT INTO operations \
         (operation_id, idempotency_key, resource_kind, resource_id, operation_type, status, requested_by, requested_at, created_at, updated_at) \
         VALUES (?, ?, 'vm', 'vm-1', 'CreateVm', ?, 'u-ops', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
    )
    .bind(operation_id)
    .bind(format!("tasks-get-{operation_id}"))
    .bind(status)
    .execute(&state.pool)
    .await
    .expect("seed operation");
}

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

#[tokio::test]
async fn tasks_get_requires_authentication() {
    let state = build_state().await;
    let (status, _) = post_json(state, "/v1/tasks/get", None, r#"{"task_id":"op-1"}"#).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn tasks_get_returns_the_single_row() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_operation(&state, "op-1", "Succeeded").await;

    let (status, body) = post_json(
        state,
        "/v1/tasks/get",
        Some(&token),
        r#"{"task_id":"op-1"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let detail = &body["detail"];
    assert_eq!(detail["task_id"], "op-1");
    // The REAL status vocabulary (`OperationStatus`, capitalized) — the
    // exact string `chvctl task watch` matches against.
    assert_eq!(detail["status"], "Succeeded");
    assert_eq!(detail["operation"], "CreateVm");
    assert_eq!(detail["resource_kind"], "vm");
    assert_eq!(detail["resource_id"], "vm-1");
    assert_eq!(detail["actor"], "u-ops");
    assert!(detail["started_unix_ms"].is_i64());
    assert!(detail.get("finished_unix_ms").is_some());
}

#[tokio::test]
async fn tasks_get_missing_task_id_is_a_400() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    let (status, _) = post_json(state, "/v1/tasks/get", Some(&token), "{}").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn tasks_get_unknown_task_id_is_a_404() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    let (status, _) = post_json(
        state,
        "/v1/tasks/get",
        Some(&token),
        r#"{"task_id":"op-none"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
