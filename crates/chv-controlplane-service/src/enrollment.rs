use crate::error::ControlPlaneServiceError;
use async_trait::async_trait;
use chv_controlplane_store::{
    BootstrapTokenRepository, BootstrapTokenValidation, NodeBootstrapResultInput,
    NodeInventoryInput, NodeRepository, NodeUpsertInput, NodeVersionInput, VtepRepository,
};
use chv_controlplane_types::domain::NodeId;
use control_plane_node_api::control_plane_node_api as proto;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// WireGuard listener port of chv-nwd's fabric transport (ADR-021 §6).
/// This matches the `fabric.wireguard_port` DEFAULT in chv-config, but
/// the port is node-config-overridable and the control plane cannot see
/// node configuration: a node that overrides `fabric.wireguard_port`
/// must not rely on control-plane-derived underlay endpoints (they pin
/// this default port). There is deliberately no CP-side knob for this
/// value — it exists only to derive endpoints from observed gRPC peer
/// addresses until agents report their own underlay addressing.
pub(crate) const FABRIC_WIREGUARD_PORT: u16 = 65001;

/// Derive a node's fabric underlay endpoint from the transport-level
/// peer address the control plane actually observed, pinned to the
/// fabric WireGuard port. IPv6 peer addresses are bracketed so the
/// endpoint parses as `host:port`.
pub(crate) fn derive_underlay_endpoint(addr: SocketAddr) -> String {
    match addr.ip() {
        std::net::IpAddr::V4(ip) => format!("{ip}:{FABRIC_WIREGUARD_PORT}"),
        std::net::IpAddr::V6(ip) => format!("[{ip}]:{FABRIC_WIREGUARD_PORT}"),
    }
}

#[async_trait]
pub trait CertificateIssuer: Send + Sync {
    async fn issue_node_certificate(
        &self,
        node_id: &NodeId,
    ) -> Result<IssuedCertificate, ControlPlaneServiceError>;
}

pub struct IssuedCertificate {
    pub certificate_pem: Vec<u8>,
    pub private_key_pem: Vec<u8>,
    pub ca_pem: Vec<u8>,
    pub serial: String,
}

/// Bounded attempts to mint a serial that is not already denied.
///
/// CHV serials are 32-bit (`gen_short_id`), so a collision with an existing
/// denylist row is unlikely but possible. Handing out a certificate whose
/// serial is already revoked would make a freshly issued credential unusable
/// (and, worse, indistinguishable from the revoked one in audit logs).
pub(crate) const MAX_CERTIFICATE_ISSUE_ATTEMPTS: usize = 8;

/// Issue a node certificate, regenerating on a denylist collision.
///
/// The revocation table is authoritative for rotation; this closes the residual
/// case where a freshly minted 32-bit serial happens to equal a previously
/// revoked serial. Bounded by [`MAX_CERTIFICATE_ISSUE_ATTEMPTS`] so a broken
/// random source fails instead of spinning.
pub(crate) async fn issue_unrevoked_node_certificate(
    issuer: &dyn CertificateIssuer,
    node_id: &NodeId,
    revocation_repo: Option<&chv_controlplane_store::CertificateRevocationRepository>,
) -> Result<IssuedCertificate, ControlPlaneServiceError> {
    for attempt in 0..MAX_CERTIFICATE_ISSUE_ATTEMPTS {
        let cert = issuer.issue_node_certificate(node_id).await?;
        match revocation_repo {
            Some(repo) if repo.is_revoked(&cert.serial).await? => {
                tracing::warn!(
                    node_id = %node_id,
                    certificate_serial = %cert.serial,
                    attempt,
                    "freshly minted certificate serial is revoked; regenerating"
                );
            }
            _ => return Ok(cert),
        }
    }

    Err(ControlPlaneServiceError::Internal(format!(
        "failed to mint a certificate with an unrevoked serial for node {node_id} \
         after {MAX_CERTIFICATE_ISSUE_ATTEMPTS} attempts"
    )))
}

pub use crate::audit_coupling::AdoptedAudit;

#[async_trait]
pub trait EnrollmentService: Send + Sync {
    /// `peer_addr` is the transport-level remote address of the gRPC call
    /// (from `tonic::Request::remote_addr`). It is used to derive the
    /// node's fabric underlay endpoint when the request does not carry
    /// one explicitly.
    ///
    /// `audit` is the request's pre-minted audit-outbox row (#706). When
    /// present, the implementation enqueues it inside its **final**
    /// mutation transaction so the evidence commits with the mutation or
    /// not at all. `None` means "not coupled": in-process/BFF callers and
    /// adapters running without an audit store pass `None` and the caller
    /// keeps the post-hoc direct insert.
    async fn enroll_node(
        &self,
        request: proto::EnrollmentRequest,
        peer_addr: Option<SocketAddr>,
        audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::EnrollmentResponse, ControlPlaneServiceError>;

    /// Rotate a node's certificate.
    ///
    /// `presented_serial` is the serial of the mTLS leaf certificate the peer
    /// actually presented (from [`crate::PeerCertificateSerial`]); in secure
    /// mode it must match the serial currently recorded for the node. This is
    /// the proof-of-possession check that prevents a compromised node holding
    /// its own valid certificate from rotating another node's certificate, and
    /// prevents replay with a stale certificate after a rotation.
    ///
    /// `allow_insecure` is the dev-mode (`CHV_ALLOW_INSECURE=1`) bypass. It
    /// still records the new serial, but skips the presented/stored equality
    /// check; it must never be set in production.
    ///
    /// `audit` follows [`Self::enroll_node`]: the outbox row commits with
    /// the compare-and-swap that records the new serial (and is never
    /// enqueued when the swap loses the race).
    async fn rotate_node_certificate(
        &self,
        request: proto::RotateNodeCertificateRequest,
        presented_serial: Option<String>,
        allow_insecure: bool,
        audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::RotateNodeCertificateResponse, ControlPlaneServiceError>;

    /// `audit` follows [`Self::enroll_node`]: the outbox row commits with
    /// the bootstrap-result upsert.
    async fn report_bootstrap_result(
        &self,
        request: proto::ReportBootstrapResultRequest,
        audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;
}

#[derive(Clone)]
pub struct EnrollmentServiceImplementation {
    node_repo: NodeRepository,
    token_repo: BootstrapTokenRepository,
    cert_issuer: Option<Arc<dyn CertificateIssuer>>,
    vtep_repo: VtepRepository,
    /// Durable certificate denylist (#661). The store-level compare-and-swap
    /// always rejects revoked serials; wiring this in adds a pre-mint check
    /// with an operator-actionable error instead of a "lost race" conflict.
    revocation_repo: Option<chv_controlplane_store::CertificateRevocationRepository>,
}

impl EnrollmentServiceImplementation {
    pub fn new(
        node_repo: NodeRepository,
        token_repo: BootstrapTokenRepository,
        cert_issuer: Option<Arc<dyn CertificateIssuer>>,
        vtep_repo: VtepRepository,
    ) -> Self {
        Self {
            node_repo,
            token_repo,
            cert_issuer,
            vtep_repo,
            revocation_repo: None,
        }
    }

    /// Attach the certificate denylist so rotation fails fast (with the
    /// revocation reason class) before minting key material.
    pub fn with_certificate_revocations(
        mut self,
        revocation_repo: chv_controlplane_store::CertificateRevocationRepository,
    ) -> Self {
        self.revocation_repo = Some(revocation_repo);
        self
    }

    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64
    }
}

use chv_controlplane_types::constants::{
    COMPONENT_AGENT, SOURCE_ENROLLMENT, STATUS_OK, SUMMARY_BOOTSTRAP_REPORTED,
    SUMMARY_CERT_ROTATED, SUMMARY_NODE_ENROLLED,
};

#[async_trait]
impl EnrollmentService for EnrollmentServiceImplementation {
    async fn enroll_node(
        &self,
        request: proto::EnrollmentRequest,
        peer_addr: Option<SocketAddr>,
        audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::EnrollmentResponse, ControlPlaneServiceError> {
        match self
            .token_repo
            .validate_and_consume(&request.bootstrap_token)
            .await?
        {
            BootstrapTokenValidation::Valid => {}
            _ => {
                return Err(ControlPlaneServiceError::Unauthorized(
                    "invalid bootstrap token".into(),
                ))
            }
        }

        let inventory = request
            .inventory
            .ok_or_else(|| ControlPlaneServiceError::InvalidArgument("missing inventory".into()))?;
        let versions = request
            .versions
            .ok_or_else(|| ControlPlaneServiceError::InvalidArgument("missing versions".into()))?;

        let node_id = NodeId::new(inventory.node_id.clone()).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid node_id: {}", e))
        })?;
        // The node id persists into the nodes table and later reaches
        // agent-socket pattern substitution (dispatch, migration, the
        // resolve relay) and certificate subjects: it must be a single safe
        // path component. `is_safe_path_component` keeps legitimately-named
        // nodes enrollable — only traversal-shaped ids (separators, dot
        // components, control characters) are rejected, at the boundary
        // where node data enters the control plane.
        if !chv_common::is_safe_path_component(node_id.as_str()) {
            return Err(ControlPlaneServiceError::InvalidArgument(format!(
                "invalid node_id {:?}: must be a single path component",
                node_id.as_str()
            )));
        }

        // Issue certificates
        let cert_issuer = self
            .cert_issuer
            .as_ref()
            .ok_or_else(|| ControlPlaneServiceError::Internal("CA not configured".into()))?;
        let cert = issue_unrevoked_node_certificate(
            cert_issuer.as_ref(),
            &node_id,
            self.revocation_repo.as_ref(),
        )
        .await?;

        let now = self.now_ms();

        // Persist node
        self.node_repo
            .upsert_node(&NodeUpsertInput {
                node_id: node_id.clone(),
                hostname: inventory.hostname.clone(),
                display_name: inventory.hostname.clone(),
                certificate_serial: Some(cert.serial.clone()),
                agent_version: Some(versions.chv_agent_version.clone()),
                control_plane_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                enrolled_unix_ms: now,
                last_seen_unix_ms: now,
            })
            .await?;

        // Persist inventory
        self.node_repo
            .upsert_inventory(&NodeInventoryInput {
                node_id: node_id.clone(),
                architecture: inventory.architecture.clone(),
                kernel_version: None,
                os_release: None,
                cpu_count: inventory.cpu_threads as i32,
                memory_bytes: inventory.memory_bytes as i64,
                disk_bytes: None,
                cloud_hypervisor_version: Some(versions.cloud_hypervisor_version.clone()),
                chv_agent_version: Some(versions.chv_agent_version.clone()),
                chv_stord_version: Some(versions.chv_stord_version.clone()),
                chv_nwd_version: Some(versions.chv_nwd_version.clone()),
                host_bundle_version: Some(versions.host_bundle_version.clone()),
                inventory_status: Some(SOURCE_ENROLLMENT.into()),
                storage_classes: if inventory.storage_classes.is_empty() {
                    None
                } else {
                    Some(
                        serde_json::to_value(&inventory.storage_classes).map_err(|e| {
                            ControlPlaneServiceError::Internal(format!(
                                "failed to serialize storage_classes: {}",
                                e
                            ))
                        })?,
                    )
                },
                network_capabilities: if inventory.network_capabilities.is_empty() {
                    None
                } else {
                    Some(
                        serde_json::to_value(&inventory.network_capabilities).map_err(|e| {
                            ControlPlaneServiceError::Internal(format!(
                                "failed to serialize network_capabilities: {}",
                                e
                            ))
                        })?,
                    )
                },
                labels: if inventory.labels.is_empty() {
                    None
                } else {
                    Some(serde_json::to_value(&inventory.labels).map_err(|e| {
                        ControlPlaneServiceError::Internal(format!(
                            "failed to serialize labels: {}",
                            e
                        ))
                    })?)
                },
                hypervisor_capabilities: if inventory.hypervisor_capabilities.is_empty() {
                    None
                } else {
                    Some(
                        serde_json::to_value(&inventory.hypervisor_capabilities).map_err(|e| {
                            ControlPlaneServiceError::Internal(format!(
                                "failed to serialize hypervisor_capabilities: {}",
                                e
                            ))
                        })?,
                    )
                },
                // #378: the authority mode rides the enrollment inventory
                // (first contact) and is re-reported on every periodic
                // inventory cycle.
                authority_mode: crate::inventory::authority_mode_text(inventory.authority_mode),
                reported_unix_ms: now,
            })
            .await?;

        // Register versions as history
        //
        // #706: the audit-outbox row rides the **final** fail-closed write
        // of the enrollment sequence so evidence and mutation share one
        // commit. A node that reports a fabric identity finishes on
        // `register_fabric_identity`; a node that does not finishes here
        // (the legacy VTEP upsert that follows is best-effort and can never
        // fail the RPC). Everything earlier — token consumption, the node
        // row, the inventory row — commits separately: a crash mid-sequence
        // still leaves a partially applied enrollment with no outbox row.
        // That is the documented residual risk for this RPC (see
        // `docs/specs/component/audit-log.md`); folding enrollment into one
        // transaction is a follow-up, not part of this change.
        let reports_fabric_identity = !inventory.wireguard_public_key.is_empty();
        let version_audit = if reports_fabric_identity { None } else { audit };
        let fabric_audit = if reports_fabric_identity { audit } else { None };

        self.node_repo
            .append_version(
                &NodeVersionInput {
                    node_id: node_id.clone(),
                    component_name: COMPONENT_AGENT.into(),
                    version: versions.chv_agent_version,
                    source: Some(SOURCE_ENROLLMENT.into()),
                    reported_unix_ms: now,
                },
                version_audit.map(AdoptedAudit::insert),
            )
            .await?;
        if let Some(audit) = version_audit {
            audit.mark_committed();
        }

        // Register VTEP for overlay networking
        if !inventory.vtep_ip.is_empty() {
            if let Err(e) = self
                .vtep_repo
                .register_vtep(
                    node_id.as_str(),
                    &inventory.vtep_ip,
                    4789, // default VXLAN port
                )
                .await
            {
                tracing::warn!(node_id = %node_id, error = %e, "failed to register VTEP during enrollment");
            }
        }

        // Register the node's fabric identity (ADR-021 §5). Unlike the
        // legacy VTEP registration above, this fails closed: a node whose
        // reported identity cannot be persisted must not be treated as
        // enrolled, or the fabric plan compiler would later fail on a node
        // the control plane believes it knows. Only the public key is
        // transported (never private key material).
        if !inventory.wireguard_public_key.is_empty() {
            self.vtep_repo
                .register_fabric_identity(
                    node_id.as_str(),
                    &inventory.wireguard_public_key,
                    inventory.underlay_mtu,
                    // The enrollment request carries no explicit underlay
                    // endpoint, so derive one from the transport-level peer
                    // address the control plane actually observed, pinned to
                    // the fabric WireGuard port. First registration wins at
                    // the store layer, so a re-enrollment through a
                    // different address does not rotate the endpoint. NAT /
                    // LB / proxy caveat: behind NAT this is the NAT's
                    // mapped address — usually exactly what remote peers
                    // must dial — but a NAT, load balancer, or proxy that
                    // maps the gRPC connection differently from the node's
                    // WireGuard listener yields an unreachable endpoint; in
                    // particular, nodes enrolled through one shared
                    // LB/proxy all appear behind the proxy's address, which
                    // then gets pinned at first registration (every node
                    // behind it would dial the proxy). The fabric plan
                    // compiler surfaces unreachable peers (fail closed)
                    // instead of guessing.
                    peer_addr.map(derive_underlay_endpoint).as_deref(),
                    fabric_audit.map(AdoptedAudit::insert),
                )
                .await?;
            if let Some(audit) = fabric_audit {
                audit.mark_committed();
            }
        }

        Ok(proto::EnrollmentResponse {
            result: Some(proto::ResultMeta {
                operation_id: "".into(),
                status: STATUS_OK.into(),
                node_observed_generation: "".into(),
                error_code: "".into(),
                human_summary: SUMMARY_NODE_ENROLLED.into(),
            }),
            issued_node_id: node_id.to_string(),
            certificate_pem: cert.certificate_pem,
            private_key_pem: cert.private_key_pem,
            ca_pem: cert.ca_pem,
        })
    }

    async fn rotate_node_certificate(
        &self,
        request: proto::RotateNodeCertificateRequest,
        presented_serial: Option<String>,
        allow_insecure: bool,
        audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::RotateNodeCertificateResponse, ControlPlaneServiceError> {
        let node_id = NodeId::new(request.node_id).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid node_id: {}", e))
        })?;

        let meta = request
            .meta
            .ok_or_else(|| ControlPlaneServiceError::InvalidArgument("missing meta".into()))?;

        // Load the currently trusted serial before minting anything. A missing
        // node is NotFound (not a proof-of-possession failure); an existing
        // node with a NULL serial is a legacy row we cannot safely rotate.
        let stored_serial = self.node_repo.get_certificate_serial(&node_id).await?;

        if !allow_insecure {
            let expected = stored_serial.as_deref().ok_or_else(|| {
                ControlPlaneServiceError::FailedPrecondition(format!(
                    "node {node_id} has no recorded certificate serial (legacy enrollment); \
                     re-enroll the node before rotating its certificate"
                ))
            })?;
            let presented = presented_serial.as_deref().ok_or_else(|| {
                ControlPlaneServiceError::PermissionDenied(
                    "certificate rotation requires proof of possession of the current node \
                     certificate serial"
                        .into(),
                )
            })?;
            if presented != expected {
                tracing::warn!(
                    node_id = %node_id,
                    presented_serial = %presented,
                    "rejecting certificate rotation: presented serial does not match active serial"
                );
                return Err(ControlPlaneServiceError::PermissionDenied(
                    "presented certificate serial does not match the node's active certificate; \
                     re-enroll if the current certificate was lost"
                        .into(),
                ));
            }

            // A revoked serial must never be exchangeable for a fresh
            // certificate: otherwise a thief holding the stolen certificate
            // could rotate to a new, unrevoked one. This pre-check gives a
            // clear error; the store's compare-and-swap independently refuses
            // the same serial, closing the check-to-commit race.
            if let Some(repo) = &self.revocation_repo {
                if repo.is_revoked(expected).await? {
                    tracing::warn!(
                        node_id = %node_id,
                        certificate_serial = %expected,
                        "rejecting certificate rotation: presented certificate is revoked"
                    );
                    return Err(ControlPlaneServiceError::PermissionDenied(
                        "the presented certificate has been revoked; re-enroll the node with a \
                         fresh bootstrap token"
                            .into(),
                    ));
                }
            }
        }

        // Only now, after the proof-of-possession check, mint the new cert.
        let cert_issuer = self
            .cert_issuer
            .as_ref()
            .ok_or_else(|| ControlPlaneServiceError::Internal("CA not configured".into()))?;
        let cert = issue_unrevoked_node_certificate(
            cert_issuer.as_ref(),
            &node_id,
            self.revocation_repo.as_ref(),
        )
        .await?;

        // Commit with a compare-and-swap against the serial we validated. If a
        // concurrent rotation won the race, do not hand out this key material.
        match stored_serial.as_deref() {
            Some(expected) => {
                let rotated = self
                    .node_repo
                    .rotate_certificate_serial(
                        &node_id,
                        expected,
                        &cert.serial,
                        audit.map(AdoptedAudit::insert),
                    )
                    .await?;
                if !rotated {
                    return Err(ControlPlaneServiceError::Conflict(format!(
                        "certificate rotation for node {node_id} lost a concurrent rotation \
                         race; retry with the current certificate"
                    )));
                }
            }
            // Insecure/dev mode only: no stored serial to compare against
            // (legacy row). Preserve the existing unconditional update so
            // local development/tests keep working.
            None => {
                self.node_repo
                    .update_certificate_serial(
                        &node_id,
                        &cert.serial,
                        audit.map(AdoptedAudit::insert),
                    )
                    .await?;
            }
        }
        if let Some(audit) = audit {
            audit.mark_committed();
        }

        Ok(proto::RotateNodeCertificateResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id,
                status: STATUS_OK.into(),
                node_observed_generation: "".into(),
                error_code: "".into(),
                human_summary: SUMMARY_CERT_ROTATED.into(),
            }),
            certificate_pem: cert.certificate_pem,
            private_key_pem: cert.private_key_pem,
            ca_pem: cert.ca_pem,
        })
    }

    async fn report_bootstrap_result(
        &self,
        request: proto::ReportBootstrapResultRequest,
        audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let node_id = NodeId::new(request.node_id).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid node_id: {}", e))
        })?;

        let meta = request
            .meta
            .ok_or_else(|| ControlPlaneServiceError::InvalidArgument("missing meta".into()))?;

        let status_lower = request.bootstrap_status.to_lowercase();
        let success = status_lower == "success" || status_lower == "ok";

        // PERSIST BOOTSTRAP RESULT
        self.node_repo
            .upsert_bootstrap_result(
                &NodeBootstrapResultInput {
                    node_id,
                    operation_id: if meta.operation_id.is_empty() {
                        None
                    } else {
                        Some(meta.operation_id.clone())
                    },
                    success,
                    error_message: if success { None } else { Some(request.message) },
                    details: None, // bootstrap report doesn't have details_json yet
                    started_unix_ms: None,
                    completed_unix_ms: self.now_ms(),
                },
                audit.map(AdoptedAudit::insert),
            )
            .await?;
        if let Some(audit) = audit {
            audit.mark_committed();
        }

        Ok(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id,
                status: STATUS_OK.into(),
                node_observed_generation: "".into(),
                error_code: "".into(),
                human_summary: SUMMARY_BOOTSTRAP_REPORTED.into(),
            }),
        })
    }
}

pub struct CaBackedCertificateIssuer {
    ca_issuer: rcgen::Issuer<'static, rcgen::KeyPair>,
    ca_pem: String,
}

impl CaBackedCertificateIssuer {
    pub fn new(ca_cert_pem: &str, ca_key_pem: &str) -> Result<Self, ControlPlaneServiceError> {
        let ca_key_pair = rcgen::KeyPair::from_pem(ca_key_pem).map_err(|e| {
            ControlPlaneServiceError::Internal(format!("failed to parse CA key: {}", e))
        })?;

        let ca_issuer = rcgen::Issuer::from_ca_cert_pem(ca_cert_pem, ca_key_pair).map_err(|e| {
            ControlPlaneServiceError::Internal(format!("failed to parse CA cert: {}", e))
        })?;

        Ok(Self {
            ca_issuer,
            ca_pem: ca_cert_pem.to_string(),
        })
    }
}

#[async_trait]
impl CertificateIssuer for CaBackedCertificateIssuer {
    async fn issue_node_certificate(
        &self,
        node_id: &NodeId,
    ) -> Result<IssuedCertificate, ControlPlaneServiceError> {
        use rcgen::string::Ia5String;
        use rcgen::{CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, SanType};

        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, node_id.as_str());

        // `CertificateParams::default()` leaves the validity unbounded in
        // practice (1975→4096), which makes the "short-lived certificate plus
        // denylist" revocation story untrue (#661 review H3). Issued node
        // certs are intentionally bounded: five minutes of clock skew on the
        // not-before side, 90 days of life. Agents rotate every 12 hours, so
        // this is a safety bound, not the normal renewal mechanism. Existing
        // certificates are unaffected.
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::minutes(5);
        params.not_after = now + time::Duration::days(90);

        let dns_name = Ia5String::try_from(node_id.as_str()).map_err(|e| {
            ControlPlaneServiceError::Internal(format!("invalid node_id for DNS: {}", e))
        })?;
        params.subject_alt_names.push(SanType::DnsName(dns_name));
        params.is_ca = IsCa::NoCa;
        let serial = chv_common::gen_short_id();
        params.serial_number = Some(rcgen::SerialNumber::from(serial.as_bytes().to_vec()));

        let key_pair =
            KeyPair::generate().map_err(|e| ControlPlaneServiceError::Internal(e.to_string()))?;

        let cert = params
            .signed_by(&key_pair, &self.ca_issuer)
            .map_err(|e| ControlPlaneServiceError::Internal(format!("signing failed: {}", e)))?;

        Ok(IssuedCertificate {
            certificate_pem: cert.pem().into_bytes(),
            private_key_pem: key_pair.serialize_pem().into_bytes(),
            ca_pem: self.ca_pem.as_bytes().to_vec(),
            serial,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chv_controlplane_store::{
        test_util::TestDb, CertificateRevocationRepository, RevokeCertificateInput,
    };
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Issuer stub that returns a scripted sequence of serials.
    struct SequenceIssuer {
        serials: Mutex<VecDeque<String>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl SequenceIssuer {
        fn new(serials: &[&str]) -> Self {
            Self {
                serials: Mutex::new(serials.iter().map(|s| (*s).to_string()).collect()),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl CertificateIssuer for SequenceIssuer {
        async fn issue_node_certificate(
            &self,
            _node_id: &NodeId,
        ) -> Result<IssuedCertificate, ControlPlaneServiceError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let serial = self
                .serials
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| "00000000".into());
            Ok(IssuedCertificate {
                certificate_pem: vec![],
                private_key_pem: vec![],
                ca_pem: vec![],
                serial,
            })
        }
    }

    async fn revoked_repo(serials: &[&str]) -> (TestDb, CertificateRevocationRepository) {
        let db = TestDb::new().await;
        let repo = CertificateRevocationRepository::new(db.pool.clone());
        for serial in serials {
            repo.revoke(&RevokeCertificateInput {
                certificate_serial: serial,
                node_id: "node-m4",
                reason: "test",
                revoked_by: None,
                revoked_at: None,
                expires_at: None,
            })
            .await
            .unwrap();
        }
        (db, repo)
    }

    #[tokio::test]
    async fn minted_serial_colliding_with_denylist_is_regenerated() {
        let (_db, repo) = revoked_repo(&["aaaaaaaa"]).await;
        let issuer = SequenceIssuer::new(&["aaaaaaaa", "bbbbbbbb"]);
        let node_id = NodeId::new("node-m4").unwrap();

        let issued = issue_unrevoked_node_certificate(&issuer, &node_id, Some(&repo))
            .await
            .expect("second serial is not revoked");

        assert_eq!(issued.serial, "bbbbbbbb");
        assert_eq!(
            issuer.calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "a revoked collision must be discarded and re-minted exactly once"
        );
    }

    #[tokio::test]
    async fn minted_serial_exhaustion_fails_closed() {
        let (_db, repo) = revoked_repo(&["aaaaaaaa"]).await;
        // Every attempt collides.
        let serials = vec!["aaaaaaaa"; MAX_CERTIFICATE_ISSUE_ATTEMPTS];
        let issuer = SequenceIssuer::new(&serials);
        let node_id = NodeId::new("node-m4").unwrap();

        let error = match issue_unrevoked_node_certificate(&issuer, &node_id, Some(&repo)).await {
            Ok(_) => panic!("must fail instead of issuing a revoked serial"),
            Err(error) => error,
        };

        assert!(matches!(error, ControlPlaneServiceError::Internal(_)));
        assert_eq!(
            issuer.calls.load(std::sync::atomic::Ordering::SeqCst),
            MAX_CERTIFICATE_ISSUE_ATTEMPTS
        );
    }

    #[tokio::test]
    async fn no_denylist_configured_accepts_first_serial() {
        let issuer = SequenceIssuer::new(&["cccccccc"]);
        let node_id = NodeId::new("node-m4").unwrap();

        let issued = issue_unrevoked_node_certificate(&issuer, &node_id, None)
            .await
            .expect("no denylist means no collision check");

        assert_eq!(issued.serial, "cccccccc");
        assert_eq!(issuer.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}

#[cfg(test)]
mod audit_coupling_tests {
    use super::*;
    use chv_controlplane_store::{test_util::TestDb, AuditLogInsert, AuditOutboxInsert};
    use std::sync::Arc;

    fn outbox_insert(outbox_id: &str, metadata: String) -> AuditOutboxInsert {
        AuditOutboxInsert {
            outbox_id: outbox_id.into(),
            audit_id: format!("audit-{outbox_id}"),
            payload: AuditLogInsert {
                occurred_at_ms: 1_000,
                actor_id: "node-a".into(),
                actor_username: "node-a".into(),
                actor_role: "node".into(),
                action: "RotateNodeCertificate".into(),
                method: "gRPC".into(),
                path: "chv.controlplane.node.v1.EnrollmentService/RotateNodeCertificate".into(),
                resource_kind: Some("node".into()),
                resource_id: Some("node-a".into()),
                outcome: "success".into(),
                status_code: 200,
                correlation_id: None,
                operation_id: None,
                source_ip: None,
                metadata,
            },
            created_at_ms: 1_000,
        }
    }

    struct StaticSerialIssuer;

    #[async_trait]
    impl CertificateIssuer for StaticSerialIssuer {
        async fn issue_node_certificate(
            &self,
            _node_id: &NodeId,
        ) -> Result<IssuedCertificate, ControlPlaneServiceError> {
            Ok(IssuedCertificate {
                certificate_pem: vec![],
                private_key_pem: vec![],
                ca_pem: vec![],
                serial: "serial-new".into(),
            })
        }
    }

    /// An adopted rotation whose audit payload exceeds the store's 16 KiB
    /// bound must fail closed: the transaction is rolled back, so neither
    /// the new serial nor an outbox row exists.
    #[tokio::test]
    async fn oversized_audit_payload_rolls_back_the_adopted_rotation() {
        let db = TestDb::new().await;
        let node_repo = NodeRepository::new(db.pool.clone());
        node_repo
            .upsert_node(&NodeUpsertInput {
                node_id: NodeId::new("node-a").unwrap(),
                hostname: "node-a".into(),
                display_name: "node-a".into(),
                certificate_serial: Some("old-serial".into()),
                agent_version: None,
                control_plane_version: None,
                enrolled_unix_ms: 1_000,
                last_seen_unix_ms: 1_000,
            })
            .await
            .expect("seed node");

        let service = EnrollmentServiceImplementation::new(
            node_repo.clone(),
            BootstrapTokenRepository::new(db.pool.clone()),
            Some(Arc::new(StaticSerialIssuer)),
            VtepRepository::new(db.pool.clone()),
        );

        // > the 16 KiB payload bound enforced by `enqueue_on_tx`.
        let oversized = outbox_insert("ob-oversized", "x".repeat(16 * 1024 + 1));
        let committed = std::sync::atomic::AtomicBool::new(false);
        let coupling = AdoptedAudit::new(&oversized, &committed);
        let error = service
            .rotate_node_certificate(
                proto::RotateNodeCertificateRequest {
                    meta: Some(proto::RequestMeta::default()),
                    node_id: "node-a".into(),
                },
                Some("old-serial".into()),
                false,
                Some(&coupling),
            )
            .await
            .expect_err("oversized evidence must fail the mutation closed");
        assert!(
            matches!(error, ControlPlaneServiceError::Store(_)),
            "expected a store validation error, got {error:?}"
        );

        assert_eq!(
            node_repo
                .get_certificate_serial(&NodeId::new("node-a").unwrap())
                .await
                .expect("read serial")
                .as_deref(),
            Some("old-serial"),
            "the certificate rotation must roll back"
        );
        let (outbox_rows,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM audit_outbox")
            .fetch_one(&db.pool)
            .await
            .expect("count outbox");
        assert_eq!(outbox_rows, 0, "no outbox row survives the rollback");
    }
}
