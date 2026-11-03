//! Integration tests for the Phase 4 Architecture Designer plan handlers
//! (`POST /v1/architectures/plan`, `destroy-plan`, `discard-plan`).
//!
//! Mirrors the harness in `tests/architectures.rs` (in-memory SQLite,
//! direct handler invocation, no HTTP layer) but injects a `ManualClock`
//! into [`AppState`] so plan TTL / `expires_at` is deterministic.

use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::State;
use axum::Json;
use chrono::{Duration, TimeZone, Utc};
use chv_common::ManualClock;
use chv_controlplane_store::{
    AlertRepository, AlertRuleRepository, ApplyRunRepository, BackupRepository,
    DesiredStateRepository, DriftReportRepository, EventRepository, ImageRepository,
    NetworkRepository, NodeRepository, NotificationOutboxRepository, ObservedStateRepository,
    OperationRepository, PlanRepository, PlanStatusUpdateInput, TopologyRepository,
};
use chv_controlplane_types::architecture::{ArchitecturePlanId, PlanAction, PlanMode, PlanStatus};
use chv_webui_bff::auth::{BearerToken, Claims};
use chv_webui_bff::handlers::architectures::{
    create_architecture, destroy_plan_architecture, discard_plan_architecture, plan_architecture,
    CreateArchitectureRequest, DiscardPlanRequest, PlanArchitectureRequest,
};
use chv_webui_bff::mutations::MutationService;
use chv_webui_bff::{AppState, BffError};
use sqlx::sqlite::SqlitePoolOptions;

// ---------------------------------------------------------------------------
// Test scaffolding
// ---------------------------------------------------------------------------

/// MutationService stub — none of the architecture handlers call into the
/// mutation service in this test surface.
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
        unreachable!("mutate_vm not used")
    }
    async fn migrate_vm(
        &self,
        _vm_id: String,
        _target_node_id: String,
        _pause_first: bool,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        unreachable!("migrate_vm not used")
    }
    async fn snapshot_vm(
        &self,
        _vm_id: String,
        _destination: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        unreachable!("snapshot_vm not used")
    }
    async fn restore_snapshot(
        &self,
        _vm_id: String,
        _source: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        unreachable!("restore_snapshot not used")
    }
    async fn mutate_node(
        &self,
        _node_id: String,
        _action: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateNodeResponse, BffError> {
        unreachable!("mutate_node not used")
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
        unreachable!("mutate_volume not used")
    }
    async fn snapshot_volume(
        &self,
        _volume_id: String,
        _snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!("snapshot_volume not used")
    }
    async fn restore_volume_snapshot(
        &self,
        _volume_id: String,
        _snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!("restore_volume_snapshot not used")
    }
    async fn delete_volume_snapshot(
        &self,
        _volume_id: String,
        _snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!("delete_volume_snapshot not used")
    }
    async fn mutate_network(
        &self,
        _network_id: String,
        _action: String,
        _force: bool,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateNetworkResponse, BffError> {
        unreachable!("mutate_network not used")
    }
    async fn clone_volume(
        &self,
        _volume_id: String,
        _new_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!("clone_volume not used")
    }
}

fn t0() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 13, 10, 0, 0).unwrap()
}

async fn build_state_with_clock(clock: ManualClock) -> AppState {
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
        clock: Arc::new(clock),
    }
}

fn claims_for(role: &str) -> Claims {
    Claims {
        sub: "u-tester".to_string(),
        username: "tester".to_string(),
        role: role.to_string(),
        exp: u64::MAX / 2,
        must_change_password: false,
        aud: None,
        token_version: None,
    }
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

/// Seed one schedulable host with plenty of headroom so fleet checks pass
/// for the happy-path YAML used below.
async fn seed_capable_host(state: &AppState) {
    sqlx::query(
        r#"INSERT INTO nodes (node_id, hostname, display_name)
           VALUES ('n1', 'host-1', 'host-1')"#,
    )
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO node_inventory (node_id, architecture, cpu_count, memory_bytes)
           VALUES ('n1', 'x86_64', 16, ?1)"#,
    )
    .bind(64i64 * 1024 * 1024 * 1024)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO node_desired_state
           (node_id, desired_generation, desired_state, scheduling_paused)
           VALUES ('n1', 1, 'Running', 0)"#,
    )
    .execute(&state.pool)
    .await
    .unwrap();
}

const HAPPY_YAML: &str = r#"apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: happy
templates:
  - name: small
    image: ubuntu-24.04
    cpu: 1
    memory_mb: 1024
instances:
  - name: app-a
    template: small
    placement:
      server: host-1
  - name: app-b
    template: small
    placement:
      server: host-1
"#;

const NO_HOST_YAML: &str = r#"apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: missing-host
templates:
  - name: small
    image: ubuntu-24.04
    cpu: 1
    memory_mb: 1024
instances:
  - name: app
    template: small
    placement:
      server: host-does-not-exist
"#;

async fn create_arch_with_yaml(state: &AppState, name: &str, yaml: Option<String>) -> String {
    create_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(CreateArchitectureRequest {
            name: name.to_string(),
            description: None,
            environment: None,
            display_name: None,
            design_graph_json: None,
            latest_yaml: yaml,
        }),
    )
    .await
    .expect("create")
    .0
    .architecture
    .id
}

// ---------------------------------------------------------------------------
// plan happy path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plan_happy_path() {
    let clock = ManualClock::new(t0());
    let state = build_state_with_clock(clock.clone()).await;
    seed_capable_host(&state).await;
    let id = create_arch_with_yaml(&state, "happy", Some(HAPPY_YAML.to_string())).await;

    let resp = plan_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(PlanArchitectureRequest {
            id: id.clone(),
            allow_warnings: None,
            refresh_inventory: None,
        }),
    )
    .await
    .expect("plan should succeed")
    .0;

    assert_eq!(resp.architecture_id, id);
    assert_eq!(resp.mode, PlanMode::Apply);
    // FIX 2: PlanResponse must echo the architecture_version (numeric) and
    // architecture_version_id so the UI can pin a subsequent apply call to
    // this exact version without an extra round-trip.
    assert_eq!(
        resp.architecture_version, 1,
        "happy plan must echo the topology version_number"
    );
    assert!(
        !resp.architecture_version_id.is_empty(),
        "happy plan must carry a non-empty architecture_version_id"
    );
    // Two instances + one template + (image not in snapshot) all become
    // Creates. We assert the summary instance + template counts and that
    // the status is one of the two terminal phase-4 successes.
    assert!(
        resp.summary.create >= 2,
        "expected at least the two instance Creates, got {:?}",
        resp.summary
    );
    assert!(matches!(
        resp.status,
        PlanStatus::ReadyToApply | PlanStatus::RequiresConfirmation
    ));
    // Expiry comes from the injected ManualClock — t0 + 15min — so it is
    // deterministic. (`created_at` is set by SQLite default and reflects
    // real wall-clock; we don't compare against it here.)
    assert_eq!(resp.expires_at, t0() + Duration::minutes(15));
}

// ---------------------------------------------------------------------------
// plan with blocking finding → failed_validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plan_with_blocking_finding() {
    let clock = ManualClock::new(t0());
    let state = build_state_with_clock(clock).await;
    // No host seeded — the "host-does-not-exist" placement triggers a
    // blocking fleet finding.
    let id = create_arch_with_yaml(&state, "missing-host", Some(NO_HOST_YAML.to_string())).await;

    let resp = plan_architecture(
        BearerToken(claims_for("operator")),
        State(state),
        Json(PlanArchitectureRequest {
            id: id.clone(),
            allow_warnings: None,
            refresh_inventory: None,
        }),
    )
    .await
    .expect("plan should succeed (blocking findings still return 200)")
    .0;

    assert_eq!(resp.status, PlanStatus::FailedValidation);
    assert!(
        resp.changes.is_empty(),
        "failed_validation must have empty changes"
    );
    assert!(
        !resp.warnings.is_empty(),
        "failed_validation must carry the blocking finding messages"
    );
    // FIX 2: even on the failed-validation path the response carries the
    // architecture version pair — clients use it to render "version N
    // failed" without a second fetch.
    assert_eq!(resp.architecture_version, 1);
    assert!(!resp.architecture_version_id.is_empty());
}

// ---------------------------------------------------------------------------
// plan with no yaml → 400
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plan_no_yaml_returns_400() {
    let clock = ManualClock::new(t0());
    let state = build_state_with_clock(clock).await;
    let id = create_arch_with_yaml(&state, "no-yaml", None).await;

    let err = plan_architecture(
        BearerToken(claims_for("operator")),
        State(state),
        Json(PlanArchitectureRequest {
            id,
            allow_warnings: None,
            refresh_inventory: None,
        }),
    )
    .await
    .expect_err("plan must fail when topology has no yaml");
    assert_eq!(err_status(&err), 400);
}

// ---------------------------------------------------------------------------
// viewer role rejected
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plan_viewer_returns_403() {
    let clock = ManualClock::new(t0());
    let state = build_state_with_clock(clock).await;
    let err = plan_architecture(
        BearerToken(claims_for("viewer")),
        State(state),
        Json(PlanArchitectureRequest {
            id: "any".to_string(),
            allow_warnings: None,
            refresh_inventory: None,
        }),
    )
    .await
    .expect_err("viewer must be forbidden");
    assert_eq!(err_status(&err), 403);
}

// ---------------------------------------------------------------------------
// destroy-plan emits Deletes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn destroy_plan_emits_deletes() {
    let clock = ManualClock::new(t0());
    let state = build_state_with_clock(clock).await;
    seed_capable_host(&state).await;
    let id = create_arch_with_yaml(&state, "destroy-me", Some(HAPPY_YAML.to_string())).await;

    let resp = destroy_plan_architecture(
        BearerToken(claims_for("operator")),
        State(state),
        Json(PlanArchitectureRequest {
            id,
            allow_warnings: None,
            refresh_inventory: None,
        }),
    )
    .await
    .expect("destroy-plan should succeed")
    .0;

    assert_eq!(resp.mode, PlanMode::Destroy);
    assert!(!resp.changes.is_empty(), "destroy plan should emit changes");
    for change in &resp.changes {
        assert_eq!(
            change.action,
            PlanAction::Delete,
            "every destroy-plan change must be a Delete, got {:?}",
            change
        );
    }
    // Destroy plans always require confirmation.
    assert_eq!(resp.status, PlanStatus::RequiresConfirmation);
    assert!(resp.changes.iter().all(|c| c.requires_confirmation));
    // FIX 2: destroy plans carry the version pair too.
    assert_eq!(resp.architecture_version, 1);
    assert!(!resp.architecture_version_id.is_empty());
}

// ---------------------------------------------------------------------------
// discard-plan idempotency + not-found
// ---------------------------------------------------------------------------

#[tokio::test]
async fn discard_plan_idempotent() {
    let clock = ManualClock::new(t0());
    let state = build_state_with_clock(clock).await;
    seed_capable_host(&state).await;
    let id = create_arch_with_yaml(&state, "to-discard", Some(HAPPY_YAML.to_string())).await;

    let plan = plan_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(PlanArchitectureRequest {
            id,
            allow_warnings: None,
            refresh_inventory: None,
        }),
    )
    .await
    .expect("plan")
    .0;

    // FIX 2: even the discard-source plan call carries the version pair.
    assert_eq!(plan.architecture_version, 1);
    assert!(!plan.architecture_version_id.is_empty());

    let plan_id = plan.plan_id.clone();

    let first = discard_plan_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(DiscardPlanRequest {
            plan_id: plan_id.clone(),
        }),
    )
    .await
    .expect("first discard")
    .0;
    assert_eq!(first.status, "discarded");

    // Second call must succeed identically.
    let second = discard_plan_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(DiscardPlanRequest {
            plan_id: plan_id.clone(),
        }),
    )
    .await
    .expect("second discard must be idempotent")
    .0;
    assert_eq!(second.status, "discarded");

    // Underlying row reflects the discarded status.
    let plan_repo = PlanRepository::new(state.pool.clone());
    let row = plan_repo
        .get(&ArchitecturePlanId::new(plan_id).unwrap(), None)
        .await
        .expect("plan row exists");
    assert_eq!(row.status, PlanStatus::Discarded);
}

#[tokio::test]
async fn discard_plan_not_found() {
    let clock = ManualClock::new(t0());
    let state = build_state_with_clock(clock).await;

    let err = discard_plan_architecture(
        BearerToken(claims_for("operator")),
        State(state),
        Json(DiscardPlanRequest {
            plan_id: "not-a-real-plan-id".to_string(),
        }),
    )
    .await
    .expect_err("unknown plan id must 404");
    assert_eq!(err_status(&err), 404);
}

// ---------------------------------------------------------------------------
// FIX 4: discard-plan must reject from terminal states with PLAN_NOT_DISCARDABLE
// ---------------------------------------------------------------------------

/// A plan in `Applying`, `Applied`, `Failed`, or `Expired` is terminal —
/// discarding it would imply the apply path could be unwound after the
/// fact. The handler must refuse with 409 / `code: "PLAN_NOT_DISCARDABLE"`
/// so the UI surfaces a clear "this plan can no longer be discarded"
/// message instead of letting the operator believe state was rolled back.
#[tokio::test]
async fn discard_plan_in_terminal_state_returns_409() {
    let clock = ManualClock::new(t0());
    let state = build_state_with_clock(clock).await;
    seed_capable_host(&state).await;
    let id = create_arch_with_yaml(&state, "terminal", Some(HAPPY_YAML.to_string())).await;

    let plan = plan_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(PlanArchitectureRequest {
            id,
            allow_warnings: None,
            refresh_inventory: None,
        }),
    )
    .await
    .expect("plan")
    .0;

    let plan_id = ArchitecturePlanId::new(plan.plan_id.clone()).unwrap();
    // Move the plan into the terminal `Applied` state directly via the
    // store layer — Phase 5's apply path will do this in production but
    // doesn't exist yet, so we drive the transition by hand.
    let plan_repo = PlanRepository::new(state.pool.clone());
    plan_repo
        .update_status(PlanStatusUpdateInput {
            id: plan_id.clone(),
            status: PlanStatus::Applied,
            confirmed_by: None,
            mark_confirmed: false,
            mark_discarded: false,
            discarded_by: None,
        })
        .await
        .expect("force-applied");

    let err = discard_plan_architecture(
        BearerToken(claims_for("operator")),
        State(state),
        Json(DiscardPlanRequest {
            plan_id: plan_id.into_inner(),
        }),
    )
    .await
    .expect_err("discard from Applied must be rejected");
    assert_eq!(err_status(&err), 409);
    match err {
        BffError::PlanNotDiscardable { current_status, .. } => {
            assert_eq!(current_status, PlanStatus::Applied);
        }
        other => panic!("expected PlanNotDiscardable, got {other:?}"),
    }
}

/// FIX 4: the discard-plan handler stamps `discarded_by` with the caller's
/// subject so audit reviews can identify the actor without joining
/// against an external table.
#[tokio::test]
async fn discard_plan_records_actor() {
    let clock = ManualClock::new(t0());
    let state = build_state_with_clock(clock).await;
    seed_capable_host(&state).await;
    let id = create_arch_with_yaml(&state, "actor", Some(HAPPY_YAML.to_string())).await;

    let plan = plan_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(PlanArchitectureRequest {
            id,
            allow_warnings: None,
            refresh_inventory: None,
        }),
    )
    .await
    .expect("plan")
    .0;

    let plan_id_str = plan.plan_id.clone();
    let _ = discard_plan_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(DiscardPlanRequest {
            plan_id: plan_id_str.clone(),
        }),
    )
    .await
    .expect("discard");

    let plan_repo = PlanRepository::new(state.pool.clone());
    let row = plan_repo
        .get(&ArchitecturePlanId::new(plan_id_str).unwrap(), None)
        .await
        .expect("plan row exists");
    assert_eq!(row.status, PlanStatus::Discarded);
    // `claims_for("operator")` builds a Claims with sub = "u-tester".
    assert_eq!(
        row.discarded_by.as_deref(),
        Some("u-tester"),
        "discarded_by must record the caller subject"
    );
    assert!(
        row.discarded_at.is_some(),
        "discarded_at must be stamped on the discard transition"
    );
}
