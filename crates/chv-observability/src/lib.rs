use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use std::sync::OnceLock;
use tracing::Span;

pub mod grpc_metrics;
pub use grpc_metrics::{GrpcMetricsLayer, GrpcMetricsService};

#[cfg(feature = "http-metrics")]
pub mod metrics_server;
#[cfg(feature = "http-metrics")]
pub use metrics_server::{metrics_router, DaemonMetricsInfo};

/// ADR-009 mandated metric names.
pub const CHV_VMS_TOTAL: &str = "chv_vms_total";
pub const CHV_NODES_READY: &str = "chv_nodes_ready";
pub const CHV_OPERATION_DURATION_SECONDS: &str = "chv_operation_duration_seconds";

// ---------------------------------------------------------------------------
// gRPC server-side RED metric names (S3-2 / C-13)
// ---------------------------------------------------------------------------
pub const CHV_GRPC_SERVER_REQUESTS_TOTAL: &str = "chv_grpc_server_requests_total";
pub const CHV_GRPC_SERVER_DURATION_SECONDS: &str = "chv_grpc_server_duration_seconds";

// ---------------------------------------------------------------------------
// VM lifecycle RED metric names (S3-1 / C-11)
//
// Emitted by the agent's Cloud Hypervisor adapter for every VM lifecycle
// operation (create, start, stop, delete, pause, resume).
//
// Labels:
//   * `op`     — one of: `create`, `start`, `stop`, `delete`, `pause`, `resume`
//   * `result` — one of: `ok`, `err` (on the counter only; the histogram is
//                always recorded so dashboards can show error latency too)
// ---------------------------------------------------------------------------
pub const CHV_VM_OPS_TOTAL: &str = "chv_vm_ops_total";
pub const CHV_VM_OP_DURATION_SECONDS: &str = "chv_vm_op_duration_seconds";

/// Buckets for the VM lifecycle duration histogram.
///
/// The Prometheus alert and recording rules read
/// `chv_vm_op_duration_seconds_bucket`; without explicit buckets the exporter
/// renders histograms as summaries and those rules stay empty.
const VM_OP_DURATION_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 30.0, 45.0, 60.0, 120.0,
    300.0,
];

/// Buckets for the server-side gRPC duration histogram.
///
/// `monitoring/rules/recording.yml` reads
/// `chv_grpc_server_duration_seconds_bucket`; like the VM-op histogram, this
/// must be exported as a real histogram, not a summary.
const GRPC_SERVER_DURATION_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];

/// The BFF's HTTP request-duration histogram, registered by
/// `chv-webui-bff::metrics_middleware` in the control-plane process.
/// `monitoring/rules/recording.yml` reads its `_bucket` series.
const HTTP_REQUEST_DURATION_SECONDS: &str = "http_request_duration_seconds";
const HTTP_REQUEST_DURATION_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];

/// Configure one histogram's buckets without giving up the builder on the
/// (impossible, because every call passes a non-empty constant) error path.
fn with_buckets(builder: PrometheusBuilder, metric: &str, buckets: &[f64]) -> PrometheusBuilder {
    match builder.set_buckets_for_metric(Matcher::Full(metric.to_string()), buckets) {
        Ok(builder) => builder,
        Err(error) => {
            tracing::error!(
                metric,
                %error,
                "invalid duration buckets; exporting the histogram as a summary"
            );
            PrometheusBuilder::new()
        }
    }
}

// ---------------------------------------------------------------------------
// Multi-node metric names (Phase 10)
// ---------------------------------------------------------------------------

// Migration metrics
pub const CHV_MIGRATION_PHASE: &str = "chv_migration_phase";
pub const CHV_MIGRATION_BYTES_TRANSFERRED: &str = "chv_migration_bytes_transferred";
pub const CHV_MIGRATION_DURATION_SECONDS: &str = "chv_migration_duration_seconds";
pub const CHV_MIGRATION_DIRTY_BLOCKS: &str = "chv_migration_dirty_blocks";
/// Counter of non-terminal rows reconciled by restart recovery.
/// Label: `recovered_phase` (`RolledBack` or `Failed`).
pub const CHV_MIGRATION_RESTART_RECONCILED_TOTAL: &str = "chv_migration_restart_reconciled_total";
/// Counter of dropped progress reports for already-terminal migrations.
/// Labels: `stored_phase`, `reported_phase`.
pub const CHV_MIGRATION_PROGRESS_DROPPED_TOTAL: &str = "chv_migration_progress_dropped_total";

// Overlay metrics
pub const CHV_VXLAN_FDB_ENTRIES: &str = "chv_vxlan_fdb_entries";

// ---------------------------------------------------------------------------
// nwd fabric datapath observability (ADR-021)
// ---------------------------------------------------------------------------
// Counters incremented by the chv-nwd handlers wherever a fabric overlay is
// applied or removed (ensure path, UpdateOverlay path, teardown paths).
// Labels:
//   * `result` — one of: `success`, `failure`
// ---------------------------------------------------------------------------
pub const NWD_FABRIC_APPLY_TOTAL: &str = "nwd_fabric_apply_total";
pub const NWD_FABRIC_REMOVE_TOTAL: &str = "nwd_fabric_remove_total";

// eBPF metrics
pub const CHV_EBPF_PACKETS_TOTAL: &str = "chv_ebpf_packets_total";
pub const CHV_EBPF_BYTES_TOTAL: &str = "chv_ebpf_bytes_total";

// ---------------------------------------------------------------------------
// Control plane convergence metrics (M2 gap analysis)
// ---------------------------------------------------------------------------
pub const CHV_CP_DRIFT_COUNT: &str = "chv_cp_drift_count";
pub const CHV_CP_PENDING_OPERATIONS: &str = "chv_cp_pending_operations";
pub const CHV_CP_CONVERGENCE_AVG_MS: &str = "chv_cp_convergence_avg_ms";
pub const CHV_CP_CONSECUTIVE_DRIFT_TICKS: &str = "chv_cp_consecutive_drift_ticks";
pub const CHV_CP_RECONCILE_TICKS_TOTAL: &str = "chv_cp_reconcile_ticks_total";
pub const CHV_CP_OPERATIONS_DISPATCHED_TOTAL: &str = "chv_cp_operations_dispatched_total";

static PROMETHEUS_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Returns the globally installed Prometheus metrics handle, if one exists.
pub fn prometheus_handle() -> Option<&'static PrometheusHandle> {
    PROMETHEUS_HANDLE.get()
}

/// Install only the Prometheus metrics recorder — no tracing
/// subscriber, no log-format env. Idempotent: succeeds immediately if
/// this process already installed the recorder through this function
/// or [`init_logger`]. Tests use this to render the exact exposition
/// the admin `/metrics` route serves without depending on tracing's
/// own once-per-process global.
pub fn install_prometheus_recorder() -> Result<(), Box<dyn std::error::Error>> {
    // Serialize the check-and-install: two callers racing here would both see
    // an unset handle, and the loser's `set_global_recorder` would fail after
    // the winner installed. The mutex makes the idempotence real for parallel
    // tests and for any daemon that initializes logging from multiple tasks.
    static INSTALL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = INSTALL_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if PROMETHEUS_HANDLE.get().is_some() {
        return Ok(());
    }
    // Every rule-backed duration histogram must be a real histogram, not a
    // summary: the recording rules read `_bucket` series. The bucket lists
    // are non-empty constants, so the error path is defensive only and never
    // fails recorder installation.
    let builder = with_buckets(
        with_buckets(
            with_buckets(
                PrometheusBuilder::new(),
                CHV_VM_OP_DURATION_SECONDS,
                VM_OP_DURATION_BUCKETS,
            ),
            CHV_GRPC_SERVER_DURATION_SECONDS,
            GRPC_SERVER_DURATION_BUCKETS,
        ),
        HTTP_REQUEST_DURATION_SECONDS,
        HTTP_REQUEST_DURATION_BUCKETS,
    );
    let recorder = builder.build_recorder();
    let handle = recorder.handle();
    metrics::set_global_recorder(recorder)?;
    let _ = PROMETHEUS_HANDLE.set(handle);
    Ok(())
}

pub fn init_logger(filter: &str) -> Result<(), Box<dyn std::error::Error>> {
    let use_json = std::env::var("CHV_LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);

    if use_json {
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .with_target(true)
            .finish();
        tracing::subscriber::set_global_default(subscriber)?;
    } else {
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(true)
            .finish();
        tracing::subscriber::set_global_default(subscriber)?;
    }

    // Order-independent with `install_prometheus_recorder`: if that
    // already installed the recorder (tests), this is a no-op instead
    // of an error; a recorder set by anyone else still fails loudly.
    install_prometheus_recorder()?;

    Ok(())
}
pub fn operation_span(op_id: &str) -> Span {
    tracing::info_span!("operation", operation_id = op_id)
}

#[derive(Debug, Clone, Default)]
pub struct Metrics;

impl Metrics {
    pub fn new() -> Self {
        Self
    }

    pub fn increment_counter(&self, name: &'static str) {
        metrics::counter!(name).increment(1);
    }

    pub fn gauge(&self, name: &'static str, value: f64) {
        metrics::gauge!(name).set(value);
    }

    pub fn record_histogram(&self, name: &'static str, value: f64) {
        metrics::histogram!(name).record(value);
    }

    pub fn record_operation_duration(&self, operation: &'static str, duration_secs: f64) {
        metrics::histogram!(CHV_OPERATION_DURATION_SECONDS, "operation" => operation)
            .record(duration_secs);
    }

    // -----------------------------------------------------------------------
    // Multi-node metric helpers (Phase 10)
    // -----------------------------------------------------------------------

    /// Record the current migration phase as a gauge (0=Pending .. 7=RolledBack).
    ///
    /// NOTE: No call sites exist yet — wiring is deferred until dirty-sync rounds
    /// are implemented in chv-stord (see docs/production-readiness-report.md P0 #7).
    pub fn set_migration_phase(&self, vm_id: &str, phase: f64) {
        metrics::gauge!(
            CHV_MIGRATION_PHASE,
            "vm_id" => vm_id.to_string()
        )
        .set(phase);
    }

    /// Increment migration bytes transferred counter.
    ///
    /// NOTE: No call sites exist yet — wiring is deferred until dirty-sync rounds
    /// are implemented in chv-stord (see docs/production-readiness-report.md P0 #7).
    pub fn add_migration_bytes(&self, vm_id: &str, bytes: u64) {
        metrics::counter!(
            CHV_MIGRATION_BYTES_TRANSFERRED,
            "vm_id" => vm_id.to_string()
        )
        .increment(bytes);
    }

    /// Record total migration duration.
    pub fn record_migration_duration(&self, vm_id: &str, outcome: &str, duration_secs: f64) {
        metrics::histogram!(
            CHV_MIGRATION_DURATION_SECONDS,
            "vm_id" => vm_id.to_string(),
            "outcome" => outcome.to_string()
        )
        .record(duration_secs);
    }

    /// Set current dirty block count during migration convergence.
    ///
    /// NOTE: No call sites exist yet — wiring is deferred until dirty-sync rounds
    /// are implemented in chv-stord (see docs/production-readiness-report.md P0 #7).
    pub fn set_migration_dirty_blocks(&self, blocks: f64) {
        metrics::gauge!(CHV_MIGRATION_DIRTY_BLOCKS).set(blocks);
    }

    /// Increment the fabric overlay apply counter (`nwd_fabric_apply_total`).
    /// `result` must be `"success"` or `"failure"`.
    pub fn increment_nwd_fabric_apply(&self, result: &'static str) {
        metrics::counter!(NWD_FABRIC_APPLY_TOTAL, "result" => result).increment(1);
    }

    /// Increment the fabric overlay remove counter (`nwd_fabric_remove_total`).
    /// `result` must be `"success"` or `"failure"`.
    pub fn increment_nwd_fabric_remove(&self, result: &'static str) {
        metrics::counter!(NWD_FABRIC_REMOVE_TOTAL, "result" => result).increment(1);
    }

    /// Set current FDB entry count for a VXLAN network.
    pub fn set_vxlan_fdb_entries(&self, network_id: &str, node_id: &str, count: f64) {
        metrics::gauge!(
            CHV_VXLAN_FDB_ENTRIES,
            "network_id" => network_id.to_string(),
            "node_id" => node_id.to_string()
        )
        .set(count);
    }

    /// Increment eBPF packet counter.
    pub fn add_ebpf_packets(&self, vm_id: &str, direction: &str, action: &str, count: u64) {
        metrics::counter!(
            CHV_EBPF_PACKETS_TOTAL,
            "vm_id" => vm_id.to_string(),
            "direction" => direction.to_string(),
            "action" => action.to_string()
        )
        .increment(count);
    }

    /// Increment eBPF byte counter.
    pub fn add_ebpf_bytes(&self, vm_id: &str, direction: &str, action: &str, bytes: u64) {
        metrics::counter!(
            CHV_EBPF_BYTES_TOTAL,
            "vm_id" => vm_id.to_string(),
            "direction" => direction.to_string(),
            "action" => action.to_string()
        )
        .increment(bytes);
    }
}
