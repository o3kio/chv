//! Integration tests for the Phase 0 Architecture Designer BFF surface.
//!
//! These tests exercise the handlers directly (no HTTP layer) against an
//! in-memory SQLite database wired to a real repository instance. The BFF
//! crate uses this same pattern in its embedded `auth.rs` tests, so the
//! authentication / authorization paths are covered here without spinning
//! up an axum server.

use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::State;
use axum::Json;
use chv_controlplane_store::{
    AlertRepository, AlertRuleRepository, ApplyRunRepository, BackupRepository,
    DesiredStateRepository, DriftReportRepository, EventRepository, ImageRepository,
    NetworkRepository, NodeRepository, NotificationOutboxRepository, ObservedStateRepository,
    OperationRepository, TopologyRepository,
};
use chv_webui_bff::auth::{BearerToken, Claims};
use chv_webui_bff::handlers::architectures::{
    archive_architecture, check_fleet_architecture, create_architecture,
    generate_architecture_yaml, get_architecture, import_yaml_architecture,
    list_architecture_versions, list_architectures, update_architecture, validate_architecture,
    validate_architecture_yaml, ArchiveArchitectureRequest, CheckFleetRequest,
    CreateArchitectureRequest, GenerateYamlRequest, GetArchitectureRequest, ImportYamlRequest,
    ListArchitecturesRequest, UpdateArchitectureRequest, ValidateArchitectureRequest,
    ValidateArchitectureYamlRequest, ValidationStatusKind,
};
use chv_webui_bff::mutations::MutationService;
use chv_webui_bff::{AppState, BffError};
use serde_json::json;
use sqlx::sqlite::SqlitePoolOptions;

// ---------------------------------------------------------------------------
// Test scaffolding
// ---------------------------------------------------------------------------

/// MutationService stub — none of the architecture handlers call into the
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
        unreachable!("mutate_vm not used in architecture tests")
    }
    async fn migrate_vm(
        &self,
        _vm_id: String,
        _target_node_id: String,
        _pause_first: bool,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        unreachable!("migrate_vm not used in architecture tests")
    }
    async fn snapshot_vm(
        &self,
        _vm_id: String,
        _destination: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        unreachable!("snapshot_vm not used in architecture tests")
    }
    async fn restore_snapshot(
        &self,
        _vm_id: String,
        _source: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        unreachable!("restore_snapshot not used in architecture tests")
    }
    async fn mutate_node(
        &self,
        _node_id: String,
        _action: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateNodeResponse, BffError> {
        unreachable!("mutate_node not used in architecture tests")
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
        unreachable!("mutate_volume not used in architecture tests")
    }
    async fn snapshot_volume(
        &self,
        _volume_id: String,
        _snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!("snapshot_volume not used in architecture tests")
    }
    async fn restore_volume_snapshot(
        &self,
        _volume_id: String,
        _snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!("restore_volume_snapshot not used in architecture tests")
    }
    async fn delete_volume_snapshot(
        &self,
        _volume_id: String,
        _snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!("delete_volume_snapshot not used in architecture tests")
    }
    async fn clone_volume(
        &self,
        _source_volume_id: String,
        _target_volume_id: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        unreachable!("clone_volume not used in architecture tests")
    }
    async fn mutate_network(
        &self,
        _network_id: String,
        _action: String,
        _force: bool,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateNetworkResponse, BffError> {
        unreachable!("mutate_network not used in architecture tests")
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

fn claims_for(role: &str) -> Claims {
    Claims {
        sub: "u-tester".to_string(),
        username: "tester".to_string(),
        role: role.to_string(),
        exp: u64::MAX / 2,
        must_change_password: false,
        token_version: None,
    }
}

fn err_status(e: &BffError) -> u16 {
    match e {
        BffError::BadRequest(_) => 400,
        BffError::Unauthorized(_) => 401,
        BffError::Forbidden(_) => 403,
        BffError::NotFound(_) => 404,
        BffError::Conflict(_) => 409,
        BffError::TooManyRequests(_) => 429,
        BffError::MonitoringUnavailable(_) => 503,
        BffError::MonitoringQuery { .. } => 400,
        BffError::Internal(_) => 500,
        BffError::NotImplemented(_) => 501,
        BffError::QuotaExceeded { .. } => 422,
        BffError::GraphEmpty => 422,
        BffError::PlanExpired { .. } => 409,
        BffError::PlanNotDiscardable { .. } => 409,
        BffError::MissingConfirmation { .. } => 400,
        BffError::WarningsNotAcknowledged { .. } => 400,
        BffError::PlanNotApplicable { .. } => 409,
        BffError::ProductionRequiresAdmin { .. } => 403,
        BffError::PlanModeMismatch { .. } => 400,
        BffError::InvalidResourceName { .. } => 400,
        BffError::DriftCheckFailed { .. } => 502,
        BffError::NetboxNotConfigured { .. } => 404,
        BffError::NetboxNotConfiguredPrecondition { .. } => 400,
        BffError::NetboxNotApplied { .. } => 400,
        BffError::NetboxHttpsRequired => 400,
        BffError::NetboxTokenMissing { .. } => 400,
        BffError::NetboxRunActive { .. } => 409,
        BffError::ProjectionRunNotRetryable { .. } => 409,
        BffError::NetboxStaleVersion { .. } => 409,
        BffError::NetboxUnreachable { .. } => 502,
        BffError::NetboxAuthFailed { .. } => 502,
    }
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn crud_lifecycle_create_list_get_update_archive() {
    let state = build_state().await;

    // 1. Create — production-environment topologies require Admin since
    //    Phase 5 reviewer F2 (operator-writable-label bypass).
    let create = create_architecture(
        BearerToken(claims_for("admin")),
        State(state.clone()),
        Json(CreateArchitectureRequest {
            name: "customer-a-prod".to_string(),
            description: Some("Customer A production".to_string()),
            environment: Some("production".to_string()),
            display_name: Some("Customer A — Production".to_string()),
            design_graph_json: Some(r#"{"nodes":[],"edges":[]}"#.to_string()),
            latest_yaml: None,
        }),
    )
    .await
    .expect("create should succeed");
    let created = create.0.architecture;
    assert_eq!(created.name, "customer-a-prod");
    assert_eq!(created.version_number, 1);
    let arch_id = created.id.clone();

    // 2. List default scope (excludes archived) — should include it
    let listed = list_architectures(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(ListArchitecturesRequest::default()),
    )
    .await
    .expect("list should succeed");
    assert_eq!(listed.0.architectures.len(), 1);
    assert_eq!(listed.0.architectures[0].id, arch_id);

    // 3. Get includes design_graph_json
    let got = get_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(GetArchitectureRequest {
            id: arch_id.clone(),
        }),
    )
    .await
    .expect("get should succeed");
    assert_eq!(got.0.architecture.id, arch_id);
    assert_eq!(
        got.0.design_graph_json.as_deref(),
        Some(r#"{"nodes":[],"edges":[]}"#)
    );

    // 4. A non-admin may not mutate a production-tagged row — even when the
    //    request carries `environment: null` (Security F7: the persisted tag
    //    fires the guard, not the request's environment field).
    let prod_guard_err = update_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(UpdateArchitectureRequest {
            id: arch_id.clone(),
            expected_version: 1,
            display_name: Some("Customer A — Prod (renamed)".to_string()),
            description: None,
            environment: None,
            design_graph_json: Some(r#"{"nodes":[1],"edges":[]}"#.to_string()),
            latest_yaml: None,
            latest_version_id: None,
        }),
    )
    .await
    .expect_err("operator update on production-tagged row must be blocked");
    assert_eq!(
        err_status(&prod_guard_err),
        403,
        "production-tagged update by operator => 403"
    );
    assert!(
        matches!(prod_guard_err, BffError::ProductionRequiresAdmin { .. }),
        "expected ProductionRequiresAdmin, got {prod_guard_err:?}"
    );

    // 5. Admin update bumps version_number.
    let updated = update_architecture(
        BearerToken(claims_for("admin")),
        State(state.clone()),
        Json(UpdateArchitectureRequest {
            id: arch_id.clone(),
            expected_version: 1,
            display_name: Some("Customer A — Prod (renamed)".to_string()),
            description: None,
            environment: None,
            design_graph_json: Some(r#"{"nodes":[1],"edges":[]}"#.to_string()),
            latest_yaml: None,
            latest_version_id: None,
        }),
    )
    .await
    .expect("admin update should succeed");
    assert_eq!(updated.0.architecture.version_number, 2);
    assert_eq!(
        updated.0.architecture.display_name.as_deref(),
        Some("Customer A — Prod (renamed)")
    );

    // 6. Archive — supplies the bumped version_number from the update.
    let _ = archive_architecture(
        BearerToken(claims_for("admin")),
        State(state.clone()),
        Json(ArchiveArchitectureRequest {
            id: arch_id.clone(),
            expected_version: updated.0.architecture.version_number,
        }),
    )
    .await
    .expect("archive should succeed");

    // 7. List default excludes archived
    let listed_again = list_architectures(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(ListArchitecturesRequest::default()),
    )
    .await
    .expect("list should succeed");
    assert!(
        listed_again.0.architectures.is_empty(),
        "archived topology must be hidden from default list"
    );

    // 8. List include_archived=true surfaces it
    let listed_arch = list_architectures(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(ListArchitecturesRequest {
            include_archived: true,
        }),
    )
    .await
    .expect("list-with-archived should succeed");
    assert_eq!(listed_arch.0.architectures.len(), 1);
    assert!(listed_arch.0.architectures[0].archived_at.is_some());
}

// ---------------------------------------------------------------------------
// Optimistic concurrency
// ---------------------------------------------------------------------------

#[tokio::test]
async fn update_with_stale_version_returns_conflict() {
    let state = build_state().await;

    let created = create_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(CreateArchitectureRequest {
            name: "stale-version-test".to_string(),
            description: None,
            environment: None,
            display_name: None,
            design_graph_json: None,
            latest_yaml: None,
        }),
    )
    .await
    .expect("create should succeed")
    .0
    .architecture;
    let id = created.id.clone();
    assert_eq!(created.version_number, 1);

    // First update succeeds, bumps version to 2
    let _ = update_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(UpdateArchitectureRequest {
            id: id.clone(),
            expected_version: 1,
            display_name: Some("first".to_string()),
            description: None,
            environment: None,
            design_graph_json: None,
            latest_yaml: None,
            latest_version_id: None,
        }),
    )
    .await
    .expect("first update should succeed");

    // Second update with the now-stale expected_version=1 must 409
    let err = update_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(UpdateArchitectureRequest {
            id: id.clone(),
            expected_version: 1,
            display_name: Some("second".to_string()),
            description: None,
            environment: None,
            design_graph_json: None,
            latest_yaml: None,
            latest_version_id: None,
        }),
    )
    .await
    .expect_err("stale-version update must fail");
    assert_eq!(err_status(&err), 409, "stale version => 409 Conflict");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("stale version")
            && msg.contains("client sent 1")
            && msg.contains("current is 2"),
        "conflict message should reveal versions; got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Not found
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_unknown_id_returns_not_found() {
    let state = build_state().await;
    let v = claims_for("viewer");

    let err = get_architecture(
        BearerToken(v),
        State(state),
        Json(GetArchitectureRequest {
            id: "does-not-exist".to_string(),
        }),
    )
    .await
    .expect_err("unknown id must error");
    assert_eq!(err_status(&err), 404);
}

// ---------------------------------------------------------------------------
// AuthZ
// ---------------------------------------------------------------------------

#[tokio::test]
async fn viewer_cannot_create_architecture() {
    let state = build_state().await;
    let v = claims_for("viewer");

    let err = create_architecture(
        BearerToken(v),
        State(state),
        Json(CreateArchitectureRequest {
            name: "viewer-cannot".to_string(),
            description: None,
            environment: None,
            display_name: None,
            design_graph_json: None,
            latest_yaml: None,
        }),
    )
    .await
    .expect_err("viewer must not be able to create");
    assert_eq!(err_status(&err), 403);
}

#[tokio::test]
async fn viewer_cannot_update_architecture() {
    let state = build_state().await;
    let created = create_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(CreateArchitectureRequest {
            name: "viewer-update-test".to_string(),
            description: None,
            environment: None,
            display_name: None,
            design_graph_json: None,
            latest_yaml: None,
        }),
    )
    .await
    .expect("create")
    .0
    .architecture;

    let v = claims_for("viewer");
    let err = update_architecture(
        BearerToken(v),
        State(state.clone()),
        Json(UpdateArchitectureRequest {
            id: created.id.clone(),
            expected_version: 1,
            display_name: Some("nope".to_string()),
            description: None,
            environment: None,
            design_graph_json: None,
            latest_yaml: None,
            latest_version_id: None,
        }),
    )
    .await
    .expect_err("viewer update must be forbidden");
    assert_eq!(err_status(&err), 403);
}

#[tokio::test]
async fn viewer_cannot_archive_architecture() {
    let state = build_state().await;
    let created = create_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(CreateArchitectureRequest {
            name: "viewer-archive-test".to_string(),
            description: None,
            environment: None,
            display_name: None,
            design_graph_json: None,
            latest_yaml: None,
        }),
    )
    .await
    .expect("create")
    .0
    .architecture;

    let v = claims_for("viewer");
    let err = archive_architecture(
        BearerToken(v),
        State(state),
        Json(ArchiveArchitectureRequest {
            id: created.id.clone(),
            expected_version: 1,
        }),
    )
    .await
    .expect_err("viewer archive must be forbidden");
    assert_eq!(err_status(&err), 403);
}

// Note on 401 / unauthenticated:
//
// 401 enforcement lives in the `BearerToken` extractor and the role-gating
// middleware in `crate::auth`, neither of which can be exercised by calling
// handlers as plain functions. The middleware paths are covered by
// `crate::auth` tests in the BFF crate, and the role-required arms are
// covered by the `viewer_cannot_*` tests above. The architecture handlers
// themselves are gate-only behind those layers, so a request without a
// bearer token never reaches them.

// ---------------------------------------------------------------------------
// Stub endpoints
// ---------------------------------------------------------------------------

#[tokio::test]
async fn validate_yaml_endpoint_returns_clean_result_for_canonical_example() {
    let state = build_state().await;
    let op = claims_for("operator");

    let yaml = include_str!("../../../docs/examples/chvarchitecture-example.yaml").to_string();
    let resp = validate_architecture_yaml(
        BearerToken(op),
        State(state),
        Json(ValidateArchitectureYamlRequest { yaml }),
    )
    .await
    .expect("validate-yaml should succeed");
    assert_eq!(
        resp.0.result.summary.errors, 0,
        "{:#?}",
        resp.0.result.findings
    );
}

#[tokio::test]
async fn validate_yaml_endpoint_returns_error_for_invalid_cidr() {
    let state = build_state().await;
    let op = claims_for("operator");

    let yaml = r#"
apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: bad-cidr
networks:
  - name: bad
    type: bridge
    cidr: 999.0.0.0/24
"#
    .to_string();
    let resp = validate_architecture_yaml(
        BearerToken(op),
        State(state),
        Json(ValidateArchitectureYamlRequest { yaml }),
    )
    .await
    .expect("validate-yaml should succeed");
    let invalid_cidr_findings: Vec<_> = resp
        .0
        .result
        .findings
        .iter()
        .filter(|f| f.code.as_ref() == "INVALID_CIDR")
        .collect();
    assert_eq!(
        invalid_cidr_findings.len(),
        1,
        "{:#?}",
        resp.0.result.findings
    );
}

#[tokio::test]
async fn validate_yaml_forbids_viewer() {
    let state = build_state().await;
    let v = claims_for("viewer");
    let err = validate_architecture_yaml(
        BearerToken(v),
        State(state),
        Json(ValidateArchitectureYamlRequest {
            yaml: "x".to_string(),
        }),
    )
    .await
    .expect_err("viewer must be forbidden");
    assert_eq!(err_status(&err), 403);
}

#[tokio::test]
async fn validate_persistent_topology_writes_validation_status() {
    let state = build_state().await;
    let op = claims_for("operator");

    let yaml = include_str!("../../../docs/examples/chvarchitecture-example.yaml").to_string();
    let created = create_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(CreateArchitectureRequest {
            name: "validate-status-test".to_string(),
            description: None,
            environment: None,
            display_name: None,
            design_graph_json: None,
            latest_yaml: Some(yaml),
        }),
    )
    .await
    .expect("create")
    .0
    .architecture;

    let _ = validate_architecture(
        BearerToken(op),
        State(state.clone()),
        Json(ValidateArchitectureRequest {
            id: created.id.clone(),
        }),
    )
    .await
    .expect("validate should succeed");

    let got = get_architecture(
        BearerToken(claims_for("operator")),
        State(state),
        Json(GetArchitectureRequest {
            id: created.id.clone(),
        }),
    )
    .await
    .expect("get");
    assert_eq!(
        got.0.architecture.last_validation_status,
        Some(chv_controlplane_types::architecture::ValidationStatus::Passed),
    );
    // version_number was bumped by set_validation_status
    assert!(got.0.architecture.version_number >= 2);
}

#[tokio::test]
async fn validate_persistent_topology_records_failed_for_bad_yaml() {
    let state = build_state().await;
    let op = claims_for("operator");

    let bad_yaml = r#"
apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: bad-cidr
networks:
  - name: bad
    type: bridge
    cidr: 999.0.0.0/24
"#
    .to_string();
    let created = create_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(CreateArchitectureRequest {
            name: "validate-status-bad".to_string(),
            description: None,
            environment: None,
            display_name: None,
            design_graph_json: None,
            latest_yaml: Some(bad_yaml),
        }),
    )
    .await
    .expect("create")
    .0
    .architecture;

    let resp = validate_architecture(
        BearerToken(op),
        State(state.clone()),
        Json(ValidateArchitectureRequest {
            id: created.id.clone(),
        }),
    )
    .await
    .expect("validate runs even when invalid");
    assert!(resp.0.result.summary.errors >= 1);

    let got = get_architecture(
        BearerToken(claims_for("operator")),
        State(state),
        Json(GetArchitectureRequest { id: created.id }),
    )
    .await
    .expect("get");
    assert_eq!(
        got.0.architecture.last_validation_status,
        Some(chv_controlplane_types::architecture::ValidationStatus::Failed),
    );
}

#[tokio::test]
async fn viewer_cannot_validate_topology() {
    let state = build_state().await;
    let v = claims_for("viewer");
    let err = validate_architecture(
        BearerToken(v),
        State(state),
        Json(ValidateArchitectureRequest {
            id: "any".to_string(),
        }),
    )
    .await
    .expect_err("viewer must be forbidden");
    assert_eq!(err_status(&err), 403);
}

#[tokio::test]
async fn check_fleet_returns_insufficient_memory_when_node_too_small() {
    let state = build_state().await;

    // Seed one node with 16 GiB RAM and 4 cores; mark it schedulable.
    sqlx::query(
        r#"INSERT INTO nodes (node_id, hostname, display_name)
           VALUES ('n1', 'host-1', 'host-1')"#,
    )
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO node_inventory (node_id, architecture, cpu_count, memory_bytes)
           VALUES ('n1', 'x86_64', 4, ?1)"#,
    )
    .bind(16i64 * 1024 * 1024 * 1024)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO node_desired_state
           (node_id, desired_generation, desired_state, scheduling_paused)
           VALUES ('n1', 1, 'Running', 0)"#,
    )
    .execute(&state.pool)
    .await
    .unwrap();

    // Architecture with one instance requesting 32 GiB on host-1.
    let yaml = r#"apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: too-big
templates:
  - name: large
    image: ubuntu-24.04
    cpu: 2
    memory_mb: 32768
instances:
  - name: app
    template: large
    placement:
      server: host-1
"#
    .to_string();

    let created = create_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(CreateArchitectureRequest {
            name: "fleet-mem-test".to_string(),
            description: None,
            environment: None,
            display_name: None,
            design_graph_json: None,
            latest_yaml: Some(yaml),
        }),
    )
    .await
    .expect("create")
    .0
    .architecture;

    let resp = check_fleet_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(CheckFleetRequest {
            id: created.id.clone(),
        }),
    )
    .await
    .expect("check-fleet should succeed");

    let body = resp.0;
    assert_eq!(body.status, ValidationStatusKind::Invalid);
    let mem_findings: Vec<_> = body
        .findings
        .iter()
        .filter(|f| f.code.as_ref() == "INSUFFICIENT_MEMORY")
        .collect();
    assert_eq!(
        mem_findings.len(),
        1,
        "expected exactly one INSUFFICIENT_MEMORY finding, got: {:#?}",
        body.findings
    );
    let f = mem_findings[0];
    assert_eq!(f.resource_ref.as_deref(), Some("instance/app"));
    assert!(
        f.path
            .as_deref()
            .map(|p| p.contains("instances["))
            .unwrap_or(false),
        "path should reference the instances[] index, got {:?}",
        f.path
    );
    assert!(!body.inventory_snapshot_id.is_empty());
    assert!(!body.checked_at.is_empty());

    // last_fleet_check_status persisted as Failed.
    let got = get_architecture(
        BearerToken(claims_for("operator")),
        State(state),
        Json(GetArchitectureRequest { id: created.id }),
    )
    .await
    .expect("get");
    assert_eq!(
        got.0.architecture.last_fleet_check_status,
        Some(chv_controlplane_types::architecture::FleetCheckStatus::Failed),
    );
}

#[tokio::test]
async fn check_fleet_happy_path_returns_valid_with_no_findings() {
    let state = build_state().await;

    sqlx::query(
        r#"INSERT INTO nodes (node_id, hostname, display_name)
           VALUES ('n1', 'host-1', 'host-1')"#,
    )
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO node_inventory (node_id, architecture, cpu_count, memory_bytes)
           VALUES ('n1', 'x86_64', 16, ?1)"#,
    )
    .bind(64i64 * 1024 * 1024 * 1024)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO node_desired_state
           (node_id, desired_generation, desired_state, scheduling_paused)
           VALUES ('n1', 1, 'Running', 0)"#,
    )
    .execute(&state.pool)
    .await
    .unwrap();

    let yaml = r#"apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: happy
templates:
  - name: small
    image: ubuntu-24.04
    cpu: 1
    memory_mb: 1024
instances:
  - name: app
    template: small
    placement:
      server: host-1
"#
    .to_string();

    let created = create_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(CreateArchitectureRequest {
            name: "fleet-happy".to_string(),
            description: None,
            environment: None,
            display_name: None,
            design_graph_json: None,
            latest_yaml: Some(yaml),
        }),
    )
    .await
    .expect("create")
    .0
    .architecture;

    let resp = check_fleet_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(CheckFleetRequest { id: created.id }),
    )
    .await
    .expect("check-fleet should succeed");

    assert_eq!(resp.0.status, ValidationStatusKind::Valid);
    assert!(
        resp.0.findings.is_empty(),
        "expected no findings, got: {:#?}",
        resp.0.findings
    );
}

#[tokio::test]
async fn check_fleet_forbids_viewer() {
    let state = build_state().await;
    let v = claims_for("viewer");
    let err = check_fleet_architecture(
        BearerToken(v),
        State(state),
        Json(CheckFleetRequest {
            id: "any".to_string(),
        }),
    )
    .await
    .expect_err("viewer must be forbidden");
    assert_eq!(err_status(&err), 403);
}

#[tokio::test]
async fn check_fleet_returns_400_when_topology_has_no_yaml() {
    let state = build_state().await;
    let created = create_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(CreateArchitectureRequest {
            name: "fleet-no-yaml".to_string(),
            description: None,
            environment: None,
            display_name: None,
            design_graph_json: None,
            latest_yaml: None,
        }),
    )
    .await
    .expect("create")
    .0
    .architecture;

    let err = check_fleet_architecture(
        BearerToken(claims_for("operator")),
        State(state),
        Json(CheckFleetRequest { id: created.id }),
    )
    .await
    .expect_err("missing yaml must 400");
    assert_eq!(err_status(&err), 400);
}

#[tokio::test]
async fn generate_yaml_returns_persisted_yaml_when_present() {
    let state = build_state().await;
    let yaml_body =
        "apiVersion: chv.kubedo.io/v1alpha1\nkind: CHVArchitecture\nmetadata:\n  name: g1\n"
            .to_string();
    let created = create_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(CreateArchitectureRequest {
            name: "gen-yaml-test".to_string(),
            description: None,
            environment: None,
            display_name: None,
            design_graph_json: None,
            latest_yaml: Some(yaml_body.clone()),
        }),
    )
    .await
    .expect("create")
    .0
    .architecture;

    let resp = generate_architecture_yaml(
        BearerToken(claims_for("operator")),
        State(state),
        Json(GenerateYamlRequest {
            id: created.id.clone(),
        }),
    )
    .await
    .expect("generate-yaml should succeed");
    assert_eq!(resp.0.yaml, yaml_body);
}

#[tokio::test]
async fn generate_yaml_returns_422_when_topology_has_empty_graph() {
    let state = build_state().await;
    let created = create_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(CreateArchitectureRequest {
            name: "gen-yaml-empty".to_string(),
            description: None,
            environment: None,
            display_name: None,
            design_graph_json: None,
            latest_yaml: None,
        }),
    )
    .await
    .expect("create")
    .0
    .architecture;

    let err = generate_architecture_yaml(
        BearerToken(claims_for("operator")),
        State(state),
        Json(GenerateYamlRequest { id: created.id }),
    )
    .await
    .expect_err("empty graph must 422");
    assert_eq!(err_status(&err), 422);
    assert!(matches!(err, BffError::GraphEmpty));
}

#[tokio::test]
async fn import_yaml_endpoint_persists_yaml_and_validation_status() {
    let state = build_state().await;
    let created = create_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(CreateArchitectureRequest {
            name: "import-yaml-test".to_string(),
            description: None,
            environment: None,
            display_name: None,
            design_graph_json: None,
            latest_yaml: None,
        }),
    )
    .await
    .expect("create")
    .0
    .architecture;

    let yaml = include_str!("../../../docs/examples/chvarchitecture-example.yaml").to_string();
    let resp = import_yaml_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(ImportYamlRequest {
            id: created.id.clone(),
            yaml: yaml.clone(),
        }),
    )
    .await
    .expect("import-yaml should succeed");
    assert_eq!(resp.0.result.summary.errors, 0);

    let got = get_architecture(
        BearerToken(claims_for("operator")),
        State(state),
        Json(GetArchitectureRequest {
            id: created.id.clone(),
        }),
    )
    .await
    .expect("get");
    assert_eq!(got.0.latest_yaml.as_deref(), Some(yaml.as_str()));
    assert_eq!(
        got.0.architecture.last_validation_status,
        Some(chv_controlplane_types::architecture::ValidationStatus::Passed),
    );
}

#[tokio::test]
async fn import_yaml_persists_invalid_yaml_with_failed_status() {
    let state = build_state().await;
    let created = create_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(CreateArchitectureRequest {
            name: "import-yaml-bad".to_string(),
            description: None,
            environment: None,
            display_name: None,
            design_graph_json: None,
            latest_yaml: None,
        }),
    )
    .await
    .expect("create")
    .0
    .architecture;

    let bad_yaml = r#"
apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: bad
networks:
  - name: x
    type: bridge
    cidr: not-a-cidr
"#
    .to_string();
    let resp = import_yaml_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(ImportYamlRequest {
            id: created.id.clone(),
            yaml: bad_yaml.clone(),
        }),
    )
    .await
    .expect("import succeeds even when validation fails");
    assert!(resp.0.result.summary.errors >= 1);

    let got = get_architecture(
        BearerToken(claims_for("operator")),
        State(state),
        Json(GetArchitectureRequest {
            id: created.id.clone(),
        }),
    )
    .await
    .expect("get");
    assert_eq!(got.0.latest_yaml.as_deref(), Some(bad_yaml.as_str()));
    assert_eq!(
        got.0.architecture.last_validation_status,
        Some(chv_controlplane_types::architecture::ValidationStatus::Failed),
    );
}

#[tokio::test]
async fn import_yaml_forbids_viewer() {
    let state = build_state().await;
    let v = claims_for("viewer");
    let err = import_yaml_architecture(
        BearerToken(v),
        State(state),
        Json(ImportYamlRequest {
            id: "x".to_string(),
            yaml: "y".to_string(),
        }),
    )
    .await
    .expect_err("viewer must be forbidden");
    assert_eq!(err_status(&err), 403);
}

// Plan / destroy-plan / discard-plan handlers are exercised in
// `tests/architectures_plan.rs` against an injected `ManualClock`. The
// Phase-0 stub tests that previously lived here have been removed; the
// real surface no longer returns 501.

// Apply / destroy handlers are exercised in `tests/architectures_apply.rs`.
// The Phase-0 stub tests that previously lived here are gone — apply and
// destroy now require a generated plan and matching confirmation, so the
// stub-shape JSON inputs no longer round-trip.

// Drift / runs/list handlers are exercised in `tests/architectures_drift.rs`
// (Phase 6) and the bottom of `tests/architectures_apply.rs` (list_runs).
// The Phase-0 stub tests that previously lived here have been removed;
// the real handlers no longer return 501 — drift returns a typed
// `DriftResponse`, runs/list returns `ListApplyRunsResponse`. See
// `architectures_drift.rs` for the new contract assertions.

#[tokio::test]
async fn versions_list_stub_returns_501_for_viewer() {
    let state = build_state().await;
    let v = claims_for("viewer");
    let err = list_architecture_versions(BearerToken(v), State(state), Json(json!({})))
        .await
        .expect_err("stub must error");
    assert_eq!(err_status(&err), 501);
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_with_blank_name_returns_400() {
    let state = build_state().await;
    let op = claims_for("operator");
    let err = create_architecture(
        BearerToken(op),
        State(state),
        Json(CreateArchitectureRequest {
            name: "   ".to_string(),
            description: None,
            environment: None,
            display_name: None,
            design_graph_json: None,
            latest_yaml: None,
        }),
    )
    .await
    .expect_err("blank name must 400");
    assert_eq!(err_status(&err), 400);
}

#[tokio::test]
async fn get_with_blank_id_returns_400() {
    let state = build_state().await;
    let v = claims_for("viewer");
    let err = get_architecture(
        BearerToken(v),
        State(state),
        Json(GetArchitectureRequest { id: "".to_string() }),
    )
    .await
    .expect_err("blank id must 400");
    assert_eq!(err_status(&err), 400);
}

// ---------------------------------------------------------------------------
// Optimistic-concurrency on archive
// ---------------------------------------------------------------------------

#[tokio::test]
async fn archive_with_stale_expected_version_returns_409() {
    let state = build_state().await;

    let created = create_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(CreateArchitectureRequest {
            name: "stale-archive".to_string(),
            description: None,
            environment: None,
            display_name: None,
            design_graph_json: None,
            latest_yaml: None,
        }),
    )
    .await
    .expect("create")
    .0
    .architecture;
    assert_eq!(created.version_number, 1);

    // Bump the version via update; client B still holds v=1.
    let _ = update_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(UpdateArchitectureRequest {
            id: created.id.clone(),
            expected_version: 1,
            display_name: Some("renamed".to_string()),
            description: None,
            environment: None,
            design_graph_json: None,
            latest_yaml: None,
            latest_version_id: None,
        }),
    )
    .await
    .expect("update");

    // Stale archive must surface as 409 with the documented version banner.
    let err = archive_architecture(
        BearerToken(claims_for("operator")),
        State(state),
        Json(ArchiveArchitectureRequest {
            id: created.id.clone(),
            expected_version: 1,
        }),
    )
    .await
    .expect_err("stale archive must 409");
    assert_eq!(err_status(&err), 409);
    let msg = format!("{err:?}");
    assert!(
        msg.contains("stale version")
            && msg.contains("client sent 1")
            && msg.contains("current is 2"),
        "conflict message should reveal versions; got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Duplicate name → 409 (UNIQUE violation surfaces as Conflict, not 500)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_duplicate_name_returns_409() {
    let state = build_state().await;

    let _ = create_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(CreateArchitectureRequest {
            name: "dup-name".to_string(),
            description: None,
            environment: None,
            display_name: None,
            design_graph_json: None,
            latest_yaml: None,
        }),
    )
    .await
    .expect("first create");

    let err = create_architecture(
        BearerToken(claims_for("operator")),
        State(state),
        Json(CreateArchitectureRequest {
            name: "dup-name".to_string(),
            description: None,
            environment: None,
            display_name: None,
            design_graph_json: None,
            latest_yaml: None,
        }),
    )
    .await
    .expect_err("duplicate name must fail");
    assert_eq!(err_status(&err), 409, "duplicate name => 409 Conflict");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("dup-name") && msg.contains("name already exists"),
        "conflict message should name the offending field; got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// No-op update keeps version stable (M1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn update_with_no_field_changes_does_not_bump_version() {
    let state = build_state().await;

    let created = create_architecture(
        BearerToken(claims_for("operator")),
        State(state.clone()),
        Json(CreateArchitectureRequest {
            name: "noop-update".to_string(),
            description: Some("d".to_string()),
            environment: Some("test".to_string()),
            display_name: None,
            design_graph_json: None,
            latest_yaml: None,
        }),
    )
    .await
    .expect("create")
    .0
    .architecture;
    assert_eq!(created.version_number, 1);

    let result = update_architecture(
        BearerToken(claims_for("operator")),
        State(state),
        Json(UpdateArchitectureRequest {
            id: created.id.clone(),
            expected_version: 1,
            display_name: None,
            description: None,
            environment: None,
            design_graph_json: None,
            latest_yaml: None,
            latest_version_id: None,
        }),
    )
    .await
    .expect("noop update should succeed");
    assert_eq!(
        result.0.architecture.version_number, 1,
        "no field changes must NOT bump the version"
    );
}
