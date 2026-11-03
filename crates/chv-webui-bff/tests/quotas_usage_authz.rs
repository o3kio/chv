//! Integration tests for `/v1/usage` and `/v1/quotas/:user_id/usage` (CR1).
//!
//! Pre-fix shape: both routes were mounted to a single `get_usage` handler
//! that ignored the `:user_id` path parameter and returned `claims.sub`'s
//! usage unconditionally. An admin querying `/v1/quotas/alice/usage` would
//! get the admin's own usage rather than alice's — silent data correctness
//! violation, authz-shape.
//!
//! Post-fix shape: two distinct handlers.
//! - `POST /v1/usage` → `get_my_usage` (uses `claims.sub`)
//! - `POST /v1/quotas/:user_id/usage` → `get_user_usage` (admin OR self)
//!
//! These tests exercise both routes through the real `bff_router` so we
//! cover routing, role middleware, and the new path-parameter authz check.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

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
use chv_webui_bff::auth::Claims;
use chv_webui_bff::mutations::MutationService;
use chv_webui_bff::{AppState, BffError};
use sqlx::sqlite::SqlitePoolOptions;
use tower::ServiceExt;

/// Stub MutationService — these tests never reach a mutation path.
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
    async fn mutate_network(
        &self,
        _network_id: String,
        _action: String,
        _force: bool,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateNetworkResponse, BffError> {
        unreachable!()
    }
    async fn clone_volume(
        &self,
        _volume_id: String,
        _new_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
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
    chv_controlplane_store::test_util::seed_test_users(&pool).await;
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

fn token_for(state: &AppState, sub: &str, role: &str) -> String {
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_secs()
        + 3600;
    let claims = Claims {
        sub: sub.to_string(),
        username: sub.to_string(),
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

/// Issue a `POST` against the BFF router and return `(status, body_json)`.
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
        .expect("build request");
    let resp = app.oneshot(req).await.expect("router oneshot");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .expect("collect body");
    let body_json: serde_json::Value = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, body_json)
}

/// Insert a VM owned by `requested_by` so `usage.vms` becomes 1 and
/// `cpu_cores` becomes `cpu`. Lets us assert we returned the *target*
/// user's usage rather than the caller's.
async fn seed_vm(state: &AppState, vm_id: &str, requested_by: &str, cpu: i64) {
    sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES (?, ?)")
        .bind(vm_id)
        .bind(vm_id)
        .execute(&state.pool)
        .await
        .expect("insert vm");
    sqlx::query(
        "INSERT INTO vm_desired_state (vm_id, desired_generation, requested_by, cpu_count, memory_bytes)
         VALUES (?, 1, ?, ?, ?)",
    )
    .bind(vm_id)
    .bind(requested_by)
    .bind(cpu)
    .bind(0_i64)
    .execute(&state.pool)
    .await
    .expect("insert vm_desired_state");
}

// ---------------------------------------------------------------------------
// Test 1: GET-shape `/v1/usage` returns the caller's own usage.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn usage_returns_self_for_caller() {
    let state = build_state().await;
    seed_vm(&state, "vm-alice-1", "u-alice", 4).await;
    seed_vm(&state, "vm-bob-1", "u-bob", 16).await;

    let alice = token_for(&state, "u-alice", "operator");
    let (status, body) = post_with_token(state, "/v1/usage", &alice, "{}").await;

    assert_eq!(status, StatusCode::OK, "self-usage must succeed: {body}");
    assert_eq!(
        body.get("user_id").and_then(|v| v.as_str()),
        Some("u-alice"),
        "self-usage must echo caller user_id"
    );
    assert_eq!(
        body.pointer("/usage/vms").and_then(|v| v.as_i64()),
        Some(1),
        "self-usage must return alice's VM count, not bob's: {body}"
    );
    assert_eq!(
        body.pointer("/usage/cpu_cores").and_then(|v| v.as_i64()),
        Some(4),
        "self-usage must return alice's cpu, not bob's: {body}"
    );
}

// ---------------------------------------------------------------------------
// Test 2: `/v1/quotas/:user_id/usage` for *another* user requires admin.
// Operator role hitting another user's path → 403 PermissionDenied.
// (Pre-fix this returned 200 with the operator's own data — the silent leak.)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn usage_for_other_user_requires_admin() {
    let state = build_state().await;
    seed_vm(&state, "vm-bob-1", "u-bob", 8).await;

    let alice = token_for(&state, "u-alice", "operator");
    let (status, body) = post_with_token(state, "/v1/quotas/u-bob/usage", &alice, "{}").await;

    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "operator must not read another user's usage: {body}"
    );
    // Hard guard against the pre-fix silent-leak shape — even if some future
    // change re-introduced a 200, it must not be carrying alice's data
    // labeled as bob's, nor bob's data via insufficient role.
    assert_ne!(
        status,
        StatusCode::OK,
        "no 200 path is permissible for non-admin reading another user's usage"
    );
}

// ---------------------------------------------------------------------------
// Test 3: admin reading another user's usage → 200 with target user's data.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn usage_for_other_user_succeeds_for_admin() {
    let state = build_state().await;
    // admin has nothing of their own, bob has 1 VM with 8 cores.
    seed_vm(&state, "vm-bob-1", "u-bob", 8).await;

    let admin = token_for(&state, "u-admin", "admin");
    let (status, body) = post_with_token(state, "/v1/quotas/u-bob/usage", &admin, "{}").await;

    assert_eq!(status, StatusCode::OK, "admin must succeed: {body}");
    assert_eq!(
        body.get("user_id").and_then(|v| v.as_str()),
        Some("u-bob"),
        "response must echo the *target* user_id, not the admin: {body}"
    );
    assert_eq!(
        body.pointer("/usage/vms").and_then(|v| v.as_i64()),
        Some(1),
        "admin must see bob's VM count (1), not admin's (0): {body}"
    );
    assert_eq!(
        body.pointer("/usage/cpu_cores").and_then(|v| v.as_i64()),
        Some(8),
        "admin must see bob's cpu (8), not admin's (0): {body}"
    );
}

// ---------------------------------------------------------------------------
// Test 4: caller reading their own usage *via the path* still succeeds —
// no admin required when `:user_id == claims.sub`.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn usage_for_self_via_path_succeeds() {
    let state = build_state().await;
    seed_vm(&state, "vm-alice-1", "u-alice", 2).await;

    let alice = token_for(&state, "u-alice", "operator");
    let (status, body) = post_with_token(state, "/v1/quotas/u-alice/usage", &alice, "{}").await;

    assert_eq!(
        status,
        StatusCode::OK,
        "self via path must not require admin: {body}"
    );
    assert_eq!(
        body.get("user_id").and_then(|v| v.as_str()),
        Some("u-alice"),
    );
    assert_eq!(
        body.pointer("/usage/cpu_cores").and_then(|v| v.as_i64()),
        Some(2),
        "self-via-path must return caller's data: {body}"
    );
}

// ---------------------------------------------------------------------------
// #525: the storage meter counts standalone volumes, and the cpu/memory
// sums no longer fan out per attached volume (the pre-#525 shape
// LEFT-JOINed the volume tables into the per-VM sum query, multiplying
// a VM's cpu/memory once per attached volume).
// ---------------------------------------------------------------------------

/// Seed a volume (owner `owner_id`, `capacity_bytes`) attached to
/// `attached_vm_id` — standalone when the attach target is None.
async fn seed_volume(
    state: &AppState,
    volume_id: &str,
    owner_id: &str,
    capacity_bytes: i64,
    attached_vm_id: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO volumes (volume_id, node_id, display_name, owner_id, capacity_bytes, updated_at) \
         VALUES (?, 'n-1', ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%SZ','now'))",
    )
    .bind(volume_id)
    .bind(volume_id)
    .bind(owner_id)
    .bind(capacity_bytes)
    .execute(&state.pool)
    .await
    .expect("insert volume");
    sqlx::query(
        "INSERT INTO volume_desired_state (volume_id, desired_generation, desired_status, requested_by, attached_vm_id) \
         VALUES (?, 1, 'Active', ?, ?)",
    )
    .bind(volume_id)
    .bind(owner_id)
    .bind(attached_vm_id)
    .execute(&state.pool)
    .await
    .expect("insert volume_desired_state");
}

#[tokio::test]
async fn usage_meter_counts_standalone_and_does_not_fan_out_per_volume() {
    let state = build_state().await;
    sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES ('n-1', 'h', 'h')")
        .execute(&state.pool)
        .await
        .expect("seed node");
    // One VM (4 cores, 2 GiB) with TWO attached volumes — the fan-out
    // shape — plus one standalone volume owned by the same user.
    seed_vm(&state, "vm-alice-1", "u-alice", 4).await;
    seed_volume(
        &state,
        "vol-a",
        "u-alice",
        1024 * 1024 * 1024,
        Some("vm-alice-1"),
    )
    .await;
    seed_volume(
        &state,
        "vol-b",
        "u-alice",
        1024 * 1024 * 1024,
        Some("vm-alice-1"),
    )
    .await;
    seed_volume(&state, "vol-solo", "u-alice", 1024 * 1024 * 1024, None).await;

    let alice = token_for(&state, "u-alice", "operator");
    let (status, body) = post_with_token(state, "/v1/usage", &alice, "{}").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    assert_eq!(
        body.pointer("/usage/cpu_cores").and_then(|v| v.as_i64()),
        Some(4),
        "a VM with two attached volumes must still count its CPU once (no join fan-out): {body}"
    );
    assert_eq!(
        body.pointer("/usage/disk_gb").and_then(|v| v.as_i64()),
        Some(3),
        "the meter must count both attached volumes AND the standalone one: {body}"
    );
}
