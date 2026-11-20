//! Integration tests for node metric batch ingestion (#602 PR-2,
//! ingestion contract v1). These exercise the full service path —
//! envelope validation, sender/ownership enforcement, registry
//! validation, rate caps, and the durable store outcomes — against a
//! real (temporary-file) monitoring database and a real operational
//! store.

use crate::monitoring_ingest::{
    MonitoringIngestImplementation, MonitoringIngestService, MAX_SAMPLES_PER_BATCH,
};
use chv_controlplane_store::test_util::TestDb;
use chv_controlplane_store::{NodeRepository, ObservedStateRepository};
use chv_controlplane_types::domain::{Generation, NodeId, ResourceId};
use chv_monitoring_store::{MonitoringHealth, MonitoringStore, MonitoringStoreConfig};
use control_plane_node_api::control_plane_node_api as proto;
use std::path::PathBuf;
use std::sync::Arc;

/// A temporary monitoring store (file-backed, unlike the in-memory
/// operational test DB, so WAL/durability semantics are real).
async fn monitoring_store(dir: &std::path::Path) -> Arc<MonitoringStore> {
    Arc::new(
        MonitoringStore::connect(MonitoringStoreConfig {
            database_url: format!("sqlite://{}/monitoring.db", dir.display()),
            migrations_dir: PathBuf::from(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../cmd/chv-controlplane/monitoring-migrations"
            )),
            ..MonitoringStoreConfig::default()
        })
        .await
        .expect("monitoring store connect"),
    )
}

struct Fixture {
    _ops_db: TestDb,
    _monitoring_dir: tempfile::TempDir,
    store: Arc<MonitoringStore>,
    node_repo: NodeRepository,
    observed_state_repo: ObservedStateRepository,
    health: MonitoringHealth,
    service: MonitoringIngestImplementation,
    node_id: String,
    vm_id: String,
}

impl Fixture {
    async fn new() -> Self {
        Self::with_headroom_floor(0).await
    }

    /// `min_headroom_bytes` above the tempdir filesystem's real free
    /// space is exactly the disk-full condition production sees (the
    /// same statvfs read, the same comparison).
    async fn with_headroom_floor(min_headroom_bytes: u64) -> Self {
        let ops_db = TestDb::new().await;
        let node_repo = NodeRepository::new(ops_db.pool.clone());
        let observed_state_repo = ObservedStateRepository::new(ops_db.pool.clone());
        let dir = tempfile::tempdir().unwrap();
        let store = monitoring_store(dir.path()).await;

        // An enrolled node and a VM observed on it.
        let node_id = NodeId::new("node-ingest-1").unwrap();
        node_repo
            .ensure_node_record(&node_id, Some("ingest-test-node"), None, 0)
            .await
            .unwrap();
        let vm_id = ResourceId::new("vm-ingest-1").unwrap();
        // The observed-state row has FKs into vms (desired) and nodes;
        // create both parents first, exactly like the real report path.
        chv_controlplane_store::DesiredStateRepository::new(ops_db.pool.clone())
            .upsert_vm(
                &chv_controlplane_store::VmDesiredStateInput {
                    vm_id: vm_id.clone(),
                    node_id: Some(node_id.clone()),
                    display_name: "ingest-test-vm".to_string(),
                    tenant_id: None,
                    placement_policy: None,
                    desired_generation: Generation::new(1),
                    desired_status: None,
                    requested_by: None,
                    updated_by: None,
                    target_node_id: None,
                    cpu_count: None,
                    memory_bytes: None,
                    image_ref: None,
                    boot_mode: None,
                    desired_power_state: None,
                    vsock_enabled: None,
                    requested_unix_ms: 0,
                },
                None,
            )
            .await
            .unwrap();
        observed_state_repo
            .upsert_vm(&chv_controlplane_store::VmObservedStateInput {
                vm_id: vm_id.clone(),
                observed_generation: Generation::new(1),
                runtime_status: "running".to_string(),
                health_status: Some("ok".to_string()),
                node_id: Some(node_id.clone()),
                cloud_hypervisor_pid: None,
                api_socket_path: None,
                last_error: None,
                last_transition_unix_ms: None,
                observed_unix_ms: 0,
            })
            .await
            .unwrap();

        let health = MonitoringHealth::new();
        let service = MonitoringIngestImplementation::new(
            Some(store.clone()),
            node_repo.clone(),
            observed_state_repo.clone(),
            health.clone(),
            20,
            min_headroom_bytes,
        );
        Fixture {
            _ops_db: ops_db,
            _monitoring_dir: dir,
            store,
            node_repo,
            observed_state_repo,
            health,
            service,
            node_id: node_id.to_string(),
            vm_id: vm_id.to_string(),
        }
    }

    /// Rebuild the ingest service with a new headroom floor over the
    /// SAME store and shared health handle — how recovery looks when
    /// the filesystem frees space (the floor comparison flips back).
    fn set_headroom_floor(&mut self, min_headroom_bytes: u64) {
        self.service = MonitoringIngestImplementation::new(
            Some(self.store.clone()),
            self.node_repo.clone(),
            self.observed_state_repo.clone(),
            self.health.clone(),
            20,
            min_headroom_bytes,
        );
    }

    fn request(
        &self,
        sequence: u64,
        samples: Vec<proto::MetricSampleV1>,
    ) -> proto::NodeMetricBatchRequest {
        proto::NodeMetricBatchRequest {
            meta: None,
            node_id: self.node_id.clone(),
            schema_version: 1,
            boot_id: "agent-boot-1".to_string(),
            sequence,
            sent_at_ms: 0,
            samples,
        }
    }

    async fn ingest(
        &self,
        sequence: u64,
        samples: Vec<proto::MetricSampleV1>,
    ) -> proto::NodeMetricBatchResponse {
        self.service
            .ingest_node_metric_batch(self.request(sequence, samples))
            .await
            .expect("ingest call succeeds")
    }
}

fn node_cpu_sample(observed_at_ms: i64, value: f64) -> proto::MetricSampleV1 {
    proto::MetricSampleV1 {
        target_kind: "node".to_string(),
        target_id: "node-ingest-1".to_string(),
        metric_id: "node.cpu.capacity_ratio".to_string(),
        source: "node_os".to_string(),
        kind: "gauge".to_string(),
        unit: "ratio".to_string(),
        observed_at_ms,
        value: Some(proto::metric_sample_v1::Value::FloatValue(value)),
        quality: "valid".to_string(),
        dimensions: Default::default(),
        boot_id: String::new(),
        identity_epoch: String::new(),
    }
}

fn vm_cores_sample(vm_id: &str, observed_at_ms: i64, value: f64) -> proto::MetricSampleV1 {
    proto::MetricSampleV1 {
        target_kind: "vm".to_string(),
        target_id: vm_id.to_string(),
        metric_id: "vm.cpu.cores_used".to_string(),
        source: "vmm".to_string(),
        kind: "gauge".to_string(),
        unit: "cores".to_string(),
        observed_at_ms,
        value: Some(proto::metric_sample_v1::Value::FloatValue(value)),
        quality: "valid".to_string(),
        dimensions: Default::default(),
        boot_id: String::new(),
        identity_epoch: String::new(),
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[tokio::test]
async fn valid_batch_is_durably_accepted_then_deduplicated() {
    let f = Fixture::new().await;
    let sample = node_cpu_sample(now_ms(), 0.5);
    let resp = f.ingest(0, vec![sample.clone()]).await;
    assert_eq!(resp.outcome, "accepted");
    assert_eq!(resp.accepted_samples, 1);

    // Byte-identical retry (same timestamps): previous durable outcome.
    let resp = f.ingest(0, vec![sample.clone()]).await;
    assert_eq!(resp.outcome, "duplicate");

    // Same key, different body.
    let resp = f.ingest(0, vec![node_cpu_sample(now_ms(), 0.9)]).await;
    assert_eq!(resp.outcome, "replay_conflict");
}

#[tokio::test]
async fn unknown_or_mismetriced_samples_reject_whole_batch() {
    let f = Fixture::new().await;
    let mut unknown = node_cpu_sample(now_ms(), 0.5);
    unknown.metric_id = "node.cpu.not_a_metric".to_string();
    let resp = f.ingest(0, vec![unknown]).await;
    assert_eq!(resp.outcome, "unsupported_metric");

    // Disallowed source for the metric.
    let mut wrong_source = node_cpu_sample(now_ms(), 0.5);
    wrong_source.source = "guest_agent".to_string();
    let resp = f.ingest(0, vec![wrong_source]).await;
    assert_eq!(resp.outcome, "unsupported_metric");

    // Kind/unit must match the registry.
    let mut wrong_kind = node_cpu_sample(now_ms(), 0.5);
    wrong_kind.kind = "counter".to_string();
    let resp = f.ingest(0, vec![wrong_kind]).await;
    assert_eq!(resp.outcome, "invalid_batch");
}

#[tokio::test]
async fn value_quality_consistency_is_enforced() {
    let f = Fixture::new().await;
    // Valid quality without a value.
    let mut no_value = node_cpu_sample(now_ms(), 0.5);
    no_value.value = None;
    let resp = f.ingest(0, vec![no_value]).await;
    assert_eq!(resp.outcome, "invalid_batch");

    // Non-valid quality carrying a value.
    let mut valued = node_cpu_sample(now_ms(), 0.5);
    valued.quality = "unavailable".to_string();
    let resp = f.ingest(0, vec![valued]).await;
    assert_eq!(resp.outcome, "invalid_batch");

    // Unknown quality string.
    let mut bad_quality = node_cpu_sample(now_ms(), 0.5);
    bad_quality.quality = "probably-fine".to_string();
    let resp = f.ingest(0, vec![bad_quality]).await;
    assert_eq!(resp.outcome, "invalid_batch");
}

#[tokio::test]
async fn target_ownership_is_enforced() {
    let f = Fixture::new().await;
    // Node sample for a different node id.
    let mut foreign = node_cpu_sample(now_ms(), 0.5);
    foreign.target_id = "node-other-1".to_string();
    let resp = f.ingest(0, vec![foreign]).await;
    assert_eq!(resp.outcome, "invalid_batch");

    // VM sample for a VM not observed on this sender.
    let foreign_vm = vm_cores_sample("vm-other-1", now_ms(), 1.0);
    let resp = f.ingest(0, vec![foreign_vm]).await;
    assert_eq!(resp.outcome, "invalid_batch");

    // VM sample for a VM on this sender is accepted.
    let owned = vm_cores_sample(&f.vm_id, now_ms(), 1.5);
    let resp = f.ingest(0, vec![owned]).await;
    assert_eq!(resp.outcome, "accepted");
}

#[tokio::test]
async fn envelope_limits_reject_with_typed_outcomes() {
    let f = Fixture::new().await;

    let mut bad_schema = f.request(0, vec![node_cpu_sample(now_ms(), 0.5)]);
    bad_schema.schema_version = 2;
    let resp = f
        .service
        .ingest_node_metric_batch(bad_schema)
        .await
        .unwrap();
    assert_eq!(resp.outcome, "invalid_batch");

    let oversized = f.request(
        0,
        (0..=MAX_SAMPLES_PER_BATCH)
            .map(|i| node_cpu_sample(now_ms() + (i as i64), 0.5))
            .collect(),
    );
    let resp = f.service.ingest_node_metric_batch(oversized).await.unwrap();
    assert_eq!(resp.outcome, "batch_too_large");

    // Stale timestamp.
    let old = node_cpu_sample(now_ms() - 10 * 60 * 1000, 0.5);
    let resp = f.ingest(0, vec![old]).await;
    assert_eq!(resp.outcome, "invalid_batch");

    // Sequence beyond the store's signed watermark range: rejected at
    // the boundary instead of wrapping to a low sequence on the cast.
    let beyond = f.request(i64::MAX as u64 + 1, vec![node_cpu_sample(now_ms(), 0.5)]);
    let resp = f.service.ingest_node_metric_batch(beyond).await.unwrap();
    assert_eq!(resp.outcome, "invalid_batch");

    // Unenrolled sender.
    let mut req = f.request(0, vec![node_cpu_sample(now_ms(), 0.5)]);
    req.node_id = "node-ghost-1".to_string();
    // Keep the node-target consistent so the enrollment check is what
    // fires (not the ownership check).
    req.samples[0].target_id = req.node_id.clone();
    let resp = f.service.ingest_node_metric_batch(req).await.unwrap();
    assert_eq!(resp.outcome, "invalid_batch");
}

#[tokio::test]
async fn per_sender_rate_cap() {
    // A fresh fixture: the rate window counts every batch that passes
    // the envelope+enrollment gates, accepted or not.
    let f = Fixture::new().await;
    for seq in 0..20 {
        let resp = f.ingest(seq, vec![node_cpu_sample(now_ms(), 0.5)]).await;
        assert_eq!(
            resp.outcome, "accepted",
            "sequence {seq} should be accepted"
        );
    }
    let resp = f.ingest(20, vec![node_cpu_sample(now_ms(), 0.5)]).await;
    assert_eq!(resp.outcome, "rate_limited");
    assert!(resp.retry_after_seconds > 0);
}

#[tokio::test]
async fn unavailable_store_is_a_typed_outcome() {
    let ops_db = TestDb::new().await;
    let service = MonitoringIngestImplementation::new(
        None,
        NodeRepository::new(ops_db.pool.clone()),
        ObservedStateRepository::new(ops_db.pool.clone()),
        MonitoringHealth::new(),
        20,
        0,
    );
    let mut req = proto::NodeMetricBatchRequest {
        meta: None,
        node_id: "node-ingest-1".to_string(),
        schema_version: 1,
        boot_id: "b".to_string(),
        sequence: 0,
        sent_at_ms: 0,
        samples: vec![node_cpu_sample(now_ms(), 0.5)],
    };
    req.samples[0].target_id = req.node_id.clone();
    let resp = service.ingest_node_metric_batch(req).await.unwrap();
    assert_eq!(resp.outcome, "ingestion_unavailable");
    assert_eq!(resp.accepted_samples, 0);
    assert_eq!(service.health().snapshot().unavailable_batches, 1);
}

/// G2 gate evidence (disk-full trigger): a full monitoring filesystem
/// degrades monitoring with the typed `ingestion_unavailable` outcome
/// and a headroom-flavored health reason, while VM lifecycle — the
/// operational store's state-report path, the same database
/// reconciliation depends on — keeps accepting writes. When headroom
/// returns, the next batch is accepted and the degradation clears.
///
/// Disk-full is induced by a headroom floor above the tempdir
/// filesystem's real free space: the identical statvfs read and
/// comparison production performs every batch.
#[tokio::test]
async fn disk_full_degrades_monitoring_but_not_lifecycle() {
    let mut fixture = Fixture::new().await;
    let real_free = chv_monitoring_store::headroom::available_bytes(fixture._monitoring_dir.path())
        .expect("statvfs on the monitoring dir");
    // 64 MiB above the real free space, not +1: free space on a live
    // filesystem moves by kilobytes between the read here and the
    // service's own probe (other processes, tempdir neighbors) — a
    // one-byte margin races that churn and flakes. 64 MiB is far above
    // any incidental drift and still the identical statvfs comparison
    // production performs every batch.
    fixture.set_headroom_floor(real_free + 64 * 1024 * 1024);

    // Disk-full: typed outcome, nothing committed, health degrades
    // with the headroom reason and counts the unavailable batch.
    let now = now_ms();
    let resp = fixture.ingest(0, vec![node_cpu_sample(now, 0.42)]).await;
    assert_eq!(resp.outcome, "ingestion_unavailable");
    assert_eq!(resp.accepted_samples, 0);
    let snapshot = fixture.health.snapshot();
    let reason = snapshot.degraded_reason.expect("monitoring degraded");
    assert!(reason.contains("headroom"), "reason: {reason}");
    assert_eq!(snapshot.unavailable_batches, 1);
    // The store genuinely has nothing: degraded is not a silent drop
    // of a batch the sender believes was committed.
    let stored = fixture
        .store
        .query_current(
            &chv_monitoring_core::model::TargetKind::Node,
            &fixture.node_id,
            &["node.cpu.capacity_ratio".to_string()],
            None,
            now as u64,
        )
        .await
        .expect("query the degraded store's committed data");
    assert!(stored.is_empty(), "nothing committed while degraded");

    // VM lifecycle is unaffected: the operational store — the same
    // database the state-report and reconciliation paths use — still
    // accepts an observed-state transition while monitoring is
    // degraded.
    fixture
        .observed_state_repo
        .upsert_vm(&chv_controlplane_store::VmObservedStateInput {
            vm_id: chv_controlplane_types::domain::ResourceId::new(&fixture.vm_id).unwrap(),
            observed_generation: Generation::new(2),
            runtime_status: "running".to_string(),
            health_status: Some("ok".to_string()),
            node_id: Some(chv_controlplane_types::domain::NodeId::new(&fixture.node_id).unwrap()),
            cloud_hypervisor_pid: None,
            api_socket_path: None,
            last_error: None,
            last_transition_unix_ms: None,
            observed_unix_ms: now,
        })
        .await
        .expect("state-report path keeps working while monitoring is degraded");

    // Headroom returns (floor back under the real free space): the
    // next batch is durably accepted and the shared health handle
    // clears the degradation.
    fixture.set_headroom_floor(0);
    let resp = fixture
        .ingest(1, vec![node_cpu_sample(now_ms(), 0.42)])
        .await;
    assert_eq!(resp.outcome, "accepted");
    assert_eq!(resp.accepted_samples, 1);
    let snapshot = fixture.health.snapshot();
    assert!(
        snapshot.degraded_reason.is_none(),
        "recovered: {snapshot:?}"
    );
    assert_eq!(
        snapshot.unavailable_batches, 1,
        "history preserved, not reset"
    );
}

/// Counters are integer-valued end to end (exact decimal-string
/// deltas on the wire and in the store): a float counter would be
/// accepted and durably stored, then never surfaced by the
/// integer-only query/rollup paths — rejected at the boundary instead
/// of committing data that can never be read back.
#[tokio::test]
async fn float_counter_values_reject_whole_batch() {
    let f = Fixture::new().await;
    let mut sample = node_cpu_sample(now_ms(), 0.5);
    sample.target_kind = "vm".to_string();
    sample.target_id = f.vm_id.clone();
    sample.metric_id = "vm.block.read_bytes_total".to_string();
    sample.source = "vmm".to_string();
    sample.kind = "counter".to_string();
    sample.unit = "bytes".to_string();
    sample.boot_id = "boot-1".to_string();
    sample.identity_epoch = "epoch-1".to_string();
    sample.value = Some(proto::metric_sample_v1::Value::FloatValue(1234.5));
    let resp = f.ingest(0, vec![sample]).await;
    assert_eq!(resp.outcome, "invalid_batch");
    assert!(resp
        .meta
        .as_ref()
        .unwrap()
        .human_summary
        .contains("integer"));
}
