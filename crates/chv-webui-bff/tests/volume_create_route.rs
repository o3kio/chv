//! Integration tests for the #513 PR 2 BFF route: `POST /v1/volumes/create`
//! — the first production producer of the PR 1 `CreateVolume` dispatch
//! carrier (#523).
//!
//! These tests boot the real `bff_router` and pin the adopted design's
//! PR-2 matrix (docs/design/issue-513-volume-create-api.md §6):
//!
//! - **happy path** — the journaled row shapes: `owner_id` stamped
//!   (`claims.sub`, the #386 lesson), `volume_kind = 'data'` (DP8),
//!   NULL `storage_class` when unnamed (the historical row shape), a
//!   `Pending` `volume_desired_state` with NULL `attached_vm_id`
//!   (DP4 — the volume is born standalone), and an `Accepted`
//!   `CreateVolume` operation with the `create-volume-{volume_id}`
//!   idempotency key (the arm PR 1 landed);
//! - **vocabulary 400** — an unknown `storage_class` rejects against
//!   the one shared list and journals nothing;
//! - **node-capability 400 with ZERO journaling** across
//!   `volumes`/`volume_desired_state`/`operations` (the #516
//!   table-loop assertion pattern), failing OPEN on unreported nodes;
//! - **core-managed 400** (DP7, the #378 mirror) with zero journaling;
//! - **quota rejection** (DP6 — the storage column, 422) with zero
//!   journaling;
//! - **display-name rejection** (the shared traversal guard);
//! - **`attached_vm_id` → 400 naming the mutate-attach path** (DP4's
//!   loud reservation — the `--vlan` lesson);
//! - **`seed_image_ref` → 400** (DP3);
//! - **unauthenticated → 401, viewer → 403** (operator tier, mirroring
//!   the sibling volume mutation routes);
//! - **DP3 field validation** — `node_id` required (no default),
//!   positive `capacity_bytes` within the 64 TiB ceiling.

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

/// Seed one enrolled node with NO inventory row (never reported classes
/// or an authority mode — both node checks fail open).
async fn seed_node(state: &AppState, node_id: &str) {
    sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES (?, 'h', 'h')")
        .bind(node_id)
        .execute(&state.pool)
        .await
        .expect("seed node");
}

/// Seed one enrolled node plus an inventory row advertising exactly
/// these storage classes (JSON array) and, optionally, an authority
/// mode.
async fn seed_node_inventory(
    state: &AppState,
    node_id: &str,
    classes: &[&str],
    authority_mode: Option<&str>,
) {
    seed_node(state, node_id).await;
    sqlx::query(
        "INSERT INTO node_inventory (node_id, architecture, cpu_count, memory_bytes, storage_classes, authority_mode) \
         VALUES (?, 'x86_64', 1, 1024, ?, ?)",
    )
    .bind(node_id)
    .bind(serde_json::to_string(classes).unwrap())
    .bind(authority_mode)
    .execute(&state.pool)
    .await
    .expect("seed node inventory");
}

/// Count the rows a create would have journaled — every table the
/// create tx writes. A rejected create must leave all of them at zero
/// (the `contract.rs` table-loop assertion pattern).
async fn assert_create_journaled_nothing(state: &AppState) {
    for (table, label) in [
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

async fn post(state: &AppState, token: &str, body: &str) -> (StatusCode, serde_json::Value) {
    let app = chv_webui_bff::bff_router(state.clone()).with_state(state.clone());
    let req = Request::builder()
        .method("POST")
        .uri("/v1/volumes/create")
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

/// POST /v1/volumes/create as the operator `u-operator`.
async fn create(state: &AppState, body: &str) -> (StatusCode, serde_json::Value) {
    let token = token_for(state, "u-operator", "operator");
    post(state, &token, body).await
}

// ─────────────────────────────────────────────────────────────────────
// Happy path — the design's row shapes
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn happy_path_journals_the_design_row_shapes() {
    let state = build_state().await;
    // No inventory row: both node checks fail open (the unreported-node
    // discipline, pinned separately below).
    seed_node(&state, "n-1").await;

    let (status, body) = create(
        &state,
        r#"{"name":"data-vol","node_id":"n-1","capacity_bytes":1073741824}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "volume create body: {body}");

    // The response shape mirrors POST /v1/vms (DP1).
    assert_eq!(body["accepted"], true, "body: {body}");
    assert!(
        body["task_id"].as_str().is_some_and(|s| !s.is_empty()),
        "body: {body}"
    );
    let volume_id = body["volume_id"].as_str().expect("volume_id").to_string();
    assert!(!volume_id.is_empty(), "the id is server-minted: {body}");
    assert!(
        body["summary"]
            .as_str()
            .is_some_and(|s| s.contains("data-vol")),
        "body: {body}"
    );
    assert_eq!(
        body["next_refresh_path"],
        format!("/api/v1/tasks/{}", body["task_id"].as_str().unwrap()),
        "body: {body}"
    );

    // volumes row: owner stamped (#386), kind 'data' (DP8), NULL class
    // when unnamed (the historical row shape), placement and capacity
    // verbatim.
    let volume: (String, Option<String>, Option<String>, i64, Option<String>) = sqlx::query_as(
        "SELECT node_id, owner_id, storage_class, capacity_bytes, volume_kind FROM volumes WHERE volume_id = ?",
    )
    .bind(&volume_id)
    .fetch_one(&state.pool)
    .await
    .expect("volume row");
    assert_eq!(volume.0, "n-1");
    assert_eq!(
        volume.1.as_deref(),
        Some("u-operator"),
        "owner_id must be stamped with claims.sub (the #386 lesson)"
    );
    assert_eq!(
        volume.2, None,
        "an unnamed storage_class must journal NULL (never materialized)"
    );
    assert_eq!(volume.3, 1073741824);
    assert_eq!(
        volume.4.as_deref(),
        Some("data"),
        "volume_kind must be stamped 'data' (DP8)"
    );

    // volume_desired_state row: Pending, standalone (NULL
    // attached_vm_id — DP4), generation 1, requested_by the creator.
    let vds: (i64, Option<String>, Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT desired_generation, desired_status, attached_vm_id, requested_by, device_name FROM volume_desired_state WHERE volume_id = ?",
    )
    .bind(&volume_id)
    .fetch_one(&state.pool)
    .await
    .expect("volume desired state row");
    assert_eq!(vds.0, 1, "desired_generation starts at 1");
    assert_eq!(
        vds.1.as_deref(),
        Some("Pending"),
        "a fresh create journals a Pending desired state"
    );
    assert_eq!(
        vds.2, None,
        "attached_vm_id must be NULL — v1 creates standalone volumes"
    );
    assert_eq!(vds.3.as_deref(), Some("u-operator"));
    assert_eq!(vds.4, None, "no device name on an unattached volume");

    // operations row: CreateVolume/Accepted — the arm PR 1 landed —
    // with the design's idempotency key and the resource_kind the
    // BFF's volume display surfaces read.
    let op: (String, String, String, String, String) = sqlx::query_as(
        "SELECT idempotency_key, resource_kind, operation_type, status, requested_by FROM operations WHERE resource_id = ?",
    )
    .bind(&volume_id)
    .fetch_one(&state.pool)
    .await
    .expect("operation row");
    assert_eq!(op.0, format!("create-volume-{volume_id}"));
    assert_eq!(op.1, "volume");
    assert_eq!(op.2, "CreateVolume");
    assert_eq!(op.3, "Accepted");
    assert_eq!(op.4, "u-operator");
}

#[tokio::test]
async fn happy_path_persists_named_storage_class() {
    let state = build_state().await;
    seed_node_inventory(&state, "n-lvm", &["lvm", "local"], None).await;

    // A named class persists verbatim; an explicit "local" is stored as
    // the string (the NULL-vs-"local" duality); a blank is semantically
    // absent (NULL) — the vm-create conventions.
    let (status, body) = create(
        &state,
        r#"{"name":"vol-lvm","node_id":"n-lvm","capacity_bytes":1024,"storage_class":"lvm"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let vid = body["volume_id"].as_str().unwrap();
    let class: Option<String> =
        sqlx::query_scalar("SELECT storage_class FROM volumes WHERE volume_id = ?")
            .bind(vid)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(class.as_deref(), Some("lvm"));

    let (status, body) = create(
        &state,
        r#"{"display_name":"vol-explicit-local","node_id":"n-lvm","capacity_bytes":1024,"storage_class":"local"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let vid = body["volume_id"].as_str().unwrap();
    let class: Option<String> =
        sqlx::query_scalar("SELECT storage_class FROM volumes WHERE volume_id = ?")
            .bind(vid)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(class.as_deref(), Some("local"));

    let (status, body) = create(
        &state,
        r#"{"name":"vol-blank","node_id":"n-lvm","capacity_bytes":1024,"storage_class":"  "}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let vid = body["volume_id"].as_str().unwrap();
    let class: Option<String> =
        sqlx::query_scalar("SELECT storage_class FROM volumes WHERE volume_id = ?")
            .bind(vid)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(class, None, "a blank storage_class is semantically absent");
}

// ─────────────────────────────────────────────────────────────────────
// Vocabulary (DP5) and node capability (#516) — 400 before journaling
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn rejects_unknown_storage_class_with_400() {
    let state = build_state().await;
    seed_node(&state, "n-1").await;

    for class in ["zfs", "local-file", "localdisk", "block", "LVM"] {
        let (status, body) = create(
            &state,
            &format!(
                r#"{{"name":"vol-bad","node_id":"n-1","capacity_bytes":1024,"storage_class":"{class}"}}"#
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "storage_class {class:?} must reject: {body}"
        );
        assert!(
            body.to_string().contains("storage_class"),
            "the rejection must name the field: {body}"
        );
    }

    // A non-string value is a 400 too, not a 500.
    let (status, body) = create(
        &state,
        r#"{"name":"vol-bad-type","node_id":"n-1","capacity_bytes":1024,"storage_class":3}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");

    assert_create_journaled_nothing(&state).await;
}

#[tokio::test]
async fn rejects_class_the_node_does_not_offer_with_zero_journaling() {
    // The definite mismatch: a local-only node must not accept an
    // LVM-class create — 400 BEFORE the transaction, so nothing is
    // journaled (the #516 table-loop assertion pattern).
    let state = build_state().await;
    seed_node_inventory(&state, "n-local", &["local"], None).await;

    let (status, body) = create(
        &state,
        r#"{"name":"vol-nope","node_id":"n-local","capacity_bytes":1024,"storage_class":"lvm"}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a class the node does not offer must reject: {body}"
    );
    let body = body.to_string();
    assert!(
        body.contains("n-local does not offer storage class lvm"),
        "the rejection must name the node and class: {body}"
    );
    assert_create_journaled_nothing(&state).await;
}

#[tokio::test]
async fn rejects_classless_create_on_lvm_only_node() {
    // Classless = local (the lifecycle-side semantics): an LVM-only
    // node does not offer it, so the default create shape rejects.
    let state = build_state().await;
    seed_node_inventory(&state, "n-lvm", &["lvm"], None).await;

    let (status, body) = create(
        &state,
        r#"{"name":"vol-bare","node_id":"n-lvm","capacity_bytes":1024}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a classless create on an LVM-only node must reject: {body}"
    );
    let body = body.to_string();
    assert!(
        body.contains("n-lvm does not offer storage class local"),
        "the rejection must name the defaulted class: {body}"
    );
    assert_create_journaled_nothing(&state).await;
}

#[tokio::test]
async fn fails_open_on_unreported_storage_classes() {
    // The fail-open discipline, byte-exactly the lifecycle-side
    // semantics: a node that never reported classes (no inventory row,
    // or an inventory row with an EMPTY list) must keep accepting ANY
    // class — an unreported node never rejects; stord's own backend
    // validation at the open remains the backstop.
    let state = build_state().await;

    seed_node(&state, "n-noreport").await;
    let (status, body) = create(
        &state,
        r#"{"name":"vol-noreport","node_id":"n-noreport","capacity_bytes":1024,"storage_class":"lvm"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "volume create body: {body}");

    seed_node_inventory(&state, "n-empty", &[], None).await;
    let (status, body) = create(
        &state,
        r#"{"name":"vol-empty","node_id":"n-empty","capacity_bytes":1024,"storage_class":"ceph"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "volume create body: {body}");
}

// ─────────────────────────────────────────────────────────────────────
// Core-managed posture (DP7, the #378 mirror)
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn rejects_core_managed_node_with_400_and_zero_journaling() {
    let state = build_state().await;
    seed_node_inventory(&state, "n-core", &["local"], Some("core-managed")).await;

    let (status, body) = create(
        &state,
        r#"{"name":"vol-core","node_id":"n-core","capacity_bytes":1024}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a create targeting a core-managed node must reject at accept: {body}"
    );
    let body = body.to_string();
    assert!(
        body.contains("core-managed"),
        "the rejection must name the core-managed posture: {body}"
    );
    assert_create_journaled_nothing(&state).await;
}

#[tokio::test]
async fn core_managed_check_fails_open_on_unreported_mode() {
    // get_authority_mode's None (no inventory row / never reported)
    // fails OPEN — the agent-side fail-closed dispatch (PR 1's
    // handler) remains the enforcement for unreported nodes.
    let state = build_state().await;
    seed_node_inventory(&state, "n-legacy", &["local"], Some("legacy")).await;

    let (status, body) = create(
        &state,
        r#"{"name":"vol-legacy","node_id":"n-legacy","capacity_bytes":1024}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "volume create body: {body}");
}

// ─────────────────────────────────────────────────────────────────────
// Quota (DP6 — the storage column)
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn rejects_when_storage_quota_exceeded() {
    let state = build_state().await;
    seed_node(&state, "n-1").await;
    sqlx::query(
        "INSERT INTO quotas (user_id, max_storage_bytes) VALUES ('u-operator', 1073741824)",
    )
    .execute(&state.pool)
    .await
    .expect("seed quota");

    // Exactly at the limit still passes (max is inclusive)...
    let (status, body) = create(
        &state,
        r#"{"name":"vol-at-limit","node_id":"n-1","capacity_bytes":1073741824}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    // ...one byte over rejects with the quota error — inside the
    // transaction, before any INSERT, so nothing is journaled.
    let (status, body) = create(
        &state,
        r#"{"name":"vol-over","node_id":"n-1","capacity_bytes":1073741825}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a quota rejection must 422: {body}"
    );
    let body = body.to_string();
    assert!(
        body.contains("QUOTA_EXCEEDED"),
        "the rejection must carry the quota code: {body}"
    );

    // Only the first (accepted) create's rows exist.
    let volumes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM volumes")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let vds: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM volume_desired_state")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let ops: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM operations")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(volumes, 1, "the rejected create must not journal a volume");
    assert_eq!(vds, 1);
    assert_eq!(ops, 1);
}

// ─────────────────────────────────────────────────────────────────────
// Field validation (DP3) and the reserved keys (DP3/DP4)
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn rejects_invalid_display_names() {
    let state = build_state().await;
    seed_node(&state, "n-1").await;

    for name in ["", "a/b", "../escape", "semi;colon", "name\nnewline"] {
        let (status, body) = create(
            &state,
            &format!(r#"{{"name":"{name}","node_id":"n-1","capacity_bytes":1024}}"#),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "display_name {name:?} must reject: {body}"
        );
    }

    // A missing name is a 400 too.
    let (status, _body) = create(&state, r#"{"node_id":"n-1","capacity_bytes":1024}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    assert_create_journaled_nothing(&state).await;
}

#[tokio::test]
async fn requires_node_id_and_positive_capacity() {
    let state = build_state().await;
    seed_node(&state, "n-1").await;

    // node_id is required — no first-enrolled-node default (DP3).
    let (status, body) = create(&state, r#"{"name":"vol-nonode","capacity_bytes":1024}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    let body = body.to_string();
    assert!(
        body.contains("node_id"),
        "the rejection must name the field: {body}"
    );

    // A blank node_id is as good as missing.
    let (status, _body) = create(
        &state,
        r#"{"name":"vol-blanknode","node_id":"  ","capacity_bytes":1024}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // capacity_bytes is required and must be positive.
    for bad in ["0", "-1"] {
        let (status, body) = create(
            &state,
            &format!(r#"{{"name":"vol-bad","node_id":"n-1","capacity_bytes":{bad}}}"#),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "capacity_bytes {bad} must reject: {body}"
        );
    }
    let (status, _body) = create(&state, r#"{"name":"vol-nocap","node_id":"n-1"}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Over the 64 TiB ceiling (the MAX_VOLUME_SIZE_GB discipline).
    let (status, body) = create(
        &state,
        r#"{"name":"vol-huge","node_id":"n-1","capacity_bytes":70368744177665}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "capacity over 64 TiB must reject: {body}"
    );

    assert_create_journaled_nothing(&state).await;
}

#[tokio::test]
async fn rejects_attached_vm_id_naming_the_attach_path() {
    // DP4's loud reservation: the key is NOT silently dropped — the
    // 400 names the mutate-attach path (the --vlan lesson).
    let state = build_state().await;
    seed_node(&state, "n-1").await;

    let (status, body) = create(
        &state,
        r#"{"name":"vol-attach","node_id":"n-1","capacity_bytes":1024,"attached_vm_id":"vm-1"}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "attached_vm_id must reject at accept: {body}"
    );
    let body = body.to_string();
    assert!(
        body.contains("attached_vm_id"),
        "the rejection must name the reserved key: {body}"
    );
    assert!(
        body.contains("attach"),
        "the rejection must name the attach path: {body}"
    );
    assert!(
        body.contains("/v1/volumes/mutate"),
        "the rejection must name the mutate-attach route: {body}"
    );
    assert_create_journaled_nothing(&state).await;
}

#[tokio::test]
async fn rejects_seed_image_ref() {
    // DP3: seed_image_ref is deferred and rejected loudly if ever sent.
    let state = build_state().await;
    seed_node(&state, "n-1").await;

    let (status, body) = create(
        &state,
        r#"{"name":"vol-seed","node_id":"n-1","capacity_bytes":1024,"seed_image_ref":"/tmp/x.img"}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "seed_image_ref must reject at accept: {body}"
    );
    let body = body.to_string();
    assert!(
        body.contains("seed_image_ref"),
        "the rejection must name the field: {body}"
    );
    assert_create_journaled_nothing(&state).await;
}

// ─────────────────────────────────────────────────────────────────────
// Auth: operator tier, mirroring the sibling volume mutation routes
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn requires_authentication_and_operator_role() {
    let state = build_state().await;
    seed_node(&state, "n-1").await;
    let body = r#"{"name":"vol-auth","node_id":"n-1","capacity_bytes":1024}"#;

    // No bearer token → 401.
    let app = chv_webui_bff::bff_router(state.clone()).with_state(state.clone());
    let req = Request::builder()
        .method("POST")
        .uri("/v1/volumes/create")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Viewer role → 403 (operator-or-admin, like volumes/mutate).
    let viewer = token_for(&state, "u-viewer", "viewer");
    let (status, body_out) = post(&state, &viewer, body).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a viewer must not create volumes: {body_out}"
    );

    // Admin is allowed (operator-OR-admin).
    let admin = token_for(&state, "u-admin", "admin");
    let (status, body_out) = post(&state, &admin, body).await;
    assert_eq!(status, StatusCode::OK, "admin create body: {body_out}");
    let volume: Option<String> =
        sqlx::query_scalar("SELECT owner_id FROM volumes WHERE volume_id = ?")
            .bind(body_out["volume_id"].as_str().unwrap())
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(
        volume.as_deref(),
        Some("u-admin"),
        "the admin's own create is stamped with the admin's sub"
    );

    // The 401/403 attempts journaled nothing.
    let volumes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM volumes")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(volumes, 1, "only the admin create journaled a volume");
}
