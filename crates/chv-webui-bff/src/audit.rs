//! Durable audit trail for privileged BFF mutations (#658).
//!
//! Emission is centralized here, in middleware wrapped around the
//! whole HTTP surface that carries privileged mutations. Individual
//! handlers do NOT remember to call an audit function: the middleware
//! classifies the request by method and path, runs it, and writes
//! exactly one row with the final HTTP status. Because it wraps the
//! role middlewares too, a 401/403 denial is recorded with the
//! credential's real actor (or the explicit anonymous marker when no
//! credential validated).
//!
//! # Audited surfaces
//!
//! The classifier covers every non-safe method under:
//!
//! - `/v1/*` — the BFF surface, including the guest-agent browser
//!   routes (`/v1/monitoring/agents/*`, mounted by the control plane
//!   outside the BFF router) and `/v1/auth/change-password` /
//!   `/v1/auth/logout` (self-service but security-relevant);
//! - `/admin/*` — control-plane admin mutations;
//! - `/api/v1/*` — the legacy cookie-authenticated surface;
//! - `/internal/*` — control-plane internal management.
//!
//! Excluded: `POST /v1/auth/login` and `POST /api/v1/auth/login`
//! (credential entry — the body carries the password, the caller is
//! anonymous by definition, and brute-force volume is accounted for by
//! auth rate limiting, not by this trail) and the machine-to-machine
//! guest ingest transport `/monitoring/v1/*` (TLS-certificate
//! authenticated, high frequency, not a privileged human mutation).
//! Unknown non-safe paths on an audited surface fail closed into a
//! row (probing leaves a trace).
//!
//! # Mounting
//!
//! Standalone callers get all of this from
//! [`crate::router::bff_router`]. The production control plane mounts
//! [`crate::router::bff_router_unaudited`] inside
//! `admin_router_with_guest_agents` and applies this middleware at the
//! outermost layer there, so the extra privileged surfaces and the
//! router fallback are covered too — and nothing is audited twice.
//!
//! Redaction contract: the BFF never reads a request body, query string,
//! Authorization header, Cookie header, password, token, or S3 key into
//! the audit row. `metadata` is built from a tiny allowlist (the auth
//! mechanism classification only). The durable store's insert API takes
//! the already-redacted JSON object.
//!
//! # Transactional coupling (#706 half 1, PR 2)
//!
//! Mutating handlers that are pure database work in one transaction
//! *adopt* the audit outbox: the middleware mints a per-request
//! [`AuditOutboxTicket`], the handler passes its mutation transaction to
//! [`commit_adopted_mutation`], and the outbox row commits with the
//! mutation or not at all. After the handler returns the middleware
//! performs **check-or-insert** against the outbox primary key:
//!
//! - row present → the evidence row is already durable; the direct
//!   insert is skipped (`finalize_pending` patches the row if the final
//!   response is not 2xx/3xx);
//! - row absent → denied, failed, rolled back, or unadopted; the direct
//!   post-hoc insert runs exactly as before.
//!
//! `outbox_id` and `audit_id` are server-minted UUIDs; no client-supplied
//! identifier is ever read or trusted. Denied and unadopted paths keep the
//! original crash window — losing a denial row loses no state change.

use std::net::{IpAddr, SocketAddr};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use axum::{
    extract::{ConnectInfo, FromRequestParts, Request, State},
    http::{request::Parts, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use chv_controlplane_store::{
    audit_outbox::enqueue_on_tx as enqueue_audit_on_tx, AuditLogInsert, AuditLogRepository,
    AuditOutboxInsert, AuditOutboxRepository, AUDIT_ANONYMOUS_ACTOR_ID, AUDIT_ANONYMOUS_ROLE,
    AUDIT_ANONYMOUS_USERNAME, AUDIT_OUTCOME_DENIED, AUDIT_OUTCOME_FAILED, AUDIT_OUTCOME_SUCCESS,
};
use serde_json::json;
use uuid::Uuid;

use crate::auth::{authenticate_headers, AuthSource};
use crate::router::AppState;
use crate::BffError;

/// Legacy tracing-only emitter retained for existing handler call sites.
/// The durable row is written by [`record_privileged_mutation`]; this
/// function intentionally contains no database access and never carries
/// request bodies or credentials.
pub fn log_mutation(user: &str, action: &str, resource_kind: &str, resource_id: &str) {
    tracing::info!(
        target: "audit",
        user = %user,
        action = %action,
        resource_kind = %resource_kind,
        resource_id = %resource_id,
        "mutation"
    );
}

/// Maximum stored length for client-supplied identifier strings
/// (`x-operation-id`, the correlation id). Identifiers are telemetry,
/// not data; capping them bounds row size against hostile or buggy
/// clients that send megabyte header values.
const MAX_CLIENT_ID_LEN: usize = 128;

fn capped_client_id(value: String) -> String {
    value.chars().take(MAX_CLIENT_ID_LEN).collect()
}

/// Maximum stored length for path-derived fields (`path`, `action`,
/// `resource_kind`, `resource_id`). These strings can embed
/// attacker-chosen URL segments (including on the fail-closed unknown
/// path arm); capping them bounds row size without affecting routing,
/// filtering on normal values, or the action/resource vocabulary.
const MAX_AUDIT_FIELD_LEN: usize = 256;

fn capped_field(value: String) -> String {
    value.chars().take(MAX_AUDIT_FIELD_LEN).collect()
}

/// Server-minted, request-scoped audit-outbox ticket (#706 half 1).
///
/// `Clone` is cheap (`Arc`): the middleware keeps one handle to read the
/// commit flag after the handler returns while the request extensions
/// carry the other to the handler. The ids are minted with UUIDv4 and are
/// **never** read from a header, body, or query string.
#[derive(Clone, Debug)]
pub struct AuditOutboxTicket(Arc<AuditOutboxTicketState>);

#[derive(Debug)]
struct AuditOutboxTicketState {
    outbox_id: String,
    audit_id: String,
    /// The already-redacted row. `outcome`/`status_code` are the
    /// commit-time prediction `success`/200; the middleware patches them
    /// with `finalize_pending` if the final response is not 2xx/3xx.
    payload: AuditLogInsert,
    /// Set only after the mutation transaction **commits** with the
    /// outbox row. The middleware uses it to distinguish "the handler
    /// adopted but the post-handler check failed" (never insert twice)
    /// from "nothing was enqueued" (fall back to the direct insert).
    committed: AtomicBool,
}

impl AuditOutboxTicket {
    /// Mint a ticket for one request from its fully-redacted base row.
    pub fn from_payload(payload: AuditLogInsert) -> Self {
        Self(Arc::new(AuditOutboxTicketState {
            outbox_id: Uuid::new_v4().simple().to_string(),
            audit_id: Uuid::new_v4().simple().to_string(),
            payload,
            committed: AtomicBool::new(false),
        }))
    }

    /// Idempotency key of this request's outbox row.
    pub fn outbox_id(&self) -> &str {
        &self.0.outbox_id
    }

    /// Pre-minted `audit_log` primary key the dispatcher will reuse.
    pub fn audit_id(&self) -> &str {
        &self.0.audit_id
    }

    /// True once the mutation transaction containing this ticket's
    /// outbox row has committed.
    pub fn was_committed(&self) -> bool {
        self.0.committed.load(Ordering::SeqCst)
    }

    fn mark_committed(&self) {
        self.0.committed.store(true, Ordering::SeqCst);
    }

    /// Build the store input stamped at enqueue time. `occurred_at_ms`
    /// is overwritten so the row's timestamp is the commit attempt, not
    /// request start.
    fn outbox_insert(&self) -> AuditOutboxInsert {
        let now_ms = chv_common::now_unix_ms();
        AuditOutboxInsert {
            outbox_id: self.0.outbox_id.clone(),
            audit_id: self.0.audit_id.clone(),
            payload: AuditLogInsert {
                occurred_at_ms: now_ms,
                ..self.0.payload.clone()
            },
            created_at_ms: now_ms,
        }
    }
}

/// Handler-side handle to the request's [`AuditOutboxTicket`].
///
/// Infallible: `None` when the handler is mounted without the audit
/// middleware (`bff_router_unaudited` standalone) or invoked directly by
/// a test. An adopted handler then commits its mutation **without**
/// enqueueing, and the outer middleware (if any) takes the direct path —
/// exactly today's behavior for an unadopted route.
#[derive(Clone, Debug, Default)]
pub struct AuditTicket(pub Option<AuditOutboxTicket>);

impl AuditTicket {
    pub fn as_ref(&self) -> Option<&AuditOutboxTicket> {
        self.0.as_ref()
    }
}

#[async_trait::async_trait]
impl<S: Send + Sync> FromRequestParts<S> for AuditTicket {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // Request extensions are server-side only: a client cannot forge
        // this ticket, so a handler can never be tricked into enqueueing
        // an attacker-chosen `outbox_id`.
        Ok(Self(parts.extensions.get::<AuditOutboxTicket>().cloned()))
    }
}

/// Commit a mutation transaction with its audit evidence attached.
///
/// This is the single adoption seam: the handler executes its mutation
/// statements on `tx`, then hands the transaction here. When a ticket is
/// present the outbox row is enqueued **inside the same transaction**
/// immediately before the commit, so the mutation and its evidence share
/// one commit — the crash window in issue #706 closes at exactly these
/// sites.
///
/// **Fail-closed (NIT 5 decision).** Enqueue validates the serialized
/// `AuditLogInsert` (≤ 16 KiB, the store's existing bound). An oversized
/// row — in practice only a pathologically long `actor_username`, since
/// the other fields are capped at construction — is *not* silently
/// truncated: the error rolls the mutation transaction back (the caller
/// returns the error and `tx` is dropped), so no mutation exists without
/// its evidence. The middleware then records the failed request on the
/// direct path, exactly once. This is covered by
/// `oversized_actor_fails_closed_and_rolls_back` in
/// `tests/audit_log.rs`.
pub async fn commit_adopted_mutation(
    mut tx: sqlx::Transaction<'_, sqlx::Sqlite>,
    ticket: Option<&AuditOutboxTicket>,
) -> Result<(), BffError> {
    if let Some(ticket) = ticket {
        let insert = ticket.outbox_insert();
        if let Err(error) = enqueue_audit_on_tx(&mut tx, &insert).await {
            // Loud and structured: an adopted mutation must not commit
            // without durable evidence. Returning here drops `tx`, which
            // rolls the mutation back.
            tracing::error!(
                target: "audit",
                outbox_id = %ticket.outbox_id(),
                audit_id = %ticket.audit_id(),
                error = %error,
                "AUDIT OUTBOX ENQUEUE FAILED — rolling back the mutation transaction"
            );
            return Err(BffError::Internal(
                "failed to record audit evidence".to_string(),
            ));
        }
    }
    tx.commit().await.map_err(|e| {
        tracing::error!(error = %e, "adopted mutation commit failed");
        BffError::Internal(format!("failed to commit transaction: {}", e))
    })?;
    // Only after a successful commit: a rolled-back ticket must never
    // suppress the middleware's direct insert.
    if let Some(ticket) = ticket {
        ticket.mark_committed();
    }
    Ok(())
}

/// Middleware: append exactly one durable audit row for every mutating
/// request on an audited surface, then return the handler/role-middleware
/// response.
pub async fn record_privileged_mutation(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    let Some(classification) = classify_mutation(&method, &path) else {
        return next.run(req).await;
    };

    let correlation_id = req
        .extensions()
        .get::<Option<String>>()
        .and_then(|id| id.clone())
        .or_else(|| header_value(&req, "x-correlation-id"))
        .map(capped_client_id);
    let operation_id = header_value(&req, "x-operation-id").map(capped_client_id);
    let source_ip = extract_source_ip(&req);

    // Decode the same credential the authorization layer uses. On any
    // failure the actor is explicitly anonymous — never inferred from an
    // unverified header or body field. The decoded result is also shared
    // with the `BearerToken` extractor via extensions so authorization
    // and audit attribution cannot drift (#658 review L6).
    let decoded = authenticate_headers(req.headers(), &state).await;
    let (actor, auth_source) = match &decoded {
        Ok((claims, source)) => (claims.clone(), *source),
        Err(_) => (
            crate::auth::Claims {
                sub: AUDIT_ANONYMOUS_ACTOR_ID.to_string(),
                username: AUDIT_ANONYMOUS_USERNAME.to_string(),
                role: AUDIT_ANONYMOUS_ROLE.to_string(),
                exp: 0,
                must_change_password: false,
                aud: None,
                token_version: None,
            },
            AuthSource::Anonymous,
        ),
    };

    // The metadata object is an explicit allowlist. Do not add request
    // headers, bodies, query strings, or handler payloads here.
    let metadata = json!({ "auth_source": auth_source.as_str() }).to_string();

    // Build the redacted row once. Outcome/status are the commit-time
    // prediction for an adopted handler (`success`/200); the final
    // outcome is only known after the handler returns and is applied
    // below — directly, or via `finalize_pending` when the row already
    // committed inside the mutation.
    let base_entry = AuditLogInsert {
        occurred_at_ms: chv_common::now_unix_ms(),
        actor_id: actor.sub,
        actor_username: actor.username,
        actor_role: actor.role,
        action: capped_field(classification.action),
        method: method.as_str().to_string(),
        path: capped_field(path),
        resource_kind: classification.resource_kind.map(capped_field),
        resource_id: classification.resource_id.map(capped_field),
        outcome: AUDIT_OUTCOME_SUCCESS.to_string(),
        status_code: 200,
        correlation_id,
        operation_id,
        source_ip,
        metadata,
    };

    // One ticket per request, server-minted and unforgeable. Adopted
    // handlers enqueue it on their own transaction; unadopted handlers
    // ignore it and the direct insert below runs unchanged.
    let ticket = AuditOutboxTicket::from_payload(base_entry.clone());
    let mut req = req;
    req.extensions_mut().insert(ticket.clone());
    req.extensions_mut()
        .insert(crate::auth::VerifiedAuth(decoded));

    let response = next.run(req).await;
    let status = response.status();
    let outcome = outcome_for_status(status);
    let status_code = i64::from(status.as_u16());

    // Check-or-insert: the outbox primary key is this request's
    // idempotency key, so the tx-coupled path and the direct path
    // compose without a double write.
    let outbox = AuditOutboxRepository::new(state.pool.clone());
    match outbox.get(ticket.outbox_id()).await {
        Ok(Some(_)) => {
            // The handler committed its mutation and the evidence row
            // with it. Never insert a second row.
            if !status.is_success() && !status.is_redirection() {
                patch_final_outcome(&outbox, &ticket, outcome, status_code).await;
            } else {
                tracing::debug!(
                    target: "audit",
                    outbox_id = %ticket.outbox_id(),
                    audit_id = %ticket.audit_id(),
                    status = status_code,
                    "audit row queued by the adopted handler"
                );
            }
            return response;
        }
        Ok(None) => {
            if ticket.was_committed() {
                // The handler committed with an enqueued row, yet the
                // row is gone (only possible if it was manually
                // deleted or pruned years early). Inserting is the
                // evidence-preserving action; log the broken invariant.
                tracing::error!(
                    target: "audit",
                    outbox_id = %ticket.outbox_id(),
                    "audit outbox row committed with the mutation but is now missing; \
                     writing the direct row"
                );
            }
            // Denied, failed before commit, rolled back, or unadopted:
            // fall through to today's post-hoc insert.
        }
        Err(error) => {
            if ticket.was_committed() {
                // The enqueue committed with the mutation, so the
                // evidence is durable in the outbox even though this
                // check failed. Do NOT insert a second row for it.
                tracing::error!(
                    target: "audit",
                    outbox_id = %ticket.outbox_id(),
                    error = %error,
                    "audit outbox check failed after the handler committed its evidence; \
                     skipping the direct insert to avoid a duplicate row"
                );
                return response;
            }
            tracing::warn!(
                target: "audit",
                outbox_id = %ticket.outbox_id(),
                error = %error,
                "audit outbox check failed before any enqueue; falling back to the direct insert"
            );
        }
    }

    let entry = AuditLogInsert {
        occurred_at_ms: chv_common::now_unix_ms(),
        outcome: outcome.to_string(),
        status_code,
        ..base_entry
    };

    match AuditLogRepository::new(state.pool.clone())
        .insert(&entry)
        .await
    {
        Ok(audit_id) => {
            tracing::debug!(
                target: "audit",
                audit_id = %audit_id,
                status = status_code,
                outcome = outcome,
                "audit row recorded"
            );
            response
        }
        Err(error) => {
            // Loud, structured, and never silent: an audit write failure
            // after a mutation succeeded must not be reported as success.
            // The dedicated counter makes a systemic audit-store outage
            // visible on dashboards even though the metrics middleware
            // records the final status on its own (#658 review M3).
            metrics::counter!("chv_bff_audit_write_failures_total").increment(1);
            tracing::error!(
                target: "audit",
                error = %error,
                action = %entry.action,
                method = %entry.method,
                path = %entry.path,
                status = status_code,
                outcome = outcome,
                "AUDIT LOG WRITE FAILED — privileged mutation may not be durably recorded"
            );
            if status.is_success() || status.is_redirection() {
                let mut body = json!({
                    "message": "privileged mutation completed but its audit record could not be persisted",
                    "code": "AUDIT_LOG_WRITE_FAILED"
                });
                // Explicit correlation handle so the operator can find
                // what the mutation actually did (#658 review M3). The
                // outer correlation middleware re-inserts the same id.
                if let Some(id) = &entry.correlation_id {
                    body["request_id"] = json!(id);
                }
                (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
            } else {
                // Preserve the original denial/failure: replacing it could
                // hide the real error from the caller, and the loud log
                // above already records the missing audit row.
                response
            }
        }
    }
}

/// Patch the already-committed outbox row when the final response is not
/// a success. The mutation committed, so the evidence row must stay; only
/// its recorded outcome/status change.
async fn patch_final_outcome(
    outbox: &AuditOutboxRepository,
    ticket: &AuditOutboxTicket,
    outcome: &str,
    status_code: i64,
) {
    match outbox
        .finalize_pending(
            ticket.outbox_id(),
            outcome,
            status_code,
            chv_common::now_unix_ms(),
        )
        .await
    {
        Ok(true) => tracing::warn!(
            target: "audit",
            outbox_id = %ticket.outbox_id(),
            audit_id = %ticket.audit_id(),
            status = status_code,
            outcome = outcome,
            "adopted mutation committed but the final response was not a success; \
             patched the pending outbox row"
        ),
        Ok(false) => tracing::warn!(
            target: "audit",
            outbox_id = %ticket.outbox_id(),
            audit_id = %ticket.audit_id(),
            status = status_code,
            outcome = outcome,
            "adopted mutation committed but its outbox row was already dispatched; \
             the evidence row keeps the commit-time success prediction"
        ),
        Err(error) => tracing::error!(
            target: "audit",
            outbox_id = %ticket.outbox_id(),
            audit_id = %ticket.audit_id(),
            error = %error,
            "failed to patch the pending audit outbox row; it keeps the \
             commit-time success prediction"
        ),
    }
}

fn outcome_for_status(status: StatusCode) -> &'static str {
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        AUDIT_OUTCOME_DENIED
    } else if status.is_success() || status.is_redirection() {
        AUDIT_OUTCOME_SUCCESS
    } else {
        AUDIT_OUTCOME_FAILED
    }
}

fn header_value(req: &Request, name: &str) -> Option<String> {
    req.headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Source IP is advisory telemetry, not an authorization input. Prefer
/// the transport peer address when the server provides connect info
/// (both the plain-HTTP and TLS serve paths inject it); otherwise
/// accept the first valid `x-forwarded-for` / `x-real-ip` hop — a
/// value only a **trusted reverse proxy** can be expected to set
/// correctly. Without ConnectInfo, a direct client can spoof these
/// headers; deployments that do not sit behind such a proxy must treat
/// `source_ip` as unverified. Invalid values are dropped rather than
/// stored verbatim.
fn extract_source_ip(req: &Request) -> Option<String> {
    if let Some(ConnectInfo(addr)) = req.extensions().get::<ConnectInfo<SocketAddr>>() {
        return Some(addr.ip().to_string());
    }
    if let Some(forwarded) = header_value(req, "x-forwarded-for") {
        if let Some(first) = forwarded.split(',').next() {
            if let Ok(ip) = first.trim().parse::<IpAddr>() {
                return Some(ip.to_string());
            }
        }
    }
    header_value(req, "x-real-ip")
        .and_then(|value| value.parse::<IpAddr>().ok())
        .map(|ip| ip.to_string())
}

#[derive(Debug, PartialEq, Eq)]
struct MutationClassification {
    action: String,
    resource_kind: Option<String>,
    resource_id: Option<String>,
}

/// Method/path classifier. Any non-safe method on an audited surface
/// is audited unless it is explicitly a read-shaped POST. Audited
/// surfaces: `/v1/*` (including the guest-agent browser routes and
/// the self-service `change-password`/`logout`), `/admin/*`,
/// `/api/v1/*` (legacy), and `/internal/*`. Excluded: the login
/// credential-entry endpoints (bodies carry passwords; anonymous
/// brute-force volume belongs to auth rate limiting, not this trail)
/// and the machine-to-machine guest ingest transport
/// `/monitoring/v1/*`. The fallback treats an unknown non-safe path
/// on an audited surface as a mutation (fail-closed: a new write
/// route — or mere probing of an unrouted one — is audited by
/// default, never silently omitted).
fn classify_mutation(method: &Method, path: &str) -> Option<MutationClassification> {
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return None;
    }
    // Machine-to-machine guest ingest transport: TLS-certificate
    // authenticated, high frequency, not a privileged human mutation.
    if path.starts_with("/monitoring/v1/") {
        return None;
    }
    // The legacy /api/v1 surface shares the /v1 classification table;
    // the row still stores the original request path.
    let normalized;
    let path = if let Some(rest) = path.strip_prefix("/api/v1") {
        normalized = format!("/v1{rest}");
        normalized.as_str()
    } else {
        path
    };
    if !path.starts_with("/v1/") && !path.starts_with("/admin/") && !path.starts_with("/internal/")
    {
        return None;
    }
    // Credential entry: unauthenticated by definition, password in the
    // body, and brute-force volume — noise for this trail.
    if path == "/v1/auth/login" {
        return None;
    }
    if is_read_shaped_post(method, path) {
        return None;
    }

    let (action, resource_kind) = action_and_kind(method, path);
    Some(MutationClassification {
        action,
        resource_kind: Some(resource_kind.to_string()),
        resource_id: resource_id_from_path(path),
    })
}

fn is_read_shaped_post(method: &Method, path: &str) -> bool {
    if *method != Method::POST {
        return false;
    }

    if path == "/v1/quotas/check" {
        return true;
    }
    if let Some(rest) = path.strip_prefix("/v1/quotas/") {
        let segments: Vec<&str> = rest.split('/').collect();
        if segments.len() == 2 && segments[1] == "usage" {
            return true;
        }
    }
    // Rendering a stored cloud-init template substitutes variables and
    // returns text; it writes nothing (and is deliberately not a
    // mutation of the template).
    if let Some(rest) = path.strip_prefix("/v1/cloud-init-templates/") {
        let segments: Vec<&str> = rest.split('/').collect();
        if segments.len() == 2 && segments[1] == "render" {
            return true;
        }
    }

    matches!(
        path,
        "/v1/overview"
            | "/v1/metrics"
            | "/v1/monitoring/catalog"
            | "/v1/monitoring/overview"
            | "/v1/monitoring/current"
            | "/v1/monitoring/checks"
            | "/v1/monitoring/history"
            | "/v1/monitoring/alerts"
            | "/v1/monitoring/alerts/detail"
            | "/v1/monitoring/alert-rules"
            | "/v1/monitoring/notifications/deliveries"
            | "/v1/monitoring/agents"
            | "/v1/nodes"
            | "/v1/nodes/get"
            | "/v1/vms"
            | "/v1/vms/get"
            | "/v1/vms/console"
            | "/v1/vms/events"
            | "/v1/vms/snapshots"
            | "/v1/tasks"
            | "/v1/tasks/get"
            | "/v1/clusters"
            | "/v1/networks"
            | "/v1/networks/get"
            | "/v1/events"
            | "/v1/images"
            | "/v1/maintenance"
            | "/v1/settings"
            | "/v1/settings/hypervisor"
            | "/v1/settings/hypervisor/profiles"
            | "/v1/volumes"
            | "/v1/volumes/get"
            | "/v1/quotas"
            | "/v1/quotas/me"
            | "/v1/usage"
            | "/v1/tokens"
            | "/v1/users"
            | "/v1/audit-log"
            | "/v1/audit-log/export"
            | "/v1/architectures/list"
            | "/v1/architectures/get"
            | "/v1/architectures/validate"
            | "/v1/architectures/versions/list"
            | "/v1/architectures/check-fleet"
            | "/v1/architectures/generate-yaml"
            | "/v1/architectures/validate-yaml"
            | "/v1/architectures/runs/list"
            | "/v1/architectures/netbox/config/get"
            | "/v1/architectures/netbox/export/dry-run"
            | "/v1/architectures/netbox/runs/list"
            | "/v1/architectures/netbox/runs/get"
    )
}

fn action_and_kind(method: &Method, path: &str) -> (String, &'static str) {
    let (action, kind) = match path {
        "/v1/auth/change-password" => ("auth.change_password", "user"),
        "/v1/auth/logout" => ("auth.logout", "user"),
        "/v1/nodes/mutate" => ("node.mutate", "node"),
        "/v1/nodes/enroll" => ("node.enroll", "node"),
        "/v1/monitoring/alert-rules/create" => ("alert_rule.create", "alert_rule"),
        "/v1/monitoring/alert-rules/update" => ("alert_rule.update", "alert_rule"),
        "/v1/monitoring/alert-rules/delete" => ("alert_rule.delete", "alert_rule"),
        "/v1/monitoring/alerts/acknowledge" => ("alert.acknowledge", "incident"),
        "/v1/monitoring/alerts/silence" => ("alert.silence", "incident"),
        "/v1/monitoring/notifications/test" => ("notification.test", "notification"),
        // Guest-agent browser surface (operator tier, mounted by the
        // control plane outside the BFF router — audited since the
        // review-round restructure).
        "/v1/monitoring/agents/claim" => ("agent.issue_claim", "monitoring_agent"),
        "/v1/monitoring/agents/revoke" => ("agent.revoke", "monitoring_agent"),
        "/v1/monitoring/agents/rotate" => ("agent.rotate", "monitoring_agent"),
        "/v1/monitoring/agents/reset" => ("agent.reset", "monitoring_agent"),
        "/v1/vms/create" => ("vm.create", "vm"),
        "/v1/vms/delete" => ("vm.delete", "vm"),
        "/v1/vms/resize" => ("vm.resize", "vm"),
        "/v1/vms/mutate" => ("vm.mutate", "vm"),
        "/v1/vms/import" => ("vm.import", "vm"),
        "/v1/vms/snapshots/create" => ("snapshot.create", "snapshot"),
        "/v1/vms/snapshots/delete" => ("snapshot.delete", "snapshot"),
        "/v1/vms/snapshots/restore" => ("snapshot.restore", "snapshot"),
        "/v1/networks/create" => ("network.create", "network"),
        "/v1/networks/delete" => ("network.delete", "network"),
        "/v1/networks/update" => ("network.update", "network"),
        "/v1/networks/mutate" => ("network.mutate", "network"),
        "/v1/images/import" => ("image.import", "image"),
        "/v1/images/delete" => ("image.delete", "image"),
        "/v1/volumes/create" => ("volume.create", "volume"),
        "/v1/volumes/mutate" => ("volume.mutate", "volume"),
        "/v1/volumes/snapshot" => ("volume.snapshot", "volume"),
        "/v1/volumes/restore-snapshot" => ("volume.restore_snapshot", "volume"),
        "/v1/volumes/delete-snapshot" => ("volume.delete_snapshot", "volume"),
        "/v1/volumes/delete" => ("volume.delete", "volume"),
        "/v1/volumes/clone" => ("volume.clone", "volume"),
        "/v1/vm-templates" => ("vm_template.create", "vm_template"),
        "/v1/cloud-init-templates" => ("cloud_init_template.create", "cloud_init_template"),
        "/v1/tokens/create" => ("api_token.create", "api_token"),
        "/v1/tokens/revoke" => ("api_token.revoke", "api_token"),
        "/v1/backup-jobs" => ("backup.create", "backup"),
        "/v1/backup-history" => ("backup.list_attempt", "backup"),
        "/v1/backups/jobs" => ("backup.create", "backup"),
        "/v1/backups/schedules" => ("backup_schedule.create", "backup_schedule"),
        "/v1/backups/restores" => ("backup_restore.create", "backup_restore"),
        "/v1/architectures/create" => ("architecture.create", "architecture"),
        "/v1/architectures/update" => ("architecture.update", "architecture"),
        "/v1/architectures/archive" => ("architecture.archive", "architecture"),
        "/v1/architectures/import-yaml" => ("architecture.import_yaml", "architecture"),
        "/v1/architectures/plan" => ("architecture.plan", "architecture"),
        "/v1/architectures/destroy-plan" => ("architecture.destroy_plan", "architecture"),
        "/v1/architectures/discard-plan" => ("architecture.discard_plan", "architecture"),
        "/v1/architectures/apply" => ("architecture.apply", "architecture"),
        "/v1/architectures/destroy" => ("architecture.destroy", "architecture"),
        "/v1/architectures/drift" => ("architecture.drift", "architecture"),
        "/v1/architectures/netbox/config/upsert" => ("netbox_config.upsert", "netbox_config"),
        "/v1/architectures/netbox/config/delete" => ("netbox_config.delete", "netbox_config"),
        "/v1/architectures/netbox/export" => ("netbox.export", "netbox_run"),
        "/v1/architectures/netbox/runs/retry" => ("netbox.retry", "netbox_run"),
        "/v1/settings/hypervisor/update" => ("hypervisor_settings.update", "hypervisor_settings"),
        "/v1/quotas/create" => ("quota.create", "quota"),
        "/v1/users/create" => ("user.create", "user"),
        "/v1/users/update" => ("user.update", "user"),
        "/v1/users/delete" => ("user.delete", "user"),
        _ => {
            // Dynamic/parameterized routes and future additions.
            if let Some(rest) = path.strip_prefix("/v1/vms/") {
                if rest.ends_with("/export") {
                    return ("vm.export".to_string(), "vm");
                }
            }
            // Legacy cookie-authenticated backup surface (reached via
            // the /api/v1 → /v1 normalization): run/toggle/delete a
            // fenced job by path id.
            if let Some(rest) = path.strip_prefix("/v1/backup-jobs/") {
                let segments: Vec<&str> = rest.split('/').collect();
                if segments.len() == 2 && segments[1] == "run" {
                    return ("backup.run".to_string(), "backup");
                }
                if segments.len() == 2 && segments[1] == "toggle" {
                    return ("backup.toggle".to_string(), "backup");
                }
                if *method == Method::DELETE {
                    return ("backup.delete".to_string(), "backup");
                }
            }
            // Control-plane admin mutations.
            if let Some(rest) = path.strip_prefix("/admin/migrations/") {
                if rest.ends_with("/cancel") {
                    return ("migration.cancel".to_string(), "migration");
                }
            }
            // Control-plane internal management.
            if path == "/internal/bootstrap-token" {
                return ("bootstrap_token.seed".to_string(), "bootstrap_token");
            }
            if path.starts_with("/v1/vm-templates/") {
                if path.ends_with("/clone") {
                    return ("vm_template.clone".to_string(), "vm_template");
                }
                if *method == Method::DELETE {
                    return ("vm_template.delete".to_string(), "vm_template");
                }
            }
            if path.starts_with("/v1/cloud-init-templates/") {
                if path.ends_with("/render") {
                    return (
                        "cloud_init_template.render".to_string(),
                        "cloud_init_template",
                    );
                }
                if *method == Method::DELETE {
                    return (
                        "cloud_init_template.delete".to_string(),
                        "cloud_init_template",
                    );
                }
            }
            if let Some(rest) = path.strip_prefix("/v1/backups/jobs/") {
                let segments: Vec<&str> = rest.split('/').collect();
                if segments.len() >= 2 && segments[1] == "execute" {
                    return ("backup.execute".to_string(), "backup");
                }
                if *method == Method::PATCH {
                    return ("backup.update".to_string(), "backup");
                }
                if *method == Method::DELETE {
                    return ("backup.delete".to_string(), "backup");
                }
            }
            if path.starts_with("/v1/backups/schedules/") {
                if *method == Method::PATCH {
                    return ("backup_schedule.update".to_string(), "backup_schedule");
                }
                if *method == Method::DELETE {
                    return ("backup_schedule.delete".to_string(), "backup_schedule");
                }
            }
            if path.starts_with("/v1/settings/hypervisor/apply-profile") {
                return (
                    "hypervisor_settings.apply".to_string(),
                    "hypervisor_settings",
                );
            }
            if path == "/v1/settings/hypervisor" && *method == Method::PATCH {
                return (
                    "hypervisor_settings.update".to_string(),
                    "hypervisor_settings",
                );
            }
            if let Some(rest) = path.strip_prefix("/v1/quotas/") {
                let segments: Vec<&str> = rest.split('/').collect();
                if segments.len() == 1 {
                    if *method == Method::PATCH {
                        return ("quota.update".to_string(), "quota");
                    }
                    if *method == Method::DELETE {
                        return ("quota.delete".to_string(), "quota");
                    }
                }
            }

            let kind = path
                .trim_start_matches('/')
                .split('/')
                .nth(1)
                .unwrap_or("unknown");
            let kind = match kind {
                "users" => "user",
                "tokens" => "api_token",
                "vms" => "vm",
                "vm-templates" => "vm_template",
                "cloud-init-templates" => "cloud_init_template",
                "networks" => "network",
                "volumes" => "volume",
                "images" => "image",
                "nodes" => "node",
                "quotas" => "quota",
                "architectures" => "architecture",
                "monitoring" => "monitoring",
                "settings" => "settings",
                "backups" | "backup-jobs" | "backup-history" => "backup",
                other => other,
            };
            return (
                format!("{kind}.{}", method.as_str().to_ascii_lowercase()),
                // Unknown future route: the action keeps the path-derived
                // noun for filtering, while the typed kind stays explicit.
                "unknown",
            );
        }
    };
    (action.to_string(), kind)
}

const PATH_ACTION_WORDS: &[&str] = &[
    "create",
    "delete",
    "update",
    "mutate",
    "resize",
    "import",
    "export",
    "snapshot",
    "restore-snapshot",
    "delete-snapshot",
    "clone",
    "list",
    "get",
    "validate",
    "versions",
    "plan",
    "apply",
    "destroy",
    "archive",
    "check-fleet",
    "generate-yaml",
    "validate-yaml",
    "import-yaml",
    "discard-plan",
    "destroy-plan",
    "runs",
    "drift",
    "config",
    "netbox",
    "overview",
    "history",
    "current",
    "checks",
    "health",
    "catalog",
    "alerts",
    "alert-rules",
    "notifications",
    "profiles",
    "apply-profile",
    "create",
    "revoke",
    "enroll",
    "execute",
    "snapshots",
];

fn resource_id_from_path(path: &str) -> Option<String> {
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let second = *segments.get(1)?;
    let third = *segments.get(2)?;

    match second {
        "backups" => {
            // /v1/backups/{jobs|schedules|restores}/:id[/execute]
            let id = *segments.get(3)?;
            if id.is_empty() || PATH_ACTION_WORDS.contains(&id) {
                None
            } else {
                Some(id.to_string())
            }
        }
        "settings" => {
            // /v1/settings/hypervisor/apply-profile/:id
            if segments.get(3) == Some(&"apply-profile") {
                segments
                    .get(4)
                    .filter(|id| !id.is_empty())
                    .map(|id| id.to_string())
            } else {
                None
            }
        }
        "quotas" => {
            if third == "check"
                || third == "me"
                || segments.get(3) == Some(&"usage")
                || PATH_ACTION_WORDS.contains(&third)
            {
                None
            } else {
                Some(third.to_string())
            }
        }
        _ => {
            if PATH_ACTION_WORDS.contains(&third) {
                None
            } else {
                Some(third.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_requests_are_not_mutations() {
        assert!(classify_mutation(&Method::GET, "/v1/vms").is_none());
        assert!(classify_mutation(&Method::GET, "/v1/audit-log").is_none());
    }

    #[test]
    fn read_shaped_posts_are_not_mutations() {
        assert!(classify_mutation(&Method::POST, "/v1/vms").is_none());
        assert!(classify_mutation(&Method::POST, "/v1/vms/get").is_none());
        assert!(classify_mutation(&Method::POST, "/v1/audit-log").is_none());
        assert!(classify_mutation(&Method::POST, "/v1/users").is_none());
        assert!(classify_mutation(&Method::POST, "/v1/quotas/u-1/usage").is_none());
        // Guest-agent inventory list (viewer tier) is a read.
        assert!(classify_mutation(&Method::POST, "/v1/monitoring/agents").is_none());
        // Cloud-init template rendering is a pure read (no write, no task).
        assert!(classify_mutation(&Method::POST, "/v1/cloud-init-templates/t-1/render").is_none());
        // A render-shaped suffix on another path is not read-shaped.
        assert!(
            classify_mutation(&Method::POST, "/v1/cloud-init-templates/t-1/render/extra").is_some()
        );
    }

    #[test]
    fn operator_and_admin_mutations_are_classified() {
        let class = classify_mutation(&Method::POST, "/v1/vms/create").expect("mutation");
        assert_eq!(class.action, "vm.create");
        assert_eq!(class.resource_kind.as_deref(), Some("vm"));

        let class = classify_mutation(&Method::DELETE, "/v1/vm-templates/t-1").expect("mutation");
        assert_eq!(class.action, "vm_template.delete");
        assert_eq!(class.resource_id.as_deref(), Some("t-1"));

        let class = classify_mutation(&Method::PATCH, "/v1/quotas/u-1").expect("mutation");
        assert_eq!(class.action, "quota.update");
        assert_eq!(class.resource_id.as_deref(), Some("u-1"));
    }

    #[test]
    fn unknown_non_safe_v1_paths_fail_closed_to_mutation() {
        let class = classify_mutation(&Method::POST, "/v1/future-write").expect("mutation");
        assert_eq!(class.action, "future-write.post");
    }

    #[test]
    fn login_is_excluded_but_security_relevant_auth_routes_are_audited() {
        // Login is credential entry: password in the body, anonymous
        // caller, brute-force volume — deliberately outside the trail.
        assert!(classify_mutation(&Method::POST, "/v1/auth/login").is_none());
        assert!(classify_mutation(&Method::POST, "/api/v1/auth/login").is_none());
        // change-password and logout are security-relevant self-service
        // actions (a session thief's seizure step) and ARE recorded —
        // with actor/action only, never the body.
        let class = classify_mutation(&Method::POST, "/v1/auth/change-password").expect("mutation");
        assert_eq!(class.action, "auth.change_password");
        assert_eq!(class.resource_kind.as_deref(), Some("user"));
        let class = classify_mutation(&Method::POST, "/v1/auth/logout").expect("mutation");
        assert_eq!(class.action, "auth.logout");
        // The legacy /api/v1 logout is classified identically.
        let class = classify_mutation(&Method::POST, "/api/v1/auth/logout").expect("mutation");
        assert_eq!(class.action, "auth.logout");
    }

    #[test]
    fn control_plane_admin_internal_and_legacy_surfaces_are_audited() {
        let class =
            classify_mutation(&Method::POST, "/admin/migrations/m-1/cancel").expect("mutation");
        assert_eq!(class.action, "migration.cancel");
        assert_eq!(class.resource_kind.as_deref(), Some("migration"));
        assert_eq!(class.resource_id.as_deref(), Some("m-1"));

        let class =
            classify_mutation(&Method::POST, "/internal/bootstrap-token").expect("mutation");
        assert_eq!(class.action, "bootstrap_token.seed");
        assert_eq!(class.resource_kind.as_deref(), Some("bootstrap_token"));

        // Legacy /api/v1 backup surface (normalized onto /v1).
        let class =
            classify_mutation(&Method::POST, "/api/v1/backup-jobs/j-9/run").expect("mutation");
        assert_eq!(class.action, "backup.run");
        assert_eq!(class.resource_id.as_deref(), Some("j-9"));
        let class =
            classify_mutation(&Method::DELETE, "/api/v1/backup-jobs/j-9").expect("mutation");
        assert_eq!(class.action, "backup.delete");
        assert_eq!(class.resource_id.as_deref(), Some("j-9"));

        // Unknown non-safe paths on every audited surface fail closed.
        assert!(classify_mutation(&Method::POST, "/admin/future").is_some());
        assert!(classify_mutation(&Method::POST, "/internal/future").is_some());
        assert!(classify_mutation(&Method::POST, "/api/v1/future").is_some());
    }

    #[test]
    fn guest_agent_browser_mutations_are_audited_but_ingest_is_not() {
        for path in [
            "/v1/monitoring/agents/claim",
            "/v1/monitoring/agents/revoke",
            "/v1/monitoring/agents/rotate",
            "/v1/monitoring/agents/reset",
        ] {
            let class = classify_mutation(&Method::POST, path).expect("mutation");
            assert_eq!(class.resource_kind.as_deref(), Some("monitoring_agent"));
        }
        // The certificate-authenticated machine ingest transport is
        // excluded: high frequency, not a privileged human mutation.
        assert!(classify_mutation(&Method::POST, "/monitoring/v1/enroll").is_none());
        assert!(classify_mutation(&Method::POST, "/monitoring/v1/ingest").is_none());
        assert!(classify_mutation(&Method::POST, "/monitoring/v1/rotate").is_none());
        // Non-audited surfaces stay excluded.
        assert!(classify_mutation(&Method::POST, "/some/static/path").is_none());
        assert!(classify_mutation(&Method::POST, "/health").is_none());
    }

    #[test]
    fn client_identifiers_are_capped() {
        let long = "x".repeat(5_000);
        assert_eq!(capped_client_id(long.clone()).len(), MAX_CLIENT_ID_LEN);
        assert_eq!(capped_client_id("short".to_string()), "short");
    }

    #[test]
    fn path_derived_fields_are_capped() {
        let long = "x".repeat(5_000);
        assert_eq!(
            capped_field(long.clone()).chars().count(),
            MAX_AUDIT_FIELD_LEN
        );
        assert_eq!(capped_field("vm.create".to_string()), "vm.create");
        assert_eq!(
            capped_field("é".repeat(1_000)).chars().count(),
            MAX_AUDIT_FIELD_LEN,
            "cap counts characters, not bytes"
        );
    }

    #[test]
    fn denied_and_failed_status_mapping() {
        assert_eq!(outcome_for_status(StatusCode::OK), AUDIT_OUTCOME_SUCCESS);
        assert_eq!(
            outcome_for_status(StatusCode::FORBIDDEN),
            AUDIT_OUTCOME_DENIED
        );
        assert_eq!(
            outcome_for_status(StatusCode::UNAUTHORIZED),
            AUDIT_OUTCOME_DENIED
        );
        assert_eq!(
            outcome_for_status(StatusCode::BAD_REQUEST),
            AUDIT_OUTCOME_FAILED
        );
        assert_eq!(
            outcome_for_status(StatusCode::INTERNAL_SERVER_ERROR),
            AUDIT_OUTCOME_FAILED
        );
    }

    fn ticket_payload() -> AuditLogInsert {
        AuditLogInsert {
            occurred_at_ms: 1_000,
            actor_id: "u-1".into(),
            actor_username: "alice".into(),
            actor_role: "admin".into(),
            action: "user.create".into(),
            method: "POST".into(),
            path: "/v1/users/create".into(),
            resource_kind: Some("user".into()),
            resource_id: None,
            outcome: AUDIT_OUTCOME_SUCCESS.into(),
            status_code: 200,
            correlation_id: Some("corr-1".into()),
            operation_id: None,
            source_ip: None,
            metadata: r#"{"auth_source":"bearer_jwt"}"#.into(),
        }
    }

    #[test]
    fn tickets_mint_distinct_ids_and_start_uncommitted() {
        let first = AuditOutboxTicket::from_payload(ticket_payload());
        let second = AuditOutboxTicket::from_payload(ticket_payload());

        assert_ne!(first.outbox_id(), second.outbox_id());
        assert_ne!(first.audit_id(), second.audit_id());
        assert_eq!(first.outbox_id().len(), 32, "UUIDv4 simple form");
        assert_eq!(first.audit_id().len(), 32);
        assert!(!first.was_committed());
        assert!(!second.was_committed());

        // The store input is stamped at enqueue time (not request start).
        let input = first.outbox_insert();
        assert_eq!(input.outbox_id, first.outbox_id());
        assert_eq!(input.audit_id, first.audit_id());
        assert_eq!(input.created_at_ms, input.payload.occurred_at_ms);
        assert!(input.payload.occurred_at_ms >= 1_000);
    }
}
