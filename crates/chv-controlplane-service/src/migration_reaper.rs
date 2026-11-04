//! Migration reaper: background task that detects and fails *stalled* migrations.
//!
//! A migration is not stuck merely because it has been running for a long time:
//! large VMs can legitimately copy for hours. The reaper therefore reaps on
//! **progress liveness** — a non-terminal migration whose `updated_at`
//! (refreshed by every agent progress report and every phase transition) has
//! not moved for the configured stall timeout. A healthy migration that has
//! been running for hours but is still reporting is left alone.
//!
//! Before the reaper resumes a source VM it confirms that the sender is no
//! longer live. Since #656 it first signals a bounded abort through the source
//! *and* destination agents' `MigrationControlService.CancelMigration` RPC
//! (`agent_cancel_pending`), so both detached task halves are stopped rather
//! than left writing/receiving. The RPC is best-effort: if an agent cannot be
//! reached the row stays pending and the next tick retries; a permanent agent
//! refusal is parked with `agent_cancel_last_error`. The resume path is still
//! conservative — the migration must already be terminal (the reaper's own
//! guarded update won), the cancel must be confirmed, no progress may have
//! arrived for at least `max(stall timeout, STALE_ROW_SECS)`, and the source
//! node must be reporting heartbeats. If any check fails the source VM is left
//! paused and an operator warning is logged rather than unpausing a VM
//! underneath a live sender.

use crate::migration::{resolve_agent_socket, SOURCE_NODE_HEARTBEAT_MAX_AGE_SECS, STALE_ROW_SECS};
use crate::node_client_pool::NodeClientPool;
use chv_controlplane_store::{MigrationRepository, PendingAgentCancel, StorePool};
use chv_errors::ChvError;
use std::time::Duration;
use tracing::{info, warn};

/// Default stall timeout: how long a migration may go without any progress
/// update before it is considered stalled. This is deliberately generous
/// enough for multi-hour bulk copies; it is *not* a total-runtime cap.
const DEFAULT_MIGRATION_STALL_TIMEOUT_SECS: i64 = 7200;

/// How often the reaper scans for stalled migrations.
const REAPER_INTERVAL_SECS: u64 = 60;

/// Upper bound on agent cancels attempted per tick. Each attempt is also
/// time-bounded, so one unreachable node cannot stall the reap loop.
const PENDING_CANCEL_BATCH: i64 = 8;

/// Per-call bound for `CancelMigration`. The shared node-client timeout is
/// 30s, which is too long for a best-effort side channel that already has a
/// durable retry queue behind it. The same bound wraps `get_or_connect` so a
/// wedged-but-listening agent socket cannot stall the whole reap loop.
const PENDING_CANCEL_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a confirmed agent cancel must settle before the source VM may be
/// auto-resumed. The agent reports `Completed` through a best-effort pending
/// queue drained on its ~5 s tick, so an immediate resume in the same reaper
/// tick could race a terminal report that would have blocked it.
const AGENT_CANCEL_SETTLE_SECS: i64 = 30;

/// Background task that reaps migrations which have stopped making progress.
pub struct MigrationReaper {
    pool: StorePool,
    interval: Duration,
    stall_timeout_secs: i64,
    node_client_pool: NodeClientPool,
    agent_socket_pattern: String,
    pending_cancel_timeout: Duration,
}

impl MigrationReaper {
    pub fn new(
        pool: StorePool,
        node_client_pool: NodeClientPool,
        agent_socket_pattern: String,
    ) -> Self {
        Self {
            pool,
            interval: Duration::from_secs(REAPER_INTERVAL_SECS),
            stall_timeout_secs: DEFAULT_MIGRATION_STALL_TIMEOUT_SECS,
            node_client_pool,
            agent_socket_pattern,
            pending_cancel_timeout: PENDING_CANCEL_TIMEOUT,
        }
    }

    /// Override the no-progress timeout (seconds). Values below one second are
    /// clamped to one second so the SQL predicate can never mean "now".
    pub fn with_timeout_secs(mut self, stall_timeout_secs: i64) -> Self {
        self.stall_timeout_secs = stall_timeout_secs.max(1);
        self
    }

    /// Override the per-call bound for agent cancels (tests use a short value;
    /// production keeps [`PENDING_CANCEL_TIMEOUT`]).
    pub fn with_pending_cancel_timeout(mut self, timeout: Duration) -> Self {
        self.pending_cancel_timeout = timeout.max(Duration::from_millis(1));
        self
    }

    /// Run the reaper loop until shutdown is signalled.
    pub async fn run(self, mut shutdown_rx: tokio::sync::watch::Receiver<()>) {
        info!(
            "migration reaper starting (interval={}s, stall_timeout={}s)",
            self.interval.as_secs(),
            self.stall_timeout_secs
        );

        let mut interval = tokio::time::interval(self.interval);
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = shutdown_rx.changed() => {
                    info!("migration reaper shutting down");
                    break;
                }
            }
            // Cancel first: a resume must never unpause the source VM while a
            // detached sender may still be streaming. Rows failed later in
            // this tick stay `agent_cancel_pending` and their resume is gated
            // until a subsequent tick confirms the cancel.
            if let Err(e) = self.drain_pending_agent_cancels().await {
                warn!(error = %e, "migration reaper agent-cancel drain failed");
            }
            if let Err(e) = self.reap_stuck_migrations().await {
                warn!(error = %e, "migration reaper tick failed");
            }
        }
    }

    /// Find non-terminal migrations with no progress for the stall timeout and
    /// mark them Failed.
    ///
    /// The predicate uses `updated_at`, not `started_at`: a migration running
    /// for more than the timeout is healthy as long as it keeps reporting.
    async fn reap_stuck_migrations(&self) -> Result<(), ChvError> {
        let stalled: Vec<StuckMigrationRow> = sqlx::query_as(
            r#"
            SELECT
                m.migration_id,
                m.operation_id,
                m.vm_id,
                m.source_node_id,
                m.phase,
                m.started_at,
                m.updated_at AS last_progress_at,
                m.interrupted_from_phase,
                m.interrupted_at,
                CAST(
                    (julianday('now') - julianday(m.started_at)) * 86400
                    AS INTEGER
                ) AS started_age_secs,
                CAST(
                    (julianday('now') - julianday(m.updated_at)) * 86400
                    AS INTEGER
                ) AS stalled_secs
            FROM migrations m
            WHERE m.phase NOT IN ('Completed', 'Failed', 'RolledBack')
              AND m.updated_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ? || ' seconds')
            "#,
        )
        .bind(format!("-{}", self.stall_timeout_secs))
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ChvError::Internal {
            reason: format!("migration reaper: failed to query stalled migrations: {e}"),
        })?;

        let candidates = stalled.len();
        let mut reaped = 0usize;
        for row in &stalled {
            // A long-running migration never reaches this branch while it is
            // reporting; only the absence of progress does. Log both ages so
            // operators can tell a genuine stall from a slow-but-healthy move.
            warn!(
                migration_id = %row.migration_id,
                operation_id = %row.operation_id,
                vm_id = %row.vm_id,
                phase = %row.phase,
                started_at = %row.started_at,
                last_progress_at = %row.last_progress_at,
                started_age_secs = row.started_age_secs,
                stalled_secs = row.stalled_secs,
                stall_timeout_secs = self.stall_timeout_secs,
                "migration reaper: migration stalled (no progress within timeout); force-failing"
            );

            // Guarded update: if a progress report landed between the SELECT
            // and this UPDATE the row's `updated_at` is fresh and the update
            // affects zero rows. Never fail a migration that just progressed.
            let failed = match self.fail_migration(row).await {
                Ok(failed) => failed,
                Err(e) => {
                    warn!(
                        migration_id = %row.migration_id,
                        error = %e,
                        "migration reaper: failed to update migration record"
                    );
                    continue;
                }
            };
            if !failed {
                info!(
                    migration_id = %row.migration_id,
                    "migration reaper: progress arrived before the reap landed; leaving migration running"
                );
                continue;
            }
            reaped += 1;

            // Also fail the parent operation so it doesn't stay in Running forever
            if let Err(e) = self.fail_operation(&row.operation_id).await {
                warn!(
                    operation_id = %row.operation_id,
                    error = %e,
                    "migration reaper: failed to update operation record"
                );
            }
        }

        if reaped > 0 {
            metrics::counter!("migration_reaper_reaped_total").increment(reaped as u64);
            metrics::counter!("migration_reaper_stalled_total").increment(reaped as u64);
            info!(
                count = reaped,
                candidates, "migration reaper: reaped stalled migrations"
            );
        }

        // Resume is a separate, durable pass: the reap is final (the row is
        // terminal and will never be scanned again), so every unresolved
        // resume must be retried on subsequent ticks. Throttling, the
        // quiescence gate, and node liveness are re-evaluated each time.
        self.drain_pending_resumes().await?;

        Ok(())
    }

    /// Signal the source *and* destination agents to abort detached migration
    /// tasks whose control-plane owner is gone (#656).
    ///
    /// A row is queued when restart recovery / a terminal transition / the
    /// reap ends control-plane ownership. `cancelled=false` (`not_tracked`) is
    /// success: the agent has no task, which is exactly what an agent restart
    /// leaves behind. A transport error keeps the row pending, and the attempt
    /// marker rotates it behind untouched rows so a dead node cannot starve
    /// the queue. A permanent refusal (`Unimplemented` for a core-managed or
    /// pre-#656 agent, `InvalidArgument` for a misrouted request) parks the row
    /// with `agent_cancel_last_error` instead of retrying forever.
    async fn drain_pending_agent_cancels(&self) -> Result<(), ChvError> {
        let repo = MigrationRepository::new(self.pool.clone());
        let rows = repo
            .list_pending_agent_cancels(PENDING_CANCEL_BATCH)
            .await
            .map_err(|e| ChvError::Internal {
                reason: format!("migration reaper: failed to list pending agent cancels: {e}"),
            })?;
        if rows.is_empty() {
            return Ok(());
        }

        let mut confirmed = 0usize;
        for row in rows {
            // Count the attempt before contacting any node: connection
            // failures and timeouts must also move this row to the back.
            if let Err(e) = repo.record_agent_cancel_attempt(&row.migration_id).await {
                warn!(
                    migration_id = %row.migration_id,
                    error = %e,
                    "migration reaper: failed to record agent-cancel attempt"
                );
            }

            // Both agents register a detached task under the same
            // `operation_id`; a cancel that only reaches the source leaves the
            // destination receiver (and its allocated port) alive.
            let mut nodes: Vec<&str> = vec![row.source_node_id.as_str()];
            if row.destination_node_id != row.source_node_id {
                nodes.push(row.destination_node_id.as_str());
            }

            let mut retry = false;
            let mut permanent: Option<String> = None;
            for node_id in nodes {
                match self.cancel_on_node(&row, node_id).await {
                    NodeCancelOutcome::Confirmed { cancelled, outcome } => {
                        metrics::counter!(
                            "chv_migration_agent_cancel_total",
                            "outcome" => outcome.clone()
                        )
                        .increment(1);
                        info!(
                            migration_id = %row.migration_id,
                            operation_id = %row.operation_id,
                            node_id = %node_id,
                            attempts = row.agent_cancel_attempts + 1,
                            cancelled,
                            outcome = %outcome,
                            "migration reaper: detached migration task cancel confirmed"
                        );
                    }
                    NodeCancelOutcome::Retry { reason } => {
                        retry = true;
                        metrics::counter!(
                            "chv_migration_agent_cancel_failures_total",
                            "reason" => reason
                        )
                        .increment(1);
                    }
                    NodeCancelOutcome::Permanent { reason } => {
                        permanent = Some(reason);
                    }
                }
            }

            if let Some(reason) = permanent {
                if retry {
                    // At least one endpoint still owes an attempt (it was
                    // unreachable/timed out). Parking now would abandon a
                    // detached task that a later attempt could still cancel.
                    // Keep the refusal observable even though it is not yet
                    // terminal.
                    metrics::counter!(
                        "chv_migration_agent_cancel_failures_total",
                        "reason" => "permanent"
                    )
                    .increment(1);
                    warn!(
                        migration_id = %row.migration_id,
                        operation_id = %row.operation_id,
                        attempts = row.agent_cancel_attempts + 1,
                        reason = %reason,
                        "migration reaper: one agent permanently refused the cancel but another endpoint is retryable; keeping the row pending"
                    );
                } else {
                    // Keep the reason operator-visible, but never claim
                    // success: `agent_cancel_confirmed_at` is deliberately
                    // left NULL.
                    if let Err(e) = repo.abandon_agent_cancel(&row.migration_id, &reason).await {
                        warn!(
                            migration_id = %row.migration_id,
                            error = %e,
                            "migration reaper: failed to park permanently refused agent cancel"
                        );
                    }
                    metrics::counter!(
                        "chv_migration_agent_cancel_failures_total",
                        "reason" => "permanent"
                    )
                    .increment(1);
                    warn!(
                        migration_id = %row.migration_id,
                        operation_id = %row.operation_id,
                        attempts = row.agent_cancel_attempts + 1,
                        reason = %reason,
                        "migration reaper: agent permanently refused the cancel; parking row for operator review"
                    );
                    continue;
                }
            }

            if retry {
                warn!(
                    migration_id = %row.migration_id,
                    operation_id = %row.operation_id,
                    attempts = row.agent_cancel_attempts + 1,
                    "migration reaper: at least one agent could not be signalled; will retry"
                );
                continue;
            }

            if let Err(e) = repo.confirm_agent_cancel(&row.migration_id).await {
                // Both agents aborted the tasks, but we could not persist it.
                // Retrying is harmless (the agents report `not_tracked`);
                // surface the store failure.
                warn!(
                    migration_id = %row.migration_id,
                    error = %e,
                    "migration reaper: agent cancels succeeded but confirmation could not be recorded"
                );
                continue;
            }
            confirmed += 1;
        }

        if confirmed > 0 {
            info!(
                count = confirmed,
                "migration reaper: confirmed detached migration task cancels"
            );
        }
        Ok(())
    }

    /// Signal one endpoint of a migration. Never panics; every failure is
    /// classified as retryable or permanent so the drain stays bounded.
    async fn cancel_on_node(&self, row: &PendingAgentCancel, node_id: &str) -> NodeCancelOutcome {
        let socket = match resolve_agent_socket(&self.agent_socket_pattern, node_id) {
            Ok(socket) => socket,
            Err(e) => {
                warn!(
                    migration_id = %row.migration_id,
                    node_id = %node_id,
                    error = %e,
                    "migration reaper: cannot resolve agent socket for pending cancel"
                );
                return if matches!(e, ChvError::InvalidArgument { .. }) {
                    NodeCancelOutcome::Permanent {
                        reason: "invalid source/destination node id".to_string(),
                    }
                } else {
                    NodeCancelOutcome::Retry {
                        reason: "resolve_error",
                    }
                };
            }
        };

        // The connect handshake is part of the bounded work: a socket that
        // accepts but never completes HTTP/2 must not stall the reap loop.
        let client = match tokio::time::timeout(
            self.pending_cancel_timeout,
            self.node_client_pool.get_or_connect(node_id, &socket),
        )
        .await
        {
            Ok(Ok(client)) => client,
            Ok(Err(e)) => {
                warn!(
                    migration_id = %row.migration_id,
                    node_id = %node_id,
                    attempts = row.agent_cancel_attempts + 1,
                    error = %e,
                    "migration reaper: agent unreachable for pending cancel; will retry"
                );
                return NodeCancelOutcome::Retry {
                    reason: "unreachable",
                };
            }
            Err(_) => {
                warn!(
                    migration_id = %row.migration_id,
                    node_id = %node_id,
                    attempts = row.agent_cancel_attempts + 1,
                    timeout_secs = self.pending_cancel_timeout.as_secs(),
                    "migration reaper: agent connect timed out for pending cancel; will retry"
                );
                return NodeCancelOutcome::Retry {
                    reason: "connect_timeout",
                };
            }
        };

        let mut client = client;
        let result = tokio::time::timeout(
            self.pending_cancel_timeout,
            client.cancel_migration(node_id, &row.operation_id, &row.migration_id),
        )
        .await;

        match result {
            Ok(Ok(response)) => NodeCancelOutcome::Confirmed {
                cancelled: response.cancelled,
                outcome: if response.cancelled {
                    "cancelled".to_string()
                } else {
                    "not_tracked".to_string()
                },
            },
            Ok(Err(ChvError::Unimplemented { .. })) => {
                warn!(
                    migration_id = %row.migration_id,
                    node_id = %node_id,
                    "migration reaper: agent does not implement CancelMigration (core-managed or pre-#656)"
                );
                NodeCancelOutcome::Permanent {
                    reason: "agent does not implement CancelMigration".to_string(),
                }
            }
            Ok(Err(ChvError::InvalidArgument { reason, .. })) => {
                warn!(
                    migration_id = %row.migration_id,
                    node_id = %node_id,
                    reason = %reason,
                    "migration reaper: agent rejected the cancel as invalid"
                );
                NodeCancelOutcome::Permanent {
                    reason: format!("agent rejected cancel: {reason}"),
                }
            }
            Ok(Err(e)) => {
                warn!(
                    migration_id = %row.migration_id,
                    node_id = %node_id,
                    attempts = row.agent_cancel_attempts + 1,
                    error = %e,
                    "migration reaper: agent cancel failed; will retry"
                );
                NodeCancelOutcome::Retry {
                    reason: "rpc_error",
                }
            }
            Err(_) => {
                warn!(
                    migration_id = %row.migration_id,
                    node_id = %node_id,
                    attempts = row.agent_cancel_attempts + 1,
                    timeout_secs = self.pending_cancel_timeout.as_secs(),
                    "migration reaper: agent cancel timed out; will retry"
                );
                NodeCancelOutcome::Retry { reason: "timeout" }
            }
        }
    }

    /// Rows whose source-VM resume has not reached a final state.
    ///
    /// `stalled_secs` is the *effective* silence, not merely the gap measured
    /// when the reaper failed the row. `fail_migration` overwrites
    /// `updated_at` with the reap time, so `reaper_stalled_secs` freezes the
    /// pre-reap progress gap. If the gate only ever compared that frozen value
    /// against `resume_quiescence_secs()` (which has a `STALE_ROW_SECS`
    /// floor), a reaper configured below the floor — e.g. the 60 s test
    /// timeout — would defer the resume forever even though the sender has
    /// been silent for far longer by then. Adding the time elapsed since the
    /// reap makes the effective silence grow monotonically and lets the retry
    /// pass eventually satisfy the floor.
    async fn pending_resumes(&self) -> Result<Vec<StuckMigrationRow>, ChvError> {
        sqlx::query_as(
            r#"
            SELECT
                m.migration_id,
                m.operation_id,
                m.vm_id,
                m.source_node_id,
                m.phase,
                m.started_at,
                m.updated_at AS last_progress_at,
                m.interrupted_from_phase,
                m.interrupted_at,
                CAST(
                    (julianday('now') - julianday(m.started_at)) * 86400
                    AS INTEGER
                ) AS started_age_secs,
                COALESCE(m.reaper_stalled_secs, 0)
                    + CAST(
                        (julianday('now') - julianday(m.updated_at)) * 86400
                        AS INTEGER
                    ) AS stalled_secs
            FROM migrations m
            WHERE m.reaper_resume_state IN ('pending', 'resuming')
            -- Rotate fairly through the backlog: a deferred row's
            -- `updated_at` is frozen at its reap time, so ordering only by
            -- age would starve every newly reaped migration behind the 64
            -- oldest unresolvable rows forever. Attempts increase once per
            -- drained row, which moves selected rows behind untouched ones.
            ORDER BY m.reaper_resume_attempts ASC, m.updated_at ASC
            LIMIT 64
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ChvError::Internal {
            reason: format!("migration reaper: failed to query pending resumes: {e}"),
        })
    }

    /// Retry every source-VM resume that has not reached a final state.
    ///
    /// A `SkippedSenderActive` refusal is not permanent: the quiescence window
    /// may simply not have elapsed yet. Likewise a source node that is not
    /// heartbeating now may report in a minute. Keeping the intent in the
    /// database (rather than the reaper's loop stack) is what prevents a
    /// transient refusal from stranding the VM paused forever.
    async fn drain_pending_resumes(&self) -> Result<(), ChvError> {
        let pending = self.pending_resumes().await?;

        for row in &pending {
            // Claim the row atomically before doing any async work. A late
            // agent-reported `Completed` sets `reaper_resume_state = 'blocked'`
            // concurrently; whichever write wins serialises the decision, and
            // the resume pass will not act on a row it no longer owns.
            // `resuming` is reclaimable: a CP crash (or a failed release write)
            // between claim and release must not strand the row forever, and a
            // late `blocked` still wins the conditional update below.
            let claimed = sqlx::query(
                "UPDATE migrations SET reaper_resume_state = 'resuming' \
                 WHERE migration_id = ? AND reaper_resume_state IN ('pending', 'resuming')",
            )
            .bind(&row.migration_id)
            .execute(&self.pool)
            .await;
            match claimed {
                Ok(result) if result.rows_affected() == 1 => {}
                Ok(_) => continue,
                Err(e) => {
                    warn!(
                        migration_id = %row.migration_id,
                        error = %e,
                        "migration reaper: failed to claim pending resume"
                    );
                    continue;
                }
            }

            if let Err(e) = sqlx::query(
                "UPDATE migrations SET reaper_resume_attempts = reaper_resume_attempts + 1 \
                 WHERE migration_id = ?",
            )
            .bind(&row.migration_id)
            .execute(&self.pool)
            .await
            {
                warn!(
                    migration_id = %row.migration_id,
                    error = %e,
                    "migration reaper: failed to record resume attempt"
                );
            }

            let outcome = self.resume_source_vm(row).await;
            let state = match &outcome {
                Ok(ResumeOutcome::Resumed) => {
                    info!(
                        migration_id = %row.migration_id,
                        vm_id = %row.vm_id,
                        source_node_id = %row.source_node_id,
                        "migration reaper: resumed source VM after migration stall"
                    );
                    Some("resumed")
                }
                Ok(ResumeOutcome::SkippedNotTerminal { .. }) => Some("cancelled"),
                Ok(ResumeOutcome::SkippedMissing) => Some("missing"),
                Ok(outcome @ ResumeOutcome::SkippedResumeBlocked) => {
                    metrics::counter!(
                        "migration_reaper_resume_skipped_total",
                        "reason" => outcome.reason()
                    )
                    .increment(1);
                    warn!(
                        migration_id = %row.migration_id,
                        vm_id = %row.vm_id,
                        source_node_id = %row.source_node_id,
                        "migration reaper: refusing to auto-resume source VM because the row was blocked after it was selected; operator review required"
                    );
                    None
                }
                Ok(outcome @ ResumeOutcome::SkippedInterruptedAmbiguous { .. }) => {
                    metrics::counter!(
                        "migration_reaper_resume_skipped_total",
                        "reason" => outcome.reason()
                    )
                    .increment(1);
                    warn!(
                        migration_id = %row.migration_id,
                        vm_id = %row.vm_id,
                        source_node_id = %row.source_node_id,
                        interrupted_phase = row.interrupted_from_phase.as_deref().unwrap_or("unknown"),
                        outcome = ?outcome,
                        "migration reaper: refusing to auto-resume source VM for an ambiguous restart-interrupted phase; operator review required"
                    );
                    Some("blocked")
                }
                Ok(ResumeOutcome::SkippedAgentCancelRefused { reason }) => {
                    metrics::counter!(
                        "migration_reaper_resume_skipped_total",
                        "reason" => "agent_cancel_refused"
                    )
                    .increment(1);
                    warn!(
                        migration_id = %row.migration_id,
                        vm_id = %row.vm_id,
                        source_node_id = %row.source_node_id,
                        reason = %reason,
                        "migration reaper: refusing to auto-resume source VM because the agent permanently refused the cancel; operator review required"
                    );
                    Some("blocked")
                }
                Ok(
                    outcome @ (ResumeOutcome::SkippedAgentCancelPending
                    | ResumeOutcome::SkippedCancelSettling { .. }
                    | ResumeOutcome::SkippedSenderActive { .. }
                    | ResumeOutcome::SkippedSourceNodeNotReporting { .. }),
                ) => {
                    metrics::counter!(
                        "migration_reaper_resume_skipped_total",
                        "reason" => outcome.reason()
                    )
                    .increment(1);
                    warn!(
                        migration_id = %row.migration_id,
                        vm_id = %row.vm_id,
                        source_node_id = %row.source_node_id,
                        outcome = ?outcome,
                        "migration reaper: deferring source VM resume (sender liveness not yet proven); retrying next tick"
                    );
                    None
                }
                Err(e) => {
                    warn!(
                        migration_id = %row.migration_id,
                        vm_id = %row.vm_id,
                        source_node_id = %row.source_node_id,
                        error = %e,
                        "migration reaper: failed to resume source VM; will retry next tick"
                    );
                    None
                }
            };

            let final_state = state.unwrap_or("pending");
            if let Err(e) = sqlx::query(
                "UPDATE migrations SET reaper_resume_state = ? \
                 WHERE migration_id = ? AND reaper_resume_state = 'resuming'",
            )
            .bind(final_state)
            .bind(&row.migration_id)
            .execute(&self.pool)
            .await
            {
                warn!(
                    migration_id = %row.migration_id,
                    state = final_state,
                    error = %e,
                    "migration reaper: failed to persist resume state"
                );
            }
        }

        Ok(())
    }

    /// Mark a single migration as Failed with a reaper error message.
    ///
    /// Returns `false` when the guarded update did not match because fresh
    /// progress arrived after the candidate scan.
    async fn fail_migration(&self, row: &StuckMigrationRow) -> Result<bool, ChvError> {
        let error_msg = format!(
            "operation reaper: migration stalled (no progress for {}s, started {}s ago, threshold={}s, phase={})",
            row.stalled_secs, row.started_age_secs, self.stall_timeout_secs, row.phase
        );

        // A row that restart recovery observed in `MemoryMigration` or
        // `Paused` must not enter the automatic resume queue, even if it moved
        // to one of those phases after the restart marker was written. Fail
        // closed and let an operator decide.
        let resume_state = if row.interrupted_at.is_some()
            && matches!(row.phase.as_str(), "MemoryMigration" | "Paused")
        {
            "blocked"
        } else {
            "pending"
        };

        let result = sqlx::query(
            r#"UPDATE migrations
               SET phase = 'Failed',
                   error_message = ?,
                   completed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                   updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                   reaper_resume_state = ?,
                   reaper_stalled_secs = ?,
                   agent_cancel_pending = 1
               WHERE migration_id = ?
                 AND phase NOT IN ('Completed', 'Failed', 'RolledBack')
                 AND updated_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ? || ' seconds')"#,
        )
        .bind(&error_msg)
        .bind(resume_state)
        .bind(row.stalled_secs)
        .bind(&row.migration_id)
        .bind(format!("-{}", self.stall_timeout_secs))
        .execute(&self.pool)
        .await
        .map_err(|e| ChvError::Internal {
            reason: format!(
                "migration reaper: failed to fail migration {}: {e}",
                row.migration_id
            ),
        })?;

        Ok(result.rows_affected() > 0)
    }

    /// Mark the parent operation as Failed so it is no longer considered in-progress.
    async fn fail_operation(&self, operation_id: &str) -> Result<(), ChvError> {
        sqlx::query(
            r#"UPDATE operations
               SET status = 'Failed',
                   error_code = 'MIGRATION_STALLED',
                   error_message = 'operation reaper: migration stalled (no progress within timeout)',
                   updated_by = 'migration_reaper',
                   updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                   completed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
               WHERE operation_id = ?
                 AND status NOT IN ('Succeeded', 'Failed', 'Rejected', 'Cancelled', 'Stale', 'Conflict')"#,
        )
        .bind(operation_id)
        .execute(&self.pool)
        .await
        .map_err(|e| ChvError::Internal {
            reason: format!(
                "migration reaper: failed to fail operation {}: {e}",
                operation_id
            ),
        })?;

        Ok(())
    }

    /// Resume the source VM after a stall, but only when sender liveness is
    /// positively excluded.
    ///
    /// Best-effort `CancelMigration` is not enough on its own: the agent may be
    /// unreachable, or the RPC may still be in flight. Instead of unpausing
    /// underneath a possibly-live stream we require all of:
    ///
    /// 1. the migration row is terminal (`Failed`/`RolledBack`) — a raced
    ///    progress report that moved the phase wins and blocks the resume;
    /// 2. the durable `agent_cancel_pending` flag has been cleared, i.e. both
    ///    participating agents confirmed the cancel (or reported no task), and
    ///    no permanent refusal was parked in `agent_cancel_last_error`;
    /// 3. no progress has been observed for at least the stall timeout (and
    ///    never less than `STALE_ROW_SECS`);
    /// 4. the source node is still sending node heartbeats, i.e. the agent is
    ///    alive and reachable but is not reporting migration progress.
    ///
    /// If the source node is unreachable we cannot prove the migration task is
    /// dead, so the source stays paused and an operator must intervene. This is
    /// the fail-safe direction: a paused VM is recoverable; an unpause racing a
    /// live memory sender is not.
    async fn resume_source_vm(&self, row: &StuckMigrationRow) -> Result<ResumeOutcome, ChvError> {
        // 1. Confirm the reaper's transition actually won (or the migration was
        // already rolled back). Never resume a Completed migration: VM state
        // lives on the destination in that case.
        let state: Option<ResumeGateRow> = sqlx::query_as(
            "SELECT phase, agent_cancel_pending, agent_cancel_last_error, \
                        reaper_resume_state, \
                        CAST((julianday('now') - julianday(agent_cancel_confirmed_at)) * 86400 \
                             AS INTEGER) \
                 FROM migrations WHERE migration_id = ?",
        )
        .bind(&row.migration_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| ChvError::Internal {
            reason: format!(
                "reaper: failed to re-check phase for migration {}: {e}",
                row.migration_id
            ),
        })?;
        let (
            phase,
            agent_cancel_pending,
            agent_cancel_last_error,
            resume_state,
            cancel_settle_secs,
        ) = match state {
            Some(state) => state,
            None => return Ok(ResumeOutcome::SkippedMissing),
        };
        // A late agent-reported `Completed` (or another safety rule) may have
        // blocked this row after the resume snapshot was taken. Never resume
        // underneath that decision. `pending` and `resuming` are the states
        // this pass owns; a NULL state is left to the caller's explicit
        // eligibility check.
        if matches!(
            resume_state.as_deref(),
            Some(state) if state != "pending" && state != "resuming"
        ) {
            return Ok(ResumeOutcome::SkippedResumeBlocked);
        }
        match phase.as_str() {
            "Failed" | "RolledBack" => {}
            other => {
                return Ok(ResumeOutcome::SkippedNotTerminal {
                    phase: other.to_string(),
                });
            }
        }

        // 2. Cancel before resume. A detached agent task may still be alive
        // even though its control-plane owner is gone; unpausing the source
        // underneath it would race a live sender. The drain runs before the
        // reap on every tick, so a newly failed row resumes on the next tick
        // at the earliest.
        if agent_cancel_pending != 0 {
            return Ok(ResumeOutcome::SkippedAgentCancelPending);
        }
        // Let the confirmed cancel settle for at least one report interval.
        // The agent may still have a terminal progress report in its pending
        // queue; resuming in the same tick would race it, and a late
        // `Completed` would have blocked the resume.
        if let Some(age) = cancel_settle_secs {
            if age < AGENT_CANCEL_SETTLE_SECS {
                return Ok(ResumeOutcome::SkippedCancelSettling {
                    age_secs: age,
                    required_secs: AGENT_CANCEL_SETTLE_SECS,
                });
            }
        }
        // A permanent refusal clears the pending flag (so the drain stops
        // retrying) but it is *not* a confirmation. The agent may still be
        // running a detached task, so never auto-resume; an operator must
        // inspect the node/VM (docs/OPERATIONS.md).
        if let Some(reason) = agent_cancel_last_error {
            return Ok(ResumeOutcome::SkippedAgentCancelRefused { reason });
        }

        // Restart recovery cannot prove an agent-side task stopped. If it
        // observed an ambiguous phase (`MemoryMigration` speaks for itself;
        // `Paused` may own the only live VM state), never auto-resume even
        // when the reaper's normal silence gate is satisfied. `interrupted_at`
        // is set both for terminalized rows and for rows recovery could only
        // observe; `interrupted_from_phase` preserves the phase.
        if row.interrupted_at.is_some() {
            let interrupted_phase = row
                .interrupted_from_phase
                .as_deref()
                .unwrap_or(row.phase.as_str());
            if matches!(interrupted_phase, "MemoryMigration" | "Paused") {
                return Ok(ResumeOutcome::SkippedInterruptedAmbiguous {
                    phase: interrupted_phase.to_string(),
                });
            }
        }

        // 3. The candidate's pre-failure progress age is the authoritative
        // "sender has been silent for" duration: `fail_migration` just wrote
        // `updated_at = now`, so re-reading that column here would erase the
        // very evidence we need.
        let required_quiescence_secs = self.resume_quiescence_secs();
        if row.stalled_secs < required_quiescence_secs {
            return Ok(ResumeOutcome::SkippedSenderActive {
                last_progress_age_secs: row.stalled_secs,
                required_secs: required_quiescence_secs,
            });
        }

        // 4. Positive evidence that the agent is alive. A stale heartbeat means
        // we cannot tell whether a sender is still running, so refuse.
        let heartbeat_age_secs = self
            .source_node_heartbeat_age_secs(&row.source_node_id)
            .await?;
        match heartbeat_age_secs {
            Some(age) if age <= SOURCE_NODE_HEARTBEAT_MAX_AGE_SECS => {}
            other => {
                return Ok(ResumeOutcome::SkippedSourceNodeNotReporting {
                    heartbeat_age_secs: other,
                });
            }
        }

        // Fetch the current VM generation from the desired state store.
        let generation: Option<(Option<i64>,)> =
            sqlx::query_as("SELECT desired_generation FROM vm_desired_state WHERE vm_id = ?")
                .bind(&row.vm_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| ChvError::Internal {
                    reason: format!(
                        "reaper: failed to fetch generation for VM {}: {e}",
                        row.vm_id
                    ),
                })?;
        let generation = match generation {
            Some((Some(g),)) => g.to_string(),
            _ => "1".to_string(),
        };

        let socket = resolve_agent_socket(&self.agent_socket_pattern, &row.source_node_id)?;
        let mut client = self
            .node_client_pool
            .get_or_connect(&row.source_node_id, &socket)
            .await
            .map_err(|e| ChvError::Internal {
                reason: format!(
                    "reaper: failed to connect to source node {}: {e}",
                    row.source_node_id
                ),
            })?;

        let op_id = format!("reaper-resume-{}", row.migration_id);
        client
            .resume_vm(
                &row.source_node_id,
                &row.vm_id,
                &generation,
                &op_id,
                Some("migration_reaper"),
            )
            .await
            .map_err(|e| ChvError::Internal {
                reason: format!(
                    "reaper: resume_vm failed for vm={} node={}: {e}",
                    row.vm_id, row.source_node_id
                ),
            })?;

        Ok(ResumeOutcome::Resumed)
    }

    /// Minimum no-progress window before a resume is allowed. The configured
    /// stall timeout wins when larger; `STALE_ROW_SECS` is the floor because a
    /// shorter window cannot distinguish silence from a healthy delayed report.
    fn resume_quiescence_secs(&self) -> i64 {
        self.stall_timeout_secs.max(STALE_ROW_SECS)
    }

    /// Age of the source node's last heartbeat. `None` means the node has no
    /// observed-state row (never reported), which is treated as unreachable.
    async fn source_node_heartbeat_age_secs(&self, node_id: &str) -> Result<Option<i64>, ChvError> {
        let last_seen: Option<(Option<String>,)> =
            sqlx::query_as("SELECT last_seen_at FROM node_observed_state WHERE node_id = ?")
                .bind(node_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| ChvError::Internal {
                    reason: format!("reaper: failed to query source node heartbeat: {e}"),
                })?;

        let Some((Some(last_seen),)) = last_seen else {
            return Ok(None);
        };
        let parsed = chrono::NaiveDateTime::parse_from_str(&last_seen, "%Y-%m-%dT%H:%M:%SZ")
            .map_err(|e| ChvError::Internal {
                reason: format!(
                    "reaper: failed to parse source node last_seen_at '{last_seen}': {e}"
                ),
            })?;
        Ok(Some(
            (chrono::Utc::now().naive_utc() - parsed).num_seconds(),
        ))
    }
}

/// Result of signalling one agent endpoint.
enum NodeCancelOutcome {
    /// The agent answered (task aborted, or no task was tracked).
    Confirmed { cancelled: bool, outcome: String },
    /// The agent could not be reached or failed transiently.
    Retry { reason: &'static str },
    /// The agent can never honour this cancel; retrying is pointless.
    Permanent { reason: String },
}

/// Why a source VM was (or was not) resumed after a reap.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ResumeOutcome {
    Resumed,
    /// The migration was no longer terminal by the time we looked — a progress
    /// report raced the reap. Do not touch the VM.
    SkippedNotTerminal {
        phase: String,
    },
    /// A detached agent task may still exist; the durable cancel has not been
    /// confirmed yet.
    SkippedAgentCancelPending,
    /// The cancel is confirmed but has not settled long enough for the agent's
    /// queued terminal report to arrive.
    SkippedCancelSettling {
        age_secs: i64,
        required_secs: i64,
    },
    /// The agent permanently refused the cancel, so confirmation is
    /// impossible. The task may still exist; block for operator review.
    SkippedAgentCancelRefused {
        reason: String,
    },
    /// The sender has not been silent long enough to prove it is gone.
    SkippedSenderActive {
        last_progress_age_secs: i64,
        required_secs: i64,
    },
    /// The source node is not heartbeating, so agent-task liveness cannot be
    /// established. The VM stays paused for operator review.
    SkippedSourceNodeNotReporting {
        heartbeat_age_secs: Option<i64>,
    },
    /// A restart observed this migration in `MemoryMigration` or `Paused`.
    /// Recovery cannot prove the agent-side task stopped, so automatic resume
    /// is blocked permanently for operator review.
    SkippedInterruptedAmbiguous {
        phase: String,
    },
    /// A late agent-reported `Completed` (or another safety rule) marked the
    /// row blocked after this resume attempt was selected; leave it blocked.
    SkippedResumeBlocked,
    /// The migration row disappeared between the scan and the resume.
    SkippedMissing,
}

/// Gate inputs re-read at resume time: `(phase, agent_cancel_pending,
/// agent_cancel_last_error, reaper_resume_state, seconds since the cancel was
/// confirmed)`.
type ResumeGateRow = (String, i64, Option<String>, Option<String>, Option<i64>);

impl ResumeOutcome {
    fn reason(&self) -> &'static str {
        match self {
            Self::Resumed => "resumed",
            Self::SkippedNotTerminal { .. } => "not_terminal",
            Self::SkippedAgentCancelPending => "agent_cancel_pending",
            Self::SkippedCancelSettling { .. } => "agent_cancel_settling",
            Self::SkippedAgentCancelRefused { .. } => "agent_cancel_refused",
            Self::SkippedSenderActive { .. } => "sender_active",
            Self::SkippedSourceNodeNotReporting { .. } => "source_node_not_reporting",
            Self::SkippedInterruptedAmbiguous { .. } => "interrupted_ambiguous",
            Self::SkippedResumeBlocked => "resume_blocked",
            Self::SkippedMissing => "missing",
        }
    }
}

#[derive(sqlx::FromRow)]
struct StuckMigrationRow {
    migration_id: String,
    operation_id: String,
    vm_id: String,
    source_node_id: String,
    phase: String,
    started_at: String,
    last_progress_at: String,
    interrupted_from_phase: Option<String>,
    interrupted_at: Option<String>,
    started_age_secs: i64,
    stalled_secs: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chv_controlplane_store::test_util::TestDb;

    #[test]
    fn test_default_constants() {
        assert_eq!(DEFAULT_MIGRATION_STALL_TIMEOUT_SECS, 7200);
        assert_eq!(REAPER_INTERVAL_SECS, 60);
        assert_eq!(STALE_ROW_SECS, 90);
    }

    fn test_reaper(pool: StorePool, stall_timeout_secs: i64) -> MigrationReaper {
        MigrationReaper::new(
            pool,
            NodeClientPool::new(),
            "/tmp/chv-migration-reaper-test.sock".to_string(),
        )
        .with_timeout_secs(stall_timeout_secs)
    }

    /// Seed the minimum rows needed by the reaper. `started_offset_secs` and
    /// `updated_offset_secs` are relative to `now` (negative = in the past).
    async fn seed_migration(
        pool: &StorePool,
        id: &str,
        phase: &str,
        started_offset_secs: i64,
        updated_offset_secs: i64,
    ) {
        let vm_id = format!("vm-{id}");
        let operation_id = format!("op-{id}");

        sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES (?, ?)")
            .bind(&vm_id)
            .bind(&vm_id)
            .execute(pool)
            .await
            .expect("insert vm");

        sqlx::query(
            r#"INSERT INTO operations
                   (operation_id, idempotency_key, resource_kind, resource_id,
                    operation_type, status, requested_at)
               VALUES (?, ?, 'vm', ?, 'Migrate', 'Running',
                       strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))"#,
        )
        .bind(&operation_id)
        .bind(format!("idem-{id}"))
        .bind(&vm_id)
        .execute(pool)
        .await
        .expect("insert operation");

        sqlx::query(
            r#"INSERT INTO migrations
                   (migration_id, operation_id, vm_id, source_node_id,
                    destination_node_id, phase, started_at, updated_at)
               VALUES (?, ?, ?, 'source-node', 'dest-node', ?,
                       strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ?),
                       strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ?))"#,
        )
        .bind(id)
        .bind(&operation_id)
        .bind(&vm_id)
        .bind(phase)
        .bind(format!("{started_offset_secs} seconds"))
        .bind(format!("{updated_offset_secs} seconds"))
        .execute(pool)
        .await
        .expect("insert migration");
    }

    fn stuck_row(id: &str) -> StuckMigrationRow {
        StuckMigrationRow {
            migration_id: id.to_string(),
            operation_id: format!("op-{id}"),
            vm_id: format!("vm-{id}"),
            source_node_id: "source-node".to_string(),
            phase: "Failed".to_string(),
            started_at: "ignored".to_string(),
            last_progress_at: "ignored".to_string(),
            interrupted_from_phase: None,
            interrupted_at: None,
            started_age_secs: 10_800,
            stalled_secs: 10_800,
        }
    }

    async fn migration_phase(pool: &StorePool, id: &str) -> String {
        sqlx::query_scalar("SELECT phase FROM migrations WHERE migration_id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("read migration phase")
    }

    async fn operation_status(pool: &StorePool, id: &str) -> String {
        sqlx::query_scalar("SELECT status FROM operations WHERE operation_id = ?")
            .bind(format!("op-{id}"))
            .fetch_one(pool)
            .await
            .expect("read operation status")
    }

    /// (a) A migration that has been running for hours but is still reporting
    /// progress must NOT be reaped. This is the regression that the old
    /// `started_at` predicate caused.
    #[tokio::test]
    async fn old_started_at_with_recent_progress_is_not_reaped() {
        let test_db = TestDb::new().await;
        seed_migration(
            &test_db.pool,
            "long-but-healthy",
            "ConvergingDisk",
            -10_800, // started 3 hours ago
            -10,     // progress 10 seconds ago
        )
        .await;

        let reaper = test_reaper(test_db.pool.clone(), 60);
        reaper
            .reap_stuck_migrations()
            .await
            .expect("reaper tick should succeed");

        assert_eq!(
            migration_phase(&test_db.pool, "long-but-healthy").await,
            "ConvergingDisk",
            "a long-running migration with recent progress must not be reaped"
        );
        assert_eq!(
            operation_status(&test_db.pool, "long-but-healthy").await,
            "Running"
        );
    }

    /// (b) A migration with no progress within the stall timeout IS reaped,
    /// regardless of how recently it started.
    #[tokio::test]
    async fn stale_progress_is_reaped() {
        let test_db = TestDb::new().await;
        seed_migration(
            &test_db.pool,
            "stalled",
            "MemoryMigration",
            -10_800,
            -3_600, // no progress for an hour
        )
        .await;

        let reaper = test_reaper(test_db.pool.clone(), 60);
        reaper
            .reap_stuck_migrations()
            .await
            .expect("reaper tick should succeed");

        assert_eq!(migration_phase(&test_db.pool, "stalled").await, "Failed");
        assert_eq!(operation_status(&test_db.pool, "stalled").await, "Failed");
        let error_message: String =
            sqlx::query_scalar("SELECT error_message FROM migrations WHERE migration_id = ?")
                .bind("stalled")
                .fetch_one(&test_db.pool)
                .await
                .expect("read error message");
        assert!(
            error_message.contains("stalled"),
            "reap reason should distinguish a stall from a long-running migration: {error_message}"
        );

        // The resume intent must be durable and retried: the row is terminal
        // now, so a one-shot attempt inside the reap loop would strand the
        // source VM whenever the first attempt is refused.
        let (state, stalled_secs, attempts): (Option<String>, Option<i64>, i64) = sqlx::query_as(
            "SELECT reaper_resume_state, reaper_stalled_secs, reaper_resume_attempts \
             FROM migrations WHERE migration_id = ?",
        )
        .bind("stalled")
        .fetch_one(&test_db.pool)
        .await
        .expect("read resume bookkeeping");
        assert_eq!(state.as_deref(), Some("pending"));
        assert_eq!(stalled_secs, Some(3_600));
        assert!(attempts >= 1, "the reap tick must attempt the resume");

        // The reap ends control-plane ownership, so both detached agent tasks
        // must be queued for cancellation. The test socket is absent, so the
        // drain keeps the row pending rather than claiming a false success.
        let pending: i64 = sqlx::query_scalar(
            "SELECT agent_cancel_pending FROM migrations WHERE migration_id = ?",
        )
        .bind("stalled")
        .fetch_one(&test_db.pool)
        .await
        .expect("read agent cancel pending");
        assert_eq!(pending, 1, "the reaper must queue the agent-side cancel");

        reaper
            .reap_stuck_migrations()
            .await
            .expect("second reaper tick should succeed");
        let (state_after, attempts_after): (Option<String>, i64) = sqlx::query_as(
            "SELECT reaper_resume_state, reaper_resume_attempts FROM migrations WHERE migration_id = ?",
        )
        .bind("stalled")
        .fetch_one(&test_db.pool)
        .await
        .expect("read attempts");
        assert!(
            attempts_after > attempts,
            "an unresolved resume must be retried on the next tick ({attempts} -> {attempts_after}, state={state_after:?})"
        );
    }

    /// Restart reconciliation must not hand any recovered row to the automatic
    /// resume pass. The row is terminal as soon as it is reconciled, so late
    /// progress reports are dropped and the reaper's "sender has been silent"
    /// gate cannot prove the agent-side task stopped. Operator-only recovery is
    /// the fail-safe choice for every interrupted phase.
    #[tokio::test]
    async fn restart_recovery_queues_no_automatic_source_resumes() {
        let test_db = TestDb::new().await;
        seed_migration(
            &test_db.pool,
            "restart-precopy",
            "ConvergingDisk",
            -10_800,
            -10,
        )
        .await;
        seed_migration(
            &test_db.pool,
            "restart-memory",
            "MemoryMigration",
            -10_800,
            -10,
        )
        .await;

        crate::migration::MigrationRegistry::new(test_db.pool.clone())
            .recover_after_restart()
            .await
            .expect("restart recovery should succeed");

        assert_eq!(
            migration_phase(&test_db.pool, "restart-precopy").await,
            "RolledBack"
        );
        assert_eq!(
            migration_phase(&test_db.pool, "restart-memory").await,
            "Failed"
        );

        for id in ["restart-precopy", "restart-memory"] {
            let resume_state: Option<String> = sqlx::query_scalar(
                "SELECT reaper_resume_state FROM migrations WHERE migration_id = ?",
            )
            .bind(id)
            .fetch_one(&test_db.pool)
            .await
            .expect("read resume state");
            assert_eq!(resume_state, None, "{id} must stay operator-only");
        }

        let reaper = test_reaper(test_db.pool.clone(), 60);
        let pending = reaper
            .pending_resumes()
            .await
            .expect("query pending resumes");
        assert!(
            pending.is_empty(),
            "restart-recovered rows must never be auto-resumed"
        );
    }

    /// An `unresolved` row that recovery marked and that later stalls in an
    /// ambiguous phase must be failed without entering the resume queue.
    #[tokio::test]
    async fn stalled_interrupted_memory_phase_is_failed_without_resume_intent() {
        let test_db = TestDb::new().await;
        seed_migration(
            &test_db.pool,
            "stalled-memory",
            "MemoryMigration",
            -10_800,
            -600,
        )
        .await;
        sqlx::query(
            r#"UPDATE migrations
               SET interrupted_from_phase = 'ConvergingDisk',
                   interrupted_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
               WHERE migration_id = 'stalled-memory'"#,
        )
        .execute(&test_db.pool)
        .await
        .expect("mark restart observation");

        let reaper = test_reaper(test_db.pool.clone(), 60);
        reaper
            .reap_stuck_migrations()
            .await
            .expect("reap stalled migration");

        let (phase, resume_state): (String, Option<String>) = sqlx::query_as(
            "SELECT phase, reaper_resume_state FROM migrations \
             WHERE migration_id = 'stalled-memory'",
        )
        .fetch_one(&test_db.pool)
        .await
        .expect("read migration");
        assert_eq!(phase, "Failed");
        assert_eq!(
            resume_state.as_deref(),
            Some("blocked"),
            "a row that reached an ambiguous phase after a restart must not auto-resume"
        );
    }

    /// Defense in depth for rows that still carry a resume intent after a
    /// restart observed an ambiguous phase (for example an `unresolved` row
    /// that recovery could only mark): the reaper must fail it but refuse to
    /// resume the source.
    #[tokio::test]
    async fn restart_interrupted_memory_phase_blocks_auto_resume() {
        let test_db = TestDb::new().await;
        seed_migration(
            &test_db.pool,
            "interrupted-memory",
            "MemoryMigration",
            -10_800,
            -10,
        )
        .await;
        sqlx::query(
            r#"UPDATE migrations
               SET phase = 'Failed',
                   interrupted_from_phase = 'MemoryMigration',
                   interrupted_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                   reaper_resume_state = 'pending',
                   reaper_stalled_secs = 10000
               WHERE migration_id = 'interrupted-memory'"#,
        )
        .execute(&test_db.pool)
        .await
        .expect("seed interrupted ambiguous migration");

        let reaper = test_reaper(test_db.pool.clone(), 60);
        reaper
            .drain_pending_resumes()
            .await
            .expect("drain pending resumes");

        let (phase, resume_state): (String, Option<String>) = sqlx::query_as(
            "SELECT phase, reaper_resume_state FROM migrations \
             WHERE migration_id = 'interrupted-memory'",
        )
        .fetch_one(&test_db.pool)
        .await
        .expect("read migration");
        assert_eq!(phase, "Failed");
        assert_eq!(
            resume_state.as_deref(),
            Some("blocked"),
            "an ambiguous restart-interrupted phase must not be auto-resumed"
        );
    }

    /// A resume whose frozen pre-reap gap is below the quiescence floor must
    /// not defer forever: the effective silence grows with the time elapsed
    /// since the reap, so a later retry tick can satisfy the floor.
    #[tokio::test]
    async fn pending_resume_effective_silence_grows_after_reap() {
        let test_db = TestDb::new().await;
        seed_migration(&test_db.pool, "slow-retry", "MemoryMigration", -10_800, -70).await;

        // 60 s is below STALE_ROW_SECS (90): the frozen pre-reap gap alone
        // can never satisfy the resume gate.
        let reaper = test_reaper(test_db.pool.clone(), 60);
        reaper
            .reap_stuck_migrations()
            .await
            .expect("reaper tick should succeed");

        let frozen: Option<i64> =
            sqlx::query_scalar("SELECT reaper_stalled_secs FROM migrations WHERE migration_id = ?")
                .bind("slow-retry")
                .fetch_one(&test_db.pool)
                .await
                .expect("read frozen stall");
        let frozen = frozen.expect("reap must record the pre-reap gap");
        assert!(
            frozen < reaper.resume_quiescence_secs(),
            "scenario requires a frozen gap below the floor ({frozen} < {})",
            reaper.resume_quiescence_secs()
        );

        let before = reaper
            .pending_resumes()
            .await
            .expect("query pending resumes");
        assert_eq!(before.len(), 1);
        assert!(
            before[0].stalled_secs >= frozen,
            "effective silence starts at the frozen gap ({} >= {frozen})",
            before[0].stalled_secs
        );

        // Simulate 45 s of retry ticks by aging the reap timestamp.
        sqlx::query(
            "UPDATE migrations \
             SET updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-45 seconds') \
             WHERE migration_id = ?",
        )
        .bind("slow-retry")
        .execute(&test_db.pool)
        .await
        .expect("age the reap timestamp");

        let after = reaper
            .pending_resumes()
            .await
            .expect("query pending resumes");
        assert_eq!(after.len(), 1);
        // `julianday('now')` and the second-granularity `updated_at` text do
        // not round identically, so the integer-cast delta can come out one
        // second short of the nominal 45 s (observed 114 instead of 115).
        // The clamp is conservative (it can only under-report silence), so
        // assert growth with a small tolerance instead of exact arithmetic.
        assert!(
            after[0].stalled_secs > before[0].stalled_secs && after[0].stalled_secs >= frozen + 43,
            "effective silence must grow with time since the reap ({} -> {}, frozen {frozen})",
            before[0].stalled_secs,
            after[0].stalled_secs
        );
        assert!(
            after[0].stalled_secs >= reaper.resume_quiescence_secs(),
            "after enough retry time the resume gate must become satisfiable ({} >= {})",
            after[0].stalled_secs,
            reaper.resume_quiescence_secs()
        );
    }

    /// The retry pass must not starve a newly reaped migration behind 64 older
    /// rows whose resumes are permanently unresolvable (dead source node or a
    /// persistently failing RPC). Ordering only by `updated_at` would return
    /// the same oldest 64 rows on every tick.
    #[tokio::test]
    async fn pending_resume_backlog_rotates_through_new_rows() {
        let test_db = TestDb::new().await;

        for i in 0..64 {
            let id = format!("zombie-{i}");
            seed_migration(&test_db.pool, &id, "Failed", -10_800, -600).await;
            sqlx::query(
                "UPDATE migrations SET reaper_resume_state = 'pending', \
                 reaper_stalled_secs = 100, reaper_resume_attempts = 5 \
                 WHERE migration_id = ?",
            )
            .bind(&id)
            .execute(&test_db.pool)
            .await
            .expect("mark zombie pending");
        }

        // Most recent row: an `updated_at`-only ordering would put it last.
        seed_migration(&test_db.pool, "newcomer", "Failed", -10_800, 0).await;
        sqlx::query(
            "UPDATE migrations SET reaper_resume_state = 'pending', reaper_stalled_secs = 100 \
             WHERE migration_id = 'newcomer'",
        )
        .execute(&test_db.pool)
        .await
        .expect("mark newcomer pending");

        let reaper = test_reaper(test_db.pool.clone(), 60);
        let rows = reaper
            .pending_resumes()
            .await
            .expect("query pending resumes");
        assert_eq!(rows.len(), 64);
        assert!(
            rows.iter().any(|row| row.migration_id == "newcomer"),
            "a newly reaped migration must not be starved behind 64 older unresolvable rows"
        );
    }

    /// A crash or a failed release write can leave a row in the in-flight
    /// `resuming` state. The next tick must reclaim it (not strand it forever)
    /// and release it to `pending` when it defers.
    #[tokio::test]
    async fn drain_reclaims_stranded_resuming_rows() {
        let test_db = TestDb::new().await;
        seed_migration(&test_db.pool, "stranded", "Failed", -10_800, -10_800).await;
        sqlx::query(
            "UPDATE migrations SET reaper_resume_state = 'resuming', \
             reaper_stalled_secs = 10_800 WHERE migration_id = 'stranded'",
        )
        .execute(&test_db.pool)
        .await
        .expect("strand the row in resuming");

        let reaper = test_reaper(test_db.pool.clone(), 60);
        reaper
            .drain_pending_resumes()
            .await
            .expect("a stranded row is reclaimable");

        let state: Option<String> = sqlx::query_scalar(
            "SELECT reaper_resume_state FROM migrations WHERE migration_id = 'stranded'",
        )
        .fetch_one(&test_db.pool)
        .await
        .expect("read resume state");
        assert_eq!(
            state.as_deref(),
            Some("pending"),
            "a deferred resume must be released back to pending, not stranded"
        );
    }

    /// A late agent-reported `Completed` that sets `blocked` during the tick
    /// must win over the resume pass's own release write.
    #[tokio::test]
    async fn late_block_wins_over_resume_release_write() {
        let test_db = TestDb::new().await;
        seed_migration(&test_db.pool, "block-race", "Failed", -10_800, -10_800).await;
        sqlx::query(
            "UPDATE migrations SET reaper_resume_state = 'pending', \
             reaper_stalled_secs = 10_800 WHERE migration_id = 'block-race'",
        )
        .execute(&test_db.pool)
        .await
        .expect("mark pending");
        sqlx::query(
            "CREATE TRIGGER block_resume_race \
             BEFORE UPDATE OF reaper_resume_attempts ON migrations \
             WHEN NEW.migration_id = 'block-race' \
             BEGIN \
                 UPDATE migrations SET reaper_resume_state = 'blocked' \
                 WHERE migration_id = 'block-race'; \
             END",
        )
        .execute(&test_db.pool)
        .await
        .expect("install race trigger");

        let reaper = test_reaper(test_db.pool.clone(), 60);
        reaper
            .drain_pending_resumes()
            .await
            .expect("the race is not an error");

        let state: Option<String> = sqlx::query_scalar(
            "SELECT reaper_resume_state FROM migrations WHERE migration_id = 'block-race'",
        )
        .fetch_one(&test_db.pool)
        .await
        .expect("read resume state");
        assert_eq!(
            state.as_deref(),
            Some("blocked"),
            "the concurrent late-Completed block must not be clobbered"
        );
    }

    /// `fail_operation` must use the same terminal set as
    /// `OperationStatus::is_terminal()`: a cancelled operation stays cancelled,
    /// while an active `AwaitingOperatorInput` operation is failed (the
    /// migration it tracks is dead, so waiting for an operator forever is a
    /// stall).
    #[tokio::test]
    async fn fail_operation_matches_operation_status_terminal_set() {
        let test_db = TestDb::new().await;
        for (id, status) in [
            ("cancelled", "Cancelled"),
            ("awaiting", "AwaitingOperatorInput"),
        ] {
            seed_migration(&test_db.pool, id, "ConvergingDisk", -10_800, -3_600).await;
            sqlx::query("UPDATE operations SET status = ? WHERE operation_id = ?")
                .bind(status)
                .bind(format!("op-{id}"))
                .execute(&test_db.pool)
                .await
                .expect("set operation status");
        }

        let reaper = test_reaper(test_db.pool.clone(), 60);
        reaper
            .reap_stuck_migrations()
            .await
            .expect("reaper tick should succeed");

        assert_eq!(
            operation_status(&test_db.pool, "cancelled").await,
            "Cancelled",
            "a terminal Cancelled operation must not be rewritten"
        );
        assert_eq!(
            operation_status(&test_db.pool, "awaiting").await,
            "Failed",
            "an active AwaitingOperatorInput operation must be failed with its migration"
        );
    }

    /// (c) The resume gate must refuse while the sender's last progress is too
    /// recent. If it got past the gate, the bogus socket path would produce a
    /// connection error instead of `SkippedSenderActive`.
    #[tokio::test]
    async fn resume_source_vm_refuses_while_sender_progress_is_fresh() {
        let test_db = TestDb::new().await;
        seed_migration(&test_db.pool, "sender-active", "Failed", -10_800, 0).await;

        let reaper = test_reaper(test_db.pool.clone(), 60);
        let row = StuckMigrationRow {
            migration_id: "sender-active".to_string(),
            operation_id: "op-sender-active".to_string(),
            vm_id: "vm-sender-active".to_string(),
            source_node_id: "source-node".to_string(),
            phase: "Failed".to_string(),
            started_at: "ignored".to_string(),
            last_progress_at: "ignored".to_string(),
            interrupted_from_phase: None,
            interrupted_at: None,
            started_age_secs: 10_800,
            stalled_secs: 5,
        };

        let outcome = reaper
            .resume_source_vm(&row)
            .await
            .expect("a skipped resume is not an error");

        assert_eq!(
            outcome,
            ResumeOutcome::SkippedSenderActive {
                last_progress_age_secs: 5,
                required_secs: 90,
            },
            "the reaper must not resume while the sender may still be streaming"
        );
    }

    /// A confirmed agent cancel must settle for one report interval before the
    /// source may be resumed: the agent can still have a terminal progress
    /// report queued, and a late `Completed` would have blocked the resume.
    #[tokio::test]
    async fn resume_source_vm_refuses_until_agent_cancel_settles() {
        let test_db = TestDb::new().await;
        seed_migration(&test_db.pool, "cancel-settling", "Failed", -10_800, -10_800).await;
        sqlx::query(
            "UPDATE migrations \
             SET agent_cancel_pending = 0, \
                 agent_cancel_confirmed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'), \
                 reaper_resume_state = 'pending', reaper_stalled_secs = 10_800 \
             WHERE migration_id = 'cancel-settling'",
        )
        .execute(&test_db.pool)
        .await
        .expect("confirm cancel just now");

        let reaper = test_reaper(test_db.pool.clone(), 60);
        let row = stuck_row("cancel-settling");
        let outcome = reaper
            .resume_source_vm(&row)
            .await
            .expect("a skipped resume is not an error");
        assert!(
            matches!(
                outcome,
                ResumeOutcome::SkippedCancelSettling {
                    required_secs: 30,
                    ..
                }
            ),
            "a just-confirmed cancel must not be resumed in the same tick: {outcome:?}"
        );
    }

    /// Once the settle window elapses the gate must fall through to the normal
    /// safety checks (it must not block the resume forever).
    #[tokio::test]
    async fn resume_source_vm_proceeds_after_agent_cancel_settles() {
        let test_db = TestDb::new().await;
        seed_migration(&test_db.pool, "cancel-settled", "Failed", -10_800, -10_800).await;
        sqlx::query(
            "UPDATE migrations \
             SET agent_cancel_pending = 0, \
                 agent_cancel_confirmed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-120 seconds'), \
                 reaper_resume_state = 'pending', reaper_stalled_secs = 10_800 \
             WHERE migration_id = 'cancel-settled'",
        )
        .execute(&test_db.pool)
        .await
        .expect("confirm cancel earlier");

        let reaper = test_reaper(test_db.pool.clone(), 60);
        let mut row = stuck_row("cancel-settled");
        row.stalled_secs = 5;
        let outcome = reaper
            .resume_source_vm(&row)
            .await
            .expect("a skipped resume is not an error");
        assert_eq!(
            outcome,
            ResumeOutcome::SkippedSenderActive {
                last_progress_age_secs: 5,
                required_secs: 90,
            },
            "a settled cancel must fall through to the sender-silence gate"
        );
    }

    /// Once the cancel has settled and the sender is silent, a real agent
    /// resume succeeds end to end (`resume_vm` over UDS).
    #[tokio::test]
    async fn resume_source_vm_resumes_after_cancel_settles() {
        use chv_agent_core::connectivity::ConnectivityState;
        use chv_agent_core::state_machine::NodeState;
        use chv_agent_core::{AgentAuth, AgentServer, NodeCache, VmRuntime};
        use chv_agent_runtime_ch::mock::MockCloudHypervisorAdapter;
        use std::os::unix::fs::MetadataExt;

        const TOKEN: &str = "agent-token-0123456789abcdef0123456789";

        let test_db = TestDb::new().await;
        seed_migration(&test_db.pool, "settled-resume", "Failed", -10_800, -10_800).await;
        sqlx::query(
            "INSERT INTO nodes (node_id, hostname, display_name) \
             VALUES ('source-node', 'source-node', 'source-node')",
        )
        .execute(&test_db.pool)
        .await
        .expect("seed source node");
        sqlx::query(
            "INSERT INTO vm_desired_state (vm_id, desired_generation, target_node_id) \
             VALUES ('vm-settled-resume', 1, 'source-node')",
        )
        .execute(&test_db.pool)
        .await
        .expect("seed placement");
        sqlx::query(
            "INSERT INTO node_observed_state \
                 (node_id, observed_generation, observed_state, last_seen_at) \
             VALUES ('source-node', 1, 'TenantReady', \
                     strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))",
        )
        .execute(&test_db.pool)
        .await
        .expect("seed heartbeat");
        sqlx::query(
            "UPDATE migrations \
             SET agent_cancel_pending = 0, \
                 agent_cancel_confirmed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-120 seconds'), \
                 reaper_resume_state = 'pending', reaper_stalled_secs = 10_800 \
             WHERE migration_id = 'settled-resume'",
        )
        .execute(&test_db.pool)
        .await
        .expect("confirm cancel earlier");

        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("source.sock");
        let mut cache = NodeCache::new("source-node");
        cache.node_state = NodeState::TenantReady.as_str().to_string();
        cache.connectivity_state = ConnectivityState::Connected;
        let uid = std::fs::metadata("/proc/self").expect("read own uid").uid();
        let agent = AgentServer::new(
            Arc::new(tokio::sync::Mutex::new(cache)),
            VmRuntime::new(Arc::new(MockCloudHypervisorAdapter::default())),
            dir.path().join("stord.sock"),
            dir.path().join("nwd.sock"),
            None,
            dir.path().join("runtime"),
        )
        .with_agent_auth(AgentAuth::new(TOKEN, false, vec![uid]));
        let serving = agent.clone();
        let serving_socket = socket.clone();
        tokio::spawn(async move {
            let _ = serving.serve(&serving_socket).await;
        });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(socket.exists(), "agent socket was not bound");

        let reaper = MigrationReaper::new(
            test_db.pool.clone(),
            NodeClientPool::new().with_auth_token(TOKEN),
            socket.to_string_lossy().into_owned(),
        );
        let outcome = reaper
            .resume_source_vm(&stuck_row("settled-resume"))
            .await
            .expect("a resume after the settle window succeeds");
        assert_eq!(
            outcome,
            ResumeOutcome::Resumed,
            "a settled, silent, live source node must be resumed"
        );
    }

    /// The resume gate must refuse while the durable agent cancel is still
    /// pending. Otherwise the source VM can be unpaused underneath a detached
    /// sender that has not been signalled yet.
    #[tokio::test]
    async fn resume_source_vm_refuses_while_agent_cancel_is_pending() {
        let test_db = TestDb::new().await;
        seed_migration(&test_db.pool, "cancel-pending", "Failed", -10_800, -10_800).await;
        sqlx::query(
            "UPDATE migrations \
             SET agent_cancel_pending = 1, reaper_resume_state = 'pending', \
                 reaper_stalled_secs = 10_800 \
             WHERE migration_id = 'cancel-pending'",
        )
        .execute(&test_db.pool)
        .await
        .expect("mark cancel pending");

        let reaper = test_reaper(test_db.pool.clone(), 60);
        let row = StuckMigrationRow {
            migration_id: "cancel-pending".to_string(),
            operation_id: "op-cancel-pending".to_string(),
            vm_id: "vm-cancel-pending".to_string(),
            source_node_id: "source-node".to_string(),
            phase: "Failed".to_string(),
            started_at: "ignored".to_string(),
            last_progress_at: "ignored".to_string(),
            interrupted_from_phase: None,
            interrupted_at: None,
            started_age_secs: 10_800,
            stalled_secs: 10_800,
        };

        let outcome = reaper
            .resume_source_vm(&row)
            .await
            .expect("a deferred resume is not an error");

        assert_eq!(outcome, ResumeOutcome::SkippedAgentCancelPending);
    }

    /// A permanently refused cancel clears `agent_cancel_pending` so the drain
    /// stops retrying, but it is not a confirmation: the source VM must stay
    /// paused for operator review.
    #[tokio::test]
    async fn resume_source_vm_refuses_after_permanent_cancel_refusal() {
        let test_db = TestDb::new().await;
        seed_migration(&test_db.pool, "cancel-refused", "Failed", -10_800, -10_800).await;
        sqlx::query(
            "UPDATE migrations \
             SET agent_cancel_pending = 0, \
                 agent_cancel_last_error = 'agent does not implement CancelMigration', \
                 reaper_resume_state = 'pending', reaper_stalled_secs = 10_800 \
             WHERE migration_id = 'cancel-refused'",
        )
        .execute(&test_db.pool)
        .await
        .expect("park refused cancel");

        let reaper = test_reaper(test_db.pool.clone(), 60);
        let row = StuckMigrationRow {
            migration_id: "cancel-refused".to_string(),
            operation_id: "op-cancel-refused".to_string(),
            vm_id: "vm-cancel-refused".to_string(),
            source_node_id: "source-node".to_string(),
            phase: "Failed".to_string(),
            started_at: "ignored".to_string(),
            last_progress_at: "ignored".to_string(),
            interrupted_from_phase: None,
            interrupted_at: None,
            started_age_secs: 10_800,
            stalled_secs: 10_800,
        };

        let outcome = reaper
            .resume_source_vm(&row)
            .await
            .expect("a deferred resume is not an error");

        assert_eq!(
            outcome,
            ResumeOutcome::SkippedAgentCancelRefused {
                reason: "agent does not implement CancelMigration".to_string()
            }
        );
    }

    /// A late agent-reported `Completed` marks the row blocked (the destination
    /// may own the VM). Even if the resume pass already selected it, the
    /// re-read must refuse to resume.
    #[tokio::test]
    async fn resume_source_vm_refuses_after_late_completed_block() {
        let test_db = TestDb::new().await;
        seed_migration(&test_db.pool, "late-complete", "Failed", -10_800, -10_800).await;
        sqlx::query(
            "UPDATE migrations \
             SET reaper_resume_state = 'blocked', reaper_stalled_secs = 10_800 \
             WHERE migration_id = 'late-complete'",
        )
        .execute(&test_db.pool)
        .await
        .expect("block resume");

        let reaper = test_reaper(test_db.pool.clone(), 60);
        let row = StuckMigrationRow {
            migration_id: "late-complete".to_string(),
            operation_id: "op-late-complete".to_string(),
            vm_id: "vm-late-complete".to_string(),
            source_node_id: "source-node".to_string(),
            phase: "Failed".to_string(),
            started_at: "ignored".to_string(),
            last_progress_at: "ignored".to_string(),
            interrupted_from_phase: None,
            interrupted_at: None,
            started_age_secs: 10_800,
            stalled_secs: 10_800,
        };

        let outcome = reaper
            .resume_source_vm(&row)
            .await
            .expect("a blocked resume is not an error");
        assert_eq!(outcome, ResumeOutcome::SkippedResumeBlocked);
    }

    // ---------------------------------------------------------------------
    // #656: detached agent-task cancellation drain
    // ---------------------------------------------------------------------

    use control_plane_node_api::control_plane_node_api as proto;
    use std::sync::{Arc, Mutex};
    use tokio_stream::wrappers::UnixListenerStream;

    #[derive(Default)]
    struct FakeCancelState {
        cancelled: bool,
        unimplemented: bool,
        invalid_argument: bool,
        delay: Option<Duration>,
        expected_token: Option<String>,
        requests: Mutex<Vec<proto::CancelMigrationRequest>>,
    }

    struct FakeCancelAgent {
        state: Arc<FakeCancelState>,
    }

    #[tonic::async_trait]
    impl proto::migration_control_service_server::MigrationControlService for FakeCancelAgent {
        async fn cancel_migration(
            &self,
            req: tonic::Request<proto::CancelMigrationRequest>,
        ) -> Result<tonic::Response<proto::CancelMigrationResponse>, tonic::Status> {
            if let Some(expected) = &self.state.expected_token {
                let got = req
                    .metadata()
                    .get(chv_common::NODE_AUTH_METADATA_KEY)
                    .and_then(|v| v.to_str().ok());
                if got != Some(expected.as_str()) {
                    return Err(tonic::Status::unauthenticated("bad agent token"));
                }
            }
            if self.state.unimplemented {
                return Err(tonic::Status::unimplemented("cancel_migration unsupported"));
            }
            if self.state.invalid_argument {
                return Err(tonic::Status::invalid_argument(
                    "this node is not the migration target",
                ));
            }
            if let Some(delay) = self.state.delay {
                tokio::time::sleep(delay).await;
            }
            let inner = req.into_inner();
            self.state
                .requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(inner);
            let outcome = if self.state.cancelled {
                "cancelled"
            } else {
                "not_tracked"
            };
            Ok(tonic::Response::new(proto::CancelMigrationResponse {
                cancelled: self.state.cancelled,
                outcome: outcome.to_string(),
            }))
        }
    }

    fn fake_state(cancelled: bool) -> Arc<FakeCancelState> {
        Arc::new(FakeCancelState {
            cancelled,
            ..FakeCancelState::default()
        })
    }

    fn spawn_cancel_agent(
        socket: &std::path::Path,
        state: Arc<FakeCancelState>,
    ) -> tokio::task::JoinHandle<()> {
        let listener = tokio::net::UnixListener::bind(socket).expect("bind fake cancel agent");
        let incoming = UnixListenerStream::new(listener);
        tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(
                    proto::migration_control_service_server::MigrationControlServiceServer::new(
                        FakeCancelAgent { state },
                    ),
                )
                .serve_with_incoming(incoming)
                .await;
        })
    }

    fn per_node_pattern(dir: &std::path::Path) -> (String, std::path::PathBuf, std::path::PathBuf) {
        (
            dir.join("{node_id}.sock").to_string_lossy().into_owned(),
            dir.join("source-node.sock"),
            dir.join("dest-node.sock"),
        )
    }

    async fn seed_pending_cancel(pool: &StorePool, id: &str) {
        seed_migration(pool, id, "Failed", -100, -100).await;
        sqlx::query("UPDATE migrations SET agent_cancel_pending = 1 WHERE migration_id = ?")
            .bind(id)
            .execute(pool)
            .await
            .expect("mark agent cancel pending");
    }

    async fn cancel_state(
        pool: &StorePool,
        id: &str,
    ) -> (i64, Option<String>, i64, Option<String>) {
        sqlx::query_as(
            "SELECT agent_cancel_pending, agent_cancel_confirmed_at, agent_cancel_attempts, \
             agent_cancel_last_error FROM migrations WHERE migration_id = ?",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("read agent cancel state")
    }

    fn requests(state: &FakeCancelState) -> Vec<proto::CancelMigrationRequest> {
        state
            .requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Both detached task owners (source and destination) are signalled, with
    /// the exact operation id and node identity the control plane recorded.
    #[tokio::test]
    async fn drain_confirms_both_agents_and_clears_pending_flag() {
        let test_db = TestDb::new().await;
        seed_pending_cancel(&test_db.pool, "orphan").await;

        let dir = tempfile::tempdir().expect("tempdir");
        let (pattern, source_socket, dest_socket) = per_node_pattern(dir.path());
        let source_state = fake_state(true);
        let dest_state = fake_state(true);
        let source_server = spawn_cancel_agent(&source_socket, source_state.clone());
        let dest_server = spawn_cancel_agent(&dest_socket, dest_state.clone());
        let reaper =
            MigrationReaper::new(test_db.pool.clone(), NodeClientPool::new(), pattern.clone());

        reaper
            .drain_pending_agent_cancels()
            .await
            .expect("drain is best-effort");

        for (state, expected_node) in [(&source_state, "source-node"), (&dest_state, "dest-node")] {
            let seen = requests(state);
            assert_eq!(seen.len(), 1, "each endpoint must be signalled once");
            assert_eq!(seen[0].node_id, expected_node);
            assert_eq!(seen[0].operation_id, "op-orphan");
            assert_eq!(seen[0].migration_id, "orphan");
        }

        let (pending, confirmed_at, attempts, last_error) =
            cancel_state(&test_db.pool, "orphan").await;
        assert_eq!(pending, 0, "a confirmed cancel must clear the flag");
        assert!(confirmed_at.is_some());
        assert_eq!(attempts, 1);
        assert!(last_error.is_none());

        // A cleared row is not attempted again.
        reaper
            .drain_pending_agent_cancels()
            .await
            .expect("second drain");
        assert_eq!(cancel_state(&test_db.pool, "orphan").await.2, 1);
        assert_eq!(requests(&source_state).len(), 1);
        assert_eq!(requests(&dest_state).len(), 1);
        source_server.abort();
        dest_server.abort();
    }

    /// `not_tracked` means the agent restarted or the task already finished.
    /// It is terminal for the control plane and must clear the flag.
    #[tokio::test]
    async fn drain_treats_not_tracked_as_success() {
        let test_db = TestDb::new().await;
        seed_pending_cancel(&test_db.pool, "already-gone").await;

        let dir = tempfile::tempdir().expect("tempdir");
        let (pattern, source_socket, dest_socket) = per_node_pattern(dir.path());
        let source_server = spawn_cancel_agent(&source_socket, fake_state(false));
        let dest_server = spawn_cancel_agent(&dest_socket, fake_state(false));
        let reaper = MigrationReaper::new(test_db.pool.clone(), NodeClientPool::new(), pattern);

        reaper
            .drain_pending_agent_cancels()
            .await
            .expect("drain is best-effort");

        let (pending, confirmed_at, attempts, _) =
            cancel_state(&test_db.pool, "already-gone").await;
        assert_eq!(pending, 0);
        assert!(confirmed_at.is_some());
        assert_eq!(attempts, 1);
        source_server.abort();
        dest_server.abort();
    }

    /// An unreachable endpoint keeps the flag set so a later tick retries.
    /// The tick itself still succeeds (best-effort side channel).
    #[tokio::test]
    async fn drain_leaves_unreachable_cancel_pending() {
        let test_db = TestDb::new().await;
        seed_pending_cancel(&test_db.pool, "unreachable").await;

        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("no-such-{node_id}.sock");
        let reaper = MigrationReaper::new(
            test_db.pool.clone(),
            NodeClientPool::new(),
            missing.to_string_lossy().into_owned(),
        );

        reaper
            .drain_pending_agent_cancels()
            .await
            .expect("an unreachable agent is not a reaper error");

        let (pending, confirmed_at, attempts, _) = cancel_state(&test_db.pool, "unreachable").await;
        assert_eq!(pending, 1, "the row must be retried on the next tick");
        assert!(confirmed_at.is_none());
        assert_eq!(attempts, 1);
    }

    /// One reachable endpoint is not enough to clear the flag; the next tick
    /// retries both (the already-cancelled one reports `not_tracked`).
    #[tokio::test]
    async fn drain_retries_both_when_one_endpoint_is_unreachable() {
        let test_db = TestDb::new().await;
        seed_pending_cancel(&test_db.pool, "partial").await;

        let dir = tempfile::tempdir().expect("tempdir");
        let (pattern, source_socket, _dest_socket) = per_node_pattern(dir.path());
        let source_state = fake_state(true);
        let source_server = spawn_cancel_agent(&source_socket, source_state.clone());
        let reaper = MigrationReaper::new(test_db.pool.clone(), NodeClientPool::new(), pattern);

        reaper
            .drain_pending_agent_cancels()
            .await
            .expect("drain is best-effort");
        let (pending, confirmed_at, attempts, _) = cancel_state(&test_db.pool, "partial").await;
        assert_eq!(pending, 1);
        assert!(confirmed_at.is_none());
        assert_eq!(attempts, 1);

        reaper
            .drain_pending_agent_cancels()
            .await
            .expect("second drain");
        assert_eq!(cancel_state(&test_db.pool, "partial").await.2, 2);
        assert_eq!(
            requests(&source_state).len(),
            2,
            "the reachable endpoint is retried after a partial tick"
        );
        assert_eq!(cancel_state(&test_db.pool, "partial").await.0, 1);
        source_server.abort();
    }

    /// A permanent refusal on one endpoint must not short-circuit the other
    /// endpoint's retry: that task may still be cancellable, so the row stays
    /// pending until every endpoint has either confirmed or refused.
    #[tokio::test]
    async fn drain_keeps_pending_when_other_endpoint_is_still_retryable() {
        let test_db = TestDb::new().await;
        seed_pending_cancel(&test_db.pool, "partial-permanent").await;

        let dir = tempfile::tempdir().expect("tempdir");
        let (pattern, source_socket, _dest_socket) = per_node_pattern(dir.path());
        let source_state = Arc::new(FakeCancelState {
            unimplemented: true,
            ..FakeCancelState::default()
        });
        let source_server = spawn_cancel_agent(&source_socket, source_state);
        let reaper = MigrationReaper::new(test_db.pool.clone(), NodeClientPool::new(), pattern);

        reaper
            .drain_pending_agent_cancels()
            .await
            .expect("drain is best-effort");

        let (pending, confirmed_at, _attempts, last_error) =
            cancel_state(&test_db.pool, "partial-permanent").await;
        assert_eq!(
            pending, 1,
            "a retryable endpoint still owes an attempt; the row must stay queued"
        );
        assert!(confirmed_at.is_none());
        assert!(
            last_error.is_none(),
            "the row must not be parked while an endpoint can still be retried"
        );
        source_server.abort();
    }

    /// A permanent refusal (`Unimplemented`: core-managed or pre-#656 agent)
    /// must not retry forever or masquerade as a confirmed cancel.
    #[tokio::test]
    async fn drain_parks_permanently_refused_cancel() {
        let test_db = TestDb::new().await;
        seed_pending_cancel(&test_db.pool, "unsupported").await;

        let dir = tempfile::tempdir().expect("tempdir");
        let (pattern, source_socket, dest_socket) = per_node_pattern(dir.path());
        let source_state = Arc::new(FakeCancelState {
            unimplemented: true,
            ..FakeCancelState::default()
        });
        let dest_state = Arc::new(FakeCancelState {
            unimplemented: true,
            ..FakeCancelState::default()
        });
        let source_server = spawn_cancel_agent(&source_socket, source_state.clone());
        let dest_server = spawn_cancel_agent(&dest_socket, dest_state.clone());
        let reaper =
            MigrationReaper::new(test_db.pool.clone(), NodeClientPool::new(), pattern.clone());

        reaper
            .drain_pending_agent_cancels()
            .await
            .expect("a refusal is not a drain error");

        let (pending, confirmed_at, attempts, last_error) =
            cancel_state(&test_db.pool, "unsupported").await;
        assert_eq!(pending, 0, "a permanent refusal must be parked");
        assert!(confirmed_at.is_none(), "refusal is not a cancellation");
        assert_eq!(attempts, 1);
        assert!(last_error
            .as_deref()
            .is_some_and(|e| e.contains("does not implement")));

        // No further attempts.
        reaper
            .drain_pending_agent_cancels()
            .await
            .expect("second drain");
        assert_eq!(cancel_state(&test_db.pool, "unsupported").await.2, 1);
        source_server.abort();
        dest_server.abort();
    }

    /// `InvalidArgument` is permanent too: a misrouted or malformed cancel
    /// must be parked, not retried forever (the same safety as `Unimplemented`).
    #[tokio::test]
    async fn drain_parks_invalid_argument_refusal() {
        let test_db = TestDb::new().await;
        seed_pending_cancel(&test_db.pool, "misrouted").await;

        let dir = tempfile::tempdir().expect("tempdir");
        let (pattern, source_socket, dest_socket) = per_node_pattern(dir.path());
        let source_state = Arc::new(FakeCancelState {
            invalid_argument: true,
            ..FakeCancelState::default()
        });
        let dest_state = Arc::new(FakeCancelState {
            invalid_argument: true,
            ..FakeCancelState::default()
        });
        let source_server = spawn_cancel_agent(&source_socket, source_state);
        let dest_server = spawn_cancel_agent(&dest_socket, dest_state);
        let reaper = MigrationReaper::new(test_db.pool.clone(), NodeClientPool::new(), pattern);

        reaper
            .drain_pending_agent_cancels()
            .await
            .expect("drain is best-effort");

        let (pending, confirmed_at, attempts, last_error) =
            cancel_state(&test_db.pool, "misrouted").await;
        assert_eq!(pending, 0, "a permanent refusal must be parked");
        assert!(confirmed_at.is_none(), "refusal is not a cancellation");
        assert_eq!(attempts, 1);
        assert!(last_error
            .as_deref()
            .is_some_and(|e| e.contains("rejected cancel")));

        reaper
            .drain_pending_agent_cancels()
            .await
            .expect("second drain");
        assert_eq!(
            cancel_state(&test_db.pool, "misrouted").await.2,
            1,
            "a parked row must not be retried"
        );
        source_server.abort();
        dest_server.abort();
    }

    /// `NodeClient` must attach the configured agent bearer token to the
    /// cancel RPC; the wrong token is a retryable failure, not a confirmation.
    #[tokio::test]
    async fn drain_attaches_agent_token() {
        let test_db = TestDb::new().await;
        seed_pending_cancel(&test_db.pool, "authed").await;

        let dir = tempfile::tempdir().expect("tempdir");
        let (pattern, source_socket, dest_socket) = per_node_pattern(dir.path());
        let state = Arc::new(FakeCancelState {
            cancelled: true,
            expected_token: Some("agent-token-0123456789abcdef0123456789".to_string()),
            ..FakeCancelState::default()
        });
        let source_server = spawn_cancel_agent(&source_socket, state.clone());
        let dest_server = spawn_cancel_agent(&dest_socket, state.clone());

        let wrong = MigrationReaper::new(
            test_db.pool.clone(),
            NodeClientPool::new().with_auth_token("wrong-token"),
            pattern.clone(),
        );
        wrong
            .drain_pending_agent_cancels()
            .await
            .expect("wrong token is a retryable RPC error");
        assert_eq!(cancel_state(&test_db.pool, "authed").await.0, 1);
        assert!(requests(&state).is_empty());

        let right = MigrationReaper::new(
            test_db.pool.clone(),
            NodeClientPool::new().with_auth_token("agent-token-0123456789abcdef0123456789"),
            pattern,
        );
        right
            .drain_pending_agent_cancels()
            .await
            .expect("correct token is accepted");
        let (pending, confirmed_at, attempts, _) = cancel_state(&test_db.pool, "authed").await;
        assert_eq!(pending, 0);
        assert!(confirmed_at.is_some());
        assert_eq!(attempts, 2);
        assert_eq!(requests(&state).len(), 2);
        source_server.abort();
        dest_server.abort();
    }

    /// A slow agent is bounded by the per-call timeout and retried later.
    #[tokio::test]
    async fn drain_times_out_slow_agent_and_retries() {
        let test_db = TestDb::new().await;
        seed_pending_cancel(&test_db.pool, "slow").await;

        let dir = tempfile::tempdir().expect("tempdir");
        let (pattern, source_socket, dest_socket) = per_node_pattern(dir.path());
        let state = Arc::new(FakeCancelState {
            cancelled: true,
            delay: Some(Duration::from_millis(500)),
            ..FakeCancelState::default()
        });
        let source_server = spawn_cancel_agent(&source_socket, state.clone());
        let dest_server = spawn_cancel_agent(&dest_socket, state);
        let reaper = MigrationReaper::new(test_db.pool.clone(), NodeClientPool::new(), pattern)
            .with_pending_cancel_timeout(Duration::from_millis(50));

        reaper
            .drain_pending_agent_cancels()
            .await
            .expect("a timeout is not a drain error");

        let (pending, confirmed_at, attempts, _) = cancel_state(&test_db.pool, "slow").await;
        assert_eq!(pending, 1);
        assert!(confirmed_at.is_none());
        assert_eq!(attempts, 1);
        source_server.abort();
        dest_server.abort();
    }

    /// A socket that accepts but never completes the HTTP/2 handshake must not
    /// stall the reap loop; the connect itself is bounded.
    #[tokio::test]
    async fn drain_bounds_connect_handshake() {
        let test_db = TestDb::new().await;
        seed_pending_cancel(&test_db.pool, "wedged").await;

        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("wedged.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind wedged socket");
        // Never accept; a client connect/handshake cannot complete.
        let reaper = MigrationReaper::new(
            test_db.pool.clone(),
            NodeClientPool::new(),
            socket.to_string_lossy().into_owned(),
        )
        .with_pending_cancel_timeout(Duration::from_millis(50));

        reaper
            .drain_pending_agent_cancels()
            .await
            .expect("a wedged connect is not a drain error");

        let (pending, _, attempts, _) = cancel_state(&test_db.pool, "wedged").await;
        assert_eq!(pending, 1);
        assert_eq!(attempts, 1);
        drop(listener);
    }

    /// Full composition: the real `NodeClient` (bearer-token metadata,
    /// circuit breaker, timeout) talks to the real `AgentServer` (peer-UID
    /// gate, auth interceptor, migration registry) over UDS.
    #[tokio::test]
    async fn cancel_rpc_composes_with_real_agent_auth_and_registry() {
        use chv_agent_core::connectivity::ConnectivityState;
        use chv_agent_core::state_machine::NodeState;
        use chv_agent_core::{AgentAuth, AgentServer, NodeCache, VmRuntime};
        use chv_agent_runtime_ch::mock::MockCloudHypervisorAdapter;
        use std::os::unix::fs::MetadataExt;
        use tokio_util::sync::CancellationToken;

        const TOKEN: &str = "agent-token-0123456789abcdef0123456789";

        let test_db = TestDb::new().await;
        seed_pending_cancel(&test_db.pool, "e2e").await;
        // One real agent serves both endpoints for this test; the store's row
        // normally names distinct nodes, and self-migration is rejected
        // elsewhere in the control plane.
        sqlx::query(
            "UPDATE migrations SET destination_node_id = source_node_id WHERE migration_id = 'e2e'",
        )
        .execute(&test_db.pool)
        .await
        .expect("collapse endpoints for the single-agent test");

        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("agent.sock");
        let mut cache = NodeCache::new("source-node");
        cache.node_state = NodeState::TenantReady.as_str().to_string();
        cache.connectivity_state = ConnectivityState::Connected;
        let uid = std::fs::metadata("/proc/self").expect("read own uid").uid();
        let agent = AgentServer::new(
            Arc::new(tokio::sync::Mutex::new(cache)),
            VmRuntime::new(Arc::new(MockCloudHypervisorAdapter::default())),
            dir.path().join("stord.sock"),
            dir.path().join("nwd.sock"),
            None,
            dir.path().join("runtime"),
        )
        .with_agent_auth(AgentAuth::new(TOKEN, false, vec![uid]));

        let serving = agent.clone();
        let serving_socket = socket.clone();
        tokio::spawn(async move {
            let _ = serving.serve(&serving_socket).await;
        });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(socket.exists(), "agent socket was not bound");

        let cancel_token = CancellationToken::new();
        let task = tokio::spawn(std::future::pending::<()>());
        agent
            .migration_tasks
            .set_cancel_grace(Duration::from_millis(50));
        agent.migration_tasks.insert(
            "op-e2e".to_string(),
            task.abort_handle(),
            cancel_token.clone(),
        );

        // Model the production completion reaper: the registry entry is
        // removed only after the future resolves. The served RPC waits for
        // that before confirming, so the drain must succeed promptly.
        let registry = agent.migration_tasks.clone();
        let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = task.await;
            registry.remove("op-e2e");
            let _ = finished_tx.send(result);
        });

        let reaper = MigrationReaper::new(
            test_db.pool.clone(),
            NodeClientPool::new().with_auth_token(TOKEN),
            socket.to_string_lossy().into_owned(),
        )
        .with_pending_cancel_timeout(Duration::from_secs(2));

        reaper
            .drain_pending_agent_cancels()
            .await
            .expect("drain with a real agent");

        let (pending, confirmed_at, attempts, last_error) =
            cancel_state(&test_db.pool, "e2e").await;
        assert_eq!(pending, 0, "the real agent must confirm the cancel");
        assert!(confirmed_at.is_some());
        assert_eq!(attempts, 1);
        assert!(last_error.is_none());
        assert!(cancel_token.is_cancelled());
        assert!(finished_rx
            .await
            .expect("completion reaper")
            .expect_err("tracked task must be aborted")
            .is_cancelled());
    }
}
