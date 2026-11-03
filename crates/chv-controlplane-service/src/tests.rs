use crate::*;
use chv_controlplane_store::*;
use chv_controlplane_types::domain::NodeId;
use control_plane_node_api::control_plane_node_api as proto;
use std::sync::Arc;

use crate::lifecycle::LifecycleService;

// Mock CertificateIssuer for enrollment tests
struct MockCertIssuer;
#[async_trait::async_trait]
impl CertificateIssuer for MockCertIssuer {
    async fn issue_node_certificate(
        &self,
        _node_id: &NodeId,
    ) -> Result<IssuedCertificate, ControlPlaneServiceError> {
        Ok(IssuedCertificate {
            certificate_pem: vec![],
            private_key_pem: vec![],
            ca_pem: vec![],
            serial: "mock-serial".into(),
        })
    }
}

fn test_app_state(pool: StorePool) -> chv_webui_bff::AppState {
    let pool_for_mutations = pool.clone();
    let pool_for_alerting = pool.clone();
    let node_repo = NodeRepository::new(pool.clone());
    let operation_repo = OperationRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let alert_repo = AlertRepository::new(pool.clone());
    let desired_state_repo = DesiredStateRepository::new(pool.clone());
    let observed_state_repo = ObservedStateRepository::new(pool.clone());
    let backup_repo = BackupRepository::new(pool.clone());
    let topology_repo = chv_controlplane_store::TopologyRepository::new(pool.clone());
    let network_repo = NetworkRepository::new(pool.clone());
    let image_repo = ImageRepository::new(pool.clone());
    let apply_runs = Arc::new(ApplyRunRepository::new(pool.clone()));
    let drift_reports = Arc::new(chv_controlplane_store::DriftReportRepository::new(
        pool.clone(),
    ));
    let netbox_config =
        Arc::new(chv_controlplane_store::NetboxProjectionConfigRepository::new(pool.clone()));
    let netbox_runs = Arc::new(chv_controlplane_store::NetboxProjectionRunRepository::new(
        pool.clone(),
    ));
    let lifecycle_service = Arc::new(crate::lifecycle::LifecycleServiceImplementation::new(
        node_repo.clone(),
        operation_repo.clone(),
        event_repo.clone(),
        desired_state_repo.clone(),
    ));
    chv_webui_bff::AppState {
        pool,
        node_repo,
        operation_repo,
        event_repo,
        alert_repo,
        alert_rules: std::sync::Arc::new(chv_controlplane_store::AlertRuleRepository::new(
            pool_for_alerting.clone(),
        )),
        notification_outbox: std::sync::Arc::new(
            chv_controlplane_store::NotificationOutboxRepository::new(pool_for_alerting),
        ),
        alerting_max_rules: 200,
        notification_channels: chv_webui_bff::NotificationChannels {
            webhook: false,
            slack: false,
        },
        desired_state_repo,
        observed_state_repo,
        backup_repo,
        topology_repo,
        network_repo,
        image_repo,
        apply_runs,
        drift_reports,
        netbox_config,
        netbox_runs,
        mutations: Arc::new(crate::ControlPlaneMutationService::new(
            pool_for_mutations,
            lifecycle_service,
        )),
        jwt_secret: "test-secret".to_string(),
        monitoring: None,
        monitoring_health: chv_webui_bff::MonitoringHealth::new(),
        agent_runtime_dir: std::path::PathBuf::from("/var/lib/chv/agent"),
        cache: chv_webui_bff::BffCache::new(5),
        clock: Arc::new(chv_common::SystemClock),
    }
}

fn test_admin_token() -> String {
    test_token_for_role("admin")
}

fn test_token_for_role(role: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let claims = chv_webui_bff::auth::Claims {
        sub: format!("test-{role}-id"),
        username: role.to_string(),
        role: role.to_string(),
        exp,
        must_change_password: false,
    };
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
    jsonwebtoken::encode(
        &header,
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(b"test-secret"),
    )
    .expect("test token encoding should succeed")
}

#[tokio::test]
async fn test_health_endpoint() {
    use axum::http::StatusCode;
    use tower::ServiceExt;

    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let app = crate::api::router::admin_router(
        test_app_state(test_db.pool.clone()),
        crate::convergence_metrics::new_shared(),
        chv_config::WebUiConfig::default(),
    );

    let response = app
        .oneshot(
            axum::http::Request::get("/health")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_ready_endpoint() {
    use axum::http::StatusCode;
    use tower::ServiceExt;

    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let app = crate::api::router::admin_router(
        test_app_state(test_db.pool.clone()),
        crate::convergence_metrics::new_shared(),
        chv_config::WebUiConfig::default(),
    );

    let response = app
        .oneshot(
            axum::http::Request::get("/ready")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

/// `POST /v1/nodes/revoke-certificate` (#661): admin-only, idempotent, and
/// durable. A viewer must be rejected before the handler; the first admin
/// call inserts the denylist row, the second reports `already_revoked`.
#[tokio::test]
async fn revoke_certificate_endpoint_is_admin_only_and_idempotent() {
    use axum::body::Body;
    use axum::http::StatusCode;
    use tower::ServiceExt;

    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    let node_id = NodeId::new("node-revoke").unwrap();
    NodeRepository::new(pool.clone())
        .upsert_node(&NodeUpsertInput {
            node_id: node_id.clone(),
            hostname: "host-revoke".into(),
            display_name: "node-revoke".into(),
            certificate_serial: Some("0a3f7b2c".into()),
            agent_version: None,
            control_plane_version: None,
            enrolled_unix_ms: 1_000,
            last_seen_unix_ms: 1_000,
        })
        .await
        .unwrap();

    let app = crate::api::router::admin_router(
        test_app_state(pool.clone()),
        crate::convergence_metrics::new_shared(),
        chv_config::WebUiConfig::default(),
    );

    let body = serde_json::json!({
        "node_id": "node-revoke",
        "reason": "host compromise"
    })
    .to_string();

    // Viewer is forbidden at the router's admin middleware.
    let viewer_response = app
        .clone()
        .oneshot(
            axum::http::Request::post("/v1/nodes/revoke-certificate")
                .header(
                    "authorization",
                    format!("Bearer {}", test_token_for_role("viewer")),
                )
                .header("content-type", "application/json")
                .body(Body::from(body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(viewer_response.status(), StatusCode::FORBIDDEN);

    // First admin call revokes the node's active serial.
    let first = app
        .clone()
        .oneshot(
            axum::http::Request::post("/v1/nodes/revoke-certificate")
                .header("authorization", format!("Bearer {}", test_admin_token()))
                .header("content-type", "application/json")
                .body(Body::from(body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(first.into_body(), 64 * 1024)
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(parsed["certificate_serial"], "0a3f7b2c");
    assert_eq!(parsed["already_revoked"], false);

    // Repeat: same serial, 200, flagged idempotent.
    let second = app
        .oneshot(
            axum::http::Request::post("/v1/nodes/revoke-certificate")
                .header("authorization", format!("Bearer {}", test_admin_token()))
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(second.into_body(), 64 * 1024)
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(parsed["already_revoked"], true);

    let repo = chv_controlplane_store::CertificateRevocationRepository::new(pool);
    let rows = repo.list_for_node("node-revoke").await.unwrap();
    assert_eq!(
        rows.len(),
        1,
        "idempotent revocation must not duplicate rows"
    );
    assert_eq!(rows[0].certificate_serial, "0a3f7b2c");
    assert_eq!(rows[0].revoked_by.as_deref(), Some("admin"));
}

/// H2 (#661 review): explicit serial input must be the canonical DB value.
/// Operators copying `openssl x509 -serial` (HEX DER bytes, 16+ chars) used to
/// get a 200 and a denylist row that matched no certificate. Uppercase input is
/// normalized; malformed/openssl-shaped input is a clear 400.
#[tokio::test]
async fn revoke_certificate_serial_shape_is_validated() {
    use axum::body::Body;
    use axum::http::StatusCode;
    use tower::ServiceExt;

    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    let node_id = NodeId::new("node-serial-shape").unwrap();
    NodeRepository::new(pool.clone())
        .upsert_node(&NodeUpsertInput {
            node_id,
            hostname: "host-serial-shape".into(),
            display_name: "node-serial-shape".into(),
            certificate_serial: Some("0a3f7b2c".into()),
            agent_version: None,
            control_plane_version: None,
            enrolled_unix_ms: 1_000,
            last_seen_unix_ms: 1_000,
        })
        .await
        .unwrap();

    let app = crate::api::router::admin_router(
        test_app_state(pool.clone()),
        crate::convergence_metrics::new_shared(),
        chv_config::WebUiConfig::default(),
    );

    let post = |body: serde_json::Value| {
        axum::http::Request::post("/v1/nodes/revoke-certificate")
            .header("authorization", format!("Bearer {}", test_admin_token()))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    for bad in [
        "0a3f7b2c2f9e1b4d6a", // openssl -serial HEX DER rendering
        "0a3f7b2",            // too short
        "0a3f7b2cg",          // not hex
        "node-serial-shape",  // pasted node id
    ] {
        let response = app
            .clone()
            .oneshot(post(serde_json::json!({
                "node_id": "node-serial-shape",
                "certificate_serial": bad,
                "reason": "shape test",
            })))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "serial {bad:?} must be rejected"
        );
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let message = parsed["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("canonical CHV serial"),
            "400 must explain the canonical serial; got: {message}"
        );
    }

    // Uppercase is normalized to the stored lowercase form.
    let response = app
        .oneshot(post(serde_json::json!({
            "node_id": "node-serial-shape",
            "certificate_serial": "0A3F7B2C",
            "reason": "uppercase shape",
        })))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(parsed["certificate_serial"], "0a3f7b2c");

    let repo = chv_controlplane_store::CertificateRevocationRepository::new(pool);
    assert!(repo.get("0a3f7b2c").await.unwrap().is_some());
    assert!(
        repo.get("0A3F7B2C").await.unwrap().is_none(),
        "only the normalized lowercase serial is stored"
    );
}

/// Regression (CI webui container smoke, G3 round 1): the guest-agent
/// browser pair must merge into the full admin router in BOTH arms.
/// The disabled pair originally carried all five paths in one router
/// passed as both viewer and operator — axum panics with
/// `Overlapping method route` at build time, i.e. a control plane that
/// cannot boot on a default (guest-ingestion-unconfigured) install.
/// No other test boots the full router with the pair mounted, which is
/// exactly how this escaped the local suites.
#[tokio::test]
async fn admin_router_builds_with_guest_agent_pair_in_both_arms() {
    use axum::http::StatusCode;
    use tower::ServiceExt;

    let test_db = chv_controlplane_store::test_util::TestDb::new().await;

    // --- Disabled arm (the default install / converged-container shape).
    let (viewer, operator) = crate::api::agent_admin::agent_admin_disabled_routers();
    let app = crate::api::router::admin_router_with_guest_agents(
        test_app_state(test_db.pool.clone()),
        crate::convergence_metrics::new_shared(),
        chv_config::WebUiConfig::default(),
        Some((viewer, operator)),
        None,
    );
    // Building the router already proves no overlap; assert the honest
    // typed 503 through the real viewer middleware + CSRF layer too.
    let token = test_admin_token();
    let response = app
        .oneshot(
            axum::http::Request::post("/v1/monitoring/agents")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["code"], "guest_ingestion_disabled");

    // --- Enabled arm: the real viewer/operator pair (disjoint paths by
    // construction) must merge into the same outer router too.
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::default();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "chv-test-agent-ca");
    let ca_cert = params.self_signed(&ca_key).unwrap();
    let issuer = Arc::new(
        crate::monitoring_agent::AgentCertificateIssuer::new(
            &ca_cert.pem(),
            &ca_key.serialize_pem(),
        )
        .unwrap(),
    );
    let service = Arc::new(crate::monitoring_agent::MonitoringAgentService::new(
        chv_controlplane_store::MonitoringAgentRepository::new(test_db.pool.clone()),
        issuer,
        None,
        chv_monitoring_store::MonitoringHealth::new(),
        chv_controlplane_store::EventRepository::new(test_db.pool.clone()),
        Default::default(),
        None,
    ));
    let admin_state = Arc::new(crate::api::agent_admin::AgentAdminState {
        service,
        offline_after_ms: 300_000,
        enrollment_grace_ms: 600_000,
        renewal_window_ms: 86_400_000,
    });
    let _ = crate::api::router::admin_router_with_guest_agents(
        test_app_state(test_db.pool.clone()),
        crate::convergence_metrics::new_shared(),
        chv_config::WebUiConfig::default(),
        Some((
            crate::api::agent_admin::agent_viewer_router(admin_state.clone()),
            crate::api::agent_admin::agent_operator_router(admin_state),
        )),
        None,
    );
}

/// #658 review H1/H2 regression: the audit layer must wrap the FULL
/// production router — the guest-agent browser routes, the CP admin
/// and `/internal` surfaces, the legacy `/api/v1` backup surface, and
/// the outer fallback — not just the BFF's `/v1` routes. Before the
/// fix the audit middleware lived only inside `bff_router`, so every
/// privileged mutation outside `/v1` (and every unmatched non-safe
/// path, which the outer `.fallback` handled outside the BFF's own
/// layered fallback) bypassed the trail entirely. The fix mounts
/// `bff_router_unaudited` inside `admin_router_with_guest_agents` and
/// applies audit at the outermost layer, after fallback registration;
/// this test pins that contract end-to-end against the real router.
#[tokio::test]
async fn production_router_audits_all_privileged_surfaces_exactly_once() {
    use axum::http::StatusCode;
    use tower::ServiceExt;

    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();

    async fn rows_for(pool: &chv_controlplane_store::StorePool, path: &str) -> Vec<AuditLogRow> {
        sqlx::query_as::<_, AuditLogRow>(
            "SELECT * FROM audit_log WHERE path = ? ORDER BY occurred_at_ms DESC, audit_id DESC",
        )
        .bind(path)
        .fetch_all(pool)
        .await
        .expect("query audit rows")
    }

    // The full production shape: guest-agent browser pair mounted
    // (disabled arm → typed 503s), webui off so the fallback is the
    // JSON 404 handler — the same fallback that used to sit outside
    // the audit layer.
    let (viewer, operator) = crate::api::agent_admin::agent_admin_disabled_routers();
    let app = crate::api::router::admin_router_with_guest_agents(
        test_app_state(pool.clone()),
        crate::convergence_metrics::new_shared(),
        chv_config::WebUiConfig::default(),
        Some((viewer, operator)),
        None,
    );
    let token = test_admin_token();

    // (a) Unknown non-safe /v1 path: unrouted, so it is answered by the
    // PRODUCTION fallback — this exact request wrote zero rows before
    // the fix. Fail-closed classification must record it (anonymous
    // probing leaves a trace).
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::post("/v1/future-privileged-write")
                .header("content-type", "application/json")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let rows = rows_for(&pool, "/v1/future-privileged-write").await;
    assert_eq!(
        rows.len(),
        1,
        "unknown non-safe /v1 POST through the production fallback must write a row"
    );
    assert_eq!(rows[0].action, "future-privileged-write.post");
    assert_eq!(rows[0].status_code, 404);
    assert_eq!(rows[0].outcome, "failed");
    assert_eq!(rows[0].actor_id, "anonymous");

    // (b) CP admin surface: a routed mutation outside the BFF router.
    // "m-1" does not exist → 404 from the handler, and the attempt is
    // still recorded with the real actor.
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::post("/admin/migrations/m-1/cancel")
                .header("authorization", format!("Bearer {token}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let rows = rows_for(&pool, "/admin/migrations/m-1/cancel").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].action, "migration.cancel");
    assert_eq!(rows[0].resource_kind.as_deref(), Some("migration"));
    assert_eq!(rows[0].resource_id.as_deref(), Some("m-1"));
    assert_eq!(rows[0].actor_id, "test-admin-id");
    assert_eq!(rows[0].actor_role, "admin");

    // (b) /internal surface: the bootstrap-token seed only accepts
    // loopback ConnectInfo; the request injects it the same way the
    // TLS PeerInjectService now does (see tls.rs, review M1).
    let mut request = axum::http::Request::post("/internal/bootstrap-token")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            r#"{"token":"regression-test-token"}"#,
        ))
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.1:54321".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let rows = rows_for(&pool, "/internal/bootstrap-token").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].action, "bootstrap_token.seed");
    assert_eq!(rows[0].status_code, 200);

    // (b) Guest-agent browser surface (operator tier, disabled arm):
    // the 503 is typed, and the attempt is recorded.
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::post("/v1/monitoring/agents/revoke")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let rows = rows_for(&pool, "/v1/monitoring/agents/revoke").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].action, "agent.revoke");
    assert_eq!(rows[0].resource_kind.as_deref(), Some("monitoring_agent"));
    assert_eq!(rows[0].status_code, 503);

    // (c) A routed BFF mutation: audited EXACTLY once — the outer layer
    // must not double-count what the /v1 surface already records.
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::post("/v1/users/create")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({
                        "username": "prod-audit-user",
                        "password": "Sup3rSecretPassw0rd!",
                        "role": "viewer"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let rows = rows_for(&pool, "/v1/users/create").await;
    assert_eq!(
        rows.len(),
        1,
        "routed BFF mutation must be audited exactly once — no double audit"
    );
    assert_eq!(rows[0].action, "user.create");
    assert_eq!(rows[0].actor_id, "test-admin-id");
    assert_eq!(rows[0].outcome, "success");
    assert_eq!(rows[0].status_code, 200);

    // Reads stay out of the trail, and the five mutations above are the
    // COMPLETE table contents — nothing extra, nothing duplicated.
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::get("/health")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(total, 5, "exactly the five audited attempts above");
    assert!(rows_for(&pool, "/health").await.is_empty());
}

#[tokio::test]
async fn test_deep_health_endpoint() {
    use axum::http::StatusCode;
    use tower::ServiceExt;

    let test_db = chv_controlplane_store::test_util::TestDb::new().await;

    // Scenario 1: directory exists but has no sockets -> healthy (skipped connectivity)
    let mut app_state = test_app_state(test_db.pool.clone());
    let temp_dir = tempfile::tempdir().unwrap();
    app_state.agent_runtime_dir = temp_dir.path().to_path_buf();
    let app = crate::api::router::admin_router(
        app_state,
        crate::convergence_metrics::new_shared(),
        chv_config::WebUiConfig::default(),
    );

    let response = app
        .oneshot(
            axum::http::Request::get("/health/deep")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["status"], "healthy");
    let checks = json["checks"].as_object().unwrap();
    assert_eq!(checks["database"]["status"], "pass");
    assert_eq!(checks["agent_socket_dir"]["status"], "pass");
    assert_eq!(checks["agent_connectivity"]["status"], "skipped");
    assert_eq!(
        checks["agent_connectivity"]["detail"],
        "no agent sockets found"
    );

    // Scenario 2: directory does not exist -> degraded
    let mut app_state2 = test_app_state(test_db.pool.clone());
    app_state2.agent_runtime_dir = std::path::PathBuf::from("/nonexistent/chv/agent/dir");
    let app2 = crate::api::router::admin_router(
        app_state2,
        crate::convergence_metrics::new_shared(),
        chv_config::WebUiConfig::default(),
    );

    let response2 = app2
        .oneshot(
            axum::http::Request::get("/health/deep")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response2.status(), StatusCode::OK);
    let body2 = axum::body::to_bytes(response2.into_body(), usize::MAX)
        .await
        .unwrap();
    let json2: serde_json::Value = serde_json::from_slice(&body2).unwrap();

    assert_eq!(json2["status"], "degraded");
    let checks2 = json2["checks"].as_object().unwrap();
    assert_eq!(checks2["database"]["status"], "pass");
    assert_eq!(checks2["agent_socket_dir"]["status"], "fail");
    assert_eq!(checks2["agent_connectivity"]["status"], "skipped");
    assert_eq!(
        checks2["agent_connectivity"]["detail"],
        "agent socket directory not available"
    );
}

#[tokio::test]
async fn test_admin_nodes_endpoint() {
    use axum::http::StatusCode;
    use tower::ServiceExt;

    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-http', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = crate::api::router::admin_router(
        test_app_state(pool),
        crate::convergence_metrics::new_shared(),
        chv_config::WebUiConfig::default(),
    );

    let token = test_admin_token();
    let response = app
        .oneshot(
            axum::http::Request::get("/admin/nodes")
                .header("authorization", format!("Bearer {}", token))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(!json["nodes"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_admin_node_not_found() {
    use axum::http::StatusCode;
    use tower::ServiceExt;

    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let app = crate::api::router::admin_router(
        test_app_state(test_db.pool.clone()),
        crate::convergence_metrics::new_shared(),
        chv_config::WebUiConfig::default(),
    );

    let token = test_admin_token();
    let response = app
        .oneshot(
            axum::http::Request::get("/admin/nodes/missing-node")
                .header("authorization", format!("Bearer {}", token))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    // Assert the handler's own 404 body, not the router fallback's
    // NOT_IMPLEMENTED shape: before the `{id}` → `:id` fix the route
    // never matched and this test passed vacuously via the fallback —
    // the body shape is what distinguishes them.
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"], "not found");
}

#[tokio::test]
async fn test_admin_node_get_by_id_resolves() {
    use axum::http::StatusCode;
    use tower::ServiceExt;

    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-http', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = crate::api::router::admin_router(
        test_app_state(pool),
        crate::convergence_metrics::new_shared(),
        chv_config::WebUiConfig::default(),
    );

    let token = test_admin_token();
    let response = app
        .oneshot(
            axum::http::Request::get("/admin/nodes/node-http")
                .header("authorization", format!("Bearer {}", token))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // Pins the route registration: with the old `{id}` brace spelling
    // (a literal segment under matchit 0.7.3) this request fell through
    // to the NOT_IMPLEMENTED fallback and 404'd.
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["node"]["node_id"], "node-http");
}

#[tokio::test]
async fn test_admin_operation_get_by_id_resolves() {
    use axum::http::StatusCode;
    use tower::ServiceExt;

    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO operations (operation_id, idempotency_key, resource_kind, resource_id, operation_type, status, requested_at) VALUES ('op-http', 'idem-op-http', 'node', 'node-http', 'Test', 'Pending', strftime('%Y-%m-%dT%H:%M:%SZ','now'))",
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = crate::api::router::admin_router(
        test_app_state(pool),
        crate::convergence_metrics::new_shared(),
        chv_config::WebUiConfig::default(),
    );

    let token = test_admin_token();
    let response = app
        .oneshot(
            axum::http::Request::get("/admin/operations/op-http")
                .header("authorization", format!("Bearer {}", token))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // Same registration pin as the node sibling above.
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["operation"]["operation_id"], "op-http");
}

#[tokio::test]
async fn test_publish_alert_persistence() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    let observed_state_repo = ObservedStateRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let alert_repo = AlertRepository::new(pool.clone());
    let node_repo = NodeRepository::new(pool.clone());

    let service =
        TelemetryServiceImplementation::new(node_repo, observed_state_repo, event_repo, alert_repo);

    let op_id = "op-123-custom-string";
    let request_ok = proto::PublishAlertRequest {
        meta: Some(proto::RequestMeta {
            operation_id: op_id.into(),
            requested_by: "test-user".into(),
            target_node_id: "node-1".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-1".into(),
        severity: "Critical".into(),
        alert_type: "disk_full".into(),
        summary: "disk is full".into(),
        details_json: b"{\"usage\": 99}".to_vec(),
    };

    sqlx::query("INSERT INTO operations (operation_id, idempotency_key, resource_kind, resource_id, operation_type, status, requested_at) VALUES (?, ?, 'node', 'node-1', 'Test', 'Pending', strftime('%Y-%m-%dT%H:%M:%SZ','now'))")
        .bind(op_id)
        .bind("idem-123")
        .execute(&pool)
        .await
        .unwrap();

    let result = service
        .publish_alert(request_ok)
        .await
        .expect("Publish failed");
    assert_eq!(result.result.unwrap().status, "ok");

    // VERIFY PERSISTENCE - use runtime query to avoid needing DATABASE_URL at compile time
    let alert = sqlx::query("SELECT alert_type, operation_id FROM alerts WHERE node_id = ?")
        .bind("node-1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let alert_type: String = sqlx::Row::get(&alert, "alert_type");
    let operation_id: Option<String> = sqlx::Row::get(&alert, "operation_id");
    assert_eq!(alert_type, "disk_full");
    assert_eq!(operation_id, Some(op_id.to_string()));

    // Case 2: Whitespace-only operation_id should return InvalidArgument
    let request_whitespace = proto::PublishAlertRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "   ".into(),
            requested_by: "test-user".into(),
            target_node_id: "node-1".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-1".into(),
        severity: "Critical".into(),
        alert_type: "disk_full".into(),
        summary: "disk is full".into(),
        details_json: b"{}".to_vec(),
    };
    let result_err = service.publish_alert(request_whitespace).await;
    match result_err {
        Err(ControlPlaneServiceError::InvalidArgument(msg)) => {
            assert!(msg.contains("operation_id cannot be empty"));
        }
        other => panic!(
            "Expected InvalidArgument for whitespace op_id, got {:?}",
            other
        ),
    }
}

#[tokio::test]
async fn test_report_node_state_auto_creates_missing_node_row() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    let service = TelemetryServiceImplementation::new(
        NodeRepository::new(pool.clone()),
        ObservedStateRepository::new(pool.clone()),
        EventRepository::new(pool.clone()),
        AlertRepository::new(pool.clone()),
    );

    let request = proto::NodeStateReport {
        node_id: "node-missing".into(),
        state: "TenantReady".into(),
        observed_generation: "7".into(),
        health_status: "Healthy".into(),
        last_error: "".into(),
        reported_unix_ms: 1_710_000_000_000,
    };

    let result = service
        .report_node_state(request)
        .await
        .expect("report should succeed");
    assert_eq!(result.result.unwrap().status, "ok");

    let node = sqlx::query("SELECT hostname, display_name FROM nodes WHERE node_id = ?")
        .bind("node-missing")
        .fetch_one(&pool)
        .await
        .unwrap();
    let hostname: String = sqlx::Row::get(&node, "hostname");
    let display_name: String = sqlx::Row::get(&node, "display_name");
    assert_eq!(hostname, "node-missing");
    assert_eq!(display_name, "node-missing");

    let observed = sqlx::query(
        "SELECT observed_generation, observed_state FROM node_observed_state WHERE node_id = ?",
    )
    .bind("node-missing")
    .fetch_one(&pool)
    .await
    .unwrap();
    let generation: i64 = sqlx::Row::get(&observed, "observed_generation");
    let observed_state: String = sqlx::Row::get(&observed, "observed_state");
    assert_eq!(generation, 7);
    assert_eq!(observed_state, "TenantReady");
}

#[tokio::test]
async fn test_enrollment_extended_inventory_persistence() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    let node_repo = NodeRepository::new(pool.clone());
    let token_repo = BootstrapTokenRepository::new(pool.clone());
    let cert_issuer = Arc::new(MockCertIssuer);
    let vtep_repo = VtepRepository::new(pool.clone());
    let service =
        EnrollmentServiceImplementation::new(node_repo, token_repo, Some(cert_issuer), vtep_repo);

    // Seed a bootstrap token for enrollment (sha256("123"))
    let hash = "a665a45920422f9d417e4867efdc4fb8a04a1f3fff1fa07e998e86f7f7a27ae3";
    sqlx::query("INSERT INTO bootstrap_tokens (token_hash, one_time_use) VALUES (?, false)")
        .bind(hash)
        .execute(&pool)
        .await
        .unwrap();

    let mut labels = std::collections::HashMap::new();
    labels.insert("env".to_string(), "prod".to_string());

    let request = proto::EnrollmentRequest {
        bootstrap_token: "123".into(),
        inventory: Some(proto::NodeInventory {
            node_id: "node-new-1".into(),
            hostname: "host-1".into(),
            architecture: "x86_64".into(),
            cpu_threads: 16,
            memory_bytes: 32 * 1024 * 1024 * 1024,
            storage_classes: vec!["ssd".into()],
            network_capabilities: vec!["vxlan".into()],
            hypervisor_capabilities: vec![],
            labels,
            vtep_ip: String::new(),
            wireguard_public_key: String::new(),
            underlay_mtu: 0,
            authority_mode: Default::default(),
        }),
        versions: Some(proto::ServiceVersions {
            node_id: "node-new-1".into(),
            chv_agent_version: "1.0.0".into(),
            chv_stord_version: "1.0.0".into(),
            chv_nwd_version: "1.0.0".into(),
            cloud_hypervisor_version: "40.0.0".into(),
            host_bundle_version: "1.2.3".into(),
        }),
    };

    service
        .enroll_node(request, None)
        .await
        .expect("Enrollment failed");

    // VERIFY PERSISTENCE
    let node = sqlx::query("SELECT hostname FROM nodes WHERE node_id = ?")
        .bind("node-new-1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let hostname: String = sqlx::Row::get(&node, "hostname");
    assert_eq!(hostname, "host-1");

    let inv = sqlx::query("SELECT storage_classes, labels FROM node_inventory WHERE node_id = ?")
        .bind("node-new-1")
        .fetch_one(&pool)
        .await
        .unwrap();

    // Check JSONB columns
    let storage_classes_val: serde_json::Value = sqlx::Row::get(&inv, "storage_classes");
    let storage_classes: Vec<String> = serde_json::from_value(storage_classes_val).unwrap();
    assert_eq!(storage_classes, vec!["ssd"]);

    let labels_val: serde_json::Value = sqlx::Row::get(&inv, "labels");
    let labels: std::collections::HashMap<String, String> =
        serde_json::from_value(labels_val).unwrap();
    assert_eq!(labels.get("env").unwrap(), "prod");
}

#[tokio::test]
async fn test_enrollment_registers_fabric_identity() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    let node_repo = NodeRepository::new(pool.clone());
    let token_repo = BootstrapTokenRepository::new(pool.clone());
    let cert_issuer = Arc::new(MockCertIssuer);
    let vtep_repo = VtepRepository::new(pool.clone());
    let service =
        EnrollmentServiceImplementation::new(node_repo, token_repo, Some(cert_issuer), vtep_repo);

    sqlx::query("INSERT INTO bootstrap_tokens (token_hash, one_time_use) VALUES (?, false)")
        .bind("a665a45920422f9d417e4867efdc4fb8a04a1f3fff1fa07e998e86f7f7a27ae3")
        .execute(&pool)
        .await
        .unwrap();

    let request = proto::EnrollmentRequest {
        bootstrap_token: "123".into(),
        inventory: Some(proto::NodeInventory {
            node_id: "node-fab-1".into(),
            hostname: "host-fab-1".into(),
            architecture: "x86_64".into(),
            cpu_threads: 8,
            memory_bytes: 16 * 1024 * 1024 * 1024,
            storage_classes: vec![],
            network_capabilities: vec![],
            hypervisor_capabilities: vec![],
            labels: std::collections::HashMap::new(),
            vtep_ip: String::new(),
            wireguard_public_key: "pub-key-material-base64".into(),
            underlay_mtu: 1500,
            authority_mode: Default::default(),
        }),
        versions: Some(proto::ServiceVersions {
            node_id: "node-fab-1".into(),
            chv_agent_version: "1.0.0".into(),
            chv_stord_version: "1.0.0".into(),
            chv_nwd_version: "1.0.0".into(),
            cloud_hypervisor_version: "40.0.0".into(),
            host_bundle_version: "1.2.3".into(),
        }),
    };

    service
        .enroll_node(request, None)
        .await
        .expect("enrollment with fabric identity must succeed");

    // The identity row exists with the public key, measured MTU, an
    // allocated fabric IP, and the legacy-column sentinel values.
    let identity = VtepRepository::new(pool.clone())
        .get_fabric_identity("node-fab-1")
        .await
        .unwrap()
        .expect("fabric identity must be registered");
    assert_eq!(
        identity.public_key.as_deref(),
        Some("pub-key-material-base64")
    );
    assert_eq!(identity.underlay_mtu, Some(1500));
    assert_eq!(identity.fabric_ip.as_deref(), Some("100.100.0.1"));
    assert_eq!(
        identity.underlay_endpoint, None,
        "enrollment without a transport peer address stores no endpoint"
    );

    // The periodic inventory re-report updates the identity in place and
    // keeps the allocated fabric IP (key rotation convergence, ADR-021 §5).
    let inventory_service = crate::inventory::InventoryServiceImplementation::new(
        NodeRepository::new(pool.clone()),
        VtepRepository::new(pool.clone()),
    );
    crate::inventory::InventoryService::report_node_inventory(
        &inventory_service,
        proto::ReportNodeInventoryRequest {
            meta: Some(proto::RequestMeta {
                operation_id: "op-inv-1".into(),
                requested_by: "test".into(),
                target_node_id: "node-fab-1".into(),
                desired_state_version: "1".into(),
                request_unix_ms: 1000,
            }),
            inventory: Some(proto::NodeInventory {
                node_id: "node-fab-1".into(),
                hostname: "host-fab-1".into(),
                architecture: "x86_64".into(),
                cpu_threads: 8,
                memory_bytes: 16 * 1024 * 1024 * 1024,
                storage_classes: vec![],
                network_capabilities: vec![],
                hypervisor_capabilities: vec![],
                labels: std::collections::HashMap::new(),
                vtep_ip: String::new(),
                wireguard_public_key: "pub-key-rotated".into(),
                underlay_mtu: 0,
                authority_mode: Default::default(),
            }),
        },
        // The transport-level peer address observed by tonic: the periodic
        // re-report derives the underlay endpoint from it.
        Some("198.51.100.7:54321".parse().unwrap()),
    )
    .await
    .expect("periodic inventory with fabric identity must succeed");

    let identity = VtepRepository::new(pool)
        .get_fabric_identity("node-fab-1")
        .await
        .unwrap()
        .expect("identity must survive re-report");
    assert_eq!(identity.public_key.as_deref(), Some("pub-key-rotated"));
    assert_eq!(
        identity.underlay_mtu, None,
        "unmeasured MTU (0) stores NULL"
    );
    assert_eq!(identity.fabric_ip.as_deref(), Some("100.100.0.1"));
    assert_eq!(
        identity.underlay_endpoint.as_deref(),
        Some("198.51.100.7:65001"),
        "the underlay endpoint must be derived from the observed peer \
         address pinned to the fabric WireGuard port"
    );
}

/// Enrolling through a transport whose remote address is known (as tonic
/// reports it in the server) must persist a derived underlay endpoint of
/// `<peer_ip>:<fabric WireGuard port>` alongside the fabric identity
/// (ADR-021 §5; the planner previously failed closed on a NULL endpoint).
#[tokio::test]
async fn test_enrollment_with_peer_addr_derives_underlay_endpoint() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    let node_repo = NodeRepository::new(pool.clone());
    let token_repo = BootstrapTokenRepository::new(pool.clone());
    let cert_issuer = Arc::new(MockCertIssuer);
    let vtep_repo = VtepRepository::new(pool.clone());
    let service =
        EnrollmentServiceImplementation::new(node_repo, token_repo, Some(cert_issuer), vtep_repo);

    // Reusable bootstrap token (sha256("123")).
    let hash = "a665a45920422f9d417e4867efdc4fb8a04a1f3fff1fa07e998e86f7f7a27ae3";
    sqlx::query("INSERT INTO bootstrap_tokens (token_hash, one_time_use) VALUES (?, false)")
        .bind(hash)
        .execute(&pool)
        .await
        .unwrap();

    let request = proto::EnrollmentRequest {
        bootstrap_token: "123".into(),
        inventory: Some(proto::NodeInventory {
            node_id: "node-fab-ep".into(),
            hostname: "host-fab-ep".into(),
            architecture: "x86_64".into(),
            cpu_threads: 8,
            memory_bytes: 16 * 1024 * 1024 * 1024,
            storage_classes: vec![],
            network_capabilities: vec![],
            hypervisor_capabilities: vec![],
            labels: std::collections::HashMap::new(),
            vtep_ip: String::new(),
            wireguard_public_key: "pub-key-ep-base64".into(),
            underlay_mtu: 1500,
            authority_mode: Default::default(),
        }),
        versions: Some(proto::ServiceVersions {
            node_id: "node-fab-ep".into(),
            chv_agent_version: "1.0.0".into(),
            chv_stord_version: "1.0.0".into(),
            chv_nwd_version: "1.0.0".into(),
            cloud_hypervisor_version: "40.0.0".into(),
            host_bundle_version: "1.2.3".into(),
        }),
    };

    // The ephemeral source port of the gRPC connection must NOT leak into
    // the stored endpoint: only the peer IP is used, pinned to the fabric
    // WireGuard port.
    let peer_addr: std::net::SocketAddr = "203.0.113.9:55555".parse().unwrap();
    service
        .enroll_node(request, Some(peer_addr))
        .await
        .expect("enrollment with a peer address must succeed");

    let identity = VtepRepository::new(pool)
        .get_fabric_identity("node-fab-ep")
        .await
        .unwrap()
        .expect("fabric identity must be registered");
    assert_eq!(
        identity.underlay_endpoint.as_deref(),
        Some("203.0.113.9:65001"),
        "underlay endpoint must be the observed peer IP pinned to the \
         fabric WireGuard port"
    );
}

/// The endpoint derivation must bracket IPv6 peer addresses so the stored
/// value parses as `host:port` (an unbracketed v6 address would glue the
/// port onto the last hextet), and keep the plain `ip:port` form for v4.
#[test]
fn test_derive_underlay_endpoint_formats_v4_and_v6() {
    let v4: std::net::SocketAddr = "203.0.113.9:55555".parse().unwrap();
    assert_eq!(
        crate::enrollment::derive_underlay_endpoint(v4),
        "203.0.113.9:65001"
    );

    let v6: std::net::SocketAddr = "[2001:db8::1]:55555".parse().unwrap();
    assert_eq!(
        crate::enrollment::derive_underlay_endpoint(v6),
        "[2001:db8::1]:65001",
        "an IPv6 peer must be bracketed before the port is appended"
    );

    // The derived endpoint must round-trip through SocketAddr parsing
    // (what the fabric plan compiler's peer validation expects).
    let parsed: std::net::SocketAddr = crate::enrollment::derive_underlay_endpoint(v6)
        .parse()
        .expect("a bracketed v6 endpoint must parse as a socket address");
    assert_eq!(parsed.port(), 65001);
    assert!(parsed.is_ipv6());
}

/// Enrolling over an IPv6 transport must persist a bracketed underlay
/// endpoint (round-2 review finding: the endpoint policy).
#[tokio::test]
async fn test_enrollment_with_ipv6_peer_addr_stores_bracketed_endpoint() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    let node_repo = NodeRepository::new(pool.clone());
    let token_repo = BootstrapTokenRepository::new(pool.clone());
    let cert_issuer = Arc::new(MockCertIssuer);
    let vtep_repo = VtepRepository::new(pool.clone());
    let service =
        EnrollmentServiceImplementation::new(node_repo, token_repo, Some(cert_issuer), vtep_repo);

    sqlx::query("INSERT INTO bootstrap_tokens (token_hash, one_time_use) VALUES (?, false)")
        .bind("a665a45920422f9d417e4867efdc4fb8a04a1f3fff1fa07e998e86f7f7a27ae3")
        .execute(&pool)
        .await
        .unwrap();

    let request = proto::EnrollmentRequest {
        bootstrap_token: "123".into(),
        inventory: Some(proto::NodeInventory {
            node_id: "node-fab-v6".into(),
            hostname: "host-fab-v6".into(),
            architecture: "x86_64".into(),
            cpu_threads: 8,
            memory_bytes: 16 * 1024 * 1024 * 1024,
            storage_classes: vec![],
            network_capabilities: vec![],
            hypervisor_capabilities: vec![],
            labels: std::collections::HashMap::new(),
            vtep_ip: String::new(),
            wireguard_public_key: "pub-key-v6-base64".into(),
            underlay_mtu: 1500,
            authority_mode: Default::default(),
        }),
        versions: Some(proto::ServiceVersions {
            node_id: "node-fab-v6".into(),
            chv_agent_version: "1.0.0".into(),
            chv_stord_version: "1.0.0".into(),
            chv_nwd_version: "1.0.0".into(),
            cloud_hypervisor_version: "40.0.0".into(),
            host_bundle_version: "1.2.3".into(),
        }),
    };

    let peer_addr: std::net::SocketAddr = "[2001:db8:42::1]:55555".parse().unwrap();
    service
        .enroll_node(request, Some(peer_addr))
        .await
        .expect("enrollment over an IPv6 transport must succeed");

    let identity = VtepRepository::new(pool)
        .get_fabric_identity("node-fab-v6")
        .await
        .unwrap()
        .expect("fabric identity must be registered");
    assert_eq!(
        identity.underlay_endpoint.as_deref(),
        Some("[2001:db8:42::1]:65001"),
        "an IPv6 peer address must be stored bracketed, pinned to the \
         fabric WireGuard port"
    );
}

/// First registration wins (round-2 review finding): a peer-derived
/// endpoint from a LATER re-report (e.g. a transient LB/proxy/VPN
/// reconnection observed as a different source address) must not replace
/// the endpoint pinned at enrollment.
#[tokio::test]
async fn test_inventory_re_report_does_not_rotate_pinned_underlay_endpoint() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    let node_repo = NodeRepository::new(pool.clone());
    let token_repo = BootstrapTokenRepository::new(pool.clone());
    let cert_issuer = Arc::new(MockCertIssuer);
    let vtep_repo = VtepRepository::new(pool.clone());
    let service =
        EnrollmentServiceImplementation::new(node_repo, token_repo, Some(cert_issuer), vtep_repo);

    sqlx::query("INSERT INTO bootstrap_tokens (token_hash, one_time_use) VALUES (?, false)")
        .bind("a665a45920422f9d417e4867efdc4fb8a04a1f3fff1fa07e998e86f7f7a27ae3")
        .execute(&pool)
        .await
        .unwrap();

    let request = proto::EnrollmentRequest {
        bootstrap_token: "123".into(),
        inventory: Some(proto::NodeInventory {
            node_id: "node-fab-pin".into(),
            hostname: "host-fab-pin".into(),
            architecture: "x86_64".into(),
            cpu_threads: 8,
            memory_bytes: 16 * 1024 * 1024 * 1024,
            storage_classes: vec![],
            network_capabilities: vec![],
            hypervisor_capabilities: vec![],
            labels: std::collections::HashMap::new(),
            vtep_ip: String::new(),
            wireguard_public_key: "pub-key-pin-base64".into(),
            underlay_mtu: 1500,
            authority_mode: Default::default(),
        }),
        versions: Some(proto::ServiceVersions {
            node_id: "node-fab-pin".into(),
            chv_agent_version: "1.0.0".into(),
            chv_stord_version: "1.0.0".into(),
            chv_nwd_version: "1.0.0".into(),
            cloud_hypervisor_version: "40.0.0".into(),
            host_bundle_version: "1.2.3".into(),
        }),
    };

    // Enrollment through the node's real address pins the endpoint.
    let enroll_peer: std::net::SocketAddr = "198.51.100.23:44444".parse().unwrap();
    service
        .enroll_node(request, Some(enroll_peer))
        .await
        .expect("enrollment must succeed");

    // The periodic inventory re-report arrives through a proxy/LB whose
    // address differs: the pinned endpoint must survive (last-writer-wins
    // would silently replace a previously-good endpoint).
    let inventory_service = crate::inventory::InventoryServiceImplementation::new(
        NodeRepository::new(pool.clone()),
        VtepRepository::new(pool.clone()),
    );
    crate::inventory::InventoryService::report_node_inventory(
        &inventory_service,
        proto::ReportNodeInventoryRequest {
            meta: Some(proto::RequestMeta {
                operation_id: "op-inv-pin".into(),
                requested_by: "test".into(),
                target_node_id: "node-fab-pin".into(),
                desired_state_version: "1".into(),
                request_unix_ms: 1000,
            }),
            inventory: Some(proto::NodeInventory {
                node_id: "node-fab-pin".into(),
                hostname: "host-fab-pin".into(),
                architecture: "x86_64".into(),
                cpu_threads: 8,
                memory_bytes: 16 * 1024 * 1024 * 1024,
                storage_classes: vec![],
                network_capabilities: vec![],
                hypervisor_capabilities: vec![],
                labels: std::collections::HashMap::new(),
                vtep_ip: String::new(),
                wireguard_public_key: "pub-key-rotated".into(),
                underlay_mtu: 1500,
                authority_mode: Default::default(),
            }),
        },
        Some("203.0.113.77:9999".parse().unwrap()),
    )
    .await
    .expect("periodic inventory must succeed");

    let identity = VtepRepository::new(pool)
        .get_fabric_identity("node-fab-pin")
        .await
        .unwrap()
        .expect("identity must survive re-report");
    assert_eq!(
        identity.underlay_endpoint.as_deref(),
        Some("198.51.100.23:65001"),
        "the endpoint pinned at first registration must not be rotated by \
         a later peer-derived re-report"
    );
    // The identity re-sync itself still works (key rotation converges).
    assert_eq!(identity.public_key.as_deref(), Some("pub-key-rotated"));
}

#[tokio::test]
async fn test_rotate_certificate_missing_node() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    let node_repo = NodeRepository::new(pool);
    let token_repo = BootstrapTokenRepository::new(test_db.pool.clone());
    let cert_issuer = Arc::new(MockCertIssuer);
    let vtep_repo = VtepRepository::new(test_db.pool.clone());
    let service =
        EnrollmentServiceImplementation::new(node_repo, token_repo, Some(cert_issuer), vtep_repo);

    let request = proto::RotateNodeCertificateRequest {
        node_id: "non-existent-node".into(),
        meta: Some(proto::RequestMeta {
            operation_id: "op-1".into(),
            requested_by: "test".into(),
            target_node_id: "non-existent-node".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
    };

    let result = service.rotate_node_certificate(request, None, false).await;
    match result {
        Err(ControlPlaneServiceError::NotFound(_)) => { /* Correct */ }
        other => panic!("Expected NotFound error for missing node, got {:?}", other),
    }
}

fn rotate_certificate_request(node_id: &str) -> proto::RotateNodeCertificateRequest {
    proto::RotateNodeCertificateRequest {
        node_id: node_id.into(),
        meta: Some(proto::RequestMeta {
            operation_id: "op-rotate".into(),
            requested_by: "test".into(),
            target_node_id: node_id.into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
    }
}

async fn seed_node_with_certificate_serial(
    repo: &NodeRepository,
    node_id: &str,
    serial: Option<&str>,
) {
    repo.upsert_node(&NodeUpsertInput {
        node_id: NodeId::new(node_id).unwrap(),
        hostname: "host".into(),
        display_name: node_id.into(),
        certificate_serial: serial.map(str::to_string),
        agent_version: Some("0.1.0".into()),
        control_plane_version: Some("0.1.0".into()),
        enrolled_unix_ms: 1000,
        last_seen_unix_ms: 1000,
    })
    .await
    .expect("seed node");
}

async fn rotate_test_service() -> (EnrollmentServiceImplementation, NodeRepository) {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let node_repo = NodeRepository::new(test_db.pool.clone());
    let token_repo = BootstrapTokenRepository::new(test_db.pool.clone());
    let vtep_repo = VtepRepository::new(test_db.pool.clone());
    let service = EnrollmentServiceImplementation::new(
        node_repo.clone(),
        token_repo,
        Some(Arc::new(MockCertIssuer)),
        vtep_repo,
    );
    (service, node_repo)
}

#[tokio::test]
async fn test_rotate_certificate_accepts_matching_presented_serial() {
    let (service, node_repo) = rotate_test_service().await;
    seed_node_with_certificate_serial(&node_repo, "node-rot", Some("old-serial")).await;

    service
        .rotate_node_certificate(
            rotate_certificate_request("node-rot"),
            Some("old-serial".into()),
            false,
        )
        .await
        .expect("matching serial must rotate");

    assert_eq!(
        node_repo
            .get_certificate_serial(&NodeId::new("node-rot").unwrap())
            .await
            .unwrap()
            .as_deref(),
        Some("mock-serial"),
        "rotation must persist the newly issued serial"
    );
}

#[tokio::test]
async fn test_rotate_certificate_rejects_stale_presented_serial() {
    let (service, node_repo) = rotate_test_service().await;
    seed_node_with_certificate_serial(&node_repo, "node-rot", Some("old-serial")).await;

    let result = service
        .rotate_node_certificate(
            rotate_certificate_request("node-rot"),
            Some("stale-serial".into()),
            false,
        )
        .await;

    match result {
        Err(ControlPlaneServiceError::PermissionDenied(msg)) => {
            assert!(msg.contains("serial"), "got: {msg}");
        }
        other => panic!("expected PermissionDenied for stale serial, got {other:?}"),
    }
    assert_eq!(
        node_repo
            .get_certificate_serial(&NodeId::new("node-rot").unwrap())
            .await
            .unwrap()
            .as_deref(),
        Some("old-serial"),
        "a rejected rotation must not mutate the stored serial"
    );
}

#[tokio::test]
async fn test_rotate_certificate_rejects_missing_presented_serial() {
    let (service, node_repo) = rotate_test_service().await;
    seed_node_with_certificate_serial(&node_repo, "node-rot", Some("old-serial")).await;

    let result = service
        .rotate_node_certificate(rotate_certificate_request("node-rot"), None, false)
        .await;

    assert!(
        matches!(result, Err(ControlPlaneServiceError::PermissionDenied(_))),
        "missing presented serial must fail closed, got {result:?}"
    );
    assert_eq!(
        node_repo
            .get_certificate_serial(&NodeId::new("node-rot").unwrap())
            .await
            .unwrap()
            .as_deref(),
        Some("old-serial")
    );
}

#[tokio::test]
async fn test_rotate_certificate_rejects_legacy_null_serial() {
    let (service, node_repo) = rotate_test_service().await;
    seed_node_with_certificate_serial(&node_repo, "node-legacy", None).await;

    let result = service
        .rotate_node_certificate(
            rotate_certificate_request("node-legacy"),
            Some("any-serial".into()),
            false,
        )
        .await;

    match result {
        Err(ControlPlaneServiceError::FailedPrecondition(msg)) => {
            assert!(
                msg.contains("re-enroll"),
                "legacy-node error must tell the operator to re-enroll; got: {msg}"
            );
        }
        other => panic!("expected FailedPrecondition for legacy NULL serial, got {other:?}"),
    }
    assert_eq!(
        node_repo
            .get_certificate_serial(&NodeId::new("node-legacy").unwrap())
            .await
            .unwrap(),
        None,
        "legacy NULL serial must not be bootstrapped by a rejected rotation"
    );
}

#[tokio::test]
async fn test_rotate_certificate_insecure_mode_succeeds_without_presented_serial() {
    let (service, node_repo) = rotate_test_service().await;
    seed_node_with_certificate_serial(&node_repo, "node-dev", Some("old-serial")).await;

    service
        .rotate_node_certificate(rotate_certificate_request("node-dev"), None, true)
        .await
        .expect("insecure/dev mode must preserve the bypass");

    assert_eq!(
        node_repo
            .get_certificate_serial(&NodeId::new("node-dev").unwrap())
            .await
            .unwrap()
            .as_deref(),
        Some("mock-serial")
    );
}

#[tokio::test]
async fn test_rotate_certificate_insecure_mode_bootstraps_legacy_null_serial() {
    let (service, node_repo) = rotate_test_service().await;
    seed_node_with_certificate_serial(&node_repo, "node-dev-legacy", None).await;

    service
        .rotate_node_certificate(rotate_certificate_request("node-dev-legacy"), None, true)
        .await
        .expect("insecure/dev mode must still update a legacy NULL serial");

    assert_eq!(
        node_repo
            .get_certificate_serial(&NodeId::new("node-dev-legacy").unwrap())
            .await
            .unwrap()
            .as_deref(),
        Some("mock-serial")
    );
}

#[tokio::test]
async fn test_report_bootstrap_result_persistence() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    let node_repo = NodeRepository::new(pool.clone());
    let token_repo = BootstrapTokenRepository::new(pool.clone());
    let cert_issuer = Arc::new(MockCertIssuer);
    let vtep_repo = VtepRepository::new(pool.clone());
    let service =
        EnrollmentServiceImplementation::new(node_repo, token_repo, Some(cert_issuer), vtep_repo);

    // Node must exist
    sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-b1', 'host-b1', 'host-b1')")
        .execute(&pool)
        .await
        .unwrap();

    let request = proto::ReportBootstrapResultRequest {
        node_id: "node-b1".into(),
        bootstrap_status: "SUCCESS".into(),
        message: "".into(),
        meta: Some(proto::RequestMeta {
            operation_id: "op-b1".into(),
            requested_by: "test".into(),
            target_node_id: "node-b1".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
    };

    service
        .report_bootstrap_result(request)
        .await
        .expect("Report failed");

    // Verify persistence
    let row =
        sqlx::query("SELECT success, operation_id FROM node_bootstrap_results WHERE node_id = ?")
            .bind("node-b1")
            .fetch_one(&pool)
            .await
            .unwrap();
    let success: bool = sqlx::Row::get(&row, "success");
    let operation_id: Option<String> = sqlx::Row::get(&row, "operation_id");
    assert!(success);
    assert_eq!(operation_id, Some("op-b1".to_string()));
}

#[tokio::test]
async fn test_ca_backed_issuer_issuance() {
    // Generate a temporary CA
    use rcgen::{CertificateParams, DistinguishedName, DnType, IsCa, KeyPair};
    let mut ca_params = CertificateParams::default();
    ca_params.distinguished_name = DistinguishedName::new();
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "Test CA");
    ca_params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);

    let ca_key_pair = KeyPair::generate().unwrap();
    let ca_cert = ca_params.self_signed(&ca_key_pair).unwrap();
    let ca_cert_pem = ca_cert.pem();
    let ca_key_pem = ca_key_pair.serialize_pem();

    let issuer =
        CaBackedCertificateIssuer::new(&ca_cert_pem, &ca_key_pem).expect("Failed to create issuer");
    let node_id = NodeId::new("test-node-1").unwrap();

    let issued = issuer
        .issue_node_certificate(&node_id)
        .await
        .expect("Failed to issue cert");

    // Basic format checks
    assert!(!issued.certificate_pem.is_empty());
    assert!(!issued.private_key_pem.is_empty());
    assert_eq!(issued.ca_pem, ca_cert_pem.as_bytes());

    // Cryptographic chain verification
    use x509_parser::prelude::*;

    // 1. Parse leaf cert from PEM
    let (_, leaf_pem) = parse_x509_pem(&issued.certificate_pem).expect("Failed to parse leaf PEM");
    let (_, leaf) =
        X509Certificate::from_der(&leaf_pem.contents).expect("Failed to parse leaf DER");

    // 2. Verify Subject Common Name
    let cn = leaf
        .subject()
        .iter_common_name()
        .next()
        .unwrap()
        .as_str()
        .unwrap();
    assert_eq!(cn, node_id.as_str());

    // 3. Verify Signature using CA public key
    let (_, ca_pem_parsed) =
        parse_x509_pem(ca_cert_pem.as_bytes()).expect("Failed to parse CA PEM");
    let (_, ca) =
        X509Certificate::from_der(&ca_pem_parsed.contents).expect("Failed to parse CA DER");

    // Verify signature
    leaf.verify_signature(Some(ca.public_key()))
        .expect("Certificate signature verification failed");
}

#[tokio::test]
async fn test_ca_backed_issuer_bounds_certificate_lifetime() {
    // H3 (#661 review): `CertificateParams::default()` yields 1975→4096.
    // Issued node certs must be short-lived so the denylist story is true.
    use rcgen::{CertificateParams, DistinguishedName, DnType, IsCa, KeyPair};
    let mut ca_params = CertificateParams::default();
    ca_params.distinguished_name = DistinguishedName::new();
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "Test CA");
    ca_params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_key_pair = KeyPair::generate().unwrap();
    let ca_cert = ca_params.self_signed(&ca_key_pair).unwrap();
    let issuer = CaBackedCertificateIssuer::new(&ca_cert.pem(), &ca_key_pair.serialize_pem())
        .expect("create issuer");

    let issued = issuer
        .issue_node_certificate(&NodeId::new("node-lifetime").unwrap())
        .await
        .expect("issue cert");

    use x509_parser::prelude::*;
    let (_, leaf_pem) = parse_x509_pem(&issued.certificate_pem).expect("parse leaf PEM");
    let (_, leaf) = X509Certificate::from_der(&leaf_pem.contents).expect("parse leaf DER");
    let validity = leaf.validity();
    let lifetime = validity.not_after.timestamp() - validity.not_before.timestamp();

    assert!(
        (89..=91).contains(&(lifetime / 86_400)),
        "lifetime must be bounded to ~90 days, got {lifetime}s"
    );
    let now = ::time::OffsetDateTime::now_utc().unix_timestamp();
    assert!(
        (validity.not_before.timestamp() - now).abs() < 15 * 60,
        "not_before must allow only a small clock-skew window"
    );
    assert!(
        validity.not_after.timestamp() > now,
        "issued certificate must not already be expired"
    );
}

#[tokio::test]
async fn test_ca_backed_issuer_serial_round_trips_through_peer_parser() {
    // #661 proof-of-possession relies on the peer parser recovering the exact
    // string that CaBackedCertificateIssuer stored at enrollment/rotation time.
    use rcgen::{CertificateParams, DistinguishedName, DnType, IsCa, KeyPair};
    let mut ca_params = CertificateParams::default();
    ca_params.distinguished_name = DistinguishedName::new();
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "Test CA");
    ca_params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);

    let ca_key_pair = KeyPair::generate().unwrap();
    let ca_cert = ca_params.self_signed(&ca_key_pair).unwrap();
    let issuer = CaBackedCertificateIssuer::new(&ca_cert.pem(), &ca_key_pair.serialize_pem())
        .expect("create issuer");

    let issued = issuer
        .issue_node_certificate(&NodeId::new("node-serial-round-trip").unwrap())
        .await
        .expect("issue cert");

    let (_, leaf_pem) =
        x509_parser::prelude::parse_x509_pem(&issued.certificate_pem).expect("parse leaf PEM");
    let parsed = crate::parse_serial_from_der(&leaf_pem.contents)
        .expect("parse serial")
        .expect("certificate must carry a textual serial");

    assert_eq!(
        parsed, issued.serial,
        "parser must return the exact serial stored by CaBackedCertificateIssuer"
    );
}

#[tokio::test]
async fn test_enrollment_rejects_invalid_bootstrap_token() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    let node_repo = NodeRepository::new(pool.clone());
    let token_repo = chv_controlplane_store::BootstrapTokenRepository::new(pool.clone());
    let cert_issuer = Arc::new(MockCertIssuer);
    let vtep_repo = VtepRepository::new(pool);
    let service =
        EnrollmentServiceImplementation::new(node_repo, token_repo, Some(cert_issuer), vtep_repo);

    let request = proto::EnrollmentRequest {
        bootstrap_token: "invalid-token".into(),
        inventory: Some(proto::NodeInventory {
            node_id: "node-invalid".into(),
            hostname: "host".into(),
            architecture: "x86_64".into(),
            cpu_threads: 1,
            memory_bytes: 1024,
            storage_classes: vec![],
            network_capabilities: vec![],
            hypervisor_capabilities: vec![],
            labels: Default::default(),
            vtep_ip: String::new(),
            wireguard_public_key: String::new(),
            underlay_mtu: 0,
            authority_mode: Default::default(),
        }),
        versions: Some(proto::ServiceVersions {
            node_id: "node-invalid".into(),
            chv_agent_version: "1.0.0".into(),
            chv_stord_version: "1.0.0".into(),
            chv_nwd_version: "1.0.0".into(),
            cloud_hypervisor_version: "1.0.0".into(),
            host_bundle_version: "1.0.0".into(),
        }),
    };

    let result = service.enroll_node(request, None).await;
    match result {
        Err(ControlPlaneServiceError::Unauthorized(_)) => { /* Correct */ }
        other => panic!(
            "Expected Unauthorized error for invalid token, got {:?}",
            other
        ),
    }
}

#[tokio::test]
async fn test_apply_vm_desired_state_persistence() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();

    // Insert a node to satisfy FK constraints
    sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-vm-1', 'host-vm-1', 'host-vm-1')")
        .execute(&pool)
        .await
        .unwrap();

    let node_repo = NodeRepository::new(pool.clone());
    let desired_state_repo = DesiredStateRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());

    let service = ReconcileServiceImplementation::new(
        node_repo,
        desired_state_repo,
        event_repo,
        ObservedStateRepository::new(pool.clone()),
        OperationRepository::new(pool.clone()),
    );

    let spec_json = r#"{"cpu_count": 2, "memory_bytes": 4294967296, "image_ref": "ubuntu-22.04"}"#;
    let request = proto::ApplyVmDesiredStateRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "".into(),
            requested_by: "test-user".into(),
            target_node_id: "node-vm-1".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-vm-1".into(),
        vm_id: "vm-1".into(),
        fragment: Some(proto::DesiredStateFragment {
            id: "vm-1".into(),
            kind: "Vm".into(),
            generation: "1".into(),
            spec_json: spec_json.as_bytes().to_vec(),
            policy_json: vec![],
            updated_at: "2024-01-01T00:00:00Z".into(),
            updated_by: "test-user".into(),
        }),
    };

    let result = service.apply_vm_desired_state(request).await;
    assert!(result.is_ok(), "Expected success, got {:?}", result);

    // Verify persistence in vm_desired_state
    let row = sqlx::query("SELECT vm_id, desired_generation FROM vm_desired_state WHERE vm_id = ?")
        .bind("vm-1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let vm_id: String = sqlx::Row::get(&row, "vm_id");
    let desired_generation: i64 = sqlx::Row::get(&row, "desired_generation");
    assert_eq!(vm_id, "vm-1");
    assert_eq!(desired_generation, 1);
}

#[tokio::test]
async fn test_apply_network_desired_state_with_exposures() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();

    // Insert a node to satisfy FK constraints
    sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-net-1', 'host-net-1', 'host-net-1')")
        .execute(&pool)
        .await
        .unwrap();

    let node_repo = NodeRepository::new(pool.clone());
    let desired_state_repo = DesiredStateRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());

    let service = ReconcileServiceImplementation::new(
        node_repo,
        desired_state_repo,
        event_repo,
        ObservedStateRepository::new(pool.clone()),
        OperationRepository::new(pool.clone()),
    );

    let spec_json = r#"{"network_class": "bridge", "exposures": [{"service_name": "web", "protocol": "tcp", "listen_port": 80, "target_port": 8080}]}"#;
    let request = proto::ApplyNetworkDesiredStateRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "".into(),
            requested_by: "test-user".into(),
            target_node_id: "node-net-1".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-net-1".into(),
        network_id: "net-1".into(),
        fragment: Some(proto::DesiredStateFragment {
            id: "net-1".into(),
            kind: "Network".into(),
            generation: "1".into(),
            spec_json: spec_json.as_bytes().to_vec(),
            policy_json: vec![],
            updated_at: "2024-01-01T00:00:00Z".into(),
            updated_by: "test-user".into(),
        }),
    };

    let result = service.apply_network_desired_state(request).await;
    assert!(result.is_ok(), "Expected success, got {:?}", result);

    // Verify persistence in network_exposures
    let row = sqlx::query(
        "SELECT network_id, service_name, listen_port FROM network_exposures WHERE network_id = ?",
    )
    .bind("net-1")
    .fetch_one(&pool)
    .await
    .unwrap();
    let network_id: String = sqlx::Row::get(&row, "network_id");
    let service_name: String = sqlx::Row::get(&row, "service_name");
    let listen_port: i32 = sqlx::Row::get(&row, "listen_port");
    assert_eq!(network_id, "net-1");
    assert_eq!(service_name, "web");
    assert_eq!(listen_port, 80);
}

#[tokio::test]
async fn test_apply_rejects_non_numeric_generation() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();

    let node_repo = NodeRepository::new(pool.clone());
    let desired_state_repo = DesiredStateRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());

    let service = ReconcileServiceImplementation::new(
        node_repo,
        desired_state_repo,
        event_repo,
        ObservedStateRepository::new(pool.clone()),
        OperationRepository::new(pool.clone()),
    );

    let request = proto::ApplyVmDesiredStateRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "op-bad-gen".into(),
            requested_by: "test-user".into(),
            target_node_id: "node-bad-gen".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-bad-gen".into(),
        vm_id: "vm-bad-gen".into(),
        fragment: Some(proto::DesiredStateFragment {
            id: "vm-bad-gen".into(),
            kind: "Vm".into(),
            generation: "not-a-number".into(),
            spec_json: b"{}".to_vec(),
            policy_json: vec![],
            updated_at: "2024-01-01T00:00:00Z".into(),
            updated_by: "test-user".into(),
        }),
    };

    let result = service.apply_vm_desired_state(request).await;
    match result {
        Err(ControlPlaneServiceError::InvalidArgument(msg)) => {
            assert!(
                msg.contains("generation must be numeric"),
                "Unexpected message: {}",
                msg
            );
        }
        other => panic!(
            "Expected InvalidArgument for non-numeric generation, got {:?}",
            other
        ),
    }
}

#[tokio::test]
async fn test_apply_node_desired_state_persistence() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    // seed node
    sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-reconcile', 'host', 'host')")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO operations (operation_id, idempotency_key, resource_kind, resource_id, operation_type, status, requested_at) VALUES ('op-node', 'idem-node', 'node', 'node-reconcile', 'Test', 'Pending', strftime('%Y-%m-%dT%H:%M:%SZ','now'))")
        .execute(&pool).await.unwrap();
    let node_repo = NodeRepository::new(pool.clone());
    let desired_repo = DesiredStateRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let service = ReconcileServiceImplementation::new(
        node_repo,
        desired_repo,
        event_repo,
        ObservedStateRepository::new(pool.clone()),
        OperationRepository::new(pool.clone()),
    );

    let req = proto::ApplyNodeDesiredStateRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "op-node".into(),
            requested_by: "test".into(),
            target_node_id: "node-reconcile".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-reconcile".into(),
        fragment: Some(proto::DesiredStateFragment {
            id: "node-reconcile".into(),
            kind: "Node".into(),
            generation: "1".into(),
            spec_json: br#"{"desired_state": "TenantReady"}"#.to_vec(),
            policy_json: vec![],
            updated_at: "".into(),
            updated_by: "".into(),
        }),
    };

    let resp = service.apply_node_desired_state(req).await.unwrap();
    assert_eq!(resp.result.unwrap().status, "ok");
}

#[tokio::test]
async fn test_apply_rejects_invalid_spec_json() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    let node_repo = NodeRepository::new(pool.clone());
    let desired_repo = DesiredStateRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let service = ReconcileServiceImplementation::new(
        node_repo,
        desired_repo,
        event_repo,
        ObservedStateRepository::new(pool.clone()),
        OperationRepository::new(pool.clone()),
    );

    let req = proto::ApplyVmDesiredStateRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "op-bad".into(),
            requested_by: "test".into(),
            target_node_id: "".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "".into(),
        vm_id: "vm-1".into(),
        fragment: Some(proto::DesiredStateFragment {
            id: "vm-1".into(),
            kind: "Vm".into(),
            generation: "1".into(),
            spec_json: br#"{"unknown_field": true}"#.to_vec(),
            policy_json: vec![],
            updated_at: "".into(),
            updated_by: "".into(),
        }),
    };

    let result = service.apply_vm_desired_state(req).await;
    match result {
        Err(ControlPlaneServiceError::InvalidArgument(_)) => { /* correct */ }
        other => panic!(
            "Expected InvalidArgument for unknown field, got {:?}",
            other
        ),
    }
}

#[tokio::test]
async fn test_create_vm_creates_operation() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();

    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-lifecycle-1', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let node_repo = NodeRepository::new(pool.clone());
    let operation_repo = OperationRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let desired_state_repo = DesiredStateRepository::new(pool.clone());

    let service = LifecycleServiceImplementation::new(
        node_repo,
        operation_repo,
        event_repo,
        desired_state_repo,
    );

    let request = proto::CreateVmRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "".into(),
            requested_by: "test-user".into(),
            target_node_id: "node-lifecycle-1".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-lifecycle-1".into(),
        vm: Some(proto::VmMutationSpec {
            vm_id: "vm-lifecycle-1".into(),
            vm_spec_json: b"{}".to_vec(),
        }),
    };

    let result = service.create_vm(request).await;
    assert!(result.is_ok(), "Expected success, got {:?}", result);

    let row = sqlx::query(
        "SELECT operation_id, status FROM operations WHERE operation_type = 'CreateVm'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let status: String = sqlx::Row::get(&row, "status");
    assert_eq!(status, "Accepted");
}

#[tokio::test]
async fn test_lifecycle_create_volume_shim_stays_closed() {
    // DP1 (#513, adopted): the standalone volume create journals
    // BFF-direct — the CP lifecycle shim must answer unimplemented
    // (naming that decision) and journal NOTHING. Pins the shim so a
    // future refactor cannot silently open a second journaling
    // pipeline behind this surface.
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();

    let node_repo = NodeRepository::new(pool.clone());
    let operation_repo = OperationRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let desired_state_repo = DesiredStateRepository::new(pool.clone());
    let service = LifecycleServiceImplementation::new(
        node_repo,
        operation_repo,
        event_repo,
        desired_state_repo,
    );

    let server = crate::server::LifecycleServer::new(Arc::new(service));
    use proto::lifecycle_service_server::LifecycleService as _;
    let result = server
        .create_volume(tonic::Request::new(proto::CreateVolumeRequest {
            meta: Some(proto::RequestMeta {
                operation_id: "op-shim-1".into(),
                requested_by: "test-user".into(),
                target_node_id: "node-shim-1".into(),
                desired_state_version: "1".into(),
                request_unix_ms: 1000,
            }),
            node_id: "node-shim-1".into(),
            volume: Some(proto::VolumeMutationSpec {
                volume_id: "vol-shim-1".into(),
                vm_id: String::new(),
                volume_spec_json: br#"{"size_bytes":4096}"#.to_vec(),
            }),
        }))
        .await;

    match result {
        Err(status) => {
            assert_eq!(status.code(), tonic::Code::Unimplemented);
            let msg = status.message();
            assert!(
                msg.contains("BFF"),
                "shim message should name the BFF-direct decision: {msg}"
            );
        }
        Ok(resp) => panic!(
            "Expected unimplemented from the create_volume shim, got {:?}",
            resp.into_inner()
        ),
    }

    // The shim must not journal anything.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM operations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        count, 0,
        "the create_volume shim must not journal operations"
    );
}

#[tokio::test]
async fn test_duplicate_idempotency_returns_same_operation() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();

    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-lifecycle-2', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let node_repo = NodeRepository::new(pool.clone());
    let operation_repo = OperationRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let desired_state_repo = DesiredStateRepository::new(pool.clone());

    let service = LifecycleServiceImplementation::new(
        node_repo,
        operation_repo,
        event_repo,
        desired_state_repo,
    );

    let request = proto::CreateVmRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "".into(),
            requested_by: "test-user".into(),
            target_node_id: "node-lifecycle-2".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-lifecycle-2".into(),
        vm: Some(proto::VmMutationSpec {
            vm_id: "vm-lifecycle-2".into(),
            vm_spec_json: b"{}".to_vec(),
        }),
    };

    let result1 = service.create_vm(request.clone()).await.unwrap();
    let result2 = service.create_vm(request).await.unwrap();

    assert_eq!(
        result1.result.unwrap().operation_id,
        result2.result.unwrap().operation_id
    );

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM operations WHERE operation_type = 'CreateVm'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn test_drain_node_updates_desired_state() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();

    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-lifecycle-3', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let node_repo = NodeRepository::new(pool.clone());
    let operation_repo = OperationRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let desired_state_repo = DesiredStateRepository::new(pool.clone());

    let service = LifecycleServiceImplementation::new(
        node_repo,
        operation_repo,
        event_repo,
        desired_state_repo,
    );

    let request = proto::DrainNodeRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "".into(),
            requested_by: "test-user".into(),
            target_node_id: "node-lifecycle-3".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-lifecycle-3".into(),
        allow_workload_stop: false,
    };

    let result = service.drain_node(request).await;
    assert!(result.is_ok(), "Expected success, got {:?}", result);

    let row = sqlx::query("SELECT desired_state FROM node_desired_state WHERE node_id = ?")
        .bind("node-lifecycle-3")
        .fetch_one(&pool)
        .await
        .unwrap();
    let desired_state: String = sqlx::Row::get(&row, "desired_state");
    assert_eq!(desired_state, "Draining");
}

#[tokio::test]
async fn test_enter_maintenance_updates_desired_state() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-maint', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let node_repo = NodeRepository::new(pool.clone());
    let op_repo = OperationRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let desired_repo = DesiredStateRepository::new(pool.clone());
    let service = LifecycleServiceImplementation::new(node_repo, op_repo, event_repo, desired_repo);

    let req = proto::EnterMaintenanceRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "op-maint".into(),
            requested_by: "test".into(),
            target_node_id: "node-maint".into(),
            desired_state_version: "2".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-maint".into(),
        reason: "upgrade".into(),
    };

    let resp = service.enter_maintenance(req).await.unwrap();
    assert_eq!(resp.result.unwrap().status, "OK");

    let row = sqlx::query("SELECT desired_state FROM node_desired_state WHERE node_id = ?")
        .bind("node-maint")
        .fetch_one(&pool)
        .await
        .unwrap();
    let state: String = sqlx::Row::get(&row, "desired_state");
    assert_eq!(state, "Maintenance");
}

#[tokio::test]
async fn test_create_vm_writes_desired_state() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-vm', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let node_repo = NodeRepository::new(pool.clone());
    let op_repo = OperationRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let desired_repo = DesiredStateRepository::new(pool.clone());
    let service = LifecycleServiceImplementation::new(node_repo, op_repo, event_repo, desired_repo);

    let req = proto::CreateVmRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "op-vm".into(),
            requested_by: "test".into(),
            target_node_id: "node-vm".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-vm".into(),
        vm: Some(proto::VmMutationSpec {
            vm_id: "vm-1".into(),
            vm_spec_json: br#"{"cpu_count": 2}"#.to_vec(),
        }),
    };

    let resp = service.create_vm(req).await.unwrap();
    assert_eq!(resp.result.unwrap().status, "OK");

    let row = sqlx::query("SELECT desired_power_state FROM vm_desired_state WHERE vm_id = ?")
        .bind("vm-1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let power: String = sqlx::Row::get(&row, "desired_power_state");
    assert_eq!(power, "Created");
}

#[tokio::test]
async fn test_lifecycle_invalid_node_id() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    let node_repo = NodeRepository::new(pool.clone());
    let op_repo = OperationRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let desired_repo = DesiredStateRepository::new(pool.clone());
    let service = LifecycleServiceImplementation::new(node_repo, op_repo, event_repo, desired_repo);

    let req = proto::DrainNodeRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "op-drain".into(),
            requested_by: "test".into(),
            target_node_id: "missing-node".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "missing-node".into(),
        allow_workload_stop: false,
    };

    let result = service.drain_node(req).await;
    match result {
        Err(ControlPlaneServiceError::NotFound(_)) => { /* correct */ }
        other => panic!("Expected NotFound for invalid node, got {:?}", other),
    }
}

#[test]
fn test_error_to_status_mapping() {
    use tonic::Status;
    let err = ControlPlaneServiceError::NotFound("node-x".into());
    let status: Status = err.into();
    assert_eq!(status.code(), tonic::Code::NotFound);

    let err = ControlPlaneServiceError::InvalidArgument("bad arg".into());
    let status: Status = err.into();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    let err = ControlPlaneServiceError::Unauthorized("no".into());
    let status: Status = err.into();
    assert_eq!(status.code(), tonic::Code::Unauthenticated);

    let err = ControlPlaneServiceError::Conflict("dup".into());
    let status: Status = err.into();
    assert_eq!(status.code(), tonic::Code::AlreadyExists);

    let err = ControlPlaneServiceError::StaleGeneration {
        expected: "5".into(),
        received: "3".into(),
    };
    let status: Status = err.into();
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
}

#[tokio::test]
async fn test_rotate_certificate_returns_not_found_status() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    let node_repo = NodeRepository::new(pool.clone());
    let token_repo = BootstrapTokenRepository::new(pool.clone());
    let cert_issuer = Arc::new(MockCertIssuer);
    let vtep_repo = VtepRepository::new(pool);
    let service =
        EnrollmentServiceImplementation::new(node_repo, token_repo, Some(cert_issuer), vtep_repo);
    let server = crate::server::EnrollmentServer::new(Arc::new(service));

    let request = tonic::Request::new(proto::RotateNodeCertificateRequest {
        node_id: "non-existent-node".into(),
        meta: Some(proto::RequestMeta {
            operation_id: "op-1".into(),
            requested_by: "test".into(),
            target_node_id: "non-existent-node".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
    });
    // Bypass mTLS peer-id check for this storage-error focused test.
    let mut request = request;
    request
        .extensions_mut()
        .insert(crate::peer_identity::InsecurePeer);

    let result = proto::enrollment_service_server::EnrollmentService::rotate_node_certificate(
        &server, request,
    )
    .await;
    match result {
        Err(status) => assert_eq!(status.code(), tonic::Code::NotFound),
        Ok(_) => panic!("Expected NotFound status"),
    }
}

// ============================================================
// Reconcile contract validation tests
// ============================================================

#[tokio::test]
async fn test_apply_vm_rejects_wrong_target_node() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-a', 'host', 'host'), ('node-b', 'host', 'host')")
        .execute(&pool).await.unwrap();

    let node_repo = NodeRepository::new(pool.clone());
    let desired_repo = DesiredStateRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let observed_repo = ObservedStateRepository::new(pool.clone());
    let op_repo = OperationRepository::new(pool.clone());
    let service = ReconcileServiceImplementation::new(
        node_repo,
        desired_repo,
        event_repo,
        observed_repo,
        op_repo,
    );

    let req = proto::ApplyVmDesiredStateRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "".into(),
            requested_by: "test".into(),
            target_node_id: "node-b".into(), // wrong node
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-a".into(),
        vm_id: "vm-1".into(),
        fragment: Some(proto::DesiredStateFragment {
            id: "vm-1".into(),
            kind: "Vm".into(),
            generation: "1".into(),
            spec_json: br#"{"cpu_count": 2}"#.to_vec(),
            policy_json: vec![],
            updated_at: "".into(),
            updated_by: "".into(),
        }),
    };

    match service.apply_vm_desired_state(req).await {
        Err(ControlPlaneServiceError::InvalidArgument(msg)) => {
            assert!(msg.contains("target_node_id mismatch"), "msg: {}", msg);
        }
        other => panic!(
            "Expected InvalidArgument for wrong target_node_id, got {:?}",
            other
        ),
    }
}

#[tokio::test]
async fn test_apply_vm_rejects_wrong_fragment_id() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-a', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let node_repo = NodeRepository::new(pool.clone());
    let desired_repo = DesiredStateRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let observed_repo = ObservedStateRepository::new(pool.clone());
    let op_repo = OperationRepository::new(pool.clone());
    let service = ReconcileServiceImplementation::new(
        node_repo,
        desired_repo,
        event_repo,
        observed_repo,
        op_repo,
    );

    let req = proto::ApplyVmDesiredStateRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "".into(),
            requested_by: "test".into(),
            target_node_id: "node-a".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-a".into(),
        vm_id: "vm-1".into(),
        fragment: Some(proto::DesiredStateFragment {
            id: "vm-WRONG".into(), // wrong id
            kind: "Vm".into(),
            generation: "1".into(),
            spec_json: br#"{"cpu_count": 2}"#.to_vec(),
            policy_json: vec![],
            updated_at: "".into(),
            updated_by: "".into(),
        }),
    };

    match service.apply_vm_desired_state(req).await {
        Err(ControlPlaneServiceError::InvalidArgument(msg)) => {
            assert!(msg.contains("fragment.id mismatch"), "msg: {}", msg);
        }
        other => panic!(
            "Expected InvalidArgument for wrong fragment id, got {:?}",
            other
        ),
    }
}

#[tokio::test]
async fn test_apply_vm_rejects_wrong_fragment_kind() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-a', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let node_repo = NodeRepository::new(pool.clone());
    let desired_repo = DesiredStateRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let observed_repo = ObservedStateRepository::new(pool.clone());
    let op_repo = OperationRepository::new(pool.clone());
    let service = ReconcileServiceImplementation::new(
        node_repo,
        desired_repo,
        event_repo,
        observed_repo,
        op_repo,
    );

    let req = proto::ApplyVmDesiredStateRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "".into(),
            requested_by: "test".into(),
            target_node_id: "node-a".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-a".into(),
        vm_id: "vm-1".into(),
        fragment: Some(proto::DesiredStateFragment {
            id: "vm-1".into(),
            kind: "Volume".into(), // wrong kind
            generation: "1".into(),
            spec_json: br#"{"cpu_count": 2}"#.to_vec(),
            policy_json: vec![],
            updated_at: "".into(),
            updated_by: "".into(),
        }),
    };

    match service.apply_vm_desired_state(req).await {
        Err(ControlPlaneServiceError::InvalidArgument(msg)) => {
            assert!(msg.contains("fragment.kind mismatch"), "msg: {}", msg);
        }
        other => panic!(
            "Expected InvalidArgument for wrong fragment kind, got {:?}",
            other
        ),
    }
}

// ============================================================
// Acknowledge desired state tests
// ============================================================

#[tokio::test]
async fn test_acknowledge_desired_state_version_persists_observed_generation() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-ack', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES ('vm-ack', 'vm-ack')")
        .execute(&pool)
        .await
        .unwrap();

    let node_repo = NodeRepository::new(pool.clone());
    let desired_repo = DesiredStateRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let observed_repo = ObservedStateRepository::new(pool.clone());
    let op_repo = OperationRepository::new(pool.clone());
    let service = ReconcileServiceImplementation::new(
        node_repo,
        desired_repo,
        event_repo,
        observed_repo,
        op_repo,
    );

    let req = proto::AcknowledgeDesiredStateVersionRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "".into(),
            requested_by: "test".into(),
            target_node_id: "node-ack".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-ack".into(),
        fragment_kind: "Vm".into(),
        fragment_id: "vm-ack".into(),
        observed_generation: "5".into(),
        apply_status: "ok".into(),
    };

    let resp = service
        .acknowledge_desired_state_version(req)
        .await
        .unwrap();
    assert_eq!(resp.result.unwrap().node_observed_generation, "5");

    let row = sqlx::query("SELECT observed_generation FROM vm_observed_state WHERE vm_id = ?")
        .bind("vm-ack")
        .fetch_one(&pool)
        .await
        .unwrap();
    let gen: i64 = sqlx::Row::get(&row, "observed_generation");
    assert_eq!(gen, 5);
}

#[tokio::test]
async fn test_acknowledge_advances_operation_to_succeeded() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-ack2', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES ('vm-ack2', 'vm-ack2')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO operations (operation_id, idempotency_key, resource_kind, resource_id, operation_type, status, requested_at) VALUES ('op-ack2', 'idem-ack2', 'vm', 'vm-ack2', 'Test', 'Pending', strftime('%Y-%m-%dT%H:%M:%SZ','now'))")
        .execute(&pool).await.unwrap();

    let node_repo = NodeRepository::new(pool.clone());
    let desired_repo = DesiredStateRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let observed_repo = ObservedStateRepository::new(pool.clone());
    let op_repo = OperationRepository::new(pool.clone());
    let service = ReconcileServiceImplementation::new(
        node_repo,
        desired_repo,
        event_repo,
        observed_repo,
        op_repo,
    );

    let req = proto::AcknowledgeDesiredStateVersionRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "op-ack2".into(),
            requested_by: "test".into(),
            target_node_id: "node-ack2".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-ack2".into(),
        fragment_kind: "Vm".into(),
        fragment_id: "vm-ack2".into(),
        observed_generation: "3".into(),
        apply_status: "".into(), // empty defaults to ok/Succeeded
    };

    service
        .acknowledge_desired_state_version(req)
        .await
        .unwrap();

    let row =
        sqlx::query("SELECT status, observed_generation FROM operations WHERE operation_id = ?")
            .bind("op-ack2")
            .fetch_one(&pool)
            .await
            .unwrap();
    let status: String = sqlx::Row::get(&row, "status");
    let gen: i64 = sqlx::Row::get(&row, "observed_generation");
    assert_eq!(status, "Succeeded");
    assert_eq!(gen, 3);
}

#[tokio::test]
async fn test_acknowledge_preserves_existing_vm_runtime_status() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-ack-runtime', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO vms (vm_id, display_name) VALUES ('vm-ack-runtime', 'vm-ack-runtime')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO vm_observed_state (vm_id, observed_generation, runtime_status, observed_at, updated_at) VALUES ('vm-ack-runtime', 1, 'Running', strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'))",
    )
    .execute(&pool)
    .await
    .unwrap();

    let service = ReconcileServiceImplementation::new(
        NodeRepository::new(pool.clone()),
        DesiredStateRepository::new(pool.clone()),
        EventRepository::new(pool.clone()),
        ObservedStateRepository::new(pool.clone()),
        OperationRepository::new(pool.clone()),
    );

    let req = proto::AcknowledgeDesiredStateVersionRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "".into(),
            requested_by: "test".into(),
            target_node_id: "node-ack-runtime".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-ack-runtime".into(),
        fragment_kind: "Vm".into(),
        fragment_id: "vm-ack-runtime".into(),
        observed_generation: "7".into(),
        apply_status: "conflict".into(),
    };

    service
        .acknowledge_desired_state_version(req)
        .await
        .unwrap();

    let row = sqlx::query(
        "SELECT observed_generation, runtime_status FROM vm_observed_state WHERE vm_id = ?",
    )
    .bind("vm-ack-runtime")
    .fetch_one(&pool)
    .await
    .unwrap();
    let generation: i64 = sqlx::Row::get(&row, "observed_generation");
    let runtime_status: String = sqlx::Row::get(&row, "runtime_status");
    assert_eq!(generation, 7);
    assert_eq!(runtime_status, "Running");
}

#[tokio::test]
async fn test_acknowledge_rejects_unknown_fragment_kind() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-ack3', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let node_repo = NodeRepository::new(pool.clone());
    let desired_repo = DesiredStateRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let observed_repo = ObservedStateRepository::new(pool.clone());
    let op_repo = OperationRepository::new(pool.clone());
    let service = ReconcileServiceImplementation::new(
        node_repo,
        desired_repo,
        event_repo,
        observed_repo,
        op_repo,
    );

    let req = proto::AcknowledgeDesiredStateVersionRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "".into(),
            requested_by: "test".into(),
            target_node_id: "node-ack3".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-ack3".into(),
        fragment_kind: "UnknownKind".into(),
        fragment_id: "x".into(),
        observed_generation: "1".into(),
        apply_status: "".into(),
    };

    match service.acknowledge_desired_state_version(req).await {
        Err(ControlPlaneServiceError::InvalidArgument(msg)) => {
            assert!(msg.contains("invalid fragment_kind"), "msg: {}", msg);
        }
        other => panic!(
            "Expected InvalidArgument for unknown fragment kind, got {:?}",
            other
        ),
    }
}

#[tokio::test]
async fn test_acknowledge_rejects_unknown_apply_status() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-ack4', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES ('vm-ack4', 'vm-ack4')")
        .execute(&pool)
        .await
        .unwrap();

    let service = ReconcileServiceImplementation::new(
        NodeRepository::new(pool.clone()),
        DesiredStateRepository::new(pool.clone()),
        EventRepository::new(pool.clone()),
        ObservedStateRepository::new(pool.clone()),
        OperationRepository::new(pool.clone()),
    );

    let req = proto::AcknowledgeDesiredStateVersionRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "".into(),
            requested_by: "test".into(),
            target_node_id: "node-ack4".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-ack4".into(),
        fragment_kind: "Vm".into(),
        fragment_id: "vm-ack4".into(),
        observed_generation: "2".into(),
        apply_status: "mystery-status".into(),
    };

    match service.acknowledge_desired_state_version(req).await {
        Err(ControlPlaneServiceError::InvalidArgument(msg)) => {
            assert!(msg.contains("invalid apply_status"), "msg: {}", msg);
        }
        other => panic!(
            "Expected InvalidArgument for unknown apply_status, got {:?}",
            other
        ),
    }
}

// ============================================================
// Lifecycle durable intent tests
// ============================================================

#[tokio::test]
async fn test_start_vm_persists_desired_power_state_running() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-start', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO vms (vm_id, node_id, display_name) VALUES ('vm-start', 'node-start', 'vm-start')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let node_repo = NodeRepository::new(pool.clone());
    let op_repo = OperationRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let desired_repo = DesiredStateRepository::new(pool.clone());
    let service = LifecycleServiceImplementation::new(node_repo, op_repo, event_repo, desired_repo);

    let req = proto::StartVmRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "op-start".into(),
            requested_by: "test".into(),
            target_node_id: "node-start".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-start".into(),
        vm_id: "vm-start".into(),
    };

    service.start_vm(req).await.unwrap();

    let row = sqlx::query(
        "SELECT desired_power_state, desired_status FROM vm_desired_state WHERE vm_id = ?",
    )
    .bind("vm-start")
    .fetch_one(&pool)
    .await
    .unwrap();
    let power: Option<String> = sqlx::Row::get(&row, "desired_power_state");
    let status: Option<String> = sqlx::Row::get(&row, "desired_status");
    assert_eq!(power, Some("Running".to_string()));
    assert!(status.is_none());
}

#[tokio::test]
async fn test_start_vm_preserves_existing_vm_shape() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-vm-preserve', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO vms (vm_id, node_id, display_name, tenant_id, placement_policy) VALUES ('vm-preserve', 'node-vm-preserve', 'vm-preserve', 'tenant-a', 'balanced')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO vm_desired_state (vm_id, desired_generation, desired_status, requested_at, updated_at, target_node_id, cpu_count, memory_bytes, image_ref, boot_mode, desired_power_state) VALUES ('vm-preserve', 1, 'seeded', strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'), 'node-vm-preserve', 4, 8192, 'image-a', 'uefi', 'Stopped')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let service = LifecycleServiceImplementation::new(
        NodeRepository::new(pool.clone()),
        OperationRepository::new(pool.clone()),
        EventRepository::new(pool.clone()),
        DesiredStateRepository::new(pool.clone()),
    );

    let req = proto::StartVmRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "".into(),
            requested_by: "test".into(),
            target_node_id: "node-vm-preserve".into(),
            desired_state_version: "2".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-vm-preserve".into(),
        vm_id: "vm-preserve".into(),
    };

    service.start_vm(req).await.unwrap();

    let row = sqlx::query(
        "SELECT cpu_count, memory_bytes, image_ref, boot_mode, desired_power_state FROM vm_desired_state WHERE vm_id = ?",
    )
    .bind("vm-preserve")
    .fetch_one(&pool)
    .await
    .unwrap();
    let cpu_count: Option<i32> = sqlx::Row::get(&row, "cpu_count");
    let memory_bytes: Option<i64> = sqlx::Row::get(&row, "memory_bytes");
    let image_ref: Option<String> = sqlx::Row::get(&row, "image_ref");
    let boot_mode: Option<String> = sqlx::Row::get(&row, "boot_mode");
    let power: Option<String> = sqlx::Row::get(&row, "desired_power_state");
    assert_eq!(cpu_count, Some(4));
    assert_eq!(memory_bytes, Some(8192));
    assert_eq!(image_ref, Some("image-a".to_string()));
    assert_eq!(boot_mode, Some("uefi".to_string()));
    assert_eq!(power, Some("Running".to_string()));
}

#[tokio::test]
async fn test_stop_vm_persists_desired_power_state_stopped() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-stop', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO vms (vm_id, node_id, display_name) VALUES ('vm-stop', 'node-stop', 'vm-stop')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let node_repo = NodeRepository::new(pool.clone());
    let op_repo = OperationRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let desired_repo = DesiredStateRepository::new(pool.clone());
    let service = LifecycleServiceImplementation::new(node_repo, op_repo, event_repo, desired_repo);

    let req = proto::StopVmRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "op-stop".into(),
            requested_by: "test".into(),
            target_node_id: "node-stop".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-stop".into(),
        vm_id: "vm-stop".into(),
        force: false,
    };

    service.stop_vm(req).await.unwrap();

    let row = sqlx::query("SELECT desired_power_state FROM vm_desired_state WHERE vm_id = ?")
        .bind("vm-stop")
        .fetch_one(&pool)
        .await
        .unwrap();
    let power: Option<String> = sqlx::Row::get(&row, "desired_power_state");
    assert_eq!(power, Some("Stopped".to_string()));
}

#[tokio::test]
async fn test_reboot_vm_persists_desired_power_state_running() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-reboot', 'host', 'host')")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO vms (vm_id, node_id, display_name) VALUES ('vm-reboot', 'node-reboot', 'vm-reboot')")
        .execute(&pool)
        .await
        .unwrap();

    let node_repo = NodeRepository::new(pool.clone());
    let op_repo = OperationRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let desired_repo = DesiredStateRepository::new(pool.clone());
    let service = LifecycleServiceImplementation::new(node_repo, op_repo, event_repo, desired_repo);

    let req = proto::RebootVmRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "op-reboot".into(),
            requested_by: "test".into(),
            target_node_id: "node-reboot".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-reboot".into(),
        vm_id: "vm-reboot".into(),
        force: false,
    };

    service.reboot_vm(req).await.unwrap();

    let row = sqlx::query("SELECT desired_power_state FROM vm_desired_state WHERE vm_id = ?")
        .bind("vm-reboot")
        .fetch_one(&pool)
        .await
        .unwrap();
    let power: Option<String> = sqlx::Row::get(&row, "desired_power_state");
    // A reboot's desired end-state is Running: the operation carries the
    // transient reboot, and the desired state must not strand itself on a
    // transient label nothing ever clears (the BFF renders desired power
    // state with priority over the observed state).
    assert_eq!(power, Some("Running".to_string()));
}

#[tokio::test]
async fn test_delete_vm_persists_desired_power_state_deleted() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-del', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO vms (vm_id, node_id, display_name) VALUES ('vm-del', 'node-del', 'vm-del')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let node_repo = NodeRepository::new(pool.clone());
    let op_repo = OperationRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let desired_repo = DesiredStateRepository::new(pool.clone());
    let service = LifecycleServiceImplementation::new(node_repo, op_repo, event_repo, desired_repo);

    let req = proto::DeleteVmRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "op-del".into(),
            requested_by: "test".into(),
            target_node_id: "node-del".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-del".into(),
        vm_id: "vm-del".into(),
        force: false,
    };

    service.delete_vm(req).await.unwrap();

    let row = sqlx::query("SELECT desired_power_state FROM vm_desired_state WHERE vm_id = ?")
        .bind("vm-del")
        .fetch_one(&pool)
        .await
        .unwrap();
    let power: Option<String> = sqlx::Row::get(&row, "desired_power_state");
    assert_eq!(power, Some("Deleted".to_string()));
}

#[tokio::test]
async fn test_attach_volume_persists_attached_vm_id() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-attach', 'host', 'host')")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES ('vm-attach', 'vm-attach')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO volumes (volume_id, node_id, display_name, capacity_bytes) VALUES ('vol-attach', 'node-attach', 'vol-attach', 1024)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let node_repo = NodeRepository::new(pool.clone());
    let op_repo = OperationRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let desired_repo = DesiredStateRepository::new(pool.clone());
    let service = LifecycleServiceImplementation::new(node_repo, op_repo, event_repo, desired_repo);

    let req = proto::AttachVolumeRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "op-attach".into(),
            requested_by: "test".into(),
            target_node_id: "node-attach".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-attach".into(),
        volume: Some(proto::VolumeMutationSpec {
            volume_id: "vol-attach".into(),
            vm_id: "vm-attach".into(),
            volume_spec_json: vec![],
        }),
    };

    service.attach_volume(req).await.unwrap();

    let row = sqlx::query("SELECT attached_vm_id FROM volume_desired_state WHERE volume_id = ?")
        .bind("vol-attach")
        .fetch_one(&pool)
        .await
        .unwrap();
    let vm_id: Option<String> = sqlx::Row::get(&row, "attached_vm_id");
    assert_eq!(vm_id, Some("vm-attach".to_string()));
}

#[tokio::test]
async fn test_detach_volume_clears_attached_vm_id() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-detach', 'host', 'host')")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES ('vm-detach', 'vm-detach')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO volumes (volume_id, node_id, display_name, capacity_bytes) VALUES ('vol-detach', 'node-detach', 'vol-detach', 1024)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let node_repo = NodeRepository::new(pool.clone());
    let op_repo = OperationRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let desired_repo = DesiredStateRepository::new(pool.clone());
    let service = LifecycleServiceImplementation::new(node_repo, op_repo, event_repo, desired_repo);

    let req = proto::DetachVolumeRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "op-detach".into(),
            requested_by: "test".into(),
            target_node_id: "node-detach".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-detach".into(),
        vm_id: "vm-detach".into(),
        volume_id: "vol-detach".into(),
        force: false,
    };

    service.detach_volume(req).await.unwrap();

    let row = sqlx::query("SELECT attached_vm_id FROM volume_desired_state WHERE volume_id = ?")
        .bind("vol-detach")
        .fetch_one(&pool)
        .await
        .unwrap();
    let vm_id: Option<String> = sqlx::Row::get(&row, "attached_vm_id");
    assert!(vm_id.is_none());
}

#[tokio::test]
async fn test_resize_volume_persists_resize_to_bytes() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-resize', 'host', 'host')")
        .execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO volumes (volume_id, node_id, display_name, capacity_bytes) VALUES ('vol-resize', 'node-resize', 'vol-resize', 1024)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let node_repo = NodeRepository::new(pool.clone());
    let op_repo = OperationRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let desired_repo = DesiredStateRepository::new(pool.clone());
    let service = LifecycleServiceImplementation::new(node_repo, op_repo, event_repo, desired_repo);

    let req = proto::ResizeVolumeRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "op-resize".into(),
            requested_by: "test".into(),
            target_node_id: "node-resize".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-resize".into(),
        volume_id: "vol-resize".into(),
        new_size_bytes: 21474836480,
    };

    service.resize_volume(req).await.unwrap();

    let row = sqlx::query("SELECT resize_to_bytes FROM volume_desired_state WHERE volume_id = ?")
        .bind("vol-resize")
        .fetch_one(&pool)
        .await
        .unwrap();
    let size: Option<i64> = sqlx::Row::get(&row, "resize_to_bytes");
    assert_eq!(size, Some(21474836480));
}

#[tokio::test]
async fn test_resize_volume_preserves_existing_volume_shape() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-vol-preserve', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO vms (vm_id, display_name) VALUES ('vm-preserve-attach', 'vm-preserve-attach')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO volumes (volume_id, node_id, display_name, capacity_bytes, volume_kind, storage_class) VALUES ('vol-preserve', 'node-vol-preserve', 'vol-preserve', 4096, 'Block', 'ssd')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO volume_desired_state (volume_id, desired_generation, desired_status, requested_at, updated_at, attached_vm_id, attachment_mode, device_name, read_only, resize_to_bytes) VALUES ('vol-preserve', 1, 'seeded', strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'), 'vm-preserve-attach', 'rw', '/dev/vdb', true, NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let service = LifecycleServiceImplementation::new(
        NodeRepository::new(pool.clone()),
        OperationRepository::new(pool.clone()),
        EventRepository::new(pool.clone()),
        DesiredStateRepository::new(pool.clone()),
    );

    let req = proto::ResizeVolumeRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "".into(),
            requested_by: "test".into(),
            target_node_id: "node-vol-preserve".into(),
            desired_state_version: "2".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-vol-preserve".into(),
        volume_id: "vol-preserve".into(),
        new_size_bytes: 16384,
    };

    service.resize_volume(req).await.unwrap();

    let volume = sqlx::query(
        "SELECT capacity_bytes, volume_kind, storage_class FROM volumes WHERE volume_id = ?",
    )
    .bind("vol-preserve")
    .fetch_one(&pool)
    .await
    .unwrap();
    let capacity_bytes: i64 = sqlx::Row::get(&volume, "capacity_bytes");
    let volume_kind: Option<String> = sqlx::Row::get(&volume, "volume_kind");
    let storage_class: Option<String> = sqlx::Row::get(&volume, "storage_class");
    assert_eq!(capacity_bytes, 4096);
    assert_eq!(volume_kind, Some("Block".to_string()));
    assert_eq!(storage_class, Some("ssd".to_string()));

    let desired = sqlx::query(
        "SELECT attached_vm_id, attachment_mode, device_name, read_only, resize_to_bytes FROM volume_desired_state WHERE volume_id = ?",
    )
    .bind("vol-preserve")
    .fetch_one(&pool)
    .await
    .unwrap();
    let attached_vm_id: Option<String> = sqlx::Row::get(&desired, "attached_vm_id");
    let attachment_mode: Option<String> = sqlx::Row::get(&desired, "attachment_mode");
    let device_name: Option<String> = sqlx::Row::get(&desired, "device_name");
    let read_only: bool = sqlx::Row::get(&desired, "read_only");
    let resize_to_bytes: Option<i64> = sqlx::Row::get(&desired, "resize_to_bytes");
    assert_eq!(attached_vm_id, Some("vm-preserve-attach".to_string()));
    assert_eq!(attachment_mode, Some("rw".to_string()));
    assert_eq!(device_name, Some("/dev/vdb".to_string()));
    assert!(read_only);
    assert_eq!(resize_to_bytes, Some(16384));
}

#[tokio::test]
async fn test_pause_node_scheduling_persists_scheduling_paused() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-pause', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO node_desired_state (node_id, desired_generation, desired_state, requested_at, updated_at, scheduling_paused) VALUES ('node-pause', 1, 'TenantReady', strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'), false)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let node_repo = NodeRepository::new(pool.clone());
    let op_repo = OperationRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let desired_repo = DesiredStateRepository::new(pool.clone());
    let service = LifecycleServiceImplementation::new(node_repo, op_repo, event_repo, desired_repo);

    let req = proto::PauseNodeSchedulingRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "op-pause".into(),
            requested_by: "test".into(),
            target_node_id: "node-pause".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-pause".into(),
    };

    service.pause_node_scheduling(req).await.unwrap();

    let row = sqlx::query("SELECT scheduling_paused FROM node_desired_state WHERE node_id = ?")
        .bind("node-pause")
        .fetch_one(&pool)
        .await
        .unwrap();
    let paused: bool = sqlx::Row::get(&row, "scheduling_paused");
    assert!(paused);
}

#[tokio::test]
async fn test_pause_node_scheduling_preserves_existing_desired_state() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('pause-preserve', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO node_desired_state (node_id, desired_generation, desired_state, requested_at, updated_at, scheduling_paused, allow_workload_stop) VALUES ('pause-preserve', 1, 'Draining', strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'), false, true)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let service = LifecycleServiceImplementation::new(
        NodeRepository::new(pool.clone()),
        OperationRepository::new(pool.clone()),
        EventRepository::new(pool.clone()),
        DesiredStateRepository::new(pool.clone()),
    );

    let req = proto::PauseNodeSchedulingRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "".into(),
            requested_by: "test".into(),
            target_node_id: "pause-preserve".into(),
            desired_state_version: "2".into(),
            request_unix_ms: 1000,
        }),
        node_id: "pause-preserve".into(),
    };

    service.pause_node_scheduling(req).await.unwrap();

    let row = sqlx::query(
        "SELECT desired_state, scheduling_paused, allow_workload_stop FROM node_desired_state WHERE node_id = ?",
    )
    .bind("pause-preserve")
    .fetch_one(&pool)
    .await
    .unwrap();
    let desired_state: String = sqlx::Row::get(&row, "desired_state");
    let scheduling_paused: bool = sqlx::Row::get(&row, "scheduling_paused");
    let allow_workload_stop: Option<bool> = sqlx::Row::get(&row, "allow_workload_stop");
    assert_eq!(desired_state, "Draining");
    assert!(scheduling_paused);
    assert_eq!(allow_workload_stop, Some(true));
}

#[tokio::test]
async fn test_resume_node_scheduling_clears_scheduling_paused() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-resume', 'host', 'host')")
        .execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO node_desired_state (node_id, desired_generation, desired_state, requested_at, updated_at, scheduling_paused) VALUES ('node-resume', 1, 'TenantReady', strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'), true)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let node_repo = NodeRepository::new(pool.clone());
    let op_repo = OperationRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let desired_repo = DesiredStateRepository::new(pool.clone());
    let service = LifecycleServiceImplementation::new(node_repo, op_repo, event_repo, desired_repo);

    let req = proto::ResumeNodeSchedulingRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "op-resume".into(),
            requested_by: "test".into(),
            target_node_id: "node-resume".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-resume".into(),
    };

    service.resume_node_scheduling(req).await.unwrap();

    let row = sqlx::query("SELECT scheduling_paused FROM node_desired_state WHERE node_id = ?")
        .bind("node-resume")
        .fetch_one(&pool)
        .await
        .unwrap();
    let paused: bool = sqlx::Row::get(&row, "scheduling_paused");
    assert!(!paused);
}

#[tokio::test]
async fn test_pause_node_scheduling_rejects_missing_desired_state() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('pause-missing', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let service = LifecycleServiceImplementation::new(
        NodeRepository::new(pool.clone()),
        OperationRepository::new(pool.clone()),
        EventRepository::new(pool.clone()),
        DesiredStateRepository::new(pool.clone()),
    );

    let req = proto::PauseNodeSchedulingRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "op-pause-missing".into(),
            requested_by: "test".into(),
            target_node_id: "pause-missing".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "pause-missing".into(),
    };

    let result = service.pause_node_scheduling(req).await;
    match result {
        Err(ControlPlaneServiceError::NotFound(msg)) => {
            assert!(msg.contains("node_desired_state"), "msg: {}", msg);
        }
        other => panic!(
            "Expected NotFound for missing desired state, got {:?}",
            other
        ),
    }

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM node_desired_state WHERE node_id = ?")
            .bind("pause-missing")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0, "no fabricated row should be created");
}

#[tokio::test]
async fn test_resume_node_scheduling_rejects_missing_desired_state() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('resume-missing', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let service = LifecycleServiceImplementation::new(
        NodeRepository::new(pool.clone()),
        OperationRepository::new(pool.clone()),
        EventRepository::new(pool.clone()),
        DesiredStateRepository::new(pool.clone()),
    );

    let req = proto::ResumeNodeSchedulingRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "op-resume-missing".into(),
            requested_by: "test".into(),
            target_node_id: "resume-missing".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "resume-missing".into(),
    };

    let result = service.resume_node_scheduling(req).await;
    match result {
        Err(ControlPlaneServiceError::NotFound(msg)) => {
            assert!(msg.contains("node_desired_state"), "msg: {}", msg);
        }
        other => panic!(
            "Expected NotFound for missing desired state, got {:?}",
            other
        ),
    }

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM node_desired_state WHERE node_id = ?")
            .bind("resume-missing")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0, "no fabricated row should be created");
}

#[tokio::test]
async fn test_exit_maintenance_persists_tenant_ready() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-exit', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let node_repo = NodeRepository::new(pool.clone());
    let op_repo = OperationRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let desired_repo = DesiredStateRepository::new(pool.clone());
    let service = LifecycleServiceImplementation::new(node_repo, op_repo, event_repo, desired_repo);

    let req = proto::ExitMaintenanceRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "op-exit".into(),
            requested_by: "test".into(),
            target_node_id: "node-exit".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-exit".into(),
    };

    service.exit_maintenance(req).await.unwrap();

    let row = sqlx::query(
        "SELECT desired_state, scheduling_paused FROM node_desired_state WHERE node_id = ?",
    )
    .bind("node-exit")
    .fetch_one(&pool)
    .await
    .unwrap();
    let state: String = sqlx::Row::get(&row, "desired_state");
    let paused: bool = sqlx::Row::get(&row, "scheduling_paused");
    assert_eq!(state, "TenantReady");
    assert!(!paused);
}

#[tokio::test]
async fn test_drain_node_persists_allow_workload_stop() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-drain2', 'host', 'host')")
        .execute(&pool).await.unwrap();

    let node_repo = NodeRepository::new(pool.clone());
    let op_repo = OperationRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let desired_repo = DesiredStateRepository::new(pool.clone());
    let service = LifecycleServiceImplementation::new(node_repo, op_repo, event_repo, desired_repo);

    let req = proto::DrainNodeRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "op-drain2".into(),
            requested_by: "test".into(),
            target_node_id: "node-drain2".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-drain2".into(),
        allow_workload_stop: true,
    };

    service.drain_node(req).await.unwrap();

    let row = sqlx::query("SELECT allow_workload_stop FROM node_desired_state WHERE node_id = ?")
        .bind("node-drain2")
        .fetch_one(&pool)
        .await
        .unwrap();
    let allow: Option<bool> = sqlx::Row::get(&row, "allow_workload_stop");
    assert_eq!(allow, Some(true));
}

#[tokio::test]
async fn test_lifecycle_operation_accepted_after_intent_persisted() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-op', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO vms (vm_id, node_id, display_name) VALUES ('vm-op', 'node-op', 'vm-op')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let node_repo = NodeRepository::new(pool.clone());
    let op_repo = OperationRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());
    let desired_repo = DesiredStateRepository::new(pool.clone());
    let service = LifecycleServiceImplementation::new(node_repo, op_repo, event_repo, desired_repo);

    let req = proto::StartVmRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "".into(),
            requested_by: "test".into(),
            target_node_id: "node-op".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-op".into(),
        vm_id: "vm-op".into(),
    };

    let resp = service.start_vm(req).await.unwrap();
    let op_id = resp.result.unwrap().operation_id;

    let row = sqlx::query("SELECT status FROM operations WHERE operation_id = ?")
        .bind(&op_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let status: String = sqlx::Row::get(&row, "status");
    assert_eq!(status, "Accepted");
}

#[tokio::test]
async fn test_resize_volume_payload_changes_affect_idempotency() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-idem-resize', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO volumes (volume_id, node_id, display_name, capacity_bytes) VALUES ('vol-idem-resize', 'node-idem-resize', 'vol-idem-resize', 1024)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let service = LifecycleServiceImplementation::new(
        NodeRepository::new(pool.clone()),
        OperationRepository::new(pool.clone()),
        EventRepository::new(pool.clone()),
        DesiredStateRepository::new(pool.clone()),
    );

    let req_one = proto::ResizeVolumeRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "".into(),
            requested_by: "test".into(),
            target_node_id: "node-idem-resize".into(),
            desired_state_version: "9".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-idem-resize".into(),
        volume_id: "vol-idem-resize".into(),
        new_size_bytes: 2048,
    };

    let req_two = proto::ResizeVolumeRequest {
        new_size_bytes: 4096,
        ..req_one.clone()
    };

    let op_one = service.resize_volume(req_one).await.unwrap();
    let op_two = service.resize_volume(req_two).await.unwrap();

    let op_id_one = op_one.result.unwrap().operation_id;
    let op_id_two = op_two.result.unwrap().operation_id;
    assert_ne!(op_id_one, op_id_two);

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM operations WHERE operation_type = 'ResizeVolume'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn test_start_vm_rejects_missing_vm() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-missing-vm', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let service = LifecycleServiceImplementation::new(
        NodeRepository::new(pool.clone()),
        OperationRepository::new(pool.clone()),
        EventRepository::new(pool.clone()),
        DesiredStateRepository::new(pool.clone()),
    );

    let req = proto::StartVmRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "".into(),
            requested_by: "test".into(),
            target_node_id: "node-missing-vm".into(),
            desired_state_version: "2".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-missing-vm".into(),
        vm_id: "vm-missing".into(),
    };

    match service.start_vm(req).await {
        Err(ControlPlaneServiceError::NotFound(msg)) => {
            assert!(msg.contains("vm"), "msg: {}", msg);
        }
        other => panic!("Expected NotFound for missing vm, got {:?}", other),
    }
}

#[tokio::test]
async fn test_resize_volume_rejects_missing_volume() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-missing-vol', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let service = LifecycleServiceImplementation::new(
        NodeRepository::new(pool.clone()),
        OperationRepository::new(pool.clone()),
        EventRepository::new(pool.clone()),
        DesiredStateRepository::new(pool.clone()),
    );

    let req = proto::ResizeVolumeRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "".into(),
            requested_by: "test".into(),
            target_node_id: "node-missing-vol".into(),
            desired_state_version: "2".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-missing-vol".into(),
        volume_id: "vol-missing".into(),
        new_size_bytes: 4096,
    };

    match service.resize_volume(req).await {
        Err(ControlPlaneServiceError::NotFound(msg)) => {
            assert!(msg.contains("volume"), "msg: {}", msg);
        }
        other => panic!("Expected NotFound for missing volume, got {:?}", other),
    }
}

#[tokio::test]
async fn test_start_vm_fails_operation_when_vm_missing() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-op-fail', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let service = LifecycleServiceImplementation::new(
        NodeRepository::new(pool.clone()),
        OperationRepository::new(pool.clone()),
        EventRepository::new(pool.clone()),
        DesiredStateRepository::new(pool.clone()),
    );

    let req = proto::StartVmRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "op-fail-test".into(),
            requested_by: "test".into(),
            target_node_id: "node-op-fail".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-op-fail".into(),
        vm_id: "vm-missing-op".into(),
    };

    let result = service.start_vm(req).await;
    match result {
        Err(ControlPlaneServiceError::NotFound(msg)) => {
            assert!(msg.contains("vm"), "msg: {}", msg);
        }
        other => panic!("Expected NotFound for missing vm, got {:?}", other),
    }

    // Look up the operation by idempotency key to get the actual operation_id
    let operation_id: String =
        sqlx::query_scalar("SELECT operation_id FROM operations WHERE idempotency_key = ?")
            .bind("request:op-fail-test")
            .fetch_one(&pool)
            .await
            .unwrap();

    // Operation should be Failed, not Pending or Accepted
    let row = sqlx::query("SELECT status FROM operations WHERE operation_id = ?")
        .bind(&operation_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let status: String = sqlx::Row::get(&row, "status");
    assert_eq!(status, "Failed");

    // OperationFailed event should exist
    let event = sqlx::query(
        "SELECT event_type FROM events WHERE operation_id = ? AND event_type = 'OperationFailed'",
    )
    .bind(&operation_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let event_type: String = sqlx::Row::get(&event, "event_type");
    assert_eq!(event_type, "OperationFailed");
}

#[tokio::test]
async fn test_failed_operation_can_be_retried_idempotently() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-op-retry', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let service = LifecycleServiceImplementation::new(
        NodeRepository::new(pool.clone()),
        OperationRepository::new(pool.clone()),
        EventRepository::new(pool.clone()),
        DesiredStateRepository::new(pool.clone()),
    );

    let req = proto::StartVmRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "op-retry-test".into(),
            requested_by: "test".into(),
            target_node_id: "node-op-retry".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: "node-op-retry".into(),
        vm_id: "vm-retry-later".into(),
    };

    // First attempt fails because VM does not exist
    let result1 = service.start_vm(req.clone()).await;
    assert!(result1.is_err(), "first attempt should fail");

    // Retrieve the actual generated operation_id
    let operation_id: String =
        sqlx::query_scalar("SELECT operation_id FROM operations WHERE idempotency_key = ?")
            .bind("request:op-retry-test")
            .fetch_one(&pool)
            .await
            .unwrap();

    // Create the VM row so the retry can succeed
    sqlx::query(
        "INSERT INTO vms (vm_id, node_id, display_name) VALUES ('vm-retry-later', 'node-op-retry', 'vm-retry-later')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Retry with the exact same request
    let resp2 = service.start_vm(req).await.unwrap();
    let op_id2 = resp2.result.unwrap().operation_id;

    // Should get the same operation idempotently
    assert_eq!(op_id2, operation_id);

    let row = sqlx::query("SELECT status FROM operations WHERE operation_id = ?")
        .bind(&operation_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let status: String = sqlx::Row::get(&row, "status");
    assert_eq!(status, "Accepted");
}

// ============================================================
// Orchestrator merge logic tests
// ============================================================

#[tokio::test]
async fn test_vm_override_takes_precedence_over_global() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();

    // Seed node, vm, vm_desired_state
    sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-merge-1', 'host', 'host')")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO vms (vm_id, node_id, display_name, hv_cpu_nested, hv_memory_shared) VALUES ('vm-merge-1', 'node-merge-1', 'vm', 1, 1)")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO vm_desired_state (vm_id, desired_generation, cpu_count, memory_bytes) VALUES ('vm-merge-1', 1, 2, 1073741824)")
        .execute(&pool).await.unwrap();

    // Set global settings opposite to VM overrides
    sqlx::query("UPDATE hypervisor_settings SET cpu_nested = 0, memory_shared = 0 WHERE id = 1")
        .execute(&pool)
        .await
        .unwrap();

    let orchestrator = crate::Orchestrator::new(
        pool.clone(),
        chv_controlplane_store::OperationRepository::new(pool.clone()),
        "/tmp/chv-{node_id}.sock".to_string(),
        "/var/lib/chv/vmlinux".to_string(),
        "/var/lib/chv/CLOUDHV.fd".to_string(),
        crate::NodeClientPool::new(),
        crate::convergence_metrics::new_shared(),
    );

    let spec_json = orchestrator
        .build_agent_vm_spec("vm-merge-1")
        .await
        .unwrap();
    let spec: serde_json::Value = serde_json::from_str(&spec_json).unwrap();
    let hv = spec["hypervisor_overrides"].as_object().unwrap();

    assert_eq!(
        hv["cpu_nested"], true,
        "VM override should take precedence over global"
    );
    assert_eq!(
        hv["memory_shared"], true,
        "VM override should take precedence over global"
    );
}

#[tokio::test]
async fn test_global_setting_takes_precedence_over_default() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();

    sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-merge-2', 'host', 'host')")
        .execute(&pool).await.unwrap();
    // VM has NULL overrides
    sqlx::query("INSERT INTO vms (vm_id, node_id, display_name) VALUES ('vm-merge-2', 'node-merge-2', 'vm')")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO vm_desired_state (vm_id, desired_generation, cpu_count, memory_bytes) VALUES ('vm-merge-2', 1, 2, 1073741824)")
        .execute(&pool).await.unwrap();

    // Set global to non-default values (default cpu_nested=true, memory_shared=false)
    sqlx::query("UPDATE hypervisor_settings SET cpu_nested = 0, memory_shared = 1 WHERE id = 1")
        .execute(&pool)
        .await
        .unwrap();

    let orchestrator = crate::Orchestrator::new(
        pool.clone(),
        chv_controlplane_store::OperationRepository::new(pool.clone()),
        "/tmp/chv-{node_id}.sock".to_string(),
        "/var/lib/chv/vmlinux".to_string(),
        "/var/lib/chv/CLOUDHV.fd".to_string(),
        crate::NodeClientPool::new(),
        crate::convergence_metrics::new_shared(),
    );

    let spec_json = orchestrator
        .build_agent_vm_spec("vm-merge-2")
        .await
        .unwrap();
    let spec: serde_json::Value = serde_json::from_str(&spec_json).unwrap();
    let hv = spec["hypervisor_overrides"].as_object().unwrap();

    assert_eq!(
        hv["cpu_nested"], false,
        "Global setting should override default"
    );
    assert_eq!(
        hv["memory_shared"], true,
        "Global setting should override default"
    );
}

#[tokio::test]
async fn test_null_vm_overrides_use_global_settings() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();

    sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-merge-3', 'host', 'host')")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO vms (vm_id, node_id, display_name) VALUES ('vm-merge-3', 'node-merge-3', 'vm')")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO vm_desired_state (vm_id, desired_generation, cpu_count, memory_bytes) VALUES ('vm-merge-3', 1, 2, 1073741824)")
        .execute(&pool).await.unwrap();

    // Explicit global values
    sqlx::query("UPDATE hypervisor_settings SET cpu_amx = 1, rng_src = '/dev/random', serial_mode = 'File' WHERE id = 1")
        .execute(&pool).await.unwrap();

    let orchestrator = crate::Orchestrator::new(
        pool.clone(),
        chv_controlplane_store::OperationRepository::new(pool.clone()),
        "/tmp/chv-{node_id}.sock".to_string(),
        "/var/lib/chv/vmlinux".to_string(),
        "/var/lib/chv/CLOUDHV.fd".to_string(),
        crate::NodeClientPool::new(),
        crate::convergence_metrics::new_shared(),
    );

    let spec_json = orchestrator
        .build_agent_vm_spec("vm-merge-3")
        .await
        .unwrap();
    let spec: serde_json::Value = serde_json::from_str(&spec_json).unwrap();
    let hv = spec["hypervisor_overrides"].as_object().unwrap();

    assert_eq!(
        hv["cpu_amx"], true,
        "NULL VM override should fall back to global"
    );
    assert_eq!(
        hv["rng_src"], "/dev/random",
        "NULL VM override should fall back to global"
    );
    assert_eq!(
        hv["serial_mode"], "File",
        "NULL VM override should fall back to global"
    );
}

#[tokio::test]
async fn test_defaults_used_when_settings_query_fails() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();

    sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-merge-4', 'host', 'host')")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO vms (vm_id, node_id, display_name) VALUES ('vm-merge-4', 'node-merge-4', 'vm')")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO vm_desired_state (vm_id, desired_generation, cpu_count, memory_bytes) VALUES ('vm-merge-4', 1, 2, 1073741824)")
        .execute(&pool).await.unwrap();

    // Remove the singleton row so get_settings() fails
    sqlx::query("DELETE FROM hypervisor_settings WHERE id = 1")
        .execute(&pool)
        .await
        .unwrap();

    let orchestrator = crate::Orchestrator::new(
        pool.clone(),
        chv_controlplane_store::OperationRepository::new(pool.clone()),
        "/tmp/chv-{node_id}.sock".to_string(),
        "/var/lib/chv/vmlinux".to_string(),
        "/var/lib/chv/CLOUDHV.fd".to_string(),
        crate::NodeClientPool::new(),
        crate::convergence_metrics::new_shared(),
    );

    let spec_json = orchestrator
        .build_agent_vm_spec("vm-merge-4")
        .await
        .unwrap();
    let spec: serde_json::Value = serde_json::from_str(&spec_json).unwrap();
    let hv = spec["hypervisor_overrides"].as_object().unwrap();

    // Verify hardcoded defaults from chv_common::hypervisor
    assert_eq!(hv["cpu_nested"], true);
    assert_eq!(hv["cpu_amx"], false);
    assert_eq!(hv["cpu_kvm_hyperv"], false);
    assert_eq!(hv["memory_mergeable"], false);
    assert_eq!(hv["memory_hugepages"], false);
    assert_eq!(hv["memory_shared"], false);
    assert_eq!(hv["memory_prefault"], false);
    assert_eq!(hv["iommu"], false);
    assert_eq!(hv["rng_src"], "/dev/urandom");
    assert_eq!(hv["watchdog"], false);
    assert_eq!(hv["landlock_enable"], false);
    // The serial transport default is Socket: cloud-hypervisor v43 gates
    // Pty-mode output until input arrives on the pty, which starves the
    // agent's passive console capture (see chv_common::hypervisor).
    assert_eq!(hv["serial_mode"], "Socket");
    assert_eq!(hv["console_mode"], "Off");
    assert_eq!(hv["pvpanic"], false);
    assert!(hv.get("tpm_type").is_none() || hv["tpm_type"].is_null());
    assert!(hv.get("tpm_socket_path").is_none() || hv["tpm_socket_path"].is_null());
}

#[tokio::test]
async fn test_migrated_serial_default_is_socket_everywhere() {
    // Migration 0054 flips the seeded serial default from Pty (which
    // cloud-hypervisor v43 gates against passive readers, starving the
    // agent's console capture) to Socket. Pin the migrated state of every
    // surface: the singleton global-settings row and the built-in
    // profiles must carry Socket so the orchestrator's normal
    // build_agent_vm_spec path dispatches the capture-capable transport.
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();

    let row = sqlx::query("SELECT serial_mode FROM hypervisor_settings WHERE id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let mode: String = sqlx::Row::get(&row, "serial_mode");
    assert_eq!(mode, "Socket");

    let profiles = sqlx::query(
        "SELECT id, serial_mode FROM hypervisor_profiles WHERE is_builtin = 1 ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(!profiles.is_empty(), "built-in profiles must exist");
    for profile in &profiles {
        let m: String = sqlx::Row::get(profile, "serial_mode");
        assert_eq!(
            m,
            "Socket",
            "built-in profile {} must carry Socket",
            sqlx::Row::get::<&str, _>(profile, "id")
        );
    }

    // The migration's safety guard: a deliberate non-Pty choice survives.
    // Re-run the 0054 statements verbatim against a row set to 'File' and
    // confirm they leave it alone (idempotent on already-migrated rows
    // with non-default values).
    sqlx::query("UPDATE hypervisor_settings SET serial_mode = 'File' WHERE id = 1")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE hypervisor_settings SET serial_mode = 'Socket', updated_at = datetime('now') WHERE id = 1 AND serial_mode = 'Pty'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE hypervisor_profiles SET serial_mode = 'Socket' WHERE is_builtin = 1 AND serial_mode = 'Pty'")
        .execute(&pool)
        .await
        .unwrap();
    let row = sqlx::query("SELECT serial_mode FROM hypervisor_settings WHERE id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let mode: String = sqlx::Row::get(&row, "serial_mode");
    assert_eq!(
        mode, "File",
        "deliberate non-Pty settings must survive the migration"
    );
}

#[tokio::test]
async fn test_post_merge_validation_rejects_iommu_without_memory_shared() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();

    sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-merge-5', 'host', 'host')")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO vms (vm_id, node_id, display_name, hv_iommu, hv_memory_shared) VALUES ('vm-merge-5', 'node-merge-5', 'vm', 1, 0)")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO vm_desired_state (vm_id, desired_generation, cpu_count, memory_bytes) VALUES ('vm-merge-5', 1, 2, 1073741824)")
        .execute(&pool).await.unwrap();

    let orchestrator = crate::Orchestrator::new(
        pool.clone(),
        chv_controlplane_store::OperationRepository::new(pool.clone()),
        "/tmp/chv-{node_id}.sock".to_string(),
        "/var/lib/chv/vmlinux".to_string(),
        "/var/lib/chv/CLOUDHV.fd".to_string(),
        crate::NodeClientPool::new(),
        crate::convergence_metrics::new_shared(),
    );

    let result = orchestrator.build_agent_vm_spec("vm-merge-5").await;
    match result {
        Err(chv_errors::ChvError::InvalidArgument { field, reason }) => {
            assert_eq!(field, "hypervisor_overrides");
            assert!(
                reason.contains("iommu=true requires memory_shared=true"),
                "reason: {}",
                reason
            );
        }
        other => panic!(
            "Expected InvalidArgument for iommu without memory_shared, got {:?}",
            other
        ),
    }
}

#[tokio::test]
async fn test_logout_clears_session_cookie() {
    use axum::body::Body;
    use axum::http::StatusCode;
    use tower::ServiceExt;

    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let app = crate::api::router::admin_router(
        test_app_state(test_db.pool.clone()),
        crate::convergence_metrics::new_shared(),
        chv_config::WebUiConfig::default(),
    );

    // Logout must terminate the cookie session: clear chv_session with
    // Max-Age=0 and the same attributes the login path sets.
    let response = app
        .oneshot(
            axum::http::Request::post("/api/v1/auth/logout")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let set_cookie = response
        .headers()
        .get(axum::http::header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .expect("logout must clear the session cookie");
    assert!(set_cookie.starts_with("chv_session=;"), "got: {set_cookie}");
    assert!(set_cookie.contains("Max-Age=0"), "got: {set_cookie}");
    assert!(set_cookie.contains("Path=/"), "got: {set_cookie}");
    assert!(set_cookie.contains("HttpOnly"), "got: {set_cookie}");
    assert!(set_cookie.contains("SameSite=Strict"), "got: {set_cookie}");
}

// ---------------------------------------------------------------------------
// resolve_inspect_required_operation relay (operator egress to the agent)
// ---------------------------------------------------------------------------

async fn resolve_relay_service() -> (LifecycleServiceImplementation, proto::RequestMeta) {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-resolve-1', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let service = LifecycleServiceImplementation::new(
        NodeRepository::new(pool.clone()),
        OperationRepository::new(pool.clone()),
        EventRepository::new(pool.clone()),
        DesiredStateRepository::new(pool.clone()),
    );
    let meta = proto::RequestMeta {
        operation_id: String::new(),
        requested_by: "operator".into(),
        target_node_id: "node-resolve-1".into(),
        desired_state_version: String::new(),
        request_unix_ms: 1,
    };
    (service, meta)
}

fn resolve_request(meta: &proto::RequestMeta) -> proto::ResolveInspectRequiredOperationRequest {
    proto::ResolveInspectRequiredOperationRequest {
        meta: Some(meta.clone()),
        vm_id: "vm-1".into(),
        operation_id: "op-1".into(),
        disposition: "failed".into(),
        note: "inspected: effect never started".into(),
    }
}

#[tokio::test]
async fn resolve_relay_fails_closed_without_node_egress() {
    let (service, meta) = resolve_relay_service().await;
    let result =
        LifecycleService::resolve_inspect_required_operation(&service, resolve_request(&meta))
            .await;
    assert!(matches!(
        result,
        Err(ControlPlaneServiceError::Unsupported(_))
    ));
}

#[tokio::test]
async fn resolve_relay_validates_payload_before_egress() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-resolve-2', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();
    // Egress is wired to a socket that does not exist: any request that
    // passes validation must fail by ATTEMPTING the relay (connection
    // error), proving validation happens first and the relay happens at
    // all.
    let socket_dir = tempfile::tempdir().unwrap();
    let service = LifecycleServiceImplementation::new(
        NodeRepository::new(pool.clone()),
        OperationRepository::new(pool.clone()),
        EventRepository::new(pool.clone()),
        DesiredStateRepository::new(pool.clone()),
    )
    .with_node_egress(
        NodeClientPool::new(),
        socket_dir
            .path()
            .join("agent-{node_id}.sock")
            .display()
            .to_string(),
    );
    let meta = proto::RequestMeta {
        operation_id: String::new(),
        requested_by: "operator".into(),
        target_node_id: "node-resolve-2".into(),
        desired_state_version: String::new(),
        request_unix_ms: 1,
    };

    // Missing meta → invalid argument, no relay.
    let mut request = resolve_request(&meta);
    request.meta = None;
    assert!(matches!(
        LifecycleService::resolve_inspect_required_operation(&service, request).await,
        Err(ControlPlaneServiceError::InvalidArgument(_))
    ));
    // Empty operation_id → invalid argument, no relay.
    let mut request = resolve_request(&meta);
    request.operation_id = "  ".into();
    assert!(matches!(
        LifecycleService::resolve_inspect_required_operation(&service, request).await,
        Err(ControlPlaneServiceError::InvalidArgument(_))
    ));
    // Bad disposition → invalid argument, no relay.
    let mut request = resolve_request(&meta);
    request.disposition = "maybe".into();
    assert!(matches!(
        LifecycleService::resolve_inspect_required_operation(&service, request).await,
        Err(ControlPlaneServiceError::InvalidArgument(_))
    ));
    // Missing note → invalid argument, no relay.
    let mut request = resolve_request(&meta);
    request.note = "   ".into();
    assert!(matches!(
        LifecycleService::resolve_inspect_required_operation(&service, request).await,
        Err(ControlPlaneServiceError::InvalidArgument(_))
    ));
    // Control characters in the note → invalid argument, no relay.
    let mut request = resolve_request(&meta);
    request.note = "line\ninjection".into();
    assert!(matches!(
        LifecycleService::resolve_inspect_required_operation(&service, request).await,
        Err(ControlPlaneServiceError::InvalidArgument(_))
    ));
    // Oversized note (the agent's 8000-byte journal bound) → invalid
    // argument at the relay, not an opaque internal error from the agent's
    // rejection after egress.
    let mut request = resolve_request(&meta);
    request.note = "x".repeat(8_001);
    assert!(matches!(
        LifecycleService::resolve_inspect_required_operation(&service, request).await,
        Err(ControlPlaneServiceError::InvalidArgument(_))
    ));
    // Empty vm_id → invalid argument, no relay.
    let mut request = resolve_request(&meta);
    request.vm_id = " ".into();
    assert!(matches!(
        LifecycleService::resolve_inspect_required_operation(&service, request).await,
        Err(ControlPlaneServiceError::InvalidArgument(_))
    ));
    // A target_node_id that is not a single safe path component would be
    // substituted into the agent socket pattern: rejected at the request
    // boundary (parse_node_id) before any socket resolution (injection
    // defense; resolve_agent_socket re-checks at substitution).
    let mut injected_meta = meta.clone();
    injected_meta.target_node_id = "../../etc/passwd".into();
    let result = LifecycleService::resolve_inspect_required_operation(
        &service,
        resolve_request(&injected_meta),
    )
    .await;
    assert!(matches!(
        result,
        Err(ControlPlaneServiceError::InvalidArgument(ref reason))
            if reason.contains("path component")
    ));
    // Valid payload → the relay is attempted; the socket does not exist, so
    // the failure is UNAVAILABLE (not internal) and names the node: the
    // caller can distinguish "bad request" from "node unreachable" and fall
    // back to node-local resolution.
    let result =
        LifecycleService::resolve_inspect_required_operation(&service, resolve_request(&meta))
            .await;
    match result {
        Err(ControlPlaneServiceError::NodeUnavailable(reason)) => {
            assert!(reason.contains("node-resolve-2"), "got: {reason}");
        }
        other => panic!("expected node-unavailable relay error, got {other:?}"),
    }
    // An empty requested_by defaults to "control-plane" (NodeClient parity)
    // instead of failing: the request still reaches egress.
    let mut anonymous_meta = meta.clone();
    anonymous_meta.requested_by = "  ".into();
    let result = LifecycleService::resolve_inspect_required_operation(
        &service,
        resolve_request(&anonymous_meta),
    )
    .await;
    match result {
        Err(ControlPlaneServiceError::NodeUnavailable(_)) => {}
        other => panic!("expected node-unavailable relay error, got {other:?}"),
    }
}

// ── #380: clone_volume must materialize the target volume row ──────────────
use chv_controlplane_types::domain::{Generation, ResourceId};
// The clone intent used to PATCH a `volume_desired_state` row for a target
// volume that nothing ever created — the FK violation surfaced as a bare
// `volume with id {target} not found` and `chvctl volume clone` always
// failed. The fix validates source/target up front and upserts the target
// (volumes row with the source's shape + the VDS intent row) in one tx.

async fn clone_test_service() -> (
    LifecycleServiceImplementation,
    chv_controlplane_store::StorePool,
) {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();

    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-clone-1', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let service = LifecycleServiceImplementation::new(
        NodeRepository::new(pool.clone()),
        OperationRepository::new(pool.clone()),
        EventRepository::new(pool.clone()),
        DesiredStateRepository::new(pool.clone()),
    );
    (service, pool)
}

fn clone_request(node: &str, source: &str, target: &str) -> proto::CloneVolumeRequest {
    proto::CloneVolumeRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "".into(),
            requested_by: "test-user".into(),
            target_node_id: node.into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        node_id: node.into(),
        source_volume_id: source.into(),
        target_volume_id: target.into(),
    }
}

#[tokio::test]
async fn clone_volume_creates_target_volume_row() {
    let (service, pool) = clone_test_service().await;

    // Seed the source volume (node + 10 GiB, local class) the way the
    // volume-fragment reconcile would.
    DesiredStateRepository::new(pool.clone())
        .upsert_volume(&VolumeDesiredStateInput {
            volume_id: ResourceId::new("vol-src-1").unwrap(),
            node_id: Some(NodeId::new("node-clone-1").unwrap()),
            display_name: "vol-src-1".into(),
            capacity_bytes: 10_737_418_240,
            volume_kind: Some("disk".into()),
            storage_class: Some("local".into()),
            // #381 review: the source carries an owner (BFF-created volumes
            // do); the clone target must inherit it or the BFF's
            // require_volume_owner makes the clone admin-only.
            owner_id: Some("user-clone-owner".into()),
            desired_generation: Generation::new(1),
            desired_status: None,
            requested_by: Some("test-user".into()),
            updated_by: None,
            attached_vm_id: None,
            attachment_mode: None,
            device_name: None,
            read_only: false,
            resize_to_bytes: None,
            snapshot_op: None,
            snapshot_name: None,
            clone_source_volume_id: None,
            requested_unix_ms: 1000,
        })
        .await
        .unwrap();

    let ack = service
        .clone_volume(clone_request("node-clone-1", "vol-src-1", "vol-dst-1"))
        .await
        .unwrap();
    let result = ack.result.expect("ack must carry result meta");
    assert_eq!(result.status, "OK", "clone must be accepted");

    // The target volume row exists with the source's shape — including the
    // inherited owner (#381 review: an ownerless row is admin-only in the
    // BFF, which would lock a non-admin cloner out of their own clone).
    let row = sqlx::query(
        "SELECT node_id, capacity_bytes, storage_class, owner_id FROM volumes WHERE volume_id = 'vol-dst-1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let node: String = sqlx::Row::get(&row, "node_id");
    let capacity: i64 = sqlx::Row::get(&row, "capacity_bytes");
    let class: String = sqlx::Row::get(&row, "storage_class");
    let owner: Option<String> = sqlx::Row::get(&row, "owner_id");
    assert_eq!(node, "node-clone-1");
    assert_eq!(capacity, 10_737_418_240);
    assert_eq!(class, "local");
    assert_eq!(owner.as_deref(), Some("user-clone-owner"));

    // The desired-state intent row records the clone source.
    let clone_source: Option<String> = sqlx::query_scalar(
        "SELECT clone_source_volume_id FROM volume_desired_state WHERE volume_id = 'vol-dst-1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(clone_source.as_deref(), Some("vol-src-1"));

    // A later owner-unaware re-upsert (the volume-fragment reconcile
    // shape: owner_id = NULL) must NOT strip the inherited owner.
    DesiredStateRepository::new(pool.clone())
        .upsert_volume(&VolumeDesiredStateInput {
            volume_id: ResourceId::new("vol-dst-1").unwrap(),
            node_id: Some(NodeId::new("node-clone-1").unwrap()),
            display_name: "vol-dst-1".into(),
            capacity_bytes: 10_737_418_240,
            volume_kind: Some("disk".into()),
            storage_class: Some("local".into()),
            owner_id: None,
            desired_generation: Generation::new(2),
            desired_status: None,
            requested_by: Some("fragment-reconcile".into()),
            updated_by: None,
            attached_vm_id: None,
            attachment_mode: None,
            device_name: None,
            read_only: false,
            resize_to_bytes: None,
            snapshot_op: None,
            snapshot_name: None,
            clone_source_volume_id: None,
            requested_unix_ms: 2000,
        })
        .await
        .unwrap();
    let preserved_owner: Option<String> =
        sqlx::query_scalar("SELECT owner_id FROM volumes WHERE volume_id = 'vol-dst-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        preserved_owner.as_deref(),
        Some("user-clone-owner"),
        "fragment reconcile must preserve the clone-inherited owner"
    );

    // The operation was journaled and accepted.
    let status: String =
        sqlx::query_scalar("SELECT status FROM operations WHERE operation_type = 'CloneVolume'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "Accepted");
}

#[tokio::test]
async fn clone_volume_rejects_existing_target() {
    let (service, pool) = clone_test_service().await;

    for id in ["vol-src-1", "vol-dst-1"] {
        DesiredStateRepository::new(pool.clone())
            .upsert_volume(&VolumeDesiredStateInput {
                volume_id: ResourceId::new(id).unwrap(),
                node_id: Some(NodeId::new("node-clone-1").unwrap()),
                display_name: id.into(),
                capacity_bytes: 1024,
                volume_kind: None,
                storage_class: None,
                owner_id: None,
                desired_generation: Generation::new(1),
                desired_status: None,
                requested_by: None,
                updated_by: None,
                attached_vm_id: None,
                attachment_mode: None,
                device_name: None,
                read_only: false,
                resize_to_bytes: None,
                snapshot_op: None,
                snapshot_name: None,
                clone_source_volume_id: None,
                requested_unix_ms: 1000,
            })
            .await
            .unwrap();
    }

    // Clone onto an EXISTING target id must fail up front — the upsert
    // would otherwise overwrite the existing volume's node/capacity.
    let result = service
        .clone_volume(clone_request("node-clone-1", "vol-src-1", "vol-dst-1"))
        .await;
    match result {
        Err(ControlPlaneServiceError::InvalidArgument(msg)) => {
            assert!(msg.contains("already exists"), "got: {msg}");
        }
        other => panic!("expected invalid-argument, got {other:?}"),
    }

    // And no CloneVolume operation was journaled for the rejection.
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM operations WHERE operation_type = 'CloneVolume'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0, "rejected clone must not journal an operation");
}

#[tokio::test]
async fn clone_volume_rejects_missing_source() {
    let (service, pool) = clone_test_service().await;

    let result = service
        .clone_volume(clone_request("node-clone-1", "vol-no-such", "vol-dst-2"))
        .await;
    match result {
        Err(ControlPlaneServiceError::NotFound(msg)) => {
            assert!(msg.contains("vol-no-such"), "got: {msg}");
        }
        other => panic!("expected not-found, got {other:?}"),
    }

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM operations WHERE operation_type = 'CloneVolume'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0, "rejected clone must not journal an operation");
}

/// Build a temp-file SQLite pool with the same pragma profile as prod
/// (WAL, busy_timeout) — the BFF quota-race suite's shape. The in-memory
/// `TestDb` pool cannot express the cross-connection write locking the
/// #384 race test pins (an in-memory database has no WAL mode). The
/// returned `TempDir` owns the database files; it is returned FIRST so
/// it drops LAST, after the pool's connections close, and cleans up on
/// scope exit — no pid-suffixed-dir accumulation under /tmp.
async fn clone_race_test_service() -> (
    tempfile::TempDir,
    crate::lifecycle::LifecycleServiceImplementation,
    StorePool,
) {
    use std::str::FromStr as _;
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("test.db").display());
    let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
        .unwrap()
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .busy_timeout(std::time::Duration::from_secs(5));
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(8)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect_with(opts)
        .await
        .unwrap();
    chv_controlplane_store::run_migrations(&pool, None)
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-clone-1', 'host', 'host')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let service = crate::lifecycle::LifecycleServiceImplementation::new(
        NodeRepository::new(pool.clone()),
        OperationRepository::new(pool.clone()),
        EventRepository::new(pool.clone()),
        DesiredStateRepository::new(pool.clone()),
    );
    (dir, service, pool)
}

/// Seed the clone source the way the volume-fragment reconcile would.
async fn seed_clone_source(pool: &StorePool, volume_id: &str, capacity_bytes: i64) {
    DesiredStateRepository::new(pool.clone())
        .upsert_volume(&VolumeDesiredStateInput {
            volume_id: ResourceId::new(volume_id).unwrap(),
            node_id: Some(NodeId::new("node-clone-1").unwrap()),
            display_name: volume_id.into(),
            capacity_bytes,
            volume_kind: Some("disk".into()),
            storage_class: Some("local".into()),
            owner_id: Some("user-clone-owner".into()),
            desired_generation: Generation::new(1),
            desired_status: None,
            requested_by: Some("test-user".into()),
            updated_by: None,
            attached_vm_id: None,
            attachment_mode: None,
            device_name: None,
            read_only: false,
            resize_to_bytes: None,
            snapshot_op: None,
            snapshot_name: None,
            clone_source_volume_id: None,
            requested_unix_ms: 1000,
        })
        .await
        .unwrap();
}

/// The #384 clone race, pinned at the lifecycle tier: two concurrent
/// `clone_volume` calls with the SAME caller-supplied target id (the BFF
/// shape: empty `operation_id`, fresh generations — distinct idempotency
/// keys, so both journal operations). Both pass the accept-time
/// pre-check before either commits; the strict insert inside the store's
/// `BEGIN IMMEDIATE` transaction must fail the loser closed with
/// `Conflict` (gRPC ALREADY_EXISTS / HTTP 409 through the BFF's
/// `map_ack`) instead of last-writer-wins overwriting the winner's row
/// and leaving two live conflicting operations on one target.
#[tokio::test]
async fn clone_volume_concurrent_same_target_yields_conflict_for_loser() {
    let (_dir, service, pool) = clone_race_test_service().await;
    seed_clone_source(&pool, "vol-src-1", 10_737_418_240).await;

    // Deterministic gate: hold the RESERVED lock so BOTH clones pass
    // their accept-time pre-checks (WAL readers never block) and
    // suspend at their first write — the operation journal INSERT —
    // inside the #384 race window, before either can materialize the
    // target. Releasing the gate lets both proceed; the strict insert
    // must then fail exactly one of them closed.
    let mut gate = pool.begin_with("BEGIN IMMEDIATE;").await.unwrap();
    sqlx::query("UPDATE nodes SET display_name = display_name WHERE node_id = 'node-clone-1'")
        .execute(&mut *gate)
        .await
        .unwrap();

    // Distinct desired_state_version => distinct idempotency keys, the
    // same shape two concurrent BFF requests produce.
    let a = {
        let service = service.clone();
        tokio::spawn(async move {
            service
                .clone_volume(clone_request("node-clone-1", "vol-src-1", "vol-dst-1"))
                .await
        })
    };
    let mut loser_request = clone_request("node-clone-1", "vol-src-1", "vol-dst-1");
    if let Some(meta) = loser_request.meta.as_mut() {
        meta.desired_state_version = "2".into();
    }
    let b = {
        let service = service.clone();
        tokio::spawn(async move { service.clone_volume(loser_request).await })
    };

    // Both pre-checks have now passed (both tasks are parked on the
    // journal write); open the window.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    gate.commit().await.unwrap();

    let results = vec![a.await.unwrap(), b.await.unwrap()];
    let winners = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(winners, 1, "exactly one clone must be accepted");
    for result in &results {
        match result {
            Ok(_) => {}
            // The race loser: NOT the pre-check's InvalidArgument (400) —
            // its request was well-formed and passed the pre-check; the
            // target appeared at persist time. Conflict is the honest
            // class (409 at the BFF tier).
            Err(ControlPlaneServiceError::Conflict(msg)) => {
                assert!(msg.contains("vol-dst-1"), "got: {msg}");
            }
            other => panic!("race loser must be a Conflict, got {other:?}"),
        }
    }

    // Exactly one target row, carrying the winner's (the source's) shape.
    let (count, capacity): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), MAX(capacity_bytes) FROM volumes WHERE volume_id = 'vol-dst-1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1, "no duplicate target row");
    assert_eq!(capacity, 10_737_418_240);

    // Both operations journaled (both passed the pre-check), but the
    // loser is Failed — no second live operation on the target.
    let statuses: Vec<(String, String)> = sqlx::query_as(
        "SELECT status, COALESCE(error_code, '') FROM operations WHERE operation_type = 'CloneVolume'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(statuses.len(), 2, "both racing clones journal operations");
    assert!(
        statuses.iter().any(|(s, _)| s == "Accepted"),
        "the winner must be Accepted: {statuses:?}"
    );
    assert!(
        statuses
            .iter()
            .any(|(s, code)| s == "Failed" && code == "INTENT_PERSISTENCE_FAILED"),
        "the loser must be Failed with the intent-persist error: {statuses:?}"
    );
}

/// #384 idempotent replay: a direct gRPC caller repeating
/// `meta.operation_id` re-runs the intent persist. The strict insert
/// must treat its own earlier materialization as an idempotent success
/// (same operation id, no Conflict, no duplicate row, no second write) —
/// only a racing DIFFERENT request gets the Conflict.
#[tokio::test]
async fn clone_volume_replayed_operation_id_is_idempotent() {
    let (service, pool) = clone_test_service().await;
    seed_clone_source(&pool, "vol-src-1", 10_737_418_240).await;

    let mut request = clone_request("node-clone-1", "vol-src-1", "vol-dst-1");
    if let Some(meta) = request.meta.as_mut() {
        meta.operation_id = "replay-op-384".into();
    }

    let first = service.clone_volume(request.clone()).await.unwrap();
    let first_result = first.result.expect("ack must carry result meta");
    assert_eq!(first_result.status, "OK");

    let (updated_at, requested_at): (String, String) = sqlx::query_as(
        "SELECT v.updated_at, vds.requested_at FROM volumes v \
         JOIN volume_desired_state vds ON v.volume_id = vds.volume_id \
         WHERE v.volume_id = 'vol-dst-1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    // The replay: same meta.operation_id, byte-for-byte.
    let second = service.clone_volume(request).await.unwrap();
    let second_result = second.result.expect("ack must carry result meta");
    assert_eq!(second_result.status, "OK", "replay must not Conflict");
    assert_eq!(
        second_result.operation_id, first_result.operation_id,
        "the replay must ack the SAME operation"
    );

    // One operation, one target row, nothing rewritten.
    let op_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM operations WHERE operation_type = 'CloneVolume'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(op_count, 1, "replay must not journal a second operation");

    let (row_count, updated_at_2, requested_at_2): (i64, String, String) = sqlx::query_as(
        "SELECT COUNT(*), MAX(v.updated_at), MAX(vds.requested_at) FROM volumes v \
         JOIN volume_desired_state vds ON v.volume_id = vds.volume_id \
         WHERE v.volume_id = 'vol-dst-1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row_count, 1, "replay must not duplicate the target row");
    assert_eq!(updated_at, updated_at_2, "replay must not rewrite volumes");
    assert_eq!(
        requested_at, requested_at_2,
        "replay must not rewrite the VDS intent row"
    );
}

// ── #378: accept-time rejection of the volume snapshot family on
//    core-managed nodes ─────────────────────────────────────────────────
//
// The agent's fail-closed dispatch (Unimplemented on every volume
// snapshot-family RPC in core-managed mode) is the enforcement; these
// tests pin the accept-time UX layer: the CP lifecycle rejects with
// InvalidArgument BEFORE journaling (no operations row, no
// volume_desired_state intent, no clone target volume row), fails OPEN
// on unknown/NULL mode, and leaves legacy nodes byte-for-byte unchanged.

async fn snapshot_family_test_service(
) -> (crate::lifecycle::LifecycleServiceImplementation, StorePool) {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    let service = crate::lifecycle::LifecycleServiceImplementation::new(
        NodeRepository::new(pool.clone()),
        OperationRepository::new(pool.clone()),
        EventRepository::new(pool.clone()),
        DesiredStateRepository::new(pool.clone()),
    );
    (service, pool)
}

/// Seed a node plus an inventory row carrying `mode` (`None` = the
/// column is NULL — never reported; there is no inventory row only when
/// the node is seeded with [`seed_node_without_inventory`]).
async fn seed_node_with_authority_mode(pool: &StorePool, node_id: &str, mode: Option<&str>) {
    sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES (?, 'host', 'host')")
        .bind(node_id)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO node_inventory (node_id, architecture, cpu_count, memory_bytes, authority_mode) \
         VALUES (?, 'x86_64', 1, 1024, ?)",
    )
    .bind(node_id)
    .bind(mode)
    .execute(pool)
    .await
    .unwrap();
}

/// A node that exists but has never reported inventory at all — the
/// fail-open edge for pre-#378 agents.
async fn seed_node_without_inventory(pool: &StorePool, node_id: &str) {
    sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES (?, 'host', 'host')")
        .bind(node_id)
        .execute(pool)
        .await
        .unwrap();
}

async fn seed_volume_on_node(pool: &StorePool, volume_id: &str, node_id: &str) {
    seed_volume(pool, volume_id, Some(node_id)).await;
}

/// A `volumes` row with a NULL `node_id` (the column is `ON DELETE SET
/// NULL`) — the volume's owning node is unknown, so the accept-time mode
/// check must fail open (#495: the helper resolves `volumes.node_id` and
/// never falls back to the request's node).
async fn seed_volume_without_node(pool: &StorePool, volume_id: &str) {
    seed_volume(pool, volume_id, None).await;
}

async fn seed_volume(pool: &StorePool, volume_id: &str, node_id: Option<&str>) {
    DesiredStateRepository::new(pool.clone())
        .upsert_volume(&VolumeDesiredStateInput {
            volume_id: ResourceId::new(volume_id).unwrap(),
            node_id: node_id.map(|n| NodeId::new(n).unwrap()),
            display_name: volume_id.into(),
            capacity_bytes: 1024,
            volume_kind: None,
            storage_class: None,
            owner_id: None,
            desired_generation: Generation::new(1),
            desired_status: None,
            requested_by: None,
            updated_by: None,
            attached_vm_id: None,
            attachment_mode: None,
            device_name: None,
            read_only: false,
            resize_to_bytes: None,
            snapshot_op: None,
            snapshot_name: None,
            clone_source_volume_id: None,
            requested_unix_ms: 1000,
        })
        .await
        .unwrap();
}

fn snapshot_family_meta(node: &str) -> proto::RequestMeta {
    proto::RequestMeta {
        operation_id: "".into(),
        requested_by: "test-user".into(),
        target_node_id: node.into(),
        desired_state_version: "1".into(),
        request_unix_ms: 1000,
    }
}

fn snapshot_request(node: &str, volume: &str) -> proto::SnapshotVolumeRequest {
    proto::SnapshotVolumeRequest {
        meta: Some(snapshot_family_meta(node)),
        node_id: node.into(),
        volume_id: volume.into(),
        snapshot_name: "snap-1".into(),
    }
}

fn restore_request(node: &str, volume: &str) -> proto::RestoreVolumeRequest {
    proto::RestoreVolumeRequest {
        meta: Some(snapshot_family_meta(node)),
        node_id: node.into(),
        volume_id: volume.into(),
        snapshot_name: "snap-1".into(),
    }
}

fn delete_snapshot_request(node: &str, volume: &str) -> proto::DeleteVolumeSnapshotRequest {
    proto::DeleteVolumeSnapshotRequest {
        meta: Some(snapshot_family_meta(node)),
        node_id: node.into(),
        volume_id: volume.into(),
        snapshot_name: "snap-1".into(),
    }
}

/// Assert the rejection journaled NOTHING: no operation row of any type
/// and no `volume_desired_state` snapshot intent for the volume. The
/// volume's own rows may pre-exist (the #495 volume-node resolution
/// needs a seeded `volumes` row) — what must not appear is an operation
/// or a `snapshot_op` intent written onto them.
async fn assert_nothing_journaled(pool: &StorePool, volume_id: &str) {
    let ops: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM operations")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(ops, 0, "a rejected request must not journal an operation");
    let intent: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM volume_desired_state WHERE volume_id = ? AND snapshot_op IS NOT NULL",
    )
    .bind(volume_id)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        intent, 0,
        "a rejected request must not write a volume_desired_state intent"
    );
}

#[tokio::test]
async fn snapshot_volume_rejects_core_managed_node() {
    let (service, pool) = snapshot_family_test_service().await;
    seed_node_with_authority_mode(&pool, "node-cm", Some("core-managed")).await;
    // #495: the check resolves the volume's node, so the volume must
    // live on the core-managed node (the BFF-equality shape: request
    // node == volume node).
    seed_volume_on_node(&pool, "vol-cm-1", "node-cm").await;

    let result = service
        .snapshot_volume(snapshot_request("node-cm", "vol-cm-1"))
        .await;
    match result {
        Err(ControlPlaneServiceError::InvalidArgument(msg)) => {
            assert_eq!(
                msg, "volume snapshot is not supported on core-managed nodes",
                "got: {msg}"
            );
        }
        other => panic!("expected invalid-argument, got {other:?}"),
    }
    assert_nothing_journaled(&pool, "vol-cm-1").await;
}

#[tokio::test]
async fn restore_volume_rejects_core_managed_node() {
    let (service, pool) = snapshot_family_test_service().await;
    seed_node_with_authority_mode(&pool, "node-cm", Some("core-managed")).await;
    seed_volume_on_node(&pool, "vol-cm-1", "node-cm").await;

    let result = service
        .restore_volume(restore_request("node-cm", "vol-cm-1"))
        .await;
    match result {
        Err(ControlPlaneServiceError::InvalidArgument(msg)) => {
            assert_eq!(
                msg, "volume restore is not supported on core-managed nodes",
                "got: {msg}"
            );
        }
        other => panic!("expected invalid-argument, got {other:?}"),
    }
    assert_nothing_journaled(&pool, "vol-cm-1").await;
}

#[tokio::test]
async fn delete_volume_snapshot_rejects_core_managed_node() {
    let (service, pool) = snapshot_family_test_service().await;
    seed_node_with_authority_mode(&pool, "node-cm", Some("core-managed")).await;
    seed_volume_on_node(&pool, "vol-cm-1", "node-cm").await;

    let result = service
        .delete_volume_snapshot(delete_snapshot_request("node-cm", "vol-cm-1"))
        .await;
    match result {
        Err(ControlPlaneServiceError::InvalidArgument(msg)) => {
            assert_eq!(
                msg, "volume snapshot deletion is not supported on core-managed nodes",
                "got: {msg}"
            );
        }
        other => panic!("expected invalid-argument, got {other:?}"),
    }
    assert_nothing_journaled(&pool, "vol-cm-1").await;
}

#[tokio::test]
async fn clone_volume_rejects_core_managed_placement_node() {
    let (service, pool) = snapshot_family_test_service().await;
    // The source lives on the core-managed node; the REQUEST names a
    // legacy node. The check must follow the placement node (the source's
    // node — the one the operation journals and dispatches to), not the
    // raw request node_id (#381 placement rule).
    seed_node_with_authority_mode(&pool, "node-cm", Some("core-managed")).await;
    seed_node_with_authority_mode(&pool, "node-leg", Some("legacy")).await;
    seed_volume_on_node(&pool, "vol-src-cm", "node-cm").await;

    let result = service
        .clone_volume(clone_request("node-leg", "vol-src-cm", "vol-dst-cm"))
        .await;
    match result {
        Err(ControlPlaneServiceError::InvalidArgument(msg)) => {
            assert_eq!(
                msg, "volume clone is not supported on core-managed nodes",
                "got: {msg}"
            );
        }
        other => panic!("expected invalid-argument, got {other:?}"),
    }
    assert_nothing_journaled(&pool, "vol-dst-cm").await;
    let target: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM volumes WHERE volume_id = 'vol-dst-cm'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        target, 0,
        "a rejected clone must not materialize the target volume row"
    );
}

#[tokio::test]
async fn snapshot_family_fails_open_on_unknown_mode() {
    // NULL column (reported, no mode) and no inventory row at all
    // (never reported — pre-#378 agents) must BOTH keep accepting: the
    // agent's fail-closed dispatch remains the enforcement, so the
    // accept-time check only fires on a definite core-managed report.
    let (service, pool) = snapshot_family_test_service().await;
    seed_node_with_authority_mode(&pool, "node-null", None).await;
    seed_node_without_inventory(&pool, "node-noreport").await;
    seed_volume_on_node(&pool, "vol-null-1", "node-null").await;
    seed_volume_on_node(&pool, "vol-noreport-1", "node-noreport").await;

    for (node, volume) in [
        ("node-null", "vol-null-1"),
        ("node-noreport", "vol-noreport-1"),
    ] {
        let ack = service
            .snapshot_volume(snapshot_request(node, volume))
            .await
            .unwrap_or_else(|e| panic!("unknown mode must fail open ({node}): {e:?}"));
        assert_eq!(
            ack.result.expect("ack result").status,
            "OK",
            "unknown mode must fail open ({node})"
        );
    }

    // And the accepts journaled as before: operation Accepted + snapshot
    // intent written.
    let statuses: Vec<String> =
        sqlx::query_scalar("SELECT status FROM operations WHERE operation_type = 'SnapshotVolume'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(statuses, vec!["Accepted".to_string(); 2]);
    let intent: Option<String> = sqlx::query_scalar(
        "SELECT snapshot_op FROM volume_desired_state WHERE volume_id = 'vol-null-1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(intent.as_deref(), Some("create"));
}

#[tokio::test]
async fn snapshot_volume_accepts_legacy_node_unchanged() {
    let (service, pool) = snapshot_family_test_service().await;
    seed_node_with_authority_mode(&pool, "node-leg", Some("legacy")).await;
    seed_volume_on_node(&pool, "vol-leg-1", "node-leg").await;

    let ack = service
        .snapshot_volume(snapshot_request("node-leg", "vol-leg-1"))
        .await
        .expect("legacy node must accept exactly as before");
    assert_eq!(ack.result.expect("ack result").status, "OK");

    let status: String =
        sqlx::query_scalar("SELECT status FROM operations WHERE operation_type = 'SnapshotVolume'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "Accepted");
    let intent: Option<String> = sqlx::query_scalar(
        "SELECT snapshot_op FROM volume_desired_state WHERE volume_id = 'vol-leg-1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(intent.as_deref(), Some("create"));
}

// ── #495: the check resolves the VOLUME's node (volumes.node_id — what
//    the orchestrator's dispatch uses), not the request's node_id. A
//    direct-gRPC caller can name any node; the mismatched shapes below
//    pin both directions plus the NULL-node fail-open. ────────────────

#[tokio::test]
async fn snapshot_family_ignores_core_managed_request_node_when_volume_on_legacy_node() {
    // Request names a core-managed node, volume is owned by a legacy
    // node: the dispatch goes to the LEGACY node (the orchestrator reads
    // volumes.node_id), so this must ACCEPT and journal as before — the
    // request-node check of the first cut would have 400'd an operation
    // that pre-#378 executed fine.
    let (service, pool) = snapshot_family_test_service().await;
    seed_node_with_authority_mode(&pool, "node-cm", Some("core-managed")).await;
    seed_node_with_authority_mode(&pool, "node-leg", Some("legacy")).await;
    seed_volume_on_node(&pool, "vol-mismatch-leg", "node-leg").await;

    let ack = service
        .snapshot_volume(snapshot_request("node-cm", "vol-mismatch-leg"))
        .await
        .expect("volume on legacy node must accept regardless of request node");
    assert_eq!(ack.result.expect("ack result").status, "OK");

    let statuses: Vec<String> =
        sqlx::query_scalar("SELECT status FROM operations WHERE operation_type = 'SnapshotVolume'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(statuses, vec!["Accepted".to_string()]);
    let intent: Option<String> = sqlx::query_scalar(
        "SELECT snapshot_op FROM volume_desired_state WHERE volume_id = 'vol-mismatch-leg'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(intent.as_deref(), Some("create"));
}

#[tokio::test]
async fn snapshot_family_rejects_legacy_request_node_when_volume_on_core_managed_node() {
    // Request names a legacy node, volume is owned by a core-managed
    // node: the dispatch goes to the CORE-MANAGED node, so this must
    // reject pre-journal — without the volume-node resolution the check
    // missed exactly the case it exists to catch (the ~70 s
    // Unimplemented-retry path).
    let (service, pool) = snapshot_family_test_service().await;
    seed_node_with_authority_mode(&pool, "node-cm", Some("core-managed")).await;
    seed_node_with_authority_mode(&pool, "node-leg", Some("legacy")).await;
    seed_volume_on_node(&pool, "vol-mismatch-cm", "node-cm").await;

    for (surface, op) in [
        ("volume snapshot", "snapshot"),
        ("volume restore", "restore"),
        ("volume snapshot deletion", "snapshot deletion"),
    ] {
        let result = match op {
            "snapshot" => {
                service
                    .snapshot_volume(snapshot_request("node-leg", "vol-mismatch-cm"))
                    .await
            }
            "restore" => {
                service
                    .restore_volume(restore_request("node-leg", "vol-mismatch-cm"))
                    .await
            }
            _ => {
                service
                    .delete_volume_snapshot(delete_snapshot_request("node-leg", "vol-mismatch-cm"))
                    .await
            }
        };
        match result {
            Err(ControlPlaneServiceError::InvalidArgument(msg)) => {
                assert_eq!(
                    msg,
                    format!("{surface} is not supported on core-managed nodes"),
                    "got: {msg}"
                );
            }
            other => panic!("expected invalid-argument for {op}, got {other:?}"),
        }
        assert_nothing_journaled(&pool, "vol-mismatch-cm").await;
    }
}

#[tokio::test]
async fn snapshot_family_fails_open_when_volume_node_is_null() {
    // The volume's owning node is unknown (NULL volumes.node_id): the
    // mode check cannot resolve a node and must fail open — it never
    // falls back to the request's node, which here names a core-managed
    // node the old request-node check would have rejected on.
    let (service, pool) = snapshot_family_test_service().await;
    seed_node_with_authority_mode(&pool, "node-cm", Some("core-managed")).await;
    seed_volume_without_node(&pool, "vol-orphan").await;

    let ack = service
        .snapshot_volume(snapshot_request("node-cm", "vol-orphan"))
        .await
        .expect("NULL volume node must fail open (accept)");
    assert_eq!(ack.result.expect("ack result").status, "OK");

    let statuses: Vec<String> =
        sqlx::query_scalar("SELECT status FROM operations WHERE operation_type = 'SnapshotVolume'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(statuses, vec!["Accepted".to_string()]);
    let intent: Option<String> = sqlx::query_scalar(
        "SELECT snapshot_op FROM volume_desired_state WHERE volume_id = 'vol-orphan'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(intent.as_deref(), Some("create"));
}

// ── #379 DP4: accept-time storage-class capability rejection ──────────
//
// The agent's volume open (and stord's own backend-class validation,
// PR 3's startup guards) is the enforcement boundary; these tests pin
// the accept-time UX layer in the #495 shape: the lifecycle rejects
// with InvalidArgument BEFORE journaling (no operations row, no
// desired-state intent), fails OPEN on never-reported classes (empty
// list — inventory not landed, or a pre-#379 agent), resolves the
// VOLUME's node (never the request's advisory node_id), and normalizes
// the DP3 local aliases so legacy `localdisk` reports keep accepting
// NULL-class volumes.

/// Seed a node plus an inventory row reporting exactly these storage
/// classes (the JSON array of strings the inventory paths write; the
/// pre-#379 directory probe emitted names like `localdisk`).
async fn seed_node_with_storage_classes(pool: &StorePool, node_id: &str, classes: &[&str]) {
    sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES (?, 'host', 'host')")
        .bind(node_id)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO node_inventory (node_id, architecture, cpu_count, memory_bytes, storage_classes) \
         VALUES (?, 'x86_64', 1, 1024, ?)",
    )
    .bind(node_id)
    .bind(serde_json::to_string(classes).unwrap())
    .execute(pool)
    .await
    .unwrap();
}

/// A `volumes` row on `node_id` carrying `storage_class` (`None` =
/// NULL = local), through the same repository write production uses.
async fn seed_volume_with_class(
    pool: &StorePool,
    volume_id: &str,
    node_id: Option<&str>,
    storage_class: Option<&str>,
) {
    DesiredStateRepository::new(pool.clone())
        .upsert_volume(&VolumeDesiredStateInput {
            volume_id: ResourceId::new(volume_id).unwrap(),
            node_id: node_id.map(|n| NodeId::new(n).unwrap()),
            display_name: volume_id.into(),
            capacity_bytes: 1024,
            volume_kind: None,
            storage_class: storage_class.map(str::to_string),
            owner_id: None,
            desired_generation: Generation::new(1),
            desired_status: None,
            requested_by: None,
            updated_by: None,
            attached_vm_id: None,
            attachment_mode: None,
            device_name: None,
            read_only: false,
            resize_to_bytes: None,
            snapshot_op: None,
            snapshot_name: None,
            clone_source_volume_id: None,
            requested_unix_ms: 1000,
        })
        .await
        .unwrap();
}

fn dp4_meta(node: &str) -> proto::RequestMeta {
    proto::RequestMeta {
        operation_id: "".into(),
        requested_by: "test-user".into(),
        target_node_id: node.into(),
        desired_state_version: "1".into(),
        request_unix_ms: 1000,
    }
}

fn dp4_create_vm_request(node: &str, vm_id: &str, vm_spec_json: &[u8]) -> proto::CreateVmRequest {
    proto::CreateVmRequest {
        meta: Some(dp4_meta(node)),
        node_id: node.into(),
        vm: Some(proto::VmMutationSpec {
            vm_id: vm_id.into(),
            vm_spec_json: vm_spec_json.to_vec(),
        }),
    }
}

fn dp4_attach_request(node: &str, volume_id: &str, vm_id: &str) -> proto::AttachVolumeRequest {
    proto::AttachVolumeRequest {
        meta: Some(dp4_meta(node)),
        node_id: node.into(),
        volume: Some(proto::VolumeMutationSpec {
            volume_id: volume_id.into(),
            vm_id: vm_id.into(),
            volume_spec_json: vec![],
        }),
    }
}

/// Assert a rejected create/attach journaled NOTHING: no operation row
/// of any type, no VM desired-state row, and no attachment intent on
/// the volume.
async fn assert_dp4_rejection_journaled_nothing(pool: &StorePool, vm_id: &str, volume_id: &str) {
    let ops: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM operations")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(ops, 0, "a rejected request must not journal an operation");
    let vms: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vms WHERE vm_id = ?")
        .bind(vm_id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(
        vms, 0,
        "a rejected create must not write a VM desired-state row"
    );
    let attached: Option<String> =
        sqlx::query_scalar("SELECT attached_vm_id FROM volume_desired_state WHERE volume_id = ?")
            .bind(volume_id)
            .fetch_optional(pool)
            .await
            .unwrap()
            .flatten();
    assert_eq!(
        attached, None,
        "a rejected attach must not write an attachment intent"
    );
}

#[tokio::test]
async fn create_vm_rejects_disk_class_the_node_does_not_offer() {
    let (service, pool) = snapshot_family_test_service().await;
    // A local-only node (PR 3's inventory now reports the REAL stord
    // backend class): an LVM-class disk in the create spec must reject
    // at accept time instead of burning dispatch retries against a
    // stord that can never open it.
    seed_node_with_storage_classes(&pool, "node-local", &["local"]).await;
    let spec = br#"{"cpu_count":1,"memory_bytes":1024,"disks":[
        {"volume_id":"vol-dp4-a","backend_class":"lvm"},
        {"volume_id":"vol-dp4-b"}]}"#;
    let result = service
        .create_vm(dp4_create_vm_request("node-local", "vm-dp4-1", spec))
        .await;
    match result {
        Err(ControlPlaneServiceError::InvalidArgument(msg)) => {
            assert!(
                msg.contains("does not offer storage class lvm"),
                "got: {msg}"
            );
        }
        other => panic!("expected invalid-argument, got {other:?}"),
    }
    assert_dp4_rejection_journaled_nothing(&pool, "vm-dp4-1", "vol-dp4-a").await;
}

#[tokio::test]
async fn create_vm_rejects_classless_disk_on_lvm_only_node() {
    // NULL = local (B1) at accept time too: an LVM-only node cannot
    // serve a classless disk, and the check says so with the canonical
    // class name rather than materializing "local" into any payload.
    let (service, pool) = snapshot_family_test_service().await;
    seed_node_with_storage_classes(&pool, "node-lvm", &["lvm"]).await;
    let spec = br#"{"cpu_count":1,"memory_bytes":1024,"disks":[{"volume_id":"vol-dp4-c"}]}"#;
    let result = service
        .create_vm(dp4_create_vm_request("node-lvm", "vm-dp4-2", spec))
        .await;
    match result {
        Err(ControlPlaneServiceError::InvalidArgument(msg)) => {
            assert!(
                msg.contains("does not offer storage class local"),
                "got: {msg}"
            );
        }
        other => panic!("expected invalid-argument, got {other:?}"),
    }
    assert_dp4_rejection_journaled_nothing(&pool, "vm-dp4-2", "vol-dp4-c").await;
}

#[tokio::test]
async fn create_vm_accepts_offered_class_and_fails_open_without_report() {
    let (service, pool) = snapshot_family_test_service().await;
    // Offered class: an LVM node accepts an LVM-class disk spec.
    seed_node_with_storage_classes(&pool, "node-lvm", &["lvm"]).await;
    let spec = br#"{"cpu_count":1,"memory_bytes":1024,"disks":[
        {"volume_id":"vol-dp4-d","backend_class":"lvm"}]}"#;
    let ack = service
        .create_vm(dp4_create_vm_request("node-lvm", "vm-dp4-3", spec))
        .await
        .expect("an offered class must accept");
    assert_eq!(ack.result.expect("ack result").status, "OK");

    // Fail-open edges in the #495 discipline: a node with NO inventory
    // row (never reported — pre-#379 agents) keeps accepting any class,
    // and the legacy `localdisk` probe report normalizes to local so a
    // classless disk still matches.
    seed_node_without_inventory(&pool, "node-noreport").await;
    seed_node_with_storage_classes(&pool, "node-legacy-probe", &["localdisk"]).await;
    for (node, vm_id, spec) in [
        (
            "node-noreport",
            "vm-dp4-4",
            br#"{"cpu_count":1,"memory_bytes":1024,"disks":[{"volume_id":"vol-dp4-e","backend_class":"lvm"}]}"#
                as &[u8],
        ),
        (
            "node-legacy-probe",
            "vm-dp4-5",
            br#"{"cpu_count":1,"memory_bytes":1024,"disks":[{"volume_id":"vol-dp4-f"}]}"#,
        ),
    ] {
        let ack = service
            .create_vm(dp4_create_vm_request(node, vm_id, spec))
            .await
            .unwrap_or_else(|e| panic!("{node} must fail open: {e:?}"));
        assert_eq!(ack.result.expect("ack result").status, "OK");
    }
}

#[tokio::test]
async fn attach_volume_rejects_class_the_volume_node_does_not_offer() {
    let (service, pool) = snapshot_family_test_service().await;
    // The node checked is the VOLUME's node (the dispatch node, #495
    // lesson), and the class checked is the volumes row's class — the
    // A8 dispatch producer's source — not the request's spec_json.
    seed_node_with_storage_classes(&pool, "node-lvm", &["lvm"]).await;
    seed_volume_with_class(&pool, "vol-dp4-ceph", Some("node-lvm"), Some("ceph")).await;

    // The request names a DIFFERENT node offering ceph: the rejection
    // must still fire against the volume's node.
    seed_node_with_storage_classes(&pool, "node-ceph", &["ceph"]).await;
    let result = service
        .attach_volume(dp4_attach_request("node-ceph", "vol-dp4-ceph", "vm-dp4-6"))
        .await;
    match result {
        Err(ControlPlaneServiceError::InvalidArgument(msg)) => {
            assert!(
                msg.contains("node-lvm does not offer storage class ceph"),
                "got: {msg}"
            );
        }
        other => panic!("expected invalid-argument, got {other:?}"),
    }
    assert_dp4_rejection_journaled_nothing(&pool, "vm-dp4-6", "vol-dp4-ceph").await;
}

#[tokio::test]
async fn attach_volume_accepts_match_and_fails_open_on_unknowns() {
    let (service, pool) = snapshot_family_test_service().await;
    sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES ('vm-dp4-7', 'vm-dp4-7')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES ('vm-dp4-8', 'vm-dp4-8')")
        .execute(&pool)
        .await
        .unwrap();
    // Definite match: an LVM node + an LVM-class volume.
    seed_node_with_storage_classes(&pool, "node-lvm", &["lvm"]).await;
    seed_volume_with_class(&pool, "vol-dp4-lvm", Some("node-lvm"), Some("lvm")).await;
    let ack = service
        .attach_volume(dp4_attach_request("node-lvm", "vol-dp4-lvm", "vm-dp4-7"))
        .await
        .expect("an offered class must accept");
    assert_eq!(ack.result.expect("ack result").status, "OK");

    // Fail-open edges: a node that never reported classes (no inventory
    // row) accepts a class the fleet may or may not offer, and a volume
    // with a NULL node (ON DELETE SET NULL) never falls back to the
    // request's node — there is nothing definitive to check.
    seed_node_without_inventory(&pool, "node-noreport").await;
    seed_volume_with_class(&pool, "vol-dp4-orphan", None, Some("ceph")).await;
    seed_node_with_storage_classes(&pool, "node-local", &["local"]).await;
    let ack = service
        .attach_volume(dp4_attach_request(
            "node-noreport",
            "vol-dp4-orphan",
            "vm-dp4-8",
        ))
        .await
        .expect("unreported classes must fail open (accept)");
    assert_eq!(ack.result.expect("ack result").status, "OK");
}

#[tokio::test]
async fn attach_volume_normalizes_legacy_localdisk_report_for_null_class() {
    // DP3 normalization at the accept-time check: a pre-#379 agent's
    // `localdisk` probe report compares as `local`, so a NULL-class
    // volume (NULL = local) keeps attaching exactly as before.
    let (service, pool) = snapshot_family_test_service().await;
    sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES ('vm-dp4-9', 'vm-dp4-9')")
        .execute(&pool)
        .await
        .unwrap();
    seed_node_with_storage_classes(&pool, "node-legacy-probe", &["localdisk"]).await;
    seed_volume_with_class(&pool, "vol-dp4-null", Some("node-legacy-probe"), None).await;
    let ack = service
        .attach_volume(dp4_attach_request(
            "node-legacy-probe",
            "vol-dp4-null",
            "vm-dp4-9",
        ))
        .await
        .expect("legacy localdisk report + NULL class must accept");
    assert_eq!(ack.result.expect("ack result").status, "OK");
}

// ── #378: inventory ingestion persists the authority mode ──────────────

#[tokio::test]
async fn report_node_inventory_persists_authority_mode() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    let inventory_service = crate::inventory::InventoryServiceImplementation::new(
        NodeRepository::new(pool.clone()),
        VtepRepository::new(pool.clone()),
    );

    let report = |node: &str, mode: i32| proto::ReportNodeInventoryRequest {
        meta: Some(proto::RequestMeta {
            operation_id: format!("op-{node}"),
            requested_by: "test".into(),
            target_node_id: node.into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1000,
        }),
        inventory: Some(proto::NodeInventory {
            node_id: node.into(),
            hostname: "host".into(),
            architecture: "x86_64".into(),
            cpu_threads: 1,
            memory_bytes: 1024,
            storage_classes: vec![],
            network_capabilities: vec![],
            hypervisor_capabilities: vec![],
            labels: std::collections::HashMap::new(),
            vtep_ip: String::new(),
            wireguard_public_key: String::new(),
            underlay_mtu: 0,
            authority_mode: mode,
        }),
    };

    let node_repo = NodeRepository::new(pool.clone());

    // A definite core-managed report persists to the column.
    crate::inventory::InventoryService::report_node_inventory(
        &inventory_service,
        report("node-inv-cm", proto::AuthorityMode::CoreManaged as i32),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        node_repo
            .get_authority_mode(&NodeId::new("node-inv-cm").unwrap())
            .await
            .unwrap()
            .as_deref(),
        Some("core-managed")
    );

    // UNSPECIFIED (pre-#378 agent) stores NULL — the fail-open edge.
    crate::inventory::InventoryService::report_node_inventory(
        &inventory_service,
        report("node-inv-unset", proto::AuthorityMode::Unspecified as i32),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        node_repo
            .get_authority_mode(&NodeId::new("node-inv-unset").unwrap())
            .await
            .unwrap(),
        None,
        "unspecified mode must persist NULL (fail-open)"
    );

    // An unknown enum int (e.g. a future mode an old CP does not know)
    // ingests to NULL too — not an error, not a persisted garbage value.
    crate::inventory::InventoryService::report_node_inventory(
        &inventory_service,
        report("node-inv-unknown", 99),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        node_repo
            .get_authority_mode(&NodeId::new("node-inv-unknown").unwrap())
            .await
            .unwrap(),
        None,
        "an unknown authority-mode enum int must persist NULL (fail-open)"
    );

    // An unspecified re-report must not wipe an established mode (the
    // upsert COALESCEs like the version columns).
    crate::inventory::InventoryService::report_node_inventory(
        &inventory_service,
        report("node-inv-cm", proto::AuthorityMode::Unspecified as i32),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        node_repo
            .get_authority_mode(&NodeId::new("node-inv-cm").unwrap())
            .await
            .unwrap()
            .as_deref(),
        Some("core-managed"),
        "an unspecified re-report must not wipe an established mode"
    );

    // A mode flip (core-managed → legacy at agent restart) overwrites.
    crate::inventory::InventoryService::report_node_inventory(
        &inventory_service,
        report("node-inv-cm", proto::AuthorityMode::Legacy as i32),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        node_repo
            .get_authority_mode(&NodeId::new("node-inv-cm").unwrap())
            .await
            .unwrap()
            .as_deref(),
        Some("legacy")
    );
}

// ── #378: the BFF surface over HTTP (router → mutation service →
//    lifecycle), same harness as the admin-router tests above ───────────

#[tokio::test]
async fn volume_snapshot_rejected_over_http_on_core_managed_node() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();
    seed_node_with_authority_mode(&pool, "node-cm", Some("core-managed")).await;
    seed_node_with_authority_mode(&pool, "node-leg", Some("legacy")).await;
    seed_volume_on_node(&pool, "vol-http-cm", "node-cm").await;
    seed_volume_on_node(&pool, "vol-http-leg", "node-leg").await;

    let app = crate::api::router::admin_router(
        test_app_state(pool.clone()),
        crate::convergence_metrics::new_shared(),
        chv_config::WebUiConfig::default(),
    );
    let token = test_admin_token();

    let post = |path: &str, body: &str| {
        Request::builder()
            .method("POST")
            .uri(path)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    // Core-managed node: HTTP 400 with the CP's message — the immediate,
    // explicit rejection that replaces 200-accepted-then-~70 s-of-retries.
    let response = app
        .clone()
        .oneshot(post(
            "/v1/volumes/snapshot",
            r#"{"volume_id":"vol-http-cm","snapshot_name":"snap-1"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("volume snapshot is not supported on core-managed nodes"),
        "rejection must carry the CP's message: {body}"
    );

    // Nothing was journaled for the rejected request.
    let ops: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM operations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ops, 0, "the rejected HTTP snapshot must journal nothing");

    // Legacy node: unchanged — 200 accepted.
    let response = app
        .clone()
        .oneshot(post(
            "/v1/volumes/snapshot",
            r#"{"volume_id":"vol-http-leg","snapshot_name":"snap-1"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["accepted"].as_bool(), Some(true));

    // Clone against a core-managed source over HTTP: same 400 contract.
    let response = app
        .oneshot(post(
            "/v1/volumes/clone",
            r#"{"source_volume_id":"vol-http-cm","target_volume_id":"vol-http-dst"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("volume clone is not supported on core-managed nodes"),
        "clone rejection must carry the CP's message: {body}"
    );
}

// ─────────────────────────────────────────────────────────────────────
// #499 — the late-fragment resurrection pin (end to end)
// ─────────────────────────────────────────────────────────────────────

/// The #499 scenario, through the real fragment entry point: an
/// operator deletes a network (the BFF route's tombstone — the NDS row
/// kept with a terminal 'Deleting' status and a generation bump), and a
/// `NetworkFragments` report that was queued on the agent before the
/// delete arrives afterwards. The fragment carries a wall-clock
/// generation (the shape that beats the tombstone's small integer), so
/// only the `IS NOT 'Deleting'` conflict-arm term stands between it and
/// a resurrected network.
///
/// Asserted: the ingest fails loudly (the warn + DesiredStateRejected
/// event path), the NDS tombstone survives byte-for-byte, the physical
/// `networks` row is not re-asserted, and no exposure row lands. The
/// network stays deleted.
#[tokio::test]
async fn late_network_fragment_cannot_resurrect_a_deleted_network() {
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let pool = test_db.pool.clone();

    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('node-499', 'host-499', 'host-499')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // The pre-delete shape: a live network the agent has applied and
    // reported (NDS generation 3 — BFF create writes 1, two updates
    // bumped it).
    sqlx::query(
        "INSERT INTO networks (network_id, node_id, display_name, network_class) \
         VALUES ('net-499', 'node-499', 'tenant-net', 'bridge')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO network_desired_state (network_id, desired_generation, desired_status) \
         VALUES ('net-499', 3, 'Active')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // The operator delete — the exact tombstone statement the BFF
    // route's transaction writes (#499).
    sqlx::query(
        r#"
        INSERT INTO network_desired_state (
            network_id, desired_generation, desired_status, updated_by,
            requested_at, updated_at
        )
        VALUES ('net-499', 1, 'Deleting', 'op@test', strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'))
        ON CONFLICT (network_id) DO UPDATE SET
            desired_status = 'Deleting',
            desired_generation = network_desired_state.desired_generation + 1,
            updated_by = EXCLUDED.updated_by,
            updated_at = EXCLUDED.updated_at
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        sqlx::query_as::<_, (i64, Option<String>)>(
            "SELECT desired_generation, desired_status FROM network_desired_state \
             WHERE network_id = 'net-499'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        (4, Some("Deleting".into())),
        "the tombstone must have landed at generation 4 (3 + 1)"
    );

    // The fragment rides a dispatched operation (the agent echoes the
    // operation id of the intent it applied) — seeded so the rejection
    // event's FK (`events.operation_id REFERENCES operations`) holds.
    sqlx::query(
        "INSERT INTO operations (operation_id, idempotency_key, resource_kind, resource_id, \
         operation_type, status) \
         VALUES ('op-499-late', 'op-499-late', 'network', 'net-499', 'ApplyNetworkDesiredState', 'Succeeded')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let node_repo = NodeRepository::new(pool.clone());
    let desired_state_repo = DesiredStateRepository::new(pool.clone());
    let event_repo = EventRepository::new(pool.clone());

    let service = ReconcileServiceImplementation::new(
        node_repo,
        desired_state_repo,
        event_repo,
        ObservedStateRepository::new(pool.clone()),
        OperationRepository::new(pool.clone()),
    );

    // The late fragment: the agent's deferred-report shape — the spec it
    // applied BEFORE the delete, echoed with a wall-clock-milliseconds
    // generation (what the CP-side mutation verbs mint; the worst case
    // for the tombstone's small-integer generation).
    let spec_json = r#"{"network_class": "bridge", "exposures": [{"service_name": "web", "protocol": "tcp", "listen_port": 80, "target_port": 8080}]}"#;
    let request = proto::ApplyNetworkDesiredStateRequest {
        meta: Some(proto::RequestMeta {
            operation_id: "op-499-late".into(),
            requested_by: "agent".into(),
            target_node_id: "node-499".into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1_769_000_000_000,
        }),
        node_id: "node-499".into(),
        network_id: "net-499".into(),
        fragment: Some(proto::DesiredStateFragment {
            id: "net-499".into(),
            kind: "Network".into(),
            generation: "1769000000000".into(),
            spec_json: spec_json.as_bytes().to_vec(),
            policy_json: vec![],
            updated_at: "2026-10-07T00:00:00Z".into(),
            updated_by: "agent".into(),
        }),
    };

    let result = service.apply_network_desired_state(request).await;
    assert!(
        result.is_err(),
        "a fragment for a tombstoned network must be dropped, got: {:?}",
        result
    );

    // No NDS resurrection, no physical re-assert, no exposures.
    assert_eq!(
        sqlx::query_as::<_, (i64, Option<String>)>(
            "SELECT desired_generation, desired_status FROM network_desired_state \
             WHERE network_id = 'net-499'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        (4, Some("Deleting".into())),
        "the tombstone must survive the late fragment byte-for-byte"
    );
    assert_eq!(
        sqlx::query_as::<_, (Option<String>, String)>(
            "SELECT node_id, display_name FROM networks WHERE network_id = 'net-499'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        (Some("node-499".into()), "tenant-net".into()),
        "the physical networks row must not be re-asserted by the refused fragment"
    );
    let exposures: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM network_exposures WHERE network_id = 'net-499'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(exposures, 0, "no exposure row may land for a tombstone");

    // The loud refusal: the DesiredStateRejected event the fragment
    // path emits before returning the error (beside the warn log).
    let rejected: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM events WHERE resource_id = 'net-499' AND event_type = 'DesiredStateRejected'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        rejected > 0,
        "the refused fragment must leave a desired_state_rejected event"
    );
}

// ---------------------------------------------------------------------------
// Static Web UI serving (issue #447 — decision D3 target)
// ---------------------------------------------------------------------------

/// A minimal built-UI-tree fixture: the SPA shell, one content-hashed
/// immutable asset, and one plain asset — enough to pin the ServeDir
/// route, the SPA fallback, and both Cache-Control postures without a
/// real `ui/build` (which is gitignored, hence ServeDir-from-disk
/// rather than rust-embed in the first place).
fn fixture_ui_tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("ui tempdir");
    std::fs::write(
        dir.path().join("index.html"),
        "<!doctype html><html><body>chv-ui-fixture-shell</body></html>",
    )
    .expect("write index.html");
    let immutable = dir.path().join("_app").join("immutable");
    std::fs::create_dir_all(&immutable).expect("mkdir _app/immutable");
    std::fs::write(
        immutable.join("app.HASH1234.js"),
        "// chv-ui-fixture-immutable-asset",
    )
    .expect("write immutable asset");
    std::fs::write(
        dir.path().join("favicon.svg"),
        "<svg><!-- chv-ui-fixture-favicon --></svg>",
    )
    .expect("write favicon");
    dir
}

fn webui_enabled_at(dir: &std::path::Path) -> chv_config::WebUiConfig {
    chv_config::WebUiConfig {
        enabled: true,
        dir: dir.to_path_buf(),
    }
}

#[tokio::test]
async fn webui_root_serves_index_html_with_no_cache() {
    use axum::http::StatusCode;
    use tower::ServiceExt;

    let ui = fixture_ui_tree();
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let app = crate::api::router::admin_router(
        test_app_state(test_db.pool.clone()),
        crate::convergence_metrics::new_shared(),
        webui_enabled_at(ui.path()),
    );

    let response = app
        .oneshot(
            axum::http::Request::get("/")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // ServeDir guesses the content type from the extension.
    assert!(response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/html")));
    // nginx parity (`location = /index.html`): the shell is never
    // cached, or an upgrade serves a stale shell against new hashed
    // assets.
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-cache, no-store, must-revalidate")
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = std::str::from_utf8(&body).unwrap();
    assert!(
        text.contains("chv-ui-fixture-shell"),
        "the served bytes must be the fixture index.html"
    );
}

#[tokio::test]
async fn webui_spa_fallback_serves_index_html() {
    use axum::http::StatusCode;
    use tower::ServiceExt;

    let ui = fixture_ui_tree();
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let app = crate::api::router::admin_router(
        test_app_state(test_db.pool.clone()),
        crate::convergence_metrics::new_shared(),
        webui_enabled_at(ui.path()),
    );

    // A client-side route that has no file behind it — the
    // `try_files … /index.html` leg.
    let response = app
        .oneshot(
            axum::http::Request::get("/vms/some-vm/console-history")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-cache, no-store, must-revalidate"),
        "the SPA fallback serves index.html, so it gets index.html's no-cache header"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = std::str::from_utf8(&body).unwrap();
    assert!(text.contains("chv-ui-fixture-shell"));
}

#[tokio::test]
async fn webui_immutable_asset_served_with_one_year_cache() {
    use axum::http::StatusCode;
    use tower::ServiceExt;

    let ui = fixture_ui_tree();
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let app = crate::api::router::admin_router(
        test_app_state(test_db.pool.clone()),
        crate::convergence_metrics::new_shared(),
        webui_enabled_at(ui.path()),
    );

    let response = app
        .oneshot(
            axum::http::Request::get("/_app/immutable/app.HASH1234.js")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // nginx parity (`location /_app/immutable/`): content-hashed
    // filenames make the response permanently cacheable.
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("public, max-age=31536000, immutable")
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        std::str::from_utf8(&body).unwrap(),
        "// chv-ui-fixture-immutable-asset",
        "the served bytes must be the fixture asset's real bytes"
    );
}

#[tokio::test]
async fn webui_plain_asset_carries_no_cache_control() {
    use axum::http::StatusCode;
    use tower::ServiceExt;

    let ui = fixture_ui_tree();
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let app = crate::api::router::admin_router(
        test_app_state(test_db.pool.clone()),
        crate::convergence_metrics::new_shared(),
        webui_enabled_at(ui.path()),
    );

    // nginx parity: only the shell and the immutable tree carry
    // Cache-Control; every other asset gets none.
    let response = app
        .oneshot(
            axum::http::Request::get("/favicon.svg")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get("cache-control"), None);
}

#[tokio::test]
async fn webui_reserved_prefixes_keep_the_json_404() {
    use axum::http::StatusCode;
    use tower::ServiceExt;

    let ui = fixture_ui_tree();
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let app = crate::api::router::admin_router(
        test_app_state(test_db.pool.clone()),
        crate::convergence_metrics::new_shared(),
        webui_enabled_at(ui.path()),
    );

    // Issue #447's reserved list, verbatim: /v1, /api, /admin, /health*,
    // /ready, /internal, /metrics. An unmatched path under one of these
    // is an API miss and must stay machine-readable JSON — never the
    // SPA's index.html.
    for path in [
        "/v1/does-not-exist",
        "/api/v1/does-not-exist",
        "/admin/does-not-exist",
        "/internal/does-not-exist",
        "/metrics/does-not-exist",
        "/healthnothing",
        "/ready/does-not-exist",
    ] {
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::get(path)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "GET {path}");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["error"]["code"], "NOT_IMPLEMENTED",
            "GET {path} must keep the JSON 404 shape"
        );
    }
}

#[tokio::test]
async fn webui_disabled_keeps_the_json_404_everywhere() {
    use axum::http::StatusCode;
    use tower::ServiceExt;

    // The fail-closed default ([webui] absent / enabled = false): the
    // router is byte-identical to the pre-#447 fallback.
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let app = crate::api::router::admin_router(
        test_app_state(test_db.pool.clone()),
        crate::convergence_metrics::new_shared(),
        chv_config::WebUiConfig::default(),
    );

    for path in ["/", "/vms/some-vm"] {
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::get(path)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "GET {path}");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"], "NOT_IMPLEMENTED");
    }
}

#[tokio::test]
async fn webui_assets_get_the_security_headers() {
    use tower::ServiceExt;

    let ui = fixture_ui_tree();
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let app = crate::api::router::admin_router(
        test_app_state(test_db.pool.clone()),
        crate::convergence_metrics::new_shared(),
        webui_enabled_at(ui.path()),
    );

    // The router-level security_headers layer wraps the fallback too —
    // a served asset must carry the same CSP/nosniff/DENY/referrer set
    // as every API response (issue #447's verification requirement).
    let response = app
        .oneshot(
            axum::http::Request::get("/")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let headers = response.headers();
    assert!(headers.get("content-security-policy").is_some());
    assert_eq!(
        headers
            .get("x-content-type-options")
            .and_then(|v| v.to_str().ok()),
        Some("nosniff")
    );
    assert_eq!(
        headers.get("x-frame-options").and_then(|v| v.to_str().ok()),
        Some("DENY")
    );
    assert!(headers.get("referrer-policy").is_some());
}

#[tokio::test]
async fn webui_enabled_does_not_change_v1_auth_or_csrf() {
    use axum::http::StatusCode;
    use tower::ServiceExt;

    let ui = fixture_ui_tree();
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let app = crate::api::router::admin_router(
        test_app_state(test_db.pool.clone()),
        crate::convergence_metrics::new_shared(),
        webui_enabled_at(ui.path()),
    );

    // Forbidden outcome unchanged (issue #447's verification
    // requirement): a matched /v1 route still demands authentication —
    // the static fallback never shields API routes.
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::post("/v1/vms")
                .header("content-type", "application/json")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], "UNAUTHORIZED");

    // Authentication now runs before the backup fence (the fence replaced
    // handlers that each required `BearerToken`): an anonymous request gets
    // 401, never the 501 posture message.
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::get("/api/v1/backup-history")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // And the CSRF middleware's content-type gate is untouched for an
    // authenticated caller: a form-native POST to a matched legacy route
    // still dies in the middleware with 415 before the fence/handler runs.
    let response = app
        .oneshot(
            axum::http::Request::post("/api/v1/backup-jobs")
                .header("authorization", format!("Bearer {}", test_admin_token()))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], "CSRF_REJECTED");

    // With a valid credential the fence still answers deterministically: the
    // route exists (501), it did not move (404).
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let app = crate::api::router::admin_router(
        test_app_state(test_db.pool.clone()),
        crate::convergence_metrics::new_shared(),
        chv_config::WebUiConfig::default(),
    );
    let response = app
        .oneshot(
            axum::http::Request::get("/api/v1/backup-history")
                .header("authorization", format!("Bearer {}", test_admin_token()))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
}

#[tokio::test]
async fn webui_matched_routes_still_win_over_the_fallback() {
    use axum::http::StatusCode;
    use tower::ServiceExt;

    let ui = fixture_ui_tree();
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let app = crate::api::router::admin_router(
        test_app_state(test_db.pool.clone()),
        crate::convergence_metrics::new_shared(),
        webui_enabled_at(ui.path()),
    );

    // The fallback only fires when NO route matches: the real /health
    // route keeps answering JSON, not the UI shell.
    let response = app
        .oneshot(
            axum::http::Request::get("/health")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json.get("status").is_some());
}

#[tokio::test]
async fn webui_missing_dir_is_a_404_not_a_crash() {
    use axum::http::StatusCode;
    use tower::ServiceExt;

    // enabled = true with a directory that does not exist: every UI
    // route 404s (ServeDir/ServeFile find nothing) — fail-closed, no
    // panic, no 500. The startup warn names the directory.
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let app = crate::api::router::admin_router(
        test_app_state(test_db.pool.clone()),
        crate::convergence_metrics::new_shared(),
        webui_enabled_at(std::path::Path::new("/nonexistent/chv/ui-fixture")),
    );

    let response = app
        .oneshot(
            axum::http::Request::get("/")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn webui_non_get_static_request_is_method_not_allowed() {
    use axum::http::StatusCode;
    use tower::ServiceExt;

    let ui = fixture_ui_tree();
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let app = crate::api::router::admin_router(
        test_app_state(test_db.pool.clone()),
        crate::convergence_metrics::new_shared(),
        webui_enabled_at(ui.path()),
    );

    // ServeDir answers non-GET/HEAD with 405 (nginx parity: static
    // files reject POST). Disclosed, not specced by #447 — the issue
    // only specifies GET serving.
    let response = app
        .oneshot(
            axum::http::Request::post("/")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn webui_metrics_stays_prometheus_and_renamed_ui_route_serves_the_spa() {
    use axum::http::StatusCode;
    use tower::ServiceExt;

    // #447 review ruling 2 (2026-10-07): the UI's former top-level
    // /metrics page (an Overview alias, ui/src/routes/metrics) was
    // shadowed by this matched admin prometheus route on every hard
    // load — F5/deep-link returned 401 or prometheus text and the SPA's
    // client-side redirect never ran. The SvelteKit route is renamed to
    // /observability; this pin is the fix's proof: /metrics keeps the
    // admin-gated prometheus route (untouched, per the ruling), and the
    // renamed path is a non-reserved SPA route served the shell.
    let ui = fixture_ui_tree();
    let test_db = chv_controlplane_store::test_util::TestDb::new().await;
    let app = crate::api::router::admin_router(
        test_app_state(test_db.pool.clone()),
        crate::convergence_metrics::new_shared(),
        webui_enabled_at(ui.path()),
    );

    // /metrics without a token: still the admin gate's 401 — the static
    // fallback never shields the matched route.
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::get("/metrics")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // /metrics with a valid admin token: the prometheus handler itself
    // (200 with the exposition, or 503 "metrics recorder not
    // initialized" in this test process — either proves the route
    // matched and auth passed; what it must never be is the SPA shell).
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::get("/metrics")
                .header("authorization", format!("Bearer {}", test_admin_token()))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        response.status() == StatusCode::OK || response.status() == StatusCode::SERVICE_UNAVAILABLE,
        "an authenticated /metrics request must reach the prometheus handler"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(
        !std::str::from_utf8(&body)
            .unwrap()
            .contains("chv-ui-fixture-shell"),
        "/metrics must never serve the SPA shell"
    );

    // The renamed UI route: a non-reserved path, so the SPA fallback
    // serves the shell and the client-side redirect to / can run.
    let response = app
        .oneshot(
            axum::http::Request::get("/observability")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(std::str::from_utf8(&body)
        .unwrap()
        .contains("chv-ui-fixture-shell"));
}
