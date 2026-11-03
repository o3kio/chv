use async_trait::async_trait;
use chv_errors::ChvError;
use chv_nwd_api::chv_nwd_api::{
    network_service_client::NetworkServiceClient, AttachVmNicRequest, DeleteNetworkTopologyRequest,
    DetachVmNicRequest, DhcpScope, DnsScope, EnsureDhcpScopeRequest, EnsureDnsScopeRequest,
    EnsureNetworkTopologyRequest, ExposeServiceRequest, ExposureSpec, FabricPlan, FirewallPolicy,
    ListNamespaceStateRequest, NatPolicy, NetworkHealthRequest, NicSpec, SetFirewallPolicyRequest,
    SetNatPolicyRequest, TopologySpec, WithdrawServiceExposureRequest,
};
use chv_nwd_core::executor::{
    DeleteOutcome, FabricOwnership, NetworkExecutor, OverlayStatusInfo, TopologyApplyResult,
};
use chv_nwd_core::fabric::{AppliedFabric, ApplyReport, FabricIdentity};
use chv_nwd_core::{NetworkServer, TopologyState};
use chv_observability::Metrics;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UnixStream;
use tonic::transport::{Endpoint, Uri};
use tower::service_fn;

struct MockExecutor;

#[async_trait]
impl NetworkExecutor for MockExecutor {
    async fn ensure_topology(
        &self,
        spec: &chv_nwd_api::chv_nwd_api::TopologySpec,
    ) -> Result<TopologyApplyResult, ChvError> {
        Ok(TopologyApplyResult {
            namespace_handle: spec.namespace_name.clone(),
            bridge_handle: spec.bridge_name.clone(),
            tenant_mtu: None,
            fabric_plan_generation: None,
            binding_generation: None,
        })
    }

    async fn delete_topology(
        &self,
        _network_id: &str,
        _state: &TopologyState,
    ) -> Result<DeleteOutcome, ChvError> {
        Ok(DeleteOutcome {
            fabric_removed: None,
        })
    }

    async fn delete_local_topology_by_derived_names(&self, _network_id: &str) {}

    async fn health(&self, _network_id: &str, _state: &TopologyState) -> Result<String, ChvError> {
        Ok("healthy".to_string())
    }

    async fn attach_vm_nic(
        &self,
        network_id: &str,
        nic_id: &str,
        _vm_id: &str,
        _bridge_name: &str,
        _tenant_mtu: Option<u32>,
        _mac_address: &str,
        _ip_address: &str,
    ) -> Result<(String, String), ChvError> {
        Ok((format!("ns-{}", network_id), format!("tap-{}", nic_id)))
    }

    async fn detach_vm_nic(
        &self,
        _nic_id: &str,
        _ownership: chv_common::AttachmentOwnership,
    ) -> Result<(), ChvError> {
        Ok(())
    }

    async fn apply_fabric_overlay(
        &self,
        network_id: &str,
        _vni: u32,
        _plan: &FabricPlan,
        _bridge_name: &str,
    ) -> Result<AppliedFabric, ChvError> {
        Ok(AppliedFabric {
            report: ApplyReport::default(),
            plan_generation: 1,
            binding_generation: 1,
            tenant_mtu: 1380,
            consumer_veth: format!("chv-{}-a", network_id),
        })
    }

    async fn remove_fabric_overlay(&self, _network_id: &str) -> Result<(), ChvError> {
        Ok(())
    }

    async fn fabric_owned(&self, _network_id: &str) -> Result<FabricOwnership, ChvError> {
        Ok(FabricOwnership::NotOwned)
    }

    async fn fabric_identity(&self) -> Result<FabricIdentity, ChvError> {
        Ok(FabricIdentity {
            public_key: "K7XbF9cV2mQpT3nZ8sL4dW6yH1jR5uA0eG9iO2pS7kM=".to_string(),
            underlay_mtu: 1500,
        })
    }

    async fn fabric_overlay_status(
        &self,
        _network_id: &str,
    ) -> Result<OverlayStatusInfo, ChvError> {
        Ok(OverlayStatusInfo {
            vxlan_interface_up: true,
            fdb_entry_count: 1,
        })
    }

    async fn reassert_tenant_mtu(
        &self,
        _network_id: &str,
        _bridge_name: &str,
        _subnet_cidr: &str,
        _gateway_ip: &str,
        _tenant_mtu: Option<u32>,
    ) -> Result<(), ChvError> {
        Ok(())
    }

    async fn set_firewall_policy(
        &self,
        _network_id: &str,
        _policy_version: &str,
        _policy_json: &[u8],
        _bridge_name: &str,
    ) -> Result<(), ChvError> {
        Ok(())
    }

    async fn set_nat_policy(
        &self,
        _network_id: &str,
        _policy_version: &str,
        _policy_json: &[u8],
        _bridge_name: &str,
    ) -> Result<(), ChvError> {
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
}

/// Wraps `MockExecutor` and records `set_firewall_policy` / `set_nat_policy`
/// invocations so tests can assert that VM-NIC attach refreshes the CHV-owned
/// guard scope with the previously applied policy.
#[derive(Clone)]
struct RecordingExecutor {
    calls: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait]
impl NetworkExecutor for RecordingExecutor {
    async fn ensure_topology(
        &self,
        spec: &chv_nwd_api::chv_nwd_api::TopologySpec,
    ) -> Result<TopologyApplyResult, ChvError> {
        MockExecutor.ensure_topology(spec).await
    }

    async fn delete_topology(
        &self,
        network_id: &str,
        _state: &TopologyState,
    ) -> Result<DeleteOutcome, ChvError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("state_teardown:{network_id}"));
        Ok(DeleteOutcome {
            fabric_removed: None,
        })
    }

    async fn delete_local_topology_by_derived_names(&self, network_id: &str) {
        self.calls
            .lock()
            .unwrap()
            .push(format!("derived_teardown:{network_id}"));
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
        _tenant_mtu: Option<u32>,
        _mac_address: &str,
        _ip_address: &str,
    ) -> Result<(String, String), ChvError> {
        Ok((format!("ns-{}", network_id), format!("tap-{}", nic_id)))
    }

    async fn detach_vm_nic(
        &self,
        _nic_id: &str,
        _ownership: chv_common::AttachmentOwnership,
    ) -> Result<(), ChvError> {
        Ok(())
    }

    async fn apply_fabric_overlay(
        &self,
        network_id: &str,
        _vni: u32,
        _plan: &FabricPlan,
        _bridge_name: &str,
    ) -> Result<AppliedFabric, ChvError> {
        Ok(AppliedFabric {
            report: ApplyReport::default(),
            plan_generation: 1,
            binding_generation: 1,
            tenant_mtu: 1380,
            consumer_veth: format!("chv-{}-a", network_id),
        })
    }

    async fn remove_fabric_overlay(&self, _network_id: &str) -> Result<(), ChvError> {
        Ok(())
    }

    async fn fabric_owned(&self, _network_id: &str) -> Result<FabricOwnership, ChvError> {
        Ok(FabricOwnership::NotOwned)
    }

    async fn fabric_identity(&self) -> Result<FabricIdentity, ChvError> {
        Ok(FabricIdentity {
            public_key: "K7XbF9cV2mQpT3nZ8sL4dW6yH1jR5uA0eG9iO2pS7kM=".to_string(),
            underlay_mtu: 1500,
        })
    }

    async fn fabric_overlay_status(
        &self,
        _network_id: &str,
    ) -> Result<OverlayStatusInfo, ChvError> {
        Ok(OverlayStatusInfo {
            vxlan_interface_up: true,
            fdb_entry_count: 1,
        })
    }

    async fn reassert_tenant_mtu(
        &self,
        _network_id: &str,
        _bridge_name: &str,
        _subnet_cidr: &str,
        _gateway_ip: &str,
        _tenant_mtu: Option<u32>,
    ) -> Result<(), ChvError> {
        Ok(())
    }

    async fn set_firewall_policy(
        &self,
        network_id: &str,
        _policy_version: &str,
        _policy_json: &[u8],
        bridge_name: &str,
    ) -> Result<(), ChvError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("fw:{network_id}:{bridge_name}"));
        Ok(())
    }

    async fn set_nat_policy(
        &self,
        network_id: &str,
        _policy_version: &str,
        _policy_json: &[u8],
        bridge_name: &str,
    ) -> Result<(), ChvError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("nat:{network_id}:{bridge_name}"));
        Ok(())
    }

    async fn ensure_dhcp_scope(
        &self,
        network_id: &str,
        cidr: &str,
        range_start: &str,
        range_end: &str,
        dns_servers: &[String],
    ) -> Result<(), ChvError> {
        MockExecutor
            .ensure_dhcp_scope(network_id, cidr, range_start, range_end, dns_servers)
            .await
    }

    async fn ensure_dns_scope(
        &self,
        network_id: &str,
        forwarders: &[&str],
        static_records: &std::collections::HashMap<String, String>,
    ) -> Result<(), ChvError> {
        MockExecutor
            .ensure_dns_scope(network_id, forwarders, static_records)
            .await
    }

    async fn expose_service(
        &self,
        network_id: &str,
        exposure_id: &str,
        protocol: &str,
        external_port: u32,
        target_ip: &str,
        target_port: u32,
        mode: &str,
    ) -> Result<(), ChvError> {
        MockExecutor
            .expose_service(
                network_id,
                exposure_id,
                protocol,
                external_port,
                target_ip,
                target_port,
                mode,
            )
            .await
    }

    async fn withdraw_service_exposure(
        &self,
        network_id: &str,
        exposure_id: &str,
    ) -> Result<(), ChvError> {
        MockExecutor
            .withdraw_service_exposure(network_id, exposure_id)
            .await
    }

    async fn send_gratuitous_arp(
        &self,
        namespace: &str,
        bridge_name: &str,
        vm_ip: &str,
    ) -> Result<(), ChvError> {
        MockExecutor
            .send_gratuitous_arp(namespace, bridge_name, vm_ip)
            .await
    }
}

async fn make_channel(socket: PathBuf) -> tonic::transport::Channel {
    Endpoint::try_from("http://[::]:50051")
        .unwrap()
        .connect_with_connector(service_fn(move |_: Uri| {
            let s = socket.clone();
            async move {
                let stream = UnixStream::connect(s).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::tokio::TokioIo::new(stream))
            }
        }))
        .await
        .unwrap()
}

async fn make_client(socket: PathBuf) -> NetworkServiceClient<tonic::transport::Channel> {
    NetworkServiceClient::new(make_channel(socket).await)
}

/// **gRPC health endpoint answers after the tonic 0.14 stack bump**
/// (#235): the server composes `health + NetworkService` through
/// tonic 0.14's axum-backed `Router` (`Server::add_service`), and the
/// health reporter's methods changed to `&self` in tonic-health 0.14 —
/// startup alone does not prove that the composed health service still
/// *answers*. This pins both the overall (`""`) status and the
/// service-specific status the server sets
/// (`chv.node.nwd.v1.NetworkService`) as `SERVING` over the same UDS
/// endpoint the daemon tests already exercise.
#[tokio::test]
async fn grpc_health_endpoint_reports_serving() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nwd.sock");

    let server = NetworkServer::new(MockExecutor, Metrics::new());
    let socket_clone = socket.clone();
    tokio::spawn(async move {
        server.serve(&socket_clone).await.ok();
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let channel = make_channel(socket).await;
    let mut health = tonic_health::pb::health_client::HealthClient::new(channel);

    use tonic_health::pb::{
        health_check_response::ServingStatus, HealthCheckRequest, HealthCheckResponse,
    };

    // Overall health (the empty service name).
    let resp: HealthCheckResponse = health
        .check(HealthCheckRequest {
            service: String::new(),
        })
        .await
        .expect("overall health Check must answer")
        .into_inner();
    assert_eq!(
        resp.status,
        ServingStatus::Serving as i32,
        "overall health must be SERVING"
    );

    // The service-specific status the server sets via
    // `set_serving::<NetworkServiceServer<..>>()`.
    let resp: HealthCheckResponse = health
        .check(HealthCheckRequest {
            service: "chv.node.nwd.v1.NetworkService".to_string(),
        })
        .await
        .expect("service-specific health Check must answer")
        .into_inner();
    assert_eq!(
        resp.status,
        ServingStatus::Serving as i32,
        "chv.node.nwd.v1.NetworkService health must be SERVING"
    );
}

/// **nwd records gRPC RED metrics (#657 review M3)**: the server builder must
/// compose `GrpcMetricsLayer` like stord/agent/control plane. A health RPC is
/// the cheapest call that proves the layer is installed and records the
/// terminal status from the response headers.
#[tokio::test]
async fn grpc_metrics_layer_records_health_check() {
    chv_observability::install_prometheus_recorder().expect("install prometheus recorder");

    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nwd.sock");

    let server = NetworkServer::new(MockExecutor, Metrics::new());
    let socket_clone = socket.clone();
    tokio::spawn(async move {
        server.serve(&socket_clone).await.ok();
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let channel = make_channel(socket).await;
    let mut health = tonic_health::pb::health_client::HealthClient::new(channel);
    health
        .check(tonic_health::pb::HealthCheckRequest {
            service: String::new(),
        })
        .await
        .expect("health Check must answer");

    let text = chv_observability::prometheus_handle()
        .expect("prometheus recorder installed")
        .render();
    assert!(
        text.lines().any(|line| {
            line.starts_with("chv_grpc_server_requests_total{")
                && line.contains("service=\"grpc.health.v1.Health\"")
                && line.contains("method=\"Check\"")
                && line.contains("grpc_status=\"0\"")
        }),
        "nwd must install GrpcMetricsLayer; exposition was:\n{text}"
    );
}

#[tokio::test]
async fn ensure_and_delete_topology_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nwd.sock");

    let server = NetworkServer::new(MockExecutor, Metrics::new());
    let socket_clone = socket.clone();
    tokio::spawn(async move {
        server.serve(&socket_clone).await.ok();
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut client = make_client(socket).await;

    let req = EnsureNetworkTopologyRequest {
        meta: None,
        topology: Some(TopologySpec {
            network_id: "net-1".to_string(),
            tenant_id: "t1".to_string(),
            bridge_name: "br-net1".to_string(),
            namespace_name: "ns-net1".to_string(),
            subnet_cidr: "10.0.1.0/24".to_string(),
            gateway_ip: "10.0.1.1".to_string(),
            options: Default::default(),
            vni: 0,
            vtep_endpoints: vec![],
            overlay_type: 0,
            fabric: None,
        }),
    };

    // First ensure
    let resp1 = client
        .ensure_network_topology(req.clone())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp1.status, "OK");

    // Idempotent ensure
    let resp2 = client
        .ensure_network_topology(req.clone())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp2.status, "OK");

    // Health
    let health = client
        .get_network_health(NetworkHealthRequest {
            network_id: "net-1".to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(health.network_id, "net-1");
    assert_eq!(health.health_status, "healthy");

    // List
    let list = client
        .list_namespace_state(ListNamespaceStateRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(list.items.len(), 1);
    assert_eq!(list.items[0].network_id, "net-1");

    // Delete
    let del1 = client
        .delete_network_topology(DeleteNetworkTopologyRequest {
            meta: None,
            network_id: "net-1".to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(del1.status, "OK");

    // Idempotent delete
    let del2 = client
        .delete_network_topology(DeleteNetworkTopologyRequest {
            meta: None,
            network_id: "net-1".to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(del2.status, "OK");

    // Health after delete
    let health2 = client
        .get_network_health(NetworkHealthRequest {
            network_id: "net-1".to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(health2.health_status, "unknown");
}

#[tokio::test]
async fn all_network_handlers_smoke() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nwd.sock");

    let server = NetworkServer::new(MockExecutor, Metrics::new());
    let socket_clone = socket.clone();
    tokio::spawn(async move {
        server.serve(&socket_clone).await.ok();
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut client = make_client(socket).await;

    // Ensure topology first
    let ensure = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(TopologySpec {
                network_id: "net-1".to_string(),
                tenant_id: "t1".to_string(),
                bridge_name: "br-net1".to_string(),
                namespace_name: "ns-net1".to_string(),
                subnet_cidr: "10.0.1.0/24".to_string(),
                gateway_ip: "10.0.1.1".to_string(),
                options: Default::default(),
                vni: 0,
                vtep_endpoints: vec![],
                overlay_type: 0,
                fabric: None,
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ensure.status, "OK");

    // attach_vm_nic
    let attach = client
        .attach_vm_nic(AttachVmNicRequest {
            meta: None,
            nic: Some(NicSpec {
                nic_id: "nic-1".to_string(),
                vm_id: "vm-1".to_string(),
                network_id: "net-1".to_string(),
                mac_address: "02:00:00:00:00:01".to_string(),
                tap_name: "tap-nic-1".to_string(),
                ip_address: "10.0.1.10".to_string(),
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(attach.result.as_ref().unwrap().status, "OK");
    assert_eq!(attach.namespace_handle, "ns-net-1");
    assert_eq!(attach.tap_handle, "tap-nic-1");

    // detach_vm_nic
    let detach = client
        .detach_vm_nic(DetachVmNicRequest {
            meta: None,
            vm_id: "vm-1".to_string(),
            nic_id: "nic-1".to_string(),
            network_id: "net-1".to_string(),
            vm_mac: "02:00:00:00:00:01".to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(detach.status, "OK");

    // set_firewall_policy
    let fw = client
        .set_firewall_policy(SetFirewallPolicyRequest {
            meta: None,
            network_id: "net-1".to_string(),
            policy: Some(FirewallPolicy {
                policy_version: "v1".to_string(),
                policy_json: b"{}".to_vec(),
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(fw.status, "OK");

    // set_nat_policy
    let nat = client
        .set_nat_policy(chv_nwd_api::chv_nwd_api::SetNatPolicyRequest {
            meta: None,
            network_id: "net-1".to_string(),
            policy: Some(NatPolicy {
                policy_version: "v1".to_string(),
                policy_json: b"{}".to_vec(),
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(nat.status, "OK");

    // ensure_dhcp_scope
    let dhcp = client
        .ensure_dhcp_scope(EnsureDhcpScopeRequest {
            meta: None,
            scope: Some(DhcpScope {
                network_id: "net-1".to_string(),
                cidr: "10.0.1.0/24".to_string(),
                range_start: "10.0.1.50".to_string(),
                range_end: "10.0.1.100".to_string(),
                dns_servers: vec!["10.0.1.1".to_string()],
                mtu: 0,
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(dhcp.status, "OK");

    // ensure_dns_scope
    let dns = client
        .ensure_dns_scope(EnsureDnsScopeRequest {
            meta: None,
            scope: Some(DnsScope {
                network_id: "net-1".to_string(),
                forwarders: vec!["8.8.8.8".to_string()],
                static_records: Default::default(),
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(dns.status, "OK");

    // expose_service
    let expose = client
        .expose_service(ExposeServiceRequest {
            meta: None,
            exposure: Some(ExposureSpec {
                network_id: "net-1".to_string(),
                exposure_id: "exp-1".to_string(),
                protocol: "tcp".to_string(),
                external_port: 8080,
                target_ip: "10.0.1.10".to_string(),
                target_port: 80,
                mode: "dnat".to_string(),
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(expose.status, "OK");

    // withdraw_service_exposure
    let withdraw = client
        .withdraw_service_exposure(WithdrawServiceExposureRequest {
            meta: None,
            exposure_id: "exp-1".to_string(),
            network_id: "net-1".to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(withdraw.status, "OK");

    // Cleanup
    let del = client
        .delete_network_topology(DeleteNetworkTopologyRequest {
            meta: None,
            network_id: "net-1".to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(del.status, "OK");
}

#[tokio::test]
async fn attach_vm_nic_missing_topology_returns_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nwd.sock");

    let server = NetworkServer::new(MockExecutor, Metrics::new());
    let socket_clone = socket.clone();
    tokio::spawn(async move {
        server.serve(&socket_clone).await.ok();
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut client = make_client(socket).await;

    let result = client
        .attach_vm_nic(AttachVmNicRequest {
            meta: None,
            nic: Some(NicSpec {
                nic_id: "nic-1".to_string(),
                vm_id: "vm-1".to_string(),
                network_id: "net-not-ensured".to_string(),
                mac_address: "02:00:00:00:00:01".to_string(),
                tap_name: "tap-nic-1".to_string(),
                ip_address: "10.0.1.10".to_string(),
            }),
        })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(result.result.as_ref().unwrap().status, "error");
    assert_eq!(result.result.as_ref().unwrap().error_code, "NOT_FOUND");
}

#[tokio::test]
async fn firewall_nat_and_exposure_smoke() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nwd.sock");

    let server = NetworkServer::new(MockExecutor, Metrics::new());
    let socket_clone = socket.clone();
    tokio::spawn(async move {
        server.serve(&socket_clone).await.ok();
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut client = make_client(socket).await;

    // Ensure topology first
    let _ = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(TopologySpec {
                network_id: "net-fw".to_string(),
                tenant_id: "t1".to_string(),
                bridge_name: "br-fw".to_string(),
                namespace_name: "ns-fw".to_string(),
                subnet_cidr: "10.0.99.0/24".to_string(),
                gateway_ip: "10.0.99.1".to_string(),
                options: Default::default(),
                vni: 0,
                vtep_endpoints: vec![],
                overlay_type: 0,
                fabric: None,
            }),
        })
        .await
        .unwrap()
        .into_inner();

    let fw = client
        .set_firewall_policy(SetFirewallPolicyRequest {
            meta: None,
            network_id: "net-fw".to_string(),
            policy: Some(FirewallPolicy {
                policy_version: "v1".to_string(),
                policy_json: b"{}".to_vec(),
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(fw.status, "OK");

    let nat = client
        .set_nat_policy(chv_nwd_api::chv_nwd_api::SetNatPolicyRequest {
            meta: None,
            network_id: "net-fw".to_string(),
            policy: Some(NatPolicy {
                policy_version: "v1".to_string(),
                policy_json: b"{}".to_vec(),
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(nat.status, "OK");

    let exp = client
        .expose_service(ExposeServiceRequest {
            meta: None,
            exposure: Some(chv_nwd_api::chv_nwd_api::ExposureSpec {
                network_id: "net-fw".to_string(),
                exposure_id: "exp1".to_string(),
                protocol: "tcp".to_string(),
                external_port: 8080,
                target_ip: "10.0.99.10".to_string(),
                target_port: 80,
                mode: "dnat".to_string(),
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(exp.status, "OK");

    let wd = client
        .withdraw_service_exposure(WithdrawServiceExposureRequest {
            meta: None,
            exposure_id: "exp1".to_string(),
            network_id: "net-fw".to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(wd.status, "OK");
}

#[tokio::test]
async fn attach_refreshes_policy_guard_scope() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nwd.sock");
    let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let rec = RecordingExecutor {
        calls: calls.clone(),
    };
    let server = NetworkServer::new(rec, Metrics::new());
    let socket_clone = socket.clone();
    tokio::spawn(async move {
        server.serve(&socket_clone).await.ok();
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut client = make_client(socket).await;

    // Ensure topology with the authoritative bridge "br-rec".
    let ensure = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(TopologySpec {
                network_id: "net-rec".to_string(),
                tenant_id: "t1".to_string(),
                bridge_name: "br-rec".to_string(),
                namespace_name: "ns-rec".to_string(),
                subnet_cidr: "10.0.7.0/24".to_string(),
                gateway_ip: "10.0.7.1".to_string(),
                options: Default::default(),
                vni: 0,
                vtep_endpoints: vec![],
                overlay_type: 0,
                fabric: None,
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ensure.status, "OK");

    // Apply firewall+nat once -> both are recorded (stored) exactly once.
    let fw = client
        .set_firewall_policy(SetFirewallPolicyRequest {
            meta: None,
            network_id: "net-rec".to_string(),
            policy: Some(FirewallPolicy {
                policy_version: "v1".to_string(),
                policy_json: b"[]".to_vec(),
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(fw.status, "OK");

    let nat = client
        .set_nat_policy(chv_nwd_api::chv_nwd_api::SetNatPolicyRequest {
            meta: None,
            network_id: "net-rec".to_string(),
            policy: Some(NatPolicy {
                policy_version: "v1".to_string(),
                policy_json: b"[]".to_vec(),
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(nat.status, "OK");
    assert_eq!(calls.lock().unwrap().len(), 2);

    // Attach a NIC -> the apply-time guard set can no longer see the new
    // interface, so the handler must re-assert (refresh) the stored policy
    // against the authoritative topology bridge. This proves a NIC attached
    // after policy application is still inside the CHV default-deny boundary.
    let attach = client
        .attach_vm_nic(AttachVmNicRequest {
            meta: None,
            nic: Some(NicSpec {
                nic_id: "nic-rec".to_string(),
                vm_id: "vm-rec".to_string(),
                network_id: "net-rec".to_string(),
                mac_address: "02:00:00:00:00:0a".to_string(),
                tap_name: "tap-rec".to_string(),
                ip_address: "10.0.7.10".to_string(),
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(attach.result.as_ref().unwrap().status, "OK");

    let recorded = calls.lock().unwrap();
    assert_eq!(
        recorded.len(),
        4,
        "attach must refresh both firewall and nat guard scopes"
    );
    assert_eq!(recorded[2], "fw:net-rec:br-rec");
    assert_eq!(recorded[3], "nat:net-rec:br-rec");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_fw_nat_applies_persist_both_halves() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nwd.sock");
    let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let rec = RecordingExecutor {
        calls: calls.clone(),
    };
    let server = NetworkServer::new(rec, Metrics::new());
    let socket_clone = socket.clone();
    tokio::spawn(async move {
        server.serve(&socket_clone).await.ok();
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut client = make_client(socket).await;

    let ensure = client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(TopologySpec {
                network_id: "net-race".to_string(),
                tenant_id: "t-race".to_string(),
                bridge_name: "br-race".to_string(),
                namespace_name: "ns-race".to_string(),
                subnet_cidr: "10.0.8.0/24".to_string(),
                gateway_ip: "10.0.8.1".to_string(),
                options: Default::default(),
                vni: 0,
                vtep_endpoints: vec![],
                overlay_type: 0,
                fabric: None,
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ensure.status, "OK");

    // Fire the firewall + NAT RPCs CONCURRENTLY on a multi-thread runtime so
    // the two handler tasks can genuinely run in parallel and both records must
    // survive: after the join, BOTH halves must still be present and re-asserted
    // on attach. Round-4 made this atomic by mutating the policy_state entry in
    // place under the DashMap shard lock (the pre-fix read-clone-modify-insert
    // could lose one half of the fw+nat pair under overlap, leaving a
    // later-attached NIC outside the default-deny boundary).
    //
    // NOTE: this is an invariant-under-concurrency test, not a deterministic
    // race reproducer. The policy_state critical section has no await points,
    // so on the default single-thread `#[tokio::test]` runtime the handlers can
    // never interleave (the test would pass even against the buggy code); even
    // here the pre-fix lost-update only manifests if the OS schedules the two
    // non-yielding sections to interleave, which is probabilistic per run.
    let mut fw_client = client.clone();
    let fw_fut = async move {
        fw_client
            .set_firewall_policy(SetFirewallPolicyRequest {
                meta: None,
                network_id: "net-race".to_string(),
                policy: Some(FirewallPolicy {
                    policy_version: "race-v1".to_string(),
                    policy_json: b"[]".to_vec(),
                }),
            })
            .await
            .unwrap()
            .into_inner()
    };
    let mut nat_client = client.clone();
    let nat_fut = async move {
        nat_client
            .set_nat_policy(SetNatPolicyRequest {
                meta: None,
                network_id: "net-race".to_string(),
                policy: Some(NatPolicy {
                    policy_version: "race-v1".to_string(),
                    policy_json: b"[]".to_vec(),
                }),
            })
            .await
            .unwrap()
            .into_inner()
    };
    let (fw, nat) = tokio::join!(fw_fut, nat_fut);
    assert_eq!(fw.status, "OK");
    assert_eq!(nat.status, "OK");

    // Attach a NIC -> refresh must re-assert BOTH halves for the network.
    let attach = client
        .attach_vm_nic(AttachVmNicRequest {
            meta: None,
            nic: Some(NicSpec {
                nic_id: "nic-race".to_string(),
                vm_id: "vm-race".to_string(),
                network_id: "net-race".to_string(),
                mac_address: "02:00:00:00:00:0b".to_string(),
                tap_name: "tap-race".to_string(),
                ip_address: "10.0.8.10".to_string(),
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(attach.result.as_ref().unwrap().status, "OK");

    let recorded = calls.lock().unwrap();
    let fw_calls = recorded
        .iter()
        .filter(|c| c.starts_with("fw:net-race:"))
        .count();
    let nat_calls = recorded
        .iter()
        .filter(|c| c.starts_with("nat:net-race:"))
        .count();
    assert_eq!(
        fw_calls, 2,
        "firewall: 1 concurrent apply + 1 attach re-scope (a lost record would leave only 1)"
    );
    assert_eq!(
        nat_calls, 2,
        "nat: 1 concurrent apply + 1 attach re-scope (a lost record would leave only 1)"
    );
}

#[tokio::test]
async fn delete_without_state_row_runs_derived_names_local_teardown() {
    // #356 N5: a delete arriving with no in-memory TopologyTable row (an
    // nwd restart wiped it) used to silently SKIP the local half of the
    // teardown — bridge / namespace / dnsmasq / nft residue survived
    // forever (the M4.4 qualification's restart-residue finding). The
    // derived-names fallback must run exactly in that case, and ONLY in
    // that case: a delete with a state row uses the authoritative
    // state-row path.
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nwd.sock");
    let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let rec = RecordingExecutor {
        calls: calls.clone(),
    };
    let server = NetworkServer::new(rec, Metrics::new());
    let socket_clone = socket.clone();
    tokio::spawn(async move {
        server.serve(&socket_clone).await.ok();
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut client = make_client(socket).await;

    // Delete a network that was never ensured on this nwd instance: no
    // state row, so the derived-names fallback runs (a no-op against
    // non-existent host objects, but the executor is invoked).
    let del = client
        .delete_network_topology(DeleteNetworkTopologyRequest {
            meta: None,
            network_id: "net-gone".to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(del.status, "OK");
    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == "derived_teardown:net-gone"),
        "a delete with no state row must run the derived-names local teardown"
    );

    // Ensure a topology so its delete takes the state-row path instead.
    client
        .ensure_network_topology(EnsureNetworkTopologyRequest {
            meta: None,
            topology: Some(TopologySpec {
                network_id: "net-live".to_string(),
                tenant_id: "t1".to_string(),
                bridge_name: "br-netlive".to_string(),
                namespace_name: "ns-netlive".to_string(),
                subnet_cidr: "10.0.9.0/24".to_string(),
                gateway_ip: "10.0.9.1".to_string(),
                options: Default::default(),
                vni: 0,
                vtep_endpoints: vec![],
                overlay_type: 0,
                fabric: None,
            }),
        })
        .await
        .unwrap()
        .into_inner();
    let del = client
        .delete_network_topology(DeleteNetworkTopologyRequest {
            meta: None,
            network_id: "net-live".to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(del.status, "OK");
    let recorded = calls.lock().unwrap();
    assert!(
        recorded.iter().any(|c| c == "state_teardown:net-live"),
        "a delete with a state row must use the state-row teardown path"
    );
    assert!(
        !recorded.iter().any(|c| c == "derived_teardown:net-live"),
        "the state-row path must not double-run the derived-names fallback"
    );
}
