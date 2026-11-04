use axum::{
    extract::{Extension, Multipart, State},
    response::Json,
};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

use crate::router::AppState;
use crate::BffError;

const QCOW2_MAGIC: &[u8] = b"QFI\xfb";
const MAX_FILE_SIZE: u64 = 100 * 1024 * 1024 * 1024; // 100 GiB

pub async fn import_vm(
    crate::auth::BearerToken(claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    Extension(correlation_id): Extension<Option<String>>,
    mut multipart: Multipart,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_operator_or_admin(&claims)?;

    let mut name: Option<String> = None;
    let mut tmp_path: Option<std::path::PathBuf> = None;
    let mut header_buf = Vec::new();
    let mut total_size: u64 = 0;

    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| BffError::BadRequest(format!("multipart error: {}", e)))?
    {
        let field_name = field.name().unwrap_or("").to_string();

        if field_name == "name" {
            let value = field
                .text()
                .await
                .map_err(|e| BffError::BadRequest(format!("invalid name: {}", e)))?;
            // Reject names that could traverse or pollute filesystem paths
            // (the name becomes vms.display_name and a volume display name).
            if !super::vms::is_valid_display_name(&value) {
                if let Some(ref p) = tmp_path {
                    let _ = tokio::fs::remove_file(p).await;
                }
                return Err(BffError::BadRequest(
                    "name must match ^[A-Za-z0-9 ._-]{1,64}$".into(),
                ));
            }
            name = Some(value);
            continue;
        }

        if field_name != "file" {
            continue;
        }

        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|e| BffError::BadRequest(format!("chunk error: {}", e)))?
        {
            total_size += chunk.len() as u64;
            if total_size > MAX_FILE_SIZE {
                if let Some(ref p) = tmp_path {
                    let _ = tokio::fs::remove_file(p).await;
                }
                return Err(BffError::BadRequest(
                    "file exceeds maximum size of 100 GiB".into(),
                ));
            }

            if tmp_path.is_none() {
                header_buf.extend_from_slice(&chunk);
                if header_buf.len() >= 4 {
                    if &header_buf[0..4] != QCOW2_MAGIC {
                        return Err(BffError::BadRequest(
                            "file is not a valid qcow2 image".into(),
                        ));
                    }
                    let tmp_file = state
                        .agent_runtime_dir
                        .join(format!(".import-tmp-{}", chv_common::gen_resource_id()));
                    let mut f = tokio::fs::File::create(&tmp_file).await.map_err(|e| {
                        BffError::Internal(format!("failed to create temp file: {}", e))
                    })?;
                    tmp_path = Some(tmp_file);

                    f.write_all(&header_buf).await.map_err(|e| {
                        BffError::Internal(format!("failed to write temp file: {}", e))
                    })?;
                    header_buf.clear();
                }
            } else if let Some(ref p) = tmp_path {
                let mut f = tokio::fs::OpenOptions::new()
                    .append(true)
                    .open(p)
                    .await
                    .map_err(|e| BffError::Internal(format!("failed to open temp file: {}", e)))?;
                f.write_all(&chunk)
                    .await
                    .map_err(|e| BffError::Internal(format!("failed to write temp file: {}", e)))?;
            }
        }
    }

    let name = match name {
        Some(n) => n,
        None => {
            if let Some(ref p) = tmp_path {
                let _ = tokio::fs::remove_file(p).await;
            }
            return Err(BffError::BadRequest("missing name".into()));
        }
    };

    let tmp_file = match tmp_path {
        Some(p) => p,
        None => {
            return Err(BffError::BadRequest(
                "missing file or file too small to be qcow2".into(),
            ));
        }
    };

    let vm_id = chv_common::gen_resource_id();
    let vm_dir = state.agent_runtime_dir.join("vms").join(&vm_id);
    let disk_path = vm_dir.join("disk.qcow2");

    // Move temp file to final location
    if let Err(e) = tokio::fs::create_dir_all(&vm_dir).await {
        let _ = tokio::fs::remove_file(&tmp_file).await;
        return Err(BffError::Internal(format!(
            "failed to create vm dir: {}",
            e
        )));
    }
    if let Err(e) = tokio::fs::rename(&tmp_file, &disk_path).await {
        let _ = tokio::fs::remove_dir_all(&vm_dir).await;
        let _ = tokio::fs::remove_file(&tmp_file).await;
        return Err(BffError::Internal(format!(
            "failed to move disk file: {}",
            e
        )));
    }

    let file_size = tokio::fs::metadata(&disk_path)
        .await
        .map_err(|e| BffError::Internal(format!("failed to get file metadata: {}", e)))?
        .len() as i64;

    // Pick a healthy node (health_status is on node_observed_state, not nodes)
    let node_id = sqlx::query_scalar::<_, String>(
        "SELECT n.node_id FROM nodes n JOIN node_observed_state nos ON n.node_id = nos.node_id WHERE nos.health_status = 'healthy' ORDER BY n.enrolled_at DESC LIMIT 1",
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| BffError::Internal(format!("failed to query nodes: {}", e)))?
    .ok_or_else(|| BffError::BadRequest("no healthy nodes available".into()))?;

    let volume_id = chv_common::gen_resource_id();
    let operation_id = correlation_id.unwrap_or_else(chv_common::gen_resource_id);

    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|e| BffError::Internal(format!("failed to begin transaction: {}", e)))?;

    let disk_path_str = disk_path.to_string_lossy().to_string();

    // Insert VM
    let insert_vm = sqlx::query(
        r#"
        INSERT INTO vms (vm_id, node_id, display_name, owner_id, created_at, updated_at)
        VALUES (?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'))
        "#,
    )
    .bind(&vm_id)
    .bind(&node_id)
    .bind(&name)
    .bind(&claims.sub)
    .execute(&mut *tx)
    .await;

    if let Err(e) = insert_vm {
        let _ = tokio::fs::remove_dir_all(&vm_dir).await;
        return Err(BffError::Internal(format!("failed to insert vm: {}", e)));
    }

    // Insert VM desired state
    let insert_vds = sqlx::query(
        r#"
        INSERT INTO vm_desired_state (
            vm_id, desired_generation, desired_status, desired_power_state,
            requested_by, target_node_id, cpu_count, memory_bytes, image_ref,
            requested_at, updated_at
        )
        VALUES (?, 1, 'Pending', 'Running', ?, ?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'))
        "#,
    )
    .bind(&vm_id)
    .bind(&claims.sub)
    .bind(&node_id)
    .bind(1i64)
    .bind(512i64 * 1024i64 * 1024i64)
    .bind(&disk_path_str)
    .execute(&mut *tx)
    .await;

    if let Err(e) = insert_vds {
        let _ = tokio::fs::remove_dir_all(&vm_dir).await;
        return Err(BffError::Internal(format!(
            "failed to insert vm_desired_state: {}",
            e
        )));
    }

    // Insert volume
    let insert_vol = sqlx::query(
        r#"
        INSERT INTO volumes (volume_id, node_id, display_name, owner_id, capacity_bytes, updated_at)
        VALUES (?, ?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%SZ','now'))
        "#,
    )
    .bind(&volume_id)
    .bind(&node_id)
    .bind(format!("{}-disk", name))
    .bind(&claims.sub)
    .bind(file_size)
    .execute(&mut *tx)
    .await;

    if let Err(e) = insert_vol {
        let _ = tokio::fs::remove_dir_all(&vm_dir).await;
        return Err(BffError::Internal(format!(
            "failed to insert volume: {}",
            e
        )));
    }

    // Insert volume desired state
    let insert_volds = sqlx::query(
        r#"
        INSERT INTO volume_desired_state (
            volume_id, desired_generation, desired_status, requested_by,
            attached_vm_id, device_name, read_only, requested_at, updated_at
        )
        VALUES (?, 1, 'Pending', ?, ?, 'vda', 0, strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'))
        "#,
    )
    .bind(&volume_id)
    .bind(&claims.sub)
    .bind(&vm_id)
    .execute(&mut *tx)
    .await;

    if let Err(e) = insert_volds {
        let _ = tokio::fs::remove_dir_all(&vm_dir).await;
        return Err(BffError::Internal(format!(
            "failed to insert volume_desired_state: {}",
            e
        )));
    }

    // Insert operation
    let idempotency_key = format!("import-vm-{}", vm_id);
    let insert_op = sqlx::query(
        r#"
        INSERT INTO operations (
            operation_id, idempotency_key, resource_kind, resource_id,
            operation_type, status, requested_by, desired_generation,
            requested_at, created_at, updated_at
        )
        VALUES (?, ?, 'vm', ?, 'CreateVm', 'Accepted', ?, 1, strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'))
        "#,
    )
    .bind(&operation_id)
    .bind(&idempotency_key)
    .bind(&vm_id)
    .bind(&claims.sub)
    .execute(&mut *tx)
    .await;

    if let Err(e) = insert_op {
        let _ = tokio::fs::remove_dir_all(&vm_dir).await;
        return Err(BffError::Internal(format!(
            "failed to insert operation: {}",
            e
        )));
    }

    tx.commit()
        .await
        .map_err(|e| BffError::Internal(format!("failed to commit transaction: {}", e)))?;
    state.cache.invalidate("vms:").await;
    state.cache.invalidate("volumes:").await;
    state.cache.invalidate("overview").await;

    Ok(Json(json!({
        "id": vm_id,
        "name": name,
        "image_id": "imported",
        "storage_pool_id": "",
        "network_id": "",
        "desired_state": "Pending",
        "actual_state": "Unknown",
        "vcpu": 1,
        "memory_mb": 512,
        "disk_path": disk_path_str,
        "seed_iso_path": "",
        "workspace_path": "",
    })))
}

#[cfg(test)]
mod tests {
    use super::QCOW2_MAGIC;

    #[test]
    fn qcow2_magic_matches() {
        let header = b"QFI\xfb\x00\x00\x00\x03";
        assert_eq!(&header[0..4], QCOW2_MAGIC);
    }

    #[test]
    fn non_qcow2_rejected() {
        let header = b"\x00\x00\x00\x00";
        assert_ne!(&header[0..4], QCOW2_MAGIC);
    }

    #[test]
    fn raw_img_rejected() {
        let header = b"\x7fELF";
        assert_ne!(&header[0..4], QCOW2_MAGIC);
    }
}
