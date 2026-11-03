//! Loopback proof that the storage-migration receiver's TLS TCP listener
//! actually enforces mTLS (issue #390), plus the observability contract for
//! rejected handshakes in both directions (issue #402).
//!
//! This is the negative the listener exists for: a stord that accepts inbound
//! migrations must never accept a peer without a valid client certificate.
//! The test drives the *production* serving path
//! ([`chv_stord_core::server::serve_migration_tls`] with a real rustls
//! `WebPkiClientVerifier`) and asserts at the transport level that:
//!
//! (a) a client WITH a valid client identity (issued by the trusted CA)
//!     completes the handshake and gets a gRPC-level response;
//! (b) a client WITHOUT any client certificate is rejected;
//! (c) a client with an identity from an UNTRUSTED CA is rejected.
//!
//! Rejections must surface as TLS errors (fatal alert), not as gRPC statuses.
//!
//! Issue #402 adds two observability legs on top of the same fail-closed
//! behavior:
//!
//! * sender-side, the TLS-layer rejection *causes* (wrong CA / wrong server
//!   name / expired server certificate) must be distinguishable in the
//!   sender's surfaced failure message and the migration task's
//!   `error_message` — previously all three collapsed to "transport error";
//! * receiver-side, a peer rejected at the TLS handshake must produce a
//!   warn-level log line (peer address + alert/reason, content-free) on the
//!   destination stord — previously nothing was logged at all. This covers
//!   both directions of the rejection: server-initiated (no client
//!   certificate, untrusted client CA) and sender-initiated (the sender
//!   aborts mid-handshake with a fatal alert when its root store does not
//!   trust the server certificate).

use chv_stord_api::chv_stord_api::storage_migration_service_client::StorageMigrationServiceClient;
use chv_stord_api::chv_stord_api::MigrationMessage;
use chv_stord_backends::LocalFileBackend;
use chv_stord_core::migration::sender::{MigrationSender, MigrationTlsConfig};
use chv_stord_core::migration::service::StorageMigrationServiceImpl;
use chv_stord_core::migration::task::MigrationTask;
use chv_stord_core::migration::tls_config::load_migration_server_tls;
use chv_stord_core::server::serve_migration_tls;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

/// Install the rustls ring crypto provider (same as `cmd/chv-stord` startup).
/// Idempotent: a second call fails harmlessly once a provider is installed.
fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A test CA that can issue leaf certificates.
struct TestCa {
    params: rcgen::CertificateParams,
    key: rcgen::KeyPair,
    cert_pem: Vec<u8>,
}

fn test_ca(cn: &str) -> TestCa {
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::default();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, cn);
    let cert = params.self_signed(&key).unwrap();
    TestCa {
        params,
        key,
        cert_pem: cert.pem().into_bytes(),
    }
}

/// Issue a leaf certificate signed by `ca`. Server leaves carry the
/// `localhost`/127.0.0.1 identities and serverAuth EKU; client leaves carry
/// clientAuth EKU.
fn issue_leaf(ca: &TestCa, cn: &str, server: bool) -> (Vec<u8>, Vec<u8>) {
    use rcgen::string::Ia5String;

    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::default();
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, cn);
    params.is_ca = rcgen::IsCa::NoCa;
    params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![if server {
        rcgen::ExtendedKeyUsagePurpose::ServerAuth
    } else {
        rcgen::ExtendedKeyUsagePurpose::ClientAuth
    }];
    if server {
        params.subject_alt_names.push(rcgen::SanType::DnsName(
            Ia5String::try_from("localhost").unwrap(),
        ));
        params.subject_alt_names.push(rcgen::SanType::IpAddress(
            std::net::Ipv4Addr::LOCALHOST.into(),
        ));
    }
    let issuer = rcgen::Issuer::from_params(&ca.params, &ca.key);
    let cert = params.signed_by(&key, &issuer).unwrap();
    (cert.pem().into_bytes(), key.serialize_pem().into_bytes())
}

fn tmp_file(bytes: &[u8]) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut f, bytes).unwrap();
    f
}

/// Issue a *server* leaf signed by `ca` whose validity window is entirely
/// in the past — for the sender-side expired-certificate rejection leg.
fn issue_expired_server_leaf(ca: &TestCa, cn: &str) -> (Vec<u8>, Vec<u8>) {
    use rcgen::string::Ia5String;

    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::default();
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, cn);
    params.is_ca = rcgen::IsCa::NoCa;
    params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    params.subject_alt_names.push(rcgen::SanType::DnsName(
        Ia5String::try_from("localhost").unwrap(),
    ));
    params.subject_alt_names.push(rcgen::SanType::IpAddress(
        std::net::Ipv4Addr::LOCALHOST.into(),
    ));
    params.not_before = rcgen::date_time_ymd(2015, 1, 1);
    params.not_after = rcgen::date_time_ymd(2020, 1, 1);
    let issuer = rcgen::Issuer::from_params(&ca.params, &ca.key);
    let cert = params.signed_by(&key, &issuer).unwrap();
    (cert.pem().into_bytes(), key.serialize_pem().into_bytes())
}

/// Bind an ephemeral loopback port and serve the production migration
/// receiver on it with the given material; returns the bound address.
/// Mirrors the wiring of `migration_tls_listener_requires_client_certificates`.
async fn spawn_receiver(
    dir: &std::path::Path,
    server_cert: &[u8],
    server_key: &[u8],
    ca_pem: &[u8],
) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let cert_file = tmp_file(server_cert);
    let key_file = tmp_file(server_key);
    let ca_file = tmp_file(ca_pem);
    let tls = load_migration_server_tls(
        true,
        Some(&addr.to_string()),
        Some(cert_file.path()),
        Some(key_file.path()),
        Some(ca_file.path()),
    )
    .expect("full receiver set must load")
    .expect("configured receiver must produce server TLS material");
    assert_eq!(tls.listen_addr, addr);

    let backend = Arc::new(LocalFileBackend::new(dir.to_path_buf()));
    let service = StorageMigrationServiceImpl::new(
        backend,
        dir.to_path_buf(),
        chv_stord_core::migration::auth::MigrationReceiverAuthorization::DenyAll,
        chv_stord_core::migration::auth::MigrationLimits::default(),
    );
    tokio::spawn(async move {
        if let Err(e) = serve_migration_tls(listener, tls, service).await {
            panic!("migration TLS listener failed: {e:?}");
        }
    });
    addr
}

/// The observable outcome of one probe against the listener.
#[derive(Debug)]
enum Outcome {
    /// A gRPC-level response was received (proves the full mTLS + HTTP/2
    /// round trip works for this client identity).
    Grpc(tonic::Code, String),
    /// The server killed the connection below gRPC with a TLS fatal alert
    /// (the mTLS rejection this listener exists to enforce).
    TlsAlert(String),
}

fn error_chain(e: &dyn std::error::Error) -> String {
    let mut msg = e.to_string();
    let mut source = e.source();
    while let Some(err) = source {
        msg.push_str("; ");
        msg.push_str(&err.to_string());
        source = err.source();
    }
    msg
}

/// Connect to the listener and drive one migration RPC whose request stream
/// is closed immediately (the service answers `InvalidArgument` — "stream
/// closed without sending a message" — which is the expected gRPC-level
/// proof for a validly authenticated peer). A peer rejected at the TLS layer
/// instead sees the connection die with a rustls fatal alert.
async fn probe(
    addr: SocketAddr,
    server_ca_pem: &[u8],
    client_identity: Option<(&[u8], &[u8])>,
) -> Outcome {
    let mut tls = ClientTlsConfig::new()
        .domain_name("localhost")
        .ca_certificate(Certificate::from_pem(server_ca_pem));
    if let Some((cert, key)) = client_identity {
        tls = tls.identity(Identity::from_pem(cert, key));
    }
    let endpoint = Endpoint::from_shared(format!("https://{addr}"))
        .unwrap()
        .tls_config(tls)
        .unwrap()
        .connect_timeout(Duration::from_secs(10));

    let channel: Channel = match endpoint.connect().await {
        Ok(channel) => channel,
        Err(e) => {
            let chain = error_chain(&e);
            if chain.contains("fatal alert") {
                return Outcome::TlsAlert(chain);
            }
            // The listener is up (the positive probe ran first); a non-TLS
            // connect failure means the test infrastructure broke.
            panic!("unexpected non-TLS connect failure: {chain}");
        }
    };

    let mut client = StorageMigrationServiceClient::new(channel);
    let (tx, rx) = mpsc::channel::<MigrationMessage>(1);
    drop(tx); // end the request stream without sending InitMigration
    match client.stream_blocks(ReceiverStream::new(rx)).await {
        Ok(_) => Outcome::Grpc(tonic::Code::Ok, "unexpected success".into()),
        Err(status) => {
            // Transport-level failures surface as a tonic `Status` (code
            // Unknown, "transport error") whose source chain carries the
            // real cause, including the rustls fatal alert sent by the
            // server when client-certificate verification fails.
            let mut chain = format!("code={:?} message={:?}", status.code(), status.message());
            let mut source = std::error::Error::source(&status);
            while let Some(err) = source {
                chain.push_str("; ");
                chain.push_str(&err.to_string());
                source = err.source();
            }
            if chain.contains("fatal alert") {
                Outcome::TlsAlert(chain)
            } else {
                Outcome::Grpc(status.code(), status.message().to_string())
            }
        }
    }
}

/// Serializes the two receiver-side tests in this binary: both drive
/// handshake rejections, and the log-capture test's negative assertions
/// (which reason a rejection must NOT carry) must not observe the other
/// test's rejections arriving in the same process-global capture buffer.
static RECEIVER_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn migration_tls_listener_requires_client_certificates() {
    install_crypto_provider();
    let _serialized = RECEIVER_TESTS.lock().await;

    let dir = tempfile::tempdir().unwrap();

    // Trusted CA: issues the server leaf and the legitimate client identity.
    let ca = test_ca("chv-migration-test-ca");
    let (server_cert, server_key) = issue_leaf(&ca, "stord-receiver", true);
    let (client_cert, client_key) = issue_leaf(&ca, "stord-peer", false);

    // Rogue CA: issues a client identity the receiver must not trust.
    let rogue_ca = test_ca("chv-migration-rogue-ca");
    let (rogue_cert, rogue_key) = issue_leaf(&rogue_ca, "rogue-peer", false);

    // Bind an ephemeral loopback port, then load the validated material
    // through the real startup loader pointing at it.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let cert_file = tmp_file(&server_cert);
    let key_file = tmp_file(&server_key);
    let ca_file = tmp_file(&ca.cert_pem);
    let tls = load_migration_server_tls(
        true,
        Some(&addr.to_string()),
        Some(cert_file.path()),
        Some(key_file.path()),
        Some(ca_file.path()),
    )
    .expect("full receiver set must load")
    .expect("configured receiver must produce server TLS material");
    assert_eq!(tls.listen_addr, addr);

    // Serve with the production path: a real tonic server with
    // ServerTlsConfig (client_ca_root, no client_auth_optional).
    let backend = Arc::new(LocalFileBackend::new(dir.path().to_path_buf()));
    let service = StorageMigrationServiceImpl::new(
        backend,
        dir.path().to_path_buf(),
        chv_stord_core::migration::auth::MigrationReceiverAuthorization::DenyAll,
        chv_stord_core::migration::auth::MigrationLimits::default(),
    );
    tokio::spawn(async move {
        if let Err(e) = serve_migration_tls(listener, tls, service).await {
            panic!("migration TLS listener failed: {e:?}");
        }
    });

    // (a) A client with a valid client identity completes the mTLS handshake
    // and receives a gRPC-level response.
    match probe(addr, &ca.cert_pem, Some((&client_cert, &client_key))).await {
        Outcome::Grpc(tonic::Code::InvalidArgument, msg) => {
            assert!(
                msg.contains("stream closed"),
                "expected the empty-stream InvalidArgument, got: {msg}"
            );
        }
        other => panic!(
            "valid client identity must complete mTLS and get a gRPC response, got: {other:?}"
        ),
    }

    // (b) A client WITHOUT a client certificate is rejected at the TLS layer.
    // `CertificateRequired` is rustls's alert for a mandatory client-cert
    // verifier seeing an empty client Certificate message — this is the
    // exact behavior the listener must enforce.
    match probe(addr, &ca.cert_pem, None).await {
        Outcome::TlsAlert(chain) => {
            assert!(
                chain.contains("CertificateRequired"),
                "expected the CertificateRequired fatal alert, got: {chain}"
            );
        }
        other => {
            panic!("client without a certificate must be rejected at the TLS layer, got: {other:?}")
        }
    }

    // (c) A client with an identity from an untrusted CA is rejected at the
    // TLS layer (the server sends a fatal alert instead of a gRPC response).
    match probe(addr, &ca.cert_pem, Some((&rogue_cert, &rogue_key))).await {
        Outcome::TlsAlert(chain) => {
            assert!(
                !chain.contains("CertificateRequired"),
                "a presented (but untrusted) certificate must fail verification, not the \
                 missing-certificate path; got: {chain}"
            );
        }
        other => panic!(
            "client with an untrusted-CA identity must be rejected at the TLS layer, got: {other:?}"
        ),
    }
}

// ---------------------------------------------------------------------------
// Issue #402, sender half: TLS-layer rejection causes must be
// distinguishable in the surfaced failure message / task error_message.
// ---------------------------------------------------------------------------

/// Run one sender leg against `addr` with the given client TLS config and
/// return the surfaced status plus the final task state. The connect fails
/// before any backend I/O, so the volume never has to exist.
async fn sender_rejection(
    dir: &std::path::Path,
    addr: SocketAddr,
    tls: MigrationTlsConfig,
) -> (
    tonic::Status,
    chv_stord_core::migration::task::MigrationTaskState,
) {
    let (task, _pause_rx) = MigrationTask::new(
        "vol-402".to_string(),
        "handle-402".to_string(),
        format!("https://{addr}"),
    );
    let sender = MigrationSender::new(
        Arc::new(LocalFileBackend::new(dir.to_path_buf())),
        "vol-402".to_string(),
        "handle-402".to_string(),
    )
    .with_tls(tls)
    .with_task(task.clone());

    let status = sender
        .start_migration(format!("https://{addr}"))
        .await
        .expect_err("TLS-rejected migration must fail");
    let state = task.state.read().await.clone();
    (status, state)
}

/// The wrong-CA rejection fragment: rustls 0.23 names the cause
/// `UnknownIssuer` when the sender's CA bundle cannot validate the
/// destination's certificate.
const WRONG_CA_FRAGMENT: &str = "UnknownIssuer";
/// The wrong-server-name rejection fragment: rustls 0.23 names the cause
/// `NotValidForName` (plain variant) or renders "not valid for name ..."
/// (context variant) when the certificate does not cover `dest_domain`.
const WRONG_NAME_FRAGMENTS: [&str; 2] = ["NotValidForName", "not valid for name"];
/// The expired-certificate rejection fragment: rustls 0.23 names the cause
/// `Expired` (plain variant) or renders "certificate expired ..." (context
/// variant).
const EXPIRED_FRAGMENTS: [&str; 2] = ["Expired", "expired"];

#[tokio::test]
async fn migration_sender_tls_rejections_are_distinguishable() {
    install_crypto_provider();

    let dir = tempfile::tempdir().unwrap();

    // Trusted CA: issues the (valid) receiver identity and the sender's
    // client identity.
    let ca = test_ca("chv-migration-test-ca");
    let (server_cert, server_key) = issue_leaf(&ca, "stord-receiver", true);
    let (client_cert, client_key) = issue_leaf(&ca, "stord-peer", false);

    // A second receiver whose certificate is expired but otherwise valid
    // (correct CA, correct name) — the sender must reject it for expiry
    // only.
    let (expired_cert, expired_key) = issue_expired_server_leaf(&ca, "stord-receiver");

    let addr = spawn_receiver(dir.path(), &server_cert, &server_key, &ca.cert_pem).await;
    let expired_addr = spawn_receiver(dir.path(), &expired_cert, &expired_key, &ca.cert_pem).await;

    // ------------------------------------------------------------------
    // Leg 1 — wrong CA: the sender's CA bundle cannot validate the
    // destination certificate (unknown issuer).
    // ------------------------------------------------------------------
    let (wrong_ca_status, wrong_ca_state) = sender_rejection(
        dir.path(),
        addr,
        MigrationTlsConfig {
            cert_pem: client_cert.clone(),
            key_pem: client_key.clone(),
            ca_pem: test_ca("chv-migration-untrusted-ca").cert_pem,
            dest_domain: "localhost".to_string(),
        },
    )
    .await;

    let wrong_ca_msg = assert_rejection(
        &wrong_ca_status,
        &wrong_ca_state,
        &[WRONG_CA_FRAGMENT, "unknown issuer"],
        "wrong-CA rejection",
    );
    // Distinguishability: the other causes' fragments must NOT appear.
    assert!(
        !wrong_ca_msg.contains("NotValidForName") && !wrong_ca_msg.contains("not valid for name"),
        "wrong-CA message must not carry the wrong-name cause: {wrong_ca_msg}"
    );
    assert!(
        !wrong_ca_msg.contains("Expired") && !wrong_ca_msg.contains("expired"),
        "wrong-CA message must not carry the expiry cause: {wrong_ca_msg}"
    );

    // ------------------------------------------------------------------
    // Leg 2 — wrong server name: the destination certificate is valid but
    // does not cover the configured dest_domain.
    // ------------------------------------------------------------------
    let (wrong_name_status, wrong_name_state) = sender_rejection(
        dir.path(),
        addr,
        MigrationTlsConfig {
            cert_pem: client_cert.clone(),
            key_pem: client_key.clone(),
            ca_pem: ca.cert_pem.clone(),
            dest_domain: "wrong.example".to_string(),
        },
    )
    .await;

    let wrong_name_msg = assert_rejection(
        &wrong_name_status,
        &wrong_name_state,
        &WRONG_NAME_FRAGMENTS,
        "wrong-server-name rejection",
    );
    assert!(
        !wrong_name_msg.contains("UnknownIssuer"),
        "wrong-name message must not carry the wrong-CA cause: {wrong_name_msg}"
    );

    // ------------------------------------------------------------------
    // Leg 3 — expired destination certificate: the CA and name are right,
    // only the validity window is in the past.
    // ------------------------------------------------------------------
    let (expired_status, expired_state) = sender_rejection(
        dir.path(),
        expired_addr,
        MigrationTlsConfig {
            cert_pem: client_cert.clone(),
            key_pem: client_key.clone(),
            ca_pem: ca.cert_pem.clone(),
            dest_domain: "localhost".to_string(),
        },
    )
    .await;

    let expired_msg = assert_rejection(
        &expired_status,
        &expired_state,
        &EXPIRED_FRAGMENTS,
        "expired-certificate rejection",
    );
    assert!(
        !expired_msg.contains("UnknownIssuer"),
        "expired message must not carry the wrong-CA cause: {expired_msg}"
    );

    // The three surfaced messages are pairwise distinguishable.
    assert_ne!(wrong_ca_msg, wrong_name_msg);
    assert_ne!(wrong_ca_msg, expired_msg);
    assert_ne!(wrong_name_msg, expired_msg);
}

/// Assert one sender rejection leg: the status is Unavailable, the message
/// keeps the "failed to connect to peer with mTLS" anchor and contains one
/// of the distinguishing `fragments`, the task failed with the same cause
/// in `error_message`, and neither carries certificate material
/// (content-free property, issue #402).
fn assert_rejection(
    status: &tonic::Status,
    state: &chv_stord_core::migration::task::MigrationTaskState,
    fragments: &[&str],
    what: &str,
) -> String {
    assert_eq!(
        status.code(),
        tonic::Code::Unavailable,
        "{what} must surface as Unavailable, got {:?}",
        status.code()
    );
    let msg = status.message();
    assert!(
        msg.contains("failed to connect to peer with mTLS"),
        "{what} must keep the connect-failure anchor, got: {msg}"
    );
    assert!(
        fragments.iter().any(|f| msg.contains(f)),
        "{what} must name its cause (one of {fragments:?}), got: {msg}"
    );
    assert_eq!(
        state.phase,
        chv_stord_core::migration::task::MigrationPhase::Failed,
        "{what} must fail the task, got {:?}",
        state.phase
    );
    assert!(
        fragments.iter().any(|f| state.error_message.contains(f)),
        "{what} must reach the task error_message, got: {}",
        state.error_message
    );
    // Content-free: alert/reason names only, never certificate material.
    for text in [msg, state.error_message.as_str()] {
        assert!(
            !text.contains("BEGIN CERTIFICATE") && !text.contains("BEGIN PRIVATE"),
            "{what} surfaced text must not contain PEM material: {text}"
        );
    }
    msg.to_string()
}

// ---------------------------------------------------------------------------
// Issue #402, receiver half: a peer rejected at the TLS handshake must be
// visible in the destination stord's logs (warn, peer address + reason).
//
// Log capture follows the house pattern from the #365/#366/#367
// observability work (see chv-webui-bff's tracing-emission-contract
// tests): a fmt layer over a shared, Mutex-guarded byte buffer installed
// once per process.
// ---------------------------------------------------------------------------

static TRACE_BUF: OnceLock<Arc<std::sync::Mutex<Vec<u8>>>> = OnceLock::new();
static TRACE_INSTALLED: OnceLock<()> = OnceLock::new();

#[derive(Clone)]
struct MakeBuf(Arc<std::sync::Mutex<Vec<u8>>>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for MakeBuf {
    type Writer = BufWriter;
    fn make_writer(&'a self) -> Self::Writer {
        BufWriter(self.0.clone())
    }
}

struct BufWriter(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for BufWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn install_tracing_subscriber() -> Arc<std::sync::Mutex<Vec<u8>>> {
    let buf = TRACE_BUF
        .get_or_init(|| Arc::new(std::sync::Mutex::new(Vec::new())))
        .clone();
    TRACE_INSTALLED.get_or_init(|| {
        use tracing_subscriber::filter::LevelFilter;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        use tracing_subscriber::Layer;
        // WARN and above only: the test drives real tonic/hyper/h2 traffic
        // (TRACE-verbose), and the contract under test is a warn line.
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(MakeBuf(buf.clone()))
            .with_target(false)
            .with_ansi(false)
            .with_filter(LevelFilter::WARN);
        let _ = tracing_subscriber::Registry::default()
            .with(layer)
            .try_init();
    });
    buf
}

/// Poll the captured log buffer until every needle appears in the bytes
/// appended after `baseline` (handshake-rejection logging happens on the
/// receiver's accept task, i.e. asynchronously w.r.t. the client probe).
async fn wait_for_capture(
    buf: &Arc<std::sync::Mutex<Vec<u8>>>,
    baseline: usize,
    needles: &[&str],
) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let captured = {
            let guard = buf.lock().unwrap();
            String::from_utf8(guard[baseline..].to_vec()).expect("utf8")
        };
        if needles.iter().all(|n| captured.contains(n)) {
            return captured;
        }
        if std::time::Instant::now() >= deadline {
            panic!("timed out waiting for log line containing {needles:?}; captured:\n{captured}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The log lines (whole lines) containing `needle` in the captured slice
/// (the same line-bound helper `migration_accept_loop.rs` uses).
fn captured_lines_containing<'a>(captured: &'a str, needle: &str) -> Vec<&'a str> {
    captured.lines().filter(|l| l.contains(needle)).collect()
}

/// Drive a raw rustls client whose root store does NOT trust the server's
/// certificate (while presenting an otherwise valid client identity): the
/// client aborts the handshake with a fatal `UnknownCA` alert as soon as it
/// receives the server's certificate flight. This is the sender-initiated
/// mirror of the receiver's own rejections — the documented case where the
/// *sender* aborts mid-handshake (e.g. its `ca_cert_path` does not match
/// the destination's CA), which surfaces receiver-side as `received fatal
/// alert: UnknownCA` through the same warn path. A raw connector (rather
/// than the tonic `probe`) is used because the failure happens client-side
/// before any gRPC machinery runs.
///
/// Returns the client's local port so the caller can pin the receiver-side
/// log line to exactly this connection.
async fn probe_with_untrusting_root(
    addr: SocketAddr,
    untrusted_root_pem: &[u8],
    client_cert: &[u8],
    client_key: &[u8],
) -> u16 {
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(untrusted_root_pem) {
        roots.add(cert.unwrap()).unwrap();
    }
    let certs: Vec<_> = CertificateDer::pem_slice_iter(client_cert)
        .collect::<Result<_, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_slice(client_key).unwrap();

    let config = tokio_rustls::rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(certs, key)
        .unwrap();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));

    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let local_port = tcp.local_addr().unwrap().port();
    let server_name =
        tokio_rustls::rustls::pki_types::ServerName::try_from("localhost".to_owned()).unwrap();
    let err = connector
        .connect(server_name, tcp)
        .await
        .expect_err("a client with an untrusting root store must abort the handshake");
    // Sanity: the client aborted because it rejected the *server*
    // certificate (its own validation), not for an unrelated reason.
    let chain = error_chain(&err);
    assert!(
        chain.contains("UnknownIssuer") || chain.contains("invalid peer certificate"),
        "client must fail validating the server certificate, got: {chain}"
    );
    local_port
}

#[tokio::test]
async fn migration_tls_listener_logs_rejected_handshakes() {
    install_crypto_provider();
    let _serialized = RECEIVER_TESTS.lock().await;
    let buf = install_tracing_subscriber();

    let dir = tempfile::tempdir().unwrap();

    let ca = test_ca("chv-migration-logtest-ca");
    let (server_cert, server_key) = issue_leaf(&ca, "stord-receiver", true);
    let (client_cert, client_key) = issue_leaf(&ca, "stord-peer", false);
    let rogue_ca = test_ca("chv-migration-logtest-rogue-ca");
    let (rogue_cert, rogue_key) = issue_leaf(&rogue_ca, "rogue-peer", false);

    let addr = spawn_receiver(dir.path(), &server_cert, &server_key, &ca.cert_pem).await;

    // Positive control: the listener is up and valid peers still get a
    // gRPC-level response (the observability change must not have weakened
    // the serving path).
    match probe(addr, &ca.cert_pem, Some((&client_cert, &client_key))).await {
        Outcome::Grpc(tonic::Code::InvalidArgument, msg) => {
            assert!(msg.contains("stream closed"), "unexpected: {msg}");
        }
        other => panic!("valid client identity must complete mTLS, got: {other:?}"),
    }

    // (b) A client WITHOUT a client certificate: rejected at the TLS layer
    // and *visible* in the destination log — warn level, peer address, and
    // the missing-certificate reason.
    let baseline = buf.lock().unwrap().len();
    match probe(addr, &ca.cert_pem, None).await {
        Outcome::TlsAlert(_) => {}
        other => panic!("client without a certificate must be rejected, got: {other:?}"),
    }
    let no_cert = wait_for_capture(
        &buf,
        baseline,
        &[
            "rejected migration TLS handshake",
            "127.0.0.1",
            "WARN",
            // rustls 0.23's mandatory-verifier missing-certificate reason.
            "peer sent no certificates",
        ],
    )
    .await;
    // Distinguishability: the missing-certificate rejection must not carry
    // the untrusted-CA verification reason.
    assert!(
        !no_cert.contains("UnknownIssuer"),
        "no-certificate rejection must not carry the untrusted-CA reason:\n{no_cert}"
    );
    // Content-free: alert/reason names only, never certificate contents.
    assert!(
        !no_cert.contains("BEGIN CERTIFICATE") && !no_cert.contains("BEGIN PRIVATE"),
        "rejected-handshake log must not contain PEM material:\n{no_cert}"
    );

    // (c) A client with an identity from an untrusted CA: rejected and
    // visible, with a *different* reason (certificate verification, not
    // the missing-certificate path).
    let baseline = buf.lock().unwrap().len();
    match probe(addr, &ca.cert_pem, Some((&rogue_cert, &rogue_key))).await {
        Outcome::TlsAlert(_) => {}
        other => panic!("untrusted-CA client must be rejected, got: {other:?}"),
    }
    let rogue = wait_for_capture(
        &buf,
        baseline,
        &[
            "rejected migration TLS handshake",
            "127.0.0.1",
            "WARN",
            // rustls 0.23's untrusted-client-CA verification reason.
            "UnknownIssuer",
        ],
    )
    .await;
    // Distinguishability: the untrusted-CA rejection must not carry the
    // missing-certificate reason.
    assert!(
        !rogue.contains("peer sent no certificates"),
        "untrusted-CA rejection must not carry the no-certificate reason:\n{rogue}"
    );
    assert!(
        !rogue.contains("BEGIN CERTIFICATE") && !rogue.contains("BEGIN PRIVATE"),
        "rejected-handshake log must not contain PEM material:\n{rogue}"
    );

    // (d) A client whose root store does not trust the SERVER certificate:
    // the *sender* aborts mid-handshake with a fatal `UnknownCA` alert
    // (e.g. its `ca_cert_path` names a CA the destination does not use).
    // The receiver surfaces the peer-sent alert through the same warn path
    // as its own rejections — this pins the exact rustls 0.23 alert
    // rendering the #402 changelog documents (`received fatal alert:
    // UnknownCA`), which was previously asserted nowhere.
    let baseline = buf.lock().unwrap().len();
    let peer_port = probe_with_untrusting_root(
        addr,
        &test_ca("chv-migration-logtest-unrelated-ca").cert_pem,
        &client_cert,
        &client_key,
    )
    .await;
    let alert = wait_for_capture(
        &buf,
        baseline,
        &[
            "rejected migration TLS handshake",
            // Pin the line to exactly this connection (the sender-side
            // test drives the same class of client concurrently and its
            // receiver's warn lines land in the same process-global
            // buffer).
            format!("127.0.0.1:{peer_port}").as_str(),
            "WARN",
            // rustls 0.23 renders a peer-sent fatal UnknownCA alert on the
            // receiving side exactly like this.
            "received fatal alert: UnknownCA",
        ],
    )
    .await;
    // Line-bound: `wait_for_capture` needles match against the whole
    // captured slice, so in an adverse schedule a concurrent sender-test
    // line of the same class (same alert, different peer) could satisfy
    // the content needle on its own. Bind the alert content to the SAME
    // line that carries the pinned peer port — the style
    // `migration_accept_loop.rs` pins its timeout line with.
    let pinned = format!("127.0.0.1:{peer_port}");
    let lines = captured_lines_containing(&alert, &pinned);
    assert_eq!(
        lines.len(),
        1,
        "exactly one rejected-handshake line expected for peer {pinned}:\n{alert}"
    );
    let line = lines[0];
    assert!(
        line.contains("rejected migration TLS handshake") && line.contains("WARN"),
        "the pinned line must be the rejection warn line:\n{line}"
    );
    assert!(
        line.contains("received fatal alert: UnknownCA"),
        "the pinned peer's rejection must carry the sender-abort UnknownCA alert:\n{line}"
    );
    // Distinguishability: the sender-abort rejection carries none of the
    // receiver-initiated reasons.
    assert!(
        !alert.contains("peer sent no certificates"),
        "sender-abort rejection must not carry the no-certificate reason:\n{alert}"
    );
    assert!(
        !alert.contains("UnknownIssuer"),
        "sender-abort rejection must not carry the untrusted-client-CA reason:\n{alert}"
    );
    assert!(
        !alert.contains("BEGIN CERTIFICATE") && !alert.contains("BEGIN PRIVATE"),
        "rejected-handshake log must not contain PEM material:\n{alert}"
    );
}
