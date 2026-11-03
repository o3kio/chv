//! Integration tests for `list_architectures` ownership scoping (Security
//! H5).
//!
//! These tests prove that:
//!
//! 1. Viewers see only the topologies they own plus system-owned starters
//!    (`owner_user_id IS NULL`); they DO NOT see other operators' drafts.
//! 2. Operators get the same scoping as viewers — list is for visibility,
//!    not for write authority.
//! 3. Admins see every row regardless of `owner_user_id`.
//! 4. The `include_archived` filter still works correctly when stacked on
//!    top of the new owner-scoping predicate.
//!
//! The tests bypass the seed crate (which is the production source of the
//! six starter rows with `owner_user_id = NULL`) and instead seed
//! system-owned rows directly through `TopologyRepository::create` with
//! `owner_user_id: None`. That keeps this crate's `dev-dependencies` lean
//! and exercises the exact same SQL invariant the seeder produces.
//!
//! Failure mode without the fix: every test except `archived_filter_*`
//! fails because the repository returns every row to every caller.

use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::State;
use axum::Json;
use chv_controlplane_store::{
    AlertRepository, AlertRuleRepository, ApplyRunRepository, BackupRepository,
    DesiredStateRepository, DriftReportRepository, EventRepository, ImageRepository,
    NetworkRepository, NodeRepository, NotificationOutboxRepository, ObservedStateRepository,
    OperationRepository, TopologyCreateInput, TopologyRepository,
};
use chv_controlplane_types::architecture::{ArchitectureId, ArchitectureStatus};
use chv_webui_bff::auth::{BearerToken, Claims};
use chv_webui_bff::handlers::architectures::{list_architectures, ListArchitecturesRequest};
use chv_webui_bff::mutations::MutationService;
use chv_webui_bff::{AppState, BffError};
use sqlx::sqlite::SqlitePoolOptions;

// ---------------------------------------------------------------------------
// Test scaffolding
// ---------------------------------------------------------------------------

/// MutationService stub. None of the architecture-list calls hit the
/// mutation service, so every method is unreachable in this test surface.
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
        unreachable!("mutate_vm not used in list-ownership tests")
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
        clock: Arc::new(chv_common::SystemClock),
    }
}

fn claims(sub: &str, role: &str) -> Claims {
    Claims {
        sub: sub.to_string(),
        username: format!("user-{sub}"),
        role: role.to_string(),
        exp: u64::MAX / 2,
        must_change_password: false,
        token_version: None,
    }
}

/// Insert a topology with an explicit owner (or `None` for system-owned).
/// The repository's create() stamps version_number=1 and status=draft.
async fn seed_topology(state: &AppState, name: &str, owner: Option<&str>) -> String {
    let id = ArchitectureId::new(format!("arch-{name}")).expect("valid id");
    state
        .topology_repo
        .create(TopologyCreateInput {
            id: id.clone(),
            name: name.to_string(),
            display_name: None,
            description: None,
            environment: None,
            status: ArchitectureStatus::Draft,
            owner_user_id: owner.map(str::to_string),
            design_graph_json: None,
            latest_yaml: None,
        })
        .await
        .unwrap_or_else(|e| panic!("seed {name}: {e}"));
    id.into_inner()
}

/// Seed six system-owned starter topologies with `owner_user_id = NULL`.
/// Mirrors the production seeder's owner-NULL invariant without pulling
/// in the seed crate as a dev-dependency. The names match the seeder's
/// canonical slugs so the tests fail in the same shape if the invariant
/// drifts.
async fn seed_six_starters(state: &AppState) -> Vec<String> {
    let names = [
        "starter-single-vm",
        "starter-lamp-wordpress",
        "starter-three-tier-web",
        "starter-k8s-ha",
        "starter-observability",
        "starter-k3s-edge",
    ];
    let mut ids = Vec::with_capacity(names.len());
    for n in names {
        ids.push(seed_topology(state, n, None).await);
    }
    ids
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Viewer scope: alice (Viewer) sees her own arch-A and the 6 system-owned
/// starters. She MUST NOT see arch-B, which is owned by bob.
#[tokio::test]
async fn viewer_sees_only_own_and_system_owned() {
    let state = build_state().await;
    let starter_ids = seed_six_starters(&state).await;
    let arch_a = seed_topology(&state, "arch-A", Some("u-alice")).await;
    let _arch_b = seed_topology(&state, "arch-B", Some("u-bob")).await;

    let resp = list_architectures(
        BearerToken(claims("u-alice", "viewer")),
        State(state.clone()),
        Json(ListArchitecturesRequest::default()),
    )
    .await
    .expect("alice viewer list");

    let ids: Vec<&str> = resp.0.architectures.iter().map(|a| a.id.as_str()).collect();
    assert!(
        ids.contains(&arch_a.as_str()),
        "alice must see her own arch-A; got {ids:?}"
    );
    for sid in &starter_ids {
        assert!(
            ids.contains(&sid.as_str()),
            "alice must see system starter {sid}; got {ids:?}"
        );
    }
    assert!(
        !ids.iter().any(|id| id.contains("arch-B")),
        "alice MUST NOT see bob's arch-B; got {ids:?}"
    );
    // Exact length: 1 (arch-A) + 6 starters = 7.
    assert_eq!(
        resp.0.architectures.len(),
        7,
        "alice viewer must see exactly arch-A + 6 starters"
    );
}

/// Operator scope: same shape as viewer. Owning a row does not require
/// elevated read scope; non-admins are limited to their own rows + system.
#[tokio::test]
async fn operator_sees_only_own_and_system_owned() {
    let state = build_state().await;
    let starter_ids = seed_six_starters(&state).await;
    let arch_a = seed_topology(&state, "arch-A", Some("u-alice")).await;
    let _arch_b = seed_topology(&state, "arch-B", Some("u-bob")).await;

    let resp = list_architectures(
        BearerToken(claims("u-alice", "operator")),
        State(state.clone()),
        Json(ListArchitecturesRequest::default()),
    )
    .await
    .expect("alice operator list");

    let ids: Vec<&str> = resp.0.architectures.iter().map(|a| a.id.as_str()).collect();
    assert!(ids.contains(&arch_a.as_str()), "operator alice sees arch-A");
    for sid in &starter_ids {
        assert!(
            ids.contains(&sid.as_str()),
            "operator alice sees starter {sid}"
        );
    }
    assert!(
        !ids.iter().any(|id| id.contains("arch-B")),
        "operator alice MUST NOT see bob's arch-B"
    );
    assert_eq!(resp.0.architectures.len(), 7);
}

/// Admin scope: every row, regardless of owner. arch-A, arch-B, and the
/// six starters all flow through.
#[tokio::test]
async fn admin_sees_all_architectures() {
    let state = build_state().await;
    let starter_ids = seed_six_starters(&state).await;
    let arch_a = seed_topology(&state, "arch-A", Some("u-alice")).await;
    let arch_b = seed_topology(&state, "arch-B", Some("u-bob")).await;

    let resp = list_architectures(
        BearerToken(claims("u-admin", "admin")),
        State(state.clone()),
        Json(ListArchitecturesRequest::default()),
    )
    .await
    .expect("admin list");

    let ids: Vec<&str> = resp.0.architectures.iter().map(|a| a.id.as_str()).collect();
    assert!(ids.contains(&arch_a.as_str()), "admin sees arch-A");
    assert!(ids.contains(&arch_b.as_str()), "admin sees arch-B");
    for sid in &starter_ids {
        assert!(ids.contains(&sid.as_str()), "admin sees starter {sid}");
    }
    // 2 user-owned + 6 system-owned starters = 8.
    assert_eq!(
        resp.0.architectures.len(),
        8,
        "admin must see all rows from every owner"
    );
}

/// The include_archived filter still composes correctly with ownership
/// scoping. Archive alice's arch-A, then list with default
/// include_archived=false: alice must NOT see her archived row, but the
/// six starters must still be present.
#[tokio::test]
async fn archived_filter_still_works() {
    let state = build_state().await;
    let starter_ids = seed_six_starters(&state).await;
    let arch_a = seed_topology(&state, "arch-A", Some("u-alice")).await;

    let arch_id = ArchitectureId::new(arch_a.clone()).unwrap();
    state
        .topology_repo
        .archive(&arch_id, 1, None)
        .await
        .expect("archive arch-A");

    // Default scope (include_archived = false): arch-A is hidden.
    let resp_default = list_architectures(
        BearerToken(claims("u-alice", "viewer")),
        State(state.clone()),
        Json(ListArchitecturesRequest::default()),
    )
    .await
    .expect("alice list default");
    let ids_default: Vec<&str> = resp_default
        .0
        .architectures
        .iter()
        .map(|a| a.id.as_str())
        .collect();
    assert!(
        !ids_default.contains(&arch_a.as_str()),
        "include_archived=false must hide alice's archived arch-A"
    );
    for sid in &starter_ids {
        assert!(
            ids_default.contains(&sid.as_str()),
            "starter {sid} still visible after arch-A archive"
        );
    }
    assert_eq!(
        resp_default.0.architectures.len(),
        6,
        "default scope: only the 6 starters remain"
    );

    // Explicit include_archived = true: arch-A reappears for alice.
    let resp_with = list_architectures(
        BearerToken(claims("u-alice", "viewer")),
        State(state.clone()),
        Json(ListArchitecturesRequest {
            include_archived: true,
        }),
    )
    .await
    .expect("alice list include_archived");
    let ids_with: Vec<&str> = resp_with
        .0
        .architectures
        .iter()
        .map(|a| a.id.as_str())
        .collect();
    assert!(
        ids_with.contains(&arch_a.as_str()),
        "include_archived=true must surface alice's archived arch-A"
    );
    assert_eq!(
        resp_with.0.architectures.len(),
        7,
        "with archived: arch-A + 6 starters"
    );
}
