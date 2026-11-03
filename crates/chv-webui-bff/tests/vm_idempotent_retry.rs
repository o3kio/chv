//! Integration tests for per-VM idempotency-key retries in the BFF
//! (`POST /v1/vms/delete`, `POST /v1/vms/resize`).
//!
//! Issue kubedoio/chv#406 (found live by the M4.7 fault-matrix run 1,
//! F7): the BFF derives per-VM idempotency keys
//! (`delete-vm-<vm_id>`, `resize-vm-<vm_id>`, ...) and recorded them as
//! plain UNIQUE INSERTs into the shared `operations` table. The M2.5
//! authority-side retention keeps the `vms`/`vm_desired_state` rows
//! after a delete, so a retried operation re-entered the handler with
//! the same key, collided on the UNIQUE constraint, and surfaced as
//! `HTTP 500 INTERNAL_ERROR` (request_id 4d5f9556) instead of a clean
//! idempotent replay.
//!
//! Fixed contract asserted here:
//! - a retried `vm delete` of a retained VM returns the recorded
//!   original outcome (200, same `task_id`) — never a 500 — and does
//!   NOT re-execute the delete (no second operation row, no extra
//!   desired_generation bump);
//! - a retried `vm resize` behaves the same way (same mechanic, same
//!   key class), including a retry with different parameters: the
//!   recorded operation is replayed, the desired state is not touched;
//! - first-operation semantics are unchanged: deleting a VM that never
//!   existed still returns 404 on every attempt (no key is recorded
//!   for a rejected first attempt).

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

/// Seed a user with the given role ('operator'/'admin') and return a
/// usable JWT bearer token.
async fn seed_jwt_as(state: &AppState, role: &str) -> String {
    let user_id = format!("u-{role}");
    sqlx::query(
        "INSERT INTO users (user_id, username, password_hash, role, must_change_password) \
         VALUES (?, ?, 'x', ?, 0)",
    )
    .bind(&user_id)
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
        sub: user_id,
        username: role.to_string(),
        role: role.to_string(),
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

async fn seed_jwt(state: &AppState) -> String {
    seed_jwt_as(state, "operator").await
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

/// Create an operator network; return its generated network_id.
async fn create_network(state: &AppState, token: &str, name: &str, cidr: &str) -> String {
    let (status, body) = post_with_token(
        state.clone(),
        "/v1/networks/create",
        token,
        &format!(r#"{{"name":"{name}","cidr":"{cidr}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "network create body: {body}");
    body["network_id"]
        .as_str()
        .expect("network_id in response")
        .to_string()
}

/// Create a VM attached to the given network; return its generated vm_id.
async fn create_vm_on_network(state: &AppState, token: &str, network_id: &str) -> String {
    let (status, body) = post_with_token(
        state.clone(),
        "/v1/vms/create",
        token,
        &format!(r#"{{"name":"vm-x","image_ref":"/tmp/x.img","network_id":"{network_id}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "vm create body: {body}");
    body["vm_id"].as_str().expect("vm_id").to_string()
}

async fn delete_vm(state: &AppState, token: &str, vm_id: &str) -> (StatusCode, serde_json::Value) {
    post_with_token(
        state.clone(),
        "/v1/vms/delete",
        token,
        &format!(r#"{{"vm_id":"{vm_id}"}}"#),
    )
    .await
}

async fn resize_vm(
    state: &AppState,
    token: &str,
    vm_id: &str,
    cpu_count: i64,
    memory_mb: i64,
) -> (StatusCode, serde_json::Value) {
    post_with_token(
        state.clone(),
        "/v1/vms/resize",
        token,
        &format!(r#"{{"vm_id":"{vm_id}","cpu_count":{cpu_count},"memory_mb":{memory_mb}}}"#),
    )
    .await
}

async fn desired_generation(state: &AppState, vm_id: &str) -> i64 {
    sqlx::query_scalar("SELECT desired_generation FROM vm_desired_state WHERE vm_id = ?")
        .bind(vm_id)
        .fetch_one(&state.pool)
        .await
        .expect("read desired_generation")
}

async fn operation_rows(state: &AppState, idempotency_key: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM operations WHERE idempotency_key = ?")
        .bind(idempotency_key)
        .fetch_one(&state.pool)
        .await
        .expect("count operation rows")
}

/// The exact #406 reproduction shape: first delete 200, second delete
/// of the same (retained) VM must replay the recorded outcome — 200
/// with the recorded task_id — instead of the pre-fix
/// `500 INTERNAL_ERROR`, and must not re-execute the delete.
#[tokio::test]
async fn retried_delete_of_retained_vm_replays_recorded_outcome_not_500() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;
    let net_id = create_network(&state, &token, "net-a", "10.88.0.0/24").await;
    let vm_id = create_vm_on_network(&state, &token, &net_id).await;

    // First delete: accepted.
    let (status, body) = delete_vm(&state, &token, &vm_id).await;
    assert_eq!(status, StatusCode::OK, "first delete body: {body}");
    let first_task_id = body["task_id"].as_str().expect("task_id").to_string();
    assert_eq!(body["vm_id"].as_str(), Some(vm_id.as_str()));

    // M2.5 authority-side retention: the vms row must still exist —
    // that is what makes the retry re-enter the handler with the same
    // per-VM idempotency key.
    let retained: Option<String> = sqlx::query_scalar("SELECT vm_id FROM vms WHERE vm_id = ?")
        .bind(&vm_id)
        .fetch_optional(&state.pool)
        .await
        .expect("check retention");
    assert!(
        retained.is_some(),
        "authority-side retention must keep the vms row after delete"
    );

    let gen_after_first = desired_generation(&state, &vm_id).await;
    let nic_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM vm_nic_desired_state WHERE vm_id = ?")
            .bind(&vm_id)
            .fetch_one(&state.pool)
            .await
            .expect("count nic rows");
    assert_eq!(nic_rows, 0, "first delete removes the nic rows (#356)");

    // THE regression: the retried delete must not 500. It replays the
    // recorded outcome: 200 with the recorded task_id.
    let (status, body) = delete_vm(&state, &token, &vm_id).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "retried delete must replay 200, not 500; body: {body}"
    );
    assert_eq!(
        body["task_id"].as_str(),
        Some(first_task_id.as_str()),
        "retry must replay the recorded task_id"
    );
    assert_eq!(body["vm_id"].as_str(), Some(vm_id.as_str()));
    assert_eq!(body["accepted"].as_bool(), Some(true));

    // The retry must not re-execute: exactly one recorded delete
    // operation, no extra generation bump.
    assert_eq!(
        operation_rows(&state, &format!("delete-vm-{vm_id}")).await,
        1,
        "retry must not insert a second operation row"
    );
    assert_eq!(
        desired_generation(&state, &vm_id).await,
        gen_after_first,
        "retry must not bump desired_generation again"
    );
}

/// A retried resize shares the same per-VM key mechanic and must replay
/// the recorded outcome (200, recorded task_id) without re-running the
/// quota delta or the desired-state UPDATE — including when the retry
/// carries different parameters (the key is per-VM, not per-request).
#[tokio::test]
async fn retried_resize_replays_recorded_outcome_not_500() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;
    let net_id = create_network(&state, &token, "net-b", "10.88.1.0/24").await;
    let vm_id = create_vm_on_network(&state, &token, &net_id).await;

    // First resize: accepted.
    let (status, body) = resize_vm(&state, &token, &vm_id, 4, 2048).await;
    assert_eq!(status, StatusCode::OK, "first resize body: {body}");
    let first_task_id = body["task_id"].as_str().expect("task_id").to_string();
    let gen_after_first = desired_generation(&state, &vm_id).await;

    // Retried resize (same shape an operator or harness issues after a
    // timeout): must replay, not 500.
    let (status, body) = resize_vm(&state, &token, &vm_id, 4, 2048).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "retried resize must replay 200, not 500; body: {body}"
    );
    assert_eq!(
        body["task_id"].as_str(),
        Some(first_task_id.as_str()),
        "retry must replay the recorded task_id"
    );
    assert!(
        body["recorded_status"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "the replay must disclose the recorded operation's status (review round finding): {body}"
    );

    // A second resize with DIFFERENT parameters derives a DIFFERENT
    // args-hashed key (review round: a per-VM key silently swallowed a
    // genuinely different resize by replaying the first outcome): it
    // must execute as a FRESH operation — new task id, desired state
    // updated, generation bumped, and its own operation row.
    let (status, body) = resize_vm(&state, &token, &vm_id, 8, 4096).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "different-args resize must be a fresh accepted op, not 500; body: {body}"
    );
    let second_task_id = body["task_id"].as_str().expect("task_id").to_string();
    assert_ne!(
        second_task_id, first_task_id,
        "different-args resize must be a NEW operation, not a replay"
    );

    let (cpu, mem): (i64, i64) =
        sqlx::query_as("SELECT cpu_count, memory_bytes FROM vm_desired_state WHERE vm_id = ?")
            .bind(&vm_id)
            .fetch_one(&state.pool)
            .await
            .expect("read vm resources");
    assert_eq!(
        cpu, 8,
        "the fresh resize must update the desired state (cpu)"
    );
    assert_eq!(
        mem,
        4096 * 1024 * 1024,
        "the fresh resize must update the desired state (memory)"
    );
    assert!(
        desired_generation(&state, &vm_id).await > gen_after_first,
        "the fresh resize must bump desired_generation"
    );
    assert_eq!(
        operation_rows(
            &state,
            &format!("resize-vm-{vm_id}-8-{}", 4096i64 * 1024 * 1024)
        )
        .await,
        1,
        "the fresh resize must record its own operation row"
    );
    assert_eq!(
        operation_rows(
            &state,
            &format!("resize-vm-{vm_id}-4-{}", 2048i64 * 1024 * 1024)
        )
        .await,
        1,
        "the first resize's key still has exactly one row (the same-args retry did not add one)"
    );
}

/// First-operation semantics are unchanged: a delete of a VM that never
/// existed returns 404 on every attempt (a rejected first attempt
/// records no idempotency key, so the retry re-runs the check).
#[tokio::test]
async fn delete_of_unknown_vm_still_404_on_every_attempt() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;

    for attempt in 1..=2 {
        let (status, body) = delete_vm(&state, &token, "vm-never-existed").await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "attempt {attempt} must stay 404; body: {body}"
        );
    }
    assert_eq!(
        operation_rows(&state, "delete-vm-vm-never-existed").await,
        0,
        "a rejected first delete must not record an idempotency key"
    );
}
