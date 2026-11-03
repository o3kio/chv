use axum::{extract::State, response::Json};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

use crate::auth::BearerToken;
use crate::router::AppState;
use crate::BffError;

/// Dummy bcrypt hash used when the username is not found, so that bcrypt::verify
/// always runs and response time is constant regardless of whether the user exists.
const DUMMY_HASH: &str = "$2b$12$JbNLkka47ajSOyzKo8fKI.CBvQav06.Vrnh4pbZf4VSaLwS7yI71m";

const MAX_LOGIN_ATTEMPTS: u32 = 10;
const RATE_WINDOW_SECS: u64 = 60;
/// Hard backstop on distinct usernames tracked at once. A spray of unique
/// usernames must not be able to grow the map without bound.
const MAX_LOGIN_USERS: usize = 10_000;

/// Minimum length for a rotated password. install.sh promises operators a
/// real rotation; an industrial-grade floor here keeps anyone from picking
/// "admin" again the moment they're forced to change.
const MIN_NEW_PASSWORD_LEN: usize = 12;

static LOGIN_ATTEMPTS: LazyLock<Mutex<HashMap<String, Vec<Instant>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// `Secure` attribute for the session cookie, read once at process start
/// from `CHV_COOKIE_SECURE` (truthy = set the flag; default off). Enable it
/// when the BFF is only reachable over TLS — i.e. when a TLS-terminating
/// proxy fronts the BFF. The flag is deliberately NOT derived from the
/// request (e.g. `X-Forwarded-Proto`), because that header is
/// client-supplied and an attacker could trivially claim https.
static COOKIE_SECURE: LazyLock<bool> =
    LazyLock::new(|| cookie_secure_enabled(std::env::var("CHV_COOKIE_SECURE").ok().as_deref()));

/// Truthiness mapping for `CHV_COOKIE_SECURE`: any non-empty value other
/// than `0`/`false` (trimmed, ASCII case-insensitive) enables `Secure`.
fn cookie_secure_enabled(value: Option<&str>) -> bool {
    match value {
        Some(v) => {
            let v = v.trim().to_ascii_lowercase();
            !v.is_empty() && v != "0" && v != "false"
        }
        None => false,
    }
}

async fn check_rate_limit(username: &str) -> Result<(), BffError> {
    let mut attempts = LOGIN_ATTEMPTS.lock().await;
    let now = Instant::now();
    let window = std::time::Duration::from_secs(RATE_WINDOW_SECS);

    // Prune expired timestamps for this username, dropping the entry when
    // nothing recent remains so unique-username sprays cannot grow the map.
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
        return Err(BffError::TooManyRequests(
            "too many login attempts, try again later".into(),
        ));
    }

    attempts.entry(username.to_string()).or_default().push(now);

    // Backstop cap: once the map grows past MAX_LOGIN_USERS, sweep every
    // entry's expired timestamps and drop empty entries so the map can only
    // hold usernames active within the current rate window.
    if attempts.len() > MAX_LOGIN_USERS {
        attempts.retain(|_, v| {
            v.retain(|t| now.duration_since(*t) < window);
            !v.is_empty()
        });
    }

    Ok(())
}

#[derive(sqlx::FromRow)]
struct UserRow {
    user_id: String,
    username: String,
    password_hash: String,
    role: String,
    /// Mirrors the `must_change_password` column added in migration 0044.
    /// Stored as INTEGER in SQLite, which sqlx maps to i64 by default.
    /// install.sh sets this to 1 on the seeded admin row so the operator
    /// is forced to rotate the credential on first login.
    must_change_password: i64,
    /// Session revocation counter (#660, migration 0066). Embedded in the
    /// issued JWT and re-checked on every request by the auth extractor.
    token_version: i64,
}

/// Login endpoint (`POST /v1/auth/login`).
///
/// This is the **browser** login. On success the response body carries only
/// non-credential session metadata (user + `must_change_password`) — never
/// the JWT. The JWT lives exclusively in the HttpOnly `chv_session` cookie
/// (Security S1, staged item 3.B), so JavaScript cannot read or exfiltrate
/// it after login.
///
/// Two additional *non-credential* cookies are set for client-side UX:
/// - `chv_role` — the caller's role, used only to hide controls the server
///   already gates; it grants nothing.
/// - `chv_logged_in=1` — an authenticated-session hint so the SPA can
///   redirect before the first API call. It grants nothing either.
///
/// API clients that legitimately need a bearer token (chvctl) use the
/// control-plane-only `POST /api/v1/auth/login`, which is a separate,
/// documented API surface and not reachable from the browser flow.
///
/// Cookie attributes:
/// - `HttpOnly` — script-immune; the UI cannot read it.
/// - `Path=/` — sent for the whole BFF surface.
/// - `SameSite=Strict` — the cookie never rides cross-site requests,
///   which (together with the csrf_middleware's un-forgeable-request-shape
///   requirement for non-GET — `application/json`, or a non-empty
///   `x-csrf-token` header on multipart routes, #496) is the CSRF
///   posture for this staged flow.
/// - `Max-Age=<token ttl>` — the cookie dies with the JWT.
/// - `Secure` — controlled by the `CHV_COOKIE_SECURE` env var (default
///   off); enable it when the BFF sits behind TLS.
pub async fn login(
    State(state): State<AppState>,
    axum::Json(payload): axum::Json<Value>,
) -> Result<(axum::http::HeaderMap, Json<Value>), BffError> {
    let username = payload
        .get("username")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing username".into()))?;

    check_rate_limit(username).await?;

    let password = payload
        .get("password")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing password".into()))?;

    let user = sqlx::query_as::<_, UserRow>(
        "SELECT user_id, username, password_hash, role, must_change_password, token_version \
         FROM users WHERE username = ?",
    )
    .bind(username)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "login db query failed");
        BffError::Internal("authentication service unavailable".into())
    })?;

    let (user_row, hash_to_check) = match user {
        Some(u) => {
            let hash = u.password_hash.clone();
            (Some(u), hash)
        }
        None => (None, DUMMY_HASH.to_string()),
    };

    let valid = bcrypt::verify(password, &hash_to_check).unwrap_or(false);

    let user = match user_row {
        Some(u) if valid => u,
        _ => {
            return Err(BffError::Unauthorized(
                "Invalid username or password".into(),
            ))
        }
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    const TOKEN_TTL_SECS: u64 = 24 * 60 * 60;
    let exp = now + TOKEN_TTL_SECS;

    let must_change_password = user.must_change_password != 0;
    let token_version = user.token_version.max(0) as u64;

    let claims = crate::auth::Claims {
        sub: user.user_id.clone(),
        username: user.username.clone(),
        role: user.role.clone(),
        exp,
        aud: Some(crate::auth::SESSION_AUDIENCE.to_string()),
        must_change_password,
        token_version: Some(token_version),
    };

    let token = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(state.jwt_secret.as_bytes()),
    )
    .map_err(|e| BffError::Internal(format!("failed to encode token: {}", e)))?;

    // Session cookie (Security S1, 3.B). `Secure` comes from the
    // `CHV_COOKIE_SECURE` env var read once at startup (see [`COOKIE_SECURE`])
    // — never from a client-supplied header. Behind nginx the BFF sees
    // plain http, so the default is NO Secure flag; operators enable it
    // when the BFF is reachable only over TLS.
    let secure_suffix = if *COOKIE_SECURE { "; Secure" } else { "" };
    let session_cookie = format!(
        "{}={token}; HttpOnly; Path=/; SameSite=Strict; Max-Age={TOKEN_TTL_SECS}{secure_suffix}",
        crate::auth::SESSION_COOKIE_NAME
    );
    // Non-credential UX hints: the server still authorizes every request
    // from the JWT, so tampering with either cookie grants nothing.
    let role_cookie = format!(
        "{}={}; Path=/; SameSite=Strict; Max-Age={TOKEN_TTL_SECS}{secure_suffix}",
        crate::auth::ROLE_COOKIE_NAME,
        user.role
    );
    let hint_cookie = format!(
        "{}=1; Path=/; SameSite=Strict; Max-Age={TOKEN_TTL_SECS}{secure_suffix}",
        crate::auth::AUTH_HINT_COOKIE_NAME
    );

    let mut response_headers = axum::http::HeaderMap::new();
    for cookie in [session_cookie, role_cookie, hint_cookie] {
        let cookie_value = cookie
            .parse::<axum::http::HeaderValue>()
            .map_err(|e| BffError::Internal(format!("failed to build session cookie: {e}")))?;
        response_headers.append(axum::http::header::SET_COOKIE, cookie_value);
    }

    Ok((
        response_headers,
        Json(json!({
            "user": {
                "username": user.username,
                "role": user.role
            },
            "must_change_password": must_change_password
        })),
    ))
}

/// Logout endpoint (`POST /v1/auth/logout`).
///
/// Clears the cookie session minted by [`login`] AND revokes the presented
/// token server-side (#660): when the request carries a valid
/// `chv_session` JWT (or `Authorization: Bearer`), the user's
/// `token_version` is incremented, so every token issued before this
/// logout — including a copy an attacker extracted — fails the
/// revocation check immediately. The cookie is then cleared with
/// `Max-Age=0` and the same attributes it was set with.
///
/// The route stays deliberately unauthenticated: clearing the cookie must
/// succeed even when the caller's token is already invalid or absent. If a
/// valid token is present but the version bump fails (database outage), the
/// endpoint fails with 500 rather than pretending the session was revoked;
/// the browser still clears the cookie client-side on redirect.
pub async fn logout(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<(axum::http::HeaderMap, Json<Value>), BffError> {
    if let Some(token) = crate::auth::request_token(&headers) {
        match crate::auth::validate_token(&token, &state.jwt_secret) {
            Ok(claims) => {
                match chv_controlplane_store::UserRepository::new(state.pool.clone())
                    .bump_token_version(&claims.sub)
                    .await
                {
                    Ok(_) => {}
                    // The account is already gone: there is no session left
                    // to revoke. Treat logout as successful and still clear
                    // every cookie (M2).
                    Err(chv_controlplane_store::StoreError::NotFound { .. }) => {
                        tracing::debug!(
                            sub = %claims.sub,
                            "logout for deleted user: nothing to revoke, clearing cookies"
                        );
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "logout failed to bump token_version");
                        return Err(BffError::Internal("failed to revoke session".into()));
                    }
                }
            }
            Err(e) => {
                // Already invalid/expired: clearing the cookie is the whole
                // operation. Do not leak which condition applied.
                tracing::debug!(error = %e, "logout token was not valid; clearing cookie only");
            }
        }
    }

    let mut headers = axum::http::HeaderMap::new();
    for cookie in [
        // Credential: also HttpOnly to match the original.
        format!(
            "{}=; Max-Age=0; Path=/; HttpOnly; SameSite=Strict",
            crate::auth::SESSION_COOKIE_NAME
        ),
        // Non-credential UX hints.
        format!(
            "{}=; Max-Age=0; Path=/; SameSite=Strict",
            crate::auth::ROLE_COOKIE_NAME
        ),
        format!(
            "{}=; Max-Age=0; Path=/; SameSite=Strict",
            crate::auth::AUTH_HINT_COOKIE_NAME
        ),
    ] {
        let cookie_value = cookie
            .parse::<axum::http::HeaderValue>()
            .map_err(|e| BffError::Internal(format!("failed to build logout cookie: {e}")))?;
        headers.append(axum::http::header::SET_COOKIE, cookie_value);
    }
    Ok((headers, Json(json!({ "ok": true }))))
}

/// Validate password policy. Centralised so the rules are visible at one site
/// and tests can pin them. Returns a human-readable reason on failure that is
/// safe to surface in 400 responses (no secrets, no PII).
fn validate_new_password_policy(password: &str) -> Result<(), &'static str> {
    if password.len() < MIN_NEW_PASSWORD_LEN {
        return Err("new password must be at least 12 characters");
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
struct PasswordRow {
    password_hash: String,
}

/// `POST /v1/auth/change-password`
///
/// Changes the authenticated caller's password, clears the
/// `must_change_password` flag, and increments `users.token_version`
/// (#660). The caller's identity comes from `claims.sub` in the bearer
/// token — a user can only change their OWN password through this endpoint.
///
/// The version bump revokes every token issued before this request,
/// including the one that authenticated it. The client must re-login with
/// the new password; the UI redirects to `/login` after success. This is
/// deliberate: a password change is a credential-rotation event, so all
/// pre-rotation sessions die.
pub async fn change_password(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    axum::Json(payload): axum::Json<Value>,
) -> Result<Json<Value>, BffError> {
    let current_password = payload
        .get("current_password")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing current_password".into()))?;
    let new_password = payload
        .get("new_password")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing new_password".into()))?;

    // Validate the new password BEFORE doing the DB lookup. Cheap rejection
    // for the most common mistake (too short) and avoids hashing work.
    validate_new_password_policy(new_password)
        .map_err(|reason| BffError::BadRequest(reason.into()))?;

    let row = sqlx::query_as::<_, PasswordRow>("SELECT password_hash FROM users WHERE user_id = ?")
        .bind(&claims.sub)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "change_password db query failed");
            BffError::Internal("authentication service unavailable".into())
        })?;

    // The bearer token decoded successfully so the user existed at issue time.
    // If they've been deleted since, treat the token as no longer valid.
    let row = row.ok_or_else(|| BffError::Unauthorized("invalid current password".into()))?;

    // Don't leak whether the user exists vs. whether the password was wrong:
    // both paths return the same Unauthorized response.
    let valid = bcrypt::verify(current_password, &row.password_hash).unwrap_or(false);
    if !valid {
        return Err(BffError::Unauthorized("invalid current password".into()));
    }

    let new_hash = bcrypt::hash(new_password, bcrypt::DEFAULT_COST).map_err(|e| {
        tracing::error!(error = %e, "bcrypt hash failed during change_password");
        BffError::Internal("failed to hash password".into())
    })?;

    let updated = sqlx::query(
        "UPDATE users \
         SET password_hash = ?, \
             must_change_password = 0, \
             token_version = token_version + 1, \
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') \
         WHERE user_id = ?",
    )
    .bind(&new_hash)
    .bind(&claims.sub)
    .execute(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "change_password db update failed");
        BffError::Internal("failed to update password".into())
    })?;

    if updated.rows_affected() != 1 {
        return Err(BffError::Internal("password update affected 0 rows".into()));
    }

    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Claims;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::sync::Arc;

    /// Verify that bcrypt::verify works with a known hash.
    /// Uses the historical default hash that migration 0008 once shipped;
    /// the test only exercises bcrypt verification, not production credentials.
    /// Hash: $2b$12$JbNLkka47ajSOyzKo8fKI.CBvQav06.Vrnh4pbZf4VSaLwS7yI71m
    /// Password: "admin"
    #[test]
    fn bcrypt_verify_known_admin_hash() {
        let hash = "$2b$12$JbNLkka47ajSOyzKo8fKI.CBvQav06.Vrnh4pbZf4VSaLwS7yI71m";
        let result = bcrypt::verify("admin", hash).expect("bcrypt::verify should not error");
        assert!(result, "bcrypt::verify should return true for known-hash");
    }

    #[test]
    fn bcrypt_verify_wrong_password_fails() {
        let hash = "$2b$12$JbNLkka47ajSOyzKo8fKI.CBvQav06.Vrnh4pbZf4VSaLwS7yI71m";
        let result = bcrypt::verify("wrong", hash).expect("bcrypt::verify should not error");
        assert!(
            !result,
            "bcrypt::verify should return false for wrong password"
        );
    }

    /// Verify that the timing-attack mitigation path works: when a user is not found we
    /// run bcrypt::verify against DUMMY_HASH so the response time is constant.
    /// A valid password checked against a hash of a *different* password must return false.
    #[test]
    fn bcrypt_verify_valid_password_against_dummy_hash_returns_false() {
        // DUMMY_HASH is a bcrypt hash of "admin".  Verifying a *different* valid password
        // ("hunter2") against it must return false, confirming the dummy path rejects correctly.
        let result = bcrypt::verify("hunter2", super::DUMMY_HASH)
            .expect("bcrypt::verify should not error against dummy hash");
        assert!(
            !result,
            "bcrypt::verify should return false when the password does not match the dummy hash"
        );
    }

    #[test]
    fn validate_new_password_rejects_short() {
        // 11 chars — one below the 12-char floor.
        assert!(validate_new_password_policy("short_pwd_1").is_err());
    }

    #[test]
    fn validate_new_password_accepts_at_floor() {
        // 12 chars — exactly at the floor.
        assert!(validate_new_password_policy("twelve_charz").is_ok());
    }

    // ------------------------------------------------------------------
    // Integration tests against a real in-memory SQLite pool.
    // Mirrors the pattern used in handlers/vms.rs (`build_test_pool`).
    // ------------------------------------------------------------------

    /// Mutation service stub — none of these auth tests trigger any
    /// mutation, so every method panics if called. Keeping the surface
    /// minimal here: if a future test needs real mutations, swap in
    /// `chv_controlplane_service::ControlPlaneMutationService`.
    struct NoopMutations;
    #[async_trait::async_trait]
    impl crate::mutations::MutationService for NoopMutations {
        async fn mutate_vm(
            &self,
            _vm_id: String,
            _action: String,
            _force: bool,
            _requested_by: String,
        ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
            unreachable!("mutate_vm not used in auth tests")
        }
        async fn migrate_vm(
            &self,
            _vm_id: String,
            _target_node_id: String,
            _pause_first: bool,
            _requested_by: String,
        ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
            unreachable!("migrate_vm not used in auth tests")
        }
        async fn snapshot_vm(
            &self,
            _vm_id: String,
            _destination: String,
            _requested_by: String,
        ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
            unreachable!("snapshot_vm not used in auth tests")
        }
        async fn restore_snapshot(
            &self,
            _vm_id: String,
            _source: String,
            _requested_by: String,
        ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
            unreachable!("restore_snapshot not used in auth tests")
        }
        async fn mutate_node(
            &self,
            _node_id: String,
            _action: String,
            _requested_by: String,
        ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateNodeResponse, BffError> {
            unreachable!("mutate_node not used in auth tests")
        }
        async fn mutate_volume(
            &self,
            _volume_id: String,
            _action: String,
            _force: bool,
            _resize_bytes: Option<u64>,
            _vm_id: Option<String>,
            _requested_by: String,
        ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
            unreachable!("mutate_volume not used in auth tests")
        }
        async fn snapshot_volume(
            &self,
            _volume_id: String,
            _snapshot_name: String,
            _requested_by: String,
        ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
            unreachable!("snapshot_volume not used in auth tests")
        }
        async fn restore_volume_snapshot(
            &self,
            _volume_id: String,
            _snapshot_name: String,
            _requested_by: String,
        ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
            unreachable!("restore_volume_snapshot not used in auth tests")
        }
        async fn delete_volume_snapshot(
            &self,
            _volume_id: String,
            _snapshot_name: String,
            _requested_by: String,
        ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
            unreachable!("delete_volume_snapshot not used in auth tests")
        }
        async fn clone_volume(
            &self,
            _source_volume_id: String,
            _target_volume_id: String,
            _requested_by: String,
        ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
            unreachable!("clone_volume not used in auth tests")
        }
        async fn mutate_network(
            &self,
            _network_id: String,
            _action: String,
            _force: bool,
            _requested_by: String,
        ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateNetworkResponse, BffError> {
            unreachable!("mutate_network not used in auth tests")
        }
    }

    async fn build_test_state() -> AppState {
        // Single-connection in-memory pool so all queries hit the same DB.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory sqlite");
        chv_controlplane_store::run_migrations(&pool, None)
            .await
            .expect("run migrations");

        use chv_controlplane_store::{
            AlertRepository, AlertRuleRepository, ApplyRunRepository, BackupRepository,
            DesiredStateRepository, DriftReportRepository, EventRepository, ImageRepository,
            NetworkRepository, NodeRepository, NotificationOutboxRepository,
            ObservedStateRepository, OperationRepository, TopologyRepository,
        };
        AppState {
            pool: pool.clone(),
            node_repo: NodeRepository::new(pool.clone()),
            operation_repo: OperationRepository::new(pool.clone()),
            event_repo: EventRepository::new(pool.clone()),
            alert_repo: AlertRepository::new(pool.clone()),
            alert_rules: std::sync::Arc::new(AlertRuleRepository::new(pool.clone())),
            notification_outbox: std::sync::Arc::new(NotificationOutboxRepository::new(
                pool.clone(),
            )),
            alerting_max_rules: 200,
            notification_channels: crate::router::NotificationChannels {
                webhook: false,
                slack: false,
            },
            desired_state_repo: DesiredStateRepository::new(pool.clone()),
            observed_state_repo: ObservedStateRepository::new(pool.clone()),
            backup_repo: BackupRepository::new(pool.clone()),
            topology_repo: TopologyRepository::new(pool.clone()),
            network_repo: NetworkRepository::new(pool.clone()),
            image_repo: ImageRepository::new(pool.clone()),
            apply_runs: Arc::new(ApplyRunRepository::new(pool.clone())),
            drift_reports: Arc::new(DriftReportRepository::new(pool.clone())),
            netbox_config: Arc::new(
                chv_controlplane_store::NetboxProjectionConfigRepository::new(pool.clone()),
            ),
            netbox_runs: Arc::new(chv_controlplane_store::NetboxProjectionRunRepository::new(
                pool.clone(),
            )),
            mutations: Arc::new(NoopMutations),
            jwt_secret: "test-secret".to_string(),
            monitoring: None,
            monitoring_health: crate::MonitoringHealth::new(),
            agent_runtime_dir: std::path::PathBuf::from("/var/lib/chv/agent"),
            cache: crate::cache::BffCache::new(5),
            clock: Arc::new(chv_common::SystemClock),
        }
    }

    async fn seed_user(
        pool: &sqlx::SqlitePool,
        user_id: &str,
        username: &str,
        password: &str,
        must_change: bool,
    ) {
        // Use a low cost so test runtime stays under a second; bcrypt
        // semantics are identical at any cost.
        let hash = bcrypt::hash(password, 4).expect("bcrypt hash");
        sqlx::query(
            "INSERT INTO users (user_id, username, password_hash, role, must_change_password) \
             VALUES (?, ?, ?, 'admin', ?)",
        )
        .bind(user_id)
        .bind(username)
        .bind(&hash)
        .bind(if must_change { 1 } else { 0 })
        .execute(pool)
        .await
        .expect("seed user");
    }

    fn admin_token_for(state: &AppState, user_id: &str, must_change: bool) -> String {
        admin_token_with_version(state, user_id, must_change, Some(0))
    }

    fn admin_token_with_version(
        state: &AppState,
        user_id: &str,
        must_change: bool,
        token_version: Option<u64>,
    ) -> String {
        let exp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let claims = Claims {
            sub: user_id.to_string(),
            username: "admin".to_string(),
            role: "admin".to_string(),
            exp,
            must_change_password: must_change,
            aud: None,
            token_version,
        };
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        jsonwebtoken::encode(
            &header,
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(state.jwt_secret.as_bytes()),
        )
        .expect("encode test token")
    }

    /// Pull the `chv_session` JWT out of a login response's Set-Cookie
    /// headers (the body no longer carries it).
    fn session_cookie_token(headers: &axum::http::HeaderMap) -> String {
        headers
            .get_all(axum::http::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|cookie| {
                let rest =
                    cookie.strip_prefix(&format!("{}=", crate::auth::SESSION_COOKIE_NAME))?;
                Some(rest.split(';').next().unwrap_or_default().to_string())
            })
            .expect("login must set the chv_session cookie")
    }

    /// Map a `BffError` variant to its HTTP status code so tests can pin
    /// the response without round-tripping through `IntoResponse`.
    fn err_status(e: &BffError) -> u16 {
        match e {
            BffError::BadRequest(_) => 400,
            BffError::Unauthorized(_) => 401,
            BffError::Forbidden(_) => 403,
            BffError::NotFound(_) => 404,
            BffError::Conflict(_) => 409,
            BffError::TooManyRequests(_) => 429,
            BffError::MonitoringUnavailable(_) => 503,
            BffError::MonitoringQuery { .. } => 400,
            BffError::Internal(_) => 500,
            BffError::NotImplemented(_) => 501,
            BffError::QuotaExceeded { .. } => 422,
            BffError::GraphEmpty => 422,
            BffError::PlanExpired { .. } => 409,
            BffError::PlanNotDiscardable { .. } => 409,
            BffError::MissingConfirmation { .. } => 400,
            BffError::WarningsNotAcknowledged { .. } => 400,
            BffError::PlanNotApplicable { .. } => 409,
            BffError::ProductionRequiresAdmin { .. } => 403,
            BffError::PlanModeMismatch { .. } => 400,
            BffError::InvalidResourceName { .. } => 400,
            BffError::DriftCheckFailed { .. } => 502,
            BffError::NetboxNotConfigured { .. } => 404,
            BffError::NetboxNotConfiguredPrecondition { .. } => 400,
            BffError::NetboxNotApplied { .. } => 400,
            BffError::NetboxHttpsRequired => 400,
            BffError::NetboxTokenMissing { .. } => 400,
            BffError::NetboxRunActive { .. } => 409,
            BffError::ProjectionRunNotRetryable { .. } => 409,
            BffError::NetboxStaleVersion { .. } => 409,
            BffError::NetboxUnreachable { .. } => 502,
            BffError::NetboxAuthFailed { .. } => 502,
        }
    }

    #[tokio::test]
    async fn login_returns_must_change_password_flag_when_set() {
        let state = build_test_state().await;
        seed_user(&state.pool, "u-must-1", "alice", "correctpassword", true).await;

        let body = serde_json::json!({"username": "alice", "password": "correctpassword"});
        let (headers, resp) = login(axum::extract::State(state.clone()), axum::Json(body))
            .await
            .expect("login should succeed");
        let v = resp.0;
        assert_eq!(
            v.get("must_change_password").and_then(|x| x.as_bool()),
            Some(true),
            "response JSON must include must_change_password=true"
        );
        assert!(
            v.get("token").is_none(),
            "the browser login must never return the JWT to JavaScript"
        );

        // The token lives only in the cookie and must carry the flag so
        // middleware can enforce it.
        let token = session_cookie_token(&headers);
        let claims = crate::auth::validate_token(&token, &state.jwt_secret).expect("decode token");
        assert!(claims.must_change_password);
        assert_eq!(claims.token_version, Some(0));
    }

    #[tokio::test]
    async fn login_returns_must_change_password_false_when_clear() {
        let state = build_test_state().await;
        seed_user(&state.pool, "u-clear-1", "bob", "correctpassword", false).await;

        let body = serde_json::json!({"username": "bob", "password": "correctpassword"});
        let (headers, resp) = login(axum::extract::State(state.clone()), axum::Json(body))
            .await
            .expect("login should succeed");
        let v = resp.0;
        assert_eq!(
            v.get("must_change_password").and_then(|x| x.as_bool()),
            Some(false)
        );
        assert!(v.get("token").is_none());
        let token = session_cookie_token(&headers);
        let claims = crate::auth::validate_token(&token, &state.jwt_secret).expect("decode token");
        assert!(!claims.must_change_password);
    }

    #[tokio::test]
    async fn login_token_version_matches_user_row() {
        let state = build_test_state().await;
        seed_user(&state.pool, "u-ver-1", "vera", "correctpassword", false).await;
        sqlx::query("UPDATE users SET token_version = 7 WHERE user_id = 'u-ver-1'")
            .execute(&state.pool)
            .await
            .unwrap();

        let body = serde_json::json!({"username": "vera", "password": "correctpassword"});
        let (headers, _resp) = login(axum::extract::State(state.clone()), axum::Json(body))
            .await
            .expect("login should succeed");
        let claims =
            crate::auth::validate_token(&session_cookie_token(&headers), &state.jwt_secret)
                .expect("decode token");
        assert_eq!(claims.token_version, Some(7));
        assert_eq!(
            claims.aud.as_deref(),
            Some(crate::auth::SESSION_AUDIENCE),
            "freshly minted session tokens must carry the session audience"
        );
    }

    // ── session cookie (Security S1, staged item 3.B) ─────────────────────

    fn set_cookie<'a>(headers: &'a axum::http::HeaderMap, name: &str) -> Option<&'a str> {
        headers
            .get_all(axum::http::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find(|cookie| cookie.starts_with(&format!("{name}=")))
    }

    #[tokio::test]
    async fn login_sets_session_cookie_with_expected_attributes() {
        let state = build_test_state().await;
        seed_user(&state.pool, "u-cookie-1", "gina", "correctpassword", false).await;

        let body = serde_json::json!({"username": "gina", "password": "correctpassword"});
        let (headers, resp) = login(axum::extract::State(state.clone()), axum::Json(body))
            .await
            .expect("login should succeed");

        assert!(
            resp.0.get("token").is_none(),
            "no JWT may be returned in the login body"
        );
        let token = session_cookie_token(&headers);
        let cookie = set_cookie(&headers, crate::auth::SESSION_COOKIE_NAME)
            .expect("login must set the session cookie");

        assert!(
            cookie.starts_with(&format!("{}={token};", crate::auth::SESSION_COOKIE_NAME)),
            "session cookie must carry the issued JWT; got: {cookie}"
        );
        assert!(cookie.contains("; HttpOnly"), "cookie must be HttpOnly");
        assert!(cookie.contains("; Path=/"), "cookie must be Path=/");
        assert!(
            cookie.contains("; SameSite=Strict"),
            "cookie must be SameSite=Strict"
        );
        assert!(
            cookie.contains("; Max-Age=86400"),
            "cookie TTL must match the 24h token TTL"
        );
        // `Secure` follows the CHV_COOKIE_SECURE env var read once at
        // startup — never a request header (the handler no longer even
        // accepts headers). Unset in the test process, so off by default.
        assert_eq!(
            cookie.contains("; Secure"),
            *COOKIE_SECURE,
            "Secure must follow CHV_COOKIE_SECURE, not the request"
        );

        // The cookie JWT must verify through the exact same path the
        // middleware uses.
        let claims =
            crate::auth::validate_token(&token, &state.jwt_secret).expect("cookie JWT valid");
        assert_eq!(claims.username, "gina");

        // Non-credential UX hints accompany the credential: the role is
        // readable by the SPA and the hint drives the login redirect; the
        // server never authorizes from either.
        let role_cookie = set_cookie(&headers, crate::auth::ROLE_COOKIE_NAME)
            .expect("login must set the role hint cookie");
        assert!(role_cookie.starts_with("chv_role=admin;"));
        assert!(
            !role_cookie.contains("HttpOnly"),
            "the role hint is intentionally script-readable and grants nothing"
        );
        let hint_cookie = set_cookie(&headers, crate::auth::AUTH_HINT_COOKIE_NAME)
            .expect("login must set the logged-in hint cookie");
        assert!(hint_cookie.starts_with("chv_logged_in=1;"));
        assert!(!hint_cookie.contains("HttpOnly"));
    }

    #[test]
    fn cookie_secure_flag_truthiness() {
        // Absent / empty / explicit off → no Secure attribute.
        assert!(!cookie_secure_enabled(None));
        assert!(!cookie_secure_enabled(Some("")));
        assert!(!cookie_secure_enabled(Some("   ")));
        assert!(!cookie_secure_enabled(Some("0")));
        assert!(!cookie_secure_enabled(Some("false")));
        assert!(!cookie_secure_enabled(Some(" FALSE ")));
        // Any other non-empty value → Secure.
        assert!(cookie_secure_enabled(Some("1")));
        assert!(cookie_secure_enabled(Some("true")));
        assert!(cookie_secure_enabled(Some("TRUE")));
        assert!(cookie_secure_enabled(Some(" yes ")));
    }

    #[tokio::test]
    async fn change_password_succeeds_with_valid_current() {
        let state = build_test_state().await;
        seed_user(&state.pool, "u-cp-ok", "carol", "oldpassword12", true).await;

        let token = admin_token_for(&state, "u-cp-ok", true);
        let claims = crate::auth::validate_token(&token, &state.jwt_secret).unwrap();

        let body = serde_json::json!({
            "current_password": "oldpassword12",
            "new_password": "BrandNewPassword!42",
        });
        let resp = change_password(
            BearerToken(claims),
            axum::extract::State(state.clone()),
            axum::Json(body),
        )
        .await
        .expect("change_password should succeed");
        assert_eq!(resp.0.get("ok").and_then(|v| v.as_bool()), Some(true));

        // DB row: new hash verifies, must_change_password cleared, and the
        // token_version bumped so the token that authenticated this request
        // is revoked (#660).
        let row: (String, i64, i64) = sqlx::query_as(
            "SELECT password_hash, must_change_password, token_version \
             FROM users WHERE user_id = ?",
        )
        .bind("u-cp-ok")
        .fetch_one(&state.pool)
        .await
        .unwrap();
        assert!(bcrypt::verify("BrandNewPassword!42", &row.0).unwrap());
        assert_eq!(row.1, 0, "must_change_password must be cleared");
        assert_eq!(row.2, 1, "password change must revoke older tokens");
    }

    #[tokio::test]
    async fn change_password_fails_with_wrong_current() {
        let state = build_test_state().await;
        seed_user(&state.pool, "u-cp-bad", "dan", "rightpassword12", true).await;

        let token = admin_token_for(&state, "u-cp-bad", true);
        let claims = crate::auth::validate_token(&token, &state.jwt_secret).unwrap();

        let body = serde_json::json!({
            "current_password": "wrong-password",
            "new_password": "AnotherStrongPassword!1",
        });
        let result = change_password(
            BearerToken(claims),
            axum::extract::State(state.clone()),
            axum::Json(body),
        )
        .await;
        let err = result.expect_err("must reject wrong current password");
        assert_eq!(
            err_status(&err),
            401,
            "wrong current password must return 401"
        );

        // DB hash unchanged, flag unchanged.
        let row: (String, i64) = sqlx::query_as(
            "SELECT password_hash, must_change_password FROM users WHERE user_id = ?",
        )
        .bind("u-cp-bad")
        .fetch_one(&state.pool)
        .await
        .unwrap();
        assert!(bcrypt::verify("rightpassword12", &row.0).unwrap());
        assert_eq!(row.1, 1, "must_change_password must still be set");
    }

    #[tokio::test]
    async fn change_password_rejects_weak_new_password() {
        let state = build_test_state().await;
        seed_user(&state.pool, "u-cp-weak", "eve", "rightpassword12", true).await;

        let token = admin_token_for(&state, "u-cp-weak", true);
        let claims = crate::auth::validate_token(&token, &state.jwt_secret).unwrap();

        let body = serde_json::json!({
            "current_password": "rightpassword12",
            "new_password": "shortpw", // 7 chars, well below the 12-char floor
        });
        let result = change_password(
            BearerToken(claims),
            axum::extract::State(state.clone()),
            axum::Json(body),
        )
        .await;
        let err = result.expect_err("must reject weak new password");
        assert_eq!(err_status(&err), 400, "weak new password must return 400");

        // Hash unchanged, flag unchanged — policy fails fast before any UPDATE.
        let row: (String, i64) = sqlx::query_as(
            "SELECT password_hash, must_change_password FROM users WHERE user_id = ?",
        )
        .bind("u-cp-weak")
        .fetch_one(&state.pool)
        .await
        .unwrap();
        assert!(bcrypt::verify("rightpassword12", &row.0).unwrap());
        assert_eq!(row.1, 1);
    }

    /// End-to-end via the real `bff_router`: a request with a JWT carrying
    /// `must_change_password=true` must be rejected on a protected route
    /// (here `/v1/overview`) but accepted on `/v1/auth/change-password`.
    #[tokio::test]
    async fn protected_route_blocked_when_must_change_password_set() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let state = build_test_state().await;
        seed_user(&state.pool, "u-block-1", "frank", "currentpassword12", true).await;

        let token = admin_token_for(&state, "u-block-1", true);

        let app = crate::bff_router(state.clone()).with_state(state.clone());

        // Protected route: must be 403 with PASSWORD_CHANGE_REQUIRED.
        let req = Request::builder()
            .method("POST")
            .uri("/v1/overview")
            .header("authorization", format!("Bearer {}", token))
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "protected route must reject must_change_password caller"
        );
        let body_bytes = axum::body::to_bytes(resp.into_body(), 65536).await.unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body_json["code"], "PASSWORD_CHANGE_REQUIRED");

        // Change-password route: must be allowed (and succeed since current pw matches).
        let req2 = Request::builder()
            .method("POST")
            .uri("/v1/auth/change-password")
            .header("authorization", format!("Bearer {}", token))
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "current_password": "currentpassword12",
                    "new_password": "VeryStrongNewPassword!1",
                })
                .to_string(),
            ))
            .unwrap();
        let resp2 = app.oneshot(req2).await.unwrap();
        assert_eq!(
            resp2.status(),
            StatusCode::OK,
            "change-password route must be reachable while flag is set"
        );
    }

    // ── session-cookie fallback (Security S1, staged item 3.B) ────────────

    #[tokio::test]
    async fn cookie_only_request_is_authenticated() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let state = build_test_state().await;
        seed_user(
            &state.pool,
            "u-cookie-mw-1",
            "cookie1",
            "irrelevant12",
            false,
        )
        .await;
        let token = admin_token_for(&state, "u-cookie-mw-1", false);

        let app = crate::bff_router(state.clone()).with_state(state);

        // No Authorization header at all; the chv_session cookie alone must
        // authenticate through the exact same JWT verification path.
        let req = Request::builder()
            .method("POST")
            .uri("/v1/overview")
            .header(
                axum::http::header::COOKIE,
                format!("{}={token}", crate::auth::SESSION_COOKIE_NAME),
            )
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "cookie-only request must authenticate"
        );
    }

    #[tokio::test]
    async fn invalid_session_cookie_returns_401() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let state = build_test_state().await;
        let app = crate::bff_router(state.clone()).with_state(state);

        // A garbage cookie JWT must be a hard 401 — never a silent fallback
        // to an anonymous request.
        let req = Request::builder()
            .method("POST")
            .uri("/v1/overview")
            .header(
                axum::http::header::COOKIE,
                format!("{}=not-a-valid-jwt", crate::auth::SESSION_COOKIE_NAME),
            )
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "invalid cookie JWT must be 401"
        );
    }

    #[tokio::test]
    async fn authorization_header_takes_precedence_over_cookie() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let state = build_test_state().await;
        seed_user(
            &state.pool,
            "u-cookie-mw-2",
            "cookie2",
            "irrelevant12",
            false,
        )
        .await;
        let token = admin_token_for(&state, "u-cookie-mw-2", false);

        let app = crate::bff_router(state.clone()).with_state(state);

        // Valid Authorization header + invalid cookie: the header wins, so
        // the request must succeed (200), not 401.
        let req = Request::builder()
            .method("POST")
            .uri("/v1/overview")
            .header("authorization", format!("Bearer {token}"))
            .header(
                axum::http::header::COOKIE,
                format!("{}=not-a-valid-jwt", crate::auth::SESSION_COOKIE_NAME),
            )
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "Authorization header must take precedence over the cookie"
        );
    }

    // ── logout and server-side revocation (#660) ──────────────────────────

    fn json_post(uri: &str) -> axum::http::request::Builder {
        axum::http::Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
    }

    async fn overview_status(
        app: &axum::Router,
        token: Option<&str>,
    ) -> (axum::http::StatusCode, serde_json::Value) {
        use axum::body::Body;
        use tower::ServiceExt;

        let mut builder = json_post("/v1/overview");
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        let resp = app
            .clone()
            .oneshot(builder.body(Body::from("{}")).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 65536).await.unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, body)
    }

    #[tokio::test]
    async fn logout_revokes_the_presented_token_server_side() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let state = build_test_state().await;
        seed_user(&state.pool, "u-logout", "logout", "irrelevant12", false).await;
        let token = admin_token_for(&state, "u-logout", false);
        let app = crate::bff_router(state.clone()).with_state(state.clone());

        // A browser logout presents the HttpOnly cookie. The endpoint must
        // both clear the cookie and bump the user's token_version.
        let req = Request::builder()
            .method("POST")
            .uri("/v1/auth/logout")
            .header(
                axum::http::header::COOKIE,
                format!("{}={token}", crate::auth::SESSION_COOKIE_NAME),
            )
            .header("content-type", "application/json")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let cleared: Vec<&str> = resp
            .headers()
            .get_all(axum::http::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        assert!(cleared.iter().any(|c| c.starts_with("chv_session=;")
            && c.contains("Max-Age=0")
            && c.contains("HttpOnly")));
        assert!(cleared
            .iter()
            .any(|c| c.starts_with("chv_role=;") && c.contains("Max-Age=0")));
        assert!(cleared
            .iter()
            .any(|c| c.starts_with("chv_logged_in=;") && c.contains("Max-Age=0")));

        let (version,): (i64,) =
            sqlx::query_as("SELECT token_version FROM users WHERE user_id = 'u-logout'")
                .fetch_one(&state.pool)
                .await
                .unwrap();
        assert_eq!(version, 1, "logout must bump token_version");

        // The exact token that just logged out must be dead, even though its
        // JWT signature and exp are still valid.
        let (status, body) = overview_status(&app, Some(&token)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["code"], "SESSION_REVOKED");
    }

    #[tokio::test]
    async fn logout_without_a_token_still_clears_the_cookies() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let state = build_test_state().await;
        let app = crate::bff_router(state.clone()).with_state(state);

        let req = Request::builder()
            .method("POST")
            .uri("/v1/auth/logout")
            .header("content-type", "application/json")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp
            .headers()
            .get_all(axum::http::header::SET_COOKIE)
            .iter()
            .any(|v| v
                .to_str()
                .map(|c| c.starts_with("chv_session=;"))
                .unwrap_or(false)));
    }

    #[tokio::test]
    async fn bumped_token_version_rejects_and_current_version_accepts() {
        use axum::http::StatusCode;

        let state = build_test_state().await;
        seed_user(&state.pool, "u-ver", "ver", "irrelevant12", false).await;
        let app = crate::bff_router(state.clone()).with_state(state.clone());

        let current = admin_token_with_version(&state, "u-ver", false, Some(1));
        let stale = admin_token_with_version(&state, "u-ver", false, Some(0));

        // Version 0 is current before the bump.
        let (status, _) = overview_status(&app, Some(&stale)).await;
        assert_eq!(status, StatusCode::OK);

        sqlx::query("UPDATE users SET token_version = 1 WHERE user_id = 'u-ver'")
            .execute(&state.pool)
            .await
            .unwrap();

        let (status, body) = overview_status(&app, Some(&stale)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "stale version must die");
        assert_eq!(body["code"], "SESSION_REVOKED");

        let (status, _) = overview_status(&app, Some(&current)).await;
        assert_eq!(status, StatusCode::OK, "current version must authenticate");
    }

    #[tokio::test]
    async fn legacy_token_without_version_claim_is_accepted_during_rollout() {
        // Documented rollout: tokens issued before #660 have no
        // token_version. They are accepted (bounded by their 24h exp) so a
        // deploy does not log every operator out; newly issued tokens always
        // carry the claim and are checked strictly.
        use axum::http::StatusCode;

        let state = build_test_state().await;
        seed_user(&state.pool, "u-legacy", "legacy", "irrelevant12", false).await;
        let app = crate::bff_router(state.clone()).with_state(state.clone());
        let legacy = admin_token_with_version(&state, "u-legacy", false, None);

        let (status, _) = overview_status(&app, Some(&legacy)).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn versioned_token_for_missing_user_is_rejected() {
        use axum::http::StatusCode;

        let state = build_test_state().await;
        let app = crate::bff_router(state.clone()).with_state(state.clone());
        let orphan = admin_token_with_version(&state, "u-does-not-exist", false, Some(0));

        let (status, _) = overview_status(&app, Some(&orphan)).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "a versioned token with no backing user must fail closed"
        );
    }

    #[tokio::test]
    async fn legacy_token_for_missing_user_is_rejected() {
        // M3: pre-#660 tokens have no version claim, but a deleted user's
        // token must not survive on the legacy path.
        use axum::http::StatusCode;

        let state = build_test_state().await;
        let app = crate::bff_router(state.clone()).with_state(state.clone());
        let orphan = admin_token_with_version(&state, "u-deleted-legacy", false, None);

        let (status, body) = overview_status(&app, Some(&orphan)).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "a legacy token with no backing user must fail closed"
        );
        assert_eq!(body["code"], "SESSION_REVOKED");
    }

    #[tokio::test]
    async fn logout_for_deleted_user_returns_200_and_clears_cookies() {
        // M2: the account is already gone, so there is nothing to revoke.
        // Logout must still succeed and clear every cookie instead of
        // returning 500 with the browser stuck holding a stale session.
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let state = build_test_state().await;
        let app = crate::bff_router(state.clone()).with_state(state.clone());
        let orphan = admin_token_with_version(&state, "u-deleted-logout", false, None);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/auth/logout")
                    .header(
                        axum::http::header::COOKIE,
                        format!("{}={orphan}", crate::auth::SESSION_COOKIE_NAME),
                    )
                    .header("content-type", "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let cleared: Vec<&str> = resp
            .headers()
            .get_all(axum::http::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        for name in ["chv_session=;", "chv_role=;", "chv_logged_in=;"] {
            assert!(
                cleared.iter().any(|c| c.starts_with(name)),
                "logout must clear {name}, got: {cleared:?}"
            );
        }
    }

    async fn admin_update_user(
        app: &axum::Router,
        caller_token: &str,
        body: serde_json::Value,
    ) -> (axum::http::StatusCode, serde_json::Value) {
        use axum::body::Body;
        use tower::ServiceExt;

        let resp = app
            .clone()
            .oneshot(
                json_post("/v1/users/update")
                    .header("authorization", format!("Bearer {caller_token}"))
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 65536).await.unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, body)
    }

    #[tokio::test]
    async fn admin_password_reset_revokes_the_target_session() {
        // H2: the standard compromise response is an admin password reset.
        // It must bump token_version so a stolen session dies immediately.
        use axum::http::StatusCode;

        let state = build_test_state().await;
        seed_user(
            &state.pool,
            "u-admin-reset",
            "reseter",
            "irrelevant12",
            false,
        )
        .await;
        seed_user(
            &state.pool,
            "u-target-reset",
            "target",
            "irrelevant12",
            false,
        )
        .await;
        let app = crate::bff_router(state.clone()).with_state(state.clone());

        let caller = admin_token_for(&state, "u-admin-reset", false);
        let target = admin_token_for(&state, "u-target-reset", false);

        // Target token works before the reset.
        let (status, _) = overview_status(&app, Some(&target)).await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = admin_update_user(
            &app,
            &caller,
            serde_json::json!({
                "user_id": "u-target-reset",
                "password": "brand-new-password-12",
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "admin reset failed: {body}");

        let (status, body) = overview_status(&app, Some(&target)).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "the target's pre-reset session must be revoked"
        );
        assert_eq!(body["code"], "SESSION_REVOKED");
    }

    #[tokio::test]
    async fn admin_role_demotion_revokes_the_target_session() {
        // H2: a demoted account must not keep an admin-scoped token.
        use axum::http::StatusCode;

        let state = build_test_state().await;
        seed_user(
            &state.pool,
            "u-admin-demote",
            "demoter",
            "irrelevant12",
            false,
        )
        .await;
        seed_user(
            &state.pool,
            "u-target-demote",
            "demotee",
            "irrelevant12",
            false,
        )
        .await;
        let app = crate::bff_router(state.clone()).with_state(state.clone());

        let caller = admin_token_for(&state, "u-admin-demote", false);
        let target = admin_token_for(&state, "u-target-demote", false);

        let (status, _) = overview_status(&app, Some(&target)).await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = admin_update_user(
            &app,
            &caller,
            serde_json::json!({
                "user_id": "u-target-demote",
                "role": "viewer",
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "role update failed: {body}");
        assert_eq!(body["role"], "viewer");

        let (status, body) = overview_status(&app, Some(&target)).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "the demoted user's prior session must be revoked"
        );
        assert_eq!(body["code"], "SESSION_REVOKED");
    }
}
