//! Admin-only audit-log query and export surfaces (#658).
//!
//! Reads are POST-shaped like the rest of the BFF's filter endpoints.
//! The audit middleware treats these paths as read-shaped, so querying
//! the trail never appends to it.

use axum::{
    extract::State,
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use chv_controlplane_store::{AuditLogFilter, AuditLogRepository, AuditLogRow};

use crate::auth::BearerToken;
use crate::router::AppState;
use crate::BffError;

const DEFAULT_QUERY_LIMIT: i64 = 100;
const MAX_QUERY_LIMIT: i64 = 500;
const DEFAULT_EXPORT_LIMIT: i64 = 1_000;
const MAX_EXPORT_LIMIT: i64 = 10_000;

#[derive(Debug, Default, Deserialize)]
pub struct AuditQueryRequest {
    /// Matches either the stable actor id or the username.
    pub actor: Option<String>,
    pub resource_kind: Option<String>,
    pub resource_id: Option<String>,
    pub action: Option<String>,
    pub outcome: Option<String>,
    /// Inclusive epoch-millisecond lower/upper bounds.
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
pub struct AuditExportRequest {
    #[serde(flatten)]
    pub query: AuditQueryRequest,
    /// `csv` (default) or `json`.
    pub format: Option<String>,
}

pub async fn query_audit_log(
    State(state): State<AppState>,
    BearerToken(claims): BearerToken,
    Json(request): Json<AuditQueryRequest>,
) -> Result<Json<Value>, BffError> {
    // Defense in depth: the route is mounted under admin_middleware, but
    // the handler repeats the role check so a future routing mistake
    // cannot expose the trail to a non-admin.
    crate::auth::require_admin(&claims)?;

    let filter = filter_from_request(&request)?;
    let limit = clamp_limit(request.limit, DEFAULT_QUERY_LIMIT, MAX_QUERY_LIMIT);
    let offset = request.offset.unwrap_or(0).max(0);

    let repo = AuditLogRepository::new(state.pool.clone());
    let (rows, total) = repo.query(&filter, limit, offset).await.map_err(|e| {
        tracing::error!(error = %e, "audit log query failed");
        BffError::Internal("failed to query audit log".into())
    })?;

    let items: Vec<Value> = rows.iter().map(row_json).collect();
    Ok(Json(json!({
        "items": items,
        "count": items.len(),
        "total": total,
        "limit": limit,
        "offset": offset,
    })))
}

pub async fn export_audit_log(
    State(state): State<AppState>,
    BearerToken(claims): BearerToken,
    Json(request): Json<AuditExportRequest>,
) -> Result<Response, BffError> {
    crate::auth::require_admin(&claims)?;

    let filter = filter_from_request(&request.query)?;
    let format = request
        .format
        .as_deref()
        .unwrap_or("csv")
        .trim()
        .to_ascii_lowercase();
    if format != "csv" && format != "json" {
        return Err(BffError::BadRequest(
            "format must be \"csv\" or \"json\"".into(),
        ));
    }
    let limit = clamp_limit(request.query.limit, DEFAULT_EXPORT_LIMIT, MAX_EXPORT_LIMIT);

    let repo = AuditLogRepository::new(state.pool.clone());
    let (_, total) = repo.query(&filter, 0, 0).await.map_err(|e| {
        tracing::error!(error = %e, "audit log export count failed");
        BffError::Internal("failed to count audit log export".into())
    })?;
    let rows = repo.export(&filter, limit).await.map_err(|e| {
        tracing::error!(error = %e, "audit log export failed");
        BffError::Internal("failed to export audit log".into())
    })?;
    let truncated = total > rows.len() as i64;

    let (content_type, filename, body) = if format == "json" {
        let items: Vec<Value> = rows.iter().map(row_json).collect();
        let payload = json!({
            "items": items,
            "count": items.len(),
            "total": total,
            "truncated": truncated,
        });
        (
            "application/json",
            "audit-log.json",
            serde_json::to_vec(&payload)?,
        )
    } else {
        let mut output = String::new();
        output.push_str(CSV_HEADER);
        output.push('\n');
        for row in &rows {
            output.push_str(&csv_row(row));
            output.push('\n');
        }
        (
            "text/csv; charset=utf-8",
            "audit-log.csv",
            output.into_bytes(),
        )
    };

    let mut response = (StatusCode::OK, body).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")).map_err(|e| {
            BffError::Internal(format!("failed to build export filename header: {e}"))
        })?,
    );
    if let Ok(value) = HeaderValue::from_str(&total.to_string()) {
        response.headers_mut().insert("x-audit-export-total", value);
    }
    if truncated {
        response
            .headers_mut()
            .insert("x-audit-export-truncated", HeaderValue::from_static("true"));
    }
    Ok(response)
}

fn filter_from_request(request: &AuditQueryRequest) -> Result<AuditLogFilter, BffError> {
    if let (Some(from), Some(to)) = (request.from_ms, request.to_ms) {
        if from > to {
            return Err(BffError::BadRequest(
                "from_ms must not be greater than to_ms".into(),
            ));
        }
    }
    if let Some(outcome) = &request.outcome {
        if !matches!(outcome.as_str(), "success" | "failed" | "denied") {
            return Err(BffError::BadRequest(
                "outcome must be one of: success, failed, denied".into(),
            ));
        }
    }
    Ok(AuditLogFilter {
        actor: normalized(request.actor.as_deref()),
        resource_kind: normalized(request.resource_kind.as_deref()),
        resource_id: normalized(request.resource_id.as_deref()),
        action: normalized(request.action.as_deref()),
        outcome: normalized(request.outcome.as_deref()),
        from_ms: request.from_ms,
        to_ms: request.to_ms,
    })
}

fn normalized(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn clamp_limit(value: Option<i64>, default: i64, maximum: i64) -> i64 {
    value.unwrap_or(default).clamp(1, maximum)
}

fn occurred_at(ms: i64) -> String {
    use chrono::TimeZone;

    chrono::Utc
        .timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

pub(crate) fn row_json(row: &AuditLogRow) -> Value {
    let metadata: Value = serde_json::from_str(&row.metadata).unwrap_or_else(|_| json!({}));
    json!({
        "audit_id": &row.audit_id,
        "occurred_at": occurred_at(row.occurred_at_ms),
        "occurred_at_ms": row.occurred_at_ms,
        "actor_id": &row.actor_id,
        "actor_username": &row.actor_username,
        "actor_role": &row.actor_role,
        "action": &row.action,
        "method": &row.method,
        "path": &row.path,
        "resource_kind": &row.resource_kind,
        "resource_id": &row.resource_id,
        "outcome": &row.outcome,
        "status_code": row.status_code,
        "correlation_id": &row.correlation_id,
        "operation_id": &row.operation_id,
        "source_ip": &row.source_ip,
        "metadata": metadata,
    })
}

const CSV_HEADER: &str = "audit_id,occurred_at,occurred_at_ms,actor_id,actor_username,actor_role,action,method,path,resource_kind,resource_id,outcome,status_code,correlation_id,operation_id,source_ip,metadata";

fn csv_row(row: &AuditLogRow) -> String {
    let fields = [
        row.audit_id.clone(),
        occurred_at(row.occurred_at_ms),
        row.occurred_at_ms.to_string(),
        row.actor_id.clone(),
        row.actor_username.clone(),
        row.actor_role.clone(),
        row.action.clone(),
        row.method.clone(),
        row.path.clone(),
        row.resource_kind.clone().unwrap_or_default(),
        row.resource_id.clone().unwrap_or_default(),
        row.outcome.clone(),
        row.status_code.to_string(),
        row.correlation_id.clone().unwrap_or_default(),
        row.operation_id.clone().unwrap_or_default(),
        row.source_ip.clone().unwrap_or_default(),
        row.metadata.clone(),
    ];
    fields
        .iter()
        .map(|field| csv_field(field))
        .collect::<Vec<_>>()
        .join(",")
}

/// RFC 4180 quoting plus spreadsheet-formula neutralization: a field
/// beginning with `=`, `+`, `-`, `@`, tab, or CR is prefixed with a
/// single quote so opening an audit export in a spreadsheet cannot
/// execute attacker-controlled text (for example a username).
fn csv_field(value: &str) -> String {
    let neutralized = if value.starts_with('=')
        || value.starts_with('+')
        || value.starts_with('-')
        || value.starts_with('@')
        || value.starts_with('\t')
        || value.starts_with('\r')
    {
        format!("'{value}")
    } else {
        value.to_string()
    };
    if neutralized.contains(',')
        || neutralized.contains('"')
        || neutralized.contains('\n')
        || neutralized.contains('\r')
    {
        format!("\"{}\"", neutralized.replace('"', "\"\""))
    } else {
        neutralized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_quotes_commas_and_quotes() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn csv_neutralizes_formula_prefixes() {
        assert_eq!(csv_field("=cmd|' /C calc'!A0"), "'=cmd|' /C calc'!A0");
        assert_eq!(csv_field("@evil"), "'@evil");
        assert_eq!(csv_field("+1"), "'+1");
        assert_eq!(csv_field("-1"), "'-1");
    }

    #[test]
    fn limit_is_bounded() {
        assert_eq!(clamp_limit(None, 100, 500), 100);
        assert_eq!(clamp_limit(Some(0), 100, 500), 1);
        assert_eq!(clamp_limit(Some(99_999), 100, 500), 500);
    }
}
