//! Transactional outbox for the privileged-mutation audit trail
//! (#706 half 1).
//!
//! The audit row is normally written *after* the handler returns, which
//! leaves a crash window: the mutation commits and the process dies
//! before the audit insert commits. An adopted mutation instead writes
//! an [`AuditOutboxInsert`] **inside its own transaction** via
//! [`enqueue_on_tx`], so the evidence intent commits with the mutation
//! or not at all.
//!
//! A bounded background dispatcher claims due rows with a lease
//! ([`AuditOutboxRepository::claim_due`]) and calls
//! [`AuditOutboxRepository::complete`], which inserts the pre-minted
//! `audit_log` row and marks the outbox row `dispatched` **in one
//! transaction**. A crash between those two statements is therefore
//! impossible and a replayed claim needs no de-duplication.
//!
//! Unlike `notification_outbox` there is **no dead-letter and no
//! attempt ceiling**: audit evidence is never droppable, so failed
//! dispatches retry forever with capped backoff. `INSERT` (not
//! `INSERT OR IGNORE`) makes an `outbox_id` collision a loud store
//! error; the emitter mints the key exactly once per request.
//!
//! ## Rollout status (PR 1)
//!
//! This module is plumbing only. No request path enqueues yet; the
//! table is created empty and the dispatcher finds nothing. Adoption
//! follows in later PRs (BFF, then gRPC).

use crate::audit_log::{insert_row_on_tx, AuditLogInsert};
use crate::{StoreError, StorePool};
use sqlx::Transaction;

/// Outbox status vocabulary. There is deliberately no `dead` state.
pub const AUDIT_OUTBOX_PENDING: &str = "pending";
pub const AUDIT_OUTBOX_DISPATCHED: &str = "dispatched";

/// Bounds for identifier fields (`outbox_id`, `audit_id`).
const MAX_ID_BYTES: usize = 256;
/// Bound for the serialized [`AuditLogInsert`] payload (the
/// `NotificationEventInput` precedent).
const MAX_PAYLOAD_BYTES: usize = 16 * 1024;

/// Everything needed to enqueue one audit row. `payload` is the
/// already-redacted row built by the emitter; this structure carries
/// it and never re-derives content.
#[derive(Debug, Clone)]
pub struct AuditOutboxInsert {
    /// Per-request idempotency key, minted exactly once by the emitter.
    pub outbox_id: String,
    /// Final `audit_log` primary key, minted at enqueue so dispatch is
    /// deterministic and replay can reuse it.
    pub audit_id: String,
    /// The redacted row the dispatcher will insert verbatim.
    pub payload: AuditLogInsert,
    /// Enqueue time; also the initial due time.
    pub created_at_ms: i64,
}

impl AuditOutboxInsert {
    /// Reject malformed inputs before they reach SQL. The payload is
    /// typed, so JSON validity is guaranteed by construction; the
    /// serialized-size bound still applies because `metadata` is
    /// caller-built.
    pub fn validate(&self) -> Result<(), StoreError> {
        for (label, value) in [("outbox_id", &self.outbox_id), ("audit_id", &self.audit_id)] {
            if value.is_empty() || value.len() > MAX_ID_BYTES {
                return Err(StoreError::InvalidConfiguration {
                    reason: format!("audit outbox {label} must be 1..={MAX_ID_BYTES} bytes"),
                });
            }
        }
        self.payload_json()?;
        Ok(())
    }

    fn payload_json(&self) -> Result<String, StoreError> {
        let payload =
            serde_json::to_string(&self.payload).map_err(|e| StoreError::InvalidConfiguration {
                reason: format!("audit outbox payload is not serializable: {e}"),
            })?;
        if payload.is_empty() || payload.len() > MAX_PAYLOAD_BYTES {
            return Err(StoreError::InvalidConfiguration {
                reason: format!("audit outbox payload must be 1..={MAX_PAYLOAD_BYTES} bytes"),
            });
        }
        Ok(payload)
    }
}

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct AuditOutboxRow {
    pub outbox_id: String,
    pub audit_id: String,
    pub payload: String,
    pub status: String,
    pub attempts: i64,
    pub next_attempt_at_ms: i64,
    pub last_attempt_ms: Option<i64>,
    pub last_error: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub dispatched_at_ms: Option<i64>,
}

impl AuditOutboxRow {
    /// Parse the stored, already-redacted row. A corrupt payload is a
    /// loud store error (the dispatcher schedules a retry and logs);
    /// it is never silently dropped.
    pub fn parse_payload(&self) -> Result<AuditLogInsert, StoreError> {
        serde_json::from_str(&self.payload).map_err(|e| StoreError::InvalidConfiguration {
            reason: format!(
                "audit outbox row {} has an unparseable payload: {e}",
                self.outbox_id
            ),
        })
    }
}

const ENQUEUE_SQL: &str = r#"
INSERT INTO audit_outbox (
    outbox_id, audit_id, payload, status, attempts, next_attempt_at_ms,
    last_attempt_ms, last_error, created_at_ms, updated_at_ms, dispatched_at_ms
)
VALUES ($1, $2, $3, 'pending', 0, $4, NULL, NULL, $4, $4, NULL)
"#;

/// Enqueue on an open transaction — the only way an enqueue may join a
/// mutation, giving the "mutation and outbox row commit together"
/// guarantee. A duplicate `outbox_id` is a loud error, not an ignored
/// replay: the emitter mints the key once per request.
pub async fn enqueue_on_tx(
    tx: &mut Transaction<'_, sqlx::Sqlite>,
    input: &AuditOutboxInsert,
) -> Result<bool, StoreError> {
    input.validate()?;
    let payload = input.payload_json()?;
    let result = sqlx::query(ENQUEUE_SQL)
        .bind(&input.outbox_id)
        .bind(&input.audit_id)
        .bind(payload)
        .bind(input.created_at_ms)
        .execute(&mut **tx)
        .await?;
    Ok(result.rows_affected() > 0)
}

#[derive(Clone)]
pub struct AuditOutboxRepository {
    pool: StorePool,
}

impl AuditOutboxRepository {
    pub fn new(pool: StorePool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &StorePool {
        &self.pool
    }

    /// Pool variant of [`enqueue_on_tx`], for tests and tools. Production
    /// mutation paths must use the tx-coupled function so evidence and
    /// mutation share a commit.
    pub async fn enqueue(&self, input: &AuditOutboxInsert) -> Result<bool, StoreError> {
        input.validate()?;
        let payload = input.payload_json()?;
        let result = sqlx::query(ENQUEUE_SQL)
            .bind(&input.outbox_id)
            .bind(&input.audit_id)
            .bind(payload)
            .bind(input.created_at_ms)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn get(&self, outbox_id: &str) -> Result<Option<AuditOutboxRow>, StoreError> {
        let row: Option<AuditOutboxRow> =
            sqlx::query_as("SELECT * FROM audit_outbox WHERE outbox_id = $1")
                .bind(outbox_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row)
    }

    /// Atomically claim due pending rows. The claim increments
    /// `attempts`, stamps `last_attempt_ms`, and pushes
    /// `next_attempt_at_ms` out by the lease, so a dispatcher crash
    /// mid-batch lets the rows return on lease expiry — no reaper.
    /// Fairness order is `(created_at_ms, outbox_id)`; read order is
    /// unaffected because `occurred_at_ms` is stamped at enqueue.
    pub async fn claim_due(
        &self,
        now_ms: i64,
        lease_ms: i64,
        limit: i64,
    ) -> Result<Vec<AuditOutboxRow>, StoreError> {
        let rows: Vec<AuditOutboxRow> = sqlx::query_as(
            r#"
            UPDATE audit_outbox SET
                attempts = attempts + 1,
                last_attempt_ms = $1,
                next_attempt_at_ms = $1 + $2,
                updated_at_ms = $1
            WHERE outbox_id IN (
                SELECT outbox_id FROM audit_outbox
                WHERE status = 'pending' AND next_attempt_at_ms <= $1
                ORDER BY created_at_ms, outbox_id
                LIMIT $3
            )
            RETURNING *
            "#,
        )
        .bind(now_ms)
        .bind(lease_ms)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Convert one claimed row into its `audit_log` row and mark it
    /// dispatched **in one transaction**. Returns `Ok(false)` when no
    /// pending row exists (already dispatched, or the id is unknown) —
    /// the insert is then skipped, so a replay cannot duplicate evidence.
    ///
    /// If the insert conflicts (e.g. a pre-existing `audit_log` row with
    /// the same `audit_id`), the transaction rolls back: the outbox row
    /// stays `pending` for a later retry. There is no `OR IGNORE`.
    pub async fn complete(&self, outbox_id: &str, now_ms: i64) -> Result<bool, StoreError> {
        let mut tx = self.pool.begin().await?;
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT audit_id, payload FROM audit_outbox \
             WHERE outbox_id = $1 AND status = 'pending'",
        )
        .bind(outbox_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((audit_id, payload)) = row else {
            return Ok(false);
        };
        let input: AuditLogInsert =
            serde_json::from_str(&payload).map_err(|e| StoreError::InvalidConfiguration {
                reason: format!("audit outbox row {outbox_id} has an unparseable payload: {e}"),
            })?;
        insert_row_on_tx(&mut tx, &audit_id, &input).await?;
        let updated = sqlx::query(
            "UPDATE audit_outbox SET \
                status = 'dispatched', dispatched_at_ms = $2, updated_at_ms = $2, \
                last_error = NULL \
             WHERE outbox_id = $1 AND status = 'pending'",
        )
        .bind(outbox_id)
        .bind(now_ms)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() == 0 {
            // Concurrent completion won the race. Roll back the inserted
            // audit row rather than duplicating evidence.
            tx.rollback().await?;
            return Ok(false);
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Record a failed dispatch and schedule the next attempt. Never
    /// marks the row dead: audit evidence retries forever with capped
    /// backoff.
    pub async fn fail_attempt(
        &self,
        outbox_id: &str,
        error: &str,
        next_attempt_at_ms: i64,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE audit_outbox SET \
                next_attempt_at_ms = $2, last_error = $3, updated_at_ms = $4 \
             WHERE outbox_id = $1 AND status = 'pending'",
        )
        .bind(outbox_id)
        .bind(next_attempt_at_ms)
        .bind(truncate_error(error))
        .bind(now_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Patch a **pending** row's outcome/status when the request
    /// completed non-2xx after its mutation committed. Returns
    /// `Ok(false)` when no pending row exists (already dispatched); the
    /// caller then logs the mismatch with the `outbox_id`.
    pub async fn finalize_pending(
        &self,
        outbox_id: &str,
        outcome: &str,
        status_code: i64,
    ) -> Result<bool, StoreError> {
        let mut tx = self.pool.begin().await?;
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT payload FROM audit_outbox WHERE outbox_id = $1 AND status = 'pending'",
        )
        .bind(outbox_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((payload,)) = row else {
            return Ok(false);
        };
        let mut input: AuditLogInsert =
            serde_json::from_str(&payload).map_err(|e| StoreError::InvalidConfiguration {
                reason: format!("audit outbox row {outbox_id} has an unparseable payload: {e}"),
            })?;
        input.outcome = outcome.to_string();
        input.status_code = status_code;
        let updated_payload =
            serde_json::to_string(&input).map_err(|e| StoreError::InvalidConfiguration {
                reason: format!("audit outbox payload is not serializable: {e}"),
            })?;
        let updated = sqlx::query(
            "UPDATE audit_outbox SET payload = $2, updated_at_ms = $3 \
             WHERE outbox_id = $1 AND status = 'pending'",
        )
        .bind(outbox_id)
        .bind(updated_payload)
        .bind(chv_common::now_unix_ms())
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() == 0 {
            tx.rollback().await?;
            return Ok(false);
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Pending row count and the age of the oldest pending row, for the
    /// dispatcher's stuck-outbox gauges. `(0, 0)` when nothing is
    /// pending.
    pub async fn pending_stats(&self, now_ms: i64) -> Result<(i64, i64), StoreError> {
        let (count, oldest_created_at): (i64, Option<i64>) = sqlx::query_as(
            "SELECT COUNT(*), MIN(created_at_ms) FROM audit_outbox WHERE status = 'pending'",
        )
        .fetch_one(&self.pool)
        .await?;
        let oldest_age_ms = match oldest_created_at {
            Some(created_at) if count > 0 => now_ms.saturating_sub(created_at),
            _ => 0,
        };
        Ok((count, oldest_age_ms))
    }

    /// Delete up to `limit` **dispatched** rows strictly older than the
    /// cutoff, oldest first. Pending rows are never pruned. This is
    /// plumbing cleanup — `audit_log` remains the evidence table.
    pub async fn prune_dispatched_before(
        &self,
        cutoff_ms: i64,
        limit: i64,
    ) -> Result<u64, StoreError> {
        let result = sqlx::query(
            "DELETE FROM audit_outbox WHERE outbox_id IN (\
             SELECT outbox_id FROM audit_outbox \
             WHERE status = 'dispatched' AND dispatched_at_ms IS NOT NULL \
               AND dispatched_at_ms < $1 \
             ORDER BY dispatched_at_ms ASC, outbox_id ASC LIMIT $2)",
        )
        .bind(cutoff_ms)
        .bind(limit)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

/// Bound a database error string before it enters the outbox row
/// (defensive: driver messages are usually short, but never unbounded).
fn truncate_error(error: &str) -> String {
    const MAX: usize = 512;
    if error.len() <= MAX {
        return error.to_string();
    }
    let mut cut = MAX;
    while cut > 0 && !error.is_char_boundary(cut) {
        cut -= 1;
    }
    error[..cut].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit_log::{insert_row_on_tx, AUDIT_OUTCOME_DENIED, AUDIT_OUTCOME_SUCCESS};
    use crate::test_util::TestDb;

    fn audit_row(occurred_at_ms: i64) -> AuditLogInsert {
        AuditLogInsert {
            occurred_at_ms,
            actor_id: "u-alice".into(),
            actor_username: "alice".into(),
            actor_role: "operator".into(),
            action: "vm.create".into(),
            method: "POST".into(),
            path: "/v1/vms".into(),
            resource_kind: Some("vm".into()),
            resource_id: Some("vm-1".into()),
            outcome: AUDIT_OUTCOME_SUCCESS.into(),
            status_code: 200,
            correlation_id: Some("corr-1".into()),
            operation_id: None,
            source_ip: Some("127.0.0.1".into()),
            metadata: r#"{"auth_source":"bearer_jwt"}"#.into(),
        }
    }

    fn outbox_input(outbox_id: &str, at_ms: i64) -> AuditOutboxInsert {
        AuditOutboxInsert {
            outbox_id: outbox_id.into(),
            audit_id: format!("audit-{outbox_id}"),
            payload: audit_row(at_ms),
            created_at_ms: at_ms,
        }
    }

    async fn outbox_count(pool: &StorePool) -> i64 {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM audit_outbox")
            .fetch_one(pool)
            .await
            .expect("count outbox");
        count
    }

    async fn audit_count(pool: &StorePool) -> i64 {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM audit_log")
            .fetch_one(pool)
            .await
            .expect("count audit");
        count
    }

    #[tokio::test]
    async fn enqueue_is_coupled_to_the_transaction() {
        let test_db = TestDb::new().await;
        let repo = AuditOutboxRepository::new(test_db.pool.clone());

        // A mutation and its outbox row commit together…
        let mut tx = test_db.pool.begin().await.expect("begin commit tx");
        sqlx::query(
            "INSERT INTO users (user_id, username, password_hash, role) \
             VALUES ('outbox-tx-user', 'outbox-tx-user', 'x', 'operator')",
        )
        .execute(&mut *tx)
        .await
        .expect("mutation");
        assert!(
            enqueue_on_tx(&mut tx, &outbox_input("ob-commit", 1_000))
                .await
                .expect("enqueue"),
            "a fresh key inserts"
        );
        tx.commit().await.expect("commit");

        assert_eq!(outbox_count(&test_db.pool).await, 1);
        let (users,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM users WHERE user_id = 'outbox-tx-user'")
                .fetch_one(&test_db.pool)
                .await
                .expect("count users");
        assert_eq!(users, 1);

        // …and roll back together: neither row survives.
        let mut tx = test_db.pool.begin().await.expect("begin rollback tx");
        sqlx::query(
            "INSERT INTO users (user_id, username, password_hash, role) \
             VALUES ('outbox-rolled-back', 'outbox-rolled-back', 'x', 'operator')",
        )
        .execute(&mut *tx)
        .await
        .expect("mutation");
        enqueue_on_tx(&mut tx, &outbox_input("ob-rolled-back", 2_000))
            .await
            .expect("enqueue");
        tx.rollback().await.expect("rollback");

        assert_eq!(outbox_count(&test_db.pool).await, 1, "no outbox row");
        let (users,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM users WHERE user_id = 'outbox-rolled-back'")
                .fetch_one(&test_db.pool)
                .await
                .expect("count rolled-back user");
        assert_eq!(users, 0, "no mutation row");
        assert!(repo.get("ob-rolled-back").await.expect("get").is_none());
    }

    #[tokio::test]
    async fn complete_inserts_and_marks_in_one_transaction() {
        let test_db = TestDb::new().await;
        let repo = AuditOutboxRepository::new(test_db.pool.clone());
        repo.enqueue(&outbox_input("ob-1", 1_000))
            .await
            .expect("enqueue");

        let claimed = repo.claim_due(1_000, 60_000, 128).await.expect("claim");
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].attempts, 1);

        assert!(repo.complete("ob-1", 2_000).await.expect("complete"));
        let row = repo.get("ob-1").await.expect("get").expect("row");
        assert_eq!(row.status, AUDIT_OUTBOX_DISPATCHED);
        assert_eq!(row.dispatched_at_ms, Some(2_000));
        assert_eq!(audit_count(&test_db.pool).await, 1);

        let (audit_id,): (String,) = sqlx::query_as("SELECT audit_id FROM audit_log")
            .fetch_one(&test_db.pool)
            .await
            .expect("audit id");
        assert_eq!(audit_id, row.audit_id, "the pre-minted id is reused");

        // A second completion is a no-op: exactly one evidence row.
        assert!(!repo.complete("ob-1", 3_000).await.expect("replay"));
        assert_eq!(audit_count(&test_db.pool).await, 1);
        assert!(repo
            .claim_due(100_000, 60_000, 128)
            .await
            .expect("claim")
            .is_empty());
    }

    #[tokio::test]
    async fn complete_conflict_rolls_back_and_leaves_row_pending() {
        let test_db = TestDb::new().await;
        let repo = AuditOutboxRepository::new(test_db.pool.clone());
        let input = outbox_input("ob-conflict", 1_000);
        repo.enqueue(&input).await.expect("enqueue");

        // An evidence row with the same audit_id already exists (e.g. a
        // manual replay or a collision). `complete` must not OR IGNORE.
        let mut tx = test_db.pool.begin().await.expect("begin");
        insert_row_on_tx(&mut tx, &input.audit_id, &input.payload)
            .await
            .expect("pre-insert conflict");
        tx.commit().await.expect("commit conflict");

        repo.claim_due(1_000, 60_000, 128).await.expect("claim");
        assert!(repo.complete("ob-conflict", 2_000).await.is_err());

        let row = repo
            .get("ob-conflict")
            .await
            .expect("get")
            .expect("pending row");
        assert_eq!(row.status, AUDIT_OUTBOX_PENDING);
        assert_eq!(row.attempts, 1, "claim bumped attempts");
        assert_eq!(audit_count(&test_db.pool).await, 1, "no duplicate");

        // Removing the conflict lets a later attempt dispatch the row.
        sqlx::query("DELETE FROM audit_log WHERE audit_id = $1")
            .bind(&input.audit_id)
            .execute(&test_db.pool)
            .await
            .expect("remove conflict");
        assert!(repo.complete("ob-conflict", 3_000).await.expect("retry"));
        assert_eq!(audit_count(&test_db.pool).await, 1);
        assert_eq!(
            repo.get("ob-conflict")
                .await
                .expect("get")
                .expect("row")
                .status,
            AUDIT_OUTBOX_DISPATCHED
        );
    }

    #[tokio::test]
    async fn claim_respects_lease_and_batch() {
        let test_db = TestDb::new().await;
        let repo = AuditOutboxRepository::new(test_db.pool.clone());
        for i in 0..2 {
            repo.enqueue(&outbox_input(&format!("ob-batch-{i}"), 1_000))
                .await
                .expect("enqueue");
        }

        let claimed = repo.claim_due(1_000, 60_000, 128).await.expect("claim");
        assert_eq!(claimed.len(), 2);
        assert!(claimed.iter().all(|row| row.attempts == 1));
        assert!(claimed.iter().all(|row| row.next_attempt_at_ms == 61_000));

        // Leased rows are unclaimable until the lease expires.
        assert!(repo
            .claim_due(60_000, 60_000, 128)
            .await
            .expect("leased")
            .is_empty());

        // New arrivals are claimed alongside the expired leases, in
        // bounded batches.
        for i in 2..5 {
            repo.enqueue(&outbox_input(&format!("ob-batch-{i}"), 1_000))
                .await
                .expect("enqueue");
        }
        let reclaimed = repo.claim_due(61_000, 60_000, 2).await.expect("reclaim");
        assert_eq!(reclaimed.len(), 2, "batch bound");
        assert!(reclaimed.iter().all(|row| row.attempts == 2));
        let rest = repo
            .claim_due(61_000, 60_000, 128)
            .await
            .expect("claim rest");
        assert_eq!(rest.len(), 3);
        assert!(rest.iter().all(|row| row.attempts == 1));
    }

    #[tokio::test]
    async fn finalize_pending_patches_only_pending_rows() {
        let test_db = TestDb::new().await;
        let repo = AuditOutboxRepository::new(test_db.pool.clone());
        repo.enqueue(&outbox_input("ob-fin", 1_000))
            .await
            .expect("enqueue");

        assert!(repo
            .finalize_pending("ob-fin", AUDIT_OUTCOME_DENIED, 403)
            .await
            .expect("finalize"));
        let row = repo.get("ob-fin").await.expect("get").expect("row");
        let payload = row.parse_payload().expect("parse");
        assert_eq!(payload.outcome, AUDIT_OUTCOME_DENIED);
        assert_eq!(payload.status_code, 403);
        assert_eq!(payload.actor_username, "alice", "attribution unchanged");

        repo.claim_due(1_000, 60_000, 128).await.expect("claim");
        assert!(repo.complete("ob-fin", 2_000).await.expect("complete"));
        assert!(
            !repo
                .finalize_pending("ob-fin", AUDIT_OUTCOME_SUCCESS, 200)
                .await
                .expect("finalize dispatched"),
            "a dispatched row is never rewritten"
        );
        let (outcome, status_code): (String, i64) =
            sqlx::query_as("SELECT outcome, status_code FROM audit_log")
                .fetch_one(&test_db.pool)
                .await
                .expect("audit row");
        assert_eq!(outcome, AUDIT_OUTCOME_DENIED);
        assert_eq!(status_code, 403);
    }

    #[tokio::test]
    async fn prune_dispatched_before_keeps_pending_rows() {
        let test_db = TestDb::new().await;
        let repo = AuditOutboxRepository::new(test_db.pool.clone());
        for (id, at) in [("ob-old", 1_000), ("ob-new", 10_000)] {
            repo.enqueue(&outbox_input(id, at)).await.expect("enqueue");
            repo.claim_due(at, 60_000, 128).await.expect("claim");
            repo.complete(id, at).await.expect("complete");
        }
        repo.enqueue(&outbox_input("ob-pending", 1_000))
            .await
            .expect("enqueue pending");

        // Old dispatched rows prune; the newer dispatched row and the
        // pending row are retained regardless of age.
        let deleted = repo
            .prune_dispatched_before(5_000, 5_000)
            .await
            .expect("prune");
        assert_eq!(deleted, 1);
        assert!(repo.get("ob-old").await.expect("get").is_none());
        assert_eq!(
            repo.get("ob-new").await.expect("get").expect("new").status,
            AUDIT_OUTBOX_DISPATCHED
        );
        assert_eq!(
            repo.get("ob-pending")
                .await
                .expect("get")
                .expect("pending")
                .status,
            AUDIT_OUTBOX_PENDING
        );

        // The batch bound is honoured.
        repo.enqueue(&outbox_input("ob-old-2", 1_000))
            .await
            .expect("enqueue");
        repo.claim_due(1_000, 60_000, 128).await.expect("claim");
        repo.complete("ob-old-2", 1_000).await.expect("complete");
        assert_eq!(
            repo.prune_dispatched_before(5_000, 0)
                .await
                .expect("bounded prune"),
            0
        );
    }

    #[tokio::test]
    async fn pending_stats_report_count_and_oldest_age() {
        let test_db = TestDb::new().await;
        let repo = AuditOutboxRepository::new(test_db.pool.clone());
        assert_eq!(repo.pending_stats(5_000).await.expect("empty"), (0, 0));

        repo.enqueue(&outbox_input("ob-1", 1_000))
            .await
            .expect("enqueue 1");
        repo.enqueue(&outbox_input("ob-2", 2_000))
            .await
            .expect("enqueue 2");
        assert_eq!(repo.pending_stats(5_000).await.expect("stats"), (2, 4_000));
    }

    #[tokio::test]
    async fn enqueue_validation_and_collisions_are_loud() {
        let test_db = TestDb::new().await;
        let repo = AuditOutboxRepository::new(test_db.pool.clone());

        let mut empty = outbox_input("", 1_000);
        empty.outbox_id.clear();
        assert!(repo.enqueue(&empty).await.is_err());

        let mut oversized = outbox_input("ob-big", 1_000);
        oversized.payload.metadata = "x".repeat(MAX_PAYLOAD_BYTES + 1);
        assert!(repo.enqueue(&oversized).await.is_err());

        assert!(repo
            .enqueue(&outbox_input("ob-dup", 1_000))
            .await
            .expect("first"));
        assert!(
            repo.enqueue(&outbox_input("ob-dup", 2_000)).await.is_err(),
            "a duplicate outbox_id is a loud error, never OR IGNORE"
        );
        assert_eq!(outbox_count(&test_db.pool).await, 1);
    }

    #[test]
    fn audit_log_insert_round_trips_through_json() {
        let input = audit_row(1_000);
        let encoded = serde_json::to_string(&input).expect("serialize");
        let decoded: AuditLogInsert = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded, input);
    }
}
