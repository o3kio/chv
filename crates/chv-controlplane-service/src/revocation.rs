//! Live node-certificate revocation snapshot (#661).
//!
//! ## Why a snapshot
//!
//! Revocation is enforced in the tonic peer-identity interceptor. Tonic
//! interceptors are synchronous, so they cannot query the store per request.
//! Instead the control plane loads `node_certificate_revocations` into an
//! immutable [`RevocationSet`] at startup and atomically swaps a new set in
//! when the table changes. The hot path is a read-lock plus a `HashSet`
//! lookup — bounded and allocation-free after the swap.
//!
//! ## Failure policy
//!
//! - **Startup (production):** if the table cannot be read, the control plane
//!   refuses to start. Serving with an unknown revocation state would silently
//!   re-enable every revoked certificate.
//! - **Refresh:** a failed read keeps the last-known-good set. Revoked serials
//!   therefore stay revoked across a store hiccup; the only impact is that a
//!   *new* revocation may take one refresh interval longer to take effect.
//! - **Insecure/dev mode:** `CHV_ALLOW_INSECURE=1` bypasses peer identity
//!   entirely, including revocation. That mode is gated behind the `dev`
//!   Cargo feature (see [`crate::validate_security_mode`]) and is never valid
//!   in a production build.
//!
//! Rotation is separately guarded in SQL (`rotate_certificate_serial`), so a
//! revoked serial cannot be exchanged for a fresh certificate even during the
//! snapshot refresh window.

use chv_controlplane_store::{CertificateRevocationRepository, StoreError};
use std::collections::HashSet;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;

/// Default interval between snapshot reloads. Small enough that an operator
/// revocation takes effect almost immediately; large enough that the query is
/// negligible. Overridable through `CHV_REVOCATION_REFRESH_SECS`.
pub const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// Environment variable overriding [`DEFAULT_REFRESH_INTERVAL`].
pub const REFRESH_INTERVAL_ENV: &str = "CHV_REVOCATION_REFRESH_SECS";

/// Immutable set loaded from the store. Swapped as a unit so readers never
/// observe a partially-loaded table.
#[derive(Debug, Default)]
struct RevocationSet {
    serials: HashSet<String>,
    loaded_at_unix_ms: i64,
}

impl RevocationSet {
    fn from_serials<I, S>(serials: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            serials: serials.into_iter().map(Into::into).collect(),
            loaded_at_unix_ms: now_unix_ms(),
        }
    }
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// Atomically refreshed, read-optimized view of the certificate denylist.
///
/// Cheap to clone (an `Arc`); every gRPC interceptor clone shares the same
/// current set. A poisoned lock is recovered with `into_inner` rather than
/// panicking: a panic in one reload task must not take the auth path down.
#[derive(Clone)]
pub struct RevocationSnapshot {
    inner: Arc<RwLock<Arc<RevocationSet>>>,
}

impl std::fmt::Debug for RevocationSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let set = self.read();
        f.debug_struct("RevocationSnapshot")
            .field("revoked_serials", &set.serials.len())
            .field("loaded_at_unix_ms", &set.loaded_at_unix_ms)
            .finish()
    }
}

impl Default for RevocationSnapshot {
    fn default() -> Self {
        Self::empty()
    }
}

impl RevocationSnapshot {
    /// Empty snapshot. Safe only for:
    /// - insecure/dev mode, where peer identity is bypassed anyway;
    /// - tests that inject serials with [`Self::from_serials`] or reload from
    ///   a test database.
    ///
    /// Production startup must use [`load_startup_snapshot`] and fail closed on
    /// error.
    pub fn empty() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Arc::new(RevocationSet::default()))),
        }
    }

    /// Build a snapshot directly from serials. Intended for tests and for
    /// callers that already hold a validated list.
    pub fn from_serials<I, S>(serials: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            inner: Arc::new(RwLock::new(Arc::new(RevocationSet::from_serials(serials)))),
        }
    }

    /// Load the full denylist from the store.
    ///
    /// The caller decides the failure policy. In production mode a startup
    /// error MUST abort startup; during refresh the previous snapshot stays
    /// active.
    pub async fn load(repo: &CertificateRevocationRepository) -> Result<Self, StoreError> {
        let rows = repo.list_all().await?;
        Ok(Self::from_serials(
            rows.into_iter().map(|row| row.certificate_serial),
        ))
    }

    /// Reload the set and swap it in atomically. Returns the number of revoked
    /// serials now active.
    ///
    /// On error the previous set is retained untouched, so revocations already
    /// known to this process remain enforced.
    pub async fn reload(
        &self,
        repo: &CertificateRevocationRepository,
    ) -> Result<usize, StoreError> {
        let rows = repo.list_all().await?;
        let set = RevocationSet::from_serials(rows.into_iter().map(|row| row.certificate_serial));
        let count = set.serials.len();
        let mut guard = self.inner.write().unwrap_or_else(|poisoned| {
            tracing::error!("revocation snapshot lock was poisoned; recovering the previous set");
            poisoned.into_inner()
        });
        *guard = Arc::new(set);
        Ok(count)
    }

    /// Whether a certificate serial is denied.
    ///
    /// An absent/empty serial is *not* treated as revoked here; the
    /// interceptor rejects missing serials separately, because a credential
    /// whose serial cannot be determined cannot be checked against the list.
    pub fn is_revoked(&self, certificate_serial: Option<&str>) -> bool {
        match certificate_serial {
            Some(serial) if !serial.is_empty() => self.read().serials.contains(serial),
            _ => false,
        }
    }

    /// Number of serials in the current set (diagnostics/readiness).
    pub fn len(&self) -> usize {
        self.read().serials.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Epoch-millisecond timestamp of the last successful load/swap. `0` for
    /// the empty initial set.
    pub fn loaded_at_unix_ms(&self) -> i64 {
        self.read().loaded_at_unix_ms
    }

    fn read(&self) -> Arc<RevocationSet> {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

/// Load the startup snapshot with the deployment's failure policy.
///
/// - **Production (`allow_insecure == false`):** returns the store error; the
///   caller MUST abort startup. Serving without knowing the denylist would
///   silently re-admit every revoked certificate.
/// - **Insecure/dev (`allow_insecure == true`):** logs and falls back to an
///   empty snapshot. This mode already bypasses peer identity/revocation and is
///   compile-time gated by the `dev` Cargo feature.
pub async fn load_startup_snapshot(
    repo: &CertificateRevocationRepository,
    allow_insecure: bool,
) -> Result<RevocationSnapshot, StoreError> {
    match RevocationSnapshot::load(repo).await {
        Ok(snapshot) => Ok(snapshot),
        Err(error) if allow_insecure => {
            tracing::warn!(
                %error,
                "CHV_ALLOW_INSECURE: certificate revocation snapshot unavailable; \
                 dev mode continues without revocation enforcement"
            );
            Ok(RevocationSnapshot::empty())
        }
        Err(error) => Err(error),
    }
}

/// Resolve the refresh interval from the environment, clamped to a sane range.
///
/// Invalid/non-positive values fall back to [`DEFAULT_REFRESH_INTERVAL`] with
/// a warning rather than failing startup: the refresh interval is not a
/// security boundary (the table remains authoritative for rotation).
pub fn refresh_interval_from_env() -> Duration {
    match std::env::var(REFRESH_INTERVAL_ENV) {
        Ok(raw) => match raw.parse::<u64>() {
            Ok(secs) if secs > 0 && secs <= 300 => Duration::from_secs(secs),
            Ok(secs) => {
                tracing::warn!(
                    value = secs,
                    "CHV_REVOCATION_REFRESH_SECS must be in 1..=300; using default"
                );
                DEFAULT_REFRESH_INTERVAL
            }
            Err(error) => {
                tracing::warn!(
                    value = %raw,
                    %error,
                    "CHV_REVOCATION_REFRESH_SECS is not an integer; using default"
                );
                DEFAULT_REFRESH_INTERVAL
            }
        },
        Err(_) => DEFAULT_REFRESH_INTERVAL,
    }
}

/// Background refresh loop. Exits when the shutdown channel closes.
///
/// A failed reload is logged and retried next tick; it never clears the
/// snapshot (see module docs). Ticks use `tokio::time::interval`, which
/// coalesces missed ticks instead of stampeding after a long stall.
pub async fn run_refresh_loop(
    repo: CertificateRevocationRepository,
    snapshot: RevocationSnapshot,
    interval: Duration,
    mut shutdown_rx: watch::Receiver<()>,
) {
    let mut ticker = tokio::time::interval(interval);
    // First tick fires immediately; the snapshot is already loaded at startup,
    // but an immediate re-read closes the window between load and server start.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_count = snapshot.len();

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                match snapshot.reload(&repo).await {
                    Ok(count) => {
                        if count < last_count {
                            // A drop in denylist size is usually a restore or a
                            // manual delete and can resurrect revoked serials.
                            // Surface it for alerting; never auto-correct.
                            metrics::counter!("chv_revocation_snapshot_shrink_total")
                                .increment(1);
                            tracing::warn!(
                                previous = last_count,
                                current = count,
                                "certificate revocation snapshot shrank; a database restore or \
                                 manual deletion may have resurrected revoked certificates. \
                                 Compare node_certificate_revocations against incident records."
                            );
                        }
                        last_count = count;
                        metrics::gauge!("chv_node_certificate_revocations_active")
                            .set(count as f64);
                        metrics::counter!(
                            "chv_revocation_snapshot_reload_total",
                            "result" => "ok",
                        )
                        .increment(1);
                        tracing::debug!(revoked_serials = count, "revocation snapshot reloaded");
                    }
                    Err(error) => {
                        metrics::counter!(
                            "chv_revocation_snapshot_reload_total",
                            "result" => "error",
                        )
                        .increment(1);
                        tracing::error!(
                            %error,
                            "failed to reload certificate revocation snapshot; \
                             keeping the last-known-good set"
                        );
                    }
                }
            }
            _ = shutdown_rx.changed() => {
                tracing::debug!("revocation refresh loop stopping");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_snapshot_revokes_nothing() {
        let snapshot = RevocationSnapshot::empty();
        assert!(!snapshot.is_revoked(Some("abc")));
        assert!(!snapshot.is_revoked(None));
        assert!(snapshot.is_empty());
    }

    #[test]
    fn snapshot_matches_exact_serial_only() {
        let snapshot = RevocationSnapshot::from_serials(["serial-a", "serial-b"]);
        assert!(snapshot.is_revoked(Some("serial-a")));
        assert!(snapshot.is_revoked(Some("serial-b")));
        assert!(!snapshot.is_revoked(Some("serial-c")));
        assert!(!snapshot.is_revoked(Some("serial-a ")));
        assert!(!snapshot.is_revoked(Some("")));
        assert_eq!(snapshot.len(), 2);
    }

    #[tokio::test]
    async fn load_and_reload_pick_up_new_revocations() {
        use chv_controlplane_store::{
            test_util::TestDb, CertificateRevocationRepository, RevokeCertificateInput,
        };

        let test_db = TestDb::new().await;
        let repo = CertificateRevocationRepository::new(test_db.pool.clone());

        let snapshot = RevocationSnapshot::load(&repo).await.expect("initial load");
        assert!(snapshot.is_empty());

        // Operator revokes while the process is running; the next reload must
        // atomically swap in the new set.
        let outcome = repo
            .revoke(&RevokeCertificateInput {
                certificate_serial: "serial-live",
                node_id: "node-a",
                reason: "compromise",
                revoked_by: Some("admin"),
                revoked_at: None,
                expires_at: None,
            })
            .await
            .expect("revoke");
        assert!(!outcome.already_revoked);

        let count = snapshot.reload(&repo).await.expect("reload");
        assert_eq!(count, 1);
        assert!(snapshot.is_revoked(Some("serial-live")));
        assert!(snapshot.loaded_at_unix_ms() > 0);
    }

    #[tokio::test]
    async fn failed_reload_keeps_last_known_good_set() {
        use chv_controlplane_store::{
            test_util::TestDb, CertificateRevocationRepository, RevokeCertificateInput,
        };

        let test_db = TestDb::new().await;
        let repo = CertificateRevocationRepository::new(test_db.pool.clone());
        repo.revoke(&RevokeCertificateInput {
            certificate_serial: "serial-kept",
            node_id: "node-a",
            reason: "compromise",
            revoked_by: None,
            revoked_at: None,
            expires_at: None,
        })
        .await
        .unwrap();

        let snapshot = RevocationSnapshot::load(&repo).await.expect("initial load");
        assert!(snapshot.is_revoked(Some("serial-kept")));

        // Force the next read to fail. A store outage must never clear the
        // denylist and silently re-admit revoked certificates.
        sqlx::query("DROP TABLE node_certificate_revocations")
            .execute(&test_db.pool)
            .await
            .unwrap();
        snapshot.reload(&repo).await.expect_err("reload must fail");

        assert!(
            snapshot.is_revoked(Some("serial-kept")),
            "failed reload must retain the previous snapshot"
        );
    }

    #[tokio::test]
    async fn startup_load_fails_closed_in_production_but_not_in_dev() {
        use chv_controlplane_store::{test_util::TestDb, CertificateRevocationRepository};

        let test_db = TestDb::new().await;
        let repo = CertificateRevocationRepository::new(test_db.pool.clone());
        sqlx::query("DROP TABLE node_certificate_revocations")
            .execute(&test_db.pool)
            .await
            .unwrap();

        let production = load_startup_snapshot(&repo, false).await;
        assert!(
            production.is_err(),
            "production startup must refuse to run with unknown revocation state"
        );

        let dev = load_startup_snapshot(&repo, true)
            .await
            .expect("dev mode documents an explicit empty-snapshot fallback");
        assert!(dev.is_empty());
    }
}
