//! Durable gRPC audit coverage for the node-facing lifecycle and
//! enrollment boundaries (#658). These tests drive the real server
//! adapters against a `TestDb`, with synthetic peer-identity extensions.

use crate::audit_coupling::AdoptedAudit;
use crate::audit_outbox_dispatcher::AuditOutboxDispatcher;
use crate::enrollment::{
    CertificateIssuer, EnrollmentService, EnrollmentServiceImplementation, IssuedCertificate,
};
use crate::error::ControlPlaneServiceError;
use crate::lifecycle::LifecycleService;
use crate::peer_identity::{InsecurePeer, PeerCertificateSerial, PeerNodeId};
use crate::reconcile::{ReconcileService, ReconcileServiceImplementation};
use crate::revocation::RevocationSnapshot;
use crate::server::{EnrollmentServer, LifecycleServer, ReconcileServer};
use chv_controlplane_store::{
    test_util::TestDb, AuditLogInsert, AuditLogRepository, AuditLogRow, AuditOutboxInsert,
    AuditOutboxRepository, AuditOutboxRow, BootstrapTokenRepository, DesiredStateRepository,
    EventRepository, NodeRepository, NodeUpsertInput, ObservedStateRepository, OperationRepository,
    StoreError, StorePool, VtepRepository, AUDIT_OUTBOX_DISPATCHED, AUDIT_OUTBOX_PENDING,
};
use chv_controlplane_types::domain::NodeId;
use control_plane_node_api::control_plane_node_api as proto;
use control_plane_node_api::control_plane_node_api::enrollment_service_server::EnrollmentService as EnrollmentServiceApi;
use control_plane_node_api::control_plane_node_api::lifecycle_service_server::LifecycleService as LifecycleServiceApi;
use control_plane_node_api::control_plane_node_api::reconcile_service_server::ReconcileService as ReconcileServiceApi;
use std::sync::Arc;
use tonic::{Code, Request};

#[derive(Default)]
struct MockLifecycle {
    fail_start: bool,
    /// When set, the adopted `pause_node_scheduling` path enqueues the row,
    /// commits it, marks the coupling, and then fails anyway — the
    /// forward-compatible shape the patch path must survive.
    enqueue_then_fail: Option<StorePool>,
}

fn ok_ack() -> proto::AckResponse {
    proto::AckResponse {
        result: Some(proto::ResultMeta {
            operation_id: "op-mock".into(),
            status: "OK".into(),
            ..Default::default()
        }),
    }
}

macro_rules! mock_lifecycle_impl {
    ($($name:ident($ty:ty)),* $(,)?; adopted: $($aname:ident($aty:ty)),* $(,)?) => {
        #[tonic::async_trait]
        impl LifecycleService for MockLifecycle {
            async fn start_vm(
                &self,
                _request: proto::StartVmRequest,
            ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
                if self.fail_start {
                    Err(ControlPlaneServiceError::Internal(
                        "mock start failure".into(),
                    ))
                } else {
                    Ok(ok_ack())
                }
            }

            $(
                async fn $name(
                    &self,
                    _request: $ty,
                ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
                    Err(ControlPlaneServiceError::Unsupported(
                        "unused in audit tests".into(),
                    ))
                }
            )*

            // The adopted `SnapshotVolume` arm mirrors the
            // `EnqueueThenFail` shape: commit the outbox row with the
            // intent transaction, mark the coupling, then fail anyway, so
            // the adapter's patch path is exercised on the real wire
            // macro.
            async fn snapshot_volume(
                &self,
                _request: proto::SnapshotVolumeRequest,
                audit: Option<&AdoptedAudit<'_>>,
            ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
                let Some(pool) = &self.enqueue_then_fail else {
                    return Err(ControlPlaneServiceError::Unsupported(
                        "unused in audit tests".into(),
                    ));
                };
                let audit = audit.expect("the adapter passes the coupling");
                let mut tx = pool
                    .begin_with("BEGIN IMMEDIATE;")
                    .await
                    .expect("begin adopted tx");
                chv_controlplane_store::audit_outbox::enqueue_on_tx(&mut tx, audit.insert())
                    .await
                    .expect("enqueue evidence");
                tx.commit().await.expect("commit evidence");
                audit.mark_committed();
                Err(ControlPlaneServiceError::Internal(
                    "post-commit failure".into(),
                ))
            }

            // The five adopted node-transition RPCs take the coupling; the
            // mock never enqueues, so the adapter falls through to the
            // direct path (the shape an adopted call without a ticket has).
            $(
                async fn $aname(
                    &self,
                    _request: $aty,
                    _audit: Option<&AdoptedAudit<'_>>,
                ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
                    Err(ControlPlaneServiceError::Unsupported(
                        "unused in audit tests".into(),
                    ))
                }
            )*

            async fn pause_node_scheduling(
                &self,
                _request: proto::PauseNodeSchedulingRequest,
                audit: Option<&AdoptedAudit<'_>>,
            ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
                let Some(pool) = &self.enqueue_then_fail else {
                    return Err(ControlPlaneServiceError::Unsupported(
                        "unused in audit tests".into(),
                    ));
                };
                let audit = audit.expect("the adapter passes the coupling");
                let mut tx = pool
                    .begin_with("BEGIN IMMEDIATE;")
                    .await
                    .expect("begin adopted tx");
                chv_controlplane_store::audit_outbox::enqueue_on_tx(&mut tx, audit.insert())
                    .await
                    .expect("enqueue evidence");
                tx.commit().await.expect("commit evidence");
                audit.mark_committed();
                Err(ControlPlaneServiceError::Internal(
                    "post-commit failure".into(),
                ))
            }
        }
    };
}

mock_lifecycle_impl! {
    create_vm(proto::CreateVmRequest),
    stop_vm(proto::StopVmRequest),
    reboot_vm(proto::RebootVmRequest),
    delete_vm(proto::DeleteVmRequest),
    resize_vm(proto::ResizeVmRequest),
    attach_volume(proto::AttachVolumeRequest),
    detach_volume(proto::DetachVolumeRequest),
    resize_volume(proto::ResizeVolumeRequest),
    clone_volume(proto::CloneVolumeRequest),
    pause_vm(proto::PauseVmRequest),
    resume_vm(proto::ResumeVmRequest),
    power_button_vm(proto::PowerButtonVmRequest),
    add_disk(proto::AddDiskRequest),
    remove_device(proto::RemoveDeviceRequest),
    add_net(proto::AddNetRequest),
    resize_disk(proto::ResizeDiskRequest),
    snapshot_vm(proto::SnapshotVmRequest),
    restore_snapshot(proto::RestoreSnapshotRequest),
    coredump_vm(proto::CoredumpVmRequest),
    start_network(proto::StartNetworkRequest),
    stop_network(proto::StopNetworkRequest),
    restart_network(proto::RestartNetworkRequest),
    migrate_vm(proto::MigrateVmRequest),
    update_overlay(proto::UpdateOverlayRequest),
    send_gratuitous_arp(proto::SendGratuitousArpRequest),
    resolve_inspect_required_operation(proto::ResolveInspectRequiredOperationRequest),
    ; adopted:
    restore_volume(proto::RestoreVolumeRequest),
    delete_volume_snapshot(proto::DeleteVolumeSnapshotRequest),
    resume_node_scheduling(proto::ResumeNodeSchedulingRequest),
    drain_node(proto::DrainNodeRequest),
    enter_maintenance(proto::EnterMaintenanceRequest),
    exit_maintenance(proto::ExitMaintenanceRequest),
}

#[derive(Default)]
struct MockEnrollment {
    fail_enroll: bool,
}

#[tonic::async_trait]
impl EnrollmentService for MockEnrollment {
    async fn enroll_node(
        &self,
        request: proto::EnrollmentRequest,
        _peer_addr: Option<std::net::SocketAddr>,
        _audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::EnrollmentResponse, ControlPlaneServiceError> {
        if self.fail_enroll {
            return Err(ControlPlaneServiceError::Unauthorized(
                "invalid bootstrap token".into(),
            ));
        }
        let issued_node_id = request
            .inventory
            .as_ref()
            .map(|inventory| inventory.node_id.clone())
            .unwrap_or_default();
        Ok(proto::EnrollmentResponse {
            result: Some(proto::ResultMeta {
                status: "OK".into(),
                ..Default::default()
            }),
            issued_node_id,
            ..Default::default()
        })
    }

    async fn rotate_node_certificate(
        &self,
        request: proto::RotateNodeCertificateRequest,
        _presented_serial: Option<String>,
        _allow_insecure: bool,
        _audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::RotateNodeCertificateResponse, ControlPlaneServiceError> {
        Ok(proto::RotateNodeCertificateResponse {
            result: Some(proto::ResultMeta {
                operation_id: request
                    .meta
                    .as_ref()
                    .map(|meta| meta.operation_id.clone())
                    .unwrap_or_default(),
                status: "OK".into(),
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    async fn report_bootstrap_result(
        &self,
        _request: proto::ReportBootstrapResultRequest,
        _audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        Ok(ok_ack())
    }
}

/// A service that commits its audit evidence and then fails anyway — the
/// forward-compatible shape the check-or-insert fallback must survive.
/// It drives the adapter's patch path: the row exists and the response is
/// a failure, so `finalize_pending` must rewrite outcome/status instead of
/// writing a second row.
struct EnqueueThenFail {
    pool: StorePool,
}

#[tonic::async_trait]
impl EnrollmentService for EnqueueThenFail {
    async fn enroll_node(
        &self,
        _request: proto::EnrollmentRequest,
        _peer_addr: Option<std::net::SocketAddr>,
        _audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::EnrollmentResponse, ControlPlaneServiceError> {
        unreachable!("only report_bootstrap_result is exercised")
    }

    async fn rotate_node_certificate(
        &self,
        _request: proto::RotateNodeCertificateRequest,
        _presented_serial: Option<String>,
        _allow_insecure: bool,
        _audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::RotateNodeCertificateResponse, ControlPlaneServiceError> {
        unreachable!("only report_bootstrap_result is exercised")
    }

    async fn report_bootstrap_result(
        &self,
        _request: proto::ReportBootstrapResultRequest,
        audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let audit = audit.expect("the adapter passes the coupling");
        // The enqueue commits in its own transaction before the failure,
        // which is all this mock needs: the adapter sees a durable outbox
        // row and a failed status. `BEGIN IMMEDIATE` is used here because
        // the row must be durable before the simulated failure; the
        // production enrollment path (`upsert_bootstrap_result`) still
        // opens a deferred `begin()`.
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE;")
            .await
            .expect("begin adopted tx");
        chv_controlplane_store::audit_outbox::enqueue_on_tx(&mut tx, audit.insert())
            .await
            .expect("enqueue evidence");
        tx.commit().await.expect("commit evidence");
        audit.mark_committed();
        // Deliberately breaks "enqueue is the last fallible operation".
        Err(ControlPlaneServiceError::Internal(
            "post-commit failure".into(),
        ))
    }
}

struct MockReconcile;

#[tonic::async_trait]
impl ReconcileService for MockReconcile {
    async fn apply_node_desired_state(
        &self,
        _request: proto::ApplyNodeDesiredStateRequest,
        _audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        Ok(ok_ack())
    }

    async fn apply_vm_desired_state(
        &self,
        _request: proto::ApplyVmDesiredStateRequest,
        _audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        Ok(ok_ack())
    }

    async fn apply_volume_desired_state(
        &self,
        _request: proto::ApplyVolumeDesiredStateRequest,
        _audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        Ok(ok_ack())
    }

    async fn apply_network_desired_state(
        &self,
        _request: proto::ApplyNetworkDesiredStateRequest,
        _audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        Ok(ok_ack())
    }

    async fn acknowledge_desired_state_version(
        &self,
        _request: proto::AcknowledgeDesiredStateVersionRequest,
        _audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        Ok(ok_ack())
    }
}

fn meta(operation_id: &str) -> proto::RequestMeta {
    proto::RequestMeta {
        operation_id: operation_id.to_string(),
        ..Default::default()
    }
}

fn peer_request<T>(message: T, peer_node_id: &str) -> Request<T> {
    let mut request = Request::new(message);
    request.extensions_mut().insert(PeerNodeId(
        NodeId::new(peer_node_id.to_string()).expect("valid test node id"),
    ));
    request
}

async fn audit_rows(pool: &StorePool) -> Vec<AuditLogRow> {
    sqlx::query_as::<_, AuditLogRow>(
        "SELECT * FROM audit_log ORDER BY occurred_at_ms ASC, audit_id ASC",
    )
    .fetch_all(pool)
    .await
    .expect("query audit rows")
}

fn assert_metadata_is_allowlisted(metadata: &str, auth_source: &str) {
    let value: serde_json::Value = serde_json::from_str(metadata).expect("metadata json");
    let object = value.as_object().expect("metadata object");
    assert_eq!(object.len(), 2, "metadata must only carry the allowlist");
    assert_eq!(value["transport"], "grpc");
    assert_eq!(value["auth_source"], auth_source);
}

#[tokio::test]
async fn server_audit_lifecycle_success_writes_one_row() {
    let db = TestDb::new().await;
    let server = LifecycleServer::new(Arc::new(MockLifecycle::default()))
        .with_audit(AuditLogRepository::new(db.pool.clone()));

    let mut request = peer_request(
        proto::StartVmRequest {
            meta: Some(meta("op-start")),
            node_id: "node-a".into(),
            vm_id: "vm-1".into(),
        },
        "node-a",
    );
    request.metadata_mut().insert(
        chv_common::OPERATION_ID_METADATA_KEY,
        "op-start".parse().expect("metadata value"),
    );
    server.start_vm(request).await.expect("start vm succeeds");

    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1, "exactly one row per mutation");
    let row = &rows[0];
    assert_eq!(row.action, "StartVm");
    assert_eq!(row.method, "gRPC");
    assert_eq!(
        row.path,
        "chv.controlplane.node.v1.LifecycleService/StartVm"
    );
    assert_eq!(row.actor_id, "node-a");
    assert_eq!(row.actor_username, "node-a");
    assert_eq!(row.actor_role, "node");
    assert_eq!(row.resource_kind.as_deref(), Some("vm"));
    assert_eq!(row.resource_id.as_deref(), Some("vm-1"));
    assert_eq!(row.outcome, "success");
    assert_eq!(row.status_code, 200);
    assert_eq!(row.operation_id.as_deref(), Some("op-start"));
    assert_eq!(row.correlation_id, None);
    assert!(row.source_ip.is_none());
    assert_metadata_is_allowlisted(&row.metadata, "mtls");
}

#[tokio::test]
async fn server_audit_lifecycle_peer_mismatch_writes_denied_row() {
    let db = TestDb::new().await;
    let server = LifecycleServer::new(Arc::new(MockLifecycle::default()))
        .with_audit(AuditLogRepository::new(db.pool.clone()));

    // Actor is node-a; the wire claims node-b. Validation denies before
    // the service call, and the attempt is still recorded.
    let request = peer_request(
        proto::StartVmRequest {
            meta: Some(meta("op-denied")),
            node_id: "node-b".into(),
            vm_id: "vm-2".into(),
        },
        "node-a",
    );
    let status = server
        .start_vm(request)
        .await
        .expect_err("peer mismatch must be denied");
    assert_eq!(status.code(), Code::PermissionDenied);

    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.action, "StartVm");
    assert_eq!(row.actor_id, "node-a", "actor is the certificate identity");
    assert_eq!(row.actor_role, "node");
    assert_eq!(row.resource_id.as_deref(), Some("vm-2"));
    assert_eq!(row.outcome, "denied");
    assert_eq!(row.status_code, 403);
}

#[tokio::test]
async fn server_audit_lifecycle_audit_write_failure_returns_internal() {
    let db = TestDb::new().await;
    // Make only the audit table unavailable; the lifecycle mutation itself
    // still succeeds, so the adapter must fail closed.
    sqlx::query("DROP TABLE audit_log")
        .execute(&db.pool)
        .await
        .expect("drop audit table");
    let server = LifecycleServer::new(Arc::new(MockLifecycle::default()))
        .with_audit(AuditLogRepository::new(db.pool.clone()));

    let request = peer_request(
        proto::StartVmRequest {
            meta: Some(meta("op-no-audit")),
            node_id: "node-a".into(),
            vm_id: "vm-3".into(),
        },
        "node-a",
    );
    let status = server
        .start_vm(request)
        .await
        .expect_err("successful mutation without an audit row must fail closed");
    assert_eq!(status.code(), Code::Internal);
    assert!(
        status.message().contains("AUDIT_LOG_WRITE_FAILED"),
        "message must be operator-greppable, got: {}",
        status.message()
    );
}

#[tokio::test]
async fn server_audit_enrollment_certificate_rotation_writes_row() {
    let db = TestDb::new().await;
    let server = EnrollmentServer::new(Arc::new(MockEnrollment::default()))
        .with_audit(AuditLogRepository::new(db.pool.clone()));

    let request = peer_request(
        proto::RotateNodeCertificateRequest {
            meta: Some(meta("op-rotate")),
            node_id: "node-a".into(),
        },
        "node-a",
    );
    server
        .rotate_node_certificate(request)
        .await
        .expect("rotation succeeds");

    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.action, "RotateNodeCertificate");
    assert_eq!(row.method, "gRPC");
    assert_eq!(
        row.path,
        "chv.controlplane.node.v1.EnrollmentService/RotateNodeCertificate"
    );
    assert_eq!(row.actor_id, "node-a");
    assert_eq!(row.actor_role, "node");
    assert_eq!(row.resource_kind.as_deref(), Some("node"));
    assert_eq!(row.resource_id.as_deref(), Some("node-a"));
    assert_eq!(row.outcome, "success");
    assert_eq!(row.status_code, 200);
    assert_metadata_is_allowlisted(&row.metadata, "mtls");
}

#[tokio::test]
async fn server_audit_enroll_node_is_anonymous_and_uses_issued_id() {
    let db = TestDb::new().await;
    let server = EnrollmentServer::new(Arc::new(MockEnrollment::default()))
        .with_audit(AuditLogRepository::new(db.pool.clone()));

    // Bootstrap enrollment is token-authenticated: even if a peer
    // extension is present, the actor must be the explicit anonymous
    // marker, never the wire-asserted inventory id.
    let request = peer_request(
        proto::EnrollmentRequest {
            bootstrap_token: "bootstrap-secret-token".into(),
            inventory: Some(proto::NodeInventory {
                node_id: "node-enrolled".into(),
                ..Default::default()
            }),
            ..Default::default()
        },
        "node-untrusted-claim",
    );
    server
        .enroll_node(request)
        .await
        .expect("enrollment succeeds");

    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.action, "EnrollNode");
    assert_eq!(row.method, "gRPC");
    assert_eq!(
        row.path,
        "chv.controlplane.node.v1.EnrollmentService/EnrollNode"
    );
    assert_eq!(row.actor_id, "anonymous");
    assert_eq!(row.actor_username, "anonymous");
    assert_eq!(row.actor_role, "anonymous");
    assert_eq!(row.resource_kind.as_deref(), Some("node"));
    assert_eq!(row.resource_id.as_deref(), Some("node-enrolled"));
    assert_eq!(row.outcome, "success");
    assert_eq!(row.status_code, 200);
    assert_metadata_is_allowlisted(&row.metadata, "bootstrap_token");
    assert!(
        !row.metadata.contains("bootstrap-secret-token"),
        "token must never reach the audit row"
    );
}

fn insecure_request<T>(message: T) -> Request<T> {
    let mut request = Request::new(message);
    request.extensions_mut().insert(InsecurePeer);
    request
}

#[tokio::test]
async fn server_audit_rotation_rejected_for_revoked_peer_writes_denied_row() {
    let db = TestDb::new().await;
    let server = EnrollmentServer::new(Arc::new(MockEnrollment::default()))
        .with_revocation(RevocationSnapshot::from_serials(["revoked-serial"]))
        .with_audit(AuditLogRepository::new(db.pool.clone()));

    let mut request = peer_request(
        proto::RotateNodeCertificateRequest {
            meta: Some(meta("op-rotate-revoked")),
            node_id: "node-a".into(),
        },
        "node-a",
    );
    request
        .extensions_mut()
        .insert(PeerCertificateSerial(Some("revoked-serial".into())));
    let status = server
        .rotate_node_certificate(request)
        .await
        .expect_err("revoked peer must not rotate");
    assert_eq!(status.code(), Code::Unauthenticated);

    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1, "denial must still be recorded");
    let row = &rows[0];
    assert_eq!(row.action, "RotateNodeCertificate");
    assert_eq!(row.actor_id, "node-a");
    assert_eq!(row.outcome, "denied");
    assert_eq!(row.status_code, 401);
    assert_eq!(row.resource_id.as_deref(), Some("node-a"));
    assert_metadata_is_allowlisted(&row.metadata, "mtls");
}

#[tokio::test]
async fn server_audit_unimplemented_shim_writes_failed_row() {
    let db = TestDb::new().await;
    let server = LifecycleServer::new(Arc::new(MockLifecycle::default()))
        .with_audit(AuditLogRepository::new(db.pool.clone()));

    let request = peer_request(
        proto::CreateVolumeRequest {
            meta: Some(meta("op-unimplemented")),
            node_id: "node-a".into(),
            volume: Some(proto::VolumeMutationSpec {
                volume_id: "vol-shim".into(),
                ..Default::default()
            }),
        },
        "node-a",
    );
    let status = server
        .create_volume(request)
        .await
        .expect_err("shim always returns Unimplemented");
    assert_eq!(status.code(), Code::Unimplemented);

    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1, "shim attempts are audited");
    let row = &rows[0];
    assert_eq!(row.action, "CreateVolume");
    assert_eq!(row.resource_kind.as_deref(), Some("volume"));
    assert_eq!(row.resource_id.as_deref(), Some("vol-shim"));
    assert_eq!(row.outcome, "failed");
    assert_eq!(row.status_code, 501);
    assert_eq!(row.actor_id, "node-a");
}

#[tokio::test]
async fn server_audit_service_failure_preserves_status_and_records_failed_row() {
    let db = TestDb::new().await;
    let server = LifecycleServer::new(Arc::new(MockLifecycle {
        fail_start: true,
        ..Default::default()
    }))
    .with_audit(AuditLogRepository::new(db.pool.clone()));

    let request = peer_request(
        proto::StartVmRequest {
            meta: Some(meta("op-service-fail")),
            node_id: "node-a".into(),
            vm_id: "vm-fail".into(),
        },
        "node-a",
    );
    let mut request = request;
    request.metadata_mut().insert(
        chv_common::OPERATION_ID_METADATA_KEY,
        "op-service-fail".parse().expect("metadata value"),
    );
    let status = server
        .start_vm(request)
        .await
        .expect_err("mock service fails");
    assert_eq!(status.code(), Code::Internal);
    assert!(
        !status.message().contains("AUDIT_LOG_WRITE_FAILED"),
        "a handler failure must not be masked as an audit-write failure: {}",
        status.message()
    );

    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.action, "StartVm");
    assert_eq!(row.outcome, "failed");
    assert_eq!(row.status_code, 500);
    assert_eq!(row.operation_id.as_deref(), Some("op-service-fail"));
}

#[tokio::test]
async fn server_audit_report_bootstrap_result_writes_row() {
    let db = TestDb::new().await;
    let server = EnrollmentServer::new(Arc::new(MockEnrollment::default()))
        .with_audit(AuditLogRepository::new(db.pool.clone()));

    let request = peer_request(
        proto::ReportBootstrapResultRequest {
            meta: Some(meta("op-bootstrap")),
            node_id: "node-a".into(),
            bootstrap_status: "ok".into(),
            message: String::new(),
        },
        "node-a",
    );
    server
        .report_bootstrap_result(request)
        .await
        .expect("bootstrap report succeeds");

    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.action, "ReportBootstrapResult");
    assert_eq!(row.actor_id, "node-a");
    assert_eq!(row.resource_kind.as_deref(), Some("node"));
    assert_eq!(row.resource_id.as_deref(), Some("node-a"));
    assert_eq!(row.outcome, "success");
    assert_eq!(row.status_code, 200);
    assert_metadata_is_allowlisted(&row.metadata, "mtls");
}

#[tokio::test]
async fn server_audit_failed_enroll_is_anonymous_with_inventory_fallback() {
    let db = TestDb::new().await;
    let server = EnrollmentServer::new(Arc::new(MockEnrollment { fail_enroll: true }))
        .with_audit(AuditLogRepository::new(db.pool.clone()));

    let request = peer_request(
        proto::EnrollmentRequest {
            bootstrap_token: "bootstrap-secret-token".into(),
            inventory: Some(proto::NodeInventory {
                node_id: "node-claimed".into(),
                ..Default::default()
            }),
            ..Default::default()
        },
        "node-untrusted-claim",
    );
    let status = server
        .enroll_node(request)
        .await
        .expect_err("enrollment fails");
    assert_eq!(status.code(), Code::Unauthenticated);

    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.action, "EnrollNode");
    assert_eq!(row.actor_id, "anonymous");
    assert_eq!(
        row.resource_id.as_deref(),
        Some("node-claimed"),
        "a failed enrollment falls back to the wire-asserted inventory id"
    );
    assert_eq!(row.outcome, "denied");
    assert_eq!(row.status_code, 401);
    assert_metadata_is_allowlisted(&row.metadata, "bootstrap_token");
    assert!(!row.metadata.contains("bootstrap-secret-token"));
}

#[tokio::test]
async fn server_audit_insecure_lifecycle_is_anonymous_auth_source_insecure() {
    let db = TestDb::new().await;
    let server = LifecycleServer::new(Arc::new(MockLifecycle::default()))
        .with_audit(AuditLogRepository::new(db.pool.clone()));

    let request = insecure_request(proto::StartVmRequest {
        meta: Some(meta("op-insecure")),
        node_id: "node-dev".into(),
        vm_id: "vm-dev".into(),
    });
    server
        .start_vm(request)
        .await
        .expect("insecure dev mode accepts the asserted node id");

    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.actor_id, "anonymous");
    assert_eq!(row.actor_role, "anonymous");
    assert_eq!(row.resource_id.as_deref(), Some("vm-dev"));
    assert_metadata_is_allowlisted(&row.metadata, "insecure");
}

#[tokio::test]
async fn server_audit_reconcile_apply_node_desired_state_writes_row() {
    let db = TestDb::new().await;
    let server = ReconcileServer::new(Arc::new(MockReconcile))
        .with_audit(AuditLogRepository::new(db.pool.clone()));

    let request = peer_request(
        proto::ApplyNodeDesiredStateRequest {
            meta: Some(meta("op-reconcile")),
            node_id: "node-a".into(),
            fragment: Some(proto::DesiredStateFragment {
                id: "node-a".into(),
                kind: "node".into(),
                ..Default::default()
            }),
        },
        "node-a",
    );
    server
        .apply_node_desired_state(request)
        .await
        .expect("apply node desired state succeeds");

    // `MockReconcile` ignores the ticket the adapter hands it, so this is
    // the "service did not adopt" shape: the row is absent and today's
    // direct insert runs unchanged.
    assert!(
        outbox_rows(&db.pool).await.is_empty(),
        "an unadopted reconcile RPC never touches the outbox"
    );
    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.action, "ApplyNodeDesiredState");
    assert_eq!(
        row.path,
        "chv.controlplane.node.v1.ReconcileService/ApplyNodeDesiredState"
    );
    assert_eq!(row.actor_id, "node-a");
    assert_eq!(row.resource_kind.as_deref(), Some("node"));
    assert_eq!(row.resource_id.as_deref(), Some("node-a"));
    assert_eq!(row.outcome, "success");
    assert_eq!(row.status_code, 200);
    assert_metadata_is_allowlisted(&row.metadata, "mtls");
}

#[tokio::test]
async fn server_audit_reconcile_acknowledge_accepts_capitalized_kind() {
    let db = TestDb::new().await;
    let server = ReconcileServer::new(Arc::new(MockReconcile))
        .with_audit(AuditLogRepository::new(db.pool.clone()));

    let request = peer_request(
        proto::AcknowledgeDesiredStateVersionRequest {
            meta: Some(meta("op-ack")),
            node_id: "node-a".into(),
            fragment_kind: "Vm".into(),
            fragment_id: "vm-ack".into(),
            observed_generation: "2".into(),
            apply_status: "applied".into(),
        },
        "node-a",
    );
    server
        .acknowledge_desired_state_version(request)
        .await
        .expect("acknowledge succeeds");

    assert!(
        outbox_rows(&db.pool).await.is_empty(),
        "an unadopted reconcile RPC never touches the outbox"
    );
    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.action, "AcknowledgeDesiredStateVersion");
    assert_eq!(
        row.resource_kind.as_deref(),
        Some("vm"),
        "the audit kind must mirror the service's accepted parser"
    );
    assert_eq!(row.resource_id.as_deref(), Some("vm-ack"));
    assert_eq!(row.outcome, "success");
}

// ---------------------------------------------------------------------------
// #706 PR 3a — enrollment RPCs coupled to the transactional audit outbox.
//
// These tests drive the *real* `EnrollmentServiceImplementation` (not a
// mock), so the outbox row is enqueued inside the service's own mutation
// transaction exactly as in production. Mocks above still cover the
// unadopted fallback shape (a service that ignores the ticket).
// ---------------------------------------------------------------------------

/// Multi-use bootstrap token seeded for the adopted-enrollment tests
/// (`sha256("123")`, the fixture used by the enrollment unit tests).
const TEST_TOKEN: &str = "123";
const TEST_TOKEN_HASH: &str = "a665a45920422f9d417e4867efdc4fb8a04a1f3fff1fa07e998e86f7f7a27ae3";

struct FixedSerialIssuer;

#[async_trait::async_trait]
impl CertificateIssuer for FixedSerialIssuer {
    async fn issue_node_certificate(
        &self,
        _node_id: &NodeId,
    ) -> Result<IssuedCertificate, ControlPlaneServiceError> {
        Ok(IssuedCertificate {
            certificate_pem: vec![],
            private_key_pem: vec![],
            ca_pem: vec![],
            serial: "serial-adopted".into(),
        })
    }
}

fn adopted_service(db: &TestDb) -> EnrollmentServiceImplementation {
    EnrollmentServiceImplementation::new(
        NodeRepository::new(db.pool.clone()),
        BootstrapTokenRepository::new(db.pool.clone()),
        Some(Arc::new(FixedSerialIssuer)),
        VtepRepository::new(db.pool.clone()),
    )
}

fn adopted_server(db: &TestDb) -> EnrollmentServer {
    EnrollmentServer::new(Arc::new(adopted_service(db)))
        .with_audit(AuditLogRepository::new(db.pool.clone()))
}

async fn seed_bootstrap_token(pool: &StorePool) {
    sqlx::query("INSERT INTO bootstrap_tokens (token_hash, one_time_use) VALUES (?, false)")
        .bind(TEST_TOKEN_HASH)
        .execute(pool)
        .await
        .expect("seed bootstrap token");
}

async fn seed_node(db: &TestDb, node_id: &str, serial: Option<&str>) {
    NodeRepository::new(db.pool.clone())
        .upsert_node(&NodeUpsertInput {
            node_id: NodeId::new(node_id).expect("valid node id"),
            hostname: node_id.into(),
            display_name: node_id.into(),
            certificate_serial: serial.map(str::to_string),
            agent_version: Some("0.1.0".into()),
            control_plane_version: Some("0.1.0".into()),
            enrolled_unix_ms: 1_000,
            last_seen_unix_ms: 1_000,
        })
        .await
        .expect("seed node");
}

async fn outbox_rows(pool: &StorePool) -> Vec<AuditOutboxRow> {
    sqlx::query_as::<_, AuditOutboxRow>(
        "SELECT * FROM audit_outbox ORDER BY created_at_ms ASC, outbox_id ASC",
    )
    .fetch_all(pool)
    .await
    .expect("query outbox rows")
}

fn dispatcher(pool: &StorePool) -> AuditOutboxDispatcher {
    AuditOutboxDispatcher::new(
        AuditOutboxRepository::new(pool.clone()),
        7 * 24 * 60 * 60 * 1_000,
    )
}

fn enrollment_request(node_id: &str, wireguard_public_key: &str) -> proto::EnrollmentRequest {
    proto::EnrollmentRequest {
        bootstrap_token: TEST_TOKEN.into(),
        inventory: Some(proto::NodeInventory {
            node_id: node_id.into(),
            hostname: format!("{node_id}-host"),
            architecture: "x86_64".into(),
            cpu_threads: 4,
            memory_bytes: 8 * 1024 * 1024 * 1024,
            vtep_ip: String::new(),
            wireguard_public_key: wireguard_public_key.into(),
            underlay_mtu: 1400,
            ..Default::default()
        }),
        versions: Some(proto::ServiceVersions {
            node_id: node_id.into(),
            chv_agent_version: "1.2.3".into(),
            chv_stord_version: "1.2.3".into(),
            chv_nwd_version: "1.2.3".into(),
            cloud_hypervisor_version: "40.0.0".into(),
            host_bundle_version: "1.2.3".into(),
        }),
    }
}

/// Drive one dispatch pass at or after the row's due time and assert it
/// converted exactly one row.
async fn dispatch_one(db: &TestDb, due_at_ms: i64) -> Vec<AuditLogRow> {
    let dispatched = dispatcher(&db.pool)
        .dispatch_pass(due_at_ms + 1)
        .await
        .expect("dispatch pass");
    assert_eq!(dispatched, 1, "exactly one outbox row dispatches");
    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1, "dispatch inserts exactly one evidence row");
    rows
}

#[tokio::test]
async fn server_audit_enroll_node_adopts_outbox_and_dispatches_exactly_once() {
    let db = TestDb::new().await;
    seed_bootstrap_token(&db.pool).await;
    let server = adopted_server(&db);

    let request = peer_request(
        enrollment_request("node-enrolled", "cHViLWVucm9sbGVk"),
        "node-untrusted-claim",
    );
    server
        .enroll_node(request)
        .await
        .expect("enrollment succeeds");

    // The adapter must not write the direct row: the evidence committed
    // with the fabric-identity transaction (the final fail-closed write).
    assert!(
        audit_rows(&db.pool).await.is_empty(),
        "adopted success skips the post-hoc direct insert"
    );
    let rows = outbox_rows(&db.pool).await;
    assert_eq!(rows.len(), 1, "exactly one outbox row per mutation");
    let row = &rows[0];
    assert_eq!(row.status, AUDIT_OUTBOX_PENDING);
    assert!(
        !row.audit_id.is_empty() && row.audit_id != row.outbox_id,
        "a separate audit_id was pre-minted"
    );
    let payload = row.parse_payload().expect("payload parses");
    assert_eq!(payload.action, "EnrollNode");
    assert_eq!(
        payload.path,
        "chv.controlplane.node.v1.EnrollmentService/EnrollNode"
    );
    assert_eq!(payload.actor_id, "anonymous");
    assert_eq!(payload.actor_role, "anonymous");
    assert_eq!(payload.resource_kind.as_deref(), Some("node"));
    assert_eq!(payload.resource_id.as_deref(), Some("node-enrolled"));
    assert_eq!(payload.outcome, "success");
    assert_eq!(payload.status_code, 200);
    assert_metadata_is_allowlisted(&payload.metadata, "bootstrap_token");

    let dispatched = dispatch_one(&db, row.created_at_ms).await;
    assert_eq!(dispatched[0].audit_id, row.audit_id, "audit_id is reused");
    assert_eq!(dispatched[0].action, "EnrollNode");
    assert_eq!(dispatched[0].outcome, "success");
    assert_eq!(
        AuditOutboxRepository::new(db.pool.clone())
            .get(&row.outbox_id)
            .await
            .expect("get outbox row")
            .expect("row exists")
            .status,
        AUDIT_OUTBOX_DISPATCHED
    );
}

#[tokio::test]
async fn server_audit_enroll_node_without_fabric_identity_couples_version_append() {
    let db = TestDb::new().await;
    seed_bootstrap_token(&db.pool).await;
    let server = adopted_server(&db);

    // No fabric identity: the version-history append is the final
    // fail-closed write of the sequence, so the ticket rides that tx.
    let request = peer_request(
        enrollment_request("node-no-fabric", ""),
        "node-untrusted-claim",
    );
    server
        .enroll_node(request)
        .await
        .expect("enrollment succeeds");

    assert!(audit_rows(&db.pool).await.is_empty());
    let rows = outbox_rows(&db.pool).await;
    assert_eq!(
        rows.len(),
        1,
        "the evidence still commits with the mutation"
    );
    assert_eq!(rows[0].status, AUDIT_OUTBOX_PENDING);

    let dispatched = dispatch_one(&db, rows[0].created_at_ms).await;
    assert_eq!(dispatched[0].action, "EnrollNode");
    assert_eq!(dispatched[0].resource_id.as_deref(), Some("node-no-fabric"));
}

#[tokio::test]
async fn server_audit_rotate_node_certificate_adopts_outbox_and_dispatches_exactly_once() {
    let db = TestDb::new().await;
    seed_node(&db, "node-a", Some("old-serial")).await;
    let server = adopted_server(&db);

    let mut request = peer_request(
        proto::RotateNodeCertificateRequest {
            meta: Some(meta("op-rotate-adopted")),
            node_id: "node-a".into(),
        },
        "node-a",
    );
    request
        .extensions_mut()
        .insert(PeerCertificateSerial(Some("old-serial".into())));
    server
        .rotate_node_certificate(request)
        .await
        .expect("rotation succeeds");

    // The compare-and-swap really committed and the audit row did not.
    assert_eq!(
        NodeRepository::new(db.pool.clone())
            .get_certificate_serial(&NodeId::new("node-a").unwrap())
            .await
            .expect("read serial")
            .as_deref(),
        Some("serial-adopted")
    );
    assert!(audit_rows(&db.pool).await.is_empty(), "no direct insert");
    let rows = outbox_rows(&db.pool).await;
    assert_eq!(rows.len(), 1);
    let payload = rows[0].parse_payload().expect("payload parses");
    assert_eq!(payload.action, "RotateNodeCertificate");
    assert_eq!(
        payload.path,
        "chv.controlplane.node.v1.EnrollmentService/RotateNodeCertificate"
    );
    assert_eq!(payload.actor_id, "node-a");
    assert_eq!(payload.actor_role, "node");
    assert_eq!(payload.resource_id.as_deref(), Some("node-a"));
    assert_eq!(payload.outcome, "success");
    assert_eq!(payload.status_code, 200);
    assert_metadata_is_allowlisted(&payload.metadata, "mtls");

    let dispatched = dispatch_one(&db, rows[0].created_at_ms).await;
    assert_eq!(dispatched[0].audit_id, rows[0].audit_id);
    assert_eq!(dispatched[0].action, "RotateNodeCertificate");
}

#[tokio::test]
async fn server_audit_report_bootstrap_result_adopts_outbox_and_dispatches_exactly_once() {
    let db = TestDb::new().await;
    seed_node(&db, "node-a", Some("old-serial")).await;
    let server = adopted_server(&db);

    let request = peer_request(
        proto::ReportBootstrapResultRequest {
            meta: Some(meta("op-bootstrap-adopted")),
            node_id: "node-a".into(),
            bootstrap_status: "ok".into(),
            message: String::new(),
        },
        "node-a",
    );
    server
        .report_bootstrap_result(request)
        .await
        .expect("bootstrap report succeeds");

    assert!(audit_rows(&db.pool).await.is_empty(), "no direct insert");
    let rows = outbox_rows(&db.pool).await;
    assert_eq!(rows.len(), 1);
    let payload = rows[0].parse_payload().expect("payload parses");
    assert_eq!(payload.action, "ReportBootstrapResult");
    assert_eq!(
        payload.path,
        "chv.controlplane.node.v1.EnrollmentService/ReportBootstrapResult"
    );
    assert_eq!(payload.actor_id, "node-a");
    assert_eq!(payload.resource_id.as_deref(), Some("node-a"));
    assert_eq!(payload.outcome, "success");
    assert_eq!(payload.status_code, 200);

    let dispatched = dispatch_one(&db, rows[0].created_at_ms).await;
    assert_eq!(dispatched[0].audit_id, rows[0].audit_id);
    assert_eq!(dispatched[0].action, "ReportBootstrapResult");
}

#[tokio::test]
async fn server_audit_rotate_mismatched_serial_writes_no_outbox_row_and_one_denied_row() {
    let db = TestDb::new().await;
    seed_node(&db, "node-a", Some("old-serial")).await;
    let server = adopted_server(&db);

    let mut request = peer_request(
        proto::RotateNodeCertificateRequest {
            meta: Some(meta("op-rotate-lost")),
            node_id: "node-a".into(),
        },
        "node-a",
    );
    request
        .extensions_mut()
        .insert(PeerCertificateSerial(Some("stale-serial".into())));
    let status = server
        .rotate_node_certificate(request)
        .await
        .expect_err("a mismatched presented serial must fail closed");
    assert_eq!(status.code(), Code::PermissionDenied);

    // The mutation never ran: no certificate was minted, nothing was
    // enqueued, and the serial did not move. The adapter records exactly
    // one denied row on the direct path.
    assert!(
        outbox_rows(&db.pool).await.is_empty(),
        "a rejected rotation enqueues nothing"
    );
    assert_eq!(
        NodeRepository::new(db.pool.clone())
            .get_certificate_serial(&NodeId::new("node-a").unwrap())
            .await
            .expect("read serial")
            .as_deref(),
        Some("old-serial"),
        "the rotation must not commit"
    );
    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1, "exactly one direct denied row");
    assert_eq!(rows[0].action, "RotateNodeCertificate");
    assert_eq!(rows[0].outcome, "denied");
    assert_eq!(rows[0].status_code, 403);
    assert_eq!(rows[0].actor_id, "node-a");
}

#[tokio::test]
async fn server_audit_failed_adopted_enroll_writes_only_the_direct_denied_row() {
    let db = TestDb::new().await;
    // No token seeded: the mutation fails before any store write.
    let server = adopted_server(&db);

    let request = peer_request(enrollment_request("node-claimed", "cHVi"), "node-claimed");
    let status = server
        .enroll_node(request)
        .await
        .expect_err("invalid bootstrap token is denied");
    assert_eq!(status.code(), Code::Unauthenticated);

    assert!(
        outbox_rows(&db.pool).await.is_empty(),
        "a rejected enrollment enqueues nothing"
    );
    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1, "exactly one direct failed/denied row");
    assert_eq!(rows[0].action, "EnrollNode");
    assert_eq!(rows[0].outcome, "denied");
    assert_eq!(rows[0].status_code, 401);
    assert_eq!(rows[0].actor_id, "anonymous");
    assert_eq!(rows[0].resource_id.as_deref(), Some("node-claimed"));
}

#[tokio::test]
async fn server_audit_adopted_crash_replay_writes_exactly_one_row() {
    let db = TestDb::new().await;
    seed_node(&db, "node-a", Some("old-serial")).await;
    let service = adopted_service(&db);

    // Deterministic crash: commit the mutation + outbox row, then never run
    // the adapter's `finish_audited` (the process died before it).
    let now = chv_common::now_unix_ms();
    let insert = AuditOutboxInsert {
        outbox_id: "ob-crash-replay".into(),
        audit_id: "audit-crash-replay".into(),
        payload: AuditLogInsert {
            occurred_at_ms: now,
            actor_id: "node-a".into(),
            actor_username: "node-a".into(),
            actor_role: "node".into(),
            action: "ReportBootstrapResult".into(),
            method: "gRPC".into(),
            path: "chv.controlplane.node.v1.EnrollmentService/ReportBootstrapResult".into(),
            resource_kind: Some("node".into()),
            resource_id: Some("node-a".into()),
            outcome: "success".into(),
            status_code: 200,
            correlation_id: None,
            operation_id: Some("op-crash".into()),
            source_ip: None,
            metadata: r#"{"transport":"grpc","auth_source":"mtls"}"#.into(),
        },
        created_at_ms: now,
    };
    let committed = std::sync::atomic::AtomicBool::new(false);
    let coupling = AdoptedAudit::new(&insert, &committed);
    service
        .report_bootstrap_result(
            proto::ReportBootstrapResultRequest {
                meta: Some(meta("op-crash")),
                node_id: "node-a".into(),
                bootstrap_status: "ok".into(),
                message: String::new(),
            },
            Some(&coupling),
        )
        .await
        .expect("mutation commits with its outbox row");
    assert!(
        coupling.was_committed(),
        "the service marks the enqueue committed"
    );

    assert!(audit_rows(&db.pool).await.is_empty(), "no post-hoc row yet");
    let pending = outbox_rows(&db.pool).await;
    assert_eq!(pending.len(), 1);

    // The startup dispatcher pass replays the durable row exactly once.
    // Dispatch at the row's actual (enqueue-stamped) due time.
    let rows = dispatch_one(&db, pending[0].created_at_ms).await;
    assert_eq!(rows[0].audit_id, "audit-crash-replay");
    assert_eq!(rows[0].action, "ReportBootstrapResult");
    assert_eq!(rows[0].operation_id.as_deref(), Some("op-crash"));

    // A second pass is a no-op: still exactly one evidence row.
    let again = dispatcher(&db.pool)
        .dispatch_pass(pending[0].created_at_ms + 60_000)
        .await
        .expect("second pass");
    assert_eq!(again, 0);
    assert_eq!(audit_rows(&db.pool).await.len(), 1);
}

// ---------------------------------------------------------------------------
// #706 PR 3c — node lifecycle transitions coupled to the transactional
// audit outbox.
//
// These drive the *real* `LifecycleServiceImplementation`, so the outbox
// row is enqueued inside the intent write's transaction exactly as in
// production. The coupling is intent-anchored: the privileged state change
// (`set_scheduling_paused`, `set_drain_intent`,
// `set_state_preserving_policy`) and its audit row share one commit. The
// operation row and `OperationStarted` event are earlier, separate commits
// and remain outside the coupled unit.
// ---------------------------------------------------------------------------

fn node_transition_meta(node_id: &str, operation_id: &str) -> proto::RequestMeta {
    proto::RequestMeta {
        operation_id: operation_id.into(),
        requested_by: "test-user".into(),
        target_node_id: node_id.into(),
        desired_state_version: "1".into(),
        request_unix_ms: 1_000,
    }
}

fn node_transition_request<T>(message: T, node_id: &str, operation_id: &str) -> Request<T> {
    let mut request = peer_request(message, node_id);
    request.metadata_mut().insert(
        chv_common::OPERATION_ID_METADATA_KEY,
        operation_id.parse().expect("metadata value"),
    );
    request
}

fn lifecycle_impl(db: &TestDb) -> crate::lifecycle::LifecycleServiceImplementation {
    crate::lifecycle::LifecycleServiceImplementation::new(
        NodeRepository::new(db.pool.clone()),
        OperationRepository::new(db.pool.clone()),
        EventRepository::new(db.pool.clone()),
        DesiredStateRepository::new(db.pool.clone()),
    )
}

fn adopted_lifecycle_server(db: &TestDb) -> LifecycleServer {
    LifecycleServer::new(Arc::new(lifecycle_impl(db)))
        .with_audit(AuditLogRepository::new(db.pool.clone()))
}

async fn seed_node_desired_state(
    pool: &StorePool,
    node_id: &str,
    state: &str,
    generation: i64,
    scheduling_paused: bool,
    state_reason: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO node_desired_state \
         (node_id, desired_generation, desired_state, state_reason, requested_at, updated_at, \
          scheduling_paused) \
         VALUES ($1, $2, $3, $4, strftime('%Y-%m-%dT%H:%M:%SZ','now'), \
                 strftime('%Y-%m-%dT%H:%M:%SZ','now'), $5)",
    )
    .bind(node_id)
    .bind(generation)
    .bind(state)
    .bind(state_reason)
    .bind(scheduling_paused)
    .execute(pool)
    .await
    .expect("seed node desired state");
}

#[tokio::test]
async fn server_audit_node_transitions_adopt_outbox_per_rpc() {
    #[derive(Clone, Copy, Debug)]
    enum Rpc {
        Pause,
        Resume,
        Drain,
        EnterMaintenance,
        ExitMaintenance,
    }

    impl Rpc {
        fn action(self) -> &'static str {
            match self {
                Rpc::Pause => "PauseNodeScheduling",
                Rpc::Resume => "ResumeNodeScheduling",
                Rpc::Drain => "DrainNode",
                Rpc::EnterMaintenance => "EnterMaintenance",
                Rpc::ExitMaintenance => "ExitMaintenance",
            }
        }

        /// The pre-state each arm must transition *from*. Seeding a single
        /// `TenantReady` row for every arm would make the applied assertions
        /// below read back the seed instead of a transition (the exit arm
        /// asserts `TenantReady`, which would then hold even if the exit
        /// patch were a no-op).
        fn seed(self) -> (&'static str, bool, Option<&'static str>) {
            match self {
                Rpc::Pause | Rpc::Resume | Rpc::Drain | Rpc::EnterMaintenance => {
                    ("TenantReady", matches!(self, Rpc::Resume), None)
                }
                Rpc::ExitMaintenance => ("Maintenance", false, Some("upgrade")),
            }
        }
    }

    /// Read back the applied intent. Asserting only the outbox row would
    /// keep passing if the store's intent statement were deleted: the
    /// enqueue still commits on its own.
    async fn assert_desired_state_applied(pool: &StorePool, rpc: Rpc) {
        let (state, reason, paused, allow_stop): (String, Option<String>, i64, Option<i64>) =
            sqlx::query_as(
                "SELECT desired_state, state_reason, scheduling_paused, allow_workload_stop \
                 FROM node_desired_state WHERE node_id = 'node-a'",
            )
            .fetch_one(pool)
            .await
            .expect("read applied desired state");

        match rpc {
            Rpc::Pause => assert_eq!(paused, 1, "{rpc:?}: pause must apply"),
            Rpc::Resume => assert_eq!(paused, 0, "{rpc:?}: resume must apply"),
            Rpc::Drain => {
                assert_eq!(state, "Draining", "{rpc:?}: drain must apply");
                assert_eq!(
                    allow_stop,
                    Some(1),
                    "{rpc:?}: drain must record allow_workload_stop"
                );
            }
            Rpc::EnterMaintenance => {
                assert_eq!(state, "Maintenance", "{rpc:?}: maintenance must apply");
                assert_eq!(
                    reason.as_deref(),
                    Some("upgrade"),
                    "{rpc:?}: maintenance reason must apply"
                );
            }
            Rpc::ExitMaintenance => {
                assert_eq!(state, "TenantReady", "{rpc:?}: exit must apply");
                assert_eq!(
                    reason, None,
                    "{rpc:?}: exit must clear the maintenance reason"
                );
            }
        }
    }

    for rpc in [
        Rpc::Pause,
        Rpc::Resume,
        Rpc::Drain,
        Rpc::EnterMaintenance,
        Rpc::ExitMaintenance,
    ] {
        let db = TestDb::new().await;
        seed_node(&db, "node-a", None).await;
        // Each iteration owns a fresh `TestDb`, so the per-arm pre-state
        // cannot leak into another arm and each applied assertion tests a
        // real transition.
        let (seed_state, seed_paused, seed_reason) = rpc.seed();
        seed_node_desired_state(&db.pool, "node-a", seed_state, 1, seed_paused, seed_reason).await;
        let server = adopted_lifecycle_server(&db);
        let op = format!("op-{rpc:?}");

        match rpc {
            Rpc::Pause => server
                .pause_node_scheduling(node_transition_request(
                    proto::PauseNodeSchedulingRequest {
                        meta: Some(node_transition_meta("node-a", &op)),
                        node_id: "node-a".into(),
                    },
                    "node-a",
                    &op,
                ))
                .await
                .expect("pause succeeds"),
            Rpc::Resume => server
                .resume_node_scheduling(node_transition_request(
                    proto::ResumeNodeSchedulingRequest {
                        meta: Some(node_transition_meta("node-a", &op)),
                        node_id: "node-a".into(),
                    },
                    "node-a",
                    &op,
                ))
                .await
                .expect("resume succeeds"),
            Rpc::Drain => server
                .drain_node(node_transition_request(
                    proto::DrainNodeRequest {
                        meta: Some(node_transition_meta("node-a", &op)),
                        node_id: "node-a".into(),
                        allow_workload_stop: true,
                    },
                    "node-a",
                    &op,
                ))
                .await
                .expect("drain succeeds"),
            Rpc::EnterMaintenance => server
                .enter_maintenance(node_transition_request(
                    proto::EnterMaintenanceRequest {
                        meta: Some(node_transition_meta("node-a", &op)),
                        node_id: "node-a".into(),
                        reason: "upgrade".into(),
                    },
                    "node-a",
                    &op,
                ))
                .await
                .expect("enter maintenance succeeds"),
            Rpc::ExitMaintenance => server
                .exit_maintenance(node_transition_request(
                    proto::ExitMaintenanceRequest {
                        meta: Some(node_transition_meta("node-a", &op)),
                        node_id: "node-a".into(),
                    },
                    "node-a",
                    &op,
                ))
                .await
                .expect("exit maintenance succeeds"),
        };

        assert_desired_state_applied(&db.pool, rpc).await;

        assert!(
            audit_rows(&db.pool).await.is_empty(),
            "{rpc:?}: adopted success skips the post-hoc direct insert"
        );
        let rows = outbox_rows(&db.pool).await;
        assert_eq!(rows.len(), 1, "{rpc:?}: exactly one outbox row");
        let row = &rows[0];
        assert_eq!(row.status, AUDIT_OUTBOX_PENDING);
        assert!(
            !row.audit_id.is_empty() && row.audit_id != row.outbox_id,
            "{rpc:?}: a separate audit_id was pre-minted"
        );
        let payload = row.parse_payload().expect("payload parses");
        let action = rpc.action();
        assert_eq!(payload.action, action);
        assert_eq!(
            payload.path,
            format!("chv.controlplane.node.v1.LifecycleService/{action}")
        );
        assert_eq!(payload.actor_id, "node-a");
        assert_eq!(payload.actor_role, "node");
        assert_eq!(payload.resource_kind.as_deref(), Some("node"));
        assert_eq!(payload.resource_id.as_deref(), Some("node-a"));
        assert_eq!(payload.operation_id.as_deref(), Some(op.as_str()));
        assert_eq!(payload.outcome, "success");
        assert_eq!(payload.status_code, 200);
        assert_metadata_is_allowlisted(&payload.metadata, "mtls");

        let dispatched = dispatch_one(&db, row.created_at_ms).await;
        assert_eq!(
            dispatched[0].audit_id, row.audit_id,
            "{rpc:?}: dispatch reuses the pre-minted audit_id"
        );
        assert_eq!(dispatched[0].action, action);

        let again = dispatcher(&db.pool)
            .dispatch_pass(row.created_at_ms + 60_000)
            .await
            .expect("second pass");
        assert_eq!(again, 0, "{rpc:?}: a dispatched row is never replayed");
        assert_eq!(
            audit_rows(&db.pool).await.len(),
            1,
            "{rpc:?}: no double write"
        );
    }
}

#[tokio::test]
async fn server_audit_node_transition_intent_failure_keeps_failed_journal_and_one_direct_row() {
    let db = TestDb::new().await;
    // No node_desired_state row: the pause UPDATE matches nothing and the
    // store returns NotFound, so the intent transaction (and the enqueue)
    // rolls back.
    seed_node(&db, "node-a", None).await;
    let server = adopted_lifecycle_server(&db);

    let status = server
        .pause_node_scheduling(peer_request(
            proto::PauseNodeSchedulingRequest {
                meta: Some(node_transition_meta("node-a", "op-pause-intent-failure")),
                node_id: "node-a".into(),
            },
            "node-a",
        ))
        .await
        .expect_err("a missing desired-state row must fail the pause");
    assert_eq!(status.code(), Code::NotFound);

    assert!(
        outbox_rows(&db.pool).await.is_empty(),
        "a rolled-back intent enqueues nothing"
    );
    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1, "exactly one direct failed row");
    assert_eq!(rows[0].action, "PauseNodeScheduling");
    assert_eq!(rows[0].outcome, "failed");
    assert_eq!(rows[0].status_code, 404);

    // The existing failure path is untouched: the operation is journaled
    // Failed and the OperationFailed event still exists.
    let (operation_status, error_code): (String, Option<String>) = sqlx::query_as(
        "SELECT status, error_code FROM operations WHERE operation_type = 'PauseNodeScheduling'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("operation row exists");
    assert_eq!(operation_status, "Failed");
    assert_eq!(error_code.as_deref(), Some("INTENT_PERSISTENCE_FAILED"));
    let (failed_events,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM events WHERE event_type = 'OperationFailed'")
            .fetch_one(&db.pool)
            .await
            .expect("count failed events");
    assert_eq!(failed_events, 1, "the Failed journal keeps its event");
}

#[tokio::test]
async fn server_audit_node_transition_crash_replay_writes_exactly_one_row() {
    let db = TestDb::new().await;
    seed_node(&db, "node-a", None).await;
    seed_node_desired_state(&db.pool, "node-a", "TenantReady", 1, false, None).await;
    let service = lifecycle_impl(&db);

    // Deterministic crash: commit the intent + outbox row, then never run
    // the adapter's `finish_audited` (the process died before it).
    let now = chv_common::now_unix_ms();
    let insert = AuditOutboxInsert {
        outbox_id: "ob-node-crash".into(),
        audit_id: "audit-node-crash".into(),
        payload: AuditLogInsert {
            occurred_at_ms: now,
            actor_id: "node-a".into(),
            actor_username: "node-a".into(),
            actor_role: "node".into(),
            action: "PauseNodeScheduling".into(),
            method: "gRPC".into(),
            path: "chv.controlplane.node.v1.LifecycleService/PauseNodeScheduling".into(),
            resource_kind: Some("node".into()),
            resource_id: Some("node-a".into()),
            outcome: "success".into(),
            status_code: 200,
            correlation_id: None,
            operation_id: Some("op-node-crash".into()),
            source_ip: None,
            metadata: r#"{"transport":"grpc","auth_source":"mtls"}"#.into(),
        },
        created_at_ms: now,
    };
    let committed = std::sync::atomic::AtomicBool::new(false);
    let coupling = AdoptedAudit::new(&insert, &committed);
    service
        .pause_node_scheduling(
            proto::PauseNodeSchedulingRequest {
                meta: Some(node_transition_meta("node-a", "op-node-crash")),
                node_id: "node-a".into(),
            },
            Some(&coupling),
        )
        .await
        .expect("intent commits with its outbox row");
    assert!(
        coupling.was_committed(),
        "the service marks the enqueue committed"
    );

    assert!(audit_rows(&db.pool).await.is_empty(), "no post-hoc row yet");
    let pending = outbox_rows(&db.pool).await;
    assert_eq!(pending.len(), 1);

    let rows = dispatch_one(&db, pending[0].created_at_ms).await;
    assert_eq!(rows[0].audit_id, "audit-node-crash");
    assert_eq!(rows[0].action, "PauseNodeScheduling");
    assert_eq!(rows[0].operation_id.as_deref(), Some("op-node-crash"));

    let again = dispatcher(&db.pool)
        .dispatch_pass(pending[0].created_at_ms + 60_000)
        .await
        .expect("second pass");
    assert_eq!(again, 0);
    assert_eq!(audit_rows(&db.pool).await.len(), 1);
}

#[tokio::test]
async fn server_audit_node_transition_post_commit_failure_patches_the_committed_row() {
    let db = TestDb::new().await;
    let server = LifecycleServer::new(Arc::new(MockLifecycle {
        enqueue_then_fail: Some(db.pool.clone()),
        ..Default::default()
    }))
    .with_audit(AuditLogRepository::new(db.pool.clone()));

    let status = server
        .pause_node_scheduling(peer_request(
            proto::PauseNodeSchedulingRequest {
                meta: Some(node_transition_meta("node-a", "op-node-post-commit")),
                node_id: "node-a".into(),
            },
            "node-a",
        ))
        .await
        .expect_err("the service failure must be preserved");
    assert_eq!(status.code(), Code::Internal);

    // The committed row is the only evidence: no direct row was added.
    assert!(
        audit_rows(&db.pool).await.is_empty(),
        "a present outbox row suppresses the direct insert"
    );
    let rows = outbox_rows(&db.pool).await;
    assert_eq!(rows.len(), 1, "exactly one outbox row");
    assert_eq!(rows[0].status, AUDIT_OUTBOX_PENDING);
    let payload = rows[0].parse_payload().expect("payload parses");
    assert_eq!(
        payload.outcome, "failed",
        "the commit-time success prediction is patched"
    );
    assert_eq!(payload.status_code, 500);
    assert_eq!(payload.action, "PauseNodeScheduling");

    let dispatched = dispatch_one(&db, rows[0].created_at_ms).await;
    assert_eq!(dispatched[0].audit_id, rows[0].audit_id);
    assert_eq!(dispatched[0].outcome, "failed");
    assert_eq!(dispatched[0].status_code, 500);
}

#[tokio::test]
async fn server_audit_node_transition_oversized_payload_rolls_back_the_intent() {
    let db = TestDb::new().await;
    seed_node(&db, "node-a", None).await;
    seed_node_desired_state(&db.pool, "node-a", "TenantReady", 1, false, None).await;
    let service = lifecycle_impl(&db);

    let now = chv_common::now_unix_ms();
    let oversized = AuditOutboxInsert {
        outbox_id: "ob-node-oversized".into(),
        audit_id: "audit-node-oversized".into(),
        payload: AuditLogInsert {
            occurred_at_ms: now,
            actor_id: "node-a".into(),
            actor_username: "node-a".into(),
            actor_role: "node".into(),
            action: "PauseNodeScheduling".into(),
            method: "gRPC".into(),
            path: "chv.controlplane.node.v1.LifecycleService/PauseNodeScheduling".into(),
            resource_kind: Some("node".into()),
            resource_id: Some("node-a".into()),
            outcome: "success".into(),
            status_code: 200,
            correlation_id: None,
            operation_id: None,
            source_ip: None,
            // > the 16 KiB payload bound enforced by `enqueue_on_tx`.
            metadata: "x".repeat(16 * 1024 + 1),
        },
        created_at_ms: now,
    };
    let committed = std::sync::atomic::AtomicBool::new(false);
    let coupling = AdoptedAudit::new(&oversized, &committed);
    let error = service
        .pause_node_scheduling(
            proto::PauseNodeSchedulingRequest {
                meta: Some(node_transition_meta("node-a", "op-node-oversized")),
                node_id: "node-a".into(),
            },
            Some(&coupling),
        )
        .await
        .expect_err("oversized evidence must fail the mutation closed");
    assert!(
        matches!(
            error,
            ControlPlaneServiceError::Store(StoreError::InvalidConfiguration { .. })
        ),
        "expected the store's payload validation error, got {error:?}"
    );

    let (paused,): (i64,) =
        sqlx::query_as("SELECT scheduling_paused FROM node_desired_state WHERE node_id = 'node-a'")
            .fetch_one(&db.pool)
            .await
            .expect("read desired state");
    assert_eq!(paused, 0, "the intent must roll back with the evidence");
    assert!(
        outbox_rows(&db.pool).await.is_empty(),
        "no outbox row survives the rollback"
    );
    assert!(!coupling.was_committed());
}

#[tokio::test]
async fn server_audit_unadopted_lifecycle_rpc_writes_one_direct_row_and_no_outbox() {
    let db = TestDb::new().await;
    let server = LifecycleServer::new(Arc::new(MockLifecycle::default()))
        .with_audit(AuditLogRepository::new(db.pool.clone()));

    let request = peer_request(
        proto::StartVmRequest {
            meta: Some(meta("op-unadopted")),
            node_id: "node-a".into(),
            vm_id: "vm-unadopted".into(),
        },
        "node-a",
    );
    server.start_vm(request).await.expect("start vm succeeds");

    assert!(
        outbox_rows(&db.pool).await.is_empty(),
        "unadopted RPCs never touch the outbox"
    );
    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1, "exactly one direct row, as before");
    assert_eq!(rows[0].action, "StartVm");
    assert_eq!(rows[0].outcome, "success");
    assert_eq!(rows[0].status_code, 200);
}

#[tokio::test]
async fn server_audit_post_commit_failure_patches_the_committed_row_exactly_once() {
    let db = TestDb::new().await;
    // The service enqueues + commits, then fails. This is not reachable
    // through the current implementations (enqueue is their last fallible
    // step); the test pins the composition's defensive contract.
    let server = EnrollmentServer::new(Arc::new(EnqueueThenFail {
        pool: db.pool.clone(),
    }))
    .with_audit(AuditLogRepository::new(db.pool.clone()));

    let request = peer_request(
        proto::ReportBootstrapResultRequest {
            meta: Some(meta("op-post-commit-failure")),
            node_id: "node-a".into(),
            bootstrap_status: "ok".into(),
            message: String::new(),
        },
        "node-a",
    );
    let status = server
        .report_bootstrap_result(request)
        .await
        .expect_err("the service failure must be preserved");
    assert_eq!(status.code(), Code::Internal);

    // The committed row is the only evidence: no direct row was added.
    assert!(
        audit_rows(&db.pool).await.is_empty(),
        "a present outbox row must suppress the direct insert"
    );
    let rows = outbox_rows(&db.pool).await;
    assert_eq!(rows.len(), 1, "exactly one outbox row");
    assert_eq!(rows[0].status, AUDIT_OUTBOX_PENDING);
    let payload = rows[0].parse_payload().expect("payload parses");
    assert_eq!(
        payload.outcome, "failed",
        "the commit-time success prediction is patched"
    );
    assert_eq!(payload.status_code, 500);
    assert_eq!(payload.action, "ReportBootstrapResult");

    // Dispatch replays the patched row once; no second row appears.
    let dispatched = dispatch_one(&db, rows[0].created_at_ms).await;
    assert_eq!(dispatched[0].audit_id, rows[0].audit_id);
    assert_eq!(dispatched[0].outcome, "failed");
    assert_eq!(dispatched[0].status_code, 500);
}

// ---------------------------------------------------------------------------
// #706 PR 4 — volume snapshot-family RPCs coupled to the audit outbox.
//
// These drive the *real* `LifecycleServiceImplementation` through the real
// `LifecycleServer` adapter, so the outbox row is enqueued inside the
// volume-desired-state write's transaction exactly as in production.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum SnapshotRpc {
    Snapshot,
    Restore,
    Delete,
}

impl SnapshotRpc {
    fn action(self) -> &'static str {
        match self {
            SnapshotRpc::Snapshot => "SnapshotVolume",
            SnapshotRpc::Restore => "RestoreVolume",
            SnapshotRpc::Delete => "DeleteVolumeSnapshot",
        }
    }

    /// The `snapshot_op` value each arm must write.
    fn op(self) -> &'static str {
        match self {
            SnapshotRpc::Snapshot => "create",
            SnapshotRpc::Restore => "restore",
            SnapshotRpc::Delete => "delete",
        }
    }
}

async fn seed_snapshot_volume(
    pool: &StorePool,
    volume_id: &str,
    node_id: &str,
    generation: i64,
    status: Option<&str>,
    snapshot_op: Option<&str>,
    snapshot_name: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO volumes (volume_id, node_id, display_name, capacity_bytes) \
         VALUES ($1, $2, $1, 1024)",
    )
    .bind(volume_id)
    .bind(node_id)
    .execute(pool)
    .await
    .expect("seed volume row");
    sqlx::query(
        "INSERT INTO volume_desired_state \
         (volume_id, desired_generation, desired_status, snapshot_op, snapshot_name) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(volume_id)
    .bind(generation)
    .bind(status)
    .bind(snapshot_op)
    .bind(snapshot_name)
    .execute(pool)
    .await
    .expect("seed volume desired state");
}

async fn snapshot_state(
    pool: &StorePool,
    volume_id: &str,
) -> (Option<String>, Option<String>, Option<String>, i64) {
    sqlx::query_as(
        "SELECT snapshot_op, snapshot_name, desired_status, desired_generation \
         FROM volume_desired_state WHERE volume_id = $1",
    )
    .bind(volume_id)
    .fetch_one(pool)
    .await
    .expect("read volume desired state")
}

async fn call_snapshot_rpc(
    server: &LifecycleServer,
    rpc: SnapshotRpc,
    operation_id: &str,
) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
    let operation = operation_id.to_string();
    let meta = Some(node_transition_meta("node-a", &operation));
    match rpc {
        SnapshotRpc::Snapshot => {
            server
                .snapshot_volume(node_transition_request(
                    proto::SnapshotVolumeRequest {
                        meta,
                        node_id: "node-a".into(),
                        volume_id: "vol-a".into(),
                        snapshot_name: "snap-1".into(),
                    },
                    "node-a",
                    &operation,
                ))
                .await
        }
        SnapshotRpc::Restore => {
            server
                .restore_volume(node_transition_request(
                    proto::RestoreVolumeRequest {
                        meta,
                        node_id: "node-a".into(),
                        volume_id: "vol-a".into(),
                        snapshot_name: "snap-1".into(),
                    },
                    "node-a",
                    &operation,
                ))
                .await
        }
        SnapshotRpc::Delete => {
            server
                .delete_volume_snapshot(node_transition_request(
                    proto::DeleteVolumeSnapshotRequest {
                        meta,
                        node_id: "node-a".into(),
                        volume_id: "vol-a".into(),
                        snapshot_name: "snap-1".into(),
                    },
                    "node-a",
                    &operation,
                ))
                .await
        }
    }
}

fn snapshot_audit_insert(
    outbox_id: &str,
    audit_id: &str,
    action: &str,
    created_at_ms: i64,
) -> AuditOutboxInsert {
    AuditOutboxInsert {
        outbox_id: outbox_id.into(),
        audit_id: audit_id.into(),
        payload: AuditLogInsert {
            occurred_at_ms: created_at_ms,
            actor_id: "node-a".into(),
            actor_username: "node-a".into(),
            actor_role: "node".into(),
            action: action.into(),
            method: "gRPC".into(),
            path: format!("chv.controlplane.node.v1.LifecycleService/{action}"),
            resource_kind: Some("volume".into()),
            resource_id: Some("vol-a".into()),
            outcome: "success".into(),
            status_code: 200,
            correlation_id: None,
            operation_id: Some("op-snapshot-crash".into()),
            source_ip: None,
            metadata: r#"{"transport":"grpc","auth_source":"mtls"}"#.into(),
        },
        created_at_ms,
    }
}

#[tokio::test]
async fn server_audit_volume_snapshot_family_adopts_outbox_per_rpc() {
    for rpc in [
        SnapshotRpc::Snapshot,
        SnapshotRpc::Restore,
        SnapshotRpc::Delete,
    ] {
        let db = TestDb::new().await;
        seed_node(&db, "node-a", None).await;
        // The seed names a *different* snapshot op/name so the applied
        // read-back below proves the transition instead of echoing the
        // seed (the PR 3c lesson).
        seed_snapshot_volume(
            &db.pool,
            "vol-a",
            "node-a",
            1,
            None,
            Some("seed"),
            Some("seed-name"),
        )
        .await;
        let server = adopted_lifecycle_server(&db);
        let op = format!("op-{rpc:?}");

        call_snapshot_rpc(&server, rpc, &op)
            .await
            .expect("the adopted snapshot-family RPC succeeds");

        let (snapshot_op, snapshot_name, status, _generation) =
            snapshot_state(&db.pool, "vol-a").await;
        assert_eq!(
            snapshot_op.as_deref(),
            Some(rpc.op()),
            "{rpc:?}: the intent must apply the RPC's snapshot op"
        );
        assert_eq!(
            snapshot_name.as_deref(),
            Some("snap-1"),
            "{rpc:?}: the intent must apply the request's snapshot name"
        );
        assert_eq!(status, None, "{rpc:?}: the desired status is untouched");

        assert!(
            audit_rows(&db.pool).await.is_empty(),
            "{rpc:?}: adopted success skips the post-hoc direct insert"
        );
        let rows = outbox_rows(&db.pool).await;
        assert_eq!(rows.len(), 1, "{rpc:?}: exactly one outbox row");
        let row = &rows[0];
        assert_eq!(row.status, AUDIT_OUTBOX_PENDING);
        assert!(
            !row.audit_id.is_empty() && row.audit_id != row.outbox_id,
            "{rpc:?}: a separate audit_id was pre-minted"
        );
        let payload = row.parse_payload().expect("payload parses");
        let action = rpc.action();
        assert_eq!(payload.action, action);
        assert_eq!(
            payload.path,
            format!("chv.controlplane.node.v1.LifecycleService/{action}")
        );
        assert_eq!(payload.actor_id, "node-a");
        assert_eq!(payload.actor_role, "node");
        assert_eq!(payload.resource_kind.as_deref(), Some("volume"));
        assert_eq!(payload.resource_id.as_deref(), Some("vol-a"));
        assert_eq!(payload.operation_id.as_deref(), Some(op.as_str()));
        assert_eq!(payload.outcome, "success");
        assert_eq!(payload.status_code, 200);
        assert_metadata_is_allowlisted(&payload.metadata, "mtls");

        let dispatched = dispatch_one(&db, row.created_at_ms).await;
        assert_eq!(
            dispatched[0].audit_id, row.audit_id,
            "{rpc:?}: dispatch reuses the pre-minted audit_id"
        );
        assert_eq!(dispatched[0].action, action);

        let again = dispatcher(&db.pool)
            .dispatch_pass(row.created_at_ms + 60_000)
            .await
            .expect("second pass");
        assert_eq!(again, 0, "{rpc:?}: a dispatched row is never replayed");
        assert_eq!(
            audit_rows(&db.pool).await.len(),
            1,
            "{rpc:?}: no double write"
        );
    }
}

#[tokio::test]
async fn server_audit_volume_snapshot_tombstone_writes_one_direct_row_and_no_outbox() {
    for rpc in [
        SnapshotRpc::Snapshot,
        SnapshotRpc::Restore,
        SnapshotRpc::Delete,
    ] {
        let db = TestDb::new().await;
        seed_node(&db, "node-a", None).await;
        // A `Deleting` tombstone blocks the patch at the SQL level; the
        // generation guard would allow the write, so this exercises the
        // tombstone arm (and its in-transaction check) specifically.
        seed_snapshot_volume(
            &db.pool,
            "vol-a",
            "node-a",
            1,
            Some("Deleting"),
            Some("seed"),
            Some("seed-name"),
        )
        .await;
        let server = adopted_lifecycle_server(&db);

        let status = call_snapshot_rpc(&server, rpc, "op-tombstone")
            .await
            .expect_err("a snapshot-family intent over a tombstone must fail");
        assert_eq!(
            status.code(),
            Code::AlreadyExists,
            "{rpc:?}: the tombstone refusal is the loud Conflict"
        );

        assert!(
            outbox_rows(&db.pool).await.is_empty(),
            "{rpc:?}: the rolled-back intent enqueues nothing"
        );
        let rows = audit_rows(&db.pool).await;
        assert_eq!(rows.len(), 1, "{rpc:?}: exactly one direct failed row");
        assert_eq!(rows[0].action, rpc.action());
        assert_eq!(rows[0].outcome, "failed");
        assert_eq!(rows[0].status_code, 409);
        assert_eq!(rows[0].resource_kind.as_deref(), Some("volume"));
        assert_eq!(rows[0].resource_id.as_deref(), Some("vol-a"));

        let state = snapshot_state(&db.pool, "vol-a").await;
        assert_eq!(
            state,
            (
                Some("seed".into()),
                Some("seed-name".into()),
                Some("Deleting".into()),
                1
            ),
            "{rpc:?}: the tombstone must survive the refused patch byte-for-byte"
        );

        // The existing failure machinery still journals the intent failure.
        let (operation_status, error_code): (String, Option<String>) =
            sqlx::query_as("SELECT status, error_code FROM operations WHERE operation_type = $1")
                .bind(rpc.action())
                .fetch_one(&db.pool)
                .await
                .expect("operation row exists");
        assert_eq!(operation_status, "Failed");
        assert_eq!(error_code.as_deref(), Some("INTENT_PERSISTENCE_FAILED"));
    }
}

#[tokio::test]
async fn server_audit_volume_snapshot_crash_replay_writes_exactly_one_row() {
    for rpc in [
        SnapshotRpc::Snapshot,
        SnapshotRpc::Restore,
        SnapshotRpc::Delete,
    ] {
        let db = TestDb::new().await;
        seed_node(&db, "node-a", None).await;
        seed_snapshot_volume(
            &db.pool,
            "vol-a",
            "node-a",
            1,
            None,
            Some("seed"),
            Some("seed-name"),
        )
        .await;
        let service = lifecycle_impl(&db);

        // Deterministic crash: commit the intent + outbox row, then never
        // run the adapter's `finish_audited` (the process died before it).
        let now = chv_common::now_unix_ms();
        let outbox_id = format!("ob-snapshot-crash-{rpc:?}");
        let audit_id = format!("audit-snapshot-crash-{rpc:?}");
        let insert = snapshot_audit_insert(&outbox_id, &audit_id, rpc.action(), now);
        let committed = std::sync::atomic::AtomicBool::new(false);
        let coupling = AdoptedAudit::new(&insert, &committed);
        let meta = Some(node_transition_meta("node-a", "op-snapshot-crash"));

        match rpc {
            SnapshotRpc::Snapshot => service
                .snapshot_volume(
                    proto::SnapshotVolumeRequest {
                        meta,
                        node_id: "node-a".into(),
                        volume_id: "vol-a".into(),
                        snapshot_name: "snap-1".into(),
                    },
                    Some(&coupling),
                )
                .await
                .expect("intent commits with its outbox row"),
            SnapshotRpc::Restore => service
                .restore_volume(
                    proto::RestoreVolumeRequest {
                        meta,
                        node_id: "node-a".into(),
                        volume_id: "vol-a".into(),
                        snapshot_name: "snap-1".into(),
                    },
                    Some(&coupling),
                )
                .await
                .expect("intent commits with its outbox row"),
            SnapshotRpc::Delete => service
                .delete_volume_snapshot(
                    proto::DeleteVolumeSnapshotRequest {
                        meta,
                        node_id: "node-a".into(),
                        volume_id: "vol-a".into(),
                        snapshot_name: "snap-1".into(),
                    },
                    Some(&coupling),
                )
                .await
                .expect("intent commits with its outbox row"),
        };
        assert!(
            coupling.was_committed(),
            "{rpc:?}: the service marks the enqueue committed"
        );
        let (snapshot_op, snapshot_name, _status, _generation) =
            snapshot_state(&db.pool, "vol-a").await;
        assert_eq!(snapshot_op.as_deref(), Some(rpc.op()));
        assert_eq!(
            snapshot_name.as_deref(),
            Some("snap-1"),
            "{rpc:?}: the patch persists the request's snapshot_name"
        );

        assert!(audit_rows(&db.pool).await.is_empty(), "no post-hoc row yet");
        let pending = outbox_rows(&db.pool).await;
        assert_eq!(pending.len(), 1);

        let rows = dispatch_one(&db, pending[0].created_at_ms).await;
        assert_eq!(rows[0].audit_id, audit_id);
        assert_eq!(rows[0].action, rpc.action());
        assert_eq!(rows[0].operation_id.as_deref(), Some("op-snapshot-crash"));

        let again = dispatcher(&db.pool)
            .dispatch_pass(pending[0].created_at_ms + 60_000)
            .await
            .expect("second pass");
        assert_eq!(again, 0);
        assert_eq!(audit_rows(&db.pool).await.len(), 1);
    }
}

#[tokio::test]
async fn server_audit_volume_snapshot_post_commit_failure_patches_the_committed_row() {
    let db = TestDb::new().await;
    // Only `SnapshotVolume` is exercised: all three snapshot-family RPCs
    // reach the same `finish_audited` patch path (PR 3c), so one arm
    // covers the behaviour.
    // The mock commits the outbox row and then fails. The real wire macro
    // must patch that row instead of inserting a second one.
    let server = LifecycleServer::new(Arc::new(MockLifecycle {
        enqueue_then_fail: Some(db.pool.clone()),
        ..Default::default()
    }))
    .with_audit(AuditLogRepository::new(db.pool.clone()));

    let status = server
        .snapshot_volume(node_transition_request(
            proto::SnapshotVolumeRequest {
                meta: Some(node_transition_meta("node-a", "op-snapshot-post-commit")),
                node_id: "node-a".into(),
                volume_id: "vol-a".into(),
                snapshot_name: "snap-1".into(),
            },
            "node-a",
            "op-snapshot-post-commit",
        ))
        .await
        .expect_err("the service failure must be preserved");
    assert_eq!(status.code(), Code::Internal);

    assert!(
        audit_rows(&db.pool).await.is_empty(),
        "a present outbox row suppresses the direct insert"
    );
    let rows = outbox_rows(&db.pool).await;
    assert_eq!(rows.len(), 1, "exactly one outbox row");
    assert_eq!(rows[0].status, AUDIT_OUTBOX_PENDING);
    let payload = rows[0].parse_payload().expect("payload parses");
    assert_eq!(
        payload.outcome, "failed",
        "the commit-time success prediction is patched"
    );
    assert_eq!(payload.status_code, 500);
    assert_eq!(payload.action, "SnapshotVolume");
    assert_eq!(payload.resource_kind.as_deref(), Some("volume"));
    assert_eq!(payload.resource_id.as_deref(), Some("vol-a"));

    let dispatched = dispatch_one(&db, rows[0].created_at_ms).await;
    assert_eq!(dispatched[0].audit_id, rows[0].audit_id);
    assert_eq!(dispatched[0].outcome, "failed");
    assert_eq!(dispatched[0].status_code, 500);
}

#[tokio::test]
async fn server_audit_volume_snapshot_oversized_payload_rolls_back_the_intent() {
    let db = TestDb::new().await;
    // Only `SnapshotVolume` is exercised: all three snapshot-family RPCs
    // enqueue through the same `enqueue_on_tx::validate` bound, so one arm
    // covers the behaviour.
    seed_node(&db, "node-a", None).await;
    seed_snapshot_volume(
        &db.pool,
        "vol-a",
        "node-a",
        1,
        None,
        Some("seed"),
        Some("seed-name"),
    )
    .await;
    let service = lifecycle_impl(&db);

    let now = chv_common::now_unix_ms();
    let mut oversized = snapshot_audit_insert(
        "ob-snapshot-oversized",
        "audit-snapshot-oversized",
        "SnapshotVolume",
        now,
    );
    // > the 16 KiB payload bound enforced by `enqueue_on_tx`.
    oversized.payload.metadata = "x".repeat(16 * 1024 + 1);
    let committed = std::sync::atomic::AtomicBool::new(false);
    let coupling = AdoptedAudit::new(&oversized, &committed);

    let error = service
        .snapshot_volume(
            proto::SnapshotVolumeRequest {
                meta: Some(node_transition_meta("node-a", "op-snapshot-oversized")),
                node_id: "node-a".into(),
                volume_id: "vol-a".into(),
                snapshot_name: "snap-1".into(),
            },
            Some(&coupling),
        )
        .await
        .expect_err("oversized evidence must fail the mutation closed");
    assert!(
        matches!(
            error,
            ControlPlaneServiceError::Store(StoreError::InvalidConfiguration { .. })
        ),
        "expected the store's payload validation error, got {error:?}"
    );

    let state = snapshot_state(&db.pool, "vol-a").await;
    assert_eq!(
        state,
        (Some("seed".into()), Some("seed-name".into()), None, 1),
        "the intent must roll back with the evidence"
    );
    assert!(
        outbox_rows(&db.pool).await.is_empty(),
        "no outbox row survives the rollback"
    );
    assert!(!coupling.was_committed());
}

#[tokio::test]
async fn server_audit_unadopted_clone_volume_writes_one_direct_row_and_no_outbox() {
    let db = TestDb::new().await;
    let server = LifecycleServer::new(Arc::new(MockLifecycle::default()))
        .with_audit(AuditLogRepository::new(db.pool.clone()));

    let request = peer_request(
        proto::CloneVolumeRequest {
            meta: Some(meta("op-clone-unadopted")),
            node_id: "node-a".into(),
            source_volume_id: "vol-a".into(),
            target_volume_id: "vol-b".into(),
        },
        "node-a",
    );
    // `CloneVolume` stays post-hoc: the mock returns Unsupported, and the
    // adapter records that one direct row without touching the outbox.
    server
        .clone_volume(request)
        .await
        .expect_err("the mock clone is unimplemented");

    assert!(
        outbox_rows(&db.pool).await.is_empty(),
        "unadopted RPCs never touch the outbox"
    );
    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1, "exactly one direct row, as before");
    assert_eq!(rows[0].action, "CloneVolume");
    assert_eq!(rows[0].outcome, "failed");
    assert_eq!(rows[0].status_code, 501);
    assert_eq!(rows[0].resource_kind.as_deref(), Some("volume"));
    assert_eq!(rows[0].resource_id.as_deref(), Some("vol-a"));
}

// ---------------------------------------------------------------------------
// #706 PR 3b — reconcile RPCs coupled to the transactional audit outbox.
//
// These drive the *real* `ReconcileServiceImplementation`, so the outbox
// row is enqueued inside the desired-state write's transaction exactly as
// in production. `MockReconcile` above still covers the unadopted shape.
// ---------------------------------------------------------------------------

fn reconcile_impl(db: &TestDb) -> ReconcileServiceImplementation {
    ReconcileServiceImplementation::new(
        NodeRepository::new(db.pool.clone()),
        DesiredStateRepository::new(db.pool.clone()),
        EventRepository::new(db.pool.clone()),
        ObservedStateRepository::new(db.pool.clone()),
        OperationRepository::new(db.pool.clone()),
    )
}

fn adopted_reconcile_server(db: &TestDb) -> ReconcileServer {
    ReconcileServer::new(Arc::new(reconcile_impl(db)))
        .with_audit(AuditLogRepository::new(db.pool.clone()))
}

fn reconcile_meta(node_id: &str, operation_id: &str) -> proto::RequestMeta {
    proto::RequestMeta {
        operation_id: operation_id.into(),
        requested_by: "test-user".into(),
        target_node_id: node_id.into(),
        desired_state_version: "1".into(),
        request_unix_ms: 1_000,
    }
}

fn desired_state_fragment(
    resource_id: &str,
    kind: &str,
    generation: &str,
    spec_json: &str,
) -> proto::DesiredStateFragment {
    proto::DesiredStateFragment {
        id: resource_id.into(),
        kind: kind.into(),
        generation: generation.into(),
        spec_json: spec_json.as_bytes().to_vec(),
        policy_json: vec![],
        updated_at: String::new(),
        updated_by: String::new(),
    }
}

async fn seed_operation(
    pool: &StorePool,
    operation_id: &str,
    resource_kind: &str,
    resource_id: &str,
) {
    sqlx::query(
        "INSERT INTO operations \
         (operation_id, idempotency_key, resource_kind, resource_id, operation_type, status, requested_at) \
         VALUES (?, ?, ?, ?, 'Test', 'Pending', strftime('%Y-%m-%dT%H:%M:%SZ','now'))",
    )
    .bind(operation_id)
    .bind(format!("idem-{operation_id}"))
    .bind(resource_kind)
    .bind(resource_id)
    .execute(pool)
    .await
    .expect("seed operation");
}

async fn assert_single_dispatch(db: &TestDb, action: &str) -> AuditLogRow {
    let rows = outbox_rows(&db.pool).await;
    assert_eq!(rows.len(), 1, "exactly one outbox row");
    let row = &rows[0];
    assert_eq!(row.status, AUDIT_OUTBOX_PENDING);
    let dispatched = dispatch_one(db, row.created_at_ms).await;
    assert_eq!(dispatched[0].audit_id, row.audit_id, "audit_id is reused");
    assert_eq!(dispatched[0].action, action);
    assert_eq!(dispatched[0].outcome, "success");
    assert_eq!(dispatched[0].status_code, 200);

    // Replay is a no-op: the full path wrote exactly one evidence row.
    let again = dispatcher(&db.pool)
        .dispatch_pass(row.created_at_ms + 60_000)
        .await
        .expect("second pass");
    assert_eq!(again, 0, "a dispatched row is never replayed");
    assert_eq!(audit_rows(&db.pool).await.len(), 1, "no double write");
    dispatched[0].clone()
}

#[tokio::test]
async fn server_audit_reconcile_apply_node_desired_state_adopts_outbox() {
    let db = TestDb::new().await;
    seed_node(&db, "node-a", None).await;
    seed_operation(&db.pool, "op-node-adopted", "node", "node-a").await;
    let server = adopted_reconcile_server(&db);

    let mut request = peer_request(
        proto::ApplyNodeDesiredStateRequest {
            meta: Some(reconcile_meta("node-a", "op-node-adopted")),
            node_id: "node-a".into(),
            fragment: Some(desired_state_fragment(
                "node-a",
                "node",
                "1",
                r#"{"desired_state":"TenantReady"}"#,
            )),
        },
        "node-a",
    );
    request.metadata_mut().insert(
        chv_common::OPERATION_ID_METADATA_KEY,
        "op-node-adopted".parse().expect("metadata value"),
    );
    server
        .apply_node_desired_state(request)
        .await
        .expect("apply node desired state succeeds");

    // The desired-state write committed and the adapter wrote no direct row.
    let (state,): (String,) =
        sqlx::query_as("SELECT desired_state FROM node_desired_state WHERE node_id = 'node-a'")
            .fetch_one(&db.pool)
            .await
            .expect("read node desired state");
    assert_eq!(state, "TenantReady");
    assert!(audit_rows(&db.pool).await.is_empty(), "no direct insert");

    let rows = outbox_rows(&db.pool).await;
    assert_eq!(rows.len(), 1);
    let payload = rows[0].parse_payload().expect("payload parses");
    assert_eq!(payload.action, "ApplyNodeDesiredState");
    assert_eq!(
        payload.path,
        "chv.controlplane.node.v1.ReconcileService/ApplyNodeDesiredState"
    );
    assert_eq!(payload.actor_id, "node-a");
    assert_eq!(payload.actor_role, "node");
    assert_eq!(payload.resource_kind.as_deref(), Some("node"));
    assert_eq!(payload.resource_id.as_deref(), Some("node-a"));
    assert_eq!(payload.operation_id.as_deref(), Some("op-node-adopted"));
    assert_metadata_is_allowlisted(&payload.metadata, "mtls");

    assert_single_dispatch(&db, "ApplyNodeDesiredState").await;
}

#[tokio::test]
async fn server_audit_reconcile_apply_vm_desired_state_adopts_outbox() {
    let db = TestDb::new().await;
    seed_node(&db, "node-a", None).await;
    seed_operation(&db.pool, "op-vm-adopted", "vm", "vm-1").await;
    let server = adopted_reconcile_server(&db);

    let request = peer_request(
        proto::ApplyVmDesiredStateRequest {
            meta: Some(reconcile_meta("node-a", "op-vm-adopted")),
            node_id: "node-a".into(),
            vm_id: "vm-1".into(),
            fragment: Some(desired_state_fragment(
                "vm-1",
                "vm",
                "2",
                r#"{"cpu_count":2,"memory_bytes":4294967296,"image_ref":"ubuntu"}"#,
            )),
        },
        "node-a",
    );
    server
        .apply_vm_desired_state(request)
        .await
        .expect("apply vm desired state succeeds");

    let (generation,): (i64,) =
        sqlx::query_as("SELECT desired_generation FROM vm_desired_state WHERE vm_id = 'vm-1'")
            .fetch_one(&db.pool)
            .await
            .expect("read vm desired state");
    assert_eq!(generation, 2);
    assert!(audit_rows(&db.pool).await.is_empty(), "no direct insert");

    let rows = outbox_rows(&db.pool).await;
    assert_eq!(rows.len(), 1);
    let payload = rows[0].parse_payload().expect("payload parses");
    assert_eq!(payload.action, "ApplyVmDesiredState");
    assert_eq!(payload.resource_kind.as_deref(), Some("vm"));
    assert_eq!(payload.resource_id.as_deref(), Some("vm-1"));
    assert_eq!(payload.outcome, "success");

    assert_single_dispatch(&db, "ApplyVmDesiredState").await;
}

#[tokio::test]
async fn server_audit_reconcile_apply_volume_desired_state_adopts_outbox() {
    let db = TestDb::new().await;
    seed_node(&db, "node-a", None).await;
    seed_operation(&db.pool, "op-vol-adopted", "volume", "vol-1").await;
    let server = adopted_reconcile_server(&db);

    let request = peer_request(
        proto::ApplyVolumeDesiredStateRequest {
            meta: Some(reconcile_meta("node-a", "op-vol-adopted")),
            node_id: "node-a".into(),
            volume_id: "vol-1".into(),
            fragment: Some(desired_state_fragment(
                "vol-1",
                "volume",
                "3",
                r#"{"capacity_bytes":1073741824,"volume_kind":"disk"}"#,
            )),
        },
        "node-a",
    );
    server
        .apply_volume_desired_state(request)
        .await
        .expect("apply volume desired state succeeds");

    let (generation,): (i64,) = sqlx::query_as(
        "SELECT desired_generation FROM volume_desired_state WHERE volume_id = 'vol-1'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("read volume desired state");
    assert_eq!(generation, 3);
    assert!(audit_rows(&db.pool).await.is_empty(), "no direct insert");

    let rows = outbox_rows(&db.pool).await;
    assert_eq!(rows.len(), 1);
    let payload = rows[0].parse_payload().expect("payload parses");
    assert_eq!(payload.action, "ApplyVolumeDesiredState");
    assert_eq!(payload.resource_kind.as_deref(), Some("volume"));
    assert_eq!(payload.resource_id.as_deref(), Some("vol-1"));

    assert_single_dispatch(&db, "ApplyVolumeDesiredState").await;
}

#[tokio::test]
async fn server_audit_reconcile_apply_network_desired_state_adopts_outbox() {
    let db = TestDb::new().await;
    seed_node(&db, "node-a", None).await;
    seed_operation(&db.pool, "op-net-adopted", "network", "net-1").await;
    let server = adopted_reconcile_server(&db);

    let request = peer_request(
        proto::ApplyNetworkDesiredStateRequest {
            meta: Some(reconcile_meta("node-a", "op-net-adopted")),
            node_id: "node-a".into(),
            network_id: "net-1".into(),
            fragment: Some(desired_state_fragment(
                "net-1",
                "network",
                "4",
                r#"{"network_class":"overlay"}"#,
            )),
        },
        "node-a",
    );
    server
        .apply_network_desired_state(request)
        .await
        .expect("apply network desired state succeeds");

    let (generation,): (i64,) = sqlx::query_as(
        "SELECT desired_generation FROM network_desired_state WHERE network_id = 'net-1'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("read network desired state");
    assert_eq!(generation, 4);
    assert!(audit_rows(&db.pool).await.is_empty(), "no direct insert");

    let rows = outbox_rows(&db.pool).await;
    assert_eq!(rows.len(), 1);
    let payload = rows[0].parse_payload().expect("payload parses");
    assert_eq!(payload.action, "ApplyNetworkDesiredState");
    assert_eq!(payload.resource_kind.as_deref(), Some("network"));
    assert_eq!(payload.resource_id.as_deref(), Some("net-1"));

    assert_single_dispatch(&db, "ApplyNetworkDesiredState").await;
}

#[tokio::test]
async fn server_audit_reconcile_acknowledge_adopts_outbox() {
    let db = TestDb::new().await;
    seed_node(&db, "node-a", None).await;
    sqlx::query(
        "INSERT INTO node_observed_state (node_id, observed_generation, observed_state) \
         VALUES ('node-a', 1, 'TenantReady')",
    )
    .execute(&db.pool)
    .await
    .expect("seed observed state");
    seed_operation(&db.pool, "op-ack-adopted", "node", "node-a").await;
    let server = adopted_reconcile_server(&db);

    let request = peer_request(
        proto::AcknowledgeDesiredStateVersionRequest {
            meta: Some(reconcile_meta("node-a", "op-ack-adopted")),
            node_id: "node-a".into(),
            fragment_kind: "node".into(),
            fragment_id: "node-a".into(),
            observed_generation: "5".into(),
            apply_status: "ok".into(),
        },
        "node-a",
    );
    server
        .acknowledge_desired_state_version(request)
        .await
        .expect("acknowledge succeeds");

    let (generation,): (i64,) = sqlx::query_as(
        "SELECT observed_generation FROM node_observed_state WHERE node_id = 'node-a'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("read observed state");
    assert_eq!(generation, 5);
    assert!(audit_rows(&db.pool).await.is_empty(), "no direct insert");

    let rows = outbox_rows(&db.pool).await;
    assert_eq!(rows.len(), 1);
    let payload = rows[0].parse_payload().expect("payload parses");
    assert_eq!(payload.action, "AcknowledgeDesiredStateVersion");
    assert_eq!(
        payload.path,
        "chv.controlplane.node.v1.ReconcileService/AcknowledgeDesiredStateVersion"
    );
    assert_eq!(payload.resource_kind.as_deref(), Some("node"));
    assert_eq!(payload.resource_id.as_deref(), Some("node-a"));

    assert_single_dispatch(&db, "AcknowledgeDesiredStateVersion").await;
}

#[tokio::test]
async fn server_audit_reconcile_stale_generation_rolls_back_and_writes_one_failed_row() {
    let db = TestDb::new().await;
    seed_node(&db, "node-a", None).await;
    sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES ('vm-stale', 'vm-stale')")
        .execute(&db.pool)
        .await
        .expect("seed vm");
    sqlx::query(
        "INSERT INTO vm_desired_state (vm_id, desired_generation, requested_at, updated_at) \
         VALUES ('vm-stale', 10, strftime('%Y-%m-%dT%H:%M:%SZ','now'), \
                 strftime('%Y-%m-%dT%H:%M:%SZ','now'))",
    )
    .execute(&db.pool)
    .await
    .expect("seed desired state");
    let server = adopted_reconcile_server(&db);

    let request = peer_request(
        proto::ApplyVmDesiredStateRequest {
            meta: Some(reconcile_meta("node-a", "")),
            node_id: "node-a".into(),
            vm_id: "vm-stale".into(),
            fragment: Some(desired_state_fragment(
                "vm-stale",
                "vm",
                "1",
                r#"{"cpu_count":2}"#,
            )),
        },
        "node-a",
    );
    let status = server
        .apply_vm_desired_state(request)
        .await
        .expect_err("a stale fragment must be refused");
    assert_eq!(status.code(), Code::Internal);

    // The stale-generation error rolled the whole transaction back: no
    // desired-state write, no outbox row. The adapter records the failed
    // request exactly once on the direct path.
    assert!(
        outbox_rows(&db.pool).await.is_empty(),
        "a rolled-back mutation enqueues nothing"
    );
    let (generation,): (i64,) =
        sqlx::query_as("SELECT desired_generation FROM vm_desired_state WHERE vm_id = 'vm-stale'")
            .fetch_one(&db.pool)
            .await
            .expect("read desired state");
    assert_eq!(generation, 10, "the stale write must not commit");
    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1, "exactly one direct failed row");
    assert_eq!(rows[0].action, "ApplyVmDesiredState");
    assert_eq!(rows[0].outcome, "failed");
    assert_eq!(rows[0].status_code, 500);
    assert_eq!(rows[0].resource_id.as_deref(), Some("vm-stale"));
}

#[tokio::test]
async fn server_audit_reconcile_acknowledge_missing_observed_row_writes_one_failed_row() {
    let db = TestDb::new().await;
    seed_node(&db, "node-a", None).await;
    let server = adopted_reconcile_server(&db);

    let request = peer_request(
        proto::AcknowledgeDesiredStateVersionRequest {
            meta: Some(reconcile_meta("node-a", "")),
            node_id: "node-a".into(),
            fragment_kind: "node".into(),
            fragment_id: "node-a".into(),
            observed_generation: "5".into(),
            apply_status: "ok".into(),
        },
        "node-a",
    );
    let status = server
        .acknowledge_desired_state_version(request)
        .await
        .expect_err("no observed-state row means NotFound");
    assert_eq!(status.code(), Code::NotFound);

    assert!(
        outbox_rows(&db.pool).await.is_empty(),
        "a rejected acknowledgement enqueues nothing"
    );
    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1, "exactly one direct failed row");
    assert_eq!(rows[0].action, "AcknowledgeDesiredStateVersion");
    assert_eq!(rows[0].outcome, "failed");
    assert_eq!(rows[0].status_code, 404);
}

#[tokio::test]
async fn server_audit_reconcile_adopted_crash_replay_writes_exactly_one_row() {
    let db = TestDb::new().await;
    seed_node(&db, "node-a", None).await;
    let service = reconcile_impl(&db);

    // Deterministic crash: commit the desired-state write + outbox row,
    // then never run the adapter's `finish_audited` (the process died).
    let now = chv_common::now_unix_ms();
    let insert = AuditOutboxInsert {
        outbox_id: "ob-reconcile-crash".into(),
        audit_id: "audit-reconcile-crash".into(),
        payload: AuditLogInsert {
            occurred_at_ms: now,
            actor_id: "node-a".into(),
            actor_username: "node-a".into(),
            actor_role: "node".into(),
            action: "ApplyVolumeDesiredState".into(),
            method: "gRPC".into(),
            path: "chv.controlplane.node.v1.ReconcileService/ApplyVolumeDesiredState".into(),
            resource_kind: Some("volume".into()),
            resource_id: Some("vol-crash".into()),
            outcome: "success".into(),
            status_code: 200,
            correlation_id: None,
            operation_id: Some("op-reconcile-crash".into()),
            source_ip: None,
            metadata: r#"{"transport":"grpc","auth_source":"mtls"}"#.into(),
        },
        created_at_ms: now,
    };
    let committed = std::sync::atomic::AtomicBool::new(false);
    let coupling = AdoptedAudit::new(&insert, &committed);
    service
        .apply_volume_desired_state(
            proto::ApplyVolumeDesiredStateRequest {
                meta: Some(reconcile_meta("node-a", "")),
                node_id: "node-a".into(),
                volume_id: "vol-crash".into(),
                fragment: Some(desired_state_fragment(
                    "vol-crash",
                    "volume",
                    "1",
                    r#"{"capacity_bytes":1024}"#,
                )),
            },
            Some(&coupling),
        )
        .await
        .expect("mutation commits with its outbox row");
    assert!(
        coupling.was_committed(),
        "the service marks the enqueue committed"
    );

    assert!(audit_rows(&db.pool).await.is_empty(), "no post-hoc row yet");
    let pending = outbox_rows(&db.pool).await;
    assert_eq!(pending.len(), 1);

    // The startup dispatcher pass replays the durable row exactly once.
    let rows = dispatch_one(&db, pending[0].created_at_ms).await;
    assert_eq!(rows[0].audit_id, "audit-reconcile-crash");
    assert_eq!(rows[0].action, "ApplyVolumeDesiredState");
    assert_eq!(rows[0].operation_id.as_deref(), Some("op-reconcile-crash"));

    let again = dispatcher(&db.pool)
        .dispatch_pass(pending[0].created_at_ms + 60_000)
        .await
        .expect("second pass");
    assert_eq!(again, 0);
    assert_eq!(audit_rows(&db.pool).await.len(), 1);
}

/// Reconcile service that enqueues + commits and then fails anyway. Not
/// reachable through the real implementation (the enqueue is its last
/// fallible step before the convergence event, which is a *later*
/// commit); the fixture pins the composition's patch contract.
struct ReconcileEnqueueThenFail {
    pool: StorePool,
}

#[tonic::async_trait]
impl ReconcileService for ReconcileEnqueueThenFail {
    async fn apply_node_desired_state(
        &self,
        _request: proto::ApplyNodeDesiredStateRequest,
        _audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        unreachable!("only apply_vm_desired_state is exercised")
    }

    async fn apply_vm_desired_state(
        &self,
        _request: proto::ApplyVmDesiredStateRequest,
        audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let audit = audit.expect("the adapter passes the coupling");
        let mut tx = self.pool.begin().await.expect("begin adopted tx");
        chv_controlplane_store::audit_outbox::enqueue_on_tx(&mut tx, audit.insert())
            .await
            .expect("enqueue evidence");
        tx.commit().await.expect("commit evidence");
        audit.mark_committed();
        Err(ControlPlaneServiceError::Internal(
            "post-commit failure".into(),
        ))
    }

    async fn apply_volume_desired_state(
        &self,
        _request: proto::ApplyVolumeDesiredStateRequest,
        _audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        unreachable!("only apply_vm_desired_state is exercised")
    }

    async fn apply_network_desired_state(
        &self,
        _request: proto::ApplyNetworkDesiredStateRequest,
        _audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        unreachable!("only apply_vm_desired_state is exercised")
    }

    async fn acknowledge_desired_state_version(
        &self,
        _request: proto::AcknowledgeDesiredStateVersionRequest,
        _audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        unreachable!("only apply_vm_desired_state is exercised")
    }
}

#[tokio::test]
async fn server_audit_reconcile_post_commit_failure_patches_the_committed_row() {
    let db = TestDb::new().await;
    let server = ReconcileServer::new(Arc::new(ReconcileEnqueueThenFail {
        pool: db.pool.clone(),
    }))
    .with_audit(AuditLogRepository::new(db.pool.clone()));

    let request = peer_request(
        proto::ApplyVmDesiredStateRequest {
            meta: Some(reconcile_meta("node-a", "")),
            node_id: "node-a".into(),
            vm_id: "vm-post-commit".into(),
            fragment: Some(desired_state_fragment("vm-post-commit", "vm", "1", "{}")),
        },
        "node-a",
    );
    let status = server
        .apply_vm_desired_state(request)
        .await
        .expect_err("the service failure must be preserved");
    assert_eq!(status.code(), Code::Internal);

    // The committed row is the only evidence: no direct row was added.
    assert!(
        audit_rows(&db.pool).await.is_empty(),
        "a present outbox row suppresses the direct insert"
    );
    let rows = outbox_rows(&db.pool).await;
    assert_eq!(rows.len(), 1, "exactly one outbox row");
    assert_eq!(rows[0].status, AUDIT_OUTBOX_PENDING);
    let payload = rows[0].parse_payload().expect("payload parses");
    assert_eq!(
        payload.outcome, "failed",
        "the commit-time success prediction is patched"
    );
    assert_eq!(payload.status_code, 500);
    assert_eq!(payload.action, "ApplyVmDesiredState");

    let dispatched = dispatch_one(&db, rows[0].created_at_ms).await;
    assert_eq!(dispatched[0].audit_id, rows[0].audit_id);
    assert_eq!(dispatched[0].outcome, "failed");
    assert_eq!(dispatched[0].status_code, 500);
}

/// An adopted reconcile write whose audit payload exceeds the store's
/// 16 KiB bound must fail closed: neither the desired-state row nor the
/// outbox row survives.
#[tokio::test]
async fn server_audit_reconcile_oversized_payload_rolls_back_the_adopted_write() {
    let db = TestDb::new().await;
    seed_node(&db, "node-a", None).await;
    let service = reconcile_impl(&db);

    let now = chv_common::now_unix_ms();
    let oversized = AuditOutboxInsert {
        outbox_id: "ob-reconcile-oversized".into(),
        audit_id: "audit-reconcile-oversized".into(),
        payload: AuditLogInsert {
            occurred_at_ms: now,
            actor_id: "node-a".into(),
            actor_username: "node-a".into(),
            actor_role: "node".into(),
            action: "ApplyVmDesiredState".into(),
            method: "gRPC".into(),
            path: "chv.controlplane.node.v1.ReconcileService/ApplyVmDesiredState".into(),
            resource_kind: Some("vm".into()),
            resource_id: Some("vm-oversized".into()),
            outcome: "success".into(),
            status_code: 200,
            correlation_id: None,
            operation_id: None,
            source_ip: None,
            // > the 16 KiB payload bound enforced by `enqueue_on_tx`.
            metadata: "x".repeat(16 * 1024 + 1),
        },
        created_at_ms: now,
    };
    let committed = std::sync::atomic::AtomicBool::new(false);
    let coupling = AdoptedAudit::new(&oversized, &committed);
    let error = service
        .apply_vm_desired_state(
            proto::ApplyVmDesiredStateRequest {
                meta: Some(reconcile_meta("node-a", "")),
                node_id: "node-a".into(),
                vm_id: "vm-oversized".into(),
                fragment: Some(desired_state_fragment("vm-oversized", "vm", "1", "{}")),
            },
            Some(&coupling),
        )
        .await
        .expect_err("oversized evidence must fail the mutation closed");
    assert!(
        matches!(
            &error,
            ControlPlaneServiceError::Internal(message)
                if message.contains("audit outbox payload")
        ),
        "expected the store's payload validation error, got {error:?}"
    );

    let (vms,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM vm_desired_state WHERE vm_id = 'vm-oversized'")
            .fetch_one(&db.pool)
            .await
            .expect("count desired state");
    assert_eq!(vms, 0, "the desired-state write must roll back");
    assert!(
        outbox_rows(&db.pool).await.is_empty(),
        "no outbox row survives the rollback"
    );
    assert!(!coupling.was_committed());
}

// ---------------------------------------------------------------------------
// #706 review — the two check-error branches of `finish_adopted_grpc_audit`.
//
// The outbox lookup is made to fail deterministically by renaming the table
// inside the service fake (no sleeps, no race). This exercises exactly the
// `Err(error)` arm: a committed enqueue must not be duplicated on the direct
// path, and an uncommitted one must not vanish from the trail.
// ---------------------------------------------------------------------------

/// Reconcile fake that commits the evidence through the real store, then
/// hides `audit_outbox` so the adapter's check-or-insert `get` fails.
struct ReconcileCommitThenHideOutbox {
    pool: StorePool,
    audit_id: std::sync::Mutex<Option<String>>,
}

#[tonic::async_trait]
impl ReconcileService for ReconcileCommitThenHideOutbox {
    async fn apply_node_desired_state(
        &self,
        _request: proto::ApplyNodeDesiredStateRequest,
        _audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        unreachable!("only apply_vm_desired_state is exercised")
    }

    async fn apply_vm_desired_state(
        &self,
        _request: proto::ApplyVmDesiredStateRequest,
        audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let audit = audit.expect("the adapter passes the coupling");
        let audit_id = audit.insert().audit_id.clone();
        let mut tx = self.pool.begin().await.expect("begin adopted tx");
        chv_controlplane_store::audit_outbox::enqueue_on_tx(&mut tx, audit.insert())
            .await
            .expect("enqueue evidence");
        tx.commit().await.expect("commit evidence");
        audit.mark_committed();
        *self.audit_id.lock().expect("audit id lock") = Some(audit_id);
        sqlx::query("ALTER TABLE audit_outbox RENAME TO audit_outbox_hidden")
            .execute(&self.pool)
            .await
            .expect("hide outbox table");
        Ok(ok_ack())
    }

    async fn apply_volume_desired_state(
        &self,
        _request: proto::ApplyVolumeDesiredStateRequest,
        _audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        unreachable!("only apply_vm_desired_state is exercised")
    }

    async fn apply_network_desired_state(
        &self,
        _request: proto::ApplyNetworkDesiredStateRequest,
        _audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        unreachable!("only apply_vm_desired_state is exercised")
    }

    async fn acknowledge_desired_state_version(
        &self,
        _request: proto::AcknowledgeDesiredStateVersionRequest,
        _audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        unreachable!("only apply_vm_desired_state is exercised")
    }
}

#[tokio::test]
async fn server_audit_reconcile_check_error_after_committed_enqueue_skips_direct_insert() {
    let db = TestDb::new().await;
    let fake = Arc::new(ReconcileCommitThenHideOutbox {
        pool: db.pool.clone(),
        audit_id: std::sync::Mutex::new(None),
    });
    let server =
        ReconcileServer::new(fake.clone()).with_audit(AuditLogRepository::new(db.pool.clone()));

    let request = peer_request(
        proto::ApplyVmDesiredStateRequest {
            meta: Some(reconcile_meta("node-a", "")),
            node_id: "node-a".into(),
            vm_id: "vm-check-error".into(),
            fragment: Some(desired_state_fragment("vm-check-error", "vm", "1", "{}")),
        },
        "node-a",
    );
    // The adapter's `get` fails ("no such table"); `was_committed()` is
    // true, so it must skip the direct insert and preserve the 2xx.
    server
        .apply_vm_desired_state(request)
        .await
        .expect("a committed mutation stays successful when the check fails");

    assert!(
        audit_rows(&db.pool).await.is_empty(),
        "a committed enqueue must not be double-written on the direct path"
    );

    // Restore the table: the durable row is still there for replay.
    sqlx::query("ALTER TABLE audit_outbox_hidden RENAME TO audit_outbox")
        .execute(&db.pool)
        .await
        .expect("restore outbox table");
    let rows = outbox_rows(&db.pool).await;
    assert_eq!(rows.len(), 1, "the committed row survives the failed check");
    let expected_audit_id = fake
        .audit_id
        .lock()
        .expect("audit id lock")
        .clone()
        .expect("fake captured the ticket audit id");
    assert_eq!(rows[0].audit_id, expected_audit_id);

    let dispatched = dispatch_one(&db, rows[0].created_at_ms).await;
    assert_eq!(
        dispatched[0].audit_id, expected_audit_id,
        "replay reuses the pre-minted audit id"
    );
    assert_eq!(dispatched[0].action, "ApplyVmDesiredState");
    assert_eq!(dispatched[0].outcome, "success");
    assert_eq!(audit_rows(&db.pool).await.len(), 1, "exactly one row");
}

/// Reconcile fake that hides the outbox table and fails before enqueueing.
struct ReconcileHideOutboxThenFail {
    pool: StorePool,
}

#[tonic::async_trait]
impl ReconcileService for ReconcileHideOutboxThenFail {
    async fn apply_node_desired_state(
        &self,
        _request: proto::ApplyNodeDesiredStateRequest,
        _audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        unreachable!("only apply_vm_desired_state is exercised")
    }

    async fn apply_vm_desired_state(
        &self,
        _request: proto::ApplyVmDesiredStateRequest,
        _audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        // No enqueue: the failure happens before any evidence is written.
        sqlx::query("ALTER TABLE audit_outbox RENAME TO audit_outbox_hidden")
            .execute(&self.pool)
            .await
            .expect("hide outbox table");
        Err(ControlPlaneServiceError::Internal(
            "pre-commit failure".into(),
        ))
    }

    async fn apply_volume_desired_state(
        &self,
        _request: proto::ApplyVolumeDesiredStateRequest,
        _audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        unreachable!("only apply_vm_desired_state is exercised")
    }

    async fn apply_network_desired_state(
        &self,
        _request: proto::ApplyNetworkDesiredStateRequest,
        _audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        unreachable!("only apply_vm_desired_state is exercised")
    }

    async fn acknowledge_desired_state_version(
        &self,
        _request: proto::AcknowledgeDesiredStateVersionRequest,
        _audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        unreachable!("only apply_vm_desired_state is exercised")
    }
}

#[tokio::test]
async fn server_audit_reconcile_check_error_before_enqueue_falls_back_to_direct_insert() {
    let db = TestDb::new().await;
    let server = ReconcileServer::new(Arc::new(ReconcileHideOutboxThenFail {
        pool: db.pool.clone(),
    }))
    .with_audit(AuditLogRepository::new(db.pool.clone()));

    let request = peer_request(
        proto::ApplyVmDesiredStateRequest {
            meta: Some(reconcile_meta("node-a", "")),
            node_id: "node-a".into(),
            vm_id: "vm-check-error-uncommitted".into(),
            fragment: Some(desired_state_fragment(
                "vm-check-error-uncommitted",
                "vm",
                "1",
                "{}",
            )),
        },
        "node-a",
    );
    // The adapter's `get` fails; `was_committed()` is false, so the direct
    // insert must run and the service error must be preserved (no panic).
    let status = server
        .apply_vm_desired_state(request)
        .await
        .expect_err("the service failure must be preserved");
    assert_eq!(status.code(), Code::Internal);

    let rows = audit_rows(&db.pool).await;
    assert_eq!(
        rows.len(),
        1,
        "an uncommitted check error must fall back to exactly one direct row"
    );
    assert_eq!(rows[0].action, "ApplyVmDesiredState");
    assert_eq!(rows[0].outcome, "failed");
    assert_eq!(rows[0].status_code, 500);
    assert_eq!(
        rows[0].resource_id.as_deref(),
        Some("vm-check-error-uncommitted")
    );

    sqlx::query("ALTER TABLE audit_outbox_hidden RENAME TO audit_outbox")
        .execute(&db.pool)
        .await
        .expect("restore outbox table");
    assert!(
        outbox_rows(&db.pool).await.is_empty(),
        "nothing was enqueued, so the outbox stays empty"
    );
}
