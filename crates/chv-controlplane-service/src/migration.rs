//! Live migration orchestrator state machine.
//!
//! Drives VM migration through phases: PreCopyDisk -> ConvergingDisk -> MemoryMigration -> Paused -> Completed.
//! Handles rollback at each phase according to spec.

use crate::node_client_pool::NodeClientPool;
use chv_common::migration_auth::{
    MigrationTokenCodec, DEFAULT_MIGRATION_TOKEN_TTL, MAX_MIGRATION_TOKEN_TTL,
};
use chv_controlplane_store::{
    InterruptedMigrationOutcome, InterruptedMigrationUpdate, MigrationRecord, MigrationRepository,
    StorePool,
};
use chv_errors::ChvError;
use control_plane_node_api::control_plane_node_api as proto;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
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
            // Transient dispatch data, never part of the persisted config.
            volume_authorizations: Vec::new(),
            allow_insecure_unauthenticated: false,
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

/// How many optimistic retries the startup recovery pass attempts when a
/// progress report moves a row between the scan and the guarded update.
const RESTART_RECOVERY_MAX_ATTEMPTS: usize = 3;

/// One migration that a control-plane restart interrupted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredMigration {
    pub migration_id: String,
    pub operation_id: String,
    pub vm_id: String,
    /// The non-terminal phase observed on disk at startup.
    pub interrupted_phase: String,
    /// The terminal phase written by recovery.
    pub recovered_phase: MigrationPhase,
}

/// Result of the startup reconciliation pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationRecoveryReport {
    /// Non-terminal rows reconciled to a terminal phase by this pass.
    pub reconciled: Vec<RecoveredMigration>,
    /// Rows that reached a terminal phase between the scan and the guarded
    /// update (for example a late progress report that raced startup). They
    /// were left exactly as found.
    pub already_terminal: usize,
    /// Rows that could not be reconciled after the retry budget (or
    /// disappeared mid-pass). A row that still exists remains non-terminal for
    /// the reaper/operator; the pass never guesses a terminal state.
    pub unresolved: usize,
}

impl MigrationRecoveryReport {
    pub fn is_empty(&self) -> bool {
        self.reconciled.is_empty() && self.already_terminal == 0 && self.unresolved == 0
    }
}

/// Durable, restart-scoped view of tracked migrations.
///
/// The `migrations` table — not an in-process map — is the source of truth.
/// A fresh registry is constructed during control-plane startup; its
/// [`Self::recover_after_restart`] pass converts rows left non-terminal by the
/// previous process into an inspectable terminal state. Terminal rows are never
/// touched (a `Completed` migration must never be resurrected).
pub struct MigrationRegistry {
    repository: MigrationRepository,
}

impl MigrationRegistry {
    pub fn new(pool: StorePool) -> Self {
        Self {
            repository: MigrationRepository::new(pool),
        }
    }

    /// Reconcile every migration row left non-terminal by a previous
    /// control-plane process.
    ///
    /// Mapping is deterministic and conservative:
    /// - `Pending`, `PreCopyDisk`, `ConvergingDisk` → `RolledBack`: the source
    ///   VM is *presumed* authoritative, so abandoning the migration there is
    ///   the least destructive choice. This is an inference, not proof: the
    ///   control-plane task did not survive, but the agent-side task may still
    ///   be running.
    /// - `MemoryMigration`, `Paused`, and any unrecognized non-terminal phase →
    ///   `Failed`: VM state may already be on the destination, so even a
    ///   presumed rollback would be unsafe.
    ///
    /// No recovered row gets an automatic source-VM resume, in either mapping.
    /// Once the row is terminal the progress path drops late reports, so the
    /// reaper's "sender has been silent" evidence would be self-fulfilling
    /// rather than proof that the agent-side task stopped. Operators inspect
    /// the row and resume placement explicitly. Under the pause-first gate the
    /// source is normally paused even in the pre-memory phases, so "confirm the
    /// VM is running on the source" must include an explicit resume step.
    ///
    /// The original phase is preserved in `interrupted_from_phase` and the
    /// reason in `error_message`; the parent operation is failed with
    /// `MIGRATION_INTERRUPTED`. Reconciliation is idempotent: a second pass
    /// finds only terminal rows and changes nothing.
    pub async fn recover_after_restart(&self) -> Result<MigrationRecoveryReport, ChvError> {
        let mut report = MigrationRecoveryReport::default();
        let rows = self
            .repository
            .list_non_terminal()
            .await
            .map_err(|e| ChvError::Internal {
                reason: format!("failed to scan migrations for restart recovery: {e}"),
            })?;

        for row in rows {
            match self.reconcile_row(row).await? {
                ReconcileRowOutcome::Reconciled(recovered) => {
                    metrics::counter!(
                        chv_observability::CHV_MIGRATION_RESTART_RECONCILED_TOTAL,
                        "recovered_phase" => recovered.recovered_phase.as_str()
                    )
                    .increment(1);
                    warn!(
                        migration_id = %recovered.migration_id,
                        operation_id = %recovered.operation_id,
                        vm_id = %recovered.vm_id,
                        interrupted_phase = %recovered.interrupted_phase,
                        recovered_phase = %recovered.recovered_phase.as_str(),
                        "migration interrupted by control-plane restart; reconciled"
                    );
                    report.reconciled.push(recovered);
                }
                ReconcileRowOutcome::AlreadyTerminal => report.already_terminal += 1,
                ReconcileRowOutcome::Unresolved => report.unresolved += 1,
            }
        }

        Ok(report)
    }

    async fn reconcile_row(
        &self,
        mut row: MigrationRecord,
    ) -> Result<ReconcileRowOutcome, ChvError> {
        for _ in 0..RESTART_RECOVERY_MAX_ATTEMPTS {
            let recovered_phase = recovery_phase(&row.phase);
            let error_message = migration_interrupted_reason(&row.phase, recovered_phase);
            let update = InterruptedMigrationUpdate {
                migration_id: row.migration_id.clone(),
                operation_id: row.operation_id.clone(),
                observed_phase: row.phase.clone(),
                observed_updated_at: row.updated_at.clone(),
                terminal_phase: recovered_phase.as_str().to_string(),
                error_message,
            };

            match self.repository.reconcile_interrupted(&update).await {
                Ok(InterruptedMigrationOutcome::Reconciled) => {
                    return Ok(ReconcileRowOutcome::Reconciled(RecoveredMigration {
                        migration_id: row.migration_id,
                        operation_id: row.operation_id,
                        vm_id: row.vm_id,
                        interrupted_phase: row.phase,
                        recovered_phase,
                    }));
                }
                Ok(InterruptedMigrationOutcome::AlreadyTerminal) => {
                    return Ok(ReconcileRowOutcome::AlreadyTerminal);
                }
                Ok(InterruptedMigrationOutcome::Missing) => {
                    warn!(
                        migration_id = %row.migration_id,
                        "migration disappeared during restart recovery; leaving it to the reaper"
                    );
                    return Ok(ReconcileRowOutcome::Unresolved);
                }
                Ok(InterruptedMigrationOutcome::PhaseChanged) => {
                    // A progress report (or another phase transition) landed
                    // while we were looking. Re-read and re-plan against the
                    // fresh state rather than overwriting it.
                    match self.repository.get(&row.migration_id).await.map_err(|e| {
                        ChvError::Internal {
                            reason: format!(
                                "failed to re-read migration {} during restart recovery: {e}",
                                row.migration_id
                            ),
                        }
                    })? {
                        Some(fresh) if !chv_controlplane_store::is_terminal_phase(&fresh.phase) => {
                            row = fresh;
                        }
                        Some(_) => return Ok(ReconcileRowOutcome::AlreadyTerminal),
                        None => return Ok(ReconcileRowOutcome::Unresolved),
                    }
                }
                Err(e) => {
                    return Err(ChvError::Internal {
                        reason: format!(
                            "failed to reconcile interrupted migration {}: {e}",
                            row.migration_id
                        ),
                    });
                }
            }
        }

        let marked = self
            .repository
            .record_restart_observed(&row.migration_id, &row.phase, &row.updated_at)
            .await
            .map_err(|e| ChvError::Internal {
                reason: format!(
                    "failed to record restart observation for migration {}: {e}",
                    row.migration_id
                ),
            })?;
        warn!(
            migration_id = %row.migration_id,
            phase = %row.phase,
            marked,
            "migration kept changing during restart recovery; leaving it non-terminal for the reaper"
        );
        Ok(ReconcileRowOutcome::Unresolved)
    }
}

enum ReconcileRowOutcome {
    Reconciled(RecoveredMigration),
    AlreadyTerminal,
    Unresolved,
}

/// Deterministic terminal mapping for a non-terminal phase observed at
/// startup. No mapping authorizes an automatic source-VM resume.
fn recovery_phase(phase: &str) -> MigrationPhase {
    match phase {
        "Pending" | "PreCopyDisk" | "ConvergingDisk" => MigrationPhase::RolledBack,
        // Memory transfer, final sync, and unknown/future phases are all
        // ambiguous: VM state may already live on the destination. Fail closed
        // and require an operator to inspect placement.
        _ => MigrationPhase::Failed,
    }
}

fn migration_interrupted_reason(observed_phase: &str, recovered_phase: MigrationPhase) -> String {
    match recovered_phase {
        MigrationPhase::RolledBack => format!(
            "control-plane restarted while migration was in phase {observed_phase}; \
             no control-plane migration task survived, but the agent-side task may still \
             be running. Recovery marked it RolledBack because the source VM is presumed \
             authoritative. No automatic resume is queued: inspect both nodes, then resume \
             the source explicitly if needed (it is normally paused under pause-first). \
             The migration is not retried automatically."
        ),
        _ => format!(
            "control-plane restarted while migration was in phase {observed_phase}; \
             no control-plane migration task survived, but the agent-side task may still \
             be running. VM state may already be on the destination, so recovery marked it \
             Failed rather than guess a rollback. Inspect VM placement on both nodes before \
             resuming or retrying; no automatic action is taken."
        ),
    }
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

/// Mint one short-lived migration authorization per attached volume
/// (issue #663).
///
/// The control plane is the only minter. The tokens are transient dispatch
/// data: they are never written to the operations row, migrations table, or
/// correlation_id. A missing signing secret fails closed before the agent is
/// contacted unless the deployment explicitly opted into the development-only
/// insecure mode on both sides.
async fn build_migration_volume_authorizations(
    pool: &StorePool,
    state: &MigrationState,
    signing_secret: Option<&[u8]>,
    token_ttl: Duration,
    allow_insecure_unauthenticated: bool,
) -> Result<Vec<proto::MigrationVolumeAuthorization>, ChvError> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        r#"SELECT v.volume_id, v.capacity_bytes
           FROM volume_desired_state vds
           JOIN volumes v ON v.volume_id = vds.volume_id
           WHERE vds.attached_vm_id = ?
           ORDER BY v.volume_id"#,
    )
    .bind(&state.vm_id)
    .fetch_all(pool)
    .await
    .map_err(|e| ChvError::Internal {
        reason: format!("failed to query attached volumes for migration authorization: {e}"),
    })?;

    if rows.is_empty() {
        // A diskless VM has no destination volume creation to authorize.
        return Ok(Vec::new());
    }

    let Some(secret) = signing_secret else {
        if allow_insecure_unauthenticated {
            warn!(
                migration_id = %state.migration_id,
                vm_id = %state.vm_id,
                "SECURITY: dispatching a disk migration WITHOUT control-plane authorizations \
                 (migration.allow_insecure_unauthenticated = true, development-only)"
            );
            return Ok(Vec::new());
        }
        return Err(ChvError::Unauthorized {
            reason: "disk migration requires a dedicated migration signing secret, but none is \
                     configured on the control plane; set [migration].signing_secret_file (or \
                     CHV_MIGRATION_SIGNING_SECRET). Refusing to dispatch an unauthorized \
                     migration"
                .to_string(),
        });
    };

    let codec = MigrationTokenCodec::new(secret).map_err(|e| ChvError::Unauthorized {
        reason: format!("invalid migration signing secret: {e}"),
    })?;
    let ttl = if token_ttl.is_zero() {
        DEFAULT_MIGRATION_TOKEN_TTL
    } else {
        token_ttl.min(MAX_MIGRATION_TOKEN_TTL)
    };

    let mut authorizations = Vec::with_capacity(rows.len());
    for (volume_id, capacity_bytes) in rows {
        let size_bytes = u64::try_from(capacity_bytes).map_err(|_| ChvError::Internal {
            reason: format!(
                "volume {volume_id} has a negative capacity ({capacity_bytes}); cannot authorize \
                 its migration"
            ),
        })?;
        let token = codec
            .mint(
                &state.source_node_id,
                &state.dest_node_id,
                &volume_id,
                size_bytes,
                ttl,
            )
            .map_err(|e| ChvError::Internal {
                reason: format!(
                    "failed to mint migration authorization for volume {volume_id}: {e}"
                ),
            })?;
        authorizations.push(proto::MigrationVolumeAuthorization { volume_id, token });
    }
    Ok(authorizations)
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
    migration_signing_secret: Option<&[u8]>,
    migration_token_ttl: Duration,
    allow_insecure_unauthenticated: bool,
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

    // Issue #663: mint per-volume authorizations BEFORE contacting the
    // source agent. If the secret is missing/misconfigured this fails here,
    // so no unauthorized migration can start and no destination volume can
    // be provisioned.
    let volume_authorizations = build_migration_volume_authorizations(
        pool,
        state,
        migration_signing_secret,
        migration_token_ttl,
        allow_insecure_unauthenticated,
    )
    .await?;

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

        // Tokens ride the transient MigrateVm request only. They are never
        // written to the operation row/correlation_id.
        let mut dispatch_config = state.config.to_proto();
        dispatch_config.volume_authorizations = volume_authorizations;
        // The tokenless marker is explicit and rides only the transient
        // dispatch config. It is true only when this CP opted into the
        // development bypass AND no tokens were minted (no secret); the
        // source agent still requires its own opt-in gate.
        dispatch_config.allow_insecure_unauthenticated =
            allow_insecure_unauthenticated && dispatch_config.volume_authorizations.is_empty();

        let precopy_result = tokio::time::timeout(
            Duration::from_secs(timeouts.precopy_disk_secs),
            source_client.migrate_vm(
                &state.source_node_id,
                &state.vm_id,
                &vm_generation,
                &state.source_node_id,
                &state.dest_node_id,
                dispatch_config,
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

/// How often a dropped terminal progress report is logged for the same
/// migration. The counter is never rate-limited; only the log line is.
const DROPPED_PROGRESS_WARN_INTERVAL: Duration = Duration::from_secs(60);

/// Last time a dropped-progress warning was emitted per migration. A late
/// `Completed` report bypasses the limiter because it is the highest-signal
/// case: the destination may be claiming success after recovery already
/// declared the migration terminal.
static DROPPED_PROGRESS_WARN_AT: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();

fn should_warn_dropped_progress(migration_id: &str, reported_phase: &str) -> bool {
    if reported_phase == "Completed" {
        return true;
    }

    let now = Instant::now();
    let mut last_warned = DROPPED_PROGRESS_WARN_AT
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Bound the map: entries older than ten intervals can no longer suppress
    // a warning, so dropping them is lossless.
    last_warned.retain(|_, at| now.duration_since(*at) < DROPPED_PROGRESS_WARN_INTERVAL * 10);
    match last_warned.get(migration_id) {
        Some(at) if now.duration_since(*at) < DROPPED_PROGRESS_WARN_INTERVAL => false,
        _ => {
            last_warned.insert(migration_id.to_string(), now);
            true
        }
    }
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
    let row: Option<(String, String, String, String)> = sqlx::query_as(
        r#"SELECT migration_id, source_node_id, destination_node_id, phase
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

    let Some((migration_id, source_node_id, destination_node_id, current_phase)) = row else {
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
        // Terminal: the report arrived too late (reaper or restart recovery
        // won). Dropping it is correct; returning an error would make the agent
        // retry forever. Keep it observable, though: a still-running agent task
        // is exactly the case restart recovery cannot prove is gone, and a late
        // `Completed` after recovery is the highest-signal variant.
        metrics::counter!(
            chv_observability::CHV_MIGRATION_PROGRESS_DROPPED_TOTAL,
            "stored_phase" => current_phase.clone(),
            "reported_phase" => phase_str,
        )
        .increment(1);
        if should_warn_dropped_progress(&migration_id, phase_str) {
            warn!(
                migration_id = %migration_id,
                operation_id = %operation_id,
                vm_id = %vm_id,
                node_id = %node_id,
                reported_phase = %phase_str,
                stored_phase = %current_phase,
                "dropped migration progress report for a terminal migration; \
                 an agent-side task may still be running"
            );
        }
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
    // One definition for the agent runtime, legacy reconcile, nwd teardown,
    // migration, and dnsmasq: truncate+hash ids that exceed IFNAMSIZ.
    chv_common::bridge_name_for_network(net_id)
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
            volume_authorizations: Vec::new(),
            allow_insecure_unauthenticated: false,
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
            volume_authorizations: Vec::new(),
            allow_insecure_unauthenticated: false,
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

    /// Seed one migration row (plus its VM and parent operation) for the
    /// restart-recovery tests. Returns `(migration_id, operation_id)`.
    async fn seed_migration_row(
        pool: &StorePool,
        suffix: &str,
        phase: &str,
        updated_offset_secs: i64,
    ) -> (String, String) {
        let migration_id = format!("mig-{suffix}");
        let operation_id = format!("op-{suffix}");
        let vm_id = format!("vm-{suffix}");

        sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES (?, ?)")
            .bind(&vm_id)
            .bind(&vm_id)
            .execute(pool)
            .await
            .expect("insert vm");

        sqlx::query(
            r#"INSERT INTO operations
                   (operation_id, idempotency_key, resource_kind, resource_id,
                    operation_type, status, requested_at)
               VALUES (?, ?, 'vm', ?, 'Migrate', 'Running',
                       strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))"#,
        )
        .bind(&operation_id)
        .bind(format!("idem-{suffix}"))
        .bind(&vm_id)
        .execute(pool)
        .await
        .expect("insert operation");

        sqlx::query(
            r#"INSERT INTO migrations
                   (migration_id, operation_id, vm_id, source_node_id,
                    destination_node_id, phase, started_at, updated_at)
               VALUES (?, ?, ?, 'node-a', 'node-b', ?,
                       strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-60 seconds'),
                       strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ?))"#,
        )
        .bind(&migration_id)
        .bind(&operation_id)
        .bind(&vm_id)
        .bind(phase)
        .bind(format!("{updated_offset_secs} seconds"))
        .execute(pool)
        .await
        .expect("insert migration");

        (migration_id, operation_id)
    }

    #[derive(sqlx::FromRow)]
    struct RecoveredRow {
        phase: String,
        interrupted_from_phase: Option<String>,
        interrupted_at: Option<String>,
        completed_at: Option<String>,
        error_message: Option<String>,
        reaper_resume_state: Option<String>,
    }

    async fn recovered_row(pool: &StorePool, migration_id: &str) -> RecoveredRow {
        sqlx::query_as(
            r#"SELECT phase, interrupted_from_phase, interrupted_at, completed_at,
                      error_message, reaper_resume_state
               FROM migrations WHERE migration_id = ?"#,
        )
        .bind(migration_id)
        .fetch_one(pool)
        .await
        .expect("read recovered migration")
    }

    /// Acceptance: a restart mid-migration leaves a recoverable, inspectable
    /// terminal state, and a fresh registry against the same database observes
    /// it deterministically instead of guessing `Completed`.
    #[tokio::test]
    async fn restart_recovery_reconciles_in_flight_migrations_deterministically() {
        let test_db = chv_controlplane_store::test_util::TestDb::new().await;

        let cases = [
            ("pending", "Pending", "RolledBack"),
            ("precopy", "PreCopyDisk", "RolledBack"),
            ("converging", "ConvergingDisk", "RolledBack"),
            // Ambiguous: VM state may already be on the destination. Fail
            // closed and leave the resume decision to an operator.
            ("memory", "MemoryMigration", "Failed"),
            ("paused", "Paused", "Failed"),
        ];
        for (suffix, phase, _) in cases {
            seed_migration_row(&test_db.pool, suffix, phase, -10).await;
        }

        let registry = MigrationRegistry::new(test_db.pool.clone());
        let report = registry
            .recover_after_restart()
            .await
            .expect("restart recovery should succeed");
        assert_eq!(
            report.reconciled.len(),
            cases.len(),
            "every in-flight row must be reconciled"
        );
        assert_eq!(report.already_terminal, 0);
        assert_eq!(report.unresolved, 0);

        // Simulate a second control-plane start: drop the old registry and
        // construct a fresh one against the same database.
        drop(registry);
        let restarted = MigrationRegistry::new(test_db.pool.clone());
        let second = restarted
            .recover_after_restart()
            .await
            .expect("second recovery should succeed");
        assert!(second.is_empty(), "recovery must be idempotent: {second:?}");

        for (suffix, interrupted_phase, recovered_phase) in cases {
            let migration_id = format!("mig-{suffix}");
            let operation_id = format!("op-{suffix}");
            let row = recovered_row(&test_db.pool, &migration_id).await;

            assert_eq!(row.phase, recovered_phase, "unexpected phase for {suffix}");
            assert_ne!(
                row.phase, "Completed",
                "an interrupted migration must never be marked Completed"
            );
            assert_eq!(
                row.interrupted_from_phase.as_deref(),
                Some(interrupted_phase),
                "the interrupted phase must stay inspectable"
            );
            assert!(row.interrupted_at.is_some(), "interruption time missing");
            assert!(row.completed_at.is_some(), "terminal time missing");
            let message = row.error_message.expect("interruption reason");
            assert!(
                message.contains("control-plane restarted") && message.contains(interrupted_phase),
                "reason must identify the restart and original phase: {message}"
            );

            assert_eq!(
                row.reaper_resume_state, None,
                "restart recovery must not queue an automatic source resume for {suffix}"
            );

            let (status, error_code): (String, Option<String>) =
                sqlx::query_as("SELECT status, error_code FROM operations WHERE operation_id = ?")
                    .bind(&operation_id)
                    .fetch_one(&test_db.pool)
                    .await
                    .expect("read operation");
            assert_eq!(
                status, "Failed",
                "operation {operation_id} must not stay Running"
            );
            assert_eq!(error_code.as_deref(), Some("MIGRATION_INTERRUPTED"));
        }
    }

    /// Terminal rows are evidence, not work: a restart must not rewrite their
    /// phase, reason, completion time, or resume bookkeeping.
    #[tokio::test]
    async fn restart_recovery_never_touches_or_resurrects_terminal_rows() {
        let test_db = chv_controlplane_store::test_util::TestDb::new().await;

        for (suffix, phase) in [
            ("completed", "Completed"),
            ("failed", "Failed"),
            ("rolledback", "RolledBack"),
        ] {
            let (migration_id, _) = seed_migration_row(&test_db.pool, suffix, phase, -600).await;
            sqlx::query(
                r#"UPDATE migrations
                   SET completed_at = '2026-01-01T00:00:00Z',
                       error_message = ?,
                       reaper_resume_state = 'resumed'
                   WHERE migration_id = ?"#,
            )
            .bind(format!("preserved-{suffix}"))
            .bind(&migration_id)
            .execute(&test_db.pool)
            .await
            .expect("annotate terminal row");
        }
        seed_migration_row(&test_db.pool, "live", "ConvergingDisk", -10).await;

        let report = MigrationRegistry::new(test_db.pool.clone())
            .recover_after_restart()
            .await
            .expect("restart recovery should succeed");
        assert_eq!(report.reconciled.len(), 1);
        assert_eq!(report.reconciled[0].migration_id, "mig-live");

        for (suffix, expected_phase) in [
            ("completed", "Completed"),
            ("failed", "Failed"),
            ("rolledback", "RolledBack"),
        ] {
            let migration_id = format!("mig-{suffix}");
            let row = recovered_row(&test_db.pool, &migration_id).await;
            assert_eq!(row.phase, expected_phase);
            assert_eq!(row.interrupted_from_phase, None);
            assert_eq!(row.interrupted_at, None);
            assert_eq!(row.completed_at.as_deref(), Some("2026-01-01T00:00:00Z"));
            let expected_message = format!("preserved-{suffix}");
            assert_eq!(
                row.error_message.as_deref(),
                Some(expected_message.as_str())
            );
            assert_eq!(row.reaper_resume_state.as_deref(), Some("resumed"));
        }
    }

    /// After recovery the row is terminal, so the existing RPC paths must
    /// behave: cancel is a no-op and a late progress report is acknowledged
    /// without resurrecting the migration.
    #[tokio::test]
    async fn cancel_and_progress_after_restart_cannot_resurrect_interrupted_migration() {
        let test_db = chv_controlplane_store::test_util::TestDb::new().await;
        seed_migration_row(&test_db.pool, "cancel", "ConvergingDisk", -10).await;

        MigrationRegistry::new(test_db.pool.clone())
            .recover_after_restart()
            .await
            .expect("restart recovery should succeed");

        assert_eq!(
            request_migration_cancel(&test_db.pool, "mig-cancel")
                .await
                .expect("cancel lookup"),
            CancelMigrationOutcome::AlreadyTerminal,
            "cancelling an interrupted (already terminal) migration is a no-op"
        );

        update_migration_progress(
            &test_db.pool,
            "node-a",
            "vm-cancel",
            "op-cancel",
            proto::MigrationPhase::MemoryMigration as i32,
            100,
            200,
            4,
            7,
        )
        .await
        .expect("late progress is acknowledged and dropped");

        let (phase, round, bytes): (String, i32, i64) = sqlx::query_as(
            "SELECT phase, convergence_round, bytes_transferred FROM migrations \
             WHERE migration_id = 'mig-cancel'",
        )
        .fetch_one(&test_db.pool)
        .await
        .expect("read migration");
        assert_eq!(phase, "RolledBack");
        assert_eq!(round, 0, "late progress must not mutate a terminal row");
        assert_eq!(bytes, 0, "late progress must not mutate a terminal row");
    }

    /// Unknown or future non-terminal phases fail closed: `Failed`, original
    /// phase preserved, no automatic source resume.
    #[tokio::test]
    async fn restart_recovery_fails_closed_on_unknown_non_terminal_phase() {
        let test_db = chv_controlplane_store::test_util::TestDb::new().await;
        seed_migration_row(&test_db.pool, "future", "SomeFuturePhase", -10).await;

        let report = MigrationRegistry::new(test_db.pool.clone())
            .recover_after_restart()
            .await
            .expect("restart recovery should succeed");
        assert_eq!(report.reconciled.len(), 1);

        let row = recovered_row(&test_db.pool, "mig-future").await;
        assert_eq!(row.phase, "Failed");
        assert_eq!(
            row.interrupted_from_phase.as_deref(),
            Some("SomeFuturePhase")
        );
        assert_eq!(
            row.reaper_resume_state, None,
            "an unrecognized phase must not trigger an automatic source resume"
        );
    }

    /// A progress writer that keeps moving the row makes every guarded update
    /// lose. Recovery must exhaust its retry budget, re-read and re-plan each
    /// time, then leave the row non-terminal, record only the observation
    /// marker, and report it unresolved.
    #[tokio::test]
    async fn restart_recovery_exhausts_retry_budget_and_marks_row_unresolved() {
        let test_db = chv_controlplane_store::test_util::TestDb::new().await;
        seed_migration_row(&test_db.pool, "racing", "ConvergingDisk", -10).await;

        // Simulate a competing progress writer: every attempt to terminalize
        // the row advances its phase and is ignored, so the guarded UPDATE
        // affects zero rows and recovery must re-read/re-plan.
        sqlx::query("CREATE TABLE reconcile_attempts (attempt INTEGER)")
            .execute(&test_db.pool)
            .await
            .expect("create attempt log");
        sqlx::query(
            r#"CREATE TRIGGER simulate_progress_race
               BEFORE UPDATE OF phase ON migrations
               WHEN OLD.migration_id = 'mig-racing'
               BEGIN
                   INSERT INTO reconcile_attempts(attempt) VALUES (NULL);
                   UPDATE migrations
                      SET phase = CASE OLD.phase
                          WHEN 'ConvergingDisk' THEN 'MemoryMigration'
                          WHEN 'MemoryMigration' THEN 'Paused'
                          ELSE 'ConvergingDisk'
                      END
                    WHERE migration_id = OLD.migration_id;
                   SELECT RAISE(IGNORE);
               END"#,
        )
        .execute(&test_db.pool)
        .await
        .expect("create race trigger");

        let repo = chv_controlplane_store::MigrationRepository::new(test_db.pool.clone());
        let row = repo.get("mig-racing").await.unwrap().expect("seeded row");
        let outcome = MigrationRegistry::new(test_db.pool.clone())
            .reconcile_row(row)
            .await
            .expect("racing recovery should complete");

        assert!(
            matches!(outcome, ReconcileRowOutcome::Unresolved),
            "a row that keeps changing must be reported unresolved"
        );
        let attempts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM reconcile_attempts")
            .fetch_one(&test_db.pool)
            .await
            .expect("read attempt count");
        assert_eq!(
            attempts, RESTART_RECOVERY_MAX_ATTEMPTS as i64,
            "recovery must use its full retry budget"
        );

        let row = repo.get("mig-racing").await.unwrap().expect("row remains");
        assert!(
            !chv_controlplane_store::is_terminal_phase(&row.phase),
            "recovery must never write a terminal phase while racing a writer: {}",
            row.phase
        );
        assert_eq!(row.phase, "ConvergingDisk");
        assert_eq!(
            row.interrupted_from_phase.as_deref(),
            Some("ConvergingDisk")
        );
        assert!(row.interrupted_at.is_some(), "observation marker missing");
        assert_eq!(row.completed_at, None);
        assert_eq!(row.error_message, None);
        assert_eq!(row.reaper_resume_state, None);

        let operation_status: String =
            sqlx::query_scalar("SELECT status FROM operations WHERE operation_id = 'op-racing'")
                .fetch_one(&test_db.pool)
                .await
                .expect("read operation");
        assert_eq!(
            operation_status, "Running",
            "the parent operation must not be failed when the row was never terminalized"
        );
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

    /// Seed a VM plus attached volumes (`volumes` + `volume_desired_state`).
    async fn seed_attached_volumes(pool: &StorePool, vm_id: &str, volumes: &[(&str, i64)]) {
        sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES (?, ?)")
            .bind(vm_id)
            .bind(vm_id)
            .execute(pool)
            .await
            .expect("insert vm");
        for (volume_id, capacity_bytes) in volumes {
            sqlx::query(
                "INSERT INTO volumes (volume_id, display_name, capacity_bytes) VALUES (?, ?, ?)",
            )
            .bind(volume_id)
            .bind(volume_id)
            .bind(capacity_bytes)
            .execute(pool)
            .await
            .expect("insert volume");
            sqlx::query(
                "INSERT INTO volume_desired_state \
                 (volume_id, desired_generation, attached_vm_id) VALUES (?, 1, ?)",
            )
            .bind(volume_id)
            .bind(vm_id)
            .execute(pool)
            .await
            .expect("insert volume desired state");
        }
    }

    fn migration_state_for(vm_id: &str) -> MigrationState {
        MigrationState {
            migration_id: "mig-authz".to_string(),
            operation_id: "op-authz".to_string(),
            vm_id: vm_id.to_string(),
            source_node_id: "node-a".to_string(),
            dest_node_id: "node-b".to_string(),
            phase: MigrationPhase::Pending,
            config: MigrationConfig::default(),
            bytes_transferred: 0,
            total_bytes: 0,
            convergence_round: 0,
            dirty_blocks_remaining: 0,
        }
    }

    const TEST_MIGRATION_SECRET: &[u8] = b"test-migration-signing-secret-0123456789abcdef";

    #[tokio::test]
    async fn migration_authorizations_fail_closed_when_volumes_exist_and_no_secret() {
        let test_db = chv_controlplane_store::test_util::TestDb::new().await;
        seed_attached_volumes(&test_db.pool, "vm-authz", &[("vol-1", 4096)]).await;
        let state = migration_state_for("vm-authz");

        let result = build_migration_volume_authorizations(
            &test_db.pool,
            &state,
            None,
            DEFAULT_MIGRATION_TOKEN_TTL,
            false,
        )
        .await;

        assert!(matches!(result, Err(ChvError::Unauthorized { .. })));
        let message = result.unwrap_err().to_string();
        assert!(message.contains("signing secret"), "{message}");
    }

    #[tokio::test]
    async fn migration_authorizations_diskless_vm_is_empty_even_without_secret() {
        let test_db = chv_controlplane_store::test_util::TestDb::new().await;
        sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES ('vm-diskless', 'vm-diskless')")
            .execute(&test_db.pool)
            .await
            .expect("insert vm");
        let state = migration_state_for("vm-diskless");

        let result = build_migration_volume_authorizations(
            &test_db.pool,
            &state,
            None,
            DEFAULT_MIGRATION_TOKEN_TTL,
            false,
        )
        .await
        .expect("no destination volume means nothing to authorize");
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn migration_authorizations_insecure_mode_returns_empty_only_when_explicitly_allowed() {
        let test_db = chv_controlplane_store::test_util::TestDb::new().await;
        seed_attached_volumes(&test_db.pool, "vm-insecure", &[("vol-1", 4096)]).await;
        let state = migration_state_for("vm-insecure");

        let result = build_migration_volume_authorizations(
            &test_db.pool,
            &state,
            None,
            DEFAULT_MIGRATION_TOKEN_TTL,
            true,
        )
        .await
        .expect("explicit insecure opt-in returns no tokens");
        assert!(result.is_empty());
    }

    async fn minted_tokens_for(
        state: &MigrationState,
        ttl: Duration,
    ) -> Vec<proto::MigrationVolumeAuthorization> {
        let test_db = chv_controlplane_store::test_util::TestDb::new().await;
        seed_attached_volumes(
            &test_db.pool,
            &state.vm_id,
            &[("vol-a", 4096), ("vol-b", 8192)],
        )
        .await;
        build_migration_volume_authorizations(
            &test_db.pool,
            state,
            Some(TEST_MIGRATION_SECRET),
            ttl,
            false,
        )
        .await
        .expect("secret is configured")
    }

    #[tokio::test]
    async fn migration_authorizations_bind_each_volume_and_normalize_zero_ttl() {
        let state = migration_state_for("vm-tokens");
        let tokens = minted_tokens_for(&state, Duration::ZERO).await;
        assert_eq!(tokens.len(), 2, "one token per attached volume");
        assert_eq!(tokens[0].volume_id, "vol-a");
        assert_eq!(tokens[1].volume_id, "vol-b");

        let first = MigrationTokenCodec::decode_claims_unverified(&tokens[0].token).unwrap();
        let second = MigrationTokenCodec::decode_claims_unverified(&tokens[1].token).unwrap();
        assert_eq!(first.source_node, state.source_node_id);
        assert_eq!(first.destination_node, state.dest_node_id);
        assert_eq!(first.volume_id, "vol-a");
        assert_eq!(first.size_bytes, 4096);
        assert_eq!(second.volume_id, "vol-b");
        assert_eq!(second.size_bytes, 8192);
        assert_ne!(
            first.nonce, second.nonce,
            "each volume gets an independent single-use nonce"
        );
        assert_eq!(
            first.expires_at_unix_ms - first.issued_at_unix_ms,
            DEFAULT_MIGRATION_TOKEN_TTL.as_millis() as i64,
            "token_ttl_seconds = 0 must normalize to the 300 s default"
        );
    }

    #[tokio::test]
    async fn migration_authorizations_clamp_overlong_ttl_to_900_seconds() {
        let state = migration_state_for("vm-long-ttl");
        let tokens = minted_tokens_for(&state, Duration::from_secs(86_400)).await;
        let claims = MigrationTokenCodec::decode_claims_unverified(&tokens[0].token).unwrap();
        assert_eq!(
            claims.expires_at_unix_ms - claims.issued_at_unix_ms,
            MAX_MIGRATION_TOKEN_TTL.as_millis() as i64
        );
    }

    #[tokio::test]
    async fn migration_authorizations_reject_negative_capacity() {
        let test_db = chv_controlplane_store::test_util::TestDb::new().await;
        seed_attached_volumes(&test_db.pool, "vm-negative", &[("vol-negative", -1)]).await;
        let state = migration_state_for("vm-negative");

        let result = build_migration_volume_authorizations(
            &test_db.pool,
            &state,
            Some(TEST_MIGRATION_SECRET),
            DEFAULT_MIGRATION_TOKEN_TTL,
            false,
        )
        .await;

        assert!(matches!(result, Err(ChvError::Internal { .. })));
        let message = result.unwrap_err().to_string();
        assert!(message.contains("negative capacity"), "{message}");
        assert!(message.contains("vol-negative"), "{message}");
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
