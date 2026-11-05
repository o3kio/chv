use axum::{extract::State, response::IntoResponse, routing::get, Router};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

use chv_monitoring_core::node_os::NodeOsCollector;

use crate::connectivity::ConnectivityState;

// Metric name constants for external consumers and consistency.
pub const METRIC_NODE_STATE: &str = "chv_agent_node_state";
pub const METRIC_VMS_TOTAL: &str = "chv_agent_vms_total";
pub const METRIC_RECONCILE_TICKS: &str = "chv_agent_reconcile_ticks_total";
pub const METRIC_RECONCILE_FAILURES: &str = "chv_agent_reconcile_failures_total";
pub const METRIC_HEALTH_FAILURES: &str = "chv_agent_health_failures_total";
pub const METRIC_UPTIME: &str = "chv_agent_uptime_seconds";
pub const METRIC_CP_CONNECTED: &str = "chv_agent_cp_connected";
pub const METRIC_RECONCILE_DRIFT_EMA: &str = "chv_agent_reconcile_drift_ema";
pub const METRIC_RECONCILE_DURATION: &str = "chv_agent_reconcile_duration_seconds";

pub struct HostResources {
    pub cpu_usage_percent: f32,
    pub memory_total_bytes: u64,
    pub memory_used_bytes: u64,
    pub disk_total_bytes: u64,
    pub disk_available_bytes: u64,
}

impl Default for HostResources {
    fn default() -> Self {
        Self {
            cpu_usage_percent: 0.0,
            memory_total_bytes: 0,
            memory_used_bytes: 0,
            disk_total_bytes: 0,
            disk_available_bytes: 0,
        }
    }
}

/// Shared state exposed via the Prometheus metrics endpoint.
/// Core journal (core-managed mode) health metrics. Absent (`None`) in
/// legacy mode — the journal poller only exists in core-managed mode.
#[derive(Debug, Clone, Copy)]
pub struct CoreJournalMetrics {
    /// Cumulative failed journal scans since agent start.
    pub scan_failures_total: u64,
    /// Whether the most recent journal scan succeeded (and the executor is
    /// not fatally terminated).
    pub healthy: bool,
    /// Current count of stuck operations: status `running` with a recovery
    /// marker (InspectRequired) awaiting operator resolution.
    pub inspect_required: u64,
}

pub struct MetricsState {
    pub node_id: String,
    pub node_state: String,
    pub vm_count: usize,
    pub tick_count: u64,
    pub reconcile_failures: u32,
    pub health_failures: u32,
    pub start_time: Instant,
    // Connectivity tracking
    pub cp_connectivity_state: ConnectivityState,
    pub cp_disconnected_duration_ms: i64,
    pub cp_consecutive_failures: u32,
    pub cp_total_deferred_messages: u64,
    // Host resources
    pub host: HostResources,
    // Reconcile performance metrics
    pub reconcile_drift_ema: f64,
    pub last_reconcile_duration_ms: u64,
    // Resource pressure indicators
    pub disk_pressure: bool,
    pub memory_pressure: bool,
    // Core journal (core-managed mode only)
    pub core_journal: Option<CoreJournalMetrics>,
    // Native monitoring sampler health (G1): present once the sampler
    // task is spawned. Counters/gauges only — no VM identifiers.
    pub sampler_health: Option<Arc<chv_monitoring_core::sampler::SamplerHealth>>,
}

impl MetricsState {
    pub fn new(node_id: String) -> Self {
        Self {
            node_id,
            node_state: "Bootstrapping".to_string(),
            vm_count: 0,
            tick_count: 0,
            reconcile_failures: 0,
            health_failures: 0,
            start_time: Instant::now(),
            cp_connectivity_state: ConnectivityState::Disconnected,
            cp_disconnected_duration_ms: 0,
            cp_consecutive_failures: 0,
            cp_total_deferred_messages: 0,
            host: HostResources::default(),
            reconcile_drift_ema: 0.0,
            last_reconcile_duration_ms: 0,
            disk_pressure: false,
            memory_pressure: false,
            core_journal: None,
            sampler_health: None,
        }
    }
}

/// Collect current host resource metrics.
///
/// CPU usage comes from a **process-global retained collector**
/// (`chv-monitoring-core`'s `NodeOsCollector`): sysinfo needs two
/// refreshes separated by its minimum interval to measure an interval,
/// so a fresh `System` per scrape would report the **since-boot
/// average** — a constant-ish number that is not the current load (the
/// G1 sampler repair, prompt 01 task 2). The first scrape after agent
/// start has no valid interval yet and reports 0 until the second; a
/// scrape inside the minimum interval retains the last valid reading.
fn collect_host_resources() -> HostResources {
    static NODE_COLLECTOR: std::sync::OnceLock<std::sync::Mutex<NodeOsCollector>> =
        std::sync::OnceLock::new();
    let collector = NODE_COLLECTOR.get_or_init(|| std::sync::Mutex::new(NodeOsCollector::new()));

    // The lock is held only for the refresh (milliseconds, the same
    // work the previous per-scrape collector did); concurrent scrapes
    // serialize on it, which is the documented per-scrape precedent.
    let snapshot = match collector.lock() {
        Ok(mut c) => c.snapshot(),
        Err(poisoned) => poisoned.into_inner().snapshot(),
    };

    HostResources {
        // The Prometheus surface has no quality markers: no valid
        // interval yet flattens to 0 here (the v1 sample path carries
        // the quality instead).
        cpu_usage_percent: snapshot.cpu_capacity_ratio.unwrap_or(0.0) as f32 * 100.0,
        memory_total_bytes: snapshot.memory_total_bytes,
        // On Linux, sysinfo's used_memory() is exactly
        // total − available (the kernel's MemAvailable already nets
        // out reclaimable caches), so total − available here matches
        // what sysinfo itself would report — we just compute it from
        // the snapshot instead of a second, less-documented field.
        memory_used_bytes: snapshot
            .memory_total_bytes
            .saturating_sub(snapshot.memory_available_bytes),
        disk_total_bytes: snapshot.root_fs_total_bytes.unwrap_or(0),
        disk_available_bytes: snapshot.root_fs_available_bytes.unwrap_or(0),
    }
}

/// Append the process-global Prometheus registry to the hand-built agent
/// exposition.
///
/// `VmOpGuard` records `chv_vm_ops_total` / `chv_vm_op_duration_seconds`
/// through the `metrics` facade (issue #702), so their samples live in the
/// registry installed by `chv_observability::init_logger`, not in the
/// `chv_agent_*` lines assembled below. Rendering the registry here makes
/// those series scrapeable while leaving the hand-built families untouched.
///
/// The hand-built families are not recorded through the facade, so the two
/// halves have disjoint metric names and appending cannot duplicate a family.
fn append_registry_metrics(mut body: String) -> String {
    let Some(handle) = chv_observability::prometheus_handle() else {
        return body;
    };
    let registry = handle.render();
    if registry.is_empty() {
        return body;
    }
    if !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(&registry);
    if !registry.ends_with('\n') {
        body.push('\n');
    }
    body
}

/// Handler that renders metrics in Prometheus text exposition format.
async fn metrics_handler(State(state): State<Arc<Mutex<MetricsState>>>) -> impl IntoResponse {
    let host = collect_host_resources();
    let s = state.lock().await;
    let uptime = s.start_time.elapsed().as_secs();
    let reconcile_duration_secs = s.last_reconcile_duration_ms as f64 / 1000.0;
    // Core journal metrics exist only in core-managed mode: absent in
    // legacy mode rather than zeroed (a zero gauge would read as
    // "journal healthy" where no journal poller runs).
    let core_journal_segment = match s.core_journal {
        Some(core) => format!(
            "# HELP chv_agent_journal_scan_failures_total Total failed core journal scans\n\
             # TYPE chv_agent_journal_scan_failures_total counter\n\
             chv_agent_journal_scan_failures_total{{node_id=\"{node_id}\"}} {scan_failures}\n\
             # HELP chv_agent_journal_healthy Whether the core journal scanner is healthy (1=healthy)\n\
             # TYPE chv_agent_journal_healthy gauge\n\
             chv_agent_journal_healthy{{node_id=\"{node_id}\"}} {healthy}\n\
             # HELP chv_agent_journal_inspect_required Operations stuck InspectRequired awaiting operator resolution\n\
             # TYPE chv_agent_journal_inspect_required gauge\n\
             chv_agent_journal_inspect_required{{node_id=\"{node_id}\"}} {inspect_required}\n",
            node_id = s.node_id,
            scan_failures = core.scan_failures_total,
            healthy = if core.healthy { 1 } else { 0 },
            inspect_required = core.inspect_required,
        ),
        None => String::new(),
    };
    // Native monitoring sampler health (G1): global counters/gauges
    // only — no VM identifiers on Prometheus labels. Absent before the
    // sampler is spawned rather than zeroed.
    let sampler_segment = match &s.sampler_health {
        Some(health) => {
            let h = health.snapshot();
            format!(
                "# HELP chv_agent_monitoring_sampling_cycles_total Completed monitoring sampling cycles\n\
                 # TYPE chv_agent_monitoring_sampling_cycles_total counter\n\
                 chv_agent_monitoring_sampling_cycles_total{{node_id=\"{node_id}\"}} {cycles}\n\
                 # HELP chv_agent_monitoring_sampling_cycle_failures_total Monitoring sampling cycles with any failure\n\
                 # TYPE chv_agent_monitoring_sampling_cycle_failures_total counter\n\
                 chv_agent_monitoring_sampling_cycle_failures_total{{node_id=\"{node_id}\"}} {failures}\n\
                 # HELP chv_agent_monitoring_sampling_source_timeouts_total Monitoring source calls that timed out\n\
                 # TYPE chv_agent_monitoring_sampling_source_timeouts_total counter\n\
                 chv_agent_monitoring_sampling_source_timeouts_total{{node_id=\"{node_id}\"}} {timeouts}\n\
                 # HELP chv_agent_monitoring_sampling_vm_collection_failures_total Failed VM sample collections\n\
                 # TYPE chv_agent_monitoring_sampling_vm_collection_failures_total counter\n\
                 chv_agent_monitoring_sampling_vm_collection_failures_total{{node_id=\"{node_id}\"}} {vm_failures}\n\
                 # HELP chv_agent_monitoring_sampling_dropped_samples_total Samples dropped because the bounded sink was full\n\
                 # TYPE chv_agent_monitoring_sampling_dropped_samples_total counter\n\
                 chv_agent_monitoring_sampling_dropped_samples_total{{node_id=\"{node_id}\"}} {dropped}\n\
                 # HELP chv_agent_monitoring_sampling_last_cycle_duration_ms Duration of the last monitoring sampling cycle\n\
                 # TYPE chv_agent_monitoring_sampling_last_cycle_duration_ms gauge\n\
                 chv_agent_monitoring_sampling_last_cycle_duration_ms{{node_id=\"{node_id}\"}} {duration_ms}\n\
                 # HELP chv_agent_monitoring_sampling_last_success_unix_ms Unix ms of the last fully successful monitoring sampling cycle\n\
                 # TYPE chv_agent_monitoring_sampling_last_success_unix_ms gauge\n\
                 chv_agent_monitoring_sampling_last_success_unix_ms{{node_id=\"{node_id}\"}} {last_success}\n",
                node_id = s.node_id,
                cycles = h.cycles_completed,
                failures = h.cycle_failures,
                timeouts = h.source_timeouts,
                vm_failures = h.vm_collection_failures,
                dropped = h.dropped_samples,
                duration_ms = h.last_cycle_duration_ms,
                last_success = h.last_success_unix_ms,
            )
        }
        None => String::new(),
    };
    let body = format!(
        "# HELP chv_agent_node_state Current node state (1=active)\n\
         # TYPE chv_agent_node_state gauge\n\
         chv_agent_node_state{{node_id=\"{node_id}\",state=\"{state}\"}} 1\n\
         # HELP chv_agent_vms_total Number of VMs managed\n\
         # TYPE chv_agent_vms_total gauge\n\
         chv_agent_vms_total{{node_id=\"{node_id}\"}} {vms}\n\
         # HELP chv_agent_reconcile_ticks_total Total reconciliation ticks\n\
         # TYPE chv_agent_reconcile_ticks_total counter\n\
         chv_agent_reconcile_ticks_total{{node_id=\"{node_id}\"}} {ticks}\n\
         # HELP chv_agent_reconcile_failures_total Total reconciliation failures\n\
         # TYPE chv_agent_reconcile_failures_total counter\n\
         chv_agent_reconcile_failures_total{{node_id=\"{node_id}\"}} {rfail}\n\
         # HELP chv_agent_health_failures_total Total health check failures\n\
         # TYPE chv_agent_health_failures_total counter\n\
         chv_agent_health_failures_total{{node_id=\"{node_id}\"}} {hfail}\n\
         # HELP chv_agent_uptime_seconds Agent uptime in seconds\n\
         # TYPE chv_agent_uptime_seconds gauge\n\
         chv_agent_uptime_seconds{{node_id=\"{node_id}\"}} {uptime}\n\
         # HELP chv_agent_cp_connected Control plane connectivity (1=connected, 0=disconnected)\n\
         # TYPE chv_agent_cp_connected gauge\n\
         chv_agent_cp_connected{{node_id=\"{node_id}\",state=\"{cp_state}\"}} {cp_connected}\n\
         # HELP chv_agent_cp_disconnected_duration_ms Duration of current control plane disconnect in milliseconds\n\
         # TYPE chv_agent_cp_disconnected_duration_ms gauge\n\
         chv_agent_cp_disconnected_duration_ms{{node_id=\"{node_id}\"}} {cp_disconnect_ms}\n\
         # HELP chv_agent_cp_consecutive_failures Consecutive control plane contact failures\n\
         # TYPE chv_agent_cp_consecutive_failures gauge\n\
         chv_agent_cp_consecutive_failures{{node_id=\"{node_id}\"}} {cp_failures}\n\
         # HELP chv_agent_cp_deferred_messages_total Total messages deferred due to control plane unavailability\n\
         # TYPE chv_agent_cp_deferred_messages_total counter\n\
         chv_agent_cp_deferred_messages_total{{node_id=\"{node_id}\"}} {cp_deferred}\n\
         # HELP chv_agent_cpu_usage_percent Overall CPU utilization percentage\n\
         # TYPE chv_agent_cpu_usage_percent gauge\n\
         chv_agent_cpu_usage_percent{{node_id=\"{node_id}\"}} {cpu_usage}\n\
         # HELP chv_agent_memory_total_bytes Total system RAM in bytes\n\
         # TYPE chv_agent_memory_total_bytes gauge\n\
         chv_agent_memory_total_bytes{{node_id=\"{node_id}\"}} {mem_total}\n\
         # HELP chv_agent_memory_used_bytes Used system RAM in bytes\n\
         # TYPE chv_agent_memory_used_bytes gauge\n\
         chv_agent_memory_used_bytes{{node_id=\"{node_id}\"}} {mem_used}\n\
         # HELP chv_agent_disk_total_bytes Total disk space on root mount in bytes\n\
         # TYPE chv_agent_disk_total_bytes gauge\n\
         chv_agent_disk_total_bytes{{node_id=\"{node_id}\"}} {disk_total}\n\
         # HELP chv_agent_disk_available_bytes Available disk space on root mount in bytes\n\
         # TYPE chv_agent_disk_available_bytes gauge\n\
         chv_agent_disk_available_bytes{{node_id=\"{node_id}\"}} {disk_avail}\n\
         # HELP chv_agent_reconcile_drift_ema Exponential moving average of reconcile drift\n\
         # TYPE chv_agent_reconcile_drift_ema gauge\n\
         chv_agent_reconcile_drift_ema{{node_id=\"{node_id}\"}} {drift_ema}\n\
         # HELP chv_agent_reconcile_duration_seconds Last reconcile tick duration\n\
         # TYPE chv_agent_reconcile_duration_seconds gauge\n\
         chv_agent_reconcile_duration_seconds{{node_id=\"{node_id}\"}} {reconcile_dur}\n\
         # HELP chv_agent_disk_pressure Whether disk is under pressure (1=yes, 0=no)\n\
         # TYPE chv_agent_disk_pressure gauge\n\
         chv_agent_disk_pressure{{node_id=\"{node_id}\"}} {disk_pressure}\n\
         # HELP chv_agent_memory_pressure Whether memory is under pressure (1=yes, 0=no)\n\
         # TYPE chv_agent_memory_pressure gauge\n\
         chv_agent_memory_pressure{{node_id=\"{node_id}\"}} {mem_pressure}\n{core_journal_segment}{sampler_segment}",
        node_id = s.node_id,
        state = s.node_state,
        vms = s.vm_count,
        ticks = s.tick_count,
        rfail = s.reconcile_failures,
        hfail = s.health_failures,
        uptime = uptime,
        cp_state = s.cp_connectivity_state.as_str(),
        cp_connected = if s.cp_connectivity_state == ConnectivityState::Connected { 1 } else { 0 },
        cp_disconnect_ms = s.cp_disconnected_duration_ms,
        cp_failures = s.cp_consecutive_failures,
        cp_deferred = s.cp_total_deferred_messages,
        cpu_usage = host.cpu_usage_percent,
        mem_total = host.memory_total_bytes,
        mem_used = host.memory_used_bytes,
        disk_total = host.disk_total_bytes,
        disk_avail = host.disk_available_bytes,
        drift_ema = s.reconcile_drift_ema,
        reconcile_dur = reconcile_duration_secs,
        disk_pressure = if s.disk_pressure { 1 } else { 0 },
        mem_pressure = if s.memory_pressure { 1 } else { 0 },
    );
    let body = append_registry_metrics(body);
    (
        axum::http::StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
}

/// Build an axum Router for the `/metrics` endpoint.
pub fn metrics_router(state: Arc<Mutex<MetricsState>>) -> Router {
    Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use std::collections::HashMap;
    use tower::ServiceExt;

    fn test_state() -> Arc<Mutex<MetricsState>> {
        let mut state = MetricsState::new("node-test".to_string());
        // Exercise the journal segment too: appending the registry must not
        // disturb the hand-built families.
        state.core_journal = Some(CoreJournalMetrics {
            scan_failures_total: 2,
            healthy: true,
            inspect_required: 1,
        });
        Arc::new(Mutex::new(state))
    }

    async fn scrape() -> String {
        let response = metrics_router(test_state())
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("router response");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("collect body")
            .to_bytes();
        String::from_utf8(body.to_vec()).expect("utf-8 exposition")
    }

    /// The agent's `/metrics` must expose series that `VmOpGuard` records
    /// through the `metrics` facade, not just the hand-built `chv_agent_*`
    /// families (issue #702).
    #[tokio::test]
    async fn metrics_endpoint_includes_registry_recorded_vm_op_series() {
        chv_observability::install_prometheus_recorder().expect("install prometheus recorder");
        // Same facade path VmOpGuard uses on drop.
        metrics::counter!(
            chv_observability::CHV_VM_OPS_TOTAL,
            "op" => "start",
            "result" => "ok"
        )
        .increment(1);
        metrics::histogram!(
            chv_observability::CHV_VM_OP_DURATION_SECONDS,
            "op" => "start"
        )
        .record(0.25);

        let text = scrape().await;

        assert!(
            text.lines().any(|line| {
                line.starts_with("chv_vm_ops_total{")
                    && line.contains("op=\"start\"")
                    && line.contains("result=\"ok\"")
            }),
            "registry-recorded VM op counter missing from /metrics, got:\n{text}"
        );
        assert!(
            text.contains("# TYPE chv_vm_op_duration_seconds histogram"),
            "registry-recorded VM op histogram missing from /metrics, got:\n{text}"
        );
        assert!(
            text.lines().any(|line| {
                line.starts_with("chv_vm_op_duration_seconds_bucket{")
                    && line.contains("op=\"start\"")
            }),
            "VM op duration buckets missing from /metrics, got:\n{text}"
        );

        // The hand-built segments keep rendering unchanged.
        for family in [
            "chv_agent_node_state",
            "chv_agent_vms_total",
            "chv_agent_journal_scan_failures_total",
            "chv_agent_journal_healthy",
            "chv_agent_journal_inspect_required",
        ] {
            assert!(
                text.contains(&format!("# TYPE {family} ")),
                "hand-built family {family} missing from /metrics, got:\n{text}"
            );
        }
    }

    /// The hand-built and registry halves must have disjoint metric names;
    /// a duplicate family would make the scrape invalid.
    #[tokio::test]
    async fn metrics_endpoint_has_exactly_one_type_per_family() {
        chv_observability::install_prometheus_recorder().expect("install prometheus recorder");
        let text = scrape().await;

        let mut type_counts: HashMap<&str, usize> = HashMap::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("# TYPE ") {
                let family = rest
                    .split_whitespace()
                    .next()
                    .expect("a TYPE line names its family");
                *type_counts.entry(family).or_insert(0) += 1;
            }
        }

        assert!(
            !type_counts.is_empty(),
            "# TYPE lines missing from /metrics, got:\n{text}"
        );
        let duplicates: Vec<_> = type_counts
            .iter()
            .filter(|(_, count)| **count > 1)
            .map(|(family, count)| (*family, *count))
            .collect();
        assert!(
            duplicates.is_empty(),
            "duplicate metric families in /metrics: {duplicates:?}\n{text}"
        );
    }
}
