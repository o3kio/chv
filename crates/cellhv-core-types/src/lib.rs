//! Platform-neutral domain model for the CellHV Core authority.
//!
//! These types intentionally contain no control-plane, cloud-platform, provider,
//! protocol, or Cloud Hypervisor implementation details.

use chv_errors::ChvError;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;

/// Deterministically normalizes a JSON value for durable request identity.
pub fn canonical_json(value: &serde_json::Value) -> Result<String, serde_json::Error> {
    fn normalize(value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(map) => serde_json::Value::Object(
                map.iter()
                    .map(|(key, value)| (key.clone(), normalize(value)))
                    .collect(),
            ),
            serde_json::Value::Array(items) => {
                serde_json::Value::Array(items.iter().map(normalize).collect())
            }
            other => other.clone(),
        }
    }
    serde_json::to_string(&normalize(value))
}

/// SHA-256 identity of the canonical JSON representation.
pub fn canonical_request_fingerprint(
    value: &serde_json::Value,
) -> Result<String, serde_json::Error> {
    Ok(hex::encode(Sha256::digest(canonical_json(value)?)))
}

fn require_non_empty(field: &str, value: &str) -> Result<(), ChvError> {
    if value.trim().is_empty() {
        return Err(ChvError::InvalidArgument {
            field: field.to_string(),
            reason: "must not be empty".to_string(),
        });
    }
    Ok(())
}

/// Rejects resource ids that could escape the runtime's path derivations.
///
/// Path-safe ids keep the runtime's `{runtime_dir}/vms/<id>` and
/// `{vm_dir}/<id>.img` derives inside the intended tree.
fn require_safe_id(value: &str, field: &str) -> Result<(), ChvError> {
    require_non_empty(field, value)?;
    if value.contains('/')
        || value.contains('\\')
        || value.contains('\0')
        || value == "."
        || value == ".."
    {
        return Err(ChvError::InvalidArgument {
            field: field.to_string(),
            reason: "must not contain path separators or dot components".to_string(),
        });
    }
    Ok(())
}

macro_rules! identifier {
    ($name:ident, $field:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, ChvError> {
                let value = value.into();
                if value.trim().is_empty() {
                    return Err(ChvError::InvalidArgument {
                        field: $field.to_string(),
                        reason: "must not be empty".to_string(),
                    });
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl TryFrom<String> for $name {
            type Error = ChvError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

identifier!(HostId, "host_id");
identifier!(VmId, "vm_id");
identifier!(OperationId, "operation_id");
identifier!(EventId, "event_id");
identifier!(IdempotencyKey, "idempotency_key");

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ResourceVersion(u64);

impl ResourceVersion {
    pub fn new(value: u64) -> Result<Self, ChvError> {
        if value == 0 {
            return Err(ChvError::InvalidArgument {
                field: "resource_version".to_string(),
                reason: "must be greater than zero".to_string(),
            });
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u64 {
        self.0
    }

    pub fn next(self) -> Result<Self, ChvError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or_else(|| ChvError::Conflict {
                resource: "resource_version".to_string(),
                id: self.0.to_string(),
            })
    }
}

impl<'de> Deserialize<'de> for ResourceVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u64::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestedPowerState {
    Running,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedPowerState {
    Unknown,
    Created,
    Running,
    Stopped,
    Paused,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    CreateVm,
    UpdateVm,
    DeleteVm,
    StartVm,
    StopVm,
    RebootVm,
    AttachVolume,
    DetachVolume,
    AttachNetwork,
    DetachNetwork,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    Accepted,
    Running,
    Succeeded,
    Failed,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnershipClass {
    CellHv,
    External,
    Unclaimed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryClass {
    OwnedRecoverable,
    OwnedInconsistent,
    Foreign,
    Ambiguous,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostIdentity {
    pub id: HostId,
    pub resource_version: ResourceVersion,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HostCapabilities {
    pub vm_definitions: bool,
    pub power_start: bool,
    pub power_stop: bool,
    pub power_reboot: bool,
    pub live_update_vcpus: bool,
    pub live_update_memory: bool,
    pub event_watch: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, try_from = "RawBootSpec")]
pub struct BootSpec {
    pub kernel: String,
    pub firmware: Option<String>,
    pub initial_disk: Option<String>,
}

impl BootSpec {
    pub fn new(kernel: impl Into<String>) -> Result<Self, ChvError> {
        let kernel = kernel.into();
        require_non_empty("boot.kernel", &kernel)?;
        Ok(Self {
            kernel,
            firmware: None,
            initial_disk: None,
        })
    }

    pub fn validate(&self) -> Result<(), ChvError> {
        require_non_empty("boot.kernel", &self.kernel)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBootSpec {
    kernel: String,
    firmware: Option<String>,
    initial_disk: Option<String>,
}

impl TryFrom<RawBootSpec> for BootSpec {
    type Error = ChvError;

    fn try_from(raw: RawBootSpec) -> Result<Self, Self::Error> {
        let value = Self {
            kernel: raw.kernel,
            firmware: raw.firmware,
            initial_disk: raw.initial_disk,
        };
        value.validate()?;
        Ok(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, try_from = "RawComputeSpec")]
pub struct ComputeSpec {
    pub vcpus: u32,
    pub memory_bytes: u64,
}

impl ComputeSpec {
    pub fn new(vcpus: u32, memory_bytes: u64) -> Result<Self, ChvError> {
        let value = Self {
            vcpus,
            memory_bytes,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), ChvError> {
        if self.vcpus == 0 {
            return Err(ChvError::InvalidArgument {
                field: "compute.vcpus".to_string(),
                reason: "must be greater than zero".to_string(),
            });
        }
        if self.memory_bytes == 0 {
            return Err(ChvError::InvalidArgument {
                field: "compute.memory_bytes".to_string(),
                reason: "must be greater than zero".to_string(),
            });
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawComputeSpec {
    vcpus: u32,
    memory_bytes: u64,
}

impl TryFrom<RawComputeSpec> for ComputeSpec {
    type Error = ChvError;

    fn try_from(raw: RawComputeSpec) -> Result<Self, Self::Error> {
        Self::new(raw.vcpus, raw.memory_bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, try_from = "RawStorageAttachmentRef")]
pub struct StorageAttachmentRef {
    pub attachment_id: String,
    pub storage_ref: String,
    pub read_only: bool,
    /// Provisioning hint for executors that create-on-open the backing
    /// volume (e.g. stord local volumes): the volume's size. Absent means
    /// the referenced storage must already exist at its configured size.
    pub size_bytes: Option<u64>,
    /// Provisioning hint: seed the volume's content from this image path on
    /// first open. Absent means no seeding.
    pub seed_from: Option<String>,
    /// Stord backend class for this attachment's volume opens (#379 PR 2,
    /// the A7 field): the per-volume dispatch class the executor threads
    /// into `open_volume`, absent meaning the executor's node-level
    /// default (historically `"local"`). `Option` +
    /// `#[serde(default)]`/`skip_serializing_if` is REQUIRED here, not
    /// merely belt-and-braces: the struct carries
    /// `#[serde(deny_unknown_fields, try_from = "RawStorageAttachmentRef")]`,
    /// so a bare additive field would make a mixed-version fleet reject
    /// the whole Core definition — a definition WITHOUT the field (every
    /// pre-#379 journal entry) must keep deserializing, and one WITH it
    /// must too. Journal compatibility is therefore upgrade-only, the
    /// same recorded posture as `firewall_policy_json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_class: Option<String>,
}

impl StorageAttachmentRef {
    pub fn validate(&self) -> Result<(), ChvError> {
        require_safe_id(&self.attachment_id, "storage.attachment_id")?;
        require_safe_id(&self.storage_ref, "storage.storage_ref")?;
        if let Some(size_bytes) = self.size_bytes {
            if size_bytes == 0 {
                return Err(ChvError::InvalidArgument {
                    field: "storage.size_bytes".to_string(),
                    reason: "must be greater than zero when present".to_string(),
                });
            }
        }
        if let Some(seed_from) = &self.seed_from {
            require_non_empty("storage.seed_from", seed_from)?;
        }
        if let Some(backend_class) = &self.backend_class {
            require_non_empty("storage.backend_class", backend_class)?;
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStorageAttachmentRef {
    attachment_id: String,
    storage_ref: String,
    read_only: bool,
    #[serde(default)]
    size_bytes: Option<u64>,
    #[serde(default)]
    seed_from: Option<String>,
    /// #379 PR 2 (A7): mirrored from [`StorageAttachmentRef`] — the
    /// `try_from` deserialization path goes through this raw struct, so
    /// the field must be `#[serde(default)]` here for a pre-#379
    /// definition (no `backend_class` key) to keep parsing under
    /// `deny_unknown_fields`.
    #[serde(default)]
    backend_class: Option<String>,
}

impl TryFrom<RawStorageAttachmentRef> for StorageAttachmentRef {
    type Error = ChvError;

    fn try_from(raw: RawStorageAttachmentRef) -> Result<Self, Self::Error> {
        let value = Self {
            attachment_id: raw.attachment_id,
            storage_ref: raw.storage_ref,
            read_only: raw.read_only,
            size_bytes: raw.size_bytes,
            seed_from: raw.seed_from,
            backend_class: raw.backend_class,
        };
        value.validate()?;
        Ok(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, try_from = "RawNetworkAttachmentRef")]
pub struct NetworkAttachmentRef {
    pub attachment_id: String,
    pub network_ref: String,
    pub mac_address: Option<String>,
    /// Per-NIC addressing for networks with control-plane-assigned addresses
    /// (internal IPAM). Absent means the executor's topology defaults apply.
    pub addressing: Option<NicAddressing>,
    /// Operator-configured firewall policy for the network — the
    /// `network_desired_state.firewall_rules_json` snapshot taken when the
    /// control plane built this spec (#355). The executor applies it via nwd
    /// after the network's topology is ensured, so default-deny + the
    /// operator's rules materialize when a network is used. An absent or
    /// empty snapshot resolves to the shared DP4 baseline
    /// (DHCP/DNS/conntrack allows + default-deny) at the attach path
    /// (#355 PR 3) — the pre-DP4 bare-table skip and the #360
    /// never-apply cutoff are both gone.
    ///
    /// Journal compatibility is upgrade-only: entries written by newer
    /// binaries (carrying this field) are rejected by older binaries
    /// (`deny_unknown_fields`); old entries without it keep parsing.
    pub firewall_policy_json: Option<String>,
}

/// Addressing assigned by the control plane for one NIC: the VM's address in
/// the network, the network's CIDR, and its gateway. When addressing is
/// present `ip_address` must be non-empty (validation rejects an assigned
/// NIC without an address); `cidr` and `gateway` may be empty, and the
/// executor defaults an empty CIDR to the topology default exactly like
/// the legacy reconcile path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NicAddressing {
    pub ip_address: String,
    pub cidr: String,
    pub gateway: String,
}

impl NetworkAttachmentRef {
    pub fn validate(&self) -> Result<(), ChvError> {
        require_safe_id(&self.attachment_id, "network.attachment_id")?;
        require_safe_id(&self.network_ref, "network.network_ref")?;
        if let Some(mac_address) = &self.mac_address {
            require_non_empty("network.mac_address", mac_address)?;
        }
        if let Some(addressing) = &self.addressing {
            require_non_empty("network.addressing.ip_address", &addressing.ip_address)?;
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNetworkAttachmentRef {
    attachment_id: String,
    network_ref: String,
    mac_address: Option<String>,
    #[serde(default)]
    addressing: Option<NicAddressing>,
    #[serde(default)]
    firewall_policy_json: Option<String>,
}

impl TryFrom<RawNetworkAttachmentRef> for NetworkAttachmentRef {
    type Error = ChvError;

    fn try_from(raw: RawNetworkAttachmentRef) -> Result<Self, Self::Error> {
        let value = Self {
            attachment_id: raw.attachment_id,
            network_ref: raw.network_ref,
            mac_address: raw.mac_address,
            addressing: raw.addressing,
            // An empty (or whitespace) policy is semantically absent —
            // normalize here so the durable definition never carries
            // Some("") sentinel values.
            firewall_policy_json: raw.firewall_policy_json.filter(|p| !p.trim().is_empty()),
        };
        value.validate()?;
        Ok(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, try_from = "RawVmDefinition")]
pub struct VmDefinition {
    pub id: VmId,
    pub name: String,
    pub boot: BootSpec,
    pub compute: ComputeSpec,
    pub storage: Vec<StorageAttachmentRef>,
    pub networks: Vec<NetworkAttachmentRef>,
    pub requested_power_state: RequestedPowerState,
    pub observed_power_state: ObservedPowerState,
    pub resource_version: ResourceVersion,
    /// Cloud-init userdata for first boot, carried verbatim for the runtime.
    pub cloud_init_userdata: Option<String>,
    /// Hypervisor tuning flags carried verbatim for the runtime (the Core
    /// mirror of the legacy `HypervisorOverrides`). Semantics are enforced
    /// by [`VmDefinition::validate`] via [`HypervisorTuning::validate`] at
    /// every Core boundary (direct submissions, journal replays, and the
    /// legacy-cache migration), mirroring the checks the legacy spec
    /// boundary applies (pinned by a cross-boundary rule-parity test in
    /// chv-agent-core).
    pub hypervisor_tuning: Option<HypervisorTuning>,
    /// G5 vsock monitoring-transport device opt-in, carried verbatim for
    /// the runtime (the Core mirror of the legacy spec's `VsockSpec`).
    /// The node authority owns the addressing (CID, socket path) at
    /// create time — this definition carries only the opt-in. Absent ⇒
    /// no device. Semantics are enforced by [`VmDefinition::validate`]
    /// at every Core boundary, mirroring the legacy spec boundary's
    /// check (pinned by the cross-boundary parity tests).
    pub vsock: Option<VsockDeviceSpec>,
}

/// Core mirror of the vsock device opt-in (see [`VmDefinition::vsock`]).
/// Present must be `enabled: true`; the opt-out form is `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VsockDeviceSpec {
    pub enabled: bool,
}

/// Core-owned mirror of the legacy hypervisor override surface. Every field
/// is optional in the legacy sense: absent means "use the runtime's
/// default". Translated 1:1 at the legacy adapter boundary so the durable
/// journal payload stays typed and self-describing (no untyped blobs, no
/// silently dropped legacy fields).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct HypervisorTuning {
    pub cpu_nested: Option<bool>,
    pub cpu_amx: Option<bool>,
    pub cpu_kvm_hyperv: Option<bool>,
    pub memory_mergeable: Option<bool>,
    pub memory_hugepages: Option<bool>,
    pub memory_shared: Option<bool>,
    pub memory_prefault: Option<bool>,
    pub iommu: Option<bool>,
    pub rng_src: Option<String>,
    pub watchdog: Option<bool>,
    pub landlock_enable: Option<bool>,
    pub serial_mode: Option<String>,
    pub console_mode: Option<String>,
    pub pvpanic: Option<bool>,
    pub tpm_type: Option<String>,
    pub tpm_socket_path: Option<String>,
}

impl HypervisorTuning {
    /// Every field of the tuning surface, in declaration order. Callers at
    /// legacy boundaries use it to distinguish "a field Core does not model"
    /// (unsupported, fail closed) from malformed values, without duplicating
    /// the field list. Pinned by
    /// `hypervisor_tuning_known_fields_match_serialization`: adding a field
    /// without extending this list fails that test.
    pub const KNOWN_FIELDS: &'static [&'static str] = &[
        "cpu_nested",
        "cpu_amx",
        "cpu_kvm_hyperv",
        "memory_mergeable",
        "memory_hugepages",
        "memory_shared",
        "memory_prefault",
        "iommu",
        "rng_src",
        "watchdog",
        "landlock_enable",
        "serial_mode",
        "console_mode",
        "pvpanic",
        "tpm_type",
        "tpm_socket_path",
    ];

    /// Semantic rules for the tuning surface, mirroring the checks the
    /// legacy spec boundary (`chv-common`'s validators, applied by
    /// `chv-agent-core`'s `VmSpec::validate`) applies to
    /// `HypervisorOverrides`: `rng_src` must be a non-empty absolute path,
    /// `serial_mode`/`console_mode`/`tpm_type` must be cloud-hypervisor
    /// values, and `tpm_socket_path` requires `tpm_type`. chv-agent-core
    /// pins the two rule sets together (it can see both crates; this crate
    /// must stay independent of the legacy surface).
    pub fn validate(&self) -> Result<(), ChvError> {
        const VALID_SERIAL_MODES: &[&str] = &["Socket", "Pty", "File", "Off", "Null"];
        const VALID_CONSOLE_MODES: &[&str] = &["Pty", "File", "Off", "Null"];
        const VALID_TPM_TYPES: &[&str] = &["swtpm"];
        if let Some(rng_src) = &self.rng_src {
            if rng_src.is_empty() {
                return Err(ChvError::InvalidArgument {
                    field: "hypervisor_tuning.rng_src".to_string(),
                    reason: "rng_src must be non-empty".to_string(),
                });
            }
            if !rng_src.starts_with('/') {
                return Err(ChvError::InvalidArgument {
                    field: "hypervisor_tuning.rng_src".to_string(),
                    reason: "rng_src must be an absolute path".to_string(),
                });
            }
        }
        if let Some(mode) = &self.serial_mode {
            if !VALID_SERIAL_MODES.contains(&mode.as_str()) {
                return Err(ChvError::InvalidArgument {
                    field: "hypervisor_tuning.serial_mode".to_string(),
                    reason: format!("serial_mode must be one of {VALID_SERIAL_MODES:?}"),
                });
            }
        }
        if let Some(mode) = &self.console_mode {
            if !VALID_CONSOLE_MODES.contains(&mode.as_str()) {
                return Err(ChvError::InvalidArgument {
                    field: "hypervisor_tuning.console_mode".to_string(),
                    reason: format!("console_mode must be one of {VALID_CONSOLE_MODES:?}"),
                });
            }
        }
        if let Some(tpm_type) = &self.tpm_type {
            if !VALID_TPM_TYPES.contains(&tpm_type.as_str()) {
                return Err(ChvError::InvalidArgument {
                    field: "hypervisor_tuning.tpm_type".to_string(),
                    reason: format!("tpm_type must be one of {VALID_TPM_TYPES:?}"),
                });
            }
        }
        if self.tpm_type.is_none() && self.tpm_socket_path.is_some() {
            return Err(ChvError::InvalidArgument {
                field: "hypervisor_tuning.tpm_socket_path".to_string(),
                reason: "tpm_socket_path cannot be set without tpm_type".to_string(),
            });
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawVmDefinition {
    id: VmId,
    name: String,
    boot: BootSpec,
    compute: ComputeSpec,
    storage: Vec<StorageAttachmentRef>,
    networks: Vec<NetworkAttachmentRef>,
    requested_power_state: RequestedPowerState,
    observed_power_state: ObservedPowerState,
    resource_version: ResourceVersion,
    #[serde(default)]
    cloud_init_userdata: Option<String>,
    #[serde(default)]
    hypervisor_tuning: Option<HypervisorTuning>,
    #[serde(default)]
    vsock: Option<VsockDeviceSpec>,
}

impl TryFrom<RawVmDefinition> for VmDefinition {
    type Error = ChvError;

    fn try_from(raw: RawVmDefinition) -> Result<Self, Self::Error> {
        let value = Self {
            id: raw.id,
            name: raw.name,
            boot: raw.boot,
            compute: raw.compute,
            storage: raw.storage,
            networks: raw.networks,
            requested_power_state: raw.requested_power_state,
            observed_power_state: raw.observed_power_state,
            resource_version: raw.resource_version,
            cloud_init_userdata: raw.cloud_init_userdata,
            hypervisor_tuning: raw.hypervisor_tuning,
            vsock: raw.vsock,
        };
        value.validate()?;
        Ok(value)
    }
}

impl VmDefinition {
    pub fn validate(&self) -> Result<(), ChvError> {
        require_safe_id(self.id.as_str(), "vm.id")?;
        require_non_empty("vm.name", &self.name)?;
        self.boot.validate()?;
        self.compute.validate()?;
        for attachment in &self.storage {
            attachment.validate()?;
        }
        for attachment in &self.networks {
            attachment.validate()?;
        }
        let mut attachment_ids = std::collections::HashSet::new();
        for attachment_id in self
            .storage
            .iter()
            .map(|item| item.attachment_id.as_str())
            .chain(self.networks.iter().map(|item| item.attachment_id.as_str()))
        {
            if !attachment_ids.insert(attachment_id) {
                return Err(ChvError::InvalidArgument {
                    field: "vm.attachments".to_string(),
                    reason: format!("duplicate attachment_id {attachment_id}"),
                });
            }
        }
        if let Some(tuning) = &self.hypervisor_tuning {
            tuning.validate()?;
        }
        if let Some(vsock) = &self.vsock {
            if !vsock.enabled {
                return Err(ChvError::InvalidArgument {
                    field: "vm.vsock.enabled".to_string(),
                    reason: "vm.vsock.enabled cannot be false — omit vsock entirely to \
                             opt out (the device is strictly opt-in)"
                        .to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, try_from = "RawOperation")]
pub struct Operation {
    pub id: OperationId,
    pub kind: OperationKind,
    pub vm_id: VmId,
    pub status: OperationStatus,
    pub request_fingerprint: String,
    pub attempt_count: u32,
    pub max_attempts: u32,
}

impl Operation {
    pub fn validate(&self) -> Result<(), ChvError> {
        require_non_empty("operation.request_fingerprint", &self.request_fingerprint)?;
        if self.max_attempts == 0 {
            return Err(ChvError::InvalidArgument {
                field: "operation.max_attempts".to_string(),
                reason: "must be greater than zero".to_string(),
            });
        }
        if self.attempt_count > self.max_attempts {
            return Err(ChvError::InvalidArgument {
                field: "operation.attempt_count".to_string(),
                reason: "must not exceed max_attempts".to_string(),
            });
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOperation {
    id: OperationId,
    kind: OperationKind,
    vm_id: VmId,
    status: OperationStatus,
    request_fingerprint: String,
    attempt_count: u32,
    max_attempts: u32,
}

impl TryFrom<RawOperation> for Operation {
    type Error = ChvError;

    fn try_from(raw: RawOperation) -> Result<Self, Self::Error> {
        let value = Self {
            id: raw.id,
            kind: raw.kind,
            vm_id: raw.vm_id,
            status: raw.status,
            request_fingerprint: raw.request_fingerprint,
            attempt_count: raw.attempt_count,
            max_attempts: raw.max_attempts,
        };
        value.validate()?;
        Ok(value)
    }
}

/// Prefix of operation IDs constructed by the legacy control-plane adapter.
///
/// Shared by the write paths (which reject `legacy:` submissions that lack a
/// legacy generation) and the store's reopen validation (which treats rows with
/// this prefix as legacy-provenance when they carry metadata).
pub const LEGACY_OPERATION_ID_PREFIX: &str = "legacy:";

/// Durable audit metadata recorded alongside an accepted operation.
///
/// Native submits journal the local surface as the requester; legacy submits
/// journal the control-plane caller, its external operation ID, the request
/// timestamp, and the legacy desired generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationRequestMetadata {
    pub requested_by: String,
    pub external_operation_id: String,
    pub request_unix_ms: i64,
    pub legacy_generation: Option<u64>,
}

/// Maximum supported `OperationRequestMetadata::request_unix_ms`.
///
/// Equal to 9999-12-31T23:59:59.999Z, the largest `SystemTime` representable
/// in milliseconds on most runtimes, so any in-range value maps to a valid
/// timestamp rather than an overflow-resolved instant.
pub const MAX_REQUEST_UNIX_MS: i64 = 253_402_300_799_999;

impl OperationRequestMetadata {
    pub fn validate(&self) -> Result<(), String> {
        if self.requested_by.trim().is_empty() {
            return Err("operation.request_metadata.requested_by must not be empty".to_owned());
        }
        if self.requested_by.chars().count() > 1024 {
            return Err(
                "operation.request_metadata.requested_by must be at most 1024 characters"
                    .to_owned(),
            );
        }
        if self.external_operation_id.trim().is_empty() {
            return Err(
                "operation.request_metadata.external_operation_id must not be empty".to_owned(),
            );
        }
        if self.external_operation_id.chars().count() > 1024 {
            return Err(
                "operation.request_metadata.external_operation_id must be at most 1024 characters"
                    .to_owned(),
            );
        }
        if self.request_unix_ms <= 0 {
            return Err(
                "operation.request_metadata.request_unix_ms must be greater than zero".to_owned(),
            );
        }
        if self.request_unix_ms > MAX_REQUEST_UNIX_MS {
            return Err(
                "operation.request_metadata.request_unix_ms exceeds the maximum supported timestamp"
                    .to_owned(),
            );
        }
        if let Some(generation) = self.legacy_generation {
            if generation < 1 {
                return Err(
                    "operation.request_metadata.legacy_generation must be at least 1".to_owned(),
                );
            }
            if generation > i64::MAX as u64 {
                return Err(
                    "operation.request_metadata.legacy_generation exceeds SQLite storage range"
                        .to_owned(),
                );
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, try_from = "RawOperationStep")]
pub struct OperationStep {
    pub operation_id: OperationId,
    pub index: u32,
    pub name: String,
    pub status: StepStatus,
    pub attempt_count: u32,
    pub last_error: Option<String>,
}

impl OperationStep {
    pub fn validate(&self) -> Result<(), ChvError> {
        require_non_empty("operation_step.name", &self.name)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOperationStep {
    operation_id: OperationId,
    index: u32,
    name: String,
    status: StepStatus,
    attempt_count: u32,
    last_error: Option<String>,
}

impl TryFrom<RawOperationStep> for OperationStep {
    type Error = ChvError;

    fn try_from(raw: RawOperationStep) -> Result<Self, Self::Error> {
        let value = Self {
            operation_id: raw.operation_id,
            index: raw.index,
            name: raw.name,
            status: raw.status,
            attempt_count: raw.attempt_count,
            last_error: raw.last_error,
        };
        value.validate()?;
        Ok(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, try_from = "RawOperationEvent")]
pub struct OperationEvent {
    pub id: EventId,
    pub sequence: u64,
    pub operation_id: Option<OperationId>,
    pub vm_id: Option<VmId>,
    pub kind: String,
    pub payload: serde_json::Value,
}

impl OperationEvent {
    pub fn validate(&self) -> Result<(), ChvError> {
        if self.sequence == 0 {
            return Err(ChvError::InvalidArgument {
                field: "event.sequence".to_string(),
                reason: "must be greater than zero".to_string(),
            });
        }
        require_non_empty("event.kind", &self.kind)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOperationEvent {
    id: EventId,
    sequence: u64,
    operation_id: Option<OperationId>,
    vm_id: Option<VmId>,
    kind: String,
    payload: serde_json::Value,
}

impl TryFrom<RawOperationEvent> for OperationEvent {
    type Error = ChvError;

    fn try_from(raw: RawOperationEvent) -> Result<Self, Self::Error> {
        let value = Self {
            id: raw.id,
            sequence: raw.sequence,
            operation_id: raw.operation_id,
            vm_id: raw.vm_id,
            kind: raw.kind,
            payload: raw.payload,
        };
        value.validate()?;
        Ok(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, try_from = "RawOwnershipMarker")]
pub struct OwnershipMarker {
    pub vm_id: VmId,
    pub owner_id: HostId,
    pub ownership: OwnershipClass,
    pub recovery: RecoveryClass,
    pub marker_version: u32,
}

impl OwnershipMarker {
    pub fn validate(&self) -> Result<(), ChvError> {
        if self.marker_version == 0 {
            return Err(ChvError::InvalidArgument {
                field: "ownership.marker_version".to_string(),
                reason: "must be greater than zero".to_string(),
            });
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOwnershipMarker {
    vm_id: VmId,
    owner_id: HostId,
    ownership: OwnershipClass,
    recovery: RecoveryClass,
    marker_version: u32,
}

impl TryFrom<RawOwnershipMarker> for OwnershipMarker {
    type Error = ChvError;

    fn try_from(raw: RawOwnershipMarker) -> Result<Self, Self::Error> {
        let value = Self {
            vm_id: raw.vm_id,
            owner_id: raw.owner_id,
            ownership: raw.ownership,
            recovery: raw.recovery,
            marker_version: raw.marker_version,
        };
        value.validate()?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_reject_empty_and_whitespace_values() {
        assert!(VmId::new("").is_err());
        assert!(HostId::new("  ").is_err());
        assert!(serde_json::from_str::<OperationId>(r#""""#).is_err());
    }

    #[test]
    fn resource_versions_start_at_one_and_increment_safely() {
        assert!(ResourceVersion::new(0).is_err());
        assert!(serde_json::from_str::<ResourceVersion>("0").is_err());
        let version = ResourceVersion::new(1).unwrap();
        assert_eq!(version.next().unwrap().get(), 2);
        assert!(ResourceVersion::new(u64::MAX).unwrap().next().is_err());
    }

    #[test]
    fn compute_and_boot_validation_reject_unusable_values() {
        assert!(ComputeSpec::new(0, 1024).is_err());
        assert!(ComputeSpec::new(1, 0).is_err());
        assert!(BootSpec::new(" ").is_err());
    }

    #[test]
    fn capabilities_default_to_false() {
        let capabilities = HostCapabilities::default();
        assert!(!capabilities.vm_definitions);
        assert!(!capabilities.power_start);
        assert!(!capabilities.power_stop);
        assert!(!capabilities.power_reboot);
        assert!(!capabilities.live_update_vcpus);
        assert!(!capabilities.live_update_memory);
        assert!(!capabilities.event_watch);
    }

    #[test]
    fn domain_contract_rejects_cloud_and_unknown_fields() {
        let input = r#"{
            "id":"vm-1",
            "name":"test",
            "boot":{"kernel":"kernel-ref","firmware":null,"initial_disk":null},
            "compute":{"vcpus":2,"memory_bytes":1073741824},
            "storage":[],
            "networks":[],
            "requested_power_state":"stopped",
            "observed_power_state":"unknown",
            "resource_version":1,
            "project_id":"must-not-enter-core"
        }"#;
        assert!(serde_json::from_str::<VmDefinition>(input).is_err());
    }

    #[test]
    fn vm_definition_round_trips() {
        let definition = VmDefinition {
            id: VmId::new("vm-1").unwrap(),
            name: "test".to_string(),
            boot: BootSpec::new("kernel-ref").unwrap(),
            compute: ComputeSpec::new(2, 1_073_741_824).unwrap(),
            storage: vec![StorageAttachmentRef {
                attachment_id: "disk-0".to_string(),
                storage_ref: "volume-1".to_string(),
                read_only: false,
                size_bytes: None,
                seed_from: None,
                backend_class: None,
            }],
            networks: vec![],
            requested_power_state: RequestedPowerState::Stopped,
            observed_power_state: ObservedPowerState::Unknown,
            resource_version: ResourceVersion::new(1).unwrap(),
            cloud_init_userdata: None,
            hypervisor_tuning: None,
            vsock: None,
        };
        let encoded = serde_json::to_string(&definition).unwrap();
        let decoded: VmDefinition = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, definition);
    }

    #[test]
    fn storage_attachment_backend_class_is_serde_additive_under_deny_unknown_fields() {
        // #379 PR 2 pin (A7, the §7 serde caveat): the class field must
        // be additive on the ONE serde surface where that is not
        // automatic — StorageAttachmentRef carries
        // `deny_unknown_fields` + `try_from`, so (i) a pre-#379
        // attachment JSON (no backend_class key) must still
        // deserialize, (ii) one carrying the field must deserialize to
        // the same value, and (iii) serializing a None-class attachment
        // must OMIT the key (the absent-field = default contract, so
        // re-serialized definitions stay byte-shaped like pre-#379
        // ones).
        let pre = r#"{
            "attachment_id": "disk-0",
            "storage_ref": "volume-1",
            "read_only": false
        }"#;
        let decoded: StorageAttachmentRef = serde_json::from_str(pre).unwrap();
        assert_eq!(decoded.backend_class, None);

        let with_class: StorageAttachmentRef = serde_json::from_str(
            r#"{
                "attachment_id": "disk-0",
                "storage_ref": "volume-1",
                "read_only": false,
                "backend_class": "lvm"
            }"#,
        )
        .unwrap();
        assert_eq!(with_class.backend_class.as_deref(), Some("lvm"));

        // Round trip: the carried class survives, and a None class
        // serializes WITHOUT the key.
        let encoded = serde_json::to_string(&with_class).unwrap();
        let reparsed: StorageAttachmentRef = serde_json::from_str(&encoded).unwrap();
        assert_eq!(reparsed, with_class);

        let none_class = StorageAttachmentRef {
            attachment_id: "disk-0".to_string(),
            storage_ref: "volume-1".to_string(),
            read_only: false,
            size_bytes: None,
            seed_from: None,
            backend_class: None,
        };
        let encoded = serde_json::to_string(&none_class).unwrap();
        assert!(
            !encoded.contains("backend_class"),
            "a None class must omit the key, got: {encoded}"
        );
        let reparsed: StorageAttachmentRef = serde_json::from_str(&encoded).unwrap();
        assert_eq!(reparsed, none_class);

        // An empty-string class is a malformed value, not a silent
        // default — the same treatment seed_from gets.
        let empty_class = r#"{
            "attachment_id": "disk-0",
            "storage_ref": "volume-1",
            "read_only": false,
            "backend_class": ""
        }"#;
        assert!(serde_json::from_str::<StorageAttachmentRef>(empty_class).is_err());
    }

    #[test]
    fn vm_definition_round_trips_provisioning_and_tuning_fields() {
        // Journals written before these fields existed deserialize with them
        // absent (covered by the fixtures above); this pins the carried
        // shape: sizing/seed and the #379 storage class on the attachment,
        // addressing on the NIC, and userdata/tuning on the definition all
        // survive a round trip.
        let definition = VmDefinition {
            id: VmId::new("vm-1").unwrap(),
            name: "test".to_string(),
            boot: BootSpec::new("kernel-ref").unwrap(),
            compute: ComputeSpec::new(2, 1_073_741_824).unwrap(),
            storage: vec![StorageAttachmentRef {
                attachment_id: "disk-0".to_string(),
                storage_ref: "volume-1".to_string(),
                read_only: false,
                size_bytes: Some(10_737_418_240),
                seed_from: Some("/var/lib/chv/images/ubuntu.img".to_string()),
                backend_class: Some("lvm".to_string()),
            }],
            networks: vec![NetworkAttachmentRef {
                attachment_id: "nic-0".to_string(),
                network_ref: "network-1".to_string(),
                mac_address: None,
                addressing: Some(NicAddressing {
                    ip_address: "10.200.0.47".to_string(),
                    cidr: "10.200.0.0/24".to_string(),
                    gateway: "10.200.0.1".to_string(),
                }),
                firewall_policy_json: None,
            }],
            requested_power_state: RequestedPowerState::Stopped,
            observed_power_state: ObservedPowerState::Unknown,
            resource_version: ResourceVersion::new(1).unwrap(),
            cloud_init_userdata: Some("#cloud-config".to_string()),
            hypervisor_tuning: Some(HypervisorTuning {
                cpu_nested: Some(true),
                rng_src: Some("/dev/hwrng".to_string()),
                ..Default::default()
            }),
            vsock: None,
        };
        let encoded = serde_json::to_string(&definition).unwrap();
        let decoded: VmDefinition = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, definition);
        // Invalid values fail validation, not just serialization.
        let mut zero_sized = definition.clone();
        zero_sized.storage[0].size_bytes = Some(0);
        assert!(zero_sized.validate().is_err());
        let mut empty_ip = definition.clone();
        empty_ip.networks[0].addressing = Some(NicAddressing {
            ip_address: String::new(),
            cidr: "10.200.0.0/24".to_string(),
            gateway: "10.200.0.1".to_string(),
        });
        assert!(empty_ip.validate().is_err());
    }

    #[test]
    fn vm_definition_vsock_is_additive_and_validated() {
        // G5 pin: the vsock opt-in is additive on the
        // deny_unknown_fields + try_from surface — (i) a pre-G5
        // definition JSON (no vsock key) deserializes with vsock: None,
        // (ii) one carrying the field parses to the same value and
        // survives a round trip, (iii) a None opt-in serializes WITHOUT
        // the key (journals stay byte-shaped like pre-G5 ones), and
        // (iv) the enabled:false double negative is rejected at the
        // Core boundary (defense in depth: the legacy spec boundary
        // rejects it first).
        let pre = r#"{
            "id": "vm-1",
            "name": "test",
            "boot": { "kernel": "kernel-ref" },
            "compute": { "vcpus": 2, "memory_bytes": 1073741824 },
            "storage": [],
            "networks": [],
            "requested_power_state": "stopped",
            "observed_power_state": "unknown",
            "resource_version": 1
        }"#;
        let decoded: VmDefinition = serde_json::from_str(pre).unwrap();
        assert_eq!(decoded.vsock, None);
        assert!(decoded.validate().is_ok());

        let with = r#"{
            "id": "vm-1",
            "name": "test",
            "boot": { "kernel": "kernel-ref" },
            "compute": { "vcpus": 2, "memory_bytes": 1073741824 },
            "storage": [],
            "networks": [],
            "requested_power_state": "stopped",
            "observed_power_state": "unknown",
            "resource_version": 1,
            "vsock": { "enabled": true }
        }"#;
        let decoded: VmDefinition = serde_json::from_str(with).unwrap();
        assert_eq!(decoded.vsock, Some(VsockDeviceSpec { enabled: true }));
        assert!(decoded.validate().is_ok());
        // Round trip keeps the opt-in; a None opt-in serializes as null —
        // exactly the shape the sibling optional fields
        // (cloud_init_userdata, hypervisor_tuning) already use, so
        // re-serialized definitions stay consistent with pre-G5 journals.
        let encoded = serde_json::to_string(&decoded).unwrap();
        let reparsed: VmDefinition = serde_json::from_str(&encoded).unwrap();
        assert_eq!(reparsed, decoded);
        let none_encoded = serde_json::to_string(&VmDefinition {
            vsock: None,
            ..decoded.clone()
        })
        .unwrap();
        let none_reparsed: VmDefinition = serde_json::from_str(&none_encoded).unwrap();
        assert_eq!(none_reparsed.vsock, None);

        // The double negative is rejected at validation.
        let mut disabled = decoded.clone();
        disabled.vsock = Some(VsockDeviceSpec { enabled: false });
        let err = disabled.validate().unwrap_err();
        assert!(
            matches!(&err, ChvError::InvalidArgument { field, .. } if field == "vm.vsock.enabled"),
            "got {err:?}"
        );
        // And it cannot even enter through the serde boundary's
        // validate-on-try_from path.
        let disabled_json = r#"{
            "id": "vm-1",
            "name": "test",
            "boot": { "kernel": "kernel-ref" },
            "compute": { "vcpus": 2, "memory_bytes": 1073741824 },
            "storage": [],
            "networks": [],
            "requested_power_state": "stopped",
            "observed_power_state": "unknown",
            "resource_version": 1,
            "vsock": { "enabled": false }
        }"#;
        assert!(serde_json::from_str::<VmDefinition>(disabled_json).is_err());
    }

    #[test]
    fn hypervisor_tuning_known_fields_match_serialization() {
        // KNOWN_FIELDS is the classification surface legacy boundaries use to
        // tell "unsupported field" from "malformed value"; it must track the
        // struct exactly, in order.
        let tuning = HypervisorTuning {
            cpu_nested: Some(true),
            cpu_amx: Some(false),
            cpu_kvm_hyperv: Some(true),
            memory_mergeable: Some(false),
            memory_hugepages: Some(true),
            memory_shared: Some(false),
            memory_prefault: Some(true),
            iommu: Some(false),
            rng_src: Some("/dev/hwrng".to_string()),
            watchdog: Some(true),
            landlock_enable: Some(false),
            serial_mode: Some("Null".to_string()),
            console_mode: Some("Pty".to_string()),
            pvpanic: Some(true),
            tpm_type: Some("swtpm".to_string()),
            tpm_socket_path: Some("/run/tpm.sock".to_string()),
        };
        let value = serde_json::to_value(&tuning).unwrap();
        let keys: std::collections::BTreeSet<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        // serde_json objects are key-sorted (BTreeMap); compare as sets. The
        // pin is set equality — a field added to the struct (or to the list
        // alone) breaks it.
        assert_eq!(
            keys,
            HypervisorTuning::KNOWN_FIELDS.iter().copied().collect()
        );
    }

    #[test]
    fn hypervisor_tuning_validation_rejects_invalid_semantics() {
        // A fully valid tuning passes.
        let valid = HypervisorTuning {
            rng_src: Some("/dev/hwrng".to_string()),
            serial_mode: Some("Null".to_string()),
            console_mode: Some("Pty".to_string()),
            tpm_type: Some("swtpm".to_string()),
            tpm_socket_path: Some("/run/tpm.sock".to_string()),
            ..Default::default()
        };
        assert!(valid.validate().is_ok());
        assert!(HypervisorTuning::default().validate().is_ok());
        // The Socket serial transport — the agent's default console
        // transport on cloud-hypervisor v43 — is an accepted value.
        assert!(HypervisorTuning {
            serial_mode: Some("Socket".to_string()),
            ..Default::default()
        }
        .validate()
        .is_ok());
        // Each rule rejects exactly its own violation.
        for (tuning, field) in [
            (
                HypervisorTuning {
                    rng_src: Some(String::new()),
                    ..Default::default()
                },
                "rng_src",
            ),
            (
                HypervisorTuning {
                    rng_src: Some("relative/path".to_string()),
                    ..Default::default()
                },
                "rng_src",
            ),
            (
                HypervisorTuning {
                    serial_mode: Some("Hero".to_string()),
                    ..Default::default()
                },
                "serial_mode",
            ),
            (
                HypervisorTuning {
                    console_mode: Some("Hero".to_string()),
                    ..Default::default()
                },
                "console_mode",
            ),
            (
                HypervisorTuning {
                    tpm_type: Some("tpm2".to_string()),
                    ..Default::default()
                },
                "tpm_type",
            ),
            (
                HypervisorTuning {
                    tpm_socket_path: Some("/run/tpm.sock".to_string()),
                    ..Default::default()
                },
                "tpm_socket_path",
            ),
        ] {
            let error = tuning.validate().unwrap_err();
            assert!(
                error.to_string().contains(field),
                "{field} violation must name the field: {error}"
            );
        }
    }

    #[test]
    fn vm_definition_validation_rejects_invalid_tuning() {
        let mut definition = VmDefinition {
            id: VmId::new("vm-1").unwrap(),
            name: "test".to_string(),
            boot: BootSpec::new("kernel-ref").unwrap(),
            compute: ComputeSpec::new(2, 1_073_741_824).unwrap(),
            storage: vec![],
            networks: vec![],
            requested_power_state: RequestedPowerState::Stopped,
            observed_power_state: ObservedPowerState::Unknown,
            resource_version: ResourceVersion::new(1).unwrap(),
            cloud_init_userdata: None,
            hypervisor_tuning: None,
            vsock: None,
        };
        definition.hypervisor_tuning = Some(HypervisorTuning {
            serial_mode: Some("Hero".to_string()),
            ..Default::default()
        });
        assert!(definition.validate().is_err());
    }

    #[test]
    fn vm_definition_deserialization_rejects_invalid_nested_values() {
        let base = serde_json::json!({
            "id": "vm-1",
            "name": "test",
            "boot": {"kernel": "kernel-ref", "firmware": null, "initial_disk": null},
            "compute": {"vcpus": 2, "memory_bytes": 1024},
            "storage": [{"attachment_id": "disk-0", "storage_ref": "volume-1", "read_only": false}],
            "networks": [{"attachment_id": "nic-0", "network_ref": "network-1", "mac_address": null}],
            "requested_power_state": "stopped",
            "observed_power_state": "unknown",
            "resource_version": 1
        });
        for (pointer, invalid) in [
            ("/name", serde_json::json!(" ")),
            ("/boot/kernel", serde_json::json!("")),
            ("/compute/vcpus", serde_json::json!(0)),
            ("/compute/memory_bytes", serde_json::json!(0)),
            ("/storage/0/attachment_id", serde_json::json!("")),
            ("/storage/0/storage_ref", serde_json::json!(" ")),
            ("/networks/0/attachment_id", serde_json::json!("")),
            ("/networks/0/network_ref", serde_json::json!("")),
        ] {
            let mut candidate = base.clone();
            *candidate.pointer_mut(pointer).unwrap() = invalid;
            assert!(
                serde_json::from_value::<VmDefinition>(candidate).is_err(),
                "accepted invalid value at {pointer}"
            );
        }
    }

    #[test]
    fn vm_definition_rejects_attachment_ids_duplicated_across_kinds() {
        let mut definition = serde_json::from_value::<VmDefinition>(serde_json::json!({
            "id":"vm-1", "name":"test",
            "boot":{"kernel":"kernel","firmware":null,"initial_disk":null},
            "compute":{"vcpus":1,"memory_bytes":1},
            "storage":[{"attachment_id":"device-0","storage_ref":"volume","read_only":false}],
            "networks":[], "requested_power_state":"stopped",
            "observed_power_state":"unknown", "resource_version":1
        }))
        .unwrap();
        definition.networks.push(NetworkAttachmentRef {
            attachment_id: "device-0".to_owned(),
            network_ref: "network".to_owned(),
            mac_address: None,
            addressing: None,
            firewall_policy_json: None,
        });
        assert!(definition.validate().is_err());
        assert!(
            serde_json::from_value::<VmDefinition>(serde_json::to_value(definition).unwrap())
                .is_err()
        );
    }

    #[test]
    fn vm_definition_rejects_path_unsafe_id() {
        for unsafe_id in ["a/b", "a\\b", "a\0b", ".", ".."] {
            let mut definition = serde_json::from_value::<VmDefinition>(serde_json::json!({
                "id":"vm-1", "name":"test",
                "boot":{"kernel":"kernel","firmware":null,"initial_disk":null},
                "compute":{"vcpus":1,"memory_bytes":1},
                "storage":[], "networks":[], "requested_power_state":"stopped",
                "observed_power_state":"unknown", "resource_version":1
            }))
            .unwrap();
            definition.id = VmId::new(unsafe_id).unwrap();
            assert!(
                definition.validate().is_err(),
                "accepted path-unsafe vm id {unsafe_id:?}"
            );
            let encoded = serde_json::to_value(&definition).unwrap();
            assert!(
                serde_json::from_value::<VmDefinition>(encoded).is_err(),
                "deserialization accepted path-unsafe vm id {unsafe_id:?}"
            );
        }
    }

    #[test]
    fn storage_attachment_ref_rejects_path_unsafe() {
        for (pointer, invalid) in [
            ("/storage/0/attachment_id", serde_json::json!("a/../../b")),
            ("/storage/0/storage_ref", serde_json::json!("a/../b")),
            ("/storage/0/storage_ref", serde_json::json!("a\\b")),
            ("/storage/0/storage_ref", serde_json::json!("\0bad")),
            ("/storage/0/storage_ref", serde_json::json!(".")),
            ("/storage/0/storage_ref", serde_json::json!("..")),
        ] {
            let base = serde_json::json!({
                "id": "vm-1", "name": "test",
                "boot": {"kernel": "kernel-ref", "firmware": null, "initial_disk": null},
                "compute": {"vcpus": 2, "memory_bytes": 1024},
                "storage": [{"attachment_id": "disk-0", "storage_ref": "volume-1", "read_only": false}],
                "networks": [], "requested_power_state": "stopped",
                "observed_power_state": "unknown", "resource_version": 1
            });
            let mut candidate = base.clone();
            *candidate.pointer_mut(pointer).unwrap() = invalid.clone();
            let parsed = serde_json::from_value::<VmDefinition>(candidate);
            assert!(
                parsed.is_err() || parsed.unwrap().validate().is_err(),
                "accepted path-unsafe storage value at {pointer}: {invalid}"
            );
        }
    }

    #[test]
    fn network_attachment_ref_rejects_path_unsafe() {
        for (pointer, invalid) in [
            ("/networks/0/attachment_id", serde_json::json!("nic/../x")),
            ("/networks/0/network_ref", serde_json::json!("../evil")),
            ("/networks/0/network_ref", serde_json::json!("a\\b")),
            ("/networks/0/network_ref", serde_json::json!("\0bad")),
            ("/networks/0/network_ref", serde_json::json!(".")),
            ("/networks/0/network_ref", serde_json::json!("..")),
        ] {
            let base = serde_json::json!({
                "id": "vm-1", "name": "test",
                "boot": {"kernel": "kernel-ref", "firmware": null, "initial_disk": null},
                "compute": {"vcpus": 2, "memory_bytes": 1024},
                "storage": [], "requested_power_state": "stopped",
                "observed_power_state": "unknown", "resource_version": 1,
                "networks": [{"attachment_id": "nic-0", "network_ref": "network-1", "mac_address": null}]
            });
            let mut candidate = base.clone();
            *candidate.pointer_mut(pointer).unwrap() = invalid.clone();
            let parsed = serde_json::from_value::<VmDefinition>(candidate);
            assert!(
                parsed.is_err() || parsed.unwrap().validate().is_err(),
                "accepted path-unsafe network value at {pointer}: {invalid}"
            );
        }
    }

    #[test]
    fn network_attachment_policy_snapshot_roundtrip_and_normalization() {
        // #355: the firewall policy snapshot rides on the attachment.
        // Absent must keep parsing (old journal entries), an empty string
        // normalizes to None (no Some("") sentinels in durable state),
        // and a real snapshot round-trips verbatim.
        let base = serde_json::json!({
            "id": "vm-1", "name": "test",
            "boot": {"kernel": "kernel-ref", "firmware": null, "initial_disk": null},
            "compute": {"vcpus": 2, "memory_bytes": 1024},
            "storage": [], "requested_power_state": "stopped",
            "observed_power_state": "unknown", "resource_version": 1,
            "networks": [{"attachment_id": "nic-0", "network_ref": "network-1", "mac_address": null}]
        });

        // Absent (pre-#355 journal entries) → None.
        let parsed = serde_json::from_value::<VmDefinition>(base.clone())
            .expect("old journal shape must still parse");
        assert_eq!(parsed.networks[0].firewall_policy_json, None);

        // Empty / whitespace-only → normalized to None.
        for empty in ["", "   "] {
            let mut candidate = base.clone();
            candidate["networks"][0]["firewall_policy_json"] = serde_json::json!(empty);
            let parsed =
                serde_json::from_value::<VmDefinition>(candidate).expect("empty policy must parse");
            assert_eq!(
                parsed.networks[0].firewall_policy_json, None,
                "empty policy must normalize to None"
            );
        }

        // A real snapshot round-trips verbatim.
        let policy = r#"[{"direction":"inbound","action":"accept","protocol":"icmp"}]"#;
        let mut candidate = base.clone();
        candidate["networks"][0]["firewall_policy_json"] = serde_json::json!(policy);
        let parsed = serde_json::from_value::<VmDefinition>(candidate).expect("policy must parse");
        assert_eq!(
            parsed.networks[0].firewall_policy_json.as_deref(),
            Some(policy)
        );
        let reserialized = serde_json::to_value(&parsed).expect("serialize back");
        assert_eq!(
            reserialized["networks"][0]["firewall_policy_json"],
            serde_json::json!(policy)
        );
    }

    #[test]
    fn operation_deserialization_enforces_fingerprint_and_attempt_bounds() {
        let base = serde_json::json!({
            "id": "op-1", "kind": "start_vm", "vm_id": "vm-1",
            "status": "accepted", "request_fingerprint": "sha256:value",
            "attempt_count": 0, "max_attempts": 3
        });
        for (pointer, invalid) in [
            ("/request_fingerprint", serde_json::json!("")),
            ("/max_attempts", serde_json::json!(0)),
            ("/attempt_count", serde_json::json!(4)),
        ] {
            let mut candidate = base.clone();
            *candidate.pointer_mut(pointer).unwrap() = invalid;
            assert!(serde_json::from_value::<Operation>(candidate).is_err());
        }
    }

    #[test]
    fn operation_request_metadata_validation_contract() {
        let valid = OperationRequestMetadata {
            requested_by: "controller-a".to_owned(),
            external_operation_id: "op-42".to_owned(),
            request_unix_ms: 1_700_000_000_000,
            legacy_generation: None,
        };
        assert_eq!(valid.clone().validate(), Ok(()));
        let legacy = OperationRequestMetadata {
            legacy_generation: Some(7),
            ..valid.clone()
        };
        assert_eq!(legacy.validate(), Ok(()));
        // Values exactly at each input cap are still accepted.
        assert_eq!(
            OperationRequestMetadata {
                request_unix_ms: MAX_REQUEST_UNIX_MS,
                ..valid.clone()
            }
            .validate(),
            Ok(())
        );
        assert_eq!(
            OperationRequestMetadata {
                legacy_generation: Some(i64::MAX as u64),
                ..valid.clone()
            }
            .validate(),
            Ok(())
        );
        for label in [
            "whitespace requester",
            "over-long requester",
            "empty external id",
            "over-long external id",
            "zero timestamp",
            "over-max timestamp",
            "zero generation",
            "over-range generation",
        ] {
            let mut candidate = valid.clone();
            match label {
                "whitespace requester" => candidate.requested_by = "   ".to_owned(),
                "over-long requester" => candidate.requested_by = "x".repeat(1025),
                "empty external id" => candidate.external_operation_id = String::new(),
                "over-long external id" => candidate.external_operation_id = "x".repeat(1025),
                "zero timestamp" => candidate.request_unix_ms = 0,
                "over-max timestamp" => candidate.request_unix_ms = MAX_REQUEST_UNIX_MS + 1,
                "zero generation" => candidate.legacy_generation = Some(0),
                "over-range generation" => candidate.legacy_generation = Some(i64::MAX as u64 + 1),
                _ => unreachable!(),
            }
            assert!(candidate.validate().is_err(), "accepted invalid {label}");
        }
        assert!(
            serde_json::from_str::<OperationRequestMetadata>(
                r#"{"requested_by":"a","external_operation_id":"b","request_unix_ms":1,"legacy_generation":null,"extra":1}"#
            )
            .is_err(),
            "unknown request metadata fields must be rejected"
        );
    }

    #[test]
    fn event_and_ownership_deserialization_reject_zero_versions() {
        let event = serde_json::json!({
            "id": "event-1", "sequence": 0, "operation_id": null,
            "vm_id": null, "kind": "accepted", "payload": {}
        });
        assert!(serde_json::from_value::<OperationEvent>(event).is_err());

        let marker = serde_json::json!({
            "vm_id": "vm-1", "owner_id": "host-1", "ownership": "cell_hv",
            "recovery": "owned_recoverable", "marker_version": 0
        });
        assert!(serde_json::from_value::<OwnershipMarker>(marker).is_err());
    }
}
