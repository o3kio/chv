//! Phase 9: End-to-end integration tests for multi-node migration and overlay networking.
//!
//! These tests exercise the migration state machine, overlay manager, VNI lifecycle,
//! and enrollment with VTEP registration—all using in-memory SQLite with mocked node
//! clients (no real gRPC connections or VMs).

use crate::migration::{
    create_migration_record, update_migration_progress, MigrationConfig, MigrationPhase,
    MigrationState, PhaseTimeouts,
};
use crate::node_client_pool::NodeClientPool;
use crate::overlay::OverlayManager;
use chv_controlplane_store::{
    test_util::create_test_pool, DesiredStateRepository, NodeRepository, NodeUpsertInput,
    OperationCreateInput, OperationRepository, StorePool, VtepRepository,
};
use chv_controlplane_types::domain::{
    Generation, NodeId, OperationId, OperationStatus, ResourceId, ResourceKind,
};
use control_plane_node_api::control_plane_node_api as proto;

/// A lightweight two-node test cluster backed by in-memory SQLite.
/// Provides repositories and helpers for setting up migration/overlay test scenarios.
struct TestCluster {
    pool: StorePool,
    node_repo: NodeRepository,
    operation_repo: OperationRepository,
    desired_state_repo: DesiredStateRepository,
    vtep_repo: VtepRepository,
}

impl TestCluster {
    /// Create a TestCluster with an in-memory SQLite database and all migrations applied.
    async fn new() -> Self {
        let pool = create_test_pool().await;
        let node_repo = NodeRepository::new(pool.clone());
        let operation_repo = OperationRepository::new(pool.clone());
        let desired_state_repo = DesiredStateRepository::new(pool.clone());
        let vtep_repo = VtepRepository::new(pool.clone());
        Self {
            pool,
            node_repo,
            operation_repo,
            desired_state_repo,
            vtep_repo,
        }
    }

    /// Enroll two nodes (node_a, node_b) with VTEP addresses registered.
    async fn setup_two_nodes(&self) {
        // Create node_a
        self.node_repo
            .upsert_node(&NodeUpsertInput {
                node_id: NodeId::new("node-a").unwrap(),
                hostname: "host-a".to_string(),
                display_name: "Node A".to_string(),
                certificate_serial: None,
                agent_version: Some("0.1.0".to_string()),
                control_plane_version: Some("0.1.0".to_string()),
                enrolled_unix_ms: 1000,
                last_seen_unix_ms: 1000,
            })
            .await
            .expect("failed to create node-a");

        // Create node_b
        self.node_repo
            .upsert_node(&NodeUpsertInput {
                node_id: NodeId::new("node-b").unwrap(),
                hostname: "host-b".to_string(),
                display_name: "Node B".to_string(),
                certificate_serial: None,
                agent_version: Some("0.1.0".to_string()),
                control_plane_version: Some("0.1.0".to_string()),
                enrolled_unix_ms: 1000,
                last_seen_unix_ms: 1000,
            })
            .await
            .expect("failed to create node-b");

        // Register VTEPs
        self.vtep_repo
            .register_vtep("node-a", "10.0.0.1", 4789)
            .await
            .expect("failed to register VTEP for node-a");
        self.vtep_repo
            .register_vtep("node-b", "10.0.0.2", 4789)
            .await
            .expect("failed to register VTEP for node-b");
    }

    /// Create a VM on the given node with a network and NIC attached.
    async fn create_vm_on_node(
        &self,
        vm_id: &str,
        node_id: &str,
        network_id: &str,
        mac: &str,
        memory_bytes: i64,
    ) {
        use chv_controlplane_store::{NetworkDesiredStateInput, VmDesiredStateInput};

        // Create VM record
        self.desired_state_repo
            .upsert_vm(&VmDesiredStateInput {
                vm_id: ResourceId::new(vm_id).unwrap(),
                node_id: Some(NodeId::new(node_id).unwrap()),
                display_name: format!("Test VM {vm_id}"),
                tenant_id: None,
                placement_policy: None,
                desired_generation: Generation::new(1),
                desired_status: Some("running".to_string()),
                requested_by: Some("test".to_string()),
                updated_by: Some("test".to_string()),
                target_node_id: Some(NodeId::new(node_id).unwrap()),
                cpu_count: Some(2),
                memory_bytes: Some(memory_bytes),
                image_ref: Some("test-image:latest".to_string()),
                boot_mode: None,
                desired_power_state: Some("Running".to_string()),
                vsock_enabled: None,
                requested_unix_ms: 1000,
            })
            .await
            .expect("failed to create VM desired state");

        // Create network if needed (ignore conflict)
        let _ = self
            .desired_state_repo
            .upsert_network(&NetworkDesiredStateInput {
                network_id: ResourceId::new(network_id).unwrap(),
                node_id: Some(NodeId::new(node_id).unwrap()),
                display_name: format!("Test Network {network_id}"),
                network_class: Some("overlay".to_string()),
                desired_generation: Generation::new(1),
                desired_status: Some("active".to_string()),
                requested_by: Some("test".to_string()),
                updated_by: Some("test".to_string()),
                firewall_rules_json: None,
                nat_rules_json: None,
                dhcp_scope_json: None,
                dns_enabled: None,
                dns_scope_json: None,
                requested_unix_ms: 1000,
            })
            .await;

        // Create NIC linking VM to network
        sqlx::query(
            r#"INSERT OR IGNORE INTO vm_nic_desired_state (nic_id, vm_id, network_id, mac_address)
               VALUES (?, ?, ?, ?)"#,
        )
        .bind(format!("nic-{vm_id}"))
        .bind(vm_id)
        .bind(network_id)
        .bind(mac)
        .execute(&self.pool)
        .await
        .expect("failed to create NIC");
    }

    /// Create an operation record required by the migrations table FK constraint.
    async fn create_operation(&self, operation_id: &str) {
        self.operation_repo
            .create_or_get(&OperationCreateInput {
                operation_id: OperationId::new(operation_id).unwrap(),
                idempotency_key: format!("idem-{operation_id}"),
                resource_kind: ResourceKind::Vm,
                resource_id: None,
                operation_type: "migrate".to_string(),
                status: OperationStatus::Running,
                requested_by: Some("test".to_string()),
                updated_by: Some("test".to_string()),
                desired_generation: Some(Generation::new(1)),
                observed_generation: None,
                correlation_id: None,
                requested_unix_ms: 1000,
            })
            .await
            .expect("failed to create operation");
    }
}

// =============================================================================
// 9.1 — Multi-node integration test harness
// =============================================================================

#[tokio::test]
async fn test_cluster_setup_two_nodes_with_vtep() {
    let cluster = TestCluster::new().await;
    cluster.setup_two_nodes().await;

    // Verify VTEPs registered
    let vtep_a = cluster.vtep_repo.get_vtep("node-a").await.unwrap();
    assert_eq!(vtep_a.vtep_ip, "10.0.0.1");
    assert_eq!(vtep_a.vtep_port, 4789);

    let vtep_b = cluster.vtep_repo.get_vtep("node-b").await.unwrap();
    assert_eq!(vtep_b.vtep_ip, "10.0.0.2");
    assert_eq!(vtep_b.vtep_port, 4789);
}

// =============================================================================
// 9.2 — Test scenarios
// =============================================================================

// Scenario 1: Happy path migration state progression
// Since execute_migration requires real gRPC connections, we test the state machine
// components (record creation, phase transitions, progress updates, placement update)
// individually and verify the database state after each operation.
#[tokio::test]
async fn test_happy_path_migration_state_progression() {
    let cluster = TestCluster::new().await;
    cluster.setup_two_nodes().await;
    cluster
        .create_vm_on_node(
            "vm-001",
            "node-a",
            "net-overlay",
            "aa:bb:cc:dd:ee:01",
            4_294_967_296,
        )
        .await;
    cluster.create_operation("op-mig-001").await;

    // Create migration record
    let state = MigrationState {
        migration_id: "mig-001".to_string(),
        operation_id: "op-mig-001".to_string(),
        vm_id: "vm-001".to_string(),
        source_node_id: "node-a".to_string(),
        dest_node_id: "node-b".to_string(),
        phase: MigrationPhase::Pending,
        config: MigrationConfig::default(),
        bytes_transferred: 0,
        total_bytes: 0,
        convergence_round: 0,
        dirty_blocks_remaining: 0,
    };

    create_migration_record(&cluster.pool, &state)
        .await
        .expect("failed to create migration record");

    // Verify record exists with Pending phase
    let row: (String, String, String) = sqlx::query_as(
        "SELECT phase, source_node_id, destination_node_id FROM migrations WHERE migration_id = ?",
    )
    .bind("mig-001")
    .fetch_one(&cluster.pool)
    .await
    .unwrap();
    assert_eq!(row.0, "Pending");
    assert_eq!(row.1, "node-a");
    assert_eq!(row.2, "node-b");

    // Simulate phase transitions via update_migration_progress (as agent would report)
    // Phase: PreCopyDisk
    update_migration_progress(
        &cluster.pool,
        "node-a",
        "vm-001",
        "op-mig-001",
        proto::MigrationPhase::PrecopyDisk as i32,
        0,
        10_737_418_240, // 10GB
        0,
        100_000,
    )
    .await
    .expect("failed to update to PreCopyDisk");

    let phase: (String,) =
        sqlx::query_as("SELECT phase FROM migrations WHERE migration_id = 'mig-001'")
            .fetch_one(&cluster.pool)
            .await
            .unwrap();
    assert_eq!(phase.0, "PreCopyDisk");

    // Phase: ConvergingDisk with decreasing dirty blocks
    update_migration_progress(
        &cluster.pool,
        "node-a",
        "vm-001",
        "op-mig-001",
        proto::MigrationPhase::ConvergingDisk as i32,
        5_000_000_000,
        10_737_418_240,
        1,
        50_000,
    )
    .await
    .unwrap();

    update_migration_progress(
        &cluster.pool,
        "node-a",
        "vm-001",
        "op-mig-001",
        proto::MigrationPhase::ConvergingDisk as i32,
        8_000_000_000,
        10_737_418_240,
        2,
        10_000,
    )
    .await
    .unwrap();

    // Verify convergence round and dirty blocks decreasing
    let row: (i32, i64) = sqlx::query_as(
        "SELECT convergence_round, dirty_blocks_remaining FROM migrations WHERE migration_id = 'mig-001'",
    )
    .fetch_one(&cluster.pool)
    .await
    .unwrap();
    assert_eq!(row.0, 2);
    assert_eq!(row.1, 10_000);

    // Phase: MemoryMigration
    update_migration_progress(
        &cluster.pool,
        "node-a",
        "vm-001",
        "op-mig-001",
        proto::MigrationPhase::MemoryMigration as i32,
        10_000_000_000,
        10_737_418_240,
        2,
        500,
    )
    .await
    .unwrap();

    // Phase: Completed
    update_migration_progress(
        &cluster.pool,
        "node-a",
        "vm-001",
        "op-mig-001",
        proto::MigrationPhase::Completed as i32,
        10_737_418_240,
        10_737_418_240,
        2,
        0,
    )
    .await
    .unwrap();

    let row: (String, Option<String>) =
        sqlx::query_as("SELECT phase, completed_at FROM migrations WHERE migration_id = 'mig-001'")
            .fetch_one(&cluster.pool)
            .await
            .unwrap();
    assert_eq!(row.0, "Completed");
    assert!(row.1.is_some(), "completed_at should be set");

    // Simulate VM placement update (as orchestrator would do after migration)
    sqlx::query("UPDATE vm_desired_state SET target_node_id = 'node-b' WHERE vm_id = 'vm-001'")
        .execute(&cluster.pool)
        .await
        .unwrap();

    let target: (String,) =
        sqlx::query_as("SELECT target_node_id FROM vm_desired_state WHERE vm_id = 'vm-001'")
            .fetch_one(&cluster.pool)
            .await
            .unwrap();
    assert_eq!(target.0, "node-b");
}

// Scenario 2: Disk convergence — verify rounds occur and dirty count decreases
#[tokio::test]
async fn test_disk_convergence_dirty_block_decrease() {
    let cluster = TestCluster::new().await;
    cluster.setup_two_nodes().await;
    cluster
        .create_vm_on_node(
            "vm-conv",
            "node-a",
            "net-001",
            "aa:bb:cc:dd:ee:02",
            2_147_483_648,
        )
        .await;
    cluster.create_operation("op-conv-001").await;

    let state = MigrationState {
        migration_id: "mig-conv-001".to_string(),
        operation_id: "op-conv-001".to_string(),
        vm_id: "vm-conv".to_string(),
        source_node_id: "node-a".to_string(),
        dest_node_id: "node-b".to_string(),
        phase: MigrationPhase::Pending,
        config: MigrationConfig {
            dirty_threshold_blocks: 1024,
            max_convergence_rounds: 5,
            block_size_bytes: 4_194_304,
            total_timeout_seconds: 0,
            timeout_multiplier: 1.0,
            pause_first: false,
        },
        bytes_transferred: 0,
        total_bytes: 0,
        convergence_round: 0,
        dirty_blocks_remaining: 0,
    };

    create_migration_record(&cluster.pool, &state)
        .await
        .unwrap();

    // Simulate multiple convergence rounds with decreasing dirty blocks
    let dirty_sequence: Vec<(u32, u64)> = vec![
        (1, 100_000), // round 1: 100k dirty blocks
        (2, 50_000),  // round 2: 50k
        (3, 10_000),  // round 3: 10k
        (4, 2_000),   // round 4: 2k
        (5, 500),     // round 5: 500 (below threshold of 1024)
    ];

    for (round, dirty) in &dirty_sequence {
        update_migration_progress(
            &cluster.pool,
            "node-a",
            "vm-conv",
            "op-conv-001",
            proto::MigrationPhase::ConvergingDisk as i32,
            0,
            10_737_418_240,
            *round,
            *dirty,
        )
        .await
        .unwrap();
    }

    // Verify final state: round 5, dirty 500
    let row: (i32, i64) = sqlx::query_as(
        "SELECT convergence_round, dirty_blocks_remaining FROM migrations WHERE migration_id = 'mig-conv-001'",
    )
    .fetch_one(&cluster.pool)
    .await
    .unwrap();
    assert_eq!(row.0, 5);
    assert_eq!(row.1, 500);
    // 500 < 1024 threshold: convergence achieved
    assert!(
        row.1 <= state.config.dirty_threshold_blocks as i64,
        "dirty blocks {} should be <= threshold {}",
        row.1,
        state.config.dirty_threshold_blocks
    );
}

// Scenario 3: Failure in PreCopyDisk — verify rollback, VM continues on source
#[tokio::test]
async fn test_failure_in_precopy_disk_rollback() {
    let cluster = TestCluster::new().await;
    cluster.setup_two_nodes().await;
    cluster
        .create_vm_on_node(
            "vm-fail-pre",
            "node-a",
            "net-001",
            "aa:bb:cc:dd:ee:03",
            1_073_741_824,
        )
        .await;
    cluster.create_operation("op-fail-pre").await;

    let state = MigrationState {
        migration_id: "mig-fail-pre".to_string(),
        operation_id: "op-fail-pre".to_string(),
        vm_id: "vm-fail-pre".to_string(),
        source_node_id: "node-a".to_string(),
        dest_node_id: "node-b".to_string(),
        phase: MigrationPhase::Pending,
        config: MigrationConfig::default(),
        bytes_transferred: 0,
        total_bytes: 0,
        convergence_round: 0,
        dirty_blocks_remaining: 0,
    };

    create_migration_record(&cluster.pool, &state)
        .await
        .unwrap();

    // Simulate agent reporting RolledBack during PreCopyDisk
    // (e.g., dest stord killed mid-stream)
    update_migration_progress(
        &cluster.pool,
        "node-a",
        "vm-fail-pre",
        "op-fail-pre",
        proto::MigrationPhase::PrecopyDisk as i32,
        1_000_000_000,
        10_737_418_240,
        0,
        0,
    )
    .await
    .unwrap();

    // Dest stord failure -> agent reports RolledBack
    update_migration_progress(
        &cluster.pool,
        "node-a",
        "vm-fail-pre",
        "op-fail-pre",
        proto::MigrationPhase::RolledBack as i32,
        1_000_000_000,
        10_737_418_240,
        0,
        0,
    )
    .await
    .unwrap();

    // Verify: migration marked RolledBack
    let row: (String, Option<String>) = sqlx::query_as(
        "SELECT phase, completed_at FROM migrations WHERE migration_id = 'mig-fail-pre'",
    )
    .fetch_one(&cluster.pool)
    .await
    .unwrap();
    assert_eq!(row.0, "RolledBack");
    assert!(row.1.is_some(), "completed_at should be set on rollback");

    // Verify: VM still on source node (target_node_id unchanged)
    let target: (String,) =
        sqlx::query_as("SELECT target_node_id FROM vm_desired_state WHERE vm_id = 'vm-fail-pre'")
            .fetch_one(&cluster.pool)
            .await
            .unwrap();
    assert_eq!(
        target.0, "node-a",
        "VM should remain on source node after rollback"
    );
}

// Scenario 4: Failure in MemoryMigration — marked Failed (no clean rollback)
#[tokio::test]
async fn test_failure_in_memory_migration_marked_failed() {
    let cluster = TestCluster::new().await;
    cluster.setup_two_nodes().await;
    cluster
        .create_vm_on_node(
            "vm-fail-mem",
            "node-a",
            "net-001",
            "aa:bb:cc:dd:ee:04",
            2_147_483_648,
        )
        .await;
    cluster.create_operation("op-fail-mem").await;

    let state = MigrationState {
        migration_id: "mig-fail-mem".to_string(),
        operation_id: "op-fail-mem".to_string(),
        vm_id: "vm-fail-mem".to_string(),
        source_node_id: "node-a".to_string(),
        dest_node_id: "node-b".to_string(),
        phase: MigrationPhase::Pending,
        config: MigrationConfig::default(),
        bytes_transferred: 0,
        total_bytes: 0,
        convergence_round: 0,
        dirty_blocks_remaining: 0,
    };

    create_migration_record(&cluster.pool, &state)
        .await
        .unwrap();

    // Progress through precopy and convergence
    update_migration_progress(
        &cluster.pool,
        "node-a",
        "vm-fail-mem",
        "op-fail-mem",
        proto::MigrationPhase::PrecopyDisk as i32,
        5_000_000_000,
        10_737_418_240,
        0,
        50_000,
    )
    .await
    .unwrap();

    update_migration_progress(
        &cluster.pool,
        "node-a",
        "vm-fail-mem",
        "op-fail-mem",
        proto::MigrationPhase::ConvergingDisk as i32,
        9_000_000_000,
        10_737_418_240,
        3,
        500,
    )
    .await
    .unwrap();

    // Enter memory migration
    update_migration_progress(
        &cluster.pool,
        "node-a",
        "vm-fail-mem",
        "op-fail-mem",
        proto::MigrationPhase::MemoryMigration as i32,
        10_000_000_000,
        10_737_418_240,
        3,
        0,
    )
    .await
    .unwrap();

    // Dest agent killed mid-transfer -> agent reports Failed
    update_migration_progress(
        &cluster.pool,
        "node-a",
        "vm-fail-mem",
        "op-fail-mem",
        proto::MigrationPhase::Failed as i32,
        10_200_000_000,
        10_737_418_240,
        3,
        0,
    )
    .await
    .unwrap();

    // Verify: migration marked Failed
    let row: (String, Option<String>) = sqlx::query_as(
        "SELECT phase, completed_at FROM migrations WHERE migration_id = 'mig-fail-mem'",
    )
    .fetch_one(&cluster.pool)
    .await
    .unwrap();
    assert_eq!(row.0, "Failed");
    assert!(row.1.is_some(), "completed_at should be set on failure");
}

// Scenario 5: Network continuity — two VMs on same VNI across nodes, FDB entries correct
#[tokio::test]
async fn test_network_continuity_vni_allocation_and_vtep_lookup() {
    let cluster = TestCluster::new().await;
    cluster.setup_two_nodes().await;

    // Create overlay network and two VMs on different nodes
    cluster
        .create_vm_on_node(
            "vm-net-a",
            "node-a",
            "net-vxlan",
            "aa:bb:cc:00:00:01",
            1_073_741_824,
        )
        .await;
    cluster
        .create_vm_on_node(
            "vm-net-b",
            "node-b",
            "net-vxlan",
            "aa:bb:cc:00:00:02",
            1_073_741_824,
        )
        .await;

    // Allocate VNI for the network
    let vni = cluster
        .vtep_repo
        .allocate_vni("net-vxlan")
        .await
        .expect("failed to allocate VNI");
    assert!(vni >= 1, "VNI should be >= 1, got {vni}");
    assert!(vni <= 16_777_214, "VNI should be <= 16777214, got {vni}");

    // Verify VNI stored for network
    let stored_vni = cluster
        .vtep_repo
        .get_vni_for_network("net-vxlan")
        .await
        .unwrap();
    assert_eq!(stored_vni, Some(vni));

    // Get VTEPs for the network (both nodes have VMs on it)
    let vteps = cluster
        .vtep_repo
        .get_vteps_for_network("net-vxlan")
        .await
        .expect("failed to get VTEPs for network");
    assert_eq!(
        vteps.len(),
        2,
        "both nodes should have VTEPs for this network"
    );

    let vtep_ips: Vec<&str> = vteps.iter().map(|v| v.vtep_ip.as_str()).collect();
    assert!(
        vtep_ips.contains(&"10.0.0.1"),
        "node-a VTEP should be present"
    );
    assert!(
        vtep_ips.contains(&"10.0.0.2"),
        "node-b VTEP should be present"
    );
}

// Scenario 6: Security policy — store and retrieve deny rules
#[tokio::test]
async fn test_security_policy_store_deny_rule() {
    let cluster = TestCluster::new().await;
    cluster.setup_two_nodes().await;
    cluster
        .create_vm_on_node(
            "vm-sec",
            "node-a",
            "net-sec",
            "aa:bb:cc:dd:ee:06",
            1_073_741_824,
        )
        .await;

    // Insert a security policy with a deny rule
    let rules_json = serde_json::json!([
        {
            "direction": "ingress",
            "protocol": "tcp",
            "port": 22,
            "action": "deny",
            "source_cidr": "0.0.0.0/0"
        },
        {
            "direction": "egress",
            "protocol": "udp",
            "port": 53,
            "action": "allow",
            "destination_cidr": "8.8.8.8/32"
        }
    ]);

    sqlx::query(
        r#"INSERT INTO security_policies (policy_id, vm_id, network_id, default_action, rules_json)
           VALUES (?, ?, ?, ?, ?)"#,
    )
    .bind("pol-001")
    .bind("vm-sec")
    .bind("net-sec")
    .bind("deny")
    .bind(rules_json.to_string())
    .execute(&cluster.pool)
    .await
    .expect("failed to create security policy");

    // Verify the rule is stored
    let row: (String, String, String) = sqlx::query_as(
        "SELECT default_action, rules_json, vm_id FROM security_policies WHERE policy_id = 'pol-001'",
    )
    .fetch_one(&cluster.pool)
    .await
    .unwrap();
    assert_eq!(row.0, "deny");
    assert_eq!(row.2, "vm-sec");

    let stored_rules: serde_json::Value = serde_json::from_str(&row.1).unwrap();
    assert_eq!(stored_rules.as_array().unwrap().len(), 2);
    assert_eq!(stored_rules[0]["action"], "deny");
    assert_eq!(stored_rules[0]["port"], 22);
    assert_eq!(stored_rules[1]["action"], "allow");
}

// =============================================================================
// 9.2 — Additional overlay/VNI lifecycle tests
// =============================================================================

#[tokio::test]
async fn test_vni_allocation_and_release_lifecycle() {
    let cluster = TestCluster::new().await;
    cluster.setup_two_nodes().await;

    // Create a network record
    cluster
        .create_vm_on_node(
            "vm-vni",
            "node-a",
            "net-vni-test",
            "aa:bb:cc:00:01:01",
            1_073_741_824,
        )
        .await;

    // Allocate VNI
    let vni1 = cluster
        .vtep_repo
        .allocate_vni("net-vni-test")
        .await
        .unwrap();
    assert!(vni1 >= 1);

    // Allocate a second VNI for a different network
    // Need to create second network first
    cluster
        .create_vm_on_node(
            "vm-vni2",
            "node-b",
            "net-vni-test2",
            "aa:bb:cc:00:01:02",
            1_073_741_824,
        )
        .await;
    let vni2 = cluster
        .vtep_repo
        .allocate_vni("net-vni-test2")
        .await
        .unwrap();
    assert_ne!(vni1, vni2, "VNIs should be distinct");

    // Release first VNI
    cluster
        .vtep_repo
        .release_vni("net-vni-test")
        .await
        .expect("failed to release VNI");

    // Verify VNI is released
    let released_vni = cluster
        .vtep_repo
        .get_vni_for_network("net-vni-test")
        .await
        .unwrap();
    assert_eq!(released_vni, None, "VNI should be None after release");

    // Second VNI still active
    let still_active = cluster
        .vtep_repo
        .get_vni_for_network("net-vni-test2")
        .await
        .unwrap();
    assert_eq!(still_active, Some(vni2));
}

#[tokio::test]
async fn test_vtep_registration_updates_existing() {
    let cluster = TestCluster::new().await;
    cluster.setup_two_nodes().await;

    // Update VTEP IP for node-a
    cluster
        .vtep_repo
        .register_vtep("node-a", "192.168.1.100", 4790)
        .await
        .expect("failed to update VTEP");

    let vtep = cluster.vtep_repo.get_vtep("node-a").await.unwrap();
    assert_eq!(vtep.vtep_ip, "192.168.1.100");
    assert_eq!(vtep.vtep_port, 4790);
}

#[tokio::test]
async fn test_enrollment_vtep_registration() {
    let cluster = TestCluster::new().await;

    // Enroll a new node and register VTEP (simulating what EnrollmentService does)
    cluster
        .node_repo
        .upsert_node(&NodeUpsertInput {
            node_id: NodeId::new("node-new").unwrap(),
            hostname: "host-new".to_string(),
            display_name: "New Node".to_string(),
            certificate_serial: None,
            agent_version: Some("0.1.0".to_string()),
            control_plane_version: Some("0.1.0".to_string()),
            enrolled_unix_ms: 2000,
            last_seen_unix_ms: 2000,
        })
        .await
        .unwrap();

    cluster
        .vtep_repo
        .register_vtep("node-new", "172.16.0.50", 4789)
        .await
        .unwrap();

    let vtep = cluster.vtep_repo.get_vtep("node-new").await.unwrap();
    assert_eq!(vtep.node_id, "node-new");
    assert_eq!(vtep.vtep_ip, "172.16.0.50");
    assert_eq!(vtep.vtep_port, 4789);
}

// =============================================================================
// 9.3 — Performance baseline test structures
// =============================================================================

/// Test structure for measuring block streaming throughput.
/// In a real benchmark, this would stream actual blocks and measure time.
/// Here we verify the measurement infrastructure works.
#[tokio::test]
async fn test_performance_baseline_block_streaming_throughput() {
    let cluster = TestCluster::new().await;
    cluster.setup_two_nodes().await;
    cluster
        .create_vm_on_node(
            "vm-perf-blk",
            "node-a",
            "net-001",
            "aa:bb:cc:dd:ee:10",
            4_294_967_296,
        )
        .await;
    cluster.create_operation("op-perf-blk").await;

    let state = MigrationState {
        migration_id: "mig-perf-blk".to_string(),
        operation_id: "op-perf-blk".to_string(),
        vm_id: "vm-perf-blk".to_string(),
        source_node_id: "node-a".to_string(),
        dest_node_id: "node-b".to_string(),
        phase: MigrationPhase::Pending,
        config: MigrationConfig {
            dirty_threshold_blocks: 1024,
            max_convergence_rounds: 10,
            block_size_bytes: 4_194_304, // 4MB blocks
            total_timeout_seconds: 0,
            timeout_multiplier: 1.0,
            pause_first: false,
        },
        bytes_transferred: 0,
        total_bytes: 0,
        convergence_round: 0,
        dirty_blocks_remaining: 0,
    };

    create_migration_record(&cluster.pool, &state)
        .await
        .unwrap();

    // Simulate block streaming: record timestamps and bytes for throughput calculation
    let start = std::time::Instant::now();
    let total_disk_bytes: u64 = 10_737_418_240; // 10 GB
    let block_size: u64 = 4_194_304; // 4 MB
    let total_blocks = total_disk_bytes / block_size;

    // Simulate streaming 100 blocks and updating progress
    let simulated_blocks = 100u64;
    let bytes_per_update = simulated_blocks * block_size;

    update_migration_progress(
        &cluster.pool,
        "node-a",
        "vm-perf-blk",
        "op-perf-blk",
        proto::MigrationPhase::PrecopyDisk as i32,
        bytes_per_update,
        total_disk_bytes,
        0,
        total_blocks - simulated_blocks,
    )
    .await
    .unwrap();

    let elapsed = start.elapsed();

    // Verify progress was recorded
    let row: (i64, i64) = sqlx::query_as(
        "SELECT bytes_transferred, total_bytes FROM migrations WHERE migration_id = 'mig-perf-blk'",
    )
    .fetch_one(&cluster.pool)
    .await
    .unwrap();
    assert_eq!(row.0, bytes_per_update as i64);
    assert_eq!(row.1, total_disk_bytes as i64);

    // Performance assertion: DB update should be fast (< 1s for in-memory SQLite)
    assert!(
        elapsed.as_secs() < 1,
        "block streaming progress update took too long: {:?}",
        elapsed
    );

    // Calculate theoretical throughput
    let throughput_mbps = (bytes_per_update as f64 / 1_048_576.0) / elapsed.as_secs_f64();
    // Just verify we can calculate it (actual value depends on machine)
    assert!(throughput_mbps > 0.0, "throughput should be positive");
}

/// Test structure for measuring total migration time.
/// Verifies the timing infrastructure across all phases.
#[tokio::test]
async fn test_performance_baseline_total_migration_time() {
    let cluster = TestCluster::new().await;
    cluster.setup_two_nodes().await;
    cluster
        .create_vm_on_node(
            "vm-perf-tot",
            "node-a",
            "net-001",
            "aa:bb:cc:dd:ee:11",
            8_589_934_592,
        )
        .await;
    cluster.create_operation("op-perf-tot").await;

    let state = MigrationState {
        migration_id: "mig-perf-tot".to_string(),
        operation_id: "op-perf-tot".to_string(),
        vm_id: "vm-perf-tot".to_string(),
        source_node_id: "node-a".to_string(),
        dest_node_id: "node-b".to_string(),
        phase: MigrationPhase::Pending,
        config: MigrationConfig::default(),
        bytes_transferred: 0,
        total_bytes: 0,
        convergence_round: 0,
        dirty_blocks_remaining: 0,
    };

    create_migration_record(&cluster.pool, &state)
        .await
        .unwrap();

    let migration_start = std::time::Instant::now();

    // Phase 1: PreCopyDisk
    let phase_start = std::time::Instant::now();
    update_migration_progress(
        &cluster.pool,
        "node-a",
        "vm-perf-tot",
        "op-perf-tot",
        proto::MigrationPhase::PrecopyDisk as i32,
        5_000_000_000,
        10_737_418_240,
        0,
        50_000,
    )
    .await
    .unwrap();
    let precopy_duration = phase_start.elapsed();

    // Phase 2: ConvergingDisk
    let phase_start = std::time::Instant::now();
    update_migration_progress(
        &cluster.pool,
        "node-a",
        "vm-perf-tot",
        "op-perf-tot",
        proto::MigrationPhase::ConvergingDisk as i32,
        9_500_000_000,
        10_737_418_240,
        3,
        500,
    )
    .await
    .unwrap();
    let converge_duration = phase_start.elapsed();

    // Phase 3: MemoryMigration
    let phase_start = std::time::Instant::now();
    update_migration_progress(
        &cluster.pool,
        "node-a",
        "vm-perf-tot",
        "op-perf-tot",
        proto::MigrationPhase::MemoryMigration as i32,
        10_500_000_000,
        10_737_418_240,
        3,
        0,
    )
    .await
    .unwrap();
    let memory_duration = phase_start.elapsed();

    // Phase 4: Completed
    let phase_start = std::time::Instant::now();
    update_migration_progress(
        &cluster.pool,
        "node-a",
        "vm-perf-tot",
        "op-perf-tot",
        proto::MigrationPhase::Completed as i32,
        10_737_418_240,
        10_737_418_240,
        3,
        0,
    )
    .await
    .unwrap();
    let complete_duration = phase_start.elapsed();

    let total_duration = migration_start.elapsed();

    // All phase transitions on in-memory DB should be very fast
    assert!(precopy_duration.as_millis() < 100);
    assert!(converge_duration.as_millis() < 100);
    assert!(memory_duration.as_millis() < 100);
    assert!(complete_duration.as_millis() < 100);
    assert!(
        total_duration.as_millis() < 500,
        "total migration state updates took too long: {:?}",
        total_duration
    );

    // Verify final state
    let row: (String, i64) = sqlx::query_as(
        "SELECT phase, bytes_transferred FROM migrations WHERE migration_id = 'mig-perf-tot'",
    )
    .fetch_one(&cluster.pool)
    .await
    .unwrap();
    assert_eq!(row.0, "Completed");
    assert_eq!(row.1, 10_737_418_240);

    // Verify calculated timeouts are reasonable for 10GB disk / 8GB memory
    let timeouts = PhaseTimeouts::calculate(10, 8, 1.0);
    assert_eq!(timeouts.precopy_disk_secs, 600); // 10 * 60
    assert_eq!(timeouts.memory_migration_secs, 360); // 8 * 30 + 120
    assert!(timeouts.total_secs > 0);
}

// =============================================================================
// Additional integration scenarios
// =============================================================================

/// Verify that multiple migrations can be tracked concurrently.
#[tokio::test]
async fn test_concurrent_migrations_tracked_independently() {
    let cluster = TestCluster::new().await;
    cluster.setup_two_nodes().await;
    cluster
        .create_vm_on_node(
            "vm-conc-1",
            "node-a",
            "net-001",
            "aa:bb:cc:dd:ee:20",
            1_073_741_824,
        )
        .await;
    cluster
        .create_vm_on_node(
            "vm-conc-2",
            "node-a",
            "net-001",
            "aa:bb:cc:dd:ee:21",
            1_073_741_824,
        )
        .await;
    cluster.create_operation("op-conc-1").await;
    cluster.create_operation("op-conc-2").await;

    // Create two concurrent migrations
    let state1 = MigrationState {
        migration_id: "mig-conc-1".to_string(),
        operation_id: "op-conc-1".to_string(),
        vm_id: "vm-conc-1".to_string(),
        source_node_id: "node-a".to_string(),
        dest_node_id: "node-b".to_string(),
        phase: MigrationPhase::Pending,
        config: MigrationConfig::default(),
        bytes_transferred: 0,
        total_bytes: 0,
        convergence_round: 0,
        dirty_blocks_remaining: 0,
    };
    let state2 = MigrationState {
        migration_id: "mig-conc-2".to_string(),
        operation_id: "op-conc-2".to_string(),
        vm_id: "vm-conc-2".to_string(),
        source_node_id: "node-a".to_string(),
        dest_node_id: "node-b".to_string(),
        phase: MigrationPhase::Pending,
        config: MigrationConfig::default(),
        bytes_transferred: 0,
        total_bytes: 0,
        convergence_round: 0,
        dirty_blocks_remaining: 0,
    };

    create_migration_record(&cluster.pool, &state1)
        .await
        .unwrap();
    create_migration_record(&cluster.pool, &state2)
        .await
        .unwrap();

    // Progress migration 1 to Completed
    update_migration_progress(
        &cluster.pool,
        "node-a",
        "vm-conc-1",
        "op-conc-1",
        proto::MigrationPhase::Completed as i32,
        10_000_000_000,
        10_000_000_000,
        2,
        0,
    )
    .await
    .unwrap();

    // Progress migration 2 to Failed
    update_migration_progress(
        &cluster.pool,
        "node-a",
        "vm-conc-2",
        "op-conc-2",
        proto::MigrationPhase::Failed as i32,
        5_000_000_000,
        10_000_000_000,
        1,
        50_000,
    )
    .await
    .unwrap();

    // Verify independent states
    let phase1: (String,) =
        sqlx::query_as("SELECT phase FROM migrations WHERE migration_id = 'mig-conc-1'")
            .fetch_one(&cluster.pool)
            .await
            .unwrap();
    let phase2: (String,) =
        sqlx::query_as("SELECT phase FROM migrations WHERE migration_id = 'mig-conc-2'")
            .fetch_one(&cluster.pool)
            .await
            .unwrap();
    assert_eq!(phase1.0, "Completed");
    assert_eq!(phase2.0, "Failed");
}

/// Verify that MigrationConfig roundtrips correctly through correlation_id format.
#[tokio::test]
async fn test_migration_config_roundtrip_via_correlation_id() {
    let config = MigrationConfig {
        dirty_threshold_blocks: 2048,
        max_convergence_rounds: 7,
        block_size_bytes: 8_388_608,
        total_timeout_seconds: 5400,
        timeout_multiplier: 1.0,
        pause_first: false,
    };

    let correlation_id = format!(
        "source=node-a:dest=node-b:threshold={}:rounds={}:block_size={}:timeout={}",
        config.dirty_threshold_blocks,
        config.max_convergence_rounds,
        config.block_size_bytes,
        config.total_timeout_seconds
    );

    let (source, dest, parsed) = MigrationConfig::from_correlation_id(&correlation_id);
    assert_eq!(source, "node-a");
    assert_eq!(dest, "node-b");
    assert_eq!(parsed.dirty_threshold_blocks, config.dirty_threshold_blocks);
    assert_eq!(parsed.max_convergence_rounds, config.max_convergence_rounds);
    assert_eq!(parsed.block_size_bytes, config.block_size_bytes);
    assert_eq!(parsed.total_timeout_seconds, config.total_timeout_seconds);
}

/// Verify overlay manager construction (no actual gRPC calls needed).
#[tokio::test]
async fn test_overlay_manager_construction() {
    let cluster = TestCluster::new().await;
    cluster.setup_two_nodes().await;

    let node_pool = NodeClientPool::new();
    let overlay_manager =
        OverlayManager::new(node_pool, "/tmp/chv/agent/{node_id}/agent.sock".to_string());

    // Verify overlay manager can be cloned (required for concurrent usage)
    let _cloned = overlay_manager.clone();
}

// =============================================================================
// 9.3 — Cooperative migration cancel (S2-5)
// =============================================================================

/// Helper: insert a migration row in a given phase for cancel-flag tests.
///
/// We bypass `create_migration_record` so we can park the row in any phase
/// without driving the full state machine.
async fn insert_migration_at_phase(
    pool: &StorePool,
    migration_id: &str,
    operation_id: &str,
    vm_id: &str,
    phase: &str,
) {
    sqlx::query(
        r#"INSERT INTO migrations
           (migration_id, operation_id, vm_id, source_node_id, destination_node_id, phase)
           VALUES (?, ?, ?, 'node-a', 'node-b', ?)"#,
    )
    .bind(migration_id)
    .bind(operation_id)
    .bind(vm_id)
    .bind(phase)
    .execute(pool)
    .await
    .expect("failed to insert migration row");
}

/// Cancel on a non-existent migration returns NotFound (no row inserted).
#[tokio::test]
async fn test_cancel_unknown_migration_is_not_found() {
    let cluster = TestCluster::new().await;
    cluster.setup_two_nodes().await;

    let outcome = crate::migration::request_migration_cancel(&cluster.pool, "mig-does-not-exist")
        .await
        .expect("cancel should not return Err for missing row");
    assert_eq!(
        outcome,
        crate::migration::CancelMigrationOutcome::NotFound,
        "missing migration must yield NotFound"
    );
}

/// Cancel on a fresh migration sets the flag and is idempotent on a second call.
#[tokio::test]
async fn test_cancel_is_idempotent_when_called_twice() {
    let cluster = TestCluster::new().await;
    cluster.setup_two_nodes().await;
    cluster
        .create_vm_on_node(
            "vm-cancel-1",
            "node-a",
            "net-overlay",
            "aa:bb:cc:dd:ee:11",
            4_294_967_296,
        )
        .await;
    cluster.create_operation("op-cancel-1").await;
    insert_migration_at_phase(
        &cluster.pool,
        "mig-cancel-1",
        "op-cancel-1",
        "vm-cancel-1",
        "ConvergingDisk",
    )
    .await;

    // First call sets the flag.
    let first = crate::migration::request_migration_cancel(&cluster.pool, "mig-cancel-1")
        .await
        .expect("first cancel must succeed");
    assert_eq!(
        first,
        crate::migration::CancelMigrationOutcome::Requested,
        "first cancel must return Requested"
    );

    // Verify the column is now non-NULL.
    let flag: Option<String> = sqlx::query_scalar(
        "SELECT cancel_requested_at FROM migrations WHERE migration_id = 'mig-cancel-1'",
    )
    .fetch_one(&cluster.pool)
    .await
    .unwrap();
    assert!(flag.is_some(), "cancel_requested_at must be populated");

    // Second call must be idempotent and not flip the flag again.
    let second = crate::migration::request_migration_cancel(&cluster.pool, "mig-cancel-1")
        .await
        .expect("second cancel must succeed");
    assert_eq!(
        second,
        crate::migration::CancelMigrationOutcome::AlreadyRequested,
        "second cancel must return AlreadyRequested (idempotent)"
    );

    // The original timestamp must be preserved (no double-write).
    let flag_after: Option<String> = sqlx::query_scalar(
        "SELECT cancel_requested_at FROM migrations WHERE migration_id = 'mig-cancel-1'",
    )
    .fetch_one(&cluster.pool)
    .await
    .unwrap();
    assert_eq!(
        flag, flag_after,
        "cancel_requested_at must not change on idempotent re-call"
    );
}

/// Cancel against a terminal-phase migration is a no-op (does not set flag).
#[tokio::test]
async fn test_cancel_in_terminal_phase_is_noop() {
    let cluster = TestCluster::new().await;
    cluster.setup_two_nodes().await;
    cluster
        .create_vm_on_node(
            "vm-term",
            "node-a",
            "net-overlay",
            "aa:bb:cc:dd:ee:22",
            4_294_967_296,
        )
        .await;

    for (mig_id, op_id, phase) in [
        ("mig-completed", "op-completed", "Completed"),
        ("mig-failed", "op-failed", "Failed"),
        ("mig-rolled", "op-rolled", "RolledBack"),
    ] {
        cluster.create_operation(op_id).await;
        insert_migration_at_phase(&cluster.pool, mig_id, op_id, "vm-term", phase).await;

        let outcome = crate::migration::request_migration_cancel(&cluster.pool, mig_id)
            .await
            .expect("terminal cancel should not return Err");
        assert_eq!(
            outcome,
            crate::migration::CancelMigrationOutcome::AlreadyTerminal,
            "{phase} migration must yield AlreadyTerminal"
        );

        let flag: Option<String> =
            sqlx::query_scalar("SELECT cancel_requested_at FROM migrations WHERE migration_id = ?")
                .bind(mig_id)
                .fetch_one(&cluster.pool)
                .await
                .unwrap();
        assert!(
            flag.is_none(),
            "cancel_requested_at must remain NULL for terminal migration in {phase}"
        );
    }
}

/// `wait_for_convergence` observes the cancel flag at the top of the next
/// iteration and returns an error containing "cancelled by operator". This
/// drives the same code path that `execute_migration` would translate into a
/// `rollback_precopy()` + RolledBack transition.
#[tokio::test]
async fn test_wait_for_convergence_honors_cancel_flag() {
    let cluster = TestCluster::new().await;
    cluster.setup_two_nodes().await;
    cluster
        .create_vm_on_node(
            "vm-cancel-conv",
            "node-a",
            "net-overlay",
            "aa:bb:cc:dd:ee:33",
            4_294_967_296,
        )
        .await;
    cluster.create_operation("op-cancel-conv").await;

    // Park the migration in ConvergingDisk with non-zero dirty blocks so the
    // loop will not converge on its own — it would otherwise poll forever.
    let state = MigrationState {
        migration_id: "mig-cancel-conv".to_string(),
        operation_id: "op-cancel-conv".to_string(),
        vm_id: "vm-cancel-conv".to_string(),
        source_node_id: "node-a".to_string(),
        dest_node_id: "node-b".to_string(),
        phase: MigrationPhase::ConvergingDisk,
        config: MigrationConfig::default(),
        bytes_transferred: 0,
        total_bytes: 10_737_418_240,
        convergence_round: 1,
        dirty_blocks_remaining: 1_000_000, // far above threshold
    };

    create_migration_record(&cluster.pool, &state)
        .await
        .expect("failed to create migration record");

    // Set initial progress so the loop sees a non-converged state.
    update_migration_progress(
        &cluster.pool,
        "node-a",
        "vm-cancel-conv",
        "op-cancel-conv",
        proto::MigrationPhase::ConvergingDisk as i32,
        1_000_000,      // bytes_transferred
        10_737_418_240, // total_bytes
        1,              // convergence_round
        1_000_000,      // dirty_blocks_remaining (well above threshold)
    )
    .await
    .unwrap();

    let pool_for_cancel = cluster.pool.clone();
    let pool_for_loop = cluster.pool.clone();

    // Spawn the convergence loop and a delayed cancel. The loop polls every 5s
    // (hardcoded inside wait_for_convergence), so we must wait at least one
    // poll interval before cancelling. Set the cancel flag after ~500ms; the
    // first iteration finishes its 5s sleep, then the cancel check at the top
    // of iteration 2 trips the early return.
    let convergence_handle = tokio::spawn(async move {
        let local_state = MigrationState {
            migration_id: "mig-cancel-conv".to_string(),
            operation_id: "op-cancel-conv".to_string(),
            vm_id: "vm-cancel-conv".to_string(),
            source_node_id: "node-a".to_string(),
            dest_node_id: "node-b".to_string(),
            phase: MigrationPhase::ConvergingDisk,
            config: MigrationConfig::default(),
            bytes_transferred: 0,
            total_bytes: 10_737_418_240,
            convergence_round: 1,
            dirty_blocks_remaining: 1_000_000,
        };
        crate::migration::wait_for_convergence(&pool_for_loop, &local_state).await
    });

    // Give the loop a moment to enter its first sleep, then signal cancel.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let outcome = crate::migration::request_migration_cancel(&pool_for_cancel, "mig-cancel-conv")
        .await
        .expect("cancel must succeed");
    assert_eq!(
        outcome,
        crate::migration::CancelMigrationOutcome::Requested,
        "cancel must transition NULL → set"
    );

    // The loop must observe cancel within at most ~2 poll intervals (5s each)
    // plus a small buffer. We bound the wait at 20s so a regression cannot
    // hang CI.
    let result = tokio::time::timeout(std::time::Duration::from_secs(20), convergence_handle)
        .await
        .expect("convergence loop must terminate within 20s after cancel")
        .expect("join handle must not panic");

    let err = result.expect_err("wait_for_convergence must return Err on cancel");
    let err_str = format!("{err}");
    assert!(
        err_str.contains("cancelled by operator"),
        "error must identify operator cancel; got: {err_str}"
    );
}

// =============================================================================
// 9.5 — ADR-021 fabric dispatch (Phase 3): planner → per-node fan-out
// =============================================================================

mod fabric_dispatch {
    use super::*;
    use chv_controlplane_store::EventRepository;
    use std::sync::{Arc, Mutex};

    /// Mock agent-side LifecycleService served over UDS: captures
    /// `UpdateOverlay` requests and fails closed (unimplemented) on every
    /// other RPC — the same mock style as the daemon-client mocks in
    /// chv-agent-core.
    #[derive(Clone, Default)]
    struct MockFabricAgent {
        overlay_requests: Arc<Mutex<Vec<proto::UpdateOverlayRequest>>>,
    }

    #[tonic::async_trait]
    impl proto::lifecycle_service_server::LifecycleService for MockFabricAgent {
        // #522 PR 1: the fabric-agent mock does not drive the volume
        // delete carrier (its coverage is the overlay fan-out).
        async fn delete_volume(
            &self,
            _request: tonic::Request<proto::DeleteVolumeRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn apply_network_policy(
            &self,
            _request: tonic::Request<proto::ApplyNetworkPolicyRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn update_overlay(
            &self,
            request: tonic::Request<proto::UpdateOverlayRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            self.overlay_requests
                .lock()
                .unwrap()
                .push(request.into_inner());
            Ok(tonic::Response::new(proto::AckResponse {
                result: Some(proto::ResultMeta {
                    operation_id: String::new(),
                    status: "OK".into(),
                    node_observed_generation: String::new(),
                    error_code: String::new(),
                    human_summary: "fabric plan applied".into(),
                }),
            }))
        }

        async fn create_vm(
            &self,
            _request: tonic::Request<proto::CreateVmRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        // #513 PR 1: the dispatch carrier is dead-but-live — no
        // integration test journals a CreateVolume operation yet (the
        // BFF route is PR 2).
        async fn create_volume(
            &self,
            _request: tonic::Request<proto::CreateVolumeRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn start_vm(
            &self,
            _request: tonic::Request<proto::StartVmRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn stop_vm(
            &self,
            _request: tonic::Request<proto::StopVmRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn reboot_vm(
            &self,
            _request: tonic::Request<proto::RebootVmRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn delete_vm(
            &self,
            _request: tonic::Request<proto::DeleteVmRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn resize_vm(
            &self,
            _request: tonic::Request<proto::ResizeVmRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn pause_vm(
            &self,
            _request: tonic::Request<proto::PauseVmRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn resume_vm(
            &self,
            _request: tonic::Request<proto::ResumeVmRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn power_button_vm(
            &self,
            _request: tonic::Request<proto::PowerButtonVmRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn attach_volume(
            &self,
            _request: tonic::Request<proto::AttachVolumeRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn detach_volume(
            &self,
            _request: tonic::Request<proto::DetachVolumeRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn resize_volume(
            &self,
            _request: tonic::Request<proto::ResizeVolumeRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn snapshot_volume(
            &self,
            _request: tonic::Request<proto::SnapshotVolumeRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn restore_volume(
            &self,
            _request: tonic::Request<proto::RestoreVolumeRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn delete_volume_snapshot(
            &self,
            _request: tonic::Request<proto::DeleteVolumeSnapshotRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn clone_volume(
            &self,
            _request: tonic::Request<proto::CloneVolumeRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn add_disk(
            &self,
            _request: tonic::Request<proto::AddDiskRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn remove_device(
            &self,
            _request: tonic::Request<proto::RemoveDeviceRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn add_net(
            &self,
            _request: tonic::Request<proto::AddNetRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn resize_disk(
            &self,
            _request: tonic::Request<proto::ResizeDiskRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn snapshot_vm(
            &self,
            _request: tonic::Request<proto::SnapshotVmRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn restore_snapshot(
            &self,
            _request: tonic::Request<proto::RestoreSnapshotRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn coredump_vm(
            &self,
            _request: tonic::Request<proto::CoredumpVmRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn ping_vmm(
            &self,
            _request: tonic::Request<proto::PingVmmRequest>,
        ) -> Result<tonic::Response<proto::PingVmmResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn pause_node_scheduling(
            &self,
            _request: tonic::Request<proto::PauseNodeSchedulingRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn resume_node_scheduling(
            &self,
            _request: tonic::Request<proto::ResumeNodeSchedulingRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn drain_node(
            &self,
            _request: tonic::Request<proto::DrainNodeRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn enter_maintenance(
            &self,
            _request: tonic::Request<proto::EnterMaintenanceRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn exit_maintenance(
            &self,
            _request: tonic::Request<proto::ExitMaintenanceRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn start_network(
            &self,
            _request: tonic::Request<proto::StartNetworkRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn stop_network(
            &self,
            _request: tonic::Request<proto::StopNetworkRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn restart_network(
            &self,
            _request: tonic::Request<proto::RestartNetworkRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn migrate_vm(
            &self,
            _request: tonic::Request<proto::MigrateVmRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn send_gratuitous_arp(
            &self,
            _request: tonic::Request<proto::SendGratuitousArpRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }

        async fn resolve_inspect_required_operation(
            &self,
            _request: tonic::Request<proto::ResolveInspectRequiredOperationRequest>,
        ) -> Result<tonic::Response<proto::AckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }
    }

    /// Seed a two-node fabric cluster: identities, endpoints (via direct SQL,
    /// the way the follow-up underlay-addressing wiring will), and VM
    /// placements on a vxlan network with desired generation 7.
    async fn seed_fabric_cluster(cluster: &TestCluster) {
        cluster
            .create_vm_on_node(
                "vm-fab-a",
                "node-a",
                "net-fabric",
                "aa:bb:cc:dd:ee:11",
                1_073_741_824,
            )
            .await;
        cluster
            .create_vm_on_node(
                "vm-fab-b",
                "node-b",
                "net-fabric",
                "aa:bb:cc:dd:ee:12",
                1_073_741_824,
            )
            .await;
        sqlx::query("UPDATE networks SET overlay_type = 'vxlan' WHERE network_id = 'net-fabric'")
            .execute(&cluster.pool)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE network_desired_state SET desired_generation = 7 WHERE network_id = 'net-fabric'",
        )
        .execute(&cluster.pool)
        .await
        .unwrap();

        cluster
            .vtep_repo
            .register_fabric_identity(
                "node-a",
                "pub-aAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
                1500,
                None,
            )
            .await
            .unwrap();
        cluster
            .vtep_repo
            .register_fabric_identity(
                "node-b",
                "pub-bBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB=",
                0,
                None,
            )
            .await
            .unwrap();
        for (node, endpoint) in [("node-a", "10.0.0.1:65001"), ("node-b", "10.0.0.2:65001")] {
            sqlx::query("UPDATE vtep_registry SET underlay_endpoint = ? WHERE node_id = ?")
                .bind(endpoint)
                .bind(node)
                .execute(&cluster.pool)
                .await
                .unwrap();
        }
    }

    /// Serve a mock agent on a UDS socket per node matching `pattern`.
    fn spawn_mock_agents(pattern: &str, agent: MockFabricAgent, nodes: &[&str]) {
        for node in nodes {
            let socket = pattern.replace("{node_id}", node);
            let uds = tokio::net::UnixListener::bind(&socket).unwrap();
            let service =
                proto::lifecycle_service_server::LifecycleServiceServer::new(agent.clone());
            tokio::spawn(async move {
                tonic::transport::Server::builder()
                    .add_service(service)
                    .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds))
                    .await
                    .ok();
            });
        }
    }

    #[tokio::test]
    async fn update_overlay_dispatch_fans_out_per_node_fabric_plans() {
        let cluster = TestCluster::new().await;
        cluster.setup_two_nodes().await;
        seed_fabric_cluster(&cluster).await;

        let dir = tempfile::tempdir().unwrap();
        let pattern = dir
            .path()
            .join("agent-{node_id}.sock")
            .to_str()
            .unwrap()
            .to_string();
        let agent = MockFabricAgent::default();
        spawn_mock_agents(&pattern, agent.clone(), &["node-a", "node-b"]);

        // Journal the UpdateOverlay operation through the lifecycle service
        // (the same path the API/BFF trigger takes).
        let event_repo = EventRepository::new(cluster.pool.clone());
        let lifecycle = crate::lifecycle::LifecycleServiceImplementation::new(
            cluster.node_repo.clone(),
            cluster.operation_repo.clone(),
            event_repo,
            cluster.desired_state_repo.clone(),
        );
        let ack = crate::lifecycle::LifecycleService::update_overlay(
            &lifecycle,
            proto::UpdateOverlayRequest {
                meta: Some(proto::RequestMeta {
                    operation_id: String::new(),
                    requested_by: "test".into(),
                    target_node_id: "node-a".into(),
                    desired_state_version: "1".into(),
                    request_unix_ms: 0,
                }),
                node_id: "node-a".into(),
                network_id: "net-fabric".into(),
                vni: 0,
                vtep_endpoints: vec![],
                fdb_entries: vec![],
                fabric: None,
            },
        )
        .await
        .expect("journaling UpdateOverlay must succeed");
        let operation_id = ack.result.expect("ack meta").operation_id;
        assert!(!operation_id.is_empty(), "operation id must be assigned");

        let status: String =
            sqlx::query_scalar("SELECT status FROM operations WHERE operation_id = ?")
                .bind(&operation_id)
                .fetch_one(&cluster.pool)
                .await
                .unwrap();
        assert_eq!(status, "Accepted", "journalled operation starts Accepted");

        // Dispatch through the orchestrator (claim → compile → fan-out).
        let node_pool = NodeClientPool::new();
        let overlay_manager = OverlayManager::new(node_pool.clone(), pattern.clone());
        let orchestrator = crate::orchestrator::Orchestrator::new(
            cluster.pool.clone(),
            cluster.operation_repo.clone(),
            pattern.clone(),
            "/tmp/vmlinux".to_string(),
            "/tmp/firmware".to_string(),
            node_pool,
            crate::convergence_metrics::new_shared(),
        )
        .with_overlay_manager(overlay_manager);

        orchestrator
            .dispatch_update_overlay(&operation_id, "net-fabric")
            .await
            .expect("fabric dispatch must succeed");

        // Both participating nodes received their own per-node fabric plan.
        let requests = agent.overlay_requests.lock().unwrap().clone();
        assert_eq!(
            requests.len(),
            2,
            "one UpdateOverlay per participating node"
        );

        let by_node: std::collections::HashMap<&str, &proto::UpdateOverlayRequest> =
            requests.iter().map(|r| (r.node_id.as_str(), r)).collect();

        for (node, other, other_key, other_endpoint) in [
            (
                "node-a",
                "node-b",
                "pub-bBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB=",
                "10.0.0.2:65001",
            ),
            (
                "node-b",
                "node-a",
                "pub-aAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
                "10.0.0.1:65001",
            ),
        ] {
            let req = by_node[node];
            let plan = req
                .fabric
                .as_ref()
                .unwrap_or_else(|| panic!("{node} must receive a fabric plan"));
            assert_eq!(plan.local_host_id, node);
            assert_eq!(plan.fabric_domain_id, "chv-default");
            assert_eq!(plan.plan_generation, 7);
            assert_eq!(plan.binding_generation, 1);
            assert_eq!(req.vni, plan_vni(&requests), "vni consistent across nodes");
            // MTU: only node-a measured (1500) -> tenant 1390, fabric 1440.
            assert_eq!(plan.tenant_mtu, 1390);
            assert_eq!(plan.fabric_mtu, 1440);
            // Generation fencing: the request carries the plan generation.
            assert_eq!(
                req.meta.as_ref().unwrap().desired_state_version,
                "7",
                "desired_state_version must fence on plan_generation"
            );
            // Bounded flood list: the other participant, never self.
            assert_eq!(plan.peers.len(), 1);
            assert_eq!(plan.peers[0].node_id, other);
            assert_eq!(plan.peers[0].public_key, other_key);
            assert_eq!(plan.peers[0].underlay_endpoint, other_endpoint);
        }

        // The operation completed successfully.
        let status: String =
            sqlx::query_scalar("SELECT status FROM operations WHERE operation_id = ?")
                .bind(&operation_id)
                .fetch_one(&cluster.pool)
                .await
                .unwrap();
        assert_eq!(status, "Succeeded");
    }

    fn plan_vni(requests: &[proto::UpdateOverlayRequest]) -> u32 {
        requests.first().expect("at least one request").vni
    }

    #[tokio::test]
    async fn update_overlay_dispatch_fails_closed_without_overlay_manager() {
        let cluster = TestCluster::new().await;
        cluster.setup_two_nodes().await;
        seed_fabric_cluster(&cluster).await;

        let orchestrator = crate::orchestrator::Orchestrator::new(
            cluster.pool.clone(),
            cluster.operation_repo.clone(),
            "/nonexistent/agent-{node_id}.sock".to_string(),
            "/tmp/vmlinux".to_string(),
            "/tmp/firmware".to_string(),
            NodeClientPool::new(),
            crate::convergence_metrics::new_shared(),
        );

        let err = orchestrator
            .dispatch_update_overlay("op-fab-x", "net-fabric")
            .await
            .expect_err("dispatch without overlay manager must fail");
        assert!(err.to_string().contains("overlay manager"), "got: {err}");
    }

    /// #582 regression (default mode): `wait_for_convergence` used to
    /// declare convergence on the first status poll of *any*
    /// migration — `dirty_blocks_remaining` is 0 until stord's first
    /// dirty-sync round snapshots the bitmap (and the row is created
    /// with 0), so `dirty <= threshold` was vacuously true while
    /// bulk copy was still running. The memory phase then ran under
    /// its own timeout budget with the VM paused for the remainder of
    /// the disk transfer.
    ///
    /// This pins the guard's round signal with TRUTHFUL row values
    /// (stord's `bytes_transferred` only accumulates dirty re-send
    /// bytes — it stays 0 through bulk copy, and remains far below
    /// `total_bytes` at clean convergence): a pre-telemetry row and a
    /// bulk-copy row (both round 0, dirty 0) must NOT converge; a
    /// dirty-sync row (round 1, dirty below threshold) must.
    #[tokio::test]
    async fn test_wait_for_convergence_does_not_declare_convergence_during_bulk_copy() {
        let cluster = TestCluster::new().await;
        cluster.setup_two_nodes().await;
        cluster
            .create_vm_on_node(
                "vm-conv-bulk",
                "node-a",
                "net-overlay",
                "aa:bb:cc:dd:ee:41",
                4_294_967_296,
            )
            .await;
        cluster.create_operation("op-conv-bulk").await;

        let state = MigrationState {
            migration_id: "mig-conv-bulk".to_string(),
            operation_id: "op-conv-bulk".to_string(),
            vm_id: "vm-conv-bulk".to_string(),
            source_node_id: "node-a".to_string(),
            dest_node_id: "node-b".to_string(),
            phase: MigrationPhase::ConvergingDisk,
            config: MigrationConfig::default(),
            bytes_transferred: 0,
            total_bytes: 10_737_418_240,
            convergence_round: 0,
            dirty_blocks_remaining: 0,
        };
        create_migration_record(&cluster.pool, &state)
            .await
            .expect("failed to create migration record");

        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let pool_for_loop = cluster.pool.clone();
        let local_state = state.clone();
        tokio::spawn(async move {
            let result = crate::migration::wait_for_convergence(&pool_for_loop, &local_state).await;
            let _ = tx.send(result).await;
        });

        // Window 1 — pre-telemetry: the loop polls every 5s and the
        // row still holds its creation values (round 0, dirty 0,
        // bytes 0, total 0). The pre-fix loop declared convergence
        // HERE — the "first 5s status poll of any migration" from the
        // issue.
        tokio::time::sleep(std::time::Duration::from_secs(7)).await;
        assert!(
            matches!(
                rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "convergence must NOT be declared before the agent has reported \
             any progress (round 0, dirty 0, total 0) — the recorded #582 looseness"
        );

        // Window 2 — bulk copy underway: the agent reports every ~5s
        // (phase PreCopyDisk) but stord exposes no bulk progress —
        // `bytes_transferred` stays 0 and no dirty-sync round has run
        // (round 0, dirty 0). This is the truthful mid-bulk shape.
        update_migration_progress(
            &cluster.pool,
            "node-a",
            "vm-conv-bulk",
            "op-conv-bulk",
            proto::MigrationPhase::PrecopyDisk as i32,
            0,              // bytes_transferred — stays 0 through bulk copy
            10_737_418_240, // total_bytes
            0,              // convergence_round — no dirty round yet
            0,              // dirty_blocks_remaining — vacuously 0
        )
        .await
        .unwrap();

        tokio::time::sleep(std::time::Duration::from_secs(7)).await;
        assert!(
            matches!(
                rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "convergence must NOT be declared while bulk copy is still running \
             (round 0, dirty 0, bytes 0 — bulk progress is invisible in the row)"
        );

        // The disk transfer finishes bulk copy and enters dirty sync:
        // round 1 (stord sets it at the top of round 1), dirty below
        // the default threshold of 1024 blocks. TRUTHFUL bytes: only
        // the dirty re-sends count (500 blocks x 4MB) — far below
        // total_bytes, which is exactly what a cleanly converged
        // migration reports.
        update_migration_progress(
            &cluster.pool,
            "node-a",
            "vm-conv-bulk",
            "op-conv-bulk",
            proto::MigrationPhase::ConvergingDisk as i32,
            500 * 4_194_304, // bytes_transferred — dirty re-sends only
            10_737_418_240,  // total_bytes
            1,               // convergence_round — dirty sync reached
            500,             // dirty_blocks_remaining — below threshold
        )
        .await
        .unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_secs(20), rx.recv())
            .await
            .expect("convergence loop must terminate within 20s of the converged row")
            .expect("sender must not be dropped");
        result.expect("convergence must be declared once dirty sync reports below threshold");
    }

    /// #582 boundary pin (multi-volume skew, DISCLOSED RESIDUAL): the
    /// agent reports the max round and max dirty across volumes, so
    /// an early volume can be converged (round >= 1, dirty 0) while a
    /// later volume is still in bulk copy — and the row cannot
    /// express that state today (stord's `bytes_transferred` excludes
    /// bulk bytes; the agent's phase vocabulary has no "all volumes
    /// finished bulk" value). The guarded check therefore converges
    /// in that window. This test PINS the residual so it cannot drift
    /// silently: the migration still completes correctly (the final
    /// sync waits for every volume), the cost being that the memory
    /// phase's timeout budget starts early. Closing it needs a
    /// telemetry contract change — see the adoption record
    /// (`docs/design/issue-394-write-canary.md`).
    #[tokio::test]
    async fn test_wait_for_convergence_multi_volume_skew_residual_is_pinned() {
        let cluster = TestCluster::new().await;
        cluster.setup_two_nodes().await;
        cluster
            .create_vm_on_node(
                "vm-conv-skew",
                "node-a",
                "net-overlay",
                "aa:bb:cc:dd:ee:42",
                4_294_967_296,
            )
            .await;
        cluster.create_operation("op-conv-skew").await;

        let state = MigrationState {
            migration_id: "mig-conv-skew".to_string(),
            operation_id: "op-conv-skew".to_string(),
            vm_id: "vm-conv-skew".to_string(),
            source_node_id: "node-a".to_string(),
            dest_node_id: "node-b".to_string(),
            phase: MigrationPhase::ConvergingDisk,
            config: MigrationConfig::default(),
            bytes_transferred: 0,
            total_bytes: 10_737_418_240,
            convergence_round: 0,
            dirty_blocks_remaining: 0,
        };
        create_migration_record(&cluster.pool, &state)
            .await
            .expect("failed to create migration record");

        // Multi-volume skew shape, TRUTHFUL values: volume A has
        // finished bulk copy and converged (its round pushes the max
        // to 1, its dirty is 0 — and volume B, still in bulk copy,
        // reports dirty 0 too, so the max is 0). Volume B's bytes are
        // invisible: stord's bytes_transferred stays 0 during its
        // bulk copy, so the row shows only A's re-sends.
        update_migration_progress(
            &cluster.pool,
            "node-a",
            "vm-conv-skew",
            "op-conv-skew",
            proto::MigrationPhase::ConvergingDisk as i32,
            2_097_152,      // bytes_transferred — volume A's re-sends only
            10_737_418_240, // total_bytes — both volumes
            1,              // convergence_round — early volume in dirty sync
            0,              // dirty_blocks_remaining — early volume converged
        )
        .await
        .unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let pool_for_loop = cluster.pool.clone();
        let local_state = state.clone();
        tokio::spawn(async move {
            let result = crate::migration::wait_for_convergence(&pool_for_loop, &local_state).await;
            let _ = tx.send(result).await;
        });

        // The residual boundary: convergence IS declared here (round
        // 1, dirty 0) while volume B's bulk copy is still running. In
        // the real flow the agent reports MemoryMigration during the
        // same window (needs_vm_pause from volume A's final sync), so
        // the phase-based signal converges it too — the same
        // boundary, two triggers. If this assert ever fails, the
        // boundary moved — either the telemetry contract gained an
        // all-volumes signal (good: delete this pin and guard against
        // the skew) or the guard regressed (bad: see the adoption
        // record).
        let result = tokio::time::timeout(std::time::Duration::from_secs(15), rx.recv())
            .await
            .expect("the pinned residual converges within one poll cycle")
            .expect("sender must not be dropped");
        result.expect("the pinned multi-volume-skew residual converges (see comment)");
    }

    /// #582 liveness pin: the stall detector is keyed on row
    /// freshness, not byte movement — stord's `bytes_transferred`
    /// freezes at 0 through bulk copy, so a byte-based detector would
    /// false-fire ~65s into any legitimate bulk copy. A row that
    /// stops being updated for over 90s (the agent reports every
    /// ~5s) means the agent stopped reporting: the migration must be
    /// rolled back, not waited on.
    #[tokio::test]
    async fn test_wait_for_convergence_fails_when_agent_stops_reporting() {
        let cluster = TestCluster::new().await;
        cluster.setup_two_nodes().await;
        cluster
            .create_vm_on_node(
                "vm-conv-stale",
                "node-a",
                "net-overlay",
                "aa:bb:cc:dd:ee:43",
                4_294_967_296,
            )
            .await;
        cluster.create_operation("op-conv-stale").await;

        let state = MigrationState {
            migration_id: "mig-conv-stale".to_string(),
            operation_id: "op-conv-stale".to_string(),
            vm_id: "vm-conv-stale".to_string(),
            source_node_id: "node-a".to_string(),
            dest_node_id: "node-b".to_string(),
            phase: MigrationPhase::ConvergingDisk,
            config: MigrationConfig::default(),
            bytes_transferred: 0,
            total_bytes: 10_737_418_240,
            convergence_round: 0,
            dirty_blocks_remaining: 0,
        };
        create_migration_record(&cluster.pool, &state)
            .await
            .expect("failed to create migration record");

        // Simulate a dead agent: the row exists but its last update
        // is far older than the 90s freshness threshold.
        sqlx::query("UPDATE migrations SET updated_at = '2020-01-01T00:00:00Z' WHERE migration_id = 'mig-conv-stale'")
            .execute(&cluster.pool)
            .await
            .unwrap();

        let pool_for_loop = cluster.pool.clone();
        let result = tokio::time::timeout(std::time::Duration::from_secs(15), {
            crate::migration::wait_for_convergence(&pool_for_loop, &state)
        })
        .await
        .expect("a stale row must terminate the loop on the first poll");
        let err = result.expect_err("a stale row must fail the convergence wait");
        assert!(
            err.to_string().contains("stalled"),
            "stall error must name the stall, got: {err}"
        );
    }

    /// #582 cap pin: once dirty sync has begun (round >= 1), hitting
    /// the round cap PROCEEDS to the memory phase instead of failing
    /// — matching stord's own forced cutover at MAX_DIRTY_ROUNDS
    /// (the sender moves to final sync there, it does not fail) and
    /// the branch's original stated intent. The pre-fix loop never
    /// reached this code (it exited at the first poll), and erroring
    /// here would roll back migrations that stord is actively
    /// completing via forced cutover.
    #[tokio::test]
    async fn test_wait_for_convergence_proceeds_at_round_cap() {
        let cluster = TestCluster::new().await;
        cluster.setup_two_nodes().await;
        cluster
            .create_vm_on_node(
                "vm-conv-cap",
                "node-a",
                "net-overlay",
                "aa:bb:cc:dd:ee:44",
                4_294_967_296,
            )
            .await;
        cluster.create_operation("op-conv-cap").await;

        let state = MigrationState {
            migration_id: "mig-conv-cap".to_string(),
            operation_id: "op-conv-cap".to_string(),
            vm_id: "vm-conv-cap".to_string(),
            source_node_id: "node-a".to_string(),
            dest_node_id: "node-b".to_string(),
            phase: MigrationPhase::ConvergingDisk,
            config: MigrationConfig::default(),
            bytes_transferred: 0,
            total_bytes: 10_737_418_240,
            convergence_round: 0,
            dirty_blocks_remaining: 0,
        };
        create_migration_record(&cluster.pool, &state)
            .await
            .expect("failed to create migration record");

        // Heavy-writer shape: dirty sync has run the full round
        // budget (10 rounds, the default max_convergence_rounds) and
        // the dirty count is still far above threshold — stord's
        // forced cutover territory.
        update_migration_progress(
            &cluster.pool,
            "node-a",
            "vm-conv-cap",
            "op-conv-cap",
            proto::MigrationPhase::ConvergingDisk as i32,
            50_000 * 4_194_304, // bytes_transferred — heavy re-send history
            10_737_418_240,     // total_bytes
            10,                 // convergence_round — at the cap
            50_000,             // dirty_blocks_remaining — far above threshold
        )
        .await
        .unwrap();

        let pool_for_loop = cluster.pool.clone();
        let result = tokio::time::timeout(std::time::Duration::from_secs(15), {
            crate::migration::wait_for_convergence(&pool_for_loop, &state)
        })
        .await
        .expect("the round cap must terminate the loop on the first poll");
        result.expect("the round cap must proceed to the memory phase, not fail");
    }

    /// #582 liveness pin, isolated: the round guard and the freshness
    /// check are separate nets — the other stale-row test uses a
    /// round-0 row, where the round guard blocks convergence first
    /// and the two pins are coupled. This variant isolates the
    /// freshness branch: the row IS in dirty sync (round 1) with dirty
    /// above threshold, so the primary check cannot fire — only the
    /// stale `updated_at` can terminate the loop.
    #[tokio::test]
    async fn test_wait_for_convergence_fails_on_stale_row_in_dirty_sync() {
        let cluster = TestCluster::new().await;
        cluster.setup_two_nodes().await;
        cluster
            .create_vm_on_node(
                "vm-conv-stale2",
                "node-a",
                "net-overlay",
                "aa:bb:cc:dd:ee:46",
                4_294_967_296,
            )
            .await;
        cluster.create_operation("op-conv-stale2").await;

        let state = MigrationState {
            migration_id: "mig-conv-stale2".to_string(),
            operation_id: "op-conv-stale2".to_string(),
            vm_id: "vm-conv-stale2".to_string(),
            source_node_id: "node-a".to_string(),
            dest_node_id: "node-b".to_string(),
            phase: MigrationPhase::ConvergingDisk,
            config: MigrationConfig::default(),
            bytes_transferred: 0,
            total_bytes: 10_737_418_240,
            convergence_round: 0,
            dirty_blocks_remaining: 0,
        };
        create_migration_record(&cluster.pool, &state)
            .await
            .expect("failed to create migration record");

        // Dirty-sync shape (round 1, dirty far above threshold — the
        // primary check cannot converge this) with a stale timestamp:
        // the agent stopped reporting mid-dirty-sync.
        update_migration_progress(
            &cluster.pool,
            "node-a",
            "vm-conv-stale2",
            "op-conv-stale2",
            proto::MigrationPhase::ConvergingDisk as i32,
            2_097_152,      // bytes_transferred — dirty re-sends
            10_737_418_240, // total_bytes
            1,              // convergence_round — dirty sync reached
            50_000,         // dirty_blocks_remaining — far above threshold
        )
        .await
        .unwrap();
        sqlx::query("UPDATE migrations SET updated_at = '2020-01-01T00:00:00Z' WHERE migration_id = 'mig-conv-stale2'")
            .execute(&cluster.pool)
            .await
            .unwrap();

        let pool_for_loop = cluster.pool.clone();
        let result = tokio::time::timeout(std::time::Duration::from_secs(15), {
            crate::migration::wait_for_convergence(&pool_for_loop, &state)
        })
        .await
        .expect("a stale dirty-sync row must terminate the loop on the first poll");
        let err = result.expect_err("a stale dirty-sync row must fail the convergence wait");
        assert!(
            err.to_string().contains("stalled"),
            "stall error must name the stall, got: {err}"
        );
    }

    /// #582 diskless pin: a VM with no volumes skips the disk loop
    /// entirely — the agent goes straight to memory migration and no
    /// dirty round ever runs, so `convergence_round` stays 0 and the
    /// round signal can never fire. The agent-reported
    /// `MemoryMigration` phase is the convergence signal for that
    /// path (and for "all volumes completed" generally). Pre-fix,
    /// diskless VMs converged only via the vacuous first-poll check.
    #[tokio::test]
    async fn test_wait_for_convergence_converges_for_diskless_vms() {
        let cluster = TestCluster::new().await;
        cluster.setup_two_nodes().await;
        cluster
            .create_vm_on_node(
                "vm-conv-diskless",
                "node-a",
                "net-overlay",
                "aa:bb:cc:dd:ee:45",
                4_294_967_296,
            )
            .await;
        cluster.create_operation("op-conv-diskless").await;

        let state = MigrationState {
            migration_id: "mig-conv-diskless".to_string(),
            operation_id: "op-conv-diskless".to_string(),
            vm_id: "vm-conv-diskless".to_string(),
            source_node_id: "node-a".to_string(),
            dest_node_id: "node-b".to_string(),
            phase: MigrationPhase::ConvergingDisk,
            config: MigrationConfig::default(),
            bytes_transferred: 0,
            total_bytes: 0,
            convergence_round: 0,
            dirty_blocks_remaining: 0,
        };
        create_migration_record(&cluster.pool, &state)
            .await
            .expect("failed to create migration record");

        // Diskless shape, TRUTHFUL values: the agent skipped the disk
        // loop (no volumes) and reported MemoryMigration directly —
        // round 0, dirty 0, no bytes.
        update_migration_progress(
            &cluster.pool,
            "node-a",
            "vm-conv-diskless",
            "op-conv-diskless",
            proto::MigrationPhase::MemoryMigration as i32,
            0, // bytes_transferred
            0, // total_bytes
            0, // convergence_round — no dirty round ever runs
            0, // dirty_blocks_remaining
        )
        .await
        .unwrap();

        let pool_for_loop = cluster.pool.clone();
        let result = tokio::time::timeout(std::time::Duration::from_secs(15), {
            crate::migration::wait_for_convergence(&pool_for_loop, &state)
        })
        .await
        .expect("a diskless migration must converge on the first poll");
        result
            .expect("the agent-reported MemoryMigration phase must converge a diskless migration");
    }
}
