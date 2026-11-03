//! Integration tests for the native alerting API (query/alerts
//! contract v1, #602 PR-6): `POST /v1/monitoring/{alerts,
//! alerts/detail, alert-rules, alert-rules/create, alert-rules/update,
//! alert-rules/delete, alerts/acknowledge, alerts/silence,
//! notifications/deliveries, notifications/test}`.
//!
//! These boot the real `bff_router` and pin the PR-6 wire rules
//! end-to-end:
//!
//! - the rule wire shape is the contract's FLAT typed-rule example:
//!   the spec fields ride at the top level and are parsed strictly
//!   (a typo'd operator or an unknown field is a typed 400, never a
//!   silent reinterpretation as another rule shape);
//! - every `metric_id` is registry-validated at creation — a rule on a
//!   nonexistent metric is a `unknown_metric` 400, not a silent
//!   never-firing trap — and so is every `dimension_match` key
//!   (`unknown_dimension`): a typo'd key matches no series;
//! - rule mutations are revision-preconditioned (a stale replay is a
//!   409 conflict and changes nothing);
//! - incidents opened by the evaluator are visible to the viewer
//!   role; acknowledgment and silence are operator-gated overlays
//!   that never resolve anything;
//! - the delivery audit lists outbox events; the authorized delivery
//!   test enqueues a real event and refuses honestly when no
//!   destination is configured.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chv_common::SystemClock;
use chv_controlplane_store::{
    AlertRepository, AlertRuleRepository, ApplyRunRepository, BackupRepository,
    DesiredStateRepository, DriftReportRepository, EventRepository, ImageRepository,
    IncidentOpenInput, NetworkRepository, NodeRepository, NotificationOutboxRepository,
    ObservedStateRepository, OperationRepository, TopologyRepository,
};
use chv_webui_bff::mutations::MutationService;
use chv_webui_bff::{AppState, BffError};
use serde_json::{json, Value};
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

    AppState {
        pool: pool.clone(),
        node_repo: NodeRepository::new(pool.clone()),
        operation_repo: OperationRepository::new(pool.clone()),
        event_repo: EventRepository::new(pool.clone()),
        alert_repo: AlertRepository::new(pool.clone()),
        alert_rules: Arc::new(AlertRuleRepository::new(pool.clone())),
        notification_outbox: Arc::new(NotificationOutboxRepository::new(pool.clone())),
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
        agent_runtime_dir: std::path::PathBuf::from("/var/lib/chv/agent"),
        cache: chv_webui_bff::BffCache::new(5),
        clock: Arc::new(SystemClock),
        monitoring: None,
        monitoring_health: chv_webui_bff::MonitoringHealth::new(),
    }
}

/// Seed a user with the given role and return a usable JWT bearer
/// token.
async fn seed_jwt_as(state: &AppState, role: &str) -> String {
    sqlx::query(
        "INSERT INTO users (user_id, username, password_hash, role, must_change_password) \
         VALUES (?, ?, 'x', ?, 0)",
    )
    .bind(format!("u-{role}"))
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
        sub: format!("u-{role}"),
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

async fn request(
    state: &AppState,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let req = match body {
        Some(b) => builder
            .header("content-type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = chv_webui_bff::bff_router(state.clone())
        .with_state(state.clone())
        .oneshot(req)
        .await
        .expect("route request");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, json)
}

/// A flat threshold-rule body in the contract's example shape.
fn threshold_rule_body() -> Value {
    json!({
        "name": "Node CPU pressure",
        "target_kind": "node",
        "target_id": "node-1",
        "metric_id": "node.cpu.capacity_ratio",
        "operator": "greater_than",
        "threshold": 0.9,
        "severity": "warning",
        "for_seconds": 300,
        "recovery_seconds": 120,
        "missing_data": "unknown",
    })
}

async fn open_incident(state: &AppState, dedup_key: &str) -> String {
    let alert_id = state
        .alert_repo
        .open_pending(&IncidentOpenInput {
            rule_id: "rule-1".into(),
            rule_revision: 1,
            dedup_key: dedup_key.into(),
            severity: "warning".into(),
            target_kind: "node".into(),
            target_id: "node-1".into(),
            node_id: None,
            message: "CPU pressure above 0.90 (rule 'Node CPU pressure')".into(),
            now_ms: 1_000_000,
            last_observed: Some("0.94 (node.cpu.capacity_ratio)".into()),
            evidence_from_ms: 900_000,
            evidence_to_ms: 1_000_000,
        })
        .await
        .expect("open incident");
    state
        .alert_repo
        .promote_to_firing(
            &alert_id,
            1_200_000,
            Some("0.95 (x)"),
            1_100_000,
            1_200_000,
            &[],
        )
        .await
        .expect("promote");
    alert_id
}

// ---------------------------------------------------------------------------
// Rule CRUD
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rule_crud_lifecycle_with_revision_preconditions() {
    let state = build_state().await;
    let operator = seed_jwt_as(&state, "operator").await;

    // Create: the flat body parses to a typed threshold rule.
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/alert-rules/create",
        Some(&operator),
        Some(threshold_rule_body()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rule = &body["rule"];
    assert_eq!(rule["rule_type"], "threshold");
    assert_eq!(rule["revision"], 1);
    assert_eq!(rule["operator"], "greater_than");
    assert_eq!(rule["threshold"], 0.9);
    assert_eq!(rule["metric_id"], "node.cpu.capacity_ratio");
    let rule_id = rule["rule_id"].as_str().unwrap().to_string();

    // Update with the current revision.
    let mut update = threshold_rule_body();
    update["name"] = "Node CPU pressure v2".into();
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/alert-rules/update",
        Some(&operator),
        Some(json!({
            "rule_id": rule_id,
            "expected_revision": 1,
            "name": "Node CPU pressure v2",
            "target_kind": "node",
            "target_id": "node-1",
            "metric_id": "node.cpu.capacity_ratio",
            "operator": "greater_than",
            "threshold": 0.8,
            "severity": "critical",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rule"]["revision"], 2);
    assert_eq!(body["rule"]["severity"], "critical");

    // A stale replay is a 409 and changes nothing.
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/alert-rules/update",
        Some(&operator),
        Some(json!({
            "rule_id": rule_id,
            "expected_revision": 1,
            "name": "stale replay",
            "target_kind": "node",
            "target_id": "node-1",
            "metric_id": "node.cpu.capacity_ratio",
            "operator": "greater_than",
            "threshold": 0.5,
            "severity": "warning",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // List (viewer tier) reflects the update.
    let viewer = seed_jwt_as(&state, "viewer").await;
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/alert-rules",
        Some(&viewer),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], 1);
    assert_eq!(body["rules"][0]["name"], "Node CPU pressure v2");

    // Delete: stale revision 409, current revision succeeds.
    let (status, _) = request(
        &state,
        "POST",
        "/v1/monitoring/alert-rules/delete",
        Some(&operator),
        Some(json!({"rule_id": rule_id, "expected_revision": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/alert-rules/delete",
        Some(&operator),
        Some(json!({"rule_id": rule_id, "expected_revision": 2})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deleted"], true);
    assert_eq!(body["retired_incidents"], 0, "no incidents were open");
}

#[tokio::test]
async fn deleting_a_rule_retires_its_active_incidents() {
    // A deleted rule's firing incident would otherwise be a permanent
    // ghost: firing forever, counted in every badge, with no recovery
    // path — the rule that could recover it is gone. Deletion retires
    // it (resolved, with the honest reason and a resolved
    // notification); a pending incident (never fired) is deleted.
    let mut state = build_state().await;
    state.notification_channels = chv_webui_bff::NotificationChannels {
        webhook: true,
        slack: false,
    };
    let operator = seed_jwt_as(&state, "operator").await;

    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/alert-rules/create",
        Some(&operator),
        Some(threshold_rule_body()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rule_id = body["rule"]["rule_id"].as_str().unwrap().to_string();

    // Seed incidents as the evaluator would: one firing, one pending.
    let firing_id = state
        .alert_repo
        .open_pending(&IncidentOpenInput {
            rule_id: rule_id.clone(),
            rule_revision: 1,
            dedup_key: format!("{rule_id}:node:node-1:-"),
            severity: "warning".into(),
            target_kind: "node".into(),
            target_id: "node-1".into(),
            node_id: None,
            message: "CPU pressure above 0.90 (rule 'Node CPU pressure')".into(),
            now_ms: 1_000,
            last_observed: Some("0.95 (node.cpu.capacity_ratio)".into()),
            evidence_from_ms: 0,
            evidence_to_ms: 1_000,
        })
        .await
        .expect("open firing");
    state
        .alert_repo
        .promote_to_firing(
            &firing_id,
            2_000,
            Some("0.95 (node.cpu.capacity_ratio)"),
            0,
            2_000,
            &[],
        )
        .await
        .expect("promote");
    let pending_id = state
        .alert_repo
        .open_pending(&IncidentOpenInput {
            rule_id: rule_id.clone(),
            rule_revision: 1,
            dedup_key: format!("{rule_id}:node:node-2:-"),
            severity: "warning".into(),
            target_kind: "node".into(),
            target_id: "node-2".into(),
            node_id: None,
            message: "CPU pressure above 0.90 (rule 'Node CPU pressure')".into(),
            now_ms: 1_000,
            last_observed: Some("0.95 (node.cpu.capacity_ratio)".into()),
            evidence_from_ms: 0,
            evidence_to_ms: 1_000,
        })
        .await
        .expect("open pending");

    // Delete the rule through the BFF.
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/alert-rules/delete",
        Some(&operator),
        Some(json!({"rule_id": rule_id, "expected_revision": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deleted"], true);
    assert_eq!(body["retired_incidents"], 1, "exactly the firing one");

    // The firing incident resolved with the honest reason, and the
    // resolved notification is enqueued on the configured channel.
    let resolved = state
        .alert_repo
        .get_incident(&firing_id)
        .await
        .expect("get");
    assert_eq!(resolved.status, "resolved");
    let transitions = state
        .alert_repo
        .list_transitions(&firing_id, 10)
        .await
        .expect("transitions");
    let last = transitions.last().expect("at least one transition");
    assert_eq!(last.to_state, "resolved");
    assert_eq!(last.reason, "rule deleted");
    let events = state
        .notification_outbox
        .list_recent(10)
        .await
        .expect("outbox");
    let resolved_events: Vec<_> = events
        .iter()
        .filter(|e| e.alert_id == firing_id && e.event_type == "resolved")
        .collect();
    assert_eq!(resolved_events.len(), 1, "one resolved event per channel");
    assert_eq!(resolved_events[0].channel, "webhook");

    // The pending incident (never fired) is gone entirely.
    assert!(
        state.alert_repo.get_incident(&pending_id).await.is_err(),
        "pending incidents are deleted, not resolved"
    );
}

#[tokio::test]
async fn editing_a_rules_dimension_match_retires_the_old_key_incident() {
    // The dedup key includes the dimension match: editing it strands
    // the old key's active incident (the evaluator only ever looks
    // up the CURRENT key). The update retires it, same semantics as
    // deletion.
    let state = build_state().await;
    let operator = seed_jwt_as(&state, "operator").await;

    let mut create = threshold_rule_body();
    create["name"] = "VM guest storage".into();
    create["target_kind"] = "vm".into();
    create["target_id"] = "vm-1".into();
    create["metric_id"] = "vm.guest.fs.available_bytes".into();
    create["threshold"] = 2_147_483_648.0.into();
    create["operator"] = "less_than".into();
    create["dimension_match"] = json!({"mount_id": "/"});
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/alert-rules/create",
        Some(&operator),
        Some(create),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rule_id = body["rule"]["rule_id"].as_str().unwrap().to_string();
    let old_key = format!("{rule_id}:vm:vm-1:{{\"mount_id\":\"/\"}}");

    let alert_id = state
        .alert_repo
        .open_pending(&IncidentOpenInput {
            rule_id: rule_id.clone(),
            rule_revision: 1,
            dedup_key: old_key.clone(),
            severity: "warning".into(),
            target_kind: "vm".into(),
            target_id: "vm-1".into(),
            node_id: None,
            message: "Guest storage below 2 GiB (rule 'VM guest storage')".into(),
            now_ms: 1_000,
            last_observed: Some("1073741824 (vm.guest.fs.available_bytes)".into()),
            evidence_from_ms: 0,
            evidence_to_ms: 1_000,
        })
        .await
        .expect("open");
    state
        .alert_repo
        .promote_to_firing(&alert_id, 2_000, None, 0, 2_000, &[])
        .await
        .expect("promote");

    // Edit the dimension match (the dedup key changes).
    let mut update = threshold_rule_body();
    update["name"] = "VM guest storage".into();
    update["metric_id"] = "vm.guest.fs.available_bytes".into();
    update["target_kind"] = "vm".into();
    update["target_id"] = "vm-1".into();
    update["threshold"] = 2_147_483_648.0.into();
    update["operator"] = "less_than".into();
    update["dimension_match"] = json!({"mount_id": "/var"});
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/alert-rules/update",
        Some(&operator),
        Some(json!({
            "rule_id": rule_id,
            "expected_revision": 1,
            "name": "VM guest storage",
            "target_kind": "vm",
            "target_id": "vm-1",
            "metric_id": "vm.guest.fs.available_bytes",
            "operator": "less_than",
            "threshold": 2147483648.0,
            "severity": "warning",
            "dimension_match": {"mount_id": "/var"},
        })),
    )
    .await;
    let _ = update;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The old key's incident resolved with the honest reason.
    let resolved = state.alert_repo.get_incident(&alert_id).await.expect("get");
    assert_eq!(resolved.status, "resolved");
    let transitions = state
        .alert_repo
        .list_transitions(&alert_id, 10)
        .await
        .expect("transitions");
    let last = transitions.last().expect("at least one transition");
    assert_eq!(last.reason, "rule edited to a different dimension match");
}

#[tokio::test]
async fn rule_spec_validation_is_strict() {
    let state = build_state().await;
    let operator = seed_jwt_as(&state, "operator").await;

    // An unknown metric is a typed unknown_metric 400 — never a
    // silent never-firing rule.
    let mut body = threshold_rule_body();
    body["metric_id"] = "not.a.metric".into();
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/alert-rules/create",
        Some(&operator),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "UNKNOWN_METRIC");

    // A dimension_match key the metric does not declare is a typed
    // unknown_dimension 400 — a typo'd key would match no series:
    // a silent never-firing threshold (or a PERMANENTLY firing
    // availability rule).
    let mut body = threshold_rule_body();
    body["dimension_match"] = serde_json::json!({"mountpath": "/"});
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/alert-rules/create",
        Some(&operator),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "UNKNOWN_DIMENSION");

    // The registry-declared dimension passes (vm.guest.fs metrics
    // carry mount_id).
    let mut body = threshold_rule_body();
    body["name"] = "VM guest storage".into();
    body["target_kind"] = "vm".into();
    body["target_id"] = "vm-1".into();
    body["metric_id"] = "vm.guest.fs.available_bytes".into();
    body["dimension_match"] = serde_json::json!({"mount_id": "/"});
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/alert-rules/create",
        Some(&operator),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A typo'd operator must not silently parse as another shape.
    let mut body = threshold_rule_body();
    body["operator"] = "moar".into();
    let (status, _) = request(
        &state,
        "POST",
        "/v1/monitoring/alert-rules/create",
        Some(&operator),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Unknown spec fields are rejected.
    let mut body = threshold_rule_body();
    body["surprise"] = true.into();
    let (status, _) = request(
        &state,
        "POST",
        "/v1/monitoring/alert-rules/create",
        Some(&operator),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A one-level group over registry-known metrics creates cleanly
    // and derives rule_type "group".
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/alert-rules/create",
        Some(&operator),
        Some(json!({
            "name": "Node under pressure",
            "target_kind": "node",
            "target_id": "node-2",
            "op": "and",
            "conditions": [
                {"metric_id": "node.cpu.capacity_ratio", "operator": "greater_than", "threshold": 0.9},
                {"metric_id": "node.memory.available_bytes", "operator": "less_than", "threshold": 1e9}
            ],
            "severity": "critical",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rule"]["rule_type"], "group");
    assert_eq!(body["rule"]["op"], "and");

    // Nested groups are refused.
    let (status, _) = request(
        &state,
        "POST",
        "/v1/monitoring/alert-rules/create",
        Some(&operator),
        Some(json!({
            "name": "nested",
            "target_kind": "node",
            "target_id": "node-2",
            "op": "and",
            "conditions": [
                {"op": "or", "conditions": [
                    {"metric_id": "node.cpu.capacity_ratio", "operator": "greater_than", "threshold": 0.9}
                ]}
            ],
            "severity": "warning",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn rules_can_be_created_disabled_and_updates_keep_the_state() {
    // The create-from-template flow depends on this: templates never
    // auto-enable, and a later partial update that omits `enabled`
    // must not silently re-enable a disabled rule.
    let state = build_state().await;
    let operator = seed_jwt_as(&state, "operator").await;

    let mut body = threshold_rule_body();
    body["name"] = "From template".into();
    body["enabled"] = false.into();
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/alert-rules/create",
        Some(&operator),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rule"]["enabled"], false);
    let rule_id = body["rule"]["rule_id"].as_str().unwrap().to_string();

    // An update without `enabled` keeps the disabled state.
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/alert-rules/update",
        Some(&operator),
        Some(json!({
            "rule_id": rule_id,
            "expected_revision": 1,
            "name": "Reviewed template",
            "target_kind": "node",
            "target_id": "node-1",
            "metric_id": "node.cpu.capacity_ratio",
            "operator": "greater_than",
            "threshold": 0.9,
            "severity": "warning",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["rule"]["enabled"], false,
        "omitting enabled keeps the disabled state"
    );

    // The disabled rule is excluded from the enabled-only listing.
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/alert-rules",
        Some(&operator),
        Some(json!({"enabled_only": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], 0);

    // Enabling is explicit.
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/alert-rules/update",
        Some(&operator),
        Some(json!({
            "rule_id": rule_id,
            "expected_revision": 2,
            "name": "Reviewed template",
            "target_kind": "node",
            "target_id": "node-1",
            "metric_id": "node.cpu.capacity_ratio",
            "operator": "greater_than",
            "threshold": 0.9,
            "severity": "warning",
            "enabled": true,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rule"]["enabled"], true);
}

#[tokio::test]
async fn rule_ceiling_is_a_loud_conflict() {
    let state = build_state().await;
    let mut state = state;
    state.alerting_max_rules = 1;
    let operator = seed_jwt_as(&state, "operator").await;

    let (status, _) = request(
        &state,
        "POST",
        "/v1/monitoring/alert-rules/create",
        Some(&operator),
        Some(threshold_rule_body()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let mut body = threshold_rule_body();
    body["name"] = "second rule".into();
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/alert-rules/create",
        Some(&operator),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["message"].as_str().unwrap_or("").contains("ceiling"),
        "{body}"
    );
}

// ---------------------------------------------------------------------------
// Incidents
// ---------------------------------------------------------------------------

#[tokio::test]
async fn incidents_are_visible_and_acknowledge_silence_are_overlays() {
    let state = build_state().await;
    let viewer = seed_jwt_as(&state, "viewer").await;
    let operator = seed_jwt_as(&state, "operator").await;
    let alert_id = open_incident(&state, "rule-1:node:node-1:-").await;

    // Viewer sees the firing incident.
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/alerts",
        Some(&viewer),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], 1);
    assert_eq!(body["incidents"][0]["status"], "firing");
    assert_eq!(body["incidents"][0]["alert_id"], alert_id);
    assert_eq!(body["incidents"][0]["target_kind"], "node");
    assert_eq!(body["incidents"][0]["target_id"], "node-1");
    assert_eq!(body["incidents"][0]["rule_id"], "rule-1");

    // Detail carries the transition history.
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/alerts/detail",
        Some(&viewer),
        Some(json!({"alert_id": alert_id})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let states: Vec<&str> = body["transitions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["to_state"].as_str().unwrap())
        .collect();
    assert_eq!(states, vec!["pending", "firing"]);

    // Acknowledge (operator): an overlay that never resolves.
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/alerts/acknowledge",
        Some(&operator),
        Some(json!({"alert_id": alert_id})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["acknowledged"], true);
    let (_, body) = request(
        &state,
        "POST",
        "/v1/monitoring/alerts",
        Some(&viewer),
        Some(json!({})),
    )
    .await;
    assert_eq!(
        body["incidents"][0]["status"], "firing",
        "ack never resolves"
    );
    assert_eq!(body["incidents"][0]["acknowledged_by"], "operator");

    // Silence (operator) with a relative duration.
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/alerts/silence",
        Some(&operator),
        Some(json!({"alert_id": alert_id, "duration_minutes": 30})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["silenced"], true);
    assert!(body["until_ms"].as_i64().unwrap() > 0);

    // A past deadline is refused.
    let (status, _) = request(
        &state,
        "POST",
        "/v1/monitoring/alerts/silence",
        Some(&operator),
        Some(json!({"alert_id": alert_id, "until_ms": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Acknowledging an unknown incident is a 404.
    let (status, _) = request(
        &state,
        "POST",
        "/v1/monitoring/alerts/acknowledge",
        Some(&operator),
        Some(json!({"alert_id": "no-such-incident"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Resolving the incident hides it from the default listing and
    // surfaces it with include_resolved.
    state
        .alert_repo
        .resolve_incident(&alert_id, 2_000_000, "recovered", None, &[])
        .await
        .expect("resolve");
    let (_, body) = request(
        &state,
        "POST",
        "/v1/monitoring/alerts",
        Some(&viewer),
        Some(json!({})),
    )
    .await;
    assert_eq!(body["total"], 0, "resolved incidents are not active");
    let (_, body) = request(
        &state,
        "POST",
        "/v1/monitoring/alerts",
        Some(&viewer),
        Some(json!({"include_resolved": true})),
    )
    .await;
    assert_eq!(body["total"], 1);
    assert_eq!(body["incidents"][0]["status"], "resolved");
}

// ---------------------------------------------------------------------------
// Delivery audit and the authorized delivery test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deliveries_list_and_notification_test_gate() {
    let state = build_state().await;
    let viewer = seed_jwt_as(&state, "viewer").await;
    let admin = seed_jwt_as(&state, "admin").await;

    // An incident with a notification enqueued shows in the audit.
    let alert_id = open_incident(&state, "rule-9:node:node-9:-").await;
    let event_id = uuid::Uuid::new_v4().to_string();
    let payload = chv_monitoring_core::notifications::render_envelope(
        &event_id,
        &alert_id,
        "firing",
        "warning",
        "node",
        "node-9",
        "test summary",
        1_200_000,
        "/nodes/node-9",
    );
    state
        .notification_outbox
        .enqueue(&chv_controlplane_store::NotificationEventInput {
            event_id: event_id.clone(),
            alert_id: alert_id.clone(),
            incident_key: "rule-9:node:node-9:-".into(),
            event_type: "firing".into(),
            severity: "warning".into(),
            target_kind: "node".into(),
            target_id: "node-9".into(),
            summary: "test summary".into(),
            occurred_at_ms: 1_200_000,
            payload,
            channel: "webhook".into(),
        })
        .await
        .expect("enqueue");

    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/notifications/deliveries",
        Some(&viewer),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deliveries"].as_array().unwrap().len(), 1);
    assert_eq!(body["deliveries"][0]["event_id"], event_id);
    assert_eq!(body["deliveries"][0]["status"], "pending");

    // With no destination configured, the delivery test refuses
    // honestly instead of enqueueing a doomed event.
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/notifications/test",
        Some(&admin),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Configured: the test enqueues a real deliverable event.
    let mut state = state;
    state.notification_channels = chv_webui_bff::NotificationChannels {
        webhook: true,
        slack: false,
    };
    let (status, body) = request(
        &state,
        "POST",
        "/v1/monitoring/notifications/test",
        Some(&admin),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["enqueued"], true);
    let test_event_id = body["event_id"].as_str().unwrap().to_string();
    let (_, body) = request(
        &state,
        "POST",
        "/v1/monitoring/notifications/deliveries",
        Some(&viewer),
        Some(json!({})),
    )
    .await;
    let ids: Vec<&str> = body["deliveries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["event_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&test_event_id.as_str()));
}
