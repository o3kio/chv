//! Shared VM resource conventions and the neutral host-resource controller trait.
//!
//! This module is the low shared crate boundary for the Core-driven
//! single-effector runtime (M2.2a). It carries:
//!
//! - pure path/string conventions (VM runtime dir, API socket, bridge name,
//!   NIC id, default NIC CIDR) that both the legacy reconcile path and the Core
//!   runtime share without duplicates;
//! - [`ensure_vm_runtime_dir`] which creates the per-VM runtime directory with
//!   the canonical 0o775 mode and a tolerant `chv-stord` group chown;
//! - [`HostResourceController`], the trait through which `CloudHypervisorCoreRuntime`
//!   performs all stord/nwd side effects without depending on `chv-agent-core`.

use async_trait::async_trait;
use chv_errors::ChvError;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The canonical default stord backend class for volume opens (#379).
///
/// Every agent-side class-writing open site (the legacy reconcile create
/// and re-attach loops, the Core executor, the legacy `CreateVm` RPC
/// branch, and the attach handler's spec_json parser) resolves the class
/// from the disk spec or runtime configuration; an absent value means
/// this default — byte-identical to the historical hardcoded literal, so
/// no producer setting the field means zero behavior change. It lives
/// here because it is the default vocabulary of
/// [`HostResourceController::open_volume`]'s `backend_class` parameter:
/// the one seam both `chv-agent-core` and `chv-agent-runtime-ch`
/// reference without depending on each other.
pub const DEFAULT_BACKEND_CLASS: &str = "local";

/// The stord `backend_type` vocabulary the volume-model class field
/// accepts (#379 DP3): `local` (the canonical default,
/// [`DEFAULT_BACKEND_CLASS`]), `iscsi`, `ceph`, and `lvm`.
///
/// This is the single frozen list every accept-time validation of a
/// storage class references (the BFF's VM-create payload in PR 2; the
/// node capability check in PR 3) — the local aliases
/// (`local-file`/`localdisk`) stay accepted at the stord boundary only,
/// and `block` remains an `lvm` alias for the device-allowlist branch,
/// so neither appears here. DP6/#372 will pin chvctl and the BFF to
/// this same list so the surfaces never diverge again.
pub const BACKEND_CLASSES: &[&str] = &["local", "iscsi", "ceph", "lvm"];

/// Whether `class` is one of the stord `backend_type` values (#379
/// DP3) — the accept-time vocabulary check for a volume-model storage
/// class. Unknown strings are rejected by callers (HTTP 400 at the
/// BFF); an absent value never reaches this check and means
/// [`DEFAULT_BACKEND_CLASS`].
pub fn is_known_backend_class(class: &str) -> bool {
    BACKEND_CLASSES.contains(&class)
}

/// The volume-capacity ceiling every create surface enforces (#513
/// PR 3 consolidation): 64 TiB, in BYTES. This is the single shared
/// definition — the same constant-of-record discipline as
/// [`BACKEND_CLASSES`] above it — stated in bytes because the
/// standalone volume-create contract is bytes-denominated
/// (`capacity_bytes`, `POST /v1/volumes/create`); the GiB-denominated
/// surfaces (`volume_size_gb` in the BFF's VM-create and
/// template-create handlers, chvctl's `--disk-size-gb` mirror) derive
/// their `MAX_VOLUME_SIZE_GB` from this value. The value is the
/// historical literal on every site (64 TiB everywhere) — the
/// consolidation de-duplicated the definition, not the bound.
pub const MAX_VOLUME_BYTES: i64 = 64 * 1024 * 1024 * 1024 * 1024;

/// The default LVM volume group name (#379 DP5): mirrors `chv-stord`'s
/// own `lvm_volume_group` default (`cmd/chv-stord/src/main.rs` and
/// `chv-config`'s `StordConfig`) so the agent's LVM locator shaping and
/// the daemon's backend derivation agree when no operator key is set.
pub const DEFAULT_LVM_VOLUME_GROUP: &str = "chv-vg";

/// The standard install's `device_allowlist` (#379 DP5): the patterns
/// `scripts/install.sh` writes into `stord.toml`. The LVM locator
/// convention ([`lvm_locator`]) is shaped so this posture admits it.
pub const STANDARD_DEVICE_ALLOWLIST: &[&str] = &["/dev/dm-*", "/dev/mapper/*"];

/// The #379 DP5 LVM locator convention: a
/// `/dev/mapper/{vg}-{vid}`-shaped dm-path token.
///
/// The LVM backend *ignores* the locator (it derives the device from the
/// sanitized volume id as `/dev/{vg}/{vid}`); the locator's only role is
/// the `device_allowlist` check at the stord open boundary, which gates
/// the raw locator string for `lvm`-class opens. The standard install's
/// [`STANDARD_DEVICE_ALLOWLIST`] therefore admits this shape while it
/// would deny the bare volume id. `is_safe_id` on the volume id (the
/// agent boundary, re-sanitized in the backend) keeps the LV-name
/// component safe; the VG comes from the same operator config source as
/// the node's reported backend class (DP4).
pub fn lvm_locator(volume_group: &str, volume_id: &str) -> String {
    format!("/dev/mapper/{volume_group}-{volume_id}")
}

/// Normalize a storage-class token for comparison (#379 DP4, per DP3's
/// vocabulary): the local aliases (`local-file`/`localdisk`, which
/// pre-#379 inventory probes and the stord boundary emit) fold to the
/// canonical `local`; `NULL`/empty (the volume-model default) is
/// `local` on the request side. Everything else compares verbatim.
pub fn normalize_storage_class(class: &str) -> &str {
    let class = class.trim();
    match class {
        "" | "local" | "local-file" | "localdisk" => DEFAULT_BACKEND_CLASS,
        other => other,
    }
}

/// The #379 DP4 accept-time capability predicate: does a node
/// advertising `advertised` storage classes serve a volume requesting
/// `requested` (`None` = NULL = local)?
///
/// Fail-open discipline (the #495 `ensure_*` shape): an EMPTY advertised
/// list cannot support a rejection — a node whose inventory never
/// landed, or a pre-#379 agent reporting an empty probe, keeps accepting
/// exactly as before — so that case returns `None` and callers accept.
/// `Some(false)` is the definite mismatch (reject with 400); `Some(true)`
/// is the definite match. Advertised tokens are normalized with
/// [`normalize_storage_class`] so legacy `localdisk` reports compare as
/// `local`.
pub fn node_offers_storage_class(advertised: &[String], requested: Option<&str>) -> Option<bool> {
    if advertised.is_empty() {
        return None;
    }
    let requested = normalize_storage_class(requested.unwrap_or(DEFAULT_BACKEND_CLASS));
    Some(
        advertised
            .iter()
            .any(|class| normalize_storage_class(class) == requested),
    )
}

/// Returns the per-VM runtime directory for the given VM.
/// This directory holds the VM's socket, logs, PID file, and other runtime artifacts.
///
/// Relocated verbatim from `crates/chv-agent-core/src/reconcile.rs` (M2.2a).
pub fn vm_runtime_dir(base: &Path, vm_id: &str) -> PathBuf {
    base.join("vms").join(vm_id)
}

/// The Cloud Hypervisor API socket for a VM runtime directory.
///
/// Unified convention across the legacy reconcile path and the Core runtime:
/// `{runtime_dir}/vms/{vm_id}/vm.sock`.
pub fn vm_api_socket(vm_dir: &Path) -> PathBuf {
    vm_dir.join("vm.sock")
}

/// The VMM pid file for a VM runtime directory (`ch.pid`).
///
/// Written by the process adapter at spawn/adoption time and removed by the
/// delete path. Shared so the adapter's own cleanup and the Core runtime's
/// orphaned-artifact cleanup (delete after a force stop) cannot drift.
pub fn vm_pid_file(vm_dir: &Path) -> PathBuf {
    vm_dir.join("ch.pid")
}

/// The persisted creation payload for a VM runtime directory
/// (`vm-config.json`).
///
/// Written by the process adapter at create time; the re-spawn and adoption
/// paths rebuild from it. Removed by the delete path (same shared-layout
/// rationale as [`vm_pid_file`]).
pub fn vm_config_file(vm_dir: &Path) -> PathBuf {
    vm_dir.join("vm-config.json")
}

/// The guest serial-console capture log for a VM runtime directory.
pub fn vm_console_log(vm_dir: &Path) -> PathBuf {
    vm_dir.join("console.log")
}

/// The previous generation of the console capture, kept by
/// [`rotate_console_log`].
pub fn vm_console_log_last(vm_dir: &Path) -> PathBuf {
    vm_dir.join("console.log.last")
}

/// Rotates the console evidence one generation back
/// (`console.log` → `console.log.last`, atomically overwriting any
/// previous generation) instead of destroying it.
///
/// The force-stop and recovery paths used to delete the log outright —
/// destroying the only record of what the guest was doing when it was
/// killed (the M2.5 freeze investigation depended on exactly this
/// evidence). One generation is kept: more would grow unbounded in the
/// runtime dir, less is what is being fixed here. A missing log is a
/// no-op (nothing to keep); other failures are returned for the caller
/// to warn about — evidence retention must never fail the stop itself.
pub async fn rotate_console_log(vm_dir: &Path) -> std::io::Result<()> {
    match tokio::fs::rename(vm_console_log(vm_dir), vm_console_log_last(vm_dir)).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Legacy-compatible NIC attachment identifier: `{vm_id}-{network_id}`.
///
/// This exactly matches `cellhv_nodecache_migration::legacy_network_attachment_id`
/// so Core attachment IDs align with the NodeCache projection (M2.2b).
pub fn nic_id(vm_id: &str, network_id: &str) -> String {
    format!("{vm_id}-{network_id}")
}

/// Default NIC subnet CIDR applied when the Core request does not carry one.
///
/// Mirrors the legacy reconcile default (`reconcile.rs` `DEFAULT_NIC_CIDR`).
/// M2.2a residual: the Core create request does not model per-NIC CIDR/gateway,
/// so the runtime always uses this default with an empty gateway.
pub const DEFAULT_NIC_CIDR: &str = "10.0.0.0/24";

/// Ensure the per-VM runtime directory exists with 0o775 permissions and,
/// when running as root and the `chv-stord` group exists, is owned by that
/// group. Both the permission fix and the group chown are best-effort: a
/// failure to chown is logged and does not fail the create (mirrors
/// `reconcile.rs:842-845` semantics).
///
/// # Errors
/// Returns [`ChvError::Io`] only if the directory cannot be created or its
/// permissions cannot be applied.
pub async fn ensure_vm_runtime_dir(vm_dir: &Path) -> Result<(), ChvError> {
    tokio::fs::create_dir_all(vm_dir)
        .await
        .map_err(|e| ChvError::Io {
            path: vm_dir.display().to_string(),
            source: e,
        })?;
    set_mode_775(vm_dir).await?;
    #[cfg(target_family = "unix")]
    chown_chv_stord(vm_dir).await;
    Ok(())
}

#[cfg(target_family = "unix")]
use std::os::unix::fs::PermissionsExt;

#[cfg(target_family = "unix")]
async fn set_mode_775(vm_dir: &Path) -> Result<(), ChvError> {
    tokio::fs::set_permissions(vm_dir, std::fs::Permissions::from_mode(0o775))
        .await
        .map_err(|e| ChvError::Io {
            path: vm_dir.display().to_string(),
            source: e,
        })
}

#[cfg(not(target_family = "unix"))]
async fn set_mode_775(vm_dir: &Path) -> Result<(), ChvError> {
    // Non-unix targets have no meaningful mode to apply; creation succeeded.
    let _ = vm_dir;
    Ok(())
}

/// Best-effort chown of the VM runtime directory to the `chv-stord` group.
/// Only attempted when running as root (non-root agents cannot change group
/// ownership) and when the group exists; any failure is logged and swallowed
/// so a missing/blocked chown never breaks VM create.
#[cfg(target_family = "unix")]
async fn chown_chv_stord(vm_dir: &Path) {
    if !nix::unistd::geteuid().is_root() {
        return;
    }
    match nix::unistd::Group::from_name("chv-stord") {
        Ok(Some(group)) => {
            if let Err(e) = nix::unistd::chown(vm_dir, None, Some(group.gid)) {
                tracing::warn!(
                    path = %vm_dir.display(),
                    error = %e,
                    "chv-stord group chown failed on vm runtime dir; continuing"
                );
            }
        }
        Ok(None) => {
            tracing::debug!(path = %vm_dir.display(), "chv-stord group not present; skipping chown");
        }
        Err(e) => {
            tracing::warn!(
                path = %vm_dir.display(),
                error = %e,
                "chv-stord group lookup failed; skipping chown"
            );
        }
    }
}

/// Neutral host-resource controller consumed by the Core runtime to perform
/// the stord/nwd side effects of a VM create/delete.
///
/// The production implementation (`chv_agent_core::resources::AgentResourceController`)
/// connects a fresh `StordClient`/`NwdClient` per call, exactly matching the
/// existing agent convention. A publicly exported deterministic mock lives in
/// `crates/chv-agent-runtime-ch/src/mock.rs` so cross-crate integration tests
/// can drive both the runtime and the controller without real daemons.
#[async_trait]
pub trait HostResourceController: Send + Sync + 'static {
    /// Open a volume in the local backend and receive a live attachment handle
    /// plus the export path to attach to the VM.
    ///
    /// # Precondition
    /// The volume is not currently open by this VM lifecycle.
    /// # Postcondition (must be honoured by the caller)
    /// A successful open MUST be followed by [`Self::attach_volume_to_vm`] on
    /// the happy path, and by [`Self::close_volume`] on any path where the
    /// volume ends up unattached. On the happy path the eventual
    /// [`Self::detach_volume_from_vm`] MUST be followed by
    /// [`Self::close_volume`].
    async fn open_volume(
        &self,
        volume_id: &str,
        backend_class: &str,
        locator: &str,
        options: HashMap<String, String>,
        operation_id: Option<&str>,
    ) -> Result<(String, String, String), ChvError>; // (volume_id, attachment_handle, export_path)

    /// Attach an opened volume to a VM, returning the export kind and the final
    /// export path to present to the hypervisor.
    ///
    /// # Precondition
    /// [`Self::open_volume`] succeeded for `attachment_handle`.
    async fn attach_volume_to_vm(
        &self,
        volume_id: &str,
        vm_id: &str,
        attachment_handle: &str,
        operation_id: Option<&str>,
    ) -> Result<(String, String), ChvError>; // (export_kind, export_path)

    /// Detach a volume from a VM (non-forced).
    async fn detach_volume_from_vm(
        &self,
        volume_id: &str,
        vm_id: &str,
        force: bool,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError>;

    /// Close a previously opened volume attachment handle.
    ///
    /// # Precondition
    /// The volume has been detached (or was never attached).
    async fn close_volume(
        &self,
        volume_id: &str,
        attachment_handle: &str,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError>;

    /// Resolve the live attachment handle for `(volume_id, vm_id)` from
    /// the storage daemon's durable session table (#711).
    ///
    /// The delete-time fallback drain's observed attachments carry a
    /// handle only when one was durably cached; after an agent restart the
    /// in-memory handle map is gone. This read-only lookup recovers the
    /// handle from stord BEFORE [`Self::detach_volume_from_vm`] clears the
    /// session's `vm_id`, restoring the detach-then-close sequence the
    /// live path performs.
    ///
    /// Returns `Ok(None)` when no live session exists for the pair. RPC
    /// failures must surface as `Err`: callers keep their existing policy
    /// (the best-effort delete drain logs and continues; the legacy
    /// cleanup records a first error).
    async fn resolve_volume_handle(
        &self,
        volume_id: &str,
        vm_id: &str,
    ) -> Result<Option<String>, ChvError>;

    /// Close every open stord session for `volume_id` whose owning VM has
    /// already been cleared by a detach (#711).
    ///
    /// A crash or failed close between detach and close leaves exactly this
    /// shape: `vm_id` empty, `runtime_status` open, handle still present. The
    /// delete drain calls this after its own detach to close both the session
    /// it just detached and any leftover from an earlier interrupted attempt.
    /// Sessions still attached to a VM are never touched.
    async fn close_detached_volume_sessions(
        &self,
        volume_id: &str,
        operation_id: Option<&str>,
    ) -> Result<usize, ChvError>;

    /// Ensure a network's bridge topology exists.
    async fn ensure_network_topology(
        &self,
        network_id: &str,
        bridge_name: &str,
        subnet_cidr: &str,
        gateway_ip: &str,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError>;

    /// Apply the network's firewall policy on the node (default-deny plus
    /// the operator's rules) via nwd.
    ///
    /// # Precondition
    /// [`Self::ensure_network_topology`] succeeded for `network_id` — nwd
    /// scopes the policy to the CHV-owned interfaces of the ensured
    /// topology and fails closed without them.
    ///
    /// Callers apply the policy ONLY when the network carries a non-empty
    /// policy snapshot: nwd engages default-deny even for an empty
    /// ruleset, which would cut a rule-less network's guests off entirely
    /// (including DHCP). `policy_version` is nwd bookkeeping (recorded for
    /// later re-scoping); a content-derived value keeps retries idempotent.
    async fn set_firewall_policy(
        &self,
        network_id: &str,
        policy_version: &str,
        policy_json: &[u8],
        operation_id: Option<&str>,
    ) -> Result<(), ChvError>;

    /// Demolish a network's host topology on this node (bridge, namespace,
    /// dnsmasq, nft table — nwd's `delete_network_topology`).
    ///
    /// # Precondition
    /// NO other VM on this node still uses `network_id` — the local
    /// teardown deletes the bridge unconditionally, and a tap still
    /// enslaved on it would cut that VM's guest off. The caller's
    /// last-detach decision must come from a durable authority
    /// ([`NetworkUsageLookup`]), never from in-memory state alone
    /// (a daemon restart empties it).
    async fn delete_network_topology(
        &self,
        network_id: &str,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError>;

    /// Attach a NIC to a VM, returning the namespace and tap handles.
    async fn attach_vm_nic(
        &self,
        nic_id: &str,
        vm_id: &str,
        network_id: &str,
        mac_address: &str,
        ip_address: &str,
        operation_id: Option<&str>,
    ) -> Result<(String, String), ChvError>; // (namespace_handle, tap_handle)

    /// Detach a NIC from a VM.
    async fn detach_vm_nic(
        &self,
        nic_id: &str,
        vm_id: &str,
        network_id: &str,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError>;
}

/// Durable answer to "does any VM other than `excluding_vm` still reference
/// `network_id` on this node?" — the last-detach teardown safety check
/// (#356 N5).
///
/// The answer MUST come from an authority that survives daemon restarts
/// (the Core store's VM definitions). In-memory runtime state is not
/// sufficient: after a restart it is empty, and "no entry references the
/// network" would falsely authorize tearing down a network a still-running
/// pre-restart VM uses — deleting its bridge would cut that VM's guest off.
///
/// Implementations MUST fail CLOSED (report in-use) whenever the underlying
/// authority cannot be read: a skipped teardown leaves observable residue,
/// a wrong teardown causes an outage.
pub trait NetworkUsageLookup: Send + Sync + 'static {
    fn network_in_use(&self, network_id: &str, excluding_vm: &str) -> bool;
}

/// Fail-closed [`NetworkUsageLookup`]: every network reports as in use, so
/// no teardown ever fires. The runtime's default before
/// `with_network_usage` wires a real authority.
pub struct AlwaysInUse;

impl NetworkUsageLookup for AlwaysInUse {
    fn network_in_use(&self, _network_id: &str, _excluding_vm: &str) -> bool {
        true
    }
}

/// Observed volume + NIC attachments for one VM, as a durable authority
/// recorded them — the delete-time fallback drain input (#405).
///
/// Produced by an [`ObservedAttachmentSource`] when the runtime's
/// in-memory side-effect map has no entry for the VM (any VM created
/// before an agent restart — the map dies with the process).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObservedVmAttachments {
    /// `(volume_id, attachment_handle)` pairs, in the source's record
    /// order. The handle is `Some` only when the source durably recorded
    /// the stord attachment handle. The delete drain no longer depends on
    /// it: detach is keyed by volume + vm, and the drain then closes every
    /// detached session for the volume from stord's session table (#711).
    pub volumes: Vec<(String, Option<String>)>,
    /// `nic_id`s, in attach order.
    pub nics: Vec<String>,
}

impl ObservedVmAttachments {
    /// True when the source observed nothing to drain for the VM.
    pub fn is_empty(&self) -> bool {
        self.volumes.is_empty() && self.nics.is_empty()
    }
}

/// Durable answer to "which volume and NIC attachments did this node
/// observe for `vm_id`?" — the delete-time side-effect fallback drain's
/// input (#405).
///
/// The answer MUST come from a source that survives daemon restarts
/// (production: the NodeCache compatibility projection, whose VM axis the
/// startup rebuild re-seeds from the Core store before the executor
/// starts). In-memory runtime state is exactly what it replaces: after a
/// restart that map is empty, which is the leak being fixed.
///
/// The source is READ-ONLY from the drain's perspective: it must not
/// remove or mutate the observed state (the delete's own projection owns
/// that), so a FAILED delete can retry the fallback drain idempotently.
///
/// Implementations fail OPEN with empty attachments when the underlying
/// state cannot be read: an absent answer only leaves the pre-existing
/// logged residual (no drain), never a wrong drain.
#[async_trait]
pub trait ObservedAttachmentSource: Send + Sync + 'static {
    async fn observed_attachments(&self, vm_id: &str) -> ObservedVmAttachments;
}

/// Fail-open [`ObservedAttachmentSource`]: no attachments are ever
/// observed, so the delete-time fallback drain degrades to the logged
/// M2.2a crash residual. The runtime's default before
/// `with_observed_attachments` wires a real source.
pub struct NoObservedAttachments;

#[async_trait]
impl ObservedAttachmentSource for NoObservedAttachments {
    async fn observed_attachments(&self, _vm_id: &str) -> ObservedVmAttachments {
        ObservedVmAttachments::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_class_vocabulary_is_the_stord_backend_type_set() {
        // #379 DP3 pin: the frozen accept-time vocabulary is exactly the
        // stord `backend_type` values with "local" canonical — the local
        // aliases and the device-allowlist "block" alias are NOT accepted
        // here (they stay stord-boundary-only), and typos reject.
        for class in ["local", "iscsi", "ceph", "lvm"] {
            assert!(
                is_known_backend_class(class),
                "{class} must be accepted (stord backend_type vocabulary)"
            );
        }
        for rejected in ["local-file", "localdisk", "block", "lvv", "Local", ""] {
            assert!(
                !is_known_backend_class(rejected),
                "{rejected} must be rejected (aliases stay at the stord boundary; typos fail closed)"
            );
        }
        assert_eq!(DEFAULT_BACKEND_CLASS, "local");
        assert!(BACKEND_CLASSES.contains(&DEFAULT_BACKEND_CLASS));
    }

    /// #513 PR 3: the single shared capacity ceiling is 64 TiB in
    /// bytes, and the GiB derivation every `volume_size_gb` site
    /// computes from it is the historical `64 * 1024` GiB literal —
    /// the consolidation must be byte-identical on both scales.
    #[test]
    fn volume_capacity_ceiling_is_64_tib_on_both_scales() {
        assert_eq!(MAX_VOLUME_BYTES, 64 * 1024 * 1024 * 1024 * 1024);
        assert_eq!(MAX_VOLUME_BYTES / 1024 / 1024 / 1024, 64 * 1024);
        assert_eq!(MAX_VOLUME_BYTES % (1024 * 1024 * 1024), 0);
    }

    /// #379 DP5: the LVM locator convention is a `/dev/mapper/{vg}-{vid}`
    /// dm-path token that the standard install's device allowlist admits
    /// (the exact patterns `install.sh` writes), while the bare volume id
    /// — the alternative the design rejected — is denied by it.
    #[test]
    fn lvm_locator_matches_the_standard_device_allowlist() {
        let locator = lvm_locator("chv-vg", "vol-1");
        assert_eq!(locator, "/dev/mapper/chv-vg-vol-1");
        // The standard fixture is prefix-glob style (stord's
        // matches_device_pattern: a trailing-* pattern matches on the
        // prefix), so the honest agent-tier fixture is the prefix check.
        let admitted = STANDARD_DEVICE_ALLOWLIST
            .iter()
            .any(|pattern| locator.starts_with(pattern.trim_end_matches('*')));
        assert!(
            admitted,
            "the DP5 locator must pass the standard device_allowlist: {locator}"
        );
        // The rejected alternative: a bare volume id matches neither
        // pattern.
        let bare = "vol-1";
        let bare_admitted = STANDARD_DEVICE_ALLOWLIST
            .iter()
            .any(|pattern| bare.starts_with(pattern.trim_end_matches('*')));
        assert!(
            !bare_admitted,
            "a bare volume id must NOT pass the standard device_allowlist"
        );
    }

    /// #379 DP4: the accept-time capability predicate — definite
    /// match/mismatch and the fail-open shapes.
    #[test]
    fn node_offers_storage_class_compares_with_normalization() {
        use super::node_offers_storage_class;
        let lvm = vec!["lvm".to_string()];
        let local = vec!["local".to_string()];
        let legacy_probe = vec!["localdisk".to_string(), "nfs".to_string()];
        // Definite matches.
        assert_eq!(node_offers_storage_class(&lvm, Some("lvm")), Some(true));
        assert_eq!(node_offers_storage_class(&local, None), Some(true));
        assert_eq!(
            node_offers_storage_class(&legacy_probe, Some("local")),
            Some(true),
            "a legacy localdisk probe report serves a local-class volume"
        );
        // Definite mismatches (NULL = local: an lvm-only node cannot
        // serve a class-less volume — stord is single-backend).
        assert_eq!(node_offers_storage_class(&lvm, None), Some(false));
        assert_eq!(node_offers_storage_class(&lvm, Some("local")), Some(false));
        assert_eq!(node_offers_storage_class(&local, Some("lvm")), Some(false));
        assert_eq!(node_offers_storage_class(&local, Some("ceph")), Some(false));
        // Fail-open: an empty/never-reported list never rejects.
        assert_eq!(node_offers_storage_class(&[], Some("lvm")), None);
        assert_eq!(node_offers_storage_class(&[], None), None);
    }

    /// #379 DP3/DP4: normalization folds the local aliases and the
    /// empty/NULL request into the canonical `local`.
    #[test]
    fn normalize_storage_class_folds_local_aliases() {
        use super::normalize_storage_class;
        for class in ["", " ", "local", "local-file", "localdisk"] {
            assert_eq!(normalize_storage_class(class), "local");
        }
        assert_eq!(normalize_storage_class("lvm"), "lvm");
        assert_eq!(normalize_storage_class("ceph"), "ceph");
    }

    #[tokio::test]
    async fn rotate_console_log_keeps_exactly_one_generation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(vm_console_log(dir.path()), b"gen-2").unwrap();
        std::fs::write(vm_console_log_last(dir.path()), b"gen-1").unwrap();

        rotate_console_log(dir.path()).await.unwrap();

        assert!(
            !vm_console_log(dir.path()).exists(),
            "the live log must be rotated away"
        );
        assert_eq!(
            std::fs::read(vm_console_log_last(dir.path())).unwrap(),
            b"gen-2",
            "the rename must atomically overwrite the previous generation"
        );
    }

    #[tokio::test]
    async fn rotate_console_log_without_a_log_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        // No console.log: nothing to keep — not an error (the stop must
        // never fail on evidence retention).
        rotate_console_log(dir.path()).await.unwrap();
        assert!(!vm_console_log_last(dir.path()).exists());
    }
}
