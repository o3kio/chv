use async_trait::async_trait;
use axum::{
    extract::{FromRequestParts, Request},
    http::{header::AUTHORIZATION, request::Parts, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::BffError;

/// Name of the HttpOnly session cookie set at login and accepted by the
/// bearer extractor as a fallback credential (Security S1, staged item 3.B).
/// The UI still keeps the JWT in localStorage for now; the cookie is a
/// server-side session channel that survives page reloads without script
/// access.
pub const SESSION_COOKIE_NAME: &str = "chv_session";

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub username: String,
    pub role: String,
    pub exp: u64,
    /// True if the user must rotate their password before performing any
    /// other action. Marked `serde(default)` so JWTs issued before this
    /// field existed deserialize cleanly with `false` — no migration of
    /// outstanding tokens required.
    #[serde(default)]
    pub must_change_password: bool,
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

/// Warn-once flag for API tokens whose `scope` column holds a value outside
/// the defined set. Legacy rows (or rows written by a newer/older control
/// plane) must not lock users out — they fall back to `full` — but the
/// anomaly should surface exactly once per process instead of spamming logs
/// on every request.
static WARNED_UNKNOWN_API_TOKEN_SCOPE: AtomicBool = AtomicBool::new(false);

/// Enforce `api_tokens.scope` (Security T1).
///
/// Scope semantics:
///
/// - `"full"` — the token keeps the user's role unchanged.
/// - `"readonly"` — the token is demoted to the `viewer` role, regardless
///   of the user's own role: read-only endpoints only, every mutating route
///   rejects it at the role middleware.
/// - anything else (including `NULL` → `""` from legacy rows) — treated as
///   `"full"` (fail-safe: no lockout), with a one-time warning so the
///   anomaly is visible in logs.
fn effective_role_for_scope(scope: &str, user_role: &str) -> String {
    match scope {
        "full" => user_role.to_string(),
        "readonly" => "viewer".to_string(),
        other => {
            if !WARNED_UNKNOWN_API_TOKEN_SCOPE.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    scope = %other,
                    "api_token has unrecognized scope; treating as \"full\" (fail-safe)"
                );
            }
            user_role.to_string()
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
    validation.validate_aud = false;
    let token_data = jsonwebtoken::decode::<Claims>(token, &decoding_key, &validation)?;
    Ok(token_data.claims)
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

#[derive(Debug)]
pub(crate) struct AuthFailure {
    pub message: &'static str,
}

/// Decode the same credential the `BearerToken` extractor accepts,
/// without consuming request parts. The audit middleware uses this so a
/// denied mutation is attributed to the real authenticated actor while
/// an unauthenticated denial carries the explicit anonymous marker.
///
/// This is the single credential-decoding implementation: the extractor
/// below delegates here, so JWT, session-cookie, and API-token semantics
/// cannot drift between authorization and audit.
pub(crate) async fn authenticate_headers(
    headers: &axum::http::HeaderMap,
    state: &crate::router::AppState,
) -> Result<(Claims, AuthSource), AuthFailure> {
    let auth_header = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok());

    // Session-cookie fallback (Security S1, staged item 3.B). When no
    // Authorization header is present, the HttpOnly `chv_session` cookie
    // (set by /v1/auth/login) is accepted as the JWT and goes through
    // the exact same verification path (constant-time HMAC, exp check).
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
    let auth = match auth_header {
        Some(header) => header,
        None => {
            return match session_cookie_token(headers) {
                Some(token) => match validate_token(&token, &state.jwt_secret) {
                    Ok(claims) => Ok((claims, AuthSource::SessionCookie)),
                    Err(e) => {
                        tracing::debug!(error = %e, "session cookie JWT validation failed");
                        Err(AuthFailure {
                            message: "invalid or expired session cookie",
                        })
                    }
                },
                None => Err(AuthFailure {
                    message: "missing authorization header",
                }),
            };
        }
    };

    if !auth.to_ascii_lowercase().starts_with("bearer ") {
        return Err(AuthFailure {
            message: "invalid authorization scheme",
        });
    }
    let token = &auth[7..];

    // Try JWT first
    match validate_token(token, &state.jwt_secret) {
        Ok(claims) => return Ok((claims, AuthSource::BearerJwt)),
        Err(e) => {
            tracing::debug!(error = %e, "JWT validation failed, checking API token");
        }
    }

    // Try API token (chv_ prefix)
    if token.starts_with("chv_") {
        let token_hash = chv_common::sha256_hex(token);

        #[derive(sqlx::FromRow)]
        struct ApiTokenUser {
            user_id: String,
            username: String,
            role: String,
            /// Enforced per Security T1: `"full"` keeps the user's role,
            /// `"readonly"` demotes to viewer, unknown values are
            /// fail-safe-treated as `"full"` (see
            /// [`effective_role_for_scope`]).
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
                    // Scope enforcement: the token's role on the wire is
                    // the user's role as narrowed by api_tokens.scope.
                    role: effective_role_for_scope(&row.scope, &row.role),
                    // Far future expiry for API tokens — their expiry is managed by expires_at in DB
                    exp: u64::MAX / 2,
                    // API tokens bypass the must-change-password gate: they are
                    // service credentials minted explicitly by an authenticated
                    // user, not the seeded bootstrap admin row.
                    must_change_password: false,
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

    Err(AuthFailure {
        message: "invalid or expired token",
    })
}

#[async_trait]
impl FromRequestParts<crate::router::AppState> for BearerToken {
    type Rejection = (StatusCode, Json<serde_json::Value>);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &crate::router::AppState,
    ) -> Result<Self, Self::Rejection> {
        let reject = |msg: &'static str| {
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "message": msg, "code": "UNAUTHORIZED" })),
            )
        };
        let (claims, _source) = authenticate_headers(&parts.headers, state)
            .await
            .map_err(|failure| reject(failure.message))?;
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
        };
        let forged = Claims {
            sub: "user-1".into(),
            username: "alice".into(),
            role: "admin".into(), // privilege escalation attempt
            exp,
            must_change_password: false,
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
        assert_eq!(effective_role_for_scope("full", "operator"), "operator");
        assert_eq!(effective_role_for_scope("full", "admin"), "admin");
        assert_eq!(effective_role_for_scope("full", "viewer"), "viewer");
    }

    #[test]
    fn readonly_scope_demotes_to_viewer() {
        assert_eq!(effective_role_for_scope("readonly", "operator"), "viewer");
        assert_eq!(effective_role_for_scope("readonly", "admin"), "viewer");
        assert_eq!(effective_role_for_scope("readonly", "viewer"), "viewer");
    }

    #[test]
    fn unknown_scope_fails_safe_to_full() {
        // Fail-safe contract: an unrecognized scope must never grant MORE
        // than the user's own role, and must never lock the user out either
        // — it degrades to the user's role ("full" semantics).
        assert_eq!(effective_role_for_scope("banana", "operator"), "operator");
        assert_eq!(effective_role_for_scope("", "admin"), "admin");
        assert_eq!(effective_role_for_scope("READONLY", "operator"), "operator");
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
}
