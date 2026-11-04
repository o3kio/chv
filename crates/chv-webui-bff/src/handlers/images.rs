use axum::{extract::State, response::Json};
use serde_json::{json, Value};

use crate::router::AppState;
use crate::BffError;

pub async fn list_images(
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
    let total_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM images WHERE status != 'deleted'")
            .fetch_one(&state.pool)
            .await
            .map_err(|e| BffError::Internal(format!("failed to count images: {}", e)))?;
    let total_pages = (total_count as u64).div_ceil(page_size);

    let rows = sqlx::query_as::<_, ImageRow>(
        r#"
        SELECT
            image_id,
            display_name AS name,
            CASE WHEN size_bytes IS NULL THEN ''
                 WHEN size_bytes >= 1073741824 THEN printf('%.1f GiB', CAST(size_bytes AS REAL)/1073741824.0)
                 WHEN size_bytes >= 1048576 THEN printf('%.1f MiB', CAST(size_bytes AS REAL)/1048576.0)
                 WHEN size_bytes >= 1024 THEN printf('%.1f KiB', CAST(size_bytes AS REAL)/1024.0)
                 ELSE printf('%d B', size_bytes) END AS size,
            status,
            COALESCE(os, '') AS os,
            COALESCE(version, '') AS version,
            usage_count,
            updated_at AS last_updated
        FROM images
        WHERE status != 'deleted'
        ORDER BY updated_at DESC
        LIMIT ? OFFSET ?
        "#,
    )
    .bind(page_size as i64)
    .bind(offset as i64)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| BffError::Internal(format!("failed to list images: {}", e)))?;

    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "image_id": r.image_id,
                "name": r.name,
                "size": r.size,
                "status": r.status,
                "os": r.os,
                "version": r.version,
                "usage_count": r.usage_count,
                "last_updated": r.last_updated,
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

/// Canonical form for image source URLs: `file://` URIs collapse to
/// bare absolute paths (the form the create-time resolution and the
/// duplicate check compare), with redundant leading slashes removed
/// (`file:////var/x` would otherwise store the POSIX
/// implementation-defined `//var/x`). Genuinely remote URLs pass
/// through unchanged.
pub(crate) fn normalize_source_url(source_url: &str) -> String {
    let stripped = source_url.strip_prefix("file://").unwrap_or(source_url);
    if let Some(rest) = stripped.strip_prefix('/') {
        format!("/{}", rest.trim_start_matches('/'))
    } else {
        stripped.to_string()
    }
}

pub async fn import_image(
    crate::auth::BearerToken(claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    axum::Json(payload): axum::Json<Value>,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_operator_or_admin(&claims)?;
    let name = payload
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing name".into()))?;

    let source_url = normalize_source_url(
        payload
            .get("source_url")
            .and_then(|v| v.as_str())
            // "url" alias: chvctl sends both keys (older builds sent only
            // "url", which was silently dropped — kubedoio/chv#339).
            .or_else(|| payload.get("url").and_then(|v| v.as_str()))
            .unwrap_or(""),
    );

    let format = payload
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("qcow2");

    let os = payload.get("os").and_then(|v| v.as_str()).unwrap_or("");

    let architecture = payload
        .get("architecture")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let checksum = payload.get("checksum").and_then(|v| v.as_str());

    // Check for duplicate by source_url — on the canonical form, so
    // `file:///x.img` and `/x.img` (the same physical file) cannot
    // import as two rows whose name collision then resolves silently.
    if !source_url.is_empty() {
        let existing: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM images WHERE source_url = ?")
            .bind(&source_url)
            .fetch_one(&state.pool)
            .await
            .map_err(|e| BffError::Internal(format!("failed to check existing image: {}", e)))?;

        if existing > 0 {
            return Err(BffError::Conflict(
                "An image with this source URL already exists".into(),
            ));
        }
    }

    let image_id = chv_common::gen_resource_id();

    sqlx::query(
        r#"INSERT INTO images
           (image_id, display_name, image_type, format, size_bytes, checksum, source_url, os, version, status, node_id, created_at, updated_at)
           VALUES (?, ?, 'disk', ?, NULL, ?, ?, ?, ?, 'available', NULL, datetime('now'), datetime('now'))"#,
    )
    .bind(&image_id)
    .bind(name)
    .bind(format)
    .bind(checksum)
    .bind(&source_url)
    .bind(os)
    .bind(architecture)
    .execute(&state.pool)
    .await
    .map_err(|e| BffError::Internal(format!("failed to insert image: {}", e)))?;
    state.cache.invalidate("overview").await;

    Ok(Json(json!({
        "image_id": image_id,
        "name": name,
        "source_url": source_url,
        "format": format,
        "status": "available",
    })))
}

pub async fn delete_image(
    crate::auth::BearerToken(claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    axum::Json(payload): axum::Json<Value>,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_operator_or_admin(&claims)?;
    let image_id = payload
        .get("image_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing image_id".into()))?;

    // Check if image exists
    let exists: bool = sqlx::query_scalar(
        "SELECT COUNT(*) > 0 FROM images WHERE image_id = ? AND status != 'deleted'",
    )
    .bind(image_id)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| BffError::Internal(format!("db error: {}", e)))?;

    if !exists {
        return Err(BffError::NotFound(format!("image {} not found", image_id)));
    }

    // Check usage_count
    let usage: i64 = sqlx::query_scalar("SELECT usage_count FROM images WHERE image_id = ?")
        .bind(image_id)
        .fetch_one(&state.pool)
        .await
        .map_err(|e| BffError::Internal(format!("db error: {}", e)))?;

    // Soft delete
    sqlx::query("UPDATE images SET status = 'deleted', updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE image_id = ?")
        .bind(image_id)
        .execute(&state.pool)
        .await
        .map_err(|e| BffError::Internal(format!("failed to delete image: {}", e)))?;
    state.cache.invalidate("overview").await;

    Ok(Json(json!({
        "deleted": true,
        "image_id": image_id,
        "was_in_use": usage > 0,
        "usage_count": usage,
    })))
}

#[derive(sqlx::FromRow)]
struct ImageRow {
    image_id: String,
    name: String,
    size: String,
    status: String,
    os: String,
    version: String,
    usage_count: i64,
    last_updated: String,
}
