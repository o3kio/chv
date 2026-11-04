//! Durable audit log for privileged mutations (#658).
//!
//! The BFF middleware and the node-facing gRPC lifecycle/enrollment
//! boundaries own emission; this module owns persistence, filtered
//! pagination, bounded export reads, and retention. The table is
//! deliberately independent of `users` so audit rows survive account
//! deletion, and the repository never accepts free-form metadata — the
//! caller constructs the already-redacted JSON object from an allowlist.

use crate::{StoreError, StorePool};

/// Outcome vocabulary. The BFF derives these from the HTTP status:
/// 2xx/3xx = success, 401/403 = denied, everything else = failed.
pub const AUDIT_OUTCOME_SUCCESS: &str = "success";
pub const AUDIT_OUTCOME_FAILED: &str = "failed";
pub const AUDIT_OUTCOME_DENIED: &str = "denied";

/// The actor recorded when no credential was presented or validated.
/// This is an explicit anonymous marker — never a forged identity.
pub const AUDIT_ANONYMOUS_ACTOR_ID: &str = "anonymous";
pub const AUDIT_ANONYMOUS_USERNAME: &str = "anonymous";
pub const AUDIT_ANONYMOUS_ROLE: &str = "anonymous";

const INSERT_SQL: &str = r#"
INSERT INTO audit_log (
    audit_id,
    occurred_at_ms,
    actor_id,
    actor_username,
    actor_role,
    action,
    method,
    path,
    resource_kind,
    resource_id,
    outcome,
    status_code,
    correlation_id,
    operation_id,
    source_ip,
    metadata
)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
"#;

const FILTER_WHERE: &str = r#"
WHERE ($1 IS NULL OR actor_id = $1 OR actor_username = $1)
  AND ($2 IS NULL OR resource_kind = $2)
  AND ($3 IS NULL OR resource_id = $3)
  AND ($4 IS NULL OR action = $4)
  AND ($5 IS NULL OR outcome = $5)
  AND ($6 IS NULL OR occurred_at_ms >= $6)
  AND ($7 IS NULL OR occurred_at_ms <= $7)
"#;

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct AuditLogRow {
    pub audit_id: String,
    pub occurred_at_ms: i64,
    pub actor_id: String,
    pub actor_username: String,
    pub actor_role: String,
    pub action: String,
    pub method: String,
    pub path: String,
    pub resource_kind: Option<String>,
    pub resource_id: Option<String>,
    pub outcome: String,
    pub status_code: i64,
    pub correlation_id: Option<String>,
    pub operation_id: Option<String>,
    pub source_ip: Option<String>,
    pub metadata: String,
}

/// Fields required to append one audit row. `metadata` must already be
/// the allowlisted, redacted JSON object produced by the BFF or the
/// node-facing gRPC audit boundary.
#[derive(Debug, Clone)]
pub struct AuditLogInsert {
    pub occurred_at_ms: i64,
    pub actor_id: String,
    pub actor_username: String,
    pub actor_role: String,
    pub action: String,
    pub method: String,
    pub path: String,
    pub resource_kind: Option<String>,
    pub resource_id: Option<String>,
    pub outcome: String,
    pub status_code: i64,
    pub correlation_id: Option<String>,
    pub operation_id: Option<String>,
    pub source_ip: Option<String>,
    pub metadata: String,
}

/// Admin query filters. Every field is optional; `actor` matches either
/// the stable actor id or the username. Time bounds are inclusive epoch
/// milliseconds.
#[derive(Debug, Clone, Default)]
pub struct AuditLogFilter {
    pub actor: Option<String>,
    pub resource_kind: Option<String>,
    pub resource_id: Option<String>,
    pub action: Option<String>,
    pub outcome: Option<String>,
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
}

/// Rows deleted per retention/prune statement. Bounded so a sweep of
/// a huge backlog commits in increments instead of one giant
/// transaction that holds the SQLite write lock — which would time
/// out audit inserts on the request path and turn successful
/// mutations into 500 `AUDIT_LOG_WRITE_FAILED` (#658 review M4).
const DELETE_BATCH_ROWS: i64 = 5_000;

#[derive(Clone)]
pub struct AuditLogRepository {
    pool: StorePool,
}

impl AuditLogRepository {
    pub fn new(pool: StorePool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &StorePool {
        &self.pool
    }

    /// Append exactly one audit row. The id is server-minted (128
    /// random bits — collision-safe for an append-only evidence table
    /// at any realistic volume); a PRIMARY KEY collision is a loud
    /// store error (never silently overwritten).
    pub async fn insert(&self, input: &AuditLogInsert) -> Result<String, StoreError> {
        let audit_id = chv_common::gen_resource_id();
        sqlx::query(INSERT_SQL)
            .bind(&audit_id)
            .bind(input.occurred_at_ms)
            .bind(&input.actor_id)
            .bind(&input.actor_username)
            .bind(&input.actor_role)
            .bind(&input.action)
            .bind(&input.method)
            .bind(&input.path)
            .bind(&input.resource_kind)
            .bind(&input.resource_id)
            .bind(&input.outcome)
            .bind(input.status_code)
            .bind(&input.correlation_id)
            .bind(&input.operation_id)
            .bind(&input.source_ip)
            .bind(&input.metadata)
            .execute(&self.pool)
            .await?;
        Ok(audit_id)
    }

    /// Filtered, newest-first page plus the unpaginated total.
    pub async fn query(
        &self,
        filter: &AuditLogFilter,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<AuditLogRow>, i64), StoreError> {
        let sql = format!(
            "SELECT * FROM audit_log {FILTER_WHERE} \
             ORDER BY occurred_at_ms DESC, audit_id DESC LIMIT $8 OFFSET $9"
        );
        let rows: Vec<AuditLogRow> = sqlx::query_as(&sql)
            .bind(&filter.actor)
            .bind(&filter.resource_kind)
            .bind(&filter.resource_id)
            .bind(&filter.action)
            .bind(&filter.outcome)
            .bind(filter.from_ms)
            .bind(filter.to_ms)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await?;

        let count_sql = format!("SELECT COUNT(*) FROM audit_log {FILTER_WHERE}");
        let total: (i64,) = sqlx::query_as(&count_sql)
            .bind(&filter.actor)
            .bind(&filter.resource_kind)
            .bind(&filter.resource_id)
            .bind(&filter.action)
            .bind(&filter.outcome)
            .bind(filter.from_ms)
            .bind(filter.to_ms)
            .fetch_one(&self.pool)
            .await?;
        Ok((rows, total.0))
    }

    /// Bounded export read in chronological order. The caller enforces
    /// the export ceiling; this method never returns more than `limit`
    /// rows so a filter mistake cannot exhaust process memory.
    pub async fn export(
        &self,
        filter: &AuditLogFilter,
        limit: i64,
    ) -> Result<Vec<AuditLogRow>, StoreError> {
        let sql = format!(
            "SELECT * FROM audit_log {FILTER_WHERE} \
             ORDER BY occurred_at_ms ASC, audit_id ASC LIMIT $8"
        );
        let rows: Vec<AuditLogRow> = sqlx::query_as(&sql)
            .bind(&filter.actor)
            .bind(&filter.resource_kind)
            .bind(&filter.resource_id)
            .bind(&filter.action)
            .bind(&filter.outcome)
            .bind(filter.from_ms)
            .bind(filter.to_ms)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    /// Retention sweep: delete rows strictly older than the cutoff.
    /// Returns the number of deleted rows. The cutoff is exclusive so a
    /// row exactly at the boundary is retained. Deletion runs in
    /// bounded batches (see [`DELETE_BATCH_ROWS`]) so a large backlog
    /// cannot hold the write lock in one transaction.
    pub async fn delete_older_than(&self, cutoff_ms: i64) -> Result<u64, StoreError> {
        let mut deleted = 0u64;
        loop {
            let n = self
                .delete_older_than_limited(cutoff_ms, DELETE_BATCH_ROWS)
                .await?;
            deleted += n;
            if n < DELETE_BATCH_ROWS as u64 {
                break;
            }
        }
        Ok(deleted)
    }

    /// One bounded retention step: delete the `limit` oldest rows
    /// strictly older than the cutoff. Private building block for
    /// [`Self::delete_older_than`]; the tests exercise it directly to
    /// pin the batching semantics.
    async fn delete_older_than_limited(
        &self,
        cutoff_ms: i64,
        limit: i64,
    ) -> Result<u64, StoreError> {
        let result = sqlx::query(
            "DELETE FROM audit_log WHERE audit_id IN (\
             SELECT audit_id FROM audit_log WHERE occurred_at_ms < $1 \
             ORDER BY occurred_at_ms ASC, audit_id ASC LIMIT $2)",
        )
        .bind(cutoff_ms)
        .bind(limit)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Row-count ceiling: prune the table down to its `max_rows`
    /// NEWEST rows, deleting the oldest first, in bounded batches.
    ///
    /// Age retention alone cannot bound the table when rows arrive
    /// faster than they age out (e.g. an anonymous-denial flood hitting
    /// the always-record denials contract) — this cap bounds the
    /// SQLite database regardless (#658 review M2). Returns the number
    /// of deleted rows. `max_rows <= 0` is a no-op (defensive; the
    /// config validator enforces a sane floor).
    ///
    /// The cutoff is computed **once**: the `max_rows`-th newest row's
    /// `(occurred_at_ms, audit_id)` tuple. Every batch then deletes
    /// rows strictly below that tuple, so total work is proportional
    /// to the overflow rather than re-ranking the whole table per
    /// batch (#658 review N2). The `audit_id` tie-break makes the
    /// boundary deterministic when timestamps collide.
    pub async fn prune_to_max_rows(&self, max_rows: i64) -> Result<u64, StoreError> {
        if max_rows <= 0 {
            return Ok(0);
        }
        let Some((cutoff_ms, cutoff_id)) = self.prune_cutoff(max_rows).await? else {
            return Ok(0);
        };
        let mut deleted = 0u64;
        loop {
            let n = self
                .prune_below_limited(cutoff_ms, &cutoff_id, DELETE_BATCH_ROWS)
                .await?;
            deleted += n;
            if n < DELETE_BATCH_ROWS as u64 {
                break;
            }
        }
        Ok(deleted)
    }

    /// The `(occurred_at_ms, audit_id)` of the `max_rows`-th newest
    /// row (1-indexed). `None` when the table holds fewer rows than
    /// the cap.
    async fn prune_cutoff(&self, max_rows: i64) -> Result<Option<(i64, String)>, StoreError> {
        let cutoff = sqlx::query_as(
            "SELECT occurred_at_ms, audit_id FROM audit_log \
             ORDER BY occurred_at_ms DESC, audit_id DESC LIMIT 1 OFFSET $1",
        )
        .bind(max_rows - 1)
        .fetch_optional(&self.pool)
        .await?;
        Ok(cutoff)
    }

    /// One bounded prune step: delete up to `limit` rows strictly below
    /// the `(cutoff_ms, cutoff_id)` ordering boundary, oldest first.
    /// Private building block for [`Self::prune_to_max_rows`]; the
    /// tests exercise it directly to pin batching.
    async fn prune_below_limited(
        &self,
        cutoff_ms: i64,
        cutoff_id: &str,
        limit: i64,
    ) -> Result<u64, StoreError> {
        let result = sqlx::query(
            "DELETE FROM audit_log WHERE audit_id IN (\
             SELECT audit_id FROM audit_log \
             WHERE occurred_at_ms < $1 OR (occurred_at_ms = $1 AND audit_id < $2) \
             ORDER BY occurred_at_ms ASC, audit_id ASC LIMIT $3)",
        )
        .bind(cutoff_ms)
        .bind(cutoff_id)
        .bind(limit)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TestDb;

    fn entry(
        occurred_at_ms: i64,
        actor: &str,
        action: &str,
        resource_kind: &str,
        resource_id: &str,
        outcome: &str,
    ) -> AuditLogInsert {
        AuditLogInsert {
            occurred_at_ms,
            actor_id: format!("id-{actor}"),
            actor_username: actor.to_string(),
            actor_role: "operator".to_string(),
            action: action.to_string(),
            method: "POST".to_string(),
            path: "/v1/test".to_string(),
            resource_kind: Some(resource_kind.to_string()),
            resource_id: Some(resource_id.to_string()),
            outcome: outcome.to_string(),
            status_code: if outcome == AUDIT_OUTCOME_SUCCESS {
                200
            } else {
                403
            },
            correlation_id: Some("corr-1".to_string()),
            operation_id: None,
            source_ip: Some("127.0.0.1".to_string()),
            metadata: r#"{"auth_source":"bearer_jwt"}"#.to_string(),
        }
    }

    #[tokio::test]
    async fn insert_and_filter_by_actor_resource_action_outcome_and_time() {
        let test_db = TestDb::new().await;
        let repo = AuditLogRepository::new(test_db.pool.clone());

        repo.insert(&entry(
            1_000,
            "alice",
            "vm.create",
            "vm",
            "vm-1",
            AUDIT_OUTCOME_SUCCESS,
        ))
        .await
        .unwrap();
        repo.insert(&entry(
            2_000,
            "bob",
            "vm.delete",
            "vm",
            "vm-2",
            AUDIT_OUTCOME_DENIED,
        ))
        .await
        .unwrap();
        repo.insert(&entry(
            3_000,
            "alice",
            "network.update",
            "network",
            "net-1",
            AUDIT_OUTCOME_FAILED,
        ))
        .await
        .unwrap();

        let all = AuditLogFilter::default();
        let (rows, total) = repo.query(&all, 50, 0).await.unwrap();
        assert_eq!(total, 3);
        assert_eq!(rows[0].occurred_at_ms, 3_000, "newest first");

        let by_actor = AuditLogFilter {
            actor: Some("alice".to_string()),
            ..Default::default()
        };
        let (rows, total) = repo.query(&by_actor, 50, 0).await.unwrap();
        assert_eq!(total, 2);
        assert!(rows.iter().all(|row| row.actor_username == "alice"));

        // The actor filter also accepts the stable id.
        let by_actor_id = AuditLogFilter {
            actor: Some("id-bob".to_string()),
            ..Default::default()
        };
        let (_, total) = repo.query(&by_actor_id, 50, 0).await.unwrap();
        assert_eq!(total, 1);

        let by_resource = AuditLogFilter {
            resource_kind: Some("vm".to_string()),
            resource_id: Some("vm-2".to_string()),
            ..Default::default()
        };
        let (rows, total) = repo.query(&by_resource, 50, 0).await.unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].action, "vm.delete");

        let by_action_outcome = AuditLogFilter {
            action: Some("network.update".to_string()),
            outcome: Some(AUDIT_OUTCOME_FAILED.to_string()),
            ..Default::default()
        };
        let (_, total) = repo.query(&by_action_outcome, 50, 0).await.unwrap();
        assert_eq!(total, 1);

        let by_time = AuditLogFilter {
            from_ms: Some(1_500),
            to_ms: Some(2_500),
            ..Default::default()
        };
        let (rows, total) = repo.query(&by_time, 50, 0).await.unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].occurred_at_ms, 2_000);
    }

    #[tokio::test]
    async fn query_paginates_with_stable_order() {
        let test_db = TestDb::new().await;
        let repo = AuditLogRepository::new(test_db.pool.clone());
        for i in 0..5 {
            repo.insert(&entry(
                i * 1_000,
                "alice",
                "vm.create",
                "vm",
                &format!("vm-{i}"),
                AUDIT_OUTCOME_SUCCESS,
            ))
            .await
            .unwrap();
        }

        let filter = AuditLogFilter::default();
        let (page1, total) = repo.query(&filter, 2, 0).await.unwrap();
        let (page2, _) = repo.query(&filter, 2, 2).await.unwrap();
        assert_eq!(total, 5);
        assert_eq!(page1.len(), 2);
        assert_eq!(page2.len(), 2);
        let page1_ids: Vec<&str> = page1.iter().map(|row| row.audit_id.as_str()).collect();
        let page2_ids: Vec<&str> = page2.iter().map(|row| row.audit_id.as_str()).collect();
        assert!(page1_ids.iter().all(|id| !page2_ids.contains(id)));
        assert_eq!(page1[0].occurred_at_ms, 4_000);
        assert_eq!(page2[0].occurred_at_ms, 2_000);
    }

    #[tokio::test]
    async fn export_is_chronological_and_bounded() {
        let test_db = TestDb::new().await;
        let repo = AuditLogRepository::new(test_db.pool.clone());
        for i in 0..3 {
            repo.insert(&entry(
                i * 1_000,
                "alice",
                "vm.create",
                "vm",
                &format!("vm-{i}"),
                AUDIT_OUTCOME_SUCCESS,
            ))
            .await
            .unwrap();
        }
        let rows = repo.export(&AuditLogFilter::default(), 2).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].occurred_at_ms, 0);
        assert_eq!(rows[1].occurred_at_ms, 1_000);
    }

    #[tokio::test]
    async fn retention_deletes_only_rows_older_than_cutoff() {
        let test_db = TestDb::new().await;
        let repo = AuditLogRepository::new(test_db.pool.clone());
        for at in [1_000, 2_000, 3_000] {
            repo.insert(&entry(
                at,
                "alice",
                "vm.create",
                "vm",
                &format!("vm-{at}"),
                AUDIT_OUTCOME_SUCCESS,
            ))
            .await
            .unwrap();
        }

        // Exclusive cutoff: the row at exactly 2_000 survives.
        let deleted = repo.delete_older_than(2_000).await.unwrap();
        assert_eq!(deleted, 1);
        let (rows, total) = repo.query(&AuditLogFilter::default(), 50, 0).await.unwrap();
        assert_eq!(total, 2);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.occurred_at_ms >= 2_000));
        assert!(rows
            .iter()
            .all(|row| row.resource_id.as_deref() != Some("vm-1000")));
    }

    #[tokio::test]
    async fn retention_deletion_is_batched_oldest_first_and_complete() {
        let test_db = TestDb::new().await;
        let repo = AuditLogRepository::new(test_db.pool.clone());
        // More rows than one small batch; timestamps make oldest-first
        // observable.
        for i in 0..12 {
            repo.insert(&entry(
                (i + 1) * 1_000,
                "alice",
                "vm.create",
                "vm",
                &format!("vm-{i}"),
                AUDIT_OUTCOME_SUCCESS,
            ))
            .await
            .unwrap();
        }

        // One bounded step deletes only its batch, oldest first.
        let deleted = repo.delete_older_than_limited(100_000, 5).await.unwrap();
        assert_eq!(deleted, 5);
        let (rows, total) = repo.query(&AuditLogFilter::default(), 50, 0).await.unwrap();
        assert_eq!(total, 7);
        assert_eq!(rows[rows.len() - 1].occurred_at_ms, 6_000, "oldest 5 gone");

        // The public sweep loops the batches to completion.
        let deleted = repo.delete_older_than(100_000).await.unwrap();
        assert_eq!(deleted, 7);
        let (_, total) = repo.query(&AuditLogFilter::default(), 50, 0).await.unwrap();
        assert_eq!(total, 0);
    }

    #[tokio::test]
    async fn prune_to_max_rows_keeps_the_newest_rows() {
        let test_db = TestDb::new().await;
        let repo = AuditLogRepository::new(test_db.pool.clone());
        for i in 0..10 {
            repo.insert(&entry(
                (i + 1) * 1_000,
                "alice",
                "vm.create",
                "vm",
                &format!("vm-{i}"),
                AUDIT_OUTCOME_SUCCESS,
            ))
            .await
            .unwrap();
        }

        // One bounded step deletes at most its batch, oldest first. The
        // cutoff is the cap boundary (the 3rd-newest row, 8_000).
        let (cutoff_ms, cutoff_id) = repo.prune_cutoff(3).await.unwrap().expect("cutoff");
        assert_eq!(cutoff_ms, 8_000);
        let deleted = repo
            .prune_below_limited(cutoff_ms, &cutoff_id, 2)
            .await
            .unwrap();
        assert_eq!(deleted, 2);
        let (rows, total) = repo.query(&AuditLogFilter::default(), 50, 0).await.unwrap();
        assert_eq!(total, 8);
        assert_eq!(rows[rows.len() - 1].occurred_at_ms, 3_000, "oldest 2 gone");

        // The public prune loops down to the cap, keeping the newest.
        let deleted = repo.prune_to_max_rows(3).await.unwrap();
        assert_eq!(deleted, 5);
        let (rows, total) = repo.query(&AuditLogFilter::default(), 50, 0).await.unwrap();
        assert_eq!(total, 3);
        let remaining: Vec<i64> = rows.iter().map(|r| r.occurred_at_ms).collect();
        assert_eq!(remaining, vec![10_000, 9_000, 8_000]);

        // Under the cap: no-op.
        assert_eq!(repo.prune_to_max_rows(3).await.unwrap(), 0);
        assert_eq!(repo.prune_to_max_rows(0).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn prune_cutoff_uses_audit_id_tie_break_for_equal_timestamps() {
        let test_db = TestDb::new().await;
        let repo = AuditLogRepository::new(test_db.pool.clone());
        // Five rows share the same millisecond: the cap boundary is
        // decided by `audit_id DESC`, and the kept set is deterministic.
        for i in 0..5 {
            repo.insert(&entry(
                7_000,
                "alice",
                "vm.create",
                "vm",
                &format!("vm-{i}"),
                AUDIT_OUTCOME_SUCCESS,
            ))
            .await
            .unwrap();
        }

        let (before, _) = repo.query(&AuditLogFilter::default(), 50, 0).await.unwrap();
        let expected_kept: Vec<String> =
            before.iter().take(3).map(|r| r.audit_id.clone()).collect();

        let deleted = repo.prune_to_max_rows(3).await.unwrap();
        assert_eq!(deleted, 2);

        let (after, total) = repo.query(&AuditLogFilter::default(), 50, 0).await.unwrap();
        assert_eq!(total, 3);
        let remaining: Vec<String> = after.iter().map(|r| r.audit_id.clone()).collect();
        assert_eq!(
            remaining, expected_kept,
            "tie-break keeps the first 3 in audit_id DESC order"
        );
        assert!(after.iter().all(|r| r.occurred_at_ms == 7_000));
    }
}
