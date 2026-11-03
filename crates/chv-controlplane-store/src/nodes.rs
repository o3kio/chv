use crate::{StoreError, StorePool};
use chv_controlplane_types::domain::{Generation, NodeId, NodeState};

/// `node_inventory.authority_mode` values (#378) — the agent config's
/// kebab-case authority-mode spellings, persisted verbatim by inventory
/// ingestion and compared by accept-time policy checks.
pub const AUTHORITY_MODE_LEGACY: &str = "legacy";
pub const AUTHORITY_MODE_CORE_MANAGED: &str = "core-managed";
pub const AUTHORITY_MODE_CORE_NATIVE: &str = "core-native";

const UPSERT_NODE_SQL: &str = r#"
INSERT INTO nodes (
    node_id,
    hostname,
    display_name,
    certificate_serial,
    agent_version,
    control_plane_version,
    enrolled_at,
    last_seen_at,
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
    strftime('%Y-%m-%dT%H:%M:%SZ', $8 / 1000.0, 'unixepoch'),
    strftime('%Y-%m-%dT%H:%M:%SZ', $8 / 1000.0, 'unixepoch')
)
ON CONFLICT (node_id) DO UPDATE SET
    hostname = EXCLUDED.hostname,
    display_name = EXCLUDED.display_name,
    certificate_serial = EXCLUDED.certificate_serial,
    agent_version = EXCLUDED.agent_version,
    control_plane_version = EXCLUDED.control_plane_version,
    enrolled_at = COALESCE(EXCLUDED.enrolled_at, nodes.enrolled_at),
    last_seen_at = EXCLUDED.last_seen_at,
    updated_at = EXCLUDED.updated_at
"#;

const ENSURE_NODE_RECORD_SQL: &str = r#"
INSERT INTO nodes (
    node_id,
    hostname,
    display_name,
    enrolled_at,
    last_seen_at,
    updated_at
)
VALUES (
    $1,
    $2,
    $3,
    strftime('%Y-%m-%dT%H:%M:%SZ', $4 / 1000.0, 'unixepoch'),
    strftime('%Y-%m-%dT%H:%M:%SZ', $4 / 1000.0, 'unixepoch'),
    strftime('%Y-%m-%dT%H:%M:%SZ', $4 / 1000.0, 'unixepoch')
)
ON CONFLICT (node_id) DO UPDATE SET
    hostname = CASE
        WHEN nodes.hostname = nodes.node_id OR nodes.hostname = '' THEN EXCLUDED.hostname
        ELSE nodes.hostname
    END,
    display_name = CASE
        WHEN nodes.display_name = nodes.node_id OR nodes.display_name = '' THEN EXCLUDED.display_name
        ELSE nodes.display_name
    END,
    last_seen_at = EXCLUDED.last_seen_at,
    updated_at = EXCLUDED.updated_at
"#;

const UPSERT_NODE_INVENTORY_SQL: &str = r#"
INSERT INTO node_inventory (
    node_id,
    architecture,
    kernel_version,
    os_release,
    cpu_count,
    memory_bytes,
    disk_bytes,
    cloud_hypervisor_version,
    chv_agent_version,
    chv_stord_version,
    chv_nwd_version,
    host_bundle_version,
    inventory_status,
    storage_classes,
    network_capabilities,
    labels,
    hypervisor_capabilities,
    authority_mode,
    last_reported_at,
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
    $14,
    $15,
    $16,
    $17,
    $18,
    strftime('%Y-%m-%dT%H:%M:%SZ', $19 / 1000.0, 'unixepoch'),
    strftime('%Y-%m-%dT%H:%M:%SZ', $19 / 1000.0, 'unixepoch')
)
ON CONFLICT (node_id) DO UPDATE SET
    architecture = EXCLUDED.architecture,
    kernel_version = EXCLUDED.kernel_version,
    os_release = EXCLUDED.os_release,
    cpu_count = EXCLUDED.cpu_count,
    memory_bytes = EXCLUDED.memory_bytes,
    disk_bytes = EXCLUDED.disk_bytes,
    cloud_hypervisor_version = COALESCE(EXCLUDED.cloud_hypervisor_version, node_inventory.cloud_hypervisor_version),
    chv_agent_version = COALESCE(EXCLUDED.chv_agent_version, node_inventory.chv_agent_version),
    chv_stord_version = COALESCE(EXCLUDED.chv_stord_version, node_inventory.chv_stord_version),
    chv_nwd_version = COALESCE(EXCLUDED.chv_nwd_version, node_inventory.chv_nwd_version),
    host_bundle_version = COALESCE(EXCLUDED.host_bundle_version, node_inventory.host_bundle_version),
    inventory_status = EXCLUDED.inventory_status,
    storage_classes = EXCLUDED.storage_classes,
    network_capabilities = EXCLUDED.network_capabilities,
    labels = EXCLUDED.labels,
    hypervisor_capabilities = EXCLUDED.hypervisor_capabilities,
    -- COALESCE, like the version columns: an agent that reports no mode
    -- (pre-#378 binaries, UNSPECIFIED) must not wipe a mode a previous
    -- report established. A mode flip always reports a non-NULL value and
    -- overwrites.
    authority_mode = COALESCE(EXCLUDED.authority_mode, node_inventory.authority_mode),
    last_reported_at = EXCLUDED.last_reported_at,
    updated_at = EXCLUDED.updated_at
"#;

const INSERT_NODE_VERSION_SQL: &str = r#"
INSERT INTO node_versions (
    node_id,
    component_name,
    version,
    source,
    reported_at
)
VALUES (
    $1,
    $2,
    $3,
    $4,
    strftime('%Y-%m-%dT%H:%M:%SZ', $5 / 1000.0, 'unixepoch')
)
"#;

const UPDATE_NODE_CERTIFICATE_SQL: &str = r#"
UPDATE nodes SET
    certificate_serial = $2,
    updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')
WHERE node_id = $1
"#;

/// Compare-and-swap certificate rotation: only update when the caller still
/// holds the serial that is currently recorded for the node. This is the
/// store-level half of certificate-rotation proof-of-possession (#661): the
/// service checks the presented peer certificate serial before minting a new
/// cert, and this conditional write closes the remaining race between that
/// check and the commit.
const ROTATE_NODE_CERTIFICATE_SQL: &str = r#"
UPDATE nodes SET
    certificate_serial = $3,
    updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')
WHERE node_id = $1 AND certificate_serial = $2
"#;

const UPSERT_BOOTSTRAP_RESULT_SQL: &str = r#"
INSERT INTO node_bootstrap_results (
    node_id,
    operation_id,
    success,
    error_message,
    details,
    started_at,
    completed_at
)
VALUES (
    $1,
    $2,
    $3,
    $4,
    $5,
    strftime('%Y-%m-%dT%H:%M:%SZ', $6 / 1000.0, 'unixepoch'),
    strftime('%Y-%m-%dT%H:%M:%SZ', $7 / 1000.0, 'unixepoch')
)
ON CONFLICT (node_id) DO UPDATE SET
    operation_id = EXCLUDED.operation_id,
    success = EXCLUDED.success,
    error_message = EXCLUDED.error_message,
    details = EXCLUDED.details,
    started_at = EXCLUDED.started_at,
    completed_at = EXCLUDED.completed_at,
    updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')
"#;

const PATCH_NODE_STATE_PRESERVING_POLICY_SQL: &str = r#"
INSERT INTO node_desired_state (
    node_id,
    desired_generation,
    desired_state,
    requested_by,
    updated_by,
    state_reason,
    scheduling_paused,
    allow_workload_stop,
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
    false,
    NULL,
    strftime('%Y-%m-%dT%H:%M:%SZ', $7 / 1000.0, 'unixepoch'),
    strftime('%Y-%m-%dT%H:%M:%SZ', $7 / 1000.0, 'unixepoch')
)
ON CONFLICT (node_id) DO UPDATE SET
    desired_generation = EXCLUDED.desired_generation,
    desired_state = EXCLUDED.desired_state,
    requested_by = EXCLUDED.requested_by,
    updated_by = EXCLUDED.updated_by,
    state_reason = EXCLUDED.state_reason,
    requested_at = EXCLUDED.requested_at,
    updated_at = EXCLUDED.updated_at
"#;

const PATCH_NODE_SCHEDULING_SQL: &str = r#"
UPDATE node_desired_state SET
    desired_generation = $2,
    requested_by = $3,
    updated_by = $4,
    scheduling_paused = $5,
    requested_at = strftime('%Y-%m-%dT%H:%M:%SZ', $6 / 1000.0, 'unixepoch'),
    updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', $6 / 1000.0, 'unixepoch')
WHERE node_id = $1
"#;

const PATCH_NODE_DRAIN_INTENT_SQL: &str = r#"
INSERT INTO node_desired_state (
    node_id,
    desired_generation,
    desired_state,
    requested_by,
    updated_by,
    state_reason,
    scheduling_paused,
    allow_workload_stop,
    requested_at,
    updated_at
)
VALUES (
    $1,
    $2,
    'Draining',
    $3,
    $4,
    NULL,
    false,
    $5,
    strftime('%Y-%m-%dT%H:%M:%SZ', $6 / 1000.0, 'unixepoch'),
    strftime('%Y-%m-%dT%H:%M:%SZ', $6 / 1000.0, 'unixepoch')
)
ON CONFLICT (node_id) DO UPDATE SET
    desired_generation = EXCLUDED.desired_generation,
    desired_state = EXCLUDED.desired_state,
    requested_by = EXCLUDED.requested_by,
    updated_by = EXCLUDED.updated_by,
    allow_workload_stop = EXCLUDED.allow_workload_stop,
    requested_at = EXCLUDED.requested_at,
    updated_at = EXCLUDED.updated_at
"#;

#[derive(Clone)]
pub struct NodeRepository {
    pool: StorePool,
}

impl NodeRepository {
    pub fn new(pool: StorePool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &StorePool {
        &self.pool
    }

    pub async fn upsert_node(&self, input: &NodeUpsertInput) -> Result<(), StoreError> {
        sqlx::query(UPSERT_NODE_SQL)
            .bind(input.node_id.as_str())
            .bind(&input.hostname)
            .bind(&input.display_name)
            .bind(&input.certificate_serial)
            .bind(&input.agent_version)
            .bind(&input.control_plane_version)
            .bind(input.enrolled_unix_ms)
            .bind(input.last_seen_unix_ms)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn ensure_node_record(
        &self,
        node_id: &NodeId,
        hostname: Option<&str>,
        display_name: Option<&str>,
        seen_unix_ms: i64,
    ) -> Result<(), StoreError> {
        let fallback = node_id.as_str();
        sqlx::query(ENSURE_NODE_RECORD_SQL)
            .bind(fallback)
            .bind(hostname.unwrap_or(fallback))
            .bind(display_name.unwrap_or(fallback))
            .bind(seen_unix_ms)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Whether a node record exists. The monitoring ingest path checks
    /// this before accepting a batch (an unenrolled node gets a typed
    /// rejection, never an auto-created record — unlike
    /// [`Self::ensure_node_record`] on the state-report path, telemetry
    /// batches must not manufacture inventory).
    pub async fn node_exists(&self, node_id: &NodeId) -> Result<bool, StoreError> {
        let exists: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM nodes WHERE node_id = ? LIMIT 1")
                .bind(node_id.as_str())
                .fetch_optional(&self.pool)
                .await?;
        Ok(exists.is_some())
    }

    pub async fn upsert_inventory(&self, input: &NodeInventoryInput) -> Result<(), StoreError> {
        sqlx::query(UPSERT_NODE_INVENTORY_SQL)
            .bind(input.node_id.as_str())
            .bind(&input.architecture)
            .bind(&input.kernel_version)
            .bind(&input.os_release)
            .bind(input.cpu_count)
            .bind(input.memory_bytes)
            .bind(input.disk_bytes)
            .bind(&input.cloud_hypervisor_version)
            .bind(&input.chv_agent_version)
            .bind(&input.chv_stord_version)
            .bind(&input.chv_nwd_version)
            .bind(&input.host_bundle_version)
            .bind(&input.inventory_status)
            .bind(&input.storage_classes)
            .bind(&input.network_capabilities)
            .bind(&input.labels)
            .bind(&input.hypervisor_capabilities)
            .bind(&input.authority_mode)
            .bind(input.reported_unix_ms)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The node's last reported authority mode (#378): `None` when the
    /// node has no inventory row or has never reported a mode. Accept-time
    /// policy checks compare against
    /// [`AUTHORITY_MODE_CORE_MANAGED`] and fail OPEN on every other
    /// value — the agent's fail-closed dispatch remains the enforcement.
    pub async fn get_authority_mode(&self, node_id: &NodeId) -> Result<Option<String>, StoreError> {
        let mode = sqlx::query_scalar::<_, Option<String>>(
            "SELECT authority_mode FROM node_inventory WHERE node_id = ?",
        )
        .bind(node_id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        Ok(mode.flatten())
    }

    /// The node's last reported storage classes (#379 DP4): parsed from
    /// the `node_inventory.storage_classes` JSON column (the enrollment
    /// and inventory paths serialize the proto's repeated string as a
    /// JSON array of strings). EMPTY when the node has no inventory row,
    /// never reported classes, or the stored blob is not a JSON array of
    /// strings — the #379 accept-time check fails OPEN on empty, exactly
    /// like [`get_authority_mode`]'s `None` (the agent-side open remains
    /// the enforcement boundary).
    pub async fn get_storage_classes(&self, node_id: &NodeId) -> Result<Vec<String>, StoreError> {
        let blob = sqlx::query_scalar::<_, Option<String>>(
            "SELECT storage_classes FROM node_inventory WHERE node_id = ?",
        )
        .bind(node_id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        Ok(blob
            .flatten()
            .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
            .unwrap_or_default())
    }

    /// The #379 DP4 accept-time node-capability check, as ONE shared
    /// composition so its two callers cannot drift: the node's
    /// advertised classes ([`Self::get_storage_classes`]) must cover
    /// `requested` (`None` = classless = `local`), compared through the
    /// shared DP3-normalizing predicate
    /// (`chv_hypervisor_api::resources::node_offers_storage_class`).
    ///
    /// Returns `Ok(None)` to ACCEPT — a definite match, or the fail-open
    /// edge: a node that never reported classes (empty list) must never
    /// reject (the agent's open and stord's own backend validation
    /// remain the enforcement backstop). Returns `Ok(Some(reason))` only
    /// on the definite mismatch; the caller maps the reason to its own
    /// 400 shape. Callers must invoke this BEFORE any journaling: a
    /// rejection leaves no rows behind.
    ///
    /// Both accept-time surfaces live on this one implementation: the
    /// lifecycle service's RPC surface (`ensure_node_offers_storage_class`)
    /// and the BFF's `POST /v1/vms` create handler — the production
    /// create path, wired in the #512 second-pass follow-up (the RPC
    /// surface has no production caller, so the BFF must not re-derive
    /// the decision).
    pub async fn node_storage_class_rejection(
        &self,
        node_id: &NodeId,
        requested: Option<&str>,
        surface: &str,
    ) -> Result<Option<String>, StoreError> {
        let advertised = self.get_storage_classes(node_id).await?;
        Ok(
            match chv_hypervisor_api::resources::node_offers_storage_class(&advertised, requested) {
                Some(false) => Some(format!(
                    "node {} does not offer storage class {} (advertised: {}) for {}",
                    node_id,
                    requested.unwrap_or(chv_hypervisor_api::resources::DEFAULT_BACKEND_CLASS),
                    advertised.join(", "),
                    surface,
                )),
                // Some(true): definite match. None: the node never
                // reported classes — fail OPEN (see above).
                Some(true) | None => None,
            },
        )
    }

    pub async fn append_version(&self, input: &NodeVersionInput) -> Result<(), StoreError> {
        sqlx::query(INSERT_NODE_VERSION_SQL)
            .bind(input.node_id.as_str())
            .bind(&input.component_name)
            .bind(&input.version)
            .bind(&input.source)
            .bind(input.reported_unix_ms)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_state_preserving_policy(
        &self,
        input: &NodeStatePatchInput,
    ) -> Result<(), StoreError> {
        sqlx::query(PATCH_NODE_STATE_PRESERVING_POLICY_SQL)
            .bind(input.node_id.as_str())
            .bind(generation_to_i64(input.desired_generation)?)
            .bind(input.desired_state.as_str())
            .bind(&input.requested_by)
            .bind(&input.updated_by)
            .bind(&input.state_reason)
            .bind(input.requested_unix_ms)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_scheduling_paused(
        &self,
        input: &NodeSchedulingPatchInput,
    ) -> Result<(), StoreError> {
        let result = sqlx::query(PATCH_NODE_SCHEDULING_SQL)
            .bind(input.node_id.as_str())
            .bind(generation_to_i64(input.desired_generation)?)
            .bind(&input.requested_by)
            .bind(&input.updated_by)
            .bind(input.scheduling_paused)
            .bind(input.requested_unix_ms)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound {
                entity: "node_desired_state",
                id: input.node_id.to_string(),
            });
        }

        Ok(())
    }

    pub async fn set_drain_intent(&self, input: &NodeDrainIntentInput) -> Result<(), StoreError> {
        sqlx::query(PATCH_NODE_DRAIN_INTENT_SQL)
            .bind(input.node_id.as_str())
            .bind(generation_to_i64(input.desired_generation)?)
            .bind(&input.requested_by)
            .bind(&input.updated_by)
            .bind(input.allow_workload_stop)
            .bind(input.requested_unix_ms)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Read the certificate serial currently recorded for a node.
    ///
    /// A missing node row is a [`StoreError::NotFound`]; an existing legacy
    /// row whose `certificate_serial` is `NULL` is `Ok(None)` so callers can
    /// fail closed with an operator-actionable message instead of guessing a
    /// serial or silently bootstrapping one (#661).
    pub async fn get_certificate_serial(
        &self,
        node_id: &NodeId,
    ) -> Result<Option<String>, StoreError> {
        let serial = sqlx::query_scalar::<_, Option<String>>(
            "SELECT certificate_serial FROM nodes WHERE node_id = ?",
        )
        .bind(node_id.as_str())
        .fetch_optional(&self.pool)
        .await?;

        match serial {
            Some(serial) => Ok(serial),
            None => Err(StoreError::NotFound {
                entity: "node",
                id: node_id.to_string(),
            }),
        }
    }

    /// Atomically rotate a node's certificate serial from `expected` to `new`.
    ///
    /// Returns `Ok(true)` when the conditional update matched (exactly one
    /// row changed), `Ok(false)` when the stored serial no longer equals
    /// `expected` (a concurrent rotation won the race) or the node row no
    /// longer exists. Callers MUST treat `Ok(false)` as "do not hand out the
    /// newly issued certificate" — see `EnrollmentServiceImplementation`.
    pub async fn rotate_certificate_serial(
        &self,
        node_id: &NodeId,
        expected: &str,
        new: &str,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(ROTATE_NODE_CERTIFICATE_SQL)
            .bind(node_id.as_str())
            .bind(expected)
            .bind(new)
            .execute(&self.pool)
            .await?;

        Ok(result.rows_affected() == 1)
    }

    pub async fn update_certificate_serial(
        &self,
        node_id: &NodeId,
        serial: &str,
    ) -> Result<(), StoreError> {
        let result = sqlx::query(UPDATE_NODE_CERTIFICATE_SQL)
            .bind(node_id.as_str())
            .bind(serial)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound {
                entity: "node",
                id: node_id.to_string(),
            });
        }

        Ok(())
    }

    pub async fn upsert_bootstrap_result(
        &self,
        input: &NodeBootstrapResultInput,
    ) -> Result<(), StoreError> {
        sqlx::query(UPSERT_BOOTSTRAP_RESULT_SQL)
            .bind(input.node_id.as_str())
            .bind(&input.operation_id)
            .bind(input.success)
            .bind(&input.error_message)
            .bind(&input.details)
            .bind(input.started_unix_ms)
            .bind(input.completed_unix_ms)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct NodeUpsertInput {
    pub node_id: NodeId,
    pub hostname: String,
    pub display_name: String,
    pub certificate_serial: Option<String>,
    pub agent_version: Option<String>,
    pub control_plane_version: Option<String>,
    pub enrolled_unix_ms: i64,
    pub last_seen_unix_ms: i64,
}

#[derive(Clone)]
pub struct NodeInventoryInput {
    pub node_id: NodeId,
    pub architecture: String,
    pub kernel_version: Option<String>,
    pub os_release: Option<String>,
    pub cpu_count: i32,
    pub memory_bytes: i64,
    pub disk_bytes: Option<i64>,
    pub cloud_hypervisor_version: Option<String>,
    pub chv_agent_version: Option<String>,
    pub chv_stord_version: Option<String>,
    pub chv_nwd_version: Option<String>,
    pub host_bundle_version: Option<String>,
    pub inventory_status: Option<String>,
    pub storage_classes: Option<serde_json::Value>,
    pub network_capabilities: Option<serde_json::Value>,
    pub labels: Option<serde_json::Value>,
    pub hypervisor_capabilities: Option<serde_json::Value>,
    /// #378: the node's authority mode, using the agent config's
    /// kebab-case spellings ([`AUTHORITY_MODE_LEGACY`] and friends).
    /// `None` = not reported (fail-open).
    pub authority_mode: Option<String>,
    pub reported_unix_ms: i64,
}

#[derive(Clone)]
pub struct NodeVersionInput {
    pub node_id: NodeId,
    pub component_name: String,
    pub version: String,
    pub source: Option<String>,
    pub reported_unix_ms: i64,
}

#[derive(Clone)]
pub struct NodeStatePatchInput {
    pub node_id: NodeId,
    pub desired_state: NodeState,
    pub desired_generation: Generation,
    pub requested_by: Option<String>,
    pub updated_by: Option<String>,
    pub state_reason: Option<String>,
    pub requested_unix_ms: i64,
}

#[derive(Clone)]
pub struct NodeSchedulingPatchInput {
    pub node_id: NodeId,
    pub desired_generation: Generation,
    pub scheduling_paused: bool,
    pub requested_by: Option<String>,
    pub updated_by: Option<String>,
    pub requested_unix_ms: i64,
}

#[derive(Clone)]
pub struct NodeDrainIntentInput {
    pub node_id: NodeId,
    pub desired_generation: Generation,
    pub allow_workload_stop: Option<bool>,
    pub requested_by: Option<String>,
    pub updated_by: Option<String>,
    pub requested_unix_ms: i64,
}

#[derive(Clone)]
pub struct NodeBootstrapResultInput {
    pub node_id: NodeId,
    pub operation_id: Option<String>,
    pub success: bool,
    pub error_message: Option<String>,
    pub details: Option<serde_json::Value>,
    pub started_unix_ms: Option<i64>,
    pub completed_unix_ms: i64,
}

fn generation_to_i64(generation: Generation) -> Result<i64, StoreError> {
    i64::try_from(generation.get()).map_err(|source| StoreError::InvalidConfiguration {
        reason: format!("generation out of range for bigint column: {source}"),
    })
}
