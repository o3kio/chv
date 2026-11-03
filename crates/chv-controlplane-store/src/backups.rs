use crate::{credential_crypto::CredentialEncryption, StoreError, StorePool};

// ── Backup Jobs ────────────────────────────────────────────────────────────

const LIST_JOBS_SQL: &str = r#"
SELECT
    job_id,
    vm_id,
    volume_id,
    schedule_id,
    status,
    backup_type,
    target_path,
    storage_backend,
    created_at,
    started_at,
    completed_at,
    error_message,
    size_bytes,
    retry_count,
    next_retry_at,
    checksum,
    checksum_algorithm,
    destination
FROM backup_jobs
ORDER BY created_at DESC
LIMIT ? OFFSET ?
"#;

const COUNT_JOBS_SQL: &str = "SELECT COUNT(*) FROM backup_jobs";

const GET_JOB_SQL: &str = r#"
SELECT
    job_id,
    vm_id,
    volume_id,
    schedule_id,
    status,
    backup_type,
    target_path,
    storage_backend,
    created_at,
    started_at,
    completed_at,
    error_message,
    size_bytes,
    retry_count,
    next_retry_at,
    checksum,
    checksum_algorithm,
    destination
FROM backup_jobs
WHERE job_id = ?
"#;

const INSERT_JOB_SQL: &str = r#"
INSERT INTO backup_jobs (
    job_id,
    vm_id,
    volume_id,
    schedule_id,
    status,
    backup_type,
    target_path,
    storage_backend,
    created_at,
    started_at,
    completed_at,
    error_message,
    size_bytes,
    checksum,
    checksum_algorithm,
    destination
)
VALUES (
    ?, ?, ?, ?, ?, ?, ?, ?,
    strftime('%Y-%m-%dT%H:%M:%SZ','now'),
    ?, ?, ?, ?, ?, ?, ?
)
"#;

const UPDATE_JOB_STATUS_SQL: &str = r#"
UPDATE backup_jobs SET
    status = ?,
    started_at = ?,
    completed_at = ?,
    error_message = ?,
    size_bytes = ?,
    target_path = ?,
    storage_backend = ?,
    checksum = ?,
    checksum_algorithm = ?,
    destination = ?
WHERE job_id = ?
"#;

const UPDATE_JOB_SQL: &str = r#"
UPDATE backup_jobs SET
    volume_id = ?,
    status = ?,
    backup_type = ?,
    target_path = ?,
    storage_backend = ?,
    started_at = ?,
    completed_at = ?,
    error_message = ?,
    size_bytes = ?,
    checksum = ?,
    checksum_algorithm = ?,
    destination = ?
WHERE job_id = ?
"#;

const DELETE_JOB_SQL: &str = "DELETE FROM backup_jobs WHERE job_id = ?";

const LIST_JOBS_FOR_VM_SQL: &str = r#"
SELECT
    job_id,
    vm_id,
    volume_id,
    schedule_id,
    status,
    backup_type,
    target_path,
    storage_backend,
    created_at,
    started_at,
    completed_at,
    error_message,
    size_bytes,
    retry_count,
    next_retry_at,
    checksum,
    checksum_algorithm,
    destination
FROM backup_jobs
WHERE vm_id = ?
ORDER BY created_at DESC
LIMIT ? OFFSET ?
"#;

const COUNT_JOBS_FOR_VM_SQL: &str = "SELECT COUNT(*) FROM backup_jobs WHERE vm_id = ?";

// ── Backup Schedules ───────────────────────────────────────────────────────

const LIST_SCHEDULES_SQL: &str = r#"
SELECT
    schedule_id,
    vm_id,
    volume_id,
    name,
    cron_expression,
    retention_count,
    retention_days,
    destination,
    enabled,
    created_at,
    updated_at,
    last_run_at,
    s3_access_key,
    s3_secret_key
FROM backup_schedules
ORDER BY created_at DESC
LIMIT ? OFFSET ?
"#;

const COUNT_SCHEDULES_SQL: &str = "SELECT COUNT(*) FROM backup_schedules";

const GET_SCHEDULE_SQL: &str = r#"
SELECT
    schedule_id,
    vm_id,
    volume_id,
    name,
    cron_expression,
    retention_count,
    retention_days,
    destination,
    enabled,
    created_at,
    updated_at,
    last_run_at,
    s3_access_key,
    s3_secret_key
FROM backup_schedules
WHERE schedule_id = ?
"#;

const INSERT_SCHEDULE_SQL: &str = r#"
INSERT INTO backup_schedules (
    schedule_id,
    vm_id,
    volume_id,
    name,
    cron_expression,
    retention_count,
    retention_days,
    destination,
    enabled,
    s3_access_key,
    s3_secret_key,
    created_at,
    updated_at
)
VALUES (
    ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
    strftime('%Y-%m-%dT%H:%M:%SZ','now'),
    strftime('%Y-%m-%dT%H:%M:%SZ','now')
)
"#;

const UPDATE_SCHEDULE_SQL: &str = r#"
UPDATE backup_schedules SET
    vm_id = ?,
    volume_id = ?,
    name = ?,
    cron_expression = ?,
    retention_count = ?,
    retention_days = ?,
    destination = ?,
    enabled = ?,
    s3_access_key = ?,
    s3_secret_key = ?,
    updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')
WHERE schedule_id = ?
"#;

const DELETE_SCHEDULE_SQL: &str = "DELETE FROM backup_schedules WHERE schedule_id = ?";

// ── Backup Restores ────────────────────────────────────────────────────────

const LIST_RESTORES_SQL: &str = r#"
SELECT
    restore_id,
    backup_job_id,
    target_vm_id,
    target_volume_id,
    status,
    created_at,
    started_at,
    completed_at,
    error_message,
    source_path,
    storage_backend
FROM backup_restores
ORDER BY created_at DESC
LIMIT ? OFFSET ?
"#;

const COUNT_RESTORES_SQL: &str = "SELECT COUNT(*) FROM backup_restores";

const GET_RESTORE_SQL: &str = r#"
SELECT
    restore_id,
    backup_job_id,
    target_vm_id,
    target_volume_id,
    status,
    created_at,
    started_at,
    completed_at,
    error_message,
    source_path,
    storage_backend
FROM backup_restores
WHERE restore_id = ?
"#;

const INSERT_RESTORE_SQL: &str = r#"
INSERT INTO backup_restores (
    restore_id,
    backup_job_id,
    target_vm_id,
    target_volume_id,
    status,
    created_at,
    started_at,
    completed_at,
    error_message,
    source_path,
    storage_backend
)
VALUES (
    ?, ?, ?, ?, ?,
    strftime('%Y-%m-%dT%H:%M:%SZ','now'),
    ?, ?, ?, ?, ?
)
"#;

// ── Repository ─────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct BackupRepository {
    pool: StorePool,
    crypto: CredentialEncryption,
}

impl BackupRepository {
    pub fn new(pool: StorePool) -> Self {
        Self {
            pool,
            crypto: CredentialEncryption::new(),
        }
    }

    /// Production constructor keyed from `[secrets] credential_encryption`
    /// (#662), bypassing the environment lookup. Callers must use the same
    /// key to read rows written by this instance.
    pub fn new_with_key(pool: StorePool, key: &str) -> Self {
        Self {
            pool,
            crypto: CredentialEncryption::from_key(key),
        }
    }

    /// Test-support constructor with an injected credential cipher, so tests
    /// can exercise fail-closed writes without mutating process-global env.
    #[cfg(any(test, feature = "test-util"))]
    pub fn with_crypto(pool: StorePool, crypto: CredentialEncryption) -> Self {
        Self { pool, crypto }
    }

    /// Encrypts a schedule credential, mapping a fail-closed encryption
    /// error to a store configuration error. Callers run this before the
    /// INSERT/UPDATE so a failed encryption never writes a row.
    fn encrypt_credential(&self, value: &str, field: &'static str) -> Result<String, StoreError> {
        self.crypto
            .encrypt(value)
            .map_err(|err| StoreError::InvalidConfiguration {
                reason: format!(
                    "cannot store backup schedule {field}: credential encryption \
                     is unavailable (fail-closed): {err}"
                ),
            })
    }

    /// Decrypts the S3 credentials on a schedule row in place.
    ///
    /// Fail-closed: if a credential cannot be decrypted (wrong key, tampered
    /// ciphertext, malformed payload, missing key) the field is set to `None`
    /// and a structured error is logged. We deliberately do NOT fall back to
    /// the ciphertext literal — passing `enc:hex...` to the S3 client would
    /// surface as opaque AWS auth errors and silently break backups.
    ///
    /// A `None` credential propagates downstream where the backup worker
    /// (or API handler) can return a clear "S3 credentials not configured
    /// or undecryptable" error to the operator. Stale credential is better
    /// than ciphertext-as-credential.
    fn decrypt_schedule_row(&self, row: &mut BackupScheduleRow) {
        if let Some(ref key) = row.s3_access_key {
            row.s3_access_key = match self.crypto.decrypt(key) {
                Ok(plain) => Some(plain),
                Err(e) => {
                    tracing::error!(
                        schedule_id = %row.schedule_id,
                        field = "s3_access_key",
                        error = %e,
                        "credential decrypt failed; setting field to None (fail-closed)"
                    );
                    None
                }
            };
        }
        if let Some(ref key) = row.s3_secret_key {
            row.s3_secret_key = match self.crypto.decrypt(key) {
                Ok(plain) => Some(plain),
                Err(e) => {
                    tracing::error!(
                        schedule_id = %row.schedule_id,
                        field = "s3_secret_key",
                        error = %e,
                        "credential decrypt failed; setting field to None (fail-closed)"
                    );
                    None
                }
            };
        }
    }

    pub fn pool(&self) -> &StorePool {
        &self.pool
    }

    // ── Jobs ─────────────────────────────────────────────────────────────────

    pub async fn list_jobs(
        &self,
        page_size: i64,
        offset: i64,
    ) -> Result<(Vec<BackupJobRow>, i64), StoreError> {
        let rows = sqlx::query_as::<_, BackupJobRow>(LIST_JOBS_SQL)
            .bind(page_size)
            .bind(offset)
            .fetch_all(&self.pool)
            .await?;

        let count: i64 = sqlx::query_scalar(COUNT_JOBS_SQL)
            .fetch_one(&self.pool)
            .await?;

        Ok((rows, count))
    }

    pub async fn get_job(&self, job_id: &str) -> Result<Option<BackupJobRow>, StoreError> {
        let row = sqlx::query_as::<_, BackupJobRow>(GET_JOB_SQL)
            .bind(job_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    pub async fn create_job(&self, input: &BackupJobCreateInput) -> Result<String, StoreError> {
        let job_id = chv_common::gen_resource_id();
        sqlx::query(INSERT_JOB_SQL)
            .bind(&job_id)
            .bind(&input.vm_id)
            .bind(&input.volume_id)
            .bind(&input.schedule_id)
            .bind(&input.status)
            .bind(&input.backup_type)
            .bind(&input.target_path)
            .bind(&input.storage_backend)
            .bind(&input.started_at)
            .bind(&input.completed_at)
            .bind(&input.error_message)
            .bind(input.size_bytes)
            .bind(&input.checksum)
            .bind(&input.checksum_algorithm)
            .bind(&input.destination)
            .execute(&self.pool)
            .await?;
        Ok(job_id)
    }

    pub async fn update_job_status(
        &self,
        input: &BackupJobStatusUpdateInput,
    ) -> Result<(), StoreError> {
        let result = sqlx::query(UPDATE_JOB_STATUS_SQL)
            .bind(&input.status)
            .bind(&input.started_at)
            .bind(&input.completed_at)
            .bind(&input.error_message)
            .bind(input.size_bytes)
            .bind(&input.target_path)
            .bind(&input.storage_backend)
            .bind(&input.checksum)
            .bind(&input.checksum_algorithm)
            .bind(&input.destination)
            .bind(&input.job_id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound {
                entity: "backup_job",
                id: input.job_id.clone(),
            });
        }
        Ok(())
    }

    pub async fn update_job(&self, input: &BackupJobUpdateInput) -> Result<(), StoreError> {
        let result = sqlx::query(UPDATE_JOB_SQL)
            .bind(&input.volume_id)
            .bind(&input.status)
            .bind(&input.backup_type)
            .bind(&input.target_path)
            .bind(&input.storage_backend)
            .bind(&input.started_at)
            .bind(&input.completed_at)
            .bind(&input.error_message)
            .bind(input.size_bytes)
            .bind(&input.checksum)
            .bind(&input.checksum_algorithm)
            .bind(&input.destination)
            .bind(&input.job_id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound {
                entity: "backup_job",
                id: input.job_id.clone(),
            });
        }
        Ok(())
    }

    pub async fn delete_job(&self, job_id: &str) -> Result<(), StoreError> {
        let result = sqlx::query(DELETE_JOB_SQL)
            .bind(job_id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound {
                entity: "backup_job",
                id: job_id.to_string(),
            });
        }
        Ok(())
    }

    pub async fn list_jobs_for_vm(
        &self,
        vm_id: &str,
        page_size: i64,
        offset: i64,
    ) -> Result<(Vec<BackupJobRow>, i64), StoreError> {
        let rows = sqlx::query_as::<_, BackupJobRow>(LIST_JOBS_FOR_VM_SQL)
            .bind(vm_id)
            .bind(page_size)
            .bind(offset)
            .fetch_all(&self.pool)
            .await?;

        let count: i64 = sqlx::query_scalar(COUNT_JOBS_FOR_VM_SQL)
            .bind(vm_id)
            .fetch_one(&self.pool)
            .await?;

        Ok((rows, count))
    }

    // ── Schedules ────────────────────────────────────────────────────────────

    pub async fn list_schedules(
        &self,
        page_size: i64,
        offset: i64,
    ) -> Result<(Vec<BackupScheduleRow>, i64), StoreError> {
        let mut rows = sqlx::query_as::<_, BackupScheduleRow>(LIST_SCHEDULES_SQL)
            .bind(page_size)
            .bind(offset)
            .fetch_all(&self.pool)
            .await?;

        let count: i64 = sqlx::query_scalar(COUNT_SCHEDULES_SQL)
            .fetch_one(&self.pool)
            .await?;

        for row in &mut rows {
            self.decrypt_schedule_row(row);
        }

        Ok((rows, count))
    }

    pub async fn get_schedule(
        &self,
        schedule_id: &str,
    ) -> Result<Option<BackupScheduleRow>, StoreError> {
        let mut row = sqlx::query_as::<_, BackupScheduleRow>(GET_SCHEDULE_SQL)
            .bind(schedule_id)
            .fetch_optional(&self.pool)
            .await?;
        if let Some(ref mut r) = row {
            self.decrypt_schedule_row(r);
        }
        Ok(row)
    }

    pub async fn create_schedule(
        &self,
        input: &BackupScheduleCreateInput,
    ) -> Result<String, StoreError> {
        let schedule_id = chv_common::gen_resource_id();
        let access_key = input
            .s3_access_key
            .as_deref()
            .map(|k| self.encrypt_credential(k, "s3_access_key"))
            .transpose()?;
        let secret_key = input
            .s3_secret_key
            .as_deref()
            .map(|k| self.encrypt_credential(k, "s3_secret_key"))
            .transpose()?;
        sqlx::query(INSERT_SCHEDULE_SQL)
            .bind(&schedule_id)
            .bind(&input.vm_id)
            .bind(&input.volume_id)
            .bind(&input.name)
            .bind(&input.cron_expression)
            .bind(input.retention_count)
            .bind(input.retention_days)
            .bind(&input.destination)
            .bind(input.enabled)
            .bind(&access_key)
            .bind(&secret_key)
            .execute(&self.pool)
            .await?;
        Ok(schedule_id)
    }

    pub async fn update_schedule(
        &self,
        input: &BackupScheduleUpdateInput,
    ) -> Result<(), StoreError> {
        let access_key = input
            .s3_access_key
            .as_deref()
            .map(|k| self.encrypt_credential(k, "s3_access_key"))
            .transpose()?;
        let secret_key = input
            .s3_secret_key
            .as_deref()
            .map(|k| self.encrypt_credential(k, "s3_secret_key"))
            .transpose()?;
        let result = sqlx::query(UPDATE_SCHEDULE_SQL)
            .bind(&input.vm_id)
            .bind(&input.volume_id)
            .bind(&input.name)
            .bind(&input.cron_expression)
            .bind(input.retention_count)
            .bind(input.retention_days)
            .bind(&input.destination)
            .bind(input.enabled)
            .bind(&access_key)
            .bind(&secret_key)
            .bind(&input.schedule_id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound {
                entity: "backup_schedule",
                id: input.schedule_id.clone(),
            });
        }
        Ok(())
    }

    pub async fn delete_schedule(&self, schedule_id: &str) -> Result<(), StoreError> {
        let result = sqlx::query(DELETE_SCHEDULE_SQL)
            .bind(schedule_id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound {
                entity: "backup_schedule",
                id: schedule_id.to_string(),
            });
        }
        Ok(())
    }

    pub async fn list_enabled_schedules(&self) -> Result<Vec<BackupScheduleRow>, StoreError> {
        let mut rows = sqlx::query_as::<_, BackupScheduleRow>(
            "SELECT schedule_id, vm_id, volume_id, name, cron_expression, retention_count, \
             retention_days, destination, enabled, created_at, updated_at, last_run_at, s3_access_key, s3_secret_key \
             FROM backup_schedules WHERE enabled = true ORDER BY created_at ASC LIMIT 100",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::from)?;

        for row in &mut rows {
            self.decrypt_schedule_row(row);
        }

        Ok(rows)
    }

    /// Atomically claim a schedule run using optimistic locking on `last_run_at`.
    /// Returns `true` if the row was updated (we won the race), `false` if another
    /// worker already processed this schedule slot.
    pub async fn try_claim_schedule_run(
        &self,
        schedule_id: &str,
        expected_last_run: Option<&str>,
        new_last_run: &str,
    ) -> Result<bool, StoreError> {
        let result = match expected_last_run {
            Some(expected) => {
                sqlx::query(
                    "UPDATE backup_schedules SET last_run_at = ? WHERE schedule_id = ? AND last_run_at = ?"
                )
                .bind(new_last_run)
                .bind(schedule_id)
                .bind(expected)
                .execute(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    "UPDATE backup_schedules SET last_run_at = ? WHERE schedule_id = ? AND last_run_at IS NULL"
                )
                .bind(new_last_run)
                .bind(schedule_id)
                .execute(&self.pool)
                .await?
            }
        };
        Ok(result.rows_affected() > 0)
    }

    /// List completed backup jobs for a specific schedule that exceed the
    /// retention count, ordered oldest first.  Used by the worker to clean up
    /// remote artifacts before deleting the DB rows.
    pub async fn list_old_jobs_for_count_retention(
        &self,
        schedule_id: &str,
        retention_count: i64,
    ) -> Result<Vec<BackupJobRow>, StoreError> {
        sqlx::query_as::<_, BackupJobRow>(
            "SELECT job_id, vm_id, volume_id, schedule_id, status, backup_type, \
             target_path, storage_backend, created_at, started_at, completed_at, \
             error_message, size_bytes, retry_count, next_retry_at, checksum, checksum_algorithm, destination \
             FROM backup_jobs \
             WHERE schedule_id = ? AND status IN ('Succeeded', 'Failed') \
             ORDER BY created_at DESC \
             LIMIT -1 OFFSET ?",
        )
        .bind(schedule_id)
        .bind(retention_count)
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::from)
    }

    /// Batch-delete backup jobs by ID.
    pub async fn delete_jobs_by_ids(&self, job_ids: &[&str]) -> Result<u64, StoreError> {
        if job_ids.is_empty() {
            return Ok(0);
        }
        let placeholders = job_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!("DELETE FROM backup_jobs WHERE job_id IN ({})", placeholders);
        let mut query = sqlx::query(&sql);
        for id in job_ids {
            query = query.bind(id);
        }
        let result = query.execute(&self.pool).await?;
        Ok(result.rows_affected())
    }

    pub async fn mark_for_retry(
        &self,
        job_id: &str,
        retry_count: i64,
        next_retry_at: &str,
        error_message: &str,
    ) -> Result<(), StoreError> {
        let result = sqlx::query(
            "UPDATE backup_jobs SET \
             status = 'RetryPending', \
             retry_count = ?, \
             next_retry_at = ?, \
             error_message = ? \
             WHERE job_id = ?",
        )
        .bind(retry_count)
        .bind(next_retry_at)
        .bind(error_message)
        .bind(job_id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound {
                entity: "backup_job",
                id: job_id.to_string(),
            });
        }
        Ok(())
    }

    // ── Restores ─────────────────────────────────────────────────────────────

    pub async fn list_restores(
        &self,
        page_size: i64,
        offset: i64,
    ) -> Result<(Vec<BackupRestoreRow>, i64), StoreError> {
        let rows = sqlx::query_as::<_, BackupRestoreRow>(LIST_RESTORES_SQL)
            .bind(page_size)
            .bind(offset)
            .fetch_all(&self.pool)
            .await?;

        let count: i64 = sqlx::query_scalar(COUNT_RESTORES_SQL)
            .fetch_one(&self.pool)
            .await?;

        Ok((rows, count))
    }

    pub async fn get_restore(
        &self,
        restore_id: &str,
    ) -> Result<Option<BackupRestoreRow>, StoreError> {
        let row = sqlx::query_as::<_, BackupRestoreRow>(GET_RESTORE_SQL)
            .bind(restore_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    pub async fn create_restore(
        &self,
        input: &BackupRestoreCreateInput,
    ) -> Result<String, StoreError> {
        let restore_id = chv_common::gen_resource_id();
        sqlx::query(INSERT_RESTORE_SQL)
            .bind(&restore_id)
            .bind(&input.backup_job_id)
            .bind(&input.target_vm_id)
            .bind(&input.target_volume_id)
            .bind(&input.status)
            .bind(&input.started_at)
            .bind(&input.completed_at)
            .bind(&input.error_message)
            .bind(&input.source_path)
            .bind(&input.storage_backend)
            .execute(&self.pool)
            .await?;
        Ok(restore_id)
    }

    /// List completed backup jobs older than `retention_days` for a given schedule.
    pub async fn list_old_jobs_for_retention(
        &self,
        schedule_id: &str,
        retention_days: i64,
    ) -> Result<Vec<BackupJobRow>, StoreError> {
        let cutoff = (chrono::Utc::now() - chrono::Duration::days(retention_days)).to_rfc3339();
        sqlx::query_as::<_, BackupJobRow>(
            "SELECT job_id, vm_id, volume_id, schedule_id, status, backup_type, \
             target_path, storage_backend, created_at, started_at, completed_at, \
             error_message, size_bytes, retry_count, next_retry_at, checksum, checksum_algorithm, destination \
             FROM backup_jobs \
             WHERE schedule_id = ? AND status IN ('Succeeded', 'Failed') \
             AND created_at < ? \
             ORDER BY created_at ASC LIMIT 100",
        )
        .bind(schedule_id)
        .bind(cutoff)
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::from)
    }

    /// Delete a backup job by ID (used by retention enforcer).
    pub async fn delete_job_by_id(&self, job_id: &str) -> Result<u64, StoreError> {
        let result = sqlx::query("DELETE FROM backup_jobs WHERE job_id = ?")
            .bind(job_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}

// ── Row Types ──────────────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
pub struct BackupJobRow {
    pub job_id: String,
    pub vm_id: String,
    pub volume_id: Option<String>,
    pub schedule_id: Option<String>,
    pub status: String,
    pub backup_type: String,
    pub target_path: Option<String>,
    pub storage_backend: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub error_message: Option<String>,
    pub size_bytes: Option<i64>,
    pub retry_count: i64,
    pub next_retry_at: Option<String>,
    pub checksum: Option<String>,
    pub checksum_algorithm: Option<String>,
    pub destination: Option<String>,
}

#[derive(sqlx::FromRow)]
pub struct BackupScheduleRow {
    pub schedule_id: String,
    pub vm_id: String,
    pub volume_id: Option<String>,
    pub name: String,
    pub cron_expression: String,
    pub retention_count: i64,
    pub retention_days: i64,
    pub destination: Option<String>,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
    pub last_run_at: Option<String>,
    pub s3_access_key: Option<String>,
    pub s3_secret_key: Option<String>,
}

#[derive(sqlx::FromRow)]
pub struct BackupRestoreRow {
    pub restore_id: String,
    pub backup_job_id: String,
    pub target_vm_id: Option<String>,
    pub target_volume_id: Option<String>,
    pub status: String,
    pub created_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub error_message: Option<String>,
    pub source_path: Option<String>,
    pub storage_backend: Option<String>,
}

// ── Input Types ──────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct BackupJobCreateInput {
    pub vm_id: String,
    pub volume_id: Option<String>,
    pub schedule_id: Option<String>,
    pub status: String,
    pub backup_type: String,
    pub target_path: Option<String>,
    pub storage_backend: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub error_message: Option<String>,
    pub size_bytes: Option<i64>,
    pub checksum: Option<String>,
    pub checksum_algorithm: Option<String>,
    pub destination: Option<String>,
}

#[derive(Clone)]
pub struct BackupJobStatusUpdateInput {
    pub job_id: String,
    pub status: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub error_message: Option<String>,
    pub size_bytes: Option<i64>,
    pub target_path: Option<String>,
    pub storage_backend: Option<String>,
    pub checksum: Option<String>,
    pub checksum_algorithm: Option<String>,
    pub destination: Option<String>,
}

#[derive(Clone)]
pub struct BackupJobUpdateInput {
    pub job_id: String,
    pub volume_id: Option<String>,
    pub status: String,
    pub backup_type: String,
    pub target_path: Option<String>,
    pub storage_backend: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub error_message: Option<String>,
    pub size_bytes: Option<i64>,
    pub checksum: Option<String>,
    pub checksum_algorithm: Option<String>,
    pub destination: Option<String>,
}

#[derive(Clone)]
pub struct BackupScheduleCreateInput {
    pub vm_id: String,
    pub volume_id: Option<String>,
    pub name: String,
    pub cron_expression: String,
    pub retention_count: i64,
    pub retention_days: i64,
    pub destination: Option<String>,
    pub enabled: bool,
    pub s3_access_key: Option<String>,
    pub s3_secret_key: Option<String>,
}

#[derive(Clone)]
pub struct BackupScheduleUpdateInput {
    pub schedule_id: String,
    pub vm_id: String,
    pub volume_id: Option<String>,
    pub name: String,
    pub cron_expression: String,
    pub retention_count: i64,
    pub retention_days: i64,
    pub destination: Option<String>,
    pub enabled: bool,
    pub s3_access_key: Option<String>,
    pub s3_secret_key: Option<String>,
}

#[derive(Clone)]
pub struct BackupRestoreCreateInput {
    pub backup_job_id: String,
    pub target_vm_id: Option<String>,
    pub target_volume_id: Option<String>,
    pub status: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub error_message: Option<String>,
    pub source_path: Option<String>,
    pub storage_backend: Option<String>,
}
