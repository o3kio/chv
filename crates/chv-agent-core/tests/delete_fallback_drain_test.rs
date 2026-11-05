//! End-to-end pin for the #405 fix at the composition layer: a VM created
//! through the real `CloudHypervisorCoreRuntime`, then deleted by a FRESH
//! runtime (an agent restart emptied the in-memory side-effect map) whose
//! delete drains from `NodeCacheAttachmentSource` — the production
//! observed-attachment source wired in `cmd/chv-agent` core-managed mode.
//!
//! This is the exact live failure from M4.7 run 1: both deletes returned
//! HTTP 200 while the host taps (`tap-c4f4aed3`, `tap-027c51ae`) and the
//! stord sessions survived, because the legacy-core delete path had no
//! fallback side-effect drain. The cache population below uses
//! `NodeCache::project_vm` — the same function the live projection (after
//! a create) and the startup rebuild (`rebuild_from_core`, before the
//! executor starts on every restart) both use — so the nic ids and volume
//! ids asserted here are the ones production derives.

use cellhv_core_executor::CoreVmRuntime;
use cellhv_core_operations::{MutationCommand, OperationJournalEntry};
use cellhv_core_types::{
    BootSpec, ComputeSpec, NetworkAttachmentRef, ObservedPowerState, Operation, OperationId,
    OperationKind, OperationStatus, RequestedPowerState, ResourceVersion, StorageAttachmentRef,
    VmDefinition, VmId,
};
use chv_agent_core::cache::NodeCache;
use chv_agent_core::resources::NodeCacheAttachmentSource;
use chv_agent_runtime_ch::core_runtime::CloudHypervisorCoreRuntime;
use chv_agent_runtime_ch::mock::{MockCloudHypervisorAdapter, MockHostResourceController};
use std::sync::Arc;
use tempfile::TempDir;

fn definition(vm_id: &str) -> VmDefinition {
    VmDefinition {
        id: VmId::new(vm_id).expect("vm id"),
        name: format!("{vm_id}-guest"),
        boot: BootSpec::new("/kernel").expect("boot"),
        compute: ComputeSpec::new(2, 1024).expect("compute"),
        storage: vec![
            StorageAttachmentRef {
                attachment_id: format!("{vm_id}-vol-0"),
                storage_ref: "vol-0".to_string(),
                read_only: false,
                size_bytes: None,
                seed_from: None,
                backend_class: None,
            },
            StorageAttachmentRef {
                attachment_id: format!("{vm_id}-vol-1"),
                storage_ref: "vol-1".to_string(),
                read_only: false,
                size_bytes: None,
                seed_from: None,
                backend_class: None,
            },
        ],
        networks: vec![NetworkAttachmentRef {
            attachment_id: format!("{vm_id}-net-0"),
            network_ref: "net-0".to_string(),
            mac_address: Some("02:00:00:00:00:01".to_string()),
            addressing: None,
            firewall_policy_json: None,
        }],
        requested_power_state: RequestedPowerState::Stopped,
        observed_power_state: ObservedPowerState::Unknown,
        resource_version: ResourceVersion::new(1).expect("version"),
        cloud_init_userdata: None,
        hypervisor_tuning: None,
        vsock: None,
    }
}

fn envelope(command: MutationCommand) -> serde_json::Value {
    serde_json::json!({
        "command": command,
        "expected_vm_version": 1,
    })
}

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

/// The #405 live sequence: create → (agent restart: fresh runtime, cache
/// rebuilt from the Core store) → delete. The delete must drain the
/// pre-restart VM's stord detach and nwd detach from the cache-observed
/// attachments instead of leaking them behind a 200.
#[tokio::test]
async fn restarted_delete_drains_pre_restart_vm_from_the_node_cache() {
    let dir = TempDir::new().expect("tempdir");
    let runtime_dir = dir.path().join("runtime");
    let adapter = Arc::new(MockCloudHypervisorAdapter::default());
    let controller = Arc::new(MockHostResourceController::new());
    let vm_id = "vm-pre-restart";

    // 1. The pre-restart agent creates the VM through the Core runtime.
    let creating = Arc::new(CloudHypervisorCoreRuntime::new(
        adapter.clone(),
        controller.clone(),
        runtime_dir.clone(),
    ));
    creating
        .execute(entry(
            OperationKind::CreateVm,
            vm_id,
            "op-create",
            envelope(MutationCommand::CreateVm {
                definition: definition(vm_id),
            }),
        ))
        .await
        .expect("create succeeds");

    // 2. The restart: the cache carries the VM axis the startup rebuild
    //    re-seeds from the Core store (`rebuild_from_core` → `project_vm`),
    //    and a FRESH runtime starts with an empty in-memory map, wired
    //    with the NodeCache observed-attachment source exactly as
    //    `cmd/chv-agent` core-managed mode wires it.
    let cache = Arc::new(tokio::sync::Mutex::new(NodeCache::new("node-1")));
    {
        let mut cache = cache.lock().await;
        cache.rebuild_from_core(&[definition(vm_id)]);
    }
    let restarted = Arc::new(
        CloudHypervisorCoreRuntime::new(adapter.clone(), controller.clone(), runtime_dir)
            .with_observed_attachments(Arc::new(NodeCacheAttachmentSource::new(cache))),
    );

    // 3. stord still holds the pre-restart VM's live session for vol-0,
    //    while vol-1 has no session (already closed/never attached). The
    //    cached handle died with the agent process, so only stord's
    //    durable session table can supply vol-0's handle (#711).
    controller.add_volume_session("vol-0", vm_id, "handle-vol-0");

    // 4. The post-restart delete: must succeed AND drain.
    restarted
        .execute(entry(
            OperationKind::DeleteVm,
            vm_id,
            "op-delete",
            envelope(MutationCommand::DeleteVm {
                vm_id: VmId::new(vm_id).expect("vm id"),
            }),
        ))
        .await
        .expect("delete after restart must succeed");

    let log = controller.calls.lock().expect("calls lock").clone();
    // The fallback drained the pre-restart VM's volumes and NIC, with the
    // nic ids the cache projection derived (the same `{vm}-{network}`
    // ids the create path used).
    for expected in [
        "detach:vol-0",
        "detach:vol-1",
        "detach_nic:vm-pre-restart-net-0",
    ] {
        assert!(
            log.iter().any(|c| c == expected),
            "fallback drain must issue {expected}: {log:?}"
        );
    }
    // vol-0's session is found from stord's durable table after the detach
    // cleared `vm_id`, so it is closed exactly like the live path (#711).
    // vol-1 has no live session, so it is detached only.
    assert!(
        is_subsequence(&log, &["detach:vol-0", "close:vol-0"]),
        "the detached session must be closed after the detach: {log:?}"
    );
    assert!(
        !log.iter().any(|c| c == "close:vol-1"),
        "a volume with no live stord session must not be closed: {log:?}"
    );
}

/// Ordering helper for the #711 close-after-detach pin.
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
