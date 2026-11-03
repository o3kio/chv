//! Hardening tests for the migration receiver's chv-owned TLS accept loop
//! (post-#479 follow-up): the loop is now responsible not just for
//! observability but for the lifetime of every accepted connection, so its
//! edge cases need their own pins.
//!
//! * **Handshake timeout**: an accepted connection that never sends a
//!   ClientHello must be closed at the handshake timeout instead of
//!   pinning one spawned handshake task plus its file descriptor forever
//!   (the slow-loris exposure scales linearly with connection count
//!   otherwise). The timeout must *not* fire during a legitimate
//!   handshake — the positive leg completes a full mTLS handshake against
//!   a receiver configured with the same, deliberately short timeout.
//! * **Shutdown race**: a handshake completing after the serving future
//!   was dropped (server shutdown raced a completing handshake) drops the
//!   finished stream and must be visible at debug level so operators can
//!   distinguish raced-shutdown from an mTLS rejection.
//! * **Channel-capacity churn**: more than 64 concurrent handshakes (the
//!   conn-channel capacity) plus mid-handshake disconnect churn must
//!   neither drop nor wedge the listener: every disconnect is observed
//!   (one warn line each, proving its handshake task ran to completion)
//!   and a valid peer still gets a gRPC-level response afterwards.
//!
//! These tests use [`serve_migration_tls_with_handshake_timeout`] — the
//! test seam for the fixed production constant — with a shortened duration
//! instead of sleeping 30 s.
//!
//! Log capture follows the house pattern from the #365/#366/#367
//! observability work (see `migration_mtls.rs` and `chv-webui-bff`'s
//! tracing-emission-contract tests): a fmt layer over a shared,
//! Mutex-guarded byte buffer installed once per process. This file is its
//! own test binary, so its DEBUG-level filter (needed for the shutdown
//! leg) cannot pollute the warn-level contract tests in `migration_mtls.rs`.

use chv_stord_api::chv_stord_api::storage_migration_service_client::StorageMigrationServiceClient;
use chv_stord_api::chv_stord_api::MigrationMessage;
use chv_stord_backends::LocalFileBackend;
use chv_stord_core::migration::service::StorageMigrationServiceImpl;
use chv_stord_core::migration::tls_config::load_migration_server_tls;
use chv_stord_core::server::serve_migration_tls_with_handshake_timeout;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};

/// Install the rustls ring crypto provider (same as `cmd/chv-stord`
/// startup). Idempotent: a second call fails harmlessly once a provider is
/// installed.
fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

// ---------------------------------------------------------------------------
// Test PKI (same shape as `migration_mtls.rs`; each integration-test
// binary in this crate carries its own copy per the house pattern).
// ---------------------------------------------------------------------------

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

/// Bind an ephemeral loopback port and serve the production migration
/// receiver on it with the given material and handshake timeout; returns
/// the bound address and the serving task's handle (kept so the
/// shutdown-race test can abort the serving future on demand).
async fn spawn_receiver(
    dir: &std::path::Path,
    server_cert: &[u8],
    server_key: &[u8],
    ca_pem: &[u8],
    handshake_timeout: Duration,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
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

    let backend = Arc::new(LocalFileBackend::new(dir.to_path_buf()));
    let service = StorageMigrationServiceImpl::new(
        backend,
        dir.to_path_buf(),
        chv_stord_core::migration::auth::MigrationReceiverAuthorization::DenyAll,
        chv_stord_core::migration::auth::MigrationLimits::default(),
    );
    let handle = tokio::spawn(async move {
        if let Err(e) =
            serve_migration_tls_with_handshake_timeout(listener, tls, service, handshake_timeout)
                .await
        {
            panic!("migration TLS listener failed: {e:?}");
        }
    });
    (addr, handle)
}

// ---------------------------------------------------------------------------
// Raw rustls client (drives the handshake without any gRPC machinery, so
// the tests can hold a connection mid-handshake or complete it fully).
// ---------------------------------------------------------------------------

fn raw_client_connector(
    root_pem: &[u8],
    client_cert: &[u8],
    client_key: &[u8],
) -> tokio_rustls::TlsConnector {
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(root_pem) {
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
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

async fn raw_handshake(
    connector: &tokio_rustls::TlsConnector,
    addr: SocketAddr,
) -> std::io::Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>> {
    let tcp = tokio::net::TcpStream::connect(addr).await?;
    let server_name =
        tokio_rustls::rustls::pki_types::ServerName::try_from("localhost".to_owned()).unwrap();
    connector.connect(server_name, tcp).await
}

/// Drive one migration RPC whose request stream is closed immediately —
/// the gRPC-level proof that a validly authenticated peer is served (the
/// service answers `InvalidArgument`, "stream closed without sending a
/// message").
async fn grpc_probe(addr: SocketAddr, ca_pem: &[u8], client_cert: &[u8], client_key: &[u8]) {
    let tls = ClientTlsConfig::new()
        .domain_name("localhost")
        .ca_certificate(Certificate::from_pem(ca_pem))
        .identity(Identity::from_pem(client_cert, client_key));
    let endpoint = Endpoint::from_shared(format!("https://{addr}"))
        .unwrap()
        .tls_config(tls)
        .unwrap()
        .connect_timeout(Duration::from_secs(10));
    let channel = endpoint
        .connect()
        .await
        .expect("valid client identity must complete mTLS");
    let mut client = StorageMigrationServiceClient::new(channel);
    let (tx, rx) = mpsc::channel::<MigrationMessage>(1);
    drop(tx); // end the request stream without sending InitMigration
    match client.stream_blocks(ReceiverStream::new(rx)).await {
        Err(status) => {
            assert_eq!(
                status.code(),
                tonic::Code::InvalidArgument,
                "expected the empty-stream InvalidArgument, got: {}",
                status.message()
            );
            assert!(
                status.message().contains("stream closed"),
                "unexpected message: {}",
                status.message()
            );
        }
        Ok(_) => panic!("empty request stream must not succeed"),
    }
}

// ---------------------------------------------------------------------------
// Log capture (house pattern; DEBUG level because the shutdown-race leg
// asserts a debug line — this binary is separate from `migration_mtls.rs`
// precisely so the warn-level contract there keeps its quiet buffer).
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
        // DEBUG and above: the shutdown-race contract under test is a
        // debug line. The extra traffic (tonic/hyper/rustls at debug) is
        // harmless — assertions needle-match and are baseline-scoped.
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(MakeBuf(buf.clone()))
            .with_target(false)
            .with_ansi(false)
            .with_filter(LevelFilter::DEBUG);
        let _ = tracing_subscriber::Registry::default()
            .with(layer)
            .try_init();
    });
    buf
}

/// Serializes the tests in this binary: all of them drive handshake
/// outcomes against their own receivers, and the churn test counts warn
/// lines in the one process-global capture buffer.
static ACCEPT_LOOP_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Poll the captured log buffer until every needle appears in the bytes
/// appended after `baseline` (handshake logging happens on the receiver's
/// spawned tasks, i.e. asynchronously w.r.t. the client probe).
async fn wait_for_capture(
    buf: &Arc<std::sync::Mutex<Vec<u8>>>,
    baseline: usize,
    needles: &[&str],
) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let captured = {
            let guard = buf.lock().unwrap();
            String::from_utf8(guard[baseline..].to_vec()).expect("utf8")
        };
        if needles.iter().all(|n| captured.contains(n)) {
            return captured;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for log line containing {needles:?}; captured:\n{captured}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Poll the captured log buffer until `needle` appears at least `want`
/// times in the bytes appended after `baseline`, then return the exact
/// count (proving every expected handshake task ran to completion — a
/// wedged task would leave the count short).
async fn wait_for_capture_count(
    buf: &Arc<std::sync::Mutex<Vec<u8>>>,
    baseline: usize,
    needle: &str,
    want: usize,
) -> usize {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let count = {
            let guard = buf.lock().unwrap();
            let slice = &guard[baseline..];
            bytecount_count(slice, needle)
        };
        if count >= want {
            return count;
        }
        if Instant::now() >= deadline {
            let captured = {
                let guard = buf.lock().unwrap();
                String::from_utf8(guard[baseline..].to_vec()).expect("utf8")
            };
            panic!(
                "timed out waiting for {want} occurrences of {needle:?}, saw {count}; \
                 captured:\n{captured}"
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn bytecount_count(haystack: &[u8], needle: &str) -> usize {
    let haystack = std::str::from_utf8(haystack).expect("utf8");
    haystack.matches(needle).count()
}

/// The log lines (whole lines) containing `needle` in the captured slice.
fn captured_lines_containing<'a>(captured: &'a str, needle: &str) -> Vec<&'a str> {
    captured.lines().filter(|l| l.contains(needle)).collect()
}

// ---------------------------------------------------------------------------
// Leg 1 — handshake timeout.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn migration_handshake_timeout_closes_silent_connections() {
    install_crypto_provider();
    let _serialized = ACCEPT_LOOP_TESTS.lock().await;
    let buf = install_tracing_subscriber();

    let dir = tempfile::tempdir().unwrap();
    let ca = test_ca("chv-migration-timeout-ca");
    let (server_cert, server_key) = issue_leaf(&ca, "stord-receiver", true);
    let (client_cert, client_key) = issue_leaf(&ca, "stord-peer", false);

    // Short timeout through the test seam: the production constant is 30 s
    // (see MIGRATION_HANDSHAKE_TIMEOUT); the behavior under test — an
    // accepted-then-silent connection is closed at the deadline — is
    // identical, only faster to reach.
    let timeout = Duration::from_millis(250);
    let (addr, _serve) =
        spawn_receiver(dir.path(), &server_cert, &server_key, &ca.cert_pem, timeout).await;

    // Positive control: a legitimate full mTLS handshake completes well
    // within the same timeout (the timeout only guards silence — it must
    // never fire on a real handshake; loopback completes in single-digit
    // milliseconds).
    let connector = raw_client_connector(&ca.cert_pem, &client_cert, &client_key);
    let tls = raw_handshake(&connector, addr)
        .await
        .expect("legitimate handshake must complete within the timeout");
    drop(tls);

    // The silent connection: accepted, then never sends a ClientHello.
    // Without the timeout it would pin one spawned task + fd forever; with
    // it, the receiver must close it — at the deadline, not immediately.
    let baseline = buf.lock().unwrap().len();
    let mut silent = tokio::net::TcpStream::connect(addr).await.unwrap();
    let started = Instant::now();
    let mut scratch = [0u8; 16];
    let read = tokio::time::timeout(Duration::from_secs(5), silent.read(&mut scratch))
        .await
        .expect("the silent connection must be closed by the receiver, not hang");
    let elapsed = started.elapsed();
    match read {
        // Clean close (FIN) or reset — both prove the receiver dropped it.
        Ok(0) | Err(_) => {}
        Ok(n) => panic!("expected connection close, got {n} bytes of data"),
    }
    assert!(
        elapsed >= timeout,
        "the connection must be closed at the handshake timeout ({timeout:?}), \
         not before; closed after {elapsed:?}"
    );

    // The timeout is observable: info level, peer address, nothing more.
    let captured = wait_for_capture(
        &buf,
        baseline,
        &["migration TLS handshake timed out", "INFO", "127.0.0.1"],
    )
    .await;
    let lines = captured_lines_containing(&captured, "migration TLS handshake timed out");
    assert_eq!(
        lines.len(),
        1,
        "exactly one timeout line expected:\n{captured}"
    );
    let line = lines[0];
    assert!(
        line.contains("peer=127.0.0.1"),
        "timeout line must carry the peer address:\n{line}"
    );
    assert!(
        !line.contains("reason="),
        "timeout line must carry the peer address and no more:\n{line}"
    );
    assert!(
        !line.contains("BEGIN CERTIFICATE") && !line.contains("BEGIN PRIVATE"),
        "timeout line must not contain PEM material:\n{line}"
    );
    // The timeout is housekeeping, not an mTLS rejection: the rejection
    // warn line must not appear for a peer that never handshook.
    assert!(
        !captured.contains("rejected migration TLS handshake"),
        "a timed-out (never-started) handshake must not be logged as a rejection:\n{captured}"
    );
}

// ---------------------------------------------------------------------------
// Leg 2 — a handshake completing after server shutdown (raced drop of the
// serving future) is visible at debug level.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn migration_handshake_completion_after_shutdown_is_logged() {
    install_crypto_provider();
    let _serialized = ACCEPT_LOOP_TESTS.lock().await;
    let buf = install_tracing_subscriber();

    let dir = tempfile::tempdir().unwrap();
    let ca = test_ca("chv-migration-shutdown-ca");
    let (server_cert, server_key) = issue_leaf(&ca, "stord-receiver", true);
    let (client_cert, client_key) = issue_leaf(&ca, "stord-peer", false);

    let (addr, serve) = spawn_receiver(
        dir.path(),
        &server_cert,
        &server_key,
        &ca.cert_pem,
        // Any timeout works (the handshake here completes in milliseconds);
        // the production default for scale.
        Duration::from_secs(30),
    )
    .await;

    // Open the TCP connection and let the accept loop pick it up, so the
    // per-connection handshake task exists before the serving future goes
    // away. (The handshake tasks are spawned independently of the serving
    // future; only the accept loop and the channel die with it.)
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Shut the server down: aborting the serving future drops the accept
    // loop (AbortOnDrop) and the channel's receiver half. Awaiting the
    // handle guarantees the drop completed before the handshake proceeds.
    serve.abort();
    let _ = serve.await;

    // The already-accepted connection still completes its handshake (the
    // handshake task survived the shutdown) — and the completed stream has
    // nowhere to go: the channel is closed. That drop must be visible at
    // debug level so an operator can tell raced-shutdown apart from an
    // mTLS rejection.
    let baseline = buf.lock().unwrap().len();
    let connector = raw_client_connector(&ca.cert_pem, &client_cert, &client_key);
    let server_name =
        tokio_rustls::rustls::pki_types::ServerName::try_from("localhost".to_owned()).unwrap();
    let tls = connector
        .connect(server_name, tcp)
        .await
        .expect("the already-accepted handshake must still complete");
    drop(tls);

    let captured = wait_for_capture(
        &buf,
        baseline,
        &[
            "migration TLS handshake completed after server shutdown",
            "DEBUG",
        ],
    )
    .await;
    let lines = captured_lines_containing(
        &captured,
        "migration TLS handshake completed after server shutdown",
    );
    assert_eq!(
        lines.len(),
        1,
        "exactly one shutdown-drop line expected:\n{captured}"
    );
    let line = lines[0];
    assert!(
        line.contains("peer=127.0.0.1"),
        "shutdown-drop line must carry the peer address:\n{line}"
    );
    assert!(
        !line.contains("reason="),
        "shutdown-drop line must carry the peer address and no more:\n{line}"
    );
    assert!(
        !line.contains("BEGIN CERTIFICATE") && !line.contains("BEGIN PRIVATE"),
        "shutdown-drop line must not contain PEM material:\n{line}"
    );
    // Distinguishability: this was a successful handshake dropped by a
    // raced shutdown, not an mTLS rejection.
    assert!(
        !captured.contains("rejected migration TLS handshake"),
        "a raced-shutdown drop must not be logged as a rejection:\n{captured}"
    );
}

// ---------------------------------------------------------------------------
// Leg 3 — channel-capacity and disconnect churn: more than 64 concurrent
// handshakes (the conn-channel capacity) plus mid-handshake disconnects
// neither drop nor wedge the listener.
// ---------------------------------------------------------------------------

/// Concurrency above the accept loop's channel capacity (64): more
/// simultaneous in-flight handshakes than the channel can buffer.
const CHURN_CONCURRENT: usize = 100;
/// Mid-handshake disconnects: connect, send a truncated ClientHello record
/// header, drop the connection.
const CHURN_DISCONNECTS: usize = 150;

#[tokio::test]
async fn migration_accept_loop_survives_handshake_churn() {
    install_crypto_provider();
    let _serialized = ACCEPT_LOOP_TESTS.lock().await;
    let buf = install_tracing_subscriber();

    let dir = tempfile::tempdir().unwrap();
    let ca = test_ca("chv-migration-churn-ca");
    let (server_cert, server_key) = issue_leaf(&ca, "stord-receiver", true);
    let (client_cert, client_key) = issue_leaf(&ca, "stord-peer", false);

    let (addr, _serve) = spawn_receiver(
        dir.path(),
        &server_cert,
        &server_key,
        &ca.cert_pem,
        // Production timeout: the churn legs here all fail or complete in
        // milliseconds, so the timeout must never fire.
        Duration::from_secs(30),
    )
    .await;

    let connector = raw_client_connector(&ca.cert_pem, &client_cert, &client_key);

    // Leg A — more than 64 concurrent handshakes: all must complete (no
    // drop under channel saturation; the cap-64 channel backpressures the
    // handshake tasks, it never loses one), and the connections stay open
    // while tonic drains the channel.
    let mut joins = Vec::with_capacity(CHURN_CONCURRENT);
    for _ in 0..CHURN_CONCURRENT {
        let connector = connector.clone();
        joins.push(tokio::spawn(async move {
            raw_handshake(&connector, addr).await
        }));
    }
    let mut held = Vec::with_capacity(CHURN_CONCURRENT);
    for join in joins {
        let tls = tokio::time::timeout(Duration::from_secs(15), join)
            .await
            .expect("concurrent handshakes must not wedge")
            .expect("handshake task must not panic")
            .expect("concurrent handshake must complete (no drop under backpressure)");
        held.push(tls);
    }
    // Hold them briefly (the channel is saturated beyond its capacity),
    // then release.
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(held);

    // Leg B — mid-handshake disconnect churn: each truncated-then-dropped
    // connection must run its handshake task to completion and be logged
    // as a rejection (exactly one warn line each — a wedged or lost task
    // would leave the count short).
    let baseline = buf.lock().unwrap().len();
    let mut joins = Vec::with_capacity(CHURN_DISCONNECTS);
    for _ in 0..CHURN_DISCONNECTS {
        joins.push(tokio::spawn(async move {
            let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
            // A TLS handshake record header claiming a body that never
            // arrives: the receiver commits to reading the record, then
            // sees the connection disappear mid-handshake.
            tcp.write_all(&[0x16, 0x03, 0x01, 0x10, 0x00])
                .await
                .unwrap();
            drop(tcp);
        }));
    }
    for join in joins {
        join.await.expect("disconnect task must not panic");
    }
    let count = wait_for_capture_count(
        &buf,
        baseline,
        "rejected migration TLS handshake",
        CHURN_DISCONNECTS,
    )
    .await;
    assert_eq!(
        count, CHURN_DISCONNECTS,
        "every mid-handshake disconnect must be observed exactly once"
    );

    // No drop, no wedge: after saturation + churn, a valid peer still
    // gets a gRPC-level response through the same listener.
    grpc_probe(addr, &ca.cert_pem, &client_cert, &client_key).await;

    // The timeout must never have fired during the churn (every peer
    // either completed or disconnected promptly).
    let captured = {
        let guard = buf.lock().unwrap();
        String::from_utf8(guard[baseline..].to_vec()).expect("utf8")
    };
    assert!(
        !captured.contains("migration TLS handshake timed out"),
        "churn peers all fail or complete promptly; no timeout expected:\n{captured}"
    );
}
