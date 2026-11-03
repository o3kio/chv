//! Phase 5 apply path — turn a `Plan` plus an `ArchitecturePlan` row into
//! a queued [`ArchitectureApplyRun`] backed by per-change Operations.
//!
//! The apply path is intentionally narrow:
//!
//! 1. Pre-condition guards (plan status, expiry, typed-name confirmation,
//!    warning acknowledgement).
//! 2. Insert an `apply_run` row with status [`RunStatus::Queued`].
//! 3. For each ordered change in the plan, idempotently insert (or fetch)
//!    a row in the `operations` table — keyed by
//!    `{plan_id}::{resource_ref}::{action}` so that re-applying the same
//!    plan after a partial failure picks up where it left off.
//! 4. Transition the run to [`RunStatus::Running`] with `task_id` pointing
//!    at the first newly-enqueued (or already-succeeded) operation so
//!    clients can stream progress.
//!
//! The orchestrator (out of scope for Phase 5) is responsible for the
//! terminal `Succeeded` / `PartiallyFailed` / `Failed` transitions; this
//! module only puts the run on the rails.

use chv_common::Clock;
use chv_controlplane_store::{
    ApplyRunCreateInput, ApplyRunRepository, ApplyRunUpdateInput, OperationCreateInput,
    OperationRepository, PlanRepository, StoreError, TopologyRepository,
};
use chv_controlplane_types::architecture::{
    ArchitectureApplyRun, ArchitectureApplyRunId, ArchitectureStatus, PlanAction, PlanStatus,
    ResourceType, RunStatus,
};
use chv_controlplane_types::domain::{OperationId, OperationStatus, ResourceId, ResourceKind};

use crate::plan::{is_expired, Plan};
use chv_controlplane_types::architecture::{ArchitecturePlan, PlanChange};

pub mod context;
pub mod error;

#[cfg(test)]
mod tests;

pub use context::{ApplyContext, ConfirmationToken};
pub use error::ApplyError;

/// Result of a successful [`apply_plan`] call.
///
/// `queued_operations` lists the operation ids that were newly inserted
/// by this call. `skipped_operations` lists ids that already exist in a
/// terminal `Succeeded` state — the apply path treats those as already-done
/// and moves on.
#[derive(Clone, Debug)]
pub struct ApplyOutcome {
    /// Persisted apply-run row after the call (post-`Running` transition).
    pub run: ArchitectureApplyRun,
    /// Operation ids that this call newly enqueued or that exist in a
    /// non-terminal state from a previous attempt.
    pub queued_operations: Vec<OperationId>,
    /// Operation ids that this call short-circuited because they already
    /// exist in [`OperationStatus::Succeeded`].
    pub skipped_operations: Vec<OperationId>,
}

/// Apply a plan: idempotently enqueue per-change operations and
/// transition the apply run from `Queued` to `Running`.
///
/// See the module docs for the full algorithm. Returns
/// [`ApplyError::MissingConfirmation`] when the plan is destructive and
/// the caller did not pass a matching typed name;
/// [`ApplyError::MissingWarningAck`] when the plan has warnings and the
/// caller did not acknowledge them; [`ApplyError::PlanExpired`] when the
/// plan TTL has elapsed; [`ApplyError::PlanNotApplicable`] when the plan
/// is not in [`PlanStatus::ReadyToApply`]. All other failures bubble up
/// from the store as [`ApplyError::Store`].
#[allow(clippy::too_many_arguments)]
pub async fn apply_plan(
    plan: &Plan,
    plan_record: &ArchitecturePlan,
    ops_repo: &OperationRepository,
    runs_repo: &ApplyRunRepository,
    plan_repo: &PlanRepository,
    topology_repo: &TopologyRepository,
    ctx: &ApplyContext,
    clock: &dyn Clock,
) -> Result<ApplyOutcome, ApplyError> {
    // ── 1. Plan-state guard ───────────────────────────────────────────
    // Accept ReadyToApply (first apply) AND Applying (retry-after-crash).
    // The Phase-5 spec §210 mandates idempotent crash-and-resume; rejecting
    // an Applying plan would break that. Status-mismatch covers everything
    // else (Draft / FailedValidation / RequiresConfirmation / Applied /
    // Failed / Expired / Discarded).
    if !matches!(
        plan_record.status,
        PlanStatus::ReadyToApply | PlanStatus::Applying
    ) {
        return Err(ApplyError::PlanNotApplicable {
            plan_id: ctx.plan_id.to_string(),
            current_status: plan_record.status.as_str().to_string(),
        });
    }

    // ── 2. Expiry guard ───────────────────────────────────────────────
    if is_expired(plan_record, clock) {
        return Err(ApplyError::PlanExpired {
            plan_id: ctx.plan_id.to_string(),
            expires_at: plan_record.expires_at.to_rfc3339(),
        });
    }

    // ── 3. Destructive-plan typed-name confirmation guard ─────────────
    if is_destructive(plan) && !ctx.confirmation.matches(&ctx.topology_name) {
        return Err(ApplyError::MissingConfirmation {
            plan_id: ctx.plan_id.to_string(),
            topology_name: ctx.topology_name.clone(),
        });
    }

    // ── 4. Warning-acknowledgement guard ──────────────────────────────
    if !plan.warnings.is_empty() && !ctx.acknowledged_warnings {
        return Err(ApplyError::MissingWarningAck {
            plan_id: ctx.plan_id.to_string(),
            warnings: plan.warnings.len(),
        });
    }

    // ── 4b. Resource-name sanitization ─────────────────────────────────
    // The idempotency key is `"{plan_id}::{resource_type}/{resource_name}::{action}"`;
    // a `resource_name` containing `::` or `/` would collide with the
    // separators and let two distinct (resource_type, resource_name)
    // pairs produce the same key. Reject up-front so the operations
    // unique index can never get a wrong-but-valid hit.
    for change in &plan.changes {
        if change.resource_name.contains("::") {
            return Err(ApplyError::InvalidResourceName {
                resource_name: change.resource_name.clone(),
                reason: "contains '::' which is reserved as the idempotency-key separator"
                    .to_string(),
            });
        }
        if change.resource_name.contains('/') {
            return Err(ApplyError::InvalidResourceName {
                resource_name: change.resource_name.clone(),
                reason: "contains '/' which is reserved as the resource_ref separator".to_string(),
            });
        }
    }

    // ── 5. Insert apply_run(Queued) ───────────────────────────────────
    //
    // We persist `started_at = clock.now()` at create time so any
    // failure-marker write below (or any subsequent terminal transition
    // by the orchestrator) renders a valid duration in the UI. A run
    // that fails before it ever transitions to Running still has a
    // started_at so `finished_at - started_at` is a sensible number.
    let run_id = ArchitectureApplyRunId::new(chv_common::gen_resource_id())?;
    let started_at = clock.now();
    let run = runs_repo
        .create(ApplyRunCreateInput {
            id: run_id.clone(),
            architecture_id: ctx.architecture_id.clone(),
            architecture_version_id: ctx.architecture_version_id.clone(),
            plan_id: Some(ctx.plan_id.clone()),
            task_id: None,
            status: RunStatus::Queued,
            requested_by: ctx.requested_by.clone(),
            started_at: Some(started_at),
        })
        .await?;

    tracing::info!(
        target: "architecture.apply",
        architecture_id = %ctx.architecture_id,
        version_id = %ctx.architecture_version_id,
        plan_id = %ctx.plan_id,
        run_id = %run_id,
        environment = ctx.environment.as_deref().unwrap_or(""),
        change_count = plan.changes.len(),
        "apply_plan started"
    );

    // ── 5b. Atomic plan status transition: ReadyToApply -> Applying ───
    //
    // This is the TOCTOU guard: if a concurrent discard or apply moved
    // the row, we lose the race and roll back the apply_run we just
    // inserted. The reconcile path stays atomic from the caller's POV.
    //
    // Skip the CAS if the plan is already Applying — that's a legitimate
    // retry-after-crash and we keep the existing claim. The status guard
    // at step 1 already filtered out everything else.
    if plan_record.status == PlanStatus::ReadyToApply {
        let claimed = plan_repo
            .update_status_if_current(&ctx.plan_id, PlanStatus::ReadyToApply, PlanStatus::Applying)
            .await?;
        if !claimed {
            // Roll back our apply_run so the table doesn't accumulate
            // orphan Cancelled rows from racing apply attempts.
            let cancel = runs_repo
                .update(ApplyRunUpdateInput {
                    id: run_id.clone(),
                    status: Some(RunStatus::Cancelled),
                    started_at: Some(started_at),
                    finished_at: Some(clock.now()),
                    task_id: None,
                    result_json: None,
                    logs_ref: None,
                    error_message: Some("plan no longer ReadyToApply".to_string()),
                })
                .await;
            if let Err(cancel_err) = cancel {
                tracing::warn!(
                    target: "architecture.apply",
                    run_id = %run_id,
                    error = %cancel_err,
                    "failed to record apply_run cancellation marker after losing plan-status race"
                );
            }
            // Re-fetch to surface the actual current status — at minimum it is
            // not ReadyToApply, but the precise value (Applying/Discarded/...)
            // helps the UI render a useful message.
            let current = plan_repo
                .get(&ctx.plan_id, None)
                .await
                .map(|p| p.status.as_str().to_string())
                .unwrap_or_else(|_| "unknown".to_string());
            return Err(ApplyError::PlanNotApplicable {
                plan_id: ctx.plan_id.to_string(),
                current_status: current,
            });
        }
    }

    // ── 5c. Topology lifecycle CAS: ${current} -> Applying ──────────────
    //
    // Move the topology row's `status` column to Applying so the dashboard
    // badge reflects an in-flight apply. This is the column that previously
    // sat at `draft` for the topology's lifetime — only set_validation_status
    // wrote to the topology row, and it touched `last_validation_status`,
    // not `status`.
    //
    // We do this AFTER the plan-status claim because that claim is the
    // authoritative race-loser detector for concurrent applies. If the
    // topology CAS itself loses (a concurrent /update bumped the row), we
    // roll the plan back to ReadyToApply so the system is not wedged with
    // a half-claimed apply.
    //
    // Skip when plan_record.status is already Applying — that path is the
    // crash-and-resume retry, where the topology was already moved to
    // Applying on the first attempt and a concurrent edit may have bumped
    // its version_number since.
    if plan_record.status == PlanStatus::ReadyToApply {
        match topology_repo
            .set_lifecycle_status(
                &ctx.architecture_id,
                ArchitectureStatus::Applying,
                ctx.topology_version,
                None,
            )
            .await
        {
            Ok(_) => {
                tracing::info!(
                    target: "architecture.apply",
                    architecture_id = %ctx.architecture_id,
                    from_status = "draft|valid|invalid|planned|applied|drifted|failed",
                    to_status = ArchitectureStatus::Applying.as_str(),
                    topology_version = ctx.topology_version,
                    "topology lifecycle status transitioned to applying"
                );
            }
            Err(err) => {
                // Revert the plan-status claim so the next /apply attempt
                // can re-acquire it cleanly. This is best-effort; if the
                // revert itself fails we still surface the original error
                // because the topology row is the durable source of truth
                // for the dashboard badge.
                let revert = plan_repo
                    .update_status_if_current(
                        &ctx.plan_id,
                        PlanStatus::Applying,
                        PlanStatus::ReadyToApply,
                    )
                    .await;
                if let Err(revert_err) = revert {
                    tracing::warn!(
                        target: "architecture.apply",
                        plan_id = %ctx.plan_id,
                        error = %revert_err,
                        "failed to revert plan status to ReadyToApply after topology lifecycle CAS failure"
                    );
                }
                let cancel = runs_repo
                    .update(ApplyRunUpdateInput {
                        id: run_id.clone(),
                        status: Some(RunStatus::Cancelled),
                        started_at: Some(started_at),
                        finished_at: Some(clock.now()),
                        task_id: None,
                        result_json: None,
                        logs_ref: None,
                        error_message: Some(format!("topology lifecycle CAS failed: {err}")),
                    })
                    .await;
                if let Err(cancel_err) = cancel {
                    tracing::warn!(
                        target: "architecture.apply",
                        run_id = %run_id,
                        error = %cancel_err,
                        "failed to record apply_run cancellation marker after topology CAS failure"
                    );
                }
                tracing::warn!(
                    target: "architecture.apply",
                    architecture_id = %ctx.architecture_id,
                    plan_id = %ctx.plan_id,
                    error = %err,
                    "topology lifecycle CAS to Applying failed; aborting apply"
                );
                return Err(map_topology_cas_err(err, &ctx.architecture_id));
            }
        }
    }

    // ── 6. Per-change idempotent operation enqueue ────────────────────
    let now_ms = clock.now().timestamp_millis();
    let mut queued_operations: Vec<OperationId> = Vec::new();
    let mut skipped_operations: Vec<OperationId> = Vec::new();

    for change in &plan.changes {
        // NoOp changes are not enqueued — they are bookkeeping only.
        if change.action == PlanAction::NoOp {
            continue;
        }

        match enqueue_change(ops_repo, ctx, change, now_ms).await {
            Ok(EnqueueOutcome::Queued(op_id)) => queued_operations.push(op_id),
            Ok(EnqueueOutcome::Skipped(op_id)) => skipped_operations.push(op_id),
            Err(err) => {
                // Best-effort run-failure marker. We bubble up the original
                // error regardless of whether the marker write succeeds.
                // `started_at` is always set (we persisted it at create
                // time) so the UI renders a sensible duration even for
                // runs that failed during enqueue.
                let marker = runs_repo
                    .update(ApplyRunUpdateInput {
                        id: run_id.clone(),
                        status: Some(RunStatus::Failed),
                        started_at: Some(run.started_at.unwrap_or(started_at)),
                        finished_at: Some(clock.now()),
                        task_id: None,
                        result_json: None,
                        logs_ref: None,
                        error_message: Some(format!(
                            "operation enqueue failed for {}/{}: {}",
                            resource_type_as_str(change.resource_type),
                            change.resource_name,
                            err
                        )),
                    })
                    .await;
                if let Err(marker_err) = marker {
                    tracing::warn!(
                        target: "architecture.apply",
                        run_id = %run_id,
                        error = %marker_err,
                        "failed to record apply_run failure marker after enqueue failure"
                    );
                }
                // ── H4: roll the plan row back to Failed ─────────────────
                //
                // Without this rollback the plan stays in `Applying`
                // (claimed by the CAS at step 5b) and the operator can
                // neither retry nor discard it until the 15-minute TTL
                // passes — and even then the row was non-discardable in
                // the legacy BFF handler. Marking the plan `Failed`
                // gives the operator a clean retry path (re-plan,
                // re-apply) AND a clean abandon path (discard).
                //
                // We use a CAS (Applying -> Failed) to avoid clobbering
                // a status the orchestrator may have already advanced
                // out of `Applying` in the unlikely interleaving where
                // an out-of-band actor moved the row. If the CAS is a
                // no-op (`Ok(false)`) we leave the row alone.
                //
                // Failures here are logged but never mask the original
                // store error — the caller needs the real cause to
                // diagnose the underlying enqueue failure.
                let rollback = plan_repo
                    .update_status_if_current(
                        &ctx.plan_id,
                        PlanStatus::Applying,
                        PlanStatus::Failed,
                    )
                    .await;
                match rollback {
                    Ok(true) => tracing::error!(
                        target: "architecture.apply",
                        plan_id = %ctx.plan_id,
                        run_id = %run_id,
                        error = %err,
                        "enqueue failed mid-apply; plan rolled back from Applying to Failed"
                    ),
                    Ok(false) => tracing::warn!(
                        target: "architecture.apply",
                        plan_id = %ctx.plan_id,
                        run_id = %run_id,
                        error = %err,
                        "enqueue failed mid-apply; plan was not in Applying so rollback CAS was a no-op"
                    ),
                    Err(rollback_err) => tracing::error!(
                        target: "architecture.apply",
                        plan_id = %ctx.plan_id,
                        run_id = %run_id,
                        error = %err,
                        rollback_error = %rollback_err,
                        "enqueue failed mid-apply; plan rollback to Failed also errored — plan may be wedged in Applying until TTL"
                    ),
                }
                return Err(err);
            }
        }
    }

    // ── 7. Transition run to Running with task_id = first op id ───────
    let first_op_id = queued_operations
        .first()
        .or_else(|| skipped_operations.first())
        .cloned();

    let updated_run = runs_repo
        .update(ApplyRunUpdateInput {
            id: run_id.clone(),
            status: Some(RunStatus::Running),
            started_at: Some(run.started_at.unwrap_or(started_at)),
            finished_at: None,
            task_id: first_op_id.as_ref().map(|id| id.as_str().to_string()),
            result_json: None,
            logs_ref: None,
            error_message: None,
        })
        .await?;

    tracing::info!(
        target: "architecture.apply",
        architecture_id = %ctx.architecture_id,
        version_id = %ctx.architecture_version_id,
        plan_id = %ctx.plan_id,
        run_id = %run_id,
        queued = queued_operations.len(),
        skipped = skipped_operations.len(),
        "apply_plan completed"
    );

    Ok(ApplyOutcome {
        run: updated_run,
        queued_operations,
        skipped_operations,
    })
}

enum EnqueueOutcome {
    Queued(OperationId),
    Skipped(OperationId),
}

async fn enqueue_change(
    ops_repo: &OperationRepository,
    ctx: &ApplyContext,
    change: &PlanChange,
    now_ms: i64,
) -> Result<EnqueueOutcome, ApplyError> {
    let action_str = plan_action_as_str(change.action);
    let resource_type_str = resource_type_as_str(change.resource_type);
    let resource_ref = format!("{}/{}", resource_type_str, change.resource_name);
    let idempotency_key = format!("{}::{}::{}", ctx.plan_id, resource_ref, action_str);

    let operation_id = OperationId::new(format!("op_{}", chv_common::gen_resource_id()))?;
    let resource_kind = map_resource_kind(change.resource_type);
    let resource_id = ResourceId::new(change.resource_name.clone()).ok();
    // Preserve full architecture-domain provenance in correlation_id even
    // when `resource_id` had to be dropped because the architecture
    // resource_name is not a valid `ResourceId`.
    let correlation_id = format!(
        "plan:{}|arch:{}/{}",
        ctx.plan_id, resource_type_str, change.resource_name
    );

    let receipt = ops_repo
        .create_or_get(&OperationCreateInput {
            operation_id,
            idempotency_key,
            resource_kind,
            resource_id,
            operation_type: action_str.to_string(),
            status: OperationStatus::Pending,
            requested_by: ctx.requested_by.clone(),
            updated_by: ctx.requested_by.clone(),
            desired_generation: None,
            observed_generation: None,
            correlation_id: Some(correlation_id),
            requested_unix_ms: now_ms,
        })
        .await?;

    Ok(if receipt.status == OperationStatus::Succeeded {
        EnqueueOutcome::Skipped(receipt.operation_id)
    } else {
        EnqueueOutcome::Queued(receipt.operation_id)
    })
}

/// A plan is "destructive" when it tears resources down: any `Delete` or
/// `Replace` change, an explicit `Destroy` mode, or a non-zero
/// delete/replace count in the summary.
fn is_destructive(plan: &Plan) -> bool {
    if matches!(
        plan.mode,
        chv_controlplane_types::architecture::PlanMode::Destroy
    ) {
        return true;
    }
    if plan.summary.delete + plan.summary.replace > 0 {
        return true;
    }
    plan.changes
        .iter()
        .any(|c| matches!(c.action, PlanAction::Delete | PlanAction::Replace))
}

/// Map a [`PlanAction`] to the operation_type string the operations table
/// expects. `NoOp` is filtered out before this is called.
fn plan_action_as_str(action: PlanAction) -> &'static str {
    match action {
        PlanAction::Create => "create",
        PlanAction::Update => "update",
        PlanAction::Delete => "delete",
        PlanAction::Replace => "replace",
        PlanAction::NoOp => "no_op",
    }
}

/// Stringify a [`ResourceType`] for resource_ref / correlation_id
/// construction. Used in the idempotency key so the human-readable form
/// is stable across renames of the variant.
fn resource_type_as_str(resource_type: ResourceType) -> &'static str {
    match resource_type {
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

/// Map an architecture-domain [`ResourceType`] to the closed
/// [`ResourceKind`] taxonomy used by the operations table.
///
/// The architecture domain has more variants than the operations
/// taxonomy. Where there is no exact match the closest "logical" kind is
/// chosen (instance → vm, datastore → volume, server → node). Variants
/// that are pure-config (`Project`, `Role`, `User`, `SshKey`,
/// `InstanceUser`, `BackupPolicy`) fall back to [`ResourceKind::Vm`] —
/// the operation_type and correlation_id preserve the precise
/// architecture context for tracing.
fn map_resource_kind(resource_type: ResourceType) -> ResourceKind {
    match resource_type {
        ResourceType::Server | ResourceType::BackupTarget => ResourceKind::Node,
        ResourceType::Datastore => ResourceKind::Volume,
        ResourceType::Network => ResourceKind::Network,
        ResourceType::Image
        | ResourceType::Template
        | ResourceType::Instance
        | ResourceType::SshKey
        | ResourceType::InstanceUser
        | ResourceType::Role
        | ResourceType::User
        | ResourceType::Project
        | ResourceType::BackupPolicy => ResourceKind::Vm,
    }
}

/// Translate a [`StoreError`] from a topology lifecycle CAS into the
/// caller-visible [`ApplyError`]. A `StaleVersion` is reported as a 409
/// `PlanNotApplicable` because the user-visible cause is the same as a
/// concurrent plan-status race ("someone else changed the topology under
/// you, retry"). Anything else flows through the existing `Store` variant.
fn map_topology_cas_err(
    err: StoreError,
    architecture_id: &chv_controlplane_types::architecture::ArchitectureId,
) -> ApplyError {
    match err {
        StoreError::StaleVersion { .. } => ApplyError::PlanNotApplicable {
            plan_id: architecture_id.to_string(),
            current_status: "topology_version_drift".to_string(),
        },
        other => ApplyError::Store(other),
    }
}

/// Transition a topology row's lifecycle `status` column to a terminal
/// state (`Applied`, `Failed`, or `Drifted`) after the orchestrator has
/// resolved an apply run.
///
/// This is the second half of the lifecycle wiring: [`apply_plan`] moves
/// the topology to `Applying` at apply time, and the orchestrator (or
/// any other completion-time signal — drift writer, manual override)
/// calls this helper to land the terminal value.
///
/// Re-reads the current `version_number` so the caller does not need to
/// thread it through. The trade-off vs. a CAS on the stale version the
/// caller saw is:
///
/// - the caller's apply context is stale by definition (the apply path
///   bumped the version when it wrote `Applying`);
/// - re-reading is a single indexed point query;
/// - we accept that an interleaving user-edit between read and write
///   silently overwrites the lifecycle column — that is the desired
///   behaviour for completion writeback (the system-of-record signal
///   wins over a user-edit racing with a finishing run).
///
/// Returns `Ok(())` on success and a structured [`StoreError`] otherwise.
/// The `architecture_id`, `to_status`, and (best-effort) `from_status`
/// are emitted as structured tracing fields per ADR-009.
pub async fn set_topology_terminal_status(
    topology_repo: &TopologyRepository,
    architecture_id: &chv_controlplane_types::architecture::ArchitectureId,
    status: ArchitectureStatus,
) -> Result<(), StoreError> {
    let current = topology_repo.get(architecture_id, None).await?;
    let from_status = current.status.as_str().to_string();
    if current.status == status {
        // Idempotent: already at target. Skip the write so we do not bump
        // version_number for a no-op transition (which would needlessly
        // invalidate any UI cache reading by version).
        tracing::debug!(
            target: "architecture.apply",
            architecture_id = %architecture_id,
            from_status = %from_status,
            to_status = status.as_str(),
            "topology lifecycle already at terminal status; skipping CAS"
        );
        return Ok(());
    }
    topology_repo
        .set_lifecycle_status(architecture_id, status, current.version_number, None)
        .await?;
    tracing::info!(
        target: "architecture.apply",
        architecture_id = %architecture_id,
        from_status = %from_status,
        to_status = status.as_str(),
        topology_version = current.version_number,
        "topology lifecycle status transitioned to terminal state"
    );
    Ok(())
}
