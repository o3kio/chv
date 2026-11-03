//! Tonic-compatible tower [`Layer`] that records RED metrics for every
//! incoming gRPC request.
//!
//! The layer wraps the inner service and observes:
//!
//! - `chv_grpc_server_requests_total{service,method,grpc_status}` — counter
//! - `chv_grpc_server_duration_seconds{service,method,grpc_status}` — histogram
//!
//! ### Status classification
//!
//! tonic returns gRPC errors as HTTP 200 with a `grpc-status` header. Unary
//! responses carry that header in the leading HEADERS frame; streaming
//! responses terminate with `grpc-status` in HTTP/2 trailers. This layer:
//!
//! - reads `grpc-status` from the response headers when present (unary and
//!   trailers-only responses) and records immediately;
//! - otherwise wraps the response body, observes the trailer frame as it
//!   passes through, and records the terminal status when the stream ends.
//!   This does not consume or alter the body: the wrapper forwards every frame
//!   untouched and only peeks at `Frame::trailers`.
//!
//! For a streaming RPC the duration therefore covers the stream's lifetime
//! (headers through trailing status), which is the semantically correct RED
//! latency. If a body ends without trailers, the HTTP status fallback is used
//! (`unknown` for HTTP 200, `http_<code>` otherwise); a transport body error is
//! recorded as `unknown`.
//!
//! ### Service / method extraction
//!
//! tonic routes requests under `/<package>.<service>/<method>`. The path is
//! parsed by splitting on `/`. Anything else (health checks, malformed paths)
//! is bucketed as `service="unknown"` / `method="unknown"`.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use http::{HeaderMap, Request, Response};
use http_body::{Body as HttpBody, Frame, SizeHint};
use pin_project_lite::pin_project;
use tower_layer::Layer;
use tower_service::Service;

use crate::{CHV_GRPC_SERVER_DURATION_SECONDS, CHV_GRPC_SERVER_REQUESTS_TOTAL};

/// Tower layer that wraps a tonic service stack with RED-style gRPC metrics.
///
/// Apply via `Server::builder().layer(GrpcMetricsLayer::new()).add_service(..)`.
#[derive(Clone, Copy, Debug, Default)]
pub struct GrpcMetricsLayer;

impl GrpcMetricsLayer {
    /// Construct a new layer. Equivalent to [`Default::default`].
    pub const fn new() -> Self {
        Self
    }
}

impl<S> Layer<S> for GrpcMetricsLayer {
    type Service = GrpcMetricsService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        GrpcMetricsService { inner }
    }
}

/// Service produced by [`GrpcMetricsLayer`]. Records duration on every
/// response — both successful and error — without altering the response.
#[derive(Clone, Copy, Debug)]
pub struct GrpcMetricsService<S> {
    inner: S,
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for GrpcMetricsService<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>>,
    ResBody: HttpBody,
{
    type Response = Response<MetricsBody<ResBody>>;
    type Error = S::Error;
    type Future = GrpcMetricsFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        let (service_label, method_label) = parse_service_method(req.uri().path());
        let started = Instant::now();
        GrpcMetricsFuture {
            inner: self.inner.call(req),
            started,
            service_label: Some(service_label),
            method_label: Some(method_label),
        }
    }
}

pin_project! {
    /// Future returned by [`GrpcMetricsService`]. Records metrics exactly once:
    /// when the headers carry a status, at response time; otherwise when the
    /// wrapped body reaches its trailers/end.
    pub struct GrpcMetricsFuture<F> {
        #[pin]
        inner: F,
        started: Instant,
        service_label: Option<String>,
        method_label: Option<String>,
    }
}

impl<F, ResBody, E> Future for GrpcMetricsFuture<F>
where
    F: Future<Output = Result<Response<ResBody>, E>>,
    ResBody: HttpBody,
{
    type Output = Result<Response<MetricsBody<ResBody>>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let result = std::task::ready!(this.inner.poll(cx));
        // Only record metrics once; the labels are taken so a re-poll is a
        // no-op on the metrics side. Service futures should not be polled
        // after completion in practice.
        let service = this
            .service_label
            .take()
            .unwrap_or_else(|| "unknown".to_string());
        let method = this
            .method_label
            .take()
            .unwrap_or_else(|| "unknown".to_string());
        let started = *this.started;

        match result {
            Ok(response) => {
                let (parts, body) = response.into_parts();
                let http_status = parts.status.as_u16();
                let header_status = status_from_headers(&parts.headers, http_status);
                let pending = if let Some(status) = header_status {
                    record(&service, &method, &status, started.elapsed().as_secs_f64());
                    None
                } else if body.is_end_stream() {
                    // No headers status and no body to observe (empty unary
                    // response): fall back immediately. A 200 with no status
                    // anywhere is still the "unknown" bucket.
                    record(
                        &service,
                        &method,
                        &fallback_status(http_status),
                        started.elapsed().as_secs_f64(),
                    );
                    None
                } else {
                    Some(PendingMetrics {
                        service,
                        method,
                        started,
                        fallback_status: fallback_status(http_status),
                    })
                };

                let body = MetricsBody {
                    inner: body,
                    pending,
                };
                Poll::Ready(Ok(Response::from_parts(parts, body)))
            }
            Err(e) => {
                // No response headers/body to observe: transport-level error.
                record(
                    &service,
                    &method,
                    "unknown",
                    started.elapsed().as_secs_f64(),
                );
                Poll::Ready(Err(e))
            }
        }
    }
}

struct PendingMetrics {
    service: String,
    method: String,
    started: Instant,
    fallback_status: String,
}

pin_project! {
    /// Response body adapter that records the terminal gRPC status from the
    /// trailer frame. Every frame is forwarded unchanged; the body is not
    /// consumed or buffered.
    pub struct MetricsBody<B> {
        #[pin]
        inner: B,
        pending: Option<PendingMetrics>,
    }
}

impl<B> HttpBody for MetricsBody<B>
where
    B: HttpBody,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.project();
        let frame = std::task::ready!(this.inner.poll_frame(cx));

        match frame {
            Some(Ok(frame)) => {
                if let Some(trailers) = frame.trailers_ref() {
                    if let Some(pending) = this.pending.take() {
                        let status = status_from_trailers(trailers)
                            .unwrap_or_else(|| pending.fallback_status.clone());
                        record(
                            &pending.service,
                            &pending.method,
                            &status,
                            pending.started.elapsed().as_secs_f64(),
                        );
                    }
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Some(Err(e)) => {
                if let Some(pending) = this.pending.take() {
                    record(
                        &pending.service,
                        &pending.method,
                        "unknown",
                        pending.started.elapsed().as_secs_f64(),
                    );
                }
                Poll::Ready(Some(Err(e)))
            }
            None => {
                if let Some(pending) = this.pending.take() {
                    record(
                        &pending.service,
                        &pending.method,
                        &pending.fallback_status,
                        pending.started.elapsed().as_secs_f64(),
                    );
                }
                Poll::Ready(None)
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        // While a recording is pending, force one final poll so a body that
        // ends without a trailer frame still records its fallback status.
        self.inner.is_end_stream() && self.pending.is_none()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// Parse a tonic path of shape `/<package>.<service>/<method>` into
/// `(service, method)` label values. Anything that does not match this shape
/// is bucketed as `("unknown", "unknown")` so cardinality cannot explode.
fn parse_service_method(path: &str) -> (String, String) {
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    let mut parts = trimmed.splitn(2, '/');
    match (parts.next(), parts.next()) {
        (Some(service), Some(method)) if !service.is_empty() && !method.is_empty() => {
            (service.to_string(), method.to_string())
        }
        _ => ("unknown".to_string(), "unknown".to_string()),
    }
}

/// Read `grpc-status` from a header map, if present. An unparseable value is
/// surfaced as `unknown` rather than ignored (it is a protocol violation, not
/// a missing status).
fn header_grpc_status(headers: &HeaderMap) -> Option<String> {
    headers.get("grpc-status").map(|v| {
        v.to_str()
            .map(str::to_string)
            .unwrap_or_else(|_| "unknown".to_string())
    })
}

/// Status available from the response headers alone. A present `grpc-status`
/// wins; a non-200 HTTP status is a protocol-level failure surfaced under the
/// same label namespace. `None` means "defer to body trailers".
fn status_from_headers(headers: &HeaderMap, http_status: u16) -> Option<String> {
    header_grpc_status(headers).or_else(|| {
        if http_status == 200 {
            None
        } else {
            Some(format!("http_{http_status}"))
        }
    })
}

/// Status to use when neither headers nor trailers carried one.
fn fallback_status(http_status: u16) -> String {
    if http_status == 200 {
        "unknown".to_string()
    } else {
        format!("http_{http_status}")
    }
}

fn status_from_trailers(trailers: &HeaderMap) -> Option<String> {
    header_grpc_status(trailers)
}

fn record(service: &str, method: &str, grpc_status: &str, duration_secs: f64) {
    metrics::counter!(
        CHV_GRPC_SERVER_REQUESTS_TOTAL,
        "service" => service.to_string(),
        "method" => method.to_string(),
        "grpc_status" => grpc_status.to_string()
    )
    .increment(1);
    metrics::histogram!(
        CHV_GRPC_SERVER_DURATION_SECONDS,
        "service" => service.to_string(),
        "method" => method.to_string(),
        "grpc_status" => grpc_status.to_string()
    )
    .record(duration_secs);
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use bytes::Bytes;
    use http::{HeaderValue, Request, Response, StatusCode};
    use http_body_util::{BodyExt, Empty};
    use tower_layer::Layer;
    use tower_service::Service;

    use super::*;

    #[test]
    fn parse_service_method_happy_path() {
        let (s, m) = parse_service_method("/chv.controlplane.LifecycleService/CreateVm");
        assert_eq!(s, "chv.controlplane.LifecycleService");
        assert_eq!(m, "CreateVm");
    }

    #[test]
    fn parse_service_method_bad_paths_bucket_to_unknown() {
        for bad in ["", "/", "/no-slash-after", "/svc/", "//method"] {
            let (s, m) = parse_service_method(bad);
            assert_eq!(s, "unknown", "service for {bad:?}");
            assert_eq!(m, "unknown", "method for {bad:?}");
        }
    }

    #[test]
    fn status_from_headers_reads_grpc_status_header() {
        let mut headers = HeaderMap::new();
        headers.insert("grpc-status", HeaderValue::from_static("5"));
        assert_eq!(status_from_headers(&headers, 200).as_deref(), Some("5"));
    }

    #[test]
    fn status_from_headers_defers_http_200_without_header() {
        let headers = HeaderMap::new();
        assert_eq!(status_from_headers(&headers, 200), None);
        assert_eq!(
            status_from_headers(&headers, 503).as_deref(),
            Some("http_503")
        );
        assert_eq!(fallback_status(200), "unknown");
        assert_eq!(fallback_status(503), "http_503");
    }

    /// Echo service that returns a `grpc-status: 0` header — the canonical
    /// "unary OK" tonic shape.
    #[derive(Clone)]
    struct OkService;

    impl Service<Request<Empty<Bytes>>> for OkService {
        type Response = Response<Empty<Bytes>>;
        type Error = Infallible;
        type Future = std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
        >;

        fn poll_ready(
            &mut self,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, _req: Request<Empty<Bytes>>) -> Self::Future {
            Box::pin(async move {
                let mut resp = Response::new(Empty::<Bytes>::new());
                *resp.status_mut() = StatusCode::OK;
                resp.headers_mut()
                    .insert("grpc-status", HeaderValue::from_static("0"));
                Ok(resp)
            })
        }
    }

    /// Streaming-shaped service: HTTP 200, no leading `grpc-status`, terminal
    /// status delivered in a trailer frame exactly as tonic does for streaming
    /// RPCs.
    #[derive(Clone)]
    struct TrailersService {
        grpc_status: Option<&'static str>,
    }

    struct TrailersBody {
        trailers: HeaderMap,
        state: u8,
    }

    impl HttpBody for TrailersBody {
        type Data = Bytes;
        type Error = Infallible;

        fn poll_frame(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            let this = self.get_mut();
            match this.state {
                0 => {
                    this.state = 1;
                    Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(b"frame")))))
                }
                1 => {
                    this.state = 2;
                    Poll::Ready(Some(Ok(Frame::trailers(this.trailers.clone()))))
                }
                _ => Poll::Ready(None),
            }
        }
    }

    impl Service<Request<Empty<Bytes>>> for TrailersService {
        type Response = Response<TrailersBody>;
        type Error = Infallible;
        type Future = std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
        >;

        fn poll_ready(
            &mut self,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, _req: Request<Empty<Bytes>>) -> Self::Future {
            let grpc_status = self.grpc_status;
            Box::pin(async move {
                let mut trailers = HeaderMap::new();
                if let Some(status) = grpc_status {
                    trailers.insert("grpc-status", HeaderValue::from_static(status));
                }
                let resp = Response::builder()
                    .status(StatusCode::OK)
                    .body(TrailersBody { trailers, state: 0 })
                    .unwrap();
                Ok(resp)
            })
        }
    }

    fn has_series(service: &str, method: &str, grpc_status: &str) -> bool {
        let text = crate::prometheus_handle()
            .expect("prometheus recorder installed")
            .render();
        text.lines().any(|line| {
            line.starts_with("chv_grpc_server_requests_total{")
                && line.contains(&format!("service=\"{service}\""))
                && line.contains(&format!("method=\"{method}\""))
                && line.contains(&format!("grpc_status=\"{grpc_status}\""))
        })
    }

    #[tokio::test]
    async fn layer_passes_response_through_unchanged() {
        let mut svc = GrpcMetricsLayer::new().layer(OkService);
        let req = Request::builder()
            .uri("/chv.controlplane.LifecycleService/CreateVm")
            .body(Empty::<Bytes>::new())
            .unwrap();

        // poll_ready then call to drive the wrapped service end-to-end.
        futures_util::future::poll_fn(|cx| svc.poll_ready(cx))
            .await
            .unwrap();
        let resp = svc.call(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("grpc-status")
                .and_then(|v| v.to_str().ok()),
            Some("0")
        );
    }

    #[tokio::test]
    async fn completed_stream_records_trailer_status_not_unknown() {
        crate::install_prometheus_recorder().expect("install prometheus recorder");
        let mut svc = GrpcMetricsLayer::new().layer(TrailersService {
            grpc_status: Some("0"),
        });
        let req = Request::builder()
            .uri("/test.StreamingSuccessService/Watch")
            .body(Empty::<Bytes>::new())
            .unwrap();

        futures_util::future::poll_fn(|cx| svc.poll_ready(cx))
            .await
            .unwrap();
        let resp = svc.call(req).await.unwrap();
        // Drive the stream to completion so the trailer frame is observed.
        let collected = resp.into_body().collect().await.unwrap();
        assert!(
            collected.trailers().is_some(),
            "test body must deliver a trailer frame"
        );

        assert!(
            has_series("test.StreamingSuccessService", "Watch", "0"),
            "completed streaming RPC must be recorded with grpc_status=0, not unknown"
        );
        assert!(
            !has_series("test.StreamingSuccessService", "Watch", "unknown"),
            "streaming RPC must not be permanently bucketed as unknown"
        );
    }

    #[tokio::test]
    async fn errored_stream_records_trailer_status_not_unknown() {
        crate::install_prometheus_recorder().expect("install prometheus recorder");
        let mut svc = GrpcMetricsLayer::new().layer(TrailersService {
            grpc_status: Some("5"),
        });
        let req = Request::builder()
            .uri("/test.StreamingErrorService/Watch")
            .body(Empty::<Bytes>::new())
            .unwrap();

        futures_util::future::poll_fn(|cx| svc.poll_ready(cx))
            .await
            .unwrap();
        let resp = svc.call(req).await.unwrap();
        let _ = resp.into_body().collect().await.unwrap();

        assert!(
            has_series("test.StreamingErrorService", "Watch", "5"),
            "errored streaming RPC must be recorded with its terminal status"
        );
        assert!(
            !has_series("test.StreamingErrorService", "Watch", "unknown"),
            "errored streaming RPC must not be bucketed as unknown"
        );
    }

    #[tokio::test]
    async fn stream_without_status_falls_back_to_unknown() {
        crate::install_prometheus_recorder().expect("install prometheus recorder");
        let mut svc = GrpcMetricsLayer::new().layer(TrailersService { grpc_status: None });
        let req = Request::builder()
            .uri("/test.StreamingNoStatusService/Watch")
            .body(Empty::<Bytes>::new())
            .unwrap();

        futures_util::future::poll_fn(|cx| svc.poll_ready(cx))
            .await
            .unwrap();
        let resp = svc.call(req).await.unwrap();
        let _ = resp.into_body().collect().await.unwrap();

        // Truly absent status is a protocol violation; it must still be
        // recorded, as the documented fallback.
        assert!(has_series(
            "test.StreamingNoStatusService",
            "Watch",
            "unknown"
        ));
    }
}
