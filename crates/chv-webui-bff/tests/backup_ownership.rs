//! Backup/restore ownership regression tests (#655).
//!
//! The backup surface has no owner column of its own: a job/schedule encodes
//! a `vm_id` / `volume_id`, and a restore additionally names a target. Before
//! this fix the mutating handlers only checked `require_operator_or_admin`, so
//! any operator could back up or restore another user's resource. These tests
//! pin the fix:
//!
//! - a non-owner operator is rejected with 403 `FORBIDDEN`;
//! - a denied call writes nothing (no job, schedule, execution, or restore);
//! - the owner and an admin both succeed;
//! - the #654 hard-fence still answers 501 for every backup write route.
//!
//! The router-level fence (#654) intentionally runs before the handlers, so
//! the ownership assertions call the handler functions directly with a real
//! `AppState`. That bypasses only the fence — authz, repositories, and the
//! database are the production ones.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Json};
use chv_common::SystemClock;
use chv_controlplane_store::{
    AlertRepository, AlertRuleRepository, ApplyRunRepository, BackupRepository,
    DesiredStateRepository, DriftReportRepository, EventRepository, ImageRepository,
    NetworkRepository, NodeRepository, NotificationOutboxRepository, ObservedStateRepository,
    OperationRepository, TopologyRepository,
};
use chv_webui_bff::auth::{BearerToken, Claims};
use chv_webui_bff::handlers::backups;
use chv_webui_bff::mutations::MutationService;
use chv_webui_bff::{AppState, BffError};
use serde_json::{json, Value};
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
        monitoring: None,
        monitoring_health: chv_webui_bff::MonitoringHealth::new(),
        agent_runtime_dir: std::path::PathBuf::from("/var/lib/chv/agent"),
        cache: chv_webui_bff::BffCache::new(5),
        clock: Arc::new(SystemClock),
    }
}

fn claims(sub: &str, role: &str) -> Claims {
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    Claims {
        sub: sub.to_string(),
        username: sub.to_string(),
        role: role.to_string(),
        exp,
        must_change_password: false,
            token_version: None,
    }
}

fn token_for(state: &AppState, role: &str) -> String {
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let claims = Claims {
        sub: format!("u-{role}"),
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

async fn seed_node(state: &AppState) {
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('n-1', 'h-1', 'Node 1')",
    )
    .execute(&state.pool)
    .await
    .expect("seed node");
}

async fn seed_vm(state: &AppState, vm_id: &str, owner: &str) {
    sqlx::query("INSERT INTO vms (vm_id, node_id, display_name, owner_id) VALUES (?, 'n-1', ?, ?)")
        .bind(vm_id)
        .bind(vm_id)
        .bind(owner)
        .execute(&state.pool)
        .await
        .expect("seed vm");
}

async fn seed_volume(state: &AppState, volume_id: &str, owner: &str) {
    sqlx::query(
        "INSERT INTO volumes (volume_id, node_id, display_name, owner_id, capacity_bytes, updated_at) \
         VALUES (?, 'n-1', ?, ?, 10737418240, '2026-01-01T00:00:00Z')",
    )
    .bind(volume_id)
    .bind(volume_id)
    .bind(owner)
    .execute(&state.pool)
    .await
    .expect("seed volume");
}

async fn seed_job(state: &AppState, job_id: &str, vm_id: &str, volume_id: Option<&str>) {
    sqlx::query("INSERT INTO backup_jobs (job_id, vm_id, volume_id) VALUES (?, ?, ?)")
        .bind(job_id)
        .bind(vm_id)
        .bind(volume_id)
        .execute(&state.pool)
        .await
        .expect("seed backup job");
}

async fn seed_schedule(state: &AppState, schedule_id: &str, vm_id: &str) {
    sqlx::query(
        "INSERT INTO backup_schedules (schedule_id, vm_id, name, cron_expression) \
         VALUES (?, ?, 'nightly', '0 0 * * *')",
    )
    .bind(schedule_id)
    .bind(vm_id)
    .execute(&state.pool)
    .await
    .expect("seed backup schedule");
}

async fn count(state: &AppState, table: &str) -> i64 {
    // `table` is a test-owned literal, never user input.
    sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
        .fetch_one(&state.pool)
        .await
        .expect("count rows")
}

async fn assert_forbidden(err: BffError) {
    let response = err.into_response();
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "ownership denial must be a 403"
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        body["code"], "FORBIDDEN",
        "ownership denial must keep the stable FORBIDDEN code: {body}"
    );
}

fn expect_ok(result: Result<Json<Value>, BffError>) -> Value {
    match result {
        Ok(Json(value)) => value,
        Err(err) => panic!("expected success, got error: {err:?}"),
    }
}

fn bearer(claims: &Claims) -> BearerToken {
    BearerToken(Claims {
        sub: claims.sub.clone(),
        username: claims.username.clone(),
        role: claims.role.clone(),
        exp: claims.exp,
        must_change_password: claims.must_change_password,
        token_version: claims.token_version,
    })
}

async fn create_job(
    state: &AppState,
    claims: &Claims,
    vm_id: &str,
    volume_id: Option<&str>,
) -> Result<Json<Value>, BffError> {
    backups::create_backup_job(
        bearer(claims),
        State(state.clone()),
        Json(json!({ "vm_id": vm_id, "volume_id": volume_id })),
    )
    .await
}

async fn execute_job(
    state: &AppState,
    claims: &Claims,
    job_id: &str,
) -> Result<Json<Value>, BffError> {
    backups::execute_backup_job(
        bearer(claims),
        State(state.clone()),
        axum::extract::Path(job_id.to_string()),
    )
    .await
}

async fn create_restore(
    state: &AppState,
    claims: &Claims,
    backup_job_id: &str,
    target_vm_id: Option<&str>,
    target_volume_id: Option<&str>,
) -> Result<Json<Value>, BffError> {
    backups::create_backup_restore(
        bearer(claims),
        State(state.clone()),
        Json(json!({
            "backup_job_id": backup_job_id,
            "target_vm_id": target_vm_id,
            "target_volume_id": target_volume_id,
        })),
    )
    .await
}

async fn update_job(
    state: &AppState,
    claims: &Claims,
    job_id: &str,
    payload: Value,
) -> Result<Json<Value>, BffError> {
    backups::update_backup_job(
        bearer(claims),
        State(state.clone()),
        axum::extract::Path(job_id.to_string()),
        Json(payload),
    )
    .await
}

async fn delete_job(
    state: &AppState,
    claims: &Claims,
    job_id: &str,
) -> Result<Json<Value>, BffError> {
    backups::delete_backup_job(
        bearer(claims),
        State(state.clone()),
        axum::extract::Path(job_id.to_string()),
    )
    .await
}

async fn update_schedule(
    state: &AppState,
    claims: &Claims,
    schedule_id: &str,
    payload: Value,
) -> Result<Json<Value>, BffError> {
    backups::update_backup_schedule(
        bearer(claims),
        State(state.clone()),
        axum::extract::Path(schedule_id.to_string()),
        Json(payload),
    )
    .await
}

async fn delete_schedule(
    state: &AppState,
    claims: &Claims,
    schedule_id: &str,
) -> Result<Json<Value>, BffError> {
    backups::delete_backup_schedule(
        bearer(claims),
        State(state.clone()),
        axum::extract::Path(schedule_id.to_string()),
    )
    .await
}

async fn delete_job_api(
    state: &AppState,
    claims: &Claims,
    job_id: &str,
) -> Result<Json<Value>, BffError> {
    backups::delete_backup_job_api(
        bearer(claims),
        State(state.clone()),
        axum::extract::Path(job_id.to_string()),
    )
    .await
}

async fn toggle_job_api(
    state: &AppState,
    claims: &Claims,
    job_id: &str,
) -> Result<Json<Value>, BffError> {
    backups::toggle_backup_job_api(
        bearer(claims),
        State(state.clone()),
        axum::extract::Path(job_id.to_string()),
    )
    .await
}

async fn job_bindings(state: &AppState, job_id: &str) -> (String, Option<String>) {
    sqlx::query_as("SELECT vm_id, volume_id FROM backup_jobs WHERE job_id = ?")
        .bind(job_id)
        .fetch_one(&state.pool)
        .await
        .expect("read job bindings")
}

async fn schedule_bindings(state: &AppState, schedule_id: &str) -> (String, Option<String>) {
    sqlx::query_as("SELECT vm_id, volume_id FROM backup_schedules WHERE schedule_id = ?")
        .bind(schedule_id)
        .fetch_one(&state.pool)
        .await
        .expect("read schedule bindings")
}

async fn schedule_enabled(state: &AppState, schedule_id: &str) -> i64 {
    sqlx::query_scalar("SELECT enabled FROM backup_schedules WHERE schedule_id = ?")
        .bind(schedule_id)
        .fetch_one(&state.pool)
        .await
        .expect("read schedule enabled")
}

// ── Create job ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_job_denies_non_owner_and_writes_nothing() {
    let state = build_state().await;
    seed_node(&state).await;
    seed_vm(&state, "vm-alice", "u-alice").await;

    let err = create_job(&state, &claims("u-bob", "operator"), "vm-alice", None)
        .await
        .expect_err("non-owner must not create a backup job");
    assert_forbidden(err).await;

    assert_eq!(
        count(&state, "backup_jobs").await,
        0,
        "a denied create must not persist a job"
    );
}

#[tokio::test]
async fn create_job_allows_owner_and_admin() {
    let state = build_state().await;
    seed_node(&state).await;
    seed_vm(&state, "vm-alice", "u-alice").await;

    let owner = create_job(&state, &claims("u-alice", "operator"), "vm-alice", None).await;
    expect_ok(owner);
    let admin = create_job(&state, &claims("u-root", "admin"), "vm-alice", None).await;
    expect_ok(admin);
    assert_eq!(count(&state, "backup_jobs").await, 2);
}

#[tokio::test]
async fn create_job_denies_foreign_volume_even_when_the_vm_is_owned() {
    let state = build_state().await;
    seed_node(&state).await;
    seed_vm(&state, "vm-bob", "u-bob").await;
    seed_volume(&state, "vol-alice", "u-alice").await;

    let err = create_job(
        &state,
        &claims("u-bob", "operator"),
        "vm-bob",
        Some("vol-alice"),
    )
    .await
    .expect_err("a foreign volume must be rejected too");
    assert_forbidden(err).await;
    assert_eq!(count(&state, "backup_jobs").await, 0);
}

// ── Execute job ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn execute_job_denies_non_owner_and_creates_no_execution() {
    let state = build_state().await;
    seed_node(&state).await;
    seed_vm(&state, "vm-alice", "u-alice").await;
    seed_job(&state, "job-1", "vm-alice", None).await;

    let err = execute_job(&state, &claims("u-bob", "operator"), "job-1")
        .await
        .expect_err("non-owner must not execute another user's backup job");
    assert_forbidden(err).await;

    assert_eq!(
        count(&state, "backup_jobs").await,
        1,
        "a denied execute must not dispatch a second job row"
    );
}

#[tokio::test]
async fn execute_job_allows_owner_and_admin() {
    let state = build_state().await;
    seed_node(&state).await;
    seed_vm(&state, "vm-alice", "u-alice").await;
    seed_job(&state, "job-1", "vm-alice", None).await;

    expect_ok(execute_job(&state, &claims("u-alice", "operator"), "job-1").await);
    expect_ok(execute_job(&state, &claims("u-root", "admin"), "job-1").await);
    assert_eq!(count(&state, "backup_jobs").await, 3);
}

// ── Create restore ──────────────────────────────────────────────────────────

#[tokio::test]
async fn restore_denies_non_owner_source_and_writes_nothing() {
    let state = build_state().await;
    seed_node(&state).await;
    seed_vm(&state, "vm-alice", "u-alice").await;
    seed_vm(&state, "vm-bob", "u-bob").await;
    seed_job(&state, "job-1", "vm-alice", None).await;

    // Bob targets his own VM but the source artifact belongs to Alice.
    let err = create_restore(
        &state,
        &claims("u-bob", "operator"),
        "job-1",
        Some("vm-bob"),
        None,
    )
    .await
    .expect_err("non-owner must not restore another user's backup");
    assert_forbidden(err).await;
    assert_eq!(count(&state, "backup_restores").await, 0);
}

#[tokio::test]
async fn restore_denies_foreign_target_and_writes_nothing() {
    let state = build_state().await;
    seed_node(&state).await;
    seed_vm(&state, "vm-bob", "u-bob").await;
    seed_vm(&state, "vm-alice", "u-alice").await;
    seed_volume(&state, "vol-alice", "u-alice").await;
    seed_job(&state, "job-bob", "vm-bob", None).await;

    let err = create_restore(
        &state,
        &claims("u-bob", "operator"),
        "job-bob",
        Some("vm-alice"),
        Some("vol-alice"),
    )
    .await
    .expect_err("non-owner must not restore into another user's resource");
    assert_forbidden(err).await;
    assert_eq!(count(&state, "backup_restores").await, 0);
}

#[tokio::test]
async fn restore_allows_owner_and_admin() {
    let state = build_state().await;
    seed_node(&state).await;
    seed_vm(&state, "vm-alice", "u-alice").await;
    seed_job(&state, "job-1", "vm-alice", None).await;

    expect_ok(
        create_restore(
            &state,
            &claims("u-alice", "operator"),
            "job-1",
            Some("vm-alice"),
            None,
        )
        .await,
    );
    expect_ok(
        create_restore(
            &state,
            &claims("u-root", "admin"),
            "job-1",
            Some("vm-alice"),
            None,
        )
        .await,
    );
    assert_eq!(count(&state, "backup_restores").await, 2);
}

// ── Schedules / legacy API-compat surfaces ─────────────────────────────────

#[tokio::test]
async fn create_schedule_denies_non_owner_and_writes_nothing() {
    let state = build_state().await;
    seed_node(&state).await;
    seed_vm(&state, "vm-alice", "u-alice").await;

    let err = backups::create_backup_schedule(
        BearerToken(claims("u-bob", "operator")),
        State(state.clone()),
        Json(json!({
            "vm_id": "vm-alice",
            "cron_expression": "0 0 * * *",
        })),
    )
    .await
    .expect_err("non-owner must not schedule another user's VM");
    assert_forbidden(err).await;
    assert_eq!(count(&state, "backup_schedules").await, 0);
}

#[tokio::test]
async fn legacy_create_job_api_denies_non_owner_and_writes_nothing() {
    let state = build_state().await;
    seed_node(&state).await;
    seed_vm(&state, "vm-alice", "u-alice").await;

    let err = backups::create_backup_job_api(
        BearerToken(claims("u-bob", "operator")),
        State(state.clone()),
        Json(json!({ "vm_id": "vm-alice", "name": "nightly" })),
    )
    .await
    .expect_err("non-owner must not create a legacy backup job");
    assert_forbidden(err).await;
    assert_eq!(count(&state, "backup_schedules").await, 0);
}

#[tokio::test]
async fn run_schedule_api_denies_non_owner_and_writes_no_job() {
    let state = build_state().await;
    seed_node(&state).await;
    seed_vm(&state, "vm-alice", "u-alice").await;
    seed_schedule(&state, "sched-1", "vm-alice").await;

    let err = backups::run_backup_job_api(
        BearerToken(claims("u-bob", "operator")),
        State(state.clone()),
        axum::extract::Path("sched-1".to_string()),
    )
    .await
    .expect_err("non-owner must not run another user's schedule");
    assert_forbidden(err).await;
    assert_eq!(
        count(&state, "backup_jobs").await,
        0,
        "a denied run must not dispatch a job"
    );
}

#[tokio::test]
async fn delete_job_denies_non_owner_and_keeps_the_row() {
    let state = build_state().await;
    seed_node(&state).await;
    seed_vm(&state, "vm-alice", "u-alice").await;
    seed_job(&state, "job-1", "vm-alice", None).await;

    let err = backups::delete_backup_job(
        BearerToken(claims("u-bob", "operator")),
        State(state.clone()),
        axum::extract::Path("job-1".to_string()),
    )
    .await
    .expect_err("non-owner must not delete another user's job");
    assert_forbidden(err).await;
    assert_eq!(count(&state, "backup_jobs").await, 1);
}

// ── Update / delete / toggle ownership (double-rebind) ─────────────────────

#[tokio::test]
async fn update_job_denies_rebinding_to_foreign_volume_and_keeps_bindings() {
    let state = build_state().await;
    seed_node(&state).await;
    seed_vm(&state, "vm-alice", "u-alice").await;
    seed_volume(&state, "vol-carol", "u-carol").await;
    seed_job(&state, "job-1", "vm-alice", None).await;

    let err = update_job(
        &state,
        &claims("u-alice", "operator"),
        "job-1",
        json!({ "volume_id": "vol-carol" }),
    )
    .await
    .expect_err("the NEW volume binding must be owned by the caller");
    assert_forbidden(err).await;
    assert_eq!(
        job_bindings(&state, "job-1").await,
        ("vm-alice".to_string(), None),
        "denied rebind must leave the original binding untouched"
    );
}

#[tokio::test]
async fn update_job_allows_rebinding_to_owned_volume() {
    let state = build_state().await;
    seed_node(&state).await;
    seed_vm(&state, "vm-alice", "u-alice").await;
    seed_volume(&state, "vol-alice", "u-alice").await;
    seed_job(&state, "job-1", "vm-alice", None).await;

    expect_ok(
        update_job(
            &state,
            &claims("u-alice", "operator"),
            "job-1",
            json!({ "volume_id": "vol-alice" }),
        )
        .await,
    );
    assert_eq!(
        job_bindings(&state, "job-1").await,
        ("vm-alice".to_string(), Some("vol-alice".to_string()))
    );
}

#[tokio::test]
async fn update_job_denies_non_owner_rebinding_to_their_own_volume() {
    let state = build_state().await;
    seed_node(&state).await;
    seed_vm(&state, "vm-alice", "u-alice").await;
    seed_volume(&state, "vol-bob", "u-bob").await;
    seed_job(&state, "job-1", "vm-alice", None).await;

    // Bob owns the new volume, so only the OLD binding (Alice's VM) can deny
    // this. Dropping the existing-row check would let the update succeed.
    let err = update_job(
        &state,
        &claims("u-bob", "operator"),
        "job-1",
        json!({ "volume_id": "vol-bob" }),
    )
    .await
    .expect_err("a non-owner must not re-target a foreign job");
    assert_forbidden(err).await;
    assert_eq!(
        job_bindings(&state, "job-1").await,
        ("vm-alice".to_string(), None)
    );
}

#[tokio::test]
async fn update_schedule_denies_rebinding_to_foreign_vm_and_keeps_bindings() {
    let state = build_state().await;
    seed_node(&state).await;
    seed_vm(&state, "vm-alice", "u-alice").await;
    seed_vm(&state, "vm-carol", "u-carol").await;
    seed_schedule(&state, "sched-1", "vm-alice").await;

    let err = update_schedule(
        &state,
        &claims("u-alice", "operator"),
        "sched-1",
        json!({ "vm_id": "vm-carol" }),
    )
    .await
    .expect_err("the NEW VM binding must be owned by the caller");
    assert_forbidden(err).await;
    assert_eq!(
        schedule_bindings(&state, "sched-1").await,
        ("vm-alice".to_string(), None)
    );
}

#[tokio::test]
async fn update_schedule_denies_rebinding_to_foreign_volume_and_keeps_bindings() {
    let state = build_state().await;
    seed_node(&state).await;
    seed_vm(&state, "vm-alice", "u-alice").await;
    seed_volume(&state, "vol-carol", "u-carol").await;
    seed_schedule(&state, "sched-1", "vm-alice").await;

    let err = update_schedule(
        &state,
        &claims("u-alice", "operator"),
        "sched-1",
        json!({ "vm_id": "vm-alice", "volume_id": "vol-carol" }),
    )
    .await
    .expect_err("the NEW volume binding must be owned by the caller");
    assert_forbidden(err).await;
    assert_eq!(
        schedule_bindings(&state, "sched-1").await,
        ("vm-alice".to_string(), None)
    );
}

#[tokio::test]
async fn update_schedule_denies_non_owner_rebinding_to_their_own_vm() {
    let state = build_state().await;
    seed_node(&state).await;
    seed_vm(&state, "vm-alice", "u-alice").await;
    seed_vm(&state, "vm-bob", "u-bob").await;
    seed_schedule(&state, "sched-1", "vm-alice").await;

    // Bob owns the new VM, so only the OLD binding (Alice's VM) can deny it.
    let err = update_schedule(
        &state,
        &claims("u-bob", "operator"),
        "sched-1",
        json!({ "vm_id": "vm-bob" }),
    )
    .await
    .expect_err("a non-owner must not re-target a foreign schedule");
    assert_forbidden(err).await;
    assert_eq!(
        schedule_bindings(&state, "sched-1").await,
        ("vm-alice".to_string(), None)
    );
}

#[tokio::test]
async fn update_schedule_allows_rebinding_to_owned_resources() {
    let state = build_state().await;
    seed_node(&state).await;
    seed_vm(&state, "vm-alice", "u-alice").await;
    seed_vm(&state, "vm-alice2", "u-alice").await;
    seed_volume(&state, "vol-alice", "u-alice").await;
    seed_schedule(&state, "sched-1", "vm-alice").await;

    expect_ok(
        update_schedule(
            &state,
            &claims("u-alice", "operator"),
            "sched-1",
            json!({ "vm_id": "vm-alice2", "volume_id": "vol-alice" }),
        )
        .await,
    );
    assert_eq!(
        schedule_bindings(&state, "sched-1").await,
        ("vm-alice2".to_string(), Some("vol-alice".to_string()))
    );
}

#[tokio::test]
async fn delete_schedule_denies_non_owner_and_keeps_the_row() {
    let state = build_state().await;
    seed_node(&state).await;
    seed_vm(&state, "vm-alice", "u-alice").await;
    seed_schedule(&state, "sched-1", "vm-alice").await;

    let err = delete_schedule(&state, &claims("u-bob", "operator"), "sched-1")
        .await
        .expect_err("non-owner must not delete another user's schedule");
    assert_forbidden(err).await;
    assert_eq!(count(&state, "backup_schedules").await, 1);
    assert_eq!(
        schedule_bindings(&state, "sched-1").await,
        ("vm-alice".to_string(), None)
    );
}

#[tokio::test]
async fn toggle_schedule_api_denies_non_owner_and_keeps_state() {
    let state = build_state().await;
    seed_node(&state).await;
    seed_vm(&state, "vm-alice", "u-alice").await;
    seed_schedule(&state, "sched-1", "vm-alice").await;
    assert_eq!(schedule_enabled(&state, "sched-1").await, 1);

    let err = toggle_job_api(&state, &claims("u-bob", "operator"), "sched-1")
        .await
        .expect_err("non-owner must not toggle another user's schedule");
    assert_forbidden(err).await;
    assert_eq!(
        schedule_enabled(&state, "sched-1").await,
        1,
        "a denied toggle must not flip enabled"
    );
}

#[tokio::test]
async fn legacy_delete_job_api_denies_non_owner_and_keeps_the_row() {
    let state = build_state().await;
    seed_node(&state).await;
    seed_vm(&state, "vm-alice", "u-alice").await;
    seed_schedule(&state, "sched-1", "vm-alice").await;

    let err = delete_job_api(&state, &claims("u-bob", "operator"), "sched-1")
        .await
        .expect_err("non-owner must not delete another user's legacy job");
    assert_forbidden(err).await;
    assert_eq!(count(&state, "backup_schedules").await, 1);
}

// ── Orphaned rows: admin-only cleanup ──────────────────────────────────────

#[tokio::test]
async fn orphaned_job_is_admin_only_for_cleanup() {
    let state = build_state().await;
    seed_node(&state).await;
    seed_vm(&state, "vm-alice", "u-alice").await;
    seed_job(&state, "job-1", "vm-alice", None).await;
    sqlx::query("DELETE FROM vms WHERE vm_id = 'vm-alice'")
        .execute(&state.pool)
        .await
        .expect("delete vm to orphan the job");

    let err = delete_job(&state, &claims("u-alice", "operator"), "job-1")
        .await
        .expect_err("the original owner cannot act on an orphaned job");
    assert_forbidden(err).await;
    assert_eq!(count(&state, "backup_jobs").await, 1);

    expect_ok(delete_job(&state, &claims("u-root", "admin"), "job-1").await);
    assert_eq!(
        count(&state, "backup_jobs").await,
        0,
        "admin cleanup is the intended path for orphaned rows"
    );
}

#[tokio::test]
async fn orphaned_schedule_is_admin_only_for_cleanup() {
    let state = build_state().await;
    seed_node(&state).await;
    seed_vm(&state, "vm-alice", "u-alice").await;
    seed_schedule(&state, "sched-1", "vm-alice").await;
    sqlx::query("DELETE FROM vms WHERE vm_id = 'vm-alice'")
        .execute(&state.pool)
        .await
        .expect("delete vm to orphan the schedule");

    let err = delete_schedule(&state, &claims("u-alice", "operator"), "sched-1")
        .await
        .expect_err("the original owner cannot act on an orphaned schedule");
    assert_forbidden(err).await;
    assert_eq!(count(&state, "backup_schedules").await, 1);

    expect_ok(delete_schedule(&state, &claims("u-root", "admin"), "sched-1").await);
    assert_eq!(
        count(&state, "backup_schedules").await,
        0,
        "admin cleanup is the intended path for orphaned rows"
    );
}

// ── Hard-fence unchanged ────────────────────────────────────────────────────

#[tokio::test]
async fn backup_write_routes_still_answer_501() {
    let state = build_state().await;
    let operator = token_for(&state, "operator");

    for (method, path) in [
        ("POST", "/v1/backups/jobs"),
        ("POST", "/v1/backups/jobs/job-1/execute"),
        ("POST", "/v1/backups/restores"),
    ] {
        let app = chv_webui_bff::bff_router(state.clone()).with_state(state.clone());
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {operator}"))
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_IMPLEMENTED,
            "{method} {path} must stay hard-fenced"
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["code"], "NOT_IMPLEMENTED");
    }
}
