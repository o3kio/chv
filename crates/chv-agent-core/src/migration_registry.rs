//! Tracking registry for in-flight VM migration tasks.
//!
//! Background
//! ----------
//! `migrate_vm` is a fire-and-forget RPC: the agent ACKs immediately and runs
//! the actual migration on a detached `tokio::spawn`. Before this registry
//! existed, the returned `JoinHandle` was dropped on the floor, so the agent
//! had no way to:
//!
//! - **Abort on operator-issued cancel** (the control plane's
//!   `request_migration_cancel` only flips a DB flag the agent never re-checks).
//! - **Reap on agent shutdown** (in-flight tasks would be torn down abruptly
//!   by the runtime exit, with no chance to surface terminal failure to the
//!   control plane).
//! - **Observe terminal `Err`** from the spawned future (stdout-only logging
//!   was the previous "interface").
//!
//! See `docs/specs/adr/ADR-008-error-handling.md` and
//! `ADR-009-async-safety.md` for the agent-side cancel/shutdown contracts.
//!
//! Design
//! ------
//! - Each tracked migration registers an `AbortHandle` keyed by `operation_id`.
//! - A reaper task removes the entry when the future completes, regardless of
//!   `Ok` / `Err` / panic / abort.
//! - `cancel(op_id)` flips the `CancellationToken` first, waits a bounded
//!   grace window, then triggers the `AbortHandle`. The grace lets the future
//!   unwind at a phase boundary (and resume a VM it paused) instead of being
//!   dropped mid-cleanup.
//! - The registry is stored as `Arc<MigrationTaskRegistry>` inside `AgentServer`.
//!   When the last `AgentServer` clone drops, the inner `Drop` aborts every
//!   tracked task — the agent's "graceful shutdown" entry point.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::task::AbortHandle;
use tokio_util::sync::CancellationToken;

/// How long a cancel for an untracked operation is remembered. The control
/// plane's dispatch timeout is 30 s, so 10 minutes covers a `MigrateVm` that
/// was delayed long enough for the owner to cancel before the task registered.
/// Operation ids are unique, so the tombstone can never affect a later
/// legitimate migration.
const CANCELLED_OP_TOMBSTONE_TTL: Duration = Duration::from_secs(600);

/// Bound the tombstone map so a misbehaving caller cannot grow it forever.
const MAX_CANCELLED_OP_TOMBSTONES: usize = 256;

/// How long a cancelled task gets to observe the cancellation token and run
/// its cleanup (notably `PausedVmGuard::resume_if_paused`) before the registry
/// force-drops the future. Aborting immediately would strand a VM paused for
/// the disk pre-copy handshake, because the guard lives inside the task.
const DEFAULT_CANCEL_GRACE: Duration = Duration::from_secs(2);

/// How often `wait_until_stopped` re-checks the registry. The production
/// reaper removes the entry in a separate task after the future resolves, so a
/// short poll is simpler (and race-free) than a completion primitive.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// A handle for a single tracked migration.
struct TrackedTask {
    abort: AbortHandle,
    cancel_token: CancellationToken,
}

#[derive(Default)]
struct RegistryState {
    tasks: HashMap<String, TrackedTask>,
    /// Operations cancelled while no task was registered. A task that
    /// registers later (a delayed `MigrateVm`) must be aborted immediately
    /// instead of running with no control-plane owner.
    cancelled: HashMap<String, Instant>,
}

impl RegistryState {
    fn remember_cancelled(&mut self, op_id: &str) {
        let now = Instant::now();
        self.cancelled
            .retain(|_, at| now.duration_since(*at) < CANCELLED_OP_TOMBSTONE_TTL);
        if self.cancelled.len() >= MAX_CANCELLED_OP_TOMBSTONES {
            if let Some(oldest) = self
                .cancelled
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(id, _)| id.clone())
            {
                self.cancelled.remove(&oldest);
            }
        }
        self.cancelled.insert(op_id.to_string(), now);
    }

    fn take_fresh_cancelled(&mut self, op_id: &str) -> bool {
        match self.cancelled.remove(op_id) {
            Some(at) => at.elapsed() < CANCELLED_OP_TOMBSTONE_TTL,
            None => false,
        }
    }
}

/// Tracks the abort/cancel handles of every in-flight migration task.
pub struct MigrationTaskRegistry {
    inner: Mutex<RegistryState>,
    /// Cooperative-cleanup window before the hard abort (millis). Tests set a
    /// short value; production keeps [`DEFAULT_CANCEL_GRACE`].
    cancel_grace_millis: AtomicU64,
}

impl Default for MigrationTaskRegistry {
    fn default() -> Self {
        Self {
            inner: Mutex::new(RegistryState::default()),
            cancel_grace_millis: AtomicU64::new(DEFAULT_CANCEL_GRACE.as_millis() as u64),
        }
    }
}

impl MigrationTaskRegistry {
    /// Create a new, empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the cooperative-cleanup window (tests only).
    pub fn set_cancel_grace(&self, grace: Duration) {
        self.cancel_grace_millis
            .store(grace.as_millis() as u64, Ordering::Relaxed);
    }

    fn cancel_grace(&self) -> Duration {
        Duration::from_millis(self.cancel_grace_millis.load(Ordering::Relaxed))
    }

    /// Insert a tracked task. Replaces any existing entry with the same
    /// `operation_id` (and aborts the displaced task — duplicate operation_ids
    /// would otherwise leak the older handle).
    pub fn insert(&self, op_id: String, abort: AbortHandle, cancel_token: CancellationToken) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(prev) = guard.tasks.insert(
            op_id.clone(),
            TrackedTask {
                abort,
                cancel_token,
            },
        ) {
            prev.cancel_token.cancel();
            prev.abort.abort();
        }

        // A cancel may have arrived while this `MigrateVm` was still in flight
        // (the control plane timed out that RPC but the handler later
        // registered the task). Honour it now rather than leaking an unowned
        // migration.
        if guard.take_fresh_cancelled(&op_id) {
            if let Some(task) = guard.tasks.get(&op_id) {
                task.cancel_token.cancel();
                task.abort.abort();
            }
        }
    }

    /// Remove the entry for `op_id` (called by the reaper when the task ends).
    pub fn remove(&self, op_id: &str) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.tasks.remove(op_id);
    }

    /// Cancel the migration task associated with `op_id`.
    ///
    /// Flips the `CancellationToken` first so a cancel-aware future can unwind
    /// at its next phase boundary (and, on the source, best-effort resume a VM
    /// it paused). After [`DEFAULT_CANCEL_GRACE`] the `AbortHandle` force-drops
    /// the future so a future that ignores the token cannot run forever.
    ///
    /// Returns `true` if a task was found and signalled. When no task exists a
    /// bounded tombstone is recorded so a late registration is aborted too;
    /// `false` still means "the agent has no task right now" to the caller.
    pub fn cancel(&self, op_id: &str) -> bool {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.remember_cancelled(op_id);
        let Some(task) = guard.tasks.get(op_id) else {
            return false;
        };
        task.cancel_token.cancel();
        let abort = task.abort.clone();
        drop(guard);

        let grace = self.cancel_grace();
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    tokio::time::sleep(grace).await;
                    abort.abort();
                });
            }
            // No runtime (sync caller/test utility): keep the old hard-abort
            // behavior; the cooperative signal has already been sent.
            Err(_) => abort.abort(),
        }
        true
    }

    /// Wait until `op_id` is no longer tracked, up to `timeout`.
    ///
    /// `CancelMigration` uses this so a `cancelled=true` response means the
    /// task actually exited (including its cleanup), not merely that a signal
    /// was queued. The production reaper removes the entry after the future
    /// resolves; aborted futures resolve promptly, so this is bounded.
    pub async fn wait_until_stopped(&self, op_id: &str, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if !self.contains(op_id) {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(STOP_POLL_INTERVAL).await;
        }
    }

    /// Number of in-flight migration tasks.
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .tasks
            .len()
    }

    /// Whether there are no in-flight migration tasks.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether a task with `op_id` is currently tracked.
    pub fn contains(&self, op_id: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .tasks
            .contains_key(op_id)
    }

    /// Abort every tracked task. Used on agent shutdown.
    pub fn abort_all(&self) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for (op_id, task) in guard.tasks.drain() {
            tracing::warn!(
                operation_id = %op_id,
                "aborting in-flight migration task on agent shutdown"
            );
            task.cancel_token.cancel();
            task.abort.abort();
        }
    }
}

impl Drop for MigrationTaskRegistry {
    /// Best-effort: when the last `Arc<MigrationTaskRegistry>` is dropped (i.e.
    /// the last `AgentServer` clone is gone), abort any remaining tasks rather
    /// than leak them into runtime-shutdown chaos.
    fn drop(&mut self) {
        // Avoid panicking in Drop: skip if the lock is poisoned.
        if let Ok(mut guard) = self.inner.lock() {
            for (op_id, task) in guard.tasks.drain() {
                tracing::warn!(
                    operation_id = %op_id,
                    "aborting in-flight migration task on registry drop"
                );
                task.cancel_token.cancel();
                task.abort.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::oneshot;

    /// `migration_task_aborts_on_cancel_signal`: spawn a task that would block
    /// "forever", register it under an `operation_id`, then call
    /// `cancel(op_id)` and assert the task observes the abort and the registry
    /// is reaped.
    #[tokio::test]
    async fn migration_task_aborts_on_cancel_signal() {
        let registry = Arc::new(MigrationTaskRegistry::new());
        let cancel_token = CancellationToken::new();
        let op_id = "op-cancel-1".to_string();

        let started = Arc::new(tokio::sync::Notify::new());
        let started_clone = started.clone();
        let token_clone = cancel_token.clone();
        let handle = tokio::spawn(async move {
            started_clone.notify_one();
            // Block forever unless cancelled (or aborted).
            tokio::select! {
                _ = token_clone.cancelled() => Err::<(), String>("cancelled".to_string()),
                _ = tokio::time::sleep(Duration::from_secs(60)) => Ok(()),
            }
        });
        registry.insert(op_id.clone(), handle.abort_handle(), cancel_token.clone());

        // Reaper: remove on completion.
        let reg_for_reap = registry.clone();
        let reap_op = op_id.clone();
        tokio::spawn(async move {
            let _ = handle.await;
            reg_for_reap.remove(&reap_op);
        });

        started.notified().await;
        assert!(registry.contains(&op_id), "task should be tracked");
        assert_eq!(registry.len(), 1);

        let signalled = registry.cancel(&op_id);
        assert!(signalled, "cancel must return true for known op_id");
        assert!(cancel_token.is_cancelled(), "token flipped");

        // Wait up to 1s for the reaper to remove the entry.
        for _ in 0..50 {
            if !registry.contains(&op_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !registry.contains(&op_id),
            "reaper must remove cancelled task from registry"
        );
        assert_eq!(registry.len(), 0);
    }

    /// `migration_task_aborts_on_agent_shutdown`: drop the registry's owning
    /// `Arc` and assert all in-flight tasks abort within 1s.
    #[tokio::test]
    async fn migration_task_aborts_on_agent_shutdown() {
        let registry = Arc::new(MigrationTaskRegistry::new());

        // Two long-running "migration" tasks.
        let mut handles = Vec::new();
        let mut completion_rxs = Vec::new();
        for i in 0..2 {
            let op_id = format!("op-shutdown-{i}");
            let token = CancellationToken::new();
            let token_clone = token.clone();
            let (tx, rx) = oneshot::channel::<()>();
            let h = tokio::spawn(async move {
                let _guard = scopeguard_send(tx);
                tokio::select! {
                    _ = token_clone.cancelled() => {}
                    _ = tokio::time::sleep(Duration::from_secs(60)) => {}
                }
            });
            registry.insert(op_id, h.abort_handle(), token);
            handles.push(h);
            completion_rxs.push(rx);
        }

        // Yield once so the spawned tasks reach their first `.await`.
        tokio::task::yield_now().await;
        assert_eq!(registry.len(), 2);

        // Drop the only Arc — fires `Drop for MigrationTaskRegistry`, which
        // aborts all tracked tasks.
        drop(registry);

        // Assert every task ran its drop guard within 1s (proves it was
        // either cancelled cooperatively or force-aborted).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        for rx in completion_rxs {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            tokio::time::timeout(remaining, rx)
                .await
                .expect("task did not finish within 1s of registry drop")
                .expect("drop-guard channel closed without firing");
        }
    }

    /// `migration_task_terminal_err_observable`: when the spawned future
    /// returns `Err(...)`, the reaper must (1) leave a log breadcrumb and
    /// (2) clean up the registry entry. We can't assert log capture in a
    /// pure unit test, so we assert behaviour: the task runs to its `Err`,
    /// the registry entry is removed, and the surfaced `Err` value matches
    /// what the future returned (proving the JoinHandle is no longer dropped
    /// on the floor).
    #[tokio::test]
    async fn migration_task_terminal_err_observable() {
        let registry = Arc::new(MigrationTaskRegistry::new());
        let op_id = "op-err-1".to_string();
        let cancel_token = CancellationToken::new();

        let observed: Arc<Mutex<Option<Result<(), String>>>> = Arc::new(Mutex::new(None));
        let observed_clone = observed.clone();
        let handle = tokio::spawn(async move {
            // Simulate a migration future that fails terminally.
            let result: Result<(), String> = Err("test failure".to_string());
            *observed_clone.lock().unwrap() = Some(result.clone());
            result
        });
        registry.insert(op_id.clone(), handle.abort_handle(), cancel_token);

        // Reaper task — same shape we use in production code.
        let reg_for_reap = registry.clone();
        let reap_op = op_id.clone();
        let reap_handle = tokio::spawn(async move {
            let join_result = handle.await;
            // Surface the terminal Err — proves the JoinHandle is reaped.
            let surfaced = join_result.expect("task did not panic");
            reg_for_reap.remove(&reap_op);
            surfaced
        });

        let surfaced = reap_handle.await.expect("reaper joined");
        assert_eq!(
            surfaced,
            Err("test failure".to_string()),
            "terminal Err must be observable via the reaper"
        );
        assert!(
            !registry.contains(&op_id),
            "reaper must clean up on terminal Err"
        );
        assert_eq!(registry.len(), 0);
        assert_eq!(
            observed.lock().unwrap().as_ref(),
            Some(&Err("test failure".to_string())),
            "task body actually ran to completion"
        );
    }

    #[test]
    fn insert_survives_poisoned_mutex() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let registry = Arc::new(MigrationTaskRegistry::new());
            let reg2 = registry.clone();

            // Poison the mutex by panicking while holding it.
            let _ = std::panic::catch_unwind(|| {
                let _guard = reg2.inner.lock().unwrap();
                panic!("intentional poison");
            });

            // Registry must still be usable after the mutex is poisoned.
            // Obtain an AbortHandle from a real spawned task.
            let handle = tokio::spawn(async {});
            let abort_handle = handle.abort_handle();
            let ct = CancellationToken::new();
            // Should not panic:
            registry.insert("op-poison-test".to_string(), abort_handle, ct);
            assert_eq!(registry.len(), 1);
        });
    }

    /// The cancellation token must be observed before the hard abort, so a
    /// future can run its cleanup (e.g. `PausedVmGuard::resume_if_paused`).
    #[tokio::test]
    async fn cancel_runs_cooperative_cleanup_before_hard_abort() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let registry = Arc::new(MigrationTaskRegistry::new());
        // Long enough that the hard abort cannot race this test.
        registry.set_cancel_grace(Duration::from_secs(30));

        let token = CancellationToken::new();
        let cleaned = Arc::new(AtomicBool::new(false));
        let token_for_task = token.clone();
        let cleaned_for_task = cleaned.clone();
        let handle = tokio::spawn(async move {
            token_for_task.cancelled().await;
            cleaned_for_task.store(true, Ordering::SeqCst);
        });
        registry.insert("op-coop".to_string(), handle.abort_handle(), token.clone());

        let reg_for_reap = registry.clone();
        tokio::spawn(async move {
            let _ = handle.await;
            reg_for_reap.remove("op-coop");
        });

        assert!(registry.cancel("op-coop"));
        assert!(
            registry
                .wait_until_stopped("op-coop", Duration::from_secs(2))
                .await,
            "cooperative exit must be observed"
        );
        assert!(
            cleaned.load(Ordering::SeqCst),
            "cleanup must run before any force-abort"
        );
    }

    /// A future that ignores the token must not keep `CancelMigration`
    /// waiting forever: `wait_until_stopped` is bounded and returns false.
    #[tokio::test]
    async fn wait_until_stopped_times_out_for_unresponsive_task() {
        let registry = Arc::new(MigrationTaskRegistry::new());
        registry.set_cancel_grace(Duration::from_secs(60));

        let handle = tokio::spawn(std::future::pending::<()>());
        registry.insert(
            "op-wedged".to_string(),
            handle.abort_handle(),
            CancellationToken::new(),
        );
        assert!(registry.cancel("op-wedged"));
        assert!(
            !registry
                .wait_until_stopped("op-wedged", Duration::from_millis(100))
                .await,
            "a still-running task must report a timeout"
        );

        handle.abort();
        let _ = handle.await;
        registry.remove("op-wedged");
    }

    /// The tombstone is both bounded and time-limited.
    #[test]
    fn tombstone_is_bounded_and_expires() {
        let mut state = RegistryState::default();
        for i in 0..300 {
            state.remember_cancelled(&format!("op-{i}"));
        }
        assert!(
            state.cancelled.len() <= MAX_CANCELLED_OP_TOMBSTONES,
            "tombstone map must stay bounded: {}",
            state.cancelled.len()
        );

        let stale = Instant::now()
            .checked_sub(CANCELLED_OP_TOMBSTONE_TTL + Duration::from_secs(1))
            .expect("platform clock supports the test TTL");
        state.cancelled.insert("op-stale".to_string(), stale);
        assert!(
            !state.take_fresh_cancelled("op-stale"),
            "an expired tombstone must not abort a late registration"
        );

        state.remember_cancelled("op-fresh");
        assert!(state.take_fresh_cancelled("op-fresh"));
        assert!(!state.take_fresh_cancelled("op-fresh"));
    }

    /// A cancel that arrives before a delayed `MigrateVm` registers its task
    /// must not be lost: the tombstone aborts the late registration.
    #[tokio::test]
    async fn cancel_before_registration_aborts_late_task() {
        let registry = Arc::new(MigrationTaskRegistry::new());
        assert!(!registry.cancel("op-late"), "no task exists yet");

        let token = CancellationToken::new();
        let handle = tokio::spawn(std::future::pending::<()>());
        registry.insert("op-late".to_string(), handle.abort_handle(), token.clone());

        assert!(token.is_cancelled(), "tombstone must cancel the late task");
        assert!(
            handle
                .await
                .expect_err("late task must be aborted")
                .is_cancelled(),
            "late registration must be aborted, not left running"
        );
        registry.remove("op-late");
        assert!(
            registry.is_empty(),
            "the cancelled task must not be reported as live"
        );

        // A second insert for the same (already consumed) operation id is not
        // affected by a stale tombstone.
        let token = CancellationToken::new();
        let handle = tokio::spawn(std::future::pending::<()>());
        registry.insert("op-late".to_string(), handle.abort_handle(), token.clone());
        assert!(!token.is_cancelled());
        registry.remove("op-late");
        handle.abort();
    }

    /// Helper: a tiny "send on drop" guard so a spawned task can signal
    /// completion via a oneshot channel without depending on `scopeguard`.
    fn scopeguard_send(tx: oneshot::Sender<()>) -> impl Drop {
        struct G(Option<oneshot::Sender<()>>);
        impl Drop for G {
            fn drop(&mut self) {
                if let Some(tx) = self.0.take() {
                    let _ = tx.send(());
                }
            }
        }
        G(Some(tx))
    }
}
