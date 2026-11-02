//! End-to-end agent tests against the real manager stack: the real
//! TLS listener (`chv-controlplane-service::api::tls`), the real
//! `MonitoringAgentService` over a real operational + monitoring
//! store, and this crate's real `Agent` (real reqwest client with the
//! enrolled client certificate). No VM is involved — the collectors
//! read the test host's /proc — but every wire, TLS, dedup, spool and
//! rotation behavior between guest and manager is exercised exactly
//! as production runs it.

use chv_controlplane_service::api::tls::{build_https_config, serve_tls};
use chv_controlplane_service::monitoring_agent::{
    AgentCertificateIssuer, GuestIngestionLimits, MonitoringAgentService,
};
use chv_controlplane_store::test_util::TestDb;
use chv_controlplane_store::{EventRepository, MonitoringAgentRepository};
use chv_monitor_agent::config::AgentConfig;
use chv_monitor_agent::credential::StoredCredential;
use chv_monitor_agent::{Agent, TickOutcome};
use chv_monitoring_core::model::CheckStatus;
use chv_monitoring_store::{MonitoringHealth, MonitoringStore, MonitoringStoreConfig};
use std::path::PathBuf;
use std::sync::Arc;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// A throwaway self-signed agent CA.
fn test_ca() -> (String, String) {
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::default();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "chv-e2e-agent-ca");
    let cert = params.self_signed(&key).unwrap();
    (cert.pem(), key.serialize_pem())
}

/// A self-signed TLS server certificate for 127.0.0.1.
fn test_server_cert() -> (String, String) {
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::default();
    params.subject_alt_names = vec![rcgen::SanType::IpAddress(std::net::IpAddr::V4(
        std::net::Ipv4Addr::LOCALHOST,
    ))];
    let cert = params.self_signed(&key).unwrap();
    (cert.pem(), key.serialize_pem())
}

/// The full in-process manager: operational DB, monitoring store,
/// agent CA, service, and a live TLS listener on 127.0.0.1.
struct Manager {
    _ops: TestDb,
    _monitoring_dir: tempfile::TempDir,
    store: Arc<MonitoringStore>,
    service: Arc<MonitoringAgentService>,
    agent_ca_pem: String,
    server_cert_pem: String,
    server_key_pem: String,
    vm_id: String,
    base_url: String,
    port: u16,
    server: Option<tokio::task::JoinHandle<()>>,
    /// Held for the manager's lifetime: dropping it would signal the
    /// listener's graceful shutdown.
    _shutdown_tx: tokio::sync::watch::Sender<()>,
}

impl Manager {
    async fn new() -> Self {
        let ops = TestDb::new().await;
        let monitoring_dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            MonitoringStore::connect(MonitoringStoreConfig {
                database_url: format!("sqlite://{}/monitoring.db", monitoring_dir.path().display()),
                migrations_dir: PathBuf::from(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../cmd/chv-controlplane/monitoring-migrations"
                )),
                ..MonitoringStoreConfig::default()
            })
            .await
            .expect("monitoring store connect"),
        );

        let vm_id = "vm-e2e-1".to_string();
        sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES ($1, $1)")
            .bind(&vm_id)
            .execute(&ops.pool)
            .await
            .unwrap();

        // Agent CA (signs the enrolled client cert) and the TLS
        // server certificate (trusted by the agent as its manager CA).
        let (agent_ca_pem, agent_ca_key) = test_ca();
        let issuer = Arc::new(AgentCertificateIssuer::new(&agent_ca_pem, &agent_ca_key).unwrap());
        let service = Arc::new(MonitoringAgentService::new(
            MonitoringAgentRepository::new(ops.pool.clone()),
            issuer,
            Some(store.clone()),
            MonitoringHealth::new(),
            EventRepository::new(ops.pool.clone()),
            GuestIngestionLimits::default(),
            None,
        ));

        let (server_cert_pem, server_key_pem) = test_server_cert();
        let https =
            build_https_config(&server_cert_pem, &server_key_pem, Some(&agent_ca_pem)).unwrap();

        let router = chv_controlplane_service::api::agent_routes::agent_routes(axum::Router::new())
            .layer(axum::Extension(service.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let server = tokio::spawn(async move {
            let _ = serve_tls(listener, https, router, shutdown_rx).await;
        });

        Self {
            _ops: ops,
            _monitoring_dir: monitoring_dir,
            store,
            service,
            agent_ca_pem,
            server_cert_pem,
            server_key_pem,
            vm_id,
            base_url: format!("https://127.0.0.1:{port}"),
            port,
            server: Some(server),
            _shutdown_tx: shutdown_tx,
        }
    }

    async fn spawn_listener(&mut self) -> tokio::task::JoinHandle<()> {
        let https = build_https_config(
            &self.server_cert_pem,
            &self.server_key_pem,
            Some(&self.agent_ca_pem),
        )
        .unwrap();
        let router = chv_controlplane_service::api::agent_routes::agent_routes(axum::Router::new())
            .layer(axum::Extension(self.service.clone()));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", self.port))
            .await
            .expect("rebind same port");
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        self._shutdown_tx = shutdown_tx;
        tokio::spawn(async move {
            let _ = serve_tls(listener, https, router, shutdown_rx).await;
        })
    }

    /// Simulate an outage, then bring the identical manager back on
    /// the same port (the agent's trust and credential stay valid).
    async fn restart_listener(&mut self) {
        self.stop_listener().await;
        let server = self.spawn_listener().await;
        self.server = Some(server);
    }

    async fn stop_listener(&mut self) {
        if let Some(server) = self.server.take() {
            server.abort();
            let _ = server.await;
        }
    }

    async fn query_load1_points(&self) -> usize {
        let now = now_ms() as u64;
        self.store
            .query_history(
                &chv_monitoring_core::model::TargetKind::Vm,
                &self.vm_id,
                &["vm.guest.load1".to_string()],
                None,
                now.saturating_sub(3_600_000),
                now + 3_600_000,
                1000,
                chv_monitoring_store::Resolution::Raw,
            )
            .await
            .unwrap()
            .iter()
            .map(|s| s.points.len())
            .sum()
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            server.abort();
        }
    }
}

/// The agent's on-disk world: config, claim file, state, spool.
struct AgentDir {
    _dir: tempfile::TempDir,
    config_path: PathBuf,
    claim_path: PathBuf,
    credential_path: PathBuf,
    state_dir: PathBuf,
    spool_dir: PathBuf,
}

impl AgentDir {
    fn new(manager: &Manager) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("manager-ca.pem"), &manager.server_cert_pem).unwrap();
        let ad = Self {
            _dir: dir,
            config_path: root.join("agent.toml"),
            claim_path: root.join("claim"),
            credential_path: root.join("credential.json"),
            state_dir: root.join("state"),
            spool_dir: root.join("spool"),
        };
        ad.write_config(&manager.base_url);
        ad
    }

    fn write_config(&self, server_url: &str) {
        let toml = format!(
            r#"
server_url = "{server_url}"
manager_ca_path = "{ca}"
claim_path = "{claim}"
credential_path = "{cred}"
state_dir = "{state}"
spool_dir = "{spool}"
interval_seconds = 1
max_spool_batches = 100
"#,
            ca = self
                .config_path
                .parent()
                .unwrap()
                .join("manager-ca.pem")
                .display(),
            claim = self.claim_path.display(),
            cred = self.credential_path.display(),
            state = self.state_dir.display(),
            spool = self.spool_dir.display(),
        );
        std::fs::write(&self.config_path, toml).unwrap();
    }

    fn start(&self) -> Agent {
        let config = AgentConfig::load(&self.config_path).unwrap();
        Agent::start(config).unwrap()
    }

    async fn place_claim(&self, manager: &Manager) {
        let claim = manager
            .service
            .issue_claim(&manager.vm_id, "e2e-op", now_ms())
            .await
            .unwrap();
        std::fs::write(&self.claim_path, &claim.token).unwrap();
    }

    fn credential(&self) -> StoredCredential {
        StoredCredential::load(&self.credential_path)
            .unwrap()
            .unwrap()
    }
}

fn init_test_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("chv_monitor_agent=debug")
        .with_test_writer()
        .try_init();
}

#[tokio::test]
async fn enroll_collect_deliver_restart_and_query() {
    init_test_logging();
    let manager = Manager::new().await;
    let ad = AgentDir::new(&manager);
    ad.place_claim(&manager).await;

    let mut agent = ad.start();
    assert_eq!(agent.tick().await, TickOutcome::Enrolled);
    // The claim is consumed: gone from disk.
    assert!(!ad.claim_path.exists());
    let cred = ad.credential();
    assert_eq!(cred.vm_id, manager.vm_id);
    assert_eq!(cred.credential_epoch, 1);

    // First delivery: the manager accepts, the store can serve it.
    assert!(matches!(agent.tick().await, TickOutcome::Delivered { .. }));
    assert_eq!(agent.spool_len(), 0);
    assert!(
        manager.query_load1_points().await > 0,
        "guest sample must be queryable through the manager store"
    );

    // Restart (same disk state): the credential survives, delivery
    // continues with a fresh, non-reused sequence.
    let mut agent = ad.start();
    assert!(matches!(agent.tick().await, TickOutcome::Delivered { .. }));
}

#[tokio::test]
async fn outage_spools_then_replays_in_order() {
    init_test_logging();
    let mut manager = Manager::new().await;
    let ad = AgentDir::new(&manager);
    ad.place_claim(&manager).await;

    let mut agent = ad.start();
    agent.tick().await; // enroll
    agent.tick().await; // deliver one batch
    assert_eq!(agent.spool_len(), 0);

    // Outage: nothing lost, everything spooled.
    manager.stop_listener().await;
    assert!(matches!(agent.tick().await, TickOutcome::Spooled));
    assert!(matches!(agent.tick().await, TickOutcome::Spooled));
    assert_eq!(agent.spool_len(), 2);

    // Recovery: the spool drains oldest-first through the real TLS
    // path and every batch reaches the durable store (the query API
    // folds sub-second samples into one gauge bucket, so durability
    // is asserted on the batch high-water mark, not point count).
    manager.restart_listener().await;
    assert!(matches!(agent.tick().await, TickOutcome::Delivered { .. }));
    assert_eq!(agent.spool_len(), 0, "spool must drain fully on recovery");
    let agent_id = ad.credential().agent_id;
    let row = manager
        .service
        .repo()
        .find_agent(&agent_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.last_sequence,
        Some(4),
        "all four batches (1 live + 2 spooled + 1 current) must be durable"
    );
    assert!(manager.query_load1_points().await >= 1);
}

#[tokio::test]
async fn renewal_due_drives_rotation() {
    init_test_logging();
    let manager = Manager::new().await;
    let ad = AgentDir::new(&manager);
    ad.place_claim(&manager).await;

    let mut agent = ad.start();
    agent.tick().await; // enroll
    agent.tick().await; // deliver (active)
    let agent_id = ad.credential().agent_id;

    // Operator-forced rotation: the next ingest response carries
    // renewal_due and the agent rotates its credential.
    manager
        .service
        .repo()
        .set_rotation_pending(&agent_id)
        .await
        .unwrap();
    assert!(matches!(agent.tick().await, TickOutcome::Delivered { .. }));
    assert_eq!(ad.credential().credential_epoch, 2, "must rotate");

    // The rotated credential keeps delivering.
    assert!(matches!(agent.tick().await, TickOutcome::Delivered { .. }));
}

/// G4 end-to-end on this host (no VM needed): the collector families,
/// the systemd service checks with REAL discovery, and declarative
/// local http/tcp checks all ride one envelope through the real TLS
/// ingest path into the store — checks recorded, samples queryable.
/// This is the regression net for the G4 real-VM lesson: real systemd
/// hosts run INSTANCE units (`user@1000.service`, …) whose `@` must
/// be valid in check identifiers, or every batch carrying a
/// discovered instance unit is rejected whole and the store starves.
#[tokio::test]
async fn g4_families_checks_and_discovery_flow_end_to_end() {
    init_test_logging();
    let manager = Manager::new().await;

    // Local endpoints for the declarative checks: one tiny HTTP 200
    // responder and one bare TCP acceptor.
    let http_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_port = http_listener.local_addr().unwrap().port();
    let tcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tcp_port = tcp_listener.local_addr().unwrap().port();
    let http_server = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = http_listener.accept().await else {
                return;
            };
            // Drain the request before answering. Closing a socket with
            // unread input makes the kernel send RST, which can discard
            // the response before the client reads it.
            let mut request = [0u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut request).await;
            let _ = tokio::io::AsyncWriteExt::write_all(
                &mut sock,
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            )
            .await;
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut sock).await;
        }
    });
    let tcp_server = tokio::spawn(async move {
        loop {
            let Ok((_sock, _)) = tcp_listener.accept().await else {
                return;
            };
        }
    });

    let ad = AgentDir::new(&manager);
    // The G4 config shape (the rig's, minus VM-specific fixtures):
    // every family on, selectors, discovery, one http + one tcp check.
    let base = std::fs::read_to_string(&ad.config_path).unwrap();
    std::fs::write(
        &ad.config_path,
        format!(
            r#"{base}
[collectors]
filesystems = true
network = true
services = true
processes = true
process_selectors = ["systemd"]

[services]
configured = ["ssh.service"]
discover = true

[[checks.http]]
label = "local"
url = "http://127.0.0.1:{http_port}/"

[[checks.tcp]]
label = "local"
host = "127.0.0.1"
port = {tcp_port}
"#
        ),
    )
    .unwrap();
    ad.place_claim(&manager).await;

    let mut agent = ad.start();
    assert_eq!(agent.tick().await, TickOutcome::Enrolled);
    // First delivery carries fs/net/process samples, the service
    // checks (configured + REAL discovered units of this host) and
    // the local http/tcp check records.
    let outcome = agent.tick().await;
    assert!(
        matches!(outcome, TickOutcome::Delivered { .. }),
        "the G4 envelope must be accepted whole: {outcome:?}"
    );
    // Counters (rx/tx bytes, errors, drops) only become valid points
    // as same-epoch deltas across bucket boundaries — a second
    // delivery, spaced like a production cadence tick, gives each
    // series observations in distinct buckets (the query below uses
    // the rig's 1000-point raw shape: 3.6 s buckets over an hour).
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    assert!(matches!(agent.tick().await, TickOutcome::Delivered { .. }));
    assert_eq!(agent.spool_len(), 0, "nothing may be left spooled");

    let vm_id = manager.vm_id.clone();
    let store = manager.store.clone();
    let points_for = |metric: &'static str| {
        let store = store.clone();
        let vm_id = vm_id.clone();
        async move {
            let now = now_ms() as u64;
            // A tight window keeps the raw bucketing (range/max_points)
            // fine-grained enough that cadence-spaced counter points
            // land in distinct buckets — the same property the rig's
            // long evidence timeline provides naturally.
            store
                .query_history(
                    &chv_monitoring_core::model::TargetKind::Vm,
                    &vm_id,
                    &[metric.to_string()],
                    None,
                    now.saturating_sub(60_000),
                    now,
                    1000,
                    chv_monitoring_store::Resolution::Raw,
                )
                .await
                .unwrap()
                .iter()
                .map(|s| {
                    s.points
                        .iter()
                        .filter(|p| p.quality == chv_monitoring_core::model::SampleQuality::Valid)
                        .count()
                })
                .sum::<usize>()
        }
    };
    for metric in [
        "vm.guest.fs.available_bytes",
        "vm.guest.net.rx_bytes_total",
        "vm.guest.process.count",
        "vm.guest.service.up",
        "check.status",
        "check.duration_seconds",
    ] {
        assert!(
            points_for(metric).await > 0,
            "valid {metric} points must be queryable through the store"
        );
    }

    // The check inventory: the configured unit, the local checks, and
    // whatever this host's discovery found — all accepted (the @ fix).
    let checks = manager
        .store
        .query_checks(
            &chv_monitoring_core::model::TargetKind::Vm,
            &manager.vm_id,
            now_ms() as u64,
        )
        .await
        .unwrap();
    assert!(
        checks.iter().any(|c| c.check_id == "service:ssh.service"),
        "the configured service check must be recorded: {checks:?}"
    );
    assert!(
        checks
            .iter()
            .any(|c| c.check_id == "http:local" && c.status == CheckStatus::Ok),
        "the local http check must be recorded ok: {checks:?}"
    );
    assert!(
        checks
            .iter()
            .any(|c| c.check_id == "tcp:local" && c.status == CheckStatus::Ok),
        "the local tcp check must be recorded ok: {checks:?}"
    );
    let instance_units = checks.iter().filter(|c| c.check_id.contains('@')).count();
    if instance_units > 0 {
        // This host runs instance units (a normal systemd host does):
        // they must have been accepted, not rejected whole-batch —
        // the presence of any delivered sample above already proves
        // it; this records their inventory explicitly.
        assert!(
            checks
                .iter()
                .filter(|c| c.check_id.contains('@'))
                .all(|c| c.check_id.starts_with("service:")),
            "instance-unit check ids stay in the service namespace"
        );
    }

    http_server.abort();
    tcp_server.abort();
}

#[tokio::test]
async fn revoke_blocks_and_fresh_claim_re_enrolls() {
    init_test_logging();
    let manager = Manager::new().await;
    let ad = AgentDir::new(&manager);
    ad.place_claim(&manager).await;

    let mut agent = ad.start();
    agent.tick().await; // enroll
    agent.tick().await; // deliver
    let agent_id = ad.credential().agent_id;

    // Revoke: the manager refuses the credential.
    manager
        .service
        .repo()
        .revoke_agent(&agent_id, "e2e-op", now_ms())
        .await
        .unwrap();
    assert_eq!(agent.tick().await, TickOutcome::Unauthorized);

    // Operator re-enrollment: a fresh claim on disk unlocks the agent
    // with a brand-new credential identity.
    ad.place_claim(&manager).await;
    assert_eq!(agent.tick().await, TickOutcome::Enrolled);
    let new_cred = ad.credential();
    assert_eq!(new_cred.credential_epoch, 1);
    assert_ne!(new_cred.agent_id, agent_id);

    assert!(matches!(agent.tick().await, TickOutcome::Delivered { .. }));
}
