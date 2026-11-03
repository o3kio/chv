use axum::{extract::State, response::Json};
use serde_json::{json, Value};
use std::path::Path;

use crate::auth::{require_admin, require_operator_or_admin, BearerToken};
use crate::router::AppState;
use crate::BffError;
use chv_controlplane_store::{
    CertificateRevocationRepository, EventAppendInput, RevokeCertificateInput,
};
use chv_controlplane_types::domain::{
    ActorId, EventSeverity, EventType, NodeId, ResourceId, ResourceKind,
};

pub async fn list_nodes(
    BearerToken(_claims): BearerToken,
    State(state): State<AppState>,
    axum::Json(payload): axum::Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, BffError> {
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
    let cache_key = format!("nodes:list:{}:{}", page, page_size);
    if let Some(cached) = state.cache.get(&cache_key).await {
        return Ok(Json(
            serde_json::from_str(&cached).map_err(|e| BffError::Internal(e.to_string()))?,
        ));
    }

    let offset = (page - 1) * page_size;
    let total_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM nodes")
        .fetch_one(&state.pool)
        .await
        .map_err(|e| BffError::Internal(format!("failed to count nodes: {}", e)))?;
    let total_pages = (total_count as u64).div_ceil(page_size);

    let rows = sqlx::query_as::<_, NodeRow>(
        r#"
        SELECT
            n.node_id,
            n.display_name AS name,
            COALESCE(nos.observed_state, 'Unknown') AS state,
            COALESCE(nos.health_status, 'unknown') AS health,
            COALESCE(CAST(ni.cpu_count AS TEXT), '') AS cpu,
            CASE WHEN ni.memory_bytes IS NULL THEN ''
                 WHEN ni.memory_bytes >= 1073741824 THEN printf('%.1f GiB', CAST(ni.memory_bytes AS REAL)/1073741824.0)
                 WHEN ni.memory_bytes >= 1048576 THEN printf('%.1f MiB', CAST(ni.memory_bytes AS REAL)/1048576.0)
                 WHEN ni.memory_bytes >= 1024 THEN printf('%.1f KiB', CAST(ni.memory_bytes AS REAL)/1024.0)
                 ELSE printf('%d B', ni.memory_bytes) END AS memory,
            CASE WHEN ni.disk_bytes IS NULL THEN ''
                 WHEN ni.disk_bytes >= 1073741824 THEN printf('%.1f GiB', CAST(ni.disk_bytes AS REAL)/1073741824.0)
                 WHEN ni.disk_bytes >= 1048576 THEN printf('%.1f MiB', CAST(ni.disk_bytes AS REAL)/1048576.0)
                 WHEN ni.disk_bytes >= 1024 THEN printf('%.1f KiB', CAST(ni.disk_bytes AS REAL)/1024.0)
                 ELSE printf('%d B', ni.disk_bytes) END AS storage,
            '' AS network,
            COALESCE(n.agent_version, '') AS version,
            COALESCE(nds.desired_state = 'Maintenance', false) AS maintenance,
            COALESCE(nds.scheduling_paused, false) AS scheduling_paused,
            COALESCE(task_counts.active_tasks, 0) AS active_tasks,
            COALESCE(alert_counts.alerts, 0) AS alerts,
            COALESCE(ni.hypervisor_capabilities, '') AS hypervisor_capabilities
        FROM nodes n
        LEFT JOIN node_observed_state nos ON n.node_id = nos.node_id
        LEFT JOIN node_desired_state nds ON n.node_id = nds.node_id
        LEFT JOIN node_inventory ni ON n.node_id = ni.node_id
        LEFT JOIN (
            SELECT resource_id, COUNT(*) AS active_tasks
            FROM operations
            WHERE resource_kind = 'node'
              AND status IN ('Pending', 'Accepted', 'Running')
            GROUP BY resource_id
        ) task_counts ON n.node_id = task_counts.resource_id
        LEFT JOIN (
            SELECT node_id, COUNT(*) AS alerts
            FROM alerts
            WHERE status IN ('open','firing')
            GROUP BY node_id
        ) alert_counts ON n.node_id = alert_counts.node_id
        ORDER BY n.node_id
        LIMIT ? OFFSET ?
        "#,
    )
    .bind(page_size as i64)
    .bind(offset as i64)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| BffError::Internal(format!("failed to list nodes: {}", e)))?;

    let items: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "node_id": r.node_id,
                "name": r.name,
                "cluster": "",
                "state": r.state,
                "health": r.health,
                "cpu": r.cpu,
                "memory": r.memory,
                "storage": r.storage,
                "network": r.network,
                "version": r.version,
                "maintenance": r.maintenance,
                "active_tasks": r.active_tasks,
                "alerts": r.alerts,
                "hypervisor_capabilities": parse_caps(&r.hypervisor_capabilities),
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

pub async fn get_node(
    BearerToken(_claims): BearerToken,
    State(state): State<AppState>,
    axum::Json(payload): axum::Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, BffError> {
    let node_id = payload
        .get("node_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing node_id".into()))?;

    let row = sqlx::query_as::<_, NodeRow>(
        r#"
        SELECT
            n.node_id,
            n.display_name AS name,
            COALESCE(nos.observed_state, 'Unknown') AS state,
            COALESCE(nos.health_status, 'unknown') AS health,
            COALESCE(CAST(ni.cpu_count AS TEXT), '') AS cpu,
            CASE WHEN ni.memory_bytes IS NULL THEN ''
                 WHEN ni.memory_bytes >= 1073741824 THEN printf('%.1f GiB', CAST(ni.memory_bytes AS REAL)/1073741824.0)
                 WHEN ni.memory_bytes >= 1048576 THEN printf('%.1f MiB', CAST(ni.memory_bytes AS REAL)/1048576.0)
                 WHEN ni.memory_bytes >= 1024 THEN printf('%.1f KiB', CAST(ni.memory_bytes AS REAL)/1024.0)
                 ELSE printf('%d B', ni.memory_bytes) END AS memory,
            CASE WHEN ni.disk_bytes IS NULL THEN ''
                 WHEN ni.disk_bytes >= 1073741824 THEN printf('%.1f GiB', CAST(ni.disk_bytes AS REAL)/1073741824.0)
                 WHEN ni.disk_bytes >= 1048576 THEN printf('%.1f MiB', CAST(ni.disk_bytes AS REAL)/1048576.0)
                 WHEN ni.disk_bytes >= 1024 THEN printf('%.1f KiB', CAST(ni.disk_bytes AS REAL)/1024.0)
                 ELSE printf('%d B', ni.disk_bytes) END AS storage,
            '' AS network,
            COALESCE(n.agent_version, '') AS version,
            COALESCE(nds.desired_state = 'Maintenance', false) AS maintenance,
            COALESCE(nds.scheduling_paused, false) AS scheduling_paused,
            COALESCE(task_counts.active_tasks, 0) AS active_tasks,
            COALESCE(alert_counts.alerts, 0) AS alerts,
            COALESCE(ni.hypervisor_capabilities, '') AS hypervisor_capabilities
        FROM nodes n
        LEFT JOIN node_observed_state nos ON n.node_id = nos.node_id
        LEFT JOIN node_desired_state nds ON n.node_id = nds.node_id
        LEFT JOIN node_inventory ni ON n.node_id = ni.node_id
        LEFT JOIN (
            SELECT resource_id, COUNT(*) AS active_tasks
            FROM operations
            WHERE resource_kind = 'node'
              AND status IN ('Pending', 'Accepted', 'Running')
            GROUP BY resource_id
        ) task_counts ON n.node_id = task_counts.resource_id
        LEFT JOIN (
            SELECT node_id, COUNT(*) AS alerts
            FROM alerts
            WHERE status IN ('open','firing')
            GROUP BY node_id
        ) alert_counts ON n.node_id = alert_counts.node_id
        WHERE n.node_id = $1
        "#,
    )
    .bind(node_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| BffError::Internal(format!("failed to get node: {}", e)))?;

    match row {
        Some(r) => {
            let hosted_vms = sqlx::query_as::<_, HostedVmRow>(
                r#"
                SELECT
                    v.vm_id,
                    v.display_name AS name,
                    -- #368 P3: same Failed-wins precedence as the VM list
                    -- and detail views (vms.rs) — an agent-reported Failed
                    -- state always wins over the desired power state, so a
                    -- terminally-failed create never shows as the phantom
                    -- desired state on the node's hosted-VM list.
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
                         ELSE printf('%d B', vds.memory_bytes) END AS memory
                FROM vms v
                LEFT JOIN vm_desired_state vds ON v.vm_id = vds.vm_id
                LEFT JOIN vm_observed_state vos ON v.vm_id = vos.vm_id
                WHERE v.node_id = $1
                ORDER BY v.vm_id
                "#,
            )
            .bind(node_id)
            .fetch_all(&state.pool)
            .await
            .map_err(|e| BffError::Internal(format!("failed to get hosted vms: {}", e)))?;

            let hosted_vms_json: Vec<Value> = hosted_vms
                .into_iter()
                .map(|vm| {
                    json!({
                        "vm_id": vm.vm_id,
                        "name": vm.name,
                        "power_state": vm.power_state,
                        "last_error": vm.last_error,
                        "health": vm.health,
                        "cpu": vm.cpu,
                        "memory": vm.memory,
                    })
                })
                .collect();

            let recent_tasks = sqlx::query_as::<_, RecentTaskRow>(
                r#"
                SELECT
                    operation_id AS task_id,
                    status,
                    operation_type AS summary,
                    operation_type AS operation,
                    CAST(strftime('%s', requested_at) AS INTEGER) * 1000 AS started_unix_ms,
                    error_code,
                    error_message
                FROM operations
                WHERE resource_kind = 'node' AND resource_id = $1
                ORDER BY requested_at DESC
                LIMIT 5
                "#,
            )
            .bind(node_id)
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
                        "operation": t.operation,
                        "started_unix_ms": t.started_unix_ms,
                        // #502: NULL until a terminal failure records a
                        // cause — passed through verbatim, never
                        // fabricated.
                        "error_code": t.error_code,
                        "error_message": t.error_message,
                    })
                })
                .collect();

            let configuration: Vec<Value> = vec![
                json!({"label": "Node ID", "value": r.node_id.clone()}),
                json!({"label": "Version", "value": r.version.clone()}),
                json!({"label": "CPU", "value": r.cpu.clone()}),
                json!({"label": "Memory", "value": r.memory.clone()}),
                json!({"label": "Storage backend", "value": "zfs"}),
                json!({"label": "Hypervisor Capabilities", "value": parse_caps(&r.hypervisor_capabilities).join(", ")}),
            ];

            let sections = vec![
                json!({"id": "summary", "label": "Summary"}),
                json!({"id": "vms", "label": "VMs", "count": hosted_vms_json.len()}),
                json!({"id": "tasks", "label": "Tasks", "count": tasks_json.len()}),
                json!({"id": "configuration", "label": "Configuration"}),
            ];

            Ok(Json(json!({
                "state": "ready",
                "summary": {
                    "node_id": r.node_id,
                    "name": r.name,
                    "cluster": "",
                    "state": r.state,
                    "health": r.health,
                    "version": r.version,
                    "cpu": r.cpu,
                    "memory": r.memory,
                    "storage": r.storage,
                    "network": r.network,
                    "maintenance": r.maintenance,
                    "scheduling": !r.scheduling_paused,
                    "hypervisor_capabilities": parse_caps(&r.hypervisor_capabilities),
                },
                "sections": sections,
                "hostedVms": hosted_vms_json,
                "recentTasks": tasks_json,
                "configuration": configuration,
            })))
        }
        None => Err(BffError::NotFound(format!("node {} not found", node_id))),
    }
}

pub async fn mutate_node(
    crate::auth::BearerToken(claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    axum::Json(payload): axum::Json<Value>,
) -> Result<Json<Value>, BffError> {
    require_operator_or_admin(&claims)?;
    let node_id = payload
        .get("node_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing node_id".into()))?
        .to_string();

    let action = payload
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing action".into()))?
        .to_string();

    let response = state
        .mutations
        .mutate_node(node_id, action, claims.sub)
        .await?;

    state.cache.invalidate("nodes:").await;
    state.cache.invalidate("overview").await;
    Ok(Json(json!({
        "accepted": response.accepted,
        "task_id": response.task_id,
        "node_id": response.node_id,
        "summary": response.summary,
    })))
}

/// Manual enrollment recovery: clears the agent cache so the next agent restart
/// triggers a fresh enrollment bootstrap. Returns instructions for the operator.
pub async fn enroll_node(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
) -> Result<Json<Value>, BffError> {
    require_admin(&claims)?;
    let cache_path = Path::new("/var/lib/chv/cache/agent-cache.json");
    let cache_existed = cache_path.exists();

    if cache_existed {
        match tokio::fs::remove_file(cache_path).await {
            Ok(_) => {}
            Err(e) => {
                return Err(BffError::Internal(format!(
                    "failed to remove agent cache: {}",
                    e
                )));
            }
        }
    }

    // Also ensure a bootstrap token exists in the DB for the agent to consume
    let token_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM bootstrap_tokens WHERE used_at IS NULL")
            .fetch_one(&state.pool)
            .await
            .map_err(|e| BffError::Internal(format!("failed to check bootstrap tokens: {}", e)))?;

    let token_ready = token_count > 0;

    state.cache.invalidate("nodes:").await;
    state.cache.invalidate("overview").await;
    Ok(Json(json!({
        "success": true,
        "cache_cleared": cache_existed,
        "token_ready": token_ready,
        "message": if cache_existed {
            "Agent enrollment cache cleared. Restart the agent service to complete enrollment: sudo systemctl restart chv-agent"
        } else {
            "No stale enrollment cache found. If the agent is not enrolling, ensure the bootstrap token is valid and restart the agent service."
        },
    })))
}

/// `POST /v1/nodes/revoke-certificate` (admin only).
///
/// Adds the node certificate serial to the durable denylist checked by the
/// gRPC peer-identity interceptor. Body:
///
/// ```json
/// {"node_id":"node-a","certificate_serial":"0a3f7b2c","reason":"host compromise"}
/// ```
///
/// `certificate_serial` is optional and defaults to the serial currently
/// recorded for the node; operators should pass it explicitly when revoking a
/// stale/previous certificate. `expires_at` (RFC 3339) is optional pruning
/// metadata only — a row always denies its serial regardless of that value.
///
/// Idempotent: a repeat request for an already-revoked serial returns 200 with
/// `already_revoked: true`, keeps the original reason/time/actor, and does not
/// duplicate the audit event. Revoking also burns certificate rotation for the
/// serial: the store's compare-and-swap refuses it, forcing re-enrollment.
pub async fn revoke_node_certificate(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, BffError> {
    require_admin(&claims)?;

    let node_id_text = payload
        .get("node_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| BffError::BadRequest("node_id is required".into()))?;
    let node_id = NodeId::new(node_id_text.to_string())
        .map_err(|e| BffError::BadRequest(format!("invalid node_id: {e}")))?;

    let reason = payload
        .get("reason")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            BffError::BadRequest(
                "reason is required: record why the certificate is being revoked".into(),
            )
        })?;
    if reason.len() > 1024 {
        return Err(BffError::BadRequest(
            "reason must not exceed 1024 bytes".into(),
        ));
    }

    let expires_at = payload
        .get("expires_at")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(value) = expires_at {
        chrono::DateTime::parse_from_rfc3339(value).map_err(|e| {
            BffError::BadRequest(format!(
                "expires_at must be RFC 3339, e.g. 2027-01-01T00:00:00Z: {e}"
            ))
        })?;
    }

    // Verify the node exists and resolve the default serial. A missing node is
    // a 404 (likely a typo); a legacy row with no serial requires an explicit
    // serial because guessing is not safe.
    let recorded_serial = state.node_repo.get_certificate_serial(&node_id).await?;
    let certificate_serial = match payload
        .get("certificate_serial")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(serial) => serial.to_string(),
        None => recorded_serial.clone().ok_or_else(|| {
            BffError::BadRequest(format!(
                "node {node_id} has no recorded certificate serial (legacy enrollment); \
                 pass certificate_serial explicitly"
            ))
        })?,
    };
    if certificate_serial.len() > 256 {
        return Err(BffError::BadRequest(
            "certificate_serial must not exceed 256 bytes".into(),
        ));
    }

    let repo = CertificateRevocationRepository::new(state.pool.clone());
    let outcome = repo
        .revoke(&RevokeCertificateInput {
            certificate_serial: &certificate_serial,
            node_id: node_id.as_str(),
            reason,
            revoked_by: Some(claims.username.as_str()),
            revoked_at: None,
            expires_at,
        })
        .await?;

    // Audit trail: durable event plus the structured tracing target. Emit only
    // on the first revocation so retries cannot inflate the journal. Both are
    // best-effort because the security boundary (the denylist row) is already
    // committed.
    if !outcome.already_revoked {
        tracing::warn!(
            target: "audit",
            user = %claims.username,
            action = "revoke_node_certificate",
            resource_kind = "node",
            resource_id = %node_id,
            certificate_serial = %certificate_serial,
            reason = %reason,
            "node certificate revoked"
        );

        let event = EventAppendInput {
            occurred_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or_default(),
            event_type: EventType::Audit,
            severity: EventSeverity::Warning,
            resource_kind: Some(ResourceKind::Node),
            resource_id: ResourceId::new(node_id.as_str().to_string()).ok(),
            node_id: Some(node_id.clone()),
            operation_id: None,
            actor_id: ActorId::new(claims.username.clone()).ok(),
            requested_by: Some(claims.username.clone()),
            correlation_id: None,
            message: format!(
                "node certificate revoked for {node_id} (serial {certificate_serial}): {reason}"
            ),
            details: Some(
                json!({
                    "certificate_serial": certificate_serial,
                    "reason": reason,
                    "expires_at": expires_at,
                })
                .to_string(),
            ),
        };
        if let Err(error) = state.event_repo.append(&event).await {
            tracing::warn!(
                %error,
                node_id = %node_id,
                "failed to append certificate-revocation audit event"
            );
        }
    }

    let revocation = outcome.revocation;
    Ok(Json(json!({
        "node_id": revocation.node_id,
        "certificate_serial": revocation.certificate_serial,
        "reason": revocation.reason,
        "revoked_at": revocation.revoked_at,
        "revoked_by": revocation.revoked_by,
        "expires_at": revocation.expires_at,
        "already_revoked": outcome.already_revoked,
        "active_serial": recorded_serial,
    })))
}

#[derive(sqlx::FromRow)]
struct NodeRow {
    node_id: String,
    name: String,
    state: String,
    health: String,
    cpu: String,
    memory: String,
    storage: String,
    network: String,
    version: String,
    maintenance: bool,
    scheduling_paused: bool,
    active_tasks: i32,
    alerts: i32,
    hypervisor_capabilities: String,
}

fn parse_caps(raw: &str) -> Vec<String> {
    if raw.is_empty() {
        return Vec::new();
    }
    serde_json::from_str(raw).unwrap_or_else(|_| vec![raw.to_string()])
}

#[derive(sqlx::FromRow)]
struct HostedVmRow {
    vm_id: String,
    name: String,
    power_state: String,
    last_error: Option<String>,
    health: String,
    cpu: String,
    memory: String,
}

#[derive(sqlx::FromRow)]
struct RecentTaskRow {
    task_id: String,
    status: String,
    summary: String,
    operation: String,
    started_unix_ms: Option<i64>,
    // #502: the terminal-failure cause the fast-fail work journals.
    error_code: Option<String>,
    error_message: Option<String>,
}
