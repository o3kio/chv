//! Phase-7 D1 — Architecture Designer permission matrix.
//!
//! Asserts every architecture endpoint enforces its declared role tier at the
//! routing layer, and that no architecture endpoint slips into the router
//! without an entry in this matrix. Mirrors the scaffold from
//! `tests/router_role_gates.rs` (in-memory sqlite, mint a JWT per role,
//! `oneshot` against `bff_router(state)`).
//!
//! We deliberately test the **role gate**, not handler behaviour. For the
//! 4xx paths a handler may legitimately answer 200/400/404/422 — what we
//! care about is `!= 403`. For the 403 paths the operator middleware rejects
//! before the handler runs, so no DB seeding is required.
//!
//! ## Inventory divergence from the plan
//!
//! The Phase-7 task plan lists 18 operator-tier architecture routes. The
//! authoritative router source (`crates/chv-webui-bff/src/router.rs`)
//! registers **14 operator-tier + 4 viewer-tier** architecture routes
//! (18 total). The matrix below mirrors `router.rs`, and the
//! `matrix_covers_every_architecture_route` exhaustiveness test reads
//! `router.rs` at compile time so this divergence is enforced
//! mechanically.
//!
//! ## Production-environment escalation (out of this matrix's scope)
//!
//! The `/v1/architectures/apply` and `/destroy` endpoints have a
//! second-tier check: when the architecture's `environment` is
//! `production` / `prod`, an additional `Admin` role is required by the
//! handler's `enforce_production_guard`. That escalation is asserted in
//! `architectures_apply.rs::apply_production_environment_as_operator_returns_403_production_requires_admin`
//! (test #4 there) and `apply_production_environment_as_admin_succeeds`
//! (test #5). This matrix only enforces routing-layer tiers; the
//! production-guard tests close the env-aware leg.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chv_common::SystemClock;
use chv_controlplane_store::{
    AlertRepository, AlertRuleRepository, ApplyRunRepository, BackupRepository,
    DesiredStateRepository, DriftReportRepository, EventRepository, ImageRepository,
    NetworkRepository, NodeRepository, NotificationOutboxRepository, ObservedStateRepository,
    OperationRepository, TopologyRepository,
};
use chv_webui_bff::auth::Claims;
use chv_webui_bff::mutations::MutationService;
use chv_webui_bff::{AppState, BffError};
use sqlx::sqlite::SqlitePoolOptions;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Endpoint inventory (authoritative for the matrix). Tier is the lowest role
// that may reach the handler. Body is a JSON literal that satisfies the
// handler's serde derivation; payload realism does NOT matter — the role
// gate runs before the handler in the routing layer.
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Tier {
    Viewer,
    Operator,
}

const ENDPOINTS: &[(&str, Tier, &str)] = &[
    // Viewer-tier (3) — mounted under viewer middleware AND the handler
    // does not re-tighten with `require_operator_or_admin`.
    ("/v1/architectures/list", Tier::Viewer, "{}"),
    (
        "/v1/architectures/get",
        Tier::Viewer,
        r#"{"id":"arch-nonexistent"}"#,
    ),
    (
        "/v1/architectures/versions/list",
        Tier::Viewer,
        r#"{"id":"arch-nonexistent"}"#,
    ),
    // `/v1/architectures/validate` is mounted in the viewer block in
    // `router.rs` but the `validate_architecture` handler calls
    // `require_operator_or_admin(&claims)?` first thing (see
    // `crates/chv-webui-bff/src/handlers/architectures.rs:499`). Effective
    // tier — what the caller actually experiences — is Operator. The
    // matrix tests **effective** behaviour, so it is classified Operator
    // here. The exhaustiveness check below also exercises the route, so
    // the divergence is recorded mechanically. Flagged for follow-up:
    // either drop the in-handler check (route-level gate is enough) or
    // move the route under the operator middleware to keep the routing
    // table honest.
    (
        "/v1/architectures/validate",
        Tier::Operator,
        r#"{"id":"arch-nonexistent"}"#,
    ),
    // Operator-tier (14). Bodies match the BFF's request DTOs from
    // `crates/chv-webui-bff/src/handlers/architectures.rs`. Where the DTO
    // has `expected_version: i64` / `plan_id: String` etc., we provide a
    // shape that survives serde without reaching real DB rows.
    (
        "/v1/architectures/create",
        Tier::Operator,
        r#"{"name":"x"}"#,
    ),
    (
        "/v1/architectures/update",
        Tier::Operator,
        r#"{"id":"arch-x","expected_version":1}"#,
    ),
    (
        "/v1/architectures/archive",
        Tier::Operator,
        r#"{"id":"arch-x","expected_version":1}"#,
    ),
    (
        "/v1/architectures/check-fleet",
        Tier::Operator,
        r#"{"id":"arch-x"}"#,
    ),
    (
        "/v1/architectures/generate-yaml",
        Tier::Operator,
        r#"{"id":"arch-x"}"#,
    ),
    (
        "/v1/architectures/validate-yaml",
        Tier::Operator,
        r#"{"yaml":"apiVersion: chv.kubedo.io/v1alpha1\nkind: CHVArchitecture\nmetadata:\n  name: x\n"}"#,
    ),
    (
        "/v1/architectures/import-yaml",
        Tier::Operator,
        r#"{"id":"arch-x","yaml":"apiVersion: chv.kubedo.io/v1alpha1\nkind: CHVArchitecture\nmetadata:\n  name: x\n"}"#,
    ),
    (
        "/v1/architectures/plan",
        Tier::Operator,
        r#"{"id":"arch-x"}"#,
    ),
    (
        "/v1/architectures/destroy-plan",
        Tier::Operator,
        r#"{"id":"arch-x"}"#,
    ),
    (
        "/v1/architectures/discard-plan",
        Tier::Operator,
        r#"{"plan_id":"plan-x"}"#,
    ),
    (
        "/v1/architectures/apply",
        Tier::Operator,
        r#"{"id":"arch-x","plan_id":"plan-x"}"#,
    ),
    (
        "/v1/architectures/destroy",
        Tier::Operator,
        r#"{"id":"arch-x","plan_id":"plan-x"}"#,
    ),
    (
        "/v1/architectures/runs/list",
        Tier::Operator,
        r#"{"id":"arch-x"}"#,
    ),
    (
        "/v1/architectures/drift",
        Tier::Operator,
        r#"{"id":"arch-x","force_refresh":false}"#,
    ),
    // NetBox projection (#239, PR 5) — all eight endpoints are
    // operator-tier per the netbox API contract (viewer has no access
    // to any netbox endpoint). The two data-level escalations —
    // production export/config writes to Admin, delete-retention to
    // Admin — are enforced inside the handlers against the persisted
    // topology row (same pattern as /apply's enforce_production_guard),
    // and are asserted in tests/architecture_netbox_routes.rs.
    (
        "/v1/architectures/netbox/config/get",
        Tier::Operator,
        r#"{"id":"arch-x"}"#,
    ),
    (
        "/v1/architectures/netbox/config/upsert",
        Tier::Operator,
        r#"{"id":"arch-x","expected_version":1,"endpoint":"https://netbox.example.internal","token_secret_ref":"ref","retention_policy":"mark_stale","enable_post_apply":false}"#,
    ),
    (
        "/v1/architectures/netbox/config/delete",
        Tier::Operator,
        r#"{"id":"arch-x"}"#,
    ),
    (
        "/v1/architectures/netbox/export/dry-run",
        Tier::Operator,
        r#"{"id":"arch-x"}"#,
    ),
    (
        "/v1/architectures/netbox/export",
        Tier::Operator,
        r#"{"id":"arch-x"}"#,
    ),
    (
        "/v1/architectures/netbox/runs/list",
        Tier::Operator,
        r#"{"id":"arch-x"}"#,
    ),
    (
        "/v1/architectures/netbox/runs/get",
        Tier::Operator,
        r#"{"id":"arch-x","run_id":"netrun-x"}"#,
    ),
    (
        "/v1/architectures/netbox/runs/retry",
        Tier::Operator,
        r#"{"id":"arch-x","run_id":"netrun-x"}"#,
    ),
];

const ALL_ROLES: &[&str] = &["viewer", "operator", "admin"];

// ---------------------------------------------------------------------------
// State + token helpers — copied from `tests/router_role_gates.rs` so the
// permission matrix stays self-contained and does not couple to that file's
// future churn.
// ---------------------------------------------------------------------------

struct NoopMutations;

#[async_trait]
impl MutationService for NoopMutations {
    async fn mutate_vm(
        &self,
        _vm_id: String,
        _action: String,
        _force: bool,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        unreachable!()
    }
    async fn migrate_vm(
        &self,
        _vm_id: String,
        _target_node_id: String,
        _pause_first: bool,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        unreachable!()
    }
    async fn snapshot_vm(
        &self,
        _vm_id: String,
        _destination: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        unreachable!()
    }
    async fn restore_snapshot(
        &self,
        _vm_id: String,
        _source: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        unreachable!()
    }
    async fn mutate_node(
        &self,
        _node_id: String,
        _action: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateNodeResponse, BffError> {
        unreachable!()
    }
    async fn mutate_volume(
        &self,
        _volume_id: String,
        _action: String,
        _force: bool,
        _resize_bytes: Option<u64>,
        _vm_id: Option<String>,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!()
    }
    async fn snapshot_volume(
        &self,
        _volume_id: String,
        _snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!()
    }
    async fn restore_volume_snapshot(
        &self,
        _volume_id: String,
        _snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!()
    }
    async fn delete_volume_snapshot(
        &self,
        _volume_id: String,
        _snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!()
    }
    async fn mutate_network(
        &self,
        _network_id: String,
        _action: String,
        _force: bool,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateNetworkResponse, BffError> {
        unreachable!()
    }
    async fn clone_volume(
        &self,
        _volume_id: String,
        _new_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!()
    }
}

async fn build_state() -> AppState {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    chv_controlplane_store::run_migrations(&pool, None)
        .await
        .expect("run migrations");
    chv_controlplane_store::test_util::seed_test_users(&pool).await;

    AppState {
        pool: pool.clone(),
        node_repo: NodeRepository::new(pool.clone()),
        operation_repo: OperationRepository::new(pool.clone()),
        event_repo: EventRepository::new(pool.clone()),
        alert_repo: AlertRepository::new(pool.clone()),
        alert_rules: std::sync::Arc::new(AlertRuleRepository::new(pool.clone())),
        notification_outbox: std::sync::Arc::new(NotificationOutboxRepository::new(pool.clone())),
        alerting_max_rules: 200,
        notification_channels: chv_webui_bff::NotificationChannels {
            webhook: false,
            slack: false,
        },
        desired_state_repo: DesiredStateRepository::new(pool.clone()),
        observed_state_repo: ObservedStateRepository::new(pool.clone()),
        backup_repo: BackupRepository::new(pool.clone()),
        topology_repo: TopologyRepository::new(pool.clone()),
        network_repo: NetworkRepository::new(pool.clone()),
        image_repo: ImageRepository::new(pool.clone()),
        apply_runs: Arc::new(ApplyRunRepository::new(pool.clone())),
        drift_reports: Arc::new(DriftReportRepository::new(pool.clone())),
        netbox_config: Arc::new(
            chv_controlplane_store::NetboxProjectionConfigRepository::new(pool.clone()),
        ),
        netbox_runs: Arc::new(chv_controlplane_store::NetboxProjectionRunRepository::new(
            pool.clone(),
        )),
        mutations: Arc::new(NoopMutations),
        jwt_secret: "test-secret".to_string(),
        console_jwt_secret: "test-secret".to_string(),
        monitoring: None,
        monitoring_health: chv_webui_bff::MonitoringHealth::new(),
        agent_runtime_dir: std::path::PathBuf::from("/var/lib/chv/agent"),
        cache: chv_webui_bff::BffCache::new(5),
        // The role gate runs before any handler reads `state.clock`, so the
        // wall-clock variant is fine for both 403 paths and "not 403"
        // sanity checks. The drift handler does read the clock, but on
        // unknown architectures it 404s before touching it.
        clock: Arc::new(SystemClock),
    }
}

fn token_for(state: &AppState, role: &str) -> String {
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let claims = Claims {
        sub: "u-tester".to_string(),
        username: "tester".to_string(),
        role: role.to_string(),
        exp,
        must_change_password: false,
        aud: None,
        token_version: None,
    };
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
    jsonwebtoken::encode(
        &header,
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(state.jwt_secret.as_bytes()),
    )
    .expect("encode test token")
}

async fn post_with_token(state: AppState, path: &str, token: &str, body: &str) -> StatusCode {
    let app = chv_webui_bff::bff_router(state.clone()).with_state(state);
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    app.oneshot(req).await.unwrap().status()
}

// ---------------------------------------------------------------------------
// 1. Viewer-tier endpoints accept all three roles.
// ---------------------------------------------------------------------------
//
// "Accept" means: the role gate let the request through. The handler then
// answers however it likes (200, 400, 404, 422). We only assert the response
// is NOT 403, because 403 is the role-gate signal we are guarding against.

#[tokio::test]
async fn viewer_tier_endpoints_accept_all_three_roles() {
    let state = build_state().await;
    for (path, tier, body) in ENDPOINTS {
        if *tier != Tier::Viewer {
            continue;
        }
        for role in ALL_ROLES {
            let token = token_for(&state, role);
            let status = post_with_token(state.clone(), path, &token, body).await;
            assert_ne!(
                status,
                StatusCode::FORBIDDEN,
                "viewer-tier route {path} unexpectedly 403'd role {role} (got {status})"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 2. Operator-tier endpoints reject viewer with 403.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn operator_tier_endpoints_block_viewer() {
    let state = build_state().await;
    let viewer = token_for(&state, "viewer");
    for (path, tier, body) in ENDPOINTS {
        if *tier != Tier::Operator {
            continue;
        }
        let status = post_with_token(state.clone(), path, &viewer, body).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "operator-tier route {path} must 403 the viewer role (got {status})"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Operator-tier endpoints accept operator and admin.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn operator_tier_endpoints_accept_operator_and_admin() {
    let state = build_state().await;
    for (path, tier, body) in ENDPOINTS {
        if *tier != Tier::Operator {
            continue;
        }
        for role in &["operator", "admin"] {
            let token = token_for(&state, role);
            let status = post_with_token(state.clone(), path, &token, body).await;
            assert_ne!(
                status,
                StatusCode::FORBIDDEN,
                "operator-tier route {path} unexpectedly 403'd role {role} (got {status})"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 4. Exhaustiveness: every architecture route declared in router.rs must
// appear in `ENDPOINTS`. Reads `router.rs` at compile time and scans for
// `/v1/architectures/...` literals. Any new route added to the router
// without a matrix entry trips this test, forcing the next contributor to
// declare its tier.
// ---------------------------------------------------------------------------

#[test]
fn matrix_covers_every_architecture_route() {
    let router_src = include_str!("../src/router.rs");
    let mut router_paths: HashSet<String> = HashSet::new();

    // Manual literal scan: locate every occurrence of "/v1/architectures/"
    // and slurp the rest of the path up to the closing quote. Cheaper than
    // pulling in the regex crate as a dev-dep, and the prefix is a stable
    // contract anchor.
    let needle = "\"/v1/architectures/";
    let mut cursor = 0;
    while let Some(rel) = router_src[cursor..].find(needle) {
        let start = cursor + rel + 1; // skip leading quote
        let rest = &router_src[start..];
        if let Some(end_rel) = rest.find('"') {
            router_paths.insert(rest[..end_rel].to_string());
            cursor = start + end_rel + 1;
        } else {
            break;
        }
    }

    let matrix_paths: HashSet<&str> = ENDPOINTS.iter().map(|(p, _, _)| *p).collect();

    let missing_from_matrix: Vec<&String> = router_paths
        .iter()
        .filter(|p| !matrix_paths.contains(p.as_str()))
        .collect();

    assert!(
        missing_from_matrix.is_empty(),
        "router.rs registers architecture routes that the permission matrix does not cover. \
         Add an ENDPOINTS entry (with the correct Tier) for each. Missing: {missing_from_matrix:?}"
    );

    // Sanity: the matrix must not declare ghost routes that no longer
    // exist. If you remove a route from router.rs, remove it here too.
    let ghosts: Vec<&str> = matrix_paths
        .iter()
        .copied()
        .filter(|p| !router_paths.contains(*p))
        .collect();
    assert!(
        ghosts.is_empty(),
        "permission matrix references architecture routes not registered in router.rs: {ghosts:?}"
    );
}
