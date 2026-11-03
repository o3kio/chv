//! Scraper-facing Prometheus metrics listener shared by the CHV daemons.
//!
//! Each daemon runs its own `/metrics` listener on a dedicated loopback port
//! (see `docs/OBSERVABILITY.md`). The listener is intentionally small:
//!
//! - it renders the process-global [`prometheus_handle`](crate::prometheus_handle)
//!   registry installed by [`init_logger`](crate::init_logger), so every
//!   counter/histogram the daemon records through the `metrics` crate is
//!   exposed;
//! - it adds real process/build series (`chv_build_info`,
//!   `process_start_time_seconds`, `chv_process_uptime_seconds`);
//! - it has no authentication of its own. Security comes from the default
//!   loopback bind. Operators scraping remotely must bind a specific
//!   management address or place an operator-managed reverse proxy in front,
//!   never a wildcard/public address (issue #657).
//!
//! The listener binds before the daemon serves traffic. A bind/config error is
//! returned to the caller so startup fails loudly instead of silently running
//! without observability.

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Prometheus text exposition content type.
const METRICS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Identity of the daemon serving the endpoint.
///
/// The build fields are passed in by the daemon (`env!("CHV_VERSION")`, …)
/// because `chv-observability` itself is not the process being described.
#[derive(Clone, Debug)]
pub struct DaemonMetricsInfo {
    pub daemon: &'static str,
    pub version: &'static str,
    pub commit: &'static str,
    pub channel: &'static str,
    /// Process start instant. Callers that want the true process start pass
    /// the instant captured at the top of `main`; the default constructor
    /// uses "now".
    started: Instant,
}

impl DaemonMetricsInfo {
    pub fn new(
        daemon: &'static str,
        version: &'static str,
        commit: &'static str,
        channel: &'static str,
    ) -> Self {
        Self {
            daemon,
            version,
            commit,
            channel,
            started: Instant::now(),
        }
    }

    /// Override the process-start instant (used by daemons that capture it at
    /// the very top of `main`).
    pub fn with_started_at(mut self, started: Instant) -> Self {
        self.started = started;
        self
    }
}

/// Resolve the listener address for a daemon.
///
/// An explicit `metrics_bind` is honored verbatim (including an invalid value,
/// which then fails at bind time — never a silent fallback). Only an unset key
/// selects the daemon's loopback default.
pub fn resolve_metrics_bind(configured: Option<&str>, default_bind: &str) -> String {
    match configured {
        Some(bind) => bind.to_string(),
        None => default_bind.to_string(),
    }
}

/// Bind the metrics listener. Callers propagate an `Err` to abort startup.
pub async fn bind_metrics_listener(bind: &str) -> std::io::Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(bind).await
}

/// Bind and spawn the metrics server.
///
/// The bind happens before this function returns, so a bad address or an
/// occupied port is a startup error. Once bound, the serving task runs for the
/// lifetime of the process and logs a terminal error if it ever stops.
pub async fn start_metrics_server(bind: &str, info: DaemonMetricsInfo) -> std::io::Result<()> {
    let listener = bind_metrics_listener(bind).await?;
    tracing::info!(
        bind = %bind,
        daemon = info.daemon,
        "metrics server listening"
    );
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, metrics_router(info)).await {
            tracing::error!(error = %e, "metrics server exited with error");
        }
    });
    Ok(())
}

/// Build the `/metrics` router. Kept public so tests (and launchers that own
/// their own listener) can exercise the exact exposition without spawning.
pub fn metrics_router(info: DaemonMetricsInfo) -> Router {
    Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(Arc::new(info))
}

async fn metrics_handler(State(info): State<Arc<DaemonMetricsInfo>>) -> Response {
    match crate::prometheus_handle() {
        Some(handle) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, METRICS_CONTENT_TYPE)],
            render_metrics(&info, handle.render()),
        )
            .into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "metrics recorder not initialized".to_string(),
        )
            .into_response(),
    }
}

/// Render the process/build preamble followed by the recorder's exposition.
fn render_metrics(info: &DaemonMetricsInfo, registry: String) -> String {
    let daemon = escape_label_value(info.daemon);
    let version = escape_label_value(info.version);
    let commit = escape_label_value(info.commit);
    let channel = escape_label_value(info.channel);
    let uptime = info.started.elapsed().as_secs_f64();
    let started_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
        - uptime;

    let mut body = String::with_capacity(registry.len() + 512);
    body.push_str(
        "# HELP chv_build_info Build and release channel of the running CHV daemon\n\
         # TYPE chv_build_info gauge\n",
    );
    body.push_str(&format!(
        "chv_build_info{{daemon=\"{daemon}\",version=\"{version}\",commit=\"{commit}\",channel=\"{channel}\"}} 1\n"
    ));
    body.push_str(
        "# HELP process_start_time_seconds Start time of the process since unix epoch in seconds\n\
         # TYPE process_start_time_seconds gauge\n",
    );
    body.push_str(&format!(
        "process_start_time_seconds{{daemon=\"{daemon}\"}} {started_unix:.3}\n"
    ));
    body.push_str(
        "# HELP chv_process_uptime_seconds Process uptime in seconds\n\
         # TYPE chv_process_uptime_seconds gauge\n",
    );
    body.push_str(&format!(
        "chv_process_uptime_seconds{{daemon=\"{daemon}\"}} {uptime:.3}\n"
    ));
    body.push_str(&registry);
    if !registry.is_empty() && !registry.ends_with('\n') {
        body.push('\n');
    }
    body
}

/// Escape a Prometheus label value (backslash, double quote, newline).
fn escape_label_value(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_info() -> DaemonMetricsInfo {
        DaemonMetricsInfo::new("chv-test", "9.9.9", "deadbeef", "test")
    }

    /// The scraper endpoint is unauthenticated by design (safety is the
    /// loopback bind) and must expose the actual recorder series — not a
    /// placeholder body.
    #[tokio::test]
    async fn metrics_endpoint_serves_registered_series_without_auth() {
        crate::install_prometheus_recorder().expect("install prometheus recorder");
        metrics::counter!(
            "chv_test_metrics_scrape_total",
            "suite" => "observability"
        )
        .increment(1);

        let app = metrics_router(test_info());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .expect("router response");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some(METRICS_CONTENT_TYPE)
        );

        let body = response
            .into_body()
            .collect()
            .await
            .expect("collect body")
            .to_bytes();
        let text = String::from_utf8(body.to_vec()).expect("utf-8 exposition");

        assert!(
            text.contains("chv_test_metrics_scrape_total"),
            "registered recorder series must be exposed, got:\n{text}"
        );
        assert!(
            text.contains(
                "chv_build_info{daemon=\"chv-test\",version=\"9.9.9\",commit=\"deadbeef\",channel=\"test\"} 1"
            ),
            "build info series missing, got:\n{text}"
        );
        assert!(
            text.contains("process_start_time_seconds{daemon=\"chv-test\"}"),
            "process start timestamp missing, got:\n{text}"
        );
        assert!(
            text.contains("chv_process_uptime_seconds{daemon=\"chv-test\"}"),
            "process uptime missing, got:\n{text}"
        );
    }

    /// Only the scrape path is exposed; no SPA/fallback body can masquerade as
    /// metrics.
    #[tokio::test]
    async fn metrics_endpoint_rejects_unknown_paths_and_methods() {
        let app = metrics_router(test_info());

        let not_found = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/not-metrics")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(not_found.status(), StatusCode::NOT_FOUND);

        let wrong_method = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/metrics")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong_method.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[test]
    fn bind_resolution_defaults_to_loopback_and_honors_explicit_values() {
        assert_eq!(
            resolve_metrics_bind(None, "127.0.0.1:9101"),
            "127.0.0.1:9101"
        );
        assert_eq!(
            resolve_metrics_bind(Some("127.0.0.1:9901"), "127.0.0.1:9101"),
            "127.0.0.1:9901"
        );
        // An explicitly empty value is a config error at bind time, never a
        // silent fallback to the default (fail-closed).
        assert_eq!(resolve_metrics_bind(Some(""), "127.0.0.1:9101"), "");
    }

    #[tokio::test]
    async fn invalid_bind_address_is_a_startup_error() {
        assert!(
            bind_metrics_listener("not-a-socket-address").await.is_err(),
            "an unparseable metrics_bind must fail closed"
        );
        assert!(
            bind_metrics_listener("").await.is_err(),
            "an empty metrics_bind must fail closed"
        );
    }

    #[test]
    fn label_values_are_escaped() {
        assert_eq!(escape_label_value("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }
}
