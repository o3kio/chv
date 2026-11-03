use crate::cache::{NodeCache, PendingControlPlaneMessage, PendingControlPlaneMessageKind};
use chv_errors::ChvError;
use control_plane_node_api::control_plane_node_api as proto;
use std::sync::Arc;
use tonic::transport::Channel;

pub struct ControlPlaneClient {
    reconcile: proto::reconcile_service_client::ReconcileServiceClient<Channel>,
    telemetry: proto::telemetry_service_client::TelemetryServiceClient<Channel>,
    inventory: proto::inventory_service_client::InventoryServiceClient<Channel>,
    monitoring: proto::monitoring_service_client::MonitoringServiceClient<Channel>,
}

impl ControlPlaneClient {
    pub async fn new(
        endpoint: impl Into<String>,
        tls_cert_path: Option<&std::path::Path>,
        tls_key_path: Option<&std::path::Path>,
        ca_cert_path: Option<&std::path::Path>,
    ) -> Result<Self, ChvError> {
        let endpoint_str: String = endpoint.into();
        let is_https = endpoint_str.starts_with("https://");
        // Extract hostname from endpoint for TLS SNI verification
        let domain = endpoint_str
            .strip_prefix("https://")
            .or_else(|| endpoint_str.strip_prefix("http://"))
            .and_then(|s| s.split(':').next())
            .and_then(|s| s.split('/').next())
            .unwrap_or("localhost")
            .to_string();
        let mut endpoint = tonic::transport::Endpoint::try_from(endpoint_str).map_err(|e| {
            ChvError::InvalidArgument {
                field: "control_plane_addr".to_string(),
                reason: e.to_string(),
            }
        })?;

        if is_https {
            if let (Some(cert), Some(key)) = (tls_cert_path, tls_key_path) {
                let cert_pem = tokio::fs::read(cert).await.map_err(|e| ChvError::Io {
                    path: cert.to_string_lossy().to_string(),
                    source: e,
                })?;
                let key_pem = tokio::fs::read(key).await.map_err(|e| ChvError::Io {
                    path: key.to_string_lossy().to_string(),
                    source: e,
                })?;
                let identity = tonic::transport::Identity::from_pem(cert_pem, key_pem);
                let mut tls = tonic::transport::ClientTlsConfig::new()
                    .domain_name(&domain)
                    .identity(identity);
                if let Some(ca) = ca_cert_path {
                    let ca_pem = tokio::fs::read(ca).await.map_err(|e| ChvError::Io {
                        path: ca.to_string_lossy().to_string(),
                        source: e,
                    })?;
                    tls = tls.ca_certificate(tonic::transport::Certificate::from_pem(ca_pem));
                }
                endpoint = endpoint.tls_config(tls).map_err(|e| {
                    let mut reason = e.to_string();
                    let mut source: Option<&dyn std::error::Error> = std::error::Error::source(&e);
                    while let Some(s) = source {
                        reason.push_str(": ");
                        reason.push_str(&s.to_string());
                        source = s.source();
                    }
                    ChvError::InvalidArgument {
                        field: "tls_config".to_string(),
                        reason,
                    }
                })?;
            }
        }

        let channel = endpoint
            .connect()
            .await
            .map_err(|e| ChvError::ControlPlaneUnavailable {
                reason: e.to_string(),
            })?;
        Ok(Self {
            reconcile: proto::reconcile_service_client::ReconcileServiceClient::new(
                channel.clone(),
            ),
            telemetry: proto::telemetry_service_client::TelemetryServiceClient::new(
                channel.clone(),
            ),
            inventory: proto::inventory_service_client::InventoryServiceClient::new(
                channel.clone(),
            ),
            monitoring: proto::monitoring_service_client::MonitoringServiceClient::new(channel),
        })
    }

    /// Ingest one node metric batch (campaign #602 PR-2). Purely
    /// observational: a transport failure is a retry-later condition and
    /// must never influence reconciliation or state reports. The batch
    /// sender owns a dedicated client instance so manager backpressure
    /// can never pause reconciliation.
    pub async fn ingest_node_metric_batch(
        &mut self,
        request: proto::NodeMetricBatchRequest,
    ) -> Result<proto::NodeMetricBatchResponse, ChvError> {
        self.monitoring
            .ingest_node_metric_batch(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|e| ChvError::ControlPlaneUnavailable {
                reason: e.to_string(),
            })
    }

    pub fn stale_generation_check(
        meta: &proto::RequestMeta,
        cache: &NodeCache,
        kind: &str,
        id: &str,
    ) -> Result<(), ChvError> {
        let incoming = &meta.desired_state_version;
        if cache.is_stale(kind, id, incoming)? {
            let current = cache.get_generation(kind, id).cloned().unwrap_or_default();
            return Err(ChvError::StaleGeneration {
                resource: kind.to_string(),
                id: id.to_string(),
                expected: current,
                got: incoming.clone(),
            });
        }
        Ok(())
    }

    pub async fn apply_node_desired_state(
        &mut self,
        req: proto::ApplyNodeDesiredStateRequest,
    ) -> Result<proto::AckResponse, ChvError> {
        self.reconcile
            .apply_node_desired_state(req)
            .await
            .map(|r| r.into_inner())
            .map_err(|e| ChvError::ControlPlaneUnavailable {
                reason: e.to_string(),
            })
    }

    pub async fn report_node_state(
        &mut self,
        req: proto::NodeStateReport,
    ) -> Result<proto::AckResponse, ChvError> {
        self.telemetry
            .report_node_state(req)
            .await
            .map(|r| r.into_inner())
            .map_err(|e| ChvError::ControlPlaneUnavailable {
                reason: e.to_string(),
            })
    }

    pub async fn report_vm_state(
        &mut self,
        req: proto::VmStateReport,
    ) -> Result<proto::AckResponse, ChvError> {
        self.telemetry
            .report_vm_state(req)
            .await
            .map(|r| r.into_inner())
            .map_err(|e| ChvError::ControlPlaneUnavailable {
                reason: e.to_string(),
            })
    }

    pub async fn report_volume_state(
        &mut self,
        req: proto::VolumeStateReport,
    ) -> Result<proto::AckResponse, ChvError> {
        self.telemetry
            .report_volume_state(req)
            .await
            .map(|r| r.into_inner())
            .map_err(|e| ChvError::ControlPlaneUnavailable {
                reason: e.to_string(),
            })
    }

    pub async fn report_network_state(
        &mut self,
        req: proto::NetworkStateReport,
    ) -> Result<proto::AckResponse, ChvError> {
        self.telemetry
            .report_network_state(req)
            .await
            .map(|r| r.into_inner())
            .map_err(|e| ChvError::ControlPlaneUnavailable {
                reason: e.to_string(),
            })
    }

    pub async fn publish_event(
        &mut self,
        req: proto::PublishEventRequest,
    ) -> Result<proto::AckResponse, ChvError> {
        self.telemetry
            .publish_event(req)
            .await
            .map(|r| r.into_inner())
            .map_err(|e| ChvError::ControlPlaneUnavailable {
                reason: e.to_string(),
            })
    }

    pub async fn publish_alert(
        &mut self,
        req: proto::PublishAlertRequest,
    ) -> Result<proto::AckResponse, ChvError> {
        self.telemetry
            .publish_alert(req)
            .await
            .map(|r| r.into_inner())
            .map_err(|e| ChvError::ControlPlaneUnavailable {
                reason: e.to_string(),
            })
    }

    pub async fn report_node_inventory(
        &mut self,
        req: proto::ReportNodeInventoryRequest,
    ) -> Result<proto::AckResponse, ChvError> {
        self.inventory
            .report_node_inventory(req)
            .await
            .map(|r| r.into_inner())
            .map_err(|e| ChvError::ControlPlaneUnavailable {
                reason: e.to_string(),
            })
    }

    pub async fn report_service_versions(
        &mut self,
        req: proto::ReportServiceVersionsRequest,
    ) -> Result<proto::AckResponse, ChvError> {
        self.inventory
            .report_service_versions(req)
            .await
            .map(|r| r.into_inner())
            .map_err(|e| ChvError::ControlPlaneUnavailable {
                reason: e.to_string(),
            })
    }

    pub async fn report_migration_progress(
        &mut self,
        req: proto::MigrationProgress,
    ) -> Result<proto::AckResponse, ChvError> {
        self.telemetry
            .report_migration_progress(req)
            .await
            .map(|r| r.into_inner())
            .map_err(|e| ChvError::ControlPlaneUnavailable {
                reason: e.to_string(),
            })
    }

    pub async fn dispatch_pending_message(
        &mut self,
        message: &PendingControlPlaneMessage,
        local_node_id: &str,
    ) -> Result<(), ChvError> {
        match message.kind {
            PendingControlPlaneMessageKind::NodeStateReport => {
                self.report_node_state(message.decode_node_state()?).await?;
            }
            PendingControlPlaneMessageKind::VmStateReport => {
                self.report_vm_state(message.decode_vm_state()?).await?;
            }
            PendingControlPlaneMessageKind::VolumeStateReport => {
                self.report_volume_state(message.decode_volume_state()?)
                    .await?;
            }
            PendingControlPlaneMessageKind::NetworkStateReport => {
                self.report_network_state(message.decode_network_state()?)
                    .await?;
            }
            PendingControlPlaneMessageKind::PublishEvent => {
                self.publish_event(message.decode_event()?).await?;
            }
            PendingControlPlaneMessageKind::PublishAlert => {
                self.publish_alert(message.decode_alert()?).await?;
            }
            PendingControlPlaneMessageKind::ReportNodeInventory => {
                self.report_node_inventory(message.decode_node_inventory()?)
                    .await?;
            }
            PendingControlPlaneMessageKind::ReportServiceVersions => {
                self.report_service_versions(message.decode_service_versions()?)
                    .await?;
            }
            PendingControlPlaneMessageKind::MigrationProgressReport => {
                let mut progress = message.decode_migration_progress()?;
                // Back-compat: the queue is persisted JSON. An agent upgraded
                // from a build that predates the `node_id` field deserialises
                // old entries with `""`; send our identity rather than a value
                // the control plane must reject (which would stall the whole
                // ordered queue behind this message).
                if progress.node_id.is_empty() {
                    progress.node_id = local_node_id.to_string();
                }
                self.report_migration_progress(progress).await?;
            }
        }
        Ok(())
    }
}

/// Drain the pending control-plane queue on an agent tick (#582).
/// The queue exists to survive control-plane unavailability, but several
/// producers enqueue unconditionally — migration progress reports (the
/// agent_server reporter) among them — and without a periodic drain those
/// messages would sit queued until the next reconnect, which never comes
/// during a stable connection: the CP's migration row would never update
/// at all (masked before #582 by a vacuous convergence check that fired
/// on the first poll), and the CP's memory-phase wait could never observe
/// the agent's MemoryMigration/Completed phase reports either.
///
/// Called on every agent tick (~5 s) with the current telemetry client:
/// - queue empty → no-op (no dispatch, no cache write);
/// - drained → the cache is persisted;
/// - dispatch failure → the unsent remainder is re-queued ahead of
///   anything enqueued meanwhile, a connectivity failure is recorded,
///   and `None` is returned so a later tick reconnects (the reconnect
///   path flushes the re-queued remainder).
///
/// The queue is snapshotted and cleared under the lock, but the gRPC
/// dispatches happen OUTSIDE it: the inner dispatch calls have no
/// timeout of their own, so holding the lock across them would let a
/// slow or half-open control plane wedge every other cache consumer
/// (the migration reporter's try_lock-or-skip enqueue, and the
/// agent-server RPC handlers) for the duration of the RPCs. Messages
/// enqueued while a batch is in flight simply wait for the next tick.
/// Each dispatch is also bounded by a 10 s deadline — a hung (but
/// established) connection is converted into a connectivity failure
/// and a client drop rather than stalling the agent tick forever.
pub async fn drain_pending_control_plane_queue(
    cache: &Arc<tokio::sync::Mutex<NodeCache>>,
    cache_path: &std::path::Path,
    telemetry: Option<ControlPlaneClient>,
    connectivity: &mut crate::connectivity::ConnectivityTracker,
) -> Option<ControlPlaneClient> {
    drain_pending_control_plane_queue_with_timeout(
        cache,
        cache_path,
        telemetry,
        connectivity,
        PENDING_DISPATCH_TIMEOUT,
    )
    .await
}

/// Deadline for a single queued-message dispatch inside the drain.
/// The inner dispatch calls carry no timeout of their own, so without
/// this a hung (but established) control-plane connection would stall
/// the drain forever — and, because the failure path would never run,
/// the agent would never record the connectivity failure or drop the
/// client to reconnect. Acking a queued message is a fast unary for a
/// healthy control plane; 10 s is far beyond anything legitimate.
/// (Tests shrink it through `drain_pending_control_plane_queue_with_timeout`
/// to pin the deadline behavior without waiting real seconds.)
const PENDING_DISPATCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

async fn drain_pending_control_plane_queue_with_timeout(
    cache: &Arc<tokio::sync::Mutex<NodeCache>>,
    cache_path: &std::path::Path,
    mut telemetry: Option<ControlPlaneClient>,
    connectivity: &mut crate::connectivity::ConnectivityTracker,
    dispatch_timeout: std::time::Duration,
) -> Option<ControlPlaneClient> {
    if telemetry.is_none() {
        return telemetry;
    }
    // Snapshot and clear the queue under the lock, then dispatch
    // outside it. The batch is only taken from memory here — the
    // on-disk cache still holds it until the post-dispatch save, so a
    // crash mid-dispatch loses nothing (the queue is at-least-once).
    let (batch, local_node_id) = {
        let mut locked = cache.lock().await;
        let batch = locked.pending_control_plane_messages().to_vec();
        if batch.is_empty() {
            (None, String::new())
        } else {
            let node_id = locked.node_id.clone();
            locked.replace_pending_control_plane_messages(Vec::new());
            (Some(batch), node_id)
        }
    };
    let Some(batch) = batch else {
        return telemetry;
    };

    // Dispatch outside the lock; stop at the first failure and keep
    // the unsent remainder (this batch plus anything enqueued while
    // we were dispatching — the remainder goes back first, ahead of
    // the newer messages, to preserve queue order). Each dispatch is
    // bounded by the caller's deadline (see PENDING_DISPATCH_TIMEOUT).
    let client = telemetry
        .as_mut()
        .expect("telemetry presence checked above");
    let total = batch.len();
    let mut failed_from = None;
    for (i, message) in batch.iter().enumerate() {
        let kind = format!("{:?}", message.kind);
        match tokio::time::timeout(
            dispatch_timeout,
            client.dispatch_pending_message(message, &local_node_id),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(
                    message_kind = %kind,
                    sent = i,
                    remaining = total - i,
                    error = %e,
                    "failed to flush pending control-plane messages on tick"
                );
                failed_from = Some(i);
                break;
            }
            Err(_) => {
                tracing::warn!(
                    message_kind = %kind,
                    sent = i,
                    remaining = total - i,
                    timeout_secs = dispatch_timeout.as_secs(),
                    "timed out flushing pending control-plane messages on tick"
                );
                failed_from = Some(i);
                break;
            }
        }
    }

    // The commit (remainder re-queue and cache save) happens under the
    // lock, unlike the dispatches: the save must be atomic with the
    // queue mutation against concurrent enqueues, and it is a fast
    // local JSON write — nothing like the unbounded RPCs above. A
    // reporter whose try_lock lands in this window skips one report,
    // which the ~5 s report cadence and the 90 s freshness budget
    // absorb.
    {
        let mut locked = cache.lock().await;
        if let Some(i) = failed_from {
            let mut remainder = batch[i..].to_vec();
            remainder.extend_from_slice(locked.pending_control_plane_messages());
            locked.replace_pending_control_plane_messages(remainder);
        }
        if let Err(e) = locked.save(cache_path).await {
            tracing::warn!(
                error = %e,
                "failed to save cache after draining pending control-plane messages"
            );
        }
    }

    if failed_from.is_some() {
        connectivity.record_failure(chv_common::now_unix_ms());
        None
    } else {
        telemetry
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::PendingControlPlaneMessage;
    use proto::{
        inventory_service_server::{InventoryService, InventoryServiceServer},
        telemetry_service_server::{TelemetryService, TelemetryServiceServer},
    };
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use tokio::sync::oneshot;
    use tonic::{Request, Response, Status};

    #[test]
    fn stale_generation_rejected() {
        let mut cache = NodeCache::new("node-1");
        cache.observe_generation("node", "node-1", "10");

        let meta = proto::RequestMeta {
            operation_id: "op-1".to_string(),
            requested_by: "cp".to_string(),
            target_node_id: "node-1".to_string(),
            desired_state_version: "9".to_string(),
            request_unix_ms: 0,
        };

        let result = ControlPlaneClient::stale_generation_check(&meta, &cache, "node", "node-1");
        assert!(matches!(result, Err(ChvError::StaleGeneration { .. })));
    }

    #[test]
    fn fresh_generation_accepted() {
        let mut cache = NodeCache::new("node-1");
        cache.observe_generation("node", "node-1", "10");

        let meta = proto::RequestMeta {
            operation_id: "op-1".to_string(),
            requested_by: "cp".to_string(),
            target_node_id: "node-1".to_string(),
            desired_state_version: "11".to_string(),
            request_unix_ms: 0,
        };

        let result = ControlPlaneClient::stale_generation_check(&meta, &cache, "node", "node-1");
        assert!(result.is_ok());
    }

    #[test]
    fn non_numeric_generation_rejected() {
        let mut cache = NodeCache::new("node-1");
        cache.observe_generation("node", "node-1", "v2");

        let meta = proto::RequestMeta {
            operation_id: "op-1".to_string(),
            requested_by: "cp".to_string(),
            target_node_id: "node-1".to_string(),
            desired_state_version: "v3".to_string(),
            request_unix_ms: 0,
        };

        let result = ControlPlaneClient::stale_generation_check(&meta, &cache, "node", "node-1");
        assert!(
            matches!(result, Err(ChvError::InvalidArgument { .. })),
            "non-numeric generations should be rejected cleanly"
        );
    }

    #[derive(Default)]
    struct MockTelemetryService {
        node_reports: Arc<Mutex<Vec<proto::NodeStateReport>>>,
        events: Arc<Mutex<Vec<proto::PublishEventRequest>>>,
        migration_reports: Arc<Mutex<Vec<proto::MigrationProgress>>>,
        /// When set, `report_migration_progress` stalls this long before
        /// responding — used to pin that the per-tick drain dispatches
        /// OUTSIDE the cache lock.
        progress_delay_ms: Arc<std::sync::atomic::AtomicU64>,
        /// Set at entry to `report_migration_progress`, before any
        /// stall — lets tests synchronize on "a dispatch is in
        /// flight" instead of guessing with a fixed sleep.
        progress_started: Arc<std::sync::atomic::AtomicBool>,
    }

    #[tonic::async_trait]
    impl TelemetryService for MockTelemetryService {
        async fn report_node_state(
            &self,
            request: Request<proto::NodeStateReport>,
        ) -> Result<Response<proto::AckResponse>, Status> {
            self.node_reports.lock().unwrap().push(request.into_inner());
            Ok(Response::new(proto::AckResponse {
                result: Some(proto::ResultMeta {
                    operation_id: "node-report".to_string(),
                    status: "ok".to_string(),
                    node_observed_generation: String::new(),
                    error_code: String::new(),
                    human_summary: "ok".to_string(),
                }),
            }))
        }

        async fn report_vm_state(
            &self,
            _request: Request<proto::VmStateReport>,
        ) -> Result<Response<proto::AckResponse>, Status> {
            Ok(Response::new(proto::AckResponse {
                result: Some(proto::ResultMeta {
                    operation_id: "vm-report".to_string(),
                    status: "ok".to_string(),
                    node_observed_generation: String::new(),
                    error_code: String::new(),
                    human_summary: "ok".to_string(),
                }),
            }))
        }

        async fn report_volume_state(
            &self,
            _request: Request<proto::VolumeStateReport>,
        ) -> Result<Response<proto::AckResponse>, Status> {
            Ok(Response::new(proto::AckResponse {
                result: Some(proto::ResultMeta {
                    operation_id: "volume-report".to_string(),
                    status: "ok".to_string(),
                    node_observed_generation: String::new(),
                    error_code: String::new(),
                    human_summary: "ok".to_string(),
                }),
            }))
        }

        async fn report_network_state(
            &self,
            _request: Request<proto::NetworkStateReport>,
        ) -> Result<Response<proto::AckResponse>, Status> {
            Ok(Response::new(proto::AckResponse {
                result: Some(proto::ResultMeta {
                    operation_id: "network-report".to_string(),
                    status: "ok".to_string(),
                    node_observed_generation: String::new(),
                    error_code: String::new(),
                    human_summary: "ok".to_string(),
                }),
            }))
        }

        async fn publish_event(
            &self,
            request: Request<proto::PublishEventRequest>,
        ) -> Result<Response<proto::AckResponse>, Status> {
            self.events.lock().unwrap().push(request.into_inner());
            Ok(Response::new(proto::AckResponse {
                result: Some(proto::ResultMeta {
                    operation_id: "event-report".to_string(),
                    status: "ok".to_string(),
                    node_observed_generation: String::new(),
                    error_code: String::new(),
                    human_summary: "ok".to_string(),
                }),
            }))
        }

        async fn publish_alert(
            &self,
            _request: Request<proto::PublishAlertRequest>,
        ) -> Result<Response<proto::AckResponse>, Status> {
            Ok(Response::new(proto::AckResponse {
                result: Some(proto::ResultMeta {
                    operation_id: "alert-report".to_string(),
                    status: "ok".to_string(),
                    node_observed_generation: String::new(),
                    error_code: String::new(),
                    human_summary: "ok".to_string(),
                }),
            }))
        }

        async fn report_migration_progress(
            &self,
            request: Request<proto::MigrationProgress>,
        ) -> Result<Response<proto::AckResponse>, Status> {
            let delay_ms = self
                .progress_delay_ms
                .load(std::sync::atomic::Ordering::SeqCst);
            self.progress_started
                .store(true, std::sync::atomic::Ordering::SeqCst);
            if delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
            self.migration_reports
                .lock()
                .unwrap()
                .push(request.into_inner());
            Ok(Response::new(proto::AckResponse {
                result: Some(proto::ResultMeta {
                    operation_id: "migration-progress".to_string(),
                    status: "ok".to_string(),
                    node_observed_generation: String::new(),
                    error_code: String::new(),
                    human_summary: "ok".to_string(),
                }),
            }))
        }
    }

    #[derive(Default)]
    struct MockInventoryService;

    #[tonic::async_trait]
    impl InventoryService for MockInventoryService {
        async fn report_node_inventory(
            &self,
            _request: Request<proto::ReportNodeInventoryRequest>,
        ) -> Result<Response<proto::AckResponse>, Status> {
            Ok(Response::new(proto::AckResponse {
                result: Some(proto::ResultMeta {
                    operation_id: "inventory-report".to_string(),
                    status: "ok".to_string(),
                    node_observed_generation: String::new(),
                    error_code: String::new(),
                    human_summary: "ok".to_string(),
                }),
            }))
        }

        async fn report_service_versions(
            &self,
            _request: Request<proto::ReportServiceVersionsRequest>,
        ) -> Result<Response<proto::AckResponse>, Status> {
            Ok(Response::new(proto::AckResponse {
                result: Some(proto::ResultMeta {
                    operation_id: "versions-report".to_string(),
                    status: "ok".to_string(),
                    node_observed_generation: String::new(),
                    error_code: String::new(),
                    human_summary: "ok".to_string(),
                }),
            }))
        }
    }

    /// #582: the per-tick drain is what actually delivers the agent's
    /// queued migration progress during a stable connection (the
    /// queue was previously flushed only on reconnect). This pins the
    /// wiring's contract: a queued MigrationProgress is dispatched,
    /// the queue is emptied, the cache is persisted, and the client
    /// is kept for the next tick.
    #[tokio::test]
    async fn drain_pending_control_plane_queue_delivers_queued_migration_progress() {
        let telemetry = MockTelemetryService::default();
        let migration_reports = telemetry.migration_reports.clone();
        let inventory = MockInventoryService;

        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let bound_addr = listener.local_addr().unwrap();

        let (tx, rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let server = tonic::transport::Server::builder()
                .layer(chv_observability::GrpcMetricsLayer::new())
                .add_service(TelemetryServiceServer::new(telemetry))
                .add_service(InventoryServiceServer::new(inventory))
                .serve_with_incoming_shutdown(
                    tokio_stream::wrappers::TcpListenerStream::new(listener),
                    async {
                        rx.await.ok();
                    },
                );
            server.await.unwrap();
        });

        let client = ControlPlaneClient::new(format!("http://{}", bound_addr), None, None, None)
            .await
            .unwrap();

        // The shape the agent_server migration reporter produces:
        // progress enqueued into the cache's pending queue. `node_id` is
        // deliberately empty: this is what an entry persisted by an agent
        // build that predates field 9 looks like after an upgrade, and the
        // drain must fill it from the cache identity instead of sending a
        // value the control plane rejects.
        let mut cache_inner = NodeCache::new("node-1");
        cache_inner.enqueue_pending_message(PendingControlPlaneMessage::migration_progress(
            proto::MigrationProgress {
                vm_id: "vm-1".to_string(),
                operation_id: "op-1".to_string(),
                phase: proto::MigrationPhase::ConvergingDisk as i32,
                bytes_transferred: 2_097_152,
                total_bytes: 10_737_418_240,
                convergence_round: 1,
                dirty_blocks_remaining: 500,
                progress_percent: 45.0,
                node_id: String::new(),
            },
        ));
        let cache = Arc::new(tokio::sync::Mutex::new(cache_inner));

        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("cache.json");
        let mut connectivity = crate::connectivity::ConnectivityTracker::new();

        let telemetry_client =
            drain_pending_control_plane_queue(&cache, &cache_path, Some(client), &mut connectivity)
                .await;

        assert!(
            telemetry_client.is_some(),
            "a successful drain must keep the client for the next tick"
        );
        assert!(cache
            .lock()
            .await
            .pending_control_plane_messages()
            .is_empty());
        assert_eq!(migration_reports.lock().unwrap().len(), 1);
        assert_eq!(connectivity.consecutive_failures(), 0);

        // The delivered message must be the migration progress itself,
        // not just any message.
        let delivered = migration_reports.lock().unwrap()[0].clone();
        assert_eq!(delivered.vm_id, "vm-1");
        assert_eq!(delivered.operation_id, "op-1");
        assert_eq!(delivered.convergence_round, 1);
        assert_eq!(delivered.dirty_blocks_remaining, 500);
        assert_eq!(
            delivered.node_id, "node-1",
            "an empty legacy node_id must be filled from the agent's own identity"
        );

        // The next tick drains an empty queue: no-op — the client is
        // kept, nothing further is dispatched, no connectivity change,
        // no cache write. (This is the hot path for every idle tick
        // in production.) Remove the file the first drain persisted so
        // a no-op write would be visible.
        std::fs::remove_file(&cache_path).unwrap();
        let telemetry_client = drain_pending_control_plane_queue(
            &cache,
            &cache_path,
            telemetry_client,
            &mut connectivity,
        )
        .await;
        assert!(
            telemetry_client.is_some(),
            "an empty queue must keep the client"
        );
        assert!(cache
            .lock()
            .await
            .pending_control_plane_messages()
            .is_empty());
        assert_eq!(migration_reports.lock().unwrap().len(), 1);
        assert_eq!(connectivity.consecutive_failures(), 0);
        assert!(
            !cache_path.exists(),
            "an empty queue must not write the cache file"
        );

        let _ = tx.send(());
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server).await;
    }

    /// #582 failure path: when dispatch fails, the drain must drop the
    /// client (so a later tick reconnects), record a connectivity
    /// failure, and leave the message queued for the reconnect flush.
    #[tokio::test]
    async fn drain_pending_control_plane_queue_drops_client_and_requeues_on_failure() {
        let telemetry = MockTelemetryService::default();
        let inventory = MockInventoryService;

        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let bound_addr = listener.local_addr().unwrap();

        let (tx, rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let server = tonic::transport::Server::builder()
                .layer(chv_observability::GrpcMetricsLayer::new())
                .add_service(TelemetryServiceServer::new(telemetry))
                .add_service(InventoryServiceServer::new(inventory))
                .serve_with_incoming_shutdown(
                    tokio_stream::wrappers::TcpListenerStream::new(listener),
                    async {
                        rx.await.ok();
                    },
                );
            server.await.unwrap();
        });

        let client = ControlPlaneClient::new(format!("http://{}", bound_addr), None, None, None)
            .await
            .unwrap();

        // Kill the control plane before draining: the next dispatch
        // must fail (connection refused on the closed listener).
        let _ = tx.send(());
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server).await;

        let mut cache_inner = NodeCache::new("node-1");
        cache_inner.enqueue_pending_message(PendingControlPlaneMessage::migration_progress(
            proto::MigrationProgress {
                vm_id: "vm-1".to_string(),
                operation_id: "op-1".to_string(),
                phase: proto::MigrationPhase::ConvergingDisk as i32,
                bytes_transferred: 2_097_152,
                total_bytes: 10_737_418_240,
                convergence_round: 1,
                dirty_blocks_remaining: 500,
                progress_percent: 45.0,
                node_id: "node-1".to_string(),
            },
        ));
        let cache = Arc::new(tokio::sync::Mutex::new(cache_inner));

        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("cache.json");
        let mut connectivity = crate::connectivity::ConnectivityTracker::new();

        let drained = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            drain_pending_control_plane_queue(&cache, &cache_path, Some(client), &mut connectivity),
        )
        .await
        .expect("the failed drain must terminate (bounded reconnect backoff)");

        assert!(
            drained.is_none(),
            "a failed drain must drop the client so a later tick reconnects"
        );
        assert_eq!(
            cache.lock().await.pending_control_plane_messages().len(),
            1,
            "the message must stay queued for the reconnect flush"
        );
        assert_eq!(connectivity.consecutive_failures(), 1);
    }

    /// #582 deadline pin: a stalled (but established) control-plane
    /// connection must not hang the per-tick drain forever — without
    /// the per-dispatch deadline the failure path would never run,
    /// so the agent would never record the connectivity failure or
    /// drop the client to reconnect, wedging the tick loop. The
    /// dispatch deadline is shrunk through the private helper (the
    /// production value is 10 s) so the stall outlives it without
    /// waiting real seconds.
    #[tokio::test]
    async fn drain_pending_control_plane_queue_times_out_a_stalled_dispatch() {
        let telemetry = MockTelemetryService::default();
        telemetry
            .progress_delay_ms
            .store(400, std::sync::atomic::Ordering::SeqCst);
        let inventory = MockInventoryService;

        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let bound_addr = listener.local_addr().unwrap();

        let (tx, rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let server = tonic::transport::Server::builder()
                .layer(chv_observability::GrpcMetricsLayer::new())
                .add_service(TelemetryServiceServer::new(telemetry))
                .add_service(InventoryServiceServer::new(inventory))
                .serve_with_incoming_shutdown(
                    tokio_stream::wrappers::TcpListenerStream::new(listener),
                    async {
                        rx.await.ok();
                    },
                );
            server.await.unwrap();
        });

        let client = ControlPlaneClient::new(format!("http://{}", bound_addr), None, None, None)
            .await
            .unwrap();

        let mut cache_inner = NodeCache::new("node-1");
        cache_inner.enqueue_pending_message(PendingControlPlaneMessage::migration_progress(
            proto::MigrationProgress {
                vm_id: "vm-1".to_string(),
                operation_id: "op-1".to_string(),
                phase: proto::MigrationPhase::ConvergingDisk as i32,
                bytes_transferred: 2_097_152,
                total_bytes: 10_737_418_240,
                convergence_round: 1,
                dirty_blocks_remaining: 500,
                progress_percent: 45.0,
                node_id: "node-1".to_string(),
            },
        ));
        let cache = Arc::new(tokio::sync::Mutex::new(cache_inner));

        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("cache.json");
        let mut connectivity = crate::connectivity::ConnectivityTracker::new();

        // A 50 ms deadline against a 400 ms stall: the drain must
        // terminate on its own (the outer bound only catches a hang),
        // take the failure path, and re-queue the message.
        let started = std::time::Instant::now();
        let drained = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            drain_pending_control_plane_queue_with_timeout(
                &cache,
                &cache_path,
                Some(client),
                &mut connectivity,
                std::time::Duration::from_millis(50),
            ),
        )
        .await
        .expect("the stalled drain must terminate via its dispatch deadline");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the drain must fail fast on a stalled dispatch, not wait out the stall"
        );

        assert!(
            drained.is_none(),
            "a timed-out drain must drop the client so a later tick reconnects"
        );
        assert_eq!(
            cache.lock().await.pending_control_plane_messages().len(),
            1,
            "the message must stay queued for the reconnect flush"
        );
        assert_eq!(connectivity.consecutive_failures(), 1);

        let _ = tx.send(());
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server).await;
    }

    /// #582 lock-scope pin: the per-tick drain must dispatch OUTSIDE
    /// the cache lock. The inner dispatch calls have no timeout of
    /// their own, so a slow or half-open control plane would wedge
    /// every other cache consumer (the migration reporter's
    /// try_lock-or-skip enqueue, and the agent-server RPC handlers)
    /// for the duration of the RPCs if the lock were held across
    /// them. With the control plane stalling the report, a reporter
    /// must still be able to enqueue, and the in-flight message must
    /// be delivered while the newer one stays queued for the next
    /// tick (the post-dispatch commit must not clobber it).
    #[tokio::test]
    async fn drain_pending_control_plane_queue_dispatches_outside_the_cache_lock() {
        let telemetry = MockTelemetryService::default();
        telemetry
            .progress_delay_ms
            .store(400, std::sync::atomic::Ordering::SeqCst);
        let migration_reports = telemetry.migration_reports.clone();
        let progress_started = telemetry.progress_started.clone();
        let inventory = MockInventoryService;

        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let bound_addr = listener.local_addr().unwrap();

        let (tx, rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let server = tonic::transport::Server::builder()
                .layer(chv_observability::GrpcMetricsLayer::new())
                .add_service(TelemetryServiceServer::new(telemetry))
                .add_service(InventoryServiceServer::new(inventory))
                .serve_with_incoming_shutdown(
                    tokio_stream::wrappers::TcpListenerStream::new(listener),
                    async {
                        rx.await.ok();
                    },
                );
            server.await.unwrap();
        });

        let client = ControlPlaneClient::new(format!("http://{}", bound_addr), None, None, None)
            .await
            .unwrap();

        let make_progress = |vm: &'static str| {
            PendingControlPlaneMessage::migration_progress(proto::MigrationProgress {
                vm_id: vm.to_string(),
                operation_id: "op-1".to_string(),
                phase: proto::MigrationPhase::ConvergingDisk as i32,
                bytes_transferred: 2_097_152,
                total_bytes: 10_737_418_240,
                convergence_round: 1,
                dirty_blocks_remaining: 500,
                progress_percent: 45.0,
                node_id: "node-1".to_string(),
            })
        };

        let mut cache_inner = NodeCache::new("node-1");
        cache_inner.enqueue_pending_message(make_progress("vm-in-flight"));
        let cache = Arc::new(tokio::sync::Mutex::new(cache_inner));

        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("cache.json");

        // Start the drain; the dispatch stalls 400ms in the mock.
        let cache_for_drain = cache.clone();
        let drain = tokio::spawn(async move {
            let mut connectivity = crate::connectivity::ConnectivityTracker::new();
            drain_pending_control_plane_queue(
                &cache_for_drain,
                &cache_path,
                Some(client),
                &mut connectivity,
            )
            .await
        });

        // Wait until the drain is provably mid-dispatch (the mock has
        // entered report_migration_progress and is stalling) — NOT a
        // fixed sleep, which an overloaded CI runner could schedule
        // around. Once the dispatch has begun, the queue snapshot has
        // necessarily already happened, so this try_lock plus enqueue
        // exactly models the migration reporter racing a live drain.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !progress_started.load(std::sync::atomic::Ordering::SeqCst) {
            assert!(
                std::time::Instant::now() < deadline,
                "the drain never started dispatching"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let enqueued_during_dispatch = match cache.try_lock() {
            Ok(mut locked) => {
                locked.enqueue_pending_message(make_progress("vm-enqueued-mid-drain"));
                true
            }
            Err(_) => false,
        };

        let drained = tokio::time::timeout(std::time::Duration::from_secs(10), drain)
            .await
            .expect("the drain must finish despite the stalled dispatch")
            .expect("drain task must not panic");

        assert!(
            enqueued_during_dispatch,
            "the cache lock must be free while the drain dispatches — \
             holding it across the RPCs would starve the migration \
             reporter and the agent-server handlers"
        );
        assert!(
            drained.is_some(),
            "a successful drain must keep the client for the next tick"
        );
        // The in-flight message was delivered; the one enqueued mid-drain
        // stays queued for the next tick (the commit must not clobber it).
        assert_eq!(migration_reports.lock().unwrap().len(), 1);
        assert_eq!(migration_reports.lock().unwrap()[0].vm_id, "vm-in-flight");
        let queued = cache.lock().await.pending_control_plane_messages().to_vec();
        assert_eq!(
            queued.len(),
            1,
            "the mid-drain enqueue must survive the commit"
        );

        let _ = tx.send(());
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server).await;
    }
}
