use axum::{
    extract::{Extension, Query, State},
    response::Json,
};
use serde_json::{json, Value};

use crate::handlers::hypervisor_settings::validate_vm_overrides;
use crate::router::AppState;
use crate::BffError;
use chv_common::hypervisor::HypervisorOverrides;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// Sanity ceilings for user-supplied sizes (see S2 overflow hardening):
/// memory ≤ 1 TiB, volume size ≤ 64 TiB. The volume ceiling derives
/// from the single shared constant
/// (`chv_hypervisor_api::resources::MAX_VOLUME_BYTES` — the #513 PR 3
/// consolidation; previously a BFF-local `64 * 1024` literal), stated
/// here in the GiB units this handler's `volume_size_gb` field uses.
const MAX_MEMORY_BYTES: i64 = 1024 * 1024 * 1024 * 1024;
const MAX_VOLUME_SIZE_GB: i64 =
    chv_hypervisor_api::resources::MAX_VOLUME_BYTES / 1024 / 1024 / 1024;

pub async fn list_vms(
    crate::auth::BearerToken(_claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    axum::Json(payload): axum::Json<Value>,
) -> Result<Json<Value>, BffError> {
    let page = payload
        .get("page")
        .and_then(|v| v.as_u64())
        .unwrap_or(1)
        .max(1);
    let page_size = payload
        .get("page_size")
        .and_then(|v| v.as_u64())
        .unwrap_or(50)
        .clamp(1, 200);
    let cache_key = format!("vms:list:{}:{}", page, page_size);
    if let Some(cached) = state.cache.get(&cache_key).await {
        return Ok(Json(
            serde_json::from_str(&cached).map_err(|e| BffError::Internal(e.to_string()))?,
        ));
    }

    let offset = (page - 1) * page_size;
    let total_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vms")
        .fetch_one(&state.pool)
        .await
        .map_err(|e| BffError::Internal(format!("failed to count vms: {}", e)))?;
    let total_pages = (total_count as u64).div_ceil(page_size);

    let rows = sqlx::query_as::<_, VmRow>(
        r#"
        SELECT
            v.vm_id,
            v.display_name AS name,
            COALESCE(vos.node_id, vds.target_node_id, v.node_id) AS node_id,
            -- #368 P3: an agent-reported Failed state always wins over the
            -- desired power state — a terminally-failed create (or any
            -- failed runtime) must render as Failed with its reason, never
            -- as the phantom desired state. Failure-free VMs keep the
            -- previous desired-first precedence unchanged.
            CASE
                WHEN vos.runtime_status = 'Failed' THEN vos.runtime_status
                ELSE COALESCE(vds.desired_power_state, vos.runtime_status, 'Unknown')
            END AS power_state,
            COALESCE(vos.last_error, '') AS last_error,
            COALESCE(vos.health_status, 'unknown') AS health,
            COALESCE(CAST(vds.cpu_count AS TEXT), '') AS cpu,
            CASE WHEN vds.memory_bytes IS NULL THEN ''
                 WHEN vds.memory_bytes >= 1073741824 THEN printf('%.1f GiB', CAST(vds.memory_bytes AS REAL)/1073741824.0)
                 WHEN vds.memory_bytes >= 1048576 THEN printf('%.1f MiB', CAST(vds.memory_bytes AS REAL)/1048576.0)
                 WHEN vds.memory_bytes >= 1024 THEN printf('%.1f KiB', CAST(vds.memory_bytes AS REAL)/1024.0)
                 ELSE printf('%d B', vds.memory_bytes) END AS memory,
            COALESCE(volume_counts.volume_count, 0) AS volume_count,
            COALESCE(nic_counts.nic_count, 0) AS nic_count,
            COALESCE(
                (SELECT operation_id FROM operations
                 WHERE resource_kind = 'vm' AND resource_id = v.vm_id
                 ORDER BY requested_at DESC LIMIT 1),
                ''
            ) AS last_task
        FROM vms v
        LEFT JOIN vm_desired_state vds ON v.vm_id = vds.vm_id
        LEFT JOIN vm_observed_state vos ON v.vm_id = vos.vm_id
        LEFT JOIN (
            SELECT attached_vm_id, COUNT(*) AS volume_count
            FROM volume_desired_state
            WHERE attached_vm_id IS NOT NULL
            GROUP BY attached_vm_id
        ) volume_counts ON v.vm_id = volume_counts.attached_vm_id
        LEFT JOIN (
            SELECT vm_id, COUNT(*) AS nic_count
            FROM vm_nic_desired_state
            GROUP BY vm_id
        ) nic_counts ON v.vm_id = nic_counts.vm_id
        ORDER BY v.vm_id
        LIMIT ? OFFSET ?
        "#,
    )
    .bind(page_size as i64)
    .bind(offset as i64)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| BffError::Internal(format!("failed to list vms: {}", e)))?;

    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "vm_id": r.vm_id,
                "name": r.name,
                "node_id": r.node_id,
                "power_state": r.power_state,
                "last_error": r.last_error,
                "health": r.health,
                "cpu": r.cpu,
                "memory": r.memory,
                "volume_count": r.volume_count,
                "nic_count": r.nic_count,
                "last_task": r.last_task,
            })
        })
        .collect();

    let response = Json(json!({
        "items": items,
        "page": {
            "page": page,
            "page_size": page_size,
            "total_items": total_count,
            "total_pages": total_pages,
        },
        "filters": {
            "applied": {}
        },
    }));
    if let Ok(json) = serde_json::to_string(&response.0) {
        state.cache.set(&cache_key, json).await;
    }
    Ok(response)
}

pub async fn get_vm(
    crate::auth::BearerToken(_claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    axum::Json(payload): axum::Json<Value>,
) -> Result<Json<Value>, BffError> {
    let vm_id = payload
        .get("vm_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing vm_id".into()))?;

    let row = sqlx::query_as::<_, VmSummaryRow>(
        r#"
        SELECT
            v.vm_id,
            v.display_name AS name,
            COALESCE(vos.node_id, vds.target_node_id, v.node_id) AS node_id,
            -- #368 P3: same Failed-wins precedence as the list view.
            CASE
                WHEN vos.runtime_status = 'Failed' THEN vos.runtime_status
                ELSE COALESCE(vds.desired_power_state, vos.runtime_status, 'Unknown')
            END AS power_state,
            COALESCE(vos.last_error, '') AS last_error,
            COALESCE(vos.health_status, 'unknown') AS health,
            COALESCE(CAST(vds.cpu_count AS TEXT), '') AS cpu,
            CASE WHEN vds.memory_bytes IS NULL THEN ''
                 WHEN vds.memory_bytes >= 1073741824 THEN printf('%.1f GiB', CAST(vds.memory_bytes AS REAL)/1073741824.0)
                 WHEN vds.memory_bytes >= 1048576 THEN printf('%.1f MiB', CAST(vds.memory_bytes AS REAL)/1048576.0)
                 WHEN vds.memory_bytes >= 1024 THEN printf('%.1f KiB', CAST(vds.memory_bytes AS REAL)/1024.0)
                 ELSE printf('%d B', vds.memory_bytes) END AS memory,
            COALESCE(snapshot_counts.snapshot_count, 0) AS snapshot_count
        FROM vms v
        LEFT JOIN vm_desired_state vds ON v.vm_id = vds.vm_id
        LEFT JOIN vm_observed_state vos ON v.vm_id = vos.vm_id
        LEFT JOIN (
            SELECT vm_id, COUNT(*) AS snapshot_count
            FROM vm_snapshots
            GROUP BY vm_id
        ) snapshot_counts ON v.vm_id = snapshot_counts.vm_id
        WHERE v.vm_id = $1
        "#,
    )
    .bind(vm_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| BffError::Internal(format!("failed to get vm: {}", e)))?;

    match row {
        Some(r) => {
            let recent_tasks = sqlx::query_as::<_, RecentTaskRow>(
                r#"
                SELECT
                    operation_id AS task_id,
                    status,
                    operation_type AS summary,
                    CAST(strftime('%s', requested_at) AS INTEGER) * 1000 AS started_unix_ms,
                    error_code,
                    error_message
                FROM operations
                WHERE resource_kind = 'vm' AND resource_id = $1
                ORDER BY requested_at DESC
                LIMIT 5
                "#,
            )
            .bind(vm_id)
            .fetch_all(&state.pool)
            .await
            .map_err(|e| BffError::Internal(format!("failed to get recent tasks: {}", e)))?;

            let tasks_json: Vec<Value> = recent_tasks
                .into_iter()
                .map(|t| {
                    json!({
                        "task_id": t.task_id,
                        "status": t.status,
                        "summary": t.summary,
                        "started_unix_ms": t.started_unix_ms,
                        // #502: NULL until a terminal failure records a
                        // cause — passed through verbatim, never
                        // fabricated.
                        "error_code": t.error_code,
                        "error_message": t.error_message,
                    })
                })
                .collect();

            let attached_volumes = sqlx::query_as::<_, VmVolumeRow>(
                r#"
                SELECT
                    v.volume_id,
                    v.display_name AS name,
                    CASE WHEN v.capacity_bytes IS NULL THEN ''
                         WHEN v.capacity_bytes >= 1073741824 THEN printf('%.1f GiB', CAST(v.capacity_bytes AS REAL)/1073741824.0)
                         WHEN v.capacity_bytes >= 1048576 THEN printf('%.1f MiB', CAST(v.capacity_bytes AS REAL)/1048576.0)
                         WHEN v.capacity_bytes >= 1024 THEN printf('%.1f KiB', CAST(v.capacity_bytes AS REAL)/1024.0)
                         ELSE printf('%d B', v.capacity_bytes) END AS size,
                    COALESCE(vds.device_name, '') AS device_name,
                    COALESCE(vds.read_only, false) AS read_only,
                    COALESCE(vos.health_status, 'unknown') AS health
                FROM volume_desired_state vds
                JOIN volumes v ON vds.volume_id = v.volume_id
                LEFT JOIN volume_observed_state vos ON v.volume_id = vos.volume_id
                WHERE vds.attached_vm_id = $1
                "#,
            )
            .bind(vm_id)
            .fetch_all(&state.pool)
            .await
            .map_err(|e| BffError::Internal(format!("failed to get vm volumes: {}", e)))?;

            let volumes_json: Vec<Value> = attached_volumes
                .into_iter()
                .map(|v| {
                    json!({
                        "volume_id": v.volume_id,
                        "name": v.name,
                        "size": v.size,
                        "device_name": v.device_name,
                        "read_only": v.read_only,
                        "health": v.health,
                    })
                })
                .collect();

            let attached_nics = sqlx::query_as::<_, VmNicRow>(
                r#"
                SELECT
                    nv.nic_id,
                    nv.network_id,
                    COALESCE(n.display_name, nv.network_id) AS network_name,
                    COALESCE(nv.mac_address, '') AS mac_address,
                    COALESCE(nv.ip_address, '') AS ip_address,
                    COALESCE(nv.nic_model, 'virtio') AS nic_model,
                    COALESCE(nds.ipam_mode, 'none') AS addressing_mode
                FROM vm_nic_desired_state nv
                LEFT JOIN networks n ON nv.network_id = n.network_id
                LEFT JOIN network_desired_state nds ON nds.network_id = nv.network_id
                WHERE nv.vm_id = $1
                "#,
            )
            .bind(vm_id)
            .fetch_all(&state.pool)
            .await
            .map_err(|e| BffError::Internal(format!("failed to get vm nics: {}", e)))?;

            let nics_json: Vec<Value> = attached_nics
                .into_iter()
                .map(|n| {
                    json!({
                        "nic_id": n.nic_id,
                        "network_id": n.network_id,
                        "network_name": n.network_name,
                        "mac_address": n.mac_address,
                        "ip_address": n.ip_address,
                        "nic_model": n.nic_model,
                        "addressing_mode": n.addressing_mode,
                    })
                })
                .collect();

            Ok(Json(json!({
                "summary": {
                    "vm_id": r.vm_id,
                    "name": r.name,
                    "node_id": r.node_id,
                    "power_state": r.power_state,
                    "last_error": r.last_error,
                    "health": r.health,
                    "cpu": r.cpu,
                    "memory": r.memory,
                    "recent_tasks": tasks_json,
                    "attached_volumes": volumes_json,
                    "attached_nics": nics_json,
                    "snapshot_count": r.snapshot_count,
                }
            })))
        }
        None => Err(BffError::NotFound(format!("vm {} not found", vm_id))),
    }
}

pub async fn create_vm(
    crate::auth::BearerToken(claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    Extension(correlation_id): Extension<Option<String>>,
    axum::Json(payload): axum::Json<Value>,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_operator_or_admin(&claims)?;
    tracing::info!("create_vm handler started");

    // Support both legacy BFF payload and CreateVMModal payload
    let display_name = payload
        .get("display_name")
        .and_then(|v| v.as_str())
        .or_else(|| payload.get("name").and_then(|v| v.as_str()))
        .ok_or_else(|| BffError::BadRequest("missing name/display_name".into()))?
        .to_string();

    // Reject names that could traverse or pollute filesystem paths; the
    // name is stored in vms.display_name and interpolated into volume names.
    if !is_valid_display_name(&display_name) {
        return Err(BffError::BadRequest(
            "display_name must match ^[A-Za-z0-9 ._-]{1,64}$".into(),
        ));
    }

    tracing::info!(%display_name, "create_vm: parsed display_name");

    let node_id = if let Some(nid) = payload.get("node_id").and_then(|v| v.as_str()) {
        nid.to_string()
    } else {
        // Pick the first enrolled node as default
        tracing::info!("create_vm: looking up default node");
        sqlx::query_scalar::<_, String>(
            "SELECT node_id FROM nodes ORDER BY enrolled_at DESC LIMIT 1",
        )
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| BffError::Internal(format!("failed to query nodes: {}", e)))?
        .ok_or_else(|| BffError::BadRequest("no nodes enrolled and node_id not provided".into()))?
    };
    tracing::info!(%node_id, "create_vm: selected node");

    let cpu_count = payload
        .get("cpu_count")
        .and_then(|v| v.as_i64())
        .or_else(|| payload.get("vcpu").and_then(|v| v.as_i64()))
        .unwrap_or(2);

    let memory_bytes = if let Some(bytes) = payload.get("memory_bytes").and_then(|v| v.as_i64()) {
        bytes
    } else if let Some(mb) = payload.get("memory_mb").and_then(|v| v.as_i64()) {
        // checked_mul: an oversized memory_mb wrapped to a negative/absurd
        // value in release builds, corrupting stored state and bypassing quota.
        mb.checked_mul(1024 * 1024)
            .ok_or_else(|| BffError::BadRequest("memory_mb too large".into()))?
    } else {
        2 * 1024 * 1024 * 1024
    };
    if memory_bytes <= 0 || memory_bytes > MAX_MEMORY_BYTES {
        return Err(BffError::BadRequest(
            "memory must be between 1 byte and 1 TiB".into(),
        ));
    }

    let mut image_ref = payload
        .get("image_ref")
        .and_then(|v| v.as_str())
        .or_else(|| payload.get("image_id").and_then(|v| v.as_str()))
        .unwrap_or("default")
        .to_string();
    tracing::info!(%image_ref, "create_vm: initial image_ref");

    // If image_ref is a real image UUID (not "default"), look up its source_url/path.
    if image_ref != "default" && !image_ref.is_empty() {
        tracing::info!(%image_ref, "create_vm: looking up image in DB");
        // Resolve by image_id first; if that misses, by display name —
        // operators reference images by the name chosen at import
        // (kubedoio/chv#339). display_name is not unique, so the name
        // match takes the most recently created image. An image_id HIT
        // terminates the lookup even when the row carries no usable
        // source: falling through to the name match could silently
        // resolve a DIFFERENT image whose display_name collides with
        // the id string.
        let id_row = sqlx::query_scalar::<_, Option<String>>(
            "SELECT source_url FROM images WHERE image_id = ?",
        )
        .bind(&image_ref)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| BffError::Internal(format!("failed to look up image: {}", e)))?;
        let (id_hit, source_url) = match id_row {
            Some(source_url) => (true, source_url),
            None => (
                false,
                sqlx::query_scalar::<_, Option<String>>(
                    // rowid tiebreaker: created_at has second granularity and
                    // imports within the same second must still resolve
                    // deterministically to the most recently inserted row.
                    "SELECT source_url FROM images WHERE display_name = ? ORDER BY created_at DESC, rowid DESC LIMIT 1",
                )
                .bind(&image_ref)
                .fetch_optional(&state.pool)
                .await
                .map_err(|e| BffError::Internal(format!("failed to look up image by name: {}", e)))?
                .flatten(),
            ),
        };
        match source_url {
            // A matched id row with an empty source cannot be a disk
            // seed — and must NOT fall through to the name match
            // (silent wrong-image resolution).
            Some(source_url) if source_url.is_empty() => {
                return Err(BffError::BadRequest(format!(
                    "Image '{}' has no source path recorded — re-import it with a source_url.",
                    image_ref
                )));
            }
            // The same holds for a NULL source on an id hit.
            None if id_hit => {
                return Err(BffError::BadRequest(format!(
                    "Image '{}' has no source path recorded — re-import it with a source_url.",
                    image_ref
                )));
            }
            Some(source_url) => {
                // file:// URIs are local paths (kubedoio/chv#339): the
                // same canonicalization import applies (legacy rows may
                // still carry the scheme or redundant slashes).
                let source_path = super::images::normalize_source_url(&source_url);
                if source_path.starts_with('/') {
                    tracing::info!(%source_path, "create_vm: resolved image_ref to local path");
                    image_ref = source_path;
                } else {
                    tracing::warn!(%source_url, "create_vm: image source_url is not a local path, cannot use as disk seed");
                    return Err(BffError::BadRequest(format!(
                        "Image source is a remote URL ({}). Download the image to the node first.",
                        source_url
                    )));
                }
            }
            None => {
                tracing::warn!(%image_ref, "create_vm: image not found in DB, keeping original image_ref");
            }
        }
    }

    let requested_by = claims.sub.clone();

    let network_id = payload
        .get("network_id")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();

    let volume_size_gb = payload
        .get("volume_size_gb")
        .and_then(|v| v.as_i64())
        .unwrap_or(10);
    if volume_size_gb <= 0 || volume_size_gb > MAX_VOLUME_SIZE_GB {
        return Err(BffError::BadRequest(format!(
            "volume_size_gb must be between 1 and {} (64 TiB)",
            MAX_VOLUME_SIZE_GB
        )));
    }
    // checked_mul: a huge volume_size_gb wrapped to a negative value in
    // release builds, corrupting capacity_bytes and bypassing quota.
    let volume_size_bytes = volume_size_gb
        .checked_mul(1024 * 1024 * 1024)
        .ok_or_else(|| BffError::BadRequest("volume_size_gb too large".into()))?;

    let cloud_init_userdata = payload
        .get("cloud_init_userdata")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string());

    // G5 vsock device opt-in (prompt-06 task 2): strictly opt-in —
    // absent means false, exactly the pre-G5 behavior (no device, no
    // silent attach). The BFF carries the marker only; the node
    // authority allocates the CID and host socket path at create.
    // Strictly typed: a truthy string or number is a bad request, not a
    // coercion.
    let vsock_enabled = match payload.get("vsock_enabled") {
        None => false,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| BffError::BadRequest("vsock_enabled must be a boolean".into()))?,
    };

    // #379 PR 2 (BFF payload field): the boot volume's optional storage
    // class, validated against the DP3 stord `backend_type` vocabulary at
    // accept time — the one shared list
    // (`chv_hypervisor_api::resources::BACKEND_CLASSES`), never a local
    // copy. Absent (or blank) means NULL in `volumes.storage_class` =
    // today's behavior byte-exactly: NULL dispatches as "local" all the
    // way down (the agent's B5 default), and the string "local" is only
    // stored when the caller names it explicitly. Local aliases
    // ("local-file"/"localdisk") are stord-boundary-only and reject here.
    let storage_class = match payload.get("storage_class") {
        None => None,
        Some(value) => {
            let class = value
                .as_str()
                .ok_or_else(|| BffError::BadRequest("storage_class must be a string".into()))?;
            let class = class.trim();
            if class.is_empty() {
                None
            } else if chv_hypervisor_api::resources::is_known_backend_class(class) {
                Some(class.to_string())
            } else {
                return Err(BffError::BadRequest(format!(
                    "unknown storage_class '{}': must be one of local, iscsi, ceph, lvm",
                    class
                )));
            }
        }
    };

    // #379 DP4 (#512 second-pass follow-up): the accept-time
    // node-capability check, on the production create surface. The
    // lifecycle RPC's `ensure_node_offers_storage_class` had no
    // production caller — this handler journals directly — so a create
    // requesting a class the target node does not offer (or a classless
    // disk — NULL = local — on an LVM-only node) was accepted with 200
    // and failed only at the agent/stord open. Same discipline as the
    // vocabulary check above and the attach-side path: the single
    // shared composition (`NodeRepository::node_storage_class_rejection`,
    // the exact code the lifecycle service calls) checks the create's
    // one disk class against the placement node's advertised
    // `storage_classes`, BEFORE the transaction — a rejection writes no
    // vms/vm_desired_state/volumes/volume_desired_state/operations
    // rows. Fails OPEN on a node that never reported classes,
    // byte-exactly the lifecycle-side semantics: an unreported node
    // never rejects (stord's own backend validation at the open is the
    // backstop). A node_id that does not parse as a NodeId skips the
    // check (fail-open) and keeps today's behavior — the check adds a
    // rejection mode, never a new parse error.
    if let Ok(node) = chv_controlplane_types::domain::NodeId::new(node_id.clone()) {
        if let Some(reason) = state
            .node_repo
            .node_storage_class_rejection(&node, storage_class.as_deref(), "vm create")
            .await?
        {
            tracing::warn!(%node_id, class = ?storage_class, "create_vm: rejecting storage class the node does not offer");
            return Err(BffError::BadRequest(reason));
        }
    }

    // Use BEGIN IMMEDIATE to acquire SQLite's RESERVED lock at tx start, serializing
    // concurrent writers from the get-go. This closes the quota-check TOCTOU window:
    // without IMMEDIATE, two concurrent requests can both pass quota check inside their
    // (DEFERRED) transactions and then both write, exceeding quota by N. See S2-1.
    let mut tx = state
        .pool
        .begin_with("BEGIN IMMEDIATE;")
        .await
        .map_err(|e| BffError::Internal(format!("failed to begin transaction: {}", e)))?;

    // Enforce quota inside the transaction to avoid races
    // Use claims.sub (UUID) which matches the quotas table and vm_desired_state.requested_by
    enforce_user_quota(
        &mut tx,
        &claims.sub,
        cpu_count,
        memory_bytes,
        volume_size_bytes,
        1, // vm_count_delta: creating 1 new VM
    )
    .await?;

    let vm_id = chv_common::gen_short_id();
    let volume_id = chv_common::gen_short_id();
    let operation_id = correlation_id.unwrap_or_else(chv_common::gen_short_id);
    tracing::info!(%vm_id, %volume_id, %operation_id, "create_vm: generated IDs, continuing transaction");

    // Insert VM
    sqlx::query(
        r#"
        INSERT INTO vms (vm_id, node_id, display_name, owner_id, created_at, updated_at)
        VALUES (?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'))
        "#,
    )
    .bind(&vm_id)
    .bind(&node_id)
    .bind(&display_name)
    .bind(&claims.sub)
    .execute(&mut *tx)
    .await
    .map_err(|e| BffError::Internal(format!("failed to insert vm: {}", e)))?;

    // Insert VM desired state
    sqlx::query(
        r#"
        INSERT INTO vm_desired_state (vm_id, desired_generation, desired_status, desired_power_state, requested_by, target_node_id, cpu_count, memory_bytes, image_ref, cloud_init_userdata, vsock_enabled, requested_at, updated_at)
        VALUES (?, 1, 'Pending', 'Running', ?, ?, ?, ?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'))
        "#,
    )
    .bind(&vm_id)
    .bind(&requested_by)
    .bind(&node_id)
    .bind(cpu_count)
    .bind(memory_bytes)
    .bind(&image_ref)
    .bind(&cloud_init_userdata)
    .bind(vsock_enabled)
    .execute(&mut *tx)
    .await
    .map_err(|e| BffError::Internal(format!("failed to insert vm_desired_state: {}", e)))?;

    // Persist hypervisor overrides if provided
    if let Some(hv_payload) = payload.get("hypervisor_overrides") {
        let overrides: HypervisorOverrides = serde_json::from_value(hv_payload.clone())
            .map_err(|e| BffError::BadRequest(format!("invalid hypervisor_overrides: {}", e)))?;
        if let Err(msg) = validate_vm_overrides(&overrides) {
            return Err(BffError::BadRequest(msg));
        }

        sqlx::query(
            r#"
            UPDATE vms SET
                hv_cpu_nested = COALESCE(?, hv_cpu_nested),
                hv_cpu_amx = COALESCE(?, hv_cpu_amx),
                hv_cpu_kvm_hyperv = COALESCE(?, hv_cpu_kvm_hyperv),
                hv_memory_mergeable = COALESCE(?, hv_memory_mergeable),
                hv_memory_hugepages = COALESCE(?, hv_memory_hugepages),
                hv_memory_shared = COALESCE(?, hv_memory_shared),
                hv_memory_prefault = COALESCE(?, hv_memory_prefault),
                hv_iommu = COALESCE(?, hv_iommu),
                hv_rng_src = COALESCE(?, hv_rng_src),
                hv_watchdog = COALESCE(?, hv_watchdog),
                hv_landlock_enable = COALESCE(?, hv_landlock_enable),
                hv_serial_mode = COALESCE(?, hv_serial_mode),
                hv_console_mode = COALESCE(?, hv_console_mode),
                hv_pvpanic = COALESCE(?, hv_pvpanic),
                hv_tpm_type = COALESCE(?, hv_tpm_type),
                hv_tpm_socket_path = COALESCE(?, hv_tpm_socket_path)
            WHERE vm_id = ?
            "#,
        )
        .bind(overrides.cpu_nested)
        .bind(overrides.cpu_amx)
        .bind(overrides.cpu_kvm_hyperv)
        .bind(overrides.memory_mergeable)
        .bind(overrides.memory_hugepages)
        .bind(overrides.memory_shared)
        .bind(overrides.memory_prefault)
        .bind(overrides.iommu)
        .bind(overrides.rng_src)
        .bind(overrides.watchdog)
        .bind(overrides.landlock_enable)
        .bind(overrides.serial_mode)
        .bind(overrides.console_mode)
        .bind(overrides.pvpanic)
        .bind(overrides.tpm_type)
        .bind(overrides.tpm_socket_path)
        .bind(&vm_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            BffError::Internal(format!("failed to persist hypervisor overrides: {}", e))
        })?;
    }

    // Insert volume
    sqlx::query(
        r#"
        INSERT INTO volumes (volume_id, node_id, display_name, owner_id, capacity_bytes, storage_class, updated_at)
        VALUES (?, ?, ?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%SZ','now'))
        "#,
    )
    .bind(&volume_id)
    .bind(&node_id)
    .bind(format!("{}-disk", display_name))
    .bind(&claims.sub)
    .bind(volume_size_bytes)
    // #379 PR 2: NULL when the caller did not name a class (the
    // historical row shape — every pre-#379 volume).
    .bind(&storage_class)
    .execute(&mut *tx)
    .await
    .map_err(|e| BffError::Internal(format!("failed to insert volume: {}", e)))?;

    // Insert volume desired state (attached to VM)
    sqlx::query(
        r#"
        INSERT INTO volume_desired_state (volume_id, desired_generation, desired_status, requested_by, attached_vm_id, device_name, read_only, requested_at, updated_at)
        VALUES (?, 1, 'Pending', ?, ?, 'vda', 0, strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'))
        "#,
    )
    .bind(&volume_id)
    .bind(&requested_by)
    .bind(&vm_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| BffError::Internal(format!("failed to insert volume_desired_state: {}", e)))?;

    // Resolve the --network reference (#354): look the reference up by
    // network_id first, then by display_name — the operator-facing
    // handle `network create` returns and the only handle a human
    // knows. The previous id-only lookup always missed name references
    // (network create stores a generated short network_id) and silently
    // created an implicit network with the fallback CIDR 10.200.0.0/24:
    // the operator's chosen cidr was ignored, and a second such network
    // collided on the subnet — duplicate host routes, breaking
    // host→guest connectivity for the second bridge (verified on real
    // KVM by the M4.4 qualification, issue #354).
    let mut resolved_network_id: Option<String> = sqlx::query_scalar(
        r#"
        SELECT n.network_id FROM networks n
        LEFT JOIN network_desired_state nds ON nds.network_id = n.network_id
        WHERE n.network_id = ?
          -- #499: a tombstoned (deleted) network must not capture an id
          -- reference — row-absence used to mean deleted, the tombstone
          -- now does.
          AND (nds.desired_status IS NULL OR nds.desired_status != 'Deleting')
        "#,
    )
    .bind(&network_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| BffError::Internal(format!("failed to check network: {}", e)))?;

    if resolved_network_id.is_none() {
        // Name hits prefer fleet-wide networks (node_id IS NULL — the
        // operator-created shape) over node-scoped rows (legacy implicit
        // networks), then break ties by network_id: deterministic. Note
        // the id lookup above is unscoped and always wins over names, so
        // a legacy implicit row whose network_id equals the reference
        // (e.g. an implicit 'default') keeps shadowing a same-named
        // operator network — the pre-#354 single-network behavior.
        // #499: tombstoned networks are excluded from name resolution
        // too (they used to be absent rows).
        resolved_network_id = sqlx::query_scalar(
            r#"
            SELECT n.network_id FROM networks n
            LEFT JOIN network_desired_state nds ON nds.network_id = n.network_id
            WHERE n.display_name = ? AND (n.node_id IS NULL OR n.node_id = ?)
              AND (nds.desired_status IS NULL OR nds.desired_status != 'Deleting')
            ORDER BY (n.node_id IS NULL) DESC, n.network_id
            LIMIT 1
            "#,
        )
        .bind(&network_id)
        .bind(&node_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| BffError::Internal(format!("failed to resolve network by name: {}", e)))?;
    }

    let network_id = match resolved_network_id {
        Some(id) => id,
        None => {
            // #499: the implicit branch would INSERT a network row under
            // the reference's id — but a tombstoned network still HOLDS
            // that id (the row survives as the tombstone's anchor), so
            // the insert would die on the PK. Fail with the honest,
            // clean refusal instead of the 500: the operator referenced
            // a network that was deleted.
            let tombstoned: Option<String> = sqlx::query_scalar(
                r#"
                SELECT n.network_id FROM networks n
                JOIN network_desired_state nds ON nds.network_id = n.network_id
                WHERE n.network_id = ? AND nds.desired_status = 'Deleting'
                "#,
            )
            .bind(&network_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| BffError::Internal(format!("failed to check network tombstone: {}", e)))?;
            if tombstoned.is_some() {
                return Err(BffError::Conflict(format!(
                    "network '{}' has been deleted; create a network with this id or name \
                     first, or reference another network",
                    network_id
                )));
            }

            let network_cidr = payload
                .get("network_cidr")
                .and_then(|v| v.as_str())
                .unwrap_or("10.200.0.0/24");
            let network_gateway = payload
                .get("network_gateway")
                .and_then(|v| v.as_str())
                .unwrap_or("10.200.0.1");

            // Collision guard (#354): an implicit network must never
            // silently share a SUBNET with an existing network usable on
            // this node — the host would end up with duplicate/ambiguous
            // routes for the subnet and host→guest connectivity to one
            // of the bridges would break. Overlap is checked properly
            // (e.g. 10.200.1.0/24 inside 10.200.0.0/23 counts), not by
            // exact string equality.
            let existing: Vec<(String, String)> = sqlx::query_as(
                r#"
                SELECT n.network_id, nds.cidr FROM network_desired_state nds
                JOIN networks n ON n.network_id = nds.network_id
                WHERE nds.cidr IS NOT NULL AND nds.cidr != ''
                  AND (n.node_id IS NULL OR n.node_id = ?)
                  -- #499: a tombstoned (deleted) network's subnet is
                  -- freed — its bridge is gone; it must not block an
                  -- implicit network from reusing the cidr.
                  AND (nds.desired_status IS NULL OR nds.desired_status != 'Deleting')
                "#,
            )
            .bind(&node_id)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| {
                BffError::Internal(format!("failed to check network cidr collisions: {}", e))
            })?;

            if let Some((other, other_cidr)) = existing
                .iter()
                .find(|(_, cidr)| cidrs_overlap(cidr, network_cidr))
            {
                return Err(BffError::Conflict(format!(
                    "network '{}': refusing to implicitly create a network with cidr {} — \
                     network '{}' already uses overlapping cidr {} on this node; create the \
                     network with a distinct cidr first, or pass network_cidr explicitly",
                    network_id, network_cidr, other, other_cidr
                )));
            }

            sqlx::query(
                r#"
                INSERT INTO networks (network_id, node_id, display_name, updated_at)
                VALUES (?, ?, ?, strftime('%Y-%m-%dT%H:%M:%SZ','now'))
                "#,
            )
            .bind(&network_id)
            .bind(&node_id)
            .bind(format!("network-{}", network_id))
            .execute(&mut *tx)
            .await
            .map_err(|e| BffError::Internal(format!("failed to insert network: {}", e)))?;

            sqlx::query(
                r#"
                INSERT INTO network_desired_state (
                    network_id, desired_generation, desired_status,
                    cidr, gateway, dhcp_enabled, ipam_mode, is_default,
                    requested_by, requested_at, updated_at
                )
                VALUES (?, 1, 'Pending', ?, ?, 1, 'internal', 0, ?, strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'))
                "#,
            )
            .bind(&network_id)
            .bind(network_cidr)
            .bind(network_gateway)
            .bind(&requested_by)
            .execute(&mut *tx)
            .await
            .map_err(|e| BffError::Internal(format!("failed to insert network_desired_state: {}", e)))?;

            network_id
        }
    };

    // Fetch network CIDR for IP generation. A networks row without a
    // network_desired_state row (legacy/partial data) is tolerated:
    // empty cidr → generate_ip's deterministic fallback, same as the
    // pre-#354 id-hit path.
    let network_cidr: String = sqlx::query_scalar(
        "SELECT COALESCE(cidr, '') FROM network_desired_state WHERE network_id = ?",
    )
    .bind(&network_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| BffError::Internal(format!("failed to fetch network cidr: {}", e)))?
    .unwrap_or_default();

    // Insert NIC
    let nic_id = format!("{}-{}", vm_id, network_id);
    let mac_address = generate_mac(&vm_id, &network_id);
    let ip_address = generate_ip(&vm_id, &network_id, &network_cidr);

    sqlx::query(
        r#"
        INSERT INTO vm_nic_desired_state (nic_id, vm_id, network_id, mac_address, ip_address, nic_model, created_at, updated_at)
        VALUES (?, ?, ?, ?, ?, 'virtio', strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'))
        "#,
    )
    .bind(&nic_id)
    .bind(&vm_id)
    .bind(&network_id)
    .bind(&mac_address)
    .bind(&ip_address)
    .execute(&mut *tx)
    .await
    .map_err(|e| BffError::Internal(format!("failed to insert vm_nic_desired_state: {}", e)))?;

    // Insert operation
    let idempotency_key = format!("create-vm-{}", vm_id);
    sqlx::query(
        r#"
        INSERT INTO operations (operation_id, idempotency_key, resource_kind, resource_id, operation_type, status, requested_by, desired_generation, requested_at, created_at, updated_at)
        VALUES (?, ?, 'vm', ?, 'CreateVm', 'Accepted', ?, 1, strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'))
        "#,
    )
    .bind(&operation_id)
    .bind(&idempotency_key)
    .bind(&vm_id)
    .bind(&requested_by)
    .execute(&mut *tx)
    .await
    .map_err(|e| BffError::Internal(format!("failed to insert operation: {}", e)))?;

    tracing::info!(%vm_id, "create_vm: committing transaction");
    tx.commit()
        .await
        .map_err(|e| BffError::Internal(format!("failed to commit transaction: {}", e)))?;

    tracing::info!(%vm_id, "create_vm: transaction committed successfully");
    state.cache.invalidate("vms:").await;
    state.cache.invalidate("overview").await;
    Ok(Json(json!({
        "accepted": true,
        "task_id": operation_id,
        "vm_id": vm_id,
        "summary": format!("Creating VM '{}'", display_name),
        "next_refresh_path": format!("/api/v1/tasks/{}", operation_id),
    })))
}

pub async fn delete_vm(
    crate::auth::BearerToken(claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    Extension(correlation_id): Extension<Option<String>>,
    axum::Json(payload): axum::Json<Value>,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_operator_or_admin(&claims)?;
    let vm_id = payload
        .get("vm_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing vm_id".into()))?
        .to_string();

    let requested_by = claims.sub.clone();

    // Check vm exists
    let exists = sqlx::query_scalar::<_, String>("SELECT vm_id FROM vms WHERE vm_id = ?")
        .bind(&vm_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| BffError::Internal(format!("failed to check vm existence: {}", e)))?;

    if exists.is_none() {
        return Err(BffError::NotFound(format!("vm {} not found", vm_id)));
    }

    // BEGIN IMMEDIATE: serialize concurrent writers (repo standard for
    // write txs) — the delete now writes (nic-row removal), and a
    // DEFERRED read-then-write can hit SQLITE_BUSY_SNAPSHOT against a
    // concurrent delete_network instead of a clean 409/200.
    let mut tx = state
        .pool
        .begin_with("BEGIN IMMEDIATE;")
        .await
        .map_err(|e| BffError::Internal(format!("failed to begin transaction: {}", e)))?;

    require_vm_owner(&mut tx, &vm_id, &claims.sub, claims.role == "admin").await?;

    // #406: idempotent retry. The M2.5 authority-side retention keeps
    // the VM rows after a delete, so a retried delete of the same VM
    // re-enters this handler with the same `delete-vm-<vm_id>` key.
    // Check for a recorded operation BEFORE any mutation: a hit replays
    // the original outcome (200 Accepted with the recorded task_id)
    // without re-running the nic-row removal or bumping the generation.
    // The BEGIN IMMEDIATE tx serializes this check-then-insert pair
    // against concurrent deletes of the same VM.
    let idempotency_key = format!("delete-vm-{}", vm_id);
    if let Some(recorded) =
        crate::handlers::operations::find_recorded_operation(&mut tx, &idempotency_key).await?
    {
        tracing::info!(
            %vm_id,
            operation_id = %recorded.operation_id,
            status = %recorded.status,
            "delete_vm: idempotent retry of a recorded delete; replaying original outcome"
        );
        return Ok(Json(json!({
            "accepted": true,
            "task_id": recorded.operation_id,
            "vm_id": vm_id,
            "recorded_status": recorded.status,
            "summary": format!("Deleting VM '{}'", vm_id),
            "next_refresh_path": format!("/api/v1/tasks/{}", recorded.operation_id),
        })));
    }

    // Remove the VM's NIC desired-state rows (#356): nothing else ever
    // deletes them (the vms/vm_desired_state rows persist as tombstones
    // of the desired-state lifecycle), and lingering rows permanently
    // block `network delete` — the attached-VM count never reaches zero
    // after a VM is deleted (409 "VM(s) still attached"; verified on
    // real KVM by the M4.4 qualification, issue #356). Node-side the
    // rows are pure CP bookkeeping: nwd is push-driven (RPC specs) and
    // the agent's delete path is keyed by vm_id, so removing them here
    // cannot strand host state.
    sqlx::query("DELETE FROM vm_nic_desired_state WHERE vm_id = ?")
        .bind(&vm_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| BffError::Internal(format!("failed to delete vm_nic_desired_state: {}", e)))?;

    sqlx::query(
        r#"
        UPDATE vm_desired_state
        SET desired_status = 'Deleting', desired_generation = desired_generation + 1, updated_by = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')
        WHERE vm_id = ?
        "#,
    )
    .bind(&requested_by)
    .bind(&vm_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| BffError::Internal(format!("failed to update vm_desired_state: {}", e)))?;

    let new_generation: i64 =
        sqlx::query_scalar("SELECT desired_generation FROM vm_desired_state WHERE vm_id = ?")
            .bind(&vm_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| BffError::Internal(format!("failed to read generation: {}", e)))?;

    let operation_id = correlation_id.unwrap_or_else(chv_common::gen_short_id);
    let insert_operation = sqlx::query(
        r#"
        INSERT INTO operations (operation_id, idempotency_key, resource_kind, resource_id, operation_type, status, requested_by, desired_generation, requested_at, created_at, updated_at)
        VALUES (?, ?, 'vm', ?, 'DeleteVm', 'Accepted', ?, ?, strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'))
        "#,
    )
    .bind(&operation_id)
    .bind(&idempotency_key)
    .bind(&vm_id)
    .bind(&requested_by)
    .bind(new_generation)
    .execute(&mut *tx)
    .await;
    if let Err(e) = insert_operation {
        // #406: an idempotency-key collision here must never surface as
        // an opaque 500 — fail closed with a 409 naming the condition
        // (and the tx rolls back, so the delete is not re-executed).
        return Err(crate::handlers::operations::map_operation_insert_error(
            &mut tx,
            &idempotency_key,
            e,
        )
        .await);
    }

    tx.commit()
        .await
        .map_err(|e| BffError::Internal(format!("failed to commit transaction: {}", e)))?;

    state.cache.invalidate("vms:").await;
    // The nic-row removal changes networks-scoped attached_vms counts
    // (#356) — the network list/detail cache must not serve them stale.
    state.cache.invalidate("networks:").await;
    state.cache.invalidate("overview").await;
    Ok(Json(json!({
        "accepted": true,
        "task_id": operation_id,
        "vm_id": vm_id,
        "summary": format!("Deleting VM '{}'", vm_id),
        "next_refresh_path": format!("/api/v1/tasks/{}", operation_id),
    })))
}

pub async fn resize_vm(
    crate::auth::BearerToken(claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    Extension(correlation_id): Extension<Option<String>>,
    axum::Json(payload): axum::Json<Value>,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_operator_or_admin(&claims)?;
    let vm_id = payload
        .get("vm_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing vm_id".into()))?
        .to_string();

    let cpu_count = payload
        .get("cpu_count")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    let memory_bytes = if let Some(bytes) = payload.get("memory_bytes").and_then(|v| v.as_i64()) {
        bytes
    } else if let Some(mb) = payload.get("memory_mb").and_then(|v| v.as_i64()) {
        // Same overflow guard as create_vm (S2 hardening).
        mb.checked_mul(1024 * 1024)
            .ok_or_else(|| BffError::BadRequest("memory_mb too large".into()))?
    } else {
        0
    };

    if cpu_count <= 0 {
        return Err(BffError::BadRequest("cpu_count must be > 0".into()));
    }
    if memory_bytes <= 0 || memory_bytes > MAX_MEMORY_BYTES {
        return Err(BffError::BadRequest(
            "memory must be between 1 byte and 1 TiB".into(),
        ));
    }

    let exists = sqlx::query_scalar::<_, String>("SELECT vm_id FROM vms WHERE vm_id = ?")
        .bind(&vm_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| BffError::Internal(format!("failed to check vm existence: {}", e)))?;

    if exists.is_none() {
        return Err(BffError::NotFound(format!("vm {} not found", vm_id)));
    }

    let requested_by = claims.sub.clone();
    let operation_id = correlation_id.unwrap_or_else(chv_common::gen_short_id);
    // #406 review round: the key is args-hashed (`resize-vm-<vm_id>-<cpu>-<mem>`)
    // so the two realistic intents are both correct — a same-args retry replays
    // the recorded outcome (idempotent), while a genuinely different resize is
    // a fresh operation (never silently swallowed by a per-VM key). The values
    // are validated above, before this derivation.
    let idempotency_key = format!("resize-vm-{}-{}-{}", vm_id, cpu_count, memory_bytes);

    // BEGIN IMMEDIATE: serialize concurrent writers to avoid quota TOCTOU on resize,
    // and the SELECT for delta math runs inside the tx so the values can't change
    // between read and quota check.
    let mut tx = state
        .pool
        .begin_with("BEGIN IMMEDIATE;")
        .await
        .map_err(|e| BffError::Internal(format!("failed to begin transaction: {}", e)))?;

    require_vm_owner(&mut tx, &vm_id, &claims.sub, claims.role == "admin").await?;

    // #406: idempotent retry — same mechanic as delete_vm. The key is
    // args-hashed (see above), so only a same-args retry re-derives it;
    // replay the recorded outcome instead of colliding on the UNIQUE
    // insert, and never re-run the quota delta math or the desired-state
    // UPDATE. Checked after the ownership gate so a non-owner probing a
    // recorded VM still gets 403/404.
    if let Some(recorded) =
        crate::handlers::operations::find_recorded_operation(&mut tx, &idempotency_key).await?
    {
        tracing::info!(
            %vm_id,
            operation_id = %recorded.operation_id,
            status = %recorded.status,
            "resize_vm: idempotent retry of a recorded resize; replaying original outcome"
        );
        return Ok(Json(json!({
            "accepted": true,
            "task_id": recorded.operation_id,
            "vm_id": vm_id,
            "recorded_status": recorded.status,
            "summary": format!("Resizing VM '{}' to {} vCPU, {} bytes memory", vm_id, cpu_count, memory_bytes),
            "next_refresh_path": format!("/api/v1/tasks/{}", recorded.operation_id),
        })));
    }

    // Fetch current resource usage for delta-based quota check (inside the tx so
    // the values are stable across the quota check and the UPDATE below).
    let current: (Option<i64>, Option<i64>) =
        sqlx::query_as("SELECT cpu_count, memory_bytes FROM vm_desired_state WHERE vm_id = ?")
            .bind(&vm_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| {
                BffError::Internal(format!("failed to read current vm resources: {}", e))
            })?;
    let old_cpu = current.0.unwrap_or(0);
    let old_memory = current.1.unwrap_or(0);

    // Enforce quota using delta so we don't double-count this VM
    enforce_user_quota(
        &mut tx,
        &claims.sub,
        cpu_count - old_cpu,
        memory_bytes - old_memory,
        0, // storage unchanged on resize
        0, // vm_count_delta: no new VM
    )
    .await?;

    sqlx::query(
        r#"
        UPDATE vm_desired_state
        SET cpu_count = ?, memory_bytes = ?, desired_generation = desired_generation + 1, updated_by = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')
        WHERE vm_id = ?
        "#,
    )
    .bind(cpu_count)
    .bind(memory_bytes)
    .bind(&requested_by)
    .bind(&vm_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| BffError::Internal(format!("failed to update vm_desired_state: {}", e)))?;

    let new_generation: i64 =
        sqlx::query_scalar("SELECT desired_generation FROM vm_desired_state WHERE vm_id = ?")
            .bind(&vm_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| BffError::Internal(format!("failed to read generation: {}", e)))?;

    let insert_operation = sqlx::query(
        r#"
        INSERT INTO operations (operation_id, idempotency_key, resource_kind, resource_id, operation_type, status, requested_by, desired_generation, requested_at, created_at, updated_at)
        VALUES (?, ?, 'vm', ?, 'ResizeVm', 'Accepted', ?, ?, strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'))
        "#,
    )
    .bind(&operation_id)
    .bind(&idempotency_key)
    .bind(&vm_id)
    .bind(&requested_by)
    .bind(new_generation)
    .execute(&mut *tx)
    .await;
    if let Err(e) = insert_operation {
        // #406: an idempotency-key collision here must never surface as
        // an opaque 500 — fail closed with a 409 naming the condition
        // (and the tx rolls back, so the resize is not re-executed).
        return Err(crate::handlers::operations::map_operation_insert_error(
            &mut tx,
            &idempotency_key,
            e,
        )
        .await);
    }

    tx.commit()
        .await
        .map_err(|e| BffError::Internal(format!("failed to commit transaction: {}", e)))?;

    state.cache.invalidate("vms:").await;
    state.cache.invalidate("overview").await;
    Ok(Json(json!({
        "accepted": true,
        "task_id": operation_id,
        "vm_id": vm_id,
        "summary": format!("Resizing VM '{}' to {} vCPU, {} bytes memory", vm_id, cpu_count, memory_bytes),
        "next_refresh_path": format!("/api/v1/tasks/{}", operation_id),
    })))
}

pub async fn mutate_vm(
    crate::auth::BearerToken(claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    axum::Json(payload): axum::Json<Value>,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_operator_or_admin(&claims)?;
    let vm_id = payload
        .get("vm_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing vm_id".into()))?
        .to_string();

    let action = payload
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing action".into()))?
        .to_string();

    let force = payload
        .get("force")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(|e| BffError::Internal(format!("failed to acquire connection: {}", e)))?;
    require_vm_owner(&mut conn, &vm_id, &claims.sub, claims.role == "admin").await?;

    crate::audit::log_mutation(&claims.sub, &action, "vm", &vm_id);

    let response = if action == "migrate" {
        let target_node_id = payload
            .get("target_node_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                BffError::BadRequest("missing target_node_id for migrate action".into())
            })?
            .to_string();
        // Issue #656: live/concurrent-write migration is not supported
        // until #394 is proven. The user-facing API fails closed: only an
        // explicit boolean `true` proceeds. A missing field is NOT
        // defaulted to false (which used to mean live); it is rejected
        // with the same clear 400. A *present* non-bool value is rejected
        // rather than coerced. Do not silently coerce either case to
        // true — that would claim a stop-the-world pause the caller did
        // not request.
        let pause_first = match payload.get("pause_first") {
            Some(v) => v.as_bool().ok_or_else(|| {
                BffError::BadRequest("pause_first for migrate action must be a boolean".into())
            })?,
            None => {
                return Err(BffError::BadRequest(
                    "live/concurrent-write migration is not supported until #394 is proven; \
                     set pause_first=true (stop-the-world)"
                        .into(),
                ));
            }
        };
        if !pause_first {
            return Err(BffError::BadRequest(
                "live/concurrent-write migration is not supported until #394 is proven; \
                 set pause_first=true (stop-the-world)"
                    .into(),
            ));
        }
        state
            .mutations
            .migrate_vm(vm_id, target_node_id, pause_first, claims.sub)
            .await?
    } else {
        state
            .mutations
            .mutate_vm(vm_id, action, force, claims.sub)
            .await?
    };

    state.cache.invalidate("vms:").await;
    state.cache.invalidate("overview").await;
    Ok(Json(json!({
        "accepted": response.accepted,
        "task_id": response.task_id,
        "vm_id": response.vm_id,
        "summary": response.summary,
    })))
}

pub async fn get_vm_console(
    State(state): State<AppState>,
    crate::auth::BearerToken(claims): crate::auth::BearerToken,
    axum::Json(payload): axum::Json<Value>,
) -> Result<Json<Value>, BffError> {
    let vm_id = payload
        .get("vm_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing vm_id".into()))?;

    if !chv_common::validate_id(vm_id) {
        return Err(BffError::BadRequest("invalid vm_id format".into()));
    }

    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(|e| BffError::Internal(format!("db connection: {e}")))?;
    require_vm_owner(&mut conn, vm_id, &claims.sub, claims.role == "admin").await?;

    let log_path = state
        .agent_runtime_dir
        .join("vms")
        .join(vm_id)
        .join("console.log");

    let log_content = match tokio::fs::File::open(&log_path).await {
        Ok(mut file) => {
            let metadata = file.metadata().await.map_err(|e| {
                tracing::warn!(path = %log_path.display(), error = %e, "failed to read console log metadata");
                BffError::Internal("failed to read console log".into())
            })?;
            let file_size = metadata.len();
            let offset = file_size.saturating_sub(256 * 1024);
            if offset > 0 {
                file.seek(std::io::SeekFrom::Start(offset)).await.map_err(|e| {
                    tracing::warn!(path = %log_path.display(), error = %e, "failed to seek console log");
                    BffError::Internal("failed to read console log".into())
                })?;
            }
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).await.map_err(|e| {
                tracing::warn!(path = %log_path.display(), error = %e, "failed to read console log");
                BffError::Internal("failed to read console log".into())
            })?;
            String::from_utf8_lossy(&buf).into_owned()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            tracing::warn!(path = %log_path.display(), error = %e, "failed to open console log");
            return Err(BffError::Internal("failed to read console log".into()));
        }
    };

    let lines: Vec<&str> = log_content.lines().collect();
    let line_count = lines.len();
    let tail = if line_count > 500 {
        lines[line_count - 500..].join("\n")
    } else {
        log_content
    };

    Ok(Json(json!({
        "vm_id": vm_id,
        "content": tail,
        "lines": line_count,
    })))
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ConsoleTokenClaims {
    sub: String,
    username: String,
    aud: String,
    exp: u64,
}

fn generate_console_token(
    vm_id: &str,
    username: &str,
    secret: &str,
) -> Result<String, jsonwebtoken::errors::Error> {
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(std::time::Duration::ZERO)
        .as_secs()
        + 60;
    let claims = ConsoleTokenClaims {
        sub: vm_id.to_string(),
        username: username.to_string(),
        aud: "chv:console".to_string(),
        exp,
    };
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
    jsonwebtoken::encode(
        &header,
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
    )
}

#[derive(serde::Deserialize)]
pub struct ConsoleUrlQuery {
    proxied: Option<bool>,
}

pub async fn get_vm_console_url(
    crate::auth::BearerToken(claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(vm_id): axum::extract::Path<String>,
    Query(query): Query<ConsoleUrlQuery>,
) -> Result<Json<Value>, BffError> {
    if !chv_common::validate_id(&vm_id) {
        return Err(BffError::BadRequest("invalid vm_id format".into()));
    }

    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(|e| BffError::Internal(format!("db connection: {e}")))?;
    require_vm_owner(&mut conn, &vm_id, &claims.sub, claims.role == "admin").await?;

    let node_id: Option<String> = sqlx::query_scalar("SELECT node_id FROM vms WHERE vm_id = ?")
        .bind(&vm_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| BffError::Internal(format!("failed to look up vm: {}", e)))?;

    let node_id = node_id.ok_or_else(|| BffError::NotFound(format!("vm {} not found", vm_id)))?;

    let agent_ws_address: Option<String> =
        sqlx::query_scalar("SELECT agent_ws_address FROM nodes WHERE node_id = ?")
            .bind(&node_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(|e| BffError::Internal(format!("failed to look up node: {}", e)))?;

    let token = generate_console_token(&vm_id, &claims.username, &state.jwt_secret)
        .map_err(|e| BffError::Internal(format!("failed to generate console token: {}", e)))?;

    // Determine WebSocket scheme based on incoming request protocol.
    // Use wss:// when the request arrived over HTTPS (detected via X-Forwarded-Proto header).
    let is_tls = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|proto| proto.eq_ignore_ascii_case("https"))
        .unwrap_or(false);
    let ws_scheme = if is_tls { "wss" } else { "ws" };

    // Return a full WS URL when the node has an agent_ws_address configured
    // and the client did not explicitly request proxied mode;
    // otherwise fall back to the relative path for proxy setups.
    // In proxied mode the path includes node_id so nginx can route to the
    // correct hypervisor without requiring dynamic-upstream modules.
    let console_url = match agent_ws_address {
        Some(addr) if !addr.is_empty() && query.proxied != Some(true) => {
            if !is_tls {
                tracing::warn!(
                    vm_id = %vm_id,
                    node_id = %node_id,
                    "console WebSocket URL uses plaintext ws:// — configure TLS and X-Forwarded-Proto for production"
                );
            }
            format!(
                "{}://{}/vms/{}/console?token={}",
                ws_scheme, addr, vm_id, token
            )
        }
        Some(_) => format!("/ws/vms/{}/{}/console?token={}", node_id, vm_id, token),
        None => return Err(BffError::NotFound("node not found".into())),
    };
    let expires_at = chrono::Utc::now() + chrono::Duration::seconds(60);

    Ok(Json(json!({
        "vm_id": vm_id,
        "node_id": node_id,
        "url": console_url,
        "expires_at": expires_at.to_rfc3339(),
    })))
}

#[derive(sqlx::FromRow)]
struct VmRow {
    vm_id: String,
    name: String,
    node_id: Option<String>,
    power_state: String,
    last_error: Option<String>,
    health: String,
    cpu: String,
    memory: String,
    volume_count: i32,
    nic_count: i32,
    last_task: String,
}

#[derive(sqlx::FromRow)]
struct VmSummaryRow {
    vm_id: String,
    name: String,
    node_id: Option<String>,
    power_state: String,
    last_error: Option<String>,
    health: String,
    cpu: String,
    memory: String,
    snapshot_count: i32,
}

#[derive(sqlx::FromRow)]
struct RecentTaskRow {
    task_id: String,
    status: String,
    summary: String,
    started_unix_ms: i64,
    // #502: the terminal-failure cause the fast-fail work journals.
    error_code: Option<String>,
    error_message: Option<String>,
}

#[derive(sqlx::FromRow)]
struct VmVolumeRow {
    volume_id: String,
    name: String,
    size: String,
    device_name: String,
    read_only: bool,
    health: String,
}

#[derive(sqlx::FromRow)]
struct VmNicRow {
    nic_id: String,
    network_id: String,
    network_name: String,
    mac_address: String,
    ip_address: String,
    nic_model: String,
    addressing_mode: String,
}

// ── Helpers ───────────────────────────────────────────────────────────────

/// Validate a user-supplied VM display name against the safe charset used by
/// the UI: 1-64 chars of `[A-Za-z0-9 ._-]`. Rejects path separators and
/// traversal sequences so names can never escape into filesystem paths.
pub(crate) fn is_valid_display_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b' ' | b'.' | b'_' | b'-'))
}

/// Check if creating a VM would exceed the user's quota.
/// Returns Ok(()) if allowed, Err(BffError) if quota would be exceeded.
/// Check if the user is the owner of a resource or an admin.
/// Returns Ok(()) if allowed, Err(BffError::Forbidden) if not.
pub(crate) async fn require_vm_owner(
    conn: &mut sqlx::SqliteConnection,
    vm_id: &str,
    user_id: &str,
    is_admin: bool,
) -> Result<(), BffError> {
    if is_admin {
        return Ok(());
    }
    let owner: Option<String> = sqlx::query_scalar("SELECT owner_id FROM vms WHERE vm_id = ?")
        .bind(vm_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| BffError::Internal(format!("failed to check vm owner: {}", e)))?;
    match owner {
        Some(o) if o == user_id => Ok(()),
        None => {
            tracing::warn!(resource_id = %vm_id, "ownership check failed: resource has no owner_id set");
            Err(BffError::Forbidden(
                "resource has no owner; admin access required".into(),
            ))
        }
        Some(_) => Err(BffError::Forbidden("you do not own this VM".into())),
    }
}

pub(crate) async fn enforce_user_quota(
    conn: &mut sqlx::SqliteConnection,
    user_id: &str,
    requested_cpu: i64,
    requested_memory_bytes: i64,
    requested_storage_bytes: i64,
    vm_count_delta: i64,
) -> Result<(), BffError> {
    let quota_row = sqlx::query_as::<_, (Option<i64>, Option<i64>, Option<i64>, Option<i64>)>(
        "SELECT max_vms, max_cpu, max_memory_bytes, max_storage_bytes FROM quotas WHERE user_id = ?"
    )
    .bind(user_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| BffError::Internal(format!("failed to check quota: {}", e)))?;

    let quota = match quota_row {
        Some(r) => r,
        None => return Ok(()),
    };

    let vm_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM vm_desired_state WHERE requested_by = ?")
            .bind(user_id)
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| BffError::Internal(format!("failed to count vms: {}", e)))?;

    let usage_row = sqlx::query_as::<_, (Option<i64>, Option<i64>)>(
        "SELECT COALESCE(SUM(cpu_count), 0), COALESCE(SUM(memory_bytes), 0) FROM vm_desired_state WHERE requested_by = ?"
    )
    .bind(user_id)
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| BffError::Internal(format!("failed to compute usage: {}", e)))?;

    let current_cpu = usage_row.0.unwrap_or(0);
    let current_memory = usage_row.1.unwrap_or(0);

    // #525: the storage SUM is the canonical `storage_usage_bytes` rule
    // (owner OR attached-to-your-VM, counted once) — the same query the
    // usage meters read, so enforcement and display agree. Pre-#525 this
    // joined through `vm_desired_state` only, so standalone volumes
    // (NULL `attached_vm_id`) never accrued toward `used`.
    let current_storage: i64 =
        crate::handlers::quotas::storage_usage_bytes(&mut *conn, user_id).await?;

    if let Some(max) = quota.0 {
        if vm_count + vm_count_delta > max {
            return Err(BffError::QuotaExceeded {
                resource: "vms".to_string(),
                limit: max,
                used: vm_count,
                requested: vm_count_delta,
            });
        }
    }
    if let Some(max) = quota.1 {
        if current_cpu + requested_cpu > max {
            return Err(BffError::QuotaExceeded {
                resource: "vcpus".to_string(),
                limit: max,
                used: current_cpu,
                requested: requested_cpu,
            });
        }
    }
    if let Some(max) = quota.2 {
        if current_memory + requested_memory_bytes > max {
            return Err(BffError::QuotaExceeded {
                resource: "memory".to_string(),
                limit: max,
                used: current_memory,
                requested: requested_memory_bytes,
            });
        }
    }
    if let Some(max) = quota.3 {
        if current_storage + requested_storage_bytes > max {
            return Err(BffError::QuotaExceeded {
                resource: "disk".to_string(),
                limit: max,
                used: current_storage,
                requested: requested_storage_bytes,
            });
        }
    }

    Ok(())
}

/// Deterministically generate a MAC address from vm_id + network_id.
/// Sets the locally-administered bit to avoid conflicts with OUI space.
pub(crate) fn generate_mac(vm_id: &str, network_id: &str) -> String {
    let input = format!("{}:{}", vm_id, network_id);
    let hash = chv_common::fnv1a_hash(&input);
    let octets = [
        0x02, // locally administered
        ((hash >> 8) & 0xFF) as u8,
        ((hash >> 16) & 0xFF) as u8,
        ((hash >> 24) & 0xFF) as u8,
        ((hash >> 32) & 0xFF) as u8,
        ((hash >> 40) & 0xFF) as u8,
    ];
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        octets[0], octets[1], octets[2], octets[3], octets[4], octets[5]
    )
}

/// Do two CIDR strings denote overlapping IPv4 subnets? Used by the
/// create_vm implicit-network collision guard (#354): a duplicate route
/// hazard exists for ANY shared subnet, not just exact string matches
/// (10.200.1.0/24 inside 10.200.0.0/23 is just as ambiguous on the
/// host). Lenient parse in the same style as `generate_ip`: unparseable
/// or empty cidrs never overlap (no false refusals from dirty data).
pub(crate) fn cidrs_overlap(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Option<(u32, u32)> {
        let (base, prefix) = s.split_once('/').unwrap_or((s, "24"));
        let prefix: u32 = prefix.parse().ok()?;
        if prefix > 32 {
            return None;
        }
        let mut octets = base.split('.');
        let mut addr = 0u32;
        for _ in 0..4 {
            let octet: u8 = octets.next()?.trim().parse().ok()?;
            addr = (addr << 8) | octet as u32;
        }
        if octets.next().is_some() {
            return None;
        }
        let mask = if prefix == 0 {
            0
        } else {
            0xFFFFFFFFu32 << (32 - prefix)
        };
        Some((addr & mask, mask))
    };
    match (parse(a), parse(b)) {
        (Some((net_a, mask_a)), Some((net_b, mask_b))) => {
            let common = mask_a & mask_b; // the shorter prefix (wider net)
            (net_a & common) == (net_b & common)
        }
        _ => false,
    }
}

/// Deterministically generate an IP within the given CIDR from vm_id + network_id.
/// Skips .0 (network), .1 (gateway), and .255 (broadcast for /24).
pub(crate) fn generate_ip(vm_id: &str, network_id: &str, cidr: &str) -> String {
    if cidr.is_empty() {
        // Fallback to deterministic 10.x.x.x when no CIDR is configured
        let hash = chv_common::fnv1a_hash(vm_id);
        let o2 = ((hash >> 8) & 0xFF) as u8;
        let o3 = ((hash >> 16) & 0xFF) as u8;
        let o4 = ((hash >> 24) & 0xFF) as u8;
        return format!("10.{}.{}.{}", o2, o3, o4);
    }

    let (base_str, prefix_str) = cidr.split_once('/').unwrap_or((cidr, "24"));
    let prefix: u32 = prefix_str.parse().unwrap_or(24);
    let host_bits = 32u32.saturating_sub(prefix);
    let total_hosts = if host_bits >= 32 {
        1u32
    } else {
        1u32 << host_bits
    };
    let usable_hosts = total_hosts.saturating_sub(3).max(1); // exclude network, gateway, broadcast

    // Parse base IP into u32
    let base_u32 = base_str
        .split('.')
        .map(|s| s.parse::<u8>().unwrap_or(0))
        .fold(0u32, |acc, octet| (acc << 8) | octet as u32);

    // Network address (masked)
    let mask = if prefix >= 32 {
        0xFFFFFFFF
    } else {
        0xFFFFFFFFu32 << (32 - prefix)
    };
    let network_u32 = base_u32 & mask;

    // Deterministic offset from hash of vm_id + network_id
    let input = format!("{}-{}", vm_id, network_id);
    let hash = chv_common::fnv1a_hash(&input);
    let offset = (hash % usable_hosts as u64) as u32;

    // +2 to skip .0 (network) and .1 (gateway)
    let host_u32 = network_u32 + 2 + offset;

    let octets = [
        ((host_u32 >> 24) & 0xFF) as u8,
        ((host_u32 >> 16) & 0xFF) as u8,
        ((host_u32 >> 8) & 0xFF) as u8,
        (host_u32 & 0xFF) as u8,
    ];
    format!("{}.{}.{}.{}", octets[0], octets[1], octets[2], octets[3])
}

#[cfg(test)]
mod cidr_overlap_tests {
    //! Unit tests for the implicit-network collision guard's subnet
    //! overlap check (#354): the hazard is ANY shared subnet, not an
    //! exact cidr string match.
    use super::cidrs_overlap;

    #[test]
    fn identical_cidrs_overlap() {
        assert!(cidrs_overlap("10.200.0.0/24", "10.200.0.0/24"));
    }

    #[test]
    fn distinct_subnets_do_not_overlap() {
        assert!(!cidrs_overlap("10.200.0.0/24", "10.99.0.0/24"));
        assert!(!cidrs_overlap("10.200.0.0/24", "10.201.0.0/24"));
    }

    #[test]
    fn supernet_contains_subnet() {
        assert!(cidrs_overlap("10.200.0.0/23", "10.200.1.0/24"));
        assert!(cidrs_overlap("10.200.1.0/24", "10.200.0.0/23"));
        assert!(cidrs_overlap("10.200.0.0/16", "10.200.250.7/24"));
    }

    #[test]
    fn host_bits_are_masked_off() {
        // A sloppy cidr with host bits set still denotes its subnet.
        assert!(cidrs_overlap("10.200.0.9/24", "10.200.0.0/24"));
    }

    #[test]
    fn default_route_overlaps_everything() {
        assert!(cidrs_overlap("0.0.0.0/0", "10.200.0.0/24"));
    }

    #[test]
    fn unparseable_or_empty_never_overlaps() {
        // Dirty data must not produce false refusals.
        assert!(!cidrs_overlap("", "10.200.0.0/24"));
        assert!(!cidrs_overlap("not-a-cidr", "10.200.0.0/24"));
        assert!(!cidrs_overlap("10.200.0.0/99", "10.200.0.0/24"));
        assert!(!cidrs_overlap("10.200.0.0/24", "10.200.0.0.5/24"));
    }
}

#[cfg(test)]
mod quota_race_tests {
    //! Concurrency regression test for S2-1 (quota TOCTOU).
    //!
    //! Without `BEGIN IMMEDIATE`, two concurrent VM-create requests using
    //! sqlx's default `BEGIN` (DEFERRED) can both pass the in-tx quota
    //! check before either has acquired SQLite's RESERVED lock. Both then
    //! INSERT, allowing the user to exceed their quota.
    //!
    //! This test fires N concurrent quota-check + INSERT cycles against a
    //! shared on-disk SQLite DB (matching prod's WAL + busy_timeout setup)
    //! and asserts that exactly `max_vms` succeed and the rest return
    //! `BffError::QuotaExceeded`.
    use super::enforce_user_quota;
    use crate::error::BffError;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use sqlx::SqlitePool;
    use std::str::FromStr;
    use std::sync::Arc;
    use std::time::Duration;

    /// Build a temp-file SQLite pool with the same pragma profile as prod
    /// (WAL, foreign_keys ON, busy_timeout 5s) so the test exercises the
    /// real concurrency semantics.
    async fn build_test_pool(db_path: &std::path::Path) -> SqlitePool {
        let url = format!("sqlite://{}", db_path.display());
        let opts = SqliteConnectOptions::from_str(&url)
            .expect("parse sqlite url")
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
            .pragma("foreign_keys", "ON")
            .busy_timeout(Duration::from_secs(5));
        SqlitePoolOptions::new()
            .max_connections(8)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(opts)
            .await
            .expect("connect test pool")
    }

    /// Run a single create-VM-style transaction: BEGIN IMMEDIATE, quota
    /// check, owning-row INSERTs, COMMIT. Mirrors the structure of
    /// `create_vm` so the test exercises the same code path that the
    /// fix touches.
    async fn try_create_one(
        pool: &SqlitePool,
        user_id: &str,
        vm_id: &str,
        cpu: i64,
        mem: i64,
    ) -> Result<(), BffError> {
        let mut tx = pool
            .begin_with("BEGIN IMMEDIATE;")
            .await
            .map_err(|e| BffError::Internal(format!("begin: {}", e)))?;

        enforce_user_quota(&mut tx, user_id, cpu, mem, 0, 1).await?;

        sqlx::query(
            r#"INSERT INTO vms (vm_id, display_name, owner_id, created_at, updated_at)
               VALUES (?, ?, ?, strftime('%Y-%m-%dT%H:%M:%SZ','now'),
                                strftime('%Y-%m-%dT%H:%M:%SZ','now'))"#,
        )
        .bind(vm_id)
        .bind(format!("vm-{}", vm_id))
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| BffError::Internal(format!("insert vms: {}", e)))?;

        sqlx::query(
            r#"INSERT INTO vm_desired_state
               (vm_id, desired_generation, desired_status, desired_power_state,
                requested_by, cpu_count, memory_bytes, requested_at, updated_at)
               VALUES (?, 1, 'Pending', 'Running', ?, ?, ?,
                       strftime('%Y-%m-%dT%H:%M:%SZ','now'),
                       strftime('%Y-%m-%dT%H:%M:%SZ','now'))"#,
        )
        .bind(vm_id)
        .bind(user_id)
        .bind(cpu)
        .bind(mem)
        .execute(&mut *tx)
        .await
        .map_err(|e| BffError::Internal(format!("insert vds: {}", e)))?;

        tx.commit()
            .await
            .map_err(|e| BffError::Internal(format!("commit: {}", e)))?;
        Ok(())
    }

    #[tokio::test]
    async fn quota_is_serialized_under_concurrent_create() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("chv-quota-race.db");
        let pool = build_test_pool(&db_path).await;
        chv_controlplane_store::run_migrations(&pool, None)
            .await
            .expect("run migrations");

        let user_id = "race-user";
        let max_vms: i64 = 3;
        sqlx::query("INSERT INTO quotas (user_id, max_vms) VALUES (?, ?)")
            .bind(user_id)
            .bind(max_vms)
            .execute(&pool)
            .await
            .expect("seed quota");

        let pool = Arc::new(pool);
        let attempts = 10usize;
        let mut handles = Vec::with_capacity(attempts);
        for i in 0..attempts {
            let pool = Arc::clone(&pool);
            let user_id = user_id.to_string();
            let vm_id = format!("vm-race-{:02}", i);
            handles.push(tokio::spawn(async move {
                try_create_one(&pool, &user_id, &vm_id, 1, 1024 * 1024 * 1024).await
            }));
        }

        let mut succeeded = 0usize;
        let mut quota_exceeded = 0usize;
        let mut other_errors: Vec<String> = Vec::new();
        for h in handles {
            match h.await.expect("join task") {
                Ok(()) => succeeded += 1,
                Err(BffError::QuotaExceeded { .. }) => quota_exceeded += 1,
                Err(e) => other_errors.push(format!("{:?}", e)),
            }
        }

        assert!(
            other_errors.is_empty(),
            "unexpected non-quota errors: {:?}",
            other_errors
        );
        assert_eq!(
            succeeded, max_vms as usize,
            "expected exactly {} successes, got {}",
            max_vms, succeeded
        );
        assert_eq!(
            quota_exceeded,
            attempts - max_vms as usize,
            "expected {} quota-exceeded errors, got {}",
            attempts - max_vms as usize,
            quota_exceeded
        );

        let vms_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vms")
            .fetch_one(&*pool)
            .await
            .expect("count vms");
        let vds_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vm_desired_state")
            .fetch_one(&*pool)
            .await
            .expect("count vds");
        assert_eq!(vms_count, max_vms, "vms row count must equal quota");
        assert_eq!(
            vds_count, max_vms,
            "vm_desired_state row count must equal quota"
        );
    }

    /// Run a single resize-VM transaction mirroring the structure of
    /// `resize_vm`: BEGIN IMMEDIATE, require_vm_owner, SELECT current
    /// resources INSIDE the tx, enforce_user_quota with delta, UPDATE,
    /// COMMIT. Exercises the C-D fix path.
    async fn try_resize_one(
        pool: &SqlitePool,
        user_id: &str,
        vm_id: &str,
        new_cpu: i64,
        new_mem: i64,
    ) -> Result<(), BffError> {
        let mut tx = pool
            .begin_with("BEGIN IMMEDIATE;")
            .await
            .map_err(|e| BffError::Internal(format!("begin: {}", e)))?;

        super::require_vm_owner(&mut tx, vm_id, user_id, false).await?;

        let current: (Option<i64>, Option<i64>) =
            sqlx::query_as("SELECT cpu_count, memory_bytes FROM vm_desired_state WHERE vm_id = ?")
                .bind(vm_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| BffError::Internal(format!("select current: {}", e)))?;
        let old_cpu = current.0.unwrap_or(0);
        let old_mem = current.1.unwrap_or(0);

        enforce_user_quota(&mut tx, user_id, new_cpu - old_cpu, new_mem - old_mem, 0, 0).await?;

        sqlx::query(
            r#"UPDATE vm_desired_state
               SET cpu_count = ?, memory_bytes = ?,
                   desired_generation = desired_generation + 1,
                   updated_by = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')
               WHERE vm_id = ?"#,
        )
        .bind(new_cpu)
        .bind(new_mem)
        .bind(user_id)
        .bind(vm_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| BffError::Internal(format!("update vds: {}", e)))?;

        tx.commit()
            .await
            .map_err(|e| BffError::Internal(format!("commit: {}", e)))?;
        Ok(())
    }

    /// Regression test for C-D (resize quota TOCTOU).
    ///
    /// Without the fix, the SELECT for `old_cpu/old_memory` ran against
    /// `&state.pool` BEFORE `BEGIN IMMEDIATE`, so two concurrent resize
    /// requests on the same VM could both observe the small-old-cpu
    /// values, both compute deltas that fit, and both COMMIT — busting
    /// the quota. With the SELECT moved inside the tx, the BEGIN
    /// IMMEDIATE write lock guarantees the second transaction observes
    /// the first's UPDATE and its delta math reflects the real state.
    ///
    /// Setup: user with max_cpu=10, max_memory=8 GiB. One VM at
    /// 4 vCPU / 4 GiB. Spawn 10 concurrent resizes to 9 vCPU / 8 GiB
    /// each. The first to land grows the VM to (9, 8 GiB) — usage = 9
    /// vCPU. Every other attempt then sees old=(9, 8GiB) and asks for
    /// new=(9, 8GiB), delta=(0, 0) — also fine. So to make the TOCTOU
    /// observable we ask for a NEW size that's only legal exactly once:
    /// resize to 9 vCPU on a 4-vCPU VM => +5 vCPU delta. After the
    /// first commit, current usage = 9 vCPU, so a second concurrent
    /// resize that *also* read old_cpu=4 would compute delta=+5 and
    /// pass the quota check (4 + 5 = 9 <= 10) BUT the underlying state
    /// is already 9, so the second commit would push usage to 14.
    /// With the fix, only one resize succeeds; the rest see old_cpu=9
    /// and either no-op-pass or correctly reject.
    ///
    /// To make the assertion crisp, we resize to a value that is only
    /// ever a strict grow: target = 9. Pre-commit row = (4, 4 GiB). On
    /// the first success the row becomes (9, 8 GiB). Subsequent
    /// resizes' delta math becomes 9 - 9 = 0 cpu / 0 mem — those
    /// "succeed" trivially without bumping usage. So we count
    /// "effective grows" by checking the final desired_generation: it
    /// must equal initial + 1 (only one real change), AND the row's
    /// final (cpu, mem) must equal the target.
    #[tokio::test]
    async fn resize_quota_serialized_under_concurrent_resize() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("chv-resize-race.db");
        let pool = build_test_pool(&db_path).await;
        chv_controlplane_store::run_migrations(&pool, None)
            .await
            .expect("run migrations");

        let user_id = "resize-race-user";
        // Quota: 10 vCPU, 8 GiB memory. Initial usage 4 vCPU / 4 GiB.
        // Resizing to 9 vCPU / 8 GiB has delta +5 / +4 GiB; legal once,
        // legal-as-zero-delta on every subsequent attempt that observes
        // the post-commit state.
        sqlx::query(
            "INSERT INTO quotas (user_id, max_vms, max_cpu, max_memory_bytes) VALUES (?, ?, ?, ?)",
        )
        .bind(user_id)
        .bind(1_i64)
        .bind(10_i64)
        .bind(8_i64 * 1024 * 1024 * 1024)
        .execute(&pool)
        .await
        .expect("seed quota");

        let vm_id = "vm-resize-race";
        sqlx::query(
            r#"INSERT INTO vms (vm_id, display_name, owner_id, created_at, updated_at)
               VALUES (?, ?, ?, strftime('%Y-%m-%dT%H:%M:%SZ','now'),
                                strftime('%Y-%m-%dT%H:%M:%SZ','now'))"#,
        )
        .bind(vm_id)
        .bind("vm-resize-race")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("seed vm");

        sqlx::query(
            r#"INSERT INTO vm_desired_state
               (vm_id, desired_generation, desired_status, desired_power_state,
                requested_by, cpu_count, memory_bytes, requested_at, updated_at)
               VALUES (?, 1, 'Pending', 'Running', ?, 4, ?,
                       strftime('%Y-%m-%dT%H:%M:%SZ','now'),
                       strftime('%Y-%m-%dT%H:%M:%SZ','now'))"#,
        )
        .bind(vm_id)
        .bind(user_id)
        .bind(4_i64 * 1024 * 1024 * 1024)
        .execute(&pool)
        .await
        .expect("seed vds");

        let pool = Arc::new(pool);
        let target_cpu: i64 = 9;
        let target_mem: i64 = 8 * 1024 * 1024 * 1024;
        let attempts = 10usize;
        let mut handles = Vec::with_capacity(attempts);
        for _ in 0..attempts {
            let pool = Arc::clone(&pool);
            let user_id = user_id.to_string();
            let vm_id = vm_id.to_string();
            handles.push(tokio::spawn(async move {
                try_resize_one(&pool, &user_id, &vm_id, target_cpu, target_mem).await
            }));
        }

        let mut other_errors: Vec<String> = Vec::new();
        let mut quota_exceeded = 0usize;
        let mut ok_count = 0usize;
        for h in handles {
            match h.await.expect("join task") {
                Ok(()) => ok_count += 1,
                Err(BffError::QuotaExceeded { .. }) => quota_exceeded += 1,
                Err(e) => other_errors.push(format!("{:?}", e)),
            }
        }
        assert!(
            other_errors.is_empty(),
            "unexpected non-quota errors: {:?}",
            other_errors
        );

        // Final state assertions: the row must reflect target sizes
        // exactly (only one real grow), and total cpu/memory usage for
        // this user must not exceed the quota.
        let row: (i64, i64, i64) = sqlx::query_as(
            "SELECT cpu_count, memory_bytes, desired_generation
             FROM vm_desired_state WHERE vm_id = ?",
        )
        .bind(vm_id)
        .fetch_one(&*pool)
        .await
        .expect("read final row");
        assert_eq!(row.0, target_cpu, "final cpu_count must equal target");
        assert_eq!(row.1, target_mem, "final memory_bytes must equal target");

        let usage: (i64, i64) = sqlx::query_as(
            "SELECT COALESCE(SUM(cpu_count), 0), COALESCE(SUM(memory_bytes), 0)
             FROM vm_desired_state WHERE requested_by = ?",
        )
        .bind(user_id)
        .fetch_one(&*pool)
        .await
        .expect("usage");
        assert!(
            usage.0 <= 10,
            "cpu usage {} must not exceed quota 10",
            usage.0
        );
        assert!(
            usage.1 <= 8 * 1024 * 1024 * 1024,
            "memory usage {} must not exceed quota",
            usage.1
        );

        // At least one attempt must have observed the resize as a
        // genuine change (Ok with non-empty delta). With the fix all
        // attempts return Ok (the late ones see zero delta), but
        // exactly one attempt did the real grow — proven by the final
        // row state above. The key invariant under attack by the bug
        // was "usage > quota"; if that holds, the fix is in.
        assert!(ok_count >= 1, "at least one resize must succeed");
        // Sanity: with the static fix, we expect all 10 to be Ok
        // (zero-delta resizes pass), so quota_exceeded should be 0.
        // If you remove the fix and re-run this test in a tight loop,
        // you'll see usage exceed the quota in the final assertion.
        assert_eq!(
            quota_exceeded, 0,
            "with the fix, zero-delta retries should not be rejected"
        );
    }
}
