//! Integration tests for the #368 P3 Failed-wins render in the BFF.
//!
//! Issue kubedoio/chv#368: a VM create whose Core effect terminally failed
//! on the agent is reported by the agent (#368 P1) as
//! `runtime_status='Failed'` with the public-safe failure code as
//! `last_error`. The BFF must render that as the VM's power state — never
//! the desired power state (the "phantom Running" zombie) — everywhere it
//! shows a power state.
//!
//! `vms.rs` (list + detail) got the Failed-wins `CASE` in the original #368
//! PR; this suite pins the SAME precedence for the node-detail hosted-VM
//! list (`nodes.rs` — the round-2 review finding: it still rendered
//! desired-first, so a #368 failed-create phantom showed as Running on
//! /nodes/[id]), plus the sibling `/v1/vms` list rendering for the same
//! fixture as the control pattern.
//!
//! Contract asserted:
//! - a VM whose observed `runtime_status='Failed'` renders
//!   `power_state="Failed"` with `last_error` surfaced, on BOTH the node
//!   detail hosted-VM list and the VM list — even though the desired power
//!   state says `Running`;
//! - a failure-free VM keeps the desired-first precedence (desired
//!   `Running` renders `Running`);
//! - a VM with no observed state at all still renders the desired state.

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

/// Seed a viewer JWT (the read routes only require authentication).
async fn seed_jwt(state: &AppState) -> String {
    sqlx::query(
        "INSERT INTO users (user_id, username, password_hash, role, must_change_password) \
         VALUES ('u-viewer', 'viewer', 'x', 'viewer', 0)",
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
        sub: "u-viewer".to_string(),
        username: "viewer".to_string(),
        role: "viewer".to_string(),
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

/// Seed the single node all fixtures live on.
async fn seed_node(state: &AppState) {
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('n-1', 'h-1', 'Node 1')",
    )
    .execute(&state.pool)
    .await
    .expect("seed node");
}

/// Seed one VM on the node with the given desired power state.
async fn seed_vm_on_node(state: &AppState, vm_id: &str, desired_power_state: &str) {
    sqlx::query("INSERT INTO vms (vm_id, node_id, display_name) VALUES (?, 'n-1', ?)")
        .bind(vm_id)
        .bind(format!("VM {vm_id}"))
        .execute(&state.pool)
        .await
        .expect("seed vm");
    sqlx::query(
        "INSERT INTO vm_desired_state \
         (vm_id, desired_generation, desired_status, desired_power_state, target_node_id, \
          cpu_count, memory_bytes) \
         VALUES (?, 1, 'Active', ?, 'n-1', 2, 2147483648)",
    )
    .bind(vm_id)
    .bind(desired_power_state)
    .execute(&state.pool)
    .await
    .expect("seed vm_desired_state");
}

/// Seed the agent-reported observed state for a VM (#368 P1 shape: a
/// terminally-failed create reports Failed with the public-safe code).
async fn seed_observed_state(
    state: &AppState,
    vm_id: &str,
    runtime_status: &str,
    last_error: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO vm_observed_state \
         (vm_id, observed_generation, runtime_status, node_id, last_error, health_status) \
         VALUES (?, 0, ?, 'n-1', ?, 'unknown')",
    )
    .bind(vm_id)
    .bind(runtime_status)
    .bind(last_error)
    .execute(&state.pool)
    .await
    .expect("seed vm_observed_state");
}

/// #368 P3 (round-2 fix): the node-detail hosted-VM list must render an
/// agent-reported Failed state as `power_state="Failed"` with `last_error`
/// surfaced — never the desired power state, which for a terminally-failed
/// create is the phantom "Running". Mirrors the Failed-wins CASE the VM
/// list and detail views already apply.
#[tokio::test]
async fn node_detail_hosted_vm_list_renders_failed_over_desired_state() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;

    // The #368 zombie shape: desired Running, agent reports the create
    // terminally failed.
    seed_vm_on_node(&state, "vm-phantom", "Running").await;
    seed_observed_state(&state, "vm-phantom", "Failed", Some("STORD_ATTACH_FAILED")).await;
    // Controls: a healthy VM keeps the desired-first precedence, and a VM
    // with no observed state at all still renders the desired state.
    seed_vm_on_node(&state, "vm-healthy", "Running").await;
    seed_observed_state(&state, "vm-healthy", "Running", None).await;
    seed_vm_on_node(&state, "vm-unobserved", "Running").await;

    let (status, body) = post_with_token(
        state.clone(),
        "/v1/nodes/get",
        &token,
        r#"{"node_id":"n-1"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "node detail body: {body}");

    let hosted = body["hostedVms"].as_array().expect("hostedVms array");
    let find = |vm_id: &str| {
        hosted
            .iter()
            .find(|vm| vm["vm_id"] == vm_id)
            .unwrap_or_else(|| panic!("hosted VM {vm_id} missing: {hosted:?}"))
    };

    let phantom = find("vm-phantom");
    assert_eq!(
        phantom["power_state"], "Failed",
        "a terminally-failed create must render Failed, not the desired phantom: {phantom:?}"
    );
    assert_eq!(
        phantom["last_error"], "STORD_ATTACH_FAILED",
        "the reported failure code must be surfaced: {phantom:?}"
    );

    assert_eq!(find("vm-healthy")["power_state"], "Running");
    assert_eq!(find("vm-unobserved")["power_state"], "Running");
}

/// The sibling `/v1/vms` list rendering for the same fixture: the same
/// Failed-wins precedence (the pattern the node-detail list now mirrors).
#[tokio::test]
async fn vm_list_renders_failed_over_desired_state() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;

    seed_vm_on_node(&state, "vm-phantom", "Running").await;
    seed_observed_state(&state, "vm-phantom", "Failed", Some("STORD_ATTACH_FAILED")).await;
    seed_vm_on_node(&state, "vm-healthy", "Running").await;
    seed_observed_state(&state, "vm-healthy", "Running", None).await;

    let (status, body) = post_with_token(
        state.clone(),
        "/v1/vms",
        &token,
        r#"{"page":1,"page_size":50}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "vm list body: {body}");

    let items = body["items"].as_array().expect("items array");
    let find = |vm_id: &str| {
        items
            .iter()
            .find(|vm| vm["vm_id"] == vm_id)
            .unwrap_or_else(|| panic!("VM {vm_id} missing: {items:?}"))
    };

    let phantom = find("vm-phantom");
    assert_eq!(phantom["power_state"], "Failed", "phantom: {phantom:?}");
    assert_eq!(
        phantom["last_error"], "STORD_ATTACH_FAILED",
        "phantom failure code: {phantom:?}"
    );
    assert_eq!(find("vm-healthy")["power_state"], "Running");
}
