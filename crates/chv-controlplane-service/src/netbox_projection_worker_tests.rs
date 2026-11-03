//! NetBox projection worker integration tests — the PR-4 composition
//! root exercised end to end against a wiremock NetBox.
//!
//! These suites drive [`crate::NetboxProjectionWorker::tick`] over a
//! real in-memory store (`TestDb`) and a real HTTP wire (`wiremock`),
//! injecting the adapter's test-only plain-HTTP client constructor via
//! the worker's [`crate::NetboxProjectionWorker::with_client_factory`]
//! seam. Production construction (`NetBoxClient::new`, HTTPS-only) is
//! covered by the adapter's own unit tests.
//!
//! The "remote already mirrors the projection" fixtures are derived
//! from the pure core itself: the test builds the desired objects with
//! [`chv_netbox_adapter::build_objects`] (the same call the worker's
//! runner performs) and renders each one back into NetBox wire JSON, so
//! a re-run produces a provably all-`no_op` plan without hand-maintained
//! fixtures drifting from the mapping contract.

use std::collections::BTreeMap;
use std::sync::Arc;

use chv_architecture_validate::model::{
    CHVArchitecture, Instance, InstanceNetwork, InstancePlacement, InstanceResources, Metadata,
    Network, NetworkType, Server, ServerResources,
};
use chv_controlplane_store::{
    test_util::TestDb, ApplyRunCreateInput, ApplyRunRepository, EventRepository,
    NetboxProjectionConfigRepository, NetboxProjectionConfigUpsertInput,
    NetboxProjectionRunCreateInput, NetboxProjectionRunRepository, TopologyCreateInput,
    TopologyRepository, VersionCreateInput, VersionRepository,
};
use chv_controlplane_types::architecture::{
    ArchitectureApplyRunId, ArchitectureId, ArchitectureStatus, ArchitectureVersionId,
    NetboxProjectionMode, NetboxProjectionRunId, NetboxProjectionRunStatus,
    NetboxProjectionTrigger, NetboxRetentionPolicy, RunStatus,
};
use chv_netbox_adapter::{
    build_objects, NetBoxClient, NetBoxKind, NetBoxObject, NetboxEntryStatus, NetboxPlanAction,
    NetboxProjectionOutcome, NetboxProjectionPlan, ProjectionConfigView, ProjectionInput,
};
use serde_json::{json, Value};
use wiremock::matchers::{method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::NetboxProjectionWorker;

/// Distinctive plaintext token; the redaction suites assert it never
/// reaches a log line, a persisted error message, or an event payload.
pub(crate) const TOKEN: &str = "netbox-secret-token-do-not-log-3f9a";
/// Site label used by every config fixture (must match
/// [`desired_objects`] so the mirrored device content compares equal).
pub(crate) const SITE: &str = "dc1";

/// Create (POST) mocks per kind path: (path, netbox id returned).
pub(crate) const CREATE_MOCKS: [(&str, i64); 6] = [
    ("/api/ipam/vlans/", 101),
    ("/api/ipam/prefixes/", 102),
    ("/api/ipam/ip-addresses/", 103),
    ("/api/virtualization/interfaces/", 104),
    ("/api/virtualization/virtual-machines/", 105),
    ("/api/dcim/devices/", 106),
];

// ---------------------------------------------------------------------------
// Minimal `tracing` subscriber capturing every field of INFO/WARN/ERROR
// events (mirroring the LogCollector convention in
// `chv-controlplane-store`'s `credential_crypto` tests, but recording all
// fields so redaction assertions cover structured values, not just
// messages).
// ---------------------------------------------------------------------------
mod log_capture {
    use std::sync::{Arc, Mutex as StdMutex};

    use tracing::field::Visit;
    use tracing::span::{Attributes, Id};
    use tracing::{Event, Level, Metadata};

    #[derive(Clone, Default)]
    pub struct LogCollector {
        events: Arc<StdMutex<Vec<String>>>,
    }

    impl LogCollector {
        /// Every captured event rendered as `field=value` pairs.
        pub fn messages(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }

        /// The process-wide collector, installed as the tracing global
        /// default exactly once.
        ///
        /// Why global rather than `set_default`: `tracing` caches
        /// per-callsite interest process-wide, and the cache is
        /// computed against the dispatchers registered when the
        /// callsite first executes. With per-thread `set_default`, a
        /// callsite first executed by a *parallel* test — with no
        /// collector installed — can be cached as never for this
        /// collector too, silently dropping the event (a known
        /// `set_default` + parallel-test race, tokio-rs/tracing#1097).
        /// A global default covers every thread, and installing it
        /// rebuilds the interest cache for callsites that already
        /// executed, so the worker's warnings are observable
        /// deterministically.
        pub fn global() -> LogCollector {
            static COLLECTOR: std::sync::OnceLock<LogCollector> = std::sync::OnceLock::new();
            COLLECTOR
                .get_or_init(|| {
                    let collector = LogCollector::default();
                    // Installing the global default also rebuilds the
                    // callsite-interest cache, so warnings that already
                    // executed under the no-op default become observable.
                    tracing::subscriber::set_global_default(collector.clone())
                        .expect("test binary owns the tracing global default");
                    collector
                })
                .clone()
        }
    }

    struct FieldVisitor(Vec<String>);

    impl Visit for FieldVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.push(format!("{}={:?}", field.name(), value));
        }
    }

    impl tracing::Subscriber for LogCollector {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            matches!(*metadata.level(), Level::INFO | Level::WARN | Level::ERROR)
        }

        fn new_span(&self, _span: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }

        fn record(&self, _span: &Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &Id, _follows_from: &Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut visitor = FieldVisitor(Vec::new());
            event.record(&mut visitor);
            if !visitor.0.is_empty() {
                self.events.lock().unwrap().push(visitor.0.join(" "));
            }
        }

        fn enter(&self, _span: &Id) {}

        fn exit(&self, _span: &Id) {}
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

// The fixtures below this point are `pub(crate)` so the composed sim
// suite (`netbox_projection_sim_tests.rs`, a sibling `#[cfg(test)]`
// module) can import them instead of duplicating the scaffolding. The
// test functions themselves stay private.

pub(crate) fn aid(s: &str) -> ArchitectureId {
    ArchitectureId::new(s).expect("valid architecture id")
}

pub(crate) fn vid(s: &str) -> ArchitectureVersionId {
    ArchitectureVersionId::new(s).expect("valid version id")
}

fn nid(s: &str) -> NetboxProjectionRunId {
    NetboxProjectionRunId::new(s).expect("valid run id")
}

pub(crate) fn appid(s: &str) -> ArchitectureApplyRunId {
    ArchitectureApplyRunId::new(s).expect("valid apply-run id")
}

/// The projected architecture: one server, one VLAN network with a CIDR,
/// one instance with a fixed IP on that network — exactly one object of
/// each of the six mapped kinds.
pub(crate) fn fixture_architecture() -> CHVArchitecture {
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

/// The model JSON stored in `architecture_versions.normalized_model_json`
/// (what the worker parses back into a `CHVArchitecture`).
pub(crate) fn model_json() -> String {
    serde_json::to_string(&fixture_architecture()).expect("model serializes")
}

/// The desired objects the worker's runner will build — computed here
/// with the same pure-core call so remote fixtures mirror it exactly.
pub(crate) fn desired_objects(architecture_id: &str) -> Vec<NetBoxObject> {
    let architecture = fixture_architecture();
    build_objects(&ProjectionInput {
        architecture: &architecture,
        architecture_id,
        architecture_version: 1,
        snapshot: None,
        config: ProjectionConfigView {
            custom_field_prefix: "chv_".to_string(),
            site_name: Some(SITE.to_string()),
        },
    })
    .expect("build_objects")
    .objects
}

/// Render one desired object back into NetBox wire JSON (the inverse of
/// the client's fail-closed `parse_remote`), so a mocked GET response
/// parses to content byte-equal to the desired projection.
pub(crate) fn remote_fixture(netbox_id: i64, object: &NetBoxObject) -> Value {
    let custom_fields =
        serde_json::to_value(object.custom_fields()).expect("custom fields serialize");
    match object {
        NetBoxObject::Device(d) => json!({
            "id": netbox_id,
            "name": d.name.clone(),
            "status": { "value": d.status.as_str() },
            "site": d.site.clone().map(|s| json!({ "name": s })),
            "tags": d.tags.clone(),
            "custom_fields": custom_fields,
        }),
        NetBoxObject::VirtualMachine(v) => json!({
            "id": netbox_id,
            "name": v.name.clone(),
            "status": { "value": v.status.as_str() },
            "cluster": v.cluster.clone().map(|c| json!({ "name": c })),
            "device": v.device.clone().map(|d| json!({ "name": d })),
            "vcpus": v.cpu,
            "memory": v.memory_mb,
            "tags": v.tags.clone(),
            "custom_fields": custom_fields,
        }),
        NetBoxObject::Interface(i) => json!({
            "id": netbox_id,
            "name": i.name.clone(),
            "virtual_machine": { "name": i.virtual_machine.clone() },
            "description": i.description.clone(),
            "tags": i.tags.clone(),
            "custom_fields": custom_fields,
        }),
        NetBoxObject::Prefix(p) => json!({
            "id": netbox_id,
            "prefix": p.prefix.clone(),
            "vlan": p.vlan.map(|vid| json!({ "vid": vid })),
            "description": p.description.clone(),
            "tags": p.tags.clone(),
            "custom_fields": custom_fields,
        }),
        NetBoxObject::Vlan(v) => json!({
            "id": netbox_id,
            "vid": v.vid,
            "name": v.name.clone(),
            "tags": v.tags.clone(),
            "custom_fields": custom_fields,
        }),
        NetBoxObject::IpAddress(a) => {
            // NetBox returns addresses with a mask suffix and nested
            // assignment objects; the client strips/normalizes both.
            let assigned = a.assigned_to_interface.as_deref().and_then(|value| {
                let (vm, iface) = value.split_once('/')?;
                Some(json!({ "name": iface, "virtual_machine": { "name": vm } }))
            });
            json!({
                "id": netbox_id,
                "address": format!("{}/24", a.address),
                "assigned_object": assigned,
                "tags": a.tags.clone(),
                "custom_fields": custom_fields,
            })
        }
    }
}

pub(crate) fn kind_path(object: &NetBoxObject) -> &'static str {
    match object.kind() {
        NetBoxKind::Vlan => "/api/ipam/vlans/",
        NetBoxKind::Prefix => "/api/ipam/prefixes/",
        NetBoxKind::IpAddress => "/api/ipam/ip-addresses/",
        NetBoxKind::Interface => "/api/virtualization/interfaces/",
        NetBoxKind::VirtualMachine => "/api/virtualization/virtual-machines/",
        NetBoxKind::Device => "/api/dcim/devices/",
    }
}

pub(crate) fn page(results: Vec<Value>) -> Value {
    json!({ "count": results.len(), "next": null, "results": results })
}

// ---------------------------------------------------------------------------
// Store + worker scaffolding
// ---------------------------------------------------------------------------

pub(crate) async fn setup_topology_and_version(
    db: &TestDb,
    topo_id: &str,
    version_id: &str,
    model: &str,
    version_number: i64,
) {
    TopologyRepository::new(db.pool.clone())
        .create(TopologyCreateInput {
            id: aid(topo_id),
            name: format!("{topo_id}-name"),
            display_name: Some(format!("{topo_id} display")),
            description: None,
            environment: Some("test".to_string()),
            status: ArchitectureStatus::Draft,
            owner_user_id: Some("user-1".to_string()),
            design_graph_json: None,
            latest_yaml: None,
        })
        .await
        .expect("topology created");
    VersionRepository::new(db.pool.clone())
        .create(VersionCreateInput {
            id: vid(version_id),
            architecture_id: aid(topo_id),
            version_number,
            yaml_content: "x".to_string(),
            design_graph_json: None,
            normalized_model_json: Some(model.to_string()),
            change_summary: None,
            created_by: None,
        })
        .await
        .expect("version created");
}

pub(crate) async fn add_succeeded_apply_run(
    db: &TestDb,
    apply_id: &str,
    topo_id: &str,
    version_id: &str,
) {
    add_apply_run(db, apply_id, topo_id, version_id, RunStatus::Succeeded).await;
}

/// Seed an apply run with an arbitrary status (the succeeded helper's
/// generalization — the post-apply sweep must skip non-succeeded runs).
async fn add_apply_run(
    db: &TestDb,
    apply_id: &str,
    topo_id: &str,
    version_id: &str,
    status: RunStatus,
) {
    ApplyRunRepository::new(db.pool.clone())
        .create(ApplyRunCreateInput {
            id: appid(apply_id),
            architecture_id: aid(topo_id),
            architecture_version_id: vid(version_id),
            plan_id: None,
            task_id: None,
            status,
            requested_by: None,
            started_at: Some(chrono::Utc::now()),
        })
        .await
        .expect("apply run created");
}

/// Backdate an apply run's `created_at` so "most recent" is
/// deterministic in the fixture (the column has second resolution;
/// without distinct timestamps the ordering falls to the `rowid DESC`
/// insertion-order tiebreak).
pub(crate) async fn backdate_apply_run(db: &TestDb, apply_id: &str, created_at: &str) {
    sqlx::query("UPDATE architecture_apply_runs SET created_at = $2 WHERE id = $1")
        .bind(apply_id)
        .bind(created_at)
        .execute(&db.pool)
        .await
        .expect("backdate apply run");
}

async fn setup_config(
    db: &TestDb,
    endpoint: &str,
    topo_id: &str,
    retention: NetboxRetentionPolicy,
) {
    setup_config_with_token(db, endpoint, topo_id, retention, TOKEN).await;
}

/// The [`setup_config`] fixture with an explicit API token (see
/// [`setup_projection_with_token`]).
async fn setup_config_with_token(
    db: &TestDb,
    endpoint: &str,
    topo_id: &str,
    retention: NetboxRetentionPolicy,
    token: &str,
) {
    NetboxProjectionConfigRepository::with_crypto(
        db.pool.clone(),
        chv_controlplane_store::test_util::test_credential_encryption(),
    )
    .upsert(NetboxProjectionConfigUpsertInput {
        architecture_id: aid(topo_id),
        endpoint: endpoint.to_string(),
        token: Some(token.to_string()),
        token_secret_ref: format!("netbox-{topo_id}"),
        retention_policy: retention,
        enable_post_apply: false,
        custom_field_prefix: "chv_".to_string(),
        site_name: Some(SITE.to_string()),
    })
    .await
    .expect("config upserted");
}

async fn enqueue_run(
    db: &TestDb,
    run_id: &str,
    topo_id: &str,
    version_id: &str,
    mode: NetboxProjectionMode,
) {
    NetboxProjectionRunRepository::new(db.pool.clone())
        .create(NetboxProjectionRunCreateInput {
            id: nid(run_id),
            architecture_id: aid(topo_id),
            architecture_version_id: vid(version_id),
            trigger_kind: NetboxProjectionTrigger::Manual,
            mode,
            plan_json: None,
            requested_by: Some("senol".to_string()),
        })
        .await
        .expect("run enqueued");
}

/// Config fixture with the post-apply trigger enabled — the sweep's
/// enablement predicate. The endpoint is deliberately dead in most
/// post-apply suites (see the section doc below).
pub(crate) async fn setup_post_apply_config(db: &TestDb, endpoint: &str, topo_id: &str) {
    setup_post_apply_config_with_token(db, endpoint, topo_id, TOKEN).await;
}

/// The [`setup_post_apply_config`] fixture with an explicit API token
/// — the real-NetBox qualification lane stores the qualification
/// instance's token, not the wiremock/sim fixture constant.
pub(crate) async fn setup_post_apply_config_with_token(
    db: &TestDb,
    endpoint: &str,
    topo_id: &str,
    token: &str,
) {
    NetboxProjectionConfigRepository::with_crypto(
        db.pool.clone(),
        chv_controlplane_store::test_util::test_credential_encryption(),
    )
    .upsert(NetboxProjectionConfigUpsertInput {
        architecture_id: aid(topo_id),
        endpoint: endpoint.to_string(),
        token: Some(token.to_string()),
        token_secret_ref: format!("netbox-{topo_id}"),
        retention_policy: NetboxRetentionPolicy::MarkStale,
        enable_post_apply: true,
        custom_field_prefix: "chv_".to_string(),
        site_name: Some(SITE.to_string()),
    })
    .await
    .expect("config upserted");
}

/// Seed a `post_apply`-triggered run directly (the sweep's own enqueue
/// shape: system trigger, export mode, no plan snapshot).
async fn enqueue_post_apply_run(db: &TestDb, run_id: &str, topo_id: &str, version_id: &str) {
    NetboxProjectionRunRepository::new(db.pool.clone())
        .create(NetboxProjectionRunCreateInput {
            id: nid(run_id),
            architecture_id: aid(topo_id),
            architecture_version_id: vid(version_id),
            trigger_kind: NetboxProjectionTrigger::PostApply,
            mode: NetboxProjectionMode::Export,
            plan_json: None,
            requested_by: None,
        })
        .await
        .expect("post-apply run enqueued");
}

/// All `post_apply`-triggered projection runs for an architecture
/// (any status).
pub(crate) async fn post_apply_runs(
    db: &TestDb,
    topo_id: &str,
) -> Vec<chv_controlplane_types::architecture::NetboxProjectionRun> {
    NetboxProjectionRunRepository::new(db.pool.clone())
        .list_by_architecture(&aid(topo_id), 50)
        .await
        .expect("runs list")
        .into_iter()
        .filter(|run| run.trigger_kind == NetboxProjectionTrigger::PostApply)
        .collect()
}

/// Full happy-path scaffolding: topology + version + succeeded apply run
/// + config + queued export run.
pub(crate) async fn setup_projection(
    db: &TestDb,
    endpoint: &str,
    topo_id: &str,
    version_id: &str,
    apply_id: &str,
    run_id: &str,
    retention: NetboxRetentionPolicy,
) {
    setup_projection_with_token(
        db, endpoint, topo_id, version_id, apply_id, run_id, retention, TOKEN,
    )
    .await;
}

/// The [`setup_projection`] fixture with an explicit API token — the
/// real-NetBox qualification lane stores the qualification instance's
/// token, not the wiremock/sim fixture constant.
// Mirrors setup_projection's 7 parameters plus the token; a test
// fixture's argument list is not design surface worth restructuring.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn setup_projection_with_token(
    db: &TestDb,
    endpoint: &str,
    topo_id: &str,
    version_id: &str,
    apply_id: &str,
    run_id: &str,
    retention: NetboxRetentionPolicy,
    token: &str,
) {
    let model = model_json();
    setup_topology_and_version(db, topo_id, version_id, &model, 1).await;
    add_succeeded_apply_run(db, apply_id, topo_id, version_id).await;
    setup_config_with_token(db, endpoint, topo_id, retention, token).await;
    enqueue_run(
        db,
        run_id,
        topo_id,
        version_id,
        NetboxProjectionMode::Export,
    )
    .await;
}

/// Worker wired to the test store with the wiremock-compatible
/// (plain-HTTP) client constructor injected through the factory seam.
pub(crate) fn worker_for(db: &TestDb) -> NetboxProjectionWorker {
    NetboxProjectionWorker::new(
        NetboxProjectionRunRepository::new(db.pool.clone()),
        NetboxProjectionConfigRepository::with_crypto(
            db.pool.clone(),
            chv_controlplane_store::test_util::test_credential_encryption(),
        ),
        EventRepository::new(db.pool.clone()),
        ApplyRunRepository::new(db.pool.clone()),
        VersionRepository::new(db.pool.clone()),
    )
    .with_client_factory(Arc::new(NetBoxClient::new_unchecked_for_tests))
}

pub(crate) async fn get_run(
    db: &TestDb,
    run_id: &str,
) -> chv_controlplane_types::architecture::NetboxProjectionRun {
    NetboxProjectionRunRepository::new(db.pool.clone())
        .get(&nid(run_id))
        .await
        .expect("run lookup")
        .expect("run exists")
}

/// Audit events appended for one run (correlation id = run id).
pub(crate) async fn audit_events(db: &TestDb, run_id: &str) -> Vec<(String, Option<String>)> {
    sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT message, details FROM events WHERE correlation_id = $1",
    )
    .bind(run_id)
    .fetch_all(&db.pool)
    .await
    .expect("events query")
}

// ---------------------------------------------------------------------------
// The default factory stays HTTPS-only (ADR-024 decision 5)
// ---------------------------------------------------------------------------

/// Structural pin for the `netbox-demo` double gate: the worker's
/// **default** client construction is `NetBoxClient::new`, which
/// rejects plain-HTTP endpoints fail-closed. This suite injects the
/// unchecked constructor explicitly ([`worker_for`]); this test pins
/// that the *default* path — what a default-feature build ships — can
/// never talk to an `http://` NetBox endpoint, with or without the
/// `netbox-demo` feature compiled in (the runtime env gate is the
/// demo factory's own second gate, exercised in `netbox_demo.rs`).
#[test]
fn default_client_factory_rejects_plain_http() {
    let token = chv_netbox_adapter::NetBoxToken::new(TOKEN.to_string());
    let error = NetBoxClient::new("http://127.0.0.1:8080", token)
        .expect_err("production constructor must reject http:// endpoints");
    assert!(
        matches!(error, chv_netbox_adapter::ClientError::HttpsRequired { .. }),
        "expected HttpsRequired, got: {error:?}"
    );
    assert!(
        error.to_string().contains("NETBOX_HTTPS_REQUIRED"),
        "stable code in message: {error}"
    );
    // And the positive control: https still constructs.
    NetBoxClient::new(
        "https://netbox.example.internal",
        chv_netbox_adapter::NetBoxToken::new(TOKEN.to_string()),
    )
    .expect("https endpoints construct through the production path");
}

// ---------------------------------------------------------------------------
// wiremock scaffolding
// ---------------------------------------------------------------------------

/// Every GET on any `/api/…` path answers with an empty page — the
/// "NetBox has nothing of ours" state (used with per-kind POST mocks).
pub(crate) async fn mount_empty_remote(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path_regex("^/api/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(Vec::new())))
        .mount(server)
        .await;
}

/// Empty pages for the given kind paths only (for suites that need other
/// kinds populated — a catch-all would overlap those mocks).
pub(crate) async fn mount_empty_lists(server: &MockServer, paths: &[&str]) {
    for kind_path in paths {
        Mock::given(method("GET"))
            .and(path(*kind_path))
            .respond_with(ResponseTemplate::new(200).set_body_json(page(Vec::new())))
            .mount(server)
            .await;
    }
}

/// 201 create responses for the given (path, id) pairs.
pub(crate) async fn mount_creates(server: &MockServer, mocks: &[(&str, i64)]) {
    for (kind_path, netbox_id) in mocks {
        Mock::given(method("POST"))
            .and(path(*kind_path))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": netbox_id })))
            .mount(server)
            .await;
    }
}

/// The runner's post-loop IP-assignment fix-up PATCH.
pub(crate) async fn mount_ip_fixup_patch(server: &MockServer) {
    Mock::given(method("PATCH"))
        .and(path_regex("^/api/ipam/ip-addresses/[0-9]+/$"))
        .respond_with(ResponseTemplate::new(200))
        .mount(server)
        .await;
}

/// The "NetBox already mirrors the projection" state: one GET mock per
/// kind path returning exactly that kind's desired objects. `except`
/// leaves one kind path unmounted for suites that need finer control.
async fn mount_mirror_remote(server: &MockServer, architecture_id: &str, except: Option<&str>) {
    let mut by_path: BTreeMap<&'static str, Vec<Value>> = BTreeMap::new();
    for (i, object) in desired_objects(architecture_id).into_iter().enumerate() {
        let fixture = remote_fixture(400 + i as i64, &object);
        by_path.entry(kind_path(&object)).or_default().push(fixture);
    }
    for (kind_path, results) in by_path {
        if Some(kind_path) == except {
            continue;
        }
        Mock::given(method("GET"))
            .and(path(kind_path))
            .respond_with(ResponseTemplate::new(200).set_body_json(page(results)))
            .mount(server)
            .await;
    }
}

/// Count received requests by method and path prefix.
pub(crate) async fn request_count(
    server: &MockServer,
    http_method: &str,
    path_prefix: &str,
) -> usize {
    server
        .received_requests()
        .await
        .expect("request recording enabled")
        .into_iter()
        .filter(|r| r.method.as_str() == http_method && r.url.path().starts_with(path_prefix))
        .count()
}

/// Paths of all requests with the given method.
pub(crate) async fn request_paths(server: &MockServer, http_method: &str) -> Vec<String> {
    server
        .received_requests()
        .await
        .expect("request recording enabled")
        .into_iter()
        .filter(|r| r.method.as_str() == http_method)
        .map(|r| r.url.path().to_string())
        .collect()
}

pub(crate) fn outcome_of(
    run: &chv_controlplane_types::architecture::NetboxProjectionRun,
) -> NetboxProjectionOutcome {
    serde_json::from_value(result_of(run)).expect("outcome parses")
}

/// The worker persists results inside a provenance envelope
/// (`{ resolved_architecture_version_id, result }`); unwrap it.
pub(crate) fn result_of(run: &chv_controlplane_types::architecture::NetboxProjectionRun) -> Value {
    let envelope: Value = serde_json::from_str(run.result_json.as_deref().expect("result json"))
        .expect("result envelope parses");
    assert!(
        envelope
            .get("resolved_architecture_version_id")
            .and_then(Value::as_str)
            .is_some(),
        "envelope carries the resolved version id: {envelope}"
    );
    envelope.get("result").cloned().expect("envelope result")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn export_creates_all_objects_then_re_run_is_all_no_op() {
    let db = TestDb::new().await;
    let server = MockServer::start().await;
    setup_projection(
        &db,
        &server.uri(),
        "topo-1",
        "v-1",
        "apply-1",
        "netrun-1",
        NetboxRetentionPolicy::MarkStale,
    )
    .await;

    // First export against an empty NetBox: everything creates.
    mount_empty_remote(&server).await;
    mount_creates(&server, &CREATE_MOCKS).await;
    mount_ip_fixup_patch(&server).await;

    worker_for(&db).tick().await.expect("tick succeeds");

    let run = get_run(&db, "netrun-1").await;
    assert_eq!(run.status, NetboxProjectionRunStatus::Succeeded);
    assert!(run.finished_at.is_some());

    let outcome = outcome_of(&run);
    assert!(outcome.error.is_none());
    assert_eq!(outcome.summary.succeeded, 6, "one create per kind");
    assert_eq!(outcome.summary.failed, 0);
    assert_eq!(outcome.summary.skipped, 0);
    assert_eq!(outcome.summary.not_attempted, 0);
    assert_eq!(outcome.plan.summary.create, 6);

    // Wire: one POST per kind, plus the post-loop IP-assignment fix-up.
    assert_eq!(request_count(&server, "POST", "/api/").await, 6);
    assert_eq!(request_count(&server, "PATCH", "/api/").await, 1);
    assert_eq!(request_count(&server, "DELETE", "/api/").await, 0);

    // Every request is authenticated: at least one GET and one POST
    // carry the configured token as `Authorization: Token …`.
    let requests = server.received_requests().await.expect("recording");
    let auth_of = |method: &str| {
        requests
            .iter()
            .find(|r| r.method.as_str() == method)
            .and_then(|r| {
                r.headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            })
    };
    assert_eq!(
        auth_of("GET").as_deref(),
        Some(format!("Token {TOKEN}").as_str()),
        "GET requests must carry the token"
    );
    assert_eq!(
        auth_of("POST").as_deref(),
        Some(format!("Token {TOKEN}").as_str()),
        "POST requests must carry the token"
    );

    // The success audit event carries the run id and the summary.
    let events = audit_events(&db, "netrun-1").await;
    assert!(
        events.iter().any(
            |(message, details)| message == "architecture_netbox_export_succeeded"
                && details
                    .as_deref()
                    .is_some_and(|d| d.contains("\"create\":6"))
        ),
        "expected export_succeeded event with summary, got {events:?}"
    );

    // Second export against the mirrored state: all no_op, zero writes.
    server.reset().await;
    mount_mirror_remote(&server, "topo-1", None).await;
    enqueue_run(
        &db,
        "netrun-2",
        "topo-1",
        "v-1",
        NetboxProjectionMode::Export,
    )
    .await;

    worker_for(&db).tick().await.expect("tick succeeds");

    let run = get_run(&db, "netrun-2").await;
    assert_eq!(run.status, NetboxProjectionRunStatus::Succeeded);
    let outcome = outcome_of(&run);
    assert!(outcome.error.is_none());
    assert_eq!(outcome.summary.skipped, 6, "all no_op");
    assert_eq!(outcome.summary.succeeded, 0);
    assert_eq!(outcome.summary.failed, 0);
    assert_eq!(outcome.plan.summary.create, 0);

    // No mutation was sent on the re-run.
    assert_eq!(request_count(&server, "POST", "/api/").await, 0);
    assert_eq!(request_count(&server, "PATCH", "/api/").await, 0);
    assert_eq!(request_count(&server, "DELETE", "/api/").await, 0);
}

/// Real NetBox assigns ids per content type (per-table primary-key
/// sequences), so a converged multi-kind state routinely has a vlan,
/// a prefix, a device, a VM, an interface, and an IP all carrying the
/// SAME id. The remote-state fetch must key its map by (kind, id):
/// keyed by id alone, the six objects collapse into one entry and
/// every plan after the first degrades to duplicate creates (the
/// simulator masks this — its ids come from one global counter — so
/// only the real-NetBox qualification lane could catch it; run
/// 37983487636, where the duplicate vlan tripped NetBox's ambiguity
/// guard on the prefix create). The wiremock mirror above assigns
/// globally-distinct ids (`400 + i`), which is exactly why this
/// needs its own fixture: every mirrored row here shares id 7.
#[tokio::test]
async fn re_run_is_all_no_op_when_kinds_share_netbox_ids() {
    let db = TestDb::new().await;
    let server = MockServer::start().await;
    setup_projection(
        &db,
        &server.uri(),
        "topo-1",
        "v-1",
        "apply-1",
        "netrun-1",
        NetboxRetentionPolicy::MarkStale,
    )
    .await;

    // Converged remote state in the real NetBox shape: same natural
    // keys and custom fields as the desired projection, one row per
    // kind, every row id 7.
    let mut by_path: BTreeMap<&'static str, Vec<Value>> = BTreeMap::new();
    for object in desired_objects("topo-1") {
        let fixture = remote_fixture(7, &object);
        by_path.entry(kind_path(&object)).or_default().push(fixture);
    }
    for (kind_path, results) in by_path {
        Mock::given(method("GET"))
            .and(path(kind_path))
            .respond_with(ResponseTemplate::new(200).set_body_json(page(results)))
            .mount(&server)
            .await;
    }

    // `setup_projection` has already enqueued the post-apply export
    // run — the first tick executes it directly against the mirrored
    // state.
    worker_for(&db).tick().await.expect("tick succeeds");

    let run = get_run(&db, "netrun-1").await;
    assert_eq!(run.status, NetboxProjectionRunStatus::Succeeded);
    let outcome = outcome_of(&run);
    assert!(outcome.error.is_none());
    assert_eq!(outcome.summary.skipped, 6, "all no_op despite shared ids");
    assert_eq!(outcome.summary.succeeded, 0);
    assert_eq!(outcome.summary.failed, 0);
    assert_eq!(outcome.plan.summary.create, 0);

    // Nothing was written — a duplicate create over converged state
    // is exactly the drift this pins.
    assert_eq!(request_count(&server, "POST", "/api/").await, 0);
    assert_eq!(request_count(&server, "PATCH", "/api/").await, 0);
    assert_eq!(request_count(&server, "DELETE", "/api/").await, 0);
}

#[tokio::test]
async fn partial_failure_aborts_and_retry_resumes_without_duplicate_create() {
    let db = TestDb::new().await;
    let server = MockServer::start().await;
    setup_projection(
        &db,
        &server.uri(),
        "topo-1",
        "v-1",
        "apply-1",
        "netrun-1",
        NetboxRetentionPolicy::MarkStale,
    )
    .await;

    // First attempt: the VLAN create succeeds, the prefix create 500s —
    // abort-on-first-hard-failure leaves the run failed and retryable.
    mount_empty_remote(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/ipam/vlans/"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 101 })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/ipam/prefixes/"))
        .respond_with(ResponseTemplate::new(500).set_body_string("netbox exploded"))
        .mount(&server)
        .await;

    worker_for(&db).tick().await.expect("tick succeeds");

    let run = get_run(&db, "netrun-1").await;
    // A 5xx is a transient class: the run is auto-requeued (bounded
    // retry with backoff) instead of staying failed.
    assert_eq!(run.status, NetboxProjectionRunStatus::Queued);
    assert_eq!(run.attempt_count, 1, "failure consumed an attempt");
    let error = run.error_message.as_deref().expect("error message");
    assert!(error.contains("status 500"), "unexpected error: {error}");
    assert!(
        run.next_attempt_at
            .is_some_and(|at| at > chrono::Utc::now()),
        "retry backoff scheduled in the future: {:?}",
        run.next_attempt_at
    );
    // F6: the partial outcome ledger (succeeded / failed /
    // not_attempted) is persisted on the failed-then-requeued run.
    let outcome = outcome_of(&run);
    assert_eq!(outcome.summary.succeeded, 1, "the VLAN create landed");
    assert_eq!(outcome.summary.failed, 1);
    assert_eq!(
        outcome.summary.not_attempted, 4,
        "the rest was never attempted"
    );

    // Exactly one create of each attempted kind before the abort.
    assert_eq!(request_count(&server, "POST", "/api/ipam/vlans/").await, 1);
    assert_eq!(
        request_count(&server, "POST", "/api/ipam/prefixes/").await,
        1
    );
    assert_eq!(request_count(&server, "POST", "/api/").await, 2);

    // While the backoff is pending, a further tick does not re-claim
    // the run (nothing new is sent to NetBox).
    worker_for(&db).tick().await.expect("backoff tick succeeds");
    assert_eq!(request_count(&server, "POST", "/api/").await, 2);
    assert_eq!(get_run(&db, "netrun-1").await.attempt_count, 1);

    // Retry once the backoff elapses (simulated by backdating the
    // schedule): the VLAN written by the failed attempt is already
    // remote (matched by external id — resume, not duplicate create).
    sqlx::query("UPDATE netbox_projection_runs SET next_attempt_at = '2020-01-01T00:00:00Z' WHERE id = 'netrun-1'")
        .execute(&db.pool)
        .await
        .expect("backdate retry backoff");
    server.reset().await;

    let vlan_fixture = desired_objects("topo-1")
        .into_iter()
        .find(|object| matches!(object, NetBoxObject::Vlan(_)))
        .expect("desired vlan");
    Mock::given(method("GET"))
        .and(path("/api/ipam/vlans/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(page(vec![remote_fixture(101, &vlan_fixture)])),
        )
        .mount(&server)
        .await;
    mount_empty_lists(
        &server,
        &[
            "/api/ipam/prefixes/",
            "/api/ipam/ip-addresses/",
            "/api/virtualization/interfaces/",
            "/api/virtualization/virtual-machines/",
            "/api/dcim/devices/",
        ],
    )
    .await;
    mount_creates(
        &server,
        &[
            ("/api/ipam/prefixes/", 102),
            ("/api/ipam/ip-addresses/", 103),
            ("/api/virtualization/interfaces/", 104),
            ("/api/virtualization/virtual-machines/", 105),
            ("/api/dcim/devices/", 106),
        ],
    )
    .await;
    mount_ip_fixup_patch(&server).await;

    worker_for(&db).tick().await.expect("tick succeeds");

    let run = get_run(&db, "netrun-1").await;
    assert_eq!(run.status, NetboxProjectionRunStatus::Succeeded);
    assert_eq!(run.attempt_count, 1, "success does not consume an attempt");
    let outcome = outcome_of(&run);
    assert!(outcome.error.is_none());
    assert_eq!(outcome.summary.skipped, 1, "the written VLAN is a no_op");
    assert_eq!(
        outcome.summary.succeeded, 5,
        "only the missing kinds create"
    );

    // The resume did not re-create the VLAN.
    assert_eq!(
        request_count(&server, "POST", "/api/ipam/vlans/").await,
        0,
        "resume must not duplicate the already-written object"
    );
    assert_eq!(request_count(&server, "POST", "/api/").await, 5);
}

#[tokio::test]
async fn auth_failure_fails_run_with_retryable_error() {
    let db = TestDb::new().await;
    let server = MockServer::start().await;
    setup_projection(
        &db,
        &server.uri(),
        "topo-1",
        "v-1",
        "apply-1",
        "netrun-1",
        NetboxRetentionPolicy::MarkStale,
    )
    .await;

    Mock::given(method("GET"))
        .and(path_regex("^/api/"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    worker_for(&db).tick().await.expect("tick succeeds");

    let run = get_run(&db, "netrun-1").await;
    // Auth failure is a transient (retryable) class: the run failed,
    // consumed an attempt, and was auto-requeued with a backoff.
    assert_eq!(run.status, NetboxProjectionRunStatus::Queued);
    assert_eq!(run.attempt_count, 1);
    let error = run.error_message.as_deref().expect("error message");
    assert!(
        error.contains("authentication failed"),
        "expected NETBOX_AUTH_FAILED classification, got: {error}"
    );
    assert!(!error.contains(TOKEN), "error message must be token-free");
    assert!(run
        .next_attempt_at
        .is_some_and(|at| at > chrono::Utc::now()));

    let events = audit_events(&db, "netrun-1").await;
    assert!(
        events
            .iter()
            .any(|(message, _)| message == "architecture_netbox_export_failed"),
        "expected export_failed event, got {events:?}"
    );

    // The requeued run stays bounded by the store's attempt cap (the
    // requeue guard refusing at MAX_ATTEMPTS is covered by the store
    // suite); here the auto-requeue facts are what matter: queued
    // again, attempt consumed, backoff scheduled, history preserved.
}

#[tokio::test]
async fn netbox_outage_fails_run_requeues_with_backoff_and_worker_survives() {
    let db = TestDb::new().await;
    // Nothing listens on port 1: every request is connection-refused.
    setup_projection(
        &db,
        "http://127.0.0.1:1",
        "topo-1",
        "v-1",
        "apply-1",
        "netrun-1",
        NetboxRetentionPolicy::MarkStale,
    )
    .await;

    let worker = worker_for(&db);
    worker.tick().await.expect("outage never kills the worker");

    // An outage is transient: the run failed and was auto-requeued
    // with a backoff (F8) — it is queued again, with the failure
    // history preserved on the row.
    let run = get_run(&db, "netrun-1").await;
    assert_eq!(run.status, NetboxProjectionRunStatus::Queued);
    assert_eq!(run.attempt_count, 1);
    let error = run.error_message.as_deref().expect("error message");
    assert!(error.contains("unreachable"), "unexpected error: {error}");
    assert!(run
        .next_attempt_at
        .is_some_and(|at| at > chrono::Utc::now()));

    // The worker keeps ticking, but the backoff gates the claim: the
    // requeued run is not re-processed until the backoff elapses.
    worker.tick().await.expect("second tick survives too");
    let run = get_run(&db, "netrun-1").await;
    assert_eq!(run.status, NetboxProjectionRunStatus::Queued);
    assert_eq!(run.attempt_count, 1, "backoff prevented an early retry");

    // Once the backoff elapses (simulated by backdating the schedule)
    // the run is claimed, fails again, and consumes another attempt.
    sqlx::query(
        "UPDATE netbox_projection_runs SET next_attempt_at = '2020-01-01T00:00:00Z' \
         WHERE id = 'netrun-1'",
    )
    .execute(&db.pool)
    .await
    .expect("backdate retry backoff");

    worker.tick().await.expect("third tick survives too");

    let run = get_run(&db, "netrun-1").await;
    assert_eq!(run.status, NetboxProjectionRunStatus::Queued);
    assert_eq!(run.attempt_count, 2);
    assert!(run
        .next_attempt_at
        .is_some_and(|at| at > chrono::Utc::now()));
}

/// A foreign VLAN squatting on the desired VLAN's natural key (vid) is a
/// `conflict` — never a write, and never a DELETE even under `delete`
/// retention.
#[tokio::test]
async fn foreign_occupied_natural_key_is_conflict_and_never_deleted() {
    let db = TestDb::new().await;
    let server = MockServer::start().await;
    setup_projection(
        &db,
        &server.uri(),
        "topo-1",
        "v-1",
        "apply-1",
        "netrun-1",
        NetboxRetentionPolicy::Delete,
    )
    .await;

    // A foreign VLAN (no ownership marker) on the desired vid.
    let foreign_vlan = json!({
        "id": 55,
        "vid": 42,
        "name": "netops-backend",
        "tags": [],
        "custom_fields": {},
    });
    Mock::given(method("GET"))
        .and(path("/api/ipam/vlans/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(vec![foreign_vlan])))
        .mount(&server)
        .await;
    mount_empty_lists(
        &server,
        &[
            "/api/ipam/prefixes/",
            "/api/ipam/ip-addresses/",
            "/api/virtualization/interfaces/",
            "/api/virtualization/virtual-machines/",
            "/api/dcim/devices/",
        ],
    )
    .await;
    mount_creates(
        &server,
        &[
            ("/api/ipam/prefixes/", 102),
            ("/api/ipam/ip-addresses/", 103),
            ("/api/virtualization/interfaces/", 104),
            ("/api/virtualization/virtual-machines/", 105),
            ("/api/dcim/devices/", 106),
        ],
    )
    .await;
    mount_ip_fixup_patch(&server).await;

    worker_for(&db).tick().await.expect("tick succeeds");

    let run = get_run(&db, "netrun-1").await;
    assert_eq!(run.status, NetboxProjectionRunStatus::Succeeded);
    let outcome = outcome_of(&run);
    assert!(outcome.error.is_none());

    let vlan_entry = outcome
        .entries
        .iter()
        .find(|entry| entry.kind == NetBoxKind::Vlan)
        .expect("vlan entry");
    assert_eq!(vlan_entry.action, NetboxPlanAction::Conflict);
    assert_eq!(vlan_entry.status, NetboxEntryStatus::Skipped);

    // Conflicts never write and the foreign object is never deleted.
    assert_eq!(request_count(&server, "POST", "/api/ipam/vlans/").await, 0);
    assert_eq!(request_count(&server, "DELETE", "/api/").await, 0);
    // The unoccupied kinds still create.
    assert_eq!(request_count(&server, "POST", "/api/").await, 5);
}

/// Under `delete` retention a stale object of this architecture IS
/// deleted (after the runner's ownership re-verification), and only that
/// object.
#[tokio::test]
async fn delete_retention_deletes_owned_stale_object_after_reverification() {
    let db = TestDb::new().await;
    let server = MockServer::start().await;
    setup_projection(
        &db,
        &server.uri(),
        "topo-5",
        "v-5",
        "apply-5",
        "netrun-5",
        NetboxRetentionPolicy::Delete,
    )
    .await;

    // Five kinds mirror the projection; the VLAN kind needs split mocks
    // because its list (desired + stale) and its natural-key lookup
    // (desired only) must answer differently to stay unambiguous.
    mount_mirror_remote(&server, "topo-5", Some("/api/ipam/vlans/")).await;

    let desired_vlan = desired_objects("topo-5")
        .into_iter()
        .find(|object| matches!(object, NetBoxObject::Vlan(_)))
        .expect("desired vlan");
    // Id 500: the mirror mounts use 400+i for the other kinds (prefix is
    // 401), and the remote state is deduplicated by NetBox id — a
    // colliding fixture would silently clobber its kind.
    let desired_vlan_fixture = remote_fixture(500, &desired_vlan);
    // A stale-but-ours VLAN: full marker of this architecture, CHV
    // source long gone.
    let stale_vlan = json!({
        "id": 77,
        "vid": 77,
        "name": "legacy",
        "tags": [],
        "custom_fields": {
            "chv_external_id": "arch:topo-5:network/legacy#vlan:1",
            "chv_architecture_id": "topo-5",
            "chv_managed_by": "chv",
            "chv_managed_state": "active",
            "chv_architecture_version": "1",
            "chv_mapping_version": "v1"
        },
    });
    Mock::given(method("GET"))
        .and(path("/api/ipam/vlans/"))
        .and(query_param("cf_chv_architecture_id", "topo-5"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(page(vec![desired_vlan_fixture.clone(), stale_vlan])),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/ipam/vlans/"))
        .and(query_param("vid", "42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(vec![desired_vlan_fixture])))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/ipam/vlans/77/"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    worker_for(&db).tick().await.expect("tick succeeds");

    let run = get_run(&db, "netrun-5").await;
    let outcome = outcome_of(&run);
    assert_eq!(run.status, NetboxProjectionRunStatus::Succeeded);
    assert!(outcome.error.is_none());

    let stale_entry = outcome
        .entries
        .iter()
        .find(|entry| entry.action == NetboxPlanAction::Stale)
        .expect("stale entry for the removed network");
    assert_eq!(stale_entry.status, NetboxEntryStatus::Succeeded);
    assert_eq!(outcome.summary.skipped, 6, "everything desired is no_op");
    assert_eq!(outcome.summary.succeeded, 1, "exactly the stale delete");

    // Exactly one DELETE, aimed at the stale object.
    assert_eq!(
        request_paths(&server, "DELETE").await,
        vec!["/api/ipam/vlans/77/"],
        "only the owned stale object is deleted"
    );
    assert_eq!(request_count(&server, "POST", "/api/").await, 0);
}

#[tokio::test]
async fn worker_claims_one_run_per_architecture_and_tick_is_idempotent() {
    let db = TestDb::new().await;
    let server = MockServer::start().await;
    let model = model_json();
    for (topo, version, apply, run) in [
        ("topo-a", "v-a", "apply-a", "netrun-a"),
        ("topo-b", "v-b", "apply-b", "netrun-b"),
    ] {
        setup_topology_and_version(&db, topo, version, &model, 1).await;
        add_succeeded_apply_run(&db, apply, topo, version).await;
        setup_config(&db, &server.uri(), topo, NetboxRetentionPolicy::MarkStale).await;
        enqueue_run(&db, run, topo, version, NetboxProjectionMode::Export).await;
    }

    mount_empty_remote(&server).await;
    mount_creates(&server, &CREATE_MOCKS).await;
    mount_ip_fixup_patch(&server).await;

    // One tick processes one run per architecture: both queued runs go.
    worker_for(&db).tick().await.expect("tick succeeds");

    for run in ["netrun-a", "netrun-b"] {
        let done = get_run(&db, run).await;
        assert_eq!(done.status, NetboxProjectionRunStatus::Succeeded, "{run}");
    }
    assert_eq!(request_count(&server, "POST", "/api/").await, 12);

    // A second tick finds nothing queued and writes nothing further.
    worker_for(&db).tick().await.expect("second tick succeeds");
    assert_eq!(request_count(&server, "POST", "/api/").await, 12);
}

#[tokio::test]
async fn dry_run_computes_plan_without_writes() {
    let db = TestDb::new().await;
    let server = MockServer::start().await;
    let model = model_json();
    setup_topology_and_version(&db, "topo-1", "v-1", &model, 1).await;
    add_succeeded_apply_run(&db, "apply-1", "topo-1", "v-1").await;
    setup_config(
        &db,
        &server.uri(),
        "topo-1",
        NetboxRetentionPolicy::MarkStale,
    )
    .await;
    enqueue_run(
        &db,
        "netrun-1",
        "topo-1",
        "v-1",
        NetboxProjectionMode::DryRun,
    )
    .await;

    // Read-only mocks only: any mutation would 404 and fail the run.
    mount_empty_remote(&server).await;

    worker_for(&db).tick().await.expect("tick succeeds");

    let run = get_run(&db, "netrun-1").await;
    assert_eq!(run.status, NetboxProjectionRunStatus::Succeeded);
    let plan: NetboxProjectionPlan =
        serde_json::from_value(result_of(&run)).expect("dry-run result is the serialized plan");
    assert_eq!(plan.summary.create, 6);

    // A dry run never mutates NetBox.
    assert_eq!(request_count(&server, "POST", "/api/").await, 0);
    assert_eq!(request_count(&server, "PATCH", "/api/").await, 0);
    assert_eq!(request_count(&server, "DELETE", "/api/").await, 0);

    let events = audit_events(&db, "netrun-1").await;
    assert!(
        events
            .iter()
            .any(|(message, _)| message == "architecture_netbox_dry_run"),
        "expected dry_run event, got {events:?}"
    );
}

#[tokio::test]
async fn reclamation_fails_stale_running_run_and_leaves_fresh_alone() {
    let db = TestDb::new().await;
    let server = MockServer::start().await;
    let model = model_json();
    let run_repo = NetboxProjectionRunRepository::new(db.pool.clone());

    // A run claimed 20 minutes ago and never finished (worker crash).
    setup_topology_and_version(&db, "topo-1", "v-1", &model, 1).await;
    setup_config(
        &db,
        &server.uri(),
        "topo-1",
        NetboxRetentionPolicy::MarkStale,
    )
    .await;
    enqueue_run(
        &db,
        "netrun-stale",
        "topo-1",
        "v-1",
        NetboxProjectionMode::Export,
    )
    .await;
    run_repo
        .claim_next_queued(&aid("topo-1"))
        .await
        .expect("claim")
        .expect("queued run");
    sqlx::query(
        "UPDATE netbox_projection_runs SET started_at = '2020-01-01T00:00:00Z' WHERE id = $1",
    )
    .bind("netrun-stale")
    .execute(&db.pool)
    .await
    .expect("backdate started_at");

    // A run claimed just now (inside the 15-minute lease).
    setup_topology_and_version(&db, "topo-2", "v-2", &model, 1).await;
    setup_config(
        &db,
        &server.uri(),
        "topo-2",
        NetboxRetentionPolicy::MarkStale,
    )
    .await;
    enqueue_run(
        &db,
        "netrun-fresh",
        "topo-2",
        "v-2",
        NetboxProjectionMode::Export,
    )
    .await;
    run_repo
        .claim_next_queued(&aid("topo-2"))
        .await
        .expect("claim")
        .expect("queued run");

    worker_for(&db).tick().await.expect("tick succeeds");

    let stale = get_run(&db, "netrun-stale").await;
    assert_eq!(stale.attempt_count, 1, "reclamation consumed an attempt");
    assert!(
        stale
            .error_message
            .as_deref()
            .unwrap_or_default()
            .contains("reclaimed"),
        "reclamation message: {:?}",
        stale.error_message
    );
    let events = audit_events(&db, "netrun-stale").await;
    assert!(
        events.iter().any(
            |(message, details)| message == "architecture_netbox_export_failed"
                && details.as_deref().is_some_and(|d| d.contains("reclaimed"))
        ),
        "expected reclaimed failure event, got {events:?}"
    );
    // A lease expiry is a transient failure class: the reclaimed run is
    // retried through the normal requeue path (bounded by MAX_ATTEMPTS
    // and the backoff schedule), so it lands back in `queued` with a
    // future next_attempt_at rather than requiring manual operator
    // action.
    assert_eq!(stale.status, NetboxProjectionRunStatus::Queued);
    assert!(
        stale
            .next_attempt_at
            .is_some_and(|at| at > chrono::Utc::now()),
        "requeued run must be backoff-gated: {:?}",
        stale.next_attempt_at
    );

    let fresh = get_run(&db, "netrun-fresh").await;
    assert_eq!(fresh.status, NetboxProjectionRunStatus::Running);

    // Idempotent: a second sweep finds nothing new to reclaim, and the
    // requeued run is still backoff-gated (not re-executed).
    worker_for(&db).tick().await.expect("second tick succeeds");
    let stale = get_run(&db, "netrun-stale").await;
    assert_eq!(stale.attempt_count, 1);
    assert_eq!(stale.status, NetboxProjectionRunStatus::Queued);
    let fresh = get_run(&db, "netrun-fresh").await;
    assert_eq!(fresh.status, NetboxProjectionRunStatus::Running);
}

/// The worker's fail-closed gates: no config → NETBOX_NOT_CONFIGURED;
/// no succeeded apply run → NETBOX_NOT_APPLIED. NetBox is never reached.
#[tokio::test]
async fn missing_config_or_succeeded_apply_fails_closed() {
    let db = TestDb::new().await;
    let model = model_json();

    // Config deleted between enqueue and execution.
    setup_topology_and_version(&db, "topo-1", "v-1", &model, 1).await;
    add_succeeded_apply_run(&db, "apply-1", "topo-1", "v-1").await;
    enqueue_run(
        &db,
        "netrun-1",
        "topo-1",
        "v-1",
        NetboxProjectionMode::Export,
    )
    .await;

    // Config present, but the architecture was never applied.
    setup_topology_and_version(&db, "topo-2", "v-2", &model, 1).await;
    setup_config(
        &db,
        "https://netbox.example.internal",
        "topo-2",
        NetboxRetentionPolicy::MarkStale,
    )
    .await;
    enqueue_run(
        &db,
        "netrun-2",
        "topo-2",
        "v-2",
        NetboxProjectionMode::Export,
    )
    .await;

    worker_for(&db).tick().await.expect("tick succeeds");

    let run = get_run(&db, "netrun-1").await;
    assert_eq!(run.status, NetboxProjectionRunStatus::Failed);
    assert!(
        run.error_message
            .as_deref()
            .unwrap_or_default()
            .contains("NETBOX_NOT_CONFIGURED"),
        "unexpected error: {:?}",
        run.error_message
    );

    let run = get_run(&db, "netrun-2").await;
    assert_eq!(run.status, NetboxProjectionRunStatus::Failed);
    assert!(
        run.error_message
            .as_deref()
            .unwrap_or_default()
            .contains("NETBOX_NOT_APPLIED"),
        "unexpected error: {:?}",
        run.error_message
    );

    // Both failures produced their audit events.
    for run_id in ["netrun-1", "netrun-2"] {
        assert!(
            audit_events(&db, run_id)
                .await
                .iter()
                .any(|(message, _)| message == "architecture_netbox_export_failed"),
            "expected failure event for {run_id}"
        );
    }
}

/// The plaintext token never reaches a log line, a persisted error
/// message, or an event payload — belt-and-braces on top of the
/// token-free-by-construction error types.
///
/// Log capture uses the process-wide global collector (see
/// [`log_capture::LogCollector::global`]): the worker's failure
/// warning is emitted through a `tracing` callsite that parallel
/// tests also execute, and a thread-local `set_default` collector
/// races with the process-wide callsite-interest cache.
#[tokio::test]
async fn token_never_appears_in_logs_or_persisted_errors() {
    let logs = log_capture::LogCollector::global();

    let db = TestDb::new().await;
    // Unique ids: the global collector also receives the warnings of
    // the other netbox tests running in parallel; the non-vacuity
    // check below must match *this* run's failure warning.
    setup_projection(
        &db,
        "http://127.0.0.1:1",
        "topo-tok",
        "v-tok",
        "apply-tok",
        "netrun-tok",
        NetboxRetentionPolicy::MarkStale,
    )
    .await;

    worker_for(&db).tick().await.expect("tick succeeds");

    let run = get_run(&db, "netrun-tok").await;
    // The outage class is transient, so the run was auto-requeued —
    // but the failure warning was still emitted, and the error
    // history is on the row either way.
    assert_eq!(run.status, NetboxProjectionRunStatus::Queued);
    assert!(run
        .error_message
        .as_deref()
        .is_some_and(|e| e.contains("unreachable")));

    // The collector saw this run's failure (the assertion below is not
    // vacuous).
    let messages = logs.messages();
    assert!(
        messages
            .iter()
            .any(|m| m.contains("netrun-tok") && m.contains("netbox projection run failed")),
        "expected the worker's failure warning, got {messages:?}; run error: {:?}",
        run.error_message
    );
    // ... but never the token, in any captured line of any test.
    for message in &messages {
        assert!(
            !message.contains(TOKEN),
            "token leaked into a log line: {message}"
        );
    }

    let error = run.error_message.as_deref().expect("error message");
    assert!(!error.contains(TOKEN), "token leaked into the run row");
    for (_, details) in audit_events(&db, "netrun-tok").await {
        let details = details.unwrap_or_default();
        assert!(!details.contains(TOKEN), "token leaked into an event");
    }
}

// ---------------------------------------------------------------------------
// Round-2 review-fix suites (rename path, version gate, ambiguity)
// ---------------------------------------------------------------------------

/// F2: a chv-owned remote renamed in NetBox (it carries our external
/// id, the desired natural key is free) is updated **in place** — the
/// PATCH goes to the renamed object's NetBox id, resolved through the
/// external-id index, and the run succeeds.
#[tokio::test]
async fn renamed_chv_owned_remote_is_updated_in_place() {
    let db = TestDb::new().await;
    let server = MockServer::start().await;
    setup_projection(
        &db,
        &server.uri(),
        "topo-1",
        "v-1",
        "apply-1",
        "netrun-1",
        NetboxRetentionPolicy::MarkStale,
    )
    .await;

    // Five kinds mirror the projection; the device kind needs split
    // mocks (list-by-architecture vs. natural-key probe).
    mount_mirror_remote(&server, "topo-1", Some("/api/dcim/devices/")).await;

    let desired_device = desired_objects("topo-1")
        .into_iter()
        .find(|object| matches!(object, NetBoxObject::Device(_)))
        .expect("desired device");
    // Id 900: chv-owned, full marker of this architecture (external id
    // included), but renamed away from the desired natural key.
    let mut renamed = remote_fixture(900, &desired_device);
    renamed["name"] = json!("chv-node-01-renamed");
    Mock::given(method("GET"))
        .and(path("/api/dcim/devices/"))
        .and(query_param("cf_chv_architecture_id", "topo-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(vec![renamed])))
        .mount(&server)
        .await;
    // The desired natural key is free.
    Mock::given(method("GET"))
        .and(path("/api/dcim/devices/"))
        .and(query_param("name", "chv-node-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(Vec::new())))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/api/dcim/devices/900/"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    worker_for(&db).tick().await.expect("tick succeeds");

    let run = get_run(&db, "netrun-1").await;
    assert_eq!(run.status, NetboxProjectionRunStatus::Succeeded);
    let outcome = outcome_of(&run);
    assert!(outcome.error.is_none());
    let device_entry = outcome
        .entries
        .iter()
        .find(|entry| entry.kind == NetBoxKind::Device)
        .expect("device entry");
    assert_eq!(device_entry.action, NetboxPlanAction::Update);
    assert_eq!(device_entry.status, NetboxEntryStatus::Succeeded);

    // The update PATCHed the renamed object's NetBox id (resolved via
    // the external-id index) — no create, no other writes.
    assert_eq!(
        request_paths(&server, "PATCH").await,
        vec!["/api/dcim/devices/900/"],
        "the rename-update must PATCH the renamed object by its NetBox id"
    );
    assert_eq!(request_count(&server, "POST", "/api/").await, 0);
}

/// F3: a missing architecture-version row for the resolved applied
/// version fails the run inline with NETBOX_NOT_APPLIED — it never
/// propagates (which would strand the run in `running` until lease
/// reclamation) and is not retried.
#[tokio::test]
async fn missing_version_row_fails_run_with_not_applied() {
    let db = TestDb::new().await;
    let server = MockServer::start().await;
    setup_projection(
        &db,
        &server.uri(),
        "topo-1",
        "v-1",
        "apply-1",
        "netrun-1",
        NetboxRetentionPolicy::MarkStale,
    )
    .await;

    // Dangle the resolved version id: repoint the succeeded apply run
    // at a version row that does not exist. (FK enforcement is
    // bypassed on a held connection — with enforcement, ON DELETE
    // CASCADE would remove the rows instead; the worker must be robust
    // to either state.)
    let mut conn = db.pool.acquire().await.expect("connection");
    sqlx::query("PRAGMA foreign_keys = OFF")
        .execute(&mut *conn)
        .await
        .expect("disable FK enforcement");
    sqlx::query(
        "UPDATE architecture_apply_runs SET architecture_version_id = 'v-missing' \
         WHERE id = 'apply-1'",
    )
    .execute(&mut *conn)
    .await
    .expect("repoint apply run at a missing version");
    drop(conn);

    worker_for(&db).tick().await.expect("tick succeeds");

    let run = get_run(&db, "netrun-1").await;
    assert_eq!(
        run.status,
        NetboxProjectionRunStatus::Failed,
        "a missing version row is a permanent failure, not a retry"
    );
    assert_eq!(run.attempt_count, 1);
    let error = run.error_message.as_deref().expect("error message");
    assert!(
        error.contains("version row missing") && error.contains("NETBOX_NOT_APPLIED"),
        "unexpected error: {error}"
    );
    // NetBox was never reached.
    assert!(server
        .received_requests()
        .await
        .expect("recording")
        .is_empty());
}

/// F4: the projection source is the most recent succeeded apply run's
/// version. A run enqueued for V1 while V2 is the latest succeeded
/// apply projects **V2** (recorded in the result envelope's
/// `resolved_architecture_version_id`), never the draft and never the
/// stale enqueued snapshot.
#[tokio::test]
async fn projection_source_is_most_recent_succeeded_apply_version() {
    let db = TestDb::new().await;
    let server = MockServer::start().await;

    // V1: the fixture model. V2: the same topology with a richer VM —
    // a distinguishable projection.
    let model_v1 = model_json();
    let mut arch = fixture_architecture();
    arch.instances[0].resources = Some(InstanceResources {
        cpu: Some(4),
        memory_mb: Some(4096),
    });
    let model_v2 = serde_json::to_string(&arch).expect("model v2 serializes");

    setup_topology_and_version(&db, "topo-1", "v-1", &model_v1, 1).await;
    // Second version row for the same topology (the helper creates the
    // topology too, and topology names are unique).
    VersionRepository::new(db.pool.clone())
        .create(VersionCreateInput {
            id: vid("v-2"),
            architecture_id: aid("topo-1"),
            version_number: 2,
            yaml_content: "x".to_string(),
            design_graph_json: None,
            normalized_model_json: Some(model_v2),
            change_summary: None,
            created_by: None,
        })
        .await
        .expect("version v-2 created");
    add_succeeded_apply_run(&db, "apply-1", "topo-1", "v-1").await;
    // created_at has second resolution: backdate the older apply run
    // so "most recent succeeded" is deterministic.
    sqlx::query("UPDATE architecture_apply_runs SET created_at = '2020-01-01T00:00:00Z' WHERE id = 'apply-1'")
        .execute(&db.pool)
        .await
        .expect("backdate apply-1");
    add_succeeded_apply_run(&db, "apply-2", "topo-1", "v-2").await;
    setup_config(
        &db,
        &server.uri(),
        "topo-1",
        NetboxRetentionPolicy::MarkStale,
    )
    .await;
    // Enqueued while V1 was applied — by execution time V2 is.
    enqueue_run(
        &db,
        "netrun-1",
        "topo-1",
        "v-1",
        NetboxProjectionMode::Export,
    )
    .await;

    mount_empty_remote(&server).await;
    mount_creates(&server, &CREATE_MOCKS).await;
    mount_ip_fixup_patch(&server).await;

    worker_for(&db).tick().await.expect("tick succeeds");

    let run = get_run(&db, "netrun-1").await;
    assert_eq!(run.status, NetboxProjectionRunStatus::Succeeded);

    // The envelope records the resolved (V2) version, not the enqueued
    // V1 snapshot.
    let envelope: Value = serde_json::from_str(run.result_json.as_deref().expect("result json"))
        .expect("envelope parses");
    assert_eq!(
        envelope
            .get("resolved_architecture_version_id")
            .and_then(Value::as_str),
        Some("v-2")
    );

    // The projected model is V2's: external ids embed version 2 and the
    // VM create body carries V2's resources.
    let outcome = outcome_of(&run);
    assert!(outcome.error.is_none());
    let vm_entry = outcome
        .plan
        .entries
        .iter()
        .find(|entry| entry.kind == NetBoxKind::VirtualMachine)
        .expect("vm entry");
    assert_eq!(vm_entry.external_id, "arch:topo-1:instance/vm-01:2");
    let vm_create = server
        .received_requests()
        .await
        .expect("recording")
        .into_iter()
        .find(|r| {
            r.method.as_str() == "POST" && r.url.path() == "/api/virtualization/virtual-machines/"
        })
        .expect("vm create request");
    let body: Value = serde_json::from_slice(&vm_create.body).expect("vm create body");
    assert_eq!(body.get("vcpus").and_then(Value::as_i64), Some(4));
    assert_eq!(body.get("memory").and_then(Value::as_i64), Some(4096));
}

/// F5: a natural-key probe matching several remote objects (here two
/// foreign IP addresses, `10.42.0.5/24` and `10.42.0.5/32`, sharing the
/// maskless natural key) degrades to a per-entry `conflict` — the run
/// completes, the affected entry never writes, the others execute.
#[tokio::test]
async fn ambiguous_natural_key_is_per_entry_conflict_not_run_failure() {
    let db = TestDb::new().await;
    let server = MockServer::start().await;
    setup_projection(
        &db,
        &server.uri(),
        "topo-1",
        "v-1",
        "apply-1",
        "netrun-1",
        NetboxRetentionPolicy::MarkStale,
    )
    .await;

    // Five kinds are empty (they create); the IP-address kind answers
    // the architecture filter with nothing but the desired-address
    // probe with TWO foreign squatters.
    mount_empty_lists(
        &server,
        &[
            "/api/ipam/vlans/",
            "/api/ipam/prefixes/",
            "/api/virtualization/interfaces/",
            "/api/virtualization/virtual-machines/",
            "/api/dcim/devices/",
        ],
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/api/ipam/ip-addresses/"))
        .and(query_param("cf_chv_architecture_id", "topo-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(Vec::new())))
        .mount(&server)
        .await;
    let squatters = page(vec![
        json!({ "id": 60, "address": "10.42.0.5/24", "assigned_object": null, "tags": [] }),
        json!({ "id": 61, "address": "10.42.0.5/32", "assigned_object": null, "tags": [] }),
    ]);
    Mock::given(method("GET"))
        .and(path("/api/ipam/ip-addresses/"))
        .and(query_param("address", "10.42.0.5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(squatters))
        .mount(&server)
        .await;
    mount_creates(
        &server,
        &[
            ("/api/ipam/vlans/", 101),
            ("/api/ipam/prefixes/", 102),
            ("/api/virtualization/interfaces/", 104),
            ("/api/virtualization/virtual-machines/", 105),
            ("/api/dcim/devices/", 106),
        ],
    )
    .await;

    worker_for(&db).tick().await.expect("tick succeeds");

    let run = get_run(&db, "netrun-1").await;
    assert_eq!(
        run.status,
        NetboxProjectionRunStatus::Succeeded,
        "ambiguity must not abort the run"
    );
    let outcome = outcome_of(&run);
    assert!(outcome.error.is_none());
    let ip_entry = outcome
        .entries
        .iter()
        .find(|entry| entry.kind == NetBoxKind::IpAddress)
        .expect("ip entry");
    assert_eq!(ip_entry.action, NetboxPlanAction::Conflict);
    assert_eq!(ip_entry.status, NetboxEntryStatus::Skipped);
    // The other five kinds executed; the ambiguous address never wrote.
    assert_eq!(outcome.summary.succeeded, 5);
    assert_eq!(request_count(&server, "POST", "/api/").await, 5);
    assert_eq!(
        request_count(&server, "POST", "/api/ipam/ip-addresses/").await,
        0,
        "the ambiguous entry must never be written"
    );
}

// ---------------------------------------------------------------------------
// Post-apply trigger sweep (PR 6)
// ---------------------------------------------------------------------------
//
// The sweep runs inside `tick()` before the claim loop, so a run it
// enqueues is claimed and executed on the SAME tick. The post-apply
// suites below point the config at a deliberately dead endpoint
// (`http://127.0.0.1:1`, the outage fixture): the enqueued run fails
// transiently and lands back in `queued` via the bounded auto-requeue.
// The enqueue facts under test — trigger kind, mode, projected version,
// system-requested provenance, and the run count — are unaffected; the
// requeue additionally proves the full pipeline is wired.

/// No projection config → the sweep enqueues nothing, even with a
/// succeeded apply run sitting there.
#[tokio::test]
async fn post_apply_no_config_creates_no_run() {
    let db = TestDb::new().await;
    let model = model_json();
    setup_topology_and_version(&db, "topo-1", "v-1", &model, 1).await;
    add_succeeded_apply_run(&db, "apply-1", "topo-1", "v-1").await;
    // Deliberately no netbox projection config.

    worker_for(&db).tick().await.expect("tick succeeds");

    assert!(
        post_apply_runs(&db, "topo-1").await.is_empty(),
        "no config → no post_apply run"
    );
    // No projection run of any trigger kind was created.
    assert!(NetboxProjectionRunRepository::new(db.pool.clone())
        .list_by_architecture(&aid("topo-1"), 10)
        .await
        .expect("runs list")
        .is_empty());
}

/// A config with `enable_post_apply = false` is invisible to the sweep.
#[tokio::test]
async fn post_apply_disabled_config_creates_no_run() {
    let db = TestDb::new().await;
    let model = model_json();
    setup_topology_and_version(&db, "topo-1", "v-1", &model, 1).await;
    add_succeeded_apply_run(&db, "apply-1", "topo-1", "v-1").await;
    setup_config(
        &db,
        "http://127.0.0.1:1",
        "topo-1",
        NetboxRetentionPolicy::MarkStale,
    )
    .await;

    worker_for(&db).tick().await.expect("tick succeeds");

    assert!(
        NetboxProjectionRunRepository::new(db.pool.clone())
            .list_by_architecture(&aid("topo-1"), 10)
            .await
            .expect("runs list")
            .is_empty(),
        "enable_post_apply = false → no run at all"
    );
}

/// The sweep enqueues exactly one run for the most recent **succeeded**
/// apply run's version — an older succeeded apply and a newer failed
/// apply are both passed over.
#[tokio::test]
async fn post_apply_enabled_enqueues_for_latest_succeeded_version() {
    let db = TestDb::new().await;
    let model = model_json();
    setup_topology_and_version(&db, "topo-1", "v-1", &model, 1).await;
    for (version_id, number) in [("v-2", 2), ("v-3", 3)] {
        VersionRepository::new(db.pool.clone())
            .create(VersionCreateInput {
                id: vid(version_id),
                architecture_id: aid("topo-1"),
                version_number: number,
                yaml_content: "x".to_string(),
                design_graph_json: None,
                normalized_model_json: Some(model.clone()),
                change_summary: None,
                created_by: None,
            })
            .await
            .expect("version created");
    }

    // Newest-first ordering must be deterministic: backdate the older
    // succeeded applies (created_at has second resolution).
    add_apply_run(&db, "apply-1", "topo-1", "v-1", RunStatus::Succeeded).await;
    backdate_apply_run(&db, "apply-1", "2020-01-01T00:00:00Z").await;
    add_apply_run(&db, "apply-2", "topo-1", "v-2", RunStatus::Succeeded).await;
    backdate_apply_run(&db, "apply-2", "2021-01-01T00:00:00Z").await;
    // The newest apply run FAILED — it must not be projected.
    add_apply_run(&db, "apply-3", "topo-1", "v-3", RunStatus::Failed).await;

    setup_post_apply_config(&db, "http://127.0.0.1:1", "topo-1").await;

    worker_for(&db).tick().await.expect("tick succeeds");

    let runs = post_apply_runs(&db, "topo-1").await;
    assert_eq!(runs.len(), 1, "exactly one post_apply run: {runs:?}");
    let run = &runs[0];
    assert_eq!(run.trigger_kind, NetboxProjectionTrigger::PostApply);
    assert_eq!(run.mode, NetboxProjectionMode::Export);
    assert_eq!(
        run.architecture_version_id,
        vid("v-2"),
        "the most recent SUCCEEDED apply, not the newer failed one"
    );
    assert_eq!(run.requested_by, None, "system trigger: no requester");
    // The same tick's claim loop executed the run against the dead
    // endpoint; the transient failure auto-requeued it, so it is back
    // in `queued` with a scheduled backoff (see the section doc).
    assert_eq!(run.status, NetboxProjectionRunStatus::Queued);
}

/// An active (queued/running) run holds the architecture's one-active
/// slot: the sweep coalesces — no post_apply run is stacked, and the
/// existing run is left untouched.
#[tokio::test]
async fn post_apply_coalesces_on_active_run() {
    let db = TestDb::new().await;
    let model = model_json();
    setup_topology_and_version(&db, "topo-1", "v-1", &model, 1).await;
    add_succeeded_apply_run(&db, "apply-1", "topo-1", "v-1").await;
    setup_post_apply_config(&db, "http://127.0.0.1:1", "topo-1").await;

    // A manual run, currently running (inside its lease) — the
    // one-active slot is taken.
    enqueue_run(
        &db,
        "netrun-active",
        "topo-1",
        "v-1",
        NetboxProjectionMode::Export,
    )
    .await;
    NetboxProjectionRunRepository::new(db.pool.clone())
        .claim_next_queued(&aid("topo-1"))
        .await
        .expect("claim")
        .expect("queued run");

    worker_for(&db).tick().await.expect("tick succeeds");

    assert!(
        post_apply_runs(&db, "topo-1").await.is_empty(),
        "the sweep must coalesce behind the active run"
    );
    // The existing run was untouched: still running, never executed,
    // never failed by the sweep.
    let active = get_run(&db, "netrun-active").await;
    assert_eq!(active.status, NetboxProjectionRunStatus::Running);
    assert_eq!(active.trigger_kind, NetboxProjectionTrigger::Manual);
    assert_eq!(active.error_message, None);
    assert_eq!(active.attempt_count, 0);
}

/// The sweep is idempotent across ticks: exactly one post_apply run
/// per (architecture, version), no matter how many ticks fire.
#[tokio::test]
async fn post_apply_is_idempotent_across_ticks() {
    let db = TestDb::new().await;
    let model = model_json();
    setup_topology_and_version(&db, "topo-1", "v-1", &model, 1).await;
    add_succeeded_apply_run(&db, "apply-1", "topo-1", "v-1").await;
    setup_post_apply_config(&db, "http://127.0.0.1:1", "topo-1").await;

    let worker = worker_for(&db);
    worker.tick().await.expect("first tick succeeds");
    worker.tick().await.expect("second tick succeeds");

    let runs = post_apply_runs(&db, "topo-1").await;
    assert_eq!(runs.len(), 1, "no re-enqueue on the second tick");
    assert_eq!(runs[0].status, NetboxProjectionRunStatus::Queued);
    assert_eq!(
        runs[0].attempt_count, 1,
        "the second tick neither re-enqueued nor re-executed the run \
         (the retry backoff gates the claim)"
    );
}

/// A permanently-failed post_apply run counts as "already attempted":
/// it is NOT re-enqueued by later sweeps (transient retries are owned
/// by the bounded auto-requeue; after the cap the operator retries).
#[tokio::test]
async fn post_apply_failed_projection_run_is_not_re_enqueued() {
    let db = TestDb::new().await;
    let model = model_json();
    setup_topology_and_version(&db, "topo-1", "v-1", &model, 1).await;
    add_succeeded_apply_run(&db, "apply-1", "topo-1", "v-1").await;
    setup_post_apply_config(&db, "http://127.0.0.1:1", "topo-1").await;

    // A failed post_apply run for v-1 already exists (driven to
    // terminal state through the repository, like any real failure).
    enqueue_post_apply_run(&db, "netrun-once", "topo-1", "v-1").await;
    let run_repo = NetboxProjectionRunRepository::new(db.pool.clone());
    let claimed = run_repo
        .claim_next_queued(&aid("topo-1"))
        .await
        .expect("claim")
        .expect("queued run");
    run_repo
        .mark_failed(&claimed.id, Some("netbox unreachable".to_string()), None)
        .await
        .expect("run failed");

    worker_for(&db).tick().await.expect("tick succeeds");

    let runs = post_apply_runs(&db, "topo-1").await;
    assert_eq!(
        runs.len(),
        1,
        "the failed post_apply run must not be re-enqueued"
    );
    assert_eq!(runs[0].id, nid("netrun-once"));
    assert_eq!(runs[0].status, NetboxProjectionRunStatus::Failed);
}

/// A new succeeded apply for a NEW version enqueues a fresh post_apply
/// run even though an earlier version's post_apply run already exists.
#[tokio::test]
async fn post_apply_new_version_after_reapply() {
    let db = TestDb::new().await;
    let model = model_json();
    setup_topology_and_version(&db, "topo-1", "v-2", &model, 2).await;
    VersionRepository::new(db.pool.clone())
        .create(VersionCreateInput {
            id: vid("v-4"),
            architecture_id: aid("topo-1"),
            version_number: 4,
            yaml_content: "x".to_string(),
            design_graph_json: None,
            normalized_model_json: Some(model.clone()),
            change_summary: None,
            created_by: None,
        })
        .await
        .expect("version v-4 created");

    add_succeeded_apply_run(&db, "apply-2", "topo-1", "v-2").await;
    backdate_apply_run(&db, "apply-2", "2020-01-01T00:00:00Z").await;
    setup_post_apply_config(&db, "http://127.0.0.1:1", "topo-1").await;

    // The v-2 projection already ran to completion.
    enqueue_post_apply_run(&db, "netrun-v2", "topo-1", "v-2").await;
    let run_repo = NetboxProjectionRunRepository::new(db.pool.clone());
    let claimed = run_repo
        .claim_next_queued(&aid("topo-1"))
        .await
        .expect("claim")
        .expect("queued run");
    run_repo
        .mark_succeeded(&claimed.id, None, None)
        .await
        .expect("run succeeded");

    // A new apply at v-4 succeeds.
    add_succeeded_apply_run(&db, "apply-4", "topo-1", "v-4").await;

    worker_for(&db).tick().await.expect("tick succeeds");

    let runs = post_apply_runs(&db, "topo-1").await;
    assert_eq!(runs.len(), 2, "one run per applied version: {runs:?}");
    let v2 = runs
        .iter()
        .find(|run| run.architecture_version_id == vid("v-2"))
        .expect("the original v-2 run");
    assert_eq!(v2.status, NetboxProjectionRunStatus::Succeeded);
    let v4 = runs
        .iter()
        .find(|run| run.architecture_version_id == vid("v-4"))
        .expect("the new v-4 run");
    assert_eq!(v4.trigger_kind, NetboxProjectionTrigger::PostApply);
    assert_eq!(v4.mode, NetboxProjectionMode::Export);
    assert_eq!(v4.requested_by, None);
    // Enqueued then auto-requeued after the dead-endpoint execution
    // (see the section doc).
    assert_eq!(v4.status, NetboxProjectionRunStatus::Queued);
}

/// A per-architecture failure inside the sweep (here: a corrupt apply
/// run row that cannot be mapped) is contained: the tick still
/// succeeds and the other architectures still get their runs.
#[tokio::test]
async fn post_apply_failure_does_not_break_other_architectures() {
    let db = TestDb::new().await;
    let model = model_json();
    setup_topology_and_version(&db, "topo-broken", "v-1", &model, 1).await;
    setup_topology_and_version(&db, "topo-healthy", "v-1b", &model, 1).await;
    add_succeeded_apply_run(&db, "apply-broken", "topo-broken", "v-1").await;
    add_succeeded_apply_run(&db, "apply-healthy", "topo-healthy", "v-1b").await;
    setup_post_apply_config(&db, "http://127.0.0.1:1", "topo-broken").await;
    setup_post_apply_config(&db, "http://127.0.0.1:1", "topo-healthy").await;

    // Corrupt the broken architecture's apply-run row so its listing
    // fails in the row mapper (an unparseable `created_at` — the
    // status column has a CHECK constraint; the timestamp does not).
    sqlx::query(
        "UPDATE architecture_apply_runs SET created_at = 'not-a-timestamp' \
         WHERE id = 'apply-broken'",
    )
    .execute(&db.pool)
    .await
    .expect("corrupt apply-run row");

    worker_for(&db)
        .tick()
        .await
        .expect("one architecture's sweep failure never fails the tick");

    // The broken architecture was skipped (warn + continue)...
    assert!(
        post_apply_runs(&db, "topo-broken").await.is_empty(),
        "the broken architecture enqueued nothing"
    );
    // ...while the healthy one still got its post_apply run.
    let healthy = post_apply_runs(&db, "topo-healthy").await;
    assert_eq!(healthy.len(), 1);
    assert_eq!(healthy[0].trigger_kind, NetboxProjectionTrigger::PostApply);
    assert_eq!(healthy[0].architecture_version_id, vid("v-1b"));
    assert_eq!(healthy[0].requested_by, None);
}

/// Isolation (the plan's PR-6 "failed projection enqueue leaves apply
/// run `Succeeded`" scenario, asserted explicitly): a post_apply run
/// that fails during execution — here against the dead-endpoint
/// outage fixture — leaves the apply run that triggered it untouched:
/// still `Succeeded`, `finished_at` exactly as seeded. The projection
/// pipeline can never rewrite apply-run state.
#[tokio::test]
async fn post_apply_failure_leaves_apply_run_succeeded() {
    let db = TestDb::new().await;
    let model = model_json();
    setup_topology_and_version(&db, "topo-1", "v-1", &model, 1).await;
    add_succeeded_apply_run(&db, "apply-1", "topo-1", "v-1").await;
    // Stamp a known terminal state on the apply run: its `finished_at`
    // is the canary the isolation assertions compare against.
    sqlx::query(
        "UPDATE architecture_apply_runs SET \
         started_at = '2025-01-01T00:00:00Z', finished_at = '2025-01-01T00:01:00Z' \
         WHERE id = 'apply-1'",
    )
    .execute(&db.pool)
    .await
    .expect("stamp apply-run terminal timestamps");
    setup_post_apply_config(&db, "http://127.0.0.1:1", "topo-1").await;

    worker_for(&db).tick().await.expect("tick succeeds");

    // The sweep enqueued a post_apply run and the same tick executed
    // it against the dead endpoint: the execution failed (transient
    // class → auto-requeued) and the failure is recorded on the row.
    let runs = post_apply_runs(&db, "topo-1").await;
    assert_eq!(runs.len(), 1, "one post_apply run: {runs:?}");
    let run = &runs[0];
    assert!(run.attempt_count >= 1, "the post_apply execution failed");
    assert!(
        run.error_message
            .as_deref()
            .is_some_and(|e| e.contains("unreachable")),
        "failure recorded: {:?}",
        run.error_message
    );

    // The triggering apply run is untouched: still Succeeded, its
    // terminal timestamps exactly as seeded.
    let apply = ApplyRunRepository::new(db.pool.clone())
        .get(&appid("apply-1"), None)
        .await
        .expect("apply run lookup");
    assert_eq!(apply.status, RunStatus::Succeeded);
    let seeded_finished_at = chrono::DateTime::parse_from_rfc3339("2025-01-01T00:01:00Z")
        .expect("seeded finished_at parses")
        .with_timezone(&chrono::Utc);
    assert_eq!(
        apply.finished_at,
        Some(seeded_finished_at),
        "the failed projection must not touch the apply run's finished_at"
    );
}
