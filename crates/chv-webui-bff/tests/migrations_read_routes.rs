//! Integration tests for the migration read routes `GET /v1/migrations`
//! and `GET /v1/migrations/{id}` (#372 DP4b — the routes `chvctl
//! migrate status`/`list` read).
//!
//! The migration machinery was always real (the `migrations` table, the
//! CP migration loop, the admin-tier cancel route) but the BFF had no
//! read surface over it, so the CLI's status/list subcommands 404'd from
//! introduction (issue #372 §2.3). The adopted design's PR 4 adds these
//! two plain-SELECT routes at the viewer tier, following the backup read
//! routes' GET shape (`GET /v1/backups/jobs[/:job_id]`). These tests
//! boot the real `bff_router` and pin that contract:
//!
//! - authentication is required (anonymous request -> 401), and the
//!   lowest role (viewer) is sufficient — the routes are reads, and
//!   DP4b keeps them viewer-tier while `migrate cancel` stays
//!   admin-tier on the CP admin router;
//! - an empty table lists as `{items: [], total: 0}`;
//! - a seeded migration row lists and gets with the `migrations`
//!   table's own column names (plus the derived `cancel_requested`
//!   flag), and a cancel-flagged row reports `cancel_requested: true`;
//! - a restart-interrupted row exposes `interrupted_from_phase` and
//!   `interrupted_at` (the recovery vocabulary from #656);
//! - an unknown migration id is a 404.

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
        monitoring: None,
        monitoring_health: chv_webui_bff::MonitoringHealth::new(),
        agent_runtime_dir: std::path::PathBuf::from("/var/lib/chv/agent"),
        cache: chv_webui_bff::BffCache::new(5),
        clock: Arc::new(SystemClock),
    }
}

/// Seed a user with the given role and return a usable JWT bearer
/// token — the viewer role proves the routes are viewer-tier (the
/// lowest role can read them).
async fn seed_jwt_as(state: &AppState, role: &str) -> String {
    sqlx::query(
        "INSERT INTO users (user_id, username, password_hash, role, must_change_password) \
         VALUES (?, ?, 'x', ?, 0)",
    )
    .bind(format!("u-{role}"))
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
        sub: format!("u-{role}"),
        username: role.to_string(),
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

/// Seed one migration row — a VM on `n-1` migrating to `n-2` (the
/// `0038_migration_operations.sql` shape; the node/vm/operation rows
/// the migration's foreign keys reference, with `cancel_requested_at`
/// optionally set to mirror what the admin-tier cancel route writes).
async fn seed_migration(state: &AppState, migration_id: &str, cancel_requested: bool) {
    sqlx::query(
        "INSERT OR IGNORE INTO nodes (node_id, hostname, display_name) \
         VALUES ('n-1', 'h-1', 'Node 1')",
    )
    .execute(&state.pool)
    .await
    .expect("seed node");
    sqlx::query(
        "INSERT OR IGNORE INTO vms (vm_id, node_id, display_name, owner_id) \
         VALUES ('vm-1', 'n-1', 'VM 1', 'u-viewer')",
    )
    .execute(&state.pool)
    .await
    .expect("seed vm");
    sqlx::query(
        "INSERT INTO operations \
         (operation_id, idempotency_key, resource_kind, resource_id, operation_type, status, requested_by, requested_at, created_at, updated_at) \
         VALUES (?, ?, 'vm', 'vm-1', 'MigrateVm', 'Running', 'u-viewer', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
    )
    .bind(format!("op-{migration_id}"))
    .bind(format!("migrations-read-{migration_id}"))
    .execute(&state.pool)
    .await
    .expect("seed operation");

    sqlx::query(
        "INSERT INTO migrations \
         (migration_id, operation_id, vm_id, source_node_id, destination_node_id, phase, \
          bytes_transferred, total_bytes, convergence_round, dirty_blocks_remaining, \
          started_at, updated_at, cancel_requested_at) \
         VALUES (?, ?, 'vm-1', 'n-1', 'n-2', 'PreCopyDisk', 500, 1000, 2, 7, \
                 '2026-01-01T00:00:00Z', '2026-01-01T00:01:00Z', ?)",
    )
    .bind(migration_id)
    .bind(format!("op-{migration_id}"))
    .bind(if cancel_requested {
        Some("2026-01-01T00:02:00Z".to_string())
    } else {
        None
    })
    .execute(&state.pool)
    .await
    .expect("seed migration");
}

async fn get_json(
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

#[tokio::test]
async fn migrations_list_requires_authentication() {
    let state = build_state().await;
    let (status, _) = get_json(state, "/v1/migrations", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn migrations_get_requires_authentication() {
    let state = build_state().await;
    let (status, _) = get_json(state, "/v1/migrations/mig-1", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn migrations_list_empty_table() {
    let state = build_state().await;
    let token = seed_jwt_as(&state, "viewer").await;
    let (status, body) = get_json(state, "/v1/migrations", Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"], serde_json::json!([]));
    assert_eq!(body["total"], 0);
}

#[tokio::test]
async fn migrations_list_returns_seeded_rows_at_viewer_tier() {
    let state = build_state().await;
    let token = seed_jwt_as(&state, "viewer").await;
    seed_migration(&state, "mig-1", false).await;
    seed_migration(&state, "mig-2", true).await;

    let (status, body) = get_json(state, "/v1/migrations", Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 2);
    assert_eq!(body["total"], 2);
    let by_id = |id: &str| {
        items
            .iter()
            .find(|i| i["migration_id"] == id)
            .unwrap_or_else(|| panic!("migration {id} missing from list"))
    };
    // The row shape is the migrations table's own column names — the
    // exact keys `chvctl migrate list` prints.
    let mig1 = by_id("mig-1");
    assert_eq!(mig1["vm_id"], "vm-1");
    assert_eq!(mig1["source_node_id"], "n-1");
    assert_eq!(mig1["destination_node_id"], "n-2");
    assert_eq!(mig1["phase"], "PreCopyDisk");
    assert_eq!(mig1["bytes_transferred"], 500);
    assert_eq!(mig1["total_bytes"], 1000);
    assert_eq!(mig1["operation_id"], "op-mig-1");
    assert_eq!(mig1["cancel_requested"], false);
    // A cancel-flagged row reports the derived flag (what the
    // admin-tier cancel route sets).
    assert_eq!(by_id("mig-2")["cancel_requested"], true);
}

#[tokio::test]
async fn migrations_get_returns_the_single_row_at_viewer_tier() {
    let state = build_state().await;
    let token = seed_jwt_as(&state, "viewer").await;
    seed_migration(&state, "mig-1", false).await;

    let (status, body) = get_json(state, "/v1/migrations/mig-1", Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    // Flat row (the get_backup_job shape), identical key set to the
    // list items.
    assert_eq!(body["migration_id"], "mig-1");
    assert_eq!(body["operation_id"], "op-mig-1");
    assert_eq!(body["vm_id"], "vm-1");
    assert_eq!(body["source_node_id"], "n-1");
    assert_eq!(body["destination_node_id"], "n-2");
    assert_eq!(body["phase"], "PreCopyDisk");
    assert_eq!(body["bytes_transferred"], 500);
    assert_eq!(body["total_bytes"], 1000);
    assert_eq!(body["convergence_round"], 2);
    assert_eq!(body["dirty_blocks_remaining"], 7);
    assert_eq!(body["cancel_requested"], false);
    assert_eq!(body["started_at"], "2026-01-01T00:00:00Z");
    assert!(body.get("completed_at").is_some());
    assert!(body.get("error_message").is_some());
}

#[tokio::test]
async fn migrations_get_exposes_restart_interruption() {
    let state = build_state().await;
    let token = seed_jwt_as(&state, "viewer").await;
    seed_migration(&state, "mig-1", false).await;
    sqlx::query(
        "UPDATE migrations \
         SET phase = 'RolledBack', \
             completed_at = '2026-01-01T00:05:00Z', \
             interrupted_from_phase = 'ConvergingDisk', \
             interrupted_at = '2026-01-01T00:05:00Z', \
             error_message = 'control-plane restarted while migration was in phase ConvergingDisk' \
         WHERE migration_id = 'mig-1'",
    )
    .execute(&state.pool)
    .await
    .expect("annotate interrupted migration");

    let (status, body) = get_json(state, "/v1/migrations/mig-1", Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["phase"], "RolledBack");
    assert_eq!(body["interrupted_from_phase"], "ConvergingDisk");
    assert_eq!(body["interrupted_at"], "2026-01-01T00:05:00Z");
    assert_eq!(body["completed_at"], "2026-01-01T00:05:00Z");
    assert!(
        body["error_message"]
            .as_str()
            .unwrap_or_default()
            .contains("control-plane restarted"),
        "operators must see the interruption reason: {body}"
    );
}

#[tokio::test]
async fn migrations_get_unknown_id_is_a_404() {
    let state = build_state().await;
    let token = seed_jwt_as(&state, "viewer").await;
    let (status, _) = get_json(state, "/v1/migrations/mig-none", Some(&token)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
