//! Integration tests for the volume snapshot/clone BFF contract
//! (kubedoio/chv#372: chvctl↔BFF field-name drift).
//!
//! The ONLY operator entry point to these endpoints is `chvctl volume
//! snapshot|clone` — and until #372's fix it sent `name` where the BFF
//! requires `snapshot_name`, and `volume_id`+`name` where the BFF
//! requires `source_volume_id`+`target_volume_id`, so the entire volume
//! snapshot/clone surface was dead (every call 400'd; the UI never calls
//! these routes). The drift survived because no test pinned the request
//! shapes. These tests do:
//!
//! - the proto-shaped payloads are accepted and forwarded to the
//!   mutation service with the right arguments;
//! - the drifted `name`-shaped payloads are REJECTED (no silent alias);
//! - required fields are enforced.

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

/// Records every volume mutation call as "method:arg:arg" for assertions.
#[derive(Default)]
struct RecordingMutations {
    calls: Mutex<Vec<String>>,
}

impl RecordingMutations {
    fn record(&self, entry: String) {
        self.calls.lock().unwrap().push(entry);
    }

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
        _volume_id: String,
        _action: String,
        _force: bool,
        _resize_bytes: Option<u64>,
        _vm_id: Option<String>,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!("no raw volume mutations in these tests")
    }
    async fn snapshot_volume(
        &self,
        volume_id: String,
        snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        self.record(format!("snapshot_volume:{volume_id}:{snapshot_name}"));
        Ok(volume_response(&volume_id))
    }
    async fn restore_volume_snapshot(
        &self,
        volume_id: String,
        snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        self.record(format!(
            "restore_volume_snapshot:{volume_id}:{snapshot_name}"
        ));
        Ok(volume_response(&volume_id))
    }
    async fn delete_volume_snapshot(
        &self,
        volume_id: String,
        snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        self.record(format!(
            "delete_volume_snapshot:{volume_id}:{snapshot_name}"
        ));
        Ok(volume_response(&volume_id))
    }
    async fn clone_volume(
        &self,
        source_volume_id: String,
        target_volume_id: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        self.record(format!(
            "clone_volume:{source_volume_id}:{target_volume_id}"
        ));
        Ok(volume_response(&target_volume_id))
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
        agent_runtime_dir: std::path::PathBuf::from("/var/lib/chv/agent"),
        cache: chv_webui_bff::BffCache::new(5),
        clock: Arc::new(SystemClock),
    }
}

async fn seed_jwt_as(state: &AppState, role: &str) -> String {
    let user_id = format!("u-{role}");
    sqlx::query(
        "INSERT INTO users (user_id, username, password_hash, role, must_change_password) \
         VALUES (?, ?, 'x', ?, 0)",
    )
    .bind(&user_id)
    .bind(role)
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
        sub: user_id,
        username: role.to_string(),
        role: role.to_string(),
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

/// Seed one volume owned by the operator; return (volume_id, token).
async fn seed_volume(state: &AppState) -> (String, String) {
    let token = seed_jwt_as(state, "operator").await;
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('n-1', 'h-1', 'Node 1')",
    )
    .execute(&state.pool)
    .await
    .expect("seed node");
    sqlx::query(
        "INSERT INTO volumes (volume_id, node_id, display_name, owner_id, capacity_bytes, updated_at) \
         VALUES ('vol-t1', 'n-1', 't1-disk', 'u-operator', 10737418240, '2026-01-01T00:00:00Z')",
    )
    .execute(&state.pool)
    .await
    .expect("seed volume");
    ("vol-t1".to_string(), token)
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

#[tokio::test]
async fn volume_snapshot_accepts_proto_shaped_payload() {
    let mutations = Arc::new(RecordingMutations::default());
    let state = build_state(mutations.clone()).await;
    let (vol_id, token) = seed_volume(&state).await;

    let (status, body) = post_with_token(
        state,
        "/v1/volumes/snapshot",
        &token,
        r#"{"volume_id":"vol-t1","snapshot_name":"snap-1"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "snapshot body: {body}");
    assert_eq!(body["volume_id"].as_str(), Some(vol_id.as_str()));
    assert_eq!(
        mutations.calls(),
        vec!["snapshot_volume:vol-t1:snap-1".to_string()],
        "the mutation service must receive exactly the request's arguments"
    );
}

#[tokio::test]
async fn volume_snapshot_rejects_the_drifted_name_shape() {
    // The pre-#372 chvctl payload: `name` instead of `snapshot_name`.
    // Rejected, not silently aliased — a client sending the drifted
    // shape must learn it is wrong.
    let mutations = Arc::new(RecordingMutations::default());
    let state = build_state(mutations.clone()).await;
    let (_vol_id, token) = seed_volume(&state).await;

    let (status, body) = post_with_token(
        state,
        "/v1/volumes/snapshot",
        &token,
        r#"{"volume_id":"vol-t1","name":"snap-1"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "snapshot body: {body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("snapshot_name"),
        "rejection must name the missing field: {body}"
    );
    assert!(mutations.calls().is_empty());
}

#[tokio::test]
async fn volume_clone_accepts_source_and_target_ids() {
    let mutations = Arc::new(RecordingMutations::default());
    let state = build_state(mutations.clone()).await;
    let (_vol_id, token) = seed_volume(&state).await;

    let (status, body) = post_with_token(
        state,
        "/v1/volumes/clone",
        &token,
        r#"{"source_volume_id":"vol-t1","target_volume_id":"vol-t2"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "clone body: {body}");
    assert_eq!(
        body["volume_id"].as_str(),
        Some("vol-t2"),
        "the response must speak about the TARGET volume: {body}"
    );
    assert_eq!(
        mutations.calls(),
        vec!["clone_volume:vol-t1:vol-t2".to_string()],
        "the mutation service must receive source and target ids"
    );
}

#[tokio::test]
async fn volume_clone_rejects_the_drifted_name_shape() {
    // The pre-#372 chvctl payload: `volume_id` + `name`.
    let mutations = Arc::new(RecordingMutations::default());
    let state = build_state(mutations.clone()).await;
    let (_vol_id, token) = seed_volume(&state).await;

    let (status, body) = post_with_token(
        state,
        "/v1/volumes/clone",
        &token,
        r#"{"volume_id":"vol-t1","name":"vol-t2"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "clone body: {body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("source_volume_id"),
        "rejection must name the missing field: {body}"
    );
    assert!(mutations.calls().is_empty());
}
