//! Integration tests for volume-row ownership on the two creation paths
//! that previously inserted ownerless volumes (kubedoio/chv#386).
//!
//! `vms.rs::create_vm` already set `volumes.owner_id = claims.sub`, but the
//! sibling creation paths did not:
//!
//! - VM import (`POST /v1/vms/import`) — `imports.rs`;
//! - VM-template instantiation (`POST /v1/vm-templates/:id/clone`) —
//!   `templates.rs`.
//!
//! Both routes are `require_operator_or_admin`, so a non-admin operator
//! could create a volume and then be locked out of it: `require_volume_owner`
//! rejects `owner_id IS NULL` for non-admins ("resource has no owner; admin
//! access required"), making the volume admin-only for attach / snapshot /
//! resize / restore.
//!
//! These tests boot the real `bff_router` and pin the fixed contract:
//!
//! - an operator importing a VM gets a volume row with `owner_id` = that
//!   operator, and can then pass `require_volume_owner` on it (mutate is
//!   accepted);
//! - an operator instantiating a template gets the same;
//! - a different operator is still forbidden (ownership tightens, it does
//!   not become public);
//! - an admin can still mutate any volume (admin path unchanged).
//!
//! NOTE on the import test: `POST /v1/vms/import` is multipart, and the
//! global CSRF middleware only admits `application/json` plus
//! `multipart/form-data` with a non-empty `x-csrf-token` header (#496;
//! `csrf_middleware.rs`). The router-driven path — multipart POST
//! through `bff_router`, CSRF-enforced — is pinned end-to-end by
//! `tests/csrf_multipart_import.rs`. This test keeps the direct
//! `imports::import_vm` invocation deliberately: what is under test
//! here is the handler's ownership journaling, and constructing the
//! `Multipart` + `BearerToken` by hand states those preconditions
//! without coupling the ownership assertions to the CSRF-layer harness
//! (the authz consequence is still asserted through the real router —
//! the mutate call below goes through `require_volume_owner`).

use std::sync::{Arc, Mutex};

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

/// Records every volume mutation call as "volume_id:action" for assertions.
#[derive(Default)]
struct RecordingMutations {
    calls: Mutex<Vec<String>>,
}

impl RecordingMutations {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

fn volume_response(volume_id: &str) -> chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse {
    chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse {
        accepted: true,
        task_id: format!("op-{volume_id}"),
        volume_id: volume_id.to_string(),
        summary: "recorded".to_string(),
    }
}

#[async_trait]
impl MutationService for RecordingMutations {
    async fn mutate_vm(
        &self,
        _vm_id: String,
        _action: String,
        _force: bool,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        unreachable!("no VM mutations in these tests")
    }
    async fn migrate_vm(
        &self,
        _vm_id: String,
        _target_node_id: String,
        _pause_first: bool,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        unreachable!("no VM mutations in these tests")
    }
    async fn snapshot_vm(
        &self,
        _vm_id: String,
        _destination: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        unreachable!("no VM mutations in these tests")
    }
    async fn restore_snapshot(
        &self,
        _vm_id: String,
        _source: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        unreachable!("no VM mutations in these tests")
    }
    async fn mutate_node(
        &self,
        _node_id: String,
        _action: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateNodeResponse, BffError> {
        unreachable!("no node mutations in these tests")
    }
    async fn mutate_volume(
        &self,
        volume_id: String,
        action: String,
        _force: bool,
        _resize_bytes: Option<u64>,
        _vm_id: Option<String>,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("{volume_id}:{action}"));
        Ok(volume_response(&volume_id))
    }
    async fn snapshot_volume(
        &self,
        _volume_id: String,
        _snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!("no volume snapshots in these tests")
    }
    async fn restore_volume_snapshot(
        &self,
        _volume_id: String,
        _snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!("no volume snapshots in these tests")
    }
    async fn delete_volume_snapshot(
        &self,
        _volume_id: String,
        _snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!("no volume snapshots in these tests")
    }
    async fn clone_volume(
        &self,
        _source_volume_id: String,
        _target_volume_id: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!("no volume clones in these tests")
    }
    async fn mutate_network(
        &self,
        _network_id: String,
        _action: String,
        _force: bool,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateNetworkResponse, BffError> {
        unreachable!("no network mutations in these tests")
    }
}

async fn build_state(mutations: Arc<RecordingMutations>, agent_runtime_dir: &str) -> AppState {
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
        mutations,
        jwt_secret: "test-secret".to_string(),
        monitoring: None,
        monitoring_health: chv_webui_bff::MonitoringHealth::new(),
        agent_runtime_dir: std::path::PathBuf::from(agent_runtime_dir),
        cache: chv_webui_bff::BffCache::new(5),
        clock: Arc::new(SystemClock),
    }
}

/// Seed a user and return a signed JWT for it. Distinct user_ids let one
/// test hold several operators (the shared `seed_jwt_as` helper keys the
/// user id off the role alone).
///
/// The claims deliberately carry `username != sub`, mirroring how production
/// mints tokens (`auth.rs`: `sub = user_id`, `username` = the login name).
/// The owner assertions expect the `sub`, so a regression that binds
/// `claims.username` into `owner_id` fails these tests instead of silently
/// passing on identical fixtures.
async fn seed_jwt_for(state: &AppState, user_id: &str, role: &str) -> String {
    let username = format!("{user_id}-user");
    sqlx::query(
        "INSERT INTO users (user_id, username, password_hash, role, must_change_password) \
         VALUES (?, ?, 'x', ?, 0)",
    )
    .bind(user_id)
    .bind(&username)
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
        sub: user_id.to_string(),
        username,
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

/// Seed one enrolled node with a healthy observed state (the import path
/// requires `health_status = 'healthy'`).
async fn seed_healthy_node(state: &AppState) {
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('n-1', 'h-1', 'Node 1')",
    )
    .execute(&state.pool)
    .await
    .expect("seed node");
    sqlx::query(
        "INSERT INTO node_observed_state (node_id, observed_generation, observed_state, \
         health_status, runtime_status) VALUES ('n-1', 1, 'TenantReady', 'healthy', 'Running')",
    )
    .execute(&state.pool)
    .await
    .expect("seed node observed state");
}

/// The single volume row in the freshly-migrated store: (volume_id, owner_id).
/// Fails loudly on zero or multiple rows instead of silently picking one.
async fn the_volume(state: &AppState) -> (String, Option<String>) {
    let mut rows =
        sqlx::query_as::<_, (String, Option<String>)>("SELECT volume_id, owner_id FROM volumes")
            .fetch_all(&state.pool)
            .await
            .expect("query volumes");
    assert_eq!(
        rows.len(),
        1,
        "expected exactly one volume row, found {}: {rows:?}",
        rows.len()
    );
    rows.pop().expect("row count asserted to be one above")
}

async fn post_json_with_token(
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

/// Minimal qcow2-looking payload: the import handler only checks the
/// 4-byte magic before accepting the stream.
fn qcow2_bytes() -> Vec<u8> {
    let mut bytes = b"QFI\xfb".to_vec();
    bytes.extend_from_slice(b"\x00\x00\x00\x03rest-of-disk");
    bytes
}

/// A raw multipart body with a `name` text field and a `file` binary field,
/// as `chvctl vm import` / the UI upload form send it.
fn multipart_body(boundary: &str, name: &str, file: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"name\"\r\n\r\n");
    body.extend_from_slice(name.as_bytes());
    body.extend_from_slice(format!("\r\n--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"disk.qcow2\"\r\n\
          Content-Type: application/octet-stream\r\n\r\n",
    );
    body.extend_from_slice(file);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

async fn import_vm_as(state: AppState, vm_name: &str) -> (StatusCode, serde_json::Value) {
    // Invoke the handler directly as the operator `u-op-import` (seeded by
    // the caller with a matching JWT): the multipart route cannot pass the
    // JSON-only CSRF gate (see module NOTE). Build the multipart body the
    // same way a real client would and let axum parse it.
    use axum::extract::FromRequest;

    let boundary = "chv-test-boundary";
    let body = multipart_body(boundary, vm_name, &qcow2_bytes());
    let req = Request::builder()
        .method("POST")
        .uri("/v1/vms/import")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap();
    let multipart = axum::extract::Multipart::from_request(req, &())
        .await
        .expect("parse multipart body");

    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    // Same user `seed_jwt_for` seeded for the caller: sub = user_id,
    // username = login name (deliberately different, see `seed_jwt_for`).
    let claims = chv_webui_bff::auth::Claims {
        sub: "u-op-import".to_string(),
        username: "u-op-import-user".to_string(),
        role: "operator".to_string(),
        exp,
        must_change_password: false,
        aud: None,
        token_version: None,
    };
    let result = chv_webui_bff::handlers::imports::import_vm(
        chv_webui_bff::BearerToken(claims),
        axum::extract::State(state),
        axum::Extension(None),
        multipart,
    )
    .await;
    match result {
        Ok(json) => (StatusCode::OK, json.0),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!(format!("{:?}", e)),
        ),
    }
}

/// The operator-facing consequence of #386: the creating operator must pass
/// `require_volume_owner` on the volume they just created (pre-#386 the row
/// was ownerless and every non-admin mutation 403'd with "resource has no
/// owner; admin access required").
async fn assert_operator_can_mutate(
    state: &AppState,
    mutations: &RecordingMutations,
    volume_id: &str,
    token: &str,
) {
    let (status, body) = post_json_with_token(
        state.clone(),
        "/v1/volumes/mutate",
        token,
        &format!(r#"{{"volume_id":"{volume_id}","action":"detach"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "mutate body: {body}");
    assert_eq!(
        mutations.calls(),
        vec![format!("{volume_id}:detach")],
        "the mutation service must receive the operator's request"
    );
}

#[tokio::test]
async fn imported_vm_volume_is_owned_by_importing_operator() {
    let mutations = Arc::new(RecordingMutations::default());
    let runtime_dir = std::env::temp_dir().join(format!(
        "chv-bff-import-ownership-{}",
        chv_common::gen_short_id()
    ));
    tokio::fs::create_dir_all(&runtime_dir)
        .await
        .expect("create agent runtime dir");
    let state = build_state(mutations.clone(), runtime_dir.to_str().unwrap()).await;
    seed_healthy_node(&state).await;
    let token = seed_jwt_for(&state, "u-op-import", "operator").await;

    let (status, body) = import_vm_as(state.clone(), "imported-vm").await;
    assert_eq!(status, StatusCode::OK, "import body: {body}");

    let (volume_id, owner_id) = the_volume(&state).await;
    assert_eq!(
        owner_id.as_deref(),
        Some("u-op-import"),
        "the import path must stamp the importing operator on the volume row (#386)"
    );

    // The operator who imported it can now mutate it where the pre-#386
    // ownerless row 403'd.
    assert_operator_can_mutate(&state, &mutations, &volume_id, &token).await;

    let _ = tokio::fs::remove_dir_all(&runtime_dir).await;
}

#[tokio::test]
async fn template_instantiated_volume_is_owned_by_instantiating_operator() {
    let mutations = Arc::new(RecordingMutations::default());
    let state = build_state(mutations.clone(), "/nonexistent-agent-runtime").await;
    seed_healthy_node(&state).await;
    sqlx::query(
        "INSERT INTO vm_templates (template_id, name, cpu_count, memory_bytes, disk_size_bytes) \
         VALUES ('tpl-1', 'tpl-one', 1, 536870912, 10737418240)",
    )
    .execute(&state.pool)
    .await
    .expect("seed vm_template");
    let token = seed_jwt_for(&state, "u-op-tpl", "operator").await;

    let (status, body) = post_json_with_token(
        state.clone(),
        "/v1/vm-templates/tpl-1/clone",
        &token,
        r#"{"name":"inst-one"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "clone body: {body}");

    let (volume_id, owner_id) = the_volume(&state).await;
    assert_eq!(
        owner_id.as_deref(),
        Some("u-op-tpl"),
        "template instantiation must stamp the instantiating operator on the volume row (#386)"
    );

    assert_operator_can_mutate(&state, &mutations, &volume_id, &token).await;
}

#[tokio::test]
async fn other_operator_still_cannot_mutate_a_created_volume() {
    // Ownership tightens; it does not become operator-public.
    let mutations = Arc::new(RecordingMutations::default());
    let state = build_state(mutations.clone(), "/nonexistent-agent-runtime").await;
    seed_healthy_node(&state).await;
    sqlx::query(
        "INSERT INTO vm_templates (template_id, name, cpu_count, memory_bytes, disk_size_bytes) \
         VALUES ('tpl-1', 'tpl-one', 1, 536870912, 10737418240)",
    )
    .execute(&state.pool)
    .await
    .expect("seed vm_template");
    let creator = seed_jwt_for(&state, "u-op-creator", "operator").await;
    let stranger = seed_jwt_for(&state, "u-op-stranger", "operator").await;

    let (status, body) = post_json_with_token(
        state.clone(),
        "/v1/vm-templates/tpl-1/clone",
        &creator,
        r#"{"name":"inst-two"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "clone body: {body}");

    let (volume_id, owner_id) = the_volume(&state).await;
    assert_eq!(owner_id.as_deref(), Some("u-op-creator"));

    let (status, body) = post_json_with_token(
        state.clone(),
        "/v1/volumes/mutate",
        &stranger,
        &format!(r#"{{"volume_id":"{volume_id}","action":"detach"}}"#),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a different operator must not gain access: {body}"
    );
    assert!(mutations.calls().is_empty());
}

#[tokio::test]
async fn admin_can_still_mutate_an_operator_created_volume() {
    // The admin path is unchanged: admins bypass require_volume_owner.
    let mutations = Arc::new(RecordingMutations::default());
    let state = build_state(mutations.clone(), "/nonexistent-agent-runtime").await;
    seed_healthy_node(&state).await;
    sqlx::query(
        "INSERT INTO vm_templates (template_id, name, cpu_count, memory_bytes, disk_size_bytes) \
         VALUES ('tpl-1', 'tpl-one', 1, 536870912, 10737418240)",
    )
    .execute(&state.pool)
    .await
    .expect("seed vm_template");
    let operator = seed_jwt_for(&state, "u-op-admin-case", "operator").await;
    let admin = seed_jwt_for(&state, "u-admin", "admin").await;

    let (status, body) = post_json_with_token(
        state.clone(),
        "/v1/vm-templates/tpl-1/clone",
        &operator,
        r#"{"name":"inst-three"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "clone body: {body}");

    let (volume_id, owner_id) = the_volume(&state).await;
    assert_eq!(owner_id.as_deref(), Some("u-op-admin-case"));

    let (status, body) = post_json_with_token(
        state.clone(),
        "/v1/volumes/mutate",
        &admin,
        &format!(r#"{{"volume_id":"{volume_id}","action":"detach"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "admin mutate body: {body}");
    assert_eq!(
        mutations.calls(),
        vec![format!("{volume_id}:detach")],
        "the admin mutation must reach the mutation service"
    );
}
