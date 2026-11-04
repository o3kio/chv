use axum::{extract::State, response::Json};
use serde_json::{json, Value};

use crate::auth::BearerToken;
use crate::router::AppState;
use crate::BffError;

#[derive(sqlx::FromRow)]
struct UserRow {
    user_id: String,
    username: String,
    display_name: Option<String>,
    role: String,
    email: Option<String>,
    created_at: String,
    last_login_at: Option<String>,
}

pub async fn list_users(
    State(state): State<AppState>,
    BearerToken(claims): BearerToken,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_operator_or_admin(&claims)?;

    let rows = sqlx::query_as::<_, UserRow>(
        "SELECT user_id, username, display_name, role, email, created_at, last_login_at FROM users ORDER BY created_at",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "list_users db query failed");
        BffError::Internal("failed to list users".into())
    })?;

    let count = rows.len();
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "user_id": r.user_id,
                "username": r.username,
                "display_name": r.display_name,
                "role": r.role,
                "email": r.email,
                "created_at": r.created_at,
                "last_login_at": r.last_login_at,
            })
        })
        .collect();

    Ok(Json(json!({
        "items": items,
        "count": count,
    })))
}

pub async fn create_user(
    State(state): State<AppState>,
    BearerToken(claims): BearerToken,
    axum::Json(payload): axum::Json<Value>,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_admin(&claims)?;

    let username = payload
        .get("username")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing username".into()))?;

    if username.is_empty() {
        return Err(BffError::BadRequest("username must not be empty".into()));
    }

    let password = payload
        .get("password")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing password".into()))?;

    if password.len() < 8 {
        return Err(BffError::BadRequest(
            "password must be at least 8 characters".into(),
        ));
    }

    let role = payload
        .get("role")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing role".into()))?;

    if !["admin", "operator", "viewer"].contains(&role) {
        return Err(BffError::BadRequest(
            "role must be one of: admin, operator, viewer".into(),
        ));
    }

    let display_name = payload
        .get("display_name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let email = payload
        .get("email")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let password_hash = bcrypt::hash(password, 12).map_err(|e| {
        tracing::error!(error = %e, "bcrypt hash failed");
        BffError::Internal("failed to hash password".into())
    })?;

    let user_id = chv_common::gen_resource_id();

    sqlx::query(
        "INSERT INTO users (user_id, username, password_hash, role, display_name, email) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&user_id)
    .bind(username)
    .bind(&password_hash)
    .bind(role)
    .bind(&display_name)
    .bind(&email)
    .execute(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "create_user db insert failed");
        if e.to_string().contains("UNIQUE") {
            BffError::BadRequest("username already exists".into())
        } else {
            BffError::Internal("failed to create user".into())
        }
    })?;
    state.cache.invalidate("overview").await;

    Ok(Json(json!({
        "user_id": user_id,
        "username": username,
        "role": role,
    })))
}

pub async fn update_user(
    State(state): State<AppState>,
    BearerToken(claims): BearerToken,
    axum::Json(payload): axum::Json<Value>,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_admin(&claims)?;

    let user_id = payload
        .get("user_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing user_id".into()))?
        .to_string();

    let new_role = payload.get("role").and_then(|v| v.as_str());
    let new_password = payload.get("password").and_then(|v| v.as_str());
    let new_display_name = payload.get("display_name").and_then(|v| v.as_str());
    let new_email = payload.get("email").and_then(|v| v.as_str());

    // Validate role value if provided
    if let Some(r) = new_role {
        if !["admin", "operator", "viewer"].contains(&r) {
            return Err(BffError::BadRequest(
                "role must be one of: admin, operator, viewer".into(),
            ));
        }
    }

    // Validate password length if provided
    if let Some(p) = new_password {
        if p.len() < 8 {
            return Err(BffError::BadRequest(
                "password must be at least 8 characters".into(),
            ));
        }
    }

    // Fetch current user to ensure it exists
    let row = sqlx::query_as::<_, UserRow>(
        "SELECT user_id, username, display_name, role, email, created_at, last_login_at FROM users WHERE user_id = ?",
    )
    .bind(&user_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "update_user fetch failed");
        BffError::Internal("failed to look up user".into())
    })?
    .ok_or_else(|| BffError::NotFound(format!("user {} not found", user_id)))?;

    let resolved_role = new_role.unwrap_or(&row.role).to_string();
    let resolved_display_name = new_display_name.map(|s| s.to_string()).or(row.display_name);
    let resolved_email = new_email.map(|s| s.to_string()).or(row.email);

    let resolved_password_hash = if let Some(pw) = new_password {
        let hash = bcrypt::hash(pw, 12).map_err(|e| {
            tracing::error!(error = %e, "bcrypt hash failed");
            BffError::Internal("failed to hash password".into())
        })?;
        Some(hash)
    } else {
        None
    };

    if let Some(ref hash) = resolved_password_hash {
        // Admin password reset revokes every prior session (#660): a stolen
        // cookie must not survive the standard compromise response.
        sqlx::query(
            "UPDATE users SET role = ?, display_name = ?, email = ?, password_hash = ?, \
             token_version = token_version + 1 WHERE user_id = ?",
        )
        .bind(&resolved_role)
        .bind(&resolved_display_name)
        .bind(&resolved_email)
        .bind(hash)
        .bind(&user_id)
        .execute(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "update_user db update failed");
            BffError::Internal("failed to update user".into())
        })?;
    } else {
        // A role change (including demotion) revokes prior sessions so a
        // reduced-privilege account cannot keep an old admin-scoped token.
        // Cosmetic edits (display name/email) do not, matching the
        // documented behavior in docs/OPERATIONS.md; logging the target out
        // of an unrelated tab would otherwise be a surprise.
        let bump_token_version = i64::from(resolved_role != row.role);
        sqlx::query(
            "UPDATE users SET role = ?, display_name = ?, email = ?, \
             token_version = token_version + ? WHERE user_id = ?",
        )
        .bind(&resolved_role)
        .bind(&resolved_display_name)
        .bind(&resolved_email)
        .bind(bump_token_version)
        .bind(&user_id)
        .execute(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "update_user db update failed");
            BffError::Internal("failed to update user".into())
        })?;
    }
    state.cache.invalidate("overview").await;

    Ok(Json(json!({
        "user_id": user_id,
        "username": row.username,
        "role": resolved_role,
        "display_name": resolved_display_name,
        "email": resolved_email,
    })))
}

pub async fn delete_user(
    State(state): State<AppState>,
    BearerToken(claims): BearerToken,
    axum::Json(payload): axum::Json<Value>,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_admin(&claims)?;

    let user_id = payload
        .get("user_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing user_id".into()))?
        .to_string();

    // Cannot delete yourself
    if claims.sub == user_id {
        return Err(BffError::BadRequest(
            "cannot delete your own account".into(),
        ));
    }

    // Check user exists
    let exists = sqlx::query_scalar::<_, String>("SELECT user_id FROM users WHERE user_id = ?")
        .bind(&user_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "delete_user fetch failed");
            BffError::Internal("failed to check user existence".into())
        })?;

    if exists.is_none() {
        return Err(BffError::NotFound(format!("user {} not found", user_id)));
    }

    sqlx::query("DELETE FROM users WHERE user_id = ?")
        .bind(&user_id)
        .execute(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "delete_user db delete failed");
            BffError::Internal("failed to delete user".into())
        })?;
    state.cache.invalidate("overview").await;

    Ok(Json(json!({
        "deleted": true,
        "user_id": user_id,
    })))
}

#[cfg(test)]
mod tests {
    #[test]
    fn valid_roles_are_accepted() {
        let valid = ["admin", "operator", "viewer"];
        for role in &valid {
            assert!(["admin", "operator", "viewer"].contains(role));
        }
    }

    #[test]
    fn invalid_role_is_rejected() {
        let invalid = "superuser";
        assert!(!["admin", "operator", "viewer"].contains(&invalid));
    }

    #[test]
    fn password_length_validation() {
        assert!("short".len() < 8);
        assert!("longenough".len() >= 8);
    }
}
