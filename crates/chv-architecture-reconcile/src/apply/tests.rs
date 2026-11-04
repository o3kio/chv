//! Unit + integration tests for [`super::apply_plan`].

use chrono::{Duration, Utc};
use chv_common::clock::{Clock, ManualClock};
use chv_controlplane_store::{
    test_util::TestDb, ApplyRunRepository, OperationRepository, PlanCreateInput, PlanRepository,
    PlanStatusUpdateInput, TopologyCreateInput, TopologyRepository, VersionCreateInput,
    VersionRepository,
};
use chv_controlplane_types::architecture::{
    ArchitectureId, ArchitecturePlan, ArchitecturePlanId, ArchitectureStatus,
    ArchitectureVersionId, PlanAction, PlanChange, PlanMode, PlanStatus, ResourceType, Risk,
};
use chv_controlplane_types::domain::{OperationId, OperationStatus, ResourceId, ResourceKind};

use super::{apply_plan, ApplyContext, ApplyError, ConfirmationToken};
use crate::plan::{Plan, PlanSummary};

const TOPO_ID: &str = "topo-apply";
const TOPO_NAME: &str = "alpha";
const VER_ID: &str = "ver-apply-1";
const PLAN_ID: &str = "plan-apply-1";

fn aid() -> ArchitectureId {
    ArchitectureId::new(TOPO_ID).unwrap()
}

fn vid() -> ArchitectureVersionId {
    ArchitectureVersionId::new(VER_ID).unwrap()
}

fn pid() -> ArchitecturePlanId {
    ArchitecturePlanId::new(PLAN_ID).unwrap()
}

/// Bundle returned by [`fixture`] containing the seeded repos, a plan
/// record, an apply context primed for the happy path, and a manual
/// clock pinned to a fixed instant so the tests are deterministic.
struct Fixture {
    ops_repo: OperationRepository,
    runs_repo: ApplyRunRepository,
    plans_repo: PlanRepository,
    topo_repo: TopologyRepository,
    plan_record: ArchitecturePlan,
    ctx: ApplyContext,
    clock: ManualClock,
}

async fn fixture(plan_status: PlanStatus, plan_mode: PlanMode) -> Fixture {
    let db = TestDb::new().await;

    let topo = TopologyRepository::new(db.pool.clone());
    let ver = VersionRepository::new(db.pool.clone());
    let plans_repo = PlanRepository::new(db.pool.clone());

    topo.create(TopologyCreateInput {
        id: aid(),
        name: TOPO_NAME.to_string(),
        display_name: Some(format!("{TOPO_NAME} display")),
        description: None,
        environment: Some("staging".to_string()),
        status: ArchitectureStatus::Draft,
        owner_user_id: Some("user-1".to_string()),
        design_graph_json: None,
        latest_yaml: None,
    })
    .await
    .expect("create topology");

    ver.create(VersionCreateInput {
        id: vid(),
        architecture_id: aid(),
        version_number: 1,
        yaml_content: "x".to_string(),
        design_graph_json: None,
        normalized_model_json: None,
        change_summary: None,
        created_by: None,
    })
    .await
    .expect("create version");

    // Plans must be created in a non-ready state because the create path
    // does not accept arbitrary statuses for the initial row in this
    // codebase. We then transition via update_status to land the test
    // plan in the requested status.
    let now = Utc::now();
    let initial_status = match plan_status {
        PlanStatus::Draft => PlanStatus::Draft,
        _ => PlanStatus::Draft,
    };
    let _ = plans_repo
        .create(PlanCreateInput {
            id: pid(),
            architecture_id: aid(),
            architecture_version_id: vid(),
            inventory_snapshot_id: None,
            mode: plan_mode,
            status: initial_status,
            plan_json: Some("{\"changes\":[]}".to_string()),
            summary_json: None,
            created_by: Some("senol".to_string()),
            expires_at: now + Duration::minutes(15),
        })
        .await
        .expect("create plan");

    if plan_status != initial_status {
        plans_repo
            .update_status(PlanStatusUpdateInput {
                id: pid(),
                status: plan_status,
                confirmed_by: None,
                mark_confirmed: false,
                mark_discarded: false,
                discarded_by: None,
            })
            .await
            .expect("transition plan status");
    }

    let plan_record = plans_repo.get(&pid(), None).await.expect("fetch plan");

    let ctx = ApplyContext {
        architecture_id: aid(),
        architecture_version_id: vid(),
        topology_name: TOPO_NAME.to_string(),
        environment: Some("staging".to_string()),
        plan_id: pid(),
        requested_by: Some("senol".to_string()),
        confirmation: ConfirmationToken::default(),
        acknowledged_warnings: false,
        topology_version: 1,
    };

    Fixture {
        ops_repo: OperationRepository::new(db.pool.clone()),
        runs_repo: ApplyRunRepository::new(db.pool.clone()),
        plans_repo,
        topo_repo: topo,
        plan_record,
        ctx,
        clock: ManualClock::new(now),
    }
}

fn change(action: PlanAction, resource_type: ResourceType, name: &str) -> PlanChange {
    PlanChange {
        action,
        resource_type,
        resource_name: name.to_string(),
        resource_ref: format!("{}/{}", resource_type_as_str_test(resource_type), name),
        description: format!("{action:?} {name}"),
        risk: Risk::Low,
        requires_confirmation: false,
    }
}

fn resource_type_as_str_test(t: ResourceType) -> &'static str {
    match t {
        ResourceType::Server => "server",
        ResourceType::Network => "network",
        ResourceType::Datastore => "datastore",
        ResourceType::BackupTarget => "backup_target",
        ResourceType::BackupPolicy => "backup_policy",
        ResourceType::Image => "image",
        ResourceType::Template => "template",
        ResourceType::Instance => "instance",
        ResourceType::SshKey => "ssh_key",
        ResourceType::InstanceUser => "instance_user",
        ResourceType::Role => "role",
        ResourceType::User => "user",
        ResourceType::Project => "project",
    }
}

fn plan_with(changes: Vec<PlanChange>, mode: PlanMode, warnings: Vec<String>) -> Plan {
    let warning_count = warnings.len() as u32;
    let summary = PlanSummary::from_changes(&changes, warning_count);
    Plan {
        mode,
        changes,
        summary,
        warnings,
    }
}

// ── Happy path ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn apply_plan_inserts_one_operation_per_change() {
    let f = fixture(PlanStatus::ReadyToApply, PlanMode::Apply).await;

    let plan = plan_with(
        vec![
            change(PlanAction::Create, ResourceType::Instance, "vm-1"),
            change(PlanAction::Create, ResourceType::Network, "net-1"),
            change(PlanAction::Update, ResourceType::Datastore, "ds-1"),
        ],
        PlanMode::Apply,
        vec![],
    );

    let outcome = apply_plan(
        &plan,
        &f.plan_record,
        &f.ops_repo,
        &f.runs_repo,
        &f.plans_repo,
        &f.topo_repo,
        &f.ctx,
        &f.clock,
    )
    .await
    .expect("apply happy path");

    assert_eq!(outcome.queued_operations.len(), 3);
    assert!(outcome.skipped_operations.is_empty());
    assert_eq!(
        outcome.run.status,
        chv_controlplane_types::architecture::RunStatus::Running
    );
    assert!(outcome.run.task_id.is_some());
    assert!(outcome.run.started_at.is_some());
}

// ── Idempotency ────────────────────────────────────────────────────────────

#[tokio::test]
async fn apply_plan_idempotent_on_retry() {
    let f = fixture(PlanStatus::ReadyToApply, PlanMode::Apply).await;

    let plan = plan_with(
        vec![
            change(PlanAction::Create, ResourceType::Instance, "vm-1"),
            change(PlanAction::Create, ResourceType::Network, "net-1"),
        ],
        PlanMode::Apply,
        vec![],
    );

    let first = apply_plan(
        &plan,
        &f.plan_record,
        &f.ops_repo,
        &f.runs_repo,
        &f.plans_repo,
        &f.topo_repo,
        &f.ctx,
        &f.clock,
    )
    .await
    .expect("first apply");

    // Force the first operation into Succeeded via direct repo write so
    // the second apply must short-circuit it as a "skipped" op rather
    // than re-enqueue.
    let succeeded_id = first.queued_operations[0].clone();
    f.ops_repo
        .update_status(&chv_controlplane_store::OperationStatusUpdateInput {
            operation_id: succeeded_id.clone(),
            status: OperationStatus::Succeeded,
            error_code: None,
            error_message: None,
            observed_generation: None,
            updated_by: Some("test".to_string()),
            updated_unix_ms: f.clock.now().timestamp_millis(),
        })
        .await
        .expect("mark op succeeded");

    // Re-fetch the plan record so the second apply sees the updated
    // status (Applying after first apply), as the BFF does in production.
    let plan_record_v2 = f
        .plans_repo
        .get(&f.plan_record.id, None)
        .await
        .expect("re-fetch plan record");

    let second = apply_plan(
        &plan,
        &plan_record_v2,
        &f.ops_repo,
        &f.runs_repo,
        &f.plans_repo,
        &f.topo_repo,
        &f.ctx,
        &f.clock,
    )
    .await
    .expect("second apply");

    // The succeeded op shows up in skipped, the other op reappears in queued.
    assert!(
        second.skipped_operations.contains(&succeeded_id),
        "expected skipped to contain the succeeded operation; got {:?}",
        second.skipped_operations
    );
    assert_eq!(
        second.skipped_operations.len() + second.queued_operations.len(),
        plan.changes.len(),
        "every plan change should map to exactly one operation across the two buckets"
    );

    // The full set of operation ids returned across both calls must be
    // identical — that is the idempotency contract.
    let mut first_ids: Vec<&OperationId> = first
        .queued_operations
        .iter()
        .chain(first.skipped_operations.iter())
        .collect();
    let mut second_ids: Vec<&OperationId> = second
        .queued_operations
        .iter()
        .chain(second.skipped_operations.iter())
        .collect();
    first_ids.sort_by_key(|id| id.as_str().to_string());
    second_ids.sort_by_key(|id| id.as_str().to_string());
    assert_eq!(
        first_ids, second_ids,
        "idempotency: same operation ids across retries"
    );
}

// ── Destructive guard ──────────────────────────────────────────────────────

#[tokio::test]
async fn apply_plan_rejects_missing_typed_name_on_destructive() {
    let f = fixture(PlanStatus::ReadyToApply, PlanMode::Destroy).await;

    let plan = plan_with(
        vec![change(PlanAction::Delete, ResourceType::Instance, "vm-1")],
        PlanMode::Destroy,
        vec![],
    );

    let err = apply_plan(
        &plan,
        &f.plan_record,
        &f.ops_repo,
        &f.runs_repo,
        &f.plans_repo,
        &f.topo_repo,
        &f.ctx,
        &f.clock,
    )
    .await
    .expect_err("destroy without typed-name must fail");

    match err {
        ApplyError::MissingConfirmation {
            plan_id,
            topology_name,
        } => {
            assert_eq!(plan_id, PLAN_ID);
            assert_eq!(topology_name, TOPO_NAME);
        }
        other => panic!("expected MissingConfirmation, got {other:?}"),
    }
}

// ── Warning-ack guard ──────────────────────────────────────────────────────

#[tokio::test]
async fn apply_plan_rejects_unacknowledged_warnings() {
    let f = fixture(PlanStatus::ReadyToApply, PlanMode::Apply).await;

    let plan = plan_with(
        vec![change(PlanAction::Create, ResourceType::Instance, "vm-1")],
        PlanMode::Apply,
        vec!["high latency suspected".to_string()],
    );

    let err = apply_plan(
        &plan,
        &f.plan_record,
        &f.ops_repo,
        &f.runs_repo,
        &f.plans_repo,
        &f.topo_repo,
        &f.ctx,
        &f.clock,
    )
    .await
    .expect_err("warnings without ack must fail");

    match err {
        ApplyError::MissingWarningAck { plan_id, warnings } => {
            assert_eq!(plan_id, PLAN_ID);
            assert_eq!(warnings, 1);
        }
        other => panic!("expected MissingWarningAck, got {other:?}"),
    }
}

// ── NoOp filtering ─────────────────────────────────────────────────────────

#[tokio::test]
async fn apply_plan_skips_no_op_changes() {
    let f = fixture(PlanStatus::ReadyToApply, PlanMode::Apply).await;

    let plan = plan_with(
        vec![
            change(PlanAction::NoOp, ResourceType::Instance, "vm-noop"),
            change(PlanAction::Create, ResourceType::Instance, "vm-real"),
        ],
        PlanMode::Apply,
        vec![],
    );

    let outcome = apply_plan(
        &plan,
        &f.plan_record,
        &f.ops_repo,
        &f.runs_repo,
        &f.plans_repo,
        &f.topo_repo,
        &f.ctx,
        &f.clock,
    )
    .await
    .expect("apply with noop");

    assert_eq!(outcome.queued_operations.len(), 1);
    assert!(outcome.skipped_operations.is_empty());
}

// ── Run-state transition ───────────────────────────────────────────────────

#[tokio::test]
async fn apply_plan_marks_run_running_after_first_op() {
    let f = fixture(PlanStatus::ReadyToApply, PlanMode::Apply).await;

    let plan = plan_with(
        vec![change(PlanAction::Create, ResourceType::Instance, "vm-1")],
        PlanMode::Apply,
        vec![],
    );

    let outcome = apply_plan(
        &plan,
        &f.plan_record,
        &f.ops_repo,
        &f.runs_repo,
        &f.plans_repo,
        &f.topo_repo,
        &f.ctx,
        &f.clock,
    )
    .await
    .expect("apply");

    assert_eq!(
        outcome.run.status,
        chv_controlplane_types::architecture::RunStatus::Running
    );
    assert!(outcome.run.started_at.is_some());
    assert_eq!(
        outcome.run.task_id.as_deref(),
        Some(outcome.queued_operations[0].as_str()),
        "task_id must point at first enqueued operation"
    );
}

// ── Expiry guard ───────────────────────────────────────────────────────────

#[tokio::test]
async fn apply_plan_rejects_expired_plan() {
    let f = fixture(PlanStatus::ReadyToApply, PlanMode::Apply).await;

    // Advance past the 15-min expires_at boundary.
    f.clock.advance(Duration::minutes(20));

    let plan = plan_with(
        vec![change(PlanAction::Create, ResourceType::Instance, "vm-1")],
        PlanMode::Apply,
        vec![],
    );

    let err = apply_plan(
        &plan,
        &f.plan_record,
        &f.ops_repo,
        &f.runs_repo,
        &f.plans_repo,
        &f.topo_repo,
        &f.ctx,
        &f.clock,
    )
    .await
    .expect_err("expired plan must fail");

    match err {
        ApplyError::PlanExpired {
            plan_id,
            expires_at,
        } => {
            assert_eq!(plan_id, PLAN_ID);
            assert!(!expires_at.is_empty());
        }
        other => panic!("expected PlanExpired, got {other:?}"),
    }
}

// ── Plan-status guard ──────────────────────────────────────────────────────

#[tokio::test]
async fn apply_plan_rejects_non_ready_to_apply_status() {
    let f = fixture(PlanStatus::Draft, PlanMode::Apply).await;

    let plan = plan_with(
        vec![change(PlanAction::Create, ResourceType::Instance, "vm-1")],
        PlanMode::Apply,
        vec![],
    );

    let err = apply_plan(
        &plan,
        &f.plan_record,
        &f.ops_repo,
        &f.runs_repo,
        &f.plans_repo,
        &f.topo_repo,
        &f.ctx,
        &f.clock,
    )
    .await
    .expect_err("draft plan must fail");

    match err {
        ApplyError::PlanNotApplicable {
            plan_id,
            current_status,
        } => {
            assert_eq!(plan_id, PLAN_ID);
            assert_eq!(current_status, "draft");
        }
        other => panic!("expected PlanNotApplicable, got {other:?}"),
    }

    // Sanity check: no apply_run row should have been written when the
    // pre-condition guard fires.
    let runs = f
        .runs_repo
        .list_for_architecture(&aid(), None)
        .await
        .expect("list runs");
    assert!(
        runs.is_empty(),
        "no run should be created when status guard rejects"
    );
    // Plan stays where it was.
    let plan_after = f.plans_repo.get(&pid(), None).await.unwrap();
    assert_eq!(plan_after.status, PlanStatus::Draft);
    // Suppress unused warnings.
    let _ = (ResourceKind::Vm, ResourceId::new("x").unwrap());
}

// ── B3: resource_name sanitization ─────────────────────────────────────────

#[tokio::test]
async fn apply_plan_rejects_resource_name_containing_double_colon() {
    let f = fixture(PlanStatus::ReadyToApply, PlanMode::Apply).await;

    let plan = plan_with(
        vec![change(PlanAction::Create, ResourceType::Instance, "vm::1")],
        PlanMode::Apply,
        vec![],
    );

    let err = apply_plan(
        &plan,
        &f.plan_record,
        &f.ops_repo,
        &f.runs_repo,
        &f.plans_repo,
        &f.topo_repo,
        &f.ctx,
        &f.clock,
    )
    .await
    .expect_err("resource_name with `::` must be rejected");

    match err {
        ApplyError::InvalidResourceName { resource_name, .. } => {
            assert_eq!(resource_name, "vm::1");
        }
        other => panic!("expected InvalidResourceName, got {other:?}"),
    }

    // No apply_run row should have been written when the guard fires.
    let runs = f
        .runs_repo
        .list_for_architecture(&aid(), None)
        .await
        .unwrap();
    assert!(runs.is_empty());
    // Plan stays in ReadyToApply (the transition only fires after the
    // sanitization guard passes).
    let plan_after = f.plans_repo.get(&pid(), None).await.unwrap();
    assert_eq!(plan_after.status, PlanStatus::ReadyToApply);
}

#[tokio::test]
async fn apply_plan_rejects_resource_name_containing_slash() {
    let f = fixture(PlanStatus::ReadyToApply, PlanMode::Apply).await;

    let plan = plan_with(
        vec![change(PlanAction::Create, ResourceType::Instance, "vm/a")],
        PlanMode::Apply,
        vec![],
    );

    let err = apply_plan(
        &plan,
        &f.plan_record,
        &f.ops_repo,
        &f.runs_repo,
        &f.plans_repo,
        &f.topo_repo,
        &f.ctx,
        &f.clock,
    )
    .await
    .expect_err("resource_name with `/` must be rejected");

    match err {
        ApplyError::InvalidResourceName { resource_name, .. } => {
            assert_eq!(resource_name, "vm/a");
        }
        other => panic!("expected InvalidResourceName, got {other:?}"),
    }
}

// ── B5: plan status transition (ReadyToApply -> Applying) ──────────────────

#[tokio::test]
async fn apply_plan_transitions_plan_to_applying() {
    let f = fixture(PlanStatus::ReadyToApply, PlanMode::Apply).await;

    let plan = plan_with(
        vec![change(PlanAction::Create, ResourceType::Instance, "vm-1")],
        PlanMode::Apply,
        vec![],
    );

    apply_plan(
        &plan,
        &f.plan_record,
        &f.ops_repo,
        &f.runs_repo,
        &f.plans_repo,
        &f.topo_repo,
        &f.ctx,
        &f.clock,
    )
    .await
    .expect("apply");

    let plan_after = f.plans_repo.get(&pid(), None).await.unwrap();
    assert_eq!(plan_after.status, PlanStatus::Applying);
}

#[tokio::test]
async fn apply_plan_concurrent_discard_loses_race_returns_plan_not_applicable() {
    let f = fixture(PlanStatus::ReadyToApply, PlanMode::Apply).await;

    // Simulate a concurrent transition: between the test's read of the
    // plan_record and the apply_plan call, somebody discarded the plan.
    f.plans_repo
        .update_status(PlanStatusUpdateInput {
            id: pid(),
            status: PlanStatus::Discarded,
            confirmed_by: None,
            mark_confirmed: false,
            mark_discarded: true,
            discarded_by: Some("racing-actor".to_string()),
        })
        .await
        .expect("force discard");

    let plan = plan_with(
        vec![change(PlanAction::Create, ResourceType::Instance, "vm-1")],
        PlanMode::Apply,
        vec![],
    );

    let err = apply_plan(
        &plan,
        &f.plan_record, // stale: still says ReadyToApply
        &f.ops_repo,
        &f.runs_repo,
        &f.plans_repo,
        &f.topo_repo,
        &f.ctx,
        &f.clock,
    )
    .await
    .expect_err("racing discard must surface as PlanNotApplicable");

    match err {
        ApplyError::PlanNotApplicable {
            plan_id,
            current_status,
        } => {
            assert_eq!(plan_id, PLAN_ID);
            assert_eq!(current_status, "discarded");
        }
        other => panic!("expected PlanNotApplicable, got {other:?}"),
    }

    // The apply_run must have been rolled back to Cancelled with the
    // explanatory error_message.
    let runs = f
        .runs_repo
        .list_for_architecture(&aid(), None)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1, "apply_run row should exist (rolled back)");
    assert_eq!(
        runs[0].status,
        chv_controlplane_types::architecture::RunStatus::Cancelled
    );
    assert_eq!(
        runs[0].error_message.as_deref(),
        Some("plan no longer ReadyToApply")
    );
}

#[tokio::test]
async fn apply_plan_second_call_with_already_applying_plan_returns_plan_not_applicable() {
    let f = fixture(PlanStatus::ReadyToApply, PlanMode::Apply).await;

    let plan = plan_with(
        vec![change(PlanAction::Create, ResourceType::Instance, "vm-1")],
        PlanMode::Apply,
        vec![],
    );

    let _first = apply_plan(
        &plan,
        &f.plan_record,
        &f.ops_repo,
        &f.runs_repo,
        &f.plans_repo,
        &f.topo_repo,
        &f.ctx,
        &f.clock,
    )
    .await
    .expect("first apply");

    // Second apply against the same plan_record (still says
    // ReadyToApply locally because we never re-read it) must lose the
    // race because the plan row is now `Applying` in the DB.
    let err = apply_plan(
        &plan,
        &f.plan_record,
        &f.ops_repo,
        &f.runs_repo,
        &f.plans_repo,
        &f.topo_repo,
        &f.ctx,
        &f.clock,
    )
    .await
    .expect_err("second apply must lose the race");

    match err {
        ApplyError::PlanNotApplicable {
            plan_id,
            current_status,
        } => {
            assert_eq!(plan_id, PLAN_ID);
            assert_eq!(current_status, "applying");
        }
        other => panic!("expected PlanNotApplicable, got {other:?}"),
    }
}

// ── B4: started_at preserved across failure marker ─────────────────────────

#[tokio::test]
async fn apply_plan_failure_preserves_started_at() {
    let f = fixture(PlanStatus::ReadyToApply, PlanMode::Apply).await;

    // Build a plan with a Create change so the happy path enqueues an
    // operation. Then, force the operations table to error on insert by
    // pre-inserting an op with the same operation_id — ahem, the
    // operation_id is a fresh `gen_resource_id`, so we cannot collide on
    // it. Instead, drop the operations table to provoke a hard failure.
    let plan = plan_with(
        vec![change(PlanAction::Create, ResourceType::Instance, "vm-1")],
        PlanMode::Apply,
        vec![],
    );

    sqlx::query("DROP TABLE operations")
        .execute(f.ops_repo.pool())
        .await
        .expect("drop operations");

    let _ = apply_plan(
        &plan,
        &f.plan_record,
        &f.ops_repo,
        &f.runs_repo,
        &f.plans_repo,
        &f.topo_repo,
        &f.ctx,
        &f.clock,
    )
    .await
    .expect_err("apply must fail when the operations table is gone");

    let runs = f
        .runs_repo
        .list_for_architecture(&aid(), None)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1);
    assert!(
        runs[0].started_at.is_some(),
        "failure marker must preserve started_at so the UI renders a duration; got {:?}",
        runs[0].started_at
    );
    assert_eq!(
        runs[0].status,
        chv_controlplane_types::architecture::RunStatus::Failed
    );
}

// ── H4: plan rolls back to Failed when enqueue errors mid-loop ─────────────
//
// Pre-fix bug: after the CAS to `Applying` succeeds, if `enqueue_change`
// fails partway through the change list the apply_run is marked `Failed`
// but the *plan* row stays in `Applying`. The BFF discard handler refuses
// a plan in `Applying` (it is normally a transient state), so the
// operator gets a ghost plan they can neither retry nor discard until the
// 15-minute TTL expires the row out — and even then `Failed`/`Expired`
// were non-discardable in the legacy handler.
//
// Post-fix contract: when the enqueue arm errors, we roll the plan row
// back to `Failed` before returning so the operator has a clean retry
// path (re-plan, re-apply) AND a clean abandon path (discard the now-
// `Failed` plan). The apply_run continues to be marked `Failed` so the
// run-history UI renders correctly. The original error is propagated
// regardless of whether the rollback succeeds — masking it would hide
// the real failure cause.

#[tokio::test]
async fn apply_plan_rolls_back_to_failed_when_enqueue_errors_mid_apply() {
    let f = fixture(PlanStatus::ReadyToApply, PlanMode::Apply).await;

    // Three Create changes: even if the first one were to succeed (it
    // won't, because we drop the table) the second/third would also
    // fail. We want the rollback to fire on the first iteration's error
    // and leave the plan in `Failed`.
    let plan = plan_with(
        vec![
            change(PlanAction::Create, ResourceType::Instance, "vm-1"),
            change(PlanAction::Create, ResourceType::Network, "net-1"),
            change(PlanAction::Update, ResourceType::Datastore, "ds-1"),
        ],
        PlanMode::Apply,
        vec![],
    );

    // Drop the operations table so any `create_or_get` errors out from
    // the store layer — same shape as a transient SQLite write failure
    // mid-loop in production.
    sqlx::query("DROP TABLE operations")
        .execute(f.ops_repo.pool())
        .await
        .expect("drop operations");

    let err = apply_plan(
        &plan,
        &f.plan_record,
        &f.ops_repo,
        &f.runs_repo,
        &f.plans_repo,
        &f.topo_repo,
        &f.ctx,
        &f.clock,
    )
    .await
    .expect_err("enqueue failure must propagate");

    // Original store error must surface unmasked.
    assert!(
        matches!(err, ApplyError::Store(_)),
        "expected ApplyError::Store, got {err:?}"
    );

    // The apply_run row should be Failed (matches existing
    // `apply_plan_failure_preserves_started_at` contract).
    let runs = f
        .runs_repo
        .list_for_architecture(&aid(), None)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].status,
        chv_controlplane_types::architecture::RunStatus::Failed,
        "apply_run must be marked Failed when enqueue errors"
    );

    // The plan row must be rolled back to Failed — NOT stuck in Applying.
    // This is the H4 regression: pre-fix the plan stayed in `Applying`
    // forever (until 15-min TTL) and the operator could not discard it.
    let plan_after = f.plans_repo.get(&pid(), None).await.unwrap();
    assert_eq!(
        plan_after.status,
        PlanStatus::Failed,
        "plan must be rolled back to Failed (not stuck in Applying) when enqueue errors mid-loop; got {:?}",
        plan_after.status
    );
}
