//! Integration tests for the vm-delete → network-delete chain
//! (`POST /v1/vms/delete`, `POST /v1/networks/delete`).
//!
//! Issue kubedoio/chv#356 (part 1, the DB-side fix): `vm delete` never
//! removed the VM's `vm_nic_desired_state` rows (nothing in the
//! workspace deletes them; the `vms`/`vm_desired_state` rows persist as
//! 'Deleting' tombstones), and `network delete` counted those rows as
//! "attached VMs" — so after deleting every VM on a network, the
//! network delete was PERMANENTLY refused with 409 "N VM(s) still
//! attached" (verified on real KVM by the M4.4 qualification, evidence
//! `04-real-host-qualification/m4.4-network.md`, Leg E).
//!
//! Fixed contract asserted here:
//! - `vm delete` removes the VM's `vm_nic_desired_state` rows;
//! - `network delete` succeeds once the attached VMs are deleted (the
//!   issue's exact reproduction now ends in 200, not 409);
//! - `network delete` still REFUSES while a live VM is attached;
//! - legacy rows from pre-fix data (a 'Deleting' tombstone VM with
//!   lingering nic rows) no longer block the delete and are GCed
//!   (required: the nic table has network_id REFERENCES networks ON
//!   DELETE RESTRICT, so leaving them would FK-fail the delete).
//!
//! Non-scope (tracked in #356 part 2 / #355): the network delete still
//! performs no HOST teardown (bridge/nft/dnsmasq) — that requires the
//! node-dispatch design and is deliberately not asserted here.

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

/// Seed a user with the given role ('operator'/'admin') and return a
/// usable JWT bearer token.
async fn seed_jwt_as(state: &AppState, role: &str) -> String {
    let user_id = format!("u-{role}");
    sqlx::query(
        "INSERT INTO users (user_id, username, password_hash, role, must_change_password) \
         VALUES (?, ?, 'x', ?, 0)",
    )
    .bind(&user_id)
    .bind(role)
    .bind(role)
    .execute(&state.pool)
    .await
    .expect("seed user");

    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let claims = chv_webui_bff::auth::Claims {
        sub: user_id,
        username: role.to_string(),
        role: role.to_string(),
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

async fn seed_jwt(state: &AppState) -> String {
    seed_jwt_as(state, "operator").await
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

/// Create a VM attached to the given network (by its network_id — the
/// id-hit resolution path, valid on both pre- and post-#354 main).
async fn create_vm_on_network(state: &AppState, token: &str, network_id: &str) -> String {
    create_named_vm_on_network(state, token, network_id, "vm-x").await
}

async fn create_named_vm_on_network(
    state: &AppState,
    token: &str,
    network_id: &str,
    name: &str,
) -> String {
    let (status, body) = post_with_token(
        state.clone(),
        "/v1/vms/create",
        token,
        &format!(r#"{{"name":"{name}","image_ref":"/tmp/x.img","network_id":"{network_id}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "vm create body: {body}");
    body["vm_id"].as_str().expect("vm_id").to_string()
}

async fn delete_vm(state: &AppState, token: &str, vm_id: &str) -> (StatusCode, serde_json::Value) {
    post_with_token(
        state.clone(),
        "/v1/vms/delete",
        token,
        &format!(r#"{{"vm_id":"{vm_id}"}}"#),
    )
    .await
}

async fn delete_network(
    state: &AppState,
    token: &str,
    network_id: &str,
) -> (StatusCode, serde_json::Value) {
    post_with_token(
        state.clone(),
        "/v1/networks/delete",
        token,
        &format!(r#"{{"network_id":"{network_id}"}}"#),
    )
    .await
}

/// Number of nic desired-state rows on a network.
async fn nic_rows_on(state: &AppState, network_id: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM vm_nic_desired_state WHERE network_id = ?")
        .bind(network_id)
        .fetch_one(&state.pool)
        .await
        .expect("count nic rows")
}

#[tokio::test]
async fn vm_delete_removes_nic_rows() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;
    let net_id = create_network(&state, &token, "tenant-a", "10.99.0.0/24").await;
    let vm_id = create_vm_on_network(&state, &token, &net_id).await;

    assert_eq!(nic_rows_on(&state, &net_id).await, 1);

    let (status, body) = delete_vm(&state, &token, &vm_id).await;
    assert_eq!(status, StatusCode::OK, "vm delete body: {body}");

    // The fix: no lingering nic rows for the deleted VM.
    assert_eq!(
        nic_rows_on(&state, &net_id).await,
        0,
        "vm delete must remove the VM's vm_nic_desired_state rows"
    );
}

#[tokio::test]
async fn network_delete_succeeds_after_vm_delete() {
    // The issue #356 reproduction, end to end: create network + VM,
    // delete the VM, then the network delete must succeed (the frozen
    // candidate refused it permanently with 409).
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;
    let net_id = create_network(&state, &token, "tenant-a", "10.99.0.0/24").await;
    let vm_id = create_vm_on_network(&state, &token, &net_id).await;

    let (status, _) = delete_vm(&state, &token, &vm_id).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = delete_network(&state, &token, &net_id).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "network delete must succeed after all VMs are deleted, body: {body}"
    );

    // #499: the delete no longer removes the rows — it tombstones the
    // NDS row (terminal 'Deleting', generation bumped past the create's
    // 1) and keeps the physical row as the tombstone's anchor (the FK
    // cascades the other way). Deleted means the tombstone, not
    // row-absence.
    let (generation, status_col): (i64, Option<String>) = sqlx::query_as(
        "SELECT desired_generation, desired_status FROM network_desired_state WHERE network_id = ?",
    )
    .bind(&net_id)
    .fetch_one(&state.pool)
    .await
    .expect("fetch nds tombstone");
    assert_eq!(
        generation, 2,
        "the tombstone must bump the create's generation 1"
    );
    assert_eq!(
        status_col.as_deref(),
        Some("Deleting"),
        "the tombstone status must be terminal 'Deleting'"
    );
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM networks WHERE network_id = ?")
        .bind(&net_id)
        .fetch_one(&state.pool)
        .await
        .expect("count networks");
    assert_eq!(
        remaining, 1,
        "the physical row must survive as the tombstone's anchor"
    );
}

#[tokio::test]
async fn network_delete_clears_vni_allocations() {
    // The last #356 FK hazard: vni_allocations references
    // networks(network_id) with no ON DELETE action, so an overlay
    // network that ever allocated a VNI (active OR soft-released rows)
    // FK-failed the delete — un-deletable forever. The delete tx now
    // soft-releases then drops the rows.
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;
    let net_id = create_network(&state, &token, "tenant-a", "10.99.0.0/24").await;

    // Seed both shapes: an active allocation and a previously
    // soft-released one — the FK blocks on either.
    sqlx::query(
        "INSERT INTO vni_allocations (vni, network_id, allocated_at, binding_generation) \
         VALUES (100, ?, strftime('%Y-%m-%dT%H:%M:%SZ','now'), 1)",
    )
    .bind(&net_id)
    .execute(&state.pool)
    .await
    .expect("seed active vni allocation");
    sqlx::query(
        "INSERT INTO vni_allocations (vni, network_id, allocated_at, released_at, binding_generation) \
         VALUES (200, ?, strftime('%Y-%m-%dT00:00:00Z','now'), strftime('%Y-%m-%dT00:00:00Z','now'), 1)",
    )
    .bind(&net_id)
    .execute(&state.pool)
    .await
    .expect("seed released vni allocation");

    let (status, body) = delete_network(&state, &token, &net_id).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "network delete must succeed despite vni_allocations rows, body: {body}"
    );

    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM vni_allocations WHERE network_id = ?")
            .bind(&net_id)
            .fetch_one(&state.pool)
            .await
            .expect("count vni allocations");
    assert_eq!(
        remaining, 0,
        "the network's vni allocation rows must be gone"
    );
}

#[tokio::test]
async fn network_delete_refuses_while_vm_attached() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;
    let net_id = create_network(&state, &token, "tenant-a", "10.99.0.0/24").await;
    let _vm_id = create_vm_on_network(&state, &token, &net_id).await;

    let (status, body) = delete_network(&state, &token, &net_id).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "network delete must refuse while a live VM is attached, body: {body}"
    );
    let msg = body["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("still attached"),
        "error must name the refusal, got: {msg}"
    );

    // Nothing was deleted.
    assert_eq!(nic_rows_on(&state, &net_id).await, 1);
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM networks WHERE network_id = ?")
        .bind(&net_id)
        .fetch_one(&state.pool)
        .await
        .expect("count networks");
    assert_eq!(remaining, 1, "network row must survive the refusal");
}

#[tokio::test]
async fn network_delete_unblocks_legacy_deleting_vm_rows() {
    // Pre-#356 data shape: a VM deleted by the old code — desired_status
    // 'Deleting' tombstone, nic rows lingering. The liveness-aware count
    // must not treat it as attached, and the GC must remove its stale
    // rows (or the FK RESTRICT would fail the networks delete).
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;
    let net_id = create_network(&state, &token, "tenant-a", "10.99.0.0/24").await;
    let vm_id = create_vm_on_network(&state, &token, &net_id).await;

    sqlx::query("UPDATE vm_desired_state SET desired_status = 'Deleting' WHERE vm_id = ?")
        .bind(&vm_id)
        .execute(&state.pool)
        .await
        .expect("simulate legacy deleting tombstone");
    assert_eq!(
        nic_rows_on(&state, &net_id).await,
        1,
        "legacy shape: nic row still present under the tombstone"
    );

    let (status, body) = delete_network(&state, &token, &net_id).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "legacy 'Deleting' tombstone must not block the network delete, body: {body}"
    );
    assert_eq!(
        nic_rows_on(&state, &net_id).await,
        0,
        "stale nic rows must be GCed by the delete"
    );
}

#[tokio::test]
async fn network_delete_refuses_with_mixed_live_and_deleting_vms() {
    // The interaction that matters most: ONE live VM must still refuse
    // the delete even when other attachments are 'Deleting' tombstones
    // — and the refusal must touch nothing (no GC, no network row
    // removal), or the live VM's attachment record would be lost.
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;
    let net_id = create_network(&state, &token, "tenant-a", "10.99.0.0/24").await;
    let _live_vm = create_named_vm_on_network(&state, &token, &net_id, "vm-live").await;
    let dying_vm = create_named_vm_on_network(&state, &token, &net_id, "vm-dying").await;

    sqlx::query("UPDATE vm_desired_state SET desired_status = 'Deleting' WHERE vm_id = ?")
        .bind(&dying_vm)
        .execute(&state.pool)
        .await
        .expect("simulate deleting tombstone");

    let (status, body) = delete_network(&state, &token, &net_id).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a live VM must still block the delete, body: {body}"
    );
    let msg = body["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("1 VM(s) still attached"),
        "count must exclude the Deleting tombstone, got: {msg}"
    );

    // Nothing was touched: both nic rows (live AND tombstone) and the
    // network row survive — the GC must never run on a refusal.
    assert_eq!(nic_rows_on(&state, &net_id).await, 2);
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM networks WHERE network_id = ?")
        .bind(&net_id)
        .fetch_one(&state.pool)
        .await
        .expect("count networks");
    assert_eq!(remaining, 1, "network row must survive the refusal");
}

#[tokio::test]
async fn network_delete_counts_every_live_vm() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;
    let net_id = create_network(&state, &token, "tenant-a", "10.99.0.0/24").await;
    let _a = create_named_vm_on_network(&state, &token, &net_id, "vm-a").await;
    let _b = create_named_vm_on_network(&state, &token, &net_id, "vm-b").await;

    let (status, body) = delete_network(&state, &token, &net_id).await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    let msg = body["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("2 VM(s) still attached"),
        "both live VMs must be counted, got: {msg}"
    );
}

#[tokio::test]
async fn network_delete_of_missing_network_is_404_for_admin() {
    // The exists check moved inside the tx — pin that a missing network
    // still yields NotFound (reachable only for admins: an operator
    // fails the ownership check first, which is the safe direction).
    let state = build_state().await;
    let token = seed_jwt_as(&state, "admin").await;
    seed_node(&state).await;

    let (status, body) = delete_network(&state, &token, "no-such-network").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "missing network must 404, body: {body}"
    );
}

/// #499: the delete's own semantics — the tombstone lands with the
/// requester stamped, the reads that must exclude it do (list, detail,
/// repeat delete), and the derived state the old cascade removed is
/// still cleaned (exposures, nic rows, vni allocations).
#[tokio::test]
async fn network_delete_tombstone_lands_and_reads_exclude_it() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;
    let net_id = create_network(&state, &token, "tenant-tomb", "10.99.3.0/24").await;

    // Derived state the old cascade removed — seed all three shapes.
    sqlx::query(
        "INSERT INTO network_exposures (network_id, service_name, protocol, listen_port) \
         VALUES (?, 'web', 'tcp', 80)",
    )
    .bind(&net_id)
    .execute(&state.pool)
    .await
    .expect("seed exposure");
    sqlx::query(
        "INSERT INTO vni_allocations (vni, network_id, allocated_at, binding_generation) \
         VALUES (100, ?, strftime('%Y-%m-%dT%H:%M:%SZ','now'), 1)",
    )
    .bind(&net_id)
    .execute(&state.pool)
    .await
    .expect("seed vni allocation");

    let (status, body) = delete_network(&state, &token, &net_id).await;
    assert_eq!(status, StatusCode::OK, "delete must succeed, body: {body}");

    // The tombstone: terminal status, generation bumped (create wrote
    // 1), the deleting operator stamped as updated_by.
    let (generation, status_col, updated_by): (i64, Option<String>, Option<String>) =
        sqlx::query_as(
            "SELECT desired_generation, desired_status, updated_by \
             FROM network_desired_state WHERE network_id = ?",
        )
        .bind(&net_id)
        .fetch_one(&state.pool)
        .await
        .expect("fetch nds tombstone");
    assert_eq!(
        generation, 2,
        "the tombstone must bump the create's generation 1"
    );
    assert_eq!(status_col.as_deref(), Some("Deleting"));
    assert_eq!(
        updated_by.as_deref(),
        Some("u-operator"),
        "the deleter must be stamped"
    );

    // The derived-state cleanup the cascade used to do.
    for (table, what) in [
        ("network_exposures", "exposure rows"),
        ("vni_allocations", "vni allocations"),
        ("vm_nic_desired_state", "nic rows"),
    ] {
        let count: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {table} WHERE network_id = ?"
        ))
        .bind(&net_id)
        .fetch_one(&state.pool)
        .await
        .expect("count");
        assert_eq!(count, 0, "{what} must be cleaned by the delete");
    }

    // The reads exclude the tombstone: list omits it, detail 404s, and
    // a repeat delete keeps the pre-#499 NotFound contract.
    let (status, body) = post_with_token(
        state.clone(),
        "/v1/networks",
        &token,
        r#"{"page":1,"page_size":50}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "list must succeed, body: {body}");
    let listed = body["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter(|item| item["network_id"].as_str() == Some(net_id.as_str()))
        .count();
    assert_eq!(
        listed, 0,
        "a tombstoned network must not appear in the list"
    );

    let (status, body) = post_with_token(
        state.clone(),
        "/v1/networks/get",
        &token,
        &format!(r#"{{"network_id":"{net_id}"}}"#),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a tombstoned network must 404 on detail, body: {body}"
    );

    let (status, body) = delete_network(&state, &token, &net_id).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a repeat delete must keep the NotFound contract, body: {body}"
    );
    let (generation, status_col): (i64, Option<String>) = sqlx::query_as(
        "SELECT desired_generation, desired_status FROM network_desired_state WHERE network_id = ?",
    )
    .bind(&net_id)
    .fetch_one(&state.pool)
    .await
    .expect("fetch nds tombstone");
    assert_eq!(
        (generation, status_col.as_deref()),
        (2, Some("Deleting")),
        "the refused repeat delete must not re-stamp the tombstone"
    );
}

/// #499: a VM create referencing a deleted network is refused with the
/// clean 409 — pre-#499 the absent row fell through to implicit
/// creation; a naive tombstone retention would have ATTACHED the VM to
/// the deleted network instead.
#[tokio::test]
async fn vm_create_referencing_a_deleted_network_is_refused() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;
    let net_id = create_network(&state, &token, "tenant-gone", "10.99.4.0/24").await;

    let (status, _) = delete_network(&state, &token, &net_id).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = post_with_token(
        state.clone(),
        "/v1/vms/create",
        &token,
        &format!(r#"{{"name":"vm-x","image_ref":"/tmp/x.img","network_id":"{net_id}"}}"#),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a VM create naming a deleted network must refuse, body: {body}"
    );
    let msg = body["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("has been deleted"),
        "the refusal must name the deletion, got: {msg}"
    );

    // Fail-closed: nothing was journaled.
    let vms: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vms")
        .fetch_one(&state.pool)
        .await
        .expect("count vms");
    assert_eq!(vms, 0, "no VM row may land from the refused create");
    let nics: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM vm_nic_desired_state WHERE network_id = ?")
            .bind(&net_id)
            .fetch_one(&state.pool)
            .await
            .expect("count nics");
    assert_eq!(nics, 0, "no nic row may attach to the deleted network");
}

/// #499, the resurrection pin at the BFF tier: after the delete, the
/// exact store upsert the CP fragment path drives
/// (`upsert_network_with_exposures`, the only production caller of the
/// physical `networks` upsert) is refused against the tombstone — no
/// NDS resurrection, no physical re-assert, no exposures. The
/// end-to-end fragment entry (`apply_network_desired_state`) is pinned
/// in `chv-controlplane-service`'s suite.
#[tokio::test]
async fn late_fragment_upsert_cannot_resurrect_a_deleted_network() {
    use chv_controlplane_types::domain::{Generation, ResourceId};

    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;
    let net_id = create_network(&state, &token, "tenant-res", "10.99.5.0/24").await;

    let (status, _) = delete_network(&state, &token, &net_id).await;
    assert_eq!(status, StatusCode::OK);

    // The late fragment's write, worst case: a wall-clock-milliseconds
    // generation (always beats the tombstone's small integer) with an
    // exposure riding the spec.
    let wall_clock = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let err = state
        .desired_state_repo
        .upsert_network_with_exposures(
            &chv_controlplane_store::NetworkDesiredStateInput {
                network_id: ResourceId::new(&net_id).unwrap(),
                node_id: None,
                display_name: format!("resurrected-{net_id}"),
                network_class: Some("bridge".into()),
                desired_generation: Generation::new(wall_clock as u64),
                desired_status: None,
                requested_by: Some("agent".into()),
                updated_by: Some("agent".into()),
                firewall_rules_json: None,
                nat_rules_json: None,
                dhcp_scope_json: None,
                dns_enabled: None,
                dns_scope_json: None,
                requested_unix_ms: wall_clock,
            },
            &[chv_controlplane_store::NetworkExposureInput {
                network_id: ResourceId::new(&net_id).unwrap(),
                service_name: "web".into(),
                protocol: "tcp".into(),
                listen_address: None,
                listen_port: Some(80),
                target_address: None,
                target_port: Some(8080),
                exposure_policy: None,
                updated_unix_ms: wall_clock,
            }],
        )
        .await
        .expect_err("the fragment upsert must refuse a tombstoned network");
    assert!(
        matches!(
            err,
            chv_controlplane_store::StoreError::Conflict { reason, .. }
                if reason.contains("network is deleting")
        ),
        "the refusal must be the loud tombstone Conflict, got: {err:?}"
    );

    // The network stays deleted: tombstone intact, physical row not
    // re-asserted, no exposure row.
    let (generation, status_col): (i64, Option<String>) = sqlx::query_as(
        "SELECT desired_generation, desired_status FROM network_desired_state WHERE network_id = ?",
    )
    .bind(&net_id)
    .fetch_one(&state.pool)
    .await
    .expect("fetch nds tombstone");
    assert_eq!((generation, status_col.as_deref()), (2, Some("Deleting")));
    let name: String = sqlx::query_scalar("SELECT display_name FROM networks WHERE network_id = ?")
        .bind(&net_id)
        .fetch_one(&state.pool)
        .await
        .expect("fetch networks row");
    assert_eq!(name, "tenant-res", "the physical row must keep its shape");
    let exposures: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM network_exposures WHERE network_id = ?")
            .bind(&net_id)
            .fetch_one(&state.pool)
            .await
            .expect("count exposures");
    assert_eq!(exposures, 0, "no exposure row may land for a tombstone");
}
