//! Regression tests for C1 + H1: cross-node trust gap on agent-side gRPC handlers.
//!
//! Threat scenario: a compromised node holding a *valid* leaf certificate for
//! `node-a` mints a request that asserts `node_id = "node-b"` on the wire,
//! attempting to rotate node-b's certificate / overwrite node-b's observed
//! state / pollute its telemetry.
//!
//! The fix wires a peer-identity layer (see `peer_identity.rs`) that pins the
//! wire-asserted `node_id` to the peer's mTLS leaf cert. These tests assert
//! the gRPC server shim rejects mismatches with `permission_denied` and
//! accepts matches.
//!
//! These tests deliberately exercise the `server.rs` shim layer (not the inner
//! service trait), because the shim is where peer-identity enforcement lives.
//! They construct `tonic::Request` instances with synthetic `PeerNodeId`
//! extensions — equivalent to what the production `PeerIdentityInterceptor`
//! would have inserted.

use chv_controlplane_service::{
    parse_node_id_from_der, EnrollmentServer, EnrollmentServiceImplementation, InsecurePeer,
    IssuedCertificate, PeerCertificateSerial, PeerIdentityInterceptor, PeerNodeId,
    RevocationSnapshot,
};
use chv_controlplane_store::{
    test_util::create_test_pool, BootstrapTokenRepository, NodeRepository, NodeUpsertInput,
    VtepRepository,
};
use chv_controlplane_types::domain::NodeId;
use control_plane_node_api::control_plane_node_api as proto;
use std::sync::Arc;
use tonic::{Code, Request};

// ---- helpers ---------------------------------------------------------------

/// Build a minimal self-signed leaf cert (DER) carrying `node_id` in CN+SAN.
fn fake_leaf_der(node_id: &str) -> Vec<u8> {
    use rcgen::string::Ia5String;
    use rcgen::{CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, SanType};

    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    params.distinguished_name.push(DnType::CommonName, node_id);
    let dns = Ia5String::try_from(node_id.to_string()).unwrap();
    params.subject_alt_names.push(SanType::DnsName(dns));
    params.is_ca = IsCa::NoCa;

    let key = KeyPair::generate().unwrap();
    let cert = params.self_signed(&key).unwrap();
    cert.der().to_vec()
}

/// CertificateIssuer stub that always returns a fixed payload.
struct StubIssuer;

#[async_trait::async_trait]
impl chv_controlplane_service::CertificateIssuer for StubIssuer {
    async fn issue_node_certificate(
        &self,
        _node_id: &NodeId,
    ) -> Result<IssuedCertificate, chv_controlplane_service::ControlPlaneServiceError> {
        Ok(IssuedCertificate {
            certificate_pem: b"cert".to_vec(),
            private_key_pem: b"key".to_vec(),
            ca_pem: b"ca".to_vec(),
            serial: "stub-serial".into(),
        })
    }
}

async fn build_enrollment_server() -> (EnrollmentServer, NodeRepository) {
    let pool = create_test_pool().await;
    let node_repo = NodeRepository::new(pool.clone());
    let token_repo = BootstrapTokenRepository::new(pool.clone());
    let vtep_repo = VtepRepository::new(pool.clone());

    // Pre-seed node-a so cert rotation against the real DB does not fail
    // before reaching the peer-identity check.
    node_repo
        .upsert_node(&NodeUpsertInput {
            node_id: NodeId::new("node-a").unwrap(),
            hostname: "host-a".into(),
            display_name: "node-a".into(),
            certificate_serial: Some("old-serial".into()),
            agent_version: Some("0.1.0".into()),
            control_plane_version: Some("0.1.0".into()),
            enrolled_unix_ms: 1_000,
            last_seen_unix_ms: 1_000,
        })
        .await
        .expect("seed node-a");

    let svc = EnrollmentServiceImplementation::new(
        node_repo.clone(),
        token_repo,
        Some(Arc::new(StubIssuer)),
        vtep_repo,
    )
    .with_certificate_revocations(
        chv_controlplane_store::CertificateRevocationRepository::new(pool.clone()),
    );
    (EnrollmentServer::new(Arc::new(svc)), node_repo)
}

fn request_with_peer<T>(payload: T, peer_node_id: &str) -> Request<T> {
    let mut req = Request::new(payload);
    req.extensions_mut().insert(PeerNodeId(
        NodeId::new(peer_node_id.to_string()).expect("valid node_id"),
    ));
    req
}

/// Production-shaped request: peer identity plus the serial of the leaf
/// certificate the peer presented.
fn request_with_peer_serial<T>(payload: T, peer_node_id: &str, serial: Option<&str>) -> Request<T> {
    let mut req = request_with_peer(payload, peer_node_id);
    req.extensions_mut()
        .insert(PeerCertificateSerial(serial.map(str::to_string)));
    req
}

fn rotate_payload(node_id: &str, operation_id: &str) -> proto::RotateNodeCertificateRequest {
    proto::RotateNodeCertificateRequest {
        meta: Some(proto::RequestMeta {
            operation_id: operation_id.into(),
            requested_by: "node-a".into(),
            target_node_id: node_id.into(),
            desired_state_version: "1".into(),
            request_unix_ms: 0,
        }),
        node_id: node_id.into(),
    }
}

// ---- regression tests for C1 + H1 -----------------------------------------

/// C1 regression: a peer authenticated as `node-a` MUST NOT be able to rotate
/// `node-b`'s certificate. Without the fix, the handler accepts the wire
/// `node_id` and mints a fresh cert for node-b — a complete cross-node
/// impersonation primitive.
#[tokio::test]
async fn rotate_node_certificate_rejects_peer_node_id_mismatch() {
    use proto::enrollment_service_server::EnrollmentService as _;

    let (server, _repo) = build_enrollment_server().await;

    let payload = proto::RotateNodeCertificateRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "op-1".into(),
            requested_by: "node-a".into(),
            target_node_id: "node-b".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 0,
        }),
        node_id: "node-b".into(), // attacker asserts a different node
    };

    let req = request_with_peer(payload, "node-a");
    let status = server
        .rotate_node_certificate(req)
        .await
        .expect_err("must reject cross-node rotation");

    assert_eq!(
        status.code(),
        Code::PermissionDenied,
        "mismatch must produce permission_denied; got: {} ({})",
        status.code(),
        status.message()
    );
    assert!(
        status.message().contains("node-a") && status.message().contains("node-b"),
        "status message must name both peer and requested ids; got: {}",
        status.message()
    );
}

/// Positive path: when the peer matches and presents the serial of its
/// currently trusted certificate, rotation proceeds.
#[tokio::test]
async fn rotate_node_certificate_accepts_matching_peer_and_serial() {
    use proto::enrollment_service_server::EnrollmentService as _;

    let (server, repo) = build_enrollment_server().await;

    let payload = proto::RotateNodeCertificateRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "op-2".into(),
            requested_by: "node-a".into(),
            target_node_id: "node-a".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 0,
        }),
        node_id: "node-a".into(),
    };

    let req = request_with_peer_serial(payload, "node-a", Some("old-serial"));
    let resp = server
        .rotate_node_certificate(req)
        .await
        .expect("matching peer and serial must succeed");
    let body = resp.into_inner();
    assert!(!body.certificate_pem.is_empty());
    assert_eq!(
        repo.get_certificate_serial(&NodeId::new("node-a").unwrap())
            .await
            .unwrap()
            .as_deref(),
        Some("stub-serial"),
        "successful rotation must persist the newly issued serial"
    );
}

/// Stale/foreign serial: the peer's node id matches, but the presented
/// certificate is not the one the control plane currently trusts. This is the
/// replay/old-cert case #661 closes: reject without minting or mutating.
#[tokio::test]
async fn rotate_node_certificate_rejects_stale_serial_without_mutation() {
    use proto::enrollment_service_server::EnrollmentService as _;

    let (server, repo) = build_enrollment_server().await;
    let req = request_with_peer_serial(
        rotate_payload("node-a", "op-stale"),
        "node-a",
        Some("stale-serial"),
    );

    let status = server
        .rotate_node_certificate(req)
        .await
        .expect_err("stale serial must be rejected");
    assert_eq!(
        status.code(),
        Code::PermissionDenied,
        "stale serial must produce permission_denied; got: {} ({})",
        status.code(),
        status.message()
    );
    assert_eq!(
        repo.get_certificate_serial(&NodeId::new("node-a").unwrap())
            .await
            .unwrap()
            .as_deref(),
        Some("old-serial"),
        "rejected rotation must leave the stored serial unchanged"
    );
}

/// A revoked serial must not rotate, even though it still matches the stored
/// active serial. Otherwise a thief holding the stolen certificate could
/// exchange it for a fresh, unrevoked certificate and defeat revocation.
#[tokio::test]
async fn rotate_node_certificate_rejects_revoked_serial() {
    use proto::enrollment_service_server::EnrollmentService as _;

    let (server, repo) = build_enrollment_server().await;
    chv_controlplane_store::CertificateRevocationRepository::new(repo.pool().clone())
        .revoke(&chv_controlplane_store::RevokeCertificateInput {
            certificate_serial: "old-serial",
            node_id: "node-a",
            reason: "host compromise",
            revoked_by: Some("admin"),
            revoked_at: None,
            expires_at: None,
        })
        .await
        .expect("revoke old-serial");

    let req = request_with_peer_serial(
        rotate_payload("node-a", "op-revoked"),
        "node-a",
        Some("old-serial"),
    );

    let status = server
        .rotate_node_certificate(req)
        .await
        .expect_err("revoked serial must not rotate");
    assert_eq!(
        status.code(),
        Code::PermissionDenied,
        "revoked serial must produce permission_denied; got: {} ({})",
        status.code(),
        status.message()
    );
    assert!(
        status.message().contains("revoked"),
        "operator-actionable message expected, got: {}",
        status.message()
    );
    assert_eq!(
        repo.get_certificate_serial(&NodeId::new("node-a").unwrap())
            .await
            .unwrap()
            .as_deref(),
        Some("old-serial"),
        "rejected rotation must leave the stored serial unchanged"
    );
}

/// Production requests without a presented serial (no extension at all) must
/// fail closed even when the node id matches.
#[tokio::test]
async fn rotate_node_certificate_rejects_missing_presented_serial() {
    use proto::enrollment_service_server::EnrollmentService as _;

    let (server, repo) = build_enrollment_server().await;
    let req = request_with_peer(rotate_payload("node-a", "op-missing-serial"), "node-a");

    let status = server
        .rotate_node_certificate(req)
        .await
        .expect_err("missing presented serial must be rejected");
    assert_eq!(status.code(), Code::PermissionDenied);
    assert_eq!(
        repo.get_certificate_serial(&NodeId::new("node-a").unwrap())
            .await
            .unwrap()
            .as_deref(),
        Some("old-serial")
    );
}

/// Explicit `None` serial (extension present but unrepresentable) is equally
/// rejected in production.
#[tokio::test]
async fn rotate_node_certificate_rejects_none_presented_serial() {
    use proto::enrollment_service_server::EnrollmentService as _;

    let (server, _repo) = build_enrollment_server().await;
    let req = request_with_peer_serial(rotate_payload("node-a", "op-none-serial"), "node-a", None);

    let status = server
        .rotate_node_certificate(req)
        .await
        .expect_err("None presented serial must be rejected");
    assert_eq!(status.code(), Code::PermissionDenied);
}

/// A missing node row must still surface NotFound (not a serial/permission
/// error) even when the request is otherwise production-shaped.
#[tokio::test]
async fn rotate_node_certificate_missing_node_returns_not_found() {
    use proto::enrollment_service_server::EnrollmentService as _;

    let (server, _repo) = build_enrollment_server().await;
    let req = request_with_peer_serial(
        rotate_payload("node-missing", "op-missing-node"),
        "node-missing",
        Some("any-serial"),
    );

    let status = server
        .rotate_node_certificate(req)
        .await
        .expect_err("missing node must be rejected");
    assert_eq!(status.code(), Code::NotFound);
}

/// H1 regression: bootstrap-result reports must also be peer-pinned. An
/// attacker holding node-a's cert cannot poison node-b's bootstrap state.
#[tokio::test]
async fn report_bootstrap_result_rejects_peer_node_id_mismatch() {
    use proto::enrollment_service_server::EnrollmentService as _;

    let (server, _repo) = build_enrollment_server().await;

    let payload = proto::ReportBootstrapResultRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "op-3".into(),
            requested_by: "node-a".into(),
            target_node_id: "node-b".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 0,
        }),
        node_id: "node-b".into(),
        bootstrap_status: "success".into(),
        message: "".into(),
    };

    let req = request_with_peer(payload, "node-a");
    let status = server
        .report_bootstrap_result(req)
        .await
        .expect_err("must reject cross-node bootstrap report");

    assert_eq!(status.code(), Code::PermissionDenied);
}

/// Defense-in-depth: when no peer identity extension is present (i.e. the
/// interceptor did NOT run), the handler must still reject — never silently
/// accept the wire-asserted node_id.
#[tokio::test]
async fn rotate_node_certificate_rejects_when_peer_identity_missing() {
    use proto::enrollment_service_server::EnrollmentService as _;

    let (server, _repo) = build_enrollment_server().await;

    let payload = proto::RotateNodeCertificateRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "op-4".into(),
            requested_by: "node-a".into(),
            target_node_id: "node-a".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 0,
        }),
        node_id: "node-a".into(),
    };

    // Do NOT insert PeerNodeId — simulate misconfigured server / interceptor bug.
    let req = Request::new(payload);
    let status = server
        .rotate_node_certificate(req)
        .await
        .expect_err("must reject when peer identity absent");
    assert_eq!(status.code(), Code::PermissionDenied);
}

/// Insecure-mode passthrough: when the request carries [`InsecurePeer`]
/// (CHV_ALLOW_INSECURE=1 dev mode), peer verification is intentionally
/// bypassed. Production deployments MUST NOT enable insecure mode, but the
/// new serial is still recorded so dev/test rotation stays coherent.
#[tokio::test]
async fn rotate_node_certificate_insecure_mode_skips_check() {
    use proto::enrollment_service_server::EnrollmentService as _;

    let (server, repo) = build_enrollment_server().await;

    let payload = proto::RotateNodeCertificateRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "op-5".into(),
            requested_by: "anyone".into(),
            target_node_id: "node-a".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 0,
        }),
        node_id: "node-a".into(),
    };

    let mut req = Request::new(payload);
    req.extensions_mut().insert(InsecurePeer);

    let resp = server
        .rotate_node_certificate(req)
        .await
        .expect("insecure mode must skip peer check");
    let body = resp.into_inner();
    assert!(!body.certificate_pem.is_empty());
    assert_eq!(
        repo.get_certificate_serial(&NodeId::new("node-a").unwrap())
            .await
            .unwrap()
            .as_deref(),
        Some("stub-serial"),
        "insecure rotation must still record the new serial"
    );
}

/// Sanity: the cert parser used by the production interceptor recovers the
/// node_id from the SAN of a CHV-issued cert. (Pairs the unit test in
/// `peer_identity::tests` from a public-API surface.)
#[test]
fn cert_parser_round_trips_node_id() {
    let der = fake_leaf_der("node-x");
    let got = parse_node_id_from_der(&der).expect("parse");
    assert_eq!(got.as_str(), "node-x");
}

/// Interceptor wiring smoke test: in insecure mode, the interceptor inserts
/// `InsecurePeer` into request extensions and lets the request through.
/// Only valid when compiled with the `dev` feature — production builds reject allow_insecure=true.
#[test]
#[cfg(feature = "dev")]
fn interceptor_insecure_mode_marks_request() {
    let interceptor = PeerIdentityInterceptor::new(true, RevocationSnapshot::empty())
        .expect("dev build permits insecure mode");
    let req: Request<()> = Request::new(());
    let out = interceptor.intercept(req).expect("insecure passthrough");
    assert!(out.extensions().get::<InsecurePeer>().is_some());
    assert!(out.extensions().get::<PeerNodeId>().is_none());
}

/// In production mode (mTLS required), a request with no TLS info MUST be
/// rejected by the interceptor — defense-in-depth so handlers never see a
/// production request without a pinned peer identity.
#[test]
fn interceptor_secure_mode_rejects_when_tls_info_absent() {
    let interceptor = PeerIdentityInterceptor::new(false, RevocationSnapshot::empty())
        .expect("secure mode always valid");
    let req: Request<()> = Request::new(());
    let err = interceptor
        .intercept(req)
        .expect_err("must reject when no TLS connect info");
    assert_eq!(err.code(), Code::Unauthenticated);
}
