//! Integration tests for object-level ownership on architecture endpoints
//! (Security H6 — cross-tenant IDOR).
//!
//! The Phase-7 review found that `list_architectures` scoped non-admins to
//! their own rows (`visible_to_user`), but every object-level endpoint
//! (get / update / archive / plan / apply / drift / runs / versions) read
//! and wrote by raw id with no ownership check. These tests pin the fixed
//! model:
//!
//! - Viewer/Operator see and mutate only their own architectures
//!   (plus system-owned starters, the documented H5 carve-out).
//! - Admin sees and mutates all.
//! - A non-admin touching a foreign row answers 403, never the row's data.
//! - The production guard fires from the *persisted* environment tag, so an
//!   `environment: null` (or absent) request field cannot bypass it.
//!
//! Handlers are exercised directly (the same pattern as
//! `tests/architectures.rs`) so the assertions can pin exact `BffError`
//! variants, not just status codes.

use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::State;
use axum::Json;
use chv_controlplane_store::{
    AlertRepository, AlertRuleRepository, ApplyRunRepository, BackupRepository,
    DesiredStateRepository, DriftReportRepository, EventRepository, ImageRepository,
    NetworkRepository, NodeRepository, NotificationOutboxRepository, ObservedStateRepository,
    OperationRepository, TopologyCreateInput, TopologyRepository,
};
use chv_controlplane_types::architecture::{ArchitectureId, ArchitectureStatus};
use chv_webui_bff::auth::{BearerToken, Claims};
use chv_webui_bff::handlers::architectures::{
    apply_architecture, archive_architecture, check_fleet_architecture, get_architecture,
    get_architecture_drift, list_architecture_runs, plan_architecture, update_architecture,
    ApplyArchitectureRequest, ArchiveArchitectureRequest, CheckFleetRequest, DriftRequest,
    GetArchitectureRequest, ListApplyRunsRequest, PlanArchitectureRequest,
    UpdateArchitectureRequest,
};
use chv_webui_bff::mutations::MutationService;
use chv_webui_bff::{AppState, BffError};
use sqlx::sqlite::SqlitePoolOptions;

// ---------------------------------------------------------------------------
// Test scaffolding
// ---------------------------------------------------------------------------

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
        clock: Arc::new(chv_common::SystemClock),
    }
}

fn claims(sub: &str, role: &str) -> Claims {
    Claims {
        sub: sub.to_string(),
        username: format!("user-{sub}"),
        role: role.to_string(),
        exp: u64::MAX / 2,
        must_change_password: false,
        token_version: None,
    }
}

/// Insert a topology owned by `owner` (or system-owned when `None`) with an
/// optional environment tag, and return its id.
async fn seed_topology(
    state: &AppState,
    name: &str,
    owner: Option<&str>,
    environment: Option<&str>,
) -> String {
    let id = ArchitectureId::new(format!("arch-{name}")).expect("valid id");
    state
        .topology_repo
        .create(TopologyCreateInput {
            id: id.clone(),
            name: name.to_string(),
            display_name: None,
            description: None,
            environment: environment.map(str::to_string),
            status: ArchitectureStatus::Draft,
            owner_user_id: owner.map(str::to_string),
            design_graph_json: Some(r#"{"nodes":[],"edges":[]}"#.to_string()),
            latest_yaml: None,
        })
        .await
        .unwrap_or_else(|e| panic!("seed {name}: {e}"));
    id.into_inner()
}

fn err_status(e: &BffError) -> u16 {
    match e {
        BffError::BadRequest(_) => 400,
        BffError::Unauthorized(_) => 401,
        BffError::Forbidden(_) => 403,
        BffError::NotFound(_) => 404,
        BffError::Conflict(_) => 409,
        BffError::TooManyRequests(_) => 429,
        BffError::MonitoringUnavailable(_) => 503,
        BffError::MonitoringQuery { .. } => 400,
        BffError::Internal(_) => 500,
        BffError::NotImplemented(_) => 501,
        BffError::QuotaExceeded { .. } => 422,
        BffError::GraphEmpty => 422,
        BffError::PlanExpired { .. } => 409,
        BffError::PlanNotDiscardable { .. } => 409,
        BffError::MissingConfirmation { .. } => 400,
        BffError::WarningsNotAcknowledged { .. } => 400,
        BffError::PlanNotApplicable { .. } => 409,
        BffError::ProductionRequiresAdmin { .. } => 403,
        BffError::PlanModeMismatch { .. } => 400,
        BffError::InvalidResourceName { .. } => 400,
        BffError::DriftCheckFailed { .. } => 502,
        BffError::NetboxNotConfigured { .. } => 404,
        BffError::NetboxNotConfiguredPrecondition { .. } => 400,
        BffError::NetboxNotApplied { .. } => 400,
        BffError::NetboxHttpsRequired => 400,
        BffError::NetboxTokenMissing { .. } => 400,
        BffError::NetboxRunActive { .. } => 409,
        BffError::ProjectionRunNotRetryable { .. } => 409,
        BffError::NetboxStaleVersion { .. } => 409,
        BffError::NetboxUnreachable { .. } => 502,
        BffError::NetboxAuthFailed { .. } => 502,
    }
}

// ---------------------------------------------------------------------------
// (a) viewer gets 403 reading another user's architecture
// ---------------------------------------------------------------------------

#[tokio::test]
async fn viewer_get_foreign_architecture_returns_403() {
    let state = build_state().await;
    let bob_arch = seed_topology(&state, "bob-a", Some("u-bob"), None).await;

    let err = get_architecture(
        BearerToken(claims("u-alice", "viewer")),
        State(state),
        Json(GetArchitectureRequest { id: bob_arch }),
    )
    .await
    .expect_err("viewer must not read another user's architecture");
    assert_eq!(err_status(&err), 403, "foreign read => 403");
    assert!(
        matches!(err, BffError::Forbidden(_)),
        "expected Forbidden, got {err:?}"
    );
}

#[tokio::test]
async fn viewer_get_own_architecture_succeeds() {
    let state = build_state().await;
    let alice_arch = seed_topology(&state, "alice-a", Some("u-alice"), None).await;

    let resp = get_architecture(
        BearerToken(claims("u-alice", "viewer")),
        State(state),
        Json(GetArchitectureRequest { id: alice_arch }),
    )
    .await
    .expect("viewer must read their own architecture");
    assert_eq!(resp.0.architecture.id, "arch-alice-a");
}

// ---------------------------------------------------------------------------
// (b) operator cannot archive another's / (c) admin can
// ---------------------------------------------------------------------------

#[tokio::test]
async fn operator_cannot_archive_foreign_architecture() {
    let state = build_state().await;
    let bob_arch = seed_topology(&state, "bob-b", Some("u-bob"), None).await;

    let err = archive_architecture(
        BearerToken(claims("u-alice", "operator")),
        State(state.clone()),
        Json(ArchiveArchitectureRequest {
            id: bob_arch.clone(),
            expected_version: 1,
        }),
    )
    .await
    .expect_err("operator must not archive another user's architecture");
    assert_eq!(err_status(&err), 403, "foreign archive => 403");
    assert!(
        matches!(err, BffError::Forbidden(_)),
        "expected Forbidden, got {err:?}"
    );

    // The row must still be active for its owner.
    let resp = get_architecture(
        BearerToken(claims("u-bob", "operator")),
        State(state.clone()),
        Json(GetArchitectureRequest { id: bob_arch }),
    )
    .await
    .expect("owner can still read their row");
    assert_eq!(resp.0.architecture.id, "arch-bob-b");
}

#[tokio::test]
async fn admin_can_archive_foreign_architecture() {
    let state = build_state().await;
    let bob_arch = seed_topology(&state, "bob-c", Some("u-bob"), None).await;

    let resp = archive_architecture(
        BearerToken(claims("u-admin", "admin")),
        State(state),
        Json(ArchiveArchitectureRequest {
            id: bob_arch,
            expected_version: 1,
        }),
    )
    .await
    .expect("admin must be able to archive any architecture");
    assert_eq!(resp.0.architecture.id, "arch-bob-c");
    assert!(resp.0.architecture.archived_at.is_some());
}

// ---------------------------------------------------------------------------
// (d) production guard cannot be bypassed with environment: null
// ---------------------------------------------------------------------------

#[tokio::test]
async fn production_guard_fires_from_persisted_tag_even_when_request_environment_is_null() {
    let state = build_state().await;
    // Operator-owned production topology: the ownership gate passes, so the
    // persisted-tag guard (b) is what must fire. (A *foreign* production
    // row answers the plain ownership 403 instead — see the next test.)
    let prod_arch = seed_topology(&state, "admin-prod", Some("u-alice"), Some("production")).await;

    // An operator tries to update it while sending environment: null (the
    // serde shape of `"environment": null` / absent field). The persisted
    // tag must fire the guard — the request field must not bypass it.
    let err = update_architecture(
        BearerToken(claims("u-alice", "operator")),
        State(state.clone()),
        Json(UpdateArchitectureRequest {
            id: prod_arch.clone(),
            expected_version: 1,
            display_name: Some("sneaky".to_string()),
            description: None,
            environment: None,
            design_graph_json: None,
            latest_yaml: None,
            latest_version_id: None,
        }),
    )
    .await
    .expect_err("production guard must fire for operator-owned production row");
    assert_eq!(
        err_status(&err),
        403,
        "production-tagged row touched by operator => 403"
    );
    assert!(
        matches!(err, BffError::ProductionRequiresAdmin { .. }),
        "expected ProductionRequiresAdmin, got {err:?}"
    );

    // Sanity: the row is unchanged (still production, version still 1).
    let resp = get_architecture(
        BearerToken(claims("u-admin", "admin")),
        State(state),
        Json(GetArchitectureRequest { id: prod_arch }),
    )
    .await
    .expect("admin read");
    assert_eq!(
        resp.0.architecture.environment.as_deref(),
        Some("production")
    );
    assert_eq!(resp.0.architecture.version_number, 1);
}

#[tokio::test]
async fn foreign_production_row_returns_plain_forbidden_before_production_guard() {
    // Information disclosure (Security F7 review): a non-admin touching a
    // FOREIGN production row must get the plain ownership 403 — never
    // ProductionRequiresAdmin, which would confirm the persisted tag of a
    // row the caller may not see.
    let state = build_state().await;
    let foreign_prod =
        seed_topology(&state, "foreign-prod", Some("u-admin"), Some("production")).await;

    let err = update_architecture(
        BearerToken(claims("u-alice", "operator")),
        State(state),
        Json(UpdateArchitectureRequest {
            id: foreign_prod,
            expected_version: 1,
            display_name: Some("sneaky".to_string()),
            description: None,
            environment: None,
            design_graph_json: None,
            latest_yaml: None,
            latest_version_id: None,
        }),
    )
    .await
    .expect_err("operator must not touch a foreign production row");
    assert_eq!(err_status(&err), 403);
    assert!(
        matches!(err, BffError::Forbidden(_)),
        "expected plain Forbidden (no production-tag leak), got {err:?}"
    );
}

#[tokio::test]
async fn operator_cannot_untag_own_production_row_via_update() {
    // The un-tag-then-apply chain: an admin tags an operator-owned row as
    // production; the operator must not be able to relabel it (e.g. to
    // "staging") and then apply it past the production guard.
    let state = build_state().await;
    let arch = seed_topology(&state, "op-prod", Some("u-alice"), Some("production")).await;

    let err = update_architecture(
        BearerToken(claims("u-alice", "operator")),
        State(state),
        Json(UpdateArchitectureRequest {
            id: arch,
            expected_version: 1,
            display_name: None,
            description: None,
            environment: Some("staging".to_string()),
            design_graph_json: None,
            latest_yaml: None,
            latest_version_id: None,
        }),
    )
    .await
    .expect_err("operator must not un-tag a production row");
    assert_eq!(err_status(&err), 403);
    assert!(
        matches!(err, BffError::ProductionRequiresAdmin { .. }),
        "expected ProductionRequiresAdmin, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// (e) object-level endpoints answer 403 on foreign rows (review follow-up)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plan_architecture_on_foreign_row_returns_403() {
    let state = build_state().await;
    let bob_arch = seed_topology(&state, "bob-plan", Some("u-bob"), None).await;

    let err = plan_architecture(
        BearerToken(claims("u-alice", "operator")),
        State(state),
        Json(PlanArchitectureRequest {
            id: bob_arch,
            allow_warnings: None,
            refresh_inventory: None,
        }),
    )
    .await
    .expect_err("operator must not plan against a foreign row");
    assert_eq!(err_status(&err), 403, "foreign plan => 403");
    assert!(
        matches!(err, BffError::Forbidden(_)),
        "expected Forbidden, got {err:?}"
    );
}

#[tokio::test]
async fn apply_architecture_on_foreign_row_returns_403() {
    let state = build_state().await;
    let bob_arch = seed_topology(&state, "bob-apply", Some("u-bob"), None).await;

    let err = apply_architecture(
        BearerToken(claims("u-alice", "operator")),
        State(state),
        Json(ApplyArchitectureRequest {
            id: bob_arch,
            plan_id: "plan-fake".to_string(),
            ..Default::default()
        }),
    )
    .await
    .expect_err("operator must not apply a foreign row");
    assert_eq!(err_status(&err), 403, "foreign apply => 403");
    assert!(
        matches!(err, BffError::Forbidden(_)),
        "expected Forbidden, got {err:?}"
    );
}

#[tokio::test]
async fn get_architecture_drift_on_foreign_row_returns_403() {
    let state = build_state().await;
    let bob_arch = seed_topology(&state, "bob-drift", Some("u-bob"), None).await;

    let err = get_architecture_drift(
        BearerToken(claims("u-alice", "operator")),
        State(state),
        Json(DriftRequest {
            id: bob_arch,
            force_refresh: false,
        }),
    )
    .await
    .expect_err("operator must not drift-check a foreign row");
    assert_eq!(err_status(&err), 403, "foreign drift => 403");
    assert!(
        matches!(err, BffError::Forbidden(_)),
        "expected Forbidden, got {err:?}"
    );
}

#[tokio::test]
async fn list_architecture_runs_on_foreign_row_returns_403() {
    let state = build_state().await;
    let bob_arch = seed_topology(&state, "bob-runs", Some("u-bob"), None).await;

    let err = list_architecture_runs(
        BearerToken(claims("u-alice", "operator")),
        State(state),
        Json(ListApplyRunsRequest { id: bob_arch }),
    )
    .await
    .expect_err("operator must not list runs of a foreign row");
    assert_eq!(err_status(&err), 403, "foreign runs => 403");
    assert!(
        matches!(err, BffError::Forbidden(_)),
        "expected Forbidden, got {err:?}"
    );
}

#[tokio::test]
async fn check_fleet_architecture_on_foreign_row_returns_403() {
    let state = build_state().await;
    let bob_arch = seed_topology(&state, "bob-fleet", Some("u-bob"), None).await;

    let err = check_fleet_architecture(
        BearerToken(claims("u-alice", "operator")),
        State(state),
        Json(CheckFleetRequest { id: bob_arch }),
    )
    .await
    .expect_err("operator must not check-fleet a foreign row");
    assert_eq!(err_status(&err), 403, "foreign check-fleet => 403");
    assert!(
        matches!(err, BffError::Forbidden(_)),
        "expected Forbidden, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// (f) NULL-owned starter rows are readable but not writable by non-admins
// ---------------------------------------------------------------------------

#[tokio::test]
async fn null_owned_starter_row_readable_but_not_archivable_by_operator() {
    let state = build_state().await;
    let starter = seed_topology(&state, "starter", None, None).await;

    // Readable: the read predicate keeps the IS NULL carve-out, so the
    // shared starter template is visible to every authenticated caller.
    let resp = get_architecture(
        BearerToken(claims("u-alice", "operator")),
        State(state.clone()),
        Json(GetArchitectureRequest {
            id: starter.clone(),
        }),
    )
    .await
    .expect("operator must read the NULL-owned starter");
    assert_eq!(resp.0.architecture.id, "arch-starter");

    // Not archivable: NULL-owned rows are read-only templates for
    // non-admins (Security H6 review) — require_owner_or_admin answers 403.
    let err = archive_architecture(
        BearerToken(claims("u-alice", "operator")),
        State(state),
        Json(ArchiveArchitectureRequest {
            id: starter,
            expected_version: 1,
        }),
    )
    .await
    .expect_err("operator must not archive a NULL-owned starter");
    assert_eq!(err_status(&err), 403);
    assert!(
        matches!(err, BffError::Forbidden(_)),
        "expected Forbidden, got {err:?}"
    );
}
