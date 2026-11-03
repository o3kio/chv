//! Router-level role-gate tests.
//!
//! These tests boot the real `bff_router` (not just the handler functions)
//! and assert that the operator-only Architecture Designer routes refuse
//! viewer-role JWTs at the routing layer. This guards against accidental
//! regressions where a route is moved back into the viewer block — see
//! Phase-5 reviewer F1 (`/v1/architectures/runs/list`) and Phase-6
//! reviewer M2 (`/v1/architectures/drift`).
//!
//! Pattern mirrors the in-crate end-to-end test
//! `protected_route_blocked_when_must_change_password_set` in
//! `src/handlers/auth.rs`: build an in-memory state, mint a viewer JWT,
//! POST to the route via `tower::ServiceExt::oneshot`, assert HTTP 403.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chv_common::{ManualClock, SystemClock};
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

/// Stub MutationService — these tests never reach a mutation path; we just
/// need a value that satisfies the `Arc<dyn MutationService>` field on
/// `AppState`.
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
        // SystemClock is fine — these tests never depend on the BFF's clock
        // because the request short-circuits at the role-gate middleware
        // before any handler runs.
        clock: Arc::new(SystemClock),
    }
}

/// Manual-clock variant for the operator-passthrough sanity check. The
/// drift handler reads `state.clock`, so a deterministic clock keeps the
/// 200-path side of the test stable.
async fn build_state_manual() -> AppState {
    let mut s = build_state().await;
    s.clock = Arc::new(ManualClock::new(chrono::Utc::now()));
    s
}

fn token_for(state: &AppState, role: &str) -> String {
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let claims = Claims {
        sub: "u-tester".to_string(),
        username: "tester".to_string(),
        role: role.to_string(),
        exp,
        must_change_password: false,
    };
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
    jsonwebtoken::encode(
        &header,
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(state.jwt_secret.as_bytes()),
    )
    .expect("encode test token")
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
// Phase-5 carryover F1: /v1/architectures/runs/list is operator-gated.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn runs_list_rejects_viewer_with_403() {
    let state = build_state().await;
    let viewer = token_for(&state, "viewer");
    let status = post_with_token(
        state,
        "/v1/architectures/runs/list",
        &viewer,
        r#"{"id":"any"}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "viewer must not reach /v1/architectures/runs/list"
    );
}

#[tokio::test]
async fn runs_list_lets_operator_through_to_handler() {
    // Sanity check: operator JWT clears the middleware. The handler then
    // 404s on the unknown architecture id, which is the desired post-
    // middleware behaviour. We just need to confirm we are NOT seeing a
    // 403 from the role gate.
    let state = build_state().await;
    let operator = token_for(&state, "operator");
    let status = post_with_token(
        state,
        "/v1/architectures/runs/list",
        &operator,
        r#"{"id":"no-such-arch"}"#,
    )
    .await;
    assert_ne!(
        status,
        StatusCode::FORBIDDEN,
        "operator must clear /v1/architectures/runs/list role gate"
    );
}

// ---------------------------------------------------------------------------
// Phase-6 M2: /v1/architectures/drift is operator-gated.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn drift_rejects_viewer_with_403() {
    let state = build_state_manual().await;
    let viewer = token_for(&state, "viewer");
    let status = post_with_token(
        state,
        "/v1/architectures/drift",
        &viewer,
        r#"{"id":"any","force_refresh":false}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "viewer must not reach /v1/architectures/drift"
    );
}

#[tokio::test]
async fn drift_lets_operator_through_to_handler() {
    let state = build_state_manual().await;
    let operator = token_for(&state, "operator");
    let status = post_with_token(
        state,
        "/v1/architectures/drift",
        &operator,
        r#"{"id":"no-such-arch","force_refresh":false}"#,
    )
    .await;
    assert_ne!(
        status,
        StatusCode::FORBIDDEN,
        "operator must clear /v1/architectures/drift role gate"
    );
}

// ---------------------------------------------------------------------------
// Native alerting routes (#602 PR-6, query/alerts contract v1):
// reads are Viewer-gated, rule/incident mutations are Operator-gated,
// the delivery test is Admin-gated.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn alert_reads_are_viewer_accessible() {
    let state = build_state().await;
    let viewer = token_for(&state, "viewer");
    for (path, body) in [
        ("/v1/monitoring/alerts", "{}"),
        ("/v1/monitoring/alerts/detail", r#"{"alert_id":"none"}"#),
        ("/v1/monitoring/alert-rules", "{}"),
        ("/v1/monitoring/notifications/deliveries", "{}"),
    ] {
        let status = post_with_token(state.clone(), path, &viewer, body).await;
        assert_ne!(
            status,
            StatusCode::FORBIDDEN,
            "viewer must clear the role gate for {path}"
        );
        assert_ne!(
            status,
            StatusCode::UNAUTHORIZED,
            "viewer must authenticate for {path}"
        );
    }
}

#[tokio::test]
async fn alert_rule_mutations_reject_viewer_with_403() {
    let state = build_state().await;
    let viewer = token_for(&state, "viewer");
    for (path, body) in [
        (
            "/v1/monitoring/alert-rules/create",
            r#"{"name":"x","target_kind":"vm","target_id":"vm-x","severity":"warning"}"#,
        ),
        (
            "/v1/monitoring/alert-rules/update",
            r#"{"rule_id":"none","expected_revision":1,"name":"x","severity":"warning"}"#,
        ),
        (
            "/v1/monitoring/alert-rules/delete",
            r#"{"rule_id":"none","expected_revision":1}"#,
        ),
        (
            "/v1/monitoring/alerts/acknowledge",
            r#"{"alert_id":"none"}"#,
        ),
        (
            "/v1/monitoring/alerts/silence",
            r#"{"alert_id":"none","duration_minutes":30}"#,
        ),
    ] {
        let status = post_with_token(state.clone(), path, &viewer, body).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "viewer must not reach {path}"
        );
    }
}

#[tokio::test]
async fn notifications_test_rejects_operator_and_viewer_with_403() {
    // The delivery test is Admin-only: it exercises the operator's
    // notification destination.
    let state = build_state().await;
    for role in ["viewer", "operator"] {
        let token = token_for(&state, role);
        let status = post_with_token(
            state.clone(),
            "/v1/monitoring/notifications/test",
            &token,
            "{}",
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{role} must not reach /v1/monitoring/notifications/test"
        );
    }
}

// ---------------------------------------------------------------------------
// #654: the backup/restore surface is hard-fenced, not "temporarily empty".
// ---------------------------------------------------------------------------

#[tokio::test]
async fn backup_surface_answers_501_not_implemented() {
    let state = build_state().await;
    let operator = token_for(&state, "operator");

    // Operator write block.
    let app = chv_webui_bff::bff_router(state.clone()).with_state(state.clone());
    let req = Request::builder()
        .method("POST")
        .uri("/v1/backups/jobs")
        .header("authorization", format!("Bearer {operator}"))
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], "NOT_IMPLEMENTED");
    assert!(
        json["message"]
            .as_str()
            .is_some_and(|m| m.contains("operator's responsibility")),
        "fence message must name the operator responsibility: {json}"
    );

    // Viewer read block — and the legacy POST alias.
    for (method, path) in [
        ("GET", "/v1/backups/jobs"),
        ("GET", "/v1/backups/restores"),
        ("POST", "/v1/backup-jobs"),
    ] {
        let app = chv_webui_bff::bff_router(state.clone()).with_state(state.clone());
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {operator}"))
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        assert_eq!(
            app.oneshot(req).await.unwrap().status(),
            StatusCode::NOT_IMPLEMENTED,
            "{method} {path} must be fenced"
        );
    }
}
