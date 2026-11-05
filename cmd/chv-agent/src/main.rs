use chv_agent_core::{
    agent_server::{AgentAuth, AgentServer},
    cache::{NodeCache, PendingControlPlaneMessage},
    config::{load_agent_config, AgentAuthorityMode, AgentConfig},
    connectivity::{ConnectivityState, ConnectivityTracker},
    console_server::ConsoleServer,
    control_plane::ControlPlaneClient,
    daemon_clients::{NwdClient, StordClient},
    enrollment::{BootstrapResponse, EnrollmentClient},
    health::{check_host_resources, HealthAggregator},
    inventory::InventoryReporter,
    metrics_server::{metrics_router, MetricsState},
    projection::ProjectingCoreRuntime,
    reconcile::{apply_core_create_states, Reconciler},
    state_machine::NodeState,
    stord_backend::StordBackendInfo,
    supervisor::DaemonSupervisor,
    telemetry::TelemetryReporter,
    vm_runtime::VmRuntime,
};
use chv_agent_runtime_ch::process::ProcessCloudHypervisorAdapter;
use chv_errors::ChvError;
use chv_observability::init_logger;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{info, warn};

const FAILED_THRESHOLD: u32 = 6; // 6 ticks * 5s = 30s
/// How often the composition-internal journal poller drives `scan_ready`.
const CORE_SCAN_INTERVAL: Duration = Duration::from_millis(250);
/// Upper bound on a single `scan_ready`. A wedged authority/store can hang the
/// scan RPC; the timeout turns a hang into a counted scan failure (health goes
/// unhealthy, backoff engages) instead of wedging the poller and shutdown.
const CORE_SCAN_TIMEOUT: Duration = Duration::from_secs(5);
/// Upper bound on the graceful journal drain at shutdown. The CH adapter bounds
/// individual ops at ~10s internally, so 60s covers a stalled batch; beyond
/// that the executor is explicitly cancelled (fail-closed -> InspectRequired).
const CORE_EXECUTOR_DRAIN_BUDGET: Duration = Duration::from_secs(60);
const CERT_ROTATION_INTERVAL_SECS: i64 = 12 * 60 * 60;
/// Default `/metrics` listener address when `metrics_bind` is unset. Loopback,
/// not `0.0.0.0`: an omitted key must never expose the Prometheus endpoint on
/// every interface. Operators wanting another bind set `metrics_bind`
/// explicitly (honored verbatim).
const DEFAULT_METRICS_BIND: &str = "127.0.0.1:9100";

/// Write `contents` to `path` with mode 0600, normalizing the permissions of
/// an existing file as well. Used for TLS private key material.
/// Durable, atomic file publish for node-local security material: write to
/// a private temp sibling, fsync, rename into place, fsync the parent
/// directory. A crash or power loss mid-write never leaves a truncated
/// key/cert at the target path (the previous version, if any, stays intact
/// until the rename commits).
async fn write_file_durable(path: &Path, contents: &[u8], mode: u32) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path has no parent directory",
        )
    })?;
    let temp = std::path::PathBuf::from(format!("{}.tmp-{}", path.display(), std::process::id()));
    let write = async {
        // `.write(true)` is required: OpenOptions defaults to read-only
        // access, and create/truncate without write access fails at the
        // library level with InvalidInput before any file is created (a
        // defect the R1 review caught in the first version of this
        // function — every enrollment/rotation write silently failed).
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .mode(mode)
            .create(true)
            .truncate(true)
            .open(&temp)
            .await?;
        tokio::io::AsyncWriteExt::write_all(&mut file, contents).await?;
        file.sync_all().await?;
        Ok::<(), std::io::Error>(())
    }
    .await;
    match write {
        Ok(()) => {}
        Err(error) => {
            let _ = tokio::fs::remove_file(&temp).await;
            return Err(error);
        }
    }
    if let Err(error) = tokio::fs::rename(&temp, path).await {
        // The publish did not happen: clean up the temp sibling so a
        // transient failure does not leak *.tmp-{pid} files.
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(error);
    }
    // Persist the rename itself: fsync the parent directory (O_RDONLY is
    // sufficient for a directory fsync on Linux). Best-effort by
    // necessity — the rename has already committed, so the target IS
    // updated; returning an error here would make callers report the
    // material as unwritten while it exists on disk (enrollment would
    // refuse to mark itself complete). A failed dir-fsync weakens the
    // crash-durability guarantee to that of a plain rename; log loudly.
    let dir_fsync = async {
        let directory = tokio::fs::File::open(parent).await?;
        directory.sync_all().await?;
        Ok::<(), std::io::Error>(())
    }
    .await;
    if let Err(error) = dir_fsync {
        tracing::warn!(
            path = %path.display(),
            error = %error,
            "published file but failed to fsync parent directory (crash durability weakened)"
        );
    }
    Ok(())
}

/// Private (0600) variant of [`write_file_durable`] for key material.
async fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    write_file_durable(path, contents, 0o600).await
}

/// Wire the guest-liveness (boot) watchdog when the agent config opts
/// in (`[watchdog] enabled = true`). Drives the adapter's watchdog tick
/// on a 5 s interval for the agent's lifetime. Disabled by default: the
/// detection is marker-based and cannot distinguish a frozen boot from
/// a legitimately quiet marker-less guest, so operators opt in per
/// node (see `BootWatchdogAgentConfig`).
/// G5 (prompt-06 task 3): the node-side vsock monitoring relay manager,
/// wired from the node's control-plane address — the per-VM relays pipe
/// guest TLS bytes to the control plane's existing mTLS listener.
/// Fail-soft by design: an unparseable address disables the relay
/// (vsock VMs still run; guest monitoring over vsock is unavailable and
/// the gap is logged loudly) — monitoring plumbing must never gate VM
/// lifecycle.
fn maybe_vsock_relay(
    config: &AgentConfig,
) -> Option<Arc<chv_agent_runtime_ch::vsock_relay::VsockRelayManager>> {
    match chv_agent_runtime_ch::vsock_relay::VsockRelayManager::new(&config.control_plane_addr) {
        Ok(manager) => Some(Arc::new(manager)),
        Err(e) => {
            warn!(
                error = %e,
                control_plane_addr = %config.control_plane_addr,
                "vsock monitoring relay disabled for this agent process"
            );
            None
        }
    }
}

fn spawn_boot_watchdog(
    process_adapter: &Arc<ProcessCloudHypervisorAdapter>,
    config: &chv_config::BootWatchdogAgentConfig,
) {
    if !config.enabled {
        return;
    }
    process_adapter.configure_boot_watchdog(chv_agent_runtime_ch::process::BootWatchdogConfig {
        boot_marker: config.boot_marker.clone(),
        stall_secs: config.stall_secs,
        max_reboots: config.max_reboots,
        healthy_reset_secs: config.healthy_reset_secs,
    });
    let adapter = Arc::clone(process_adapter);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            adapter.boot_watchdog_tick().await;
        }
    });
    info!(
        stall_secs = config.stall_secs,
        marker = %config.boot_marker,
        max_reboots = config.max_reboots,
        "guest-liveness (boot) watchdog enabled"
    );
}

/// Build the durable network-usage lookup for last-detach teardown
/// (#356 N5): a READ-ONLY handle onto the Core store the runtime owner is
/// about to activate. The store file exists by the time this runs (both
/// start paths activate it first). If it cannot be opened read-only, fail
/// closed — `AlwaysInUse` — and say so loudly: no network teardown will
/// fire on this node until the store is readable, leaving residue instead
/// of risking an outage.
fn network_usage_lookup(config: &AgentConfig) -> Arc<dyn chv_hypervisor_api::NetworkUsageLookup> {
    match chv_agent_core::network_usage::CoreStoreNetworkUsage::open(&config.core_store_path) {
        Ok(lookup) => Arc::new(lookup),
        Err(error) => {
            warn!(
                path = %config.core_store_path.display(),
                %error,
                "cannot open the Core store read-only for the network-usage lookup; \
                 last-detach network teardown is disabled (fail closed) on this node"
            );
            Arc::new(chv_hypervisor_api::AlwaysInUse)
        }
    }
}

async fn start_core_managed(
    config: &AgentConfig,
    adapter: Arc<dyn chv_agent_runtime_ch::adapter::CloudHypervisorAdapter>,
    cache: &Arc<tokio::sync::Mutex<NodeCache>>,
    cache_path: PathBuf,
    stord_backend: &StordBackendInfo,
) -> Result<cellhv_core_runtime_owner::CoreRuntimeOwner, Box<dyn std::error::Error>> {
    let paths = cellhv_core_startup::StartupPaths {
        node_cache: config.cache_path.clone(),
        core_database: config.core_store_path.clone(),
        node_cache_archive: config.core_archive_path.clone(),
    };
    let configured_seed = match config.node_id.trim() {
        "" => None,
        node_id => Some(node_id.to_owned()),
    };
    let activated =
        cellhv_core_startup::StartupTransaction::begin(&paths)?.activate(configured_seed, None)?;
    // Refuse ineligible authorities BEFORE any pre-start work mutates
    // on-disk state: the startup rebuild below persists a rebuilt cache,
    // and it must not clobber the live cache of an authority the runtime
    // owner is about to refuse (foreign migration provenance).
    cellhv_core_runtime_owner::validate_activation(&activated)?;
    // M2.2b startup rebuild: seed NodeCache's VM axis from the Core store's
    // authoritative VM list BEFORE the executor poller starts, so a
    // crash-recovery operation can never race the rebuild (a projection that
    // landed after the snapshot would otherwise be wiped and never re-added
    // while the Reconciler is observe-only). On any open/list/rebuild/save
    // failure we warn and continue: the Reconciler and the legacy desired-state
    // RPCs have no mutation surface in core-managed mode (M2.3), so a stale
    // compatibility cache cannot silently launch a second authority.
    //
    // Divergence guard: when the live cache references VMs the authority
    // does not know (a stale projection or a restored legacy cache that
    // activation did not import — the Core database wins by design), the
    // rebuild would DESTROY the only surviving record of those VMs.
    // Refuse to overwrite it and leave operator inspection possible.
    // (The decision itself is `NodeCache::unadopted_cache_vm_ids`, unit
    // tested in chv-agent-core.)
    match activated.service().vms() {
        Ok(rebuild_vms) => {
            let mut cache = cache.lock().await;
            let unadopted = cache.unadopted_cache_vm_ids(&rebuild_vms);
            if !unadopted.is_empty() {
                warn!(
                    unadopted = ?unadopted,
                    "live NodeCache references VMs the Core authority does not know \
                     (stale projection or restored legacy cache was NOT imported); \
                     refusing to overwrite the on-disk cache — Core remains authoritative"
                );
            } else {
                cache.rebuild_from_core(&rebuild_vms);
                if let Err(e) = cache.save(&cache_path).await {
                    warn!(error = %e, "core startup rebuild: failed to persist rebuilt NodeCache");
                }
            }
        }
        Err(e) => {
            warn!(error = %e, "core startup rebuild: skipping NodeCache rebuild from Core");
        }
    }
    let resources = Arc::new(chv_agent_core::resources::AgentResourceController::new(
        config.stord_socket.clone(),
        config.nwd_socket.clone(),
    ));
    let runtime = Arc::new(
        chv_agent_runtime_ch::core_runtime::CloudHypervisorCoreRuntime::new(
            adapter,
            resources,
            config.runtime_dir.clone(),
        )
        .with_network_usage(network_usage_lookup(config))
        // #379 DP5: LVM-class opens locate against the operator's VG
        // (the same stord-config source as the reported backend class).
        .with_lvm_volume_group(stord_backend.volume_group())
        // #405: delete-time side-effect fallback drain. The in-memory
        // handle map dies with the process; without this, a delete of a
        // VM created before an agent restart leaks its host taps and
        // stord sessions behind an HTTP 200. The cache's VM axis is
        // re-seeded from the Core store by the startup rebuild above,
        // BEFORE the executor starts, so it is a durable observed
        // source for exactly those pre-restart VMs.
        .with_observed_attachments(Arc::new(
            chv_agent_core::resources::NodeCacheAttachmentSource::new(cache.clone()),
        )),
    );
    // M2.2b: wrap the single effector with the NodeCache compatibility
    // projection — Succeeded Core outcomes are projected into NodeCache and
    // persisted before the executor finishes the operation.
    let projecting = Arc::new(ProjectingCoreRuntime::new(
        runtime,
        cache.clone(),
        cache_path,
    ));
    Ok(cellhv_core_runtime_owner::CoreRuntimeOwner::start(
        projecting,
        activated,
        &config.core_api_socket_path,
        128,
        Duration::from_secs(2),
        cellhv_core_runtime_owner::JournalPollerConfig {
            scan_interval: CORE_SCAN_INTERVAL,
            scan_timeout: CORE_SCAN_TIMEOUT,
            drain_budget: CORE_EXECUTOR_DRAIN_BUDGET,
        },
    )
    .await?)
}

async fn start_core_native(
    config: &AgentConfig,
    stord_backend: &StordBackendInfo,
) -> Result<
    (
        cellhv_core_runtime_owner::CoreRuntimeOwner,
        Arc<ProcessCloudHypervisorAdapter>,
    ),
    Box<dyn std::error::Error>,
> {
    let paths = cellhv_core_startup::StartupPaths {
        node_cache: config.cache_path.clone(),
        core_database: config.core_store_path.clone(),
        node_cache_archive: config.core_archive_path.clone(),
    };
    let configured_seed = match config.node_id.trim() {
        "" => None,
        node_id => Some(node_id.to_owned()),
    };
    let activated = cellhv_core_startup::StartupTransaction::begin(&paths)?
        .activate_native_only(configured_seed)?;
    let process_adapter =
        chv_agent_runtime_ch::process::ProcessCloudHypervisorAdapter::new(&config.chv_binary_path)
            // G5: wire the vsock monitoring relay BEFORE adoption so live vsock
            // VMs get their endpoint back at startup.
            .with_vsock_relay(maybe_vsock_relay(config));
    if let Err(e) = process_adapter.adopt_running_vms(&config.runtime_dir).await {
        warn!(error = %e, "vm runtime state adoption failed; starting with an empty runtime map");
    }
    // Keep a concrete handle for the graceful-shutdown console drain;
    // the dyn Arc below is the one the runtime uses.
    let process_adapter = Arc::new(process_adapter);
    spawn_boot_watchdog(&process_adapter, &config.watchdog);
    let adapter: Arc<dyn chv_agent_runtime_ch::adapter::CloudHypervisorAdapter> =
        process_adapter.clone();
    let resources = Arc::new(chv_agent_core::resources::AgentResourceController::new(
        config.stord_socket.clone(),
        config.nwd_socket.clone(),
    ));
    let runtime = Arc::new(
        chv_agent_runtime_ch::core_runtime::CloudHypervisorCoreRuntime::new(
            adapter,
            resources,
            config.runtime_dir.clone(),
        )
        .with_network_usage(network_usage_lookup(config))
        // #379 DP5: LVM-class opens locate against the operator's VG
        // (the same stord-config source as the reported backend class).
        .with_lvm_volume_group(stord_backend.volume_group()),
    );
    Ok((
        cellhv_core_runtime_owner::CoreRuntimeOwner::start(
            runtime,
            activated,
            &config.core_api_socket_path,
            128,
            Duration::from_secs(2),
            cellhv_core_runtime_owner::JournalPollerConfig {
                scan_interval: CORE_SCAN_INTERVAL,
                scan_timeout: CORE_SCAN_TIMEOUT,
                drain_budget: CORE_EXECUTOR_DRAIN_BUDGET,
            },
        )
        .await?,
        process_adapter,
    ))
}

async fn run_core_native(
    config: &AgentConfig,
    stord_backend: &StordBackendInfo,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let (owner, process_adapter) = start_core_native(config, stord_backend).await?;
    info!(socket = %owner.socket_path().display(), "core-native authority ready");
    let mut fatality_check = tokio::time::interval(Duration::from_millis(500));
    fatality_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = sigterm.recv() => {
                info!("received SIGTERM, shutting down core-native authority");
                break;
            }
            _ = sigint.recv() => {
                info!("received SIGINT, shutting down core-native authority");
                break;
            }
            _ = fatality_check.tick() => {
                // A fatally terminated executor must fail the process: the
                // authority would keep acknowledging operations that are
                // never executed. Exit non-zero so the supervisor restarts
                // the agent.
                if owner.executor_fatal() {
                    tracing::error!(
                        "core journal executor terminated fatally — exiting for supervisor restart"
                    );
                    // Drain consoles even when the owner shutdown itself
                    // errors: an early `?` here would exit the process
                    // with live serial connections undrained — the exact
                    // freeze hazard the drain exists to prevent.
                    let shutdown_result = owner.shutdown().await;
                    // VMs outlive the agent: convert process exit into a
                    // clean close of every live serial connection so a
                    // running guest cannot be frozen by the exit itself.
                    process_adapter.drain_and_close_consoles().await;
                    shutdown_result?;
                    return Err("core journal executor terminated fatally".into());
                }
            }
        }
    }
    // Same discipline on the graceful exit: drain consoles even when the
    // owner shutdown errors, then propagate the shutdown failure.
    let shutdown_result = owner.shutdown().await;
    // VMs outlive the agent: convert process exit into a clean close of
    // every live serial connection so a running guest cannot be frozen
    // by the exit itself (cloud-hypervisor v43 serial-manager defect —
    // see ProcessCloudHypervisorAdapter::drain_and_close_consoles).
    // Runs AFTER the owner shutdown: the executor's bounded drain inside
    // it can keep lifecycle ops and console traffic alive until then.
    process_adapter.drain_and_close_consoles().await;
    shutdown_result?;
    Ok(())
}

fn initial_node_id(config: &AgentConfig) -> String {
    if config.node_id.is_empty() {
        "unknown".to_string()
    } else {
        config.node_id.clone()
    }
}

async fn load_or_initialize_cache(config: &AgentConfig) -> NodeCache {
    // Fail fast and loud when the cache parent is unusable: a missing
    // directory would otherwise make every later save fail as a per-tick
    // warning while the node re-enrolls on each boot (state silently
    // non-persistent). Creating the parent here is best-effort; a
    // permission problem still surfaces through the first save's warn.
    if let Some(parent) = config.cache_path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            warn!(parent = %parent.display(), error = %e, "cannot create cache parent directory");
        }
    }
    match NodeCache::load(&config.cache_path).await {
        Ok(cache) => {
            info!(
                node_id = %cache.node_id,
                node_state = %cache.node_state,
                "loaded cache"
            );
            cache
        }
        Err(chv_errors::ChvError::NotFound { .. }) => NodeCache::new(initial_node_id(config)),
        Err(e) => {
            warn!(error = %e, "failed to load cache, starting fresh");
            NodeCache::new(initial_node_id(config))
        }
    }
}

fn now_unix_ms() -> i64 {
    chv_common::now_unix_ms()
}

async fn resolve_tls_paths(
    cache: &Arc<tokio::sync::Mutex<NodeCache>>,
    config: &AgentConfig,
) -> (Option<PathBuf>, Option<PathBuf>, Option<PathBuf>) {
    let cache = cache.lock().await;
    let tls_cert = cache
        .certificate_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| config.tls_cert_path.clone());
    let tls_key = cache
        .private_key_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| config.tls_key_path.clone());
    let ca_cert = cache
        .ca_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| config.ca_cert_path.clone());
    (tls_cert, tls_key, ca_cert)
}

async fn connect_control_plane(
    cache: &Arc<tokio::sync::Mutex<NodeCache>>,
    config: &AgentConfig,
) -> Result<ControlPlaneClient, ChvError> {
    let (tls_cert, tls_key, ca_cert) = resolve_tls_paths(cache, config).await;
    ControlPlaneClient::new(
        &config.control_plane_addr,
        tls_cert.as_deref(),
        tls_key.as_deref(),
        ca_cert.as_deref(),
    )
    .await
}

async fn connect_enrollment_client(
    cache: &Arc<tokio::sync::Mutex<NodeCache>>,
    config: &AgentConfig,
) -> Result<EnrollmentClient, ChvError> {
    let (tls_cert, tls_key, ca_cert) = resolve_tls_paths(cache, config).await;
    EnrollmentClient::connect_with_tls(
        &config.control_plane_addr,
        tls_cert.as_deref(),
        tls_key.as_deref(),
        ca_cert.as_deref(),
    )
    .await
}

fn bootstrap_result_meta(
    node_id: &str,
) -> control_plane_node_api::control_plane_node_api::RequestMeta {
    control_plane_node_api::control_plane_node_api::RequestMeta {
        operation_id: format!("bootstrap-result-{node_id}"),
        requested_by: "agent".to_string(),
        target_node_id: node_id.to_string(),
        desired_state_version: String::new(),
        request_unix_ms: now_unix_ms(),
    }
}

/// Persist the newly issued identity and record it in the NodeCache.
///
/// Returns `true` only when all three files and the NodeCache record were
/// durably written. A partial write or a cache-save failure is treated as
/// failure: the caller must not trust a half-written credential, must not
/// mark enrollment complete, and must report the failure using the issued
/// identity still held in memory.
async fn persist_enrollment_material(
    config: &AgentConfig,
    cache: &mut NodeCache,
    issued: &BootstrapResponse,
) -> bool {
    let cert_path = config.runtime_dir.join("agent.crt");
    let key_path = config.runtime_dir.join("agent.key");
    let ca_path = config.runtime_dir.join("ca.crt");
    if let Err(e) = tokio::fs::create_dir_all(&config.runtime_dir).await {
        warn!(error = %e, "failed to create runtime dir");
        return false;
    }

    let mut ok = true;
    if let Err(e) = write_file_durable(&cert_path, &issued.certificate_pem, 0o644).await {
        warn!(error = %e, "failed to write certificate");
        ok = false;
    }
    if let Err(e) = write_private_file(&key_path, &issued.private_key_pem).await {
        warn!(error = %e, "failed to write private key");
        ok = false;
    }
    if let Err(e) = write_file_durable(&ca_path, &issued.ca_pem, 0o644).await {
        warn!(error = %e, "failed to write ca certificate");
        ok = false;
    }
    if !ok {
        return false;
    }

    cache.node_id = issued.node_id.clone();
    cache.certificate_path = Some(cert_path.to_string_lossy().to_string());
    cache.private_key_path = Some(key_path.to_string_lossy().to_string());
    cache.ca_path = Some(ca_path.to_string_lossy().to_string());
    cache.last_certificate_rotation_unix_ms = Some(now_unix_ms());
    cache.enrollment_complete = true;
    if let Err(e) = cache.save(&config.cache_path).await {
        // The files are durable, but without the cache record a restart
        // cannot know it enrolled (and the bootstrap token is single-use).
        // Report failure rather than claiming durable enrollment.
        warn!(error = %e, "failed to save cache after enrollment");
        cache.enrollment_complete = false;
        return false;
    }
    info!(node_id = %issued.node_id, "enrollment complete");
    true
}

/// Report the first-enrollment result on the newly issued node identity.
///
/// The enrollment connection intentionally uses the *pre-enrollment* client
/// certificate, so the control plane's peer-node-id check rejects
/// `ReportBootstrapResult` on that channel (#709). Success is reported through
/// a fresh client resolved from the cache, which now points at the issued
/// certificate/key.
///
/// When persisting the issued material failed, the issued certificate may not
/// be on disk, but the response still holds it. Report the failure with that
/// in-memory identity: it is the credential the control plane just issued for
/// this node, and using it keeps the peer-node-id binding intact. The
/// pre-enrollment channel is deliberately not used as a fallback: the control
/// plane's peer identity check rejects it, so attempting it would only add a
/// misleading denied audit row.
async fn report_enrollment_result(
    config: &AgentConfig,
    cache: &NodeCache,
    issued: &BootstrapResponse,
    persisted: bool,
) {
    let (status, message) = if persisted {
        ("ok", "bootstrap complete")
    } else {
        ("failed", "failed to persist enrollment material")
    };

    let result = if persisted {
        let tls_cert = cache
            .certificate_path
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| config.tls_cert_path.clone());
        let tls_key = cache
            .private_key_path
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| config.tls_key_path.clone());
        let ca_cert = cache
            .ca_path
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| config.ca_cert_path.clone());
        match ControlPlaneClient::new(
            &config.control_plane_addr,
            tls_cert.as_deref(),
            tls_key.as_deref(),
            ca_cert.as_deref(),
        )
        .await
        {
            Ok(mut client) => {
                client
                    .report_bootstrap_result(
                        bootstrap_result_meta(&issued.node_id),
                        &issued.node_id,
                        status,
                        message,
                    )
                    .await
            }
            Err(e) => Err(e),
        }
    } else {
        match ControlPlaneClient::new_with_pem(
            &config.control_plane_addr,
            issued.certificate_pem.clone(),
            issued.private_key_pem.clone(),
            Some(issued.ca_pem.clone()),
        )
        .await
        {
            Ok(mut client) => {
                client
                    .report_bootstrap_result(
                        bootstrap_result_meta(&issued.node_id),
                        &issued.node_id,
                        status,
                        message,
                    )
                    .await
            }
            Err(e) => {
                // The issued identity is the only channel the control plane
                // will accept for this node. Do not fall back to the
                // pre-enrollment certificate: it is guaranteed to be rejected
                // and would leave a misleading denied audit row.
                warn!(
                    error = %e,
                    "issued identity unusable for bootstrap failure report; \
                     not attempting the pre-enrollment channel"
                );
                Err(e)
            }
        }
    };

    if let Err(e) = result {
        warn!(error = %e, "failed to report bootstrap result");
    }
}

async fn enqueue_pending_message(
    cache: &Arc<tokio::sync::Mutex<NodeCache>>,
    cache_path: &Path,
    message: PendingControlPlaneMessage,
    connectivity: &mut ConnectivityTracker,
) {
    connectivity.record_message_deferred();
    let mut cache = cache.lock().await;
    cache.enqueue_pending_message(message);
    if let Err(e) = cache.save(cache_path).await {
        warn!(error = %e, "failed to save cache after queueing deferred control-plane message");
    }
}

async fn send_or_defer_control_plane_message(
    cache: &Arc<tokio::sync::Mutex<NodeCache>>,
    cache_path: &Path,
    config: &AgentConfig,
    telemetry: &mut Option<ControlPlaneClient>,
    message: PendingControlPlaneMessage,
    connectivity: &mut ConnectivityTracker,
) {
    let now = now_unix_ms();
    if telemetry.is_none() {
        match connect_control_plane(cache, config).await {
            Ok(client) => {
                info!("connected to control plane");
                // Flush the deferred backlog through the shared drain
                // (#582): snapshot-dispatch-commit, outside the cache
                // lock, deadline-bounded per dispatch. On failure the
                // drain has already recorded the connectivity failure
                // and dropped the client; the in-flight message then
                // defers like any other.
                let client = chv_agent_core::control_plane::drain_pending_control_plane_queue(
                    cache,
                    cache_path,
                    Some(client),
                    connectivity,
                )
                .await;
                match client {
                    Some(client) => {
                        connectivity.record_success(now);
                        *telemetry = Some(client);
                    }
                    None => {
                        enqueue_pending_message(cache, cache_path, message, connectivity).await;
                        return;
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "control plane unavailable; deferring report");
                connectivity.record_failure(now);
                enqueue_pending_message(cache, cache_path, message, connectivity).await;
                return;
            }
        }
    }

    let Some(client) = telemetry.as_mut() else {
        connectivity.record_failure(now);
        enqueue_pending_message(cache, cache_path, message, connectivity).await;
        return;
    };

    // Same legacy-queue repair as the shared drain: a message persisted by
    // a build that predates `MigrationProgress.node_id` has an empty field,
    // and the control plane will reject it. Fill it from the local identity
    // before dispatch.
    let local_node_id = cache.lock().await.node_id.clone();
    if let Err(e) = client
        .dispatch_pending_message(&message, &local_node_id)
        .await
    {
        warn!(error = %e, "failed to send control-plane message, deferring");
        *telemetry = None;
        connectivity.record_failure(now);
        enqueue_pending_message(cache, cache_path, message, connectivity).await;
    } else {
        connectivity.record_success(now);
    }
}

fn certificate_rotation_due(cache: &NodeCache, now_unix_ms: i64) -> bool {
    if !cache.enrollment_complete {
        return false;
    }
    let (cert_path, key_path) = match (
        cache.certificate_path.as_ref(),
        cache.private_key_path.as_ref(),
    ) {
        (Some(c), Some(k)) => (c, k),
        _ => return false,
    };
    if !std::path::Path::new(cert_path).exists() || !std::path::Path::new(key_path).exists() {
        return false;
    }
    cache
        .last_certificate_rotation_unix_ms
        .map(|last| now_unix_ms - last >= CERT_ROTATION_INTERVAL_SECS * 1000)
        .unwrap_or(true)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!(
            "{} {} (commit {}, build {}, channel {})",
            env!("CARGO_PKG_NAME"),
            env!("CHV_VERSION"),
            env!("CHV_GIT_SHA"),
            env!("CHV_BUILD_DATE"),
            env!("CHV_RELEASE_CHANNEL"),
        );
        return Ok(());
    }

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls ring crypto provider");

    let config_path = std::env::args().nth(1).map(PathBuf::from);
    let config = load_agent_config(config_path.as_deref())?;

    init_logger(&config.log_level)?;

    // The agent's view of the node's stord backend (#379 DP4/DP5): learned
    // from the operator's stord.toml via the #385 stord_config_path key —
    // the same file the supervisor parses for respawn validation — so it
    // is available BEFORE the supervisor starts stord. Absent path (the
    // config default) or an unusable file means local; the supervisor's
    // own stord class validation remains the enforcement boundary.
    // Computed before every authority-mode branch: the inventory reporter,
    // the AgentServer/Reconciler, AND the core runtimes (native and
    // managed) all consume it.
    let stord_backend =
        StordBackendInfo::from_stord_config_path(config.stord_config_path.as_deref());

    // Security-mode gate (production-readiness prompt 03, workstream A;
    // mirrors the control plane's `validate_security_mode` from #233/PR #253):
    // `CHV_ALLOW_INSECURE=1` relaxes the enrolled-agent mTLS credential
    // requirement and is valid only on builds compiled with the `dev` Cargo
    // feature. Validated here — before the authority-mode branch, the cache,
    // any listener, adapter, or runtime wiring — so a production build that
    // requests insecure mode exits with a clean, operator-greppable error
    // instead of running without its peer identity.
    let allow_insecure_requested = std::env::var("CHV_ALLOW_INSECURE")
        .map(|v| v == "1")
        .unwrap_or(false);
    if allow_insecure_requested && !cfg!(feature = "dev") {
        return Err(
            "CHV_ALLOW_INSECURE=1 is set but this build does not have the 'dev' Cargo feature. \
             This env var relaxes the enrolled-agent mTLS credential requirement and must never \
             be enabled in production. To use it for local development, rebuild with: \
             cargo build --features dev"
                .into(),
        );
    }

    // #662: the agent-auth bypass is a second local-development escape
    // hatch. Config validation already refuses it when production = true;
    // also require the same compile-time `dev` feature as CHV_ALLOW_INSECURE
    // so a production build cannot be talked into disabling authentication
    // by configuration alone.
    if config.security.allow_insecure_agent_auth && !cfg!(feature = "dev") {
        return Err(
            "security.allow_insecure_agent_auth = true is set but this build does not have \
             the 'dev' Cargo feature. The bypass disables control-plane authentication on the \
             agent socket and must never be enabled in production. To use it for local \
             development, rebuild with: cargo build --features dev"
                .into(),
        );
    }

    info!(
        "{} starting (version {}, commit {}, channel {})",
        env!("CARGO_PKG_NAME"),
        env!("CHV_VERSION"),
        env!("CHV_GIT_SHA"),
        env!("CHV_RELEASE_CHANNEL"),
    );

    if config.authority_mode == AgentAuthorityMode::CoreNative {
        return run_core_native(&config, &stord_backend).await;
    }

    let mut cache = load_or_initialize_cache(&config).await;

    // Enrollment
    if !cache.enrollment_complete {
        if let Some(token_path) = &config.bootstrap_token_path {
            match tokio::fs::read_to_string(token_path).await {
                Ok(token) => {
                    let token = token.trim();
                    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
                        .unwrap_or_else(|_| "unknown".to_string())
                        .trim()
                        .to_string();
                    let reporter = InventoryReporter::new(&cache.node_id, &hostname)
                        .with_stord_backend(stord_backend.clone())
                        .with_authority_mode(config.authority_mode.clone());
                    // Best-effort fabric identity (ADR-021): nwd may not be
                    // up yet during enrollment; the empty identity is
                    // reported then and re-sent on the periodic inventory
                    // cycle once nwd is reachable.
                    let fabric_identity =
                        chv_agent_core::inventory::fetch_fabric_identity(&config.nwd_socket).await;
                    let inventory = reporter.build_inventory_with_fabric_identity(fabric_identity);
                    let versions = reporter.build_versions();
                    let tls_cert = cache
                        .certificate_path
                        .as_ref()
                        .map(PathBuf::from)
                        .or_else(|| config.tls_cert_path.clone());
                    let tls_key = cache
                        .private_key_path
                        .as_ref()
                        .map(PathBuf::from)
                        .or_else(|| config.tls_key_path.clone());
                    let ca_cert = cache
                        .ca_path
                        .as_ref()
                        .map(PathBuf::from)
                        .or_else(|| config.ca_cert_path.clone());
                    match EnrollmentClient::connect_with_tls(
                        &config.control_plane_addr,
                        tls_cert.as_deref(),
                        tls_key.as_deref(),
                        ca_cert.as_deref(),
                    )
                    .await
                    {
                        Ok(mut client) => {
                            match client.enroll_node(token, inventory, versions).await {
                                Ok(resp) => {
                                    let persisted =
                                        persist_enrollment_material(&config, &mut cache, &resp)
                                            .await;
                                    report_enrollment_result(&config, &cache, &resp, persisted)
                                        .await;
                                }
                                Err(e) => {
                                    warn!(error = %e, "enrollment failed");
                                }
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "failed to connect to enrollment endpoint");
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, path = %token_path.display(), "failed to read bootstrap token");
                }
            }
        }
    }

    // Enforce mTLS post-enrollment (unless dev mode)
    let allow_insecure = std::env::var("CHV_ALLOW_INSECURE")
        .map(|v| v == "1")
        .unwrap_or(false);
    if !allow_insecure && cache.enrollment_complete {
        let has_certs = cache.certificate_path.is_some()
            && cache.private_key_path.is_some()
            && cache.ca_path.is_some();
        if !has_certs {
            return Err(
                "mTLS required: agent is enrolled but TLS credentials are missing. Set CHV_ALLOW_INSECURE=1 for dev"
                    .into(),
            );
        }
    }

    let process_adapter = Arc::new(
        ProcessCloudHypervisorAdapter::new(&config.chv_binary_path)
            // G5: wire the vsock monitoring relay BEFORE adoption so
            // live vsock VMs get their endpoint back at startup.
            .with_vsock_relay(maybe_vsock_relay(&config)),
    );
    // Rebuild the in-memory VM map from on-disk runtime state before any
    // mode wiring: VMs outlive agent restarts, and lifecycle ops must not
    // fail with NotFound against VMs this node still runs. Fail-soft: an
    // adoption failure degrades to today's empty-map semantics (warned)
    // rather than bricking the agent.
    if let Err(e) = process_adapter.adopt_running_vms(&config.runtime_dir).await {
        warn!(error = %e, "vm runtime state adoption failed; starting with an empty runtime map");
    }
    spawn_boot_watchdog(&process_adapter, &config.watchdog);
    // The dyn Arc is the one the runtime uses; the concrete handle stays
    // for the graceful-shutdown console drain at process exit.
    let adapter: Arc<dyn chv_agent_runtime_ch::adapter::CloudHypervisorAdapter> =
        process_adapter.clone();
    let vm_runtime = VmRuntime::new(adapter.clone());

    let cache = Arc::new(tokio::sync::Mutex::new(cache));

    let mut core_owner = None;
    if config.authority_mode == AgentAuthorityMode::CoreManaged {
        let owner = match start_core_managed(
            &config,
            adapter.clone(),
            &cache,
            config.cache_path.clone(),
            &stord_backend,
        )
        .await
        {
            Ok(owner) => owner,
            Err(e) => {
                // Adoption above already attached live consoles: exit via
                // the same clean-close discipline as the graceful paths
                // so an early-startup failure cannot freeze a running
                // guest.
                process_adapter.drain_and_close_consoles().await;
                return Err(e);
            }
        };
        core_owner = Some(owner);
    }

    let mut agent_server = AgentServer::new(
        cache.clone(),
        vm_runtime.clone(),
        config.stord_socket.clone(),
        config.nwd_socket.clone(),
        Some(config.cache_path.clone()),
        config.runtime_dir.clone(),
    )
    .with_stord_backend(stord_backend.clone())
    // #662: fail-closed application-layer authentication for the
    // control-plane → agent Unix socket. Config validation has already
    // refused the insecure bypass in production.
    .with_agent_auth(AgentAuth::new(
        config.effective_agent_auth_token(),
        config.security.allow_insecure_agent_auth,
        config.security.agent_auth_allowed_peer_uids.clone(),
    ))
    .with_allow_insecure_migration_dispatch(config.allow_insecure_migration_dispatch);
    if config.allow_insecure_migration_dispatch {
        tracing::warn!(
            "SECURITY: allow_insecure_migration_dispatch = true — this agent will honor a \
             control-plane request to dispatch disk migrations WITHOUT per-volume \
             authorizations. Development-only; never run this in production"
        );
    }
    if let Some(owner) = &core_owner {
        agent_server = agent_server.with_core_authority(owner.authority());
    }
    // Share the migration registry with the reconciler so drain evacuation
    // can gate on in-flight migrations (see reconcile.rs Draining arm).
    let migration_registry = agent_server.migration_tasks.clone();
    let server_socket = config.socket_path.clone();
    let agent_server_clone = agent_server.clone();
    let mut agent_server_handle = tokio::spawn(async move {
        if let Err(e) = agent_server_clone.serve(&server_socket).await {
            tracing::error!(error = %e, "agent server exited with error — node is unreachable");
        }
    });

    let console_bind = config.console_bind.clone();
    let console_listener = match ConsoleServer::try_bind(&console_bind).await {
        Ok(listener) => listener,
        Err(e) => {
            tracing::error!(
                bind = %console_bind,
                error = %e,
                "FATAL: console server cannot bind"
            );
            // Adoption above already attached live consoles: exit via the
            // same clean-close discipline as the graceful paths so an
            // early-startup failure cannot freeze a running guest.
            process_adapter.drain_and_close_consoles().await;
            return Err(e.into());
        }
    };
    let console_server =
        ConsoleServer::new(vm_runtime.clone(), config.effective_console_jwt_secret());
    tokio::spawn(async move {
        if let Err(e) = console_server.run(console_listener).await {
            warn!(error = %e, bind = %console_bind, "console server exited");
        }
    });

    // Metrics HTTP server for Prometheus scraping
    let metrics_bind_addr = resolve_metrics_bind(config.metrics_bind.as_deref());
    if config.metrics_bind.is_none() {
        warn!(
            bind = %metrics_bind_addr,
            "metrics_bind is not set; binding /metrics on loopback — set metrics_bind \
             explicitly to serve a different address"
        );
    }
    let node_id = cache.lock().await.node_id.clone();
    let metrics_state = Arc::new(tokio::sync::Mutex::new(MetricsState::new(node_id.clone())));
    // Native monitoring sampler (G1/G2, #602): one bounded task,
    // independent of reconciliation and state reports, collecting node
    // contract samples (5 s cadence) and VM samples (10 s cadence)
    // through chv-monitoring-core. Health counters are exported on
    // /metrics without VM identifiers; the latest samples feed the
    // batch sender below.
    let (sampler_health, latest_samples) = monitoring::spawn_monitoring_sampler(
        node_id.clone(),
        vm_runtime.clone(),
        chv_monitoring_core::sampler::SamplerConfig::default(),
    );
    metrics_state.lock().await.sampler_health = Some(sampler_health);

    // Node metric batch sender (G2, #602): ships the latest samples to
    // the control plane every 15 s on a DEDICATED client so manager
    // backpressure can never pause reconciliation. Purely best-effort:
    // connection failures retry with backoff and nothing here can
    // affect the agent's control loops.
    {
        let (tls_cert, tls_key, ca_cert) = resolve_tls_paths(&cache, &config).await;
        monitoring::spawn_monitoring_ingest_sender(
            node_id.clone(),
            latest_samples,
            config.control_plane_addr.clone(),
            tls_cert,
            tls_key,
            ca_cert,
        );
    }
    let metrics_state_clone = metrics_state.clone();
    tokio::spawn(async move {
        let app = metrics_router(metrics_state_clone);
        let listener = match tokio::net::TcpListener::bind(&metrics_bind_addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(bind = %metrics_bind_addr, error = %e, "metrics server cannot bind");
                return;
            }
        };
        // Match the shared listener's posture: a wildcard address exposes the
        // unauthenticated scrape surface on every interface, which needs an
        // explicit firewall/reverse-proxy decision (#657 review N2).
        if metrics_bind_addr
            .parse::<std::net::SocketAddr>()
            .map(|addr| addr.ip().is_unspecified())
            .unwrap_or(false)
        {
            tracing::warn!(
                bind = %metrics_bind_addr,
                "metrics_bind is an unspecified/wildcard address: the unauthenticated /metrics \
                 listener is exposed on ALL interfaces. Restrict it with a firewall or network \
                 policy, or front it with an authenticated reverse proxy; prefer a specific \
                 management address or loopback (issue #657)"
            );
        }
        info!(bind = %metrics_bind_addr, "metrics server listening");
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "metrics server exited with error");
        }
    });

    // The Reconciler drives the node state machine (including daemon health
    // probes) in every mode. Its provider-MUTATION surface is mode-selected at
    // construction (M2.3): core-managed builds an observe-only Reconciler that
    // structurally holds no mutation state and has no setter, so it can never
    // act as a second authority — the Core runtime is the sole provider
    // effector (NodeCache is rebuilt from the Core store and projected only
    // after Core execution). Legacy mode keeps the full mutation surface.
    let mut reconciler = match config.authority_mode {
        // Core-managed: observe-only Reconciler (no mutation surface at all).
        AgentAuthorityMode::CoreManaged => Reconciler::new_observe_only(
            cache.clone(),
            vm_runtime.clone(),
            config.stord_socket.clone(),
            config.nwd_socket.clone(),
            migration_registry,
        )
        .await
        .with_stord_backend(stord_backend.clone()),
        // Legacy: the full legacy provider-mutation surface (explicit opt-in).
        AgentAuthorityMode::Legacy => {
            warn!(
                "authority_mode=legacy: the agent keeps its own provider-mutation \
                 surface (compatibility adapter). The qualified single-authority \
                 deployment is core-managed; legacy requires an explicit opt-in"
            );
            Reconciler::new_legacy(
                cache.clone(),
                vm_runtime.clone(),
                config.stord_socket.clone(),
                config.nwd_socket.clone(),
                config.runtime_dir.clone(),
                migration_registry,
            )
            .await
            .with_stord_backend(stord_backend.clone())
        }
        // CoreNative returns in run_core_native() well before this point; a
        // future variant here becomes a compile error instead of silently
        // defaulting to the mutation-capable Reconciler.
        AgentAuthorityMode::CoreNative => {
            unreachable!("CoreNative mode exited normally before Reconciler composition")
        }
    };

    let mut supervisor = DaemonSupervisor::new(
        config.stord_binary_path.clone(),
        config.nwd_binary_path.clone(),
        config.stord_socket.clone(),
        config.nwd_socket.clone(),
        config.runtime_dir.clone(),
        config.stord_path_allowlist.clone(),
        config.stord_config_path.clone(),
        config.nwd_config_path.clone(),
    );

    if let Err(e) = supervisor.start_all().await {
        warn!(error = %e, "failed to start local daemons during bootstrap");
    }

    let mut telemetry = match connect_control_plane(&cache, &config).await {
        Ok(client) => {
            info!("connected to control plane");
            Some(client)
        }
        Err(e) => {
            warn!(error = %e, "control plane unavailable; will retry later");
            None
        }
    };

    // Connectivity tracker — makes partition-autonomous operation explicit and observable.
    let mut connectivity = ConnectivityTracker::new();
    if telemetry.is_some() {
        connectivity.record_success(now_unix_ms());
    }

    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .unwrap_or_else(|_| "unknown".to_string())
        .trim()
        .to_string();
    let node_id = cache.lock().await.node_id.clone();
    let inventory_reporter = InventoryReporter::new(&node_id, hostname)
        .with_stord_backend(stord_backend.clone())
        .with_authority_mode(config.authority_mode.clone());
    let mut tick_count = 0u64;
    let mut consecutive_health_failures = 0u32;
    let mut consecutive_reconcile_failures: u32 = 0;
    let mut prev_connectivity = connectivity.state();

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    let mut interval = tokio::time::interval(Duration::from_secs(5));
    // Fatality watchdog: a fatally terminated Core executor must fail the
    // process promptly (≤500 ms, matching the core-native loop's cadence),
    // not on the next 5 s tick — the authority would otherwise keep
    // acknowledging operations that are never executed for up to 5 s.
    let mut fatality_check = tokio::time::interval(Duration::from_millis(500));
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = fatality_check.tick() => {
                // The health aggregation and telemetry paths have already
                // marked the node Degraded and reported it; exit non-zero so
                // the supervisor restarts the agent.
                if core_owner
                    .as_ref()
                    .is_some_and(|owner| owner.executor_fatal())
                {
                    tracing::error!(
                        "core journal executor terminated fatally — shutting down for supervisor restart"
                    );
                    supervisor.shutdown().await;
                    if let Some(owner) = core_owner.take() {
                        let _ = owner.shutdown().await;
                    }
                    // VMs outlive the agent: convert process exit into a
                    // clean close of every live serial connection so a
                    // running guest cannot be frozen by the exit itself.
                    process_adapter.drain_and_close_consoles().await;
                    return Err("core journal executor terminated fatally".into());
                }
                // Watchdog tick only: skip the 5 s body's heavy work.
                continue;
            }
            _ = sigterm.recv() => {
                info!("received SIGTERM, shutting down gracefully");
                supervisor.shutdown().await;
                if let Some(owner) = core_owner.take() { let _ = owner.shutdown().await; }
                break;
            }
            _ = sigint.recv() => {
                info!("received SIGINT, shutting down gracefully");
                supervisor.shutdown().await;
                if let Some(owner) = core_owner.take() { let _ = owner.shutdown().await; }
                break;
            }
            _ = &mut agent_server_handle => {
                tracing::error!("agent gRPC server exited unexpectedly — shutting down");
                supervisor.shutdown().await;
                if let Some(owner) = core_owner.take() { let _ = owner.shutdown().await; }
                // Exit non-zero: the systemd unit uses Restart=on-failure,
                // and in core-managed mode this process is the sole Core
                // authority — a clean exit here would strand the node with
                // no supervisor recovery (mirrors the executor-fatal path).
                process_adapter.drain_and_close_consoles().await;
                return Err("agent gRPC server exited unexpectedly".into());
            }
        }

        if let Err(e) = supervisor.restart_if_needed().await {
            warn!(error = %e, "supervisor restart failed");
        }

        // Detect connectivity transition: Disconnected/Reconnecting ->
        // Connected. The pending queue is flushed by the per-tick drain
        // below on this very tick — outside the cache lock and with a
        // per-dispatch deadline — so the transition itself only needs
        // the log line (the pre-#582 dedicated reconnect flush held the
        // cache lock across unbounded dispatch and is subsumed).
        let current_connectivity = connectivity.state();
        if current_connectivity == ConnectivityState::Connected
            && prev_connectivity != ConnectivityState::Connected
        {
            info!("connectivity restored; pending control-plane messages drain on this tick");
        }
        prev_connectivity = current_connectivity;

        // #582: drain the pending control-plane queue on every tick
        // while connected — migration progress reports (and other
        // unconditional enqueues) would otherwise sit queued until the
        // next reconnect, which never comes during a stable
        // connection. See drain_pending_control_plane_queue for the
        // full rationale and failure handling.
        telemetry = chv_agent_core::control_plane::drain_pending_control_plane_queue(
            &cache,
            &config.cache_path,
            telemetry,
            &mut connectivity,
        )
        .await;

        let now = now_unix_ms();
        let rotate_due = {
            let cache = cache.lock().await;
            certificate_rotation_due(&cache, now)
        };
        if rotate_due {
            match connect_enrollment_client(&cache, &config).await {
                Ok(mut client) => {
                    let node_id = cache.lock().await.node_id.clone();
                    let meta = control_plane_node_api::control_plane_node_api::RequestMeta {
                        operation_id: format!("cert-rotate-{}", tick_count),
                        requested_by: "agent".to_string(),
                        target_node_id: node_id.clone(),
                        desired_state_version: String::new(),
                        request_unix_ms: now,
                    };
                    match client.rotate_node_certificate(meta, &node_id).await {
                        Ok(resp) => {
                            let (cert_path, key_path, ca_path) = {
                                let cache = cache.lock().await;
                                (
                                    cache.certificate_path.clone(),
                                    cache.private_key_path.clone(),
                                    cache.ca_path.clone(),
                                )
                            };
                            if let (Some(cert_path), Some(key_path), Some(ca_path)) =
                                (cert_path, key_path, ca_path)
                            {
                                let write_result = async {
                                    write_file_durable(
                                        Path::new(&cert_path),
                                        &resp.certificate_pem,
                                        0o644,
                                    )
                                    .await?;
                                    write_private_file(Path::new(&key_path), &resp.private_key_pem)
                                        .await?;
                                    write_file_durable(Path::new(&ca_path), &resp.ca_pem, 0o644)
                                        .await?;
                                    Ok::<(), std::io::Error>(())
                                }
                                .await;
                                match write_result {
                                    Ok(()) => {
                                        let mut cache = cache.lock().await;
                                        cache.last_certificate_rotation_unix_ms = Some(now);
                                        if let Err(e) = cache.save(&config.cache_path).await {
                                            warn!(error = %e, "failed to save cache after certificate rotation");
                                        }
                                    }
                                    Err(e) => {
                                        warn!(error = %e, "failed to persist rotated certificate material");
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "certificate rotation failed");
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, "failed to connect for certificate rotation");
                }
            }
        }

        let stord_ok = match StordClient::connect(&config.stord_socket).await {
            Ok(mut c) => c.health_probe().await.unwrap_or(false),
            Err(_) => false,
        };

        let nwd_ok = match NwdClient::connect(&config.nwd_socket).await {
            Ok(mut c) => c.health_probe().await.unwrap_or(false),
            Err(_) => false,
        };

        // #562 (Option 1, maintainer ruling 2026-10-08): the host-resource
        // pressure check runs once per tick — the same sysinfo getters the
        // #558 migration verified (memory refresh + root-mount disk probe)
        // — and feeds both consumers the aggregator was designed for: the
        // node-state derivation below (a TenantReady node under the 90%
        // memory / 5 GB disk thresholds degrades) and the /metrics
        // pressure gauges, updated with the rest of MetricsState at the
        // end of the tick so `chv_agent_memory_pressure` /
        // `chv_agent_disk_pressure` stop emitting constant 0. The
        // scrape-time collector in `metrics_server.rs` keeps its own
        // sample for the byte counters — one read per tick, one per
        // scrape, the per-scrape precedent.
        let host_pressure = check_host_resources();

        let mut health = HealthAggregator::new();
        health.update_stord(stord_ok);
        health.update_nwd(nwd_ok);
        health.update_resource_pressure(host_pressure.clone());
        if let Some(owner) = &core_owner {
            // A wedged Core journal must not silently pass for a healthy node:
            // accepted operations would stop executing while the API keeps
            // acknowledging them. A fatal executor termination is included
            // here even though the poller stopped scanning at that point.
            health.update_core_journal(owner.journal_scan_healthy() && !owner.executor_fatal());
        }

        let current_state = reconciler.current_state().await;
        let derived = health.derive_node_state(current_state);
        if derived != current_state {
            let from_str = current_state.as_str().to_string();
            info!(
                from = %from_str,
                to = %derived.as_str(),
                "state transition"
            );
            if let Err(e) = reconciler.transition_state(derived).await {
                warn!(error = %e, "invalid state transition ignored");
            } else {
                {
                    let cache = cache.lock().await;
                    if let Err(e) = cache.save(&config.cache_path).await {
                        warn!(error = %e, "failed to save cache");
                    }
                }
                if derived != NodeState::Degraded {
                    consecutive_health_failures = 0;
                }
                let severity = match derived {
                    NodeState::Failed => "Critical",
                    NodeState::Degraded => "Warning",
                    NodeState::TenantReady => "Info",
                    _ => "Info",
                };
                let (target_node_id, desired_state_version) = {
                    let cache = cache.lock().await;
                    (cache.node_id.clone(), cache.observed_generation.clone())
                };
                let reporter = TelemetryReporter::new(&target_node_id);
                let event = reporter.event_report(
                    control_plane_node_api::control_plane_node_api::RequestMeta {
                        operation_id: format!("state-transition-{}", tick_count),
                        requested_by: "agent".to_string(),
                        target_node_id,
                        desired_state_version,
                        request_unix_ms: now_unix_ms(),
                    },
                    severity,
                    "StateTransition",
                    &format!(
                        "node transitioned from {} to {}",
                        from_str,
                        derived.as_str()
                    ),
                );
                send_or_defer_control_plane_message(
                    &cache,
                    &config.cache_path,
                    &config,
                    &mut telemetry,
                    PendingControlPlaneMessage::event(event),
                    &mut connectivity,
                )
                .await;
            }
        } else if current_state == NodeState::Degraded {
            consecutive_health_failures += 1;
            if consecutive_health_failures >= FAILED_THRESHOLD {
                info!("health failures exceeded threshold, transitioning to Failed");
                if let Err(e) = reconciler.transition_state(NodeState::Failed).await {
                    warn!(error = %e, "failed to transition to Failed");
                } else {
                    {
                        let cache = cache.lock().await;
                        if let Err(e) = cache.save(&config.cache_path).await {
                            warn!(error = %e, "failed to save cache");
                        }
                    }
                    let (target_node_id, desired_state_version) = {
                        let cache = cache.lock().await;
                        (cache.node_id.clone(), cache.observed_generation.clone())
                    };
                    let reporter = TelemetryReporter::new(&target_node_id);
                    let event = reporter.event_report(
                        control_plane_node_api::control_plane_node_api::RequestMeta {
                            operation_id: format!("state-transition-failed-{}", tick_count),
                            requested_by: "agent".to_string(),
                            target_node_id,
                            desired_state_version,
                            request_unix_ms: now_unix_ms(),
                        },
                        "Critical",
                        "StateTransition",
                        "node transitioned to Failed after persistent health degradation",
                    );
                    send_or_defer_control_plane_message(
                        &cache,
                        &config.cache_path,
                        &config,
                        &mut telemetry,
                        PendingControlPlaneMessage::event(event),
                        &mut connectivity,
                    )
                    .await;
                }
                consecutive_health_failures = 0;
            }
        } else {
            consecutive_health_failures = 0;
        }

        let (node_id, node_state, observed_generation, last_error) = {
            let cache = cache.lock().await;
            (
                cache.node_id.clone(),
                cache.node_state.clone(),
                cache.observed_generation.clone(),
                cache.last_error.clone(),
            )
        };
        let reporter = TelemetryReporter::new(&node_id);
        let report = reporter.node_state_report(
            node_state.as_str(),
            observed_generation.as_str(),
            if reconciler.current_state().await == NodeState::TenantReady {
                "Healthy"
            } else {
                "Degraded"
            },
            last_error,
        );
        send_or_defer_control_plane_message(
            &cache,
            &config.cache_path,
            &config,
            &mut telemetry,
            PendingControlPlaneMessage::node_state(report),
            &mut connectivity,
        )
        .await;

        let mut reported = reconciler.reported_vms().await;
        // #368 P1: in core-managed mode, merge the journal's latest-create
        // states into the report set BEFORE emitting — a terminally failed
        // create must report Failed with its public-safe code (never the
        // desired-state phantom), an incomplete create must report Pending,
        // and a failed create with no cache fragment (the projection only
        // follows Succeeded outcomes) must still be reported at all.
        if let Some(owner) = &core_owner {
            match owner.authority().latest_create_states().await {
                Ok(latest) => {
                    reported = apply_core_create_states(reported, latest);
                }
                Err(error) => {
                    warn!(
                        %error,
                        "failed to read latest create states from the Core authority; \
                         emitting fragment-derived VM states only"
                    );
                }
            }
        }
        for vm in reported {
            let mut counters = control_plane_node_api::control_plane_node_api::VmStateReport {
                node_id: node_id.clone(),
                vm_id: vm.vm_id.clone(),
                runtime_status: vm.runtime_status.clone(),
                observed_generation: vm.observed_generation.clone(),
                // Core-managed telemetry reports the Core projection
                // (desired state) as runtime_status — a documented
                // residual, narrowed but not closed by the #368 P1 merge
                // at the call site above (a terminally failed or in-flight
                // create reports Failed/Pending with its public-safe code;
                // converged VMs still report their desired state) — so
                // observed health is genuinely not known here, and a stuck
                // (inspect-required) VM must not be reported Healthy.
                // Legacy reports keep their historical value.
                health_status: if core_owner.is_some() {
                    "Unknown"
                } else {
                    "Healthy"
                }
                .to_string(),
                last_error: vm.last_error.unwrap_or_default(),
                reported_unix_ms: now_unix_ms(),
                cpu_percent: 0.0,
                memory_bytes_used: 0,
                // Configured guest memory is configuration, not a
                // measurement — the spec (VmRecord) is authoritative
                // here; the sampler's measured host-accounted used
                // bytes fill the field below.
                memory_bytes_total: vm.memory_bytes as i64,
                disk_bytes_read: 0,
                disk_bytes_written: 0,
                net_bytes_rx: 0,
                net_bytes_tx: 0,
            };

            if vm.runtime_status == "Running" {
                if let Ok(c) = reconciler.vm_runtime().vm_counters(&vm.vm_id).await {
                    counters.cpu_percent = c.cpu_percent;
                    counters.memory_bytes_used = c.memory_bytes_used as i64;
                    // memory_bytes_total stays the spec-derived value
                    // above: the adapter returns 0 for it by contract
                    // (configuration, not measurement — see VmCounters'
                    // field docs), so overwriting here would zero the
                    // configured total for every Running VM.
                    counters.disk_bytes_read = c.disk_bytes_read as i64;
                    counters.disk_bytes_written = c.disk_bytes_written as i64;
                    counters.net_bytes_rx = c.net_bytes_rx as i64;
                    counters.net_bytes_tx = c.net_bytes_tx as i64;
                }
            }

            send_or_defer_control_plane_message(
                &cache,
                &config.cache_path,
                &config,
                &mut telemetry,
                PendingControlPlaneMessage::vm_state(counters),
                &mut connectivity,
            )
            .await;
        }

        let volume_generations: std::collections::HashMap<String, String> = {
            let cache = cache.lock().await;
            cache
                .volume_handles
                .keys()
                .map(|k| {
                    (
                        k.clone(),
                        cache.volume_generations.get(k).cloned().unwrap_or_default(),
                    )
                })
                .collect()
        };
        for (volume_id, observed_generation) in volume_generations {
            let vol_report = reporter.volume_state_report(
                &volume_id,
                "Attached",
                observed_generation.as_str(),
                "Healthy",
            );
            send_or_defer_control_plane_message(
                &cache,
                &config.cache_path,
                &config,
                &mut telemetry,
                PendingControlPlaneMessage::volume_state(vol_report),
                &mut connectivity,
            )
            .await;
        }

        let network_fragments: Vec<(String, String)> = {
            let cache = cache.lock().await;
            cache
                .network_fragments
                .iter()
                .map(|(k, v)| (k.clone(), v.generation.clone()))
                .collect()
        };
        for (network_id, generation) in network_fragments {
            let net_report =
                reporter.network_state_report(&network_id, "Ready", &generation, "Healthy");
            send_or_defer_control_plane_message(
                &cache,
                &config.cache_path,
                &config,
                &mut telemetry,
                PendingControlPlaneMessage::network_state(net_report),
                &mut connectivity,
            )
            .await;
        }

        tick_count += 1;
        if tick_count.is_multiple_of(6) {
            let op_id = format!("inventory-{}", tick_count);
            // Best-effort fabric identity (ADR-021): a transient nwd outage
            // must not block the periodic inventory report; the empty
            // identity is re-reported on the next cycle.
            let fabric_identity =
                chv_agent_core::inventory::fetch_fabric_identity(&config.nwd_socket).await;
            let inv = inventory_reporter.build_inventory_with_fabric_identity(fabric_identity);
            let ver = inventory_reporter.build_versions();
            let inv_req =
                control_plane_node_api::control_plane_node_api::ReportNodeInventoryRequest {
                    meta: Some(
                        control_plane_node_api::control_plane_node_api::RequestMeta {
                            operation_id: op_id.clone(),
                            requested_by: "agent".to_string(),
                            target_node_id: node_id.clone(),
                            desired_state_version: String::new(),
                            request_unix_ms: now_unix_ms(),
                        },
                    ),
                    inventory: Some(inv),
                };
            send_or_defer_control_plane_message(
                &cache,
                &config.cache_path,
                &config,
                &mut telemetry,
                PendingControlPlaneMessage::node_inventory(inv_req),
                &mut connectivity,
            )
            .await;

            let ver_req =
                control_plane_node_api::control_plane_node_api::ReportServiceVersionsRequest {
                    meta: Some(
                        control_plane_node_api::control_plane_node_api::RequestMeta {
                            operation_id: op_id,
                            requested_by: "agent".to_string(),
                            target_node_id: node_id.clone(),
                            desired_state_version: String::new(),
                            request_unix_ms: now_unix_ms(),
                        },
                    ),
                    versions: Some(ver),
                };
            send_or_defer_control_plane_message(
                &cache,
                &config.cache_path,
                &config,
                &mut telemetry,
                PendingControlPlaneMessage::service_versions(ver_req),
                &mut connectivity,
            )
            .await;
        }

        if let Err(e) = reconciler.run_once().await {
            warn!(error = %e, "reconcile tick failed");
            consecutive_reconcile_failures += 1;
            let backoff_secs = std::cmp::min(1u64 << consecutive_reconcile_failures.min(6), 60);
            tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        } else {
            consecutive_reconcile_failures = 0;
        }

        // Periodically persist cache so gRPC mutations are durable even without state transitions.
        {
            let cache = cache.lock().await;
            if let Err(e) = cache.save(&config.cache_path).await {
                warn!(error = %e, "failed to save cache");
            }
        }

        // Update metrics state for Prometheus scraping
        {
            let mut ms = metrics_state.lock().await;
            ms.node_id = cache.lock().await.node_id.clone();
            ms.node_state = reconciler.current_state().await.as_str().to_string();
            ms.vm_count = reconciler.reported_vms().await.len();
            ms.tick_count = tick_count;
            ms.reconcile_failures = consecutive_reconcile_failures;
            ms.health_failures = consecutive_health_failures;
            let cp_snapshot = connectivity.metrics_snapshot(now_unix_ms());
            ms.cp_connectivity_state = cp_snapshot.state;
            ms.cp_disconnected_duration_ms = cp_snapshot.disconnected_duration_ms;
            ms.cp_consecutive_failures = cp_snapshot.consecutive_failures;
            ms.cp_total_deferred_messages = cp_snapshot.total_deferred_messages;
            // #562: the same per-tick pressure sample the health
            // derivation consumed above — the gauges and the derived node
            // state can never disagree about which sample they saw.
            ms.disk_pressure = host_pressure.disk_pressure;
            ms.memory_pressure = host_pressure.memory_pressure;
            // Core journal health: emitted only when the core-managed owner
            // exists (legacy mode has no journal poller — the metrics are
            // absent, not zeroed).
            ms.core_journal = core_owner.as_ref().map(|owner| {
                chv_agent_core::metrics_server::CoreJournalMetrics {
                    scan_failures_total: owner.journal_scan_failures(),
                    healthy: owner.journal_scan_healthy() && !owner.executor_fatal(),
                    inspect_required: owner.journal_inspect_required_count(),
                }
            });
        }
    }

    // VMs outlive the agent: convert process exit into a clean close of
    // every live serial connection so a running guest cannot be frozen
    // by the exit itself (cloud-hypervisor v43 serial-manager defect —
    // see ProcessCloudHypervisorAdapter::drain_and_close_consoles).
    // Runs as the last act: the core owner / supervisor shutdown above
    // (and the executor's bounded drain inside it) can keep lifecycle
    // ops and console traffic alive until this point.
    process_adapter.drain_and_close_consoles().await;

    Ok(())
}

/// Resolve the agent's `/metrics` listener address. An explicit `metrics_bind`
/// is honored verbatim; only an unset key falls back to loopback.
fn resolve_metrics_bind(configured: Option<&str>) -> String {
    configured.unwrap_or(DEFAULT_METRICS_BIND).to_string()
}

/// Native monitoring sampler wiring (gate G1, #602).
mod monitoring;

#[cfg(test)]
mod tests {
    use super::*;
    use control_plane_node_api::control_plane_node_api as proto;
    use proto::enrollment_service_server::{EnrollmentService, EnrollmentServiceServer};
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::oneshot;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::transport::server::{TcpConnectInfo, TlsConnectInfo};
    use tonic::transport::{Certificate, Identity, ServerTlsConfig};
    use tonic::{Request, Response, Status};

    fn core_config(directory: &tempfile::TempDir) -> AgentConfig {
        let cache = directory.path().join("cache");
        let core = directory.path().join("core");
        let run = directory.path().join("run");
        for path in [&cache, &core, &run] {
            std::fs::create_dir(path).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        AgentConfig {
            authority_mode: AgentAuthorityMode::CoreNative,
            cache_path: cache.join("agent-cache.json"),
            core_store_path: core.join("core.db"),
            core_archive_path: core.join("node-cache.archive"),
            core_api_socket_path: run.join("core.sock"),
            node_id: "native-test-host".to_owned(),
            ..AgentConfig::default()
        }
    }

    async fn unix_http(config: &AgentConfig, request: &str) -> String {
        let mut stream = tokio::net::UnixStream::connect(&config.core_api_socket_path)
            .await
            .unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    #[tokio::test]
    async fn write_file_durable_publishes_content_and_leaves_no_temp_files() {
        // Regression pin for the R1-review MAJOR: the first version of
        // write_file_durable omitted `.write(true)`, so OpenOptions failed
        // with InvalidInput before any file was created and EVERY
        // enrollment/rotation write silently failed (the node would have
        // re-enrolled with a fresh node_id on every boot). This test drives
        // the real function so a recurrence fails CI instead of production.
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("agent.crt");

        write_file_durable(&target, b"certificate-bytes", 0o644)
            .await
            .expect("durable write must succeed");
        let persisted = tokio::fs::read(&target).await.unwrap();
        assert_eq!(persisted, b"certificate-bytes");

        // The private-key variant must land owner-only.
        let key = directory.path().join("agent.key");
        write_private_file(&key, b"key-bytes").await.unwrap();
        let mode = std::fs::metadata(&key).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        // Overwrite (rotation) replaces the previous version atomically.
        write_file_durable(&target, b"certificate-bytes-v2", 0o644)
            .await
            .unwrap();
        let persisted = tokio::fs::read(&target).await.unwrap();
        assert_eq!(persisted, b"certificate-bytes-v2");

        // No temp siblings are leaked after success.
        let mut entries = tokio::fs::read_dir(directory.path()).await.unwrap();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            let name = entry.file_name().to_string_lossy().to_string();
            assert!(!name.contains(".tmp-"), "leaked temp file: {name}");
        }
    }

    #[tokio::test]
    async fn core_native_http_create_survives_restart_and_excludes_second_instance() {
        let directory = tempfile::tempdir().unwrap();
        let config = core_config(&directory);
        let (owner, _process_adapter) = start_core_native(&config, &StordBackendInfo::default())
            .await
            .unwrap();
        assert!(start_core_native(&config, &StordBackendInfo::default())
            .await
            .is_err());
        let body = serde_json::json!({"request_id":"create-1","definition":{
            "id":"vm-1","name":"vm-1","boot":{"kernel":"/kernel","firmware":null,"initial_disk":null},
            "compute":{"vcpus":1,"memory_bytes":1048576},"storage":[],"networks":[],
            "requested_power_state":"stopped","observed_power_state":"unknown","resource_version":1
        }}).to_string();
        let response = unix_http(
            &config,
            &format!("POST /v1/vms HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nIdempotency-Key: create-vm-1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body),
        ).await;
        assert!(response.starts_with("HTTP/1.1 202"), "{response}");
        owner.shutdown().await.unwrap();

        let (owner, _process_adapter) = start_core_native(&config, &StordBackendInfo::default())
            .await
            .unwrap();
        let response = unix_http(
            &config,
            "GET /v1/vms HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(response.contains("vm-1"));
        owner.shutdown().await.unwrap();
        assert!(!config.cache_path.exists());
    }

    #[tokio::test]
    async fn core_native_concurrent_identical_creates_commit_once() {
        let directory = tempfile::tempdir().unwrap();
        let config = core_config(&directory);
        let (owner, _process_adapter) = start_core_native(&config, &StordBackendInfo::default())
            .await
            .unwrap();
        let body = serde_json::json!({"request_id":"create-1","definition":{
            "id":"vm-1","name":"vm-1","boot":{"kernel":"/kernel","firmware":null,"initial_disk":null},
            "compute":{"vcpus":1,"memory_bytes":1048576},"storage":[],"networks":[],
            "requested_power_state":"stopped","observed_power_state":"unknown","resource_version":1
        }}).to_string();
        let request = format!("POST /v1/vms HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nIdempotency-Key: create-vm-1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);

        async fn send(socket: &std::path::Path, request: &str) -> String {
            let mut stream = tokio::net::UnixStream::connect(socket).await.unwrap();
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).await.unwrap();
            response
        }

        // Two concurrent, byte-identical submissions race the acceptance
        // path. The store's transactional idempotency resolution must let
        // exactly one accept and turn the other into a replay of the same
        // operation: never two operations, never a double creation, never
        // a 5xx from a lost acceptance race.
        let (first, second) = tokio::join!(
            send(&config.core_api_socket_path, &request),
            send(&config.core_api_socket_path, &request)
        );
        let mut dispositions = Vec::new();
        for response in [&first, &second] {
            assert!(response.starts_with("HTTP/1.1 202"), "{response}");
            let body: serde_json::Value =
                serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap()).unwrap();
            dispositions.push(
                body["disposition"]
                    .as_str()
                    .expect("disposition in acceptance body")
                    .to_owned(),
            );
        }
        dispositions.sort();
        assert_eq!(dispositions, vec!["accepted", "replay"]);

        // Exactly one creation committed: the listing contains the VM once.
        let listing = send(
            &config.core_api_socket_path,
            "GET /v1/vms HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(listing.starts_with("HTTP/1.1 200"), "{listing}");
        let entries: serde_json::Value =
            serde_json::from_str(listing.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        let count = entries
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["id"] == "vm-1")
            .count();
        assert_eq!(count, 1, "{entries}");
        owner.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn core_native_refuses_legacy_cache_without_creating_core_state() {
        let directory = tempfile::tempdir().unwrap();
        let config = core_config(&directory);
        std::fs::write(&config.cache_path, b"{}").unwrap();
        std::fs::set_permissions(&config.cache_path, std::fs::Permissions::from_mode(0o600))
            .unwrap();
        assert!(start_core_native(&config, &StordBackendInfo::default())
            .await
            .is_err());
        assert!(!config.core_store_path.exists());
        assert!(!config.core_api_socket_path.exists());
    }

    #[tokio::test]
    async fn core_native_treats_whitespace_node_id_as_absent() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = core_config(&directory);
        config.node_id = "  \t ".to_owned();
        let (owner, _process_adapter) = start_core_native(&config, &StordBackendInfo::default())
            .await
            .unwrap();
        let response = unix_http(
            &config,
            "GET /v1/host HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200"));
        let body = response.split("\r\n\r\n").nth(1).unwrap();
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        let id = body["identity"]["id"].as_str().unwrap();
        assert!(!id.trim().is_empty());
        assert_ne!(id, config.node_id);
        owner.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn core_native_listener_start_failure_releases_runtime_lease() {
        let directory = tempfile::tempdir().unwrap();
        let config = core_config(&directory);
        std::fs::write(&config.core_api_socket_path, b"occupied").unwrap();
        assert!(start_core_native(&config, &StordBackendInfo::default())
            .await
            .is_err());
        std::fs::remove_file(&config.core_api_socket_path).unwrap();
        let (owner, _process_adapter) = start_core_native(&config, &StordBackendInfo::default())
            .await
            .unwrap();
        owner.shutdown().await.unwrap();
    }

    #[test]
    fn omitted_authority_mode_is_legacy_and_does_not_select_core_paths() {
        let config = AgentConfig::default();
        assert_eq!(config.authority_mode, AgentAuthorityMode::Legacy);
    }

    #[test]
    fn metrics_bind_defaults_to_loopback_and_honors_explicit_bind() {
        // Regression pin: an agent config omitting `metrics_bind` used to bind
        // the Prometheus endpoint on 0.0.0.0:9100 (all interfaces). The
        // fallback must stay on loopback; an explicit bind is passed through
        // unchanged so packaged configs keep working verbatim.
        assert_eq!(DEFAULT_METRICS_BIND, "127.0.0.1:9100");
        assert_eq!(resolve_metrics_bind(None), "127.0.0.1:9100");
        assert_eq!(resolve_metrics_bind(Some("0.0.0.0:9901")), "0.0.0.0:9901");
        assert_eq!(
            resolve_metrics_bind(Some("127.0.0.1:9901")),
            "127.0.0.1:9901"
        );
    }

    #[tokio::test]
    async fn load_or_initialize_cache_preserves_persisted_state() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("agent-cache.json");
        let mut cache = NodeCache::new("node-1");
        cache.node_state = NodeState::Maintenance.as_str().to_string();
        cache.save(&cache_path).await.unwrap();

        let config = AgentConfig {
            cache_path,
            ..AgentConfig::default()
        };

        let loaded = load_or_initialize_cache(&config).await;
        assert_eq!(loaded.node_state, "Maintenance");
    }

    #[tokio::test]
    async fn load_or_initialize_cache_bootstraps_new_node() {
        let dir = tempfile::tempdir().unwrap();
        let config = AgentConfig {
            cache_path: dir.path().join("missing-cache.json"),
            node_id: "node-123".to_string(),
            ..AgentConfig::default()
        };

        let cache = load_or_initialize_cache(&config).await;
        assert_eq!(cache.node_id, "node-123");
        assert_eq!(cache.node_state, "Bootstrapping");
    }

    #[test]
    fn certificate_rotation_due_respects_interval() {
        let cert_file = tempfile::NamedTempFile::new().unwrap();
        let key_file = tempfile::NamedTempFile::new().unwrap();

        let mut cache = NodeCache::new("node-1");
        cache.enrollment_complete = true;
        cache.certificate_path = Some(cert_file.path().to_str().unwrap().to_string());
        cache.private_key_path = Some(key_file.path().to_str().unwrap().to_string());
        assert!(certificate_rotation_due(
            &cache,
            CERT_ROTATION_INTERVAL_SECS * 1000
        ));

        cache.last_certificate_rotation_unix_ms = Some(1_000);
        assert!(!certificate_rotation_due(
            &cache,
            1_000 + (CERT_ROTATION_INTERVAL_SECS * 1000) - 1
        ));
        assert!(certificate_rotation_due(
            &cache,
            1_000 + (CERT_ROTATION_INTERVAL_SECS * 1000)
        ));
    }

    // ---- first-enrollment bootstrap result (#709) -------------------------

    struct EnrollmentTestCa {
        params: rcgen::CertificateParams,
        key: rcgen::KeyPair,
        cert_pem: Vec<u8>,
    }

    fn enrollment_test_ca(cn: &str) -> EnrollmentTestCa {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::default();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
        ];
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, cn);
        let cert = params.self_signed(&key).unwrap();
        EnrollmentTestCa {
            params,
            key,
            cert_pem: cert.pem().into_bytes(),
        }
    }

    /// Issue a leaf and return `(cert_pem, key_pem, cert_der)`.
    fn issue_enrollment_test_leaf(
        ca: &EnrollmentTestCa,
        cn: &str,
        server: bool,
    ) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        use rcgen::string::Ia5String;

        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::default();
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, cn);
        params.is_ca = rcgen::IsCa::NoCa;
        params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![if server {
            rcgen::ExtendedKeyUsagePurpose::ServerAuth
        } else {
            rcgen::ExtendedKeyUsagePurpose::ClientAuth
        }];
        if server {
            params.subject_alt_names.push(rcgen::SanType::DnsName(
                Ia5String::try_from("localhost").unwrap(),
            ));
            params.subject_alt_names.push(rcgen::SanType::IpAddress(
                std::net::Ipv4Addr::LOCALHOST.into(),
            ));
        }
        let issuer = rcgen::Issuer::from_params(&ca.params, &ca.key);
        let cert = params.signed_by(&key, &issuer).unwrap();
        let der = cert.der().to_vec();
        (
            cert.pem().into_bytes(),
            key.serialize_pem().into_bytes(),
            der,
        )
    }

    #[derive(Debug, Clone)]
    struct RecordedBootstrapReport {
        node_id: String,
        status: String,
        message: String,
    }

    #[derive(Clone)]
    struct MockBootstrapService {
        expected_peer_der: Vec<u8>,
        reports: Arc<tokio::sync::Mutex<Vec<RecordedBootstrapReport>>>,
    }

    fn peer_leaf_der<T>(request: &Request<T>) -> Option<Vec<u8>> {
        let info = request
            .extensions()
            .get::<TlsConnectInfo<TcpConnectInfo>>()?;
        let certs = info.peer_certs()?;
        certs.first().map(|cert| cert.as_ref().to_vec())
    }

    #[tonic::async_trait]
    impl EnrollmentService for MockBootstrapService {
        async fn enroll_node(
            &self,
            _request: Request<proto::EnrollmentRequest>,
        ) -> Result<Response<proto::EnrollmentResponse>, Status> {
            Err(Status::unimplemented("not used by this test"))
        }

        async fn rotate_node_certificate(
            &self,
            _request: Request<proto::RotateNodeCertificateRequest>,
        ) -> Result<Response<proto::RotateNodeCertificateResponse>, Status> {
            Err(Status::unimplemented("not used by this test"))
        }

        async fn report_bootstrap_result(
            &self,
            request: Request<proto::ReportBootstrapResultRequest>,
        ) -> Result<Response<proto::AckResponse>, Status> {
            let Some(peer_der) = peer_leaf_der(&request) else {
                return Err(Status::unauthenticated("missing peer certificate"));
            };
            // Mirror the control plane's peer-node-id binding: only the
            // newly issued identity may report this node's bootstrap result.
            if peer_der != self.expected_peer_der {
                return Err(Status::permission_denied("peer node_id mismatch"));
            }
            let payload = request.into_inner();
            self.reports.lock().await.push(RecordedBootstrapReport {
                node_id: payload.node_id,
                status: payload.bootstrap_status,
                message: payload.message,
            });
            Ok(Response::new(proto::AckResponse {
                result: Some(proto::ResultMeta {
                    operation_id: "bootstrap-report".to_string(),
                    status: "ok".to_string(),
                    node_observed_generation: String::new(),
                    error_code: String::new(),
                    human_summary: "bootstrap reported".to_string(),
                }),
            }))
        }
    }

    async fn spawn_mock_control_plane(
        ca: &EnrollmentTestCa,
        expected_peer_der: Vec<u8>,
    ) -> (
        String,
        Arc<tokio::sync::Mutex<Vec<RecordedBootstrapReport>>>,
        oneshot::Sender<()>,
    ) {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (server_cert, server_key, _) = issue_enrollment_test_leaf(ca, "localhost", true);
        let service = MockBootstrapService {
            expected_peer_der,
            reports: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        };
        let reports = service.reports.clone();
        let tls = ServerTlsConfig::new()
            .identity(Identity::from_pem(server_cert, server_key))
            .client_ca_root(Certificate::from_pem(ca.cert_pem.clone()));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .tls_config(tls)
                .unwrap()
                .add_service(EnrollmentServiceServer::new(service))
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                    shutdown_rx.await.ok();
                })
                .await
                .unwrap();
        });
        (
            format!("https://localhost:{}", addr.port()),
            reports,
            shutdown_tx,
        )
    }

    #[tokio::test]
    async fn first_enrollment_reports_success_on_issued_identity() {
        let ca = enrollment_test_ca("test-ca");
        let (issued_cert, issued_key, issued_der) =
            issue_enrollment_test_leaf(&ca, "qual-node-1", false);
        let (endpoint, reports, shutdown) = spawn_mock_control_plane(&ca, issued_der).await;

        let dir = tempfile::tempdir().unwrap();
        let config = AgentConfig {
            control_plane_addr: endpoint.clone(),
            runtime_dir: dir.path().join("runtime"),
            cache_path: dir.path().join("agent-cache.json"),
            ..AgentConfig::default()
        };
        let issued = BootstrapResponse {
            node_id: "qual-node-1".to_string(),
            certificate_pem: issued_cert,
            private_key_pem: issued_key,
            ca_pem: ca.cert_pem.clone(),
        };
        let mut cache = NodeCache::new("qual-enroll");

        let persisted = persist_enrollment_material(&config, &mut cache, &issued).await;
        assert!(persisted, "issued material must persist");
        report_enrollment_result(&config, &cache, &issued, persisted).await;

        let reports = reports.lock().await;
        assert_eq!(reports.len(), 1, "exactly one successful report");
        assert_eq!(reports[0].node_id, "qual-node-1");
        assert_eq!(reports[0].status, "ok");
        assert_eq!(reports[0].message, "bootstrap complete");
        drop(reports);

        assert!(cache.enrollment_complete);
        assert_eq!(cache.node_id, "qual-node-1");
        let expected_cert = config.runtime_dir.join("agent.crt");
        let expected_key = config.runtime_dir.join("agent.key");
        let expected_ca = config.runtime_dir.join("ca.crt");
        assert_eq!(cache.certificate_path.as_deref(), expected_cert.to_str());
        assert_eq!(cache.private_key_path.as_deref(), expected_key.to_str());
        assert_eq!(cache.ca_path.as_deref(), expected_ca.to_str());
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn persist_failure_reports_failure_on_issued_identity() {
        let ca = enrollment_test_ca("test-ca");
        let (issued_cert, issued_key, issued_der) =
            issue_enrollment_test_leaf(&ca, "qual-node-1", false);
        let (endpoint, reports, shutdown) = spawn_mock_control_plane(&ca, issued_der).await;

        let dir = tempfile::tempdir().unwrap();
        let runtime_dir = dir.path().join("runtime");
        // A file where the runtime directory belongs makes every persist
        // attempt fail, simulating an unwritable runtime directory.
        std::fs::write(&runtime_dir, b"not a directory").unwrap();
        let config = AgentConfig {
            control_plane_addr: endpoint.clone(),
            runtime_dir,
            cache_path: dir.path().join("agent-cache.json"),
            ..AgentConfig::default()
        };
        let issued = BootstrapResponse {
            node_id: "qual-node-1".to_string(),
            certificate_pem: issued_cert,
            private_key_pem: issued_key,
            ca_pem: ca.cert_pem.clone(),
        };
        let mut cache = NodeCache::new("qual-enroll");

        let persisted = persist_enrollment_material(&config, &mut cache, &issued).await;
        assert!(!persisted, "persist failure must be detected");
        assert!(!cache.enrollment_complete, "failed persist must not enroll");
        report_enrollment_result(&config, &cache, &issued, persisted).await;

        let reports = reports.lock().await;
        assert_eq!(reports.len(), 1, "exactly one failure report");
        assert_eq!(reports[0].node_id, "qual-node-1");
        assert_eq!(reports[0].status, "failed");
        assert_eq!(reports[0].message, "failed to persist enrollment material");
        drop(reports);
        let _ = shutdown.send(());
    }

    /// The identity files can land while the NodeCache record cannot be
    /// written (disk full, permission change). Reporting success then would
    /// leave the control plane claiming a durable enrollment the agent forgets
    /// on restart — and the bootstrap token is single-use, so re-enrollment
    /// fails. The persist step must fail closed instead.
    #[tokio::test]
    async fn cache_save_failure_is_reported_as_persist_failure() {
        let ca = enrollment_test_ca("test-ca");
        let (issued_cert, issued_key, issued_der) =
            issue_enrollment_test_leaf(&ca, "qual-node-1", false);
        let (endpoint, reports, shutdown) = spawn_mock_control_plane(&ca, issued_der).await;

        let dir = tempfile::tempdir().unwrap();
        // A directory where the cache file belongs makes the atomic rename
        // fail after the certificate/key/CA writes have succeeded.
        let cache_path = dir.path().join("agent-cache.json");
        std::fs::create_dir(&cache_path).unwrap();
        let config = AgentConfig {
            control_plane_addr: endpoint.clone(),
            runtime_dir: dir.path().join("runtime"),
            cache_path,
            ..AgentConfig::default()
        };
        let issued = BootstrapResponse {
            node_id: "qual-node-1".to_string(),
            certificate_pem: issued_cert,
            private_key_pem: issued_key,
            ca_pem: ca.cert_pem.clone(),
        };
        let mut cache = NodeCache::new("qual-enroll");

        let persisted = persist_enrollment_material(&config, &mut cache, &issued).await;
        assert!(
            !persisted,
            "a cache-save failure must not be reported as durable enrollment"
        );
        assert!(
            !cache.enrollment_complete,
            "a cache-save failure must clear enrollment_complete"
        );
        assert!(
            config.runtime_dir.join("agent.crt").exists(),
            "the identity files may still be on disk; only the record failed"
        );
        report_enrollment_result(&config, &cache, &issued, persisted).await;

        let reports = reports.lock().await;
        assert_eq!(reports.len(), 1, "exactly one failure report");
        assert_eq!(reports[0].node_id, "qual-node-1");
        assert_eq!(reports[0].status, "failed");
        assert_eq!(reports[0].message, "failed to persist enrollment material");
        drop(reports);
        let _ = shutdown.send(());
    }
}
