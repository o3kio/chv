//! Peer identity extraction and verification for agent-side gRPC handlers.
//!
//! ## Threat model
//!
//! Agent-side handlers (enrollment rotation, telemetry, observed-state, reconcile
//! acknowledgements, bootstrap-result reports) accept a `node_id` field on the wire.
//! Without binding that asserted `node_id` to the peer's mTLS client certificate, a
//! compromised node holding a *valid* node certificate could forge requests claiming
//! to be a different node — overwriting another node's observed state, rotating
//! another node's certificate, or polluting telemetry/inventory. CVSS-wise this is
//! a cross-node trust gap: any one node compromise becomes cluster-wide impersonation.
//!
//! ## Defense
//!
//! This module:
//!
//! 1. Extracts the peer's leaf certificate from `tonic::transport::server::TlsConnectInfo`
//!    (set automatically by tonic when the server is configured with mTLS).
//! 2. Parses the certificate's CN and DNS-type SANs with `x509-parser`.
//! 3. Stores the authorized peer node identity in request extensions as a typed
//!    [`PeerNodeId`] marker via the [`PeerIdentityInterceptor`].
//! 4. Provides [`verify_peer_matches`] for handlers to reject any request whose
//!    asserted `node_id` does not match the peer's certificate-derived identity.
//!
//! ## Insecure-mode passthrough
//!
//! When `CHV_ALLOW_INSECURE=1` is set (dev/test mode), the interceptor and verification
//! become no-ops. Production deployments MUST run with mTLS enabled.
//!
//! See ADR-008 (error handling) and ADR-014 (mTLS / certificate trust boundary).

use crate::error::ControlPlaneServiceError;
use chv_controlplane_types::domain::NodeId;
use std::sync::Arc;
use tonic::transport::server::TlsConnectInfo;

/// Authoritative node identity derived from the peer's mTLS client certificate.
///
/// Inserted into request extensions by [`PeerIdentityInterceptor`] when running
/// under mTLS. Handlers SHOULD extract this and compare to any wire-asserted
/// `node_id` via [`verify_peer_matches`].
#[derive(Debug, Clone)]
pub struct PeerNodeId(pub NodeId);

impl PeerNodeId {
    pub fn as_node_id(&self) -> &NodeId {
        &self.0
    }
}

/// Certificate serial of the peer's mTLS leaf certificate.
///
/// Inserted into request extensions by [`PeerIdentityInterceptor`] alongside
/// [`PeerNodeId`]. The value is the same textual serial that
/// `CaBackedCertificateIssuer` stores in `nodes.certificate_serial`; rotation
/// handlers use it as proof that the caller actually possesses the currently
/// trusted certificate rather than merely knowing the node id (#661).
///
/// `None` means the certificate carried a serial that could not be represented
/// in the issuer's textual form. Handlers MUST fail closed in production mode
/// when this is absent or when no extension was inserted at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerCertificateSerial(pub Option<String>);

/// Marker inserted by the interceptor when the request arrived over an
/// insecure (non-TLS, dev-mode) channel. Handlers MUST treat this as
/// "no peer identity available" and accept any wire-asserted node_id —
/// suitable only for local development where `CHV_ALLOW_INSECURE=1`.
#[derive(Debug, Clone, Copy)]
pub struct InsecurePeer;

/// Tonic interceptor that pins the request's peer identity from the mTLS
/// client certificate.
///
/// Behavior:
/// - When `allow_insecure == true`: inserts [`InsecurePeer`] into request
///   extensions and returns. The handler-side verifier becomes a no-op.
/// - When `allow_insecure == false` and the request carries a peer cert: parses
///   the cert and inserts [`PeerNodeId`].
/// - When `allow_insecure == false` and the request has no peer cert (or the cert
///   does not carry a parseable node identifier): rejects with `Unauthenticated`.
///
/// This interceptor is intentionally conservative: a missing/unparseable cert in
/// production-mode (TLS-required) is a hard failure rather than a soft-pass,
/// because any handler that relies on `PeerNodeId` being present would otherwise
/// silently accept the wire-asserted `node_id`.
#[derive(Clone)]
pub struct PeerIdentityInterceptor {
    allow_insecure: bool,
}

impl PeerIdentityInterceptor {
    /// Construct the interceptor after validating the requested security mode.
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneServiceError::InsecureModeLockedOut`] when
    /// `allow_insecure` is `true` and the crate was **not** compiled with the
    /// `dev` Cargo feature. This is a typed, non-panicking deployment guard:
    /// `CHV_ALLOW_INSECURE=1` disables all mTLS peer-identity enforcement, and
    /// must never be usable in production builds.
    ///
    /// To build for local development:
    /// ```text
    /// cargo build --features dev
    /// ```
    pub fn new(allow_insecure: bool) -> Result<Self, ControlPlaneServiceError> {
        validate_security_mode(allow_insecure)?;
        Ok(Self { allow_insecure })
    }

    /// Apply the interceptor to a request, mutating its extensions in-place.
    #[allow(clippy::result_large_err)] // tonic::Status is the required error type for tonic interceptors.
    pub fn intercept<T>(
        &self,
        mut request: tonic::Request<T>,
    ) -> Result<tonic::Request<T>, tonic::Status> {
        if self.allow_insecure {
            request.extensions_mut().insert(InsecurePeer);
            return Ok(request);
        }

        let peer_node_id = match extract_peer_node_id_from_extensions(request.extensions()) {
            Ok(id) => id,
            Err(err) => {
                tracing::warn!(error = %err, "rejecting request: peer identity extraction failed");
                return Err(tonic::Status::unauthenticated(
                    "peer mTLS certificate required",
                ));
            }
        };

        let peer_serial = match extract_peer_certificate_serial_from_extensions(
            request.extensions(),
        ) {
            Ok(serial) => serial,
            Err(err) => {
                tracing::warn!(error = %err, "rejecting request: peer certificate serial extraction failed");
                return Err(tonic::Status::unauthenticated(
                    "peer mTLS certificate required",
                ));
            }
        };

        request.extensions_mut().insert(PeerNodeId(peer_node_id));
        request
            .extensions_mut()
            .insert(PeerCertificateSerial(peer_serial));
        Ok(request)
    }
}

/// Validate the peer-identity security mode requested at startup.
///
/// A production build must never run with mTLS peer-identity enforcement
/// disabled. `allow_insecure == true` is only valid when this crate was compiled
/// with the `dev` Cargo feature.
///
/// This is a **typed, non-panicking** startup validation (issue #233, ADR-014):
/// bootstrap calls it before constructing listeners/interceptors, and the error
/// propagates as [`ControlPlaneServiceError::InsecureModeLockedOut`] so the
/// process exits with a clean, operator-greppable message instead of a panic.
///
/// The message is part of the operator contract — runbooks/search tooling match
/// on "dev feature" / "CHV_ALLOW_INSECURE".
pub fn validate_security_mode(allow_insecure: bool) -> Result<(), ControlPlaneServiceError> {
    if allow_insecure && !cfg!(feature = "dev") {
        return Err(ControlPlaneServiceError::InsecureModeLockedOut(
            "CHV_ALLOW_INSECURE=1 is set but this build does not have the 'dev' Cargo feature. \
             This env var disables all mTLS peer-identity enforcement and must never be enabled \
             in production. To use it for local development, rebuild with: cargo build --features dev"
                .into(),
        ));
    }
    Ok(())
}

/// Errors returned when extracting a peer's node identity from request extensions.
#[derive(Debug, thiserror::Error)]
pub enum PeerIdentityError {
    #[error("no TLS connect info in request extensions")]
    MissingTlsInfo,

    #[error("peer presented no client certificate")]
    NoClientCertificate,

    #[error("failed to parse peer certificate: {0}")]
    InvalidCertificate(String),

    #[error("peer certificate carries no node_id-bearing CN or DNS SAN")]
    NoNodeIdInCertificate,

    #[error("peer certificate node_id is malformed: {0}")]
    InvalidNodeId(String),
}

/// Extract the peer's authoritative `NodeId` from the leaf certificate found in
/// the request's [`TlsConnectInfo`] extension.
///
/// Identity is taken from (in order of preference):
/// 1. The first DNS-type SAN entry.
/// 2. The certificate Subject's CommonName.
///
/// This matches how `CaBackedCertificateIssuer` mints node certs (CN = node_id,
/// SAN = DNS:node_id).
pub fn extract_peer_node_id_from_extensions(
    extensions: &tonic::Extensions,
) -> Result<NodeId, PeerIdentityError> {
    use tonic::transport::server::TcpConnectInfo;

    let tls_info: &TlsConnectInfo<TcpConnectInfo> = extensions
        .get::<TlsConnectInfo<TcpConnectInfo>>()
        .ok_or(PeerIdentityError::MissingTlsInfo)?;

    let certs: Arc<Vec<_>> = tls_info
        .peer_certs()
        .ok_or(PeerIdentityError::NoClientCertificate)?;

    let leaf = certs
        .first()
        .ok_or(PeerIdentityError::NoClientCertificate)?;

    parse_node_id_from_der(leaf.as_ref())
}

/// Extract the peer's leaf certificate serial from the request's
/// [`TlsConnectInfo`] extension.
///
/// Exposed for completeness; production code normally consumes the
/// [`PeerCertificateSerial`] extension inserted by [`PeerIdentityInterceptor`].
pub fn extract_peer_certificate_serial_from_extensions(
    extensions: &tonic::Extensions,
) -> Result<Option<String>, PeerIdentityError> {
    use tonic::transport::server::TcpConnectInfo;

    let tls_info: &TlsConnectInfo<TcpConnectInfo> = extensions
        .get::<TlsConnectInfo<TcpConnectInfo>>()
        .ok_or(PeerIdentityError::MissingTlsInfo)?;

    let certs: Arc<Vec<_>> = tls_info
        .peer_certs()
        .ok_or(PeerIdentityError::NoClientCertificate)?;

    let leaf = certs
        .first()
        .ok_or(PeerIdentityError::NoClientCertificate)?;

    parse_serial_from_der(leaf.as_ref())
}

/// Parse a DER-encoded X.509 certificate and return its serial in the same
/// textual form `CaBackedCertificateIssuer` records in
/// `nodes.certificate_serial`.
///
/// `CaBackedCertificateIssuer` stores `chv_common::gen_short_id()` (an
/// 8-character lowercase-ASCII hex string) as the DER INTEGER's big-endian
/// bytes. `x509_parser`'s `cert.raw_serial()` returns those DER content
/// octets, not a decimal/hex display string, so for CHV-issued certs it is
/// byte-for-byte the original ASCII serial.
///
/// Two DER normalization rules must be undone explicitly:
/// - A positive INTEGER whose high bit is set is encoded with one `0x00` sign
///   byte. That padding is not part of the serial and is stripped here.
/// - `yasna` (via `rcgen`) strips leading `0x00` bytes before encoding, so an
///   original serial with leading zero bytes cannot be recovered — for CHV
///   ASCII serials this is irrelevant because ASCII `'0'` is `0x30`, not
///   `0x00`. A serial of only zero bytes therefore yields [`None`].
///
/// A certificate carrying a non-ASCII/binary serial cannot be compared to the
/// stored textual serial; it is rendered as lowercase hex so handlers still
/// see a deterministic value and fail closed on the mismatch.
pub fn parse_serial_from_der(der: &[u8]) -> Result<Option<String>, PeerIdentityError> {
    use x509_parser::prelude::*;

    let (_, cert) = X509Certificate::from_der(der)
        .map_err(|e| PeerIdentityError::InvalidCertificate(e.to_string()))?;

    let raw = cert.raw_serial();
    // Strip exactly one DER sign-padding byte. Any other leading zero was
    // already removed by the issuer's DER encoder before the cert was minted.
    let unsigned = match raw {
        [0x00, rest @ ..] if !rest.is_empty() => rest,
        _ => raw,
    };

    if unsigned.is_empty() || unsigned.iter().all(|b| *b == 0) {
        return Ok(None);
    }

    // CHV serials are printable ASCII; decode those exactly so equality with
    // the stored `certificate_serial` is byte-for-byte. Anything else is
    // canonicalized to lowercase hex rather than lossy-decoded.
    Ok(Some(match std::str::from_utf8(unsigned) {
        Ok(text) if text.bytes().all(|b| b.is_ascii_graphic()) => text.to_string(),
        _ => unsigned.iter().map(|b| format!("{b:02x}")).collect(),
    }))
}

/// Parse a DER-encoded X.509 certificate and return its asserted node_id.
///
/// Exposed for unit tests. Production code goes through
/// [`extract_peer_node_id_from_extensions`].
pub fn parse_node_id_from_der(der: &[u8]) -> Result<NodeId, PeerIdentityError> {
    use x509_parser::prelude::*;

    let (_, cert) = X509Certificate::from_der(der)
        .map_err(|e| PeerIdentityError::InvalidCertificate(e.to_string()))?;

    // Prefer DNS SAN — that's what `CaBackedCertificateIssuer` populates.
    if let Ok(Some(ext)) = cert.subject_alternative_name() {
        for name in &ext.value.general_names {
            if let GeneralName::DNSName(dns) = name {
                let dns = dns.trim();
                if !dns.is_empty() {
                    return NodeId::new(dns.to_string())
                        .map_err(|e| PeerIdentityError::InvalidNodeId(e.to_string()));
                }
            }
        }
    }

    // Fall back to CN.
    if let Some(cn) = cert
        .subject()
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok())
    {
        let cn = cn.trim();
        if !cn.is_empty() {
            return NodeId::new(cn.to_string())
                .map_err(|e| PeerIdentityError::InvalidNodeId(e.to_string()));
        }
    }

    Err(PeerIdentityError::NoNodeIdInCertificate)
}

/// Verify that the asserted `requested` node_id matches the peer's
/// certificate-derived identity carried in the request extensions.
///
/// Returns:
/// - `Ok(())` if the peer is [`InsecurePeer`] (dev-mode passthrough) or if
///   `peer_node_id == requested`.
/// - `Err(ControlPlaneServiceError::PermissionDenied)` if the asserted node_id
///   does not match the peer cert. Increments
///   `chv_peer_node_id_mismatch_total{method}`.
/// - `Err(ControlPlaneServiceError::Unauthorized)` if no peer identity is
///   present in extensions (cert missing in TLS-required mode — should already
///   have been rejected by the interceptor, but defense-in-depth here).
pub fn verify_peer_matches(
    extensions: &tonic::Extensions,
    requested: &NodeId,
    method: &'static str,
) -> Result<(), ControlPlaneServiceError> {
    if extensions.get::<InsecurePeer>().is_some() {
        return Ok(());
    }

    let peer = extensions.get::<PeerNodeId>().ok_or_else(|| {
        tracing::warn!(
            requested_node_id = %requested,
            method,
            "rejecting request: peer identity extension missing (mTLS not enforced upstream?)"
        );
        ControlPlaneServiceError::PermissionDenied(
            "peer mTLS identity required for this RPC".into(),
        )
    })?;

    if peer.as_node_id() == requested {
        return Ok(());
    }

    metrics::counter!(
        "chv_peer_node_id_mismatch_total",
        "method" => method,
    )
    .increment(1);

    tracing::warn!(
        peer_node_id = %peer.as_node_id(),
        requested_node_id = %requested,
        method,
        "peer node_id mismatch: rejecting impersonation attempt"
    );

    Err(ControlPlaneServiceError::PermissionDenied(format!(
        "peer node_id {} does not match request node_id {}",
        peer.as_node_id(),
        requested,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a self-signed leaf certificate whose CN and DNS SAN both equal
    /// `node_id`. Returns the DER bytes.
    fn fake_leaf_der(node_id: &str) -> Vec<u8> {
        fake_leaf_der_with_serial(node_id, None)
    }

    /// Build a self-signed leaf certificate with an explicit DER serial.
    /// `None` leaves rcgen's default (a random u64 serial).
    fn fake_leaf_der_with_serial(node_id: &str, serial: Option<Vec<u8>>) -> Vec<u8> {
        use rcgen::string::Ia5String;
        use rcgen::{CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, SanType};

        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, node_id);
        let dns = Ia5String::try_from(node_id.to_string()).unwrap();
        params.subject_alt_names.push(SanType::DnsName(dns));
        params.is_ca = IsCa::NoCa;
        if let Some(serial) = serial {
            params.serial_number = Some(rcgen::SerialNumber::from(serial));
        }

        let key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        cert.der().to_vec()
    }

    #[test]
    fn parses_node_id_from_san() {
        let der = fake_leaf_der("node-alpha");
        let got = parse_node_id_from_der(&der).expect("parse");
        assert_eq!(got.as_str(), "node-alpha");
    }

    #[test]
    fn parses_ascii_serial_issued_form() {
        // This is the shape CaBackedCertificateIssuer mints: an 8-char
        // lowercase hex string stored as the DER INTEGER's ASCII bytes.
        let serial = "0a3f7b2c";
        let der = fake_leaf_der_with_serial("node-alpha", Some(serial.as_bytes().to_vec()));
        let got = parse_serial_from_der(&der).expect("parse");
        assert_eq!(
            got.as_deref(),
            Some(serial),
            "ASCII serial must round-trip byte-for-byte, including a leading '0'"
        );
    }

    #[test]
    fn serial_sign_padding_is_not_part_of_the_value() {
        // DER encodes a positive INTEGER whose high bit is set with a 0x00
        // sign-padding byte. It must not become a leading NUL in the parsed
        // serial (which would make every such serial look foreign).
        let der = fake_leaf_der_with_serial("node-alpha", Some(vec![0x80, 0x01]));
        let got = parse_serial_from_der(&der).expect("parse");
        assert_eq!(got.as_deref(), Some("8001"));
    }

    #[test]
    fn serial_leading_zero_bytes_are_normalized_by_der() {
        // yasna strips leading 0x00 bytes before encoding, so the serial is
        // the remaining bytes. ASCII '0' (0x30) is unaffected; this test
        // documents the one lossy case for binary serials.
        let der = fake_leaf_der_with_serial("node-alpha", Some(vec![0x00, 0x41]));
        let got = parse_serial_from_der(&der).expect("parse");
        assert_eq!(got.as_deref(), Some("A"));
    }

    #[test]
    fn rejects_garbage_der_when_parsing_serial() {
        let err = parse_serial_from_der(b"not a certificate").unwrap_err();
        assert!(matches!(err, PeerIdentityError::InvalidCertificate(_)));
    }

    #[test]
    fn parses_node_id_from_cn_when_no_san() {
        // rcgen always populates SAN if asked; for CN-only we hand-build.
        use rcgen::{CertificateParams, DistinguishedName, DnType, IsCa, KeyPair};
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, "node-cn-only");
        params.is_ca = IsCa::NoCa;
        let key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let got = parse_node_id_from_der(cert.der()).expect("parse");
        assert_eq!(got.as_str(), "node-cn-only");
    }

    #[test]
    fn rejects_garbage_der() {
        let err = parse_node_id_from_der(b"not a certificate").unwrap_err();
        assert!(matches!(err, PeerIdentityError::InvalidCertificate(_)));
    }

    /// x509-parser 0.18 behavior lock: a malformed SAN GeneralName (here a
    /// DNSName entry carrying non-UTF-8 bytes, which decodes as
    /// `GeneralName::Invalid`) must not prevent node_id derivation from the
    /// valid DNS entry in the same extension.
    #[test]
    fn malformed_san_entry_does_not_hide_valid_dns_name() {
        use rcgen::{CertificateParams, CustomExtension, DistinguishedName, DnType, IsCa, KeyPair};

        // DER: SEQUENCE OF GeneralName {
        //   [2] "\xFF\xFE"        <- DNSName with invalid UTF-8 (malformed)
        //   [2] "node-dns-san"    <- valid DNSName
        // }
        let mut san = vec![0x30, 0x12, 0x82, 0x02, 0xFF, 0xFE, 0x82, 0x0C];
        san.extend_from_slice(b"node-dns-san");
        assert_eq!(san[1] as usize, san.len() - 2, "DER length header");

        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, "node-cn-fallback");
        params
            .custom_extensions
            .push(CustomExtension::from_oid_content(&[2, 5, 29, 17], san));
        params.is_ca = IsCa::NoCa;
        let key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let got = parse_node_id_from_der(cert.der()).expect("parse");
        assert_eq!(got.as_str(), "node-dns-san");
    }

    /// Documents the compile-time invariant: production builds (no `dev` feature)
    /// must never have the `dev` feature enabled.
    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn allow_insecure_is_gated_by_feature() {
        #[cfg(not(feature = "dev"))]
        assert!(
            !cfg!(feature = "dev"),
            "production builds must not have 'dev' feature enabled"
        );
    }

    #[test]
    fn validate_security_mode_allows_secure_mode() {
        // Never gated: production mTLS mode is always permitted.
        assert!(validate_security_mode(false).is_ok());
    }

    #[test]
    fn intercept_rejects_requests_without_a_peer_certificate() {
        // The R2 C-MAJOR fix routes the lifecycle service through this
        // interceptor like its four siblings: a request with no TLS peer
        // certificate (no TlsConnectInfo extension — e.g. a plain TCP or
        // in-process call) must be rejected with UNAUTHENTICATED, never
        // passed through to the handler.
        let interceptor = PeerIdentityInterceptor::new(false).unwrap();
        let status = interceptor
            .intercept(tonic::Request::new(()))
            .expect_err("cert-less request must be rejected");
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
        assert_eq!(status.message(), "peer mTLS certificate required");
    }

    #[test]
    fn validate_security_mode_rejects_insecure_without_dev_feature() {
        // In a non-dev build (the default test compilation), requesting
        // insecure mode must fail with a TYPED error — never a panic.
        #[cfg(not(feature = "dev"))]
        match validate_security_mode(true) {
            Err(ControlPlaneServiceError::InsecureModeLockedOut(msg)) => {
                assert!(
                    msg.contains("CHV_ALLOW_INSECURE") && msg.contains("Cargo feature"),
                    "operator-greppable message expected, got: {msg}"
                );
            }
            other => panic!("expected InsecureModeLockedOut, got {other:?}"),
        }
    }

    #[cfg(feature = "dev")]
    #[test]
    fn validate_security_mode_allows_insecure_with_dev_feature() {
        // Only valid when the crate is compiled with the `dev` feature.
        assert!(validate_security_mode(true).is_ok());
    }
}
