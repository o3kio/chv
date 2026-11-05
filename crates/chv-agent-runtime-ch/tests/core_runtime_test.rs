//! Integration tests for the single full-side-effect Core runtime (M2.2a).
//!
//! These drive `CloudHypervisorCoreRuntime::execute` with real journal entries
//! whose `request` is the canonical envelope (as the executor would produce),
//! using `MockHostResourceController` + `MockCloudHypervisorAdapter` so no
//! real stord/nwd/KVM is needed. They also smoke the public mock exports for
//! cross-crate use.

use cellhv_core_executor::{CoreVmRuntime, RuntimeFailure};
use cellhv_core_operations::{MutationCommand, OperationJournalEntry};
use cellhv_core_types::{
    BootSpec, ComputeSpec, NetworkAttachmentRef, ObservedPowerState, Operation, OperationId,
    OperationKind, OperationStatus, RequestedPowerState, ResourceVersion, StorageAttachmentRef,
    VmDefinition, VmId,
};
use chv_agent_runtime_ch::core_runtime::CloudHypervisorCoreRuntime;
use chv_agent_runtime_ch::mock::{MockCloudHypervisorAdapter, MockHostResourceController};
use chv_hypervisor_api::resources::{
    vm_api_socket, vm_config_file, vm_pid_file, ObservedAttachmentSource, ObservedVmAttachments,
};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

fn definition(vm_id: &str, nvols: usize, nnics: usize) -> VmDefinition {
    let storage = (0..nvols)
        .map(|i| StorageAttachmentRef {
            attachment_id: format!("vol-{i}"),
            storage_ref: format!("vol-{i}"),
            read_only: false,
            size_bytes: None,
            seed_from: None,
            backend_class: None,
        })
        .collect();
    let networks = (0..nnics)
        .map(|i| NetworkAttachmentRef {
            attachment_id: format!("{vm_id}-net{i}"),
            network_ref: format!("net-{i}"),
            mac_address: Some(format!("02:00:00:00:00:{i:02x}")),
            addressing: None,
            firewall_policy_json: None,
        })
        .collect();
    VmDefinition {
        id: VmId::new(vm_id).expect("vm id"),
        name: format!("{vm_id}-guest"),
        boot: BootSpec::new("/kernel").expect("boot"),
        compute: ComputeSpec::new(2, 1024).expect("compute"),
        storage,
        networks,
        requested_power_state: RequestedPowerState::Stopped,
        observed_power_state: ObservedPowerState::Unknown,
        resource_version: ResourceVersion::new(1).expect("version"),
        cloud_init_userdata: None,
        hypervisor_tuning: None,
        vsock: None,
    }
}

/// Build the canonical envelope JSON exactly as `canonical_request` does:
/// `{"command": <internally-tagged command>, "expected_vm_version": N}`.
fn envelope(command: MutationCommand) -> serde_json::Value {
    serde_json::json!({
        "command": command,
        "expected_vm_version": 1,
    })
}

/// Build a journal entry carrying the given request value.
fn entry(
    kind: OperationKind,
    vm_id: &str,
    op_id: &str,
    request: serde_json::Value,
) -> OperationJournalEntry {
    OperationJournalEntry {
        operation: Operation {
            id: OperationId::new(op_id).expect("op id"),
            kind,
            vm_id: VmId::new(vm_id).expect("vm id"),
            status: OperationStatus::Running,
            request_fingerprint: "fingerprint".to_string(),
            attempt_count: 1,
            max_attempts: 3,
        },
        request,
        result: None,
        error: None,
        request_metadata: None,
        recovery_assessment: None,
    }
}

struct Harness {
    _dir: TempDir,
    runtime_dir: PathBuf,
    adapter: Arc<MockCloudHypervisorAdapter>,
    controller: Arc<MockHostResourceController>,
    runtime: Arc<CloudHypervisorCoreRuntime>,
}

fn harness(fail_on: Option<&str>) -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let adapter = Arc::new(MockCloudHypervisorAdapter::default());
    let controller = Arc::new(match fail_on {
        Some(step) => MockHostResourceController::new_with_fail(step),
        None => MockHostResourceController::new(),
    });
    let runtime = Arc::new(CloudHypervisorCoreRuntime::new(
        adapter.clone(),
        controller.clone(),
        runtime_dir.clone(),
    ));
    Harness {
        _dir: dir,
        runtime_dir,
        adapter,
        controller,
        runtime,
    }
}

/// Same as [`harness`] but with the runtime's node-level default backend
/// class overridden (#379 PR 1), so tests can pin that the configured
/// class value — not an inline literal — reaches the storage controller.
fn harness_with_backend_class(backend_class: &str) -> Harness {
    harness_with(|rt| rt.with_default_backend_class(backend_class))
}

/// Same as [`harness`] but with the runtime's LVM volume group overridden
/// (#379 PR 3, DP5), so tests can pin that the operator's configured
/// group — not the default — shapes the dm-path locator of LVM-class
/// opens. The node-level default backend class stays "local"; tests drive
/// the LVM arm via the per-attachment class.
fn harness_with_lvm(volume_group: &str) -> Harness {
    harness_with(|rt| rt.with_lvm_volume_group(volume_group))
}

fn harness_with(
    configure: impl FnOnce(CloudHypervisorCoreRuntime) -> CloudHypervisorCoreRuntime,
) -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let adapter = Arc::new(MockCloudHypervisorAdapter::default());
    let controller = Arc::new(MockHostResourceController::new());
    let runtime = Arc::new(configure(CloudHypervisorCoreRuntime::new(
        adapter.clone(),
        controller.clone(),
        runtime_dir.clone(),
    )));
    Harness {
        _dir: dir,
        runtime_dir,
        adapter,
        controller,
        runtime,
    }
}

fn calls(controller: &MockHostResourceController) -> Vec<String> {
    controller.calls.lock().expect("calls lock").clone()
}

fn is_subsequence(haystack: &[String], needle: &[&str]) -> bool {
    let mut hay = haystack.iter();
    for want in needle {
        loop {
            match hay.next() {
                Some(got) if got == want => break,
                Some(_) => continue,
                None => return false,
            }
        }
    }
    true
}

fn vm_dir_path(h: &Harness, vm_id: &str) -> PathBuf {
    h.runtime_dir.join("vms").join(vm_id)
}

#[tokio::test]
async fn create_vm_performs_full_side_effects() {
    let h = harness(None);
    let vm_id = "vm-a";
    let command = MutationCommand::CreateVm {
        definition: definition(vm_id, 2, 1),
    };
    let result = h
        .runtime
        .execute(entry(
            OperationKind::CreateVm,
            vm_id,
            "op-create",
            envelope(command),
        ))
        .await;
    assert!(result.is_ok(), "create must succeed: {result:?}");

    // Full side-effect sequencing: open+attach per volume in order, then
    // ensure+attach_nic per network.
    let log = calls(&h.controller);
    assert!(
        is_subsequence(
            &log,
            &["open:vol-0", "attach:vol-0", "open:vol-1", "attach:vol-1"]
        ),
        "volume open/attach ordering: {log:?}"
    );
    assert!(
        is_subsequence(&log, &["ensure:net-0", "attach_nic:vm-a-net-0"]),
        "network ensure/attach ordering: {log:?}"
    );

    // Adapter received the exact config derived from the Core definition.
    let vms = h.adapter.vms.lock().expect("vms lock");
    let config = vms
        .get(vm_id)
        .expect("vm must be present in the adapter map");
    assert_eq!(
        config.api_socket_path,
        h.runtime_dir.join("vms").join(vm_id).join("vm.sock")
    );
    assert_eq!(config.cpus, 2);
    assert_eq!(config.memory_bytes, 1024);
    assert_eq!(config.kernel_path, PathBuf::from("/kernel"));
    assert_eq!(config.disks.len(), 2, "two disks");
    assert_eq!(config.disks[0].path, PathBuf::from("/dev/mock/vol-0"));
    assert_eq!(config.disks[0].id.as_deref(), Some("vol-0"));
    assert_eq!(config.disks[1].path, PathBuf::from("/dev/mock/vol-1"));
    assert_eq!(config.disks[1].id.as_deref(), Some("vol-1"));
    assert_eq!(config.nics.len(), 1, "one nic");
    assert_eq!(config.nics[0].tap_name, "tap-vm-a-net-0");
    assert_eq!(config.nics[0].mac_address, "02:00:00:00:00:00");
    assert_eq!(config.nics[0].cidr, "10.0.0.0/24");
    // Without control-plane addressing the NIC stays unassigned and the
    // gateway empty (topology defaults), and no tuning/userdata is invented.
    assert_eq!(config.nics[0].ip_address, "");
    assert_eq!(config.nics[0].gateway, "");
    assert_eq!(config.cloud_init_userdata, None);
    assert_eq!(config.hypervisor_overrides, None);
    drop(vms);
    // No provisioning hints on the definition: both volumes open bare.
    let options = h.controller.open_options.lock().expect("options lock");
    assert_eq!(options.len(), 2, "two opens");
    assert!(options[0].2.is_empty(), "no hints on vol-0: {options:?}");
    assert!(options[1].2.is_empty(), "no hints on vol-1: {options:?}");
    drop(options);

    // VM runtime dir exists with mode 0o775.
    let vm_dir = vm_dir_path(&h, vm_id);
    let metadata = std::fs::metadata(&vm_dir).expect("vm dir exists");
    assert_eq!(
        metadata.permissions().mode() & 0o777,
        0o775,
        "vm dir must be 0o775"
    );
}

#[tokio::test]
async fn create_vm_volume_opens_default_to_the_local_backend_class() {
    // #379 PR 1 pin (A3): the Core executor's volume opens resolve the
    // backend class from the runtime instead of an inline literal; a
    // default-constructed runtime must keep sending exactly "local" (the
    // historical literal) while no producer exists.
    let h = harness(None);
    let vm_id = "vm-cls-default";
    let command = MutationCommand::CreateVm {
        definition: definition(vm_id, 1, 1),
    };
    let result = h
        .runtime
        .execute(entry(
            OperationKind::CreateVm,
            vm_id,
            "op-create-cls-default",
            envelope(command),
        ))
        .await;
    assert!(result.is_ok(), "create must succeed: {result:?}");
    let opens = h.controller.open_options.lock().expect("options lock");
    assert_eq!(opens.len(), 1, "one open: {opens:?}");
    assert_eq!(
        opens[0].1, "local",
        "absent class must default to the historical local literal"
    );
}

#[tokio::test]
async fn create_vm_volume_opens_thread_the_configured_backend_class() {
    // #379 PR 1 pin (A3), red/green: the node-level default backend class
    // configured on the runtime must reach the storage controller's open
    // call — reverting the A3 site to the "local" literal fails this test.
    let h = harness_with_backend_class("lvm");
    let vm_id = "vm-cls-lvm";
    let command = MutationCommand::CreateVm {
        definition: definition(vm_id, 1, 1),
    };
    let result = h
        .runtime
        .execute(entry(
            OperationKind::CreateVm,
            vm_id,
            "op-create-cls-lvm",
            envelope(command),
        ))
        .await;
    assert!(result.is_ok(), "create must succeed: {result:?}");
    let opens = h.controller.open_options.lock().expect("options lock");
    assert_eq!(opens.len(), 1, "one open: {opens:?}");
    assert_eq!(opens[0].1, "lvm", "the configured class must reach stord");
}

#[tokio::test]
async fn create_vm_volume_opens_thread_the_attachment_backend_class() {
    // #379 PR 2 pin (A3 consumer / A7 producer): the per-attachment class
    // carried by the Core definition overrides the runtime's node-level
    // default — a default-constructed runtime ("local") opens a
    // class-carrying attachment with the definition's class, and a
    // class-less attachment on the SAME definition still gets the
    // runtime default. Red/green: reverting the A3 site to the bare
    // runtime default fails the vol-cls assertion.
    let h = harness(None);
    let vm_id = "vm-cls-att";
    let mut def = definition(vm_id, 2, 0);
    def.storage[0].backend_class = Some("lvm".to_string());
    let command = MutationCommand::CreateVm { definition: def };
    let result = h
        .runtime
        .execute(entry(
            OperationKind::CreateVm,
            vm_id,
            "op-create-cls-att",
            envelope(command),
        ))
        .await;
    assert!(result.is_ok(), "create must succeed: {result:?}");
    let opens = h.controller.open_options.lock().expect("options lock");
    assert_eq!(opens.len(), 2, "two opens: {opens:?}");
    assert_eq!(
        opens[0].1, "lvm",
        "the attachment's class must override the runtime default"
    );
    assert_eq!(
        opens[1].1, "local",
        "a class-less attachment keeps the runtime default"
    );
}

#[tokio::test]
async fn create_vm_lvm_opens_carry_the_dm_path_locator() {
    // #379 PR 3 pin (A3 / DP5), red/green: an LVM-class volume open must
    // carry the `/dev/mapper/{vg}-{vid}` dm-path locator (shaped from the
    // runtime's configured volume group) instead of the historical
    // `{volume_id}.img` under the VM dir — the device_allowlist on an LVM
    // node admits the dm-path token and denies the VM-dir path. Reverting
    // the A3 locator branch to the always-img form fails this test.
    let h = harness_with_lvm("qual-vg");
    let vm_id = "vm-lvm-loc";
    let mut def = definition(vm_id, 2, 0);
    def.storage[1].backend_class = Some("lvm".to_string());
    let command = MutationCommand::CreateVm { definition: def };
    let result = h
        .runtime
        .execute(entry(
            OperationKind::CreateVm,
            vm_id,
            "op-create-lvm-loc",
            envelope(command),
        ))
        .await;
    assert!(result.is_ok(), "create must succeed: {result:?}");
    let locators = h.controller.open_locators.lock().expect("locators lock");
    assert_eq!(locators.len(), 2, "two opens: {locators:?}");
    assert_eq!(
        locators[0].1,
        vm_dir_path(&h, vm_id).join("vol-0.img").to_string_lossy(),
        "a local-class open keeps the historical VM-dir .img locator"
    );
    assert_eq!(
        locators[1].1, "/dev/mapper/qual-vg-vol-1",
        "an lvm-class open must carry the dm-path locator shaped from the configured VG"
    );
    drop(locators);
}

#[tokio::test]
async fn create_vm_carries_provisioning_hints_addressing_and_tuning() {
    let h = harness(None);
    let vm_id = "vm-tuned";
    let mut def = definition(vm_id, 2, 1);
    // Boot disk: sized + seeded; second disk: pre-provisioned (no hints).
    def.storage[0].size_bytes = Some(10_737_418_240);
    def.storage[0].seed_from = Some("/var/lib/chv/images/ubuntu.img".to_string());
    def.networks[0].addressing = Some(cellhv_core_types::NicAddressing {
        ip_address: "10.200.0.47".to_string(),
        cidr: "10.200.0.0/24".to_string(),
        gateway: "10.200.0.1".to_string(),
    });
    def.cloud_init_userdata = Some("#cloud-config".to_string());
    def.hypervisor_tuning = Some(cellhv_core_types::HypervisorTuning {
        cpu_nested: Some(true),
        rng_src: Some("/dev/hwrng".to_string()),
        ..Default::default()
    });
    let command = MutationCommand::CreateVm { definition: def };
    let result = h
        .runtime
        .execute(entry(
            OperationKind::CreateVm,
            vm_id,
            "op-create-tuned",
            envelope(command),
        ))
        .await;
    assert!(result.is_ok(), "create must succeed: {result:?}");

    // Provisioning hints reach the storage layer exactly once, on the boot
    // disk; the pre-provisioned second disk opens without options.
    let options = h
        .controller
        .open_options
        .lock()
        .expect("options lock")
        .clone();
    assert_eq!(options.len(), 2, "two opens: {options:?}");
    assert_eq!(options[0].0, "vol-0");
    assert_eq!(
        options[0].2.get("size_bytes").map(String::as_str),
        Some("10737418240")
    );
    assert_eq!(
        options[0].2.get("seed_from").map(String::as_str),
        Some("/var/lib/chv/images/ubuntu.img")
    );
    assert_eq!(options[1].0, "vol-1");
    assert!(options[1].2.is_empty(), "no hints on vol-1: {options:?}");

    // Addressing and tuning reach the hypervisor config verbatim.
    let vms = h.adapter.vms.lock().expect("vms lock");
    let config = vms
        .get(vm_id)
        .expect("vm must be present in the adapter map");
    assert_eq!(config.nics[0].ip_address, "10.200.0.47");
    assert_eq!(config.nics[0].cidr, "10.200.0.0/24");
    assert_eq!(config.nics[0].gateway, "10.200.0.1");
    assert_eq!(config.cloud_init_userdata.as_deref(), Some("#cloud-config"));
    let overrides = config
        .hypervisor_overrides
        .as_ref()
        .expect("tuning must reach the hypervisor config");
    assert_eq!(overrides.cpu_nested, Some(true));
    assert_eq!(overrides.rng_src.as_deref(), Some("/dev/hwrng"));
}

#[tokio::test]
async fn create_vm_threads_the_vsock_device_opt_in() {
    let h = harness(None);

    // Opt-in: the device is prepared with a node-scoped, durable,
    // monotonic CID and the VM runtime-dir socket path.
    let vm_id = "vm-vsock";
    let mut def = definition(vm_id, 1, 1);
    def.vsock = Some(cellhv_core_types::VsockDeviceSpec { enabled: true });
    let result = h
        .runtime
        .execute(entry(
            OperationKind::CreateVm,
            vm_id,
            "op-create-vsock",
            envelope(MutationCommand::CreateVm { definition: def }),
        ))
        .await;
    assert!(result.is_ok(), "create must succeed: {result:?}");
    {
        let vms = h.adapter.vms.lock().expect("vms lock");
        let config = vms.get(vm_id).expect("vm must be present");
        let vsock = config.vsock.as_ref().expect("device must be attached");
        assert_eq!(vsock.cid, 1000, "fresh node: first CID is the floor");
        assert_eq!(
            vsock.socket_path,
            vm_dir_path(&h, vm_id).join("vsock.sock"),
            "device socket lives in the VM runtime dir"
        );
    }
    // The cursor is durable node state: it records the last allocation.
    assert_eq!(
        std::fs::read_to_string(h.runtime_dir.join("vsock-cid-cursor"))
            .expect("cursor file")
            .trim(),
        "1000"
    );

    // A second opted-in VM gets a distinct, monotonic CID — node-scoped
    // uniqueness by construction.
    let vm_id2 = "vm-vsock-2";
    let mut def2 = definition(vm_id2, 1, 1);
    def2.vsock = Some(cellhv_core_types::VsockDeviceSpec { enabled: true });
    let result = h
        .runtime
        .execute(entry(
            OperationKind::CreateVm,
            vm_id2,
            "op-create-vsock-2",
            envelope(MutationCommand::CreateVm { definition: def2 }),
        ))
        .await;
    assert!(result.is_ok(), "second create must succeed: {result:?}");
    {
        let vms = h.adapter.vms.lock().expect("vms lock");
        let config = vms.get(vm_id2).expect("vm2 must be present");
        assert_eq!(config.vsock.as_ref().expect("device 2").cid, 1001);
    }

    // No opt-in, no device — the default is and stays inert (no silent
    // attach, boot mode untouched).
    let vm_id3 = "vm-plain";
    let result = h
        .runtime
        .execute(entry(
            OperationKind::CreateVm,
            vm_id3,
            "op-create-plain",
            envelope(MutationCommand::CreateVm {
                definition: definition(vm_id3, 1, 1),
            }),
        ))
        .await;
    assert!(result.is_ok(), "plain create must succeed: {result:?}");
    {
        let vms = h.adapter.vms.lock().expect("vms lock");
        let config = vms.get(vm_id3).expect("vm3 must be present");
        assert!(
            config.vsock.is_none(),
            "no device without the opt-in: {:?}",
            config.vsock
        );
    }
}

#[tokio::test]
async fn create_vm_applies_attach_time_firewall_policy() {
    // #355: a policy snapshot carried on the network attachment is
    // applied via nwd AFTER the topology is ensured (nwd scopes the
    // policy to the ensured topology's CHV-owned interfaces and fails
    // closed without them) and BEFORE the NIC attaches (no guest traffic
    // outside the CHV boundary). The recorded policy is the snapshot
    // verbatim.
    let h = harness(None);
    let vm_id = "vm-policy";
    let policy = r#"[{"direction":"inbound","action":"accept","protocol":"icmp"}]"#;
    let mut def = definition(vm_id, 1, 1);
    def.networks[0].firewall_policy_json = Some(policy.to_string());
    let command = MutationCommand::CreateVm { definition: def };
    let result = h
        .runtime
        .execute(entry(
            OperationKind::CreateVm,
            vm_id,
            "op-create-policy",
            envelope(command),
        ))
        .await;
    assert!(result.is_ok(), "create must succeed: {result:?}");

    let calls = h.controller.calls.lock().expect("calls lock").clone();
    let ensure = calls
        .iter()
        .position(|c| c.starts_with("ensure:net-0"))
        .expect("topology ensure must be recorded");
    let policy_at = calls
        .iter()
        .position(|c| c.starts_with("policy:net-0:") && c.ends_with(policy))
        .expect("policy application must be recorded with the snapshot verbatim");
    let attach = calls
        .iter()
        .position(|c| c.starts_with("attach_nic:"))
        .expect("nic attach must be recorded");
    assert!(
        ensure < policy_at && policy_at < attach,
        "policy must apply after ensure and before the nic attaches: {calls:?}"
    );
}

#[tokio::test]
async fn policy_version_is_content_derived_and_stable_across_nics_and_retries() {
    // The policy_version passed to nwd is bookkeeping for later
    // re-scoping, but it must be CONTENT-derived so that retries (and
    // multiple NICs on the same network within one create) are
    // idempotent — the same snapshot always yields the same version.
    // One set_firewall_policy is issued per NIC (network-scoped RPC,
    // cheap and idempotent).
    let h = harness(None);
    let policy = r#"[{"direction":"inbound","action":"accept","protocol":"tcp"}]"#;

    // Two NICs on DIFFERENT networks (net-0, net-1) carrying the SAME
    // snapshot, in one create.
    let vm_id = "vm-two-nics";
    let mut def = definition(vm_id, 1, 2);
    def.networks[0].firewall_policy_json = Some(policy.to_string());
    def.networks[1].firewall_policy_json = Some(policy.to_string());
    let result = h
        .runtime
        .execute(entry(
            OperationKind::CreateVm,
            vm_id,
            "op-two-nics",
            envelope(MutationCommand::CreateVm { definition: def }),
        ))
        .await;
    assert!(result.is_ok(), "create must succeed: {result:?}");

    // A retry-equivalent: a second VM with the same snapshot.
    let vm_id = "vm-two-nics-2";
    let mut def = definition(vm_id, 1, 1);
    def.networks[0].firewall_policy_json = Some(policy.to_string());
    let result = h
        .runtime
        .execute(entry(
            OperationKind::CreateVm,
            vm_id,
            "op-two-nics-2",
            envelope(MutationCommand::CreateVm { definition: def }),
        ))
        .await;
    assert!(result.is_ok(), "create must succeed: {result:?}");

    let calls = h.controller.calls.lock().expect("calls lock").clone();
    let policy_calls: Vec<&String> = calls
        .iter()
        .filter(|c| c.starts_with("policy:") && c.ends_with(policy))
        .collect();
    assert_eq!(
        policy_calls.len(),
        3,
        "one policy application per NIC (2 nics + 1 on the second VM): {calls:?}"
    );
    // Records are "policy:<network_id>:<version>:<json>"; the version is
    // the field between the network id and the known json suffix.
    let suffix = format!(":{policy}");
    let versions: std::collections::HashSet<&str> = policy_calls
        .iter()
        .map(|c| {
            let net_start = "policy:".len();
            let net_end = net_start + c[net_start..].find(':').expect("network delimiter") + 1;
            &c[net_end..c.len() - suffix.len()]
        })
        .collect();
    assert_eq!(
        versions.len(),
        1,
        "the same snapshot must always yield the same content-derived version: {calls:?}"
    );
    assert!(
        versions.iter().all(|v| v.starts_with("attach-")),
        "versions must carry the attach- prefix: {calls:?}"
    );
}

#[tokio::test]
async fn create_vm_applies_the_dp4_baseline_without_user_rules() {
    // The DP4 half of #355 (ruled 2026-10-08): no snapshot, a blank, or
    // an empty ruleset (`[]` = no user rules) is NO LONGER a skip —
    // rule-less networks get the shared BASELINE (DHCP/DNS/conntrack
    // allows) on top of default-deny at attach, replacing both the
    // pre-DP4 unfiltered skip and the #360 never-apply cutoff. Every
    // VM create on such a network records exactly one baseline policy
    // application per NIC, and its content-derived version is stable.
    let h = harness(None);
    let baseline = chv_common::firewall::baseline_policy_json();

    for (vm_id, snapshot) in [
        ("vm-nopolicy", None),
        ("vm-emptylist", Some("[]")),
        ("vm-blank", Some("")),
        ("vm-whitespace", Some("   ")),
    ] {
        let mut def = definition(vm_id, 1, 1);
        def.networks[0].firewall_policy_json = snapshot.map(str::to_string);
        let result = h
            .runtime
            .execute(entry(
                OperationKind::CreateVm,
                vm_id,
                &format!("op-create-{vm_id}"),
                envelope(MutationCommand::CreateVm { definition: def }),
            ))
            .await;
        assert!(result.is_ok(), "create must succeed: {result:?}");
    }

    let calls = h.controller.calls.lock().expect("calls lock").clone();
    let policy_calls: Vec<&String> = calls
        .iter()
        .filter(|c| c.starts_with("policy:") && c.ends_with(&baseline))
        .collect();
    assert_eq!(
        policy_calls.len(),
        4,
        "one baseline policy application per create: {calls:?}"
    );
    assert!(
        policy_calls
            .iter()
            .all(|c| c.starts_with("policy:net-0:attach-")),
        "baseline applications carry the content-derived attach- version: {calls:?}"
    );
    // One version across all four — the baseline is a constant, so its
    // content-derived version must be too (retry idempotency).
    let versions: std::collections::HashSet<&str> = policy_calls
        .iter()
        .map(|c| {
            let net_start = "policy:".len();
            let net_end = net_start + c[net_start..].find(':').expect("network delimiter") + 1;
            let suffix = format!(":{baseline}");
            &c[net_end..c.len() - suffix.len()]
        })
        .collect();
    assert_eq!(
        versions.len(),
        1,
        "the baseline's content-derived version must be constant: {calls:?}"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|c| c.starts_with("ensure:net-0"))
            .count(),
        4,
        "topology is still ensured for every VM: {calls:?}"
    );
}

#[tokio::test]
async fn create_vm_defaults_empty_nic_cidr_like_the_legacy_path() {
    // A network row without a CIDR makes the BFF emit an empty cidr next to
    // an assigned ip (build_agent_vm_spec uses unwrap_or_default); the Core
    // executor must default it to the topology default exactly like the
    // legacy reconcile path instead of routing an empty CIDR to nwd.
    let h = harness(None);
    let vm_id = "vm-nocidr";
    let mut def = definition(vm_id, 1, 1);
    def.networks[0].addressing = Some(cellhv_core_types::NicAddressing {
        ip_address: "10.200.0.47".to_string(),
        cidr: String::new(),
        gateway: String::new(),
    });
    let command = MutationCommand::CreateVm { definition: def };
    let result = h
        .runtime
        .execute(entry(
            OperationKind::CreateVm,
            vm_id,
            "op-create-nocidr",
            envelope(command),
        ))
        .await;
    assert!(result.is_ok(), "create must succeed: {result:?}");
    let vms = h.adapter.vms.lock().expect("vms lock");
    let config = vms
        .get(vm_id)
        .expect("vm must be present in the adapter map");
    assert_eq!(config.nics[0].cidr, "10.0.0.0/24");
    assert_eq!(config.nics[0].ip_address, "10.200.0.47");
    assert_eq!(config.nics[0].gateway, "");
}

#[tokio::test]
async fn create_vm_failure_midway_unwinds() {
    // Fail when opening the 2nd volume: volume 1 is opened+attached and must be
    // detached+closed, volume 2's open fails so nothing to clean, and the VM
    // dir is removed.
    let h = harness(Some("open:2"));
    let vm_id = "vm-b";
    let command = MutationCommand::CreateVm {
        definition: definition(vm_id, 2, 1),
    };
    let result = h
        .runtime
        .execute(entry(
            OperationKind::CreateVm,
            vm_id,
            "op-fail",
            envelope(command),
        ))
        .await;
    assert!(result.is_err(), "create must fail: {result:?}");

    // The adapter must never have been asked to create the VM.
    assert!(
        !h.adapter.vms.lock().expect("vms lock").contains_key(vm_id),
        "adapter must not contain the failed VM"
    );

    // No leaked handles: every opened volume has a matching close, every
    // attached nic a matching detach_nic.
    let log = calls(&h.controller);
    assert!(
        is_subsequence(&log, &["open:vol-0", "attach:vol-0", "detach:vol-0"]),
        "first volume must be closed: {log:?}"
    );
    let opened: Vec<&str> = log.iter().filter_map(|c| c.strip_prefix("open:")).collect();
    for volume in &opened {
        assert!(
            log.iter().any(|c| c == &format!("close:{volume}")),
            "no close for opened volume {volume}: {log:?}"
        );
    }
    let attached_nics: Vec<&str> = log
        .iter()
        .filter_map(|c| c.strip_prefix("attach_nic:"))
        .collect();
    for nic in &attached_nics {
        assert!(
            log.iter().any(|c| c == &format!("detach_nic:{nic}")),
            "no detach_nic for attached nic {nic}: {log:?}"
        );
    }
    // VM dir removed (best-effort teardown).
    assert!(
        !vm_dir_path(&h, vm_id).exists(),
        "vm dir should be removed after failed create"
    );
}

/// #368 residue-idempotency (mock tier): the requeued create a re-drive
/// executes is a FRESH CreateVm operation over the same definition — its
/// effector re-runs the full side-effect set (open/attach per volume,
/// ensure/attach_nic per network, adapter create, vm dir) against the
/// residue a terminally-failed create can leave behind (the M4.7 F3
/// shape: storage that stayed attached because the failing provider was
/// also the one the unwind talks to). The re-execution must converge,
/// not fail on pre-attached storage.
///
/// The local-backend residue semantics (re-open does not re-seed or
/// resize an already-provisioned volume) are pinned separately against
/// a real `LocalFileBackend` in chv-stord-core; ceph/iscsi/lvm have no
/// equivalent pin (unpinned, see the #368 design doc residual risk).
#[tokio::test]
async fn create_reexecution_over_attached_residue_converges() {
    let h = harness(None);
    let vm_id = "vm-redrive";
    let command = MutationCommand::CreateVm {
        definition: definition(vm_id, 2, 1),
    };

    // The original execution: full side effects land (storage attached,
    // topology ensured, VM created).
    h.runtime
        .execute(entry(
            OperationKind::CreateVm,
            vm_id,
            "op-create-original",
            envelope(command.clone()),
        ))
        .await
        .expect("original create must succeed");
    let first_run_log = calls(&h.controller);
    assert!(is_subsequence(
        &first_run_log,
        &["open:vol-0", "attach:vol-0", "open:vol-1", "attach:vol-1"]
    ));

    // The requeued re-drive: a NEW operation id carrying the same
    // definition (exactly what `requeue_failed_create` journals — the
    // envelope is re-derived from the live row). It re-executes the full
    // side-effect set against the already-attached residue and must
    // converge.
    h.runtime
        .execute(entry(
            OperationKind::CreateVm,
            vm_id,
            "op-create-requeued",
            envelope(command),
        ))
        .await
        .expect("re-executed create over residue must succeed");

    // The re-drive actually re-ran the side-effect set (it did not skip
    // because "already done") — one full open/attach pass per execution.
    let log = calls(&h.controller);
    let opens: Vec<&str> = log.iter().filter_map(|c| c.strip_prefix("open:")).collect();
    assert_eq!(
        opens,
        vec!["vol-0", "vol-1", "vol-0", "vol-1"],
        "each execution must open every volume: {log:?}"
    );
    let attaches: Vec<&str> = log
        .iter()
        .filter_map(|c| c.strip_prefix("attach:"))
        .collect();
    assert_eq!(
        attaches,
        vec!["vol-0", "vol-1", "vol-0", "vol-1"],
        "each execution must attach every volume: {log:?}"
    );
    // The VM runtime dir is create_dir_all — re-execution keeps it.
    assert!(vm_dir_path(&h, vm_id).exists());
}

#[tokio::test]
async fn delete_vm_drains_side_effects() {
    let h = harness(None);
    let vm_id = "vm-c";
    let create = MutationCommand::CreateVm {
        definition: definition(vm_id, 2, 1),
    };
    h.runtime
        .execute(entry(
            OperationKind::CreateVm,
            vm_id,
            "op-create",
            envelope(create),
        ))
        .await
        .expect("create succeeds");

    let delete = MutationCommand::DeleteVm {
        vm_id: VmId::new(vm_id).expect("vm id"),
    };
    let result = h
        .runtime
        .execute(entry(
            OperationKind::DeleteVm,
            vm_id,
            "op-del",
            envelope(delete),
        ))
        .await;
    assert!(result.is_ok(), "delete must succeed: {result:?}");
    assert!(
        !h.adapter.vms.lock().expect("vms lock").contains_key(vm_id),
        "adapter must no longer contain the VM"
    );

    // Volumes drained in reverse (detach+close each), then NICs detached.
    let log = calls(&h.controller);
    assert!(
        is_subsequence(
            &log,
            &[
                "detach:vol-1",
                "close:vol-1",
                "detach:vol-0",
                "close:vol-0",
                "detach_nic:vm-c-net-0",
            ]
        ),
        "delete drain ordering: {log:?}"
    );
    let opened: Vec<&str> = log.iter().filter_map(|c| c.strip_prefix("open:")).collect();
    let closed: Vec<&str> = log
        .iter()
        .filter_map(|c| c.strip_prefix("close:"))
        .collect();
    assert_eq!(
        opened.len(),
        closed.len(),
        "every opened volume must be closed: {log:?}"
    );
    for volume in &opened {
        assert!(
            closed.contains(volume),
            "no close for opened volume {volume}: {log:?}"
        );
    }
}

#[tokio::test]
async fn create_then_restart_delete_without_state() {
    let h = harness(None);
    let vm_id = "vm-d";
    let create = MutationCommand::CreateVm {
        definition: definition(vm_id, 1, 1),
    };
    h.runtime
        .execute(entry(
            OperationKind::CreateVm,
            vm_id,
            "op-create",
            envelope(create),
        ))
        .await
        .expect("create succeeds");

    // Simulate an agent restart: a FRESH runtime with an empty in-memory map
    // (same adapter + controller — the daemons/CH are still up).
    let restarted = Arc::new(CloudHypervisorCoreRuntime::new(
        h.adapter.clone(),
        h.controller.clone(),
        h.runtime_dir.clone(),
    ));
    let delete = MutationCommand::DeleteVm {
        vm_id: VmId::new(vm_id).expect("vm id"),
    };
    let result = restarted
        .execute(entry(
            OperationKind::DeleteVm,
            vm_id,
            "op-del",
            envelope(delete),
        ))
        .await;
    // Delete still succeeds (the missing handle state is a logged crash
    // residual, not an op failure); the adapter VM is gone.
    assert!(
        result.is_ok(),
        "delete after restart must succeed: {result:?}"
    );
    assert!(
        !h.adapter.vms.lock().expect("vms lock").contains_key(vm_id),
        "adapter must no longer contain the VM"
    );
    // No drain was possible (fresh runtime has no handle state).
    let log = calls(&h.controller);
    assert!(
        !log.iter()
            .any(|c| c.starts_with("detach:") || c.starts_with("close:")),
        "no handle drain expected without in-memory state: {log:?}"
    );
}

/// A controllable [`ObservedAttachmentSource`] for the fallback-drain
/// tests: returns a preset answer and counts how often it was consulted.
struct TestAttachmentSource {
    attachments: ObservedVmAttachments,
    queries: std::sync::Mutex<Vec<String>>,
}

impl TestAttachmentSource {
    fn new(attachments: ObservedVmAttachments) -> Arc<Self> {
        Arc::new(Self {
            attachments,
            queries: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn queried(&self, vm_id: &str) -> usize {
        self.queries
            .lock()
            .expect("queries lock")
            .iter()
            .filter(|vm| vm == &vm_id)
            .count()
    }
}

#[async_trait::async_trait]
impl ObservedAttachmentSource for TestAttachmentSource {
    async fn observed_attachments(&self, vm_id: &str) -> ObservedVmAttachments {
        self.queries
            .lock()
            .expect("queries lock")
            .push(vm_id.to_string());
        self.attachments.clone()
    }
}

/// #405: a delete with NO in-memory runtime record (the VM was created
/// before an agent restart) but cache-observed attachments must still
/// drain stord detach/close and nwd detach from the durable
/// observed-attachment source — this is the live M4.7 run-1 leak (host
/// taps `tap-c4f4aed3`/`tap-027c51ae` survived two HTTP-200 deletes).
#[tokio::test]
async fn delete_after_restart_drains_from_observed_attachments() {
    let h = harness(None);
    let vm_id = "vm-r";
    let create = MutationCommand::CreateVm {
        definition: definition(vm_id, 1, 1),
    };
    h.runtime
        .execute(entry(
            OperationKind::CreateVm,
            vm_id,
            "op-create",
            envelope(create),
        ))
        .await
        .expect("create succeeds");

    // The post-restart world: a FRESH runtime (empty in-memory map; same
    // adapter + controller — the daemons/CH are still up) wired with the
    // durable observed-attachment source, carrying what the NodeCache
    // projection would have recorded for the pre-restart create: volume
    // ids (a handle only where one was durably recorded) and nic ids.
    let source = TestAttachmentSource::new(ObservedVmAttachments {
        volumes: vec![
            ("vol-0".to_string(), None),
            ("vol-1".to_string(), Some("handle-vol-1".to_string())),
        ],
        nics: vec![format!("{vm_id}-net-0")],
    });
    let restarted = Arc::new(
        CloudHypervisorCoreRuntime::new(
            h.adapter.clone(),
            h.controller.clone(),
            h.runtime_dir.clone(),
        )
        .with_observed_attachments(source),
    );
    let delete = MutationCommand::DeleteVm {
        vm_id: VmId::new(vm_id).expect("vm id"),
    };
    let result = restarted
        .execute(entry(
            OperationKind::DeleteVm,
            vm_id,
            "op-del",
            envelope(delete),
        ))
        .await;
    assert!(
        result.is_ok(),
        "delete after restart must succeed: {result:?}"
    );
    assert!(
        !h.adapter.vms.lock().expect("vms lock").contains_key(vm_id),
        "adapter must no longer contain the VM"
    );
    // The fallback drained: every observed volume detached, the one with a
    // live stord session was closed after its detach (its cached handle
    // was gone — the post-restart case), the volume with neither a cached
    // handle nor a live session was NOT closed, and every observed nic was
    // detached (the tap removal).
    let log = calls(&h.controller);
    assert!(
        is_subsequence(
            &log,
            &[
                "detach:vol-0",
                "detach:vol-1",
                "close:vol-1",
                "detach_nic:vm-r-net-0",
            ]
        ),
        "fallback drain must detach volumes, close the handle-bearing one, and detach nics: {log:?}"
    );
    assert!(
        !log.iter().any(|c| c == "close:vol-0"),
        "a volume with no cached handle and no live stord session must not be closed: {log:?}"
    );
    // No last-detach network teardown on the fallback path (residue until
    // network delete / node reprovision, as before the fallback existed).
    assert!(
        !log.iter().any(|c| c.starts_with("net_teardown:")),
        "the fallback must not tear network topology down: {log:?}"
    );
}

/// #711: the post-restart delete's cached handle is gone, but stord's
/// durable session table still has the live attachment for `(volume,
/// vm)`. The fallback drain detaches (clearing `vm_id` and marking the
/// row `detached`), then closes every detached session for the volume —
/// restoring the tracked path's detach-then-close pair instead of
/// leaving a detached row behind.
#[tokio::test]
async fn delete_after_restart_closes_the_detached_session() {
    let h = harness(None);
    let vm_id = "vm-resolve";
    h.runtime
        .execute(entry(
            OperationKind::CreateVm,
            vm_id,
            "op-create",
            envelope(MutationCommand::CreateVm {
                definition: definition(vm_id, 1, 1),
            }),
        ))
        .await
        .expect("create succeeds");

    // The post-restart world: the cache handle died with the process, but
    // stord still reports the session attached to this VM.
    h.controller
        .add_volume_session("vol-0", vm_id, "handle-vol-0");
    let source = TestAttachmentSource::new(ObservedVmAttachments {
        volumes: vec![("vol-0".to_string(), None)],
        nics: vec![format!("{vm_id}-net-0")],
    });
    let restarted = Arc::new(
        CloudHypervisorCoreRuntime::new(
            h.adapter.clone(),
            h.controller.clone(),
            h.runtime_dir.clone(),
        )
        .with_observed_attachments(source),
    );
    let result = restarted
        .execute(entry(
            OperationKind::DeleteVm,
            vm_id,
            "op-del-resolve",
            envelope(MutationCommand::DeleteVm {
                vm_id: VmId::new(vm_id).expect("vm id"),
            }),
        ))
        .await;
    assert!(
        result.is_ok(),
        "delete after restart must succeed: {result:?}"
    );
    let log = calls(&h.controller);
    assert!(
        is_subsequence(
            &log,
            &["detach:vol-0", "close:vol-0", "detach_nic:vm-resolve-net-0",]
        ),
        "the detached session must be closed AFTER the detach: {log:?}"
    );
}

/// #711's error policy: stord's detached-session close is part of the
/// best-effort fallback drain, so a transient failure logs and continues —
/// the detach still runs and the already-deleted VM's op still succeeds
/// (matching every other drain step). The session remains for a retry.
#[tokio::test]
async fn fallback_drain_close_failure_is_best_effort() {
    let h = harness(Some("close_detached:1"));
    let vm_id = "vm-resolve-fail";
    h.runtime
        .execute(entry(
            OperationKind::CreateVm,
            vm_id,
            "op-create",
            envelope(MutationCommand::CreateVm {
                definition: definition(vm_id, 1, 0),
            }),
        ))
        .await
        .expect("create succeeds");

    h.controller
        .add_volume_session("vol-0", vm_id, "handle-vol-0");
    let source = TestAttachmentSource::new(ObservedVmAttachments {
        volumes: vec![("vol-0".to_string(), None)],
        nics: Vec::new(),
    });
    let restarted = Arc::new(
        CloudHypervisorCoreRuntime::new(
            h.adapter.clone(),
            h.controller.clone(),
            h.runtime_dir.clone(),
        )
        .with_observed_attachments(source),
    );
    let result = restarted
        .execute(entry(
            OperationKind::DeleteVm,
            vm_id,
            "op-del-close-fail",
            envelope(MutationCommand::DeleteVm {
                vm_id: VmId::new(vm_id).expect("vm id"),
            }),
        ))
        .await;
    assert!(
        result.is_ok(),
        "a failed detached-session close must not fail the delete: {result:?}"
    );
    let log = calls(&h.controller);
    assert!(
        log.iter().any(|c| c == "detach:vol-0"),
        "the detach must still run before the close attempt: {log:?}"
    );
    assert!(
        !log.iter().any(|c| c == "close:vol-0"),
        "a failed close must leave the session for a retry: {log:?}"
    );
}

/// The tracked (in-memory-record-present) delete path is unchanged by the
/// fallback: the observed-attachment source is never consulted, and the
/// drain comes from the in-memory record exactly as before.
#[tokio::test]
async fn tracked_delete_never_consults_the_fallback_source() {
    // Wire the source into the runtime BEFORE the create, so the delete
    // runs on a runtime whose in-memory map DOES hold the VM's record —
    // a source that would return WRONG attachments if consulted.
    let source = TestAttachmentSource::new(ObservedVmAttachments {
        volumes: vec![("vol-bogus".to_string(), Some("handle-bogus".to_string()))],
        nics: vec!["vm-tracked-net-bogus".to_string()],
    });
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let adapter = Arc::new(MockCloudHypervisorAdapter::default());
    let controller = Arc::new(MockHostResourceController::new());
    let runtime = Arc::new(
        CloudHypervisorCoreRuntime::new(adapter.clone(), controller.clone(), runtime_dir)
            .with_observed_attachments(source.clone()),
    );

    let vm_id = "vm-tracked";
    let create = MutationCommand::CreateVm {
        definition: definition(vm_id, 2, 1),
    };
    runtime
        .execute(entry(
            OperationKind::CreateVm,
            vm_id,
            "op-create",
            envelope(create),
        ))
        .await
        .expect("create succeeds");
    assert_eq!(runtime.debug_side_effects_len(), 1);

    let delete = MutationCommand::DeleteVm {
        vm_id: VmId::new(vm_id).expect("vm id"),
    };
    let result = runtime
        .execute(entry(
            OperationKind::DeleteVm,
            vm_id,
            "op-del",
            envelope(delete),
        ))
        .await;
    assert!(result.is_ok(), "tracked delete must succeed: {result:?}");
    assert_eq!(
        source.queried(vm_id),
        0,
        "the fallback source must not be consulted for a tracked delete"
    );
    assert!(
        !adapter.vms.lock().expect("vms lock").contains_key(vm_id),
        "adapter must no longer contain the VM"
    );
    // The in-memory record drove the drain (reverse-order detach+close of
    // BOTH volumes — a handle-less fallback drain would have closed
    // neither).
    let log = calls(&controller);
    assert!(
        is_subsequence(
            &log,
            &[
                "detach:vol-1",
                "close:vol-1",
                "detach:vol-0",
                "close:vol-0",
                "detach_nic:vm-tracked-net-0",
            ]
        ),
        "tracked drain ordering must be unchanged: {log:?}"
    );
    assert!(
        !log.iter().any(|c| c.contains("bogus")),
        "no fallback attachment may leak into a tracked drain: {log:?}"
    );
    assert_eq!(
        runtime.debug_side_effects_len(),
        0,
        "successful tracked delete must remove the entry"
    );
}

/// #711 review: the tracked delete arm must sweep detached rows too. A
/// close that failed on an earlier attempt leaves `runtime_status =
/// "detached"`; without the sweep that row would survive until the next
/// agent restart (the fallback arm) and trip session-count checks.
#[tokio::test]
async fn tracked_delete_sweeps_leftover_detached_sessions() {
    let h = harness(None);
    let vm_id = "vm-tracked-sweep";
    h.runtime
        .execute(entry(
            OperationKind::CreateVm,
            vm_id,
            "op-create",
            envelope(MutationCommand::CreateVm {
                definition: definition(vm_id, 1, 0),
            }),
        ))
        .await
        .expect("create succeeds");
    // A crash-window orphan from an earlier attempt: `vm_id` cleared, marked
    // detached, and carrying a handle this delete does not close directly.
    h.controller.add_detached_session("vol-0", "handle-orphan");

    let result = h
        .runtime
        .execute(entry(
            OperationKind::DeleteVm,
            vm_id,
            "op-del",
            envelope(MutationCommand::DeleteVm {
                vm_id: VmId::new(vm_id).expect("vm id"),
            }),
        ))
        .await;
    assert!(result.is_ok(), "tracked delete must succeed: {result:?}");
    assert!(
        h.controller
            .volume_sessions
            .lock()
            .expect("sessions lock")
            .is_empty(),
        "the detached row must be swept by the tracked drain: {:?}",
        h.controller.volume_sessions.lock().unwrap()
    );
}

/// The fallback drain is fail-open exactly like the tracked drain: a
/// failing cleanup step (detach, close, or detach_nic) logs and continues,
/// and the delete still succeeds — a missing tap or an unknown volume must
/// never fail an already-deleted VM's op.
#[tokio::test]
async fn fallback_drain_cleanup_failures_do_not_fail_the_delete() {
    for fail_step in ["detach:1", "close:1", "detach_nic:1"] {
        // The fail knob arms a cleanup step (detach/close/detach_nic) that
        // a SUCCESSFUL create never calls, so one shared controller can
        // serve both the pre-restart create and the post-restart delete.
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime_dir = dir.path().join("runtime");
        let adapter = Arc::new(MockCloudHypervisorAdapter::default());
        let controller = Arc::new(MockHostResourceController::new_with_fail(fail_step));
        let vm_id = "vm-fail";
        CloudHypervisorCoreRuntime::new(adapter.clone(), controller.clone(), runtime_dir.clone())
            .execute(entry(
                OperationKind::CreateVm,
                vm_id,
                "op-create",
                envelope(MutationCommand::CreateVm {
                    definition: definition(vm_id, 1, 1),
                }),
            ))
            .await
            .expect("create succeeds");

        let source = TestAttachmentSource::new(ObservedVmAttachments {
            volumes: vec![("vol-0".to_string(), Some("handle-vol-0".to_string()))],
            nics: vec!["vm-fail-net-0".to_string()],
        });
        let restarted = Arc::new(
            CloudHypervisorCoreRuntime::new(adapter.clone(), controller.clone(), runtime_dir)
                .with_observed_attachments(source),
        );
        let delete = MutationCommand::DeleteVm {
            vm_id: VmId::new(vm_id).expect("vm id"),
        };
        let result = restarted
            .execute(entry(
                OperationKind::DeleteVm,
                vm_id,
                "op-del-fallback-fail",
                envelope(delete),
            ))
            .await;
        assert!(
            result.is_ok(),
            "a failing fallback cleanup step ({fail_step}) must not fail the delete: {result:?}"
        );
        // The drain continued past the injected failure: the OTHER cleanup
        // steps still ran (each case fails exactly one step).
        let log = calls(&controller);
        if fail_step != "detach:1" {
            assert!(
                log.iter().any(|c| c == "detach:vol-0"),
                "the volume detach must still run ({fail_step}): {log:?}"
            );
        }
        if fail_step != "close:1" {
            assert!(
                log.iter().any(|c| c == "close:vol-0"),
                "the volume close must still run ({fail_step}): {log:?}"
            );
        }
        if fail_step != "detach_nic:1" {
            assert!(
                log.iter().any(|c| c == "detach_nic:vm-fail-net-0"),
                "the nic detach must still run ({fail_step}): {log:?}"
            );
        }
    }
}

/// The fail-open default: with NO observed-attachment source wired (the
/// pre-#405 composition, and today's core-native mode), a post-restart
/// delete still succeeds and drains nothing — pinned by
/// `create_then_restart_delete_without_state` above; this test pins the
/// same for the empty-source case (a cache that observed nothing).
#[tokio::test]
async fn delete_after_restart_with_empty_observed_attachments_drains_nothing() {
    let h = harness(None);
    let vm_id = "vm-empty";
    let create = MutationCommand::CreateVm {
        definition: definition(vm_id, 1, 1),
    };
    h.runtime
        .execute(entry(
            OperationKind::CreateVm,
            vm_id,
            "op-create",
            envelope(create),
        ))
        .await
        .expect("create succeeds");

    let source = TestAttachmentSource::new(ObservedVmAttachments::default());
    let restarted = Arc::new(
        CloudHypervisorCoreRuntime::new(
            h.adapter.clone(),
            h.controller.clone(),
            h.runtime_dir.clone(),
        )
        .with_observed_attachments(source),
    );
    let delete = MutationCommand::DeleteVm {
        vm_id: VmId::new(vm_id).expect("vm id"),
    };
    let result = restarted
        .execute(entry(
            OperationKind::DeleteVm,
            vm_id,
            "op-del",
            envelope(delete),
        ))
        .await;
    assert!(
        result.is_ok(),
        "delete with an empty observed source must succeed: {result:?}"
    );
    let log = calls(&h.controller);
    assert!(
        !log.iter()
            .any(|c| c.starts_with("detach:") || c.starts_with("close:")),
        "nothing to drain when the source observed nothing: {log:?}"
    );
}

#[tokio::test]
async fn delete_after_force_stop_is_idempotent() {
    // Run 8d of the M2.5 qualification: the force-stop path removes the
    // adapter's map entry (pre-existing force-stop semantics), so a delete
    // issued afterwards finds no runtime entry and the adapter reports
    // NotFound. The VM exists at the authority and its runtime dir is on
    // disk — the delete must complete the adapter-owned artifact cleanup
    // itself and succeed, not fail NOT_FOUND on every retry.
    let h = harness(None);
    let vm_id = "vm-force";

    // The force-stopped state: no adapter entry (the force path removed it),
    // but the runtime dir with the adapter's artifacts and the
    // storage-layer volume backing.
    let vm_dir = vm_dir_path(&h, vm_id);
    std::fs::create_dir_all(&vm_dir).expect("vm dir");
    std::fs::write(vm_pid_file(&vm_dir), "12345").expect("pid file");
    std::fs::write(vm_config_file(&vm_dir), "{}").expect("persisted config");
    std::fs::write(vm_api_socket(&vm_dir), b"").expect("api socket file");
    std::fs::write(vm_dir.join("vol-0.img"), b"disk").expect("volume backing");

    let delete = MutationCommand::DeleteVm {
        vm_id: VmId::new(vm_id).expect("vm id"),
    };
    let result = h
        .runtime
        .execute(entry(
            OperationKind::DeleteVm,
            vm_id,
            "op-del-force",
            envelope(delete),
        ))
        .await;
    assert!(
        result.is_ok(),
        "delete after force stop must succeed: {result:?}"
    );
    assert!(!vm_pid_file(&vm_dir).exists(), "pid file must be removed");
    assert!(
        !vm_config_file(&vm_dir).exists(),
        "persisted config must be removed"
    );
    assert!(
        !vm_api_socket(&vm_dir).exists(),
        "api socket must be removed"
    );
    assert!(
        vm_dir.join("vol-0.img").exists(),
        "storage-layer files must survive the runtime delete"
    );
}

#[tokio::test]
async fn delete_without_runtime_dir_stays_not_found() {
    // No adapter entry AND no runtime dir: the VM never ran on this node.
    // NotFound must surface — a delete misrouted to the wrong node must
    // not be silently swallowed as an idempotent success.
    let h = harness(None);
    let vm_id = "vm-never";

    let delete = MutationCommand::DeleteVm {
        vm_id: VmId::new(vm_id).expect("vm id"),
    };
    let result = h
        .runtime
        .execute(entry(
            OperationKind::DeleteVm,
            vm_id,
            "op-del-never",
            envelope(delete),
        ))
        .await;
    assert!(
        matches!(result, Err(RuntimeFailure::NotFound)),
        "delete without any runtime footprint must stay NotFound, got {result:?}"
    );
}

#[tokio::test]
async fn start_stop_reboot_passthrough() {
    let h = harness(None);
    let vm_id = "vm-e";

    let start = MutationCommand::StartVm {
        vm_id: VmId::new(vm_id).expect("vm id"),
    };
    h.runtime
        .execute(entry(
            OperationKind::StartVm,
            vm_id,
            "op-start",
            envelope(start),
        ))
        .await
        .expect("start succeeds");

    let stop = MutationCommand::StopVm {
        vm_id: VmId::new(vm_id).expect("vm id"),
    };
    h.runtime
        .execute(entry(
            OperationKind::StopVm,
            vm_id,
            "op-stop",
            envelope(stop),
        ))
        .await
        .expect("stop succeeds");

    let reboot = MutationCommand::RebootVm {
        vm_id: VmId::new(vm_id).expect("vm id"),
    };
    h.runtime
        .execute(entry(
            OperationKind::RebootVm,
            vm_id,
            "op-reboot",
            envelope(reboot),
        ))
        .await
        .expect("reboot succeeds");
}

#[tokio::test]
async fn update_attach_detach_are_unsupported() {
    let h = harness(None);
    let vm_id = "vm-f";
    let vm = VmId::new(vm_id).expect("vm id");
    let cases: Vec<(OperationKind, MutationCommand)> = vec![
        (
            OperationKind::UpdateVm,
            MutationCommand::UpdateVm {
                definition: definition(vm_id, 0, 0),
            },
        ),
        (
            OperationKind::AttachVolume,
            MutationCommand::AttachVolume {
                vm_id: vm.clone(),
                attachment: StorageAttachmentRef {
                    attachment_id: "vol-x".to_string(),
                    storage_ref: "vol-x".to_string(),
                    read_only: false,
                    size_bytes: None,
                    seed_from: None,
                    backend_class: None,
                },
            },
        ),
        (
            OperationKind::DetachVolume,
            MutationCommand::DetachVolume {
                vm_id: vm.clone(),
                attachment_id: "vol-x".to_string(),
            },
        ),
        (
            OperationKind::AttachNetwork,
            MutationCommand::AttachNetwork {
                vm_id: vm.clone(),
                attachment: NetworkAttachmentRef {
                    attachment_id: format!("{vm_id}-netx"),
                    network_ref: "net-x".to_string(),
                    mac_address: None,
                    addressing: None,
                    firewall_policy_json: None,
                },
            },
        ),
        (
            OperationKind::DetachNetwork,
            MutationCommand::DetachNetwork {
                vm_id: vm.clone(),
                attachment_id: format!("{vm_id}-netx"),
            },
        ),
    ];
    for (op_index, (kind, command)) in cases.into_iter().enumerate() {
        let result = h
            .runtime
            .execute(entry(
                kind,
                vm_id,
                &format!("op-{}-{}", kind_name(kind), op_index),
                envelope(command),
            ))
            .await;
        assert!(
            matches!(result, Err(RuntimeFailure::Unsupported)),
            "{kind:?} must be Unsupported, got {result:?}"
        );
    }
}

fn kind_name(kind: OperationKind) -> &'static str {
    match kind {
        OperationKind::UpdateVm => "update",
        OperationKind::AttachVolume => "attach-volume",
        OperationKind::DetachVolume => "detach-volume",
        OperationKind::AttachNetwork => "attach-network",
        OperationKind::DetachNetwork => "detach-network",
        _ => "other",
    }
}

#[tokio::test]
async fn malformed_envelope_is_invalid_request() {
    let h = harness(None);
    let request = serde_json::json!({"not_a_command": true, "unexpected": "shape"});
    let result = h
        .runtime
        .execute(entry(
            OperationKind::CreateVm,
            "vm-g",
            "op-malformed",
            request,
        ))
        .await;
    assert!(
        matches!(result, Err(RuntimeFailure::InvalidRequest)),
        "malformed request must be InvalidRequest, got {result:?}"
    );
}

#[tokio::test]
async fn create_vm_kind_with_non_create_command_is_invalid() {
    let h = harness(None);
    // A CreateVm-kind operation whose envelope carries a DeleteVm command is a
    // journal-integrity failure, not a user mistake.
    let command = MutationCommand::DeleteVm {
        vm_id: VmId::new("vm-x").expect("vm id"),
    };
    let result = h
        .runtime
        .execute(entry(
            OperationKind::CreateVm,
            "vm-kind",
            "op-kind",
            envelope(command),
        ))
        .await;
    assert!(
        matches!(result, Err(RuntimeFailure::InvalidRequest)),
        "CreateVm-kind with a non-create command must be InvalidRequest, got {result:?}"
    );
    assert!(
        calls(&h.controller).is_empty(),
        "no side effects may run for a mismatched command"
    );
}

#[tokio::test]
async fn create_unwind_never_attached_volume_is_close_only() {
    // Fail attaching the 2nd volume. Volume 1 is opened+attached and must be
    // detached+closed; volume 2 is opened but NEVER attached, so its unwind
    // must be close-only (HostResourceController contract: a never-attached
    // volume is closed without a detach).
    let h = harness(Some("attach:2"));
    let vm_id = "vm-closeonly";
    let command = MutationCommand::CreateVm {
        definition: definition(vm_id, 2, 0),
    };
    let result = h
        .runtime
        .execute(entry(
            OperationKind::CreateVm,
            vm_id,
            "op-closeonly",
            envelope(command),
        ))
        .await;
    assert!(result.is_err(), "create must fail: {result:?}");

    let log = calls(&h.controller);
    // First volume attached: detach+close.
    assert!(
        is_subsequence(
            &log,
            &["open:vol-0", "attach:vol-0", "detach:vol-0", "close:vol-0"]
        ),
        "attached volume must detach+close: {log:?}"
    );
    // Second volume's attach never happened -> close only, NEVER detach.
    assert!(
        log.iter().any(|c| c == "close:vol-1"),
        "never-attached volume must still be closed: {log:?}"
    );
    assert!(
        !log.iter().any(|c| c == "detach:vol-1"),
        "never-attached volume must NOT be detached (contract violation): {log:?}"
    );
    assert!(
        !log.iter().any(|c| c == "attach:vol-1"),
        "second attach must not have succeeded: {log:?}"
    );
    assert!(
        !h.adapter.vms.lock().expect("vms lock").contains_key(vm_id),
        "adapter must not contain the failed VM"
    );
    assert!(
        !vm_dir_path(&h, vm_id).exists(),
        "vm dir should be removed after failed create"
    );
}

#[tokio::test]
async fn delete_drains_side_effects_when_adapter_delete_fails() {
    let h = harness(None);
    let vm_id = "vm-drainfail";
    let create = MutationCommand::CreateVm {
        definition: definition(vm_id, 2, 1),
    };
    h.runtime
        .execute(entry(
            OperationKind::CreateVm,
            vm_id,
            "op-create",
            envelope(create),
        ))
        .await
        .expect("create succeeds");
    assert_eq!(
        h.runtime.debug_side_effects_len(),
        1,
        "create tracks one entry"
    );

    // Make the hypervisor delete fail deterministically.
    *h.adapter.fail_delete.lock().unwrap() = true;
    let delete = MutationCommand::DeleteVm {
        vm_id: VmId::new(vm_id).expect("vm id"),
    };
    let result = h
        .runtime
        .execute(entry(
            OperationKind::DeleteVm,
            vm_id,
            "op-del-fail",
            envelope(delete.clone()),
        ))
        .await;
    assert!(
        matches!(result, Err(RuntimeFailure::Internal)),
        "failed hypervisor delete must surface the effect failure (mapped from ChvError::Internal via the fail_delete knob), got {result:?}"
    );
    // The VM was NOT removed by the mock (delete failed).
    assert!(
        h.adapter.vms.lock().expect("vms lock").contains_key(vm_id),
        "adapter VM must survive a failed delete for retry"
    );

    // Drain ran DESPITE the delete failure: every volume detach+close, every
    // nic detached, and the tracked entry is kept for a later retry.
    let log = calls(&h.controller);
    assert!(
        is_subsequence(
            &log,
            &[
                "detach:vol-1",
                "close:vol-1",
                "detach:vol-0",
                "close:vol-0",
                "detach_nic:vm-drainfail-net-0",
            ]
        ),
        "failed delete must still drain side effects: {log:?}"
    );
    assert_eq!(
        h.runtime.debug_side_effects_len(),
        1,
        "tracked entry must survive a failed delete for retry"
    );

    // A SECOND delete (failure cleared) succeeds and finishes the drain.
    *h.adapter.fail_delete.lock().unwrap() = false;
    let result = h
        .runtime
        .execute(entry(
            OperationKind::DeleteVm,
            vm_id,
            "op-del-retry",
            envelope(delete),
        ))
        .await;
    assert!(result.is_ok(), "retry delete must succeed: {result:?}");
    assert!(
        !h.adapter.vms.lock().expect("vms lock").contains_key(vm_id),
        "adapter VM must be gone after the retry delete"
    );
    assert_eq!(
        h.runtime.debug_side_effects_len(),
        0,
        "no entry may remain after a successful delete"
    );
    let log = calls(&h.controller);
    let detaches: Vec<&str> = log
        .iter()
        .filter_map(|c| c.strip_prefix("detach:"))
        .collect();
    assert!(
        detaches.iter().filter(|v| v == &&"vol-0").count() >= 2
            && detaches.iter().filter(|v| v == &&"vol-1").count() >= 2,
        "retry delete must drain the retained entry again: {log:?}"
    );
}

/// A controllable [`NetworkUsageLookup`] for the last-detach tests: the
/// test's model of the durable Core store's live VM list, as
/// `(vm_id, network_id)` pairs.
struct TestNetworkUsage {
    vms: std::sync::Mutex<Vec<(String, String)>>,
}

impl TestNetworkUsage {
    fn new(vms: &[(&str, &str)]) -> Arc<Self> {
        Arc::new(Self {
            vms: std::sync::Mutex::new(
                vms.iter()
                    .map(|(vm, net)| ((*vm).to_owned(), (*net).to_owned()))
                    .collect(),
            ),
        })
    }

    /// Model the store's accept-time tombstone: a deleted VM no longer
    /// protects its networks.
    fn tombstone(&self, vm_id: &str) {
        self.vms
            .lock()
            .expect("vms lock")
            .retain(|(vm, _)| vm != vm_id);
    }
}

impl chv_hypervisor_api::NetworkUsageLookup for TestNetworkUsage {
    fn network_in_use(&self, network_id: &str, excluding_vm: &str) -> bool {
        self.vms
            .lock()
            .expect("vms lock")
            .iter()
            .any(|(vm, net)| vm != excluding_vm && net == network_id)
    }
}

fn harness_with_usage(usage: Arc<dyn chv_hypervisor_api::NetworkUsageLookup>) -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let adapter = Arc::new(MockCloudHypervisorAdapter::default());
    let controller = Arc::new(MockHostResourceController::new());
    let runtime = Arc::new(
        CloudHypervisorCoreRuntime::new(adapter.clone(), controller.clone(), runtime_dir.clone())
            .with_network_usage(usage),
    );
    Harness {
        _dir: dir,
        runtime_dir,
        adapter,
        controller,
        runtime,
    }
}

async fn create_for_delete_tests(h: &Harness, vm_id: &str, op_id: &str) {
    h.runtime
        .execute(entry(
            OperationKind::CreateVm,
            vm_id,
            op_id,
            envelope(MutationCommand::CreateVm {
                definition: definition(vm_id, 1, 1),
            }),
        ))
        .await
        .expect("create succeeds");
}

async fn delete_for_delete_tests(h: &Harness, vm_id: &str, op_id: &str) {
    h.runtime
        .execute(entry(
            OperationKind::DeleteVm,
            vm_id,
            op_id,
            envelope(MutationCommand::DeleteVm {
                vm_id: VmId::new(vm_id).expect("vm id"),
            }),
        ))
        .await
        .expect("delete succeeds");
}

#[tokio::test]
async fn delete_vm_tears_down_network_topology_on_last_detach() {
    // #356 N5: a successful delete of the node's LAST VM on a network
    // tears the network's host topology down via nwd. The decision comes
    // from the DURABLE usage authority — while another VM still uses the
    // network, no teardown fires; once the last user is gone (and its
    // delete tombstoned it in the authority), the teardown fires exactly
    // once for the network.
    let usage = TestNetworkUsage::new(&[("vm-a", "net-0"), ("vm-b", "net-0")]);
    let h = harness_with_usage(usage.clone());
    create_for_delete_tests(&h, "vm-a", "op-create-a").await;
    create_for_delete_tests(&h, "vm-b", "op-create-b").await;

    // vm-b still uses net-0: deleting vm-a must NOT tear it down.
    delete_for_delete_tests(&h, "vm-a", "op-del-a").await;
    assert!(
        !calls(&h.controller)
            .iter()
            .any(|c| c.starts_with("net_teardown:")),
        "a network another VM still uses must not be torn down: {:?}",
        calls(&h.controller)
    );

    // Model the authority: vm-a's accepted delete tombstoned it. Now
    // vm-b is the last user — deleting it must tear net-0 down.
    usage.tombstone("vm-a");
    delete_for_delete_tests(&h, "vm-b", "op-del-b").await;
    let teardowns: Vec<String> = calls(&h.controller)
        .into_iter()
        .filter(|c| c.starts_with("net_teardown:"))
        .collect();
    assert_eq!(
        teardowns,
        vec!["net_teardown:net-0".to_string()],
        "the last user's delete must tear the network down exactly once"
    );
}

#[tokio::test]
async fn delete_vm_without_a_usage_authority_never_tears_down_networks() {
    // The fail-closed default: without `with_network_usage`, no teardown
    // ever fires — host residue is cleaned by network delete / node
    // reprovision instead of risking an outage on a wrong decision.
    let h = harness(None);
    create_for_delete_tests(&h, "vm-only", "op-create").await;
    delete_for_delete_tests(&h, "vm-only", "op-del").await;
    assert!(
        !calls(&h.controller)
            .iter()
            .any(|c| c.starts_with("net_teardown:")),
        "the default (fail-closed) lookup must never authorize a teardown: {:?}",
        calls(&h.controller)
    );
}

#[tokio::test]
async fn failed_delete_never_tears_down_network_topology() {
    // The store tombstones a VM when its DeleteVm is ACCEPTED — before the
    // effector runs — so the usage authority alone cannot distinguish a
    // deleted VM from one whose delete FAILED with its VMM still running
    // and its taps still enslaved. The runtime must gate the teardown on
    // the delete's SUCCESS: a failed delete keeps the entry for retry and
    // must not tear the network down even when the authority says the
    // network is otherwise unused.
    let usage = TestNetworkUsage::new(&[("vm-fail", "net-0")]);
    let h = harness_with_usage(usage.clone());
    create_for_delete_tests(&h, "vm-fail", "op-create").await;
    *h.adapter.fail_delete.lock().unwrap() = true;
    let result = h
        .runtime
        .execute(entry(
            OperationKind::DeleteVm,
            "vm-fail",
            "op-del-fail",
            envelope(MutationCommand::DeleteVm {
                vm_id: VmId::new("vm-fail").expect("vm id"),
            }),
        ))
        .await;
    assert!(result.is_err(), "the delete must fail: {result:?}");
    // Even with the authority reporting the network as unused (the
    // tombstone landed at accept), no teardown may fire.
    usage.tombstone("vm-fail");
    assert!(
        !calls(&h.controller)
            .iter()
            .any(|c| c.starts_with("net_teardown:")),
        "a failed delete must not tear the network down: {:?}",
        calls(&h.controller)
    );
}

#[tokio::test]
async fn sibling_failed_delete_vetoes_last_detach_teardown() {
    // The store tombstones a VM at DeleteVm ACCEPT time, so a sibling whose
    // delete then FAILED (retry pending, VMM still running, taps still
    // enslaved on the shared bridge) is invisible to the durable usage
    // authority. Its retained side-effects entry must VETO this VM's
    // teardown — otherwise a successful delete here would tear the bridge
    // out from under the failed-delete VM's live VMM.
    let usage = TestNetworkUsage::new(&[("vm-a", "net-0"), ("vm-b", "net-0")]);
    let h = harness_with_usage(usage.clone());
    create_for_delete_tests(&h, "vm-a", "op-create-a").await;
    create_for_delete_tests(&h, "vm-b", "op-create-b").await;

    // vm-a's delete is accepted (the authority tombstones it) but the
    // effector FAILS: the runtime retains vm-a's entry for the retry.
    usage.tombstone("vm-a");
    *h.adapter.fail_delete.lock().unwrap() = true;
    let result = h
        .runtime
        .execute(entry(
            OperationKind::DeleteVm,
            "vm-a",
            "op-del-a-fail",
            envelope(MutationCommand::DeleteVm {
                vm_id: VmId::new("vm-a").expect("vm id"),
            }),
        ))
        .await;
    assert!(result.is_err(), "vm-a's delete must fail: {result:?}");
    *h.adapter.fail_delete.lock().unwrap() = false;

    // vm-b deletes successfully. The authority now sees nobody on net-0
    // (both tombstoned) — the in-session veto is the only guard left.
    usage.tombstone("vm-b");
    delete_for_delete_tests(&h, "vm-b", "op-del-b").await;
    assert!(
        !calls(&h.controller)
            .iter()
            .any(|c| c.starts_with("net_teardown:")),
        "a sibling failed-delete's retained entry must veto the teardown: {:?}",
        calls(&h.controller)
    );

    // Control: once the failed delete is retried successfully (its entry
    // drains away), a subsequent last user WOULD tear down. Delete vm-a
    // again successfully — net-0 is now truly unused.
    delete_for_delete_tests(&h, "vm-a", "op-del-a-retry").await;
    let teardowns: Vec<String> = calls(&h.controller)
        .into_iter()
        .filter(|c| c.starts_with("net_teardown:"))
        .collect();
    assert_eq!(
        teardowns,
        vec!["net_teardown:net-0".to_string()],
        "after the failed delete's successful retry, the teardown must fire"
    );
}

#[tokio::test]
async fn delete_removes_side_effects_entry_after_success() {
    let h = harness(None);
    let vm_id = "vm-entries";
    let create = MutationCommand::CreateVm {
        definition: definition(vm_id, 1, 1),
    };
    h.runtime
        .execute(entry(
            OperationKind::CreateVm,
            vm_id,
            "op-create",
            envelope(create),
        ))
        .await
        .expect("create succeeds");
    assert_eq!(h.runtime.debug_side_effects_len(), 1);

    let delete = MutationCommand::DeleteVm {
        vm_id: VmId::new(vm_id).expect("vm id"),
    };
    let result = h
        .runtime
        .execute(entry(
            OperationKind::DeleteVm,
            vm_id,
            "op-del",
            envelope(delete),
        ))
        .await;
    assert!(result.is_ok(), "delete must succeed: {result:?}");
    assert_eq!(
        h.runtime.debug_side_effects_len(),
        0,
        "successful delete must remove the tracked entry"
    );
}

/// Build a CreateVm envelope whose `request` carries the given raw definition
/// JSON for public-path tests that verify end-to-end rejection of path-unsafe
/// ids (no side effects, no fs mutation). Layer A (`VmDefinition` serde
/// try_from → validate) already rejects these inputs at deserialization, so the
/// Layer-B `is_safe_resource_id`/`verify_vm_dir_within_base` guards are
/// defense-in-depth, covered only by the dedicated in-crate unit tests.
fn create_envelope_with_raw_definition(definition: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "command": {
            "command": "create_vm",
            "definition": definition,
        },
        "expected_vm_version": 1,
    })
}

fn raw_definition(id: &str, storage: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "name": "evil",
        "boot": {"kernel": "/kernel", "firmware": null, "initial_disk": null},
        "compute": {"vcpus": 2, "memory_bytes": 1024},
        "storage": storage,
        "networks": [],
        "requested_power_state": "stopped",
        "observed_power_state": "unknown",
        "resource_version": 1,
    })
}

#[tokio::test]
async fn create_rejects_path_unsafe_vm_id() {
    let h = harness(None);
    let request = create_envelope_with_raw_definition(raw_definition(
        "../../../etc/evil",
        serde_json::json!([]),
    ));
    let result = h
        .runtime
        .execute(entry(
            OperationKind::CreateVm,
            "vm-safe-op",
            "op-evil-id",
            request,
        ))
        .await;
    assert!(
        matches!(result, Err(RuntimeFailure::InvalidRequest)),
        "path-unsafe vm id must be InvalidRequest, got {result:?}"
    );
    assert!(
        calls(&h.controller).is_empty(),
        "no side effects may run for a path-unsafe vm id"
    );
    // No fs mutation: the runtime never created `{runtime_dir}/vms`.
    assert!(
        !h.runtime_dir.join("vms").exists(),
        "no vms tree may be created for a path-unsafe vm id"
    );
}

#[tokio::test]
async fn create_rejects_path_unsafe_storage_ref() {
    let h = harness(None);
    let request = create_envelope_with_raw_definition(raw_definition(
        "vm-safe",
        serde_json::json!([{
            "attachment_id": "disk-0",
            "storage_ref": "../vol-0",
            "read_only": false,
        }]),
    ));
    let result = h
        .runtime
        .execute(entry(
            OperationKind::CreateVm,
            "vm-safe",
            "op-evil-storage",
            request,
        ))
        .await;
    assert!(
        matches!(result, Err(RuntimeFailure::InvalidRequest)),
        "path-unsafe storage_ref must be InvalidRequest, got {result:?}"
    );
    assert!(
        calls(&h.controller).is_empty(),
        "no side effects may run for a path-unsafe storage_ref"
    );
    assert!(
        !h.runtime_dir.join("vms").exists(),
        "no vms tree may be created for a path-unsafe storage_ref"
    );
}
