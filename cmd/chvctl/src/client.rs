use reqwest::Client;
use serde_json::Value;
use std::fmt;

/// The non-empty `x-csrf-token` marker the BFF's CSRF gate requires on
/// multipart requests (#496/#531; sent by [`BffClient::post_multipart`]
/// — see that method's doc comment for why a constant, not a secret).
const MULTIPART_CSRF_MARKER: &str = "chvctl";

#[derive(Debug)]
pub enum CliError {
    Http(String),
    Api {
        status: u16,
        message: String,
    },
    Parse(String),
    Io(String),
    /// A watched task reached a non-successful terminal status or the
    /// watch timed out (#372 DP6) — the API call itself succeeded.
    Task(String),
    /// A client-side usage/contract rejection that never reaches the
    /// server (e.g. the fail-closed live-migration gate, #656).
    Usage(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Http(msg) => write!(f, "HTTP error: {msg}"),
            CliError::Api { status, message } => {
                write!(f, "API error (HTTP {status}): {message}")
            }
            CliError::Parse(msg) => write!(f, "Parse error: {msg}"),
            CliError::Io(msg) => write!(f, "I/O error: {msg}"),
            CliError::Task(msg) => write!(f, "Task error: {msg}"),
            CliError::Usage(msg) => write!(f, "Usage error: {msg}"),
        }
    }
}

impl std::error::Error for CliError {}

pub struct BffClient {
    base_url: String,
    token: Option<String>,
    http: Client,
}

impl BffClient {
    pub fn new(base_url: String, token: Option<String>) -> Self {
        let http = Client::builder()
            .build()
            .expect("failed to build HTTP client");

        Self {
            base_url,
            token,
            http,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn apply_auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(ref token) = self.token {
            builder.header("Authorization", format!("Bearer {token}"))
        } else {
            builder
        }
    }

    async fn handle_response(&self, resp: reqwest::Response) -> Result<Value, CliError> {
        let status = resp.status().as_u16();
        let body = resp
            .text()
            .await
            .map_err(|e| CliError::Http(e.to_string()))?;

        if status >= 400 {
            let message = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
                .unwrap_or(body);
            return Err(CliError::Api { status, message });
        }

        if body.is_empty() {
            return Ok(Value::Null);
        }

        serde_json::from_str(&body).map_err(|e| CliError::Parse(e.to_string()))
    }

    pub async fn get(&self, path: &str) -> Result<Value, CliError> {
        let req = self.http.get(self.url(path));
        let req = self.apply_auth(req);
        let resp = req
            .send()
            .await
            .map_err(|e| CliError::Http(e.to_string()))?;
        self.handle_response(resp).await
    }

    pub async fn post(&self, path: &str, body: &Value) -> Result<Value, CliError> {
        let req = self.http.post(self.url(path)).json(body);
        let req = self.apply_auth(req);
        let resp = req
            .send()
            .await
            .map_err(|e| CliError::Http(e.to_string()))?;
        self.handle_response(resp).await
    }

    pub async fn delete(&self, path: &str) -> Result<Value, CliError> {
        let req = self.http.delete(self.url(path));
        let req = self.apply_auth(req);
        let resp = req
            .send()
            .await
            .map_err(|e| CliError::Http(e.to_string()))?;
        self.handle_response(resp).await
    }

    /// POST a multipart form (`name` text field + `file` binary field)
    /// to the BFF — the `POST /v1/vms/import` contract (#532).
    ///
    /// # The CSRF marker (a discovery, disclosed)
    ///
    /// This is chvctl's FIRST multipart request, and multipart is the
    /// one content type the BFF's CSRF gate does not admit on shape
    /// alone: `application/json` is un-forgeable cross-site (an HTML
    /// form's enctype can never produce it), but multipart IS
    /// form-native, so the gate (#496/#531) requires a non-empty
    /// `x-csrf-token` header on it — 403 `CSRF_REJECTED` without one.
    ///
    /// No CSRF mechanism existed anywhere in chvctl's client before
    /// this method: every existing mutation goes through [`post`],
    /// whose JSON content type is itself the admissible shape, so no
    /// header was ever needed. There is also no token model to mirror:
    /// the tree has no CSRF token issuance or verification (the
    /// `x-csrf-token` name appears only in the BFF's CORS allow-list,
    /// and the UI's `bffFetch` — also always-JSON — sends no such
    /// header either). The gate is presence-based by design: it never
    /// reads the value, so the marker here is a fixed non-empty
    /// constant, not a secret. If a token model is ever introduced,
    /// this is the single send-site to adopt it.
    ///
    /// The marker's soundness leans on one invariant elsewhere: the
    /// CORS layer (`build_cors_layer`) allows the header for exactly
    /// the single configured origin — never a wildcard, never a
    /// reflected one. If the CORS layer ever starts reflecting
    /// origins, this marker and the #496/#531 gate stop meaning
    /// anything (any cross-site fetch could preflight through).
    pub async fn post_multipart(
        &self,
        path: &str,
        name: &str,
        file_name: &str,
        file_bytes: Vec<u8>,
    ) -> Result<Value, CliError> {
        let file_part = reqwest::multipart::Part::bytes(file_bytes)
            .file_name(file_name.to_string())
            .mime_str("application/octet-stream")
            .expect("static mime type");
        let form = reqwest::multipart::Form::new()
            .text("name", name.to_string())
            .part("file", file_part);

        let req = self.http.post(self.url(path));
        let req = self.apply_auth(req);
        let req = req.header("x-csrf-token", MULTIPART_CSRF_MARKER);
        let resp = req
            .multipart(form)
            .send()
            .await
            .map_err(|e| CliError::Http(e.to_string()))?;
        self.handle_response(resp).await
    }
}
