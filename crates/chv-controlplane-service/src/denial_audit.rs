//! Durable audit emission for steady-state mTLS interceptor rejections (#706).
//!
//! The [`crate::peer_identity::PeerIdentityInterceptor`] runs before the
//! generated gRPC handler, so interceptor-level rejections (missing or
//! unparseable client certificate, unusable serial, revoked serial) would
//! otherwise only appear in metrics and logs. This module carries those
//! rejections, off the hot path, to a writer task that appends `audit_log`
//! rows.
//!
//! ## Fidelity limit
//!
//! Tonic inserts `tonic::extensions::GrpcMethod` only on the *client* side;
//! generated server code does not, and `tonic::Request` does not expose the
//! HTTP URI. The interceptor therefore cannot know the RPC method name. Each
//! row records the fully-qualified gRPC **service** (known at wiring time) as
//! `path`, with `action = "interceptor_denied"`. Method-level path fidelity is
//! a follow-up.
//!
//! ## Flood control
//!
//! A bounded channel keeps rejection handling non-blocking. The writer drops
//! events when the channel is full and suppresses duplicate
//! `(service, reason, peer)` rows within [`DENIAL_DEDUPE_WINDOW`]. Both are
//! reflected in metrics so an investigator can tell a quiet boundary from a
//! dropped/suppressed one.

use chv_controlplane_store::{
    AuditLogInsert, AuditLogRepository, AUDIT_ANONYMOUS_ACTOR_ID, AUDIT_ANONYMOUS_ROLE,
    AUDIT_ANONYMOUS_USERNAME, AUDIT_OUTCOME_DENIED,
};
use serde_json::json;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

/// Bounded queue between the interceptor hot path and the writer task.
pub const DENIAL_AUDIT_CHANNEL_CAPACITY: usize = 1024;

/// Operational default: identical `(service, reason, peer)` rejections are
/// recorded at most once per window. Tunable later if real traffic shows the
/// default is too coarse or too chatty.
pub const DENIAL_DEDUPE_WINDOW: Duration = Duration::from_secs(60);

/// Maximum entries retained in the dedupe map before stale entries are pruned.
const DENIAL_DEDUPE_MAX_ENTRIES: usize = 1024;

/// On shutdown, flush at most this many already-queued events before returning.
const SHUTDOWN_DRAIN_LIMIT: usize = 64;

/// Same bound as the server adapters' `capped_audit_field` so a hostile or
/// misconfigured service label cannot inflate audit rows.
const MAX_AUDIT_FIELD_LEN: usize = 256;

/// Why the steady-state interceptor rejected a node-facing request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DenialReason {
    MissingCertificate,
    UnusableSerial,
    RevokedSerial,
}

impl DenialReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MissingCertificate => "missing_certificate",
            Self::UnusableSerial => "unusable_serial",
            Self::RevokedSerial => "revoked_serial",
        }
    }
}

/// One interceptor rejection, already reduced to allowlisted, non-secret
/// fields. Never carries certificates, tokens, or message bodies.
#[derive(Clone, Debug)]
pub struct DenialEvent {
    pub service: &'static str,
    pub reason: DenialReason,
    pub source_ip: Option<String>,
    pub certificate_serial: Option<String>,
}

/// Non-blocking producer handle cloned into each gRPC service interceptor.
#[derive(Clone)]
pub struct DenialAuditSink {
    tx: mpsc::Sender<DenialEvent>,
}

impl DenialAuditSink {
    /// Create the bounded channel connecting the interceptor to
    /// [`run_writer`].
    pub fn channel() -> (Self, mpsc::Receiver<DenialEvent>) {
        let (tx, rx) = mpsc::channel(DENIAL_AUDIT_CHANNEL_CAPACITY);
        (Self { tx }, rx)
    }

    /// Queue a denial event. Never blocks and never panics: dropping a denial
    /// audit row must not change request handling or let a flood stall the
    /// listener.
    pub fn emit(&self, event: DenialEvent) {
        match self.tx.try_send(event) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                metrics::counter!(
                    "chv_grpc_denial_audit_dropped_total",
                    "cause" => "full",
                )
                .increment(1);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                metrics::counter!(
                    "chv_grpc_denial_audit_dropped_total",
                    "cause" => "closed",
                )
                .increment(1);
            }
        }
    }
}

/// Stable peer key for dedupe: certificate serial wins, then source IP, then
/// the literal `unknown`.
fn peer_key(event: &DenialEvent) -> String {
    event
        .certificate_serial
        .as_deref()
        .filter(|serial| !serial.is_empty())
        .map(str::to_string)
        .or_else(|| {
            event
                .source_ip
                .as_deref()
                .filter(|ip| !ip.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".to_string())
}

fn capped(value: &str) -> String {
    value.chars().take(MAX_AUDIT_FIELD_LEN).collect()
}

/// Build the durable row for one rejection.
///
/// `path` is the gRPC service, not the RPC method: server-side tonic does not
/// expose `GrpcMethod` (see the module-level fidelity note).
pub fn row_for(event: &DenialEvent) -> AuditLogInsert {
    let mut metadata = json!({
        "transport": "grpc",
        "auth_source": "mtls_rejected",
        "reason": event.reason.as_str(),
    });
    if let Some(serial) = event
        .certificate_serial
        .as_deref()
        .filter(|serial| !serial.is_empty())
    {
        metadata["certificate_serial"] = json!(capped(serial));
    }

    AuditLogInsert {
        occurred_at_ms: chv_common::now_unix_ms(),
        actor_id: AUDIT_ANONYMOUS_ACTOR_ID.to_string(),
        actor_username: AUDIT_ANONYMOUS_USERNAME.to_string(),
        actor_role: AUDIT_ANONYMOUS_ROLE.to_string(),
        action: "interceptor_denied".to_string(),
        method: "gRPC".to_string(),
        path: capped(event.service),
        resource_kind: None,
        resource_id: None,
        outcome: AUDIT_OUTCOME_DENIED.to_string(),
        status_code: 401,
        correlation_id: None,
        operation_id: None,
        source_ip: event.source_ip.clone(),
        metadata: metadata.to_string(),
    }
}

/// Drain denial events into `audit_log` until the channel closes or shutdown
/// is requested.
///
/// Failures are counted and logged; they never panic or feed back into request
/// handling.
pub async fn run_writer(
    repo: AuditLogRepository,
    mut rx: mpsc::Receiver<DenialEvent>,
    mut shutdown: watch::Receiver<()>,
) {
    let mut recent: HashMap<(String, DenialReason, String), Instant> = HashMap::new();

    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Some(event) => write_event(&repo, &mut recent, &event).await,
                    None => return,
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() {
                    return;
                }
                // Best effort: flush a bounded slice of already-queued
                // rejections so a clean shutdown does not lose the most
                // recent evidence, then let the task finish promptly.
                for _ in 0..SHUTDOWN_DRAIN_LIMIT {
                    match rx.try_recv() {
                        Ok(event) => write_event(&repo, &mut recent, &event).await,
                        Err(_) => break,
                    }
                }
                let leftover = rx.len() as u64;
                if leftover > 0 {
                    metrics::counter!(
                        "chv_grpc_denial_audit_dropped_total",
                        "cause" => "shutdown",
                    )
                    .increment(leftover);
                }
                return;
            }
        }
    }
}

async fn write_event(
    repo: &AuditLogRepository,
    recent: &mut HashMap<(String, DenialReason, String), Instant>,
    event: &DenialEvent,
) {
    let now = Instant::now();
    let key = (event.service.to_string(), event.reason, peer_key(event));

    if let Some(last_written) = recent.get(&key) {
        if now.duration_since(*last_written) < DENIAL_DEDUPE_WINDOW {
            metrics::counter!(
                "chv_grpc_denial_audit_suppressed_total",
                "reason" => event.reason.as_str(),
            )
            .increment(1);
            return;
        }
    }

    // Soft-bound memory under a sustained, high-cardinality rejection flood:
    // prune entries older than two windows once the map grows past the cap.
    // Growth is not hard-capped; each fresh key costs the attacker a distinct
    // TCP+TLS connection and roughly 100 B of map entry.
    if recent.len() > DENIAL_DEDUPE_MAX_ENTRIES {
        let cutoff = DENIAL_DEDUPE_WINDOW * 2;
        recent.retain(|_, last_written| now.duration_since(*last_written) <= cutoff);
    }

    let row = row_for(event);
    match repo.insert(&row).await {
        Ok(audit_id) => {
            recent.insert(key, now);
            metrics::counter!(
                "chv_grpc_denial_audit_written_total",
                "reason" => event.reason.as_str(),
            )
            .increment(1);
            tracing::info!(
                target: "audit",
                audit_id = %audit_id,
                action = %row.action,
                path = %row.path,
                status = row.status_code,
                reason = event.reason.as_str(),
                "interceptor denial audit row recorded"
            );
        }
        Err(error) => {
            metrics::counter!(
                "chv_grpc_denial_audit_write_failures_total",
                "reason" => event.reason.as_str(),
            )
            .increment(1);
            tracing::error!(
                target: "audit",
                error = %error,
                action = %row.action,
                path = %row.path,
                reason = event.reason.as_str(),
                "AUDIT LOG WRITE FAILED — mTLS interceptor denial was not durably recorded"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chv_controlplane_store::test_util::TestDb;
    use chv_controlplane_store::{AuditLogRow, StorePool};

    async fn audit_rows(pool: &StorePool) -> Vec<AuditLogRow> {
        sqlx::query_as::<_, AuditLogRow>(
            "SELECT * FROM audit_log ORDER BY occurred_at_ms ASC, audit_id ASC",
        )
        .fetch_all(pool)
        .await
        .expect("query audit rows")
    }

    fn revoked_event(serial: &str) -> DenialEvent {
        DenialEvent {
            service: "chv.controlplane.node.v1.LifecycleService",
            reason: DenialReason::RevokedSerial,
            source_ip: Some("192.0.2.10".into()),
            certificate_serial: Some(serial.into()),
        }
    }

    async fn run_events(db: &TestDb, events: Vec<DenialEvent>) -> Vec<AuditLogRow> {
        let (sink, rx) = DenialAuditSink::channel();
        let (shutdown_tx, shutdown_rx) = watch::channel(());
        let writer = tokio::spawn(run_writer(
            AuditLogRepository::new(db.pool.clone()),
            rx,
            shutdown_rx,
        ));

        for event in events {
            sink.emit(event);
        }
        drop(sink);

        tokio::time::timeout(Duration::from_secs(5), writer)
            .await
            .expect("writer task finishes after sink drop")
            .expect("writer task does not panic");
        drop(shutdown_tx);

        audit_rows(&db.pool).await
    }

    #[tokio::test]
    async fn revoked_serial_event_produces_one_anonymous_denial_row() {
        let db = TestDb::new().await;
        let rows = run_events(&db, vec![revoked_event("revoked-serial")]).await;

        assert_eq!(rows.len(), 1, "exactly one audit row");
        let row = &rows[0];
        assert_eq!(row.action, "interceptor_denied");
        assert_eq!(row.method, "gRPC");
        assert_eq!(row.path, "chv.controlplane.node.v1.LifecycleService");
        assert_eq!(row.outcome, AUDIT_OUTCOME_DENIED);
        assert_eq!(row.outcome, "denied");
        assert_eq!(row.status_code, 401);
        assert_eq!(row.actor_id, AUDIT_ANONYMOUS_ACTOR_ID);
        assert_eq!(row.actor_username, AUDIT_ANONYMOUS_USERNAME);
        assert_eq!(row.actor_role, AUDIT_ANONYMOUS_ROLE);
        assert_eq!(row.source_ip.as_deref(), Some("192.0.2.10"));
        assert_eq!(row.resource_kind, None);
        assert_eq!(row.correlation_id, None);
        assert_eq!(row.operation_id, None);

        let metadata: serde_json::Value =
            serde_json::from_str(&row.metadata).expect("metadata json");
        assert_eq!(metadata["transport"], "grpc");
        assert_eq!(metadata["auth_source"], "mtls_rejected");
        assert_eq!(metadata["reason"], "revoked_serial");
        assert_eq!(metadata["certificate_serial"], "revoked-serial");
    }

    #[tokio::test]
    async fn identical_event_inside_window_is_suppressed() {
        let db = TestDb::new().await;
        let rows = run_events(
            &db,
            vec![
                revoked_event("revoked-serial"),
                revoked_event("revoked-serial"),
            ],
        )
        .await;

        assert_eq!(rows.len(), 1, "duplicate inside the window is suppressed");
    }

    #[tokio::test]
    async fn different_reason_or_peer_key_writes_another_row() {
        let db = TestDb::new().await;
        let mut other_reason = revoked_event("revoked-serial");
        other_reason.reason = DenialReason::UnusableSerial;

        let mut other_peer = revoked_event("revoked-serial");
        other_peer.certificate_serial = Some("another-serial".into());
        other_peer.source_ip = Some("192.0.2.11".into());

        let rows = run_events(
            &db,
            vec![revoked_event("revoked-serial"), other_reason, other_peer],
        )
        .await;

        assert_eq!(
            rows.len(),
            3,
            "each distinct (service, reason, peer) key gets its own row"
        );
    }

    #[test]
    fn row_omits_empty_serial_and_caps_service() {
        let long_service = "x".repeat(MAX_AUDIT_FIELD_LEN + 32);
        // `service` is &'static str, so leak only in this unit test to exercise
        // the cap without changing the event contract.
        let service: &'static str = Box::leak(long_service.into_boxed_str());
        let row = row_for(&DenialEvent {
            service,
            reason: DenialReason::MissingCertificate,
            source_ip: None,
            certificate_serial: Some(String::new()),
        });

        assert_eq!(row.path.chars().count(), MAX_AUDIT_FIELD_LEN);
        let metadata: serde_json::Value =
            serde_json::from_str(&row.metadata).expect("metadata json");
        assert!(
            metadata.get("certificate_serial").is_none(),
            "empty serial must not be recorded"
        );
    }
}
