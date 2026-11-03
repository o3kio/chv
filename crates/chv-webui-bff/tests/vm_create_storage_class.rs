//! Integration tests for the #379 PR 2 BFF payload field: the VM-create
//! payload's optional per-disk `storage_class`.
//!
//! The design (docs/design/issue-379-storage-class-dispatch.md, §4
//! Option A / §5.1 DP1) makes `volumes.storage_class` the semantic field
//! it always pretended to be: the BFF's VM-create payload accepts an
//! optional class, default NULL = local. These tests boot the real
//! `bff_router` and pin the accept-time contract:
//!
//! - a payload WITHOUT the field creates the volume with a NULL class —
//!   today's behavior byte-exactly (NULL dispatches as "local" all the
//!   way down; the string is never materialized);
//! - a payload naming a DP3 vocabulary class (`lvm`) persists it to
//!   `volumes.storage_class`;
//! - an EXPLICIT `"local"` is stored as the string `"local"` (the
//!   documented NULL-vs-"local" duality, §9 risk 4 — both dispatch
//!   identically);
//! - an unknown class string is rejected with HTTP 400 at accept time
//!   (validated against the single shared DP3 vocabulary in
//!   `chv-hypervisor-api`, never a local copy) and creates nothing;
//!   and — the #512 second-pass follow-up, the node-capability
//!   dimension — the requested class (classless = NULL = local) must
//!   be offered by the placement node's advertised
//!   `node_inventory.storage_classes` (`NodeRepository::node_storage_class_rejection`,
//!   the same shared composition the lifecycle RPC uses), rejecting
//!   with 400 before any row is journaled and failing OPEN on a node
//!   that never reported classes.

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
        console_jwt_secret: "test-secret".to_string(),
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

/// Seed one enrolled node so create_vm has a placement target.
async fn seed_node(state: &AppState) {
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('n-1', 'h-1', 'Node 1')",
    )
    .execute(&state.pool)
    .await
    .expect("seed node");
}

/// Seed one enrolled node plus an inventory row advertising exactly
/// these storage classes — the JSON array of strings the inventory
/// paths write (`NodeRepository`'s reader parses the same column).
async fn seed_node_with_storage_classes(state: &AppState, node_id: &str, classes: &[&str]) {
    sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES (?, 'h', 'h')")
        .bind(node_id)
        .execute(&state.pool)
        .await
        .expect("seed node");
    sqlx::query(
        "INSERT INTO node_inventory (node_id, architecture, cpu_count, memory_bytes, storage_classes) \
         VALUES (?, 'x86_64', 1, 1024, ?)",
    )
    .bind(node_id)
    .bind(serde_json::to_string(classes).unwrap())
    .execute(&state.pool)
    .await
    .expect("seed node inventory");
}

/// Count the rows a create would have journaled — every table the
/// create tx writes. A rejected create must leave all of them at zero
/// (the attach-side DP4 test's journaling-nothing pattern).
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

/// The stored storage_class of the (single) boot volume the create tx
/// minted for `display_name` — the volume row is named `{name}-disk`.
async fn volume_class(state: &AppState, display_name: &str) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT storage_class FROM volumes WHERE display_name = ?",
    )
    .bind(format!("{display_name}-disk"))
    .fetch_one(&state.pool)
    .await
    .expect("volume row")
}

#[tokio::test]
async fn vm_create_without_storage_class_stores_null() {
    // The additive default: a payload without the field creates the
    // volume with a NULL class — byte-exactly today's row shape, so the
    // agent dispatches "local" via the absent-field default.
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
        volume_class(&state, "vm-bare").await,
        None,
        "no storage_class in the payload must store NULL"
    );
}

#[tokio::test]
async fn vm_create_with_storage_class_persists_it() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;

    let (status, body) = create_vm(
        &state,
        &token,
        r#"{"name":"vm-lvm","image_ref":"/tmp/x.img","storage_class":"lvm"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "vm create body: {body}");
    assert_eq!(
        volume_class(&state, "vm-lvm").await.as_deref(),
        Some("lvm"),
        "a DP3 vocabulary class must persist to volumes.storage_class"
    );

    // An explicit "local" is stored as the string (the documented
    // NULL-vs-"local" duality — both dispatch identically).
    let (status, body) = create_vm(
        &state,
        &token,
        r#"{"name":"vm-explicit-local","image_ref":"/tmp/x.img","storage_class":"local"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "vm create body: {body}");
    assert_eq!(
        volume_class(&state, "vm-explicit-local").await.as_deref(),
        Some("local"),
        "an explicit local is stored as the string, never rewritten to NULL"
    );

    // A blank value is semantically absent (NULL), matching the
    // cloud_init_userdata blank-filter convention.
    let (status, body) = create_vm(
        &state,
        &token,
        r#"{"name":"vm-blank","image_ref":"/tmp/x.img","storage_class":"  "}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "vm create body: {body}");
    assert_eq!(
        volume_class(&state, "vm-blank").await,
        None,
        "a blank storage_class must be treated as absent"
    );
}

#[tokio::test]
async fn vm_create_rejects_unknown_storage_class_with_400() {
    // Accept-time validation against the single shared DP3 vocabulary:
    // unknown strings (including the stord-boundary local aliases and
    // typos) reject with 400 and create nothing.
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;

    for class in ["zfs", "local-file", "localdisk", "block", "LVM"] {
        let (status, body) = create_vm(
            &state,
            &token,
            &format!(r#"{{"name":"vm-bad","image_ref":"/tmp/x.img","storage_class":"{class}"}}"#),
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

    let vms: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vms")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let volumes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM volumes")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(vms, 0, "a rejected create must not persist a VM");
    assert_eq!(volumes, 0, "a rejected create must not persist a volume");

    // A non-string value is a 400 too, not a 500.
    let (status, body) = create_vm(
        &state,
        &token,
        r#"{"name":"vm-bad-type","image_ref":"/tmp/x.img","storage_class":3}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
}

// ─────────────────────────────────────────────────────────────────────
// #379 DP4, #512 second-pass follow-up: the node-capability dimension
// on the production create surface. The check is the SAME shared
// composition the lifecycle RPC uses
// (`NodeRepository::node_storage_class_rejection`), fired before the
// create transaction — mirroring the attach-side path's reachable 400.
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn vm_create_accepts_class_the_node_offers() {
    // A definite match: an LVM-reporting node accepts an LVM-class
    // create (and persists the class on the boot volume).
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node_with_storage_classes(&state, "n-lvm", &["lvm"]).await;

    let (status, body) = create_vm(
        &state,
        &token,
        r#"{"name":"vm-offer","node_id":"n-lvm","image_ref":"/tmp/x.img","storage_class":"lvm"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "vm create body: {body}");
    assert_eq!(
        volume_class(&state, "vm-offer").await.as_deref(),
        Some("lvm"),
        "an offered class must persist to volumes.storage_class"
    );

    // Classless (NULL = local) on a local-reporting node accepts —
    // local-only creates are unchanged.
    seed_node_with_storage_classes(&state, "n-local", &["local"]).await;
    let (status, body) = create_vm(
        &state,
        &token,
        r#"{"name":"vm-bare-local","node_id":"n-local","image_ref":"/tmp/x.img"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "vm create body: {body}");
    assert_eq!(
        volume_class(&state, "vm-bare-local").await,
        None,
        "a classless create on a local node must keep storing NULL"
    );
}

#[tokio::test]
async fn vm_create_rejects_class_the_node_does_not_offer() {
    // The definite mismatch: a local-only node must not accept an
    // LVM-class create — 400 BEFORE the transaction, so nothing is
    // journaled (the attach-side DP4 test's assertion pattern).
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node_with_storage_classes(&state, "n-local", &["local"]).await;

    let (status, body) = create_vm(
        &state,
        &token,
        r#"{"name":"vm-nope","node_id":"n-local","image_ref":"/tmp/x.img","storage_class":"lvm"}"#,
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
async fn vm_create_rejects_classless_disk_on_lvm_only_node() {
    // Classless = local (the lifecycle-side semantics): an LVM-only
    // node does not offer it, so the default create shape rejects —
    // the exact case that used to 200 and fail only at the agent's
    // stord open.
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node_with_storage_classes(&state, "n-lvm", &["lvm"]).await;

    let (status, body) = create_vm(
        &state,
        &token,
        r#"{"name":"vm-bare-lvm","node_id":"n-lvm","image_ref":"/tmp/x.img"}"#,
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
async fn vm_create_fails_open_on_unreported_storage_classes() {
    // The fail-open discipline, byte-exactly the lifecycle-side
    // semantics: a node that never reported classes (no inventory row,
    // or an inventory row with an EMPTY list) must keep accepting ANY
    // class — an unreported node never rejects; stord's class
    // validation at the open remains the backstop.
    let state = build_state().await;
    let token = seed_jwt(&state).await;

    // No inventory row at all (never reported / pre-#379 agent).
    seed_node(&state).await;
    let (status, body) = create_vm(
        &state,
        &token,
        r#"{"name":"vm-noreport","image_ref":"/tmp/x.img","storage_class":"lvm"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "vm create body: {body}");

    // An inventory row whose storage_classes list is empty.
    seed_node_with_storage_classes(&state, "n-empty", &[]).await;
    let (status, body) = create_vm(
        &state,
        &token,
        r#"{"name":"vm-empty","node_id":"n-empty","image_ref":"/tmp/x.img","storage_class":"ceph"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "vm create body: {body}");
}

#[tokio::test]
async fn vm_create_capability_check_normalizes_legacy_localdisk_report() {
    // DP3 normalization at the shared predicate: a pre-#379 agent's
    // `localdisk` probe report compares as `local`, so a classless
    // create on such a node still matches (the lifecycle-side test's
    // edge, now pinned on the production create surface too).
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node_with_storage_classes(&state, "n-legacy", &["localdisk"]).await;

    let (status, body) = create_vm(
        &state,
        &token,
        r#"{"name":"vm-legacy","node_id":"n-legacy","image_ref":"/tmp/x.img"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "vm create body: {body}");
}
