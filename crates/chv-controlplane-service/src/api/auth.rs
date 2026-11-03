use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
    Json as AxumJson,
};
use chv_webui_bff::auth::Claims;
use chv_webui_bff::AppState;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

/// Dummy bcrypt hash used when the username is not found, so that
/// bcrypt::verify always runs and response time is constant regardless of
/// whether the user exists (mirrors chv-webui-bff handlers/auth.rs).
const DUMMY_HASH: &str = "$2b$12$JbNLkka47ajSOyzKo8fKI.CBvQav06.Vrnh4pbZf4VSaLwS7yI71m";

const MAX_LOGIN_ATTEMPTS: u32 = 10;
const RATE_WINDOW_SECS: u64 = 60;
/// Hard backstop on distinct usernames tracked at once. A spray of unique
/// usernames must not be able to grow the map without bound.
const MAX_LOGIN_USERS: usize = 10_000;

static LOGIN_ATTEMPTS: LazyLock<Mutex<HashMap<String, Vec<Instant>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Per-username login rate limiting, mirroring the hardened BFF login
/// handler: prune expired timestamps (dropping empty entries so unique
/// usernames cannot grow the map) and sweep the whole map when it exceeds
/// MAX_LOGIN_USERS.
async fn check_rate_limit(username: &str) -> Result<(), chv_webui_bff::BffError> {
    let mut attempts = LOGIN_ATTEMPTS.lock().await;
    let now = Instant::now();
    let window = Duration::from_secs(RATE_WINDOW_SECS);

    let recent_count = match attempts.get_mut(username) {
        Some(entry) => {
            entry.retain(|t| now.duration_since(*t) < window);
            if entry.is_empty() {
                attempts.remove(username);
                0
            } else {
                entry.len()
            }
        }
        None => 0,
    };

    if recent_count >= MAX_LOGIN_ATTEMPTS as usize {
        return Err(chv_webui_bff::BffError::TooManyRequests(
            "too many login attempts, try again later".into(),
        ));
    }

    attempts.entry(username.to_string()).or_default().push(now);

    if attempts.len() > MAX_LOGIN_USERS {
        attempts.retain(|_, v| {
            v.retain(|t| now.duration_since(*t) < window);
            !v.is_empty()
        });
    }

    Ok(())
}

/// Direct control-plane login (`POST /api/v1/auth/login`).
///
/// This is the **API-only** token path. Unlike the browser login
/// (`POST /v1/auth/login`, which never returns the JWT), callers here are
/// non-browser clients such as `chvctl` that legitimately hold a bearer
/// token. The browser flow must not be pointed at this endpoint: the UI
/// authenticates exclusively through the BFF's HttpOnly cookie.
pub async fn login_handler(
    State(state): State<AppState>,
    AxumJson(payload): AxumJson<Value>,
) -> Result<axum::response::Json<serde_json::Value>, chv_webui_bff::BffError> {
    let username = match payload.get("username").and_then(|v| v.as_str()) {
        Some(u) => u,
        None => {
            return Err(chv_webui_bff::BffError::BadRequest(
                "missing username".into(),
            ));
        }
    };

    check_rate_limit(username).await?;

    let password = match payload.get("password").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => {
            return Err(chv_webui_bff::BffError::BadRequest(
                "missing password".into(),
            ));
        }
    };

    // Look up user by username
    let row = sqlx::query_as::<_, UserRow>(
        "SELECT user_id, password_hash, role, must_change_password, token_version \
         FROM users WHERE username = $1",
    )
    .bind(username)
    .fetch_optional(&state.pool)
    .await;

    // Timing mitigation: when the user is unknown we still run bcrypt::verify
    // against DUMMY_HASH so response time does not reveal whether the
    // username exists (mirrors the hardened BFF login handler).
    let (user_row, hash_to_check) = match row {
        Ok(Some(r)) => {
            let hash = r.password_hash.clone();
            (Some(r), hash)
        }
        Ok(None) => (None, DUMMY_HASH.to_string()),
        Err(e) => {
            tracing::error!(error = %e, "db error during login");
            return Err(chv_webui_bff::BffError::Internal("Internal error".into()));
        }
    };

    let password_ok = bcrypt::verify(password, &hash_to_check).unwrap_or(false);

    let row = match user_row {
        Some(r) if password_ok => r,
        _ => {
            return Err(chv_webui_bff::BffError::Unauthorized(
                "Invalid credentials".into(),
            ));
        }
    };

    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        + 24 * 60 * 60; // 24 hours

    let must_change_password = row.must_change_password != 0;
    let token_version = row.token_version.max(0) as u64;

    let claims = Claims {
        sub: row.user_id.clone(),
        username: username.to_string(),
        role: row.role.clone(),
        exp,
        aud: Some(chv_webui_bff::auth::SESSION_AUDIENCE.to_string()),
        must_change_password,
        token_version: Some(token_version),
    };

    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
    let token = match jsonwebtoken::encode(
        &header,
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(state.jwt_secret.as_bytes()),
    ) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "failed to encode jwt token");
            return Err(chv_webui_bff::BffError::Internal(
                "failed to generate token".into(),
            ));
        }
    };

    Ok(axum::response::Json(serde_json::json!({
        "token": token,
        "user": {
            "id": row.user_id,
            "username": username,
            "role": row.role,
        },
        "must_change_password": must_change_password
    })))
}

pub async fn me_handler(
    chv_webui_bff::auth::BearerToken(claims): chv_webui_bff::auth::BearerToken,
) -> impl axum::response::IntoResponse {
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "id": claims.sub,
            "username": claims.username,
            "role": claims.role
        })),
    )
}

/// Logout terminates the browser session: the HttpOnly `chv_session`
/// cookie minted by the **BFF's** `/v1/auth/login` is cleared with
/// `Max-Age=0` and the same attributes it was set with (`Path=/`,
/// `HttpOnly`, `SameSite=Strict`), and the two non-credential UX hint
/// cookies are cleared too. (`/api/v1/auth/login` is the API-only token
/// path and never sets a cookie.)
///
/// Server-side revocation (#660): when the request presents a valid
/// session JWT (cookie or `Authorization: Bearer`), the user's
/// `token_version` is incremented, so a copied token dies immediately
/// instead of surviving until `exp`. The route remains unauthenticated so
/// clearing the cookie works even for an already-expired session.
pub async fn logout_handler(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> impl axum::response::IntoResponse {
    if let Some(token) = chv_webui_bff::auth::request_token(&headers) {
        if let Ok(claims) = chv_webui_bff::auth::validate_token(&token, &state.jwt_secret) {
            match chv_controlplane_store::UserRepository::new(state.pool.clone())
                .bump_token_version(&claims.sub)
                .await
            {
                Ok(_) => {}
                // The account is already gone: nothing left to revoke, so
                // logout succeeds and still clears every cookie (M2).
                Err(chv_controlplane_store::StoreError::NotFound { .. }) => {
                    tracing::debug!(
                        sub = %claims.sub,
                        "logout for deleted user: nothing to revoke, clearing cookies"
                    );
                }
                Err(e) => {
                    tracing::error!(error = %e, "logout failed to bump token_version");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({
                            "message": "failed to revoke session",
                            "code": "INTERNAL"
                        })),
                    )
                        .into_response();
                }
            }
        }
    }

    let mut response_headers = axum::http::HeaderMap::new();
    for cookie in [
        format!(
            "{}=; Max-Age=0; Path=/; HttpOnly; SameSite=Strict",
            chv_webui_bff::auth::SESSION_COOKIE_NAME
        ),
        format!(
            "{}=; Max-Age=0; Path=/; SameSite=Strict",
            chv_webui_bff::auth::ROLE_COOKIE_NAME
        ),
        format!(
            "{}=; Max-Age=0; Path=/; SameSite=Strict",
            chv_webui_bff::auth::AUTH_HINT_COOKIE_NAME
        ),
    ] {
        if let Ok(value) = cookie.parse::<axum::http::HeaderValue>() {
            response_headers.append(axum::http::header::SET_COOKIE, value);
        }
    }
    (
        StatusCode::OK,
        response_headers,
        Json(serde_json::json!({"ok": true})),
    )
        .into_response()
}

#[derive(sqlx::FromRow)]
struct UserRow {
    user_id: String,
    password_hash: String,
    role: String,
    /// Mirrors the `must_change_password` column added in migration 0044.
    /// Stored as INTEGER in SQLite; sqlx maps that to `i64`. install.sh
    /// sets it to 1 on the seeded admin row so the operator is forced to
    /// rotate the credential on first login.
    must_change_password: i64,
    /// Session revocation counter (#660, migration 0066).
    token_version: i64,
}
