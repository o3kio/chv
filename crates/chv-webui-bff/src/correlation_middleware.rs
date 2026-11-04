use axum::{
    body::Body,
    extract::Request,
    http::HeaderValue,
    middleware::Next,
    response::{IntoResponse, Response},
};

/// Maximum size in bytes that the middleware will read back from an error
/// response body in order to inject the `request_id` field. Error JSON is tiny
/// in practice; bounding this avoids ever buffering a large streamed body.
const MAX_ERR_BODY: usize = 64 * 1024;

/// Middleware that:
///
/// 1. Resolves a correlation id from the inbound `x-operation-id` header, or
///    generates a new short id if absent.
/// 2. Stores the id in the request extensions as `Option<String>` for handlers
///    to consume.
/// 3. Sets the `x-correlation-id` response header.
/// 4. For any 4xx/5xx response with `Content-Type: application/json`, reads the
///    body (bounded), parses it as a JSON object, and injects a top-level
///    `request_id` field equal to the correlation id. Bodies that are not a
///    JSON object, exceed [`MAX_ERR_BODY`], or fail to parse are passed
///    through unchanged so the content-type contract is never broken.
pub async fn extract_correlation_id(req: Request, next: Next) -> impl IntoResponse {
    let correlation_id = req
        .headers()
        .get("x-operation-id")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(chv_common::gen_resource_id);
    let mut req = req;
    req.extensions_mut().insert(Some(correlation_id.clone()));

    let response = next.run(req).await;
    let status = response.status();
    let is_error = status.is_client_error() || status.is_server_error();
    let is_json = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.starts_with("application/json"))
        .unwrap_or(false);

    let mut response = if is_error && is_json {
        inject_request_id(response, &correlation_id).await
    } else {
        response
    };

    response.headers_mut().insert(
        "x-correlation-id",
        HeaderValue::from_str(&correlation_id)
            .unwrap_or_else(|_| HeaderValue::from_static("unknown")),
    );
    response
}

/// Read the response body (up to [`MAX_ERR_BODY`]), parse it as a JSON object,
/// and insert/overwrite a top-level `request_id` field with the supplied id.
///
/// On any failure (oversized body, invalid UTF-8, non-object JSON,
/// re-serialization error) the original bytes are returned unchanged so the
/// caller still sees a coherent response. The middleware is the authority on
/// `request_id`, so any value the handler may have written is overwritten.
async fn inject_request_id(resp: Response, request_id: &str) -> Response {
    let (mut parts, body) = resp.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_ERR_BODY).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "failed to buffer error response body for request_id injection; returning empty body"
            );
            // Build a small JSON replacement that still carries the request_id
            // so the client gets a usable correlation handle. The original
            // upstream Content-Length advertises the unbuffered body's length,
            // which would now be a framing lie (clients on HTTP/1.1 hang
            // waiting for the promised bytes; HTTP/2 may RST_STREAM). We must
            // either rewrite or drop Content-Length to match the new payload.
            let replacement = serde_json::json!({
                "error": "response_body_too_large_for_correlation_injection",
                "request_id": request_id,
            });
            // serde_json::to_vec on a Value built from a json! macro is
            // infallible for these inputs, but fall back defensively.
            let new_bytes = serde_json::to_vec(&replacement).unwrap_or_else(|_| {
                b"{\"error\":\"response_body_too_large_for_correlation_injection\"}".to_vec()
            });
            if let Ok(len) = HeaderValue::from_str(&new_bytes.len().to_string()) {
                parts
                    .headers
                    .insert(axum::http::header::CONTENT_LENGTH, len);
            } else {
                parts.headers.remove(axum::http::header::CONTENT_LENGTH);
            }
            return Response::from_parts(parts, Body::from(new_bytes));
        }
    };

    let mut value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => {
            // Body is not valid JSON despite the content-type. Pass through.
            return Response::from_parts(parts, Body::from(bytes));
        }
    };

    let Some(obj) = value.as_object_mut() else {
        // JSON, but not a top-level object (e.g. an array or scalar). Pass through.
        return Response::from_parts(parts, Body::from(bytes));
    };

    obj.insert(
        "request_id".to_string(),
        serde_json::Value::String(request_id.to_string()),
    );

    let new_bytes = match serde_json::to_vec(&value) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "failed to re-serialize error body after request_id injection; passing through original"
            );
            return Response::from_parts(parts, Body::from(bytes));
        }
    };

    if let Ok(len) = HeaderValue::from_str(&new_bytes.len().to_string()) {
        parts
            .headers
            .insert(axum::http::header::CONTENT_LENGTH, len);
    } else {
        parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    }
    Response::from_parts(parts, Body::from(new_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::to_bytes,
        http::{Request as HttpRequest, StatusCode},
        middleware,
        routing::get,
        Router,
    };
    use tower::ServiceExt;

    use crate::error::BffError;

    fn app() -> Router {
        Router::new()
            .route(
                "/bad",
                get(|| async {
                    Err::<&'static str, BffError>(BffError::BadRequest("nope".into()))
                }),
            )
            .route(
                "/internal",
                get(|| async { Err::<&'static str, BffError>(BffError::Internal("boom".into())) }),
            )
            .route("/ok", get(|| async { "hello" }))
            .route(
                "/plain-error",
                get(|| async {
                    // Returns a 4xx with a non-JSON body to ensure the
                    // middleware leaves it alone.
                    (StatusCode::BAD_REQUEST, "raw text error")
                }),
            )
            .layer(middleware::from_fn(extract_correlation_id))
    }

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let (parts, body) = resp.into_parts();
        let bytes = to_bytes(body, 64 * 1024).await.unwrap();
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|e| panic!("body not JSON: {e}; bytes={bytes:?}; parts={parts:?}"))
    }

    #[tokio::test]
    async fn bad_request_body_contains_request_id_matching_header() {
        let resp = app()
            .oneshot(
                HttpRequest::builder()
                    .uri("/bad")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let header = resp
            .headers()
            .get("x-correlation-id")
            .expect("x-correlation-id header missing")
            .to_str()
            .unwrap()
            .to_string();
        let body = body_json(resp).await;
        let request_id = body
            .get("request_id")
            .and_then(|v| v.as_str())
            .expect("request_id missing in error body");
        assert_eq!(request_id, header);
        assert!(!request_id.is_empty());
        // Existing fields preserved.
        assert_eq!(
            body.get("code").and_then(|v| v.as_str()),
            Some("BAD_REQUEST")
        );
        assert_eq!(body.get("message").and_then(|v| v.as_str()), Some("nope"));
    }

    #[tokio::test]
    async fn internal_error_body_contains_request_id() {
        let resp = app()
            .oneshot(
                HttpRequest::builder()
                    .uri("/internal")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let header = resp
            .headers()
            .get("x-correlation-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let body = body_json(resp).await;
        assert_eq!(
            body.get("request_id").and_then(|v| v.as_str()),
            Some(header.as_str())
        );
        assert_eq!(
            body.get("code").and_then(|v| v.as_str()),
            Some("INTERNAL_ERROR")
        );
    }

    #[tokio::test]
    async fn success_body_is_not_mutated() {
        let resp = app()
            .oneshot(
                HttpRequest::builder()
                    .uri("/ok")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        // Header still set on success path.
        assert!(resp.headers().get("x-correlation-id").is_some());
        let (_, body) = resp.into_parts();
        let bytes = to_bytes(body, 64 * 1024).await.unwrap();
        assert_eq!(&bytes[..], b"hello");
    }

    #[tokio::test]
    async fn inbound_x_operation_id_is_propagated_to_request_id() {
        let supplied = "op-supplied-12345";
        let resp = app()
            .oneshot(
                HttpRequest::builder()
                    .uri("/bad")
                    .header("x-operation-id", supplied)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.headers()
                .get("x-correlation-id")
                .unwrap()
                .to_str()
                .unwrap(),
            supplied
        );
        let body = body_json(resp).await;
        assert_eq!(
            body.get("request_id").and_then(|v| v.as_str()),
            Some(supplied)
        );
    }

    #[tokio::test]
    async fn non_json_error_body_is_left_untouched() {
        let resp = app()
            .oneshot(
                HttpRequest::builder()
                    .uri("/plain-error")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // Header still set.
        assert!(resp.headers().get("x-correlation-id").is_some());
        let (_, body) = resp.into_parts();
        let bytes = to_bytes(body, 64 * 1024).await.unwrap();
        assert_eq!(&bytes[..], b"raw text error");
    }

    /// Regression: when the upstream JSON error body exceeds `MAX_ERR_BODY`,
    /// `axum::body::to_bytes` returns Err, and the middleware must replace the
    /// body with a small JSON payload AND recompute (or drop) Content-Length.
    /// Otherwise Content-Length still advertises the original (huge) length
    /// while the wire body is short — an HTTP framing lie.
    #[tokio::test]
    async fn oversized_error_body_does_not_lie_about_content_length() {
        use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};

        // Handler returns a 500 with an `application/json` body that
        // serializes well past MAX_ERR_BODY. A Vec<u8> serializes as a JSON
        // array of integers (each "0," is two bytes), so 200_000 elements
        // dwarfs the 64 KiB cap.
        async fn huge_json_error() -> Response {
            let payload = serde_json::json!({
                "code": "OVERSIZED",
                "data": vec![0u8; 200_000],
            });
            let body = serde_json::to_vec(&payload).unwrap();
            let mut resp = Response::new(Body::from(body.clone()));
            *resp.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            resp.headers_mut()
                .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
            resp.headers_mut().insert(
                CONTENT_LENGTH,
                HeaderValue::from_str(&body.len().to_string()).unwrap(),
            );
            resp
        }

        let app = Router::new()
            .route("/huge", get(huge_json_error))
            .layer(middleware::from_fn(extract_correlation_id));

        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/huge")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        // x-correlation-id is still attached by the outer wrapper.
        assert!(resp.headers().get("x-correlation-id").is_some());

        let advertised_len: Option<usize> = resp
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse().ok());

        let (_, body) = resp.into_parts();
        // Cap higher than the original advertised length so we can detect a lie.
        let bytes = to_bytes(body, 1024 * 1024).await.unwrap();

        // Core invariant: if Content-Length is present, it must equal the
        // actual body length. Equivalently, the middleware may drop it.
        if let Some(n) = advertised_len {
            assert_eq!(
                n,
                bytes.len(),
                "Content-Length advertised {} bytes but body is {} bytes — framing lie",
                n,
                bytes.len()
            );
        }

        // The replacement payload must still carry the request_id so the
        // client can correlate this oversized failure.
        let v: serde_json::Value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|e| panic!("replacement body is not valid JSON: {e}; bytes={bytes:?}"));
        assert!(
            v.get("request_id")
                .and_then(|x| x.as_str())
                .is_some_and(|s| !s.is_empty()),
            "replacement body missing non-empty request_id: {v}"
        );
    }
}
