//! Repository for the control-plane `migrations` table (#656).
//!
//! The table is the durable record of a live migration; the migration state
//! machine itself runs as an in-process future. A control-plane restart
//! therefore leaves any non-terminal row behind with no task attached to it.
//! [`MigrationRepository::reconcile_interrupted`] is the guarded transition
//! that records such a row as interrupted in a way an operator (or the
//! migration reaper) can act on.

use crate::{StoreError, StorePool};

/// A row from the `migrations` table, including the restart-recovery columns
/// added by migration `0065` and the agent-cancel bookkeeping added by `0070`.
#[derive(Debug, Clone, sqlx::FromRow, PartialEq, Eq)]
pub struct MigrationRecord {
    pub migration_id: String,
    pub operation_id: String,
    pub vm_id: String,
    pub source_node_id: String,
    pub destination_node_id: String,
    pub phase: String,
    pub started_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
    pub error_message: Option<String>,
    pub cancel_requested_at: Option<String>,
    pub reaper_resume_state: Option<String>,
    pub interrupted_from_phase: Option<String>,
    pub interrupted_at: Option<String>,
    /// 1 while a detached source-agent migration task still needs to be
    /// signalled through `MigrationControlService.CancelMigration` (#656).
    pub agent_cancel_pending: i64,
    /// When the agent confirmed the task was signalled (or was already gone).
    pub agent_cancel_confirmed_at: Option<String>,
    /// Drain attempts so far (reset when a cleared row is re-marked).
    pub agent_cancel_attempts: i64,
    /// Why a permanently refused cancel was parked (operator-visible).
    pub agent_cancel_last_error: Option<String>,
}

/// A terminal migration row whose detached agent task may still be alive.
#[derive(Debug, Clone, sqlx::FromRow, PartialEq, Eq)]
pub struct PendingAgentCancel {
    pub migration_id: String,
    pub operation_id: String,
    pub source_node_id: String,
    pub destination_node_id: String,
    /// Failed drain attempts so far (reset when a cleared row is re-marked).
    pub agent_cancel_attempts: i64,
}

/// Outcome of one guarded interrupt transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptedMigrationOutcome {
    /// The row was non-terminal when observed and is now terminal.
    Reconciled,
    /// The row is already terminal; nothing was written (never resurrect it).
    AlreadyTerminal,
    /// The row moved to another non-terminal phase (or was refreshed by a late
    /// progress report) between observation and update. The caller may retry
    /// with a fresh record.
    PhaseChanged,
    /// The row disappeared between observation and update.
    Missing,
}

/// Input for a single guarded interrupt transition.
#[derive(Debug, Clone)]
pub struct InterruptedMigrationUpdate {
    pub migration_id: String,
    pub operation_id: String,
    /// The non-terminal phase the caller observed.
    pub observed_phase: String,
    /// The `updated_at` value the caller observed. Guarding on it prevents a
    /// concurrent progress report from being overwritten by the recovery pass.
    pub observed_updated_at: String,
    /// Terminal phase to write: `Failed` or `RolledBack`.
    pub terminal_phase: String,
    /// Operator-facing reason, stored on both the migration and its operation.
    pub error_message: String,
}

#[derive(Clone)]
pub struct MigrationRepository {
    pool: StorePool,
}

impl MigrationRepository {
    pub fn new(pool: StorePool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &StorePool {
        &self.pool
    }

    /// All rows that are not in a terminal phase, oldest first.
    pub async fn list_non_terminal(&self) -> Result<Vec<MigrationRecord>, StoreError> {
        let rows = sqlx::query_as::<_, MigrationRecord>(
            r#"
            SELECT migration_id, operation_id, vm_id, source_node_id,
                   destination_node_id, phase, started_at, updated_at,
                   completed_at, error_message, cancel_requested_at,
                   reaper_resume_state, interrupted_from_phase, interrupted_at,
                   agent_cancel_pending, agent_cancel_confirmed_at,
                   agent_cancel_attempts, agent_cancel_last_error
            FROM migrations
            WHERE phase NOT IN ('Completed', 'Failed', 'RolledBack')
            ORDER BY started_at ASC, migration_id ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn get(&self, migration_id: &str) -> Result<Option<MigrationRecord>, StoreError> {
        let row = sqlx::query_as::<_, MigrationRecord>(
            r#"
            SELECT migration_id, operation_id, vm_id, source_node_id,
                   destination_node_id, phase, started_at, updated_at,
                   completed_at, error_message, cancel_requested_at,
                   reaper_resume_state, interrupted_from_phase, interrupted_at,
                   agent_cancel_pending, agent_cancel_confirmed_at,
                   agent_cancel_attempts, agent_cancel_last_error
            FROM migrations
            WHERE migration_id = ?
            "#,
        )
        .bind(migration_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Record a non-terminal migration as interrupted by a control-plane
    /// restart.
    ///
    /// The update is optimistic: it matches only the exact `migration_id`,
    /// `phase`, and `updated_at` the caller observed, and only while the row is
    /// still non-terminal. A racing progress report or terminal transition
    /// therefore wins and this call reports it instead of overwriting it.
    ///
    /// The parent operation is failed in the same transaction so the operator
    /// does not see a migration Failed while its operation stays Running. A
    /// terminal operation (for example one an operator already cancelled) is
    /// left untouched.
    ///
    /// Restart recovery never queues an automatic source-VM resume. After a
    /// restart the control plane cannot prove the agent-side migration task
    /// stopped: the row is immediately terminal, so any progress the agent
    /// still sends is dropped by the normal progress path. Waiting for
    /// "silence" would therefore be self-fulfilling. The resume bookkeeping is
    /// cleared so the reaper cannot act on a stale intent; an operator must
    /// inspect placement and resume explicitly.
    ///
    /// Terminalizing here also sets `agent_cancel_pending`: the detached
    /// `MigrateVm` task on the source agent may still be alive, and the
    /// control plane no longer has an owner for it. The migration reaper
    /// drains that flag through `CancelMigration` (see
    /// [`MigrationRepository::list_pending_agent_cancels`]).
    pub async fn reconcile_interrupted(
        &self,
        update: &InterruptedMigrationUpdate,
    ) -> Result<InterruptedMigrationOutcome, StoreError> {
        if !matches!(update.terminal_phase.as_str(), "Failed" | "RolledBack") {
            return Err(StoreError::InvalidConfiguration {
                reason: format!(
                    "migration interrupt terminal phase must be Failed or RolledBack, got {:?}",
                    update.terminal_phase
                ),
            });
        }

        let mut tx = self.pool.begin().await?;

        // `updated_at` has one-second resolution. Two writes in the same
        // second therefore share a guard value, and this optimistic check
        // cannot distinguish them; the later one wins as long as both still
        // observe the same phase. Terminal-state protection is unaffected
        // because every UPDATE re-checks `phase NOT IN (...)`.
        let result = sqlx::query(
            r#"
            UPDATE migrations
               SET phase = ?,
                   error_message = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                   completed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                   interrupted_from_phase = ?,
                   interrupted_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                   reaper_resume_state = NULL,
                   reaper_resume_attempts = 0,
                   reaper_stalled_secs = NULL,
                   agent_cancel_pending = 1
             WHERE migration_id = ?
               AND phase = ?
               AND updated_at = ?
               AND phase NOT IN ('Completed', 'Failed', 'RolledBack')
            "#,
        )
        .bind(&update.terminal_phase)
        .bind(&update.error_message)
        .bind(&update.observed_phase)
        .bind(&update.migration_id)
        .bind(&update.observed_phase)
        .bind(&update.observed_updated_at)
        .execute(&mut *tx)
        .await?;

        if result.rows_affected() == 0 {
            let phase: Option<(String,)> =
                sqlx::query_as("SELECT phase FROM migrations WHERE migration_id = ?")
                    .bind(&update.migration_id)
                    .fetch_optional(&mut *tx)
                    .await?;
            tx.commit().await?;
            return Ok(match phase {
                None => InterruptedMigrationOutcome::Missing,
                Some((phase,)) if is_terminal_phase(&phase) => {
                    InterruptedMigrationOutcome::AlreadyTerminal
                }
                Some(_) => InterruptedMigrationOutcome::PhaseChanged,
            });
        }

        sqlx::query(
            r#"
            UPDATE operations
               SET status = 'Failed',
                   error_code = 'MIGRATION_INTERRUPTED',
                   error_message = ?,
                   updated_by = 'migration_registry',
                   updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                   completed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
             WHERE operation_id = ?
               AND status NOT IN ('Succeeded', 'Failed', 'Rejected', 'Cancelled', 'Stale', 'Conflict')
            "#,
        )
        .bind(&update.error_message)
        .bind(&update.operation_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(InterruptedMigrationOutcome::Reconciled)
    }

    /// Mark a still-non-terminal row as observed by restart recovery when the
    /// row could not be terminalized because progress kept racing the scan.
    ///
    /// `phase`, `updated_at`, and the resume bookkeeping are intentionally left
    /// untouched: the migration may still be alive, so recovery only records
    /// what it saw. The reaper consults `interrupted_from_phase` later and
    /// refuses to auto-resume ambiguous (`MemoryMigration`/`Paused`) phases.
    ///
    /// Returns `true` when the observation matched, `false` when the row moved
    /// on again (in which case the caller still reports it unresolved).
    pub async fn record_restart_observed(
        &self,
        migration_id: &str,
        observed_phase: &str,
        observed_updated_at: &str,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            r#"
            UPDATE migrations
               SET interrupted_from_phase = ?,
                   interrupted_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
             WHERE migration_id = ?
               AND phase = ?
               AND updated_at = ?
               AND phase NOT IN ('Completed', 'Failed', 'RolledBack')
            "#,
        )
        .bind(observed_phase)
        .bind(migration_id)
        .bind(observed_phase)
        .bind(observed_updated_at)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Queue an idempotent agent-side cancel for a terminal migration row.
    ///
    /// Only `Failed`/`RolledBack` rows are eligible. A `Completed` migration's
    /// source task has already finished, and a non-terminal row is still owned
    /// by the in-process state machine, which cancels through the cooperative
    /// `cancel_requested_at` path instead. Returns `true` when the row is (now)
    /// marked.
    pub async fn mark_agent_cancel_pending(&self, migration_id: &str) -> Result<bool, StoreError> {
        let result = sqlx::query(
            r#"
            UPDATE migrations
               SET agent_cancel_pending = 1,
                   -- A row that was already queued keeps its retry history; a
                   -- row re-marked after a confirmed cancel starts fresh.
                   agent_cancel_attempts = CASE
                       WHEN agent_cancel_pending = 1 THEN agent_cancel_attempts
                       ELSE 0
                   END,
                   agent_cancel_last_attempt_at = CASE
                       WHEN agent_cancel_pending = 1 THEN agent_cancel_last_attempt_at
                       ELSE NULL
                   END,
                   -- A row re-marked after a previous confirmed or abandoned
                   -- cancel has not been confirmed yet. Leaving the old
                   -- timestamp would make a resume gate treat it as done.
                   agent_cancel_confirmed_at = CASE
                       WHEN agent_cancel_pending = 1 THEN agent_cancel_confirmed_at
                       ELSE NULL
                   END,
                   agent_cancel_last_error = CASE
                       WHEN agent_cancel_pending = 1 THEN agent_cancel_last_error
                       ELSE NULL
                   END
             WHERE migration_id = ?
               AND phase IN ('Failed', 'RolledBack')
            "#,
        )
        .bind(migration_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Terminal rows whose detached agent tasks have not been signalled.
    ///
    /// Both endpoints of the migration are returned: `MigrateVm` registers a
    /// detached task on the source *and* the destination agent, and the
    /// reaper must abort both. The `phase` guard is deliberate — a row that a
    /// zombie in-process task moved back to a non-terminal phase must never
    /// have its live task aborted by this drain.
    ///
    /// Least-recently-attempted first so a persistently unreachable node
    /// cannot starve newer rows; the reaper bounds each tick.
    pub async fn list_pending_agent_cancels(
        &self,
        limit: i64,
    ) -> Result<Vec<PendingAgentCancel>, StoreError> {
        let rows = sqlx::query_as::<_, PendingAgentCancel>(
            r#"
            SELECT migration_id, operation_id, source_node_id, destination_node_id,
                   agent_cancel_attempts
            FROM migrations
            WHERE agent_cancel_pending = 1
              AND phase IN ('Failed', 'RolledBack')
            ORDER BY COALESCE(agent_cancel_last_attempt_at, '') ASC,
                     updated_at ASC,
                     migration_id ASC
            LIMIT ?
            "#,
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Record one drain attempt, whether or not the agents answered.
    ///
    /// The timestamp rotates the row behind untouched pending rows, which is
    /// what makes a bounded batch fair when one node stays unreachable.
    pub async fn record_agent_cancel_attempt(&self, migration_id: &str) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            UPDATE migrations
               SET agent_cancel_attempts = agent_cancel_attempts + 1,
                   agent_cancel_last_attempt_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
             WHERE migration_id = ?
               AND agent_cancel_pending = 1
               AND phase IN ('Failed', 'RolledBack')
            "#,
        )
        .bind(migration_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Clear the pending flag after every participating agent accepted (or no
    /// longer tracks) the cancel.
    ///
    /// A `not_tracked` response also lands here: it means the agent has no
    /// task for this operation (already finished or the agent restarted), so
    /// retrying can never succeed. The phase guard keeps a resurrected
    /// non-terminal row from losing its pending marker silently.
    pub async fn confirm_agent_cancel(&self, migration_id: &str) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            UPDATE migrations
               SET agent_cancel_pending = 0,
                   agent_cancel_confirmed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                   agent_cancel_last_error = NULL
             WHERE migration_id = ?
               AND agent_cancel_pending = 1
               AND phase IN ('Failed', 'RolledBack')
            "#,
        )
        .bind(migration_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Park a cancel that the agent refused permanently.
    ///
    /// `Unimplemented` (core-managed or pre-#656 agent) and `InvalidArgument`
    /// (misrouted node) can never succeed on retry. Clearing the flag stops an
    /// unbounded retry/log/metric loop while `agent_cancel_last_error` keeps
    /// the reason operator-visible; `agent_cancel_confirmed_at` stays NULL so
    /// the row is not misreported as successfully cancelled.
    pub async fn abandon_agent_cancel(
        &self,
        migration_id: &str,
        reason: &str,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            UPDATE migrations
               SET agent_cancel_pending = 0,
                   agent_cancel_last_error = ?
             WHERE migration_id = ?
               AND agent_cancel_pending = 1
               AND phase IN ('Failed', 'RolledBack')
            "#,
        )
        .bind(reason)
        .bind(migration_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

pub fn is_terminal_phase(phase: &str) -> bool {
    matches!(phase, "Completed" | "Failed" | "RolledBack")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TestDb;

    async fn seed_migration(pool: &StorePool, id: &str, phase: &str, updated_at: &str) {
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
                    destination_node_id, phase, updated_at)
               VALUES (?, ?, ?, 'node-a', 'node-b', ?, ?)"#,
        )
        .bind(id)
        .bind(&operation_id)
        .bind(&vm_id)
        .bind(phase)
        .bind(updated_at)
        .execute(pool)
        .await
        .expect("insert migration");
    }

    fn update(
        id: &str,
        observed_phase: &str,
        observed_updated_at: &str,
    ) -> InterruptedMigrationUpdate {
        InterruptedMigrationUpdate {
            migration_id: id.to_string(),
            operation_id: format!("op-{id}"),
            observed_phase: observed_phase.to_string(),
            observed_updated_at: observed_updated_at.to_string(),
            terminal_phase: "RolledBack".to_string(),
            error_message: "control-plane restarted".to_string(),
        }
    }

    /// The guarded transition must never rewrite a terminal row, and must not
    /// overwrite a row that moved (or received fresh progress) after it was
    /// observed.
    #[tokio::test]
    async fn reconcile_interrupted_is_optimistic_and_never_resurrects_terminal() {
        let test_db = TestDb::new().await;
        let repo = MigrationRepository::new(test_db.pool.clone());

        seed_migration(
            &test_db.pool,
            "terminal",
            "Completed",
            "2026-01-01T00:00:00Z",
        )
        .await;
        let outcome = repo
            .reconcile_interrupted(&update("terminal", "Completed", "2026-01-01T00:00:00Z"))
            .await
            .expect("terminal lookup should succeed");
        assert_eq!(outcome, InterruptedMigrationOutcome::AlreadyTerminal);
        let row = repo.get("terminal").await.unwrap().unwrap();
        assert_eq!(row.phase, "Completed");
        assert_eq!(row.interrupted_from_phase, None);
        assert_eq!(row.interrupted_at, None);

        seed_migration(
            &test_db.pool,
            "moved",
            "ConvergingDisk",
            "2026-01-01T00:00:00Z",
        )
        .await;
        let outcome = repo
            .reconcile_interrupted(&update(
                "moved",
                "ConvergingDisk",
                // The row's actual updated_at is one second newer: a progress
                // report won the race.
                "2025-12-31T23:59:59Z",
            ))
            .await
            .expect("stale observation should be classified");
        assert_eq!(outcome, InterruptedMigrationOutcome::PhaseChanged);
        let row = repo.get("moved").await.unwrap().unwrap();
        assert_eq!(row.phase, "ConvergingDisk");
        assert_eq!(row.interrupted_from_phase, None);
    }

    #[tokio::test]
    async fn reconcile_interrupted_records_phase_clears_resume_and_fails_operation() {
        let test_db = TestDb::new().await;
        let repo = MigrationRepository::new(test_db.pool.clone());
        seed_migration(
            &test_db.pool,
            "precopy",
            "PreCopyDisk",
            "2026-01-01T00:00:00Z",
        )
        .await;
        // Even a pre-existing resume intent must not survive recovery: after a
        // restart the reaper cannot prove the agent-side sender stopped.
        sqlx::query(
            r#"UPDATE migrations
               SET reaper_resume_state = 'pending',
                   reaper_resume_attempts = 4,
                   reaper_stalled_secs = 30
               WHERE migration_id = 'precopy'"#,
        )
        .execute(&test_db.pool)
        .await
        .expect("seed stale resume intent");

        let outcome = repo
            .reconcile_interrupted(&update("precopy", "PreCopyDisk", "2026-01-01T00:00:00Z"))
            .await
            .expect("reconcile should succeed");
        assert_eq!(outcome, InterruptedMigrationOutcome::Reconciled);

        let row = repo.get("precopy").await.unwrap().unwrap();
        assert_eq!(row.phase, "RolledBack");
        assert_eq!(row.interrupted_from_phase.as_deref(), Some("PreCopyDisk"));
        assert!(row.interrupted_at.is_some());
        assert_eq!(row.reaper_resume_state, None);
        assert_eq!(
            row.agent_cancel_pending, 1,
            "recovery leaves a detached agent task behind; the reaper must be able to cancel it"
        );

        let (attempts, stalled_secs): (i64, Option<i64>) = sqlx::query_as(
            "SELECT reaper_resume_attempts, reaper_stalled_secs \
             FROM migrations WHERE migration_id = 'precopy'",
        )
        .fetch_one(&test_db.pool)
        .await
        .expect("read resume bookkeeping");
        assert_eq!(attempts, 0);
        assert_eq!(stalled_secs, None);

        let (status, error_code, updated_by): (String, Option<String>, Option<String>) =
            sqlx::query_as(
                "SELECT status, error_code, updated_by FROM operations WHERE operation_id = 'op-precopy'",
            )
            .fetch_one(&test_db.pool)
            .await
            .expect("read operation");
        assert_eq!(status, "Failed");
        assert_eq!(error_code.as_deref(), Some("MIGRATION_INTERRUPTED"));
        assert_eq!(updated_by.as_deref(), Some("migration_registry"));
    }

    /// Recovery may only observe a row it could not terminalize. That marker
    /// must not move the phase or touch resume bookkeeping, so a live agent
    /// task is not disturbed; it only lets the reaper fail safe later.
    #[tokio::test]
    async fn record_restart_observed_marks_without_changing_phase() {
        let test_db = TestDb::new().await;
        let repo = MigrationRepository::new(test_db.pool.clone());
        seed_migration(
            &test_db.pool,
            "unresolved",
            "MemoryMigration",
            "2026-01-01T00:00:00Z",
        )
        .await;
        sqlx::query("UPDATE migrations SET reaper_resume_state = 'pending' WHERE migration_id = 'unresolved'")
            .execute(&test_db.pool)
            .await
            .expect("seed resume state");

        assert!(repo
            .record_restart_observed("unresolved", "MemoryMigration", "2026-01-01T00:00:00Z")
            .await
            .expect("record observation"));
        let row = repo.get("unresolved").await.unwrap().unwrap();
        assert_eq!(row.phase, "MemoryMigration");
        assert_eq!(
            row.interrupted_from_phase.as_deref(),
            Some("MemoryMigration")
        );
        assert!(row.interrupted_at.is_some());
        assert_eq!(row.reaper_resume_state.as_deref(), Some("pending"));

        // A row that moved on after the observation must not be marked stale.
        assert!(!repo
            .record_restart_observed("unresolved", "MemoryMigration", "2025-12-31T23:59:59Z")
            .await
            .expect("stale observation is a no-op"));
    }

    /// Only terminal (non-Completed) rows queue an agent cancel. Draining is
    /// idempotent and confirmation clears the flag permanently.
    #[tokio::test]
    async fn pending_agent_cancel_is_terminal_only_and_drains_once() {
        let test_db = TestDb::new().await;
        let repo = MigrationRepository::new(test_db.pool.clone());
        seed_migration(
            &test_db.pool,
            "running",
            "ConvergingDisk",
            "2026-01-01T00:00:00Z",
        )
        .await;
        seed_migration(&test_db.pool, "failed", "Failed", "2026-01-01T00:00:00Z").await;
        seed_migration(&test_db.pool, "done", "Completed", "2026-01-01T00:00:00Z").await;

        // A row still owned by the in-process state machine (or a successful
        // migration whose task already finished) is never queued.
        assert!(!repo
            .mark_agent_cancel_pending("running")
            .await
            .expect("non-terminal mark"));
        assert!(!repo
            .mark_agent_cancel_pending("done")
            .await
            .expect("completed mark"));
        assert!(repo
            .mark_agent_cancel_pending("failed")
            .await
            .expect("terminal mark"));

        let pending = repo
            .list_pending_agent_cancels(10)
            .await
            .expect("list pending cancels");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].migration_id, "failed");
        assert_eq!(pending[0].operation_id, "op-failed");
        assert_eq!(pending[0].source_node_id, "node-a");
        assert_eq!(pending[0].destination_node_id, "node-b");
        assert_eq!(pending[0].agent_cancel_attempts, 0);

        // Marking twice is harmless and still yields one drain entry.
        assert!(repo
            .mark_agent_cancel_pending("failed")
            .await
            .expect("idempotent mark"));
        assert_eq!(
            repo.list_pending_agent_cancels(10)
                .await
                .expect("list after re-mark")
                .len(),
            1
        );

        // An attempt rotates the row but keeps it pending; a re-mark while
        // pending preserves that retry history.
        repo.record_agent_cancel_attempt("failed")
            .await
            .expect("record attempt");
        assert_eq!(
            repo.list_pending_agent_cancels(10)
                .await
                .expect("list after attempt")[0]
                .agent_cancel_attempts,
            1
        );
        assert!(repo
            .mark_agent_cancel_pending("failed")
            .await
            .expect("re-mark pending row"));
        assert_eq!(
            repo.list_pending_agent_cancels(10)
                .await
                .expect("list after pending re-mark")[0]
                .agent_cancel_attempts,
            1,
            "an already-pending row must keep its retry history"
        );

        repo.confirm_agent_cancel("failed")
            .await
            .expect("confirm cancel");
        assert!(repo
            .list_pending_agent_cancels(10)
            .await
            .expect("list after confirm")
            .is_empty());
        let row = repo.get("failed").await.unwrap().unwrap();
        assert_eq!(row.agent_cancel_pending, 0);
        assert!(row.agent_cancel_confirmed_at.is_some());

        // A later terminal write can re-mark the row. That is a fresh cancel:
        // the old confirmation/attempt history must not leak into it, or a
        // resume gate would think the new task was already signalled.
        assert!(repo
            .mark_agent_cancel_pending("failed")
            .await
            .expect("re-mark after confirmation"));
        let row = repo.get("failed").await.unwrap().unwrap();
        assert_eq!(row.agent_cancel_pending, 1);
        assert!(row.agent_cancel_confirmed_at.is_none());
        assert_eq!(row.agent_cancel_attempts, 0);
    }

    /// The phase guard keeps a resurrected non-terminal row out of the drain,
    /// and the batch/rotation ordering is what prevents starvation.
    #[tokio::test]
    async fn pending_agent_cancel_drain_is_bounded_and_fair() {
        let test_db = TestDb::new().await;
        let repo = MigrationRepository::new(test_db.pool.clone());

        for i in 0..10 {
            let id = format!("row-{i:02}");
            seed_migration(&test_db.pool, &id, "Failed", "2026-01-01T00:00:00Z").await;
            assert!(repo.mark_agent_cancel_pending(&id).await.expect("mark"));
        }
        // A resurrected non-terminal row must not be drained even if its
        // pending bit was left behind by a racing writer.
        seed_migration(
            &test_db.pool,
            "resurrected",
            "ConvergingDisk",
            "2026-01-01T00:00:00Z",
        )
        .await;
        sqlx::query(
            "UPDATE migrations SET agent_cancel_pending = 1 WHERE migration_id = 'resurrected'",
        )
        .execute(&test_db.pool)
        .await
        .expect("force stale pending bit");

        let first = repo
            .list_pending_agent_cancels(8)
            .await
            .expect("bounded list");
        assert_eq!(first.len(), 8, "the batch bound must be honoured");
        assert!(
            first.iter().all(|row| row.migration_id != "resurrected"),
            "non-terminal rows must never be drained"
        );
        let first_id = first[0].migration_id.clone();

        // Attempting a row rotates it behind untouched rows.
        repo.record_agent_cancel_attempt(&first_id)
            .await
            .expect("record attempt");
        let second = repo
            .list_pending_agent_cancels(8)
            .await
            .expect("list after attempt");
        assert_eq!(second.len(), 8);
        assert!(
            second.iter().all(|row| row.migration_id != first_id),
            "an attempted row must rotate behind untouched pending rows"
        );

        // A permanent refusal parks the row with an operator-visible reason
        // and is never misreported as confirmed.
        repo.abandon_agent_cancel("row-00", "agent does not implement CancelMigration")
            .await
            .expect("abandon");
        let (pending, confirmed_at, last_error): (i64, Option<String>, Option<String>) =
            sqlx::query_as(
                "SELECT agent_cancel_pending, agent_cancel_confirmed_at, agent_cancel_last_error \
                 FROM migrations WHERE migration_id = 'row-00'",
            )
            .fetch_one(&test_db.pool)
            .await
            .expect("read abandoned state");
        assert_eq!(pending, 0);
        assert!(confirmed_at.is_none());
        assert_eq!(
            last_error.as_deref(),
            Some("agent does not implement CancelMigration")
        );

        // An operator requeue after abandonment is a fresh cancel: the parked
        // error must clear, otherwise the resume gate would stay blocked even
        // after a retry confirms.
        assert!(repo
            .mark_agent_cancel_pending("row-00")
            .await
            .expect("requeue abandoned row"));
        let row = repo.get("row-00").await.unwrap().unwrap();
        assert_eq!(row.agent_cancel_pending, 1);
        assert!(row.agent_cancel_last_error.is_none());
    }
}
