//! Single full-side-effect Core runtime: stord/nwd volume+NIC setup, VM
//! runtime directory, and Cloud Hypervisor create/start/stop/reboot/delete.
//!
//! `CloudHypervisorCoreRuntime` is the Core executor's effector (M2.2a). It
//! receives a journal [`OperationJournalEntry`] whose `request` is the
//! canonical envelope `{"command": {...}, "expected_vm_version": N}` (see
//! `canonical_request` in cellhv-core-operations) and performs every side
//! effect itself — it does NOT delegate volume/NIC preparation to the legacy
//! reconcile path.
//!
//! # Envelope parsing (latent-bug fix)
//! The old adapter-only runtime deserialized `operation.request` straight into
//! `VmDefinition`, which ALWAYS failed because the envelope wraps the
//! internally-tagged `MutationCommand` and `VmDefinition` denies unknown
//! fields. The runtime now strips the envelope through the shared
//! [`CanonicalRequest`] (`cellhv-core-operations`) and dispatches on the real
//! `MutationCommand`.
//!
//! # In-memory side-effect state
//! Successful creates record the stord/nwd handle state in an in-memory map so
//! a later delete can drain (detach+close volumes, detach NICs). Delete always
//! attempts the drain best-effort, EVEN when the hypervisor delete itself
//! failed, so a failed delete cannot strand open stord/nwd handles; the tracked
//! entry is preserved for a later retry when the delete did not succeed. The
//! map is in-memory only: a daemon restart loses it. When that happens, the
//! delete falls back to draining from a DURABLE observed-attachment source
//! ([`ObservedAttachmentSource`], #405 — production: the NodeCache
//! compatibility projection, whose VM axis the startup rebuild re-seeds from
//! the Core store before the executor starts), so a VM created before a
//! restart still gets its host taps detached and its stord volumes
//! detached/closed. When even that source has nothing (no cache record), the
//! delete logs the "no durable handle persistence" residual and still returns
//! Ok (the delete already succeeded; a leaking handle is a logged residual,
//! not an infinite-retry failure). M2.2a deliberately adds no durable handle
//! persistence; the fallback narrows that debt to the cache-empty case.

use crate::adapter::{CloudHypervisorAdapter, VmConfig, VmDiskConfig, VmNicConfig};
use cellhv_core_executor::{CoreVmRuntime, RuntimeFailure};
use cellhv_core_operations::{CanonicalRequest, MutationCommand, OperationJournalEntry};
use cellhv_core_types::{OperationKind, StorageAttachmentRef};
use chv_errors::ChvError;
use chv_hypervisor_api::resources::{
    ensure_vm_runtime_dir, nic_id, vm_api_socket, vm_config_file, vm_pid_file, vm_runtime_dir,
    HostResourceController, NetworkUsageLookup, NoObservedAttachments, ObservedAttachmentSource,
    DEFAULT_NIC_CIDR,
};
use chv_hypervisor_api::{bridge_name_for_network, AlwaysInUse};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

/// Runtime-side mirror of the `cellhv-core-types` path-safety rule.
///
/// The authority-side gate lives in `VmDefinition::validate`, `StorageAttachmentRef::validate`
/// and `NetworkAttachmentRef::validate` (`crates/cellhv-core-types`); that is what keeps a
/// traversal from ever being journaled. cellhv-core-types must stay free of a
/// `chv-hypervisor-api` dependency, so this tiny rule is duplicated here
/// DELIBERATELY: even a pre-journaled row that never passed the authority gate
/// must not become an fs-mutation primitive through this runtime.
/// Translate the Core hypervisor tuning mirror back into the legacy override
/// surface for the hypervisor-facing `VmConfig`, field for field. Field parity
/// with `cellhv_core_types::HypervisorTuning` is pinned by
/// `tuning_to_legacy_pins_field_parity_with_core_mirror` below: adding a field
/// to either side without mirroring it fails that test (the fixture is a fully
/// populated struct literal, so the compiler forces it).
fn tuning_to_legacy(
    tuning: &cellhv_core_types::HypervisorTuning,
) -> chv_common::hypervisor::HypervisorOverrides {
    chv_common::hypervisor::HypervisorOverrides {
        cpu_nested: tuning.cpu_nested,
        cpu_amx: tuning.cpu_amx,
        cpu_kvm_hyperv: tuning.cpu_kvm_hyperv,
        memory_mergeable: tuning.memory_mergeable,
        memory_hugepages: tuning.memory_hugepages,
        memory_shared: tuning.memory_shared,
        memory_prefault: tuning.memory_prefault,
        iommu: tuning.iommu,
        rng_src: tuning.rng_src.clone(),
        watchdog: tuning.watchdog,
        landlock_enable: tuning.landlock_enable,
        serial_mode: tuning.serial_mode.clone(),
        console_mode: tuning.console_mode.clone(),
        pvpanic: tuning.pvpanic,
        tpm_type: tuning.tpm_type.clone(),
        tpm_socket_path: tuning.tpm_socket_path.clone(),
    }
}

/// Shared with the process adapter (adoption scan) — see there for the
/// second call site.
pub(crate) fn is_safe_resource_id(value: &str) -> bool {
    !value.is_empty()
        && !value.contains('/')
        && !value.contains('\\')
        && !value.contains('\0')
        && value != "."
        && value != ".."
}

/// Belt-and-braces canonicalization: require the VM runtime dir to be a strict
/// descendant of `{runtime_dir}/vms`, even against symlinks/`..` in the
/// containing tree. A VM dir that is not a strict descendant is refused as an
/// invalid (path-unsafe) request; a canonicalize failure is an internal error.
fn verify_vm_dir_within_base(vm_dir: &Path, base: &Path) -> Result<(), RuntimeFailure> {
    let base_canonical = std::fs::canonicalize(base).map_err(|_| RuntimeFailure::Internal)?;
    let vm_dir_canonical = std::fs::canonicalize(vm_dir).map_err(|_| RuntimeFailure::Internal)?;
    if watched_path_is_strict_descendant(&vm_dir_canonical, &base_canonical) {
        Ok(())
    } else {
        Err(RuntimeFailure::InvalidRequest)
    }
}

/// True when `candidate` is a strict (component-wise) descendant of `base`.
fn watched_path_is_strict_descendant(candidate: &Path, base: &Path) -> bool {
    candidate.starts_with(base) && candidate != base
}

/// In-memory record of the stord/nwd handle state created for a VM.
///
/// Only lifetimes in process memory: a daemon restart loses this and delete
/// degrades to the logged crash residual (see module docs).
#[derive(Default, Clone)]
struct VmSideEffects {
    /// `(volume_id, attachment_handle, attached)` tuples, in open order. Only
    /// fully-successful creates are recorded, so every stored volume is
    /// attached; the flag is kept so the drain follows the same
    /// HostResourceController contract as the create-unwind path.
    volumes: Vec<(String, String, bool)>,
    /// `nic_id`s, in attach order.
    nics: Vec<String>,
    /// DISTINCT `network_id`s the VM's NICs attach to, in attach order
    /// (#356 N5) — the input to the last-detach teardown decision. In-memory
    /// like `nics`: after a daemon restart both are gone, so a delete simply
    /// drains nothing and no teardown fires (residue, never an outage).
    networks: Vec<String>,
}

/// The production Cloud Hypervisor effector runtime.
pub struct CloudHypervisorCoreRuntime {
    adapter: Arc<dyn CloudHypervisorAdapter>,
    resources: Arc<dyn HostResourceController>,
    runtime_dir: PathBuf,
    /// Durable last-detach safety check (#356 N5): answers "does any VM
    /// other than the deleting one still use this network" from the Core
    /// store. Defaults to fail-closed ([`AlwaysInUse`] — no teardown ever
    /// fires) until [`Self::with_network_usage`] wires the real authority.
    network_usage: Arc<dyn NetworkUsageLookup>,
    /// Per-VM in-memory handle map (see module docs for lifetime).
    side_effects: Mutex<HashMap<String, VmSideEffects>>,
    /// Durable observed-attachment fallback for the delete drain (#405):
    /// consulted ONLY when the in-memory map has no entry for the VM (any
    /// VM created before an agent restart). Defaults to
    /// [`NoObservedAttachments`] (fail-open: the logged crash residual)
    /// until [`Self::with_observed_attachments`] wires a real source.
    observed_attachments: Arc<dyn ObservedAttachmentSource>,
    /// Node-level default backend class for stord volume opens (#379):
    /// what the volume open dispatches when the disk's
    /// [`cellhv_core_types::StorageAttachmentRef`] carries no class of
    /// its own. Defaults to
    /// [`chv_hypervisor_api::resources::DEFAULT_BACKEND_CLASS`] —
    /// byte-identical to the historical inline `"local"` literal, and no
    /// production wiring sets it, so the per-volume class (PR 2's A7
    /// field, threaded through the Core definition) is the only
    /// class-changing input.
    default_backend_class: String,
    /// The LVM volume group LVM-class opens are located against (#379
    /// DP5): shapes the `/dev/mapper/{vg}-{vid}` dm-path locator the
    /// standard `device_allowlist` admits. Defaults to stord's own
    /// `chv-vg`; the agent wires the operator's configured group via
    /// [`Self::with_lvm_volume_group`] (the same stord-config source as
    /// the node's reported backend class, DP4).
    lvm_volume_group: String,
}

impl CloudHypervisorCoreRuntime {
    pub fn new(
        adapter: Arc<dyn CloudHypervisorAdapter>,
        resources: Arc<dyn HostResourceController>,
        runtime_dir: PathBuf,
    ) -> Self {
        Self {
            adapter,
            resources,
            runtime_dir,
            network_usage: Arc::new(AlwaysInUse),
            side_effects: Mutex::new(HashMap::new()),
            observed_attachments: Arc::new(NoObservedAttachments),
            default_backend_class: chv_hypervisor_api::resources::DEFAULT_BACKEND_CLASS.to_string(),
            lvm_volume_group: chv_hypervisor_api::resources::DEFAULT_LVM_VOLUME_GROUP.to_string(),
        }
    }

    /// Wire the durable network-usage authority for last-detach teardown
    /// (#356 N5). Without this, the runtime never tears network topology
    /// down (fail-closed default): host residue is then cleaned by network
    /// delete / node reprovision instead.
    pub fn with_network_usage(mut self, lookup: Arc<dyn NetworkUsageLookup>) -> Self {
        self.network_usage = lookup;
        self
    }

    /// Wire the durable observed-attachment source for the delete-time
    /// side-effect fallback drain (#405). Without this, a delete that finds
    /// no in-memory record (any VM created before an agent restart) drains
    /// nothing and logs the M2.2a crash residual (fail-open default).
    pub fn with_observed_attachments(mut self, source: Arc<dyn ObservedAttachmentSource>) -> Self {
        self.observed_attachments = source;
        self
    }

    /// Set the node-level default backend class for stord volume opens
    /// (#379). Defaults to `"local"` (the historical literal); a
    /// per-volume class carried by the Core definition (PR 2's A7 field)
    /// overrides this per attachment, with this value as the
    /// absent-field fallback.
    pub fn with_default_backend_class(mut self, backend_class: impl Into<String>) -> Self {
        self.default_backend_class = backend_class.into();
        self
    }

    /// Set the LVM volume group LVM-class opens are located against
    /// (#379 DP5). The agent passes the operator's stord-config value so
    /// the dm-path locator and the backend's own `/dev/{vg}/{vid}`
    /// derivation agree.
    pub fn with_lvm_volume_group(mut self, volume_group: impl Into<String>) -> Self {
        self.lvm_volume_group = volume_group.into();
        self
    }

    /// De-envelope the canonical request into its command. A value that is not
    /// the canonical envelope shape is a malformed request.
    fn request_command(request: &serde_json::Value) -> Result<MutationCommand, RuntimeFailure> {
        let envelope = match CanonicalRequest::try_from_value(request) {
            Ok(Some(envelope)) => envelope,
            Ok(None) | Err(_) => return Err(RuntimeFailure::InvalidRequest),
        };
        Ok(envelope.command)
    }

    /// Map an effector error to the closed public-safe [`RuntimeFailure`] set.
    ///
    /// The mapping is also the LAST place the underlying reason exists —
    /// the journal persists only the public-safe code, so a terminally
    /// failed operation would otherwise be SILENT in the agent log (the
    /// M4.4 re-qualification spent a full forensic session on exactly
    /// that: a create failed RUNTIME_UNAVAILABLE with zero log lines).
    /// Log the reason here, at warn, before dropping it.
    fn map_err(e: ChvError) -> RuntimeFailure {
        warn!(error = ?e, "vm runtime effector failed; mapping to public-safe failure code");
        match e {
            ChvError::NotFound { .. } => RuntimeFailure::NotFound,
            ChvError::AlreadyExists { .. } | ChvError::Conflict { .. } => RuntimeFailure::Conflict,
            ChvError::InvalidArgument { .. } | ChvError::BadRequest { .. } => {
                RuntimeFailure::InvalidRequest
            }
            ChvError::BackendUnavailable { .. } | ChvError::NetworkUnavailable { .. } => {
                RuntimeFailure::RuntimeUnavailable
            }
            _ => RuntimeFailure::Internal,
        }
    }

    async fn create_vm(
        &self,
        operation: &OperationJournalEntry,
        op_id: &str,
    ) -> Result<Option<serde_json::Value>, RuntimeFailure> {
        let MutationCommand::CreateVm { definition } = Self::request_command(&operation.request)?
        else {
            // A CreateVm-kind operation carrying a different command is a
            // journal-integrity failure, not a user mistake.
            return Err(RuntimeFailure::InvalidRequest);
        };
        // Layer-B path-safety guard: even a pre-journaled row that bypassed the
        // authority-side `VmDefinition::validate` must never become an
        // fs-mutation primitive (see `is_safe_resource_id`).
        if !is_safe_resource_id(definition.id.as_str()) {
            return Err(RuntimeFailure::InvalidRequest);
        }
        for storage in &definition.storage {
            if !is_safe_resource_id(storage.storage_ref.as_str()) {
                return Err(RuntimeFailure::InvalidRequest);
            }
        }
        for network in &definition.networks {
            if !is_safe_resource_id(network.network_ref.as_str()) {
                return Err(RuntimeFailure::InvalidRequest);
            }
        }
        let vm_id = definition.id.as_str();
        let vm_dir = vm_runtime_dir(&self.runtime_dir, vm_id);
        ensure_vm_runtime_dir(&vm_dir)
            .await
            .map_err(Self::map_err)?;
        // Canonicalize and confirm the VM dir is a strict descendant of
        // `{runtime_dir}/vms` (belt-and-braces on top of the id guard).
        if let Err(e) = verify_vm_dir_within_base(&vm_dir, &self.runtime_dir.join("vms")) {
            // The create never reached any attached resource, so the freshly
            // created `vm_dir` is stray: remove it best-effort (log-only so a
            // cleanup failure can never mask the verify failure).
            if let Err(cleanup_err) = tokio::fs::remove_dir_all(&vm_dir).await {
                tracing::warn!(
                    vm_id,
                    error = %cleanup_err,
                    "failed to remove stray vm dir after verify failure"
                );
            }
            return Err(e);
        }

        let mut opened_volumes: Vec<(String, String, bool)> = Vec::new();
        let mut attached_nic_ids: Vec<String> = Vec::new();
        let mut disks: Vec<VmDiskConfig> = Vec::new();
        let mut nics: Vec<VmNicConfig> = Vec::new();

        for storage in &definition.storage {
            if let Err(e) = self
                .open_and_attach_volume(
                    storage,
                    &vm_dir,
                    vm_id,
                    op_id,
                    &mut disks,
                    &mut opened_volumes,
                )
                .await
            {
                self.teardown_partial_create(
                    vm_id,
                    &vm_dir,
                    &attached_nic_ids,
                    &opened_volumes,
                    op_id,
                )
                .await;
                return Err(Self::map_err(e));
            }
        }
        for network in &definition.networks {
            if let Err(e) = self
                .ensure_and_attach_nic(network, vm_id, op_id, &mut nics, &mut attached_nic_ids)
                .await
            {
                self.teardown_partial_create(
                    vm_id,
                    &vm_dir,
                    &attached_nic_ids,
                    &opened_volumes,
                    op_id,
                )
                .await;
                return Err(Self::map_err(e));
            }
        }

        // G5 vsock device: strictly opt-in. The definition carries only
        // the opt-in marker — the node authority owns the addressing.
        // CID: durable, monotonic, node-scoped allocation (never reused;
        // a VMM respawn keeps the CID persisted in vm-config.json).
        // Socket: the device's host-side socket under the VM's runtime
        // dir — the VMM binds it (the host→guest entry; unused by the
        // monitoring transport, which is guest-initiated) and connects
        // out to "<socket>_<port>" per guest connection, which the
        // node-side relay (chv-agent) listens on. Fail closed: an
        // opted-in VM is never silently created device-less. The
        // allocator does blocking I/O (flock cursor); it runs on the
        // blocking pool because this is an async context.
        let vsock = match &definition.vsock {
            Some(_) => {
                let runtime_dir = self.runtime_dir.clone();
                let vm_dir_for_vsock = vm_dir.clone();
                Some(
                    tokio::task::spawn_blocking(move || {
                        chv_hypervisor_api::prepare_vm_vsock_device(&runtime_dir, &vm_dir_for_vsock)
                    })
                    .await
                    .map_err(|e| {
                        Self::map_err(ChvError::Internal {
                            reason: format!("vsock device preparation task failed: {e}"),
                        })
                    })?
                    .map_err(|e| {
                        Self::map_err(ChvError::Internal {
                            reason: format!("vsock device preparation failed: {e}"),
                        })
                    })?,
                )
            }
            None => None,
        };

        let config = VmConfig {
            vm_id: vm_id.to_string(),
            cpus: definition.compute.vcpus,
            memory_bytes: definition.compute.memory_bytes,
            kernel_path: PathBuf::from(&definition.boot.kernel),
            firmware_path: definition.boot.firmware.as_ref().map(PathBuf::from),
            disks,
            nics,
            api_socket_path: vm_api_socket(&vm_dir),
            cloud_init_userdata: definition.cloud_init_userdata.clone(),
            hypervisor_overrides: definition.hypervisor_tuning.as_ref().map(tuning_to_legacy),
            vsock,
        };

        if let Err(e) = self.adapter.create_vm(&config, Some(op_id)).await {
            self.teardown_partial_create(vm_id, &vm_dir, &attached_nic_ids, &opened_volumes, op_id)
                .await;
            return Err(Self::map_err(e));
        }

        // Record the in-memory handle map so a later delete can drain it. A
        // poisoned mutex here is an internal bug; fail the op rather than drop
        // the create's side-effect bookkeeping silently.
        let Ok(mut map) = self.side_effects.lock() else {
            warn!(
                vm_id,
                operation_id = op_id,
                "side-effects mutex poisoned; refusing to record handles"
            );
            return Err(RuntimeFailure::Internal);
        };
        // Key by the operation's authoritative `vm_id` (equal to definition.id
        // by authority construction, but keeping the map keyed on the operation
        // makes the runtime robust to an inconsistent journal).
        map.insert(
            operation.operation.vm_id.as_str().to_string(),
            VmSideEffects {
                volumes: opened_volumes,
                nics: attached_nic_ids,
                // Distinct network ids in attach order — the last-detach
                // teardown input (#356 N5).
                networks: {
                    let mut networks: Vec<String> = Vec::new();
                    for network in &definition.networks {
                        if !networks.contains(&network.network_ref) {
                            networks.push(network.network_ref.clone());
                        }
                    }
                    networks
                },
            },
        );
        Ok(None)
    }

    /// Open+attach one storage attachment and record its disk config + handle.
    async fn open_and_attach_volume(
        &self,
        storage: &StorageAttachmentRef,
        vm_dir: &Path,
        vm_id: &str,
        op_id: &str,
        disks: &mut Vec<VmDiskConfig>,
        opened: &mut Vec<(String, String, bool)>,
    ) -> Result<(), ChvError> {
        let volume_id = storage.storage_ref.as_str();
        // #379 PR 2 (A3): the per-attachment class from the Core
        // definition (A7), falling back to the runtime's node-level
        // default — absent stays absent here, so a definition without
        // the field (every pre-#379 journal entry) opens exactly as
        // before.
        let backend_class = storage
            .backend_class
            .as_deref()
            .unwrap_or(&self.default_backend_class);
        // #379 DP5: class-dependent locator shaping — an LVM-class open
        // carries a /dev/mapper/{vg}-{vid} dm-path token the standard
        // device_allowlist admits (the LVM backend ignores the locator
        // and derives /dev/{vg}/{vid} itself); every other class keeps
        // the historical {volume_id}.img under the VM dir. Same
        // convention as the legacy open sites (reconcile.rs A1/A2,
        // agent_server.rs A4/A10).
        let locator = if backend_class == "lvm" {
            chv_hypervisor_api::resources::lvm_locator(&self.lvm_volume_group, volume_id)
        } else {
            vm_dir
                .join(format!("{volume_id}.img"))
                .to_string_lossy()
                .into_owned()
        };
        // Provisioning hints from the Core definition (mirroring the legacy
        // reconcile path): size and seed apply on first open; stord ignores
        // them for an already-provisioned volume.
        let mut open_options = HashMap::new();
        if let Some(size_bytes) = storage.size_bytes {
            open_options.insert("size_bytes".to_string(), size_bytes.to_string());
        }
        if let Some(seed_from) = storage
            .seed_from
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            open_options.insert("seed_from".to_string(), seed_from.to_string());
        }
        let (_volume_id, handle, _export_path) = self
            .resources
            .open_volume(
                volume_id,
                backend_class,
                &locator,
                open_options,
                Some(op_id),
            )
            .await?;
        // Track the handle as soon as it exists so any later failure still
        // closes it (no leaked handle even when this attach fails). The
        // attach flag starts false (NOT yet attached), so teardown only closes
        // if the attach below never succeeds.
        opened.push((volume_id.to_string(), handle.clone(), false));
        let (_export_kind, export_path) = self
            .resources
            .attach_volume_to_vm(volume_id, vm_id, &handle, Some(op_id))
            .await?;
        // A successful attach marks this volume attached (its entry is the
        // last one pushed).
        if let Some(entry) = opened.last_mut() {
            entry.2 = true;
        }
        disks.push(VmDiskConfig {
            path: PathBuf::from(export_path),
            read_only: storage.read_only,
            id: Some(storage.attachment_id.clone()),
        });
        Ok(())
    }

    /// Ensure topology + attach one NIC and record its config + nic_id.
    async fn ensure_and_attach_nic(
        &self,
        network: &cellhv_core_types::NetworkAttachmentRef,
        vm_id: &str,
        op_id: &str,
        nics: &mut Vec<VmNicConfig>,
        attached: &mut Vec<String>,
    ) -> Result<(), ChvError> {
        let network_id = network.network_ref.as_str();
        let nid = nic_id(vm_id, network_id);
        let bridge = bridge_name_for_network(network_id);
        // Addressing assigned by the control plane (internal IPAM) is carried
        // on the attachment; absent fields fall back to the topology default.
        // An empty CIDR (a network row without one — the BFF's
        // build_agent_vm_spec emits `unwrap_or_default()`) defaults exactly
        // like the legacy reconcile path, so the two authorities cannot
        // diverge on the same data.
        let (ip_address, cidr, gateway) = match &network.addressing {
            Some(addressing) => (
                addressing.ip_address.clone(),
                if addressing.cidr.is_empty() {
                    DEFAULT_NIC_CIDR.to_string()
                } else {
                    addressing.cidr.clone()
                },
                addressing.gateway.clone(),
            ),
            None => (String::new(), DEFAULT_NIC_CIDR.to_string(), String::new()),
        };
        self.resources
            .ensure_network_topology(network_id, &bridge, &cidr, &gateway, Some(op_id))
            .await?;
        // Attach-time policy application (#355): every network gets a
        // filtered boundary BEFORE the NIC attaches so no guest traffic
        // can flow outside the CHV boundary. DP4 (ruled 2026-10-08):
        // a non-empty snapshot applies the operator's rules on top of
        // default-deny; no snapshot, a blank, or an empty ruleset
        // (`[]` = no user rules) applies the shared BASELINE (DHCP
        // UDP 67/68, DNS UDP/TCP 53; established/related rides the
        // engine's conntrack head rule) — replacing the pre-DP4 skip
        // (rule-less networks were left unfiltered) and the #360
        // cutoff (an empty ruleset was never applied because bare
        // default-deny with zero allows cut a network's guests off
        // entirely). The version is content-derived so retries are
        // idempotent; nwd records it for later policy re-scoping only.
        // NOTE: a create that fails AFTER this point leaves the applied
        // policy in place — deliberately, exactly like the ensured
        // topology: both are network-scoped (not VM-scoped) state, the
        // create-unwind path only reverses VM-scoped effects, and nwd
        // removes the policy at network teardown.
        // NOTE (pre-existing, unchanged): the HostResourceController
        // seam calls the UNCHECKED nwd variant, so a semantic refusal
        // here is swallowed and the attach proceeds — the follow-up
        // flagged in #355 PR 1; the DP4 baseline does not widen it
        // (a refused baseline leaves the network exactly as
        // unfiltered as the pre-DP4 skip did).
        let baseline = chv_common::firewall::baseline_policy_json();
        let policy_json: &[u8] = match network.firewall_policy_json.as_deref() {
            Some(p) if !chv_common::firewall_ruleset_is_empty(p) => p.as_bytes(),
            _ => baseline.as_bytes(),
        };
        let policy_version = format!(
            "attach-{:016x}",
            chv_common::fnv1a_hash(&String::from_utf8_lossy(policy_json))
        );
        self.resources
            .set_firewall_policy(network_id, &policy_version, policy_json, Some(op_id))
            .await?;
        let mac_address = network.mac_address.clone().unwrap_or_default();
        let (_namespace_handle, tap_handle) = self
            .resources
            .attach_vm_nic(
                &nid,
                vm_id,
                network_id,
                &mac_address,
                &ip_address,
                Some(op_id),
            )
            .await?;
        nics.push(VmNicConfig {
            network_id: network_id.to_string(),
            mac_address,
            ip_address,
            tap_name: tap_handle,
            cidr,
            gateway,
        });
        attached.push(nid);
        Ok(())
    }

    /// Best-effort unwind of a partially-created VM: detach attached NICs in
    /// reverse, close opened volumes in reverse (attach+close only those that
    /// were actually attached, per the HostResourceController contract that a
    /// never-attached volume is closed without a detach), then remove the VM
    /// directory. Every step logs and continues; errors never escalate the
    /// original create failure.
    async fn teardown_partial_create(
        &self,
        vm_id: &str,
        vm_dir: &Path,
        nics: &[String],
        volumes: &[(String, String, bool)],
        op_id: &str,
    ) {
        for nic_id in nics.iter().rev() {
            if let Err(e) = self
                .resources
                .detach_vm_nic(nic_id, vm_id, "", Some(op_id))
                .await
            {
                warn!(vm_id, nic_id, error = %e, "create unwind: detach_vm_nic failed, continuing");
            }
        }
        for (volume_id, handle, attached) in volumes.iter().rev() {
            if *attached {
                if let Err(e) = self
                    .resources
                    .detach_volume_from_vm(volume_id, vm_id, false, Some(op_id))
                    .await
                {
                    warn!(vm_id, volume_id, error = %e, "create unwind: detach_volume_from_vm failed, continuing");
                }
            }
            if let Err(e) = self
                .resources
                .close_volume(volume_id, handle, Some(op_id))
                .await
            {
                warn!(vm_id, volume_id, error = %e, "create unwind: close_volume failed, continuing");
            }
        }
        if let Err(e) = tokio::fs::remove_dir_all(vm_dir).await {
            warn!(vm_id, path = %vm_dir.display(), error = %e, "create unwind: failed to remove vm dir (best-effort)");
        }
    }

    /// Best-effort drain of a VM's tracked side effects, on any delete path.
    ///
    /// The drain always attempts cleanup once a delete is in flight so a failed
    /// hypervisor delete cannot strand open stord/nwd resources. When
    /// `keep_entry` is true (the delete did NOT succeed), the tracked entry is
    /// preserved so a later retry can finish the drain; when false (delete
    /// succeeded) the entry is removed. Every cleanup step is best-effort and
    /// logs-and-continues; no drain failure ever escalates the delete outcome.
    ///
    /// When there is no tracked entry (a VM created before an agent restart),
    /// the drain falls back to the durable observed-attachment source (#405,
    /// see the `None` arm below) before giving up with the logged crash
    /// residual.
    async fn drain_side_effects(&self, vm_id: &str, op_id: &str, keep_entry: bool) {
        let effects = match self.side_effects.lock() {
            Ok(mut map) => {
                let entry = map.remove(vm_id);
                if keep_entry {
                    if let Some(entry) = entry.as_ref() {
                        map.insert(vm_id.to_string(), entry.clone());
                    }
                }
                entry
            }
            Err(_poisoned) => {
                warn!(
                    vm_id,
                    operation_id = op_id,
                    "side-effects mutex poisoned during delete; handle state lost"
                );
                None
            }
        };
        let Some(effects) = effects else {
            // #405 fallback: after a daemon restart the in-memory handle
            // map is gone, but the delete's side effects (host taps,
            // stord sessions) are still live on the node. Drain them from
            // the DURABLE observed-attachment source (production: the
            // NodeCache projection, re-seeded from the Core store at
            // startup) — mirroring the CH adapter's own `delete_untracked_vm`
            // precedent for the no-entry delete case. This fallback runs
            // on BOTH delete outcomes (the caller already decided
            // `keep_entry`); there is no entry to keep, and every step is
            // the same best-effort log-and-continue as the tracked drain,
            // so a missing tap (nwd detach is Ok on "cannot find device")
            // or an unknown volume cannot fail the delete.
            let observed = self.observed_attachments.observed_attachments(vm_id).await;
            if observed.is_empty() {
                // Documented crash residual: neither the in-memory map nor
                // the durable observed state knows this VM's handles, so
                // stord/nwd handles may leak (M2.2a adds no durable handle
                // persistence; the fallback above narrows but does not
                // eliminate that debt).
                warn!(
                    vm_id,
                    operation_id = op_id,
                    "no in-memory side-effect state available for delete; leaking stord/nwd handles is a documented crash residual (no durable handle persistence in M2.2a)"
                );
                return;
            }
            info!(
                vm_id,
                operation_id = op_id,
                volumes = observed.volumes.len(),
                nics = observed.nics.len(),
                "no in-memory side-effect state for delete; draining stord/nwd handles from durable observed attachments (post-restart fallback)"
            );
            for (volume_id, handle) in &observed.volumes {
                // #711: the cached handle died with the agent process on
                // restart, but stord's durable session table still knows
                // the live attachment. Resolve it BEFORE the detach clears
                // the session's `vm_id`, so this fallback detach-then-close
                // exactly like the tracked path. A lookup failure follows
                // the drain's fail-open policy below (log and continue);
                // the detach still releases the VM-scoped attachment.
                let handle = match handle {
                    Some(handle) => Some(handle.clone()),
                    None => match self.resources.resolve_volume_handle(volume_id, vm_id).await {
                        Ok(handle) => handle,
                        Err(e) => {
                            warn!(vm_id, volume_id, error = %e, "delete cleanup: resolve_volume_handle failed, continuing");
                            None
                        }
                    },
                };
                if let Err(e) = self
                    .resources
                    .detach_volume_from_vm(volume_id, vm_id, false, Some(op_id))
                    .await
                {
                    warn!(vm_id, volume_id, error = %e, "delete cleanup: detach_volume failed, continuing");
                }
                // No handle (not cached, and stord reported no live
                // session) means there is nothing to close.
                if let Some(handle) = handle {
                    if let Err(e) = self
                        .resources
                        .close_volume(volume_id, &handle, Some(op_id))
                        .await
                    {
                        warn!(vm_id, volume_id, error = %e, "delete cleanup: close_volume failed, continuing");
                    }
                }
            }
            for nic_id in &observed.nics {
                if let Err(e) = self
                    .resources
                    .detach_vm_nic(nic_id, vm_id, "", Some(op_id))
                    .await
                {
                    warn!(vm_id, nic_id, error = %e, "delete cleanup: detach_nic failed, continuing");
                }
            }
            // Deliberately NO last-detach network teardown on the fallback
            // path: the tracked path's teardown input (`effects.networks`)
            // comes from the in-memory record, and promoting network
            // teardown onto the restart path is a separate decision with
            // its own authority guards (#356 N5). Host topology residue
            // from a post-restart delete is cleaned by network delete /
            // node reprovision, exactly as before this fallback existed.
            return;
        };
        for (volume_id, handle, attached) in effects.volumes.iter().rev() {
            if *attached {
                if let Err(e) = self
                    .resources
                    .detach_volume_from_vm(volume_id, vm_id, false, Some(op_id))
                    .await
                {
                    warn!(vm_id, volume_id, error = %e, "delete cleanup: detach_volume failed, continuing");
                }
            }
            if let Err(e) = self
                .resources
                .close_volume(volume_id, handle, Some(op_id))
                .await
            {
                warn!(vm_id, volume_id, error = %e, "delete cleanup: close_volume failed, continuing");
            }
        }
        for nic_id in effects.nics.iter().rev() {
            if let Err(e) = self
                .resources
                .detach_vm_nic(nic_id, vm_id, "", Some(op_id))
                .await
            {
                warn!(vm_id, nic_id, error = %e, "delete cleanup: detach_nic failed, continuing");
            }
        }
        // #356 N5 — last-detach host teardown. Only after a SUCCESSFUL
        // delete (`keep_entry` is false): the VM's NICs are detached and
        // the authority tombstoned it, so this node's claim on the
        // network's topology (bridge / namespace / dnsmasq / nft table)
        // may be over. The in-use check comes from the DURABLE authority
        // (`network_usage`), never from this in-memory map: after a
        // daemon restart the map is empty and "no entry references the
        // network" would falsely authorize cutting a still-running
        // pre-restart VM off its bridge. A failed check skips the
        // teardown (residue, never an outage); a failed teardown is
        // best-effort with a loud warning for the same reason.
        if !keep_entry {
            for network_id in &effects.networks {
                // Durable authority first: does any live VM (other than the
                // deleting one) still reference the network?
                let in_use = {
                    // The lookup does disk I/O + JSON parsing (the Core
                    // store's VM list); keep it OFF the async worker.
                    let lookup = self.network_usage.clone();
                    let network = network_id.clone();
                    let vm = vm_id.to_string();
                    match tokio::task::spawn_blocking(move || lookup.network_in_use(&network, &vm))
                        .await
                    {
                        Ok(in_use) => in_use,
                        Err(join_error) => {
                            // The lookup panicked (join failure): fail closed.
                            warn!(
                                vm_id,
                                network_id,
                                error = %join_error,
                                "network-usage lookup task failed; treating the network as in use"
                            );
                            continue;
                        }
                    }
                };
                if in_use {
                    continue;
                }
                // In-session veto (#356 review): the store tombstones a VM
                // when its delete is ACCEPTED, so a sibling whose delete
                // later FAILED (retry pending, VMM still running, taps
                // still enslaved) is invisible to the lookup — and nwd
                // deletes the bridge unconditionally. Failed deletes
                // retain their side-effects entry for the retry, so any
                // OTHER retained entry referencing the network vetoes the
                // teardown. In-memory state only ever VETOES here, never
                // authorizes: after a daemon restart the map is empty and
                // this guard degrades to the durable store's answer.
                if self.session_retains_network_user(network_id, vm_id) {
                    continue;
                }
                if let Err(e) = self
                    .resources
                    .delete_network_topology(network_id, Some(op_id))
                    .await
                {
                    warn!(
                        vm_id,
                        network_id,
                        error = %e,
                        "last-detach network teardown failed; host residue (bridge/dnsmasq/nft) remains until network delete or node reprovision"
                    );
                }
            }
        }
    }

    /// In-session veto for last-detach teardown: true when any OTHER VM's
    /// retained side-effects entry still references `network_id`.
    ///
    /// A failed delete keeps its entry (the retry needs the handles) while
    /// the durable authority has already tombstoned the VM at accept — this
    /// map is the only in-process signal that such a VM's host effects (VMM,
    /// enslaved taps) are still live. Veto-only by design: an unreadable map
    /// also vetoes (fail closed), and the map is NEVER used to authorize a
    /// teardown — after a daemon restart it is empty and the decision
    /// degrades to the durable store's answer. Residual, documented: a VM
    /// tombstoned pre-restart whose delete then failed and whose VMM was
    /// adopted after the restart is invisible to both authorities; its
    /// network can be torn down by a sibling's successful delete.
    fn session_retains_network_user(&self, network_id: &str, excluding_vm: &str) -> bool {
        let Ok(map) = self.side_effects.lock() else {
            warn!(
                network_id,
                "side-effects mutex poisoned during teardown veto check; vetoing teardown"
            );
            return true;
        };
        map.iter().any(|(vm, effects)| {
            vm.as_str() != excluding_vm && effects.networks.iter().any(|n| n == network_id)
        })
    }

    async fn delete_vm(
        &self,
        operation: &OperationJournalEntry,
        op_id: &str,
    ) -> Result<Option<serde_json::Value>, RuntimeFailure> {
        // Key the drain by the operation's authoritative `vm_id` (the same key
        // the CreateVm arm inserts under).
        let vm_id = operation.operation.vm_id.as_str();
        let mut delete_result = self.adapter.delete_vm(vm_id, Some(op_id)).await;
        // Idempotent delete: the force-stop path removes the adapter's map
        // entry by design (pre-existing force-stop semantics), so a delete
        // issued after a force stop finds no runtime entry and the adapter
        // reports NotFound — although the VM exists at the authority and its
        // runtime dir is still on disk (run 8d of the M2.5 qualification: the
        // S2 retry stop's graceful window expired, the force fallback dropped
        // the entry, and the subsequent delete failed NOT_FOUND on every
        // retry). When the runtime dir shows the VM once ran on this node,
        // the delete's runtime goal — no live VMM, no adapter-owned
        // artifacts — is completable from the shared layout, so finish the
        // cleanup the adapter would have done and succeed. A VM with neither
        // an entry nor a runtime dir never ran on this node: NotFound stays
        // NotFound (a misrouted delete must surface, not silently no-op).
        if let Err(ChvError::NotFound { .. }) = &delete_result {
            if vm_runtime_dir(&self.runtime_dir, vm_id).is_dir() {
                // Adapter-agnostic FALLBACK only: the production process
                // adapter handles the no-entry case itself, inside its
                // per-VM lifecycle lock (reaping any live untracked
                // owner — "no entry" is not proof of "no VMM" — before
                // removing artifacts) and returns Ok for it; this arm
                // therefore fires for it only when the dir holds no
                // persisted payload (e.g. a prior partial delete). Mock
                // and future adapters that return NotFound for the
                // residual get the idempotent cleanup here. When the
                // runtime dir shows the VM once ran on this node, the
                // delete's runtime goal — no adapter-owned artifacts —
                // is completable from the shared layout. A VM with
                // neither an entry nor a runtime dir never ran on this
                // node: NotFound stays NotFound (a misrouted delete
                // must surface, not silently no-op).
                self.remove_orphaned_vm_artifacts(vm_id, op_id).await;
                delete_result = Ok(());
            }
        }
        if delete_result.is_err() {
            tracing::warn!(
                vm_id,
                operation_id = op_id,
                "hypervisor delete failed; draining tracked side effects best-effort"
            );
        }
        // Best-effort drain REGARDLESS of the delete result so a failed delete
        // cannot strand open stord/nwd resources. Keep the tracked entry when
        // the delete did NOT succeed so a later retry can finish the drain.
        self.drain_side_effects(vm_id, op_id, delete_result.is_err())
            .await;
        delete_result.map_err(Self::map_err)?;
        Ok(None)
    }

    /// Best-effort removal of the adapter-owned artifacts (the api socket,
    /// the pid file, the persisted creation payload) for a VM whose runtime
    /// entry is already gone — the same set the adapter's delete removes,
    /// derived from the shared layout helpers so the two cannot drift. Disk
    /// images and the VM directory itself belong to the storage/authority
    /// layers and are deliberately left alone.
    async fn remove_orphaned_vm_artifacts(&self, vm_id: &str, op_id: &str) {
        let vm_dir = vm_runtime_dir(&self.runtime_dir, vm_id);
        info!(
            vm_id,
            operation_id = op_id,
            "no runtime entry for delete (force-stopped or prior delete); cleaning adapter-owned artifacts from the runtime dir"
        );
        for artifact in [
            vm_api_socket(&vm_dir),
            vm_pid_file(&vm_dir),
            vm_config_file(&vm_dir),
        ] {
            match tokio::fs::remove_file(&artifact).await {
                Ok(()) => info!(
                    vm_id,
                    operation_id = op_id,
                    path = %artifact.display(),
                    "removed orphaned vm artifact"
                ),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => warn!(
                    vm_id,
                    operation_id = op_id,
                    path = %artifact.display(),
                    error = %e,
                    "orphaned vm artifact removal failed (continuing)"
                ),
            }
        }
    }
}

/// Test-only visibility into the tracked side-effects map (doc-hidden so
/// integration tests in the external `tests/` crate can assert no entry is
/// stranded after a successful delete). NOT `cfg(test)`: it must compile into
/// normal builds so the cross-crate test can call it.
#[doc(hidden)]
impl CloudHypervisorCoreRuntime {
    pub fn debug_side_effects_len(&self) -> usize {
        // A poisoned lock is unreachable in production (no panic is taken
        // while holding the guard); 0 is only the deterministic fallback this
        // doc-hidden test observer exposes to integration tests.
        match self.side_effects.lock() {
            Ok(guard) => guard.len(),
            Err(_) => 0,
        }
    }
}

#[async_trait::async_trait]
impl CoreVmRuntime for CloudHypervisorCoreRuntime {
    async fn execute(
        &self,
        operation: OperationJournalEntry,
    ) -> std::result::Result<Option<serde_json::Value>, RuntimeFailure> {
        let op_id = operation.operation.id.as_str();
        match operation.operation.kind {
            OperationKind::CreateVm => self.create_vm(&operation, op_id).await,
            OperationKind::DeleteVm => self.delete_vm(&operation, op_id).await,
            OperationKind::StartVm => {
                self.adapter
                    .start_vm(operation.operation.vm_id.as_str(), Some(op_id))
                    .await
                    .map_err(Self::map_err)?;
                Ok(None)
            }
            OperationKind::StopVm => {
                // Stop without force (matches the legacy lifecycle contract).
                self.adapter
                    .stop_vm(operation.operation.vm_id.as_str(), false, Some(op_id))
                    .await
                    .map_err(Self::map_err)?;
                Ok(None)
            }
            OperationKind::RebootVm => {
                self.adapter
                    .reboot_vm(operation.operation.vm_id.as_str(), Some(op_id))
                    .await
                    .map_err(Self::map_err)?;
                Ok(None)
            }
            // Out-of-RC-lifecycle in M2.2a: these mutate a running VM's
            // attachment set, which needs the M2.2b/M3 projection work and
            // durable handle bookkeeping. Fail closed and honest (Unsupported)
            // rather than falsely reporting Succeeded — and never the old
            // broken InvalidRequest path.
            OperationKind::UpdateVm
            | OperationKind::AttachVolume
            | OperationKind::DetachVolume
            | OperationKind::AttachNetwork
            | OperationKind::DetachNetwork => Err(RuntimeFailure::Unsupported),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tuning_to_legacy_pins_field_parity_with_core_mirror() {
        // Fully populated on BOTH sides so the compiler forces this fixture
        // to grow whenever either struct gains a field. The total check is
        // the JSON-equality assertion below: both structs share field names
        // and 1:1 types, so a faithful translation reproduces the input
        // object exactly — any dropped, added, or swapped field breaks it.
        let tuning = cellhv_core_types::HypervisorTuning {
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
        let overrides = tuning_to_legacy(&tuning);
        assert_eq!(
            serde_json::to_value(&overrides).unwrap(),
            serde_json::to_value(&tuning).unwrap(),
            "tuning_to_legacy must reproduce the Core object field for field"
        );
        // And the empty mirror stays empty.
        let empty = tuning_to_legacy(&cellhv_core_types::HypervisorTuning::default());
        assert_eq!(
            empty,
            chv_common::hypervisor::HypervisorOverrides::default()
        );
    }

    #[test]
    fn is_safe_resource_id_rejects_path_components_and_dots() {
        for safe in ["vm-1", "a.b", "default", "net-0"] {
            assert!(is_safe_resource_id(safe), "{safe} must be safe");
        }
        for unsafe_value in [
            "a/b",
            "a\\b",
            "/etc/passwd",
            "a/../b",
            ".",
            "..",
            "a\0b",
            "",
        ] {
            assert!(
                !is_safe_resource_id(unsafe_value),
                "{unsafe_value:?} must be rejected"
            );
        }
    }

    #[test]
    fn verify_vm_dir_within_base_accepts_descendants_and_rejects_escapes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().join("vms");
        std::fs::create_dir_all(&base).expect("create base");

        // A normal strict descendant is accepted.
        let vm_dir = base.join("vm-1");
        std::fs::create_dir_all(&vm_dir).expect("create vm dir");
        assert!(verify_vm_dir_within_base(&vm_dir, &base).is_ok());

        // The base itself is NOT a strict descendant.
        assert!(matches!(
            verify_vm_dir_within_base(&base, &base),
            Err(RuntimeFailure::InvalidRequest)
        ));

        // A sibling outside the base escapes.
        let sibling = tmp.path().join("other");
        std::fs::create_dir_all(&sibling).expect("create sibling");
        assert!(matches!(
            verify_vm_dir_within_base(&sibling, &base),
            Err(RuntimeFailure::InvalidRequest)
        ));

        // Missing VM dir is an internal error (creation already happened, so a
        // missing canonical target is a real internal inconsistency).
        assert!(matches!(
            verify_vm_dir_within_base(&base.join("ghost"), &base),
            Err(RuntimeFailure::Internal)
        ));
    }
}
