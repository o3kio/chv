//! Notification dispatcher pure core (ADR-027, prompt 05 PR-6).
//!
//! This module holds the total, side-effect-free half of the
//! notification dispatcher described in
//! `docs/specs/contracts/chv-monitoring-query-alerts-v1.md`
//! ("Notifications and integrations") and
//! `docs/specs/component/chv-monitoring-history-alerts-spec.md`
//! ("Notification integration"):
//!
//! * [`sign_payload`] — HMAC-SHA256 webhook signatures
//!   (`v1=<64 lowercase hex>`), verifying the operator-configured
//!   secret without ever placing it in the payload.
//! * [`backoff_delay`] — capped exponential backoff between delivery
//!   attempts (no jitter; the worker layers seeded jitter on top).
//! * [`classify`] — HTTP outcome to delivery class (2xx delivered;
//!   network errors/timeouts, 5xx and 429 retryable; other 4xx
//!   permanent → dead-letter).
//! * [`render_envelope`] — the contract's versioned JSON webhook
//!   envelope (`schema_version` 1). Redaction here is structural: the
//!   payload is built from a fixed field set and can never carry
//!   tenant secrets, agent claims, command lines, plugin output or
//!   raw SQL.
//! * [`render_slack`] — the Slack incoming-webhook `{"text": "..."}`
//!   single-line adapter.
//!
//! The async worker (claim/sign/POST/mark/retry/dead-letter loop over
//! the durable `notification_outbox`) lives elsewhere in this crate
//! and composes these primitives.

use hmac::{Hmac, KeyInit, Mac};
use serde::Serialize;
use sha2::Sha256;
use std::fmt::Write as _;
use std::time::Duration;

type HmacSha256 = Hmac<Sha256>;

/// First backoff step: 5 seconds.
const BACKOFF_BASE_SECS: u64 = 5;

/// Backoff ceiling: 1 hour.
const BACKOFF_CAP_SECS: u64 = 60 * 60;

/// Slack incoming-webhook payload: a single `text` field, nothing else.
#[derive(Serialize)]
struct SlackText {
    text: String,
}

/// Sign a webhook body with the operator-configured secret.
///
/// Returns `v1=<hmac-sha256 of body keyed by secret, 64 lowercase hex
/// digits>`. The secret is never part of the output. Total over all
/// inputs (HMAC accepts keys of any length).
pub fn sign_payload(secret: &[u8], body: &[u8]) -> String {
    // `new_from_slice` is infallible for HMAC (any key length is
    // valid), so the error branch is unreachable by construction.
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(body);
    let digest = mac.finalize().into_bytes();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest.iter() {
        let _ = write!(hex, "{byte:02x}");
    }
    format!("v1={hex}")
}

/// Exponential backoff between delivery attempts, with no jitter (the
/// worker applies seeded jitter on top of this value).
///
/// `attempts` is the attempt number about to be scheduled, `>= 1` by
/// contract: step *n* waits `5s * 2^(attempts-1)`, capped at 1 hour.
/// `attempts == 0` saturates to the first step so the function stays
/// total; very large values saturate at the cap.
pub fn backoff_delay(attempts: u32) -> Duration {
    // Cap the shift so the intermediate value always fits; 5s << 64
    // already exceeds the 1h ceiling by many orders of magnitude.
    let exponent = attempts.saturating_sub(1).min(64);
    let secs = ((BACKOFF_BASE_SECS as u128) << exponent).min(BACKOFF_CAP_SECS as u128) as u64;
    Duration::from_secs(secs)
}

/// What the dispatcher should do after a delivery attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryClass {
    /// 2xx — mark delivered.
    Delivered,
    /// Network error/timeout, 5xx or 429 — schedule a retry.
    Retryable,
    /// Any other 4xx — dead-letter immediately.
    Permanent,
}

/// Classify a delivery attempt outcome.
///
/// `None` means the request never completed (connect failure, timeout,
/// TLS error) and is retryable. 2xx is delivered; 5xx and 429 are
/// retryable; every other 4xx is permanent. 1xx/3xx never occur on the
/// production path (the client follows no redirects), and are treated
/// as retryable anomalies rather than permanent failures.
pub fn classify(status: Option<u16>) -> DeliveryClass {
    match status {
        Some(code) if (200..300).contains(&code) => DeliveryClass::Delivered,
        // 429 is a 4xx, so it must be matched before the permanent arm.
        Some(429) | None => DeliveryClass::Retryable,
        Some(code) if (400..500).contains(&code) => DeliveryClass::Permanent,
        Some(_) => DeliveryClass::Retryable,
    }
}

/// Render the contract's versioned webhook envelope as compact JSON.
///
/// Field order matches the contract example (`schema_version` first,
/// `resource_url` last). Redaction is structural: only the ten
/// contract fields exist, so no caller data beyond these parameters
/// can leak into the payload. Callers validate inputs; this function
/// only renders.
///
/// The definition lives in `chv-monitoring-core::notifications` (the
/// BFF's delivery-test endpoint renders the same byte-identical
/// envelope); re-exported here for the dispatcher's callers.
pub use chv_monitoring_core::notifications::render_envelope;

/// Render the Slack incoming-webhook payload: a single human-readable
/// line wrapped as `{"text": "..."}`.
///
/// The line is `[<event_type>] <summary> (<severity>) —
/// <resource_url>`, with the resource URL rendered relative (e.g.
/// `/vms/<id>`). No other Slack blocks or fields are emitted.
pub fn render_slack(event_type: &str, severity: &str, summary: &str, resource_url: &str) -> String {
    let text = format!("[{event_type}] {summary} ({severity}) — {resource_url}");
    serde_json::to_string(&SlackText { text }).expect("slack payload serialization is infallible")
}

// ---------------------------------------------------------------------------
// Worker: claim/sign/POST/mark/retry/dead-letter over the durable
// `notification_outbox`. The outbox gives at-least-once delivery with
// idempotent enqueue; this loop adds bounded retries with capped
// exponential backoff (plus jitter) and dead-lettering.
// ---------------------------------------------------------------------------

/// Claim lease: how long a claimed-but-unfinished event stays
/// unclaimable. Must cover one full claimed batch's worst-case
/// duration (max_batch sequential POSTs at the request timeout).
const CLAIM_LEASE_MS: i64 = 300_000;

/// Request timeout for one delivery attempt.
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// Jitter fraction applied to the backoff delay (±20%).
const BACKOFF_JITTER_FRACTION: f64 = 0.2;

/// A transport failure (no HTTP response: connect, timeout, TLS).
#[derive(Debug)]
pub struct TransportError(pub String);

/// Seam for tests (the NetBox worker's client-factory pattern): one
/// authenticated POST, returning the HTTP status on response.
#[async_trait::async_trait]
pub trait NotificationTransport: Send + Sync {
    async fn post(
        &self,
        url: &str,
        body: Vec<u8>,
        signature: Option<&str>,
    ) -> Result<u16, TransportError>;
}

/// Production transport: hardened reqwest client (HTTPS-only, no
/// redirects, bounded timeouts, optional operator CA bundle).
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    /// Build the hardened client. `ca_pem` adds an operator-supplied
    /// CA for internal destinations on top of the system roots.
    pub fn new(ca_pem: Option<&str>) -> Result<Self, String> {
        let mut builder = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(DELIVERY_TIMEOUT)
            .connect_timeout(Duration::from_secs(5));
        if let Some(pem) = ca_pem {
            let certificate = reqwest::tls::Certificate::from_pem(pem.as_bytes()).map_err(|e| {
                format!("webhook_ca_path is not a valid PEM certificate bundle: {e}")
            })?;
            builder = builder.add_root_certificate(certificate);
        }
        let client = builder
            .build()
            .map_err(|e| format!("failed to build notification client: {e}"))?;
        Ok(Self { client })
    }
}

#[async_trait::async_trait]
impl NotificationTransport for ReqwestTransport {
    async fn post(
        &self,
        url: &str,
        body: Vec<u8>,
        signature: Option<&str>,
    ) -> Result<u16, TransportError> {
        let mut request = self
            .client
            .post(url)
            .header("content-type", "application/json")
            .body(body);
        if let Some(signature) = signature {
            request = request.header("x-chv-signature", signature);
        }
        let response = request
            .send()
            .await
            .map_err(|e| TransportError(redact_transport_error(&e)))?;
        Ok(response.status().as_u16())
    }
}

/// Classify a reqwest error WITHOUT the URL: reqwest's Display
/// embeds the request URL, and a Slack webhook URL carries its
/// credential in the path. The classified string is persisted in
/// `last_response` (viewer-readable) and logged — it must never
/// contain the destination or its credentials.
fn redact_transport_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "network timeout".to_string()
    } else if error.is_connect() {
        "connect failed".to_string()
    } else if error.is_request() {
        "request failed".to_string()
    } else if error.is_body() || error.is_decode() {
        "invalid response body".to_string()
    } else {
        "network error".to_string()
    }
}

#[cfg(test)]
mod transport_redaction_tests {
    use super::*;

    #[tokio::test]
    async fn transport_errors_never_expose_the_destination_url() {
        // A request to a reserved-invalid domain fails; reqwest's
        // Display for the error EMBEDS the URL (and a Slack webhook
        // URL carries its credential in the path). The redacted
        // classification must contain none of it.
        let client = reqwest::Client::builder().https_only(true).build().unwrap();
        let secret_path = "T000/B000/XXXXXXXsecretcredential";
        let error = client
            .post(format!("https://nonexistent.invalid/{secret_path}"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .expect_err("the .invalid domain cannot resolve");
        let raw = error.to_string();
        assert!(raw.contains("nonexistent.invalid"), "sanity: {raw}");
        let redacted = redact_transport_error(&error);
        assert!(
            !redacted.contains("nonexistent.invalid")
                && !redacted.contains(secret_path)
                && !redacted.contains("https://"),
            "the redacted classification must not carry the destination: {redacted}"
        );
    }
}

/// Dispatcher settings, flattened from
/// `[monitoring.notifications]` by the bootstrap.
#[derive(Clone)]
pub struct DispatcherSettings {
    pub webhook_url: Option<String>,
    /// The HMAC secret. Never logged, never placed in a payload.
    pub signing_secret: String,
    pub slack_webhook_url: Option<String>,
    pub max_attempts: u32,
    pub max_batch: i64,
}

/// Background worker delivering outbox events to the configured
/// destinations.
#[derive(Clone)]
pub struct NotificationDispatcher {
    outbox: chv_controlplane_store::NotificationOutboxRepository,
    events: chv_controlplane_store::EventRepository,
    transport: std::sync::Arc<dyn NotificationTransport>,
    settings: DispatcherSettings,
    /// Backoff jitter source (contract: seeded/testable, not the
    /// ambient global RNG). Defaults to the ±20% draw; tests inject
    /// a deterministic one.
    jitter: std::sync::Arc<dyn Fn(Duration) -> i64 + Send + Sync>,
}

impl NotificationDispatcher {
    pub fn new(
        outbox: chv_controlplane_store::NotificationOutboxRepository,
        events: chv_controlplane_store::EventRepository,
        settings: DispatcherSettings,
        ca_pem: Option<&str>,
    ) -> Result<Self, String> {
        Ok(Self::with_transport(
            outbox,
            events,
            settings,
            std::sync::Arc::new(ReqwestTransport::new(ca_pem)?),
        ))
    }

    /// Test seam: inject a fake transport.
    pub fn with_transport(
        outbox: chv_controlplane_store::NotificationOutboxRepository,
        events: chv_controlplane_store::EventRepository,
        settings: DispatcherSettings,
        transport: std::sync::Arc<dyn NotificationTransport>,
    ) -> Self {
        Self {
            outbox,
            events,
            transport,
            settings,
            jitter: std::sync::Arc::new(jitter_ms),
        }
    }

    /// Replace the backoff jitter source (tests inject deterministic
    /// draws; the default is the ±20% production jitter).
    pub fn with_jitter(
        mut self,
        jitter: std::sync::Arc<dyn Fn(Duration) -> i64 + Send + Sync>,
    ) -> Self {
        self.jitter = jitter;
        self
    }

    /// Run until shutdown, one bounded dispatch pass per tick.
    pub async fn run(&self, interval: Duration, mut shutdown: tokio::sync::watch::Receiver<()>) {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = ticker.tick() => {}
            }
            let now_ms = chrono_now_ms();
            if let Err(e) = self.dispatch_pass(now_ms).await {
                tracing::warn!(error = %e, "notification dispatch pass failed");
            }
        }
    }

    /// One bounded pass: claim due events, deliver each, and record
    /// the outcome. Failures of individual deliveries never fail the
    /// pass (they schedule retries); only store errors do.
    pub async fn dispatch_pass(
        &self,
        now_ms: i64,
    ) -> Result<usize, chv_controlplane_store::StoreError> {
        let claimed = self
            .outbox
            .claim_due(now_ms, CLAIM_LEASE_MS, self.settings.max_batch)
            .await?;
        for event in &claimed {
            if let Err(e) = self.deliver(event, now_ms).await {
                tracing::warn!(
                    event_id = %event.event_id,
                    error = %e,
                    "notification delivery bookkeeping failed"
                );
            }
        }
        Ok(claimed.len())
    }

    async fn deliver(
        &self,
        event: &chv_controlplane_store::OutboxEventRow,
        now_ms: i64,
    ) -> Result<(), chv_controlplane_store::StoreError> {
        let (url, body, signature) = match event.channel.as_str() {
            chv_controlplane_store::CHANNEL_WEBHOOK => {
                let Some(url) = self.settings.webhook_url.clone() else {
                    // The operator removed the destination after the
                    // event was enqueued: dead-letter honestly.
                    return self
                        .dead_letter(
                            event,
                            now_ms,
                            "webhook destination removed from configuration",
                        )
                        .await;
                };
                let body = event.payload.clone().into_bytes();
                let signature = sign_payload(self.settings.signing_secret.as_bytes(), &body);
                (url, body, Some(signature))
            }
            chv_controlplane_store::CHANNEL_SLACK => {
                let Some(url) = self.settings.slack_webhook_url.clone() else {
                    return self
                        .dead_letter(
                            event,
                            now_ms,
                            "slack destination removed from configuration",
                        )
                        .await;
                };
                let resource_url = format!("/{}s/{}", event.target_kind, event.target_id);
                let body = render_slack(
                    &event.event_type,
                    &event.severity,
                    &event.summary,
                    &resource_url,
                )
                .into_bytes();
                // Slack's incoming-webhook URL is its own credential;
                // our HMAC header would be meaningless there.
                (url, body, None)
            }
            other => {
                return self
                    .dead_letter(event, now_ms, &format!("unknown channel {other:?}"))
                    .await;
            }
        };

        let outcome = self.transport.post(&url, body, signature.as_deref()).await;
        match outcome {
            Ok(status) => match classify(Some(status)) {
                DeliveryClass::Delivered => {
                    self.outbox
                        .mark_delivered(&event.event_id, now_ms, &format!("{status}"))
                        .await?;
                    tracing::info!(
                        event_id = %event.event_id,
                        status,
                        "notification delivered"
                    );
                }
                DeliveryClass::Retryable => {
                    self.retry_or_dead(event, now_ms, &format!("{status}"))
                        .await?;
                }
                DeliveryClass::Permanent => {
                    self.dead_letter(event, now_ms, &format!("http {status}"))
                        .await?;
                }
            },
            Err(e) => {
                self.retry_or_dead(event, now_ms, &format!("transport: {}", e.0))
                    .await?;
            }
        }
        Ok(())
    }

    async fn retry_or_dead(
        &self,
        event: &chv_controlplane_store::OutboxEventRow,
        now_ms: i64,
        note: &str,
    ) -> Result<(), chv_controlplane_store::StoreError> {
        let attempts = event.attempts.max(0) as u32;
        if attempts >= self.settings.max_attempts {
            return self.dead_letter(event, now_ms, note).await;
        }
        let backoff = backoff_delay(attempts + 1);
        let jitter = (self.jitter)(backoff);
        let next_attempt_at_ms = now_ms + backoff.as_millis() as i64 + jitter;
        self.outbox
            .schedule_retry(&event.event_id, next_attempt_at_ms, now_ms, note)
            .await?;
        tracing::warn!(
            event_id = %event.event_id,
            attempts,
            note,
            "notification delivery failed; retry scheduled"
        );
        Ok(())
    }

    /// Dead-letter an event, audit it durably, and (once — never for
    /// a `delivery_failed` event itself, no recursion) enqueue a
    /// `delivery_failed` notification so the outage is visible to
    /// whoever eventually receives the surviving channel.
    async fn dead_letter(
        &self,
        event: &chv_controlplane_store::OutboxEventRow,
        now_ms: i64,
        note: &str,
    ) -> Result<(), chv_controlplane_store::StoreError> {
        self.outbox
            .dead_letter(&event.event_id, now_ms, note)
            .await?;
        tracing::error!(
            event_id = %event.event_id,
            alert_id = %event.alert_id,
            note,
            "notification dead-lettered"
        );
        self.audit_dead_letter(event, now_ms, note).await;
        if event.event_type != chv_controlplane_store::EVENT_TYPE_DELIVERY_FAILED {
            // Route the courtesy notice to a channel that can still
            // deliver it: prefer a channel other than the dead one
            // (its destination may be gone or failing), fall back to
            // the dead channel when it is the only configured one,
            // and skip the enqueue entirely when no destination
            // remains (the durable audit event still records the
            // failure — a courtesy event nothing can deliver would
            // just be a second dead letter).
            match self.courtesy_channel(&event.channel) {
                Some(channel) => {
                    let failure_event_id = uuid::Uuid::new_v4().to_string();
                    let summary = format!(
                        "notification delivery failed permanently: {}",
                        truncate_for_summary(note)
                    );
                    let payload = render_envelope(
                        &failure_event_id,
                        &event.alert_id,
                        chv_controlplane_store::EVENT_TYPE_DELIVERY_FAILED,
                        &event.severity,
                        &event.target_kind,
                        &event.target_id,
                        &summary,
                        event.occurred_at_ms,
                        &format!("/{}s/{}", event.target_kind, event.target_id),
                    );
                    // Best-effort: the outbox insert is idempotent by
                    // event id, so a crash between dead-letter and
                    // enqueue simply loses this courtesy event, never
                    // duplicates it.
                    let _ = self
                        .outbox
                        .enqueue(&chv_controlplane_store::NotificationEventInput {
                            event_id: failure_event_id,
                            alert_id: event.alert_id.clone(),
                            incident_key: event.incident_key.clone(),
                            event_type: chv_controlplane_store::EVENT_TYPE_DELIVERY_FAILED
                                .to_string(),
                            severity: event.severity.clone(),
                            target_kind: event.target_kind.clone(),
                            target_id: event.target_id.clone(),
                            summary,
                            occurred_at_ms: now_ms,
                            payload,
                            channel,
                        })
                        .await;
                }
                None => {
                    tracing::warn!(
                        event_id = %event.event_id,
                        "no configured destination can deliver the delivery_failed courtesy \
                         event; it is recorded in the audit trail only"
                    );
                }
            }
        }
        Ok(())
    }

    /// The channel for a dead-letter courtesy event (see
    /// [`Self::dead_letter`]): a configured channel other than the
    /// dead one, else the dead one if still configured, else none.
    fn courtesy_channel(&self, dead_channel: &str) -> Option<String> {
        let is_configured = |channel: &str| match channel {
            chv_controlplane_store::CHANNEL_WEBHOOK => self.settings.webhook_url.is_some(),
            _ => self.settings.slack_webhook_url.is_some(),
        };
        [
            chv_controlplane_store::CHANNEL_WEBHOOK,
            chv_controlplane_store::CHANNEL_SLACK,
        ]
        .into_iter()
        .filter(|channel| is_configured(channel))
        .find(|channel| channel != &dead_channel)
        .map(str::to_string)
        .or_else(|| is_configured(dead_channel).then(|| dead_channel.to_string()))
    }

    /// Best-effort durable audit trail (mirrors the monitoring-agent
    /// audit helper; a failure here never blocks delivery bookkeeping).
    async fn audit_dead_letter(
        &self,
        event: &chv_controlplane_store::OutboxEventRow,
        now_ms: i64,
        note: &str,
    ) {
        use chv_controlplane_store::EventAppendInput;
        use chv_controlplane_types::domain::{ActorId, EventSeverity, EventType};
        let input = EventAppendInput {
            occurred_unix_ms: now_ms,
            event_type: EventType::Audit,
            severity: EventSeverity::Warning,
            resource_kind: None,
            resource_id: None,
            node_id: None,
            operation_id: None,
            actor_id: ActorId::new("system:notification-dispatcher").ok(),
            requested_by: Some("system:notification-dispatcher".to_string()),
            correlation_id: Some(event.event_id.clone()),
            message: format!(
                "notification {} for incident {} dead-lettered: {}",
                event.event_type, event.incident_key, note
            ),
            details: Some(
                serde_json::json!({
                    "event": "monitoring.notification.dead_letter",
                    "channel": event.channel,
                    "attempts": event.attempts,
                })
                .to_string(),
            ),
        };
        if let Err(e) = self.events.append(&input).await {
            tracing::warn!(error = %e, "notification dead-letter audit append failed");
        }
    }
}

/// Wall-clock epoch milliseconds (the worker's clock; tests inject
/// times via [`NotificationDispatcher::dispatch_pass`]).
fn chrono_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// ±20% jitter on a backoff delay, in milliseconds. The magnitude
/// never exceeds 20% of the backoff, so the net delay (backoff +
/// jitter) always stays positive. Shared by the notification
/// dispatcher and the monitoring export worker (interface D4: the
/// export retry uses the same jitter so N managers against one
/// recovering receiver do not retry in lockstep).
pub(crate) fn jitter_ms(backoff: Duration) -> i64 {
    use rand::RngExt;
    let base = backoff.as_millis() as f64;
    let spread = base * BACKOFF_JITTER_FRACTION;
    // random() is [0,1): map to [-1,1) and scale.
    let drawn: f64 = (rand::rng().random::<f64>() * 2.0 - 1.0) * spread;
    drawn as i64
}

/// Bound a note before it enters a summary (summaries are capped at
/// 512 bytes by the outbox validation).
fn truncate_for_summary(note: &str) -> String {
    const MAX: usize = 400;
    if note.len() <= MAX {
        return note.to_string();
    }
    let mut cut = MAX;
    while cut > 0 && !note.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &note[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use sha2::Digest;

    /// Independent HMAC-SHA256 built directly on sha2 (RFC 2104
    /// ipad/opad construction), so the vectors below are not simply
    /// re-deriving `sign_payload` through the same `hmac` code path.
    fn hmac_sha256_reference(key: &[u8], message: &[u8]) -> [u8; 32] {
        const BLOCK: usize = 64;
        let mut key_block = [0u8; BLOCK];
        if key.len() > BLOCK {
            let hashed = Sha256::digest(key);
            key_block[..hashed.len()].copy_from_slice(&hashed);
        } else {
            key_block[..key.len()].copy_from_slice(key);
        }
        let mut ipad = [0x36u8; BLOCK];
        let mut opad = [0x5cu8; BLOCK];
        for i in 0..BLOCK {
            ipad[i] ^= key_block[i];
            opad[i] ^= key_block[i];
        }
        let mut inner = Sha256::new();
        inner.update(ipad);
        inner.update(message);
        let inner_digest = inner.finalize();
        let mut outer = Sha256::new();
        outer.update(opad);
        outer.update(inner_digest);
        let digest = outer.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        out
    }

    fn hex_string(bytes: &[u8]) -> String {
        let mut hex = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }

    #[test]
    fn sign_payload_matches_rfc4231_vector() {
        // RFC 4231 test case 2 (verified independently of this crate).
        assert_eq!(
            sign_payload(b"Jefe", b"what do ya want for nothing?"),
            "v1=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn sign_payload_matches_reference_implementation() {
        for (secret, body) in [
            (&b"Jefe"[..], &b"what do ya want for nothing?"[..]),
            (&b""[..], &b""[..]),
            (&b"0123456789abcdef"[..], &b"\x00\x01\x02\xff"[..]),
            (&b"sixteen-byte-key!"[..], &b"repeat repeat repeat"[..]),
            // Key longer than the 64-byte block (must be hashed down).
            (&[0x42u8; 100][..], &b"long key"[..]),
        ] {
            let expected = hex_string(&hmac_sha256_reference(secret, body));
            assert_eq!(sign_payload(secret, body), format!("v1={expected}"));
        }
    }

    #[test]
    fn sign_payload_shape_is_v1_lowercase_hex_64() {
        let signed = sign_payload(b"any-secret", b"any-body");
        assert!(signed.starts_with("v1="));
        let hex_part = &signed[3..];
        assert_eq!(hex_part.len(), 64);
        assert!(
            hex_part
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "signature must be lowercase hex, got {hex_part}"
        );
        // Same inputs must be deterministic; different body must not.
        assert_eq!(signed, sign_payload(b"any-secret", b"any-body"));
        assert_ne!(signed, sign_payload(b"any-secret", b"any-body-2"));
        assert_ne!(signed, sign_payload(b"other-secret", b"any-body"));
    }

    #[test]
    fn backoff_delay_follows_capped_exponential_sequence() {
        assert_eq!(backoff_delay(1), Duration::from_secs(5));
        assert_eq!(backoff_delay(2), Duration::from_secs(10));
        assert_eq!(backoff_delay(3), Duration::from_secs(20));
        assert_eq!(backoff_delay(4), Duration::from_secs(40));
        assert_eq!(backoff_delay(5), Duration::from_secs(80));
        assert_eq!(backoff_delay(6), Duration::from_secs(160));
        assert_eq!(backoff_delay(7), Duration::from_secs(320));
        assert_eq!(backoff_delay(8), Duration::from_secs(640));
        assert_eq!(backoff_delay(9), Duration::from_secs(1280));
        assert_eq!(backoff_delay(10), Duration::from_secs(2560));
    }

    #[test]
    fn backoff_delay_caps_at_one_hour() {
        // 5s * 2^10 = 5120s would exceed the 1h cap.
        assert_eq!(backoff_delay(11), Duration::from_secs(3600));
        assert_eq!(backoff_delay(20), Duration::from_secs(3600));
        assert_eq!(backoff_delay(u32::MAX), Duration::from_secs(3600));
        // Monotone non-decreasing across the whole range.
        let mut previous = Duration::ZERO;
        for attempts in 1..=40u32 {
            let delay = backoff_delay(attempts);
            assert!(delay >= previous, "delay decreased at attempt {attempts}");
            previous = delay;
        }
    }

    #[test]
    fn backoff_delay_is_total_below_one() {
        // Contract says attempts >= 1; 0 must not panic and saturates
        // to the first step.
        assert_eq!(backoff_delay(0), Duration::from_secs(5));
    }

    #[test]
    fn classify_delivered_for_2xx() {
        assert_eq!(classify(Some(200)), DeliveryClass::Delivered);
        assert_eq!(classify(Some(201)), DeliveryClass::Delivered);
        assert_eq!(classify(Some(204)), DeliveryClass::Delivered);
        assert_eq!(classify(Some(299)), DeliveryClass::Delivered);
    }

    #[test]
    fn classify_permanent_for_other_4xx() {
        assert_eq!(classify(Some(400)), DeliveryClass::Permanent);
        assert_eq!(classify(Some(401)), DeliveryClass::Permanent);
        assert_eq!(classify(Some(403)), DeliveryClass::Permanent);
        assert_eq!(classify(Some(404)), DeliveryClass::Permanent);
        assert_eq!(classify(Some(410)), DeliveryClass::Permanent);
        assert_eq!(classify(Some(499)), DeliveryClass::Permanent);
    }

    #[test]
    fn classify_retryable_for_none_5xx_and_429() {
        assert_eq!(classify(None), DeliveryClass::Retryable);
        assert_eq!(classify(Some(429)), DeliveryClass::Retryable);
        assert_eq!(classify(Some(500)), DeliveryClass::Retryable);
        assert_eq!(classify(Some(502)), DeliveryClass::Retryable);
        assert_eq!(classify(Some(503)), DeliveryClass::Retryable);
        assert_eq!(classify(Some(599)), DeliveryClass::Retryable);
    }

    #[test]
    fn classify_retryable_for_non_2xx_non_4xx_anomalies() {
        // 1xx/3xx cannot occur on the production path (no redirects);
        // they are treated as transient, never permanent.
        assert_eq!(classify(Some(100)), DeliveryClass::Retryable);
        assert_eq!(classify(Some(301)), DeliveryClass::Retryable);
        assert_eq!(classify(Some(304)), DeliveryClass::Retryable);
    }

    #[test]
    fn render_envelope_matches_contract_example_exactly() {
        // Values from the contract example in
        // docs/specs/contracts/chv-monitoring-query-alerts-v1.md.
        let rendered = render_envelope(
            "d0b2a2f7-2a77-4f86-ae55-53f0c8bf4aac",
            "f4528846-4925-435e-a69a-a4a147501527",
            "firing",
            "warning",
            "vm",
            "83dab870-4903-48a9-9d37-486e100ed009",
            "Guest filesystem nearly full",
            1791576000000,
            "/vms/83dab870-4903-48a9-9d37-486e100ed009",
        );
        // Exact compact string, field order per the contract example.
        let expected = concat!(
            r#"{"schema_version":1,"#,
            r#""event_id":"d0b2a2f7-2a77-4f86-ae55-53f0c8bf4aac","#,
            r#""incident_id":"f4528846-4925-435e-a69a-a4a147501527","#,
            r#""event_type":"firing","#,
            r#""severity":"warning","#,
            r#""target_kind":"vm","#,
            r#""target_id":"83dab870-4903-48a9-9d37-486e100ed009","#,
            r#""summary":"Guest filesystem nearly full","#,
            r#""occurred_at_ms":1791576000000,"#,
            r#""resource_url":"/vms/83dab870-4903-48a9-9d37-486e100ed009"}"#
        );
        assert_eq!(rendered, expected);

        // Same conclusion via parsed Values (shape equality).
        let parsed: Value = serde_json::from_str(&rendered).expect("envelope must be valid JSON");
        let expected_value = serde_json::json!({
            "schema_version": 1,
            "event_id": "d0b2a2f7-2a77-4f86-ae55-53f0c8bf4aac",
            "incident_id": "f4528846-4925-435e-a69a-a4a147501527",
            "event_type": "firing",
            "severity": "warning",
            "target_kind": "vm",
            "target_id": "83dab870-4903-48a9-9d37-486e100ed009",
            "summary": "Guest filesystem nearly full",
            "occurred_at_ms": 1791576000000i64,
            "resource_url": "/vms/83dab870-4903-48a9-9d37-486e100ed009"
        });
        assert_eq!(parsed, expected_value);
    }

    #[test]
    fn render_envelope_field_order_and_closed_field_set() {
        let rendered = render_envelope(
            "event",
            "incident",
            "resolved",
            "critical",
            "node",
            "node-1",
            "Node unreachable",
            -1,
            "/nodes/node-1",
        );
        // Field order must follow the contract example.
        let expected_order = [
            "schema_version",
            "event_id",
            "incident_id",
            "event_type",
            "severity",
            "target_kind",
            "target_id",
            "summary",
            "occurred_at_ms",
            "resource_url",
        ];
        let positions: Vec<Option<usize>> = expected_order
            .iter()
            .map(|field| rendered.find(&format!("\"{field}\":")))
            .collect();
        assert!(
            positions.iter().all(Option::is_some),
            "missing field in {rendered}"
        );
        let mut sorted = positions.clone();
        sorted.sort();
        assert_eq!(
            positions, sorted,
            "fields out of contract order: {rendered}"
        );

        // Structural redaction: exactly the ten contract fields.
        let parsed: Value = serde_json::from_str(&rendered).expect("must be valid JSON");
        let object = parsed.as_object().expect("envelope must be an object");
        assert_eq!(object.len(), 10);
        // Negative timestamps pass through untouched (callers validate).
        assert_eq!(object["occurred_at_ms"], serde_json::json!(-1));
    }

    #[test]
    fn render_envelope_escapes_untrusted_strings() {
        // Summary is rule-operator text; it must be JSON-escaped, never
        // able to break out of the envelope or inject fields.
        let rendered = render_envelope(
            "e",
            "i",
            "firing",
            "warning",
            "vm",
            "vm-1",
            "quote \" backslash \\ newline\n brace } ctrl\u{1}",
            0,
            "/vms/vm-1",
        );
        let parsed: Value = serde_json::from_str(&rendered).expect("must stay valid JSON");
        let object = parsed.as_object().expect("must stay an object");
        assert_eq!(object.len(), 10);
        assert_eq!(
            object["summary"],
            serde_json::json!("quote \" backslash \\ newline\n brace } ctrl\u{1}")
        );
    }

    #[test]
    fn render_slack_is_single_text_line() {
        let rendered = render_slack(
            "firing",
            "warning",
            "Guest filesystem nearly full",
            "/vms/83dab870-4903-48a9-9d37-486e100ed009",
        );
        assert_eq!(
            rendered,
            "{\"text\":\"[firing] Guest filesystem nearly full (warning) — \
             /vms/83dab870-4903-48a9-9d37-486e100ed009\"}"
        );
        let parsed: Value = serde_json::from_str(&rendered).expect("slack payload must be JSON");
        let object = parsed.as_object().expect("slack payload must be an object");
        // No blocks, no fields beyond text.
        assert_eq!(object.len(), 1);
        assert!(object.contains_key("text"));
        let text = object["text"].as_str().expect("text must be a string");
        assert_eq!(
            text,
            "[firing] Guest filesystem nearly full (warning) — \
             /vms/83dab870-4903-48a9-9d37-486e100ed009"
        );
        assert!(!text.contains('\n'), "must be a single line");
    }

    #[test]
    fn render_slack_escapes_untrusted_strings() {
        let rendered = render_slack("test", "info", "say \"hi\" </b>", "/nodes/n1");
        let parsed: Value = serde_json::from_str(&rendered).expect("must stay valid JSON");
        let object = parsed.as_object().expect("must stay an object");
        assert_eq!(object.len(), 1);
        assert_eq!(
            object["text"],
            serde_json::json!("[test] say \"hi\" </b> (info) — /nodes/n1")
        );
    }

    // -----------------------------------------------------------------
    // Worker tests: the transport seam fakes HTTP; an in-memory store
    // pool carries the real outbox.
    // -----------------------------------------------------------------

    use chv_controlplane_store::{NotificationEventInput, NotificationOutboxRepository};

    struct FakeTransport {
        /// (url, body, signature) per call.
        calls: std::sync::Mutex<Vec<RecordedCall>>,
        /// Statuses to return per call (None = transport error).
        script: std::sync::Mutex<Vec<Option<u16>>>,
    }

    type RecordedCall = (String, Vec<u8>, Option<String>);

    impl FakeTransport {
        fn new(script: Vec<Option<u16>>) -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                script: std::sync::Mutex::new(script),
            }
        }
    }

    #[async_trait::async_trait]
    impl NotificationTransport for FakeTransport {
        async fn post(
            &self,
            url: &str,
            body: Vec<u8>,
            signature: Option<&str>,
        ) -> Result<u16, TransportError> {
            self.calls
                .lock()
                .unwrap()
                .push((url.to_string(), body, signature.map(str::to_string)));
            let mut script = self.script.lock().unwrap();
            match script.len() {
                0 => Err(TransportError("script exhausted".into())),
                _ => match script.remove(0) {
                    Some(status) => Ok(status),
                    None => Err(TransportError("simulated timeout".into())),
                },
            }
        }
    }

    fn dispatcher_settings() -> DispatcherSettings {
        DispatcherSettings {
            webhook_url: Some("https://alerts.example.internal/hook".into()),
            signing_secret: "0123456789abcdef0123456789abcdef".into(),
            slack_webhook_url: None,
            max_attempts: 3,
            max_batch: 10,
        }
    }

    async fn test_dispatcher(
        script: Vec<Option<u16>>,
    ) -> (
        NotificationDispatcher,
        NotificationOutboxRepository,
        chv_controlplane_store::EventRepository,
        std::sync::Arc<FakeTransport>,
    ) {
        let pool = chv_controlplane_store::test_util::create_test_pool().await;
        let outbox = NotificationOutboxRepository::new(pool.clone());
        let events = chv_controlplane_store::EventRepository::new(pool.clone());
        let transport = std::sync::Arc::new(FakeTransport::new(script));
        let dispatcher = NotificationDispatcher::with_transport(
            outbox.clone(),
            events.clone(),
            dispatcher_settings(),
            transport.clone(),
        );
        (dispatcher, outbox, events, transport)
    }

    fn firing_event(event_id: &str, occurred_at_ms: i64) -> NotificationEventInput {
        NotificationEventInput {
            event_id: event_id.into(),
            alert_id: "alert-1".into(),
            incident_key: "rule-1:vm:vm-1:-".into(),
            event_type: chv_controlplane_store::EVENT_TYPE_FIRING.into(),
            severity: "warning".into(),
            target_kind: "vm".into(),
            target_id: "vm-1".into(),
            summary: "VM CPU pressure firing".into(),
            occurred_at_ms,
            payload: render_envelope(
                event_id,
                "alert-1",
                chv_controlplane_store::EVENT_TYPE_FIRING,
                "warning",
                "vm",
                "vm-1",
                "VM CPU pressure firing",
                occurred_at_ms,
                "/vms/vm-1",
            ),
            channel: chv_controlplane_store::CHANNEL_WEBHOOK.into(),
        }
    }

    #[tokio::test]
    async fn delivered_event_carries_signature_and_marks_delivered() {
        let (dispatcher, outbox, _events, transport) = test_dispatcher(vec![Some(200)]).await;
        outbox
            .enqueue(&firing_event("evt-deliver", 1_000))
            .await
            .expect("enqueue");

        let claimed = dispatcher.dispatch_pass(2_000).await.expect("pass");
        assert_eq!(claimed, 1);

        let calls: Vec<RecordedCall> = transport.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "https://alerts.example.internal/hook");
        // The signature matches the exact body bytes sent.
        let expected = sign_payload("0123456789abcdef0123456789abcdef".as_bytes(), &calls[0].1);
        assert_eq!(calls[0].2.as_deref(), Some(expected.as_str()));

        let rows = outbox.list_recent(10).await.expect("list");
        assert_eq!(rows[0].status, "delivered");
        assert_eq!(rows[0].attempts, 1);
    }

    #[tokio::test]
    async fn injected_jitter_makes_backoff_scheduling_deterministic() {
        // The contract pins the jitter to a seeded/testable source:
        // with a fixed draw the scheduled next attempt is exactly
        // now + backoff + jitter, no ±20% tolerance bands.
        let pool = chv_controlplane_store::test_util::create_test_pool().await;
        let outbox = NotificationOutboxRepository::new(pool.clone());
        let events = chv_controlplane_store::EventRepository::new(pool.clone());
        let transport = std::sync::Arc::new(FakeTransport::new(vec![None, Some(204)]));
        let dispatcher = NotificationDispatcher::with_transport(
            outbox.clone(),
            events,
            dispatcher_settings(),
            transport,
        )
        .with_jitter(std::sync::Arc::new(|_backoff| 1_234));
        outbox
            .enqueue(&firing_event("evt-fixed", 1_000))
            .await
            .expect("enqueue");

        dispatcher.dispatch_pass(2_000).await.expect("pass");
        let rows = outbox.list_recent(10).await.expect("list");
        // Backoff for attempt 2 is 10s (5s * 2^1); jitter is fixed.
        assert_eq!(rows[0].status, "pending");
        assert_eq!(rows[0].next_attempt_at_ms, 2_000 + 10_000 + 1_234);
    }

    #[tokio::test]
    async fn retryable_failure_schedules_backoff_then_recovers() {
        // Timeout, then 503, then success.
        let (dispatcher, outbox, _events, _transport) =
            test_dispatcher(vec![None, Some(503), Some(204)]).await;
        outbox
            .enqueue(&firing_event("evt-retry", 1_000))
            .await
            .expect("enqueue");

        let claimed = dispatcher.dispatch_pass(2_000).await.expect("pass 1");
        assert_eq!(claimed, 1);
        let rows = outbox.list_recent(10).await.expect("list");
        assert_eq!(rows[0].status, "pending");
        assert_eq!(rows[0].attempts, 1);
        // Backoff for attempt 2 is 10s (5s * 2^1) plus bounded jitter.
        let next = rows[0].next_attempt_at_ms;
        assert!(
            next >= 2_000 + 10_000 - 2_000,
            "next attempt too early: {next}"
        );
        assert!(
            next <= 2_000 + 10_000 + 2_000,
            "next attempt too late: {next}"
        );

        // Not due before the backoff elapses.
        let claimed = dispatcher.dispatch_pass(5_000).await.expect("pass early");
        assert_eq!(claimed, 0);

        // Due after backoff: second failure, then recovery. The final
        // pass sits well beyond the latest possible jittered retry
        // time (backoff 20s ± 20%).
        dispatcher.dispatch_pass(20_000).await.expect("pass 2");
        let rows = outbox.list_recent(10).await.expect("list");
        assert_eq!(rows[0].status, "pending");
        assert_eq!(rows[0].attempts, 2);

        dispatcher.dispatch_pass(100_000).await.expect("pass 3");
        let rows = outbox.list_recent(10).await.expect("list");
        assert_eq!(rows[0].status, "delivered");
        assert_eq!(rows[0].attempts, 3);
    }

    #[tokio::test]
    async fn exhausted_retries_dead_letter_with_failure_notice() {
        // max_attempts = 3: three transport failures exhaust it.
        let (dispatcher, outbox, _events, _transport) =
            test_dispatcher(vec![None, None, None]).await;
        outbox
            .enqueue(&firing_event("evt-dead", 1_000))
            .await
            .expect("enqueue");

        dispatcher.dispatch_pass(2_000).await.expect("pass 1");
        dispatcher.dispatch_pass(60_000).await.expect("pass 2");
        dispatcher.dispatch_pass(200_000).await.expect("pass 3");

        let rows = outbox.list_recent(10).await.expect("list");
        let dead = rows
            .iter()
            .find(|r| r.event_id == "evt-dead")
            .expect("dead row");
        assert_eq!(dead.status, "dead");
        assert_eq!(dead.attempts, 3);

        // A delivery_failed courtesy event was enqueued for the same
        // incident (and is itself deliverable).
        let failure = rows
            .iter()
            .find(|r| r.event_type == chv_controlplane_store::EVENT_TYPE_DELIVERY_FAILED)
            .expect("delivery_failed event");
        assert_eq!(failure.alert_id, "alert-1");
        assert_eq!(failure.status, "pending");
    }

    #[tokio::test]
    async fn permanent_failure_dead_letters_immediately_without_retry() {
        let (dispatcher, outbox, _events, transport) = test_dispatcher(vec![Some(404)]).await;
        outbox
            .enqueue(&firing_event("evt-404", 1_000))
            .await
            .expect("enqueue");

        dispatcher.dispatch_pass(2_000).await.expect("pass");
        // Exactly one attempt: 404 is permanent.
        assert_eq!(transport.calls.lock().unwrap().len(), 1);
        let rows = outbox.list_recent(10).await.expect("list");
        let dead = rows.iter().find(|r| r.event_id == "evt-404").expect("row");
        assert_eq!(dead.status, "dead");
        assert_eq!(dead.attempts, 1);
    }

    #[tokio::test]
    async fn dead_webhook_routes_courtesy_notice_to_surviving_slack() {
        // Both channels configured; the webhook event dead-letters
        // (404). The courtesy notice must ride the SURVIVING slack
        // channel, not the dead webhook one.
        let pool = chv_controlplane_store::test_util::create_test_pool().await;
        let outbox = NotificationOutboxRepository::new(pool.clone());
        let events = chv_controlplane_store::EventRepository::new(pool.clone());
        let transport = std::sync::Arc::new(FakeTransport::new(vec![Some(404)]));
        let mut settings = dispatcher_settings();
        settings.slack_webhook_url = Some("https://hooks.slack.example/T/B/X".into());
        let dispatcher =
            NotificationDispatcher::with_transport(outbox.clone(), events, settings, transport);
        outbox
            .enqueue(&firing_event("evt-dual", 1_000))
            .await
            .expect("enqueue");

        dispatcher.dispatch_pass(2_000).await.expect("pass");
        let rows = outbox.list_recent(10).await.expect("list");
        let dead = rows.iter().find(|r| r.event_id == "evt-dual").expect("row");
        assert_eq!(dead.status, "dead");
        let failure = rows
            .iter()
            .find(|r| r.event_type == chv_controlplane_store::EVENT_TYPE_DELIVERY_FAILED)
            .expect("courtesy event");
        assert_eq!(
            failure.channel, "slack",
            "the courtesy notice rides the surviving channel"
        );
    }

    #[tokio::test]
    async fn dead_event_with_no_configured_destination_skips_the_courtesy_enqueue() {
        // The dead event's channel destination is gone and nothing
        // else is configured: no courtesy event is enqueued (it
        // could only dead-letter too); the audit trail still records
        // the failure.
        let pool = chv_controlplane_store::test_util::create_test_pool().await;
        let outbox = NotificationOutboxRepository::new(pool.clone());
        let events = chv_controlplane_store::EventRepository::new(pool.clone());
        let transport = std::sync::Arc::new(FakeTransport::new(vec![]));
        let mut settings = dispatcher_settings();
        settings.webhook_url = None;
        settings.slack_webhook_url = None;
        let dispatcher =
            NotificationDispatcher::with_transport(outbox.clone(), events, settings, transport);
        // A WEBHOOK-channel event dead-letters on the "destination
        // removed from configuration" path without any transport
        // call.
        outbox
            .enqueue(&firing_event("evt-skip", 1_000))
            .await
            .expect("enqueue");

        dispatcher.dispatch_pass(2_000).await.expect("pass");
        let rows = outbox.list_recent(10).await.expect("list");
        assert!(
            rows.iter()
                .all(|r| r.event_type != chv_controlplane_store::EVENT_TYPE_DELIVERY_FAILED),
            "no courtesy event when no configured channel can deliver it: {rows:?}"
        );
        // The dead event itself is still recorded.
        assert!(rows
            .iter()
            .any(|r| r.event_id == "evt-skip" && r.status == "dead"));
    }

    #[tokio::test]
    async fn delivery_failed_events_do_not_recurse_on_their_own_dead_letter() {
        // The courtesy event itself fails permanently: it must
        // dead-letter without enqueuing another delivery_failed.
        let (dispatcher, outbox, _events, transport) =
            test_dispatcher(vec![Some(404), Some(410)]).await;
        outbox
            .enqueue(&firing_event("evt-once", 1_000))
            .await
            .expect("enqueue");

        dispatcher.dispatch_pass(2_000).await.expect("pass 1");
        let rows = outbox.list_recent(10).await.expect("list");
        let failure_id = rows
            .iter()
            .find(|r| r.event_type == chv_controlplane_store::EVENT_TYPE_DELIVERY_FAILED)
            .expect("courtesy event enqueued")
            .event_id
            .clone();

        dispatcher.dispatch_pass(60_000).await.expect("pass 2");
        let rows = outbox.list_recent(10).await.expect("list");
        // Exactly one delivery_failed row exists (the courtesy event,
        // now dead), and the transport saw exactly two attempts.
        let failures = rows
            .iter()
            .filter(|r| r.event_type == chv_controlplane_store::EVENT_TYPE_DELIVERY_FAILED)
            .count();
        assert_eq!(failures, 1);
        let failure = rows.iter().find(|r| r.event_id == failure_id).expect("row");
        assert_eq!(failure.status, "dead");
        assert_eq!(transport.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn slack_channel_renders_text_and_never_signs() {
        let pool = chv_controlplane_store::test_util::create_test_pool().await;
        let outbox = NotificationOutboxRepository::new(pool.clone());
        let events = chv_controlplane_store::EventRepository::new(pool.clone());
        let transport = std::sync::Arc::new(FakeTransport::new(vec![Some(200)]));
        let settings = DispatcherSettings {
            webhook_url: None,
            signing_secret: "0123456789abcdef0123456789abcdef".into(),
            slack_webhook_url: Some("https://hooks.slack.com/services/T/B/X".into()),
            max_attempts: 3,
            max_batch: 10,
        };
        let dispatcher = NotificationDispatcher::with_transport(
            outbox.clone(),
            events,
            settings,
            transport.clone(),
        );

        let mut event = firing_event("evt-slack", 1_000);
        event.channel = chv_controlplane_store::CHANNEL_SLACK.into();
        outbox.enqueue(&event).await.expect("enqueue");

        dispatcher.dispatch_pass(2_000).await.expect("pass");
        let calls: Vec<RecordedCall> = transport.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "https://hooks.slack.com/services/T/B/X");
        // Slack posts are unsigned: the HMAC header would be
        // meaningless against Slack's own URL credential.
        assert_eq!(calls[0].2, None);
        let body: Value = serde_json::from_slice(&calls[0].1).expect("slack body");
        assert_eq!(
            body["text"],
            "[firing] VM CPU pressure firing (warning) — /vms/vm-1"
        );
        let rows = outbox.list_recent(10).await.expect("list");
        assert_eq!(rows[0].status, "delivered");
    }
}
