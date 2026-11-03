//! Viewer-tier read surface for live-migration operations (#372 DP4b).
//!
//! The migration machinery is real and live — the `migrations` table
//! (`0038_migration_operations.sql`), the CP migration loop with
//! cooperative cancel, and the admin-tier cancel route
//! (`POST /admin/migrations/{id}/cancel`) — but the BFF had no read
//! surface over any of it, which is why `chvctl migrate status`/`list`
//! 404'd from introduction (#372 §2.3). These routes are plain SELECTs
//! over the existing table: read-only, no journaling, no
//! mutation-service involvement, viewer tier (any authenticated
//! bearer). The GET + path-param shape follows the backup read routes
//! (`GET /v1/backups/jobs[/:job_id]`), matching the design's named
//! paths — the start/cancel entry points stay where they already
//! lived (the vm-mutate migrate action and the CP admin router).

use axum::{
    extract::{Path, State},
    response::Json,
};
use serde_json::{json, Value};

use crate::router::AppState;
use crate::BffError;

/// One `migrations`-table row. Column names are the table's own (the
/// operator-facing vocabulary `chvctl migrate list` prints); the only
/// derived field is `cancel_requested`, the cooperative-cancel flag
/// the admin-tier cancel route sets and the migration loop observes.
#[derive(sqlx::FromRow)]
struct MigrationRow {
    migration_id: String,
    operation_id: String,
    vm_id: String,
    source_node_id: String,
    destination_node_id: String,
    phase: String,
    bytes_transferred: i64,
    total_bytes: i64,
    convergence_round: i64,
    dirty_blocks_remaining: i64,
    started_at: String,
    updated_at: String,
    completed_at: Option<String>,
    error_message: Option<String>,
    cancel_requested_at: Option<String>,
    interrupted_from_phase: Option<String>,
    interrupted_at: Option<String>,
}

impl MigrationRow {
    fn to_json(&self) -> Value {
        json!({
            "migration_id": self.migration_id,
            "operation_id": self.operation_id,
            "vm_id": self.vm_id,
            "source_node_id": self.source_node_id,
            "destination_node_id": self.destination_node_id,
            "phase": self.phase,
            "bytes_transferred": self.bytes_transferred,
            "total_bytes": self.total_bytes,
            "convergence_round": self.convergence_round,
            "dirty_blocks_remaining": self.dirty_blocks_remaining,
            "cancel_requested": self.cancel_requested_at.is_some(),
            "started_at": self.started_at,
            "updated_at": self.updated_at,
            "completed_at": self.completed_at,
            "error_message": self.error_message,
            "interrupted_from_phase": self.interrupted_from_phase,
            "interrupted_at": self.interrupted_at,
        })
    }
}

/// `GET /v1/migrations` — the most recent migration rows, newest first
/// (the `list_backup_jobs_rest` shape: `{items, total}`, bounded like
/// the other unpaginated list routes).
pub async fn list_migrations(
    crate::auth::BearerToken(_claims): crate::auth::BearerToken,
    State(state): State<AppState>,
) -> Result<Json<Value>, BffError> {
    let rows = sqlx::query_as::<_, MigrationRow>(
        r#"
        SELECT migration_id, operation_id, vm_id, source_node_id, destination_node_id,
               phase, bytes_transferred, total_bytes, convergence_round,
               dirty_blocks_remaining, started_at, updated_at, completed_at,
               error_message, cancel_requested_at, interrupted_from_phase, interrupted_at
        FROM migrations
        ORDER BY started_at DESC
        LIMIT 1000
        "#,
    )
    .fetch_all(&state.pool)
    .await
    .map_err(|e| BffError::Internal(format!("failed to list migrations: {}", e)))?;

    let total = rows.len() as u64;
    let items: Vec<Value> = rows.iter().map(MigrationRow::to_json).collect();

    Ok(Json(json!({
        "items": items,
        "total": total,
    })))
}

/// `GET /v1/migrations/{migration_id}` — one row, flat (the
/// `get_backup_job` shape); an unknown id is a 404 like the other
/// per-id read routes.
pub async fn get_migration(
    crate::auth::BearerToken(_claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    Path(migration_id): Path<String>,
) -> Result<Json<Value>, BffError> {
    let row = sqlx::query_as::<_, MigrationRow>(
        r#"
        SELECT migration_id, operation_id, vm_id, source_node_id, destination_node_id,
               phase, bytes_transferred, total_bytes, convergence_round,
               dirty_blocks_remaining, started_at, updated_at, completed_at,
               error_message, cancel_requested_at, interrupted_from_phase, interrupted_at
        FROM migrations
        WHERE migration_id = ?
        "#,
    )
    .bind(&migration_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| BffError::Internal(format!("failed to get migration: {}", e)))?;

    match row {
        Some(r) => Ok(Json(r.to_json())),
        None => Err(BffError::NotFound(format!(
            "migration {} not found",
            migration_id
        ))),
    }
}
