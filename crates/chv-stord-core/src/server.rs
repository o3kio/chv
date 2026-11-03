use crate::handlers::StorageServiceImpl;
use crate::migration::auth::{MigrationLimits, MigrationReceiverAuthorization};
use crate::migration::sender::MigrationTlsConfig;
use crate::migration::service::StorageMigrationServiceImpl;
use crate::migration::tls_config::MigrationServerTls;
use crate::migration::MAX_MIGRATION_MESSAGE_SIZE_BYTES;
use crate::session::SessionTable;
use crate::store::SessionStore;
use chv_errors::ChvError;
use chv_observability::Metrics;
use chv_stord_api::chv_stord_api::storage_migration_service_server::StorageMigrationServiceServer;
use chv_stord_api::chv_stord_api::storage_service_server::StorageServiceServer;
use chv_stord_backends::StorageBackend;
use nix::unistd::{chown, Group};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream, UnixListener};
use tokio::sync::mpsc;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::{server::WebPkiClientVerifier, RootCertStore, ServerConfig};
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;
use tokio_stream::wrappers::{ReceiverStream, UnixListenerStream};
use tonic::transport::Server;
use tracing::{debug, info, warn};

pub struct StorageServer<B: StorageBackend> {
    inner: StorageServiceImpl<B>,
    migration_service: StorageMigrationServiceImpl<B>,
    /// Validated material for the migration receiver mTLS TCP listener
    /// (issue #390). `None` = source-only stord: no TCP listener is opened.
    migration_server_tls: Option<MigrationServerTls>,
}

impl<B: StorageBackend> StorageServer<B> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        backend: B,
        runtime_dir: std::path::PathBuf,
        metrics: Metrics,
        backend_allowlist: Vec<String>,
        path_allowlist: Vec<std::path::PathBuf>,
        device_allowlist: Vec<String>,
        migration_dest_allowlist: Vec<String>,
        migration_tls: Option<MigrationTlsConfig>,
        migration_server_tls: Option<MigrationServerTls>,
        migration_authorization: MigrationReceiverAuthorization,
        migration_limits: MigrationLimits,
        store: Option<SessionStore>,
    ) -> Self {
        let backend = Arc::new(backend);
        let sessions = Arc::new(SessionTable::new());
        let mut inner = StorageServiceImpl::new(
            backend.clone(),
            sessions,
            Arc::new(metrics),
            runtime_dir.clone(),
            backend_allowlist,
            path_allowlist,
            device_allowlist,
            migration_dest_allowlist,
            migration_tls,
        );
        if let Some(store) = store {
            inner.set_store(store);
        }
        let migration_service = StorageMigrationServiceImpl::new(
            backend,
            runtime_dir,
            migration_authorization,
            migration_limits,
        );
        Self {
            inner,
            migration_service,
            migration_server_tls,
        }
    }

    pub async fn serve(self, socket_path: &Path, db_path: Option<&Path>) -> Result<(), ChvError> {
        // Hydrate sessions from SQLite if db_path provided
        if let Some(db) = db_path {
            let db = db.to_path_buf();
            match tokio::task::spawn_blocking(move || SessionStore::new(&db)).await {
                Ok(Ok(store)) => match store.list().await {
                    Ok(sessions) => {
                        let table = self.inner.sessions();
                        for s in sessions {
                            table.upsert(s);
                        }
                        info!(count = table.list().len(), "hydrated sessions from SQLite");
                    }
                    Err(e) => tracing::warn!(error = %e, "failed to list sessions from SQLite"),
                },
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "failed to open SQLite store; continuing with empty session table")
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to open SQLite store; continuing with empty session table")
                }
            }
        }

        if let Some(parent) = socket_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ChvError::Io {
                    path: parent.to_string_lossy().to_string(),
                    source: e,
                })?;
        }

        if socket_path.exists() {
            tokio::fs::remove_file(socket_path)
                .await
                .map_err(|e| ChvError::Io {
                    path: socket_path.to_string_lossy().to_string(),
                    source: e,
                })?;
        }

        let uds = UnixListener::bind(socket_path).map_err(|e| ChvError::Io {
            path: socket_path.to_string_lossy().to_string(),
            source: e,
        })?;

        tokio::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))
            .await
            .map_err(|e| ChvError::Io {
                path: socket_path.to_string_lossy().to_string(),
                source: e,
            })?;

        // Ensure the socket is owned by the chv-stord group for defense in depth.
        if let Ok(Some(group)) = Group::from_name("chv-stord") {
            let _ = chown(socket_path, None::<nix::unistd::Uid>, Some(group.gid));
        }

        let uds_stream = UnixListenerStream::new(uds);

        let (health_reporter, health_service) = tonic_health::server::health_reporter();
        health_reporter
            .set_serving::<StorageServiceServer<StorageServiceImpl<B>>>()
            .await;

        info!(socket = %socket_path.display(), "starting chv-stord server");

        // Migration receiver mTLS TCP listener (issue #390). Binding happens
        // here, before the UDS server starts, so a bind failure is a startup
        // error (fail-closed) rather than a silent missing listener.
        let tls_serve = match self.migration_server_tls {
            Some(tls) => {
                let listener =
                    TcpListener::bind(tls.listen_addr)
                        .await
                        .map_err(|e| ChvError::Io {
                            path: tls.listen_addr.to_string(),
                            source: e,
                        })?;
                info!(
                    addr = %tls.listen_addr,
                    "storage migration receiver listening on {} (mTLS, client auth required)",
                    tls.listen_addr
                );
                Some(serve_migration_tls(
                    listener,
                    tls,
                    self.migration_service.clone(),
                ))
            }
            None => None,
        };

        let uds_serve = Server::builder()
            .layer(chv_observability::GrpcMetricsLayer::new())
            .add_service(health_service)
            .add_service(StorageServiceServer::new(self.inner))
            .add_service(
                StorageMigrationServiceServer::new(self.migration_service)
                    .max_decoding_message_size(MAX_MIGRATION_MESSAGE_SIZE_BYTES),
            )
            .serve_with_incoming(uds_stream);

        match tls_serve {
            None => uds_serve.await.map_err(|e| ChvError::Internal {
                reason: format!("server error: {e}"),
            }),
            // Fail fast: if either listener dies the daemon reports the error
            // instead of limping along with half its serving surface. Log at
            // the death site so operators see which listener failed even if
            // the process exit path does not print the error.
            Some(tls_serve) => tokio::select! {
                result = uds_serve => result.map_err(|e| {
                    tracing::error!("stord UDS server failed: {e}");
                    ChvError::Internal {
                        reason: format!("server error: {e}"),
                    }
                }),
                result = tls_serve => result.inspect_err(|e| {
                    tracing::error!("stord migration TLS listener failed: {e}");
                }),
            },
        }
    }
}

/// Serve `StorageMigrationService` on an already-bound TCP listener with
/// mandatory client-certificate (mTLS) authentication.
///
/// Applies [`MIGRATION_HANDSHAKE_TIMEOUT`]; tests that need to exercise the
/// timeout behavior use [`serve_migration_tls_with_handshake_timeout`]
/// instead.
///
/// # What enforces mandatory client auth
///
/// The rustls `ServerConfig` is built by [`migration_server_rustls_config`]
/// with a `WebPkiClientVerifier` over `tls.client_ca_pem` and **without**
/// `allow_unauthenticated()` (mirroring exactly what tonic's
/// `ServerTlsConfig` builds from `client_ca_root(...)` without
/// `client_auth_optional(true)`), installed via
/// `ServerConfig::with_client_cert_verifier`, so the TLS handshake itself
/// fails unless the peer presents a certificate that chains to
/// `tls.client_ca_pem`. This is the same mechanism the control plane uses
/// (`cmd/chv-controlplane/src/bootstrap.rs`).
///
/// # Why the TLS handshake runs in our own accept loop
///
/// tonic performs the TLS handshake inside its hyper accept path and drops
/// transport-level failures — including mTLS rejections — with at most a
/// debug-level log line carrying no peer address, so a peer refused at the
/// handshake is invisible in destination logs (issue #402). Here each
/// accepted TCP connection completes its handshake *before* it is handed
/// to the tonic server: a rejected handshake never becomes a connection,
/// and the rejection is logged at warn level with the peer address and the
/// alert/reason name (`invalid peer certificate: ...`, `peer sent no
/// certificates`, ...) — content-free, never certificate contents or key
/// material.
///
/// This function is also the seam exercised by the loopback mTLS proof tests
/// (`tests/migration_mtls.rs`, `tests/migration_accept_loop.rs`), which
/// assert that clients without a valid identity are rejected at the TLS
/// layer *and* that the rejection is visible in destination logs.
pub async fn serve_migration_tls<B: StorageBackend>(
    listener: TcpListener,
    tls: MigrationServerTls,
    service: StorageMigrationServiceImpl<B>,
) -> Result<(), ChvError> {
    serve_migration_tls_with_handshake_timeout(listener, tls, service, MIGRATION_HANDSHAKE_TIMEOUT)
        .await
}

/// How long an accepted connection may take to complete its TLS handshake
/// before the migration receiver gives up on it and closes the connection
/// (post-#479 hardening of the accept loop chv now owns).
///
/// This only guards the ClientHello-never-arrives case: a handshake is a
/// handful of round trips over an already-established TCP connection (the
/// loopback proof tests complete in single-digit milliseconds), so 30 s is
/// orders of magnitude above any legitimate completion time. Without a
/// bound, an idle TCP connection pins one spawned handshake task plus its
/// file descriptor forever, and the exposure scales linearly with
/// connection count (slow-loris on the migration port). Deliberately not a
/// config knob: tonic's internal handshake path — which this loop replaced
/// — had no timeout either, so this is parity hardening, not a tunable;
/// pass an explicit duration via
/// [`serve_migration_tls_with_handshake_timeout`] if a caller ever needs
/// something else.
const MIGRATION_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// [`serve_migration_tls`] with an explicit TLS-handshake timeout.
///
/// Production callers must use [`serve_migration_tls`], which applies
/// [`MIGRATION_HANDSHAKE_TIMEOUT`]. This constructor exists so the
/// loopback hardening tests (`tests/migration_accept_loop.rs`) can drive
/// the timeout behavior with a shortened duration instead of sleeping 30 s
/// — the timeout is a fixed constant by design (see its docs), so the test
/// seam is a function parameter, not a config knob.
pub async fn serve_migration_tls_with_handshake_timeout<B: StorageBackend>(
    listener: TcpListener,
    tls: MigrationServerTls,
    service: StorageMigrationServiceImpl<B>,
    handshake_timeout: Duration,
) -> Result<(), ChvError> {
    let acceptor = TlsAcceptor::from(Arc::new(migration_server_rustls_config(&tls)?));

    // Connections that completed the mTLS handshake, fed to the tonic
    // server below. Handshake failures are logged and dropped; they are
    // never yielded (fail-closed: a rejected peer gets no HTTP/2
    // connection at all, exactly as before).
    let (conn_tx, conn_rx) = mpsc::channel::<Result<TlsStream<TcpStream>, std::io::Error>>(64);

    // The accept task must not outlive the serving future (tests drop it;
    // the daemon runs it to process exit). Aborting on drop also closes
    // the channel once the last handshake task finishes, ending the
    // serving stream.
    let accept_guard = AbortOnDrop(tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    // Parity with tonic's TCP listener (nodelay on accept).
                    let _ = stream.set_nodelay(true);
                    let acceptor = acceptor.clone();
                    let conn_tx = conn_tx.clone();
                    tokio::spawn(async move {
                        match tokio::time::timeout(handshake_timeout, acceptor.accept(stream)).await
                        {
                            Ok(Ok(tls_stream)) => {
                                if conn_tx.send(Ok(tls_stream)).await.is_err() {
                                    // The serving future — and with it the
                                    // channel's receiver half — was dropped
                                    // while this handshake ran: server
                                    // shutdown raced a completing handshake.
                                    // Debug, not warn: nothing was rejected;
                                    // the completed stream is simply dropped.
                                    // Content-free like every other
                                    // handshake log line (peer only).
                                    debug!(
                                        peer = %peer,
                                        "migration TLS handshake completed after server shutdown; \
                                         connection dropped"
                                    );
                                }
                            }
                            Ok(Err(e)) => {
                                // Quality-of-failure only (issue #402): the
                                // handshake is refused exactly as before;
                                // the rejection is now visible. Content-free
                                // by construction: the error display is the
                                // rustls alert/reason name.
                                warn!(
                                    peer = %peer,
                                    reason = %e,
                                    "rejected migration TLS handshake"
                                );
                            }
                            Err(_elapsed) => {
                                // The peer never sent a ClientHello (or
                                // stalled mid-handshake): give up on it
                                // instead of pinning this task and its fd
                                // forever. Dropping the timed-out accept
                                // future drops the TcpStream, closing the
                                // connection. Info, not warn: a peer that
                                // never started a handshake was not
                                // rejected by mTLS policy — this is a
                                // housekeeping line, and the timeout only
                                // observes and closes, never admits.
                                // Content-free (peer address, no more).
                                info!(
                                    peer = %peer,
                                    "migration TLS handshake timed out; closing connection"
                                );
                            }
                        }
                    });
                }
                Err(e) => {
                    warn!(error = %e, "migration TLS listener accept failed");
                }
            }
        }
    }));

    let result = Server::builder()
        .layer(chv_observability::GrpcMetricsLayer::new())
        .add_service(
            StorageMigrationServiceServer::new(service)
                // BlockChunks carry a full 4 MiB migration block plus
                // protobuf overhead, exceeding tonic's 4 MiB default.
                .max_decoding_message_size(MAX_MIGRATION_MESSAGE_SIZE_BYTES),
        )
        .serve_with_incoming(ReceiverStream::new(conn_rx))
        .await;

    drop(accept_guard);
    result.map_err(|e| ChvError::Internal {
        reason: format!("migration TLS server error: {e}"),
    })
}

/// Build the rustls server config for the migration receiver listener.
///
/// Mirrors what tonic's `ServerTlsConfig` (built with `client_ca_root(...)`
/// and *without* `client_auth_optional(true)`) constructs internally: a
/// `WebPkiClientVerifier` over the configured client CA **without**
/// `allow_unauthenticated()`, plus the `h2` ALPN entry the gRPC server
/// requires. Any parse failure of the configured material is an error
/// (fail-closed); no PEM/key material is included in errors.
fn migration_server_rustls_config(tls: &MigrationServerTls) -> Result<ServerConfig, ChvError> {
    let mut roots = RootCertStore::empty();
    // rustls-pki-types' folded-in PEM parser (the rustls-pemfile
    // successor, >= 1.9): slice iterators over the in-memory PEM,
    // no io::Cursor dance needed.
    for cert in CertificateDer::pem_slice_iter(&tls.client_ca_pem) {
        let cert = cert.map_err(|e| ChvError::Internal {
            reason: format!("migration TLS client CA parse error: {e}"),
        })?;
        roots.add(cert).map_err(|_| ChvError::Internal {
            reason: "migration TLS client CA contains an unparseable certificate".to_string(),
        })?;
    }
    let verifier = WebPkiClientVerifier::builder(roots.into())
        .build()
        .map_err(|e| ChvError::Internal {
            reason: format!("migration TLS client verifier error: {e}"),
        })?;

    let certs = CertificateDer::pem_slice_iter(&tls.cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| ChvError::Internal {
            reason: format!("migration TLS server certificate parse error: {e}"),
        })?;
    // `from_pem_slice` reports a missing key as `Error::NoItemsFound`
    // (the old rustls-pemfile API returned `Ok(None)` for that case).
    let key = PrivateKeyDer::from_pem_slice(&tls.key_pem).map_err(|e| ChvError::Internal {
        reason: format!("migration TLS server key parse error: {e}"),
    })?;

    let mut config = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .map_err(|e| ChvError::Internal {
            reason: format!("migration TLS server config error: {e}"),
        })?;
    config.alpn_protocols.push(b"h2".to_vec());
    Ok(config)
}

/// Aborts the wrapped task when dropped, so an accept loop tied to a
/// serving future cannot outlive it.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
