use axum::{
    extract::{Path, State},
    response::Json,
};
use serde_json::{json, Value};

use crate::router::AppState;
use crate::BffError;
use chrono::Utc;
use chv_controlplane_store::{
    BackupJobCreateInput, BackupJobUpdateInput, BackupRestoreCreateInput,
    BackupScheduleCreateInput, BackupScheduleUpdateInput,
};

/// Stable reason returned by every backup/restore route while the posture is
/// fenced (#654).
///
/// CHV has no tenant-data backup execution engine: `execute_job` fails, the
/// off-host ship path was removed, and there is no restore validation. The
/// pre-production gate therefore chose **fence**, not implement: an API that
/// promises an engine it cannot run is worse than an explicit 501.
///
/// `docs/DEPLOYMENT-ARCHITECTURE.md` §7 and `docs/GAP_ANALYSIS.md` carry the
/// operator-facing statement of responsibility. Do not remove this fence
/// without an end-to-end create → ship → restore → verify test.
pub const BACKUP_NOT_IMPLEMENTED: &str =
    "backup and restore are not implemented in this release; tenant-data backup is the \
     operator's responsibility outside CHV (see docs/DEPLOYMENT-ARCHITECTURE.md §7)";

/// Router-level fence for every `/v1/backups/**`, `/v1/backup-jobs`,
/// `/v1/backup-history`, and legacy `/api/v1/backup*` route.
///
/// Returning 501 (not 404) is deliberate: the surface exists and callers get a
/// deterministic `NOT_IMPLEMENTED` code instead of guessing whether the route
/// moved. Authentication and CSRF middleware still run first, so fencing never
/// creates an unauthenticated path.
pub async fn backup_fence() -> Result<Json<Value>, BffError> {
    Err(BffError::NotImplemented(BACKUP_NOT_IMPLEMENTED.to_string()))
}

// ─────────────────────────────────────────────────────────────────────────────
// Legacy /v1/backup-jobs and /v1/backup-history (POST with page payload)
// ─────────────────────────────────────────────────────────────────────────────

pub async fn list_backup_jobs(
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
    let offset = (page - 1) * page_size;

    let (rows, total_count) = state
        .backup_repo
        .list_jobs(page_size as i64, offset as i64)
        .await
        .map_err(|e| BffError::Internal(format!("failed to list backup jobs: {}", e)))?;

    let total_pages = (total_count as u64).div_ceil(page_size);

    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "job_id": r.job_id,
                "vm_id": r.vm_id,
                "volume_id": r.volume_id,
                "status": r.status,
                "backup_type": r.backup_type,
                "target_path": r.target_path,
                "storage_backend": r.storage_backend,
                "created_at": r.created_at,
                "started_at": r.started_at,
                "completed_at": r.completed_at,
                "error_message": r.error_message,
                "size_bytes": r.size_bytes,
                "checksum": r.checksum,
                "checksum_algorithm": r.checksum_algorithm,
            })
        })
        .collect();

    Ok(Json(json!({
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
    })))
}

pub async fn list_backup_history(
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
    let offset = (page - 1) * page_size;

    let job_id = payload.get("job_id").and_then(|v| v.as_str());

    let (rows, total_count) = if let Some(job_id) = job_id {
        state
            .backup_repo
            .list_jobs_for_vm(job_id, page_size as i64, offset as i64)
            .await
            .map_err(|e| BffError::Internal(format!("failed to list backup history: {}", e)))?
    } else {
        state
            .backup_repo
            .list_jobs(page_size as i64, offset as i64)
            .await
            .map_err(|e| BffError::Internal(format!("failed to list backup history: {}", e)))?
    };

    let total_pages = (total_count as u64).div_ceil(page_size);

    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "job_id": r.job_id,
                "vm_id": r.vm_id,
                "volume_id": r.volume_id,
                "started_at": r.started_at,
                "completed_at": r.completed_at,
                "status": r.status,
                "size_bytes": r.size_bytes,
                "error_message": r.error_message,
                "checksum": r.checksum,
                "checksum_algorithm": r.checksum_algorithm,
                "storage_backend": r.storage_backend,
                "created_at": r.created_at,
            })
        })
        .collect();

    Ok(Json(json!({
        "items": items,
        "page": {
            "page": page,
            "page_size": page_size,
            "total_pages": total_pages,
            "total_items": total_count,
        },
        "filters": {
            "applied": {}
        },
    })))
}

// ─────────────────────────────────────────────────────────────────────────────
// REST-style backup jobs (/v1/backups/jobs)
// ─────────────────────────────────────────────────────────────────────────────

pub async fn create_backup_job(
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

    let volume_id = payload
        .get("volume_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let backup_type = payload
        .get("backup_type")
        .and_then(|v| v.as_str())
        .unwrap_or("full")
        .to_string();
    let target_path = payload
        .get("target_path")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let storage_backend = payload
        .get("storage_backend")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let schedule_id = payload
        .get("schedule_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let input = BackupJobCreateInput {
        vm_id,
        volume_id,
        schedule_id,
        status: "Pending".into(),
        backup_type,
        target_path: target_path.clone(),
        storage_backend,
        started_at: None,
        completed_at: None,
        error_message: None,
        size_bytes: None,
        checksum: None,
        checksum_algorithm: None,
        destination: target_path.clone(),
    };

    let job_id = state
        .backup_repo
        .create_job(&input)
        .await
        .map_err(|e| BffError::Internal(format!("failed to create backup job: {}", e)))?;
    state.cache.invalidate("overview").await;

    Ok(Json(json!({
        "accepted": true,
        "task_id": job_id,
        "job_id": job_id,
        "summary": format!("Creating backup job for VM '{}'", input.vm_id),
        "next_refresh_path": format!("/api/v1/backups/jobs/{}", job_id),
    })))
}

pub async fn list_backup_jobs_rest(
    crate::auth::BearerToken(_claims): crate::auth::BearerToken,
    State(state): State<AppState>,
) -> Result<Json<Value>, BffError> {
    let (rows, total_count) = state
        .backup_repo
        .list_jobs(1000, 0)
        .await
        .map_err(|e| BffError::Internal(format!("failed to list backup jobs: {}", e)))?;

    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "job_id": r.job_id,
                "vm_id": r.vm_id,
                "volume_id": r.volume_id,
                "status": r.status,
                "backup_type": r.backup_type,
                "target_path": r.target_path,
                "storage_backend": r.storage_backend,
                "created_at": r.created_at,
                "started_at": r.started_at,
                "completed_at": r.completed_at,
                "error_message": r.error_message,
                "size_bytes": r.size_bytes,
                "checksum": r.checksum,
                "checksum_algorithm": r.checksum_algorithm,
            })
        })
        .collect();

    Ok(Json(json!({
        "items": items,
        "total": total_count,
    })))
}

pub async fn get_backup_job(
    crate::auth::BearerToken(_claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<Value>, BffError> {
    let row = state
        .backup_repo
        .get_job(&job_id)
        .await
        .map_err(|e| BffError::Internal(format!("failed to get backup job: {}", e)))?;

    match row {
        Some(r) => Ok(Json(json!({
            "job_id": r.job_id,
            "vm_id": r.vm_id,
            "volume_id": r.volume_id,
            "status": r.status,
            "backup_type": r.backup_type,
            "target_path": r.target_path,
            "storage_backend": r.storage_backend,
            "created_at": r.created_at,
            "started_at": r.started_at,
            "completed_at": r.completed_at,
            "error_message": r.error_message,
            "size_bytes": r.size_bytes,
            "checksum": r.checksum,
            "checksum_algorithm": r.checksum_algorithm,
        }))),
        None => Err(BffError::NotFound(format!(
            "backup job {} not found",
            job_id
        ))),
    }
}

pub async fn delete_backup_job(
    crate::auth::BearerToken(claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_operator_or_admin(&claims)?;
    state
        .backup_repo
        .delete_job(&job_id)
        .await
        .map_err(|e| BffError::Internal(format!("failed to delete backup job: {}", e)))?;
    state.cache.invalidate("overview").await;

    Ok(Json(json!({ "deleted": true })))
}

pub async fn update_backup_job(
    crate::auth::BearerToken(claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    Path(job_id): Path<String>,
    axum::Json(payload): axum::Json<Value>,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_operator_or_admin(&claims)?;
    let row = state
        .backup_repo
        .get_job(&job_id)
        .await
        .map_err(|e| BffError::Internal(format!("failed to get backup job: {}", e)))?
        .ok_or_else(|| BffError::NotFound(format!("backup job {} not found", job_id)))?;

    let volume_id = payload
        .get("volume_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or(row.volume_id);
    let status = payload
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or(&row.status)
        .to_string();
    let backup_type = payload
        .get("backup_type")
        .and_then(|v| v.as_str())
        .unwrap_or(&row.backup_type)
        .to_string();
    let target_path = payload
        .get("target_path")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or(row.target_path);
    let storage_backend = payload
        .get("storage_backend")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or(row.storage_backend);
    let started_at = payload
        .get("started_at")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or(row.started_at);
    let completed_at = payload
        .get("completed_at")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or(row.completed_at);
    let error_message = payload
        .get("error_message")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or(row.error_message);
    let size_bytes = payload
        .get("size_bytes")
        .and_then(|v| v.as_i64())
        .or(row.size_bytes);

    let input = BackupJobUpdateInput {
        job_id,
        volume_id,
        status,
        backup_type,
        target_path,
        storage_backend,
        started_at,
        completed_at,
        error_message,
        size_bytes,
        checksum: None,
        checksum_algorithm: None,
        destination: None,
    };

    state
        .backup_repo
        .update_job(&input)
        .await
        .map_err(|e| BffError::Internal(format!("failed to update backup job: {}", e)))?;
    state.cache.invalidate("overview").await;

    Ok(Json(json!({ "updated": true })))
}

pub async fn execute_backup_job(
    crate::auth::BearerToken(claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_operator_or_admin(&claims)?;
    let row = state
        .backup_repo
        .get_job(&job_id)
        .await
        .map_err(|e| BffError::Internal(format!("failed to get backup job: {}", e)))?
        .ok_or_else(|| BffError::NotFound(format!("backup job {} not found", job_id)))?;

    let input = BackupJobCreateInput {
        vm_id: row.vm_id,
        volume_id: row.volume_id,
        schedule_id: row.schedule_id,
        status: "Pending".into(),
        backup_type: row.backup_type,
        target_path: row.target_path.clone(),
        storage_backend: row.storage_backend,
        started_at: None,
        completed_at: None,
        error_message: None,
        size_bytes: None,
        checksum: None,
        checksum_algorithm: None,
        destination: row.destination.clone(),
    };

    let execution_id = state
        .backup_repo
        .create_job(&input)
        .await
        .map_err(|e| BffError::Internal(format!("failed to execute backup job: {}", e)))?;
    state.cache.invalidate("overview").await;

    Ok(Json(json!({
        "accepted": true,
        "task_id": execution_id,
        "job_id": job_id,
        "summary": format!("Executing backup job '{}'", job_id),
        "next_refresh_path": format!("/api/v1/backups/jobs/{}", execution_id),
    })))
}

// ─────────────────────────────────────────────────────────────────────────────
// Backup schedules (/v1/backups/schedules)
// ─────────────────────────────────────────────────────────────────────────────

pub async fn create_backup_schedule(
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

    let volume_id = payload
        .get("volume_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let name = payload
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("Backup")
        .to_string();
    let cron_expression = payload
        .get("cron_expression")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing cron_expression".into()))?
        .to_string();
    let retention_count = payload
        .get("retention_count")
        .and_then(|v| v.as_i64())
        .unwrap_or(7);
    let destination = payload
        .get("destination")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let retention_days = payload
        .get("retention_days")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let s3_access_key = payload
        .get("s3_access_key")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let s3_secret_key = payload
        .get("s3_secret_key")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let enabled = payload
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let input = BackupScheduleCreateInput {
        vm_id,
        volume_id,
        name,
        cron_expression,
        retention_count,
        retention_days,
        destination,
        enabled,
        s3_access_key,
        s3_secret_key,
    };

    let schedule_id = state
        .backup_repo
        .create_schedule(&input)
        .await
        .map_err(|e| BffError::Internal(format!("failed to create backup schedule: {}", e)))?;
    state.cache.invalidate("overview").await;

    Ok(Json(json!({
        "accepted": true,
        "task_id": null,
        "schedule_id": schedule_id,
        "summary": format!("Created backup schedule for VM '{}'", input.vm_id),
    })))
}

pub async fn list_backup_schedules(
    crate::auth::BearerToken(_claims): crate::auth::BearerToken,
    State(state): State<AppState>,
) -> Result<Json<Value>, BffError> {
    let (rows, total_count) = state
        .backup_repo
        .list_schedules(1000, 0)
        .await
        .map_err(|e| BffError::Internal(format!("failed to list backup schedules: {}", e)))?;

    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "schedule_id": r.schedule_id,
                "vm_id": r.vm_id,
                "volume_id": r.volume_id,
                "name": r.name,
                "cron_expression": r.cron_expression,
                "retention_count": r.retention_count,
                "retention_days": r.retention_days,
                "destination": r.destination,
                "enabled": r.enabled,
                "created_at": r.created_at,
                "updated_at": r.updated_at,
            })
        })
        .collect();

    Ok(Json(json!({
        "items": items,
        "total": total_count,
    })))
}

pub async fn get_backup_schedule(
    crate::auth::BearerToken(_claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    Path(schedule_id): Path<String>,
) -> Result<Json<Value>, BffError> {
    let row = state
        .backup_repo
        .get_schedule(&schedule_id)
        .await
        .map_err(|e| BffError::Internal(format!("failed to get backup schedule: {}", e)))?;

    match row {
        Some(r) => Ok(Json(json!({
            "schedule_id": r.schedule_id,
            "vm_id": r.vm_id,
            "volume_id": r.volume_id,
            "name": r.name,
            "cron_expression": r.cron_expression,
            "retention_count": r.retention_count,
            "retention_days": r.retention_days,
            "destination": r.destination,
            "enabled": r.enabled,
            "created_at": r.created_at,
            "updated_at": r.updated_at,
        }))),
        None => Err(BffError::NotFound(format!(
            "backup schedule {} not found",
            schedule_id
        ))),
    }
}

pub async fn update_backup_schedule(
    crate::auth::BearerToken(claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    Path(schedule_id): Path<String>,
    axum::Json(payload): axum::Json<Value>,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_operator_or_admin(&claims)?;
    // Fetch existing record to preserve unspecified fields (correct PATCH semantics)
    let existing = state
        .backup_repo
        .get_schedule(&schedule_id)
        .await
        .map_err(|e| BffError::Internal(format!("failed to get backup schedule: {}", e)))?
        .ok_or_else(|| BffError::NotFound(format!("backup schedule {} not found", schedule_id)))?;

    let vm_id = payload
        .get("vm_id")
        .and_then(|v| v.as_str())
        .unwrap_or(&existing.vm_id)
        .to_string();
    let volume_id = payload
        .get("volume_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or(existing.volume_id);
    let name = payload
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or(&existing.name)
        .to_string();
    let cron_expression = payload
        .get("cron_expression")
        .and_then(|v| v.as_str())
        .unwrap_or(&existing.cron_expression)
        .to_string();
    let retention_count = payload
        .get("retention_count")
        .and_then(|v| v.as_i64())
        .unwrap_or(existing.retention_count);
    let destination = payload
        .get("destination")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or(existing.destination);
    let retention_days = payload
        .get("retention_days")
        .and_then(|v| v.as_i64())
        .unwrap_or(existing.retention_days);
    let s3_access_key = payload
        .get("s3_access_key")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or(existing.s3_access_key);
    let s3_secret_key = payload
        .get("s3_secret_key")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or(existing.s3_secret_key);
    let enabled = payload
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(existing.enabled);

    let input = BackupScheduleUpdateInput {
        schedule_id,
        vm_id,
        volume_id,
        name,
        cron_expression,
        retention_count,
        retention_days,
        destination,
        enabled,
        s3_access_key,
        s3_secret_key,
    };

    state
        .backup_repo
        .update_schedule(&input)
        .await
        .map_err(|e| BffError::Internal(format!("failed to update backup schedule: {}", e)))?;
    state.cache.invalidate("overview").await;

    Ok(Json(json!({ "updated": true })))
}

pub async fn delete_backup_schedule(
    crate::auth::BearerToken(claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    Path(schedule_id): Path<String>,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_operator_or_admin(&claims)?;
    state
        .backup_repo
        .delete_schedule(&schedule_id)
        .await
        .map_err(|e| BffError::Internal(format!("failed to delete backup schedule: {}", e)))?;
    state.cache.invalidate("overview").await;

    Ok(Json(json!({ "deleted": true })))
}

// ─────────────────────────────────────────────────────────────────────────────
// Backup restores (/v1/backups/restores)
// ─────────────────────────────────────────────────────────────────────────────

pub async fn create_backup_restore(
    crate::auth::BearerToken(claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    axum::Json(payload): axum::Json<Value>,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_operator_or_admin(&claims)?;
    let backup_job_id = payload
        .get("backup_job_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing backup_job_id".into()))?
        .to_string();

    let target_vm_id = payload
        .get("target_vm_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let target_volume_id = payload
        .get("target_volume_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let input = BackupRestoreCreateInput {
        backup_job_id,
        target_vm_id,
        target_volume_id,
        status: "Pending".into(),
        started_at: None,
        completed_at: None,
        error_message: None,
        source_path: None,
        storage_backend: None,
    };

    let restore_id = state
        .backup_repo
        .create_restore(&input)
        .await
        .map_err(|e| BffError::Internal(format!("failed to create backup restore: {}", e)))?;
    state.cache.invalidate("overview").await;

    Ok(Json(json!({
        "accepted": true,
        "task_id": restore_id,
        "restore_id": restore_id,
        "summary": format!("Restoring backup job '{}'", input.backup_job_id),
        "next_refresh_path": format!("/api/v1/backups/restores/{}", restore_id),
    })))
}

pub async fn list_backup_restores(
    crate::auth::BearerToken(_claims): crate::auth::BearerToken,
    State(state): State<AppState>,
) -> Result<Json<Value>, BffError> {
    let (rows, total_count) = state
        .backup_repo
        .list_restores(1000, 0)
        .await
        .map_err(|e| BffError::Internal(format!("failed to list backup restores: {}", e)))?;

    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "restore_id": r.restore_id,
                "backup_job_id": r.backup_job_id,
                "target_vm_id": r.target_vm_id,
                "target_volume_id": r.target_volume_id,
                "status": r.status,
                "created_at": r.created_at,
                "started_at": r.started_at,
                "completed_at": r.completed_at,
                "error_message": r.error_message,
            })
        })
        .collect();

    Ok(Json(json!({
        "items": items,
        "total": total_count,
    })))
}

pub async fn get_backup_restore(
    crate::auth::BearerToken(_claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    Path(restore_id): Path<String>,
) -> Result<Json<Value>, BffError> {
    let row = state
        .backup_repo
        .get_restore(&restore_id)
        .await
        .map_err(|e| BffError::Internal(format!("failed to get backup restore: {}", e)))?;

    match row {
        Some(r) => Ok(Json(json!({
            "restore_id": r.restore_id,
            "backup_job_id": r.backup_job_id,
            "target_vm_id": r.target_vm_id,
            "target_volume_id": r.target_volume_id,
            "status": r.status,
            "created_at": r.created_at,
            "started_at": r.started_at,
            "completed_at": r.completed_at,
            "error_message": r.error_message,
        }))),
        None => Err(BffError::NotFound(format!(
            "backup restore {} not found",
            restore_id
        ))),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// API-compat handlers for /api/v1/backup-jobs (used by UI)
// These map the UI's "backup jobs" concept to backup_schedules.
// ─────────────────────────────────────────────────────────────────────────────

pub async fn list_backup_jobs_api(
    _claims: crate::auth::BearerToken,
    State(state): State<AppState>,
) -> Result<Json<Value>, BffError> {
    let (rows, _total) = state
        .backup_repo
        .list_schedules(1000, 0)
        .await
        .map_err(|e| BffError::Internal(format!("failed to list backup jobs: {}", e)))?;

    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.schedule_id,
                "job_id": r.schedule_id,
                "vm_id": r.vm_id,
                "name": r.name,
                "schedule": r.cron_expression,
                "retention": r.retention_count,
                "destination": r.destination,
                "enabled": r.enabled,
                "created_at": r.created_at,
                "updated_at": r.updated_at,
            })
        })
        .collect();

    Ok(Json(json!(items)))
}

pub async fn create_backup_job_api(
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

    let name = payload
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("Backup")
        .to_string();
    let schedule = payload
        .get("schedule")
        .and_then(|v| v.as_str())
        .unwrap_or("0 0 * * *")
        .to_string();
    let retention = payload
        .get("retention")
        .and_then(|v| v.as_i64())
        .unwrap_or(7);
    let destination = payload
        .get("destination")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let input = BackupScheduleCreateInput {
        vm_id,
        volume_id: None,
        name,
        cron_expression: schedule,
        retention_count: retention,
        retention_days: 0,
        destination,
        enabled: true,
        s3_access_key: None,
        s3_secret_key: None,
    };

    let schedule_id = state
        .backup_repo
        .create_schedule(&input)
        .await
        .map_err(|e| BffError::Internal(format!("failed to create backup job: {}", e)))?;
    state.cache.invalidate("overview").await;

    Ok(Json(json!({
        "id": schedule_id,
        "job_id": schedule_id,
        "vm_id": input.vm_id,
        "name": input.name,
        "schedule": input.cron_expression,
        "retention": input.retention_count,
        "destination": input.destination,
        "enabled": true,
        "created_at": Utc::now().to_rfc3339(),
    })))
}

pub async fn delete_backup_job_api(
    crate::auth::BearerToken(claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_operator_or_admin(&claims)?;
    state
        .backup_repo
        .delete_schedule(&job_id)
        .await
        .map_err(|e| BffError::Internal(format!("failed to delete backup job: {}", e)))?;
    state.cache.invalidate("overview").await;

    Ok(Json(json!({ "success": true })))
}

pub async fn list_backup_history_api(
    _claims: crate::auth::BearerToken,
    State(state): State<AppState>,
) -> Result<Json<Value>, BffError> {
    let (rows, _total) = state
        .backup_repo
        .list_jobs(1000, 0)
        .await
        .map_err(|e| BffError::Internal(format!("failed to list backup history: {}", e)))?;

    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.job_id,
                "job_id": r.job_id,
                "vm_id": r.vm_id,
                "snapshot_id": r.volume_id.as_deref().unwrap_or(""),
                "status": match r.status.as_str() {
                    "Succeeded" => "completed",
                    "Failed" => "failed",
                    "Running" => "running",
                    _ => "pending",
                },
                "size_bytes": r.size_bytes.unwrap_or(0),
                "started_at": r.started_at,
                "completed_at": r.completed_at,
                "error": r.error_message,
            })
        })
        .collect();

    Ok(Json(json!(items)))
}

pub async fn run_backup_job_api(
    crate::auth::BearerToken(claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_operator_or_admin(&claims)?;
    // "Run" a schedule by creating a backup job execution
    let schedule = state
        .backup_repo
        .get_schedule(&job_id)
        .await
        .map_err(|e| BffError::Internal(format!("failed to get schedule: {}", e)))?
        .ok_or_else(|| BffError::NotFound(format!("schedule {} not found", job_id)))?;

    let input = BackupJobCreateInput {
        vm_id: schedule.vm_id.clone(),
        volume_id: schedule.volume_id.clone(),
        schedule_id: Some(job_id),
        status: "Pending".into(),
        backup_type: "full".into(),
        target_path: schedule.destination.clone(),
        storage_backend: None,
        started_at: Some(Utc::now().to_rfc3339()),
        completed_at: None,
        error_message: None,
        size_bytes: None,
        checksum: None,
        checksum_algorithm: None,
        destination: schedule.destination.clone(),
    };

    let execution_id = state
        .backup_repo
        .create_job(&input)
        .await
        .map_err(|e| BffError::Internal(format!("failed to run backup job: {}", e)))?;
    state.cache.invalidate("overview").await;

    Ok(Json(json!({
        "id": execution_id,
        "job_id": execution_id,
        "status": "Running",
        "started_at": input.started_at,
    })))
}

pub async fn toggle_backup_job_api(
    crate::auth::BearerToken(claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_operator_or_admin(&claims)?;
    let schedule = state
        .backup_repo
        .get_schedule(&job_id)
        .await
        .map_err(|e| BffError::Internal(format!("failed to get schedule: {}", e)))?
        .ok_or_else(|| BffError::NotFound(format!("schedule {} not found", job_id)))?;

    let new_enabled = !schedule.enabled;
    let input = BackupScheduleUpdateInput {
        schedule_id: job_id,
        vm_id: schedule.vm_id,
        volume_id: schedule.volume_id,
        name: schedule.name,
        cron_expression: schedule.cron_expression,
        retention_count: schedule.retention_count,
        retention_days: schedule.retention_days,
        destination: schedule.destination,
        enabled: new_enabled,
        s3_access_key: schedule.s3_access_key,
        s3_secret_key: schedule.s3_secret_key,
    };

    state
        .backup_repo
        .update_schedule(&input)
        .await
        .map_err(|e| BffError::Internal(format!("failed to toggle backup job: {}", e)))?;
    state.cache.invalidate("overview").await;

    Ok(Json(json!({ "success": true, "enabled": new_enabled })))
}

pub async fn list_vm_backups_api(
    _claims: crate::auth::BearerToken,
    State(state): State<AppState>,
    Path(vm_id): Path<String>,
) -> Result<Json<Value>, BffError> {
    let (rows, _total) = state
        .backup_repo
        .list_jobs_for_vm(&vm_id, 1000, 0)
        .await
        .map_err(|e| BffError::Internal(format!("failed to list vm backups: {}", e)))?;

    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.job_id,
                "job_id": r.job_id,
                "vm_id": r.vm_id,
                "snapshot_id": r.volume_id.as_deref().unwrap_or(""),
                "status": match r.status.as_str() {
                    "Succeeded" => "completed",
                    "Failed" => "failed",
                    "Running" => "running",
                    _ => "pending",
                },
                "size_bytes": r.size_bytes.unwrap_or(0),
                "started_at": r.started_at,
                "completed_at": r.completed_at,
                "error": r.error_message,
            })
        })
        .collect();

    Ok(Json(json!(items)))
}
