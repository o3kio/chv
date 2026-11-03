use chv_errors::ChvError;
use control_plane_node_api::control_plane_node_api as proto;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::net::UnixStream;
use tokio::time::timeout;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;
use tracing::Instrument;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

struct MethodCircuit {
    state: CircuitState,
    failures: Vec<Instant>,
    opened_at: Option<Instant>,
    /// Limits HalfOpen state to a single concurrent probe request.
    probe_in_flight: bool,
}

pub struct CircuitBreaker {
    inner: Mutex<HashMap<String, MethodCircuit>>,
    failure_threshold: usize,
    failure_window: Duration,
    open_duration: Duration,
}

impl CircuitBreaker {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            failure_threshold: 5,
            failure_window: Duration::from_secs(30),
            open_duration: Duration::from_secs(30),
        }
    }

    pub fn check(&self, method: &str) -> Result<(), ChvError> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let entry = inner
            .entry(method.to_string())
            .or_insert_with(|| MethodCircuit {
                state: CircuitState::Closed,
                failures: Vec::new(),
                opened_at: None,
                probe_in_flight: false,
            });

        match entry.state {
            CircuitState::Closed => Ok(()),
            CircuitState::Open => {
                if let Some(opened_at) = entry.opened_at {
                    if now.duration_since(opened_at) >= self.open_duration {
                        entry.state = CircuitState::HalfOpen;
                        entry.opened_at = None;
                        entry.probe_in_flight = true;
                        Ok(())
                    } else {
                        Err(ChvError::BackendUnavailable {
                            backend: "agent".to_string(),
                            reason: format!("circuit breaker open for {method}"),
                        })
                    }
                } else {
                    entry.state = CircuitState::HalfOpen;
                    entry.probe_in_flight = true;
                    Ok(())
                }
            }
            CircuitState::HalfOpen => {
                if entry.probe_in_flight {
                    // Only one probe request allowed in HalfOpen state
                    Err(ChvError::BackendUnavailable {
                        backend: "agent".to_string(),
                        reason: format!("circuit breaker half-open probe in flight for {method}"),
                    })
                } else {
                    entry.probe_in_flight = true;
                    Ok(())
                }
            }
        }
    }

    pub fn record_success(&self, method: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = inner.get_mut(method) {
            entry.state = CircuitState::Closed;
            entry.failures.clear();
            entry.opened_at = None;
            entry.probe_in_flight = false;
        }
    }

    pub fn record_failure(&self, method: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let entry = inner
            .entry(method.to_string())
            .or_insert_with(|| MethodCircuit {
                state: CircuitState::Closed,
                failures: Vec::new(),
                opened_at: None,
                probe_in_flight: false,
            });

        match entry.state {
            CircuitState::HalfOpen => {
                entry.state = CircuitState::Open;
                entry.opened_at = Some(now);
                entry.probe_in_flight = false;
                metrics::counter!(
                    "chv_node_client_circuit_breaker_trips_total",
                    "method" => method.to_string(),
                )
                .increment(1);
            }
            CircuitState::Closed => {
                entry
                    .failures
                    .retain(|&t| now.duration_since(t) < self.failure_window);
                entry.failures.push(now);
                if entry.failures.len() >= self.failure_threshold {
                    entry.state = CircuitState::Open;
                    entry.opened_at = Some(now);
                    entry.failures.clear();
                    metrics::counter!(
                        "chv_node_client_circuit_breaker_trips_total",
                        "method" => method.to_string(),
                    )
                    .increment(1);
                }
            }
            CircuitState::Open => {}
        }
    }
}

async fn with_timeout<F, T>(future: F, backend: &str, method: &str) -> Result<T, ChvError>
where
    F: std::future::Future<Output = Result<tonic::Response<T>, tonic::Status>>,
{
    timeout(Duration::from_secs(30), future)
        .await
        .map_err(|_| ChvError::BackendUnavailable {
            backend: backend.to_string(),
            reason: format!("{method} timed out after 30s"),
        })?
        .map_err(|e| {
            // Preserve the error's identity for gRPC UNIMPLEMENTED (#378
            // §7 fast-fail piece): it is terminal-class for this method on
            // this peer — a verbatim retry can never succeed — so callers
            // (the orchestrator's dispatch-retry machinery) must be able to
            // distinguish it from retryable failures without string-matching
            // the status text. The message text is kept byte-identical to
            // the Internal flattening below so the agent's refusal
            // explanation (e.g. "snapshot_volume is unsupported in
            // core-managed mode") still rides the error. Every other tonic
            // code flattens exactly as before.
            if e.code() == tonic::Code::Unimplemented {
                ChvError::Unimplemented {
                    reason: format!("{method} failed: {e}"),
                }
            } else {
                ChvError::Internal {
                    reason: format!("{method} failed: {e}"),
                }
            }
        })
        .map(|r| r.into_inner())
}

#[derive(Clone)]
pub struct NodeClient {
    reconcile: proto::reconcile_service_client::ReconcileServiceClient<Channel>,
    lifecycle: proto::lifecycle_service_client::LifecycleServiceClient<Channel>,
    circuit_breaker: Arc<CircuitBreaker>,
    /// Control-plane agent bearer token attached to every RPC (#662). One
    /// value is shared by every agent this control plane serves. `None` is
    /// only used by transport-health probes and tests that do not issue
    /// RPCs; a real dispatch client always carries the configured token.
    auth_token: Option<Arc<str>>,
}

impl NodeClient {
    pub async fn connect(socket_path: &Path) -> Result<Self, ChvError> {
        Self::connect_with_breaker(socket_path, Arc::new(CircuitBreaker::new())).await
    }

    /// Connect with an explicit agent authentication token (#662).
    pub async fn connect_with_token(socket_path: &Path, token: &str) -> Result<Self, ChvError> {
        Self::connect_with_breaker_and_token(
            socket_path,
            Arc::new(CircuitBreaker::new()),
            Some(token),
        )
        .await
    }

    pub async fn connect_with_breaker(
        socket_path: &Path,
        circuit_breaker: Arc<CircuitBreaker>,
    ) -> Result<Self, ChvError> {
        Self::connect_with_breaker_and_token(socket_path, circuit_breaker, None).await
    }

    pub async fn connect_with_breaker_and_token(
        socket_path: &Path,
        circuit_breaker: Arc<CircuitBreaker>,
        token: Option<&str>,
    ) -> Result<Self, ChvError> {
        let path = socket_path.to_path_buf();
        let channel = Endpoint::try_from("http://[::]:50051")
            .map_err(|e| ChvError::InvalidArgument {
                field: "node_socket".to_string(),
                reason: e.to_string(),
            })?
            .connect_with_connector(service_fn(move |_: Uri| {
                let p = path.clone();
                async move {
                    let stream = UnixStream::connect(p).await?;
                    Ok::<_, std::io::Error>(hyper_util::rt::tokio::TokioIo::new(stream))
                }
            }))
            .await
            .map_err(|e| ChvError::BackendUnavailable {
                backend: "agent".to_string(),
                reason: e.to_string(),
            })?;
        Ok(Self {
            reconcile: proto::reconcile_service_client::ReconcileServiceClient::new(
                channel.clone(),
            ),
            lifecycle: proto::lifecycle_service_client::LifecycleServiceClient::new(channel),
            circuit_breaker,
            auth_token: token
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(Arc::<str>::from),
        })
    }

    pub async fn apply_vm_desired_state(
        &mut self,
        node_id: &str,
        vm_id: &str,
        generation: &str,
        spec_json: Vec<u8>,
        operation_id: &str,
        requested_by: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::ApplyVmDesiredStateRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: generation.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            vm_id: vm_id.to_string(),
            fragment: Some(proto::DesiredStateFragment {
                id: vm_id.to_string(),
                kind: "vm".to_string(),
                generation: generation.to_string(),
                spec_json,
                policy_json: vec![],
                updated_at: now_iso(),
                updated_by: requested_by.unwrap_or("control-plane").to_string(),
            }),
        };
        let method = "apply_vm_desired_state";
        let span = tracing::info_span!("apply_vm_desired_state", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.reconcile
                .apply_vm_desired_state(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    pub async fn apply_volume_desired_state(
        &mut self,
        node_id: &str,
        volume_id: &str,
        generation: &str,
        spec_json: Vec<u8>,
        operation_id: &str,
        requested_by: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::ApplyVolumeDesiredStateRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: generation.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            volume_id: volume_id.to_string(),
            fragment: Some(proto::DesiredStateFragment {
                id: volume_id.to_string(),
                kind: "volume".to_string(),
                generation: generation.to_string(),
                spec_json,
                policy_json: vec![],
                updated_at: now_iso(),
                updated_by: requested_by.unwrap_or("control-plane").to_string(),
            }),
        };
        let method = "apply_volume_desired_state";
        let span = tracing::info_span!("apply_volume_desired_state", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.reconcile
                .apply_volume_desired_state(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    pub async fn apply_network_desired_state(
        &mut self,
        node_id: &str,
        network_id: &str,
        generation: &str,
        spec_json: Vec<u8>,
        operation_id: &str,
        requested_by: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::ApplyNetworkDesiredStateRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: generation.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            network_id: network_id.to_string(),
            fragment: Some(proto::DesiredStateFragment {
                id: network_id.to_string(),
                kind: "network".to_string(),
                generation: generation.to_string(),
                spec_json,
                policy_json: vec![],
                updated_at: now_iso(),
                updated_by: requested_by.unwrap_or("control-plane").to_string(),
            }),
        };
        let method = "apply_network_desired_state";
        let span = tracing::info_span!("apply_network_desired_state", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.reconcile
                .apply_network_desired_state(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    pub async fn create_vm(
        &mut self,
        node_id: &str,
        vm_id: &str,
        generation: &str,
        vm_spec_json: Vec<u8>,
        operation_id: &str,
        requested_by: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::CreateVmRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: generation.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            vm: Some(proto::VmMutationSpec {
                vm_id: vm_id.to_string(),
                vm_spec_json,
            }),
        };
        let method = "create_vm";
        let span = tracing::info_span!("create_vm", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.lifecycle
                .create_vm(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    pub async fn start_vm(
        &mut self,
        node_id: &str,
        vm_id: &str,
        generation: &str,
        operation_id: &str,
        requested_by: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::StartVmRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: generation.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            vm_id: vm_id.to_string(),
        };
        let method = "start_vm";
        let span = tracing::info_span!("start_vm", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.lifecycle
                .start_vm(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    pub async fn stop_vm(
        &mut self,
        node_id: &str,
        vm_id: &str,
        generation: &str,
        force: bool,
        operation_id: &str,
        requested_by: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::StopVmRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: generation.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            vm_id: vm_id.to_string(),
            force,
        };
        let method = "stop_vm";
        let span = tracing::info_span!("stop_vm", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.lifecycle
                .stop_vm(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    pub async fn reboot_vm(
        &mut self,
        node_id: &str,
        vm_id: &str,
        generation: &str,
        force: bool,
        operation_id: &str,
        requested_by: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::RebootVmRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: generation.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            vm_id: vm_id.to_string(),
            force,
        };
        let method = "reboot_vm";
        let span = tracing::info_span!("reboot_vm", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.lifecycle
                .reboot_vm(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    pub async fn delete_vm(
        &mut self,
        node_id: &str,
        vm_id: &str,
        generation: &str,
        force: bool,
        operation_id: &str,
        requested_by: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::DeleteVmRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: generation.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            vm_id: vm_id.to_string(),
            force,
        };
        let method = "delete_vm";
        let span = tracing::info_span!("delete_vm", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.lifecycle
                .delete_vm(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    pub async fn snapshot_vm(
        &mut self,
        node_id: &str,
        vm_id: &str,
        generation: &str,
        destination: &str,
        operation_id: &str,
        requested_by: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::SnapshotVmRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: generation.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            vm_id: vm_id.to_string(),
            destination: destination.to_string(),
        };
        let method = "snapshot_vm";
        let span = tracing::info_span!("snapshot_vm", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.lifecycle
                .snapshot_vm(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    pub async fn restore_snapshot(
        &mut self,
        node_id: &str,
        vm_id: &str,
        generation: &str,
        source: &str,
        operation_id: &str,
        requested_by: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::RestoreSnapshotRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: generation.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            vm_id: vm_id.to_string(),
            source: source.to_string(),
        };
        let method = "restore_snapshot";
        let span = tracing::info_span!("restore_snapshot", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.lifecycle
                .restore_snapshot(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn attach_volume(
        &mut self,
        node_id: &str,
        volume_id: &str,
        vm_id: &str,
        generation: &str,
        operation_id: &str,
        requested_by: Option<&str>,
        backend_class: Option<&str>,
        volume_kind: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::AttachVolumeRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: generation.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            volume: Some(proto::VolumeMutationSpec {
                volume_id: volume_id.to_string(),
                vm_id: vm_id.to_string(),
                // #379 PR 2 (A8): the volume's class rides the attach
                // dispatch — see [`volume_attach_spec_json`]. #533: the
                // volume's KIND rides it too, so a standalone volume's
                // attach opens at the #513 create carrier's locator
                // instead of the bare-id default's second file.
                volume_spec_json: volume_attach_spec_json(volume_id, backend_class, volume_kind),
            }),
        };
        let method = "attach_volume";
        let span = tracing::info_span!("attach_volume", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.lifecycle
                .attach_volume(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    /// #513 PR 1 (DP2): dispatch the standalone volume-create carrier.
    /// Dead-but-live with this PR — the only future caller is the
    /// orchestrator's `"CreateVolume"` arm, and no producer journals
    /// that operation type until the BFF route lands (PR 2). The
    /// `volume_spec_json` is built by [`volume_create_spec_json`]: the
    /// requested capacity (REQUIRED — the agent's open provisions the
    /// backing store; the attach path's option-less open cannot) plus
    /// the class through the documented #511 `backend_class` wire-key
    /// seam (NULL = local = no key materialized).
    #[allow(clippy::too_many_arguments)]
    pub async fn create_volume(
        &mut self,
        node_id: &str,
        volume_id: &str,
        size_bytes: u64,
        generation: &str,
        operation_id: &str,
        requested_by: Option<&str>,
        backend_class: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::CreateVolumeRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: generation.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            volume: Some(proto::VolumeMutationSpec {
                volume_id: volume_id.to_string(),
                // Standalone volumes have no VM (DP4 defers
                // attach-at-create); the field is part of the shared
                // mutation-spec shape, unused here.
                vm_id: String::new(),
                volume_spec_json: volume_create_spec_json(size_bytes, backend_class),
            }),
        };
        let method = "create_volume";
        let span = tracing::info_span!("create_volume", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.lifecycle
                .create_volume(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    /// #522 PR 1 (DP2): dispatch the volume-delete carrier. Dead-but-
    /// live with this PR — the only future caller is the
    /// orchestrator's `"DeleteVolume"` arm, and no producer journals
    /// that operation type until the BFF route lands (PR 2). The
    /// volume's class rides the dispatch (resolved in the claim query
    /// like the attach arm's) so the agent can shape the DP4 carrier
    /// locator; a NULL class emits the empty string, never a
    /// materialized `"local"` (the #511 wire-key discipline). No size
    /// rides a delete, and the destroy is idempotent by stord's
    /// contract, so a redriven dispatch re-acks.
    pub async fn delete_volume(
        &mut self,
        node_id: &str,
        volume_id: &str,
        generation: &str,
        operation_id: &str,
        requested_by: Option<&str>,
        backend_class: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::DeleteVolumeRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: generation.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            volume_id: volume_id.to_string(),
            backend_class: backend_class.unwrap_or("").to_string(),
        };
        let method = "delete_volume";
        let span = tracing::info_span!("delete_volume", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.lifecycle
                .delete_volume(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    /// #355 (PR 1 of the decomposition): dispatch the network's stored
    /// firewall ruleset to one node's agent. `policy_json` is the NDS
    /// `firewall_rules_json` verbatim; `generation` rides
    /// `meta.desired_state_version` (the DP7 fence, enforced from PR 3).
    /// Dead-but-live with this PR: no producer journals an
    /// `UpdateNetworkPolicy` operation until the BFF route lands (PR 2).
    pub async fn apply_network_policy(
        &mut self,
        node_id: &str,
        network_id: &str,
        generation: &str,
        policy_json: &[u8],
        operation_id: &str,
        requested_by: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::ApplyNetworkPolicyRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: generation.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            network_id: network_id.to_string(),
            policy_json: policy_json.to_vec(),
        };
        let method = "apply_network_policy";
        let span = tracing::info_span!("apply_network_policy", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.lifecycle
                .apply_network_policy(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    /// Relays the operator's terminal resolution of a restart-interrupted
    /// (`InspectRequired`) operation to the owning agent's core journal.
    /// Pure relay: the agent validates disposition/note and owns the
    /// terminal persistence; the control plane stays a desired-state
    /// authority.
    pub async fn resolve_inspect_required_operation(
        &mut self,
        node_id: &str,
        vm_id: &str,
        operation_id: &str,
        disposition: &str,
        note: &str,
        requested_by: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::ResolveInspectRequiredOperationRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: String::new(),
                request_unix_ms: now_unix_ms(),
            }),
            vm_id: vm_id.to_string(),
            operation_id: operation_id.to_string(),
            disposition: disposition.to_string(),
            note: note.to_string(),
        };
        let method = "resolve_inspect_required_operation";
        let span = tracing::info_span!("resolve_inspect_required_operation", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.lifecycle
                .resolve_inspect_required_operation(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn detach_volume(
        &mut self,
        node_id: &str,
        volume_id: &str,
        vm_id: &str,
        generation: &str,
        force: bool,
        operation_id: &str,
        requested_by: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::DetachVolumeRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: generation.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            vm_id: vm_id.to_string(),
            volume_id: volume_id.to_string(),
            force,
        };
        let method = "detach_volume";
        let span = tracing::info_span!("detach_volume", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.lifecycle
                .detach_volume(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    pub async fn resize_volume(
        &mut self,
        node_id: &str,
        volume_id: &str,
        generation: &str,
        new_size_bytes: u64,
        operation_id: &str,
        requested_by: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::ResizeVolumeRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: generation.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            volume_id: volume_id.to_string(),
            new_size_bytes,
        };
        let method = "resize_volume";
        let span = tracing::info_span!("resize_volume", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.lifecycle
                .resize_volume(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    pub async fn snapshot_volume(
        &mut self,
        node_id: &str,
        volume_id: &str,
        generation: &str,
        snapshot_name: &str,
        operation_id: &str,
        requested_by: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::SnapshotVolumeRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: generation.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            volume_id: volume_id.to_string(),
            snapshot_name: snapshot_name.to_string(),
        };
        let method = "snapshot_volume";
        let span = tracing::info_span!("snapshot_volume", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.lifecycle
                .snapshot_volume(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    pub async fn restore_volume(
        &mut self,
        node_id: &str,
        volume_id: &str,
        generation: &str,
        snapshot_name: &str,
        operation_id: &str,
        requested_by: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::RestoreVolumeRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: generation.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            volume_id: volume_id.to_string(),
            snapshot_name: snapshot_name.to_string(),
        };
        let method = "restore_volume";
        let span = tracing::info_span!("restore_volume", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.lifecycle
                .restore_volume(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    pub async fn delete_volume_snapshot(
        &mut self,
        node_id: &str,
        volume_id: &str,
        generation: &str,
        snapshot_name: &str,
        operation_id: &str,
        requested_by: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::DeleteVolumeSnapshotRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: generation.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            volume_id: volume_id.to_string(),
            snapshot_name: snapshot_name.to_string(),
        };
        let method = "delete_volume_snapshot";
        let span = tracing::info_span!("delete_volume_snapshot", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.lifecycle
                .delete_volume_snapshot(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    pub async fn clone_volume(
        &mut self,
        node_id: &str,
        source_volume_id: &str,
        target_volume_id: &str,
        generation: &str,
        operation_id: &str,
        requested_by: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::CloneVolumeRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: generation.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            source_volume_id: source_volume_id.to_string(),
            target_volume_id: target_volume_id.to_string(),
        };
        let method = "clone_volume";
        let span = tracing::info_span!("clone_volume", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.lifecycle
                .clone_volume(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    pub async fn start_network(
        &mut self,
        node_id: &str,
        network_id: &str,
        generation: &str,
        operation_id: &str,
        requested_by: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::StartNetworkRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: generation.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            network_id: network_id.to_string(),
        };
        let method = "start_network";
        let span = tracing::info_span!("start_network", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.lifecycle
                .start_network(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    pub async fn stop_network(
        &mut self,
        node_id: &str,
        network_id: &str,
        generation: &str,
        force: bool,
        operation_id: &str,
        requested_by: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::StopNetworkRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: generation.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            network_id: network_id.to_string(),
            force,
        };
        let method = "stop_network";
        let span = tracing::info_span!("stop_network", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.lifecycle
                .stop_network(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    pub async fn restart_network(
        &mut self,
        node_id: &str,
        network_id: &str,
        generation: &str,
        operation_id: &str,
        requested_by: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::RestartNetworkRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: generation.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            network_id: network_id.to_string(),
        };
        let method = "restart_network";
        let span = tracing::info_span!("restart_network", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.lifecycle
                .restart_network(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    pub async fn pause_vm(
        &mut self,
        node_id: &str,
        vm_id: &str,
        generation: &str,
        operation_id: &str,
        requested_by: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::PauseVmRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: generation.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            vm_id: vm_id.to_string(),
        };
        let method = "pause_vm";
        let span = tracing::info_span!("pause_vm", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.lifecycle
                .pause_vm(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    pub async fn resume_vm(
        &mut self,
        node_id: &str,
        vm_id: &str,
        generation: &str,
        operation_id: &str,
        requested_by: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::ResumeVmRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: generation.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            vm_id: vm_id.to_string(),
        };
        let method = "resume_vm";
        let span = tracing::info_span!("resume_vm", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.lifecycle
                .resume_vm(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    pub async fn power_button_vm(
        &mut self,
        node_id: &str,
        vm_id: &str,
        generation: &str,
        operation_id: &str,
        requested_by: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::PowerButtonVmRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: generation.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            vm_id: vm_id.to_string(),
        };
        let method = "power_button_vm";
        let span = tracing::info_span!("power_button_vm", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.lifecycle
                .power_button_vm(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    pub async fn coredump_vm(
        &mut self,
        node_id: &str,
        vm_id: &str,
        generation: &str,
        destination: &str,
        operation_id: &str,
        requested_by: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::CoredumpVmRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: generation.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            vm_id: vm_id.to_string(),
            destination: destination.to_string(),
        };
        let method = "coredump_vm";
        let span = tracing::info_span!("coredump_vm", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.lifecycle
                .coredump_vm(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn migrate_vm(
        &mut self,
        node_id: &str,
        vm_id: &str,
        generation: &str,
        source_node_id: &str,
        destination_node_id: &str,
        config: proto::MigrationConfig,
        operation_id: &str,
        requested_by: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::MigrateVmRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: generation.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            vm_id: vm_id.to_string(),
            source_node_id: source_node_id.to_string(),
            destination_node_id: destination_node_id.to_string(),
            config: Some(config),
        };
        let method = "migrate_vm";
        let span = tracing::info_span!("migrate_vm", operation_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.lifecycle
                .migrate_vm(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    /// Dispatch an ADR-021 fabric plan to a node agent's UpdateOverlay RPC.
    ///
    /// The legacy pre-ADR-021 VTEP/FDB variants were retired; `fabric` is
    /// required (nwd rejects a fabric-less UpdateOverlay in-band).
    #[allow(clippy::too_many_arguments)]
    pub async fn update_overlay(
        &mut self,
        node_id: &str,
        network_id: &str,
        vni: u32,
        operation_id: &str,
        requested_by: Option<&str>,
        fabric: Option<proto::FabricPlan>,
        desired_state_version: &str,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::UpdateOverlayRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: desired_state_version.to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            network_id: network_id.to_string(),
            vni,
            // Deprecated legacy fields (pre-ADR-021): never populated.
            vtep_endpoints: Vec::new(),
            fdb_entries: Vec::new(),
            fabric,
        };
        let method = "update_overlay";
        let span = tracing::info_span!("update_overlay", operation_id, network_id);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.lifecycle
                .update_overlay(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }

    pub async fn send_gratuitous_arp(
        &mut self,
        node_id: &str,
        network_id: &str,
        vm_ip: &str,
        bridge_name: &str,
        operation_id: &str,
        requested_by: Option<&str>,
    ) -> Result<proto::AckResponse, ChvError> {
        let req = proto::SendGratuitousArpRequest {
            meta: Some(proto::RequestMeta {
                operation_id: operation_id.to_string(),
                requested_by: requested_by.unwrap_or("control-plane").to_string(),
                target_node_id: node_id.to_string(),
                desired_state_version: "".to_string(),
                request_unix_ms: now_unix_ms(),
            }),
            node_id: node_id.to_string(),
            network_id: network_id.to_string(),
            vm_ip: vm_ip.to_string(),
            bridge_name: bridge_name.to_string(),
        };
        let method = "send_gratuitous_arp";
        let span = tracing::info_span!("send_gratuitous_arp", operation_id, network_id, vm_ip);
        self.circuit_breaker.check(method)?;
        let result = with_timeout(
            self.lifecycle
                .send_gratuitous_arp(with_operation_id_metadata(
                    req,
                    operation_id,
                    &self.auth_token,
                ))
                .instrument(span),
            "agent",
            method,
        )
        .await;
        match &result {
            Ok(_) => self.circuit_breaker.record_success(method),
            Err(ChvError::BackendUnavailable { .. }) => self.circuit_breaker.record_failure(method),
            Err(_) => {}
        };
        result
    }
}

fn with_operation_id_metadata<T>(
    req: T,
    operation_id: &str,
    auth_token: &Option<Arc<str>>,
) -> tonic::Request<T> {
    let mut grpc_req = tonic::Request::new(req);
    if let Ok(val) = tonic::metadata::MetadataValue::try_from(operation_id) {
        grpc_req
            .metadata_mut()
            .insert(chv_common::OPERATION_ID_METADATA_KEY, val);
    }
    if let Some(token) = auth_token {
        if let Ok(val) = tonic::metadata::MetadataValue::try_from(token.as_ref()) {
            grpc_req
                .metadata_mut()
                .insert(chv_common::NODE_AUTH_METADATA_KEY, val);
        }
    }
    grpc_req
}

fn now_unix_ms() -> i64 {
    chv_common::now_unix_ms()
}

/// Build the `AttachVolume` RPC's `volume_spec_json` payload (#379 PR 2,
/// the A8/A9 shared producer; NULL-class bytes corrected in PR 3; the
/// #533 standalone locator key added on top).
///
/// The agent's attach handler (A4) parses `backend_class` and `locator`
/// out of this JSON, defaulting both. PR 2 populated ONLY
/// `{"backend_class": …}` and only when the volume carries a class:
///
/// - **NULL class → empty bytes** — byte-exact with the pre-#379
///   dispatch (every existing volume), so the NULL = local contract
///   holds end-to-end and the string `"local"` is never materialized
///   into the payload (the agent's B5 default resolves it instead).
/// - **class present → `{"backend_class": "<class>"}`** — no `locator`
///   key, so A4's default locator applies; the class-dependent dm-path
///   locator convention is DP5, deliberately scoped to PR 3.
///
/// **PR 3 correction (disclosed, design §2.1):** the NULL-class leg now
/// emits `{}` instead of empty bytes. PR 2 disclosed that the empty
/// payload does not parse at the agent's A4 seam (serde_json `from_slice`
/// on empty bytes is an EOF error — the design doc's "the parser always
/// takes the defaults" was inaccurate at the letter), and chose to
/// preserve the pre-existing behavior rather than change it inside the
/// carry PR. With LVM-class attach dispatch now real (DP5), a NULL-class
/// attach must actually reach the open, so the payload is the empty JSON
/// object: still no `backend_class` key, still no `"local"` string
/// materialized — the agent's B5 default resolves it, and A4's explicit
/// `locator` key remains absent.
///
/// **#533 (the #513 design's DP2 locator guard, §8):** a STANDALONE
/// volume (`volume_kind = 'data'` — the #513 DP8 stamp, the same
/// discriminator the #522 delete's kind gate rides) now carries the
/// CARRIER's relative `{volume_id}.img` locator, because the A4
/// parser's non-LVM default is the bare volume id — an option-less
/// create-on-open at `runtime_dir/{volume_id}` that mints a SECOND
/// default-size file and permanently orphans the file the #513 create
/// carrier minted (and that the #522 delete's DP4 destroy targets
/// exactly, by design). LVM is the one class that keeps NO locator
/// key: the agent's LVM default already shapes the carrier's
/// `/dev/mapper/{vg}-{vid}` dm-path token, and an explicit `.img`
/// locator would be the wrong shape for that open. Every embedded
/// volume (NULL kind — all pre-#513 lineage, boot disks, imports,
/// templates) keeps the pre-#533 bytes byte-exactly: no locator key,
/// the A4 bare-id default, the vm-nested A1 path untouched.
pub(crate) fn volume_attach_spec_json(
    volume_id: &str,
    storage_class: Option<&str>,
    volume_kind: Option<&str>,
) -> Vec<u8> {
    let standalone = volume_kind == Some("data");
    let mut payload = serde_json::Map::new();
    if let Some(class) = storage_class {
        payload.insert("backend_class".to_string(), serde_json::json!(class));
    }
    if standalone && storage_class != Some("lvm") {
        payload.insert(
            "locator".to_string(),
            serde_json::json!(format!("{}.img", volume_id)),
        );
    }
    // Infallible for these string-valued keys; an empty fallback would
    // merely mean the agent's default path.
    serde_json::to_vec(&serde_json::Value::Object(payload)).unwrap_or_default()
}

/// Build the `CreateVolume` RPC's `volume_spec_json` payload (#513 PR 1,
/// DP2/DP3 — the dispatch-carrier producer; the agent half of the pin is
/// `create_volume_parses_the_cp_spec_json_shape` in chv-agent-core).
///
/// The agent's create handler parses `size_bytes` and `backend_class`
/// out of this JSON. The two keys are the whole contract:
///
/// - **`size_bytes` is always present** — the load-bearing #513
///   finding: the local backend's create-on-open only provisions on a
///   sized open, and an LVM open of an absent LV without a size is
///   rejected, so a create that dispatches without a size mints a
///   volume the attach path can never materialize (journal-only is not
///   viable; design §2.4).
/// - **NULL class → no `backend_class` key** — the same never-materialize-
///   `"local"` discipline as [`volume_attach_spec_json`] (the #511
///   wire-key seam: the CP-side store name is `storage_class`, the
///   agent-bound key is `backend_class`); the agent's absent-field
///   default resolves local.
pub(crate) fn volume_create_spec_json(size_bytes: u64, storage_class: Option<&str>) -> Vec<u8> {
    match storage_class {
        Some(class) => serde_json::to_vec(
            &(serde_json::json!({ "backend_class": class, "size_bytes": size_bytes })),
        )
        .unwrap_or_default(),
        None => {
            serde_json::to_vec(&serde_json::json!({ "size_bytes": size_bytes })).unwrap_or_default()
        }
    }
}

fn now_iso() -> String {
    // RFC 3339-ish format using current unix millis as a simple timestamp string.
    // Sufficient for fragment updated_at; agent does not parse this field.
    let ms = now_unix_ms();
    format!("{ms}")
}

#[cfg(test)]
mod tests {
    use super::{volume_attach_spec_json, volume_create_spec_json, with_operation_id_metadata};
    use std::sync::Arc;

    /// #662: every dispatch request carries both the operation id and the
    /// control-plane agent token, so no RPC can be sent unauthenticated once
    /// the pool is configured.
    #[test]
    fn dispatch_metadata_carries_operation_id_and_agent_token() {
        let token: Option<Arc<str>> = Some(Arc::from("agent-token-0123456789abcdef"));
        let request = with_operation_id_metadata((), "op-42", &token);
        let metadata = request.metadata();
        assert_eq!(
            metadata
                .get(chv_common::OPERATION_ID_METADATA_KEY)
                .expect("operation id")
                .to_str()
                .unwrap(),
            "op-42"
        );
        assert_eq!(
            metadata
                .get(chv_common::NODE_AUTH_METADATA_KEY)
                .expect("agent token")
                .to_str()
                .unwrap(),
            "agent-token-0123456789abcdef"
        );
    }

    /// Token-less clients (health probes / tests) must not emit an empty
    /// credential header: absence is a clean `Unauthenticated`, whereas an
    /// empty value would look like a malformed credential.
    #[test]
    fn dispatch_metadata_omits_absent_token() {
        let request = with_operation_id_metadata((), "op-42", &None);
        assert!(request
            .metadata()
            .get(chv_common::NODE_AUTH_METADATA_KEY)
            .is_none());
    }

    /// #513 PR 1 (DP2/DP3, the dispatch-carrier producer): the create
    /// spec_json is `{"size_bytes": N}` — and ONLY that key — for a
    /// NULL-class volume (the never-materialize-`"local"` discipline
    /// the attach producer set in #379 PR 3), and adds exactly the
    /// `backend_class` key for a class-carrying one (the #511 wire-key
    /// seam). The size key is unconditional: the agent's open cannot
    /// provision without it (design §2.4, the load-bearing finding).
    /// The agent half of the contract pair is
    /// `create_volume_parses_the_cp_spec_json_shape` (chv-agent-core).
    #[test]
    fn volume_create_spec_json_carries_size_and_only_the_class_key() {
        assert_eq!(
            volume_create_spec_json(1073741824, None),
            br#"{"size_bytes":1073741824}"#.to_vec(),
            "a NULL-class create carries exactly the size key — no backend_class, no materialized \"local\""
        );
        assert_eq!(
            volume_create_spec_json(1073741824, Some("lvm")),
            br#"{"backend_class":"lvm","size_bytes":1073741824}"#.to_vec(),
            "a class-carrying create adds exactly the backend_class key"
        );
        // The attach producer's embedded legs are unchanged beside the
        // new one (re-pinned here so the #533 change cannot drift them).
        assert_eq!(volume_attach_spec_json("vol-x", None, None), b"{}".to_vec());
        assert_eq!(
            volume_attach_spec_json("vol-x", Some("lvm"), None),
            br#"{"backend_class":"lvm"}"#.to_vec()
        );
    }

    /// #533 (the #513 design's DP2 locator guard): the attach producer
    /// shapes a STANDALONE volume's (`volume_kind = 'data'`, the #513
    /// DP8 stamp / the #522 delete gate's discriminator) open locator
    /// as the create carrier's relative `{volume_id}.img` — the A4
    /// parser's bare-id default would create-on-open a second
    /// default-size file at `runtime_dir/{volume_id}` and orphan the
    /// carrier-minted one. LVM keeps no locator key (the agent's LVM
    /// default already shapes the carrier's dm-path token); embedded
    /// volumes (NULL kind) keep the pre-#533 bytes byte-exactly.
    #[test]
    fn volume_attach_spec_json_shapes_the_standalone_carrier_locator() {
        // Standalone + NULL class (the #513 route's own row shape):
        // exactly the locator key — no backend_class, no "local".
        assert_eq!(
            volume_attach_spec_json("vol-std", None, Some("data")),
            br#"{"locator":"vol-std.img"}"#.to_vec(),
            "a standalone NULL-class attach carries exactly the carrier locator key"
        );
        // Standalone + LVM: no locator key — the agent's LVM default
        // shapes the carrier's dm-path token itself.
        assert_eq!(
            volume_attach_spec_json("vol-std", Some("lvm"), Some("data")),
            br#"{"backend_class":"lvm"}"#.to_vec(),
            "a standalone LVM attach keeps the class-only shape (the agent's LVM default is the carrier locator)"
        );
        // Standalone + a non-LVM class: the class key AND the locator.
        assert_eq!(
            volume_attach_spec_json("vol-std", Some("ceph"), Some("data")),
            br#"{"backend_class":"ceph","locator":"vol-std.img"}"#.to_vec(),
            "a standalone class-carrying attach adds the carrier locator beside the class"
        );
        // Embedded (NULL kind): byte-exact pre-#533 legs — the A4
        // bare-id default applies, the A1 vm-nested path is untouched.
        assert_eq!(
            volume_attach_spec_json("vol-emb", None, None),
            b"{}".to_vec(),
            "an embedded NULL-class attach stays the empty object"
        );
        assert_eq!(
            volume_attach_spec_json("vol-emb", Some("lvm"), None),
            br#"{"backend_class":"lvm"}"#.to_vec(),
            "an embedded LVM attach stays the class-only shape"
        );
        // A non-'data' kind value (none exists in production today) is
        // NOT standalone: only the DP8 stamp discriminates.
        assert_eq!(
            volume_attach_spec_json("vol-disk", None, Some("disk")),
            b"{}".to_vec(),
            "a non-'data' kind keeps the embedded shape (the DP8 stamp is the discriminator)"
        );
    }
}
