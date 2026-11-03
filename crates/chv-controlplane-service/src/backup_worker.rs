use crate::backup_shipper::shipper_from_destination;
use crate::node_client_pool::NodeClientPool;
use chv_controlplane_store::{
    BackupJobCreateInput, BackupJobStatusUpdateInput, BackupRepository, StorePool,
};
use chv_errors::ChvError;
use cron::Schedule;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
use tracing::{error, info, warn};

const MAX_RETRIES: i64 = 3;

/// Hard-fence (#654): no new backup/restore work may be created while the
/// posture is fenced. Existing rows are still drained by the executor so no
/// job is left in a misleading `Pending`/`Running` state; the scheduler stops
/// queueing new jobs even if a pre-fence schedule row is still enabled.
const BACKUP_EXECUTION_FENCED: bool = true;

/// Background worker that schedules backup jobs from cron expressions
/// and executes pending backup jobs against node agents.
pub struct BackupWorker {
    pool: StorePool,
    backup_repo: BackupRepository,
    _agent_socket_pattern: String,
    tick_interval: Duration,
    _node_client_pool: NodeClientPool,
    backup_staging_dir: PathBuf,
}

impl BackupWorker {
    pub fn new(
        pool: StorePool,
        backup_repo: BackupRepository,
        agent_socket_pattern: String,
        node_client_pool: NodeClientPool,
        backup_staging_dir: PathBuf,
    ) -> Self {
        Self {
            pool,
            backup_repo,
            _agent_socket_pattern: agent_socket_pattern,
            tick_interval: Duration::from_secs(30),
            _node_client_pool: node_client_pool,
            backup_staging_dir,
        }
    }

    pub async fn run(self, mut shutdown_rx: tokio::sync::watch::Receiver<()>) {
        info!(staging_dir = %self.backup_staging_dir.display(), "backup worker starting");
        let mut interval = tokio::time::interval(self.tick_interval);
        let mut tick_count: u64 = 0;
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = shutdown_rx.changed() => {
                    info!("backup worker shutting down");
                    break;
                }
            }
            tick_count += 1;
            if let Err(e) = self.tick(tick_count).await {
                warn!(error = %e, "backup worker tick failed");
            }
        }
    }

    async fn tick(&self, tick_count: u64) -> Result<(), ChvError> {
        // Executor runs every tick (30s)
        if let Err(e) = self.run_executor().await {
            warn!(error = %e, "backup executor failed");
        }

        // Scheduler runs every 2nd tick (60s)
        if tick_count.is_multiple_of(2) {
            if let Err(e) = self.run_scheduler().await {
                warn!(error = %e, "backup scheduler failed");
            }
        }

        Ok(())
    }

    async fn run_scheduler(&self) -> Result<(), ChvError> {
        let schedules = self
            .backup_repo
            .list_enabled_schedules()
            .await
            .map_err(|e| ChvError::Internal {
                reason: format!("failed to list enabled backup schedules: {e}"),
            })?;

        let now = chrono::Utc::now();

        for schedule in schedules {
            if let Err(e) = self.process_schedule(&schedule, now).await {
                warn!(
                    schedule_id = %schedule.schedule_id,
                    error = %e,
                    "failed to process backup schedule"
                );
            }
        }

        Ok(())
    }

    async fn process_schedule(
        &self,
        schedule: &chv_controlplane_store::BackupScheduleRow,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), ChvError> {
        let cron =
            Schedule::from_str(&schedule.cron_expression).map_err(|e| ChvError::Internal {
                reason: format!(
                    "invalid cron expression '{}': {e}",
                    schedule.cron_expression
                ),
            })?;

        let last_run = schedule
            .last_run_at
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc));

        let reference_time = last_run.unwrap_or_else(|| {
            schedule
                .created_at
                .parse::<chrono::DateTime<chrono::Utc>>()
                .unwrap_or(now)
        });

        // Find the next occurrence after the reference time
        let next_due = cron.after(&reference_time).next();

        if let Some(due_time) = next_due {
            if due_time <= now {
                // #654 hard-fence: never create new backup work. Checked
                // before the optimistic claim so a pre-fence schedule is not
                // even marked as run; it stays inert for a future
                // implementation to adopt.
                if BACKUP_EXECUTION_FENCED {
                    warn!(
                        schedule_id = %schedule.schedule_id,
                        "backup/restore is hard-fenced; skipping scheduled job creation"
                    );
                    return Ok(());
                }

                // Optimistic locking: claim the schedule run atomically before creating the job.
                // This prevents duplicate job creation when multiple backup workers run concurrently.
                let now_str = now.to_rfc3339();
                let claimed = self
                    .backup_repo
                    .try_claim_schedule_run(
                        &schedule.schedule_id,
                        schedule.last_run_at.as_deref(),
                        &now_str,
                    )
                    .await
                    .map_err(|e| ChvError::Internal {
                        reason: format!("failed to claim schedule run: {e}"),
                    })?;

                if !claimed {
                    info!(
                        schedule_id = %schedule.schedule_id,
                        "schedule run already claimed by another worker; skipping"
                    );
                    return Ok(());
                }

                // Create a pending backup job
                let input = BackupJobCreateInput {
                    vm_id: schedule.vm_id.clone(),
                    volume_id: schedule.volume_id.clone(),
                    schedule_id: Some(schedule.schedule_id.clone()),
                    status: "Pending".into(),
                    backup_type: "full".into(),
                    target_path: schedule.destination.clone(),
                    storage_backend: None,
                    started_at: None,
                    completed_at: None,
                    error_message: None,
                    size_bytes: None,
                    checksum: None,
                    checksum_algorithm: None,
                    destination: schedule.destination.clone(),
                };

                let job_id =
                    self.backup_repo
                        .create_job(&input)
                        .await
                        .map_err(|e| ChvError::Internal {
                            reason: format!("failed to create backup job from schedule: {e}"),
                        })?;

                info!(
                    schedule_id = %schedule.schedule_id,
                    job_id = %job_id,
                    "created scheduled backup job"
                );

                // Enforce retention: prune old completed jobs beyond retention_count.
                // List the jobs first, clean up remote artifacts, then batch-delete DB rows.
                if schedule.retention_count > 0 {
                    match self
                        .backup_repo
                        .list_old_jobs_for_count_retention(
                            &schedule.schedule_id,
                            schedule.retention_count,
                        )
                        .await
                    {
                        Ok(old_jobs) if !old_jobs.is_empty() => {
                            let mut job_ids: Vec<&str> = Vec::new();
                            for old_job in &old_jobs {
                                if !Self::artifact_removal_allows_row_delete(
                                    old_job,
                                    schedule.s3_access_key.clone(),
                                    schedule.s3_secret_key.clone(),
                                )
                                .await
                                {
                                    // Delete failed: keep the row so the
                                    // artifact is recorded and the next pass
                                    // retries (#655).
                                    continue;
                                }
                                job_ids.push(old_job.job_id.as_str());
                            }
                            match self.backup_repo.delete_jobs_by_ids(&job_ids).await {
                                Ok(deleted) if deleted > 0 => {
                                    info!(
                                        schedule_id = %schedule.schedule_id,
                                        vm_id = %schedule.vm_id,
                                        pruned_count = deleted,
                                        retention_count = schedule.retention_count,
                                        "pruned old backup jobs by count"
                                    );
                                }
                                Err(e) => {
                                    warn!(
                                        schedule_id = %schedule.schedule_id,
                                        error = %e,
                                        "failed to delete old backup jobs by count"
                                    );
                                }
                                _ => {}
                            }
                        }
                        Err(e) => {
                            warn!(
                                schedule_id = %schedule.schedule_id,
                                error = %e,
                                "failed to list old backup jobs for count retention"
                            );
                        }
                        _ => {}
                    }
                }

                // Enforce retention: prune old completed jobs beyond retention_days
                if schedule.retention_days > 0 {
                    match self
                        .backup_repo
                        .list_old_jobs_for_retention(&schedule.schedule_id, schedule.retention_days)
                        .await
                    {
                        Ok(old_jobs) if !old_jobs.is_empty() => {
                            for old_job in &old_jobs {
                                // Attempt to delete the remote artifact if
                                // shipping was used. A failed delete keeps the
                                // row (and its record of the artifact) so the
                                // next retention pass retries instead of
                                // silently orphaning the object (#655).
                                if !Self::artifact_removal_allows_row_delete(
                                    old_job,
                                    schedule.s3_access_key.clone(),
                                    schedule.s3_secret_key.clone(),
                                )
                                .await
                                {
                                    continue;
                                }

                                if let Err(e) =
                                    self.backup_repo.delete_job_by_id(&old_job.job_id).await
                                {
                                    warn!(
                                        job_id = %old_job.job_id,
                                        error = %e,
                                        "failed to delete old backup job during retention cleanup"
                                    );
                                } else {
                                    info!(
                                        job_id = %old_job.job_id,
                                        retention_days = schedule.retention_days,
                                        "pruned old backup job by age"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            warn!(
                                schedule_id = %schedule.schedule_id,
                                error = %e,
                                "failed to list old backup jobs for retention cleanup"
                            );
                        }
                        _ => {}
                    }
                }
            }
        }

        Ok(())
    }

    async fn run_executor(&self) -> Result<(), ChvError> {
        // Atomically claim pending jobs by setting status to Running in a single query.
        // This prevents double-execution if multiple backup workers run concurrently.
        let jobs: Vec<chv_controlplane_store::BackupJobRow> = sqlx::query_as(
            "UPDATE backup_jobs SET status = 'Running', started_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') \
             WHERE job_id IN ( \
                 SELECT job_id FROM backup_jobs WHERE status = 'Pending' \
                 ORDER BY created_at ASC LIMIT 10 \
             ) \
             RETURNING job_id, vm_id, volume_id, status, backup_type, target_path, \
                       storage_backend, created_at, started_at, completed_at, \
                       error_message, size_bytes, retry_count, next_retry_at",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ChvError::Internal {
            reason: format!("failed to claim pending backup jobs: {e}"),
        })?;

        let now_str = chrono::Utc::now().to_rfc3339();
        let retryable: Vec<chv_controlplane_store::BackupJobRow> = sqlx::query_as(
            "UPDATE backup_jobs SET status = 'Running', started_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') \
             WHERE job_id IN ( \
                 SELECT job_id FROM backup_jobs \
                 WHERE status = 'RetryPending' AND next_retry_at <= ? \
                 ORDER BY next_retry_at ASC LIMIT 10 \
             ) \
             RETURNING job_id, vm_id, volume_id, status, backup_type, target_path, \
                       storage_backend, created_at, started_at, completed_at, \
                       error_message, size_bytes, retry_count, next_retry_at",
        )
        .bind(&now_str)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ChvError::Internal {
            reason: format!("failed to claim retryable backup jobs: {e}"),
        })?;

        for job in jobs.iter().chain(retryable.iter()) {
            if let Err(e) = self.execute_job(job).await {
                warn!(
                    job_id = %job.job_id,
                    error = %e,
                    "failed to execute backup job"
                );

                // Check if we can retry
                let new_retry_count = job.retry_count + 1;
                if new_retry_count <= MAX_RETRIES {
                    let backoff_secs = 60i64 * (1 << (new_retry_count - 1)); // 60s, 120s, 240s
                    let next_retry = chrono::Utc::now() + chrono::Duration::seconds(backoff_secs);
                    if let Err(retry_err) = self
                        .backup_repo
                        .mark_for_retry(
                            &job.job_id,
                            new_retry_count,
                            &next_retry.to_rfc3339(),
                            &e.to_string(),
                        )
                        .await
                    {
                        error!(
                            job_id = %job.job_id,
                            error = %retry_err,
                            "failed to mark job for retry"
                        );
                    } else {
                        info!(
                            job_id = %job.job_id,
                            retry = new_retry_count,
                            next_retry_at = %next_retry.to_rfc3339(),
                            "backup job scheduled for retry"
                        );
                    }
                } else {
                    // Permanently failed
                    let now = chrono::Utc::now().to_rfc3339();
                    if let Err(update_err) = self
                        .backup_repo
                        .update_job_status(&BackupJobStatusUpdateInput {
                            job_id: job.job_id.clone(),
                            status: "Failed".into(),
                            started_at: Some(now.clone()),
                            completed_at: Some(now),
                            error_message: Some(format!(
                                "permanently failed after {} retries: {}",
                                MAX_RETRIES, e
                            )),
                            size_bytes: None,
                            target_path: None,
                            storage_backend: None,
                            checksum: None,
                            checksum_algorithm: None,
                            destination: None,
                        })
                        .await
                    {
                        error!(
                            job_id = %job.job_id,
                            error = %update_err,
                            "failed to update job status after exhausting retries"
                        );
                    }
                }
            }
        }

        Ok(())
    }

    async fn execute_job(
        &self,
        job: &chv_controlplane_store::BackupJobRow,
    ) -> Result<(), ChvError> {
        // Backup/restore is HARD-FENCED (#654): the API surface refuses new
        // work, and this executor only drains rows created before the fence so
        // none is left in a misleading Pending/Running state.
        warn!(
            job_id = %job.job_id,
            vm_id = %job.vm_id,
            "backup/restore is hard-fenced (no execution engine) — job will be marked failed"
        );
        Err(ChvError::Internal {
            reason: "backup/restore is not implemented in this release (hard-fenced, #654)"
                .to_string(),
        })
    }

    /// Attempt to delete a retention candidate's remote artifact.
    ///
    /// Returns `true` only when the caller may delete the database row: either
    /// the destination has no remote artifact, or the shipper confirmed the
    /// delete (including "already absent"). A failed delete returns `false`,
    /// increments `backup_retention_artifact_delete_failures_total`, and leaves
    /// the row in place so the artifact is not silently orphaned; the next
    /// retention pass retries (#655).
    async fn artifact_removal_allows_row_delete(
        job: &chv_controlplane_store::BackupJobRow,
        access_key: Option<String>,
        secret_key: Option<String>,
    ) -> bool {
        let (Some(dest), Some(remote_path)) =
            (job.destination.as_deref(), job.target_path.as_deref())
        else {
            return true;
        };
        if !Self::is_remote_destination(dest) {
            return true;
        }

        match shipper_from_destination(dest, access_key, secret_key) {
            Ok(shipper) => {
                Self::delete_artifact_with(shipper.as_ref(), &job.job_id, remote_path).await
            }
            Err(e) => {
                metrics::counter!("backup_retention_artifact_delete_failures_total").increment(1);
                warn!(
                    job_id = %job.job_id,
                    error = %e,
                    "failed to build shipper for remote backup artifact cleanup; keeping DB row for retry"
                );
                false
            }
        }
    }

    /// Delete one remote artifact and report whether the row may be removed.
    ///
    /// Split from [`Self::artifact_removal_allows_row_delete`] so tests can
    /// inject a failing [`crate::backup_shipper::BackupShipper`] without a
    /// network endpoint.
    async fn delete_artifact_with(
        shipper: &dyn crate::backup_shipper::BackupShipper,
        job_id: &str,
        remote_path: &str,
    ) -> bool {
        match shipper.delete(remote_path).await {
            Ok(()) => true,
            Err(e) => {
                metrics::counter!("backup_retention_artifact_delete_failures_total").increment(1);
                warn!(
                    job_id = %job_id,
                    path = %remote_path,
                    error = %e,
                    "failed to delete remote backup artifact during retention cleanup; keeping DB row for retry"
                );
                false
            }
        }
    }

    fn is_remote_destination(destination: &str) -> bool {
        destination.starts_with("s3://")
            || destination.starts_with("nfs://")
            || destination.eq_ignore_ascii_case("null")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup_shipper::BackupShipper;

    struct AlwaysOkShipper;
    #[async_trait::async_trait]
    impl BackupShipper for AlwaysOkShipper {
        async fn delete(&self, _remote_path: &str) -> Result<(), ChvError> {
            Ok(())
        }
    }

    struct AlwaysFailShipper;
    #[async_trait::async_trait]
    impl BackupShipper for AlwaysFailShipper {
        async fn delete(&self, _remote_path: &str) -> Result<(), ChvError> {
            Err(ChvError::Internal {
                reason: "synthetic delete failure".to_string(),
            })
        }
    }

    fn job(
        destination: Option<&str>,
        target_path: Option<&str>,
    ) -> chv_controlplane_store::BackupJobRow {
        chv_controlplane_store::BackupJobRow {
            job_id: "job-test".to_string(),
            vm_id: "vm-test".to_string(),
            volume_id: None,
            schedule_id: None,
            status: "Succeeded".to_string(),
            backup_type: "full".to_string(),
            target_path: target_path.map(str::to_string),
            storage_backend: None,
            created_at: "2026-11-03T00:00:00Z".to_string(),
            started_at: None,
            completed_at: None,
            error_message: None,
            size_bytes: None,
            retry_count: 0,
            next_retry_at: None,
            checksum: None,
            checksum_algorithm: None,
            destination: destination.map(str::to_string),
        }
    }

    #[tokio::test]
    async fn successful_artifact_delete_allows_row_removal() {
        assert!(
            BackupWorker::delete_artifact_with(&AlwaysOkShipper, "job-1", "key").await,
            "a confirmed delete must allow the row delete"
        );
    }

    #[tokio::test]
    async fn failed_artifact_delete_keeps_row() {
        assert!(
            !BackupWorker::delete_artifact_with(&AlwaysFailShipper, "job-2", "key").await,
            "a failed delete must retain the row so the artifact is not orphaned"
        );
    }

    #[tokio::test]
    async fn non_remote_destination_does_not_require_artifact_delete() {
        let row = job(None, Some("/var/lib/chv/backups/artifact"));
        assert!(
            BackupWorker::artifact_removal_allows_row_delete(&row, None, None).await,
            "a local row has no remote artifact to delete"
        );
    }

    #[tokio::test]
    async fn unreachable_shipper_keeps_row() {
        // A syntactically valid but unreachable endpoint exercises the real
        // delete-failure path: the row must stay so the artifact is retried.
        let row = job(
            Some("s3://bucket/prefix?endpoint=http://127.0.0.1:1"),
            Some("prefix/artifact"),
        );
        assert!(
            !BackupWorker::artifact_removal_allows_row_delete(
                &row,
                Some("ak".to_string()),
                Some("sk".to_string())
            )
            .await,
            "an unreachable shipper must retain the row for retry"
        );
    }
}
