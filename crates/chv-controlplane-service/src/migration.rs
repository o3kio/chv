//! Live migration orchestrator state machine.
//!
//! Drives VM migration through phases: PreCopyDisk -> ConvergingDisk -> MemoryMigration -> Paused -> Completed.
//! Handles rollback at each phase according to spec.

use crate::node_client_pool::NodeClientPool;
use chv_controlplane_store::StorePool;
use chv_errors::ChvError;
use control_plane_node_api::control_plane_node_api as proto;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{error, info, warn};

/// Phases of a live migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationPhase {
    Pending,
    PreCopyDisk,
    ConvergingDisk,
    MemoryMigration,
    Paused,
    Completed,
    Failed,
    RolledBack,
}

impl MigrationPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::PreCopyDisk => "PreCopyDisk",
            Self::ConvergingDisk => "ConvergingDisk",
            Self::MemoryMigration => "MemoryMigration",
            Self::Paused => "Paused",
            Self::Completed => "Completed",
            Self::Failed => "Failed",
            Self::RolledBack => "RolledBack",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Pending" => Some(Self::Pending),
            "PreCopyDisk" => Some(Self::PreCopyDisk),
            "ConvergingDisk" => Some(Self::ConvergingDisk),
            "MemoryMigration" => Some(Self::MemoryMigration),
            "Paused" => Some(Self::Paused),
            "Completed" => Some(Self::Completed),
            "Failed" => Some(Self::Failed),
            "RolledBack" => Some(Self::RolledBack),
            _ => None,
        }
    }

    /// Convert to proto enum value.
    pub fn to_proto(&self) -> i32 {
        match self {
            Self::Pending => proto::MigrationPhase::Pending as i32,
            Self::PreCopyDisk => proto::MigrationPhase::PrecopyDisk as i32,
            Self::ConvergingDisk => proto::MigrationPhase::ConvergingDisk as i32,
            Self::MemoryMigration => proto::MigrationPhase::MemoryMigration as i32,
            Self::Paused => proto::MigrationPhase::Paused as i32,
            Self::Completed => proto::MigrationPhase::Completed as i32,
            Self::Failed => proto::MigrationPhase::Failed as i32,
            Self::RolledBack => proto::MigrationPhase::RolledBack as i32,
        }
    }
}

/// Configuration parameters for a migration operation.
#[derive(Debug, Clone)]
pub struct MigrationConfig {
    pub dirty_threshold_blocks: u32,
    pub max_convergence_rounds: u32,
    pub block_size_bytes: u32,
    pub total_timeout_seconds: u32,
    /// Multiplier applied to all phase timeouts for slow storage (default 1.0).
    ///
    /// A value > 1.0 extends all phase timeouts proportionally, useful for
    /// migrations involving slow backends (e.g., NFS, remote storage).
    pub timeout_multiplier: f64,
    /// Issue #394 Option C: opt-in stop-the-world mode — pause the VM
    /// before disk pre-copy instead of only at final sync. Not
    /// representable in the correlation-id format (defaults false there).
    pub pause_first: bool,
}

impl Default for MigrationConfig {
    fn default() -> Self {
        Self {
            dirty_threshold_blocks: 1024,
            max_convergence_rounds: 10,
            block_size_bytes: 4_194_304, // 4MB
            total_timeout_seconds: 0,    // 0 = use calculated default
            timeout_multiplier: 1.0,
            pause_first: false,
        }
    }
}

impl MigrationConfig {
    /// Parse from proto message.
    pub fn from_proto(proto: &proto::MigrationConfig) -> Self {
        let defaults = Self::default();
        Self {
            dirty_threshold_blocks: if proto.dirty_threshold_blocks > 0 {
                proto.dirty_threshold_blocks
            } else {
                defaults.dirty_threshold_blocks
            },
            max_convergence_rounds: if proto.max_convergence_rounds > 0 {
                proto.max_convergence_rounds
            } else {
                defaults.max_convergence_rounds
            },
            block_size_bytes: if proto.block_size_bytes > 0 {
                proto.block_size_bytes
            } else {
                defaults.block_size_bytes
            },
            total_timeout_seconds: proto.total_timeout_seconds,
            timeout_multiplier: defaults.timeout_multiplier,
            pause_first: proto.pause_first,
        }
    }

    /// Convert to proto message.
    pub fn to_proto(&self) -> proto::MigrationConfig {
        proto::MigrationConfig {
            dirty_threshold_blocks: self.dirty_threshold_blocks,
            max_convergence_rounds: self.max_convergence_rounds,
            block_size_bytes: self.block_size_bytes,
            total_timeout_seconds: self.total_timeout_seconds,
            pause_first: self.pause_first,
        }
    }

    /// Parse from correlation_id format:
    /// `source={source_node_id}:dest={dest_node_id}:threshold={N}:rounds={N}:block_size={N}:timeout={N}:timeout_multiplier={F}`
    ///
    /// Issue #394 Option C adds an optional `pause_first=true` key (any
    /// other value means false — the format's other keys are equally
    /// lenient with unparseable values). Without the key the mode is off:
    /// the correlation_id is the ONLY carrier from the RPC's
    /// `MigrationConfig` to the migration state machine, so a dropped key
    /// would silently downgrade an operator's stop-the-world request to
    /// quiescent-assumed — the exact #394 failure mode the mode exists to
    /// close.
    pub fn from_correlation_id(corr: &str) -> (String, String, Self) {
        let mut source_node = String::new();
        let mut dest_node = String::new();
        let mut config = Self::default();

        for part in corr.split(':') {
            if let Some(val) = part.strip_prefix("source=") {
                source_node = val.to_string();
            } else if let Some(val) = part.strip_prefix("dest=") {
                dest_node = val.to_string();
            } else if let Some(val) = part.strip_prefix("threshold=") {
                if let Ok(v) = val.parse::<u32>() {
                    config.dirty_threshold_blocks = v;
                }
            } else if let Some(val) = part.strip_prefix("rounds=") {
                if let Ok(v) = val.parse::<u32>() {
                    config.max_convergence_rounds = v;
                }
            } else if let Some(val) = part.strip_prefix("block_size=") {
                if let Ok(v) = val.parse::<u32>() {
                    config.block_size_bytes = v;
                }
            } else if let Some(val) = part.strip_prefix("timeout=") {
                if let Ok(v) = val.parse::<u32>() {
                    config.total_timeout_seconds = v;
                }
            } else if let Some(val) = part.strip_prefix("pause_first=") {
                if val == "true" {
                    config.pause_first = true;
                }
            }
        }

        (source_node, dest_node, config)
    }
}

/// Tracks the current state of a live migration.
#[derive(Debug, Clone)]
pub struct MigrationState {
    pub migration_id: String,
    pub operation_id: String,
    pub vm_id: String,
    pub source_node_id: String,
    pub dest_node_id: String,
    pub phase: MigrationPhase,
    pub config: MigrationConfig,
    pub bytes_transferred: u64,
    pub total_bytes: u64,
    pub convergence_round: u32,
    pub dirty_blocks_remaining: u64,
}

/// Calculate phase timeouts based on VM resource sizes.
pub struct PhaseTimeouts {
    pub precopy_disk_secs: u64,
    pub converging_disk_per_round_secs: u64,
    pub converging_disk_total_secs: u64,
    pub memory_migration_secs: u64,
    pub paused_secs: u64,
    pub total_secs: u64,
}

impl PhaseTimeouts {
    /// Calculate timeouts based on VM disk and memory sizes.
    ///
    /// - PreCopyDisk: disk_size_gb * 60s
    /// - ConvergingDisk: 300s per round, total 3000s max
    /// - MemoryMigration: memory_size_gb * 30s + 120s
    /// - Paused (final sync): 60s
    /// - Total: sum + 300s buffer
    ///
    /// The `timeout_multiplier` is applied to all phase timeouts to accommodate
    /// slow storage backends. A multiplier of 1.0 leaves timeouts unchanged.
    pub fn calculate(disk_size_gb: u64, memory_size_gb: u64, timeout_multiplier: f64) -> Self {
        let multiplier = if timeout_multiplier > 0.0 {
            timeout_multiplier
        } else {
            1.0
        };

        let precopy_disk_secs = ((disk_size_gb.max(1) * 60) as f64 * multiplier) as u64;
        let converging_disk_per_round_secs = (300.0 * multiplier) as u64;
        let converging_disk_total_secs = (3000.0 * multiplier) as u64;
        let memory_migration_secs = ((memory_size_gb.max(1) * 30 + 120) as f64 * multiplier) as u64;
        let paused_secs = (60.0 * multiplier) as u64;
        let total_secs = precopy_disk_secs
            + converging_disk_total_secs
            + memory_migration_secs
            + paused_secs
            + (300.0 * multiplier) as u64;

        Self {
            precopy_disk_secs,
            converging_disk_per_round_secs,
            converging_disk_total_secs,
            memory_migration_secs,
            paused_secs,
            total_secs,
        }
    }
}

/// Maximum allowed age of a source node heartbeat for migration to proceed.
pub(crate) const SOURCE_NODE_HEARTBEAT_MAX_AGE_SECS: i64 = 30;

/// Hard cap on the disk-convergence loop.
const MAX_TOTAL_SECS: u64 = 7200;

/// Maximum age of a migration row before its agent is considered to have
/// stopped reporting. The agent's disk-poll loop reports every ~5 s, so a row
/// this old means ~18 consecutive missed reports — the agent (or its telemetry
/// path) is dead or wedged. Bulk-copy progress is invisible in the row
/// (stord's `bytes_transferred` only accumulates dirty re-send bytes), so
/// freshness — not byte movement — is the liveness signal.
///
/// Shared with the migration reaper: it uses this as the minimum quiescence
/// window before it is willing to resume a source VM after a reap. Resuming
/// while the sender may still be streaming would unpause a VM whose memory is
/// still being transferred.
pub(crate) const STALE_ROW_SECS: i64 = 90;

/// Validate preconditions before starting a migration.
///
/// Checks:
/// 1. Source node heartbeat is within 30 seconds (node is healthy and reachable).
/// 2. Destination node is in a schedulable state (not Maintenance, Draining, Failed, etc.).
/// 3. No other migration is currently in progress for the same VM.
///
/// These checks prevent wasted multi-hour migrations that would inevitably fail.
async fn validate_preconditions(pool: &StorePool, state: &MigrationState) -> Result<(), ChvError> {
    // Check 1: Source node heartbeat freshness.
    // The node_observed_state.last_seen_at is updated on every state report from the agent.
    let source_last_seen: Option<(Option<String>,)> =
        sqlx::query_as("SELECT last_seen_at FROM node_observed_state WHERE node_id = ?")
            .bind(&state.source_node_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| ChvError::Internal {
                reason: format!("failed to query source node heartbeat: {e}"),
            })?;

    match source_last_seen {
        None => {
            return Err(ChvError::BadRequest {
                reason: format!(
                    "source node '{}' has no observed state record (never reported)",
                    state.source_node_id
                ),
            });
        }
        Some((None,)) => {
            return Err(ChvError::BadRequest {
                reason: format!(
                    "source node '{}' has no last_seen_at timestamp",
                    state.source_node_id
                ),
            });
        }
        Some((Some(last_seen_str),)) => {
            let last_seen =
                chrono::NaiveDateTime::parse_from_str(&last_seen_str, "%Y-%m-%dT%H:%M:%SZ")
                    .map_err(|e| ChvError::Internal {
                        reason: format!(
                            "failed to parse source node last_seen_at '{}': {e}",
                            last_seen_str
                        ),
                    })?;
            let now = chrono::Utc::now().naive_utc();
            let age_secs = (now - last_seen).num_seconds();

            if age_secs > SOURCE_NODE_HEARTBEAT_MAX_AGE_SECS {
                return Err(ChvError::BadRequest {
                    reason: format!(
                        "source node '{}' last heartbeat was {}s ago (max {}s), node may be unhealthy",
                        state.source_node_id, age_secs, SOURCE_NODE_HEARTBEAT_MAX_AGE_SECS
                    ),
                });
            }
        }
    }

    // Check 2: Destination node is in a schedulable state.
    // We check node_desired_state.desired_state — nodes in Maintenance, Draining, or Failed
    // states should not receive new workloads.
    let dest_state: Option<(String, bool)> = sqlx::query_as(
        "SELECT desired_state, scheduling_paused FROM node_desired_state WHERE node_id = ?",
    )
    .bind(&state.dest_node_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| ChvError::Internal {
        reason: format!("failed to query destination node state: {e}"),
    })?;

    match dest_state {
        None => {
            return Err(ChvError::BadRequest {
                reason: format!(
                    "destination node '{}' has no desired state record",
                    state.dest_node_id
                ),
            });
        }
        Some((state_str, scheduling_paused)) => {
            match state_str.as_str() {
                "Maintenance" | "Draining" | "Failed" | "Unreachable" => {
                    return Err(ChvError::BadRequest {
                        reason: format!(
                            "destination node '{}' is in state '{}', not eligible to receive migrations",
                            state.dest_node_id, state_str
                        ),
                    });
                }
                _ => {}
            }

            if scheduling_paused {
                return Err(ChvError::BadRequest {
                    reason: format!(
                        "destination node '{}' has scheduling paused",
                        state.dest_node_id
                    ),
                });
            }
        }
    }

    // Check 3: No other migration currently in progress for the same VM.
    // Query the migrations table directly — active migrations are in non-terminal phases.
    let active_migration: Option<(String, String)> = sqlx::query_as(
        r#"SELECT migration_id, phase FROM migrations
           WHERE vm_id = ? AND phase NOT IN ('Completed', 'Failed', 'RolledBack')
           AND migration_id != ?
           LIMIT 1"#,
    )
    .bind(&state.vm_id)
    .bind(&state.migration_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| ChvError::Internal {
        reason: format!("failed to query active migrations for vm: {e}"),
    })?;

    if let Some((existing_id, existing_phase)) = active_migration {
        return Err(ChvError::BadRequest {
            reason: format!(
                "VM '{}' already has an active migration '{}' in phase '{}'",
                state.vm_id, existing_id, existing_phase
            ),
        });
    }

    Ok(())
}

/// Execute the full migration state machine.
///
/// This drives the migration through all phases:
/// 1. Pending -> PreCopyDisk: Send migrate_vm to source agent to start disk precopy
/// 2. PreCopyDisk -> ConvergingDisk: Iteratively sync dirty blocks until convergence
/// 3. ConvergingDisk -> MemoryMigration: Transfer VM memory to destination
/// 4. MemoryMigration -> Paused: Pause VM for final sync
/// 5. Paused -> Completed: Resume VM on destination
///
/// Returns Ok(()) if migration completed successfully, Err if failed or rolled back.
#[tracing::instrument(
    skip(pool, node_client_pool, state),
    fields(
        migration_id = %state.migration_id,
        vm_id = %state.vm_id,
        source = %state.source_node_id,
        dest = %state.dest_node_id,
    )
)]
pub async fn execute_migration(
    pool: &StorePool,
    node_client_pool: &NodeClientPool,
    agent_socket_pattern: &str,
    state: &mut MigrationState,
) -> Result<(), ChvError> {
    info!(
        migration_id = %state.migration_id,
        vm_id = %state.vm_id,
        source = %state.source_node_id,
        dest = %state.dest_node_id,
        "starting live migration"
    );
    metrics::counter!("chv_migration_started_total").increment(1);

    // Validate preconditions before committing to the migration.
    // These checks prevent wasted multi-hour migrations that would inevitably fail.
    validate_preconditions(pool, state).await?;

    // Look up VM disk and memory sizes for timeout calculation
    let (disk_size_gb, memory_size_gb) = get_vm_sizes(pool, &state.vm_id).await?;
    let timeouts = PhaseTimeouts::calculate(
        disk_size_gb,
        memory_size_gb,
        state.config.timeout_multiplier,
    );

    // Use configured total_timeout if provided, otherwise use calculated
    let total_timeout = if state.config.total_timeout_seconds > 0 {
        Duration::from_secs(state.config.total_timeout_seconds as u64)
    } else {
        Duration::from_secs(timeouts.total_secs)
    };

    let migration_result = tokio::time::timeout(total_timeout, async {
        // Phase 1: PreCopyDisk - dispatch migrate_vm to source agent
        transition_phase(pool, state, MigrationPhase::PreCopyDisk).await?;

        // Cooperative cancel check: a cancel issued before precopy starts must
        // short-circuit before we contact the source agent at all.
        if is_cancel_requested(pool, &state.migration_id).await? {
            info!(
                migration_id = %state.migration_id,
                "cancel observed before precopy dispatch; rolling back"
            );
            rollback_precopy(pool, state).await?;
            return Err(ChvError::Internal {
                reason: format!(
                    "migration {} cancelled by operator (PreCopyDisk)",
                    state.migration_id
                ),
            });
        }

        let vm_generation = get_vm_generation(pool, &state.vm_id).await?;

        let source_socket = resolve_agent_socket(agent_socket_pattern, &state.source_node_id)?;
        let mut source_client = node_client_pool
            .get_or_connect(&state.source_node_id, &source_socket)
            .await?;

        let precopy_result = tokio::time::timeout(
            Duration::from_secs(timeouts.precopy_disk_secs),
            source_client.migrate_vm(
                &state.source_node_id,
                &state.vm_id,
                &vm_generation,
                &state.source_node_id,
                &state.dest_node_id,
                state.config.to_proto(),
                &state.operation_id,
                None,
            ),
        )
        .await;

        match precopy_result {
            Ok(Ok(_ack)) => {
                info!(
                    migration_id = %state.migration_id,
                    "precopy disk phase initiated on source agent"
                );
            }
            Ok(Err(e)) => {
                warn!(
                    migration_id = %state.migration_id,
                    error = %e,
                    "precopy disk failed, initiating rollback"
                );
                rollback_precopy(pool, state).await?;
                return Err(e);
            }
            Err(_elapsed) => {
                warn!(
                    migration_id = %state.migration_id,
                    "precopy disk timed out, initiating rollback"
                );
                rollback_precopy(pool, state).await?;
                return Err(ChvError::Internal {
                    reason: format!(
                        "migration {} precopy disk timed out after {}s",
                        state.migration_id, timeouts.precopy_disk_secs
                    ),
                });
            }
        }

        // Phase 2: ConvergingDisk - wait for agent to converge dirty blocks
        transition_phase(pool, state, MigrationPhase::ConvergingDisk).await?;

        let converge_result = tokio::time::timeout(
            Duration::from_secs(timeouts.converging_disk_total_secs),
            wait_for_convergence(pool, state),
        )
        .await;

        match converge_result {
            Ok(Ok(())) => {
                info!(
                    migration_id = %state.migration_id,
                    "disk convergence achieved"
                );
            }
            Ok(Err(e)) => {
                warn!(
                    migration_id = %state.migration_id,
                    error = %e,
                    "disk convergence failed, initiating rollback"
                );
                rollback_precopy(pool, state).await?;
                return Err(e);
            }
            Err(_elapsed) => {
                warn!(
                    migration_id = %state.migration_id,
                    "disk convergence timed out, initiating rollback"
                );
                rollback_precopy(pool, state).await?;
                return Err(ChvError::Internal {
                    reason: format!(
                        "migration {} converging disk timed out after {}s",
                        state.migration_id, timeouts.converging_disk_total_secs
                    ),
                });
            }
        }

        // Phase 3: MemoryMigration - transfer VM memory
        // Cooperative cancel check between disk convergence and memory migration —
        // last clean rollback point before CH owns the live transfer.
        if is_cancel_requested(pool, &state.migration_id).await? {
            info!(
                migration_id = %state.migration_id,
                "cancel observed before memory migration; rolling back"
            );
            rollback_precopy(pool, state).await?;
            return Err(ChvError::Internal {
                reason: format!(
                    "migration {} cancelled by operator (before MemoryMigration)",
                    state.migration_id
                ),
            });
        }

        transition_phase(pool, state, MigrationPhase::MemoryMigration).await?;

        let memory_result = tokio::time::timeout(
            Duration::from_secs(timeouts.memory_migration_secs),
            wait_for_memory_migration(pool, state),
        )
        .await;

        match memory_result {
            Ok(Ok(())) => {
                info!(
                    migration_id = %state.migration_id,
                    "memory migration completed"
                );
            }
            Ok(Err(e)) => {
                // Cannot cleanly rollback during memory migration
                error!(
                    migration_id = %state.migration_id,
                    error = %e,
                    "memory migration failed, cannot rollback cleanly"
                );
                transition_phase(pool, state, MigrationPhase::Failed).await?;
                return Err(e);
            }
            Err(_elapsed) => {
                error!(
                    migration_id = %state.migration_id,
                    "memory migration timed out, marking failed (no clean rollback possible)"
                );
                transition_phase(pool, state, MigrationPhase::Failed).await?;
                return Err(ChvError::Internal {
                    reason: format!(
                        "migration {} memory migration timed out after {}s",
                        state.migration_id, timeouts.memory_migration_secs
                    ),
                });
            }
        }

        // Phase 4: Paused - final sync, VM is paused
        // Cooperative cancel check before we explicitly pause the source. After
        // memory migration succeeded, CH holds VM state on the destination, so
        // honoring cancel here means attempting `rollback_paused` (resume on
        // source) — best-effort and may fail if source has already released state.
        // Once paused, source VM state is on destination; cancel becomes "attempt
        // to resume on source" which is best-effort and may fail.
        if is_cancel_requested(pool, &state.migration_id).await? {
            warn!(
                migration_id = %state.migration_id,
                "cancel observed after memory migration; attempting best-effort rollback to source"
            );
            rollback_paused(pool, state, node_client_pool, agent_socket_pattern).await?;
            return Err(ChvError::Internal {
                reason: format!(
                    "migration {} cancelled by operator (post-MemoryMigration, best-effort)",
                    state.migration_id
                ),
            });
        }

        // Explicitly pause the source VM before final sync to ensure no writes
        // can occur during the final data transfer. Cloud Hypervisor's send-migration
        // typically pauses the VM internally, but we enforce it here for coordination.
        let source_socket_for_pause =
            resolve_agent_socket(agent_socket_pattern, &state.source_node_id)?;
        let pause_result = node_client_pool
            .get_or_connect(&state.source_node_id, &source_socket_for_pause)
            .await;

        match pause_result {
            Ok(mut pause_client) => {
                if let Err(e) = pause_client
                    .pause_vm(
                        &state.source_node_id,
                        &state.vm_id,
                        &vm_generation,
                        &state.operation_id,
                        Some("control-plane"),
                    )
                    .await
                {
                    // Log but don't fail — the VM may already be paused by CH's
                    // send-migration protocol. If it truly isn't paused, the final
                    // sync will still work but may have slightly more dirty data.
                    warn!(
                        migration_id = %state.migration_id,
                        vm_id = %state.vm_id,
                        error = %e,
                        "failed to explicitly pause source VM for final sync (may already be paused by CH)"
                    );
                }
                info!(
                    migration_id = %state.migration_id,
                    vm_id = %state.vm_id,
                    "source VM paused for final sync"
                );
            }
            Err(e) => {
                warn!(
                    migration_id = %state.migration_id,
                    vm_id = %state.vm_id,
                    error = %e,
                    "could not connect to source agent for explicit pause (VM may already be paused)"
                );
            }
        }

        transition_phase(pool, state, MigrationPhase::Paused).await?;

        let dest_socket = resolve_agent_socket(agent_socket_pattern, &state.dest_node_id)?;
        let mut dest_client = node_client_pool
            .get_or_connect(&state.dest_node_id, &dest_socket)
            .await?;

        // Resume VM on destination
        let resume_result = tokio::time::timeout(
            Duration::from_secs(timeouts.paused_secs),
            dest_client.resume_vm(
                &state.dest_node_id,
                &state.vm_id,
                &vm_generation,
                &state.operation_id,
                None,
            ),
        )
        .await;

        match resume_result {
            Ok(Ok(_ack)) => {
                info!(
                    migration_id = %state.migration_id,
                    "VM resumed on destination, migration complete"
                );
            }
            Ok(Err(e)) => {
                // Destination failed to resume - try to resume on source
                warn!(
                    migration_id = %state.migration_id,
                    error = %e,
                    "destination resume failed, attempting rollback to source"
                );
                rollback_paused(pool, state, node_client_pool, agent_socket_pattern).await?;
                return Err(e);
            }
            Err(_elapsed) => {
                warn!(
                    migration_id = %state.migration_id,
                    "destination resume timed out, attempting rollback to source"
                );
                rollback_paused(pool, state, node_client_pool, agent_socket_pattern).await?;
                return Err(ChvError::Internal {
                    reason: format!(
                        "migration {} paused phase timed out after {}s",
                        state.migration_id, timeouts.paused_secs
                    ),
                });
            }
        }

        // Phase 5: Completed — transition phase and update placement atomically
        // to prevent split-brain if crash occurs between the two operations.
        complete_migration_atomically(pool, state).await?;

        // Best-effort: send gratuitous ARP from the destination node so
        // peers' ARP caches and the fabric's kernel MAC learning re-point
        // to the VM's new location (ADR-021: correctness never depends on
        // control-plane-pushed FDB entries).
        notify_overlay_after_migration(pool, node_client_pool, agent_socket_pattern, state).await;

        Ok(())
    })
    .await;

    match migration_result {
        Ok(Ok(())) => {
            metrics::counter!("chv_migration_completed_total").increment(1);
            info!(
                migration_id = %state.migration_id,
                "migration completed successfully"
            );
            // Best-effort: disable dirty tracking on source volumes
            disable_source_dirty_tracking(pool, node_client_pool, agent_socket_pattern, state)
                .await;
            Ok(())
        }
        Ok(Err(e)) => {
            metrics::counter!("chv_migration_failed_total").increment(1);
            // Migration failed (already marked Failed or RolledBack in the inner logic)
            // Best-effort: disable dirty tracking on source volumes
            disable_source_dirty_tracking(pool, node_client_pool, agent_socket_pattern, state)
                .await;
            Err(e)
        }
        Err(_elapsed) => {
            metrics::counter!("chv_migration_failed_total").increment(1);
            // Total timeout exceeded
            error!(
                migration_id = %state.migration_id,
                "total migration timeout exceeded"
            );
            transition_phase(pool, state, MigrationPhase::Failed).await?;
            // Best-effort: disable dirty tracking on source volumes
            disable_source_dirty_tracking(pool, node_client_pool, agent_socket_pattern, state)
                .await;
            Err(ChvError::Internal {
                reason: format!(
                    "migration {} exceeded total timeout of {}s",
                    state.migration_id,
                    total_timeout.as_secs()
                ),
            })
        }
    }
}

/// Transition the migration to a new phase, updating the database.
async fn transition_phase(
    pool: &StorePool,
    state: &mut MigrationState,
    new_phase: MigrationPhase,
) -> Result<(), ChvError> {
    state.phase = new_phase;
    update_migration_phase(pool, &state.migration_id, new_phase).await
}

/// Rollback during PreCopyDisk or ConvergingDisk phases.
/// VM continues on source, abort migration, mark as RolledBack.
async fn rollback_precopy(pool: &StorePool, state: &mut MigrationState) -> Result<(), ChvError> {
    warn!(
        migration_id = %state.migration_id,
        phase = %state.phase.as_str(),
        "rolling back migration (precopy/converge phase)"
    );
    transition_phase(pool, state, MigrationPhase::RolledBack).await?;
    Ok(())
}

/// Rollback during Paused phase.
/// If destination fails to resume, try to resume on source.
async fn rollback_paused(
    pool: &StorePool,
    state: &mut MigrationState,
    node_client_pool: &NodeClientPool,
    agent_socket_pattern: &str,
) -> Result<(), ChvError> {
    warn!(
        migration_id = %state.migration_id,
        "attempting rollback: resuming VM on source"
    );

    let source_socket = resolve_agent_socket(agent_socket_pattern, &state.source_node_id)?;
    let resume_source = node_client_pool
        .get_or_connect(&state.source_node_id, &source_socket)
        .await;

    match resume_source {
        Ok(mut client) => {
            let gen = get_vm_generation(pool, &state.vm_id)
                .await
                .unwrap_or_else(|_| "1".to_string());
            match client
                .resume_vm(
                    &state.source_node_id,
                    &state.vm_id,
                    &gen,
                    &state.operation_id,
                    None,
                )
                .await
            {
                Ok(_) => {
                    info!(
                        migration_id = %state.migration_id,
                        "VM resumed on source after failed destination resume"
                    );
                    transition_phase(pool, state, MigrationPhase::RolledBack).await?;
                }
                Err(e) => {
                    error!(
                        migration_id = %state.migration_id,
                        error = %e,
                        "failed to resume VM on source during rollback, marking Failed"
                    );
                    transition_phase(pool, state, MigrationPhase::Failed).await?;
                }
            }
        }
        Err(e) => {
            error!(
                migration_id = %state.migration_id,
                error = %e,
                "failed to connect to source agent during rollback, marking Failed"
            );
            transition_phase(pool, state, MigrationPhase::Failed).await?;
        }
    }

    Ok(())
}

/// Disable dirty tracking on all source volumes after migration completes (success or failure).
///
/// Per ADR-012: "Dirty tracking MUST be disabled on source after migration completes
/// (success or failure)." Leaving it enabled causes ~15-20% I/O degradation.
///
/// This is best-effort: failures are logged as warnings but do not fail the overall operation.
async fn disable_source_dirty_tracking(
    pool: &StorePool,
    node_client_pool: &NodeClientPool,
    agent_socket_pattern: &str,
    state: &MigrationState,
) {
    // Query volumes attached to the migrating VM.
    let volumes: Result<Vec<(String,)>, _> =
        sqlx::query_as("SELECT volume_id FROM volume_desired_state WHERE attached_vm_id = ?")
            .bind(&state.vm_id)
            .fetch_all(pool)
            .await;

    let volume_ids = match volumes {
        Ok(rows) => rows,
        Err(e) => {
            warn!(
                migration_id = %state.migration_id,
                vm_id = %state.vm_id,
                error = %e,
                "failed to query volumes for dirty tracking cleanup (best-effort)"
            );
            return;
        }
    };

    if volume_ids.is_empty() {
        return;
    }

    // Connect to the source agent.
    let source_socket = match resolve_agent_socket(agent_socket_pattern, &state.source_node_id) {
        Ok(socket) => socket,
        Err(e) => {
            warn!(
                migration_id = %state.migration_id,
                vm_id = %state.vm_id,
                error = %e,
                "failed to resolve source agent socket (best-effort)"
            );
            return;
        }
    };
    let mut source_client = match node_client_pool
        .get_or_connect(&state.source_node_id, &source_socket)
        .await
    {
        Ok(client) => client,
        Err(e) => {
            warn!(
                migration_id = %state.migration_id,
                source_node_id = %state.source_node_id,
                error = %e,
                "failed to connect to source agent for dirty tracking cleanup (best-effort)"
            );
            return;
        }
    };

    // For each volume, send a desired state update that disables dirty tracking.
    // The agent interprets a volume spec with `dirty_tracking: false` as a signal
    // to stop tracking dirty blocks on the volume.
    for (volume_id,) in &volume_ids {
        let spec_json = serde_json::json!({
            "dirty_tracking": false
        });
        let spec_bytes = serde_json::to_vec(&spec_json).unwrap_or_default();

        match source_client
            .apply_volume_desired_state(
                &state.source_node_id,
                volume_id,
                "0", // generation 0 — best-effort cleanup, no ordering guarantee needed
                spec_bytes,
                &state.operation_id,
                Some("control-plane"),
            )
            .await
        {
            Ok(_) => {
                info!(
                    migration_id = %state.migration_id,
                    volume_id = %volume_id,
                    source_node_id = %state.source_node_id,
                    "disabled dirty tracking on source volume"
                );
            }
            Err(e) => {
                warn!(
                    migration_id = %state.migration_id,
                    volume_id = %volume_id,
                    source_node_id = %state.source_node_id,
                    error = %e,
                    "failed to disable dirty tracking on source volume (best-effort)"
                );
            }
        }
    }
}

/// Query the volume IDs attached to a VM.
/// Wait for disk convergence by polling the migration record.
/// Wait for disk convergence by polling the migration record.
///
/// The agent reports stord's per-volume status (max convergence round, max
/// dirty blocks remaining) via telemetry roughly every 5 s; this loop polls
/// the persisted row on the same cadence.
///
/// Convergence is declared when either:
/// - the agent-reported dirty count drops to or below the configured
///   threshold AND the disk transfer has demonstrably reached dirty sync
///   (`convergence_round >= 1` — see the guard comment at the primary check
///   below), or
/// - the agent reports the disk work done (`MemoryMigration` phase or
///   beyond) — the path diskless VMs converge through, since they skip the
///   disk loop and never run a dirty round.
///
/// Safety nets:
/// - Row freshness: if the row stops being updated the agent has stopped
///   reporting, and the migration is rolled back.
/// - Dirty-sync caps: once dirty sync has begun (`round >= 1`), the loop
///   proceeds to the memory phase after the round cap or a bounded amount
///   of dirty-sync poll time without convergence — matching stord's own
///   forced cutover at MAX_DIRTY_ROUNDS (the sender moves to final sync
///   there, it does not fail). The memory phase's timeout budget bounds
///   the remaining work.
/// - Max total: 7200 s hard cap, and the outer per-phase timeout
///   (PhaseTimeouts) always applies.
pub(crate) async fn wait_for_convergence(
    pool: &StorePool,
    state: &MigrationState,
) -> Result<(), ChvError> {
    let max_rounds = state.config.max_convergence_rounds;
    let threshold = state.config.dirty_threshold_blocks as i64;
    let mut poll_count: u32 = 0;
    let mut dirty_sync_polls: u32 = 0;
    let started = tokio::time::Instant::now();

    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        poll_count += 1;

        // Cooperative cancel check: observe the flag at every iteration. This
        // is a safe point — we have not yet read other state for this round,
        // so an early return here cannot leave anything inconsistent.
        if is_cancel_requested(pool, &state.migration_id).await? {
            info!(
                migration_id = %state.migration_id,
                round = state.convergence_round,
                "cancel observed during convergence; rolling back"
            );
            return Err(ChvError::Internal {
                reason: format!(
                    "migration {} cancelled by operator during convergence",
                    state.migration_id
                ),
            });
        }

        // Enforce max total time
        if started.elapsed().as_secs() >= MAX_TOTAL_SECS {
            warn!(
                migration_id = %state.migration_id,
                elapsed_secs = started.elapsed().as_secs(),
                "convergence exceeded max total time (7200s), forcing transition"
            );
            return Err(ChvError::Internal {
                reason: format!(
                    "migration {} convergence timeout: exceeded max total time ({}s)",
                    state.migration_id, MAX_TOTAL_SECS
                ),
            });
        }

        let row: Option<(String, i64, i64, String)> = sqlx::query_as(
            "SELECT phase, convergence_round, dirty_blocks_remaining, updated_at FROM migrations WHERE migration_id = ?",
        )
        .bind(&state.migration_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| ChvError::Internal {
            reason: format!("failed to poll migration convergence: {e}"),
        })?;

        match row {
            Some((phase, round, dirty_remaining, updated_at)) => {
                if phase == "Failed" || phase == "RolledBack" {
                    return Err(ChvError::Internal {
                        reason: format!(
                            "migration {} entered terminal phase {} during convergence",
                            state.migration_id, phase
                        ),
                    });
                }

                // Primary check: agent-reported dirty blocks below
                // threshold — guarded by evidence that the disk
                // transfer has actually reached dirty sync (#582).
                //
                // `dirty_blocks_remaining` is 0 until stord's first
                // dirty-sync round snapshots the bitmap (and the
                // migration row is created with 0), so a bare
                // `dirty <= threshold` is vacuously true during bulk
                // copy: it declared convergence on the first poll of
                // any migration while the transfer was still running,
                // and the memory phase then ran under its own timeout
                // budget with the VM paused for the remainder of the
                // disk transfer.
                //
                // `round >= 1` closes that: stord sets
                // `convergence_round` to 1 at the top of dirty-sync
                // round 1 — before any dirty count exists — and a
                // Completed task always passes through at least round
                // 1, so round >= 1 means bulk copy has finished. It
                // also covers the pre-telemetry window (the row is
                // created with round 0). The agent reports the max
                // round across volumes, so this holds once ANY volume
                // has finished bulk copy.
                //
                // Disclosed residual (#582, see the adoption record):
                // in multi-volume skew — an early volume converged
                // (round >= 1, dirty 0) while a later volume is still
                // in bulk copy — the max-dirty signal is 0 and this
                // check passes early. The row cannot express that
                // state today: stord's `bytes_transferred` excludes
                // bulk bytes and the agent's phase vocabulary has no
                // "all volumes finished bulk" value. The migration
                // still completes correctly (the final sync waits for
                // every volume); the cost is that the memory phase's
                // timeout budget starts early. Pinned by
                // `test_wait_for_convergence_multi_volume_skew_residual_is_pinned`.
                // (`dirty_remaining >= 0` is deliberate defense, not a
                // tautology: the column is INTEGER NOT NULL but SQLite
                // integers can be negative, and corrupt telemetry must
                // not count as "below threshold".)
                if round >= 1 && dirty_remaining >= 0 && dirty_remaining <= threshold {
                    info!(
                        migration_id = %state.migration_id,
                        round = round,
                        dirty_remaining = dirty_remaining,
                        "convergence achieved: dirty blocks below threshold"
                    );
                    return Ok(());
                }

                // Agent-reported disk-work-done signal: the agent
                // reports `MemoryMigration` once all volumes completed
                // — or, in default mode, at the final-sync pause — and
                // immediately for diskless VMs, which skip the disk
                // loop entirely (no dirty rounds, so the round signal
                // above can never fire for them). `Paused`/`Completed`
                // are further still (accepted defensively: the agent
                // does not report `Paused`/`Completed` during this
                // window today, but they are unambiguous disk-done
                // signals if its vocabulary grows). In multi-volume
                // skew this fires early too — the early volume's
                // final-sync pause — the phase-based trigger of the
                // disclosed residual (the round-based trigger is at
                // the primary check). The CP last wrote
                // `ConvergingDisk` to this row, so any of these phases
                // was written by the agent. Without this signal a
                // diskless migration could never converge (pre-fix it
                // "worked" only via the vacuous first-poll check).
                if phase == "MemoryMigration" || phase == "Paused" || phase == "Completed" {
                    info!(
                        migration_id = %state.migration_id,
                        phase = %phase,
                        "convergence achieved: agent reported disk work complete"
                    );
                    return Ok(());
                }

                // Liveness: the agent's disk-poll loop reports every
                // ~5 s and the agent daemon flushes the pending
                // telemetry queue every ~5 s tick, refreshing
                // `updated_at` even when the values are unchanged
                // (bulk copy freezes them at 0 — bulk progress is
                // invisible in this row). A row older than
                // STALE_ROW_SECS means the agent stopped reporting.
                // (Both the row's timestamp and `Utc::now()` come from
                // the same host, so only a forward clock step > 90 s
                // during a healthy migration could false-fire this —
                // fail-safe: it rolls back, it never corrupts.)
                let row_age_secs = match chrono::DateTime::parse_from_rfc3339(&updated_at) {
                    Ok(ts) => Some(chrono::Utc::now().signed_duration_since(ts).num_seconds()),
                    Err(e) => {
                        // The column is written by our own strftime
                        // ('%Y-%m-%dT%H:%M:%SZ'), so this should be
                        // impossible — fail open for this poll (the
                        // outer timeouts still bound the phase) but
                        // say so.
                        warn!(
                            migration_id = %state.migration_id,
                            updated_at = %updated_at,
                            error = %e,
                            "unparseable updated_at; freshness check skipped for this poll"
                        );
                        None
                    }
                };
                if row_age_secs.is_some_and(|age| age > STALE_ROW_SECS) {
                    warn!(
                        migration_id = %state.migration_id,
                        round = round,
                        dirty_remaining = dirty_remaining,
                        row_age_secs = row_age_secs,
                        "migration row stale: agent stopped reporting"
                    );
                    return Err(ChvError::Internal {
                        reason: format!(
                            "migration {} stalled: no telemetry update for over {}s (round {})",
                            state.migration_id, STALE_ROW_SECS, round
                        ),
                    });
                }

                // Dirty-sync time bookkeeping: count polls only once
                // dirty sync has begun — bulk-copy time (round 0) must
                // not consume the dirty-sync budget.
                if round >= 1 {
                    dirty_sync_polls += 1;
                }

                // Dirty-sync caps: once round >= 1, bound how long we
                // wait for the dirty count to fall below threshold —
                // the round cap, or a bounded amount of dirty-sync
                // poll time (bulk-copy polls are excluded above). On
                // cap we PROCEED to the memory phase — matching
                // stord's own forced cutover at MAX_DIRTY_ROUNDS (the
                // sender moves to final sync there, it does not fail)
                // and this branch's original stated intent. The memory
                // phase's timeout budget bounds the remaining work.
                //
                // During round 0 (bulk copy) these caps must not fire
                // at all: bulk progress is invisible in the row, so a
                // legitimately long bulk copy would otherwise be
                // mistaken for non-convergence. Round-0 time is bounded
                // by MAX_TOTAL_SECS and the outer phase timeout.
                // `max_convergence_rounds` is operator-supplied and
                // unvalidated above the proto layer's defaults, so bound
                // the multiplication against overflow (a giant config
                // value saturates the cap rather than wrapping).
                if round >= 1
                    && (round >= max_rounds as i64
                        || dirty_sync_polls >= max_rounds.saturating_mul(6))
                {
                    warn!(
                        migration_id = %state.migration_id,
                        rounds = round,
                        poll_count = poll_count,
                        dirty_sync_polls = dirty_sync_polls,
                        dirty_remaining = dirty_remaining,
                        "convergence round limit reached, proceeding to memory migration"
                    );
                    return Ok(());
                }
            }
            None => {
                return Err(ChvError::Internal {
                    reason: format!("migration {} record not found", state.migration_id),
                });
            }
        }
    }
}

/// Wait for memory migration to complete by polling the migration record.
async fn wait_for_memory_migration(
    pool: &StorePool,
    state: &MigrationState,
) -> Result<(), ChvError> {
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;

        let row: Option<(String,)> =
            sqlx::query_as("SELECT phase FROM migrations WHERE migration_id = ?")
                .bind(&state.migration_id)
                .fetch_optional(pool)
                .await
                .map_err(|e| ChvError::Internal {
                    reason: format!("failed to poll migration memory phase: {e}"),
                })?;

        match row {
            Some((phase,)) => {
                // Agent transitions to Paused when memory migration is done
                if phase == "Paused" || phase == "Completed" {
                    return Ok(());
                }
                if phase == "Failed" || phase == "RolledBack" {
                    return Err(ChvError::Internal {
                        reason: format!(
                            "migration {} entered terminal phase {} during memory migration",
                            state.migration_id, phase
                        ),
                    });
                }
                // Still in MemoryMigration, keep waiting
            }
            None => {
                return Err(ChvError::Internal {
                    reason: format!("migration {} record not found", state.migration_id),
                });
            }
        }
    }
}

/// Update the migration phase in the database.
async fn update_migration_phase(
    pool: &StorePool,
    migration_id: &str,
    phase: MigrationPhase,
) -> Result<(), ChvError> {
    let completed_at = if matches!(
        phase,
        MigrationPhase::Completed | MigrationPhase::Failed | MigrationPhase::RolledBack
    ) {
        Some("strftime('%Y-%m-%dT%H:%M:%SZ', 'now')")
    } else {
        None
    };

    if completed_at.is_some() {
        sqlx::query(
            r#"UPDATE migrations
               SET phase = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                   completed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
               WHERE migration_id = ?"#,
        )
        .bind(phase.as_str())
        .bind(migration_id)
        .execute(pool)
        .await
        .map_err(|e| ChvError::Internal {
            reason: format!("failed to update migration phase: {e}"),
        })?;
    } else {
        sqlx::query(
            r#"UPDATE migrations
               SET phase = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
               WHERE migration_id = ?"#,
        )
        .bind(phase.as_str())
        .bind(migration_id)
        .execute(pool)
        .await
        .map_err(|e| ChvError::Internal {
            reason: format!("failed to update migration phase: {e}"),
        })?;
    }

    Ok(())
}

/// Check whether a cooperative cancel has been requested for the given migration.
///
/// Cancel is signalled by the `cancel_requested_at` column being non-NULL. This
/// function is invoked at safe points by the long-running migration loop so that
/// an operator-initiated cancel can be observed without forcing a hard abort.
async fn is_cancel_requested(pool: &StorePool, migration_id: &str) -> Result<bool, ChvError> {
    let result: Option<Option<String>> =
        sqlx::query_scalar("SELECT cancel_requested_at FROM migrations WHERE migration_id = ?")
            .bind(migration_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| ChvError::Internal {
                reason: format!("failed to check cancel flag: {e}"),
            })?;
    Ok(result.flatten().is_some())
}

/// Outcome of a cooperative cancel request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelMigrationOutcome {
    /// Cancel flag has been set; migration loop will observe it at the next safe point.
    Requested,
    /// Cancel flag was already set on a prior call; idempotent no-op.
    AlreadyRequested,
    /// Migration is already in a terminal phase (Completed/Failed/RolledBack); no-op.
    AlreadyTerminal,
    /// No migration with the given ID was found.
    NotFound,
}

/// Public API: request a cooperative cancel for an in-flight migration.
///
/// This is a sideband signal — it does not change the FSM phase. The long-running
/// migration loop polls the `cancel_requested_at` column at safe points and
/// transitions to `RolledBack` (or best-effort recovery during MemoryMigration /
/// Paused) when the flag is observed.
///
/// The call is idempotent: setting the flag twice is harmless. Once the migration
/// is in a terminal phase (Completed, Failed, RolledBack), the call is a no-op
/// because rollback is no longer meaningful.
///
/// Cancel is honored at:
///  - Pending → no-op until the loop starts; observed at PreCopyDisk entry.
///  - PreCopyDisk → observed between phases; triggers `rollback_precopy`.
///  - ConvergingDisk → observed at top of each `wait_for_convergence` iteration.
///  - Paused → operator must call `rollback_paused` separately; here we just mark
///    the flag, and the existing destination-resume failure path takes over if
///    relevant. Once paused, source VM state is on destination; cancel becomes
///    "attempt to resume on source" which is best-effort and may fail.
///  - MemoryMigration → flag is observable but cancel is best-effort: CH may be
///    mid-transfer with no clean rollback path.
///  - Completed/Failed/RolledBack → no-op (`AlreadyTerminal`).
pub async fn request_migration_cancel(
    pool: &StorePool,
    migration_id: &str,
) -> Result<CancelMigrationOutcome, ChvError> {
    // Look up current phase and existing flag in one read.
    let row: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT phase, cancel_requested_at FROM migrations WHERE migration_id = ?")
            .bind(migration_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| ChvError::Internal {
                reason: format!("failed to look up migration {migration_id} for cancel: {e}"),
            })?;

    let (phase, existing_flag) = match row {
        Some(r) => r,
        None => return Ok(CancelMigrationOutcome::NotFound),
    };

    // Once terminal, cancel is meaningless — do not flip the flag.
    if matches!(phase.as_str(), "Completed" | "Failed" | "RolledBack") {
        return Ok(CancelMigrationOutcome::AlreadyTerminal);
    }

    if existing_flag.is_some() {
        return Ok(CancelMigrationOutcome::AlreadyRequested);
    }

    // Set the flag. The phase guard in WHERE is belt-and-suspenders against a
    // race where the migration completed between the SELECT above and this UPDATE.
    let affected = sqlx::query(
        r#"UPDATE migrations
              SET cancel_requested_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
            WHERE migration_id = ?
              AND cancel_requested_at IS NULL
              AND phase NOT IN ('Completed', 'Failed', 'RolledBack')"#,
    )
    .bind(migration_id)
    .execute(pool)
    .await
    .map_err(|e| ChvError::Internal {
        reason: format!("failed to set cancel flag for {migration_id}: {e}"),
    })?
    .rows_affected();

    if affected == 0 {
        // Race: another writer set the flag, or migration reached terminal phase.
        // Re-read to give a precise outcome.
        let phase_now: Option<String> =
            sqlx::query_scalar("SELECT phase FROM migrations WHERE migration_id = ?")
                .bind(migration_id)
                .fetch_optional(pool)
                .await
                .map_err(|e| ChvError::Internal {
                    reason: format!("failed to recheck phase after cancel race: {e}"),
                })?;
        return Ok(match phase_now.as_deref() {
            Some("Completed") | Some("Failed") | Some("RolledBack") => {
                CancelMigrationOutcome::AlreadyTerminal
            }
            Some(_) => CancelMigrationOutcome::AlreadyRequested,
            None => CancelMigrationOutcome::NotFound,
        });
    }

    info!(
        migration_id = %migration_id,
        phase = %phase,
        "cancel requested for migration"
    );
    metrics::counter!("chv_migration_cancel_requested_total").increment(1);
    Ok(CancelMigrationOutcome::Requested)
}

/// Create a migration record in the database.
pub async fn create_migration_record(
    pool: &StorePool,
    state: &MigrationState,
) -> Result<(), ChvError> {
    sqlx::query(
        r#"INSERT INTO migrations (migration_id, operation_id, vm_id, source_node_id, destination_node_id, phase)
           VALUES (?, ?, ?, ?, ?, ?)"#,
    )
    .bind(&state.migration_id)
    .bind(&state.operation_id)
    .bind(&state.vm_id)
    .bind(&state.source_node_id)
    .bind(&state.dest_node_id)
    .bind(state.phase.as_str())
    .execute(pool)
    .await
    .map_err(|e| ChvError::Internal {
        reason: format!("failed to create migration record: {e}"),
    })?;
    Ok(())
}

/// Update migration progress in the database (called from telemetry).
///
/// `node_id` is the authenticated sender (the control plane pins it to the
/// mTLS peer certificate before calling this). The update is scoped to a
/// migration the sender actually participates in, so a compromised node
/// cannot overwrite another node's migration row even though it holds a valid
/// cluster certificate. A late report for a migration that already reached a
/// terminal phase is dropped (returns `Ok`): the reaper may have failed the
/// row and resumed the source VM, and a queued `ConvergingDisk` report must
/// never resurrect it.
#[allow(clippy::too_many_arguments)]
pub async fn update_migration_progress(
    pool: &StorePool,
    node_id: &str,
    vm_id: &str,
    operation_id: &str,
    phase: i32,
    bytes_transferred: u64,
    total_bytes: u64,
    convergence_round: u32,
    dirty_blocks_remaining: u64,
) -> Result<(), ChvError> {
    let phase_str = proto_phase_to_str(phase);

    let completed_at_clause =
        if phase_str == "Completed" || phase_str == "Failed" || phase_str == "RolledBack" {
            ", completed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')"
        } else {
            ""
        };

    let sql = format!(
        r#"UPDATE migrations
           SET phase = ?,
               bytes_transferred = ?,
               total_bytes = ?,
               convergence_round = ?,
               dirty_blocks_remaining = ?,
               updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
               {}
           WHERE vm_id = ? AND operation_id = ?
             AND (source_node_id = ? OR destination_node_id = ?)
             AND phase NOT IN ('Completed', 'Failed', 'RolledBack')"#,
        completed_at_clause
    );

    let result = sqlx::query(&sql)
        .bind(phase_str)
        .bind(bytes_transferred as i64)
        .bind(total_bytes as i64)
        .bind(convergence_round as i32)
        .bind(dirty_blocks_remaining as i64)
        .bind(vm_id)
        .bind(operation_id)
        .bind(node_id)
        .bind(node_id)
        .execute(pool)
        .await
        .map_err(|e| ChvError::Internal {
            reason: format!("failed to update migration progress: {e}"),
        })?;

    if result.rows_affected() > 0 {
        return Ok(());
    }

    // The guarded update matched nothing. Classify the no-op so the caller
    // can distinguish "late terminal report, safely dropped" (Ok) from
    // "this node does not own this migration" (denied) and "no such row".
    let row: Option<(String, String, String)> = sqlx::query_as(
        r#"SELECT source_node_id, destination_node_id, phase
           FROM migrations
           WHERE vm_id = ? AND operation_id = ?
           ORDER BY started_at DESC
           LIMIT 1"#,
    )
    .bind(vm_id)
    .bind(operation_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| ChvError::Internal {
        reason: format!("failed to classify migration progress update: {e}"),
    })?;

    let Some((source_node_id, destination_node_id, current_phase)) = row else {
        // Progress can race the control plane's own record creation; treat a
        // missing row as an idempotent no-op rather than a poison-pill error
        // that the agent would retry forever.
        return Ok(());
    };

    if source_node_id != node_id && destination_node_id != node_id {
        return Err(ChvError::AccessDenied {
            resource: format!("migration {operation_id}"),
            reason: format!("node {node_id} is neither the source nor destination"),
        });
    }

    if matches!(
        current_phase.as_str(),
        "Completed" | "Failed" | "RolledBack"
    ) {
        // Terminal: the report arrived too late (reaper won). Dropping it is
        // correct; returning an error would make the agent retry forever.
        return Ok(());
    }

    // The row exists, the sender participates, and the phase is still
    // non-terminal — the update should have matched. Treat this as an
    // internal inconsistency rather than silently acknowledging it.
    Err(ChvError::Internal {
        reason: format!(
            "migration progress update matched no rows for operation {operation_id} in phase {current_phase}"
        ),
    })
}

/// Fetch the current generation for a VM from the desired state store.
async fn get_vm_generation(pool: &StorePool, vm_id: &str) -> Result<String, ChvError> {
    let gen: Option<(Option<i64>,)> =
        sqlx::query_as("SELECT generation FROM vm_desired_state WHERE vm_id = ?")
            .bind(vm_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| ChvError::Internal {
                reason: format!("failed to fetch generation for VM {vm_id}: {e}"),
            })?;

    match gen {
        Some((Some(g),)) => Ok(g.to_string()),
        _ => Ok("1".to_string()),
    }
}

/// Get VM disk and memory sizes in GB for timeout calculation.
async fn get_vm_sizes(pool: &StorePool, vm_id: &str) -> Result<(u64, u64), ChvError> {
    let row: Option<(Option<i64>, Option<i64>)> = sqlx::query_as(
        r#"SELECT
            (SELECT COALESCE(SUM(v.capacity_bytes), 0)
             FROM volume_desired_state vds
             JOIN volumes v ON v.volume_id = vds.volume_id
             WHERE vds.attached_vm_id = ?) as disk_bytes,
            vms_ds.memory_bytes
           FROM vm_desired_state vms_ds
           WHERE vms_ds.vm_id = ?"#,
    )
    .bind(vm_id)
    .bind(vm_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| ChvError::Internal {
        reason: format!("failed to query VM sizes for migration timeout: {e}"),
    })?;

    let (disk_bytes, memory_bytes) = row.unwrap_or((Some(0), Some(0)));
    let disk_gb = (disk_bytes.unwrap_or(0) as u64) / (1024 * 1024 * 1024);
    let memory_gb = (memory_bytes.unwrap_or(0) as u64) / (1024 * 1024 * 1024);

    Ok((disk_gb.max(1), memory_gb.max(1)))
}

fn migration_bridge_name(net_id: &str) -> String {
    if net_id == "default" {
        return "chvbr0".to_string();
    }
    let candidate = format!("br-{}", net_id);
    if candidate.len() <= 15 {
        return candidate;
    }
    let prefix: String = net_id.chars().take(8).collect();
    let hash = {
        let mut h: u32 = 0x811c9dc5;
        for b in net_id.as_bytes() {
            h = h.wrapping_mul(0x01000193) ^ (*b as u32);
        }
        format!("{:04x}", h & 0xffff)
    };
    format!("br-{}{}", prefix, hash)
}

/// Atomically mark migration as Completed AND update VM placement.
/// Both operations run in a single SQLite transaction to prevent split-brain
/// if a crash occurs between them.
async fn complete_migration_atomically(
    pool: &StorePool,
    state: &mut MigrationState,
) -> Result<(), ChvError> {
    let mut tx = pool.begin().await.map_err(|e| ChvError::Internal {
        reason: format!("failed to begin transaction for migration completion: {e}"),
    })?;

    sqlx::query(
        r#"UPDATE migrations
           SET phase = 'Completed',
               updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
               completed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
           WHERE migration_id = ?"#,
    )
    .bind(&state.migration_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| ChvError::Internal {
        reason: format!("failed to mark migration completed: {e}"),
    })?;

    sqlx::query("UPDATE vm_desired_state SET target_node_id = ? WHERE vm_id = ?")
        .bind(&state.dest_node_id)
        .bind(&state.vm_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| ChvError::Internal {
            reason: format!("failed to update VM placement: {e}"),
        })?;

    tx.commit().await.map_err(|e| ChvError::Internal {
        reason: format!("failed to commit migration completion transaction: {e}"),
    })?;

    state.phase = MigrationPhase::Completed;
    Ok(())
}

/// Resolve agent socket path for a node.
///
/// When the pattern substitutes `{node_id}`, the id must be a single safe
/// path component: node ids reach this join from request parsing, the
/// nodes table, and migration state, and a traversal-shaped id
/// (`../../x`) would steer the control plane at an arbitrary local
/// socket. Patterns without `{node_id}` never substitute and are
/// unaffected.
pub(crate) fn resolve_agent_socket(pattern: &str, node_id: &str) -> Result<PathBuf, ChvError> {
    if !pattern.contains("{node_id}") {
        return Ok(PathBuf::from(pattern));
    }
    if !chv_common::is_safe_path_component(node_id) {
        return Err(ChvError::InvalidArgument {
            field: "node_id".to_string(),
            reason: format!("node id {node_id:?} is not a safe path component"),
        });
    }
    Ok(PathBuf::from(pattern.replace("{node_id}", node_id)))
}

/// Convert proto MigrationPhase enum int to string.
fn proto_phase_to_str(phase: i32) -> &'static str {
    match phase {
        x if x == proto::MigrationPhase::Pending as i32 => "Pending",
        x if x == proto::MigrationPhase::PrecopyDisk as i32 => "PreCopyDisk",
        x if x == proto::MigrationPhase::ConvergingDisk as i32 => "ConvergingDisk",
        x if x == proto::MigrationPhase::MemoryMigration as i32 => "MemoryMigration",
        x if x == proto::MigrationPhase::Paused as i32 => "Paused",
        x if x == proto::MigrationPhase::Completed as i32 => "Completed",
        x if x == proto::MigrationPhase::Failed as i32 => "Failed",
        x if x == proto::MigrationPhase::RolledBack as i32 => "RolledBack",
        _ => "Unknown",
    }
}

/// VM NIC info needed for the post-migration gratuitous ARP.
struct VmNicInfo {
    network_id: String,
    ip_address: String,
}

/// Fetch all NICs for a VM that are on overlay networks (vni > 0).
async fn get_vm_overlay_nics(pool: &StorePool, vm_id: &str) -> Result<Vec<VmNicInfo>, ChvError> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        r#"SELECT vn.network_id, vn.ip_address
           FROM vm_nic_desired_state vn
           JOIN networks n ON n.network_id = vn.network_id
           WHERE vn.vm_id = ? AND COALESCE(n.vni, 0) > 0
             AND vn.mac_address IS NOT NULL"#,
    )
    .bind(vm_id)
    .fetch_all(pool)
    .await
    .map_err(|e| ChvError::Internal {
        reason: format!("failed to query VM NIC overlay info: {e}"),
    })?;

    Ok(rows
        .into_iter()
        .map(|(network_id, ip_address)| VmNicInfo {
            network_id,
            ip_address,
        })
        .collect())
}

/// Best-effort post-migration overlay notification.
///
/// Sends gratuitous ARP from the destination node for every overlay-backed
/// NIC so peers' ARP caches and the fabric's kernel MAC learning re-point
/// to the VM's new location. Per ADR-021 the control plane no longer
/// re-points per-VM unicast FDB entries (the legacy datapath was retired);
/// fabric plan re-dispatch on placement change is owned by the
/// orchestrator's UpdateOverlay arm. Failures are logged as warnings but
/// do not fail the migration.
async fn notify_overlay_after_migration(
    pool: &StorePool,
    node_client_pool: &NodeClientPool,
    agent_socket_pattern: &str,
    state: &MigrationState,
) {
    let nics = match get_vm_overlay_nics(pool, &state.vm_id).await {
        Ok(nics) => nics,
        Err(e) => {
            warn!(
                migration_id = %state.migration_id,
                vm_id = %state.vm_id,
                error = %e,
                "failed to query VM NICs for post-migration overlay update"
            );
            return;
        }
    };

    if nics.is_empty() {
        return;
    }

    // Bridge name follows the standard naming convention on the node
    // (first 8 chars of network_id prefixed with "br-"). The agent resolves
    // the actual interface; we pass our best guess for the ARP request.
    for nic in &nics {
        let bridge_name = migration_bridge_name(&nic.network_id);

        // Send gratuitous ARP from the destination node: the GARP is
        // flooded across the fabric, and every peer's kernel MAC learning
        // re-points the VM's MAC to its new VTEP.
        let dest_socket = match resolve_agent_socket(agent_socket_pattern, &state.dest_node_id) {
            Ok(socket) => socket,
            Err(e) => {
                warn!(
                    migration_id = %state.migration_id,
                    vm_id = %state.vm_id,
                    error = %e,
                    "failed to resolve destination agent socket (best-effort)"
                );
                continue;
            }
        };
        match node_client_pool
            .get_or_connect(&state.dest_node_id, &dest_socket)
            .await
        {
            Ok(mut client) => {
                if let Err(e) = client
                    .send_gratuitous_arp(
                        &state.dest_node_id,
                        &nic.network_id,
                        &nic.ip_address,
                        &bridge_name,
                        &state.operation_id,
                        Some("control-plane"),
                    )
                    .await
                {
                    warn!(
                        migration_id = %state.migration_id,
                        vm_id = %state.vm_id,
                        network_id = %nic.network_id,
                        vm_ip = %nic.ip_address,
                        error = %e,
                        "post-migration gratuitous ARP failed (best-effort)"
                    );
                }
            }
            Err(e) => {
                warn!(
                    migration_id = %state.migration_id,
                    vm_id = %state.vm_id,
                    dest_node_id = %state.dest_node_id,
                    error = %e,
                    "failed to connect to dest node for gratuitous ARP (best-effort)"
                );
            }
        }
    }

    info!(
        migration_id = %state.migration_id,
        vm_id = %state.vm_id,
        nic_count = nics.len(),
        "post-migration overlay update completed"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_agent_socket_rejects_traversal_shaped_node_ids() {
        // A substituting pattern must never produce a path outside the
        // intended socket directory: traversal-shaped node ids are
        // rejected at the substitution boundary. (The request boundary and
        // the enrollment gate keep such ids out of the system; this is the
        // last line for ids that predate those gates.)
        assert!(resolve_agent_socket("/run/chv/agent-{node_id}.sock", "../../etc/passwd").is_err());
        assert!(resolve_agent_socket("/run/chv/agent-{node_id}.sock", "a/b").is_err());
        assert!(resolve_agent_socket("/run/chv/agent-{node_id}.sock", "..").is_err());
        assert!(resolve_agent_socket("/run/chv/agent-{node_id}.sock", "node-a").is_ok());
        // Printable ids (e.g. colons) are legal filename components.
        assert!(resolve_agent_socket("/run/chv/agent-{node_id}.sock", "node:1").is_ok());
        // Patterns without substitution are unaffected regardless of the id.
        assert_eq!(
            resolve_agent_socket("/run/chv/agent.sock", "../../x").unwrap(),
            std::path::PathBuf::from("/run/chv/agent.sock")
        );
    }

    #[test]
    fn test_migration_phase_roundtrip() {
        let phases = [
            MigrationPhase::Pending,
            MigrationPhase::PreCopyDisk,
            MigrationPhase::ConvergingDisk,
            MigrationPhase::MemoryMigration,
            MigrationPhase::Paused,
            MigrationPhase::Completed,
            MigrationPhase::Failed,
            MigrationPhase::RolledBack,
        ];

        for phase in &phases {
            let s = phase.as_str();
            let recovered = MigrationPhase::parse(s).expect("should parse");
            assert_eq!(*phase, recovered);
        }
    }

    #[test]
    fn test_migration_phase_transitions() {
        // Valid transitions: Pending -> PreCopyDisk -> ConvergingDisk -> MemoryMigration -> Paused -> Completed
        let transitions = [
            (MigrationPhase::Pending, MigrationPhase::PreCopyDisk),
            (MigrationPhase::PreCopyDisk, MigrationPhase::ConvergingDisk),
            (
                MigrationPhase::ConvergingDisk,
                MigrationPhase::MemoryMigration,
            ),
            (MigrationPhase::MemoryMigration, MigrationPhase::Paused),
            (MigrationPhase::Paused, MigrationPhase::Completed),
        ];

        for (from, to) in &transitions {
            assert_ne!(from, to, "transition should change phase");
        }
    }

    #[test]
    fn test_rollback_phases() {
        // PreCopyDisk and ConvergingDisk can rollback to RolledBack
        let rollbackable = [MigrationPhase::PreCopyDisk, MigrationPhase::ConvergingDisk];
        for phase in &rollbackable {
            assert!(
                matches!(
                    phase,
                    MigrationPhase::PreCopyDisk | MigrationPhase::ConvergingDisk
                ),
                "phase {:?} should be rollbackable",
                phase
            );
        }

        // MemoryMigration cannot cleanly rollback - goes to Failed
        assert_eq!(MigrationPhase::MemoryMigration.as_str(), "MemoryMigration");

        // Paused can rollback by resuming on source
        assert_eq!(MigrationPhase::Paused.as_str(), "Paused");
    }

    #[test]
    fn test_phase_timeout_calculation() {
        // 100GB disk, 16GB memory, default multiplier
        let timeouts = PhaseTimeouts::calculate(100, 16, 1.0);

        assert_eq!(timeouts.precopy_disk_secs, 6000); // 100 * 60
        assert_eq!(timeouts.converging_disk_per_round_secs, 300);
        assert_eq!(timeouts.converging_disk_total_secs, 3000);
        assert_eq!(timeouts.memory_migration_secs, 600); // 16 * 30 + 120
        assert_eq!(timeouts.paused_secs, 60);
        assert_eq!(timeouts.total_secs, 6000 + 3000 + 600 + 60 + 300); // 9960
    }

    #[test]
    fn test_phase_timeout_minimum_sizes() {
        // 0GB disk, 0GB memory - should use minimum of 1GB
        let timeouts = PhaseTimeouts::calculate(0, 0, 1.0);

        assert_eq!(timeouts.precopy_disk_secs, 60); // max(0,1) * 60
        assert_eq!(timeouts.memory_migration_secs, 150); // max(0,1) * 30 + 120
    }

    #[test]
    fn test_migration_config_defaults() {
        let config = MigrationConfig::default();
        assert_eq!(config.dirty_threshold_blocks, 1024);
        assert_eq!(config.max_convergence_rounds, 10);
        assert_eq!(config.block_size_bytes, 4_194_304);
        assert_eq!(config.total_timeout_seconds, 0);
        assert_eq!(config.timeout_multiplier, 1.0);
    }

    #[test]
    fn test_migration_config_from_correlation_id() {
        let corr =
            "source=node-001:dest=node-002:threshold=512:rounds=5:block_size=2097152:timeout=3600";
        let (source, dest, config) = MigrationConfig::from_correlation_id(corr);

        assert_eq!(source, "node-001");
        assert_eq!(dest, "node-002");
        assert_eq!(config.dirty_threshold_blocks, 512);
        assert_eq!(config.max_convergence_rounds, 5);
        assert_eq!(config.block_size_bytes, 2_097_152);
        assert_eq!(config.total_timeout_seconds, 3600);
    }

    #[test]
    fn test_migration_config_from_correlation_id_partial() {
        let corr = "source=node-a:dest=node-b";
        let (source, dest, config) = MigrationConfig::from_correlation_id(corr);

        assert_eq!(source, "node-a");
        assert_eq!(dest, "node-b");
        // Should use defaults for missing fields
        assert_eq!(config.dirty_threshold_blocks, 1024);
        assert_eq!(config.max_convergence_rounds, 10);
        assert_eq!(config.block_size_bytes, 4_194_304);
        assert_eq!(config.total_timeout_seconds, 0);
    }

    /// Issue #394 Option C: the correlation_id is the only carrier from
    /// the `MigrateVm` RPC's config to the migration state machine — the
    /// pause-first opt-in must survive the trip, and any non-`true` value
    /// (or absence) must mean false, never a parse error.
    #[test]
    fn test_migration_config_pause_first_from_correlation_id() {
        let (.., config) = MigrationConfig::from_correlation_id("source=a:dest=b:pause_first=true");
        assert!(config.pause_first, "pause_first=true must carry");

        let (.., config) =
            MigrationConfig::from_correlation_id("source=a:dest=b:pause_first=false");
        assert!(!config.pause_first, "pause_first=false must stay off");

        let (.., config) = MigrationConfig::from_correlation_id("source=a:dest=b");
        assert!(!config.pause_first, "absence must mean false (the default)");
    }

    #[test]
    fn test_migration_config_from_proto() {
        let proto_config = proto::MigrationConfig {
            dirty_threshold_blocks: 2048,
            max_convergence_rounds: 15,
            block_size_bytes: 8_388_608,
            total_timeout_seconds: 7200,
            pause_first: true,
        };
        let config = MigrationConfig::from_proto(&proto_config);

        assert_eq!(config.dirty_threshold_blocks, 2048);
        assert_eq!(config.max_convergence_rounds, 15);
        assert_eq!(config.block_size_bytes, 8_388_608);
        assert_eq!(config.total_timeout_seconds, 7200);
        assert!(config.pause_first);
    }

    #[test]
    fn test_migration_config_from_proto_zeros_use_defaults() {
        let proto_config = proto::MigrationConfig {
            dirty_threshold_blocks: 0,
            max_convergence_rounds: 0,
            block_size_bytes: 0,
            total_timeout_seconds: 0,
            pause_first: false,
        };
        let config = MigrationConfig::from_proto(&proto_config);
        let defaults = MigrationConfig::default();

        assert_eq!(
            config.dirty_threshold_blocks,
            defaults.dirty_threshold_blocks
        );
        assert_eq!(
            config.max_convergence_rounds,
            defaults.max_convergence_rounds
        );
        assert_eq!(config.block_size_bytes, defaults.block_size_bytes);
        assert_eq!(config.total_timeout_seconds, 0); // 0 is valid (means use calculated)
    }

    #[test]
    fn test_proto_phase_to_str() {
        assert_eq!(
            proto_phase_to_str(proto::MigrationPhase::Pending as i32),
            "Pending"
        );
        assert_eq!(
            proto_phase_to_str(proto::MigrationPhase::PrecopyDisk as i32),
            "PreCopyDisk"
        );
        assert_eq!(
            proto_phase_to_str(proto::MigrationPhase::ConvergingDisk as i32),
            "ConvergingDisk"
        );
        assert_eq!(
            proto_phase_to_str(proto::MigrationPhase::MemoryMigration as i32),
            "MemoryMigration"
        );
        assert_eq!(
            proto_phase_to_str(proto::MigrationPhase::Paused as i32),
            "Paused"
        );
        assert_eq!(
            proto_phase_to_str(proto::MigrationPhase::Completed as i32),
            "Completed"
        );
        assert_eq!(
            proto_phase_to_str(proto::MigrationPhase::Failed as i32),
            "Failed"
        );
        assert_eq!(
            proto_phase_to_str(proto::MigrationPhase::RolledBack as i32),
            "RolledBack"
        );
    }

    async fn seed_migration_for_progress(pool: &StorePool, phase: &str) {
        sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES ('vm-progress', 'vm-progress')")
            .execute(pool)
            .await
            .expect("insert vm");
        sqlx::query(
            r#"INSERT INTO operations
                   (operation_id, idempotency_key, resource_kind, resource_id,
                    operation_type, status, requested_at)
               VALUES ('op-progress', 'idem-progress', 'vm', 'vm-progress',
                       'Migrate', 'Running', strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))"#,
        )
        .execute(pool)
        .await
        .expect("insert operation");
        sqlx::query(
            r#"INSERT INTO migrations
                   (migration_id, operation_id, vm_id, source_node_id,
                    destination_node_id, phase)
               VALUES ('mig-progress', 'op-progress', 'vm-progress',
                       'node-a', 'node-b', ?)"#,
        )
        .bind(phase)
        .execute(pool)
        .await
        .expect("insert migration");
    }

    #[tokio::test]
    async fn progress_from_a_non_participant_node_is_denied_and_not_persisted() {
        let test_db = chv_controlplane_store::test_util::TestDb::new().await;
        seed_migration_for_progress(&test_db.pool, "ConvergingDisk").await;

        let result = update_migration_progress(
            &test_db.pool,
            "node-c",
            "vm-progress",
            "op-progress",
            proto::MigrationPhase::MemoryMigration as i32,
            1,
            2,
            3,
            4,
        )
        .await;

        assert!(matches!(result, Err(ChvError::AccessDenied { .. })));
        let (phase, round): (String, i32) = sqlx::query_as(
            "SELECT phase, convergence_round FROM migrations WHERE migration_id = 'mig-progress'",
        )
        .fetch_one(&test_db.pool)
        .await
        .expect("read migration");
        assert_eq!(phase, "ConvergingDisk");
        assert_eq!(round, 0, "a denied update must not mutate the row");
    }

    #[tokio::test]
    async fn progress_from_either_participant_is_accepted() {
        let test_db = chv_controlplane_store::test_util::TestDb::new().await;
        seed_migration_for_progress(&test_db.pool, "ConvergingDisk").await;

        for node in ["node-a", "node-b"] {
            update_migration_progress(
                &test_db.pool,
                node,
                "vm-progress",
                "op-progress",
                proto::MigrationPhase::ConvergingDisk as i32,
                10,
                20,
                1,
                5,
            )
            .await
            .expect("source and destination may both report progress");
        }

        let round: i32 = sqlx::query_scalar(
            "SELECT convergence_round FROM migrations WHERE migration_id = 'mig-progress'",
        )
        .fetch_one(&test_db.pool)
        .await
        .expect("read migration");
        assert_eq!(round, 1);
    }

    #[tokio::test]
    async fn late_progress_cannot_resurrect_a_terminal_migration() {
        let test_db = chv_controlplane_store::test_util::TestDb::new().await;
        seed_migration_for_progress(&test_db.pool, "Failed").await;

        // A queued non-terminal report arrives after the reaper failed the
        // row: it must be acknowledged (so the agent drops it) but ignored.
        update_migration_progress(
            &test_db.pool,
            "node-a",
            "vm-progress",
            "op-progress",
            proto::MigrationPhase::ConvergingDisk as i32,
            1,
            2,
            9,
            9,
        )
        .await
        .expect("a late report is dropped, not retried forever");

        let (phase, round): (String, i32) = sqlx::query_as(
            "SELECT phase, convergence_round FROM migrations WHERE migration_id = 'mig-progress'",
        )
        .fetch_one(&test_db.pool)
        .await
        .expect("read migration");
        assert_eq!(phase, "Failed");
        assert_eq!(round, 0);
    }
}
