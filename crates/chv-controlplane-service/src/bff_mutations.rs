use async_trait::async_trait;
use chv_controlplane_store::StorePool;
use chv_controlplane_types::domain::Generation;
use chv_webui_bff::{BffError, MutationService};
use chv_webui_bff_api::chv_webui_bff_v1::{
    MutateNetworkResponse, MutateNodeResponse, MutateVmResponse, MutateVolumeResponse,
};
use control_plane_node_api::control_plane_node_api as proto;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::lifecycle::LifecycleService;
use crate::ControlPlaneServiceError;

#[derive(Clone)]
pub struct ControlPlaneMutationService {
    pool: StorePool,
    lifecycle_service: Arc<dyn LifecycleService>,
    /// Monotonic generation counter. Tracks the last generation value issued
    /// so that NTP clock adjustments (rollbacks) cannot produce a generation
    /// lower than one already in use.
    last_generation: Arc<AtomicU64>,
}

#[derive(sqlx::FromRow)]
#[allow(dead_code)]
struct VolumeLookupRow {
    node_id: String,
    vm_id: Option<String>,
    size_bytes: i64,
    /// #379 PR 2 (A9): the volume's storage class (NULL = local), riding
    /// the attach mutation's `volume_spec_json`.
    storage_class: Option<String>,
    /// #533: the volume's kind (NULL = embedded/boot/pre-#513 lineage;
    /// `'data'` = the #513 standalone stamp), riding the attach
    /// mutation's `volume_spec_json` so a standalone volume's open
    /// locator is the create carrier's `{volume_id}.img` — the same
    /// discriminator the #522 delete's kind gate rides.
    volume_kind: Option<String>,
}

impl ControlPlaneMutationService {
    pub fn new(pool: StorePool, lifecycle_service: Arc<dyn LifecycleService>) -> Self {
        Self {
            pool,
            lifecycle_service,
            last_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64
    }

    fn fresh_generation(&self) -> Generation {
        let clock_ms = Self::now_ms() as u64;
        let mut prev = self.last_generation.load(Ordering::Acquire);
        loop {
            let next = if clock_ms > prev { clock_ms } else { prev + 1 };
            match self.last_generation.compare_exchange_weak(
                prev,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Generation::new(next),
                Err(actual) => prev = actual,
            }
        }
    }

    /// Highest generation already durably recorded, across every table a
    /// generation can order. The control plane mints wall-clock
    /// milliseconds, so a clock rollback across a restart could otherwise
    /// produce a generation below an existing row and have the guarded
    /// desired-state upserts silently discard the write (#664).
    async fn generation_floor(&self) -> Result<u64, BffError> {
        let floor: Option<i64> = sqlx::query_scalar(
            r#"
            SELECT MAX(generation) FROM (
                SELECT MAX(desired_generation) AS generation FROM vm_desired_state
                UNION ALL
                SELECT MAX(desired_generation) FROM volume_desired_state
                UNION ALL
                SELECT MAX(desired_generation) FROM network_desired_state
                UNION ALL
                SELECT MAX(desired_generation) FROM node_desired_state
                UNION ALL
                SELECT MAX(desired_generation) FROM operations
            ) WHERE generation IS NOT NULL
            "#,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| BffError::Internal(format!("failed to read generation floor: {}", e)))?;
        Ok(floor.unwrap_or(0).max(0) as u64)
    }

    /// Raise the process-local counter to at least `floor`, preserving the
    /// in-process monotonicity guarantee while seeding it from durable
    /// state after a restart.
    fn observe_generation_floor(&self, floor: u64) {
        let mut prev = self.last_generation.load(Ordering::Acquire);
        while prev < floor {
            match self.last_generation.compare_exchange_weak(
                prev,
                floor,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(actual) => prev = actual,
            }
        }
    }

    async fn build_meta(
        &self,
        node_id: String,
        requested_by: String,
    ) -> Result<proto::RequestMeta, BffError> {
        // Read the durable floor on every mint. It is a single statement of
        // indexed MAX lookups; correctness matters more than the microseconds
        // saved by caching it, and a cached floor could be stale after a
        // database restore.
        let floor = self.generation_floor().await?;
        self.observe_generation_floor(floor);
        let generation = self.fresh_generation();
        Ok(proto::RequestMeta {
            operation_id: "".into(),
            requested_by,
            target_node_id: node_id,
            desired_state_version: generation.to_string(),
            request_unix_ms: Self::now_ms(),
        })
    }

    fn map_ack(
        ack: Result<proto::AckResponse, ControlPlaneServiceError>,
    ) -> Result<proto::AckResponse, BffError> {
        let ack = ack.map_err(|e| match e {
            ControlPlaneServiceError::NotFound(msg) => BffError::NotFound(msg),
            ControlPlaneServiceError::InvalidArgument(msg) => BffError::BadRequest(msg),
            ControlPlaneServiceError::Unauthorized(msg) => BffError::Unauthorized(msg),
            ControlPlaneServiceError::Conflict(msg) => BffError::Conflict(msg),
            _ => BffError::Internal(e.to_string()),
        })?;
        if ack.result.as_ref().map(|r| r.status.as_str()) != Some("OK") {
            let msg = ack
                .result
                .as_ref()
                .map(|r| r.human_summary.clone())
                .unwrap_or_else(|| "operation rejected".into());
            return Err(BffError::BadRequest(msg));
        }
        Ok(ack)
    }
}

#[async_trait]
impl MutationService for ControlPlaneMutationService {
    async fn mutate_vm(
        &self,
        vm_id: String,
        action: String,
        force: bool,
        requested_by: String,
    ) -> Result<MutateVmResponse, BffError> {
        // Look up the VM's node_id from the vms table.
        let node_id =
            sqlx::query_scalar::<_, Option<String>>("SELECT node_id FROM vms WHERE vm_id = ?")
                .bind(&vm_id)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| BffError::Internal(format!("failed to look up vm: {}", e)))?
                .ok_or_else(|| BffError::NotFound(format!("vm {} not found", vm_id)))?;

        let meta = Some(self.build_meta(node_id.clone(), requested_by).await?);

        let ack = match action.as_str() {
            "start" => {
                self.lifecycle_service
                    .start_vm(proto::StartVmRequest {
                        meta,
                        node_id: node_id.clone(),
                        vm_id: vm_id.clone(),
                    })
                    .await
            }
            "stop" => {
                self.lifecycle_service
                    .stop_vm(proto::StopVmRequest {
                        meta,
                        node_id: node_id.clone(),
                        vm_id: vm_id.clone(),
                        force,
                    })
                    .await
            }
            "poweroff" => {
                self.lifecycle_service
                    .stop_vm(proto::StopVmRequest {
                        meta,
                        node_id: node_id.clone(),
                        vm_id: vm_id.clone(),
                        force: true,
                    })
                    .await
            }
            "restart" => {
                self.lifecycle_service
                    .reboot_vm(proto::RebootVmRequest {
                        meta,
                        node_id: node_id.clone(),
                        vm_id: vm_id.clone(),
                        force,
                    })
                    .await
            }
            "delete" => {
                self.lifecycle_service
                    .delete_vm(proto::DeleteVmRequest {
                        meta,
                        node_id: node_id.clone(),
                        vm_id: vm_id.clone(),
                        force,
                    })
                    .await
            }
            "pause" => {
                self.lifecycle_service
                    .pause_vm(proto::PauseVmRequest {
                        meta,
                        node_id: node_id.clone(),
                        vm_id: vm_id.clone(),
                    })
                    .await
            }
            "resume" => {
                self.lifecycle_service
                    .resume_vm(proto::ResumeVmRequest {
                        meta,
                        node_id: node_id.clone(),
                        vm_id: vm_id.clone(),
                    })
                    .await
            }
            _ => return Err(BffError::BadRequest(format!("invalid action: {}", action))),
        };

        let ack = Self::map_ack(ack)?;
        let result = ack
            .result
            .ok_or_else(|| BffError::Internal("missing ack result".into()))?;

        Ok(MutateVmResponse {
            accepted: result.status == "OK",
            task_id: result.operation_id,
            vm_id,
            summary: result.human_summary,
        })
    }

    async fn migrate_vm(
        &self,
        vm_id: String,
        target_node_id: String,
        pause_first: bool,
        requested_by: String,
    ) -> Result<MutateVmResponse, BffError> {
        // Issue #656: live/concurrent-write migration is not supported
        // until #394 is proven. Fail closed at the user-facing API before
        // any lookup or lifecycle dispatch; do NOT silently coerce false
        // to true (that would claim a stop-the-world pause the operator
        // did not request).
        if !pause_first {
            return Err(BffError::BadRequest(
                "live/concurrent-write migration is not supported until #394 is proven; \
                 set pause_first=true (stop-the-world)"
                    .to_string(),
            ));
        }

        let source_node_id =
            sqlx::query_scalar::<_, Option<String>>("SELECT node_id FROM vms WHERE vm_id = ?")
                .bind(&vm_id)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| BffError::Internal(format!("failed to look up vm: {}", e)))?
                .ok_or_else(|| BffError::NotFound(format!("vm {} not found", vm_id)))?;

        if source_node_id == target_node_id {
            return Err(BffError::BadRequest(
                "target_node_id must differ from current node".into(),
            ));
        }

        let meta = Some(
            self.build_meta(source_node_id.clone(), requested_by)
                .await?,
        );
        let ack = self
            .lifecycle_service
            .migrate_vm(proto::MigrateVmRequest {
                meta,
                node_id: source_node_id.clone(),
                vm_id: vm_id.clone(),
                source_node_id: source_node_id.clone(),
                destination_node_id: target_node_id,
                // Issue #394 Option C: the operator's opt-in
                // stop-the-world mode. The tuning fields stay 0
                // ("use stord's defaults") — pause_first is the only
                // field the agent currently reads.
                config: Some(proto::MigrationConfig {
                    dirty_threshold_blocks: 0,
                    max_convergence_rounds: 0,
                    block_size_bytes: 0,
                    total_timeout_seconds: 0,
                    pause_first,
                    // Minted later by the orchestrator, immediately before
                    // dispatching to the source agent (issue #663).
                    volume_authorizations: Vec::new(),
                    allow_insecure_unauthenticated: false,
                }),
            })
            .await;

        let ack = Self::map_ack(ack)?;
        let result = ack
            .result
            .ok_or_else(|| BffError::Internal("missing ack result".into()))?;

        Ok(MutateVmResponse {
            accepted: result.status == "OK",
            task_id: result.operation_id,
            vm_id,
            summary: result.human_summary,
        })
    }

    async fn snapshot_vm(
        &self,
        vm_id: String,
        destination: String,
        requested_by: String,
    ) -> Result<MutateVmResponse, BffError> {
        let node_id =
            sqlx::query_scalar::<_, Option<String>>("SELECT node_id FROM vms WHERE vm_id = ?")
                .bind(&vm_id)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| BffError::Internal(format!("failed to look up vm: {}", e)))?
                .ok_or_else(|| BffError::NotFound(format!("vm {} not found", vm_id)))?;

        let meta = Some(self.build_meta(node_id.clone(), requested_by).await?);
        let ack = self
            .lifecycle_service
            .snapshot_vm(proto::SnapshotVmRequest {
                meta,
                node_id: node_id.clone(),
                vm_id: vm_id.clone(),
                destination,
            })
            .await;

        let ack = Self::map_ack(ack)?;
        let result = ack
            .result
            .ok_or_else(|| BffError::Internal("missing ack result".into()))?;

        Ok(MutateVmResponse {
            accepted: result.status == "OK",
            task_id: result.operation_id,
            vm_id,
            summary: result.human_summary,
        })
    }

    async fn restore_snapshot(
        &self,
        vm_id: String,
        source: String,
        requested_by: String,
    ) -> Result<MutateVmResponse, BffError> {
        let node_id =
            sqlx::query_scalar::<_, Option<String>>("SELECT node_id FROM vms WHERE vm_id = ?")
                .bind(&vm_id)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| BffError::Internal(format!("failed to look up vm: {}", e)))?
                .ok_or_else(|| BffError::NotFound(format!("vm {} not found", vm_id)))?;

        let meta = Some(self.build_meta(node_id.clone(), requested_by).await?);
        let ack = self
            .lifecycle_service
            .restore_snapshot(proto::RestoreSnapshotRequest {
                meta,
                node_id: node_id.clone(),
                vm_id: vm_id.clone(),
                source,
            })
            .await;

        let ack = Self::map_ack(ack)?;
        let result = ack
            .result
            .ok_or_else(|| BffError::Internal("missing ack result".into()))?;

        Ok(MutateVmResponse {
            accepted: result.status == "OK",
            task_id: result.operation_id,
            vm_id,
            summary: result.human_summary,
        })
    }

    async fn mutate_node(
        &self,
        node_id: String,
        action: String,
        requested_by: String,
    ) -> Result<MutateNodeResponse, BffError> {
        let meta = Some(self.build_meta(node_id.clone(), requested_by).await?);

        let ack = match action.as_str() {
            "pause_scheduling" => {
                self.lifecycle_service
                    .pause_node_scheduling(proto::PauseNodeSchedulingRequest {
                        meta,
                        node_id: node_id.clone(),
                    })
                    .await
            }
            "resume_scheduling" => {
                self.lifecycle_service
                    .resume_node_scheduling(proto::ResumeNodeSchedulingRequest {
                        meta,
                        node_id: node_id.clone(),
                    })
                    .await
            }
            "drain" => {
                self.lifecycle_service
                    .drain_node(proto::DrainNodeRequest {
                        meta,
                        node_id: node_id.clone(),
                        allow_workload_stop: false,
                    })
                    .await
            }
            "enter_maintenance" => {
                self.lifecycle_service
                    .enter_maintenance(proto::EnterMaintenanceRequest {
                        meta,
                        node_id: node_id.clone(),
                        reason: "webui initiated".into(),
                    })
                    .await
            }
            "exit_maintenance" => {
                self.lifecycle_service
                    .exit_maintenance(proto::ExitMaintenanceRequest {
                        meta,
                        node_id: node_id.clone(),
                    })
                    .await
            }
            _ => return Err(BffError::BadRequest(format!("invalid action: {}", action))),
        };

        let ack = Self::map_ack(ack)?;
        let result = ack
            .result
            .ok_or_else(|| BffError::Internal("missing ack result".into()))?;

        Ok(MutateNodeResponse {
            accepted: result.status == "OK",
            task_id: result.operation_id,
            node_id,
            summary: result.human_summary,
        })
    }

    async fn mutate_volume(
        &self,
        volume_id: String,
        action: String,
        force: bool,
        resize_bytes: Option<u64>,
        vm_id: Option<String>,
        requested_by: String,
    ) -> Result<MutateVolumeResponse, BffError> {
        // Look up the volume's node_id from volumes and attachment/size from volume_desired_state/volumes.
        let row = sqlx::query_as::<_, VolumeLookupRow>(
            r#"
            SELECT
                v.node_id as node_id,
                vds.attached_vm_id as vm_id,
                v.capacity_bytes as size_bytes,
                v.storage_class as storage_class,
                v.volume_kind as volume_kind
            FROM volumes v
            JOIN volume_desired_state vds ON v.volume_id = vds.volume_id
            WHERE v.volume_id = ?
            "#,
        )
        .bind(&volume_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| BffError::Internal(format!("failed to look up volume: {}", e)))?
        .ok_or_else(|| BffError::NotFound(format!("volume {} not found", volume_id)))?;

        let meta = Some(self.build_meta(row.node_id.clone(), requested_by).await?);

        let ack = match action.as_str() {
            "attach" => {
                let target_vm_id = vm_id.unwrap_or_else(|| row.vm_id.unwrap_or_default());
                self.lifecycle_service
                    .attach_volume(proto::AttachVolumeRequest {
                        meta,
                        node_id: row.node_id.clone(),
                        volume: Some(proto::VolumeMutationSpec {
                            volume_id: volume_id.clone(),
                            vm_id: target_vm_id,
                            // #379 PR 2 (A9): the volume's class rides the
                            // attach mutation — the same producer shape as
                            // the orchestrator's A8 dispatch
                            // (`volume_attach_spec_json`): `{}` for a
                            // NULL-class volume (PR 3 correction — parses
                            // at the agent's A4 seam, still key-free),
                            // `{"backend_class": …}` when the volume
                            // carries a class. #533: the volume's KIND
                            // rides it too, so a standalone ('data')
                            // volume's attach opens at the #513 create
                            // carrier's `{volume_id}.img` locator — not
                            // the A4 bare-id default's second file.
                            volume_spec_json: crate::node_client::volume_attach_spec_json(
                                &volume_id,
                                row.storage_class.as_deref(),
                                row.volume_kind.as_deref(),
                            ),
                        }),
                    })
                    .await
            }
            "detach" => {
                // The explicit `vm_id` wins over the stored attachment, the
                // same precedence as attach: after a failed node dispatch the
                // CP has already cleared `attached_vm_id`, but stord's session
                // still names the VM, and only an explicit id can detach it
                // (#711 fifth review).
                let target_vm_id = vm_id.or_else(|| row.vm_id.clone()).unwrap_or_default();
                self.lifecycle_service
                    .detach_volume(proto::DetachVolumeRequest {
                        meta,
                        node_id: row.node_id.clone(),
                        vm_id: target_vm_id,
                        volume_id: volume_id.clone(),
                        force,
                    })
                    .await
            }
            "resize" => {
                let new_size = resize_bytes.ok_or_else(|| {
                    BffError::BadRequest("resize_bytes is required for resize action".into())
                })?;
                self.lifecycle_service
                    .resize_volume(proto::ResizeVolumeRequest {
                        meta,
                        node_id: row.node_id.clone(),
                        volume_id: volume_id.clone(),
                        new_size_bytes: new_size,
                    })
                    .await
            }
            _ => return Err(BffError::BadRequest(format!("invalid action: {}", action))),
        };

        let ack = Self::map_ack(ack)?;
        let result = ack
            .result
            .ok_or_else(|| BffError::Internal("missing ack result".into()))?;

        Ok(MutateVolumeResponse {
            accepted: result.status == "OK",
            task_id: result.operation_id,
            volume_id,
            summary: result.human_summary,
        })
    }

    async fn snapshot_volume(
        &self,
        volume_id: String,
        snapshot_name: String,
        requested_by: String,
    ) -> Result<MutateVolumeResponse, BffError> {
        let row = sqlx::query_as::<_, VolumeLookupRow>(
            r#"
            SELECT
                v.node_id as node_id,
                vds.attached_vm_id as vm_id,
                v.capacity_bytes as size_bytes,
                v.storage_class as storage_class,
                v.volume_kind as volume_kind
            FROM volumes v
            JOIN volume_desired_state vds ON v.volume_id = vds.volume_id
            WHERE v.volume_id = ?
            "#,
        )
        .bind(&volume_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| BffError::Internal(format!("failed to look up volume: {}", e)))?
        .ok_or_else(|| BffError::NotFound(format!("volume {} not found", volume_id)))?;

        let meta = Some(self.build_meta(row.node_id.clone(), requested_by).await?);
        let ack = self
            .lifecycle_service
            .snapshot_volume(proto::SnapshotVolumeRequest {
                meta,
                node_id: row.node_id.clone(),
                volume_id: volume_id.clone(),
                snapshot_name,
            })
            .await;

        let ack = Self::map_ack(ack)?;
        let result = ack
            .result
            .ok_or_else(|| BffError::Internal("missing ack result".into()))?;

        Ok(MutateVolumeResponse {
            accepted: result.status == "OK",
            task_id: result.operation_id,
            volume_id,
            summary: result.human_summary,
        })
    }

    async fn restore_volume_snapshot(
        &self,
        volume_id: String,
        snapshot_name: String,
        requested_by: String,
    ) -> Result<MutateVolumeResponse, BffError> {
        let row = sqlx::query_as::<_, VolumeLookupRow>(
            r#"
            SELECT
                v.node_id as node_id,
                vds.attached_vm_id as vm_id,
                v.capacity_bytes as size_bytes,
                v.storage_class as storage_class,
                v.volume_kind as volume_kind
            FROM volumes v
            JOIN volume_desired_state vds ON v.volume_id = vds.volume_id
            WHERE v.volume_id = ?
            "#,
        )
        .bind(&volume_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| BffError::Internal(format!("failed to look up volume: {}", e)))?
        .ok_or_else(|| BffError::NotFound(format!("volume {} not found", volume_id)))?;

        let meta = Some(self.build_meta(row.node_id.clone(), requested_by).await?);
        let ack = self
            .lifecycle_service
            .restore_volume(proto::RestoreVolumeRequest {
                meta,
                node_id: row.node_id.clone(),
                volume_id: volume_id.clone(),
                snapshot_name,
            })
            .await;

        let ack = Self::map_ack(ack)?;
        let result = ack
            .result
            .ok_or_else(|| BffError::Internal("missing ack result".into()))?;

        Ok(MutateVolumeResponse {
            accepted: result.status == "OK",
            task_id: result.operation_id,
            volume_id,
            summary: result.human_summary,
        })
    }

    async fn delete_volume_snapshot(
        &self,
        volume_id: String,
        snapshot_name: String,
        requested_by: String,
    ) -> Result<MutateVolumeResponse, BffError> {
        let row = sqlx::query_as::<_, VolumeLookupRow>(
            r#"
            SELECT
                v.node_id as node_id,
                vds.attached_vm_id as vm_id,
                v.capacity_bytes as size_bytes,
                v.storage_class as storage_class,
                v.volume_kind as volume_kind
            FROM volumes v
            JOIN volume_desired_state vds ON v.volume_id = vds.volume_id
            WHERE v.volume_id = ?
            "#,
        )
        .bind(&volume_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| BffError::Internal(format!("failed to look up volume: {}", e)))?
        .ok_or_else(|| BffError::NotFound(format!("volume {} not found", volume_id)))?;

        let meta = Some(self.build_meta(row.node_id.clone(), requested_by).await?);
        let ack = self
            .lifecycle_service
            .delete_volume_snapshot(proto::DeleteVolumeSnapshotRequest {
                meta,
                node_id: row.node_id.clone(),
                volume_id: volume_id.clone(),
                snapshot_name,
            })
            .await;

        let ack = Self::map_ack(ack)?;
        let result = ack
            .result
            .ok_or_else(|| BffError::Internal("missing ack result".into()))?;

        Ok(MutateVolumeResponse {
            accepted: result.status == "OK",
            task_id: result.operation_id,
            volume_id,
            summary: result.human_summary,
        })
    }

    async fn clone_volume(
        &self,
        source_volume_id: String,
        target_volume_id: String,
        requested_by: String,
    ) -> Result<MutateVolumeResponse, BffError> {
        let row = sqlx::query_as::<_, VolumeLookupRow>(
            r#"
            SELECT
                v.node_id as node_id,
                vds.attached_vm_id as vm_id,
                v.capacity_bytes as size_bytes,
                v.storage_class as storage_class,
                v.volume_kind as volume_kind
            FROM volumes v
            JOIN volume_desired_state vds ON v.volume_id = vds.volume_id
            WHERE v.volume_id = ?
            "#,
        )
        .bind(&source_volume_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| BffError::Internal(format!("failed to look up volume: {}", e)))?
        .ok_or_else(|| BffError::NotFound(format!("volume {} not found", source_volume_id)))?;

        let meta = Some(self.build_meta(row.node_id.clone(), requested_by).await?);
        let ack = self
            .lifecycle_service
            .clone_volume(proto::CloneVolumeRequest {
                meta,
                node_id: row.node_id.clone(),
                source_volume_id: source_volume_id.clone(),
                target_volume_id: target_volume_id.clone(),
            })
            .await;

        let ack = Self::map_ack(ack)?;
        let result = ack
            .result
            .ok_or_else(|| BffError::Internal("missing ack result".into()))?;

        Ok(MutateVolumeResponse {
            accepted: result.status == "OK",
            task_id: result.operation_id,
            volume_id: target_volume_id,
            summary: result.human_summary,
        })
    }

    async fn mutate_network(
        &self,
        network_id: String,
        action: String,
        force: bool,
        requested_by: String,
    ) -> Result<MutateNetworkResponse, BffError> {
        // #499: a tombstoned (deleted) network is NotFound here (the
        // NULL-safe DP9 predicate) — the mutation verbs (start/stop/
        // restart) must not reach a deleted network's lifecycle, and
        // the store's `set_network_status` guard backstops this
        // non-transactional read with a loud Conflict at the SQL level.
        // fetch_optional (not the historical fetch_one) so both a
        // missing and a tombstoned network take the clean 404 arm —
        // the old shape surfaced a missing network as a 500.
        let node_id = sqlx::query_scalar::<_, Option<String>>(
            r#"
            SELECT n.node_id FROM networks n
            LEFT JOIN network_desired_state nds ON nds.network_id = n.network_id
            WHERE n.network_id = ?
              AND (nds.desired_status IS NULL OR nds.desired_status != 'Deleting')
            "#,
        )
        .bind(&network_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| BffError::Internal(format!("failed to look up network: {}", e)))?
        .flatten()
        .ok_or_else(|| BffError::NotFound(format!("network {} not found", network_id)))?;

        let meta = Some(self.build_meta(node_id.clone(), requested_by).await?);

        let ack = match action.as_str() {
            "start" => {
                self.lifecycle_service
                    .start_network(proto::StartNetworkRequest {
                        meta,
                        node_id: node_id.clone(),
                        network_id: network_id.clone(),
                    })
                    .await
            }
            "stop" => {
                self.lifecycle_service
                    .stop_network(proto::StopNetworkRequest {
                        meta,
                        node_id: node_id.clone(),
                        network_id: network_id.clone(),
                        force,
                    })
                    .await
            }
            "restart" => {
                self.lifecycle_service
                    .restart_network(proto::RestartNetworkRequest {
                        meta,
                        node_id: node_id.clone(),
                        network_id: network_id.clone(),
                    })
                    .await
            }
            _ => return Err(BffError::BadRequest(format!("invalid action: {}", action))),
        };

        let ack = Self::map_ack(ack)?;
        let result = ack
            .result
            .ok_or_else(|| BffError::Internal("missing ack result".into()))?;

        Ok(MutateNetworkResponse {
            accepted: result.status == "OK",
            task_id: result.operation_id,
            network_id,
            summary: result.human_summary,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_client::volume_attach_spec_json;

    /// #379 PR 2 (A8/A9 shared producer): the attach spec_json is
    /// `{"backend_class": …}` — and ONLY that key — when the volume
    /// carries a class. A NULL class emits `{}` (PR 3 correction,
    /// disclosed): PR 2 kept the pre-#379 empty bytes but also disclosed
    /// that they fail the agent's A4 JSON parse (EOF on empty input);
    /// with DP5 attach dispatch real, the NULL-class leg must reach the
    /// open, so it is now the empty JSON object — no keys, no
    /// materialized `"local"`, the agent's B5 defaults apply. (#533
    /// added the standalone locator key on top — pinned in
    /// `volume_attach_spec_json_shapes_the_standalone_carrier_locator`
    /// and here for the embedded legs it must not disturb.)
    #[test]
    fn volume_attach_spec_json_carries_only_the_class() {
        assert_eq!(
            volume_attach_spec_json("vol-a9", Some("lvm"), None),
            br#"{"backend_class":"lvm"}"#.to_vec(),
            "a class-carrying embedded volume produces exactly the backend_class key"
        );
        assert_eq!(
            volume_attach_spec_json("vol-a9", None, None),
            b"{}".to_vec(),
            "a NULL-class embedded volume emits the empty JSON object so the agent's A4 parse succeeds and takes the defaults"
        );
    }

    /// Records every `AttachVolume` request and answers with the OK ack;
    /// every other lifecycle RPC is unreachable in these tests. Drives
    /// `ControlPlaneMutationService::mutate_volume` so the A9 leg is
    /// pinned at the same trait boundary production uses.
    struct RecordingLifecycle {
        attach_calls: std::sync::Arc<std::sync::Mutex<Vec<proto::AttachVolumeRequest>>>,
        detach_calls: std::sync::Arc<std::sync::Mutex<Vec<proto::DetachVolumeRequest>>>,
        migrate_calls: std::sync::Arc<std::sync::Mutex<Vec<proto::MigrateVmRequest>>>,
    }

    fn ok_ack(operation_id: String) -> proto::AckResponse {
        proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id,
                status: "OK".to_string(),
                node_observed_generation: "1".to_string(),
                error_code: String::new(),
                human_summary: "attach volume accepted".to_string(),
            }),
        }
    }

    #[tonic::async_trait]
    impl crate::lifecycle::LifecycleService for RecordingLifecycle {
        async fn create_vm(
            &self,
            _request: proto::CreateVmRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no create_vm in the A9 tests")
        }
        async fn start_vm(
            &self,
            _request: proto::StartVmRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no start_vm in the A9 tests")
        }
        async fn stop_vm(
            &self,
            _request: proto::StopVmRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no stop_vm in the A9 tests")
        }
        async fn reboot_vm(
            &self,
            _request: proto::RebootVmRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no reboot_vm in the A9 tests")
        }
        async fn delete_vm(
            &self,
            _request: proto::DeleteVmRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no delete_vm in the A9 tests")
        }
        async fn resize_vm(
            &self,
            _request: proto::ResizeVmRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no resize_vm in the A9 tests")
        }
        async fn attach_volume(
            &self,
            request: proto::AttachVolumeRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            let operation_id = request
                .meta
                .as_ref()
                .map(|m| m.operation_id.clone())
                .unwrap_or_default();
            self.attach_calls.lock().unwrap().push(request);
            Ok(ok_ack(operation_id))
        }
        async fn detach_volume(
            &self,
            request: proto::DetachVolumeRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            let operation_id = request
                .meta
                .as_ref()
                .map(|m| m.operation_id.clone())
                .unwrap_or_default();
            self.detach_calls.lock().unwrap().push(request);
            Ok(ok_ack(operation_id))
        }
        async fn resize_volume(
            &self,
            _request: proto::ResizeVolumeRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no resize_volume in the A9 tests")
        }
        async fn snapshot_volume(
            &self,
            _request: proto::SnapshotVolumeRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no snapshot_volume in the A9 tests")
        }
        async fn restore_volume(
            &self,
            _request: proto::RestoreVolumeRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no restore_volume in the A9 tests")
        }
        async fn delete_volume_snapshot(
            &self,
            _request: proto::DeleteVolumeSnapshotRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no delete_volume_snapshot in the A9 tests")
        }
        async fn clone_volume(
            &self,
            _request: proto::CloneVolumeRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no clone_volume in the A9 tests")
        }
        async fn pause_node_scheduling(
            &self,
            _request: proto::PauseNodeSchedulingRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no pause_node_scheduling in the A9 tests")
        }
        async fn resume_node_scheduling(
            &self,
            _request: proto::ResumeNodeSchedulingRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no resume_node_scheduling in the A9 tests")
        }
        async fn drain_node(
            &self,
            _request: proto::DrainNodeRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no drain_node in the A9 tests")
        }
        async fn enter_maintenance(
            &self,
            _request: proto::EnterMaintenanceRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no enter_maintenance in the A9 tests")
        }
        async fn exit_maintenance(
            &self,
            _request: proto::ExitMaintenanceRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no exit_maintenance in the A9 tests")
        }
        async fn pause_vm(
            &self,
            _request: proto::PauseVmRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no pause_vm in the A9 tests")
        }
        async fn resume_vm(
            &self,
            _request: proto::ResumeVmRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no resume_vm in the A9 tests")
        }
        async fn power_button_vm(
            &self,
            _request: proto::PowerButtonVmRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no power_button_vm in the A9 tests")
        }
        async fn add_disk(
            &self,
            _request: proto::AddDiskRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no add_disk in the A9 tests")
        }
        async fn remove_device(
            &self,
            _request: proto::RemoveDeviceRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no remove_device in the A9 tests")
        }
        async fn add_net(
            &self,
            _request: proto::AddNetRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no add_net in the A9 tests")
        }
        async fn resize_disk(
            &self,
            _request: proto::ResizeDiskRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no resize_disk in the A9 tests")
        }
        async fn snapshot_vm(
            &self,
            _request: proto::SnapshotVmRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no snapshot_vm in the A9 tests")
        }
        async fn restore_snapshot(
            &self,
            _request: proto::RestoreSnapshotRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no restore_snapshot in the A9 tests")
        }
        async fn coredump_vm(
            &self,
            _request: proto::CoredumpVmRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no coredump_vm in the A9 tests")
        }
        async fn start_network(
            &self,
            _request: proto::StartNetworkRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no start_network in the A9 tests")
        }
        async fn stop_network(
            &self,
            _request: proto::StopNetworkRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no stop_network in the A9 tests")
        }
        async fn restart_network(
            &self,
            _request: proto::RestartNetworkRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no restart_network in the A9 tests")
        }
        async fn migrate_vm(
            &self,
            request: proto::MigrateVmRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            let operation_id = request
                .meta
                .as_ref()
                .map(|m| m.operation_id.clone())
                .unwrap_or_default();
            self.migrate_calls.lock().unwrap().push(request);
            Ok(ok_ack(operation_id))
        }
        async fn update_overlay(
            &self,
            _request: proto::UpdateOverlayRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no update_overlay in the A9 tests")
        }
        async fn send_gratuitous_arp(
            &self,
            _request: proto::SendGratuitousArpRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no send_gratuitous_arp in the A9 tests")
        }
        async fn resolve_inspect_required_operation(
            &self,
            _request: proto::ResolveInspectRequiredOperationRequest,
        ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
            unreachable!("no resolve_inspect_required_operation in the A9 tests")
        }
    }

    /// Issue #656: the BFF mutation service rejects the live path at its
    /// own boundary — before the VM lookup and before dispatching
    /// `MigrateVm` — and never coerces `pause_first=false` to true.
    #[tokio::test]
    async fn migrate_vm_rejects_live_path_before_lookup_or_dispatch() {
        let pool = chv_controlplane_store::test_util::create_test_pool().await;
        let migrate_calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let service = ControlPlaneMutationService::new(
            pool,
            std::sync::Arc::new(RecordingLifecycle {
                attach_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                detach_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                migrate_calls: migrate_calls.clone(),
            }),
        );

        // No VM row exists: a lookup-first implementation would return
        // NotFound here. The gate must return BadRequest instead.
        let err = service
            .migrate_vm(
                "vm-does-not-exist".to_string(),
                "n-2".to_string(),
                false,
                "test-user".to_string(),
            )
            .await
            .expect_err("live migration must fail closed");
        match err {
            BffError::BadRequest(msg) => {
                assert!(
                    msg.contains("#394") && msg.contains("pause_first=true"),
                    "the rejection must explain the gate and remedy: {msg}"
                );
            }
            other => panic!("expected bad-request, got {other:?}"),
        }
        assert!(
            migrate_calls.lock().unwrap().is_empty(),
            "a rejected live request must not dispatch a lifecycle RPC"
        );
    }

    /// Issue #656: the stop-the-world path still threads
    /// `pause_first=true` through to the lifecycle request.
    #[tokio::test]
    async fn migrate_vm_forwards_pause_first_true() {
        let pool = chv_controlplane_store::test_util::create_test_pool().await;
        sqlx::query(
            "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-656', 'host', 'host')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO vms (vm_id, display_name, node_id) VALUES ('vm-656', 'vm-656', 'node-656')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let migrate_calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let service = ControlPlaneMutationService::new(
            pool,
            std::sync::Arc::new(RecordingLifecycle {
                attach_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                detach_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                migrate_calls: migrate_calls.clone(),
            }),
        );

        service
            .migrate_vm(
                "vm-656".to_string(),
                "node-656-b".to_string(),
                true,
                "test-user".to_string(),
            )
            .await
            .expect("pause-first migration must be accepted");

        let calls = migrate_calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "exactly one MigrateVm dispatch");
        let request = &calls[0];
        assert_eq!(request.vm_id, "vm-656");
        assert_eq!(request.source_node_id, "node-656");
        assert_eq!(request.destination_node_id, "node-656-b");
        assert!(
            request.config.as_ref().is_some_and(|c| c.pause_first),
            "the stop-the-world flag must be forwarded"
        );
    }

    /// #664: a restart after a clock rollback must not mint generations
    /// below rows already in the database. The mint reads the durable
    /// floor (here, a future-dated `vm_desired_state` row) and the next
    /// two mutations remain strictly above it and above each other.
    #[tokio::test]
    async fn generation_floor_is_seeded_from_durable_state() {
        // Larger than wall-clock milliseconds and comfortably inside i64.
        const SEEDED: i64 = 4_000_000_000_000_000;

        let pool = chv_controlplane_store::test_util::create_test_pool().await;
        sqlx::query(
            "INSERT INTO nodes (node_id, hostname, display_name) \
             VALUES ('node-floor', 'node-floor', 'node-floor')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO vms (vm_id, display_name, node_id) \
             VALUES ('vm-floor', 'vm-floor', 'node-floor')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO vm_desired_state (vm_id, desired_generation) VALUES ('vm-floor', ?)",
        )
        .bind(SEEDED)
        .execute(&pool)
        .await
        .unwrap();

        let migrate_calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let service = ControlPlaneMutationService::new(
            pool,
            std::sync::Arc::new(RecordingLifecycle {
                attach_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                detach_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                migrate_calls: migrate_calls.clone(),
            }),
        );

        for _ in 0..2 {
            service
                .migrate_vm(
                    "vm-floor".to_string(),
                    "node-floor-b".to_string(),
                    true,
                    "test-user".to_string(),
                )
                .await
                .expect("pause-first migration must be accepted");
        }

        let calls = migrate_calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        let generations: Vec<u64> = calls
            .iter()
            .map(|request| {
                request
                    .meta
                    .as_ref()
                    .expect("meta")
                    .desired_state_version
                    .parse()
                    .expect("numeric generation")
            })
            .collect();
        assert!(
            generations[0] > SEEDED as u64,
            "first mint {} must exceed the durable floor {SEEDED}",
            generations[0]
        );
        assert!(
            generations[1] > generations[0],
            "successive mints must strictly increase: {generations:?}"
        );
    }

    /// #379 PR 2 (A9): the BFF attach mutation populates
    /// `volume_spec_json` with the volume's parsed class — the same
    /// producer shape as the orchestrator's A8 dispatch — and emits the
    /// key-free `{}` for a NULL-class volume (PR 3 correction). #533
    /// adds the standalone leg: a `volume_kind = 'data'` volume's
    /// attach mutation carries the #513 create carrier's relative
    /// `{volume_id}.img` locator, so the BFF accept path and the
    /// orchestrator dispatch agree on the locator (the trait boundary
    /// production uses — the BFF's inline payload and the A8 dispatch
    /// are the same producer).
    #[tokio::test]
    async fn mutate_volume_attach_populates_spec_json_with_the_class() {
        let pool = chv_controlplane_store::test_util::create_test_pool().await;
        sqlx::query(
            "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-a9', 'host', 'host')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES ('vm-a9', 'vm-a9')")
            .execute(&pool)
            .await
            .unwrap();
        for (volume_id, class) in [("vol-a9-lvm", Some("lvm")), ("vol-a9-null", None)] {
            sqlx::query(
                "INSERT INTO volumes (volume_id, node_id, display_name, capacity_bytes, storage_class) \
                 VALUES (?, 'node-a9', ?, 1024, ?)",
            )
            .bind(volume_id)
            .bind(volume_id)
            .bind(class)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO volume_desired_state \
                 (volume_id, desired_generation, desired_status, attached_vm_id, read_only) \
                 VALUES (?, 1, 'Pending', 'vm-a9', 0)",
            )
            .bind(volume_id)
            .execute(&pool)
            .await
            .unwrap();
        }
        // #533: a standalone 'data' volume in the #513 route's own row
        // shape (NULL class, kind stamped) — the volume whose attach
        // used to mint the stray bare-id file.
        sqlx::query(
            "INSERT INTO volumes (volume_id, node_id, display_name, capacity_bytes, volume_kind) \
             VALUES ('vol-a9-std', 'node-a9', 'vol-a9-std', 1024, 'data')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO volume_desired_state \
             (volume_id, desired_generation, desired_status, attached_vm_id, read_only) \
             VALUES ('vol-a9-std', 1, 'Pending', NULL, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let attach_calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let service = ControlPlaneMutationService::new(
            pool,
            std::sync::Arc::new(RecordingLifecycle {
                attach_calls: attach_calls.clone(),
                detach_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                migrate_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            }),
        );

        for volume_id in ["vol-a9-lvm", "vol-a9-null", "vol-a9-std"] {
            service
                .mutate_volume(
                    volume_id.to_string(),
                    "attach".to_string(),
                    false,
                    None,
                    Some("vm-a9".to_string()),
                    "test-user".to_string(),
                )
                .await
                .expect("attach mutation must be accepted");
        }

        let calls = attach_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 3, "all attach mutations relayed: {calls:?}");
        let spec_json_for = |volume_id: &str| {
            calls
                .iter()
                .find(|c| c.volume.as_ref().map(|v| v.volume_id.as_str()) == Some(volume_id))
                .unwrap_or_else(|| panic!("no attach mutation for {volume_id}: {calls:?}"))
                .volume
                .clone()
                .unwrap()
                .volume_spec_json
        };
        assert_eq!(
            spec_json_for("vol-a9-lvm"),
            br#"{"backend_class":"lvm"}"#.to_vec(),
            "the attach mutation must carry the parsed class (A9)"
        );
        assert_eq!(
            spec_json_for("vol-a9-null"),
            b"{}".to_vec(),
            "a NULL-class volume must carry the empty JSON object (PR 3 correction: parseable at A4, key-free)"
        );
        assert_eq!(
            spec_json_for("vol-a9-std"),
            br#"{"locator":"vol-a9-std.img"}"#.to_vec(),
            "a standalone ('data') volume's attach mutation must carry the create carrier's relative locator (#533)"
        );
    }

    /// #711 fifth review: a detach mutation must honor an explicit `vm_id`
    /// over the stored attachment, like attach does. After a failed node
    /// dispatch the CP has already cleared `attached_vm_id` while stord's
    /// session still names the dead VM; without the explicit id the detach
    /// no-ops and the volume stays undestroyable.
    #[tokio::test]
    async fn mutate_volume_detach_prefers_an_explicit_vm_id() {
        let pool = chv_controlplane_store::test_util::create_test_pool().await;
        sqlx::query(
            "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-detach', 'host', 'host')",
        )
        .execute(&pool)
        .await
        .unwrap();
        for vm_id in ["vm-stored", "vm-explicit"] {
            sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES (?, ?)")
                .bind(vm_id)
                .bind(vm_id)
                .execute(&pool)
                .await
                .unwrap();
        }
        for (volume_id, attached) in [
            ("vol-detach-stored", Some("vm-stored")),
            ("vol-detach-explicit", None),
        ] {
            sqlx::query(
                "INSERT INTO volumes (volume_id, node_id, display_name, capacity_bytes) \
                 VALUES (?, 'node-detach', ?, 1024)",
            )
            .bind(volume_id)
            .bind(volume_id)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO volume_desired_state \
                 (volume_id, desired_generation, desired_status, attached_vm_id, read_only) \
                 VALUES (?, 1, 'Active', ?, 0)",
            )
            .bind(volume_id)
            .bind(attached)
            .execute(&pool)
            .await
            .unwrap();
        }

        let detach_calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let service = ControlPlaneMutationService::new(
            pool,
            std::sync::Arc::new(RecordingLifecycle {
                attach_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                detach_calls: detach_calls.clone(),
                migrate_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            }),
        );

        service
            .mutate_volume(
                "vol-detach-stored".to_string(),
                "detach".to_string(),
                false,
                None,
                None,
                "test-user".to_string(),
            )
            .await
            .expect("detach with the stored attachment must be accepted");
        service
            .mutate_volume(
                "vol-detach-explicit".to_string(),
                "detach".to_string(),
                false,
                None,
                Some("vm-explicit".to_string()),
                "test-user".to_string(),
            )
            .await
            .expect("detach with an explicit vm_id must be accepted");

        let calls = detach_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2, "both detaches relayed: {calls:?}");
        let vm_for = |volume_id: &str| {
            calls
                .iter()
                .find(|c| c.volume_id == volume_id)
                .unwrap_or_else(|| panic!("no detach mutation for {volume_id}: {calls:?}"))
                .vm_id
                .clone()
        };
        assert_eq!(
            vm_for("vol-detach-stored"),
            "vm-stored",
            "no explicit id keeps the stored attachment"
        );
        assert_eq!(
            vm_for("vol-detach-explicit"),
            "vm-explicit",
            "an explicit id wins even when the CP has cleared the stored attachment"
        );
    }

    /// #379 DP4 BFF surface: a storage-class the volume's node does not
    /// offer rejects at the lifecycle's accept time (before journaling)
    /// and surfaces at the BFF tier as `BffError::BadRequest` — HTTP
    /// 400, the same `map_ack` contract as the #495 mode rejections —
    /// instead of a 200-accepted operation that burns dispatch retries.
    /// Drives the REAL lifecycle service (not RecordingLifecycle) so the
    /// whole accept-time path is exercised.
    #[tokio::test]
    async fn mutate_volume_attach_maps_storage_class_rejection_to_bad_request() {
        use crate::lifecycle::LifecycleServiceImplementation;

        let pool = chv_controlplane_store::test_util::create_test_pool().await;
        sqlx::query(
            "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-dp4-bff', 'host', 'host')",
        )
        .execute(&pool)
        .await
        .unwrap();
        // The node reports a local-only stord (PR 3's inventory: the
        // REAL backend class); the volume carries ceph.
        sqlx::query(
            "INSERT INTO node_inventory (node_id, architecture, cpu_count, memory_bytes, storage_classes) \
             VALUES ('node-dp4-bff', 'x86_64', 1, 1024, '[\"local\"]')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO volumes (volume_id, node_id, display_name, capacity_bytes, storage_class) \
             VALUES ('vol-dp4-bff', 'node-dp4-bff', 'vol-dp4-bff', 1024, 'ceph')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO volume_desired_state \
             (volume_id, desired_generation, desired_status, attached_vm_id, read_only) \
             VALUES ('vol-dp4-bff', 1, 'Pending', NULL, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let service = ControlPlaneMutationService::new(
            pool.clone(),
            std::sync::Arc::new(LifecycleServiceImplementation::new(
                chv_controlplane_store::NodeRepository::new(pool.clone()),
                chv_controlplane_store::OperationRepository::new(pool.clone()),
                chv_controlplane_store::EventRepository::new(pool.clone()),
                chv_controlplane_store::DesiredStateRepository::new(pool.clone()),
            )),
        );

        match service
            .mutate_volume(
                "vol-dp4-bff".to_string(),
                "attach".to_string(),
                false,
                None,
                Some("vm-dp4-bff".to_string()),
                "test-user".to_string(),
            )
            .await
        {
            Err(BffError::BadRequest(msg)) => {
                assert!(
                    msg.contains("does not offer storage class ceph"),
                    "got: {msg}"
                );
            }
            other => panic!("expected bad-request, got {other:?}"),
        }
        // The rejection journaled nothing.
        let ops: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM operations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(ops, 0, "a rejected attach must not journal an operation");
        let attached: Option<String> = sqlx::query_scalar(
            "SELECT attached_vm_id FROM volume_desired_state WHERE volume_id = 'vol-dp4-bff'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(attached, None, "no attachment intent may be written");
    }

    /// #384: the clone race loser surfaces as `Conflict` at the BFF
    /// tier. The control plane's Conflict class (gRPC `ALREADY_EXISTS`)
    /// must map to `BffError::Conflict` — HTTP 409 — not to the generic
    /// internal-error arm, and not to the pre-check's BadRequest (400).
    #[test]
    fn map_ack_conflict_maps_to_bff_conflict() {
        let err = ControlPlaneServiceError::Conflict(
            "volume 'vol-dst': target volume id already materialized by a concurrent request"
                .into(),
        );
        match ControlPlaneMutationService::map_ack(Err(err)) {
            Err(BffError::Conflict(msg)) => {
                assert!(msg.contains("vol-dst"), "got: {msg}");
            }
            other => panic!("expected BffError::Conflict, got {other:?}"),
        }
    }

    /// The pre-check's `InvalidArgument` keeps the 400 contract — the
    /// two paths stay distinguishable.
    #[test]
    fn map_ack_invalid_argument_maps_to_bad_request() {
        let err = ControlPlaneServiceError::InvalidArgument(
            "target volume id already exists: vol-dst".into(),
        );
        match ControlPlaneMutationService::map_ack(Err(err)) {
            Err(BffError::BadRequest(msg)) => {
                assert!(msg.contains("already exists"), "got: {msg}");
            }
            other => panic!("expected BffError::BadRequest, got {other:?}"),
        }
    }
}
