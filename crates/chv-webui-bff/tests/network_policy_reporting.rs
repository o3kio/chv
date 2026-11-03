//! Integration tests for `POST /v1/networks/update` policy reporting
//! (issue kubedoio/chv#355).
//!
//! #355 PR 2 (DP1 + DP6): a firewall-carrying update is a real
//! mutation — it journals an `Accepted` `UpdateNetworkPolicy`
//! operation in the same transaction as the desired-state write (the
//! CreateVolume journaling precedent), which the PR 1 orchestrator
//! arm claims and fans out to every node with a live attached VM on
//! the network. The response is the standard mutation task surface
//! (`{accepted, task_id, network_id, summary, next_refresh_path}`),
//! replacing the pre-#355 `policy_application` prose notes — the
//! task's terminal state (and its #502 cause, on failure) is the
//! honest reporting now.
//!
//! Pinned here:
//! - an update carrying `firewall_rules` journals the per-generation
//!   operation and answers with the task shape (field-by-field);
//! - the network detail's `last_task` resolves to the policy operation;
//! - clearing (`[]`) mints its own per-generation task whose dispatch
//!   carries the DP4 baseline (DHCP/DNS/conntrack + default-deny), with
//!   a summary naming the DP5 clear semantics;
//! - an update without firewall fields journals nothing and keeps the
//!   read-after-write detail response;
//! - the save-time vocabulary/rejection gates (the M4.4 N7 class) are
//!   unchanged.

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
    build_state_with_pool(pool).await
}

/// The race-pool shape for the #355 PR 2 concurrency pin: a temp-file
/// SQLite database with the prod pragma profile (WAL, busy_timeout 5s)
/// and multiple connections — the BFF quota-race suite's shape
/// (vms.rs `build_test_pool`). The in-memory single-connection
/// harness cannot express cross-connection write locking: the pool
/// serializes everything before SQLite ever sees concurrent writers.
async fn build_race_state() -> (tempfile::TempDir, AppState) {
    use std::str::FromStr as _;
    let dir = tempfile::tempdir().expect("tempdir");
    let url = format!(
        "sqlite://{}",
        dir.path().join("chv-policy-race.db").display()
    );
    let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
        .expect("parse sqlite url")
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .pragma("foreign_keys", "ON")
        .busy_timeout(std::time::Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect_with(opts)
        .await
        .expect("connect race pool");
    let state = build_state_with_pool(pool).await;
    (dir, state)
}

async fn build_state_with_pool(pool: sqlx::sqlite::SqlitePool) -> AppState {
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

#[tokio::test]
async fn update_network_journals_and_returns_the_policy_task() {
    // #355 PR 2 (DP1 + DP6): a firewall-carrying update answers with
    // the standard mutation task surface — NOT the read-after-write
    // detail plus prose note it used to return — and journals an
    // `UpdateNetworkPolicy` operation in the same transaction as the
    // desired-state write, keyed per-generation (the CreateVolume
    // journaling precedent). The PR 1 orchestrator arm claims the
    // journaled row; the attach-time snapshot mechanism is unchanged.
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;
    let net_id = create_network(&state, &token, "tenant-a", "10.99.0.0/24").await;

    let (status, body) = post_with_token(
        state.clone(),
        "/v1/networks/update",
        &token,
        &format!(
            r#"{{"network_id":"{net_id}","firewall_rules":[{{"direction":"inbound","action":"accept","protocol":"icmp","source_cidr":"10.99.0.0/24"}}]}}"#
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "update body: {body}");
    // The task surface (the mutate_volume/create_volume shape).
    assert_eq!(body["accepted"].as_bool(), Some(true), "task body: {body}");
    let task_id = body["task_id"]
        .as_str()
        .unwrap_or_else(|| panic!("task_id must be present: {body}"))
        .to_string();
    assert_eq!(
        body["network_id"].as_str(),
        Some(net_id.as_str()),
        "the task body carries the network id (the UI's only read key): {body}"
    );
    assert!(
        body["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("applies on every node"),
        "the summary names the dispatch semantics: {body}"
    );
    assert_eq!(
        body["next_refresh_path"].as_str(),
        Some(format!("/api/v1/tasks/{}", task_id).as_str()),
        "the refresh path names the task: {body}"
    );
    // The prose note is REPLACED by the task surface (DP6).
    assert!(
        body.get("policy_application").is_none(),
        "the policy_application prose note is gone: {body}"
    );

    // The journaled operation: Accepted, per-generation key, owner
    // stamped, the bumped generation riding the row.
    let (op_type, op_status, key, requested_by, generation): (String, String, String, String, i64) =
        sqlx::query_as(
            "SELECT operation_type, status, idempotency_key, requested_by, desired_generation \
             FROM operations WHERE operation_id = ?",
        )
        .bind(&task_id)
        .fetch_one(&state.pool)
        .await
        .expect("query journaled operation");
    assert_eq!(op_type, "UpdateNetworkPolicy");
    assert_eq!(op_status, "Accepted");
    assert_eq!(
        key,
        format!("update-network-policy-{}-2", net_id),
        "the idempotency key is per-generation (create seeded 1, this update bumped to 2)"
    );
    assert_eq!(requested_by, "u-operator", "the owner is stamped (#386)");
    assert_eq!(generation, 2);

    // The snapshot is persisted (the attach-time mechanism reads it).
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT firewall_rules_json FROM network_desired_state WHERE network_id = ?",
    )
    .bind(&net_id)
    .fetch_one(&state.pool)
    .await
    .expect("query stored policy");
    assert!(
        stored.unwrap_or_default().contains("icmp"),
        "the rules must be persisted for the attach-time path to pick up"
    );

    // DP6's detail half: the network's `last_task` resolves to the
    // policy operation (the detail query keys on resource_kind
    // 'network').
    let (status, detail) = post_with_token(
        state.clone(),
        "/v1/networks/get",
        &token,
        &format!(r#"{{"network_id":"{net_id}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "detail body: {detail}");
    assert_eq!(
        detail["detail"]["last_task"].as_str(),
        Some("UpdateNetworkPolicy"),
        "the detail's last_task names the policy operation: {detail}"
    );
}

#[tokio::test]
async fn update_network_clearing_rules_journals_the_baseline_task() {
    // The clear story (#355 PR 3, DP5): `[]` is a real mutation — it
    // journals an `UpdateNetworkPolicy` task that dispatches the DP4
    // BASELINE (DHCP/DNS/conntrack allows + default-deny) to every
    // attached node, and the response's summary says so: a cleared
    // network is a live, FILTERED network, not a teardown and not the
    // pre-baseline stale-policy residual.
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;
    let net_id = create_network(&state, &token, "tenant-a", "10.99.0.0/24").await;

    // First apply a real ruleset...
    let (status, body) = post_with_token(
        state.clone(),
        "/v1/networks/update",
        &token,
        &format!(
            r#"{{"network_id":"{net_id}","firewall_rules":[{{"direction":"inbound","action":"accept","protocol":"icmp"}}]}}"#
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "apply body: {body}");
    assert!(
        body["task_id"].as_str().is_some(),
        "the apply carries a task: {body}"
    );

    // ...then clear it.
    let (status, body) = post_with_token(
        state.clone(),
        "/v1/networks/update",
        &token,
        &format!(r#"{{"network_id":"{net_id}","firewall_rules":[]}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "clear body: {body}");
    let summary = body["summary"]
        .as_str()
        .unwrap_or_else(|| panic!("the clear carries its own task summary: {body}"));
    assert!(
        summary.contains("cleared"),
        "the summary must identify the update as a clear: {summary}"
    );
    assert!(
        summary.contains("baseline"),
        "the summary must name the DP5 semantics (baseline-only, not a teardown, not a stale residual): {summary}"
    );
    assert!(
        summary.contains("applies on every node"),
        "the summary states the dispatch semantics: {summary}"
    );
    // The clear journaled its own per-generation task.
    let ops: Vec<(String, String)> = sqlx::query_as(
        "SELECT operation_type, idempotency_key FROM operations \
         WHERE resource_id = ? ORDER BY desired_generation",
    )
    .bind(&net_id)
    .fetch_all(&state.pool)
    .await
    .expect("query network ops");
    assert_eq!(ops.len(), 2, "apply + clear each mint a task: {ops:?}");
    assert!(
        ops.iter().all(|(t, _)| t == "UpdateNetworkPolicy"),
        "both tasks are policy operations: {ops:?}"
    );
    assert_eq!(ops[0].1, format!("update-network-policy-{}-2", net_id));
    assert_eq!(ops[1].1, format!("update-network-policy-{}-3", net_id));
}

#[tokio::test]
async fn update_network_without_firewall_fields_journals_nothing() {
    // DP1's other half: only firewall-field updates mint operations —
    // a name-only or NDS-fields-only update keeps the detail response
    // it always returned and journals zero rows (it dispatches
    // nothing; bumping the generation alone must not mint a task).
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;
    let net_id = create_network(&state, &token, "tenant-a", "10.99.0.0/24").await;

    // Name-only: the read-after-write detail response, no task keys.
    let (status, body) = post_with_token(
        state.clone(),
        "/v1/networks/update",
        &token,
        &format!(r#"{{"network_id":"{net_id}","name":"renamed"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "name-only body: {body}");
    assert!(
        body.get("task_id").is_none() && body.get("accepted").is_none(),
        "a name-only update returns the detail shape, not the task shape: {body}"
    );
    assert_eq!(
        body["detail"]["network_id"].as_str(),
        Some(net_id.as_str()),
        "the detail shape still carries network_id: {body}"
    );

    // NDS-fields-only (cidr): the detail shape too, generation bumped,
    // still zero operations.
    let (status, body) = post_with_token(
        state.clone(),
        "/v1/networks/update",
        &token,
        &format!(r#"{{"network_id":"{net_id}","cidr":"10.99.1.0/24"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "cidr-only body: {body}");
    assert!(
        body.get("task_id").is_none(),
        "a cidr-only update mints no task: {body}"
    );
    let (ops, generation): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM operations WHERE resource_id = ?), \
                desired_generation FROM network_desired_state WHERE network_id = ?",
    )
    .bind(&net_id)
    .bind(&net_id)
    .fetch_one(&state.pool)
    .await
    .expect("query ops and generation");
    assert_eq!(ops, 0, "no operations journaled by non-firewall updates");
    assert_eq!(
        generation, 2,
        "the cidr update bumped the generation without minting a task"
    );
}

#[tokio::test]
async fn update_network_rejects_non_array_firewall_rules() {
    // A non-array firewall_rules (JSON null, a scalar, an object) would
    // be stored verbatim and dispatched verbatim to nwd at attach time,
    // where it fails rule parsing and bricks every subsequent VM create
    // on the network. Reject at the API boundary instead.
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;
    let net_id = create_network(&state, &token, "tenant-a", "10.99.0.0/24").await;

    for bad in ["null", "\"allow-all\"", "{\"rules\":[]}"] {
        let (status, body) = post_with_token(
            state.clone(),
            "/v1/networks/update",
            &token,
            &format!(r#"{{"network_id":"{net_id}","firewall_rules":{bad}}}"#),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "non-array firewall_rules ({bad}) must be rejected: {body}"
        );
        assert!(
            body["message"]
                .as_str()
                .unwrap_or_default()
                .contains("firewall_rules must be an array"),
            "the rejection must name the field: {body}"
        );
    }

    // Nothing was stored by the rejected updates.
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT firewall_rules_json FROM network_desired_state WHERE network_id = ?",
    )
    .bind(&net_id)
    .fetch_one(&state.pool)
    .await
    .expect("query stored policy");
    assert!(
        stored.as_deref().unwrap_or("").is_empty(),
        "rejected payloads must not persist: {stored:?}"
    );
}

#[tokio::test]
async fn update_network_rejects_ui_dialect_and_malformed_rules() {
    // N7's trigger (M4.4 re-qualification, #368): a rule authored in the
    // UI-era dialect — `direction: "ingress"`, `action: "allow"`, the
    // `source` field — was persisted verbatim, rode the VM spec (#355),
    // passed `set_firewall_policy`, and then detonated in nwd's
    // attach-time policy-guard refresh: the VM create failed
    // RUNTIME_UNAVAILABLE with no operator-visible cause. The engine
    // vocabulary is the contract; every dialect or malformed shape is
    // rejected at the API boundary with a message that teaches the
    // mapping.
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;
    let net_id = create_network(&state, &token, "tenant-a", "10.99.0.0/24").await;

    // (label, rule JSON, substring the 400 must contain)
    let bad_rules: &[(&str, &str, &str)] = &[
        (
            "UI dialect direction",
            r#"{"direction":"ingress","action":"accept","protocol":"icmp"}"#,
            "ingress",
        ),
        (
            "UI dialect action",
            r#"{"direction":"inbound","action":"allow","protocol":"icmp"}"#,
            "allow",
        ),
        (
            "UI dialect source field",
            r#"{"direction":"inbound","action":"accept","protocol":"icmp","source":"10.99.0.0/24"}"#,
            "source",
        ),
        (
            "UI dialect port field",
            r#"{"direction":"inbound","action":"accept","protocol":"tcp","port_range":"8080-8090"}"#,
            "port_range",
        ),
        (
            "unknown protocol",
            r#"{"direction":"inbound","action":"accept","protocol":"gre"}"#,
            "protocol",
        ),
        (
            "missing action",
            r#"{"direction":"inbound","protocol":"icmp"}"#,
            "action",
        ),
        (
            "malformed source cidr",
            r#"{"direction":"inbound","action":"accept","protocol":"icmp","source_cidr":"10.99.0.0"}"#,
            "source_cidr",
        ),
        (
            "service-name port",
            r#"{"direction":"inbound","action":"accept","protocol":"tcp","dest_port":"ssh"}"#,
            "dest_port",
        ),
        ("non-object rule", r#""allow all""#, "must be an object"),
        (
            "non-string optional field (source_cidr as number)",
            r#"{"direction":"inbound","action":"accept","protocol":"icmp","source_cidr":123}"#,
            "must be a string",
        ),
        (
            "non-string required field (direction as number)",
            r#"{"direction":5,"action":"accept","protocol":"icmp"}"#,
            "must be a string",
        ),
    ];
    for (label, rule, expected_fragment) in bad_rules {
        let (status, body) = post_with_token(
            state.clone(),
            "/v1/networks/update",
            &token,
            &format!(r#"{{"network_id":"{net_id}","firewall_rules":[{rule}]}}"#),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{label} must be rejected: {body}"
        );
        assert!(
            body["message"]
                .as_str()
                .unwrap_or_default()
                .contains(expected_fragment),
            "{label} rejection must name the offender ({expected_fragment}): {body}"
        );
    }

    // The create path shares the same gate.
    let (status, body) = post_with_token(
        state.clone(),
        "/v1/networks/create",
        &token,
        r#"{"name":"tenant-b","cidr":"10.98.0.0/24","firewall_rules":[{"direction":"egress","action":"deny","protocol":"tcp"}]}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "dialect rules must be rejected at create too: {body}"
    );

    // Nothing was stored by the rejected payloads.
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT firewall_rules_json FROM network_desired_state WHERE network_id = ?",
    )
    .bind(&net_id)
    .fetch_one(&state.pool)
    .await
    .expect("query stored policy");
    assert!(
        stored.as_deref().unwrap_or("").is_empty(),
        "rejected payloads must not persist: {stored:?}"
    );
}

#[tokio::test]
async fn update_network_accepts_full_engine_vocabulary() {
    // The positive shape: every optional field, both directions, all
    // port forms — accepted and persisted verbatim for the attach-time
    // snapshot.
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;
    let net_id = create_network(&state, &token, "tenant-a", "10.99.0.0/24").await;

    let (status, body) = post_with_token(
        state.clone(),
        "/v1/networks/update",
        &token,
        &format!(
            r#"{{"network_id":"{net_id}","firewall_rules":[
                {{"direction":"inbound","action":"accept","protocol":"tcp","source_cidr":"10.99.0.0/24","dest_port":"443"}},
                {{"direction":"inbound","action":"accept","protocol":"udp","dest_port":"53"}},
                {{"direction":"outbound","action":"drop","protocol":"all"}},
                {{"direction":"outbound","action":"reject","protocol":"sctp","source_cidr":"fd00::/8","dest_port":"8080-8090"}}
            ]}}"#
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "update body: {body}");
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT firewall_rules_json FROM network_desired_state WHERE network_id = ?",
    )
    .bind(&net_id)
    .fetch_one(&state.pool)
    .await
    .expect("query stored policy");
    let stored = stored.unwrap_or_default();
    for fragment in ["\"443\"", "\"53\"", "fd00::/8", "8080-8090"] {
        assert!(
            stored.contains(fragment),
            "engine-vocabulary rule field {fragment} must persist verbatim: {stored}"
        );
    }
}

#[tokio::test]
async fn update_network_without_policy_fields_carries_no_note() {
    let state = build_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;
    let net_id = create_network(&state, &token, "tenant-a", "10.99.0.0/24").await;

    let (status, body) = post_with_token(
        state.clone(),
        "/v1/networks/update",
        &token,
        &format!(r#"{{"network_id":"{net_id}","name":"renamed"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "update body: {body}");
    assert!(
        body.get("policy_application").is_none(),
        "no policy note when no policy field was updated: {body}"
    );
    assert_eq!(
        body["detail"]["name"].as_str(),
        Some("renamed"),
        "rename applied: {body}"
    );
}

#[tokio::test]
async fn concurrent_policy_updates_journal_distinct_per_generation_tasks() {
    // Round-2 review pin (the quota-race suite's shape): concurrent
    // firewall updates through the real router must serialize — every
    // mutation mints its own per-generation task (the generation is
    // read IN the transaction via UPDATE...RETURNING), no request
    // fails on lock contention, and the generation advances exactly
    // once per update; the UNIQUE(idempotency_key) backstop never
    // fires. (BEGIN IMMEDIATE additionally takes the write lock up
    // front, keeping the shape safe if a read ever lands inside the
    // transaction before the write — the SQLITE_BUSY_SNAPSHOT
    // upgrade class; this test does not discriminate that mode, it
    // pins the observable contract.) Requires the race pool: the
    // single-connection in-memory harness serializes at the pool
    // before SQLite ever sees a concurrent writer.
    let (_dir, state) = build_race_state().await;
    let token = seed_jwt(&state).await;
    seed_node(&state).await;
    let net_id = create_network(&state, &token, "tenant-a", "10.99.0.0/24").await;

    const ATTEMPTS: usize = 8;
    let mut handles = Vec::with_capacity(ATTEMPTS);
    for _ in 0..ATTEMPTS {
        let state = state.clone();
        let token = token.clone();
        let net_id = net_id.clone();
        handles.push(tokio::spawn(async move {
            post_with_token(
                state,
                "/v1/networks/update",
                &token,
                &format!(
                    r#"{{"network_id":"{net_id}","firewall_rules":[{{"direction":"inbound","action":"accept","protocol":"icmp"}}]}}"#
                ),
            )
            .await
        }));
    }
    let mut task_ids: Vec<String> = Vec::with_capacity(ATTEMPTS);
    for h in handles {
        let (status, body) = h.await.expect("join update task");
        assert_eq!(status, StatusCode::OK, "concurrent update body: {body}");
        task_ids.push(
            body["task_id"]
                .as_str()
                .unwrap_or_else(|| panic!("every concurrent update answers with its task: {body}"))
                .to_string(),
        );
    }
    let distinct: std::collections::HashSet<&String> = task_ids.iter().collect();
    assert_eq!(
        distinct.len(),
        ATTEMPTS,
        "every mutation mints its own operation id"
    );

    // The observable contract: N ops with per-generation keys -2..-N+1,
    // no UNIQUE(idempotency_key) collision (the silent-duplication
    // backstop), generation advanced exactly N times.
    let keys: Vec<String> = sqlx::query_scalar(
        "SELECT idempotency_key FROM operations WHERE resource_id = ? ORDER BY desired_generation",
    )
    .bind(&net_id)
    .fetch_all(&state.pool)
    .await
    .expect("query race ops");
    assert_eq!(keys.len(), ATTEMPTS, "all concurrent updates journaled");
    for (i, key) in keys.iter().enumerate() {
        assert_eq!(
            key,
            &format!("update-network-policy-{}-{}", net_id, i + 2),
            "the serialized writers mint consecutive per-generation keys"
        );
    }
    let generation: i64 = sqlx::query_scalar(
        "SELECT desired_generation FROM network_desired_state WHERE network_id = ?",
    )
    .bind(&net_id)
    .fetch_one(&state.pool)
    .await
    .expect("query generation");
    assert_eq!(
        generation,
        ATTEMPTS as i64 + 1,
        "the generation advanced exactly once per serialized update (create seeded 1)"
    );
}
