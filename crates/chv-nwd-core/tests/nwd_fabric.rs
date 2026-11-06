//! Handler-level integration tests for the ADR-021 stretched-L2 fabric path.
//!
//! These tests drive the real tonic UDS server (`NetworkServer`) with a
//! recording mock `NetworkExecutor` that mirrors the `LinuxExecutor`
//! contract: fabric apply during topology ensure, fabric-first teardown,
//! generation fencing in the handlers (which own the topology table),
//! tenant-MTU propagation into NIC attach, and the fail-closed behavior
//! when the fabric provider is disabled.
//!
//! The sandbox is unprivileged, so the real `ip`/`wg`-backed executor
//! cannot be driven here; its RecordingRunner-backed provider coverage
//! lives in `chv-nwd-core/src/fabric.rs` unit tests, and the shared
//! provider contract is gated in `tests/fabric_conformance.rs`.

use async_trait::async_trait;
use chv_errors::ChvError;
use chv_nwd_api::chv_nwd_api::{
    network_service_client::NetworkServiceClient, AttachVmNicRequest, DeleteNetworkTopologyRequest,
    EnsureNetworkTopologyRequest, FabricPeer, FabricPlan, FirewallPolicy, GetFabricIdentityRequest,
    GetOverlayStatusRequest, NatPolicy, NicSpec, OverlayType, SetFirewallPolicyRequest,
    SetNatPolicyRequest, TopologySpec, UpdateOverlayRequest,
};
use chv_nwd_core::executor::{
    DeleteOutcome, FabricOwnership, NetworkExecutor, OverlayStatusInfo, TopologyApplyResult,
};
use chv_nwd_core::fabric::{AppliedFabric, ApplyReport, FabricIdentity};
use chv_nwd_core::handlers::NetworkServiceImpl;
use chv_nwd_core::{NetworkServer, TopologyState, TopologyTable};
use chv_observability::Metrics;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::net::UnixStream;
use tonic::transport::{Endpoint, Uri};
use tonic::Request as TonicRequest;
use tower::service_fn;

const TEST_PUBLIC_KEY: &str = "K7XbF9cV2mQpT3nZ8sL4dW6yH1jR5uA0eG9iO2pS7kM=";
const PEER_PUBLIC_KEY: &str = "Q8YcG0dW3nRqU4oA9tM5eX7zI2kS6vB1fH0jP3qT8lN=";
const DEFAULT_TENANT_MTU: u32 = 1380;

/// Serializes tests that increment or exactly assert the process-global
/// fabric metric counters: the capturing recorder installed by
/// `fabric_metrics_recorder::install()` is shared by every test in this
/// binary and cargo runs the tests concurrently, so a before/after
/// equality assertion would otherwise race a concurrent test's
/// increment. An async lock so the guard may be held across the test's
/// await points (clippy::await_holding_lock).
static FABRIC_METRIC_MUTEX: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

async fn metric_test_lock() -> tokio::sync::MutexGuard<'static, ()> {
    FABRIC_METRIC_MUTEX.lock().await
}

/// Recording mock mirroring the `LinuxExecutor` fabric contract.
#[derive(Clone)]
struct FabricExecutor {
    calls: Arc<StdMutex<Vec<String>>>,
    fabric_enabled: bool,
    /// Stand-in for the provider's durable ownership journal: survives a
    /// "restart" (clearing the TopologyTable) in tests.
    owned: Arc<StdMutex<HashSet<String>>>,
    /// When set, `fabric_overlay_status` fails (m9 error path).
    overlay_status_error: bool,
    /// When set, fabric removal fails persistently (m5 handler semantics).
    remove_fails: bool,
    /// When set, `delete_topology` fails its LOCAL teardown half (n11:
    /// the handler must not proxy the fabric metric from the aggregate
    /// error).
    local_delete_fails: bool,
    /// When set, firewall/NAT applies fail (used to drive the post-graft
    /// guard-refresh rollback on the overlay path).
    policy_apply_fails: bool,
    /// When set, an MTU re-assert to an explicit value fails (m8 ordering).
    mtu_reassert_fails: bool,
    /// When set, the next fabric apply pauses after recording its call so a
    /// test can queue a competing ensure behind the handler's fabric lock.
    pause_apply: Arc<std::sync::atomic::AtomicBool>,
    apply_entered: Arc<tokio::sync::Notify>,
    apply_release: Arc<tokio::sync::Notify>,
}

impl FabricExecutor {
    fn enabled() -> Self {
        Self {
            calls: Arc::new(StdMutex::new(Vec::new())),
            fabric_enabled: true,
            owned: Arc::new(StdMutex::new(HashSet::new())),
            overlay_status_error: false,
            remove_fails: false,
            local_delete_fails: false,
            policy_apply_fails: false,
            mtu_reassert_fails: false,
            pause_apply: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            apply_entered: Arc::new(tokio::sync::Notify::new()),
            apply_release: Arc::new(tokio::sync::Notify::new()),
        }
    }

    fn with_failing_policy_apply() -> Self {
        Self {
            policy_apply_fails: true,
            ..Self::enabled()
        }
    }

    fn with_failing_policy_apply_and_removal() -> Self {
        Self {
            policy_apply_fails: true,
            remove_fails: true,
            ..Self::enabled()
        }
    }

    fn with_failing_mtu_reassert() -> Self {
        Self {
            mtu_reassert_fails: true,
            ..Self::enabled()
        }
    }

    fn disabled() -> Self {
        Self {
            fabric_enabled: false,
            ..Self::enabled()
        }
    }

    fn with_overlay_status_error() -> Self {
        Self {
            overlay_status_error: true,
            ..Self::enabled()
        }
    }

    fn with_failing_removal() -> Self {
        Self {
            remove_fails: true,
            ..Self::enabled()
        }
    }

    fn with_failing_local_delete() -> Self {
        Self {
            local_delete_fails: true,
            ..Self::enabled()
        }
    }

    fn record(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn count(&self, prefix: &str) -> usize {
        self.calls()
            .iter()
            .filter(|c| c.starts_with(prefix))
            .count()
    }
}

#[async_trait]
impl NetworkExecutor for FabricExecutor {
    async fn ensure_topology(&self, spec: &TopologySpec) -> Result<TopologyApplyResult, ChvError> {
        self.record(format!("ensure:{}", spec.network_id));
        let mut tenant_mtu = None;
        let mut fabric_plan_generation = None;
        let mut binding_generation = None;
        if spec.vni > 0 && spec.overlay_type == OverlayType::OverlayVxlan as i32 {
            let plan = spec
                .fabric
                .as_ref()
                .ok_or_else(|| ChvError::InvalidArgument {
                    field: "fabric".to_string(),
                    reason: "VXLAN overlay requires a fabric plan".to_string(),
                })?;
            let applied = self
                .apply_fabric_overlay(&spec.network_id, spec.vni, plan, &spec.bridge_name)
                .await?;
            tenant_mtu = Some(applied.tenant_mtu);
            fabric_plan_generation = Some(applied.plan_generation);
            binding_generation = Some(applied.binding_generation);
        }
        Ok(TopologyApplyResult {
            namespace_handle: spec.namespace_name.clone(),
            bridge_handle: spec.bridge_name.clone(),
            tenant_mtu,
            fabric_plan_generation,
            binding_generation,
        })
    }

    async fn delete_local_topology_by_derived_names(&self, _network_id: &str) {}

    async fn delete_topology(
        &self,
        network_id: &str,
        state: &TopologyState,
    ) -> Result<DeleteOutcome, ChvError> {
        // Fabric-first teardown (reverse dependency order), fail-open for
        // the fabric half like the LinuxExecutor (m5): the outcome is
        // reported through DeleteOutcome so the handler can count the
        // remove metric truthfully (n11). A disabled provider means no
        // teardown is attempted at all (unobservable, like LinuxExecutor).
        let mut fabric_removed = None;
        if state.fabric_plan_generation.is_some() && self.fabric_enabled {
            fabric_removed = Some(self.remove_fabric_overlay(network_id).await);
        }
        self.record(format!("delete:{network_id}"));
        if self.local_delete_fails {
            return Err(ChvError::Internal {
                reason: "local topology teardown deliberately failing".to_string(),
            });
        }
        Ok(DeleteOutcome { fabric_removed })
    }

    async fn health(&self, _network_id: &str, _state: &TopologyState) -> Result<String, ChvError> {
        Ok("healthy".to_string())
    }

    async fn attach_vm_nic(
        &self,
        network_id: &str,
        nic_id: &str,
        _vm_id: &str,
        _bridge_name: &str,
        tenant_mtu: Option<u32>,
        _mac_address: &str,
        _ip_address: &str,
    ) -> Result<(String, String), ChvError> {
        let mtu = tenant_mtu
            .map(|m| m.to_string())
            .unwrap_or_else(|| "none".to_string());
        self.record(format!("attach:{nic_id}:{mtu}"));
        Ok((format!("ns-{network_id}"), format!("tap-{nic_id}")))
    }

    async fn detach_vm_nic(
        &self,
        _nic_id: &str,
        _ownership: chv_common::AttachmentOwnership,
    ) -> Result<(), ChvError> {
        Ok(())
    }

    async fn set_firewall_policy(
        &self,
        network_id: &str,
        policy_version: &str,
        _policy_json: &[u8],
        _bridge_name: &str,
    ) -> Result<(), ChvError> {
        self.record(format!("set_firewall:{network_id}:{policy_version}"));
        if self.policy_apply_fails {
            return Err(ChvError::Internal {
                reason: "policy apply deliberately failing".to_string(),
            });
        }
        Ok(())
    }

    async fn set_nat_policy(
        &self,
        network_id: &str,
        policy_version: &str,
        _policy_json: &[u8],
        _bridge_name: &str,
    ) -> Result<(), ChvError> {
        self.record(format!("set_nat:{network_id}:{policy_version}"));
        if self.policy_apply_fails {
            return Err(ChvError::Internal {
                reason: "policy apply deliberately failing".to_string(),
            });
        }
        Ok(())
    }

    async fn ensure_dhcp_scope(
        &self,
        _network_id: &str,
        _cidr: &str,
        _range_start: &str,
        _range_end: &str,
        _dns_servers: &[String],
    ) -> Result<(), ChvError> {
        Ok(())
    }

    async fn ensure_dns_scope(
        &self,
        _network_id: &str,
        _forwarders: &[&str],
        _static_records: &std::collections::HashMap<String, String>,
    ) -> Result<(), ChvError> {
        Ok(())
    }

    async fn expose_service(
        &self,
        _network_id: &str,
        _exposure_id: &str,
        _protocol: &str,
        _external_port: u32,
        _target_ip: &str,
        _target_port: u32,
        _mode: &str,
    ) -> Result<(), ChvError> {
        Ok(())
    }

    async fn withdraw_service_exposure(
        &self,
        _network_id: &str,
        _exposure_id: &str,
    ) -> Result<(), ChvError> {
        Ok(())
    }

    async fn send_gratuitous_arp(
        &self,
        _namespace: &str,
        _bridge_name: &str,
        _vm_ip: &str,
    ) -> Result<(), ChvError> {
        Ok(())
    }

    async fn apply_fabric_overlay(
        &self,
        network_id: &str,
        vni: u32,
        plan: &FabricPlan,
        bridge_name: &str,
    ) -> Result<AppliedFabric, ChvError> {
        if !self.fabric_enabled {
            return Err(ChvError::InvalidArgument {
                field: "fabric".to_string(),
                reason: "fabric provider is disabled".to_string(),
            });
        }
        self.record(format!(
            "apply_fabric:{network_id}:{vni}:{}:{bridge_name}",
            plan.plan_generation
        ));
        if self.pause_apply.load(std::sync::atomic::Ordering::SeqCst) {
            self.apply_entered.notify_one();
            self.apply_release.notified().await;
        }
        self.owned.lock().unwrap().insert(network_id.to_string());
        Ok(AppliedFabric {
            report: ApplyReport::default(),
            plan_generation: plan.plan_generation,
            binding_generation: plan.binding_generation,
            tenant_mtu: if plan.tenant_mtu == 0 {
                DEFAULT_TENANT_MTU
            } else {
                plan.tenant_mtu
            },
            consumer_veth: format!("chv-{network_id}-a"),
        })
    }

    async fn remove_fabric_overlay(&self, network_id: &str) -> Result<(), ChvError> {
        if !self.fabric_enabled {
            return Err(ChvError::InvalidArgument {
                field: "fabric".to_string(),
                reason: "fabric provider is disabled".to_string(),
            });
        }
        self.record(format!("remove_fabric:{network_id}"));
        if self.remove_fails {
            return Err(ChvError::Internal {
                reason: "fabric removal deliberately failing".to_string(),
            });
        }
        self.owned.lock().unwrap().remove(network_id);
        Ok(())
    }

    async fn fabric_owned(&self, network_id: &str) -> Result<FabricOwnership, ChvError> {
        self.record(format!("fabric_owned:{network_id}"));
        if !self.fabric_enabled {
            return Ok(FabricOwnership::ProviderDisabled);
        }
        if self.owned.lock().unwrap().contains(network_id) {
            Ok(FabricOwnership::Owned)
        } else {
            Ok(FabricOwnership::NotOwned)
        }
    }

    async fn fabric_identity(&self) -> Result<FabricIdentity, ChvError> {
        if !self.fabric_enabled {
            return Err(ChvError::InvalidArgument {
                field: "fabric".to_string(),
                reason: "fabric provider is disabled".to_string(),
            });
        }
        Ok(FabricIdentity {
            public_key: TEST_PUBLIC_KEY.to_string(),
            underlay_mtu: 1500,
        })
    }

    async fn fabric_overlay_status(&self, network_id: &str) -> Result<OverlayStatusInfo, ChvError> {
        self.record(format!("overlay_status:{network_id}"));
        if self.overlay_status_error {
            return Err(ChvError::Internal {
                reason: "fabric status deliberately failing".to_string(),
            });
        }
        if self.owned.lock().unwrap().contains(network_id) {
            Ok(OverlayStatusInfo {
                vxlan_interface_up: true,
                fdb_entry_count: 1,
            })
        } else {
            Ok(OverlayStatusInfo {
                vxlan_interface_up: false,
                fdb_entry_count: 0,
            })
        }
    }

    async fn reassert_tenant_mtu(
        &self,
        network_id: &str,
        _bridge_name: &str,
        _subnet_cidr: &str,
        _gateway_ip: &str,
        tenant_mtu: Option<u32>,
    ) -> Result<(), ChvError> {
        // `none` marks the bridge-only reset (m7 residue): port MTUs back
        // to the default and dnsmasq restarted without DHCP option 26.
        let mtu = tenant_mtu
            .map(|m| m.to_string())
            .unwrap_or_else(|| "none".to_string());
        self.record(format!("reassert_mtu:{network_id}:{mtu}"));
        if self.mtu_reassert_fails && tenant_mtu.is_some() {
            return Err(ChvError::Internal {
                reason: "MTU re-assert deliberately failing".to_string(),
            });
        }
        Ok(())
    }
}

async fn make_client(socket: PathBuf) -> NetworkServiceClient<tonic::transport::Channel> {
    let channel = Endpoint::try_from("http://[::]:50051")
        .unwrap()
        .connect_with_connector(service_fn(move |_: Uri| {
            let s = socket.clone();
            async move {
                let stream = UnixStream::connect(s).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::tokio::TokioIo::new(stream))
            }
        }))
        .await
        .unwrap();
    NetworkServiceClient::new(channel)
}

/// Spawn a real tonic UDS server over the mock executor. The returned
/// `TempDir` keeps the socket path alive for the duration of the test.
async fn spawn_server(executor: FabricExecutor) -> (PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nwd.sock");
    let server = NetworkServer::new(executor, Metrics::new());
    let socket_clone = socket.clone();
    tokio::spawn(async move {
        server.serve(&socket_clone).await.ok();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (socket, dir)
}

fn fabric_plan(generation: u64, tenant_mtu: u32) -> Option<FabricPlan> {
    fabric_plan_with_binding(generation, 1, tenant_mtu)
}

fn fabric_plan_with_binding(
    generation: u64,
    binding_generation: u64,
    tenant_mtu: u32,
) -> Option<FabricPlan> {
    Some(FabricPlan {
        fabric_domain_id: "fabric-1".to_string(),
        local_host_id: "host-01".to_string(),
        local_fabric_ip: "100.100.0.1".to_string(),
        tenant_mtu,
        fabric_mtu: 0,
        binding_generation,
        plan_generation: generation,
        peers: vec![FabricPeer {
            node_id: "host-02".to_string(),
            public_key: PEER_PUBLIC_KEY.to_string(),
            underlay_endpoint: "203.0.113.7:65001".to_string(),
            fabric_ip: "100.100.0.2".to_string(),
        }],
    })
}

fn fabric_topology_spec(network_id: &str, generation: u64) -> TopologySpec {
    TopologySpec {
        network_id: network_id.to_string(),
        tenant_id: "t1".to_string(),
        bridge_name: "br-fab".to_string(),
        namespace_name: "ns-fab".to_string(),
        subnet_cidr: "10.0.50.0/24".to_string(),
        gateway_ip: "10.0.50.1".to_string(),
        options: Default::default(),
        vni: 100,
        vtep_endpoints: vec![],
        overlay_type: OverlayType::OverlayVxlan as i32,
        fabric: fabric_plan(generation, 0),
    }
}

#[tokio::test]
async fn ensure_with_fabric_applies_and_persists_state() {
    let executor = FabricExecutor::enabled();
    let (socket, _dir) = spawn_server(executor.clone()).await;
    let mut client = make_client(socket).await;

    let resp = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(fabric_topology_spec("net-fab", 7)),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.status, "OK");

    // The fabric apply must carry network, vni, plan generation and bridge.
    assert!(executor
        .calls()
        .iter()
        .any(|c| c == "apply_fabric:net-fab:100:7:br-fab"));

    // Persisted state must carry the tenant MTU (defaulted from 0) and the
    // applied generation into later operations: attach receives the MTU.
    let attach = client
        .attach_vm_nic(AttachVmNicRequest {
            meta: None,
            nic: Some(NicSpec {
                nic_id: "nic-fab".to_string(),
                vm_id: "vm-fab".to_string(),
                network_id: "net-fab".to_string(),
                mac_address: "02:00:00:00:00:01".to_string(),
                tap_name: "tap-fab".to_string(),
                ip_address: "10.0.50.10".to_string(),
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(attach.result.as_ref().unwrap().status, "OK");
    assert!(executor.calls().iter().any(|c| c == "attach:nic-fab:1380"));
}

#[tokio::test]
async fn ensure_rejects_stale_fabric_generation() {
    let executor = FabricExecutor::enabled();
    let (socket, _dir) = spawn_server(executor.clone()).await;
    let mut client = make_client(socket).await;

    let first = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(fabric_topology_spec("net-stale", 5)),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(first.status, "OK");
    assert_eq!(executor.count("apply_fabric:"), 1);

    // An older plan generation is fenced off (ADR-021 §4).
    let stale = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(fabric_topology_spec("net-stale", 3)),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(stale.status, "error");
    assert_eq!(stale.error_code, "STALE_GENERATION");
    assert_eq!(executor.count("apply_fabric:"), 1, "no re-apply on stale");

    // An unchanged topology (same generation) is idempotently OK without
    // touching the executor again.
    let replay = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(fabric_topology_spec("net-stale", 5)),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(replay.status, "OK");
    assert_eq!(executor.count("apply_fabric:"), 1, "no re-apply on replay");
}

#[tokio::test]
async fn update_overlay_fabric_path_fences_and_updates_state() {
    let executor = FabricExecutor::enabled();
    let (socket, _dir) = spawn_server(executor.clone()).await;
    let mut client = make_client(socket).await;

    // Ensure with generation 2 (explicit tenant MTU 1400).
    let mut spec = fabric_topology_spec("net-upd", 2);
    spec.fabric = fabric_plan(2, 1400);
    let ensure = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(spec),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ensure.status, "OK");
    assert!(executor
        .calls()
        .iter()
        .any(|c| c == "apply_fabric:net-upd:100:2:br-fab"));

    // A newer plan via UpdateOverlay applies and persists the new
    // generation and tenant MTU.
    let update = client
        .update_overlay(UpdateOverlayRequest {
            network_id: "net-upd".to_string(),
            vni: 100,
            vtep_endpoints: vec![],
            fdb_entries: vec![],
            fabric: fabric_plan(9, 0),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(update.result.as_ref().unwrap().status, "OK");
    assert!(executor
        .calls()
        .iter()
        .any(|c| c == "apply_fabric:net-upd:100:9:br-fab"));

    // Stale generation is rejected in-band.
    let stale = client
        .update_overlay(UpdateOverlayRequest {
            network_id: "net-upd".to_string(),
            vni: 100,
            vtep_endpoints: vec![],
            fdb_entries: vec![],
            fabric: fabric_plan(4, 0),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(stale.result.as_ref().unwrap().status, "error");
    assert_eq!(
        stale.result.as_ref().unwrap().error_code,
        "STALE_GENERATION"
    );
    assert_eq!(executor.count("apply_fabric:"), 2, "stale plan not applied");

    // Replaying the same (current) plan is OK: the handler always applies
    // and the provider layer dedups (idempotent re-assert).
    let replay = client
        .update_overlay(UpdateOverlayRequest {
            network_id: "net-upd".to_string(),
            vni: 100,
            vtep_endpoints: vec![],
            fdb_entries: vec![],
            fabric: fabric_plan(9, 0),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(replay.result.as_ref().unwrap().status, "OK");

    // The persisted generation (9) now fences a later ensure with the old
    // generation 2 plan.
    let fenced = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(fabric_topology_spec("net-upd", 2)),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(fenced.status, "error");
    assert_eq!(fenced.error_code, "STALE_GENERATION");
}

#[tokio::test]
async fn update_overlay_with_a_rejected_policy_does_not_graft_the_veth() {
    let executor = FabricExecutor::enabled();
    let (socket, _dir) = spawn_server(executor.clone()).await;
    let mut client = make_client(socket).await;

    let mut spec = fabric_topology_spec("net-ov-reject", 2);
    spec.fabric = fabric_plan(2, 0);
    let ensure = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(spec),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ensure.status, "OK");
    let applied_before = executor.count("apply_fabric:");

    // A rejected NAT save leaves a marker and no desired state for NAT. The
    // overlay update grafts a consumer veth, so it must refuse before touching
    // the datapath rather than leave the veth outside the guard set.
    let nat = client
        .set_nat_policy(SetNatPolicyRequest {
            meta: None,
            network_id: "net-ov-reject".to_string(),
            policy: Some(NatPolicy {
                policy_version: "v1".to_string(),
                policy_json: br#"[{"source_cidr":"not-a-cidr"}]"#.to_vec(),
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_ne!(nat.status, "OK");

    let refused = client
        .update_overlay(UpdateOverlayRequest {
            network_id: "net-ov-reject".to_string(),
            vni: 100,
            vtep_endpoints: vec![],
            fdb_entries: vec![],
            fabric: fabric_plan(3, 0),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(refused.result.as_ref().unwrap().status, "error");
    assert_eq!(
        executor.count("apply_fabric:"),
        applied_before,
        "a rejected policy must not graft a veth that the guard set cannot cover"
    );

    // A valid save clears the marker and the same update then applies.
    let valid = client
        .set_nat_policy(SetNatPolicyRequest {
            meta: None,
            network_id: "net-ov-reject".to_string(),
            policy: Some(NatPolicy {
                policy_version: "v2".to_string(),
                policy_json: b"[]".to_vec(),
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(valid.status, "OK");

    let applied = client
        .update_overlay(UpdateOverlayRequest {
            network_id: "net-ov-reject".to_string(),
            vni: 100,
            vtep_endpoints: vec![],
            fdb_entries: vec![],
            fabric: fabric_plan(3, 0),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(applied.result.as_ref().unwrap().status, "OK");
    assert!(executor
        .calls()
        .iter()
        .any(|c| c == "apply_fabric:net-ov-reject:100:3:br-fab"));
}

/// If the guard refresh fails *after* the overlay grafted its consumer veth
/// (e.g. the executor's policy apply errors), the handler must remove the
/// overlay and clear the persisted fabric state. Leaving the veth attached
/// with no re-scope would put it outside the guard set.
#[tokio::test]
async fn update_overlay_rolls_back_when_the_guard_refresh_fails() {
    let executor = FabricExecutor::with_failing_policy_apply();
    let (socket, _dir) = spawn_server(executor.clone()).await;
    let mut client = make_client(socket).await;

    let mut spec = fabric_topology_spec("net-ov-rollback", 2);
    spec.fabric = fabric_plan(2, 0);
    let ensure = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(spec),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ensure.status, "OK");

    // A valid save is recorded (desired state) even though the executor apply
    // fails; the update's guard refresh will hit the same failure.
    let fw = client
        .set_firewall_policy(SetFirewallPolicyRequest {
            meta: None,
            network_id: "net-ov-rollback".to_string(),
            policy: Some(FirewallPolicy {
                policy_version: "v1".to_string(),
                policy_json: b"[]".to_vec(),
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_ne!(fw.status, "OK");

    let update = client
        .update_overlay(UpdateOverlayRequest {
            network_id: "net-ov-rollback".to_string(),
            vni: 100,
            vtep_endpoints: vec![],
            fdb_entries: vec![],
            fabric: fabric_plan(3, 0),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(update.result.as_ref().unwrap().status, "error");
    assert_eq!(
        executor.count("remove_fabric:net-ov-rollback"),
        1,
        "a failed guard refresh must roll the overlay back"
    );

    // The rolled-back generation must not fence a later ensure with the older
    // plan: the persisted state was cleared with the datapath.
    let mut replay_spec = fabric_topology_spec("net-ov-rollback", 2);
    replay_spec.fabric = fabric_plan(2, 0);
    let replay = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(replay_spec),
        })
        .await
        .unwrap()
        .into_inner();
    assert_ne!(
        replay.error_code, "STALE_GENERATION",
        "the rolled-back generation must not leave a stale fence: {replay:?}"
    );
    assert!(
        executor
            .calls()
            .iter()
            .any(|c| c == "apply_fabric:net-ov-rollback:100:2:br-fab"),
        "the older plan must be re-applied, not fenced: {:?}",
        executor.calls()
    );
}

#[tokio::test]
async fn ensure_with_fabric_refuses_a_rejected_policy_before_grafting() {
    let executor = FabricExecutor::enabled();
    let (socket, _dir) = spawn_server(executor.clone()).await;
    let mut client = make_client(socket).await;

    let mut spec = fabric_topology_spec("net-ensure-reject", 2);
    spec.fabric = fabric_plan(2, 0);
    let ensure = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(spec),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ensure.status, "OK");
    let applied_before = executor.count("apply_fabric:");

    let nat = client
        .set_nat_policy(SetNatPolicyRequest {
            meta: None,
            network_id: "net-ensure-reject".to_string(),
            policy: Some(NatPolicy {
                policy_version: "v1".to_string(),
                policy_json: br#"[{"source_cidr":"not-a-cidr"}]"#.to_vec(),
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_ne!(nat.status, "OK");

    let mut newer = fabric_topology_spec("net-ensure-reject", 3);
    newer.fabric = fabric_plan(3, 0);
    let refused = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(newer.clone()),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(refused.status, "error");
    assert_eq!(
        executor.count("apply_fabric:"),
        applied_before,
        "a rejected policy must not graft a veth that the guard set cannot cover"
    );

    let valid = client
        .set_nat_policy(SetNatPolicyRequest {
            meta: None,
            network_id: "net-ensure-reject".to_string(),
            policy: Some(NatPolicy {
                policy_version: "v2".to_string(),
                policy_json: b"[]".to_vec(),
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(valid.status, "OK");

    let applied = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(newer),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(applied.status, "OK");
    assert!(executor
        .calls()
        .iter()
        .any(|c| c == "apply_fabric:net-ensure-reject:100:3:br-fab"));
}

/// If a fabric ensure's post-graft guard refresh fails, the overlay must be
/// rolled back (the veth would otherwise sit outside the guard set), the
/// bridge MTU reset, and the generation fence cleared so the latest plan can
/// be retried.
#[tokio::test]
async fn ensure_with_fabric_rolls_back_when_the_guard_refresh_fails() {
    let executor = FabricExecutor::with_failing_policy_apply();
    let (socket, _dir) = spawn_server(executor.clone()).await;
    let mut client = make_client(socket).await;

    let mut spec = fabric_topology_spec("net-ensure-rollback", 2);
    spec.fabric = fabric_plan(2, 0);
    let ensure = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(spec),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ensure.status, "OK");

    // The desired state is recorded even though the executor apply fails, so
    // the next fabric apply's guard refresh hits the same failure.
    let fw = client
        .set_firewall_policy(SetFirewallPolicyRequest {
            meta: None,
            network_id: "net-ensure-rollback".to_string(),
            policy: Some(FirewallPolicy {
                policy_version: "v1".to_string(),
                policy_json: b"[]".to_vec(),
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_ne!(fw.status, "OK");

    let mut newer = fabric_topology_spec("net-ensure-rollback", 3);
    newer.fabric = fabric_plan(3, 0);
    let refused = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(newer),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(refused.status, "error");
    assert!(
        refused.human_summary.contains("rolled back"),
        "the error must disclose the rollback: {refused:?}"
    );
    assert!(
        executor
            .calls()
            .iter()
            .any(|c| c == "remove_fabric:net-ensure-rollback"),
        "a failed guard refresh must remove the overlay: {:?}",
        executor.calls()
    );
    assert!(
        executor
            .calls()
            .iter()
            .any(|c| c == "reassert_mtu:net-ensure-rollback:none"),
        "the rollback must reset the bridge MTU: {:?}",
        executor.calls()
    );

    // The cleared fence must let an older plan be re-applied (the guard
    // refresh still fails, so the RPC errors, but not as STALE_GENERATION).
    let mut replay_spec = fabric_topology_spec("net-ensure-rollback", 2);
    replay_spec.fabric = fabric_plan(2, 0);
    let replay = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(replay_spec),
        })
        .await
        .unwrap()
        .into_inner();
    assert_ne!(
        replay.error_code, "STALE_GENERATION",
        "the rolled-back generation must not leave a stale fence: {replay:?}"
    );
    assert_ne!(
        replay.status, "OK",
        "the same-generation replay must not short-circuit to OK after the overlay \
         was removed: {replay:?}"
    );
    assert!(
        executor
            .calls()
            .iter()
            .any(|c| c == "apply_fabric:net-ensure-rollback:100:2:br-fab"),
        "the older plan must be re-applied, not short-circuited: {:?}",
        executor.calls()
    );
}

/// When the rollback removal itself fails, the error must not claim the
/// overlay was rolled back: the consumer veth may still be grafted and
/// unguarded. Cover both the overlay-update and fabric-ensure paths.
#[tokio::test]
async fn failed_overlay_rollback_is_reported_truthfully() {
    let executor = FabricExecutor::with_failing_policy_apply_and_removal();
    let (socket, _dir) = spawn_server(executor.clone()).await;
    let mut client = make_client(socket).await;

    let mut spec = fabric_topology_spec("net-rollback-fail", 2);
    spec.fabric = fabric_plan(2, 0);
    let ensure = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(spec),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ensure.status, "OK");

    let fw = client
        .set_firewall_policy(SetFirewallPolicyRequest {
            meta: None,
            network_id: "net-rollback-fail".to_string(),
            policy: Some(FirewallPolicy {
                policy_version: "v1".to_string(),
                policy_json: b"[]".to_vec(),
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_ne!(fw.status, "OK");

    let update = client
        .update_overlay(UpdateOverlayRequest {
            network_id: "net-rollback-fail".to_string(),
            vni: 100,
            vtep_endpoints: vec![],
            fdb_entries: vec![],
            fabric: fabric_plan(3, 0),
        })
        .await
        .unwrap()
        .into_inner();
    let summary = &update.result.as_ref().unwrap().human_summary;
    assert_eq!(update.result.as_ref().unwrap().status, "error");
    assert!(
        summary.contains("rollback FAILED") && summary.contains("unguarded"),
        "the update error must disclose a failed rollback: {summary}"
    );
    assert!(
        !summary.contains("was rolled back"),
        "the update error must not claim a rollback that did not happen: {summary}"
    );

    // Same disclosure on the ensure path.
    let mut newer = fabric_topology_spec("net-rollback-fail", 3);
    newer.fabric = fabric_plan(3, 0);
    let ensured = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(newer),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ensured.status, "error");
    assert!(
        ensured.human_summary.contains("rollback FAILED")
            && ensured.human_summary.contains("unguarded"),
        "the ensure error must disclose a failed rollback: {ensured:?}"
    );
    assert!(
        !ensured.human_summary.contains("was rolled back"),
        "the ensure error must not claim a rollback that did not happen: {ensured:?}"
    );
}

/// The guard refresh must run before the m8 MTU re-assert: an MTU failure
/// after a fabric apply must leave the overlay guarded (no rollback) rather
/// than returning before the veth is re-scoped.
#[tokio::test]
async fn fabric_ensure_refreshes_the_guard_before_an_mtu_failure() {
    let executor = FabricExecutor::with_failing_mtu_reassert();
    let (socket, _dir) = spawn_server(executor.clone()).await;
    let mut client = make_client(socket).await;

    let mut spec = fabric_topology_spec("net-mtu-order", 2);
    spec.fabric = fabric_plan(2, 1400);
    let ensure = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(spec),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ensure.status, "OK");

    let fw = client
        .set_firewall_policy(SetFirewallPolicyRequest {
            meta: None,
            network_id: "net-mtu-order".to_string(),
            policy: Some(FirewallPolicy {
                policy_version: "v1".to_string(),
                policy_json: b"[]".to_vec(),
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(fw.status, "OK");
    executor.calls.lock().unwrap().clear();

    let mut newer = fabric_topology_spec("net-mtu-order", 3);
    newer.fabric = fabric_plan(3, 1500);
    let failed = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(newer),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(failed.status, "error");
    assert!(
        executor
            .calls()
            .iter()
            .any(|c| c.starts_with("set_firewall:net-mtu-order:")),
        "the guard must be refreshed before the MTU re-assert: {:?}",
        executor.calls()
    );
    assert!(
        !executor
            .calls()
            .iter()
            .any(|c| c == "remove_fabric:net-mtu-order"),
        "an MTU failure after a successful refresh must not roll the overlay back: {:?}",
        executor.calls()
    );
}

/// A queued fabric ensure must re-read the topology after acquiring
/// `fabric_ops`; otherwise it fences against a pre-lock generation and can
/// apply an older plan over a newer one.
#[tokio::test]
async fn queued_fabric_ensure_rechecks_the_generation_under_the_lock() {
    let executor = FabricExecutor::enabled();
    let (socket, _dir) = spawn_server(executor.clone()).await;
    let mut client = make_client(socket).await;

    let mut spec = fabric_topology_spec("net-ensure-race", 2);
    spec.fabric = fabric_plan(2, 0);
    let ensure = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(spec),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ensure.status, "OK");

    // Pause the newer overlay update inside apply_fabric_overlay while it
    // holds fabric_ops.
    executor
        .pause_apply
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut update_client = client.clone();
    let update = tokio::spawn(async move {
        update_client
            .update_overlay(UpdateOverlayRequest {
                network_id: "net-ensure-race".to_string(),
                vni: 100,
                vtep_endpoints: vec![],
                fdb_entries: vec![],
                fabric: fabric_plan(6, 0),
            })
            .await
            .unwrap()
            .into_inner()
    });
    executor.apply_entered.notified().await;

    // Queue an older ensure: it snapshots gen 2 before blocking on the lock.
    let mut ensure_client = client.clone();
    let queued = tokio::spawn(async move {
        let mut older = fabric_topology_spec("net-ensure-race", 5);
        older.fabric = fabric_plan(5, 0);
        ensure_client
            .ensure_network_topology(EnsureNetworkTopologyRequest {
                meta: None,
                topology: Some(older),
            })
            .await
            .unwrap()
            .into_inner()
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    executor
        .pause_apply
        .store(false, std::sync::atomic::Ordering::SeqCst);
    executor.apply_release.notify_one();
    let update_result = update.await.unwrap();
    assert_eq!(update_result.result.as_ref().unwrap().status, "OK");

    let queued_result = queued.await.unwrap();
    assert_eq!(
        queued_result.error_code, "STALE_GENERATION",
        "the queued ensure must fence against the generation applied while it waited: \
         {queued_result:?}"
    );
    assert!(
        !executor
            .calls()
            .iter()
            .any(|c| c == "apply_fabric:net-ensure-race:100:5:br-fab"),
        "the older plan must not be applied over the newer one: {:?}",
        executor.calls()
    );
}

#[tokio::test]
async fn update_overlay_without_fabric_plan_is_rejected_in_band() {
    let executor = FabricExecutor::enabled();
    let (socket, _dir) = spawn_server(executor.clone()).await;
    let mut client = make_client(socket).await;

    // Ensure a fabric-backed topology first so the network exists.
    let ensure = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(fabric_topology_spec("net-nofab", 1)),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ensure.status, "OK");

    // UpdateOverlay without a fabric plan is rejected in-band: the legacy
    // nolearning VXLAN/FDB datapath was retired by ADR-021.
    let update = client
        .update_overlay(UpdateOverlayRequest {
            network_id: "net-nofab".to_string(),
            vni: 100,
            vtep_endpoints: vec![],
            fdb_entries: vec![],
            fabric: None,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(update.result.as_ref().unwrap().status, "error");
    assert_eq!(
        update.result.as_ref().unwrap().error_code,
        "INVALID_ARGUMENT"
    );
    assert!(
        update
            .result
            .as_ref()
            .unwrap()
            .human_summary
            .contains("ADR-021"),
        "the rejection must name ADR-021, got: {}",
        update.result.as_ref().unwrap().human_summary
    );
    assert_eq!(
        executor.count("apply_fabric:"),
        1,
        "no fabric apply beyond the initial ensure"
    );
}

#[tokio::test]
async fn get_overlay_status_uses_fabric_branch() {
    let executor = FabricExecutor::enabled();
    let (socket, _dir) = spawn_server(executor.clone()).await;
    let mut client = make_client(socket).await;

    let ensure = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(fabric_topology_spec("net-obs", 1)),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ensure.status, "OK");

    let status = client
        .get_overlay_status(GetOverlayStatusRequest {
            network_id: "net-obs".to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(status.network_id, "net-obs");
    assert_eq!(status.vni, 100);
    assert!(status.vxlan_interface_up);
    assert_eq!(status.fdb_entry_count, 1);
    assert!(executor
        .calls()
        .iter()
        .any(|c| c == "overlay_status:net-obs"));
}

#[tokio::test]
async fn get_fabric_identity_returns_public_key_only() {
    let executor = FabricExecutor::enabled();
    let (socket, _dir) = spawn_server(executor.clone()).await;
    let mut client = make_client(socket).await;

    let identity = client
        .get_fabric_identity(GetFabricIdentityRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(identity.result.as_ref().unwrap().status, "OK");
    assert_eq!(identity.public_key, TEST_PUBLIC_KEY);
    assert_eq!(identity.underlay_mtu, 1500);
}

#[tokio::test]
async fn fabric_rpcs_fail_closed_when_provider_disabled() {
    let executor = FabricExecutor::disabled();
    let (socket, _dir) = spawn_server(executor.clone()).await;
    let mut client = make_client(socket).await;

    // Ensure with a VXLAN overlay + fabric plan must fail closed when the
    // provider is disabled; no topology state may be persisted.
    let ensure = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(fabric_topology_spec("net-off", 1)),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ensure.status, "error");
    assert_eq!(ensure.error_code, "INVALID_ARGUMENT");

    // GetFabricIdentity fails closed with an in-band error.
    let identity = client
        .get_fabric_identity(GetFabricIdentityRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(identity.result.as_ref().unwrap().status, "error");
    assert_eq!(
        identity.result.as_ref().unwrap().error_code,
        "INVALID_ARGUMENT"
    );
    assert!(
        identity.public_key.is_empty(),
        "no identity may be invented when the provider is disabled"
    );

    // VXLAN overlay without any fabric plan is rejected as invalid.
    let mut spec = fabric_topology_spec("net-off", 1);
    spec.fabric = None;
    let no_plan = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(spec),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(no_plan.status, "error");
    assert_eq!(no_plan.error_code, "INVALID_ARGUMENT");
}

#[tokio::test]
async fn delete_topology_tears_down_fabric_first() {
    let _metrics = metric_test_lock().await;
    let executor = FabricExecutor::enabled();
    let (socket, _dir) = spawn_server(executor.clone()).await;
    let mut client = make_client(socket).await;

    let ensure = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(fabric_topology_spec("net-del", 1)),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ensure.status, "OK");

    let del = client
        .delete_network_topology(DeleteNetworkTopologyRequest {
            meta: None,
            network_id: "net-del".to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(del.status, "OK");

    // Fabric teardown must run, and must precede local topology teardown.
    let calls = executor.calls();
    let remove_idx = calls
        .iter()
        .position(|c| c == "remove_fabric:net-del")
        .expect("fabric overlay must be removed");
    let delete_idx = calls
        .iter()
        .position(|c| c == "delete:net-del")
        .expect("local topology must be torn down");
    assert!(remove_idx < delete_idx, "fabric teardown runs first");

    // Idempotent delete: no second teardown, still OK.
    let del2 = client
        .delete_network_topology(DeleteNetworkTopologyRequest {
            meta: None,
            network_id: "net-del".to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(del2.status, "OK");
    assert_eq!(executor.count("remove_fabric:"), 1);
}

// ---- M1: VNI / binding_generation in the ensure short-circuit -------------

#[tokio::test]
async fn ensure_replay_with_different_vni_does_not_short_circuit() {
    let executor = FabricExecutor::enabled();
    let (socket, _dir) = spawn_server(executor.clone()).await;
    let mut client = make_client(socket).await;

    let first = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(fabric_topology_spec("net-vni", 5)),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(first.status, "OK");
    assert_eq!(executor.count("apply_fabric:"), 1);

    // A VNI re-bind changes only the VNI (binding_generation may bump while
    // desired_generation — the fabric plan generation — stays put). The
    // replay must NOT short-circuit, or the datapath would keep the OLD
    // VNI and two networks could cross-bleed silently.
    let mut spec = fabric_topology_spec("net-vni", 5);
    spec.vni = 200;
    let second = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(spec),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(second.status, "OK");
    assert!(
        executor
            .calls()
            .iter()
            .any(|c| c == "apply_fabric:net-vni:200:5:br-fab"),
        "the new VNI must be applied, got {:?}",
        executor.calls()
    );
    assert_eq!(
        executor.count("apply_fabric:"),
        2,
        "a different VNI must not be short-circuited"
    );
}

#[tokio::test]
async fn update_overlay_rejects_lower_binding_generation() {
    let executor = FabricExecutor::enabled();
    let (socket, _dir) = spawn_server(executor.clone()).await;
    let mut client = make_client(socket).await;

    // Ensure with plan generation 2, binding generation 7.
    let mut spec = fabric_topology_spec("net-bg", 2);
    spec.fabric = fabric_plan_with_binding(2, 7, 0);
    let ensure = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(spec),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ensure.status, "OK");
    assert_eq!(executor.count("apply_fabric:"), 1);

    // A newer plan generation with a LOWER binding generation is a stale
    // binding: rejected in-band, nothing applied.
    let stale_binding = client
        .update_overlay(UpdateOverlayRequest {
            network_id: "net-bg".to_string(),
            vni: 100,
            vtep_endpoints: vec![],
            fdb_entries: vec![],
            fabric: fabric_plan_with_binding(9, 3, 0),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(stale_binding.result.as_ref().unwrap().status, "error");
    assert_eq!(
        stale_binding.result.as_ref().unwrap().error_code,
        "STALE_GENERATION"
    );
    assert_eq!(
        executor.count("apply_fabric:"),
        1,
        "a stale binding generation must not be applied"
    );

    // Equal binding generation replays fine (the update path always
    // re-applies; the provider layer dedups).
    let equal = client
        .update_overlay(UpdateOverlayRequest {
            network_id: "net-bg".to_string(),
            vni: 100,
            vtep_endpoints: vec![],
            fdb_entries: vec![],
            fabric: fabric_plan_with_binding(9, 7, 0),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(equal.result.as_ref().unwrap().status, "OK");

    // A higher binding generation (a VNI re-bind) proceeds.
    let higher = client
        .update_overlay(UpdateOverlayRequest {
            network_id: "net-bg".to_string(),
            vni: 100,
            vtep_endpoints: vec![],
            fdb_entries: vec![],
            fabric: fabric_plan_with_binding(10, 9, 0),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(higher.result.as_ref().unwrap().status, "OK");
    assert!(executor
        .calls()
        .iter()
        .any(|c| c == "apply_fabric:net-bg:100:10:br-fab"));
}

// ---- m7: fabric → bridge-only re-ensure ------------------------------------

#[tokio::test]
async fn fabric_to_bridge_only_reensure_removes_fabric_overlay() {
    let _metrics = metric_test_lock().await;
    let executor = FabricExecutor::enabled();
    let (socket, _dir) = spawn_server(executor.clone()).await;
    let mut client = make_client(socket).await;

    let ensure = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(fabric_topology_spec("net-m7", 1)),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ensure.status, "OK");
    assert_eq!(executor.count("apply_fabric:"), 1);

    // Re-ensure the same network as bridge-only (no fabric plan, vni 0).
    let mut spec = fabric_topology_spec("net-m7", 1);
    spec.vni = 0;
    spec.overlay_type = OverlayType::OverlayNone as i32;
    spec.fabric = None;
    let reensure = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(spec),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(reensure.status, "OK");
    assert!(
        executor.calls().iter().any(|c| c == "remove_fabric:net-m7"),
        "the applied fabric object must be removed on bridge-only re-ensure"
    );
    // m7 residue cleanup: the fabric MTU (1380) and DHCP option 26 must
    // not survive the transition — the re-assert machinery must reset the
    // bridge/port MTUs to the bridge-only default and restart dnsmasq
    // without option 26 (`none` marks the reset invocation).
    assert!(
        executor
            .calls()
            .iter()
            .any(|c| c == "reassert_mtu:net-m7:none"),
        "the bridge-only re-ensure must reset the fabric tenant MTU residue, \
         got {:?}",
        executor.calls()
    );

    // The fabric fields are cleared from state: the overlay reports down.
    let status = client
        .get_overlay_status(GetOverlayStatusRequest {
            network_id: "net-m7".to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(status.vni, 0);
    assert!(!status.vxlan_interface_up);

    // A second bridge-only ensure is a plain idempotent replay: no further
    // fabric removal.
    let mut spec = fabric_topology_spec("net-m7", 1);
    spec.vni = 0;
    spec.overlay_type = OverlayType::OverlayNone as i32;
    spec.fabric = None;
    let again = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(spec),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(again.status, "OK");
    assert_eq!(executor.count("remove_fabric:"), 1);
}

// ---- m7 residue: fabric → bridge-only clears tenant_mtu state ---------------

#[tokio::test]
async fn fabric_to_bridge_only_reensure_clears_tenant_mtu_state() {
    let _metrics = metric_test_lock().await;
    let executor = FabricExecutor::enabled();
    let (service, table) = direct_service(executor.clone());

    let ensure = service
        .ensure_network_topology(TonicRequest::new(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(fabric_topology_spec("net-m7-state", 1)),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ensure.status, "OK");
    assert_eq!(
        table.get("net-m7-state").unwrap().tenant_mtu,
        Some(DEFAULT_TENANT_MTU)
    );

    // Re-ensure the same network as bridge-only (no fabric plan, vni 0):
    // the persisted state must record the bridge-only defaults — no
    // tenant MTU, no fabric fields — matching the reset datapath.
    let mut spec = fabric_topology_spec("net-m7-state", 1);
    spec.vni = 0;
    spec.overlay_type = OverlayType::OverlayNone as i32;
    spec.fabric = None;
    let reensure = service
        .ensure_network_topology(TonicRequest::new(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(spec),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(reensure.status, "OK");

    let state = table.get("net-m7-state").unwrap();
    assert_eq!(
        state.tenant_mtu, None,
        "the fabric tenant MTU must not survive a bridge-only re-ensure"
    );
    assert_eq!(state.vni, None);
    assert_eq!(state.fabric_plan_generation, None);
    assert_eq!(state.binding_generation, None);
    assert!(
        executor
            .calls()
            .iter()
            .any(|c| c == "reassert_mtu:net-m7-state:none"),
        "the MTU/dnsmasq reset must have run, got {:?}",
        executor.calls()
    );
}

// ---- input hardening: vni > 0 requires a fabric plan ------------------------

#[tokio::test]
async fn ensure_rejects_nonzero_vni_without_fabric_plan() {
    let executor = FabricExecutor::enabled();
    let (socket, _dir) = spawn_server(executor.clone()).await;
    let mut client = make_client(socket).await;

    // vni > 0 with no fabric plan (the legacy field): rejected in-band
    // before anything is applied or persisted — the legacy nolearning
    // VXLAN/FDB datapath was retired, so a nonzero VNI has no datapath.
    let mut spec = fabric_topology_spec("net-vni-nofab", 1);
    spec.fabric = None;
    let rejected = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(spec),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(rejected.status, "error");
    assert_eq!(rejected.error_code, "INVALID_ARGUMENT");
    assert!(
        rejected.human_summary.contains("fabric plan"),
        "the rejection must name the missing fabric plan, got: {}",
        rejected.human_summary
    );
    assert_eq!(
        executor.count("ensure:"),
        0,
        "nothing may reach the executor on a rejected spec"
    );

    // vni == 0 with no fabric plan: a plain bridge-only ensure is fine.
    let mut spec = fabric_topology_spec("net-vni-nofab", 1);
    spec.vni = 0;
    spec.overlay_type = OverlayType::OverlayNone as i32;
    spec.fabric = None;
    let bridge_only = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(spec),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(bridge_only.status, "OK");
    assert_eq!(executor.count("ensure:"), 1);
}

// ---- m8: tenant MTU change reaches ports and dnsmasq -----------------------

#[tokio::test]
async fn tenant_mtu_change_reasserts_mtu_on_running_topology() {
    let executor = FabricExecutor::enabled();
    let (socket, _dir) = spawn_server(executor.clone()).await;
    let mut client = make_client(socket).await;

    // First apply carries an explicit MTU of 1400. No prior state, so no
    // re-assert (the initial apply already sets bridge/veth/dnsmasq MTU).
    let mut spec = fabric_topology_spec("net-mtu-chg", 1);
    spec.fabric = fabric_plan(1, 1400);
    let first = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(spec),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(first.status, "OK");
    assert_eq!(executor.count("reassert_mtu:"), 0);

    // New generation, same MTU: applied, but no MTU re-assert.
    let mut spec = fabric_topology_spec("net-mtu-chg", 2);
    spec.fabric = fabric_plan(2, 1400);
    let same = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(spec),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(same.status, "OK");
    assert_eq!(
        executor.count("reassert_mtu:"),
        0,
        "an unchanged MTU must not re-assert"
    );

    // Same generation but a DIFFERENT explicit MTU: the replay must not be
    // short-circuited, and the MTU re-assert must fire with the new value
    // (bridge + enslaved ports + dnsmasq restart).
    let mut spec = fabric_topology_spec("net-mtu-chg", 2);
    spec.fabric = fabric_plan(2, 1420);
    let changed = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(spec),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(changed.status, "OK");
    assert!(
        executor
            .calls()
            .iter()
            .any(|c| c == "reassert_mtu:net-mtu-chg:1420"),
        "a changed tenant MTU must be re-asserted, got {:?}",
        executor.calls()
    );
}

// ---- M3: delete after a restart cleans orphaned fabric state ---------------

use chv_nwd_api::chv_nwd_api::network_service_server::NetworkService as _;

fn direct_service(
    executor: FabricExecutor,
) -> (NetworkServiceImpl<FabricExecutor>, Arc<TopologyTable>) {
    let table = Arc::new(TopologyTable::new());
    let service =
        NetworkServiceImpl::new(Arc::new(executor), table.clone(), Arc::new(Metrics::new()));
    (service, table)
}

#[tokio::test]
async fn delete_after_restart_cleans_orphaned_fabric_state() {
    let _metrics = metric_test_lock().await;
    let executor = FabricExecutor::enabled();
    let (service, table) = direct_service(executor.clone());

    let ensure = service
        .ensure_network_topology(TonicRequest::new(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(fabric_topology_spec("net-restart", 1)),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ensure.status, "OK");

    // Simulate an nwd restart: the in-memory topology table is wiped, but
    // the provider's durable ownership journal still holds the network.
    table.remove("net-restart");
    assert!(table.get("net-restart").is_none());
    assert!(
        matches!(
            executor.fabric_owned("net-restart").await.unwrap(),
            FabricOwnership::Owned
        ),
        "the ownership journal survives the restart"
    );

    let del = service
        .delete_network_topology(TonicRequest::new(DeleteNetworkTopologyRequest {
            meta: None,
            network_id: "net-restart".to_string(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(del.status, "OK");
    assert!(
        executor
            .calls()
            .iter()
            .any(|c| c == "remove_fabric:net-restart"),
        "the orphaned fabric state must be torn down, got {:?}",
        executor.calls()
    );

    // The teardown is idempotent: a second delete finds no ownership.
    let del2 = service
        .delete_network_topology(TonicRequest::new(DeleteNetworkTopologyRequest {
            meta: None,
            network_id: "net-restart".to_string(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(del2.status, "OK");
    assert_eq!(executor.count("remove_fabric:"), 1);
}

#[tokio::test]
async fn delete_after_restart_without_fabric_ownership_is_a_plain_noop() {
    let executor = FabricExecutor::enabled();
    let (service, table) = direct_service(executor.clone());

    service
        .ensure_network_topology(TonicRequest::new(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(fabric_topology_spec("net-nojournal", 1)),
        }))
        .await
        .unwrap();

    // Restart wipes the table AND the provider holds no entry (e.g. the
    // fabric network was already removed out-of-band).
    table.remove("net-nojournal");
    executor.owned.lock().unwrap().clear();

    let del = service
        .delete_network_topology(TonicRequest::new(DeleteNetworkTopologyRequest {
            meta: None,
            network_id: "net-nojournal".to_string(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(del.status, "OK");
    assert_eq!(
        executor.count("remove_fabric:"),
        0,
        "nothing is torn down when the provider holds no entry"
    );
}

#[tokio::test]
async fn delete_after_restart_reports_fabric_teardown_failure_loudly_but_succeeds() {
    // A fabric teardown failure with no local topology must NOT turn the
    // delete into an error (nothing local failed); the failure is loudly
    // visible via the warn log and the remove-failure metric.
    let executor = FabricExecutor::with_failing_removal();
    let counters = fabric_metrics_recorder::install();
    let _metrics = metric_test_lock().await;
    let (service, table) = direct_service(executor.clone());

    // Seed ownership directly (the mock's removal fails, so apply-then-wipe
    // is replaced by inserting into the ownership stand-in).
    executor
        .owned
        .lock()
        .unwrap()
        .insert("net-faildel".to_string());
    assert!(table.get("net-faildel").is_none());

    let before = counters
        .lock()
        .unwrap()
        .get("nwd_fabric_remove_total{result=failure}")
        .copied()
        .unwrap_or(0);
    let del = service
        .delete_network_topology(TonicRequest::new(DeleteNetworkTopologyRequest {
            meta: None,
            network_id: "net-faildel".to_string(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        del.status, "OK",
        "a fabric teardown failure must not fail a delete with no local topology"
    );
    assert!(
        executor
            .calls()
            .iter()
            .any(|c| c == "remove_fabric:net-faildel"),
        "the removal must have been attempted"
    );
    let after = counters
        .lock()
        .unwrap()
        .get("nwd_fabric_remove_total{result=failure}")
        .copied()
        .unwrap_or(0);
    assert!(
        after > before,
        "the failed removal must be counted in nwd_fabric_remove_total{{result=failure}}"
    );
}

// ---- n11 (round 2): truthful fabric-remove metric on the state-row path ----

#[tokio::test]
async fn delete_with_state_row_counts_fabric_failure_but_succeeds() {
    // The executor's fabric teardown is fail-open: a failing removal must
    // not fail the delete, but the metric must count the FAILURE (the old
    // approximation counted it as a success because the aggregate delete
    // returned Ok).
    let counters = fabric_metrics_recorder::install();
    let _metrics = metric_test_lock().await;
    let executor = FabricExecutor::with_failing_removal();
    let (service, table) = direct_service(executor.clone());

    let ensure = service
        .ensure_network_topology(TonicRequest::new(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(fabric_topology_spec("net-delfail", 1)),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ensure.status, "OK");

    let before_failure = counters
        .lock()
        .unwrap()
        .get("nwd_fabric_remove_total{result=failure}")
        .copied()
        .unwrap_or(0);
    let before_success = counters
        .lock()
        .unwrap()
        .get("nwd_fabric_remove_total{result=success}")
        .copied()
        .unwrap_or(0);

    let del = service
        .delete_network_topology(TonicRequest::new(DeleteNetworkTopologyRequest {
            meta: None,
            network_id: "net-delfail".to_string(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        del.status, "OK",
        "a fail-open fabric teardown failure must not fail the delete"
    );
    assert!(
        executor
            .calls()
            .iter()
            .any(|c| c == "remove_fabric:net-delfail"),
        "the removal must have been attempted"
    );

    let snapshot = counters.lock().unwrap().clone();
    let after_failure = snapshot
        .get("nwd_fabric_remove_total{result=failure}")
        .copied()
        .unwrap_or(0);
    let after_success = snapshot
        .get("nwd_fabric_remove_total{result=success}")
        .copied()
        .unwrap_or(0);
    assert!(
        after_failure > before_failure,
        "the failed fabric removal must be counted as a failure"
    );
    assert_eq!(
        after_success, before_success,
        "a failed fabric removal must NOT be counted as a success"
    );
    assert!(
        table.get("net-delfail").is_none(),
        "the local state row is removed (fail-open teardown semantics)"
    );
}

#[tokio::test]
async fn delete_with_local_teardown_error_does_not_proxy_fabric_metric() {
    // A LOCAL teardown error must not be proxied into the fabric-remove
    // metric (the old approximation counted it as a fabric failure even
    // though the fabric half had succeeded).
    let counters = fabric_metrics_recorder::install();
    let _metrics = metric_test_lock().await;
    let executor = FabricExecutor::with_failing_local_delete();
    let (service, _table) = direct_service(executor.clone());

    let ensure = service
        .ensure_network_topology(TonicRequest::new(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(fabric_topology_spec("net-localfail", 1)),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ensure.status, "OK");

    let before_failure = counters
        .lock()
        .unwrap()
        .get("nwd_fabric_remove_total{result=failure}")
        .copied()
        .unwrap_or(0);
    let before_success = counters
        .lock()
        .unwrap()
        .get("nwd_fabric_remove_total{result=success}")
        .copied()
        .unwrap_or(0);

    let del = service
        .delete_network_topology(TonicRequest::new(DeleteNetworkTopologyRequest {
            meta: None,
            network_id: "net-localfail".to_string(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(del.status, "error");
    assert_eq!(del.error_code, "INTERNAL_ERROR");
    // The fabric half ran first and succeeded...
    assert!(
        executor
            .calls()
            .iter()
            .any(|c| c == "remove_fabric:net-localfail"),
        "the fabric teardown must have run before the local failure"
    );
    // ...but neither fabric metric moves: the aggregate error is a local
    // teardown failure, and the fabric-half outcome is not observable
    // through it (counting it as a fabric failure was the removed
    // approximation).
    let snapshot = counters.lock().unwrap().clone();
    assert_eq!(
        snapshot
            .get("nwd_fabric_remove_total{result=failure}")
            .copied()
            .unwrap_or(0),
        before_failure,
        "a local teardown error must not count as a fabric failure"
    );
    assert_eq!(
        snapshot
            .get("nwd_fabric_remove_total{result=success}")
            .copied()
            .unwrap_or(0),
        before_success
    );
}

// ---- M3 observability (round 2): disabled provider + no state row warns -----

#[tokio::test]
async fn delete_without_state_row_and_disabled_provider_warns() {
    // Residue case: the fabric overlay may have been applied before an
    // nwd restart that came up with the fabric provider disabled — the
    // delete with no state row must not be a SILENT no-op: the ownership
    // probe is consulted and a warning is emitted.
    let executor = FabricExecutor::disabled();
    let (service, _table) = direct_service(executor.clone());
    let warnings = warn_capture::WarnCollector::default();
    let _guard = tracing::subscriber::set_default(warnings.clone());

    let del = service
        .delete_network_topology(TonicRequest::new(DeleteNetworkTopologyRequest {
            meta: None,
            network_id: "net-ghost-fab".to_string(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        del.status, "OK",
        "nothing local failed; the RPC result stays Ok"
    );
    assert!(
        executor
            .calls()
            .iter()
            .any(|c| c == "fabric_owned:net-ghost-fab"),
        "the ownership probe must be consulted"
    );
    assert_eq!(
        executor.count("remove_fabric:"),
        0,
        "nothing can be torn down with the provider disabled"
    );
    assert!(
        warnings
            .messages()
            .iter()
            .any(|m| m.contains("fabric provider disabled")),
        "the unobservable residue must be warned about, got {:?}",
        warnings.messages()
    );
}

// ---- m9: get_overlay_status error propagation ------------------------------

#[tokio::test]
async fn get_overlay_status_propagates_fabric_errors() {
    let executor = FabricExecutor::with_overlay_status_error();
    let (socket, _dir) = spawn_server(executor.clone()).await;
    let mut client = make_client(socket).await;

    let ensure = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(fabric_topology_spec("net-obs-err", 1)),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ensure.status, "OK");

    // A provider error must surface, not be masked as a "down" status.
    let status = client
        .get_overlay_status(GetOverlayStatusRequest {
            network_id: "net-obs-err".to_string(),
        })
        .await;
    assert!(
        status.is_err(),
        "a fabric provider error must not be swallowed into a down status"
    );
    let err = status.unwrap_err();
    assert_eq!(
        err.code(),
        tonic::Code::Internal,
        "the structured ChvError must map to an internal status, got: {}",
        err.message()
    );
}

#[tokio::test]
async fn get_overlay_status_reports_truthful_down_when_not_owned() {
    // State row exists (fabric fields set) but the provider holds no
    // ownership entry: truthful "down", not an error.
    let executor = FabricExecutor::enabled();
    let (service, table) = direct_service(executor.clone());
    table.upsert(TopologyState {
        network_id: "net-ghost".to_string(),
        tenant_id: "t1".to_string(),
        bridge_name: "br-fab".to_string(),
        namespace_name: "ns-fab".to_string(),
        subnet_cidr: "10.0.50.0/24".to_string(),
        gateway_ip: "10.0.50.1".to_string(),
        runtime_status: "ensured".to_string(),
        vni: Some(100),
        tenant_mtu: Some(1380),
        fabric_plan_generation: Some(1),
        binding_generation: Some(1),
    });

    let status = service
        .get_overlay_status(TonicRequest::new(GetOverlayStatusRequest {
            network_id: "net-ghost".to_string(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(status.vni, 100);
    assert!(!status.vxlan_interface_up, "not owned must report down");
    assert_eq!(status.fdb_entry_count, 0);
}

#[tokio::test]
async fn get_overlay_status_reports_down_when_provider_disabled() {
    // The fabric provider was disabled in configuration after the overlay
    // was applied: the overlay state is unobservable, reported as down —
    // not as an error.
    let executor = FabricExecutor::disabled();
    let (service, table) = direct_service(executor.clone());
    table.upsert(TopologyState {
        network_id: "net-off-obs".to_string(),
        tenant_id: "t1".to_string(),
        bridge_name: "br-fab".to_string(),
        namespace_name: "ns-fab".to_string(),
        subnet_cidr: "10.0.50.0/24".to_string(),
        gateway_ip: "10.0.50.1".to_string(),
        runtime_status: "ensured".to_string(),
        vni: Some(100),
        tenant_mtu: Some(1380),
        fabric_plan_generation: Some(1),
        binding_generation: Some(1),
    });

    let status = service
        .get_overlay_status(TonicRequest::new(GetOverlayStatusRequest {
            network_id: "net-off-obs".to_string(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(status.vni, 100);
    assert!(
        !status.vxlan_interface_up,
        "a disabled provider must report down, not an error"
    );
}

// ---- warn capture (test-only tracing subscriber) ----------------------------

/// Minimal `tracing` subscriber that records the messages of WARN-level
/// events, so handler-level tests can assert a warning is observable
/// (M3 round 2). Installed per-thread with
/// `tracing::subscriber::set_default`, which the current-thread
/// `#[tokio::test]` runtime keeps alive across every await point.
mod warn_capture {
    use std::sync::{Arc, Mutex as StdMutex};
    use tracing::field::Visit;
    use tracing::span::{Attributes, Id};
    use tracing::{Event, Metadata};

    #[derive(Clone, Default)]
    pub struct WarnCollector {
        warnings: Arc<StdMutex<Vec<String>>>,
    }

    impl WarnCollector {
        pub fn messages(&self) -> Vec<String> {
            self.warnings.lock().unwrap().clone()
        }
    }

    struct MessageVisitor(Option<String>);

    impl Visit for MessageVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0 = Some(format!("{:?}", value));
            }
        }
    }

    impl tracing::Subscriber for WarnCollector {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            *metadata.level() == tracing::Level::WARN
        }

        fn new_span(&self, _span: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }

        fn record(&self, _span: &Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut visitor = MessageVisitor(None);
            event.record(&mut visitor);
            if let Some(message) = visitor.0 {
                self.warnings.lock().unwrap().push(message);
            }
        }

        fn enter(&self, _span: &Id) {}

        fn exit(&self, _span: &Id) {}
    }
}

// ---- n11: fabric datapath metrics -------------------------------------------

mod fabric_metrics_recorder {
    use metrics::{
        Counter, CounterFn, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit,
    };
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex as StdMutex, OnceLock};

    struct CapturingCounter {
        map: Arc<StdMutex<HashMap<String, u64>>>,
        key: String,
    }

    impl CounterFn for CapturingCounter {
        fn increment(&self, value: u64) {
            *self
                .map
                .lock()
                .unwrap()
                .entry(self.key.clone())
                .or_insert(0) += value;
        }

        fn absolute(&self, value: u64) {
            self.map.lock().unwrap().insert(self.key.clone(), value);
        }
    }

    struct CapturingRecorder {
        counters: Arc<StdMutex<HashMap<String, u64>>>,
    }

    impl Recorder for CapturingRecorder {
        fn describe_counter(&self, _key: KeyName, _unit: Option<Unit>, _desc: SharedString) {}
        fn describe_gauge(&self, _key: KeyName, _unit: Option<Unit>, _desc: SharedString) {}
        fn describe_histogram(&self, _key: KeyName, _unit: Option<Unit>, _desc: SharedString) {}

        fn register_counter(&self, key: &Key, _metadata: &Metadata<'_>) -> Counter {
            let labels = key
                .labels()
                .map(|l| format!("{}={}", l.key(), l.value()))
                .collect::<Vec<_>>()
                .join(",");
            let entry = format!("{}{{{}}}", key.name(), labels);
            Counter::from_arc(Arc::new(CapturingCounter {
                map: self.counters.clone(),
                key: entry,
            }))
        }

        fn register_gauge(&self, _key: &Key, _metadata: &Metadata<'_>) -> Gauge {
            Gauge::noop()
        }

        fn register_histogram(&self, _key: &Key, _metadata: &Metadata<'_>) -> Histogram {
            Histogram::noop()
        }
    }

    static COUNTERS: OnceLock<Arc<StdMutex<HashMap<String, u64>>>> = OnceLock::new();

    /// Install the capturing recorder once per test process and return the
    /// shared counter map. Other tests in this binary may also increment
    /// counters once the recorder is live, so assertions use >= bounds.
    pub fn install() -> Arc<StdMutex<HashMap<String, u64>>> {
        COUNTERS
            .get_or_init(|| {
                let counters: Arc<StdMutex<HashMap<String, u64>>> =
                    Arc::new(StdMutex::new(HashMap::new()));
                // A failure here means another recorder is already
                // installed in this process; the returned map is still the
                // one that recorder captures into (or stays empty, which
                // fails the assertions loudly rather than silently).
                let _ = metrics::set_global_recorder(CapturingRecorder {
                    counters: counters.clone(),
                });
                counters
            })
            .clone()
    }
}

#[tokio::test]
async fn fabric_apply_and_remove_are_counted_in_metrics() {
    let counters = fabric_metrics_recorder::install();
    let _metrics = metric_test_lock().await;
    let executor = FabricExecutor::enabled();
    let (socket, _dir) = spawn_server(executor.clone()).await;
    let mut client = make_client(socket).await;

    let ensure = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(fabric_topology_spec("net-metrics", 1)),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ensure.status, "OK");

    let del = client
        .delete_network_topology(DeleteNetworkTopologyRequest {
            meta: None,
            network_id: "net-metrics".to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(del.status, "OK");

    let snapshot = counters.lock().unwrap().clone();
    let apply_success = snapshot
        .get("nwd_fabric_apply_total{result=success}")
        .copied()
        .unwrap_or(0);
    assert!(
        apply_success >= 1,
        "nwd_fabric_apply_total{{result=success}} must be counted, got {snapshot:?}"
    );
    let remove_success = snapshot
        .get("nwd_fabric_remove_total{result=success}")
        .copied()
        .unwrap_or(0);
    assert!(
        remove_success >= 1,
        "nwd_fabric_remove_total{{result=success}} must be counted, got {snapshot:?}"
    );
}
