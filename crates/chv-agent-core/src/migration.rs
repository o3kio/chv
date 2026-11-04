//! Agent-side migration coordination.
//!
//! This module handles live VM migration at the agent level. When the control plane
//! dispatches a `MigrateVm` RPC to an agent, the agent determines its role (source or
//! destination) based on matching its own node_id against `source_node_id` /
//! `destination_node_id` in the request and executes the appropriate Cloud Hypervisor
//! REST API calls.

use crate::daemon_clients::StordClient;
use crate::migration_registry::TransferWindow;
use crate::vm_runtime::VmRuntime;
use chv_errors::ChvError;
use control_plane_node_api::control_plane_node_api as proto;
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// Port range for migration receiver sockets.
const MIGRATION_PORT_RANGE_START: u16 = 49152;
const MIGRATION_PORT_RANGE_END: u16 = 49200;

/// Bounded retry for the destination's transient admission rejection
/// (`RESOURCE_EXHAUSTED`). The authorization token is deliberately NOT
/// consumed by a rejected admission, so retrying the same trigger is safe.
/// Four attempts with 250 ms/500 ms/1 s backoff ride out a short burst of
/// competing migrations without hiding a genuinely full destination.
const TRIGGER_ADMISSION_RETRY_ATTEMPTS: u32 = 4;
const TRIGGER_ADMISSION_RETRY_BASE_DELAY: std::time::Duration =
    std::time::Duration::from_millis(250);
const TRIGGER_ADMISSION_RETRY_MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

/// The role this agent plays in a migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationRole {
    Source,
    Destination,
}

/// Determine the role of this agent in a migration request.
pub fn determine_role(
    my_node_id: &str,
    source_node_id: &str,
    destination_node_id: &str,
) -> Option<MigrationRole> {
    if my_node_id == source_node_id {
        Some(MigrationRole::Source)
    } else if my_node_id == destination_node_id {
        Some(MigrationRole::Destination)
    } else {
        None
    }
}

/// Allocate a port from the migration port range (49152-49200).
/// Returns the bound TcpListener which holds the port reservation.
/// The caller should extract the port with `.local_addr()` and pass the
/// listener or drop it only when ready to bind the actual migration socket.
pub fn allocate_migration_port() -> Result<(u16, TcpListener), &'static str> {
    for port in MIGRATION_PORT_RANGE_START..=MIGRATION_PORT_RANGE_END {
        if let Ok(listener) = TcpListener::bind(("0.0.0.0", port)) {
            return Ok((port, listener));
        }
    }
    Err("no available migration port in range 49152-49200")
}

/// Build a receiver URL for the destination agent.
pub fn build_receiver_url(port: u16) -> String {
    format!("tcp://0.0.0.0:{}", port)
}

/// Build a destination URL for the source agent to connect to.
pub fn build_destination_url(dest_host: &str, port: u16) -> String {
    format!("tcp://{}:{}", dest_host, port)
}

/// Extract the destination host address from the MigrateVmRequest.
/// The control plane populates the `destination_node_id` which maps to a known
/// agent address. For now, we expect the meta.target_node_id to encode enough
/// information, or we derive it from the request fields.
///
/// In the current architecture, the CP orchestrator sends MigrateVm to both
/// source and dest with the full request. The destination host address is
/// embedded in the request by the CP as part of the config or meta context.
/// For simplicity, we extract it from a convention: the destination_node_id
/// can serve as a DNS-resolvable hostname, or the CP will populate a
/// `destination_address` field in the config.
pub fn extract_destination_host(req: &proto::MigrateVmRequest) -> String {
    // The destination_node_id is typically a hostname or IP that the source
    // can reach. In production, the CP would resolve node_id -> agent_address.
    // For now, we use destination_node_id directly as the host.
    req.destination_node_id.clone()
}

/// Execute migration as the destination agent.
/// This spawns a background task that calls CH receive-migration.
pub fn spawn_destination_migration(
    vm_runtime: VmRuntime,
    vm_id: String,
    operation_id: String,
    receiver_url: String,
    transfer: Arc<TransferWindow>,
) -> tokio::task::JoinHandle<Result<(), String>> {
    tokio::spawn(async move {
        info!(
            vm_id = %vm_id,
            receiver_url = %receiver_url,
            operation_id = %operation_id,
            "destination agent: starting receive-migration"
        );

        // Enter the non-cancellable window atomically: if a cancel was armed
        // first, do not start the VMM receive at all (dropping this future
        // would not stop it).
        if !transfer.try_begin() {
            warn!(
                vm_id = %vm_id,
                operation_id = %operation_id,
                "destination agent: migration cancelled before receive-migration"
            );
            return Err("migration cancelled before receive-migration".to_string());
        }
        let result = vm_runtime
            .receive_migration(&vm_id, &receiver_url, Some(&operation_id))
            .await;
        match &result {
            // The agent stopped waiting, or lost the VMM reply, while Cloud
            // Hypervisor may still be receiving. Seal the window so a cancel
            // can never be confirmed.
            Err(e) if is_outcome_unknown(e) => transfer.seal_unknown(),
            _ => transfer.end(),
        }

        match result {
            Ok(()) => {
                info!(
                    vm_id = %vm_id,
                    operation_id = %operation_id,
                    "destination agent: receive-migration completed, VM is now running"
                );
                Ok(())
            }
            Err(e) => {
                error!(
                    vm_id = %vm_id,
                    operation_id = %operation_id,
                    error = %e,
                    "destination agent: receive-migration failed"
                );
                Err(e.to_string())
            }
        }
    })
}

/// A volume descriptor for disk pre-copy migration.
///
/// Each volume attached to a VM needs its data migrated to the destination node
/// before the memory migration can begin. This struct identifies a volume and its
/// attachment handle so the stord can locate and stream the correct data.
#[derive(Debug, Clone)]
pub struct MigrationVolume {
    /// The unique volume identifier (e.g., "vol-abc123").
    pub volume_id: String,
    /// The attachment handle returned when the volume was opened/attached.
    pub attachment_handle: String,
}

/// Configuration for disk pre-copy migration phase.
///
/// Encapsulates the stord connection details and volume list needed to coordinate
/// disk migration before memory migration begins.
#[derive(Debug, Clone)]
pub struct DiskPrecopyConfig {
    /// Path to the local stord Unix socket.
    pub stord_socket: std::path::PathBuf,
    /// The gRPC endpoint of the destination stord's migration service
    /// (e.g., `http://dest-host:50052`).
    pub dest_stord_endpoint: String,
    /// Volumes to migrate.
    pub volumes: Vec<MigrationVolume>,
    /// Control-plane-issued migration authorizations keyed by volume id
    /// (issue #663). The agent forwards each token to stord's
    /// TriggerDiskMigration; the destination verifies it before creating
    /// the receiving volume. A volume with no matching token fails before
    /// the trigger rather than attempting an unauthorized migration.
    pub volume_authorizations: std::collections::HashMap<String, String>,
    /// Explicit development-only tokenless dispatch marker. The control
    /// plane sets this only when it has opted into
    /// `allow_insecure_unauthenticated`; the agent honors it only when its
    /// own `allow_insecure_migration_dispatch` gate is also enabled.
    /// Production deployments leave both false, so a missing per-volume
    /// token is always refused before the trigger.
    pub allow_insecure_unauthenticated: bool,
    /// Issue #394 Option C: opt-in stop-the-world mode — the VM is paused
    /// before stord's bulk copy begins (the PAUSED_PRE_COPY handshake)
    /// instead of only at final sync. Downtime equals the full transfer;
    /// correctness by construction.
    pub pause_first: bool,
}

/// A callback type for reporting migration progress to the control plane.
///
/// When provided to `source_migration_with_disk_precopy`, this is called
/// after each disk sync round with the current dirty_blocks_remaining count,
/// enabling the control plane to track convergence.
pub type ProgressReporter = Arc<Mutex<dyn FnMut(proto::MigrationProgress) + Send>>;

/// Build a progress reporter closure that sends updates to the control plane.
///
/// This creates a `ProgressReporter` wrapping a control plane client reference,
/// suitable for passing to `source_migration_with_disk_precopy`.
pub fn make_progress_reporter<F>(callback: F) -> ProgressReporter
where
    F: FnMut(proto::MigrationProgress) + Send + 'static,
{
    Arc::new(Mutex::new(callback))
}

/// Tracks whether this migration paused the source VM so that every
/// post-pause failure path can best-effort resume it before returning the
/// error.
///
/// A VM left paused by a failed migration would otherwise never recover: the
/// reconciler's `start_vm` treats `Paused` as already-running, so this guard
/// is the single owner of resume-after-pause. The resume here is best-effort —
/// a failed resume is logged and the caller still surfaces the original
/// migration error.
struct PausedVmGuard {
    vm_runtime: VmRuntime,
    vm_id: String,
    operation_id: String,
    paused: bool,
}

impl PausedVmGuard {
    fn new(vm_runtime: VmRuntime, vm_id: String, operation_id: String) -> Self {
        Self {
            vm_runtime,
            vm_id,
            operation_id,
            paused: false,
        }
    }

    fn is_paused(&self) -> bool {
        self.paused
    }

    /// Pause the VM once and remember it so `resume_if_paused` can undo it.
    async fn pause_vm(&mut self) -> Result<(), String> {
        if self.paused {
            return Ok(());
        }
        if let Err(e) = self
            .vm_runtime
            .pause_vm(&self.vm_id, Some(&self.operation_id))
            .await
        {
            return Err(format!("failed to pause VM for final disk sync: {e}"));
        }
        self.paused = true;
        Ok(())
    }

    /// Best-effort resume, at most once per pause. Never fails: callers must
    /// surface the original error; a failed resume is logged as a warning.
    async fn resume_if_paused(&mut self) {
        if !self.paused {
            return;
        }
        self.paused = false;
        if let Err(e) = self
            .vm_runtime
            .resume_vm(&self.vm_id, Some(&self.operation_id))
            .await
        {
            warn!(
                vm_id = %self.vm_id,
                operation_id = %self.operation_id,
                error = %e,
                "best-effort VM resume after migration failure failed; the VM may remain paused and require a manual resume"
            );
        }
    }
}

/// Seam over the two stord disk-migration calls the pause/final-sync poll
/// loop needs, so the pause-recovery behavior can be tested with a stub.
#[async_trait::async_trait]
trait DiskMigrationControl: Send {
    async fn get_status(
        &mut self,
        migration_id: &str,
    ) -> Result<chv_stord_api::chv_stord_api::GetDiskMigrationStatusResponse, String>;

    async fn resume(&mut self, migration_id: &str, vm_paused: bool) -> Result<(), String>;
}

#[async_trait::async_trait]
impl DiskMigrationControl for StordClient {
    async fn get_status(
        &mut self,
        migration_id: &str,
    ) -> Result<chv_stord_api::chv_stord_api::GetDiskMigrationStatusResponse, String> {
        self.get_disk_migration_status(migration_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn resume(&mut self, migration_id: &str, vm_paused: bool) -> Result<(), String> {
        self.resume_disk_migration(migration_id, vm_paused)
            .await
            .map_err(|e| e.to_string())
    }
}

/// Poll all volume migrations until they converge or fail, pausing the VM for
/// the final sync when stord signals it.
///
/// Every failure path after the pause best-effort resumes the VM (via
/// `pause_guard`) before returning the original error, so a failed or
/// cancelled migration never leaves the VM paused forever.
#[allow(clippy::too_many_arguments)]
async fn run_disk_precopy_poll(
    client: &mut impl DiskMigrationControl,
    volume_migrations: &[(String, String)],
    poll_interval: std::time::Duration,
    pause_first: bool,
    cancel_token: &CancellationToken,
    pause_guard: &mut PausedVmGuard,
    progress_reporter: Option<&ProgressReporter>,
    node_id: &str,
    vm_id: &str,
    operation_id: &str,
) -> Result<(), String> {
    loop {
        // Phase boundary 3: each poll iteration. Use select! so a cancel
        // signal short-circuits the sleep instead of waiting up to 5s.
        tokio::select! {
            _ = cancel_token.cancelled() => {
                warn!(
                    vm_id = %vm_id,
                    operation_id = %operation_id,
                    "source agent: migration cancelled during disk-precopy polling"
                );
                pause_guard.resume_if_paused().await;
                return Err("migration cancelled at phase: disk-precopy-poll".to_string());
            }
            _ = tokio::time::sleep(poll_interval) => {}
        }

        let mut all_completed = true;
        let mut any_failed = false;
        let mut total_bytes_transferred: u64 = 0;
        let mut total_bytes: u64 = 0;
        let mut max_dirty_remaining: u64 = 0;
        let mut max_round: u32 = 0;
        let mut needs_vm_pause = false;

        for (_vol_id, mig_id) in volume_migrations {
            let status = match client.get_status(mig_id).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(
                        vm_id = %vm_id,
                        operation_id = %operation_id,
                        migration_id = %mig_id,
                        error = %e,
                        "failed to query disk migration status"
                    );
                    all_completed = false;
                    continue;
                }
            };

            if let Some(ref result) = status.result {
                if !result.status.eq_ignore_ascii_case("ok") {
                    error!(
                        vm_id = %vm_id,
                        operation_id = %operation_id,
                        migration_id = %mig_id,
                        error = %result.human_summary,
                        "disk migration returned error status"
                    );
                    any_failed = true;
                    break;
                }
            }

            total_bytes_transferred += status.bytes_transferred;
            total_bytes += status.total_bytes;
            max_dirty_remaining = max_dirty_remaining.max(status.dirty_blocks_remaining);
            max_round = max_round.max(status.convergence_round);

            use chv_stord_api::chv_stord_api::get_disk_migration_status_response::Phase as StordPhase;
            let phase = StordPhase::try_from(status.phase).unwrap_or(StordPhase::Pending);

            match phase {
                StordPhase::Pending | StordPhase::BulkCopy | StordPhase::DirtySync => {
                    all_completed = false;
                }
                StordPhase::PausedPreCopy => {
                    // Issue #394 Option C: the volume is waiting for the
                    // VM pause BEFORE bulk copy. Same handshake as the
                    // final-sync pause: pause the VM, then resume the
                    // migration (which arms the canary and starts the
                    // transfer against the quiesced source).
                    all_completed = false;
                    needs_vm_pause = true;
                }
                StordPhase::PausedFinalSync => {
                    all_completed = false;
                    needs_vm_pause = true;
                }
                StordPhase::Completed => {
                    // volume done
                }
                StordPhase::Failed => {
                    any_failed = true;
                    error!(
                        vm_id = %vm_id,
                        operation_id = %operation_id,
                        migration_id = %mig_id,
                        error = %status.error_message,
                        "disk migration failed"
                    );
                }
            }
        }

        if any_failed {
            pause_guard.resume_if_paused().await;
            return Err("disk migration failed for one or more volumes".to_string());
        }

        // Report progress to control plane
        if let Some(reporter) = progress_reporter {
            let proto_phase = if all_completed {
                proto::MigrationPhase::MemoryMigration
            } else if needs_vm_pause && !pause_first {
                // Final-sync pause in the default mode: disk work is done,
                // memory migration is next.
                proto::MigrationPhase::MemoryMigration
            } else if needs_vm_pause && pause_first {
                // Pause-first mode: the pause is part of the disk phase
                // (before bulk copy or at final sync) — still disk work.
                proto::MigrationPhase::PrecopyDisk
            } else if max_round > 0 {
                proto::MigrationPhase::ConvergingDisk
            } else {
                proto::MigrationPhase::PrecopyDisk
            };
            let progress_pct = if all_completed {
                50.0
            } else {
                let ratio = if total_bytes > 0 {
                    (total_bytes_transferred as f32 / total_bytes as f32) * 50.0
                } else {
                    0.0
                };
                ratio.min(45.0) // cap at 45% until all disk work is done
            };

            let progress = build_progress(
                node_id,
                vm_id,
                operation_id,
                proto_phase,
                total_bytes_transferred,
                total_bytes,
                max_round,
                if needs_vm_pause || all_completed {
                    0
                } else {
                    max_dirty_remaining
                },
                progress_pct,
            );
            let mut cb = reporter.lock().await;
            cb(progress);
        }

        if all_completed {
            info!(
                vm_id = %vm_id,
                operation_id = %operation_id,
                "source agent: all disk migrations completed"
            );
            return Ok(());
        }

        if needs_vm_pause && !pause_guard.is_paused() {
            // Phase boundary 4: before pausing the VM for final sync.
            if cancel_token.is_cancelled() {
                warn!(
                    vm_id = %vm_id,
                    operation_id = %operation_id,
                    phase = "pre-vm-pause",
                    "source agent: migration cancelled at phase boundary"
                );
                return Err("migration cancelled at phase: pre-vm-pause".to_string());
            }

            info!(
                vm_id = %vm_id,
                operation_id = %operation_id,
                pause_reason = if pause_first { "pre-copy" } else { "final-sync" },
                "source agent: pausing VM for disk migration pause handshake"
            );
            if let Err(e) = pause_guard.pause_vm().await {
                error!(
                    vm_id = %vm_id,
                    operation_id = %operation_id,
                    error = %e,
                    "source agent: failed to pause VM for disk migration pause handshake"
                );
                return Err(e);
            }

            for (_vol_id, mig_id) in volume_migrations {
                if let Err(e) = client.resume(mig_id, true).await {
                    error!(
                        vm_id = %vm_id,
                        operation_id = %operation_id,
                        migration_id = %mig_id,
                        error = %e,
                        "source agent: failed to resume disk migration after VM pause"
                    );
                    pause_guard.resume_if_paused().await;
                    return Err(format!("failed to resume disk migration {mig_id}: {e}"));
                }
            }

            info!(
                vm_id = %vm_id,
                operation_id = %operation_id,
                "source agent: VM paused, resumed all disk migrations"
            );
        }
    }
}

/// Bounded exponential backoff for the admission retry loop. Attempt 0
/// fails over after 250 ms, then 500 ms, then 1 s; capped at 2 s.
fn trigger_admission_retry_delay(attempt: u32) -> std::time::Duration {
    let factor = 1u32 << attempt.min(4);
    (TRIGGER_ADMISSION_RETRY_BASE_DELAY * factor).min(TRIGGER_ADMISSION_RETRY_MAX_DELAY)
}

/// Select the authorization token for one volume.
///
/// The empty-token fallback exists **only** for the explicit, double-gated
/// development mode (control-plane marker + agent config gate). Otherwise a
/// missing token is a hard refusal: the agent must never let stord fall back
/// to an unauthenticated receiver because the control plane is old or buggy.
fn migration_authorization_for_volume(
    volume_id: &str,
    authorizations: &std::collections::HashMap<String, String>,
    allow_insecure_unauthenticated: bool,
) -> Result<String, String> {
    match authorizations.get(volume_id) {
        Some(token) if !token.is_empty() => Ok(token.clone()),
        _ if allow_insecure_unauthenticated => Ok(String::new()),
        _ => Err(format!(
            "disk pre-copy refused for volume {volume_id}: no control-plane migration \
             authorization was supplied (the control plane must mint one per volume)"
        )),
    }
}

/// Trigger one volume's disk migration, retrying only the transient
/// `RESOURCE_EXHAUSTED` admission rejection.
///
/// Safe by construction: destination admission runs before the nonce is
/// consumed, so a rejected attempt leaves the token usable. Any other error
/// (auth, transport, not-found) fails immediately.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn trigger_disk_migration_with_admission_retry(
    stord_client: &mut StordClient,
    volume: &MigrationVolume,
    dest_endpoint: &str,
    migration_authorization: &str,
    pause_first: bool,
    operation_id: &str,
    cancel_token: &CancellationToken,
) -> Result<String, String> {
    let mut attempt = 0u32;
    loop {
        match stord_client
            .trigger_disk_migration(
                &volume.volume_id,
                &volume.attachment_handle,
                dest_endpoint,
                migration_authorization,
                pause_first,
                Some(operation_id),
            )
            .await
        {
            Ok(id) => return Ok(id),
            Err(ChvError::ResourceExhausted { reason })
                if attempt + 1 < TRIGGER_ADMISSION_RETRY_ATTEMPTS =>
            {
                let delay = trigger_admission_retry_delay(attempt);
                warn!(
                    volume_id = %volume.volume_id,
                    attempt = attempt + 1,
                    max_attempts = TRIGGER_ADMISSION_RETRY_ATTEMPTS,
                    retry_in_ms = delay.as_millis() as u64,
                    %reason,
                    "source agent: destination migration admission is full; retrying"
                );
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        return Err(format!(
                            "disk pre-copy trigger for volume {} cancelled while waiting for \
                             destination migration capacity",
                            volume.volume_id
                        ));
                    }
                    _ = tokio::time::sleep(delay) => {}
                }
                attempt += 1;
            }
            Err(ChvError::ResourceExhausted { reason }) => {
                return Err(format!(
                    "destination migration admission is exhausted after {} attempt(s) for volume \
                     {}: {reason}. The destination node's max_inflight_receiving / \
                     max_inflight_receiving_bytes ceiling is full; no authorization token was \
                     consumed, so retry after capacity frees or raise the destination limits",
                    TRIGGER_ADMISSION_RETRY_ATTEMPTS, volume.volume_id
                ));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// Run a full source-side migration with disk pre-copy.
///
/// This is the cancel-aware async core. The `cancel_token` is consulted at every **phase boundary**
/// — before connecting to stord, before each volume's pre-copy trigger, on
/// every poll iteration, before pausing the VM, and before the memory
/// migration phase — matching the retro pattern in
/// `cancel-signal-recheck-after-phase.md`. If cancellation is observed, the
/// function returns `Err("migration cancelled at phase: …")` so the agent
/// can surface the cancel back to the control plane.
///
/// Once the VM has been paused for the final disk sync, every failure or
/// cancellation path best-effort resumes it before returning the error, so a
/// failed migration never leaves the VM paused forever.
#[allow(clippy::too_many_arguments)]
pub async fn source_migration_with_disk_precopy(
    vm_runtime: VmRuntime,
    vm_id: String,
    operation_id: String,
    node_id: String,
    destination_url: String,
    disk_config: DiskPrecopyConfig,
    progress_reporter: Option<ProgressReporter>,
    cancel_token: CancellationToken,
    transfer: Arc<TransferWindow>,
) -> Result<(), String> {
    info!(
        vm_id = %vm_id,
        destination_url = %destination_url,
        operation_id = %operation_id,
        volume_count = disk_config.volumes.len(),
        dest_stord_endpoint = %disk_config.dest_stord_endpoint,
        "source agent: starting migration with disk pre-copy"
    );

    let mut pause_guard =
        PausedVmGuard::new(vm_runtime.clone(), vm_id.clone(), operation_id.clone());

    macro_rules! check_cancel {
        ($phase:expr) => {
            if cancel_token.is_cancelled() {
                warn!(
                    vm_id = %vm_id,
                    operation_id = %operation_id,
                    phase = $phase,
                    "source agent: migration cancelled at phase boundary"
                );
                return Err(format!("migration cancelled at phase: {}", $phase));
            }
        };
    }

    // Phase boundary 1: before stord connect.
    check_cancel!("pre-stord-connect");

    // Phase 1: Disk pre-copy
    let mut stord_client = match StordClient::connect(Path::new(&disk_config.stord_socket)).await {
        Ok(client) => client,
        Err(e) => {
            error!(
                vm_id = %vm_id,
                operation_id = %operation_id,
                error = %e,
                "source agent: failed to connect to local stord for disk pre-copy"
            );
            return Err(format!(
                "disk pre-copy failed: cannot connect to stord: {e}"
            ));
        }
    };

    let mut volume_migrations: Vec<(String, String)> = Vec::new();
    // (volume_id, migration_id)

    if !disk_config.volumes.is_empty() {
        info!(
            vm_id = %vm_id,
            operation_id = %operation_id,
            "source agent: beginning disk pre-copy phase"
        );

        for volume in &disk_config.volumes {
            // Phase boundary 2: before each volume's pre-copy trigger.
            check_cancel!("pre-disk-trigger");

            // Issue #663: never trigger an unauthorized migration. Missing
            // authorization is a control-plane/version-skew error, not a
            // reason to let stord fall back to the old unauthenticated path.
            // The only exception is the explicit development-only marker,
            // honored only after both the CP and agent gates are on.
            let migration_authorization = migration_authorization_for_volume(
                &volume.volume_id,
                &disk_config.volume_authorizations,
                disk_config.allow_insecure_unauthenticated,
            )?;

            info!(
                vm_id = %vm_id,
                operation_id = %operation_id,
                volume_id = %volume.volume_id,
                attachment_handle = %volume.attachment_handle,
                "source agent: triggering disk pre-copy for volume"
            );

            let migration_id = match trigger_disk_migration_with_admission_retry(
                &mut stord_client,
                volume,
                &disk_config.dest_stord_endpoint,
                &migration_authorization,
                disk_config.pause_first,
                &operation_id,
                &cancel_token,
            )
            .await
            {
                Ok(id) => id,
                Err(e) => {
                    error!(
                        vm_id = %vm_id,
                        operation_id = %operation_id,
                        volume_id = %volume.volume_id,
                        error = %e,
                        "source agent: disk pre-copy trigger failed for volume"
                    );
                    return Err(format!(
                        "disk pre-copy trigger failed for volume {}: {e}",
                        volume.volume_id
                    ));
                }
            };

            volume_migrations.push((volume.volume_id.clone(), migration_id));

            info!(
                vm_id = %vm_id,
                operation_id = %operation_id,
                volume_id = %volume.volume_id,
                migration_id = %volume_migrations.last().unwrap().1,
                "source agent: disk migration triggered for volume"
            );
        }

        // Poll all volume migrations until they converge or fail. The helper
        // drives the pause/resume handshake with stord and best-effort
        // resumes the VM on every post-pause failure path.
        run_disk_precopy_poll(
            &mut stord_client,
            &volume_migrations,
            std::time::Duration::from_secs(5),
            disk_config.pause_first,
            &cancel_token,
            &mut pause_guard,
            progress_reporter.as_ref(),
            &node_id,
            &vm_id,
            &operation_id,
        )
        .await?;
    } else {
        info!(
            vm_id = %vm_id,
            operation_id = %operation_id,
            "source agent: no volumes to migrate, skipping disk pre-copy"
        );
    }

    // Phase boundary 5: before memory migration begins. The VM may already be
    // paused for the final disk sync at this point, so best-effort resume it
    // before surfacing the cancellation.
    if cancel_token.is_cancelled() {
        warn!(
            vm_id = %vm_id,
            operation_id = %operation_id,
            phase = "pre-memory-migration",
            "source agent: migration cancelled at phase boundary"
        );
        pause_guard.resume_if_paused().await;
        return Err("migration cancelled at phase: pre-memory-migration".to_string());
    }

    send_memory_migration(
        &vm_runtime,
        &vm_id,
        &operation_id,
        &node_id,
        &destination_url,
        progress_reporter.as_ref(),
        &mut pause_guard,
        &transfer,
    )
    .await
}

/// Phase 2 of a source migration: report the memory phase, enter the
/// non-cancellable VMM transfer window atomically, run `send-migration`, then
/// seal the window on an unknown outcome (a cancel must never be confirmed) or
/// end it on a definitive result. On failure the paused-source policy resumes
/// the VM only when the transfer is definitively stopped.
///
/// Extracted so the seal/resume wiring has a direct test against a real paused
/// VM, not just the classification predicates.
#[allow(clippy::too_many_arguments)]
async fn send_memory_migration(
    vm_runtime: &VmRuntime,
    vm_id: &str,
    operation_id: &str,
    node_id: &str,
    destination_url: &str,
    progress_reporter: Option<&ProgressReporter>,
    pause_guard: &mut PausedVmGuard,
    transfer: &TransferWindow,
) -> Result<(), String> {
    // Phase 2: Memory migration
    info!(
        vm_id = %vm_id,
        destination_url = %destination_url,
        operation_id = %operation_id,
        "source agent: starting memory migration (send-migration)"
    );

    if let Some(reporter) = progress_reporter {
        let progress = build_progress(
            node_id,
            vm_id,
            operation_id,
            proto::MigrationPhase::MemoryMigration,
            0,
            0,
            0,
            0,
            50.0,
        );
        let mut cb = reporter.lock().await;
        cb(progress);
    }

    // Enter the non-cancellable window atomically. If a cancel was armed
    // first, do not start send-migration; resume a VM paused for the disk
    // phase and surface the cancellation.
    if !transfer.try_begin() {
        warn!(
            vm_id = %vm_id,
            operation_id = %operation_id,
            "source agent: migration cancelled before send-migration"
        );
        pause_guard.resume_if_paused().await;
        return Err("migration cancelled before memory migration".to_string());
    }
    let send_result = vm_runtime
        .send_migration(vm_id, destination_url, Some(operation_id))
        .await;
    if matches!(&send_result, Err(e) if is_outcome_unknown(e)) {
        transfer.seal_unknown();
    } else {
        transfer.end();
    }
    match send_result {
        Ok(()) => {
            if let Some(reporter) = progress_reporter {
                let progress = build_progress(
                    node_id,
                    vm_id,
                    operation_id,
                    proto::MigrationPhase::Completed,
                    0,
                    0,
                    0,
                    0,
                    100.0,
                );
                let mut cb = reporter.lock().await;
                cb(progress);
            }

            info!(
                vm_id = %vm_id,
                operation_id = %operation_id,
                "source agent: send-migration completed successfully (disk + memory)"
            );
            Ok(())
        }
        Err(e) => {
            error!(
                vm_id = %vm_id,
                operation_id = %operation_id,
                error = %e,
                "source agent: memory migration (send-migration) failed"
            );
            // A normal failure means Cloud Hypervisor is no longer sending,
            // so the VM must be resumed or the reconciler cannot run it. An
            // unknown outcome is different: CH may still be streaming, so
            // leave the VM paused for the reaper / operator (the sealed window
            // keeps the cancel unconfirmed).
            settle_paused_source_after_send_failure(&e, pause_guard, vm_id, operation_id).await;
            Err(format!("memory migration failed: {e}"))
        }
    }
}

/// Whether an error from a migration VMM call leaves Cloud Hypervisor in a
/// known-stopped state. A timeout or a lost response does not: the request was
/// delivered and the transfer may still run, so unpausing the source would
/// race a live sender (and a cancel must never be confirmed).
fn is_outcome_unknown(error: &ChvError) -> bool {
    matches!(
        error,
        ChvError::Timeout { .. } | ChvError::OutcomeUnknown { .. }
    )
}

/// Whether a failed source-side `send-migration` leaves Cloud Hypervisor in a
/// known-stopped state, i.e. the paused VM is safe to resume.
fn source_should_resume_after_send_failure(error: &ChvError) -> bool {
    !is_outcome_unknown(error)
}

/// Apply the send-failure policy to the paused source VM.
///
/// A definitive failure resumes it so the reconciler can drive it back to
/// Running; an unknown outcome leaves it paused for the reaper/operator so a
/// possibly-live memory sender is never resumed underneath.
async fn settle_paused_source_after_send_failure(
    error: &ChvError,
    pause_guard: &mut PausedVmGuard,
    vm_id: &str,
    operation_id: &str,
) {
    if source_should_resume_after_send_failure(error) {
        pause_guard.resume_if_paused().await;
    } else {
        warn!(
            vm_id = %vm_id,
            operation_id = %operation_id,
            "source agent: send-migration outcome is unknown; leaving the VM paused \
             because the transfer may still be running"
        );
    }
}

/// Build a MigrationProgress proto message.
#[allow(clippy::too_many_arguments)]
pub fn build_progress(
    node_id: &str,
    vm_id: &str,
    operation_id: &str,
    phase: proto::MigrationPhase,
    bytes_transferred: u64,
    total_bytes: u64,
    convergence_round: u32,
    dirty_blocks_remaining: u64,
    progress_percent: f32,
) -> proto::MigrationProgress {
    proto::MigrationProgress {
        vm_id: vm_id.to_string(),
        operation_id: operation_id.to_string(),
        phase: phase.into(),
        bytes_transferred,
        total_bytes,
        convergence_round,
        dirty_blocks_remaining,
        progress_percent,
        node_id: node_id.to_string(),
    }
}

/// Default migration port to use when the receiver communicates its port back.
/// This is used as a fallback when port allocation information is not available.
pub const DEFAULT_MIGRATION_PORT: u16 = 49152;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration_registry::MigrationTaskRegistry;

    #[test]
    fn determine_role_source() {
        let role = determine_role("node-a", "node-a", "node-b");
        assert_eq!(role, Some(MigrationRole::Source));
    }

    #[test]
    fn determine_role_destination() {
        let role = determine_role("node-b", "node-a", "node-b");
        assert_eq!(role, Some(MigrationRole::Destination));
    }

    #[test]
    fn determine_role_unrelated() {
        let role = determine_role("node-c", "node-a", "node-b");
        assert_eq!(role, None);
    }

    #[test]
    fn allocate_port_succeeds() {
        // This test may fail if all ports in range are busy, but should work in CI.
        let result = allocate_migration_port();
        assert!(result.is_ok());
        let (port, listener) = result.unwrap();
        assert!(port >= MIGRATION_PORT_RANGE_START);
        assert!(port <= MIGRATION_PORT_RANGE_END);
        // The listener holds the port open — verify it matches
        assert_eq!(listener.local_addr().unwrap().port(), port);
    }

    #[test]
    fn build_receiver_url_format() {
        let url = build_receiver_url(49155);
        assert_eq!(url, "tcp://0.0.0.0:49155");
    }

    #[test]
    fn build_destination_url_format() {
        let url = build_destination_url("192.168.1.10", 49155);
        assert_eq!(url, "tcp://192.168.1.10:49155");
    }

    #[test]
    fn build_progress_message() {
        let progress = build_progress(
            "node-1",
            "vm-1",
            "op-1",
            proto::MigrationPhase::MemoryMigration,
            1024,
            4096,
            2,
            10,
            25.0,
        );
        assert_eq!(progress.vm_id, "vm-1");
        assert_eq!(progress.operation_id, "op-1");
        assert_eq!(progress.node_id, "node-1");
        assert_eq!(
            progress.phase,
            proto::MigrationPhase::MemoryMigration as i32
        );
        assert_eq!(progress.bytes_transferred, 1024);
        assert_eq!(progress.total_bytes, 4096);
        assert_eq!(progress.convergence_round, 2);
        assert_eq!(progress.dirty_blocks_remaining, 10);
        assert_eq!(progress.progress_percent, 25.0);
    }

    #[test]
    fn migration_volume_struct() {
        let vol = MigrationVolume {
            volume_id: "vol-abc".to_string(),
            attachment_handle: "handle-1".to_string(),
        };
        assert_eq!(vol.volume_id, "vol-abc");
        assert_eq!(vol.attachment_handle, "handle-1");
    }

    #[test]
    fn disk_precopy_config_construction() {
        let config = DiskPrecopyConfig {
            stord_socket: std::path::PathBuf::from("/run/chv/stord.sock"),
            dest_stord_endpoint: "http://10.0.0.5:50052".to_string(),
            pause_first: false,
            volume_authorizations: std::collections::HashMap::new(),
            allow_insecure_unauthenticated: false,
            volumes: vec![
                MigrationVolume {
                    volume_id: "vol-1".to_string(),
                    attachment_handle: "h-1".to_string(),
                },
                MigrationVolume {
                    volume_id: "vol-2".to_string(),
                    attachment_handle: "h-2".to_string(),
                },
            ],
        };
        assert_eq!(config.volumes.len(), 2);
        assert_eq!(
            config.stord_socket,
            std::path::PathBuf::from("/run/chv/stord.sock")
        );
        assert_eq!(config.dest_stord_endpoint, "http://10.0.0.5:50052");
    }

    #[test]
    fn disk_precopy_config_empty_volumes() {
        let config = DiskPrecopyConfig {
            stord_socket: std::path::PathBuf::from("/run/chv/stord.sock"),
            dest_stord_endpoint: "http://10.0.0.5:50052".to_string(),
            pause_first: false,
            volume_authorizations: std::collections::HashMap::new(),
            allow_insecure_unauthenticated: false,
            volumes: vec![],
        };
        assert!(config.volumes.is_empty());
    }

    #[test]
    fn volume_without_authorization_is_refused_in_production_mode() {
        let authorizations = std::collections::HashMap::new();
        let err = migration_authorization_for_volume("vol-1", &authorizations, false)
            .expect_err("a tokenless volume must never reach stord outside the explicit dev mode");
        assert!(err.contains("no control-plane migration authorization"));
        assert!(err.contains("vol-1"));
    }

    #[test]
    fn volume_authorization_is_returned_verbatim() {
        let mut authorizations = std::collections::HashMap::new();
        authorizations.insert("vol-1".to_string(), "v1.payload.signature".to_string());
        assert_eq!(
            migration_authorization_for_volume("vol-1", &authorizations, false).unwrap(),
            "v1.payload.signature"
        );
        // Empty tokens count as absent: refused unless the explicit
        // development-mode gate is on.
        authorizations.insert("vol-2".to_string(), String::new());
        assert!(migration_authorization_for_volume("vol-2", &authorizations, false).is_err());
        assert_eq!(
            migration_authorization_for_volume("vol-2", &authorizations, true).unwrap(),
            ""
        );
    }

    #[test]
    fn admission_retry_backoff_is_bounded_exponential() {
        use std::time::Duration;
        assert_eq!(trigger_admission_retry_delay(0), Duration::from_millis(250));
        assert_eq!(trigger_admission_retry_delay(1), Duration::from_millis(500));
        assert_eq!(
            trigger_admission_retry_delay(2),
            Duration::from_millis(1000)
        );
        assert_eq!(
            trigger_admission_retry_delay(3),
            Duration::from_millis(2000)
        );
        // Saturates instead of overflow/runaway for absurd attempt values.
        assert_eq!(
            trigger_admission_retry_delay(64),
            Duration::from_millis(2000)
        );
    }

    // Guard against a regression where caller passes total_bytes=0 and
    // computes progress_percent upstream as `bytes / total`. The constructor
    // itself must accept the value verbatim and never panic.
    #[test]
    fn build_progress_with_zero_total_does_not_panic() {
        let progress = build_progress(
            "node-1",
            "vm-1",
            "op-1",
            proto::MigrationPhase::Pending,
            0,
            0,
            0,
            0,
            0.0,
        );
        assert_eq!(progress.total_bytes, 0);
        assert_eq!(progress.bytes_transferred, 0);
        assert_eq!(progress.progress_percent, 0.0);
    }

    // Re-counted dirty pages can legitimately push bytes_transferred past
    // total_bytes during convergence rounds. The proto builder must not clamp
    // or reject — that is the caller's job — so we verify pass-through.
    #[test]
    fn build_progress_with_excess_bytes_preserves_values() {
        let progress = build_progress(
            "node-1",
            "vm-1",
            "op-1",
            proto::MigrationPhase::ConvergingDisk,
            8192,
            4096,
            7,
            32,
            150.0,
        );
        assert_eq!(progress.bytes_transferred, 8192);
        assert_eq!(progress.total_bytes, 4096);
        assert_eq!(progress.convergence_round, 7);
        assert_eq!(progress.dirty_blocks_remaining, 32);
        assert_eq!(progress.progress_percent, 150.0);
    }

    // MigrationRole is `Copy`. Removing the derive would silently move the
    // value out of locals at every match site in the codebase. This test
    // makes that regression a compile error in the test build.
    #[test]
    fn migration_role_is_copy() {
        let original = MigrationRole::Source;
        let copied = original; // Copy semantics — original must remain usable.
        assert_eq!(original, MigrationRole::Source);
        assert_eq!(copied, MigrationRole::Source);
    }

    // Guards against proto enum drift: every variant must round-trip through
    // i32 unchanged. If a variant is added to the .proto and the agent forgets
    // to handle it, this test forces a compile-time match-arm failure here.
    #[test]
    fn migration_phase_round_trip_all_variants() {
        use proto::MigrationPhase;
        for variant in [
            MigrationPhase::Unspecified,
            MigrationPhase::Pending,
            MigrationPhase::PrecopyDisk,
            MigrationPhase::ConvergingDisk,
            MigrationPhase::MemoryMigration,
            MigrationPhase::Paused,
            MigrationPhase::Completed,
            MigrationPhase::Failed,
            MigrationPhase::RolledBack,
        ] {
            let raw = variant as i32;
            let recovered =
                MigrationPhase::try_from(raw).expect("known variant must round-trip through i32");
            assert_eq!(recovered, variant, "round-trip mismatch for {:?}", variant);
        }
    }

    // Symmetric test for the string-form round-trip — protects against
    // ProtoBuf field-name renames that would silently break wire-compat.
    #[test]
    fn migration_phase_round_trip_via_str_name() {
        use proto::MigrationPhase;
        for variant in [
            MigrationPhase::Unspecified,
            MigrationPhase::Pending,
            MigrationPhase::PrecopyDisk,
            MigrationPhase::ConvergingDisk,
            MigrationPhase::MemoryMigration,
            MigrationPhase::Paused,
            MigrationPhase::Completed,
            MigrationPhase::Failed,
            MigrationPhase::RolledBack,
        ] {
            let name = variant.as_str_name();
            let recovered = MigrationPhase::from_str_name(name)
                .expect("known variant must round-trip through str_name");
            assert_eq!(
                recovered, variant,
                "str-name round-trip mismatch for {:?}",
                variant
            );
        }
    }

    // --- Post-pause resume recovery tests ---

    use chv_agent_runtime_ch::mock::MockCloudHypervisorAdapter;
    use chv_hypervisor_api::VmConfig;
    use chv_stord_api::chv_stord_api::get_disk_migration_status_response::Phase as StordPhase;
    use chv_stord_api::chv_stord_api::GetDiskMigrationStatusResponse;
    use std::collections::VecDeque;
    use std::path::PathBuf;

    fn test_runtime() -> (VmRuntime, Arc<MockCloudHypervisorAdapter>) {
        let mock = Arc::new(MockCloudHypervisorAdapter::default());
        (VmRuntime::new(mock.clone()), mock)
    }

    async fn create_vm_record(rt: &VmRuntime) {
        let config = VmConfig {
            vm_id: "vm-1".to_string(),
            cpus: 2,
            memory_bytes: 1024,
            kernel_path: PathBuf::from("/dev/null"),
            firmware_path: None,
            disks: vec![],
            nics: vec![],
            api_socket_path: PathBuf::from("/var/lib/chv/agent/vms/vm-1/vm.sock"),
            cloud_init_userdata: None,
            hypervisor_overrides: None,
            vsock: None,
        };
        rt.create_vm("vm-1", "1", &config, Some("op-1"))
            .await
            .unwrap();
    }

    fn stub_status(phase: StordPhase) -> GetDiskMigrationStatusResponse {
        GetDiskMigrationStatusResponse {
            result: Some(chv_stord_api::chv_stord_api::Result {
                status: "ok".to_string(),
                error_code: "OK".to_string(),
                human_summary: String::new(),
            }),
            phase: phase as i32,
            convergence_round: 0,
            dirty_blocks_remaining: 0,
            bytes_transferred: 0,
            total_bytes: 0,
            needs_vm_pause: phase == StordPhase::PausedFinalSync
                || phase == StordPhase::PausedPreCopy,
            error_message: String::new(),
        }
    }

    struct StubDiskMigration {
        statuses: VecDeque<GetDiskMigrationStatusResponse>,
        resume_result: Result<(), String>,
        resume_calls: u32,
    }

    #[async_trait::async_trait]
    impl DiskMigrationControl for StubDiskMigration {
        async fn get_status(
            &mut self,
            _migration_id: &str,
        ) -> Result<GetDiskMigrationStatusResponse, String> {
            Ok(self
                .statuses
                .pop_front()
                .unwrap_or_else(|| stub_status(StordPhase::Completed)))
        }

        async fn resume(&mut self, _migration_id: &str, _vm_paused: bool) -> Result<(), String> {
            self.resume_calls += 1;
            self.resume_result.clone()
        }
    }

    fn new_guard(rt: &VmRuntime) -> PausedVmGuard {
        PausedVmGuard::new(rt.clone(), "vm-1".to_string(), "op-1".to_string())
    }

    #[tokio::test]
    async fn failed_resume_after_pause_resumes_vm_and_returns_original_error() {
        let (rt, _mock) = test_runtime();
        create_vm_record(&rt).await;

        let mut stub = StubDiskMigration {
            statuses: VecDeque::from([stub_status(StordPhase::PausedFinalSync)]),
            resume_result: Err("stord rejected resume after pause".to_string()),
            resume_calls: 0,
        };
        let mut guard = new_guard(&rt);
        let cancel = CancellationToken::new();

        let err = run_disk_precopy_poll(
            &mut stub,
            &[("vol-1".to_string(), "dm-test-1".to_string())],
            std::time::Duration::from_millis(10),
            false,
            &cancel,
            &mut guard,
            None,
            "node-1",
            "vm-1",
            "op-1",
        )
        .await
        .unwrap_err();

        assert_eq!(stub.resume_calls, 1, "stord resume must be attempted once");
        assert!(
            err.contains("failed to resume disk migration dm-test-1"),
            "original error must be surfaced, got: {err}"
        );
        assert!(
            err.contains("stord rejected resume after pause"),
            "underlying cause must be preserved, got: {err}"
        );
        assert_eq!(
            rt.get("vm-1").await.unwrap().runtime_status,
            "Running",
            "VM must be resumed after the failure"
        );
    }

    #[tokio::test]
    async fn post_pause_status_failure_resumes_vm_and_returns_original_error() {
        let (rt, _mock) = test_runtime();
        create_vm_record(&rt).await;

        // First poll round signals the pause; the resumed migration then fails.
        let mut stub = StubDiskMigration {
            statuses: VecDeque::from([
                stub_status(StordPhase::PausedFinalSync),
                stub_status(StordPhase::Failed),
            ]),
            resume_result: Ok(()),
            resume_calls: 0,
        };
        let mut guard = new_guard(&rt);
        let cancel = CancellationToken::new();

        let err = run_disk_precopy_poll(
            &mut stub,
            &[("vol-1".to_string(), "dm-test-1".to_string())],
            std::time::Duration::from_millis(10),
            false,
            &cancel,
            &mut guard,
            None,
            "node-1",
            "vm-1",
            "op-1",
        )
        .await
        .unwrap_err();

        assert_eq!(stub.resume_calls, 1);
        assert_eq!(err, "disk migration failed for one or more volumes");
        assert_eq!(
            rt.get("vm-1").await.unwrap().runtime_status,
            "Running",
            "VM must be resumed after the post-pause failure"
        );
    }

    /// Pause-first mode (issue #394, Option C): a `PausedPreCopy` status
    /// (stord waiting for the VM pause BEFORE bulk copy) drives the same
    /// pause-and-resume handshake as the final-sync pause, and the
    /// progress reported during that window must stay in the disk phase —
    /// reporting `MemoryMigration` there (the default-mode mapping for
    /// `needs_vm_pause`) would claim disk work is done when not one byte
    /// has transferred.
    #[tokio::test]
    async fn paused_pre_copy_drives_pause_handshake_and_reports_disk_phase() {
        let (rt, _mock) = test_runtime();
        create_vm_record(&rt).await;

        // The pre-copy pause, one productive bulk-copy poll, then done —
        // the shape a real pause-first migration presents to the poller.
        let mut stub = StubDiskMigration {
            statuses: VecDeque::from([
                stub_status(StordPhase::PausedPreCopy),
                stub_status(StordPhase::BulkCopy),
                stub_status(StordPhase::Completed),
            ]),
            resume_result: Ok(()),
            resume_calls: 0,
        };
        let mut guard = new_guard(&rt);
        let cancel = CancellationToken::new();

        let observed_phases: Arc<std::sync::Mutex<Vec<proto::MigrationPhase>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = observed_phases.clone();
        let reporter: ProgressReporter = make_progress_reporter(move |progress| {
            if let Ok(phase) = proto::MigrationPhase::try_from(progress.phase) {
                recorder.lock().unwrap().push(phase);
            }
        });

        run_disk_precopy_poll(
            &mut stub,
            &[("vol-1".to_string(), "dm-test-1".to_string())],
            std::time::Duration::from_millis(1),
            true, // pause_first — the mode that produces PausedPreCopy
            &cancel,
            &mut guard,
            Some(&reporter),
            "node-1",
            "vm-1",
            "op-1",
        )
        .await
        .expect("pause-first disk pre-copy poll must succeed");

        assert_eq!(
            stub.resume_calls, 1,
            "PausedPreCopy must trigger exactly one resume after the pause"
        );
        assert!(
            guard.is_paused(),
            "the VM stays paused through the disk phase (stop-the-world)"
        );
        let phases = observed_phases.lock().unwrap().clone();
        // [PausedPreCopy, BulkCopy, Completed] polls map to disk-phase
        // progress for the first two; only the Completed poll reports
        // MemoryMigration (disk done, memory migration next) — the
        // default-mode mapping for needs_vm_pause would have claimed it
        // on the very first poll.
        assert_eq!(
            phases.first(),
            Some(&proto::MigrationPhase::PrecopyDisk),
            "progress during PausedPreCopy must stay in the disk phase, got {:?}",
            phases
        );
        assert!(
            phases[..phases.len() - 1]
                .iter()
                .all(|p| *p == proto::MigrationPhase::PrecopyDisk),
            "every pre-completion poll must stay in the disk phase, got {:?}",
            phases
        );
        assert_eq!(
            phases.last(),
            Some(&proto::MigrationPhase::MemoryMigration),
            "the Completed poll hands off to the memory phase, got {:?}",
            phases
        );
    }

    #[tokio::test]
    async fn resume_if_paused_is_noop_when_vm_was_not_paused() {
        let (rt, _mock) = test_runtime();
        create_vm_record(&rt).await;
        let mut guard = new_guard(&rt);
        guard.resume_if_paused().await;
        // A resume call would flip the record to Running; staying "Created"
        // proves resume_vm was never invoked.
        assert_eq!(rt.get("vm-1").await.unwrap().runtime_status, "Created");
    }

    /// A timeout or a lost VMM reply from the adapter must seal the transfer
    /// window (the VMM may still be receiving) so a later cancel cannot be
    /// confirmed. This drives the real `spawn_destination_migration` wiring.
    #[tokio::test]
    async fn destination_timeout_seals_transfer_window() {
        let (rt, mock) = test_runtime();
        *mock.migration_timeout.lock().unwrap() = true;
        let transfer = Arc::new(TransferWindow::default());

        let handle = spawn_destination_migration(
            rt,
            "vm-1".to_string(),
            "op-timeout".to_string(),
            "tcp://127.0.0.1:1".to_string(),
            transfer.clone(),
        );
        let err = handle
            .await
            .expect("task joins")
            .expect_err("the mock must time out");
        assert!(err.contains("timed out"), "{err}");
        assert!(
            transfer.is_unknown(),
            "a VMM timeout must seal the transfer as outcome-unknown"
        );
        assert!(transfer.in_progress());
        assert!(
            !transfer.try_begin(),
            "a sealed transfer must not restart the VMM call"
        );

        // Connect the adapter timeout to the registry: the tracked task must
        // stay tracked (and its cancel never confirm) after the completion
        // reaper's `remove`.
        let registry = Arc::new(MigrationTaskRegistry::new());
        registry.set_cancel_grace(std::time::Duration::from_millis(20));
        let task = tokio::spawn(std::future::pending::<()>());
        registry.insert_with_transfer_window(
            "op-timeout".to_string(),
            task.abort_handle(),
            CancellationToken::new(),
            transfer.clone(),
        );
        assert!(registry.cancel("op-timeout"));
        assert!(
            !registry
                .wait_until_stopped("op-timeout", std::time::Duration::from_millis(100))
                .await,
            "a timed-out transfer must never look stopped"
        );
        registry.remove("op-timeout");
        assert!(
            registry.contains("op-timeout"),
            "a sealed task must stay tracked after remove so cancels keep failing closed"
        );
        task.abort();

        // A lost VMM reply is the same outcome class as a timeout and must
        // seal through the same production call site.
        let (rt, mock) = test_runtime();
        *mock.migration_outcome_unknown.lock().unwrap() = true;
        let lost = Arc::new(TransferWindow::default());
        let err = spawn_destination_migration(
            rt,
            "vm-1".to_string(),
            "op-lost-reply".to_string(),
            "tcp://127.0.0.1:1".to_string(),
            lost.clone(),
        )
        .await
        .expect("task joins")
        .expect_err("the mock must report a lost reply");
        assert!(err.contains("outcome unknown"), "{err}");
        assert!(
            lost.is_unknown(),
            "a lost VMM reply must seal the transfer as outcome-unknown"
        );
    }

    /// Only a definitive send-migration failure may unpause the source. A
    /// timeout or a lost response leaves Cloud Hypervisor potentially
    /// streaming, so the VM stays paused for the reaper/operator.
    #[test]
    fn source_resumes_only_on_definitive_send_failure() {
        assert!(!source_should_resume_after_send_failure(
            &ChvError::Timeout {
                operation: "send-migration".to_string(),
                secs: 7200,
            }
        ));
        assert!(!source_should_resume_after_send_failure(
            &ChvError::OutcomeUnknown {
                operation: "send-migration".to_string(),
                reason: "connection closed mid-response".to_string(),
            }
        ));
        assert!(source_should_resume_after_send_failure(
            &ChvError::Internal {
                reason: "connection reset".to_string(),
            }
        ));
    }

    /// The fail-closed branch must hold when applied to a real paused VM, not
    /// just in the predicate: an unknown outcome keeps the guard and the VM
    /// paused, while a definitive failure resumes it.
    #[tokio::test]
    async fn unknown_send_outcome_keeps_source_paused_but_definitive_failure_resumes() {
        let (rt, _mock) = test_runtime();
        create_vm_record(&rt).await;
        let mut guard = new_guard(&rt);
        guard.pause_vm().await.expect("pause the VM");
        assert_eq!(rt.get("vm-1").await.unwrap().runtime_status, "Paused");

        for unknown in [
            ChvError::OutcomeUnknown {
                operation: "send-migration".to_string(),
                reason: "connection closed mid-response".to_string(),
            },
            ChvError::Timeout {
                operation: "send-migration".to_string(),
                secs: 7200,
            },
        ] {
            settle_paused_source_after_send_failure(&unknown, &mut guard, "vm-1", "op-1").await;
            assert!(
                guard.is_paused(),
                "the guard must still consider it paused after {unknown:?}"
            );
        }
        assert_eq!(
            rt.get("vm-1").await.unwrap().runtime_status,
            "Paused",
            "an unknown transfer outcome must not unpause the source"
        );

        settle_paused_source_after_send_failure(
            &ChvError::Internal {
                reason: "connection reset".to_string(),
            },
            &mut guard,
            "vm-1",
            "op-1",
        )
        .await;
        assert_eq!(
            rt.get("vm-1").await.unwrap().runtime_status,
            "Running",
            "a definitive failure must resume the source"
        );
    }

    /// Drive the real source memory-phase wiring: both unknown outcomes must
    /// seal the transfer window, leave the paused source paused, and keep the
    /// cancel unconfirmable.
    #[tokio::test]
    async fn source_send_unknown_outcomes_seal_window_and_stay_paused() {
        for (timeout, lost_reply) in [(true, false), (false, true)] {
            let (rt, mock) = test_runtime();
            create_vm_record(&rt).await;
            *mock.migration_timeout.lock().unwrap() = timeout;
            *mock.migration_outcome_unknown.lock().unwrap() = lost_reply;

            let transfer = Arc::new(TransferWindow::default());
            let mut guard = new_guard(&rt);
            guard.pause_vm().await.expect("pause the VM");

            let err = send_memory_migration(
                &rt,
                "vm-1",
                "op-1",
                "node-1",
                "tcp://127.0.0.1:1",
                None,
                &mut guard,
                &transfer,
            )
            .await
            .expect_err("the mock must report an unknown outcome");
            assert!(err.contains("memory migration failed"), "{err}");
            assert!(
                transfer.is_unknown(),
                "timeout={timeout} lost_reply={lost_reply}: the source send must seal the window"
            );
            assert!(
                guard.is_paused(),
                "the guard must still consider the VM paused"
            );
            assert_eq!(
                rt.get("vm-1").await.unwrap().runtime_status,
                "Paused",
                "an unknown source outcome must not unpause the VM"
            );
            assert!(
                !transfer.try_begin(),
                "a sealed task must not start another VMM transfer"
            );
        }
    }

    /// The destination success and definitive-failure arms must end (not seal)
    /// the window, and a pre-armed cancel must refuse to start receive at all.
    #[tokio::test]
    async fn destination_end_window_and_prearmed_cancel() {
        let (rt, _mock) = test_runtime();
        let transfer = Arc::new(TransferWindow::default());
        spawn_destination_migration(
            rt,
            "vm-1".to_string(),
            "op-1".to_string(),
            "tcp://127.0.0.1:1".to_string(),
            transfer.clone(),
        )
        .await
        .expect("task joins")
        .expect("the mock receive succeeds");
        assert!(!transfer.is_unknown());
        assert!(!transfer.in_progress(), "success must end the window");

        let (rt, mock) = test_runtime();
        *mock.migration_definitive_failure.lock().unwrap() = true;
        let transfer = Arc::new(TransferWindow::default());
        let err = spawn_destination_migration(
            rt,
            "vm-1".to_string(),
            "op-1".to_string(),
            "tcp://127.0.0.1:1".to_string(),
            transfer.clone(),
        )
        .await
        .expect("task joins")
        .expect_err("the mock reports a definitive failure");
        assert!(err.contains("receive-migration failed"), "{err}");
        assert!(!transfer.is_unknown());
        assert!(
            !transfer.in_progress(),
            "a definitive failure must end the window"
        );

        // A cancel armed before the task starts must prevent the VMM call.
        let (rt, mock) = test_runtime();
        *mock.migration_timeout.lock().unwrap() = true;
        let transfer = Arc::new(TransferWindow::default());
        assert!(!transfer.arm_cancel(), "nothing was active yet");
        let err = spawn_destination_migration(
            rt,
            "vm-1".to_string(),
            "op-1".to_string(),
            "tcp://127.0.0.1:1".to_string(),
            transfer.clone(),
        )
        .await
        .expect("task joins")
        .expect_err("a pre-armed cancel must refuse to start receive");
        assert_eq!(err, "migration cancelled before receive-migration");
        assert!(!transfer.in_progress());
    }

    /// The success and definitive-failure arms must end (not seal) the window.
    /// A stuck `TRANSFER_ACTIVE` would make a cancel that lands while the entry
    /// is still tracked return `Unavailable` and skip the registry's delayed
    /// force-abort, even though the VMM call already finished.
    #[tokio::test]
    async fn source_send_success_and_definitive_failure_end_window() {
        // Success: the migrated source stays paused, but the window is over.
        let (rt, _mock) = test_runtime();
        create_vm_record(&rt).await;
        let transfer = Arc::new(TransferWindow::default());
        let mut guard = new_guard(&rt);
        guard.pause_vm().await.expect("pause the VM");
        send_memory_migration(
            &rt,
            "vm-1",
            "op-1",
            "node-1",
            "tcp://127.0.0.1:1",
            None,
            &mut guard,
            &transfer,
        )
        .await
        .expect("the mock send succeeds");
        assert!(!transfer.is_unknown());
        assert!(!transfer.in_progress(), "success must end the window");
        assert!(
            guard.is_paused(),
            "a successfully migrated source must not be resumed"
        );
        assert!(
            transfer.try_begin(),
            "the window must be reusable after end"
        );

        // Definitive failure: end the window and resume the paused source.
        let (rt, mock) = test_runtime();
        create_vm_record(&rt).await;
        *mock.migration_definitive_failure.lock().unwrap() = true;
        let transfer = Arc::new(TransferWindow::default());
        let mut guard = new_guard(&rt);
        guard.pause_vm().await.expect("pause the VM");
        let err = send_memory_migration(
            &rt,
            "vm-1",
            "op-1",
            "node-1",
            "tcp://127.0.0.1:1",
            None,
            &mut guard,
            &transfer,
        )
        .await
        .expect_err("the mock reports a definitive failure");
        assert!(err.contains("memory migration failed"), "{err}");
        assert!(!transfer.is_unknown());
        assert!(
            !transfer.in_progress(),
            "a definitive failure must end the window"
        );
        assert_eq!(
            rt.get("vm-1").await.unwrap().runtime_status,
            "Running",
            "a definitive failure must resume the source"
        );
    }

    /// A cancel armed before the memory phase must refuse to start the VMM
    /// send and resume the VM paused for the disk pre-copy.
    #[tokio::test]
    async fn source_prearmed_cancel_resumes_paused_vm_without_send() {
        let (rt, _mock) = test_runtime();
        create_vm_record(&rt).await;
        let transfer = Arc::new(TransferWindow::default());
        assert!(!transfer.arm_cancel(), "nothing was active yet");
        let mut guard = new_guard(&rt);
        guard.pause_vm().await.expect("pause the VM");

        let err = send_memory_migration(
            &rt,
            "vm-1",
            "op-1",
            "node-1",
            "tcp://127.0.0.1:1",
            None,
            &mut guard,
            &transfer,
        )
        .await
        .expect_err("a pre-armed cancel must not start send-migration");
        assert_eq!(err, "migration cancelled before memory migration");
        assert_eq!(
            rt.get("vm-1").await.unwrap().runtime_status,
            "Running",
            "the VM paused for disk pre-copy must be resumed"
        );
    }
}
