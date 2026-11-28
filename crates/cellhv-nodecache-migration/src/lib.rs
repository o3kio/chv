//! Read-only compatibility importer for the version-1 `chv-agent` NodeCache.
//!
//! This crate never writes or removes the source cache and performs no VM,
//! provider, or VMM side effects. Callers must archive the exact source bytes
//! before requesting cutover.

use cellhv_core_operations::{MigrationDisposition, OperationService, OperationServiceError};
use cellhv_core_types::{
    BootSpec, ComputeSpec, HostId, HostIdentity, NetworkAttachmentRef, ObservedPowerState,
    RequestedPowerState, ResourceVersion, StorageAttachmentRef, VmDefinition, VmId,
    VsockDeviceSpec,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

pub const SOURCE_NAME: &str = "chv-agent-node-cache-v1";

pub fn legacy_storage_attachment_id(volume_id: &str) -> String {
    volume_id.to_owned()
}

pub fn legacy_network_attachment_id(vm_id: &str, network_id: &str) -> String {
    format!("{vm_id}-{network_id}")
}

#[derive(Debug, Error)]
pub enum MigrationError {
    #[error("malformed NodeCache: {0}")]
    Malformed(String),
    #[error("NodeCache contains unsupported data at {0}")]
    Unsupported(String),
    #[error(transparent)]
    Operations(#[from] OperationServiceError),
}

pub type Result<T> = std::result::Result<T, MigrationError>;

#[derive(Debug, Clone)]
pub struct ImportPlan {
    checksum: String,
    host: HostIdentity,
    definitions: Vec<VmDefinition>,
}

impl ImportPlan {
    pub fn checksum(&self) -> &str {
        &self.checksum
    }
    pub fn host(&self) -> &HostIdentity {
        &self.host
    }
    pub fn definitions(&self) -> &[VmDefinition] {
        &self.definitions
    }

    pub fn import(&self, service: &mut OperationService) -> Result<MigrationDisposition> {
        Ok(service.import_legacy_snapshot(
            SOURCE_NAME,
            &self.checksum,
            &self.host,
            &self.definitions,
        )?)
    }

    pub fn cutover(&self, service: &mut OperationService) -> Result<MigrationDisposition> {
        Ok(service.cutover_legacy_snapshot(SOURCE_NAME, &self.checksum)?)
    }
}

pub fn plan(source: &[u8]) -> Result<ImportPlan> {
    let cache: NodeCacheV1 = serde_json::from_slice(source)
        .map_err(|error| MigrationError::Malformed(error.to_string()))?;
    if cache.cache_version != 1 {
        return Err(malformed("cache_version must equal 1"));
    }
    validate_core_host_id(&cache.node_id)?;
    nonempty("observed_generation", &cache.observed_generation)?;
    nonempty("node_state", &cache.node_state)?;
    validate_optional_path("certificate_path", cache.certificate_path.as_deref())?;
    validate_optional_path("private_key_path", cache.private_key_path.as_deref())?;
    validate_optional_path("ca_path", cache.ca_path.as_deref())?;
    if cache
        .last_certificate_rotation_unix_ms
        .is_some_and(|value| value < 0)
    {
        return Err(malformed(
            "last_certificate_rotation_unix_ms must not be negative",
        ));
    }
    let _compatibility_metadata = (cache.enrollment_complete, cache.last_error.as_deref());
    if !cache.volume_handles.is_empty() {
        return Err(unsupported("volume_handles"));
    }
    if !cache.pending_control_plane.is_empty() {
        return Err(unsupported("pending_control_plane"));
    }
    validate_auxiliary_fragments("volume", &cache.volume_fragments, &cache.volume_generations)?;
    validate_auxiliary_fragments(
        "network",
        &cache.network_fragments,
        &cache.network_generations,
    )?;

    let fragment_ids = cache.vm_fragments.keys().cloned().collect::<BTreeSet<_>>();
    let generation_ids = cache
        .vm_generations
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    if fragment_ids != generation_ids {
        return Err(malformed(
            "vm_fragments and vm_generations must contain identical VM IDs",
        ));
    }
    let attachment_ids = cache
        .vm_attachments
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    if !attachment_ids.is_subset(&fragment_ids) {
        return Err(malformed(
            "vm_attachments contains a VM without a VM fragment",
        ));
    }

    let mut definitions = Vec::with_capacity(cache.vm_fragments.len());
    for (map_id, fragment) in &cache.vm_fragments {
        nonempty("vm_fragments key", map_id)?;
        if fragment.id != *map_id || fragment.kind != "vm" {
            return Err(malformed(&format!(
                "fragment {map_id} identity or kind mismatch"
            )));
        }
        validate_fragment_metadata(map_id, fragment)?;
        let generation = cache.vm_generations.get(map_id).expect("sets matched");
        if fragment.generation != *generation {
            return Err(malformed(&format!("fragment {map_id} generation mismatch")));
        }
        let version_num = generation
            .parse::<u64>()
            .map_err(|_| malformed(&format!("VM {map_id} generation is not numeric")))?;
        let version = ResourceVersion::new(version_num)
            .map_err(|error| malformed(&format!("VM {map_id} generation: {error}")))?;
        let spec: LegacyVmSpec = serde_json::from_slice(&fragment.spec_json)
            .map_err(|error| malformed(&format!("VM {map_id} spec_json: {error}")))?;
        definitions.push(convert_vm(
            map_id,
            version,
            spec,
            cache.vm_attachments.get(map_id),
        )?);
    }
    definitions.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(ImportPlan {
        checksum: hex::encode(Sha256::digest(source)),
        host: HostIdentity {
            id: HostId::new(cache.node_id).map_err(|error| malformed(&error.to_string()))?,
            resource_version: ResourceVersion::new(1).expect("one is valid"),
        },
        definitions,
    })
}

fn convert_vm(
    id: &str,
    version: ResourceVersion,
    spec: LegacyVmSpec,
    observed: Option<&LegacyAttachments>,
) -> Result<VmDefinition> {
    // Legacy fields Core does not model are still rejected explicitly —
    // never silently dropped. Cloud-init userdata and the hypervisor
    // override surface ARE modeled now (VmDefinition carries them) and are
    // translated below.
    let hypervisor_tuning = spec
        .hypervisor_overrides
        .map(|value| parse_hypervisor_tuning(id, value))
        .transpose()?;
    // G5 vsock opt-in: carried across the one-time legacy-cache → Core
    // migration verbatim (a VM created with the device before the
    // migration keeps it; the definition validator re-checks the
    // enabled marker).
    let vsock = spec.vsock;
    let requested_power_state = match spec.desired_state.as_str() {
        "Running" => RequestedPowerState::Running,
        "Stopped" => RequestedPowerState::Stopped,
        other => {
            return Err(malformed(&format!(
                "VM {id} desired_state {other:?} is invalid"
            )))
        }
    };
    // An empty (or whitespace) seed path is semantically absent — the same
    // normalization the adapter applies, keeping the durable definition free
    // of Some("") sentinel values.
    let seed_path = spec
        .disk_seed_path
        .as_deref()
        .filter(|path| !path.trim().is_empty())
        .map(str::to_string);
    let mut storage = Vec::new();
    for (index, disk) in spec.disks.into_iter().enumerate() {
        nonempty("disk.volume_id", &disk.volume_id)?;
        storage.push(StorageAttachmentRef {
            attachment_id: legacy_storage_attachment_id(&disk.volume_id),
            storage_ref: disk.volume_id,
            read_only: disk.read_only,
            size_bytes: disk.size_bytes,
            // #379 PR 2 (A7): the per-disk stord backend class rides the
            // imported definition so the Core executor's volume opens keep
            // dispatching with the class the legacy spec carried — absent
            // stays absent (the executor's "local" default), never
            // materialized.
            backend_class: disk.backend_class,
            // The legacy seed path is per-VM and applies to the boot disk
            // only — a deliberate divergence from the legacy reconcile
            // loop, which would seed every absent disk from the same
            // image; existing (already-provisioned) volumes skip seeding
            // in stord either way.
            seed_from: if index == 0 { seed_path.clone() } else { None },
        });
    }
    let mut networks = Vec::new();
    for (index, nic) in spec.nics.into_iter().enumerate() {
        if !nic.tap_name.is_empty() {
            return Err(unsupported(&format!(
                "vm_fragments.{id}.spec_json.nics[{index}].tap_name"
            )));
        }
        nonempty("nic.network_id", &nic.network_id)?;
        nonempty("nic.mac_address", &nic.mac_address)?;
        let addressing =
            if nic.ip_address.is_empty() && nic.cidr.is_empty() && nic.gateway.is_empty() {
                None
            } else {
                Some(cellhv_core_types::NicAddressing {
                    ip_address: nic.ip_address,
                    cidr: nic.cidr,
                    gateway: nic.gateway,
                })
            };
        networks.push(NetworkAttachmentRef {
            attachment_id: legacy_network_attachment_id(id, &nic.network_id),
            network_ref: nic.network_id,
            mac_address: Some(nic.mac_address),
            addressing,
            // Legacy nodecache entries carry no policy snapshot —
            // attach-time policy application (#355) applies only to specs
            // dispatched after the field existed.
            firewall_policy_json: None,
        });
    }
    validate_attachment_projection(id, &storage, &networks, observed)?;
    let definition = VmDefinition {
        id: VmId::new(id.to_owned()).map_err(|error| malformed(&error.to_string()))?,
        name: spec.name,
        boot: BootSpec {
            kernel: spec.kernel_path,
            firmware: spec.firmware_path,
            initial_disk: seed_path,
        },
        compute: ComputeSpec::new(spec.cpus, spec.memory_bytes)
            .map_err(|error| malformed(&error.to_string()))?,
        storage,
        networks,
        requested_power_state,
        observed_power_state: ObservedPowerState::Unknown,
        resource_version: version,
        cloud_init_userdata: spec.cloud_init_userdata,
        hypervisor_tuning,
        vsock,
    };
    definition
        .validate()
        .map_err(|error| malformed(&error.to_string()))?;
    Ok(definition)
}

/// Parse and semantically validate the legacy hypervisor override surface.
/// The error taxonomy is deliberate: a non-object value or a wrongly-typed
/// field is malformed legacy data; a key outside Core's tuning mirror
/// ([`cellhv_core_types::HypervisorTuning::KNOWN_FIELDS`]) is unsupported —
/// Core does not model it and it must never be silently dropped; a value
/// failing the semantic rules (`rng_src`, `serial_mode`, `console_mode`,
/// `tpm_type`, tpm pairing) is malformed, matching what the legacy spec
/// boundary rejects.
fn parse_hypervisor_tuning(
    id: &str,
    value: serde_json::Value,
) -> Result<cellhv_core_types::HypervisorTuning> {
    let field = format!("vm_fragments.{id}.spec_json.hypervisor_overrides");
    let Some(map) = value.as_object() else {
        return Err(malformed(&format!("{field}: expected an object")));
    };
    for key in map.keys() {
        if !cellhv_core_types::HypervisorTuning::KNOWN_FIELDS.contains(&key.as_str()) {
            return Err(unsupported(&format!("{field}.{key}")));
        }
    }
    let tuning: cellhv_core_types::HypervisorTuning =
        serde_json::from_value(value).map_err(|error| malformed(&format!("{field}: {error}")))?;
    tuning
        .validate()
        .map_err(|error| malformed(&format!("{field}: {error}")))?;
    Ok(tuning)
}

fn validate_attachment_projection(
    id: &str,
    storage: &[StorageAttachmentRef],
    networks: &[NetworkAttachmentRef],
    observed: Option<&LegacyAttachments>,
) -> Result<()> {
    let Some(observed) = observed else {
        return Ok(());
    };
    let expected_volumes = storage
        .iter()
        .map(|item| item.storage_ref.as_str())
        .collect::<BTreeSet<_>>();
    let actual_volumes = observed
        .volume_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if expected_volumes != actual_volumes {
        return Err(malformed(&format!(
            "VM {id} observed volume attachments disagree with its spec"
        )));
    }
    let expected_nics = networks
        .iter()
        .map(|item| item.network_ref.as_str())
        .collect::<BTreeSet<_>>();
    let actual_nics = observed
        .nics
        .iter()
        .map(|item| item.network_id.as_str())
        .collect::<BTreeSet<_>>();
    if expected_nics != actual_nics {
        return Err(malformed(&format!(
            "VM {id} observed network attachments disagree with its spec"
        )));
    }
    for nic in &observed.nics {
        if nic.nic_id != legacy_network_attachment_id(id, &nic.network_id) {
            return Err(malformed(&format!(
                "VM {id} observed NIC {} has a non-deterministic identity",
                nic.nic_id
            )));
        }
    }
    Ok(())
}

fn nonempty(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        Err(malformed(&format!("{field} must not be empty")))
    } else {
        Ok(())
    }
}
fn validate_core_host_id(value: &str) -> Result<()> {
    nonempty("node_id", value)?;
    if matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "unknown" | "unset" | "none" | "null"
    ) {
        return Err(malformed("node_id is a reserved Core identity placeholder"));
    }
    Ok(())
}
fn validate_optional_path(field: &str, value: Option<&str>) -> Result<()> {
    if value.is_some_and(|item| item.trim().is_empty()) {
        Err(malformed(&format!(
            "{field} must not be empty when present"
        )))
    } else {
        Ok(())
    }
}
fn validate_fragment_metadata(id: &str, fragment: &LegacyFragment) -> Result<()> {
    nonempty("fragment.generation", &fragment.generation)?;
    nonempty("fragment.updated_at", &fragment.updated_at)?;
    nonempty("fragment.updated_by", &fragment.updated_by)?;
    // Every real producer writes an EMPTY policy_json for fragments (the
    // control plane's node_client has sent `vec![]` since the first commit,
    // and the M2.2b projection writes `Vec::new()`): empty means "no
    // policy". Requiring it to parse as JSON rejected every real cache —
    // a latent bug surfaced by the projection round-trip test. Non-empty
    // policy must still be valid JSON.
    if !fragment.policy_json.is_empty() {
        serde_json::from_slice::<serde_json::Value>(&fragment.policy_json)
            .map_err(|error| malformed(&format!("fragment {id} policy_json: {error}")))?;
    }
    Ok(())
}
fn validate_auxiliary_fragments(
    kind: &str,
    fragments: &BTreeMap<String, LegacyFragment>,
    generations: &BTreeMap<String, String>,
) -> Result<()> {
    if fragments.keys().collect::<BTreeSet<_>>() != generations.keys().collect::<BTreeSet<_>>() {
        return Err(malformed(&format!(
            "{kind}_fragments and {kind}_generations must contain identical IDs"
        )));
    }
    for (id, fragment) in fragments {
        if fragment.id != *id
            || fragment.kind != kind
            || generations.get(id) != Some(&fragment.generation)
        {
            return Err(malformed(&format!(
                "{kind} fragment {id} identity, kind, or generation mismatch"
            )));
        }
        validate_fragment_metadata(id, fragment)?;
        serde_json::from_slice::<serde_json::Value>(&fragment.spec_json)
            .map_err(|error| malformed(&format!("fragment {id} spec_json: {error}")))?;
    }
    Ok(())
}
fn malformed(reason: &str) -> MigrationError {
    MigrationError::Malformed(reason.to_owned())
}
fn unsupported(path: &str) -> MigrationError {
    MigrationError::Unsupported(path.to_owned())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeCacheV1 {
    cache_version: u32,
    node_id: String,
    observed_generation: String,
    node_state: String,
    #[serde(default)]
    enrollment_complete: bool,
    #[serde(default)]
    certificate_path: Option<String>,
    #[serde(default)]
    private_key_path: Option<String>,
    #[serde(default)]
    ca_path: Option<String>,
    #[serde(default)]
    last_certificate_rotation_unix_ms: Option<i64>,
    vm_generations: BTreeMap<String, String>,
    volume_generations: BTreeMap<String, String>,
    network_generations: BTreeMap<String, String>,
    vm_fragments: BTreeMap<String, LegacyFragment>,
    volume_fragments: BTreeMap<String, LegacyFragment>,
    network_fragments: BTreeMap<String, LegacyFragment>,
    #[serde(default)]
    vm_attachments: BTreeMap<String, LegacyAttachments>,
    #[serde(default)]
    volume_handles: BTreeMap<String, String>,
    #[serde(default)]
    pending_control_plane: Vec<serde_json::Value>,
    #[serde(default)]
    last_error: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyFragment {
    id: String,
    kind: String,
    generation: String,
    spec_json: Vec<u8>,
    policy_json: Vec<u8>,
    updated_at: String,
    updated_by: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyAttachments {
    #[serde(default)]
    volume_ids: Vec<String>,
    #[serde(default)]
    nics: Vec<LegacyNicAttachment>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyNicAttachment {
    nic_id: String,
    network_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyVmSpec {
    name: String,
    cpus: u32,
    memory_bytes: u64,
    kernel_path: String,
    #[serde(default)]
    firmware_path: Option<String>,
    #[serde(default)]
    disk_seed_path: Option<String>,
    disks: Vec<LegacyDisk>,
    nics: Vec<LegacyNic>,
    #[serde(default = "running")]
    desired_state: String,
    #[serde(default)]
    cloud_init_userdata: Option<String>,
    #[serde(default)]
    hypervisor_overrides: Option<serde_json::Value>,
    /// G5: the vsock device opt-in. `deny_unknown_fields` means this
    /// MUST be declared — a cache written by a post-G5 agent (whose VM
    /// fragments carry the key once the CP produces it) would otherwise
    /// be rejected as unsupported data on the core-activation import
    /// path. Carried one-to-one into the Core definition.
    #[serde(default)]
    vsock: Option<VsockDeviceSpec>,
}
fn running() -> String {
    "Running".to_owned()
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyDisk {
    volume_id: String,
    #[serde(default)]
    read_only: bool,
    #[serde(default)]
    size_bytes: Option<u64>,
    /// #379 PR 2 (A7): the per-disk stord backend class. `deny_unknown_fields`
    /// means this MUST be declared — a cache written by a post-#379 agent
    /// (whose VM fragments carry the key once the CP produces it) would
    /// otherwise be rejected as unsupported data on the core-activation
    /// import path.
    #[serde(default)]
    backend_class: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyNic {
    network_id: String,
    mac_address: String,
    ip_address: String,
    #[serde(default)]
    tap_name: String,
    #[serde(default)]
    cidr: String,
    #[serde(default)]
    gateway: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn source(generation: &str) -> Vec<u8> {
        let spec = serde_json::to_vec(&json!({
            "name":"legacy-vm", "cpus":2, "memory_bytes":1073741824_u64,
            "kernel_path":"/var/lib/chv/vmlinux", "disks":[], "nics":[],
            "desired_state":"Running"
        }))
        .unwrap();
        serde_json::to_vec(&json!({
            "cache_version":1, "node_id":"node-a", "observed_generation":"7",
            "node_state":"TenantReady", "enrollment_complete":true,
            "vm_generations":{"vm-a":generation}, "volume_generations":{},
            "network_generations":{},
            "vm_fragments":{"vm-a":{"id":"vm-a","kind":"vm","generation":generation,
                "spec_json":spec,"policy_json":b"{}","updated_at":"2026-07-21T00:00:00Z","updated_by":"controller"}},
            "volume_fragments":{}, "network_fragments":{}, "vm_attachments":{},
            "volume_handles":{}, "pending_control_plane":[]
        })).unwrap()
    }

    fn add_vm(value: &mut serde_json::Value, id: &str, generation: &str, cpus: u64) {
        let spec = serde_json::to_vec(&json!({
            "name":id, "cpus":cpus, "memory_bytes":536870912_u64,
            "kernel_path":"/kernel", "disks":[], "nics":[]
        }))
        .unwrap();
        value["vm_generations"][id] = json!(generation);
        value["vm_fragments"][id] = json!({
            "id":id,"kind":"vm","generation":generation,"spec_json":spec,
            "policy_json":b"{}","updated_at":"2026-07-21T00:00:00Z","updated_by":"controller"
        });
    }

    #[test]
    fn stable_identity_and_checksum() {
        let bytes = source("9");
        let first = plan(&bytes).unwrap();
        let second = plan(&bytes).unwrap();
        assert_eq!(first.checksum(), second.checksum());
        assert_eq!(first.host().id.as_str(), "node-a");
        assert_eq!(first.definitions()[0].id.as_str(), "vm-a");
        assert_eq!(first.definitions()[0].resource_version.get(), 9);
    }

    #[test]
    fn reserved_host_identity_placeholders_fail_before_import() {
        for placeholder in ["unknown", " UNKNOWN ", "Unset", "none", "NULL"] {
            let mut value: serde_json::Value = serde_json::from_slice(&source("1")).unwrap();
            value["node_id"] = json!(placeholder);
            let error = plan(&serde_json::to_vec(&value).unwrap()).unwrap_err();
            assert!(matches!(error, MigrationError::Malformed(_)));
            assert!(error
                .to_string()
                .contains("reserved Core identity placeholder"));
        }

        let mut value: serde_json::Value = serde_json::from_slice(&source("1")).unwrap();
        value["node_id"] = json!("unknown-host");
        assert_eq!(
            plan(&serde_json::to_vec(&value).unwrap())
                .unwrap()
                .host()
                .id
                .as_str(),
            "unknown-host"
        );
    }

    #[test]
    fn malformed_and_unrepresentable_data_fail_explicitly() {
        assert!(matches!(plan(b"{"), Err(MigrationError::Malformed(_))));
        let mut value: serde_json::Value = serde_json::from_slice(&source("1")).unwrap();
        value["vm_fragments"]["vm-a"]["generation"] = json!("2");
        assert!(matches!(
            plan(&serde_json::to_vec(&value).unwrap()),
            Err(MigrationError::Malformed(_))
        ));
        let mut spec: serde_json::Value = serde_json::from_slice(
            &value["vm_fragments"]["vm-a"]["spec_json"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_u64().unwrap() as u8)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        value["vm_fragments"]["vm-a"]["generation"] = json!("1");

        // Modeled legacy fields are carried, not dropped: cloud-init
        // userdata now translates into the Core definition.
        spec["cloud_init_userdata"] = json!("do not drop me");
        value["vm_fragments"]["vm-a"]["spec_json"] = json!(serde_json::to_vec(&spec).unwrap());
        let carried = plan(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(
            carried.definitions()[0].cloud_init_userdata.as_deref(),
            Some("do not drop me")
        );

        // Unmodeled legacy fields still fail closed instead of being
        // dropped: tap configuration is runtime-owned.
        spec["nics"] = json!([{
            "network_id":"net-0", "mac_address":"02:00:00:00:00:01",
            "ip_address":"", "tap_name":"tap-leftover",
            "cidr":"", "gateway":""
        }]);
        value["vm_fragments"]["vm-a"]["spec_json"] = json!(serde_json::to_vec(&spec).unwrap());
        assert!(matches!(
            plan(&serde_json::to_vec(&value).unwrap()),
            Err(MigrationError::Unsupported(_))
        ));
    }

    #[test]
    fn modeled_legacy_fields_translate_losslessly() {
        let mut value: serde_json::Value = serde_json::from_slice(&source("1")).unwrap();
        let mut spec: serde_json::Value = serde_json::from_slice(
            &value["vm_fragments"]["vm-a"]["spec_json"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_u64().unwrap() as u8)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        spec["disk_seed_path"] = json!("/var/lib/chv/images/ubuntu.img");
        spec["disks"] =
            json!([{"volume_id":"vol-a", "read_only":false, "size_bytes":10_737_418_240_u64}]);
        spec["nics"] = json!([{
            "network_id":"net-0", "mac_address":"02:00:00:00:00:01",
            "ip_address":"10.200.0.47", "tap_name":"",
            "cidr":"10.200.0.0/24", "gateway":"10.200.0.1"
        }]);
        spec["hypervisor_overrides"] = json!({"cpu_nested": true, "rng_src": "/dev/hwrng"});
        value["vm_fragments"]["vm-a"]["spec_json"] = json!(serde_json::to_vec(&spec).unwrap());
        let import = plan(&serde_json::to_vec(&value).unwrap()).unwrap();
        let definition = &import.definitions()[0];

        assert_eq!(
            definition.boot.initial_disk.as_deref(),
            Some("/var/lib/chv/images/ubuntu.img")
        );
        assert_eq!(definition.storage[0].size_bytes, Some(10_737_418_240));
        // A disk with no backend_class key imports as absent (the
        // executor's default), never materialized.
        assert_eq!(definition.storage[0].backend_class, None);
        // The per-VM legacy seed path seeds the boot disk attachment.
        assert_eq!(
            definition.storage[0].seed_from.as_deref(),
            Some("/var/lib/chv/images/ubuntu.img")
        );
        let addressing = definition.networks[0]
            .addressing
            .as_ref()
            .expect("addressing must be carried");
        assert_eq!(addressing.ip_address, "10.200.0.47");
        assert_eq!(addressing.cidr, "10.200.0.0/24");
        assert_eq!(addressing.gateway, "10.200.0.1");
        let tuning = definition
            .hypervisor_tuning
            .as_ref()
            .expect("tuning must be carried");
        assert_eq!(tuning.cpu_nested, Some(true));
        assert_eq!(tuning.rng_src.as_deref(), Some("/dev/hwrng"));

        // An override key the Core mirror does not know is still rejected,
        // never silently dropped.
        spec["hypervisor_overrides"] = json!({"made_up_flag": true});
        value["vm_fragments"]["vm-a"]["spec_json"] = json!(serde_json::to_vec(&spec).unwrap());
        assert!(matches!(
            plan(&serde_json::to_vec(&value).unwrap()),
            Err(MigrationError::Unsupported(_))
        ));
    }

    #[test]
    fn vsock_opt_in_imports_and_pre_g5_caches_stay_deviceless() {
        // G5: the vsock opt-in is declared on LegacyVmSpec — a post-G5
        // cache (whose spec_json carries the key once the CP produces
        // it) imports instead of failing as unsupported data — and is
        // carried one-to-one into the Core definition. A pre-G5 cache
        // (no key) imports deviceless. The enabled:false double
        // negative is rejected by the definition validation the import
        // runs (never silently normalized to the opt-out).
        let mut value: serde_json::Value = serde_json::from_slice(&source("1")).unwrap();
        let mut spec: serde_json::Value = serde_json::from_slice(
            &value["vm_fragments"]["vm-a"]["spec_json"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_u64().unwrap() as u8)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        spec["vsock"] = json!({ "enabled": true });
        value["vm_fragments"]["vm-a"]["spec_json"] = json!(serde_json::to_vec(&spec).unwrap());
        let import = plan(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(
            import.definitions()[0].vsock,
            Some(VsockDeviceSpec { enabled: true })
        );

        // Pre-G5 cache: no key, no device.
        let plain = plan(&source("1")).unwrap();
        assert_eq!(plain.definitions()[0].vsock, None);

        // The double negative never crosses the import boundary.
        spec["vsock"] = json!({ "enabled": false });
        value["vm_fragments"]["vm-a"]["spec_json"] = json!(serde_json::to_vec(&spec).unwrap());
        assert!(plan(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    #[test]
    fn hypervisor_override_taxonomy_distinguishes_unsupported_from_malformed() {
        let mut value: serde_json::Value = serde_json::from_slice(&source("1")).unwrap();
        let mut spec: serde_json::Value = serde_json::from_slice(
            &value["vm_fragments"]["vm-a"]["spec_json"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_u64().unwrap() as u8)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let repack = |value: &mut serde_json::Value, spec: &serde_json::Value| {
            value["vm_fragments"]["vm-a"]["spec_json"] = json!(serde_json::to_vec(spec).unwrap());
            serde_json::to_vec(value).unwrap()
        };
        // A key outside Core's tuning mirror: unsupported — Core does not
        // model it and it must never be silently dropped.
        spec["hypervisor_overrides"] = json!({"made_up_flag": true});
        let error = plan(&repack(&mut value, &spec)).unwrap_err();
        assert!(matches!(error, MigrationError::Unsupported(_)));
        assert!(error.to_string().contains("made_up_flag"));
        // A wrongly-typed value: malformed legacy data.
        spec["hypervisor_overrides"] = json!({"cpu_nested": "yes"});
        assert!(matches!(
            plan(&repack(&mut value, &spec)).unwrap_err(),
            MigrationError::Malformed(_)
        ));
        // A non-object overrides value: malformed legacy data.
        spec["hypervisor_overrides"] = json!("nope");
        assert!(matches!(
            plan(&repack(&mut value, &spec)).unwrap_err(),
            MigrationError::Malformed(_)
        ));
        // A semantically invalid value: malformed — the same verdict the
        // legacy spec boundary (VmSpec::validate) would have given.
        spec["hypervisor_overrides"] = json!({"serial_mode": "Hero"});
        let error = plan(&repack(&mut value, &spec)).unwrap_err();
        assert!(matches!(error, MigrationError::Malformed(_)));
        assert!(error.to_string().contains("serial_mode"));
    }

    #[test]
    fn disk_backend_class_translates_and_absent_stays_absent() {
        // #379 PR 2 pin (A7 on the import surface): a legacy cache whose
        // VM fragment carries a per-disk backend_class (the shape a
        // post-#379 control plane produces) must import into the Core
        // definition with the class — `LegacyDisk`'s deny_unknown_fields
        // would otherwise reject the whole cache as unsupported data —
        // while a disk without the key keeps the absent default.
        let mut value: serde_json::Value = serde_json::from_slice(&source("1")).unwrap();
        let mut spec: serde_json::Value = serde_json::from_slice(
            &value["vm_fragments"]["vm-a"]["spec_json"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_u64().unwrap() as u8)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        spec["disks"] = json!([
            {"volume_id":"vol-cls", "read_only":false, "backend_class":"lvm"},
            {"volume_id":"vol-bare", "read_only":false}
        ]);
        value["vm_fragments"]["vm-a"]["spec_json"] = json!(serde_json::to_vec(&spec).unwrap());
        let import = plan(&serde_json::to_vec(&value).unwrap()).unwrap();
        let definition = &import.definitions()[0];
        assert_eq!(definition.storage.len(), 2);
        assert_eq!(
            definition.storage[0].backend_class.as_deref(),
            Some("lvm"),
            "the class must ride the imported definition"
        );
        assert_eq!(
            definition.storage[1].backend_class, None,
            "an absent class must stay absent, never materialized"
        );
    }

    #[test]
    fn empty_seed_path_is_treated_as_absent() {
        let mut value: serde_json::Value = serde_json::from_slice(&source("1")).unwrap();
        let mut spec: serde_json::Value = serde_json::from_slice(
            &value["vm_fragments"]["vm-a"]["spec_json"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_u64().unwrap() as u8)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        spec["disk_seed_path"] = json!("");
        spec["disks"] = json!([{"volume_id":"vol-a", "read_only":false}]);
        value["vm_fragments"]["vm-a"]["spec_json"] = json!(serde_json::to_vec(&spec).unwrap());
        let import = plan(&serde_json::to_vec(&value).unwrap()).unwrap();
        let definition = &import.definitions()[0];
        assert_eq!(definition.boot.initial_disk, None);
        assert_eq!(definition.storage[0].seed_from, None);
    }

    #[test]
    fn empty_policy_json_is_absent_not_malformed() {
        // Every real producer (the control plane's node_client and the M2.2b
        // projection) writes an EMPTY policy_json for fragments; the
        // migration must accept the format producers actually write.
        // Non-empty policy must still parse as JSON.
        let mut value: serde_json::Value = serde_json::from_slice(&source("1")).unwrap();
        value["vm_fragments"]["vm-a"]["policy_json"] = json!(Vec::<u8>::new());
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(plan(&bytes).is_ok());
        value["vm_fragments"]["vm-a"]["policy_json"] = json!(b"not json".to_vec());
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(matches!(
            plan(&bytes).unwrap_err(),
            MigrationError::Malformed(_)
        ));
    }

    #[test]
    fn import_replay_and_cutover_are_guarded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("core.db");
        let mut store = OperationService::create_migration_target(&path).unwrap();
        let import = plan(&source("3")).unwrap();
        assert_eq!(
            import.import(&mut store).unwrap(),
            MigrationDisposition::Imported
        );
        assert_eq!(
            import.import(&mut store).unwrap(),
            MigrationDisposition::Replay
        );
        assert_eq!(
            import.cutover(&mut store).unwrap(),
            MigrationDisposition::Cutover
        );
        assert_eq!(
            import.cutover(&mut store).unwrap(),
            MigrationDisposition::Cutover
        );
    }

    #[test]
    fn changed_source_cannot_replay_or_cut_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("core.db");
        let mut store = OperationService::create_migration_target(&path).unwrap();
        let first = plan(&source("3")).unwrap();
        let changed = plan(&source("4")).unwrap();
        first.import(&mut store).unwrap();
        assert!(matches!(
            changed.import(&mut store),
            Err(MigrationError::Operations(OperationServiceError::Store(_)))
        ));
        assert!(matches!(
            changed.cutover(&mut store),
            Err(MigrationError::Operations(OperationServiceError::Store(_)))
        ));
    }

    #[test]
    fn multi_vm_failure_is_preflight_atomic_and_defaults_are_stable() {
        let mut value: serde_json::Value = serde_json::from_slice(&source("1")).unwrap();
        add_vm(&mut value, "vm-b", "2", 0);
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(matches!(plan(&bytes), Err(MigrationError::Malformed(_))));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("core.db");
        let service = OperationService::create_migration_target(&path).unwrap();
        assert!(service.vms().unwrap().is_empty());

        value["vm_fragments"]["vm-b"]["spec_json"] = json!(serde_json::to_vec(&json!({
            "name":"vm-b", "cpus":1, "memory_bytes":536870912_u64,
            "kernel_path":"/kernel", "disks":[], "nics":[]
        }))
        .unwrap());
        let plan = plan(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(plan.definitions().len(), 2);
        assert!(plan
            .definitions()
            .iter()
            .all(|vm| vm.requested_power_state == RequestedPowerState::Running));
        assert_eq!(legacy_storage_attachment_id("vol-a"), "vol-a");
        assert_eq!(legacy_network_attachment_id("vm-a", "net-a"), "vm-a-net-a");
    }
}
