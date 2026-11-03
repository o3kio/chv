//! Durable privileged-mutation audit trail (#658).
//!
//! These tests boot the real `bff_router` and assert the middleware
//! contract end-to-end: exactly one row per mutation, failures and
//! role denials recorded, reads not recorded, admin-only query/export,
//! filtering, pagination, and credential redaction.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use chv_common::SystemClock;
use chv_controlplane_store::{
    AlertRepository, AlertRuleRepository, ApplyRunRepository, AuditLogFilter, AuditLogInsert,
    AuditLogRepository, AuditLogRow, BackupRepository, DesiredStateRepository,
    DriftReportRepository, EventRepository, ImageRepository, NetworkRepository, NodeRepository,
    NotificationOutboxRepository, ObservedStateRepository, OperationRepository, TopologyRepository,
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
        _vm_id: String,
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
    // #660 validates the user's existence for legacy (no `token_version`)
    // test JWTs, so synthetic `sub`s must have rows.
    chv_controlplane_store::test_util::seed_test_users(&pool).await;

    AppState {
        pool: pool.clone(),
        node_repo: NodeRepository::new(pool.clone()),
        operation_repo: OperationRepository::new(pool.clone()),
        event_repo: EventRepository::new(pool.clone()),
        alert_repo: AlertRepository::new(pool.clone()),
        alert_rules: Arc::new(AlertRuleRepository::new(pool.clone())),
        notification_outbox: Arc::new(NotificationOutboxRepository::new(pool.clone())),
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
        console_jwt_secret: "test-console-secret".to_string(),
        monitoring: None,
        monitoring_health: chv_webui_bff::MonitoringHealth::new(),
        agent_runtime_dir: std::path::PathBuf::from("/var/lib/chv/agent"),
        cache: chv_webui_bff::BffCache::new(5),
        clock: Arc::new(SystemClock),
    }
}

fn token(state: &AppState, sub: &str, username: &str, role: &str) -> String {
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_secs()
        + 3600;
    let claims = Claims {
        sub: sub.to_string(),
        username: username.to_string(),
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

fn post_json(path: &str, token: Option<&str>, body: serde_json::Value) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    builder
        .body(Body::from(body.to_string()))
        .expect("build request")
}

fn get(path: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method("GET").uri(path);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    builder.body(Body::empty()).expect("build request")
}

async fn send(state: &AppState, request: Request<Body>) -> axum::response::Response {
    chv_webui_bff::bff_router(state.clone())
        .with_state(state.clone())
        .oneshot(request)
        .await
        .expect("router response")
}

async fn rows_for_path(state: &AppState, path: &str) -> Vec<AuditLogRow> {
    sqlx::query_as::<_, AuditLogRow>(
        "SELECT * FROM audit_log WHERE path = ? ORDER BY occurred_at_ms DESC, audit_id DESC",
    )
    .bind(path)
    .fetch_all(&state.pool)
    .await
    .expect("query audit rows")
}

async fn all_rows(state: &AppState) -> Vec<AuditLogRow> {
    sqlx::query_as::<_, AuditLogRow>(
        "SELECT * FROM audit_log ORDER BY occurred_at_ms DESC, audit_id DESC",
    )
    .fetch_all(&state.pool)
    .await
    .expect("query audit rows")
}

fn body_json(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice(body).expect("response body is JSON")
}

async fn body_bytes(response: axum::response::Response) -> Vec<u8> {
    to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("read response body")
        .to_vec()
}

async fn insert_row(
    state: &AppState,
    at: i64,
    actor: &str,
    action: &str,
    resource_kind: &str,
    outcome: &str,
) {
    AuditLogRepository::new(state.pool.clone())
        .insert(&AuditLogInsert {
            occurred_at_ms: at,
            actor_id: format!("id-{actor}"),
            actor_username: actor.to_string(),
            actor_role: "admin".to_string(),
            action: action.to_string(),
            method: "POST".to_string(),
            path: "/v1/seeded".to_string(),
            resource_kind: Some(resource_kind.to_string()),
            resource_id: Some(format!("{resource_kind}-1")),
            outcome: outcome.to_string(),
            status_code: 200,
            correlation_id: Some("seed-correlation".to_string()),
            operation_id: None,
            source_ip: Some("127.0.0.1".to_string()),
            metadata: r#"{"auth_source":"bearer_jwt"}"#.to_string(),
        })
        .await
        .expect("insert audit row");
}

#[tokio::test]
async fn exactly_one_row_per_successful_mutation() {
    let state = build_state().await;
    let admin = token(&state, "u-admin", "admin", "admin");

    let response = send(
        &state,
        post_json(
            "/v1/users/create",
            Some(&admin),
            serde_json::json!({
                "username": "audited-user",
                "password": "Sup3rSecretPassw0rd!",
                "role": "viewer"
            }),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let rows = rows_for_path(&state, "/v1/users/create").await;
    assert_eq!(rows.len(), 1, "one request must write exactly one row");
    let row = &rows[0];
    assert_eq!(row.action, "user.create");
    assert_eq!(row.resource_kind.as_deref(), Some("user"));
    assert_eq!(row.outcome, "success");
    assert_eq!(row.status_code, 200);
    assert_eq!(row.actor_id, "u-admin");
    assert_eq!(row.actor_username, "admin");
    assert_eq!(row.actor_role, "admin");
    assert!(row
        .correlation_id
        .as_deref()
        .is_some_and(|id| !id.is_empty()));

    // A read request must not add a row.
    let response = send(&state, get("/v1/health", Some(&admin))).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(all_rows(&state).await.len(), 1);
}

#[tokio::test]
async fn failed_mutation_is_recorded_as_failed() {
    let state = build_state().await;
    let admin = token(&state, "u-admin", "admin", "admin");

    let response = send(
        &state,
        post_json(
            "/v1/users/create",
            Some(&admin),
            serde_json::json!({ "username": "missing-password", "role": "viewer" }),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let rows = rows_for_path(&state, "/v1/users/create").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outcome, "failed");
    assert_eq!(rows[0].status_code, 400);
    assert_eq!(rows[0].actor_username, "admin");
}

#[tokio::test]
async fn denied_attempt_is_recorded_with_real_actor_then_anonymous() {
    let state = build_state().await;
    let viewer = token(&state, "u-viewer", "viewer-1", "viewer");

    let response = send(
        &state,
        post_json(
            "/v1/vms/delete",
            Some(&viewer),
            serde_json::json!({ "vm_id": "vm-secret-id" }),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let rows = rows_for_path(&state, "/v1/vms/delete").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outcome, "denied");
    assert_eq!(rows[0].status_code, 403);
    assert_eq!(rows[0].actor_id, "u-viewer");
    assert_eq!(rows[0].actor_username, "viewer-1");
    assert_eq!(rows[0].actor_role, "viewer");

    // No credential: explicit anonymous actor, never a forged identity.
    let response = send(
        &state,
        post_json(
            "/v1/vms/delete",
            None,
            serde_json::json!({ "vm_id": "vm-secret-id" }),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let rows = rows_for_path(&state, "/v1/vms/delete").await;
    assert_eq!(rows.len(), 2);
    let anonymous = rows.iter().find(|row| row.status_code == 401).unwrap();
    assert_eq!(anonymous.outcome, "denied");
    assert_eq!(anonymous.actor_id, "anonymous");
    assert_eq!(anonymous.actor_username, "anonymous");
    assert_eq!(anonymous.actor_role, "anonymous");
}

#[tokio::test]
async fn get_and_read_shaped_posts_are_not_recorded() {
    let state = build_state().await;
    let admin = token(&state, "u-admin", "admin", "admin");

    assert_eq!(
        send(&state, get("/v1/health", Some(&admin))).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        send(
            &state,
            post_json("/v1/audit-log", Some(&admin), serde_json::json!({})),
        )
        .await
        .status(),
        StatusCode::OK
    );
    // Rendering a cloud-init template is a pure read: the POST must not
    // append an audit row even when the template is missing (404).
    assert_eq!(
        send(
            &state,
            post_json(
                "/v1/cloud-init-templates/t-missing/render",
                Some(&admin),
                serde_json::json!({}),
            ),
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert!(all_rows(&state).await.is_empty());
}

#[tokio::test]
async fn admin_query_is_authorized_filtered_and_paginated() {
    let state = build_state().await;
    let admin = token(&state, "u-admin", "admin", "admin");
    let operator = token(&state, "u-op", "operator", "operator");
    let viewer = token(&state, "u-viewer", "viewer", "viewer");

    insert_row(&state, 1_000, "alice", "vm.create", "vm", "success").await;
    insert_row(&state, 2_000, "alice", "vm.delete", "vm", "denied").await;
    insert_row(&state, 3_000, "bob", "network.update", "network", "failed").await;

    for denied in [&operator, &viewer] {
        let response = send(
            &state,
            post_json("/v1/audit-log", Some(denied), serde_json::json!({})),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    let response = send(
        &state,
        post_json(
            "/v1/audit-log",
            Some(&admin),
            serde_json::json!({
                "actor": "alice",
                "action": "vm.delete",
                "outcome": "denied",
                "from_ms": 1_500,
                "to_ms": 2_500
            }),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(&body_bytes(response).await);
    assert_eq!(body["total"], 1);
    assert_eq!(body["count"], 1);
    assert_eq!(body["items"][0]["actor_username"], "alice");
    assert_eq!(body["items"][0]["resource_kind"], "vm");
    assert_eq!(body["items"][0]["outcome"], "denied");

    let response = send(
        &state,
        post_json(
            "/v1/audit-log",
            Some(&admin),
            serde_json::json!({ "limit": 1, "offset": 1 }),
        ),
    )
    .await;
    let body = body_json(&body_bytes(response).await);
    assert_eq!(body["total"], 3);
    assert_eq!(body["count"], 1);
    assert_eq!(body["limit"], 1);
    assert_eq!(body["offset"], 1);
}

#[tokio::test]
async fn export_is_admin_only_and_honors_filters() {
    let state = build_state().await;
    let admin = token(&state, "u-admin", "admin", "admin");
    let viewer = token(&state, "u-viewer", "viewer", "viewer");

    insert_row(&state, 1_000, "alice", "vm.create", "vm", "success").await;
    insert_row(&state, 2_000, "bob", "vm.delete", "vm", "denied").await;

    let response = send(
        &state,
        post_json(
            "/v1/audit-log/export",
            Some(&viewer),
            serde_json::json!({ "format": "json" }),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let response = send(
        &state,
        post_json(
            "/v1/audit-log/export",
            Some(&admin),
            serde_json::json!({ "actor": "alice", "format": "json" }),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "application/json"
    );
    let body = body_json(&body_bytes(response).await);
    assert_eq!(body["total"], 1);
    assert_eq!(body["count"], 1);
    assert_eq!(body["items"][0]["actor_username"], "alice");

    let response = send(
        &state,
        post_json(
            "/v1/audit-log/export",
            Some(&admin),
            serde_json::json!({ "format": "csv" }),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "text/csv; charset=utf-8"
    );
    let csv = String::from_utf8(body_bytes(response).await).expect("CSV is UTF-8");
    assert!(csv.starts_with("audit_id,occurred_at,occurred_at_ms,"));
    assert!(csv.contains("alice"));
    assert!(csv.contains("bob"));
    assert_eq!(csv.lines().count(), 3, "header plus two rows");
}

#[tokio::test]
async fn credentials_are_never_persisted_in_the_audit_trail() {
    let state = build_state().await;
    let admin = token(&state, "u-admin", "admin", "admin");
    let password = "SuperSecretPassw0rd!";
    let cookie_token = "forged-cookie-token-that-must-not-be-stored";
    let s3_secret = "AKIAIOSFODNN7EXAMPLE-S3-SECRET";

    let request = Request::builder()
        .method("POST")
        .uri("/v1/users/create")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {admin}"))
        .header(header::COOKIE, format!("chv_session={cookie_token}"))
        .header("x-s3-secret-key", s3_secret)
        .body(Body::from(
            serde_json::json!({
                "username": "redaction-user",
                "password": password,
                "role": "viewer"
            })
            .to_string(),
        ))
        .expect("build request");
    let response = send(&state, request).await;
    assert_eq!(response.status(), StatusCode::OK);

    let rows = all_rows(&state).await;
    assert_eq!(rows.len(), 1);
    let dump = format!("{:?}", rows[0]);
    for secret in [
        password,
        admin.as_str(),
        cookie_token,
        s3_secret,
        "Authorization",
        "Cookie",
        "chv_session",
    ] {
        assert!(
            !dump.contains(secret),
            "audit row contains forbidden credential material {secret:?}: {dump}"
        );
    }
    assert!(dump.contains("bearer_jwt"));
}

#[tokio::test]
async fn retention_removes_only_expired_rows_through_repository() {
    // Store-level retention coverage also lives in
    // chv-controlplane-store; this asserts the same contract against the
    // BFF's migrated database shape.
    let state = build_state().await;
    insert_row(&state, 1_000, "alice", "vm.create", "vm", "success").await;
    insert_row(&state, 2_000, "alice", "vm.delete", "vm", "success").await;

    let repo = AuditLogRepository::new(state.pool.clone());
    let deleted = repo.delete_older_than(2_000).await.expect("retention");
    assert_eq!(deleted, 1);
    let (rows, total) = repo
        .query(&AuditLogFilter::default(), 50, 0)
        .await
        .expect("query after retention");
    assert_eq!(total, 1);
    assert_eq!(rows[0].occurred_at_ms, 2_000);
}

#[tokio::test]
async fn security_relevant_auth_routes_are_audited_but_login_is_not() {
    let state = build_state().await;
    let admin = token(&state, "u-admin", "admin", "admin");

    // Logout severs a session: a session thief's seizure step. Audited.
    let response = send(
        &state,
        post_json("/v1/auth/logout", Some(&admin), serde_json::json!({})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let rows = rows_for_path(&state, "/v1/auth/logout").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].action, "auth.logout");
    assert_eq!(rows[0].resource_kind.as_deref(), Some("user"));
    assert_eq!(rows[0].actor_id, "u-admin");
    assert_eq!(rows[0].outcome, "success");
    assert_eq!(rows[0].status_code, 200);

    // change-password with a body that passes policy but an unknown
    // actor: 401 from the handler, still recorded with the real
    // (token-verified) actor and never the body.
    let response = send(
        &state,
        post_json(
            "/v1/auth/change-password",
            Some(&admin),
            serde_json::json!({
                "current_password": "WrongButWellFormed1!",
                "new_password": "AnotherWellFormed1!"
            }),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let rows = rows_for_path(&state, "/v1/auth/change-password").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].action, "auth.change_password");
    assert_eq!(rows[0].actor_id, "u-admin");
    assert_eq!(rows[0].outcome, "denied");
    assert_eq!(rows[0].status_code, 401);
    // The passwords never reach the durable row.
    let dump = format!("{:?}", rows[0]);
    assert!(!dump.contains("WrongButWellFormed1"));
    assert!(!dump.contains("AnotherWellFormed1"));

    // Login is credential entry (anonymous by definition, password in
    // the body, brute-force volume accounted by rate limiting): NO row.
    let response = send(
        &state,
        post_json(
            "/v1/auth/login",
            None,
            serde_json::json!({ "username": "nobody", "password": "guess" }),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(
        rows_for_path(&state, "/v1/auth/login").await.is_empty(),
        "login must not be audited"
    );
    assert_eq!(all_rows(&state).await.len(), 2);
}

#[tokio::test]
async fn csrf_rejected_mutation_is_recorded() {
    let state = build_state().await;
    let admin = token(&state, "u-admin", "admin", "admin");

    // A form-native content type is rejected by the CSRF middleware
    // with 415 before any handler runs. The audit layer wraps the CSRF
    // layer, so the rejected attempt is still recorded — attribution
    // comes from the decoded credential, not the request shape.
    let request = Request::builder()
        .method("POST")
        .uri("/v1/vms/delete")
        .header(header::CONTENT_TYPE, "text/plain")
        .header(header::AUTHORIZATION, format!("Bearer {admin}"))
        .body(Body::from("vm_id=vm-1"))
        .expect("build request");
    let response = send(&state, request).await;
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

    let rows = rows_for_path(&state, "/v1/vms/delete").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].action, "vm.delete");
    assert_eq!(rows[0].status_code, 415);
    assert_eq!(rows[0].outcome, "failed");
    assert_eq!(rows[0].actor_id, "u-admin");
}

#[tokio::test]
async fn client_supplied_identifiers_are_capped() {
    let state = build_state().await;
    let admin = token(&state, "u-admin", "admin", "admin");

    // A hostile or buggy client sends a megabyte-scale identifier. The
    // stored operation_id and correlation_id must be capped (review L2)
    // so a row cannot be ballooned by header content.
    let huge = "x".repeat(5_000);
    let request = Request::builder()
        .method("POST")
        .uri("/v1/users/create")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {admin}"))
        .header("x-operation-id", huge.clone())
        .body(Body::from(
            serde_json::json!({
                "username": "capped-id-user",
                "password": "Sup3rSecretPassw0rd!",
                "role": "viewer"
            })
            .to_string(),
        ))
        .expect("build request");
    let response = send(&state, request).await;
    assert_eq!(response.status(), StatusCode::OK);

    let rows = rows_for_path(&state, "/v1/users/create").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].operation_id.as_deref().map(str::len),
        Some(128),
        "operation_id must be capped at 128 chars"
    );
    assert_eq!(
        rows[0].correlation_id.as_deref().map(str::len),
        Some(128),
        "correlation_id must be capped at 128 chars"
    );
    assert_eq!(rows[0].operation_id, rows[0].correlation_id);

    // Path-derived fields are capped too: a matched route with a
    // hostile 1 KiB id segment must not balloon the row (review R2).
    // `/v1/vm-templates/:id` DELETE is used because its handler touches
    // only `state.pool` — `export_vm` additionally acquires a pooled
    // connection, which deadlocks the test pool's single connection.
    let long_segment = "a".repeat(1_024);
    let response = send(
        &state,
        Request::builder()
            .method("DELETE")
            .uri(format!("/v1/vm-templates/{long_segment}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::AUTHORIZATION, format!("Bearer {admin}"))
            .body(Body::empty())
            .expect("build request"),
    )
    .await;
    let status = response.status();
    let response_body = body_bytes(response).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "unexpected body: {}",
        String::from_utf8_lossy(&response_body)
    );
    let template_row = all_rows(&state)
        .await
        .into_iter()
        .find(|row| row.action == "vm_template.delete")
        .expect("vm_template.delete row");
    assert_eq!(template_row.path.chars().count(), 256, "path capped at 256");
    assert_eq!(
        template_row
            .resource_id
            .as_deref()
            .map(|v| v.chars().count()),
        Some(256),
        "resource_id capped at 256"
    );
}

#[tokio::test]
async fn audit_write_failure_fails_closed_with_correlation_handle() {
    let state = build_state().await;
    let admin = token(&state, "u-admin", "admin", "admin");

    // Break the audit store AFTER startup (an operator could drop the
    // table, or the volume could fill): the mutation itself succeeds,
    // but the response must not claim success without its durable
    // record. The 500 replacement carries the correlation id so the
    // operator can find what the mutation actually did (review M3).
    sqlx::query("DROP TABLE audit_log")
        .execute(&state.pool)
        .await
        .expect("drop audit table");

    let response = send(
        &state,
        post_json(
            "/v1/users/create",
            Some(&admin),
            serde_json::json!({
                "username": "unrecorded-user",
                "password": "Sup3rSecretPassw0rd!",
                "role": "viewer"
            }),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let correlation = response
        .headers()
        .get("x-correlation-id")
        .and_then(|v| v.to_str().ok())
        .expect("correlation id header")
        .to_string();
    let body = body_json(&body_bytes(response).await);
    assert_eq!(body["code"], "AUDIT_LOG_WRITE_FAILED");
    assert_eq!(body["request_id"], correlation.as_str());
    assert!(!correlation.is_empty());
}
