//! Security tests for the CSRF middleware's multipart path (issue #496)
//! and the reachability of `POST /v1/vms/import` through the real router.
//!
//! Before #496 the middleware 415'd every non-JSON content type before
//! routing, so the multipart import route could never reach its handler
//! (pinned in `volume_creation_ownership.rs`'s module NOTE). The fix adds
//! a second admissible shape for non-GET requests —
//! `multipart/form-data` + a non-empty `x-csrf-token` header — because
//! multipart is form-native (a cross-site HTML form CAN produce it) and
//! therefore needs its own un-forgeable marker, which a custom request
//! header provides (HTML forms cannot set headers at all; `fetch()`
//! attaches custom headers only after a successful CORS preflight).
//!
//! These tests boot the real `bff_router` and pin the four security
//! facts the fix must hold:
//!
//! 1. **Reachability** — a multipart POST with the CSRF header reaches
//!    the import handler end-to-end (200, journaled rows — not 415/403).
//! 2. **CSRF enforcement on the multipart path** — the same request
//!    WITHOUT the header (or with a blank one) is rejected 403
//!    `CSRF_REJECTED` before the handler runs, with zero journaling —
//!    even with a fully valid operator JWT, so the rejection is the
//!    CSRF layer, not the auth layer.
//! 3. **The JSON path is unchanged** — a JSON POST still passes the
//!    CSRF gate (and is stopped by auth, not by CSRF, when
//!    unauthenticated), and every other content type (form-native
//!    `application/x-www-form-urlencoded`, `text/plain`, none) still
//!    415s exactly as before.
//! 4. **Content-type lies cannot evade the check** — claiming
//!    `application/json` against the import route dies in the handler's
//!    `Multipart` extractor (400), and claiming `multipart/form-data`
//!    against a JSON route dies either in the middleware (403, without
//!    the header) or in the handler's `Json` extractor (415, with it).
//!    On routes whose handler takes no body extractor (e.g. the
//!    body-less backup-job deletes/toggles) a multipart+header request
//!    does reach the handler — but only after satisfying the
//!    un-forgeable header marker, the same bar the JSON path sets, so
//!    no content-type switch lowers the CSRF bar on any route.

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

/// Like the sibling suites' `build_state`, but with a per-test temp
/// runtime dir (created, and left for the OS to reap) — the import
/// handler writes the uploaded image there.
async fn build_state() -> AppState {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    chv_controlplane_store::run_migrations(&pool, None)
        .await
        .expect("run migrations");

    let agent_runtime_dir = std::env::temp_dir().join(format!(
        "chv-bff-csrf-import-{}",
        chv_common::gen_short_id()
    ));
    tokio::fs::create_dir_all(&agent_runtime_dir)
        .await
        .expect("create agent runtime dir");

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
        agent_runtime_dir,
        cache: chv_webui_bff::BffCache::new(5),
        clock: Arc::new(SystemClock),
    }
}

/// Seed an operator user + JWT (the import route's authz tier) and a
/// healthy node (the import path requires one for placement).
async fn seed_operator(state: &AppState) -> String {
    sqlx::query(
        "INSERT INTO users (user_id, username, password_hash, role, must_change_password) \
         VALUES ('u-op', 'op', 'x', 'operator', 0)",
    )
    .execute(&state.pool)
    .await
    .expect("seed user");

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

    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let claims = chv_webui_bff::auth::Claims {
        sub: "u-op".to_string(),
        username: "op".to_string(),
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

/// Minimal qcow2-looking payload: the import handler only checks the
/// 4-byte magic before accepting the stream.
fn qcow2_bytes() -> Vec<u8> {
    let mut bytes = b"QFI\xfb".to_vec();
    bytes.extend_from_slice(b"\x00\x00\x00\x03rest-of-disk");
    bytes
}

/// A raw multipart body with a `name` text field and a `file` binary
/// field, as `chvctl vm import` / the UI upload form send it.
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

#[allow(clippy::too_many_arguments)]
async fn send(
    state: AppState,
    method: &str,
    uri: &str,
    content_type: Option<&str>,
    csrf_header: Option<&str>,
    auth_token: Option<&str>,
    body: Vec<u8>,
) -> (StatusCode, serde_json::Value) {
    let app = chv_webui_bff::bff_router(state.clone()).with_state(state);
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(ct) = content_type {
        builder = builder.header("content-type", ct);
    }
    if let Some(csrf) = csrf_header {
        builder = builder.header("x-csrf-token", csrf);
    }
    if let Some(token) = auth_token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let req = builder.body(Body::from(body)).unwrap();
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

/// (vm rows, volume rows, operation rows) — the zero-journaling
/// assertion for every rejected request.
async fn journal_counts(state: &AppState) -> (i64, i64, i64) {
    let vms: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vms")
        .fetch_one(&state.pool)
        .await
        .expect("count vms");
    let volumes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM volumes")
        .fetch_one(&state.pool)
        .await
        .expect("count volumes");
    let ops: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM operations")
        .fetch_one(&state.pool)
        .await
        .expect("count operations");
    (vms, volumes, ops)
}

/// A complete, well-formed multipart import request (valid operator JWT,
/// healthy node, qcow2 payload) — the only varied input across the
/// reachability and rejection tests is the CSRF header.
async fn import_request() -> (AppState, &'static str, Vec<u8>, String) {
    let state = build_state().await;
    let token = seed_operator(&state).await;
    let boundary = "chv-test-boundary";
    let body = multipart_body(boundary, "imported-vm", &qcow2_bytes());
    (state, boundary, body, token)
}

// ---------------------------------------------------------------------------
// 1. Reachability (#496's headline defect)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multipart_import_with_csrf_header_reaches_the_handler() {
    let (state, boundary, body, token) = import_request().await;
    let (status, resp) = send(
        state.clone(),
        "POST",
        "/v1/vms/import",
        Some(&format!("multipart/form-data; boundary={boundary}")),
        Some("csrf-token-value"),
        Some(&token),
        body,
    )
    .await;
    // The pre-#496 middleware answered 415 before routing; reaching the
    // handler at all is the fix. A 200 with the server-minted id and the
    // journaled rows proves the whole path executed.
    assert_eq!(status, StatusCode::OK, "response body: {resp}");
    assert_eq!(resp["name"], "imported-vm");
    assert!(resp["id"].as_str().is_some(), "server-minted vm id: {resp}");
    let (vms, volumes, ops) = journal_counts(&state).await;
    assert_eq!(
        (vms, volumes, ops),
        (1, 1, 1),
        "the import journaled its rows"
    );
}

// ---------------------------------------------------------------------------
// 2. CSRF enforcement on the multipart path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multipart_import_without_csrf_header_is_rejected_before_the_handler() {
    let (state, boundary, body, token) = import_request().await;
    let (status, resp) = send(
        state.clone(),
        "POST",
        "/v1/vms/import",
        Some(&format!("multipart/form-data; boundary={boundary}")),
        None,
        // A fully valid operator JWT: the ONLY missing thing is the CSRF
        // marker, so a 403 here is the CSRF layer, not the auth layer.
        Some(&token),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "response body: {resp}");
    assert_eq!(resp["code"], "CSRF_REJECTED");
    assert_eq!(
        journal_counts(&state).await,
        (0, 0, 0),
        "a token-less multipart POST must not journal anything"
    );
}

#[tokio::test]
async fn multipart_import_with_blank_csrf_header_is_rejected() {
    let (state, boundary, body, token) = import_request().await;
    let (status, resp) = send(
        state.clone(),
        "POST",
        "/v1/vms/import",
        Some(&format!("multipart/form-data; boundary={boundary}")),
        Some("   "),
        Some(&token),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "response body: {resp}");
    assert_eq!(resp["code"], "CSRF_REJECTED");
    assert_eq!(journal_counts(&state).await, (0, 0, 0));
}

// ---------------------------------------------------------------------------
// 3. The JSON path is unchanged
// ---------------------------------------------------------------------------

#[tokio::test]
async fn json_post_still_passes_the_csrf_gate() {
    let state = build_state().await;
    // A JSON POST with no credentials: the CSRF middleware must let it
    // through (JSON is the long-standing admissible shape) so the 401
    // comes from the auth layer — proving the gate did not tighten.
    let (status, resp) = send(
        state.clone(),
        "POST",
        "/v1/vms",
        Some("application/json"),
        None,
        None,
        b"{}".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "response body: {resp}");
    assert_eq!(journal_counts(&state).await, (0, 0, 0));
}

#[tokio::test]
async fn non_json_non_multipart_content_types_still_rejected() {
    // The form-native content types and a missing one keep the exact
    // pre-#496 rejection: 415 CSRF_REJECTED before routing.
    for content_type in [
        Some("application/x-www-form-urlencoded"),
        Some("text/plain"),
        None,
    ] {
        let state = build_state().await;
        let (status, resp) = send(
            state.clone(),
            "POST",
            "/v1/vms",
            content_type,
            // The CSRF header must NOT buy a form-native content type
            // through the gate — only multipart has the header path.
            Some("csrf-token-value"),
            None,
            b"field=value".to_vec(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content-type {content_type:?}: {resp}"
        );
        assert_eq!(
            resp["code"], "CSRF_REJECTED",
            "content-type {content_type:?}"
        );
        assert_eq!(journal_counts(&state).await, (0, 0, 0));
    }
}

// ---------------------------------------------------------------------------
// 4. Content-type lies cannot evade the check
// ---------------------------------------------------------------------------

#[tokio::test]
async fn claiming_json_against_the_import_route_cannot_reach_the_handler() {
    // A multipart route cannot be reached by claiming the JSON content
    // type (the JSON path has no header requirement): the handler's
    // `Multipart` extractor rejects a body with no parseable boundary.
    let (state, _boundary, _body, token) = import_request().await;
    let (status, resp) = send(
        state.clone(),
        "POST",
        "/v1/vms/import",
        Some("application/json"),
        None,
        Some(&token),
        b"{}".to_vec(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "the Multipart extractor's InvalidBoundary rejection: {resp}"
    );
    assert_eq!(journal_counts(&state).await, (0, 0, 0));
}

#[tokio::test]
async fn claiming_multipart_against_a_json_route_is_rejected_without_the_header() {
    // The content-type gate must not open a hole on OTHER routes: a
    // token-less request claiming multipart is 403'd by the middleware
    // before any handler, on every route.
    let (state, boundary, body, token) = import_request().await;
    let (status, resp) = send(
        state.clone(),
        "POST",
        "/v1/vms",
        Some(&format!("multipart/form-data; boundary={boundary}")),
        None,
        Some(&token),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "response body: {resp}");
    assert_eq!(resp["code"], "CSRF_REJECTED");
    assert_eq!(journal_counts(&state).await, (0, 0, 0));
}

#[tokio::test]
async fn claiming_multipart_with_header_against_a_json_route_is_rejected_by_the_extractor() {
    // With the header the middleware passes the request (it carried the
    // un-forgeable marker), but a JSON route's `Json` extractor still
    // refuses a multipart body — no state change via the content-type
    // switch on body-consuming routes. (Body-less handlers can be
    // reached via multipart+header, but only after satisfying the same
    // un-forgeable header marker the JSON path requires.)
    let (state, boundary, body, token) = import_request().await;
    let (status, resp) = send(
        state.clone(),
        "POST",
        "/v1/vms",
        Some(&format!("multipart/form-data; boundary={boundary}")),
        Some("csrf-token-value"),
        Some(&token),
        body,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "the Json extractor's MissingJsonContentType rejection: {resp}"
    );
    assert_eq!(journal_counts(&state).await, (0, 0, 0));
}
