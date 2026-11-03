use axum::{extract::State, response::Json};
use rand::RngExt;
use serde_json::{json, Value};

use crate::auth::BearerToken;
use crate::router::AppState;
use crate::BffError;

fn rand_bytes_32() -> [u8; 32] {
    rand::rng().random()
}

/// Validate an API-token scope at creation time. The allowed set is the
/// only vocabulary `crate::auth::effective_role_for_scope` interprets;
/// rejecting anything else here means mint-time 400s instead of silent
/// "full" treatment at every lookup.
fn validate_scope(scope: &str) -> Result<(), BffError> {
    if matches!(scope, "full" | "readonly") {
        Ok(())
    } else {
        Err(BffError::BadRequest(format!(
            "invalid scope {scope:?}; expected \"full\" or \"readonly\""
        )))
    }
}

#[derive(sqlx::FromRow)]
struct TokenRow {
    token_id: String,
    name: String,
    scope: String,
    expires_at: Option<String>,
    last_used_at: Option<String>,
    created_at: String,
}

pub async fn list_tokens(
    State(state): State<AppState>,
    BearerToken(claims): BearerToken,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_operator_or_admin(&claims)?;
    let rows = sqlx::query_as::<_, TokenRow>(
        "SELECT token_id, name, scope, expires_at, last_used_at, created_at FROM api_tokens WHERE user_id = ? ORDER BY created_at",
    )
    .bind(&claims.sub)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "list_tokens db query failed");
        BffError::Internal(format!("failed to list tokens: {e}"))
    })?;

    let count = rows.len();
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "token_id": r.token_id,
                "name": r.name,
                "scope": r.scope,
                "expires_at": r.expires_at,
                "last_used_at": r.last_used_at,
                "created_at": r.created_at,
            })
        })
        .collect();

    Ok(Json(json!({
        "items": items,
        "count": count,
    })))
}

pub async fn create_token(
    State(state): State<AppState>,
    BearerToken(claims): BearerToken,
    axum::Json(payload): axum::Json<Value>,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_operator_or_admin(&claims)?;
    let name = payload
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing name".into()))?;

    if name.is_empty() {
        return Err(BffError::BadRequest("name must not be empty".into()));
    }

    // Scope semantics (Security T1 — enforced at token lookup in
    // `crate::auth::effective_role_for_scope`):
    //
    // - "full"     -> the token keeps the user's role unchanged.
    // - "readonly" -> the token is demoted to the viewer role (read-only
    //                 endpoints only).
    //
    // Only these two values are accepted at creation time; anything else is
    // a 400 so bad scopes are caught at mint time instead of being
    // silently treated as "full" later.
    let scope = payload
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("full");
    validate_scope(scope)?;

    let token_id = chv_common::gen_resource_id();
    let raw_token = format!("chv_{}", hex::encode(rand_bytes_32()));
    let token_hash = chv_common::sha256_hex(&raw_token);

    sqlx::query(
        "INSERT INTO api_tokens (token_id, user_id, name, token_hash, scope) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&token_id)
    .bind(&claims.sub)
    .bind(name)
    .bind(&token_hash)
    .bind(scope)
    .execute(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "create_token db insert failed");
        BffError::Internal(format!("failed to create token: {e}"))
    })?;

    let created_at =
        sqlx::query_scalar::<_, String>("SELECT created_at FROM api_tokens WHERE token_id = ?")
            .bind(&token_id)
            .fetch_one(&state.pool)
            .await
            .unwrap_or_else(|_| "unknown".to_string());
    state.cache.invalidate("overview").await;

    Ok(Json(json!({
        "token_id": token_id,
        "name": name,
        "token": raw_token,
        "scope": scope,
        "created_at": created_at,
    })))
}

pub async fn revoke_token(
    State(state): State<AppState>,
    BearerToken(claims): BearerToken,
    axum::Json(payload): axum::Json<Value>,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_operator_or_admin(&claims)?;
    let token_id = payload
        .get("token_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing token_id".into()))?
        .to_string();

    // Verify ownership before deleting
    let exists = sqlx::query_scalar::<_, String>(
        "SELECT token_id FROM api_tokens WHERE token_id = ? AND user_id = ?",
    )
    .bind(&token_id)
    .bind(&claims.sub)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "revoke_token fetch failed");
        BffError::Internal(format!("failed to check token existence: {e}"))
    })?;

    if exists.is_none() {
        return Err(BffError::NotFound(format!("token {} not found", token_id)));
    }

    sqlx::query("DELETE FROM api_tokens WHERE token_id = ? AND user_id = ?")
        .bind(&token_id)
        .bind(&claims.sub)
        .execute(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "revoke_token db delete failed");
            BffError::Internal(format!("failed to revoke token: {e}"))
        })?;
    state.cache.invalidate("overview").await;

    Ok(Json(json!({
        "revoked": true,
        "token_id": token_id,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_scope_accepts_full_and_readonly() {
        assert!(validate_scope("full").is_ok());
        assert!(validate_scope("readonly").is_ok());
    }

    #[test]
    fn validate_scope_rejects_unknown_values() {
        for bad in ["", "admin", "READONLY", "Full", "read-only"] {
            let err = validate_scope(bad).expect_err("unknown scope must be rejected");
            assert!(
                matches!(err, BffError::BadRequest(_)),
                "expected BadRequest for {bad:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn token_format_is_correct() {
        let raw_token = format!("chv_{}", hex::encode(rand_bytes_32()));
        assert!(raw_token.starts_with("chv_"));
        // "chv_" (4) + 64 hex chars (32 bytes) = 68 chars
        assert_eq!(raw_token.len(), 68);
    }

    #[test]
    fn sha256_hex_produces_64_char_string() {
        let hash = chv_common::sha256_hex("test-input");
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn sha256_hex_is_deterministic() {
        let h1 = chv_common::sha256_hex("same-input");
        let h2 = chv_common::sha256_hex("same-input");
        assert_eq!(h1, h2);
    }

    #[test]
    fn sha256_hex_differs_for_different_inputs() {
        let h1 = chv_common::sha256_hex("input-a");
        let h2 = chv_common::sha256_hex("input-b");
        assert_ne!(h1, h2);
    }
}
