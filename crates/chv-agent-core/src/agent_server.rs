use crate::cache::{NodeCache, VmNicAttachment};
use crate::control_plane::ControlPlaneClient;
use crate::migration_registry::MigrationTaskRegistry;
use crate::reconcile::{bridge_name_for_network, cleanup_vm_resources, vm_runtime_dir};
use crate::state_machine::NodeState;
use crate::vm_runtime::VmRuntime;
use chv_errors::ChvError;
use chv_hypervisor_api::VmConfig;
use control_plane_node_api::control_plane_node_api as proto;
use futures_util::StreamExt as _;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::service::InterceptorLayer;
use tonic::{Request, Response, Status};
use tracing::{info, warn};

/// Interval between agent-auth rejection warnings. Prevents a broken or
/// hostile local client from flooding the log while still surfacing the
/// first failure and a suppressed count on the next interval.
const AUTH_REJECTION_WARN_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Default)]
struct AuthWarnState {
    last_warn: Option<Instant>,
    suppressed: u64,
}

/// Application-layer authentication for the control-plane → agent hop
/// (#662).
///
/// The transport is a local Unix-domain socket (0600), so the filesystem
/// permission check is the first boundary. This layer adds two more:
///
/// 1. Unix peer credentials: only the configured UID set (default: the
///    agent's own effective UID) may complete a connection.
/// 2. The control plane's agent bearer token, carried in gRPC metadata on
///    every RPC and compared in constant time before any handler runs.
///    The control plane has one token and sends it to every agent it
///    serves today; per-node tokens are future ADR-029 work.
///
/// `allow_insecure` is an explicit local-development bypass. It is refused
/// when the config declares `security.production = true` (the default).
#[derive(Clone)]
pub struct AgentAuth {
    token: Option<Arc<str>>,
    allow_insecure: bool,
    allowed_peer_uids: Arc<Vec<u32>>,
    warn_state: Arc<std::sync::Mutex<AuthWarnState>>,
}

impl AgentAuth {
    pub fn new(
        token: impl Into<String>,
        allow_insecure: bool,
        allowed_peer_uids: Vec<u32>,
    ) -> Self {
        let token = token.into();
        let token = token.trim();
        Self {
            token: (!token.is_empty()).then(|| Arc::<str>::from(token)),
            allow_insecure,
            allowed_peer_uids: Arc::new(allowed_peer_uids),
            warn_state: Arc::new(std::sync::Mutex::new(AuthWarnState::default())),
        }
    }

    /// Fail-closed default used by direct constructors and tests that do
    /// not exercise the transport: every request is rejected. Production
    /// startup always installs a concrete token via
    /// [`AgentServer::with_agent_auth`].
    pub fn deny_all() -> Self {
        Self {
            token: None,
            allow_insecure: false,
            allowed_peer_uids: Arc::new(Vec::new()),
            warn_state: Arc::new(std::sync::Mutex::new(AuthWarnState::default())),
        }
    }

    fn peer_uid_allowed(&self, uid: u32) -> bool {
        if self.allow_insecure {
            return true;
        }
        if self.allowed_peer_uids.is_empty() {
            return uid == nix::unistd::geteuid().as_raw();
        }
        self.allowed_peer_uids.contains(&uid)
    }

    /// Log a rejection at most once per [`AUTH_REJECTION_WARN_INTERVAL`].
    /// The `reason` is always a static string; token material is never
    /// logged.
    fn warn_rejection(&self, reason: &'static str) {
        let now = Instant::now();
        let Ok(mut state) = self.warn_state.lock() else {
            // A poisoned lock must not turn a rejected RPC into a panic.
            return;
        };
        let should_warn = state
            .last_warn
            .map(|last| now.duration_since(last) >= AUTH_REJECTION_WARN_INTERVAL)
            .unwrap_or(true);
        if !should_warn {
            state.suppressed = state.suppressed.saturating_add(1);
            return;
        }
        let suppressed = state.suppressed;
        state.last_warn = Some(now);
        state.suppressed = 0;
        drop(state);
        warn!(
            reason,
            suppressed, "agent rejected a control-plane RPC authentication attempt"
        );
    }
}

impl tonic::service::Interceptor for AgentAuth {
    fn call(&mut self, req: Request<()>) -> Result<Request<()>, Status> {
        if self.allow_insecure {
            return Ok(req);
        }
        let Some(expected) = self.token.as_deref() else {
            self.warn_rejection("agent authentication is not configured");
            return Err(Status::unauthenticated(
                "agent authentication is not configured",
            ));
        };
        let Some(presented) = req.metadata().get(chv_common::NODE_AUTH_METADATA_KEY) else {
            self.warn_rejection("missing agent authentication token");
            return Err(Status::unauthenticated(
                "missing agent authentication token",
            ));
        };
        if chv_common::constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
            Ok(req)
        } else {
            self.warn_rejection("invalid agent authentication token");
            Err(Status::unauthenticated(
                "invalid agent authentication token",
            ))
        }
    }
}

#[derive(Clone)]
pub struct AgentServer {
    pub cache: Arc<tokio::sync::Mutex<NodeCache>>,
    pub vm_runtime: VmRuntime,
    pub stord_socket: std::path::PathBuf,
    pub nwd_socket: std::path::PathBuf,
    pub cache_path: Option<std::path::PathBuf>,
    pub runtime_dir: std::path::PathBuf,
    /// #379 DP5: the node's stord backend info (class + LVM volume
    /// group, from `AgentConfig.stord_config_path`) — the source the
    /// LVM locator shaping uses. Defaults to local/no-VG; set via
    /// [`AgentServer::with_stord_backend`].
    pub stord_backend: crate::stord_backend::StordBackendInfo,
    /// Tracks in-flight migration tasks for cancellation and shutdown.
    ///
    /// Before this field existed, `migrate_vm` spawned a `JoinHandle` that
    /// was dropped on the floor — meaning the agent could not abort, reap, or
    /// observe failures of long-running migration tasks. The registry closes
    /// that gap. See [`MigrationTaskRegistry`] and ADR-008 / ADR-009.
    pub migration_tasks: Arc<MigrationTaskRegistry>,
    /// #355 PR 3 (DP7 — dispatch fencing): the last APPLIED network
    /// policy generation per network id. A dispatch whose
    /// `meta.desired_state_version` parses below the recorded
    /// generation is an idempotent no-op Ack, so an out-of-order
    /// fan-out result (or a retry of an older operation after a newer
    /// one succeeded) can never regress a newer policy with an older
    /// one. In-memory by design (nwd's policy_state is not queryable
    /// over RPC): after an agent restart the fence starts empty and
    /// the first dispatch applies — safe, because the CP journals
    /// operations per-generation and retries the newest, and nwd's
    /// apply is idempotent. Entries advance ONLY on successful nwd
    /// applies (a failed generation must not block an older one that
    /// still matches the last applied state).
    pub network_policy_fence: Arc<tokio::sync::Mutex<std::collections::HashMap<String, u64>>>,
    pub core_authority: Option<cellhv_core_operations::AuthorityHandle>,
    /// Inbound control-plane authentication (#662). Deny-all until
    /// production wiring calls [`AgentServer::with_agent_auth`].
    pub auth: AgentAuth,
}

impl AgentServer {
    pub fn new(
        cache: Arc<tokio::sync::Mutex<NodeCache>>,
        vm_runtime: VmRuntime,
        stord_socket: std::path::PathBuf,
        nwd_socket: std::path::PathBuf,
        cache_path: Option<std::path::PathBuf>,
        runtime_dir: std::path::PathBuf,
    ) -> Self {
        Self {
            cache,
            vm_runtime,
            stord_socket,
            nwd_socket,
            cache_path,
            runtime_dir,
            stord_backend: crate::stord_backend::StordBackendInfo::default(),
            migration_tasks: Arc::new(MigrationTaskRegistry::new()),
            network_policy_fence: Arc::new(tokio::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            core_authority: None,
            auth: AgentAuth::deny_all(),
        }
    }

    /// Install the inbound control-plane authentication policy (#662).
    pub fn with_agent_auth(mut self, auth: AgentAuth) -> Self {
        self.auth = auth;
        self
    }

    /// Set the node's stord backend info (#379 DP5): the LVM volume
    /// group LVM-class open locators are shaped against. Mirrors
    /// [`AgentServer::with_core_authority`]'s builder shape.
    pub fn with_stord_backend(
        mut self,
        stord_backend: crate::stord_backend::StordBackendInfo,
    ) -> Self {
        self.stord_backend = stord_backend;
        self
    }

    pub fn with_core_authority(
        mut self,
        authority: cellhv_core_operations::AuthorityHandle,
    ) -> Self {
        self.core_authority = Some(authority);
        self
    }

    /// Cancel a tracked migration task by its `operation_id`.
    ///
    /// Triggers both the cooperative `CancellationToken` (so phase boundaries
    /// in the migration future bail out cleanly) and the `AbortHandle` (so the
    /// future is force-dropped at its next `.await` if it doesn't honor the
    /// token quickly). Returns `true` if a task was found and signalled.
    pub fn cancel_migration(&self, operation_id: &str) -> bool {
        self.migration_tasks.cancel(operation_id)
    }

    /// Abort every in-flight migration task. Used on agent shutdown.
    pub fn shutdown_migrations(&self) {
        self.migration_tasks.abort_all();
    }

    async fn persist_cache(&self, cache: &NodeCache) {
        if let Some(ref path) = self.cache_path {
            if let Err(e) = cache.save(path).await {
                tracing::warn!(error = %e, "failed to persist cache");
            }
        }
    }

    async fn open_and_attach_volume(
        &self,
        volume_id: &str,
        vm_id: &str,
        spec_json: &[u8],
        operation_id: &str,
    ) -> Result<String, Status> {
        let spec = serde_json::from_slice::<serde_json::Value>(spec_json)
            .map_err(|e| Status::invalid_argument(format!("invalid volume spec_json: {}", e)))?;
        let backend_class = spec
            .get("backend_class")
            .and_then(|v| v.as_str())
            .unwrap_or("local");
        // #379 DP5: an LVM-class open carries a /dev/mapper/{vg}-{vid}
        // dm-path locator (VG from the same stord-config source as DP4)
        // so the standard device_allowlist admits it — the backend itself
        // ignores the locator and derives /dev/{vg}/{vid}. Every other
        // class keeps the historical bare-volume-id default.
        let locator = match spec.get("locator").and_then(|v| v.as_str()) {
            Some(explicit) => explicit.to_string(),
            None if backend_class == "lvm" => chv_hypervisor_api::resources::lvm_locator(
                self.stord_backend.volume_group(),
                volume_id,
            ),
            None => volume_id.to_string(),
        };

        let mut stord = crate::daemon_clients::StordClient::connect(&self.stord_socket)
            .await
            .map_err(|e| Status::unavailable(format!("stord unavailable: {}", e)))?;
        let (_, handle, _) = stord
            .open_volume(volume_id, backend_class, &locator, Some(operation_id))
            .await
            .map_err(|e| Status::internal(format!("open_volume failed: {}", e)))?;
        stord
            .attach_volume_to_vm(volume_id, vm_id, &handle, Some(operation_id))
            .await
            .map_err(|e| Status::internal(format!("attach_volume_to_vm failed: {}", e)))?;
        Ok(handle)
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
        // Restrict the socket to the agent's own user; the control plane runs
        // as the same user, so no group access is required. Log a failure
        // rather than silently keeping a wider mode; the peer-UID gate still
        // applies, but operators should know the filesystem boundary is weak.
        if let Err(err) =
            std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))
        {
            warn!(
                error = %err,
                path = %socket_path.display(),
                "failed to restrict agent socket permissions to 0600"
            );
        }
        // Pre-auth peer-credential gate (#662): a peer whose UID is not in
        // the allowlist is dropped before HTTP/2 is spoken, so an
        // unauthorized local process cannot even reach the token check.
        let auth_for_peers = self.auth.clone();
        let uds_stream = UnixListenerStream::new(uds).filter_map(move |item| {
            let auth = auth_for_peers.clone();
            async move {
                match item {
                    Ok(stream) => match stream.peer_cred() {
                        Ok(cred) => {
                            if auth.peer_uid_allowed(cred.uid()) {
                                Some(Ok(stream))
                            } else {
                                warn!(
                                    peer_uid = cred.uid(),
                                    peer_pid = cred.pid(),
                                    "agent rejected Unix peer outside the allowed UID set"
                                );
                                None
                            }
                        }
                        Err(error) => {
                            warn!(
                                error = %error,
                                "agent could not read Unix peer credentials; connection dropped"
                            );
                            None
                        }
                    },
                    Err(error) => Some(Err(error)),
                }
            }
        });
        tonic::transport::Server::builder()
            .layer(chv_observability::GrpcMetricsLayer::new())
            .layer(InterceptorLayer::new(self.auth.clone()))
            .add_service(proto::reconcile_service_server::ReconcileServiceServer::new(self.clone()))
            .add_service(proto::lifecycle_service_server::LifecycleServiceServer::new(self))
            .serve_with_incoming(uds_stream)
            .await
            .map_err(|e| ChvError::Internal {
                reason: format!("agent server error: {e}"),
            })
    }
}

#[tonic::async_trait]
impl proto::reconcile_service_server::ReconcileService for AgentServer {
    async fn apply_node_desired_state(
        &self,
        req: Request<proto::ApplyNodeDesiredStateRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        // M2.2b single-writer enforcement: fail closed in core-managed
        // mode so this legacy node desired-state write side effect can never run behind the
        // Core authority (Core M1 does not model it).
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "apply_node_desired_state is unsupported in core-managed mode",
            ));
        }
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let mut cache = self.cache.lock().await;
        ControlPlaneClient::stale_generation_check(meta, &cache, "node", &inner.node_id)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        if let Some(frag) = inner.fragment {
            cache.observe_generation("node", &inner.node_id, &frag.generation);
            self.persist_cache(&cache).await;
        }
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: cache.observed_generation.clone(),
                error_code: "".to_string(),
                human_summary: "node desired state accepted".to_string(),
            }),
        }))
    }

    async fn apply_vm_desired_state(
        &self,
        req: Request<proto::ApplyVmDesiredStateRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        // M2.5 dispatch shim: in core-managed mode the NodeCache VM axis is
        // a projection of Core execution only, so this legacy desired-state
        // dispatch never writes the cache directly — it routes through the
        // Core authority BEFORE any provider side effect, exactly like the
        // direct lifecycle RPCs. The control plane dispatches both creates
        // and resizes through this entry point; the task's target
        // generation (fixed at accept, retried verbatim) selects the Core
        // command: 1 = CreateVm, anything higher is a spec update.
        if let Some(ref authority) = self.core_authority {
            let inner = req.into_inner();
            if !chv_common::is_safe_id(&inner.vm_id) {
                return Err(Status::invalid_argument("invalid vm_id"));
            }
            let meta = inner
                .meta
                .as_ref()
                .ok_or_else(|| Status::invalid_argument("missing meta"))?;
            let frag = inner
                .fragment
                .as_ref()
                .ok_or_else(|| Status::invalid_argument("missing fragment"))?;
            if frag.id != inner.vm_id || frag.kind != "vm" {
                return Err(Status::invalid_argument(
                    "fragment identity or kind mismatch",
                ));
            }
            // The fragment's generation must agree with the task's: the
            // control plane sets both from the same accept-time value, and
            // the routing decision below must never arbitrate between two
            // disagreeing inputs (a divergence is rejected, not averaged,
            // guessed, or silently dropped).
            if frag.generation != meta.desired_state_version {
                return Err(Status::invalid_argument(
                    "fragment generation must match desired_state_version",
                ));
            }
            // Canonical generation parse BEFORE the stale gate, so a
            // malformed generation is always InvalidArgument — never a
            // stale-gate FailedPrecondition that depends on projection
            // state. The same strict parse the adapter applies to direct
            // mutations (no zero, no leading zeros, no signed or
            // non-canonical forms); generations are control-plane i64
            // sequence values, so anything above i64::MAX is out of range
            // rather than an update task. The routing decision never
            // guesses.
            let generation =
                crate::legacy_core_adapter::parse_generation(&meta.desired_state_version)
                    .map_err(|e| Status::invalid_argument(e.to_string()))?;
            if generation > i64::MAX as u64 {
                return Err(Status::invalid_argument(
                    "desired_state_version is out of range",
                ));
            }
            // Same stale-generation gate as the legacy branch, against the
            // projection (never the authority): a dispatch older than the
            // last projected outcome is rejected before Core sees it. A
            // create retry lands equal to the projection (both "1") and
            // passes; Core's idempotency journal then replays it.
            {
                let cache = self.cache.lock().await;
                ControlPlaneClient::stale_generation_check(meta, &cache, "vm", &inner.vm_id)
                    .map_err(|e| Status::failed_precondition(e.to_string()))?;
            }
            if generation != 1 {
                // A spec update (resize carries the next generation, >= 2).
                // Refused at this boundary BEFORE Core reserves any desired
                // state: the Core executor does not implement UpdateVm
                // (OperationKind::UpdateVm is Unsupported there), so
                // accepting it would journal a definition the runtime
                // cannot converge to. Resize-through-Core is tracked as
                // deferred scope (#234).
                return Err(Status::unimplemented(
                    "desired-state VM updates (generations >= 2, e.g. resize) are unsupported in core-managed mode until the executor implements UpdateVm",
                ));
            }
            let spec =
                crate::spec::VmSpec::from_json(std::str::from_utf8(&frag.spec_json).unwrap_or(""))
                    .map_err(|e| {
                        Status::invalid_argument(format!("invalid fragment spec_json: {}", e))
                    })?;
            let legacy_meta = crate::legacy_core_adapter::LegacyRequestMeta {
                operation_id: meta.operation_id.clone(),
                requested_by: meta.requested_by.clone(),
                target_node_id: meta.target_node_id.clone(),
                desired_state_version: meta.desired_state_version.clone(),
                request_unix_ms: meta.request_unix_ms,
            };
            // #368 C1 create-vs-redrive routing. A generation-1 task for a
            // VM id whose Core journal already holds a create is never a
            // fresh create (the `vms` row is durable — a re-submitted
            // create would PK-conflict): it is a re-drive task, routed on
            // the journal-derived state of the latest create.
            // Coupling with the generation gate above: this routing is
            // reachable only at generation 1 (creates always dispatch at
            // gen 1; only Delete/Resize carry higher generations, which
            // that gate rejects as Unimplemented first). If create-family
            // ops ever dispatch at a higher generation, that gate must be
            // revisited or this re-drive routing becomes unreachable.
            let vm_id = cellhv_core_types::VmId::new(inner.vm_id.clone())
                .map_err(|e| Status::invalid_argument(e.to_string()))?;
            // Task-identity replay comes FIRST: a verbatim retry of a
            // control-plane task (same operation id and generation — what
            // the dispatcher retries) must replay through Core
            // idempotency whatever the latest create's CURRENT state.
            // Routing purely on that state would turn a retry of an
            // in-flight create (or of an already-accepted re-drive) into
            // a FailedPrecondition refusal instead of a replay — a
            // dispatch retry is not a re-drive, and a re-drive retry is
            // not a second re-drive. The two derivations below are the
            // task's own journal identities: the create form every
            // create task journals under, and the `:requeue` form every
            // re-drive task journals under.
            let create_operation_id = crate::legacy_core_adapter::legacy_operation_id(
                &meta.target_node_id,
                &vm_id,
                &meta.operation_id,
            )
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
            let requeue_operation_id = crate::legacy_core_adapter::legacy_requeue_operation_id(
                &meta.target_node_id,
                &vm_id,
                &meta.operation_id,
            )
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
            // A verbatim retry of a re-drive task: replay the requeue
            // (its idempotency key converges on the one requeued
            // operation, whatever state that operation has since
            // reached).
            if journaled_operation_exists(authority, requeue_operation_id).await? {
                return Ok(Response::new(
                    dispatch_create_redrive(self, authority, &legacy_meta, &inner.vm_id, &vm_id)
                        .await?,
                ));
            }
            // A verbatim retry of the original create task: replay the
            // submission through Core idempotency — the journal answers
            // whether the create is in flight, inspect-required, or
            // already terminal; the shim does not guess and never
            // inserts a second create.
            if journaled_operation_exists(authority, create_operation_id).await? {
                let intent = crate::legacy_core_adapter::adapt_legacy_vm_mutation(
                    &legacy_meta,
                    &meta.target_node_id,
                    crate::legacy_core_adapter::LegacyVmMutation::Create {
                        vm_id: inner.vm_id.clone(),
                        spec: Box::new(spec),
                    },
                    // The submission pins version 1 like the fresh-create
                    // path; on replay the idempotency lookup short-circuits
                    // before any version check, so the pin only matters
                    // for a task the journal has never seen.
                    cellhv_core_types::ResourceVersion::new(1)
                        .expect("resource version 1 is representable"),
                )
                .map_err(|e| Status::invalid_argument(e.to_string()))?;
                let accepted = authority
                    .submit(intent.submission)
                    .await
                    .map_err(map_authority_error)?;
                return Ok(Response::new(proto::AckResponse {
                    result: Some(proto::ResultMeta {
                        operation_id: meta.operation_id.clone(),
                        status: "ok".to_string(),
                        node_observed_generation: self
                            .cache
                            .lock()
                            .await
                            .observed_generation
                            .clone(),
                        error_code: "".to_string(),
                        human_summary: format!("{:?}", accepted.disposition),
                    }),
                }));
            }
            let latest = authority
                .latest_create_state(vm_id.clone())
                .await
                .map_err(map_authority_error)?;
            match latest.map(|state| state.status) {
                // No journaled create for this id: the original path —
                // submit a fresh create pinned at Core version 1.
                None => {}
                // The latest create terminally failed: re-drive it through
                // the requeue primitive. The definition is re-derived by
                // the store from the live journal row (the spec in this
                // fragment is the same generation-1 spec the original
                // create journaled — it is deliberately not re-converted).
                Some(cellhv_core_types::OperationStatus::Failed) => {
                    return Ok(Response::new(
                        dispatch_create_redrive(
                            self,
                            authority,
                            &legacy_meta,
                            &inner.vm_id,
                            &vm_id,
                        )
                        .await?,
                    ));
                }
                // The create already converged: a duplicate dispatch is an
                // idempotent success (the desired state is journaled and
                // projected — there is nothing to re-drive).
                Some(cellhv_core_types::OperationStatus::Succeeded) => {
                    return Ok(Response::new(proto::AckResponse {
                        result: Some(proto::ResultMeta {
                            operation_id: meta.operation_id.clone(),
                            status: "ok".to_string(),
                            node_observed_generation: self
                                .cache
                                .lock()
                                .await
                                .observed_generation
                                .clone(),
                            error_code: "".to_string(),
                            human_summary: "create already converged".to_string(),
                        }),
                    }));
                }
                // A terminally unsupported create is permanent (the Core
                // requeue only re-drives `failed`): refuse visibly instead
                // of looping re-drives the store would refuse anyway.
                Some(cellhv_core_types::OperationStatus::Unsupported) => {
                    return Err(Status::failed_precondition(
                        "create for this VM terminally failed as unsupported; \
                         resolve the spec before retrying",
                    ));
                }
                // An incomplete create (in flight, or inspect-required
                // after a crash): never re-driven concurrently. The
                // control plane retries the dispatch later; an
                // inspect-required create stays resolvable through the
                // operator resolve RPC.
                Some(cellhv_core_types::OperationStatus::Accepted)
                | Some(cellhv_core_types::OperationStatus::Running) => {
                    return Err(Status::failed_precondition(
                        "create for this VM is incomplete (in flight or inspect-required); \
                         retry after it resolves",
                    ));
                }
            }
            let intent = crate::legacy_core_adapter::adapt_legacy_vm_mutation(
                &legacy_meta,
                &meta.target_node_id,
                crate::legacy_core_adapter::LegacyVmMutation::Create {
                    vm_id: inner.vm_id.clone(),
                    spec: Box::new(spec),
                },
                // A generation-1 task targets a VM Core has never accepted;
                // the create submission pins expected version 1 (validated
                // by Core; a duplicate create surfaces as already_exists).
                cellhv_core_types::ResourceVersion::new(1)
                    .expect("resource version 1 is representable"),
            )
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
            let accepted = authority
                .submit(intent.submission)
                .await
                .map_err(map_authority_error)?;
            return Ok(Response::new(proto::AckResponse {
                result: Some(proto::ResultMeta {
                    operation_id: meta.operation_id.clone(),
                    status: "ok".to_string(),
                    node_observed_generation: self.cache.lock().await.observed_generation.clone(),
                    error_code: "".to_string(),
                    human_summary: format!("{:?}", accepted.disposition),
                }),
            }));
        }
        let inner = req.into_inner();
        if !chv_common::is_safe_id(&inner.vm_id) {
            return Err(Status::invalid_argument("invalid vm_id"));
        }
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let mut cache = self.cache.lock().await;
        ControlPlaneClient::stale_generation_check(meta, &cache, "vm", &inner.vm_id)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        if let Some(frag) = inner.fragment {
            cache.observe_generation("vm", &inner.vm_id, &frag.generation);
            cache.store_fragment(
                "vm",
                &inner.vm_id,
                crate::cache::DesiredStateFragment {
                    id: frag.id,
                    kind: frag.kind,
                    generation: frag.generation,
                    spec_json: frag.spec_json,
                    policy_json: frag.policy_json,
                    updated_at: frag.updated_at,
                    updated_by: frag.updated_by,
                },
            );
            self.persist_cache(&cache).await;
        }
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: cache.observed_generation.clone(),
                error_code: "".to_string(),
                human_summary: "vm desired state accepted".to_string(),
            }),
        }))
    }

    async fn apply_volume_desired_state(
        &self,
        req: Request<proto::ApplyVolumeDesiredStateRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        // M2.2b single-writer enforcement: this legacy path performs a direct
        // stord open+attach provider side effect and writes the cache behind
        // the Core authority's back. In core-managed mode it must fail closed
        // so volume lifecycle is enforced solely through Core execution.
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "apply_volume_desired_state is unsupported in core-managed mode",
            ));
        }
        let inner = req.into_inner();
        if !chv_common::is_safe_id(&inner.volume_id) {
            return Err(Status::invalid_argument("invalid volume_id"));
        }
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;

        let spec_json = {
            let mut cache = self.cache.lock().await;
            ControlPlaneClient::stale_generation_check(meta, &cache, "volume", &inner.volume_id)
                .map_err(|e| Status::failed_precondition(e.to_string()))?;
            let mut spec_json = None;
            if let Some(frag) = inner.fragment {
                cache.observe_generation("volume", &inner.volume_id, &frag.generation);
                spec_json = Some(frag.spec_json.clone());
                cache.store_fragment(
                    "volume",
                    &inner.volume_id,
                    crate::cache::DesiredStateFragment {
                        id: frag.id,
                        kind: frag.kind,
                        generation: frag.generation,
                        spec_json: frag.spec_json,
                        policy_json: frag.policy_json,
                        updated_at: frag.updated_at,
                        updated_by: frag.updated_by,
                    },
                );
                self.persist_cache(&cache).await;
            }
            spec_json
            // lock dropped here
        };

        if let Some(spec_json) = spec_json {
            let spec = serde_json::from_slice::<serde_json::Value>(&spec_json)
                .map_err(|e| Status::invalid_argument(format!("invalid spec_json: {}", e)))?;
            if let Some(vm_id) = spec
                .get("vm_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                let handle = self
                    .open_and_attach_volume(&inner.volume_id, vm_id, &spec_json, &meta.operation_id)
                    .await?;

                let mut cache = self.cache.lock().await;
                cache.volume_handles.insert(inner.volume_id.clone(), handle);
                cache.observe_vm_attachment(vm_id, std::slice::from_ref(&inner.volume_id), &[]);
                self.persist_cache(&cache).await;
            }
        }

        let observed_generation = self.cache.lock().await.observed_generation.clone();
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "volume desired state accepted".to_string(),
            }),
        }))
    }

    async fn apply_network_desired_state(
        &self,
        req: Request<proto::ApplyNetworkDesiredStateRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        // M2.2b single-writer enforcement: this legacy path performs nwd
        // provider side effects and writes the network axis behind the Core
        // authority's back. In core-managed mode it must fail closed.
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "apply_network_desired_state is unsupported in core-managed mode",
            ));
        }
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;

        let (spec_json_for_nwd, observed_generation) = {
            let mut cache = self.cache.lock().await;
            ControlPlaneClient::stale_generation_check(meta, &cache, "network", &inner.network_id)
                .map_err(|e| Status::failed_precondition(e.to_string()))?;
            let mut spec_json_out = None;
            if let Some(ref frag) = inner.fragment {
                cache.observe_generation("network", &inner.network_id, &frag.generation);
                cache.store_fragment(
                    "network",
                    &inner.network_id,
                    crate::cache::DesiredStateFragment {
                        id: frag.id.clone(),
                        kind: frag.kind.clone(),
                        generation: frag.generation.clone(),
                        spec_json: frag.spec_json.clone(),
                        policy_json: frag.policy_json.clone(),
                        updated_at: frag.updated_at.clone(),
                        updated_by: frag.updated_by.clone(),
                    },
                );
                self.persist_cache(&cache).await;
                spec_json_out = Some(frag.spec_json.clone());
            }
            (spec_json_out, cache.observed_generation.clone())
            // lock dropped here
        };

        if let Some(spec_json) = spec_json_for_nwd {
            let spec = match serde_json::from_slice::<serde_json::Value>(&spec_json) {
                Ok(v) => v,
                Err(e) => {
                    warn!(
                        network_id = %inner.network_id,
                        error = %e,
                        fragment = %String::from_utf8_lossy(&spec_json),
                        "failed to parse network spec_json, falling back to defaults (bridge=br0, cidr=10.0.0.0/24)"
                    );
                    serde_json::Value::default()
                }
            };
            let bridge = spec
                .get("bridge_name")
                .and_then(|v| v.as_str())
                .unwrap_or("br0");
            let cidr = spec
                .get("cidr")
                .and_then(|v| v.as_str())
                .unwrap_or("10.0.0.0/24");
            let gateway = spec.get("gateway").and_then(|v| v.as_str()).unwrap_or("");

            let mut nwd = crate::daemon_clients::NwdClient::connect(&self.nwd_socket)
                .await
                .map_err(|e| Status::unavailable(format!("nwd unavailable: {}", e)))?;

            nwd.ensure_network_topology(
                &inner.network_id,
                bridge,
                cidr,
                gateway,
                Some(&meta.operation_id),
            )
            .await
            .map_err(|e| Status::internal(format!("ensure_network_topology failed: {}", e)))?;

            if let Some(exposures) = spec.get("exposures").and_then(|v| v.as_array()) {
                for exp in exposures {
                    let eid = exp
                        .get("exposure_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let proto_str = exp
                        .get("protocol")
                        .and_then(|v| v.as_str())
                        .unwrap_or("tcp");
                    let ext_port = exp
                        .get("external_port")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    let tip = exp.get("target_ip").and_then(|v| v.as_str()).unwrap_or("");
                    let tport = exp.get("target_port").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                    let mode = exp.get("mode").and_then(|v| v.as_str()).unwrap_or("nat");
                    if !eid.is_empty() {
                        if let Err(e) = nwd
                            .expose_service(
                                &inner.network_id,
                                eid,
                                proto_str,
                                ext_port,
                                tip,
                                tport,
                                mode,
                                Some(&meta.operation_id),
                            )
                            .await
                        {
                            warn!(network_id = %inner.network_id, exposure_id = %eid, error = %e, "failed to expose service");
                        }
                    }
                }
            }

            // Firewall policy
            if let Some(rules) = spec.get("firewall_rules") {
                let fw_op_id = format!("{}-firewall", meta.operation_id);
                let policy_json = serde_json::to_vec(rules).unwrap_or_default();
                // #360: only dispatch a SEMANTICALLY non-empty ruleset —
                // nwd's engine engages default-deny even for an empty
                // ruleset, which would cut a rule-less network's guests
                // off entirely (including DHCP). #355 PR 3 note: the
                // core-managed paths no longer share this predicate —
                // they resolve the DP4 baseline for empty rulesets —
                // but the legacy path keeps the #360 skip (design §6:
                // legacy paths unchanged).
                if chv_common::firewall_ruleset_is_empty(&String::from_utf8_lossy(&policy_json)) {
                    warn!(network_id = %inner.network_id, "skipping empty firewall ruleset: applying it would engage default-deny with zero allows");
                } else if let Err(e) = nwd
                    .set_firewall_policy(&inner.network_id, "v1", policy_json, Some(&fw_op_id))
                    .await
                {
                    warn!(network_id = %inner.network_id, error = %e, "failed to set firewall policy");
                }
            }

            // NAT policy
            if spec
                .get("nat_enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                let nat_op_id = format!("{}-nat", meta.operation_id);
                let policy_json = spec
                    .get("nat_rules")
                    .map(|v| serde_json::to_vec(v).unwrap_or_default())
                    .unwrap_or_default();
                if let Err(e) = nwd
                    .set_nat_policy(&inner.network_id, "v1", policy_json, Some(&nat_op_id))
                    .await
                {
                    warn!(network_id = %inner.network_id, error = %e, "failed to set NAT policy");
                }
            }

            // DHCP scope
            if spec
                .get("dhcp_enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                if let Some(scope) = spec.get("dhcp_scope") {
                    let dhcp_op_id = format!("{}-dhcp", meta.operation_id);
                    let range_start = scope
                        .get("range_start")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let range_end = scope
                        .get("range_end")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let dns_servers: Vec<String> = scope
                        .get("dns_servers")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    if let Err(e) = nwd
                        .ensure_dhcp_scope(
                            &inner.network_id,
                            cidr,
                            range_start,
                            range_end,
                            dns_servers,
                            Some(&dhcp_op_id),
                        )
                        .await
                    {
                        warn!(network_id = %inner.network_id, error = %e, "failed to ensure DHCP scope");
                    }
                }
            }

            // DNS scope
            if spec
                .get("dns_enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                if let Some(scope) = spec.get("dns_scope") {
                    let dns_op_id = format!("{}-dns", meta.operation_id);
                    let forwarders: Vec<String> = scope
                        .get("forwarders")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    let static_records: std::collections::HashMap<String, String> = scope
                        .get("static_records")
                        .and_then(|v| v.as_object())
                        .map(|obj| {
                            obj.iter()
                                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                                .collect()
                        })
                        .unwrap_or_default();
                    if let Err(e) = nwd
                        .ensure_dns_scope(
                            &inner.network_id,
                            forwarders,
                            static_records,
                            Some(&dns_op_id),
                        )
                        .await
                    {
                        warn!(network_id = %inner.network_id, error = %e, "failed to ensure DNS scope");
                    }
                }
            }
        }
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "network desired state accepted".to_string(),
            }),
        }))
    }

    async fn acknowledge_desired_state_version(
        &self,
        req: Request<proto::AcknowledgeDesiredStateVersionRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let cache = self.cache.lock().await;
        let observed = cache
            .get_generation(&inner.fragment_kind, &inner.fragment_id)
            .cloned()
            .unwrap_or_default();
        if observed != inner.observed_generation {
            return Err(Status::failed_precondition(format!(
                "generation mismatch: observed {}, got {}",
                observed, inner.observed_generation
            )));
        }
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: cache.observed_generation.clone(),
                error_code: "".to_string(),
                human_summary: "desired state version acknowledged".to_string(),
            }),
        }))
    }
}

/// Whether the Core journal already holds an operation under this id —
/// the task-identity probe for the #368 create-vs-redrive routing. A
/// missing operation is `false`; every other authority failure surfaces.
async fn journaled_operation_exists(
    authority: &cellhv_core_operations::AuthorityHandle,
    id: cellhv_core_types::OperationId,
) -> Result<bool, Status> {
    match authority.operation(id).await {
        Ok(_) => Ok(true),
        Err(cellhv_core_operations::AuthorityActorError::Service(err))
            if err.class() == cellhv_core_operations::ErrorClass::NotFound =>
        {
            Ok(false)
        }
        Err(e) => Err(map_authority_error(e)),
    }
}

/// #368 C1: submit a create re-drive through the Core requeue primitive
/// and build the legacy ack. This is also the replay path for a verbatim
/// retry of a re-drive task: the requeue's own idempotency key converges
/// on the one requeued operation instead of inserting another.
async fn dispatch_create_redrive(
    server: &AgentServer,
    authority: &cellhv_core_operations::AuthorityHandle,
    legacy_meta: &crate::legacy_core_adapter::LegacyRequestMeta,
    vm_id_str: &str,
    vm_id: &cellhv_core_types::VmId,
) -> Result<proto::AckResponse, Status> {
    // The requeue CASes the live row's version: read it, and let a raced
    // mutation surface as a retryable failed-precondition instead of
    // guessing.
    let live = authority
        .vm(vm_id.clone())
        .await
        .map_err(map_authority_error)?;
    let redrive = crate::legacy_core_adapter::adapt_legacy_create_redrive(
        legacy_meta,
        &legacy_meta.target_node_id,
        vm_id_str,
        live.resource_version,
    )
    .map_err(|e| Status::invalid_argument(e.to_string()))?;
    let accepted = authority
        .requeue_failed_create(redrive)
        .await
        .map_err(map_authority_error)?;
    Ok(proto::AckResponse {
        result: Some(proto::ResultMeta {
            operation_id: legacy_meta.operation_id.clone(),
            status: "ok".to_string(),
            node_observed_generation: server.cache.lock().await.observed_generation.clone(),
            error_code: "".to_string(),
            human_summary: format!("{:?}", accepted.disposition),
        }),
    })
}

/// Maps a core-operation submit error into a gRPC [`Status`] for the legacy
/// lifecycle handlers.
///
/// Every legacy lifecycle handler (create_vm, start_vm, stop_vm, reboot_vm,
/// delete_vm) submits through [`cellhv_core_operations::AuthorityHandle`].
/// M2.1b metadata validation (empty `requested_by`, `request_unix_ms <= 0`)
/// surfaces as [`cellhv_core_operations::ErrorClass::Invalid`] here, so this
/// helper guarantees such validation errors are reported to legacy clients as
/// `invalid_argument` instead of a 500 for *every* lifecycle handler — not
/// just the ones that happen to carry the `Invalid` arm inline.
fn map_authority_error(e: cellhv_core_operations::AuthorityActorError) -> Status {
    match e {
        cellhv_core_operations::AuthorityActorError::Service(err) => match err.class() {
            cellhv_core_operations::ErrorClass::Invalid => {
                Status::invalid_argument(err.to_string())
            }
            cellhv_core_operations::ErrorClass::NotFound => Status::not_found(err.to_string()),
            cellhv_core_operations::ErrorClass::Conflict => Status::already_exists(err.to_string()),
            cellhv_core_operations::ErrorClass::Precondition => {
                Status::failed_precondition(err.to_string())
            }
            _ => Status::internal(err.to_string()),
        },
        // The actor is draining (shutdown) or its queue is gone: the caller
        // resolved ambiguity by retrying the same idempotent request, so
        // surface retryability instead of a 500.
        cellhv_core_operations::AuthorityActorError::Unavailable => {
            Status::unavailable("core authority is shutting down; retry the idempotent request")
        }
        _ => Status::internal(e.to_string()),
    }
}

/// Maps a control-plane `FabricPlan` (chv.controlplane.node.v1) to the nwd
/// `FabricPlan` (chv.node.nwd.v1) for the update_overlay relay, field by
/// field including every peer (ADR-021 plan carriage).
fn fabric_plan_to_nwd(plan: &proto::FabricPlan) -> chv_nwd_api::chv_nwd_api::FabricPlan {
    chv_nwd_api::chv_nwd_api::FabricPlan {
        fabric_domain_id: plan.fabric_domain_id.clone(),
        local_host_id: plan.local_host_id.clone(),
        local_fabric_ip: plan.local_fabric_ip.clone(),
        tenant_mtu: plan.tenant_mtu,
        fabric_mtu: plan.fabric_mtu,
        binding_generation: plan.binding_generation,
        plan_generation: plan.plan_generation,
        peers: plan
            .peers
            .iter()
            .map(|peer| chv_nwd_api::chv_nwd_api::FabricPeer {
                node_id: peer.node_id.clone(),
                public_key: peer.public_key.clone(),
                underlay_endpoint: peer.underlay_endpoint.clone(),
                fabric_ip: peer.fabric_ip.clone(),
            })
            .collect(),
    }
}

/// Resolves the current core-journal version of a VM for an expected-version
/// CAS. The authority error is mapped by class: a VM unknown to the core
/// journal is a `not_found`, an authority outage is `unavailable`, and
/// internal authority failures are `internal` — never a silently guessed
/// version 1, which would turn an authority outage into a misleading
/// stale-version rejection.
async fn authority_vm_version_or_status(
    authority: &cellhv_core_operations::AuthorityHandle,
    vm_id: cellhv_core_types::VmId,
) -> Result<cellhv_core_types::ResourceVersion, Status> {
    match authority.vm(vm_id.clone()).await {
        Ok(vm) => Ok(vm.resource_version),
        Err(error) => Err(map_authority_error(error)),
    }
}

#[tonic::async_trait]
impl proto::lifecycle_service_server::LifecycleService for AgentServer {
    async fn create_vm(
        &self,
        req: Request<proto::CreateVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        if let Some(ref authority) = self.core_authority {
            let inner = req.into_inner();
            let meta = inner
                .meta
                .as_ref()
                .ok_or_else(|| Status::invalid_argument("missing meta"))?;
            let vm = inner
                .vm
                .as_ref()
                .ok_or_else(|| Status::invalid_argument("missing vm"))?;
            let legacy_meta = crate::legacy_core_adapter::LegacyRequestMeta {
                operation_id: meta.operation_id.clone(),
                requested_by: meta.requested_by.clone(),
                target_node_id: meta.target_node_id.clone(),
                desired_state_version: meta.desired_state_version.clone(),
                request_unix_ms: meta.request_unix_ms,
            };
            let spec =
                crate::spec::VmSpec::from_json(std::str::from_utf8(&vm.vm_spec_json).unwrap_or(""))
                    .map_err(|e| {
                        Status::invalid_argument(format!("invalid vm_spec_json: {}", e))
                    })?;
            let intent = crate::legacy_core_adapter::adapt_legacy_vm_mutation(
                &legacy_meta,
                &meta.target_node_id,
                crate::legacy_core_adapter::LegacyVmMutation::Create {
                    vm_id: vm.vm_id.clone(),
                    spec: Box::new(spec),
                },
                cellhv_core_types::ResourceVersion::new(1).unwrap(),
            )
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
            let accepted = authority
                .submit(intent.submission)
                .await
                .map_err(map_authority_error)?;
            return Ok(Response::new(proto::AckResponse {
                result: Some(proto::ResultMeta {
                    operation_id: meta.operation_id.clone(),
                    status: "ok".to_string(),
                    node_observed_generation: self.cache.lock().await.observed_generation.clone(),
                    error_code: "".to_string(),
                    human_summary: format!("{:?}", accepted.disposition),
                }),
            }));
        }

        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let vm = inner
            .vm
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing vm"))?;
        {
            let cache = self.cache.lock().await;
            ControlPlaneClient::stale_generation_check(meta, &cache, "vm", &vm.vm_id)
                .map_err(|e| Status::failed_precondition(e.to_string()))?;
            let node_state = cache
                .node_state
                .parse::<crate::state_machine::NodeState>()
                .unwrap_or(crate::state_machine::NodeState::Bootstrapping);
            if node_state != crate::state_machine::NodeState::TenantReady {
                return Err(Status::failed_precondition(format!(
                    "node not schedulable: {}",
                    cache.node_state
                )));
            }
            if cache.connectivity_state == crate::connectivity::ConnectivityState::Disconnected {
                return Err(Status::failed_precondition(
                    "control plane unreachable — VM creation denied to prevent split-brain",
                ));
            }
        }
        let vm_spec =
            crate::spec::VmSpec::from_json(std::str::from_utf8(&vm.vm_spec_json).unwrap_or(""))
                .map_err(|e| Status::invalid_argument(format!("invalid vm_spec_json: {}", e)))?;
        vm_spec
            .validate()
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let op_id = meta.operation_id.as_str();
        let mut disks = Vec::new();
        if !vm_spec.disks.is_empty() {
            let mut stord = crate::daemon_clients::StordClient::connect(&self.stord_socket)
                .await
                .map_err(|e| Status::unavailable(format!("stord unavailable: {}", e)))?;
            for disk in &vm_spec.disks {
                // #379 DP5: class-dependent locator shaping — an LVM-class
                // disk opens against a /dev/mapper/{vg}-{vid} dm-path token
                // (the standard device_allowlist's shape); every other
                // class keeps the historical {volume_id}.img default.
                let backend_class = disk.backend_class_or_local();
                let locator = if backend_class == "lvm" {
                    chv_hypervisor_api::resources::lvm_locator(
                        self.stord_backend.volume_group(),
                        &disk.volume_id,
                    )
                } else {
                    format!("{}.img", disk.volume_id)
                };
                let (_, handle, export_path) = stord
                    .open_volume(
                        &disk.volume_id,
                        // #379 PR 1 (A10): the class value from the disk
                        // spec instead of the inline "local" literal
                        // (absent → local).
                        backend_class,
                        &locator,
                        Some(op_id),
                    )
                    .await
                    .map_err(|e| Status::internal(format!("open_volume failed: {}", e)))?;
                stord
                    .attach_volume_to_vm(&disk.volume_id, &vm.vm_id, &handle, Some(op_id))
                    .await
                    .map_err(|e| Status::internal(format!("attach_volume_to_vm failed: {}", e)))?;
                disks.push(chv_hypervisor_api::VmDiskConfig {
                    path: std::path::PathBuf::from(export_path),
                    read_only: disk.read_only,
                    id: Some(disk.volume_id.clone()),
                });
                {
                    let mut cache = self.cache.lock().await;
                    cache.volume_handles.insert(disk.volume_id.clone(), handle);
                    self.persist_cache(&cache).await;
                }
            }
        }

        let mut nics = Vec::new();
        if !vm_spec.nics.is_empty() {
            let mut nwd = crate::daemon_clients::NwdClient::connect(&self.nwd_socket)
                .await
                .map_err(|e| Status::unavailable(format!("nwd unavailable: {}", e)))?;
            for nic in &vm_spec.nics {
                let nic_cidr = if nic.cidr.is_empty() {
                    "10.0.0.0/24".to_string()
                } else {
                    nic.cidr.clone()
                };
                let nic_gateway = nic.gateway.clone();
                let bridge = bridge_name_for_network(&nic.network_id);
                if let Err(e) = nwd
                    .ensure_network_topology(
                        &nic.network_id,
                        &bridge,
                        &nic_cidr,
                        &nic_gateway,
                        Some(op_id),
                    )
                    .await
                {
                    warn!(network_id = %nic.network_id, error = %e, "failed to ensure network topology in create_vm");
                }
                let nic_id = format!("{}-{}", vm.vm_id, nic.network_id);
                let (_ns, tap_handle) = nwd
                    .attach_vm_nic(
                        &nic_id,
                        &vm.vm_id,
                        &nic.network_id,
                        &nic.mac_address,
                        &nic.ip_address,
                        Some(op_id),
                    )
                    .await
                    .map_err(|e| Status::internal(format!("attach_vm_nic failed: {}", e)))?;
                nics.push(chv_hypervisor_api::VmNicConfig {
                    network_id: nic.network_id.clone(),
                    mac_address: nic.mac_address.clone(),
                    ip_address: nic.ip_address.clone(),
                    tap_name: tap_handle,
                    cidr: nic.cidr.clone(),
                    gateway: nic.gateway.clone(),
                });
            }
        }

        {
            let mut cache = self.cache.lock().await;
            let volume_ids = vm_spec
                .disks
                .iter()
                .map(|disk| disk.volume_id.clone())
                .collect::<Vec<_>>();
            let nics = vm_spec
                .nics
                .iter()
                .map(|nic| VmNicAttachment {
                    nic_id: format!("{}-{}", vm.vm_id, nic.network_id),
                    network_id: nic.network_id.clone(),
                })
                .collect::<Vec<_>>();
            cache.observe_vm_attachment(&vm.vm_id, &volume_ids, &nics);
            self.persist_cache(&cache).await;
        }

        let vm_dir = vm_runtime_dir(&self.runtime_dir, &vm.vm_id);
        // G5 vsock device (legacy direct-RPC path): same node-authority
        // semantics as the reconciler and the Core runtime — strictly
        // opt-in, durable monotonic node-scoped CID, device socket
        // under the VM's runtime dir, fail closed. The allocator does
        // blocking I/O (flock cursor); it runs on the blocking pool
        // because this is an async context.
        let vsock = match vm_spec.vsock {
            Some(_) => {
                let runtime_dir = self.runtime_dir.clone();
                let vm_dir_for_vsock = vm_dir.clone();
                Some(
                    tokio::task::spawn_blocking(move || {
                        chv_hypervisor_api::prepare_vm_vsock_device(&runtime_dir, &vm_dir_for_vsock)
                    })
                    .await
                    .map_err(|e| {
                        Status::internal(format!("vsock device preparation task failed: {e}"))
                    })?
                    .map_err(|e| {
                        Status::internal(format!("vsock device preparation failed: {e}"))
                    })?,
                )
            }
            None => None,
        };
        let config = VmConfig {
            vm_id: vm.vm_id.clone(),
            cpus: vm_spec.cpus,
            memory_bytes: vm_spec.memory_bytes,
            kernel_path: std::path::PathBuf::from(vm_spec.kernel_path),
            firmware_path: vm_spec.firmware_path.as_ref().map(std::path::PathBuf::from),
            disks,
            nics,
            api_socket_path: vm_dir.join("vm.sock"),
            cloud_init_userdata: vm_spec.cloud_init_userdata.clone(),
            hypervisor_overrides: vm_spec.hypervisor_overrides.clone(),
            vsock,
        };
        self.vm_runtime
            .create_vm(&vm.vm_id, &meta.desired_state_version, &config, Some(op_id))
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        let observed_generation = self.cache.lock().await.observed_generation.clone();
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "vm created".to_string(),
            }),
        }))
    }

    /// #513 PR 1 (DP2/DP5/DP7): the standalone volume-create dispatch
    /// carrier — `prepare_vm_resources`' open minus the attach. The
    /// load-bearing finding (design §2.4): the open MUST carry the
    /// requested capacity, because the local backend's create-on-open
    /// only provisions on a sized open and an LVM open of an absent LV
    /// without a size is rejected — this is NOT the attach path's
    /// option-less open. Dead-but-live with this PR: nothing dispatches
    /// it until the BFF route lands (PR 2).
    async fn create_volume(
        &self,
        req: Request<proto::CreateVolumeRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        // #513 DP7 (the #378/#495 posture): standalone volume
        // provisioning is a legacy-path stord side effect; on a
        // core-managed node the single writer is Core, which cannot
        // express it (its StorageAttachmentRef has no standalone-volume
        // concept). Fail closed exactly like the sibling legacy volume
        // RPCs — the orchestrator's Unimplemented fast-fail takes the
        // refused operation terminal without retry.
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "create_volume is unsupported in core-managed mode",
            ));
        }

        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let vol = inner
            .volume
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing volume"))?;
        // The volume id becomes a path component of the stord locator
        // (`{volume_id}.img` under stord's runtime dir for local,
        // `/dev/mapper/{vg}-{vid}` for LVM): a crafted id (`../..`) is a
        // write traversal. Reject anything that is not a single safe
        // component at the node boundary — the prepare_vm_resources
        // discipline (the control plane is a trusted-but-buggy peer).
        if !chv_common::is_safe_id(&vol.volume_id) {
            return Err(Status::invalid_argument(format!(
                "'{}' is not a safe volume id (must be a single path component)",
                vol.volume_id
            )));
        }
        {
            let cache = self.cache.lock().await;
            ControlPlaneClient::stale_generation_check(meta, &cache, "volume", &vol.volume_id)
                .map_err(|e| Status::failed_precondition(e.to_string()))?;
            // lock dropped here
        }
        let spec = serde_json::from_slice::<serde_json::Value>(&vol.volume_spec_json)
            .map_err(|e| Status::invalid_argument(format!("invalid volume spec_json: {}", e)))?;
        // The size is REQUIRED (not the attach path's optional-everything
        // parse): a create that opens without a size mints a volume the
        // attach path can never materialize — the local backend would
        // fall back to its default sparse size and an absent LVM LV is
        // refused outright.
        let size_bytes = spec
            .get("size_bytes")
            .and_then(|v| v.as_u64())
            .filter(|n| *n > 0)
            .ok_or_else(|| {
                Status::invalid_argument(
                    "volume spec_json requires a positive integer size_bytes (a standalone \
                     create must provision its backing store)",
                )
            })?;
        // #510's #379 DP5 class consumption (the attach parser's seam):
        // absent `backend_class` key → local; the stord backend fails
        // closed on a class it does not serve.
        let backend_class = spec
            .get("backend_class")
            .and_then(|v| v.as_str())
            .unwrap_or("local");
        // #379 DP5: class-dependent locator shaping — an LVM-class open
        // carries a /dev/mapper/{vg}-{vid} dm-path token the standard
        // device_allowlist admits (VG from the same stord-config source
        // as the VM path); every other class keeps the legacy CreateVm
        // branch's `{volume_id}.img` default (relative — the local
        // backend resolves it under its own runtime dir; a standalone
        // volume has no VM dir).
        let locator = if backend_class == "lvm" {
            chv_hypervisor_api::resources::lvm_locator(
                self.stord_backend.volume_group(),
                &vol.volume_id,
            )
        } else {
            format!("{}.img", vol.volume_id)
        };
        let mut open_options = std::collections::HashMap::new();
        open_options.insert("size_bytes".to_string(), size_bytes.to_string());

        let mut stord = crate::daemon_clients::StordClient::connect(&self.stord_socket)
            .await
            .map_err(|e| Status::unavailable(format!("stord unavailable: {}", e)))?;
        let (_, handle, _) = stord
            .open_volume_with_options(
                &vol.volume_id,
                backend_class,
                &locator,
                open_options,
                Some(&meta.operation_id),
            )
            .await
            .map_err(|e| Status::internal(format!("open_volume failed: {}", e)))?;

        let mut cache = self.cache.lock().await;
        cache.volume_handles.insert(vol.volume_id.clone(), handle);
        self.persist_cache(&cache).await;
        let observed_generation = cache.observed_generation.clone();
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "volume created".to_string(),
            }),
        }))
    }

    /// #522 (DP2, PR 1 of the decomposition): the volume-delete
    /// dispatch carrier's agent half — close → destroy → evict.
    /// Dead-but-live with this PR: no producer journals a
    /// `DeleteVolume` operation until the BFF route lands (PR 2).
    ///
    /// The sequence is the design's DP2/DP3 contract: the cached
    /// stord handle (if any) is closed FIRST (stord's
    /// `destroy_volume` refuses while a session is open — the agent
    /// is the cooperative caller), then the backing store is
    /// destroyed through stord at the DP4 CARRIER locator
    /// (byte-identical to the create handler's shaping — the
    /// relative `{volume_id}.img` for local, the
    /// `/dev/mapper/{vg}-{vid}` dm-path token for LVM; NEVER the
    /// attach path's stray bare-id file, which is #533's to fix), then
    /// the cache entry is evicted and persisted. The destroy is
    /// idempotent by stord's contract, so a redriven delete of an
    /// already-gone volume re-acks.
    async fn delete_volume(
        &self,
        req: Request<proto::DeleteVolumeRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        // #522 DP10 (the #378/#495 posture, mirroring create_volume):
        // the destroy is a legacy-path stord side effect; on a
        // core-managed node the single writer is Core. Fail closed
        // exactly like the sibling legacy volume RPCs — the
        // orchestrator's Unimplemented fast-fail takes the refused
        // operation terminal without retry.
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "delete_volume is unsupported in core-managed mode",
            ));
        }

        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        // The volume id becomes a path component of the destroy
        // locator (`{volume_id}.img` under stord's runtime dir for
        // local, `/dev/mapper/{vg}-{vid}` for LVM): a crafted id
        // (`../..`) is a REMOVAL traversal — worse than the write
        // traversal the create handler guards. Reject anything that
        // is not a single safe component at the node boundary.
        if !chv_common::is_safe_id(&inner.volume_id) {
            return Err(Status::invalid_argument(format!(
                "'{}' is not a safe volume id (must be a single path component)",
                inner.volume_id
            )));
        }
        {
            let cache = self.cache.lock().await;
            ControlPlaneClient::stale_generation_check(meta, &cache, "volume", &inner.volume_id)
                .map_err(|e| Status::failed_precondition(e.to_string()))?;
            // lock dropped here
        }
        // The class rides the dispatch (resolved in the CP's claim
        // query like the attach arm's) so the DP4 locator can be
        // shaped byte-identically to the create handler's; an empty
        // class is the NULL-class discipline (local, never
        // materialized).
        let backend_class = if inner.backend_class.is_empty() {
            "local"
        } else {
            inner.backend_class.as_str()
        };
        let locator = if backend_class == "lvm" {
            chv_hypervisor_api::resources::lvm_locator(
                self.stord_backend.volume_group(),
                &inner.volume_id,
            )
        } else {
            format!("{}.img", inner.volume_id)
        };

        let mut stord = crate::daemon_clients::StordClient::connect(&self.stord_socket)
            .await
            .map_err(|e| Status::unavailable(format!("stord unavailable: {}", e)))?;

        // Close the cached handle first (cooperative half of stord's
        // session refusal). A volume with no cached handle skips the
        // close — stord's own session table is the authority, and an
        // uncached-but-open session is stord's refusal to make, not
        // ours to paper over. The handle is cloned and the cache lock
        // dropped BEFORE the stord round-trip (review NIT — the create
        // handler's scoping: never hold the NodeCache mutex across an
        // await on another daemon, it serializes every cache-dependent
        // request behind the RPC).
        let cached_handle = {
            let cache = self.cache.lock().await;
            cache.volume_handles.get(&inner.volume_id).cloned()
        };
        if let Some(handle) = cached_handle {
            stord
                .close_volume(&inner.volume_id, &handle, Some(&meta.operation_id))
                .await
                .map_err(|e| Status::internal(format!("close_volume failed: {}", e)))?;
        }

        stord
            .destroy_volume(
                &inner.volume_id,
                backend_class,
                &locator,
                Some(&meta.operation_id),
            )
            .await
            .map_err(|e| Status::internal(format!("destroy_volume failed: {}", e)))?;

        // Evict the cache entry (if any) and persist: a stale handle
        // must never outlive the backing store it names.
        let observed_generation = {
            let mut cache = self.cache.lock().await;
            cache.volume_handles.remove(&inner.volume_id);
            self.persist_cache(&cache).await;
            cache.observed_generation.clone()
        };
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "volume deleted".to_string(),
            }),
        }))
    }

    /// #355 (design docs/design/issue-355-network-policy-dispatch.md,
    /// §3 Option A + DP3, PR 1 of the decomposition): the network
    /// firewall-policy dispatch carrier's agent half — apply the
    /// network's stored ruleset through nwd's existing
    /// `SetFirewallPolicy`, the same call the attach-time path makes
    /// (`core_runtime.rs::ensure_and_attach_nic`). Dead-but-live with
    /// this PR: no producer journals an `UpdateNetworkPolicy` operation
    /// until the BFF route lands (PR 2).
    ///
    /// Deliberately ACCEPTED in core-managed mode — the M2.2b carve-out
    /// the design rules on, and the one load-bearing difference from
    /// every sibling legacy nwd-side-effect RPC: policy application
    /// already runs behind the Core authority at VM attach (the core
    /// executor's set_firewall_policy call), the Core store models no
    /// network state to journal into, and the CP
    /// operations row is the journal. What M2.2b refuses is the legacy
    /// fragment path's NodeCache network-axis writes and unprompted
    /// topology ensure — this handler does neither: it applies policy
    /// to an EXISTING topology (nwd fails closed on an unknown
    /// topology) and writes no cache.
    async fn apply_network_policy(
        &self,
        req: Request<proto::ApplyNetworkPolicyRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        // The network id becomes the nft table name and rides nwd's
        // topology locator: reject anything that is not a single safe
        // component at the node boundary (the prepare_vm_resources
        // discipline — the control plane is a trusted-but-buggy peer).
        if !chv_common::is_safe_id(&inner.network_id) {
            return Err(Status::invalid_argument(format!(
                "'{}' is not a safe network id (must be a single path component)",
                inner.network_id
            )));
        }
        // Fail closed on malformed policy bytes BEFORE nwd: the CP is a
        // trusted-but-buggy peer, and the M4.4 N7 lesson (a UI-dialect
        // rule rode the spec) is exactly the class a node-boundary
        // parse catches. nwd re-validates the vocabulary on apply.
        let policy: serde_json::Value = serde_json::from_slice(&inner.policy_json)
            .map_err(|e| Status::invalid_argument(format!("invalid policy_json: {}", e)))?;
        if !policy.is_array() {
            return Err(Status::invalid_argument(
                "policy_json must be a JSON array of firewall rules",
            ));
        }
        // DP4 (ruled 2026-10-08): `[]` = no user rules → the shared
        // BASELINE (DHCP UDP 67/68, DNS UDP/TCP 53, conntrack from the
        // engine) on top of default-deny — replacing the pre-DP4 no-op
        // (the #360 cutoff: a bare default-deny with zero allows would
        // cut the network's guests off entirely, which is why empty
        // used to mean "never apply"). The orchestrator resolves the
        // same baseline before dispatching, so this branch is
        // belt-and-suspenders for a direct RPC caller (or a future
        // second producer): everyone gets the DP4 boundary, never a
        // blackout, never an unfiltered skip.
        let policy_json: Vec<u8> = if chv_common::firewall_ruleset_is_empty(
            &String::from_utf8_lossy(&inner.policy_json),
        ) {
            chv_common::firewall::baseline_policy_json().into_bytes()
        } else {
            inner.policy_json.clone()
        };
        // DP7 — dispatch fencing (#355 PR 3): `desired_state_version`
        // carries the NDS generation (the carrier sends it numeric).
        // Parse it AS A NUMBER — never string-compare ("policy-9" vs
        // "policy-10" sorts the wrong way) — and no-op any dispatch
        // below the last APPLIED generation for this network, so an
        // out-of-order fan-out result or an older operation's retry
        // cannot regress a newer policy. Unparseable versions fail
        // closed (the CP is a trusted-but-buggy peer).
        let requested_generation: u64 = meta.desired_state_version.parse().map_err(|_| {
            Status::invalid_argument(format!(
                "desired_state_version '{}' is not a numeric generation (the DP7 fence)",
                meta.desired_state_version
            ))
        })?;
        let last_applied: u64 = self
            .network_policy_fence
            .lock()
            .await
            .get(&inner.network_id)
            .copied()
            .unwrap_or(0);
        if requested_generation < last_applied {
            info!(
                network_id = %inner.network_id,
                requested_generation,
                last_applied,
                "network policy dispatch is stale; idempotent no-op"
            );
            let observed_generation = {
                let cache = self.cache.lock().await;
                cache.observed_generation.clone()
            };
            return Ok(Response::new(proto::AckResponse {
                result: Some(proto::ResultMeta {
                    operation_id: meta.operation_id.clone(),
                    status: "ok".to_string(),
                    node_observed_generation: observed_generation,
                    error_code: "".to_string(),
                    human_summary: format!(
                        "network policy unchanged (generation {requested_generation} is stale; \
                         generation {last_applied} is already applied)"
                    ),
                }),
            }));
        }
        // policy_version is nwd bookkeeping only (recorded for
        // re-scoping on attach/overlay refresh); the generation string
        // is a stable, per-update discriminator.
        let policy_version = format!("policy-{}", meta.desired_state_version);
        let mut nwd = crate::daemon_clients::NwdClient::connect(&self.nwd_socket)
            .await
            .map_err(|e| Status::unavailable(format!("nwd unavailable: {}", e)))?;
        // The CHECKED variant: nwd reports semantic refusals (unknown
        // topology, engine failure, vocabulary rejection) as a gRPC Ok
        // wrapping an err_result — an unchecked call would ack a false
        // success, the exact #355 lie in task form. The failure fails
        // the operation (retryable on the shared curve), never a
        // silent drop. The error's own Display already carries the
        // full cause chain ("network unavailable: nwd —
        // set_firewall_policy failed: {summary} ({code}))" — no
        // wrapper prefix, no double-wrap. The CP flattens every
        // non-Unimplemented code onto the shared retry curve, so the
        // single Internal mapping is the deliberate PR-1 shape; PR 2/3
        // may key off nwd's error_code if the surfaces ever need to
        // distinguish permanent-absence from transient.
        nwd.set_firewall_policy_checked(
            &inner.network_id,
            &policy_version,
            policy_json,
            Some(&meta.operation_id),
        )
        .await
        .map_err(|e| Status::internal(e.to_string()))?;
        // DP7: the fence advances ONLY on a successful apply — a
        // failed generation must not block a subsequent dispatch of an
        // older generation that still matches the last applied state —
        // and MONOTONICALLY (max, never plain insert): the read above
        // is separated from this write by the awaited nwd apply, so
        // two concurrent dispatches to the same network could both
        // have read the old mark; a plain insert would let the slower,
        // older generation regress the high-water mark.
        {
            let mut fence = self.network_policy_fence.lock().await;
            let current = fence.get(&inner.network_id).copied().unwrap_or(0);
            if requested_generation > current {
                fence.insert(inner.network_id.clone(), requested_generation);
            }
        }
        let observed_generation = {
            let cache = self.cache.lock().await;
            cache.observed_generation.clone()
        };
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "network policy applied".to_string(),
            }),
        }))
    }

    async fn start_vm(
        &self,
        req: Request<proto::StartVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        if let Some(ref authority) = self.core_authority {
            let inner = req.into_inner();
            let meta = inner
                .meta
                .as_ref()
                .ok_or_else(|| Status::invalid_argument("missing meta"))?;
            let legacy_meta = crate::legacy_core_adapter::LegacyRequestMeta {
                operation_id: meta.operation_id.clone(),
                requested_by: meta.requested_by.clone(),
                target_node_id: meta.target_node_id.clone(),
                desired_state_version: meta.desired_state_version.clone(),
                request_unix_ms: meta.request_unix_ms,
            };
            let vm_id = cellhv_core_types::VmId::new(&inner.vm_id)
                .map_err(|e| Status::invalid_argument(e.to_string()))?;
            let expected_version = authority_vm_version_or_status(authority, vm_id).await?;
            let intent = crate::legacy_core_adapter::adapt_legacy_vm_mutation(
                &legacy_meta,
                &meta.target_node_id,
                crate::legacy_core_adapter::LegacyVmMutation::Start {
                    vm_id: inner.vm_id.clone(),
                },
                expected_version,
            )
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
            let accepted = authority
                .submit(intent.submission)
                .await
                .map_err(map_authority_error)?;
            return Ok(Response::new(proto::AckResponse {
                result: Some(proto::ResultMeta {
                    operation_id: meta.operation_id.clone(),
                    status: "ok".to_string(),
                    node_observed_generation: self.cache.lock().await.observed_generation.clone(),
                    error_code: "".to_string(),
                    human_summary: format!("{:?}", accepted.disposition),
                }),
            }));
        }

        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let mut cache = self.cache.lock().await;
        ControlPlaneClient::stale_generation_check(meta, &cache, "vm", &inner.vm_id)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        let node_state = cache
            .node_state
            .parse::<crate::state_machine::NodeState>()
            .unwrap_or(crate::state_machine::NodeState::Bootstrapping);
        if node_state != crate::state_machine::NodeState::TenantReady {
            return Err(Status::failed_precondition(format!(
                "node not schedulable: {}",
                cache.node_state
            )));
        }
        cache.update_vm_desired_state(&inner.vm_id, "Running");
        self.persist_cache(&cache).await;
        drop(cache);
        self.vm_runtime
            .start_vm(&inner.vm_id, Some(&meta.operation_id))
            .await
            .map_err(|e| Status::not_found(e.to_string()))?;
        let observed_generation = self.cache.lock().await.observed_generation.clone();
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "vm started".to_string(),
            }),
        }))
    }

    async fn stop_vm(
        &self,
        req: Request<proto::StopVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        if let Some(ref authority) = self.core_authority {
            let inner = req.into_inner();
            let meta = inner
                .meta
                .as_ref()
                .ok_or_else(|| Status::invalid_argument("missing meta"))?;
            let legacy_meta = crate::legacy_core_adapter::LegacyRequestMeta {
                operation_id: meta.operation_id.clone(),
                requested_by: meta.requested_by.clone(),
                target_node_id: meta.target_node_id.clone(),
                desired_state_version: meta.desired_state_version.clone(),
                request_unix_ms: meta.request_unix_ms,
            };
            let vm_id = cellhv_core_types::VmId::new(&inner.vm_id)
                .map_err(|e| Status::invalid_argument(e.to_string()))?;
            let expected_version = authority_vm_version_or_status(authority, vm_id).await?;
            let intent = crate::legacy_core_adapter::adapt_legacy_vm_mutation(
                &legacy_meta,
                &meta.target_node_id,
                crate::legacy_core_adapter::LegacyVmMutation::Stop {
                    vm_id: inner.vm_id.clone(),
                    force: inner.force,
                },
                expected_version,
            )
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
            let accepted = authority
                .submit(intent.submission)
                .await
                .map_err(map_authority_error)?;
            return Ok(Response::new(proto::AckResponse {
                result: Some(proto::ResultMeta {
                    operation_id: meta.operation_id.clone(),
                    status: "ok".to_string(),
                    node_observed_generation: self.cache.lock().await.observed_generation.clone(),
                    error_code: "".to_string(),
                    human_summary: format!("{:?}", accepted.disposition),
                }),
            }));
        }

        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let mut cache = self.cache.lock().await;
        ControlPlaneClient::stale_generation_check(meta, &cache, "vm", &inner.vm_id)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        cache.update_vm_desired_state(&inner.vm_id, "Stopped");
        self.persist_cache(&cache).await;
        drop(cache);
        self.vm_runtime
            .stop_vm(&inner.vm_id, inner.force, Some(&meta.operation_id))
            .await
            .map_err(|e| Status::not_found(e.to_string()))?;
        let observed_generation = self.cache.lock().await.observed_generation.clone();
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "vm stopped".to_string(),
            }),
        }))
    }

    async fn reboot_vm(
        &self,
        req: Request<proto::RebootVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        if let Some(ref authority) = self.core_authority {
            let inner = req.into_inner();
            let meta = inner
                .meta
                .as_ref()
                .ok_or_else(|| Status::invalid_argument("missing meta"))?;
            let legacy_meta = crate::legacy_core_adapter::LegacyRequestMeta {
                operation_id: meta.operation_id.clone(),
                requested_by: meta.requested_by.clone(),
                target_node_id: meta.target_node_id.clone(),
                desired_state_version: meta.desired_state_version.clone(),
                request_unix_ms: meta.request_unix_ms,
            };
            let vm_id = cellhv_core_types::VmId::new(&inner.vm_id)
                .map_err(|e| Status::invalid_argument(e.to_string()))?;
            let expected_version = authority_vm_version_or_status(authority, vm_id).await?;
            let intent = crate::legacy_core_adapter::adapt_legacy_vm_mutation(
                &legacy_meta,
                &meta.target_node_id,
                crate::legacy_core_adapter::LegacyVmMutation::Reboot {
                    vm_id: inner.vm_id.clone(),
                    force: false,
                },
                expected_version,
            )
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
            let accepted = authority
                .submit(intent.submission)
                .await
                .map_err(map_authority_error)?;
            return Ok(Response::new(proto::AckResponse {
                result: Some(proto::ResultMeta {
                    operation_id: meta.operation_id.clone(),
                    status: "ok".to_string(),
                    node_observed_generation: self.cache.lock().await.observed_generation.clone(),
                    error_code: "".to_string(),
                    human_summary: format!("{:?}", accepted.disposition),
                }),
            }));
        }

        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let cache = self.cache.lock().await;
        ControlPlaneClient::stale_generation_check(meta, &cache, "vm", &inner.vm_id)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        self.vm_runtime
            .reboot_vm(&inner.vm_id, Some(&meta.operation_id))
            .await
            .map_err(|e| Status::not_found(e.to_string()))?;
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: cache.observed_generation.clone(),
                error_code: "".to_string(),
                human_summary: "vm rebooted".to_string(),
            }),
        }))
    }

    async fn delete_vm(
        &self,
        req: Request<proto::DeleteVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        if let Some(ref authority) = self.core_authority {
            let inner = req.into_inner();
            let meta = inner
                .meta
                .as_ref()
                .ok_or_else(|| Status::invalid_argument("missing meta"))?;
            let legacy_meta = crate::legacy_core_adapter::LegacyRequestMeta {
                operation_id: meta.operation_id.clone(),
                requested_by: meta.requested_by.clone(),
                target_node_id: meta.target_node_id.clone(),
                desired_state_version: meta.desired_state_version.clone(),
                request_unix_ms: meta.request_unix_ms,
            };
            let vm_id = cellhv_core_types::VmId::new(&inner.vm_id)
                .map_err(|e| Status::invalid_argument(e.to_string()))?;
            let expected_version = authority_vm_version_or_status(authority, vm_id).await?;
            let intent = crate::legacy_core_adapter::adapt_legacy_vm_mutation(
                &legacy_meta,
                &meta.target_node_id,
                crate::legacy_core_adapter::LegacyVmMutation::Delete {
                    vm_id: inner.vm_id.clone(),
                    force: false,
                },
                expected_version,
            )
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
            let accepted = authority
                .submit(intent.submission)
                .await
                .map_err(map_authority_error)?;
            return Ok(Response::new(proto::AckResponse {
                result: Some(proto::ResultMeta {
                    operation_id: meta.operation_id.clone(),
                    status: "ok".to_string(),
                    node_observed_generation: self.cache.lock().await.observed_generation.clone(),
                    error_code: "".to_string(),
                    human_summary: format!("{:?}", accepted.disposition),
                }),
            }));
        }

        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let observed_generation = {
            let cache = self.cache.lock().await;
            ControlPlaneClient::stale_generation_check(meta, &cache, "vm", &inner.vm_id)
                .map_err(|e| Status::failed_precondition(e.to_string()))?;
            cache.observed_generation.clone()
        };
        cleanup_vm_resources(
            &self.cache,
            &self.stord_socket,
            &self.nwd_socket,
            &inner.vm_id,
            Some(&meta.operation_id),
        )
        .await
        .map_err(|e| Status::internal(format!("vm cleanup failed: {}", e)))?;
        self.vm_runtime
            .delete_vm(&inner.vm_id, Some(&meta.operation_id))
            .await
            .map_err(|e| Status::not_found(e.to_string()))?;
        {
            let mut cache = self.cache.lock().await;
            cache.remove_vm_state(&inner.vm_id);
            self.persist_cache(&cache).await;
        }
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "vm deleted".to_string(),
            }),
        }))
    }

    async fn resize_vm(
        &self,
        req: Request<proto::ResizeVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "resize_vm is unsupported in core-managed mode",
            ));
        }

        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let cache = self.cache.lock().await;
        ControlPlaneClient::stale_generation_check(meta, &cache, "vm", &inner.vm_id)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        drop(cache);
        self.vm_runtime
            .resize_vm(
                &inner.vm_id,
                inner.desired_vcpus,
                inner.desired_memory_bytes,
                Some(&meta.operation_id),
            )
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        let observed_generation = self.cache.lock().await.observed_generation.clone();
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "vm resized".to_string(),
            }),
        }))
    }

    async fn attach_volume(
        &self,
        req: Request<proto::AttachVolumeRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "attach_volume is unsupported in core-managed mode",
            ));
        }

        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let vol = inner
            .volume
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing volume"))?;
        {
            let cache = self.cache.lock().await;
            ControlPlaneClient::stale_generation_check(meta, &cache, "volume", &vol.volume_id)
                .map_err(|e| Status::failed_precondition(e.to_string()))?;
            // lock dropped here
        }

        let handle = self
            .open_and_attach_volume(
                &vol.volume_id,
                &vol.vm_id,
                &vol.volume_spec_json,
                &meta.operation_id,
            )
            .await?;

        let mut cache = self.cache.lock().await;
        cache.volume_handles.insert(vol.volume_id.clone(), handle);
        cache.observe_vm_attachment(&vol.vm_id, std::slice::from_ref(&vol.volume_id), &[]);
        self.persist_cache(&cache).await;

        let observed_generation = cache.observed_generation.clone();
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "volume attached".to_string(),
            }),
        }))
    }

    async fn detach_volume(
        &self,
        req: Request<proto::DetachVolumeRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "detach_volume is unsupported in core-managed mode",
            ));
        }

        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;

        // Extract what we need from cache, then drop the lock before I/O
        let (volume_handle, observed_generation) = {
            let cache = self.cache.lock().await;
            ControlPlaneClient::stale_generation_check(meta, &cache, "volume", &inner.volume_id)
                .map_err(|e| Status::failed_precondition(e.to_string()))?;
            let handle = cache.volume_handles.get(&inner.volume_id).cloned();
            let gen = cache.observed_generation.clone();
            (handle, gen)
        };

        let mut stord = crate::daemon_clients::StordClient::connect(&self.stord_socket)
            .await
            .map_err(|e| Status::unavailable(format!("stord unavailable: {}", e)))?;

        stord
            .detach_volume_from_vm(
                &inner.volume_id,
                &inner.vm_id,
                inner.force,
                Some(&meta.operation_id),
            )
            .await
            .map_err(|e| Status::internal(format!("detach_volume_from_vm failed: {}", e)))?;

        if let Some(handle) = volume_handle {
            if let Err(e) = stord
                .close_volume(&inner.volume_id, &handle, Some(&meta.operation_id))
                .await
            {
                tracing::warn!(
                    volume_id = %inner.volume_id,
                    error = %e,
                    "close_volume failed after detach"
                );
            }
        }

        // Re-acquire lock only for cache mutation
        {
            let mut cache = self.cache.lock().await;
            cache.volume_handles.remove(&inner.volume_id);
            self.persist_cache(&cache).await;
        }

        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "volume detached".to_string(),
            }),
        }))
    }

    async fn resize_volume(
        &self,
        req: Request<proto::ResizeVolumeRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        // M2.2b single-writer enforcement: fail closed in core-managed
        // mode so this legacy stord volume resize (+ hypervisor disk resize) side effect can never run behind the
        // Core authority (Core M1 does not model it).
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "resize_volume is unsupported in core-managed mode",
            ));
        }
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let cache = self.cache.lock().await;
        ControlPlaneClient::stale_generation_check(meta, &cache, "volume", &inner.volume_id)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        drop(cache);

        let mut stord = crate::daemon_clients::StordClient::connect(&self.stord_socket)
            .await
            .map_err(|e| Status::unavailable(format!("stord unavailable: {}", e)))?;

        stord
            .resize_volume(
                &inner.volume_id,
                inner.new_size_bytes,
                Some(&meta.operation_id),
            )
            .await
            .map_err(|e| Status::internal(format!("resize_volume failed: {}", e)))?;

        // Find which VM (if any) has this volume attached and notify CH
        let attached_vm = {
            let cache = self.cache.lock().await;
            cache
                .vm_attachments
                .iter()
                .find(|(_, state)| state.volume_ids.contains(&inner.volume_id))
                .map(|(vm_id, _)| vm_id.clone())
        };

        if let Some(vm_id) = attached_vm {
            if let Err(e) = self
                .vm_runtime
                .resize_disk(
                    &vm_id,
                    &inner.volume_id,
                    inner.new_size_bytes,
                    Some(&meta.operation_id),
                )
                .await
            {
                tracing::warn!(
                    vm_id = %vm_id,
                    volume_id = %inner.volume_id,
                    error = %e,
                    "vm.resize-zone failed; volume resized on disk but VM may need restart"
                );
            }
        }

        let observed_generation = self.cache.lock().await.observed_generation.clone();
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "volume resized".to_string(),
            }),
        }))
    }

    async fn snapshot_volume(
        &self,
        req: Request<proto::SnapshotVolumeRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        // M2.2b single-writer enforcement: fail closed in core-managed
        // mode so this legacy stord volume snapshot side effect can never run behind the
        // Core authority (Core M1 does not model it).
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "snapshot_volume is unsupported in core-managed mode",
            ));
        }
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let cache = self.cache.lock().await;
        ControlPlaneClient::stale_generation_check(meta, &cache, "volume", &inner.volume_id)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        drop(cache);

        let mut stord = crate::daemon_clients::StordClient::connect(&self.stord_socket)
            .await
            .map_err(|e| Status::unavailable(format!("stord unavailable: {}", e)))?;

        stord
            .prepare_snapshot(
                &inner.volume_id,
                &inner.snapshot_name,
                Some(&meta.operation_id),
            )
            .await
            .map_err(|e| Status::internal(format!("prepare_snapshot failed: {}", e)))?;

        let observed_generation = self.cache.lock().await.observed_generation.clone();
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "volume snapshot created".to_string(),
            }),
        }))
    }

    async fn restore_volume(
        &self,
        req: Request<proto::RestoreVolumeRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        // M2.2b single-writer enforcement: fail closed in core-managed
        // mode so this legacy stord volume restore side effect can never run behind the
        // Core authority (Core M1 does not model it).
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "restore_volume is unsupported in core-managed mode",
            ));
        }
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let cache = self.cache.lock().await;
        ControlPlaneClient::stale_generation_check(meta, &cache, "volume", &inner.volume_id)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        drop(cache);

        let mut stord = crate::daemon_clients::StordClient::connect(&self.stord_socket)
            .await
            .map_err(|e| Status::unavailable(format!("stord unavailable: {}", e)))?;

        stord
            .restore_snapshot(
                &inner.volume_id,
                &inner.snapshot_name,
                Some(&meta.operation_id),
            )
            .await
            .map_err(|e| Status::internal(format!("restore_snapshot failed: {}", e)))?;

        let observed_generation = self.cache.lock().await.observed_generation.clone();
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "volume snapshot restored".to_string(),
            }),
        }))
    }

    async fn delete_volume_snapshot(
        &self,
        req: Request<proto::DeleteVolumeSnapshotRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        // M2.2b single-writer enforcement: fail closed in core-managed
        // mode so this legacy stord volume snapshot delete side effect can never run behind the
        // Core authority (Core M1 does not model it).
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "delete_volume_snapshot is unsupported in core-managed mode",
            ));
        }
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let cache = self.cache.lock().await;
        ControlPlaneClient::stale_generation_check(meta, &cache, "volume", &inner.volume_id)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        drop(cache);

        let mut stord = crate::daemon_clients::StordClient::connect(&self.stord_socket)
            .await
            .map_err(|e| Status::unavailable(format!("stord unavailable: {}", e)))?;

        stord
            .delete_snapshot(
                &inner.volume_id,
                &inner.snapshot_name,
                Some(&meta.operation_id),
            )
            .await
            .map_err(|e| Status::internal(format!("delete_snapshot failed: {}", e)))?;

        let observed_generation = self.cache.lock().await.observed_generation.clone();
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "volume snapshot deleted".to_string(),
            }),
        }))
    }

    async fn clone_volume(
        &self,
        req: Request<proto::CloneVolumeRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        // M2.2b single-writer enforcement: fail closed in core-managed
        // mode so this legacy stord volume clone side effect can never run behind the
        // Core authority (Core M1 does not model it).
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "clone_volume is unsupported in core-managed mode",
            ));
        }
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let cache = self.cache.lock().await;
        ControlPlaneClient::stale_generation_check(meta, &cache, "volume", &inner.source_volume_id)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        drop(cache);

        let mut stord = crate::daemon_clients::StordClient::connect(&self.stord_socket)
            .await
            .map_err(|e| Status::unavailable(format!("stord unavailable: {}", e)))?;

        stord
            .prepare_clone(
                &inner.source_volume_id,
                &inner.target_volume_id,
                Some(&meta.operation_id),
            )
            .await
            .map_err(|e| Status::internal(format!("prepare_clone failed: {}", e)))?;

        let observed_generation = self.cache.lock().await.observed_generation.clone();
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "volume cloned".to_string(),
            }),
        }))
    }

    async fn pause_node_scheduling(
        &self,
        req: Request<proto::PauseNodeSchedulingRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let mut cache = self.cache.lock().await;
        if let Err(e) = cache.transition_node_state(NodeState::Degraded) {
            return Err(Status::failed_precondition(e.to_string()));
        }
        self.persist_cache(&cache).await;
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: cache.observed_generation.clone(),
                error_code: "".to_string(),
                human_summary: "node scheduling paused".to_string(),
            }),
        }))
    }

    async fn resume_node_scheduling(
        &self,
        req: Request<proto::ResumeNodeSchedulingRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let mut cache = self.cache.lock().await;
        if let Err(e) = cache.transition_node_state(NodeState::TenantReady) {
            return Err(Status::failed_precondition(e.to_string()));
        }
        self.persist_cache(&cache).await;
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: cache.observed_generation.clone(),
                error_code: "".to_string(),
                human_summary: "node scheduling resumed".to_string(),
            }),
        }))
    }

    async fn drain_node(
        &self,
        req: Request<proto::DrainNodeRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let mut cache = self.cache.lock().await;
        if let Err(e) = cache.transition_node_state(NodeState::Draining) {
            return Err(Status::failed_precondition(e.to_string()));
        }
        self.persist_cache(&cache).await;
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: cache.observed_generation.clone(),
                error_code: "".to_string(),
                human_summary: "node draining".to_string(),
            }),
        }))
    }

    async fn enter_maintenance(
        &self,
        req: Request<proto::EnterMaintenanceRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let mut cache = self.cache.lock().await;
        if let Err(e) = cache.transition_node_state(NodeState::Maintenance) {
            return Err(Status::failed_precondition(e.to_string()));
        }
        self.persist_cache(&cache).await;
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: cache.observed_generation.clone(),
                error_code: "".to_string(),
                human_summary: "node entering maintenance".to_string(),
            }),
        }))
    }

    async fn exit_maintenance(
        &self,
        req: Request<proto::ExitMaintenanceRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let mut cache = self.cache.lock().await;
        if let Err(e) = cache.transition_node_state(NodeState::Bootstrapping) {
            return Err(Status::failed_precondition(e.to_string()));
        }
        self.persist_cache(&cache).await;
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: cache.observed_generation.clone(),
                error_code: "".to_string(),
                human_summary: "node exiting maintenance".to_string(),
            }),
        }))
    }

    async fn pause_vm(
        &self,
        req: Request<proto::PauseVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        // M2.2b single-writer enforcement: fail closed in core-managed
        // mode so this legacy hypervisor pause side effect can never run behind the
        // Core authority (Core M1 does not model it).
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "pause_vm is unsupported in core-managed mode",
            ));
        }
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let cache = self.cache.lock().await;
        ControlPlaneClient::stale_generation_check(meta, &cache, "vm", &inner.vm_id)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        drop(cache);
        self.vm_runtime
            .pause_vm(&inner.vm_id, Some(&meta.operation_id))
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        let observed_generation = self.cache.lock().await.observed_generation.clone();
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "vm paused".to_string(),
            }),
        }))
    }

    async fn resume_vm(
        &self,
        req: Request<proto::ResumeVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        // M2.2b single-writer enforcement: fail closed in core-managed
        // mode so this legacy hypervisor resume side effect can never run behind the
        // Core authority (Core M1 does not model it).
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "resume_vm is unsupported in core-managed mode",
            ));
        }
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let cache = self.cache.lock().await;
        ControlPlaneClient::stale_generation_check(meta, &cache, "vm", &inner.vm_id)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        drop(cache);
        self.vm_runtime
            .resume_vm(&inner.vm_id, Some(&meta.operation_id))
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        let observed_generation = self.cache.lock().await.observed_generation.clone();
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "vm resumed".to_string(),
            }),
        }))
    }

    async fn power_button_vm(
        &self,
        req: Request<proto::PowerButtonVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        // M2.2b single-writer enforcement: fail closed in core-managed
        // mode so this legacy hypervisor power-button side effect can never run behind the
        // Core authority (Core M1 does not model it).
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "power_button_vm is unsupported in core-managed mode",
            ));
        }
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let cache = self.cache.lock().await;
        ControlPlaneClient::stale_generation_check(meta, &cache, "vm", &inner.vm_id)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        drop(cache);
        self.vm_runtime
            .power_button(&inner.vm_id, Some(&meta.operation_id))
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        let observed_generation = self.cache.lock().await.observed_generation.clone();
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "vm power button pressed".to_string(),
            }),
        }))
    }

    async fn add_disk(
        &self,
        req: Request<proto::AddDiskRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        // M2.2b single-writer enforcement: fail closed in core-managed
        // mode so this legacy live device topology side effect can never run behind the
        // Core authority (Core M1 does not model it).
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "add_disk is unsupported in core-managed mode",
            ));
        }
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let cache = self.cache.lock().await;
        ControlPlaneClient::stale_generation_check(meta, &cache, "vm", &inner.vm_id)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        drop(cache);
        let params = chv_hypervisor_api::AddDiskParams {
            path: std::path::PathBuf::from(&inner.disk_path),
            read_only: inner.read_only,
            id: if inner.disk_id.is_empty() {
                None
            } else {
                Some(inner.disk_id.clone())
            },
        };
        self.vm_runtime
            .add_disk(&inner.vm_id, &params, Some(&meta.operation_id))
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        let observed_generation = self.cache.lock().await.observed_generation.clone();
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "disk added".to_string(),
            }),
        }))
    }

    async fn remove_device(
        &self,
        req: Request<proto::RemoveDeviceRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        // M2.2b single-writer enforcement: fail closed in core-managed
        // mode so this legacy live device topology side effect can never run behind the
        // Core authority (Core M1 does not model it).
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "remove_device is unsupported in core-managed mode",
            ));
        }
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let cache = self.cache.lock().await;
        ControlPlaneClient::stale_generation_check(meta, &cache, "vm", &inner.vm_id)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        drop(cache);
        self.vm_runtime
            .remove_device(&inner.vm_id, &inner.device_id, Some(&meta.operation_id))
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        let observed_generation = self.cache.lock().await.observed_generation.clone();
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "device removed".to_string(),
            }),
        }))
    }

    async fn add_net(
        &self,
        req: Request<proto::AddNetRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        // M2.2b single-writer enforcement: fail closed in core-managed
        // mode so this legacy live device topology side effect can never run behind the
        // Core authority (Core M1 does not model it).
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "add_net is unsupported in core-managed mode",
            ));
        }
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let cache = self.cache.lock().await;
        ControlPlaneClient::stale_generation_check(meta, &cache, "vm", &inner.vm_id)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        drop(cache);
        let params = chv_hypervisor_api::AddNetParams {
            tap_name: inner.tap_name.clone(),
            mac_address: inner.mac_address.clone(),
            id: if inner.net_id.is_empty() {
                None
            } else {
                Some(inner.net_id.clone())
            },
        };
        self.vm_runtime
            .add_net(&inner.vm_id, &params, Some(&meta.operation_id))
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        let observed_generation = self.cache.lock().await.observed_generation.clone();
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "network interface added".to_string(),
            }),
        }))
    }

    async fn resize_disk(
        &self,
        req: Request<proto::ResizeDiskRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        // M2.2b single-writer enforcement: fail closed in core-managed
        // mode so this legacy live device topology side effect can never run behind the
        // Core authority (Core M1 does not model it).
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "resize_disk is unsupported in core-managed mode",
            ));
        }
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let cache = self.cache.lock().await;
        ControlPlaneClient::stale_generation_check(meta, &cache, "vm", &inner.vm_id)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        drop(cache);
        self.vm_runtime
            .resize_disk(
                &inner.vm_id,
                &inner.disk_id,
                inner.new_size_bytes,
                Some(&meta.operation_id),
            )
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        let observed_generation = self.cache.lock().await.observed_generation.clone();
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "disk resized".to_string(),
            }),
        }))
    }

    async fn snapshot_vm(
        &self,
        req: Request<proto::SnapshotVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        // M2.2b single-writer enforcement: fail closed in core-managed
        // mode so this legacy hypervisor snapshot side effect can never run behind the
        // Core authority (Core M1 does not model it).
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "snapshot_vm is unsupported in core-managed mode",
            ));
        }
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let cache = self.cache.lock().await;
        ControlPlaneClient::stale_generation_check(meta, &cache, "vm", &inner.vm_id)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        drop(cache);
        self.vm_runtime
            .snapshot_vm(&inner.vm_id, &inner.destination, Some(&meta.operation_id))
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        let observed_generation = self.cache.lock().await.observed_generation.clone();
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "vm snapshot taken".to_string(),
            }),
        }))
    }

    async fn restore_snapshot(
        &self,
        req: Request<proto::RestoreSnapshotRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        // M2.2b single-writer enforcement: fail closed in core-managed
        // mode so this legacy hypervisor snapshot restore side effect can never run behind the
        // Core authority (Core M1 does not model it).
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "restore_snapshot is unsupported in core-managed mode",
            ));
        }
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let cache = self.cache.lock().await;
        ControlPlaneClient::stale_generation_check(meta, &cache, "vm", &inner.vm_id)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        drop(cache);
        self.vm_runtime
            .restore_snapshot(&inner.vm_id, &inner.source, Some(&meta.operation_id))
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        let observed_generation = self.cache.lock().await.observed_generation.clone();
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "snapshot restored".to_string(),
            }),
        }))
    }

    async fn coredump_vm(
        &self,
        req: Request<proto::CoredumpVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        // M2.2b single-writer enforcement: fail closed in core-managed
        // mode so this legacy hypervisor coredump side effect can never run behind the
        // Core authority (Core M1 does not model it).
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "coredump_vm is unsupported in core-managed mode",
            ));
        }
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let cache = self.cache.lock().await;
        ControlPlaneClient::stale_generation_check(meta, &cache, "vm", &inner.vm_id)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        drop(cache);
        self.vm_runtime
            .coredump(&inner.vm_id, &inner.destination, Some(&meta.operation_id))
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        let observed_generation = self.cache.lock().await.observed_generation.clone();
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "vm coredump complete".to_string(),
            }),
        }))
    }

    async fn start_network(
        &self,
        req: Request<proto::StartNetworkRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        // M2.2b single-writer enforcement: fail closed in core-managed mode so
        // no legacy nwd side effect runs behind the Core authority.
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "start_network is unsupported in core-managed mode",
            ));
        }
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let cache = self.cache.lock().await;
        ControlPlaneClient::stale_generation_check(meta, &cache, "network", &inner.network_id)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        let spec_json = cache
            .get_fragment("network", &inner.network_id)
            .map(|f| f.spec_json.clone())
            .unwrap_or_default();
        let spec = serde_json::from_slice::<serde_json::Value>(&spec_json).unwrap_or_default();
        let bridge = spec
            .get("bridge_name")
            .and_then(|v| v.as_str())
            .unwrap_or("br0");
        let cidr = spec
            .get("cidr")
            .and_then(|v| v.as_str())
            .unwrap_or("10.0.0.0/24");
        let gateway = spec.get("gateway").and_then(|v| v.as_str()).unwrap_or("");
        drop(cache);

        let mut nwd = crate::daemon_clients::NwdClient::connect(&self.nwd_socket)
            .await
            .map_err(|e| Status::unavailable(format!("nwd unavailable: {}", e)))?;

        nwd.ensure_network_topology(
            &inner.network_id,
            bridge,
            cidr,
            gateway,
            Some(&meta.operation_id),
        )
        .await
        .map_err(|e| Status::internal(format!("ensure_network_topology failed: {}", e)))?;

        let observed_generation = self.cache.lock().await.observed_generation.clone();
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "network started".to_string(),
            }),
        }))
    }

    async fn stop_network(
        &self,
        req: Request<proto::StopNetworkRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        // M2.2b single-writer enforcement: fail closed in core-managed mode so
        // no legacy nwd side effect runs behind the Core authority.
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "stop_network is unsupported in core-managed mode",
            ));
        }
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let cache = self.cache.lock().await;
        ControlPlaneClient::stale_generation_check(meta, &cache, "network", &inner.network_id)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        drop(cache);

        let mut nwd = crate::daemon_clients::NwdClient::connect(&self.nwd_socket)
            .await
            .map_err(|e| Status::unavailable(format!("nwd unavailable: {}", e)))?;

        nwd.delete_network_topology(&inner.network_id, Some(&meta.operation_id))
            .await
            .map_err(|e| Status::internal(format!("delete_network_topology failed: {}", e)))?;

        let observed_generation = self.cache.lock().await.observed_generation.clone();
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "network stopped".to_string(),
            }),
        }))
    }

    async fn restart_network(
        &self,
        req: Request<proto::RestartNetworkRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        // M2.2b single-writer enforcement: fail closed in core-managed mode so
        // no legacy nwd side effect runs behind the Core authority.
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "restart_network is unsupported in core-managed mode",
            ));
        }
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let cache = self.cache.lock().await;
        ControlPlaneClient::stale_generation_check(meta, &cache, "network", &inner.network_id)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        let spec_json = cache
            .get_fragment("network", &inner.network_id)
            .map(|f| f.spec_json.clone())
            .unwrap_or_default();
        let spec = serde_json::from_slice::<serde_json::Value>(&spec_json).unwrap_or_default();
        let bridge = spec
            .get("bridge_name")
            .and_then(|v| v.as_str())
            .unwrap_or("br0");
        let cidr = spec
            .get("cidr")
            .and_then(|v| v.as_str())
            .unwrap_or("10.0.0.0/24");
        let gateway = spec.get("gateway").and_then(|v| v.as_str()).unwrap_or("");
        drop(cache);

        let mut nwd = crate::daemon_clients::NwdClient::connect(&self.nwd_socket)
            .await
            .map_err(|e| Status::unavailable(format!("nwd unavailable: {}", e)))?;

        nwd.delete_network_topology(&inner.network_id, Some(&meta.operation_id))
            .await
            .map_err(|e| Status::internal(format!("delete_network_topology failed: {}", e)))?;
        nwd.ensure_network_topology(
            &inner.network_id,
            bridge,
            cidr,
            gateway,
            Some(&meta.operation_id),
        )
        .await
        .map_err(|e| Status::internal(format!("ensure_network_topology failed: {}", e)))?;

        let observed_generation = self.cache.lock().await.observed_generation.clone();
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "network restarted".to_string(),
            }),
        }))
    }

    async fn ping_vmm(
        &self,
        req: Request<proto::PingVmmRequest>,
    ) -> Result<Response<proto::PingVmmResponse>, Status> {
        let inner = req.into_inner();
        let result = self
            .vm_runtime
            .ping(&inner.vm_id)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(proto::PingVmmResponse { alive: result }))
    }

    async fn migrate_vm(
        &self,
        req: Request<proto::MigrateVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        // M2.2b single-writer enforcement: fail closed in core-managed
        // mode so this legacy live migration side effect can never run behind the
        // Core authority (Core M1 does not model it).
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "migrate_vm is unsupported in core-managed mode",
            ));
        }
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let operation_id = meta.operation_id.clone();

        let my_node_id = {
            let cache = self.cache.lock().await;
            if cache.connectivity_state == crate::connectivity::ConnectivityState::Disconnected {
                return Err(Status::failed_precondition(
                    "control plane unreachable — VM migration denied to prevent split-brain",
                ));
            }
            cache.node_id.clone()
        };

        let role = crate::migration::determine_role(
            &my_node_id,
            &inner.source_node_id,
            &inner.destination_node_id,
        )
        .ok_or_else(|| {
            Status::invalid_argument(format!(
                "this node ({}) is neither source ({}) nor destination ({})",
                my_node_id, inner.source_node_id, inner.destination_node_id
            ))
        })?;

        let vm_id = inner.vm_id.clone();
        let vm_runtime = self.vm_runtime.clone();

        match role {
            crate::migration::MigrationRole::Source => {
                // Source agent: initiate disk pre-copy then send-migration.
                let dest_host = crate::migration::extract_destination_host(&inner);
                let dest_port = crate::migration::DEFAULT_MIGRATION_PORT;
                let destination_url =
                    crate::migration::build_destination_url(&dest_host, dest_port);

                // Build disk pre-copy config from attached volumes in cache.
                let (volumes, dest_stord_endpoint) = {
                    let cache = self.cache.lock().await;
                    let vol_ids = cache
                        .vm_attachment_state(&vm_id)
                        .map(|a| a.volume_ids.clone())
                        .unwrap_or_default();
                    let mut volumes = Vec::new();
                    for vol_id in vol_ids {
                        if let Some(handle) = cache.volume_handles.get(&vol_id).cloned() {
                            volumes.push(crate::migration::MigrationVolume {
                                volume_id: vol_id,
                                attachment_handle: handle,
                            });
                        }
                    }
                    // Derive destination stord endpoint from dest host.
                    // The destination stord migration service listens on a fixed port.
                    let dest_stord_endpoint = format!("https://{}:50052", dest_host);
                    (volumes, dest_stord_endpoint)
                };

                let disk_config = crate::migration::DiskPrecopyConfig {
                    stord_socket: self.stord_socket.clone(),
                    dest_stord_endpoint,
                    volumes,
                    // Issue #394 Option C: the operator's opt-in
                    // stop-the-world mode. The config message's tuning
                    // fields remain defaults-only (0 = "use stord's
                    // defaults"); pause_first is the first field the
                    // agent actually reads.
                    pause_first: inner.config.as_ref().is_some_and(|c| c.pause_first),
                };

                // Build a progress reporter that enqueues migration progress to the
                // control plane via the cache's pending message queue.
                let cache_for_reporter = self.cache.clone();
                let progress_reporter = crate::migration::make_progress_reporter(move |progress| {
                    let msg =
                        crate::cache::PendingControlPlaneMessage::migration_progress(progress);
                    // Best-effort: if lock is held, skip rather than block.
                    if let Ok(mut cache) = cache_for_reporter.try_lock() {
                        cache.enqueue_pending_message(msg);
                    } else {
                        tracing::warn!("cache locked, skipping migration progress enqueue");
                    }
                });

                tracing::info!(
                    vm_id = %vm_id,
                    operation_id = %operation_id,
                    destination_url = %destination_url,
                    volume_count = disk_config.volumes.len(),
                    "source agent: ACKing migrate_vm, spawning disk+memory migration task"
                );

                // Track the spawned migration so it can be cancelled by
                // operator request and aborted on agent shutdown. Without
                // this, the JoinHandle was dropped on the floor and the agent
                // had no way to abort, reap, or surface terminal failure.
                let cancel_token = CancellationToken::new();
                let registry = self.migration_tasks.clone();
                let op_id_for_task = operation_id.clone();
                let op_id_for_log = operation_id.clone();
                let node_id_for_task = my_node_id.clone();
                let cancel_for_task = cancel_token.clone();
                let handle = tokio::spawn(async move {
                    crate::migration::source_migration_with_disk_precopy(
                        vm_runtime,
                        vm_id,
                        op_id_for_task,
                        node_id_for_task,
                        destination_url,
                        disk_config,
                        Some(progress_reporter),
                        cancel_for_task,
                    )
                    .await
                });
                let abort_handle = handle.abort_handle();
                self.migration_tasks
                    .insert(operation_id.clone(), abort_handle, cancel_token);

                // Reaper: await the task, log terminal status, remove from registry.
                let reap_op = operation_id.clone();
                tokio::spawn(async move {
                    match handle.await {
                        Ok(Ok(())) => tracing::info!(
                            operation_id = %op_id_for_log,
                            "source migration task completed"
                        ),
                        Ok(Err(e)) => tracing::error!(
                            operation_id = %op_id_for_log,
                            error = %e,
                            "source migration task failed"
                        ),
                        Err(join_err) if join_err.is_cancelled() => tracing::warn!(
                            operation_id = %op_id_for_log,
                            "source migration task aborted"
                        ),
                        Err(join_err) => tracing::error!(
                            operation_id = %op_id_for_log,
                            error = %join_err,
                            "source migration task panicked"
                        ),
                    }
                    registry.remove(&reap_op);
                });
            }
            crate::migration::MigrationRole::Destination => {
                // Destination agent: open receive-migration socket.
                let (port, _listener) =
                    crate::migration::allocate_migration_port().map_err(|e| {
                        Status::resource_exhausted(format!(
                            "failed to allocate migration port: {}",
                            e
                        ))
                    })?;
                let receiver_url = crate::migration::build_receiver_url(port);

                tracing::info!(
                    vm_id = %vm_id,
                    operation_id = %operation_id,
                    receiver_url = %receiver_url,
                    "destination agent: ACKing migrate_vm, spawning receive-migration task"
                );

                // Destination side has no cooperative cancel-aware variant
                // yet (CH receive-migration is a single blocking call). The
                // cancel token is still tracked so the abort_handle path
                // force-drops the future at its next .await point.
                let cancel_token = CancellationToken::new();
                let registry = self.migration_tasks.clone();
                let op_id_for_log = operation_id.clone();
                let handle = crate::migration::spawn_destination_migration(
                    vm_runtime,
                    vm_id,
                    operation_id.clone(),
                    receiver_url,
                );
                let abort_handle = handle.abort_handle();
                self.migration_tasks
                    .insert(operation_id.clone(), abort_handle, cancel_token);

                let reap_op = operation_id.clone();
                tokio::spawn(async move {
                    match handle.await {
                        Ok(Ok(())) => tracing::info!(
                            operation_id = %op_id_for_log,
                            "destination migration task completed"
                        ),
                        Ok(Err(e)) => tracing::error!(
                            operation_id = %op_id_for_log,
                            error = %e,
                            "destination migration task failed"
                        ),
                        Err(join_err) if join_err.is_cancelled() => tracing::warn!(
                            operation_id = %op_id_for_log,
                            "destination migration task aborted"
                        ),
                        Err(join_err) => tracing::error!(
                            operation_id = %op_id_for_log,
                            error = %join_err,
                            "destination migration task panicked"
                        ),
                    }
                    registry.remove(&reap_op);
                });
            }
        }

        let observed_generation = self.cache.lock().await.observed_generation.clone();
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id,
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: format!("migration accepted as {:?}", role),
            }),
        }))
    }

    async fn update_overlay(
        &self,
        req: Request<proto::UpdateOverlayRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        // M2.2b single-writer enforcement: fail closed in core-managed
        // mode so this legacy nwd overlay update side effect can never run behind the
        // Core authority (Core M1 does not model it).
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "update_overlay is unsupported in core-managed mode",
            ));
        }
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let operation_id = meta.operation_id.clone();

        let mut nwd = crate::daemon_clients::NwdClient::connect(&self.nwd_socket)
            .await
            .map_err(|e| Status::unavailable(format!("nwd unavailable: {}", e)))?;

        nwd.update_overlay(
            &inner.network_id,
            inner.vni,
            inner
                .vtep_endpoints
                .iter()
                .map(|ep| chv_nwd_api::chv_nwd_api::VtepEndpoint {
                    node_id: ep.node_id.clone(),
                    vtep_ip: ep.vtep_ip.clone(),
                    vtep_port: ep.vtep_port,
                })
                .collect(),
            inner
                .fdb_entries
                .iter()
                .map(|fdb| chv_nwd_api::chv_nwd_api::FdbEntry {
                    mac_address: fdb.mac_address.clone(),
                    vtep_ip: fdb.vtep_ip.clone(),
                })
                .collect(),
            inner.fabric.as_ref().map(fabric_plan_to_nwd),
            Some(&operation_id),
        )
        .await
        .map_err(|e| Status::internal(format!("update_overlay failed: {}", e)))?;

        let observed_generation = self.cache.lock().await.observed_generation.clone();
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id,
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "overlay updated".to_string(),
            }),
        }))
    }

    async fn resolve_inspect_required_operation(
        &self,
        req: Request<proto::ResolveInspectRequiredOperationRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        // Inverse gate to the legacy-effectors fail-closed rule: this is the
        // operator egress for core-journal recovery and only exists in
        // core-managed mode.
        let authority = self.core_authority.as_ref().ok_or_else(|| {
            Status::unimplemented(
                "resolve_inspect_required_operation is only available in core-managed mode",
            )
        })?;
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        // Informational field, but it is echoed on the single-line audit
        // record: hold it to the same injection boundary as the note. (The
        // identifier! constructor only rejects empty strings.)
        if inner.vm_id.chars().any(|c| c.is_control()) {
            return Err(Status::invalid_argument(
                "vm_id must not contain control characters",
            ));
        }
        let vm_id = cellhv_core_types::VmId::new(&inner.vm_id)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let operation_id = cellhv_core_types::OperationId::new(&inner.operation_id)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let succeeded = match inner.disposition.as_str() {
            "succeeded" => true,
            "failed" => false,
            other => {
                return Err(Status::invalid_argument(format!(
                    "disposition must be \"succeeded\" or \"failed\", got {other:?}"
                )))
            }
        };
        let note = inner.note.trim();
        if note.is_empty() {
            return Err(Status::invalid_argument(
                "resolution note is required (operator inspection evidence)",
            ));
        }
        // The store bounds the whole recovery-evidence record at 16 KiB;
        // reject oversized notes as client input instead of surfacing the
        // store bound as an internal error.
        if note.len() > 8_000 {
            return Err(Status::invalid_argument(
                "resolution note must be at most 8000 bytes",
            ));
        }
        // Control characters are rejected outright: each one serializes as
        // an escape sequence (up to six bytes for a \u escape) in the
        // evidence record, so they are the only way a note could expand
        // past the 8000-byte bound into the store's 16 KiB record limit —
        // and they have no place in a single-line audit record (log
        // injection).
        if note.chars().any(|c| c.is_control()) {
            return Err(Status::invalid_argument(
                "resolution note must not contain control characters",
            ));
        }
        // The resolution's audit identity is taken from the caller's meta;
        // an empty identity would make the terminal audit record
        // unattributable. Mirrors the M2.1b submit-path rule.
        let requested_by = meta.requested_by.trim();
        if requested_by.is_empty() {
            return Err(Status::invalid_argument(
                "requested_by is required (resolution audit identity)",
            ));
        }
        // Same injection boundary as the note: the audit line carries this
        // identity and must stay single-line.
        if requested_by.chars().any(|c| c.is_control()) {
            return Err(Status::invalid_argument(
                "requested_by must not contain control characters",
            ));
        }
        let resolved = authority
            .resolve_inspect_required(operation_id, succeeded, note.to_owned())
            .await
            .map_err(map_authority_error)?;
        // Audit trail: the resolution terminal-persists a stuck operation;
        // record who decided what, with the journal's own identifiers as the
        // authoritative reference. Journal identifiers are escape_debug'd,
        // not trusted: identifier admission historically accepts control
        // characters, and this record must stay single-line.
        tracing::info!(
            operation_id = %resolved.entry.operation.id.as_str().escape_debug(),
            vm_id = %resolved.entry.operation.vm_id.as_str().escape_debug(),
            requested_vm_id = %vm_id,
            succeeded,
            note,
            requested_by,
            "operator resolved inspect-required operation"
        );
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id.clone(),
                status: "ok".to_string(),
                node_observed_generation: self.cache.lock().await.observed_generation.clone(),
                error_code: "".to_string(),
                human_summary: format!("{:?}", resolved.disposition),
            }),
        }))
    }

    async fn send_gratuitous_arp(
        &self,
        req: Request<proto::SendGratuitousArpRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        // M2.2b single-writer enforcement: fail closed in core-managed
        // mode so this legacy nwd gratuitous ARP side effect can never run behind the
        // Core authority (Core M1 does not model it).
        if self.core_authority.is_some() {
            return Err(Status::unimplemented(
                "send_gratuitous_arp is unsupported in core-managed mode",
            ));
        }
        let inner = req.into_inner();
        let meta = inner
            .meta
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing meta"))?;
        let operation_id = meta.operation_id.clone();

        let mut nwd = crate::daemon_clients::NwdClient::connect(&self.nwd_socket)
            .await
            .map_err(|e| Status::unavailable(format!("nwd unavailable: {}", e)))?;

        nwd.send_gratuitous_arp(
            &inner.network_id,
            &inner.vm_ip,
            &inner.bridge_name,
            Some(&operation_id),
        )
        .await
        .map_err(|e| Status::internal(format!("send_gratuitous_arp failed: {}", e)))?;

        let observed_generation = self.cache.lock().await.observed_generation.clone();
        Ok(Response::new(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id,
                status: "ok".to_string(),
                node_observed_generation: observed_generation,
                error_code: "".to_string(),
                human_summary: "gratuitous ARP sent".to_string(),
            }),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chv_agent_runtime_ch::mock::MockCloudHypervisorAdapter;
    use std::sync::Arc;

    fn test_server() -> AgentServer {
        let mut cache = NodeCache::new("node-1");
        cache.node_state = crate::state_machine::NodeState::TenantReady
            .as_str()
            .to_string();
        cache.connectivity_state = crate::connectivity::ConnectivityState::Connected;
        AgentServer::new(
            Arc::new(tokio::sync::Mutex::new(cache)),
            VmRuntime::new(Arc::new(MockCloudHypervisorAdapter::default())),
            std::path::PathBuf::from("/run/chv/stord/api.sock"),
            std::path::PathBuf::from("/run/chv/nwd/api.sock"),
            None,
            std::path::PathBuf::from("/var/lib/chv/agent"),
        )
    }

    /// `cancel_migration` returns false when the op_id is unknown — proves
    /// the public API doesn't accidentally swallow operator errors.
    #[tokio::test]
    async fn cancel_migration_returns_false_for_unknown_op_id() {
        let server = test_server();
        assert!(!server.cancel_migration("op-does-not-exist"));
        assert_eq!(server.migration_tasks.len(), 0);
    }

    /// `shutdown_migrations` is a no-op on an empty registry — proves the
    /// graceful-shutdown entry point is safe to call unconditionally.
    #[tokio::test]
    async fn shutdown_migrations_on_empty_registry_is_noop() {
        let server = test_server();
        server.shutdown_migrations();
        assert_eq!(server.migration_tasks.len(), 0);
    }

    fn test_meta(desired_state_version: &str) -> proto::RequestMeta {
        proto::RequestMeta {
            operation_id: "op-1".to_string(),
            requested_by: "cp".to_string(),
            target_node_id: "node-1".to_string(),
            desired_state_version: desired_state_version.to_string(),
            request_unix_ms: 0,
        }
    }

    // ---------------------------------------------------------------------
    // #662: control-plane → agent authentication over the Unix socket.
    // ---------------------------------------------------------------------

    const TEST_AGENT_TOKEN: &str = "agent-token-0123456789abcdef0123456789";

    async fn spawn_agent_with_auth(auth: AgentAuth) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("agent.sock");
        let server = test_server().with_agent_auth(auth);
        let server_socket = socket.clone();
        tokio::spawn(async move {
            let _ = server.serve(&server_socket).await;
        });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(socket.exists(), "agent socket was not bound");
        (dir, socket)
    }

    async fn connect_reconcile(
        socket: &std::path::Path,
    ) -> proto::reconcile_service_client::ReconcileServiceClient<tonic::transport::Channel> {
        let path = socket.to_path_buf();
        let channel = tonic::transport::Endpoint::try_from("http://[::]:50051")
            .expect("endpoint")
            .connect_with_connector(tower::service_fn(move |_| {
                let path = path.clone();
                async move {
                    tokio::net::UnixStream::connect(path)
                        .await
                        .map(hyper_util::rt::TokioIo::new)
                }
            }))
            .await
            .expect("connect to agent");
        proto::reconcile_service_client::ReconcileServiceClient::new(channel)
    }

    fn node_desired_state_request() -> proto::ApplyNodeDesiredStateRequest {
        proto::ApplyNodeDesiredStateRequest {
            meta: Some(test_meta("1")),
            node_id: "node-1".to_string(),
            fragment: None,
        }
    }

    fn with_token<T>(req: T, token: &str) -> Request<T> {
        let mut request = Request::new(req);
        request.metadata_mut().insert(
            chv_common::NODE_AUTH_METADATA_KEY,
            tonic::metadata::MetadataValue::try_from(token).expect("metadata value"),
        );
        request
    }

    #[tokio::test]
    async fn agent_rejects_missing_token_over_uds() {
        let (_dir, socket) = spawn_agent_with_auth(AgentAuth::new(
            TEST_AGENT_TOKEN,
            false,
            vec![nix::unistd::geteuid().as_raw()],
        ))
        .await;
        let mut client = connect_reconcile(&socket).await;
        let status = client
            .apply_node_desired_state(node_desired_state_request())
            .await
            .expect_err("missing token must be rejected");
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
        assert!(status.message().contains("missing"));
    }

    #[tokio::test]
    async fn agent_rejects_wrong_token_over_uds() {
        let (_dir, socket) = spawn_agent_with_auth(AgentAuth::new(
            TEST_AGENT_TOKEN,
            false,
            vec![nix::unistd::geteuid().as_raw()],
        ))
        .await;
        let mut client = connect_reconcile(&socket).await;
        let status = client
            .apply_node_desired_state(with_token(
                node_desired_state_request(),
                "wrong-token-0123456789abcdef0123456789",
            ))
            .await
            .expect_err("wrong token must be rejected");
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
    }

    #[tokio::test]
    async fn agent_accepts_authorized_control_plane_token() {
        let (_dir, socket) = spawn_agent_with_auth(AgentAuth::new(
            TEST_AGENT_TOKEN,
            false,
            vec![nix::unistd::geteuid().as_raw()],
        ))
        .await;
        let mut client = connect_reconcile(&socket).await;
        let response = client
            .apply_node_desired_state(with_token(node_desired_state_request(), TEST_AGENT_TOKEN))
            .await
            .expect("authorized token must reach the handler");
        assert_eq!(
            response.into_inner().result.expect("result meta").status,
            "ok"
        );
    }

    #[test]
    fn agent_auth_deny_all_rejects_every_token() {
        let mut auth = AgentAuth::deny_all();
        let err = tonic::service::Interceptor::call(&mut auth, with_token((), TEST_AGENT_TOKEN))
            .expect_err("deny-all must reject");
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn agent_peer_uid_gate_defaults_to_own_euid() {
        let auth = AgentAuth::new(TEST_AGENT_TOKEN, false, Vec::new());
        let own = nix::unistd::geteuid().as_raw();
        assert!(auth.peer_uid_allowed(own));
        assert!(!auth.peer_uid_allowed(own.wrapping_add(1)));
    }

    #[test]
    fn insecure_agent_auth_bypasses_token_and_peer_checks() {
        let mut auth = AgentAuth::new(TEST_AGENT_TOKEN, true, Vec::new());
        assert!(auth.peer_uid_allowed(12345));
        assert!(tonic::service::Interceptor::call(&mut auth, Request::new(()),).is_ok());
    }

    #[tokio::test]
    async fn apply_vm_desired_state_updates_generation_and_fragment() {
        let server = test_server();
        let req = proto::ApplyVmDesiredStateRequest {
            meta: Some(test_meta("5")),
            node_id: "node-1".to_string(),
            vm_id: "vm-1".to_string(),
            fragment: Some(proto::DesiredStateFragment {
                id: "vm-1".to_string(),
                kind: "vm".to_string(),
                generation: "5".to_string(),
                spec_json: vec![],
                policy_json: vec![],
                updated_at: "".to_string(),
                updated_by: "".to_string(),
            }),
        };
        let resp = proto::reconcile_service_server::ReconcileService::apply_vm_desired_state(
            &server,
            Request::new(req),
        )
        .await;
        assert!(resp.is_ok());
        let cache = server.cache.lock().await;
        assert_eq!(cache.get_generation("vm", "vm-1"), Some(&"5".to_string()));
        assert!(cache.get_fragment("vm", "vm-1").is_some());
    }

    #[tokio::test]
    async fn create_vm_lifecycle_flow() {
        let server = test_server();
        let vm_spec_json = r#"{"name":"vm-1","cpus":2,"memory_bytes":1024,"kernel_path":"/dev/null","disks":[],"nics":[]}"#;
        let create_req = proto::CreateVmRequest {
            meta: Some(test_meta("1")),
            node_id: "node-1".to_string(),
            vm: Some(proto::VmMutationSpec {
                vm_id: "vm-1".to_string(),
                vm_spec_json: vm_spec_json.as_bytes().to_vec(),
            }),
        };
        let resp = proto::lifecycle_service_server::LifecycleService::create_vm(
            &server,
            Request::new(create_req),
        )
        .await;
        assert!(resp.is_ok());

        let start_req = proto::StartVmRequest {
            meta: Some(test_meta("1")),
            node_id: "node-1".to_string(),
            vm_id: "vm-1".to_string(),
        };
        let resp = proto::lifecycle_service_server::LifecycleService::start_vm(
            &server,
            Request::new(start_req),
        )
        .await;
        assert!(resp.is_ok());
        assert_eq!(
            server.vm_runtime.get("vm-1").await.unwrap().runtime_status,
            "Running"
        );

        let stop_req = proto::StopVmRequest {
            meta: Some(test_meta("1")),
            node_id: "node-1".to_string(),
            vm_id: "vm-1".to_string(),
            force: false,
        };
        let resp = proto::lifecycle_service_server::LifecycleService::stop_vm(
            &server,
            Request::new(stop_req),
        )
        .await;
        assert!(resp.is_ok());
        assert_eq!(
            server.vm_runtime.get("vm-1").await.unwrap().runtime_status,
            "Stopped"
        );
    }

    #[tokio::test]
    async fn lifecycle_stale_generation_rejected() {
        let server = test_server();
        let mut cache = server.cache.lock().await;
        cache.observe_generation("vm", "vm-1", "10");
        drop(cache);

        let req = proto::StartVmRequest {
            meta: Some(test_meta("9")),
            node_id: "node-1".to_string(),
            vm_id: "vm-1".to_string(),
        };
        let resp =
            proto::lifecycle_service_server::LifecycleService::start_vm(&server, Request::new(req))
                .await;
        assert_eq!(resp.unwrap_err().code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn reboot_vm_success() {
        let server = test_server();
        let vm_spec_json = r#"{"name":"vm-1","cpus":2,"memory_bytes":1024,"kernel_path":"/dev/null","disks":[],"nics":[]}"#;
        let create_req = proto::CreateVmRequest {
            meta: Some(test_meta("1")),
            node_id: "node-1".to_string(),
            vm: Some(proto::VmMutationSpec {
                vm_id: "vm-1".to_string(),
                vm_spec_json: vm_spec_json.as_bytes().to_vec(),
            }),
        };
        let resp = proto::lifecycle_service_server::LifecycleService::create_vm(
            &server,
            Request::new(create_req),
        )
        .await;
        assert!(resp.is_ok());

        let reboot_req = proto::RebootVmRequest {
            meta: Some(test_meta("1")),
            node_id: "node-1".to_string(),
            vm_id: "vm-1".to_string(),
            force: false,
        };
        let resp = proto::lifecycle_service_server::LifecycleService::reboot_vm(
            &server,
            Request::new(reboot_req),
        )
        .await;
        assert!(resp.is_ok());
        assert_eq!(
            server.vm_runtime.get("vm-1").await.unwrap().runtime_status,
            "Running"
        );
    }

    #[tokio::test]
    async fn acknowledge_generation_matches() {
        let server = test_server();
        let mut cache = server.cache.lock().await;
        cache.observe_generation("vm", "vm-1", "5");
        drop(cache);

        let req = proto::AcknowledgeDesiredStateVersionRequest {
            meta: Some(test_meta("5")),
            node_id: "node-1".to_string(),
            fragment_kind: "vm".to_string(),
            fragment_id: "vm-1".to_string(),
            observed_generation: "5".to_string(),
            apply_status: "ok".to_string(),
        };
        let resp =
            proto::reconcile_service_server::ReconcileService::acknowledge_desired_state_version(
                &server,
                Request::new(req),
            )
            .await;
        assert!(resp.is_ok());
    }

    #[tokio::test]
    async fn acknowledge_generation_mismatch() {
        let server = test_server();
        let mut cache = server.cache.lock().await;
        cache.observe_generation("vm", "vm-1", "5");
        drop(cache);

        let req = proto::AcknowledgeDesiredStateVersionRequest {
            meta: Some(test_meta("4")),
            node_id: "node-1".to_string(),
            fragment_kind: "vm".to_string(),
            fragment_id: "vm-1".to_string(),
            observed_generation: "4".to_string(),
            apply_status: "ok".to_string(),
        };
        let resp =
            proto::reconcile_service_server::ReconcileService::acknowledge_desired_state_version(
                &server,
                Request::new(req),
            )
            .await;
        assert_eq!(resp.unwrap_err().code(), tonic::Code::FailedPrecondition);
    }

    /// Shared log of the `open_volume` RPCs the mock stord received:
    /// `(volume_id, backend_class, locator)` per call, in call order — so
    /// tests can pin the backend class and locator the agent threads into
    /// its opens (#379 PR 1; the locator joins in PR 2 to pin that
    /// populating the class does NOT change the locator default).
    /// `open_options` records each call's options map at the same index
    /// (#513 PR 1: the create handler's sized open — `size_bytes` — is
    /// pinned there). #522 PR 1 adds the delete-carrier pins:
    /// `closes`/`destroys` record the `close_volume`/`destroy_volume`
    /// calls, and `events` records the interleaved order of opens,
    /// closes, and destroys so the close→destroy sequence of the
    /// `delete_volume` handler can be pinned. #540 adds `clones`: the
    /// `prepare_clone` calls as `(source_volume_id, clone_name)` — the
    /// clone target id the agent threads (the whole basis of the
    /// carrier-locator dest) — plus its `prepare_clone:{id}` event.
    #[derive(Clone, Default)]
    struct StordOpenLog {
        opens: Arc<std::sync::Mutex<Vec<(String, String, String)>>>,
        open_options: Arc<std::sync::Mutex<Vec<std::collections::HashMap<String, String>>>>,
        closes: Arc<std::sync::Mutex<Vec<(String, String)>>>,
        destroys: Arc<std::sync::Mutex<Vec<(String, String, String)>>>,
        clones: Arc<std::sync::Mutex<Vec<(String, String)>>>,
        events: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[derive(Default)]
    struct MockStord {
        opens: StordOpenLog,
    }

    #[tonic::async_trait]
    impl chv_stord_api::chv_stord_api::storage_service_server::StorageService for MockStord {
        async fn list_volume_sessions(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::ListVolumeSessionsRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::ListVolumeSessionsResponse>, Status>
        {
            Ok(Response::new(
                chv_stord_api::chv_stord_api::ListVolumeSessionsResponse { sessions: vec![] },
            ))
        }

        async fn open_volume(
            &self,
            req: Request<chv_stord_api::chv_stord_api::OpenVolumeRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::OpenVolumeResponse>, Status> {
            let inner = req.into_inner();
            // #379 PR 1: record the backend class the agent threaded into
            // this open so the class-plumbing tests can pin it; PR 2 adds
            // the locator for the same reason. #513 PR 1 adds the options
            // map (index-aligned with `opens`) so the create handler's
            // sized open can be pinned.
            self.opens.open_options.lock().unwrap().push(
                inner
                    .backend
                    .as_ref()
                    .map(|b| b.options.clone())
                    .unwrap_or_default(),
            );
            self.opens.opens.lock().unwrap().push((
                inner.volume_id.clone(),
                inner
                    .backend
                    .as_ref()
                    .map(|b| b.backend_class.clone())
                    .unwrap_or_default(),
                inner
                    .backend
                    .as_ref()
                    .map(|b| b.locator.clone())
                    .unwrap_or_default(),
            ));
            self.opens
                .events
                .lock()
                .unwrap()
                .push(format!("open:{}", inner.volume_id));
            Ok(Response::new(
                chv_stord_api::chv_stord_api::OpenVolumeResponse {
                    result: Some(chv_stord_api::chv_stord_api::Result {
                        status: "ok".to_string(),
                        error_code: "".to_string(),
                        human_summary: "".to_string(),
                    }),
                    volume_id: inner.volume_id,
                    attachment_handle: "handle-1".to_string(),
                    export_kind: "".to_string(),
                    export_path: "".to_string(),
                },
            ))
        }

        async fn attach_volume_to_vm(
            &self,
            req: Request<chv_stord_api::chv_stord_api::AttachVolumeToVmRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::AttachVolumeToVmResponse>, Status>
        {
            let inner = req.into_inner();
            Ok(Response::new(
                chv_stord_api::chv_stord_api::AttachVolumeToVmResponse {
                    result: Some(chv_stord_api::chv_stord_api::Result {
                        status: "ok".to_string(),
                        error_code: "".to_string(),
                        human_summary: "".to_string(),
                    }),
                    volume_id: inner.volume_id,
                    vm_id: inner.vm_id,
                    export_kind: "".to_string(),
                    export_path: "".to_string(),
                },
            ))
        }

        async fn close_volume(
            &self,
            req: Request<chv_stord_api::chv_stord_api::CloseVolumeRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::Result>, Status> {
            // #522 PR 1: record the close so the delete-carrier tests
            // can pin the close→destroy ordering and the handle the
            // agent threaded (the cached handle, not a fabricated one).
            let inner = req.into_inner();
            self.opens
                .closes
                .lock()
                .unwrap()
                .push((inner.volume_id.clone(), inner.attachment_handle.clone()));
            self.opens
                .events
                .lock()
                .unwrap()
                .push(format!("close:{}", inner.volume_id));
            Ok(Response::new(chv_stord_api::chv_stord_api::Result {
                status: "ok".to_string(),
                error_code: "".to_string(),
                human_summary: "".to_string(),
            }))
        }

        async fn destroy_volume(
            &self,
            req: Request<chv_stord_api::chv_stord_api::DestroyVolumeRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::Result>, Status> {
            // #522 PR 1: record the destroy so the delete-carrier tests
            // can pin the DP4 carrier locator and class the agent
            // threaded, and the close→destroy ordering.
            let inner = req.into_inner();
            let (class, locator) = inner
                .backend
                .as_ref()
                .map(|b| (b.backend_class.clone(), b.locator.clone()))
                .unwrap_or_default();
            self.opens.destroys.lock().unwrap().push((
                inner.volume_id.clone(),
                class.clone(),
                locator.clone(),
            ));
            self.opens
                .events
                .lock()
                .unwrap()
                .push(format!("destroy:{}", inner.volume_id));
            Ok(Response::new(chv_stord_api::chv_stord_api::Result {
                status: "ok".to_string(),
                error_code: "".to_string(),
                human_summary: "".to_string(),
            }))
        }

        async fn get_volume_health(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::VolumeHealthRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::VolumeHealthResponse>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn detach_volume_from_vm(
            &self,
            req: Request<chv_stord_api::chv_stord_api::DetachVolumeFromVmRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::Result>, Status> {
            // #533: ok (not unimplemented) so the attach→detach→delete
            // lifecycle test can drive the detach between the open and
            // the destroy — recorded in the event log to pin the
            // interleaving.
            let inner = req.into_inner();
            self.opens
                .events
                .lock()
                .unwrap()
                .push(format!("detach:{}", inner.volume_id));
            Ok(Response::new(chv_stord_api::chv_stord_api::Result {
                status: "ok".to_string(),
                error_code: "".to_string(),
                human_summary: "".to_string(),
            }))
        }

        async fn resize_volume(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::ResizeVolumeRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::Result>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn prepare_snapshot(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::PrepareSnapshotRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::Result>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn prepare_clone(
            &self,
            req: Request<chv_stord_api::chv_stord_api::PrepareCloneRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::Result>, Status> {
            // #540: record the clone so the clone lifecycle test can pin
            // the (source, target) pair the agent threaded — the target
            // id IS the clone name stord materializes the carrier
            // locator `{clone_id}.img` for.
            let inner = req.into_inner();
            self.opens
                .clones
                .lock()
                .unwrap()
                .push((inner.volume_id.clone(), inner.clone_name.clone()));
            self.opens
                .events
                .lock()
                .unwrap()
                .push(format!("prepare_clone:{}", inner.clone_name));
            Ok(Response::new(chv_stord_api::chv_stord_api::Result {
                status: "ok".to_string(),
                error_code: "".to_string(),
                human_summary: "".to_string(),
            }))
        }

        async fn restore_snapshot(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::RestoreSnapshotRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::Result>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn delete_snapshot(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::DeleteSnapshotRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::Result>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn set_device_policy(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::SetDevicePolicyRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::Result>, Status> {
            Err(Status::unimplemented(""))
        }
        async fn trigger_disk_migration(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::TriggerDiskMigrationRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::TriggerDiskMigrationResponse>, Status>
        {
            Err(Status::unimplemented(""))
        }
        async fn get_disk_migration_status(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::GetDiskMigrationStatusRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::GetDiskMigrationStatusResponse>, Status>
        {
            Err(Status::unimplemented(""))
        }
        async fn resume_disk_migration(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::ResumeDiskMigrationRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::ResumeDiskMigrationResponse>, Status>
        {
            Err(Status::unimplemented(""))
        }
    }

    #[derive(Clone, Default)]
    struct CleanupTracker {
        detached_volumes: Arc<std::sync::Mutex<Vec<String>>>,
        closed_volumes: Arc<std::sync::Mutex<Vec<String>>>,
        detached_nics: Arc<std::sync::Mutex<Vec<String>>>,
    }

    struct MockCleanupStord {
        tracker: CleanupTracker,
    }

    #[tonic::async_trait]
    impl chv_stord_api::chv_stord_api::storage_service_server::StorageService for MockCleanupStord {
        async fn list_volume_sessions(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::ListVolumeSessionsRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::ListVolumeSessionsResponse>, Status>
        {
            Ok(Response::new(
                chv_stord_api::chv_stord_api::ListVolumeSessionsResponse { sessions: vec![] },
            ))
        }

        async fn open_volume(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::OpenVolumeRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::OpenVolumeResponse>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn attach_volume_to_vm(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::AttachVolumeToVmRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::AttachVolumeToVmResponse>, Status>
        {
            Err(Status::unimplemented(""))
        }

        async fn close_volume(
            &self,
            req: Request<chv_stord_api::chv_stord_api::CloseVolumeRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::Result>, Status> {
            self.tracker
                .closed_volumes
                .lock()
                .unwrap()
                .push(req.into_inner().volume_id);
            Ok(Response::new(chv_stord_api::chv_stord_api::Result {
                status: "ok".to_string(),
                error_code: "".to_string(),
                human_summary: "".to_string(),
            }))
        }

        // #522 PR 1: not driven by the cleanup tests (VM delete stops
        // at close, by design — the reclaim primitive is the
        // delete-volume carrier's, not the VM-delete path's).
        async fn destroy_volume(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::DestroyVolumeRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::Result>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn get_volume_health(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::VolumeHealthRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::VolumeHealthResponse>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn detach_volume_from_vm(
            &self,
            req: Request<chv_stord_api::chv_stord_api::DetachVolumeFromVmRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::Result>, Status> {
            self.tracker
                .detached_volumes
                .lock()
                .unwrap()
                .push(req.into_inner().volume_id);
            Ok(Response::new(chv_stord_api::chv_stord_api::Result {
                status: "ok".to_string(),
                error_code: "".to_string(),
                human_summary: "".to_string(),
            }))
        }

        async fn resize_volume(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::ResizeVolumeRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::Result>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn prepare_snapshot(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::PrepareSnapshotRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::Result>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn prepare_clone(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::PrepareCloneRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::Result>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn restore_snapshot(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::RestoreSnapshotRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::Result>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn delete_snapshot(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::DeleteSnapshotRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::Result>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn set_device_policy(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::SetDevicePolicyRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::Result>, Status> {
            Err(Status::unimplemented(""))
        }
        async fn trigger_disk_migration(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::TriggerDiskMigrationRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::TriggerDiskMigrationResponse>, Status>
        {
            Err(Status::unimplemented(""))
        }
        async fn get_disk_migration_status(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::GetDiskMigrationStatusRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::GetDiskMigrationStatusResponse>, Status>
        {
            Err(Status::unimplemented(""))
        }
        async fn resume_disk_migration(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::ResumeDiskMigrationRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::ResumeDiskMigrationResponse>, Status>
        {
            Err(Status::unimplemented(""))
        }
    }

    struct MockCleanupNwd {
        tracker: CleanupTracker,
    }

    #[tonic::async_trait]
    impl chv_nwd_api::chv_nwd_api::network_service_server::NetworkService for MockCleanupNwd {
        async fn list_namespace_state(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::ListNamespaceStateRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::ListNamespaceStateResponse>, Status>
        {
            Ok(Response::new(
                chv_nwd_api::chv_nwd_api::ListNamespaceStateResponse { items: vec![] },
            ))
        }

        async fn ensure_network_topology(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::EnsureNetworkTopologyRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::Result>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn delete_network_topology(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::DeleteNetworkTopologyRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::Result>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn get_network_health(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::NetworkHealthRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::NetworkHealthResponse>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn attach_vm_nic(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::AttachVmNicRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::AttachVmNicResponse>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn detach_vm_nic(
            &self,
            req: Request<chv_nwd_api::chv_nwd_api::DetachVmNicRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::Result>, Status> {
            let nic_id = req.into_inner().nic_id;
            self.tracker.detached_nics.lock().unwrap().push(nic_id);
            Ok(Response::new(chv_nwd_api::chv_nwd_api::Result {
                status: "ok".to_string(),
                error_code: "".to_string(),
                human_summary: "".to_string(),
            }))
        }

        async fn set_firewall_policy(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::SetFirewallPolicyRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::Result>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn set_nat_policy(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::SetNatPolicyRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::Result>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn ensure_dhcp_scope(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::EnsureDhcpScopeRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::Result>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn ensure_dns_scope(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::EnsureDnsScopeRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::Result>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn expose_service(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::ExposeServiceRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::Result>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn withdraw_service_exposure(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::WithdrawServiceExposureRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::Result>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn update_overlay(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::UpdateOverlayRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::UpdateOverlayResponse>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn update_security_policy(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::SecurityPolicy>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::UpdateSecurityPolicyResponse>, Status>
        {
            Err(Status::unimplemented(""))
        }

        async fn update_rate_limit(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::RateLimitPolicy>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::UpdateRateLimitResponse>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn get_overlay_status(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::GetOverlayStatusRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::OverlayStatus>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn get_fabric_identity(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::GetFabricIdentityRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::FabricIdentityResponse>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn send_gratuitous_arp(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::SendGratuitousArpRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::SendGratuitousArpResponse>, Status> {
            Err(Status::unimplemented(""))
        }
    }

    #[tokio::test]
    async fn delete_vm_cleans_up_storage_and_network_resources() {
        let dir = tempfile::tempdir().unwrap();
        let stord_socket = dir.path().join("cleanup-stord.sock");
        let nwd_socket = dir.path().join("cleanup-nwd.sock");
        let tracker = CleanupTracker::default();

        {
            let tracker = tracker.clone();
            let uds = tokio::net::UnixListener::bind(&stord_socket).unwrap();
            tokio::spawn(async move {
                tonic::transport::Server::builder()
                    .add_service(
                        chv_stord_api::chv_stord_api::storage_service_server::StorageServiceServer::new(
                            MockCleanupStord { tracker },
                        ),
                    )
                    .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds))
                    .await
                    .ok();
            });
        }

        {
            let tracker = tracker.clone();
            let uds = tokio::net::UnixListener::bind(&nwd_socket).unwrap();
            tokio::spawn(async move {
                tonic::transport::Server::builder()
                    .add_service(
                        chv_nwd_api::chv_nwd_api::network_service_server::NetworkServiceServer::new(
                            MockCleanupNwd { tracker },
                        ),
                    )
                    .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds))
                    .await
                    .ok();
            });
        }

        // No startup race: the Unix listener is bound (listen() done)
        // before the server task is spawned, so connect() succeeds via
        // the kernel backlog even before the task accepts.

        let mut cache = NodeCache::new("node-1");
        cache.node_state = crate::state_machine::NodeState::TenantReady
            .as_str()
            .to_string();
        cache.connectivity_state = crate::connectivity::ConnectivityState::Connected;
        cache.observe_generation("vm", "vm-1", "1");
        cache.observe_vm_attachment(
            "vm-1",
            &["vol-1".to_string()],
            &[VmNicAttachment {
                nic_id: "vm-1-net-1".to_string(),
                network_id: "net-1".to_string(),
            }],
        );
        cache
            .volume_handles
            .insert("vol-1".to_string(), "handle-1".to_string());

        let runtime = VmRuntime::new(Arc::new(MockCloudHypervisorAdapter::default()));
        let config = VmConfig {
            vm_id: "vm-1".to_string(),
            cpus: 1,
            memory_bytes: 1024,
            kernel_path: std::path::PathBuf::from("/dev/null"),
            firmware_path: None,
            disks: vec![],
            nics: vec![],
            api_socket_path: dir.path().join("vms/vm-1/vm.sock"),
            cloud_init_userdata: None,
            hypervisor_overrides: None,
            vsock: None,
        };
        runtime
            .create_vm("vm-1", "1", &config, Some("op-1"))
            .await
            .unwrap();

        let server = AgentServer::new(
            Arc::new(tokio::sync::Mutex::new(cache)),
            runtime,
            stord_socket,
            nwd_socket,
            None,
            dir.path().to_path_buf(),
        );

        let req = proto::DeleteVmRequest {
            meta: Some(test_meta("1")),
            node_id: "node-1".to_string(),
            vm_id: "vm-1".to_string(),
            force: false,
        };
        let resp = proto::lifecycle_service_server::LifecycleService::delete_vm(
            &server,
            Request::new(req),
        )
        .await;
        assert!(resp.is_ok());

        assert_eq!(
            tracker.detached_volumes.lock().unwrap().as_slice(),
            ["vol-1"]
        );
        assert_eq!(tracker.closed_volumes.lock().unwrap().as_slice(), ["vol-1"]);
        assert_eq!(
            tracker.detached_nics.lock().unwrap().as_slice(),
            ["vm-1-net-1"]
        );
        assert!(server.vm_runtime.get("vm-1").await.is_none());
        assert!(!server
            .cache
            .lock()
            .await
            .volume_handles
            .contains_key("vol-1"));
    }

    #[tokio::test]
    async fn delete_vm_removes_cached_desired_state() {
        let server = test_server();
        {
            let mut cache = server.cache.lock().await;
            cache.observe_generation("vm", "vm-1", "1");
            cache.store_fragment(
                "vm",
                "vm-1",
                crate::cache::DesiredStateFragment {
                    id: "vm-1".to_string(),
                    kind: "vm".to_string(),
                    generation: "1".to_string(),
                    spec_json: vec![],
                    policy_json: vec![],
                    updated_at: String::new(),
                    updated_by: String::new(),
                },
            );
        }

        let config = VmConfig {
            vm_id: "vm-1".to_string(),
            cpus: 1,
            memory_bytes: 1024,
            kernel_path: std::path::PathBuf::from("/dev/null"),
            firmware_path: None,
            disks: vec![],
            nics: vec![],
            api_socket_path: std::path::PathBuf::from("/var/lib/chv/agent/vms/vm-1/vm.sock"),
            cloud_init_userdata: None,
            hypervisor_overrides: None,
            vsock: None,
        };
        server
            .vm_runtime
            .create_vm("vm-1", "1", &config, Some("op-1"))
            .await
            .unwrap();

        let req = proto::DeleteVmRequest {
            meta: Some(test_meta("1")),
            node_id: "node-1".to_string(),
            vm_id: "vm-1".to_string(),
            force: false,
        };
        let resp = proto::lifecycle_service_server::LifecycleService::delete_vm(
            &server,
            Request::new(req),
        )
        .await;
        assert!(resp.is_ok());

        let cache = server.cache.lock().await;
        assert!(cache.get_generation("vm", "vm-1").is_none());
        assert!(cache.get_fragment("vm", "vm-1").is_none());
        assert!(cache.vm_attachment_state("vm-1").is_none());
    }

    #[tokio::test]
    async fn apply_volume_desired_state_attaches_to_vm() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("stord.sock");

        let uds = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(
                    chv_stord_api::chv_stord_api::storage_service_server::StorageServiceServer::new(
                        MockStord::default(),
                    ),
                )
                .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds))
                .await
                .ok();
        });

        // No startup race: the Unix listener is bound (listen() done)
        // before the server task is spawned, so connect() succeeds via
        // the kernel backlog even before the task accepts.

        let mut cache = NodeCache::new("node-1");
        cache.node_state = crate::state_machine::NodeState::TenantReady
            .as_str()
            .to_string();
        cache.connectivity_state = crate::connectivity::ConnectivityState::Connected;
        let server = AgentServer::new(
            Arc::new(tokio::sync::Mutex::new(cache)),
            VmRuntime::new(Arc::new(MockCloudHypervisorAdapter::default())),
            socket,
            std::path::PathBuf::from("/run/chv/nwd/api.sock"),
            None,
            dir.path().to_path_buf(),
        );

        let req = proto::ApplyVolumeDesiredStateRequest {
            meta: Some(test_meta("1")),
            node_id: "node-1".to_string(),
            volume_id: "vol-1".to_string(),
            fragment: Some(proto::DesiredStateFragment {
                id: "vol-1".to_string(),
                kind: "volume".to_string(),
                generation: "1".to_string(),
                spec_json: r#"{"vm_id":"vm-1","backend_class":"local","locator":"vol-1.img"}"#
                    .as_bytes()
                    .to_vec(),
                policy_json: vec![],
                updated_at: "".to_string(),
                updated_by: "".to_string(),
            }),
        };
        let resp = proto::reconcile_service_server::ReconcileService::apply_volume_desired_state(
            &server,
            Request::new(req),
        )
        .await;
        assert!(resp.is_ok());
        let cache = server.cache.lock().await;
        assert_eq!(
            cache.volume_handles.get("vol-1"),
            Some(&"handle-1".to_string())
        );
    }

    #[tokio::test]
    async fn apply_volume_desired_state_threads_backend_class_to_stord_open() {
        // #379 PR 1 pin (A4/B5 — the parser/default seam): the attach
        // handler's `open_and_attach_volume` threads the spec_json's
        // `backend_class` into the stord open and defaults to "local"
        // when the key is absent. (The handler already parsed the field;
        // this pins that the parsed value actually reaches stord, which
        // no pre-#379 test observed.)
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("stord.sock");
        let opens = StordOpenLog::default();

        {
            let opens = opens.clone();
            let uds = tokio::net::UnixListener::bind(&socket).unwrap();
            tokio::spawn(async move {
                tonic::transport::Server::builder()
                    .add_service(
                        chv_stord_api::chv_stord_api::storage_service_server::StorageServiceServer::new(
                            MockStord { opens },
                        ),
                    )
                    .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds))
                    .await
                    .ok();
            });
        }

        let mut cache = NodeCache::new("node-1");
        cache.node_state = crate::state_machine::NodeState::TenantReady
            .as_str()
            .to_string();
        cache.connectivity_state = crate::connectivity::ConnectivityState::Connected;
        let server = AgentServer::new(
            Arc::new(tokio::sync::Mutex::new(cache)),
            VmRuntime::new(Arc::new(MockCloudHypervisorAdapter::default())),
            socket,
            std::path::PathBuf::from("/run/chv/nwd/api.sock"),
            None,
            dir.path().to_path_buf(),
        );

        let req = proto::ApplyVolumeDesiredStateRequest {
            meta: Some(test_meta("1")),
            node_id: "node-1".to_string(),
            volume_id: "vol-cls-a".to_string(),
            fragment: Some(proto::DesiredStateFragment {
                id: "vol-cls-a".to_string(),
                kind: "volume".to_string(),
                generation: "1".to_string(),
                spec_json: r#"{"vm_id":"vm-1","backend_class":"lvm","locator":"vol-cls-a.img"}"#
                    .as_bytes()
                    .to_vec(),
                policy_json: vec![],
                updated_at: "".to_string(),
                updated_by: "".to_string(),
            }),
        };
        let resp = proto::reconcile_service_server::ReconcileService::apply_volume_desired_state(
            &server,
            Request::new(req),
        )
        .await;
        assert!(resp.is_ok());

        let req = proto::ApplyVolumeDesiredStateRequest {
            meta: Some(test_meta("1")),
            node_id: "node-1".to_string(),
            volume_id: "vol-cls-b".to_string(),
            fragment: Some(proto::DesiredStateFragment {
                id: "vol-cls-b".to_string(),
                kind: "volume".to_string(),
                generation: "1".to_string(),
                // No backend_class key: the B5 absent-field default.
                spec_json: r#"{"vm_id":"vm-1"}"#.as_bytes().to_vec(),
                policy_json: vec![],
                updated_at: "".to_string(),
                updated_by: "".to_string(),
            }),
        };
        let resp = proto::reconcile_service_server::ReconcileService::apply_volume_desired_state(
            &server,
            Request::new(req),
        )
        .await;
        assert!(resp.is_ok());

        assert_eq!(
            opens.opens.lock().unwrap().as_slice(),
            [
                (
                    "vol-cls-a".to_string(),
                    "lvm".to_string(),
                    "vol-cls-a.img".to_string(),
                ),
                (
                    "vol-cls-b".to_string(),
                    "local".to_string(),
                    "vol-cls-b".to_string(),
                ),
            ],
            "A4 must thread the spec_json backend_class, defaulting to local when absent"
        );
    }

    #[tokio::test]
    async fn attach_volume_rpc_parses_the_cp_spec_json_shape() {
        // #379 PR 2 contract pair (agent half; the CP half is pinned in
        // chv-controlplane-service — `volume_attach_spec_json` produces
        // exactly `{"backend_class":"lvm"}`, nothing else). This pins
        // that the AttachVolume RPC's consumer (A4's parser behind the
        // A8 dispatch) reads that shape to the same class — and that
        // populating ONLY the class does NOT change the locator
        // behavior: with no `locator` key the parser's default (the
        // bare volume id) applies, the same default the fragment path
        // has always exercised.
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("stord.sock");
        let opens = StordOpenLog::default();

        {
            let opens = opens.clone();
            let uds = tokio::net::UnixListener::bind(&socket).unwrap();
            tokio::spawn(async move {
                tonic::transport::Server::builder()
                    .add_service(
                        chv_stord_api::chv_stord_api::storage_service_server::StorageServiceServer::new(
                            MockStord { opens },
                        ),
                    )
                    .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds))
                    .await
                    .ok();
            });
        }

        let mut cache = NodeCache::new("node-1");
        cache.node_state = crate::state_machine::NodeState::TenantReady
            .as_str()
            .to_string();
        cache.connectivity_state = crate::connectivity::ConnectivityState::Connected;
        let server = AgentServer::new(
            Arc::new(tokio::sync::Mutex::new(cache)),
            VmRuntime::new(Arc::new(MockCloudHypervisorAdapter::default())),
            socket,
            std::path::PathBuf::from("/run/chv/nwd/api.sock"),
            None,
            dir.path().to_path_buf(),
        );

        let attach = |volume_id: &str, spec_json: &[u8]| proto::AttachVolumeRequest {
            meta: Some(test_meta("1")),
            node_id: "node-1".to_string(),
            volume: Some(proto::VolumeMutationSpec {
                volume_id: volume_id.to_string(),
                vm_id: "vm-1".to_string(),
                volume_spec_json: spec_json.to_vec(),
            }),
        };

        // The CP producer's exact bytes for a class-carrying volume.
        let resp = proto::lifecycle_service_server::LifecycleService::attach_volume(
            &server,
            Request::new(attach("vol-att-cls", br#"{"backend_class":"lvm"}"#)),
        )
        .await;
        assert!(resp.is_ok(), "attach must succeed: {:?}", resp.err());

        // A spec_json with no class key: the B5 default.
        let resp = proto::lifecycle_service_server::LifecycleService::attach_volume(
            &server,
            Request::new(attach("vol-att-bare", br#"{}"#)),
        )
        .await;
        assert!(resp.is_ok(), "attach must succeed: {:?}", resp.err());

        assert_eq!(
            opens.opens.lock().unwrap().as_slice(),
            [
                (
                    "vol-att-cls".to_string(),
                    "lvm".to_string(),
                    // #379 DP5: no `locator` key in the CP payload → the
                    // parser's class-dependent default: an LVM-class open
                    // carries the dm-path locator convention (VG from the
                    // node's stord backend info; default chv-vg here) so
                    // the standard device_allowlist admits it.
                    "/dev/mapper/chv-vg-vol-att-cls".to_string(),
                ),
                (
                    "vol-att-bare".to_string(),
                    "local".to_string(),
                    "vol-att-bare".to_string(),
                ),
            ],
            "A4 must parse the CP-produced spec_json to the same class, shaping the LVM locator default"
        );

        // The CP producer's exact bytes for a NULL-class volume are `{}`
        // (pinned CP-side: `volume_attach_spec_json(None) == b"{}"`) — a
        // parseable empty object whose absent keys take the A4 defaults
        // (local class, bare-id locator), so the attach RPC path now
        // WORKS for class-less volumes. HISTORY (the PR 2 pin this leg
        // replaces): the pre-#379 dispatch sent EMPTY bytes, which fail
        // `serde_json::from_slice` with an EOF error the handler maps to
        // invalid_argument — every NULL-class attach errored. PR 2
        // preserved that byte-exactly and left the fix to PR 3's
        // DP5/locator pass (design §5 DP5, "the carried {}-vs-[]
        // question"); this PR changes the producer to `{}` — a disclosed
        // behavior change (see the CHANGELOG) — so the same RPC now
        // opens with the defaults instead of failing. A local-class open
        // happens for the NULL volume (the third StordOpenLog entry).
        let resp = proto::lifecycle_service_server::LifecycleService::attach_volume(
            &server,
            Request::new(attach("vol-att-null", b"{}")),
        )
        .await;
        assert!(
            resp.is_ok(),
            "the CP producer's NULL-class bytes must parse and take the defaults: {:?}",
            resp.err()
        );
        assert_eq!(
            opens.opens.lock().unwrap().as_slice(),
            [
                (
                    "vol-att-cls".to_string(),
                    "lvm".to_string(),
                    "/dev/mapper/chv-vg-vol-att-cls".to_string(),
                ),
                (
                    "vol-att-bare".to_string(),
                    "local".to_string(),
                    "vol-att-bare".to_string(),
                ),
                (
                    "vol-att-null".to_string(),
                    "local".to_string(),
                    "vol-att-null".to_string(),
                ),
            ],
            "the NULL-class empty-object payload must open with the local defaults, not error"
        );

        // #533 (the #513 design's DP2 locator guard): the CP producer's
        // exact bytes for a STANDALONE volume (`volume_kind = 'data'`,
        // NULL class) are `{"locator":"{volume_id}.img"}` — the create
        // carrier's relative locator. The A4 parser must thread it to
        // the stord open so the attach lands on the file the #513
        // create carrier minted, NOT a create-on-open second file at
        // the bare-id default (`vol-att-std`) — the stray the #522
        // delete's DP4 destroy deliberately never chases. Red/green:
        // reverting the producer (or the parser's explicit-locator arm)
        // fails this leg with the bare `vol-att-std` locator.
        let resp = proto::lifecycle_service_server::LifecycleService::attach_volume(
            &server,
            Request::new(attach("vol-att-std", br#"{"locator":"vol-att-std.img"}"#)),
        )
        .await;
        assert!(
            resp.is_ok(),
            "the CP producer's standalone bytes must parse and open at the carrier locator: {:?}",
            resp.err()
        );
        assert_eq!(
            opens.opens.lock().unwrap().as_slice(),
            [
                (
                    "vol-att-cls".to_string(),
                    "lvm".to_string(),
                    "/dev/mapper/chv-vg-vol-att-cls".to_string(),
                ),
                (
                    "vol-att-bare".to_string(),
                    "local".to_string(),
                    "vol-att-bare".to_string(),
                ),
                (
                    "vol-att-null".to_string(),
                    "local".to_string(),
                    "vol-att-null".to_string(),
                ),
                (
                    "vol-att-std".to_string(),
                    "local".to_string(),
                    "vol-att-std.img".to_string(),
                ),
            ],
            "a standalone volume's attach must open at the create carrier's `{{volume_id}}.img` locator, never the bare-id default"
        );
    }

    /// Shared harness for the #513 create-volume handler tests: an
    /// `AgentServer` whose stord socket is the recording `MockStord`,
    /// with the cache in the TenantReady/Connected state the RPC paths
    /// expect. Returns `(server, opens, dir)` — the tempdir must outlive
    /// the test body (it owns the mock's UDS path).
    async fn create_volume_test_server() -> (AgentServer, StordOpenLog, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("stord.sock");
        let opens = StordOpenLog::default();

        {
            let opens = opens.clone();
            let uds = tokio::net::UnixListener::bind(&socket).unwrap();
            tokio::spawn(async move {
                tonic::transport::Server::builder()
                    .add_service(
                        chv_stord_api::chv_stord_api::storage_service_server::StorageServiceServer::new(
                            MockStord { opens },
                        ),
                    )
                    .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds))
                    .await
                    .ok();
            });
        }

        let mut cache = NodeCache::new("node-1");
        cache.node_state = crate::state_machine::NodeState::TenantReady
            .as_str()
            .to_string();
        cache.connectivity_state = crate::connectivity::ConnectivityState::Connected;
        let server = AgentServer::new(
            Arc::new(tokio::sync::Mutex::new(cache)),
            VmRuntime::new(Arc::new(MockCloudHypervisorAdapter::default())),
            socket,
            std::path::PathBuf::from("/run/chv/nwd/api.sock"),
            None,
            dir.path().to_path_buf(),
        );
        (server, opens, dir)
    }

    fn create_volume_request(volume_id: &str, spec_json: &[u8]) -> proto::CreateVolumeRequest {
        proto::CreateVolumeRequest {
            meta: Some(test_meta("1")),
            node_id: "node-1".to_string(),
            volume: Some(proto::VolumeMutationSpec {
                volume_id: volume_id.to_string(),
                vm_id: String::new(),
                volume_spec_json: spec_json.to_vec(),
            }),
        }
    }

    #[tokio::test]
    async fn create_volume_opens_with_size_class_and_dp5_locator() {
        // #513 PR 1 pin (DP2/DP5 — the load-bearing open-with-size): the
        // create handler opens the volume WITH the requested capacity in
        // the options map (NOT the attach path's option-less open — a
        // sizeless open cannot provision: the local backend's
        // create-on-open only fires on a sized open, and an absent LVM
        // LV without a size is refused), threads the spec_json's
        // `backend_class` (absent → local, the #510 B5 seam), and shapes
        // the DP5 locator per class: LVM carries the
        // /dev/mapper/{vg}-{vid} dm-path token; local keeps the legacy
        // CreateVm branch's `{volume_id}.img` default. The CP producer's
        // exact bytes (pinned CP-side by `volume_create_spec_json`) are
        // fed verbatim — the agent half of the contract pair.
        let (server, opens, _dir) = create_volume_test_server().await;

        // The CP producer's exact bytes for a class-carrying volume.
        let resp = proto::lifecycle_service_server::LifecycleService::create_volume(
            &server,
            Request::new(create_volume_request(
                "vol-cr-lvm",
                br#"{"backend_class":"lvm","size_bytes":1073741824}"#,
            )),
        )
        .await;
        assert!(resp.is_ok(), "create must succeed: {:?}", resp.err());

        // The CP producer's exact bytes for a NULL-class volume: the
        // size-only payload (no backend_class key, no materialized
        // "local" — the agent's B5 default resolves it).
        let resp = proto::lifecycle_service_server::LifecycleService::create_volume(
            &server,
            Request::new(create_volume_request(
                "vol-cr-bare",
                br#"{"size_bytes":536870912}"#,
            )),
        )
        .await;
        assert!(resp.is_ok(), "create must succeed: {:?}", resp.err());

        assert_eq!(
            opens.opens.lock().unwrap().as_slice(),
            [
                (
                    "vol-cr-lvm".to_string(),
                    "lvm".to_string(),
                    "/dev/mapper/chv-vg-vol-cr-lvm".to_string(),
                ),
                (
                    "vol-cr-bare".to_string(),
                    "local".to_string(),
                    "vol-cr-bare.img".to_string(),
                ),
            ],
            "the create open threads the class and shapes the DP5 locator"
        );
        let options = opens.open_options.lock().unwrap().clone();
        assert_eq!(
            options.len(),
            2,
            "every create open carries an options map: {options:?}"
        );
        assert_eq!(
            options[0].get("size_bytes").map(String::as_str),
            Some("1073741824"),
            "the LVM-class create open carries the requested capacity"
        );
        assert_eq!(
            options[1].get("size_bytes").map(String::as_str),
            Some("536870912"),
            "the NULL-class create open carries the requested capacity — the option the attach path's open never sends"
        );

        // The open's handle is cached (the A10 create_vm branch's
        // discipline) so the volume is observable in the node cache.
        let cache = server.cache.lock().await;
        assert!(cache.volume_handles.contains_key("vol-cr-lvm"));
        assert!(cache.volume_handles.contains_key("vol-cr-bare"));
    }

    #[tokio::test]
    async fn create_volume_requires_a_positive_size_bytes() {
        // #513 PR 1 pin (the DP2 contract's hard edge): a spec_json with
        // no size (or a non-positive one) is rejected with
        // invalid_argument BEFORE any stord open — a sizeless create
        // would mint a volume the attach path can never materialize
        // (journal-only is not viable, design §2.4). The attach RPC's
        // option-less open is exactly what this handler must not do.
        let (server, opens, _dir) = create_volume_test_server().await;

        for (label, spec_json) in [
            ("no size key", b"{}" as &[u8]),
            ("zero size", br#"{"size_bytes":0}"#),
        ] {
            let resp = proto::lifecycle_service_server::LifecycleService::create_volume(
                &server,
                Request::new(create_volume_request("vol-cr-nosize", spec_json)),
            )
            .await;
            let err = resp
                .err()
                .unwrap_or_else(|| panic!("{label} must be rejected"));
            assert_eq!(
                err.code(),
                tonic::Code::InvalidArgument,
                "{label} must be an invalid_argument: {err}"
            );
        }
        assert!(
            opens.opens.lock().unwrap().is_empty(),
            "no stord open may happen for a sizeless create"
        );
    }

    #[tokio::test]
    async fn create_volume_rejects_unsafe_volume_ids() {
        // The volume id becomes a path component of the stord locator
        // (`{volume_id}.img`); a traversal id is rejected at the node
        // boundary (the prepare_vm_resources discipline) before any
        // open.
        let (server, opens, _dir) = create_volume_test_server().await;

        let resp = proto::lifecycle_service_server::LifecycleService::create_volume(
            &server,
            Request::new(create_volume_request(
                "../../escape",
                br#"{"size_bytes":1024}"#,
            )),
        )
        .await;
        assert_eq!(
            resp.expect_err("traversal id must be rejected").code(),
            tonic::Code::InvalidArgument
        );
        assert!(
            opens.opens.lock().unwrap().is_empty(),
            "no stord open may happen for an unsafe volume id"
        );
    }

    fn delete_volume_request(volume_id: &str, backend_class: &str) -> proto::DeleteVolumeRequest {
        proto::DeleteVolumeRequest {
            meta: Some(test_meta("1")),
            node_id: "node-1".to_string(),
            volume_id: volume_id.to_string(),
            backend_class: backend_class.to_string(),
        }
    }

    #[tokio::test]
    async fn delete_volume_closes_destroys_and_evicts() {
        // #522 PR 1 pin (DP2/DP3/DP4): the delete handler's contract
        // sequence is close → destroy → evict. The cached handle is
        // closed FIRST (stord's destroy refuses while a session is
        // open — the agent is the cooperative caller); the destroy
        // carries the DP4 CARRIER locator byte-identical to the create
        // handler's shaping (the relative `{volume_id}.img` for local,
        // the `/dev/mapper/{vg}-{vid}` dm-path token for LVM — a
        // revert to the attach path's bare-id default must fail this
        // test, the #379 PR 1 red/green discipline); the cache entry
        // is evicted and the ack carries the observed generation.
        let (server, opens, _dir) = create_volume_test_server().await;

        // Seed cached handles exactly the create handler would have
        // (the A10 discipline), then delete both volumes.
        {
            let mut cache = server.cache.lock().await;
            cache
                .volume_handles
                .insert("vol-del-lvm".to_string(), "handle-lvm".to_string());
            cache
                .volume_handles
                .insert("vol-del-bare".to_string(), "handle-bare".to_string());
        }
        for (vid, class) in [("vol-del-lvm", "lvm"), ("vol-del-bare", "")] {
            let resp = proto::lifecycle_service_server::LifecycleService::delete_volume(
                &server,
                Request::new(delete_volume_request(vid, class)),
            )
            .await;
            assert!(
                resp.is_ok(),
                "delete of {vid} must succeed: {:?}",
                resp.err()
            );
        }

        // The interleaved order: every close precedes its destroy.
        assert_eq!(
            opens.events.lock().unwrap().as_slice(),
            [
                "close:vol-del-lvm".to_string(),
                "destroy:vol-del-lvm".to_string(),
                "close:vol-del-bare".to_string(),
                "destroy:vol-del-bare".to_string(),
            ],
            "the handler must close the cached handle before destroying"
        );
        // The closes thread the CACHED handles (not fabricated ones).
        assert_eq!(
            opens.closes.lock().unwrap().as_slice(),
            [
                ("vol-del-lvm".to_string(), "handle-lvm".to_string()),
                ("vol-del-bare".to_string(), "handle-bare".to_string()),
            ]
        );
        // The destroys carry the DP4 carrier locators: the LVM-class
        // delete the dm-path token (default VG), the NULL-class delete
        // the relative `{volume_id}.img` — never the attach path's
        // bare `{volume_id}` (#533).
        assert_eq!(
            opens.destroys.lock().unwrap().as_slice(),
            [
                (
                    "vol-del-lvm".to_string(),
                    "lvm".to_string(),
                    "/dev/mapper/chv-vg-vol-del-lvm".to_string(),
                ),
                (
                    "vol-del-bare".to_string(),
                    "local".to_string(),
                    "vol-del-bare.img".to_string(),
                ),
            ],
            "the destroy locator must be byte-identical to the create carrier's"
        );
        // The cache entries are evicted (a stale handle must never
        // outlive the backing store it names).
        let cache = server.cache.lock().await;
        assert!(!cache.volume_handles.contains_key("vol-del-lvm"));
        assert!(!cache.volume_handles.contains_key("vol-del-bare"));
    }

    /// #533 end-to-end (the #522 DP4 interaction): create → attach →
    /// detach → delete of ONE standalone volume, asserting every open
    /// and the destroy name the SAME carrier locator — the relative
    /// `{volume_id}.img`. The attach's spec_json is the CP producer's
    /// exact standalone bytes (`{"locator":"{volume_id}.img"}`), so the
    /// attach open lands on the file the create carrier minted; the
    /// #522 delete's destroy then reclaims exactly that file — no
    /// bare-id second file is ever minted at any step (the pre-#533
    /// attach opened at the bare `vol-e2e`, double-minting and leaving
    /// the stray the destroy deliberately never chases). Red/green:
    /// reverting the CP producer or the parser's explicit-locator arm
    /// fails the opens assertion with the bare-id locator.
    ///
    /// Scope note (review): this test drives the REAL agent handlers
    /// against the recording mock stord with a hard-coded CP-shaped
    /// attach spec — the CP producer that emits those bytes is not in
    /// this test's loop (its bytes are pinned separately at the
    /// producer/orchestrator/BFF tiers; the coverage composes). The
    /// assertion tier is the agent→stord RPC boundary, not the
    /// filesystem.
    #[tokio::test]
    async fn standalone_volume_attach_then_detach_then_delete_reclaims_one_file() {
        let (server, opens, _dir) = create_volume_test_server().await;

        // 1. The #513 create carrier mints the backing file at the
        //    relative `{volume_id}.img` (a sized open).
        let resp = proto::lifecycle_service_server::LifecycleService::create_volume(
            &server,
            Request::new(create_volume_request(
                "vol-e2e",
                br#"{"size_bytes":1073741824}"#,
            )),
        )
        .await;
        assert!(resp.is_ok(), "create must succeed: {:?}", resp.err());

        // 2. The attach (the CP dispatch's standalone spec bytes) must
        //    open the SAME file — the carrier locator, not the bare id.
        let resp = proto::lifecycle_service_server::LifecycleService::attach_volume(
            &server,
            Request::new(proto::AttachVolumeRequest {
                meta: Some(test_meta("1")),
                node_id: "node-1".to_string(),
                volume: Some(proto::VolumeMutationSpec {
                    volume_id: "vol-e2e".to_string(),
                    vm_id: "vm-e2e".to_string(),
                    volume_spec_json: br#"{"locator":"vol-e2e.img"}"#.to_vec(),
                }),
            }),
        )
        .await;
        assert!(resp.is_ok(), "attach must succeed: {:?}", resp.err());

        // 3. Detach (no re-open — the cached handle is closed).
        let resp = proto::lifecycle_service_server::LifecycleService::detach_volume(
            &server,
            Request::new(proto::DetachVolumeRequest {
                meta: Some(test_meta("1")),
                node_id: "node-1".to_string(),
                vm_id: "vm-e2e".to_string(),
                volume_id: "vol-e2e".to_string(),
                force: false,
            }),
        )
        .await;
        assert!(resp.is_ok(), "detach must succeed: {:?}", resp.err());

        // 4. The #522 delete destroys at the carrier locator.
        let resp = proto::lifecycle_service_server::LifecycleService::delete_volume(
            &server,
            Request::new(delete_volume_request("vol-e2e", "")),
        )
        .await;
        assert!(resp.is_ok(), "delete must succeed: {:?}", resp.err());

        // Every open (the create's sized open AND the attach's
        // option-less open) names the carrier locator — the attach
        // never mints the bare-id second file.
        assert_eq!(
            opens.opens.lock().unwrap().as_slice(),
            [
                (
                    "vol-e2e".to_string(),
                    "local".to_string(),
                    "vol-e2e.img".to_string(),
                ),
                (
                    "vol-e2e".to_string(),
                    "local".to_string(),
                    "vol-e2e.img".to_string(),
                ),
            ],
            "the create and the attach must open the SAME carrier-locator file — never the bare-id default"
        );
        // The destroy targets that same carrier locator: exactly one
        // file named, one file reclaimed.
        assert_eq!(
            opens.destroys.lock().unwrap().as_slice(),
            [(
                "vol-e2e".to_string(),
                "local".to_string(),
                "vol-e2e.img".to_string(),
            )],
            "the delete must destroy the carrier locator the opens used — the reclaim is clean"
        );
        // The full interleaving: create-open, attach-open, detach,
        // close, destroy.
        assert_eq!(
            opens.events.lock().unwrap().as_slice(),
            [
                "open:vol-e2e".to_string(),
                "open:vol-e2e".to_string(),
                "detach:vol-e2e".to_string(),
                "close:vol-e2e".to_string(),
                "destroy:vol-e2e".to_string(),
            ],
            "the attached-then-detached lifecycle must interleave open→open→detach→close→destroy"
        );
    }

    /// #540 end-to-end (the #539 pattern extended over the clone): a
    /// standalone volume is cloned, the CLONE is attached, detached,
    /// and deleted — and every name in the chain is the clone's
    /// carrier locator `{clone_id}.img`. The agent's clone handler
    /// threads `target_volume_id` as the clone name, so stord's
    /// `prepare_clone` materializes the clone's real data at
    /// `{clone_id}.img` (pinned at the backend tier by
    /// `local_backend_prepare_clone_raw_file`); the attach (the CP
    /// dispatch's standalone spec bytes, the #539 producer shape)
    /// opens that SAME file instead of create-on-opening a fresh
    /// empty one; the #522 delete's destroy then reclaims exactly
    /// that file — the clone's real data, the #539-disclosed gap
    /// closed. Red/green: reverting the local backend's clone dest to
    /// the pre-#540 `{source}-{clone}.img` fails the backend-tier pin
    /// (the attach open and the destroy still name `{clone_id}.img`,
    /// so the composition is load-bearing); reverting the agent's
    /// target-id threading fails the `clones` assertion.
    ///
    /// Scope note (the #539 test's discipline): this test drives the
    /// REAL agent handlers against the recording mock stord with a
    /// hard-coded CP-shaped attach spec — the CP producer's bytes are
    /// pinned at the producer/orchestrator/BFF tiers, and the
    /// filesystem-level "byte-identical to the source" claim is
    /// pinned at the backend tier; the coverage composes. Embedded
    /// (NULL-kind) clone targets are deliberately not driven here:
    /// their attach still does not navigate the carrier locator (the
    /// A1/A4 pre-existing breakage #540 discloses as residue).
    #[tokio::test]
    async fn standalone_clone_attach_then_detach_then_delete_reclaims_the_clones_real_file() {
        let (server, opens, _dir) = create_volume_test_server().await;

        // 1. The #513 create carrier mints the SOURCE's backing file
        //    at the relative `{volume_id}.img` (a sized open).
        let resp = proto::lifecycle_service_server::LifecycleService::create_volume(
            &server,
            Request::new(create_volume_request(
                "vol-cl-src",
                br#"{"size_bytes":1073741824}"#,
            )),
        )
        .await;
        assert!(resp.is_ok(), "create must succeed: {:?}", resp.err());

        // 2. The clone: the agent threads the TARGET id as the clone
        //    name — the id stord materializes `{clone_id}.img` for.
        let resp = proto::lifecycle_service_server::LifecycleService::clone_volume(
            &server,
            Request::new(proto::CloneVolumeRequest {
                meta: Some(test_meta("1")),
                node_id: "node-1".to_string(),
                source_volume_id: "vol-cl-src".to_string(),
                target_volume_id: "vol-cl-tgt".to_string(),
            }),
        )
        .await;
        assert!(resp.is_ok(), "clone must succeed: {:?}", resp.err());

        // 3. Attach the CLONE (the CP dispatch's standalone spec
        //    bytes): the open must land on the clone's carrier
        //    locator — the file prepare_clone materialized, never a
        //    fresh empty create-on-open at a foreign name.
        let resp = proto::lifecycle_service_server::LifecycleService::attach_volume(
            &server,
            Request::new(proto::AttachVolumeRequest {
                meta: Some(test_meta("1")),
                node_id: "node-1".to_string(),
                volume: Some(proto::VolumeMutationSpec {
                    volume_id: "vol-cl-tgt".to_string(),
                    vm_id: "vm-cl".to_string(),
                    volume_spec_json: br#"{"locator":"vol-cl-tgt.img"}"#.to_vec(),
                }),
            }),
        )
        .await;
        assert!(resp.is_ok(), "attach must succeed: {:?}", resp.err());

        // 4. Detach (no re-open — the cached handle is closed).
        let resp = proto::lifecycle_service_server::LifecycleService::detach_volume(
            &server,
            Request::new(proto::DetachVolumeRequest {
                meta: Some(test_meta("1")),
                node_id: "node-1".to_string(),
                vm_id: "vm-cl".to_string(),
                volume_id: "vol-cl-tgt".to_string(),
                force: false,
            }),
        )
        .await;
        assert!(resp.is_ok(), "detach must succeed: {:?}", resp.err());

        // 5. The #522 delete destroys the CLONE at its carrier
        //    locator — the clone's real data file.
        let resp = proto::lifecycle_service_server::LifecycleService::delete_volume(
            &server,
            Request::new(delete_volume_request("vol-cl-tgt", "")),
        )
        .await;
        assert!(resp.is_ok(), "delete must succeed: {:?}", resp.err());

        // The clone dispatch threaded the target id as the clone name
        // (the whole basis of the carrier-locator dest).
        assert_eq!(
            opens.clones.lock().unwrap().as_slice(),
            [("vol-cl-src".to_string(), "vol-cl-tgt".to_string())],
            "the clone must dispatch prepare_clone(source, target) — the target id is the name stord materializes"
        );
        // The clone target's attach open names its carrier locator —
        // the same file prepare_clone wrote, not a fresh empty file.
        assert_eq!(
            opens.opens.lock().unwrap().as_slice(),
            [
                (
                    "vol-cl-src".to_string(),
                    "local".to_string(),
                    "vol-cl-src.img".to_string(),
                ),
                (
                    "vol-cl-tgt".to_string(),
                    "local".to_string(),
                    "vol-cl-tgt.img".to_string(),
                ),
            ],
            "the clone's attach must open the CLONE's carrier-locator file"
        );
        // The destroy reclaims exactly that file: one destroy, the
        // clone's real data, never the source's.
        assert_eq!(
            opens.destroys.lock().unwrap().as_slice(),
            [(
                "vol-cl-tgt".to_string(),
                "local".to_string(),
                "vol-cl-tgt.img".to_string(),
            )],
            "the delete must destroy the clone's carrier locator — the clone's real data"
        );
        // The full interleaving: the clone materialization happens
        // between the source's create open and the clone's attach
        // open, and the source is never closed or destroyed by the
        // clone's lifecycle.
        assert_eq!(
            opens.events.lock().unwrap().as_slice(),
            [
                "open:vol-cl-src".to_string(),
                "prepare_clone:vol-cl-tgt".to_string(),
                "open:vol-cl-tgt".to_string(),
                "detach:vol-cl-tgt".to_string(),
                "close:vol-cl-tgt".to_string(),
                "destroy:vol-cl-tgt".to_string(),
            ],
            "the clone lifecycle must interleave open→prepare_clone→open→detach→close→destroy"
        );
    }

    #[tokio::test]
    async fn delete_volume_without_a_cached_handle_skips_the_close() {
        // A volume whose open's ack was lost (or whose create dispatch
        // never landed) has no cached handle: the close is skipped —
        // stord's own session table is the authority — and the destroy
        // still runs (idempotent by contract, so a redriven delete of
        // an already-gone volume re-acks).
        let (server, opens, _dir) = create_volume_test_server().await;

        let resp = proto::lifecycle_service_server::LifecycleService::delete_volume(
            &server,
            Request::new(delete_volume_request("vol-uncached", "")),
        )
        .await;
        assert!(
            resp.is_ok(),
            "an uncached delete must succeed: {:?}",
            resp.err()
        );
        assert_eq!(
            opens.events.lock().unwrap().as_slice(),
            ["destroy:vol-uncached".to_string()],
            "no close may happen without a cached handle"
        );
    }

    #[tokio::test]
    async fn delete_volume_rejects_unsafe_volume_ids() {
        // The volume id becomes a path component of the destroy
        // locator; a traversal id is a REMOVAL traversal — rejected at
        // the node boundary before any stord RPC.
        let (server, opens, _dir) = create_volume_test_server().await;

        let resp = proto::lifecycle_service_server::LifecycleService::delete_volume(
            &server,
            Request::new(delete_volume_request("../../escape", "")),
        )
        .await;
        assert_eq!(
            resp.expect_err("traversal id must be rejected").code(),
            tonic::Code::InvalidArgument
        );
        assert!(
            opens.events.lock().unwrap().is_empty(),
            "no stord RPC may happen for an unsafe volume id"
        );
    }

    #[tokio::test]
    async fn create_volume_parses_the_cp_spec_json_shape() {
        // #513 PR 1 contract pair (agent half; the CP half is
        // `volume_create_spec_json` in chv-controlplane-service — it
        // produces exactly `{"backend_class":"lvm","size_bytes":N}` /
        // `{"size_bytes":N}`, nothing else). Feeding those exact bytes
        // pins that the handler's parser reads the same keys the
        // producer writes: class AND size, with the absent class key
        // taking the local default.
        let (server, opens, _dir) = create_volume_test_server().await;

        let resp = proto::lifecycle_service_server::LifecycleService::create_volume(
            &server,
            Request::new(create_volume_request(
                "vol-cr-contract",
                br#"{"backend_class":"lvm","size_bytes":2048}"#,
            )),
        )
        .await;
        assert!(
            resp.is_ok(),
            "the CP producer's bytes must parse: {:?}",
            resp.err()
        );
        assert_eq!(
            opens.opens.lock().unwrap().as_slice(),
            [(
                "vol-cr-contract".to_string(),
                "lvm".to_string(),
                "/dev/mapper/chv-vg-vol-cr-contract".to_string(),
            )],
            "the CP-produced spec_json must drive the class and the DP5 LVM locator"
        );
        assert_eq!(
            opens.open_options.lock().unwrap()[0]
                .get("size_bytes")
                .map(String::as_str),
            Some("2048"),
            "the CP-produced spec_json must drive the sized open"
        );
    }

    #[tokio::test]
    async fn create_vm_legacy_branch_threads_disk_backend_class_to_stord() {
        // #379 PR 1 pin (A10): the legacy create_vm RPC branch opens every
        // disk with the class from the disk spec instead of the "local"
        // literal — vol-cls-1's spec value must arrive at stord verbatim,
        // and vol-cls-2 (no field) must get the historical "local"
        // default. Red/green: reverting the A10 site to the literal fails
        // the vol-cls-1 assertion.
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("stord.sock");
        let opens = StordOpenLog::default();

        {
            let opens = opens.clone();
            let uds = tokio::net::UnixListener::bind(&socket).unwrap();
            tokio::spawn(async move {
                tonic::transport::Server::builder()
                    .add_service(
                        chv_stord_api::chv_stord_api::storage_service_server::StorageServiceServer::new(
                            MockStord { opens },
                        ),
                    )
                    .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds))
                    .await
                    .ok();
            });
        }

        let mut cache = NodeCache::new("node-1");
        cache.node_state = crate::state_machine::NodeState::TenantReady
            .as_str()
            .to_string();
        cache.connectivity_state = crate::connectivity::ConnectivityState::Connected;
        let server = AgentServer::new(
            Arc::new(tokio::sync::Mutex::new(cache)),
            VmRuntime::new(Arc::new(MockCloudHypervisorAdapter::default())),
            socket,
            std::path::PathBuf::from("/run/chv/nwd/api.sock"),
            None,
            dir.path().to_path_buf(),
        );

        let spec_json = r#"{"name":"vm-cls","cpus":1,"memory_bytes":1024,"kernel_path":"/dev/null","disks":[{"volume_id":"vol-cls-1","backend_class":"lvm"},{"volume_id":"vol-cls-2"}],"nics":[]}"#;
        let req = proto::CreateVmRequest {
            meta: Some(test_meta("1")),
            node_id: "node-1".to_string(),
            vm: Some(proto::VmMutationSpec {
                vm_id: "vm-cls".to_string(),
                vm_spec_json: spec_json.as_bytes().to_vec(),
            }),
        };
        let resp = proto::lifecycle_service_server::LifecycleService::create_vm(
            &server,
            Request::new(req),
        )
        .await;
        assert!(resp.is_ok(), "create_vm must succeed: {:?}", resp.err());

        assert_eq!(
            opens.opens.lock().unwrap().as_slice(),
            [
                (
                    "vol-cls-1".to_string(),
                    "lvm".to_string(),
                    // #379 DP5: an LVM-class disk opens against the
                    // dm-path locator convention (VG from the node's
                    // stord backend info — the default chv-vg when no
                    // operator config names one).
                    "/dev/mapper/chv-vg-vol-cls-1".to_string(),
                ),
                (
                    "vol-cls-2".to_string(),
                    "local".to_string(),
                    "vol-cls-2.img".to_string(),
                ),
            ],
            "A10 must thread the disk spec's backend_class, defaulting to local when absent"
        );
    }

    #[tokio::test]
    async fn create_vm_legacy_branch_shapes_lvm_locator_from_stord_backend() {
        // #379 DP5 (A10 locator): the dm-path locator's VG comes from the
        // server's stord backend info — an operator-configured
        // lvm_volume_group shapes the locator, not the hardcoded default.
        // The locator is an allowlist token the LVM backend ignores, but
        // it must carry the node's real VG so the standard
        // device_allowlist posture stays meaningful (see the
        // lvm_locator pins in chv-hypervisor-api).
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("stord.sock");
        let opens = StordOpenLog::default();

        {
            let opens = opens.clone();
            let uds = tokio::net::UnixListener::bind(&socket).unwrap();
            tokio::spawn(async move {
                tonic::transport::Server::builder()
                    .add_service(
                        chv_stord_api::chv_stord_api::storage_service_server::StorageServiceServer::new(
                            MockStord { opens },
                        ),
                    )
                    .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds))
                    .await
                    .ok();
            });
        }

        let mut cache = NodeCache::new("node-1");
        cache.node_state = crate::state_machine::NodeState::TenantReady
            .as_str()
            .to_string();
        cache.connectivity_state = crate::connectivity::ConnectivityState::Connected;
        let server = AgentServer::new(
            Arc::new(tokio::sync::Mutex::new(cache)),
            VmRuntime::new(Arc::new(MockCloudHypervisorAdapter::default())),
            socket,
            std::path::PathBuf::from("/run/chv/nwd/api.sock"),
            None,
            dir.path().to_path_buf(),
        )
        .with_stord_backend(crate::stord_backend::StordBackendInfo {
            backend_class: "lvm".to_string(),
            lvm_volume_group: Some("qual-vg".to_string()),
        });

        let spec_json = r#"{"name":"vm-cls-vg","cpus":1,"memory_bytes":1024,"kernel_path":"/dev/null","disks":[{"volume_id":"vol-vg-1","backend_class":"lvm"}],"nics":[]}"#;
        let req = proto::CreateVmRequest {
            meta: Some(test_meta("1")),
            node_id: "node-1".to_string(),
            vm: Some(proto::VmMutationSpec {
                vm_id: "vm-cls-vg".to_string(),
                vm_spec_json: spec_json.as_bytes().to_vec(),
            }),
        };
        let resp = proto::lifecycle_service_server::LifecycleService::create_vm(
            &server,
            Request::new(req),
        )
        .await;
        assert!(resp.is_ok(), "create_vm must succeed: {:?}", resp.err());

        assert_eq!(
            opens.opens.lock().unwrap().as_slice(),
            [(
                "vol-vg-1".to_string(),
                "lvm".to_string(),
                "/dev/mapper/qual-vg-vol-vg-1".to_string(),
            )],
            "the LVM locator must carry the configured volume group"
        );
    }

    #[tokio::test]
    async fn apply_volume_desired_state_rejects_stale() {
        let server = test_server();
        let mut cache = server.cache.lock().await;
        cache.observe_generation("volume", "vol-1", "10");
        drop(cache);

        let req = proto::ApplyVolumeDesiredStateRequest {
            meta: Some(test_meta("9")),
            node_id: "node-1".to_string(),
            volume_id: "vol-1".to_string(),
            fragment: Some(proto::DesiredStateFragment {
                id: "vol-1".to_string(),
                kind: "volume".to_string(),
                generation: "9".to_string(),
                spec_json: vec![],
                policy_json: vec![],
                updated_at: "".to_string(),
                updated_by: "".to_string(),
            }),
        };
        let resp = proto::reconcile_service_server::ReconcileService::apply_volume_desired_state(
            &server,
            Request::new(req),
        )
        .await;
        assert_eq!(resp.unwrap_err().code(), tonic::Code::FailedPrecondition);
    }

    /// One dispatched `set_firewall_policy` RPC: `(network_id, policy_json)`.
    type FirewallCall = (String, Vec<u8>);

    /// Records what the legacy network-apply path actually dispatched to
    /// nwd: every ensured topology plus every `set_firewall_policy` RPC.
    /// The ensure log lets firewall-skip tests prove the request reached
    /// nwd and passed the topology step, so an empty `firewall_calls` can
    /// never pass vacuously.
    #[derive(Clone, Default)]
    struct NetworkPolicyTracker {
        ensured_networks: Arc<std::sync::Mutex<Vec<String>>>,
        firewall_calls: Arc<std::sync::Mutex<Vec<FirewallCall>>>,
    }

    struct MockNetworkPolicyNwd {
        tracker: NetworkPolicyTracker,
        /// #355 PR 1 review fold: when set, `set_firewall_policy`
        /// answers the nwd semantic-failure shape — a gRPC `Ok`
        /// wrapping an `err_result` — so tests can pin that the
        /// carrier surfaces the refusal instead of acking a false
        /// success. PR 3: ONE-SHOT (taken on first fire) so a test
        /// can pin recovery behavior — a refused apply followed by a
        /// successful one.
        firewall_failure: Arc<std::sync::Mutex<Option<(String, String)>>>,
    }

    #[tonic::async_trait]
    impl chv_nwd_api::chv_nwd_api::network_service_server::NetworkService for MockNetworkPolicyNwd {
        async fn list_namespace_state(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::ListNamespaceStateRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::ListNamespaceStateResponse>, Status>
        {
            Ok(Response::new(
                chv_nwd_api::chv_nwd_api::ListNamespaceStateResponse { items: vec![] },
            ))
        }

        async fn ensure_network_topology(
            &self,
            req: Request<chv_nwd_api::chv_nwd_api::EnsureNetworkTopologyRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::Result>, Status> {
            let inner = req.into_inner();
            self.tracker
                .ensured_networks
                .lock()
                .unwrap()
                .push(inner.topology.map(|t| t.network_id).unwrap_or_default());
            Ok(Response::new(chv_nwd_api::chv_nwd_api::Result {
                status: "ok".to_string(),
                error_code: "".to_string(),
                human_summary: "".to_string(),
            }))
        }

        async fn delete_network_topology(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::DeleteNetworkTopologyRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::Result>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn get_network_health(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::NetworkHealthRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::NetworkHealthResponse>, Status> {
            Ok(Response::new(
                chv_nwd_api::chv_nwd_api::NetworkHealthResponse {
                    result: Some(chv_nwd_api::chv_nwd_api::Result {
                        status: "ok".to_string(),
                        error_code: "".to_string(),
                        human_summary: "".to_string(),
                    }),
                    network_id: "".to_string(),
                    health_status: "healthy".to_string(),
                    last_error: "".to_string(),
                },
            ))
        }

        async fn attach_vm_nic(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::AttachVmNicRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::AttachVmNicResponse>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn detach_vm_nic(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::DetachVmNicRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::Result>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn set_firewall_policy(
            &self,
            req: Request<chv_nwd_api::chv_nwd_api::SetFirewallPolicyRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::Result>, Status> {
            let inner = req.into_inner();
            let policy_json = inner.policy.map(|p| p.policy_json).unwrap_or_default();
            self.tracker
                .firewall_calls
                .lock()
                .unwrap()
                .push((inner.network_id, policy_json));
            if let Some((error_code, human_summary)) = self.firewall_failure.lock().unwrap().take()
            {
                return Ok(Response::new(chv_nwd_api::chv_nwd_api::Result {
                    status: "error".to_string(),
                    error_code,
                    human_summary,
                }));
            }
            Ok(Response::new(chv_nwd_api::chv_nwd_api::Result {
                status: "ok".to_string(),
                error_code: "".to_string(),
                human_summary: "".to_string(),
            }))
        }

        async fn set_nat_policy(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::SetNatPolicyRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::Result>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn ensure_dhcp_scope(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::EnsureDhcpScopeRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::Result>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn ensure_dns_scope(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::EnsureDnsScopeRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::Result>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn expose_service(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::ExposeServiceRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::Result>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn withdraw_service_exposure(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::WithdrawServiceExposureRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::Result>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn update_overlay(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::UpdateOverlayRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::UpdateOverlayResponse>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn update_security_policy(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::SecurityPolicy>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::UpdateSecurityPolicyResponse>, Status>
        {
            Err(Status::unimplemented(""))
        }

        async fn update_rate_limit(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::RateLimitPolicy>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::UpdateRateLimitResponse>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn get_overlay_status(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::GetOverlayStatusRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::OverlayStatus>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn get_fabric_identity(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::GetFabricIdentityRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::FabricIdentityResponse>, Status> {
            Err(Status::unimplemented(""))
        }

        async fn send_gratuitous_arp(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::SendGratuitousArpRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::SendGratuitousArpResponse>, Status> {
            Err(Status::unimplemented(""))
        }
    }

    /// Drives `apply_network_desired_state` (the legacy-mode RPC; it fails
    /// closed in core-managed mode) against a recording mock nwd and returns
    /// what was dispatched, keyed by the spec's `firewall_rules` JSON.
    async fn apply_network_with_rules(firewall_rules: &str) -> (NetworkPolicyTracker, bool) {
        let dir = tempfile::tempdir().unwrap();
        let nwd_socket = dir.path().join("nwd.sock");
        let tracker = NetworkPolicyTracker::default();

        {
            let tracker = tracker.clone();
            let uds = tokio::net::UnixListener::bind(&nwd_socket).unwrap();
            tokio::spawn(async move {
                tonic::transport::Server::builder()
                    .add_service(
                        chv_nwd_api::chv_nwd_api::network_service_server::NetworkServiceServer::new(
                            MockNetworkPolicyNwd {
                                tracker,
                                firewall_failure: Arc::new(std::sync::Mutex::new(None)),
                            },
                        ),
                    )
                    .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds))
                    .await
                    .ok();
            });
        }

        // No startup race: the Unix listener is bound (listen() done)
        // before the server task is spawned, so connect() succeeds via
        // the kernel backlog even before the task accepts.

        let mut cache = NodeCache::new("node-1");
        cache.node_state = crate::state_machine::NodeState::TenantReady
            .as_str()
            .to_string();
        cache.connectivity_state = crate::connectivity::ConnectivityState::Connected;
        let server = AgentServer::new(
            Arc::new(tokio::sync::Mutex::new(cache)),
            VmRuntime::new(Arc::new(MockCloudHypervisorAdapter::default())),
            std::path::PathBuf::from("/run/chv/stord/api.sock"),
            nwd_socket,
            None,
            dir.path().to_path_buf(),
        );

        let req = proto::ApplyNetworkDesiredStateRequest {
            meta: Some(test_meta("1")),
            node_id: "node-1".to_string(),
            network_id: "net-1".to_string(),
            fragment: Some(proto::DesiredStateFragment {
                id: "net-1".to_string(),
                kind: "network".to_string(),
                generation: "1".to_string(),
                spec_json: format!(r#"{{"cidr":"10.0.0.0/24","firewall_rules":{firewall_rules}}}"#)
                    .into_bytes(),
                policy_json: vec![],
                updated_at: "".to_string(),
                updated_by: "".to_string(),
            }),
        };
        let resp = proto::reconcile_service_server::ReconcileService::apply_network_desired_state(
            &server,
            Request::new(req),
        )
        .await;
        (tracker, resp.is_ok())
    }

    #[tokio::test]
    async fn apply_network_desired_state_does_not_dispatch_empty_firewall_ruleset() {
        // #360 pin: a semantically empty firewall_rules in the legacy
        // apply_network_desired_state path must NOT reach nwd's
        // set_firewall_policy — nwd's engine engages default-deny even for
        // an empty ruleset, which would cut the network's guests off
        // entirely (including DHCP). The topology ensure is asserted
        // alongside so the skip can never pass vacuously (e.g. because the
        // RPC aborted before reaching nwd).
        let (tracker, acked) = apply_network_with_rules("[]").await;
        assert!(acked);
        // The RPC reached nwd and ensured net-1's topology...
        assert_eq!(
            tracker.ensured_networks.lock().unwrap().as_slice(),
            ["net-1"]
        );
        // ...but no firewall policy was dispatched for it.
        assert!(
            tracker.firewall_calls.lock().unwrap().is_empty(),
            "an empty ruleset must never be dispatched to nwd's set_firewall_policy"
        );
    }

    #[tokio::test]
    async fn apply_network_desired_state_still_dispatches_non_empty_firewall_ruleset() {
        // The #360 guard must not over-fire: a real ruleset keeps flowing
        // to nwd byte-identically (same network, same serialized policy).
        let rules = r#"[{"direction":"ingress","action":"accept","protocol":"icmp"}]"#;
        let (tracker, acked) = apply_network_with_rules(rules).await;
        assert!(acked);
        assert_eq!(
            tracker.ensured_networks.lock().unwrap().as_slice(),
            ["net-1"]
        );
        let firewall_calls = tracker.firewall_calls.lock().unwrap();
        assert_eq!(
            firewall_calls.len(),
            1,
            "a non-empty ruleset must still be dispatched exactly once"
        );
        assert_eq!(firewall_calls[0].0, "net-1");
        assert_eq!(
            firewall_calls[0].1,
            serde_json::to_vec(&serde_json::json!([{
                "direction": "ingress",
                "action": "accept",
                "protocol": "icmp"
            }]))
            .unwrap()
        );
    }

    /// Drives the #355 carrier (`apply_network_policy`) against a
    /// recording mock nwd and returns the tracker plus the raw tonic
    /// result, so tests can pin both the applied bytes and the
    /// boundary failures. `core_managed` constructs the server with a
    /// (disconnected) core authority — the M2.2b carve-out pin: unlike
    /// every sibling nwd-side-effect RPC, the policy carrier must NOT
    /// refuse in core-managed mode.
    async fn apply_network_policy_with_rules(
        firewall_rules: &[u8],
        core_managed: bool,
        nwd_rejects_with: Option<(String, String)>,
    ) -> (
        NetworkPolicyTracker,
        Result<Response<proto::AckResponse>, Status>,
    ) {
        let (_dir, tracker, server) = policy_carrier_harness(core_managed, nwd_rejects_with).await;
        let result = drive_apply_network_policy(&server, firewall_rules, "7").await;
        (tracker, result)
    }

    /// Builds the #355 PR 3 carrier harness: a recording mock nwd on a
    /// temp socket plus an AgentServer wired to it. The DP7 fence tests
    /// reuse the SAME server across multiple requests — the fence is
    /// server state. The returned `TempDir` owns the socket; bind it
    /// (`_dir`) for the test's duration.
    async fn policy_carrier_harness(
        core_managed: bool,
        nwd_rejects_with: Option<(String, String)>,
    ) -> (tempfile::TempDir, NetworkPolicyTracker, AgentServer) {
        let dir = tempfile::tempdir().unwrap();
        let nwd_socket = dir.path().join("nwd.sock");
        let tracker = NetworkPolicyTracker::default();

        {
            let tracker = tracker.clone();
            let uds = tokio::net::UnixListener::bind(&nwd_socket).unwrap();
            tokio::spawn(async move {
                tonic::transport::Server::builder()
                    .add_service(
                        chv_nwd_api::chv_nwd_api::network_service_server::NetworkServiceServer::new(
                            MockNetworkPolicyNwd {
                                tracker,
                                firewall_failure: Arc::new(std::sync::Mutex::new(nwd_rejects_with)),
                            },
                        ),
                    )
                    .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds))
                    .await
                    .ok();
            });
        }

        let mut cache = NodeCache::new("node-1");
        cache.node_state = crate::state_machine::NodeState::TenantReady
            .as_str()
            .to_string();
        cache.connectivity_state = crate::connectivity::ConnectivityState::Connected;
        let mut server = AgentServer::new(
            Arc::new(tokio::sync::Mutex::new(cache)),
            VmRuntime::new(Arc::new(MockCloudHypervisorAdapter::default())),
            std::path::PathBuf::from("/run/chv/stord/api.sock"),
            nwd_socket,
            None,
            dir.path().to_path_buf(),
        );
        if core_managed {
            server.core_authority = Some(cellhv_core_operations::AuthorityHandle::disconnected());
        }
        (dir, tracker, server)
    }

    /// One `apply_network_policy` request against `server`, with a
    /// controllable DP7 generation (`desired_state_version`).
    async fn drive_apply_network_policy(
        server: &AgentServer,
        firewall_rules: &[u8],
        generation: &str,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let req = proto::ApplyNetworkPolicyRequest {
            meta: Some(test_meta(generation)),
            node_id: "node-1".to_string(),
            network_id: "net-1".to_string(),
            policy_json: firewall_rules.to_vec(),
        };
        proto::lifecycle_service_server::LifecycleService::apply_network_policy(
            server,
            Request::new(req),
        )
        .await
    }

    #[tokio::test]
    async fn apply_network_policy_applies_non_empty_ruleset() {
        // #355 PR 1 pin: the carrier threads the stored ruleset to
        // nwd's set_firewall_policy byte-identically, on the network
        // the request names.
        let rules = br#"[{"direction":"ingress","action":"accept","protocol":"icmp"}]"#;
        let (tracker, result) = apply_network_policy_with_rules(rules, false, None).await;
        let resp = result.expect("a valid ruleset must ack");
        let result_meta = resp.into_inner().result.expect("ack carries result meta");
        assert_eq!(result_meta.status, "ok");
        assert_eq!(result_meta.human_summary, "network policy applied");
        let firewall_calls = tracker.firewall_calls.lock().unwrap();
        assert_eq!(firewall_calls.len(), 1);
        assert_eq!(firewall_calls[0].0, "net-1");
        assert_eq!(firewall_calls[0].1, rules.to_vec());
    }

    #[tokio::test]
    async fn apply_network_policy_applies_the_dp4_baseline_for_empty_rulesets() {
        // DP4 (ruled 2026-10-08, #355 PR 3): `[]` is no longer a skip —
        // it means "no user rules" and the shared BASELINE (DHCP
        // UDP 67/68, DNS UDP/TCP 53) applies on top of default-deny,
        // replacing the #360 never-apply cutoff. The applied bytes are
        // the shared module's baseline verbatim, so every producer
        // (orchestrator arm, direct RPC caller) feeds nwd the same
        // policy.
        let (tracker, result) = apply_network_policy_with_rules(b"[]", false, None).await;
        let resp = result.expect("an empty ruleset must still ack ok (baseline, not a failure)");
        let result_meta = resp.into_inner().result.expect("ack carries result meta");
        assert_eq!(result_meta.status, "ok");
        assert_eq!(result_meta.human_summary, "network policy applied");
        let baseline = chv_common::firewall::baseline_policy_json();
        let firewall_calls = tracker.firewall_calls.lock().unwrap();
        assert_eq!(
            firewall_calls.len(),
            1,
            "an empty ruleset dispatches the baseline to nwd"
        );
        assert_eq!(firewall_calls[0].0, "net-1");
        assert_eq!(
            firewall_calls[0].1,
            baseline.into_bytes(),
            "the applied bytes are the shared DP4 baseline verbatim"
        );
    }

    #[tokio::test]
    async fn apply_network_policy_noops_stale_generations() {
        // DP7 (#355 PR 3): a dispatch whose generation is BELOW the
        // last applied generation for the network is an idempotent
        // no-op — an out-of-order fan-out result or an older
        // operation's retry must never regress a newer policy. The
        // generation is compared NUMERICALLY (never string-wise).
        let (_dir, tracker, server) = policy_carrier_harness(false, None).await;
        let rules = br#"[{"direction":"inbound","action":"accept","protocol":"icmp"}]"#;

        // Generation 10 applies (fresh fence).
        let resp = drive_apply_network_policy(&server, rules, "10")
            .await
            .expect("generation 10 must apply");
        assert_eq!(
            resp.into_inner().result.unwrap().human_summary,
            "network policy applied"
        );

        // Generation 9 — numerically lower, LEXICOGRAPHICALLY higher
        // ("9" > "10" as a string) — must no-op without touching nwd.
        let resp = drive_apply_network_policy(&server, rules, "9")
            .await
            .expect("a stale dispatch acks ok (idempotent no-op, not a failure)");
        let meta = resp.into_inner().result.unwrap();
        assert_eq!(meta.status, "ok");
        assert!(
            meta.human_summary.contains("stale"),
            "the ack names the fence: {}",
            meta.human_summary
        );

        // Generation 10 again (equal) re-applies idempotently (nwd's
        // apply is idempotent; the fence only blocks REGRESSION).
        let resp = drive_apply_network_policy(&server, rules, "10")
            .await
            .expect("an equal generation re-applies");
        assert_eq!(
            resp.into_inner().result.unwrap().human_summary,
            "network policy applied"
        );

        // Generation 11 applies (advance).
        let resp = drive_apply_network_policy(&server, rules, "11")
            .await
            .expect("a newer generation applies");
        assert_eq!(
            resp.into_inner().result.unwrap().human_summary,
            "network policy applied"
        );

        let firewall_calls = tracker.firewall_calls.lock().unwrap();
        assert_eq!(
            firewall_calls.len(),
            3,
            "exactly the non-stale dispatches reached nwd (10, 10, 11): {firewall_calls:?}"
        );
    }

    #[tokio::test]
    async fn apply_network_policy_concurrent_dispatches_cannot_regress_the_fence() {
        // The round-1 fold's raison d'être, pinned: two concurrent
        // dispatches to the SAME network both read the old fence mark
        // (the guard lock is dropped across the awaited nwd apply), so
        // a plain insert would let whichever apply finishes LAST —
        // possibly the older generation — rewrite the high-water mark.
        // The advance is monotonic (strictly-bigger only): whichever
        // order the applies complete in, the fence ends at the max.
        let (_dir, tracker, server) = policy_carrier_harness(false, None).await;
        let rules = br#"[{"direction":"inbound","action":"accept","protocol":"icmp"}]"#;

        let (newer, older) = tokio::join!(
            drive_apply_network_policy(&server, rules, "8"),
            drive_apply_network_policy(&server, rules, "7"),
        );
        newer.expect("the newer generation applies");
        older.expect("the concurrent older dispatch acks (applies or no-ops, never fails)");

        // Whichever landed, a subsequent stale dispatch must no-op:
        // the fence must read 8, never 7.
        let resp = drive_apply_network_policy(&server, rules, "7")
            .await
            .expect("the stale re-dispatch acks ok");
        let meta = resp.into_inner().result.unwrap();
        assert_eq!(meta.status, "ok");
        assert!(
            meta.human_summary.contains("stale"),
            "generation 7 must be fenced out (the high-water mark is 8, not 7): {}",
            meta.human_summary
        );
        assert!(
            tracker.firewall_calls.lock().unwrap().len() <= 3,
            "sanity: at most the two concurrent applies plus the fenced no-op's zero"
        );
    }

    #[tokio::test]
    async fn apply_network_policy_failed_apply_does_not_advance_the_fence() {
        // DP7's failure half: the fence advances ONLY on a successful
        // nwd apply. A refused generation 10 must not block a
        // subsequent generation 9 (which still matches the last
        // applied state) — the refusal surfaces, the fence stays.
        let (_dir, tracker, server) = policy_carrier_harness(
            false,
            Some((
                "policy_engine_failure".to_string(),
                "engine refused".to_string(),
            )),
        )
        .await;
        let rules = br#"[{"direction":"inbound","action":"accept","protocol":"icmp"}]"#;

        let err = drive_apply_network_policy(&server, rules, "10")
            .await
            .expect_err("the nwd refusal must surface (never a false ack)");
        assert_eq!(err.code(), tonic::Code::Internal);

        // Generation 9 still applies: the failed 10 never became the
        // fence.
        let resp = drive_apply_network_policy(&server, rules, "9")
            .await
            .expect("generation 9 applies after 10's refusal");
        assert_eq!(
            resp.into_inner().result.unwrap().human_summary,
            "network policy applied"
        );
        // Both calls reached nwd (the mock records the refused one
        // too); the pin is the FENCE behavior, not the call count.
        assert_eq!(
            tracker.firewall_calls.lock().unwrap().len(),
            2,
            "the refused generation-10 apply and the successful generation-9 apply"
        );
    }

    #[tokio::test]
    async fn apply_network_policy_rejects_non_numeric_generation() {
        // DP7 fail-closed: `desired_state_version` must parse as a
        // numeric generation — the carrier sends the NDS generation.
        // A non-numeric version is a buggy peer and gets an
        // invalid_argument BEFORE any nwd call (never a string
        // comparison, never a silent apply).
        let (_dir, tracker, server) = policy_carrier_harness(false, None).await;
        let err = drive_apply_network_policy(&server, b"[]", "policy-abc")
            .await
            .expect_err("a non-numeric generation must fail closed");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(
            tracker.firewall_calls.lock().unwrap().is_empty(),
            "a rejected generation must never reach nwd"
        );
    }

    #[tokio::test]
    async fn apply_network_policy_rejects_unsafe_network_ids() {
        // The network id becomes the nft table name: a non-safe id must
        // fail closed at the node boundary before any nwd call.
        let dir = tempfile::tempdir().unwrap();
        let mut cache = NodeCache::new("node-1");
        cache.node_state = crate::state_machine::NodeState::TenantReady
            .as_str()
            .to_string();
        let server = AgentServer::new(
            Arc::new(tokio::sync::Mutex::new(cache)),
            VmRuntime::new(Arc::new(MockCloudHypervisorAdapter::default())),
            std::path::PathBuf::from("/run/chv/stord/api.sock"),
            std::path::PathBuf::from("/nonexistent/nwd.sock"),
            None,
            dir.path().to_path_buf(),
        );
        let req = proto::ApplyNetworkPolicyRequest {
            meta: Some(test_meta("7")),
            node_id: "node-1".to_string(),
            network_id: "../../escape".to_string(),
            policy_json: b"[]".to_vec(),
        };
        let err = proto::lifecycle_service_server::LifecycleService::apply_network_policy(
            &server,
            Request::new(req),
        )
        .await
        .expect_err("an unsafe network id must be rejected");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn apply_network_policy_rejects_malformed_policy_json() {
        // Fail closed BEFORE nwd (the M4.4 N7 class): malformed bytes
        // get an invalid_argument, never a silent drop or a pass-through.
        let (tracker, result) =
            apply_network_policy_with_rules(b"not json at all", false, None).await;
        let err = result.expect_err("malformed policy_json must fail closed");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(
            tracker.firewall_calls.lock().unwrap().is_empty(),
            "malformed policy bytes must never reach nwd"
        );
    }

    #[tokio::test]
    async fn apply_network_policy_accepted_in_core_managed_mode() {
        // THE #355 M2.2b carve-out pin: with a core authority attached,
        // the policy carrier still applies — unlike every sibling
        // nwd-side-effect RPC (apply_network_desired_state,
        // create/delete_volume), which refuse with `unimplemented`.
        // Policy application already runs behind Core at VM attach; if
        // this test ever fails with `unsupported in core-managed mode`,
        // the deployed-mode dispatch leg is dead.
        let rules = br#"[{"direction":"ingress","action":"accept","protocol":"icmp"}]"#;
        let (tracker, result) = apply_network_policy_with_rules(rules, true, None).await;
        let resp = result.expect(
            "apply_network_policy must NOT refuse in core-managed mode (the #355 M2.2b carve-out)",
        );
        let result_meta = resp.into_inner().result.expect("ack carries result meta");
        assert_eq!(result_meta.status, "ok");
        let firewall_calls = tracker.firewall_calls.lock().unwrap();
        assert_eq!(firewall_calls.len(), 1);
        assert_eq!(firewall_calls[0].1, rules.to_vec());
    }

    #[tokio::test]
    async fn apply_network_policy_surfaces_nwd_rejection() {
        // #355 PR 1 review fold (the MUST-FIX): nwd reports semantic
        // refusals as a gRPC Ok wrapping an err_result — the
        // unreachable-topology `NotFound` shape here, exactly what a
        // stale target-set resolution (a VM placed but never
        // materialized, a topology torn down at last-detach) produces.
        // The carrier must FAIL the operation with the refusal
        // surfaced, never ack a false success — the #355 lie in task
        // form. The tracker proves the call reached nwd, so this
        // cannot pass vacuously.
        let rules = br#"[{"direction":"ingress","action":"accept","protocol":"icmp"}]"#;
        let (tracker, result) = apply_network_policy_with_rules(
            rules,
            false,
            Some((
                "NotFound".to_string(),
                "no topology for network net-1".to_string(),
            )),
        )
        .await;
        let err = result.expect_err("an nwd err_result must fail the carrier, not ack ok");
        assert_eq!(err.code(), tonic::Code::Internal);
        assert!(
            err.message().contains("NotFound"),
            "the nwd error code must ride the failure: {}",
            err.message()
        );
        assert!(
            err.message().contains("no topology for network net-1"),
            "the nwd human summary must ride the failure: {}",
            err.message()
        );
        // The apply reached nwd (the refusal is nwd's, not a
        // pre-flight abort)...
        let firewall_calls = tracker.firewall_calls.lock().unwrap();
        assert_eq!(firewall_calls.len(), 1);
        assert_eq!(firewall_calls[0].1, rules.to_vec());
    }

    #[tokio::test]
    async fn pause_and_resume_node_scheduling() {
        let server = test_server();
        let pause_req = proto::PauseNodeSchedulingRequest {
            meta: Some(test_meta("1")),
            node_id: "node-1".to_string(),
        };
        let resp = proto::lifecycle_service_server::LifecycleService::pause_node_scheduling(
            &server,
            Request::new(pause_req),
        )
        .await;
        assert!(resp.is_ok());
        assert_eq!(server.cache.lock().await.node_state, "Degraded");

        let resume_req = proto::ResumeNodeSchedulingRequest {
            meta: Some(test_meta("1")),
            node_id: "node-1".to_string(),
        };
        let resp = proto::lifecycle_service_server::LifecycleService::resume_node_scheduling(
            &server,
            Request::new(resume_req),
        )
        .await;
        assert!(resp.is_ok());
        assert_eq!(server.cache.lock().await.node_state, "TenantReady");
    }

    #[tokio::test]
    async fn drain_node_transitions_state() {
        let server = test_server();
        let req = proto::DrainNodeRequest {
            meta: Some(test_meta("1")),
            node_id: "node-1".to_string(),
            allow_workload_stop: false,
        };
        let resp = proto::lifecycle_service_server::LifecycleService::drain_node(
            &server,
            Request::new(req),
        )
        .await;
        assert!(resp.is_ok());
        assert_eq!(server.cache.lock().await.node_state, "Draining");
    }

    #[tokio::test]
    async fn enter_and_exit_maintenance() {
        let server = test_server();
        let enter_req = proto::EnterMaintenanceRequest {
            meta: Some(test_meta("1")),
            node_id: "node-1".to_string(),
            reason: "".to_string(),
        };
        let resp = proto::lifecycle_service_server::LifecycleService::enter_maintenance(
            &server,
            Request::new(enter_req),
        )
        .await;
        assert!(resp.is_ok());
        assert_eq!(server.cache.lock().await.node_state, "Maintenance");

        let exit_req = proto::ExitMaintenanceRequest {
            meta: Some(test_meta("1")),
            node_id: "node-1".to_string(),
        };
        let resp = proto::lifecycle_service_server::LifecycleService::exit_maintenance(
            &server,
            Request::new(exit_req),
        )
        .await;
        assert!(resp.is_ok());
        assert_eq!(server.cache.lock().await.node_state, "Bootstrapping");
    }

    /// Core-managed single-writer enforcement: every legacy VM/storage/network
    /// effector and desired-state write fails closed with `Unimplemented` when a
    /// Core authority is attached. The gate runs before the request body is
    /// inspected, so a degenerate request is sufficient to prove it.
    #[tokio::test]
    async fn core_managed_legacy_effectors_fail_closed() {
        let mut server = test_server();
        server.core_authority = Some(cellhv_core_operations::AuthorityHandle::disconnected());

        // ReconcileService desired-state / network-lifecycle surface.
        // (apply_vm_desired_state is no longer a blanket refusal: since the
        // M2.5 dispatch shim it Core-routes generation-1 creates — its
        // fail-closed behavior is pinned by the dedicated shim tests.)
        let node = proto::reconcile_service_server::ReconcileService::apply_node_desired_state(
            &server,
            Request::new(proto::ApplyNodeDesiredStateRequest::default()),
        )
        .await;
        let net = proto::lifecycle_service_server::LifecycleService::start_network(
            &server,
            Request::new(proto::StartNetworkRequest::default()),
        )
        .await;
        for (name, result) in [("apply_node_desired_state", node), ("start_network", net)] {
            assert_eq!(
                result.unwrap_err().code(),
                tonic::Code::Unimplemented,
                "{name} must fail closed in core-managed mode"
            );
        }

        // Storage-plane effectors.
        let resize = proto::lifecycle_service_server::LifecycleService::resize_volume(
            &server,
            Request::new(proto::ResizeVolumeRequest::default()),
        )
        .await;
        let snap_vol = proto::lifecycle_service_server::LifecycleService::snapshot_volume(
            &server,
            Request::new(proto::SnapshotVolumeRequest::default()),
        )
        .await;
        // #513 DP7: the standalone volume-create carrier joins the
        // storage-plane fail-closed set (provisioning is a legacy-path
        // stord side effect behind Core's back).
        let create_vol = proto::lifecycle_service_server::LifecycleService::create_volume(
            &server,
            Request::new(proto::CreateVolumeRequest::default()),
        )
        .await;
        // #522 DP10: the volume-delete carrier joins the same set —
        // the destroy is a legacy-path stord side effect, and on a
        // core-managed node the single writer is Core (the refusal
        // takes the operation terminal via the orchestrator's
        // Unimplemented fast-fail, no retry).
        let delete_vol = proto::lifecycle_service_server::LifecycleService::delete_volume(
            &server,
            Request::new(proto::DeleteVolumeRequest::default()),
        )
        .await;
        for (name, result) in [
            ("resize_volume", resize),
            ("snapshot_volume", snap_vol),
            ("create_volume", create_vol),
            ("delete_volume", delete_vol),
        ] {
            assert_eq!(
                result.unwrap_err().code(),
                tonic::Code::Unimplemented,
                "{name} must fail closed in core-managed mode"
            );
        }

        // VM/hypervisor effectors.
        let pause = proto::lifecycle_service_server::LifecycleService::pause_vm(
            &server,
            Request::new(proto::PauseVmRequest::default()),
        )
        .await;
        let add_disk = proto::lifecycle_service_server::LifecycleService::add_disk(
            &server,
            Request::new(proto::AddDiskRequest::default()),
        )
        .await;
        let migrate = proto::lifecycle_service_server::LifecycleService::migrate_vm(
            &server,
            Request::new(proto::MigrateVmRequest::default()),
        )
        .await;
        let coredump = proto::lifecycle_service_server::LifecycleService::coredump_vm(
            &server,
            Request::new(proto::CoredumpVmRequest::default()),
        )
        .await;
        for (name, result) in [
            ("pause_vm", pause),
            ("add_disk", add_disk),
            ("migrate_vm", migrate),
            ("coredump_vm", coredump),
        ] {
            assert_eq!(
                result.unwrap_err().code(),
                tonic::Code::Unimplemented,
                "{name} must fail closed in core-managed mode"
            );
        }

        // NWD data-plane effectors.
        let overlay = proto::lifecycle_service_server::LifecycleService::update_overlay(
            &server,
            Request::new(proto::UpdateOverlayRequest::default()),
        )
        .await;
        let arp = proto::lifecycle_service_server::LifecycleService::send_gratuitous_arp(
            &server,
            Request::new(proto::SendGratuitousArpRequest::default()),
        )
        .await;
        for (name, result) in [("update_overlay", overlay), ("send_gratuitous_arp", arp)] {
            assert_eq!(
                result.unwrap_err().code(),
                tonic::Code::Unimplemented,
                "{name} must fail closed in core-managed mode"
            );
        }
    }

    /// The CP→nwd fabric plan mapping is the update_overlay relay's core:
    /// every field of a two-peer sample plan must survive the translation
    /// from `chv.controlplane.node.v1` to `chv.node.nwd.v1` (ADR-021 plan
    /// carriage).
    #[test]
    fn fabric_plan_mapping_preserves_all_fields() {
        let plan = proto::FabricPlan {
            fabric_domain_id: "fabric-domain-1".to_string(),
            local_host_id: "node-1".to_string(),
            local_fabric_ip: "100.100.0.1".to_string(),
            tenant_mtu: 1380,
            fabric_mtu: 1440,
            binding_generation: 3,
            plan_generation: 7,
            peers: vec![
                proto::FabricPeer {
                    node_id: "node-2".to_string(),
                    public_key: "wg-pub-node-2".to_string(),
                    underlay_endpoint: "10.0.0.2:65001".to_string(),
                    fabric_ip: "100.100.0.2".to_string(),
                },
                proto::FabricPeer {
                    node_id: "node-3".to_string(),
                    public_key: "wg-pub-node-3".to_string(),
                    underlay_endpoint: "10.0.0.3:65001".to_string(),
                    fabric_ip: "100.100.0.3".to_string(),
                },
            ],
        };
        let mapped = fabric_plan_to_nwd(&plan);
        assert_eq!(mapped.fabric_domain_id, "fabric-domain-1");
        assert_eq!(mapped.local_host_id, "node-1");
        assert_eq!(mapped.local_fabric_ip, "100.100.0.1");
        assert_eq!(mapped.tenant_mtu, 1380);
        assert_eq!(mapped.fabric_mtu, 1440);
        assert_eq!(mapped.binding_generation, 3);
        assert_eq!(mapped.plan_generation, 7);
        assert_eq!(mapped.peers.len(), 2);
        assert_eq!(mapped.peers[0].node_id, "node-2");
        assert_eq!(mapped.peers[0].public_key, "wg-pub-node-2");
        assert_eq!(mapped.peers[0].underlay_endpoint, "10.0.0.2:65001");
        assert_eq!(mapped.peers[0].fabric_ip, "100.100.0.2");
        assert_eq!(mapped.peers[1].node_id, "node-3");
        assert_eq!(mapped.peers[1].public_key, "wg-pub-node-3");
        assert_eq!(mapped.peers[1].underlay_endpoint, "10.0.0.3:65001");
        assert_eq!(mapped.peers[1].fabric_ip, "100.100.0.3");
    }

    /// Positive-path update_overlay relay (legacy mode): the CP fabric plan
    /// must reach nwd field-by-field, including the full peer set, and the
    /// agent must acknowledge the operation.
    #[tokio::test]
    async fn update_overlay_relays_fabric_plan_to_nwd() {
        use crate::daemon_clients::fabric_test_support::{FabricNwdCalls, MockFabricNwd};

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("nwd.sock");

        let calls = std::sync::Arc::new(FabricNwdCalls::default());
        {
            let uds = tokio::net::UnixListener::bind(&socket).unwrap();
            let service = MockFabricNwd {
                calls: calls.clone(),
                public_key: String::new(),
                underlay_mtu: 0,
                identity_error: false,
            };
            tokio::spawn(async move {
                tonic::transport::Server::builder()
                    .add_service(
                        chv_nwd_api::chv_nwd_api::network_service_server::NetworkServiceServer::new(
                            service,
                        ),
                    )
                    .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds))
                    .await
                    .ok();
            });
        }

        let mut cache = NodeCache::new("node-1");
        cache.node_state = crate::state_machine::NodeState::TenantReady
            .as_str()
            .to_string();
        cache.connectivity_state = crate::connectivity::ConnectivityState::Connected;
        let server = AgentServer::new(
            Arc::new(tokio::sync::Mutex::new(cache)),
            VmRuntime::new(Arc::new(MockCloudHypervisorAdapter::default())),
            std::path::PathBuf::from("/run/chv/stord/api.sock"),
            socket,
            None,
            dir.path().to_path_buf(),
        );

        let req = proto::UpdateOverlayRequest {
            meta: Some(test_meta("1")),
            node_id: "node-1".to_string(),
            network_id: "net-1".to_string(),
            vni: 42,
            vtep_endpoints: vec![],
            fdb_entries: vec![],
            fabric: Some(proto::FabricPlan {
                fabric_domain_id: "fabric-domain-1".to_string(),
                local_host_id: "node-1".to_string(),
                local_fabric_ip: "100.100.0.1".to_string(),
                tenant_mtu: 1380,
                fabric_mtu: 1440,
                binding_generation: 3,
                plan_generation: 7,
                peers: vec![
                    proto::FabricPeer {
                        node_id: "node-2".to_string(),
                        public_key: "wg-pub-node-2".to_string(),
                        underlay_endpoint: "10.0.0.2:65001".to_string(),
                        fabric_ip: "100.100.0.2".to_string(),
                    },
                    proto::FabricPeer {
                        node_id: "node-3".to_string(),
                        public_key: "wg-pub-node-3".to_string(),
                        underlay_endpoint: "10.0.0.3:65001".to_string(),
                        fabric_ip: "100.100.0.3".to_string(),
                    },
                ],
            }),
        };
        let resp = proto::lifecycle_service_server::LifecycleService::update_overlay(
            &server,
            Request::new(req),
        )
        .await;
        assert!(resp.is_ok());

        let recorded = calls.overlay_fabrics.lock().unwrap();
        assert_eq!(recorded.len(), 1, "nwd must receive exactly one plan");
        let plan = &recorded[0];
        assert_eq!(plan.fabric_domain_id, "fabric-domain-1");
        assert_eq!(plan.local_host_id, "node-1");
        assert_eq!(plan.local_fabric_ip, "100.100.0.1");
        assert_eq!(plan.tenant_mtu, 1380);
        assert_eq!(plan.fabric_mtu, 1440);
        assert_eq!(plan.binding_generation, 3);
        assert_eq!(plan.plan_generation, 7);
        assert_eq!(plan.peers.len(), 2);
        assert_eq!(plan.peers[0].node_id, "node-2");
        assert_eq!(plan.peers[0].public_key, "wg-pub-node-2");
        assert_eq!(plan.peers[0].underlay_endpoint, "10.0.0.2:65001");
        assert_eq!(plan.peers[0].fabric_ip, "100.100.0.2");
        assert_eq!(plan.peers[1].node_id, "node-3");
        assert_eq!(plan.peers[1].public_key, "wg-pub-node-3");
        assert_eq!(plan.peers[1].underlay_endpoint, "10.0.0.3:65001");
        assert_eq!(plan.peers[1].fabric_ip, "100.100.0.3");
    }

    /// An overlay update without a fabric plan relays `fabric: None` —
    /// legacy VXLAN-only updates keep taking the non-fabric nwd path.
    #[tokio::test]
    async fn update_overlay_without_fabric_relays_none() {
        use crate::daemon_clients::fabric_test_support::{FabricNwdCalls, MockFabricNwd};

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("nwd.sock");

        let calls = std::sync::Arc::new(FabricNwdCalls::default());
        {
            let uds = tokio::net::UnixListener::bind(&socket).unwrap();
            let service = MockFabricNwd {
                calls: calls.clone(),
                public_key: String::new(),
                underlay_mtu: 0,
                identity_error: false,
            };
            tokio::spawn(async move {
                tonic::transport::Server::builder()
                    .add_service(
                        chv_nwd_api::chv_nwd_api::network_service_server::NetworkServiceServer::new(
                            service,
                        ),
                    )
                    .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds))
                    .await
                    .ok();
            });
        }

        let mut cache = NodeCache::new("node-1");
        cache.node_state = crate::state_machine::NodeState::TenantReady
            .as_str()
            .to_string();
        cache.connectivity_state = crate::connectivity::ConnectivityState::Connected;
        let server = AgentServer::new(
            Arc::new(tokio::sync::Mutex::new(cache)),
            VmRuntime::new(Arc::new(MockCloudHypervisorAdapter::default())),
            std::path::PathBuf::from("/run/chv/stord/api.sock"),
            socket,
            None,
            dir.path().to_path_buf(),
        );

        let req = proto::UpdateOverlayRequest {
            meta: Some(test_meta("1")),
            node_id: "node-1".to_string(),
            network_id: "net-1".to_string(),
            vni: 42,
            vtep_endpoints: vec![proto::VtepEndpoint {
                node_id: "node-2".to_string(),
                vtep_ip: "10.0.0.2".to_string(),
                vtep_port: 4789,
            }],
            fdb_entries: vec![],
            fabric: None,
        };
        let resp = proto::lifecycle_service_server::LifecycleService::update_overlay(
            &server,
            Request::new(req),
        )
        .await;
        assert!(resp.is_ok());

        let recorded = calls.overlay_fabrics.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0], chv_nwd_api::chv_nwd_api::FabricPlan::default());
    }

    /// The five Core-routed lifecycle handlers must NOT be gated: with an
    /// authority attached they enter the Core-routing branch and fail with a
    /// transport/operation error (here, the disconnected handle), never
    /// `Unimplemented`. This guards against an over-broad gate breaking the
    /// control plane's core-managed lifecycle path.
    #[tokio::test]
    async fn core_managed_routed_lifecycle_is_not_unimplemented() {
        let mut server = test_server();
        server.core_authority = Some(cellhv_core_operations::AuthorityHandle::disconnected());
        let create = proto::lifecycle_service_server::LifecycleService::create_vm(
            &server,
            Request::new(proto::CreateVmRequest::default()),
        )
        .await;
        let start = proto::lifecycle_service_server::LifecycleService::start_vm(
            &server,
            Request::new(proto::StartVmRequest::default()),
        )
        .await;
        let desired = proto::reconcile_service_server::ReconcileService::apply_vm_desired_state(
            &server,
            Request::new(proto::ApplyVmDesiredStateRequest::default()),
        )
        .await;
        for (name, result) in [
            ("create_vm", create),
            ("start_vm", start),
            ("apply_vm_desired_state", desired),
        ] {
            let err = result.unwrap_err();
            assert_ne!(
                err.code(),
                tonic::Code::Unimplemented,
                "{name} must stay core-routed (not gated) in core-managed mode"
            );
        }
    }

    /// The inspect-required resolution egress is the inverse gate: it only
    /// exists in core-managed mode and must fail closed (`Unimplemented`) for
    /// legacy-only agents.
    #[tokio::test]
    async fn legacy_mode_resolve_inspect_required_fails_closed() {
        let server = test_server();
        let resp =
            proto::lifecycle_service_server::LifecycleService::resolve_inspect_required_operation(
                &server,
                Request::new(proto::ResolveInspectRequiredOperationRequest::default()),
            )
            .await;
        assert_eq!(
            resp.unwrap_err().code(),
            tonic::Code::Unimplemented,
            "resolve_inspect_required_operation must fail closed in legacy-only mode"
        );
    }

    /// The resolution egress parses its payload strictly before touching the
    /// authority: unknown dispositions, empty notes, and empty identifiers
    /// are `InvalidArgument`, never a guessed resolution. A well-formed
    /// request reaches the authority instead of being rejected locally.
    #[tokio::test]
    async fn resolve_inspect_required_validates_payload_strictly() {
        let mut server = test_server();
        server.core_authority = Some(cellhv_core_operations::AuthorityHandle::disconnected());
        let request = |vm_id: &str, operation_id: &str, disposition: &str, note: &str| {
            proto::ResolveInspectRequiredOperationRequest {
                meta: Some(test_meta("9")),
                vm_id: vm_id.to_string(),
                operation_id: operation_id.to_string(),
                disposition: disposition.to_string(),
                note: note.to_string(),
            }
        };
        for (label, req) in [
            (
                "unknown disposition",
                request("vm-1", "op-1", "maybe", "operator inspected"),
            ),
            ("empty note", request("vm-1", "op-1", "succeeded", "   ")),
            (
                "oversized note",
                request("vm-1", "op-1", "succeeded", &"a".repeat(8_001)),
            ),
            (
                "control characters in note",
                request(
                    "vm-1",
                    "op-1",
                    "succeeded",
                    "operator inspected\nsecond line",
                ),
            ),
            (
                "control characters in vm_id",
                request("vm-1\nforged", "op-1", "succeeded", "operator inspected"),
            ),
            (
                "empty vm_id",
                request("", "op-1", "succeeded", "operator inspected"),
            ),
            (
                "empty operation_id",
                request("vm-1", "", "failed", "operator inspected"),
            ),
        ] {
            let resp =
                proto::lifecycle_service_server::LifecycleService::resolve_inspect_required_operation(
                    &server,
                    Request::new(req),
                )
                .await;
            assert_eq!(
                resp.unwrap_err().code(),
                tonic::Code::InvalidArgument,
                "{label} must be rejected before the authority is touched"
            );
        }
        let resp =
            proto::lifecycle_service_server::LifecycleService::resolve_inspect_required_operation(
                &server,
                Request::new(request("vm-1", "op-1", "failed", "operator inspected")),
            )
            .await;
        let err = resp.unwrap_err();
        assert_ne!(
            err.code(),
            tonic::Code::InvalidArgument,
            "well-formed resolve requests must reach the authority"
        );
        assert_ne!(
            err.code(),
            tonic::Code::Unimplemented,
            "resolve must stay core-routed in core-managed mode"
        );
    }

    /// The audit identity is payload too: a resolution without a
    /// `requested_by` identity — blank, or carrying control characters that
    /// could inject into the single-line audit record — must be rejected
    /// before the authority is touched.
    #[tokio::test]
    async fn resolve_inspect_required_requires_audit_identity() {
        let mut server = test_server();
        server.core_authority = Some(cellhv_core_operations::AuthorityHandle::disconnected());
        let base = || proto::ResolveInspectRequiredOperationRequest {
            meta: Some(test_meta("9")),
            vm_id: "vm-1".to_string(),
            operation_id: "op-1".to_string(),
            disposition: "succeeded".to_string(),
            note: "operator inspected".to_string(),
        };
        for (label, requested_by) in [
            ("blank requested_by", "   "),
            ("control characters", "cp\ninjected"),
        ] {
            let mut req = base();
            req.meta.as_mut().unwrap().requested_by = requested_by.to_string();
            let resp = proto::lifecycle_service_server::LifecycleService::resolve_inspect_required_operation(
                &server,
                Request::new(req),
            )
            .await;
            assert_eq!(
                resp.unwrap_err().code(),
                tonic::Code::InvalidArgument,
                "{label} must be rejected before the authority is touched"
            );
        }
    }

    /// End-to-end with a real authority over a real journal: the
    /// core-routed lifecycle handlers must surface a VM unknown to the core
    /// journal as `not_found` — the expected-version probe must never guess
    /// version 1 for a phantom VM.
    #[tokio::test]
    async fn core_routed_lifecycle_maps_unknown_vm_to_not_found() {
        let dir = tempfile::tempdir().unwrap();
        // The store requires the fresh Core parent to be an euid-owned 0700
        // directory; tempdir's mode depends on the host umask, so pin it
        // explicitly (same pattern as the cellhv-core-store test suite).
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let host = cellhv_core_types::HostIdentity {
            id: cellhv_core_types::HostId::new("agent-server-test-host").unwrap(),
            resource_version: cellhv_core_types::ResourceVersion::new(1).unwrap(),
        };
        let service = cellhv_core_operations::OperationService::create_new(
            &dir.path().join("core.db"),
            &host,
        )
        .unwrap();
        let (authority, join) = cellhv_core_operations::AuthorityActor::spawn(service, 16).unwrap();
        let mut server = test_server();
        server.core_authority = Some(authority.clone());
        let resp = proto::lifecycle_service_server::LifecycleService::start_vm(
            &server,
            Request::new(proto::StartVmRequest {
                meta: Some(test_meta("9")),
                vm_id: "vm-unknown".to_string(),
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(
            resp.unwrap_err().code(),
            tonic::Code::NotFound,
            "unknown VM must surface as not_found, never a guessed version-1 CAS"
        );
        authority.shutdown().await.unwrap();
        join.join().await.unwrap();
    }

    /// Shared fixture for the desired-state shim tests: a real authority over
    /// a real Core journal (execution is not driven — submit-time behavior is
    /// what the shim owns). The returned guards keep the journal directory
    /// and actor alive for the test's duration; dropping them shuts the
    /// actor down gracefully.
    async fn shim_server() -> (
        AgentServer,
        cellhv_core_operations::AuthorityHandle,
        tempfile::TempDir,
        cellhv_core_operations::AuthorityActorJoin,
    ) {
        let dir = tempfile::tempdir().unwrap();
        // The store requires the fresh Core parent to be an euid-owned 0700
        // directory; tempdir's mode depends on the host umask, so pin it
        // explicitly (same pattern as the cellhv-core-store test suite).
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let host = cellhv_core_types::HostIdentity {
            id: cellhv_core_types::HostId::new("agent-server-test-host").unwrap(),
            resource_version: cellhv_core_types::ResourceVersion::new(1).unwrap(),
        };
        let service = cellhv_core_operations::OperationService::create_new(
            &dir.path().join("core.db"),
            &host,
        )
        .unwrap();
        let (authority, join) = cellhv_core_operations::AuthorityActor::spawn(service, 16).unwrap();
        let mut server = test_server();
        server.core_authority = Some(authority.clone());
        (server, authority, dir, join)
    }

    fn desired_state_request(
        meta: proto::RequestMeta,
        vm_id: &str,
        fragment_id: &str,
        spec_json: &str,
    ) -> proto::ApplyVmDesiredStateRequest {
        // The control plane's node client sets the fragment generation from
        // the same accept-time task generation as the request meta; the
        // fixture mirrors that invariant (the divergence test below breaks
        // it deliberately).
        let generation = meta.desired_state_version.clone();
        proto::ApplyVmDesiredStateRequest {
            meta: Some(meta),
            node_id: "node-1".to_string(),
            vm_id: vm_id.to_string(),
            fragment: Some(proto::DesiredStateFragment {
                id: fragment_id.to_string(),
                kind: "vm".to_string(),
                generation,
                spec_json: spec_json.as_bytes().to_vec(),
                policy_json: vec![],
                updated_at: "2026-09-28T00:00:00Z".to_string(),
                updated_by: "cp".to_string(),
            }),
        }
    }

    /// Like [`test_meta`] but with a real timestamp: submissions (unlike
    /// version probes) validate request metadata, which requires
    /// `request_unix_ms > 0`.
    fn submit_meta(desired_state_version: &str) -> proto::RequestMeta {
        proto::RequestMeta {
            request_unix_ms: 1_759_000_000_000,
            ..test_meta(desired_state_version)
        }
    }

    /// Claim and terminally fail the VM's latest journaled create through
    /// the execution capability — the #368 fixture shape (a transient
    /// effector failure with a public-safe code).
    async fn terminally_fail_latest_create(
        authority: &cellhv_core_operations::AuthorityHandle,
        vm_id: &str,
    ) {
        let latest = authority
            .latest_create_state(cellhv_core_types::VmId::new(vm_id).unwrap())
            .await
            .unwrap()
            .expect("fixture requires a journaled create");
        let execution = authority.execution_handle();
        let token = cellhv_core_operations::AttemptToken::new("attempt-shim-fail").unwrap();
        let operation_id = latest.operation_id.clone();
        assert!(matches!(
            execution
                .claim_attempt(operation_id.clone(), token.clone())
                .await
                .unwrap(),
            cellhv_core_operations::ClaimResult::Acquired(_)
        ));
        execution
            .finish(
                operation_id,
                token,
                cellhv_core_operations::TerminalOutcome::Failed(
                    serde_json::json!({"code": "RUNTIME_UNAVAILABLE"}),
                ),
            )
            .await
            .unwrap();
    }

    /// The M2.5 dispatch shim: a generation-1 desired-state dispatch (the
    /// control plane's create) routes through the Core authority. The VM
    /// lands in the Core journal at version 1, the NodeCache VM axis is
    /// NOT written directly (it stays a projection of Core execution), and
    /// a verbatim retry of the same operation replays idempotently instead
    /// of double-creating.
    #[tokio::test]
    async fn core_managed_desired_state_create_routes_through_core() {
        let (server, authority, _dir, _join) = shim_server().await;
        let spec = r#"{"name":"vm-new","cpus":1,"memory_bytes":1024,"kernel_path":"/dev/null","disks":[],"nics":[]}"#;
        let make_request = || desired_state_request(submit_meta("1"), "vm-new", "vm-new", spec);

        let resp = proto::reconcile_service_server::ReconcileService::apply_vm_desired_state(
            &server,
            Request::new(make_request()),
        )
        .await
        .expect("generation-1 create dispatch must be accepted");
        let result = resp.into_inner().result.unwrap();
        assert_eq!(result.status, "ok");
        assert!(result.human_summary.contains("Accepted"));

        // The authority journal holds the VM at version 1.
        let vm = authority
            .vm(cellhv_core_types::VmId::new("vm-new").unwrap())
            .await
            .expect("VM must exist in the Core journal");
        assert_eq!(vm.resource_version.get(), 1);

        // The NodeCache was not written: the VM axis is projection-only in
        // core-managed mode.
        assert!(
            server
                .cache
                .lock()
                .await
                .get_fragment("vm", "vm-new")
                .is_none(),
            "the shim must never write the cache directly"
        );

        // A verbatim retry (same operation id and generation — the control
        // plane retries the same row) replays instead of double-creating.
        let replay = proto::reconcile_service_server::ReconcileService::apply_vm_desired_state(
            &server,
            Request::new(make_request()),
        )
        .await
        .expect("create retry must replay, not fail");
        let replay_result = replay.into_inner().result.unwrap();
        assert_eq!(replay_result.status, "ok");
        assert!(
            replay_result.human_summary.contains("Replay"),
            "retry must surface the replay disposition, got {:?}",
            replay_result.human_summary
        );
        let vm_after = authority
            .vm(cellhv_core_types::VmId::new("vm-new").unwrap())
            .await
            .unwrap();
        assert_eq!(vm_after.resource_version.get(), 1);
    }

    /// #368 C1: a generation-1 re-drive task (a NEW control-plane operation
    /// id for a VM whose journaled create terminally failed) routes through
    /// the Core requeue primitive — a NEW CreateVm operation is journaled
    /// for the residue-idempotent effector, the failed original stays
    /// terminal, and a verbatim retry of the re-drive task converges on the
    /// one requeued operation.
    #[tokio::test]
    async fn core_managed_create_redrive_routes_through_the_requeue_primitive() {
        let (server, authority, _dir, _join) = shim_server().await;
        let spec = r#"{"name":"vm-new","cpus":1,"memory_bytes":1024,"kernel_path":"/dev/null","disks":[],"nics":[]}"#;

        // The original create task, then its terminal effector failure.
        let create_meta = proto::RequestMeta {
            operation_id: "cp-create-1".to_owned(),
            ..submit_meta("1")
        };
        proto::reconcile_service_server::ReconcileService::apply_vm_desired_state(
            &server,
            Request::new(desired_state_request(create_meta, "vm-new", "vm-new", spec)),
        )
        .await
        .expect("original create dispatch must be accepted");
        terminally_fail_latest_create(&authority, "vm-new").await;
        let failed = authority
            .latest_create_state(cellhv_core_types::VmId::new("vm-new").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            failed.status,
            cellhv_core_types::OperationStatus::Failed,
            "fixture: the create must be terminally failed before the re-drive"
        );

        // The re-drive task: same generation, FRESH operation id (what the
        // orchestrator's re-drive pass issues).
        let redrive_meta = proto::RequestMeta {
            operation_id: "cp-redrive-1".to_owned(),
            ..submit_meta("1")
        };
        let resp = proto::reconcile_service_server::ReconcileService::apply_vm_desired_state(
            &server,
            Request::new(desired_state_request(
                redrive_meta.clone(),
                "vm-new",
                "vm-new",
                spec,
            )),
        )
        .await
        .expect("re-drive dispatch must be accepted");
        let result = resp.into_inner().result.unwrap();
        assert_eq!(result.status, "ok");
        assert!(result.human_summary.contains("Accepted"));

        // The journal now holds TWO create operations for the VM: the
        // failed original (untouched, still terminal) and the requeued
        // re-drive (accepted, claimable). The live row was never rewritten.
        let operations = authority.operations().await.unwrap();
        let creates = operations
            .iter()
            .filter(|entry| entry.operation.kind == cellhv_core_types::OperationKind::CreateVm)
            .count();
        assert_eq!(creates, 2);
        let latest = authority
            .latest_create_state(cellhv_core_types::VmId::new("vm-new").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(latest.status, cellhv_core_types::OperationStatus::Accepted);
        assert_eq!(
            authority
                .vm(cellhv_core_types::VmId::new("vm-new").unwrap())
                .await
                .unwrap()
                .resource_version
                .get(),
            1,
            "the requeue must never rewrite the vms row"
        );

        // A verbatim retry of the re-drive task (the control plane retries
        // the same operation row) converges on the one requeued operation.
        let replay = proto::reconcile_service_server::ReconcileService::apply_vm_desired_state(
            &server,
            Request::new(desired_state_request(
                redrive_meta,
                "vm-new",
                "vm-new",
                spec,
            )),
        )
        .await
        .expect("re-drive retry must replay, not fail");
        let replay_result = replay.into_inner().result.unwrap();
        assert_eq!(replay_result.status, "ok");
        assert!(replay_result.human_summary.contains("Replay"));
        let operations = authority.operations().await.unwrap();
        let creates = operations
            .iter()
            .filter(|entry| entry.operation.kind == cellhv_core_types::OperationKind::CreateVm)
            .count();
        assert_eq!(creates, 2, "the retry must not insert a third create");
    }

    /// #368 C1 fence cooperation: while the latest create is incomplete
    /// (in flight or inspect-required), a re-drive dispatch is refused with
    /// FailedPrecondition — never a concurrent second create.
    #[tokio::test]
    async fn core_managed_create_redrive_refuses_an_incomplete_create() {
        let (server, authority, _dir, _join) = shim_server().await;
        let spec = r#"{"name":"vm-new","cpus":1,"memory_bytes":1024,"kernel_path":"/dev/null","disks":[],"nics":[]}"#;
        let create_meta = proto::RequestMeta {
            operation_id: "cp-create-1".to_owned(),
            ..submit_meta("1")
        };
        proto::reconcile_service_server::ReconcileService::apply_vm_desired_state(
            &server,
            Request::new(desired_state_request(create_meta, "vm-new", "vm-new", spec)),
        )
        .await
        .unwrap();
        // Leave the create claimed (running) — the in-flight shape.
        let execution = authority.execution_handle();
        let latest = authority
            .latest_create_state(cellhv_core_types::VmId::new("vm-new").unwrap())
            .await
            .unwrap()
            .unwrap();
        let token = cellhv_core_operations::AttemptToken::new("attempt-shim-1").unwrap();
        assert!(matches!(
            execution
                .claim_attempt(latest.operation_id, token)
                .await
                .unwrap(),
            cellhv_core_operations::ClaimResult::Acquired(_)
        ));

        let redrive_meta = proto::RequestMeta {
            operation_id: "cp-redrive-1".to_owned(),
            ..submit_meta("1")
        };
        let error = proto::reconcile_service_server::ReconcileService::apply_vm_desired_state(
            &server,
            Request::new(desired_state_request(
                redrive_meta,
                "vm-new",
                "vm-new",
                spec,
            )),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        // No second create was journaled.
        let operations = authority.operations().await.unwrap();
        assert_eq!(operations.len(), 1);
    }

    /// #368 review round 2 — joins the two separately-pinned halves of the
    /// crash-recovery loop on the agent side: a re-drive claimed by the
    /// executor and interrupted by a process crash surfaces as
    /// InspectRequired after the restart classification; the operator
    /// resolves it as Failed through the REAL resolve RPC, and P1's
    /// `apply_core_create_states` then reports the VM as
    /// `runtime_status="Failed"` with the resolution code — the exact
    /// telemetry state the control plane's `redrive_failed_creates` pass
    /// keys on. The CP half of the join is pinned in
    /// chv-controlplane-service
    /// (`redrive_rearms_after_operator_resolves_inspect_required`), which
    /// consumes this exact report shape through the real telemetry
    /// ingestion.
    #[tokio::test]
    async fn resolve_inspect_required_as_failed_reports_failed_state_for_redrive() {
        let (server, authority, dir, join) = shim_server().await;
        let spec = r#"{"name":"vm-new","cpus":1,"memory_bytes":1024,"kernel_path":"/dev/null","disks":[],"nics":[]}"#;

        // The #368 shape: the original create terminally fails, then the
        // re-drive dispatch requeues a fresh journaled create.
        let create_meta = proto::RequestMeta {
            operation_id: "cp-create-1".to_owned(),
            ..submit_meta("1")
        };
        proto::reconcile_service_server::ReconcileService::apply_vm_desired_state(
            &server,
            Request::new(desired_state_request(create_meta, "vm-new", "vm-new", spec)),
        )
        .await
        .expect("original create dispatch must be accepted");
        terminally_fail_latest_create(&authority, "vm-new").await;

        let redrive_meta = proto::RequestMeta {
            operation_id: "cp-redrive-1".to_owned(),
            ..submit_meta("1")
        };
        proto::reconcile_service_server::ReconcileService::apply_vm_desired_state(
            &server,
            Request::new(desired_state_request(
                redrive_meta,
                "vm-new",
                "vm-new",
                spec,
            )),
        )
        .await
        .expect("re-drive dispatch must be accepted");

        // The agent claims the requeued create, then the process dies
        // mid-run (the crash-during-re-drive window).
        let latest = authority
            .latest_create_state(cellhv_core_types::VmId::new("vm-new").unwrap())
            .await
            .unwrap()
            .expect("requeued create");
        let execution = authority.execution_handle();
        let token = cellhv_core_operations::AttemptToken::new("attempt-crash").unwrap();
        assert!(matches!(
            execution
                .claim_attempt(latest.operation_id.clone(), token)
                .await
                .unwrap(),
            cellhv_core_operations::ClaimResult::Acquired(_)
        ));
        // Crash + restart: shut the actor down, reopen the journal, and
        // run the restart classification (the existing M2.4 machinery).
        authority.shutdown().await.unwrap();
        join.join().await.unwrap();
        let service =
            cellhv_core_operations::OperationService::open_existing(&dir.path().join("core.db"))
                .unwrap();
        let (authority, join) = cellhv_core_operations::AuthorityActor::spawn(service, 16).unwrap();
        let classified = authority
            .execution_handle()
            .classify_restart_interrupted()
            .await
            .unwrap();
        assert_eq!(
            classified.len(),
            1,
            "the interrupted re-drive must be classified for operator resolution"
        );

        // While the re-drive is inspect-required, P1 must NOT report the
        // VM Failed: a rebuild-seeded fragment (reported as its desired
        // state) is flipped to Pending by the latest-create merge — the
        // signal that holds the CP's re-drive selection closed.
        let pre_resolve = crate::reconcile::apply_core_create_states(
            vec![crate::vm_runtime::VmRecord {
                vm_id: "vm-new".to_owned(),
                observed_generation: "1".to_owned(),
                runtime_status: "Running".to_owned(),
                last_error: None,
                consecutive_failures: 0,
                cpus: 1,
                memory_bytes: 1024,
            }],
            authority.latest_create_states().await.unwrap(),
        );
        assert_eq!(pre_resolve.len(), 1, "one record reported: {pre_resolve:?}");
        assert_eq!(
            pre_resolve[0].runtime_status, "Pending",
            "an inspect-required create must report Pending, never Failed nor the desired phantom"
        );

        // The operator resolves the inspect-required re-drive as Failed
        // through the real RPC surface (the agent-shim resolve seam).
        let latest = authority
            .latest_create_state(cellhv_core_types::VmId::new("vm-new").unwrap())
            .await
            .unwrap()
            .expect("requeued create");
        let mut resolved_server = test_server();
        resolved_server.core_authority = Some(authority.clone());
        let resp =
            proto::lifecycle_service_server::LifecycleService::resolve_inspect_required_operation(
                &resolved_server,
                Request::new(proto::ResolveInspectRequiredOperationRequest {
                    meta: Some(submit_meta("1")),
                    vm_id: "vm-new".to_string(),
                    operation_id: latest.operation_id.as_str().to_string(),
                    disposition: "failed".to_string(),
                    note: "operator verified the backend did not recover".to_string(),
                }),
            )
            .await
            .expect("resolve must succeed against the real authority");
        assert_eq!(resp.into_inner().result.unwrap().status, "ok");

        // P1: the merged report now carries the Failed state with the
        // resolution code — the exact VmStateReport payload
        // (runtime_status="Failed", last_error="OPERATOR_RESOLUTION",
        // observed_generation="0") the CP-side counterpart test consumes.
        let merged = crate::reconcile::apply_core_create_states(
            Vec::new(),
            authority.latest_create_states().await.unwrap(),
        );
        assert_eq!(
            merged.len(),
            1,
            "the failed create must be reported: {merged:?}"
        );
        assert_eq!(merged[0].vm_id, "vm-new");
        assert_eq!(merged[0].runtime_status, "Failed");
        assert_eq!(
            merged[0].last_error.as_deref(),
            Some("OPERATOR_RESOLUTION"),
            "the public-safe resolution code must ride the report"
        );
        assert_eq!(merged[0].observed_generation, "0");

        authority.shutdown().await.unwrap();
        join.join().await.unwrap();
    }

    /// #368 C1: a duplicate create dispatch after the create converged is
    /// an idempotent success — no new operation, no requeue.
    #[tokio::test]
    async fn core_managed_create_dispatch_after_success_acks_idempotently() {
        let (server, authority, _dir, _join) = shim_server().await;
        let spec = r#"{"name":"vm-new","cpus":1,"memory_bytes":1024,"kernel_path":"/dev/null","disks":[],"nics":[]}"#;
        let create_meta = proto::RequestMeta {
            operation_id: "cp-create-1".to_owned(),
            ..submit_meta("1")
        };
        proto::reconcile_service_server::ReconcileService::apply_vm_desired_state(
            &server,
            Request::new(desired_state_request(create_meta, "vm-new", "vm-new", spec)),
        )
        .await
        .unwrap();
        // The create succeeds.
        let execution = authority.execution_handle();
        let latest = authority
            .latest_create_state(cellhv_core_types::VmId::new("vm-new").unwrap())
            .await
            .unwrap()
            .unwrap();
        let token = cellhv_core_operations::AttemptToken::new("attempt-shim-1").unwrap();
        let succeeded_id = latest.operation_id.clone();
        execution
            .claim_attempt(succeeded_id.clone(), token.clone())
            .await
            .unwrap();
        execution
            .finish(
                succeeded_id,
                token,
                cellhv_core_operations::TerminalOutcome::Succeeded(Some(
                    serde_json::json!({"runtime": "created"}),
                )),
            )
            .await
            .unwrap();

        // A NEW generation-1 task for the converged VM acks ok without
        // journaling anything.
        let duplicate_meta = proto::RequestMeta {
            operation_id: "cp-other-1".to_owned(),
            ..submit_meta("1")
        };
        let resp = proto::reconcile_service_server::ReconcileService::apply_vm_desired_state(
            &server,
            Request::new(desired_state_request(
                duplicate_meta,
                "vm-new",
                "vm-new",
                spec,
            )),
        )
        .await
        .expect("duplicate dispatch after convergence must ack ok");
        let result = resp.into_inner().result.unwrap();
        assert_eq!(result.status, "ok");
        assert_eq!(authority.operations().await.unwrap().len(), 1);
    }

    /// A spec update (resize carries generation >= 2) is refused at the
    /// shim boundary BEFORE Core reserves any desired state: the Core
    /// executor does not implement UpdateVm, so accepting it would journal
    /// a definition the runtime cannot converge to.
    #[tokio::test]
    async fn core_managed_desired_state_update_refused_before_core_reservation() {
        let (server, authority, _dir, _join) = shim_server().await;
        let spec = r#"{"name":"vm-new","cpus":2,"memory_bytes":2048,"kernel_path":"/dev/null","disks":[],"nics":[]}"#;
        let request = desired_state_request(submit_meta("2"), "vm-new", "vm-new", spec);

        let error = proto::reconcile_service_server::ReconcileService::apply_vm_desired_state(
            &server,
            Request::new(request),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.code(),
            tonic::Code::Unimplemented,
            "spec updates must fail closed with an explicit refusal"
        );

        // Nothing was reserved in Core: the VM is still unknown to the
        // journal (no desired state, no version bump).
        let vm = authority
            .vm(cellhv_core_types::VmId::new("vm-new").unwrap())
            .await;
        assert!(vm.is_err(), "no Core state may be reserved by a refusal");
    }

    /// The routing decision must never guess: every non-canonical
    /// generation — non-numeric, empty, zero, leading zeros, signed
    /// forms, surrounding whitespace, or out of i64 range — is rejected
    /// as InvalidArgument BEFORE the stale gate and the authority. The
    /// cache is seeded so the VM is already in the projection: the code
    /// must not depend on projection state (a malformed generation is
    /// InvalidArgument whether or not a projection entry exists, never
    /// a stale-gate FailedPrecondition).
    #[tokio::test]
    async fn core_managed_desired_state_rejects_unparseable_generation() {
        let mut server = test_server();
        server.core_authority = Some(cellhv_core_operations::AuthorityHandle::disconnected());
        // Seed the projection so the stale-gate branch is live for this VM:
        // the malformed-generation rejection must fire before it.
        {
            let mut cache = server.cache.lock().await;
            cache.observe_generation("vm", "vm-x", "1");
        }
        let spec = r#"{"name":"vm-x","cpus":1,"memory_bytes":1024,"kernel_path":"/dev/null","disks":[],"nics":[]}"#;
        for malformed in [
            "not-a-number",
            "",
            "0",
            "00",
            "01",
            "+1",
            "-1",
            " 1",
            "1 ",
            // i64::MAX + 1 and u64::MAX: generations are control-plane
            // i64 sequence values; anything larger is out of range, not
            // an update task.
            "9223372036854775808",
            "18446744073709551615",
        ] {
            let error = proto::reconcile_service_server::ReconcileService::apply_vm_desired_state(
                &server,
                Request::new(desired_state_request(
                    test_meta(malformed),
                    "vm-x",
                    "vm-x",
                    spec,
                )),
            )
            .await
            .unwrap_err();
            assert_eq!(
                error.code(),
                tonic::Code::InvalidArgument,
                "generation {malformed:?} must be rejected as invalid argument"
            );
        }
    }

    /// The fragment's generation must agree with the task's: a divergence
    /// is malformed dispatch data (the control plane sets both from the
    /// same accept-time value), rejected before any authority access.
    #[tokio::test]
    async fn core_managed_desired_state_rejects_fragment_generation_divergence() {
        let mut server = test_server();
        server.core_authority = Some(cellhv_core_operations::AuthorityHandle::disconnected());
        let spec = r#"{"name":"vm-x","cpus":1,"memory_bytes":1024,"kernel_path":"/dev/null","disks":[],"nics":[]}"#;
        let mut request = desired_state_request(test_meta("1"), "vm-x", "vm-x", spec);
        request.fragment.as_mut().unwrap().generation = "5".to_string();
        let error = proto::reconcile_service_server::ReconcileService::apply_vm_desired_state(
            &server,
            Request::new(request),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.code(),
            tonic::Code::InvalidArgument,
            "a fragment/meta generation divergence must never be arbitrated or guessed"
        );
    }

    /// A fragment whose identity disagrees with the request's VM id is
    /// malformed dispatch data, rejected before any authority access.
    #[tokio::test]
    async fn core_managed_desired_state_rejects_fragment_identity_mismatch() {
        let mut server = test_server();
        server.core_authority = Some(cellhv_core_operations::AuthorityHandle::disconnected());
        let spec = r#"{"name":"vm-x","cpus":1,"memory_bytes":1024,"kernel_path":"/dev/null","disks":[],"nics":[]}"#;
        let error = proto::reconcile_service_server::ReconcileService::apply_vm_desired_state(
            &server,
            Request::new(desired_state_request(
                test_meta("1"),
                "vm-x",
                "some-other-vm",
                spec,
            )),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), tonic::Code::InvalidArgument);
    }

    /// The same stale-generation gate as the legacy branch: a dispatch
    /// older than the last projected outcome is rejected before Core sees
    /// it (the disconnected handle would surface as Unavailable if the
    /// submission were attempted).
    #[tokio::test]
    async fn core_managed_desired_state_rejects_stale_generation() {
        let mut server = test_server();
        server.core_authority = Some(cellhv_core_operations::AuthorityHandle::disconnected());
        {
            let mut cache = server.cache.lock().await;
            cache.observe_generation("vm", "vm-old", "5");
        }
        let spec = r#"{"name":"vm-old","cpus":1,"memory_bytes":1024,"kernel_path":"/dev/null","disks":[],"nics":[]}"#;
        let error = proto::reconcile_service_server::ReconcileService::apply_vm_desired_state(
            &server,
            Request::new(desired_state_request(
                test_meta("3"),
                "vm-old",
                "vm-old",
                spec,
            )),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    }

    /// Node-operator state transitions (drain/maintenance/scheduling) are not
    /// lifecycle effectors: they must keep working in core-managed mode so
    /// operators can still drain or maintain a node.
    #[tokio::test]
    async fn core_managed_operator_node_state_remains_available() {
        let mut server = test_server();
        server.core_authority = Some(cellhv_core_operations::AuthorityHandle::disconnected());
        let resp = proto::lifecycle_service_server::LifecycleService::drain_node(
            &server,
            Request::new(proto::DrainNodeRequest {
                meta: Some(test_meta("5")),
                ..Default::default()
            }),
        )
        .await;
        assert!(resp.is_ok());
        assert_eq!(server.cache.lock().await.node_state, "Draining");
    }
}
