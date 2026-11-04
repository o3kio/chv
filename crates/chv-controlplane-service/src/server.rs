use crate::enrollment::EnrollmentService;
use crate::inventory::InventoryService;
use crate::lifecycle::LifecycleService;
use crate::peer_identity::{verify_peer_matches, InsecurePeer, PeerCertificateSerial, PeerNodeId};
use crate::reconcile::ReconcileService;
use crate::revocation::RevocationSnapshot;
use crate::telemetry::TelemetryService;
use chv_controlplane_store::{
    AuditLogInsert, AuditLogRepository, AUDIT_ANONYMOUS_ACTOR_ID, AUDIT_ANONYMOUS_ROLE,
    AUDIT_ANONYMOUS_USERNAME, AUDIT_OUTCOME_DENIED, AUDIT_OUTCOME_FAILED, AUDIT_OUTCOME_SUCCESS,
};
use chv_controlplane_types::domain::{NodeId, ResourceKind};
use control_plane_node_api::control_plane_node_api as proto;
use serde_json::json;
use std::sync::Arc;
use tonic::{Request, Response, Status};
use tracing::Instrument;

const LIFECYCLE_SERVICE_PATH: &str = "chv.controlplane.node.v1.LifecycleService";
const ENROLLMENT_SERVICE_PATH: &str = "chv.controlplane.node.v1.EnrollmentService";
const RECONCILE_SERVICE_PATH: &str = "chv.controlplane.node.v1.ReconcileService";

/// Bound the stored identifier fields (same limits as the BFF trail) so a
/// hostile request cannot inflate audit rows.
const MAX_AUDIT_FIELD_LEN: usize = 256;
const MAX_CLIENT_ID_LEN: usize = 128;

fn extract_op_id<T>(request: &Request<T>) -> Option<String> {
    request
        .metadata()
        .get(chv_common::OPERATION_ID_METADATA_KEY)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

fn capped_audit_field(value: String) -> String {
    value.chars().take(MAX_AUDIT_FIELD_LEN).collect()
}

fn capped_client_id(value: String) -> String {
    value.chars().take(MAX_CLIENT_ID_LEN).collect()
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(capped_audit_field(value.to_string()))
    }
}

/// Redacted, request-derived context for one audited gRPC mutation.
///
/// The actor comes only from the mTLS [`PeerNodeId`] extension. A
/// wire-asserted `node_id` is never trusted for attribution.
struct GrpcAuditContext {
    action: &'static str,
    service: &'static str,
    resource_kind: Option<&'static str>,
    resource_id: Option<String>,
    actor_id: String,
    actor_username: String,
    actor_role: &'static str,
    operation_id: Option<String>,
    source_ip: Option<String>,
    metadata: String,
}

impl GrpcAuditContext {
    fn new<T>(
        request: &Request<T>,
        action: &'static str,
        service: &'static str,
        resource_kind: Option<&'static str>,
        resource_id: Option<String>,
        anonymous_auth_source: Option<&'static str>,
    ) -> Self {
        let peer = request.extensions().get::<PeerNodeId>();
        // `EnrollNode` is authenticated by a one-time bootstrap token, not by
        // the peer certificate, so it forces the anonymous actor with its own
        // `auth_source` label. Labelling it `insecure` (the no-peer branch)
        // would falsely claim the deployment ran without mTLS.
        let (actor_id, actor_username, actor_role, auth_source) = match anonymous_auth_source {
            Some(auth_source) => (
                AUDIT_ANONYMOUS_ACTOR_ID.to_string(),
                AUDIT_ANONYMOUS_USERNAME.to_string(),
                AUDIT_ANONYMOUS_ROLE,
                auth_source,
            ),
            None => match peer {
                Some(peer) => (
                    capped_audit_field(peer.as_node_id().to_string()),
                    capped_audit_field(peer.as_node_id().to_string()),
                    "node",
                    "mtls",
                ),
                None => (
                    AUDIT_ANONYMOUS_ACTOR_ID.to_string(),
                    AUDIT_ANONYMOUS_USERNAME.to_string(),
                    AUDIT_ANONYMOUS_ROLE,
                    "insecure",
                ),
            },
        };

        Self {
            action,
            service,
            resource_kind,
            resource_id,
            actor_id,
            actor_username,
            actor_role,
            operation_id: extract_op_id(request)
                .map(capped_client_id)
                .filter(|value| !value.is_empty()),
            source_ip: request.remote_addr().map(|addr| addr.ip().to_string()),
            // Explicit allowlist only: never bodies, tokens, certificates,
            // or handler error details.
            metadata: json!({
                "transport": "grpc",
                "auth_source": auth_source,
            })
            .to_string(),
        }
    }

    fn path(&self) -> String {
        format!("{}/{}", self.service, self.action)
    }
}

fn audit_status(code: tonic::Code) -> (i64, &'static str) {
    let status_code = match code {
        tonic::Code::Ok => 200,
        tonic::Code::InvalidArgument | tonic::Code::OutOfRange => 400,
        tonic::Code::Unauthenticated => 401,
        tonic::Code::PermissionDenied => 403,
        tonic::Code::NotFound => 404,
        tonic::Code::AlreadyExists | tonic::Code::Aborted => 409,
        tonic::Code::FailedPrecondition => 412,
        tonic::Code::ResourceExhausted => 429,
        tonic::Code::Cancelled => 499,
        tonic::Code::Unimplemented => 501,
        tonic::Code::Unavailable => 503,
        tonic::Code::DeadlineExceeded => 504,
        tonic::Code::Internal | tonic::Code::DataLoss | tonic::Code::Unknown => 500,
    };
    let outcome = match code {
        tonic::Code::Ok => AUDIT_OUTCOME_SUCCESS,
        tonic::Code::Unauthenticated | tonic::Code::PermissionDenied => AUDIT_OUTCOME_DENIED,
        _ => AUDIT_OUTCOME_FAILED,
    };
    (status_code, outcome)
}

/// Append exactly one durable audit row for a node-facing mutation. Returns
/// `Err(())` when the row could not be persisted; the caller decides whether
/// that masks a successful mutation (fail closed) or preserves the original
/// handler error.
async fn record_grpc_audit(
    audit: Option<&AuditLogRepository>,
    context: &GrpcAuditContext,
    code: tonic::Code,
) -> Result<(), ()> {
    let Some(repo) = audit else {
        return Ok(());
    };
    let (status_code, outcome) = audit_status(code);
    let entry = AuditLogInsert {
        occurred_at_ms: chv_common::now_unix_ms(),
        actor_id: context.actor_id.clone(),
        actor_username: context.actor_username.clone(),
        actor_role: context.actor_role.to_string(),
        action: capped_audit_field(context.action.to_string()),
        method: "gRPC".to_string(),
        path: capped_audit_field(context.path()),
        resource_kind: context
            .resource_kind
            .map(|kind| capped_audit_field(kind.to_string())),
        resource_id: context.resource_id.clone(),
        outcome: outcome.to_string(),
        status_code,
        correlation_id: None,
        operation_id: context.operation_id.clone(),
        source_ip: context.source_ip.clone(),
        metadata: context.metadata.clone(),
    };

    match repo.insert(&entry).await {
        Ok(audit_id) => {
            tracing::debug!(
                target: "audit",
                audit_id = %audit_id,
                status = status_code,
                outcome,
                "gRPC audit row recorded"
            );
            Ok(())
        }
        Err(error) => {
            metrics::counter!("chv_grpc_audit_write_failures_total").increment(1);
            tracing::error!(
                target: "audit",
                error = %error,
                action = %entry.action,
                method = %entry.method,
                path = %entry.path,
                status = status_code,
                outcome,
                "AUDIT LOG WRITE FAILED — privileged gRPC mutation may not be durably recorded"
            );
            Err(())
        }
    }
}

async fn finish_audited<T>(
    audit: Option<&AuditLogRepository>,
    context: &GrpcAuditContext,
    result: Result<Response<T>, Status>,
) -> Result<Response<T>, Status> {
    let code = match &result {
        Ok(_) => tonic::Code::Ok,
        Err(status) => status.code(),
    };
    let audit_failed = record_grpc_audit(audit, context, code).await.is_err();
    if audit_failed && result.is_ok() {
        // Mirror the BFF: a committed mutation without a durable row must
        // not be reported as success.
        Err(Status::internal(
            "AUDIT_LOG_WRITE_FAILED: privileged mutation completed but its audit record could not be persisted",
        ))
    } else {
        // Preserve denials/failures; the loud audit error above is the
        // operator signal that the row is missing.
        result
    }
}

/// Wrap the standard peer-pinned service shape (validate node identity,
/// call the service) in one audited result.
macro_rules! audited_service_call {
    ($self:ident, $request:ident, $action:literal, $service_path:expr, $span:literal, $method:ident, $kind:expr, $resource_id:expr) => {{
        let context = GrpcAuditContext::new(
            &$request,
            $action,
            $service_path,
            $kind,
            $resource_id,
            None,
        );
        let op_id = extract_op_id(&$request).unwrap_or_default();
        let result = async {
            enforce_peer_node_id(&$request, &$request.get_ref().node_id, $span)?;
            let _span = tracing::info_span!($span, %op_id);
            let resp = $self
                .service
                .$method($request.into_inner())
                .instrument(_span)
                .await
                .map_err(tonic::Status::from)?;
            Ok(Response::new(resp))
        }
        .await;
        finish_audited($self.audit.as_ref(), &context, result).await
    }};
}

macro_rules! audited_lifecycle_call {
    ($self:ident, $request:ident, $action:literal, $span:literal, $method:ident, $kind:expr, $resource_id:expr) => {
        audited_service_call!(
            $self,
            $request,
            $action,
            LIFECYCLE_SERVICE_PATH,
            $span,
            $method,
            $kind,
            $resource_id
        )
    };
}

/// Pin the wire-asserted `node_id` to the peer's mTLS-derived identity.
///
/// Used by every agent-side handler that accepts a `node_id` from an
/// authenticated agent. Returns `permission_denied` if the peer's certificate
/// CN/SAN does not match `requested_node_id`. Closes C1/H1: a compromised
/// node holding a valid leaf cert can no longer impersonate another node by
/// sending its `node_id` on the wire.
///
/// `enroll_node` is the one path that intentionally skips this — at enrollment
/// time the agent has no leaf cert yet and is authenticated solely by
/// bootstrap token.
#[allow(clippy::result_large_err)] // tonic::Status is the lingua franca of every gRPC handler in this file.
fn enforce_peer_node_id<T>(
    request: &Request<T>,
    requested_node_id: &str,
    method: &'static str,
) -> Result<(), Status> {
    let node_id = NodeId::new(requested_node_id.to_string())
        .map_err(|e| Status::invalid_argument(format!("invalid node_id: {}", e)))?;
    verify_peer_matches(request.extensions(), &node_id, method).map_err(Status::from)
}

/// Resolve the authoritative sender node id for a report and pin any
/// wire-asserted value to it.
///
/// Returns the mTLS peer identity in secure mode. An empty asserted `node_id`
/// is accepted in that case: it is the back-compat shape of a
/// `MigrationProgress` queued by an agent version that predates the field
/// (proto3 leaves it `""` after deserialising the old cache JSON), and
/// rejecting it would poison the agent's at-least-once queue forever. In
/// insecure/dev mode there is no certificate to fall back to, so the asserted
/// value is required.
#[allow(clippy::result_large_err)]
fn authoritative_peer_node_id<T>(
    request: &Request<T>,
    asserted_node_id: &str,
    method: &'static str,
) -> Result<String, Status> {
    if let Some(peer) = request.extensions().get::<PeerNodeId>() {
        if !asserted_node_id.is_empty() {
            enforce_peer_node_id(request, asserted_node_id, method)?;
        }
        return Ok(peer.as_node_id().to_string());
    }

    if request.extensions().get::<InsecurePeer>().is_some() {
        let node_id = NodeId::new(asserted_node_id.to_string())
            .map_err(|e| Status::invalid_argument(format!("invalid node_id: {}", e)))?;
        return Ok(node_id.to_string());
    }

    Err(Status::permission_denied(
        "peer mTLS identity required for this RPC",
    ))
}

pub struct EnrollmentServer {
    service: Arc<dyn EnrollmentService>,
    /// Denylist used to re-check `RotateNodeCertificate`. The enrollment
    /// service interceptor is identity-only (so a revoked certificate can
    /// re-enroll with a bootstrap token), so rotation enforcement happens here.
    revocation: Option<RevocationSnapshot>,
    audit: Option<AuditLogRepository>,
}

impl EnrollmentServer {
    pub fn new(service: Arc<dyn EnrollmentService>) -> Self {
        Self {
            service,
            revocation: None,
            audit: None,
        }
    }

    /// Enable durable audit emission for enrollment mutations.
    pub fn with_audit(mut self, audit: AuditLogRepository) -> Self {
        self.audit = Some(audit);
        self
    }

    /// Enforce the certificate denylist on `RotateNodeCertificate`.
    ///
    /// `EnrollNode` remains reachable with a revoked certificate (token-
    /// authenticated recovery); rotation must not be.
    pub fn with_revocation(mut self, revocation: RevocationSnapshot) -> Self {
        self.revocation = Some(revocation);
        self
    }

    /// Re-check the peer serial against the denylist for enrollment-service
    /// RPCs that are *not* the token-authenticated `EnrollNode` recovery path.
    ///
    /// The enrollment service uses an identity-only interceptor, so handlers
    /// that mutate state (`RotateNodeCertificate`, `ReportBootstrapResult`)
    /// must call this themselves. The log line uses the certificate-derived
    /// [`PeerNodeId`], never the wire-asserted `node_id`, so a revoked peer
    /// cannot plant a victim's id in the logs.
    #[allow(clippy::result_large_err)] // tonic::Status is the handler error type.
    fn reject_revoked_peer<T>(
        &self,
        request: &Request<T>,
        method: &'static str,
    ) -> Result<(), Status> {
        if request.extensions().get::<InsecurePeer>().is_some() {
            return Ok(());
        }
        let Some(revocation) = &self.revocation else {
            return Ok(());
        };
        let serial = request
            .extensions()
            .get::<PeerCertificateSerial>()
            .and_then(|serial| serial.0.as_deref());
        if !revocation.is_revoked(serial) {
            return Ok(());
        }

        metrics::counter!("chv_revoked_peer_rejected_total").increment(1);
        let peer_node_id = request
            .extensions()
            .get::<PeerNodeId>()
            .map(|peer| peer.as_node_id().to_string())
            .unwrap_or_else(|| "<missing>".to_string());
        tracing::warn!(
            peer_node_id = %peer_node_id,
            certificate_serial = ?serial,
            method,
            "rejecting request: peer certificate serial is revoked"
        );
        Err(Status::unauthenticated("peer certificate is revoked"))
    }
}

#[tonic::async_trait]
impl proto::enrollment_service_server::EnrollmentService for EnrollmentServer {
    async fn enroll_node(
        &self,
        request: Request<proto::EnrollmentRequest>,
    ) -> Result<Response<proto::EnrollmentResponse>, Status> {
        // Bootstrap enrollment is token-authenticated, not certificate-
        // authenticated: attribute the attempt to the explicit anonymous
        // marker, never to the wire-asserted inventory node id.
        let inventory_node_id = request
            .get_ref()
            .inventory
            .as_ref()
            .and_then(|inventory| non_empty(&inventory.node_id));
        let mut context = GrpcAuditContext::new(
            &request,
            "EnrollNode",
            ENROLLMENT_SERVICE_PATH,
            Some("node"),
            inventory_node_id,
            Some("bootstrap_token"),
        );
        let op_id = extract_op_id(&request).unwrap_or_default();
        let _span = tracing::info_span!("enroll_node", %op_id);
        // Transport-level peer address: the enrollment path derives the
        // node's fabric underlay endpoint from it (see
        // EnrollmentService::enroll_node). Must be read before
        // `into_inner()` consumes the request.
        let peer_addr = request.remote_addr();
        let result = self
            .service
            .enroll_node(request.into_inner(), peer_addr)
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)
            .map(Response::new);
        if let Ok(response) = &result {
            if let Some(issued_node_id) = non_empty(&response.get_ref().issued_node_id) {
                context.resource_id = Some(issued_node_id);
            }
        }
        finish_audited(self.audit.as_ref(), &context, result).await
    }

    async fn rotate_node_certificate(
        &self,
        request: Request<proto::RotateNodeCertificateRequest>,
    ) -> Result<Response<proto::RotateNodeCertificateResponse>, Status> {
        let context = GrpcAuditContext::new(
            &request,
            "RotateNodeCertificate",
            ENROLLMENT_SERVICE_PATH,
            Some("node"),
            non_empty(&request.get_ref().node_id),
            None,
        );
        let op_id = extract_op_id(&request).unwrap_or_default();
        let result = async {
            enforce_peer_node_id(
                &request,
                &request.get_ref().node_id,
                "rotate_node_certificate",
            )?;
            // The enrollment service uses an identity-only interceptor so a
            // revoked certificate can still present a one-time bootstrap token
            // to `EnrollNode`. Rotation is *not* a recovery path: re-check the
            // denylist here before any handler logic, mirroring the
            // steady-state interceptor's UNAUTHENTICATED response.
            self.reject_revoked_peer(&request, "rotate_node_certificate")?;

            // Read the peer certificate serial and security-mode marker before
            // `into_inner()` consumes the request. In secure mode the serial is
            // proof that the caller possesses the currently trusted
            // certificate; in insecure/dev mode the marker preserves the
            // existing bypass.
            let presented_serial = request
                .extensions()
                .get::<PeerCertificateSerial>()
                .and_then(|serial| serial.0.clone());
            let allow_insecure = request.extensions().get::<InsecurePeer>().is_some();
            let _span = tracing::info_span!("rotate_node_certificate", %op_id);
            let resp = self
                .service
                .rotate_node_certificate(request.into_inner(), presented_serial, allow_insecure)
                .instrument(_span)
                .await
                .map_err(tonic::Status::from)?;
            Ok(Response::new(resp))
        }
        .await;
        finish_audited(self.audit.as_ref(), &context, result).await
    }

    async fn report_bootstrap_result(
        &self,
        request: Request<proto::ReportBootstrapResultRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let context = GrpcAuditContext::new(
            &request,
            "ReportBootstrapResult",
            ENROLLMENT_SERVICE_PATH,
            Some("node"),
            non_empty(&request.get_ref().node_id),
            None,
        );
        let op_id = extract_op_id(&request).unwrap_or_default();
        let result = async {
            enforce_peer_node_id(
                &request,
                &request.get_ref().node_id,
                "report_bootstrap_result",
            )?;
            // Same enrollment-service exemption as rotation: bootstrap-result
            // upserts are self-scoped, but a revoked certificate must not
            // mutate node state. `EnrollNode` alone is the
            // token-authenticated exception.
            self.reject_revoked_peer(&request, "report_bootstrap_result")?;
            let _span = tracing::info_span!("report_bootstrap_result", %op_id);
            let resp = self
                .service
                .report_bootstrap_result(request.into_inner())
                .instrument(_span)
                .await
                .map_err(tonic::Status::from)?;
            Ok(Response::new(resp))
        }
        .await;
        finish_audited(self.audit.as_ref(), &context, result).await
    }
}

pub struct InventoryServer {
    service: Arc<dyn InventoryService>,
}

impl InventoryServer {
    pub fn new(service: Arc<dyn InventoryService>) -> Self {
        Self { service }
    }
}

#[tonic::async_trait]
impl proto::inventory_service_server::InventoryService for InventoryServer {
    async fn report_node_inventory(
        &self,
        request: Request<proto::ReportNodeInventoryRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        let asserted = request
            .get_ref()
            .inventory
            .as_ref()
            .map(|i| i.node_id.as_str())
            .unwrap_or_default();
        enforce_peer_node_id(&request, asserted, "report_node_inventory")?;
        let _span = tracing::info_span!("report_node_inventory", %op_id);
        // Transport-level peer address: used to derive the node's fabric
        // underlay endpoint (see InventoryService::report_node_inventory).
        // Must be read before `into_inner()` consumes the request.
        let peer_addr = request.remote_addr();
        let resp = self
            .service
            .report_node_inventory(request.into_inner(), peer_addr)
            .instrument(_span)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "report_node_inventory failed");
                tonic::Status::from(e)
            })?;
        Ok(Response::new(resp))
    }

    async fn report_service_versions(
        &self,
        request: Request<proto::ReportServiceVersionsRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        let asserted = request
            .get_ref()
            .versions
            .as_ref()
            .map(|v| v.node_id.as_str())
            .unwrap_or_default();
        enforce_peer_node_id(&request, asserted, "report_service_versions")?;
        let _span = tracing::info_span!("report_service_versions", %op_id);
        let resp = self
            .service
            .report_service_versions(request.into_inner())
            .instrument(_span)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "report_service_versions failed");
                tonic::Status::from(e)
            })?;
        Ok(Response::new(resp))
    }
}

pub struct TelemetryServer {
    service: Arc<dyn TelemetryService>,
}

impl TelemetryServer {
    pub fn new(service: Arc<dyn TelemetryService>) -> Self {
        Self { service }
    }
}

#[tonic::async_trait]
impl proto::telemetry_service_server::TelemetryService for TelemetryServer {
    async fn report_node_state(
        &self,
        request: Request<proto::NodeStateReport>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "report_node_state")?;
        let _span = tracing::info_span!("report_node_state", %op_id);
        let resp = self
            .service
            .report_node_state(request.into_inner())
            .instrument(_span)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "report_node_state failed");
                tonic::Status::from(e)
            })?;
        Ok(Response::new(resp))
    }

    async fn report_vm_state(
        &self,
        request: Request<proto::VmStateReport>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "report_vm_state")?;
        let _span = tracing::info_span!("report_vm_state", %op_id);
        let resp = self
            .service
            .report_vm_state(request.into_inner())
            .instrument(_span)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "report_vm_state failed");
                tonic::Status::from(e)
            })?;
        Ok(Response::new(resp))
    }

    async fn report_volume_state(
        &self,
        request: Request<proto::VolumeStateReport>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "report_volume_state")?;
        let _span = tracing::info_span!("report_volume_state", %op_id);
        let resp = self
            .service
            .report_volume_state(request.into_inner())
            .instrument(_span)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "report_volume_state failed");
                tonic::Status::from(e)
            })?;
        Ok(Response::new(resp))
    }

    async fn report_network_state(
        &self,
        request: Request<proto::NetworkStateReport>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "report_network_state")?;
        let _span = tracing::info_span!("report_network_state", %op_id);
        let resp = self
            .service
            .report_network_state(request.into_inner())
            .instrument(_span)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "report_network_state failed");
                tonic::Status::from(e)
            })?;
        Ok(Response::new(resp))
    }

    async fn publish_event(
        &self,
        request: Request<proto::PublishEventRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "publish_event")?;
        let _span = tracing::info_span!("publish_event", %op_id);
        let resp = self
            .service
            .publish_event(request.into_inner())
            .instrument(_span)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "publish_event failed");
                tonic::Status::from(e)
            })?;
        Ok(Response::new(resp))
    }

    async fn publish_alert(
        &self,
        request: Request<proto::PublishAlertRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "publish_alert")?;
        let _span = tracing::info_span!("publish_alert", %op_id);
        let resp = self
            .service
            .publish_alert(request.into_inner())
            .instrument(_span)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "publish_alert failed");
                tonic::Status::from(e)
            })?;
        Ok(Response::new(resp))
    }

    async fn report_migration_progress(
        &self,
        mut request: Request<proto::MigrationProgress>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        let authoritative = authoritative_peer_node_id(
            &request,
            &request.get_ref().node_id,
            "report_migration_progress",
        )?;
        // Overwrite the wire value with the certificate-derived identity: the
        // store scopes the UPDATE by this field, so it must never be
        // caller-controlled. This also repairs legacy queued reports whose
        // `node_id` is empty.
        request.get_mut().node_id = authoritative;
        let _span = tracing::info_span!("report_migration_progress", %op_id);
        let resp = self
            .service
            .report_migration_progress(request.into_inner())
            .instrument(_span)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "report_migration_progress failed");
                tonic::Status::from(e)
            })?;
        Ok(Response::new(resp))
    }
}

pub struct LifecycleServer {
    service: Arc<dyn LifecycleService>,
    audit: Option<AuditLogRepository>,
}

impl LifecycleServer {
    pub fn new(service: Arc<dyn LifecycleService>) -> Self {
        Self {
            service,
            audit: None,
        }
    }

    /// Enable durable audit emission for privileged lifecycle mutations.
    pub fn with_audit(mut self, audit: AuditLogRepository) -> Self {
        self.audit = Some(audit);
        self
    }
}

#[tonic::async_trait]
impl proto::lifecycle_service_server::LifecycleService for LifecycleServer {
    async fn create_vm(
        &self,
        request: Request<proto::CreateVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "CreateVm",
            "create_vm",
            create_vm,
            Some("vm"),
            request
                .get_ref()
                .vm
                .as_ref()
                .and_then(|vm| non_empty(&vm.vm_id))
        )
    }

    /// #513 (DP1, adopted): the standalone volume create journals
    /// BFF-direct (mirroring `POST /v1/vms`), NOT through a CP
    /// lifecycle RPC — the adopted design rejected the second
    /// journaling pipeline behind this surface (the lifecycle create
    /// RPCs have no production caller, per #516's finding). The node
    /// RPC of the same name (the #513 PR 1 dispatch carrier the
    /// orchestrator dispatches to agents) is a different surface; this
    /// CP-side shim stays closed until/unless lifecycle journaling is
    /// ever adopted for the create.
    ///
    /// Authz note: this handler never calls `into_inner()` or touches node
    /// state — it always returns `Unimplemented`. Request fields are read
    /// only to build the audit row's resource id; there is no authority to
    /// bind to a peer identity, so it remains exempt from
    /// `enforce_peer_node_id`. The denied attempt is still audited.
    async fn create_volume(
        &self,
        request: Request<proto::CreateVolumeRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let context = GrpcAuditContext::new(
            &request,
            "CreateVolume",
            LIFECYCLE_SERVICE_PATH,
            Some("volume"),
            request
                .get_ref()
                .volume
                .as_ref()
                .and_then(|volume| non_empty(&volume.volume_id)),
            None,
        );
        let result: Result<Response<proto::AckResponse>, Status> = Err(Status::unimplemented(
            "create_volume is not served by the control plane lifecycle; the standalone volume \
                 create journals via the BFF (#513)",
        ));
        finish_audited(self.audit.as_ref(), &context, result).await
    }

    /// #522 (DP1, adopted): the volume delete journals BFF-direct
    /// (mirroring `POST /v1/vms/delete`), NOT through a CP lifecycle
    /// RPC — the adopted design's tombstone transaction belongs to the
    /// BFF route (PR 2). The node RPC of the same name (the #522 PR 1
    /// dispatch carrier the orchestrator dispatches to agents) is a
    /// different surface; this CP-side shim stays closed.
    ///
    /// Authz note: unconditional `Unimplemented`; request fields are read
    /// only for the audit row, no node state is touched, and it is exempt
    /// for the same reason as `create_volume`. Still audited.
    async fn delete_volume(
        &self,
        request: Request<proto::DeleteVolumeRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let context = GrpcAuditContext::new(
            &request,
            "DeleteVolume",
            LIFECYCLE_SERVICE_PATH,
            Some("volume"),
            non_empty(&request.get_ref().volume_id),
            None,
        );
        let result: Result<Response<proto::AckResponse>, Status> = Err(Status::unimplemented(
            "delete_volume is not served by the control plane lifecycle; the volume delete \
                 journals via the BFF (#522)",
        ));
        finish_audited(self.audit.as_ref(), &context, result).await
    }

    /// #355 (PR 1 of the decomposition): the network policy update
    /// journals BFF-direct in the desired-state transaction (PR 2,
    /// the CreateVolume journaling precedent), NOT through a CP
    /// lifecycle RPC. The node RPC of the same name (the #355 dispatch
    /// carrier the orchestrator fans out to agents) is a different
    /// surface; this CP-side shim stays closed.
    ///
    /// Authz note: unconditional `Unimplemented`; request fields are read
    /// only for the audit row, no node state is touched, and it is exempt
    /// for the same reason as `create_volume`. Still audited.
    async fn apply_network_policy(
        &self,
        request: Request<proto::ApplyNetworkPolicyRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let context = GrpcAuditContext::new(
            &request,
            "ApplyNetworkPolicy",
            LIFECYCLE_SERVICE_PATH,
            Some("network"),
            non_empty(&request.get_ref().network_id),
            None,
        );
        let result: Result<Response<proto::AckResponse>, Status> = Err(Status::unimplemented(
            "apply_network_policy is not served by the control plane lifecycle; the network \
                 policy update journals via the BFF (#355)",
        ));
        finish_audited(self.audit.as_ref(), &context, result).await
    }

    async fn start_vm(
        &self,
        request: Request<proto::StartVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "StartVm",
            "start_vm",
            start_vm,
            Some("vm"),
            non_empty(&request.get_ref().vm_id)
        )
    }

    async fn stop_vm(
        &self,
        request: Request<proto::StopVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "StopVm",
            "stop_vm",
            stop_vm,
            Some("vm"),
            non_empty(&request.get_ref().vm_id)
        )
    }

    async fn reboot_vm(
        &self,
        request: Request<proto::RebootVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "RebootVm",
            "reboot_vm",
            reboot_vm,
            Some("vm"),
            non_empty(&request.get_ref().vm_id)
        )
    }

    async fn delete_vm(
        &self,
        request: Request<proto::DeleteVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "DeleteVm",
            "delete_vm",
            delete_vm,
            Some("vm"),
            non_empty(&request.get_ref().vm_id)
        )
    }

    async fn resize_vm(
        &self,
        request: Request<proto::ResizeVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "ResizeVm",
            "resize_vm",
            resize_vm,
            Some("vm"),
            non_empty(&request.get_ref().vm_id)
        )
    }

    async fn attach_volume(
        &self,
        request: Request<proto::AttachVolumeRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "AttachVolume",
            "attach_volume",
            attach_volume,
            Some("volume"),
            request
                .get_ref()
                .volume
                .as_ref()
                .and_then(|volume| non_empty(&volume.volume_id))
        )
    }

    async fn detach_volume(
        &self,
        request: Request<proto::DetachVolumeRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "DetachVolume",
            "detach_volume",
            detach_volume,
            Some("volume"),
            non_empty(&request.get_ref().volume_id)
        )
    }

    async fn resize_volume(
        &self,
        request: Request<proto::ResizeVolumeRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "ResizeVolume",
            "resize_volume",
            resize_volume,
            Some("volume"),
            non_empty(&request.get_ref().volume_id)
        )
    }

    async fn snapshot_volume(
        &self,
        request: Request<proto::SnapshotVolumeRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "SnapshotVolume",
            "snapshot_volume",
            snapshot_volume,
            Some("volume"),
            non_empty(&request.get_ref().volume_id)
        )
    }

    async fn restore_volume(
        &self,
        request: Request<proto::RestoreVolumeRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "RestoreVolume",
            "restore_volume",
            restore_volume,
            Some("volume"),
            non_empty(&request.get_ref().volume_id)
        )
    }

    async fn delete_volume_snapshot(
        &self,
        request: Request<proto::DeleteVolumeSnapshotRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "DeleteVolumeSnapshot",
            "delete_volume_snapshot",
            delete_volume_snapshot,
            Some("volume"),
            non_empty(&request.get_ref().volume_id)
        )
    }

    async fn clone_volume(
        &self,
        request: Request<proto::CloneVolumeRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "CloneVolume",
            "clone_volume",
            clone_volume,
            Some("volume"),
            non_empty(&request.get_ref().source_volume_id)
        )
    }

    async fn pause_node_scheduling(
        &self,
        request: Request<proto::PauseNodeSchedulingRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "PauseNodeScheduling",
            "pause_node_scheduling",
            pause_node_scheduling,
            Some("node"),
            non_empty(&request.get_ref().node_id)
        )
    }

    async fn resume_node_scheduling(
        &self,
        request: Request<proto::ResumeNodeSchedulingRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "ResumeNodeScheduling",
            "resume_node_scheduling",
            resume_node_scheduling,
            Some("node"),
            non_empty(&request.get_ref().node_id)
        )
    }

    async fn drain_node(
        &self,
        request: Request<proto::DrainNodeRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "DrainNode",
            "drain_node",
            drain_node,
            Some("node"),
            non_empty(&request.get_ref().node_id)
        )
    }

    async fn enter_maintenance(
        &self,
        request: Request<proto::EnterMaintenanceRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "EnterMaintenance",
            "enter_maintenance",
            enter_maintenance,
            Some("node"),
            non_empty(&request.get_ref().node_id)
        )
    }

    async fn exit_maintenance(
        &self,
        request: Request<proto::ExitMaintenanceRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "ExitMaintenance",
            "exit_maintenance",
            exit_maintenance,
            Some("node"),
            non_empty(&request.get_ref().node_id)
        )
    }

    async fn pause_vm(
        &self,
        request: Request<proto::PauseVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "PauseVm",
            "pause_vm",
            pause_vm,
            Some("vm"),
            non_empty(&request.get_ref().vm_id)
        )
    }

    async fn resume_vm(
        &self,
        request: Request<proto::ResumeVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "ResumeVm",
            "resume_vm",
            resume_vm,
            Some("vm"),
            non_empty(&request.get_ref().vm_id)
        )
    }

    async fn power_button_vm(
        &self,
        request: Request<proto::PowerButtonVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "PowerButtonVm",
            "power_button_vm",
            power_button_vm,
            Some("vm"),
            non_empty(&request.get_ref().vm_id)
        )
    }

    async fn add_disk(
        &self,
        request: Request<proto::AddDiskRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "AddDisk",
            "add_disk",
            add_disk,
            Some("vm"),
            non_empty(&request.get_ref().vm_id)
        )
    }

    async fn remove_device(
        &self,
        request: Request<proto::RemoveDeviceRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "RemoveDevice",
            "remove_device",
            remove_device,
            Some("vm"),
            non_empty(&request.get_ref().vm_id)
        )
    }

    async fn add_net(
        &self,
        request: Request<proto::AddNetRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "AddNet",
            "add_net",
            add_net,
            Some("vm"),
            non_empty(&request.get_ref().vm_id)
        )
    }

    async fn resize_disk(
        &self,
        request: Request<proto::ResizeDiskRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "ResizeDisk",
            "resize_disk",
            resize_disk,
            Some("vm"),
            non_empty(&request.get_ref().vm_id)
        )
    }

    async fn snapshot_vm(
        &self,
        request: Request<proto::SnapshotVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "SnapshotVm",
            "snapshot_vm",
            snapshot_vm,
            Some("vm"),
            non_empty(&request.get_ref().vm_id)
        )
    }

    async fn restore_snapshot(
        &self,
        request: Request<proto::RestoreSnapshotRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "RestoreSnapshot",
            "restore_snapshot",
            restore_snapshot,
            Some("vm"),
            non_empty(&request.get_ref().vm_id)
        )
    }

    async fn coredump_vm(
        &self,
        request: Request<proto::CoredumpVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "CoredumpVm",
            "coredump_vm",
            coredump_vm,
            Some("vm"),
            non_empty(&request.get_ref().vm_id)
        )
    }

    async fn start_network(
        &self,
        request: Request<proto::StartNetworkRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "StartNetwork",
            "start_network",
            start_network,
            Some("network"),
            non_empty(&request.get_ref().network_id)
        )
    }

    async fn stop_network(
        &self,
        request: Request<proto::StopNetworkRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "StopNetwork",
            "stop_network",
            stop_network,
            Some("network"),
            non_empty(&request.get_ref().network_id)
        )
    }

    async fn restart_network(
        &self,
        request: Request<proto::RestartNetworkRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "RestartNetwork",
            "restart_network",
            restart_network,
            Some("network"),
            non_empty(&request.get_ref().network_id)
        )
    }

    /// Authz note: unconditional `Unimplemented` (the control plane does
    /// not proxy `PingVmm`), no request/state access; exempt from
    /// `enforce_peer_node_id`. Read-only: deliberately not audited.
    async fn ping_vmm(
        &self,
        _request: Request<proto::PingVmmRequest>,
    ) -> Result<Response<proto::PingVmmResponse>, Status> {
        Err(tonic::Status::unimplemented(
            "ping_vmm not supported on control plane",
        ))
    }

    /// Transport relay for `MigrateVm`; the fail-closed `pause_first`
    /// invariant is enforced in the lifecycle service so a direct gRPC
    /// caller cannot bypass the BFF/CLI gate (issue #656).
    async fn migrate_vm(
        &self,
        request: Request<proto::MigrateVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "MigrateVm",
            "migrate_vm",
            migrate_vm,
            Some("vm"),
            non_empty(&request.get_ref().vm_id)
        )
    }

    async fn update_overlay(
        &self,
        request: Request<proto::UpdateOverlayRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "UpdateOverlay",
            "update_overlay",
            update_overlay,
            Some("network"),
            non_empty(&request.get_ref().network_id)
        )
    }

    async fn send_gratuitous_arp(
        &self,
        request: Request<proto::SendGratuitousArpRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_lifecycle_call!(
            self,
            request,
            "SendGratuitousArp",
            "send_gratuitous_arp",
            send_gratuitous_arp,
            Some("network"),
            non_empty(&request.get_ref().network_id)
        )
    }

    /// Authz note: the request carries no `node_id` field; the target is
    /// `meta.target_node_id`. This RPC is exposed on the node-facing mTLS
    /// listener, so the only authenticated caller identity available to the
    /// control plane is the peer node certificate. Pin the target to that
    /// peer: a compromised node may resolve its own interrupted operations,
    /// but must not be able to relay a resolution into another node's core
    /// journal.
    async fn resolve_inspect_required_operation(
        &self,
        request: Request<proto::ResolveInspectRequiredOperationRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let target_node_id = request
            .get_ref()
            .meta
            .as_ref()
            .map(|meta| meta.target_node_id.clone())
            .unwrap_or_default();
        let context = GrpcAuditContext::new(
            &request,
            "ResolveInspectRequiredOperation",
            LIFECYCLE_SERVICE_PATH,
            Some("vm"),
            non_empty(&request.get_ref().vm_id),
            None,
        );
        let op_id = extract_op_id(&request).unwrap_or_default();
        let result = async {
            enforce_peer_node_id(
                &request,
                &target_node_id,
                "resolve_inspect_required_operation",
            )?;
            let _span =
                tracing::info_span!("resolve_inspect_required_operation", op_id = %op_id.escape_debug());
            let resp = self
                .service
                .resolve_inspect_required_operation(request.into_inner())
                .instrument(_span)
                .await
                .map_err(tonic::Status::from)?;
            Ok(Response::new(resp))
        }
        .await;
        finish_audited(self.audit.as_ref(), &context, result).await
    }
}

pub struct ReconcileServer {
    service: Arc<dyn ReconcileService>,
    audit: Option<AuditLogRepository>,
}

impl ReconcileServer {
    pub fn new(service: Arc<dyn ReconcileService>) -> Self {
        Self {
            service,
            audit: None,
        }
    }

    /// Enable durable audit emission for desired-state mutations.
    ///
    /// Production constructs every node-facing server in `container.rs` with
    /// `with_audit`; the optional form exists for narrow test fixtures and
    /// embedders.
    pub fn with_audit(mut self, audit: AuditLogRepository) -> Self {
        self.audit = Some(audit);
        self
    }
}

#[tonic::async_trait]
impl proto::reconcile_service_server::ReconcileService for ReconcileServer {
    async fn apply_node_desired_state(
        &self,
        request: Request<proto::ApplyNodeDesiredStateRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_service_call!(
            self,
            request,
            "ApplyNodeDesiredState",
            RECONCILE_SERVICE_PATH,
            "apply_node_desired_state",
            apply_node_desired_state,
            Some("node"),
            non_empty(&request.get_ref().node_id)
        )
    }

    async fn apply_vm_desired_state(
        &self,
        request: Request<proto::ApplyVmDesiredStateRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_service_call!(
            self,
            request,
            "ApplyVmDesiredState",
            RECONCILE_SERVICE_PATH,
            "apply_vm_desired_state",
            apply_vm_desired_state,
            Some("vm"),
            non_empty(&request.get_ref().vm_id)
        )
    }

    async fn apply_volume_desired_state(
        &self,
        request: Request<proto::ApplyVolumeDesiredStateRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_service_call!(
            self,
            request,
            "ApplyVolumeDesiredState",
            RECONCILE_SERVICE_PATH,
            "apply_volume_desired_state",
            apply_volume_desired_state,
            Some("volume"),
            non_empty(&request.get_ref().volume_id)
        )
    }

    async fn apply_network_desired_state(
        &self,
        request: Request<proto::ApplyNetworkDesiredStateRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        audited_service_call!(
            self,
            request,
            "ApplyNetworkDesiredState",
            RECONCILE_SERVICE_PATH,
            "apply_network_desired_state",
            apply_network_desired_state,
            Some("network"),
            non_empty(&request.get_ref().network_id)
        )
    }

    async fn acknowledge_desired_state_version(
        &self,
        request: Request<proto::AcknowledgeDesiredStateVersionRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let fragment = &request.get_ref();
        // `ResourceKind::from_str` is the same parser the service uses and
        // accepts both `vm` and `Vm`; mirror it so an accepted capitalized
        // kind does not silently lose the audit row's resource kind.
        let kind = fragment
            .fragment_kind
            .parse::<ResourceKind>()
            .ok()
            .map(ResourceKind::as_str);
        let resource_id = non_empty(&fragment.fragment_id).or_else(|| non_empty(&fragment.node_id));
        audited_service_call!(
            self,
            request,
            "AcknowledgeDesiredStateVersion",
            RECONCILE_SERVICE_PATH,
            "acknowledge_desired_state_version",
            acknowledge_desired_state_version,
            kind,
            resource_id
        )
    }
}

/// gRPC adapter for the node metric batch service (campaign #602 PR-2).
/// Same peer-identity discipline as every other node-facing RPC: the
/// wire-asserted `node_id` must match the mTLS peer certificate, so a
/// sender can never ingest telemetry under another node's identity.
pub struct MonitoringServer {
    service: Arc<dyn crate::monitoring_ingest::MonitoringIngestService>,
}

impl MonitoringServer {
    pub fn new(service: Arc<dyn crate::monitoring_ingest::MonitoringIngestService>) -> Self {
        Self { service }
    }
}

#[tonic::async_trait]
impl proto::monitoring_service_server::MonitoringService for MonitoringServer {
    async fn ingest_node_metric_batch(
        &self,
        request: Request<proto::NodeMetricBatchRequest>,
    ) -> Result<Response<proto::NodeMetricBatchResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(
            &request,
            &request.get_ref().node_id,
            "ingest_node_metric_batch",
        )?;
        let _span = tracing::info_span!("ingest_node_metric_batch", %op_id);
        let resp = self
            .service
            .ingest_node_metric_batch(request.into_inner())
            .instrument(_span)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "ingest_node_metric_batch failed");
                tonic::Status::from(e)
            })?;
        Ok(Response::new(resp))
    }
}
