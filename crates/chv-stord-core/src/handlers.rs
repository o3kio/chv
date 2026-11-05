use crate::migration::sender::{start_migration_to_peer, MigrationTlsConfig};
use crate::migration::task::{MigrationPhase, MigrationTask, MigrationTaskTable};
use crate::session::{Session, SessionTable};
use chv_common::migration_auth::MigrationTokenCodec;
use chv_common::types::{BackendLocator, DevicePolicy};
use chv_errors::{ChvError, ErrorCode};
use chv_observability::{operation_span, Metrics};
use chv_stord_api::chv_stord_api as proto;
use chv_stord_backends::StorageBackend;
use std::sync::Arc;
use std::time::Duration;
use tonic::{Request, Response, Status};

/// Bound on the source stord trigger's wait for the destination's initial
/// handshake/admission result (issue #663 follow-up).
///
/// The destination decides authorization and per-node admission *before* it
/// creates the receiving volume and before it sends response headers, so a
/// rejection (including `RESOURCE_EXHAUSTED`) normally arrives within one
/// network round trip. Awaiting it here lets the agent's bounded retry act
/// on a transient capacity rejection instead of seeing only an asynchronous
/// task failure. The bound protects the trigger RPC from a slow or wedged
/// destination: on timeout the migration is considered started (the
/// pre-existing behavior) and continues in the background.
const TRIGGER_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) trait ChvErrorProtoExt {
    fn to_proto_result(&self) -> proto::Result;
}

impl ChvErrorProtoExt for ChvError {
    fn to_proto_result(&self) -> proto::Result {
        let (status, error_code, human_summary) = self.to_result_fields();
        proto::Result {
            status: status.to_string(),
            error_code: error_code.to_string(),
            human_summary,
        }
    }
}

pub(crate) fn contains_dot_components(path: &str) -> bool {
    path.split('/').any(|c| c == "." || c == "..")
}

/// Canonicalize `path`, resolving symlinks. If the path does not exist
/// yet (the common case for not-yet-created volume files), canonicalize
/// the nearest existing ancestor and re-append the missing components
/// lexically — they cannot contain symlinks because they do not exist.
///
/// The fallback only triggers for `NotFound`; every other error (EACCES,
/// ELOOP, ...) is treated as a verification failure and returned as an
/// error so the allowlist check can never pass on a resolution failure.
pub(crate) fn canonicalize_or_ancestor(
    path: &std::path::Path,
) -> Result<std::path::PathBuf, ChvError> {
    match std::fs::canonicalize(path) {
        Ok(canonical) => Ok(canonical),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut suffix: Vec<std::ffi::OsString> = Vec::new();
            let mut cur = path;
            loop {
                match std::fs::canonicalize(cur) {
                    Ok(canonical) => {
                        let mut out = canonical;
                        for component in suffix.iter().rev() {
                            out.push(component);
                        }
                        return Ok(out);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        let name = cur.file_name().ok_or_else(|| ChvError::AccessDenied {
                            resource: path.to_string_lossy().to_string(),
                            reason: "path has no resolvable file name component".to_string(),
                        })?;
                        suffix.push(name.to_os_string());
                        let parent = cur.parent().ok_or_else(|| ChvError::AccessDenied {
                            resource: path.to_string_lossy().to_string(),
                            reason: "path has no resolvable parent".to_string(),
                        })?;
                        if parent == cur {
                            // Reached the root without finding an existing
                            // ancestor; nothing left to resolve.
                            return Err(ChvError::AccessDenied {
                                resource: path.to_string_lossy().to_string(),
                                reason: "no existing ancestor found to canonicalize".to_string(),
                            });
                        }
                        cur = parent;
                    }
                    Err(e) => {
                        return Err(ChvError::AccessDenied {
                            resource: path.to_string_lossy().to_string(),
                            reason: format!("cannot canonicalize path (fail closed): {}", e),
                        });
                    }
                }
            }
        }
        Err(e) => Err(ChvError::AccessDenied {
            resource: path.to_string_lossy().to_string(),
            reason: format!("cannot canonicalize path (fail closed): {}", e),
        }),
    }
}

pub struct StorageServiceImpl<B: StorageBackend> {
    backend: Arc<B>,
    sessions: Arc<SessionTable>,
    metrics: Arc<Metrics>,
    runtime_dir: std::path::PathBuf,
    backend_allowlist: Vec<String>,
    path_allowlist: Vec<std::path::PathBuf>,
    device_allowlist: Vec<String>,
    store: Option<Arc<crate::store::SessionStore>>,
    migration_tasks: Arc<MigrationTaskTable>,
    migration_dest_allowlist: Vec<String>,
    migration_tls: Option<MigrationTlsConfig>,
    /// Bound on awaiting the destination's initial handshake inside
    /// `TriggerDiskMigration`. Production default is
    /// [`TRIGGER_HANDSHAKE_TIMEOUT`]; tests can shorten it through
    /// [`StorageServiceImpl::with_trigger_handshake_timeout`].
    trigger_handshake_timeout: Duration,
}

impl<B: StorageBackend> StorageServiceImpl<B> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        backend: Arc<B>,
        sessions: Arc<SessionTable>,
        metrics: Arc<Metrics>,
        runtime_dir: std::path::PathBuf,
        backend_allowlist: Vec<String>,
        path_allowlist: Vec<std::path::PathBuf>,
        device_allowlist: Vec<String>,
        migration_dest_allowlist: Vec<String>,
        migration_tls: Option<MigrationTlsConfig>,
    ) -> Self {
        Self {
            backend,
            sessions,
            metrics,
            runtime_dir,
            backend_allowlist,
            path_allowlist,
            device_allowlist,
            store: None,
            migration_tasks: Arc::new(MigrationTaskTable::new()),
            migration_dest_allowlist,
            migration_tls,
            trigger_handshake_timeout: TRIGGER_HANDSHAKE_TIMEOUT,
        }
    }

    /// Override the bounded wait for the destination's initial
    /// handshake/admission result. Tests use a short value so a deliberately
    /// silent destination does not stall the trigger path; production keeps
    /// [`TRIGGER_HANDSHAKE_TIMEOUT`].
    pub fn with_trigger_handshake_timeout(mut self, timeout: Duration) -> Self {
        self.trigger_handshake_timeout = timeout;
        self
    }

    pub fn sessions(&self) -> Arc<SessionTable> {
        self.sessions.clone()
    }

    pub fn set_store(&mut self, store: crate::store::SessionStore) {
        self.store = Some(Arc::new(store));
    }

    async fn persist_upsert(&self, session: &crate::session::Session) {
        if let Some(store) = &self.store {
            if let Err(e) = store.upsert(session).await {
                tracing::error!(error = %e, "failed to persist session to SQLite");
            }
        }
    }

    async fn persist_remove(&self, volume_id: &str, handle: &str) {
        if let Some(store) = &self.store {
            if let Err(e) = store.remove(volume_id, handle).await {
                tracing::error!(error = %e, "failed to remove session from SQLite");
            }
        }
    }

    fn map_backend_locator(b: Option<proto::BackendLocator>) -> Result<BackendLocator, ChvError> {
        let b = b.ok_or_else(|| ChvError::InvalidArgument {
            field: "backend".to_string(),
            reason: "missing".to_string(),
        })?;
        Ok(BackendLocator {
            backend_class: b.backend_class,
            locator: b.locator,
            options: b.options.into_iter().collect(),
        })
    }

    fn map_device_policy(p: Option<proto::DevicePolicy>) -> DevicePolicy {
        p.map(|p| DevicePolicy {
            read_bps: p.read_bps,
            write_bps: p.write_bps,
            read_iops: p.read_iops,
            write_iops: p.write_iops,
            burst_allowed: p.burst_allowed,
            read_only: p.read_only,
            no_exec: p.no_exec,
            io_scheduler: p.io_scheduler,
            cache_mode: p.cache_mode,
        })
        .unwrap_or_default()
    }

    fn ok_result() -> proto::Result {
        proto::Result {
            status: ErrorCode::OK.to_string(),
            error_code: ErrorCode::OK.to_string(),
            human_summary: String::new(),
        }
    }

    fn check_allowlist(&self, backend_class: &str) -> Result<(), ChvError> {
        if self.backend_allowlist.is_empty() {
            return Ok(());
        }
        if self.backend_allowlist.iter().any(|b| b == backend_class) {
            return Ok(());
        }
        Err(ChvError::BackendUnavailable {
            backend: backend_class.to_string(),
            reason: format!("backend class '{}' not in allowlist", backend_class),
        })
    }

    fn check_migration_dest_allowlist(&self, dest_endpoint: &str) -> Result<(), ChvError> {
        if self.migration_dest_allowlist.is_empty() {
            return Ok(());
        }
        // Extract host from endpoint, e.g. "https://host:50052" -> "host"
        let host = dest_endpoint
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .split(':')
            .next()
            .unwrap_or(dest_endpoint);
        if self.migration_dest_allowlist.iter().any(|h| h == host) {
            return Ok(());
        }
        Err(ChvError::BackendUnavailable {
            backend: "migration".to_string(),
            reason: format!("migration destination '{}' not in allowlist", dest_endpoint),
        })
    }

    /// Threat model: this check hardens the API boundary against hostile
    /// API callers (control-plane / agent requests), not against hostile
    /// local users with write access to the runtime directories. It
    /// canonicalizes the path and then the backend opens it later, so
    /// there is a check-then-use window in which a local attacker could
    /// swap in a symlink. If that threat model ever changes, the next
    /// layer is an O_NOFOLLOW reopen at the backend level.
    fn check_path_allowlist(&self, locator: &str) -> Result<(), ChvError> {
        if self.path_allowlist.is_empty() {
            return Ok(());
        }

        // (a) Reject `.` and `..` components outright, before any resolution.
        if contains_dot_components(locator) {
            return Err(ChvError::AccessDenied {
                resource: locator.to_string(),
                reason: "locator contains '.' or '..' path components".to_string(),
            });
        }

        // Resolve the effective path exactly the way the local backend will:
        // absolute locators are used verbatim, relative ones are joined onto
        // runtime_dir (see LocalFileBackend::resolve_path).
        let locator_path = std::path::Path::new(locator);
        let effective = if locator_path.is_absolute() {
            locator_path.to_path_buf()
        } else {
            self.runtime_dir.join(locator_path)
        };

        // (b) Canonicalize the resolved path, resolving any symlinks in the
        // middle. If the file does not exist yet, canonicalize the parent and
        // re-append the file name. Any resolution error rejects.
        let canonical = canonicalize_or_ancestor(&effective)?;

        // (c) Verify containment: the canonical path must start with one of
        // the allowlisted prefixes, themselves canonicalized with the same
        // ancestor fallback (the prefix dir may not exist yet). A prefix that
        // cannot be verified never grants access.
        for allowed in &self.path_allowlist {
            let Ok(allowed_canonical) = canonicalize_or_ancestor(allowed) else {
                continue;
            };
            if canonical.starts_with(&allowed_canonical) {
                return Ok(());
            }
        }

        Err(ChvError::AccessDenied {
            resource: locator.to_string(),
            reason: format!(
                "path '{}' (resolved to '{}') not within allowed prefixes: {:?}",
                locator,
                canonical.display(),
                self.path_allowlist
            ),
        })
    }

    fn matches_device_pattern(path: &str, pattern: &str) -> bool {
        // Simple glob matching: only '*' wildcard is supported.
        let parts: Vec<&str> = pattern.split('*').collect();
        if parts.is_empty() {
            return true;
        }
        if !path.starts_with(parts[0]) {
            return false;
        }
        let mut rest = &path[parts[0].len()..];
        for &part in &parts[1..] {
            if part.is_empty() {
                continue;
            }
            match rest.find(part) {
                Some(i) => rest = &rest[i + part.len()..],
                None => return false,
            }
        }
        if !pattern.ends_with('*') && !rest.is_empty() {
            return false;
        }
        true
    }

    fn check_device_allowlist(&self, path: &str) -> Result<(), ChvError> {
        if self.device_allowlist.is_empty() {
            return Ok(());
        }
        for pattern in &self.device_allowlist {
            if Self::matches_device_pattern(path, pattern) {
                return Ok(());
            }
        }
        Err(ChvError::AccessDenied {
            resource: path.to_string(),
            reason: format!(
                "device path '{}' not in device_allowlist: {:?}",
                path, self.device_allowlist
            ),
        })
    }

    /// Snapshot and clone names become path components in backend filenames
    /// (snapshots compose `{volume_id}-{snapshot_name}.img`; clones embed as
    /// the carrier `{clone_id}.img`), so reject any name that is not a
    /// single safe component before it reaches the backend.
    fn validate_snapshot_or_clone_name(name: &str, field: &str) -> Result<(), ChvError> {
        if !chv_common::is_safe_id(name) {
            return Err(ChvError::InvalidArgument {
                field: field.to_string(),
                reason: format!("'{name}' is not a safe id (must be a single path component)"),
            });
        }
        Ok(())
    }
}

#[tonic::async_trait]
impl<B: StorageBackend> proto::storage_service_server::StorageService for StorageServiceImpl<B> {
    async fn open_volume(
        &self,
        request: Request<proto::OpenVolumeRequest>,
    ) -> Result<Response<proto::OpenVolumeResponse>, Status> {
        self.metrics.increment_counter("stord_open_volume_total");
        let req = request.into_inner();
        let _span = req
            .meta
            .as_ref()
            .map(|m| operation_span(&m.operation_id))
            .unwrap_or_else(|| operation_span(""));

        if req.volume_id.is_empty() {
            return Ok(Response::new(proto::OpenVolumeResponse {
                result: Some(
                    ChvError::InvalidArgument {
                        field: "volume_id".to_string(),
                        reason: "volume_id must not be empty".to_string(),
                    }
                    .to_proto_result(),
                ),
                volume_id: req.volume_id,
                attachment_handle: String::new(),
                export_kind: String::new(),
                export_path: String::new(),
            }));
        }

        let locator = match Self::map_backend_locator(req.backend) {
            Ok(l) => l,
            Err(e) => {
                return Ok(Response::new(proto::OpenVolumeResponse {
                    result: Some(e.to_proto_result()),
                    volume_id: req.volume_id,
                    attachment_handle: String::new(),
                    export_kind: String::new(),
                    export_path: String::new(),
                }));
            }
        };

        if let Err(e) = self.check_allowlist(&locator.backend_class) {
            return Ok(Response::new(proto::OpenVolumeResponse {
                result: Some(e.to_proto_result()),
                volume_id: req.volume_id,
                attachment_handle: String::new(),
                export_kind: String::new(),
                export_path: String::new(),
            }));
        }

        // The path allowlist only makes sense for the local/filesystem
        // backend: lvm/ceph/iscsi locators are volume/device identifiers,
        // not filesystem paths, and treating them as paths would either
        // fail the check or wrongly resolve against runtime_dir.
        if matches!(
            locator.backend_class.as_str(),
            "local" | "local-file" | "localdisk"
        ) {
            if let Err(e) = self.check_path_allowlist(&locator.locator) {
                return Ok(Response::new(proto::OpenVolumeResponse {
                    result: Some(e.to_proto_result()),
                    volume_id: req.volume_id,
                    attachment_handle: String::new(),
                    export_kind: String::new(),
                    export_path: String::new(),
                }));
            }
        }

        if locator.backend_class == "lvm" || locator.backend_class == "block" {
            if let Err(e) = self.check_device_allowlist(&locator.locator) {
                return Ok(Response::new(proto::OpenVolumeResponse {
                    result: Some(e.to_proto_result()),
                    volume_id: req.volume_id,
                    attachment_handle: String::new(),
                    export_kind: String::new(),
                    export_path: String::new(),
                }));
            }
        }

        let policy = Self::map_device_policy(req.policy);

        // Idempotency: if already open with same volume+path, return existing
        let precompute_path = if std::path::Path::new(&locator.locator).is_absolute() {
            locator.locator.clone()
        } else {
            // We don't know runtime_dir here; backend handles resolution.
            // For local backend idempotency we rely on the backend trait eventually.
            // As a best-effort shortcut, skip pre-check for relative locators.
            String::new()
        };
        if !precompute_path.is_empty() {
            if let Some(s) = self
                .sessions
                .find_by_volume_and_path(&req.volume_id, &precompute_path)
            {
                if !s.export_path.is_empty() {
                    return Ok(Response::new(proto::OpenVolumeResponse {
                        result: Some(Self::ok_result()),
                        volume_id: s.volume_id,
                        attachment_handle: s.attachment_handle,
                        export_kind: s.export_kind,
                        export_path: s.export_path,
                    }));
                }
            }
        }

        let export = match self.backend.open(&req.volume_id, &locator, &policy).await {
            Ok(e) => e,
            Err(e) => {
                return Ok(Response::new(proto::OpenVolumeResponse {
                    result: Some(e.to_proto_result()),
                    volume_id: req.volume_id,
                    attachment_handle: String::new(),
                    export_kind: String::new(),
                    export_path: String::new(),
                }));
            }
        };

        // Post-open idempotency: same handle may already exist
        if let Some(s) = self.sessions.get(&req.volume_id, &export.attachment_handle) {
            if !s.export_path.is_empty() {
                return Ok(Response::new(proto::OpenVolumeResponse {
                    result: Some(Self::ok_result()),
                    volume_id: s.volume_id,
                    attachment_handle: s.attachment_handle,
                    export_kind: s.export_kind,
                    export_path: s.export_path,
                }));
            }
        }

        // Enable dirty tracking for the freshly opened volume so that disk
        // migration dirty-sync rounds can always snapshot a bitmap (empty
        // when nothing was written yet). Read-only opens are skipped: a
        // read-only volume cannot be dirtied, so the bitmap would only
        // waste memory. Failures must not fail the open: the volume remains
        // usable, only dirty tracking is degraded.
        if !policy.read_only {
            match self
                .backend
                .volume_size(&req.volume_id, &export.attachment_handle)
                .await
            {
                Ok(size_bytes) => {
                    if let Err(e) = self
                        .backend
                        .enable_dirty_tracking(
                            &req.volume_id,
                            &export.attachment_handle,
                            size_bytes,
                        )
                        .await
                    {
                        tracing::warn!(
                            volume_id = %req.volume_id,
                            handle = %export.attachment_handle,
                            error = %e,
                            "failed to enable dirty tracking for opened volume"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        volume_id = %req.volume_id,
                        handle = %export.attachment_handle,
                        error = %e,
                        "could not determine volume size; dirty tracking not enabled"
                    );
                }
            }
        }

        let session = Session {
            volume_id: req.volume_id.clone(),
            vm_id: None,
            attachment_handle: export.attachment_handle.clone(),
            export_kind: export.export_kind.clone(),
            export_path: export.export_path.clone(),
            runtime_status: "open".to_string(),
        };
        self.sessions.upsert(session.clone());
        self.persist_upsert(&session).await;

        Ok(Response::new(proto::OpenVolumeResponse {
            result: Some(Self::ok_result()),
            volume_id: req.volume_id,
            attachment_handle: export.attachment_handle,
            export_kind: export.export_kind,
            export_path: export.export_path,
        }))
    }

    async fn close_volume(
        &self,
        request: Request<proto::CloseVolumeRequest>,
    ) -> Result<Response<proto::Result>, Status> {
        self.metrics.increment_counter("stord_close_volume_total");
        let req = request.into_inner();
        let _span = req
            .meta
            .as_ref()
            .map(|m| operation_span(&m.operation_id))
            .unwrap_or_else(|| operation_span(""));

        if let Some(s) = self.sessions.get(&req.volume_id, &req.attachment_handle) {
            if let Err(e) = self.backend.close(&s.volume_id, &s.attachment_handle).await {
                return Ok(Response::new(e.to_proto_result()));
            }
            self.sessions.remove(&req.volume_id, &req.attachment_handle);
            self.persist_remove(&req.volume_id, &req.attachment_handle)
                .await;
        }

        Ok(Response::new(Self::ok_result()))
    }

    async fn get_volume_health(
        &self,
        request: Request<proto::VolumeHealthRequest>,
    ) -> Result<Response<proto::VolumeHealthResponse>, Status> {
        let req = request.into_inner();

        let sessions = self.sessions.list();
        let session = sessions.into_iter().find(|s| s.volume_id == req.volume_id);

        let (status, backend_state, last_error) = if let Some(s) = session {
            match self
                .backend
                .health(&s.volume_id, &s.attachment_handle)
                .await
            {
                Ok(h) => (h.status, h.backend_state, h.last_error),
                Err(e) => ("unhealthy".to_string(), "error".to_string(), e.to_string()),
            }
        } else {
            ("unknown".to_string(), "closed".to_string(), String::new())
        };

        Ok(Response::new(proto::VolumeHealthResponse {
            result: Some(Self::ok_result()),
            volume_id: req.volume_id,
            health_status: status,
            backend_state,
            last_error,
        }))
    }

    async fn list_volume_sessions(
        &self,
        _request: Request<proto::ListVolumeSessionsRequest>,
    ) -> Result<Response<proto::ListVolumeSessionsResponse>, Status> {
        let sessions: Vec<proto::VolumeSession> = self
            .sessions
            .list()
            .into_iter()
            .map(|s| proto::VolumeSession {
                volume_id: s.volume_id,
                vm_id: s.vm_id.unwrap_or_default(),
                attachment_handle: s.attachment_handle,
                export_kind: s.export_kind,
                export_path: s.export_path,
                runtime_status: s.runtime_status,
            })
            .collect();

        Ok(Response::new(proto::ListVolumeSessionsResponse {
            sessions,
        }))
    }

    async fn attach_volume_to_vm(
        &self,
        request: Request<proto::AttachVolumeToVmRequest>,
    ) -> Result<Response<proto::AttachVolumeToVmResponse>, Status> {
        self.metrics.increment_counter("stord_attach_volume_total");
        let req = request.into_inner();
        let _span = req
            .meta
            .as_ref()
            .map(|m| operation_span(&m.operation_id))
            .unwrap_or_else(|| operation_span(""));

        if self
            .sessions
            .get(&req.volume_id, &req.attachment_handle)
            .is_none()
        {
            let e = ChvError::NotFound {
                resource: "session".to_string(),
                id: format!("{}/{}", req.volume_id, req.attachment_handle),
            };
            return Ok(Response::new(proto::AttachVolumeToVmResponse {
                result: Some(e.to_proto_result()),
                volume_id: req.volume_id,
                vm_id: req.vm_id,
                export_kind: String::new(),
                export_path: String::new(),
            }));
        }

        let export = match self
            .backend
            .attach(&req.volume_id, &req.attachment_handle, &req.vm_id)
            .await
        {
            Ok(e) => e,
            Err(e) => {
                return Ok(Response::new(proto::AttachVolumeToVmResponse {
                    result: Some(e.to_proto_result()),
                    volume_id: req.volume_id,
                    vm_id: req.vm_id,
                    export_kind: String::new(),
                    export_path: String::new(),
                }));
            }
        };

        let updated = self.sessions.update_vm_id(
            &req.volume_id,
            &req.attachment_handle,
            Some(req.vm_id.clone()),
            "attached".to_string(),
        );

        if !updated {
            if let Err(e) = self
                .backend
                .detach(
                    &req.volume_id,
                    &req.attachment_handle,
                    chv_common::AttachmentOwnership {
                        vm_id: req.vm_id.clone(),
                        operation_id: req.meta.as_ref().map(|m| m.operation_id.clone()),
                        requester: None,
                    },
                    false,
                )
                .await
            {
                tracing::warn!(
                    volume_id = %req.volume_id,
                    attachment_handle = %req.attachment_handle,
                    vm_id = %req.vm_id,
                    error = %e,
                    "rollback detach failed after concurrent session removal"
                );
            }
            let e = ChvError::NotFound {
                resource: "session".to_string(),
                id: format!("{}/{}", req.volume_id, req.attachment_handle),
            };
            return Ok(Response::new(proto::AttachVolumeToVmResponse {
                result: Some(e.to_proto_result()),
                volume_id: req.volume_id,
                vm_id: req.vm_id,
                export_kind: String::new(),
                export_path: String::new(),
            }));
        }

        if let Some(session) = self.sessions.get(&req.volume_id, &req.attachment_handle) {
            self.persist_upsert(&session).await;
        }

        Ok(Response::new(proto::AttachVolumeToVmResponse {
            result: Some(Self::ok_result()),
            volume_id: req.volume_id,
            vm_id: req.vm_id,
            export_kind: export.export_kind,
            export_path: export.export_path,
        }))
    }

    async fn detach_volume_from_vm(
        &self,
        request: Request<proto::DetachVolumeFromVmRequest>,
    ) -> Result<Response<proto::Result>, Status> {
        self.metrics.increment_counter("stord_detach_volume_total");
        let req = request.into_inner();
        let _span = req
            .meta
            .as_ref()
            .map(|m| operation_span(&m.operation_id))
            .unwrap_or_else(|| operation_span(""));

        // TODO: add a secondary index or find_by_volume_and_vm method if this becomes a hot path.
        let sessions = self.sessions.list();
        let session = sessions
            .into_iter()
            .find(|s| s.volume_id == req.volume_id && s.vm_id.as_deref() == Some(&req.vm_id));

        if let Some(s) = session {
            if let Err(e) = self
                .backend
                .detach(
                    &req.volume_id,
                    &s.attachment_handle,
                    chv_common::AttachmentOwnership {
                        vm_id: req.vm_id.clone(),
                        operation_id: req.meta.as_ref().map(|m| m.operation_id.clone()),
                        requester: None,
                    },
                    req.force,
                )
                .await
            {
                if !req.force {
                    return Ok(Response::new(e.to_proto_result()));
                }
                tracing::warn!(
                    volume_id = %req.volume_id,
                    vm_id = %req.vm_id,
                    error = %e,
                    "force detach swallowed backend error"
                );
            }

            let updated = self.sessions.update_vm_id(
                &req.volume_id,
                &s.attachment_handle,
                None,
                // Distinct from `open`: an opened-but-attach-pending session
                // is also `vm_id = None`, and the agent's post-delete sweep
                // must never close that live session (#711 review).
                "detached".to_string(),
            );

            if !updated {
                tracing::warn!(
                    volume_id = %req.volume_id,
                    vm_id = %req.vm_id,
                    "concurrent session removal detected during detach"
                );
            }

            if let Some(session) = self.sessions.get(&req.volume_id, &s.attachment_handle) {
                self.persist_upsert(&session).await;
            }
        }

        Ok(Response::new(Self::ok_result()))
    }

    async fn resize_volume(
        &self,
        request: Request<proto::ResizeVolumeRequest>,
    ) -> Result<Response<proto::Result>, Status> {
        self.metrics.increment_counter("stord_resize_volume_total");
        let req = request.into_inner();
        let _span = req
            .meta
            .as_ref()
            .map(|m| operation_span(&m.operation_id))
            .unwrap_or_else(|| operation_span(""));

        if req.new_size_bytes == 0 {
            return Ok(Response::new(
                ChvError::InvalidArgument {
                    field: "new_size_bytes".to_string(),
                    reason: "new_size_bytes must be > 0".to_string(),
                }
                .to_proto_result(),
            ));
        }

        let sessions = self.sessions.list();
        let session = sessions.into_iter().find(|s| s.volume_id == req.volume_id);

        let Some(s) = session else {
            let e = ChvError::NotFound {
                resource: "session".to_string(),
                id: req.volume_id.clone(),
            };
            return Ok(Response::new(e.to_proto_result()));
        };

        if let Err(e) = self
            .backend
            .resize(&s.volume_id, &s.attachment_handle, req.new_size_bytes)
            .await
        {
            return Ok(Response::new(e.to_proto_result()));
        }

        Ok(Response::new(Self::ok_result()))
    }

    async fn prepare_snapshot(
        &self,
        request: Request<proto::PrepareSnapshotRequest>,
    ) -> Result<Response<proto::Result>, Status> {
        self.metrics
            .increment_counter("stord_prepare_snapshot_total");
        let req = request.into_inner();
        let _span = req
            .meta
            .as_ref()
            .map(|m| operation_span(&m.operation_id))
            .unwrap_or_else(|| operation_span(""));

        let sessions = self.sessions.list();
        let session = sessions.into_iter().find(|s| s.volume_id == req.volume_id);

        let Some(s) = session else {
            let e = ChvError::NotFound {
                resource: "session".to_string(),
                id: req.volume_id.clone(),
            };
            return Ok(Response::new(e.to_proto_result()));
        };

        if let Err(e) = Self::validate_snapshot_or_clone_name(&req.snapshot_name, "snapshot_name") {
            return Ok(Response::new(e.to_proto_result()));
        }

        if let Err(e) = self
            .backend
            .prepare_snapshot(
                &s.volume_id,
                &s.attachment_handle,
                chv_common::AttachmentOwnership {
                    vm_id: s.vm_id.clone().unwrap_or_default(),
                    operation_id: req.meta.as_ref().map(|m| m.operation_id.clone()),
                    requester: None,
                },
                &req.snapshot_name,
            )
            .await
        {
            return Ok(Response::new(e.to_proto_result()));
        }

        Ok(Response::new(Self::ok_result()))
    }

    async fn prepare_clone(
        &self,
        request: Request<proto::PrepareCloneRequest>,
    ) -> Result<Response<proto::Result>, Status> {
        self.metrics.increment_counter("stord_prepare_clone_total");
        let req = request.into_inner();
        let _span = req
            .meta
            .as_ref()
            .map(|m| operation_span(&m.operation_id))
            .unwrap_or_else(|| operation_span(""));

        let sessions = self.sessions.list();
        let session = sessions.into_iter().find(|s| s.volume_id == req.volume_id);

        let Some(s) = session else {
            let e = ChvError::NotFound {
                resource: "session".to_string(),
                id: req.volume_id.clone(),
            };
            return Ok(Response::new(e.to_proto_result()));
        };

        if let Err(e) = Self::validate_snapshot_or_clone_name(&req.clone_name, "clone_name") {
            return Ok(Response::new(e.to_proto_result()));
        }

        if let Err(e) = self
            .backend
            .prepare_clone(
                &s.volume_id,
                &s.attachment_handle,
                chv_common::AttachmentOwnership {
                    vm_id: s.vm_id.clone().unwrap_or_default(),
                    operation_id: req.meta.as_ref().map(|m| m.operation_id.clone()),
                    requester: None,
                },
                &req.clone_name,
            )
            .await
        {
            return Ok(Response::new(e.to_proto_result()));
        }

        Ok(Response::new(Self::ok_result()))
    }

    async fn restore_snapshot(
        &self,
        request: Request<proto::RestoreSnapshotRequest>,
    ) -> Result<Response<proto::Result>, Status> {
        self.metrics
            .increment_counter("stord_restore_snapshot_total");
        let req = request.into_inner();
        let _span = req
            .meta
            .as_ref()
            .map(|m| operation_span(&m.operation_id))
            .unwrap_or_else(|| operation_span(""));

        let sessions = self.sessions.list();
        let session = sessions.into_iter().find(|s| s.volume_id == req.volume_id);

        let Some(s) = session else {
            let e = ChvError::NotFound {
                resource: "session".to_string(),
                id: req.volume_id.clone(),
            };
            return Ok(Response::new(e.to_proto_result()));
        };

        if let Err(e) = Self::validate_snapshot_or_clone_name(&req.snapshot_name, "snapshot_name") {
            return Ok(Response::new(e.to_proto_result()));
        }

        if let Err(e) = self
            .backend
            .restore_snapshot(&s.volume_id, &s.attachment_handle, &req.snapshot_name)
            .await
        {
            return Ok(Response::new(e.to_proto_result()));
        }

        Ok(Response::new(Self::ok_result()))
    }

    async fn delete_snapshot(
        &self,
        request: Request<proto::DeleteSnapshotRequest>,
    ) -> Result<Response<proto::Result>, Status> {
        self.metrics
            .increment_counter("stord_delete_snapshot_total");
        let req = request.into_inner();
        let _span = req
            .meta
            .as_ref()
            .map(|m| operation_span(&m.operation_id))
            .unwrap_or_else(|| operation_span(""));

        let sessions = self.sessions.list();
        let session = sessions.into_iter().find(|s| s.volume_id == req.volume_id);

        let Some(s) = session else {
            let e = ChvError::NotFound {
                resource: "session".to_string(),
                id: req.volume_id.clone(),
            };
            return Ok(Response::new(e.to_proto_result()));
        };

        if let Err(e) = Self::validate_snapshot_or_clone_name(&req.snapshot_name, "snapshot_name") {
            return Ok(Response::new(e.to_proto_result()));
        }

        if let Err(e) = self
            .backend
            .delete_snapshot(&s.volume_id, &s.attachment_handle, &req.snapshot_name)
            .await
        {
            return Ok(Response::new(e.to_proto_result()));
        }

        Ok(Response::new(Self::ok_result()))
    }

    /// #522 (DP3): the stord destroy primitive's API half — the first
    /// data-destroying RPC in this service. Session-checked and
    /// idempotent: the handler REFUSES while any open session exists
    /// for the volume (the agent's `DeleteVolume` node RPC is the
    /// cooperative close-then-destroy caller; this is the defense for
    /// every other caller), then delegates to
    /// [`StorageBackend::destroy`], whose contract makes an
    /// already-gone backing store a SUCCESS (crash-redrive and
    /// operator retry must be able to replay a delete — PR 2's
    /// accept-time story depends on it). The allowlist checks mirror
    /// `open_volume`'s exactly: a removal path deserves the same
    /// API-boundary hardening a write path gets, if not more.
    async fn destroy_volume(
        &self,
        request: Request<proto::DestroyVolumeRequest>,
    ) -> Result<Response<proto::Result>, Status> {
        self.metrics.increment_counter("stord_destroy_volume_total");
        let req = request.into_inner();
        let _span = req
            .meta
            .as_ref()
            .map(|m| operation_span(&m.operation_id))
            .unwrap_or_else(|| operation_span(""));

        if req.volume_id.is_empty() {
            return Ok(Response::new(
                ChvError::InvalidArgument {
                    field: "volume_id".to_string(),
                    reason: "volume_id must not be empty".to_string(),
                }
                .to_proto_result(),
            ));
        }

        // Session refusal (DP3): a volume with an open stord session is
        // not destroyable — a raw destroy under a live session would
        // strand the persisted session row and yank the backing store
        // out from under an attached VM. Close first.
        if self
            .sessions
            .list()
            .iter()
            .any(|s| s.volume_id == req.volume_id)
        {
            return Ok(Response::new(
                ChvError::InvalidArgument {
                    field: "volume_id".to_string(),
                    reason: format!(
                        "an open session exists for volume {}; close it before destroying",
                        req.volume_id
                    ),
                }
                .to_proto_result(),
            ));
        }

        let locator = match Self::map_backend_locator(req.backend) {
            Ok(l) => l,
            Err(e) => return Ok(Response::new(e.to_proto_result())),
        };

        if let Err(e) = self.check_allowlist(&locator.backend_class) {
            return Ok(Response::new(e.to_proto_result()));
        }

        // Same class-conditional allowlist split as open_volume: the
        // path allowlist only means anything for filesystem locators,
        // the device allowlist for lvm/block ones.
        if matches!(
            locator.backend_class.as_str(),
            "local" | "local-file" | "localdisk"
        ) {
            if let Err(e) = self.check_path_allowlist(&locator.locator) {
                return Ok(Response::new(e.to_proto_result()));
            }
        }
        if locator.backend_class == "lvm" || locator.backend_class == "block" {
            if let Err(e) = self.check_device_allowlist(&locator.locator) {
                return Ok(Response::new(e.to_proto_result()));
            }
        }

        if let Err(e) = self.backend.destroy(&req.volume_id, &locator).await {
            return Ok(Response::new(e.to_proto_result()));
        }

        Ok(Response::new(Self::ok_result()))
    }

    async fn set_device_policy(
        &self,
        request: Request<proto::SetDevicePolicyRequest>,
    ) -> Result<Response<proto::Result>, Status> {
        self.metrics
            .increment_counter("stord_set_device_policy_total");
        let req = request.into_inner();
        let _span = req
            .meta
            .as_ref()
            .map(|m| operation_span(&m.operation_id))
            .unwrap_or_else(|| operation_span(""));

        let sessions = self.sessions.list();
        let session = sessions.into_iter().find(|s| s.volume_id == req.volume_id);

        let Some(s) = session else {
            let e = ChvError::NotFound {
                resource: "session".to_string(),
                id: req.volume_id.clone(),
            };
            return Ok(Response::new(e.to_proto_result()));
        };

        let policy = Self::map_device_policy(req.policy);

        if let Err(e) = self
            .backend
            .set_device_policy(&s.volume_id, &s.attachment_handle, &policy)
            .await
        {
            return Ok(Response::new(e.to_proto_result()));
        }

        Ok(Response::new(Self::ok_result()))
    }

    async fn trigger_disk_migration(
        &self,
        request: Request<proto::TriggerDiskMigrationRequest>,
    ) -> Result<Response<proto::TriggerDiskMigrationResponse>, Status> {
        self.metrics
            .increment_counter("stord_trigger_disk_migration_total");
        let req = request.into_inner();
        let _span = req
            .meta
            .as_ref()
            .map(|m| operation_span(&m.operation_id))
            .unwrap_or_else(|| operation_span(""));

        if req.volume_id.is_empty() {
            return Ok(Response::new(proto::TriggerDiskMigrationResponse {
                result: Some(
                    ChvError::InvalidArgument {
                        field: "volume_id".to_string(),
                        reason: "volume_id must not be empty".to_string(),
                    }
                    .to_proto_result(),
                ),
                migration_id: String::new(),
                // No migration started — no mode to echo.
                pause_first: false,
            }));
        }

        if let Err(e) = self.check_migration_dest_allowlist(&req.dest_endpoint) {
            return Ok(Response::new(proto::TriggerDiskMigrationResponse {
                result: Some(e.to_proto_result()),
                migration_id: String::new(),
                // No migration started — no mode to echo.
                pause_first: false,
            }));
        }

        // Find the session for this volume+handle
        let session = self.sessions.get(&req.volume_id, &req.attachment_handle);
        let Some(session) = session else {
            return Ok(Response::new(proto::TriggerDiskMigrationResponse {
                result: Some(
                    ChvError::NotFound {
                        resource: "session".to_string(),
                        id: format!("{}/{}", req.volume_id, req.attachment_handle),
                    }
                    .to_proto_result(),
                ),
                migration_id: String::new(),
                // No migration started — no mode to echo.
                pause_first: false,
            }));
        };

        // Verify volume health before starting migration
        match self
            .backend
            .health(&session.volume_id, &session.attachment_handle)
            .await
        {
            Ok(h) if h.status == "error" => {
                return Ok(Response::new(proto::TriggerDiskMigrationResponse {
                    result: Some(
                        ChvError::BackendUnavailable {
                            backend: "stord".to_string(),
                            reason: format!(
                                "volume {} is unhealthy: {}",
                                req.volume_id, h.last_error
                            ),
                        }
                        .to_proto_result(),
                    ),
                    migration_id: String::new(),
                    // No migration started — no mode to echo.
                    pause_first: false,
                }));
            }
            Err(e) => {
                return Ok(Response::new(proto::TriggerDiskMigrationResponse {
                    result: Some(e.to_proto_result()),
                    migration_id: String::new(),
                    // No migration started — no mode to echo.
                    pause_first: false,
                }));
            }
            _ => {}
        }

        let migration_id = format!(
            "dm-{}-{}",
            req.volume_id,
            uuid::Uuid::new_v4()
                .to_string()
                .split('-')
                .next()
                .unwrap_or("x")
        );

        // The returned pause receiver is deliberately kept alive inside
        // the spawned sender future (see below): a tokio watch channel
        // with zero receivers is closed, and `pause_tx.send` from
        // `ResumeDiskMigration` would fail while the sender has not yet
        // reached its pause gate — the multi-volume staggered case
        // (issue #394 Option C review: the agent pauses and resumes ALL
        // volumes when the FIRST one requests the pause; a sibling still
        // connecting would abort the whole migration, and without a
        // latched value it would later deadlock at its own gate).
        // Holding one receiver open keeps `send` succeeding and the
        // value latched, so a sender that subscribes late sees the pause
        // already signaled and proceeds immediately.
        //
        // The guard drops when the sender future exits — by which point
        // the task is terminal (every non-panic exit sets
        // Completed/Failed before returning; a panicking future skips
        // the marking and leaves the pre-fix behavior: a loud resume
        // error) — and `resume_disk_migration` treats a failed send on
        // a terminal task as a benign no-op (#582).
        let (task, pause_rx) = MigrationTask::new(
            req.volume_id.clone(),
            req.attachment_handle.clone(),
            req.dest_endpoint.clone(),
        );

        // Update initial total_bytes from volume size
        let volume_size = match self
            .backend
            .volume_size(&req.volume_id, &req.attachment_handle)
            .await
        {
            Ok(size) => size,
            Err(e) => {
                return Ok(Response::new(proto::TriggerDiskMigrationResponse {
                    result: Some(e.to_proto_result()),
                    migration_id: String::new(),
                    // No migration started — no mode to echo.
                    pause_first: false,
                }));
            }
        };
        {
            let mut state = task.state.write().await;
            state.total_bytes = volume_size;
        }

        // Issue #663 MEDIUM-2: best-effort source-side cross-check. The
        // destination is the authority (it verifies the HMAC), but decoding
        // the already-minted token here catches a size mismatch before we
        // open the peer stream and lets the operator see both numbers. The
        // claims are NOT trusted — this is diagnostics only.
        if !req.migration_authorization.is_empty() {
            match MigrationTokenCodec::decode_claims_unverified(&req.migration_authorization) {
                Ok(claims) if claims.size_bytes != volume_size => {
                    return Ok(Response::new(proto::TriggerDiskMigrationResponse {
                        result: Some(
                            ChvError::InvalidArgument {
                                field: "migration_authorization".to_string(),
                                reason: format!(
                                    "size mismatch for volume {}: token authorizes {} bytes, \
                                     source backend reports {} bytes; refusing to start a \
                                     migration the destination would reject",
                                    req.volume_id, claims.size_bytes, volume_size
                                ),
                            }
                            .to_proto_result(),
                        ),
                        migration_id: String::new(),
                        pause_first: false,
                    }));
                }
                Ok(claims) if claims.volume_id != req.volume_id => {
                    return Ok(Response::new(proto::TriggerDiskMigrationResponse {
                        result: Some(
                            ChvError::InvalidArgument {
                                field: "migration_authorization".to_string(),
                                reason: format!(
                                    "volume mismatch: token authorizes {:?}, trigger is for {:?}",
                                    claims.volume_id, req.volume_id
                                ),
                            }
                            .to_proto_result(),
                        ),
                        migration_id: String::new(),
                        pause_first: false,
                    }));
                }
                Ok(_) => {}
                Err(error) => {
                    // Malformed/older tokens are left for the destination's
                    // full verifier to reject with the authoritative error.
                    tracing::warn!(
                        volume_id = %req.volume_id,
                        error = %error,
                        "could not preflight-decode migration authorization; destination will verify"
                    );
                }
            }
        }

        // Ensure dirty tracking is enabled before the sender reads the
        // bitmap. This also covers sessions hydrated from the SQLite store
        // after a stord restart (their trackers are not recreated by open).
        if let Err(e) = self
            .backend
            .enable_dirty_tracking(&req.volume_id, &req.attachment_handle, volume_size)
            .await
        {
            tracing::warn!(
                volume_id = %req.volume_id,
                handle = %req.attachment_handle,
                error = %e,
                "failed to enable dirty tracking before migration"
            );
        }

        self.migration_tasks
            .insert(migration_id.clone(), task.clone());

        let backend = self.backend.clone();
        let tls_config = self.migration_tls.clone();
        let endpoint = req.dest_endpoint.clone();
        let volume_id = req.volume_id.clone();
        let handle = req.attachment_handle.clone();
        let migration_authorization = req.migration_authorization.clone();
        let tasks = self.migration_tasks.clone();
        let mig_id = migration_id.clone();
        // Issue #394 Option C: the opt-in stop-the-world mode threads from
        // the trigger request into the sender.
        let req_pause_first = req.pause_first;
        // Surface the destination's initial handshake/admission result to
        // the agent synchronously (issue #663 follow-up). A rejected
        // admission never consumes the token, so the agent can retry the
        // same trigger after capacity frees.
        let (handshake_tx, handshake_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            // Keep the pause channel open for this migration's lifetime
            // (see the comment at the `MigrationTask::new` call): the
            // sender subscribes to `pause_tx` only when it reaches a
            // pause gate, and a resume arriving before that must latch,
            // not fail.
            let _pause_channel_guard = pause_rx;

            tracing::info!(
                migration_id = %mig_id,
                volume_id = %volume_id,
                dest_endpoint = %endpoint,
                "starting disk migration background task"
            );

            let task_clone = task.clone();
            let result = start_migration_to_peer(
                endpoint,
                volume_id.clone(),
                handle,
                backend,
                tls_config,
                Some(task_clone),
                migration_authorization,
                req_pause_first,
                Some(handshake_tx),
                // Default bound (30 s) for the FinalizeAck wait; operators
                // can tune it in a later config PR.
                None,
            )
            .await;

            if let Err(ref e) = result {
                tracing::error!(
                    migration_id = %mig_id,
                    volume_id = %volume_id,
                    error = %e,
                    "disk migration background task failed"
                );
                task.mark_failed(e.to_string());
            } else {
                tracing::info!(
                    migration_id = %mig_id,
                    volume_id = %volume_id,
                    "disk migration background task completed"
                );
            }

            // Keep the task record for a short while so status queries can read it.
            // A future enhancement could expire old entries.
            let _ = tasks;
        });

        // Wait for the destination's initial handshake: either the
        // receiving volume was authorized, admitted, and created
        // (`MigrationReady`), or the destination rejected the stream. The
        // rejection is returned as the gRPC status so the agent's
        // `RESOURCE_EXHAUSTED` retry can fire. Anything after readiness
        // remains asynchronous through the migration task, as before.
        match tokio::time::timeout(self.trigger_handshake_timeout, handshake_rx).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(status))) => {
                tracing::warn!(
                    migration_id = %migration_id,
                    volume_id = %req.volume_id,
                    code = ?status.code(),
                    %status,
                    "destination rejected the migration before readiness; returning the \
                     status synchronously so the agent can retry when appropriate"
                );
                // The agent never saw this migration id; do not leave a
                // phantom task behind for status queries.
                self.migration_tasks.remove(&migration_id);
                return Err(status);
            }
            Ok(Err(_)) => {
                tracing::error!(
                    migration_id = %migration_id,
                    volume_id = %req.volume_id,
                    "migration task ended before reporting the destination handshake result"
                );
                self.migration_tasks.remove(&migration_id);
                return Err(Status::internal(
                    "migration task ended before reporting the destination handshake result",
                ));
            }
            Err(_) => {
                // Bounded wait elapsed. The destination may still be
                // creating a large receiving volume or the path may be
                // slow. Preserve the prior semantics: report the migration
                // as started and let the background task finish. A later
                // admission failure then surfaces only as a failed task,
                // not as a synchronous retryable status.
                tracing::warn!(
                    migration_id = %migration_id,
                    volume_id = %req.volume_id,
                    timeout_secs = self.trigger_handshake_timeout.as_secs_f64(),
                    "destination handshake did not complete within the trigger bound; \
                     returning success and leaving the migration in the background"
                );
            }
        }

        Ok(Response::new(proto::TriggerDiskMigrationResponse {
            result: Some(Self::ok_result()),
            migration_id,
            // Issue #582 (T=0 skew guard): echo the requested mode so
            // the agent can detect a version-skewed stord that would
            // otherwise silently ignore `pause_first` (proto3 drops
            // unknown request fields).
            pause_first: req_pause_first,
        }))
    }

    async fn get_disk_migration_status(
        &self,
        request: Request<proto::GetDiskMigrationStatusRequest>,
    ) -> Result<Response<proto::GetDiskMigrationStatusResponse>, Status> {
        let req = request.into_inner();
        let task = self.migration_tasks.get(&req.migration_id);

        let Some(task) = task else {
            return Ok(Response::new(proto::GetDiskMigrationStatusResponse {
                result: Some(
                    ChvError::NotFound {
                        resource: "migration".to_string(),
                        id: req.migration_id.clone(),
                    }
                    .to_proto_result(),
                ),
                phase: proto::get_disk_migration_status_response::Phase::Pending as i32,
                convergence_round: 0,
                dirty_blocks_remaining: 0,
                bytes_transferred: 0,
                total_bytes: 0,
                needs_vm_pause: false,
                error_message: "migration not found".to_string(),
            }));
        };

        let state = task.state.read().await;
        let phase = match state.phase {
            MigrationPhase::Pending => proto::get_disk_migration_status_response::Phase::Pending,
            MigrationPhase::BulkCopy => proto::get_disk_migration_status_response::Phase::BulkCopy,
            MigrationPhase::DirtySync => {
                proto::get_disk_migration_status_response::Phase::DirtySync
            }
            MigrationPhase::PausedPreCopy => {
                proto::get_disk_migration_status_response::Phase::PausedPreCopy
            }
            MigrationPhase::PausedFinalSync => {
                proto::get_disk_migration_status_response::Phase::PausedFinalSync
            }
            MigrationPhase::Completed => {
                proto::get_disk_migration_status_response::Phase::Completed
            }
            MigrationPhase::Failed => proto::get_disk_migration_status_response::Phase::Failed,
        };

        Ok(Response::new(proto::GetDiskMigrationStatusResponse {
            result: Some(Self::ok_result()),
            phase: phase as i32,
            convergence_round: state.convergence_round,
            dirty_blocks_remaining: state.dirty_blocks_remaining,
            bytes_transferred: state.bytes_transferred,
            total_bytes: state.total_bytes,
            needs_vm_pause: state.needs_vm_pause,
            error_message: state.error_message.clone(),
        }))
    }

    async fn resume_disk_migration(
        &self,
        request: Request<proto::ResumeDiskMigrationRequest>,
    ) -> Result<Response<proto::ResumeDiskMigrationResponse>, Status> {
        let req = request.into_inner();
        let task = self.migration_tasks.get(&req.migration_id);

        let Some(task) = task else {
            return Ok(Response::new(proto::ResumeDiskMigrationResponse {
                result: Some(
                    ChvError::NotFound {
                        resource: "migration".to_string(),
                        id: req.migration_id.clone(),
                    }
                    .to_proto_result(),
                ),
            }));
        };

        if req.vm_paused {
            if let Err(e) = task.pause_tx.send(true) {
                // The pause channel's receiver lives exactly as long as
                // the spawned sender future (`_pause_channel_guard`,
                // #578), and every non-panic exit of that future sets a
                // terminal phase before it returns — so a failed send
                // means the migration already finished. A resume racing
                // completion is a benign no-op, not an error (#582
                // review residue): erroring here made the agent's
                // resume-all abort a migration that had in fact
                // succeeded. (A panicking future skips the terminal
                // marking; that resume then errors, as before.)
                let state = task.state.read().await;
                if matches!(
                    state.phase,
                    MigrationPhase::Completed | MigrationPhase::Failed
                ) {
                    tracing::info!(
                        migration_id = %req.migration_id,
                        phase = ?state.phase,
                        "resume for terminal migration treated as a no-op"
                    );
                    return Ok(Response::new(proto::ResumeDiskMigrationResponse {
                        result: Some(Self::ok_result()),
                    }));
                }
                tracing::warn!(
                    migration_id = %req.migration_id,
                    error = %e,
                    "failed to send vm_paused signal to migration task"
                );
                return Ok(Response::new(proto::ResumeDiskMigrationResponse {
                    result: Some(
                        ChvError::Internal {
                            reason: format!("failed to resume migration: {e}"),
                        }
                        .to_proto_result(),
                    ),
                }));
            }
            tracing::info!(
                migration_id = %req.migration_id,
                "sent vm_paused signal to migration task"
            );
        }

        Ok(Response::new(proto::ResumeDiskMigrationResponse {
            result: Some(Self::ok_result()),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chv_stord_backends::LocalFileBackend;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn make_service(
        runtime_dir: &std::path::Path,
        allowlist: Vec<PathBuf>,
    ) -> StorageServiceImpl<LocalFileBackend> {
        let backend = Arc::new(LocalFileBackend::new(runtime_dir.to_path_buf()));
        StorageServiceImpl::new(
            backend,
            Arc::new(SessionTable::new()),
            Arc::new(Metrics::new()),
            runtime_dir.to_path_buf(),
            vec!["local".to_string()],
            allowlist,
            vec![],
            vec![],
            None,
        )
    }

    #[test]
    fn empty_allowlist_passes_everything() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path(), vec![]);
        // No allowlist configured: the check is a no-op (existing behavior).
        assert!(svc.check_path_allowlist("../../etc/passwd").is_ok());
        assert!(svc.check_path_allowlist("vol.img").is_ok());
    }

    #[test]
    fn relative_locator_resolving_outside_allowlist_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("allowed")).unwrap();
        std::fs::create_dir_all(dir.path().join("other")).unwrap();
        let svc = make_service(dir.path(), vec![dir.path().join("allowed")]);
        // Previously relative locators skipped the allowlist entirely; they
        // must now resolve against runtime_dir and be verified.
        assert!(svc.check_path_allowlist("other/vol.img").is_err());
    }

    #[test]
    fn relative_locator_resolving_inside_allowlist_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("allowed");
        std::fs::create_dir_all(&allowed).unwrap();
        let svc = make_service(dir.path(), vec![allowed]);
        assert!(svc.check_path_allowlist("allowed/vol.img").is_ok());
    }

    #[test]
    fn dot_dot_locator_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("allowed")).unwrap();
        let svc = make_service(dir.path(), vec![dir.path().to_path_buf()]);
        assert!(svc.check_path_allowlist("../escape.img").is_err());
        assert!(svc.check_path_allowlist("allowed/../escape.img").is_err());
        let abs = dir
            .path()
            .join("allowed/../escape.img")
            .to_string_lossy()
            .to_string();
        assert!(svc.check_path_allowlist(&abs).is_err());
    }

    #[test]
    fn dot_component_locator_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path(), vec![dir.path().to_path_buf()]);
        assert!(svc.check_path_allowlist("./vol.img").is_err());
        assert!(svc.check_path_allowlist("sub/./vol.img").is_err());
    }

    #[test]
    fn absolute_locator_inside_allowlist_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("allowed");
        std::fs::create_dir_all(&allowed).unwrap();
        let disk = allowed.join("disk.img");
        std::fs::write(&disk, b"disk").unwrap();
        let svc = make_service(dir.path(), vec![allowed]);
        assert!(svc.check_path_allowlist(disk.to_str().unwrap()).is_ok());
    }

    #[test]
    fn absolute_locator_outside_allowlist_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("allowed")).unwrap();
        std::fs::create_dir_all(dir.path().join("outside")).unwrap();
        let disk = dir.path().join("outside/disk.img");
        std::fs::write(&disk, b"disk").unwrap();
        let svc = make_service(dir.path(), vec![dir.path().join("allowed")]);
        assert!(svc.check_path_allowlist(disk.to_str().unwrap()).is_err());
    }

    #[test]
    fn symlink_escaping_allowlist_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("allowed");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, allowed.join("link")).unwrap();
        std::fs::write(outside.join("disk.img"), b"disk").unwrap();
        let svc = make_service(dir.path(), vec![allowed.clone()]);
        let locator = allowed.join("link/disk.img");
        assert!(svc.check_path_allowlist(locator.to_str().unwrap()).is_err());
    }

    #[test]
    fn missing_file_with_parent_inside_allowlist_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("allowed");
        std::fs::create_dir_all(&allowed).unwrap();
        let svc = make_service(dir.path(), vec![allowed.clone()]);
        // File does not exist yet; canonicalize must resolve the parent and
        // accept the not-yet-created volume file.
        let locator = allowed.join("new.img");
        assert!(svc.check_path_allowlist(locator.to_str().unwrap()).is_ok());
    }

    #[test]
    fn missing_file_with_missing_parent_inside_allowlist_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("allowed");
        std::fs::create_dir_all(&allowed).unwrap();
        let svc = make_service(dir.path(), vec![allowed.clone()]);
        // Neither the file nor its parent directory exist yet.
        let locator = allowed.join("sub/new.img");
        assert!(svc.check_path_allowlist(locator.to_str().unwrap()).is_ok());
    }

    #[test]
    fn nonexistent_allowlist_prefix_uses_ancestor_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("allowed").join("nested");
        // The allowlisted prefix itself does not exist yet; the check must
        // fall back to the canonical nearest ancestor and still accept.
        let svc = make_service(dir.path(), vec![allowed.clone()]);
        let locator = allowed.join("vol.img");
        assert!(svc.check_path_allowlist(locator.to_str().unwrap()).is_ok());
    }

    // ============================================================
    // #522 PR 1 (DP3) — the destroy primitive's API half
    // ============================================================

    fn destroy_request(volume_id: &str, locator: &str) -> proto::DestroyVolumeRequest {
        proto::DestroyVolumeRequest {
            meta: Some(proto::Meta {
                operation_id: "op-destroy".to_string(),
                request_unix_ms: 0,
            }),
            volume_id: volume_id.to_string(),
            backend: Some(proto::BackendLocator {
                backend_class: "local".to_string(),
                locator: locator.to_string(),
                options: Default::default(),
            }),
        }
    }

    fn result_fields(res: &proto::Result) -> (String, String, String) {
        (
            res.status.clone(),
            res.error_code.clone(),
            res.human_summary.clone(),
        )
    }

    #[tokio::test]
    async fn destroy_volume_removes_the_file_and_is_idempotent() {
        // #522 DP3: with no open session, the destroy removes the
        // backing file at the carrier locator and replays as a SUCCESS
        // on the absent artifact (the idempotency contract PR 2's
        // crash-redrive story depends on).
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path(), vec![dir.path().to_path_buf()]);
        let img = dir.path().join("vol-del.img");
        std::fs::write(&img, b"payload").unwrap();

        let resp = proto::storage_service_server::StorageService::destroy_volume(
            &svc,
            Request::new(destroy_request("vol-del", "vol-del.img")),
        )
        .await
        .unwrap();
        let (status, code, _) = result_fields(&resp.into_inner());
        assert!(
            status.eq_ignore_ascii_case("ok"),
            "first destroy: {status} {code}"
        );
        assert!(!img.exists(), "the backing file must be unlinked");

        let resp = proto::storage_service_server::StorageService::destroy_volume(
            &svc,
            Request::new(destroy_request("vol-del", "vol-del.img")),
        )
        .await
        .unwrap();
        let (status, code, summary) = result_fields(&resp.into_inner());
        assert!(
            status.eq_ignore_ascii_case("ok"),
            "replayed destroy must succeed: {status} {code} {summary}"
        );
        assert!(!img.exists());
    }

    #[tokio::test]
    async fn destroy_volume_refuses_while_a_session_is_open() {
        // #522 DP3: an open stord session blocks the destroy — the
        // agent is the cooperative close-then-destroy caller; stord is
        // the defense for every other caller. The backing file must
        // survive the refusal.
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path(), vec![dir.path().to_path_buf()]);
        let img = dir.path().join("vol-open.img");
        std::fs::write(&img, b"payload").unwrap();
        svc.sessions().upsert(Session {
            volume_id: "vol-open".to_string(),
            vm_id: None,
            attachment_handle: "local-vol-open-vol-open.img".to_string(),
            export_kind: "raw".to_string(),
            export_path: img.to_string_lossy().to_string(),
            runtime_status: "open".to_string(),
        });

        let resp = proto::storage_service_server::StorageService::destroy_volume(
            &svc,
            Request::new(destroy_request("vol-open", "vol-open.img")),
        )
        .await
        .unwrap();
        let inner = resp.into_inner();
        let (status, _, summary) = result_fields(&inner);
        assert_ne!(status, "ok", "an open session must refuse the destroy");
        assert!(
            summary.contains("close it before destroying"),
            "the refusal must name the close-first path: {summary}"
        );
        assert!(
            img.exists(),
            "the backing file must survive a refused destroy"
        );
    }

    #[tokio::test]
    async fn destroy_volume_rejects_a_locator_outside_the_path_allowlist() {
        // The removal path runs the same allowlist hardening the open
        // path runs — a traversal locator on a destroy is worse than
        // on a write.
        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("allowed");
        std::fs::create_dir_all(&allowed).unwrap();
        let svc = make_service(dir.path(), vec![allowed]);

        let resp = proto::storage_service_server::StorageService::destroy_volume(
            &svc,
            Request::new(destroy_request("vol-x", "../escape.img")),
        )
        .await
        .unwrap();
        let (status, _, _) = result_fields(&resp.into_inner());
        assert_ne!(status, "ok", "a traversal locator must be refused");
    }
}
