use async_trait::async_trait;
use axum::{
    extract::{FromRequestParts, Request},
    http::{header::AUTHORIZATION, request::Parts, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::BffError;

/// Name of the HttpOnly session cookie set at login and accepted by the
/// bearer extractor as a fallback credential (Security S1, staged item 3.B).
/// As of #660 this is the ONLY browser credential: the JWT is never
/// returned to JavaScript or written to localStorage.
pub const SESSION_COOKIE_NAME: &str = "chv_session";

/// Non-credential UX hint cookies set alongside the session cookie.
///
/// Neither is an authentication credential: the server authorizes every
/// request from the `chv_session` JWT and never reads these. They exist so
/// the static SPA can hide controls (role) and show the login redirect
/// (logged-in hint) without script access to the HttpOnly session cookie.
/// Tampering with either one grants nothing.
pub const ROLE_COOKIE_NAME: &str = "chv_role";
pub const AUTH_HINT_COOKIE_NAME: &str = "chv_logged_in";

/// Audience bound into every session JWT (#660). The console server mints
/// tickets with a different audience (`chv:console`), so a session token
/// cannot be replayed against a console and a console ticket cannot be
/// replayed against the session API. Local to the BFF for now; sibling work
/// adds a shared `chv_common` constant and this will be unified at rebase.
pub const SESSION_AUDIENCE: &str = "chv:session";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub username: String,
    pub role: String,
    pub exp: u64,
    /// Audience claim. `None` on pre-#660 tokens; those are accepted for the
    /// documented rollout window (see `docs/OPERATIONS.md`). A token with a
    /// different audience — notably a `chv:console` ticket — is rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aud: Option<String>,
    /// True if the user must rotate their password before performing any
    /// other action. Marked `serde(default)` so JWTs issued before this
    /// field existed deserialize cleanly with `false` — no migration of
    /// outstanding tokens required.
    #[serde(default)]
    pub must_change_password: bool,
    /// Server-side session revocation counter (#660). Newly issued session
    /// tokens always carry the user's current `users.token_version`; the
    /// auth extractor compares it with the database on every request.
    ///
    /// `None` means the token predates #660 (or is an API token built from
    /// a DB row, which is already authoritative). Legacy tokens are
    /// accepted without a database lookup but cannot outlive their 24-hour
    /// `exp`; see `docs/OPERATIONS.md` for the rollout. They are skipped
    /// from serialization so a legacy token round-trips unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_version: Option<u64>,
}

impl Claims {
    /// True when this token predates the #660 `token_version` claim.
    pub fn is_legacy_session(&self) -> bool {
        self.token_version.is_none()
    }
}

/// Why a session JWT was rejected. Kept separate from
/// `jsonwebtoken::errors::Error` so callers can distinguish a malformed or
/// expired token (401) from a revoked one (401, different message) and a
/// database outage (503) without string matching.
#[derive(Debug)]
pub enum SessionAuthError {
    /// Signature, algorithm, expiry, or payload shape failed.
    Invalid(jsonwebtoken::errors::Error),
    /// The token is well-formed but its `token_version` is behind the
    /// user's current value — logout or password change revoked it.
    Revoked,
    /// The token version lookup itself failed (database outage). A
    /// missing user row is [`SessionAuthError::Revoked`], not this.
    Unavailable,
}

impl std::fmt::Display for SessionAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionAuthError::Invalid(e) => write!(f, "invalid token: {e}"),
            SessionAuthError::Revoked => write!(f, "token has been revoked"),
            SessionAuthError::Unavailable => write!(f, "token version unavailable"),
        }
    }
}

impl std::error::Error for SessionAuthError {}

/// Environment switch for the hard-close of pre-#660 session tokens.
///
/// Default `false` during rollout: legacy tokens are still accepted for
/// existing users (they expire within the 24-hour session TTL). Setting it
/// to `1`/`true`/`yes` rejects every token without a `token_version` claim
/// immediately. Operators flip it once the rollout window has passed or
/// after a suspected leak. See `docs/OPERATIONS.md`.
pub const REQUIRE_TOKEN_VERSION_ENV: &str = "CHV_REQUIRE_TOKEN_VERSION";

fn parse_env_flag(value: Option<&str>) -> bool {
    matches!(
        value.map(str::trim),
        Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
    )
}

/// True when the operator has opted into rejecting legacy session tokens.
pub fn require_token_version() -> bool {
    parse_env_flag(std::env::var(REQUIRE_TOKEN_VERSION_ENV).ok().as_deref())
}

/// Decode a session JWT and enforce server-side revocation.
///
/// Crypto and audience validation are delegated to [`validate_token`]. A
/// token with a `token_version` claim (every token issued after #660) must
/// match `users.token_version` exactly. A missing user row is rejected.
///
/// Legacy tokens without the claim are accepted only while their `sub`
/// still has a user row, unless [`require_token_version`] is enabled. That
/// keeps the rollout from logging every operator out on deploy while still
/// rejecting a deleted user's pre-upgrade token instead of trusting it
/// until `exp`. A database lookup failure is a hard `AUTH_UNAVAILABLE` —
/// auth fails closed.
pub async fn validate_session_token(
    pool: &chv_controlplane_store::StorePool,
    token: &str,
    secret: &str,
) -> Result<Claims, SessionAuthError> {
    validate_session_token_with_policy(pool, token, secret, require_token_version()).await
}

/// Policy-explicit form of [`validate_session_token`], split out so tests
/// can exercise both sides of the rollout switch without mutating process
/// environment.
pub async fn validate_session_token_with_policy(
    pool: &chv_controlplane_store::StorePool,
    token: &str,
    secret: &str,
    require_version: bool,
) -> Result<Claims, SessionAuthError> {
    let claims = validate_token(token, secret).map_err(SessionAuthError::Invalid)?;
    let repo = chv_controlplane_store::UserRepository::new(pool.clone());

    // `is_legacy_session` is the single definition of "pre-#660".
    if claims.is_legacy_session() {
        if require_version {
            tracing::warn!(
                sub = %claims.sub,
                env = REQUIRE_TOKEN_VERSION_ENV,
                "legacy session token rejected by rollout switch"
            );
            return Err(SessionAuthError::Revoked);
        }
        return match repo.token_version(&claims.sub).await {
            Ok(Some(_)) => Ok(claims),
            // Deleted user: a legacy token must not outlive the account.
            Ok(None) => Err(SessionAuthError::Revoked),
            Err(e) => {
                tracing::error!(error = %e, "legacy token user lookup failed");
                Err(SessionAuthError::Unavailable)
            }
        };
    }

    let expected = claims
        .token_version
        .expect("non-legacy session token always carries token_version");

    match repo.token_version(&claims.sub).await {
        Ok(Some(current)) if current == expected => Ok(claims),
        Ok(Some(_)) => Err(SessionAuthError::Revoked),
        // Deleted user: the token is no longer backed by an account, so it
        // is revoked rather than a transient outage.
        Ok(None) => Err(SessionAuthError::Revoked),
        Err(e) => {
            tracing::error!(error = %e, "token_version lookup failed");
            Err(SessionAuthError::Unavailable)
        }
    }
}

/// Paths a user with `must_change_password=true` is permitted to hit. Every
/// other path must be rejected with 403 until the password is rotated and a
/// fresh JWT is issued by re-login.
fn is_password_rotation_safe_path(path: &str) -> bool {
    matches!(path, "/v1/auth/change-password" | "/v1/auth/logout")
}

#[derive(Clone, Copy, Debug)]
pub enum Role {
    Viewer,
    Operator,
    Admin,
}

impl Role {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "admin" => Some(Role::Admin),
            "operator" => Some(Role::Operator),
            "viewer" => Some(Role::Viewer),
            _ => None,
        }
    }

    pub fn rank(&self) -> u8 {
        match self {
            Role::Viewer => 0,
            Role::Operator => 1,
            Role::Admin => 2,
        }
    }

    pub fn meets(&self, required: Role) -> bool {
        self.rank() >= required.rank()
    }
}

/// True if `role` carries the implicit `architecture:apply` permission used
/// by the drift heuristic in `chv-architecture-reconcile`. Admin and Operator
/// hold the permission; Viewer does not.
///
/// Centralised so all drift call sites in `handlers::architectures` agree on
/// the mapping. Co-located with `Role` so any future role addition has to
/// make a deliberate decision here.
pub fn has_apply_permission(role: &Role) -> bool {
    match role {
        Role::Admin | Role::Operator => true,
        Role::Viewer => false,
    }
}

/// Enforce `api_tokens.scope` (Security T1, hardened by #660).
///
/// Scope semantics:
///
/// - `"full"` — the token keeps the user's role unchanged.
/// - `"readonly"` — the token is demoted to the `viewer` role, regardless
///   of the user's own role: read-only endpoints only, every mutating route
///   rejects it at the role middleware.
/// - anything else (empty string, typo'd, mis-cased, or future/unknown
///   values) — the token is REJECTED. There is no fallback to the user's
///   role: an unrecognized scope must never inherit mutation authority.
///   Fail closed. (`scope` is `NOT NULL`; a hand-edited NULL row cannot be
///   read as a token at all, so it fails earlier with 401.)
fn effective_role_for_scope(scope: &str, user_role: &str) -> Option<String> {
    match scope {
        "full" => Some(user_role.to_string()),
        "readonly" => Some("viewer".to_string()),
        other => {
            tracing::warn!(
                scope = %other,
                "api_token has unrecognized scope; rejecting the token (fail closed)"
            );
            None
        }
    }
}

fn forbidden_response() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({
            "message": "insufficient permissions",
            "code": "FORBIDDEN"
        })),
    )
        .into_response()
}

fn password_change_required_response() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({
            "message": "password change required before any other action",
            "code": "PASSWORD_CHANGE_REQUIRED",
            "error": "password_change_required"
        })),
    )
        .into_response()
}

async fn role_check(claims: &Claims, required: Role, req: Request, next: Next) -> Response {
    // Industrial-grade enforcement of the must_change_password promise made
    // by install.sh: a user flagged for rotation can ONLY hit the change-
    // password endpoint (or log out). Every other request is short-circuited
    // with 403 PASSWORD_CHANGE_REQUIRED so the UI knows to route to the
    // forced-change screen.
    if claims.must_change_password && !is_password_rotation_safe_path(req.uri().path()) {
        return password_change_required_response();
    }
    let user_role = Role::parse(&claims.role).unwrap_or(Role::Viewer);
    if !user_role.meets(required) {
        return forbidden_response();
    }
    next.run(req).await
}

pub async fn viewer_middleware(
    BearerToken(claims): BearerToken,
    req: Request,
    next: Next,
) -> Response {
    role_check(&claims, Role::Viewer, req, next).await
}

pub async fn operator_middleware(
    BearerToken(claims): BearerToken,
    req: Request,
    next: Next,
) -> Response {
    role_check(&claims, Role::Operator, req, next).await
}

pub async fn admin_middleware(
    BearerToken(claims): BearerToken,
    req: Request,
    next: Next,
) -> Response {
    role_check(&claims, Role::Admin, req, next).await
}

pub fn require_operator_or_admin(claims: &Claims) -> Result<(), BffError> {
    if claims.role == "admin" || claims.role == "operator" {
        Ok(())
    } else {
        Err(BffError::Forbidden(
            "operator or admin role required".into(),
        ))
    }
}

pub fn require_admin(claims: &Claims) -> Result<(), BffError> {
    if claims.role == "admin" {
        Ok(())
    } else {
        Err(BffError::Forbidden("admin role required".into()))
    }
}

pub fn validate_token(token: &str, secret: &str) -> Result<Claims, jsonwebtoken::errors::Error> {
    let decoding_key = jsonwebtoken::DecodingKey::from_secret(secret.as_bytes());
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
    // Audience is checked manually below so pre-#660 tokens (no `aud`) keep
    // working during the documented rollout window.
    validation.validate_aud = false;
    let token_data = jsonwebtoken::decode::<Claims>(token, &decoding_key, &validation)?;
    let claims = token_data.claims;

    if let Some(aud) = claims.aud.as_deref() {
        if aud != SESSION_AUDIENCE {
            return Err(jsonwebtoken::errors::Error::from(
                jsonwebtoken::errors::ErrorKind::InvalidAudience,
            ));
        }
    }
    Ok(claims)
}

/// Parse the `chv_session` cookie value out of the request's `Cookie`
/// header. Returns `None` when the cookie is absent or malformed.
///
/// Hand-rolled instead of pulling the `cookie` crate (no new dependency):
/// the BFF sets this cookie itself and the value is a JWT (base64url and
/// `.` only — no percent-encoding), so an exact-prefix scan over
/// `;`-separated pairs is sufficient. A cookie *name* that merely shares
/// the prefix (`chv_sessionX=...`) does not match because the character
/// after the prefix must be `=`.
fn session_cookie_token(headers: &axum::http::HeaderMap) -> Option<String> {
    let raw = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for pair in raw.split(';') {
        let pair = pair.trim();
        if let Some(rest) = pair.strip_prefix(SESSION_COOKIE_NAME) {
            let rest = rest.trim_start();
            if let Some(value) = rest.strip_prefix('=') {
                let value = value.trim();
                // Some clients (and the RFC 6265 cookie grammar) wrap the
                // value in double quotes. Strip a balanced surrounding
                // pair before validating — a quoted JWT is not a token and
                // must not silently fail an otherwise-valid session.
                let value = value
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .unwrap_or(value);
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

pub struct BearerToken(pub Claims);

/// How the request proved its identity. Recorded in audit metadata as a
/// classification only; no credential material ever crosses this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthSource {
    BearerJwt,
    ApiToken,
    SessionCookie,
    /// No credential validated; the audit row carries the explicit
    /// anonymous actor rather than a forged identity.
    Anonymous,
}

impl AuthSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthSource::BearerJwt => "bearer_jwt",
            AuthSource::ApiToken => "api_token",
            AuthSource::SessionCookie => "session_cookie",
            AuthSource::Anonymous => "anonymous",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AuthFailure {
    /// HTTP status the extractor must return. Kept here (rather than
    /// inferred from the message) so the audit middleware and the
    /// authorizer cannot disagree about why a credential was rejected.
    pub status: StatusCode,
    /// Stable machine-readable code returned to clients, e.g.
    /// `SESSION_REVOKED` or `AUTH_UNAVAILABLE` (#660).
    pub code: &'static str,
    pub message: &'static str,
}

impl AuthFailure {
    fn unauthorized(code: &'static str, message: &'static str) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code,
            message,
        }
    }
}

/// The credential decode the audit middleware already performed,
/// shared with the `BearerToken` extractor through request extensions
/// (#658 review L6).
///
/// Inserted only by the audit middleware (server-side code — a client
/// cannot forge request extensions). When present, the extractor
/// reuses it verbatim, so the audit row's actor and the authorizer's
/// identity are guaranteed to agree and an API-token credential is
/// decoded (and its `last_used_at` touched) exactly once per mutating
/// request instead of twice. Absent on requests that did not pass
/// through the audit middleware (GETs, read-shaped POSTs, routers
/// built from `bff_router_unaudited` without the outer layer), where
/// the extractor decodes for itself exactly as before.
#[derive(Debug, Clone)]
pub(crate) struct VerifiedAuth(pub Result<(Claims, AuthSource), AuthFailure>);

/// Decode the same credential the `BearerToken` extractor accepts,
/// without consuming request parts. The audit middleware uses this so a
/// denied mutation is attributed to the real authenticated actor while
/// an unauthenticated denial carries the explicit anonymous marker.
///
/// This is the single credential-decoding implementation: the extractor
/// below delegates here, so JWT, session-cookie, and API-token semantics
/// cannot drift between authorization and audit. #660's session
/// revocation (`token_version`) check lives here for the same reason.
pub(crate) async fn authenticate_headers(
    headers: &axum::http::HeaderMap,
    state: &crate::router::AppState,
) -> Result<(Claims, AuthSource), AuthFailure> {
    let auth_header = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok());

    let (token, from_cookie, source) = match auth_header {
        Some(header) => {
            if !header.to_ascii_lowercase().starts_with("bearer ") {
                return Err(AuthFailure::unauthorized(
                    "UNAUTHORIZED",
                    "invalid authorization scheme",
                ));
            }
            (header[7..].to_string(), false, AuthSource::BearerJwt)
        }
        // Session-cookie fallback (Security S1, staged item 3.B). When no
        // Authorization header is present, the HttpOnly `chv_session` cookie
        // (set by /v1/auth/login) is accepted as the JWT and goes through
        // the exact same verification path (constant-time HMAC, exp check,
        // and the #660 token_version revocation check).
        //
        // Deliberate behaviors:
        // - If BOTH are present, the Authorization header wins (the cookie
        //   is not even parsed).
        // - An invalid cookie JWT is a hard 401 — we never silently fall
        //   back to an anonymous request.
        // - Cookie auth is NOT restricted to non-mutating methods: the
        //   csrf_middleware requires an un-forgeable request shape for
        //   every non-GET — `application/json` content-type or (multipart
        //   routes, #496) a non-empty `x-csrf-token` header, neither of
        //   which a cross-site HTML form can produce without a CORS
        //   preflight — and SameSite=Strict blocks the cookie from being
        //   sent on cross-site requests at all. That reasoning is the
        //   documented CSRF posture for this staged flow.
        None => match session_cookie_token(headers) {
            Some(token) => (token, true, AuthSource::SessionCookie),
            None => {
                return Err(AuthFailure::unauthorized(
                    "UNAUTHORIZED",
                    "missing authorization header",
                ))
            }
        },
    };

    // Try the session JWT first. A well-formed token with a stale
    // `token_version` is a revoked session: reject it outright (never
    // fall through to the API-token path).
    match validate_session_token(&state.pool, &token, &state.jwt_secret).await {
        Ok(claims) => return Ok((claims, source)),
        Err(SessionAuthError::Revoked) => {
            return Err(AuthFailure::unauthorized(
                "SESSION_REVOKED",
                "session has been revoked",
            ))
        }
        Err(SessionAuthError::Unavailable) => {
            return Err(AuthFailure {
                status: StatusCode::SERVICE_UNAVAILABLE,
                code: "AUTH_UNAVAILABLE",
                message: "authentication service unavailable",
            })
        }
        Err(SessionAuthError::Invalid(e)) => {
            tracing::debug!(error = %e, "JWT validation failed, checking API token");
        }
    }

    // A cookie is only ever a session JWT. An invalid cookie must not
    // be reinterpreted as an API token.
    if from_cookie {
        return Err(AuthFailure::unauthorized(
            "UNAUTHORIZED",
            "invalid or expired session cookie",
        ));
    }

    // Try API token (chv_ prefix)
    if token.starts_with("chv_") {
        let token_hash = chv_common::sha256_hex(&token);

        #[derive(sqlx::FromRow)]
        struct ApiTokenUser {
            user_id: String,
            username: String,
            role: String,
            /// Enforced per Security T1 and hardened by #660: `"full"`
            /// keeps the user's role, `"readonly"` demotes to viewer,
            /// and every unknown value rejects the token in
            /// [`effective_role_for_scope`].
            scope: String,
        }

        let result = sqlx::query_as::<_, ApiTokenUser>(
            "SELECT u.user_id, u.username, u.role, t.scope \
             FROM api_tokens t \
             JOIN users u ON t.user_id = u.user_id \
             WHERE t.token_hash = ? \
             AND (t.expires_at IS NULL OR t.expires_at > strftime('%Y-%m-%dT%H:%M:%SZ','now'))",
        )
        .bind(&token_hash)
        .fetch_optional(&state.pool)
        .await;

        match result {
            Ok(Some(row)) => {
                // Fail closed: an unrecognized scope never inherits the
                // user's role (and therefore never grants mutation).
                let Some(role) = effective_role_for_scope(&row.scope, &row.role) else {
                    return Err(AuthFailure {
                        status: StatusCode::FORBIDDEN,
                        code: "INVALID_TOKEN_SCOPE",
                        message: "api token has an unrecognized scope",
                    });
                };

                // Update last_used_at in the background (best effort)
                let pool = state.pool.clone();
                let hash = token_hash.clone();
                tokio::spawn(async move {
                    let _ = sqlx::query(
                        "UPDATE api_tokens SET last_used_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE token_hash = ?",
                    )
                    .bind(&hash)
                    .execute(&pool)
                    .await;
                });

                let claims = Claims {
                    sub: row.user_id,
                    username: row.username,
                    role,
                    // Far future expiry for API tokens — their expiry is managed by expires_at in DB
                    exp: u64::MAX / 2,
                    // API tokens bypass the must-change-password gate: they are
                    // service credentials minted explicitly by an authenticated
                    // user, not the seeded bootstrap admin row.
                    must_change_password: false,
                    // API tokens are not JWTs and carry no audience claim.
                    aud: None,
                    // API tokens are already resolved through the DB on every
                    // request; no JWT revocation counter applies.
                    token_version: None,
                };
                return Ok((claims, AuthSource::ApiToken));
            }
            Ok(None) => {
                tracing::warn!("API token not found or expired");
            }
            Err(e) => {
                tracing::error!(error = %e, "API token DB lookup failed");
            }
        }
    }

    Err(AuthFailure::unauthorized(
        "UNAUTHORIZED",
        "invalid or expired token",
    ))
}

/// Extract the raw credential a logout handler should revoke.
///
/// Prefers the `Authorization: Bearer` header (API clients, chvctl) and
/// falls back to the HttpOnly `chv_session` cookie (browser). The value is
/// not validated here — callers decode it with [`validate_token`] and only
/// bump the version when it is a genuine, unexpired token.
pub fn request_token(headers: &axum::http::HeaderMap) -> Option<String> {
    if let Some(auth) = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
    {
        let lower = auth.to_ascii_lowercase();
        if lower.starts_with("bearer ") {
            let token = auth[7..].trim();
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
    }
    session_cookie_token(headers)
}

#[async_trait]
impl FromRequestParts<crate::router::AppState> for BearerToken {
    type Rejection = (StatusCode, Json<serde_json::Value>);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &crate::router::AppState,
    ) -> Result<Self, Self::Rejection> {
        let reject = |status: StatusCode, code: &'static str, msg: &'static str| {
            (
                status,
                Json(serde_json::json!({ "message": msg, "code": code })),
            )
        };
        // Reuse the audit middleware's decode when it ran: one decode
        // per mutating request, and attribution that cannot drift from
        // authorization (see `VerifiedAuth`). Extensions are only
        // writable by server-side middleware, never by the client.
        if let Some(shared) = parts.extensions.get::<VerifiedAuth>() {
            return shared
                .0
                .clone()
                .map(|(claims, _source)| BearerToken(claims))
                .map_err(|failure| reject(failure.status, failure.code, failure.message));
        }
        let (claims, _source) = authenticate_headers(&parts.headers, state)
            .await
            .map_err(|failure| reject(failure.status, failure.code, failure.message))?;
        Ok(BearerToken(claims))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn encode_claims(claims: &Claims, secret: &str) -> String {
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        jsonwebtoken::encode(
            &header,
            claims,
            &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
        )
        .expect("encoding should succeed in tests")
    }

    fn test_secret() -> String {
        "test-secret-do-not-use-in-production".to_string()
    }

    #[test]
    fn valid_token_passes_validation() {
        let exp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let claims = Claims {
            sub: "user-1".to_string(),
            username: "admin".to_string(),
            role: "admin".to_string(),
            exp,
            must_change_password: false,
            aud: None,
            token_version: None,
        };
        let token = encode_claims(&claims, &test_secret());
        let result = validate_token(&token, &test_secret());
        assert!(result.is_ok());
        let validated = result.unwrap();
        assert_eq!(validated.sub, "user-1");
        assert_eq!(validated.username, "admin");
        assert_eq!(validated.role, "admin");
    }

    #[test]
    fn expired_token_is_rejected() {
        let claims = Claims {
            sub: "user-1".to_string(),
            username: "admin".to_string(),
            role: "admin".to_string(),
            exp: 1, // expired in 1970
            must_change_password: false,
            aud: None,
            token_version: None,
        };
        let token = encode_claims(&claims, &test_secret());
        let result = validate_token(&token, &test_secret());
        assert!(result.is_err());
    }

    #[test]
    fn empty_token_is_rejected() {
        let result = validate_token("", &test_secret());
        assert!(result.is_err());
    }

    #[test]
    fn wrong_secret_is_rejected() {
        let exp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let claims = Claims {
            sub: "user-1".to_string(),
            username: "admin".to_string(),
            role: "admin".to_string(),
            exp,
            must_change_password: false,
            aud: None,
            token_version: None,
        };
        let token = encode_claims(&claims, "wrong-secret");
        let result = validate_token(&token, &test_secret());
        assert!(result.is_err());
    }

    #[test]
    fn malformed_token_is_rejected() {
        let result = validate_token("not-a-valid-jwt", &test_secret());
        assert!(result.is_err());
    }

    #[test]
    fn sha256_hex_is_correct_length() {
        let hash = chv_common::sha256_hex("chv_test_token");
        assert_eq!(hash.len(), 64);
    }

    fn future_exp() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time must be after UNIX epoch")
            .as_secs()
            + 3600
    }

    // Forge a token by splicing the payload from `forged_claims` (signed with `secret`)
    // onto the signature of `original_claims` (also signed with `secret`). Both tokens
    // share an identical HS256 header, so concatenating the foreign payload with the
    // original signature produces a token whose recomputed HMAC will not match.
    #[test]
    fn tampered_payload_is_rejected() {
        // Attack class: payload forgery (e.g., privilege escalation by changing role).
        let secret = test_secret();
        let exp = future_exp();
        let original = Claims {
            sub: "user-1".into(),
            username: "alice".into(),
            role: "viewer".into(),
            exp,
            must_change_password: false,
            aud: None,
            token_version: None,
        };
        let forged = Claims {
            sub: "user-1".into(),
            username: "alice".into(),
            role: "admin".into(), // privilege escalation attempt
            exp,
            must_change_password: false,
            aud: None,
            token_version: None,
        };
        let original_token = encode_claims(&original, &secret);
        let forged_token = encode_claims(&forged, &secret);

        let mut original_parts = original_token.split('.');
        let original_header = original_parts.next().expect("header segment");
        let _original_payload = original_parts.next().expect("payload segment");
        let original_sig = original_parts.next().expect("signature segment");

        let mut forged_parts = forged_token.split('.');
        let _forged_header = forged_parts.next().expect("header segment");
        let forged_payload = forged_parts.next().expect("payload segment");

        // Splice: header.forged_payload.original_sig -> signature mismatch on verify.
        let tampered = format!("{}.{}.{}", original_header, forged_payload, original_sig);
        let result = validate_token(&tampered, &secret);
        assert!(
            result.is_err(),
            "tampered payload must be rejected (signature mismatch)"
        );
    }

    #[test]
    fn tampered_signature_is_rejected() {
        // Attack class: blind signature mutation.
        let secret = test_secret();
        let claims = Claims {
            sub: "user-1".into(),
            username: "alice".into(),
            role: "admin".into(),
            exp: future_exp(),
            must_change_password: false,
            aud: None,
            token_version: None,
        };
        let token = encode_claims(&claims, &secret);

        // Flip the last character of the signature segment to a different valid
        // base64url char so the structure still parses but the HMAC fails.
        let mut bytes = token.into_bytes();
        let last = bytes.last_mut().expect("non-empty token");
        *last = if *last == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(bytes).expect("ascii only");

        let result = validate_token(&tampered, &secret);
        assert!(result.is_err(), "byte-flipped signature must be rejected");
    }

    #[test]
    fn hs512_token_against_hs256_validator_is_rejected() {
        // Attack class: algorithm confusion / downgrade — a token signed with a
        // different (even if stronger) algorithm must not validate against the
        // pinned HS256 validator.
        let secret = test_secret();
        let claims = Claims {
            sub: "user-1".into(),
            username: "alice".into(),
            role: "admin".into(),
            exp: future_exp(),
            must_change_password: false,
            aud: None,
            token_version: None,
        };
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS512);
        let token = jsonwebtoken::encode(
            &header,
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
        )
        .expect("HS512 encoding should succeed");

        let result = validate_token(&token, &secret);
        assert!(
            result.is_err(),
            "HS512-signed token must be rejected by HS256-pinned validator"
        );
    }

    #[test]
    fn token_past_exp_leeway_is_rejected() {
        // Attack class: clock-skew exploitation — a token expired well past the
        // default jsonwebtoken leeway (60s) must be rejected. We use 5 minutes
        // to leave no doubt: any reasonable leeway tweak still rejects this.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time must be after UNIX epoch")
            .as_secs();
        let claims = Claims {
            sub: "user-1".into(),
            username: "alice".into(),
            role: "admin".into(),
            exp: now - 300,
            must_change_password: false,
            aud: None,
            token_version: None,
        };
        let token = encode_claims(&claims, &test_secret());
        let result = validate_token(&token, &test_secret());
        assert!(
            result.is_err(),
            "token expired 5 minutes ago must be rejected (beyond default 60s leeway)"
        );
    }

    #[test]
    fn token_with_far_future_exp_is_accepted() {
        // Mirrors the API-token codepath which uses `exp: u64::MAX / 2`.
        // Confirms an extremely-large exp does not overflow validation.
        let claims = Claims {
            sub: "api-token-user".into(),
            username: "svc".into(),
            role: "operator".into(),
            exp: u64::MAX / 2,
            must_change_password: false,
            aud: None,
            token_version: None,
        };
        let token = encode_claims(&claims, &test_secret());
        let result = validate_token(&token, &test_secret());
        let validated = result.expect("far-future exp must validate");
        assert_eq!(validated.exp, u64::MAX / 2);
        assert_eq!(validated.role, "operator");
    }

    // ── api_tokens.scope enforcement (Security T1) ────────────────────────

    #[test]
    fn full_scope_keeps_user_role() {
        assert_eq!(
            effective_role_for_scope("full", "operator").as_deref(),
            Some("operator")
        );
        assert_eq!(
            effective_role_for_scope("full", "admin").as_deref(),
            Some("admin")
        );
        assert_eq!(
            effective_role_for_scope("full", "viewer").as_deref(),
            Some("viewer")
        );
    }

    #[test]
    fn readonly_scope_demotes_to_viewer() {
        assert_eq!(
            effective_role_for_scope("readonly", "operator").as_deref(),
            Some("viewer")
        );
        assert_eq!(
            effective_role_for_scope("readonly", "admin").as_deref(),
            Some("viewer")
        );
        assert_eq!(
            effective_role_for_scope("readonly", "viewer").as_deref(),
            Some("viewer")
        );
    }

    #[test]
    fn unknown_scope_is_rejected_not_fail_open() {
        // Security T1 hardening (#660): an unrecognized scope must never
        // inherit the user's role (and therefore never grant mutation).
        assert_eq!(effective_role_for_scope("banana", "operator"), None);
        assert_eq!(effective_role_for_scope("", "admin"), None);
        assert_eq!(effective_role_for_scope("READONLY", "operator"), None);
        assert_eq!(effective_role_for_scope("read only", "operator"), None);
        assert_eq!(effective_role_for_scope("full ", "admin"), None);
    }

    // ── session cookie parsing (Security S1) ──────────────────────────────

    fn headers_with_cookie(value: &str) -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(axum::http::header::COOKIE, value.parse().unwrap());
        headers
    }

    #[test]
    fn session_cookie_parses_single_cookie() {
        let headers = headers_with_cookie("chv_session=abc.def.ghi");
        assert_eq!(
            session_cookie_token(&headers).as_deref(),
            Some("abc.def.ghi")
        );
    }

    #[test]
    fn session_cookie_parses_among_other_cookies() {
        let headers =
            headers_with_cookie("theme=dark; chv_session=abc.def.ghi; other=1; chv_sessionX=nope");
        assert_eq!(
            session_cookie_token(&headers).as_deref(),
            Some("abc.def.ghi")
        );
    }

    #[test]
    fn session_cookie_rejects_missing_or_malformed() {
        assert!(session_cookie_token(&axum::http::HeaderMap::new()).is_none());
        assert!(session_cookie_token(&headers_with_cookie("other=1")).is_none());
        // Prefix look-alike must not match.
        assert!(session_cookie_token(&headers_with_cookie("chv_sessionX=abc")).is_none());
        // Empty value is not a credential.
        assert!(session_cookie_token(&headers_with_cookie("chv_session=")).is_none());
        // Whitespace around the value is tolerated by the parser.
        assert_eq!(
            session_cookie_token(&headers_with_cookie("chv_session= abc ")).as_deref(),
            Some("abc")
        );
    }

    #[test]
    fn session_cookie_strips_surrounding_quotes() {
        // RFC 6265 allows a quoted cookie value; a quoted JWT must parse
        // to the bare token instead of failing as garbage.
        assert_eq!(
            session_cookie_token(&headers_with_cookie("chv_session=\"abc.def.ghi\"")).as_deref(),
            Some("abc.def.ghi")
        );
        // An unbalanced leading quote is left alone — no half-parse.
        assert_eq!(
            session_cookie_token(&headers_with_cookie("chv_session=\"abc.def.ghi")).as_deref(),
            Some("\"abc.def.ghi")
        );
    }

    #[test]
    fn token_with_extra_unknown_fields_is_accepted() {
        // Forward-compat: extra payload fields (e.g., `tenant_id`, `iss`) added by
        // future issuers must not break deserialization into `Claims`.
        let secret = test_secret();
        let payload = serde_json::json!({
            "sub": "user-1",
            "username": "alice",
            "role": "admin",
            "exp": future_exp(),
            "iss": "chv-test-issuer",
            "tenant_id": "tenant-42",
            "scope": ["read", "write"],
        });
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        let token = jsonwebtoken::encode(
            &header,
            &payload,
            &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
        )
        .expect("encoding json value should succeed");
        let validated = validate_token(&token, &secret).expect("unknown fields must be ignored");
        assert_eq!(validated.sub, "user-1");
        assert_eq!(validated.role, "admin");
    }

    #[test]
    fn missing_required_claim_is_rejected() {
        // Attack class: malformed/incomplete payload — a token missing the `role`
        // field must fail to deserialize into `Claims` (no defaulting to viewer).
        let secret = test_secret();
        let payload = serde_json::json!({
            "sub": "user-1",
            "username": "alice",
            // role intentionally omitted
            "exp": future_exp(),
        });
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        let token = jsonwebtoken::encode(
            &header,
            &payload,
            &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
        )
        .expect("encoding json value should succeed");
        let result = validate_token(&token, &secret);
        assert!(
            result.is_err(),
            "token missing required claim must be rejected"
        );
    }

    // ── session/console audience separation (#660 M1) ────────────────────

    #[test]
    fn session_audience_is_accepted() {
        let claims = Claims {
            sub: "user-1".into(),
            username: "alice".into(),
            role: "admin".into(),
            exp: future_exp(),
            must_change_password: false,
            aud: Some(SESSION_AUDIENCE.into()),
            token_version: Some(0),
        };
        let token = encode_claims(&claims, &test_secret());
        let validated = validate_token(&token, &test_secret()).expect("session audience valid");
        assert_eq!(validated.aud.as_deref(), Some(SESSION_AUDIENCE));
    }

    #[test]
    fn console_ticket_cannot_validate_as_a_session() {
        // A stolen console ticket carries `chv:console`; it must never be
        // accepted by the session API.
        let claims = Claims {
            sub: "user-1".into(),
            username: "alice".into(),
            role: "admin".into(),
            exp: future_exp(),
            must_change_password: false,
            aud: Some("chv:console".into()),
            token_version: Some(0),
        };
        let token = encode_claims(&claims, &test_secret());
        assert!(
            validate_token(&token, &test_secret()).is_err(),
            "console ticket must not validate as a session"
        );
    }

    #[test]
    fn legacy_token_without_aud_still_validates_cryptographically() {
        // The rollout window accepts pre-#660 tokens with no `aud`.
        let claims = Claims {
            sub: "user-1".into(),
            username: "alice".into(),
            role: "admin".into(),
            exp: future_exp(),
            must_change_password: false,
            aud: None,
            token_version: None,
        };
        let token = encode_claims(&claims, &test_secret());
        assert!(validate_token(&token, &test_secret()).is_ok());
    }

    // ── legacy-token existence lookup / hard close (#660 M3) ─────────────

    async fn test_pool() -> chv_controlplane_store::StorePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory sqlite");
        chv_controlplane_store::run_migrations(&pool, None)
            .await
            .expect("run migrations");
        pool
    }

    async fn seed_user(pool: &chv_controlplane_store::StorePool, user_id: &str, version: i64) {
        sqlx::query(
            "INSERT INTO users (user_id, username, password_hash, role, token_version) \
             VALUES (?, ?, 'x', 'admin', ?)",
        )
        .bind(user_id)
        .bind(format!("user-{user_id}"))
        .bind(version)
        .execute(pool)
        .await
        .expect("seed user");
    }

    fn signed(sub: &str, aud: Option<&str>, version: Option<u64>) -> String {
        encode_claims(
            &Claims {
                sub: sub.into(),
                username: "alice".into(),
                role: "admin".into(),
                exp: future_exp(),
                must_change_password: false,
                aud: aud.map(str::to_string),
                token_version: version,
            },
            &test_secret(),
        )
    }

    #[tokio::test]
    async fn legacy_token_for_existing_user_is_accepted() {
        let pool = test_pool().await;
        seed_user(&pool, "u-legacy", 0).await;
        let token = signed("u-legacy", None, None);
        let claims = validate_session_token_with_policy(&pool, &token, &test_secret(), false)
            .await
            .expect("legacy token for an existing user must validate");
        assert_eq!(claims.sub, "u-legacy");
    }

    #[tokio::test]
    async fn legacy_token_for_deleted_user_is_revoked() {
        let pool = test_pool().await;
        let token = signed("u-deleted", None, None);
        assert!(matches!(
            validate_session_token_with_policy(&pool, &token, &test_secret(), false).await,
            Err(SessionAuthError::Revoked)
        ));
    }

    #[tokio::test]
    async fn legacy_token_is_rejected_when_hard_close_is_enabled() {
        let pool = test_pool().await;
        seed_user(&pool, "u-legacy", 0).await;
        let token = signed("u-legacy", None, None);
        assert!(matches!(
            validate_session_token_with_policy(&pool, &token, &test_secret(), true).await,
            Err(SessionAuthError::Revoked)
        ));
    }

    #[tokio::test]
    async fn matching_token_version_is_accepted() {
        let pool = test_pool().await;
        seed_user(&pool, "u-1", 3).await;
        let token = signed("u-1", Some(SESSION_AUDIENCE), Some(3));
        let claims = validate_session_token_with_policy(&pool, &token, &test_secret(), true)
            .await
            .expect("current version must validate");
        assert_eq!(claims.sub, "u-1");
    }

    #[tokio::test]
    async fn stale_token_version_is_revoked() {
        let pool = test_pool().await;
        seed_user(&pool, "u-1", 3).await;
        let token = signed("u-1", Some(SESSION_AUDIENCE), Some(2));
        assert!(matches!(
            validate_session_token_with_policy(&pool, &token, &test_secret(), true).await,
            Err(SessionAuthError::Revoked)
        ));
    }

    #[tokio::test]
    async fn versioned_token_for_deleted_user_is_revoked() {
        let pool = test_pool().await;
        let token = signed("u-deleted", Some(SESSION_AUDIENCE), Some(0));
        assert!(matches!(
            validate_session_token_with_policy(&pool, &token, &test_secret(), false).await,
            Err(SessionAuthError::Revoked)
        ));
    }

    #[test]
    fn audit_flag_parsing_accepts_only_truthy_values() {
        assert!(parse_env_flag(Some("1")));
        assert!(parse_env_flag(Some("true")));
        assert!(parse_env_flag(Some("TRUE")));
        assert!(parse_env_flag(Some("yes")));
        assert!(!parse_env_flag(Some("0")));
        assert!(!parse_env_flag(Some("false")));
        assert!(!parse_env_flag(Some("")));
        assert!(!parse_env_flag(None));
    }
}
