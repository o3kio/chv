use axum::{extract::State, response::Json};
use serde_json::{json, Value};

use crate::router::AppState;
use crate::BffError;

pub async fn list_clusters(
    crate::auth::BearerToken(_claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    _payload: axum::Json<Value>,
) -> Result<Json<Value>, BffError> {
    let row = load_cluster_summary(&state.pool)
        .await
        .map_err(|e| BffError::Internal(format!("failed to list clusters: {}", e)))?;

    let state_str = if row.degraded_count > 0 {
        "degraded"
    } else if row.maintenance_count > 0 {
        "warning"
    } else {
        "healthy"
    };

    let top_issue = if row.version_skew.unwrap_or(false) {
        Some("Version skew")
    } else if row.degraded_count > 0 {
        Some("Node health degraded")
    } else {
        None
    };

    let items = vec![json!({
        "cluster_id": "c-fleet-1",
        "name": "default-fleet",
        "datacenter": "Primary",
        "node_count": row.node_count,
        "state": state_str,
        "maintenance": row.maintenance_count > 0,
        "version": row.version.unwrap_or_else(|| "unknown".into()),
        "version_skew": row.version_skew.unwrap_or(false),
        "cpu_percent": row.cpu_percent.unwrap_or(0),
        "memory_percent": row.memory_percent.unwrap_or(0),
        "storage_percent": row.storage_percent.unwrap_or(0),
        "active_tasks": row.active_tasks,
        "alerts": row.alerts,
        "top_issue": top_issue,
    })];

    Ok(Json(json!({
        "items": items,
        "page": {
            "page": 1,
            "page_size": 50,
            "total_items": items.len() as u64,
        },
        "filters": {
            "applied": {}
        },
    })))
}

/// Load the fleet summary row. Extracted from the handler so the
/// CTE/CROSS-JOIN aggregate (stop/stale-sample filtering and the storage
/// allocation ratio) can be regression-tested against a real migrated
/// SQLite pool without constructing a full `AppState`.
async fn load_cluster_summary(pool: &sqlx::SqlitePool) -> Result<ClusterRow, sqlx::Error> {
    sqlx::query_as::<_, ClusterRow>(
        r#"
        WITH latest_running_vm_metrics AS (
            -- Latest sample per VM, restricted to VMs whose observed state is
            -- Running with a KNOWN health value (same join as metrics.rs).
            -- A stale sample from a stopped VM must not skew fleet averages;
            -- core-managed desired-Running-but-unknown is excluded rather than
            -- presented as observed running. Computed once and reused for CPU
            -- and memory to avoid two full vm_metrics scans.
            SELECT m.vm_id, m.cpu_percent, m.memory_bytes_used, m.memory_bytes_total
            FROM vm_metrics m
            JOIN (
                SELECT vm_id, MAX(collected_at) AS latest
                FROM vm_metrics
                GROUP BY vm_id
            ) latest ON m.vm_id = latest.vm_id AND m.collected_at = latest.latest
            JOIN vm_observed_state s
              ON s.vm_id = m.vm_id
             AND s.runtime_status = 'Running'
             AND COALESCE(s.health_status, 'Unknown') != 'Unknown'
        )
        SELECT
            COUNT(*) AS node_count,
            SUM(CASE WHEN nos.health_status IN ('degraded', 'warning', 'critical') THEN 1 ELSE 0 END) AS degraded_count,
            SUM(CASE WHEN nds.desired_state = 'Maintenance' THEN 1 ELSE 0 END) AS maintenance_count,
            (SELECT agent_version FROM nodes GROUP BY agent_version ORDER BY COUNT(*) DESC LIMIT 1) AS version,
            CASE WHEN COUNT(DISTINCT n.agent_version) > 1 THEN 1 ELSE 0 END AS version_skew,
            (SELECT COUNT(*) FROM operations WHERE status IN ('Pending', 'Accepted', 'Running')) AS active_tasks,
            (SELECT COUNT(*) FROM alerts WHERE status IN ('open','firing')) AS alerts,
            -- Utilization is computed from observed data, never a fixed
            -- placeholder. NULL/zero means "no observed data yet", which is
            -- the honest answer for a fresh fleet.
            util.cpu_percent,
            util.memory_percent,
            (
                -- ALLOCATION percentage, not live usage: provisioned volume
                -- capacity over enrolled node disk capacity. It is the
                -- operator's overcommit signal and can legitimately exceed
                -- 100% when thin-provisioned volumes overcommit physical
                -- space. Do not clamp or fabricate it; NULL/zero -> 0.
                SELECT CAST(COALESCE(ROUND(100.0 * SUM(v.capacity_bytes)
                    / NULLIF((SELECT SUM(ni.disk_bytes) FROM node_inventory ni), 0)), 0) AS INTEGER)
                FROM volumes v
            ) AS storage_percent
        FROM nodes n
        LEFT JOIN node_observed_state nos ON n.node_id = nos.node_id
        LEFT JOIN node_desired_state nds ON n.node_id = nds.node_id
        -- Scalar aggregate over the latest running VM samples: one row, with
        -- NULLs coalesced to 0 when the CTE is empty.
        CROSS JOIN (
            SELECT
                CAST(COALESCE(ROUND(AVG(cpu_percent)), 0) AS INTEGER) AS cpu_percent,
                CAST(COALESCE(ROUND(100.0 * SUM(memory_bytes_used)
                    / NULLIF(SUM(memory_bytes_total), 0)), 0) AS INTEGER) AS memory_percent
            FROM latest_running_vm_metrics
        ) util
        "#,
    )
    .fetch_one(pool)
    .await
}

#[derive(sqlx::FromRow)]
struct ClusterRow {
    node_count: i64,
    degraded_count: i64,
    maintenance_count: i64,
    version: Option<String>,
    version_skew: Option<bool>,
    active_tasks: i64,
    alerts: i64,
    cpu_percent: Option<i64>,
    memory_percent: Option<i64>,
    storage_percent: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::load_cluster_summary;
    use sqlx::sqlite::SqlitePoolOptions;
    use sqlx::SqlitePool;

    async fn migrated_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory sqlite");
        chv_controlplane_store::run_migrations(&pool, None)
            .await
            .expect("run migrations");
        pool
    }

    /// Regression for the fleet utilization CTE/CROSS-JOIN:
    /// - CPU is the average of the LATEST sample of VMs observed Running with a
    ///   known health value (two samples on one VM prove latest-wins); a
    ///   stopped VM's (even newer) stale sample must not skew it, and a
    ///   desired-Running VM whose health is `Unknown` must be excluded.
    /// - Memory is running-only used/total.
    /// - Storage is provisioned volume capacity over enrolled node disk.
    /// - With no metric rows at all, CPU/memory fall back to 0 (not NULL).
    #[tokio::test]
    async fn cluster_summary_uses_running_only_metrics_and_storage_allocation() {
        let pool = migrated_pool().await;
        sqlx::query(
            "INSERT INTO nodes (node_id, hostname, display_name, agent_version) \
             VALUES ('node-1', 'node-1', 'Node 1', '0.4.0')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO node_inventory \
                 (node_id, architecture, cpu_count, memory_bytes, disk_bytes) \
             VALUES ('node-1', 'x86_64', 8, 17179869184, 1000000000000)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO vms (vm_id, display_name) VALUES \
             ('vm-running', 'running'), ('vm-stopped', 'stopped'), \
             ('vm-unknown', 'unknown')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO vm_observed_state \
                 (vm_id, observed_generation, runtime_status, health_status) VALUES \
             ('vm-running', 1, 'Running', 'Healthy'), \
             ('vm-stopped', 1, 'Stopped', 'Healthy'), \
             ('vm-unknown', 1, 'Running', 'Unknown')",
        )
        .execute(&pool)
        .await
        .unwrap();
        // vm-running has TWO samples: only its latest (40%, 4/8 GiB) counts.
        // The stopped VM's sample is NEWER (latest per VM), so a missing
        // runtime-state filter would pull it into the average. The
        // desired-Running-but-Unknown VM is excluded by the health filter.
        sqlx::query(
            "INSERT INTO vm_metrics \
                 (vm_id, collected_at, cpu_percent, memory_bytes_used, memory_bytes_total) VALUES \
             ('vm-running', '2026-01-01T00:00:00Z', 10.0, 1073741824, 8589934592), \
             ('vm-running', '2026-01-01T01:00:00Z', 40.0, 4294967296, 8589934592), \
             ('vm-stopped', '2026-01-02T00:00:00Z', 90.0, 7516192768, 8589934592), \
             ('vm-unknown', '2026-01-03T00:00:00Z', 80.0, 8589934592, 8589934592)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // 600 GB provisioned over 1000 GB enrolled disk = 60%.
        sqlx::query(
            "INSERT INTO volumes (volume_id, node_id, display_name, capacity_bytes) \
             VALUES ('vol-1', 'node-1', 'vol 1', 600000000000)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let row = load_cluster_summary(&pool)
            .await
            .expect("cluster summary query must run against real migrations");
        assert_eq!(
            row.cpu_percent,
            Some(40),
            "CPU must use vm-running's latest sample only, exclude the stopped VM's \
             90% sample, and exclude the Unknown-health VM's 80% sample"
        );
        assert_eq!(
            row.memory_percent,
            Some(50),
            "memory must use the running VM's latest 4/8 GiB, excluding the stopped \
             VM's 7 GiB and the Unknown-health VM's 8 GiB"
        );
        assert_eq!(
            row.storage_percent,
            Some(60),
            "storage must be provisioned-volume allocation over enrolled disk capacity"
        );

        // No observed metrics at all: the honest answer is 0, never NULL/fabricated.
        sqlx::query("DELETE FROM vm_metrics")
            .execute(&pool)
            .await
            .unwrap();
        let empty = load_cluster_summary(&pool).await.unwrap();
        assert_eq!(empty.cpu_percent, Some(0));
        assert_eq!(empty.memory_percent, Some(0));
        assert_eq!(
            empty.storage_percent,
            Some(60),
            "storage is independent of VM metrics"
        );
    }
}
