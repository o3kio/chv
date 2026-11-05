use chv_errors::ChvError;
use chv_nwd_api::chv_nwd_api::{
    network_service_client::NetworkServiceClient, AttachVmNicRequest, DeleteNetworkTopologyRequest,
    DetachVmNicRequest, DhcpScope, DnsScope, EnsureDhcpScopeRequest, EnsureDnsScopeRequest,
    EnsureNetworkTopologyRequest, ExposeServiceRequest, ListNamespaceStateRequest,
    NetworkHealthRequest, SendGratuitousArpRequest, SetFirewallPolicyRequest, SetNatPolicyRequest,
    UpdateOverlayRequest, WithdrawServiceExposureRequest,
};
use chv_stord_api::chv_stord_api::{
    storage_service_client::StorageServiceClient, AttachVolumeToVmRequest, CloseVolumeRequest,
    DeleteSnapshotRequest, DestroyVolumeRequest, DetachVolumeFromVmRequest, DevicePolicy,
    GetDiskMigrationStatusRequest, ListVolumeSessionsRequest, OpenVolumeRequest,
    PrepareCloneRequest, PrepareSnapshotRequest, ResizeVolumeRequest, RestoreSnapshotRequest,
    ResumeDiskMigrationRequest, SetDevicePolicyRequest, TriggerDiskMigrationRequest,
    VolumeHealthRequest,
};
use std::path::Path;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;
use tracing::Instrument;

fn with_operation_id<T>(req: T, operation_id: Option<&str>) -> tonic::Request<T> {
    let mut grpc_req = tonic::Request::new(req);
    if let Some(op_id) = operation_id {
        if let Ok(val) = tonic::metadata::MetadataValue::try_from(op_id) {
            grpc_req
                .metadata_mut()
                .insert(chv_common::OPERATION_ID_METADATA_KEY, val);
        }
    }
    grpc_req
}

pub struct StordClient {
    inner: StorageServiceClient<Channel>,
}

impl StordClient {
    pub async fn connect(socket_path: &Path) -> Result<Self, ChvError> {
        let path = socket_path.to_path_buf();
        let channel = Endpoint::try_from("http://[::]:50051")
            .map_err(|e| ChvError::InvalidArgument {
                field: "stord_socket".to_string(),
                reason: e.to_string(),
            })?
            .timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(5))
            .connect_with_connector(service_fn(move |_: Uri| {
                let p = path.clone();
                async move {
                    let stream = UnixStream::connect(p).await?;
                    Ok::<_, std::io::Error>(hyper_util::rt::tokio::TokioIo::new(stream))
                }
            }))
            .await
            .map_err(|e| ChvError::BackendUnavailable {
                backend: "stord".to_string(),
                reason: e.to_string(),
            })?;
        Ok(Self {
            inner: StorageServiceClient::new(channel),
        })
    }

    pub async fn health_probe(&mut self) -> Result<bool, ChvError> {
        let span = tracing::info_span!("stord_health_probe");
        let _ = self
            .inner
            .list_volume_sessions(ListVolumeSessionsRequest {})
            .instrument(span)
            .await
            .map_err(|e| ChvError::BackendUnavailable {
                backend: "stord".to_string(),
                reason: e.to_string(),
            })?;
        Ok(true)
    }

    pub async fn open_volume(
        &mut self,
        volume_id: &str,
        backend_class: &str,
        locator: &str,
        operation_id: Option<&str>,
    ) -> Result<(String, String, String), ChvError> {
        self.open_volume_with_options(
            volume_id,
            backend_class,
            locator,
            std::collections::HashMap::new(),
            operation_id,
        )
        .await
    }

    pub async fn open_volume_with_options(
        &mut self,
        volume_id: &str,
        backend_class: &str,
        locator: &str,
        mut options: std::collections::HashMap<String, String>,
        operation_id: Option<&str>,
    ) -> Result<(String, String, String), ChvError> {
        options
            .entry("volume_id".to_string())
            .or_insert_with(|| volume_id.to_string());
        let req = OpenVolumeRequest {
            meta: Some(chv_stord_api::chv_stord_api::Meta {
                operation_id: operation_id.unwrap_or("").to_string(),
                request_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            }),
            volume_id: volume_id.to_string(),
            backend: Some(chv_stord_api::chv_stord_api::BackendLocator {
                backend_class: backend_class.to_string(),
                locator: locator.to_string(),
                options,
            }),
            policy: None,
        };
        let span = tracing::info_span!("open_volume", operation_id = operation_id.unwrap_or(""));
        let resp = self
            .inner
            .open_volume(with_operation_id(req, operation_id))
            .instrument(span)
            .await
            .map_err(|e| ChvError::BackendUnavailable {
                backend: "stord".to_string(),
                reason: e.to_string(),
            })?
            .into_inner();
        if let Some(ref res) = resp.result {
            if !res.status.eq_ignore_ascii_case("ok")
                && !res.status.eq_ignore_ascii_case("0")
                && !res.status.is_empty()
            {
                return Err(ChvError::BackendUnavailable {
                    backend: "stord".to_string(),
                    reason: format!(
                        "stord open_volume failed (code {}): {}",
                        res.error_code, res.human_summary
                    ),
                });
            }
        }
        Ok((resp.volume_id, resp.attachment_handle, resp.export_path))
    }

    pub async fn attach_volume_to_vm(
        &mut self,
        volume_id: &str,
        vm_id: &str,
        attachment_handle: &str,
        operation_id: Option<&str>,
    ) -> Result<(String, String), ChvError> {
        let req = AttachVolumeToVmRequest {
            meta: Some(chv_stord_api::chv_stord_api::Meta {
                operation_id: operation_id.unwrap_or("").to_string(),
                request_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            }),
            volume_id: volume_id.to_string(),
            vm_id: vm_id.to_string(),
            attachment_handle: attachment_handle.to_string(),
        };
        let span = tracing::info_span!(
            "attach_volume_to_vm",
            operation_id = operation_id.unwrap_or("")
        );
        let resp = self
            .inner
            .attach_volume_to_vm(with_operation_id(req, operation_id))
            .instrument(span)
            .await
            .map_err(|e| ChvError::BackendUnavailable {
                backend: "stord".to_string(),
                reason: e.to_string(),
            })?
            .into_inner();
        Ok((resp.export_kind, resp.export_path))
    }

    pub async fn detach_volume_from_vm(
        &mut self,
        volume_id: &str,
        vm_id: &str,
        force: bool,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        let req = DetachVolumeFromVmRequest {
            meta: Some(chv_stord_api::chv_stord_api::Meta {
                operation_id: operation_id.unwrap_or("").to_string(),
                request_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            }),
            volume_id: volume_id.to_string(),
            vm_id: vm_id.to_string(),
            force,
        };
        let span = tracing::info_span!(
            "detach_volume_from_vm",
            operation_id = operation_id.unwrap_or("")
        );
        self.inner
            .detach_volume_from_vm(with_operation_id(req, operation_id))
            .instrument(span)
            .await
            .map_err(|e| ChvError::BackendUnavailable {
                backend: "stord".to_string(),
                reason: e.to_string(),
            })?;
        Ok(())
    }

    pub async fn close_volume(
        &mut self,
        volume_id: &str,
        attachment_handle: &str,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        let req = CloseVolumeRequest {
            meta: Some(chv_stord_api::chv_stord_api::Meta {
                operation_id: operation_id.unwrap_or("").to_string(),
                request_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            }),
            volume_id: volume_id.to_string(),
            attachment_handle: attachment_handle.to_string(),
        };
        let span = tracing::info_span!("close_volume", operation_id = operation_id.unwrap_or(""));
        self.inner
            .close_volume(with_operation_id(req, operation_id))
            .instrument(span)
            .await
            .map_err(|e| ChvError::BackendUnavailable {
                backend: "stord".to_string(),
                reason: e.to_string(),
            })?;
        Ok(())
    }

    /// Resolve the live attachment handle for `(volume_id, vm_id)` from
    /// stord's session table (#711).
    ///
    /// The agent cache's `volume_handles` map is in-memory and dies with
    /// the process, while stord's session table survives an agent restart.
    /// A post-restart delete therefore cannot close a pre-restart volume
    /// from the cache alone. This read-only lookup recovers the handle
    /// BEFORE the detach clears the session's `vm_id`, so the caller can
    /// detach and then close exactly like the live path.
    ///
    /// Returns `Ok(None)` when stord has no session for the pair (already
    /// detached/closed, or never attached). Transport/RPC failures surface
    /// as [`ChvError::BackendUnavailable`] so each caller keeps its
    /// existing failure policy (warn-and-continue for the best-effort
    /// delete drain, collected first-error for the legacy cleanup).
    pub async fn resolve_volume_handle(
        &mut self,
        volume_id: &str,
        vm_id: &str,
    ) -> Result<Option<String>, ChvError> {
        let span = tracing::info_span!("resolve_volume_handle", volume_id, vm_id);
        let sessions = self
            .inner
            .list_volume_sessions(tonic::Request::new(ListVolumeSessionsRequest {}))
            .instrument(span)
            .await
            .map_err(|e| ChvError::BackendUnavailable {
                backend: "stord".to_string(),
                reason: format!("list_volume_sessions for {volume_id}/{vm_id}: {e}"),
            })?
            .into_inner()
            .sessions;
        Ok(sessions
            .into_iter()
            .find(|session| {
                session.volume_id == volume_id
                    && !session.vm_id.is_empty()
                    && session.vm_id == vm_id
            })
            .map(|session| session.attachment_handle)
            .filter(|handle| !handle.is_empty()))
    }

    /// #522 PR 1 (DP3): dispatch the stord destroy primitive. The
    /// locator is the CREATE carrier's (DP4 — `{volume_id}.img` /
    /// `/dev/mapper/{vg}-{vid}`), and stord refuses while a session is
    /// open, so the cooperative caller closes first (the agent's
    /// `delete_volume` node-RPC handler does exactly that). Idempotent
    /// by stord's contract: an already-gone backing store is a
    /// success, never an error.
    pub async fn destroy_volume(
        &mut self,
        volume_id: &str,
        backend_class: &str,
        locator: &str,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        let req = DestroyVolumeRequest {
            meta: Some(chv_stord_api::chv_stord_api::Meta {
                operation_id: operation_id.unwrap_or("").to_string(),
                request_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            }),
            volume_id: volume_id.to_string(),
            backend: Some(chv_stord_api::chv_stord_api::BackendLocator {
                backend_class: backend_class.to_string(),
                locator: locator.to_string(),
                options: Default::default(),
            }),
        };
        let span = tracing::info_span!("destroy_volume", operation_id = operation_id.unwrap_or(""));
        let resp = self
            .inner
            .destroy_volume(with_operation_id(req, operation_id))
            .instrument(span)
            .await
            .map_err(|e| ChvError::BackendUnavailable {
                backend: "stord".to_string(),
                reason: e.to_string(),
            })?
            .into_inner();
        // The stord rpc returns the bare `Result` message (not a
        // wrapper): a non-ok status is the destroy's refusal (session
        // open, unimplemented class, allowlist) — surface it loudly.
        if !resp.status.eq_ignore_ascii_case("ok")
            && !resp.status.eq_ignore_ascii_case("0")
            && !resp.status.is_empty()
        {
            return Err(ChvError::BackendUnavailable {
                backend: "stord".to_string(),
                reason: format!(
                    "stord destroy_volume failed (code {}): {}",
                    resp.error_code, resp.human_summary
                ),
            });
        }
        Ok(())
    }

    pub async fn resize_volume(
        &mut self,
        volume_id: &str,
        new_size_bytes: u64,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        let req = ResizeVolumeRequest {
            meta: Some(chv_stord_api::chv_stord_api::Meta {
                operation_id: operation_id.unwrap_or("").to_string(),
                request_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            }),
            volume_id: volume_id.to_string(),
            new_size_bytes,
        };
        let span = tracing::info_span!("resize_volume", operation_id = operation_id.unwrap_or(""));
        self.inner
            .resize_volume(with_operation_id(req, operation_id))
            .instrument(span)
            .await
            .map_err(|e| ChvError::BackendUnavailable {
                backend: "stord".to_string(),
                reason: e.to_string(),
            })?;
        Ok(())
    }

    pub async fn get_volume_health(
        &mut self,
        volume_id: &str,
    ) -> Result<chv_stord_api::chv_stord_api::VolumeHealthResponse, ChvError> {
        let req = VolumeHealthRequest {
            volume_id: volume_id.to_string(),
        };
        let span = tracing::info_span!("get_volume_health");
        let resp = self
            .inner
            .get_volume_health(req)
            .instrument(span)
            .await
            .map_err(|e| ChvError::BackendUnavailable {
                backend: "stord".to_string(),
                reason: e.to_string(),
            })?
            .into_inner();
        Ok(resp)
    }

    pub async fn prepare_snapshot(
        &mut self,
        volume_id: &str,
        snapshot_name: &str,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        let req = PrepareSnapshotRequest {
            meta: Some(chv_stord_api::chv_stord_api::Meta {
                operation_id: operation_id.unwrap_or("").to_string(),
                request_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            }),
            volume_id: volume_id.to_string(),
            snapshot_name: snapshot_name.to_string(),
        };
        let span = tracing::info_span!(
            "prepare_snapshot",
            operation_id = operation_id.unwrap_or("")
        );
        self.inner
            .prepare_snapshot(with_operation_id(req, operation_id))
            .instrument(span)
            .await
            .map_err(|e| ChvError::BackendUnavailable {
                backend: "stord".to_string(),
                reason: e.to_string(),
            })?;
        Ok(())
    }

    pub async fn prepare_clone(
        &mut self,
        volume_id: &str,
        clone_name: &str,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        let req = PrepareCloneRequest {
            meta: Some(chv_stord_api::chv_stord_api::Meta {
                operation_id: operation_id.unwrap_or("").to_string(),
                request_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            }),
            volume_id: volume_id.to_string(),
            clone_name: clone_name.to_string(),
        };
        let span = tracing::info_span!("prepare_clone", operation_id = operation_id.unwrap_or(""));
        self.inner
            .prepare_clone(with_operation_id(req, operation_id))
            .instrument(span)
            .await
            .map_err(|e| ChvError::BackendUnavailable {
                backend: "stord".to_string(),
                reason: e.to_string(),
            })?;
        Ok(())
    }

    pub async fn restore_snapshot(
        &mut self,
        volume_id: &str,
        snapshot_name: &str,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        let req = RestoreSnapshotRequest {
            meta: Some(chv_stord_api::chv_stord_api::Meta {
                operation_id: operation_id.unwrap_or("").to_string(),
                request_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            }),
            volume_id: volume_id.to_string(),
            snapshot_name: snapshot_name.to_string(),
        };
        let span = tracing::info_span!(
            "restore_snapshot",
            operation_id = operation_id.unwrap_or("")
        );
        self.inner
            .restore_snapshot(with_operation_id(req, operation_id))
            .instrument(span)
            .await
            .map_err(|e| ChvError::BackendUnavailable {
                backend: "stord".to_string(),
                reason: e.to_string(),
            })?;
        Ok(())
    }

    pub async fn delete_snapshot(
        &mut self,
        volume_id: &str,
        snapshot_name: &str,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        let req = DeleteSnapshotRequest {
            meta: Some(chv_stord_api::chv_stord_api::Meta {
                operation_id: operation_id.unwrap_or("").to_string(),
                request_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            }),
            volume_id: volume_id.to_string(),
            snapshot_name: snapshot_name.to_string(),
        };
        let span =
            tracing::info_span!("delete_snapshot", operation_id = operation_id.unwrap_or(""));
        self.inner
            .delete_snapshot(with_operation_id(req, operation_id))
            .instrument(span)
            .await
            .map_err(|e| ChvError::BackendUnavailable {
                backend: "stord".to_string(),
                reason: e.to_string(),
            })?;
        Ok(())
    }

    /// Trigger disk pre-copy migration for a volume to a remote stord peer.
    ///
    /// Returns the migration_id assigned by the local stord.
    pub async fn trigger_disk_migration(
        &mut self,
        volume_id: &str,
        attachment_handle: &str,
        dest_endpoint: &str,
        migration_authorization: &str,
        pause_first: bool,
        operation_id: Option<&str>,
    ) -> Result<String, ChvError> {
        let req = TriggerDiskMigrationRequest {
            meta: Some(chv_stord_api::chv_stord_api::Meta {
                operation_id: operation_id.unwrap_or("").to_string(),
                request_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            }),
            volume_id: volume_id.to_string(),
            attachment_handle: attachment_handle.to_string(),
            dest_endpoint: dest_endpoint.to_string(),
            pause_first,
            migration_authorization: migration_authorization.to_string(),
        };
        let span = tracing::info_span!(
            "trigger_disk_migration",
            volume_id = %volume_id,
            dest_endpoint = %dest_endpoint,
            operation_id = operation_id.unwrap_or("")
        );
        let resp = self
            .inner
            .trigger_disk_migration(with_operation_id(req, operation_id))
            .instrument(span)
            .await
            .map_err(|e| match e.code() {
                // Destination admission is a transient, retryable capacity
                // signal. Keep it distinct from a transport/backend failure
                // so the migration loop can back off and retry the trigger
                // without consuming the (still unused) authorization token.
                tonic::Code::ResourceExhausted => ChvError::ResourceExhausted {
                    reason: e.message().to_string(),
                },
                _ => ChvError::BackendUnavailable {
                    backend: "stord".to_string(),
                    reason: e.to_string(),
                },
            })?
            .into_inner();
        if let Some(ref result) = resp.result {
            if !result.status.eq_ignore_ascii_case("ok") {
                return Err(ChvError::BackendUnavailable {
                    backend: "stord".to_string(),
                    reason: format!(
                        "trigger_disk_migration failed: {} ({})",
                        result.human_summary, result.error_code
                    ),
                });
            }
        }
        // Issue #582 (T=0 skew guard): an older stord predating the
        // `pause_first` request field drops it (proto3 unknown fields)
        // and silently runs the DEFAULT mode — the stop-the-world
        // mode the operator opted into is ignored, and the migration
        // proceeds live against a running VM. The response echoes the
        // mode the stord will actually run; a missing echo on a
        // pause-first request means version skew. Fail closed rather
        // than run a different mode than the one that was requested.
        if pause_first && !resp.pause_first {
            return Err(ChvError::BackendUnavailable {
                backend: "stord".to_string(),
                reason: "pause_first migration requested but stord did not \
                         echo the mode back — stord version skew (stord \
                         predates the pause-first contract); refusing to \
                         migrate without VM-pause coordination"
                    .to_string(),
            });
        }
        Ok(resp.migration_id)
    }

    /// Query the status of an active disk migration from the local stord.
    pub async fn get_disk_migration_status(
        &mut self,
        migration_id: &str,
    ) -> Result<chv_stord_api::chv_stord_api::GetDiskMigrationStatusResponse, ChvError> {
        let req = GetDiskMigrationStatusRequest {
            migration_id: migration_id.to_string(),
        };
        let span = tracing::info_span!("get_disk_migration_status", migration_id = %migration_id);
        let resp = self
            .inner
            .get_disk_migration_status(req)
            .instrument(span)
            .await
            .map_err(|e| ChvError::BackendUnavailable {
                backend: "stord".to_string(),
                reason: e.to_string(),
            })?
            .into_inner();
        Ok(resp)
    }

    /// Resume a disk migration after the VM has been paused (or unpaused).
    pub async fn resume_disk_migration(
        &mut self,
        migration_id: &str,
        vm_paused: bool,
    ) -> Result<(), ChvError> {
        let req = ResumeDiskMigrationRequest {
            migration_id: migration_id.to_string(),
            vm_paused,
        };
        let span = tracing::info_span!(
            "resume_disk_migration",
            migration_id = %migration_id,
            vm_paused = vm_paused
        );
        let resp = self
            .inner
            .resume_disk_migration(req)
            .instrument(span)
            .await
            .map_err(|e| ChvError::BackendUnavailable {
                backend: "stord".to_string(),
                reason: e.to_string(),
            })?
            .into_inner();
        if let Some(ref result) = resp.result {
            if !result.status.eq_ignore_ascii_case("ok") {
                return Err(ChvError::BackendUnavailable {
                    backend: "stord".to_string(),
                    reason: format!(
                        "resume_disk_migration failed: {} ({})",
                        result.human_summary, result.error_code
                    ),
                });
            }
        }
        Ok(())
    }

    pub async fn set_device_policy(
        &mut self,
        volume_id: &str,
        policy: DevicePolicy,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        let req = SetDevicePolicyRequest {
            meta: Some(chv_stord_api::chv_stord_api::Meta {
                operation_id: operation_id.unwrap_or("").to_string(),
                request_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            }),
            volume_id: volume_id.to_string(),
            policy: Some(policy),
        };
        let span = tracing::info_span!(
            "set_device_policy",
            operation_id = operation_id.unwrap_or("")
        );
        self.inner
            .set_device_policy(with_operation_id(req, operation_id))
            .instrument(span)
            .await
            .map_err(|e| ChvError::BackendUnavailable {
                backend: "stord".to_string(),
                reason: e.to_string(),
            })?;
        Ok(())
    }
}

pub struct NwdClient {
    inner: NetworkServiceClient<Channel>,
}

impl NwdClient {
    pub async fn connect(socket_path: &Path) -> Result<Self, ChvError> {
        let path = socket_path.to_path_buf();
        let channel = Endpoint::try_from("http://[::]:50051")
            .map_err(|e| ChvError::InvalidArgument {
                field: "nwd_socket".to_string(),
                reason: e.to_string(),
            })?
            .timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(5))
            .connect_with_connector(service_fn(move |_: Uri| {
                let p = path.clone();
                async move {
                    let stream = UnixStream::connect(p).await?;
                    Ok::<_, std::io::Error>(hyper_util::rt::tokio::TokioIo::new(stream))
                }
            }))
            .await
            .map_err(|e| ChvError::NetworkUnavailable {
                resource: "nwd".to_string(),
                reason: e.to_string(),
            })?;
        Ok(Self {
            inner: NetworkServiceClient::new(channel),
        })
    }

    pub async fn health_probe(&mut self) -> Result<bool, ChvError> {
        let span = tracing::info_span!("nwd_health_probe");
        let _ = self
            .inner
            .list_namespace_state(ListNamespaceStateRequest {})
            .instrument(span)
            .await
            .map_err(|e| ChvError::NetworkUnavailable {
                resource: "nwd".to_string(),
                reason: e.to_string(),
            })?;
        Ok(true)
    }

    pub async fn ensure_network_topology(
        &mut self,
        network_id: &str,
        bridge_name: &str,
        subnet_cidr: &str,
        gateway_ip: &str,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        let req = EnsureNetworkTopologyRequest {
            meta: Some(chv_nwd_api::chv_nwd_api::Meta {
                operation_id: operation_id.unwrap_or("").to_string(),
                request_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            }),
            topology: Some(chv_nwd_api::chv_nwd_api::TopologySpec {
                network_id: network_id.to_string(),
                tenant_id: "".to_string(),
                bridge_name: bridge_name.to_string(),
                namespace_name: format!("ns-{}", network_id),
                subnet_cidr: subnet_cidr.to_string(),
                gateway_ip: gateway_ip.to_string(),
                options: std::collections::HashMap::new(),
                vni: 0,
                vtep_endpoints: vec![],
                overlay_type: 0,
                fabric: None,
            }),
        };
        let span = tracing::info_span!(
            "ensure_network_topology",
            operation_id = operation_id.unwrap_or("")
        );
        let resp = self
            .inner
            .ensure_network_topology(with_operation_id(req, operation_id))
            .instrument(span)
            .await
            .map_err(|e| ChvError::NetworkUnavailable {
                resource: "nwd".to_string(),
                reason: e.to_string(),
            })?
            .into_inner();
        if !resp.status.eq_ignore_ascii_case("ok") {
            return Err(ChvError::NetworkUnavailable {
                resource: "nwd".to_string(),
                reason: format!(
                    "ensure_network_topology failed: {} ({})",
                    resp.human_summary, resp.error_code
                ),
            });
        }
        Ok(())
    }

    pub async fn attach_vm_nic(
        &mut self,
        nic_id: &str,
        vm_id: &str,
        network_id: &str,
        mac_address: &str,
        ip_address: &str,
        operation_id: Option<&str>,
    ) -> Result<(String, String), ChvError> {
        let req = AttachVmNicRequest {
            meta: Some(chv_nwd_api::chv_nwd_api::Meta {
                operation_id: operation_id.unwrap_or("").to_string(),
                request_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            }),
            nic: Some(chv_nwd_api::chv_nwd_api::NicSpec {
                nic_id: nic_id.to_string(),
                vm_id: vm_id.to_string(),
                network_id: network_id.to_string(),
                mac_address: mac_address.to_string(),
                tap_name: "".to_string(),
                ip_address: ip_address.to_string(),
            }),
        };
        let span = tracing::info_span!("attach_vm_nic", operation_id = operation_id.unwrap_or(""));
        let resp = self
            .inner
            .attach_vm_nic(with_operation_id(req, operation_id))
            .instrument(span)
            .await
            .map_err(|e| ChvError::NetworkUnavailable {
                resource: "nwd".to_string(),
                reason: e.to_string(),
            })?
            .into_inner();
        if let Some(ref result) = resp.result {
            if !result.status.eq_ignore_ascii_case("ok") {
                return Err(ChvError::NetworkUnavailable {
                    resource: "nwd".to_string(),
                    reason: format!(
                        "attach_vm_nic failed: {} ({})",
                        result.human_summary, result.error_code
                    ),
                });
            }
        }
        if resp.tap_handle.is_empty() {
            return Err(ChvError::NetworkUnavailable {
                resource: "nwd".to_string(),
                reason: "attach_vm_nic returned empty tap_handle".to_string(),
            });
        }
        Ok((resp.namespace_handle, resp.tap_handle))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn expose_service(
        &mut self,
        network_id: &str,
        exposure_id: &str,
        protocol: &str,
        external_port: u32,
        target_ip: &str,
        target_port: u32,
        mode: &str,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        let req = ExposeServiceRequest {
            meta: Some(chv_nwd_api::chv_nwd_api::Meta {
                operation_id: operation_id.unwrap_or("").to_string(),
                request_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            }),
            exposure: Some(chv_nwd_api::chv_nwd_api::ExposureSpec {
                network_id: network_id.to_string(),
                exposure_id: exposure_id.to_string(),
                protocol: protocol.to_string(),
                external_port,
                target_ip: target_ip.to_string(),
                target_port,
                mode: mode.to_string(),
            }),
        };
        let span = tracing::info_span!("expose_service", operation_id = operation_id.unwrap_or(""));
        self.inner
            .expose_service(with_operation_id(req, operation_id))
            .instrument(span)
            .await
            .map_err(|e| ChvError::NetworkUnavailable {
                resource: "nwd".to_string(),
                reason: e.to_string(),
            })?;
        Ok(())
    }

    pub async fn withdraw_service_exposure(
        &mut self,
        exposure_id: &str,
        network_id: &str,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        let req = WithdrawServiceExposureRequest {
            meta: Some(chv_nwd_api::chv_nwd_api::Meta {
                operation_id: operation_id.unwrap_or("").to_string(),
                request_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            }),
            exposure_id: exposure_id.to_string(),
            network_id: network_id.to_string(),
        };
        let span = tracing::info_span!(
            "withdraw_service_exposure",
            operation_id = operation_id.unwrap_or("")
        );
        self.inner
            .withdraw_service_exposure(with_operation_id(req, operation_id))
            .instrument(span)
            .await
            .map_err(|e| ChvError::NetworkUnavailable {
                resource: "nwd".to_string(),
                reason: e.to_string(),
            })?;
        Ok(())
    }

    pub async fn list_namespace_state(
        &mut self,
    ) -> Result<chv_nwd_api::chv_nwd_api::ListNamespaceStateResponse, ChvError> {
        let span = tracing::info_span!("list_namespace_state");
        let resp = self
            .inner
            .list_namespace_state(ListNamespaceStateRequest {})
            .instrument(span)
            .await
            .map_err(|e| ChvError::NetworkUnavailable {
                resource: "nwd".to_string(),
                reason: e.to_string(),
            })?
            .into_inner();
        Ok(resp)
    }

    pub async fn delete_network_topology(
        &mut self,
        network_id: &str,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        let req = DeleteNetworkTopologyRequest {
            meta: Some(chv_nwd_api::chv_nwd_api::Meta {
                operation_id: operation_id.unwrap_or("").to_string(),
                request_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            }),
            network_id: network_id.to_string(),
        };
        let span = tracing::info_span!(
            "delete_network_topology",
            operation_id = operation_id.unwrap_or("")
        );
        self.inner
            .delete_network_topology(with_operation_id(req, operation_id))
            .instrument(span)
            .await
            .map_err(|e| ChvError::NetworkUnavailable {
                resource: "nwd".to_string(),
                reason: e.to_string(),
            })?;
        Ok(())
    }

    pub async fn set_firewall_policy(
        &mut self,
        network_id: &str,
        policy_version: &str,
        policy_json: Vec<u8>,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        // Deliberately UNCHECKED (byte-identical to its pre-#355
        // behavior): nwd's semantic err_result is dropped here. The
        // attach path depends on this shape; the #355 carrier uses the
        // checked variant below.
        self.set_firewall_policy_response(network_id, policy_version, policy_json, operation_id)
            .await?;
        Ok(())
    }

    /// #355 (PR 1): the checked variant of [`set_firewall_policy`] —
    /// nwd reports SEMANTIC failures (unknown topology `NotFound`,
    /// engine/command failure, vocabulary rejection) as a gRPC `Ok`
    /// wrapping an `err_result`, so a caller that only maps the
    /// transport error acks a false success when the apply was
    /// refused. This variant inspects the inner result status, the
    /// `ensure_network_topology` discipline. The #355 policy carrier
    /// uses it; the attach path keeps the unchecked variant
    /// byte-identically (its swallow is a pre-existing, separately
    /// tracked gap — changing it is not this PR's zero-behavior-change
    /// contract).
    pub async fn set_firewall_policy_checked(
        &mut self,
        network_id: &str,
        policy_version: &str,
        policy_json: Vec<u8>,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        let resp = self
            .set_firewall_policy_response(network_id, policy_version, policy_json, operation_id)
            .await?
            .into_inner();
        if !resp.status.eq_ignore_ascii_case("ok") {
            return Err(ChvError::NetworkUnavailable {
                resource: "nwd".to_string(),
                reason: format!(
                    "set_firewall_policy failed: {} ({})",
                    resp.human_summary, resp.error_code
                ),
            });
        }
        Ok(())
    }

    /// The raw-response core both firewall-policy variants share.
    async fn set_firewall_policy_response(
        &mut self,
        network_id: &str,
        policy_version: &str,
        policy_json: Vec<u8>,
        operation_id: Option<&str>,
    ) -> Result<tonic::Response<chv_nwd_api::chv_nwd_api::Result>, ChvError> {
        let req = SetFirewallPolicyRequest {
            meta: Some(chv_nwd_api::chv_nwd_api::Meta {
                operation_id: operation_id.unwrap_or("").to_string(),
                request_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            }),
            network_id: network_id.to_string(),
            policy: Some(chv_nwd_api::chv_nwd_api::FirewallPolicy {
                policy_version: policy_version.to_string(),
                policy_json,
            }),
        };
        let span = tracing::info_span!(
            "set_firewall_policy",
            operation_id = operation_id.unwrap_or("")
        );
        self.inner
            .set_firewall_policy(with_operation_id(req, operation_id))
            .instrument(span)
            .await
            .map_err(|e| ChvError::NetworkUnavailable {
                resource: "nwd".to_string(),
                reason: e.to_string(),
            })
    }

    pub async fn set_nat_policy(
        &mut self,
        network_id: &str,
        policy_version: &str,
        policy_json: Vec<u8>,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        let req = SetNatPolicyRequest {
            meta: Some(chv_nwd_api::chv_nwd_api::Meta {
                operation_id: operation_id.unwrap_or("").to_string(),
                request_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            }),
            network_id: network_id.to_string(),
            policy: Some(chv_nwd_api::chv_nwd_api::NatPolicy {
                policy_version: policy_version.to_string(),
                policy_json,
            }),
        };
        let span = tracing::info_span!("set_nat_policy", operation_id = operation_id.unwrap_or(""));
        self.inner
            .set_nat_policy(with_operation_id(req, operation_id))
            .instrument(span)
            .await
            .map_err(|e| ChvError::NetworkUnavailable {
                resource: "nwd".to_string(),
                reason: e.to_string(),
            })?;
        Ok(())
    }

    pub async fn detach_vm_nic(
        &mut self,
        nic_id: &str,
        vm_id: &str,
        _network_id: &str,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        let req = DetachVmNicRequest {
            meta: Some(chv_nwd_api::chv_nwd_api::Meta {
                operation_id: operation_id.unwrap_or("").to_string(),
                request_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            }),
            nic_id: nic_id.to_string(),
            vm_id: vm_id.to_string(),
            network_id: _network_id.to_string(),
            vm_mac: String::new(),
        };
        let span = tracing::info_span!("detach_vm_nic", operation_id = operation_id.unwrap_or(""));
        self.inner
            .detach_vm_nic(with_operation_id(req, operation_id))
            .instrument(span)
            .await
            .map_err(|e| ChvError::NetworkUnavailable {
                resource: "nwd".to_string(),
                reason: e.to_string(),
            })?;
        Ok(())
    }

    pub async fn get_network_health(
        &mut self,
        network_id: &str,
    ) -> Result<chv_nwd_api::chv_nwd_api::NetworkHealthResponse, ChvError> {
        let req = NetworkHealthRequest {
            network_id: network_id.to_string(),
        };
        let span = tracing::info_span!("get_network_health");
        let resp = self
            .inner
            .get_network_health(req)
            .instrument(span)
            .await
            .map_err(|e| ChvError::NetworkUnavailable {
                resource: "nwd".to_string(),
                reason: e.to_string(),
            })?
            .into_inner();
        Ok(resp)
    }

    pub async fn ensure_dhcp_scope(
        &mut self,
        network_id: &str,
        cidr: &str,
        range_start: &str,
        range_end: &str,
        dns_servers: Vec<String>,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        let req = EnsureDhcpScopeRequest {
            meta: Some(chv_nwd_api::chv_nwd_api::Meta {
                operation_id: operation_id.unwrap_or("").to_string(),
                request_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            }),
            scope: Some(DhcpScope {
                network_id: network_id.to_string(),
                cidr: cidr.to_string(),
                range_start: range_start.to_string(),
                range_end: range_end.to_string(),
                dns_servers,
                mtu: 0,
            }),
        };
        let span = tracing::info_span!(
            "ensure_dhcp_scope",
            operation_id = operation_id.unwrap_or("")
        );
        self.inner
            .ensure_dhcp_scope(with_operation_id(req, operation_id))
            .instrument(span)
            .await
            .map_err(|e| ChvError::NetworkUnavailable {
                resource: "nwd".to_string(),
                reason: e.to_string(),
            })?;
        Ok(())
    }

    pub async fn ensure_dns_scope(
        &mut self,
        network_id: &str,
        forwarders: Vec<String>,
        static_records: std::collections::HashMap<String, String>,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        let req = EnsureDnsScopeRequest {
            meta: Some(chv_nwd_api::chv_nwd_api::Meta {
                operation_id: operation_id.unwrap_or("").to_string(),
                request_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            }),
            scope: Some(DnsScope {
                network_id: network_id.to_string(),
                forwarders,
                static_records,
            }),
        };
        let span = tracing::info_span!(
            "ensure_dns_scope",
            operation_id = operation_id.unwrap_or("")
        );
        self.inner
            .ensure_dns_scope(with_operation_id(req, operation_id))
            .instrument(span)
            .await
            .map_err(|e| ChvError::NetworkUnavailable {
                resource: "nwd".to_string(),
                reason: e.to_string(),
            })?;
        Ok(())
    }

    pub async fn get_fabric_identity(&mut self) -> Result<(String, u32), ChvError> {
        let req = chv_nwd_api::chv_nwd_api::GetFabricIdentityRequest {};
        let span = tracing::info_span!("get_fabric_identity");
        let resp = self
            .inner
            .get_fabric_identity(req)
            .instrument(span)
            .await
            .map_err(|e| ChvError::NetworkUnavailable {
                resource: "nwd".to_string(),
                reason: e.to_string(),
            })?
            .into_inner();
        if let Some(ref result) = resp.result {
            if !result.status.eq_ignore_ascii_case("ok") && !result.status.is_empty() {
                return Err(ChvError::NetworkUnavailable {
                    resource: "nwd".to_string(),
                    reason: format!(
                        "get_fabric_identity failed: {} ({})",
                        result.human_summary, result.error_code
                    ),
                });
            }
        }
        Ok((resp.public_key, resp.underlay_mtu))
    }

    pub async fn update_overlay(
        &mut self,
        network_id: &str,
        vni: u32,
        vtep_endpoints: Vec<chv_nwd_api::chv_nwd_api::VtepEndpoint>,
        fdb_entries: Vec<chv_nwd_api::chv_nwd_api::FdbEntry>,
        fabric: Option<chv_nwd_api::chv_nwd_api::FabricPlan>,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        let req = UpdateOverlayRequest {
            network_id: network_id.to_string(),
            vni,
            vtep_endpoints,
            fdb_entries,
            fabric,
        };
        let span = tracing::info_span!("update_overlay", operation_id = operation_id.unwrap_or(""));
        let resp = self
            .inner
            .update_overlay(with_operation_id(req, operation_id))
            .instrument(span)
            .await
            .map_err(|e| ChvError::NetworkUnavailable {
                resource: "nwd".to_string(),
                reason: e.to_string(),
            })?
            .into_inner();
        if let Some(ref result) = resp.result {
            if !result.status.eq_ignore_ascii_case("ok") {
                return Err(ChvError::NetworkUnavailable {
                    resource: "nwd".to_string(),
                    reason: format!(
                        "update_overlay failed: {} ({})",
                        result.human_summary, result.error_code
                    ),
                });
            }
        }
        Ok(())
    }

    pub async fn send_gratuitous_arp(
        &mut self,
        network_id: &str,
        vm_ip: &str,
        bridge_name: &str,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        let req = SendGratuitousArpRequest {
            meta: Some(chv_nwd_api::chv_nwd_api::Meta {
                operation_id: operation_id.unwrap_or("").to_string(),
                request_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            }),
            network_id: network_id.to_string(),
            vm_ip: vm_ip.to_string(),
            bridge_name: bridge_name.to_string(),
        };
        let span = tracing::info_span!(
            "send_gratuitous_arp",
            operation_id = operation_id.unwrap_or("")
        );
        let resp = self
            .inner
            .send_gratuitous_arp(with_operation_id(req, operation_id))
            .instrument(span)
            .await
            .map_err(|e| ChvError::NetworkUnavailable {
                resource: "nwd".to_string(),
                reason: e.to_string(),
            })?
            .into_inner();
        if let Some(ref result) = resp.result {
            if !result.status.eq_ignore_ascii_case("ok") {
                return Err(ChvError::NetworkUnavailable {
                    resource: "nwd".to_string(),
                    reason: format!(
                        "send_gratuitous_arp failed: {} ({})",
                        result.human_summary, result.error_code
                    ),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod fabric_test_support {
    //! Shared fabric-aware NWD mock for agent tests: serves
    //! `GetFabricIdentity` with a fixed identity and records the fabric
    //! plan of every `UpdateOverlay` call. All other RPCs (and the
    //! identity RPC when `identity_error` is set) fail like an unhealthy
    //! nwd, exercising the callers' best-effort paths.

    use chv_nwd_api::chv_nwd_api::network_service_server::NetworkService;
    use std::sync::{Arc, Mutex};
    use tonic::{Request, Response, Status};

    #[derive(Default)]
    pub(crate) struct FabricNwdCalls {
        pub overlay_fabrics: Mutex<Vec<chv_nwd_api::chv_nwd_api::FabricPlan>>,
    }

    pub(crate) struct MockFabricNwd {
        pub calls: Arc<FabricNwdCalls>,
        pub public_key: String,
        pub underlay_mtu: u32,
        /// When true, `GetFabricIdentity` answers with an in-band error
        /// result instead of an identity.
        pub identity_error: bool,
    }

    fn ok_result() -> chv_nwd_api::chv_nwd_api::Result {
        chv_nwd_api::chv_nwd_api::Result {
            status: "ok".to_string(),
            error_code: String::new(),
            human_summary: String::new(),
        }
    }

    #[tonic::async_trait]
    impl NetworkService for MockFabricNwd {
        async fn get_fabric_identity(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::GetFabricIdentityRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::FabricIdentityResponse>, Status> {
            if self.identity_error {
                return Ok(Response::new(
                    chv_nwd_api::chv_nwd_api::FabricIdentityResponse {
                        result: Some(chv_nwd_api::chv_nwd_api::Result {
                            status: "error".to_string(),
                            error_code: "FABRIC_DISABLED".to_string(),
                            human_summary: "fabric disabled".to_string(),
                        }),
                        public_key: String::new(),
                        underlay_mtu: 0,
                    },
                ));
            }
            Ok(Response::new(
                chv_nwd_api::chv_nwd_api::FabricIdentityResponse {
                    result: Some(ok_result()),
                    public_key: self.public_key.clone(),
                    underlay_mtu: self.underlay_mtu,
                },
            ))
        }

        async fn update_overlay(
            &self,
            req: Request<chv_nwd_api::chv_nwd_api::UpdateOverlayRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::UpdateOverlayResponse>, Status> {
            let inner = req.into_inner();
            self.calls
                .overlay_fabrics
                .lock()
                .unwrap()
                .push(inner.fabric.unwrap_or_default());
            Ok(Response::new(
                chv_nwd_api::chv_nwd_api::UpdateOverlayResponse {
                    result: Some(ok_result()),
                },
            ))
        }

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
            _req: Request<chv_nwd_api::chv_nwd_api::DetachVmNicRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::Result>, Status> {
            Err(Status::unimplemented(""))
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

        async fn send_gratuitous_arp(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::SendGratuitousArpRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::SendGratuitousArpResponse>, Status> {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chv_nwd_api::chv_nwd_api::network_service_server::NetworkService;
    use chv_stord_api::chv_stord_api::storage_service_server::StorageService;
    use tonic::{Request, Response, Status};

    #[derive(Default)]
    struct MockStord {
        /// Sessions returned by `list_volume_sessions`; empty by default.
        /// Used to pin the #711 `(volume_id, vm_id)` handle resolution.
        sessions: Vec<chv_stord_api::chv_stord_api::VolumeSession>,
        /// When set, `trigger_disk_migration` captures the request and
        /// answers OK instead of `unimplemented` — the wire-contract pin
        /// for the agent→stord thread (issue #394 Option C).
        capture_trigger: Option<
            std::sync::Arc<
                std::sync::Mutex<Option<chv_stord_api::chv_stord_api::TriggerDiskMigrationRequest>>,
            >,
        >,
        /// When true, the mock plays a pre-#582 (version-skewed) stord:
        /// it answers OK but never echoes `pause_first` back (proto3
        /// default false) — the shape the T=0 skew guard must catch.
        skew_no_echo: bool,
        /// Reject this many initial trigger calls with
        /// `RESOURCE_EXHAUSTED` before behaving normally. Models a
        /// destination at its admission ceiling whose slot is later
        /// released, so the agent's bounded retry can be exercised.
        resource_exhausted_first: std::sync::atomic::AtomicU32,
    }

    #[tonic::async_trait]
    impl StorageService for MockStord {
        async fn list_volume_sessions(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::ListVolumeSessionsRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::ListVolumeSessionsResponse>, Status>
        {
            Ok(Response::new(
                chv_stord_api::chv_stord_api::ListVolumeSessionsResponse {
                    sessions: self.sessions.clone(),
                },
            ))
        }
        // Stub remaining methods
        async fn open_volume(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::OpenVolumeRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::OpenVolumeResponse>, Status> {
            Err(Status::unimplemented(""))
        }
        async fn close_volume(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::CloseVolumeRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::Result>, Status> {
            Err(Status::unimplemented(""))
        }
        // #522 PR 1: the daemon-client suite's mock does not drive the
        // destroy primitive (its coverage is the connect/open paths).
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
        async fn attach_volume_to_vm(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::AttachVolumeToVmRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::AttachVolumeToVmResponse>, Status>
        {
            Err(Status::unimplemented(""))
        }
        async fn detach_volume_from_vm(
            &self,
            _req: Request<chv_stord_api::chv_stord_api::DetachVolumeFromVmRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::Result>, Status> {
            Err(Status::unimplemented(""))
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
            req: Request<chv_stord_api::chv_stord_api::TriggerDiskMigrationRequest>,
        ) -> Result<Response<chv_stord_api::chv_stord_api::TriggerDiskMigrationResponse>, Status>
        {
            if self
                .resource_exhausted_first
                .load(std::sync::atomic::Ordering::SeqCst)
                > 0
            {
                self.resource_exhausted_first
                    .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                return Err(Status::resource_exhausted(
                    "migration admission rejected: concurrency limit reached (mock)",
                ));
            }
            if let Some(capture) = &self.capture_trigger {
                let inner = req.into_inner();
                // Mirror the real stord's echo (#582): the response
                // carries the mode that will actually run — unless the
                // mock plays a skewed stord that predates the field.
                let echo = if self.skew_no_echo {
                    false
                } else {
                    inner.pause_first
                };
                *capture.lock().unwrap() = Some(inner);
                return Ok(Response::new(
                    chv_stord_api::chv_stord_api::TriggerDiskMigrationResponse {
                        result: Some(chv_stord_api::chv_stord_api::Result {
                            status: "OK".to_string(),
                            error_code: "OK".to_string(),
                            human_summary: String::new(),
                        }),
                        migration_id: "dm-mock-1".to_string(),
                        pause_first: echo,
                    },
                ));
            }
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

    struct MockNwd;
    #[tonic::async_trait]
    impl NetworkService for MockNwd {
        async fn list_namespace_state(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::ListNamespaceStateRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::ListNamespaceStateResponse>, Status>
        {
            Ok(Response::new(
                chv_nwd_api::chv_nwd_api::ListNamespaceStateResponse { items: vec![] },
            ))
        }
        // Stub remaining methods
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
            _req: Request<chv_nwd_api::chv_nwd_api::DetachVmNicRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::Result>, Status> {
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

        async fn send_gratuitous_arp(
            &self,
            _req: Request<chv_nwd_api::chv_nwd_api::SendGratuitousArpRequest>,
        ) -> Result<Response<chv_nwd_api::chv_nwd_api::SendGratuitousArpResponse>, Status> {
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
    }

    #[tokio::test]
    async fn stord_health_probe_mock() {
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
        let mut client = StordClient::connect(&socket).await.unwrap();
        assert!(client.health_probe().await.unwrap());
    }

    /// #711: the post-restart delete recovers the live handle from stord's
    /// durable session table by `(volume_id, vm_id)`. Detached rows (empty
    /// `vm_id`) and sessions for another VM/volume must never be selected.
    #[tokio::test]
    async fn stord_resolve_volume_handle_matches_the_attached_session() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("stord.sock");

        let session = |volume_id: &str, vm_id: &str, handle: &str| {
            chv_stord_api::chv_stord_api::VolumeSession {
                volume_id: volume_id.to_string(),
                vm_id: vm_id.to_string(),
                attachment_handle: handle.to_string(),
                export_kind: "local".to_string(),
                export_path: String::new(),
                runtime_status: if vm_id.is_empty() { "open" } else { "attached" }.to_string(),
            }
        };
        let mock = MockStord {
            sessions: vec![
                session("vol-0", "", "handle-detached"),
                session("vol-0", "vm-other", "handle-other"),
                session("vol-1", "vm-1", "handle-wrong-volume"),
                session("vol-0", "vm-1", "handle-live"),
            ],
            ..Default::default()
        };

        let uds = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(
                    chv_stord_api::chv_stord_api::storage_service_server::StorageServiceServer::new(
                        mock,
                    ),
                )
                .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds))
                .await
                .ok();
        });

        let mut client = StordClient::connect(&socket).await.unwrap();
        assert_eq!(
            client.resolve_volume_handle("vol-0", "vm-1").await.unwrap(),
            Some("handle-live".to_string())
        );
        assert_eq!(
            client.resolve_volume_handle("vol-9", "vm-1").await.unwrap(),
            None
        );
        assert_eq!(
            client.resolve_volume_handle("vol-0", "").await.unwrap(),
            None,
            "a detached row has no live VM attachment to close"
        );
    }

    #[tokio::test]
    async fn nwd_health_probe_mock() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("nwd.sock");

        let uds = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(
                    chv_nwd_api::chv_nwd_api::network_service_server::NetworkServiceServer::new(
                        MockNwd,
                    ),
                )
                .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds))
                .await
                .ok();
        });

        // No startup race: the Unix listener is bound (listen() done)
        // before the server task is spawned, so connect() succeeds via
        // the kernel backlog even before the task accepts.
        let mut client = NwdClient::connect(&socket).await.unwrap();
        assert!(client.health_probe().await.unwrap());
    }

    #[tokio::test]
    async fn stord_resize_volume_propagates_backend_error() {
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
        let mut client = StordClient::connect(&socket).await.unwrap();
        let result = client.resize_volume("vol-1", 1024, Some("op-1")).await;
        assert!(matches!(result, Err(ChvError::BackendUnavailable { .. })));
    }

    /// Issue #394 Option C wire pin: `StordClient::trigger_disk_migration`
    /// must place `pause_first` (and the identifying fields) on the
    /// `TriggerDiskMigrationRequest` — the agent→stord hop of the
    /// stop-the-world opt-in. The e2e tiers pin the CLI→BFF→CP and
    /// handler→sender hops; this is the one between them.
    #[tokio::test]
    async fn stord_trigger_disk_migration_carries_pause_first() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("stord.sock");

        let captured: std::sync::Arc<
            std::sync::Mutex<Option<chv_stord_api::chv_stord_api::TriggerDiskMigrationRequest>>,
        > = std::sync::Arc::new(std::sync::Mutex::new(None));
        let mock = MockStord {
            sessions: Vec::new(),
            capture_trigger: Some(captured.clone()),
            skew_no_echo: false,
            resource_exhausted_first: std::sync::atomic::AtomicU32::new(0),
        };

        let uds = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(
                    chv_stord_api::chv_stord_api::storage_service_server::StorageServiceServer::new(
                        mock,
                    ),
                )
                .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds))
                .await
                .ok();
        });

        let mut client = StordClient::connect(&socket).await.unwrap();
        let migration_id = client
            .trigger_disk_migration(
                "vol-pf",
                "handle-pf",
                "https://10.0.0.5:50052",
                "token-pf",
                true,
                Some("op-pf"),
            )
            .await
            .expect("trigger must succeed against the capturing mock");
        assert_eq!(migration_id, "dm-mock-1");

        let req = captured.lock().unwrap().take().expect("request captured");
        assert_eq!(req.volume_id, "vol-pf");
        assert_eq!(req.attachment_handle, "handle-pf");
        assert_eq!(req.dest_endpoint, "https://10.0.0.5:50052");
        assert!(
            req.pause_first,
            "the stop-the-world opt-in must ride the trigger request"
        );
        assert_eq!(
            req.migration_authorization, "token-pf",
            "the control-plane migration authorization must ride the trigger request"
        );
        assert_eq!(
            req.meta.expect("meta present").operation_id,
            "op-pf",
            "the operation id must thread for tracing"
        );
    }

    /// Issue #582 (T=0 skew guard): a stord that predates the
    /// `pause_first` field drops the unknown request field (proto3)
    /// and would silently run the migration in DEFAULT mode — the
    /// operator's stop-the-world opt-in ignored. The guard: the
    /// response must echo the mode; a missing echo on a pause-first
    /// request fails closed at trigger time. The mock plays the
    /// skewed stord (OK result, no echo).
    ///
    /// Also pins the guard's one-directionality: a default-mode
    /// request (pause_first=false) against the SAME skewed stord must
    /// still succeed — the guard must not false-positive on the mode
    /// nobody asked to coordinate.
    #[tokio::test]
    async fn stord_trigger_disk_migration_fails_closed_on_missing_pause_first_echo() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("stord.sock");

        let captured: std::sync::Arc<
            std::sync::Mutex<Option<chv_stord_api::chv_stord_api::TriggerDiskMigrationRequest>>,
        > = std::sync::Arc::new(std::sync::Mutex::new(None));
        let mock = MockStord {
            sessions: Vec::new(),
            capture_trigger: Some(captured.clone()),
            skew_no_echo: true,
            resource_exhausted_first: std::sync::atomic::AtomicU32::new(0),
        };

        let uds = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(
                    chv_stord_api::chv_stord_api::storage_service_server::StorageServiceServer::new(
                        mock,
                    ),
                )
                .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds))
                .await
                .ok();
        });

        let mut client = StordClient::connect(&socket).await.unwrap();

        // The discriminating call: pause-first requested, no echo back.
        let err = client
            .trigger_disk_migration(
                "vol-pf",
                "handle-pf",
                "https://10.0.0.5:50052",
                "token-pf",
                true,
                Some("op-pf"),
            )
            .await
            .expect_err("a missing pause_first echo must fail closed");
        match err {
            ChvError::BackendUnavailable { reason, .. } => {
                assert!(
                    reason.contains("version skew"),
                    "the error must name the skew: {reason}"
                );
                assert!(
                    reason.contains("pause_first") || reason.contains("pause-first"),
                    "the error must name the mode: {reason}"
                );
            }
            other => panic!("expected BackendUnavailable, got: {other:?}"),
        }

        // The request DID carry the flag — proving the guard fired on
        // the response echo, not on a dropped request field.
        let req = captured.lock().unwrap().take().expect("request captured");
        assert!(
            req.pause_first,
            "the guard must discriminate on the echo, not the request"
        );

        // One-directionality: default mode needs no echo and must
        // succeed against the same skewed stord.
        client
            .trigger_disk_migration(
                "vol-def",
                "handle-def",
                "https://10.0.0.5:50052",
                "token-def",
                false,
                Some("op-def"),
            )
            .await
            .expect("default-mode trigger must not require the echo");
    }

    /// Spawn a mock stord UDS server that rejects the first
    /// `resource_exhausted_first` triggers with `RESOURCE_EXHAUSTED` and
    /// then behaves normally. Returns the connected client plus the
    /// request-capture slot used by the mock.
    async fn retry_mock_stord(
        resource_exhausted_first: u32,
    ) -> (
        tempfile::TempDir,
        StordClient,
        std::sync::Arc<
            std::sync::Mutex<Option<chv_stord_api::chv_stord_api::TriggerDiskMigrationRequest>>,
        >,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("stord.sock");

        let captured: std::sync::Arc<
            std::sync::Mutex<Option<chv_stord_api::chv_stord_api::TriggerDiskMigrationRequest>>,
        > = std::sync::Arc::new(std::sync::Mutex::new(None));
        let mock = MockStord {
            sessions: Vec::new(),
            capture_trigger: Some(captured.clone()),
            skew_no_echo: false,
            resource_exhausted_first: std::sync::atomic::AtomicU32::new(resource_exhausted_first),
        };

        let uds = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(
                    chv_stord_api::chv_stord_api::storage_service_server::StorageServiceServer::new(
                        mock,
                    ),
                )
                .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds))
                .await
                .ok();
        });

        (dir, StordClient::connect(&socket).await.unwrap(), captured)
    }

    fn retry_test_volume() -> crate::migration::MigrationVolume {
        crate::migration::MigrationVolume {
            volume_id: "vol-retry".to_string(),
            attachment_handle: "handle-retry".to_string(),
        }
    }

    /// Issue #663 follow-up: a transient destination admission rejection
    /// (`RESOURCE_EXHAUSTED`) must be retried with the SAME authorization
    /// token — rejection happens before nonce consumption, so the token is
    /// still valid once a slot frees.
    #[tokio::test]
    async fn stord_trigger_retries_resource_exhausted_then_succeeds() {
        let (_dir, mut client, captured) = retry_mock_stord(2).await;
        let cancel = tokio_util::sync::CancellationToken::new();

        let id = crate::migration::trigger_disk_migration_with_admission_retry(
            &mut client,
            &retry_test_volume(),
            "https://10.0.0.5:50052",
            "token-retry",
            false,
            "op-retry",
            &cancel,
        )
        .await
        .expect("the third trigger (two rejections then capacity) must succeed");
        assert_eq!(id, "dm-mock-1");

        let req = captured.lock().unwrap().take().expect("request captured");
        assert_eq!(
            req.migration_authorization, "token-retry",
            "the retry must reuse the same unconsumed authorization token"
        );
    }

    /// The retry is bounded: a destination that stays at its admission
    /// ceiling must fail after exactly the configured attempt count with
    /// actionable text, rather than retrying forever.
    #[tokio::test]
    async fn stord_trigger_fails_after_bounded_resource_exhausted_retries() {
        let (_dir, mut client, _captured) = retry_mock_stord(u32::MAX).await;
        let cancel = tokio_util::sync::CancellationToken::new();

        let err = crate::migration::trigger_disk_migration_with_admission_retry(
            &mut client,
            &retry_test_volume(),
            "https://10.0.0.5:50052",
            "token-retry",
            false,
            "op-retry",
            &cancel,
        )
        .await
        .expect_err("a permanently full destination must fail");
        assert!(
            err.contains("after 4 attempt(s)"),
            "the error must report the bounded attempt count: {err}"
        );
        assert!(
            err.contains("no authorization token was consumed"),
            "the error must state the token remained usable: {err}"
        );
        assert!(
            err.contains("max_inflight_receiving"),
            "the error must name the destination admission ceiling: {err}"
        );
    }

    #[tokio::test]
    async fn nwd_detach_vm_nic_rpc_ok() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("nwd.sock");

        let uds = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(
                    chv_nwd_api::chv_nwd_api::network_service_server::NetworkServiceServer::new(
                        MockNwd,
                    ),
                )
                .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds))
                .await
                .ok();
        });

        // No startup race: the Unix listener is bound (listen() done)
        // before the server task is spawned, so connect() succeeds via
        // the kernel backlog even before the task accepts.
        let mut client = NwdClient::connect(&socket).await.unwrap();
        let result = client
            .detach_vm_nic("nic-1", "vm-1", "net-1", Some("op-1"))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn nwd_get_fabric_identity_maps_fields() {
        use super::fabric_test_support::{FabricNwdCalls, MockFabricNwd};

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("nwd.sock");

        let calls = std::sync::Arc::new(FabricNwdCalls::default());
        {
            let uds = tokio::net::UnixListener::bind(&socket).unwrap();
            let service = MockFabricNwd {
                calls: calls.clone(),
                public_key: "wg-pub-key-1".to_string(),
                underlay_mtu: 1500,
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

        let mut client = NwdClient::connect(&socket).await.unwrap();
        let (public_key, underlay_mtu) = client.get_fabric_identity().await.unwrap();
        assert_eq!(public_key, "wg-pub-key-1");
        assert_eq!(underlay_mtu, 1500);
    }

    #[tokio::test]
    async fn nwd_get_fabric_identity_rpc_error_maps_to_network_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("nwd.sock");

        let uds = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(
                    chv_nwd_api::chv_nwd_api::network_service_server::NetworkServiceServer::new(
                        MockNwd,
                    ),
                )
                .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds))
                .await
                .ok();
        });

        let mut client = NwdClient::connect(&socket).await.unwrap();
        let result = client.get_fabric_identity().await;
        assert!(matches!(result, Err(ChvError::NetworkUnavailable { .. })));
    }

    #[tokio::test]
    async fn nwd_get_fabric_identity_in_band_error_maps_to_network_unavailable() {
        use super::fabric_test_support::{FabricNwdCalls, MockFabricNwd};

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("nwd.sock");

        let calls = std::sync::Arc::new(FabricNwdCalls::default());
        {
            let uds = tokio::net::UnixListener::bind(&socket).unwrap();
            let service = MockFabricNwd {
                calls,
                public_key: String::new(),
                underlay_mtu: 0,
                identity_error: true,
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

        let mut client = NwdClient::connect(&socket).await.unwrap();
        let result = client.get_fabric_identity().await;
        assert!(matches!(result, Err(ChvError::NetworkUnavailable { .. })));
    }

    #[tokio::test]
    async fn nwd_update_overlay_relays_fabric_plan() {
        use super::fabric_test_support::{FabricNwdCalls, MockFabricNwd};

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

        let plan = chv_nwd_api::chv_nwd_api::FabricPlan {
            fabric_domain_id: "fabric-1".to_string(),
            local_host_id: "node-1".to_string(),
            local_fabric_ip: "100.100.0.1".to_string(),
            tenant_mtu: 1380,
            fabric_mtu: 1440,
            binding_generation: 3,
            plan_generation: 7,
            peers: vec![chv_nwd_api::chv_nwd_api::FabricPeer {
                node_id: "node-2".to_string(),
                public_key: "wg-pub-node-2".to_string(),
                underlay_endpoint: "10.0.0.2:65001".to_string(),
                fabric_ip: "100.100.0.2".to_string(),
            }],
        };

        let mut client = NwdClient::connect(&socket).await.unwrap();
        client
            .update_overlay("net-1", 42, vec![], vec![], Some(plan), Some("op-1"))
            .await
            .unwrap();

        let recorded = calls.overlay_fabrics.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].fabric_domain_id, "fabric-1");
        assert_eq!(recorded[0].peers.len(), 1);
        assert_eq!(recorded[0].peers[0].public_key, "wg-pub-node-2");
    }
}
