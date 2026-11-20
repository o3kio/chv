use crate::audit_outbox::{enqueue_adopted_row, AuditOutboxInsert};
use crate::{StoreError, StorePool};
use chv_controlplane_types::domain::{Generation, NodeId, NodeState, ResourceId};

const UPSERT_NODE_OBSERVED_STATE_SQL: &str = r#"
INSERT INTO node_observed_state (
    node_id,
    observed_generation,
    observed_state,
    health_status,
    runtime_status,
    state_reason,
    entered_at,
    observed_at,
    updated_at,
    last_seen_at
)
VALUES (
    $1,
    $2,
    $3,
    $4,
    $5,
    $6,
    CASE WHEN $7 IS NULL THEN NULL ELSE strftime('%Y-%m-%dT%H:%M:%SZ', $7 / 1000.0, 'unixepoch') END,
    strftime('%Y-%m-%dT%H:%M:%SZ', $8 / 1000.0, 'unixepoch'),
    strftime('%Y-%m-%dT%H:%M:%SZ', $8 / 1000.0, 'unixepoch'),
    strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
)
ON CONFLICT (node_id) DO UPDATE SET
    observed_generation = EXCLUDED.observed_generation,
    observed_state = EXCLUDED.observed_state,
    health_status = EXCLUDED.health_status,
    runtime_status = EXCLUDED.runtime_status,
    state_reason = EXCLUDED.state_reason,
    entered_at = EXCLUDED.entered_at,
    observed_at = EXCLUDED.observed_at,
    updated_at = EXCLUDED.updated_at,
    last_seen_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
"#;

const UPSERT_VM_OBSERVED_STATE_SQL: &str = r#"
INSERT INTO vm_observed_state (
    vm_id,
    observed_generation,
    runtime_status,
    health_status,
    node_id,
    cloud_hypervisor_pid,
    api_socket_path,
    last_transition_at,
    last_error,
    observed_at,
    updated_at
)
VALUES (
    $1,
    $2,
    $3,
    $4,
    $5,
    $6,
    $7,
    CASE WHEN $8 IS NULL THEN NULL ELSE strftime('%Y-%m-%dT%H:%M:%SZ', $8 / 1000.0, 'unixepoch') END,
    $9,
    strftime('%Y-%m-%dT%H:%M:%SZ', $10 / 1000.0, 'unixepoch'),
    strftime('%Y-%m-%dT%H:%M:%SZ', $10 / 1000.0, 'unixepoch')
)
ON CONFLICT (vm_id) DO UPDATE SET
    observed_generation = EXCLUDED.observed_generation,
    runtime_status = EXCLUDED.runtime_status,
    health_status = EXCLUDED.health_status,
    node_id = EXCLUDED.node_id,
    cloud_hypervisor_pid = EXCLUDED.cloud_hypervisor_pid,
    api_socket_path = EXCLUDED.api_socket_path,
    last_transition_at = EXCLUDED.last_transition_at,
    last_error = EXCLUDED.last_error,
    observed_at = EXCLUDED.observed_at,
    updated_at = EXCLUDED.updated_at
"#;

const UPSERT_VOLUME_OBSERVED_STATE_SQL: &str = r#"
INSERT INTO volume_observed_state (
    volume_id,
    observed_generation,
    runtime_status,
    health_status,
    attached_vm_id,
    device_path,
    export_path,
    last_transition_at,
    observed_at,
    updated_at
)
VALUES (
    $1,
    $2,
    $3,
    $4,
    $5,
    $6,
    $7,
    CASE WHEN $8 IS NULL THEN NULL ELSE strftime('%Y-%m-%dT%H:%M:%SZ', $8 / 1000.0, 'unixepoch') END,
    strftime('%Y-%m-%dT%H:%M:%SZ', $9 / 1000.0, 'unixepoch'),
    strftime('%Y-%m-%dT%H:%M:%SZ', $9 / 1000.0, 'unixepoch')
)
ON CONFLICT (volume_id) DO UPDATE SET
    observed_generation = EXCLUDED.observed_generation,
    runtime_status = EXCLUDED.runtime_status,
    health_status = EXCLUDED.health_status,
    attached_vm_id = EXCLUDED.attached_vm_id,
    device_path = EXCLUDED.device_path,
    export_path = EXCLUDED.export_path,
    last_transition_at = EXCLUDED.last_transition_at,
    observed_at = EXCLUDED.observed_at,
    updated_at = EXCLUDED.updated_at
"#;

const INSERT_VM_METRICS_SQL: &str = r#"
INSERT INTO vm_metrics (
    vm_id,
    collected_at,
    cpu_percent,
    memory_bytes_used,
    memory_bytes_total,
    disk_bytes_read,
    disk_bytes_written,
    net_bytes_rx,
    net_bytes_tx
)
VALUES (
    $1,
    strftime('%Y-%m-%dT%H:%M:%SZ', $2 / 1000.0, 'unixepoch'),
    $3,
    $4,
    $5,
    $6,
    $7,
    $8,
    $9
)
"#;

const UPSERT_NETWORK_OBSERVED_STATE_SQL: &str = r#"
INSERT INTO network_observed_state (
    network_id,
    observed_generation,
    runtime_status,
    health_status,
    exposure_status,
    applied_at,
    observed_at,
    updated_at
)
VALUES (
    $1,
    $2,
    $3,
    $4,
    $5,
    CASE WHEN $6 IS NULL THEN NULL ELSE strftime('%Y-%m-%dT%H:%M:%SZ', $6 / 1000.0, 'unixepoch') END,
    strftime('%Y-%m-%dT%H:%M:%SZ', $7 / 1000.0, 'unixepoch'),
    strftime('%Y-%m-%dT%H:%M:%SZ', $7 / 1000.0, 'unixepoch')
)
ON CONFLICT (network_id) DO UPDATE SET
    observed_generation = EXCLUDED.observed_generation,
    runtime_status = EXCLUDED.runtime_status,
    health_status = EXCLUDED.health_status,
    exposure_status = EXCLUDED.exposure_status,
    applied_at = EXCLUDED.applied_at,
    observed_at = EXCLUDED.observed_at,
    updated_at = EXCLUDED.updated_at
"#;

const ACK_NODE_OBSERVED_GENERATION_SQL: &str = r#"
UPDATE node_observed_state SET
    observed_generation = $2,
    observed_at = strftime('%Y-%m-%dT%H:%M:%SZ', $3 / 1000.0, 'unixepoch'),
    updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', $3 / 1000.0, 'unixepoch')
WHERE node_id = $1 AND observed_generation <= $2
"#;

const ACK_VM_OBSERVED_GENERATION_SQL: &str = r#"
INSERT INTO vm_observed_state (
    vm_id,
    observed_generation,
    runtime_status,
    observed_at,
    updated_at
)
VALUES (
    $1,
    $2,
    COALESCE((SELECT runtime_status FROM vm_observed_state WHERE vm_id = $1), ''),
    strftime('%Y-%m-%dT%H:%M:%SZ', $3 / 1000.0, 'unixepoch'),
    strftime('%Y-%m-%dT%H:%M:%SZ', $3 / 1000.0, 'unixepoch')
)
ON CONFLICT (vm_id) DO UPDATE SET
    observed_generation = EXCLUDED.observed_generation,
    observed_at = EXCLUDED.observed_at,
    updated_at = EXCLUDED.updated_at
WHERE vm_observed_state.observed_generation <= EXCLUDED.observed_generation
"#;

const ACK_VOLUME_OBSERVED_GENERATION_SQL: &str = r#"
INSERT INTO volume_observed_state (
    volume_id,
    observed_generation,
    runtime_status,
    observed_at,
    updated_at
)
VALUES (
    $1,
    $2,
    COALESCE((SELECT runtime_status FROM volume_observed_state WHERE volume_id = $1), ''),
    strftime('%Y-%m-%dT%H:%M:%SZ', $3 / 1000.0, 'unixepoch'),
    strftime('%Y-%m-%dT%H:%M:%SZ', $3 / 1000.0, 'unixepoch')
)
ON CONFLICT (volume_id) DO UPDATE SET
    observed_generation = EXCLUDED.observed_generation,
    observed_at = EXCLUDED.observed_at,
    updated_at = EXCLUDED.updated_at
WHERE volume_observed_state.observed_generation <= EXCLUDED.observed_generation
"#;

const ACK_NETWORK_OBSERVED_GENERATION_SQL: &str = r#"
INSERT INTO network_observed_state (
    network_id,
    observed_generation,
    runtime_status,
    observed_at,
    updated_at
)
VALUES (
    $1,
    $2,
    COALESCE((SELECT runtime_status FROM network_observed_state WHERE network_id = $1), ''),
    strftime('%Y-%m-%dT%H:%M:%SZ', $3 / 1000.0, 'unixepoch'),
    strftime('%Y-%m-%dT%H:%M:%SZ', $3 / 1000.0, 'unixepoch')
)
ON CONFLICT (network_id) DO UPDATE SET
    observed_generation = EXCLUDED.observed_generation,
    observed_at = EXCLUDED.observed_at,
    updated_at = EXCLUDED.updated_at
WHERE network_observed_state.observed_generation <= EXCLUDED.observed_generation
"#;

#[derive(Clone)]
pub struct ObservedStateRepository {
    pool: StorePool,
}

impl ObservedStateRepository {
    pub fn new(pool: StorePool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &StorePool {
        &self.pool
    }

    pub async fn upsert_node(&self, input: &NodeObservedStateInput) -> Result<(), StoreError> {
        sqlx::query(UPSERT_NODE_OBSERVED_STATE_SQL)
            .bind(input.node_id.as_str())
            .bind(generation_to_i64(input.observed_generation)?)
            .bind(input.observed_state.as_str())
            .bind(&input.health_status)
            .bind(&input.runtime_status)
            .bind(&input.state_reason)
            .bind(input.entered_unix_ms)
            .bind(input.observed_unix_ms)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Record a node's acknowledged desired-state generation.
    ///
    /// `audit` is the request's pre-minted audit-outbox row (#706). When
    /// present, the acknowledgement and the outbox enqueue share one
    /// transaction, so a missing observed-state row commits neither.
    pub async fn acknowledge_node_generation(
        &self,
        node_id: &NodeId,
        observed_generation: Generation,
        observed_unix_ms: i64,
        audit: Option<&AuditOutboxInsert>,
    ) -> Result<(), StoreError> {
        match audit {
            None => {
                let mut conn = self.pool.acquire().await?;
                Self::acknowledge_node_generation_on(
                    &mut conn,
                    node_id,
                    observed_generation,
                    observed_unix_ms,
                )
                .await
            }
            Some(audit) => {
                let mut tx = self.pool.begin().await?;
                Self::acknowledge_node_generation_on(
                    &mut tx,
                    node_id,
                    observed_generation,
                    observed_unix_ms,
                )
                .await?;
                enqueue_adopted_row(&mut tx, audit).await?;
                tx.commit().await?;
                Ok(())
            }
        }
    }

    async fn acknowledge_node_generation_on(
        conn: &mut sqlx::SqliteConnection,
        node_id: &NodeId,
        observed_generation: Generation,
        observed_unix_ms: i64,
    ) -> Result<(), StoreError> {
        let result = sqlx::query(ACK_NODE_OBSERVED_GENERATION_SQL)
            .bind(node_id.as_str())
            .bind(generation_to_i64(observed_generation)?)
            .bind(observed_unix_ms)
            .execute(&mut *conn)
            .await?;

        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound {
                entity: "node_observed_state",
                id: node_id.to_string(),
            });
        }

        Ok(())
    }

    /// The node a VM is currently observed running on (its placement
    /// authority for ingestion contract v1): `None` when the VM is
    /// unknown or has no observed node. The monitoring ingest path uses
    /// this to reject batches claiming VM samples for VMs the
    /// authenticated sender does not own — a node may never write
    /// another node's VM telemetry.
    pub async fn vm_reporting_node(
        &self,
        vm_id: &ResourceId,
    ) -> Result<Option<NodeId>, StoreError> {
        let node: Option<String> =
            sqlx::query_scalar("SELECT node_id FROM vm_observed_state WHERE vm_id = ? LIMIT 1")
                .bind(vm_id.as_str())
                .fetch_optional(&self.pool)
                .await?
                .flatten();
        match node {
            None => Ok(None),
            Some(raw) => match NodeId::new(&raw) {
                Ok(id) => Ok(Some(id)),
                // A malformed node_id is store corruption, not a
                // placement decision: surface it loudly and fail the
                // ownership check closed (None ⇒ the sender cannot
                // prove ownership).
                Err(e) => {
                    tracing::warn!(
                        vm_id = vm_id.as_str(),
                        error = %e,
                        "vm_observed_state carries a malformed node_id; failing ownership closed"
                    );
                    Ok(None)
                }
            },
        }
    }

    pub async fn upsert_vm(&self, input: &VmObservedStateInput) -> Result<(), StoreError> {
        sqlx::query(UPSERT_VM_OBSERVED_STATE_SQL)
            .bind(input.vm_id.as_str())
            .bind(generation_to_i64(input.observed_generation)?)
            .bind(&input.runtime_status)
            .bind(&input.health_status)
            .bind(input.node_id.as_ref().map(NodeId::as_str))
            .bind(input.cloud_hypervisor_pid)
            .bind(&input.api_socket_path)
            .bind(input.last_transition_unix_ms)
            .bind(&input.last_error)
            .bind(input.observed_unix_ms)
            .execute(&self.pool)
            .await
            .map_err(|e| match &e {
                sqlx::Error::Database(db_err) if db_err.is_foreign_key_violation() => {
                    // In SQLite, FK violations don't report constraint names.
                    // If node_id was provided, the node FK is the likely culprit;
                    // otherwise the vm_id FK failed (vm doesn't exist).
                    let (entity, id) = if input.node_id.is_some() {
                        (
                            "node",
                            input
                                .node_id
                                .as_ref()
                                .map(|n| n.to_string())
                                .unwrap_or_default(),
                        )
                    } else {
                        ("vm", input.vm_id.to_string())
                    };
                    StoreError::NotFound { entity, id }
                }
                _ => StoreError::from(e),
            })?;
        Ok(())
    }

    fn acknowledge_generation_error(
        error: sqlx::Error,
        entity: &'static str,
        id: &str,
    ) -> StoreError {
        match &error {
            sqlx::Error::Database(db_err) if db_err.is_foreign_key_violation() => {
                StoreError::NotFound {
                    entity,
                    id: id.to_string(),
                }
            }
            _ => StoreError::from(error),
        }
    }

    /// Record a VM's acknowledged desired-state generation (`audit`
    /// follows [`Self::acknowledge_node_generation`]).
    pub async fn acknowledge_vm_generation(
        &self,
        vm_id: &ResourceId,
        observed_generation: Generation,
        observed_unix_ms: i64,
        audit: Option<&AuditOutboxInsert>,
    ) -> Result<(), StoreError> {
        match audit {
            None => {
                let mut conn = self.pool.acquire().await?;
                Self::acknowledge_vm_generation_on(
                    &mut conn,
                    vm_id,
                    observed_generation,
                    observed_unix_ms,
                )
                .await
            }
            Some(audit) => {
                let mut tx = self.pool.begin().await?;
                Self::acknowledge_vm_generation_on(
                    &mut tx,
                    vm_id,
                    observed_generation,
                    observed_unix_ms,
                )
                .await?;
                enqueue_adopted_row(&mut tx, audit).await?;
                tx.commit().await?;
                Ok(())
            }
        }
    }

    async fn acknowledge_vm_generation_on(
        conn: &mut sqlx::SqliteConnection,
        vm_id: &ResourceId,
        observed_generation: Generation,
        observed_unix_ms: i64,
    ) -> Result<(), StoreError> {
        sqlx::query(ACK_VM_OBSERVED_GENERATION_SQL)
            .bind(vm_id.as_str())
            .bind(generation_to_i64(observed_generation)?)
            .bind(observed_unix_ms)
            .execute(&mut *conn)
            .await
            .map_err(|e| Self::acknowledge_generation_error(e, "vm", vm_id.as_str()))?;
        Ok(())
    }

    pub async fn upsert_volume(&self, input: &VolumeObservedStateInput) -> Result<(), StoreError> {
        sqlx::query(UPSERT_VOLUME_OBSERVED_STATE_SQL)
            .bind(input.volume_id.as_str())
            .bind(generation_to_i64(input.observed_generation)?)
            .bind(&input.runtime_status)
            .bind(&input.health_status)
            .bind(input.attached_vm_id.as_ref().map(ResourceId::as_str))
            .bind(&input.device_path)
            .bind(&input.export_path)
            .bind(input.last_transition_unix_ms)
            .bind(input.observed_unix_ms)
            .execute(&self.pool)
            .await
            .map_err(|e| match &e {
                sqlx::Error::Database(db_err) if db_err.is_foreign_key_violation() => {
                    // In SQLite, FK violations don't report constraint names.
                    // If attached_vm_id was provided, the vm FK is the likely culprit;
                    // otherwise the volume_id FK failed.
                    let (entity, id) = if input.attached_vm_id.is_some() {
                        (
                            "vm",
                            input
                                .attached_vm_id
                                .as_ref()
                                .map(|v| v.to_string())
                                .unwrap_or_default(),
                        )
                    } else {
                        ("volume", input.volume_id.to_string())
                    };
                    StoreError::NotFound { entity, id }
                }
                _ => StoreError::from(e),
            })?;
        Ok(())
    }

    /// Record a volume's acknowledged desired-state generation (`audit`
    /// follows [`Self::acknowledge_node_generation`]).
    pub async fn acknowledge_volume_generation(
        &self,
        volume_id: &ResourceId,
        observed_generation: Generation,
        observed_unix_ms: i64,
        audit: Option<&AuditOutboxInsert>,
    ) -> Result<(), StoreError> {
        match audit {
            None => {
                let mut conn = self.pool.acquire().await?;
                Self::acknowledge_volume_generation_on(
                    &mut conn,
                    volume_id,
                    observed_generation,
                    observed_unix_ms,
                )
                .await
            }
            Some(audit) => {
                let mut tx = self.pool.begin().await?;
                Self::acknowledge_volume_generation_on(
                    &mut tx,
                    volume_id,
                    observed_generation,
                    observed_unix_ms,
                )
                .await?;
                enqueue_adopted_row(&mut tx, audit).await?;
                tx.commit().await?;
                Ok(())
            }
        }
    }

    async fn acknowledge_volume_generation_on(
        conn: &mut sqlx::SqliteConnection,
        volume_id: &ResourceId,
        observed_generation: Generation,
        observed_unix_ms: i64,
    ) -> Result<(), StoreError> {
        sqlx::query(ACK_VOLUME_OBSERVED_GENERATION_SQL)
            .bind(volume_id.as_str())
            .bind(generation_to_i64(observed_generation)?)
            .bind(observed_unix_ms)
            .execute(&mut *conn)
            .await
            .map_err(|e| Self::acknowledge_generation_error(e, "volume", volume_id.as_str()))?;
        Ok(())
    }

    pub async fn insert_vm_metrics(&self, input: &VmMetricsInput) -> Result<(), StoreError> {
        sqlx::query(INSERT_VM_METRICS_SQL)
            .bind(input.vm_id.as_str())
            .bind(input.collected_unix_ms)
            .bind(input.cpu_percent)
            .bind(input.memory_bytes_used)
            .bind(input.memory_bytes_total)
            .bind(input.disk_bytes_read)
            .bind(input.disk_bytes_written)
            .bind(input.net_bytes_rx)
            .bind(input.net_bytes_tx)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn upsert_network(
        &self,
        input: &NetworkObservedStateInput,
    ) -> Result<(), StoreError> {
        sqlx::query(UPSERT_NETWORK_OBSERVED_STATE_SQL)
            .bind(input.network_id.as_str())
            .bind(generation_to_i64(input.observed_generation)?)
            .bind(&input.runtime_status)
            .bind(&input.health_status)
            .bind(&input.exposure_status)
            .bind(input.applied_unix_ms)
            .bind(input.observed_unix_ms)
            .execute(&self.pool)
            .await
            .map_err(|e| match &e {
                sqlx::Error::Database(db_err) if db_err.is_foreign_key_violation() => {
                    StoreError::NotFound {
                        entity: "network",
                        id: input.network_id.to_string(),
                    }
                }
                _ => StoreError::from(e),
            })?;
        Ok(())
    }

    /// Record a network's acknowledged desired-state generation (`audit`
    /// follows [`Self::acknowledge_node_generation`]).
    pub async fn acknowledge_network_generation(
        &self,
        network_id: &ResourceId,
        observed_generation: Generation,
        observed_unix_ms: i64,
        audit: Option<&AuditOutboxInsert>,
    ) -> Result<(), StoreError> {
        match audit {
            None => {
                let mut conn = self.pool.acquire().await?;
                Self::acknowledge_network_generation_on(
                    &mut conn,
                    network_id,
                    observed_generation,
                    observed_unix_ms,
                )
                .await
            }
            Some(audit) => {
                let mut tx = self.pool.begin().await?;
                Self::acknowledge_network_generation_on(
                    &mut tx,
                    network_id,
                    observed_generation,
                    observed_unix_ms,
                )
                .await?;
                enqueue_adopted_row(&mut tx, audit).await?;
                tx.commit().await?;
                Ok(())
            }
        }
    }

    async fn acknowledge_network_generation_on(
        conn: &mut sqlx::SqliteConnection,
        network_id: &ResourceId,
        observed_generation: Generation,
        observed_unix_ms: i64,
    ) -> Result<(), StoreError> {
        sqlx::query(ACK_NETWORK_OBSERVED_GENERATION_SQL)
            .bind(network_id.as_str())
            .bind(generation_to_i64(observed_generation)?)
            .bind(observed_unix_ms)
            .execute(&mut *conn)
            .await
            .map_err(|e| Self::acknowledge_generation_error(e, "network", network_id.as_str()))?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct NodeObservedStateInput {
    pub node_id: NodeId,
    pub observed_generation: Generation,
    pub observed_state: NodeState,
    pub health_status: Option<String>,
    pub runtime_status: Option<String>,
    pub state_reason: Option<String>,
    pub entered_unix_ms: Option<i64>,
    pub observed_unix_ms: i64,
}

#[derive(Clone)]
pub struct VmObservedStateInput {
    pub vm_id: ResourceId,
    pub observed_generation: Generation,
    pub runtime_status: String,
    pub health_status: Option<String>,
    pub node_id: Option<NodeId>,
    pub cloud_hypervisor_pid: Option<i32>,
    pub api_socket_path: Option<String>,
    pub last_error: Option<String>,
    pub last_transition_unix_ms: Option<i64>,
    pub observed_unix_ms: i64,
}

#[derive(Clone)]
pub struct VolumeObservedStateInput {
    pub volume_id: ResourceId,
    pub observed_generation: Generation,
    pub runtime_status: String,
    pub health_status: Option<String>,
    pub attached_vm_id: Option<ResourceId>,
    pub device_path: Option<String>,
    pub export_path: Option<String>,
    pub last_transition_unix_ms: Option<i64>,
    pub observed_unix_ms: i64,
}

#[derive(Clone)]
pub struct NetworkObservedStateInput {
    pub network_id: ResourceId,
    pub observed_generation: Generation,
    pub runtime_status: String,
    pub health_status: Option<String>,
    pub exposure_status: Option<String>,
    pub applied_unix_ms: Option<i64>,
    pub observed_unix_ms: i64,
}

#[derive(Clone)]
pub struct VmMetricsInput {
    pub vm_id: ResourceId,
    pub collected_unix_ms: i64,
    pub cpu_percent: f64,
    pub memory_bytes_used: i64,
    pub memory_bytes_total: i64,
    pub disk_bytes_read: i64,
    pub disk_bytes_written: i64,
    pub net_bytes_rx: i64,
    pub net_bytes_tx: i64,
}

fn generation_to_i64(generation: Generation) -> Result<i64, StoreError> {
    i64::try_from(generation.get()).map_err(|source| StoreError::InvalidConfiguration {
        reason: format!("generation out of range for bigint column: {source}"),
    })
}
