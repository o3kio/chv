//! HTTPS serving for the manager's HTTP listener (BFF + guest
//! monitoring agent routes, campaign #602 prompt 03).
//!
//! axum 0.7's `serve` only accepts a plain `TcpListener`; TLS with
//! peer-certificate extraction needs an explicit accept loop. Each
//! accepted connection is TLS-handshaken here, the client certificate
//! (when presented) is extracted, and the per-connection service is
//! the shared router with a `TlsPeer` extension injected — handlers
//! on the agent routes read the extension; browser routes ignore it.
//!
//! Client certificates are **optional** at the TLS layer (browsers
//! do not present one): the rustls verifier validates the chain only
//! when a cert is presented, and the agent routes enforce presence
//! themselves. Verification uses the dedicated guest-agent CA, never
//! the node-enrollment CA.

use axum::Router;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::RootCertStore;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// Per-connection TLS identity, injected into request extensions by
/// the accept loop. `client_cert_der` is the raw DER of the presented
/// client certificate; `None` means the client presented none.
#[derive(Clone)]
pub struct TlsPeer {
    pub remote_addr: Option<SocketAddr>,
    pub client_cert_der: Option<Arc<Vec<u8>>>,
}

/// Build the HTTPS server config. `client_ca_pem` (the dedicated
/// guest-agent CA) enables optional client-certificate verification;
/// without it the listener serves TLS with no client auth, exactly
/// like an ordinary HTTPS server.
pub fn build_https_config(
    server_cert_pem: &str,
    server_key_pem: &str,
    client_ca_pem: Option<&str>,
) -> Result<Arc<rustls::ServerConfig>, String> {
    // rustls-pki-types' folded-in PEM parser (the rustls-pemfile
    // successor, retired from this repo in #235): slice iterators
    // over the in-memory PEM.
    let certs: Vec<CertificateDer<'static>> =
        CertificateDer::pem_slice_iter(server_cert_pem.as_bytes())
            .collect::<Result<_, _>>()
            .map_err(|e| format!("failed to parse server certificate PEM: {e}"))?;
    if certs.is_empty() {
        return Err("server certificate PEM contains no certificates".to_string());
    }
    // `from_pem_slice` reports a missing key as `Error::NoItemsFound`
    // (the old rustls-pemfile API returned `Ok(None)` for that case).
    let key = PrivateKeyDer::from_pem_slice(server_key_pem.as_bytes()).map_err(|e| match e {
        rustls::pki_types::pem::Error::NoItemsFound => {
            "server key PEM contains no private key".to_string()
        }
        e => format!("failed to parse server key PEM: {e}"),
    })?;

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("failed to negotiate TLS protocol versions: {e}"))?;

    let config = match client_ca_pem {
        Some(ca_pem) => {
            let mut roots = RootCertStore::empty();
            let cas: Vec<CertificateDer<'static>> =
                CertificateDer::pem_slice_iter(ca_pem.as_bytes())
                    .collect::<Result<_, _>>()
                    .map_err(|e| format!("failed to parse agent CA PEM: {e}"))?;
            if cas.is_empty() {
                return Err("agent CA PEM contains no certificates".to_string());
            }
            for ca in cas {
                roots
                    .add(ca)
                    .map_err(|e| format!("failed to add agent CA to trust store: {e}"))?;
            }
            // Optional client auth: browsers connect without a
            // certificate; agents present one that must chain to the
            // agent CA when presented. The verifier is pinned to the
            // same explicit provider as the config builder — rustls
            // would otherwise demand a process-default provider.
            let verifier = WebPkiClientVerifier::builder_with_provider(
                Arc::new(roots),
                Arc::new(rustls::crypto::ring::default_provider()),
            )
            .allow_unauthenticated()
            .build()
            .map_err(|e| format!("failed to build client certificate verifier: {e}"))?;
            builder.with_client_cert_verifier(verifier)
        }
        None => builder.with_no_client_auth(),
    };

    let mut config = config
        .with_single_cert(certs, key)
        .map_err(|e| format!("failed to assemble server certificate chain: {e}"))?;
    // The guest routes are small bounded JSON posts.
    config.max_early_data_size = 0;
    Ok(Arc::new(config))
}

/// Serve `router` over TLS on `listener` until `shutdown` fires.
/// In-flight connections get a bounded drain window (60 s), matching
/// the graceful-shutdown discipline of the plain-HTTP path.
pub async fn serve_tls(
    listener: TcpListener,
    tls: Arc<rustls::ServerConfig>,
    router: Router,
    mut shutdown: tokio::sync::watch::Receiver<()>,
) -> io::Result<()> {
    let acceptor = TlsAcceptor::from(tls);
    let mut tasks = tokio::task::JoinSet::new();

    loop {
        let (stream, remote_addr) = tokio::select! {
            _ = shutdown.changed() => break,
            accepted = listener.accept() => match accepted {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!(error = %e, "https accept error");
                    continue;
                }
            },
        };

        let acceptor = acceptor.clone();
        let router = router.clone();
        tasks.spawn(async move {
            let tls_stream = match acceptor.accept(stream).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(error = %e, "tls handshake failed");
                    return;
                }
            };
            let client_cert_der = tls_stream
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|certs| certs.first())
                .map(|c| Arc::new(c.as_ref().to_vec()));
            let peer = TlsPeer {
                remote_addr: Some(remote_addr),
                client_cert_der,
            };

            // Inject the per-connection identity into every request.
            let service = PeerInjectService {
                inner: router,
                peer: peer.clone(),
            };
            let hyper_service = hyper_util::service::TowerToHyperService::new(service);
            let io = hyper_util::rt::TokioIo::new(tls_stream);
            let _ =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection(io, hyper_service)
                    .await;
        });
    }

    // Bounded drain: in-flight requests (guest batches are small)
    // finish or the connection is dropped after the window.
    let drain = async {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        while tasks.join_next().await.is_some() {
            if tokio::time::Instant::now() >= deadline {
                tasks.abort_all();
                break;
            }
        }
    };
    drain.await;
    Ok(())
}

/// A tower service that stamps the connection's `TlsPeer` into every
/// request's extensions. Handlers on the agent routes read it;
/// browser routes ignore it.
#[derive(Clone)]
struct PeerInjectService<S> {
    inner: S,
    peer: TlsPeer,
}

impl<S, B> tower::Service<axum::http::Request<B>> for PeerInjectService<S>
where
    S: tower::Service<axum::http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: axum::http::Request<B>) -> Self::Future {
        req.extensions_mut().insert(self.peer.clone());
        // The real transport peer for anything that reads
        // `ConnectInfo<SocketAddr>` — most importantly the BFF audit
        // middleware's `source_ip` (#658 review M1). Without this, the
        // TLS path carries no ConnectInfo and the audit row would fall
        // back to the client-supplied `X-Forwarded-For`, i.e. a
        // spoofable attribution field. axum's
        // `into_make_service_with_connect_info` works exactly this
        // way: the extractor reads itself from request extensions, so
        // a manual insertion is equivalent.
        if let Some(remote_addr) = self.peer.remote_addr {
            req.extensions_mut()
                .insert(axum::extract::ConnectInfo(remote_addr));
        }
        self.inner.call(req)
    }
}
