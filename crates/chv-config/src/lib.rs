use rand::RngExt;
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Multi-node configuration: Overlay and eBPF
// ---------------------------------------------------------------------------

/// VXLAN overlay network configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct OverlayConfig {
    /// Disable MAC learning on VXLAN interfaces (use explicit FDB entries only).
    #[serde(default = "default_nolearning")]
    pub nolearning: bool,

    /// Enable ARP suppression on the VXLAN interface.
    #[serde(default)]
    pub arp_suppress: bool,

    /// Inner MTU for VXLAN traffic. "auto" calculates from outer MTU minus overhead.
    #[serde(default = "default_inner_mtu")]
    pub inner_mtu: String,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            nolearning: default_nolearning(),
            arp_suppress: false,
            inner_mtu: default_inner_mtu(),
        }
    }
}

fn default_nolearning() -> bool {
    true
}
fn default_inner_mtu() -> String {
    "auto".to_string()
}

/// eBPF policy engine configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct EbpfConfig {
    /// Directory containing compiled eBPF object files (.o).
    #[serde(default = "default_ebpf_program_path")]
    pub program_path: PathBuf,

    /// Default action when no rule matches: "deny" or "allow".
    #[serde(default = "default_ebpf_action")]
    pub default_action: String,
}

impl Default for EbpfConfig {
    fn default() -> Self {
        Self {
            program_path: default_ebpf_program_path(),
            default_action: default_ebpf_action(),
        }
    }
}

fn default_ebpf_program_path() -> PathBuf {
    PathBuf::from("/usr/lib/chv/ebpf/")
}
fn default_ebpf_action() -> String {
    "deny".to_string()
}

fn generate_secure_secret() -> String {
    let bytes: [u8; 32] = rand::rng().random();
    hex::encode(bytes)
}

const SHARED_SECRET_PATH: &str = "/etc/chv/jwt_secret";

/// Creates/rewrites the shared secret file with owner-only permissions from
/// creation: a plain `fs::write` followed by a chmod leaves a umask-dependent
/// window where the fresh secret is group/world-readable.
#[cfg(unix)]
fn write_secret_private(path: &str, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_bytes())
}

#[cfg(not(unix))]
fn write_secret_private(path: &str, contents: &str) -> std::io::Result<()> {
    std::fs::write(path, contents)
}

fn resolve_jwt_secret(current: &str, service_name: &str) -> String {
    if current != "chv-dev-secret-change-in-production" && current.len() >= 32 {
        return current.to_string();
    }
    // Check CHV_JWT_SECRET env var first
    if let Ok(env_secret) = std::env::var("CHV_JWT_SECRET") {
        if env_secret.len() >= 32 {
            tracing::info!("loaded jwt_secret from CHV_JWT_SECRET env var");
            return env_secret;
        }
    }
    if current == "chv-dev-secret-change-in-production" {
        tracing::error!(
            service = service_name,
            "SECURITY: jwt_secret is set to the known default value. \
             This is insecure — set a unique jwt_secret (>= 32 chars) in the {} config or CHV_JWT_SECRET env var. \
             Auto-generating a random secret for this session.",
            service_name
        );
    }
    if let Ok(secret) = std::fs::read_to_string(SHARED_SECRET_PATH) {
        let secret = secret.trim().to_string();
        if secret.len() >= 32 {
            tracing::info!("loaded jwt_secret from {}", SHARED_SECRET_PATH);
            return secret;
        }
    }
    let generated = generate_secure_secret();
    if write_secret_private(SHARED_SECRET_PATH, &generated).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                SHARED_SECRET_PATH,
                std::fs::Permissions::from_mode(0o600),
            );
        }
        tracing::warn!(
            "auto-generated jwt_secret and saved to {} (shared by all CHV services). \
             For production, configure an explicit jwt_secret.",
            SHARED_SECRET_PATH
        );
    } else {
        tracing::error!(
            "auto-generated jwt_secret but could not write to {}. \
             Each service will generate its own secret — tokens will NOT be portable between services. \
             Set jwt_secret explicitly in {} config.",
            SHARED_SECRET_PATH, service_name
        );
    }
    generated
}

const DEV_SECRET: &str = "chv-dev-secret-change-in-production";

/// Domain-separate the per-purpose secrets that used to be the single
/// shared `jwt_secret`.
///
/// The derivation is HMAC-SHA256 keyed by the legacy master with a
/// versioned, purpose-specific label. It preserves existing single-host
/// installs (both the control plane and the agent derive the same console
/// key) while ensuring a user JWT, a console JWT, and the agent credential
/// are different cryptographic keys even before an operator configures the
/// new `[secrets]` fields. Independent rotation requires explicit values —
/// rotating the legacy master rotates every derived key together.
fn derive_domain_secret(master: &str, domain: &str) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac =
        Hmac::<Sha256>::new_from_slice(master.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(b"chv-secret-v1");
    mac.update(b"\0");
    mac.update(domain.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn explicit_secret(value: &Option<String>) -> Option<&str> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|secret| !secret.is_empty())
}

fn secret_is_weak(secret: &str) -> bool {
    secret.trim().is_empty() || secret == DEV_SECRET || secret.len() < 32
}

/// Per-purpose credentials. Every field is optional: when omitted, the
/// legacy `jwt_secret` is expanded with domain separation (see
/// [`derive_domain_secret`]). Fields that are present must be strong and
/// must not duplicate another purpose (validation fails closed).
#[derive(Clone, Default, Deserialize)]
pub struct SecretsConfig {
    /// Signing key for end-user session JWTs (BFF/control-plane login).
    #[serde(default)]
    pub user_jwt: Option<String>,
    /// Signing key for short-lived serial-console JWTs. The agent must be
    /// configured with the same value as the control plane.
    #[serde(default)]
    pub console_jwt: Option<String>,
    /// Bearer token the control plane presents on every agent RPC today.
    /// The control plane holds one value and sends it to every agent it
    /// serves, so all of those agents must be configured with this same
    /// token. Per-node token selection is future ADR-029 (multi-host)
    /// work, not an implemented capability. Rotating one agent's token in
    /// isolation therefore requires re-keying the control plane or a
    /// maintenance window.
    #[serde(default)]
    pub agent_auth_token: Option<String>,
    /// Key used for credential encryption at rest (backup S3 secrets,
    /// NetBox API tokens). Takes precedence over `CHV_ENCRYPTION_KEY`.
    #[serde(default)]
    pub credential_encryption: Option<String>,
}

// `AgentConfig` and `ControlPlaneConfig` derive `Debug` and embed this
// struct, so a plain derive here would print every secret through `{:?}`.
// Report only whether a purpose is configured, never the value or length.
impl std::fmt::Debug for SecretsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn presence(value: &Option<String>) -> &'static str {
            if value.is_some() {
                "<redacted>"
            } else {
                "<unset>"
            }
        }

        f.debug_struct("SecretsConfig")
            .field("user_jwt", &presence(&self.user_jwt))
            .field("console_jwt", &presence(&self.console_jwt))
            .field("agent_auth_token", &presence(&self.agent_auth_token))
            .field(
                "credential_encryption",
                &presence(&self.credential_encryption),
            )
            .finish()
    }
}

/// Deployment-security switches shared by the control plane and agent.
#[derive(Debug, Clone, Deserialize)]
pub struct SecurityConfig {
    /// Declares a production deployment. The default is fail-closed: it
    /// refuses the insecure agent-auth bypass. Development setups must opt
    /// out explicitly (`production = false`) AND enable the bypass.
    #[serde(default = "default_production")]
    pub production: bool,
    /// Explicitly disables token authentication and the peer-UID gate on
    /// the agent's Unix socket. On the control plane this only relaxes
    /// config validation (it never changes the token the control plane
    /// sends). Logged loudly at startup; refused when `production = true`.
    /// Only for local development.
    #[serde(default)]
    pub allow_insecure_agent_auth: bool,
    /// Agent-only: Unix UIDs allowed to connect to the agent socket. Empty
    /// means "the agent's own effective UID" (the control plane normally
    /// runs as the same service user on a single host). Ignored by the
    /// control plane and when the insecure bypass is active.
    #[serde(default)]
    pub agent_auth_allowed_peer_uids: Vec<u32>,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            production: default_production(),
            allow_insecure_agent_auth: false,
            agent_auth_allowed_peer_uids: Vec::new(),
        }
    }
}

fn default_production() -> bool {
    true
}

impl SecretsConfig {
    fn user_jwt(&self, master: &str) -> String {
        explicit_secret(&self.user_jwt)
            .map(str::to_owned)
            .unwrap_or_else(|| derive_domain_secret(master, "user-jwt"))
    }

    fn console_jwt(&self, master: &str) -> String {
        explicit_secret(&self.console_jwt)
            .map(str::to_owned)
            .unwrap_or_else(|| derive_domain_secret(master, "console-jwt"))
    }

    fn agent_auth_token(&self, master: &str) -> String {
        explicit_secret(&self.agent_auth_token)
            .map(str::to_owned)
            .unwrap_or_else(|| derive_domain_secret(master, "agent-auth-token"))
    }

    fn credential_encryption(&self) -> Option<String> {
        explicit_secret(&self.credential_encryption)
            .map(str::to_owned)
            .or_else(|| {
                std::env::var("CHV_ENCRYPTION_KEY")
                    .ok()
                    .map(|key| key.trim().to_owned())
                    .filter(|key| !key.is_empty())
            })
    }

    fn validate_explicit(&self, include_control_plane_only: bool) -> Result<(), ConfigError> {
        let mut fields = vec![
            ("secrets.console_jwt", &self.console_jwt),
            ("secrets.agent_auth_token", &self.agent_auth_token),
        ];
        if include_control_plane_only {
            fields.push(("secrets.user_jwt", &self.user_jwt));
            fields.push(("secrets.credential_encryption", &self.credential_encryption));
        }
        for (name, value) in fields {
            if let Some(secret) = explicit_secret(value) {
                if secret_is_weak(secret) {
                    return Err(ConfigError::Invalid(format!(
                        "{name} must be at least 32 characters and must not be the \
                         development placeholder"
                    )));
                }
                // The agent token is carried as gRPC metadata. A non-ASCII
                // value would make `MetadataValue::try_from` fail at send
                // time and silently omit the header, so reject it at load
                // time with an actionable error instead.
                if name == "secrets.agent_auth_token"
                    && !secret.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
                {
                    return Err(ConfigError::Invalid(format!(
                        "{name} must contain only printable ASCII characters so it can be \
                         sent as gRPC metadata"
                    )));
                }
            }
        }
        Ok(())
    }
}

fn ensure_distinct_secrets(secrets: &[(&str, &str)]) -> Result<(), ConfigError> {
    for (index, (left_name, left)) in secrets.iter().enumerate() {
        for (right_name, right) in secrets.iter().skip(index + 1) {
            if chv_common::constant_time_eq(left.as_bytes(), right.as_bytes()) {
                return Err(ConfigError::Invalid(format!(
                    "{left_name} and {right_name} must be distinct secrets; \
                     configure separate values under [secrets]"
                )));
            }
        }
    }
    Ok(())
}

fn validate_security_mode(security: &SecurityConfig) -> Result<(), ConfigError> {
    if security.production && security.allow_insecure_agent_auth {
        return Err(ConfigError::Invalid(
            "security.allow_insecure_agent_auth is forbidden when security.production = true; \
             it is a local-development bypass only"
                .to_string(),
        ));
    }
    if security.allow_insecure_agent_auth {
        tracing::warn!(
            "SECURITY: agent token authentication is DISABLED \
             (security.allow_insecure_agent_auth = true). Never use this in production."
        );
    }
    Ok(())
}

/// Fabric overlay (ADR-021) configuration for the network daemon.
///
/// When `enabled` is false (the default), `chv-nwd` runs bridge-only and every
/// fabric RPC fails closed. `state_dir` MUST be persistent (not under `/run`,
/// which is tmpfs): it holds the fabric ownership journal and the host's
/// WireGuard private key, which must survive reboots and fabric teardown.
#[derive(Debug, Clone, Deserialize)]
pub struct FabricNwdConfig {
    /// Master switch for the stretched-L2 fabric provider.
    #[serde(default)]
    pub enabled: bool,

    /// Durable state root (ownership journal, plan journal, WireGuard key).
    #[serde(default = "default_fabric_state_dir")]
    pub state_dir: PathBuf,

    /// Interface-name prefix for generated fabric objects (netns, WireGuard,
    /// veths). Bounded to 4 characters by the shared provider (IFNAMSIZ).
    #[serde(default = "default_fabric_name_prefix")]
    pub name_prefix: String,

    /// WireGuard listen port (shared Kubedo fabric convention).
    #[serde(default = "default_fabric_wireguard_port")]
    pub wireguard_port: u16,

    /// VXLAN destination port inside the tunnel.
    #[serde(default = "default_fabric_vxlan_port")]
    pub vxlan_port: u16,

    /// Tenant MTU used when a fabric plan omits it (underlay 1500 − 110 − 10).
    #[serde(default = "default_fabric_tenant_mtu")]
    pub default_tenant_mtu: u32,

    /// Fabric (WireGuard) MTU used when a fabric plan omits it.
    #[serde(default = "default_fabric_fabric_mtu")]
    pub default_fabric_mtu: u32,
}

impl Default for FabricNwdConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            state_dir: default_fabric_state_dir(),
            name_prefix: default_fabric_name_prefix(),
            wireguard_port: default_fabric_wireguard_port(),
            vxlan_port: default_fabric_vxlan_port(),
            default_tenant_mtu: default_fabric_tenant_mtu(),
            default_fabric_mtu: default_fabric_fabric_mtu(),
        }
    }
}

fn default_fabric_state_dir() -> PathBuf {
    PathBuf::from("/var/lib/chv/nwd/fabric")
}
fn default_fabric_name_prefix() -> String {
    "chv".to_string()
}
fn default_fabric_wireguard_port() -> u16 {
    65_001
}
fn default_fabric_vxlan_port() -> u16 {
    4789
}
fn default_fabric_tenant_mtu() -> u32 {
    1380
}
fn default_fabric_fabric_mtu() -> u32 {
    1440
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse error: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid config: {0}")]
    Invalid(String),
}

/// Resolve the dedicated migration-signing secret from exactly one source:
/// an inline config value, a file, or an environment variable (in that
/// precedence after rejecting inline+file). Never logs the value.
///
/// A configured-but-empty source is a startup error, not "unset": silently
/// falling through would turn an operator typo into unauthenticated
/// migrations on the dev path or an opaque dispatch failure on the CP path.
pub fn resolve_migration_signing_secret(
    inline: Option<&str>,
    file: Option<&Path>,
    env_var: &str,
) -> Result<Option<Vec<u8>>, ConfigError> {
    if inline.is_some() && file.is_some() {
        return Err(ConfigError::Invalid(
            "migration signing_secret and signing_secret_file are mutually exclusive".to_string(),
        ));
    }

    let (secret, source) = if let Some(inline) = inline {
        (inline.to_string(), "config")
    } else if let Some(file) = file {
        let contents = std::fs::read_to_string(file)?;
        (contents.trim().to_string(), "file")
    } else {
        match std::env::var(env_var) {
            Ok(value) => (value, "environment"),
            Err(_) => return Ok(None),
        }
    };

    if secret.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "migration signing secret from {source} is empty; refusing to start/run with \
             an unusable migration authorization secret"
        )));
    }
    if secret.len() < 32 {
        return Err(ConfigError::Invalid(format!(
            "migration signing secret from {source} is shorter than 32 bytes; use a \
             dedicated high-entropy secret (never the JWT/enrollment secret)"
        )));
    }
    Ok(Some(secret.into_bytes()))
}

#[derive(Debug, Clone, Deserialize)]
pub struct StordConfig {
    pub socket_path: PathBuf,
    pub runtime_dir: PathBuf,
    pub log_level: String,
    #[serde(default)]
    pub backend_allowlist: Vec<String>,
    #[serde(default)]
    pub path_allowlist: Vec<PathBuf>,
    #[serde(default)]
    pub device_allowlist: Vec<String>,
    pub metrics_bind: Option<String>,
    /// Storage backend type: "local" (default), "iscsi", "ceph", or "lvm".
    #[serde(default)]
    pub backend_type: Option<String>,
    /// Allowed migration destination hosts. Empty = allow all.
    #[serde(default)]
    pub migration_dest_allowlist: Vec<String>,
    /// iSCSI backend configuration (required when backend_type = "iscsi").
    #[serde(default)]
    pub iscsi: Option<StordIscsiConfig>,
    /// Ceph RBD backend configuration (required when backend_type = "ceph").
    #[serde(default)]
    pub ceph: Option<StordCephConfig>,
    /// LVM backend configuration (required when backend_type = "lvm").
    /// The value is the volume group name.
    #[serde(default)]
    pub lvm_volume_group: Option<String>,
    /// Storage migration configuration. `migration.enabled = true` requires
    /// at least one of the two halves — client identity fields and/or
    /// receiver fields — each all-or-nothing, validated at startup
    /// (fail-closed). Disabled (default) means migration actions are
    /// unavailable rather than downgraded. See `docs/specs` migration ADR
    /// and issues #232, #401.
    #[serde(default)]
    pub migration: StordMigrationConfig,
}

/// Storage migration configuration.
///
/// Explicitly distinguishes "migration disabled" (daemon may start without
/// credentials; migration actions fail unavailable) from "migration enabled but
/// broken" (missing/invalid TLS material is a startup error).
#[derive(Debug, Clone, Deserialize)]
pub struct StordMigrationConfig {
    /// Master switch. When `true`, at least one half below must be
    /// configured: the client identity fields (source half) and/or the
    /// receiver fields (destination half) — each half is all-or-nothing and
    /// validated at startup; neither half configured is a startup error
    /// (issue #401). When `false`, the daemon starts without migration
    /// credentials and any field set anywhere in this section is a startup
    /// error.
    #[serde(default)]
    pub enabled: bool,
    /// PEM client/node certificate path (issued by the CHV CA). Source
    /// (outbound-migration) half: the four client fields are all-or-nothing
    /// among themselves and independently optional as a set — omitting all
    /// four under `enabled = true` configures a destination-only stord that
    /// never initiates migrations (issue #401).
    #[serde(default)]
    pub client_cert_path: Option<PathBuf>,
    /// PEM client private key path (all-or-nothing with the other client
    /// fields).
    #[serde(default)]
    pub client_key_path: Option<PathBuf>,
    /// PEM CA bundle used to validate the migration destination
    /// (all-or-nothing with the other client fields).
    #[serde(default)]
    pub ca_cert_path: Option<PathBuf>,
    /// Expected destination server name used for certificate validation.
    /// Must match the destination certificate's DNS SAN / identity
    /// (all-or-nothing with the other client fields).
    #[serde(default)]
    pub dest_server_name: Option<String>,
    /// TCP address for the migration receiver mTLS listener (server half,
    /// issue #390), e.g. `"127.0.0.1:50052"`. Kept as a raw String here and
    /// parsed/validated by the fail-closed startup loader
    /// (`load_migration_server_tls`); no cross-field validation in this crate.
    /// Unset (default) means this stord is migration-source-only and never
    /// accepts inbound migrations. NOTE: the receiver fields take effect only
    /// when `migration.enabled = true`; setting any of them with
    /// `enabled = false` is a startup error (fail-closed) — `enabled` is the
    /// single master switch for both halves of migration TLS.
    #[serde(default)]
    pub listen_addr: Option<String>,
    /// PEM server certificate path for the migration receiver listener
    /// (validated at startup, all-or-nothing with the other receiver fields).
    #[serde(default)]
    pub server_cert_path: Option<PathBuf>,
    /// PEM server private key path for the migration receiver listener.
    #[serde(default)]
    pub server_key_path: Option<PathBuf>,
    /// PEM CA bundle used to authenticate migration peers (client
    /// certificates) on the receiver listener.
    #[serde(default)]
    pub client_ca_path: Option<PathBuf>,
    /// This stord's node id, used to verify that a migration authorization
    /// (issue #663) names THIS node as the destination. Required when a
    /// receiver half is configured and authorization is not explicitly
    /// disabled; must match the owning agent's `node_id`.
    #[serde(default)]
    pub node_id: Option<String>,
    /// Dedicated HMAC secret for migration authorizations (issue #663).
    /// Never the user JWT/enrollment secret. Prefer `signing_secret_file`;
    /// the value is never logged. With a receiver half configured, one of
    /// `signing_secret`, `signing_secret_file`, or the
    /// `CHV_MIGRATION_SIGNING_SECRET` environment variable is required
    /// unless `allow_insecure_unauthenticated = true` is set explicitly.
    #[serde(default)]
    pub signing_secret: Option<String>,
    /// File containing the dedicated migration-signing secret (trimmed).
    /// Mutually exclusive with `signing_secret`. The file should be
    /// root:chv 0640 (same posture as the TLS key material).
    #[serde(default)]
    pub signing_secret_file: Option<PathBuf>,
    /// Explicit development-only escape hatch (issue #663). When true,
    /// inbound migration streams do not require a control-plane token.
    /// NEVER enable this in production: any CA-certified peer could create
    /// receiving volumes. Default false = fail closed.
    #[serde(default)]
    pub allow_insecure_unauthenticated: bool,
    /// Maximum number of concurrently active inbound receiving migrations
    /// on this stord (issue #663). Default 2. Rejected migrations never
    /// reach `create_receiving_volume`.
    #[serde(default = "default_max_inflight_receiving")]
    pub max_inflight_receiving: usize,
    /// Maximum total bytes reserved by concurrently active inbound
    /// receiving migrations on this stord (issue #663). Default 2 TiB.
    #[serde(default = "default_max_inflight_receiving_bytes")]
    pub max_inflight_receiving_bytes: u64,
}

fn default_max_inflight_receiving() -> usize {
    2
}

fn default_max_inflight_receiving_bytes() -> u64 {
    2 * 1024 * 1024 * 1024 * 1024
}

impl Default for StordMigrationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            client_cert_path: None,
            client_key_path: None,
            ca_cert_path: None,
            dest_server_name: None,
            listen_addr: None,
            server_cert_path: None,
            server_key_path: None,
            client_ca_path: None,
            node_id: None,
            signing_secret: None,
            signing_secret_file: None,
            allow_insecure_unauthenticated: false,
            max_inflight_receiving: default_max_inflight_receiving(),
            max_inflight_receiving_bytes: default_max_inflight_receiving_bytes(),
        }
    }
}

/// iSCSI backend configuration embedded in StordConfig.
#[derive(Debug, Clone, Deserialize)]
pub struct StordIscsiConfig {
    pub portal: String,
    pub target_iqn: String,
    pub initiator_name: String,
    pub chap_username: Option<String>,
    pub chap_secret: Option<String>,
}

/// Ceph RBD backend configuration embedded in StordConfig.
#[derive(Debug, Clone, Deserialize)]
pub struct StordCephConfig {
    #[serde(default = "default_ceph_cluster_name")]
    pub cluster_name: String,
    pub pool_name: String,
    pub user: String,
    pub keyring_path: String,
    pub monitors: String,
}

fn default_ceph_cluster_name() -> String {
    "ceph".to_string()
}

impl Default for StordConfig {
    fn default() -> Self {
        Self {
            socket_path: PathBuf::from("/run/chv/stord/api.sock"),
            runtime_dir: PathBuf::from("/var/lib/chv/storage/localdisk"),
            log_level: "info".to_string(),
            backend_allowlist: vec![],
            path_allowlist: vec![
                PathBuf::from("/var/lib/chv/storage/localdisk"),
                PathBuf::from("/var/lib/chv/storage/lvm"),
                PathBuf::from("/var/lib/chv/agent"),
            ],
            device_allowlist: vec!["/dev/dm-*".to_string(), "/dev/mapper/*".to_string()],
            metrics_bind: None,
            backend_type: None,
            migration_dest_allowlist: vec![],
            iscsi: None,
            ceph: None,
            lvm_volume_group: None,
            migration: StordMigrationConfig::default(),
        }
    }
}

pub fn load_stord_config(path: Option<&Path>) -> Result<StordConfig, ConfigError> {
    let mut cfg = StordConfig::default();
    if let Some(p) = path {
        let text = std::fs::read_to_string(p)?;
        cfg = toml::from_str(&text)?;
    }
    Ok(cfg)
}

#[derive(Debug, Clone, Deserialize)]
pub struct NwdConfig {
    pub socket_path: PathBuf,
    pub runtime_dir: PathBuf,
    pub log_level: String,
    pub metrics_bind: Option<String>,
    /// VXLAN overlay network settings.
    #[serde(default)]
    pub overlay: OverlayConfig,
    /// eBPF policy engine settings.
    #[serde(default)]
    pub ebpf: EbpfConfig,
    /// Stretched-L2 fabric (ADR-021) settings.
    #[serde(default)]
    pub fabric: FabricNwdConfig,
}

impl Default for NwdConfig {
    fn default() -> Self {
        Self {
            socket_path: PathBuf::from("/run/chv/nwd/api.sock"),
            runtime_dir: PathBuf::from("/run/chv/nwd"),
            log_level: "info".to_string(),
            metrics_bind: None,
            overlay: OverlayConfig::default(),
            ebpf: EbpfConfig::default(),
            fabric: FabricNwdConfig::default(),
        }
    }
}

pub fn load_nwd_config(path: Option<&Path>) -> Result<NwdConfig, ConfigError> {
    let mut cfg = NwdConfig::default();
    if let Some(p) = path {
        let text = std::fs::read_to_string(p)?;
        cfg = toml::from_str(&text)?;
    }
    Ok(cfg)
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AgentAuthorityMode {
    #[default]
    Legacy,
    CoreNative,
    CoreManaged,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    #[serde(default)]
    pub authority_mode: AgentAuthorityMode,
    pub socket_path: PathBuf,
    pub runtime_dir: PathBuf,
    pub log_level: String,
    pub control_plane_addr: String,
    pub stord_socket: PathBuf,
    pub nwd_socket: PathBuf,
    pub chv_binary_path: PathBuf,
    pub stord_binary_path: PathBuf,
    pub nwd_binary_path: PathBuf,
    pub cache_path: PathBuf,
    #[serde(default = "default_core_store_path")]
    pub core_store_path: PathBuf,
    #[serde(default = "default_core_api_socket_path")]
    pub core_api_socket_path: PathBuf,
    #[serde(default = "default_core_archive_path")]
    pub core_archive_path: PathBuf,
    pub node_id: String,
    pub metrics_bind: Option<String>,
    pub tls_cert_path: Option<PathBuf>,
    pub tls_key_path: Option<PathBuf>,
    pub ca_cert_path: Option<PathBuf>,
    pub bootstrap_token_path: Option<PathBuf>,
    #[serde(default = "default_storage_base_dir")]
    pub storage_base_dir: PathBuf,
    /// Paths preserved in the supervisor-generated chv-stord.toml's
    /// `path_allowlist` when the agent respawns stord (#376). Empty omits
    /// the key — stord then allows all locator paths (the pre-#376
    /// behavior, kept as the default). Deployments relying on stord's
    /// path confinement set this to mirror their stord config (it must
    /// cover the volume locator dir, e.g. storage_base_dir, and any
    /// image/seed dirs the control plane references).
    #[serde(default)]
    pub stord_path_allowlist: Vec<PathBuf>,
    /// Operator's `stord.toml`, passed through verbatim when the
    /// supervisor must respawn stord (#385): the respawned daemon is
    /// exec'd with this file instead of a supervisor-generated config,
    /// so every operator key (`runtime_dir`, `backend_type`,
    /// `device_allowlist`, `[migration]`, …) survives respawn. The
    /// supervisor validates only that the file parses and its
    /// `socket_path` matches [`AgentConfig::stord_socket`] — on any
    /// mismatch or read/parse failure it falls back to today's
    /// generated config with a loud warn (never worse than status
    /// quo). `None` (default) keeps the historical generated-config
    /// respawn byte-exactly. Requires the agent user to be able to
    /// read the file (the standard install grants this via the
    /// `chv-stord` group).
    #[serde(default)]
    pub stord_config_path: Option<PathBuf>,
    /// Operator's `nwd.toml`, passed through verbatim when the
    /// supervisor must respawn nwd (#504, mirroring #385's stord
    /// pass-through): the respawned daemon is exec'd with this file
    /// instead of a supervisor-generated config, so every operator
    /// key (`metrics_bind`, the `[overlay]`, `[ebpf]` and `[fabric]`
    /// blocks, and future keys) survives respawn. The supervisor
    /// validates only that the file parses and its `socket_path`
    /// matches [`AgentConfig::nwd_socket`] — on any mismatch or
    /// read/parse failure it falls back to today's generated config
    /// with a loud warn (never worse than status quo). `None`
    /// (default) keeps the historical generated-config respawn
    /// byte-exactly. Requires the agent user to be able to read the
    /// file (the standard install grants this: nwd.toml is owned
    /// `root:chv 0640`).
    #[serde(default)]
    pub nwd_config_path: Option<PathBuf>,
    #[serde(default = "default_console_bind")]
    pub console_bind: String,
    #[serde(default = "default_agent_jwt_secret")]
    pub jwt_secret: String,
    /// Per-purpose secrets (#662). `console_jwt` must match the control
    /// plane's value; `agent_auth_token` must match the control plane's
    /// `[secrets] agent_auth_token`. The control plane sends one token to
    /// every agent it serves today, so all agents behind one control plane
    /// share that token (per-node tokens are future ADR-029 work). Both may
    /// instead be derived from the same legacy `jwt_secret` on a
    /// single-host install.
    #[serde(default)]
    pub secrets: SecretsConfig,
    /// Production/development security switches (#662). Production is the
    /// default: the unauthenticated agent bypass is refused.
    #[serde(default)]
    pub security: SecurityConfig,
    #[serde(default)]
    pub watchdog: BootWatchdogAgentConfig,
}

/// Node-level guest-liveness (boot) watchdog settings, from the agent
/// config's `[watchdog]` section. The watchdog detects the frozen-guest
/// condition CH v43's serial-manager defect produces (§ evidence:
/// a mid-burst serial-client death silently kills the manager; the guest
/// freezes mid-boot while `vm.info` keeps reporting Running) and recovers
/// it with `vm.reboot`, which also re-creates the serial manager and
/// restores console capture.
///
/// Detection is marker-based: a boot is complete when `boot_marker`
/// (default `systemd-logind`) appears in the VM's console capture after
/// the current boot's kernel banner (a re-spawned VMM or an agent-driven
/// reboot starts a fresh boot at a recorded byte offset — earlier
/// boots' markers never count). A console that has stalled (no new
/// bytes) with no marker on a Running VM triggers the reboot; a console
/// that wraps or truncates (the 10 MiB capture cap, a graceful stop)
/// never looks frozen on its own.
///
/// OPT-IN by design: a marker-based detector cannot distinguish a frozen
/// boot from a legitimately quiet guest whose image never prints the
/// marker (non-systemd/minimal images — set `boot_marker` accordingly or
/// leave the watchdog disabled), nor from an adopted long-running guest
/// whose console wrapped past its banner. Reboots are bounded by
/// `max_reboots` per unhealthy episode, and a declined or failed reboot
/// never consumes budget it did not earn.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BootWatchdogAgentConfig {
    /// Master switch. Default: disabled.
    #[serde(default)]
    pub enabled: bool,
    /// The boot-complete marker string searched for in the console
    /// capture after the most recent kernel banner. Default
    /// `systemd-logind` (present exactly twice per systemd boot:
    /// Starting + Started lines).
    #[serde(default = "default_watchdog_boot_marker")]
    pub boot_marker: String,
    /// Seconds of no new console bytes (with the boot-complete marker
    /// absent and the VMM alive) before the reboot fires. Default 120 —
    /// far above a healthy boot's quiet gaps on the reference stack.
    #[serde(default = "default_watchdog_stall_secs")]
    pub stall_secs: u64,
    /// Maximum watchdog reboots per unhealthy episode before standing
    /// down (operator territory). Default 2.
    #[serde(default = "default_watchdog_max_reboots")]
    pub max_reboots: u32,
    /// Seconds of continuous marker-healthy state after which the
    /// reboot budget resets. Default 900 (15 minutes).
    #[serde(default = "default_watchdog_healthy_reset_secs")]
    pub healthy_reset_secs: u64,
}

impl Default for BootWatchdogAgentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            boot_marker: default_watchdog_boot_marker(),
            stall_secs: default_watchdog_stall_secs(),
            max_reboots: default_watchdog_max_reboots(),
            healthy_reset_secs: default_watchdog_healthy_reset_secs(),
        }
    }
}

fn default_watchdog_boot_marker() -> String {
    "systemd-logind".to_string()
}

fn default_watchdog_stall_secs() -> u64 {
    120
}

fn default_watchdog_max_reboots() -> u32 {
    2
}

fn default_watchdog_healthy_reset_secs() -> u64 {
    900
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            authority_mode: AgentAuthorityMode::Legacy,
            socket_path: PathBuf::from("/run/chv/agent/api.sock"),
            runtime_dir: PathBuf::from("/var/lib/chv/agent"),
            log_level: "info".to_string(),
            control_plane_addr: "https://localhost:8443".to_string(),
            stord_socket: PathBuf::from("/run/chv/stord/api.sock"),
            nwd_socket: PathBuf::from("/run/chv/nwd/api.sock"),
            chv_binary_path: PathBuf::from("/usr/bin/cloud-hypervisor"),
            stord_binary_path: PathBuf::from("/usr/bin/chv-stord"),
            nwd_binary_path: PathBuf::from("/usr/bin/chv-nwd"),
            cache_path: PathBuf::from("/var/lib/chv/cache/agent-cache.json"),
            core_store_path: default_core_store_path(),
            core_api_socket_path: default_core_api_socket_path(),
            core_archive_path: default_core_archive_path(),
            node_id: String::new(),
            metrics_bind: None,
            tls_cert_path: None,
            tls_key_path: None,
            ca_cert_path: None,
            bootstrap_token_path: None,
            storage_base_dir: PathBuf::from("/var/lib/chv/storage"),
            stord_path_allowlist: vec![],
            stord_config_path: None,
            nwd_config_path: None,
            console_bind: default_console_bind(),
            jwt_secret: default_agent_jwt_secret(),
            secrets: SecretsConfig::default(),
            security: SecurityConfig::default(),
            watchdog: BootWatchdogAgentConfig::default(),
        }
    }
}

fn default_core_store_path() -> PathBuf {
    PathBuf::from("/var/lib/chv/agent/core.db")
}

fn default_core_api_socket_path() -> PathBuf {
    PathBuf::from("/run/chv/core/core-v1.sock")
}

fn default_core_archive_path() -> PathBuf {
    PathBuf::from("/var/lib/chv/agent/node-cache-v1.archive")
}

fn default_storage_base_dir() -> PathBuf {
    PathBuf::from("/var/lib/chv/storage")
}

fn default_console_bind() -> String {
    "127.0.0.1:8444".to_string()
}

fn default_agent_jwt_secret() -> String {
    "chv-dev-secret-change-in-production".to_string()
}

pub fn load_agent_config(path: Option<&Path>) -> Result<AgentConfig, ConfigError> {
    let mut cfg = AgentConfig::default();
    if let Some(p) = path {
        let text = std::fs::read_to_string(p)?;
        cfg = toml::from_str(&text)?;
    }
    materialize_agent_jwt_secret(&mut cfg);
    cfg.validate_secrets()?;
    Ok(cfg)
}

impl AgentConfig {
    /// Console JWT signing key. Explicit `[secrets] console_jwt` wins;
    /// otherwise it is derived from the legacy `jwt_secret` with domain
    /// separation, matching the control plane's derived value on a
    /// single-host install.
    pub fn effective_console_jwt_secret(&self) -> String {
        self.secrets.console_jwt(&self.jwt_secret)
    }

    /// Bearer token the control plane presents on every agent RPC.
    /// Explicit `[secrets] agent_auth_token` wins; otherwise this is a
    /// distinct domain-separated derivation of the legacy master. The
    /// control plane uses one token for all agents it serves, so every one
    /// of those agents must use the same effective value. Per-node tokens
    /// are future ADR-029 work.
    pub fn effective_agent_auth_token(&self) -> String {
        self.secrets.agent_auth_token(&self.jwt_secret)
    }

    /// Fail-closed validation of the #662 secret split. Run by
    /// [`load_agent_config`]; kept public so startup code can re-check a
    /// programmatically constructed config.
    pub fn validate_secrets(&self) -> Result<(), ConfigError> {
        validate_security_mode(&self.security)?;
        // The agent never uses `user_jwt` or `credential_encryption`; skip
        // validating control-plane-only keys it would not consume, so a
        // weak value in agent.toml cannot block startup confusingly.
        self.secrets.validate_explicit(false)?;
        if self.security.allow_insecure_agent_auth {
            return Ok(());
        }
        let console = self.effective_console_jwt_secret();
        let agent = self.effective_agent_auth_token();
        if secret_is_weak(&console) || secret_is_weak(&agent) {
            return Err(ConfigError::Invalid(
                "console/agent secrets must be at least 32 characters".to_string(),
            ));
        }
        ensure_distinct_secrets(&[
            ("secrets.console_jwt (effective)", console.as_str()),
            ("secrets.agent_auth_token (effective)", agent.as_str()),
        ])
    }
}

fn materialize_agent_jwt_secret(cfg: &mut AgentConfig) {
    // main.rs constructs ConsoleServer (the console_jwt consumer) in legacy
    // and core-managed modes, so a default or short secret must not survive
    // config load in either. Core-native returns into run_core_native before
    // the console exists: no in-process consumer, so the configured value is
    // left untouched and no secret is minted on disk.
    if cfg.authority_mode != AgentAuthorityMode::CoreNative
        && (cfg.jwt_secret == DEV_SECRET || cfg.jwt_secret.len() < 32)
    {
        cfg.jwt_secret = resolve_jwt_secret(&cfg.jwt_secret, "agent");
    }
}

const DEFAULT_CONTROLPLANE_GRPC_BIND: &str = "127.0.0.1:8443";
const DEFAULT_CONTROLPLANE_HTTP_BIND: &str = "127.0.0.1:8080";
const DEFAULT_CONTROLPLANE_LOG_LEVEL: &str = "info";
const DEFAULT_CONTROLPLANE_RUNTIME_DIR: &str = "/run/chv/controlplane";
const DEFAULT_CONTROLPLANE_DATABASE_URL: &str = "sqlite:///var/lib/chv/controlplane.db";
const DEFAULT_CONTROLPLANE_MIGRATIONS_DIR: &str = "cmd/chv-controlplane/migrations";
const DEFAULT_CONTROLPLANE_DB_MAX_CONNECTIONS: u32 = 16;
const DEFAULT_CONTROLPLANE_DB_MIN_CONNECTIONS: u32 = 1;
const DEFAULT_CONTROLPLANE_DB_ACQUIRE_TIMEOUT_SECS: u64 = 5;
const DEFAULT_CONTROLPLANE_DB_IDLE_TIMEOUT_SECS: u64 = 300;
const DEFAULT_CONTROLPLANE_DB_MAX_LIFETIME_SECS: u64 = 1800;
const DEFAULT_CONTROLPLANE_AGENT_SOCKET_PATTERN: &str = "/run/chv/agent/api.sock";
const DEFAULT_CONTROLPLANE_KERNEL_PATH: &str = "/var/lib/chv/vmlinux";
const DEFAULT_CONTROLPLANE_FIRMWARE_PATH: &str = "/var/lib/chv/hypervisor-fw";
const DEFAULT_CONTROLPLANE_WEBUI_DIR: &str = "/usr/share/chv/ui";

/// Static Web UI serving (`[webui]`, issue #447 — decision D3 target,
/// `docs/DEPLOYMENT-ARCHITECTURE.md` §8 D3).
///
/// `enabled` defaults to `false` (fail-closed): the control plane serves
/// no static assets unless the operator opts in — no new serving surface
/// appears on the HTTP listener by default. `dir` defaults to the
/// packaged UI tree (`packaging/nfpm/chv-controlplane.yaml` installs
/// `ui/build/` at `/usr/share/chv/ui`; `scripts/install.sh` uses
/// `/opt/chv/ui` and enables the section for its own tree).
#[derive(Debug, Clone, Deserialize)]
pub struct WebUiConfig {
    /// Master switch. Default: disabled.
    #[serde(default)]
    pub enabled: bool,

    /// Root of the built UI tree (`index.html` at its root). When
    /// enabled, non-reserved request paths are served from this
    /// directory with an SPA fallback to `index.html`.
    #[serde(default = "default_webui_dir")]
    pub dir: PathBuf,
}

impl Default for WebUiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: default_webui_dir(),
        }
    }
}

fn default_webui_dir() -> PathBuf {
    PathBuf::from(DEFAULT_CONTROLPLANE_WEBUI_DIR)
}

/// Durable privileged-mutation audit log retention (`[audit]`, #658).
///
/// The audit table lives in the control-plane database and is swept by
/// a bounded periodic worker in the control plane. `retention_days`
/// defaults to 365 days: conservative (long enough for incident review)
/// but bounded. A retention value below one day is rejected by the
/// config loader so a typo cannot silently erase a day's trail.
#[derive(Debug, Clone, Deserialize)]
pub struct AuditConfig {
    #[serde(default = "default_audit_retention_days")]
    pub retention_days: u64,
    /// Sweep cadence. Floored at 60 seconds by the worker to keep a tiny
    /// interval from turning retention into a busy loop.
    #[serde(default = "default_audit_sweep_interval_secs")]
    pub sweep_interval_secs: u64,
    /// Hard ceiling on `audit_log` rows. After every age sweep the
    /// worker prunes the table down to its `max_rows` NEWEST rows, so
    /// a row flood that ages out slower than it arrives (e.g. an
    /// anonymous-denial flood — denials are always recorded) still
    /// cannot grow the SQLite database without bound (#658 review M2).
    #[serde(default = "default_audit_max_rows")]
    pub max_rows: u64,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            retention_days: default_audit_retention_days(),
            sweep_interval_secs: default_audit_sweep_interval_secs(),
            max_rows: default_audit_max_rows(),
        }
    }
}

impl AuditConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.retention_days == 0 {
            return Err("audit.retention_days must be at least 1".to_string());
        }
        // Upper bound (#658 review L5): `retention_days` is multiplied
        // by 86_400_000 into an i64 millisecond cutoff — an absurd
        // value could overflow the cast and land the cutoff in the
        // future, which would delete everything. 36,500 days = 100
        // years is far past any compliance horizon.
        if self.retention_days > 36_500 {
            return Err("audit.retention_days must not exceed 36500 (100 years)".to_string());
        }
        if self.sweep_interval_secs < 60 {
            return Err("audit.sweep_interval_secs must be at least 60".to_string());
        }
        if self.max_rows < 1_000 {
            return Err("audit.max_rows must be at least 1000".to_string());
        }
        if self.max_rows > 1_000_000_000_000 {
            return Err("audit.max_rows must not exceed 1000000000000".to_string());
        }
        Ok(())
    }
}

fn default_audit_retention_days() -> u64 {
    365
}

fn default_audit_sweep_interval_secs() -> u64 {
    86_400
}

fn default_audit_max_rows() -> u64 {
    1_000_000
}

/// Native monitoring history configuration (`[monitoring]`, ADR-027,
/// campaign #602). Backs the isolated monitoring SQLite database; all
/// knobs are advisory budgets enforced by `chv-monitoring-store` — the
/// store treats its file as disposable telemetry and never lets a
/// budget breach block the control plane.
#[derive(Debug, Clone, Deserialize)]
pub struct MonitoringConfig {
    /// Master switch. Default: enabled. When disabled (or when the
    /// store cannot be opened) the control plane serves
    /// `monitoring_unavailable` and VM lifecycle is unaffected.
    #[serde(default = "default_monitoring_enabled")]
    pub enabled: bool,
    #[serde(default = "default_monitoring_database_url")]
    pub database_url: String,
    #[serde(default = "default_monitoring_migrations_dir")]
    pub migrations_dir: PathBuf,
    #[serde(default = "default_monitoring_max_connections")]
    pub max_connections: u32,
    /// Maintenance pass cadence (rollups, retention, WAL checkpoint).
    #[serde(default = "default_monitoring_maintenance_interval_secs")]
    pub maintenance_interval_secs: u64,
    /// Raw sample retention (ADR-027 first-release target: 48 hours).
    #[serde(default = "default_monitoring_raw_retention_hours")]
    pub raw_retention_hours: u64,
    /// Five-minute rollup retention (30 days).
    #[serde(default = "default_monitoring_rollup_5m_retention_days")]
    pub rollup_5m_retention_days: u64,
    /// One-hour rollup retention (180 days).
    #[serde(default = "default_monitoring_rollup_1h_retention_days")]
    pub rollup_1h_retention_days: u64,
    /// Per-target series cap (ingestion contract v1).
    #[serde(default = "default_monitoring_max_series_per_target")]
    pub max_series_per_target: i64,
    /// Filesystem headroom floor in MiB: ingestion stops and monitoring
    /// degrades below it (a separate file on the same filesystem does
    /// NOT isolate disk-full risk — ADR-027).
    #[serde(default = "default_monitoring_min_headroom_mib")]
    pub min_headroom_mib: u64,
    /// Hard database size budget in GiB; maintenance evicts oldest raw
    /// data first when exceeded.
    #[serde(default = "default_monitoring_max_db_gib")]
    pub max_db_gib: u64,
    /// Maximum node metric batches accepted per sender per minute
    /// (rate cap; the agent's 15 s cadence needs 4).
    #[serde(default = "default_monitoring_batches_per_minute")]
    pub batches_per_minute: u32,
    /// Optional guest agent ingestion (ADR-026, G3). Off by default:
    /// enabling it is an explicit deployment decision that also
    /// requires `[http_tls]` (the guest route MUST NOT be served on a
    /// plain-HTTP listener exposed to the VM network).
    #[serde(default)]
    pub guest_ingestion: MonitoringGuestIngestConfig,
    /// Native alerting (ADR-027, G4 part 2). On by default but a
    /// no-op with zero rules — the default install changes nothing.
    #[serde(default)]
    pub alerting: MonitoringAlertingConfig,
    /// Outbound notifications (ADR-027, G4 part 2). Unset by default:
    /// with no destination configured, incidents are UI-only and no
    /// outbox events are enqueued.
    #[serde(default)]
    pub notifications: MonitoringNotificationsConfig,
    /// Optional external export (ADR-027 decision 9, prompt-05 task 7).
    /// Off by default and inert without an explicit metric allowlist;
    /// never load-bearing for the native UI.
    #[serde(default)]
    pub export: MonitoringExportConfig,
}

/// Guest monitoring agent ingestion configuration
/// (`[monitoring.guest_ingestion]`). Every default follows the agent
/// security / ingestion contracts; the ceilings there are not raised
/// by configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct MonitoringGuestIngestConfig {
    #[serde(default)]
    pub enabled: bool,
    /// The base URL guest agents use to reach this manager, e.g.
    /// `https://manager.example.org:8080`. Shown in enrollment
    /// instructions; must be reachable from the VM network over TLS.
    #[serde(default)]
    pub public_base_url: Option<String>,
    /// Dedicated CA for guest monitoring agent certificates. Signs the
    /// agent CSRs at claim redemption and verifies client
    /// certificates on the HTTPS listener. Use a CA separate from the
    /// node-enrollment CA: agent certificates carry no node identity.
    #[serde(default)]
    pub agent_ca_cert_path: Option<PathBuf>,
    #[serde(default)]
    pub agent_ca_key_path: Option<PathBuf>,
    /// Agent certificate lifetime (security contract: short-lived
    /// credentials with rotation and revocation).
    #[serde(default = "default_guest_credential_ttl_days")]
    pub credential_ttl_days: u32,
    /// The agent must rotate once its credential enters this window.
    #[serde(default = "default_guest_renewal_window_days")]
    pub renewal_window_days: u32,
    /// How long the previous credential still authenticates after a
    /// rotation (in-flight batch retry tolerance).
    #[serde(default = "default_guest_rotation_grace_minutes")]
    pub rotation_grace_minutes: u32,
    /// Enrollment claim TTL (contract ceiling: 10 minutes).
    #[serde(default = "default_guest_claim_ttl_seconds")]
    pub claim_ttl_seconds: u32,
    /// Claim redemption attempts accepted per source IP per minute.
    #[serde(default = "default_guest_enroll_attempts_per_minute")]
    pub enroll_attempts_per_minute: u32,
    /// Guest batches accepted per agent per minute (the agent's 15 s
    /// cadence needs 4).
    #[serde(default = "default_guest_agent_batches_per_minute")]
    pub agent_batches_per_minute: u32,
    /// An agent is reported `offline` after this long without
    /// authenticated contact.
    #[serde(default = "default_guest_offline_after_seconds")]
    pub offline_after_seconds: u32,
    /// Fresh enrollment shows `active` for this long without a batch
    /// before flipping to `offline`.
    #[serde(default = "default_guest_enrollment_grace_seconds")]
    pub enrollment_grace_seconds: u32,
}

impl Default for MonitoringGuestIngestConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            public_base_url: None,
            agent_ca_cert_path: None,
            agent_ca_key_path: None,
            credential_ttl_days: default_guest_credential_ttl_days(),
            renewal_window_days: default_guest_renewal_window_days(),
            rotation_grace_minutes: default_guest_rotation_grace_minutes(),
            claim_ttl_seconds: default_guest_claim_ttl_seconds(),
            enroll_attempts_per_minute: default_guest_enroll_attempts_per_minute(),
            agent_batches_per_minute: default_guest_agent_batches_per_minute(),
            offline_after_seconds: default_guest_offline_after_seconds(),
            enrollment_grace_seconds: default_guest_enrollment_grace_seconds(),
        }
    }
}

impl MonitoringGuestIngestConfig {
    /// Whether the manager can serve guest ingestion: enabled, TLS on
    /// the HTTP listener, and the agent CA provisioned.
    pub fn is_ready(&self, http_tls_configured: bool) -> bool {
        self.enabled
            && http_tls_configured
            && self.agent_ca_cert_path.is_some()
            && self.agent_ca_key_path.is_some()
    }

    /// Contract ceilings that configuration must not raise (the
    /// security contract caps claim expiry at 10 minutes; the
    /// ingestion contract caps guest send frequency at one batch per
    /// 5 seconds). Violations are boot errors — loud, never silent.
    pub fn validate(&self) -> Result<(), String> {
        const MAX_CLAIM_TTL_SECONDS: u32 = 600;
        const MAX_AGENT_BATCHES_PER_MINUTE: u32 = 12;
        if self.claim_ttl_seconds == 0 || self.claim_ttl_seconds > MAX_CLAIM_TTL_SECONDS {
            return Err(format!(
                "monitoring.guest_ingestion.claim_ttl_seconds must be 1..={MAX_CLAIM_TTL_SECONDS} \
                 (the security contract caps claim expiry at 10 minutes), got {}",
                self.claim_ttl_seconds
            ));
        }
        if self.agent_batches_per_minute == 0
            || self.agent_batches_per_minute > MAX_AGENT_BATCHES_PER_MINUTE
        {
            return Err(format!(
                "monitoring.guest_ingestion.agent_batches_per_minute must be \
                 1..={MAX_AGENT_BATCHES_PER_MINUTE} (the ingestion contract caps guest send \
                 frequency at one batch per 5 seconds), got {}",
                self.agent_batches_per_minute
            ));
        }
        Ok(())
    }
}

fn default_guest_credential_ttl_days() -> u32 {
    30
}
fn default_guest_renewal_window_days() -> u32 {
    7
}
fn default_guest_rotation_grace_minutes() -> u32 {
    30
}
fn default_guest_claim_ttl_seconds() -> u32 {
    600
}
fn default_guest_enroll_attempts_per_minute() -> u32 {
    10
}
fn default_guest_agent_batches_per_minute() -> u32 {
    // The ingestion contract's ceiling: max guest send frequency of
    // one batch per 5 seconds (= 12/minute).
    12
}
fn default_guest_offline_after_seconds() -> u32 {
    90
}
fn default_guest_enrollment_grace_seconds() -> u32 {
    600
}

/// Native alerting configuration (`[monitoring.alerting]`, ADR-027,
/// campaign #602, prompt 05 / G4 part 2).
///
/// The evaluator is a no-op until rules exist: with zero rules (the
/// default install) it evaluates nothing, opens nothing and notifies
/// nobody — enabling alerting never changes a deployment that has not
/// asked for it. The evaluator only runs while the monitoring store
/// is connected (it reads samples from it).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonitoringAlertingConfig {
    #[serde(default = "default_alerting_enabled")]
    pub enabled: bool,
    /// Evaluator tick. Bounded so hold-down windows in the tens of
    /// seconds stay honest.
    #[serde(default = "default_alerting_evaluation_interval_secs")]
    pub evaluation_interval_secs: u64,
    /// Ceiling on stored rules; creating beyond it fails loudly.
    /// Bounds per-tick evaluation work (rules are per-target in v1).
    #[serde(default = "default_alerting_max_rules")]
    pub max_rules: u32,
}

impl Default for MonitoringAlertingConfig {
    fn default() -> Self {
        Self {
            enabled: default_alerting_enabled(),
            evaluation_interval_secs: default_alerting_evaluation_interval_secs(),
            max_rules: default_alerting_max_rules(),
        }
    }
}

impl MonitoringAlertingConfig {
    pub fn validate(&self) -> Result<(), String> {
        const MIN_INTERVAL_SECS: u64 = 5;
        const MAX_INTERVAL_SECS: u64 = 60;
        const MIN_RULES: u32 = 1;
        const MAX_RULES_CEILING: u32 = 500;
        if self.evaluation_interval_secs < MIN_INTERVAL_SECS
            || self.evaluation_interval_secs > MAX_INTERVAL_SECS
        {
            return Err(format!(
                "monitoring.alerting.evaluation_interval_secs must be \
                 {MIN_INTERVAL_SECS}..={MAX_INTERVAL_SECS}, got {}",
                self.evaluation_interval_secs
            ));
        }
        if self.max_rules < MIN_RULES || self.max_rules > MAX_RULES_CEILING {
            return Err(format!(
                "monitoring.alerting.max_rules must be {MIN_RULES}..={MAX_RULES_CEILING}, got {}",
                self.max_rules
            ));
        }
        Ok(())
    }
}

fn default_alerting_enabled() -> bool {
    true
}
fn default_alerting_evaluation_interval_secs() -> u64 {
    15
}
fn default_alerting_max_rules() -> u32 {
    200
}

/// The webhook signing secret. Debug-redacted so a printed config can
/// never leak it.
#[derive(Clone, Default)]
pub struct NotificationSecret(String);

impl std::fmt::Debug for NotificationSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NotificationSecret([redacted])")
    }
}

impl<'de> Deserialize<'de> for NotificationSecret {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Ok(NotificationSecret(raw))
    }
}

impl NotificationSecret {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Outbound notification configuration (`[monitoring.notifications]`).
///
/// Everything is optional and unset by default: with no destination
/// configured the evaluator still records incidents (the UI is always
/// a channel) but enqueues no webhook events — an unconfigured system
/// never accumulates outbox rows.
///
/// Destinations come ONLY from this operator section. That is the
/// allowlist: rules and the UI can never direct a notification
/// anywhere. The transport adds the remaining guardrails (HTTPS-only,
/// no redirects), and boot validation rejects credentials-in-URL and
/// link-local/unspecified IP literals (the cloud-metadata SSRF
/// hazard). Loopback and private ranges are allowed on purpose — an
/// internal receiver is a legitimate destination for an
/// operator-configured webhook.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonitoringNotificationsConfig {
    /// Signed webhook destination (contract envelope, HMAC-SHA256).
    #[serde(default)]
    pub webhook_url: Option<String>,
    /// HMAC secret for `webhook_url` deliveries (`v1=` signatures).
    /// Required (>= 16 chars) whenever a destination is set.
    #[serde(default)]
    pub webhook_signing_secret: NotificationSecret,
    /// Optional Slack incoming-webhook destination (plain `{"text":…}`
    /// posts; the URL itself is Slack's secret).
    #[serde(default)]
    pub slack_webhook_url: Option<String>,
    /// Optional PEM CA bundle for internal destinations; system roots
    /// when unset.
    #[serde(default)]
    pub webhook_ca_path: Option<PathBuf>,
    /// Delivery attempts before an event is dead-lettered.
    #[serde(default = "default_notifications_max_attempts")]
    pub max_attempts: u32,
    /// Dispatcher tick.
    #[serde(default = "default_notifications_dispatch_interval_secs")]
    pub dispatch_interval_secs: u64,
    /// Events claimed per dispatch pass.
    #[serde(default = "default_notifications_max_batch")]
    pub max_batch: u32,
}

impl Default for MonitoringNotificationsConfig {
    fn default() -> Self {
        Self {
            webhook_url: None,
            webhook_signing_secret: NotificationSecret::default(),
            slack_webhook_url: None,
            webhook_ca_path: None,
            max_attempts: default_notifications_max_attempts(),
            dispatch_interval_secs: default_notifications_dispatch_interval_secs(),
            max_batch: default_notifications_max_batch(),
        }
    }
}

impl MonitoringNotificationsConfig {
    /// Whether any destination is configured (events are enqueued at
    /// all only when this is true).
    pub fn has_destination(&self) -> bool {
        self.webhook_url.is_some() || self.slack_webhook_url.is_some()
    }

    pub fn validate(&self) -> Result<(), String> {
        const MIN_SECRET_BYTES: usize = 16;
        const MAX_SECRET_BYTES: usize = 256;

        for (label, url) in [
            ("webhook_url", &self.webhook_url),
            ("slack_webhook_url", &self.slack_webhook_url),
        ] {
            let Some(url) = url else { continue };
            validate_notification_url(label, url)?;
        }

        if self.has_destination() {
            let secret = self.webhook_signing_secret.as_str();
            if secret.is_empty() {
                return Err(
                    "monitoring.notifications.webhook_signing_secret is required when a \
                     destination URL is set (HMAC-SHA256 webhook signatures)"
                        .to_string(),
                );
            }
            if secret.len() < MIN_SECRET_BYTES || secret.len() > MAX_SECRET_BYTES {
                return Err(format!(
                    "monitoring.notifications.webhook_signing_secret must be \
                     {MIN_SECRET_BYTES}..={MAX_SECRET_BYTES} bytes"
                ));
            }
        }

        if !(1..=20).contains(&self.max_attempts) {
            return Err(format!(
                "monitoring.notifications.max_attempts must be 1..=20, got {}",
                self.max_attempts
            ));
        }
        if !(1..=60).contains(&self.dispatch_interval_secs) {
            return Err(format!(
                "monitoring.notifications.dispatch_interval_secs must be 1..=60, got {}",
                self.dispatch_interval_secs
            ));
        }
        if !(1..=50).contains(&self.max_batch) {
            return Err(format!(
                "monitoring.notifications.max_batch must be 1..=50, got {}",
                self.max_batch
            ));
        }

        if let Some(ca_path) = &self.webhook_ca_path {
            if !ca_path.is_file() {
                return Err(format!(
                    "monitoring.notifications.webhook_ca_path is not a readable file: {}",
                    ca_path.display()
                ));
            }
        }
        Ok(())
    }
}

/// Structural destination-URL validation (boot-time, fail-loud):
/// HTTPS only, no credentials, and no link-local/unspecified IP
/// literal (the cloud-metadata SSRF hazard). Loopback and private
/// ranges are deliberately ALLOWED: the operator's config IS the
/// allowlist, and internal receivers on private networks are a
/// first-class deployment shape.
///
/// Hosts are parsed with the `url` crate — the SAME WHATWG parser
/// reqwest uses at send time — so every encoding form (integer and
/// hex IPv4, IPv4-mapped and IPv4-compatible IPv6, bracketed
/// literals) is seen exactly as the HTTP client will see it. A
/// string-prefix check would miss `https://2852039166/` (integer
/// 169.254.169.254) or `https://[::ffff:169.254.169.254]/`.
fn validate_notification_url(label: &str, url: &str) -> Result<(), String> {
    const MAX_URL_BYTES: usize = 2048;
    if url.len() > MAX_URL_BYTES {
        return Err(format!(
            "monitoring.notifications.{label} exceeds {MAX_URL_BYTES} bytes"
        ));
    }
    let parsed = url::Url::parse(url)
        .map_err(|e| format!("monitoring.notifications.{label} is not a valid URL: {e}"))?;
    if parsed.scheme() != "https" {
        return Err(format!(
            "monitoring.notifications.{label} must use https://, got {url:?}"
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(format!(
            "monitoring.notifications.{label} must not carry credentials in the URL"
        ));
    }
    let is_bad_ipv4 =
        |v4: std::net::Ipv4Addr| v4.is_unspecified() || v4.is_link_local() || v4.is_broadcast();
    match parsed.host() {
        None => {
            return Err(format!("monitoring.notifications.{label} has no host"));
        }
        Some(url::Host::Ipv4(v4)) => {
            if is_bad_ipv4(v4) {
                return Err(rejected_ip(label));
            }
        }
        Some(url::Host::Ipv6(v6)) => {
            // Unspecified and the full fe80::/10 link-local range
            // (not just the exact fe80:: node), plus IPv4-mapped and
            // IPv4-compatible forms that hide a bad IPv4 literal.
            let mapped_bad = v6.to_ipv4().is_some_and(is_bad_ipv4);
            let link_local_v6 = v6.is_unspecified() || (v6.segments()[0] & 0xffc0) == 0xfe80;
            if mapped_bad || link_local_v6 {
                return Err(rejected_ip(label));
            }
        }
        Some(url::Host::Domain(_)) => {
            // A hostname is the operator's choice; it is not an IP
            // literal and DNS is outside this boot check's scope
            // (the config itself is the allowlist).
        }
    }
    Ok(())
}

fn rejected_ip(label: &str) -> String {
    format!(
        "monitoring.notifications.{label} must not use an unspecified, broadcast or \
         link-local address (cloud metadata services live there)"
    )
}

fn default_notifications_max_attempts() -> u32 {
    8
}
fn default_notifications_dispatch_interval_secs() -> u64 {
    5
}
fn default_notifications_max_batch() -> u32 {
    10
}

/// Optional external export configuration
/// (`[monitoring.export]`, ADR-027 decision 9 — prompt-05 task 7).
///
/// Off by default and inert without an explicit metric allowlist:
/// export is an egress surface, so "selected data" must be an operator
/// decision, never a default. The destination comes ONLY from this
/// operator section (the allowlist discipline the notifications
/// transport uses); the transport adds HTTPS-only, no-redirects and
/// the cloud-metadata IP rejections, and pushes Prometheus
/// remote-write v1 (VictoriaMetrics/Prometheus/Mimir-compatible).
///
/// The worker degrades, never blocks: ingest, queries, alerting and
/// VM lifecycle are unaffected by export failures (ADR-027 decision
/// 11 — export must never become load-bearing for the native UI).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonitoringExportConfig {
    /// Master switch (explicit opt-in; default false).
    #[serde(default)]
    pub enabled: bool,
    /// Remote-write destination, e.g.
    /// `https://victoriametrics.internal:8428/api/v1/write`.
    /// Required when enabled; validated like a webhook destination
    /// (HTTPS-only, no URL credentials, no link-local/unspecified IP).
    #[serde(default)]
    pub destination_url: Option<String>,
    /// Optional bearer token file for the destination (read once at
    /// worker start; the token never lives in the config file or the
    /// logs). 1..=4096 bytes.
    #[serde(default)]
    pub bearer_token_path: Option<PathBuf>,
    /// Optional PEM CA bundle for internal destinations; system roots
    /// when unset.
    #[serde(default)]
    pub ca_cert_path: Option<PathBuf>,
    /// Export pass cadence.
    #[serde(default = "default_export_interval_secs")]
    pub interval_secs: u64,
    /// Bounded catch-up window on restart or destination outage:
    /// samples older than this are skipped with a time-range
    /// warning, never replayed unboundedly — monitoring.db is
    /// disposable telemetry (the exact skipped count is deliberately
    /// not reported: counting would mean reading the data being
    /// discarded).
    #[serde(default = "default_export_max_backfill_secs")]
    pub max_backfill_secs: u64,
    /// Beyond this lag the exporter is marked degraded (warn + gauge);
    /// never blocks anything.
    #[serde(default = "default_export_lag_degraded_secs")]
    pub lag_degraded_secs: u64,
    /// The data filter: metric ids to export (the closed v1 registry
    /// vocabulary). REQUIRED when enabled — an enabled export with no
    /// selection is a boot error. Unknown ids are rejected when the
    /// exporter is constructed (registry-validated, loud at boot).
    #[serde(default)]
    pub metric_ids: Vec<String>,
    /// Optional target-kind filter (vocabulary: `node`, `vm`); empty
    /// means both. Re-validated against the registry at boot.
    #[serde(default)]
    pub target_kinds: Vec<String>,
    /// Optional source filter (vocabulary: the ingestion contract's
    /// snake_case source strings, e.g. `node_os`, `vmm`, `guest_agent`,
    /// `derived`); empty means every source the metric allows.
    #[serde(default)]
    pub sources: Vec<String>,
}

impl Default for MonitoringExportConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            destination_url: None,
            bearer_token_path: None,
            ca_cert_path: None,
            interval_secs: default_export_interval_secs(),
            max_backfill_secs: default_export_max_backfill_secs(),
            lag_degraded_secs: default_export_lag_degraded_secs(),
            metric_ids: Vec::new(),
            target_kinds: Vec::new(),
            sources: Vec::new(),
        }
    }
}

/// The target-kind vocabulary the export filter accepts (mirrors
/// `chv_monitoring_core::model::TargetKind::as_str` for the kinds that
/// carry monitoring series; the worker re-validates authoritatively).
pub const EXPORT_TARGET_KINDS: &[&str] = &["node", "vm"];
/// The source-filter vocabulary (mirrors
/// `chv_monitoring_core::model::Source::as_str`; the worker
/// re-validates authoritatively against the registry).
pub const EXPORT_SOURCES: &[&str] = &[
    "node_os",
    "vmm",
    "vm_cgroup",
    "storage_provider",
    "network_provider",
    "guest_agent",
    "derived",
];

impl MonitoringExportConfig {
    pub fn validate(&self) -> Result<(), String> {
        const MAX_METRIC_IDS: usize = 64;

        if !self.enabled {
            // Structural checks only when disabled? No — an operator
            // PRE-configuring the section should get the same loud
            // feedback; but a disabled section with no destination is
            // the shipped default, so require nothing then.
            return Ok(());
        }
        let Some(url) = &self.destination_url else {
            return Err(
                "monitoring.export.destination_url is required when export is enabled".to_string(),
            );
        };
        validate_notification_url("export.destination_url", url)?;

        if self.metric_ids.is_empty() {
            return Err(
                "monitoring.export.metric_ids is required when export is enabled — export is \
                 an egress surface, the data selection must be explicit (unknown metric ids \
                 are rejected at startup against the v1 registry)"
                    .to_string(),
            );
        }
        if self.metric_ids.len() > MAX_METRIC_IDS {
            return Err(format!(
                "monitoring.export.metric_ids must have at most {MAX_METRIC_IDS} entries, got {}",
                self.metric_ids.len()
            ));
        }
        if let Some(dup) = first_duplicate(&self.metric_ids) {
            return Err(format!(
                "monitoring.export.metric_ids contains {dup:?} twice"
            ));
        }

        for kind in &self.target_kinds {
            if !EXPORT_TARGET_KINDS.contains(&kind.as_str()) {
                return Err(format!(
                    "monitoring.export.target_kinds entry {kind:?} is not one of \
                     {EXPORT_TARGET_KINDS:?}"
                ));
            }
        }
        if let Some(dup) = first_duplicate(&self.target_kinds) {
            return Err(format!(
                "monitoring.export.target_kinds contains {dup:?} twice"
            ));
        }

        for source in &self.sources {
            if !EXPORT_SOURCES.contains(&source.as_str()) {
                return Err(format!(
                    "monitoring.export.sources entry {source:?} is not one of {EXPORT_SOURCES:?}"
                ));
            }
        }
        if let Some(dup) = first_duplicate(&self.sources) {
            return Err(format!("monitoring.export.sources contains {dup:?} twice"));
        }

        if !(5..=60).contains(&self.interval_secs) {
            return Err(format!(
                "monitoring.export.interval_secs must be 5..=60, got {}",
                self.interval_secs
            ));
        }
        if !(60..=3600).contains(&self.max_backfill_secs) {
            return Err(format!(
                "monitoring.export.max_backfill_secs must be 60..=3600, got {}",
                self.max_backfill_secs
            ));
        }
        if !(30..=3600).contains(&self.lag_degraded_secs) {
            return Err(format!(
                "monitoring.export.lag_degraded_secs must be 30..=3600, got {}",
                self.lag_degraded_secs
            ));
        }
        // The degraded threshold must sit inside the backfill window:
        // a lag marker beyond what the exporter would ever replay
        // could never fire.
        if self.lag_degraded_secs > self.max_backfill_secs {
            return Err(
                "monitoring.export.lag_degraded_secs must not exceed max_backfill_secs — the \
                 degraded marker must be reachable inside the bounded catch-up window"
                    .to_string(),
            );
        }

        if let Some(token_path) = &self.bearer_token_path {
            if !token_path.is_file() {
                return Err(format!(
                    "monitoring.export.bearer_token_path is not a readable file: {}",
                    token_path.display()
                ));
            }
            let len = std::fs::metadata(token_path).map(|m| m.len()).unwrap_or(0);
            if !(1..=4096).contains(&len) {
                return Err(format!(
                    "monitoring.export.bearer_token_path must point at a 1..=4096 byte file, \
                     got {len} bytes"
                ));
            }
        }
        if let Some(ca_path) = &self.ca_cert_path {
            if !ca_path.is_file() {
                return Err(format!(
                    "monitoring.export.ca_cert_path is not a readable file: {}",
                    ca_path.display()
                ));
            }
        }
        Ok(())
    }
}

fn first_duplicate(items: &[String]) -> Option<&str> {
    let mut seen = std::collections::BTreeSet::new();
    items
        .iter()
        .find(|i| !seen.insert(i.as_str()))
        .map(|i| i.as_str())
}

fn default_export_interval_secs() -> u64 {
    15
}
fn default_export_max_backfill_secs() -> u64 {
    600
}
fn default_export_lag_degraded_secs() -> u64 {
    120
}

impl Default for MonitoringConfig {
    fn default() -> Self {
        Self {
            enabled: default_monitoring_enabled(),
            database_url: default_monitoring_database_url(),
            migrations_dir: default_monitoring_migrations_dir(),
            max_connections: default_monitoring_max_connections(),
            maintenance_interval_secs: default_monitoring_maintenance_interval_secs(),
            raw_retention_hours: default_monitoring_raw_retention_hours(),
            rollup_5m_retention_days: default_monitoring_rollup_5m_retention_days(),
            rollup_1h_retention_days: default_monitoring_rollup_1h_retention_days(),
            max_series_per_target: default_monitoring_max_series_per_target(),
            min_headroom_mib: default_monitoring_min_headroom_mib(),
            max_db_gib: default_monitoring_max_db_gib(),
            batches_per_minute: default_monitoring_batches_per_minute(),
            guest_ingestion: MonitoringGuestIngestConfig::default(),
            alerting: MonitoringAlertingConfig::default(),
            notifications: MonitoringNotificationsConfig::default(),
            export: MonitoringExportConfig::default(),
        }
    }
}

fn default_monitoring_enabled() -> bool {
    true
}
fn default_monitoring_database_url() -> String {
    "sqlite:///var/lib/chv/monitoring/monitoring.db".to_string()
}
fn default_monitoring_migrations_dir() -> PathBuf {
    PathBuf::from("cmd/chv-controlplane/monitoring-migrations")
}
fn default_monitoring_max_connections() -> u32 {
    4
}
fn default_monitoring_maintenance_interval_secs() -> u64 {
    60
}
fn default_monitoring_raw_retention_hours() -> u64 {
    48
}
fn default_monitoring_rollup_5m_retention_days() -> u64 {
    30
}
fn default_monitoring_rollup_1h_retention_days() -> u64 {
    180
}
fn default_monitoring_max_series_per_target() -> i64 {
    1024
}
fn default_monitoring_min_headroom_mib() -> u64 {
    256
}
fn default_monitoring_max_db_gib() -> u64 {
    2
}
fn default_monitoring_batches_per_minute() -> u32 {
    20
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ControlPlaneTlsConfig {
    #[serde(default)]
    pub server_cert_path: Option<PathBuf>,
    #[serde(default)]
    pub server_key_path: Option<PathBuf>,
    #[serde(default)]
    pub client_ca_path: Option<PathBuf>,
    #[serde(default)]
    pub ca_cert_path: Option<PathBuf>,
    #[serde(default)]
    pub ca_key_path: Option<PathBuf>,
}

/// Control-plane half of the stord migration-authorization configuration
/// (issue #663). The signing secret is dedicated to migration tokens and
/// is deliberately separate from `jwt_secret`.
#[derive(Debug, Clone, Deserialize)]
pub struct ControlPlaneMigrationConfig {
    /// Dedicated HMAC secret used to mint short-lived migration
    /// authorizations. Prefer `signing_secret_file`; the value is never
    /// logged.
    #[serde(default)]
    pub signing_secret: Option<String>,
    /// File containing the dedicated migration-signing secret (trimmed).
    /// Mutually exclusive with `signing_secret`.
    #[serde(default)]
    pub signing_secret_file: Option<PathBuf>,
    /// Lifetime of a minted authorization. Default 300 s, hard-capped by
    /// the stord verifier at 900 s.
    #[serde(default = "default_migration_token_ttl_seconds")]
    pub token_ttl_seconds: u64,
    /// Explicit development-only escape hatch: dispatch migrations without
    /// minting tokens. Requires the destination stord to set the matching
    /// insecure flag too. NEVER enable this in production (default false,
    /// fail closed: a migration dispatch without a configured secret
    /// errors before contacting the agent).
    #[serde(default)]
    pub allow_insecure_unauthenticated: bool,
}

fn default_migration_token_ttl_seconds() -> u64 {
    300
}

impl Default for ControlPlaneMigrationConfig {
    fn default() -> Self {
        Self {
            signing_secret: None,
            signing_secret_file: None,
            token_ttl_seconds: default_migration_token_ttl_seconds(),
            allow_insecure_unauthenticated: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ControlPlaneConfig {
    pub grpc_bind: SocketAddr,
    pub http_bind: SocketAddr,
    /// Dedicated, loopback-by-default Prometheus listener (issue #657).
    ///
    /// The admin-gated `/metrics` route on `http_bind` is retained for
    /// backward compatibility, but scrapers should use this listener: it is
    /// unauthenticated and non-public, so scraping never needs a rotating
    /// admin JWT. Unset selects the daemon's loopback default
    /// (`127.0.0.1:9103`); an explicit value is honored verbatim and a bad
    /// value fails startup.
    #[serde(default)]
    pub metrics_bind: Option<String>,
    pub log_level: String,
    pub runtime_dir: PathBuf,
    #[serde(default = "default_jwt_secret")]
    pub jwt_secret: String,
    /// Per-purpose secrets (#662): user JWT, console JWT, agent bearer
    /// token, and credential-encryption key. Omitted values are derived
    /// from `jwt_secret` with domain separation so existing single-host
    /// installations keep working; explicit values are required for
    /// independent rotation.
    #[serde(default)]
    pub secrets: SecretsConfig,
    /// Production/development security switches (#662). Production is the
    /// default; the unauthenticated agent bypass is refused there.
    #[serde(default)]
    pub security: SecurityConfig,
    #[serde(default)]
    pub database: ControlPlaneDatabaseConfig,
    #[serde(default)]
    pub tls: ControlPlaneTlsConfig,
    /// Dedicated migration-authorization configuration (issue #663).
    #[serde(default)]
    pub migration: ControlPlaneMigrationConfig,
    #[serde(default = "default_agent_socket_pattern")]
    pub agent_socket_pattern: String,
    #[serde(default = "default_agent_runtime_dir")]
    pub agent_runtime_dir: PathBuf,
    #[serde(default = "default_kernel_path")]
    pub kernel_path: String,
    #[serde(default = "default_firmware_path")]
    pub firmware_path: String,
    /// VXLAN overlay network defaults for cluster-wide behavior.
    #[serde(default)]
    pub overlay: OverlayConfig,
    /// Static Web UI serving (issue #447, D3 target). Disabled by
    /// default — fail-closed.
    #[serde(default)]
    pub webui: WebUiConfig,
    /// Durable privileged-mutation audit log retention (#658).
    #[serde(default)]
    pub audit: AuditConfig,
    /// Native monitoring history (ADR-027, campaign #602 PR-2). The
    /// monitoring store is disposable telemetry in a separate SQLite
    /// file with its own pool; a failure to connect degrades monitoring
    /// and never aborts control-plane startup.
    #[serde(default)]
    pub monitoring: MonitoringConfig,
    /// Optional TLS on the HTTP (BFF + guest ingestion) listener
    /// (`[http_tls]`). When set, the listener serves HTTPS and may
    /// verify guest monitoring agent client certificates; when unset
    /// the listener is plain HTTP exactly as before, and guest
    /// ingestion stays disabled (the guest route requires TLS).
    #[serde(default)]
    pub http_tls: Option<HttpTlsConfig>,
}

/// TLS configuration for the manager's HTTP listener (BFF + guest
/// monitoring agent routes). Server identity only; client-certificate
/// verification for guest agents uses the dedicated agent CA from
/// `[monitoring.guest_ingestion]`.
#[derive(Debug, Clone, Deserialize)]
pub struct HttpTlsConfig {
    pub server_cert_path: PathBuf,
    pub server_key_path: PathBuf,
}

fn default_jwt_secret() -> String {
    "chv-dev-secret-change-in-production".to_string()
}

fn default_agent_runtime_dir() -> PathBuf {
    PathBuf::from("/var/lib/chv/agent")
}

#[derive(Debug, Clone, Deserialize)]
pub struct ControlPlaneDatabaseConfig {
    pub url: String,
    pub migrations_dir: PathBuf,
    #[serde(default = "default_controlplane_db_max_connections")]
    pub max_connections: u32,
    #[serde(default = "default_controlplane_db_min_connections")]
    pub min_connections: u32,
    #[serde(default = "default_controlplane_db_acquire_timeout_secs")]
    pub acquire_timeout_secs: u64,
    #[serde(default = "default_controlplane_db_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
    #[serde(default = "default_controlplane_db_max_lifetime_secs")]
    pub max_lifetime_secs: u64,
}

impl Default for ControlPlaneDatabaseConfig {
    fn default() -> Self {
        Self {
            url: DEFAULT_CONTROLPLANE_DATABASE_URL.to_string(),
            migrations_dir: PathBuf::from(DEFAULT_CONTROLPLANE_MIGRATIONS_DIR),
            max_connections: default_controlplane_db_max_connections(),
            min_connections: default_controlplane_db_min_connections(),
            acquire_timeout_secs: default_controlplane_db_acquire_timeout_secs(),
            idle_timeout_secs: default_controlplane_db_idle_timeout_secs(),
            max_lifetime_secs: default_controlplane_db_max_lifetime_secs(),
        }
    }
}

impl Default for ControlPlaneConfig {
    fn default() -> Self {
        Self {
            grpc_bind: DEFAULT_CONTROLPLANE_GRPC_BIND
                .parse()
                .expect("valid default grpc bind"),
            http_bind: DEFAULT_CONTROLPLANE_HTTP_BIND
                .parse()
                .expect("valid default http bind"),
            metrics_bind: None,
            log_level: DEFAULT_CONTROLPLANE_LOG_LEVEL.to_string(),
            runtime_dir: PathBuf::from(DEFAULT_CONTROLPLANE_RUNTIME_DIR),
            jwt_secret: default_jwt_secret(),
            secrets: SecretsConfig::default(),
            security: SecurityConfig::default(),
            database: ControlPlaneDatabaseConfig::default(),
            tls: ControlPlaneTlsConfig::default(),
            monitoring: MonitoringConfig::default(),
            http_tls: None,
            migration: ControlPlaneMigrationConfig::default(),
            agent_socket_pattern: default_agent_socket_pattern(),
            agent_runtime_dir: default_agent_runtime_dir(),
            kernel_path: default_kernel_path(),
            firmware_path: default_firmware_path(),
            overlay: OverlayConfig::default(),
            webui: WebUiConfig::default(),
            audit: AuditConfig::default(),
        }
    }
}

fn default_controlplane_db_max_connections() -> u32 {
    DEFAULT_CONTROLPLANE_DB_MAX_CONNECTIONS
}

fn default_controlplane_db_min_connections() -> u32 {
    DEFAULT_CONTROLPLANE_DB_MIN_CONNECTIONS
}

fn default_controlplane_db_acquire_timeout_secs() -> u64 {
    DEFAULT_CONTROLPLANE_DB_ACQUIRE_TIMEOUT_SECS
}

fn default_controlplane_db_idle_timeout_secs() -> u64 {
    DEFAULT_CONTROLPLANE_DB_IDLE_TIMEOUT_SECS
}

fn default_controlplane_db_max_lifetime_secs() -> u64 {
    DEFAULT_CONTROLPLANE_DB_MAX_LIFETIME_SECS
}

fn default_agent_socket_pattern() -> String {
    DEFAULT_CONTROLPLANE_AGENT_SOCKET_PATTERN.to_string()
}

fn default_kernel_path() -> String {
    DEFAULT_CONTROLPLANE_KERNEL_PATH.to_string()
}

fn default_firmware_path() -> String {
    DEFAULT_CONTROLPLANE_FIRMWARE_PATH.to_string()
}

pub fn load_controlplane_config(path: Option<&Path>) -> Result<ControlPlaneConfig, ConfigError> {
    let mut cfg = ControlPlaneConfig::default();
    if let Some(p) = path {
        let text = std::fs::read_to_string(p)?;
        cfg = toml::from_str(&text)?;
    }
    if cfg.jwt_secret == DEV_SECRET || cfg.jwt_secret.len() < 32 {
        cfg.jwt_secret = resolve_jwt_secret(&cfg.jwt_secret, "controlplane");
    }
    cfg.audit.validate().map_err(ConfigError::Invalid)?;
    cfg.validate_secrets()?;
    Ok(cfg)
}

impl ControlPlaneConfig {
    /// End-user session JWT signing key.
    pub fn effective_user_jwt_secret(&self) -> String {
        self.secrets.user_jwt(&self.jwt_secret)
    }

    /// Short-lived console JWT signing key. The agent must be configured
    /// with the same effective value.
    pub fn effective_console_jwt_secret(&self) -> String {
        self.secrets.console_jwt(&self.jwt_secret)
    }

    /// Bearer token presented on every agent RPC. Explicit configuration
    /// wins; otherwise it is derived from the legacy master. One value is
    /// sent to every agent this control plane serves, so all of those
    /// agents must share it. Per-node tokens are future ADR-029 work.
    pub fn effective_agent_auth_token(&self) -> String {
        self.secrets.agent_auth_token(&self.jwt_secret)
    }

    /// Explicit credential-encryption key, falling back to
    /// `CHV_ENCRYPTION_KEY`. `None` means the store keeps its legacy
    /// environment lookup (including the documented `CHV_JWT_SECRET`
    /// compatibility path) unchanged.
    pub fn effective_credential_encryption_key(&self) -> Option<String> {
        self.secrets.credential_encryption()
    }

    /// Fail-closed validation of the #662 secret split. Run by
    /// [`load_controlplane_config`]; public for programmatic configs.
    pub fn validate_secrets(&self) -> Result<(), ConfigError> {
        validate_security_mode(&self.security)?;
        self.secrets.validate_explicit(true)?;
        let user = self.effective_user_jwt_secret();
        let console = self.effective_console_jwt_secret();
        let agent = self.effective_agent_auth_token();
        if !self.security.allow_insecure_agent_auth && secret_is_weak(&agent) {
            return Err(ConfigError::Invalid(
                "secrets.agent_auth_token (effective) must be at least 32 characters".to_string(),
            ));
        }
        let credential = self.effective_credential_encryption_key();
        let mut all = vec![
            ("secrets.user_jwt (effective)", user.as_str()),
            ("secrets.console_jwt (effective)", console.as_str()),
            ("secrets.agent_auth_token (effective)", agent.as_str()),
        ];
        if let Some(ref key) = credential {
            all.push(("secrets.credential_encryption (effective)", key.as_str()));
        }
        ensure_distinct_secrets(&all)?;
        if self.security.production
            && explicit_secret(&self.secrets.user_jwt).is_none()
            && explicit_secret(&self.secrets.console_jwt).is_none()
        {
            tracing::warn!(
                "user/console JWT keys are derived from the legacy jwt_secret; \
                 set [secrets] user_jwt/console_jwt for independent rotation"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alerting_and_notifications_default_to_no_op() {
        let monitoring = MonitoringConfig::default();
        // Alerting: on by default but inert with zero rules.
        assert!(monitoring.alerting.enabled);
        assert_eq!(monitoring.alerting.evaluation_interval_secs, 15);
        assert_eq!(monitoring.alerting.max_rules, 200);
        assert!(monitoring.alerting.validate().is_ok());
        // Notifications: nothing configured — no destination, no
        // secret requirement, valid.
        assert!(!monitoring.notifications.has_destination());
        assert!(monitoring.notifications.webhook_url.is_none());
        assert!(monitoring.notifications.webhook_signing_secret.is_empty());
        assert!(monitoring.notifications.validate().is_ok());
    }

    #[test]
    fn alerting_sections_parse_from_toml() {
        let raw = r#"
[monitoring.alerting]
enabled = false
evaluation_interval_secs = 30
max_rules = 50

[monitoring.notifications]
webhook_url = "https://alerts.example.internal/hooks/chv"
webhook_signing_secret = "0123456789abcdef0123456789abcdef"
max_attempts = 4
dispatch_interval_secs = 10
max_batch = 5
"#;
        let value: toml::Value = toml::from_str(raw).expect("parse toml");
        let alerting: MonitoringAlertingConfig = value
            .get("monitoring")
            .unwrap()
            .get("alerting")
            .map(|v| MonitoringAlertingConfig::deserialize(v.clone()).expect("alerting section"))
            .unwrap_or_default();
        assert!(!alerting.enabled);
        assert_eq!(alerting.evaluation_interval_secs, 30);
        assert_eq!(alerting.max_rules, 50);

        let notifications: MonitoringNotificationsConfig = value
            .get("monitoring")
            .unwrap()
            .get("notifications")
            .map(|v| {
                MonitoringNotificationsConfig::deserialize(v.clone())
                    .expect("notifications section")
            })
            .unwrap_or_default();
        assert!(notifications.has_destination());
        assert_eq!(
            notifications.webhook_url.as_deref(),
            Some("https://alerts.example.internal/hooks/chv")
        );
        assert_eq!(notifications.webhook_signing_secret.as_str().len(), 32);
        assert_eq!(notifications.max_attempts, 4);
        assert_eq!(notifications.dispatch_interval_secs, 10);
        assert_eq!(notifications.max_batch, 5);
        assert!(notifications.validate().is_ok());
    }

    #[test]
    fn notification_secret_is_debug_redacted() {
        let cfg = MonitoringNotificationsConfig {
            webhook_signing_secret: NotificationSecret(
                "super-secret-value-never-in-logs".to_string(),
            ),
            ..Default::default()
        };
        let rendered = format!("{cfg:?}");
        assert!(
            !rendered.contains("super-secret-value"),
            "secret leaked: {rendered}"
        );
        assert!(rendered.contains("[redacted]"));
    }

    #[test]
    fn notifications_validation_rejects_bad_destinations() {
        let secret = NotificationSecret("0123456789abcdef0123456789abcdef".to_string());
        let base = || MonitoringNotificationsConfig {
            webhook_signing_secret: secret.clone(),
            ..Default::default()
        };

        // Plain HTTP is rejected.
        let mut cfg = base();
        cfg.webhook_url = Some("http://alerts.example.internal/hook".into());
        assert!(cfg.validate().is_err());

        // Credentials in the URL are rejected.
        let mut cfg = base();
        cfg.webhook_url = Some("https://user:pass@alerts.example.internal/hook".into());
        assert!(cfg.validate().is_err());

        // Link-local (cloud metadata) is rejected.
        let mut cfg = base();
        cfg.webhook_url = Some("https://169.254.169.254/latest/meta-data".into());
        assert!(cfg.validate().is_err());

        // …in every encoding form the WHATWG URL parser accepts: the
        // same parser reqwest uses at send time, so none of these
        // can slip past boot validation and still be dialed.
        for bypass in [
            "https://2852039166/hook",               // integer 169.254.169.254
            "https://0xa9fea9fe/hook",               // hex 169.254.169.254
            "https://[::ffff:169.254.169.254]/hook", // IPv4-mapped
            "https://[::169.254.169.254]/hook",      // IPv4-compatible
            "https://[fe80::1]/hook",                // link-local range, not just fe80::
            "https://[::]/hook",                     // unspecified v6
            "https://255.255.255.255/hook",          // broadcast
        ] {
            let mut cfg = base();
            cfg.webhook_url = Some(bypass.into());
            let err = cfg.validate().unwrap_err();
            assert!(
                err.contains("link-local") || err.contains("unspecified"),
                "{bypass} must be rejected as a metadata/SSRF hazard: {err}"
            );
        }

        // Unspecified address is rejected.
        let mut cfg = base();
        cfg.webhook_url = Some("https://0.0.0.0/hook".into());
        assert!(cfg.validate().is_err());

        // A destination without a signing secret is rejected.
        let cfg = MonitoringNotificationsConfig {
            webhook_url: Some("https://alerts.example.internal/hook".into()),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("webhook_signing_secret"), "{err}");

        // A too-short secret is rejected.
        let cfg = MonitoringNotificationsConfig {
            webhook_url: Some("https://alerts.example.internal/hook".into()),
            webhook_signing_secret: NotificationSecret("short".to_string()),
            ..Default::default()
        };
        assert!(cfg.validate().is_err());

        // Loopback, private hosts and ports are legitimate internal
        // destinations for an operator-configured webhook.
        let mut cfg = base();
        cfg.webhook_url = Some("https://127.0.0.1:9443/hook".into());
        assert!(cfg.validate().is_ok());
        let mut cfg = base();
        cfg.webhook_url = Some("https://10.1.2.3/hook".into());
        assert!(cfg.validate().is_ok());
        let mut cfg = base();
        cfg.slack_webhook_url = Some("https://hooks.slack.com/services/T/B/X".into());
        assert!(cfg.validate().is_ok());

        // Range checks.
        let mut cfg = base();
        cfg.max_attempts = 21;
        assert!(cfg.validate().is_err());
        let mut cfg = base();
        cfg.dispatch_interval_secs = 0;
        assert!(cfg.validate().is_err());
        let mut cfg = base();
        cfg.max_batch = 51;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn alerting_validation_rejects_out_of_range() {
        let cfg = MonitoringAlertingConfig {
            evaluation_interval_secs: 4,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
        let cfg = MonitoringAlertingConfig {
            evaluation_interval_secs: 61,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
        let cfg = MonitoringAlertingConfig {
            evaluation_interval_secs: 60,
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
        let cfg = MonitoringAlertingConfig {
            max_rules: 0,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
        let cfg = MonitoringAlertingConfig {
            max_rules: 501,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
        let cfg = MonitoringAlertingConfig {
            max_rules: 500,
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn example_controlplane_toml_documents_alerting_sections() {
        // Drift guard: the shipped example documents the alerting and
        // notifications sections with their no-op-by-default contract.
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/examples/controlplane.toml"
        );
        let config = load_controlplane_config(Some(std::path::Path::new(path)))
            .expect("docs/examples/controlplane.toml must parse");
        assert!(config.monitoring.alerting.enabled);
        assert_eq!(config.monitoring.alerting.evaluation_interval_secs, 15);
        assert_eq!(config.monitoring.alerting.max_rules, 200);
        // No destination in the shipped example: notifications off.
        assert!(!config.monitoring.notifications.has_destination());
        assert!(config.monitoring.notifications.webhook_url.is_none());
        assert_eq!(config.monitoring.notifications.max_attempts, 8);
        assert!(config.monitoring.notifications.validate().is_ok());
        assert!(config.monitoring.alerting.validate().is_ok());
    }

    #[test]
    fn example_controlplane_toml_parses_with_guest_ingestion_sections() {
        // Drift guard for docs/examples/controlplane.toml: the shipped
        // example must stay loadable as the real control-plane config
        // (guest ingestion + http_tls sections included, ADR-026).
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/examples/controlplane.toml"
        );
        let config = load_controlplane_config(Some(std::path::Path::new(path)))
            .expect("docs/examples/controlplane.toml must parse");
        let guest = &config.monitoring.guest_ingestion;
        // The example documents the disabled-by-default contract.
        assert!(!guest.enabled);
        assert_eq!(guest.credential_ttl_days, 30);
        assert!(guest.agent_ca_cert_path.is_some());
        assert!(config.http_tls.is_some(), "example documents [http_tls]");
    }

    #[test]
    fn watchdog_section_parses_with_defaults_and_overrides() {
        let base = r#"
socket_path = "/run/chv/agent/api.sock"
runtime_dir = "/var/lib/chv/agent"
log_level = "info"
control_plane_addr = "https://localhost:8443"
stord_socket = "/run/chv/stord/api.sock"
nwd_socket = "/run/chv/nwd/api.sock"
chv_binary_path = "/usr/bin/cloud-hypervisor"
stord_binary_path = "/usr/bin/chv-stord"
nwd_binary_path = "/usr/bin/chv-nwd"
cache_path = "/var/lib/chv/cache/agent-cache.json"
node_id = "test-node"
jwt_secret = "0123456789abcdef0123456789abcdef"
"#;
        // Without the section: disabled with the documented defaults —
        // existing agent.toml files are unaffected.
        let cfg = load_agent_config_from_str(base).expect("parse without section");
        assert!(!cfg.watchdog.enabled);
        assert_eq!(cfg.watchdog.boot_marker, "systemd-logind");
        assert_eq!(cfg.watchdog.stall_secs, 120);
        assert_eq!(cfg.watchdog.max_reboots, 2);
        assert_eq!(cfg.watchdog.healthy_reset_secs, 900);

        // With the section: every field overridable (a non-systemd
        // guest image needs a custom marker).
        let cfg = load_agent_config_from_str(&format!(
            "{base}\n[watchdog]\nenabled = true\nboot_marker = \"login:\"\nstall_secs = 45\nmax_reboots = 1\nhealthy_reset_secs = 300\n"
        ))
        .expect("parse with section");
        assert!(cfg.watchdog.enabled);
        assert_eq!(cfg.watchdog.boot_marker, "login:");
        assert_eq!(cfg.watchdog.stall_secs, 45);
        assert_eq!(cfg.watchdog.max_reboots, 1);
        assert_eq!(cfg.watchdog.healthy_reset_secs, 300);
    }

    #[test]
    fn agent_stord_config_path_defaults_none_and_parses() {
        // #385: stord_config_path is an explicit opt-in — absent means
        // today's generated-config respawn, byte-exactly.
        let base = r#"
socket_path = "/run/chv/agent/api.sock"
runtime_dir = "/var/lib/chv/agent"
log_level = "info"
control_plane_addr = "https://localhost:8443"
stord_socket = "/run/chv/stord/api.sock"
nwd_socket = "/run/chv/nwd/api.sock"
chv_binary_path = "/usr/bin/cloud-hypervisor"
stord_binary_path = "/usr/bin/chv-stord"
nwd_binary_path = "/usr/bin/chv-nwd"
cache_path = "/var/lib/chv/cache/agent-cache.json"
node_id = "test-node"
jwt_secret = "0123456789abcdef0123456789abcdef"
"#;
        let cfg = load_agent_config_from_str(base).expect("parse without key");
        assert_eq!(cfg.stord_config_path, None);

        let cfg = load_agent_config_from_str(&format!(
            "{base}\nstord_config_path = \"/etc/chv/stord.toml\"\n"
        ))
        .expect("parse with key");
        assert_eq!(
            cfg.stord_config_path,
            Some(PathBuf::from("/etc/chv/stord.toml"))
        );
    }

    #[test]
    fn agent_nwd_config_path_defaults_none_and_parses() {
        // #504: nwd_config_path is an explicit opt-in — absent means
        // today's generated-config respawn, byte-exactly (mirrors the
        // #385 stord_config_path contract).
        let base = r#"
socket_path = "/run/chv/agent/api.sock"
runtime_dir = "/var/lib/chv/agent"
log_level = "info"
control_plane_addr = "https://localhost:8443"
stord_socket = "/run/chv/stord/api.sock"
nwd_socket = "/run/chv/nwd/api.sock"
chv_binary_path = "/usr/bin/cloud-hypervisor"
stord_binary_path = "/usr/bin/chv-stord"
nwd_binary_path = "/usr/bin/chv-nwd"
cache_path = "/var/lib/chv/cache/agent-cache.json"
node_id = "test-node"
jwt_secret = "0123456789abcdef0123456789abcdef"
"#;
        let cfg = load_agent_config_from_str(base).expect("parse without key");
        assert_eq!(cfg.nwd_config_path, None);

        let cfg = load_agent_config_from_str(&format!(
            "{base}\nnwd_config_path = \"/etc/chv/nwd.toml\"\n"
        ))
        .expect("parse with key");
        assert_eq!(
            cfg.nwd_config_path,
            Some(PathBuf::from("/etc/chv/nwd.toml"))
        );
    }

    fn load_agent_config_from_str(
        contents: &str,
    ) -> Result<AgentConfig, Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("agent.toml");
        std::fs::write(&config_path, contents).expect("write config");
        Ok(load_agent_config(Some(&config_path))?)
    }

    #[test]
    fn load_agent_config_auto_generates_secret_when_default() {
        let cfg = load_agent_config(None).expect("should succeed with auto-generated secret");
        assert_ne!(cfg.jwt_secret, "chv-dev-secret-change-in-production");
        assert!(
            cfg.jwt_secret.len() >= 32,
            "auto-generated secret should be at least 32 chars"
        );
    }

    #[test]
    fn load_agent_config_auto_generates_secret_when_short() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("agent.toml");
        std::fs::write(
            &config_path,
            r#"
socket_path = "/run/chv/agent/api.sock"
runtime_dir = "/var/lib/chv/agent"
log_level = "info"
control_plane_addr = "https://localhost:8443"
stord_socket = "/run/chv/stord/api.sock"
nwd_socket = "/run/chv/nwd/api.sock"
chv_binary_path = "/usr/bin/cloud-hypervisor"
stord_binary_path = "/usr/bin/chv-stord"
nwd_binary_path = "/usr/bin/chv-nwd"
cache_path = "/var/lib/chv/cache/agent-cache.json"
node_id = "test-node"
jwt_secret = "tooshort"
"#,
        )
        .expect("write config");

        let cfg = load_agent_config(Some(&config_path))
            .expect("should succeed with auto-generated secret");
        assert_ne!(cfg.jwt_secret, "tooshort");
        assert!(cfg.jwt_secret.len() >= 32);
        assert_eq!(
            cfg.core_store_path,
            PathBuf::from("/var/lib/chv/agent/core.db")
        );
        assert_eq!(
            cfg.core_api_socket_path,
            PathBuf::from("/run/chv/core/core-v1.sock")
        );
    }

    #[test]
    fn controlplane_webui_section_defaults_disabled_and_parses_overrides() {
        // #447: the [webui] section is an explicit opt-in — absent
        // means disabled with the packaged-tree default dir, so every
        // existing controlplane.toml is unaffected (fail-closed).
        let cfg = load_controlplane_config(None).expect("default controlplane config");
        assert!(!cfg.webui.enabled);
        assert_eq!(cfg.webui.dir, PathBuf::from("/usr/share/chv/ui"));

        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("controlplane.toml");
        std::fs::write(
            &config_path,
            r#"
grpc_bind = "127.0.0.1:8443"
http_bind = "127.0.0.1:8080"
log_level = "info"
runtime_dir = "/run/chv/controlplane"
jwt_secret = "a]Kx8v2mN!pR7qYsW3dF6gH9jL0nBcTe"

[webui]
enabled = true
dir = "/opt/chv/ui"
"#,
        )
        .expect("write config");

        let cfg = load_controlplane_config(Some(&config_path)).expect("parse with [webui] section");
        assert!(cfg.webui.enabled);
        assert_eq!(cfg.webui.dir, PathBuf::from("/opt/chv/ui"));

        // Invalid: a non-path value for `dir` is a parse error, not a
        // silent default (fail-closed on misconfiguration).
        std::fs::write(
            &config_path,
            r#"
grpc_bind = "127.0.0.1:8443"
http_bind = "127.0.0.1:8080"
log_level = "info"
runtime_dir = "/run/chv/controlplane"
jwt_secret = "a]Kx8v2mN!pR7qYsW3dF6gH9jL0nBcTe"

[webui]
enabled = true
dir = 1234
"#,
        )
        .expect("write config");
        assert!(load_controlplane_config(Some(&config_path)).is_err());
    }

    #[test]
    fn load_controlplane_config_auto_generates_secret_when_default() {
        let cfg =
            load_controlplane_config(None).expect("should succeed with auto-generated secret");
        assert_ne!(cfg.jwt_secret, "chv-dev-secret-change-in-production");
        assert!(cfg.jwt_secret.len() >= 32);
    }

    #[test]
    fn load_controlplane_config_reads_explicit_values() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("controlplane.toml");
        std::fs::write(
            &config_path,
            r#"
grpc_bind = "0.0.0.0:9443"
http_bind = "0.0.0.0:9080"
log_level = "debug"
runtime_dir = "/tmp/chv-controlplane"
jwt_secret = "a]Kx8v2mN!pR7qYsW3dF6gH9jL0nBcTe"

[tls]
server_cert_path = "/tmp/server.crt"
server_key_path = "/tmp/server.key"
client_ca_path = "/tmp/ca.crt"

[database]
url = "sqlite:///tmp/test.db"
migrations_dir = "custom/migrations"
max_connections = 32
min_connections = 2
acquire_timeout_secs = 7
idle_timeout_secs = 90
max_lifetime_secs = 1200
"#,
        )
        .expect("write config");

        let config = load_controlplane_config(Some(&config_path)).expect("config should load");
        assert_eq!(
            config.grpc_bind,
            "0.0.0.0:9443".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            config.http_bind,
            "0.0.0.0:9080".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(config.log_level, "debug");
        assert_eq!(config.runtime_dir, PathBuf::from("/tmp/chv-controlplane"));
        assert_eq!(config.database.url, "sqlite:///tmp/test.db");
        assert_eq!(
            config.database.migrations_dir,
            PathBuf::from("custom/migrations")
        );
        assert_eq!(config.database.max_connections, 32);
        assert_eq!(config.database.min_connections, 2);
        assert_eq!(config.database.acquire_timeout_secs, 7);
        assert_eq!(config.database.idle_timeout_secs, 90);
        assert_eq!(config.database.max_lifetime_secs, 1200);
        assert_eq!(
            config.tls.server_cert_path,
            Some(PathBuf::from("/tmp/server.crt"))
        );
        assert_eq!(
            config.tls.server_key_path,
            Some(PathBuf::from("/tmp/server.key"))
        );
        assert_eq!(
            config.tls.client_ca_path,
            Some(PathBuf::from("/tmp/ca.crt"))
        );
    }

    #[test]
    fn agent_authority_mode_is_strict_and_defaults_legacy() {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            authority_mode: AgentAuthorityMode,
        }
        let defaulted: Wrapper = toml::from_str("").unwrap();
        assert_eq!(defaulted.authority_mode, AgentAuthorityMode::Legacy);
        let native: Wrapper = toml::from_str("authority_mode = 'core-native'").unwrap();
        assert_eq!(native.authority_mode, AgentAuthorityMode::CoreNative);
        assert!(toml::from_str::<Wrapper>("authority_mode = 'core'").is_err());
    }

    #[test]
    fn core_managed_also_materializes_console_jwt_secret() {
        // Core-managed agents run the console server on this secret; a
        // default or short secret must not survive config load.
        let mut config = AgentConfig {
            authority_mode: AgentAuthorityMode::CoreManaged,
            jwt_secret: "short".to_owned(),
            ..AgentConfig::default()
        };
        materialize_agent_jwt_secret(&mut config);
        assert_eq!(config.authority_mode, AgentAuthorityMode::CoreManaged);
        assert!(config.jwt_secret.len() >= 32);
        assert_ne!(config.jwt_secret, "short");
    }

    #[test]
    fn nwd_config_fabric_defaults_and_overrides() {
        let defaulted = load_nwd_config(None).expect("default nwd config");
        assert!(!defaulted.fabric.enabled);
        assert_eq!(
            defaulted.fabric.state_dir,
            PathBuf::from("/var/lib/chv/nwd/fabric")
        );
        assert_eq!(defaulted.fabric.name_prefix, "chv");
        assert_eq!(defaulted.fabric.wireguard_port, 65001);
        assert_eq!(defaulted.fabric.vxlan_port, 4789);
        assert_eq!(defaulted.fabric.default_tenant_mtu, 1380);
        assert_eq!(defaulted.fabric.default_fabric_mtu, 1440);

        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("nwd.toml");
        std::fs::write(
            &config_path,
            r#"
socket_path = "/run/chv/nwd/api.sock"
runtime_dir = "/run/chv/nwd"
log_level = "info"

[fabric]
enabled = true
state_dir = "/var/lib/chv/nwd/fabric"
name_prefix = "chv"
wireguard_port = 65002
vxlan_port = 4790
default_tenant_mtu = 1370
default_fabric_mtu = 1430
"#,
        )
        .expect("write config");

        let cfg = load_nwd_config(Some(&config_path)).expect("nwd config");
        assert!(cfg.fabric.enabled);
        assert_eq!(cfg.fabric.wireguard_port, 65002);
        assert_eq!(cfg.fabric.vxlan_port, 4790);
        assert_eq!(cfg.fabric.default_tenant_mtu, 1370);
        assert_eq!(cfg.fabric.default_fabric_mtu, 1430);
    }

    #[test]
    fn core_native_does_not_materialize_unused_jwt_secret() {
        // Core-native returns into run_core_native before the console server
        // is constructed: there is no in-process consumer, so the configured
        // value is stored untouched and nothing is minted on disk.
        let mut config = AgentConfig {
            authority_mode: AgentAuthorityMode::CoreNative,
            jwt_secret: "short".to_owned(),
            ..AgentConfig::default()
        };
        materialize_agent_jwt_secret(&mut config);
        assert_eq!(config.authority_mode, AgentAuthorityMode::CoreNative);
        assert_eq!(config.jwt_secret, "short");
    }

    #[test]
    fn stord_migration_receiver_fields_parse_with_defaults_and_overrides() {
        let base = r#"
socket_path = "/run/chv/stord/api.sock"
runtime_dir = "/var/lib/chv/storage/localdisk"
log_level = "info"
"#;

        // Without any receiver fields (and even with the client half set):
        // the four receiver fields stay raw Option::None — validation is the
        // startup loader's job, not this crate's.
        let cfg = load_stord_config_from_str(base).expect("parse without receiver fields");
        assert!(!cfg.migration.enabled);
        assert_eq!(cfg.migration.listen_addr, None);
        assert_eq!(cfg.migration.server_cert_path, None);
        assert_eq!(cfg.migration.server_key_path, None);
        assert_eq!(cfg.migration.client_ca_path, None);
        // Issue #663 defaults: no authorization material, fail-closed
        // insecure flag off, conservative admission limits.
        assert_eq!(cfg.migration.node_id, None);
        assert_eq!(cfg.migration.signing_secret, None);
        assert_eq!(cfg.migration.signing_secret_file, None);
        assert!(!cfg.migration.allow_insecure_unauthenticated);
        assert_eq!(cfg.migration.max_inflight_receiving, 2);
        assert_eq!(
            cfg.migration.max_inflight_receiving_bytes,
            2 * 1024 * 1024 * 1024 * 1024
        );

        // With receiver fields set: every field round-trips as configured
        // (listen_addr stays a String; it is parsed by the fail-closed
        // startup loader, so even an invalid value must parse here).
        let cfg = load_stord_config_from_str(
            r#"
socket_path = "/run/chv/stord/api.sock"
runtime_dir = "/var/lib/chv/storage/localdisk"
log_level = "info"

[migration]
enabled = true
client_cert_path = "/etc/chv/tls/stord-client.crt"
client_key_path = "/etc/chv/tls/stord-client.key"
ca_cert_path = "/etc/chv/tls/chv-ca.crt"
dest_server_name = "stord-peer.internal"
listen_addr = "127.0.0.1:50052"
server_cert_path = "/etc/chv/tls/stord-server.crt"
server_key_path = "/etc/chv/tls/stord-server.key"
client_ca_path = "/etc/chv/tls/chv-ca.crt"
node_id = "node-b"
signing_secret_file = "/etc/chv/migration/signing-secret"
max_inflight_receiving = 5
max_inflight_receiving_bytes = 1099511627776
"#,
        )
        .expect("parse with receiver fields");
        assert!(cfg.migration.enabled);
        assert_eq!(
            cfg.migration.listen_addr.as_deref(),
            Some("127.0.0.1:50052")
        );
        assert_eq!(
            cfg.migration.server_cert_path,
            Some(PathBuf::from("/etc/chv/tls/stord-server.crt"))
        );
        assert_eq!(
            cfg.migration.server_key_path,
            Some(PathBuf::from("/etc/chv/tls/stord-server.key"))
        );
        assert_eq!(
            cfg.migration.client_ca_path,
            Some(PathBuf::from("/etc/chv/tls/chv-ca.crt"))
        );
        assert_eq!(cfg.migration.node_id.as_deref(), Some("node-b"));
        assert_eq!(
            cfg.migration.signing_secret_file,
            Some(PathBuf::from("/etc/chv/migration/signing-secret"))
        );
        assert_eq!(cfg.migration.max_inflight_receiving, 5);
        assert_eq!(
            cfg.migration.max_inflight_receiving_bytes,
            1_099_511_627_776
        );
    }

    #[test]
    fn controlplane_migration_authorization_config_defaults_and_overrides() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("controlplane.toml");
        std::fs::write(
            &config_path,
            r#"
grpc_bind = "127.0.0.1:8443"
http_bind = "127.0.0.1:8080"
log_level = "info"
runtime_dir = "/run/chv/controlplane"
jwt_secret = "a]Kx8v2mN!pR7qYsW3dF6gH9jL0nBcTe"

[database]
url = "sqlite:///tmp/test.db"
migrations_dir = "migrations"

[migration]
signing_secret_file = "/etc/chv/migration/signing-secret"
token_ttl_seconds = 120
allow_insecure_unauthenticated = false
"#,
        )
        .expect("write config");

        let config = load_controlplane_config(Some(&config_path)).expect("config should load");
        assert_eq!(
            config.migration.signing_secret_file,
            Some(PathBuf::from("/etc/chv/migration/signing-secret"))
        );
        assert_eq!(config.migration.token_ttl_seconds, 120);
        assert!(!config.migration.allow_insecure_unauthenticated);

        let defaults = ControlPlaneMigrationConfig::default();
        assert_eq!(defaults.signing_secret, None);
        assert_eq!(defaults.signing_secret_file, None);
        assert_eq!(defaults.token_ttl_seconds, 300);
        assert!(!defaults.allow_insecure_unauthenticated);
    }

    fn load_stord_config_from_str(
        contents: &str,
    ) -> Result<StordConfig, Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("stord.toml");
        std::fs::write(&config_path, contents).expect("write config");
        Ok(load_stord_config(Some(&config_path))?)
    }

    #[test]
    fn export_defaults_to_off_and_valid() {
        let monitoring = MonitoringConfig::default();
        assert!(!monitoring.export.enabled);
        assert!(monitoring.export.destination_url.is_none());
        assert!(monitoring.export.metric_ids.is_empty());
        assert!(monitoring.export.validate().is_ok());
    }

    #[test]
    fn export_section_parses_from_toml() {
        let raw = r#"
[monitoring.export]
enabled = true
destination_url = "https://victoriametrics.internal:8428/api/v1/write"
interval_secs = 30
max_backfill_secs = 900
lag_degraded_secs = 180
metric_ids = ["vm.cpu.capacity_ratio", "vm.memory.guest_available_bytes"]
target_kinds = ["vm"]
sources = ["vmm", "derived"]
"#;
        let value: toml::Value = toml::from_str(raw).expect("parse toml");
        let export: MonitoringExportConfig = value
            .get("monitoring")
            .unwrap()
            .get("export")
            .map(|v| MonitoringExportConfig::deserialize(v.clone()).expect("export section"))
            .unwrap_or_default();
        assert!(export.enabled);
        assert_eq!(
            export.destination_url.as_deref(),
            Some("https://victoriametrics.internal:8428/api/v1/write")
        );
        assert_eq!(export.metric_ids.len(), 2);
        assert_eq!(export.target_kinds, vec!["vm".to_string()]);
        assert!(export.validate().is_ok());
    }

    #[test]
    fn export_rejects_unknown_fields() {
        let raw = r#"
[monitoring.export]
enabled = true
destination_url = "https://vm.internal:8428/api/v1/write"
metric_ids = ["vm.cpu.capacity_ratio"]
nonsense = true
"#;
        let value: toml::Value = toml::from_str(raw).expect("parse toml");
        let err = MonitoringExportConfig::deserialize(
            value
                .get("monitoring")
                .unwrap()
                .get("export")
                .unwrap()
                .clone(),
        )
        .expect_err("unknown field must be rejected");
        assert!(err.to_string().contains("nonsense"), "{err}");
    }

    fn export_config_for_validate() -> MonitoringExportConfig {
        MonitoringExportConfig {
            enabled: true,
            destination_url: Some("https://victoriametrics.internal:8428/api/v1/write".into()),
            metric_ids: vec!["vm.cpu.capacity_ratio".into()],
            ..Default::default()
        }
    }

    #[test]
    fn export_requires_destination_and_selection_when_enabled() {
        // No destination.
        let mut cfg = export_config_for_validate();
        cfg.destination_url = None;
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("destination_url"), "{err}");
        // No metric selection: an egress surface must be explicit.
        let mut cfg = export_config_for_validate();
        cfg.metric_ids = Vec::new();
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("metric_ids"), "{err}");
        // Disabled sections are exempt (the shipped default).
        let mut cfg = export_config_for_validate();
        cfg.enabled = false;
        cfg.destination_url = None;
        cfg.metric_ids = Vec::new();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn export_rejects_unsafe_destinations() {
        let mut cfg = export_config_for_validate();
        cfg.destination_url = Some("http://victoriametrics.internal:8428/api/v1/write".into());
        assert!(cfg.validate().unwrap_err().contains("https://"));
        // Cloud-metadata hazard, same rule as notifications.
        let mut cfg = export_config_for_validate();
        cfg.destination_url = Some("https://169.254.169.254/api/v1/write".into());
        assert!(cfg.validate().unwrap_err().contains("link-local"));
        // Credentials in the URL.
        let mut cfg = export_config_for_validate();
        cfg.destination_url =
            Some("https://user:secret@victoriametrics.internal:8428/api/v1/write".into());
        assert!(cfg.validate().unwrap_err().contains("credentials"));
        // Internal private/loopback receivers are legitimate.
        let mut cfg = export_config_for_validate();
        cfg.destination_url = Some("https://127.0.0.1:8428/api/v1/write".into());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn export_rejects_bad_filters_and_bounds() {
        let mut cfg = export_config_for_validate();
        cfg.target_kinds = vec!["volume".into()];
        assert!(cfg.validate().unwrap_err().contains("target_kinds"));
        let mut cfg = export_config_for_validate();
        cfg.sources = vec!["guest".into()];
        assert!(cfg.validate().unwrap_err().contains("sources"));
        let mut cfg = export_config_for_validate();
        cfg.metric_ids = vec!["a".into(), "a".into()];
        assert!(cfg.validate().unwrap_err().contains("twice"));
        let mut cfg = export_config_for_validate();
        cfg.interval_secs = 1;
        assert!(cfg.validate().unwrap_err().contains("interval_secs"));
        let mut cfg = export_config_for_validate();
        cfg.lag_degraded_secs = 3600;
        cfg.max_backfill_secs = 300;
        // The degraded marker must be reachable inside the catch-up
        // window, or it could never fire.
        assert!(cfg.validate().unwrap_err().contains("lag_degraded_secs"));
        let mut cfg = export_config_for_validate();
        cfg.metric_ids = (0..65).map(|i| format!("m{i}")).collect();
        assert!(cfg.validate().unwrap_err().contains("at most 64"));
    }

    #[test]
    fn export_bearer_token_and_ca_paths_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("token");
        std::fs::write(&token, "sufficiently-long-token-material").unwrap();

        let mut cfg = export_config_for_validate();
        cfg.bearer_token_path = Some(dir.path().join("missing"));
        assert!(cfg.validate().unwrap_err().contains("bearer_token_path"));

        cfg.bearer_token_path = Some(token.clone());
        assert!(cfg.validate().is_ok());

        // Empty token file: an empty Authorization header is never a
        // real deployment shape.
        std::fs::write(&token, "").unwrap();
        assert!(cfg.validate().unwrap_err().contains("bearer_token_path"));

        let mut cfg = export_config_for_validate();
        cfg.ca_cert_path = Some(dir.path().join("missing-ca.pem"));
        assert!(cfg.validate().unwrap_err().contains("ca_cert_path"));
    }

    // ---------------------------------------------------------------------
    // #662: per-purpose secrets and fail-closed agent authentication.
    // ---------------------------------------------------------------------

    const TEST_MASTER: &str = "legacy-master-0123456789abcdef0123456789abcdef";

    fn control_plane_with_master(master: &str) -> ControlPlaneConfig {
        ControlPlaneConfig {
            jwt_secret: master.to_string(),
            ..Default::default()
        }
    }

    fn agent_with_master(master: &str) -> AgentConfig {
        AgentConfig {
            jwt_secret: master.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn legacy_master_derives_distinct_stable_per_purpose_secrets() {
        let cfg = control_plane_with_master(TEST_MASTER);
        let user = cfg.effective_user_jwt_secret();
        let console = cfg.effective_console_jwt_secret();
        let agent = cfg.effective_agent_auth_token();
        assert_eq!(user, cfg.effective_user_jwt_secret());
        assert_eq!(console, cfg.effective_console_jwt_secret());
        assert_eq!(agent, cfg.effective_agent_auth_token());
        assert_ne!(user, console, "user and console keys must be distinct");
        assert_ne!(user, agent, "user and agent keys must be distinct");
        assert_ne!(console, agent, "console and agent keys must be distinct");
        // Not simply the legacy master re-used verbatim.
        assert_ne!(user, TEST_MASTER);
        assert!(cfg.validate_secrets().is_ok());

        // A single-host agent sharing the legacy master derives the same
        // console + agent keys, which is what keeps existing installs
        // working without new configuration.
        let agent_cfg = agent_with_master(TEST_MASTER);
        assert_eq!(agent_cfg.effective_console_jwt_secret(), console);
        assert_eq!(agent_cfg.effective_agent_auth_token(), agent);
    }

    #[test]
    fn explicit_secrets_override_derivations() {
        let mut cfg = control_plane_with_master(TEST_MASTER);
        cfg.secrets.user_jwt = Some("user-key-0123456789abcdef0123456789aaaa".into());
        cfg.secrets.console_jwt = Some("console-key-0123456789abcdef0123456789".into());
        cfg.secrets.agent_auth_token = Some("agent-key-0123456789abcdef0123456789ab".into());
        cfg.secrets.credential_encryption =
            Some("credential-key-0123456789abcdef0123456789".into());
        assert_eq!(
            cfg.effective_user_jwt_secret(),
            "user-key-0123456789abcdef0123456789aaaa"
        );
        assert_eq!(
            cfg.effective_console_jwt_secret(),
            "console-key-0123456789abcdef0123456789"
        );
        assert_eq!(
            cfg.effective_agent_auth_token(),
            "agent-key-0123456789abcdef0123456789ab"
        );
        assert_eq!(
            cfg.effective_credential_encryption_key().as_deref(),
            Some("credential-key-0123456789abcdef0123456789")
        );
        assert!(cfg.validate_secrets().is_ok());
    }

    #[test]
    fn duplicate_or_weak_explicit_secrets_fail_closed() {
        let mut cfg = control_plane_with_master(TEST_MASTER);
        cfg.secrets.user_jwt = Some("same-key-0123456789abcdef0123456789abcd".into());
        cfg.secrets.console_jwt = Some("same-key-0123456789abcdef0123456789abcd".into());
        let err = cfg.validate_secrets().unwrap_err().to_string();
        assert!(err.contains("distinct"), "{err}");

        let mut cfg = control_plane_with_master(TEST_MASTER);
        cfg.secrets.user_jwt = Some("short".into());
        let err = cfg.validate_secrets().unwrap_err().to_string();
        assert!(err.contains("at least 32"), "{err}");

        // The shipped development placeholder is never acceptable as an
        // explicit production secret.
        let mut cfg = control_plane_with_master(TEST_MASTER);
        cfg.secrets.console_jwt = Some(DEV_SECRET.into());
        let err = cfg.validate_secrets().unwrap_err().to_string();
        assert!(err.contains("development placeholder"), "{err}");

        // The agent token is sent as gRPC metadata: non-printable/non-ASCII
        // values would be silently dropped at send time.
        let mut cfg = control_plane_with_master(TEST_MASTER);
        cfg.secrets.agent_auth_token = Some("agent-token-0123456789abcdef0123456789é".into());
        let err = cfg.validate_secrets().unwrap_err().to_string();
        assert!(err.contains("printable ASCII"), "{err}");
    }

    #[test]
    fn secrets_config_is_debug_redacted() {
        let cfg = SecretsConfig {
            user_jwt: Some("user-secret-value-never-in-logs".into()),
            console_jwt: Some("console-secret-value-never-in-logs".into()),
            agent_auth_token: Some("agent-secret-value-never-in-logs".into()),
            credential_encryption: Some("credential-secret-value-never-in-logs".into()),
        };
        let rendered = format!("{cfg:?}");
        for secret in [
            "user-secret-value-never-in-logs",
            "console-secret-value-never-in-logs",
            "agent-secret-value-never-in-logs",
            "credential-secret-value-never-in-logs",
        ] {
            assert!(!rendered.contains(secret), "secret leaked: {rendered}");
        }
        assert!(rendered.contains("<redacted>"));
        assert!(rendered.contains("user_jwt"));
        assert!(rendered.contains("credential_encryption"));

        let rendered = format!("{:?}", SecretsConfig::default());
        assert!(rendered.contains("<unset>"));
        assert!(!rendered.contains("<redacted>"));
    }

    #[test]
    fn agent_ignores_control_plane_only_secrets() {
        // The agent never consumes `user_jwt` or `credential_encryption`;
        // values that are invalid for the control plane must not block
        // agent startup.
        let mut agent = agent_with_master(TEST_MASTER);
        agent.secrets.user_jwt = Some("short".into());
        agent.secrets.credential_encryption = Some("short".into());
        assert!(agent.validate_secrets().is_ok());

        let mut control_plane = control_plane_with_master(TEST_MASTER);
        control_plane.secrets.user_jwt = Some("short".into());
        assert!(control_plane.validate_secrets().is_err());

        let mut control_plane = control_plane_with_master(TEST_MASTER);
        control_plane.secrets.credential_encryption = Some("short".into());
        assert!(control_plane.validate_secrets().is_err());
    }

    #[test]
    fn production_refuses_the_insecure_agent_bypass() {
        let mut cfg = control_plane_with_master(TEST_MASTER);
        cfg.security.production = true;
        cfg.security.allow_insecure_agent_auth = true;
        let err = cfg.validate_secrets().unwrap_err().to_string();
        assert!(err.contains("forbidden"), "{err}");

        let mut cfg = control_plane_with_master(TEST_MASTER);
        cfg.security.production = false;
        cfg.security.allow_insecure_agent_auth = true;
        assert!(
            cfg.validate_secrets().is_ok(),
            "explicit local-dev opt-out may disable agent auth"
        );

        // The agent side enforces the same polarity.
        let mut agent = agent_with_master(TEST_MASTER);
        agent.security.production = true;
        agent.security.allow_insecure_agent_auth = true;
        assert!(agent.validate_secrets().is_err());
        agent.security.production = false;
        assert!(agent.validate_secrets().is_ok());
    }

    #[test]
    fn agent_console_and_daemon_keys_must_differ() {
        let mut cfg = agent_with_master(TEST_MASTER);
        cfg.secrets.console_jwt = Some("shared-key-0123456789abcdef0123456789ab".into());
        cfg.secrets.agent_auth_token = Some("shared-key-0123456789abcdef0123456789ab".into());
        let err = cfg.validate_secrets().unwrap_err().to_string();
        assert!(err.contains("distinct"), "{err}");
    }

    #[test]
    fn secrets_and_security_sections_parse_from_toml() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("controlplane.toml");
        std::fs::write(
            &config_path,
            r#"
grpc_bind = "127.0.0.1:8443"
http_bind = "127.0.0.1:8080"
log_level = "info"
runtime_dir = "/run/chv/controlplane"
jwt_secret = "legacy-master-0123456789abcdef0123456789abcdef"

[secrets]
user_jwt = "user-key-0123456789abcdef0123456789aaaa"
console_jwt = "console-key-0123456789abcdef0123456789"
agent_auth_token = "agent-key-0123456789abcdef0123456789ab"

[security]
production = false
allow_insecure_agent_auth = true
agent_auth_allowed_peer_uids = [1000, 1001]
"#,
        )
        .expect("write config");
        let cfg = load_controlplane_config(Some(&config_path)).expect("load config");
        assert_eq!(
            cfg.effective_user_jwt_secret(),
            "user-key-0123456789abcdef0123456789aaaa"
        );
        assert!(!cfg.security.production);
        assert!(cfg.security.allow_insecure_agent_auth);
        assert_eq!(cfg.security.agent_auth_allowed_peer_uids, vec![1000, 1001]);
    }
}
