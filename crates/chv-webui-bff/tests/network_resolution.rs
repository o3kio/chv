//! Integration tests for the vm-create → network resolution chain
//! (`POST /v1/networks/create`, `POST /v1/vms/create`).
//!
//! Issue kubedoio/chv#354: `vm create --network <name>` looked the
//! reference up by `network_id` only. `network create` stores the
//! operator's name as `display_name` with a generated short
//! `network_id`, so the lookup always missed and the create silently
//! made an implicit network with the fallback CIDR `10.200.0.0/24` —
//! the operator's chosen cidr was ignored, and a second such network
//! collided on the subnet (duplicate host routes; host→guest
//! connectivity to the second bridge broken; verified on real KVM by
//! the M4.4 qualification, evidence
//! `04-real-host-qualification/m4.4-network.md`).
//!
//! Fixed contract asserted here:
//! - `vm create --network <name>` resolves an existing network by
//!   `network_id` first, then by `display_name` (fleet-wide or scoped
//!   to the placement node);
//! - the resolved network's cidr drives the VM's IPAM address;
//! - an unknown reference still implicitly creates a network, but the
//!   create now REFUSES (409) when the implicit cidr would collide
//!   with an existing network's cidr usable on the same node;
//! - an unknown reference with a non-colliding (explicit) cidr still
//!   implicitly creates the network (legacy escape hatch preserved);
//! - `network_id` resolution wins over a same-named display_name;
//! - the default path (no `--network`) still implicitly creates the
//!   `'default'` network when nothing of that id/name exists.

use std::sync::Arc;

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
use chv_webui_bff::mutations::MutationService;
use chv_webui_bff::{AppState, BffError};
use sqlx::sqlite::SqlitePoolOptions;
use tower::ServiceExt;

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
        _vm_id: String,
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
    async fn clone_volume(
        &self,
        _source_volume_id: String,
        _target_volume_id: String,
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
        monitoring: None,
        monitoring_health: chv_webui_bff::MonitoringHealth::new(),
        agent_runtime_dir: std::path::PathBuf::from("/var/lib/chv/agent"),
        cache: chv_webui_bff::BffCache::new(5),
        clock: Arc::new(SystemClock),
    }
}

/// Seed an operator user and return a usable JWT bearer token.
async fn seed_jwt(state: &AppState) -> String {
    sqlx::query(
        "INSERT INTO users (user_id, username, password_hash, role, must_change_password) \
         VALUES ('u-ops', 'ops', 'x', 'operator', 0)",
    )
    .execute(&state.pool)
    .await
    .expect("seed user");

    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let claims = chv_webui_bff::auth::Claims {
        sub: "u-ops".to_string(),
        username: "ops".to_string(),
        role: "operator".to_string(),
        exp,
        must_change_password: false,
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

/// Seed one enrolled node so create_vm has a placement target.
async fn seed_node(state: &AppState) {
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('n-1', 'h-1', 'Node 1')",
    )
    .execute(&state.pool)
    .await
    .expect("seed node");
}

async fn post_with_token(
    state: AppState,
    path: &str,
    token: &str,
    body: &str,
) -> (StatusCode, serde_json::Value) {
    let app = chv_webui_bff::bff_router(state.clone()).with_state(state);
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, body)
}

/// Create a VM with the given network reference; return (status, vm_id).
async fn create_vm_on_network(
    state: &AppState,
    token: &str,
    network_ref: &str,
) -> (StatusCode, Option<String>) {
    let (status, body) = post_with_token(
        state.clone(),
        "/v1/vms/create",
        token,
        &format!(r#"{{"name":"vm-x","image_ref":"/tmp/x.img","network_id":"{network_ref}"}}"#),
    )
    .await;
    let vm_id = body["vm_id"].as_str().map(|s| s.to_string());
    (status, vm_id)
}

/// Create an operator network; return its generated network_id.
async fn create_network(state: &AppState, token: &str, name: &str, cidr: &str) -> String {
    let (status, body) = post_with_token(
        state.clone(),
        "/v1/networks/create",
        token,
        &format!(r#"{{"name":"{name}","cidr":"{cidr}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "network create body: {body}");
    body["network_id"]
        .as_str()
        .expect("network_id in response")
        .to_string()
}

/// The (network_id, ip_address) of the VM's NIC row.
async fn vm_nic(state: &AppState, vm_id: &str) -> (String, String) {
    sqlx::query_as::<_, (String, String)>(
        "SELECT network_id, ip_address FROM vm_nic_desired_state WHERE vm_id = ?",
    )
    .bind(vm_id)
    .fetch_one(&state.pool)
    .await
    .expect("query vm_nic_desired_state")
}

/// Seed a networks + network_desired_state row pair with an explicit
/// node scope (None = fleet-wide, the operator-created shape).
async fn seed_network_row(
    state: &AppState,
    network_id: &str,
    display_name: &str,
    node_id: Option<&str>,
    cidr: &str,
) {
    sqlx::query(
        "INSERT INTO networks (network_id, node_id, display_name, network_class, owner_id, created_at, updated_at) \
         VALUES (?, ?, ?, 'bridge', 'u-ops', strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'))",
    )
    .bind(network_id)
    .bind(node_id)
    .bind(display_name)
    .execute(&state.pool)
    .await
    .expect("seed networks row");
    sqlx::query(
        "INSERT INTO network_desired_state (network_id, desired_generation, desired_status, cidr, gateway, dhcp_enabled, ipam_mode, is_default, requested_at, updated_at) \
         VALUES (?, 1, 'Pending', ?, '10.0.0.1', 1, 'internal', 0, strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'))",
    )
    .bind(network_id)
    .bind(cidr)
    .execute(&state.pool)
    .await
    .expect("seed network_desired_state row");
}

// ---------------------------------------------------------------------------
// resolution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn network_create_defaults_absent_gateway() {
    // N6 (M4.4 re-qualification): a network created WITHOUT a gateway must
    // default to the cidr's first usable host — the same default the
    // vm-create implicit-network fallback always carried. A NULL gateway
    // dispatches `gateway: ""` in the VM spec, and nwd's ensure then skips
    // both the bridge's L3 address and dnsmasq: VMs attach to an L2-only
    // bridge with no connectivity and no DHCP.
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;

    // Absent gateway → defaulted to the cidr's .1 (persisted, and visible
    // in the create response's detail).
    let (status, body) = post_with_token(
        state.clone(),
        "/v1/networks/create",
        &token,
        r#"{"name":"tenant-gw","cidr":"10.77.0.0/24"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create body: {body}");
    let net_id = body["network_id"].as_str().expect("network_id").to_string();
    let stored: Option<String> =
        sqlx::query_scalar("SELECT gateway FROM network_desired_state WHERE network_id = ?")
            .bind(&net_id)
            .fetch_one(&state.pool)
            .await
            .expect("query gateway");
    assert_eq!(stored.as_deref(), Some("10.77.0.1"));

    // Explicit gateway → kept verbatim.
    let (status, body) = post_with_token(
        state.clone(),
        "/v1/networks/create",
        &token,
        r#"{"name":"tenant-explicit","cidr":"10.78.0.0/24","gateway":"10.78.0.254"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create body: {body}");
    let net_id = body["network_id"].as_str().expect("network_id").to_string();
    let stored: Option<String> =
        sqlx::query_scalar("SELECT gateway FROM network_desired_state WHERE network_id = ?")
            .bind(&net_id)
            .fetch_one(&state.pool)
            .await
            .expect("query gateway");
    assert_eq!(stored.as_deref(), Some("10.78.0.254"));

    // Explicit EMPTY gateway → an operator's deliberate L2-only choice,
    // kept empty (never silently defaulted).
    let (status, body) = post_with_token(
        state.clone(),
        "/v1/networks/create",
        &token,
        r#"{"name":"tenant-l2","cidr":"10.79.0.0/24","gateway":""}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create body: {body}");
    let net_id = body["network_id"].as_str().expect("network_id").to_string();
    let stored: Option<String> =
        sqlx::query_scalar("SELECT gateway FROM network_desired_state WHERE network_id = ?")
            .bind(&net_id)
            .fetch_one(&state.pool)
            .await
            .expect("query gateway");
    assert_eq!(stored.as_deref(), Some(""));

    // JSON null is NOT an explicit empty string — it means "no value",
    // same as absent, and defaults (pinned: pre-fix it silently
    // persisted "" instead).
    let (status, body) = post_with_token(
        state.clone(),
        "/v1/networks/create",
        &token,
        r#"{"name":"tenant-null","cidr":"10.80.0.0/24","gateway":null}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create body: {body}");
    let net_id = body["network_id"].as_str().expect("network_id").to_string();
    let stored: Option<String> =
        sqlx::query_scalar("SELECT gateway FROM network_desired_state WHERE network_id = ?")
            .bind(&net_id)
            .fetch_one(&state.pool)
            .await
            .expect("query gateway");
    assert_eq!(
        stored.as_deref(),
        Some("10.80.0.1"),
        "JSON null must default like an absent field"
    );
}

#[tokio::test]
async fn vm_create_resolves_network_by_display_name() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;

    let net_id = create_network(&state, &token, "tenant-a", "10.99.0.0/24").await;

    let (status, vm_id) = create_vm_on_network(&state, &token, "tenant-a").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "vm create by network name should be accepted"
    );
    let vm_id = vm_id.expect("vm_id in response");

    // The VM's NIC must sit on the operator's network (not an implicit
    // fallback row) and draw its IP from the operator's cidr.
    let (nic_net, nic_ip) = vm_nic(&state, &vm_id).await;
    assert_eq!(nic_net, net_id, "NIC must attach to the named network");
    assert!(
        nic_ip.starts_with("10.99."),
        "IPAM address must come from the operator's cidr, got {nic_ip}"
    );

    // No implicit network row with the name as its id may exist.
    let implicit: Option<String> =
        sqlx::query_scalar("SELECT network_id FROM networks WHERE network_id = 'tenant-a'")
            .fetch_optional(&state.pool)
            .await
            .expect("query networks");
    assert!(implicit.is_none(), "no implicit network row may be created");
}

#[tokio::test]
async fn vm_create_network_id_resolution_wins_over_name() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;

    // A network whose network_id is 'dup' (cidr 10.55.x) and an operator
    // network whose display_name is 'dup' (cidr 10.99.x): the id hit must
    // win.
    sqlx::query(
        "INSERT INTO networks (network_id, display_name, network_class, owner_id, created_at, updated_at) \
         VALUES ('dup', 'id-side', 'bridge', 'u-ops', strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'))",
    )
    .execute(&state.pool)
    .await
    .expect("seed id network");
    sqlx::query(
        "INSERT INTO network_desired_state (network_id, desired_generation, desired_status, cidr, gateway, dhcp_enabled, ipam_mode, is_default, requested_at, updated_at) \
         VALUES ('dup', 1, 'Pending', '10.55.0.0/24', '10.55.0.1', 1, 'internal', 0, strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'))",
    )
    .execute(&state.pool)
    .await
    .expect("seed id network desired state");
    let named_id = create_network(&state, &token, "dup", "10.99.0.0/24").await;

    let (status, vm_id) = create_vm_on_network(&state, &token, "dup").await;
    assert_eq!(status, StatusCode::OK);
    let vm_id = vm_id.expect("vm_id in response");

    let (nic_net, nic_ip) = vm_nic(&state, &vm_id).await;
    assert_eq!(nic_net, "dup", "network_id hit must win over display_name");
    assert!(
        nic_ip.starts_with("10.55."),
        "IPAM must come from the id-matched network's cidr, got {nic_ip}"
    );
    assert_ne!(named_id, "dup");
}

#[tokio::test]
async fn vm_create_default_still_implicit_when_no_network_named_default() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;

    let (status, vm_id) = create_vm_on_network(&state, &token, "default").await;
    assert_eq!(status, StatusCode::OK);
    let vm_id = vm_id.expect("vm_id in response");

    let (nic_net, nic_ip) = vm_nic(&state, &vm_id).await;
    assert_eq!(
        nic_net, "default",
        "default path keeps the implicit network"
    );
    assert!(
        nic_ip.starts_with("10.200."),
        "default path keeps the fallback cidr, got {nic_ip}"
    );
}

// ---------------------------------------------------------------------------
// implicit creation + collision guard
// ---------------------------------------------------------------------------

#[tokio::test]
async fn vm_create_refuses_implicit_cidr_collision() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;

    // An existing network on the fallback cidr (fleet-wide, so it is
    // usable on the placement node).
    create_network(&state, &token, "existing", "10.200.0.0/24").await;

    // An unknown reference implicitly wants the fallback cidr → refuse.
    let (status, body) = post_with_token(
        state.clone(),
        "/v1/vms/create",
        &token,
        r#"{"name":"vm-x","image_ref":"/tmp/x.img","network_id":"unknown-net"}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "implicit cidr collision must be refused, body: {body}"
    );
    let msg = body["detail"]["message"]
        .as_str()
        .or_else(|| body["message"].as_str())
        .unwrap_or_default();
    assert!(
        msg.contains("refusing to implicitly create"),
        "error must name the refusal, got: {msg}"
    );

    // Nothing may have been created — no implicit network, no VM.
    let implicit: Option<String> =
        sqlx::query_scalar("SELECT network_id FROM networks WHERE network_id = 'unknown-net'")
            .fetch_optional(&state.pool)
            .await
            .expect("query networks");
    assert!(implicit.is_none(), "no implicit network row on refusal");
    let vms: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vm_desired_state")
        .fetch_one(&state.pool)
        .await
        .expect("count vms");
    assert_eq!(vms, 0, "no VM row on refusal");
}

#[tokio::test]
async fn vm_create_implicit_network_with_distinct_cidr_still_works() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;

    // An existing network on the fallback cidr must not block an
    // implicit creation that asks for a DISTINCT cidr explicitly.
    create_network(&state, &token, "existing", "10.200.0.0/24").await;

    let (status, body) = post_with_token(
        state.clone(),
        "/v1/vms/create",
        &token,
        r#"{"name":"vm-x","image_ref":"/tmp/x.img","network_id":"fresh-net","network_cidr":"10.77.0.0/24"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let vm_id = body["vm_id"].as_str().expect("vm_id").to_string();

    let (nic_net, nic_ip) = vm_nic(&state, &vm_id).await;
    assert_eq!(nic_net, "fresh-net");
    assert!(
        nic_ip.starts_with("10.77."),
        "implicit network must honor the explicit cidr, got {nic_ip}"
    );
}

// ---------------------------------------------------------------------------
// resolution precedence + guard scoping (review follow-ups)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn vm_create_name_resolution_prefers_fleet_network_over_node_scoped() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;

    // Two networks named 'shared': a node-scoped legacy implicit row
    // (network_id 'aaa-scoped' — lexically FIRST, so a regression to a
    // plain `ORDER BY network_id` tiebreak would pick it and fail this
    // test) and a fleet-wide operator network ('zzz-fleet', cidr
    // 10.10.x). The fleet row must win the name lookup via the explicit
    // fleet-over-node-scoped precedence.
    seed_network_row(&state, "aaa-scoped", "shared", Some("n-1"), "10.20.0.0/24").await;
    seed_network_row(&state, "zzz-fleet", "shared", None, "10.10.0.0/24").await;

    let (status, vm_id) = create_vm_on_network(&state, &token, "shared").await;
    assert_eq!(status, StatusCode::OK);
    let vm_id = vm_id.expect("vm_id in response");

    let (nic_net, nic_ip) = vm_nic(&state, &vm_id).await;
    assert_eq!(
        nic_net, "zzz-fleet",
        "fleet-wide network must win over the lexically-first node-scoped row"
    );
    assert!(
        nic_ip.starts_with("10.10."),
        "IPAM must come from the fleet network's cidr, got {nic_ip}"
    );
}

#[tokio::test]
async fn vm_create_ignores_same_cidr_network_on_other_node() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;

    // A network on the fallback cidr scoped to ANOTHER node must not
    // block an implicit creation on this node: networks materialize
    // per node, so the two never share a host bridge.
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('n-other', 'h-2', 'Node 2')",
    )
    .execute(&state.pool)
    .await
    .expect("seed other node");
    seed_network_row(
        &state,
        "other-node-net",
        "elsewhere",
        Some("n-other"),
        "10.200.0.0/24",
    )
    .await;

    let (status, vm_id) = create_vm_on_network(&state, &token, "new-net").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "other-node cidr must not conflict with this node's implicit create"
    );
    let vm_id = vm_id.expect("vm_id in response");

    let (nic_net, _) = vm_nic(&state, &vm_id).await;
    assert_eq!(nic_net, "new-net");
}

#[tokio::test]
async fn vm_create_refuses_implicit_cidr_overlap_not_just_exact_match() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;

    // An existing /23 supernet CONTAINS the fallback /24: sharing a
    // subnet is the same duplicate-route hazard even though the cidr
    // strings differ. The guard must refuse.
    create_network(&state, &token, "wide", "10.200.0.0/23").await;

    let (status, body) = post_with_token(
        state.clone(),
        "/v1/vms/create",
        &token,
        r#"{"name":"vm-x","image_ref":"/tmp/x.img","network_id":"unknown-net"}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "overlapping-subnet implicit create must be refused, body: {body}"
    );
}
