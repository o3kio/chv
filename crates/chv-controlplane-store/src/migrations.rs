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
/// added by migration `0064`.
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
                   reaper_resume_state, interrupted_from_phase, interrupted_at
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
                   reaper_resume_state, interrupted_from_phase, interrupted_at
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
                   reaper_stalled_secs = NULL
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
}
