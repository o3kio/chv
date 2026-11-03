use crate::executor::NetworkExecutor;
use crate::handlers::NetworkServiceImpl;
use crate::link_monitor::{link_health_loop, LinkHealthSnapshot};
use crate::state::TopologyTable;
use chv_errors::ChvError;
use chv_nwd_api::chv_nwd_api::network_service_server::NetworkServiceServer;
use chv_observability::Metrics;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use tracing::info;

pub struct NetworkServer<E: NetworkExecutor> {
    inner: NetworkServiceImpl<E>,
    /// Interfaces to monitor for link health. Defaults to ["eth0"].
    monitor_interfaces: Vec<String>,
}

impl<E: NetworkExecutor> NetworkServer<E> {
    pub fn new(executor: E, metrics: Metrics) -> Self {
        let executor = Arc::new(executor);
        let topologies = Arc::new(TopologyTable::new());
        let inner = NetworkServiceImpl::new(executor, topologies, Arc::new(metrics));
        Self {
            inner,
            monitor_interfaces: vec!["eth0".to_string()],
        }
    }

    /// Set the interfaces to monitor for link health.
    pub fn with_monitor_interfaces(mut self, interfaces: Vec<String>) -> Self {
        self.monitor_interfaces = interfaces;
        self
    }

    pub async fn serve(self, socket_path: &Path) -> Result<(), ChvError> {
        if let Some(parent) = socket_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ChvError::Io {
                    path: parent.to_string_lossy().to_string(),
                    source: e,
                })?;
        }

        if let Err(e) = tokio::fs::remove_file(socket_path).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(ChvError::Io {
                    path: socket_path.to_string_lossy().to_string(),
                    source: e,
                });
            }
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

        let uds_stream = UnixListenerStream::new(uds);

        let (health_reporter, health_service) = tonic_health::server::health_reporter();
        health_reporter
            .set_serving::<NetworkServiceServer<NetworkServiceImpl<E>>>()
            .await;

        info!(socket = %socket_path.display(), "starting chv-nwd server");

        // Spawn link health monitoring
        let (link_shutdown_tx, link_shutdown_rx) = tokio::sync::watch::channel(());
        let monitor_interfaces = self.monitor_interfaces.clone();
        tokio::spawn(async move {
            link_health_loop(
                monitor_interfaces,
                30, // check every 30 seconds
                link_shutdown_rx,
                |snapshots: &[LinkHealthSnapshot]| {
                    for snap in snapshots {
                        if !snap.is_up {
                            tracing::warn!(
                                interface = %snap.iface,
                                carrier = snap.carrier,
                                flap_count = snap.flap_count,
                                "link status change detected: interface down"
                            );
                        }
                    }
                },
            )
            .await;
        });

        let result = Server::builder()
            .layer(chv_observability::GrpcMetricsLayer::new())
            .add_service(health_service)
            .add_service(NetworkServiceServer::new(self.inner))
            .serve_with_incoming(uds_stream)
            .await
            .map_err(|e| ChvError::Internal {
                reason: format!("server error: {e}"),
            });

        // Signal link monitor shutdown
        let _ = link_shutdown_tx.send(());

        result
    }
}
