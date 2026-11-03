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
//! longer live. The control-plane↔node protocol has no abort-migration RPC, so
//! the conservative fallback is used: the migration must already be terminal
//! (the reaper's own guarded update won), no progress may have arrived for at
//! least `max(stall timeout, STALE_ROW_SECS)`, and the source node must be
//! reporting heartbeats (the agent is alive but is not reporting migration
//! progress). If any check fails the source VM is left paused and an operator
//! warning is logged rather than unpausing a VM underneath a live sender.

use crate::migration::{resolve_agent_socket, SOURCE_NODE_HEARTBEAT_MAX_AGE_SECS, STALE_ROW_SECS};
use crate::node_client_pool::NodeClientPool;
use chv_controlplane_store::StorePool;
use chv_errors::ChvError;
use std::time::Duration;
use tracing::{info, warn};

/// Default stall timeout: how long a migration may go without any progress
/// update before it is considered stalled. This is deliberately generous
/// enough for multi-hour bulk copies; it is *not* a total-runtime cap.
const DEFAULT_MIGRATION_STALL_TIMEOUT_SECS: i64 = 7200;

/// How often the reaper scans for stalled migrations.
const REAPER_INTERVAL_SECS: u64 = 60;

/// Background task that reaps migrations which have stopped making progress.
pub struct MigrationReaper {
    pool: StorePool,
    interval: Duration,
    stall_timeout_secs: i64,
    node_client_pool: NodeClientPool,
    agent_socket_pattern: String,
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
        }
    }

    /// Override the no-progress timeout (seconds). Values below one second are
    /// clamped to one second so the SQL predicate can never mean "now".
    pub fn with_timeout_secs(mut self, stall_timeout_secs: i64) -> Self {
        self.stall_timeout_secs = stall_timeout_secs.max(1);
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

        if stalled.is_empty() {
            return Ok(());
        }

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

            // Best-effort: resume the source VM so it doesn't stay permanently
            // paused — but only after proving the sender is quiescent.
            match self.resume_source_vm(row).await {
                Ok(ResumeOutcome::Resumed) => {
                    info!(
                        migration_id = %row.migration_id,
                        vm_id = %row.vm_id,
                        source_node_id = %row.source_node_id,
                        "migration reaper: resumed source VM after migration stall"
                    );
                }
                Ok(outcome) => {
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
                        "migration reaper: refusing to resume source VM (sender liveness not proven); VM may remain paused until an operator intervenes"
                    );
                }
                Err(e) => {
                    warn!(
                        migration_id = %row.migration_id,
                        vm_id = %row.vm_id,
                        source_node_id = %row.source_node_id,
                        error = %e,
                        "migration reaper: failed to resume source VM after timeout — VM may be stuck paused"
                    );
                }
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

        let result = sqlx::query(
            r#"UPDATE migrations
               SET phase = 'Failed',
                   error_message = ?,
                   completed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                   updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
               WHERE migration_id = ?
                 AND phase NOT IN ('Completed', 'Failed', 'RolledBack')
                 AND updated_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ? || ' seconds')"#,
        )
        .bind(&error_msg)
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
                 AND status NOT IN ('Succeeded', 'Failed', 'Rejected', 'Stale', 'Conflict', 'AwaitingOperatorInput')"#,
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
    /// The protocol has no "abort migration and acknowledge" RPC. Instead of
    /// unpausing underneath a possibly-live stream we require all of:
    ///
    /// 1. the migration row is terminal (`Failed`/`RolledBack`) — a raced
    ///    progress report that moved the phase wins and blocks the resume;
    /// 2. no progress has been observed for at least the stall timeout (and
    ///    never less than `STALE_ROW_SECS`);
    /// 3. the source node is still sending node heartbeats, i.e. the agent is
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
        let phase: Option<String> =
            sqlx::query_scalar("SELECT phase FROM migrations WHERE migration_id = ?")
                .bind(&row.migration_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| ChvError::Internal {
                    reason: format!(
                        "reaper: failed to re-check phase for migration {}: {e}",
                        row.migration_id
                    ),
                })?;
        match phase.as_deref() {
            Some("Failed") | Some("RolledBack") => {}
            Some(other) => {
                return Ok(ResumeOutcome::SkippedNotTerminal {
                    phase: other.to_string(),
                });
            }
            None => return Ok(ResumeOutcome::SkippedMissing),
        }

        // 2. The candidate's pre-failure progress age is the authoritative
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

        // 3. Positive evidence that the agent is alive. A stale heartbeat means
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
            sqlx::query_as("SELECT generation FROM vm_desired_state WHERE vm_id = ?")
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

/// Why a source VM was (or was not) resumed after a reap.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ResumeOutcome {
    Resumed,
    /// The migration was no longer terminal by the time we looked — a progress
    /// report raced the reap. Do not touch the VM.
    SkippedNotTerminal {
        phase: String,
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
    /// The migration row disappeared between the scan and the resume.
    SkippedMissing,
}

impl ResumeOutcome {
    fn reason(&self) -> &'static str {
        match self {
            Self::Resumed => "resumed",
            Self::SkippedNotTerminal { .. } => "not_terminal",
            Self::SkippedSenderActive { .. } => "sender_active",
            Self::SkippedSourceNodeNotReporting { .. } => "source_node_not_reporting",
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
}
