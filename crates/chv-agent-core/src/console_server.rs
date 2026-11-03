use axum::{
    extract::{Path, State, WebSocketUpgrade},
    http::{header::SEC_WEBSOCKET_PROTOCOL, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use lru::LruCache;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

nix::ioctl_write_ptr_bad!(set_winsize, nix::libc::TIOCSWINSZ, nix::libc::winsize);

#[derive(Clone)]
pub struct ConsoleServer {
    vm_runtime: crate::vm_runtime::VmRuntime,
    jwt_secret: String,
    rate_limiter: Arc<tokio::sync::Mutex<HashMap<String, Instant>>>,
    consumed_tokens: Arc<tokio::sync::Mutex<LruCache<String, Instant>>>,
}

#[derive(serde::Deserialize)]
struct ResizeMsg {
    #[serde(rename = "type")]
    msg_type: String,
    cols: u16,
    rows: u16,
}

/// Audience bound into every console ticket by the BFF. The console server
/// rejects session tokens (or any other JWT) that do not carry exactly this
/// audience, so a stolen short-lived console token cannot be replayed
/// against the session API and a full session JWT cannot open a console.
///
/// Defined once in `chv-common`; the BFF mint path and this validator must
/// never drift (#662 review M-2).
const CONSOLE_AUDIENCE: &str = chv_common::CONSOLE_AUDIENCE;

#[derive(serde::Deserialize, serde::Serialize)]
#[allow(dead_code)]
struct Claims {
    sub: String,
    username: String,
    /// Domain-separation audience (#662): console tokens may only be
    /// accepted by the console endpoint, never as a user session token.
    aud: String,
    exp: u64,
}

impl ConsoleServer {
    pub fn new(vm_runtime: crate::vm_runtime::VmRuntime, jwt_secret: String) -> Self {
        Self {
            vm_runtime,
            jwt_secret,
            rate_limiter: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            consumed_tokens: Arc::new(tokio::sync::Mutex::new(LruCache::new(
                NonZeroUsize::new(2048).unwrap(),
            ))),
        }
    }

    pub async fn try_bind(bind: &str) -> Result<TcpListener, chv_errors::ChvError> {
        TcpListener::bind(bind)
            .await
            .map_err(|e| chv_errors::ChvError::Io {
                path: bind.to_string(),
                source: e,
            })
    }

    fn router(self) -> Router {
        // Spawn periodic cleanup of rate limiter and consumed token cache
        let rate_limiter = self.rate_limiter.clone();
        let consumed_tokens = self.consumed_tokens.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                let now = Instant::now();
                let cutoff = Duration::from_secs(300);
                let mut limits = rate_limiter.lock().await;
                limits.retain(|_, last| now.duration_since(*last) < cutoff);
                drop(limits);
                let mut tokens = consumed_tokens.lock().await;
                let expired: Vec<String> = tokens
                    .iter()
                    .filter(|(_, last)| now.duration_since(**last) >= cutoff)
                    .map(|(k, _)| k.clone())
                    .collect();
                for k in expired {
                    tokens.pop(&k);
                }
            }
        });

        Router::new()
            .route("/vms/:vm_id/console", get(Self::ws_handler))
            .with_state(self)
    }

    pub async fn run(self, listener: TcpListener) -> Result<(), chv_errors::ChvError> {
        let app = self.router();
        axum::serve(listener, app)
            .await
            .map_err(|e| chv_errors::ChvError::Internal {
                reason: format!("console server error: {}", e),
            })?;
        Ok(())
    }

    async fn check_rate_limit(
        vm_id: &str,
        rate_limiter: &Arc<tokio::sync::Mutex<HashMap<String, Instant>>>,
    ) -> Option<Response> {
        const RATE_LIMIT_SECS: u64 = 2;
        let mut limits = rate_limiter.lock().await;
        let now = Instant::now();
        if let Some(last) = limits.get(vm_id) {
            if now.duration_since(*last) < Duration::from_secs(RATE_LIMIT_SECS) {
                tracing::warn!(vm_id = %vm_id, "console connection rate limited");
                return Some(StatusCode::TOO_MANY_REQUESTS.into_response());
            }
        }
        limits.insert(vm_id.to_string(), now);
        None
    }

    async fn check_replay(
        token: &str,
        consumed_tokens: &Arc<tokio::sync::Mutex<LruCache<String, Instant>>>,
    ) -> Option<Response> {
        let mut tokens = consumed_tokens.lock().await;
        if tokens.contains(token) {
            return Some(StatusCode::UNAUTHORIZED.into_response());
        }
        tokens.put(token.to_string(), Instant::now());
        None
    }

    async fn ws_handler(
        State(state): State<ConsoleServer>,
        Path(vm_id): Path<String>,
        headers: HeaderMap,
        ws: WebSocketUpgrade,
    ) -> Response {
        const MAX_TOKEN_LEN: usize = 8192;
        const MAX_VM_ID_LEN: usize = 256;

        if vm_id.len() > MAX_VM_ID_LEN {
            tracing::warn!(vm_id_len = vm_id.len(), "vm_id too long");
            return StatusCode::UNAUTHORIZED.into_response();
        }

        // #660: the console ticket arrives as a WebSocket subprotocol, never
        // in the URL. Query strings leak through access logs, proxy logs, and
        // browser history, so `?token=` is ignored entirely — a request with
        // no subprotocol ticket is unauthenticated even if the query carries
        // one.
        let token = match console_token_from_headers(&headers) {
            Some(token) => token,
            None => {
                tracing::warn!(
                    vm_id = %vm_id,
                    "console token missing from Sec-WebSocket-Protocol"
                );
                return StatusCode::UNAUTHORIZED.into_response();
            }
        };

        if token.len() > MAX_TOKEN_LEN {
            tracing::warn!(token_len = token.len(), "console token too long");
            return StatusCode::UNAUTHORIZED.into_response();
        }

        let claims = match validate_console_token(&token, &state.jwt_secret) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "console token validation failed");
                return StatusCode::UNAUTHORIZED.into_response();
            }
        };

        if claims.sub != vm_id {
            tracing::warn!(claims_sub = %claims.sub, vm_id = %vm_id, "console token subject mismatch");
            return StatusCode::UNAUTHORIZED.into_response();
        }

        if let Some(response) = Self::check_rate_limit(&vm_id, &state.rate_limiter).await {
            return response;
        }

        if let Some(response) = Self::check_replay(&token, &state.consumed_tokens).await {
            tracing::warn!(vm_id = %vm_id, "console token replay detected");
            return response;
        }

        let vm_runtime = state.vm_runtime.clone();
        // Echo the accepted ticket as the negotiated subprotocol. Browsers
        // only deliver a 101 to the page when the server selects one of the
        // offered protocols, so this both completes the handshake and keeps
        // the credential out of the URL.
        ws.protocols([token])
            .max_message_size(64 * 1024)
            .max_frame_size(64 * 1024)
            .on_upgrade(move |socket| Self::handle_socket(socket, vm_id, vm_runtime))
    }

    async fn handle_socket(
        socket: axum::extract::ws::WebSocket,
        vm_id: String,
        vm_runtime: crate::vm_runtime::VmRuntime,
    ) {
        let Some(pty_fd) =
            Self::retry_fetch(|| vm_runtime.pty_master(&vm_id), &vm_id, "pty master").await
        else {
            return;
        };
        let (mut ws_tx, mut ws_rx) = socket.split();

        // Subscribe to live feed first, then fetch scrollback, to minimize
        // the window where PTY output between scrollback read and broadcast
        // subscription could be lost. Duplicates are acceptable for a console.
        let mut pty_rx = match Self::retry_fetch(
            || vm_runtime.pty_output_rx(&vm_id),
            &vm_id,
            "pty broadcast channel",
        )
        .await
        {
            Some(rx) => rx,
            None => {
                return;
            }
        };

        // Send scrollback history so the client sees previous console
        // output immediately on connect.
        if let Some(scrollback) = vm_runtime.pty_scrollback(&vm_id).await {
            const CHUNK_SIZE: usize = 32 * 1024;
            for chunk in scrollback.chunks(CHUNK_SIZE) {
                let msg = axum::extract::ws::Message::Binary(chunk.to_vec());
                if ws_tx.send(msg).await.is_err() {
                    drop(pty_fd);
                    return;
                }
            }
        }

        const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
        const MAX_MSG_SIZE: usize = 64 * 1024;

        let write_pty_fd = match nix::unistd::dup(&pty_fd) {
            Ok(fd) => fd,
            Err(error) => {
                tracing::warn!(%error, "failed to dup pty fd for write");
                return;
            }
        };

        // PTY broadcast → WebSocket
        let mut read_task = tokio::spawn(async move {
            loop {
                match tokio::time::timeout(IDLE_TIMEOUT, pty_rx.recv()).await {
                    Ok(Ok(data)) => {
                        let msg = axum::extract::ws::Message::Binary(data);
                        if ws_tx.send(msg).await.is_err() {
                            break;
                        }
                    }
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped))) => {
                        // The live view is lossy BY DESIGN (issue #476): a
                        // client whose TCP drain parks the WS send skips
                        // forward instead of pinning agent memory, and
                        // recovers via reconnect + scrollback replay. The
                        // skip is logged so a lagging client stays
                        // observable instead of silently missing bytes.
                        tracing::warn!(
                            vm_id = %vm_id,
                            skipped,
                            "console live view lagged; the WS client skipped ahead \
                             (reconnect replays the scrollback)"
                        );
                    }
                    Err(_) => {
                        tracing::info!("console connection idle timeout on read");
                        break;
                    }
                }
            }
        });

        // WebSocket → PTY
        let mut write_task = tokio::spawn(async move {
            // Set FD_CLOEXEC on the dup'd fd so it is not leaked to child processes
            if let Ok(flags) = nix::fcntl::fcntl(&write_pty_fd, nix::fcntl::FcntlArg::F_GETFD) {
                let new_flags =
                    nix::fcntl::FdFlag::from_bits_truncate(flags) | nix::fcntl::FdFlag::FD_CLOEXEC;
                let _ = nix::fcntl::fcntl(&write_pty_fd, nix::fcntl::FcntlArg::F_SETFD(new_flags));
            }
            let std_file = std::fs::File::from(write_pty_fd);
            let tokio_file = tokio::fs::File::from_std(std_file);
            let mut pty_writer = tokio_file;

            loop {
                let msg = match tokio::time::timeout(IDLE_TIMEOUT, ws_rx.next()).await {
                    Ok(Some(Ok(msg))) => msg,
                    Ok(Some(Err(_))) | Ok(None) => break,
                    Err(_) => {
                        tracing::info!("console connection idle timeout on write");
                        break;
                    }
                };

                match msg {
                    axum::extract::ws::Message::Text(text) => {
                        if text.len() > MAX_MSG_SIZE {
                            tracing::warn!(size = text.len(), "console text message too large");
                            break;
                        }
                        match serde_json::from_str::<ResizeMsg>(&text) {
                            Ok(resize) if resize.msg_type == "resize" => {
                                let writer_fd = pty_writer.as_raw_fd();
                                if let Err(e) = set_pty_size(writer_fd, resize.cols, resize.rows) {
                                    tracing::warn!(error = %e, "failed to set pty size");
                                }
                            }
                            _ => {
                                if pty_writer.write_all(text.as_bytes()).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    axum::extract::ws::Message::Binary(data) => {
                        if data.len() > MAX_MSG_SIZE {
                            tracing::warn!(size = data.len(), "console binary message too large");
                            break;
                        }
                        if pty_writer.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    axum::extract::ws::Message::Close(_) => break,
                    _ => {}
                }
            }
        });

        tokio::select! {
            _ = &mut read_task => {
                write_task.abort();
            },
            _ = &mut write_task => {
                read_task.abort();
            },
        }

        drop(pty_fd);
    }

    /// Retry an async operation up to 10 times with 500 ms delays.
    async fn retry_fetch<T, F, Fut>(mut fetch: F, vm_id: &str, resource_name: &str) -> Option<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Option<T>>,
    {
        for attempt in 1..=10 {
            if let Some(val) = fetch().await {
                return Some(val);
            }
            if attempt == 10 {
                tracing::warn!(vm_id = %vm_id, "no {} for vm after 10 retries", resource_name);
                return None;
            }
            tracing::debug!(vm_id = %vm_id, attempt = attempt, "{} not ready, retrying", resource_name);
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        None
    }
}

fn set_pty_size(fd: std::os::fd::RawFd, cols: u16, rows: u16) -> Result<(), nix::Error> {
    const MAX_DIM: u16 = 1000;
    if cols == 0 || cols > MAX_DIM || rows == 0 || rows > MAX_DIM {
        tracing::warn!(cols = cols, rows = rows, "invalid pty resize dimensions");
        return Ok(());
    }
    let ws = nix::libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe { set_winsize(fd, &ws).map(|_| ()) }
}

/// Extract the console ticket from the `Sec-WebSocket-Protocol` request
/// header (#660).
///
/// Browsers cannot set arbitrary headers on a WebSocket handshake, so the
/// conventional channel for a bearer credential is the subprotocol list:
/// the UI calls `new WebSocket(url, [token])`. A handshake may offer several
/// protocols (comma-separated); the first JWT-shaped candidate is returned.
/// Non-JWT protocol names (e.g. `graphql-ws`) are skipped so a client that
/// offers both still authenticates. The candidate is fully validated by
/// [`validate_console_token`] afterwards, including the `chv:console`
/// audience.
fn console_token_from_headers(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(SEC_WEBSOCKET_PROTOCOL)?.to_str().ok()?;
    raw.split(',')
        .map(str::trim)
        .find(|candidate| candidate.matches('.').count() == 2)
        .map(str::to_string)
}

/// Validate a console ticket against the given secret using HS256.
///
/// The token must carry exactly the `chv:console` audience (#660): a normal
/// session JWT (no `aud`) or an API token is rejected, so a stolen console
/// ticket cannot be replayed against the session API and a full session
/// credential cannot open a console. Returns Ok(Claims) if the token is
/// valid and not expired, Err otherwise.
fn validate_console_token(
    token: &str,
    secret: &str,
) -> Result<Claims, jsonwebtoken::errors::Error> {
    let decoding_key = jsonwebtoken::DecodingKey::from_secret(secret.as_bytes());
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
    // Console tokens must carry exactly the console audience, and `exp`,
    // `aud`, and `sub` are all mandatory. This stops a user session token
    // (even one signed with the same key) from being replayed against the
    // console endpoint, and keeps the console key useless for user-session
    // validation.
    validation.set_audience(&[CONSOLE_AUDIENCE]);
    validation.set_required_spec_claims(&["exp", "aud", "sub"]);
    jsonwebtoken::decode::<Claims>(token, &decoding_key, &validation).map(|d| d.claims)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chv_errors::ChvError;
    use chv_hypervisor_api::{AddDiskParams, AddNetParams, VmConfig, VmCounters, VmInfo};
    use std::os::fd::OwnedFd;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::io::AsyncReadExt;

    fn test_secret() -> String {
        "test-secret-do-not-use-in-production".to_string()
    }

    fn encode_claims(sub: &str, username: &str, exp: u64, secret: &str) -> String {
        encode_claims_with_aud(sub, username, CONSOLE_AUDIENCE, exp, secret)
    }

    fn encode_claims_with_aud(
        sub: &str,
        username: &str,
        aud: &str,
        exp: u64,
        secret: &str,
    ) -> String {
        let claims = Claims {
            sub: sub.to_string(),
            username: username.to_string(),
            aud: aud.to_string(),
            exp,
        };
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        jsonwebtoken::encode(
            &header,
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
        )
        .expect("encoding should succeed in tests")
    }

    fn future_exp() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600
    }

    #[test]
    fn valid_jwt_is_accepted() {
        let token = encode_claims("user-1", "admin", future_exp(), &test_secret());
        let result = validate_console_token(&token, &test_secret());
        assert!(
            result.is_ok(),
            "valid JWT should be accepted, got: {:?}",
            result.err()
        );
    }

    #[test]
    fn expired_jwt_is_rejected() {
        // exp = 1 means epoch 1 second — long expired
        let token = encode_claims("user-1", "admin", 1, &test_secret());
        let result = validate_console_token(&token, &test_secret());
        assert!(result.is_err(), "expired JWT should be rejected");
    }

    #[test]
    fn empty_token_is_rejected() {
        let result = validate_console_token("", &test_secret());
        assert!(result.is_err(), "empty token should be rejected");
    }

    #[test]
    fn malformed_token_is_rejected() {
        let result = validate_console_token("not-a-valid-jwt", &test_secret());
        assert!(result.is_err(), "malformed token should be rejected");
    }

    #[test]
    fn wrong_secret_is_rejected() {
        let token = encode_claims("user-1", "admin", future_exp(), "wrong-secret");
        let result = validate_console_token(&token, &test_secret());
        assert!(
            result.is_err(),
            "token signed with wrong secret should be rejected"
        );
    }

    #[test]
    fn non_console_audience_is_rejected() {
        // A session JWT carries a different audience (or none); only console
        // tickets are accepted.
        let token = encode_claims_with_aud(
            "user-1",
            "admin",
            "chv:session",
            future_exp(),
            &test_secret(),
        );
        let result = validate_console_token(&token, &test_secret());
        assert!(
            result.is_err(),
            "token with the wrong audience must be rejected"
        );
    }

    #[test]
    fn missing_audience_is_rejected() {
        // No `aud` claim at all must also fail (set_required_spec_claims).
        let claims = serde_json::json!({
            "sub": "user-1",
            "username": "admin",
            "exp": future_exp(),
        });
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(test_secret().as_bytes()),
        )
        .unwrap();
        assert!(validate_console_token(&token, &test_secret()).is_err());
    }

    #[test]
    fn console_audience_is_accepted() {
        let token = encode_claims_with_aud(
            "user-1",
            "admin",
            CONSOLE_AUDIENCE,
            future_exp(),
            &test_secret(),
        );
        let claims =
            validate_console_token(&token, &test_secret()).expect("console audience valid");
        assert_eq!(claims.aud, CONSOLE_AUDIENCE);
    }

    #[test]
    fn console_token_is_read_from_subprotocol_header() {
        let token = encode_claims("user-1", "admin", future_exp(), &test_secret());
        let mut headers = HeaderMap::new();
        headers.insert(
            SEC_WEBSOCKET_PROTOCOL,
            format!("graphql-ws, {token}").parse().unwrap(),
        );
        assert_eq!(console_token_from_headers(&headers), Some(token));
    }

    #[test]
    fn query_string_tokens_are_not_read() {
        // Request headers without a subprotocol never yield a token, no
        // matter what the (ignored) query string says.
        let headers = HeaderMap::new();
        assert_eq!(console_token_from_headers(&headers), None);
    }

    #[test]
    fn user_secret_does_not_validate_console_token() {
        // Console and user keys are independent (#662). A token minted
        // with the console key must not validate under the user key, and
        // a token minted with the user key must not validate under the
        // console key even when it carries the console audience.
        let console_secret = "console-secret-0123456789abcdef0123456789";
        let user_secret = "user-secret-0123456789abcdef0123456789abcd";
        let console_token = encode_claims("user-1", "admin", future_exp(), console_secret);
        assert!(validate_console_token(&console_token, console_secret).is_ok());
        assert!(validate_console_token(&console_token, user_secret).is_err());

        let user_token = encode_claims("user-1", "admin", future_exp(), user_secret);
        assert!(validate_console_token(&user_token, console_secret).is_err());
    }

    #[tokio::test]
    async fn try_bind_fails_when_port_in_use() {
        // Bind a temporary listener to occupy the port
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // try_bind should fail because the port is already occupied
        let result = ConsoleServer::try_bind(&addr.to_string()).await;
        assert!(
            result.is_err(),
            "try_bind should fail when port is already in use"
        );
        let err = result.unwrap_err();
        let err_str = format!("{}", err);
        assert!(
            err_str.contains("Address already in use") || err_str.contains("io error"),
            "error should indicate address in use, got: {}",
            err_str
        );
    }

    fn test_console_server() -> ConsoleServer {
        let adapter: Arc<dyn chv_agent_runtime_ch::CloudHypervisorAdapter> =
            Arc::new(chv_agent_runtime_ch::mock::MockCloudHypervisorAdapter::default());
        let vm_runtime = crate::vm_runtime::VmRuntime::new(adapter);
        ConsoleServer::new(vm_runtime, test_secret())
    }

    /// A raw HTTP/1.1 WebSocket upgrade request. Needed for assertions that
    /// run *after* the `WebSocketUpgrade` extractor, because `oneshot` has no
    /// `OnUpgrade` extension (it answers 426 before the handler runs). The
    /// duplex harness below carries a real hyper upgrade.
    fn raw_ws_request(path: &str, protocols: Option<&str>) -> String {
        let mut request = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: localhost\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n"
        );
        if let Some(protocol) = protocols {
            request.push_str(&format!("Sec-WebSocket-Protocol: {protocol}\r\n"));
        }
        request.push_str("\r\n");
        request
    }

    /// Serve one raw request through a real upgrade-capable hyper connection
    /// and return the HTTP status code.
    async fn status_over_duplex(request: String) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let app = test_console_server().router();
        let (mut client, server_io) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let _ =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection_with_upgrades(
                        hyper_util::rt::TokioIo::new(server_io),
                        hyper_util::service::TowerToHyperService::new(app),
                    )
                    .await;
        });

        client
            .write_all(request.as_bytes())
            .await
            .expect("write raw request");
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while client.read_exact(&mut byte).await.is_ok() {
            head.push(byte[0]);
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8_lossy(&head);
        head.lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse().ok())
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn missing_token_returns_unauthorized() {
        let status = status_over_duplex(raw_ws_request("/vms/test-vm/console", None)).await;
        assert_eq!(status, 401);
    }

    #[tokio::test]
    async fn query_string_token_is_ignored() {
        let token = encode_claims("test-vm", "admin", future_exp(), &test_secret());
        // #660: the query channel was removed; only the subprotocol is read.
        let status = status_over_duplex(raw_ws_request(
            &format!("/vms/test-vm/console?token={token}"),
            None,
        ))
        .await;
        assert_eq!(status, 401);
    }

    #[tokio::test]
    async fn valid_token_without_ws_headers_returns_bad_request() {
        use tower::ServiceExt;
        let app = test_console_server().router();
        let token = encode_claims("test-vm", "admin", future_exp(), &test_secret());
        let request = axum::http::Request::get("/vms/test-vm/console")
            .header("sec-websocket-protocol", token)
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        // WebSocketUpgrade extractor requires upgrade headers
        assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn rate_limit_blocks_rapid_requests() {
        let rate_limiter = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        // First request should pass
        assert!(ConsoleServer::check_rate_limit("vm-1", &rate_limiter)
            .await
            .is_none());
        // Immediate second request should be blocked
        assert!(
            ConsoleServer::check_rate_limit("vm-1", &rate_limiter)
                .await
                .is_some(),
            "rapid request should be rate limited"
        );
        // Different VM should pass
        assert!(ConsoleServer::check_rate_limit("vm-2", &rate_limiter)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn rate_limit_allows_after_cooldown() {
        let rate_limiter = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        assert!(ConsoleServer::check_rate_limit("vm-1", &rate_limiter)
            .await
            .is_none());
        assert!(ConsoleServer::check_rate_limit("vm-1", &rate_limiter)
            .await
            .is_some());
        // Manually expire the entry
        rate_limiter
            .lock()
            .await
            .insert("vm-1".to_string(), Instant::now() - Duration::from_secs(10));
        assert!(ConsoleServer::check_rate_limit("vm-1", &rate_limiter)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn replay_prevention_blocks_reused_token() {
        let consumed = Arc::new(tokio::sync::Mutex::new(LruCache::new(
            NonZeroUsize::new(2048).unwrap(),
        )));
        let token = "token-abc";
        // First use should pass
        assert!(ConsoleServer::check_replay(token, &consumed)
            .await
            .is_none());
        // Reuse should be blocked
        assert!(
            ConsoleServer::check_replay(token, &consumed)
                .await
                .is_some(),
            "reused token should be blocked"
        );
        // Different token should pass
        assert!(ConsoleServer::check_replay("token-def", &consumed)
            .await
            .is_none());
    }

    // ---- WS live-view lag observability (issue #476) ------------------------
    //
    // The skip/count semantics of the live view are pinned in
    // chv-agent-runtime-ch's ConsoleFanout tests; these tests pin the
    // *emission* of the WS handler's `Lagged` warn: it fires on the
    // lagging path carrying the vm_id and the skipped count, and does
    // not fire when the client keeps up.

    /// Minimal `tracing` subscriber that records WARN-level events
    /// together with all their fields, so tests can assert on the
    /// structured content of a warn (here: `vm_id` and `skipped`).
    /// Installed per-thread with `tracing::subscriber::set_default` —
    /// the warn-capture convention from chv-nwd-core's fabric tests
    /// and chv-controlplane-store's log_capture — which the
    /// current-thread `#[tokio::test]` runtime keeps visible to every
    /// task spawned by the connection under test.
    mod warn_capture {
        use std::sync::{Arc, Mutex as StdMutex};
        use tracing::field::Visit;
        use tracing::span::{Attributes, Id};
        use tracing::{Event, Metadata};

        /// One captured WARN event: every field it recorded, as
        /// `(field name, rendered value)` pairs.
        #[derive(Clone, Debug)]
        pub struct CapturedWarn {
            pub fields: Vec<(String, String)>,
        }

        impl CapturedWarn {
            pub fn field(&self, name: &str) -> Option<&str> {
                self.fields
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, v)| v.as_str())
            }

            pub fn message(&self) -> &str {
                self.field("message").unwrap_or("")
            }
        }

        #[derive(Clone, Default)]
        pub struct WarnCollector {
            warnings: Arc<StdMutex<Vec<CapturedWarn>>>,
        }

        impl WarnCollector {
            pub fn warnings(&self) -> Vec<CapturedWarn> {
                self.warnings.lock().unwrap().clone()
            }

            pub fn warnings_containing(&self, needle: &str) -> Vec<CapturedWarn> {
                self.warnings()
                    .into_iter()
                    .filter(|w| w.message().contains(needle))
                    .collect()
            }
        }

        struct FieldVisitor(Vec<(String, String)>);

        impl Visit for FieldVisitor {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0
                    .push((field.name().to_string(), format!("{:?}", value)));
            }

            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.0.push((field.name().to_string(), value.to_string()));
            }

            // `skipped` in the lag warn is a bare u64 field.
            fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                self.0.push((field.name().to_string(), value.to_string()));
            }

            fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
                self.0.push((field.name().to_string(), value.to_string()));
            }
        }

        impl tracing::Subscriber for WarnCollector {
            fn enabled(&self, metadata: &Metadata<'_>) -> bool {
                *metadata.level() == tracing::Level::WARN
            }

            fn new_span(&self, _span: &Attributes<'_>) -> Id {
                Id::from_u64(1)
            }

            fn record(&self, _span: &Id, _values: &tracing::span::Record<'_>) {}

            fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

            fn event(&self, event: &Event<'_>) {
                let mut visitor = FieldVisitor(Vec::new());
                event.record(&mut visitor);
                self.warnings
                    .lock()
                    .unwrap()
                    .push(CapturedWarn { fields: visitor.0 });
            }

            fn enter(&self, _span: &Id) {}

            fn exit(&self, _span: &Id) {}
        }
    }

    /// Test adapter pinning the PTY seams `handle_socket` consumes: a
    /// broadcast channel the test drives as the guest serial feed
    /// (capacity deliberately tiny — the handler reacts to
    /// `RecvError::Lagged`, whose semantics do not depend on the
    /// production 4096-message capacity pinned in
    /// chv-agent-runtime-ch's ConsoleFanout tests), a writable fd
    /// standing in for the PTY write side (never written to here: the
    /// test client sends no messages), and an empty scrollback. Every
    /// non-PTY trait method delegates to the stock mock.
    struct LagPtyAdapter {
        inner: chv_agent_runtime_ch::MockCloudHypervisorAdapter,
        live_tx: tokio::sync::broadcast::Sender<Vec<u8>>,
        subscribed: Arc<tokio::sync::Notify>,
        pty_fd: OwnedFd,
    }

    impl LagPtyAdapter {
        fn new(
            live_tx: tokio::sync::broadcast::Sender<Vec<u8>>,
            subscribed: Arc<tokio::sync::Notify>,
        ) -> Self {
            let pty_fd = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/null")
                .expect("open stand-in fd for the PTY write side")
                .into();
            Self {
                inner: chv_agent_runtime_ch::MockCloudHypervisorAdapter::default(),
                live_tx,
                subscribed,
                pty_fd,
            }
        }
    }

    #[async_trait]
    impl chv_agent_runtime_ch::CloudHypervisorAdapter for LagPtyAdapter {
        async fn create_vm(
            &self,
            config: &VmConfig,
            operation_id: Option<&str>,
        ) -> Result<String, ChvError> {
            self.inner.create_vm(config, operation_id).await
        }

        async fn start_vm(&self, vm_id: &str, operation_id: Option<&str>) -> Result<(), ChvError> {
            self.inner.start_vm(vm_id, operation_id).await
        }

        async fn stop_vm(
            &self,
            vm_id: &str,
            force: bool,
            operation_id: Option<&str>,
        ) -> Result<(), ChvError> {
            self.inner.stop_vm(vm_id, force, operation_id).await
        }

        async fn delete_vm(&self, vm_id: &str, operation_id: Option<&str>) -> Result<(), ChvError> {
            self.inner.delete_vm(vm_id, operation_id).await
        }

        async fn reboot_vm(&self, vm_id: &str, operation_id: Option<&str>) -> Result<(), ChvError> {
            self.inner.reboot_vm(vm_id, operation_id).await
        }

        async fn pause_vm(&self, vm_id: &str, operation_id: Option<&str>) -> Result<(), ChvError> {
            self.inner.pause_vm(vm_id, operation_id).await
        }

        async fn resume_vm(&self, vm_id: &str, operation_id: Option<&str>) -> Result<(), ChvError> {
            self.inner.resume_vm(vm_id, operation_id).await
        }

        async fn power_button(
            &self,
            vm_id: &str,
            operation_id: Option<&str>,
        ) -> Result<(), ChvError> {
            self.inner.power_button(vm_id, operation_id).await
        }

        async fn resize_vm(
            &self,
            vm_id: &str,
            cpus: Option<u32>,
            memory_bytes: Option<u64>,
            operation_id: Option<&str>,
        ) -> Result<(), ChvError> {
            self.inner
                .resize_vm(vm_id, cpus, memory_bytes, operation_id)
                .await
        }

        async fn add_disk(
            &self,
            vm_id: &str,
            params: &AddDiskParams,
            operation_id: Option<&str>,
        ) -> Result<String, ChvError> {
            self.inner.add_disk(vm_id, params, operation_id).await
        }

        async fn remove_device(
            &self,
            vm_id: &str,
            device_id: &str,
            operation_id: Option<&str>,
        ) -> Result<(), ChvError> {
            self.inner
                .remove_device(vm_id, device_id, operation_id)
                .await
        }

        async fn add_net(
            &self,
            vm_id: &str,
            params: &AddNetParams,
            operation_id: Option<&str>,
        ) -> Result<String, ChvError> {
            self.inner.add_net(vm_id, params, operation_id).await
        }

        async fn resize_disk(
            &self,
            vm_id: &str,
            disk_id: &str,
            new_size_bytes: u64,
            operation_id: Option<&str>,
        ) -> Result<(), ChvError> {
            self.inner
                .resize_disk(vm_id, disk_id, new_size_bytes, operation_id)
                .await
        }

        async fn vm_info(&self, vm_id: &str) -> Result<VmInfo, ChvError> {
            self.inner.vm_info(vm_id).await
        }

        async fn vm_counters(&self, vm_id: &str) -> Result<VmCounters, ChvError> {
            self.inner.vm_counters(vm_id).await
        }

        async fn ping(&self, vm_id: &str) -> Result<bool, ChvError> {
            self.inner.ping(vm_id).await
        }

        async fn snapshot_vm(
            &self,
            vm_id: &str,
            destination: &str,
            operation_id: Option<&str>,
        ) -> Result<(), ChvError> {
            self.inner
                .snapshot_vm(vm_id, destination, operation_id)
                .await
        }

        async fn restore_snapshot(
            &self,
            vm_id: &str,
            source: &str,
            operation_id: Option<&str>,
        ) -> Result<(), ChvError> {
            self.inner
                .restore_snapshot(vm_id, source, operation_id)
                .await
        }

        async fn send_migration(
            &self,
            vm_id: &str,
            destination_url: &str,
            operation_id: Option<&str>,
        ) -> Result<(), ChvError> {
            self.inner
                .send_migration(vm_id, destination_url, operation_id)
                .await
        }

        async fn receive_migration(
            &self,
            vm_id: &str,
            receiver_url: &str,
            operation_id: Option<&str>,
        ) -> Result<(), ChvError> {
            self.inner
                .receive_migration(vm_id, receiver_url, operation_id)
                .await
        }

        async fn get_vm_state(&self, vm_id: &str) -> Result<String, ChvError> {
            self.inner.get_vm_state(vm_id).await
        }

        async fn coredump(
            &self,
            vm_id: &str,
            destination: &str,
            operation_id: Option<&str>,
        ) -> Result<(), ChvError> {
            self.inner.coredump(vm_id, destination, operation_id).await
        }

        async fn pty_master(&self, _vm_id: &str) -> Option<OwnedFd> {
            nix::unistd::dup(&self.pty_fd).ok()
        }

        async fn pty_output_rx(
            &self,
            _vm_id: &str,
        ) -> Option<tokio::sync::broadcast::Receiver<Vec<u8>>> {
            // Subscribe first, then notify: the stored permit makes a
            // later `notified().await` in the test complete
            // immediately, and by then the receiver exists, so test
            // sends cannot race the subscription (mirrors the
            // create_parked knob in the stock mock).
            let rx = self.live_tx.subscribe();
            self.subscribed.notify_one();
            Some(rx)
        }

        async fn pty_scrollback(&self, _vm_id: &str) -> Option<Vec<u8>> {
            Some(Vec::new())
        }
    }

    /// Serves the console router over one end of an in-memory duplex
    /// pair (the same `serve_connection_with_upgrades` shape the Core
    /// API listener uses) and hands back the other end plus the
    /// adapter's controls. The duplex buffer is deliberately tiny: a
    /// WS frame larger than it parks the server's `ws_tx.send` until
    /// the test client reads, which is what makes the lag sequencing
    /// deterministic without real TCP kernel buffers.
    async fn lag_test_console() -> (
        tokio::io::DuplexStream,
        tokio::sync::broadcast::Sender<Vec<u8>>,
        Arc<tokio::sync::Notify>,
    ) {
        let (live_tx, _) = tokio::sync::broadcast::channel(8);
        let subscribed = Arc::new(tokio::sync::Notify::new());
        let adapter = LagPtyAdapter::new(live_tx.clone(), subscribed.clone());
        let vm_runtime = crate::vm_runtime::VmRuntime::new(Arc::new(adapter));
        let app = ConsoleServer::new(vm_runtime, test_secret()).router();

        let (client, server_io) = tokio::io::duplex(64);
        tokio::spawn(async move {
            // Connection errors after the test client drops are
            // expected; nothing is asserted on this task.
            let _ =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection_with_upgrades(
                        hyper_util::rt::TokioIo::new(server_io),
                        hyper_util::service::TowerToHyperService::new(app),
                    )
                    .await;
        });
        (client, live_tx, subscribed)
    }

    /// Performs the HTTP→WebSocket upgrade from the raw client end of
    /// the duplex pair (hand-rolled: no WS client dependency) and
    /// consumes the 101 head. No frame bytes can arrive before the
    /// test feeds the broadcast channel, so the head ends exactly at
    /// the blank line.
    async fn ws_upgrade_over_duplex(client: &mut tokio::io::DuplexStream, vm_id: &str) {
        use tokio::io::AsyncWriteExt;

        let token = encode_claims(vm_id, "admin", future_exp(), &test_secret());
        let request = format!(
            "GET /vms/{vm_id}/console HTTP/1.1\r\n\
             Host: localhost\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Protocol: {token}\r\n\
             \r\n"
        );
        client
            .write_all(request.as_bytes())
            .await
            .expect("write WS upgrade request");

        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            client.read_exact(&mut byte).await.expect("read 101 head");
            head.push(byte[0]);
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8_lossy(&head);
        assert!(
            head.starts_with("HTTP/1.1 101"),
            "expected 101 Switching Protocols, got: {}",
            head
        );
    }

    /// Issue #476, emission side: when the live-view broadcast skips a
    /// lagging WS client forward, the handler must emit a WARN naming
    /// the VM and the number of skipped messages (the skip is by
    /// design; the warn keeps it observable). The sequencing is
    /// deterministic: the first message's WS frame is larger than the
    /// duplex buffer, so the read loop parks inside `ws_tx.send`
    /// after consuming exactly one message; the test floods the
    /// broadcast past its capacity while the server is parked, and
    /// only then lets the frame through. The next `recv` therefore
    /// reports `Lagged(total_sent - capacity - 1)`.
    #[tokio::test]
    async fn console_live_view_lag_emits_warn_with_vm_id_and_skipped() {
        let logs = warn_capture::WarnCollector::default();
        let _subscriber = tracing::subscriber::set_default(logs.clone());

        // Fail fast instead of hanging on the 300 s idle timeout if
        // the sequencing ever regresses.
        tokio::time::timeout(Duration::from_secs(30), async {
            let vm_id = "lag-vm";
            let (mut client, live_tx, subscribed) = lag_test_console().await;
            ws_upgrade_over_duplex(&mut client, vm_id).await;

            // The handler has subscribed to the live view.
            subscribed.notified().await;

            // Message 0: its WS frame (4-byte header + 4096 payload)
            // cannot fit through the 64-byte duplex buffer, so the
            // read loop parks inside the WS send after consuming it.
            live_tx
                .send(vec![b'a'; 4096])
                .expect("send the first live message");

            // The first frame byte arriving proves the read loop
            // consumed message 0 and is parked inside its send.
            let mut first = [0u8; 1];
            client
                .read_exact(&mut first)
                .await
                .expect("read first frame byte");
            assert_eq!(first[0], 0x82, "expected the binary frame opcode");

            // Flood past the capacity of 8 while the server is
            // parked: messages 1..=20.
            for i in 1..=20u8 {
                live_tx
                    .send(vec![b'0' + i; 4])
                    .expect("flood the live view");
            }

            // Let the parked frame through. The read loop then calls
            // recv() again with its position at message 1 of 21 sent;
            // the retained window starts at 21 - 8 = 13, so exactly
            // 12 messages are reported as skipped.
            let mut rest = vec![0u8; 4099];
            client
                .read_exact(&mut rest)
                .await
                .expect("read rest of first frame");
            assert_eq!(&rest[..3], &[0x7e, 0x10, 0x00], "16-bit length 4096");
            assert!(
                rest[3..].iter().all(|&b| b == b'a'),
                "first message payload must round-trip"
            );

            // The Lagged warn fires before the retained tail is
            // forwarded; the client receiving all 8 retained frames
            // proves the loop passed through the Lagged branch.
            let mut tail = vec![0u8; 8 * 6];
            client
                .read_exact(&mut tail)
                .await
                .expect("read the retained tail frames");
            let mut expected_tail = Vec::new();
            for i in 13u8..=20 {
                expected_tail.extend_from_slice(&[
                    0x82,
                    0x04,
                    b'0' + i,
                    b'0' + i,
                    b'0' + i,
                    b'0' + i,
                ]);
            }
            assert_eq!(
                tail, expected_tail,
                "the client must resume from the retained tail"
            );

            let lagged = logs.warnings_containing("console live view lagged");
            assert_eq!(
                lagged.len(),
                1,
                "exactly one lag warn expected; captured: {:?}",
                logs.warnings()
            );
            assert_eq!(lagged[0].field("vm_id"), Some(vm_id));
            assert_eq!(lagged[0].field("skipped"), Some("12"));
        })
        .await
        .expect("lag test must complete within 30 s");
    }

    /// The counter-case: a client that keeps up with the live view
    /// receives every message and no warn is emitted.
    #[tokio::test]
    async fn console_live_view_kept_up_client_emits_no_warn() {
        let logs = warn_capture::WarnCollector::default();
        let _subscriber = tracing::subscriber::set_default(logs.clone());

        tokio::time::timeout(Duration::from_secs(30), async {
            let vm_id = "kept-up-vm";
            let (mut client, live_tx, subscribed) = lag_test_console().await;
            ws_upgrade_over_duplex(&mut client, vm_id).await;
            subscribed.notified().await;

            // A handful of messages, well under the capacity of 8.
            for i in 0..3u8 {
                live_tx.send(vec![b'0' + i; 4]).expect("send live message");
            }

            // The client receives all three frames.
            let mut received = vec![0u8; 3 * 6];
            client
                .read_exact(&mut received)
                .await
                .expect("read live frames");
            let mut expected = Vec::new();
            for i in 0..3u8 {
                expected.extend_from_slice(&[0x82, 0x04, b'0' + i, b'0' + i, b'0' + i, b'0' + i]);
            }
            assert_eq!(
                received, expected,
                "a kept-up client must receive every message"
            );

            let warnings = logs.warnings();
            assert!(
                warnings.is_empty(),
                "a kept-up client must not produce warns, got {:?}",
                warnings
            );
        })
        .await
        .expect("kept-up test must complete within 30 s");
    }
}
