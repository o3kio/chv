use axum::{
    extract::{Path, State},
    response::Json,
};
use serde_json::{json, Value};

use crate::router::AppState;
use crate::BffError;

#[derive(sqlx::FromRow)]
struct ExportRow {
    filename: String,
    export_path: String,
    status: String,
    vm_id: String,
}

#[derive(sqlx::FromRow)]
struct VmRow {
    display_name: String,
}

#[derive(sqlx::FromRow)]
struct VolumeRow {
    volume_id: String,
}

pub async fn export_vm(
    crate::auth::BearerToken(claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    Path(vm_id): Path<String>,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_operator_or_admin(&claims)?;

    if !chv_common::validate_id(&vm_id) {
        return Err(BffError::BadRequest("invalid vm_id format".into()));
    }

    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(|e| BffError::Internal(format!("db connection: {e}")))?;
    super::vms::require_vm_owner(&mut conn, &vm_id, &claims.sub, claims.role == "admin").await?;

    // Verify VM exists
    let vm = sqlx::query_as::<_, VmRow>("SELECT display_name FROM vms WHERE vm_id = ?")
        .bind(&vm_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| BffError::Internal(format!("failed to look up vm: {}", e)))?
        .ok_or_else(|| BffError::NotFound(format!("vm {} not found", vm_id)))?;

    // Never interpolate the DB-stored display_name into filesystem paths
    // verbatim: use it only when it passes the same charset validation as
    // create/import, otherwise fall back to the validated vm_id.
    let safe_name = if super::vms::is_valid_display_name(&vm.display_name) {
        vm.display_name.as_str()
    } else {
        vm_id.as_str()
    };

    // Find ALL attached volumes
    let volumes: Vec<VolumeRow> =
        sqlx::query_as("SELECT volume_id FROM volume_desired_state WHERE attached_vm_id = ?")
            .bind(&vm_id)
            .fetch_all(&state.pool)
            .await
            .map_err(|e| BffError::Internal(format!("failed to find volumes: {}", e)))?;

    if volumes.is_empty() {
        return Err(BffError::BadRequest("vm has no attached volumes".into()));
    }

    let vm_dir = state.agent_runtime_dir.join("vms").join(&vm_id);
    let expected_base = tokio::fs::canonicalize(&state.agent_runtime_dir.join("vms"))
        .await
        .unwrap_or_else(|_| state.agent_runtime_dir.join("vms"));

    // Validate all disk paths exist and are within the expected directory
    let mut disk_paths = Vec::new();
    for vol in &volumes {
        let disk_path = vm_dir.join(format!("{}.img", vol.volume_id));
        if !disk_path.exists() {
            return Err(BffError::NotFound(format!(
                "disk image not found for volume {}",
                vol.volume_id
            )));
        }
        let canonical = tokio::fs::canonicalize(&disk_path).await.map_err(|e| {
            BffError::Internal(format!(
                "failed to resolve disk path for {}: {}",
                vol.volume_id, e
            ))
        })?;
        if !canonical.starts_with(&expected_base) {
            return Err(BffError::BadRequest(
                "disk path is outside expected directory".into(),
            ));
        }
        disk_paths.push(canonical);
    }

    let export_id = chv_common::gen_resource_id();
    let export_dir = state.agent_runtime_dir.join("exports").join(&export_id);
    tokio::fs::create_dir_all(&export_dir)
        .await
        .map_err(|e| BffError::Internal(format!("failed to create export dir: {}", e)))?;

    // Copy all disks into the export directory with descriptive names
    for (i, disk_path) in disk_paths.iter().enumerate() {
        let vol = &volumes[i];
        let dest = export_dir.join(format!("{}-{}.img", safe_name, vol.volume_id));
        tokio::fs::copy(disk_path, dest)
            .await
            .map_err(|e| BffError::Internal(format!("failed to copy disk image: {}", e)))?;
    }

    // Create a tar archive of the export directory
    let filename = format!("{}-export.tar.gz", safe_name);
    let archive_path = export_dir.join(&filename);
    let tar_status = tokio::process::Command::new("tar")
        .arg("czf")
        .arg(&archive_path)
        .arg("-C")
        .arg(&export_dir)
        .arg(".")
        .status()
        .await
        .map_err(|e| BffError::Internal(format!("failed to run tar: {}", e)))?;

    if !tar_status.success() {
        let _ = tokio::fs::remove_dir_all(&export_dir).await;
        return Err(BffError::Internal("tar archive creation failed".into()));
    }

    // Insert export record as 'creating' first, then update to 'ready'
    sqlx::query(
        r#"
        INSERT INTO vm_exports (export_id, vm_id, filename, export_path, status)
        VALUES (?, ?, ?, ?, 'creating')
        "#,
    )
    .bind(&export_id)
    .bind(&vm_id)
    .bind(&filename)
    .bind(archive_path.to_string_lossy().to_string())
    .execute(&state.pool)
    .await
    .map_err(|e| BffError::Internal(format!("failed to insert export record: {}", e)))?;

    sqlx::query("UPDATE vm_exports SET status = 'ready' WHERE export_id = ?")
        .bind(&export_id)
        .execute(&state.pool)
        .await
        .map_err(|e| BffError::Internal(format!("failed to update export status: {}", e)))?;

    let download_url = format!("/api/v1/exports/{}/download", export_id);
    state.cache.invalidate("vms:").await;
    state.cache.invalidate("overview").await;

    Ok(Json(json!({
        "export_id": export_id,
        "filename": filename,
        "download_url": download_url,
    })))
}

pub async fn download_export(
    crate::auth::BearerToken(claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    Path(export_id): Path<String>,
) -> Result<axum::response::Response, BffError> {
    if !chv_common::validate_id(&export_id) {
        return Err(BffError::BadRequest("invalid export_id format".into()));
    }

    let row = sqlx::query_as::<_, ExportRow>(
        "SELECT filename, export_path, status, vm_id FROM vm_exports WHERE export_id = ?",
    )
    .bind(&export_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| BffError::Internal(format!("failed to look up export: {}", e)))?
    .ok_or_else(|| BffError::NotFound(format!("export {} not found", export_id)))?;

    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(|e| BffError::Internal(format!("db connection: {e}")))?;
    super::vms::require_vm_owner(&mut conn, &row.vm_id, &claims.sub, claims.role == "admin")
        .await?;

    if row.status != "ready" {
        return Err(BffError::BadRequest("export is not ready".into()));
    }

    let file = tokio::fs::File::open(&row.export_path)
        .await
        .map_err(|e| BffError::Internal(format!("failed to open export file: {}", e)))?;

    let stream = tokio_util::io::ReaderStream::new(file);
    let body = axum::body::Body::from_stream(stream);

    let response = axum::response::Response::builder()
        .header("Content-Type", "application/gzip")
        .header(
            "Content-Disposition",
            format!("attachment; filename=\"{}\"", row.filename),
        )
        .body(body)
        .map_err(|e| BffError::Internal(format!("failed to build response: {}", e)))?;

    Ok(response)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    #[test]
    fn path_traversal_detected() {
        let base = PathBuf::from("/var/lib/chv/vms");
        // Note: PathBuf::starts_with compares components, so canonicalize first
        // as the production code does. Here we simulate the canonicalized result
        // of resolving "/var/lib/chv/vms/../etc/passwd".
        let canonical_bad = PathBuf::from("/var/lib/chv/etc/passwd");
        assert!(!canonical_bad.starts_with(&base));
    }

    #[test]
    fn valid_path_inside_base() {
        let base = PathBuf::from("/var/lib/chv/vms");
        let good = PathBuf::from("/var/lib/chv/vms/vm-123/disk.img");
        assert!(good.starts_with(&base));
    }
}
