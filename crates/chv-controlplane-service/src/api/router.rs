use crate::api::{auth, bootstrap, health, migrations, nodes, operations, stub};
use crate::convergence_metrics::SharedConvergenceMetrics;
use axum::{
    extract::Request,
    http::{header, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Json, Response},
    routing::{delete, get, post},
    Extension, Router,
};
use chv_webui_bff::AppState;
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};

async fn not_found_handler() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "error": {
                "code": "NOT_IMPLEMENTED",
                "message": "This endpoint is not implemented in the current control plane build.",
                "retryable": false,
                "hint": "Use the BFF-backed /v1 routes for supported UI workflows."
            }
        })),
    )
}

async fn security_headers(req: Request, next: Next) -> impl IntoResponse {
    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    headers.insert(
        "content-security-policy",
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'"
            .parse()
            .unwrap(),
    );
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    headers.insert("x-frame-options", "DENY".parse().unwrap());
    headers.insert(
        "referrer-policy",
        "strict-origin-when-cross-origin".parse().unwrap(),
    );
    response
}

// ---------------------------------------------------------------------------
// Static Web UI serving (issue #447 — decision D3 target,
// docs/DEPLOYMENT-ARCHITECTURE.md §8 D3)
// ---------------------------------------------------------------------------

/// `Cache-Control` for `index.html` — mirrors the interim nginx posture
/// (`location = /index.html` in `scripts/install.sh`'s install_nginx and
/// `packaging/nginx/chv-example.conf`): the UI shell must never be
/// cached, or an upgrade serves a stale shell that references deleted
/// hashed assets.
const WEBUI_INDEX_CACHE_CONTROL: &str = "no-cache, no-store, must-revalidate";

/// `Cache-Control` for SvelteKit's content-hashed assets — mirrors the
/// interim `location /_app/immutable/` block: the hash in the filename
/// changes with the content, so the response is permanently cacheable.
const WEBUI_IMMUTABLE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

/// Prefix of SvelteKit's immutable (content-hashed) asset tree.
const WEBUI_IMMUTABLE_PREFIX: &str = "/_app/immutable/";

/// Reserved API prefixes that keep the JSON 404 fallback even when the
/// Web UI is served (`[webui] enabled = true`): an unmatched path under
/// one of these is an API miss and must stay machine-readable JSON
/// (the `NOT_IMPLEMENTED` shape API consumers already parse), never the
/// SPA's `index.html`. The list is issue #447's, verbatim: `/v1`,
/// `/api`, `/admin`, `/health*`, `/ready`, `/internal`, `/metrics`.
/// `/health*` is a glob on the first segment (`/health`, `/health/deep`,
/// `/healthz`, …); the others match the whole first segment.
fn is_reserved_path(path: &str) -> bool {
    let first_segment = path.trim_start_matches('/').split('/').next().unwrap_or("");
    first_segment == "v1"
        || first_segment == "api"
        || first_segment == "admin"
        || first_segment == "ready"
        || first_segment == "internal"
        || first_segment == "metrics"
        || first_segment.starts_with("health")
}

fn apply_cache_control(response: &mut Response, value: &'static str) {
    if let Ok(header_value) = value.parse() {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, header_value);
    }
}

/// The static-asset fallback state (issue #447). `ServeDir` from disk,
/// deliberately NOT rust-embed: `ui/build/` is gitignored, so embedding
/// would require a build-time artifact the source tree does not carry
/// (the D3 ruling in docs/DEPLOYMENT-ARCHITECTURE.md §8).
#[derive(Clone)]
struct WebUiFallback {
    serve_dir: ServeDir,
    index_file: ServeFile,
}

/// Prefix-guarded static fallback for non-reserved paths (issue #447).
///
/// Mirrors the interim nginx posture (`try_files $uri $uri/ /index.html`
/// plus the two `Cache-Control` locations):
///
/// - `ServeDir` serves the file at `$uri`, and the directory index
///   (`$uri/` → `index.html`) for directory paths;
/// - a 404 falls back to `index.html` (the SPA leg) — the served file
///   is `index.html`, so it gets `index.html`'s no-cache header (nginx
///   re-matches `location = /index.html` on the try_files internal
///   redirect, same outcome);
/// - reserved prefixes keep the JSON 404.
///
/// Middleware posture: this handler sits on the router-level fallback,
/// OUTSIDE the BFF's and the admin routes' auth layers (the UI shell
/// must load before login) and outside the CSRF middleware — which is
/// a non-GET content-type gate on the matched API routes only, so GET
/// asset requests are unaffected either way. The router-level
/// `security_headers` layer wraps the fallback, so every served asset
/// carries the CSP/nosniff/DENY/referrer-policy set (pinned by test).
async fn webui_fallback(Extension(webui): Extension<WebUiFallback>, req: Request) -> Response {
    let path = req.uri().path().to_owned();
    if is_reserved_path(&path) {
        return not_found_handler().await.into_response();
    }

    // ServeDir handles `try_files $uri $uri/` (file, then directory
    // index); its error type is Infallible. Non-GET/HEAD methods get a
    // 405 from ServeDir itself (nginx parity: static files reject POST).
    let mut response = webui
        .serve_dir
        .clone()
        .oneshot(req)
        .await
        .expect("ServeDir is infallible")
        .into_response();

    if response.status() == StatusCode::NOT_FOUND {
        // SPA fallback — the `/index.html` leg of try_files.
        let index_request = Request::builder()
            .method(Method::GET)
            .uri("/index.html")
            .body(axum::body::Body::empty())
            .expect("static index request is always constructible");
        response = webui
            .index_file
            .clone()
            .oneshot(index_request)
            .await
            .expect("ServeFile is infallible")
            .into_response();
        apply_cache_control(&mut response, WEBUI_INDEX_CACHE_CONTROL);
        return response;
    }

    // nginx parity: only the shell (`/` directory index and a direct
    // `/index.html`) and the immutable asset tree carry Cache-Control;
    // every other asset gets none (the edge may add its own).
    if path == "/" || path == "/index.html" {
        apply_cache_control(&mut response, WEBUI_INDEX_CACHE_CONTROL);
    } else if path.starts_with(WEBUI_IMMUTABLE_PREFIX) {
        apply_cache_control(&mut response, WEBUI_IMMUTABLE_CACHE_CONTROL);
    }
    response
}

pub fn admin_router(
    bff_state: AppState,
    convergence_metrics: SharedConvergenceMetrics,
    webui: chv_config::WebUiConfig,
) -> Router {
    admin_router_with_guest_agents(bff_state, convergence_metrics, webui, None, None)
}

/// The full admin router, optionally including the guest monitoring
/// agent surfaces (ADR-026, G3):
/// - `agent_browser`: the browser-authenticated pair (viewer-tier
///   inventory + operator-tier lifecycle actions), role-gated with the
///   same middlewares as their BFF siblings and CSRF-protected like
///   every other JSON-mutating browser route;
/// - `agent_ingest`: the certificate-authenticated guest transport
///   (NO session auth — mounted only when the listener serves TLS;
///   its identity is the TLS client credential plus the registry
///   binding).
pub fn admin_router_with_guest_agents(
    bff_state: AppState,
    convergence_metrics: SharedConvergenceMetrics,
    webui: chv_config::WebUiConfig,
    agent_browser: Option<(Router<AppState>, Router<AppState>)>,
    agent_ingest: Option<Router<AppState>>,
) -> Router {
    // The BFF's /v1 surface WITHOUT its own audit/metrics/correlation
    // wrappers: this router applies them at the very end so they wrap
    // every route below AND the fallback — the extra privileged
    // surfaces (admin, legacy /api/v1, /internal, guest-agent browser
    // routes) and unmatched non-safe paths included — exactly once
    // (#658 review H1/H2).
    let bff_router = chv_webui_bff::bff_router_unaudited(bff_state.clone());

    // Legacy cookie-authenticated backup routes. Mounted on the outer
    // router (not the BFF's /v1 surface) for the stage-1 UI, they accept
    // the `chv_session` session cookie as a credential — so they get the
    // same CSRF middleware the BFF surface runs: non-GET requests must
    // carry an un-forgeable request shape — `Content-Type:
    // application/json`, or (multipart routes, #496) a non-empty
    // `x-csrf-token` header — neither of which cross-site HTML forms
    // can produce (review finding: these routes previously had no CSRF
    // protection at all).
    // Backup/restore is HARD-FENCED (#654): every legacy route answers 501
    // (`NOT_IMPLEMENTED`) instead of queuing work no engine can execute.
    // Authentication runs first (401), then CSRF (415 for a form-native POST),
    // then the fence (501).
    let legacy_backup_routes = Router::new()
        .route(
            "/api/v1/backup-jobs",
            get(chv_webui_bff::handlers::backups::backup_fence)
                .post(chv_webui_bff::handlers::backups::backup_fence),
        )
        .route(
            "/api/v1/backup-jobs/:job_id",
            delete(chv_webui_bff::handlers::backups::backup_fence),
        )
        .route(
            "/api/v1/backup-jobs/:job_id/run",
            post(chv_webui_bff::handlers::backups::backup_fence),
        )
        .route(
            "/api/v1/backup-jobs/:job_id/toggle",
            post(chv_webui_bff::handlers::backups::backup_fence),
        )
        .route(
            "/api/v1/backup-history",
            get(chv_webui_bff::handlers::backups::backup_fence),
        )
        .route(
            "/api/v1/vms/:vm_id/backups",
            get(chv_webui_bff::handlers::backups::backup_fence),
        )
        .layer(middleware::from_fn(
            chv_webui_bff::csrf_middleware::csrf_protection,
        ))
        // Authentication must run before the fence: the fence replaced
        // handlers that each extracted `BearerToken`, and returning 501 to an
        // anonymous caller would both leak the surface and regress the
        // pre-fence 401 contract. `viewer` is the pre-fence floor (any
        // authenticated session).
        .layer(middleware::from_fn_with_state(
            bff_state.clone(),
            chv_webui_bff::auth::viewer_middleware,
        ));

    let admin_routes = Router::new()
        .route("/admin/nodes", get(nodes::list_nodes))
        // `:id`, not `{id}`: axum 0.7's matchit (0.7.3) has no brace
        // path-param syntax — a `{id}` spelling registers a LITERAL
        // segment, so the route can never match any request and every
        // real id falls through to the CP's NOT_IMPLEMENTED fallback
        // (a 404 indistinguishable from a missing id). Found while
        // repointing `chvctl migrate cancel` (#372 DP4, review round);
        // the sibling `/admin/nodes/{id}` and `/admin/operations/{id}`
        // carried the identical latent bug and are fixed the same way.
        // Path contracts, admin tier, and handlers are unchanged —
        // this only makes the routes resolvable.
        .route("/admin/nodes/:id", get(nodes::get_node))
        .route("/admin/operations", get(operations::list_operations))
        .route("/admin/operations/:id", get(operations::get_operation))
        .route(
            // Same `{id}` → `:id` fix as the siblings above (see the
            // comment there); verified by the #372 PR 4 contract row.
            "/admin/migrations/:id/cancel",
            post(migrations::cancel_migration),
        )
        .route("/metrics", get(health::metrics_handler))
        .route("/api/v1/install/status", get(stub::get_install_status_stub))
        .route(
            "/api/v1/install/bootstrap",
            post(stub::bootstrap_install_stub),
        )
        .route("/api/v1/install/repair", post(stub::repair_install_stub))
        .layer(middleware::from_fn_with_state(
            bff_state.clone(),
            chv_webui_bff::auth::admin_middleware,
        ));

    let router = Router::new()
        .merge(bff_router)
        // Health (unauthenticated — needed for load balancer probes)
        .route("/health", get(health::health_handler))
        .route("/health/deep", get(health::deep_health_handler))
        .route("/health/convergence", get(health::convergence_handler))
        .route("/ready", get(health::ready_handler))
        // Internal management (unauthenticated — protected by localhost-only check in handler)
        .route(
            "/internal/bootstrap-token",
            post(bootstrap::seed_bootstrap_token),
        )
        // Admin-protected routes
        .merge(admin_routes)
        // Auth routes
        .route("/api/v1/auth/login", post(auth::login_handler))
        .route("/api/v1/auth/me", get(auth::me_handler))
        .route("/api/v1/auth/logout", post(auth::logout_handler))
        // Resource list stubs (return empty arrays so the UI renders empty states)
        .route("/api/v1/nodes", get(stub::list_nodes_stub))
        .route("/api/v1/vms", get(stub::list_vms_stub))
        .route("/api/v1/networks", get(stub::list_networks_stub))
        .route("/api/v1/operations", get(stub::list_operations_stub))
        .route("/api/v1/events", get(stub::list_events_stub))
        .route("/api/v1/images", get(stub::list_images_stub))
        .route("/api/v1/vm-templates", get(stub::list_vm_templates_stub))
        .route(
            "/api/v1/cloud-init-templates",
            get(stub::list_cloud_init_templates_stub),
        )
        .merge(legacy_backup_routes)
        .route("/api/v1/quotas", get(stub::list_quotas_stub))
        .route("/api/v1/usage", get(stub::get_usage_stub));

    // Guest monitoring agent surfaces (G3). The browser-tier routes
    // carry the same role + CSRF middlewares as their BFF siblings;
    // the agent-authenticated transport deliberately carries none
    // (TLS client credential + registry binding is its only
    // identity).
    let router = match agent_browser {
        Some((viewer, operator)) => router
            .merge(
                viewer
                    .layer(middleware::from_fn(
                        chv_webui_bff::csrf_middleware::csrf_protection,
                    ))
                    .layer(middleware::from_fn_with_state(
                        bff_state.clone(),
                        chv_webui_bff::auth::viewer_middleware,
                    )),
            )
            .merge(
                operator
                    .layer(middleware::from_fn(
                        chv_webui_bff::csrf_middleware::csrf_protection,
                    ))
                    .layer(middleware::from_fn_with_state(
                        bff_state.clone(),
                        chv_webui_bff::auth::operator_middleware,
                    )),
            ),
        None => router,
    };
    let router = match agent_ingest {
        Some(ingest) => router.merge(ingest),
        None => router,
    };

    // Fallback (issue #447): with `[webui] enabled = true` the JSON 404
    // is replaced — for NON-reserved prefixes only — by the static UI
    // fallback (ServeDir + SPA index.html + cache headers, see
    // `webui_fallback`). Disabled (the fail-closed default) the router
    // is byte-identical to the pre-#447 fallback. The fallback is
    // registered BEFORE the layers below so `security_headers` (and the
    // Extension layers) wrap the served assets too.
    let router = if webui.enabled {
        tracing::info!(
            dir = %webui.dir.display(),
            "serving Web UI static assets ([webui] enabled)"
        );
        if !webui.dir.is_dir() {
            tracing::warn!(
                dir = %webui.dir.display(),
                "[webui] is enabled but the directory does not exist; \
                 every UI route will 404 until it is present \
                 (check the [webui] dir setting and the UI build/install)"
            );
        }
        router
            .fallback(webui_fallback)
            .layer(Extension(WebUiFallback {
                serve_dir: ServeDir::new(&webui.dir),
                index_file: ServeFile::new(webui.dir.join("index.html")),
            }))
    } else {
        router.fallback(not_found_handler)
    };

    router
        .layer(Extension(convergence_metrics))
        // Audit wraps EVERY route and the fallback (registered above):
        // the BFF's /v1 surface, the guest-agent browser routes, the
        // CP admin routes, the legacy /api/v1 backup surface,
        // /internal, and unmatched non-safe paths all pass through the
        // same audit middleware, so no privileged mutation can bypass
        // the trail (#658 review H1/H2). bff_router_unaudited supplies
        // the /v1 routes without their own wrapper, so nothing is
        // audited twice.
        .layer(middleware::from_fn_with_state(
            bff_state.clone(),
            chv_webui_bff::audit::record_privileged_mutation,
        ))
        // Metrics sits outside the audit layer so the status it counts
        // is the final one on the wire — including the 500
        // AUDIT_LOG_WRITE_FAILED replacement (#658 review M3). It only
        // records API-shaped paths (see is_recorded_path), so the
        // static tree and health probes stay out of the label set.
        .layer(middleware::from_fn(
            chv_webui_bff::metrics_middleware::track_metrics,
        ))
        .layer(middleware::from_fn(security_headers))
        // Correlation outermost: the id is in request extensions by the
        // time the audit row is built, and every response (including
        // audit's 500 replacement) carries x-correlation-id.
        .layer(middleware::from_fn(
            chv_webui_bff::correlation_middleware::extract_correlation_id,
        ))
        .with_state(bff_state)
}
