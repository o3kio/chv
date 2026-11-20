use crate::audit_outbox::{enqueue_adopted_row, AuditOutboxInsert};
use crate::{NetworkExposureInput, StoreError, StorePool};
use chv_controlplane_types::domain::{Generation, NodeId, ResourceId};

const UPSERT_VM_SQL: &str = r#"
INSERT INTO vms (
    vm_id,
    node_id,
    display_name,
    tenant_id,
    placement_policy,
    updated_at
)
VALUES (
    $1,
    $2,
    $3,
    $4,
    $5,
    strftime('%Y-%m-%dT%H:%M:%SZ', $6 / 1000.0, 'unixepoch')
)
ON CONFLICT (vm_id) DO UPDATE SET
    node_id = EXCLUDED.node_id,
    display_name = EXCLUDED.display_name,
    tenant_id = EXCLUDED.tenant_id,
    placement_policy = EXCLUDED.placement_policy,
    updated_at = EXCLUDED.updated_at
"#;

const UPSERT_VM_DESIRED_STATE_SQL: &str = r#"
INSERT INTO vm_desired_state (
    vm_id,
    desired_generation,
    desired_status,
    requested_by,
    updated_by,
    target_node_id,
    cpu_count,
    memory_bytes,
    image_ref,
    boot_mode,
    desired_power_state,
    vsock_enabled,
    requested_at,
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
    $8,
    $9,
    $10,
    $11,
    COALESCE($12, 0),
    strftime('%Y-%m-%dT%H:%M:%SZ', $13 / 1000.0, 'unixepoch'),
    strftime('%Y-%m-%dT%H:%M:%SZ', $13 / 1000.0, 'unixepoch')
)
ON CONFLICT (vm_id) DO UPDATE SET
    desired_generation = EXCLUDED.desired_generation,
    desired_status = EXCLUDED.desired_status,
    requested_by = EXCLUDED.requested_by,
    updated_by = EXCLUDED.updated_by,
    target_node_id = EXCLUDED.target_node_id,
    cpu_count = EXCLUDED.cpu_count,
    memory_bytes = EXCLUDED.memory_bytes,
    image_ref = EXCLUDED.image_ref,
    boot_mode = EXCLUDED.boot_mode,
    desired_power_state = EXCLUDED.desired_power_state,
    vsock_enabled = COALESCE($12, vm_desired_state.vsock_enabled),
    requested_at = EXCLUDED.requested_at,
    updated_at = EXCLUDED.updated_at
WHERE vm_desired_state.desired_generation <= EXCLUDED.desired_generation
"#;

const UPSERT_VOLUME_SQL: &str = r#"
INSERT INTO volumes (
    volume_id,
    node_id,
    display_name,
    capacity_bytes,
    volume_kind,
    storage_class,
    owner_id,
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
    strftime('%Y-%m-%dT%H:%M:%SZ', $8 / 1000.0, 'unixepoch')
)
ON CONFLICT (volume_id) DO UPDATE SET
    node_id = EXCLUDED.node_id,
    display_name = EXCLUDED.display_name,
    capacity_bytes = EXCLUDED.capacity_bytes,
    volume_kind = EXCLUDED.volume_kind,
    storage_class = EXCLUDED.storage_class,
    -- Ownership is never cleared by a re-upsert: callers that do not know
    -- the owner (e.g. the agent volume-fragment reconcile) pass NULL and
    -- must not strip the owner a BFF creation or clone set (#381 review:
    -- an ownerless volume is admin-only in the BFF, so a NULL-overwrite
    -- would lock non-admin operators out of their own clones).
    owner_id = COALESCE(EXCLUDED.owner_id, volumes.owner_id),
    updated_at = EXCLUDED.updated_at
"#;

/// Strict insert for the clone target's physical row (#384): unlike
/// `UPSERT_VOLUME_SQL`, there is no DO UPDATE — the clone target must NOT
/// exist, so a concurrent same-target materialization fails closed here
/// (rows_affected == 0) instead of last-writer-wins overwriting the
/// winner's shape.
const INSERT_VOLUME_FOR_CLONE_SQL: &str = r#"
INSERT INTO volumes (
    volume_id,
    node_id,
    display_name,
    capacity_bytes,
    volume_kind,
    storage_class,
    owner_id,
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
    strftime('%Y-%m-%dT%H:%M:%SZ', $8 / 1000.0, 'unixepoch')
)
ON CONFLICT (volume_id) DO NOTHING
"#;

const UPSERT_VOLUME_DESIRED_STATE_SQL: &str = r#"
INSERT INTO volume_desired_state (
    volume_id,
    desired_generation,
    desired_status,
    requested_by,
    updated_by,
    attached_vm_id,
    attachment_mode,
    device_name,
    read_only,
    resize_to_bytes,
    snapshot_op,
    snapshot_name,
    clone_source_volume_id,
    requested_at,
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
    $8,
    $9,
    $10,
    $11,
    $12,
    $13,
    strftime('%Y-%m-%dT%H:%M:%SZ', $14 / 1000.0, 'unixepoch'),
    strftime('%Y-%m-%dT%H:%M:%SZ', $14 / 1000.0, 'unixepoch')
)
ON CONFLICT (volume_id) DO UPDATE SET
    desired_generation = EXCLUDED.desired_generation,
    desired_status = EXCLUDED.desired_status,
    requested_by = EXCLUDED.requested_by,
    updated_by = EXCLUDED.updated_by,
    attached_vm_id = EXCLUDED.attached_vm_id,
    attachment_mode = EXCLUDED.attachment_mode,
    device_name = EXCLUDED.device_name,
    read_only = EXCLUDED.read_only,
    resize_to_bytes = EXCLUDED.resize_to_bytes,
    snapshot_op = EXCLUDED.snapshot_op,
    snapshot_name = EXCLUDED.snapshot_name,
    clone_source_volume_id = EXCLUDED.clone_source_volume_id,
    requested_at = EXCLUDED.requested_at,
    updated_at = EXCLUDED.updated_at
WHERE volume_desired_state.desired_generation <= EXCLUDED.desired_generation
"#;

const UPSERT_NETWORK_SQL: &str = r#"
INSERT INTO networks (
    network_id,
    node_id,
    display_name,
    network_class,
    updated_at
)
VALUES (
    $1,
    $2,
    $3,
    $4,
    strftime('%Y-%m-%dT%H:%M:%SZ', $5 / 1000.0, 'unixepoch')
)
ON CONFLICT (network_id) DO UPDATE SET
    node_id = EXCLUDED.node_id,
    display_name = EXCLUDED.display_name,
    network_class = EXCLUDED.network_class,
    updated_at = EXCLUDED.updated_at
"#;

const UPSERT_NETWORK_DESIRED_STATE_SQL: &str = r#"
INSERT INTO network_desired_state (
    network_id,
    desired_generation,
    desired_status,
    requested_by,
    updated_by,
    firewall_rules_json,
    nat_rules_json,
    dhcp_scope_json,
    dns_enabled,
    dns_scope_json,
    requested_at,
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
    $8,
    $9,
    $10,
    strftime('%Y-%m-%dT%H:%M:%SZ', $11 / 1000.0, 'unixepoch'),
    strftime('%Y-%m-%dT%H:%M:%SZ', $11 / 1000.0, 'unixepoch')
)
ON CONFLICT (network_id) DO UPDATE SET
    desired_generation = EXCLUDED.desired_generation,
    desired_status = EXCLUDED.desired_status,
    requested_by = EXCLUDED.requested_by,
    updated_by = EXCLUDED.updated_by,
    firewall_rules_json = EXCLUDED.firewall_rules_json,
    nat_rules_json = EXCLUDED.nat_rules_json,
    dhcp_scope_json = EXCLUDED.dhcp_scope_json,
    dns_enabled = EXCLUDED.dns_enabled,
    dns_scope_json = EXCLUDED.dns_scope_json,
    requested_at = EXCLUDED.requested_at,
    updated_at = EXCLUDED.updated_at
WHERE network_desired_state.desired_generation <= EXCLUDED.desired_generation
  -- #499: never journal over a delete tombstone. Network delete keeps
  -- the NDS row with a terminal 'Deleting' status (the #522 volume
  -- tombstone pattern); this IS NOT term makes the conflict arm refuse
  -- a late agent fragment (or any other writer) that would otherwise
  -- re-materialize the row's intent and, through the tx-coupled
  -- physical upsert, resurrect the deleted network. The INSERT path
  -- (no row) is closed by construction: the tombstone IS the row.
  -- IS NOT is NULL-safe: a NULL-status row passes the guard.
  AND network_desired_state.desired_status IS NOT 'Deleting'
"#;

const PATCH_VM_POWER_STATE_SQL: &str = r#"
INSERT INTO vm_desired_state (
    vm_id,
    desired_generation,
    desired_status,
    requested_by,
    updated_by,
    target_node_id,
    desired_power_state,
    requested_at,
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
    strftime('%Y-%m-%dT%H:%M:%SZ', $8 / 1000.0, 'unixepoch'),
    strftime('%Y-%m-%dT%H:%M:%SZ', $8 / 1000.0, 'unixepoch')
)
ON CONFLICT (vm_id) DO UPDATE SET
    desired_generation = EXCLUDED.desired_generation,
    desired_status = EXCLUDED.desired_status,
    requested_by = EXCLUDED.requested_by,
    updated_by = EXCLUDED.updated_by,
    target_node_id = COALESCE(EXCLUDED.target_node_id, vm_desired_state.target_node_id),
    desired_power_state = EXCLUDED.desired_power_state,
    requested_at = EXCLUDED.requested_at,
    updated_at = EXCLUDED.updated_at
WHERE vm_desired_state.desired_generation <= EXCLUDED.desired_generation
"#;

const PATCH_VM_RESOURCES_SQL: &str = r#"
UPDATE vm_desired_state SET
    cpu_count = COALESCE($2, cpu_count),
    memory_bytes = COALESCE($3, memory_bytes),
    desired_generation = $4,
    requested_by = $5,
    target_node_id = COALESCE($6, target_node_id),
    updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', $7 / 1000.0, 'unixepoch')
WHERE vm_id = $1 AND desired_generation <= $4
"#;

const PATCH_VOLUME_ATTACHMENT_SQL: &str = r#"
INSERT INTO volume_desired_state (
    volume_id,
    desired_generation,
    desired_status,
    requested_by,
    updated_by,
    attached_vm_id,
    requested_at,
    updated_at
)
VALUES (
    $1,
    $2,
    $3,
    $4,
    $5,
    $6,
    strftime('%Y-%m-%dT%H:%M:%SZ', $7 / 1000.0, 'unixepoch'),
    strftime('%Y-%m-%dT%H:%M:%SZ', $7 / 1000.0, 'unixepoch')
)
ON CONFLICT (volume_id) DO UPDATE SET
    desired_generation = EXCLUDED.desired_generation,
    desired_status = EXCLUDED.desired_status,
    requested_by = EXCLUDED.requested_by,
    updated_by = EXCLUDED.updated_by,
    attached_vm_id = EXCLUDED.attached_vm_id,
    requested_at = EXCLUDED.requested_at,
    updated_at = EXCLUDED.updated_at
WHERE volume_desired_state.desired_generation <= EXCLUDED.desired_generation
  -- #522 (review S1): never journal over a delete tombstone. The
  -- generations the control plane mints are wall-clock milliseconds,
  -- so the generation guard alone would always let a late-arriving
  -- attach/detach overwrite desired_status 'Deleting' and re-attach a
  -- volume whose backing the delete is concurrently reclaiming. IS
  -- NOT is NULL-safe: a NULL-status row passes the guard.
  AND volume_desired_state.desired_status IS NOT 'Deleting'
"#;

const PATCH_NETWORK_STATUS_SQL: &str = r#"
INSERT INTO network_desired_state (
    network_id,
    desired_generation,
    desired_status,
    requested_by,
    updated_by,
    requested_at,
    updated_at
)
VALUES (
    $1,
    $2,
    $3,
    $4,
    $5,
    strftime('%Y-%m-%dT%H:%M:%SZ', $6 / 1000.0, 'unixepoch'),
    strftime('%Y-%m-%dT%H:%M:%SZ', $6 / 1000.0, 'unixepoch')
)
ON CONFLICT (network_id) DO UPDATE SET
    desired_generation = EXCLUDED.desired_generation,
    desired_status = EXCLUDED.desired_status,
    requested_by = EXCLUDED.requested_by,
    updated_by = EXCLUDED.updated_by,
    requested_at = EXCLUDED.requested_at,
    updated_at = EXCLUDED.updated_at
WHERE network_desired_state.desired_generation <= EXCLUDED.desired_generation
  -- #499 (the #522 review-S1 sibling-verb guard, network-shaped): the
  -- start/stop/restart mutation verbs journal wall-clock-millisecond
  -- generations, so the generation guard alone would always let a
  -- late-arriving status patch overwrite a 'Deleting' tombstone and
  -- resurrect a deleted network's intent. IS NOT is NULL-safe: a
  -- NULL-status row passes the guard.
  AND network_desired_state.desired_status IS NOT 'Deleting'
"#;

const PATCH_VOLUME_RESIZE_SQL: &str = r#"
INSERT INTO volume_desired_state (
    volume_id,
    desired_generation,
    desired_status,
    requested_by,
    updated_by,
    resize_to_bytes,
    requested_at,
    updated_at
)
VALUES (
    $1,
    $2,
    $3,
    $4,
    $5,
    $6,
    strftime('%Y-%m-%dT%H:%M:%SZ', $7 / 1000.0, 'unixepoch'),
    strftime('%Y-%m-%dT%H:%M:%SZ', $7 / 1000.0, 'unixepoch')
)
ON CONFLICT (volume_id) DO UPDATE SET
    desired_generation = EXCLUDED.desired_generation,
    desired_status = EXCLUDED.desired_status,
    requested_by = EXCLUDED.requested_by,
    updated_by = EXCLUDED.updated_by,
    resize_to_bytes = EXCLUDED.resize_to_bytes,
    requested_at = EXCLUDED.requested_at,
    updated_at = EXCLUDED.updated_at
WHERE volume_desired_state.desired_generation <= EXCLUDED.desired_generation
  -- #522 (review S1): never journal over a delete tombstone (see
  -- PATCH_VOLUME_ATTACHMENT_SQL). The generation guard alone would
  -- always let a late-arriving resize overwrite desired_status
  -- 'Deleting' on a volume whose backing the delete is concurrently
  -- reclaiming.
  AND volume_desired_state.desired_status IS NOT 'Deleting'
"#;

const PATCH_VOLUME_SNAPSHOT_SQL: &str = r#"
INSERT INTO volume_desired_state (
    volume_id,
    desired_generation,
    desired_status,
    requested_by,
    updated_by,
    snapshot_op,
    snapshot_name,
    requested_at,
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
    strftime('%Y-%m-%dT%H:%M:%SZ', $8 / 1000.0, 'unixepoch'),
    strftime('%Y-%m-%dT%H:%M:%SZ', $8 / 1000.0, 'unixepoch')
)
ON CONFLICT (volume_id) DO UPDATE SET
    desired_generation = EXCLUDED.desired_generation,
    desired_status = EXCLUDED.desired_status,
    requested_by = EXCLUDED.requested_by,
    updated_by = EXCLUDED.updated_by,
    snapshot_op = EXCLUDED.snapshot_op,
    snapshot_name = EXCLUDED.snapshot_name,
    requested_at = EXCLUDED.requested_at,
    updated_at = EXCLUDED.updated_at
WHERE volume_desired_state.desired_generation <= EXCLUDED.desired_generation
  -- #522 (review S1): never journal over a delete tombstone (see
  -- PATCH_VOLUME_ATTACHMENT_SQL). The generation guard alone would
  -- always let a late-arriving snapshot overwrite desired_status
  -- 'Deleting' on a volume whose backing the delete is concurrently
  -- reclaiming.
  AND volume_desired_state.desired_status IS NOT 'Deleting'
"#;

/// Read a volume's `volumes`-table summary (#380): the clone path uses it
/// to materialize the target volume row with the source's shape.
const GET_VOLUME_SUMMARY_SQL: &str = r#"
SELECT node_id, display_name, capacity_bytes, volume_kind, storage_class, owner_id
FROM volumes
WHERE volume_id = $1
"#;

/// Row shape returned by [`DesiredStateRepository::get_volume_summary`].
#[derive(sqlx::FromRow, Clone, Debug)]
pub struct VolumeSummaryRow {
    pub node_id: Option<String>,
    pub display_name: String,
    pub capacity_bytes: i64,
    pub volume_kind: Option<String>,
    pub storage_class: Option<String>,
    pub owner_id: Option<String>,
}

/// What the clone path knows BEFORE the write transaction opens (#384).
/// The source-derived shape fields (`capacity_bytes`, `volume_kind`,
/// `storage_class`, `owner_id`) are deliberately absent: they are read
/// from the source row INSIDE the transaction, under the same
/// `BEGIN IMMEDIATE` lock that guards the target insert, so a racing
/// resize of the source cannot split the read from the target write.
#[derive(Clone)]
pub struct CloneTargetSpec {
    pub target_volume_id: ResourceId,
    /// The node the target materializes on (the source's node; the
    /// operation record journals this same node — #381 placement).
    pub placement_node_id: Option<NodeId>,
    pub display_name: String,
    pub desired_generation: Generation,
    pub requested_by: Option<String>,
    pub requested_unix_ms: i64,
}

/// Outcome of [`DesiredStateRepository::materialize_clone_target`].
#[derive(Clone, Debug)]
pub struct CloneTargetMaterialization {
    /// The source row as read inside the write transaction — the shape
    /// the target was materialized from (or, on an idempotent replay,
    /// the source's current shape).
    pub source: VolumeSummaryRow,
    /// `false` when the target row already existed and was accepted as
    /// this operation's own earlier materialization (idempotent replay:
    /// nothing was written).
    pub created: bool,
}

#[derive(Clone)]
pub struct DesiredStateRepository {
    pool: StorePool,
}

impl DesiredStateRepository {
    pub fn new(pool: StorePool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &StorePool {
        &self.pool
    }

    /// Persist a VM desired-state fragment.
    ///
    /// `audit` is the request's pre-minted audit-outbox row (#706). The
    /// desired-state transaction is opened either way; when a row is
    /// present it is enqueued as the transaction's last fallible
    /// statement, so the evidence commits with the fragment or not at
    /// all (a stale generation rolls both back).
    pub async fn upsert_vm(
        &self,
        input: &VmDesiredStateInput,
        audit: Option<&AuditOutboxInsert>,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        let generation = generation_to_i64(input.desired_generation)?;

        sqlx::query(UPSERT_VM_SQL)
            .bind(input.vm_id.as_str())
            .bind(input.node_id.as_ref().map(NodeId::as_str))
            .bind(&input.display_name)
            .bind(&input.tenant_id)
            .bind(&input.placement_policy)
            .bind(input.requested_unix_ms)
            .execute(&mut *tx)
            .await?;

        let result = sqlx::query(UPSERT_VM_DESIRED_STATE_SQL)
            .bind(input.vm_id.as_str())
            .bind(generation)
            .bind(&input.desired_status)
            .bind(&input.requested_by)
            .bind(&input.updated_by)
            .bind(input.target_node_id.as_ref().map(NodeId::as_str))
            .bind(input.cpu_count)
            .bind(input.memory_bytes)
            .bind(&input.image_ref)
            .bind(&input.boot_mode)
            .bind(&input.desired_power_state)
            .bind(input.vsock_enabled)
            .bind(input.requested_unix_ms)
            .execute(&mut *tx)
            .await?;

        if result.rows_affected() == 0 {
            return Err(StoreError::StaleGeneration {
                entity: "vm",
                id: input.vm_id.to_string(),
                incoming: generation,
            });
        }

        if let Some(audit) = audit {
            enqueue_adopted_row(&mut tx, audit).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn set_vm_power_state(
        &self,
        input: &VmPowerStatePatchInput,
    ) -> Result<(), StoreError> {
        let generation = generation_to_i64(input.desired_generation)?;
        let result = sqlx::query(PATCH_VM_POWER_STATE_SQL)
            .bind(input.vm_id.as_str())
            .bind(generation)
            .bind(&input.desired_status)
            .bind(&input.requested_by)
            .bind(&input.updated_by)
            .bind(input.target_node_id.as_ref().map(NodeId::as_str))
            .bind(&input.desired_power_state)
            .bind(input.requested_unix_ms)
            .execute(&self.pool)
            .await
            .map_err(|e| match &e {
                sqlx::Error::Database(db_err) if db_err.is_foreign_key_violation() => {
                    StoreError::NotFound {
                        entity: "vm",
                        id: input.vm_id.to_string(),
                    }
                }
                _ => StoreError::from(e),
            })?;
        if result.rows_affected() == 0 {
            return Err(StoreError::StaleGeneration {
                entity: "vm",
                id: input.vm_id.to_string(),
                incoming: generation,
            });
        }
        Ok(())
    }

    pub async fn set_vm_resources(&self, input: &VmResourcesPatchInput) -> Result<(), StoreError> {
        let generation = generation_to_i64(input.desired_generation)?;
        let rows = sqlx::query(PATCH_VM_RESOURCES_SQL)
            .bind(input.vm_id.as_str())
            .bind(input.cpu_count)
            .bind(input.memory_bytes)
            .bind(generation)
            .bind(&input.requested_by)
            .bind(input.target_node_id.as_ref().map(NodeId::as_str))
            .bind(input.requested_unix_ms)
            .execute(&self.pool)
            .await
            .map_err(|e| match &e {
                sqlx::Error::Database(db_err) if db_err.is_foreign_key_violation() => {
                    StoreError::NotFound {
                        entity: "vm",
                        id: input.vm_id.to_string(),
                    }
                }
                _ => StoreError::from(e),
            })?;
        if rows.rows_affected() == 0 {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM vm_desired_state WHERE vm_id = $1)",
            )
            .bind(input.vm_id.as_str())
            .fetch_one(&self.pool)
            .await?;
            if exists {
                return Err(StoreError::StaleGeneration {
                    entity: "vm",
                    id: input.vm_id.to_string(),
                    incoming: generation,
                });
            }
            return Err(StoreError::NotFound {
                entity: "vm",
                id: input.vm_id.to_string(),
            });
        }
        Ok(())
    }

    /// Persist a volume desired-state fragment.
    ///
    /// `audit` follows [`Self::upsert_vm`]: when present the row is
    /// enqueued inside the desired-state transaction, so a stale
    /// generation commits neither the fragment nor its evidence.
    pub async fn upsert_volume(
        &self,
        input: &VolumeDesiredStateInput,
        audit: Option<&AuditOutboxInsert>,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        let generation = generation_to_i64(input.desired_generation)?;

        sqlx::query(UPSERT_VOLUME_SQL)
            .bind(input.volume_id.as_str())
            .bind(input.node_id.as_ref().map(NodeId::as_str))
            .bind(&input.display_name)
            .bind(input.capacity_bytes)
            .bind(&input.volume_kind)
            .bind(&input.storage_class)
            .bind(&input.owner_id)
            .bind(input.requested_unix_ms)
            .execute(&mut *tx)
            .await?;

        let result = sqlx::query(UPSERT_VOLUME_DESIRED_STATE_SQL)
            .bind(input.volume_id.as_str())
            .bind(generation)
            .bind(&input.desired_status)
            .bind(&input.requested_by)
            .bind(&input.updated_by)
            .bind(input.attached_vm_id.as_ref().map(ResourceId::as_str))
            .bind(&input.attachment_mode)
            .bind(&input.device_name)
            .bind(input.read_only)
            .bind(input.resize_to_bytes)
            .bind(&input.snapshot_op)
            .bind(&input.snapshot_name)
            .bind(
                input
                    .clone_source_volume_id
                    .as_ref()
                    .map(ResourceId::as_str),
            )
            .bind(input.requested_unix_ms)
            .execute(&mut *tx)
            .await?;

        if result.rows_affected() == 0 {
            return Err(StoreError::StaleGeneration {
                entity: "volume",
                id: input.volume_id.to_string(),
                incoming: generation,
            });
        }

        if let Some(audit) = audit {
            enqueue_adopted_row(&mut tx, audit).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// #522 (review S1): disambiguate a blocked volume-desired-state
    /// patch UPSERT. The conflict arms of the three mutation-verb
    /// patches (attach/detach, resize, snapshot) refuse to journal
    /// over a `'Deleting'` tombstone at the SQL level; this helper
    /// turns that block into a loud [`StoreError::Conflict`] (gRPC
    /// ALREADY_EXISTS / HTTP 409 through the BFF's `map_ack`, the
    /// #384 precedent) instead of the pre-existing stale-generation
    /// refusal, so a client racing a delete sees *why* the mutation
    /// was refused. The wall-clock generations the control plane
    /// mints would otherwise always beat the tombstone's small
    /// integer, letting a late-arriving mutation re-attach (or
    /// re-flag) a volume whose backing the delete is concurrently
    /// reclaiming.
    ///
    /// The general [`Self::upsert_volume`] path (agent-fragment
    /// reconcile) needs no such guard: the tombstone writes
    /// `desired_generation + 1` over the row's current maximum, and
    /// the delete's agent handler evicts the volume from its cache,
    /// so any fragment still in flight carries a generation the
    /// existing `<=` guard already refuses.
    async fn refuse_volume_patch_if_deleting(
        &self,
        volume_id: &ResourceId,
        generation: i64,
    ) -> Result<(), StoreError> {
        let mut conn = self.pool.acquire().await?;
        Self::refuse_volume_patch_if_deleting_on(&mut conn, volume_id, generation).await
    }

    /// [`Self::refuse_volume_patch_if_deleting`] on an explicit connection.
    ///
    /// The adopted [`Self::set_volume_snapshot`] path must run this check
    /// on the SAME connection that holds its `BEGIN IMMEDIATE` write
    /// transaction. The pool-level helper would acquire a *second*
    /// connection while the first still holds the write lock; under
    /// concurrent adopted mutations that stalls on connection acquisition
    /// (shipped configs allow 4 connections; the in-memory test pool holds
    /// one and blocks outright). Running it on the held connection needs no
    /// extra connection and observes exactly the row state the patch saw.
    async fn refuse_volume_patch_if_deleting_on(
        conn: &mut sqlx::SqliteConnection,
        volume_id: &ResourceId,
        generation: i64,
    ) -> Result<(), StoreError> {
        let status: Option<Option<String>> = sqlx::query_scalar(
            "SELECT desired_status FROM volume_desired_state WHERE volume_id = $1",
        )
        .bind(volume_id.as_str())
        .fetch_optional(&mut *conn)
        .await?;
        if matches!(&status, Some(Some(status)) if status == "Deleting") {
            return Err(StoreError::Conflict {
                entity: "volume",
                id: volume_id.to_string(),
                reason: "volume is deleting; the mutation verb was refused",
            });
        }
        Err(StoreError::StaleGeneration {
            entity: "volume",
            id: volume_id.to_string(),
            incoming: generation,
        })
    }

    pub async fn set_volume_attachment(
        &self,
        input: &VolumeAttachmentPatchInput,
    ) -> Result<(), StoreError> {
        let generation = generation_to_i64(input.desired_generation)?;
        let result = sqlx::query(PATCH_VOLUME_ATTACHMENT_SQL)
            .bind(input.volume_id.as_str())
            .bind(generation)
            .bind(&input.desired_status)
            .bind(&input.requested_by)
            .bind(&input.updated_by)
            .bind(input.attached_vm_id.as_ref().map(ResourceId::as_str))
            .bind(input.requested_unix_ms)
            .execute(&self.pool)
            .await
            .map_err(|e| match &e {
                sqlx::Error::Database(db_err) if db_err.is_foreign_key_violation() => {
                    let (entity, id) = if input.attached_vm_id.is_some() {
                        (
                            "vm",
                            input
                                .attached_vm_id
                                .as_ref()
                                .map(|vm| vm.to_string())
                                .unwrap_or_default(),
                        )
                    } else {
                        ("volume", input.volume_id.to_string())
                    };
                    StoreError::NotFound { entity, id }
                }
                _ => StoreError::from(e),
            })?;
        if result.rows_affected() == 0 {
            // #522 (review S1): a 'Deleting' tombstone blocks the
            // conflict arm (see the SQL) — refuse loudly instead of
            // reporting a stale generation.
            return self
                .refuse_volume_patch_if_deleting(&input.volume_id, generation)
                .await;
        }
        Ok(())
    }

    pub async fn set_volume_resize(
        &self,
        input: &VolumeResizePatchInput,
    ) -> Result<(), StoreError> {
        let generation = generation_to_i64(input.desired_generation)?;
        let result = sqlx::query(PATCH_VOLUME_RESIZE_SQL)
            .bind(input.volume_id.as_str())
            .bind(generation)
            .bind(&input.desired_status)
            .bind(&input.requested_by)
            .bind(&input.updated_by)
            .bind(input.resize_to_bytes)
            .bind(input.requested_unix_ms)
            .execute(&self.pool)
            .await
            .map_err(|e| match &e {
                sqlx::Error::Database(db_err) if db_err.is_foreign_key_violation() => {
                    StoreError::NotFound {
                        entity: "volume",
                        id: input.volume_id.to_string(),
                    }
                }
                _ => StoreError::from(e),
            })?;
        if result.rows_affected() == 0 {
            // #522 (review S1): a 'Deleting' tombstone blocks the
            // conflict arm (see the SQL) — refuse loudly instead of
            // reporting a stale generation.
            return self
                .refuse_volume_patch_if_deleting(&input.volume_id, generation)
                .await;
        }
        Ok(())
    }

    /// Persist a volume snapshot-family intent fragment (create, restore,
    /// or delete).
    ///
    /// `audit` is the request's pre-minted audit-outbox row (#706 PR 4).
    /// When present, the intent patch, its `'Deleting'`-tombstone check,
    /// and the outbox enqueue share one `BEGIN IMMEDIATE` transaction, so
    /// the evidence commits with the snapshot/restore/delete intent or
    /// not at all. Every other caller passes `None` and keeps the
    /// original autocommit statement byte-for-byte.
    pub async fn set_volume_snapshot(
        &self,
        input: &VolumeSnapshotPatchInput,
        audit: Option<&AuditOutboxInsert>,
    ) -> Result<(), StoreError> {
        match audit {
            None => {
                let mut conn = self.pool.acquire().await?;
                Self::set_volume_snapshot_on(&mut conn, input).await
            }
            Some(audit) => {
                let mut tx = self.pool.begin_with("BEGIN IMMEDIATE;").await?;
                Self::set_volume_snapshot_on(&mut tx, input).await?;
                enqueue_adopted_row(&mut tx, audit).await?;
                tx.commit().await?;
                Ok(())
            }
        }
    }

    async fn set_volume_snapshot_on(
        conn: &mut sqlx::SqliteConnection,
        input: &VolumeSnapshotPatchInput,
    ) -> Result<(), StoreError> {
        let generation = generation_to_i64(input.desired_generation)?;
        let result = sqlx::query(PATCH_VOLUME_SNAPSHOT_SQL)
            .bind(input.volume_id.as_str())
            .bind(generation)
            .bind(&input.desired_status)
            .bind(&input.requested_by)
            .bind(&input.updated_by)
            .bind(&input.snapshot_op)
            .bind(&input.snapshot_name)
            .bind(input.requested_unix_ms)
            .execute(&mut *conn)
            .await
            .map_err(|e| match &e {
                sqlx::Error::Database(db_err) if db_err.is_foreign_key_violation() => {
                    StoreError::NotFound {
                        entity: "volume",
                        id: input.volume_id.to_string(),
                    }
                }
                _ => StoreError::from(e),
            })?;
        if result.rows_affected() == 0 {
            // #522 (review S1): a 'Deleting' tombstone blocks the
            // conflict arm (see the SQL) — refuse loudly instead of
            // reporting a stale generation. This must run on `conn` (the
            // same connection that holds the adopted write transaction):
            // the pool-level helper would acquire a *second* connection
            // while the first still holds the write lock; under concurrent
            // adopted mutations that stalls on connection acquisition
            // (shipped configs allow 4 connections; the in-memory test
            // pool holds one and blocks outright). Running it here needs
            // no extra connection and observes exactly the row state the
            // patch saw.
            return Self::refuse_volume_patch_if_deleting_on(conn, &input.volume_id, generation)
                .await;
        }
        Ok(())
    }

    /// Read a volume's `volumes`-table summary (node, size, class). Used by
    /// the clone path to materialize the target volume row with the
    /// source's shape (#380: the clone intent used to PATCH a
    /// `volume_desired_state` row for a target volume that nothing ever
    /// created — the FK violation surfaced as a bare
    /// `volume with id {target} not found`).
    pub async fn get_volume_summary(
        &self,
        volume_id: &ResourceId,
    ) -> Result<Option<VolumeSummaryRow>, StoreError> {
        let row = sqlx::query_as::<_, VolumeSummaryRow>(GET_VOLUME_SUMMARY_SQL)
            .bind(volume_id.as_str())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    /// Materialize a clone's target volume row and its desired-state
    /// intent row in ONE `BEGIN IMMEDIATE` transaction (#384).
    ///
    /// This closes the clone path's check-then-upsert TOCTOU and its
    /// source-read/write skew in one place:
    ///
    /// - the source row is read INSIDE the write transaction, so a
    ///   resize of the source committing between the lifecycle's
    ///   pre-check read and this call cannot shape the target from a
    ///   stale capacity (the read and the target write serialize under
    ///   the same RESERVED lock — the resize executor's
    ///   `UPDATE volumes SET capacity_bytes` is the only unguarded
    ///   direct writer this closes);
    /// - the target insert is STRICT (`ON CONFLICT DO NOTHING` +
    ///   rows_affected), so a concurrent same-target materialization
    ///   fails closed with [`StoreError::Conflict`] instead of
    ///   last-writer-wins overwriting the winner's shape;
    /// - an idempotent replay of the same `meta.operation_id`
    ///   (`own_replay = true`) whose conflicting row is exactly the
    ///   shape this operation materialized earlier is an idempotent
    ///   success (`created = false`, nothing written) — anything else
    ///   that collides is a racing different request and fails closed.
    ///
    /// The general [`Self::upsert_volume`] path (agent fragment
    /// reconcile) is deliberately untouched: clone is the only writer
    /// that must not see a pre-existing target row.
    pub async fn materialize_clone_target(
        &self,
        source_volume_id: &ResourceId,
        spec: &CloneTargetSpec,
        own_replay: bool,
    ) -> Result<CloneTargetMaterialization, StoreError> {
        // BEGIN IMMEDIATE: acquire SQLite's RESERVED lock at transaction
        // start, serializing concurrent writers from the get-go (repo
        // standard for check-then-write pairs — the BFF's VM-create quota
        // transaction and network delete use the same shape). Without
        // IMMEDIATE, two racing clones could both pass the strict insert
        // inside their DEFERRED transactions and deadlock/overwrite.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE;").await?;
        let generation = generation_to_i64(spec.desired_generation)?;

        // Source read under the write lock: the shape below is the
        // source's committed state as of this transaction, not as of the
        // lifecycle's earlier pre-check read.
        let source = sqlx::query_as::<_, VolumeSummaryRow>(GET_VOLUME_SUMMARY_SQL)
            .bind(source_volume_id.as_str())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| StoreError::NotFound {
                entity: "volume",
                id: source_volume_id.to_string(),
            })?;

        let insert = sqlx::query(INSERT_VOLUME_FOR_CLONE_SQL)
            .bind(spec.target_volume_id.as_str())
            .bind(spec.placement_node_id.as_ref().map(NodeId::as_str))
            .bind(&spec.display_name)
            .bind(source.capacity_bytes)
            .bind(&source.volume_kind)
            .bind(&source.storage_class)
            .bind(&source.owner_id)
            .bind(spec.requested_unix_ms)
            .execute(&mut *tx)
            .await?;

        if insert.rows_affected() == 0 {
            // The target row exists. Two ways to get here:
            //
            // 1. a racing different request materialized it first —
            //    fail closed with Conflict (the request was well-formed
            //    and the target genuinely existed at persist time);
            // 2. THIS operation already materialized it on an earlier
            //    attempt (an idempotent replay of meta.operation_id) —
            //    accept it, but only if the existing row matches this
            //    operation's shape on the fields compared below
            //    (placement node, capacity, kind, storage class, owner);
            //    display_name is deliberately excluded — a cloned row
            //    always carries the target id as its name, so it cannot
            //    distinguish this operation's write from a foreign one.
            //    Anything else is a foreign row that happens to sit on
            //    the target id and must fail closed too.
            if own_replay {
                let existing = sqlx::query_as::<_, VolumeSummaryRow>(GET_VOLUME_SUMMARY_SQL)
                    .bind(spec.target_volume_id.as_str())
                    .fetch_optional(&mut *tx)
                    .await?;
                let matches = existing.is_some_and(|row| {
                    row.node_id.as_deref() == spec.placement_node_id.as_ref().map(NodeId::as_str)
                        && row.capacity_bytes == source.capacity_bytes
                        && row.volume_kind == source.volume_kind
                        && row.storage_class == source.storage_class
                        && row.owner_id == source.owner_id
                });
                if matches {
                    return Ok(CloneTargetMaterialization {
                        source,
                        created: false,
                    });
                }
            }
            return Err(StoreError::Conflict {
                entity: "volume",
                id: spec.target_volume_id.to_string(),
                reason: "target volume id already materialized by a concurrent request",
            });
        }

        // The desired-state intent row, generation-guarded, in the same
        // transaction — same shape as `upsert_volume`'s second statement.
        // A stale generation rolls the physical insert back with it.
        let result = sqlx::query(UPSERT_VOLUME_DESIRED_STATE_SQL)
            .bind(spec.target_volume_id.as_str())
            .bind(generation)
            .bind(None::<String>) // desired_status
            .bind(&spec.requested_by)
            .bind(None::<String>) // updated_by
            .bind(None::<&str>) // attached_vm_id
            .bind(None::<String>) // attachment_mode
            .bind(None::<String>) // device_name
            .bind(false) // read_only
            .bind(None::<i64>) // resize_to_bytes
            .bind(None::<String>) // snapshot_op
            .bind(None::<String>) // snapshot_name
            .bind(source_volume_id.as_str()) // clone_source_volume_id
            .bind(spec.requested_unix_ms)
            .execute(&mut *tx)
            .await?;

        if result.rows_affected() == 0 {
            return Err(StoreError::StaleGeneration {
                entity: "volume",
                id: spec.target_volume_id.to_string(),
                incoming: generation,
            });
        }

        tx.commit().await?;
        Ok(CloneTargetMaterialization {
            source,
            created: true,
        })
    }

    /// #499 (the #522 review-S1 disambiguation, network-shaped): the
    /// conflict arms of [`UPSERT_NETWORK_DESIRED_STATE_SQL`] (the agent
    /// fragment path — the only production caller of the physical
    /// `networks` upsert) and [`PATCH_NETWORK_STATUS_SQL`] (the
    /// start/stop/restart mutation verbs) refuse to journal over a
    /// `'Deleting'` tombstone at the SQL level; this helper turns that
    /// block into a loud [`StoreError::Conflict`] (gRPC ALREADY_EXISTS
    /// / HTTP 409 through the BFF's `map_ack`, the #384 precedent)
    /// instead of the pre-existing stale-generation refusal. The
    /// network generations the control plane mints are small integers
    /// (BFF create writes 1, update bumps +1) while the mutation verbs
    /// carry wall-clock milliseconds, so the generation guard alone
    /// never blocks a late writer against a tombstone — and, unlike
    /// the volume case, the delete is BFF-direct with NO agent
    /// dispatch, so nothing but this term stands between a late
    /// fragment and a resurrected network.
    ///
    /// Takes an executor (not `&self.pool`) because the two fragment
    /// upserts are still inside their transaction when they disambiguate
    /// — a pool query there would wait on the connection the tx itself
    /// holds (deadlock on a single-connection pool). The tx-bound read
    /// sees the same committed tombstone the refused upsert just
    /// evaluated.
    async fn refuse_network_write_if_deleting<'e, E>(
        executor: E,
        network_id: &ResourceId,
        generation: i64,
    ) -> Result<(), StoreError>
    where
        E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
    {
        let status: Option<Option<String>> = sqlx::query_scalar(
            "SELECT desired_status FROM network_desired_state WHERE network_id = $1",
        )
        .bind(network_id.as_str())
        .fetch_optional(executor)
        .await?;
        if matches!(&status, Some(Some(status)) if status == "Deleting") {
            return Err(StoreError::Conflict {
                entity: "network",
                id: network_id.to_string(),
                reason: "network is deleting; the write was refused (the network stays deleted)",
            });
        }
        Err(StoreError::StaleGeneration {
            entity: "network",
            id: network_id.to_string(),
            incoming: generation,
        })
    }

    pub async fn upsert_network(&self, input: &NetworkDesiredStateInput) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        let generation = generation_to_i64(input.desired_generation)?;

        sqlx::query(UPSERT_NETWORK_SQL)
            .bind(input.network_id.as_str())
            .bind(input.node_id.as_ref().map(NodeId::as_str))
            .bind(&input.display_name)
            .bind(&input.network_class)
            .bind(input.requested_unix_ms)
            .execute(&mut *tx)
            .await?;

        let result = sqlx::query(UPSERT_NETWORK_DESIRED_STATE_SQL)
            .bind(input.network_id.as_str())
            .bind(generation)
            .bind(&input.desired_status)
            .bind(&input.requested_by)
            .bind(&input.updated_by)
            .bind(&input.firewall_rules_json)
            .bind(&input.nat_rules_json)
            .bind(&input.dhcp_scope_json)
            .bind(
                input
                    .dns_enabled
                    .map(|v| if v { 1 } else { 0 })
                    .unwrap_or(0),
            )
            .bind(&input.dns_scope_json)
            .bind(input.requested_unix_ms)
            .execute(&mut *tx)
            .await?;

        if result.rows_affected() == 0 {
            // #499: a 'Deleting' tombstone blocks the conflict arm (see
            // the SQL) — refuse loudly instead of reporting a stale
            // generation. The whole transaction (the physical `networks`
            // re-assert included) rolls back with this error.
            return Self::refuse_network_write_if_deleting(&mut *tx, &input.network_id, generation)
                .await;
        }

        tx.commit().await?;
        Ok(())
    }

    /// Persist a network desired-state fragment plus its exposure rows.
    ///
    /// `audit` follows [`Self::upsert_vm`]: when present the row is
    /// enqueued inside the desired-state transaction, so a blocked
    /// tombstone or a stale generation commits neither the fragment,
    /// the exposures, nor the evidence.
    pub async fn upsert_network_with_exposures(
        &self,
        input: &NetworkDesiredStateInput,
        exposures: &[NetworkExposureInput],
        audit: Option<&AuditOutboxInsert>,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        let generation = generation_to_i64(input.desired_generation)?;

        sqlx::query(UPSERT_NETWORK_SQL)
            .bind(input.network_id.as_str())
            .bind(input.node_id.as_ref().map(NodeId::as_str))
            .bind(&input.display_name)
            .bind(&input.network_class)
            .bind(input.requested_unix_ms)
            .execute(&mut *tx)
            .await?;

        let result = sqlx::query(UPSERT_NETWORK_DESIRED_STATE_SQL)
            .bind(input.network_id.as_str())
            .bind(generation)
            .bind(&input.desired_status)
            .bind(&input.requested_by)
            .bind(&input.updated_by)
            .bind(&input.firewall_rules_json)
            .bind(&input.nat_rules_json)
            .bind(&input.dhcp_scope_json)
            .bind(
                input
                    .dns_enabled
                    .map(|v| if v { 1 } else { 0 })
                    .unwrap_or(0),
            )
            .bind(&input.dns_scope_json)
            .bind(input.requested_unix_ms)
            .execute(&mut *tx)
            .await?;

        if result.rows_affected() == 0 {
            // #499: a 'Deleting' tombstone blocks the conflict arm (see
            // the SQL) — refuse loudly instead of reporting a stale
            // generation. The whole transaction (the physical `networks`
            // re-assert and every exposure row included) rolls back with
            // this error: a late agent fragment for a deleted network is
            // dropped, never resurrecting anything.
            return Self::refuse_network_write_if_deleting(&mut *tx, &input.network_id, generation)
                .await;
        }

        for exposure in exposures {
            sqlx::query(crate::network_exposures::UPSERT_SQL)
                .bind(exposure.network_id.as_str())
                .bind(&exposure.service_name)
                .bind(&exposure.protocol)
                .bind(&exposure.listen_address)
                .bind(exposure.listen_port)
                .bind(&exposure.target_address)
                .bind(exposure.target_port)
                .bind(&exposure.exposure_policy)
                .bind(exposure.updated_unix_ms)
                .execute(&mut *tx)
                .await
                .map_err(|e| match &e {
                    sqlx::Error::Database(db_err) if db_err.is_foreign_key_violation() => {
                        StoreError::NotFound {
                            entity: "network",
                            id: exposure.network_id.to_string(),
                        }
                    }
                    _ => StoreError::from(e),
                })?;
        }

        if let Some(audit) = audit {
            enqueue_adopted_row(&mut tx, audit).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn set_network_status(
        &self,
        input: &NetworkStatusPatchInput,
    ) -> Result<(), StoreError> {
        let generation = generation_to_i64(input.desired_generation)?;
        let result = sqlx::query(PATCH_NETWORK_STATUS_SQL)
            .bind(input.network_id.as_str())
            .bind(generation)
            .bind(&input.desired_status)
            .bind(&input.requested_by)
            .bind(&input.updated_by)
            .bind(input.requested_unix_ms)
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
        if result.rows_affected() == 0 {
            // #499 (the #522 review-S1 sibling-verb guard): a 'Deleting'
            // tombstone blocks the conflict arm (see the SQL) — refuse
            // loudly instead of reporting a stale generation.
            return Self::refuse_network_write_if_deleting(
                &self.pool,
                &input.network_id,
                generation,
            )
            .await;
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct VmDesiredStateInput {
    pub vm_id: ResourceId,
    pub node_id: Option<NodeId>,
    pub display_name: String,
    pub tenant_id: Option<String>,
    pub placement_policy: Option<String>,
    pub desired_generation: Generation,
    pub desired_status: Option<String>,
    pub requested_by: Option<String>,
    pub updated_by: Option<String>,
    pub target_node_id: Option<NodeId>,
    pub cpu_count: Option<i32>,
    pub memory_bytes: Option<i64>,
    pub image_ref: Option<String>,
    pub boot_mode: Option<String>,
    pub desired_power_state: Option<String>,
    /// G5 vsock device opt-in. `None` means "not specified": a fresh
    /// insert defaults to 0 (off), and an update preserves the stored
    /// value (the COALESCE in the upsert) so callers that do not carry
    /// the field cannot silently strip a VM's device intent.
    pub vsock_enabled: Option<bool>,
    pub requested_unix_ms: i64,
}

#[derive(Clone)]
pub struct VmPowerStatePatchInput {
    pub vm_id: ResourceId,
    pub desired_generation: Generation,
    pub desired_status: Option<String>,
    pub requested_by: Option<String>,
    pub updated_by: Option<String>,
    pub target_node_id: Option<NodeId>,
    pub desired_power_state: Option<String>,
    pub requested_unix_ms: i64,
}

#[derive(Clone)]
pub struct VmResourcesPatchInput {
    pub vm_id: ResourceId,
    pub cpu_count: Option<i32>,
    pub memory_bytes: Option<i64>,
    pub desired_generation: Generation,
    pub requested_by: Option<String>,
    pub target_node_id: Option<NodeId>,
    pub requested_unix_ms: i64,
}

#[derive(Clone)]
pub struct VolumeDesiredStateInput {
    pub volume_id: ResourceId,
    pub node_id: Option<NodeId>,
    pub display_name: String,
    pub capacity_bytes: i64,
    pub volume_kind: Option<String>,
    pub storage_class: Option<String>,
    /// Ownership for a NEW volumes row (clone materialization copies the
    /// source's owner). NULL on conflict preserves the existing owner.
    pub owner_id: Option<String>,
    pub desired_generation: Generation,
    pub desired_status: Option<String>,
    pub requested_by: Option<String>,
    pub updated_by: Option<String>,
    pub attached_vm_id: Option<ResourceId>,
    pub attachment_mode: Option<String>,
    pub device_name: Option<String>,
    pub read_only: bool,
    pub resize_to_bytes: Option<i64>,
    pub snapshot_op: Option<String>,
    pub snapshot_name: Option<String>,
    pub clone_source_volume_id: Option<ResourceId>,
    pub requested_unix_ms: i64,
}

#[derive(Clone)]
pub struct VolumeAttachmentPatchInput {
    pub volume_id: ResourceId,
    pub desired_generation: Generation,
    pub desired_status: Option<String>,
    pub requested_by: Option<String>,
    pub updated_by: Option<String>,
    pub attached_vm_id: Option<ResourceId>,
    pub requested_unix_ms: i64,
}

#[derive(Clone)]
pub struct VolumeResizePatchInput {
    pub volume_id: ResourceId,
    pub desired_generation: Generation,
    pub desired_status: Option<String>,
    pub requested_by: Option<String>,
    pub updated_by: Option<String>,
    pub resize_to_bytes: Option<i64>,
    pub requested_unix_ms: i64,
}

#[derive(Clone)]
pub struct VolumeSnapshotPatchInput {
    pub volume_id: ResourceId,
    pub desired_generation: Generation,
    pub desired_status: Option<String>,
    pub requested_by: Option<String>,
    pub updated_by: Option<String>,
    pub snapshot_op: Option<String>,
    pub snapshot_name: Option<String>,
    pub requested_unix_ms: i64,
}

#[derive(Clone)]
pub struct NetworkDesiredStateInput {
    pub network_id: ResourceId,
    pub node_id: Option<NodeId>,
    pub display_name: String,
    pub network_class: Option<String>,
    pub desired_generation: Generation,
    pub desired_status: Option<String>,
    pub requested_by: Option<String>,
    pub updated_by: Option<String>,
    pub firewall_rules_json: Option<String>,
    pub nat_rules_json: Option<String>,
    pub dhcp_scope_json: Option<String>,
    pub dns_enabled: Option<bool>,
    pub dns_scope_json: Option<String>,
    pub requested_unix_ms: i64,
}

#[derive(Clone)]
pub struct NetworkStatusPatchInput {
    pub network_id: ResourceId,
    pub desired_generation: Generation,
    pub desired_status: Option<String>,
    pub requested_by: Option<String>,
    pub updated_by: Option<String>,
    pub requested_unix_ms: i64,
}

fn generation_to_i64(generation: Generation) -> Result<i64, StoreError> {
    i64::try_from(generation.get()).map_err(|source| StoreError::InvalidConfiguration {
        reason: format!("generation out of range for bigint column: {source}"),
    })
}
