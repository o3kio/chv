//! Integration tests for the topology lifecycle `status` column wiring.
//!
//! Before this fix the `architecture_topologies.status` column sat at
//! `draft` for the topology's lifetime: the only updater
//! (`set_validation_status`) wrote to `last_validation_status`, not
//! `status`. The CHECK constraint allows
//! `applying / applied / drifted / failed` but no code path actually
//! transitioned the column, so the dashboard's per-topology badge was
//! permanently stale.
//!
//! These tests pin the three writer paths now wired:
//!
//! 1. `apply_transitions_topology_status_to_applying` —
//!    apply handler → `apply_plan` → `set_lifecycle_status(Applying)`.
//! 2. `apply_orchestrator_terminal_writeback` —
//!    after the orchestrator marks `apply_run.status = Succeeded`,
//!    `set_topology_terminal_status` lands the topology row at `Applied`.
//! 3. `drift_persist_transitions_topology_status` —
//!    drift compute returns `Drifted` → drift writer transitions the
//!    topology row to `Drifted`.
//!
//! Each test asserts the topology row's `status` column directly via
//! `TopologyRepository::get` so the wiring is observable through the
//! repository surface (and therefore the dashboard) — not just through
//! a hand-rolled SQL probe.

use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::State;
use axum::Json;
use chrono::{TimeZone, Utc};
use chv_architecture_reconcile::set_topology_terminal_status;
use chv_common::ManualClock;
use chv_controlplane_store::{
    AlertRepository, AlertRuleRepository, ApplyRunRepository, ApplyRunUpdateInput,
    BackupRepository, DesiredStateRepository, DriftReportRepository, EventRepository,
    ImageRepository, NetworkRepository, NodeRepository, NotificationOutboxRepository,
    ObservedStateRepository, OperationRepository, PlanRepository, PlanStatusUpdateInput,
    TopologyRepository,
};
use chv_controlplane_types::architecture::{
    ArchitectureId, ArchitecturePlanId, ArchitectureStatus, PlanStatus, RunStatus,
};
use chv_webui_bff::auth::{BearerToken, Claims};
use chv_webui_bff::handlers::architectures::{
    apply_architecture, create_architecture, get_architecture_drift, plan_architecture,
    ApplyArchitectureRequest, ConfirmationDto, CreateArchitectureRequest, DriftRequest,
    PlanArchitectureRequest,
};
use chv_webui_bff::mutations::MutationService;
use chv_webui_bff::{AppState, BffError};
use sqlx::sqlite::SqlitePoolOptions;

// ---------------------------------------------------------------------------
// MutationService stub — none of the handlers under test touch mutations.
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

// ---------------------------------------------------------------------------
// Scaffolding
// ---------------------------------------------------------------------------

fn t0() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 15, 10, 0, 0).unwrap()
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
        token_version: None,
    }
}

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
"#;

/// YAML that declares a bridge that the live snapshot will not provide,
/// so drift compute returns `Drifted`. Mirrors `NETWORK_BRIDGE_YAML` from
/// `architectures_drift.rs`.
const NETWORK_BRIDGE_YAML: &str = r#"apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: net-drift
networks:
  - name: edge-1
    type: bridge
    bridge: br0
"#;

async fn create_arch(state: &AppState, name: &str, yaml: &str) -> String {
    create_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(CreateArchitectureRequest {
            name: name.to_string(),
            description: None,
            environment: None,
            display_name: None,
            design_graph_json: None,
            latest_yaml: Some(yaml.to_string()),
        }),
    )
    .await
    .expect("create")
    .0
    .architecture
    .id
}

async fn force_ready_plan(state: &AppState, arch_id: &str) -> String {
    let plan = plan_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(PlanArchitectureRequest {
            id: arch_id.to_string(),
            allow_warnings: None,
            refresh_inventory: None,
        }),
    )
    .await
    .expect("plan")
    .0;
    if plan.status != PlanStatus::ReadyToApply {
        let plan_repo = PlanRepository::new(state.pool.clone());
        plan_repo
            .update_status(PlanStatusUpdateInput {
                id: ArchitecturePlanId::new(plan.plan_id.clone()).unwrap(),
                status: PlanStatus::ReadyToApply,
                confirmed_by: Some("u-tester".to_string()),
                mark_confirmed: true,
                mark_discarded: false,
                discarded_by: None,
            })
            .await
            .expect("force-ready");
    }
    plan.plan_id
}

async fn topology_status(state: &AppState, arch_id: &str) -> ArchitectureStatus {
    let arch = ArchitectureId::new(arch_id.to_string()).expect("parse arch id");
    state
        .topology_repo
        .get(&arch, None)
        .await
        .expect("read topology")
        .status
}

// ---------------------------------------------------------------------------
// 1. apply transitions topology status to Applying
// ---------------------------------------------------------------------------

#[tokio::test]
async fn apply_transitions_topology_status_to_applying() {
    let clock = ManualClock::new(t0());
    let state = build_state_with_clock(clock).await;
    seed_capable_host(&state).await;

    let arch_id = create_arch(&state, "lifecycle-applying", HAPPY_YAML).await;

    // Pre-condition: a freshly-created topology starts at Draft. This is
    // the regression we are pinning — before the fix, the column would
    // STAY at Draft after apply.
    assert_eq!(
        topology_status(&state, &arch_id).await,
        ArchitectureStatus::Draft,
        "newly created topology must start at Draft"
    );

    let plan_id = force_ready_plan(&state, &arch_id).await;

    let _resp = apply_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(ApplyArchitectureRequest {
            id: arch_id.clone(),
            plan_id: plan_id.clone(),
            confirmation: ConfirmationDto::default(),
            acknowledged_warnings: false,
        }),
    )
    .await
    .expect("apply should succeed");

    // The CAS in apply_plan must have moved the lifecycle status.
    assert_eq!(
        topology_status(&state, &arch_id).await,
        ArchitectureStatus::Applying,
        "apply must transition topology status to Applying (was the silent regression)"
    );
}

// ---------------------------------------------------------------------------
// 2. orchestrator terminal writeback transitions topology status to Applied
// ---------------------------------------------------------------------------

#[tokio::test]
async fn apply_orchestrator_terminal_writeback() {
    let clock = ManualClock::new(t0());
    let state = build_state_with_clock(clock).await;
    seed_capable_host(&state).await;

    let arch_id = create_arch(&state, "lifecycle-applied", HAPPY_YAML).await;
    let plan_id = force_ready_plan(&state, &arch_id).await;

    let resp = apply_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(ApplyArchitectureRequest {
            id: arch_id.clone(),
            plan_id: plan_id.clone(),
            confirmation: ConfirmationDto::default(),
            acknowledged_warnings: false,
        }),
    )
    .await
    .expect("apply should succeed")
    .0;

    // Sanity: apply just moved us to Applying.
    assert_eq!(
        topology_status(&state, &arch_id).await,
        ArchitectureStatus::Applying
    );

    // Simulate the orchestrator marking apply_run.status = Succeeded.
    let run_id = chv_controlplane_types::architecture::ArchitectureApplyRunId::new(resp.run_id)
        .expect("run id");
    state
        .apply_runs
        .update(ApplyRunUpdateInput {
            id: run_id,
            status: Some(RunStatus::Succeeded),
            started_at: None,
            finished_at: Some(state.clock.now()),
            task_id: None,
            result_json: None,
            logs_ref: None,
            error_message: None,
        })
        .await
        .expect("mark run succeeded");

    // The orchestrator's terminal-state hand-off: call the public
    // writeback contract that the orchestrator is responsible for
    // invoking when the run flips to a terminal state.
    let arch = ArchitectureId::new(arch_id.clone()).unwrap();
    set_topology_terminal_status(&state.topology_repo, &arch, ArchitectureStatus::Applied)
        .await
        .expect("terminal writeback");

    assert_eq!(
        topology_status(&state, &arch_id).await,
        ArchitectureStatus::Applied,
        "terminal writeback must land the topology row at Applied"
    );

    // Idempotency: a second writeback for the same target is a no-op (no
    // version bump, no error) — important because the orchestrator may
    // legitimately double-fire on transient failures.
    set_topology_terminal_status(&state.topology_repo, &arch, ArchitectureStatus::Applied)
        .await
        .expect("idempotent writeback");
    assert_eq!(
        topology_status(&state, &arch_id).await,
        ArchitectureStatus::Applied
    );
}

// ---------------------------------------------------------------------------
// 3. drift persist transitions topology status to Drifted
// ---------------------------------------------------------------------------

#[tokio::test]
async fn drift_persist_transitions_topology_status() {
    let clock = ManualClock::new(t0());
    let state = build_state_with_clock(clock).await;
    let arch_id = create_arch(&state, "lifecycle-drifted", NETWORK_BRIDGE_YAML).await;

    // Seed a live network row that does not match the baseline so drift
    // compute returns Drifted (the bridge is declared in the baseline but
    // the live snapshot has no bridge populated).
    sqlx::query(
        r#"INSERT INTO networks (network_id, display_name, network_class)
           VALUES ('edge-1', 'edge-1', 'bridge')"#,
    )
    .execute(&state.pool)
    .await
    .unwrap();

    let resp = get_architecture_drift(
        BearerToken(claims_for("viewer")),
        State(state.clone()),
        Json(DriftRequest {
            id: arch_id.clone(),
            force_refresh: false,
        }),
    )
    .await
    .expect("drift compute")
    .0;

    assert_eq!(
        resp.status,
        chv_controlplane_types::architecture::DriftStatus::Drifted,
        "drift must report Drifted for this fixture"
    );

    // The drift writer must have transitioned the topology row.
    assert_eq!(
        topology_status(&state, &arch_id).await,
        ArchitectureStatus::Drifted,
        "drift persist must transition topology status to Drifted"
    );
}
