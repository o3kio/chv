//! Integration tests for the operator health routes (`chvctl health ...`).
//!
//! Issue #320: `chvctl health check | cluster | report <id>` issue GET
//! requests to `/v1/health`, `/v1/cluster/health`, and
//! `/v1/nodes/{node_id}/health`, which previously hit no route at all
//! (404/501 on every healthy deployment). These tests boot the real
//! `bff_router` and assert the read-only contract end-to-end:
//!
//! - authentication is required (anonymous request -> 401);
//! - `/v1/health` reports database reachability;
//! - `/v1/cluster/health` aggregates `node_observed_state` with a stable
//!   shape and an overall status;
//! - `/v1/nodes/{id}/health` returns the per-node observed health and 404s
//!   on unknown node ids.

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
        monitoring: None,
        monitoring_health: chv_webui_bff::MonitoringHealth::new(),
        agent_runtime_dir: std::path::PathBuf::from("/var/lib/chv/agent"),
        cache: chv_webui_bff::BffCache::new(5),
        clock: Arc::new(SystemClock),
    }
}

/// Seed a viewer user and return a usable JWT bearer token.
async fn seed_jwt(state: &AppState) -> String {
    sqlx::query(
        "INSERT INTO users (user_id, username, password_hash, role, must_change_password) \
         VALUES ('u-ops', 'ops', 'x', 'operator', 0)",
    )
    .execute(&state.pool)
    .await
    .expect("seed user");

    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let claims = chv_webui_bff::auth::Claims {
        sub: "u-ops".to_string(),
        username: "ops".to_string(),
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

/// Seed one node with the given observed health status.
async fn seed_node(state: &AppState, node_id: &str, health: Option<&str>) {
    sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES (?, ?, ?)")
        .bind(node_id)
        .bind(format!("host-{node_id}"))
        .bind(format!("Node {node_id}"))
        .execute(&state.pool)
        .await
        .expect("seed node");

    sqlx::query(
        "INSERT INTO node_observed_state (node_id, observed_generation, observed_state, \
         health_status, runtime_status) VALUES (?, 1, 'TenantReady', ?, 'Running')",
    )
    .bind(node_id)
    .bind(health)
    .execute(&state.pool)
    .await
    .expect("seed observed state");
}

async fn get_with_token(
    state: AppState,
    path: &str,
    token: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let app = chv_webui_bff::bff_router(state.clone()).with_state(state);
    let mut builder = Request::builder().method("GET").uri(path);
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    let req = builder.body(Body::empty()).unwrap();
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

// ---------------------------------------------------------------------------
// /v1/health
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_requires_authentication() {
    let state = build_state().await;
    let (status, _) = get_with_token(state, "/v1/health", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn health_reports_database_reachable() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    let (status, body) = get_with_token(state, "/v1/health", Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["database"], "reachable");
}

// ---------------------------------------------------------------------------
// /v1/cluster/health
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cluster_health_aggregates_node_states() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state, "n-1", Some("healthy")).await;
    seed_node(&state, "n-2", Some("healthy")).await;
    seed_node(&state, "n-3", Some("degraded")).await;

    let (status, body) = get_with_token(state, "/v1/cluster/health", Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "degraded");
    assert_eq!(body["total_nodes"], 3);
    assert_eq!(body["healthy"], 2);
    assert_eq!(body["degraded"], 1);
    assert_eq!(body["critical"], 0);
    assert_eq!(body["warning"], 0);
    assert_eq!(body["unknown"], 0);
}

#[tokio::test]
async fn cluster_health_critical_dominates_and_empty_fleet_is_unknown() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;

    let (status, body) = get_with_token(state, "/v1/cluster/health", Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "unknown");
    assert_eq!(body["total_nodes"], 0);

    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state, "n-1", Some("critical")).await;
    seed_node(&state, "n-2", None).await;
    let (status, body) = get_with_token(state, "/v1/cluster/health", Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "critical");
    assert_eq!(body["unknown"], 1);
    assert_eq!(body["critical"], 1);
}

// ---------------------------------------------------------------------------
// /v1/nodes/{node_id}/health
// ---------------------------------------------------------------------------

#[tokio::test]
async fn node_health_returns_observed_state() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state, "n-1", Some("healthy")).await;

    let (status, body) = get_with_token(state, "/v1/nodes/n-1/health", Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["node_id"], "n-1");
    assert_eq!(body["hostname"], "host-n-1");
    assert_eq!(body["state"], "TenantReady");
    assert_eq!(body["health"], "healthy");
    assert_eq!(body["runtime_status"], "Running");
}

#[tokio::test]
async fn node_health_unknown_node_is_404() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    let (status, _) = get_with_token(state, "/v1/nodes/nope/health", Some(&token)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
