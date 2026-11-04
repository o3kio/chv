use crate::enrollment::EnrollmentServiceImplementation;
use crate::error::ControlPlaneServiceError;
use crate::inventory::InventoryServiceImplementation;
use crate::lifecycle::LifecycleServiceImplementation;
use crate::monitoring_ingest::MonitoringIngestImplementation;
use crate::peer_identity::PeerIdentityInterceptor;
use crate::reconcile::ReconcileServiceImplementation;
use crate::revocation::run_refresh_loop;
use crate::telemetry::TelemetryServiceImplementation;
use chv_controlplane_store::{AuditLogRepository, CertificateRevocationRepository, StorePool};
use control_plane_node_api::control_plane_node_api as proto;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};

pub struct ControlPlaneRuntime {
    bind_addr: SocketAddr,
    runtime_dir: PathBuf,
    tls_config: Option<tonic::transport::ServerTlsConfig>,
    /// When true, the peer-identity interceptor passes through and handlers
    /// accept the wire-asserted node_id. Set from `CHV_ALLOW_INSECURE=1` only.
    allow_insecure: bool,
    http_shutdown_tx: tokio::sync::watch::Sender<()>,
    http_join_handle:
        tokio::sync::Mutex<Option<tokio::task::JoinHandle<Result<(), std::io::Error>>>>,
    shutdown_rx: tokio::sync::watch::Receiver<()>,
}

impl ControlPlaneRuntime {
    pub fn new(
        bind_addr: SocketAddr,
        runtime_dir: PathBuf,
        tls_config: Option<tonic::transport::ServerTlsConfig>,
        allow_insecure: bool,
        http_shutdown_tx: tokio::sync::watch::Sender<()>,
        http_join_handle: tokio::task::JoinHandle<Result<(), std::io::Error>>,
        shutdown_rx: tokio::sync::watch::Receiver<()>,
    ) -> Self {
        Self {
            bind_addr,
            runtime_dir,
            tls_config,
            allow_insecure,
            http_shutdown_tx,
            http_join_handle: tokio::sync::Mutex::new(Some(http_join_handle)),
            shutdown_rx,
        }
    }

    pub fn bind_addr(&self) -> SocketAddr {
        self.bind_addr
    }

    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    pub fn tls_config(&self) -> &Option<tonic::transport::ServerTlsConfig> {
        &self.tls_config
    }

    pub fn allow_insecure(&self) -> bool {
        self.allow_insecure
    }
}

#[derive(Clone)]
pub struct ControlPlaneComponents {
    store_pool: StorePool,
    enrollment_service: EnrollmentServiceImplementation,
    inventory_service: InventoryServiceImplementation,
    telemetry_service: TelemetryServiceImplementation,
    reconcile_service: ReconcileServiceImplementation,
    lifecycle_service: LifecycleServiceImplementation,
    monitoring_ingest_service: MonitoringIngestImplementation,
}

impl ControlPlaneComponents {
    pub fn new(
        store_pool: StorePool,
        enrollment_service: EnrollmentServiceImplementation,
        inventory_service: InventoryServiceImplementation,
        telemetry_service: TelemetryServiceImplementation,
        reconcile_service: ReconcileServiceImplementation,
        lifecycle_service: LifecycleServiceImplementation,
        monitoring_ingest_service: MonitoringIngestImplementation,
    ) -> Self {
        Self {
            store_pool,
            enrollment_service,
            inventory_service,
            telemetry_service,
            reconcile_service,
            lifecycle_service,
            monitoring_ingest_service,
        }
    }

    pub fn store_pool(&self) -> &StorePool {
        &self.store_pool
    }

    pub fn enrollment_service(&self) -> &EnrollmentServiceImplementation {
        &self.enrollment_service
    }

    pub fn inventory_service(&self) -> &InventoryServiceImplementation {
        &self.inventory_service
    }

    pub fn telemetry_service(&self) -> &TelemetryServiceImplementation {
        &self.telemetry_service
    }

    pub fn reconcile_service(&self) -> &ReconcileServiceImplementation {
        &self.reconcile_service
    }

    pub fn lifecycle_service(&self) -> &LifecycleServiceImplementation {
        &self.lifecycle_service
    }

    pub fn monitoring_ingest_service(&self) -> &MonitoringIngestImplementation {
        &self.monitoring_ingest_service
    }
}

pub struct ControlPlaneService {
    runtime: ControlPlaneRuntime,
    components: ControlPlaneComponents,
    shutdown_tx: tokio::sync::watch::Sender<()>,
    worker_handles: tokio::sync::Mutex<Option<Vec<tokio::task::JoinHandle<()>>>>,
}

impl ControlPlaneService {
    pub fn new(
        runtime: ControlPlaneRuntime,
        components: ControlPlaneComponents,
        shutdown_tx: tokio::sync::watch::Sender<()>,
        worker_handles: Vec<tokio::task::JoinHandle<()>>,
    ) -> Self {
        Self {
            runtime,
            components,
            shutdown_tx,
            worker_handles: tokio::sync::Mutex::new(Some(worker_handles)),
        }
    }

    pub async fn shutdown(&self) {
        // Signal all workers to stop via the watch channel
        let _ = self.shutdown_tx.send(());

        // Take ownership of the handles so we can await them
        let handles = self.worker_handles.lock().await.take();
        if let Some(handles) = handles {
            const GRACEFUL_TIMEOUT: Duration = Duration::from_secs(10);

            for handle in handles {
                let abort_handle = handle.abort_handle();
                match tokio::time::timeout(GRACEFUL_TIMEOUT, handle).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        warn!("worker task panicked during shutdown: {}", e);
                    }
                    Err(_) => {
                        warn!(
                            "worker task did not finish within {:?}, aborting",
                            GRACEFUL_TIMEOUT
                        );
                        abort_handle.abort();
                    }
                }
            }
        }
    }

    pub fn shutdown_tx(&self) -> tokio::sync::watch::Sender<()> {
        self.shutdown_tx.clone()
    }

    pub fn runtime(&self) -> &ControlPlaneRuntime {
        &self.runtime
    }

    pub fn components(&self) -> &ControlPlaneComponents {
        &self.components
    }

    pub async fn run(&self) -> Result<(), ControlPlaneServiceError> {
        let addr = self.runtime.bind_addr();

        // Peer-identity interceptor: pins the wire-asserted node_id to the
        // peer's mTLS leaf certificate (closes C1/H1 cross-node trust gap)
        // and rejects serials present in the certificate denylist (#661).
        // In CHV_ALLOW_INSECURE=1 mode this is a no-op; in production it
        // rejects requests without a parseable peer cert. Constructing it is
        // fallible: requesting insecure mode without the `dev` feature returns
        // a typed startup error (issue #233) instead of panicking.
        //
        // The revocation snapshot is loaded BEFORE the listener starts. In
        // production mode a failed load is fatal: serving with unknown
        // revocation state would silently re-enable revoked certificates.
        let revocation_repo =
            CertificateRevocationRepository::new(self.components.store_pool.clone());
        let revocation = crate::revocation::load_startup_snapshot(
            &revocation_repo,
            self.runtime.allow_insecure(),
        )
        .await
        .map_err(|error| {
            ControlPlaneServiceError::Internal(format!(
                "refusing to start: cannot load node certificate revocation snapshot \
                         from node_certificate_revocations: {error}. Fix the store or rebuild the \
                         database; never start production with unknown revocation state."
            ))
        })?;
        metrics::gauge!("chv_node_certificate_revocations_active").set(revocation.len() as f64);

        let interceptor =
            PeerIdentityInterceptor::new(self.runtime.allow_insecure(), revocation.clone())?;

        // Enrollment is the controlled recovery path: a node whose certificate
        // was revoked must still be able to present a fresh one-time bootstrap
        // token. This interceptor extracts/binds the peer identity but skips
        // the denylist; rotation is re-checked in EnrollmentServer. Every other
        // service keeps full revocation enforcement.
        let enrollment_interceptor = PeerIdentityInterceptor::for_enrollment(
            self.runtime.allow_insecure(),
            revocation.clone(),
        )?;

        // Refresh the denylist in the background. In dev mode revocation is
        // bypassed entirely, so no reload task is needed.
        let refresh_handle = if self.runtime.allow_insecure() {
            None
        } else {
            let refresh_shutdown = self.runtime.shutdown_rx.clone();
            Some(tokio::spawn(run_refresh_loop(
                revocation_repo,
                revocation.clone(),
                crate::revocation::refresh_interval_from_env(),
                refresh_shutdown,
            )))
        };

        let make_intercept = || {
            let interceptor = interceptor.clone();
            #[allow(clippy::result_large_err)]
            // tonic::Status is the required error type for tonic interceptors.
            move |req: tonic::Request<()>| interceptor.intercept(req)
        };

        let make_enrollment_intercept = || {
            let interceptor = enrollment_interceptor.clone();
            #[allow(clippy::result_large_err)]
            move |req: tonic::Request<()>| interceptor.intercept(req)
        };

        let enrollment_server =
            proto::enrollment_service_server::EnrollmentServiceServer::with_interceptor(
                crate::server::EnrollmentServer::new(Arc::new(
                    self.components.enrollment_service.clone(),
                ))
                .with_revocation(revocation.clone())
                .with_audit(AuditLogRepository::new(self.components.store_pool.clone())),
                make_enrollment_intercept(),
            );

        let inventory_server =
            proto::inventory_service_server::InventoryServiceServer::with_interceptor(
                crate::server::InventoryServer::new(Arc::new(
                    self.components.inventory_service.clone(),
                )),
                make_intercept(),
            );

        let telemetry_server =
            proto::telemetry_service_server::TelemetryServiceServer::with_interceptor(
                crate::server::TelemetryServer::new(Arc::new(
                    self.components.telemetry_service.clone(),
                )),
                make_intercept(),
            );

        let reconcile_server =
            proto::reconcile_service_server::ReconcileServiceServer::with_interceptor(
                crate::server::ReconcileServer::new(Arc::new(
                    self.components.reconcile_service.clone(),
                )),
                make_intercept(),
            );

        let lifecycle_server =
            proto::lifecycle_service_server::LifecycleServiceServer::with_interceptor(
                crate::server::LifecycleServer::new(Arc::new(
                    self.components.lifecycle_service.clone(),
                ))
                .with_audit(AuditLogRepository::new(self.components.store_pool.clone())),
                make_intercept(),
            );

        let monitoring_server =
            proto::monitoring_service_server::MonitoringServiceServer::with_interceptor(
                crate::server::MonitoringServer::new(Arc::new(
                    self.components.monitoring_ingest_service.clone(),
                )),
                make_intercept(),
            );

        if self.runtime.tls_config().is_some() {
            info!(?addr, "starting gRPC server with TLS");
        } else {
            warn!(?addr, "gRPC server starting WITHOUT TLS — connections are unencrypted. \
                Configure [tls] section with server_cert_path and server_key_path for production use.");
        }

        let mut server = tonic::transport::Server::builder();
        if let Some(tls_config) = &self.runtime.tls_config {
            server = match server.tls_config(tls_config.clone()) {
                Ok(s) => {
                    info!("gRPC TLS configured successfully");
                    s
                }
                Err(e) => {
                    error!(
                        error = %e,
                        "failed to configure gRPC TLS; refusing to start without TLS"
                    );
                    return Err(ControlPlaneServiceError::Internal(format!(
                        "TLS configuration failed: {}. Fix the TLS config or remove [tls] section to run without TLS.", e
                    )));
                }
            };
        }

        let mut shutdown_rx = self.runtime.shutdown_rx.clone();
        let grpc_result = server
            .layer(chv_observability::GrpcMetricsLayer::new())
            .add_service(enrollment_server)
            .add_service(inventory_server)
            .add_service(telemetry_server)
            .add_service(reconcile_server)
            .add_service(lifecycle_server)
            .add_service(monitoring_server)
            .serve_with_shutdown(addr, async move {
                let _ = shutdown_rx.changed().await;
            })
            .await;

        let _ = self.runtime.http_shutdown_tx.send(());

        let handle = self.runtime.http_join_handle.lock().await.take();
        if let Some(handle) = handle {
            if let Err(e) = handle.await {
                error!("HTTP admin server join error: {}", e);
            }
        }

        grpc_result.map_err(|e: tonic::transport::Error| {
            error!("gRPC server error: {}", e);
            ControlPlaneServiceError::Internal(e.to_string())
        })?;

        if let Some(handle) = refresh_handle {
            handle.abort();
        }

        Ok(())
    }
}
