//! BFF route tests for the vm-mutate migrate action's `pause_first`
//! field (issue #394, Option C; fail-closed gate per issue #656) — the
//! API-boundary discipline of stop-the-world migration.
//!
//! The chvctl contract row (`vm_migrate_pause_first_row`) pins the
//! CLI→body→handler→mutation-service thread with a well-typed bool;
//! these tests pin the boundary itself, through the real `bff_router`:
//!
//! 1. **A present non-bool `pause_first` is rejected, not coerced** —
//!    `"pause_first": "true"` (quoted) or any other non-boolean JSON
//!    value must fail 400 before the mutation service runs.
//! 2. **A well-typed `pause_first: true` forwards with the flag set.**
//! 3. **An absent or explicit `false` `pause_first` is rejected** —
//!    live/concurrent-write migration is not supported until #394 is
//!    proven. The old absent→false default silently chose the live path;
//!    the gate must reject it with a clear 400, never coerce it to true.

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

/// Records every `migrate_vm` call as `(vm_id, target_node_id,
/// pause_first)`; every other mutation is unreachable in these tests.
#[derive(Default)]
struct RecordingMutations {
    migrate_calls: Mutex<Vec<(String, String, bool)>>,
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
        unreachable!()
    }
    async fn migrate_vm(
        &self,
        vm_id: String,
        target_node_id: String,
        pause_first: bool,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        self.migrate_calls
            .lock()
            .unwrap()
            .push((vm_id.clone(), target_node_id, pause_first));
        Ok(chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse {
            accepted: true,
            task_id: format!("op-{vm_id}"),
            vm_id,
            summary: "recorded".to_string(),
        })
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

async fn build_state(mutations: Arc<RecordingMutations>) -> AppState {
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
        agent_runtime_dir: std::env::temp_dir(),
        cache: chv_webui_bff::BffCache::new(5),
        clock: Arc::new(SystemClock),
    }
}

/// Seed an operator user + JWT and a VM the operator owns (the mutate
/// route's authz tier — `require_vm_owner`).
async fn seed_operator_and_vm(state: &AppState) -> String {
    sqlx::query(
        "INSERT INTO users (user_id, username, password_hash, role, must_change_password) \
         VALUES ('u-op', 'op', 'x', 'operator', 0)",
    )
    .execute(&state.pool)
    .await
    .expect("seed user");
    sqlx::query("INSERT INTO vms (vm_id, display_name, owner_id) VALUES ('vm-1', 'VM 1', 'u-op')")
        .execute(&state.pool)
        .await
        .expect("seed vm");

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

async fn send_migrate(
    state: AppState,
    token: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let app = chv_webui_bff::bff_router(state.clone()).with_state(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/vms/mutate")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .expect("request must be served");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("read body");
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

#[tokio::test]
async fn non_bool_pause_first_is_rejected_not_coerced() {
    let mutations = Arc::new(RecordingMutations::default());
    let state = build_state(mutations.clone()).await;
    let token = seed_operator_and_vm(&state).await;

    // A quoted "true" — the classic client bug a coercion would swallow.
    let (status, body) = send_migrate(
        state.clone(),
        &token,
        serde_json::json!({
            "vm_id": "vm-1",
            "action": "migrate",
            "target_node_id": "n-2",
            "pause_first": "true",
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a non-bool pause_first must be a 400, got {status}: {body}"
    );
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|e| e.contains("pause_first")),
        "the rejection must name the field: {body}"
    );
    assert!(
        mutations.migrate_calls.lock().unwrap().is_empty(),
        "the mutation service must not run on a rejected payload"
    );
}

#[tokio::test]
async fn bool_pause_first_forwards_with_the_flag_set() {
    let mutations = Arc::new(RecordingMutations::default());
    let state = build_state(mutations.clone()).await;
    let token = seed_operator_and_vm(&state).await;

    let (status, _body) = send_migrate(
        state.clone(),
        &token,
        serde_json::json!({
            "vm_id": "vm-1",
            "action": "migrate",
            "target_node_id": "n-2",
            "pause_first": true,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let calls = mutations.migrate_calls.lock().unwrap();
    assert_eq!(
        calls.as_slice(),
        [("vm-1".to_string(), "n-2".to_string(), true)],
        "the stop-the-world opt-in must forward to the mutation service"
    );
}

#[tokio::test]
async fn absent_pause_first_is_rejected_as_live_migration() {
    let mutations = Arc::new(RecordingMutations::default());
    let state = build_state(mutations.clone()).await;
    let token = seed_operator_and_vm(&state).await;

    let (status, body) = send_migrate(
        state.clone(),
        &token,
        serde_json::json!({
            "vm_id": "vm-1",
            "action": "migrate",
            "target_node_id": "n-2",
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an absent pause_first must fail closed, got {status}: {body}"
    );
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|e| e.contains("#394") && e.contains("pause_first=true")),
        "the rejection must explain the #394 gate and the remedy: {body}"
    );
    assert!(
        mutations.migrate_calls.lock().unwrap().is_empty(),
        "the mutation service must not run on an absent pause_first"
    );
}

#[tokio::test]
async fn false_pause_first_is_rejected_as_live_migration() {
    let mutations = Arc::new(RecordingMutations::default());
    let state = build_state(mutations.clone()).await;
    let token = seed_operator_and_vm(&state).await;

    let (status, body) = send_migrate(
        state.clone(),
        &token,
        serde_json::json!({
            "vm_id": "vm-1",
            "action": "migrate",
            "target_node_id": "n-2",
            "pause_first": false,
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an explicit live request must fail closed, got {status}: {body}"
    );
    assert!(
        body["message"].as_str().is_some_and(|e| e.contains("#394")),
        "the rejection must explain the #394 gate: {body}"
    );
    assert!(
        mutations.migrate_calls.lock().unwrap().is_empty(),
        "the mutation service must not run on a live request"
    );
}
