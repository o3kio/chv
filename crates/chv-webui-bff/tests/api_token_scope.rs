//! Integration tests for `api_tokens.scope` enforcement (Security T1).
//!
//! Token auth (the `chv_...` bearer path in `crate::auth::BearerToken`)
//! previously selected the user's role straight from `users.role` and
//! ignored `api_tokens.scope`, so a `readonly`-scoped token carried full
//! write authority. The fixed contract:
//!
//! - `scope = "full"`     -> token keeps the user's role.
//! - `scope = "readonly"` -> token is demoted to the viewer role.
//! - any other value      -> the token is REJECTED with 403
//!   `INVALID_TOKEN_SCOPE` (fail closed, #660). There is no fallback to the
//!   user's role: an unrecognized scope must never inherit mutation
//!   authority.
//!
//! These tests boot the real `bff_router` and assert the role middleware
//! behavior end-to-end: a readonly token is rejected (403) on a mutating
//! route and accepted (200) on a read route; a full token keeps the user's
//! role (reaches the handler — 404 on an unknown id, not 403).

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

/// Seed an operator user plus an API token row with the given scope. The
/// returned raw token string is the credential clients present as
/// `Authorization: Bearer chv_...`.
async fn seed_api_token(
    state: &AppState,
    user_id: &str,
    username: &str,
    user_role: &str,
    scope: &str,
) -> String {
    sqlx::query(
        "INSERT INTO users (user_id, username, password_hash, role, must_change_password) \
         VALUES (?, ?, 'x', ?, 0)",
    )
    .bind(user_id)
    .bind(username)
    .bind(user_role)
    .execute(&state.pool)
    .await
    .expect("seed user");

    let raw = format!("chv_{}", "a".repeat(64));
    let token_hash = chv_common::sha256_hex(&raw);
    sqlx::query(
        "INSERT INTO api_tokens (token_id, user_id, name, token_hash, scope) \
         VALUES (?, ?, 'test-token', ?, ?)",
    )
    .bind(format!("tok-{user_id}-{scope}"))
    .bind(user_id)
    .bind(&token_hash)
    .bind(scope)
    .execute(&state.pool)
    .await
    .expect("seed api token");

    raw
}

async fn post_with_token(state: AppState, path: &str, token: &str, body: &str) -> StatusCode {
    let app = chv_webui_bff::bff_router(state.clone()).with_state(state);
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    app.oneshot(req).await.unwrap().status()
}

// ---------------------------------------------------------------------------
// readonly scope
// ---------------------------------------------------------------------------

#[tokio::test]
async fn readonly_scope_token_is_rejected_on_mutating_endpoint() {
    let state = build_state().await;
    let token = seed_api_token(&state, "u-svc", "svc", "operator", "readonly").await;

    // /v1/architectures/plan is operator-gated; the demoted viewer role must
    // be rejected by the operator middleware with 403.
    let status = post_with_token(
        state,
        "/v1/architectures/plan",
        &token,
        r#"{"id":"arch-x"}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "readonly token must not reach a mutating route"
    );
}

#[tokio::test]
async fn readonly_scope_token_works_on_read_endpoint() {
    let state = build_state().await;
    let token = seed_api_token(&state, "u-svc", "svc", "operator", "readonly").await;

    // /v1/overview is viewer-gated and read-only: the demoted viewer role
    // passes.
    let status = post_with_token(state, "/v1/overview", &token, "{}").await;
    assert_eq!(status, StatusCode::OK, "readonly token must read");
}

// ---------------------------------------------------------------------------
// full scope and fail-closed unknown scopes (#660)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_scope_token_keeps_the_users_role() {
    let state = build_state().await;
    let token = seed_api_token(&state, "u-svc", "svc", "operator", "full").await;

    // Operator role + full scope: the token clears the operator middleware
    // and reaches the handler, which answers 404 for the unknown id (NOT the
    // middleware's 403).
    let status = post_with_token(
        state,
        "/v1/architectures/plan",
        &token,
        r#"{"id":"arch-x"}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "full-scope token must keep the user's operator role"
    );
}

#[tokio::test]
async fn unknown_scope_is_rejected_with_invalid_token_scope() {
    let state = build_state().await;
    let token = seed_api_token(&state, "u-svc", "svc", "operator", "banana").await;

    // #660 hardening: an unrecognized scope is a hard 403, never a fallback
    // to the user's role. The token must not reach the handler (which would
    // answer 404 for the unknown id) and must not inherit operator authority.
    let status = post_with_token(
        state.clone(),
        "/v1/architectures/plan",
        &token,
        r#"{"id":"arch-x"}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "unknown scope must fail closed"
    );

    // Even a read-only route rejects it: the credential itself is invalid.
    let status = post_with_token(state, "/v1/overview", &token, "{}").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn empty_scope_is_rejected() {
    let state = build_state().await;
    let token = seed_api_token(&state, "u-svc", "svc", "admin", "").await;
    let status = post_with_token(state, "/v1/overview", &token, "{}").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn mixed_case_scope_is_rejected() {
    // Scope matching is exact: "READONLY" is not "readonly".
    let state = build_state().await;
    let token = seed_api_token(&state, "u-svc", "svc", "operator", "READONLY").await;
    let status = post_with_token(state, "/v1/overview", &token, "{}").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
