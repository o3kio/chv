use axum::{
    extract::{MatchedPath, Request},
    middleware::Next,
    response::Response,
};
use std::time::Instant;

/// Label used for API-prefixed requests that matched no route. A single
/// constant keeps attacker-chosen probe paths from minting histogram
/// series; the raw path is never used as a label.
pub(crate) const UNMATCHED_API_PATH: &str = "__unmatched__";

/// Axum middleware that records HTTP request metrics:
/// - `http_requests_total` counter (method, status)
/// - `http_request_duration_seconds` histogram (method, path)
///
/// Only API-shaped routes are recorded (`/v1/`, `/api/`, `/admin/`,
/// `/internal/`). The `path` label is the **matched route template**
/// from axum's [`MatchedPath`] extension (e.g. `/v1/vms/:vm_id`), not
/// the raw request URI, so dynamic segments cannot balloon cardinality.
/// API-prefixed requests that match no route (probes like `/v1/xk9z`)
/// collapse to the single [`UNMATCHED_API_PATH`] label; raw unmatched
/// segments are never exposed. Non-API paths — the static tree, health
/// probes, scrapes — are skipped entirely.
pub async fn track_metrics(req: Request, next: Next) -> Response {
    let method = req.method().to_string();
    let Some(path) = metric_path_label(
        req.extensions().get::<MatchedPath>().map(|m| m.as_str()),
        req.uri().path(),
    ) else {
        return next.run(req).await;
    };

    let start = Instant::now();

    let response = next.run(req).await;

    let status = response.status().as_u16().to_string();
    let duration = start.elapsed().as_secs_f64();

    metrics::counter!("http_requests_total", "method" => method.clone(), "status" => status)
        .increment(1);
    metrics::histogram!(
        "http_request_duration_seconds",
        "method" => method,
        "path" => path
    )
    .record(duration);

    response
}

/// Resolve the bounded `path` label for a request.
///
/// `matched` is axum's matched route template when the router matched a
/// route; `raw` is the request URI path. Returns `None` when the
/// request should not be recorded at all.
fn metric_path_label(matched: Option<&str>, raw: &str) -> Option<String> {
    match matched {
        // A matched route is already a fixed template (`:param` /
        // `*wildcard` placeholders), so it is cardinality-bounded by
        // the route table. `sanitize_path` is retained as a second
        // guard for literal id-shaped segments in a template.
        Some(route) if is_recorded_path(route) => Some(sanitize_path(route)),
        // Matched but not an API surface: health, ready, metrics, the
        // static tree. These are probes/assets, not API traffic.
        Some(_) => None,
        // Unmatched. An API-prefixed probe is recorded under one
        // constant label; everything else is skipped.
        None if is_recorded_path(raw) => Some(UNMATCHED_API_PATH.to_string()),
        None => None,
    }
}

/// Whether a path belongs to an API surface whose metrics are
/// recorded. Health/ready/metrics endpoints are deliberately excluded:
/// they are probes and scrapes, not API traffic, and counting them
/// would skew request-rate alerting.
fn is_recorded_path(path: &str) -> bool {
    path.starts_with("/v1/")
        || path.starts_with("/api/")
        || path.starts_with("/admin/")
        || path.starts_with("/internal/")
}

/// Sanitize dynamic path segments so a label stays bounded even if a
/// route template or a future caller passes a concrete path. Replaces
/// UUID-like and long hex segments with `{id}`.
fn sanitize_path(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            if is_uuid_like(segment) {
                "{id}"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn is_uuid_like(segment: &str) -> bool {
    if segment.len() == 36
        && segment.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
        && segment.matches('-').count() == 4
    {
        return true;
    }
    // Also catch short hex IDs (8+ chars) to prevent unbounded cardinality
    segment.len() >= 8 && segment.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request as HttpRequest, StatusCode},
        middleware,
        routing::get,
        Router,
    };
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    #[test]
    fn is_recorded_path_covers_only_api_surfaces() {
        for path in [
            "/v1/vms",
            "/v1/vms/abc",
            "/api/v1/backups/jobs",
            "/admin/migrations/m-1/cancel",
            "/internal/bootstrap-token",
        ] {
            assert!(is_recorded_path(path), "{path} should be recorded");
        }
        for path in [
            "/",
            "/health",
            "/ready",
            "/metrics",
            "/observability",
            "/_app/immutable/chunk.js",
            "/favicon.ico",
        ] {
            assert!(!is_recorded_path(path), "{path} should not be recorded");
        }
    }

    #[test]
    fn sanitize_path_collapses_uuid_and_hex_segments() {
        assert_eq!(
            sanitize_path("/v1/vms/550e8400-e29b-41d4-a716-446655440000/export"),
            "/v1/vms/{id}/export"
        );
        assert_eq!(
            sanitize_path("/v1/vms/deadbeef/export"),
            "/v1/vms/{id}/export"
        );
        assert_eq!(sanitize_path("/v1/vms/:vm_id"), "/v1/vms/:vm_id");
        assert_eq!(
            sanitize_path("/admin/migrations/m-1/cancel"),
            "/admin/migrations/m-1/cancel"
        );
    }

    #[test]
    fn matched_route_uses_the_template_not_the_raw_path() {
        let label = metric_path_label(Some("/v1/vms/:vm_id"), "/v1/vms/abc-123");
        assert_eq!(label.as_deref(), Some("/v1/vms/:vm_id"));

        // A matched non-API route (health, static) stays unrecorded.
        assert_eq!(metric_path_label(Some("/health"), "/health"), None);
        assert_eq!(metric_path_label(Some("/*path"), "/index.html"), None);
    }

    #[test]
    fn unmatched_api_paths_collapse_to_one_label() {
        // UUID-shaped, hex-shaped, and arbitrary attacker-chosen probes
        // all share one label: no raw segment reaches the histogram.
        for raw in [
            "/v1/xk9z",
            "/v1/vms/550e8400-e29b-41d4-a716-446655440000",
            "/v1/vms/deadbeefdeadbeef",
            "/api/v1/nope",
            "/admin/nope",
            "/internal/nope",
        ] {
            assert_eq!(
                metric_path_label(None, raw).as_deref(),
                Some(UNMATCHED_API_PATH),
                "{raw} must collapse"
            );
        }

        // Cardinality proof: a thousand distinct probes produce exactly
        // one label value.
        let labels: std::collections::HashSet<String> = (0..1_000)
            .map(|i| {
                metric_path_label(None, &format!("/v1/probe-{i}")).expect("recorded API probe")
            })
            .collect();
        assert_eq!(labels.len(), 1);
        assert!(labels.contains(UNMATCHED_API_PATH));

        // Unmatched non-API paths are not recorded.
        assert_eq!(metric_path_label(None, "/favicon.ico"), None);
        assert_eq!(metric_path_label(None, "/health"), None);
    }

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<Option<String>>>>);

    async fn capture(
        axum::extract::State(captured): axum::extract::State<Captured>,
        req: Request,
        next: Next,
    ) -> Response {
        let label = metric_path_label(
            req.extensions().get::<MatchedPath>().map(|m| m.as_str()),
            req.uri().path(),
        );
        captured.0.lock().expect("capture lock").push(label);
        next.run(req).await
    }

    #[tokio::test]
    async fn router_provides_matched_path_and_fallback_has_none() {
        let captured = Captured::default();
        let app = Router::new()
            .route("/v1/vms/:vm_id", get(|| async { "ok" }))
            .fallback(|| async { (StatusCode::NOT_FOUND, "missing") })
            .layer(middleware::from_fn_with_state(captured.clone(), capture));

        for uri in ["/v1/vms/abc", "/v1/xk9z"] {
            let response = app
                .clone()
                .oneshot(HttpRequest::get(uri).body(Body::empty()).expect("request"))
                .await
                .expect("router response");
            assert_eq!(
                response.status(),
                if uri == "/v1/vms/abc" {
                    StatusCode::OK
                } else {
                    StatusCode::NOT_FOUND
                }
            );
        }

        let labels = captured.0.lock().expect("capture lock").clone();
        assert_eq!(
            labels,
            vec![
                Some("/v1/vms/:vm_id".to_string()),
                Some(UNMATCHED_API_PATH.to_string())
            ],
            "matched routes use the template; the fallback collapses"
        );
    }
}
