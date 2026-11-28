//! G4 gate evidence (part 2): native alerting and signed webhook
//! notifications on a REAL VM.
//!
//! Env-gated exactly like `g4_real_vm.rs` (CI has no KVM; the
//! real-host record lives in
//! `docs/evidence/native-monitoring/g4b/README.md`):
//!
//! ```sh
//! CHV_G1_VMM_BINARY=/tmp/opencode/g0b/cloud-hypervisor \
//! CHV_G1_FIRMWARE=/var/lib/chv/qual/hypervisor-fw \
//! CHV_G1_IMAGE=/var/lib/chv/qual/images/noble-qual-patched.img \
//! CHV_G4_AGENT_DEB=dist/packages/chv-monitor-agent_<ver>_amd64.deb \
//! cargo test -p chv-monitor-agent --test g4b_alerting -- --nocapture
//! ```
//!
//! What this proves, end to end on the production paths (real VMM,
//! real firmware boot, real Ubuntu noble guest, real package install,
//! real systemd service stop/start, real mTLS ingestion, the REAL
//! evaluator and dispatcher workers from `cmd/chv-controlplane`'s
//! bootstrap wiring):
//!
//! 1. **A real guest outage becomes a real incident.** Stopping the
//!    guest's `g4-http.service` makes the agent's `http:app` check go
//!    critical; the evaluator — running the same
//!    `AlertEvaluatorWorker` the control plane spawns — holds the
//!    condition for the rule's `for_seconds`, promotes the incident
//!    to firing, and enqueues the notification on the transition's
//!    own transaction.
//! 2. **The signed webhook is real and correctly signed.** A local
//!    HTTPS receiver (its self-signed certificate trusted through the
//!    dispatcher's `webhook_ca_path` mechanism) receives the firing
//!    envelope; the test verifies the `x-chv-signature: v1=<hex>`
//!    header against an INDEPENDENT HMAC-SHA256 computation over the
//!    raw body, and checks the envelope's event type, severity and
//!    target identity.
//! 3. **The delivery audit is honest.** The outbox row for the firing
//!    event is marked delivered with its attempt count.
//! 4. **A real recovery resolves the incident and notifies.**
//!    Restarting the service makes the check healthy again; after the
//!    rule's recovery window the incident resolves and a second —
//!    equally signed — `resolved` webhook arrives.
//!
//! What this deliberately does NOT claim: notification destinations
//! beyond the rig's webhook (Slack), retry/backoff timing under real
//! outages (unit-tested in the dispatcher), and vsock transport
//! (PR-7).

use chv_controlplane_service::alert_evaluator::{
    dedup_key, AlertEvaluatorWorker, EvaluatorChannels,
};
use chv_controlplane_service::api::tls::{build_https_config, serve_tls};
use chv_controlplane_service::monitoring_agent::{
    AgentCertificateIssuer, GuestIngestionLimits, MonitoringAgentService,
};
use chv_controlplane_service::notification_dispatcher::{
    DispatcherSettings, NotificationDispatcher,
};
use chv_controlplane_store::test_util::TestDb;
use chv_controlplane_store::{
    AlertRepository, AlertRuleRepository, AlertRuleSpec, CheckStatusMatch, EventRepository,
    MissingDataPolicy, MonitoringAgentRepository, MonitoringAgentRow, NotificationOutboxRepository,
    RuleCreateInput,
};
use chv_monitoring_core::model::{CheckStatus, TargetKind};
use chv_monitoring_store::{MonitoringHealth, MonitoringStore, MonitoringStoreConfig, StoredCheck};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Short interface names (IFNAMSIZ limit); unique to this rig so a
/// crashed run is visible and cleanable. Distinct subnet from the
/// g3/g4 rigs so several evidence runs can coexist on one host.
const BRIDGE: &str = "br-g4al";
const TAP: &str = "tap-g4al";
const BRIDGE_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 64, 1);
const GUEST_IP: &str = "192.168.64.50";
const GUEST_MAC: &str = "52:54:00:64:00:50";
const VM_ID: &str = "g4b-vm";

/// The guest's loopback HTTP endpoint (the outage trigger target).
const GUEST_HTTP_PORT: u16 = 8080;
/// The agent's declarative check id for that endpoint
/// (`http:<label>`, see the agent's checks module).
const APP_CHECK_ID: &str = "http:app";

/// The rig's webhook signing secret (>= 16 bytes, as boot validation
/// demands). Only the dispatcher and this test's independent HMAC
/// verification know it.
const SIGNING_SECRET: &str = "g4b-rig-signing-secret-0123456789abcdef";

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn checkpoint(msg: &str) {
    let _ = std::io::Write::write_fmt(
        &mut std::io::stderr(),
        format_args!("g4b checkpoint: {msg}\n"),
    );
}

fn run_ip(args: &[&str], fatal: bool) -> bool {
    let ok = std::process::Command::new("ip")
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if fatal && !ok {
        panic!("ip {:?} failed", args);
    }
    ok
}

fn setup_network() {
    run_ip(&["link", "del", TAP], false);
    run_ip(&["link", "del", BRIDGE], false);
    run_ip(&["link", "add", BRIDGE, "type", "bridge"], true);
    run_ip(&["addr", "add", "192.168.64.1/24", "dev", BRIDGE], true);
    run_ip(&["link", "set", BRIDGE, "up"], true);
    run_ip(&["tuntap", "add", "dev", TAP, "mode", "tap"], true);
    run_ip(&["link", "set", TAP, "master", BRIDGE], true);
    run_ip(&["link", "set", TAP, "up"], true);
}

fn teardown_network() {
    run_ip(&["link", "del", TAP], false);
    run_ip(&["link", "del", BRIDGE], false);
}

/// Synchronous best-effort cleanup for host resources a failed run
/// must not leak (the VMM via SIGKILL, the network topology). Owns
/// the VM's tempdir so diagnostics survive unwinding.
struct HostCleanup {
    _dir: Option<tempfile::TempDir>,
    vm_dir: Option<PathBuf>,
}

impl HostCleanup {
    fn new() -> Self {
        Self {
            _dir: None,
            vm_dir: None,
        }
    }

    fn keep_dir(&mut self, dir: tempfile::TempDir) {
        self._dir = Some(dir);
    }
}

impl Drop for HostCleanup {
    fn drop(&mut self) {
        if let Some(vm_dir) = self.vm_dir.take() {
            if let Ok(pid) = std::fs::read_to_string(vm_dir.join("ch.pid")) {
                let pid = pid.trim();
                if !pid.is_empty() {
                    let _ = std::process::Command::new("kill")
                        .arg("-9")
                        .arg(pid)
                        .output();
                }
            }
            dump_diagnostics(&vm_dir);
        }
        teardown_network();
    }
}

/// Last ~40 lines of the guest console (progress visibility during
/// the enrollment wait).
fn dump_console_tail(vm_dir: &Path) {
    match std::fs::read_to_string(vm_dir.join("console.log")) {
        Ok(text) => {
            let lines: Vec<&str> = text.lines().collect();
            let start = lines.len().saturating_sub(40);
            let _ = std::io::Write::write_fmt(
                &mut std::io::stderr(),
                format_args!("{}\n", lines[start..].join("\n")),
            );
        }
        Err(e) => {
            let _ = std::io::Write::write_fmt(
                &mut std::io::stderr(),
                format_args!("g4b console.log: unreadable ({e})\n"),
            );
        }
    }
}

fn dump_diagnostics(vm_dir: &Path) {
    for (label, name) in [
        ("console.log", "console.log"),
        ("vmm stderr", "cloud-hypervisor.stderr.log"),
    ] {
        match std::fs::read_to_string(vm_dir.join(name)) {
            Ok(text) => {
                let lines: Vec<&str> = text.lines().collect();
                let start = lines.len().saturating_sub(120);
                let _ = std::io::Write::write_fmt(
                    &mut std::io::stderr(),
                    format_args!("g4b {label} (tail):\n{}\n", lines[start..].join("\n")),
                );
            }
            Err(e) => {
                let _ = std::io::Write::write_fmt(
                    &mut std::io::stderr(),
                    format_args!("g4b {label}: unreadable ({e})\n"),
                );
            }
        }
    }
}

/// A self-signed TLS server certificate for the bridge IP (used for
/// BOTH the manager ingest listener and the webhook receiver; each
/// gets its own instance).
fn bridge_server_cert() -> (String, String) {
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::default();
    params.subject_alt_names = vec![rcgen::SanType::IpAddress(std::net::IpAddr::V4(BRIDGE_IP))];
    let cert = params.self_signed(&key).unwrap();
    (cert.pem(), key.serialize_pem())
}

/// A throwaway self-signed agent CA (signs the enrolled client cert).
fn test_ca() -> (String, String) {
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::default();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "g4b-evidence-agent-ca");
    let cert = params.self_signed(&key).unwrap();
    (cert.pem(), key.serialize_pem())
}

// ---------------------------------------------------------------------------
// Webhook receiver
// ---------------------------------------------------------------------------

/// One received delivery: the raw body and the signature header,
/// exactly as they arrived on the wire.
#[derive(Clone)]
struct ReceivedWebhook {
    body: String,
    signature: Option<String>,
    content_type: String,
}

/// A local HTTPS receiver standing in for the operator's webhook
/// destination. Its self-signed certificate is trusted by the
/// dispatcher through the same `webhook_ca_path` mechanism production
/// uses for internal receivers — this rig exercises that path too.
struct WebhookReceiver {
    received: Arc<std::sync::Mutex<Vec<ReceivedWebhook>>>,
    url: String,
    /// The receiver's own certificate — what an operator would put in
    /// `webhook_ca_path` for a private-CA internal receiver. The
    /// dispatcher must trust THIS certificate, not a fresh one.
    cert_pem: String,
    server: Option<tokio::task::JoinHandle<()>>,
    /// Held for the listener's lifetime: dropping it signals graceful
    /// shutdown.
    _shutdown_tx: tokio::sync::watch::Sender<()>,
}

impl WebhookReceiver {
    async fn start() -> Self {
        let (cert_pem, key_pem) = bridge_server_cert();
        let https = build_https_config(&cert_pem, &key_pem, None).unwrap();
        let received = Arc::new(std::sync::Mutex::new(Vec::new()));

        async fn handle_hook(
            axum::extract::State(received): axum::extract::State<
                Arc<std::sync::Mutex<Vec<ReceivedWebhook>>>,
            >,
            headers: axum::http::HeaderMap,
            body: axum::body::Bytes,
        ) -> axum::response::Response {
            let webhook = ReceivedWebhook {
                body: String::from_utf8_lossy(&body).into_owned(),
                signature: headers
                    .get("x-chv-signature")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string),
                content_type: headers
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string(),
            };
            received.lock().unwrap().push(webhook);
            axum::response::IntoResponse::into_response((
                axum::http::StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                "{}",
            ))
        }

        let router = axum::Router::new()
            .route("/hook", axum::routing::post(handle_hook))
            .with_state(received.clone());
        let listener = tokio::net::TcpListener::bind((BRIDGE_IP, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let server = tokio::spawn(async move {
            let _ = serve_tls(listener, https, router, shutdown_rx).await;
        });
        Self {
            received,
            url: format!("https://{BRIDGE_IP}:{port}/hook"),
            cert_pem,
            server: Some(server),
            _shutdown_tx: shutdown_tx,
        }
    }

    fn events(&self, event_type: &str) -> Vec<ReceivedWebhook> {
        self.received
            .lock()
            .unwrap()
            .iter()
            .filter(|w| {
                serde_json::from_str::<serde_json::Value>(&w.body)
                    .map(|v| v.get("event_type").and_then(|t| t.as_str()) == Some(event_type))
                    .unwrap_or(false)
            })
            .cloned()
            .collect()
    }
}

impl Drop for WebhookReceiver {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            server.abort();
        }
    }
}

/// The expected `x-chv-signature` value for a body: an INDEPENDENT
/// HMAC-SHA256 computation (the dispatcher's signer and this check
/// share only the secret and RFC 2104).
fn expected_signature(body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(SIGNING_SECRET.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    let digest = mac.finalize().into_bytes();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("v1={hex}")
}

// ---------------------------------------------------------------------------
// The rig's control plane
// ---------------------------------------------------------------------------

/// The in-process manager: operational DB (with the PR-6 alerting
/// tables), monitoring store, agent CA, ingest service and TLS
/// listener — plus the REAL evaluator and dispatcher workers the
/// control-plane bootstrap spawns, pointed at the rig's webhook
/// receiver.
struct Rig {
    _ops: TestDb,
    _monitoring_dir: tempfile::TempDir,
    store: Arc<MonitoringStore>,
    repo: MonitoringAgentRepository,
    service: Arc<MonitoringAgentService>,
    server_cert_pem: String,
    base_url: String,
    rules: AlertRuleRepository,
    alerts: AlertRepository,
    outbox: NotificationOutboxRepository,
    receiver: WebhookReceiver,
    ingest_server: Option<tokio::task::JoinHandle<()>>,
    /// Held for the listener's lifetime: dropping it would signal the
    /// ingest listener's graceful shutdown.
    _ingest_shutdown_tx: tokio::sync::watch::Sender<()>,
    /// Worker lifetime: dropping it signals both workers' shutdown.
    _workers_shutdown_tx: tokio::sync::watch::Sender<()>,
}

impl Rig {
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

        sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES ($1, $1)")
            .bind(VM_ID)
            .execute(&ops.pool)
            .await
            .unwrap();

        let (agent_ca_pem, agent_ca_key) = test_ca();
        let issuer = Arc::new(AgentCertificateIssuer::new(&agent_ca_pem, &agent_ca_key).unwrap());
        let repo = MonitoringAgentRepository::new(ops.pool.clone());
        let limits = GuestIngestionLimits {
            claim_ttl_ms: 30 * 60_000,
            ..GuestIngestionLimits::default()
        };
        let service = Arc::new(MonitoringAgentService::new(
            repo.clone(),
            issuer,
            Some(store.clone()),
            MonitoringHealth::new(),
            EventRepository::new(ops.pool.clone()),
            limits,
            None,
        ));

        let (server_cert_pem, server_key_pem) = bridge_server_cert();
        let https =
            build_https_config(&server_cert_pem, &server_key_pem, Some(&agent_ca_pem)).unwrap();

        let router = chv_controlplane_service::api::agent_routes::agent_routes(axum::Router::new())
            .layer(axum::Extension(service.clone()));
        let listener = tokio::net::TcpListener::bind((BRIDGE_IP, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (ingest_shutdown_tx, ingest_shutdown_rx) = tokio::sync::watch::channel(());
        let ingest_server = tokio::spawn(async move {
            let _ = serve_tls(listener, https, router, ingest_shutdown_rx).await;
        });

        // The webhook receiver (the operator's destination stand-in).
        let receiver = WebhookReceiver::start().await;

        // The REAL alerting workers, wired exactly like the bootstrap:
        // evaluator over the operational rule/alert repositories and
        // the monitoring store; dispatcher over the outbox with the
        // receiver as the webhook destination, its self-signed cert
        // trusted via the ca_pem mechanism (webhook_ca_path).
        let rules = AlertRuleRepository::new(ops.pool.clone());
        let alerts = AlertRepository::new(ops.pool.clone());
        let outbox = NotificationOutboxRepository::new(ops.pool.clone());

        let evaluator = AlertEvaluatorWorker::new(
            rules.clone(),
            alerts.clone(),
            store.clone(),
            EvaluatorChannels {
                webhook: true,
                slack: false,
            },
        );
        let (workers_shutdown_tx, workers_shutdown_rx) = tokio::sync::watch::channel(());
        let evaluator_shutdown = workers_shutdown_rx.clone();
        tokio::spawn(async move {
            evaluator
                .run(Duration::from_secs(5), evaluator_shutdown)
                .await;
        });

        let dispatcher = NotificationDispatcher::new(
            outbox.clone(),
            EventRepository::new(ops.pool.clone()),
            DispatcherSettings {
                webhook_url: Some(receiver.url.clone()),
                signing_secret: SIGNING_SECRET.to_string(),
                slack_webhook_url: None,
                max_attempts: 8,
                max_batch: 10,
            },
            // The receiver's own certificate, exactly what a
            // production operator puts in webhook_ca_path for an
            // internal receiver with a private CA.
            Some(&receiver.cert_pem),
        )
        .expect("build the notification dispatcher");
        let dispatcher_shutdown = workers_shutdown_rx;
        tokio::spawn(async move {
            dispatcher
                .run(Duration::from_secs(2), dispatcher_shutdown)
                .await;
        });

        let base_url = format!("https://{BRIDGE_IP}:{port}");
        checkpoint(format!("rig up: ingest on {base_url}, receiver at {}", receiver.url).as_str());

        Self {
            _ops: ops,
            _monitoring_dir: monitoring_dir,
            store,
            repo,
            service,
            server_cert_pem,
            base_url,
            rules,
            alerts,
            outbox,
            receiver,
            ingest_server: Some(ingest_server),
            _ingest_shutdown_tx: ingest_shutdown_tx,
            _workers_shutdown_tx: workers_shutdown_tx,
        }
    }

    fn base_url(&self) -> String {
        self.base_url.clone()
    }

    async fn active_agent(&self) -> Option<MonitoringAgentRow> {
        self.repo.find_active_agent_by_vm(VM_ID).await.unwrap()
    }

    async fn find_check(&self, check_id: &str) -> Option<StoredCheck> {
        self.store
            .query_checks(&TargetKind::Vm, VM_ID, now_ms().unsigned_abs())
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.check_id == check_id)
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        if let Some(server) = self.ingest_server.take() {
            server.abort();
        }
    }
}

/// The guest agent's g4b config: enrollment plus the single local
/// HTTP check that drives the whole scenario. 5 s interval; the check
/// family rides its 60 s cadence (see the agent's checks module).
fn guest_agent_toml(manager_url: &str) -> String {
    format!(
        r#"server_url = "{manager_url}"
manager_ca_path = "/etc/chv-monitor/manager-ca.pem"
claim_path = "/var/lib/chv-monitor/claim"
credential_path = "/var/lib/chv-monitor/credential.json"
state_dir = "/var/lib/chv-monitor"
spool_dir = "/var/lib/chv-monitor/spool"
interval_seconds = 5
max_spool_batches = 500
log_level = "info"

[[checks.http]]
label = "app"
url = "http://127.0.0.1:{port}/"
"#,
        port = GUEST_HTTP_PORT
    )
}

fn guest_userdata(ssh_pubkey: &str) -> String {
    format!(
        r#"#cloud-config
ssh_authorized_keys:
  - {ssh_pubkey}
write_files:
  - path: /etc/systemd/system/g4-http.service
    content: |
      [Unit]
      Description=G4b evidence loopback HTTP endpoint
      After=network.target
      [Service]
      ExecStart=/usr/bin/python3 -m http.server {port} --bind 127.0.0.1
      Restart=always
      [Install]
      WantedBy=multi-user.target
runcmd:
  - mkdir -p /media/cidata
  - mount -o ro "$(findfs LABEL=cidata)" /media/cidata
  - dpkg -i /media/cidata/chv-monitor-agent.deb
  - install -m 0644 /media/cidata/agent.toml /etc/chv-monitor/agent.toml
  - install -m 0644 /media/cidata/manager-ca.pem /etc/chv-monitor/manager-ca.pem
  - install -o chv-monitor -g chv-monitor -m 0600 /media/cidata/claim /var/lib/chv-monitor/claim
  - systemctl enable --now g4-http.service
  - systemctl enable --now chv-monitor-agent
"#,
        port = GUEST_HTTP_PORT
    )
}

/// Enrich the production-built NoCloud seed with the agent package
/// and its inputs.
async fn enrich_seed(
    vm_dir: &Path,
    manager_url: &str,
    manager_ca_pem: &str,
    deb: &Path,
    claim_token: &str,
) {
    let seed_dir = vm_dir.join("seed");
    std::fs::copy(deb, seed_dir.join("chv-monitor-agent.deb"))
        .expect("copy the agent deb into the seed");
    std::fs::write(seed_dir.join("agent.toml"), guest_agent_toml(manager_url))
        .expect("write the guest agent config into the seed");
    std::fs::write(seed_dir.join("manager-ca.pem"), manager_ca_pem)
        .expect("write the manager CA into the seed");
    std::fs::write(seed_dir.join("claim"), claim_token).expect("write the claim into the seed");

    let iso = vm_dir.join("seed.iso");
    let iso_new = vm_dir.join("seed.iso.new");
    let command = |binary: &str| {
        let mut command = tokio::process::Command::new(binary);
        command
            .arg("-output")
            .arg(&iso_new)
            .arg("-volid")
            .arg("cidata")
            .arg("-joliet")
            .arg("-rock")
            .arg(seed_dir.join("user-data"))
            .arg(seed_dir.join("meta-data"))
            .arg(seed_dir.join("network-config"))
            .arg(seed_dir.join("chv-monitor-agent.deb"))
            .arg(seed_dir.join("agent.toml"))
            .arg(seed_dir.join("manager-ca.pem"))
            .arg(seed_dir.join("claim"));
        command
    };
    let output = match command("genisoimage").output().await {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => command("/usr/bin/genisoimage")
            .output()
            .await
            .expect("run genisoimage for the enriched seed"),
        Err(e) => panic!("failed to run genisoimage: {e}"),
    };
    assert!(
        output.status.success(),
        "genisoimage failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::rename(&iso_new, &iso).expect("atomically replace the seed ISO");
    checkpoint("seed enriched with the agent package and claim");
}

/// Run a command in the guest over ssh (the scenario trigger — the
/// real systemctl stop/start of the endpoint unit). The noble cloud
/// image's default user has passwordless sudo.
fn ssh(key_path: &Path, command: &str) -> String {
    let out = std::process::Command::new("ssh")
        .arg("-i")
        .arg(key_path)
        .arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg("-o")
        .arg("UserKnownHostsFile=/dev/null")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg(format!("ubuntu@{GUEST_IP}"))
        .arg("--")
        .arg(command)
        .output()
        .unwrap_or_else(|e| panic!("ssh to the guest failed: {e}"));
    assert!(
        out.status.success(),
        "guest command {command:?} failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A fresh ed25519 keypair for the rig's guest access.
fn rig_ssh_key(dir: &Path) -> (PathBuf, String) {
    let key = dir.join("rig-ed25519");
    let st = std::process::Command::new("ssh-keygen")
        .arg("-t")
        .arg("ed25519")
        .arg("-N")
        .arg("")
        .arg("-f")
        .arg(&key)
        .output()
        .expect("run ssh-keygen for the rig key");
    assert!(
        st.status.success(),
        "ssh-keygen failed: {}",
        String::from_utf8_lossy(&st.stderr)
    );
    let pubkey = std::fs::read_to_string(key.with_extension("pub")).expect("read the rig pubkey");
    (key, pubkey.trim().to_string())
}

/// Poll `probe` until it returns Some, or fail with `what` after
/// `budget`. 2 s poll interval — the guest phases take tens of
/// seconds.
async fn wait_until<T, F>(what: &str, budget: Duration, mut probe: impl FnMut() -> F) -> T
where
    F: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + budget;
    loop {
        if let Some(value) = probe().await {
            return value;
        }
        if Instant::now() >= deadline {
            panic!("g4b: timed out waiting for {what} (budget {budget:?})");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn is_root_with_ip() -> bool {
    let has_ip = std::process::Command::new("ip")
        .arg("link")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let is_root = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .map(|uid| uid == "0")
        })
        .unwrap_or(false);
    has_ip && is_root
}

#[tokio::test]
async fn g4b_real_vm_alerting_and_signed_notifications() {
    let Ok(vmm_binary) = std::env::var("CHV_G1_VMM_BINARY") else {
        eprintln!("skipping: CHV_G1_VMM_BINARY not set (real-KVM evidence test)");
        return;
    };
    let firmware =
        std::env::var("CHV_G1_FIRMWARE").expect("CHV_G1_FIRMWARE with CHV_G1_VMM_BINARY");
    let image = std::env::var("CHV_G1_IMAGE").expect("CHV_G1_IMAGE with CHV_G1_VMM_BINARY");
    let Ok(agent_deb) = std::env::var("CHV_G4_AGENT_DEB") else {
        eprintln!("skipping: CHV_G4_AGENT_DEB not set (build the guest package first)");
        return;
    };
    let agent_deb = PathBuf::from(agent_deb);
    assert!(
        agent_deb.is_file(),
        "CHV_G4_AGENT_DEB={:?} is not a file",
        agent_deb
    );
    if !is_root_with_ip() {
        eprintln!("skipping: bridge setup needs root + iproute2 (test-side topology)");
        return;
    }

    let mut cleanup = HostCleanup::new();
    setup_network();
    checkpoint("network up (bridge + tap)");

    let rig = Rig::new().await;

    // The alert rule: the guest's loopback HTTP endpoint going
    // critical, held for 15 s before firing, recovered after 15 s.
    // Created BEFORE the VM boots — the evaluator simply sees the
    // condition not met until the guest reports.
    let rule = rig
        .rules
        .create(&RuleCreateInput {
            name: "g4b app endpoint down".into(),
            enabled: true,
            target_kind: "vm".into(),
            target_id: VM_ID.into(),
            spec: AlertRuleSpec::CheckStatus {
                check_id: APP_CHECK_ID.into(),
                status_match: CheckStatusMatch::Critical,
            },
            severity: "critical".into(),
            for_seconds: 15,
            recovery_seconds: 15,
            missing_data: MissingDataPolicy::Unknown,
            created_by: "g4b-rig".into(),
            now_ms: now_ms(),
        })
        .await
        .expect("create the alert rule");
    checkpoint(&format!(
        "alert rule created: {} on {} (check {} -> critical)",
        rule.rule_id, VM_ID, APP_CHECK_ID
    ));

    // The claim is issued BEFORE the VM exists: it is embedded in the
    // seed, so its TTL covers boot + cloud-init + install.
    let claim = rig
        .service
        .issue_claim(VM_ID, "g4b-evidence", now_ms())
        .await
        .expect("issue the one-time claim");

    let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let (ssh_key, ssh_pubkey) = rig_ssh_key(dir.path());
    let root_disk = dir.path().join("g4b-root.qcow2");
    let cp = std::process::Command::new("cp")
        .arg("--sparse=always")
        .arg(&image)
        .arg(&root_disk)
        .status()
        .expect("run cp for the writable root copy");
    assert!(cp.success(), "cp of the qualified image failed");
    let adapter =
        chv_agent_runtime_ch::ProcessCloudHypervisorAdapter::new(PathBuf::from(&vmm_binary));
    let runtime = chv_agent_core::vm_runtime::VmRuntime::new(Arc::new(adapter));

    let vm_dir = dir.path().join("vms").join(VM_ID);
    cleanup.vm_dir = Some(vm_dir.clone());
    cleanup.keep_dir(dir);

    let config = chv_hypervisor_api::VmConfig {
        vm_id: VM_ID.to_string(),
        cpus: 2,
        memory_bytes: 512 * 1024 * 1024,
        kernel_path: PathBuf::from("/dev/null"),
        firmware_path: Some(PathBuf::from(&firmware)),
        disks: vec![chv_hypervisor_api::VmDiskConfig {
            path: root_disk,
            read_only: false,
            id: None,
        }],
        nics: vec![chv_hypervisor_api::VmNicConfig {
            network_id: "g4b-evidence".to_string(),
            mac_address: GUEST_MAC.to_string(),
            ip_address: GUEST_IP.to_string(),
            tap_name: TAP.to_string(),
            cidr: "192.168.64.0/24".to_string(),
            gateway: BRIDGE_IP.to_string(),
        }],
        api_socket_path: vm_dir.join("vm.sock"),
        cloud_init_userdata: Some(guest_userdata(&ssh_pubkey)),
        hypervisor_overrides: None,
        vsock: None,
    };

    checkpoint("creating vm (production adapter, seed built)");
    runtime
        .create_vm(VM_ID, "g4b-1", &config, None)
        .await
        .expect("production create_vm builds the cloud-init seed");
    enrich_seed(
        &vm_dir,
        &rig.base_url(),
        &rig.server_cert_pem,
        &agent_deb,
        &claim.token,
    )
    .await;
    runtime
        .start_vm(VM_ID, None)
        .await
        .expect("production start_vm boots the enriched seed");

    // Phase 1 — enrollment (the identical production path to G3/G4).
    let mut poll_count: u32 = 0;
    let agent = wait_until(
        "the guest agent to enroll (cloud-init + dpkg + systemd + mTLS)",
        Duration::from_secs(420),
        || {
            poll_count += 1;
            if poll_count.is_multiple_of(30) {
                checkpoint(&format!(
                    "still waiting for enrollment (poll {poll_count}); guest console tail:"
                ));
                dump_console_tail(&vm_dir);
            }
            async { rig.active_agent().await }
        },
    )
    .await;
    checkpoint("agent enrolled");
    assert_eq!(agent.vm_id, VM_ID);

    wait_until(
        "ssh access to the guest",
        Duration::from_secs(120),
        || async {
            std::process::Command::new("ssh")
                .arg("-i")
                .arg(&ssh_key)
                .arg("-o")
                .arg("StrictHostKeyChecking=no")
                .arg("-o")
                .arg("UserKnownHostsFile=/dev/null")
                .arg("-o")
                .arg("ConnectTimeout=5")
                .arg(format!("ubuntu@{GUEST_IP}"))
                .arg("--")
                .arg("true")
                .output()
                .map(|o| o.status.success().then_some(()))
                .unwrap_or(None)
        },
    )
    .await;
    checkpoint("ssh reachable");

    // Phase 2 — the healthy baseline: the app check reports ok and
    // the rule does not fire (no incident, no webhook).
    let healthy = wait_until(
        "the app check to report ok",
        Duration::from_secs(300),
        || async {
            rig.find_check(APP_CHECK_ID)
                .await
                .filter(|c| c.status == CheckStatus::Ok && !c.stale)
        },
    )
    .await;
    checkpoint(&format!(
        "app check healthy (status ok, summary {:?})",
        healthy.summary
    ));
    assert!(
        rig.alerts
            .find_active_incident(&dedup_key(&rule))
            .await
            .unwrap()
            .is_none(),
        "no incident while the endpoint is healthy"
    );
    assert!(
        rig.receiver.received.lock().unwrap().is_empty(),
        "no webhook while the endpoint is healthy"
    );

    // Phase 3 — the outage: stop the real service; the real check
    // goes critical; the real evaluator fires the incident.
    ssh(&ssh_key, "sudo systemctl stop g4-http.service");
    checkpoint("g4-http.service stopped in the guest");

    let incident = wait_until(
        "the incident to fire (check critical + hold window)",
        Duration::from_secs(300),
        || async {
            rig.alerts
                .find_active_incident(&dedup_key(&rule))
                .await
                .unwrap()
                .filter(|i| i.status == "firing")
        },
    )
    .await;
    checkpoint(&format!(
        "incident {} firing (opened at {}, last observed {:?})",
        incident.alert_id, incident.opened_at, incident.last_observed
    ));
    assert_eq!(incident.severity, "critical");
    assert_eq!(incident.resource_kind.as_deref(), Some("vm"));
    assert_eq!(incident.resource_id.as_deref(), Some(VM_ID));

    // Phase 4 — the signed firing webhook arrives and verifies.
    let firing = wait_until(
        "the signed firing webhook to arrive at the receiver",
        Duration::from_secs(60),
        || async {
            let events = rig.receiver.events("firing");
            (!events.is_empty()).then_some(events)
        },
    )
    .await;
    assert_eq!(
        firing.len(),
        1,
        "exactly one firing webhook (no duplicates)"
    );
    let firing = &firing[0];
    assert_eq!(
        firing.content_type, "application/json",
        "the webhook posts JSON"
    );
    assert_eq!(
        firing.signature.as_deref(),
        Some(expected_signature(&firing.body).as_str()),
        "x-chv-signature verifies against an independent HMAC-SHA256 over the raw body"
    );
    let envelope: serde_json::Value =
        serde_json::from_str(&firing.body).expect("the firing body is the contract envelope");
    assert_eq!(envelope["event_type"], "firing");
    assert_eq!(envelope["severity"], "critical");
    assert_eq!(envelope["target_kind"], "vm");
    assert_eq!(envelope["target_id"], VM_ID);
    assert_eq!(
        envelope["incident_id"], incident.alert_id,
        "the envelope identifies the incident it is about"
    );
    assert!(envelope["event_id"].as_str().is_some_and(|e| !e.is_empty()));
    assert!(
        envelope["summary"]
            .as_str()
            .unwrap_or("")
            .contains("g4b app endpoint down"),
        "the summary names the rule: {}",
        envelope["summary"]
    );
    checkpoint("firing webhook received and signature verified");

    // The delivery audit is honest: the firing event is delivered.
    let delivered_firing = wait_until(
        "the firing outbox row to be marked delivered",
        Duration::from_secs(30),
        || async {
            rig.outbox
                .list_recent(50)
                .await
                .unwrap()
                .into_iter()
                .find(|e| e.event_type == "firing" && e.status == "delivered")
        },
    )
    .await;
    assert_eq!(delivered_firing.channel, "webhook");
    assert!(delivered_firing.attempts >= 1);
    checkpoint(&format!(
        "delivery audit: firing event {} delivered after {} attempt(s)",
        delivered_firing.event_id, delivered_firing.attempts
    ));

    // Phase 5 — the recovery: restart the real service; the check
    // recovers; the recovery window elapses; the incident resolves.
    ssh(&ssh_key, "sudo systemctl start g4-http.service");
    checkpoint("g4-http.service restarted in the guest");

    let resolved_row = wait_until(
        "the incident to resolve (check ok + recovery window)",
        Duration::from_secs(300),
        || async {
            let row = rig.alerts.get_incident(&incident.alert_id).await.unwrap();
            (row.status == "resolved").then_some(row)
        },
    )
    .await;
    assert!(resolved_row.resolved_at.is_some());
    checkpoint("incident resolved");

    let resolved_events = wait_until(
        "the signed resolved webhook to arrive at the receiver",
        Duration::from_secs(60),
        || async {
            let events = rig.receiver.events("resolved");
            (!events.is_empty()).then_some(events)
        },
    )
    .await;
    assert_eq!(resolved_events.len(), 1, "exactly one resolved webhook");
    let resolved = &resolved_events[0];
    assert_eq!(
        resolved.signature.as_deref(),
        Some(expected_signature(&resolved.body).as_str()),
        "the resolved webhook's signature verifies too"
    );
    let resolved_envelope: serde_json::Value = serde_json::from_str(&resolved.body).unwrap();
    assert_eq!(resolved_envelope["event_type"], "resolved");
    assert_eq!(
        resolved_envelope["incident_id"], envelope["incident_id"],
        "firing and resolved carry the same incident identity"
    );
    checkpoint("resolved webhook received and signature verified");

    // The outbox never accumulated retries beyond the delivered
    // events (no dead letters in a healthy scenario).
    let dead = rig
        .outbox
        .list_recent(50)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.status == "dead")
        .count();
    assert_eq!(dead, 0, "no dead-lettered events in a healthy scenario");

    // Success path cleanup: graceful VM stop + delete.
    let _ = runtime.stop_vm(VM_ID, false, None).await;
    let _ = runtime.delete_vm(VM_ID, None).await;
    cleanup.vm_dir = None;
    checkpoint("vm stopped and deleted; g4b evidence complete");
}
