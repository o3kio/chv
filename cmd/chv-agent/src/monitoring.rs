//! Native monitoring sampler wiring for `chv-agent` (gates G1/G2,
//! #602).
//!
//! One bounded sampling task, **independent of reconciliation and state
//! reports**, collects node-level contract samples (CPU ratio, load
//! averages, memory, swap, root filesystem, per-interface and
//! per-block-device counters) through `chv-monitoring-core`'s
//! [`run_sampler`], and VM samples (host-accounted CPU/memory gauges and
//! per-device block/net counters from the pinned `vm.counters` map) on
//! the slower VM cadence. Its bounds and health counters live in the
//! sampler crate; this module provides:
//!
//! - [`NodeOsSampleSource`] — the node source adapter (retained
//!   `NodeOsCollector` plus per-cycle `/proc/net/dev` and
//!   `/proc/diskstats` reads);
//! - [`VmSampleSource`] — the VM source adapter over the agent's
//!   `VmRuntime` (identity-fenced `/proc` process readings plus the
//!   VMM's own device counters);
//! - [`LatestSamples`] — the bounded latest-sample store the sink
//!   consumer maintains (per metric+target+dimension key);
//! - [`spawn_monitoring_sampler`] — spawns the loop and returns the
//!   health handle for the `/metrics` surface;
//! - [`spawn_monitoring_ingest_sender`] — the 15 s batch sender that
//!   ships the latest samples to the control plane over the versioned
//!   node metric batch transport (ingestion contract v1), on a
//!   **dedicated** control-plane client so manager backpressure can
//!   never pause reconciliation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chv_monitoring_core::model::{
    Sample, SampleBuilder, SampleQuality, SampleValue, Source, TargetKind,
};
use chv_monitoring_core::node_os::{NodeOsCollector, NodeOsSnapshot};
use chv_monitoring_core::sampler::{
    run_sampler, NodeOsSource, SamplerConfig, SamplerError, SamplerEvent, SamplerHealth,
    SamplerSources, VmRuntimeSource,
};
use tokio::sync::Mutex;

/// Retained node collector shared by the sampling task. The retention is
/// the CPU-interval fix (see `chv-monitoring-core::node_os`).
struct NodeOsSampleSource {
    node_id: String,
    collector: Mutex<NodeOsCollector>,
    proc_root: &'static Path,
}

impl NodeOsSampleSource {
    fn new(node_id: String) -> Self {
        NodeOsSampleSource {
            node_id,
            collector: Mutex::new(NodeOsCollector::new()),
            proc_root: Path::new("/proc"),
        }
    }
}

/// Build one node sample; a construction failure (should be impossible
/// for registry node metrics) is a source error, never a zero-valued
/// sample.
fn node_sample(
    node_id: &str,
    metric: &str,
    observed_ms: u64,
    value: Option<SampleValue>,
) -> Result<Sample, SamplerError> {
    let mut b = SampleBuilder::new(
        TargetKind::Node,
        node_id,
        metric,
        Source::NodeOs,
        observed_ms,
    )
    .map_err(|e| SamplerError::Source(e.to_string()))?;
    match value {
        Some(v) => b = b.value(v),
        None => b = b.quality(SampleQuality::InsufficientSamples),
    }
    b.build().map_err(|e| SamplerError::Source(e.to_string()))
}

/// Counter samples need their epoch (boot id + a stable identity for the
/// series); interface/device counters restart with the host, and the
/// agent does not survive a host reboot, so the node identity is the
/// fence.
fn node_counter_sample(
    node_id: &str,
    metric: &str,
    observed_ms: u64,
    boot_id: &str,
    series: &str,
    value: u64,
    dimension: (&str, &str),
) -> Result<Sample, SamplerError> {
    let err = |e: chv_monitoring_core::model::SampleError| SamplerError::Source(e.to_string());
    let builder = SampleBuilder::new(
        TargetKind::Node,
        node_id,
        metric,
        Source::NodeOs,
        observed_ms,
    )
    .map_err(err)?
    .dimension(dimension.0, dimension.1)
    .map_err(err)?
    .epoch(boot_id, series)
    .value(SampleValue::Integer(value));
    builder.build().map_err(err)
}

#[async_trait::async_trait]
impl NodeOsSource for NodeOsSampleSource {
    async fn collect_node(&self) -> Result<Vec<Sample>, SamplerError> {
        let snapshot = {
            let mut collector = self.collector.lock().await;
            collector.snapshot()
        };
        let mut samples = Vec::with_capacity(16);

        let NodeOsSnapshot {
            sysinfo_available,
            observed_at_ms,
            cpu_capacity_ratio,
            cpu_observed_at_ms,
            load1,
            load5,
            load15,
            memory_total_bytes,
            memory_available_bytes,
            swap_used_bytes,
            root_fs_total_bytes,
            root_fs_available_bytes,
            ..
        } = snapshot;

        // CPU: no valid interval yet ⇒ insufficient_samples, never zero.
        // The sample's observed_at is the measurement time, not the
        // cycle time — a retained reading (sub-interval cycle) is older
        // than this cycle and must not be stamped as fresh.
        samples.push(node_sample(
            &self.node_id,
            "node.cpu.capacity_ratio",
            if cpu_capacity_ratio.is_some() && cpu_observed_at_ms > 0 {
                cpu_observed_at_ms
            } else {
                observed_at_ms
            },
            cpu_capacity_ratio.map(SampleValue::Float),
        )?);
        for (metric, value) in [
            ("node.cpu.load1", load1),
            ("node.cpu.load5", load5),
            ("node.cpu.load15", load15),
        ] {
            samples.push(node_sample(
                &self.node_id,
                metric,
                observed_at_ms,
                value.map(SampleValue::Float),
            )?);
        }

        // Memory/swap are sysinfo-backed. If sysinfo is unavailable the
        // struct carries placeholders; emit them as unavailable rather than
        // as real zero observations (available=0 is alert-shaped).
        for (metric, value) in [
            ("node.memory.total_bytes", memory_total_bytes),
            ("node.memory.available_bytes", memory_available_bytes),
            ("node.swap.used_bytes", swap_used_bytes),
        ] {
            let err =
                |e: chv_monitoring_core::model::SampleError| SamplerError::Source(e.to_string());
            let mut b = SampleBuilder::new(
                TargetKind::Node,
                &self.node_id,
                metric,
                Source::NodeOs,
                observed_at_ms,
            )
            .map_err(err)?;
            if sysinfo_available {
                b = b.value(SampleValue::Integer(value));
            } else {
                b = b.quality(SampleQuality::Unavailable);
            }
            samples.push(b.build().map_err(err)?);
        }

        for (metric, value) in [
            ("node.fs.total_bytes", root_fs_total_bytes),
            ("node.fs.available_bytes", root_fs_available_bytes),
        ] {
            let err =
                |e: chv_monitoring_core::model::SampleError| SamplerError::Source(e.to_string());
            let mut b = SampleBuilder::new(
                TargetKind::Node,
                &self.node_id,
                metric,
                Source::NodeOs,
                observed_at_ms,
            )
            .map_err(err)?
            .dimension("mount_id", "/")
            .map_err(err)?;
            match value {
                Some(v) => b = b.value(SampleValue::Integer(v)),
                None => b = b.quality(SampleQuality::Unavailable),
            }
            samples.push(b.build().map_err(err)?);
        }

        // Per-interface and per-block-device counters, labeled — never
        // summed blindly across bridges or stacked devices. Counter
        // samples need the boot epoch; if /proc is unreadable we keep
        // the gauge samples above (memory, fs, …) and skip counters
        // this cycle rather than failing the whole batch — a source
        // failure is counted by the sampler's health counters either
        // way, and the log line names the actual cause.
        let boot_id = chv_monitoring_core::process_probe::read_boot_id(self.proc_root);
        let boot_id = match &boot_id {
            Ok(b) => Some(b.as_str()),
            Err(e) => {
                tracing::warn!(error = %e, "node sampler: boot_id unreadable, skipping counter samples this cycle");
                None
            }
        };
        if let Some(boot_id) = boot_id {
            match chv_monitoring_core::proc_net::read_net_dev(self.proc_root) {
                Ok(interfaces) => {
                    for (iface, counters) in interfaces {
                        let series = format!("iface-{iface}");
                        samples.push(node_counter_sample(
                            &self.node_id,
                            "node.net.rx_bytes_total",
                            observed_at_ms,
                            boot_id,
                            &series,
                            counters.rx_bytes,
                            ("interface_id", iface.as_str()),
                        )?);
                        samples.push(node_counter_sample(
                            &self.node_id,
                            "node.net.tx_bytes_total",
                            observed_at_ms,
                            boot_id,
                            &series,
                            counters.tx_bytes,
                            ("interface_id", iface.as_str()),
                        )?);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "node sampler: /proc/net/dev unreadable, skipping interface counters this cycle");
                }
            }
            match chv_monitoring_core::proc_net::read_diskstats(self.proc_root) {
                Ok(devices) => {
                    for (device, counters) in devices {
                        let series = format!("block-{device}");
                        samples.push(node_counter_sample(
                            &self.node_id,
                            "node.block.read_bytes_total",
                            observed_at_ms,
                            boot_id,
                            &series,
                            counters.read_bytes,
                            ("block_device_id", device.as_str()),
                        )?);
                        samples.push(node_counter_sample(
                            &self.node_id,
                            "node.block.write_bytes_total",
                            observed_at_ms,
                            boot_id,
                            &series,
                            counters.write_bytes,
                            ("block_device_id", device.as_str()),
                        )?);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "node sampler: /proc/diskstats unreadable, skipping block counters this cycle");
                }
            }
        }

        Ok(samples)
    }
}

/// VM sample source over the agent's `VmRuntime`. Read-only: the
/// observed-set is `VmRuntime::list()` filtered to running VMs, and
/// every reading comes from `vm_counters` (the identity-fenced `/proc`
/// process probe plus the pinned flat `vm.counters` device map). This
/// adapter never mutates VM state and never blocks reconciliation.
pub struct VmSampleSource {
    runtime: chv_agent_core::vm_runtime::VmRuntime,
}

impl VmSampleSource {
    pub fn new(runtime: chv_agent_core::vm_runtime::VmRuntime) -> Self {
        Self { runtime }
    }
}

fn vm_sample_err(e: chv_monitoring_core::model::SampleError) -> SamplerError {
    SamplerError::Source(e.to_string())
}

/// One VM gauge sample; `value: None` ⇒ a non-valid quality sample
/// (missing data is never zero).
fn vm_gauge(
    vm_id: &str,
    metric: &str,
    observed_ms: u64,
    value: Option<SampleValue>,
    quality: SampleQuality,
) -> Result<Sample, SamplerError> {
    let mut b = SampleBuilder::new(TargetKind::Vm, vm_id, metric, Source::Vmm, observed_ms)
        .map_err(vm_sample_err)?;
    match value {
        Some(v) => b = b.value(v),
        None => b = b.quality(quality),
    }
    b.build().map_err(vm_sample_err)
}

/// One VM counter sample with its VMM-process epoch fence.
fn vm_counter(
    vm_id: &str,
    metric: &str,
    observed_ms: u64,
    boot_id: &str,
    identity_epoch: &str,
    value: u64,
    dimension: (&str, &str),
) -> Result<Sample, SamplerError> {
    SampleBuilder::new(TargetKind::Vm, vm_id, metric, Source::Vmm, observed_ms)
        .map_err(vm_sample_err)?
        .dimension(dimension.0, dimension.1)
        .map_err(vm_sample_err)?
        .epoch(boot_id, identity_epoch)
        .value(SampleValue::Integer(value))
        .build()
        .map_err(vm_sample_err)
}

#[async_trait::async_trait]
impl VmRuntimeSource for VmSampleSource {
    async fn vm_ids(&self) -> Result<Vec<String>, SamplerError> {
        Ok(self
            .runtime
            .list()
            .await
            .into_iter()
            .filter(|record| record.runtime_status == "Running")
            .map(|record| record.vm_id)
            .collect())
    }

    async fn collect_vm(&self, vm_id: &str) -> Result<Vec<Sample>, SamplerError> {
        let counters = self
            .runtime
            .vm_counters(vm_id)
            .await
            .map_err(|e| SamplerError::Source(e.to_string()))?;
        let observed_ms = chv_monitoring_core::node_os::unix_now_ms();
        let mut samples = Vec::with_capacity(8);

        // Host-accounted CPU: percent-of-one-core across the VMM
        // process. Unmeasured (first interval, epoch reset, lost
        // identity) ⇒ insufficient_samples, never zero.
        samples.push(vm_gauge(
            vm_id,
            "vm.cpu.cores_used",
            observed_ms,
            if counters.cpu_percent_measured {
                Some(SampleValue::Float(counters.cpu_percent / 100.0))
            } else {
                None
            },
            SampleQuality::InsufficientSamples,
        )?);

        // Host-accounted memory (VMM RSS). Unreadable identity ⇒
        // unavailable, never zero.
        samples.push(vm_gauge(
            vm_id,
            "vm.memory.host_accounted_bytes",
            observed_ms,
            if counters.memory_measured {
                Some(SampleValue::Integer(counters.memory_bytes_used))
            } else {
                None
            },
            SampleQuality::Unavailable,
        )?);

        // Per-device counters, attributed to the VMM's own device ids —
        // never summed across devices. Without the epoch fence (the
        // VMM process identity could not be established this cycle) no
        // counter is emitted: a counter that cannot be fenced across a
        // VMM restart cannot be delta-subtracted safely.
        if let Some((boot_id, identity_epoch)) = &counters.counter_epoch {
            for (device, value) in &counters.disk_read_by_device {
                samples.push(vm_counter(
                    vm_id,
                    "vm.block.read_bytes_total",
                    observed_ms,
                    boot_id,
                    identity_epoch,
                    *value,
                    ("block_device_id", device.as_str()),
                )?);
            }
            for (device, value) in &counters.disk_write_by_device {
                samples.push(vm_counter(
                    vm_id,
                    "vm.block.write_bytes_total",
                    observed_ms,
                    boot_id,
                    identity_epoch,
                    *value,
                    ("block_device_id", device.as_str()),
                )?);
            }
            for (device, value) in &counters.net_rx_by_device {
                samples.push(vm_counter(
                    vm_id,
                    "vm.net.rx_bytes_total",
                    observed_ms,
                    boot_id,
                    identity_epoch,
                    *value,
                    ("interface_id", device.as_str()),
                )?);
            }
            for (device, value) in &counters.net_tx_by_device {
                samples.push(vm_counter(
                    vm_id,
                    "vm.net.tx_bytes_total",
                    observed_ms,
                    boot_id,
                    identity_epoch,
                    *value,
                    ("interface_id", device.as_str()),
                )?);
            }
        }

        Ok(samples)
    }
}

/// Bounded store of the latest sample per series key
/// (metric, target, dimensions). The node surface is inherently small
/// (registry metrics × node × interfaces/devices); the store replaces
/// each key's entry every cycle, so memory is bounded by the node's own
/// device count — the same bound the collector reads.
#[derive(Debug, Default)]
pub struct LatestSamples {
    inner: Mutex<HashMap<String, Sample>>,
}

impl LatestSamples {
    pub fn new() -> Self {
        Self::default()
    }

    fn key(sample: &Sample) -> String {
        let dims: Vec<String> = sample
            .dimensions
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        format!(
            "{}|{}|{:?}|{}",
            sample.metric_id,
            sample.target_id,
            sample.source,
            dims.join(",")
        )
    }

    /// Replace the stored sample for each series in the batch.
    pub async fn replace(&self, batch: Vec<Sample>) {
        let mut inner = self.inner.lock().await;
        for sample in batch {
            inner.insert(Self::key(&sample), sample);
        }
    }

    /// Reconcile VM-target series against the sampler's observed
    /// roster: every stored VM-target series whose target is NOT in
    /// the roster is stale by construction (the VM left this node) and
    /// is dropped. Node-target series are never touched — a roster
    /// event only ever describes VM targets. Called only when the
    /// sampler's listing SUCCEEDED; an unknown roster must not evict.
    pub async fn retain_vms(&self, roster: &[String]) {
        let mut inner = self.inner.lock().await;
        let before = inner.len();
        inner.retain(|_, s| s.target_kind != TargetKind::Vm || roster.contains(&s.target_id));
        let evicted = before - inner.len();
        if evicted > 0 {
            tracing::debug!(
                evicted,
                live = roster.len(),
                "evicted VM-target series no longer on this node"
            );
        }
    }

    /// The latest samples (any order), pruned of series whose stored
    /// observation aged past `max_age_ms` — a sample the control plane
    /// would reject for age must never poison the whole batch (the
    /// ingestion contract rejects an over-age sample together with
    /// everything shipped beside it).
    pub async fn all_fresh(&self, now_ms: u64, max_age_ms: u64) -> Vec<Sample> {
        let mut inner = self.inner.lock().await;
        inner.retain(|_, s| now_ms.saturating_sub(s.observed_at_ms) <= max_age_ms);
        inner.values().cloned().collect()
    }

    /// The latest samples (any order). Consumed by PR-2's node ingest;
    /// test-used until then.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn all(&self) -> Vec<Sample> {
        self.inner.lock().await.values().cloned().collect()
    }

    /// Number of series currently stored. Consumed by PR-2's node
    /// ingest; test-used until then.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn len(&self) -> usize {
        self.inner.lock().await.len()
    }

    /// Whether no series is stored. Consumed by PR-2's node ingest;
    /// test-used until then.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn is_empty(&self) -> bool {
        self.inner.lock().await.is_empty()
    }
}

/// Spawn the monitoring sampler: the bounded loop plus the sink consumer
/// that maintains [`LatestSamples`]. Returns the shared health handle
/// for the `/metrics` surface and the store for the batch sender.
pub fn spawn_monitoring_sampler(
    node_id: String,
    vm_runtime: chv_agent_core::vm_runtime::VmRuntime,
    config: SamplerConfig,
) -> (Arc<SamplerHealth>, Arc<LatestSamples>) {
    let health = Arc::new(SamplerHealth::new());
    let store = Arc::new(LatestSamples::new());
    let (tx, mut rx) = tokio::sync::mpsc::channel::<SamplerEvent>(256);

    let sources = SamplerSources {
        node: Arc::new(NodeOsSampleSource::new(node_id)),
        // VM samples: host-accounted process gauges plus the pinned
        // vm.counters device map, on the slower VM cadence.
        vms: Some(Arc::new(VmSampleSource::new(vm_runtime))),
        // stord/nwd expose no attributable v1 metrics yet (volume health
        // exists but is not a registry metric) — no adapters wired, no
        // coverage faked.
        storage: None,
        network: None,
    };

    tokio::spawn(run_sampler(config, sources, tx, health.clone()));
    let consumer_store = store.clone();
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                SamplerEvent::Samples(batch) => consumer_store.replace(batch).await,
                // Reconcile against the roster the sampler actually
                // observed: series for VM targets that left this node
                // (deleted, migrated away) stop being shipped NOW — the
                // control plane fails the WHOLE batch for an unowned VM
                // target, so one departed VM would otherwise poison
                // every future batch from this node.
                SamplerEvent::VmRoster(roster) => {
                    consumer_store.retain_vms(&roster).await;
                }
            }
        }
    });

    (health, store)
}

/// The batch send cadence (15 s; the native spec's node batch timer).
const SEND_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
/// Hard cap per batch (mirrors the control plane's ingestion cap).
const MAX_SAMPLES_PER_BATCH: usize = 512;
/// Reconnect backoff bounds for the dedicated ingest client.
const RECONNECT_BACKOFF_MIN: std::time::Duration = std::time::Duration::from_secs(1);
const RECONNECT_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(60);
/// Samples older than this are never shipped: the control plane's
/// ingestion contract rejects over-age samples together with the whole
/// batch, so one stale series must not poison the node's telemetry.
const SEND_MAX_SAMPLE_AGE_MS: u64 = 2 * 60 * 1000;

/// Convert one contract sample to the wire shape. The sample was built
/// through the contract-validating `SampleBuilder`, so this is a pure
/// field mapping — the control plane re-validates everything anyway.
fn sample_to_proto(
    sample: &Sample,
) -> control_plane_node_api::control_plane_node_api::MetricSampleV1 {
    use control_plane_node_api::control_plane_node_api as proto;
    proto::MetricSampleV1 {
        target_kind: sample.target_kind.as_str().to_string(),
        target_id: sample.target_id.clone(),
        metric_id: sample.metric_id.clone(),
        source: sample.source.as_str().to_string(),
        kind: sample.kind.as_str().to_string(),
        unit: sample.unit.as_str().to_string(),
        observed_at_ms: sample.observed_at_ms as i64,
        value: sample.value.as_ref().map(|v| match v {
            SampleValue::Float(f) => proto::metric_sample_v1::Value::FloatValue(*f),
            SampleValue::Integer(i) => proto::metric_sample_v1::Value::IntegerValue(*i),
        }),
        quality: sample.quality.as_str().to_string(),
        dimensions: sample
            .dimensions
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        boot_id: sample.boot_id.clone().unwrap_or_default(),
        identity_epoch: sample.identity_epoch.clone().unwrap_or_default(),
    }
}

/// Spawn the node metric batch sender (ingestion contract v1): every
/// 15 s, drain [`LatestSamples`] and ship the samples as
/// `IngestNodeMetricBatch` requests on a **dedicated** control-plane
/// client.
///
/// Delivery semantics:
/// - `boot_id` is a **per-agent-run UUID** (a fresh value on agent
///   start, never the host boot id): a restarted agent must never
///   collide with its previous incarnation's sequences.
/// - `sequence` advances only on a durably-committed outcome
///   (`accepted`/`duplicate`); an unknown-fate transport failure
///   retries the same sequence. If the retry carries different
///   samples, the control plane answers `replay_conflict` and the
///   sender starts a fresh epoch (new boot id, sequence 0) — the
///   durable store keeps the first-committed batch, so no double
///   counting is possible either way.
/// - Every failure degrades to retry; nothing here can crash the
///   agent, block reconciliation, or backpressure the sampler (the
///   sender reads the latest-per-series store, so a stalled sender
///   costs freshness, not memory).
pub fn spawn_monitoring_ingest_sender(
    node_id: String,
    samples: Arc<LatestSamples>,
    endpoint: String,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
    ca_cert: Option<PathBuf>,
) {
    tokio::spawn(async move {
        // Per-agent-run sender epoch (ingestion contract v1). Regenerated
        // on replay_conflict/stale_sequence, which a single incarnation
        // should never see — defensively, not as a normal path.
        let mut boot_id = uuid::Uuid::new_v4().to_string();
        let mut sequence: u64 = 0;
        let mut client: Option<chv_agent_core::control_plane::ControlPlaneClient> = None;
        let mut backoff = RECONNECT_BACKOFF_MIN;

        loop {
            // (Re)connect with bounded backoff. Connection failures are
            // expected during control-plane outages and enrollment gaps;
            // monitoring simply waits.
            if client.is_none() {
                match chv_agent_core::control_plane::ControlPlaneClient::new(
                    &endpoint,
                    tls_cert.as_deref(),
                    tls_key.as_deref(),
                    ca_cert.as_deref(),
                )
                .await
                {
                    Ok(c) => {
                        client = Some(c);
                        backoff = RECONNECT_BACKOFF_MIN;
                    }
                    Err(e) => {
                        tracing::debug!(
                            error = %e,
                            "monitoring ingest client connect failed; retrying"
                        );
                        tokio::time::sleep(backoff).await;
                        backoff = std::cmp::min(backoff * 2, RECONNECT_BACKOFF_MAX);
                        continue;
                    }
                }
            }

            tokio::time::sleep(SEND_INTERVAL).await;

            // Age-pruned latest samples: an over-age observation (the
            // control plane rejects samples older than 5 minutes
            // together with the whole batch that carries them) must
            // never poison the node's other series. 2 minutes is well
            // under that cap and far above the sampler cadences.
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let all = samples.all_fresh(now_ms, SEND_MAX_SAMPLE_AGE_MS).await;
            if all.is_empty() {
                continue;
            }
            // Chunk to the per-batch cap; each chunk is its own batch
            // with its own sequence.
            for chunk in all.chunks(MAX_SAMPLES_PER_BATCH) {
                use control_plane_node_api::control_plane_node_api as proto;
                let request = proto::NodeMetricBatchRequest {
                    meta: None,
                    node_id: node_id.clone(),
                    schema_version: 1,
                    boot_id: boot_id.clone(),
                    sequence,
                    sent_at_ms: chv_monitoring_core::node_os::unix_now_ms() as i64,
                    samples: chunk.iter().map(sample_to_proto).collect(),
                };
                let Some(c) = client.as_mut() else {
                    break;
                };
                match c.ingest_node_metric_batch(request).await {
                    Ok(resp) => match resp.outcome.as_str() {
                        "accepted" | "duplicate" => {
                            sequence = sequence.wrapping_add(1);
                        }
                        "rate_limited" => {
                            // Respect the advertised backoff; the batch
                            // was not committed, so the sequence does
                            // not advance and the same batch retries.
                            let pause = resp.retry_after_seconds.clamp(1, 60) as u64;
                            tokio::time::sleep(std::time::Duration::from_secs(pause)).await;
                        }
                        "replay_conflict" | "stale_sequence" => {
                            // Defensive: this incarnation's sequence
                            // history disagrees with the durable store.
                            // Start a fresh sender epoch — never resend
                            // the old one.
                            tracing::warn!(
                                outcome = resp.outcome.as_str(),
                                "monitoring ingest sequence conflict; starting a fresh sender epoch"
                            );
                            boot_id = uuid::Uuid::new_v4().to_string();
                            sequence = 0;
                        }
                        "ingestion_unavailable" => {
                            // Monitoring degraded control-plane-side;
                            // retry on the next tick.
                        }
                        other => {
                            // invalid_batch / batch_too_large /
                            // unsupported_metric / series_cap_exceeded:
                            // our samples violate the contract or caps.
                            // The samples will be replaced by the next
                            // sampler cycle; log and move on — never
                            // spin on a poison batch.
                            tracing::warn!(
                                outcome = other,
                                accepted = resp.accepted_samples,
                                "monitoring batch rejected"
                            );
                            sequence = sequence.wrapping_add(1);
                        }
                    },
                    Err(e) => {
                        // Unknown fate: the batch may or may not be
                        // committed. Drop the client (reconnect next
                        // tick) and retry the same sequence with the
                        // then-latest samples — the durable dedup on
                        // the control plane arbitrates.
                        tracing::debug!(error = %e, "monitoring ingest transport failed");
                        client = None;
                        break;
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn node_source_produces_contract_samples() {
        let source = NodeOsSampleSource::new("node-test".to_string());
        let samples = source.collect_node().await.unwrap();
        let ids: Vec<&str> = samples.iter().map(|s| s.metric_id.as_str()).collect();
        for expected in [
            "node.cpu.capacity_ratio",
            "node.cpu.load1",
            "node.cpu.load5",
            "node.cpu.load15",
            "node.memory.total_bytes",
            "node.memory.available_bytes",
            "node.swap.used_bytes",
            "node.fs.total_bytes",
            "node.fs.available_bytes",
        ] {
            assert!(ids.contains(&expected), "missing {expected}");
        }
        // Interface/block samples appear only when the host actually
        // reports them (a namespace with only `lo` or a masked
        // diskstats legitimately yields none).
        let has_interfaces =
            !chv_monitoring_core::proc_net::read_net_dev(std::path::Path::new("/proc"))
                .map(|m| m.is_empty())
                .unwrap_or(true);
        if has_interfaces {
            assert!(samples
                .iter()
                .any(|s| s.metric_id == "node.net.rx_bytes_total"));
        }
        let has_devices =
            !chv_monitoring_core::proc_net::read_diskstats(std::path::Path::new("/proc"))
                .map(|m| m.is_empty())
                .unwrap_or(true);
        if has_devices {
            assert!(samples
                .iter()
                .any(|s| s.metric_id == "node.block.read_bytes_total"));
        }
        // Counters carry their epoch.
        for s in samples
            .iter()
            .filter(|s| s.kind == chv_monitoring_core::model::MetricKind::Counter)
        {
            assert!(s.boot_id.is_some() && s.identity_epoch.is_some());
            assert!(s.value.is_some(), "counter sample without value");
        }
    }

    #[tokio::test]
    async fn first_cycle_cpu_is_insufficient_not_zero() {
        let source = NodeOsSampleSource::new("node-test".to_string());
        let samples = source.collect_node().await.unwrap();
        let cpu = samples
            .iter()
            .find(|s| s.metric_id == "node.cpu.capacity_ratio")
            .unwrap();
        assert_eq!(cpu.quality, SampleQuality::InsufficientSamples);
        assert!(cpu.value.is_none(), "no interval, no value");
    }

    #[tokio::test]
    async fn store_replaces_per_series() {
        let store = LatestSamples::new();
        let s1 = node_sample("n", "node.cpu.load1", 1, Some(SampleValue::Float(1.0))).unwrap();
        let s2 = node_sample("n", "node.cpu.load1", 2, Some(SampleValue::Float(2.0))).unwrap();
        store.replace(vec![s1]).await;
        store.replace(vec![s2]).await;
        let all = store.all().await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].value, Some(SampleValue::Float(2.0)));
        // Different dimension = different series.
        let s3 = node_counter_sample(
            "n",
            "node.net.rx_bytes_total",
            3,
            "boot",
            "iface-eth0",
            10,
            ("interface_id", "eth0"),
        )
        .unwrap();
        store.replace(vec![s3]).await;
        assert_eq!(store.len().await, 2);
    }

    #[tokio::test]
    async fn sampler_end_to_end_fills_the_store() {
        let (health, store) = spawn_monitoring_sampler(
            "node-test".to_string(),
            test_vm_runtime(chv_hypervisor_api::VmCounters::default()).await,
            SamplerConfig {
                interval: std::time::Duration::from_millis(50),
                ..SamplerConfig::default()
            },
        );
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let snap = health.snapshot();
        assert!(snap.cycles_completed >= 2, "sampler cycles ran");
        assert!(!store.is_empty().await, "store filled");
        // The sampler and consumer tasks are detached; the test runtime
        // drops them at exit once the assertions hold.
    }

    /// A VmRuntime over the mock adapter whose `vm_counters` returns the
    /// injected value, holding one running VM (`vm-1`).
    async fn test_vm_runtime(
        counters: chv_hypervisor_api::VmCounters,
    ) -> chv_agent_core::vm_runtime::VmRuntime {
        let adapter = chv_agent_runtime_ch::MockCloudHypervisorAdapter::default();
        *adapter.counters_result.lock().unwrap() = Some(counters);
        let runtime = chv_agent_core::vm_runtime::VmRuntime::new(Arc::new(adapter));
        let config = chv_hypervisor_api::VmConfig {
            vm_id: "vm-1".to_string(),
            cpus: 1,
            memory_bytes: 1024,
            kernel_path: std::path::PathBuf::from("/dev/null"),
            firmware_path: None,
            disks: vec![],
            nics: vec![],
            api_socket_path: std::path::PathBuf::from("/tmp/chv-test-vm-1.sock"),
            cloud_init_userdata: None,
            hypervisor_overrides: None,
            vsock: None,
        };
        runtime.create_vm("vm-1", "1", &config, None).await.unwrap();
        runtime.start_vm("vm-1", None).await.unwrap();
        runtime
    }

    #[tokio::test]
    async fn vm_samples_carry_measured_values_and_epoch_fences() {
        let mut counters = chv_hypervisor_api::VmCounters {
            cpu_percent: 150.0, // 1.5 cores
            cpu_percent_measured: true,
            memory_bytes_used: 4096,
            memory_measured: true,
            counter_epoch: Some(("boot-1".to_string(), "ticks-42".to_string())),
            ..Default::default()
        };
        counters
            .disk_read_by_device
            .insert("_disk0".to_string(), 1000);
        counters.net_rx_by_device.insert("_net1".to_string(), 500);
        let runtime = test_vm_runtime(counters).await;
        let source = VmSampleSource::new(runtime);

        let ids = source.vm_ids().await.unwrap();
        assert_eq!(ids, vec!["vm-1".to_string()]);
        let samples = source.collect_vm("vm-1").await.unwrap();

        let cpu = samples
            .iter()
            .find(|s| s.metric_id == "vm.cpu.cores_used")
            .expect("cpu sample");
        assert_eq!(
            cpu.value,
            Some(chv_monitoring_core::model::SampleValue::Float(1.5))
        );
        assert_eq!(cpu.source, Source::Vmm);

        let memory = samples
            .iter()
            .find(|s| s.metric_id == "vm.memory.host_accounted_bytes")
            .expect("memory sample");
        assert_eq!(
            memory.value,
            Some(chv_monitoring_core::model::SampleValue::Integer(4096))
        );

        let disk = samples
            .iter()
            .find(|s| s.metric_id == "vm.block.read_bytes_total")
            .expect("per-device disk counter");
        assert_eq!(
            disk.value,
            Some(chv_monitoring_core::model::SampleValue::Integer(1000))
        );
        assert_eq!(disk.dimensions.get("block_device_id"), Some("_disk0"));
        assert_eq!(disk.boot_id.as_deref(), Some("boot-1"));
        assert_eq!(disk.identity_epoch.as_deref(), Some("ticks-42"));

        let net = samples
            .iter()
            .find(|s| s.metric_id == "vm.net.rx_bytes_total")
            .expect("per-device net counter");
        assert_eq!(
            net.value,
            Some(chv_monitoring_core::model::SampleValue::Integer(500))
        );
        assert_eq!(net.dimensions.get("interface_id"), Some("_net1"));
    }

    #[tokio::test]
    async fn unmeasured_vm_data_is_never_zero() {
        // All-default counters: nothing measured, no epoch — the honest
        // "VM just started / identity unknown" shape.
        let source =
            VmSampleSource::new(test_vm_runtime(chv_hypervisor_api::VmCounters::default()).await);
        let samples = source.collect_vm("vm-1").await.unwrap();

        let cpu = samples
            .iter()
            .find(|s| s.metric_id == "vm.cpu.cores_used")
            .expect("cpu sample");
        assert!(cpu.value.is_none(), "unmeasured CPU is not zero");
        assert_eq!(
            cpu.quality,
            chv_monitoring_core::model::SampleQuality::InsufficientSamples
        );

        let memory = samples
            .iter()
            .find(|s| s.metric_id == "vm.memory.host_accounted_bytes")
            .expect("memory sample");
        assert!(memory.value.is_none(), "unmeasured memory is not zero");
        assert_eq!(
            memory.quality,
            chv_monitoring_core::model::SampleQuality::Unavailable
        );

        // No epoch ⇒ no counter samples at all (never an unfenced
        // counter, never a zero counter).
        assert!(samples
            .iter()
            .all(|s| s.kind != chv_monitoring_core::model::MetricKind::Counter));
    }

    #[test]
    fn sample_to_proto_maps_the_wire_fields() {
        let sample = SampleBuilder::new(
            TargetKind::Vm,
            "vm-1",
            "vm.block.read_bytes_total",
            Source::Vmm,
            1_700_000_000_000,
        )
        .unwrap()
        .dimension("block_device_id", "_disk0")
        .unwrap()
        .epoch("boot-1", "ticks-42")
        .value(SampleValue::Integer(1000))
        .build()
        .unwrap();
        let proto = sample_to_proto(&sample);
        assert_eq!(proto.target_kind, "vm");
        assert_eq!(proto.target_id, "vm-1");
        assert_eq!(proto.metric_id, "vm.block.read_bytes_total");
        assert_eq!(proto.source, "vmm");
        assert_eq!(proto.kind, "counter");
        assert_eq!(proto.unit, "bytes");
        assert_eq!(proto.observed_at_ms, 1_700_000_000_000);
        assert_eq!(
            proto.value,
            Some(
                control_plane_node_api::control_plane_node_api::metric_sample_v1::Value::IntegerValue(
                    1000
                )
            )
        );
        assert_eq!(proto.quality, "valid");
        assert_eq!(proto.dimensions.get("block_device_id").unwrap(), "_disk0");
        assert_eq!(proto.boot_id, "boot-1");
        assert_eq!(proto.identity_epoch, "ticks-42");

        // Non-valid quality ⇒ no value on the wire (missing data is
        // never encoded as zero).
        let hole = SampleBuilder::new(
            TargetKind::Node,
            "node-1",
            "node.cpu.capacity_ratio",
            Source::NodeOs,
            1_700_000_000_000,
        )
        .unwrap()
        .quality(SampleQuality::InsufficientSamples)
        .build()
        .unwrap();
        let proto = sample_to_proto(&hole);
        assert!(proto.value.is_none());
        assert_eq!(proto.quality, "insufficient_samples");
    }

    /// G2 gate evidence (real-host, qualified pin): a real cloud-
    /// hypervisor VM, created through the PRODUCTION `create_vm` path
    /// (not an adopted stray process), sampled twice through the
    /// production `VmSampleSource`, and its samples durably ingested
    /// into a real file-backed monitoring store and read back through
    /// the history/current query paths — the entire agent-side
    /// pipeline of PR-2 on a qualified real-host VM.
    ///
    /// Skipped unless the qualified-pin env vars are set (CI has no
    /// KVM); the real-host record lives in
    /// `docs/evidence/native-monitoring/g2/README.md`:
    ///
    /// ```sh
    /// CHV_G1_VMM_BINARY=/tmp/opencode/g0b/cloud-hypervisor \
    /// CHV_G1_FIRMWARE=/var/lib/chv/qual/hypervisor-fw \
    /// CHV_G1_IMAGE=/var/lib/chv/qual/images/noble-qual-patched.img \
    /// cargo test -p chv-agent --bin chv-agent g2_real_vmm -- --nocapture
    /// ```
    #[tokio::test]
    async fn g2_real_vmm_samples_persist_to_bounded_history() {
        fn checkpoint(msg: &str) {
            let _ = std::io::Write::write_fmt(
                &mut std::io::stderr(),
                format_args!("g2 checkpoint: {msg}\n"),
            );
        }
        let Ok(vmm_binary) = std::env::var("CHV_G1_VMM_BINARY") else {
            let _ = std::io::Write::write_fmt(
                &mut std::io::stderr(),
                format_args!("skipping: CHV_G1_VMM_BINARY not set (real-KVM evidence test)\n"),
            );
            return;
        };
        let firmware =
            std::env::var("CHV_G1_FIRMWARE").expect("CHV_G1_FIRMWARE with CHV_G1_VMM_BINARY");
        let image = std::env::var("CHV_G1_IMAGE").expect("CHV_G1_IMAGE with CHV_G1_VMM_BINARY");

        let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        // The qualified image is qcow2 content under an `.img` name; the
        // production adapter derives the VMM's `image_type` from the file
        // extension, so expose it under its true extension via a
        // read-only symlink (same bytes — the pinned image is never
        // written, `readonly=on` stays in force).
        let image_qcow2 = dir.path().join("qual-disk.qcow2");
        std::os::unix::fs::symlink(&image, &image_qcow2).expect("symlink the qualified image");
        let adapter = chv_agent_runtime_ch::ProcessCloudHypervisorAdapter::new(
            std::path::PathBuf::from(&vmm_binary),
        );
        let runtime = chv_agent_core::vm_runtime::VmRuntime::new(Arc::new(adapter));

        // The qualification shape (G0b/G1): 2 vCPU, 512 MiB, firmware
        // boot, one READ-ONLY disk — the pinned image is never written.
        let config = chv_hypervisor_api::VmConfig {
            vm_id: "g2-vm".to_string(),
            cpus: 2,
            memory_bytes: 512 * 1024 * 1024,
            kernel_path: std::path::PathBuf::from("/dev/null"),
            firmware_path: Some(std::path::PathBuf::from(&firmware)),
            disks: vec![chv_hypervisor_api::VmDiskConfig {
                path: image_qcow2,
                read_only: true,
                id: None,
            }],
            nics: vec![],
            api_socket_path: dir.path().join("vms/g2-vm/vm.sock"),
            cloud_init_userdata: None,
            hypervisor_overrides: None,
            vsock: None,
        };
        checkpoint("creating vm");
        runtime
            .create_vm("g2-vm", "g2-1", &config, None)
            .await
            .expect("production create_vm boots the qualified VMM");
        checkpoint("create_vm done");
        runtime
            .start_vm("g2-vm", None)
            .await
            .expect("production start_vm confirms the running VM");
        checkpoint("start_vm done");

        let source = VmSampleSource::new(runtime.clone());

        // Let the firmware's early boot settle (G1 harness discipline:
        // the counters endpoint reports real device activity within the
        // first seconds; an immediate probe races the VMM's own boot).
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        // First collection: baselines the CPU interval (no rate yet —
        // insufficient_samples, never zero) and reads real device
        // counters + RSS with the epoch fence.
        checkpoint("collecting first");
        let first = source.collect_vm("g2-vm").await.expect("first samples");
        checkpoint("first samples collected");
        let _ = std::io::Write::write_fmt(
            &mut std::io::stderr(),
            format_args!("g2 first samples: {first:#?}\n"),
        );
        let first_cpu = first
            .iter()
            .find(|s| s.metric_id == "vm.cpu.cores_used")
            .unwrap();
        assert!(
            first_cpu.value.is_none() && first_cpu.quality == SampleQuality::InsufficientSamples,
            "first observation has no CPU interval: {first_cpu:?}"
        );
        let first_memory = first
            .iter()
            .find(|s| s.metric_id == "vm.memory.host_accounted_bytes")
            .unwrap();
        assert!(
            matches!(first_memory.value, Some(SampleValue::Integer(m)) if m > 10_000_000),
            "real VMM RSS for a 512M VM must be well above 10 MB: {first_memory:?}"
        );
        let first_disk = first
            .iter()
            .find(|s| s.metric_id == "vm.block.read_bytes_total")
            .expect("per-device disk counter");
        assert!(
            matches!(first_disk.value, Some(SampleValue::Integer(v)) if v > 1_000_000),
            "real VMM must report the firmware's disk reads: {first_disk:?}"
        );
        assert!(first_disk.boot_id.is_some() && first_disk.identity_epoch.is_some());

        // Second collection after a real interval: a measured CPU rate
        // (the booting guest is I/O- and CPU-active) and the same
        // epoch-fenced counter series.
        tokio::time::sleep(std::time::Duration::from_secs(6)).await;
        checkpoint("collecting second");
        let second = source.collect_vm("g2-vm").await.expect("second samples");
        checkpoint("second samples collected");
        let _ = std::io::Write::write_fmt(
            &mut std::io::stderr(),
            format_args!("g2 second samples: {second:#?}\n"),
        );
        let second_cpu = second
            .iter()
            .find(|s| s.metric_id == "vm.cpu.cores_used")
            .unwrap();
        assert!(
            matches!(second_cpu.value, Some(SampleValue::Float(_))),
            "a real CPU interval over a booting guest yields a rate: {second_cpu:?}"
        );
        let second_disk = second
            .iter()
            .find(|s| s.metric_id == "vm.block.read_bytes_total")
            .unwrap();
        assert_eq!(
            second_disk.identity_epoch, first_disk.identity_epoch,
            "same VMM incarnation, same epoch fence"
        );

        // Durable history: ingest both collections as node batches into
        // a real file-backed monitoring store (the CP-side service does
        // exactly this after its validation), then read the history
        // back through the query path the BFF serves.
        let store_dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let store = Arc::new(
            chv_monitoring_store::MonitoringStore::connect(
                chv_monitoring_store::MonitoringStoreConfig {
                    database_url: format!("sqlite://{}/monitoring.db", store_dir.path().display()),
                    migrations_dir: std::path::PathBuf::from(concat!(
                        env!("CARGO_MANIFEST_DIR"),
                        "/../../cmd/chv-controlplane/monitoring-migrations"
                    )),
                    ..chv_monitoring_store::MonitoringStoreConfig::default()
                },
            )
            .await
            .expect("monitoring store connect"),
        );
        checkpoint("ingesting to store");
        let now = chv_monitoring_core::node_os::unix_now_ms();
        for (sequence, samples) in [(0u64, &first), (1, &second)] {
            let batch = chv_monitoring_store::NodeBatch {
                boot_id: "g2-agent-boot".to_string(),
                sequence,
                sent_at_ms: now,
                samples: samples.clone(),
            };
            let outcome = store
                .ingest_node_batch("g2-node", &batch, now)
                .await
                .expect("durable ingest");
            let _ = std::io::Write::write_fmt(
                &mut std::io::stderr(),
                format_args!("g2 ingest batch {sequence}: {outcome:?}\n"),
            );
            assert!(
                matches!(
                    outcome,
                    chv_monitoring_store::IngestOutcome::Accepted { .. }
                ),
                "batch {sequence} durably accepted: {outcome:?}"
            );
        }

        let series = store
            .query_history(
                &chv_monitoring_core::model::TargetKind::Vm,
                "g2-vm",
                &[
                    "vm.cpu.cores_used".to_string(),
                    "vm.block.read_bytes_total".to_string(),
                ],
                None,
                now - 60_000,
                now,
                100,
                chv_monitoring_store::Resolution::Raw,
            )
            .await
            .expect("history query");
        let cores = series
            .iter()
            .find(|s| s.metric_id == "vm.cpu.cores_used")
            .expect("cores series in history");
        let valid_points: Vec<_> = cores
            .points
            .iter()
            .filter(|p| p.quality == SampleQuality::Valid)
            .collect();
        assert!(
            !valid_points.is_empty(),
            "the measured second observation is in durable history: {cores:?}"
        );
        let disk = series
            .iter()
            .find(|s| s.metric_id == "vm.block.read_bytes_total")
            .expect("disk series in history");
        assert!(
            disk.points
                .iter()
                .any(|p| p.quality == SampleQuality::Valid),
            "epoch-fenced disk counters are in durable history: {disk:?}"
        );

        checkpoint("query done; cleaning up");
        let _ = runtime.delete_vm("g2-vm", None).await;
        checkpoint("delete_vm done");
    }

    fn vm_memory_sample(vm_id: &str, observed_ms: u64) -> Sample {
        SampleBuilder::new(
            TargetKind::Vm,
            vm_id,
            "vm.memory.host_accounted_bytes",
            Source::Vmm,
            observed_ms,
        )
        .unwrap()
        .value(SampleValue::Integer(402_264_064))
        .build()
        .unwrap()
    }

    fn node_cpu_sample(observed_ms: u64) -> Sample {
        SampleBuilder::new(
            TargetKind::Node,
            "node-1",
            "node.cpu.capacity_ratio",
            Source::NodeOs,
            observed_ms,
        )
        .unwrap()
        .value(SampleValue::Float(0.42))
        .build()
        .unwrap()
    }

    /// The round-2 review finding, pinned: a VM that left this node
    /// (deleted, migrated) must stop being shipped IMMEDIATELY — the
    /// control plane rejects the WHOLE batch for an unowned VM target,
    /// so one stale series would poison every future batch from the
    /// node. The sampler's roster event drives eviction; node series
    /// are never collateral damage.
    #[tokio::test]
    async fn departed_vm_series_are_evicted_by_roster() {
        let store = LatestSamples::new();
        let now = 1_700_000_000_000;
        store
            .replace(vec![
                node_cpu_sample(now),
                vm_memory_sample("vm-a", now),
                vm_memory_sample("vm-b", now),
            ])
            .await;
        assert_eq!(store.len().await, 3);

        // Roster shrinks to vm-a: vm-b's series must go, the rest stay.
        store.retain_vms(&["vm-a".to_string()]).await;
        let all = store.all().await;
        assert!(all.iter().all(|s| s.target_id != "vm-b"), "vm-b evicted");
        assert!(all.iter().any(|s| s.target_id == "vm-a"));
        assert!(all.iter().any(|s| s.target_id == "node-1"));

        // Empty roster (all VMs gone): every VM series goes, node
        // series survive.
        store.retain_vms(&[]).await;
        let all = store.all().await;
        assert!(
            all.iter().all(|s| s.target_kind != TargetKind::Vm),
            "no VM series remain"
        );
        assert!(all.iter().any(|s| s.target_id == "node-1"));
    }

    fn node_swap_sample(observed_ms: u64) -> Sample {
        SampleBuilder::new(
            TargetKind::Node,
            "node-1",
            "node.swap.used_bytes",
            Source::NodeOs,
            observed_ms,
        )
        .unwrap()
        .value(SampleValue::Integer(0))
        .build()
        .unwrap()
    }

    /// Belt to the roster braces: a stored observation that aged past
    /// the send cutoff is never shipped — the control plane rejects
    /// over-age samples together with the whole batch carrying them.
    #[tokio::test]
    async fn stale_samples_are_pruned_from_the_send_set() {
        let store = LatestSamples::new();
        let now = 1_700_000_060_000;
        store
            .replace(vec![
                node_cpu_sample(now),
                // 5 minutes old: past the 2-minute send cutoff.
                node_swap_sample(now - 300_000),
                vm_memory_sample("vm-a", now - 300_000),
            ])
            .await;
        assert_eq!(store.len().await, 3);

        let fresh = store.all_fresh(now, 2 * 60 * 1000).await;
        assert_eq!(fresh.len(), 1, "only the fresh observation survives");
        assert_eq!(fresh[0].target_id, "node-1");
        // The prune is durable, not a read-time filter.
        assert_eq!(store.len().await, 1);
    }
}
