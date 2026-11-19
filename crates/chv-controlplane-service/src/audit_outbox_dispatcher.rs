//! Bounded background dispatcher for the transactional audit outbox
//! (#706 half 1).
//!
//! Each pass claims at most [`DISPATCH_BATCH_ROWS`] due rows, converts
//! each into its pre-minted `audit_log` row and marks it dispatched in
//! one short transaction, then records stuck-outbox gauges and prunes
//! old dispatched rows. There is no dead-letter and no attempt ceiling:
//! audit evidence is never droppable, so failures retry forever with
//! capped exponential backoff.
//!
//! The worker is deliberately model-free: it never re-derives row
//! content, so redaction happens exactly once at enqueue.
//!
//! ## Rollout status (PR 1)
//!
//! No request path enqueues yet. The worker is mounted and drains an
//! empty table; adoption lands in PR 2 (BFF) and PR 3 (gRPC).

use chv_controlplane_store::{AuditOutboxRepository, AuditOutboxRow, StoreError};
use std::sync::Arc;
use std::time::Duration;

/// Claim lease. Dispatch is a local DB transaction (one INSERT + one
/// indexed UPDATE), so 60 s is generous; a crashed dispatcher's claims
/// expire and the rows retry without a reaper.
const CLAIM_LEASE_MS: i64 = 60_000;

/// Rows claimed per pass. Far below SQLite write throughput; bounds
/// memory and each pass's duration.
const DISPATCH_BATCH_ROWS: i64 = 128;

/// Dispatched rows pruned per pass. Bounded so cleanup never holds the
/// SQLite write lock for a large backlog.
const PRUNE_BATCH_ROWS: i64 = 5_000;

/// First retry step (1 s) and ceiling (60 s). Tighter than the
/// notification curve because the destination is the local database.
const BACKOFF_BASE_MS: i64 = 1_000;
const BACKOFF_CAP_MS: i64 = 60_000;

/// SQLITE_BUSY retry floor/ceiling for the run loop. A busy database is
/// expected under write contention; back off instead of spinning.
const BUSY_BACKOFF_BASE_MS: u64 = 250;
const BUSY_BACKOFF_CAP_MS: u64 = 5_000;

/// Capped exponential retry delay before attempt `attempts >= 1`.
/// `attempts == 0` saturates to the first step so the function is total.
pub fn retry_backoff(attempts: u32) -> Duration {
    let exponent = attempts.saturating_sub(1).min(32);
    let millis = (BACKOFF_BASE_MS as u64)
        .saturating_mul(1u64 << exponent)
        .min(BACKOFF_CAP_MS as u64);
    Duration::from_millis(millis)
}

/// SQLITE_BUSY run-loop backoff after `streak` consecutive busy passes.
fn busy_backoff(streak: u32) -> Duration {
    let exponent = streak.saturating_sub(1).min(16);
    Duration::from_millis(
        BUSY_BACKOFF_BASE_MS
            .saturating_mul(1u64 << exponent)
            .min(BUSY_BACKOFF_CAP_MS),
    )
}

/// SQLITE_BUSY / SQLITE_BUSY_SNAPSHOT are expected under write
/// contention and are retried; every other store error is retried too,
/// but only busy gets the run loop's short backoff.
fn is_sqlite_busy(error: &StoreError) -> bool {
    match error {
        StoreError::Database(sqlx::Error::Database(db)) => {
            matches!(db.code().as_deref(), Some("5") | Some("517"))
                || db.message().contains("database is locked")
        }
        _ => false,
    }
}

/// Background worker draining `audit_outbox` into `audit_log`.
#[derive(Clone)]
pub struct AuditOutboxDispatcher {
    outbox: AuditOutboxRepository,
    retention_ms: i64,
    /// Backoff jitter source. Defaults to the shared ±20% draw; tests
    /// inject a deterministic one when they need exact timestamps.
    jitter: Arc<dyn Fn(Duration) -> i64 + Send + Sync>,
}

impl AuditOutboxDispatcher {
    /// `retention_ms` is how long dispatched outbox rows are kept for
    /// operator inspection before pruning (`audit.outbox_retention_days`
    /// at boot). Pending rows are never pruned.
    pub fn new(outbox: AuditOutboxRepository, retention_ms: i64) -> Self {
        Self {
            outbox,
            retention_ms: retention_ms.max(0),
            jitter: Arc::new(crate::notification_dispatcher::jitter_ms),
        }
    }

    /// Replace the backoff jitter source (tests inject deterministic
    /// draws; the default is the production ±20% jitter).
    pub fn with_jitter(mut self, jitter: Arc<dyn Fn(Duration) -> i64 + Send + Sync>) -> Self {
        self.jitter = jitter;
        self
    }

    /// Run until shutdown, one bounded pass per tick. Tokio's interval
    /// fires immediately, so the first pass is the post-crash replay
    /// pass required by the startup contract.
    pub async fn run(&self, interval: Duration, mut shutdown: tokio::sync::watch::Receiver<()>) {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut busy_streak = 0u32;
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = ticker.tick() => {}
            }
            let now_ms = chv_common::now_unix_ms();
            match self.dispatch_pass(now_ms).await {
                Ok(dispatched) => {
                    busy_streak = 0;
                    if dispatched > 0 {
                        tracing::info!(
                            target: "audit",
                            dispatched,
                            "audit outbox dispatch pass completed"
                        );
                    }
                }
                Err(error) if is_sqlite_busy(&error) => {
                    busy_streak = busy_streak.saturating_add(1);
                    let delay = busy_backoff(busy_streak);
                    tracing::warn!(
                        target: "audit",
                        error = %error,
                        backoff_ms = delay.as_millis() as u64,
                        "audit outbox dispatch hit SQLITE_BUSY; backing off"
                    );
                    tokio::select! {
                        _ = shutdown.changed() => break,
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
                Err(error) => {
                    busy_streak = 0;
                    tracing::warn!(
                        target: "audit",
                        error = %error,
                        "audit outbox dispatch pass failed; rows remain pending"
                    );
                }
            }
        }
    }

    /// One bounded pass with an injectable clock (tests never sleep).
    /// Returns the number of rows converted into `audit_log` rows.
    pub async fn dispatch_pass(&self, now_ms: i64) -> Result<usize, StoreError> {
        // Cleanup first: a large prune must not delay due evidence.
        let cutoff = now_ms.saturating_sub(self.retention_ms);
        match self
            .outbox
            .prune_dispatched_before(cutoff, PRUNE_BATCH_ROWS)
            .await
        {
            Ok(pruned) if pruned > 0 => tracing::info!(
                target: "audit",
                pruned,
                retention_ms = self.retention_ms,
                "audit outbox pruned old dispatched rows"
            ),
            Ok(_) => {}
            Err(error) => tracing::warn!(
                target: "audit",
                error = %error,
                "audit outbox prune failed; previous rows remain"
            ),
        }

        let claimed = match self
            .outbox
            .claim_due(now_ms, CLAIM_LEASE_MS, DISPATCH_BATCH_ROWS)
            .await
        {
            Ok(rows) => rows,
            Err(error) => {
                metrics::counter!("chv_audit_outbox_dispatch_failures_total").increment(1);
                return Err(error);
            }
        };

        let mut dispatched = 0usize;
        for row in &claimed {
            match self.outbox.complete(&row.outbox_id, now_ms).await {
                Ok(true) => {
                    dispatched += 1;
                    metrics::counter!("chv_audit_outbox_dispatched_total").increment(1);
                }
                Ok(false) => {
                    // The claim lease should make this impossible; if a
                    // concurrent completion won, evidence is already
                    // durable and this pass writes nothing.
                    tracing::warn!(
                        target: "audit",
                        outbox_id = %row.outbox_id,
                        "audit outbox row was no longer pending at completion"
                    );
                }
                Err(error) => {
                    metrics::counter!("chv_audit_outbox_dispatch_failures_total").increment(1);
                    let next_attempt_ms = self.retry_at(row, now_ms);
                    let busy = is_sqlite_busy(&error);
                    tracing::warn!(
                        target: "audit",
                        outbox_id = %row.outbox_id,
                        attempts = row.attempts,
                        busy,
                        next_attempt_ms,
                        error = %error,
                        "audit outbox dispatch failed; retrying forever with backoff"
                    );
                    if let Err(mark_error) = self
                        .outbox
                        .fail_attempt(&row.outbox_id, &error.to_string(), next_attempt_ms, now_ms)
                        .await
                    {
                        tracing::error!(
                            target: "audit",
                            outbox_id = %row.outbox_id,
                            error = %mark_error,
                            "audit outbox retry bookkeeping failed; row remains leased and \
                             will be reclaimed on lease expiry"
                        );
                    }
                }
            }
        }

        match self.outbox.pending_stats(now_ms).await {
            Ok((pending, oldest_age_ms)) => {
                metrics::gauge!("chv_audit_outbox_pending").set(pending as f64);
                metrics::gauge!("chv_audit_outbox_oldest_pending_ms").set(oldest_age_ms as f64);
                if pending > 0 {
                    tracing::debug!(
                        target: "audit",
                        pending,
                        oldest_age_ms,
                        "audit outbox pass found pending rows"
                    );
                }
            }
            Err(error) => tracing::warn!(
                target: "audit",
                error = %error,
                "audit outbox pending stats failed; gauges are stale"
            ),
        }

        Ok(dispatched)
    }

    fn retry_at(&self, row: &AuditOutboxRow, now_ms: i64) -> i64 {
        let attempts = row.attempts.clamp(0, u32::MAX as i64) as u32;
        let backoff = retry_backoff(attempts);
        let jitter = (self.jitter)(backoff);
        now_ms
            .saturating_add(backoff.as_millis() as i64)
            .saturating_add(jitter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chv_controlplane_store::{
        AuditLogInsert, AuditOutboxInsert, AUDIT_OUTBOX_DISPATCHED, AUDIT_OUTCOME_SUCCESS,
    };

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

    fn input(outbox_id: &str, at_ms: i64) -> AuditOutboxInsert {
        AuditOutboxInsert {
            outbox_id: outbox_id.into(),
            audit_id: format!("audit-{outbox_id}"),
            payload: audit_row(at_ms),
            created_at_ms: at_ms,
        }
    }

    fn dispatcher(repo: &AuditOutboxRepository, retention_ms: i64) -> AuditOutboxDispatcher {
        AuditOutboxDispatcher::new(repo.clone(), retention_ms).with_jitter(Arc::new(|_| 0))
    }

    async fn counts(pool: &chv_controlplane_store::StorePool) -> (i64, i64) {
        let (outbox,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM audit_outbox")
            .fetch_one(pool)
            .await
            .expect("count outbox");
        let (audit,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM audit_log")
            .fetch_one(pool)
            .await
            .expect("count audit");
        (outbox, audit)
    }

    #[tokio::test]
    async fn dispatch_writes_exactly_one_audit_row_and_second_pass_is_noop() {
        let test_db = chv_controlplane_store::test_util::TestDb::new().await;
        let repo = AuditOutboxRepository::new(test_db.pool.clone());
        let worker = dispatcher(&repo, 7 * 86_400_000);
        repo.enqueue(&input("ob-1", 1_000)).await.expect("enqueue");

        assert_eq!(worker.dispatch_pass(1_000).await.expect("pass 1"), 1);
        let row = repo.get("ob-1").await.expect("get").expect("row");
        assert_eq!(row.status, AUDIT_OUTBOX_DISPATCHED);
        assert_eq!(row.attempts, 1);
        assert_eq!(counts(&test_db.pool).await, (1, 1));

        let (audit_id,): (String,) = sqlx::query_as("SELECT audit_id FROM audit_log")
            .fetch_one(&test_db.pool)
            .await
            .expect("audit id");
        assert_eq!(audit_id, row.audit_id);

        assert_eq!(worker.dispatch_pass(2_000).await.expect("pass 2"), 0);
        assert_eq!(counts(&test_db.pool).await, (1, 1), "no duplicate row");
    }

    #[tokio::test]
    async fn claimed_but_uncompleted_rows_are_reclaimed_after_lease_expiry() {
        let test_db = chv_controlplane_store::test_util::TestDb::new().await;
        let repo = AuditOutboxRepository::new(test_db.pool.clone());
        let worker = dispatcher(&repo, 7 * 86_400_000);
        repo.enqueue(&input("ob-crash", 1_000))
            .await
            .expect("enqueue");

        // This claim *is* a dispatcher crash: attempts incremented, row
        // leased, nothing completed.
        let claimed = repo
            .claim_due(1_000, CLAIM_LEASE_MS, DISPATCH_BATCH_ROWS)
            .await
            .expect("claim");
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].attempts, 1);

        // Nothing is due while the lease is live.
        assert_eq!(worker.dispatch_pass(2_000).await.expect("leased pass"), 0);
        assert_eq!(counts(&test_db.pool).await, (1, 0));

        // Lease expiry (1 s claim + 60 s lease) makes it due again.
        assert_eq!(worker.dispatch_pass(61_000).await.expect("replay pass"), 1);
        assert_eq!(counts(&test_db.pool).await, (1, 1));

        let row = repo.get("ob-crash").await.expect("get").expect("row");
        assert_eq!(row.status, AUDIT_OUTBOX_DISPATCHED);
        assert_eq!(row.attempts, 2, "the replay is visible on the row");
    }

    #[tokio::test]
    async fn failed_completion_schedules_a_retry_and_never_dead_letters() {
        let test_db = chv_controlplane_store::test_util::TestDb::new().await;
        let repo = AuditOutboxRepository::new(test_db.pool.clone());
        let worker = dispatcher(&repo, 7 * 86_400_000);
        let pending = input("ob-conflict", 1_000);
        repo.enqueue(&pending).await.expect("enqueue");

        // Force `complete` to fail: an evidence row already owns the id.
        let mut tx = test_db.pool.begin().await.expect("begin");
        sqlx::query(
            "INSERT INTO audit_log (\
                audit_id, occurred_at_ms, actor_id, actor_username, actor_role, action, \
                method, path, resource_kind, resource_id, outcome, status_code, \
                correlation_id, operation_id, source_ip, metadata\
             ) VALUES ($1, 0, 'a', 'a', 'operator', 'vm.create', 'POST', '/v1/vms', \
                'vm', 'vm-1', 'success', 200, NULL, NULL, NULL, '{}')",
        )
        .bind(&pending.audit_id)
        .execute(&mut *tx)
        .await
        .expect("conflict row");
        tx.commit().await.expect("commit");

        assert_eq!(worker.dispatch_pass(1_000).await.expect("failing pass"), 0);
        let row = repo.get("ob-conflict").await.expect("get").expect("row");
        assert_eq!(row.status, "pending", "never dead-lettered");
        assert_eq!(row.attempts, 1);
        assert_eq!(row.last_attempt_ms, Some(1_000));
        assert_eq!(row.next_attempt_at_ms, 2_000, "1 s first backoff");
        assert!(row.last_error.is_some());
        assert_eq!(counts(&test_db.pool).await, (1, 1), "no duplicate evidence");
    }

    #[tokio::test]
    async fn dispatch_pass_prunes_dispatched_rows_after_retention() {
        let test_db = chv_controlplane_store::test_util::TestDb::new().await;
        let repo = AuditOutboxRepository::new(test_db.pool.clone());
        let worker = dispatcher(&repo, 1_000);
        repo.enqueue(&input("ob-prune", 1_000))
            .await
            .expect("enqueue");
        assert_eq!(worker.dispatch_pass(1_000).await.expect("dispatch"), 1);
        assert_eq!(counts(&test_db.pool).await, (1, 1));

        // Dispatched at 1_000; retention is 1 s. A later pass prunes it.
        assert_eq!(worker.dispatch_pass(3_000).await.expect("prune pass"), 0);
        assert_eq!(counts(&test_db.pool).await, (0, 1), "audit_log is kept");
    }

    #[test]
    fn retry_backoff_doubles_from_one_second_and_caps_at_one_minute() {
        assert_eq!(retry_backoff(0), Duration::from_millis(1_000));
        assert_eq!(retry_backoff(1), Duration::from_millis(1_000));
        assert_eq!(retry_backoff(2), Duration::from_millis(2_000));
        assert_eq!(retry_backoff(3), Duration::from_millis(4_000));
        assert_eq!(retry_backoff(6), Duration::from_millis(32_000));
        assert_eq!(retry_backoff(7), Duration::from_millis(60_000));
        assert_eq!(retry_backoff(u32::MAX), Duration::from_millis(60_000));

        let mut previous = Duration::ZERO;
        for attempts in 0..=40 {
            let delay = retry_backoff(attempts);
            assert!(delay >= previous, "backoff decreased at attempt {attempts}");
            previous = delay;
        }
    }

    #[test]
    fn busy_backoff_is_bounded() {
        assert_eq!(busy_backoff(1), Duration::from_millis(250));
        assert_eq!(busy_backoff(2), Duration::from_millis(500));
        assert_eq!(busy_backoff(5), Duration::from_millis(4_000));
        assert_eq!(busy_backoff(6), Duration::from_millis(5_000));
        assert_eq!(busy_backoff(u32::MAX), Duration::from_millis(5_000));
    }

    #[test]
    fn dispatched_outcome_constant_stays_in_sync() {
        // Guards a rename from silently detaching the dispatcher's
        // status comparison.
        assert_eq!(AUDIT_OUTBOX_DISPATCHED, "dispatched");
        let row = AuditOutboxRow {
            outbox_id: "ob".into(),
            audit_id: "a".into(),
            payload: "{}".into(),
            status: AUDIT_OUTBOX_DISPATCHED.into(),
            attempts: 1,
            next_attempt_at_ms: 0,
            last_attempt_ms: None,
            last_error: None,
            created_at_ms: 0,
            updated_at_ms: 0,
            dispatched_at_ms: Some(1),
        };
        assert_eq!(row.status, AUDIT_OUTBOX_DISPATCHED);
    }
}
