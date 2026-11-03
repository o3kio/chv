//! Integration tests for the NetBox projection BFF surface (#239, PR 5).
//!
//! Covers the eight POST-only endpoints of
//! `docs/specs/architecture-designer/contracts/netbox-api-contract.md`
//! against the real router (in-memory sqlite, JWT per role, `oneshot`),
//! so the assertions pin the **wire surface**: status codes, the flat
//! `{code, message}` error shape with the contract's stable codes, and
//! the secret-free config responses.
//!
//! Suites mirrored (per the plan's PR-5 test list):
//!
//! - **permission matrix** — viewer 403 on all eight (also enforced
//!   mechanically by `architecture_permission_matrix.rs`), operator ok
//!   on non-production, admin ok everywhere, production export by
//!   operator → 403 `PRODUCTION_REQUIRES_ADMIN`;
//! - **ownership/IDOR** — foreign-owned topology → 403 on every
//!   endpoint (the `architectures_ownership_idor.rs` convention);
//! - **config token redaction** — `token_set: true`, no token or
//!   ciphertext substring anywhere in any response body;
//! - **stable error codes** — not-configured, not-applied,
//!   https-required, run-active, not-retryable, plan-expired;
//! - **happy paths** — config upsert → get roundtrip, runs list/get
//!   after an enqueued run, retry of a failed below-cap run;
//! - **dry-run gates** — the not-configured / not-applied preconditions,
//!   the 502 `NETBOX_UNREACHABLE` transport leg against a dead HTTPS
//!   port, and the fail-closed 400 `NETBOX_TOKEN_MISSING` when the
//!   stored ciphertext no longer decrypts;
//! - **dry-run over a mock NetBox** — via this crate's dev-only
//!   `test-http` client seam (see `build_netbox_client` in
//!   handlers/netbox.rs), a wiremock NetBox driven through the real
//!   route: a 401 → 502 `NETBOX_AUTH_FAILED`, and an all-empty remote
//!   → 200 with the contract's plan shape asserted field-by-field;
//! - **run payload degradation** — an unparseable `plan_json` column
//!   comes back as the raw string, not null and not an error;
//! - **result-envelope unwrap** — the worker's
//!   `{ resolved_architecture_version_id, result }` provenance envelope
//!   is unwrapped on runs/get (the inner outcome is served, the version
//!   id surfaces as `resolved_architecture_version_id`), while
//!   non-envelope objects and raw strings pass through unchanged.
//!
//! ## Plain-HTTP test seam
//!
//! wiremock serves plain HTTP, while the production dry-run path is
//! HTTPS-only end to end (accept-time gate + fail-closed client
//! constructor). The crate's `test-http` feature — enabled for this
//! suite by the self dev-dependency in Cargo.toml, mirroring how
//! `chv-controlplane-service` enables the adapter's `test-http` for
//! its wiremock suites — switches the handler's client seam to the
//! adapter's test-only plain-HTTP constructor. Configs pointing at the
//! mock are seeded through the repository (bypassing the BFF's
//! accept-time HTTPS gate, which has its own test above).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use chv_architecture_validate::model::{
    CHVArchitecture, Instance, InstanceNetwork, InstancePlacement, InstanceResources, Metadata,
    Network, NetworkType, Server, ServerResources,
};
use chv_common::SystemClock;
use chv_controlplane_store::{
    AlertRepository, AlertRuleRepository, ApplyRunCreateInput, ApplyRunRepository,
    BackupRepository, DesiredStateRepository, DriftReportRepository, EventRepository,
    ImageRepository, NetboxProjectionConfigRepository, NetboxProjectionConfigUpsertInput,
    NetboxProjectionRunRepository, NetworkRepository, NodeRepository, NotificationOutboxRepository,
    ObservedStateRepository, OperationRepository, TopologyCreateInput, TopologyRepository,
    VersionCreateInput, VersionRepository,
};
use chv_controlplane_types::architecture::{
    ArchitectureApplyRunId, ArchitectureId, ArchitectureStatus, ArchitectureVersionId,
    NetboxRetentionPolicy, RunStatus,
};
use chv_webui_bff::auth::Claims;
use chv_webui_bff::mutations::MutationService;
use chv_webui_bff::{AppState, BffError};
use serde_json::Value;
use sqlx::sqlite::SqlitePoolOptions;
use tower::ServiceExt;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Scaffolding (mirrors tests/architecture_permission_matrix.rs)
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
        unreachable!("mutate_vm not used in netbox route tests")
    }
    async fn migrate_vm(
        &self,
        _vm_id: String,
        _target_node_id: String,
        _pause_first: bool,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        unreachable!("migrate_vm not used in netbox route tests")
    }
    async fn snapshot_vm(
        &self,
        _vm_id: String,
        _destination: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        unreachable!("snapshot_vm not used in netbox route tests")
    }
    async fn restore_snapshot(
        &self,
        _vm_id: String,
        _source: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        unreachable!("restore_snapshot not used in netbox route tests")
    }
    async fn mutate_node(
        &self,
        _node_id: String,
        _action: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateNodeResponse, BffError> {
        unreachable!("mutate_node not used in netbox route tests")
    }
    async fn mutate_volume(
        &self,
        _vm_id: String,
        _action: String,
        _force: bool,
        _resize_bytes: Option<u64>,
        _vm_id2: Option<String>,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!("mutate_volume not used in netbox route tests")
    }
    async fn snapshot_volume(
        &self,
        _vm_id: String,
        _snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!("snapshot_volume not used in netbox route tests")
    }
    async fn restore_volume_snapshot(
        &self,
        _vm_id: String,
        _snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!("restore_volume_snapshot not used in netbox route tests")
    }
    async fn delete_volume_snapshot(
        &self,
        _vm_id: String,
        _snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!("delete_volume_snapshot not used in netbox route tests")
    }
    async fn clone_volume(
        &self,
        _vm_id: String,
        _new_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!("clone_volume not used in netbox route tests")
    }
    async fn mutate_network(
        &self,
        _network_id: String,
        _action: String,
        _force: bool,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateNetworkResponse, BffError> {
        unreachable!("mutate_network not used in netbox route tests")
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
        netbox_config: Arc::new(NetboxProjectionConfigRepository::with_crypto(
            pool.clone(),
            chv_controlplane_store::test_util::test_credential_encryption(),
        )),
        netbox_runs: Arc::new(NetboxProjectionRunRepository::new(pool.clone())),
        mutations: Arc::new(NoopMutations),
        jwt_secret: "test-secret".to_string(),
        console_jwt_secret: "test-secret".to_string(),
        monitoring: None,
        monitoring_health: chv_webui_bff::MonitoringHealth::new(),
        agent_runtime_dir: std::path::PathBuf::from("/var/lib/chv/agent"),
        cache: chv_webui_bff::BffCache::new(5),
        clock: Arc::new(SystemClock),
    }
}

fn token_for(state: &AppState, sub: &str, role: &str) -> String {
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let claims = Claims {
        sub: sub.to_string(),
        username: format!("user-{sub}"),
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

/// POST JSON through the full router; returns (status, parsed body).
async fn post_json(state: &AppState, path: &str, token: &str, body: &str) -> (StatusCode, Value) {
    let app = chv_webui_bff::bff_router(state.clone()).with_state(state.clone());
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.expect("oneshot request");
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read response body");
    let body: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("response body is JSON")
    };
    (status, body)
}

/// POST JSON through the full router; returns only the raw body string
/// (for substring-leak assertions).
async fn post_raw(state: &AppState, path: &str, token: &str, body: &str) -> (StatusCode, String) {
    let app = chv_webui_bff::bff_router(state.clone()).with_state(state.clone());
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.expect("oneshot request");
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read response body");
    (status, String::from_utf8_lossy(&bytes).to_string())
}

// ---------------------------------------------------------------------------
// Seeding helpers
// ---------------------------------------------------------------------------

/// Insert a topology owned by `owner` with an optional environment tag.
async fn seed_topology(
    state: &AppState,
    name: &str,
    owner: &str,
    environment: Option<&str>,
) -> String {
    let id = ArchitectureId::new(format!("arch-{name}")).expect("valid id");
    state
        .topology_repo
        .create(TopologyCreateInput {
            id: id.clone(),
            name: name.to_string(),
            display_name: None,
            description: None,
            environment: environment.map(str::to_string),
            status: ArchitectureStatus::Draft,
            owner_user_id: Some(owner.to_string()),
            design_graph_json: Some(r#"{"nodes":[],"edges":[]}"#.to_string()),
            latest_yaml: None,
        })
        .await
        .unwrap_or_else(|e| panic!("seed {name}: {e}"));
    id.into_inner()
}

/// The minimal CHVArchitecture model persisted as the applied version's
/// `normalized_model_json` (every collection defaults to empty).
const MINIMAL_MODEL_JSON: &str = r#"{"apiVersion":"chv.kubedo.io/v1alpha1","kind":"CHVArchitecture","metadata":{"name":"seed"}}"#;

/// Seed an applied version: a version row carrying the minimal model
/// plus a `succeeded` apply run referencing it — the projection source
/// the dry-run/export gates resolve. Returns the version id.
async fn seed_applied_version(state: &AppState, arch_id: &str) -> String {
    seed_applied_version_with_model(state, arch_id, MINIMAL_MODEL_JSON).await
}

/// Like [`seed_applied_version`], but with an explicit
/// `normalized_model_json` (used by the wiremock dry-run suites, whose
/// plan assertions need a non-empty architecture).
async fn seed_applied_version_with_model(
    state: &AppState,
    arch_id: &str,
    model_json: &str,
) -> String {
    let architecture_id = ArchitectureId::new(arch_id).expect("valid architecture id");
    let version_id = ArchitectureVersionId::new(format!("aver-{arch_id}")).expect("valid id");
    VersionRepository::new(state.pool.clone())
        .create(VersionCreateInput {
            id: version_id.clone(),
            architecture_id: architecture_id.clone(),
            version_number: 1,
            yaml_content: String::new(),
            design_graph_json: None,
            normalized_model_json: Some(model_json.to_string()),
            change_summary: Some("seeded by netbox route tests".to_string()),
            created_by: None,
        })
        .await
        .expect("seed version row");
    state
        .apply_runs
        .create(ApplyRunCreateInput {
            id: ArchitectureApplyRunId::new(format!("aprun-{arch_id}")).expect("valid id"),
            architecture_id,
            architecture_version_id: version_id.clone(),
            plan_id: None,
            task_id: None,
            status: RunStatus::Succeeded,
            requested_by: None,
            started_at: None,
        })
        .await
        .expect("seed succeeded apply run");
    version_id.into_inner()
}

/// The projected architecture for the wiremock dry-run suites — one
/// server, one VLAN network with a CIDR, one instance with a fixed IP
/// on that network: exactly one object of each of the six mapped
/// kinds. Mirrors `fixture_architecture` in
/// `chv-controlplane-service/src/netbox_projection_worker_tests.rs` so
/// the two suites pin the same mapping inputs.
fn fixture_architecture() -> CHVArchitecture {
    CHVArchitecture {
        api_version: "chv.kubedo.io/v1alpha1".to_string(),
        kind: "CHVArchitecture".to_string(),
        metadata: Metadata {
            name: "t1".to_string(),
            display_name: None,
            description: None,
            environment: Some("production".to_string()),
            owner: Some("alice".to_string()),
            labels: [("team".to_string(), "platform".to_string())].into(),
        },
        servers: vec![Server {
            name: "chv-node-01".to_string(),
            management_ip: None,
            role: None,
            labels: BTreeMap::new(),
            resources: Some(ServerResources {
                cpu_cores: Some(4),
                memory_gb: Some(8),
            }),
            networks: None,
        }],
        networks: vec![Network {
            name: "backend".to_string(),
            network_type: NetworkType::Vlan,
            bridge: None,
            vlan_id: Some(42),
            cidr: Some("10.42.0.0/24".to_string()),
            gateway: None,
            dns: Vec::new(),
            dhcp: None,
        }],
        datastores: Vec::new(),
        backup_targets: Vec::new(),
        backup_policies: Vec::new(),
        images: Vec::new(),
        templates: Vec::new(),
        instances: vec![Instance {
            name: "vm-01".to_string(),
            template: None,
            placement: Some(InstancePlacement {
                server: Some("chv-node-01".to_string()),
            }),
            resources: Some(InstanceResources {
                cpu: Some(2),
                memory_mb: Some(2048),
            }),
            disks: Vec::new(),
            networks: vec![InstanceNetwork {
                name: "backend".to_string(),
                ip: Some("10.42.0.5".to_string()),
            }],
            cloud_init: None,
            backup: None,
            tags: Vec::new(),
        }],
        ssh_keys: Vec::new(),
        instance_users: Vec::new(),
        roles: Vec::new(),
        users: Vec::new(),
        projects: Vec::new(),
    }
}

/// The model JSON persisted as the applied version's
/// `normalized_model_json` for the wiremock dry-run suites.
fn fixture_model_json() -> String {
    serde_json::to_string(&fixture_architecture()).expect("model serializes")
}

/// Upsert a projection config directly through the repository (bypasses
/// the BFF gates — for arranging preconditions, not testing them).
async fn seed_config(state: &AppState, arch_id: &str, endpoint: &str) {
    state
        .netbox_config
        .upsert(NetboxProjectionConfigUpsertInput {
            architecture_id: ArchitectureId::new(arch_id).expect("valid id"),
            endpoint: endpoint.to_string(),
            token: Some("seeded-netbox-token".to_string()),
            token_secret_ref: format!("netbox-{arch_id}"),
            retention_policy: NetboxRetentionPolicy::MarkStale,
            enable_post_apply: false,
            custom_field_prefix: "chv_".to_string(),
            site_name: None,
        })
        .await
        .expect("seed netbox config");
}

/// The config/upsert body used by most tests.
fn upsert_body(arch_id: &str) -> String {
    format!(
        r#"{{"id":"{arch_id}","expected_version":1,"endpoint":"https://netbox.example.internal","token":"netbox-token-do-not-leak-0123456789","token_secret_ref":"netbox-{arch_id}","retention_policy":"mark_stale","enable_post_apply":true,"site_name":"dc1"}}"#
    )
}

const ALL_EIGHT: &[(&str, &str)] = &[
    ("/v1/architectures/netbox/config/get", r#"{"id":"ID"}"#),
    (
        "/v1/architectures/netbox/config/upsert",
        r#"{"id":"ID","expected_version":1,"endpoint":"https://netbox.example.internal","token_secret_ref":"ref","retention_policy":"mark_stale","enable_post_apply":false}"#,
    ),
    ("/v1/architectures/netbox/config/delete", r#"{"id":"ID"}"#),
    ("/v1/architectures/netbox/export/dry-run", r#"{"id":"ID"}"#),
    ("/v1/architectures/netbox/export", r#"{"id":"ID"}"#),
    ("/v1/architectures/netbox/runs/list", r#"{"id":"ID"}"#),
    (
        "/v1/architectures/netbox/runs/get",
        r#"{"id":"ID","run_id":"netrun-x"}"#,
    ),
    (
        "/v1/architectures/netbox/runs/retry",
        r#"{"id":"ID","run_id":"netrun-x"}"#,
    ),
];

fn body_for(path_body: &str, arch_id: &str) -> String {
    path_body.replace("ID", arch_id)
}

// ---------------------------------------------------------------------------
// Permission matrix
// ---------------------------------------------------------------------------

/// Viewer is 403 on every netbox endpoint (contract: "Viewer: no access
/// to any NetBox projection endpoint"). The routing-layer gate fires
/// before any handler logic, so no seeding is needed.
#[tokio::test]
async fn viewer_is_403_on_all_eight_netbox_endpoints() {
    let state = build_state().await;
    let arch = seed_topology(&state, "perm-view", "u-alice", None).await;
    let viewer = token_for(&state, "u-alice", "viewer");
    for (path, body) in ALL_EIGHT {
        let (status, body) = post_json(&state, path, &viewer, &body_for(body, &arch)).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "viewer must be 403 on {path} (got {status}, body {body})"
        );
    }
}

/// Operator manages the full config lifecycle on their own
/// non-production topology (upsert → get roundtrip), and admin can do
/// the same on a foreign topology.
#[tokio::test]
async fn operator_and_admin_pass_on_non_production_topology() {
    let state = build_state().await;
    let arch = seed_topology(&state, "perm-ok", "u-alice", None).await;

    let operator = token_for(&state, "u-alice", "operator");
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/config/upsert",
        &operator,
        &upsert_body(&arch),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "operator upsert: {body}");
    assert_eq!(body["token_set"], serde_json::json!(true));
    assert_eq!(body["retention_policy"], serde_json::json!("mark_stale"));
    assert_eq!(body["custom_field_prefix"], serde_json::json!("chv_"));

    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/config/get",
        &operator,
        &format!(r#"{{"id":"{arch}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "operator get: {body}");
    assert_eq!(
        body["endpoint"],
        serde_json::json!("https://netbox.example.internal")
    );

    // Admin passes on the same (foreign-to-admin) topology.
    let admin = token_for(&state, "u-admin", "admin");
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/config/get",
        &admin,
        &format!(r#"{{"id":"{arch}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "admin get: {body}");
}

/// Production topology: export by the owner-operator is 403
/// `PRODUCTION_REQUIRES_ADMIN`; export by admin is 200/queued. Config
/// writes on a production row are admin-only too (the update-guard
/// mirror), while config reads stay operator-accessible (the contract
/// escalates export, not config read).
#[tokio::test]
async fn production_export_requires_admin() {
    let state = build_state().await;
    // Operator-owned production topology: ownership passes, so the
    // production guard is what must fire for the operator.
    let arch = seed_topology(&state, "perm-prod", "u-alice", Some("production")).await;
    seed_applied_version(&state, &arch).await;
    seed_config(&state, &arch, "https://netbox.example.internal").await;

    let operator = token_for(&state, "u-alice", "operator");
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/export",
        &operator,
        &format!(r#"{{"id":"{arch}"}}"#),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "operator export on production: {body}"
    );
    assert_eq!(body["code"], serde_json::json!("PRODUCTION_REQUIRES_ADMIN"));

    // Config writes on the production row are admin-only as well.
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/config/upsert",
        &operator,
        &upsert_body(&arch),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "operator config upsert on production: {body}"
    );
    assert_eq!(body["code"], serde_json::json!("PRODUCTION_REQUIRES_ADMIN"));

    // Config READ stays operator-accessible (contract: production
    // escalates export, not config read).
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/config/get",
        &operator,
        &format!(r#"{{"id":"{arch}"}}"#),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "operator config get on production: {body}"
    );

    // Admin exports fine.
    let admin = token_for(&state, "u-admin", "admin");
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/export",
        &admin,
        &format!(r#"{{"id":"{arch}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "admin export on production: {body}");
    assert_eq!(body["status"], serde_json::json!("queued"));
    assert!(body["run_id"].as_str().is_some_and(|s| !s.is_empty()));
}

// ---------------------------------------------------------------------------
// Ownership / IDOR
// ---------------------------------------------------------------------------

/// A non-admin touching a foreign topology gets 403 on every netbox
/// endpoint — never the row's data (the H6 convention from
/// `architectures_ownership_idor.rs`).
#[tokio::test]
async fn foreign_topology_is_403_on_all_eight_netbox_endpoints() {
    let state = build_state().await;
    let bob_arch = seed_topology(&state, "idor-bob", "u-bob", None).await;
    // Seed config + applied version + a run so the 403 provably fires
    // at the ownership gate, not at a later precondition.
    seed_applied_version(&state, &bob_arch).await;
    seed_config(&state, &bob_arch, "https://netbox.example.internal").await;

    let alice = token_for(&state, "u-alice", "operator");
    for (path, body) in ALL_EIGHT {
        let (status, body) = post_json(&state, path, &alice, &body_for(body, &bob_arch)).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "foreign row must 403 on {path} (got {status}, body {body})"
        );
        assert_eq!(body["code"], serde_json::json!("FORBIDDEN"));
    }

    // A missing topology is 404, not 403.
    let (status, _) = post_json(
        &state,
        "/v1/architectures/netbox/config/get",
        &alice,
        r#"{"id":"arch-nonexistent"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Config token redaction
// ---------------------------------------------------------------------------

/// The token (and its ciphertext) never appear in any response body.
/// `token_set: true` is the only token signal on the wire.
#[tokio::test]
async fn config_responses_never_leak_token_or_ciphertext() {
    let state = build_state().await;
    let arch = seed_topology(&state, "redact", "u-alice", None).await;
    let token = "netbox-token-do-not-leak-0123456789";
    let operator = token_for(&state, "u-alice", "operator");

    let (status, raw) = post_raw(
        &state,
        "/v1/architectures/netbox/config/upsert",
        &operator,
        &upsert_body(&arch),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "upsert: {raw}");
    assert!(
        raw.contains("\"token_set\":true"),
        "summary carries token_set: {raw}"
    );
    assert!(
        !raw.contains(token),
        "token leaked in upsert response: {raw}"
    );

    let (status, raw) = post_raw(
        &state,
        "/v1/architectures/netbox/config/get",
        &operator,
        &format!(r#"{{"id":"{arch}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "get: {raw}");
    assert!(!raw.contains(token), "token leaked in get response: {raw}");

    // The stored ciphertext must not leak either (belt-and-braces: the
    // ciphertext is not a secret per se, but echoing it would be a
    // disclosure of the at-rest blob).
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT token_ciphertext FROM netbox_projection_config WHERE architecture_id = ?",
    )
    .bind(&arch)
    .fetch_optional(&state.pool)
    .await
    .expect("fetch ciphertext");
    let ciphertext = row.expect("config row exists").0;
    assert!(!ciphertext.is_empty());
    assert!(
        !raw.contains(&ciphertext),
        "ciphertext leaked in get response"
    );

    // Error responses never embed the token either (the upsert error
    // path — stale expected_version — re-renders the request-derived
    // state without echoing secret fields).
    let (status, raw) = post_raw(
        &state,
        "/v1/architectures/netbox/config/upsert",
        &operator,
        &upsert_body(&arch).replace("\"expected_version\":1", "\"expected_version\":99"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "stale upsert: {raw}");
    assert!(
        !raw.contains(token),
        "token leaked in error response: {raw}"
    );

    // The audit event carries the architecture id and changed-field
    // names only — never the token.
    let events: Vec<(String, String)> = sqlx::query_as(
        "SELECT message, COALESCE(details, '') FROM events WHERE message = 'architecture_netbox_config_updated'",
    )
    .fetch_all(&state.pool)
    .await
    .expect("fetch events");
    assert!(!events.is_empty(), "config_updated event must be emitted");
    for (message, details) in &events {
        assert_eq!(message, "architecture_netbox_config_updated");
        assert!(
            !details.contains(token),
            "token leaked into event details: {details}"
        );
        assert!(
            details.contains(&arch),
            "event carries architecture id: {details}"
        );
    }
}

// ---------------------------------------------------------------------------
// Stable error codes
// ---------------------------------------------------------------------------

/// config/get without a config → 404 `NETBOX_NOT_CONFIGURED` (the
/// contract pins 404 on the config read path).
#[tokio::test]
async fn config_get_without_config_is_404_netbox_not_configured() {
    let state = build_state().await;
    let arch = seed_topology(&state, "noconf", "u-alice", None).await;
    let operator = token_for(&state, "u-alice", "operator");

    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/config/get",
        &operator,
        &format!(r#"{{"id":"{arch}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], serde_json::json!("NETBOX_NOT_CONFIGURED"));

    // config/delete on an absent config answers the same 404.
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/config/delete",
        &operator,
        &format!(r#"{{"id":"{arch}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], serde_json::json!("NETBOX_NOT_CONFIGURED"));
}

/// A plain-HTTP endpoint is rejected at accept time with 400
/// `NETBOX_HTTPS_REQUIRED` (the BFF is the accept-time gate; the store
/// would otherwise persist it verbatim).
#[tokio::test]
async fn config_upsert_http_endpoint_is_400_https_required() {
    let state = build_state().await;
    let arch = seed_topology(&state, "https", "u-alice", None).await;
    let operator = token_for(&state, "u-alice", "operator");

    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/config/upsert",
        &operator,
        &upsert_body(&arch).replace("https://", "http://"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], serde_json::json!("NETBOX_HTTPS_REQUIRED"));

    // Nothing was persisted.
    let (status, _) = post_json(
        &state,
        "/v1/architectures/netbox/config/get",
        &operator,
        &format!(r#"{{"id":"{arch}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A stale `expected_version` answers 409 with the contract's stable
/// `PLAN_EXPIRED` code (the topology optimistic-concurrency rule reused
/// from /v1/architectures/update).
#[tokio::test]
async fn config_upsert_stale_expected_version_is_409_plan_expired() {
    let state = build_state().await;
    let arch = seed_topology(&state, "stale", "u-alice", None).await;
    let operator = token_for(&state, "u-alice", "operator");

    // Create the config first (expected_version 1 is current).
    let (status, _) = post_json(
        &state,
        "/v1/architectures/netbox/config/upsert",
        &operator,
        &upsert_body(&arch),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Bump the topology version out from under the client.
    state
        .topology_repo
        .create(TopologyCreateInput {
            id: ArchitectureId::new("arch-bump-noop").unwrap(),
            name: "bump".into(),
            display_name: None,
            description: None,
            environment: None,
            status: ArchitectureStatus::Draft,
            owner_user_id: Some("u-alice".into()),
            design_graph_json: None,
            latest_yaml: Some("x".into()),
        })
        .await
        .expect("unrelated topology");
    sqlx::query("UPDATE architecture_topologies SET version_number = 2 WHERE id = ?")
        .bind(&arch)
        .execute(&state.pool)
        .await
        .expect("bump version");

    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/config/upsert",
        &operator,
        &upsert_body(&arch),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], serde_json::json!("PLAN_EXPIRED"));
}

/// `retention_policy: "delete"` requires Admin: the operator gets a
/// plain 403, the admin succeeds (contract: delete retention requires
/// Admin; no dedicated code is defined — plain FORBIDDEN, documented in
/// the handler).
#[tokio::test]
async fn delete_retention_policy_requires_admin() {
    let state = build_state().await;
    let arch = seed_topology(&state, "retention", "u-alice", None).await;

    let operator = token_for(&state, "u-alice", "operator");
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/config/upsert",
        &operator,
        &upsert_body(&arch).replace("\"mark_stale\"", "\"delete\""),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], serde_json::json!("FORBIDDEN"));

    let admin = token_for(&state, "u-admin", "admin");
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/config/upsert",
        &admin,
        &upsert_body(&arch).replace("\"mark_stale\"", "\"delete\""),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "admin delete retention: {body}");
    assert_eq!(body["retention_policy"], serde_json::json!("delete"));

    // An invalid retention value is a flat 400.
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/config/upsert",
        &admin,
        &upsert_body(&arch).replace("\"mark_stale\"", "\"explode\""),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

/// Export gates: not-configured (400), never-applied (400
/// `NETBOX_NOT_APPLIED`), happy enqueue (200/queued), active-run
/// conflict (409 `NETBOX_RUN_ACTIVE`).
#[tokio::test]
async fn export_error_codes_not_configured_not_applied_run_active() {
    let state = build_state().await;
    let arch = seed_topology(&state, "export", "u-alice", None).await;
    let operator = token_for(&state, "u-alice", "operator");

    // 1. No config → 400 NETBOX_NOT_CONFIGURED (action path).
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/export",
        &operator,
        &format!(r#"{{"id":"{arch}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], serde_json::json!("NETBOX_NOT_CONFIGURED"));

    // 2. Config but never applied → 400 NETBOX_NOT_APPLIED.
    seed_config(&state, &arch, "https://netbox.example.internal").await;
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/export",
        &operator,
        &format!(r#"{{"id":"{arch}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], serde_json::json!("NETBOX_NOT_APPLIED"));

    // Dry-run answers the same two gates (contract: 400 class on the
    // action paths).
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/export/dry-run",
        &operator,
        &format!(r#"{{"id":"{arch}"}}"#),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "dry-run not applied: {body}"
    );
    assert_eq!(body["code"], serde_json::json!("NETBOX_NOT_APPLIED"));

    // 3. Applied → enqueue succeeds.
    seed_applied_version(&state, &arch).await;
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/export",
        &operator,
        &format!(r#"{{"id":"{arch}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "export enqueue: {body}");
    assert_eq!(body["status"], serde_json::json!("queued"));
    assert_eq!(body["architecture_id"], serde_json::json!(arch));
    let run_id = body["run_id"].as_str().expect("run_id").to_string();

    // 4. Second export while the first is queued → 409 NETBOX_RUN_ACTIVE.
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/export",
        &operator,
        &format!(r#"{{"id":"{arch}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], serde_json::json!("NETBOX_RUN_ACTIVE"));

    // 5. Retrying the still-queued run → 409 PROJECTION_RUN_NOT_RETRYABLE.
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/runs/retry",
        &operator,
        &format!(r#"{{"id":"{arch}","run_id":"{run_id}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        body["code"],
        serde_json::json!("PROJECTION_RUN_NOT_RETRYABLE")
    );
}

/// Dry-run against a dead HTTPS port answers 502 `NETBOX_UNREACHABLE`
/// (the synchronous transport leg; the client's connect fails fast on
/// connection refused). The full plan-shape success leg lives in the
/// PR-4 wiremock suites — see the module doc.
#[tokio::test]
async fn dry_run_unreachable_netbox_is_502_netbox_unreachable() {
    let state = build_state().await;
    let arch = seed_topology(&state, "dryrun", "u-alice", None).await;
    seed_applied_version(&state, &arch).await;
    // Port 1 on loopback: connection refused — an instant Unreachable,
    // not a 10s timeout wait.
    seed_config(&state, &arch, "https://127.0.0.1:1").await;

    let operator = token_for(&state, "u-alice", "operator");
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/export/dry-run",
        &operator,
        &format!(r#"{{"id":"{arch}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "dry-run dead port: {body}");
    assert_eq!(body["code"], serde_json::json!("NETBOX_UNREACHABLE"));
}

/// A stored ciphertext that no longer decrypts (here: corrupted via
/// direct SQL) fails closed on dry-run: 400 with the literal
/// `NETBOX_TOKEN_MISSING` code — never the ciphertext, never a 500.
/// The config row exists, so the not-configured gate passes and the
/// decrypt failure is what must surface.
#[tokio::test]
async fn dry_run_corrupted_ciphertext_is_400_netbox_token_missing() {
    let state = build_state().await;
    let arch = seed_topology(&state, "tokmissing", "u-alice", None).await;
    seed_applied_version(&state, &arch).await;
    seed_config(&state, &arch, "https://netbox.example.internal").await;

    // Corrupt the stored ciphertext directly: the row still exists
    // (row-present == token-set on config responses), but the secret
    // can no longer be decrypted. The `enc:` prefix is required —
    // unprefixed values are backward-compatibility plaintext and would
    // pass through; a well-formed prefix with a bogus payload fails
    // closed (hex/GCM authentication).
    sqlx::query(
        "UPDATE netbox_projection_config SET token_ciphertext = 'enc:deadbeef' WHERE architecture_id = ?",
    )
    .bind(&arch)
    .execute(&state.pool)
    .await
    .expect("corrupt ciphertext");

    let operator = token_for(&state, "u-alice", "operator");
    let (status, raw) = post_raw(
        &state,
        "/v1/architectures/netbox/export/dry-run",
        &operator,
        &format!(r#"{{"id":"{arch}"}}"#),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "corrupted ciphertext: {raw}"
    );
    assert!(
        raw.contains("\"NETBOX_TOKEN_MISSING\""),
        "literal NETBOX_TOKEN_MISSING code must be in the body: {raw}"
    );
    // Fail-closed: the ciphertext blob never reaches the wire.
    assert!(!raw.contains("deadbeef"), "ciphertext echoed: {raw}");
}

/// #655: when no credential-encryption key is configured, a config upsert
/// that *does* carry a token must fail closed and must not be reported as
/// `NETBOX_TOKEN_MISSING` — the caller supplied a token; the server cannot
/// encrypt it. The row must not be written, and the response must not echo
/// the plaintext token.
#[tokio::test]
async fn config_upsert_without_encryption_key_fails_closed_and_is_not_mislabeled() {
    let mut state = build_state().await;
    let arch = seed_topology(&state, "nokey-encrypt", "u-alice", None).await;
    state.netbox_config = Arc::new(NetboxProjectionConfigRepository::with_crypto(
        state.pool.clone(),
        chv_controlplane_store::CredentialEncryption::without_key(),
    ));

    let operator = token_for(&state, "u-alice", "operator");
    let (status, raw) = post_raw(
        &state,
        "/v1/architectures/netbox/config/upsert",
        &operator,
        &upsert_body(&arch),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "a missing encryption key is a server misconfiguration: {raw}"
    );
    assert!(
        raw.contains("\"INTERNAL_ERROR\""),
        "expected the generic internal-error code: {raw}"
    );
    assert!(
        !raw.contains("NETBOX_TOKEN_MISSING"),
        "a supplied token must not be reported as missing: {raw}"
    );
    assert!(
        !raw.contains("netbox-token-do-not-leak"),
        "the request token must never be echoed: {raw}"
    );

    assert!(
        state
            .netbox_config
            .get(&ArchitectureId::new(arch.clone()).expect("valid architecture id"))
            .await
            .unwrap()
            .is_none(),
        "a failed encryption must not persist a config row"
    );
}

/// A NetBox that rejects the configured token (401 on the first list
/// endpoint) answers 502 with the literal `NETBOX_AUTH_FAILED` code.
/// The mock is reached through the crate's dev-only `test-http` client
/// seam (see the module doc); the config is seeded repository-side
/// because the BFF's accept-time gate (rightly) refuses plain HTTP.
#[tokio::test]
async fn dry_run_netbox_rejecting_token_is_502_netbox_auth_failed() {
    let state = build_state().await;
    let arch = seed_topology(&state, "authfail", "u-alice", None).await;
    seed_applied_version(&state, &arch).await;

    let server = MockServer::start().await;
    // 401 on every GET — the first per-kind list endpoint the runner
    // fetches fails auth.
    Mock::given(method("GET"))
        .and(path_regex("^/api/"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    seed_config(&state, &arch, &server.uri()).await;

    let operator = token_for(&state, "u-alice", "operator");
    let (status, raw) = post_raw(
        &state,
        "/v1/architectures/netbox/export/dry-run",
        &operator,
        &format!(r#"{{"id":"{arch}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "401 mock: {raw}");
    assert!(
        raw.contains("\"NETBOX_AUTH_FAILED\""),
        "literal NETBOX_AUTH_FAILED code must be in the body: {raw}"
    );
}

/// Dry-run against an all-empty NetBox (every per-kind list and every
/// natural-key probe returns an empty page) succeeds with the
/// contract's plan shape, asserted field-by-field: the fixture
/// architecture projects exactly one object of each of the six mapped
/// kinds, all `create` (nothing of ours exists remotely). Mirrors the
/// worker-suite fixture (`netbox_projection_worker_tests.rs`).
#[tokio::test]
async fn dry_run_against_empty_netbox_returns_contract_plan_shape() {
    let state = build_state().await;
    let arch = seed_topology(&state, "planshape", "u-alice", None).await;
    seed_applied_version_with_model(&state, &arch, &fixture_model_json()).await;

    let server = MockServer::start().await;
    // Every GET under /api/ — the six per-kind custom-field lists and
    // the natural-key probes alike — answers an empty page.
    Mock::given(method("GET"))
        .and(path_regex("^/api/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "count": 0,
            "next": null,
            "results": [],
        })))
        .mount(&server)
        .await;
    seed_config(&state, &arch, &server.uri()).await;

    let operator = token_for(&state, "u-alice", "operator");
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/export/dry-run",
        &operator,
        &format!(r#"{{"id":"{arch}"}}"#),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "dry-run against empty netbox: {body}"
    );

    // The contract's plan shape, field by field.
    assert_eq!(body["mapping_version"], serde_json::json!("v1"));
    assert_eq!(body["architecture_id"], serde_json::json!(arch));
    assert_eq!(body["architecture_version"], serde_json::json!(1));
    assert_eq!(body["retention"], serde_json::json!("mark_stale"));
    assert_eq!(
        body["summary"],
        serde_json::json!({"create": 6, "update": 0, "no_op": 0, "conflict": 0, "stale": 0}),
        "one create per mapped kind: {body}"
    );

    // Entries: six creates, one per kind, each with the action/kind/ref
    // fields the contract's entry shape defines.
    let entries = body["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 6, "one entry per mapped kind: {body}");
    let mut kinds: Vec<&str> = entries
        .iter()
        .map(|entry| {
            assert_eq!(
                entry["action"],
                serde_json::json!("create"),
                "entry: {entry}"
            );
            // A create carries no diff lines.
            assert_eq!(entry["changes"], serde_json::json!([]), "entry: {entry}");
            let kind = entry["kind"].as_str().expect("kind string");
            let reference = entry["chv_resource_ref"].as_str().expect("ref string");
            assert!(
                reference.contains('/'),
                "chv_resource_ref is servers/<name>-style: {entry}"
            );
            assert!(
                !entry["netbox_natural_key"]
                    .as_object()
                    .expect("natural key object")
                    .is_empty(),
                "entry carries its natural key: {entry}"
            );
            assert!(
                entry["external_id"].as_str().is_some_and(|s| !s.is_empty()),
                "entry carries an external id: {entry}"
            );
            kind
        })
        .collect();
    kinds.sort_unstable();
    assert_eq!(
        kinds,
        [
            "device",
            "interface",
            "ip_address",
            "prefix",
            "virtual_machine",
            "vlan"
        ],
        "exactly one entry of each mapped kind: {body}"
    );

    // The dry-run event carries the contract's event fields:
    // architecture id, run id (null — nothing persisted), trigger
    // (manual — the only way in is the API), and the summary counts.
    let events: Vec<(String,)> = sqlx::query_as(
        "SELECT COALESCE(details, '') FROM events WHERE message = 'architecture_netbox_dry_run'",
    )
    .fetch_all(&state.pool)
    .await
    .expect("fetch events");
    assert_eq!(events.len(), 1, "exactly one dry-run event: {events:?}");
    let details = &events[0].0;
    assert!(
        details.contains(&arch),
        "event carries architecture id: {details}"
    );
    assert!(
        details.contains("\"run_id\":null"),
        "event carries run_id: null (no run persisted): {details}"
    );
    assert!(
        details.contains("\"trigger\":\"manual\""),
        "event carries trigger manual: {details}"
    );
    assert!(
        details.contains("\"create\":6"),
        "event carries the summary counts: {details}"
    );
}

/// An unparseable `plan_json` column degrades to the raw string on
/// runs/get (the contract: parsed JSON when parseable, raw otherwise) —
/// never null, never a 500. An unparseable `result_json` degrades the
/// same way, and (no envelope to lift) leaves
/// `resolved_architecture_version_id` null.
#[tokio::test]
async fn runs_get_unparseable_plan_json_returns_raw_string() {
    let state = build_state().await;
    let arch = seed_topology(&state, "rawplan", "u-alice", None).await;
    let version_id = seed_applied_version(&state, &arch).await;

    let run_repo = NetboxProjectionRunRepository::new(state.pool.clone());
    let run_id = chv_controlplane_types::architecture::NetboxProjectionRunId::new("netrun-raw-1")
        .expect("valid id");
    run_repo
        .create(chv_controlplane_store::NetboxProjectionRunCreateInput {
            id: run_id.clone(),
            architecture_id: ArchitectureId::new(&arch).expect("valid id"),
            architecture_version_id: ArchitectureVersionId::new(&version_id).expect("valid id"),
            trigger_kind: chv_controlplane_types::architecture::NetboxProjectionTrigger::Manual,
            mode: chv_controlplane_types::architecture::NetboxProjectionMode::Export,
            plan_json: None,
            requested_by: None,
        })
        .await
        .expect("create run");

    // A plan payload that is not JSON (a truncated write, say).
    sqlx::query("UPDATE netbox_projection_runs SET plan_json = 'not-json{' WHERE id = ?")
        .bind(run_id.as_str())
        .execute(&state.pool)
        .await
        .expect("corrupt plan_json");
    // … and a result payload that is not JSON either.
    sqlx::query("UPDATE netbox_projection_runs SET result_json = 'also-not-json{' WHERE id = ?")
        .bind(run_id.as_str())
        .execute(&state.pool)
        .await
        .expect("corrupt result_json");

    let operator = token_for(&state, "u-alice", "operator");
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/runs/get",
        &operator,
        &format!(r#"{{"id":"{arch}","run_id":"{run_id}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "runs/get with garbage plan: {body}");
    assert_eq!(
        body["plan_json"],
        serde_json::json!("not-json{"),
        "the raw string comes back verbatim — not null, not an error: {body}"
    );
    assert_eq!(
        body["result_json"],
        serde_json::json!("also-not-json{"),
        "the raw result string comes back verbatim: {body}"
    );
    assert_eq!(
        body["resolved_architecture_version_id"],
        serde_json::Value::Null,
        "no envelope to lift on a raw-string column: {body}"
    );
}

/// runs/get unwraps the worker's provenance envelope: the worker
/// persists `result_json` as `{ resolved_architecture_version_id,
/// result }` (see `NetboxProjectionWorker::result_envelope` in
/// `chv-controlplane-service`), and the contract's runs/get serves the
/// per-entry outcome — so the response's `result_json` is the UNWRAPPED
/// outcome and `resolved_architecture_version_id` surfaces the
/// envelope's value as a first-class field.
#[tokio::test]
async fn runs_get_unwraps_worker_result_envelope() {
    let state = build_state().await;
    let arch = seed_topology(&state, "envelope", "u-alice", None).await;
    let version_id = seed_applied_version(&state, &arch).await;
    seed_config(&state, &arch, "https://netbox.example.internal").await;

    // The run row exactly as the worker leaves it after a successful
    // export: created → claimed → `mark_succeeded` with the envelope
    // (same repository calls the worker makes — no BFF shortcut).
    let run_repo = NetboxProjectionRunRepository::new(state.pool.clone());
    let run_id = chv_controlplane_types::architecture::NetboxProjectionRunId::new("netrun-env-1")
        .expect("valid id");
    run_repo
        .create(chv_controlplane_store::NetboxProjectionRunCreateInput {
            id: run_id.clone(),
            architecture_id: ArchitectureId::new(&arch).expect("valid id"),
            architecture_version_id: ArchitectureVersionId::new(&version_id).expect("valid id"),
            trigger_kind: chv_controlplane_types::architecture::NetboxProjectionTrigger::Manual,
            mode: chv_controlplane_types::architecture::NetboxProjectionMode::Export,
            plan_json: None,
            requested_by: Some("u-alice".into()),
        })
        .await
        .expect("create run");
    let claimed = run_repo
        .claim_next_queued(&ArchitectureId::new(&arch).unwrap())
        .await
        .expect("claim")
        .expect("queued run");
    assert_eq!(claimed.id, run_id);

    // The adapter outcome (flat: plan + entries + summary + error)
    // inside the worker's provenance envelope — mirroring what
    // `result_envelope` serializes.
    let outcome = serde_json::json!({
        "plan": {
            "mapping_version": "v1",
            "architecture_id": arch,
            "architecture_version": 1,
            "retention": "mark_stale",
            "summary": { "create": 6, "update": 0, "no_op": 0, "conflict": 0, "stale": 0 },
            "entries": [],
        },
        "entries": [
            {
                "action": "create",
                "kind": "vlan",
                "chv_resource_ref": "networks/backend",
                "status": "succeeded",
                "error": null,
            }
        ],
        "summary": { "succeeded": 1, "failed": 0, "skipped": 0, "not_attempted": 0 },
        "error": null,
    });
    let envelope = serde_json::json!({
        "resolved_architecture_version_id": version_id,
        "result": outcome,
    });
    run_repo
        .mark_succeeded(
            &run_id,
            Some(envelope.to_string()),
            Some(r#"{"create":6,"update":0,"no_op":0,"conflict":0,"stale":0}"#.to_string()),
        )
        .await
        .expect("mark succeeded");

    let operator = token_for(&state, "u-alice", "operator");
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/runs/get",
        &operator,
        &format!(r#"{{"id":"{arch}","run_id":"{run_id}"}}"#),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "runs/get with enveloped result: {body}"
    );
    // THE unwrap: the served result_json is the inner outcome, not the
    // envelope — the UI's flat `NetboxRunResult` shape.
    assert_eq!(
        body["result_json"], outcome,
        "result_json is the unwrapped outcome: {body}"
    );
    assert_eq!(
        body["resolved_architecture_version_id"],
        serde_json::json!(version_id),
        "the envelope's version id surfaces as a first-class field: {body}"
    );
    // The envelope's own keys never leak into the served outcome.
    assert!(
        body["result_json"]
            .get("resolved_architecture_version_id")
            .is_none()
            && body["result_json"].get("result").is_none(),
        "no envelope keys inside the served result_json: {body}"
    );
}

/// Forward compatibility: a parsed `result_json` object WITHOUT the
/// envelope's key pair passes through unchanged and
/// `resolved_architecture_version_id` stays null — the unwrap never
/// guesses at unknown shapes (a pre-envelope worker's flat outcome, or
/// a future envelope revision, is served verbatim).
#[tokio::test]
async fn runs_get_non_envelope_result_json_passes_through() {
    let state = build_state().await;
    let arch = seed_topology(&state, "flatres", "u-alice", None).await;
    let version_id = seed_applied_version(&state, &arch).await;

    let run_repo = NetboxProjectionRunRepository::new(state.pool.clone());
    let run_id = chv_controlplane_types::architecture::NetboxProjectionRunId::new("netrun-flat-1")
        .expect("valid id");
    run_repo
        .create(chv_controlplane_store::NetboxProjectionRunCreateInput {
            id: run_id.clone(),
            architecture_id: ArchitectureId::new(&arch).expect("valid id"),
            architecture_version_id: ArchitectureVersionId::new(&version_id).expect("valid id"),
            trigger_kind: chv_controlplane_types::architecture::NetboxProjectionTrigger::Manual,
            mode: chv_controlplane_types::architecture::NetboxProjectionMode::Export,
            plan_json: None,
            requested_by: None,
        })
        .await
        .expect("create run");
    run_repo
        .claim_next_queued(&ArchitectureId::new(&arch).unwrap())
        .await
        .expect("claim")
        .expect("queued run");

    // A flat outcome (no envelope keys) — what a pre-envelope writer
    // would have left in the column.
    let flat = serde_json::json!({
        "entries": [
            {
                "action": "create",
                "kind": "vlan",
                "chv_resource_ref": "networks/backend",
                "status": "succeeded",
                "error": null,
            }
        ],
        "summary": { "succeeded": 1, "failed": 0, "skipped": 0, "not_attempted": 0 },
    });
    run_repo
        .mark_succeeded(&run_id, Some(flat.to_string()), None)
        .await
        .expect("mark succeeded");

    let operator = token_for(&state, "u-alice", "operator");
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/runs/get",
        &operator,
        &format!(r#"{{"id":"{arch}","run_id":"{run_id}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "runs/get with flat result: {body}");
    assert_eq!(
        body["result_json"], flat,
        "a non-envelope object passes through unchanged: {body}"
    );
    assert_eq!(
        body["resolved_architecture_version_id"],
        serde_json::Value::Null,
        "no envelope → no resolved version id: {body}"
    );
}

// ---------------------------------------------------------------------------
// Happy paths — runs list / get / retry
// ---------------------------------------------------------------------------

/// After an enqueued export: runs/list shows the queued run with the
/// contract's summary fields; runs/get returns the full run; a run id
/// from another architecture (or a bogus id) answers 404.
#[tokio::test]
async fn runs_list_and_get_after_export_enqueue() {
    let state = build_state().await;
    let arch = seed_topology(&state, "runs", "u-alice", None).await;
    let version_id = seed_applied_version(&state, &arch).await;
    seed_config(&state, &arch, "https://netbox.example.internal").await;

    let operator = token_for(&state, "u-alice", "operator");
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/export",
        &operator,
        &format!(r#"{{"id":"{arch}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let run_id = body["run_id"].as_str().expect("run_id").to_string();

    // runs/list
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/runs/list",
        &operator,
        &format!(r#"{{"id":"{arch}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let runs = body["runs"].as_array().expect("runs array");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["id"], serde_json::json!(run_id));
    assert_eq!(runs[0]["trigger"], serde_json::json!("manual"));
    assert_eq!(runs[0]["status"], serde_json::json!("queued"));
    assert_eq!(runs[0]["mode"], serde_json::json!("export"));
    assert!(runs[0].get("created_at").is_some());

    // runs/get — full run incl. the (still-null) plan_json.
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/runs/get",
        &operator,
        &format!(r#"{{"id":"{arch}","run_id":"{run_id}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], serde_json::json!(run_id));
    assert_eq!(
        body["architecture_version_id"],
        serde_json::json!(version_id)
    );
    assert_eq!(body["plan_json"], serde_json::Value::Null);
    assert_eq!(body["requested_by"], serde_json::json!("u-alice"));
    // No result yet → no envelope → the resolved-version field is null.
    assert_eq!(body["result_json"], serde_json::Value::Null);
    assert_eq!(
        body["resolved_architecture_version_id"],
        serde_json::Value::Null
    );

    // A run id under a DIFFERENT architecture answers 404 — run ids
    // must not become cross-architecture probes.
    let other_arch = seed_topology(&state, "runs-other", "u-alice", None).await;
    let (status, _) = post_json(
        &state,
        "/v1/architectures/netbox/runs/get",
        &operator,
        &format!(r#"{{"id":"{other_arch}","run_id":"{run_id}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A bogus run id answers 404.
    let (status, _) = post_json(
        &state,
        "/v1/architectures/netbox/runs/get",
        &operator,
        &format!(r#"{{"id":"{arch}","run_id":"netrun-bogus"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// runs/list parses `summary_json` into the `summary` field (invalid
/// JSON degrades to `null`) and honors the limit: newest first, default
/// 20, clamped to `1..=100` rather than rejected.
#[tokio::test]
async fn runs_list_parses_summary_and_applies_limit() {
    let state = build_state().await;
    let arch = seed_topology(&state, "runs-limit", "u-alice", None).await;
    let version_id = seed_applied_version(&state, &arch).await;
    seed_config(&state, &arch, "https://netbox.example.internal").await;

    let run_repo = NetboxProjectionRunRepository::new(state.pool.clone());
    let architecture_id = ArchitectureId::new(&arch).expect("valid id");
    let architecture_version_id = ArchitectureVersionId::new(&version_id).expect("valid id");

    // Three terminal runs, each driven through the real state machine
    // (create → claim → mark_succeeded) so the one-active index permits
    // them all. Per-index summary shapes: l0 (oldest) valid JSON, l1
    // unparseable (must degrade to null), l2 (newest) absent.
    let summaries: [Option<&str>; 3] = [
        Some(r#"{"create":1,"update":2,"no_op":0,"conflict":0,"stale":0}"#),
        Some("not-json"),
        None,
    ];
    for (i, summary) in summaries.into_iter().enumerate() {
        let run_id = chv_controlplane_types::architecture::NetboxProjectionRunId::new(format!(
            "netrun-l{i}"
        ))
        .expect("valid id");
        run_repo
            .create(chv_controlplane_store::NetboxProjectionRunCreateInput {
                id: run_id.clone(),
                architecture_id: architecture_id.clone(),
                architecture_version_id: architecture_version_id.clone(),
                trigger_kind: chv_controlplane_types::architecture::NetboxProjectionTrigger::Manual,
                mode: chv_controlplane_types::architecture::NetboxProjectionMode::Export,
                plan_json: None,
                requested_by: None,
            })
            .await
            .expect("create run");
        let claimed = run_repo
            .claim_next_queued(&architecture_id)
            .await
            .expect("claim")
            .expect("queued run");
        assert_eq!(claimed.id, run_id);
        run_repo
            .mark_succeeded(&run_id, None, summary.map(str::to_string))
            .await
            .expect("mark succeeded");

        // SQLite's DEFAULT timestamps have second granularity, so three
        // runs created in the same test tick would tie on created_at
        // and the newest-first ordering would be undefined. Stamp
        // distinct RFC3339 values (the format parse_ts accepts).
        sqlx::query("UPDATE netbox_projection_runs SET created_at = ? WHERE id = ?")
            .bind(format!("2024-01-01T00:00:0{i}Z"))
            .bind(run_id.as_str())
            .execute(&state.pool)
            .await
            .expect("stamp distinct created_at");
    }

    let operator = token_for(&state, "u-alice", "operator");

    // Default limit (20): all three runs, newest first.
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/runs/list",
        &operator,
        &format!(r#"{{"id":"{arch}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let runs = body["runs"].as_array().expect("runs array");
    assert_eq!(runs.len(), 3, "default limit shows all three: {body}");
    assert_eq!(runs[0]["id"], serde_json::json!("netrun-l2"));
    assert_eq!(runs[1]["id"], serde_json::json!("netrun-l1"));
    assert_eq!(runs[2]["id"], serde_json::json!("netrun-l0"));
    assert_eq!(runs[0]["status"], serde_json::json!("succeeded"));

    // summary_json parsing: absent → null, unparseable → null (the
    // counts are advisory), valid → parsed object.
    assert_eq!(runs[0]["summary"], serde_json::Value::Null);
    assert_eq!(runs[1]["summary"], serde_json::Value::Null);
    assert_eq!(
        runs[2]["summary"],
        serde_json::json!({"create": 1, "update": 2, "no_op": 0, "conflict": 0, "stale": 0})
    );

    // limit = 2 → only the newest two.
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/runs/list",
        &operator,
        &format!(r#"{{"id":"{arch}","limit":2}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let runs = body["runs"].as_array().expect("runs array");
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0]["id"], serde_json::json!("netrun-l2"));
    assert_eq!(runs[1]["id"], serde_json::json!("netrun-l1"));

    // limit = 0 clamps to 1 (clamped, not rejected).
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/runs/list",
        &operator,
        &format!(r#"{{"id":"{arch}","limit":0}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["runs"].as_array().expect("runs array").len(), 1);

    // An oversized limit is clamped to the cap, not rejected.
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/runs/list",
        &operator,
        &format!(r#"{{"id":"{arch}","limit":100000}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["runs"].as_array().expect("runs array").len(), 3);
}

/// Retry of a failed below-cap run requeues it (200 `{run_id, status:
/// "queued"}`) and emits the `architecture_netbox_export_retried` event.
#[tokio::test]
async fn runs_retry_failed_below_cap_run_requeues() {
    let state = build_state().await;
    let arch = seed_topology(&state, "retry", "u-alice", None).await;
    let version_id = seed_applied_version(&state, &arch).await;
    seed_config(&state, &arch, "https://netbox.example.internal").await;

    // Create a run directly and drive it to `failed` (the worker is
    // PR-4-tested; here we only need the row state).
    let run_repo = NetboxProjectionRunRepository::new(state.pool.clone());
    let run_id = chv_controlplane_types::architecture::NetboxProjectionRunId::new("netrun-retry-1")
        .expect("valid id");
    run_repo
        .create(chv_controlplane_store::NetboxProjectionRunCreateInput {
            id: run_id.clone(),
            architecture_id: ArchitectureId::new(&arch).expect("valid id"),
            architecture_version_id: ArchitectureVersionId::new(&version_id).expect("valid id"),
            trigger_kind: chv_controlplane_types::architecture::NetboxProjectionTrigger::Manual,
            mode: chv_controlplane_types::architecture::NetboxProjectionMode::Export,
            plan_json: None,
            requested_by: Some("u-alice".into()),
        })
        .await
        .expect("create run");
    let claimed = run_repo
        .claim_next_queued(&ArchitectureId::new(&arch).unwrap())
        .await
        .expect("claim")
        .expect("queued run");
    assert_eq!(claimed.id, run_id);
    run_repo
        .mark_failed(&run_id, Some("netbox unreachable (seeded)".into()), None)
        .await
        .expect("mark failed");

    let operator = token_for(&state, "u-alice", "operator");
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/runs/retry",
        &operator,
        &format!(r#"{{"id":"{arch}","run_id":"{run_id}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "retry failed run: {body}");
    assert_eq!(body["run_id"], serde_json::json!(run_id.as_str()));
    assert_eq!(body["status"], serde_json::json!("queued"));

    // The run is back in the queued state (and re-listed as such).
    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/runs/get",
        &operator,
        &format!(r#"{{"id":"{arch}","run_id":"{run_id}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], serde_json::json!("queued"));

    // The retried event was emitted, carrying run + architecture ids.
    let events: Vec<(String, String)> = sqlx::query_as(
        "SELECT message, COALESCE(details, '') FROM events WHERE message = 'architecture_netbox_export_retried'",
    )
    .fetch_all(&state.pool)
    .await
    .expect("fetch events");
    assert_eq!(events.len(), 1, "exactly one retried event: {events:?}");
    assert!(events[0].1.contains(&arch), "event carries architecture id");
    assert!(
        events[0].1.contains(run_id.as_str()),
        "event carries run id: {}",
        events[0].1
    );
}

/// config/delete removes the config (NetBox untouched), returns
/// `{deleted: true}`, and is idempotent-404 on repeat.
#[tokio::test]
async fn config_delete_roundtrip() {
    let state = build_state().await;
    let arch = seed_topology(&state, "del", "u-alice", None).await;
    let operator = token_for(&state, "u-alice", "operator");

    let (status, _) = post_json(
        &state,
        "/v1/architectures/netbox/config/upsert",
        &operator,
        &upsert_body(&arch),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = post_json(
        &state,
        "/v1/architectures/netbox/config/delete",
        &operator,
        &format!(r#"{{"id":"{arch}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deleted"], serde_json::json!(true));

    // Gone: get → 404, second delete → 404.
    let (status, _) = post_json(
        &state,
        "/v1/architectures/netbox/config/get",
        &operator,
        &format!(r#"{{"id":"{arch}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = post_json(
        &state,
        "/v1/architectures/netbox/config/delete",
        &operator,
        &format!(r#"{{"id":"{arch}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The deletion emitted a config_updated event with deleted: true.
    let events: Vec<(String,)> = sqlx::query_as(
        "SELECT COALESCE(details, '') FROM events WHERE message = 'architecture_netbox_config_updated'",
    )
    .fetch_all(&state.pool)
    .await
    .expect("fetch events");
    assert!(
        events
            .iter()
            .any(|(d,)| d.contains("\"deleted\":true") || d.contains("\"deleted\": true")),
        "a deleted:true detail must be present: {events:?}"
    );
}
