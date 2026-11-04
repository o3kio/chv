use crate::convergence_metrics::SharedConvergenceMetrics;
use crate::fabric_planner::FabricPlanner;
use crate::migration::resolve_agent_socket;
use crate::node_client_pool::NodeClientPool;
use crate::overlay::OverlayManager;
use chv_controlplane_store::{
    HypervisorSettingsRepository, HypervisorSettingsRow, OperationCreateInput, OperationRepository,
    OperationStatusUpdateInput, StorePool,
};
use chv_controlplane_types::domain::{
    Generation, OperationId, OperationStatus, ResourceId, ResourceKind,
};
use chv_errors::ChvError;
use chv_observability::{CHV_NODES_READY, CHV_OPERATION_DURATION_SECONDS, CHV_VMS_TOTAL};
use std::time::Duration;
use tracing::{error, info, warn};

use chv_common::hypervisor::HypervisorOverrides;

const MAX_DISPATCH_RETRIES: i32 = 3;

/// Terminal error code written when a dispatch fails because the agent
/// answered gRPC `UNIMPLEMENTED` for the dispatched RPC (#378 §7
/// fast-fail piece). Distinct from `AGENT_REJECTED` (the agent examined
/// and refused the request) and `DISPATCH_FAILED` (retries exhausted):
/// this code names a peer/method mismatch — the agent does not
/// implement the RPC (e.g. a legacy-only surface behind the Core
/// authority), so a verbatim retry can never succeed.
const UNSUPPORTED_BY_AGENT_ERROR_CODE: &str = "UNSUPPORTED_BY_AGENT";

/// Background task that polls for accepted operations and dispatches them to node agents.
pub struct Orchestrator {
    pool: StorePool,
    operation_repo: OperationRepository,
    agent_socket_pattern: String,
    kernel_path: String,
    firmware_path: String,
    tick_interval: Duration,
    node_client_pool: NodeClientPool,
    overlay_manager: Option<OverlayManager>,
    fabric_planner: FabricPlanner,
    convergence_metrics: SharedConvergenceMetrics,
    /// Dedicated secret for minting short-lived migration authorizations
    /// (issue #663), plus its token lifetime. `None` means disk migrations
    /// fail closed at dispatch unless the explicit development-only
    /// insecure flag is set.
    migration_signing_secret: Option<Vec<u8>>,
    migration_token_ttl: Duration,
    allow_insecure_migration_authorization: bool,
}

impl Orchestrator {
    pub fn new(
        pool: StorePool,
        operation_repo: OperationRepository,
        agent_socket_pattern: String,
        kernel_path: String,
        firmware_path: String,
        node_client_pool: NodeClientPool,
        convergence_metrics: SharedConvergenceMetrics,
    ) -> Self {
        let fabric_planner = FabricPlanner::new(pool.clone());
        Self {
            pool,
            operation_repo,
            agent_socket_pattern,
            kernel_path,
            firmware_path,
            tick_interval: Duration::from_secs(2),
            node_client_pool,
            overlay_manager: None,
            fabric_planner,
            convergence_metrics,
            migration_signing_secret: None,
            migration_token_ttl: chv_common::migration_auth::DEFAULT_MIGRATION_TOKEN_TTL,
            allow_insecure_migration_authorization: false,
        }
    }

    /// Configure control-plane migration authorization minting (issue #663).
    /// The secret should be resolved once at startup from the dedicated
    /// `[migration]` config section.
    pub fn with_migration_authorization(
        mut self,
        signing_secret: Option<Vec<u8>>,
        token_ttl: Duration,
        allow_insecure_unauthenticated: bool,
    ) -> Self {
        self.migration_signing_secret = signing_secret;
        self.migration_token_ttl = token_ttl;
        self.allow_insecure_migration_authorization = allow_insecure_unauthenticated;
        self
    }

    /// Set the overlay manager for post-migration FDB updates and gratuitous ARP.
    pub fn with_overlay_manager(mut self, overlay_manager: OverlayManager) -> Self {
        self.overlay_manager = Some(overlay_manager);
        self
    }

    pub async fn run(self, mut shutdown_rx: tokio::sync::watch::Receiver<()>) {
        info!("orchestrator starting");
        let mut interval = tokio::time::interval(self.tick_interval);
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = shutdown_rx.changed() => {
                    info!("orchestrator shutting down");
                    break;
                }
            }
            if let Err(e) = self.tick().await {
                warn!(error = %e, "orchestrator tick failed");
            }
        }
    }

    pub(crate) async fn tick(&self) -> Result<(), ChvError> {
        let tick_start = std::time::Instant::now();

        // Record tick start in convergence metrics
        {
            let mut cm = self.convergence_metrics.write().await;
            cm.tick_start();
        }

        // Update ADR-009 gauges
        let vm_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vms")
            .fetch_one(&self.pool)
            .await
            .unwrap_or(0);
        metrics::gauge!(CHV_VMS_TOTAL).set(vm_count as f64);

        let node_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM nodes WHERE status = 'TenantReady'")
                .fetch_one(&self.pool)
                .await
                .unwrap_or(0);
        metrics::gauge!(CHV_NODES_READY).set(node_count as f64);

        // Compute drift: count resources where desired_generation != observed_generation
        let vm_drift: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*) FROM vm_desired_state vds
            LEFT JOIN vm_observed_state vos ON vds.vm_id = vos.vm_id
            WHERE vds.desired_generation != COALESCE(vos.observed_generation, -1)
            "#,
        )
        .fetch_one(&self.pool)
        .await
        .unwrap_or(0);

        let volume_drift: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*) FROM volume_desired_state vds
            LEFT JOIN volume_observed_state vos ON vds.volume_id = vos.volume_id
            WHERE vds.desired_generation != COALESCE(vos.observed_generation, -1)
            "#,
        )
        .fetch_one(&self.pool)
        .await
        .unwrap_or(0);

        let network_drift: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*) FROM network_desired_state nds
            LEFT JOIN network_observed_state nos ON nds.network_id = nos.network_id
            WHERE nds.desired_generation != COALESCE(nos.observed_generation, -1)
              -- #499: a 'Deleting' tombstone never converges (network
              -- delete is BFF-direct with no agent dispatch, so no
              -- fragment ever reports the tombstone's generation) —
              -- without this exclusion every deleted network would
              -- count as permanent drift. Unlike the VM/volume
              -- tombstones, whose dispatches do converge the observed
              -- generation, there is nothing to wait for.
              AND (nds.desired_status IS NULL OR nds.desired_status != 'Deleting')
            "#,
        )
        .fetch_one(&self.pool)
        .await
        .unwrap_or(0);

        let total_drift = (vm_drift + volume_drift + network_drift) as u32;

        // Count pending operations (Accepted + RetryPending)
        let pending_ops: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM operations WHERE status IN ('Accepted', 'RetryPending')",
        )
        .fetch_one(&self.pool)
        .await
        .unwrap_or(0);

        // Update drift in convergence metrics
        {
            let mut cm = self.convergence_metrics.write().await;
            cm.record_drift(total_drift);
            cm.record_pending_operations(pending_ops as u32);
        }

        self.reap_stuck_operations().await?;
        self.check_node_liveness().await?;

        // Atomically claim operations by marking them Running and resolve node_id
        // in the same statement using correlated subqueries in the RETURNING clause.
        // This prevents double-dispatch if tick overlaps (takes longer than interval)
        // and eliminates an N+1 query (was: 2 + N round trips per tick; now: 2).
        // SQLite 3.35+ supports correlated subqueries in RETURNING (sqlx 0.8 bundles 3.46+).
        let claimed_rows = sqlx::query_as::<_, AcceptedOperationRow>(
            r#"
            UPDATE operations SET status = 'Running', updated_by = 'orchestrator'
            WHERE operation_id IN (
                SELECT o.operation_id
                FROM operations o
                WHERE o.status = 'Accepted'
                ORDER BY o.requested_at ASC
                LIMIT 10
            )
            RETURNING
                operation_id,
                operation_type,
                resource_kind,
                resource_id,
                desired_generation,
                correlation_id,
                COALESCE(
                    (SELECT target_node_id FROM vm_desired_state WHERE vm_id = operations.resource_id),
                    (SELECT node_id FROM volumes WHERE volume_id = operations.resource_id),
                    (SELECT node_id FROM networks WHERE network_id = operations.resource_id)
                ) AS node_id,
                (SELECT storage_class FROM volumes WHERE volume_id = operations.resource_id) AS volume_storage_class,
                (SELECT capacity_bytes FROM volumes WHERE volume_id = operations.resource_id) AS volume_capacity_bytes,
                (SELECT volume_kind FROM volumes WHERE volume_id = operations.resource_id) AS volume_kind
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ChvError::Internal {
            reason: format!("failed to claim accepted operations: {e}"),
        })?;

        // Also claim operations that are pending retry and whose next_retry_at has passed.
        // Same node_id resolution strategy as the Accepted-claim above.
        let retryable_rows = sqlx::query_as::<_, AcceptedOperationRow>(
            r#"
            UPDATE operations SET status = 'Running', updated_by = 'orchestrator'
            WHERE operation_id IN (
                SELECT o.operation_id
                FROM operations o
                WHERE o.status = 'RetryPending'
                  AND o.next_retry_at <= strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
                ORDER BY o.next_retry_at ASC
                LIMIT 5
            )
            RETURNING
                operation_id,
                operation_type,
                resource_kind,
                resource_id,
                desired_generation,
                correlation_id,
                COALESCE(
                    (SELECT target_node_id FROM vm_desired_state WHERE vm_id = operations.resource_id),
                    (SELECT node_id FROM volumes WHERE volume_id = operations.resource_id),
                    (SELECT node_id FROM networks WHERE network_id = operations.resource_id)
                ) AS node_id,
                (SELECT storage_class FROM volumes WHERE volume_id = operations.resource_id) AS volume_storage_class,
                (SELECT capacity_bytes FROM volumes WHERE volume_id = operations.resource_id) AS volume_capacity_bytes,
                (SELECT volume_kind FROM volumes WHERE volume_id = operations.resource_id) AS volume_kind
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ChvError::Internal {
            reason: format!("failed to claim retryable operations: {e}"),
        })?;

        // Combine accepted + retryable claims, preserving order (accepted first, then retryable).
        let mut rows = Vec::with_capacity(claimed_rows.len() + retryable_rows.len());
        rows.extend(claimed_rows);
        rows.extend(retryable_rows);

        metrics::gauge!("orchestrator_operations_accepted").set(rows.len() as f64);

        type DispatchFut<'a> = std::pin::Pin<
            Box<dyn std::future::Future<Output = (usize, Result<(), ChvError>, f64)> + Send + 'a>,
        >;
        let mut futs: Vec<DispatchFut<'_>> = Vec::with_capacity(rows.len());

        for (idx, row) in rows.iter().enumerate() {
            futs.push(Box::pin(async move {
                let start = std::time::Instant::now();
                let result = self.dispatch_operation(row).await;
                let duration = start.elapsed().as_secs_f64();
                (idx, result, duration)
            }));
        }

        let dispatch_results = futures::future::join_all(futs).await;

        for (idx, dispatch_result, duration) in dispatch_results {
            let row = &rows[idx];
            let status_label = if dispatch_result.is_ok() {
                "success"
            } else {
                "failure"
            };
            metrics::counter!(
                "orchestrator_operations_dispatched_total",
                "type" => row.operation_type.clone(),
                "status" => status_label,
            )
            .increment(1);
            metrics::histogram!(
                "orchestrator_dispatch_duration_seconds",
                "type" => row.operation_type.clone(),
            )
            .record(duration);
            metrics::histogram!(
                CHV_OPERATION_DURATION_SECONDS,
                "operation" => row.operation_type.clone(),
            )
            .record(duration);

            if let Err(e) = dispatch_result {
                warn!(
                    operation_id = %row.operation_id,
                    operation_type = %row.operation_type,
                    error = %e,
                    "dispatch failed"
                );

                // #378 §7 fast-fail: gRPC UNIMPLEMENTED is terminal-class
                // for this method on this peer — a verbatim retry can never
                // succeed — so the operation must NOT enter the shared
                // retry arm. This bypass is load-bearing: the retry arm
                // below would call `mark_for_retry`, whose UPDATE has no
                // status guard and would RESURRECT the terminal `Failed`
                // row `dispatch_operation` just wrote back to
                // `RetryPending`. The terminal row (Failed /
                // UNSUPPORTED_BY_AGENT, carrying the agent's refusal
                // message) is written in `dispatch_operation`'s error arm
                // before the error propagates here.
                if matches!(e, ChvError::Unimplemented { .. }) {
                    info!(
                        operation_id = %row.operation_id,
                        operation_type = %row.operation_type,
                        "dispatch refused with Unimplemented: operation failed terminally without retry"
                    );
                    continue;
                }

                // Check current retry count
                let retry_count: i32 =
                    sqlx::query_scalar("SELECT retry_count FROM operations WHERE operation_id = ?")
                        .bind(&row.operation_id)
                        .fetch_one(&self.pool)
                        .await
                        .unwrap_or(0);

                let new_retry_count = retry_count + 1;
                if new_retry_count <= MAX_DISPATCH_RETRIES {
                    // Schedule retry with exponential backoff: 10s, 20s, 40s
                    let backoff_secs = 10i64 * (1 << (new_retry_count - 1));
                    let next_retry = chrono::Utc::now() + chrono::Duration::seconds(backoff_secs);
                    let op_id = OperationId::new(row.operation_id.clone()).map_err(|e| {
                        ChvError::Internal {
                            reason: format!("invalid operation_id: {e}"),
                        }
                    })?;
                    if let Err(retry_err) = self
                        .operation_repo
                        .mark_for_retry(
                            &op_id,
                            new_retry_count,
                            &next_retry.to_rfc3339(),
                            &e.to_string(),
                            now_unix_ms(),
                        )
                        .await
                    {
                        error!(
                            operation_id = %row.operation_id,
                            error = %retry_err,
                            "failed to mark operation for retry"
                        );
                    } else {
                        info!(
                            operation_id = %row.operation_id,
                            retry = new_retry_count,
                            next_retry_at = %next_retry.to_rfc3339(),
                            "operation scheduled for retry"
                        );
                    }
                } else {
                    // Permanently failed after exhausting retries
                    if let Err(update_err) = self
                        .operation_repo
                        .update_status(&OperationStatusUpdateInput {
                            operation_id: OperationId::new(row.operation_id.clone()).map_err(
                                |e| ChvError::Internal {
                                    reason: format!("invalid operation_id: {e}"),
                                },
                            )?,
                            status: OperationStatus::Failed,
                            error_code: Some("DISPATCH_FAILED".into()),
                            error_message: Some(format!(
                                "permanently failed after {} retries: {}",
                                MAX_DISPATCH_RETRIES, e
                            )),
                            observed_generation: None,
                            updated_by: Some("orchestrator".into()),
                            updated_unix_ms: now_unix_ms(),
                        })
                        .await
                    {
                        error!(
                            operation_id = %row.operation_id,
                            error = %update_err,
                            "failed to update operation status after exhausting retries"
                        );
                    }
                }
            }
        }

        // Record dispatch metrics and emit prometheus convergence gauges
        let dispatched_count = rows.len() as u64;
        let elapsed_ms = tick_start.elapsed().as_secs_f64() * 1000.0;
        {
            let mut cm = self.convergence_metrics.write().await;
            cm.record_dispatch(dispatched_count, elapsed_ms);
            cm.emit_prometheus();
        }

        // #368 P2: re-drive terminally-failed creates (bounded).
        self.redrive_failed_creates().await?;

        Ok(())
    }

    async fn reap_stuck_operations(&self) -> Result<u64, ChvError> {
        let result = sqlx::query(
            r#"
            UPDATE operations SET status = 'Accepted', updated_by = 'reaper'
            WHERE status = 'Running'
              AND (
                (operation_type = 'MigrateVm' AND updated_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-7200 seconds'))
                OR
                (operation_type IN ('SnapshotVm', 'RestoreSnapshot', 'SnapshotVolume', 'RestoreVolume', 'CloneVolume') AND updated_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-600 seconds'))
                OR
                (operation_type = 'StartVm' AND updated_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-300 seconds'))
                OR
                (operation_type NOT IN ('MigrateVm', 'SnapshotVm', 'RestoreSnapshot', 'SnapshotVolume', 'RestoreVolume', 'CloneVolume', 'StartVm') AND updated_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-120 seconds'))
              )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| ChvError::Internal {
            reason: format!("failed to reap stuck operations: {e}"),
        })?;

        let reaped = result.rows_affected();
        if reaped > 0 {
            warn!(
                count = reaped,
                "reaped stuck Running operations back to Accepted"
            );
        }
        Ok(reaped)
    }

    /// Detect nodes that have not reported observed state within 60 seconds and mark
    /// them as Unreachable so the scheduler will not place new VMs there.
    async fn check_node_liveness(&self) -> Result<(), ChvError> {
        let stale_nodes: Vec<(String,)> = sqlx::query_as(
            r#"
            SELECT node_id FROM node_observed_state
            WHERE observed_state NOT IN ('Unreachable', 'Failed')
              AND last_seen_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-60 seconds')
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ChvError::Internal {
            reason: format!("failed to query stale nodes: {e}"),
        })?;

        for (node_id,) in &stale_nodes {
            warn!(node_id = %node_id, "node has not reported in 60s, marking Unreachable");
            sqlx::query(
                r#"UPDATE node_observed_state
                   SET observed_state = 'Unreachable',
                       updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
                   WHERE node_id = ?"#,
            )
            .bind(node_id)
            .execute(&self.pool)
            .await
            .map_err(|e| ChvError::Internal {
                reason: format!("failed to mark node {node_id} as Unreachable: {e}"),
            })?;

            // Evict from connection pool since the agent is likely dead
            self.node_client_pool.evict(node_id);
        }

        Ok(())
    }

    /// #368 P2: bounded re-drive of terminally-failed VM creates.
    ///
    /// A create whose Core effect terminally failed on the agent (the
    /// #368 class: a transient backend failure) leaves the VM
    /// desired-but-never-created — the dispatch already acked ok, so the
    /// dispatch loop never converges it. The agent's #368 P1 telemetry
    /// reports that shape as `runtime_status = 'Failed'` with the Core
    /// failure code; this pass keys on exactly that signal and re-issues
    /// the create as a NEW `RecreateVm` operation at the original
    /// create's generation. The agent's dispatch shim routes it through
    /// the Core requeue primitive (C1): a new journaled CreateVm whose
    /// effector re-executes residue-idempotently. On success the agent's
    /// projection reports the VM again and this pass stops selecting it.
    ///
    /// Gates — every refusal arm just stops the loop for that VM
    /// (nothing is guessed; the agent-side requeue refuses independently):
    /// - the desired state still demands the VM (a `vm_desired_state`
    ///   row that is not deleted — an operator delete stops the loop);
    /// - no incomplete create-family operation is in flight (this is
    ///   also the concurrent-delete and crash-during-re-drive safety: an
    ///   in-flight delete dispatch or an inspect-required re-drive holds
    ///   the gate closed);
    /// - the bound is not exhausted: at most `MAX_DISPATCH_RETRIES`
    ///   re-drive operations per VM, spaced by the dispatch-retry backoff
    ///   curve (10s, 20s, 40s). No new config knobs.
    ///
    /// On exhaustion the pass marks a terminal Failed `RecreateVm`
    /// operation carrying the reported failure code — the VM is visibly
    /// Failed-with-reason, never a silent zombie.
    ///
    /// `pub(crate)` so the crate's integration tests can drive the pass
    /// (and `tick`) without a wall-clock interval, the same seam
    /// `dispatch_update_overlay` already provides.
    pub(crate) async fn redrive_failed_creates(&self) -> Result<(), ChvError> {
        let rows = sqlx::query_as::<_, FailedCreateRedriveRow>(
            r#"
            SELECT
                vds.vm_id,
                vds.target_node_id,
                (SELECT o.desired_generation FROM operations o
                 WHERE o.resource_kind = 'vm' AND o.resource_id = vds.vm_id
                   AND o.operation_type IN ('create', 'CreateVm')
                 ORDER BY o.requested_at ASC LIMIT 1) AS create_generation,
                (SELECT COUNT(*) FROM operations rd
                 WHERE rd.resource_kind = 'vm' AND rd.resource_id = vds.vm_id
                   AND rd.operation_type = 'RecreateVm') AS redrive_count,
                (SELECT rd.updated_at FROM operations rd
                 WHERE rd.resource_kind = 'vm' AND rd.resource_id = vds.vm_id
                   AND rd.operation_type = 'RecreateVm'
                 ORDER BY rd.updated_at DESC LIMIT 1) AS last_redrive_at,
                vos.last_error AS reported_error
            FROM vm_desired_state vds
            JOIN vm_observed_state vos ON vds.vm_id = vos.vm_id
            WHERE vos.runtime_status = 'Failed'
              AND COALESCE(vds.desired_power_state, '') != 'Deleted'
              AND vds.target_node_id IS NOT NULL
              AND NOT EXISTS (
                  SELECT 1 FROM operations o
                  WHERE o.resource_kind = 'vm' AND o.resource_id = vds.vm_id
                    AND o.operation_type IN ('create', 'CreateVm', 'RecreateVm')
                    AND o.status IN ('Accepted', 'RetryPending', 'Running')
              )
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ChvError::Internal {
            reason: format!("failed to select failed creates for re-drive: {e}"),
        })?;

        for row in rows {
            let Some(create_generation) = row.create_generation.filter(|gen| *gen > 0) else {
                warn!(
                    vm_id = %row.vm_id,
                    "failed create has no journaled create operation to derive a generation from; not re-driving"
                );
                continue;
            };

            if row.redrive_count >= i64::from(MAX_DISPATCH_RETRIES) {
                // Exhausted: mark a terminal Failed re-drive operation
                // carrying the reported failure code, once (the fixed
                // idempotency key converges repeat ticks on the one row).
                let marker_key = format!("recreate:{}:{}:exhausted", row.vm_id, create_generation);
                let reported = row
                    .reported_error
                    .clone()
                    .unwrap_or_else(|| "CREATE_FAILED".to_owned());
                match self
                    .mark_redrive_exhausted(&row.vm_id, &marker_key, &reported)
                    .await
                {
                    Ok(true) => {
                        warn!(
                            vm_id = %row.vm_id,
                            attempts = MAX_DISPATCH_RETRIES,
                            failure_code = %reported,
                            "create re-drives exhausted; VM marked Failed"
                        );
                        metrics::counter!("orchestrator_create_redrive_exhausted_total")
                            .increment(1);
                    }
                    Ok(false) => {}
                    Err(e) => {
                        warn!(
                            vm_id = %row.vm_id,
                            error = %e,
                            "failed to mark create re-drive exhaustion"
                        );
                    }
                }
                continue;
            }

            // Re-drive spacing: the dispatch-retry backoff curve
            // (10s * 2^(attempt-1)) measured from the previous re-drive's
            // last update.
            if let Some(last_redrive_at) = row.last_redrive_at.as_deref() {
                let attempt = row.redrive_count.max(1) as u32;
                let backoff_secs = 10i64 * (1 << (attempt - 1));
                match chrono::DateTime::parse_from_rfc3339(last_redrive_at) {
                    Ok(last)
                        if chrono::Utc::now() < last + chrono::Duration::seconds(backoff_secs) =>
                    {
                        continue;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!(
                            vm_id = %row.vm_id,
                            last_redrive_at,
                            error = %e,
                            "failed to parse last re-drive timestamp; not re-driving"
                        );
                        continue;
                    }
                }
            }

            let attempt = row.redrive_count + 1;
            let operation_id =
                OperationId::new(format!("RecreateVm-{}", chv_common::gen_resource_id())).map_err(
                    |e| ChvError::Internal {
                        reason: format!("invalid operation_id: {e}"),
                    },
                )?;
            let vm_id = ResourceId::new(row.vm_id.clone()).map_err(|e| ChvError::Internal {
                reason: format!("invalid resource_id: {e}"),
            })?;
            // Deterministic per (vm, generation, attempt): a racing tick
            // converges on the one operation row instead of double-issuing.
            let idempotency_key =
                format!("recreate:{}:{}:{}", row.vm_id, create_generation, attempt);
            self.operation_repo
                .create_or_get(&OperationCreateInput {
                    operation_id,
                    idempotency_key,
                    resource_kind: ResourceKind::Vm,
                    resource_id: Some(vm_id),
                    operation_type: "RecreateVm".into(),
                    status: OperationStatus::Accepted,
                    requested_by: Some("orchestrator".into()),
                    updated_by: Some("orchestrator".into()),
                    desired_generation: Some(Generation::new(create_generation as u64)),
                    observed_generation: None,
                    correlation_id: Some(format!("redrive-attempt={}", attempt)),
                    requested_unix_ms: now_unix_ms(),
                })
                .await
                .map_err(|e| ChvError::Internal {
                    reason: format!("failed to create re-drive operation: {e}"),
                })?;
            info!(
                vm_id = %row.vm_id,
                node_id = %row.target_node_id,
                attempt = attempt,
                generation = create_generation,
                "issuing create re-drive"
            );
            metrics::counter!("orchestrator_create_redrives_total").increment(1);
        }
        Ok(())
    }

    /// Marks the re-drive exhaustion for one VM: a terminal Failed
    /// `RecreateVm` operation with the reported failure code. Returns
    /// `true` when this call created the marker (the fixed idempotency
    /// key converges repeat invocations on the existing row).
    async fn mark_redrive_exhausted(
        &self,
        vm_id: &str,
        idempotency_key: &str,
        reported_failure: &str,
    ) -> Result<bool, ChvError> {
        let resource_id = ResourceId::new(vm_id.to_owned()).map_err(|e| ChvError::Internal {
            reason: format!("invalid resource_id: {e}"),
        })?;
        let operation_id =
            OperationId::new(format!("RecreateVm-{}", chv_common::gen_resource_id())).map_err(
                |e| ChvError::Internal {
                    reason: format!("invalid operation_id: {e}"),
                },
            )?;
        let receipt = self
            .operation_repo
            .create_or_get(&OperationCreateInput {
                operation_id,
                idempotency_key: idempotency_key.to_owned(),
                resource_kind: ResourceKind::Vm,
                resource_id: Some(resource_id),
                operation_type: "RecreateVm".into(),
                status: OperationStatus::Pending,
                requested_by: Some("orchestrator".into()),
                updated_by: Some("orchestrator".into()),
                desired_generation: None,
                observed_generation: None,
                correlation_id: Some("redrive-exhausted".into()),
                requested_unix_ms: now_unix_ms(),
            })
            .await
            .map_err(|e| ChvError::Internal {
                reason: format!("failed to create re-drive exhaustion marker: {e}"),
            })?;
        if receipt.status.is_terminal() {
            // Already marked by a previous tick.
            return Ok(false);
        }
        self.operation_repo
            .update_status(&OperationStatusUpdateInput {
                operation_id: receipt.operation_id,
                status: OperationStatus::Failed,
                error_code: Some("CREATE_REDRIVE_EXHAUSTED".into()),
                error_message: Some(format!(
                    "create re-drives exhausted after {} attempts; last reported failure: {}",
                    MAX_DISPATCH_RETRIES, reported_failure
                )),
                observed_generation: None,
                updated_by: Some("orchestrator".into()),
                updated_unix_ms: now_unix_ms(),
            })
            .await
            .map_err(|e| ChvError::Internal {
                reason: format!("failed to mark re-drive exhaustion terminal: {e}"),
            })?;
        Ok(true)
    }

    async fn dispatch_operation(&self, row: &AcceptedOperationRow) -> Result<(), ChvError> {
        // UpdateOverlay fans out to EVERY participating node of the network
        // (ADR-021 bounded flood list), not just the claim-resolved anchor
        // node, so it bypasses the single-node resolution below entirely.
        if row.operation_type == "UpdateOverlay" {
            return self
                .dispatch_update_overlay(&row.operation_id, &row.resource_id)
                .await;
        }

        // #355 (PR 1 of the decomposition): the network firewall-policy
        // dispatch fans out to every node with an attached VM on the
        // network (DP2 — `networks.node_id` is NULL for the
        // operator-created networks that carry rules, and materialization
        // is per-node and lazy), so it bypasses the single-node resolution
        // below exactly like UpdateOverlay. Dead-but-live with this PR:
        // no producer journals an `UpdateNetworkPolicy` operation until
        // the BFF route lands (PR 2).
        if row.operation_type == "UpdateNetworkPolicy" {
            return self
                .dispatch_update_network_policy(&row.operation_id, &row.resource_id)
                .await;
        }

        let node_id = row
            .node_id
            .as_deref()
            .ok_or_else(|| ChvError::InvalidArgument {
                field: "node_id".to_string(),
                reason: format!("operation {} has no target node", row.operation_id),
            })?;

        // Schedulability check: operations that place new workloads require TenantReady
        if Self::requires_schedulable_node(&row.operation_type) {
            self.require_node_schedulable(node_id).await?;
        }

        let socket_path = resolve_agent_socket(&self.agent_socket_pattern, node_id)?;
        let mut client = self
            .node_client_pool
            .get_or_connect(node_id, &socket_path)
            .await?;

        let generation = match row.desired_generation {
            Some(g) => g.to_string(),
            None => {
                // Fetch the node's current observed_generation from the DB
                // rather than defaulting to "1" which could cause stale operations.
                let observed: Option<i64> = sqlx::query_scalar(
                    "SELECT observed_generation FROM vm_observed_state WHERE vm_id = ?",
                )
                .bind(&row.resource_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| ChvError::Internal {
                    reason: format!(
                        "failed to fetch observed_generation for {}: {e}",
                        row.resource_id
                    ),
                })?
                .flatten();
                observed.unwrap_or(1).to_string()
            }
        };

        // Status already set to Running by the atomic claim in tick()

        let ack = match row.operation_type.as_str() {
            // "RecreateVm" is the #368 P2 re-drive: dispatched exactly like
            // a create (same desired-state path, the original create's
            // generation) — the agent's shim routes it through the Core
            // requeue primitive when the journal holds a terminally failed
            // create, and acks idempotently when it has converged.
            "create" | "CreateVm" | "ResizeVm" | "RecreateVm" => {
                // Desired-state path: build full agent spec and dispatch ApplyVmDesiredState
                let vm_spec_json = self.build_agent_vm_spec(&row.resource_id).await?;
                client
                    .apply_vm_desired_state(
                        node_id,
                        &row.resource_id,
                        &generation,
                        vm_spec_json.into_bytes(),
                        &row.operation_id,
                        None,
                    )
                    .await
            }
            "StartVm" => {
                client
                    .start_vm(
                        node_id,
                        &row.resource_id,
                        &generation,
                        &row.operation_id,
                        None,
                    )
                    .await
            }
            "StopVm" => {
                client
                    .stop_vm(
                        node_id,
                        &row.resource_id,
                        &generation,
                        false,
                        &row.operation_id,
                        None,
                    )
                    .await
            }
            "ForceStopVm" => {
                client
                    .stop_vm(
                        node_id,
                        &row.resource_id,
                        &generation,
                        true,
                        &row.operation_id,
                        None,
                    )
                    .await
            }
            "RebootVm" => {
                let force_reboot = row
                    .correlation_id
                    .as_deref()
                    .map(|s| s.contains("force=true"))
                    .unwrap_or(false);
                client
                    .reboot_vm(
                        node_id,
                        &row.resource_id,
                        &generation,
                        force_reboot,
                        &row.operation_id,
                        None,
                    )
                    .await
            }
            "DeleteVm" => {
                client
                    .delete_vm(
                        node_id,
                        &row.resource_id,
                        &generation,
                        false,
                        &row.operation_id,
                        None,
                    )
                    .await
            }
            "SnapshotVm" => {
                let destination = row.correlation_id.as_deref().unwrap_or("");
                client
                    .snapshot_vm(
                        node_id,
                        &row.resource_id,
                        &generation,
                        destination,
                        &row.operation_id,
                        None,
                    )
                    .await
            }
            "RestoreSnapshot" => {
                let source = row.correlation_id.as_deref().unwrap_or("");
                client
                    .restore_snapshot(
                        node_id,
                        &row.resource_id,
                        &generation,
                        source,
                        &row.operation_id,
                        None,
                    )
                    .await
            }
            "AttachVolume" => {
                let corr = row.correlation_id.as_deref().unwrap_or("");
                let vm_id = corr.strip_prefix("vm=").unwrap_or(corr);
                client
                    .attach_volume(
                        node_id,
                        &row.resource_id,
                        vm_id,
                        &generation,
                        &row.operation_id,
                        None,
                        // #379 PR 2 (A8): the volume's class rides the
                        // attach dispatch (resolved in the claim query);
                        // NULL emits the key-free `{}` (PR 3 correction).
                        row.volume_storage_class.as_deref(),
                        // #533: the volume's KIND rides it too (resolved
                        // in the same claim query), so a standalone
                        // ('data') volume's attach opens at the #513
                        // create carrier's `{volume_id}.img` locator
                        // instead of the A4 bare-id default's second
                        // file — the #522 delete's DP4 destroy targets
                        // the carrier locator exactly.
                        row.volume_kind.as_deref(),
                    )
                    .await
            }
            "DetachVolume" => {
                let corr = row.correlation_id.as_deref().unwrap_or("");
                let vm_id = corr
                    .strip_prefix("vm=")
                    .and_then(|s| s.split(':').next())
                    .unwrap_or(corr);
                let force = corr.contains("force=true");
                client
                    .detach_volume(
                        node_id,
                        &row.resource_id,
                        vm_id,
                        &generation,
                        force,
                        &row.operation_id,
                        None,
                    )
                    .await
            }
            "ResizeVolume" => {
                let new_size = row
                    .correlation_id
                    .as_deref()
                    .and_then(|s| s.strip_prefix("size="))
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                client
                    .resize_volume(
                        node_id,
                        &row.resource_id,
                        &generation,
                        new_size,
                        &row.operation_id,
                        None,
                    )
                    .await
            }
            "SnapshotVolume" => {
                let snapshot_name = row.correlation_id.as_deref().unwrap_or("");
                client
                    .snapshot_volume(
                        node_id,
                        &row.resource_id,
                        &generation,
                        snapshot_name,
                        &row.operation_id,
                        None,
                    )
                    .await
            }
            "RestoreVolume" => {
                let snapshot_name = row.correlation_id.as_deref().unwrap_or("");
                client
                    .restore_volume(
                        node_id,
                        &row.resource_id,
                        &generation,
                        snapshot_name,
                        &row.operation_id,
                        None,
                    )
                    .await
            }
            "DeleteVolumeSnapshot" => {
                let snapshot_name = row.correlation_id.as_deref().unwrap_or("");
                client
                    .delete_volume_snapshot(
                        node_id,
                        &row.resource_id,
                        &generation,
                        snapshot_name,
                        &row.operation_id,
                        None,
                    )
                    .await
            }
            "CloneVolume" => {
                let source = row.correlation_id.as_deref().unwrap_or("");
                let source_volume_id = source.strip_prefix("source=").unwrap_or(source);
                client
                    .clone_volume(
                        node_id,
                        source_volume_id,
                        &row.resource_id,
                        &generation,
                        &row.operation_id,
                        None,
                    )
                    .await
            }
            // #513 PR 1 (DP2): the standalone volume-create dispatch
            // arm. MUST land before the BFF route (PR 2) — an unknown
            // operation_type is actively Failed below, so a journaled
            // CreateVolume with no arm would ship the
            // accepted-then-failed UX #378 was filed to kill. The
            // capacity and class resolve in the claim query (the
            // AttachVolume arm's single-round-trip discipline); the
            // agent's handler opens WITH the size (create-on-open), so
            // a missing/non-positive capacity is refused here rather
            // than dispatching an open that can never provision.
            "CreateVolume" => {
                let capacity = row.volume_capacity_bytes.unwrap_or(0);
                if capacity <= 0 {
                    return Err(ChvError::InvalidArgument {
                        field: "capacity_bytes".to_string(),
                        reason: format!(
                            "CreateVolume dispatch for {} requires a positive capacity_bytes \
                             on the volume row (resolved: {capacity})",
                            row.resource_id
                        ),
                    });
                }
                client
                    .create_volume(
                        node_id,
                        &row.resource_id,
                        capacity as u64,
                        &generation,
                        &row.operation_id,
                        None,
                        // The volume's class rides the dispatch through
                        // the #511 wire-key seam; NULL emits a
                        // key-free size-only payload (local default at
                        // the agent).
                        row.volume_storage_class.as_deref(),
                    )
                    .await
            }
            // #522 PR 1 (DP2): the volume-delete dispatch arm —
            // dead-but-live: no producer journals a `DeleteVolume`
            // operation until the BFF route lands (PR 2), and the arm
            // MUST precede it (an unknown operation_type is actively
            // Failed below — the accepted-then-failed UX #378 was
            // filed to kill). Simpler than create's arm: no capacity
            // refusal, no placement check (a delete reclaims, it does
            // not place); the class resolves in the claim query (the
            // attach arm's discipline) so the agent can shape the DP4
            // carrier locator; NULL emits the empty string.
            "DeleteVolume" => {
                client
                    .delete_volume(
                        node_id,
                        &row.resource_id,
                        &generation,
                        &row.operation_id,
                        None,
                        row.volume_storage_class.as_deref(),
                    )
                    .await
            }
            "StartNetwork" => {
                client
                    .start_network(
                        node_id,
                        &row.resource_id,
                        &generation,
                        &row.operation_id,
                        None,
                    )
                    .await
            }
            "StopNetwork" => {
                client
                    .stop_network(
                        node_id,
                        &row.resource_id,
                        &generation,
                        false,
                        &row.operation_id,
                        None,
                    )
                    .await
            }
            "ForceStopNetwork" => {
                client
                    .stop_network(
                        node_id,
                        &row.resource_id,
                        &generation,
                        true,
                        &row.operation_id,
                        None,
                    )
                    .await
            }
            "RestartNetwork" => {
                client
                    .restart_network(
                        node_id,
                        &row.resource_id,
                        &generation,
                        &row.operation_id,
                        None,
                    )
                    .await
            }
            "PauseVm" => {
                client
                    .pause_vm(
                        node_id,
                        &row.resource_id,
                        &generation,
                        &row.operation_id,
                        None,
                    )
                    .await
            }
            "ResumeVm" => {
                client
                    .resume_vm(
                        node_id,
                        &row.resource_id,
                        &generation,
                        &row.operation_id,
                        None,
                    )
                    .await
            }
            "PowerButtonVm" => {
                client
                    .power_button_vm(
                        node_id,
                        &row.resource_id,
                        &generation,
                        &row.operation_id,
                        None,
                    )
                    .await
            }
            "CoredumpVm" => {
                let destination = row.correlation_id.as_deref().unwrap_or("");
                client
                    .coredump_vm(
                        node_id,
                        &row.resource_id,
                        &generation,
                        destination,
                        &row.operation_id,
                        None,
                    )
                    .await
            }
            "MigrateVm" => {
                // MigrateVm is a long-running operation driven by the migration state machine.
                // Parse correlation_id to extract source, dest, and config.
                let corr = row.correlation_id.as_deref().unwrap_or("");
                let (source_node_id, dest_node_id, config) =
                    crate::migration::MigrationConfig::from_correlation_id(corr);

                if source_node_id.is_empty() || dest_node_id.is_empty() {
                    return Err(ChvError::InvalidArgument {
                        field: "correlation_id".to_string(),
                        reason: format!(
                            "MigrateVm requires source= and dest= in correlation_id, got: {}",
                            corr
                        ),
                    });
                }

                let migration_id = format!("mig-{}", row.operation_id);
                let mut state = crate::migration::MigrationState {
                    migration_id: migration_id.clone(),
                    operation_id: row.operation_id.clone(),
                    vm_id: row.resource_id.clone(),
                    source_node_id: source_node_id.clone(),
                    dest_node_id: dest_node_id.clone(),
                    phase: crate::migration::MigrationPhase::Pending,
                    config,
                    bytes_transferred: 0,
                    total_bytes: 0,
                    convergence_round: 0,
                    dirty_blocks_remaining: 0,
                };

                // Create migration record in DB
                crate::migration::create_migration_record(&self.pool, &state).await?;

                // Execute the migration state machine
                let result = crate::migration::execute_migration(
                    &self.pool,
                    &self.node_client_pool,
                    &self.agent_socket_pattern,
                    &mut state,
                    self.migration_signing_secret.as_deref(),
                    self.migration_token_ttl,
                    self.allow_insecure_migration_authorization,
                )
                .await;

                // Mark the operation based on migration result
                let (final_status, error_message) = match &result {
                    Ok(()) => (OperationStatus::Succeeded, None),
                    Err(e) => (OperationStatus::Failed, Some(e.to_string())),
                };

                self.operation_repo
                    .update_status(&OperationStatusUpdateInput {
                        operation_id: OperationId::new(row.operation_id.clone()).map_err(|e| {
                            ChvError::Internal {
                                reason: format!("invalid operation_id: {e}"),
                            }
                        })?,
                        status: final_status,
                        error_code: if result.is_err() {
                            Some("MIGRATION_FAILED".into())
                        } else {
                            None
                        },
                        error_message,
                        observed_generation: None,
                        updated_by: Some("orchestrator".into()),
                        updated_unix_ms: now_unix_ms(),
                    })
                    .await
                    .map_err(|e| ChvError::Internal {
                        reason: format!("failed to mark migration operation terminal: {e}"),
                    })?;

                // Return Ok since we handled the status update ourselves
                return match result {
                    Ok(()) => Ok(()),
                    Err(e) => Err(e),
                };
            }
            other => {
                return Err(ChvError::Internal {
                    reason: format!("unsupported operation_type for dispatch: {other}"),
                });
            }
        };

        match ack {
            Ok(result) => {
                let status = result
                    .result
                    .as_ref()
                    .map(|r| r.status.as_str())
                    .unwrap_or("OK");
                let accepted = status.eq_ignore_ascii_case("ok");
                let final_status = if accepted {
                    OperationStatus::Succeeded
                } else {
                    OperationStatus::Failed
                };
                let error_message = if accepted {
                    None
                } else {
                    result.result.map(|r| r.human_summary)
                };
                self.operation_repo
                    .update_status(&OperationStatusUpdateInput {
                        operation_id: OperationId::new(row.operation_id.clone()).map_err(|e| {
                            ChvError::Internal {
                                reason: format!("invalid operation_id: {e}"),
                            }
                        })?,
                        status: final_status,
                        error_code: None,
                        error_message,
                        observed_generation: None,
                        updated_by: Some("orchestrator".into()),
                        updated_unix_ms: now_unix_ms(),
                    })
                    .await
                    .map_err(|e| ChvError::Internal {
                        reason: format!("failed to mark operation terminal: {e}"),
                    })?;

                // For successful resize, apply the new size to volumes.capacity_bytes
                if accepted && row.operation_type == "ResizeVolume" {
                    if let Some(new_size) = row
                        .correlation_id
                        .as_deref()
                        .and_then(|s| s.strip_prefix("size="))
                        .and_then(|s| s.parse::<i64>().ok())
                    {
                        let volume_id = &row.resource_id;
                        if let Err(e) =
                            sqlx::query("UPDATE volumes SET capacity_bytes = ? WHERE volume_id = ?")
                                .bind(new_size)
                                .bind(volume_id)
                                .execute(&self.pool)
                                .await
                        {
                            tracing::error!(
                                operation_id = %row.operation_id,
                                volume_id = %volume_id,
                                new_size = new_size,
                                error = %e,
                                "failed to persist resized capacity after successful dispatch"
                            );
                        }
                        if let Err(e) = sqlx::query(
                            "UPDATE volume_desired_state SET resize_to_bytes = NULL WHERE volume_id = ?"
                        )
                        .bind(volume_id)
                        .execute(&self.pool)
                        .await
                        {
                            tracing::error!(
                                operation_id = %row.operation_id,
                                volume_id = %volume_id,
                                error = %e,
                                "failed to clear resize_to_bytes after successful dispatch"
                            );
                        }
                    }
                }

                info!(
                    operation_id = %row.operation_id,
                    operation_type = %row.operation_type,
                    node_id = %node_id,
                    "dispatch succeeded"
                );
                Ok(())
            }
            Err(e) => {
                if matches!(e, ChvError::BackendUnavailable { .. }) {
                    self.node_client_pool.evict(node_id);
                }
                // #378 §7 fast-fail: an UNIMPLEMENTED answer names the
                // cause (the agent does not implement this RPC — e.g. a
                // legacy-only surface behind the Core authority), so the
                // terminal row's error code must name it too. The tick
                // handler bypasses its retry arm for this class; the
                // message text is the agent's own refusal explanation.
                let error_code = if matches!(e, ChvError::Unimplemented { .. }) {
                    UNSUPPORTED_BY_AGENT_ERROR_CODE
                } else {
                    "AGENT_REJECTED"
                };
                self.operation_repo
                    .update_status(&OperationStatusUpdateInput {
                        operation_id: OperationId::new(row.operation_id.clone()).map_err(|e| {
                            ChvError::Internal {
                                reason: format!("invalid operation_id: {e}"),
                            }
                        })?,
                        status: OperationStatus::Failed,
                        error_code: Some(error_code.into()),
                        error_message: Some(e.to_string()),
                        observed_generation: None,
                        updated_by: Some("orchestrator".into()),
                        updated_unix_ms: now_unix_ms(),
                    })
                    .await
                    // #378 §7 fast-fail: if this terminal write itself
                    // fails, the Unimplemented identity is deliberately
                    // flattened to Internal (not preserved) — the row
                    // stays retryable and a re-dispatch re-derives it.
                    .map_err(|e2| ChvError::Internal {
                        reason: format!("agent rejected operation and status update failed: {e2}"),
                    })?;
                Err(e)
            }
        }
    }

    /// Dispatch an `UpdateOverlay` operation (ADR-021): compile per-node
    /// fabric plans for the network and fan them out to every participating
    /// node agent via the overlay manager.
    ///
    /// On success the operation is marked `Succeeded` here (the generic
    /// single-node ack handling in `dispatch_operation` does not apply to a
    /// fan-out). On failure the error propagates to `tick()` — except the
    /// #378 §7 fast-fail shape: when every per-node failure was an
    /// `Unimplemented` refusal, the overlay manager preserves the refusal
    /// identity, and this arm mirrors the single-node dispatch path by
    /// writing the terminal `Failed` / `UNSUPPORTED_BY_AGENT` row BEFORE the
    /// error propagates (the tick's Unimplemented bypass skips
    /// `mark_for_retry`, so without this write the row would be stranded in
    /// `Running`). Every other error class propagates with no terminal
    /// write, keeping the shared retry machinery byte-for-byte.
    ///
    /// `pub(crate)` so integration tests can invoke the dispatch directly
    /// (driving the full orchestrator tick loop requires live agent
    /// sockets for every claimed operation).
    pub(crate) async fn dispatch_update_overlay(
        &self,
        operation_id: &str,
        network_id: &str,
    ) -> Result<(), ChvError> {
        let overlay_manager = self
            .overlay_manager
            .as_ref()
            .ok_or_else(|| ChvError::Internal {
                reason: "overlay manager is not configured; cannot dispatch UpdateOverlay"
                    .to_string(),
            })?;

        let plans = self
            .fabric_planner
            .compile_for_network(network_id)
            .await
            .map_err(|e| ChvError::Internal {
                reason: format!("failed to compile fabric plan for network {network_id}: {e}"),
            })?;

        if let Err(e) = overlay_manager
            .send_fabric_update(network_id, &plans, operation_id)
            .await
        {
            // #378 §7 fast-fail, UpdateOverlay leg: an all-refusals
            // fan-out arrives here as `ChvError::Unimplemented` (the
            // overlay manager preserves the identity exactly when every
            // per-node failure was a refusal). Mirror the single-node
            // dispatch arm: write the terminal Failed row with the
            // cause-naming UNSUPPORTED_BY_AGENT code carrying the agents'
            // refusal detail BEFORE propagating, because the tick's
            // Unimplemented bypass never reaches mark_for_retry — without
            // this write the row would sit in Running forever. In the
            // partial-success shape (#502) the roll-up reason riding
            // `error_message` names both sets — refusing nodes with their
            // per-node error text, and the applied nodes — so the #530
            // surfaces (task detail, TaskTimeline, `chvctl task watch`)
            // render the distinction verbatim with zero client work. Any
            // other error class (mixed failures included) propagates
            // unchanged with no terminal write, so the shared retry curve
            // keeps today's semantics.
            if matches!(e, ChvError::Unimplemented { .. }) {
                self.operation_repo
                    .update_status(&OperationStatusUpdateInput {
                        operation_id: OperationId::new(operation_id.to_string()).map_err(|e| {
                            ChvError::Internal {
                                reason: format!("invalid operation_id: {e}"),
                            }
                        })?,
                        status: OperationStatus::Failed,
                        error_code: Some(UNSUPPORTED_BY_AGENT_ERROR_CODE.into()),
                        error_message: Some(e.to_string()),
                        observed_generation: None,
                        updated_by: Some("orchestrator".into()),
                        updated_unix_ms: now_unix_ms(),
                    })
                    .await
                    // Same convention as the single-node arm: if the
                    // terminal write itself fails, the Unimplemented
                    // identity is flattened to Internal — the row stays
                    // retryable and a re-dispatch re-derives it.
                    .map_err(|e2| ChvError::Internal {
                        reason: format!("overlay fan-out refused and status update failed: {e2}"),
                    })?;
            }
            return Err(e);
        }

        let nodes: Vec<&str> = plans.iter().map(|p| p.node_id.as_str()).collect();
        info!(
            operation_id = operation_id,
            network_id = network_id,
            nodes = %nodes.join(","),
            "fabric plans dispatched to all participating nodes"
        );

        self.operation_repo
            .update_status(&OperationStatusUpdateInput {
                operation_id: OperationId::new(operation_id.to_string()).map_err(|e| {
                    ChvError::Internal {
                        reason: format!("invalid operation_id: {e}"),
                    }
                })?,
                status: OperationStatus::Succeeded,
                error_code: None,
                error_message: None,
                observed_generation: None,
                updated_by: Some("orchestrator".into()),
                updated_unix_ms: now_unix_ms(),
            })
            .await
            .map_err(|e| ChvError::Internal {
                reason: format!("failed to mark UpdateOverlay operation terminal: {e}"),
            })?;

        Ok(())
    }

    /// #355 (PR 1 of the decomposition): dispatch the network's stored
    /// firewall ruleset to every node with an attached VM on the
    /// network (DP2), with the UpdateOverlay fan-out discipline: the
    /// #378 §7 all-refusals fast-fail (terminal `Failed` /
    /// `UNSUPPORTED_BY_AGENT`, no retry), mixed failures on the shared
    /// retry curve, and the #502 partial-success roll-up naming both
    /// sets. Zero targets is a no-op `Succeeded` — a fleet network with
    /// rules but no attached VMs yet is the normal create-then-populate
    /// order, and the attach-time snapshot path applies the policy at
    /// first materialization. Dead-but-live with this PR: no producer
    /// journals an `UpdateNetworkPolicy` operation until the BFF route
    /// lands (PR 2).
    #[allow(clippy::too_many_lines)]
    pub(crate) async fn dispatch_update_network_policy(
        &self,
        operation_id: &str,
        network_id: &str,
    ) -> Result<(), ChvError> {
        // The stored ruleset + generation (the DP7 fence rides the
        // request's meta.desired_state_version; enforcement lands in
        // PR 3). A missing or tombstoned NDS row is a no-op success: a
        // policy update racing a network delete has nothing to apply —
        // the delete path tears the topology down at last-detach.
        let nds = sqlx::query_as::<_, (Option<String>, Option<String>, i64)>(
            "SELECT firewall_rules_json, desired_status, desired_generation \
             FROM network_desired_state WHERE network_id = ?",
        )
        .bind(network_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| ChvError::Internal {
            reason: format!("failed to query network desired state for {network_id}: {e}"),
        })?;
        let Some((firewall_rules_json, desired_status, desired_generation)) = nds else {
            info!(
                operation_id = operation_id,
                network_id = network_id,
                "network policy update: no desired-state row; no-op success"
            );
            return self.mark_network_policy_succeeded(operation_id).await;
        };
        if desired_status.as_deref() == Some("Deleting") {
            info!(
                operation_id = operation_id,
                network_id = network_id,
                "network policy update: network is Deleting; no-op success"
            );
            return self.mark_network_policy_succeeded(operation_id).await;
        }
        // DP4 (ruled 2026-10-08, #355 PR 3): `[]` — and a never-set
        // ruleset — mean "no user rules", and the boundary is the
        // shared BASELINE (DHCP UDP 67/68, DNS UDP/TCP 53, conntrack
        // from the engine) on top of default-deny. This replaces the
        // #360 workaround (a semantically empty ruleset was never
        // dispatched, because nwd's engine engages default-deny with
        // zero allows and would cut the network's guests off): the
        // baseline keeps DHCP/DNS alive inside the boundary, so empty
        // dispatches instead of skipping. The agent's carrier handler
        // resolves the same baseline belt-and-suspenders.
        let policy_json = match firewall_rules_json {
            Some(p) if !chv_common::firewall_ruleset_is_empty(&p) => p,
            _ => chv_common::firewall::baseline_policy_json(),
        };

        // DP2 target set: distinct nodes with at least one NIC of a
        // live (non-Deleting) VM on the network. Tombstone-aware so a
        // policy update racing a VM delete does not dispatch to a node
        // about to tear the topology down.
        let target_nodes: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT vds.target_node_id \
             FROM vm_nic_desired_state n \
             JOIN vm_desired_state vds ON vds.vm_id = n.vm_id \
             WHERE n.network_id = ? AND vds.target_node_id IS NOT NULL \
               AND COALESCE(vds.desired_status, '') != 'Deleting'",
        )
        .bind(network_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ChvError::Internal {
            reason: format!("failed to resolve network policy target nodes for {network_id}: {e}"),
        })?;
        if target_nodes.is_empty() {
            info!(
                operation_id = operation_id,
                network_id = network_id,
                "network policy update: no attached VMs; no-op success (applies at first attach)"
            );
            return self.mark_network_policy_succeeded(operation_id).await;
        }

        let mut failures: Vec<String> = Vec::new();
        let mut applied: Vec<String> = Vec::new();
        // #378 §7 fast-fail, network-policy leg: whether every
        // per-node failure was an `Unimplemented` refusal.
        let mut all_refusals = true;
        for node_id in &target_nodes {
            let dispatch = async {
                let socket_path = resolve_agent_socket(&self.agent_socket_pattern, node_id)?;
                let mut client = self
                    .node_client_pool
                    .get_or_connect(node_id, &socket_path)
                    .await?;
                client
                    .apply_network_policy(
                        node_id,
                        network_id,
                        &desired_generation.to_string(),
                        policy_json.as_bytes(),
                        operation_id,
                        Some("orchestrator"),
                    )
                    .await?;
                Ok::<(), ChvError>(())
            };
            match dispatch.await {
                Ok(()) => {
                    applied.push(node_id.clone());
                    info!(
                        operation_id = operation_id,
                        network_id = network_id,
                        node_id = %node_id,
                        "network policy dispatched to node"
                    );
                }
                Err(e) => {
                    warn!(
                        operation_id = operation_id,
                        network_id = network_id,
                        node_id = %node_id,
                        error = %e,
                        "failed to dispatch network policy to node"
                    );
                    if !matches!(e, ChvError::Unimplemented { .. }) {
                        all_refusals = false;
                    }
                    failures.push(format!("{node_id}: {e}"));
                }
            }
        }

        if !failures.is_empty() {
            // The UpdateOverlay roll-up discipline verbatim: an
            // all-refusals fan-out preserves the refusal identity so
            // the tick's Unimplemented bypass fast-fails the operation
            // terminal with the cause-naming code (the terminal row is
            // written HERE — the bypass never reaches mark_for_retry);
            // mixed failures keep the Internal aggregation and stay on
            // the shared retry curve. The #502 partial-success shape
            // names both sets.
            if all_refusals {
                let applied_summary = if applied.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; applied on {} node(s): {}",
                        applied.len(),
                        applied.join(", ")
                    )
                };
                let reason = format!(
                    "network policy update for {network_id} refused by all {} failing node(s): {}{}",
                    failures.len(),
                    failures.join("; "),
                    applied_summary,
                );
                self.operation_repo
                    .update_status(&OperationStatusUpdateInput {
                        operation_id: OperationId::new(operation_id.to_string()).map_err(|e| {
                            ChvError::Internal {
                                reason: format!("invalid operation_id: {e}"),
                            }
                        })?,
                        status: OperationStatus::Failed,
                        error_code: Some(UNSUPPORTED_BY_AGENT_ERROR_CODE.into()),
                        error_message: Some(reason.clone()),
                        observed_generation: None,
                        updated_by: Some("orchestrator".into()),
                        updated_unix_ms: now_unix_ms(),
                    })
                    .await
                    .map_err(|e2| ChvError::Internal {
                        reason: format!("network policy refused and status update failed: {e2}"),
                    })?;
                return Err(ChvError::Unimplemented { reason });
            }
            return Err(ChvError::Internal {
                reason: format!(
                    "network policy update for {network_id} failed on {} node(s): {}",
                    failures.len(),
                    failures.join("; ")
                ),
            });
        }

        let nodes: Vec<&str> = target_nodes.iter().map(String::as_str).collect();
        info!(
            operation_id = operation_id,
            network_id = network_id,
            nodes = %nodes.join(","),
            "network policy dispatched to all nodes with attached VMs"
        );
        self.mark_network_policy_succeeded(operation_id).await
    }

    /// The success terminal write shared by the #355 network policy
    /// dispatch paths (`dispatch_update_network_policy`). Carries no
    /// `error_message` by design — the #502 convention: a successful
    /// terminal write clears the error fields, so a mid-retry failure
    /// message never survives a success. The no-op REASON (empty
    /// ruleset, no attached VMs, missing/Deleting NDS row) is
    /// info-logged at each call site with the operation id.
    async fn mark_network_policy_succeeded(&self, operation_id: &str) -> Result<(), ChvError> {
        self.operation_repo
            .update_status(&OperationStatusUpdateInput {
                operation_id: OperationId::new(operation_id.to_string()).map_err(|e| {
                    ChvError::Internal {
                        reason: format!("invalid operation_id: {e}"),
                    }
                })?,
                status: OperationStatus::Succeeded,
                error_code: None,
                error_message: None,
                observed_generation: None,
                updated_by: Some("orchestrator".into()),
                updated_unix_ms: now_unix_ms(),
            })
            .await
            .map_err(|e| ChvError::Internal {
                reason: format!("failed to mark UpdateNetworkPolicy operation terminal: {e}"),
            })?;
        Ok(())
    }

    fn requires_schedulable_node(operation_type: &str) -> bool {
        matches!(
            operation_type,
            // #513 PR 1: a standalone volume create places new storage
            // on the node — the same placement discipline as a VM
            // create (the design's DP3 rationale: silent storage
            // placement is worse than silent VM placement).
            "create" | "CreateVm" | "CreateVolume" | "MigrateVm" | "ResizeVm"
        )
    }

    async fn require_node_schedulable(&self, node_id: &str) -> Result<(), ChvError> {
        let observed_state: Option<String> =
            sqlx::query_scalar("SELECT observed_state FROM node_observed_state WHERE node_id = ?")
                .bind(node_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| ChvError::Internal {
                    reason: format!("failed to check node state for {}: {e}", node_id),
                })?;

        let scheduling_paused: Option<bool> = sqlx::query_scalar(
            "SELECT scheduling_paused FROM node_desired_state WHERE node_id = ?",
        )
        .bind(node_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| ChvError::Internal {
            reason: format!("failed to check scheduling_paused for {}: {e}", node_id),
        })?;

        if scheduling_paused.unwrap_or(false) {
            return Err(ChvError::InvalidArgument {
                field: "node_id".to_string(),
                reason: format!("node {} has scheduling paused", node_id),
            });
        }

        match observed_state.as_deref() {
            Some("TenantReady") => Ok(()),
            Some(state) => Err(ChvError::InvalidArgument {
                field: "node_id".to_string(),
                reason: format!(
                    "node {} is in state '{}', must be TenantReady for placement",
                    node_id, state
                ),
            }),
            None => Err(ChvError::InvalidArgument {
                field: "node_id".to_string(),
                reason: format!(
                    "node {} has no observed state, cannot accept placements",
                    node_id
                ),
            }),
        }
    }

    /// Build the agent-compatible VmSpec JSON from control-plane DB records.
    pub(crate) async fn build_agent_vm_spec(&self, vm_id: &str) -> Result<String, ChvError> {
        let vm_row = sqlx::query_as::<_, VmDesiredStateRow>(
            r#"
            SELECT
                v.display_name,
                vds.cpu_count,
                vds.memory_bytes,
                vds.image_ref,
                vds.desired_power_state,
                vds.cloud_init_userdata,
                vds.vsock_enabled,
                v.hv_cpu_nested,
                v.hv_cpu_amx,
                v.hv_cpu_kvm_hyperv,
                v.hv_memory_mergeable,
                v.hv_memory_hugepages,
                v.hv_memory_shared,
                v.hv_memory_prefault,
                v.hv_iommu,
                v.hv_rng_src,
                v.hv_watchdog,
                v.hv_landlock_enable,
                v.hv_serial_mode,
                v.hv_console_mode,
                v.hv_pvpanic,
                v.hv_tpm_type,
                v.hv_tpm_socket_path
            FROM vms v
            JOIN vm_desired_state vds ON v.vm_id = vds.vm_id
            WHERE v.vm_id = ?
            "#,
        )
        .bind(vm_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| ChvError::Internal {
            reason: format!("failed to query vm desired state: {e}"),
        })?
        .ok_or_else(|| ChvError::NotFound {
            resource: "vm_desired_state".to_string(),
            id: vm_id.to_string(),
        })?;

        let global = HypervisorSettingsRepository::new(self.pool.clone())
            .get_settings()
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(vm_id = %vm_id, error = %e, "failed to fetch hypervisor_settings, using defaults");
                HypervisorSettingsRow {
                    id: 1,
                    cpu_nested: chv_common::hypervisor::DEFAULT_CPU_NESTED,
                    cpu_amx: chv_common::hypervisor::DEFAULT_CPU_AMX,
                    cpu_kvm_hyperv: chv_common::hypervisor::DEFAULT_CPU_KVM_HYPERV,
                    memory_mergeable: chv_common::hypervisor::DEFAULT_MEMORY_MERGEABLE,
                    memory_hugepages: chv_common::hypervisor::DEFAULT_MEMORY_HUGEPAGES,
                    memory_shared: chv_common::hypervisor::DEFAULT_MEMORY_SHARED,
                    memory_prefault: chv_common::hypervisor::DEFAULT_MEMORY_PREFAULT,
                    iommu: chv_common::hypervisor::DEFAULT_IOMMU,
                    rng_src: chv_common::hypervisor::DEFAULT_RNG_SRC.to_string(),
                    watchdog: chv_common::hypervisor::DEFAULT_WATCHDOG,
                    landlock_enable: chv_common::hypervisor::DEFAULT_LANDLOCK_ENABLE,
                    serial_mode: chv_common::hypervisor::DEFAULT_SERIAL_MODE.to_string(),
                    console_mode: chv_common::hypervisor::DEFAULT_CONSOLE_MODE.to_string(),
                    pvpanic: chv_common::hypervisor::DEFAULT_PVPANIC,
                    tpm_type: chv_common::hypervisor::DEFAULT_TPM_TYPE.map(|s| s.to_string()),
                    tpm_socket_path: chv_common::hypervisor::DEFAULT_TPM_SOCKET_PATH.map(|s| s.to_string()),
                    profile_id: None,
                    updated_at: String::new(),
                }
            });

        let volume_rows = sqlx::query_as::<_, VolumeDesiredStateRow>(
            r#"
            SELECT
                vds.volume_id,
                vds.read_only,
                v.capacity_bytes,
                v.storage_class
            FROM volume_desired_state vds
            JOIN volumes v ON v.volume_id = vds.volume_id
            WHERE vds.attached_vm_id = ?
            ORDER BY vds.volume_id
            "#,
        )
        .bind(vm_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ChvError::Internal {
            reason: format!("failed to query volume desired state: {e}"),
        })?;

        let nic_rows = sqlx::query_as::<_, VmNicRow>(
            r#"
            SELECT
                network_id,
                mac_address,
                ip_address
            FROM vm_nic_desired_state
            WHERE vm_id = ?
            ORDER BY nic_id
            "#,
        )
        .bind(vm_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ChvError::Internal {
            reason: format!("failed to query vm nic desired state: {e}"),
        })?;

        let kernel_path = if let Some(ref image_ref) = vm_row.image_ref {
            self.resolve_kernel_path(image_ref)?
        } else {
            self.kernel_path.clone()
        };

        let disks: Vec<AgentDiskSpec> =
            volume_rows
                .into_iter()
                .map(|v| AgentDiskSpec {
                    volume_id: v.volume_id,
                    read_only: v.read_only.unwrap_or(false),
                    size_bytes: v.capacity_bytes.and_then(|b| {
                        if b > 0 {
                            Some(b as u64)
                        } else {
                            None
                        }
                    }),
                    // #379 PR 2 (A5): the volume's stord backend class
                    // leaves the store — NULL stays absent on the wire
                    // (the agent's "local" default, PR 1's A6 seam);
                    // "local" is never materialized into the spec.
                    backend_class: v.storage_class,
                })
                .collect();

        let mut network_configs: std::collections::HashMap<
            String,
            (String, String, Option<String>),
        > = std::collections::HashMap::new();
        let unique_network_ids: Vec<&str> = nic_rows
            .iter()
            .map(|n| n.network_id.as_str())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        if !unique_network_ids.is_empty() {
            let placeholders = unique_network_ids
                .iter()
                .map(|_| "?")
                .collect::<Vec<_>>()
                .join(",");
            let query_str = format!(
                "SELECT network_id, cidr, gateway, firewall_rules_json FROM network_desired_state WHERE network_id IN ({})",
                placeholders
            );
            let mut query = sqlx::query_as::<_, NetworkDesiredStateWithIdRow>(&query_str);
            for id in &unique_network_ids {
                query = query.bind(id);
            }
            let net_rows = query
                .fetch_all(&self.pool)
                .await
                .map_err(|e| ChvError::Internal {
                    reason: format!("failed to query network desired states: {e}"),
                })?;
            for nr in net_rows {
                network_configs.insert(
                    nr.network_id,
                    (
                        nr.cidr.unwrap_or_default(),
                        nr.gateway.unwrap_or_default(),
                        // #355 DP4: the snapshot rides the spec AS-IS —
                        // an empty ruleset (`[]` = no user rules) is no
                        // longer filtered out here; the Core executor's
                        // attach path resolves it (and a never-set
                        // snapshot) to the shared BASELINE +
                        // default-deny, so a rule-less network gets a
                        // filtered boundary at materialization instead
                        // of the pre-DP4 unfiltered skip.
                        nr.firewall_rules_json,
                    ),
                );
            }
        }

        let nics: Vec<AgentNicSpec> = nic_rows
            .into_iter()
            .map(|n| {
                let (cidr, gateway, firewall_policy_json) = network_configs
                    .get(&n.network_id)
                    .cloned()
                    .unwrap_or_default();
                AgentNicSpec {
                    network_id: n.network_id,
                    mac_address: n.mac_address.unwrap_or_default(),
                    ip_address: n.ip_address.unwrap_or_default(),
                    cidr,
                    gateway,
                    firewall_policy_json,
                }
            })
            .collect();

        let desired_state = vm_row
            .desired_power_state
            .unwrap_or_else(|| "Running".to_string());

        let overrides = HypervisorOverrides {
            cpu_nested: Some(vm_row.hv_cpu_nested.unwrap_or(global.cpu_nested)),
            cpu_amx: Some(vm_row.hv_cpu_amx.unwrap_or(global.cpu_amx)),
            cpu_kvm_hyperv: Some(vm_row.hv_cpu_kvm_hyperv.unwrap_or(global.cpu_kvm_hyperv)),
            memory_mergeable: Some(
                vm_row
                    .hv_memory_mergeable
                    .unwrap_or(global.memory_mergeable),
            ),
            memory_hugepages: Some(
                vm_row
                    .hv_memory_hugepages
                    .unwrap_or(global.memory_hugepages),
            ),
            memory_shared: Some(vm_row.hv_memory_shared.unwrap_or(global.memory_shared)),
            memory_prefault: Some(vm_row.hv_memory_prefault.unwrap_or(global.memory_prefault)),
            iommu: Some(vm_row.hv_iommu.unwrap_or(global.iommu)),
            rng_src: Some(vm_row.hv_rng_src.unwrap_or_else(|| global.rng_src.clone())),
            watchdog: Some(vm_row.hv_watchdog.unwrap_or(global.watchdog)),
            landlock_enable: Some(vm_row.hv_landlock_enable.unwrap_or(global.landlock_enable)),
            serial_mode: Some(
                vm_row
                    .hv_serial_mode
                    .unwrap_or_else(|| global.serial_mode.clone()),
            ),
            console_mode: Some(
                vm_row
                    .hv_console_mode
                    .unwrap_or_else(|| global.console_mode.clone()),
            ),
            pvpanic: Some(vm_row.hv_pvpanic.unwrap_or(global.pvpanic)),
            tpm_type: vm_row
                .hv_tpm_type
                .clone()
                .or_else(|| global.tpm_type.clone())
                .or_else(|| chv_common::hypervisor::DEFAULT_TPM_TYPE.map(|s| s.to_string())),
            tpm_socket_path: vm_row
                .hv_tpm_socket_path
                .clone()
                .or_else(|| global.tpm_socket_path.clone())
                .or_else(|| chv_common::hypervisor::DEFAULT_TPM_SOCKET_PATH.map(|s| s.to_string())),
        };

        if let Err(e) = validate_merged_overrides(&overrides) {
            return Err(ChvError::InvalidArgument {
                field: "hypervisor_overrides".to_string(),
                reason: e,
            });
        }

        // Validate disk seed image path exists before dispatching to agent.
        // NOTE: This validation only works in all-in-one deployments where controlplane
        // and agent share a filesystem. In multi-node setups, the agent-side reconciler
        // handles missing images via backoff retry.
        let disk_seed_path = self.resolve_disk_seed_path(vm_row.image_ref.as_deref());
        let disk_seed_path = match disk_seed_path {
            Some(Err(error)) => return Err(error),
            Some(Ok(path)) => Some(path),
            None => None,
        };
        if let Some(ref seed_path) = disk_seed_path {
            let path = std::path::Path::new(seed_path);
            if !path.exists() {
                return Err(ChvError::InvalidArgument {
                    field: "image_ref".to_string(),
                    reason: format!(
                        "image file not found at resolved path: {}. Import the image first.",
                        seed_path
                    ),
                });
            }
        }

        let spec = AgentVmSpec {
            name: vm_row.display_name.unwrap_or_else(|| vm_id.to_string()),
            cpus: vm_row.cpu_count.unwrap_or(1) as u32,
            memory_bytes: vm_row.memory_bytes.unwrap_or(512 * 1024 * 1024) as u64,
            kernel_path,
            firmware_path: Some(self.firmware_path.clone()),
            disk_seed_path,
            disks,
            nics,
            desired_state,
            cloud_init_userdata: vm_row.cloud_init_userdata,
            hypervisor_overrides: Some(overrides),
            // G5 vsock opt-in: emitted ONLY when the VM was created with
            // it — every other VM's spec JSON stays byte-shaped like a
            // pre-G5 one (the agent's serde-tolerant parser treats
            // absence as the opt-out). The spec carries the marker only;
            // the node authority allocates the CID and socket path.
            vsock: vm_row
                .vsock_enabled
                .unwrap_or(false)
                .then_some(AgentVsockSpec { enabled: true }),
        };

        serde_json::to_string(&spec).map_err(|e| ChvError::Internal {
            reason: format!("failed to serialize agent vm spec: {e}"),
        })
    }

    fn resolve_kernel_path(&self, image_ref: &str) -> Result<String, ChvError> {
        // For the first VM milestone, use a simple config-based mapping.
        // In production this would query an image registry.
        // If image_ref looks like a disk image path (absolute path or file:// URI),
        // use the default kernel path instead.
        if image_ref == "default"
            || image_ref.is_empty()
            || image_ref.starts_with('/')
            || image_ref.starts_with("file://")
        {
            return Ok(self.kernel_path.clone());
        }
        // A relative image_ref becomes a single path component under the
        // kernels root: reject anything that could escape it (a ".."
        // component in image_ref is an API-client-supplied path traversal,
        // not an image name). `is_safe_path_component` (not the stricter
        // `is_safe_id`) so tag-style names accepted before this boundary
        // check existed — e.g. `image:latest` — keep working.
        if !chv_common::is_safe_path_component(image_ref) {
            return Err(ChvError::InvalidArgument {
                field: "image_ref".to_string(),
                reason: format!(
                    "'{image_ref}' is not a safe image name (must be a single path component)"
                ),
            });
        }
        Ok(format!("/var/lib/chv/kernels/{image_ref}"))
    }

    fn resolve_disk_seed_path(&self, image_ref: Option<&str>) -> Option<Result<String, ChvError>> {
        let image_ref = image_ref?.trim();
        if image_ref.is_empty() || image_ref == "default" {
            return None;
        }
        // Absolute and file:// paths are the operator escape hatch and pass
        // through verbatim; the node-side stord allowlist constrains what
        // the agent will actually open. A RELATIVE image_ref is joined under
        // the images root and must be a single safe component.
        if let Some(path) = image_ref.strip_prefix("file://") {
            return Some(Ok(path.to_string()));
        }
        if image_ref.starts_with('/') {
            return Some(Ok(image_ref.to_string()));
        }
        if !chv_common::is_safe_path_component(image_ref) {
            return Some(Err(ChvError::InvalidArgument {
                field: "image_ref".to_string(),
                reason: format!(
                    "'{image_ref}' is not a safe image name (must be a single path component)"
                ),
            }));
        }
        Some(Ok(format!("/var/lib/chv/images/{image_ref}")))
    }
}

fn validate_merged_overrides(overrides: &HypervisorOverrides) -> Result<(), String> {
    if overrides.iommu == Some(true) && overrides.memory_shared != Some(true) {
        return Err("iommu=true requires memory_shared=true".to_string());
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
struct AcceptedOperationRow {
    operation_id: String,
    operation_type: String,
    #[allow(dead_code)]
    resource_kind: String,
    resource_id: String,
    desired_generation: Option<i64>,
    node_id: Option<String>,
    correlation_id: Option<String>,
    /// #379 PR 2 (A8): the volume's storage class, resolved in the same
    /// claim statement for Volume-kind rows (NULL for other kinds and for
    /// class-less volumes) so the AttachVolume dispatch can carry it in
    /// `volume_spec_json` without a follow-up query.
    volume_storage_class: Option<String>,
    /// #513 PR 1 (DP2): the volume's capacity, resolved in the same
    /// claim statement for Volume-kind rows (NULL when the resource is
    /// not a volume row) so the CreateVolume dispatch can carry the
    /// provisioning size in `volume_spec_json` without a follow-up
    /// query — the same single-round-trip discipline as the class.
    volume_capacity_bytes: Option<i64>,
    /// #533: the volume's kind, resolved in the same claim statement
    /// for Volume-kind rows (NULL for other kinds and for every
    /// embedded/boot/pre-#513 volume) so the AttachVolume dispatch can
    /// shape a standalone (`volume_kind = 'data'`) volume's open
    /// locator as the #513 create carrier's — the same
    /// single-round-trip discipline as the class and capacity.
    volume_kind: Option<String>,
}

/// #368 P2 selection: one VM whose desired state still demands it, whose
/// agent-reported state says the create terminally failed, and that has
/// no incomplete create-family operation in flight.
#[derive(sqlx::FromRow)]
struct FailedCreateRedriveRow {
    vm_id: String,
    target_node_id: String,
    /// The generation the ORIGINAL create operation dispatched at (the
    /// re-drive must re-issue the same generation-1 task shape).
    create_generation: Option<i64>,
    /// How many `RecreateVm` operations this VM has already been issued.
    redrive_count: i64,
    /// `updated_at` of the most recent re-drive (backoff anchor).
    last_redrive_at: Option<String>,
    /// The agent-reported Core failure code (#368 P1).
    reported_error: Option<String>,
}

#[derive(sqlx::FromRow)]
struct VmDesiredStateRow {
    display_name: Option<String>,
    cpu_count: Option<i32>,
    memory_bytes: Option<i64>,
    image_ref: Option<String>,
    desired_power_state: Option<String>,
    cloud_init_userdata: Option<String>,
    /// G5 vsock device opt-in (NOT NULL, 0 = off, from 0062).
    vsock_enabled: Option<bool>,
    hv_cpu_nested: Option<bool>,
    hv_cpu_amx: Option<bool>,
    hv_cpu_kvm_hyperv: Option<bool>,
    hv_memory_mergeable: Option<bool>,
    hv_memory_hugepages: Option<bool>,
    hv_memory_shared: Option<bool>,
    hv_memory_prefault: Option<bool>,
    hv_iommu: Option<bool>,
    hv_rng_src: Option<String>,
    hv_watchdog: Option<bool>,
    hv_landlock_enable: Option<bool>,
    hv_serial_mode: Option<String>,
    hv_console_mode: Option<String>,
    hv_pvpanic: Option<bool>,
    hv_tpm_type: Option<String>,
    hv_tpm_socket_path: Option<String>,
}

#[derive(sqlx::FromRow)]
struct VolumeDesiredStateRow {
    volume_id: String,
    read_only: Option<bool>,
    capacity_bytes: Option<i64>,
    /// #379 PR 2 (A5): the volume's storage class (NULL = local).
    storage_class: Option<String>,
}

#[derive(sqlx::FromRow)]
struct VmNicRow {
    network_id: String,
    mac_address: Option<String>,
    ip_address: Option<String>,
}

#[derive(sqlx::FromRow)]
struct NetworkDesiredStateWithIdRow {
    network_id: String,
    cidr: Option<String>,
    gateway: Option<String>,
    firewall_rules_json: Option<String>,
}

#[derive(serde::Serialize)]
struct AgentVmSpec {
    name: String,
    cpus: u32,
    memory_bytes: u64,
    kernel_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    firmware_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disk_seed_path: Option<String>,
    disks: Vec<AgentDiskSpec>,
    nics: Vec<AgentNicSpec>,
    desired_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cloud_init_userdata: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hypervisor_overrides: Option<HypervisorOverrides>,
    /// G5 vsock device opt-in — serialized under the key the agent's
    /// `VmSpec` parser reads (`vsock`, PR-7's additive field) and
    /// OMITTED when the VM did not opt in.
    #[serde(skip_serializing_if = "Option::is_none")]
    vsock: Option<AgentVsockSpec>,
}

/// The wire mirror of the agent-side `VsockSpec` opt-in marker.
#[derive(serde::Serialize)]
struct AgentVsockSpec {
    enabled: bool,
}

#[derive(serde::Serialize)]
struct AgentDiskSpec {
    volume_id: String,
    read_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_bytes: Option<u64>,
    /// #379 PR 2 (A5): the volume's stord backend class, from
    /// `volumes.storage_class` — serialized under the key the agent's
    /// `DiskSpec` parser reads (`backend_class`, PR 1's A6 field) and
    /// OMITTED when NULL so the agent's `"local"` default applies
    /// (NULL = local end-to-end; the string is never materialized).
    #[serde(skip_serializing_if = "Option::is_none")]
    backend_class: Option<String>,
}

#[derive(serde::Serialize)]
struct AgentNicSpec {
    network_id: String,
    mac_address: String,
    ip_address: String,
    cidr: String,
    gateway: String,
    /// The network's firewall_rules_json snapshot at dispatch time (#355):
    /// the Core executor applies it at attach time (default-deny + the
    /// operator's rules) so a policy actually materializes on the deployed
    /// path. Only included when the operator stored rules — an empty
    /// ruleset would engage default-deny with no allows and cut the
    /// network's guests off entirely.
    firewall_policy_json: Option<String>,
}

fn now_unix_ms() -> i64 {
    chv_common::now_unix_ms()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chv_controlplane_store::test_util::create_test_pool;
    use control_plane_node_api::control_plane_node_api as proto;

    /// SQL used by `tick()` to claim Accepted operations and resolve `node_id`
    /// in a single round trip via correlated subqueries in RETURNING.
    /// Kept in sync with the production query in `tick()`.
    const CLAIM_ACCEPTED_SQL: &str = r#"
        UPDATE operations SET status = 'Running', updated_by = 'orchestrator'
        WHERE operation_id IN (
            SELECT o.operation_id
            FROM operations o
            WHERE o.status = 'Accepted'
            ORDER BY o.requested_at ASC
            LIMIT 10
        )
        RETURNING
            operation_id,
            operation_type,
            resource_kind,
            resource_id,
            desired_generation,
            correlation_id,
            COALESCE(
                (SELECT target_node_id FROM vm_desired_state WHERE vm_id = operations.resource_id),
                (SELECT node_id FROM volumes WHERE volume_id = operations.resource_id),
                (SELECT node_id FROM networks WHERE network_id = operations.resource_id)
            ) AS node_id,
            (SELECT storage_class FROM volumes WHERE volume_id = operations.resource_id) AS volume_storage_class,
            (SELECT capacity_bytes FROM volumes WHERE volume_id = operations.resource_id) AS volume_capacity_bytes,
            (SELECT volume_kind FROM volumes WHERE volume_id = operations.resource_id) AS volume_kind
    "#;

    async fn seed_node(pool: &StorePool, node_id: &str) {
        sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES (?, ?, ?)")
            .bind(node_id)
            .bind(format!("host-{node_id}"))
            .bind(format!("Node {node_id}"))
            .execute(pool)
            .await
            .expect("insert node");
    }

    /// #513 PR 1: the CreateVolume dispatch arm joins the placement
    /// discipline (`requires_schedulable_node`), so its tests seed the
    /// TenantReady observed state the VM-create paths' nodes carry.
    async fn seed_node_tenant_ready(pool: &StorePool, node_id: &str) {
        sqlx::query(
            "INSERT INTO node_observed_state (node_id, observed_generation, observed_state) \
             VALUES (?, 1, 'TenantReady')",
        )
        .bind(node_id)
        .execute(pool)
        .await
        .expect("insert node observed state");
    }

    async fn seed_vm(pool: &StorePool, vm_id: &str, target_node_id: &str) {
        sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES (?, ?)")
            .bind(vm_id)
            .bind(format!("VM {vm_id}"))
            .execute(pool)
            .await
            .expect("insert vm");
        sqlx::query(
            "INSERT INTO vm_desired_state (vm_id, desired_generation, target_node_id) \
             VALUES (?, 1, ?)",
        )
        .bind(vm_id)
        .bind(target_node_id)
        .execute(pool)
        .await
        .expect("insert vm_desired_state");
    }

    async fn seed_volume(pool: &StorePool, volume_id: &str, node_id: &str) {
        sqlx::query(
            "INSERT INTO volumes (volume_id, node_id, display_name, capacity_bytes) \
             VALUES (?, ?, ?, 1024)",
        )
        .bind(volume_id)
        .bind(node_id)
        .bind(format!("Vol {volume_id}"))
        .execute(pool)
        .await
        .expect("insert volume");
    }

    async fn seed_network(pool: &StorePool, network_id: &str, node_id: &str) {
        sqlx::query("INSERT INTO networks (network_id, node_id, display_name) VALUES (?, ?, ?)")
            .bind(network_id)
            .bind(node_id)
            .bind(format!("Net {network_id}"))
            .execute(pool)
            .await
            .expect("insert network");
    }

    async fn seed_accepted_op(
        pool: &StorePool,
        operation_id: &str,
        resource_kind: &str,
        resource_id: &str,
        operation_type: &str,
        requested_at: &str,
    ) {
        sqlx::query(
            "INSERT INTO operations \
             (operation_id, idempotency_key, resource_kind, resource_id, operation_type, status, \
              desired_generation, requested_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, 'Accepted', 1, ?, ?)",
        )
        .bind(operation_id)
        .bind(format!("idem-{operation_id}"))
        .bind(resource_kind)
        .bind(resource_id)
        .bind(operation_type)
        .bind(requested_at)
        .bind(requested_at)
        .execute(pool)
        .await
        .expect("insert operation");
    }

    /// #355 DP4 (PR 3): the VM spec fragment carries the network's
    /// firewall policy snapshot AS-IS — an empty ruleset (`[]` = no
    /// user rules) rides the spec too, and the Core executor's attach
    /// path resolves it (and a never-set snapshot) to the shared
    /// BASELINE (DHCP/DNS/conntrack allows + default-deny). The
    /// pre-DP4 filter here is what left rule-less networks
    /// unfiltered.
    #[tokio::test]
    async fn build_agent_vm_spec_carries_network_firewall_policy_snapshot() {
        let pool = create_test_pool().await;
        seed_node(&pool, "node-a").await;
        seed_vm(&pool, "vm-spec", "node-a").await;
        let policy = r#"[{"direction":"inbound","action":"accept","protocol":"icmp"}]"#;
        for (idx, (network_id, rules)) in [("net-with-policy", policy), ("net-bare", "[]")]
            .into_iter()
            .enumerate()
        {
            seed_network(&pool, network_id, "node-a").await;
            sqlx::query(
                "INSERT INTO network_desired_state \
                 (network_id, desired_generation, desired_status, cidr, gateway, dhcp_enabled, \
                  ipam_mode, is_default, firewall_rules_json) \
                 VALUES (?, 1, 'Pending', '10.200.0.0/24', '10.200.0.1', 1, 'internal', 0, ?)",
            )
            .bind(network_id)
            .bind(rules)
            .execute(&pool)
            .await
            .expect("seed network desired state");
            sqlx::query(
                "INSERT INTO vm_nic_desired_state (nic_id, vm_id, network_id, mac_address) \
                 VALUES (?, 'vm-spec', ?, ?)",
            )
            .bind(format!("nic-{network_id}"))
            .bind(network_id)
            .bind(format!("02:00:00:00:00:{idx:02x}"))
            .execute(&pool)
            .await
            .expect("seed vm nic");
        }

        let orchestrator = Orchestrator::new(
            pool.clone(),
            OperationRepository::new(pool.clone()),
            String::new(),
            "/kernel".to_string(),
            String::new(),
            NodeClientPool::new(),
            crate::convergence_metrics::new_shared(),
        );
        let spec_json = orchestrator
            .build_agent_vm_spec("vm-spec")
            .await
            .expect("build agent vm spec");
        let spec: serde_json::Value = serde_json::from_str(&spec_json).expect("spec json");
        let nics = spec["nics"].as_array().expect("nics array");
        assert_eq!(nics.len(), 2, "both nics must ride the spec: {spec}");

        let by_network = |id: &str| {
            nics.iter()
                .find(|n| n["network_id"] == id)
                .unwrap_or_else(|| panic!("nic for {id} missing: {spec}"))
                .clone()
        };
        assert_eq!(
            by_network("net-with-policy")["firewall_policy_json"].as_str(),
            Some(policy),
            "a non-empty policy snapshot must ride the spec"
        );
        assert_eq!(
            by_network("net-bare")["firewall_policy_json"].as_str(),
            Some("[]"),
            "an empty ruleset rides the spec AS-IS (#355 DP4): the Core executor's \
             attach path resolves it to the shared baseline + default-deny — the \
             pre-DP4 filter here is what left rule-less networks unfiltered"
        );
    }

    /// G5 (prompt-06 task 2): the vsock opt-in rides the agent spec ONLY
    /// when the VM was created with it, and carries ONLY the marker —
    /// the CID and host socket path are node-authority runtime state,
    /// never control-plane fields. A VM that did not opt in produces a
    /// spec byte-shaped like a pre-G5 one (no vsock key at all).
    #[tokio::test]
    async fn build_agent_vm_spec_emits_the_vsock_opt_in_only_when_enabled() {
        let pool = create_test_pool().await;
        seed_node(&pool, "node-a").await;
        seed_vm(&pool, "vm-vsock-on", "node-a").await;
        seed_vm(&pool, "vm-vsock-off", "node-a").await;
        sqlx::query("UPDATE vm_desired_state SET vsock_enabled = 1 WHERE vm_id = 'vm-vsock-on'")
            .execute(&pool)
            .await
            .expect("opt the vm in");

        let orchestrator = Orchestrator::new(
            pool.clone(),
            OperationRepository::new(pool.clone()),
            String::new(),
            "/kernel".to_string(),
            String::new(),
            NodeClientPool::new(),
            crate::convergence_metrics::new_shared(),
        );

        let spec_json = orchestrator
            .build_agent_vm_spec("vm-vsock-on")
            .await
            .expect("build agent vm spec (opted in)");
        let spec: serde_json::Value = serde_json::from_str(&spec_json).expect("spec json");
        assert_eq!(
            spec["vsock"]["enabled"],
            serde_json::json!(true),
            "the opt-in must ride the spec: {spec}"
        );
        assert_eq!(
            spec["vsock"].as_object().expect("vsock object").len(),
            1,
            "the spec carries ONLY the marker — no cid/socket (node state): {spec}"
        );

        let spec_json = orchestrator
            .build_agent_vm_spec("vm-vsock-off")
            .await
            .expect("build agent vm spec (opted out)");
        let spec: serde_json::Value = serde_json::from_str(&spec_json).expect("spec json");
        assert!(
            spec.get("vsock").is_none(),
            "no opt-in, no key — the spec stays byte-shaped like a pre-G5 one: {spec}"
        );
    }

    /// End-to-end check that the production claim SQL atomically:
    ///   1. transitions Accepted -> Running, and
    ///   2. resolves `node_id` for VM, volume, and network operations,
    /// all within a single statement (no follow-up SELECTs needed).
    #[tokio::test]
    async fn claim_returning_resolves_node_id_for_mixed_resources() {
        let pool = create_test_pool().await;

        seed_node(&pool, "node-a").await;
        seed_node(&pool, "node-b").await;
        seed_node(&pool, "node-c").await;

        seed_vm(&pool, "vm-1", "node-a").await;
        seed_volume(&pool, "vol-1", "node-b").await;
        seed_network(&pool, "net-1", "node-c").await;

        // requested_at varies so we can assert ORDER BY requested_at ASC ordering survives.
        seed_accepted_op(
            &pool,
            "op-vm",
            "Vm",
            "vm-1",
            "StartVm",
            "2026-01-01T00:00:01Z",
        )
        .await;
        seed_accepted_op(
            &pool,
            "op-vol",
            "Volume",
            "vol-1",
            "AttachVolume",
            "2026-01-01T00:00:02Z",
        )
        .await;
        seed_accepted_op(
            &pool,
            "op-net",
            "Network",
            "net-1",
            "StartNetwork",
            "2026-01-01T00:00:03Z",
        )
        .await;
        // Operation whose resource_id matches no resource → node_id must be NULL.
        seed_accepted_op(
            &pool,
            "op-orphan",
            "Vm",
            "ghost",
            "StartVm",
            "2026-01-01T00:00:04Z",
        )
        .await;

        let rows = sqlx::query_as::<_, AcceptedOperationRow>(CLAIM_ACCEPTED_SQL)
            .fetch_all(&pool)
            .await
            .expect("claim query must succeed: SQLite supports correlated subqueries in RETURNING");

        // All four Accepted ops should be claimed in one statement.
        assert_eq!(rows.len(), 4, "expected all Accepted ops claimed");

        // RETURNING does not guarantee order, so look up by operation_id.
        let by_id: std::collections::HashMap<&str, &AcceptedOperationRow> =
            rows.iter().map(|r| (r.operation_id.as_str(), r)).collect();

        assert_eq!(
            by_id["op-vm"].node_id.as_deref(),
            Some("node-a"),
            "VM op should resolve node_id from vm_desired_state.target_node_id"
        );
        assert_eq!(
            by_id["op-vol"].node_id.as_deref(),
            Some("node-b"),
            "Volume op should resolve node_id from volumes.node_id"
        );
        assert_eq!(
            by_id["op-net"].node_id.as_deref(),
            Some("node-c"),
            "Network op should resolve node_id from networks.node_id"
        );
        assert!(
            by_id["op-orphan"].node_id.is_none(),
            "orphan op (no matching resource) must yield NULL node_id"
        );

        // Confirm the UPDATE actually transitioned the rows to Running.
        let still_accepted: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM operations WHERE status = 'Accepted'")
                .fetch_one(&pool)
                .await
                .expect("count");
        assert_eq!(still_accepted, 0, "all Accepted ops should now be Running");

        let now_running: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM operations WHERE status = 'Running'")
                .fetch_one(&pool)
                .await
                .expect("count");
        assert_eq!(now_running, 4);
    }

    /// Subsequent ticks must not re-claim rows already moved to Running.
    /// This guards against accidental loosening of the WHERE-IN subquery.
    #[tokio::test]
    async fn claim_does_not_reclaim_running_rows() {
        let pool = create_test_pool().await;
        seed_node(&pool, "node-a").await;
        seed_vm(&pool, "vm-1", "node-a").await;
        seed_accepted_op(
            &pool,
            "op-1",
            "Vm",
            "vm-1",
            "StartVm",
            "2026-01-01T00:00:00Z",
        )
        .await;

        let first = sqlx::query_as::<_, AcceptedOperationRow>(CLAIM_ACCEPTED_SQL)
            .fetch_all(&pool)
            .await
            .expect("first claim");
        assert_eq!(first.len(), 1);

        let second = sqlx::query_as::<_, AcceptedOperationRow>(CLAIM_ACCEPTED_SQL)
            .fetch_all(&pool)
            .await
            .expect("second claim");
        assert!(
            second.is_empty(),
            "second tick must not reclaim already-Running rows"
        );
    }

    // ============================================================
    // #379 PR 2 — CP class carry (A5 spec, A8 attach dispatch)
    // ============================================================

    /// #379 PR 2 (A5): `build_agent_vm_spec` embeds each volume's
    /// storage class under the key the agent's `DiskSpec` parser reads —
    /// a NULL class OMITS the key (the agent's "local" default applies;
    /// the string is never materialized), a set class rides verbatim.
    #[tokio::test]
    async fn build_agent_vm_spec_embeds_volume_backend_class() {
        let pool = create_test_pool().await;
        seed_node(&pool, "node-a").await;
        seed_vm(&pool, "vm-spec-cls", "node-a").await;
        for (volume_id, class) in [("vol-cls-lvm", Some("lvm")), ("vol-cls-null", None)] {
            sqlx::query(
                "INSERT INTO volumes (volume_id, node_id, display_name, capacity_bytes, storage_class) \
                 VALUES (?, 'node-a', ?, 1024, ?)",
            )
            .bind(volume_id)
            .bind(format!("Vol {volume_id}"))
            .bind(class)
            .execute(&pool)
            .await
            .expect("seed volume");
            sqlx::query(
                "INSERT INTO volume_desired_state \
                 (volume_id, desired_generation, desired_status, attached_vm_id, read_only) \
                 VALUES (?, 1, 'Pending', 'vm-spec-cls', 0)",
            )
            .bind(volume_id)
            .execute(&pool)
            .await
            .expect("seed volume desired state");
        }

        let orchestrator = Orchestrator::new(
            pool.clone(),
            OperationRepository::new(pool.clone()),
            String::new(),
            "/kernel".to_string(),
            String::new(),
            NodeClientPool::new(),
            crate::convergence_metrics::new_shared(),
        );
        let spec_json = orchestrator
            .build_agent_vm_spec("vm-spec-cls")
            .await
            .expect("build agent vm spec");
        let spec: serde_json::Value = serde_json::from_str(&spec_json).expect("spec json");
        let disks = spec["disks"].as_array().expect("disks array");
        assert_eq!(disks.len(), 2, "both disks must ride the spec: {spec}");

        let by_volume = |id: &str| {
            disks
                .iter()
                .find(|d| d["volume_id"] == id)
                .unwrap_or_else(|| panic!("disk for {id} missing: {spec}"))
                .clone()
        };
        assert_eq!(
            by_volume("vol-cls-lvm")["backend_class"].as_str(),
            Some("lvm"),
            "a set class must ride the spec verbatim: {spec}"
        );
        assert!(
            by_volume("vol-cls-null").get("backend_class").is_none(),
            "a NULL class must OMIT the key (never materialize 'local'): {spec}"
        );
    }

    /// #379 PR 2 (A5 wire hardening): byte-level pin — a classless
    /// `AgentDiskSpec` serializes to EXACTLY the pre-PR2 bytes (the
    /// new field is `skip_serializing_if`-omitted), so an old agent's
    /// serde-tolerant `DiskSpec` receives an unchanged spec. The JSON
    /// assertions above check semantics; this pins the raw bytes.
    #[test]
    fn agent_disk_spec_classless_serialization_is_byte_identical_to_pre_pr2() {
        let spec = AgentDiskSpec {
            volume_id: "vol-1".to_string(),
            read_only: false,
            size_bytes: Some(1024),
            backend_class: None,
        };
        assert_eq!(
            serde_json::to_vec(&spec).unwrap(),
            br#"{"volume_id":"vol-1","read_only":false,"size_bytes":1024}"#.to_vec(),
            "a NULL class must not change the wire bytes an old agent receives"
        );
    }

    /// #379 PR 2 (A8): the claim query resolves the volume's storage
    /// class in the same statement — Some for a class-carrying volume,
    /// NULL for a class-less volume and for non-volume ops.
    #[tokio::test]
    async fn claim_returning_resolves_volume_storage_class() {
        let pool = create_test_pool().await;

        seed_node(&pool, "node-a").await;
        seed_node(&pool, "node-b").await;
        seed_vm(&pool, "vm-1", "node-a").await;
        seed_volume(&pool, "vol-bare", "node-b").await;
        sqlx::query(
            "INSERT INTO volumes (volume_id, node_id, display_name, capacity_bytes, storage_class) \
             VALUES ('vol-lvm', 'node-b', 'Vol vol-lvm', 1024, 'lvm')",
        )
        .execute(&pool)
        .await
        .expect("seed class-carrying volume");
        // #533: a standalone 'data' volume (the #513 DP8 stamp), so the
        // claim's kind resolution is pinned beside the class's.
        sqlx::query(
            "INSERT INTO volumes (volume_id, node_id, display_name, capacity_bytes, volume_kind) \
             VALUES ('vol-std', 'node-b', 'Vol vol-std', 1024, 'data')",
        )
        .execute(&pool)
        .await
        .expect("seed standalone volume");

        seed_accepted_op(
            &pool,
            "op-vol-lvm",
            "Volume",
            "vol-lvm",
            "AttachVolume",
            "2026-01-01T00:00:01Z",
        )
        .await;
        seed_accepted_op(
            &pool,
            "op-vol-bare",
            "Volume",
            "vol-bare",
            "AttachVolume",
            "2026-01-01T00:00:02Z",
        )
        .await;
        seed_accepted_op(
            &pool,
            "op-vol-std",
            "Volume",
            "vol-std",
            "AttachVolume",
            "2026-01-01T00:00:03Z",
        )
        .await;
        seed_accepted_op(
            &pool,
            "op-vm",
            "Vm",
            "vm-1",
            "StartVm",
            "2026-01-01T00:00:04Z",
        )
        .await;

        let rows = sqlx::query_as::<_, AcceptedOperationRow>(CLAIM_ACCEPTED_SQL)
            .fetch_all(&pool)
            .await
            .expect("claim query must succeed");

        let by_id: std::collections::HashMap<&str, &AcceptedOperationRow> =
            rows.iter().map(|r| (r.operation_id.as_str(), r)).collect();
        assert_eq!(
            by_id["op-vol-lvm"].volume_storage_class.as_deref(),
            Some("lvm"),
            "a class-carrying volume resolves its class in the claim"
        );
        assert_eq!(
            by_id["op-vol-bare"].volume_storage_class, None,
            "a class-less volume resolves NULL (the local default)"
        );
        assert_eq!(
            by_id["op-vm"].volume_storage_class, None,
            "a non-volume op resolves NULL"
        );
        // #533: the kind rides the same claim statement — Some("data")
        // only for the #513 standalone stamp, NULL for embedded
        // volumes and non-volume ops.
        assert_eq!(
            by_id["op-vol-std"].volume_kind.as_deref(),
            Some("data"),
            "a standalone volume resolves its kind in the claim (#533's locator discriminator)"
        );
        assert_eq!(
            by_id["op-vol-lvm"].volume_kind, None,
            "an embedded volume resolves a NULL kind"
        );
        assert_eq!(
            by_id["op-vm"].volume_kind, None,
            "a non-volume op resolves a NULL kind"
        );
    }

    /// #379 PR 2 (A8) + the clone carry: the AttachVolume dispatch
    /// populates `volume_spec_json` from the volume's class —
    /// `{"backend_class":"lvm"}` for a class-carrying volume, and `{}`
    /// (the empty JSON object) for a NULL-class volume. PR 2 pinned the
    /// NULL-class leg as byte-exact EMPTY bytes but disclosed that they
    /// fail the agent's A4 JSON parse (EOF on empty input); PR 3 (DP5)
    /// corrects the leg to `{}` so a NULL-class attach actually reaches
    /// the open — still no keys, still no materialized `"local"`. The
    /// class-carrying volume is a #501 CLONE TARGET (the clone copies
    /// `storage_class` from the source), pinning that a clone of a
    /// class-carrying volume dispatches with the class.
    #[tokio::test]
    async fn attach_volume_dispatch_carries_the_volume_class() {
        use crate::lifecycle::LifecycleService as _;
        use chv_controlplane_store::{DesiredStateRepository, EventRepository, NodeRepository};
        use chv_controlplane_types::domain::{Generation, NodeId, ResourceId};

        let pool = create_test_pool().await;
        seed_node(&pool, "node-att").await;

        // A class-carrying source volume, cloned through the real #501
        // transaction so the target row is exactly what production
        // materializes (storage_class copied from the source).
        let clone_service = crate::lifecycle::LifecycleServiceImplementation::new(
            NodeRepository::new(pool.clone()),
            OperationRepository::new(pool.clone()),
            EventRepository::new(pool.clone()),
            DesiredStateRepository::new(pool.clone()),
        );
        DesiredStateRepository::new(pool.clone())
            .upsert_volume(&chv_controlplane_store::VolumeDesiredStateInput {
                volume_id: ResourceId::new("vol-clone-src").unwrap(),
                node_id: Some(NodeId::new("node-att").unwrap()),
                display_name: "vol-clone-src".into(),
                capacity_bytes: 1024,
                volume_kind: Some("disk".into()),
                storage_class: Some("lvm".into()),
                owner_id: Some("user-att".into()),
                desired_generation: Generation::new(1),
                desired_status: None,
                requested_by: Some("test-user".into()),
                updated_by: None,
                attached_vm_id: None,
                attachment_mode: None,
                device_name: None,
                read_only: false,
                resize_to_bytes: None,
                snapshot_op: None,
                snapshot_name: None,
                clone_source_volume_id: None,
                requested_unix_ms: 1000,
            })
            .await
            .unwrap();
        let ack = clone_service
            .clone_volume(proto::CloneVolumeRequest {
                meta: Some(proto::RequestMeta {
                    operation_id: "op-clone-src".into(),
                    requested_by: "test-user".into(),
                    target_node_id: "node-att".into(),
                    desired_state_version: "1".into(),
                    request_unix_ms: 1000,
                }),
                node_id: "node-att".into(),
                source_volume_id: "vol-clone-src".into(),
                target_volume_id: "vol-clone-dst".into(),
            })
            .await
            .expect("clone must be accepted");
        assert_eq!(
            ack.result.expect("ack result").status,
            "OK",
            "clone must be accepted"
        );
        let cloned_class: Option<String> = sqlx::query_scalar(
            "SELECT storage_class FROM volumes WHERE volume_id = 'vol-clone-dst'",
        )
        .fetch_one(&pool)
        .await
        .expect("clone target row");
        assert_eq!(
            cloned_class.as_deref(),
            Some("lvm"),
            "the #501 clone must copy the source's class (#384/#501 pin, restated for #379)"
        );

        // A NULL-class volume for the empty-object spec_json leg (the
        // PR 3 `{}` correction of PR 2's empty-bytes pin).
        seed_volume(&pool, "vol-bare", "node-att").await;
        sqlx::query(
            "INSERT INTO volume_desired_state \
             (volume_id, desired_generation, desired_status, attached_vm_id, read_only) \
             VALUES ('vol-bare', 1, 'Pending', NULL, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed volume desired state");

        for (op_id, volume_id) in [("op-att-cls", "vol-clone-dst"), ("op-att-bare", "vol-bare")] {
            sqlx::query(
                "INSERT INTO operations \
                 (operation_id, idempotency_key, resource_kind, resource_id, operation_type, status, \
                  desired_generation, correlation_id, requested_at, updated_at) \
                 VALUES (?, ?, 'Volume', ?, 'AttachVolume', 'Accepted', 1, 'vm=vm-att', \
                  '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            )
            .bind(op_id)
            .bind(format!("idem-{op_id}"))
            .bind(volume_id)
            .execute(&pool)
            .await
            .expect("seed attach op");
        }

        let dir = tempfile::tempdir().unwrap();
        let pattern = dir
            .path()
            .join("agent-{node_id}.sock")
            .to_str()
            .unwrap()
            .to_string();
        let agent = MockLifecycleAgent {
            snapshot_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_status: tonic::Status::unimplemented(""),
            snapshot_vm_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_vm_status: tonic::Status::unimplemented(""),
            overlay_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            overlay_status_by_node: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            attach_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_status: tonic::Status::ok(""),
            create_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_status: tonic::Status::ok(""),
            create_status: tonic::Status::ok(""),
        };
        spawn_mock_lifecycle_agent(&pattern, "node-att", agent.clone());

        let orchestrator = test_orchestrator(&pool, &pattern);
        orchestrator.tick().await.expect("tick");

        let calls = agent.attach_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2, "both attach ops dispatched: {calls:?}");
        let spec_json_for = |volume_id: &str| {
            calls
                .iter()
                .find(|c| c.volume.as_ref().map(|v| v.volume_id.as_str()) == Some(volume_id))
                .unwrap_or_else(|| panic!("no attach dispatch for {volume_id}: {calls:?}"))
                .volume
                .clone()
                .unwrap()
                .volume_spec_json
        };
        assert_eq!(
            spec_json_for("vol-clone-dst"),
            br#"{"backend_class":"lvm"}"#.to_vec(),
            "a class-carrying volume's attach must carry exactly the class key"
        );
        assert_eq!(
            spec_json_for("vol-bare"),
            b"{}".to_vec(),
            "a NULL-class volume's attach must carry the empty JSON object (PR 3 correction: parseable, key-free, defaults at the agent)"
        );

        // Both ops converged on the OK ack.
        for op_id in ["op-att-cls", "op-att-bare"] {
            let status: String =
                sqlx::query_scalar("SELECT status FROM operations WHERE operation_id = ?")
                    .bind(op_id)
                    .fetch_one(&pool)
                    .await
                    .expect("op status");
            assert_eq!(status, "Succeeded", "{op_id} must converge");
        }
    }

    /// #533 (the #513 design's DP2 locator guard, §8): the
    /// AttachVolume dispatch shapes a STANDALONE volume's
    /// (`volume_kind = 'data'` — the #513 DP8 stamp, the same
    /// discriminator the #522 delete's kind gate rides)
    /// `volume_spec_json` with the create carrier's relative
    /// `{volume_id}.img` locator, so the agent's A4 open lands on the
    /// file the #513 create carrier minted — not a create-on-open
    /// SECOND default-size file at the bare-id default, the stray the
    /// #522 delete's DP4 destroy deliberately never chases. LVM keeps
    /// the class-only shape (the agent's LVM default already shapes
    /// the carrier's dm-path token). An embedded volume (NULL kind —
    /// every pre-#513 volume, boot disk, import, template) keeps the
    /// pre-#533 bytes: no locator key, so the A4 bare-id default and
    /// the A1 vm-nested path are untouched.
    #[tokio::test]
    async fn attach_volume_dispatch_shapes_the_standalone_carrier_locator() {
        let pool = create_test_pool().await;
        seed_node(&pool, "node-att-533").await;

        // Two standalone 'data' volumes in the #513 route's own row
        // shapes (NULL class = local, never materialized; the LVM one
        // carries its class), plus an embedded NULL-kind volume.
        sqlx::query(
            "INSERT INTO volumes (volume_id, node_id, display_name, capacity_bytes, volume_kind, storage_class) \
             VALUES ('vol-std', 'node-att-533', 'Vol vol-std', 1073741824, 'data', NULL), \
                    ('vol-std-lvm', 'node-att-533', 'Vol vol-std-lvm', 1073741824, 'data', 'lvm')",
        )
        .execute(&pool)
        .await
        .expect("seed standalone volumes");
        seed_volume(&pool, "vol-emb", "node-att-533").await;
        for volume_id in ["vol-std", "vol-std-lvm", "vol-emb"] {
            sqlx::query(
                "INSERT INTO volume_desired_state \
                 (volume_id, desired_generation, desired_status, attached_vm_id, read_only) \
                 VALUES (?, 1, 'Pending', NULL, 0)",
            )
            .bind(volume_id)
            .execute(&pool)
            .await
            .expect("seed volume desired state");
        }
        for (op_id, volume_id) in [
            ("op-att-std", "vol-std"),
            ("op-att-std-lvm", "vol-std-lvm"),
            ("op-att-emb", "vol-emb"),
        ] {
            sqlx::query(
                "INSERT INTO operations \
                 (operation_id, idempotency_key, resource_kind, resource_id, operation_type, status, \
                  desired_generation, correlation_id, requested_at, updated_at) \
                 VALUES (?, ?, 'Volume', ?, 'AttachVolume', 'Accepted', 1, 'vm=vm-att', \
                  '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            )
            .bind(op_id)
            .bind(format!("idem-{op_id}"))
            .bind(volume_id)
            .execute(&pool)
            .await
            .expect("seed attach op");
        }

        let dir = tempfile::tempdir().unwrap();
        let pattern = dir
            .path()
            .join("agent-{node_id}.sock")
            .to_str()
            .unwrap()
            .to_string();
        let agent = MockLifecycleAgent {
            snapshot_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_status: tonic::Status::unimplemented(""),
            snapshot_vm_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_vm_status: tonic::Status::unimplemented(""),
            overlay_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            overlay_status_by_node: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            attach_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_status: tonic::Status::ok(""),
            create_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_status: tonic::Status::ok(""),
            create_status: tonic::Status::ok(""),
        };
        spawn_mock_lifecycle_agent(&pattern, "node-att-533", agent.clone());

        let orchestrator = test_orchestrator(&pool, &pattern);
        orchestrator.tick().await.expect("tick");

        let calls = agent.attach_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 3, "all three attach ops dispatched: {calls:?}");
        let spec_json_for = |volume_id: &str| {
            calls
                .iter()
                .find(|c| c.volume.as_ref().map(|v| v.volume_id.as_str()) == Some(volume_id))
                .unwrap_or_else(|| panic!("no attach dispatch for {volume_id}: {calls:?}"))
                .volume
                .clone()
                .unwrap()
                .volume_spec_json
        };
        assert_eq!(
            spec_json_for("vol-std"),
            br#"{"locator":"vol-std.img"}"#.to_vec(),
            "a standalone NULL-class volume's attach must carry the create carrier's relative locator — exactly that key"
        );
        assert_eq!(
            spec_json_for("vol-std-lvm"),
            br#"{"backend_class":"lvm"}"#.to_vec(),
            "a standalone LVM volume's attach keeps the class-only shape (the agent's LVM default IS the carrier locator)"
        );
        assert_eq!(
            spec_json_for("vol-emb"),
            b"{}".to_vec(),
            "an embedded (NULL-kind) volume's attach keeps the pre-#533 key-free bytes"
        );

        // All three ops converged on the OK ack.
        for op_id in ["op-att-std", "op-att-std-lvm", "op-att-emb"] {
            let status: String =
                sqlx::query_scalar("SELECT status FROM operations WHERE operation_id = ?")
                    .bind(op_id)
                    .fetch_one(&pool)
                    .await
                    .expect("op status");
            assert_eq!(status, "Succeeded", "{op_id} must converge");
        }
    }

    /// #513 PR 1 (DP2): the `CreateVolume` dispatch arm resolves the
    /// volume's capacity AND class in the claim query and dispatches
    /// the carrier RPC with a provisioning payload —
    /// `{"backend_class":"lvm","size_bytes":N}` for a class-carrying
    /// volume, `{"size_bytes":N}` for a NULL-class one (the
    /// never-materialize-`"local"` discipline; the #511 wire-key seam).
    /// Dead-but-live: no producer journals a `CreateVolume` operation
    /// until the BFF route lands (PR 2) — the rows here are seeded
    /// directly, exactly as the attach arm's test does.
    #[tokio::test]
    async fn create_volume_dispatch_carries_size_and_class() {
        let pool = create_test_pool().await;
        seed_node(&pool, "node-cr").await;
        // The create arm places new storage: the node must be schedulable
        // (the VM-create placement discipline).
        seed_node_tenant_ready(&pool, "node-cr").await;

        // A class-carrying volume and a NULL-class one, with distinct
        // capacities the dispatch must carry verbatim.
        sqlx::query(
            "INSERT INTO volumes (volume_id, node_id, display_name, capacity_bytes, storage_class) \
             VALUES ('vol-cr-lvm', 'node-cr', 'Vol vol-cr-lvm', 1073741824, 'lvm'), \
                    ('vol-cr-bare', 'node-cr', 'Vol vol-cr-bare', 536870912, NULL)",
        )
        .execute(&pool)
        .await
        .expect("seed volumes");
        for (op_id, volume_id) in [("op-cr-cls", "vol-cr-lvm"), ("op-cr-bare", "vol-cr-bare")] {
            sqlx::query(
                "INSERT INTO operations \
                 (operation_id, idempotency_key, resource_kind, resource_id, operation_type, status, \
                  desired_generation, requested_at, updated_at) \
                 VALUES (?, ?, 'Volume', ?, 'CreateVolume', 'Accepted', 1, \
                  '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            )
            .bind(op_id)
            .bind(format!("idem-{op_id}"))
            .bind(volume_id)
            .execute(&pool)
            .await
            .expect("seed create-volume op");
        }

        let dir = tempfile::tempdir().unwrap();
        let pattern = dir
            .path()
            .join("agent-{node_id}.sock")
            .to_str()
            .unwrap()
            .to_string();
        let agent = MockLifecycleAgent {
            snapshot_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_status: tonic::Status::unimplemented(""),
            snapshot_vm_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_vm_status: tonic::Status::unimplemented(""),
            overlay_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            overlay_status_by_node: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            attach_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_status: tonic::Status::ok(""),
            create_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_status: tonic::Status::ok(""),
            create_status: tonic::Status::ok(""),
        };
        spawn_mock_lifecycle_agent(&pattern, "node-cr", agent.clone());

        let orchestrator = test_orchestrator(&pool, &pattern);
        orchestrator.tick().await.expect("tick");

        let calls = agent.create_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2, "both create ops dispatched: {calls:?}");
        let call_for = |volume_id: &str| {
            calls
                .iter()
                .find(|c| c.volume.as_ref().map(|v| v.volume_id.as_str()) == Some(volume_id))
                .unwrap_or_else(|| panic!("no create dispatch for {volume_id}: {calls:?}"))
        };
        assert_eq!(
            call_for("vol-cr-lvm")
                .volume
                .as_ref()
                .unwrap()
                .volume_spec_json,
            br#"{"backend_class":"lvm","size_bytes":1073741824}"#.to_vec(),
            "a class-carrying volume's create must carry exactly the class and size keys"
        );
        assert_eq!(
            call_for("vol-cr-bare")
                .volume
                .as_ref()
                .unwrap()
                .volume_spec_json,
            br#"{"size_bytes":536870912}"#.to_vec(),
            "a NULL-class volume's create must carry the size only — no materialized \"local\""
        );
        // The dispatch targets the volume's node and leaves the unused
        // vm_id empty (DP4: standalone volumes have no VM).
        for call in &calls {
            assert_eq!(call.node_id, "node-cr");
            assert_eq!(call.volume.as_ref().unwrap().vm_id, "");
        }

        // Both ops converged on the OK ack.
        for op_id in ["op-cr-cls", "op-cr-bare"] {
            let status: String =
                sqlx::query_scalar("SELECT status FROM operations WHERE operation_id = ?")
                    .bind(op_id)
                    .fetch_one(&pool)
                    .await
                    .expect("op status");
            assert_eq!(status, "Succeeded", "{op_id} must converge");
        }
    }

    /// #513 PR 1: the arm refuses to dispatch a create whose volume row
    /// carries no positive capacity (the agent's open cannot provision
    /// without a size — the load-bearing §2.4 finding). The refusal is
    /// an ordinary dispatch error (not terminal-class): the operation
    /// enters the shared retry arm, byte-exactly like every other
    /// non-Unimplemented dispatch failure, and no RPC reaches the agent.
    #[tokio::test]
    async fn create_volume_dispatch_without_capacity_enters_the_retry_arm() {
        let pool = create_test_pool().await;
        seed_node(&pool, "node-cr2").await;
        seed_node_tenant_ready(&pool, "node-cr2").await;
        // No volumes row for the resource: the claim resolves
        // capacity NULL (and node via the vm fallback — seed a VM row so
        // the dispatch gets past node resolution and fails at the arm).
        seed_vm(&pool, "vol-cr-missing", "node-cr2").await;
        sqlx::query(
            "INSERT INTO operations \
             (operation_id, idempotency_key, resource_kind, resource_id, operation_type, status, \
              desired_generation, requested_at, updated_at) \
             VALUES ('op-cr-nocap', 'idem-op-cr-nocap', 'Volume', 'vol-cr-missing', \
              'CreateVolume', 'Accepted', 1, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .expect("seed capacity-less create op");

        let dir = tempfile::tempdir().unwrap();
        let pattern = dir
            .path()
            .join("agent-{node_id}.sock")
            .to_str()
            .unwrap()
            .to_string();
        let agent = MockLifecycleAgent {
            snapshot_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_status: tonic::Status::unimplemented(""),
            snapshot_vm_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_vm_status: tonic::Status::unimplemented(""),
            overlay_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            overlay_status_by_node: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            attach_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_status: tonic::Status::ok(""),
            create_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_status: tonic::Status::ok(""),
            create_status: tonic::Status::ok(""),
        };
        spawn_mock_lifecycle_agent(&pattern, "node-cr2", agent.clone());

        let orchestrator = test_orchestrator(&pool, &pattern);
        orchestrator.tick().await.expect("tick");

        let (status, _, error_message, retry_count, next_retry_at, _) =
            op_row(&pool, "op-cr-nocap").await;
        assert_eq!(
            status, "RetryPending",
            "a capacity-less create is an ordinary retried dispatch error"
        );
        assert_eq!(retry_count, 1);
        assert!(next_retry_at.is_some(), "the backoff schedule is written");
        assert!(
            error_message
                .as_deref()
                .unwrap_or_default()
                .contains("positive capacity_bytes"),
            "the refusal must name the missing input: {error_message:?}"
        );
        assert!(
            agent.create_calls.lock().unwrap().is_empty(),
            "no RPC may reach the agent for a capacity-less create"
        );
    }

    /// #513 DP7 (the CP half of the core-managed posture): an agent
    /// refusing the create with gRPC `Unimplemented` (the fail-closed
    /// core-managed gate) takes the operation terminal on the FIRST
    /// dispatch — `Failed`/`UNSUPPORTED_BY_AGENT` carrying the agent's
    /// refusal text, zero retries, no `mark_for_retry` resurrection —
    /// the #378 §7 fast-fail machinery, pinned on the new arm.
    #[tokio::test]
    async fn create_volume_dispatch_refusal_fails_fast_without_retry() {
        let pool = create_test_pool().await;
        seed_node(&pool, "node-cr3").await;
        seed_node_tenant_ready(&pool, "node-cr3").await;
        sqlx::query(
            "INSERT INTO volumes (volume_id, node_id, display_name, capacity_bytes) \
             VALUES ('vol-cr-cm', 'node-cr3', 'Vol vol-cr-cm', 1024)",
        )
        .execute(&pool)
        .await
        .expect("seed volume");
        sqlx::query(
            "INSERT INTO operations \
             (operation_id, idempotency_key, resource_kind, resource_id, operation_type, status, \
              desired_generation, requested_at, updated_at) \
             VALUES ('op-cr-cm', 'idem-op-cr-cm', 'Volume', 'vol-cr-cm', 'CreateVolume', \
              'Accepted', 1, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .expect("seed create op");

        let dir = tempfile::tempdir().unwrap();
        let pattern = dir
            .path()
            .join("agent-{node_id}.sock")
            .to_str()
            .unwrap()
            .to_string();
        let agent = MockLifecycleAgent {
            snapshot_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_status: tonic::Status::unimplemented(""),
            snapshot_vm_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_vm_status: tonic::Status::unimplemented(""),
            overlay_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            overlay_status_by_node: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            attach_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_status: tonic::Status::ok(""),
            create_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_status: tonic::Status::ok(""),
            create_status: tonic::Status::unimplemented(
                "create_volume is unsupported in core-managed mode",
            ),
        };
        spawn_mock_lifecycle_agent(&pattern, "node-cr3", agent.clone());

        let orchestrator = test_orchestrator(&pool, &pattern);

        // First (and only) dispatch: the op must go terminal here.
        orchestrator.tick().await.expect("tick 1");
        let (status, error_code, error_message, retry_count, next_retry_at, completed_at) =
            op_row(&pool, "op-cr-cm").await;
        assert_eq!(
            status, "Failed",
            "Unimplemented is terminal on first dispatch"
        );
        assert_eq!(
            error_code.as_deref(),
            Some("UNSUPPORTED_BY_AGENT"),
            "the error code must name the cause"
        );
        assert!(
            error_message
                .as_deref()
                .unwrap_or_default()
                .contains("create_volume is unsupported in core-managed mode"),
            "the agent's refusal text must ride the error message: {error_message:?}"
        );
        assert_eq!(retry_count, 0, "no retry may be scheduled");
        assert_eq!(next_retry_at, None, "mark_for_retry must never run");
        assert!(
            completed_at.is_some(),
            "the terminal write stamps completed_at"
        );

        // Further ticks must not resurrect the terminal row or re-dispatch.
        orchestrator.tick().await.expect("tick 2");
        orchestrator.tick().await.expect("tick 3");
        assert_eq!(
            agent.create_calls.lock().unwrap().len(),
            1,
            "exactly one agent dispatch across all ticks"
        );
        let (status, _, _, retry_count, next_retry_at, _) = op_row(&pool, "op-cr-cm").await;
        assert_eq!(status, "Failed", "the terminal row stays terminal");
        assert_eq!(retry_count, 0);
        assert_eq!(next_retry_at, None);
    }

    /// #522 PR 1 (DP2): the `DeleteVolume` dispatch arm resolves the
    /// volume's class in the claim query (the attach arm's discipline)
    /// and dispatches the carrier RPC — a class-carrying volume's
    /// delete threads the class so the agent can shape the DP4 carrier
    /// locator; a NULL-class volume's delete threads the EMPTY string
    /// (never a materialized `"local"`). Dead-but-live: no producer
    /// journals a `DeleteVolume` operation until the BFF route lands
    /// (PR 2) — the rows here are seeded directly, exactly as the
    /// create arm's test does.
    #[tokio::test]
    async fn delete_volume_dispatch_carries_volume_and_class() {
        let pool = create_test_pool().await;
        seed_node(&pool, "node-dl").await;
        // A delete reclaims storage; it does not place any — the
        // schedulability gate is deliberately NOT joined (the
        // create arm's placement discipline does not apply).
        sqlx::query(
            "INSERT INTO volumes (volume_id, node_id, display_name, capacity_bytes, storage_class) \
             VALUES ('vol-dl-lvm', 'node-dl', 'Vol vol-dl-lvm', 1073741824, 'lvm'), \
                     ('vol-dl-bare', 'node-dl', 'Vol vol-dl-bare', 536870912, NULL)",
        )
        .execute(&pool)
        .await
        .expect("seed volumes");
        for (op_id, volume_id) in [("op-dl-cls", "vol-dl-lvm"), ("op-dl-bare", "vol-dl-bare")] {
            sqlx::query(
                "INSERT INTO operations \
                 (operation_id, idempotency_key, resource_kind, resource_id, operation_type, status, \
                  desired_generation, requested_at, updated_at) \
                 VALUES (?, ?, 'Volume', ?, 'DeleteVolume', 'Accepted', 1, \
                  '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            )
            .bind(op_id)
            .bind(format!("idem-{op_id}"))
            .bind(volume_id)
            .execute(&pool)
            .await
            .expect("seed delete-volume op");
        }

        let dir = tempfile::tempdir().unwrap();
        let pattern = dir
            .path()
            .join("agent-{node_id}.sock")
            .to_str()
            .unwrap()
            .to_string();
        let agent = MockLifecycleAgent {
            snapshot_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_status: tonic::Status::unimplemented(""),
            snapshot_vm_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_vm_status: tonic::Status::unimplemented(""),
            overlay_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            overlay_status_by_node: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            attach_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_status: tonic::Status::ok(""),
            create_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            create_status: tonic::Status::ok(""),
            delete_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_status: tonic::Status::ok(""),
        };
        spawn_mock_lifecycle_agent(&pattern, "node-dl", agent.clone());

        let orchestrator = test_orchestrator(&pool, &pattern);
        orchestrator.tick().await.expect("tick");

        let calls = agent.delete_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2, "both delete ops dispatched: {calls:?}");
        let call_for = |volume_id: &str| {
            calls
                .iter()
                .find(|c| c.volume_id == volume_id)
                .unwrap_or_else(|| panic!("no delete dispatch for {volume_id}: {calls:?}"))
        };
        // The dispatch targets the volume's node and resource id, and
        // threads the class through the #511 wire-key discipline (NULL
        // emits the empty string, never "local").
        assert_eq!(call_for("vol-dl-lvm").node_id, "node-dl");
        assert_eq!(call_for("vol-dl-lvm").volume_id, "vol-dl-lvm");
        assert_eq!(call_for("vol-dl-lvm").backend_class, "lvm");
        assert_eq!(
            call_for("vol-dl-bare").backend_class,
            "",
            "a NULL-class volume's delete must carry the empty class — no materialized \"local\""
        );

        // Both ops converged on the OK ack.
        for op_id in ["op-dl-cls", "op-dl-bare"] {
            let status: String =
                sqlx::query_scalar("SELECT status FROM operations WHERE operation_id = ?")
                    .bind(op_id)
                    .fetch_one(&pool)
                    .await
                    .expect("op status");
            assert_eq!(status, "Succeeded", "{op_id} must converge");
        }
    }

    /// #522 DP10 (the CP half of the core-managed posture): an agent
    /// refusing the delete with gRPC `Unimplemented` (the fail-closed
    /// core-managed gate) takes the operation terminal on the FIRST
    /// dispatch — `Failed`/`UNSUPPORTED_BY_AGENT` carrying the agent's
    /// refusal text, zero retries — the #378 §7 fast-fail machinery,
    /// pinned on the new arm (the create arm's twin).
    #[tokio::test]
    async fn delete_volume_dispatch_refusal_fails_fast_without_retry() {
        let pool = create_test_pool().await;
        seed_node(&pool, "node-dl2").await;
        sqlx::query(
            "INSERT INTO volumes (volume_id, node_id, display_name, capacity_bytes) \
             VALUES ('vol-dl-cm', 'node-dl2', 'Vol vol-dl-cm', 1024)",
        )
        .execute(&pool)
        .await
        .expect("seed volume");
        sqlx::query(
            "INSERT INTO operations \
             (operation_id, idempotency_key, resource_kind, resource_id, operation_type, status, \
              desired_generation, requested_at, updated_at) \
             VALUES ('op-dl-cm', 'idem-op-dl-cm', 'Volume', 'vol-dl-cm', 'DeleteVolume', \
              'Accepted', 1, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .expect("seed delete op");

        let dir = tempfile::tempdir().unwrap();
        let pattern = dir
            .path()
            .join("agent-{node_id}.sock")
            .to_str()
            .unwrap()
            .to_string();
        let agent = MockLifecycleAgent {
            snapshot_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_status: tonic::Status::unimplemented(""),
            snapshot_vm_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_vm_status: tonic::Status::unimplemented(""),
            overlay_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            overlay_status_by_node: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            attach_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_status: tonic::Status::ok(""),
            create_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            create_status: tonic::Status::ok(""),
            delete_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_status: tonic::Status::unimplemented(
                "delete_volume is unsupported in core-managed mode",
            ),
        };
        spawn_mock_lifecycle_agent(&pattern, "node-dl2", agent.clone());

        let orchestrator = test_orchestrator(&pool, &pattern);

        // First (and only) dispatch: the op must go terminal here.
        orchestrator.tick().await.expect("tick 1");
        let (status, error_code, error_message, retry_count, next_retry_at, completed_at) =
            op_row(&pool, "op-dl-cm").await;
        assert_eq!(
            status, "Failed",
            "Unimplemented is terminal on first dispatch"
        );
        assert_eq!(
            error_code.as_deref(),
            Some("UNSUPPORTED_BY_AGENT"),
            "the error code must name the cause"
        );
        assert!(
            error_message
                .as_deref()
                .unwrap_or_default()
                .contains("delete_volume is unsupported in core-managed mode"),
            "the agent's refusal text must ride the error message: {error_message:?}"
        );
        assert_eq!(retry_count, 0, "no retry may be scheduled");
        assert_eq!(next_retry_at, None, "mark_for_retry must never run");
        assert!(
            completed_at.is_some(),
            "the terminal write stamps completed_at"
        );

        // Further ticks must not resurrect the terminal row or re-dispatch.
        orchestrator.tick().await.expect("tick 2");
        assert_eq!(
            agent.delete_calls.lock().unwrap().len(),
            1,
            "exactly one agent dispatch across all ticks"
        );
        let (status, _, _, retry_count, next_retry_at, _) = op_row(&pool, "op-dl-cm").await;
        assert_eq!(status, "Failed", "the terminal row stays terminal");
        assert_eq!(retry_count, 0);
        assert_eq!(next_retry_at, None);
    }

    // ============================================================
    // #355 PR 1 — the UpdateNetworkPolicy dispatch carrier (fan-out)
    // (design docs/design/issue-355-network-policy-dispatch.md, §3
    // Option A + DP2; the M2.2b carve-out is pinned agent-side)
    // ============================================================

    /// Seeds a network + desired-state row with the given ruleset and
    /// generation, and (optionally) a VM with a NIC on it at the given
    /// node. The networks row is the fleet shape the design targets:
    /// `node_id` NULL (an operator-created network has no owning node
    /// — DP2's whole premise).
    async fn seed_network_policy_fixture(
        pool: &StorePool,
        network_id: &str,
        firewall_rules_json: Option<&str>,
        desired_generation: i64,
    ) {
        sqlx::query("INSERT INTO networks (network_id, node_id, display_name) VALUES (?, NULL, ?)")
            .bind(network_id)
            .bind(format!("Net {network_id}"))
            .execute(pool)
            .await
            .expect("insert network");
        sqlx::query(
            "INSERT INTO network_desired_state \
             (network_id, desired_generation, desired_status, cidr, gateway, dhcp_enabled, \
              ipam_mode, is_default, firewall_rules_json) \
             VALUES (?, ?, 'Pending', '10.200.0.0/24', '10.200.0.1', 1, 'internal', 0, ?)",
        )
        .bind(network_id)
        .bind(desired_generation)
        .bind(firewall_rules_json)
        .execute(pool)
        .await
        .expect("seed network desired state");
    }

    async fn seed_vm_with_nic(
        pool: &StorePool,
        vm_id: &str,
        node_id: &str,
        network_id: &str,
        desired_status: Option<&str>,
    ) {
        seed_node(pool, node_id).await;
        seed_vm(pool, vm_id, node_id).await;
        sqlx::query(
            "INSERT INTO vm_nic_desired_state (nic_id, vm_id, network_id, mac_address) \
             VALUES (?, ?, ?, '52:54:00:00:00:01')",
        )
        .bind(format!("nic-{vm_id}"))
        .bind(vm_id)
        .bind(network_id)
        .execute(pool)
        .await
        .expect("seed vm nic");
        if let Some(status) = desired_status {
            sqlx::query("UPDATE vm_desired_state SET desired_status = ? WHERE vm_id = ?")
                .bind(status)
                .bind(vm_id)
                .execute(pool)
                .await
                .expect("set vm desired_status");
        }
    }

    /// #355 PR 1 (DP2): the dispatch fans out to every node with a
    /// LIVE attached VM on the network — the fleet-network shape where
    /// `networks.node_id` is meaningless — threading the stored
    /// ruleset and the NDS generation (the DP7 fence rides
    /// meta.desired_state_version), and a Deleting VM's node is
    /// excluded (tombstone-aware target resolution). Dead-but-live: no
    /// producer journals an `UpdateNetworkPolicy` operation until the
    /// BFF route lands (PR 2) — the row here is seeded directly, exactly
    /// as the volume arms' tests do.
    #[tokio::test]
    async fn update_network_policy_fans_out_to_nodes_with_live_attached_vms() {
        let pool = create_test_pool().await;
        let policy = r#"[{"direction":"ingress","action":"accept","protocol":"icmp","source":"10.200.0.0/24"}]"#;
        seed_network_policy_fixture(&pool, "net-np", Some(policy), 4).await;
        seed_vm_with_nic(&pool, "vm-np-a", "node-np-a", "net-np", None).await;
        seed_vm_with_nic(&pool, "vm-np-b", "node-np-b", "net-np", None).await;
        // A Deleting VM's node must NOT join the target set.
        seed_vm_with_nic(
            &pool,
            "vm-np-dying",
            "node-np-dying",
            "net-np",
            Some("Deleting"),
        )
        .await;
        seed_accepted_op(
            &pool,
            "op-np",
            "Network",
            "net-np",
            "UpdateNetworkPolicy",
            "2026-01-01T00:00:00Z",
        )
        .await;

        let dir = tempfile::tempdir().unwrap();
        let pattern = dir
            .path()
            .join("agent-{node_id}.sock")
            .to_str()
            .unwrap()
            .to_string();
        let agent = MockLifecycleAgent {
            snapshot_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_status: tonic::Status::unimplemented(""),
            snapshot_vm_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_vm_status: tonic::Status::unimplemented(""),
            overlay_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            overlay_status_by_node: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            attach_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_status: tonic::Status::ok(""),
            create_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_status: tonic::Status::ok(""),
            create_status: tonic::Status::ok(""),
        };
        spawn_mock_lifecycle_agent(&pattern, "node-np-a", agent.clone());
        spawn_mock_lifecycle_agent(&pattern, "node-np-b", agent.clone());
        spawn_mock_lifecycle_agent(&pattern, "node-np-dying", agent.clone());

        let orchestrator = test_orchestrator(&pool, &pattern);
        orchestrator.tick().await.expect("tick");

        let calls = agent.network_policy_calls.lock().unwrap().clone();
        assert_eq!(
            calls.len(),
            2,
            "exactly the two live nodes dispatch, not the Deleting VM's node: {calls:?}"
        );
        let mut target_nodes: Vec<&str> = calls.iter().map(|c| c.node_id.as_str()).collect();
        target_nodes.sort_unstable();
        assert_eq!(target_nodes, ["node-np-a", "node-np-b"]);
        for call in &calls {
            assert_eq!(call.network_id, "net-np");
            assert_eq!(call.policy_json, policy.as_bytes().to_vec());
            assert_eq!(
                call.meta.as_ref().map(|m| m.desired_state_version.as_str()),
                Some("4"),
                "the NDS generation rides meta.desired_state_version (the DP7 fence)"
            );
        }
        let status: String =
            sqlx::query_scalar("SELECT status FROM operations WHERE operation_id = ?")
                .bind("op-np")
                .fetch_one(&pool)
                .await
                .expect("op status");
        assert_eq!(status, "Succeeded", "the fan-out converged on the OK acks");
    }

    /// #355 PR 1: the #360 discipline (unchanged by this PR) — a
    /// semantically empty ruleset is never dispatched; the operation
    /// completes no-op Succeeded rather than failing. PR 3 flips the
    /// semantics (DP4, ruled 2026-10-08: `[]` = baseline +
    /// default-deny).
    #[tokio::test]
    async fn update_network_policy_empty_ruleset_dispatches_the_dp4_baseline() {
        let pool = create_test_pool().await;
        seed_network_policy_fixture(&pool, "net-np-empty", Some("[]"), 2).await;
        seed_vm_with_nic(&pool, "vm-np-empty", "node-np-empty", "net-np-empty", None).await;
        seed_accepted_op(
            &pool,
            "op-np-empty",
            "Network",
            "net-np-empty",
            "UpdateNetworkPolicy",
            "2026-01-01T00:00:00Z",
        )
        .await;
        // The never-set (NULL) ruleset arm: same DP4 meaning ("no user
        // rules" → baseline). Unreachable from the BFF producer (it
        // journals only when firewall_rules rode the request), but the
        // arm is generic — pin the resolution so a future producer
        // cannot reintroduce an unfiltered skip for rule-less
        // networks.
        seed_network_policy_fixture(&pool, "net-np-null", None, 3).await;
        seed_vm_with_nic(&pool, "vm-np-null", "node-np-null", "net-np-null", None).await;
        seed_accepted_op(
            &pool,
            "op-np-null",
            "Network",
            "net-np-null",
            "UpdateNetworkPolicy",
            "2026-01-01T00:00:00Z",
        )
        .await;

        let dir = tempfile::tempdir().unwrap();
        let pattern = dir
            .path()
            .join("agent-{node_id}.sock")
            .to_str()
            .unwrap()
            .to_string();
        let agent = MockLifecycleAgent {
            snapshot_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_status: tonic::Status::unimplemented(""),
            snapshot_vm_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_vm_status: tonic::Status::unimplemented(""),
            overlay_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            overlay_status_by_node: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            attach_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_status: tonic::Status::ok(""),
            create_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_status: tonic::Status::ok(""),
            create_status: tonic::Status::ok(""),
        };
        spawn_mock_lifecycle_agent(&pattern, "node-np-empty", agent.clone());
        spawn_mock_lifecycle_agent(&pattern, "node-np-null", agent.clone());

        let orchestrator = test_orchestrator(&pool, &pattern);
        orchestrator.tick().await.expect("tick");

        // DP4 (ruled 2026-10-08): `[]` = no user rules → the shared
        // BASELINE + default-deny is dispatched to the attached node —
        // the pre-DP4 #360 no-op is gone, and the bytes the agent's
        // fence sees are the shared module's baseline verbatim (with
        // the generation riding meta.desired_state_version). The
        // never-set (NULL) ruleset resolves identically.
        let calls = agent.network_policy_calls.lock().unwrap().clone();
        assert_eq!(
            calls.len(),
            2,
            "the empty and never-set rulesets each dispatch the baseline (DP4): {calls:?}"
        );
        let mut by_network: std::collections::HashMap<&str, &proto::ApplyNetworkPolicyRequest> =
            calls.iter().map(|c| (c.network_id.as_str(), c)).collect();
        for (network_id, generation) in [("net-np-empty", "2"), ("net-np-null", "3")] {
            let call = by_network
                .remove(network_id)
                .unwrap_or_else(|| panic!("no dispatch for {network_id}: {calls:?}"));
            assert_eq!(
                call.policy_json,
                chv_common::firewall::baseline_policy_json().into_bytes(),
                "{network_id}: the dispatched bytes are the shared DP4 baseline verbatim"
            );
            assert_eq!(
                call.meta.as_ref().map(|m| m.desired_state_version.as_str()),
                Some(generation),
                "{network_id}: the DP7 fence rides the NDS generation"
            );
        }
        assert!(by_network.is_empty(), "no unexpected dispatches: {calls:?}");
        let (status, _, _, retry_count, next_retry_at, completed_at) =
            op_row(&pool, "op-np-empty").await;
        assert_eq!(
            status, "Succeeded",
            "the dispatch completes, it does not fail"
        );
        assert_eq!(retry_count, 0);
        assert_eq!(next_retry_at, None);
        assert!(
            completed_at.is_some(),
            "the terminal write stamps completed_at"
        );
        let (status, _, _, _, _, completed_at) = op_row(&pool, "op-np-null").await;
        assert_eq!(status, "Succeeded", "the never-set dispatch completes too");
        assert!(completed_at.is_some());
    }

    /// #355 PR 1 (DP2): a fleet network with rules but no attached VMs
    /// yet is the normal create-then-populate order — zero targets is
    /// a no-op Succeeded (the attach-time snapshot path applies the
    /// policy at first materialization), not a failure.
    #[tokio::test]
    async fn update_network_policy_without_attached_vms_is_a_noop_success() {
        let pool = create_test_pool().await;
        seed_network_policy_fixture(
            &pool,
            "net-np-bare",
            Some(r#"[{"direction":"ingress","action":"accept","protocol":"icmp"}]"#),
            3,
        )
        .await;
        seed_accepted_op(
            &pool,
            "op-np-bare",
            "Network",
            "net-np-bare",
            "UpdateNetworkPolicy",
            "2026-01-01T00:00:00Z",
        )
        .await;

        let dir = tempfile::tempdir().unwrap();
        let pattern = dir
            .path()
            .join("agent-{node_id}.sock")
            .to_str()
            .unwrap()
            .to_string();
        let orchestrator = test_orchestrator(&pool, &pattern);
        orchestrator.tick().await.expect("tick");

        let (status, _, _, retry_count, next_retry_at, completed_at) =
            op_row(&pool, "op-np-bare").await;
        assert_eq!(status, "Succeeded", "zero targets is a no-op success");
        assert_eq!(retry_count, 0);
        assert_eq!(next_retry_at, None);
        assert!(
            completed_at.is_some(),
            "the terminal write stamps completed_at"
        );
    }

    /// #355 PR 1 review fold: the two remaining no-op branches — a
    /// missing NDS row (the op's network was deleted before the
    /// dispatch claimed it) and a `'Deleting'` NDS row (the policy
    /// update raced the network delete) — complete no-op `Succeeded`,
    /// never a failure: the delete path tears the topology down at
    /// last-detach, so there is nothing to apply.
    #[tokio::test]
    async fn update_network_policy_missing_or_deleting_nds_is_a_noop_success() {
        let pool = create_test_pool().await;
        // Missing NDS: the op names a network with no desired-state
        // row at all.
        seed_accepted_op(
            &pool,
            "op-np-missing",
            "Network",
            "net-np-gone",
            "UpdateNetworkPolicy",
            "2026-01-01T00:00:00Z",
        )
        .await;
        // Deleting NDS: the row exists but is tombstoned.
        seed_network_policy_fixture(
            &pool,
            "net-np-del",
            Some(r#"[{"direction":"ingress","action":"accept","protocol":"icmp"}]"#),
            2,
        )
        .await;
        sqlx::query("UPDATE network_desired_state SET desired_status = 'Deleting' WHERE network_id = 'net-np-del'")
            .execute(&pool)
            .await
            .expect("tombstone the NDS row");
        seed_accepted_op(
            &pool,
            "op-np-del",
            "Network",
            "net-np-del",
            "UpdateNetworkPolicy",
            "2026-01-01T00:00:00Z",
        )
        .await;

        let dir = tempfile::tempdir().unwrap();
        let pattern = dir
            .path()
            .join("agent-{node_id}.sock")
            .to_str()
            .unwrap()
            .to_string();
        // A live target on the Deleting network would dispatch if the
        // tombstone check were missing — seed one and prove it does
        // not (the branch is not vacuously safe).
        seed_vm_with_nic(&pool, "vm-np-del", "node-np-del", "net-np-del", None).await;
        let agent = MockLifecycleAgent {
            snapshot_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_status: tonic::Status::unimplemented(""),
            snapshot_vm_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_vm_status: tonic::Status::unimplemented(""),
            overlay_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            overlay_status_by_node: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            attach_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_status: tonic::Status::ok(""),
            create_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_status: tonic::Status::ok(""),
            create_status: tonic::Status::ok(""),
        };
        spawn_mock_lifecycle_agent(&pattern, "node-np-del", agent.clone());

        let orchestrator = test_orchestrator(&pool, &pattern);
        orchestrator.tick().await.expect("tick");

        assert!(
            agent.network_policy_calls.lock().unwrap().is_empty(),
            "neither a missing nor a Deleting NDS row may dispatch — the live target on the \
             tombstoned network proves the branch is not vacuously safe"
        );
        for op_id in ["op-np-missing", "op-np-del"] {
            let (status, error_code, _, retry_count, next_retry_at, completed_at) =
                op_row(&pool, op_id).await;
            assert_eq!(status, "Succeeded", "{op_id} completes as a no-op");
            assert_eq!(error_code, None, "{op_id} carries no failure cause");
            assert_eq!(retry_count, 0);
            assert_eq!(next_retry_at, None);
            assert!(
                completed_at.is_some(),
                "{op_id}'s terminal write stamps completed_at"
            );
        }
    }

    /// A `MockLifecycleAgent` preset for the #355 network-policy fan-out
    /// tests: only the policy status varies; every sibling surface
    /// stays at its inert default (snapshot/VM refusals, OK acks
    /// elsewhere).
    fn network_policy_mock_agent(network_policy_status: tonic::Status) -> MockLifecycleAgent {
        MockLifecycleAgent {
            snapshot_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_status: tonic::Status::unimplemented(""),
            snapshot_vm_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_vm_status: tonic::Status::unimplemented(""),
            overlay_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            overlay_status_by_node: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            attach_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_status,
            create_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_status: tonic::Status::ok(""),
            create_status: tonic::Status::ok(""),
        }
    }

    /// #355 PR 1 review round 2: the MIXED-failure branch — one node
    /// acks, one node's transport is DOWN (a connect failure, not an
    /// `Unimplemented` refusal) — must stay on the shared retry curve
    /// (`RetryPending`, a scheduled retry), NOT go terminal: the
    /// overlay arm's `update_overlay_fan_out_mixed_failure_keeps_retry_semantics`
    /// discipline, now pinned for the network-policy leg too.
    #[tokio::test]
    async fn update_network_policy_mixed_failure_keeps_retry_semantics() {
        let pool = create_test_pool().await;
        seed_network_policy_fixture(
            &pool,
            "net-np-mix",
            Some(r#"[{"direction":"ingress","action":"accept","protocol":"icmp"}]"#),
            2,
        )
        .await;
        seed_vm_with_nic(&pool, "vm-np-mix-a", "node-np-mix-a", "net-np-mix", None).await;
        seed_vm_with_nic(&pool, "vm-np-mix-b", "node-np-mix-b", "net-np-mix", None).await;
        seed_accepted_op(
            &pool,
            "op-np-mix",
            "Network",
            "net-np-mix",
            "UpdateNetworkPolicy",
            "2026-01-01T00:00:00Z",
        )
        .await;

        let dir = tempfile::tempdir().unwrap();
        let pattern = dir
            .path()
            .join("agent-{node_id}.sock")
            .to_str()
            .unwrap()
            .to_string();
        // Node A: a healthy agent. Node B: NO socket at all — the
        // connect failure is a transport error, not a refusal, so the
        // fan-out is MIXED (all_refusals = false).
        let agent_a = network_policy_mock_agent(tonic::Status::ok(""));
        spawn_mock_lifecycle_agent(&pattern, "node-np-mix-a", agent_a.clone());

        let orchestrator = test_orchestrator(&pool, &pattern);
        orchestrator.tick().await.expect("tick");

        // The healthy node dispatched exactly once...
        assert_eq!(
            agent_a.network_policy_calls.lock().unwrap().len(),
            1,
            "the reachable node's dispatch happened"
        );
        // ...and the op sits on the retry curve, not terminal.
        let (status, error_code, _, retry_count, next_retry_at, completed_at) =
            op_row(&pool, "op-np-mix").await;
        assert_eq!(
            status, "RetryPending",
            "a mixed failure retries — it does not go terminal"
        );
        assert_eq!(error_code, None, "no terminal cause is recorded mid-retry");
        assert_eq!(retry_count, 1, "exactly one retry is scheduled");
        assert!(
            next_retry_at.is_some(),
            "the retry is scheduled for the future"
        );
        assert_eq!(completed_at, None, "the op is not completed");
    }

    /// #355 PR 1 review round 2: the partial-success ALL-REFUSALS
    /// shape — one node acks, one node refuses `unimplemented` (the
    /// mixed-version fleet rollout shape) — goes terminal
    /// `Failed`/`UNSUPPORTED_BY_AGENT` on the first dispatch AND the
    /// #502 roll-up names the applied set alongside the refusing set.
    #[tokio::test]
    async fn update_network_policy_partial_refusal_names_the_applied_set() {
        let pool = create_test_pool().await;
        seed_network_policy_fixture(
            &pool,
            "net-np-part",
            Some(r#"[{"direction":"ingress","action":"accept","protocol":"icmp"}]"#),
            2,
        )
        .await;
        seed_vm_with_nic(&pool, "vm-np-part-a", "node-np-part-a", "net-np-part", None).await;
        seed_vm_with_nic(&pool, "vm-np-part-b", "node-np-part-b", "net-np-part", None).await;
        seed_accepted_op(
            &pool,
            "op-np-part",
            "Network",
            "net-np-part",
            "UpdateNetworkPolicy",
            "2026-01-01T00:00:00Z",
        )
        .await;

        let dir = tempfile::tempdir().unwrap();
        let pattern = dir
            .path()
            .join("agent-{node_id}.sock")
            .to_str()
            .unwrap()
            .to_string();
        let agent_a = network_policy_mock_agent(tonic::Status::ok(""));
        let agent_b = network_policy_mock_agent(tonic::Status::unimplemented(
            "apply_network_policy is not served by this agent version",
        ));
        spawn_mock_lifecycle_agent(&pattern, "node-np-part-a", agent_a.clone());
        spawn_mock_lifecycle_agent(&pattern, "node-np-part-b", agent_b.clone());

        let orchestrator = test_orchestrator(&pool, &pattern);
        orchestrator.tick().await.expect("tick");

        // Both nodes were attempted...
        assert_eq!(
            agent_a.network_policy_calls.lock().unwrap().len(),
            1,
            "the accepting node's dispatch happened"
        );
        assert_eq!(
            agent_b.network_policy_calls.lock().unwrap().len(),
            1,
            "the refusing node was attempted too"
        );
        // ...the op went terminal with the refusal cause...
        let (status, error_code, error_message, retry_count, next_retry_at, completed_at) =
            op_row(&pool, "op-np-part").await;
        assert_eq!(status, "Failed", "an all-refusals fan-out is terminal");
        assert_eq!(error_code.as_deref(), Some("UNSUPPORTED_BY_AGENT"));
        assert_eq!(retry_count, 0);
        assert_eq!(next_retry_at, None);
        assert!(completed_at.is_some());
        // ...and the roll-up names BOTH sets (the #502 discipline).
        let message = error_message.as_deref().unwrap_or_default();
        assert!(
            message.contains("refused by all"),
            "the all-refusals shape is named: {message:?}"
        );
        assert!(
            message.contains("applied on 1 node(s): node-np-part-a"),
            "the applied set is named beside the refusing set: {message:?}"
        );
    }

    /// #355 PR 1: the #378 §7 fast-fail, network-policy leg — an
    /// all-refusals fan-out (every agent answering `unimplemented`,
    /// e.g. a mixed-version fleet before the agent leg rolls out)
    /// goes terminal `Failed` / `UNSUPPORTED_BY_AGENT` on the first
    /// dispatch with no retry, exactly like the volume carriers.
    #[tokio::test]
    async fn update_network_policy_refusal_fails_fast_without_retry() {
        let pool = create_test_pool().await;
        seed_network_policy_fixture(
            &pool,
            "net-np-ref",
            Some(r#"[{"direction":"ingress","action":"accept","protocol":"icmp"}]"#),
            2,
        )
        .await;
        seed_vm_with_nic(&pool, "vm-np-ref", "node-np-ref", "net-np-ref", None).await;
        seed_accepted_op(
            &pool,
            "op-np-ref",
            "Network",
            "net-np-ref",
            "UpdateNetworkPolicy",
            "2026-01-01T00:00:00Z",
        )
        .await;

        let dir = tempfile::tempdir().unwrap();
        let pattern = dir
            .path()
            .join("agent-{node_id}.sock")
            .to_str()
            .unwrap()
            .to_string();
        let agent = MockLifecycleAgent {
            snapshot_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_status: tonic::Status::unimplemented(""),
            snapshot_vm_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_vm_status: tonic::Status::unimplemented(""),
            overlay_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            overlay_status_by_node: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            attach_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_status: tonic::Status::unimplemented(
                "apply_network_policy is not served by this agent version",
            ),
            create_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_status: tonic::Status::ok(""),
            create_status: tonic::Status::ok(""),
        };
        spawn_mock_lifecycle_agent(&pattern, "node-np-ref", agent.clone());

        let orchestrator = test_orchestrator(&pool, &pattern);

        // First (and only) dispatch: the op must go terminal here.
        orchestrator.tick().await.expect("tick 1");
        let (status, error_code, error_message, retry_count, next_retry_at, completed_at) =
            op_row(&pool, "op-np-ref").await;
        assert_eq!(
            status, "Failed",
            "Unimplemented is terminal on first dispatch"
        );
        assert_eq!(
            error_code.as_deref(),
            Some("UNSUPPORTED_BY_AGENT"),
            "the error code must name the cause"
        );
        assert!(
            error_message
                .as_deref()
                .unwrap_or_default()
                .contains("refused by all"),
            "the roll-up must name the all-refusals shape: {error_message:?}"
        );
        assert_eq!(retry_count, 0, "no retry may be scheduled");
        assert_eq!(next_retry_at, None, "mark_for_retry must never run");
        assert!(
            completed_at.is_some(),
            "the terminal write stamps completed_at"
        );

        // Further ticks must not resurrect the terminal row or re-dispatch.
        orchestrator.tick().await.expect("tick 2");
        orchestrator.tick().await.expect("tick 3");
        assert_eq!(
            agent.network_policy_calls.lock().unwrap().len(),
            1,
            "exactly one agent dispatch across all ticks"
        );
        let (status, _, _, retry_count, next_retry_at, _) = op_row(&pool, "op-np-ref").await;
        assert_eq!(status, "Failed", "the terminal row stays terminal");
        assert_eq!(retry_count, 0);
        assert_eq!(next_retry_at, None);
    }

    // ============================================================
    // #368 P2 — bounded re-drive of terminally-failed creates
    // (design §7 CP rows; see docs/design/issue-368-journaled-create-redrive.md)
    // ============================================================

    /// Seed the #368 zombie shape's observed half: the agent-reported
    /// state says the create terminally failed with a Core failure code
    /// (#368 P1 telemetry), while the desired state still demands the VM.
    async fn seed_failed_observed_state(
        pool: &StorePool,
        vm_id: &str,
        node_id: &str,
        failure_code: &str,
    ) {
        seed_vm(pool, vm_id, node_id).await;
        sqlx::query(
            "INSERT INTO vm_observed_state \
             (vm_id, observed_generation, runtime_status, node_id, last_error) \
             VALUES (?, 0, 'Failed', ?, ?)",
        )
        .bind(vm_id)
        .bind(node_id)
        .bind(failure_code)
        .execute(pool)
        .await
        .expect("insert vm_observed_state");
    }

    /// Seed the journaled ORIGINAL create operation for a VM (the row
    /// the re-drive derives its generation from), in the given status.
    async fn seed_journaled_create(
        pool: &StorePool,
        vm_id: &str,
        create_generation: i64,
        status: &str,
        error_code: &str,
    ) {
        sqlx::query(
            "INSERT INTO operations \
             (operation_id, idempotency_key, resource_kind, resource_id, operation_type, status, \
              desired_generation, error_code, requested_at, updated_at) \
             VALUES (?, ?, 'vm', ?, 'create', ?, ?, ?, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .bind(format!("op-create-{vm_id}"))
        .bind(format!("idem-create-{vm_id}"))
        .bind(vm_id)
        .bind(status)
        .bind(create_generation)
        .bind(error_code)
        .execute(pool)
        .await
        .expect("insert original create operation");
    }

    /// Seed one already-issued `RecreateVm` operation (a prior re-drive
    /// attempt) in the given status with the given `updated_at` (the
    /// backoff anchor).
    async fn seed_recreate_op(
        pool: &StorePool,
        vm_id: &str,
        create_generation: i64,
        attempt: i64,
        status: &str,
        updated_at: &str,
    ) {
        sqlx::query(
            "INSERT INTO operations \
             (operation_id, idempotency_key, resource_kind, resource_id, operation_type, status, \
              desired_generation, requested_at, updated_at) \
             VALUES (?, ?, 'vm', ?, 'RecreateVm', ?, ?, '2026-01-01T00:00:00Z', ?)",
        )
        .bind(format!("op-recreate-{vm_id}-{attempt}"))
        .bind(format!("recreate:{vm_id}:{create_generation}:{attempt}"))
        .bind(vm_id)
        .bind(status)
        .bind(create_generation)
        .bind(updated_at)
        .execute(pool)
        .await
        .expect("insert prior re-drive operation");
    }

    /// All `RecreateVm` operation rows for a VM, as
    /// (idempotency_key, status, error_code, correlation_id,
    /// desired_generation).
    async fn recreate_op_rows(
        pool: &StorePool,
        vm_id: &str,
    ) -> Vec<(String, String, Option<String>, Option<String>, Option<i64>)> {
        sqlx::query_as(
            "SELECT idempotency_key, status, error_code, correlation_id, desired_generation \
             FROM operations \
             WHERE resource_kind = 'vm' AND resource_id = ? AND operation_type = 'RecreateVm'",
        )
        .bind(vm_id)
        .fetch_all(pool)
        .await
        .expect("select RecreateVm operations")
    }

    fn test_orchestrator(pool: &StorePool, socket_pattern: &str) -> Orchestrator {
        Orchestrator::new(
            pool.clone(),
            OperationRepository::new(pool.clone()),
            socket_pattern.to_string(),
            "/kernel".to_string(),
            String::new(),
            NodeClientPool::new(),
            crate::convergence_metrics::new_shared(),
        )
    }

    /// §7 CP row "re-drive happy path": the #368 zombie shape (desired
    /// state still demands the VM, observed state reports the create
    /// terminally failed, the original create is journaled terminal)
    /// is selected and re-issued as a NEW `RecreateVm` operation at the
    /// ORIGINAL create's generation — never a resurrection of the
    /// failed op. A VM that never hit a terminally-failed create is not
    /// touched.
    #[tokio::test]
    async fn redrive_failed_create_issues_new_recreate_operation() {
        let pool = create_test_pool().await;
        seed_node(&pool, "node-a").await;
        seed_failed_observed_state(&pool, "vm-zombie", "node-a", "BACKEND_IO_ERROR").await;
        seed_journaled_create(&pool, "vm-zombie", 3, "Failed", "BACKEND_IO_ERROR").await;
        // Control: a healthy VM must not be re-driven.
        seed_vm(&pool, "vm-healthy", "node-a").await;
        sqlx::query(
            "INSERT INTO vm_observed_state \
             (vm_id, observed_generation, runtime_status, node_id) \
             VALUES ('vm-healthy', 3, 'Running', 'node-a')",
        )
        .execute(&pool)
        .await
        .expect("insert healthy observed state");

        test_orchestrator(&pool, "")
            .redrive_failed_creates()
            .await
            .expect("re-drive pass");

        let rows = recreate_op_rows(&pool, "vm-zombie").await;
        assert_eq!(rows.len(), 1, "exactly one re-drive operation: {rows:?}");
        assert_eq!(rows[0].0, "recreate:vm-zombie:3:1");
        assert_eq!(rows[0].1, "Accepted");
        assert_eq!(
            rows[0].4,
            Some(3),
            "the re-drive re-issues the ORIGINAL create's generation"
        );
        assert_eq!(rows[0].3.as_deref(), Some("redrive-attempt=1"));

        assert!(
            recreate_op_rows(&pool, "vm-healthy").await.is_empty(),
            "VMs that never hit a terminally-failed create are not re-driven"
        );
        // The failed op stays terminal: the original create was not
        // resurrected or mutated.
        let original: (String,) = sqlx::query_as(
            "SELECT status FROM operations WHERE idempotency_key = 'idem-create-vm-zombie'",
        )
        .fetch_one(&pool)
        .await
        .expect("original create op");
        assert_eq!(original.0, "Failed", "failed ops stay terminal");
    }

    /// §7 CP row "operator delete during re-drive stops the loop": a
    /// `Deleted` desired power state (the operator-delete intent,
    /// persisted before any delete dispatch) means the desired state no
    /// longer demands the VM — nothing is re-driven.
    #[tokio::test]
    async fn redrive_refuses_operator_deleted_vm() {
        let pool = create_test_pool().await;
        seed_node(&pool, "node-a").await;
        seed_failed_observed_state(&pool, "vm-deleted", "node-a", "BACKEND_IO_ERROR").await;
        seed_journaled_create(&pool, "vm-deleted", 1, "Failed", "BACKEND_IO_ERROR").await;
        sqlx::query("UPDATE vm_desired_state SET desired_power_state = 'Deleted' WHERE vm_id = ?")
            .bind("vm-deleted")
            .execute(&pool)
            .await
            .expect("mark desired state Deleted");

        test_orchestrator(&pool, "")
            .redrive_failed_creates()
            .await
            .expect("re-drive pass");

        assert!(
            recreate_op_rows(&pool, "vm-deleted").await.is_empty(),
            "an operator-deleted VM must not be re-driven"
        );
    }

    /// §7 CP row "in-flight create-family gate": any incomplete
    /// create-family operation (`create`/`CreateVm`/`RecreateVm` in
    /// Accepted/RetryPending/Running) holds the gate closed. This is
    /// also the CP half of crash-during-re-drive safety: a re-drive
    /// crashed mid-dispatch is left Running and stops the loop until the
    /// CP-side op resolves — `reap_stuck_operations` re-queues the stuck
    /// Running row back to Accepted and the dispatch-retry path drives it
    /// to a terminal status (the agent-side requeued create is recovered
    /// separately, by the operator's inspect-required resolve RPC).
    #[tokio::test]
    async fn redrive_refuses_incomplete_create_family_in_flight() {
        let pool = create_test_pool().await;
        seed_node(&pool, "node-a").await;
        let cases = [
            ("create", "Accepted"),
            ("CreateVm", "RetryPending"),
            ("RecreateVm", "Running"),
            ("RecreateVm", "RetryPending"),
        ];
        for (idx, (op_type, status)) in cases.iter().enumerate() {
            let vm_id = format!("vm-inflight-{idx}");
            seed_failed_observed_state(&pool, &vm_id, "node-a", "BACKEND_IO_ERROR").await;
            sqlx::query(
                "INSERT INTO operations \
                 (operation_id, idempotency_key, resource_kind, resource_id, operation_type, \
                  status, desired_generation, requested_at, updated_at) \
                 VALUES (?, ?, 'vm', ?, ?, ?, 1, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            )
            .bind(format!("op-inflight-{idx}"))
            .bind(format!("idem-inflight-{idx}"))
            .bind(&vm_id)
            .bind(op_type)
            .bind(status)
            .execute(&pool)
            .await
            .expect("insert in-flight operation");
        }

        test_orchestrator(&pool, "")
            .redrive_failed_creates()
            .await
            .expect("re-drive pass");

        for (idx, (op_type, _)) in cases.iter().enumerate() {
            let vm_id = format!("vm-inflight-{idx}");
            let expected = if *op_type == "RecreateVm" { 1 } else { 0 };
            assert_eq!(
                recreate_op_rows(&pool, &vm_id).await.len(),
                expected,
                "an incomplete create-family op in flight must hold the gate closed for {vm_id}"
            );
        }
    }

    /// §7 CP row "bounded backoff": re-drives are spaced by the
    /// dispatch-retry backoff curve (10s * 2^(attempt-1)) measured from
    /// the previous re-drive's last update. A fresh prior re-drive is
    /// skipped; once its backoff window has elapsed the next attempt is
    /// issued.
    #[tokio::test]
    async fn redrive_backoff_spaces_attempts() {
        let pool = create_test_pool().await;
        seed_node(&pool, "node-a").await;
        seed_failed_observed_state(&pool, "vm-backoff", "node-a", "BACKEND_IO_ERROR").await;
        seed_journaled_create(&pool, "vm-backoff", 1, "Failed", "BACKEND_IO_ERROR").await;
        // A prior re-drive updated "now" — inside the 10s window.
        seed_recreate_op(
            &pool,
            "vm-backoff",
            1,
            1,
            "Failed",
            &chrono::Utc::now().to_rfc3339(),
        )
        .await;

        test_orchestrator(&pool, "")
            .redrive_failed_creates()
            .await
            .expect("re-drive pass (backoff)");
        assert_eq!(
            recreate_op_rows(&pool, "vm-backoff").await.len(),
            1,
            "a re-drive inside its backoff window must not be re-issued"
        );

        // Elapse the window: backdate the prior re-drive's update.
        sqlx::query(
            "UPDATE operations SET updated_at = '2026-01-01T00:00:00Z' \
                     WHERE idempotency_key = 'recreate:vm-backoff:1:1'",
        )
        .execute(&pool)
        .await
        .expect("backdate prior re-drive");

        test_orchestrator(&pool, "")
            .redrive_failed_creates()
            .await
            .expect("re-drive pass (due)");
        let rows = recreate_op_rows(&pool, "vm-backoff").await;
        assert_eq!(
            rows.len(),
            2,
            "the next attempt is issued once due: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.0 == "recreate:vm-backoff:1:2"),
            "attempt numbering advances: {rows:?}"
        );
    }

    /// §7 CP row "exhaustion → Failed with reason": once
    /// `MAX_DISPATCH_RETRIES` re-drives have been issued, the pass
    /// marks the VM visibly Failed via a terminal `RecreateVm`
    /// operation carrying `CREATE_REDRIVE_EXHAUSTED` and the reported
    /// Core failure code — never a silent zombie. The fixed marker key
    /// converges repeat passes on the one row.
    #[tokio::test]
    async fn redrive_exhaustion_marks_vm_failed_with_reason() {
        let pool = create_test_pool().await;
        seed_node(&pool, "node-a").await;
        seed_failed_observed_state(&pool, "vm-exhausted", "node-a", "STORD_ATTACH_FAILED").await;
        seed_journaled_create(&pool, "vm-exhausted", 2, "Failed", "STORD_ATTACH_FAILED").await;
        for attempt in 1..=i64::from(MAX_DISPATCH_RETRIES) {
            seed_recreate_op(
                &pool,
                "vm-exhausted",
                2,
                attempt,
                "Failed",
                "2026-01-01T00:00:00Z",
            )
            .await;
        }

        test_orchestrator(&pool, "")
            .redrive_failed_creates()
            .await
            .expect("re-drive pass (exhausted)");

        let rows = recreate_op_rows(&pool, "vm-exhausted").await;
        let marker = rows
            .iter()
            .find(|r| r.0 == "recreate:vm-exhausted:2:exhausted")
            .unwrap_or_else(|| panic!("exhaustion marker must exist: {rows:?}"));
        assert_eq!(marker.1, "Failed", "the marker is terminal Failed");
        assert_eq!(
            marker.2.as_deref(),
            Some("CREATE_REDRIVE_EXHAUSTED"),
            "the marker carries the exhaustion code"
        );
        assert!(
            rows.iter().all(|r| r.0 != "recreate:vm-exhausted:2:4"),
            "no attempt beyond the bound is issued"
        );
        let marker_message: String = sqlx::query_scalar(
            "SELECT error_message FROM operations \
             WHERE idempotency_key = 'recreate:vm-exhausted:2:exhausted'",
        )
        .fetch_one(&pool)
        .await
        .expect("marker error_message");
        assert!(
            marker_message.contains("STORD_ATTACH_FAILED"),
            "the marker reports the agent's failure code: {marker_message}"
        );

        // A repeat pass converges on the one marker row (no duplication,
        // no re-marking).
        test_orchestrator(&pool, "")
            .redrive_failed_creates()
            .await
            .expect("repeat re-drive pass");
        assert_eq!(
            recreate_op_rows(&pool, "vm-exhausted").await.len(),
            rows.len(),
            "repeat passes must not duplicate the exhaustion marker"
        );
    }

    /// §7 CP refusal arm: a failed VM with no journaled create
    /// operation has no generation to re-issue at — the pass refuses
    /// rather than guessing.
    #[tokio::test]
    async fn redrive_skips_failed_vm_without_journaled_create() {
        let pool = create_test_pool().await;
        seed_node(&pool, "node-a").await;
        seed_failed_observed_state(&pool, "vm-nojournal", "node-a", "BACKEND_IO_ERROR").await;

        test_orchestrator(&pool, "")
            .redrive_failed_creates()
            .await
            .expect("re-drive pass");

        assert!(
            recreate_op_rows(&pool, "vm-nojournal").await.is_empty(),
            "no journaled create to derive a generation from — must not re-drive"
        );
    }

    /// #368 review round 2 — joins the resolve→report→re-drive path across
    /// the CP seam. The agent half is pinned in chv-agent-core
    /// (`resolve_inspect_required_as_failed_reports_failed_state_for_redrive`):
    /// after the operator resolves a crash-interrupted re-drive as Failed,
    /// P1 reports the VM as `runtime_status="Failed"` with
    /// `last_error="OPERATOR_RESOLUTION"` and `observed_generation="0"`.
    /// This test feeds that EXACT reported state through the REAL
    /// telemetry ingestion (`TelemetryServiceImplementation::report_vm_state`
    /// — the same `vm_observed_state` upsert the gRPC server performs) and
    /// asserts `redrive_failed_creates` re-arms: while the crashed re-drive
    /// is unresolved the agent reports `Pending` (selection closed), and
    /// the Failed report issues the next attempt.
    #[tokio::test]
    async fn redrive_rearms_after_operator_resolves_inspect_required() {
        use crate::telemetry::{TelemetryService, TelemetryServiceImplementation};
        use chv_controlplane_store::{
            AlertRepository, EventRepository, NodeRepository, ObservedStateRepository,
        };

        let pool = create_test_pool().await;
        seed_node(&pool, "node-a").await;
        seed_vm(&pool, "vm-crash", "node-a").await;
        seed_journaled_create(&pool, "vm-crash", 1, "Failed", "RUNTIME_UNAVAILABLE").await;
        // The first re-drive was dispatched and acked ok at submit level
        // (the CP's view of the re-drive the agent later crashed on); it
        // is terminal here, so the incomplete-create-family gate is open.
        seed_recreate_op(&pool, "vm-crash", 1, 1, "Succeeded", "2026-01-01T00:00:00Z").await;

        // The real telemetry ingestion path — the same upsert the CP's
        // gRPC server performs for the agent's VmStateReport.
        let telemetry = TelemetryServiceImplementation::new(
            NodeRepository::new(pool.clone()),
            ObservedStateRepository::new(pool.clone()),
            EventRepository::new(pool.clone()),
            AlertRepository::new(pool.clone()),
        );
        let report =
            |runtime_status: &'static str, last_error: &'static str| proto::VmStateReport {
                node_id: "node-a".to_string(),
                vm_id: "vm-crash".to_string(),
                runtime_status: runtime_status.to_string(),
                observed_generation: "0".to_string(),
                health_status: "Unknown".to_string(),
                last_error: last_error.to_string(),
                reported_unix_ms: 1_759_000_000_000,
                ..Default::default()
            };

        // Crash window: the agent's re-drive is inspect-required, so P1
        // reports Pending (never the desired-state phantom) — the
        // re-drive pass must not select the VM.
        telemetry
            .report_vm_state(report("Pending", ""))
            .await
            .expect("telemetry report (inspect-required window)");
        test_orchestrator(&pool, "")
            .redrive_failed_creates()
            .await
            .expect("re-drive pass (crash window)");
        assert_eq!(
            recreate_op_rows(&pool, "vm-crash").await.len(),
            1,
            "an inspect-required (Pending-reported) VM must not be re-driven"
        );

        // The operator resolves the inspect-required re-drive as Failed →
        // P1 reports Failed with the resolution code (the exact shape the
        // agent-tier counterpart test produces).
        telemetry
            .report_vm_state(report("Failed", "OPERATOR_RESOLUTION"))
            .await
            .expect("telemetry report (post-resolution)");

        // The ingestion path itself is pinned: the reported state is what
        // the selection keys on.
        let (observed_status, observed_error): (String, Option<String>) = sqlx::query_as(
            "SELECT runtime_status, last_error FROM vm_observed_state WHERE vm_id = 'vm-crash'",
        )
        .fetch_one(&pool)
        .await
        .expect("observed state row");
        assert_eq!(observed_status, "Failed");
        assert_eq!(observed_error.as_deref(), Some("OPERATOR_RESOLUTION"));

        // Re-armed: the next attempt issues (attempt+1).
        test_orchestrator(&pool, "")
            .redrive_failed_creates()
            .await
            .expect("re-drive pass (re-armed)");
        let rows = recreate_op_rows(&pool, "vm-crash").await;
        assert_eq!(rows.len(), 2, "the re-drive must re-arm: {rows:?}");
        let attempt2 = rows
            .iter()
            .find(|r| r.0 == "recreate:vm-crash:1:2")
            .unwrap_or_else(|| panic!("attempt 2 must be issued: {rows:?}"));
        assert_eq!(attempt2.1, "Accepted");
        assert_eq!(attempt2.3.as_deref(), Some("redrive-attempt=2"));
        assert_eq!(
            attempt2.4,
            Some(1),
            "the re-issued attempt fences on the original create's generation"
        );
    }

    /// Mock ReconcileService agent for the #368 end-to-end dispatch
    /// test: records every ApplyVmDesiredState it receives and acks ok.
    #[derive(Clone, Default)]
    struct MockReconcileAgent {
        vm_applies: std::sync::Arc<std::sync::Mutex<Vec<proto::ApplyVmDesiredStateRequest>>>,
    }

    fn mock_ok_ack(operation_id: &str) -> proto::AckResponse {
        proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: operation_id.to_string(),
                status: "ok".into(),
                node_observed_generation: String::new(),
                error_code: String::new(),
                human_summary: String::new(),
            }),
        }
    }

    #[tonic::async_trait]
    impl proto::reconcile_service_server::ReconcileService for MockReconcileAgent {
        async fn apply_node_desired_state(
            &self,
            _request: tonic::Request<proto::ApplyNodeDesiredStateRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Ok(tonic::Response::new(mock_ok_ack("")))
        }

        async fn apply_vm_desired_state(
            &self,
            request: tonic::Request<proto::ApplyVmDesiredStateRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            let inner = request.into_inner();
            let operation_id = inner
                .meta
                .as_ref()
                .map(|m| m.operation_id.clone())
                .unwrap_or_default();
            self.vm_applies.lock().unwrap().push(inner);
            Ok(tonic::Response::new(mock_ok_ack(&operation_id)))
        }

        async fn apply_volume_desired_state(
            &self,
            _request: tonic::Request<proto::ApplyVolumeDesiredStateRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Ok(tonic::Response::new(mock_ok_ack("")))
        }

        async fn apply_network_desired_state(
            &self,
            _request: tonic::Request<proto::ApplyNetworkDesiredStateRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Ok(tonic::Response::new(mock_ok_ack("")))
        }

        async fn acknowledge_desired_state_version(
            &self,
            _request: tonic::Request<proto::AcknowledgeDesiredStateVersionRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Ok(tonic::Response::new(mock_ok_ack("")))
        }
    }

    /// §7 CP row "re-drive end-to-end at mock tier": a failed create is
    /// re-driven through `tick()` and the re-drive dispatches to the
    /// agent as ApplyVmDesiredState — the same desired-state path a
    /// fresh create takes — fenced on the ORIGINAL create's generation.
    /// The agent's ok ack converges the re-drive operation.
    #[tokio::test]
    async fn recreate_vm_dispatches_via_apply_vm_desired_state() {
        let pool = create_test_pool().await;
        seed_node(&pool, "node-redrive").await;
        seed_failed_observed_state(&pool, "vm-e2e", "node-redrive", "BACKEND_IO_ERROR").await;
        seed_journaled_create(&pool, "vm-e2e", 3, "Failed", "BACKEND_IO_ERROR").await;

        let dir = tempfile::tempdir().unwrap();
        let pattern = dir
            .path()
            .join("agent-{node_id}.sock")
            .to_str()
            .unwrap()
            .to_string();
        let agent = MockReconcileAgent::default();
        {
            let socket = pattern.replace("{node_id}", "node-redrive");
            let uds = tokio::net::UnixListener::bind(&socket).unwrap();
            let service =
                proto::reconcile_service_server::ReconcileServiceServer::new(agent.clone());
            tokio::spawn(async move {
                tonic::transport::Server::builder()
                    .add_service(service)
                    .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds))
                    .await
                    .ok();
            });
        }

        let orchestrator = test_orchestrator(&pool, &pattern);

        // Tick 1: nothing Accepted to dispatch yet; the tail pass issues
        // the re-drive.
        orchestrator.tick().await.expect("tick 1");
        let rows = recreate_op_rows(&pool, "vm-e2e").await;
        assert_eq!(rows.len(), 1, "one re-drive issued: {rows:?}");
        assert_eq!(rows[0].1, "Accepted");
        let redrive_op_id: String = sqlx::query_scalar(
            "SELECT operation_id FROM operations WHERE idempotency_key = 'recreate:vm-e2e:3:1'",
        )
        .fetch_one(&pool)
        .await
        .expect("re-drive operation id");

        // Tick 2: claims the RecreateVm op and dispatches it to the agent
        // through the create arm.
        orchestrator.tick().await.expect("tick 2");

        let applies = agent.vm_applies.lock().unwrap().clone();
        assert_eq!(applies.len(), 1, "exactly one agent dispatch: {applies:?}");
        let apply = &applies[0];
        assert_eq!(apply.vm_id, "vm-e2e");
        assert_eq!(apply.node_id, "node-redrive");
        let meta = apply.meta.as_ref().expect("request meta");
        assert_eq!(meta.operation_id, redrive_op_id);
        assert_eq!(
            meta.desired_state_version, "3",
            "the re-drive fences on the ORIGINAL create's generation"
        );
        let fragment = apply.fragment.as_ref().expect("desired-state fragment");
        assert_eq!(fragment.kind, "vm");
        assert_eq!(fragment.generation, "3");
        let spec: serde_json::Value =
            serde_json::from_slice(&fragment.spec_json).expect("agent vm spec json");
        assert_eq!(
            spec["name"].as_str(),
            Some("VM vm-e2e"),
            "the re-drive carries the full agent spec: {spec}"
        );

        // The ok ack converged the re-drive operation, and the tail pass
        // did not double-issue (backoff).
        let status: String =
            sqlx::query_scalar("SELECT status FROM operations WHERE operation_id = ?")
                .bind(&redrive_op_id)
                .fetch_one(&pool)
                .await
                .expect("re-drive status");
        assert_eq!(status, "Succeeded");
        assert_eq!(
            recreate_op_rows(&pool, "vm-e2e").await.len(),
            1,
            "no second re-drive inside the backoff window"
        );
    }

    // ============================================================
    // #378 §7 — Unimplemented dispatch fast-fail (no retry, no
    // mark_for_retry resurrection of the terminal Failed row)
    // ============================================================

    /// Mock agent-side LifecycleService served over a real UDS socket:
    /// records every `SnapshotVolume` / `SnapshotVm` / `UpdateOverlay`
    /// request and answers it with the configured tonic status; every
    /// other RPC fails closed. Driving the real tonic client against this
    /// socket means the tests pin the full identity-preservation path:
    /// server status → `with_timeout` mapping → `ChvError` variant →
    /// orchestrator classification. `UpdateOverlay` is answered per node
    /// (`overlay_status_by_node`); a node with no entry gets the OK ack,
    /// so the same mock serves the fabric fan-out legs.
    #[derive(Clone)]
    struct MockLifecycleAgent {
        snapshot_calls: std::sync::Arc<std::sync::Mutex<Vec<proto::SnapshotVolumeRequest>>>,
        snapshot_status: tonic::Status,
        snapshot_vm_calls: std::sync::Arc<std::sync::Mutex<Vec<proto::SnapshotVmRequest>>>,
        snapshot_vm_status: tonic::Status,
        overlay_calls: std::sync::Arc<std::sync::Mutex<Vec<proto::UpdateOverlayRequest>>>,
        overlay_status_by_node:
            std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, tonic::Status>>>,
        /// #379 PR 2 (A8): every AttachVolume request, answered with the
        /// OK ack so the dispatch converges; tests assert the
        /// `volume_spec_json` the CP threaded.
        attach_calls: std::sync::Arc<std::sync::Mutex<Vec<proto::AttachVolumeRequest>>>,
        /// #513 PR 1: every CreateVolume request, answered with
        /// `create_status` (OK by default) so tests can pin the
        /// size/class the CP threaded — and the terminal fast-fail on a
        /// refusal (the core-managed posture).
        create_calls: std::sync::Arc<std::sync::Mutex<Vec<proto::CreateVolumeRequest>>>,
        create_status: tonic::Status,
        /// #522 PR 1: every DeleteVolume request, answered with
        /// `delete_status` (OK by default) so tests can pin the
        /// volume_id/class the CP threaded — and the terminal
        /// fast-fail on a refusal (the core-managed posture).
        delete_calls: std::sync::Arc<std::sync::Mutex<Vec<proto::DeleteVolumeRequest>>>,
        delete_status: tonic::Status,
        /// #355 PR 1: every ApplyNetworkPolicy request, answered with
        /// `network_policy_status` (OK by default) so tests can pin the
        /// network_id/policy bytes the CP threaded — and the fan-out
        /// roll-ups (all-refusals terminal, mixed retry, no-op shapes).
        network_policy_calls:
            std::sync::Arc<std::sync::Mutex<Vec<proto::ApplyNetworkPolicyRequest>>>,
        network_policy_status: tonic::Status,
    }

    #[tonic::async_trait]
    impl proto::lifecycle_service_server::LifecycleService for MockLifecycleAgent {
        async fn snapshot_volume(
            &self,
            request: tonic::Request<proto::SnapshotVolumeRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            self.snapshot_calls
                .lock()
                .unwrap()
                .push(request.into_inner());
            Err(self.snapshot_status.clone())
        }

        async fn create_vm(
            &self,
            _request: tonic::Request<proto::CreateVmRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn create_volume(
            &self,
            request: tonic::Request<proto::CreateVolumeRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            let inner = request.into_inner();
            let op_id = inner
                .meta
                .as_ref()
                .map(|m| m.operation_id.clone())
                .unwrap_or_default();
            self.create_calls.lock().unwrap().push(inner);
            if self.create_status.code() != tonic::Code::Ok {
                return Err(self.create_status.clone());
            }
            Ok(tonic::Response::new(proto::AckResponse {
                result: Some(proto::ResultMeta {
                    operation_id: op_id,
                    status: "ok".to_string(),
                    node_observed_generation: "1".to_string(),
                    error_code: "".to_string(),
                    human_summary: "volume created".to_string(),
                }),
            }))
        }

        async fn start_vm(
            &self,
            _request: tonic::Request<proto::StartVmRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn stop_vm(
            &self,
            _request: tonic::Request<proto::StopVmRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn reboot_vm(
            &self,
            _request: tonic::Request<proto::RebootVmRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn delete_vm(
            &self,
            _request: tonic::Request<proto::DeleteVmRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn delete_volume(
            &self,
            request: tonic::Request<proto::DeleteVolumeRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            let inner = request.into_inner();
            let op_id = inner
                .meta
                .as_ref()
                .map(|m| m.operation_id.clone())
                .unwrap_or_default();
            self.delete_calls.lock().unwrap().push(inner);
            if self.delete_status.code() != tonic::Code::Ok {
                return Err(self.delete_status.clone());
            }
            Ok(tonic::Response::new(proto::AckResponse {
                result: Some(proto::ResultMeta {
                    operation_id: op_id,
                    status: "ok".to_string(),
                    node_observed_generation: "1".to_string(),
                    error_code: "".to_string(),
                    human_summary: "volume deleted".to_string(),
                }),
            }))
        }

        async fn apply_network_policy(
            &self,
            request: tonic::Request<proto::ApplyNetworkPolicyRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            let inner = request.into_inner();
            let op_id = inner
                .meta
                .as_ref()
                .map(|m| m.operation_id.clone())
                .unwrap_or_default();
            self.network_policy_calls.lock().unwrap().push(inner);
            if self.network_policy_status.code() != tonic::Code::Ok {
                return Err(self.network_policy_status.clone());
            }
            Ok(tonic::Response::new(proto::AckResponse {
                result: Some(proto::ResultMeta {
                    operation_id: op_id,
                    status: "ok".to_string(),
                    node_observed_generation: "1".to_string(),
                    error_code: "".to_string(),
                    human_summary: "network policy applied".to_string(),
                }),
            }))
        }

        async fn resize_vm(
            &self,
            _request: tonic::Request<proto::ResizeVmRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn pause_vm(
            &self,
            _request: tonic::Request<proto::PauseVmRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn resume_vm(
            &self,
            _request: tonic::Request<proto::ResumeVmRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn power_button_vm(
            &self,
            _request: tonic::Request<proto::PowerButtonVmRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn attach_volume(
            &self,
            request: tonic::Request<proto::AttachVolumeRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            let inner = request.into_inner();
            let op_id = inner
                .meta
                .as_ref()
                .map(|m| m.operation_id.clone())
                .unwrap_or_default();
            self.attach_calls.lock().unwrap().push(inner);
            Ok(tonic::Response::new(proto::AckResponse {
                result: Some(proto::ResultMeta {
                    operation_id: op_id,
                    status: "ok".to_string(),
                    node_observed_generation: "1".to_string(),
                    error_code: "".to_string(),
                    human_summary: "volume attached".to_string(),
                }),
            }))
        }

        async fn detach_volume(
            &self,
            _request: tonic::Request<proto::DetachVolumeRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn resize_volume(
            &self,
            _request: tonic::Request<proto::ResizeVolumeRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn restore_volume(
            &self,
            _request: tonic::Request<proto::RestoreVolumeRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn delete_volume_snapshot(
            &self,
            _request: tonic::Request<proto::DeleteVolumeSnapshotRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn clone_volume(
            &self,
            _request: tonic::Request<proto::CloneVolumeRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn add_disk(
            &self,
            _request: tonic::Request<proto::AddDiskRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn remove_device(
            &self,
            _request: tonic::Request<proto::RemoveDeviceRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn add_net(
            &self,
            _request: tonic::Request<proto::AddNetRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn resize_disk(
            &self,
            _request: tonic::Request<proto::ResizeDiskRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn snapshot_vm(
            &self,
            request: tonic::Request<proto::SnapshotVmRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            self.snapshot_vm_calls
                .lock()
                .unwrap()
                .push(request.into_inner());
            Err(self.snapshot_vm_status.clone())
        }

        async fn restore_snapshot(
            &self,
            _request: tonic::Request<proto::RestoreSnapshotRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn coredump_vm(
            &self,
            _request: tonic::Request<proto::CoredumpVmRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn ping_vmm(
            &self,
            _request: tonic::Request<proto::PingVmmRequest>,
        ) -> Result<tonic::Response<proto::PingVmmResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn pause_node_scheduling(
            &self,
            _request: tonic::Request<proto::PauseNodeSchedulingRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn resume_node_scheduling(
            &self,
            _request: tonic::Request<proto::ResumeNodeSchedulingRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn drain_node(
            &self,
            _request: tonic::Request<proto::DrainNodeRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn enter_maintenance(
            &self,
            _request: tonic::Request<proto::EnterMaintenanceRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn exit_maintenance(
            &self,
            _request: tonic::Request<proto::ExitMaintenanceRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn start_network(
            &self,
            _request: tonic::Request<proto::StartNetworkRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn stop_network(
            &self,
            _request: tonic::Request<proto::StopNetworkRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn restart_network(
            &self,
            _request: tonic::Request<proto::RestartNetworkRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn migrate_vm(
            &self,
            _request: tonic::Request<proto::MigrateVmRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn update_overlay(
            &self,
            request: tonic::Request<proto::UpdateOverlayRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            let inner = request.into_inner();
            let status = self
                .overlay_status_by_node
                .lock()
                .unwrap()
                .get(&inner.node_id)
                .cloned();
            self.overlay_calls.lock().unwrap().push(inner);
            match status {
                Some(status) => Err(status),
                None => Ok(tonic::Response::new(proto::AckResponse {
                    result: Some(proto::ResultMeta {
                        operation_id: String::new(),
                        status: "OK".into(),
                        node_observed_generation: String::new(),
                        error_code: String::new(),
                        human_summary: "fabric plan applied".into(),
                    }),
                })),
            }
        }

        async fn send_gratuitous_arp(
            &self,
            _request: tonic::Request<proto::SendGratuitousArpRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn resolve_inspect_required_operation(
            &self,
            _request: tonic::Request<proto::ResolveInspectRequiredOperationRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }
    }

    /// Seed the volume-op shape the fast-fail tests dispatch: a node, a
    /// volume on it (the claim query resolves the dispatch node from
    /// `volumes.node_id`), and an Accepted `SnapshotVolume` operation.
    async fn seed_snapshot_op(pool: &StorePool, node_id: &str, volume_id: &str, op_id: &str) {
        seed_node(pool, node_id).await;
        seed_volume(pool, volume_id, node_id).await;
        seed_accepted_op(
            pool,
            op_id,
            "Volume",
            volume_id,
            "SnapshotVolume",
            "2026-01-01T00:00:00Z",
        )
        .await;
    }

    /// Seed the VM-snapshot shape the second fast-fail test dispatches:
    /// a node, a VM placed on it (the claim query resolves the dispatch
    /// node from `vm_desired_state.target_node_id`), and an Accepted
    /// `SnapshotVm` operation.
    async fn seed_snapshot_vm_op(pool: &StorePool, node_id: &str, vm_id: &str, op_id: &str) {
        seed_node(pool, node_id).await;
        seed_vm(pool, vm_id, node_id).await;
        seed_accepted_op(
            pool,
            op_id,
            "vm",
            vm_id,
            "SnapshotVm",
            "2026-01-01T00:00:00Z",
        )
        .await;
    }

    /// The operations row as the ops surface reads it: status, error
    /// code, error message, retry bookkeeping, and terminal timestamp.
    #[allow(clippy::type_complexity)]
    async fn op_row(
        pool: &StorePool,
        op_id: &str,
    ) -> (
        String,
        Option<String>,
        Option<String>,
        i32,
        Option<String>,
        Option<String>,
    ) {
        sqlx::query_as(
            "SELECT status, error_code, error_message, retry_count, next_retry_at, completed_at \
             FROM operations WHERE operation_id = ?",
        )
        .bind(op_id)
        .fetch_one(pool)
        .await
        .expect("operations row")
    }

    /// Serve a mock lifecycle agent on a UDS socket matching `pattern`
    /// for `node_id`.
    fn spawn_mock_lifecycle_agent(pattern: &str, node_id: &str, agent: MockLifecycleAgent) {
        let socket = pattern.replace("{node_id}", node_id);
        let uds = tokio::net::UnixListener::bind(&socket).unwrap();
        let service = proto::lifecycle_service_server::LifecycleServiceServer::new(agent);
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(service)
                .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds))
                .await
                .ok();
        });
    }

    /// §7 fast-fail, the pinned behavior: an agent answering gRPC
    /// `Unimplemented` (here: a core-managed node's fail-closed
    /// `snapshot_volume` gate) sends the operation terminal on the
    /// FIRST dispatch — `Failed` with the cause-naming
    /// `UNSUPPORTED_BY_AGENT` code carrying the agent's refusal text —
    /// with zero retries and no `mark_for_retry` resurrection of the
    /// terminal row (retry_count stays 0, no `next_retry_at` is ever
    /// scheduled, and the agent sees exactly one request).
    #[tokio::test]
    async fn unimplemented_dispatch_fails_fast_without_retry() {
        let pool = create_test_pool().await;
        seed_snapshot_op(&pool, "node-ff", "vol-ff", "op-ff-1").await;

        let dir = tempfile::tempdir().unwrap();
        let pattern = dir
            .path()
            .join("agent-{node_id}.sock")
            .to_str()
            .unwrap()
            .to_string();
        let agent = MockLifecycleAgent {
            snapshot_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_status: tonic::Status::unimplemented(
                "snapshot_volume is unsupported in core-managed mode",
            ),
            snapshot_vm_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_vm_status: tonic::Status::unimplemented(""),
            overlay_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            overlay_status_by_node: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            attach_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_status: tonic::Status::ok(""),
            create_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_status: tonic::Status::ok(""),
            create_status: tonic::Status::ok(""),
        };
        spawn_mock_lifecycle_agent(&pattern, "node-ff", agent.clone());

        let orchestrator = test_orchestrator(&pool, &pattern);

        // First (and only) dispatch: the op must go terminal here.
        orchestrator.tick().await.expect("tick 1");

        let (status, error_code, error_message, retry_count, next_retry_at, completed_at) =
            op_row(&pool, "op-ff-1").await;
        assert_eq!(
            status, "Failed",
            "Unimplemented is terminal on first dispatch"
        );
        assert_eq!(
            error_code.as_deref(),
            Some("UNSUPPORTED_BY_AGENT"),
            "the error code must name the cause"
        );
        assert!(
            error_message
                .as_deref()
                .unwrap_or_default()
                .contains("snapshot_volume is unsupported in core-managed mode"),
            "the agent's refusal text must ride the error message: {error_message:?}"
        );
        assert_eq!(
            retry_count, 0,
            "no retry may be scheduled for a terminal-class error"
        );
        assert_eq!(
            next_retry_at, None,
            "mark_for_retry must never run: no next_retry_at may be written"
        );
        assert!(
            completed_at.is_some(),
            "the terminal write stamps completed_at"
        );

        // Further ticks must not resurrect the terminal row (the
        // mark_for_retry UPDATE has no status guard — the bypass in the
        // tick error handler is what prevents the Failed → RetryPending
        // flip) and must not re-dispatch.
        orchestrator.tick().await.expect("tick 2");
        orchestrator.tick().await.expect("tick 3");
        assert_eq!(
            agent.snapshot_calls.lock().unwrap().len(),
            1,
            "exactly one agent dispatch across all ticks"
        );
        let (status, _, _, retry_count, next_retry_at, _) = op_row(&pool, "op-ff-1").await;
        assert_eq!(status, "Failed", "the terminal row stays terminal");
        assert_eq!(retry_count, 0);
        assert_eq!(next_retry_at, None);
    }

    /// §7 fast-fail pinned on a second, NON-volume surface: `SnapshotVm`
    /// — another simple fail-closed core-managed gate in the agent
    /// (`snapshot_vm is unsupported in core-managed mode`, same
    /// core-authority check as the volume surfaces) dispatched through
    /// the identical single-node error arm. Pinning the same properties
    /// on a VM-surface operation proves the fast-fail is a property of
    /// the shared dispatch path, not of the volume surfaces: terminal
    /// `Failed` / `UNSUPPORTED_BY_AGENT` with the agent's refusal text
    /// on the FIRST dispatch, `retry_count` 0, no `next_retry_at` ever
    /// written, exactly one agent request, and no resurrection across
    /// further ticks.
    #[tokio::test]
    async fn unimplemented_snapshot_vm_dispatch_fails_fast_without_retry() {
        let pool = create_test_pool().await;
        seed_snapshot_vm_op(&pool, "node-ff", "vm-ff", "op-ff-4").await;

        let dir = tempfile::tempdir().unwrap();
        let pattern = dir
            .path()
            .join("agent-{node_id}.sock")
            .to_str()
            .unwrap()
            .to_string();
        let agent = MockLifecycleAgent {
            snapshot_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_status: tonic::Status::unimplemented(""),
            snapshot_vm_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_vm_status: tonic::Status::unimplemented(
                "snapshot_vm is unsupported in core-managed mode",
            ),
            overlay_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            overlay_status_by_node: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            attach_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_status: tonic::Status::ok(""),
            create_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_status: tonic::Status::ok(""),
            create_status: tonic::Status::ok(""),
        };
        spawn_mock_lifecycle_agent(&pattern, "node-ff", agent.clone());

        let orchestrator = test_orchestrator(&pool, &pattern);

        // First (and only) dispatch: the op must go terminal here.
        orchestrator.tick().await.expect("tick 1");

        let (status, error_code, error_message, retry_count, next_retry_at, completed_at) =
            op_row(&pool, "op-ff-4").await;
        assert_eq!(
            status, "Failed",
            "Unimplemented is terminal on first dispatch (non-volume surface)"
        );
        assert_eq!(
            error_code.as_deref(),
            Some("UNSUPPORTED_BY_AGENT"),
            "the error code must name the cause on the shared dispatch path"
        );
        assert!(
            error_message
                .as_deref()
                .unwrap_or_default()
                .contains("snapshot_vm is unsupported in core-managed mode"),
            "the agent's refusal text must ride the error message: {error_message:?}"
        );
        assert_eq!(
            retry_count, 0,
            "no retry may be scheduled for a terminal-class error"
        );
        assert_eq!(
            next_retry_at, None,
            "mark_for_retry must never run: no next_retry_at may be written"
        );
        assert!(
            completed_at.is_some(),
            "the terminal write stamps completed_at"
        );

        // Further ticks must not resurrect the terminal row and must
        // not re-dispatch — same pins as the volume-surface test.
        orchestrator.tick().await.expect("tick 2");
        orchestrator.tick().await.expect("tick 3");
        assert_eq!(
            agent.snapshot_vm_calls.lock().unwrap().len(),
            1,
            "exactly one agent dispatch across all ticks"
        );
        let (status, _, _, retry_count, next_retry_at, _) = op_row(&pool, "op-ff-4").await;
        assert_eq!(status, "Failed", "the terminal row stays terminal");
        assert_eq!(retry_count, 0);
        assert_eq!(next_retry_at, None);
    }

    /// Control for the fast-fail: a non-Unimplemented tonic status
    /// (`Unavailable` from the agent) keeps today's retry semantics
    /// byte-for-byte — the dispatch failure write (`Failed` /
    /// `AGENT_REJECTED`) is resurrected to `RetryPending` by the tick
    /// error handler with the 10/20/40 s backoff curve, and exhaustion
    /// still lands on `Failed` / `DISPATCH_FAILED` after exactly
    /// MAX_DISPATCH_RETRIES retries.
    #[tokio::test]
    async fn unavailable_dispatch_retries_exactly_as_before() {
        let pool = create_test_pool().await;
        seed_snapshot_op(&pool, "node-ff", "vol-ff", "op-ff-2").await;

        let dir = tempfile::tempdir().unwrap();
        let pattern = dir
            .path()
            .join("agent-{node_id}.sock")
            .to_str()
            .unwrap()
            .to_string();
        let agent = MockLifecycleAgent {
            snapshot_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_status: tonic::Status::unavailable("agent restarting"),
            snapshot_vm_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_vm_status: tonic::Status::unavailable("agent restarting"),
            overlay_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            overlay_status_by_node: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            attach_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_status: tonic::Status::ok(""),
            create_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_status: tonic::Status::ok(""),
            create_status: tonic::Status::ok(""),
        };
        spawn_mock_lifecycle_agent(&pattern, "node-ff", agent.clone());

        let orchestrator = test_orchestrator(&pool, &pattern);

        // Attempt 1: dispatch fails → Failed/AGENT_REJECTED written by
        // dispatch_operation, then resurrected to RetryPending (retry 1).
        orchestrator.tick().await.expect("tick 1");
        let (status, _, _, retry_count, next_retry_at, _) = op_row(&pool, "op-ff-2").await;
        assert_eq!(
            status, "RetryPending",
            "non-Unimplemented errors keep the retry semantics"
        );
        assert_eq!(retry_count, 1);
        assert!(
            next_retry_at.is_some(),
            "the backoff schedule is written as before"
        );

        // Attempts 2 and 3: backdate the backoff anchor and re-tick.
        for expected_retry in [2, 3] {
            sqlx::query("UPDATE operations SET next_retry_at = '2026-01-01T00:00:00Z' WHERE operation_id = 'op-ff-2'")
                .execute(&pool)
                .await
                .expect("backdate retry anchor");
            orchestrator.tick().await.expect("retry tick");
            let (status, _, _, retry_count, _, _) = op_row(&pool, "op-ff-2").await;
            assert_eq!(status, "RetryPending");
            assert_eq!(retry_count, expected_retry);
        }

        // Attempt 4 exceeds MAX_DISPATCH_RETRIES: terminal
        // Failed/DISPATCH_FAILED with the exhaustion message.
        sqlx::query("UPDATE operations SET next_retry_at = '2026-01-01T00:00:00Z' WHERE operation_id = 'op-ff-2'")
            .execute(&pool)
            .await
            .expect("backdate retry anchor");
        orchestrator.tick().await.expect("exhaustion tick");
        let (status, error_code, error_message, _, _, _) = op_row(&pool, "op-ff-2").await;
        assert_eq!(status, "Failed");
        assert_eq!(error_code.as_deref(), Some("DISPATCH_FAILED"));
        assert!(
            error_message
                .as_deref()
                .unwrap_or_default()
                .contains("permanently failed after 3 retries"),
            "the exhaustion message shape is unchanged: {error_message:?}"
        );

        // The full curve dispatched exactly 1 + MAX_DISPATCH_RETRIES times.
        assert_eq!(
            agent.snapshot_calls.lock().unwrap().len(),
            4,
            "initial attempt plus 3 retries — the retry curve is unchanged"
        );
    }

    /// Control for the fast-fail, transport class: an unreachable agent
    /// (no socket — the `BackendUnavailable` connect failure, which
    /// bypasses `dispatch_operation`'s error arm entirely) is scheduled
    /// for retry exactly as before, with no terminal write.
    #[tokio::test]
    async fn transport_unavailable_dispatch_retries_as_before() {
        let pool = create_test_pool().await;
        seed_snapshot_op(&pool, "node-ff", "vol-ff", "op-ff-3").await;

        // A pattern whose socket is never bound: connect fails with
        // BackendUnavailable, the same class as an agent restart.
        let dir = tempfile::tempdir().unwrap();
        let pattern = dir
            .path()
            .join("agent-{node_id}.sock")
            .to_str()
            .unwrap()
            .to_string();

        let orchestrator = test_orchestrator(&pool, &pattern);
        orchestrator.tick().await.expect("tick 1");

        let (status, error_code, error_message, retry_count, next_retry_at, completed_at) =
            op_row(&pool, "op-ff-3").await;
        assert_eq!(
            status, "RetryPending",
            "transport failures keep the retry semantics"
        );
        assert_eq!(retry_count, 1);
        assert!(
            next_retry_at.is_some(),
            "the backoff schedule is written as before"
        );
        assert_eq!(
            error_code, None,
            "no terminal write happens on the connect-failure path (as before)"
        );
        assert!(
            completed_at.is_none(),
            "the op is not terminal: it is queued for retry"
        );
        assert!(
            error_message
                .as_deref()
                .unwrap_or_default()
                .contains("agent"),
            "the retry row carries the failure text: {error_message:?}"
        );
    }

    /// Seed the two-node fabric cluster an `UpdateOverlay` dispatch fans
    /// out to: a vxlan network at desired generation 1, one VM placement
    /// per participating node (the bounded flood list), and full fabric
    /// identities (the compile is fail-closed without them) — plus the
    /// Accepted `UpdateOverlay` operation itself.
    async fn seed_update_overlay_op(pool: &StorePool, network_id: &str, op_id: &str) {
        let nodes = ["node-ovl-a", "node-ovl-b"];
        for node_id in nodes {
            seed_node(pool, node_id).await;
        }
        sqlx::query(
            "INSERT INTO networks (network_id, node_id, display_name, overlay_type) \
             VALUES (?, ?, ?, 'vxlan')",
        )
        .bind(network_id)
        .bind("node-ovl-a")
        .bind(format!("Net {network_id}"))
        .execute(pool)
        .await
        .expect("insert network");
        sqlx::query(
            "INSERT INTO network_desired_state (network_id, desired_generation) VALUES (?, 1)",
        )
        .bind(network_id)
        .execute(pool)
        .await
        .expect("insert network desired state");
        for (idx, node_id) in nodes.iter().enumerate() {
            let vm_id = format!("vm-ovl-{idx}");
            sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES (?, ?)")
                .bind(&vm_id)
                .bind(format!("VM {vm_id}"))
                .execute(pool)
                .await
                .expect("insert vm");
            sqlx::query(
                "INSERT INTO vm_desired_state (vm_id, desired_generation, target_node_id) \
                 VALUES (?, 1, ?)",
            )
            .bind(&vm_id)
            .bind(node_id)
            .execute(pool)
            .await
            .expect("insert vm desired state");
            sqlx::query(
                "INSERT INTO vm_nic_desired_state (nic_id, vm_id, network_id) VALUES (?, ?, ?)",
            )
            .bind(format!("nic-{vm_id}"))
            .bind(&vm_id)
            .bind(network_id)
            .execute(pool)
            .await
            .expect("insert vm nic desired state");
        }
        let vtep_repo = chv_controlplane_store::VtepRepository::new(pool.clone());
        for (node_id, public_key, endpoint) in [
            (
                "node-ovl-a",
                "pub-aAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
                "10.0.0.1:65001",
            ),
            (
                "node-ovl-b",
                "pub-bBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB=",
                "10.0.0.2:65001",
            ),
        ] {
            vtep_repo
                .register_fabric_identity(node_id, public_key, 0, None)
                .await
                .expect("register fabric identity");
            sqlx::query("UPDATE vtep_registry SET underlay_endpoint = ? WHERE node_id = ?")
                .bind(endpoint)
                .bind(node_id)
                .execute(pool)
                .await
                .expect("set underlay endpoint");
        }
        seed_accepted_op(
            pool,
            op_id,
            "Network",
            network_id,
            "UpdateOverlay",
            "2026-01-01T00:00:00Z",
        )
        .await;
    }

    /// `test_orchestrator` with the overlay manager wired (the
    /// `UpdateOverlay` dispatch arm fails closed without it), sharing the
    /// node client pool the way the service wiring does.
    fn test_orchestrator_with_overlay(pool: &StorePool, socket_pattern: &str) -> Orchestrator {
        let node_pool = NodeClientPool::new();
        Orchestrator::new(
            pool.clone(),
            OperationRepository::new(pool.clone()),
            socket_pattern.to_string(),
            "/kernel".to_string(),
            String::new(),
            node_pool.clone(),
            crate::convergence_metrics::new_shared(),
        )
        .with_overlay_manager(OverlayManager::new(node_pool, socket_pattern.to_string()))
    }

    /// §7 fast-fail, the `UpdateOverlay` fan-out leg: when EVERY
    /// participating node refuses the fabric-plan dispatch with gRPC
    /// `Unimplemented` (the all-core-managed cluster shape), the fan-out
    /// preserves the refusal identity instead of flattening it into a
    /// fresh Internal — the operation goes terminal on the FIRST dispatch
    /// with `Failed` / `UNSUPPORTED_BY_AGENT` carrying the agents'
    /// refusal text, zero retries, no `mark_for_retry` resurrection of
    /// the terminal row, and exactly one `UpdateOverlay` request per
    /// participating node across all ticks.
    #[tokio::test]
    async fn update_overlay_fan_out_all_unimplemented_fails_fast_without_retry() {
        let pool = create_test_pool().await;
        seed_update_overlay_op(&pool, "net-ovl", "op-ovl-1").await;

        let dir = tempfile::tempdir().unwrap();
        let pattern = dir
            .path()
            .join("agent-{node_id}.sock")
            .to_str()
            .unwrap()
            .to_string();
        let refusal =
            || tonic::Status::unimplemented("update_overlay is unsupported in core-managed mode");
        let mut overlay_status_by_node = std::collections::HashMap::new();
        overlay_status_by_node.insert("node-ovl-a".to_string(), refusal());
        overlay_status_by_node.insert("node-ovl-b".to_string(), refusal());
        let agent = MockLifecycleAgent {
            snapshot_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_status: tonic::Status::unimplemented(""),
            snapshot_vm_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_vm_status: tonic::Status::unimplemented(""),
            overlay_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            overlay_status_by_node: std::sync::Arc::new(std::sync::Mutex::new(
                overlay_status_by_node,
            )),
            attach_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_status: tonic::Status::ok(""),
            create_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_status: tonic::Status::ok(""),
            create_status: tonic::Status::ok(""),
        };
        spawn_mock_lifecycle_agent(&pattern, "node-ovl-a", agent.clone());
        spawn_mock_lifecycle_agent(&pattern, "node-ovl-b", agent.clone());

        let orchestrator = test_orchestrator_with_overlay(&pool, &pattern);

        // First (and only) dispatch: the op must go terminal here.
        orchestrator.tick().await.expect("tick 1");

        let (status, error_code, error_message, retry_count, next_retry_at, completed_at) =
            op_row(&pool, "op-ovl-1").await;
        assert_eq!(
            status, "Failed",
            "an all-refusals fan-out is terminal on first dispatch"
        );
        assert_eq!(
            error_code.as_deref(),
            Some("UNSUPPORTED_BY_AGENT"),
            "the error code must name the cause"
        );
        assert!(
            error_message
                .as_deref()
                .unwrap_or_default()
                .contains("update_overlay is unsupported in core-managed mode"),
            "the agents' refusal text must ride the error message: {error_message:?}"
        );
        // #502: the pure all-refusals shape (nothing applied) keeps the
        // roll-up message byte-for-byte — no applied clause is appended.
        assert!(
            error_message
                .as_deref()
                .unwrap_or_default()
                .contains("refused by all 2 failing node(s)"),
            "the all-refusals roll-up shape is unchanged: {error_message:?}"
        );
        assert!(
            !error_message
                .as_deref()
                .unwrap_or_default()
                .contains("applied on"),
            "no applied clause when nothing applied: {error_message:?}"
        );
        assert_eq!(
            retry_count, 0,
            "no retry may be scheduled for a terminal-class error"
        );
        assert_eq!(
            next_retry_at, None,
            "mark_for_retry must never run: no next_retry_at may be written"
        );
        assert!(
            completed_at.is_some(),
            "the terminal write stamps completed_at"
        );
        assert_eq!(
            agent.overlay_calls.lock().unwrap().len(),
            2,
            "the fan-out still attempts every participating node exactly once"
        );

        // Further ticks must not resurrect the terminal row (the
        // mark_for_retry UPDATE has no status guard — the tick's
        // Unimplemented bypass is what prevents the Failed → RetryPending
        // flip) and must not re-dispatch.
        orchestrator.tick().await.expect("tick 2");
        orchestrator.tick().await.expect("tick 3");
        assert_eq!(
            agent.overlay_calls.lock().unwrap().len(),
            2,
            "exactly one dispatch per node across all ticks"
        );
        let (status, _, _, retry_count, next_retry_at, _) = op_row(&pool, "op-ovl-1").await;
        assert_eq!(status, "Failed", "the terminal row stays terminal");
        assert_eq!(retry_count, 0);
        assert_eq!(next_retry_at, None);
    }

    /// #502 partial-success roll-up: a fan-out where SOME participating
    /// nodes apply their fabric plan and the rest refuse with gRPC
    /// `Unimplemented` is still the all-refusals fast-fail shape (every
    /// FAILURE is a refusal), so the operation goes terminal on the FIRST
    /// dispatch with `Failed` / `UNSUPPORTED_BY_AGENT` and zero retries —
    /// but the terminal record now names BOTH sets: the refusing nodes
    /// with their per-node error text AND the applied nodes, so an
    /// operator reading the error message (the #530 surfaces render it
    /// verbatim) can tell partial application happened. Before #502 the
    /// roll-up listed only the refusing nodes.
    #[tokio::test]
    async fn update_overlay_fan_out_partial_success_rollup_names_applied_and_refused_nodes() {
        let pool = create_test_pool().await;
        seed_update_overlay_op(&pool, "net-ovl", "op-ovl-3").await;

        let dir = tempfile::tempdir().unwrap();
        let pattern = dir
            .path()
            .join("agent-{node_id}.sock")
            .to_str()
            .unwrap()
            .to_string();
        // node-ovl-a applies its plan (no entry → Ok ack); node-ovl-b
        // refuses with the core-managed fail-closed gate.
        let mut overlay_status_by_node = std::collections::HashMap::new();
        overlay_status_by_node.insert(
            "node-ovl-b".to_string(),
            tonic::Status::unimplemented("update_overlay is unsupported in core-managed mode"),
        );
        let agent = MockLifecycleAgent {
            snapshot_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_status: tonic::Status::unimplemented(""),
            snapshot_vm_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_vm_status: tonic::Status::unimplemented(""),
            overlay_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            overlay_status_by_node: std::sync::Arc::new(std::sync::Mutex::new(
                overlay_status_by_node,
            )),
            attach_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_status: tonic::Status::ok(""),
            create_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_status: tonic::Status::ok(""),
            create_status: tonic::Status::ok(""),
        };
        spawn_mock_lifecycle_agent(&pattern, "node-ovl-a", agent.clone());
        spawn_mock_lifecycle_agent(&pattern, "node-ovl-b", agent.clone());

        let orchestrator = test_orchestrator_with_overlay(&pool, &pattern);

        // First (and only) dispatch: the op must go terminal here — a
        // partial refusal is terminal-class because every FAILURE was a
        // refusal (the applied nodes are not failures).
        orchestrator.tick().await.expect("tick 1");

        let (status, error_code, error_message, retry_count, next_retry_at, completed_at) =
            op_row(&pool, "op-ovl-3").await;
        assert_eq!(
            status, "Failed",
            "a partial-success all-refusals fan-out is terminal on first dispatch"
        );
        assert_eq!(
            error_code.as_deref(),
            Some("UNSUPPORTED_BY_AGENT"),
            "the error code must name the cause"
        );
        let message = error_message.as_deref().unwrap_or_default();
        // The refusing set: count, node id, and the agent's refusal text.
        assert!(
            message.contains("refused by all 1 failing node(s)"),
            "the refusing count rides the roll-up: {message:?}"
        );
        assert!(
            message.contains("node-ovl-b"),
            "the refusing node is named: {message:?}"
        );
        assert!(
            message.contains("update_overlay is unsupported in core-managed mode"),
            "the refusing node's agent text rides the roll-up verbatim: {message:?}"
        );
        // The applied set (#502): the applied node is named distinctly.
        assert!(
            message.contains("applied on 1 node(s): node-ovl-a"),
            "the applied node is named distinctly from the refusals: {message:?}"
        );
        assert!(
            message.contains("node-ovl-a") && message.contains("node-ovl-b"),
            "both sets appear in one terminal message: {message:?}"
        );
        assert_eq!(
            retry_count, 0,
            "no retry may be scheduled for a terminal-class error"
        );
        assert_eq!(
            next_retry_at, None,
            "mark_for_retry must never run: no next_retry_at may be written"
        );
        assert!(
            completed_at.is_some(),
            "the terminal write stamps completed_at"
        );
        assert_eq!(
            agent.overlay_calls.lock().unwrap().len(),
            2,
            "the fan-out still attempts every participating node exactly once"
        );

        // Further ticks must not resurrect the terminal row or re-dispatch.
        orchestrator.tick().await.expect("tick 2");
        orchestrator.tick().await.expect("tick 3");
        assert_eq!(
            agent.overlay_calls.lock().unwrap().len(),
            2,
            "exactly one dispatch per node across all ticks"
        );
        let (status, _, _, retry_count, next_retry_at, _) = op_row(&pool, "op-ovl-3").await;
        assert_eq!(status, "Failed", "the terminal row stays terminal");
        assert_eq!(retry_count, 0);
        assert_eq!(next_retry_at, None);
    }

    /// Control for the fan-out fast-fail: a MIXED fan-out failure (one
    /// node refuses with `Unimplemented`, one fails with a non-refusal
    /// error) keeps today's aggregation semantics byte-for-byte — the
    /// fan-out error stays Internal, no terminal row is written at
    /// dispatch time, and the tick's shared retry arm schedules the
    /// 10/20/40 s backoff curve with exhaustion landing on `Failed` /
    /// `DISPATCH_FAILED` after exactly MAX_DISPATCH_RETRIES retries.
    #[tokio::test]
    async fn update_overlay_fan_out_mixed_failure_keeps_retry_semantics() {
        let pool = create_test_pool().await;
        seed_update_overlay_op(&pool, "net-ovl", "op-ovl-2").await;

        let dir = tempfile::tempdir().unwrap();
        let pattern = dir
            .path()
            .join("agent-{node_id}.sock")
            .to_str()
            .unwrap()
            .to_string();
        let mut overlay_status_by_node = std::collections::HashMap::new();
        overlay_status_by_node.insert(
            "node-ovl-a".to_string(),
            tonic::Status::unimplemented("update_overlay is unsupported in core-managed mode"),
        );
        overlay_status_by_node.insert(
            "node-ovl-b".to_string(),
            tonic::Status::unavailable("agent restarting"),
        );
        let agent = MockLifecycleAgent {
            snapshot_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_status: tonic::Status::unimplemented(""),
            snapshot_vm_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot_vm_status: tonic::Status::unimplemented(""),
            overlay_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            overlay_status_by_node: std::sync::Arc::new(std::sync::Mutex::new(
                overlay_status_by_node,
            )),
            attach_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            network_policy_status: tonic::Status::ok(""),
            create_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_status: tonic::Status::ok(""),
            create_status: tonic::Status::ok(""),
        };
        spawn_mock_lifecycle_agent(&pattern, "node-ovl-a", agent.clone());
        spawn_mock_lifecycle_agent(&pattern, "node-ovl-b", agent.clone());

        let orchestrator = test_orchestrator_with_overlay(&pool, &pattern);

        // Attempt 1: mixed fan-out failure → Internal aggregation, no
        // terminal write at dispatch, resurrected to RetryPending (retry 1).
        orchestrator.tick().await.expect("tick 1");
        let (status, error_code, _, retry_count, next_retry_at, completed_at) =
            op_row(&pool, "op-ovl-2").await;
        assert_eq!(
            status, "RetryPending",
            "mixed fan-out failures keep the retry semantics"
        );
        assert_eq!(retry_count, 1);
        assert!(
            next_retry_at.is_some(),
            "the backoff schedule is written as before"
        );
        assert_eq!(
            error_code, None,
            "no terminal write happens for a mixed failure (as before)"
        );
        assert!(
            completed_at.is_none(),
            "the op is not terminal: it is queued for retry"
        );
        assert_eq!(
            agent.overlay_calls.lock().unwrap().len(),
            2,
            "one fan-out attempt per node on the initial dispatch"
        );

        // Attempts 2 and 3: backdate the backoff anchor and re-tick.
        for expected_retry in [2, 3] {
            sqlx::query(
                "UPDATE operations SET next_retry_at = '2026-01-01T00:00:00Z' \
                 WHERE operation_id = 'op-ovl-2'",
            )
            .execute(&pool)
            .await
            .expect("backdate retry anchor");
            orchestrator.tick().await.expect("retry tick");
            let (status, _, _, retry_count, _, _) = op_row(&pool, "op-ovl-2").await;
            assert_eq!(status, "RetryPending");
            assert_eq!(retry_count, expected_retry);
        }

        // Attempt 4 exceeds MAX_DISPATCH_RETRIES: terminal
        // Failed/DISPATCH_FAILED with the exhaustion message.
        sqlx::query(
            "UPDATE operations SET next_retry_at = '2026-01-01T00:00:00Z' \
             WHERE operation_id = 'op-ovl-2'",
        )
        .execute(&pool)
        .await
        .expect("backdate retry anchor");
        orchestrator.tick().await.expect("exhaustion tick");
        let (status, error_code, error_message, _, _, _) = op_row(&pool, "op-ovl-2").await;
        assert_eq!(status, "Failed");
        assert_eq!(error_code.as_deref(), Some("DISPATCH_FAILED"));
        assert!(
            error_message
                .as_deref()
                .unwrap_or_default()
                .contains("permanently failed after 3 retries"),
            "the exhaustion message shape is unchanged: {error_message:?}"
        );

        // The full curve dispatched the fan-out exactly 1 +
        // MAX_DISPATCH_RETRIES times (per node).
        assert_eq!(
            agent.overlay_calls.lock().unwrap().len(),
            8,
            "initial attempt plus 3 retries per node — the retry curve is unchanged"
        );
    }
}
