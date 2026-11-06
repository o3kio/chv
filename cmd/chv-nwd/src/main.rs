use chv_config::load_nwd_config;
use chv_nwd_core::NetworkServer;
use chv_observability::init_logger;
use chv_observability::metrics_server::{
    resolve_metrics_bind, start_metrics_server, DaemonMetricsInfo,
};
use std::path::PathBuf;
use tokio::signal::unix::{signal, SignalKind};
use tracing::info;

/// Default `/metrics` listener when `metrics_bind` is unset. Loopback, never
/// `0.0.0.0`: an omitted key must not expose network-daemon telemetry on every
/// interface. Operators scraping remotely set `metrics_bind` explicitly and
/// restrict access at the network layer (issue #657).
const DEFAULT_METRICS_BIND: &str = "127.0.0.1:9102";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Capture the true process start before any work so the metrics listener
    // reports a truthful uptime / `process_start_time_seconds` (issue #657).
    let process_started = std::time::Instant::now();

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

    let config_path = std::env::args().nth(1).map(PathBuf::from);
    let config = load_nwd_config(config_path.as_deref())?;

    init_logger(&config.log_level)?;

    info!(
        "{} starting (version {}, commit {}, channel {})",
        env!("CARGO_PKG_NAME"),
        env!("CHV_VERSION"),
        env!("CHV_GIT_SHA"),
        env!("CHV_RELEASE_CHANNEL"),
    );

    // Bind the Prometheus listener before touching the network executor: a
    // bad `metrics_bind` or an occupied port aborts startup loudly instead of
    // running a daemon with no observability surface (#657).
    start_metrics_endpoint(&config, process_started).await?;

    let mut executor = chv_nwd_core::executor::LinuxExecutor::new(config.runtime_dir.clone());
    if config.fabric.enabled {
        let fabric = chv_nwd_core::fabric::NwdFabricProvider::real(&config.fabric)?;
        info!(
            state_dir = %config.fabric.state_dir.display(),
            name_prefix = %config.fabric.name_prefix,
            wireguard_port = config.fabric.wireguard_port,
            vxlan_port = config.fabric.vxlan_port,
            default_tenant_mtu = config.fabric.default_tenant_mtu,
            "stretched-L2 fabric provider enabled (ADR-021)"
        );
        executor = executor.with_fabric(fabric);
    } else {
        info!("stretched-L2 fabric provider disabled; fabric RPCs fail closed");
    }

    // rc.8 blocker B9: journal every bridge-only topology and the last
    // requested firewall/NAT policy under /var/lib/chv/nwd, then replay both
    // before serving. A documented nwd restart (or host reboot) re-materializes
    // the topology and its guard set instead of failing the next policy update
    // with `not found: topology` or leaving attached guests unfiltered.
    let server = NetworkServer::new(executor, chv_observability::Metrics::new())
        .with_topology_store(chv_nwd_core::topology_store::TopologyStore::default_dir())
        .with_policy_store(chv_nwd_core::policy_store::PolicyStore::default_dir());

    let socket_path = config.socket_path.clone();
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    tokio::select! {
        result = server.serve(&config.socket_path) => {
            result?;
        }
        _ = sigterm.recv() => {
            info!("received SIGTERM, shutting down");
        }
        _ = sigint.recv() => {
            info!("received SIGINT, shutting down");
        }
    }

    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

/// Build the process identity exposed by the `/metrics` endpoint.
fn daemon_metrics_info(process_started: std::time::Instant) -> DaemonMetricsInfo {
    DaemonMetricsInfo::new(
        env!("CARGO_PKG_NAME"),
        env!("CHV_VERSION"),
        env!("CHV_GIT_SHA"),
        env!("CHV_RELEASE_CHANNEL"),
    )
    .with_started_at(process_started)
}

/// Resolve and bind the `/metrics` listener. The bind happens before this
/// returns, so an invalid `metrics_bind` or an occupied port is a startup
/// error (fail-closed).
async fn start_metrics_endpoint(
    config: &chv_config::NwdConfig,
    process_started: std::time::Instant,
) -> Result<(), String> {
    let bind = resolve_metrics_bind(config.metrics_bind.as_deref(), DEFAULT_METRICS_BIND);
    if config.metrics_bind.is_none() {
        tracing::warn!(
            bind = %bind,
            "metrics_bind is not set; binding /metrics on loopback — set metrics_bind \
             explicitly to serve a different address"
        );
    }
    start_metrics_server(&bind, daemon_metrics_info(process_started))
        .await
        .map_err(|e| {
            format!(
                "failed to bind metrics listener on {bind}: {e}; set metrics_bind to a valid \
                 bind address (loopback by default)"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::{start_metrics_endpoint, DEFAULT_METRICS_BIND};

    #[test]
    fn metrics_bind_defaults_to_loopback() {
        assert_eq!(DEFAULT_METRICS_BIND, "127.0.0.1:9102");
        assert_eq!(
            chv_observability::metrics_server::resolve_metrics_bind(None, DEFAULT_METRICS_BIND),
            "127.0.0.1:9102"
        );
        assert_eq!(
            chv_observability::metrics_server::resolve_metrics_bind(
                Some("192.0.2.10:9102"),
                DEFAULT_METRICS_BIND
            ),
            "192.0.2.10:9102"
        );
    }

    #[tokio::test]
    async fn invalid_metrics_bind_fails_startup() {
        let config = chv_config::NwdConfig {
            metrics_bind: Some("not-a-socket-address".to_string()),
            ..Default::default()
        };
        let err = start_metrics_endpoint(&config, std::time::Instant::now())
            .await
            .expect_err("an invalid metrics_bind must abort startup");
        assert!(
            err.contains("failed to bind metrics listener"),
            "startup error must name the metrics bind failure, got: {err}"
        );
    }
}
