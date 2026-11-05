use async_trait::async_trait;
use chv_errors::ChvError;
use chv_hypervisor_api::resources::HostResourceController;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::adapter::{
    AddDiskParams, AddNetParams, CloudHypervisorAdapter, VmConfig, VmCounters, VmInfo,
};

/// Options passed to one `open_volume` call: the volume id, the backend
/// class the runtime threaded into the open, and the option map.
pub type RecordedOpenOptions = (String, String, HashMap<String, String>);

/// One live stord session the mock controller knows about, used by
/// [`HostResourceController::resolve_volume_handle`] (#711).
#[derive(Debug, Clone)]
pub struct MockVolumeSession {
    pub volume_id: String,
    pub vm_id: String,
    pub attachment_handle: String,
}

/// Deterministic host-resource controller for tests.
///
/// Records every controller call as a canonical log line (for example
/// `open:<volume_id>`, `attach:<volume_id>`, `ensure:<network_id>`,
/// `attach_nic:<nic_id>`, `detach:<volume_id>`, `close:<volume_id>`,
/// `detach_nic:<nic_id>`) and can inject a single failure at a named step
/// via [`MockHostResourceController::fail_next`], keyed as `"<step>:<ordinal>"`
/// — `"open:2"` fails when opening the 2nd volume, `"attach_nic:1"` fails
/// the 1st NIC attach, and so on (the delete-drain steps `detach`, `close`
/// and `detach_nic` are injectable too, for fail-open drain tests). This
/// supports asserting exact side-effect sequencing (create open/attach/
/// ensure ordering, create-unwind close/detach pairing, delete drain
/// ordering) without real daemons.
///
/// Exported `pub` (not `cfg(test)`) so OTHER crates' integration tests can
/// drive the Core runtime, mirroring `MockCloudHypervisorAdapter`.
#[derive(Debug, Default)]
pub struct MockHostResourceController {
    /// Canonical log of controller invocations in call order.
    pub calls: Arc<Mutex<Vec<String>>>,
    /// `Some("step:ordinal")` fails exactly one upcoming step then clears.
    pub fail_next: Arc<Mutex<Option<String>>>,
    /// Options passed to each `open_volume` call, in call order, so tests
    /// can assert provisioning hints (size/seed) and the threaded backend
    /// class reached the storage layer.
    pub open_options: Arc<Mutex<Vec<RecordedOpenOptions>>>,
    /// Locator argument of each `open_volume` call as `(volume_id,
    /// locator)`, in call order, so tests can pin the #379 DP5
    /// class-dependent locator shaping (the LVM dm-path token vs the
    /// historical `{volume_id}.img` under the VM dir).
    pub open_locators: Arc<Mutex<Vec<(String, String)>>>,
    /// Live stord sessions the controller can resolve (#711). Empty by
    /// default; tests add sessions explicitly to model a restart where the
    /// cache handle died but stord's durable session row survived.
    pub volume_sessions: Arc<Mutex<Vec<MockVolumeSession>>>,
}

impl MockHostResourceController {
    pub fn new() -> Self {
        Self::default()
    }

    /// Inject a failure at the named step (e.g. `"open:2"`).
    pub fn new_with_fail(fail_on: &str) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            fail_next: Arc::new(Mutex::new(Some(fail_on.to_string()))),
            open_options: Arc::new(Mutex::new(Vec::new())),
            open_locators: Arc::new(Mutex::new(Vec::new())),
            volume_sessions: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Register a live stord session for `(volume_id, vm_id)`, so a
    /// handle-less fallback drain can resolve it exactly like stord's
    /// durable session table does after an agent restart (#711). A
    /// session is consumed by the mock detach (stord clears `vm_id`), so
    /// a retry after a successful detach resolves nothing.
    pub fn add_volume_session(&self, volume_id: &str, vm_id: &str, attachment_handle: &str) {
        self.volume_sessions
            .lock()
            .unwrap()
            .push(MockVolumeSession {
                volume_id: volume_id.to_string(),
                vm_id: vm_id.to_string(),
                attachment_handle: attachment_handle.to_string(),
            });
    }

    /// Count successes already recorded for `name` and check the fail hook.
    fn begin_step(&self, name: &str) -> Result<(), ChvError> {
        let prefix = format!("{name}:");
        let ordinal = {
            let calls = self.calls.lock().unwrap();
            calls.iter().filter(|c| c.starts_with(&prefix)).count() + 1
        };
        let mut fail = self.fail_next.lock().unwrap();
        if fail.as_deref() == Some(&format!("{name}:{ordinal}")) {
            *fail = None;
            return Err(ChvError::Internal {
                reason: format!("mock injected failure at {name}:{ordinal}"),
            });
        }
        Ok(())
    }

    /// Record one successful step.
    fn record(&self, name: &str, detail: &str) {
        self.calls.lock().unwrap().push(format!("{name}:{detail}"));
    }
}

#[async_trait]
impl HostResourceController for MockHostResourceController {
    async fn open_volume(
        &self,
        volume_id: &str,
        backend_class: &str,
        locator: &str,
        options: HashMap<String, String>,
        _operation_id: Option<&str>,
    ) -> Result<(String, String, String), ChvError> {
        self.begin_step("open")?;
        self.record("open", volume_id);
        self.open_options.lock().unwrap().push((
            volume_id.to_string(),
            backend_class.to_string(),
            options,
        ));
        self.open_locators
            .lock()
            .unwrap()
            .push((volume_id.to_string(), locator.to_string()));
        Ok((
            volume_id.to_string(),
            format!("handle-{volume_id}"),
            format!("/dev/mock/{volume_id}"),
        ))
    }

    async fn attach_volume_to_vm(
        &self,
        volume_id: &str,
        _vm_id: &str,
        _attachment_handle: &str,
        _operation_id: Option<&str>,
    ) -> Result<(String, String), ChvError> {
        self.begin_step("attach")?;
        self.record("attach", volume_id);
        Ok(("block".to_string(), format!("/dev/mock/{volume_id}")))
    }

    async fn detach_volume_from_vm(
        &self,
        volume_id: &str,
        vm_id: &str,
        _force: bool,
        _operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        self.begin_step("detach")?;
        self.record("detach", volume_id);
        // stord clears the session's vm_id on detach; mirror that so a
        // post-detach `resolve_volume_handle` cannot hand out a handle.
        for session in self.volume_sessions.lock().unwrap().iter_mut() {
            if session.volume_id == volume_id && session.vm_id == vm_id {
                session.vm_id.clear();
            }
        }
        Ok(())
    }

    async fn close_volume(
        &self,
        volume_id: &str,
        _attachment_handle: &str,
        _operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        self.begin_step("close")?;
        self.record("close", volume_id);
        Ok(())
    }

    async fn resolve_volume_handle(
        &self,
        volume_id: &str,
        vm_id: &str,
    ) -> Result<Option<String>, ChvError> {
        self.begin_step("resolve")?;
        self.record("resolve", volume_id);
        Ok(self
            .volume_sessions
            .lock()
            .unwrap()
            .iter()
            .find(|session| session.volume_id == volume_id && session.vm_id == vm_id)
            .map(|session| session.attachment_handle.clone()))
    }

    async fn ensure_network_topology(
        &self,
        network_id: &str,
        _bridge_name: &str,
        _subnet_cidr: &str,
        _gateway_ip: &str,
        _operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        self.begin_step("ensure")?;
        self.record("ensure", network_id);
        Ok(())
    }

    async fn set_firewall_policy(
        &self,
        network_id: &str,
        policy_version: &str,
        policy_json: &[u8],
        _operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        self.begin_step("policy")?;
        self.record(
            "policy",
            &format!(
                "{}:{}:{}",
                network_id,
                policy_version,
                String::from_utf8_lossy(policy_json)
            ),
        );
        Ok(())
    }

    async fn delete_network_topology(
        &self,
        network_id: &str,
        _operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        self.begin_step("net_teardown")?;
        self.record("net_teardown", network_id);
        Ok(())
    }

    async fn attach_vm_nic(
        &self,
        nic_id: &str,
        _vm_id: &str,
        _network_id: &str,
        _mac_address: &str,
        _ip_address: &str,
        _operation_id: Option<&str>,
    ) -> Result<(String, String), ChvError> {
        self.begin_step("attach_nic")?;
        self.record("attach_nic", nic_id);
        Ok((format!("ns-{nic_id}"), format!("tap-{nic_id}")))
    }

    async fn detach_vm_nic(
        &self,
        nic_id: &str,
        _vm_id: &str,
        _network_id: &str,
        _operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        self.begin_step("detach_nic")?;
        self.record("detach_nic", nic_id);
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct MockCloudHypervisorAdapter {
    pub vms: Arc<Mutex<HashMap<String, VmConfig>>>,
    /// When true, `delete_vm` fails deterministically (as a generic
    /// `Internal` effect failure) without removing the VM, letting a test
    /// pin the drain-always behavior on a failed delete. Per-instance
    /// state; never a shared static. (`NotFound` is reserved for the real
    /// adapter's no-runtime-entry semantics — see `delete_vm`.)
    pub fail_delete: Arc<Mutex<bool>>,
    /// When true, the next `create_vm` parks forever WITHOUT creating the
    /// VM: a deterministic mid-effect crash window for the create lifecycle
    /// (the controller has already opened+attached volumes; the
    /// cloud-hypervisor create never happens). Consumed on use — one park
    /// per set. Per-instance state; never a shared static.
    pub park_create: Arc<Mutex<bool>>,
    /// Fires when a `create_vm` call parks (the stored permit makes
    /// `notified()` deterministic regardless of await ordering).
    pub create_parked: Arc<tokio::sync::Notify>,
    /// Injected `vm_counters` return value (PR-2 sample-path tests):
    /// `None` ⇒ the all-default counters (nothing measured, no epoch —
    /// the honest "VM just started / identity unknown" shape).
    pub counters_result: Arc<Mutex<Option<VmCounters>>>,
    /// When true, `send_migration`/`receive_migration` return
    /// `ChvError::Timeout` without touching the VM, modelling the agent's
    /// bounded wait expiring while the VMM transfer may still run. Used to
    /// pin the transfer-window seal (fail-closed cancellation) end to end.
    pub migration_timeout: Arc<Mutex<bool>>,
    /// When true, `send_migration`/`receive_migration` return
    /// `ChvError::OutcomeUnknown` without touching the VM, modelling a VMM
    /// reply lost after the request was delivered. Must seal exactly like a
    /// timeout (the transfer may still run).
    pub migration_outcome_unknown: Arc<Mutex<bool>>,
    /// When true, `send_migration`/`receive_migration` return a definitive
    /// `ChvError::Internal`. Used to pin the non-sealing branch: the transfer
    /// window must end and a paused source must be resumed.
    pub migration_definitive_failure: Arc<Mutex<bool>>,
}

#[async_trait]
impl CloudHypervisorAdapter for MockCloudHypervisorAdapter {
    async fn create_vm(
        &self,
        config: &VmConfig,
        _operation_id: Option<&str>,
    ) -> Result<String, ChvError> {
        if std::mem::take(&mut *self.park_create.lock().unwrap()) {
            self.create_parked.notify_one();
            std::future::pending::<()>().await;
        }
        self.vms
            .lock()
            .unwrap()
            .insert(config.vm_id.clone(), config.clone());
        Ok(config.vm_id.clone())
    }

    async fn start_vm(&self, _vm_id: &str, _operation_id: Option<&str>) -> Result<(), ChvError> {
        Ok(())
    }

    async fn stop_vm(
        &self,
        _vm_id: &str,
        _force: bool,
        _operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        Ok(())
    }

    async fn delete_vm(&self, vm_id: &str, _operation_id: Option<&str>) -> Result<(), ChvError> {
        if *self.fail_delete.lock().unwrap() {
            return Err(ChvError::Internal {
                reason: "forced delete failure (fail_delete knob)".to_string(),
            });
        }
        // Model the real process adapter: a delete for a VM with no runtime
        // entry (force-stopped, or already deleted) is NotFound. The
        // idempotency policy for that case belongs to the Core runtime's
        // delete arm, which owns the runtime-dir layout.
        if self.vms.lock().unwrap().remove(vm_id).is_none() {
            return Err(ChvError::NotFound {
                resource: "vm".to_string(),
                id: vm_id.to_string(),
            });
        }
        Ok(())
    }

    async fn reboot_vm(&self, _vm_id: &str, _operation_id: Option<&str>) -> Result<(), ChvError> {
        Ok(())
    }

    async fn pause_vm(&self, _vm_id: &str, _operation_id: Option<&str>) -> Result<(), ChvError> {
        Ok(())
    }

    async fn resume_vm(&self, _vm_id: &str, _operation_id: Option<&str>) -> Result<(), ChvError> {
        Ok(())
    }

    async fn power_button(
        &self,
        _vm_id: &str,
        _operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        Ok(())
    }

    async fn resize_vm(
        &self,
        vm_id: &str,
        cpus: Option<u32>,
        memory_bytes: Option<u64>,
        _operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        let mut map = self.vms.lock().unwrap();
        let config = map.get_mut(vm_id).ok_or_else(|| ChvError::NotFound {
            resource: "vm".to_string(),
            id: vm_id.to_string(),
        })?;
        if let Some(c) = cpus {
            config.cpus = c;
        }
        if let Some(m) = memory_bytes {
            config.memory_bytes = m;
        }
        Ok(())
    }

    async fn add_disk(
        &self,
        _vm_id: &str,
        _params: &AddDiskParams,
        _operation_id: Option<&str>,
    ) -> Result<String, ChvError> {
        Ok("mock-disk-id".to_string())
    }

    async fn remove_device(
        &self,
        _vm_id: &str,
        _device_id: &str,
        _operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        Ok(())
    }

    async fn add_net(
        &self,
        _vm_id: &str,
        _params: &AddNetParams,
        _operation_id: Option<&str>,
    ) -> Result<String, ChvError> {
        Ok("mock-net-id".to_string())
    }

    async fn resize_disk(
        &self,
        _vm_id: &str,
        _disk_id: &str,
        _new_size_bytes: u64,
        _operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        Ok(())
    }

    async fn snapshot_vm(
        &self,
        _vm_id: &str,
        _destination: &str,
        _operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        Ok(())
    }

    async fn restore_snapshot(
        &self,
        _vm_id: &str,
        _source: &str,
        _operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        Ok(())
    }

    async fn vm_info(&self, vm_id: &str) -> Result<VmInfo, ChvError> {
        let map = self.vms.lock().unwrap();
        let config = map.get(vm_id).ok_or_else(|| ChvError::NotFound {
            resource: "vm".to_string(),
            id: vm_id.to_string(),
        })?;
        Ok(VmInfo {
            state: "Running".to_string(),
            cpus: config.cpus,
            memory_bytes: config.memory_bytes,
        })
    }

    async fn vm_counters(&self, _vm_id: &str) -> Result<VmCounters, ChvError> {
        Ok(self
            .counters_result
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_default())
    }

    async fn ping(&self, _vm_id: &str) -> Result<bool, ChvError> {
        Ok(true)
    }

    async fn send_migration(
        &self,
        _vm_id: &str,
        _destination_url: &str,
        _operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        if *self.migration_timeout.lock().unwrap() {
            return Err(ChvError::Timeout {
                operation: "send-migration (mock)".to_string(),
                secs: 7200,
            });
        }
        if *self.migration_outcome_unknown.lock().unwrap() {
            return Err(ChvError::OutcomeUnknown {
                operation: "send-migration (mock)".to_string(),
                reason: "reply lost after delivery".to_string(),
            });
        }
        if *self.migration_definitive_failure.lock().unwrap() {
            return Err(ChvError::Internal {
                reason: "send-migration failed (mock)".to_string(),
            });
        }
        Ok(())
    }

    async fn receive_migration(
        &self,
        _vm_id: &str,
        _receiver_url: &str,
        _operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        if *self.migration_timeout.lock().unwrap() {
            return Err(ChvError::Timeout {
                operation: "receive-migration (mock)".to_string(),
                secs: 7200,
            });
        }
        if *self.migration_outcome_unknown.lock().unwrap() {
            return Err(ChvError::OutcomeUnknown {
                operation: "receive-migration (mock)".to_string(),
                reason: "reply lost after delivery".to_string(),
            });
        }
        if *self.migration_definitive_failure.lock().unwrap() {
            return Err(ChvError::Internal {
                reason: "receive-migration failed (mock)".to_string(),
            });
        }
        Ok(())
    }

    async fn get_vm_state(&self, vm_id: &str) -> Result<String, ChvError> {
        let map = self.vms.lock().unwrap();
        if map.contains_key(vm_id) {
            Ok("Running".to_string())
        } else {
            Err(ChvError::NotFound {
                resource: "vm".to_string(),
                id: vm_id.to_string(),
            })
        }
    }

    async fn coredump(
        &self,
        _vm_id: &str,
        _destination: &str,
        _operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        Ok(())
    }
}
