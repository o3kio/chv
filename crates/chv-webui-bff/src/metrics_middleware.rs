use axum::{extract::Request, middleware::Next, response::Response};
use std::time::Instant;

/// Axum middleware that records HTTP request metrics:
/// - `http_requests_total` counter (method, status)
/// - `http_request_duration_seconds` histogram (method, path)
///
/// Only API-shaped paths are recorded (`/v1/`, `/api/`, `/admin/`,
/// `/internal/`). The middleware runs at the outermost production
/// layer, where it would otherwise also see the WebUI static tree,
/// health probes, and arbitrary 404 paths — unbounded histogram
/// cardinality (content-hashed asset names, attacker-chosen probe
/// paths) and rate skew from load-balancer probes. Skipping
/// non-API paths bounds the label set; the surfaces whose final
/// status (including audit 500 replacements) matters are all covered.
pub async fn track_metrics(req: Request, next: Next) -> Response {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    if !is_recorded_path(&path) {
        return next.run(req).await;
    }

    let start = Instant::now();

    let response = next.run(req).await;

    let status = response.status().as_u16().to_string();
    let duration = start.elapsed().as_secs_f64();

    metrics::counter!("http_requests_total", "method" => method.clone(), "status" => status)
        .increment(1);
    metrics::histogram!(
        "http_request_duration_seconds",
        "method" => method,
        "path" => sanitize_path(&path)
    )
    .record(duration);

    response
}

/// Whether a request path belongs to an API surface whose metrics are
/// recorded. Health/ready/metrics endpoints are deliberately excluded:
/// they are probes and scrapes, not API traffic, and counting them
/// would skew request-rate alerting.
fn is_recorded_path(path: &str) -> bool {
    path.starts_with("/v1/")
        || path.starts_with("/api/")
        || path.starts_with("/admin/")
        || path.starts_with("/internal/")
}

/// Sanitize dynamic path segments so histogram cardinality stays bounded.
/// Replaces UUID-like segments with `{id}`.
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
