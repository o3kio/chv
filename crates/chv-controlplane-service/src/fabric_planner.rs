//! ADR-021 fabric plan compiler.
//!
//! Compiles per-node `FabricPlan`s for an overlay-enabled (`vxlan`) tenant
//! network: the bounded head-end replication flood list (every enrolled node
//! hosting a VM NIC of that network), derived MTUs, and generation fencing.
//! The compiler is fail-closed throughout: any missing datum (no VNI, a
//! participant without a registered identity, an unusable MTU) is a
//! structured error naming the offending entity — never a silent skip that
//! would shrink the flood list or ship an unencrypted peer.

use crate::error::ControlPlaneServiceError;
use chv_controlplane_store::{StorePool, VtepRepository};
use control_plane_node_api::control_plane_node_api as proto;

/// Fabric domain identifier. A single default domain spans the whole
/// cluster in this phase; making it configurable is a follow-up.
const FABRIC_DOMAIN_ID: &str = "chv-default";

/// Tenant (VM-facing) MTU when no participant has measured its underlay.
const DEFAULT_TENANT_MTU: u32 = 1380;
/// Fabric (WireGuard/VXLAN) MTU when no participant has measured its underlay.
const DEFAULT_FABRIC_MTU: u32 = 1440;
/// Encapsulation overhead between the underlay and the tenant payload
/// (WireGuard + VXLAN + Ethernet headers), per ADR-021 §3.
const TENANT_MTU_OVERHEAD: u32 = 110;
/// Encapsulation overhead between the underlay and the fabric overlay.
const FABRIC_MTU_OVERHEAD: u32 = 60;
/// IPv6 minimum MTU — a tenant MTU below this cannot carry traffic.
const MIN_TENANT_MTU: u32 = 576;
/// Tenant MTUs never exceed the classic Ethernet payload size.
const TENANT_MTU_CEILING: u32 = 1500;

/// A fabric plan compiled for one participating node, plus the dispatch
/// metadata (VNI) the `UpdateOverlay` request carries alongside it.
#[derive(Debug, Clone)]
pub struct CompiledFabricPlan {
    /// Node this plan is addressed to (`plan.local_host_id`).
    pub node_id: String,
    /// VNI of the network (carried in `UpdateOverlayRequest.vni`).
    pub vni: u32,
    /// The plan itself (carried in `UpdateOverlayRequest.fabric`).
    pub plan: proto::FabricPlan,
}

/// Compiles ADR-021 fabric plans from control-plane desired state.
#[derive(Clone)]
pub struct FabricPlanner {
    pool: StorePool,
}

/// A participant with a fully validated fabric identity. Built from
/// `FabricPeerRecord` with fail-closed checks so later compilation steps
/// can never see a missing datum.
struct ValidatedParticipant {
    node_id: String,
    public_key: String,
    underlay_endpoint: String,
    fabric_ip: String,
    underlay_mtu: Option<u32>,
}

#[derive(sqlx::FromRow)]
struct NetworkOverlayRow {
    overlay_type: Option<String>,
    vni: Option<i64>,
    desired_generation: Option<i64>,
}

impl FabricPlanner {
    pub fn new(pool: StorePool) -> Self {
        Self { pool }
    }

    /// Compile the per-node fabric plans for a network (ADR-021 plan §Phase 3).
    ///
    /// Fails closed on: unknown network, non-vxlan network, no participants,
    /// any participant missing public key / fabric IP / underlay endpoint,
    /// and a derived tenant MTU below 576.
    pub async fn compile_for_network(
        &self,
        network_id: &str,
    ) -> Result<Vec<CompiledFabricPlan>, ControlPlaneServiceError> {
        let vtep_repo = VtepRepository::new(self.pool.clone());

        let network = self.load_network(network_id).await?;
        if network.overlay_type.as_deref() != Some("vxlan") {
            return Err(ControlPlaneServiceError::InvalidArgument(format!(
                "network {network_id} is not overlay-enabled (overlay_type must be 'vxlan')"
            )));
        }

        // Lazy VNI allocation (plan §9): allocate on first compilation
        // instead of touching the BFF network-create path.
        let vni = match network.vni {
            Some(v) if v > 0 => v,
            _ => i64::from(vtep_repo.allocate_vni(network_id).await?),
        };

        let binding_generation = vtep_repo
            .get_binding_generation(network_id)
            .await?
            .unwrap_or(1);
        let plan_generation = network.desired_generation.unwrap_or(1);

        let peers = vtep_repo.get_fabric_peers_for_network(network_id).await?;
        if peers.is_empty() {
            return Err(ControlPlaneServiceError::InvalidArgument(format!(
                "network {network_id} has no fabric participants (no enrolled node hosts a VM NIC of this network)"
            )));
        }

        let participants: Vec<ValidatedParticipant> = peers
            .into_iter()
            .map(|peer| self.validate_participant(peer, network_id))
            .collect::<Result<_, _>>()?;

        let (tenant_mtu, fabric_mtu) = derive_mtus(&participants, network_id)?;

        let plans = participants
            .iter()
            .map(|local| CompiledFabricPlan {
                node_id: local.node_id.clone(),
                vni: vni as u32,
                plan: proto::FabricPlan {
                    fabric_domain_id: FABRIC_DOMAIN_ID.to_string(),
                    local_host_id: local.node_id.clone(),
                    local_fabric_ip: local.fabric_ip.clone(),
                    tenant_mtu,
                    fabric_mtu,
                    binding_generation: binding_generation as u64,
                    plan_generation: plan_generation as u64,
                    // Bounded flood list: every other participant. The
                    // local node is never its own peer.
                    peers: participants
                        .iter()
                        .filter(|p| p.node_id != local.node_id)
                        .map(|p| proto::FabricPeer {
                            node_id: p.node_id.clone(),
                            public_key: p.public_key.clone(),
                            underlay_endpoint: p.underlay_endpoint.clone(),
                            fabric_ip: p.fabric_ip.clone(),
                        })
                        .collect(),
                },
            })
            .collect();

        tracing::info!(
            network_id = network_id,
            vni = vni,
            participants = participants.len(),
            tenant_mtu = tenant_mtu,
            fabric_mtu = fabric_mtu,
            plan_generation = plan_generation,
            binding_generation = binding_generation,
            "compiled fabric plans"
        );

        Ok(plans)
    }

    /// Load the network row with the overlay columns plus the desired
    /// generation (the plan generation fence).
    ///
    /// #499: tombstoned (deleted) networks are NotFound — the physical
    /// row survives the delete as the tombstone's anchor, so without
    /// this exclusion a deleted overlay network would still compile
    /// fabric plans and (through `allocate_vni`) mint fresh VNI
    /// allocations on a dead network. This is the load-bearing gate
    /// between the vtep writers (`vtep.rs`'s `UPDATE networks SET vni`)
    /// and a tombstone: everything they allocate flows through this
    /// lookup first.
    async fn load_network(
        &self,
        network_id: &str,
    ) -> Result<NetworkOverlayRow, ControlPlaneServiceError> {
        sqlx::query_as::<_, NetworkOverlayRow>(
            r#"SELECT n.overlay_type, n.vni, nds.desired_generation
               FROM networks n
               LEFT JOIN network_desired_state nds ON nds.network_id = n.network_id
               WHERE n.network_id = ?
                 AND (nds.desired_status IS NULL OR nds.desired_status != 'Deleting')"#,
        )
        .bind(network_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| ControlPlaneServiceError::Store(chv_controlplane_store::StoreError::from(e)))?
        .ok_or_else(|| {
            ControlPlaneServiceError::NotFound(format!("network {network_id} not found"))
        })
    }

    /// Fail-closed identity validation: every participant must carry a
    /// public key, a fabric IP, and an underlay endpoint before it may
    /// enter a compiled plan.
    fn validate_participant(
        &self,
        peer: chv_controlplane_store::FabricPeerRecord,
        network_id: &str,
    ) -> Result<ValidatedParticipant, ControlPlaneServiceError> {
        let public_key = non_empty(peer.public_key).ok_or_else(|| {
            ControlPlaneServiceError::InvalidArgument(format!(
                "node {} has no WireGuard public key registered; fabric overlay for network {} cannot be compiled",
                peer.node_id, network_id
            ))
        })?;
        let fabric_ip = non_empty(peer.fabric_ip).ok_or_else(|| {
            ControlPlaneServiceError::InvalidArgument(format!(
                "node {} has no fabric transport IP allocated; fabric overlay for network {} cannot be compiled",
                peer.node_id, network_id
            ))
        })?;
        let underlay_endpoint = non_empty(peer.underlay_endpoint).ok_or_else(|| {
            ControlPlaneServiceError::InvalidArgument(format!(
                "node {} has no underlay endpoint registered; fabric overlay cannot be compiled",
                peer.node_id
            ))
        })?;
        let underlay_mtu = peer
            .underlay_mtu
            .and_then(|mtu| u32::try_from(mtu).ok())
            .filter(|&mtu| mtu > 0);
        Ok(ValidatedParticipant {
            node_id: peer.node_id,
            public_key,
            underlay_endpoint,
            fabric_ip,
            underlay_mtu,
        })
    }
}

/// Derive tenant/fabric MTUs from the participants' measured underlay MTUs
/// (ADR-021 §3): `tenant = min(underlay) - 110` (capped at 1500),
/// `fabric = min(underlay) - 60`; defaults 1380/1440 when unmeasured.
fn derive_mtus(
    participants: &[ValidatedParticipant],
    network_id: &str,
) -> Result<(u32, u32), ControlPlaneServiceError> {
    let Some(min_underlay) = participants.iter().filter_map(|p| p.underlay_mtu).min() else {
        return Ok((DEFAULT_TENANT_MTU, DEFAULT_FABRIC_MTU));
    };

    let tenant_mtu = std::cmp::min(
        min_underlay.saturating_sub(TENANT_MTU_OVERHEAD),
        TENANT_MTU_CEILING,
    );
    let fabric_mtu = min_underlay.saturating_sub(FABRIC_MTU_OVERHEAD);
    if tenant_mtu < MIN_TENANT_MTU {
        return Err(ControlPlaneServiceError::InvalidArgument(format!(
            "derived tenant MTU {tenant_mtu} for network {network_id} is below the minimum of {MIN_TENANT_MTU} (smallest underlay MTU is {min_underlay})"
        )));
    }
    Ok((tenant_mtu, fabric_mtu))
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chv_controlplane_store::test_util::create_test_pool;

    async fn seed_node(pool: &StorePool, node_id: &str) {
        sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES (?, ?, ?)")
            .bind(node_id)
            .bind(format!("host-{node_id}"))
            .bind(format!("Node {node_id}"))
            .execute(pool)
            .await
            .expect("insert node");
    }

    /// Seed a vxlan overlay network with a desired generation.
    async fn seed_network(pool: &StorePool, network_id: &str, desired_generation: i64) {
        sqlx::query(
            "INSERT INTO networks (network_id, display_name, overlay_type) VALUES (?, ?, 'vxlan')",
        )
        .bind(network_id)
        .bind(format!("Net {network_id}"))
        .execute(pool)
        .await
        .expect("insert network");
        sqlx::query(
            "INSERT INTO network_desired_state (network_id, desired_generation) VALUES (?, ?)",
        )
        .bind(network_id)
        .bind(desired_generation)
        .execute(pool)
        .await
        .expect("insert network desired state");
    }

    /// Seed a VM placed on a node with a NIC on the network.
    async fn seed_placement(pool: &StorePool, vm_id: &str, node_id: &str, network_id: &str) {
        sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES (?, ?)")
            .bind(vm_id)
            .bind(format!("VM {vm_id}"))
            .execute(pool)
            .await
            .expect("insert vm");
        sqlx::query(
            "INSERT INTO vm_desired_state (vm_id, desired_generation, target_node_id) \
             VALUES (?, 1, ?)",
        )
        .bind(vm_id)
        .bind(node_id)
        .execute(pool)
        .await
        .expect("insert vm desired state");
        sqlx::query(
            "INSERT INTO vm_nic_desired_state (nic_id, vm_id, network_id) VALUES (?, ?, ?)",
        )
        .bind(format!("nic-{vm_id}"))
        .bind(vm_id)
        .bind(network_id)
        .execute(pool)
        .await
        .expect("insert vm nic desired state");
    }

    /// Register a full fabric identity for a node (endpoint via direct SQL:
    /// the enrollment path stores NULL until underlay addressing is wired).
    async fn seed_fabric_node(
        pool: &StorePool,
        node_id: &str,
        public_key: &str,
        underlay_mtu: u32,
        endpoint: Option<&str>,
    ) {
        let repo = VtepRepository::new(pool.clone());
        repo.register_fabric_identity(node_id, public_key, underlay_mtu, None, None)
            .await
            .expect("register fabric identity");
        if let Some(endpoint) = endpoint {
            sqlx::query("UPDATE vtep_registry SET underlay_endpoint = ? WHERE node_id = ?")
                .bind(endpoint)
                .bind(node_id)
                .execute(pool)
                .await
                .expect("set underlay endpoint");
        }
    }

    async fn two_node_cluster_with_identity(pool: &StorePool) {
        seed_node(pool, "node-a").await;
        seed_node(pool, "node-b").await;
        seed_network(pool, "net-fabric", 4).await;
        seed_placement(pool, "vm-a", "node-a", "net-fabric").await;
        seed_placement(pool, "vm-b", "node-b", "net-fabric").await;
        seed_fabric_node(
            pool,
            "node-a",
            "pub-aAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            0,
            Some("10.0.0.1:65001"),
        )
        .await;
        seed_fabric_node(
            pool,
            "node-b",
            "pub-bBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB=",
            0,
            Some("10.0.0.2:65001"),
        )
        .await;
    }

    #[tokio::test]
    async fn compiles_two_node_plans_with_each_other_as_sole_peer() {
        let pool = create_test_pool().await;
        two_node_cluster_with_identity(&pool).await;

        let planner = FabricPlanner::new(pool.clone());
        let plans = planner.compile_for_network("net-fabric").await.unwrap();
        assert_eq!(plans.len(), 2, "both participants get a plan");

        let by_node: std::collections::HashMap<&str, &CompiledFabricPlan> =
            plans.iter().map(|p| (p.node_id.as_str(), p)).collect();

        for (node, other, other_key) in [
            (
                "node-a",
                "node-b",
                "pub-bBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB=",
            ),
            (
                "node-b",
                "node-a",
                "pub-aAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            ),
        ] {
            let plan = by_node[node];
            assert_eq!(plan.plan.local_host_id, node);
            assert_eq!(plan.plan.fabric_domain_id, "chv-default");
            assert_eq!(plan.plan.plan_generation, 4);
            assert_eq!(plan.plan.binding_generation, 1);
            assert_eq!(plan.plan.peers.len(), 1, "sole peer is the other node");
            let peer = &plan.plan.peers[0];
            assert_eq!(peer.node_id, other);
            assert_eq!(peer.public_key, other_key);
            assert_eq!(
                peer.underlay_endpoint,
                format!("10.0.0.{}:65001", if other == "node-a" { 1 } else { 2 })
            );
        }

        // Fabric IPs were allocated sequentially from 100.100.0.1.
        assert_eq!(by_node["node-a"].plan.local_fabric_ip, "100.100.0.1");
        assert_eq!(by_node["node-b"].plan.local_fabric_ip, "100.100.0.2");
        let peer_fabric_ip = &by_node["node-a"].plan.peers[0].fabric_ip;
        assert_eq!(peer_fabric_ip, "100.100.0.2");
    }

    #[tokio::test]
    async fn allocates_vni_lazily_and_stamps_binding_generation() {
        let pool = create_test_pool().await;
        two_node_cluster_with_identity(&pool).await;

        // No VNI allocated up front.
        let vni: Option<i64> =
            sqlx::query_scalar("SELECT vni FROM networks WHERE network_id = 'net-fabric'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(vni, Some(0), "network starts with no VNI");

        let planner = FabricPlanner::new(pool.clone());
        let plans = planner.compile_for_network("net-fabric").await.unwrap();
        let vni = plans[0].vni;
        assert!(vni >= 1, "VNI allocated lazily, got {vni}");

        // The allocation is durable and mirrored onto the network row.
        let stored: i64 =
            sqlx::query_scalar("SELECT vni FROM networks WHERE network_id = 'net-fabric'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored, i64::from(vni));
        let repo = VtepRepository::new(pool.clone());
        assert_eq!(
            repo.get_vni_for_network("net-fabric").await.unwrap(),
            Some(vni as i32)
        );
        assert_eq!(
            repo.get_binding_generation("net-fabric").await.unwrap(),
            Some(1),
            "first allocation stamps binding generation 1"
        );

        // Re-compilation is idempotent: same VNI, no new allocation.
        let again = planner.compile_for_network("net-fabric").await.unwrap();
        assert_eq!(again[0].vni, vni);
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM vni_allocations WHERE network_id = 'net-fabric'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 1, "lazy allocation must not double-allocate");
    }

    #[tokio::test]
    async fn fabric_ip_allocation_is_sequential_from_first_host_address() {
        let pool = create_test_pool().await;
        seed_node(&pool, "node-a").await;
        seed_node(&pool, "node-b").await;
        seed_fabric_node(&pool, "node-a", "pub-a", 0, None).await;
        seed_fabric_node(&pool, "node-b", "pub-b", 0, None).await;

        let repo = VtepRepository::new(pool.clone());
        let id_a = repo.get_fabric_identity("node-a").await.unwrap().unwrap();
        let id_b = repo.get_fabric_identity("node-b").await.unwrap().unwrap();
        assert_eq!(id_a.fabric_ip.as_deref(), Some("100.100.0.1"));
        assert_eq!(id_b.fabric_ip.as_deref(), Some("100.100.0.2"));

        // Re-registration keeps the allocated IP and updates the MTU.
        repo.register_fabric_identity("node-a", "pub-a", 1500, None, None)
            .await
            .unwrap();
        let id_a = repo.get_fabric_identity("node-a").await.unwrap().unwrap();
        assert_eq!(id_a.fabric_ip.as_deref(), Some("100.100.0.1"));
        assert_eq!(id_a.underlay_mtu, Some(1500));
        assert_eq!(id_a.public_key.as_deref(), Some("pub-a"));
    }

    #[tokio::test]
    async fn peer_missing_public_key_fails_closed() {
        let pool = create_test_pool().await;
        two_node_cluster_with_identity(&pool).await;

        // Erase node-b's public key (legacy VTEP row without identity).
        sqlx::query("UPDATE vtep_registry SET public_key = NULL WHERE node_id = 'node-b'")
            .execute(&pool)
            .await
            .unwrap();

        let planner = FabricPlanner::new(pool);
        let err = planner.compile_for_network("net-fabric").await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("node-b") && msg.contains("WireGuard public key"),
            "error must name the node and the missing datum, got: {msg}"
        );
    }

    #[tokio::test]
    async fn peer_missing_underlay_endpoint_fails_closed() {
        let pool = create_test_pool().await;
        two_node_cluster_with_identity(&pool).await;

        sqlx::query("UPDATE vtep_registry SET underlay_endpoint = NULL WHERE node_id = 'node-a'")
            .execute(&pool)
            .await
            .unwrap();

        let planner = FabricPlanner::new(pool);
        let err = planner.compile_for_network("net-fabric").await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("node-a") && msg.contains("no underlay endpoint registered"),
            "error must name the node and the missing endpoint, got: {msg}"
        );
    }

    #[tokio::test]
    async fn placement_change_removes_departed_node_from_flood_list() {
        let pool = create_test_pool().await;
        two_node_cluster_with_identity(&pool).await;

        // Remove node-b's VM placement from the network.
        sqlx::query("DELETE FROM vm_nic_desired_state WHERE vm_id = 'vm-b'")
            .execute(&pool)
            .await
            .unwrap();

        let planner = FabricPlanner::new(pool);
        let plans = planner.compile_for_network("net-fabric").await.unwrap();
        assert_eq!(plans.len(), 1, "only node-a remains a participant");
        assert_eq!(plans[0].node_id, "node-a");
        assert!(
            plans[0].plan.peers.is_empty(),
            "node-a has no peers after node-b's placement is gone"
        );
    }

    #[tokio::test]
    async fn defaults_mtus_when_no_underlay_mtu_measured() {
        let pool = create_test_pool().await;
        two_node_cluster_with_identity(&pool).await;

        let planner = FabricPlanner::new(pool);
        let plans = planner.compile_for_network("net-fabric").await.unwrap();
        assert_eq!(plans[0].plan.tenant_mtu, 1380);
        assert_eq!(plans[0].plan.fabric_mtu, 1440);
    }

    #[tokio::test]
    async fn derives_mtus_from_smallest_measured_underlay() {
        let pool = create_test_pool().await;
        seed_node(&pool, "node-a").await;
        seed_node(&pool, "node-b").await;
        seed_network(&pool, "net-mtu", 1).await;
        seed_placement(&pool, "vm-a", "node-a", "net-mtu").await;
        seed_placement(&pool, "vm-b", "node-b", "net-mtu").await;
        seed_fabric_node(&pool, "node-a", "pub-a", 9000, Some("10.0.0.1:65001")).await;
        seed_fabric_node(&pool, "node-b", "pub-b", 1500, Some("10.0.0.2:65001")).await;

        let planner = FabricPlanner::new(pool);
        let plans = planner.compile_for_network("net-mtu").await.unwrap();
        // min(9000, 1500) = 1500 -> tenant = 1500 - 110 = 1390, fabric = 1440.
        assert_eq!(plans[0].plan.tenant_mtu, 1390);
        assert_eq!(plans[0].plan.fabric_mtu, 1440);
    }

    #[tokio::test]
    async fn tiny_underlay_mtu_fails_closed() {
        let pool = create_test_pool().await;
        seed_node(&pool, "node-a").await;
        seed_network(&pool, "net-tiny", 1).await;
        seed_placement(&pool, "vm-a", "node-a", "net-tiny").await;
        seed_fabric_node(&pool, "node-a", "pub-a", 600, Some("10.0.0.1:65001")).await;

        let planner = FabricPlanner::new(pool);
        let err = planner.compile_for_network("net-tiny").await.unwrap_err();
        assert!(
            err.to_string().contains("below the minimum"),
            "tiny underlay must fail closed, got: {err}"
        );
    }

    #[tokio::test]
    async fn non_vxlan_network_fails_closed() {
        let pool = create_test_pool().await;
        seed_node(&pool, "node-a").await;
        seed_network(&pool, "net-plain", 1).await;
        sqlx::query("UPDATE networks SET overlay_type = 'none' WHERE network_id = 'net-plain'")
            .execute(&pool)
            .await
            .unwrap();
        seed_placement(&pool, "vm-a", "node-a", "net-plain").await;
        seed_fabric_node(&pool, "node-a", "pub-a", 0, Some("10.0.0.1:65001")).await;

        let planner = FabricPlanner::new(pool);
        let err = planner.compile_for_network("net-plain").await.unwrap_err();
        assert!(
            err.to_string().contains("not overlay-enabled"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn network_without_participants_fails_closed() {
        let pool = create_test_pool().await;
        seed_node(&pool, "node-a").await;
        seed_network(&pool, "net-empty", 1).await;
        seed_fabric_node(&pool, "node-a", "pub-a", 0, Some("10.0.0.1:65001")).await;

        let planner = FabricPlanner::new(pool);
        let err = planner.compile_for_network("net-empty").await.unwrap_err();
        assert!(
            err.to_string().contains("no fabric participants"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn unknown_network_is_not_found() {
        let pool = create_test_pool().await;
        let planner = FabricPlanner::new(pool);
        let err = planner.compile_for_network("net-ghost").await.unwrap_err();
        assert!(matches!(err, ControlPlaneServiceError::NotFound(_)));
    }

    /// #499: a tombstoned (deleted) network is NotFound at the plan
    /// fence — the physical row survives the delete as the tombstone's
    /// anchor, so without `load_network`'s exclusion a deleted overlay
    /// network would still compile fabric plans and (through the lazy
    /// `allocate_vni`) mint a fresh VNI allocation on a dead network.
    /// `load_network` is the gate between the vtep writers and the
    /// tombstone; this pin keeps it closed.
    #[tokio::test]
    async fn compile_for_network_refuses_a_tombstoned_network() {
        let pool = create_test_pool().await;
        seed_node(&pool, "node-a").await;
        seed_node(&pool, "node-b").await;
        seed_network(&pool, "net-dead", 4).await;
        seed_placement(&pool, "vm-a", "node-a", "net-dead").await;
        seed_placement(&pool, "vm-b", "node-b", "net-dead").await;

        // The tombstone the BFF delete route writes.
        sqlx::query(
            "UPDATE network_desired_state SET desired_status = 'Deleting', \
             desired_generation = desired_generation + 1 WHERE network_id = 'net-dead'",
        )
        .execute(&pool)
        .await
        .expect("tombstone the network");

        let planner = FabricPlanner::new(pool.clone());
        let err = planner
            .compile_for_network("net-dead")
            .await
            .expect_err("a tombstoned network must not compile fabric plans");
        assert!(
            matches!(err, ControlPlaneServiceError::NotFound(ref msg) if msg.contains("net-dead")),
            "the refusal must be the NotFound from load_network, got: {err:?}"
        );

        // And no VNI was allocated on the dead network (the vtep writer
        // interleaving the #499 census flagged).
        let allocations: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM vni_allocations WHERE network_id = 'net-dead'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(allocations, 0, "no VNI allocation may land for a tombstone");
    }
}
