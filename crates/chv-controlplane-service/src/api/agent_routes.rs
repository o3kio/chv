//! Guest monitoring agent HTTP routes (ingestion contract v1's guest
//! transport; ADR-026 / campaign #602 prompt 03).
//!
//! These routes are **agent-authenticated, not browser-authenticated**:
//! no JWT session, no cookies, no CSRF token — identity is the TLS
//! client credential (or, at claim redemption, the one-time claim
//! itself). They are mounted on the manager's HTTPS listener only
//! when guest ingestion is enabled and TLS is provisioned; a
//! plain-HTTP listener never serves them (bootstrap refuses the
//! combination at startup).
//!
//! Contract response discipline: every error is
//! `{"error":{"code":"...","request_id":"..."}}`; success carries the
//! acknowledged state. Bodies are capped at 256 KiB (413 with the
//! same error shape); counters travel as exact decimal strings above
//! the JS safe-integer range.

use crate::api::tls::TlsPeer;
use crate::monitoring_agent::{
    peer_credential_from_der, AgentPeerCredential, EnrollOutcome, GuestIngestOutcome,
    MonitoringAgentService, RotateOutcome, MAX_GUEST_BODY_BYTES,
};

use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};

use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// The agent-authenticated routes, added onto any router. State is the
/// shared service (via `Extension`); the `TlsPeer` extension is
/// injected by the TLS accept loop. Generic over the router state so
/// the same routes mount on the admin router (`AppState`) in
/// production and on a plain `Router<()>` in the TLS tests — the
/// handlers authenticate via the TLS credential, never router state.
pub fn agent_routes<S: Clone + Send + Sync + 'static>(router: Router<S>) -> Router<S> {
    router
        .route("/monitoring/v1/enroll", axum::routing::post(enroll))
        .route("/monitoring/v1/ingest", axum::routing::post(ingest))
        .route("/monitoring/v1/rotate", axum::routing::post(rotate))
}

/// The agent-authenticated router for the admin router merge: BFF
/// `AppState`-typed with the shared service installed.
pub fn agent_router(service: Arc<MonitoringAgentService>) -> Router<chv_webui_bff::AppState> {
    agent_routes(Router::<chv_webui_bff::AppState>::new()).layer(Extension(service))
}

// ---------------------------------------------------------------------------
// Bounded JSON extraction (contract error shape on oversize/malformed)
// ---------------------------------------------------------------------------

struct LimitedJson<T>(T);

/// Errors that carry the contract JSON shape and an HTTP status.
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let request_id = chv_common::gen_resource_id();
        let body = json!({
            "error": {
                "code": self.code,
                "message": self.message,
                "request_id": request_id,
            }
        });
        (self.status, Json(body)).into_response()
    }
}

#[async_trait::async_trait]
impl<T: for<'de> Deserialize<'de>, S: Send + Sync> axum::extract::FromRequest<S>
    for LimitedJson<T>
{
    type Rejection = ApiError;

    async fn from_request(
        req: Request<axum::body::Body>,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        let bytes = axum::body::to_bytes(req.into_body(), MAX_GUEST_BODY_BYTES.saturating_add(1))
            .await
            .map_err(|e| {
                ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "batch_too_large",
                    format!(
                        "request body exceeds the {} byte limit: {e}",
                        MAX_GUEST_BODY_BYTES
                    ),
                )
            })?;
        if bytes.len() > MAX_GUEST_BODY_BYTES {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "batch_too_large",
                format!(
                    "request body exceeds the {} byte limit",
                    MAX_GUEST_BODY_BYTES
                ),
            ));
        }
        serde_json::from_slice::<T>(&bytes)
            .map_err(|e| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_batch",
                    format!("malformed JSON body: {e}"),
                )
            })
            .map(LimitedJson)
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct EnrollRequest {
    schema_version: i32,
    claim: String,
    install_id: String,
    csr_pem: String,
}

async fn enroll(
    Extension(service): Extension<Arc<MonitoringAgentService>>,
    Extension(peer): Extension<TlsPeer>,
    LimitedJson(req): LimitedJson<EnrollRequest>,
) -> Response {
    if req.schema_version != 1 {
        return ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_batch",
            "schema_version must be 1",
        )
        .into_response();
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    let remote_ip = peer.remote_addr.map(|a| a.ip().to_string());
    match service
        .redeem_claim(
            &req.claim,
            &req.install_id,
            &req.csr_pem,
            remote_ip.as_deref(),
            now_ms,
        )
        .await
    {
        Ok(EnrollOutcome::Enrolled(agent)) => (
            StatusCode::CREATED,
            Json(json!({
                "schema_version": 1,
                "agent_id": agent.agent_id,
                "vm_id": agent.vm_id,
                "certificate_pem": agent.certificate_pem,
                "ca_pem": agent.ca_pem,
                "credential_epoch": agent.credential_epoch,
                "expires_at_ms": agent.expires_at_ms,
                "identity_epoch": format!("agent-credential-generation-{}", agent.credential_epoch),
            })),
        )
            .into_response(),
        Ok(EnrollOutcome::RateLimited) => ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "too many enrollment attempts from this address",
        )
        .into_response(),
        Ok(EnrollOutcome::UnknownClaim) => {
            ApiError::new(StatusCode::UNAUTHORIZED, "unauthenticated", "unknown claim")
                .into_response()
        }
        Ok(EnrollOutcome::ExpiredClaim) => {
            ApiError::new(StatusCode::UNAUTHORIZED, "unauthenticated", "claim expired")
                .into_response()
        }
        Ok(EnrollOutcome::AlreadyUsedClaim) => ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "claim already used",
        )
        .into_response(),
        Ok(EnrollOutcome::AlreadyEnrolled { .. }) => ApiError::new(
            StatusCode::CONFLICT,
            "agent_already_enrolled",
            "an active guest agent is already enrolled for this vm; revoke it first",
        )
        .into_response(),
        Ok(EnrollOutcome::InvalidRequest(detail)) => {
            ApiError::new(StatusCode::BAD_REQUEST, "invalid_batch", detail).into_response()
        }
        Err(e) => internal(e),
    }
}

async fn ingest(
    Extension(service): Extension<Arc<MonitoringAgentService>>,
    Extension(peer): Extension<TlsPeer>,
    LimitedJson(envelope): LimitedJson<crate::monitoring_agent::GuestBatchEnvelope>,
) -> Response {
    // Identity: the TLS client credential, never the payload.
    let Some(cert_der) = peer.client_cert_der.clone() else {
        return ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "guest ingestion requires an enrolled agent credential (TLS client certificate)",
        )
        .into_response();
    };
    let credential: AgentPeerCredential = match peer_credential_from_der(&cert_der) {
        Ok(c) => c,
        Err(reason) => {
            return ApiError::new(StatusCode::UNAUTHORIZED, "unauthenticated", reason)
                .into_response()
        }
    };

    let now_ms = chrono::Utc::now().timestamp_millis();
    match service.ingest_batch(&credential, &envelope, now_ms).await {
        Ok(GuestIngestOutcome::Accepted {
            samples,
            renewal_due,
        }) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "schema_version": 1,
                "status": "accepted",
                "accepted_samples": samples,
                "credential": { "state": if renewal_due { "renewal_due" } else { "active" } },
            })),
        )
            .into_response(),
        Ok(GuestIngestOutcome::Duplicate {
            samples,
            renewal_due,
        }) => (
            StatusCode::OK,
            Json(json!({
                "schema_version": 1,
                "status": "duplicate",
                "accepted_samples": samples,
                "credential": { "state": if renewal_due { "renewal_due" } else { "active" } },
            })),
        )
            .into_response(),
        Ok(GuestIngestOutcome::ReplayConflict) => ApiError::new(
            StatusCode::CONFLICT,
            "replay_conflict",
            "same batch key with a different body",
        )
        .into_response(),
        Ok(GuestIngestOutcome::StaleSequence) => ApiError::new(
            StatusCode::CONFLICT,
            "resync_required",
            "sequence at or below the durable high-water mark; restart the agent run",
        )
        .into_response(),
        Ok(GuestIngestOutcome::Unauthenticated(reason)) => {
            ApiError::new(StatusCode::UNAUTHORIZED, "unauthenticated", reason).into_response()
        }
        Ok(GuestIngestOutcome::ForbiddenTarget) => ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden_target",
            "sender cannot write to the requested target",
        )
        .into_response(),
        Ok(GuestIngestOutcome::InvalidBatch(detail)) => {
            ApiError::new(StatusCode::BAD_REQUEST, "invalid_batch", detail).into_response()
        }
        Ok(GuestIngestOutcome::BatchTooLarge(detail)) => {
            ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "batch_too_large", detail).into_response()
        }
        Ok(GuestIngestOutcome::UnsupportedMetric(detail)) => ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unsupported_metric",
            detail,
        )
        .into_response(),
        Ok(GuestIngestOutcome::RateLimited {
            retry_after_seconds,
        }) => {
            let body = Json(json!({
                "error": {
                    "code": "rate_limited",
                    "message": "per-agent rate or concurrency limit exceeded",
                    "request_id": chv_common::gen_resource_id(),
                }
            }));
            (
                StatusCode::TOO_MANY_REQUESTS,
                [("retry-after", retry_after_seconds.to_string())],
                body,
            )
                .into_response()
        }
        Ok(GuestIngestOutcome::IngestionUnavailable) => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "ingestion_unavailable",
            "no durable write accepted; monitoring is degraded",
        )
        .into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
struct RotateRequest {
    schema_version: i32,
    csr_pem: String,
}

async fn rotate(
    Extension(service): Extension<Arc<MonitoringAgentService>>,
    Extension(peer): Extension<TlsPeer>,
    LimitedJson(req): LimitedJson<RotateRequest>,
) -> Response {
    if req.schema_version != 1 {
        return ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_batch",
            "schema_version must be 1",
        )
        .into_response();
    }
    let Some(cert_der) = peer.client_cert_der.clone() else {
        return ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "rotation requires the current agent credential",
        )
        .into_response();
    };
    let credential: AgentPeerCredential = match peer_credential_from_der(&cert_der) {
        Ok(c) => c,
        Err(reason) => {
            return ApiError::new(StatusCode::UNAUTHORIZED, "unauthenticated", reason)
                .into_response()
        }
    };
    let now_ms = chrono::Utc::now().timestamp_millis();
    match service
        .rotate_credential(&credential, &req.csr_pem, now_ms)
        .await
    {
        Ok(RotateOutcome::Rotated {
            cert,
            credential_epoch,
        }) => (
            StatusCode::OK,
            Json(json!({
                "schema_version": 1,
                "certificate_pem": cert.certificate_pem,
                "ca_pem": service.issuer().ca_pem(),
                "credential_epoch": credential_epoch,
                "expires_at_ms": cert.not_after_ms,
                "identity_epoch": format!("agent-credential-generation-{credential_epoch}"),
            })),
        )
            .into_response(),
        Ok(RotateOutcome::Unauthenticated(reason)) => {
            ApiError::new(StatusCode::UNAUTHORIZED, "unauthenticated", reason).into_response()
        }
        Ok(RotateOutcome::InvalidRequest(detail)) => {
            ApiError::new(StatusCode::BAD_REQUEST, "invalid_batch", detail).into_response()
        }
        Err(e) => internal(e),
    }
}

fn internal(e: crate::error::ControlPlaneServiceError) -> Response {
    let request_id = chv_common::gen_resource_id();
    tracing::warn!(error = %e, %request_id, "guest monitoring route internal error");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "error": {
                "code": "internal_error",
                "message": "internal error; see manager logs",
                "request_id": request_id,
            }
        })),
    )
        .into_response()
}

/// Reject any non-POST method on the agent surface with the contract
/// error shape (mounted by the router's method-not-allowed default —
/// this only exists to keep the error vocabulary consistent for
/// OPTIONS/GET probes).
#[allow(dead_code)]
fn method_not_allowed() -> Response {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "the guest monitoring surface only accepts POST",
    )
    .into_response()
}
