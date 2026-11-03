//! Integration tests for the G5 (prompt-06 task 2) BFF payload field:
//! the VM-create payload's optional `vsock_enabled` opt-in for the
//! virtio-vsock monitoring transport.
//!
//! These tests boot the real `bff_router` and pin the accept-time
//! contract:
//!
//! - a payload WITHOUT the field stores 0 — today's behavior
//!   byte-exactly (no device; the opt-in is strictly additive);
//! - a payload with `vsock_enabled: true` persists 1 to
//!   `vm_desired_state.vsock_enabled` (0062), the field the
//!   orchestrator's `build_agent_vm_spec` emits to the agent;
//! - a NON-BOOLEAN value is rejected with HTTP 400 at accept time and
//!   journals nothing (strictly typed — no truthy-string coercion).

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
        _vm_id: String,
        _action: String,
        _force: bool,
        _resize_bytes: Option<u64>,
        _vm_id_param: Option<String>,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!()
    }
    async fn snapshot_volume(
        &self,
        _vm_id: String,
        _snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!()
    }
    async fn restore_volume_snapshot(
        &self,
        _vm_id: String,
        _snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!()
    }
    async fn delete_volume_snapshot(
        &self,
        _vm_id: String,
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
    let user_id = "u-operator";
    sqlx::query(
        "INSERT INTO users (user_id, username, password_hash, role, must_change_password) \
         VALUES (?, 'operator', 'x', 'operator', 0)",
    )
    .bind(user_id)
    .execute(&state.pool)
    .await
    .expect("seed user");

    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let claims = chv_webui_bff::auth::Claims {
        sub: user_id.to_string(),
        username: "operator".to_string(),
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

/// Seed one enrolled node so create_vm has a placement target.
async fn seed_node(state: &AppState) {
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('n-1', 'h-1', 'Node 1')",
    )
    .execute(&state.pool)
    .await
    .expect("seed node");
}

async fn post_with_token(
    state: AppState,
    path: &str,
    token: &str,
    body: &str,
) -> (StatusCode, serde_json::Value) {
    let app = chv_webui_bff::bff_router(state.clone()).with_state(state);
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
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

/// POST /v1/vms/create and return `(status, body)`.
async fn create_vm(state: &AppState, token: &str, body: &str) -> (StatusCode, serde_json::Value) {
    post_with_token(state.clone(), "/v1/vms/create", token, body).await
}

/// The stored vsock_enabled of the (single) VM the create tx minted.
async fn vm_vsock_enabled(state: &AppState, display_name: &str) -> Option<i64> {
    sqlx::query_scalar::<_, Option<i64>>(
        "SELECT vds.vsock_enabled FROM vm_desired_state vds \
         JOIN vms v ON v.vm_id = vds.vm_id WHERE v.display_name = ?",
    )
    .bind(display_name)
    .fetch_one(&state.pool)
    .await
    .expect("vm desired state row")
}

/// Count the rows a create would have journaled — every table the
/// create tx writes. A rejected create must leave all of them at zero.
async fn assert_create_journaled_nothing(state: &AppState) {
    for (table, label) in [
        ("vms", "vms"),
        ("vm_desired_state", "vm desired state"),
        ("volumes", "volumes"),
        ("volume_desired_state", "volume desired state"),
        ("operations", "operations"),
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "a rejected create must not journal {label}");
    }
}

#[tokio::test]
async fn vm_create_without_vsock_stores_off() {
    // The additive default: a payload without the field stores 0 —
    // byte-exactly today's row shape. No device, no silent attach.
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;

    let (status, body) = create_vm(
        &state,
        &token,
        r#"{"name":"vm-bare","image_ref":"/tmp/x.img"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "vm create body: {body}");
    assert_eq!(
        vm_vsock_enabled(&state, "vm-bare").await,
        Some(0),
        "no vsock_enabled in the payload must store 0 (off)"
    );
}

#[tokio::test]
async fn vm_create_with_vsock_persists_the_opt_in() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;

    let (status, body) = create_vm(
        &state,
        &token,
        r#"{"name":"vm-vsock","image_ref":"/tmp/x.img","vsock_enabled":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "vm create body: {body}");
    assert_eq!(
        vm_vsock_enabled(&state, "vm-vsock").await,
        Some(1),
        "vsock_enabled: true must persist to vm_desired_state (0062)"
    );
}

#[tokio::test]
async fn vm_create_rejects_non_boolean_vsock() {
    // Strictly typed: a truthy string is a bad request, not a coercion —
    // and the rejection journals nothing.
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;

    let (status, body) = create_vm(
        &state,
        &token,
        r#"{"name":"vm-bad","image_ref":"/tmp/x.img","vsock_enabled":"yes"}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a non-boolean vsock_enabled must reject: {body}"
    );
    assert_create_journaled_nothing(&state).await;
}
