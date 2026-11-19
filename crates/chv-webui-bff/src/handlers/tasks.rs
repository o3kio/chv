use axum::response::sse::{Event, Sse};
use axum::{extract::State, response::Json};
use serde_json::{json, Value};
use std::convert::Infallible;
use std::time::Duration;
use tokio::time::interval;
use tokio_stream::wrappers::IntervalStream;
use tokio_stream::StreamExt;

use crate::router::AppState;
use crate::BffError;

pub async fn list_tasks(
    crate::auth::BearerToken(_claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    axum::Json(req): axum::Json<Value>,
) -> Result<Json<Value>, BffError> {
    // Clamp to sane bounds BEFORE narrowing to u32: a page >= 2^32 used to
    // truncate to 0 and underflow the offset computation, and a page_size
    // >= 2^32 used to truncate to 0 and make div_ceil below panic.
    let page = req
        .get("page")
        .and_then(|v| v.as_u64())
        .map(|p| p.max(1))
        .unwrap_or(1)
        .min(u32::MAX as u64);

    let page_size = req
        .get("page_size")
        .and_then(|v| v.as_u64())
        .map(|p| if p == 0 { 50 } else { p })
        .unwrap_or(50)
        .clamp(1, 200);

    let filters = req.get("filters").cloned().unwrap_or(json!({}));

    let mut query_sql = String::from(
        r#"
        SELECT
            operation_id AS task_id,
            status,
            operation_type AS operation,
            resource_kind,
            resource_id,
            requested_by AS actor,
            CAST(strftime('%s', requested_at) AS INTEGER) * 1000 AS started_unix_ms,
            CAST(strftime('%s', completed_at) AS INTEGER) * 1000 AS finished_unix_ms,
            error_code,
            error_message
        FROM operations
        WHERE 1=1
        "#,
    );

    let mut bindings: Vec<String> = Vec::new();

    if let Some(status) = filters.get("status").and_then(|v| v.as_str()) {
        if status != "all" {
            bindings.push(status.to_string());
            query_sql.push_str(&format!(" AND status = ${}", bindings.len()));
        }
    }

    if let Some(resource_kind) = filters.get("resource_kind").and_then(|v| v.as_str()) {
        if resource_kind != "all" {
            bindings.push(resource_kind.to_string());
            query_sql.push_str(&format!(" AND resource_kind = ${}", bindings.len()));
        }
    }

    if let Some(window) = filters.get("window").and_then(|v| v.as_str()) {
        match window {
            "24h" => query_sql
                .push_str(" AND requested_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-24 hours')"),
            "7d" => query_sql
                .push_str(" AND requested_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-7 days')"),
            "30d" => query_sql
                .push_str(" AND requested_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-30 days')"),
            "active" => query_sql.push_str(
                " AND status IN ('Pending', 'Accepted', 'Running', 'AwaitingOperatorInput')",
            ),
            _ => {}
        }
    }

    query_sql.push_str(" ORDER BY requested_at DESC");
    query_sql.push_str(" LIMIT ? OFFSET ?");

    let offset = (page - 1) * page_size;

    let count_sql = query_sql
        .replace("SELECT\n            operation_id AS task_id,\n            status,\n            operation_type AS operation,\n            resource_kind,\n            resource_id,\n            requested_by AS actor,\n            CAST(strftime('%s', requested_at) AS INTEGER) * 1000 AS started_unix_ms,\n            CAST(strftime('%s', completed_at) AS INTEGER) * 1000 AS finished_unix_ms,\n            error_code,\n            error_message",
                 "SELECT COUNT(*)")
        .replace(" ORDER BY requested_at DESC LIMIT ? OFFSET ?", "");

    let mut count_query = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(count_sql.as_str()));
    for b in &bindings {
        count_query = count_query.bind(b.clone());
    }
    let total_count = count_query
        .fetch_one(&state.pool)
        .await
        .map_err(|e| BffError::Internal(format!("failed to count tasks: {}", e)))?;

    let mut query = sqlx::query_as::<_, TaskRow>(sqlx::AssertSqlSafe(query_sql.as_str()));
    for b in bindings {
        query = query.bind(b);
    }
    query = query.bind(page_size as i64).bind(offset as i64);

    let rows = query
        .fetch_all(&state.pool)
        .await
        .map_err(|e| BffError::Internal(format!("failed to list tasks: {}", e)))?;

    let total = total_count as u64;
    let total_pages = total.div_ceil(page_size);

    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "task_id": r.task_id,
                "status": r.status,
                "operation": r.operation,
                "resource_kind": r.resource_kind,
                "resource_id": r.resource_id.unwrap_or_default(),
                "actor": r.actor.unwrap_or_default(),
                "started_unix_ms": r.started_unix_ms,
                "finished_unix_ms": r.finished_unix_ms,
                // #502: the terminal-failure cause the fast-fail work
                // (#498/#500) journals — NULL until a failure records
                // it, passed through verbatim (never fabricated, never
                // a generic placeholder).
                "error_code": r.error_code,
                "error_message": r.error_message,
            })
        })
        .collect();

    Ok(Json(json!({
        "items": items,
        "page": {
            "page": page,
            "page_size": page_size,
            "total_items": total,
            "total_pages": total_pages,
        },
        "filters": {
            "applied": {}
        },
    })))
}

pub async fn get_task(
    crate::auth::BearerToken(_claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    axum::Json(payload): axum::Json<Value>,
) -> Result<Json<Value>, BffError> {
    // The house /get convention (vms/get, nodes/get, volumes/get,
    // networks/get — #372 DP6): 400 on a missing id, 404 on an unknown
    // one, and the payload nested under a single top-level key
    // (`detail`, the networks/get spelling). The item shape is
    // byte-identical to list_tasks' items — the same keys
    // `chvctl task list` prints — so a poller and a lister render the
    // same row.
    let task_id = payload
        .get("task_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing task_id".into()))?;

    let row = sqlx::query_as::<_, TaskRow>(
        r#"
        SELECT
            operation_id AS task_id,
            status,
            operation_type AS operation,
            resource_kind,
            resource_id,
            requested_by AS actor,
            CAST(strftime('%s', requested_at) AS INTEGER) * 1000 AS started_unix_ms,
            CAST(strftime('%s', completed_at) AS INTEGER) * 1000 AS finished_unix_ms,
            error_code,
            error_message
        FROM operations
        WHERE operation_id = ?
        "#,
    )
    .bind(task_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| BffError::Internal(format!("failed to get task: {}", e)))?;

    match row {
        Some(r) => Ok(Json(json!({
            "detail": {
                "task_id": r.task_id,
                "status": r.status,
                "operation": r.operation,
                "resource_kind": r.resource_kind,
                "resource_id": r.resource_id.unwrap_or_default(),
                "actor": r.actor.unwrap_or_default(),
                "started_unix_ms": r.started_unix_ms,
                "finished_unix_ms": r.finished_unix_ms,
                // #502: same two keys as the list items — the item
                // shape here is byte-identical to list_tasks' by
                // design (#372 DP6), so the terminal-failure cause
                // rides along (NULL until a failure records it).
                "error_code": r.error_code,
                "error_message": r.error_message,
            }
        }))),
        None => Err(BffError::NotFound(format!("task {} not found", task_id))),
    }
}

pub async fn stream_tasks(
    crate::auth::BearerToken(_claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let pool = state.pool.clone();
    let resource_ids_filter: Vec<String> = params
        .get("resource_ids")
        .map(|s| {
            s.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let resource_kinds_filter: Vec<String> = params
        .get("resource_kinds")
        .map(|s| {
            s.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let tick_interval = interval(Duration::from_secs(3));
    let stream = IntervalStream::new(tick_interval).then(move |_tick| {
        let pool = pool.clone();
        let ids = resource_ids_filter.clone();
        let kinds = resource_kinds_filter.clone();

        async move {
            let mut sql = String::from(
                r#"
                SELECT
                    operation_id AS task_id,
                    status,
                    operation_type AS summary,
                    resource_kind,
                    resource_id,
                    CAST(strftime('%s', requested_at) AS INTEGER) * 1000 AS event_unix_ms,
                    error_code,
                    error_message
                FROM operations
                WHERE requested_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-30 seconds')
                "#,
            );

            if !ids.is_empty() {
                sql.push_str(" AND resource_id IN (");
                for (i, _id) in ids.iter().enumerate() {
                    if i > 0 {
                        sql.push(',');
                    }
                    sql.push_str(&format!("${}", i + 1));
                }
                sql.push(')');
            }

            if !kinds.is_empty() {
                let offset = ids.len();
                sql.push_str(" AND resource_kind IN (");
                for (i, _kind) in kinds.iter().enumerate() {
                    if i > 0 {
                        sql.push(',');
                    }
                    sql.push_str(&format!("${}", i + 1 + offset));
                }
                sql.push(')');
            }

            sql.push_str(" ORDER BY requested_at DESC LIMIT 100");

            let mut query = sqlx::query_as::<_, TaskStreamRow>(sqlx::AssertSqlSafe(sql.as_str()));
            for id in &ids {
                query = query.bind(id);
            }
            for kind in &kinds {
                query = query.bind(kind);
            }

            let rows = match query.fetch_all(&pool).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("stream_tasks db query failed: {}", e);
                    vec![]
                }
            };

            let payload = json!({
                "items": rows.into_iter().map(|r| json!({
                    "task_id": r.task_id,
                    "status": r.status,
                    "summary": r.summary,
                    "resource_kind": r.resource_kind,
                    "resource_id": r.resource_id.unwrap_or_default(),
                    "event_unix_ms": r.event_unix_ms,
                    // #502: the terminal-failure cause, same pass-through
                    // discipline as the list/get items above.
                    "error_code": r.error_code,
                    "error_message": r.error_message,
                })).collect::<Vec<Value>>()
            });

            Ok::<Event, Infallible>(Event::default().data(payload.to_string()))
        }
    });

    Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

#[derive(sqlx::FromRow)]
struct TaskStreamRow {
    task_id: String,
    status: String,
    summary: String,
    resource_kind: String,
    resource_id: Option<String>,
    event_unix_ms: Option<i64>,
    error_code: Option<String>,
    error_message: Option<String>,
}

#[derive(sqlx::FromRow)]
struct TaskRow {
    task_id: String,
    status: String,
    operation: String,
    resource_kind: String,
    resource_id: Option<String>,
    actor: Option<String>,
    started_unix_ms: Option<i64>,
    finished_unix_ms: Option<i64>,
    error_code: Option<String>,
    error_message: Option<String>,
}
