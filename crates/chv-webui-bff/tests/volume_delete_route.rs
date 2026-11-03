//! Integration tests for the #522 PR 2 BFF route: `POST
//! /v1/volumes/delete` — the first producer of the PR 1 `DeleteVolume`
//! dispatch carrier (#534), and the platform's first data-destroying
//! operator surface.
//!
//! These tests boot the real `bff_router` and pin the adopted design's
//! PR-2 matrix (docs/design/issue-522-volume-delete-api.md §6):
//!
//! - **happy path** — the journaled row shapes: the TOMBSTONE
//!   (`volume_desired_state.desired_status = 'Deleting'`, generation
//!   bumped, `updated_by` the deleter — the `volumes` row is NOT
//!   deleted, DP1's claim-time class resolution / clone replay
//!   rationale) plus an `Accepted` `DeleteVolume` operation with the
//!   `delete-volume-{volume_id}` idempotency key (the arm PR 1
//!   landed), and the volume read surfaces rendering "Deleting";
//! - **DP5 attached guard** — an attached volume → 400 naming the
//!   mutate-detach path, no force flag, ZERO journaling; the
//!   refinement: an attachment to a VM whose own desired_status is
//!   'Deleting' does NOT count (volumes of deleted VMs are deletable);
//! - **DP6 kind gate** — only `volume_kind = 'data'` is deletable;
//!   NULL-kind (boot disks / VM-embedded) → 400 naming the VM
//!   lifecycle;
//! - **DP7 reference guards** — an in-flight operation → 409 (a
//!   terminal one does NOT block), an enabled backup schedule → 400
//!   naming the schedule (a disabled one does not block);
//! - **DP10 core-managed** — 400 at accept, failing open on
//!   unreported/legacy nodes;
//! - **ownership** — `require_volume_owner` inside the transaction
//!   (non-owner → 403, ownerless → admin-only);
//! - **#406 idempotent retry** — a retried delete replays the recorded
//!   outcome without re-bumping the generation or journaling a second
//!   operation;
//! - **task watch** — the DeleteVolume operation surfaces through
//!   `POST /v1/tasks/get`;
//! - **DP8 sibling verbs** — mutate/snapshot/restore-snapshot/
//!   delete-snapshot/clone refuse a 'Deleting' volume before reaching
//!   the mutation service.

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
use chv_webui_bff::auth::Claims;
use chv_webui_bff::mutations::MutationService;
use chv_webui_bff::{AppState, BffError};
use serde_json::Value;
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
        console_jwt_secret: "test-secret".to_string(),
        monitoring: None,
        monitoring_health: chv_webui_bff::MonitoringHealth::new(),
        agent_runtime_dir: std::path::PathBuf::from("/var/lib/chv/agent"),
        cache: chv_webui_bff::BffCache::new(5),
        clock: Arc::new(SystemClock),
    }
}

fn token_for(state: &AppState, sub: &str, role: &str) -> String {
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let claims = Claims {
        sub: sub.to_string(),
        username: format!("user-{sub}"),
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

/// Seed one enrolled node with NO inventory row (never reported an
/// authority mode — the DP10 check fails open).
async fn seed_node(state: &AppState, node_id: &str) {
    sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES (?, 'h', 'h')")
        .bind(node_id)
        .execute(&state.pool)
        .await
        .expect("seed node");
}

/// Seed one enrolled node plus an inventory row with an optional
/// authority mode.
async fn seed_node_inventory(state: &AppState, node_id: &str, authority_mode: Option<&str>) {
    seed_node(state, node_id).await;
    sqlx::query(
        "INSERT INTO node_inventory (node_id, architecture, cpu_count, memory_bytes, storage_classes, authority_mode) \
         VALUES (?, 'x86_64', 1, 1024, '[\"local\"]', ?)",
    )
    .bind(node_id)
    .bind(authority_mode)
    .execute(&state.pool)
    .await
    .expect("seed node inventory");
}

/// Seed a VM with an explicit desired_status (the DP5 refinement's
/// 'Deleting' leg needs one) and a requester.
async fn seed_vm(state: &AppState, vm_id: &str, requested_by: &str, desired_status: &str) {
    sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES (?, ?)")
        .bind(vm_id)
        .bind(vm_id)
        .execute(&state.pool)
        .await
        .expect("insert vm");
    sqlx::query(
        "INSERT INTO vm_desired_state (vm_id, desired_generation, desired_status, requested_by, cpu_count, memory_bytes) \
         VALUES (?, 1, ?, ?, 0, 0)",
    )
    .bind(vm_id)
    .bind(desired_status)
    .bind(requested_by)
    .execute(&state.pool)
    .await
    .expect("insert vm_desired_state");
}

/// Seed a deletable standalone volume: `volume_kind = 'data'` (DP6's
/// gate), an owner, a capacity, and an `Active` desired state at
/// generation 1 — optionally attached to a VM (DP5's guard input).
async fn seed_data_volume(
    state: &AppState,
    volume_id: &str,
    owner_id: Option<&str>,
    attached_vm_id: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO volumes (volume_id, node_id, display_name, owner_id, capacity_bytes, volume_kind, updated_at) \
         VALUES (?, 'n-1', ?, ?, 1073741824, 'data', strftime('%Y-%m-%dT%H:%M:%SZ','now'))",
    )
    .bind(volume_id)
    .bind(volume_id)
    .bind(owner_id)
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

/// Seed an operation row for a volume with an explicit status (DP7's
/// in-flight guard input).
async fn seed_volume_operation(
    state: &AppState,
    operation_id: &str,
    volume_id: &str,
    operation_type: &str,
    status: &str,
) {
    sqlx::query(
        "INSERT INTO operations (operation_id, idempotency_key, resource_kind, resource_id, operation_type, status, requested_by) \
         VALUES (?, ?, 'volume', ?, ?, ?, 'u-operator')",
    )
    .bind(operation_id)
    .bind(format!("seed-{operation_id}"))
    .bind(volume_id)
    .bind(operation_type)
    .bind(status)
    .execute(&state.pool)
    .await
    .expect("insert operation");
}

/// The state a rejected delete must leave untouched: the `volumes` row
/// (never deleted — DP1), the `volume_desired_state` row (status and
/// generation — the tombstone is the only writer), and the
/// `operations` table (no DeleteVolume row). The three-table loop of
/// the create-route suite, delete-shaped: a rejection must journal
/// nothing anywhere a delete would write.
async fn assert_delete_journaled_nothing(state: &AppState, volume_id: &str) {
    let volumes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM volumes WHERE volume_id = ?")
        .bind(volume_id)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(
        volumes, 1,
        "a rejected delete must not remove the volumes row"
    );
    let vds: (i64, Option<String>) = sqlx::query_as(
        "SELECT desired_generation, desired_status FROM volume_desired_state WHERE volume_id = ?",
    )
    .bind(volume_id)
    .fetch_one(&state.pool)
    .await
    .expect("volume desired state row");
    assert_eq!(
        vds.1.as_deref(),
        Some("Active"),
        "a rejected delete must not tombstone the desired state"
    );
    assert_eq!(vds.0, 1, "a rejected delete must not bump the generation");
    let delete_ops: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM operations WHERE operation_type = 'DeleteVolume' AND resource_id = ?",
    )
    .bind(volume_id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(
        delete_ops, 0,
        "a rejected delete must not journal a DeleteVolume operation"
    );
}

async fn post(state: &AppState, token: &str, path: &str, body: &str) -> (StatusCode, Value) {
    let app = chv_webui_bff::bff_router(state.clone()).with_state(state.clone());
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
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, body)
}

/// POST /v1/volumes/delete as the operator `sub`.
async fn delete_as(state: &AppState, sub: &str, volume_id: &str) -> (StatusCode, Value) {
    let token = token_for(state, sub, "operator");
    post(
        state,
        &token,
        "/v1/volumes/delete",
        &format!(r#"{{"volume_id":"{volume_id}"}}"#),
    )
    .await
}

// ─────────────────────────────────────────────────────────────────────
// Happy path — the design's row shapes
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn happy_path_journals_the_design_row_shapes() {
    let state = build_state().await;
    seed_node(&state, "n-1").await;

    // Create through the real route so the delete operates on the
    // exact rows create journals (kind 'data', Pending, generation 1),
    // then resolve the create so DP7's in-flight guard does not (and
    // must not) hold the delete — a volume whose create is terminal is
    // deletable, the DP8 no-state-gating stance.
    let token = token_for(&state, "u-operator", "operator");
    let (status, body) = post(
        &state,
        &token,
        "/v1/volumes/create",
        r#"{"name":"data-vol","node_id":"n-1","capacity_bytes":1073741824}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "volume create body: {body}");
    let volume_id = body["volume_id"].as_str().expect("volume_id").to_string();
    sqlx::query("UPDATE operations SET status = 'Succeeded' WHERE resource_id = ?")
        .bind(&volume_id)
        .execute(&state.pool)
        .await
        .expect("resolve create");

    let (status, body) = delete_as(&state, "u-operator", &volume_id).await;
    assert_eq!(status, StatusCode::OK, "volume delete body: {body}");

    // The response shape mirrors the create route / delete_vm (DP1).
    assert_eq!(body["accepted"], true, "body: {body}");
    let task_id = body["task_id"].as_str().expect("task_id").to_string();
    assert!(!task_id.is_empty(), "body: {body}");
    assert_eq!(body["volume_id"].as_str(), Some(volume_id.as_str()));
    assert!(
        body["summary"]
            .as_str()
            .is_some_and(|s| s.contains("data-vol")),
        "the summary names the volume: {body}"
    );
    assert_eq!(
        body["next_refresh_path"],
        format!("/api/v1/tasks/{task_id}"),
        "body: {body}"
    );

    // The tombstone (DP1): desired_status 'Deleting', generation
    // bumped 1 → 2, updated_by the deleter. The volumes row is NOT
    // deleted — claim-time class resolution and clone replay depend
    // on living rows.
    let vds: (i64, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT desired_generation, desired_status, updated_by FROM volume_desired_state WHERE volume_id = ?",
    )
    .bind(&volume_id)
    .fetch_one(&state.pool)
    .await
    .expect("volume desired state row");
    assert_eq!(vds.0, 2, "the tombstone bumps the generation exactly once");
    assert_eq!(
        vds.1.as_deref(),
        Some("Deleting"),
        "the tombstone's desired_status"
    );
    assert_eq!(
        vds.2.as_deref(),
        Some("u-operator"),
        "updated_by is the deleter"
    );

    let volumes: (Option<String>, Option<String>, Option<i64>) = sqlx::query_as(
        "SELECT volume_kind, owner_id, capacity_bytes FROM volumes WHERE volume_id = ?",
    )
    .bind(&volume_id)
    .fetch_one(&state.pool)
    .await
    .expect("the volumes row persists (DP1)");
    assert_eq!(volumes.0.as_deref(), Some("data"));
    assert_eq!(volumes.1.as_deref(), Some("u-operator"));
    assert_eq!(volumes.2, Some(1073741824));

    // The Accepted DeleteVolume operation — the arm PR 1 landed — with
    // the design's idempotency key, the resource_kind the BFF's volume
    // display surfaces read, the owner stamped, and the bumped
    // generation.
    let op: (String, String, String, String, String, Option<i64>) = sqlx::query_as(
        "SELECT idempotency_key, resource_kind, operation_type, status, requested_by, desired_generation FROM operations WHERE operation_id = ?",
    )
    .bind(&task_id)
    .fetch_one(&state.pool)
    .await
    .expect("operation row");
    assert_eq!(op.0, format!("delete-volume-{volume_id}"));
    assert_eq!(op.1, "volume");
    assert_eq!(op.2, "DeleteVolume");
    assert_eq!(op.3, "Accepted");
    assert_eq!(op.4, "u-operator");
    assert_eq!(op.5, Some(2), "the operation carries the bumped generation");

    // The read surfaces render the tombstone with zero extra work (the
    // COALESCE already prefers vds.desired_status).
    let (status, body) = post(
        &state,
        &token,
        "/v1/volumes/get",
        &format!(r#"{{"volume_id":"{volume_id}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        body.pointer("/summary/status").and_then(|v| v.as_str()),
        Some("Deleting"),
        "the detail surface must render the tombstone: {body}"
    );
}

#[tokio::test]
async fn a_terminally_failed_create_is_deletable() {
    // DP8: no state-machine gating beyond the guards — a volume whose
    // create dispatch failed terminally is exactly the volume
    // operators most need to delete (the artifact may or may not
    // exist; the destroy's absent-artifact-is-success contract makes
    // either case safe).
    let state = build_state().await;
    seed_node(&state, "n-1").await;
    seed_data_volume(&state, "vol-failed", Some("u-operator"), None).await;
    seed_volume_operation(&state, "op-create", "vol-failed", "CreateVolume", "Failed").await;

    let (status, body) = delete_as(&state, "u-operator", "vol-failed").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let status: Option<String> = sqlx::query_scalar(
        "SELECT desired_status FROM volume_desired_state WHERE volume_id = 'vol-failed'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(status.as_deref(), Some("Deleting"));
}

// ─────────────────────────────────────────────────────────────────────
// Auth, ids, ownership
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn requires_authentication_and_operator_role() {
    let state = build_state().await;
    seed_node(&state, "n-1").await;
    seed_data_volume(&state, "vol-auth", Some("u-admin"), None).await;
    let body = r#"{"volume_id":"vol-auth"}"#;

    // No bearer token → 401.
    let app = chv_webui_bff::bff_router(state.clone()).with_state(state.clone());
    let req = Request::builder()
        .method("POST")
        .uri("/v1/volumes/delete")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Viewer role → 403 (operator-or-admin, like the sibling volume routes).
    let viewer = token_for(&state, "u-viewer", "viewer");
    let (status, body_out) = post(&state, &viewer, "/v1/volumes/delete", body).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a viewer must not delete volumes: {body_out}"
    );

    // Admin is allowed (operator-OR-admin).
    let admin = token_for(&state, "u-admin", "admin");
    let (status, body_out) = post(&state, &admin, "/v1/volumes/delete", body).await;
    assert_eq!(status, StatusCode::OK, "admin delete body: {body_out}");

    // The 401/403 attempts journaled nothing (only the admin delete
    // tombstoned).
    let ops: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM operations WHERE operation_type = 'DeleteVolume'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(ops, 1, "only the admin delete journaled an operation");
}

#[tokio::test]
async fn rejects_missing_and_unknown_volume_ids() {
    let state = build_state().await;
    seed_node(&state, "n-1").await;

    // A body with no volume_id at all → 400 naming the field.
    let token = token_for(&state, "u-operator", "operator");
    let (status, body) = post(&state, &token, "/v1/volumes/delete", "{}").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    let body = body.to_string();
    assert!(
        body.contains("volume_id"),
        "the rejection must name the field: {body}"
    );

    // An unknown volume_id → 404 (the delete_vm shape).
    let (status, body) = delete_as(&state, "u-operator", "vol-nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");

    // Zero journaling across the three tables a delete writes.
    for (table, label) in [
        ("volumes", "volumes"),
        ("volume_desired_state", "volume desired state"),
        ("operations", "operations"),
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "a rejected delete must not journal {label}");
    }
}

#[tokio::test]
async fn ownership_is_enforced_inside_the_transaction() {
    let state = build_state().await;
    seed_node(&state, "n-1").await;
    seed_data_volume(&state, "vol-alice", Some("u-alice"), None).await;
    // Pre-#386 legacy shape: no owner stamped.
    seed_data_volume(&state, "vol-legacy", None, None).await;

    // A non-owner operator → 403.
    let (status, body) = delete_as(&state, "u-bob", "vol-alice").await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a non-owner must not delete: {body}"
    );
    assert_delete_journaled_nothing(&state, "vol-alice").await;

    // An ownerless volume is admin-only by design.
    let (status, body) = delete_as(&state, "u-operator", "vol-legacy").await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "an ownerless volume must be admin-only: {body}"
    );
    assert_delete_journaled_nothing(&state, "vol-legacy").await;

    // The owner deletes their own.
    let (status, body) = delete_as(&state, "u-alice", "vol-alice").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
}

// ─────────────────────────────────────────────────────────────────────
// DP5 — the attached guard, and its deleting-VM refinement
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn rejects_attached_volume_naming_the_detach_path() {
    let state = build_state().await;
    seed_node(&state, "n-1").await;
    seed_vm(&state, "vm-live", "u-bob", "Running").await;
    seed_data_volume(&state, "vol-attached", Some("u-operator"), Some("vm-live")).await;

    let (status, body) = delete_as(&state, "u-operator", "vol-attached").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an attached volume must reject at accept: {body}"
    );
    let body = body.to_string();
    assert!(
        body.contains("vm-live"),
        "the rejection must name the attaching VM: {body}"
    );
    assert!(
        body.contains("detach"),
        "the rejection must name the detach-first path: {body}"
    );
    assert!(
        body.contains("/v1/volumes/mutate"),
        "the rejection must name the mutate-detach route: {body}"
    );
    assert!(
        body.contains("no force"),
        "the rejection must state there is no force path (DP5): {body}"
    );
    assert_delete_journaled_nothing(&state, "vol-attached").await;
}

#[tokio::test]
async fn allows_delete_of_a_volume_attached_to_a_deleting_vm() {
    // DP5's refinement, pinned: VM delete tombstones the VM rows and
    // never clears volume VDS attached_vm_id, so the guard treats an
    // attachment to a 'Deleting' VM as not-attached — without this, a
    // volume whose VM was deleted could never be deleted.
    let state = build_state().await;
    seed_node(&state, "n-1").await;
    seed_vm(&state, "vm-dying", "u-bob", "Deleting").await;
    seed_data_volume(&state, "vol-orphaned", Some("u-operator"), Some("vm-dying")).await;

    let (status, body) = delete_as(&state, "u-operator", "vol-orphaned").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a volume attached to a deleting VM IS deletable: {body}"
    );
    let vds: (i64, Option<String>) = sqlx::query_as(
        "SELECT desired_generation, desired_status FROM volume_desired_state WHERE volume_id = 'vol-orphaned'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(vds.1.as_deref(), Some("Deleting"));
    assert_eq!(vds.0, 2);
}

// ─────────────────────────────────────────────────────────────────────
// DP6 — the kind gate
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn rejects_null_kind_volumes_naming_the_vm_lifecycle() {
    // NULL volume_kind = every VM-embedded/boot disk (and every
    // pre-#513 volume). The gate is also the locator gate: the carrier
    // locator would MISS an embedded disk's vm-dir-nested path, so
    // accepting it would tombstone the row and reclaim nothing.
    let state = build_state().await;
    seed_node(&state, "n-1").await;
    sqlx::query(
        "INSERT INTO volumes (volume_id, node_id, display_name, owner_id, capacity_bytes, updated_at) \
         VALUES ('vol-boot', 'n-1', 'vol-boot', 'u-operator', 1073741824, strftime('%Y-%m-%dT%H:%M:%SZ','now'))",
    )
    .execute(&state.pool)
    .await
    .expect("insert embedded volume");
    sqlx::query(
        "INSERT INTO volume_desired_state (volume_id, desired_generation, desired_status, requested_by) \
         VALUES ('vol-boot', 1, 'Active', 'u-operator')",
    )
    .execute(&state.pool)
    .await
    .expect("insert volume_desired_state");

    let (status, body) = delete_as(&state, "u-operator", "vol-boot").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a NULL-kind volume must reject at accept: {body}"
    );
    let body = body.to_string();
    assert!(
        body.contains("data"),
        "the rejection must name the deletable kind: {body}"
    );
    assert!(
        body.contains("VM"),
        "the rejection must name the VM lifecycle: {body}"
    );
    assert_delete_journaled_nothing(&state, "vol-boot").await;
}

// ─────────────────────────────────────────────────────────────────────
// DP7 — the reference guards
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn rejects_in_flight_operations_with_409() {
    let state = build_state().await;
    seed_node(&state, "n-1").await;
    seed_data_volume(&state, "vol-busy", Some("u-operator"), None).await;

    for (op_id, status) in [
        ("op-acc", "Accepted"),
        ("op-run", "Running"),
        // Review S2: a create sitting in dispatch backoff (RetryPending)
        // must block the delete too — its retry would re-provision the
        // store behind the tombstone.
        ("op-retry", "RetryPending"),
    ] {
        seed_volume_operation(&state, op_id, "vol-busy", "SnapshotVolume", status).await;
        let (status, body) = delete_as(&state, "u-operator", "vol-busy").await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "an in-flight operation must 409 (a transient condition, not a validation error): {body}"
        );
        let body = body.to_string();
        assert!(
            body.contains(op_id),
            "the rejection must name the in-flight operation: {body}"
        );
        // Resolve this one so the next iteration's 409 names its own
        // operation (the guard's LIMIT 1 picks either in-flight row).
        sqlx::query("UPDATE operations SET status = 'Succeeded' WHERE operation_id = ?")
            .bind(op_id)
            .execute(&state.pool)
            .await
            .expect("resolve seeded op");
    }
    assert_delete_journaled_nothing(&state, "vol-busy").await;

    // A terminal operation does NOT block — and neither does the
    // volume's own recorded DeleteVolume (the #406 replay, pinned
    // separately below).
    sqlx::query("UPDATE operations SET status = 'Succeeded' WHERE resource_id = 'vol-busy'")
        .execute(&state.pool)
        .await
        .expect("resolve in-flight");
    let (status, body) = delete_as(&state, "u-operator", "vol-busy").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
}

#[tokio::test]
async fn rejects_enabled_backup_schedules_naming_the_schedule() {
    let state = build_state().await;
    seed_node(&state, "n-1").await;
    seed_vm(&state, "vm-1", "u-operator", "Running").await;
    seed_data_volume(&state, "vol-backed", Some("u-operator"), None).await;
    sqlx::query(
        "INSERT INTO backup_schedules (schedule_id, vm_id, volume_id, name, cron_expression, enabled) \
         VALUES ('sched-1', 'vm-1', 'vol-backed', 'nightly', '0 2 * * *', 1)",
    )
    .execute(&state.pool)
    .await
    .expect("seed backup schedule");

    let (status, body) = delete_as(&state, "u-operator", "vol-backed").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an enabled backup schedule must reject the delete: {body}"
    );
    let body = body.to_string();
    assert!(
        body.contains("sched-1"),
        "the rejection must name the schedule: {body}"
    );
    assert!(
        body.contains("/v1/backups/schedules/"),
        "the rejection must name the disable path: {body}"
    );
    assert_delete_journaled_nothing(&state, "vol-backed").await;

    // A disabled schedule does not block (the worker will not claim
    // it) — the guard's clause is "enabled".
    sqlx::query("UPDATE backup_schedules SET enabled = 0 WHERE schedule_id = 'sched-1'")
        .execute(&state.pool)
        .await
        .expect("disable schedule");
    let (status, body) = delete_as(&state, "u-operator", "vol-backed").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
}

// ─────────────────────────────────────────────────────────────────────
// DP10 — core-managed posture (the #378 mirror)
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn rejects_core_managed_node_with_400_and_zero_journaling() {
    let state = build_state().await;
    seed_node_inventory(&state, "n-core", Some("core-managed")).await;
    sqlx::query(
        "INSERT INTO volumes (volume_id, node_id, display_name, owner_id, capacity_bytes, volume_kind, updated_at) \
         VALUES ('vol-core', 'n-core', 'vol-core', 'u-operator', 1073741824, 'data', strftime('%Y-%m-%dT%H:%M:%SZ','now'))",
    )
    .execute(&state.pool)
    .await
    .expect("insert volume");
    sqlx::query(
        "INSERT INTO volume_desired_state (volume_id, desired_generation, desired_status, requested_by) \
         VALUES ('vol-core', 1, 'Active', 'u-operator')",
    )
    .execute(&state.pool)
    .await
    .expect("insert volume_desired_state");

    let (status, body) = delete_as(&state, "u-operator", "vol-core").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a delete targeting a core-managed node must reject at accept: {body}"
    );
    let body = body.to_string();
    assert!(
        body.contains("core-managed"),
        "the rejection must name the core-managed posture: {body}"
    );
    assert_delete_journaled_nothing(&state, "vol-core").await;
}

#[tokio::test]
async fn core_managed_check_fails_open_on_unreported_and_legacy_modes() {
    // get_authority_mode's None (no inventory row) and 'legacy' both
    // fail OPEN — the agent-side fail-closed dispatch (PR 1's handler)
    // remains the enforcement for unreported nodes.
    let state = build_state().await;
    seed_node(&state, "n-noreport").await;
    seed_node_inventory(&state, "n-legacy", Some("legacy")).await;
    for (vol, node) in [("vol-noreport", "n-noreport"), ("vol-legacy", "n-legacy")] {
        sqlx::query(
            "INSERT INTO volumes (volume_id, node_id, display_name, owner_id, capacity_bytes, volume_kind, updated_at) \
             VALUES (?, ?, ?, 'u-operator', 1073741824, 'data', strftime('%Y-%m-%dT%H:%M:%SZ','now'))",
        )
        .bind(vol)
        .bind(node)
        .bind(vol)
        .execute(&state.pool)
        .await
        .expect("insert volume");
        sqlx::query(
            "INSERT INTO volume_desired_state (volume_id, desired_generation, desired_status, requested_by) \
             VALUES (?, 1, 'Active', 'u-operator')",
        )
        .bind(vol)
        .execute(&state.pool)
        .await
        .expect("insert volume_desired_state");

        let (status, body) = delete_as(&state, "u-operator", vol).await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
    }
}

// ─────────────────────────────────────────────────────────────────────
// #406 — idempotent retry
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn idempotent_retry_replays_the_recorded_outcome() {
    // The tombstone keeps the rows, so a retried delete re-enters the
    // handler with the same `delete-volume-{volume_id}` key. The #406
    // contract: replay the recorded outcome — the original task_id —
    // without re-bumping the generation or journaling a second
    // operation. The replay check runs BEFORE the guards, so the
    // delete's own now-in-flight operation cannot 409 the retry.
    let state = build_state().await;
    seed_node(&state, "n-1").await;
    seed_data_volume(&state, "vol-retry", Some("u-operator"), None).await;

    let (status, first) = delete_as(&state, "u-operator", "vol-retry").await;
    assert_eq!(status, StatusCode::OK, "body: {first}");
    let first_task = first["task_id"].as_str().unwrap().to_string();

    let (status, second) = delete_as(&state, "u-operator", "vol-retry").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a retried delete must replay, not 409/500: {second}"
    );
    assert_eq!(
        second["task_id"].as_str(),
        Some(first_task.as_str()),
        "the retry replays the recorded task_id: {second}"
    );
    assert_eq!(
        second["recorded_status"].as_str(),
        Some("Accepted"),
        "the replay surfaces the recorded status: {second}"
    );

    // Exactly one DeleteVolume operation, and the generation bumped
    // exactly once.
    let ops: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM operations WHERE operation_type = 'DeleteVolume' AND resource_id = 'vol-retry'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(ops, 1, "the retry must not journal a second operation");
    let generation: i64 = sqlx::query_scalar(
        "SELECT desired_generation FROM volume_desired_state WHERE volume_id = 'vol-retry'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(generation, 2, "the retry must not re-bump the generation");
}

// ─────────────────────────────────────────────────────────────────────
// Task watch — the DeleteVolume operation on the /v1/tasks/get surface
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn task_watch_surfaces_the_delete_operation() {
    let state = build_state().await;
    seed_node(&state, "n-1").await;
    seed_data_volume(&state, "vol-watch", Some("u-operator"), None).await;

    let (status, body) = delete_as(&state, "u-operator", "vol-watch").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let task_id = body["task_id"].as_str().unwrap().to_string();

    // The route `chvctl task watch` polls (#372 DP6).
    let token = token_for(&state, "u-operator", "operator");
    let (status, body) = post(
        &state,
        &token,
        "/v1/tasks/get",
        &format!(r#"{{"task_id":"{task_id}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        body.pointer("/detail/operation").and_then(|v| v.as_str()),
        Some("DeleteVolume"),
        "the task surface must carry the DeleteVolume operation: {body}"
    );
    assert_eq!(
        body.pointer("/detail/resource_kind")
            .and_then(|v| v.as_str()),
        Some("volume")
    );
    assert_eq!(
        body.pointer("/detail/resource_id").and_then(|v| v.as_str()),
        Some("vol-watch")
    );
    assert_eq!(
        body.pointer("/detail/status").and_then(|v| v.as_str()),
        Some("Accepted")
    );
    assert_eq!(
        body.pointer("/detail/actor").and_then(|v| v.as_str()),
        Some("u-operator")
    );
}

// ─────────────────────────────────────────────────────────────────────
// DP8 — the sibling verbs refuse a 'Deleting' volume
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn sibling_verbs_reject_a_deleting_volume() {
    // Without this guard an attach raced against a delete would
    // create-on-open a fresh default-size file behind the tombstone
    // (the #533 stray-file failure class, manufactured by this
    // design's own window). Every sibling route must refuse BEFORE
    // reaching the mutation service (NoopMutations panics if reached
    // — the guard firing first is the assertion).
    let state = build_state().await;
    seed_node(&state, "n-1").await;
    seed_data_volume(&state, "vol-gone", Some("u-operator"), None).await;
    let (status, body) = delete_as(&state, "u-operator", "vol-gone").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let token = token_for(&state, "u-operator", "operator");
    for (path, body) in [
        (
            "/v1/volumes/mutate",
            r#"{"volume_id":"vol-gone","action":"attach","vm_id":"vm-1"}"#,
        ),
        (
            "/v1/volumes/snapshot",
            r#"{"volume_id":"vol-gone","snapshot_name":"s"}"#,
        ),
        (
            "/v1/volumes/restore-snapshot",
            r#"{"volume_id":"vol-gone","snapshot_name":"s"}"#,
        ),
        (
            "/v1/volumes/delete-snapshot",
            r#"{"volume_id":"vol-gone","snapshot_name":"s"}"#,
        ),
        (
            "/v1/volumes/clone",
            r#"{"source_volume_id":"vol-gone","target_volume_id":"vol-dst"}"#,
        ),
    ] {
        let (status, body_out) = post(&state, &token, path, body).await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "{path} must refuse a 'Deleting' volume: {body_out}"
        );
        let body_out = body_out.to_string();
        assert!(
            body_out.contains("delet"),
            "the rejection must name the delete: {body_out}"
        );
    }

    // The sibling rejections journaled nothing new.
    let ops: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM operations WHERE resource_id = 'vol-gone'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(ops, 1, "only the DeleteVolume operation exists");
}
