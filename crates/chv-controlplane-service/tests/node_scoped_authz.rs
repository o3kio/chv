//! Regression tests for #651: every node-scoped RPC must bind its wire
//! `node_id` to the mTLS peer identity.
//!
//! Before this change, `LifecycleService`, the reconcile-apply handlers, and
//! `ReportMigrationProgress` trusted the asserted `node_id` without comparing
//! it to the caller's certificate. A compromised agent holding a valid cert for
//! node A could therefore mutate node B's lifecycle/reconcile state or write
//! migration progress into node B's migration row.
//!
//! These tests deliberately drive the gRPC shim in `server.rs` (the layer that
//! owns peer-identity enforcement), inserting a synthetic [`PeerNodeId`] — the
//! same extension the production `PeerIdentityInterceptor` inserts.

use chv_controlplane_service::{
    LifecycleServer, LifecycleServiceImplementation, PeerNodeId, TelemetryServer,
    TelemetryServiceImplementation,
};
use chv_controlplane_store::{
    test_util::create_test_pool, AlertRepository, DesiredStateRepository, EventRepository,
    NodeRepository, ObservedStateRepository, OperationRepository,
};
use chv_controlplane_types::domain::NodeId;
use control_plane_node_api::control_plane_node_api as proto;
use std::sync::Arc;
use tonic::{Code, Request};

fn request_with_peer<T>(payload: T, peer_node_id: &str) -> Request<T> {
    let mut req = Request::new(payload);
    req.extensions_mut().insert(PeerNodeId(
        NodeId::new(peer_node_id.to_string()).expect("valid node_id"),
    ));
    req
}

async fn lifecycle_server() -> LifecycleServer {
    let pool = create_test_pool().await;
    sqlx::query(
        "INSERT INTO nodes (node_id, hostname, display_name) \
         VALUES ('node-a', 'host-a', 'node-a')",
    )
    .execute(&pool)
    .await
    .expect("seed node-a");
    sqlx::query(
        "INSERT INTO node_desired_state \
         (node_id, desired_generation, desired_state, requested_at, updated_at, scheduling_paused) \
         VALUES ('node-a', 1, 'TenantReady', \
                 strftime('%Y-%m-%dT%H:%M:%SZ','now'), \
                 strftime('%Y-%m-%dT%H:%M:%SZ','now'), false)",
    )
    .execute(&pool)
    .await
    .expect("seed node-a desired state");

    let service = LifecycleServiceImplementation::new(
        NodeRepository::new(pool.clone()),
        OperationRepository::new(pool.clone()),
        EventRepository::new(pool.clone()),
        DesiredStateRepository::new(pool),
    );
    LifecycleServer::new(Arc::new(service))
}

async fn telemetry_server() -> TelemetryServer {
    let pool = create_test_pool().await;
    let service = TelemetryServiceImplementation::new(
        NodeRepository::new(pool.clone()),
        ObservedStateRepository::new(pool.clone()),
        EventRepository::new(pool.clone()),
        AlertRepository::new(pool),
    );
    TelemetryServer::new(Arc::new(service))
}

fn pause_node_request(asserted_node_id: &str) -> proto::PauseNodeSchedulingRequest {
    proto::PauseNodeSchedulingRequest {
        meta: Some(proto::RequestMeta {
            operation_id: format!("op-pause-{asserted_node_id}"),
            requested_by: "node-a".into(),
            target_node_id: asserted_node_id.into(),
            desired_state_version: "1".into(),
            request_unix_ms: 1_000,
        }),
        node_id: asserted_node_id.into(),
    }
}

fn migration_progress(asserted_node_id: &str) -> proto::MigrationProgress {
    proto::MigrationProgress {
        vm_id: "vm-1".into(),
        operation_id: "op-migrate-1".into(),
        phase: proto::MigrationPhase::MemoryMigration as i32,
        bytes_transferred: 1_024,
        total_bytes: 4_096,
        convergence_round: 1,
        dirty_blocks_remaining: 0,
        progress_percent: 25.0,
        node_id: asserted_node_id.into(),
    }
}

/// A lifecycle dispatch asserting another node's identity must be rejected
/// before it can journal an operation or mutate that node's desired state.
#[tokio::test]
async fn pause_node_scheduling_rejects_peer_node_id_mismatch() {
    use proto::lifecycle_service_server::LifecycleService as _;

    let server = lifecycle_server().await;
    let req = request_with_peer(pause_node_request("node-b"), "node-a");
    let status = server
        .pause_node_scheduling(req)
        .await
        .expect_err("must reject cross-node lifecycle dispatch");

    assert_eq!(
        status.code(),
        Code::PermissionDenied,
        "mismatch must produce permission_denied; got: {} ({})",
        status.code(),
        status.message()
    );
    assert!(
        status.message().contains("node-a") && status.message().contains("node-b"),
        "status message must name both peer and requested ids; got: {}",
        status.message()
    );
}

/// The legitimate case still works: node A dispatching for node A is accepted.
#[tokio::test]
async fn pause_node_scheduling_accepts_matching_peer() {
    use proto::lifecycle_service_server::LifecycleService as _;

    let server = lifecycle_server().await;
    let req = request_with_peer(pause_node_request("node-a"), "node-a");
    let resp = server
        .pause_node_scheduling(req)
        .await
        .expect("matching peer must be accepted");

    assert_eq!(resp.into_inner().result.expect("result meta").status, "OK");
}

/// `MigrationProgress` has no nested node identity: the new `node_id` field is
/// the only binding point. A peer authenticated as node A must not be able to
/// write progress into node B's migration row.
#[tokio::test]
async fn report_migration_progress_rejects_peer_node_id_mismatch() {
    use proto::telemetry_service_server::TelemetryService as _;

    let server = telemetry_server().await;
    let req = request_with_peer(migration_progress("node-b"), "node-a");
    let status = server
        .report_migration_progress(req)
        .await
        .expect_err("must reject cross-node migration progress");

    assert_eq!(
        status.code(),
        Code::PermissionDenied,
        "mismatch must produce permission_denied; got: {} ({})",
        status.code(),
        status.message()
    );
    assert!(
        status.message().contains("node-a") && status.message().contains("node-b"),
        "status message must name both peer and requested ids; got: {}",
        status.message()
    );
}

/// Matching identity passes through to the telemetry service and is acked.
#[tokio::test]
async fn report_migration_progress_accepts_matching_peer() {
    use proto::telemetry_service_server::TelemetryService as _;

    let server = telemetry_server().await;
    let req = request_with_peer(migration_progress("node-a"), "node-a");
    let resp = server
        .report_migration_progress(req)
        .await
        .expect("matching peer must be accepted");

    assert_eq!(resp.into_inner().result.expect("result meta").status, "ok");
}
