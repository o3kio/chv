//! Integration tests for #525: standalone volumes accrue toward the
//! aggregate storage quota.
//!
//! Pre-fix shape: `enforce_user_quota`'s storage-usage SUM joined
//! `volumes → volume_desired_state → vm_desired_state` via
//! `attached_vm_id`/`requested_by`, so a standalone volume (NULL
//! `attached_vm_id`) never accrued toward `used` — each create was
//! checked individually, but N successive creates could stack past
//! `max_storage_bytes` in total. Both usage meters
//! (`quotas.rs::compute_usage_payload` and `quotas.rs::check_quota`)
//! under-reported standalone volumes the same way.
//!
//! Post-fix shape (the #525 counting rule, in
//! `quotas.rs::storage_usage_bytes`): a volume accrues toward user U's
//! storage usage iff U owns it OR it is attached to a VM U requested —
//! counted exactly once per user when both hold.
//!
//! These tests pin:
//!
//! - **stacking** — create at the limit, second create → 422 with zero
//!   journaling (the #525 sketch's test);
//! - **no double-count** — a volume the user owns AND attached to the
//!   user's own VM counts once, not twice;
//! - **the cross-user rule** — a volume owned by A attached to B's VM
//!   accrues once to A (owner) and once to B (the VM's requester);
//! - **the meters** — `/v1/usage` and `/v1/quotas/:user_id/usage` (and
//!   `/v1/quotas/check`) include standalone volume bytes, matching
//!   enforcement;
//! - **#522 DP9 release** — a `'Deleting'` volume stops accruing for
//!   the owner AND the attacher the moment the tombstone is journaled
//!   (driven through the real `POST /v1/volumes/delete` route), on
//!   every read path: enforcement, both usage meters, and the
//!   quota-check meter; plus the predicate's LEFT-JOIN safety (a
//!   volume with no VDS row still accrues via ownership).

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
use chv_webui_bff::auth::Claims;
use chv_webui_bff::mutations::MutationService;
use chv_webui_bff::{AppState, BffError};
use serde_json::Value;
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
        _source_volume_id: String,
        _target_volume_id: String,
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
        monitoring: None,
        monitoring_health: chv_webui_bff::MonitoringHealth::new(),
        agent_runtime_dir: std::path::PathBuf::from("/var/lib/chv/agent"),
        cache: chv_webui_bff::BffCache::new(5),
        clock: Arc::new(SystemClock),
    }
}

fn token_for(state: &AppState, sub: &str, role: &str) -> String {
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
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

async fn seed_node(state: &AppState, node_id: &str) {
    sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES (?, 'h', 'h')")
        .bind(node_id)
        .execute(&state.pool)
        .await
        .expect("seed node");
}

/// Seed a quota row with only a storage cap (the column under test).
async fn seed_storage_quota(state: &AppState, user_id: &str, max_storage_bytes: i64) {
    sqlx::query("INSERT INTO quotas (user_id, max_storage_bytes) VALUES (?, ?)")
        .bind(user_id)
        .bind(max_storage_bytes)
        .execute(&state.pool)
        .await
        .expect("seed quota");
}

/// Seed a VM requested by `requested_by` (a `vm_desired_state` row with
/// NULL storage footprint so only the volumes accrue).
async fn seed_vm(state: &AppState, vm_id: &str, requested_by: &str) {
    sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES (?, ?)")
        .bind(vm_id)
        .bind(vm_id)
        .execute(&state.pool)
        .await
        .expect("insert vm");
    sqlx::query(
        "INSERT INTO vm_desired_state (vm_id, desired_generation, requested_by, cpu_count, memory_bytes) \
         VALUES (?, 1, ?, 0, 0)",
    )
    .bind(vm_id)
    .bind(requested_by)
    .execute(&state.pool)
    .await
    .expect("insert vm_desired_state");
}

/// Seed a volume row plus its desired state — standalone when
/// `attached_vm_id` is None, attached to that VM otherwise. `owner_id`
/// and the attach target's requester are set independently: the
/// cross-user rule test needs them to differ.
async fn seed_volume(
    state: &AppState,
    volume_id: &str,
    owner_id: &str,
    capacity_bytes: i64,
    attached_vm_id: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO volumes (volume_id, node_id, display_name, owner_id, capacity_bytes, updated_at) \
         VALUES (?, 'n-1', ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%SZ','now'))",
    )
    .bind(volume_id)
    .bind(volume_id)
    .bind(owner_id)
    .bind(capacity_bytes)
    .execute(&state.pool)
    .await
    .expect("insert volume");
    sqlx::query(
        "INSERT INTO volume_desired_state (volume_id, desired_generation, desired_status, requested_by, attached_vm_id) \
         VALUES (?, 1, 'Active', ?, ?)",
    )
    .bind(volume_id)
    .bind(owner_id)
    .bind(attached_vm_id)
    .execute(&state.pool)
    .await
    .expect("insert volume_desired_state");
}

async fn post(state: &AppState, token: &str, path: &str, body: &str) -> (StatusCode, Value) {
    let app = chv_webui_bff::bff_router(state.clone()).with_state(state.clone());
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
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, body)
}

/// POST /v1/volumes/create as the operator `sub`.
async fn create_as(state: &AppState, sub: &str, body: &str) -> (StatusCode, Value) {
    let token = token_for(state, sub, "operator");
    post(state, &token, "/v1/volumes/create", body).await
}

const GIB: i64 = 1024 * 1024 * 1024;

// ─────────────────────────────────────────────────────────────────────
// (a) Stacking — the #525 sketch's test
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn standalone_creates_cannot_stack_past_the_aggregate_limit() {
    let state = build_state().await;
    seed_node(&state, "n-1").await;
    // A 1 GiB cap: the first create fills it exactly, so any second
    // create must be refused on the aggregate — the stacking the gap
    // allowed (pre-#525 the second create saw used = 0 and passed).
    seed_storage_quota(&state, "u-operator", GIB).await;

    // The first create fills the quota exactly (max is inclusive).
    let (status, body) = create_as(
        &state,
        "u-operator",
        r#"{"name":"vol-1","node_id":"n-1","capacity_bytes":1073741824}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    // The second create — a tiny one, individually far under the
    // per-create bound — is refused: the first standalone volume now
    // accrues toward `used`, so 1 GiB + 1 KiB > 1 GiB. Pre-#525 this
    // returned OK (the gap: N creates could stack arbitrarily).
    let (status, body) = create_as(
        &state,
        "u-operator",
        r#"{"name":"vol-2","node_id":"n-1","capacity_bytes":1024}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a stacked create past the aggregate limit must 422: {body}"
    );
    let body = body.to_string();
    assert!(
        body.contains("QUOTA_EXCEEDED"),
        "the rejection must carry the quota code: {body}"
    );

    // Zero journaling for the rejected create: only the first create's
    // rows exist (the table-loop assertion pattern).
    for (table, label) in [
        ("volumes", "volumes"),
        ("volume_desired_state", "volume desired state"),
        ("operations", "operations"),
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "the rejected create must not journal {label}");
    }
}

// ─────────────────────────────────────────────────────────────────────
// (b) No double-count — owned AND attached to your own VM counts once
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_owned_volume_attached_to_your_own_vm_counts_once() {
    let state = build_state().await;
    seed_node(&state, "n-1").await;
    seed_vm(&state, "vm-own", "u-operator").await;
    // 1 GiB volume, owned by u-operator, attached to u-operator's VM —
    // it matches BOTH counting conditions for the same user.
    seed_volume(&state, "vol-dual", "u-operator", GIB, Some("vm-own")).await;
    seed_storage_quota(&state, "u-operator", 2 * GIB).await;

    // If the SUM double-counted the volume (used = 2 GiB), this
    // 1 GiB create would 422; counted once (used = 1 GiB) it fits.
    let (status, body) = create_as(
        &state,
        "u-operator",
        r#"{"name":"vol-next","node_id":"n-1","capacity_bytes":1073741824}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a volume both owned and attached-to-your-VM must count once: {body}"
    );

    // And the aggregate is now genuinely at the cap: one more byte is a
    // 422 (proves the first volume plus the new one did accrue).
    let (status, body) = create_as(
        &state,
        "u-operator",
        r#"{"name":"vol-over","node_id":"n-1","capacity_bytes":1024}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the cap must still bind after the counted-once create: {body}"
    );
}

// ─────────────────────────────────────────────────────────────────────
// (c) Cross-user rule — A's volume on B's VM accrues once to each
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_volume_owned_by_alice_attached_to_bobs_vm_accrues_once_to_each() {
    let state = build_state().await;
    seed_node(&state, "n-1").await;
    seed_vm(&state, "vm-bob", "u-bob").await;
    // 1 GiB owned by u-alice, attached to u-bob's VM.
    seed_volume(&state, "vol-cross", "u-alice", GIB, Some("vm-bob")).await;
    seed_storage_quota(&state, "u-alice", 2 * GIB).await;
    seed_storage_quota(&state, "u-bob", 2 * GIB).await;

    // The owner sees it: 1 GiB of alice's 2 GiB is used, so a 1 GiB
    // create fits and a further byte does not.
    let (status, body) = create_as(
        &state,
        "u-alice",
        r#"{"name":"vol-alice","node_id":"n-1","capacity_bytes":1073741824}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the owner's quota must see the attached-away volume (once): {body}"
    );
    let (status, _) = create_as(
        &state,
        "u-alice",
        r#"{"name":"vol-alice-2","node_id":"n-1","capacity_bytes":1024}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the owner's cap must bind at 2 GiB (the volume counted once, not twice)"
    );

    // The VM's requester sees it too, independently: bob's quota also
    // has 1 GiB used, so the same 1 GiB create fits for him.
    let (status, body) = create_as(
        &state,
        "u-bob",
        r#"{"name":"vol-bob","node_id":"n-1","capacity_bytes":1073741824}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the VM requester's quota must see the attached volume (the pre-#525 behavior): {body}"
    );

    // The meters agree with enforcement for both users — each shows the
    // volume's bytes exactly once (2 GiB after each user's own create:
    // 1 GiB cross volume + 1 GiB own standalone create).
    let admin = token_for(&state, "u-admin", "admin");
    for user in ["u-alice", "u-bob"] {
        let (status, body) = post(&state, &admin, &format!("/v1/quotas/{user}/usage"), "{}").await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert_eq!(
            body.pointer("/usage/disk_gb").and_then(|v| v.as_i64()),
            Some(2),
            "{user}'s meter must show the cross volume and their own create, the cross volume counted once: {body}"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────
// (d) The meters include standalone volumes
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_ownerless_legacy_volume_accrues_only_via_attachment() {
    // Pre-#386 legacy rows (owner_id NULL) keep the pre-#525 accrual
    // behavior: never via ownership (NULL matches no user), only via
    // attachment to a VM — and to nobody when standalone.
    let state = build_state().await;
    seed_node(&state, "n-1").await;
    seed_vm(&state, "vm-bob", "u-bob").await;
    // 1 GiB ownerless legacy volume, attached to u-bob's VM.
    for (vol, attached) in [
        ("vol-legacy-att", Some("vm-bob")),
        ("vol-legacy-solo", None),
    ] {
        sqlx::query(
            "INSERT INTO volumes (volume_id, node_id, display_name, owner_id, capacity_bytes, updated_at) \
             VALUES (?, 'n-1', ?, NULL, ?, strftime('%Y-%m-%dT%H:%M:%SZ','now'))",
        )
        .bind(vol)
        .bind(vol)
        .bind(GIB)
        .execute(&state.pool)
        .await
        .expect("insert ownerless volume");
        sqlx::query(
            "INSERT INTO volume_desired_state (volume_id, desired_generation, desired_status, requested_by, attached_vm_id) \
             VALUES (?, 1, 'Active', 'u-bob', ?)",
        )
        .bind(vol)
        .bind(attached)
        .execute(&state.pool)
        .await
        .expect("insert volume_desired_state");
    }
    seed_storage_quota(&state, "u-bob", 2 * GIB).await;

    // Bob accrues the ATTACHED legacy volume (1 GiB) but not the
    // STANDALONE ownerless one: 1 GiB used, so a 1 GiB create fits
    // and one more byte does not.
    let (status, body) = create_as(
        &state,
        "u-bob",
        r#"{"name":"vol-bob","node_id":"n-1","capacity_bytes":1073741824}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "bob accrues the attached ownerless volume but NOT the standalone ownerless one: {body}"
    );
    let (status, _) = create_as(
        &state,
        "u-bob",
        r#"{"name":"vol-bob-2","node_id":"n-1","capacity_bytes":1024}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "bob's cap must bind at 2 GiB (only the attached legacy volume counted)"
    );
}

#[tokio::test]
async fn usage_meters_include_standalone_volume_bytes() {
    let state = build_state().await;
    seed_node(&state, "n-1").await;
    seed_storage_quota(&state, "u-operator", 64 * GIB).await;

    // No VMs, no attachments — only a standalone create.
    let (status, body) = create_as(
        &state,
        "u-operator",
        r#"{"name":"vol-solo","node_id":"n-1","capacity_bytes":2147483648}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    // `/v1/usage` (self) shows the accrued bytes — pre-#525 this was 0.
    let token = token_for(&state, "u-operator", "operator");
    let (status, body) = post(&state, &token, "/v1/usage", "{}").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        body.pointer("/usage/disk_gb").and_then(|v| v.as_i64()),
        Some(2),
        "the self-usage meter must include standalone volumes: {body}"
    );

    // `/v1/quotas/:user_id/usage` (admin path) shows the same number —
    // display matches enforcement.
    let admin = token_for(&state, "u-admin", "admin");
    let (status, body) = post(&state, &admin, "/v1/quotas/u-operator/usage", "{}").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        body.pointer("/usage/disk_gb").and_then(|v| v.as_i64()),
        Some(2),
        "the per-user usage meter must include standalone volumes: {body}"
    );

    // `/v1/quotas/check` reports the same `current_usage` the
    // enforcement check would see.
    let (status, body) = post(&state, &token, "/v1/quotas/check", "{}").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        body.pointer("/current_usage/disk_gb")
            .and_then(|v| v.as_i64()),
        Some(2),
        "the quota-check meter must include standalone volumes: {body}"
    );
}

// ─────────────────────────────────────────────────────────────────────
// (e) #522 DP9 — a 'Deleting' volume stops accruing (the release)
// ─────────────────────────────────────────────────────────────────────

/// POST /v1/volumes/delete as the operator `sub` (the #522 PR 2 route).
async fn delete_as(state: &AppState, sub: &str, volume_id: &str) -> (StatusCode, Value) {
    let token = token_for(state, sub, "operator");
    post(
        state,
        &token,
        "/v1/volumes/delete",
        &format!(r#"{{"volume_id":"{volume_id}"}}"#),
    )
    .await
}

#[tokio::test]
async fn a_deleting_volume_stops_accruing_for_owner_and_attacher() {
    // The one place the volume tombstone (DP1) needs a quota predicate
    // (DP9): a tombstoned volume's capacity stops counting the moment
    // the delete is journaled, releasing the owner's AND the attacher's
    // accrual at once — they read the same canonical query. Driven
    // through the real delete route, on every read path: enforcement,
    // both usage meters, and the quota-check meter.
    let state = build_state().await;
    seed_node(&state, "n-1").await;
    // Bob's VM is 'Deleting' — the DP5 refinement that makes a volume
    // attached to a deleted VM deletable (its attachment still accrues
    // to Bob until the volume's own tombstone lands).
    seed_vm(&state, "vm-bob", "u-bob").await;
    sqlx::query("UPDATE vm_desired_state SET desired_status = 'Deleting' WHERE vm_id = 'vm-bob'")
        .execute(&state.pool)
        .await
        .expect("tombstone bob's vm");
    // 1 GiB owned by u-alice, attached to u-bob's VM, kind 'data' (the
    // DP6 gate) — the cross-user shape of test (c).
    seed_volume(&state, "vol-cross", "u-alice", GIB, Some("vm-bob")).await;
    sqlx::query("UPDATE volumes SET volume_kind = 'data' WHERE volume_id = 'vol-cross'")
        .execute(&state.pool)
        .await
        .expect("stamp data kind");
    seed_storage_quota(&state, "u-alice", GIB).await;
    seed_storage_quota(&state, "u-bob", 2 * GIB).await;

    let admin = token_for(&state, "u-admin", "admin");
    for user in ["u-alice", "u-bob"] {
        let (status, body) = post(&state, &admin, &format!("/v1/quotas/{user}/usage"), "{}").await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert_eq!(
            body.pointer("/usage/disk_gb").and_then(|v| v.as_i64()),
            Some(1),
            "{user} accrues the volume before the delete: {body}"
        );
    }

    // Enforcement sees it too: alice is at her 1 GiB cap, so one more
    // byte is a 422.
    let (status, _) = create_as(
        &state,
        "u-alice",
        r#"{"name":"vol-nope","node_id":"n-1","capacity_bytes":1024}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the live volume must bind alice's quota before the delete"
    );

    // The delete (by the owner) journals the tombstone.
    let (status, body) = delete_as(&state, "u-alice", "vol-cross").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    // Both meters drop to zero — the owner's AND the attacher's
    // accrual release at once, through the one predicate.
    for user in ["u-alice", "u-bob"] {
        let (status, body) = post(&state, &admin, &format!("/v1/quotas/{user}/usage"), "{}").await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert_eq!(
            body.pointer("/usage/disk_gb").and_then(|v| v.as_i64()),
            Some(0),
            "{user}'s meter must drop after the tombstone: {body}"
        );
    }

    // The self-usage meter and the quota-check meter agree.
    let alice = token_for(&state, "u-alice", "operator");
    let (status, body) = post(&state, &alice, "/v1/usage", "{}").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        body.pointer("/usage/disk_gb").and_then(|v| v.as_i64()),
        Some(0),
        "the self-usage meter must drop after the tombstone: {body}"
    );
    let (status, body) = post(&state, &alice, "/v1/quotas/check", "{}").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        body.pointer("/current_usage/disk_gb")
            .and_then(|v| v.as_i64()),
        Some(0),
        "the quota-check meter must drop after the tombstone: {body}"
    );

    // And enforcement releases with them: the same 1 GiB create alice
    // was refused now fits (exactly — her cap is 1 GiB).
    let (status, body) = create_as(
        &state,
        "u-alice",
        r#"{"name":"vol-after","node_id":"n-1","capacity_bytes":1073741824}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the deleted volume's capacity must no longer bind alice's quota: {body}"
    );
}

#[tokio::test]
async fn a_volume_with_no_vds_row_still_accrues_via_ownership() {
    // The DP9 predicate is LEFT-JOIN-safe (the `networks.rs` liveness
    // discipline, byte-exactly): `vd.desired_status` is NULL for a
    // volume with no `volume_desired_state` row, and the predicate's
    // IS-NULL arm keeps today's semantics for orphan rows — ownership
    // still accrues. A naive `!= 'Deleting'` alone would silently drop
    // every orphan row from the count.
    let state = build_state().await;
    seed_node(&state, "n-1").await;
    seed_storage_quota(&state, "u-alice", 2 * GIB).await;
    sqlx::query(
        "INSERT INTO volumes (volume_id, node_id, display_name, owner_id, capacity_bytes, updated_at) \
         VALUES ('vol-orphan', 'n-1', 'vol-orphan', 'u-alice', ?, strftime('%Y-%m-%dT%H:%M:%SZ','now'))",
    )
    .bind(GIB)
    .execute(&state.pool)
    .await
    .expect("insert orphan volume (no VDS row)");

    let admin = token_for(&state, "u-admin", "admin");
    let (status, body) = post(&state, &admin, "/v1/quotas/u-alice/usage", "{}").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        body.pointer("/usage/disk_gb").and_then(|v| v.as_i64()),
        Some(1),
        "an orphan (VDS-less) volume must still accrue via ownership: {body}"
    );
}
