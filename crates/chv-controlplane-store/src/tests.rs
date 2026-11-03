use crate::*;
use chv_controlplane_types::domain::{Generation, NodeId, ResourceId};

// NOTE: These tests use an ephemeral Postgres instance via testcontainers.
// No manual setup is required.

use crate::test_util::{test_credential_encryption, TestDb};

#[tokio::test]
async fn test_bootstrap_token_validation() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = BootstrapTokenRepository::new(pool.clone());

    let hash = "a665a45920422f9d417e4867efdc4fb8a04a1f3fff1fa07e998e86f7f7a27ae3"; // sha256("123")
    sqlx::query("INSERT INTO bootstrap_tokens (token_hash, one_time_use) VALUES ($1, true)")
        .bind(hash)
        .execute(&pool)
        .await
        .unwrap();

    assert_eq!(
        repo.validate_and_consume("123").await.unwrap(),
        BootstrapTokenValidation::Valid
    );
    assert_eq!(
        repo.validate_and_consume("123").await.unwrap(),
        BootstrapTokenValidation::AlreadyUsed
    );
    assert_eq!(
        repo.validate_and_consume("999").await.unwrap(),
        BootstrapTokenValidation::Invalid
    );
}

#[tokio::test]
async fn test_expired_bootstrap_token() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = BootstrapTokenRepository::new(pool.clone());

    let hash = "a665a45920422f9d417e4867efdc4fb8a04a1f3fff1fa07e998e86f7f7a27ae3"; // sha256("123")
    sqlx::query("INSERT INTO bootstrap_tokens (token_hash, one_time_use, expires_at) VALUES ($1, true, strftime('%Y-%m-%dT%H:%M:%SZ', strftime('%s','now') - 3600, 'unixepoch'))")
        .bind(hash)
        .execute(&pool)
        .await
        .unwrap();

    assert_eq!(
        repo.validate_and_consume("123").await.unwrap(),
        BootstrapTokenValidation::Expired
    );
}

#[tokio::test]
async fn test_reusable_bootstrap_token() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = BootstrapTokenRepository::new(pool.clone());

    let hash = "a665a45920422f9d417e4867efdc4fb8a04a1f3fff1fa07e998e86f7f7a27ae3"; // sha256("123")
    sqlx::query("INSERT INTO bootstrap_tokens (token_hash, one_time_use) VALUES ($1, false)")
        .bind(hash)
        .execute(&pool)
        .await
        .unwrap();

    assert_eq!(
        repo.validate_and_consume("123").await.unwrap(),
        BootstrapTokenValidation::Valid
    );
    assert_eq!(
        repo.validate_and_consume("123").await.unwrap(),
        BootstrapTokenValidation::Valid
    );
}

#[tokio::test]
async fn test_bootstrap_result_repeatable_upsert() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = NodeRepository::new(pool);
    let node_id = NodeId::new("test-node-bootstrap").unwrap();

    // Ensure node exists
    repo.upsert_node(&NodeUpsertInput {
        node_id: node_id.clone(),
        hostname: "test-host".into(),
        display_name: "test-host".into(),
        certificate_serial: None,
        agent_version: None,
        control_plane_version: None,
        enrolled_unix_ms: 0,
        last_seen_unix_ms: 0,
    })
    .await
    .unwrap();

    let input = NodeBootstrapResultInput {
        node_id: node_id.clone(),
        operation_id: Some("op-1".into()),
        success: true,
        error_message: None,
        details: None,
        started_unix_ms: Some(1000),
        completed_unix_ms: 2000,
    };

    // First write
    repo.upsert_bootstrap_result(&input)
        .await
        .expect("First write failed");

    // Second write (updates existing row via ON CONFLICT)
    repo.upsert_bootstrap_result(&input).await.expect(
        "Second write failed - should have succeeded with ON CONFLICT and updated_at now()",
    );
}

#[tokio::test]
async fn test_telemetry_no_fabrication() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = ObservedStateRepository::new(pool);
    let vm_id = ResourceId::new("non-existent-vm").unwrap();

    let input = VmObservedStateInput {
        vm_id: vm_id.clone(),
        observed_generation: Generation::new(1),
        runtime_status: "running".into(),
        health_status: None,
        node_id: None,
        cloud_hypervisor_pid: None,
        api_socket_path: None,
        last_error: None,
        last_transition_unix_ms: None,
        observed_unix_ms: 1000,
    };

    let result = repo.upsert_vm(&input).await;

    // Should fail with NotFound, not create a skeleton row
    match result {
        Err(StoreError::NotFound { entity, id }) => {
            assert_eq!(entity, "vm");
            assert_eq!(id, vm_id.to_string());
        }
        other => panic!("Expected NotFound error, got {:?}", other),
    }
}

#[tokio::test]
async fn test_update_certificate_serial_missing_node() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = NodeRepository::new(pool);
    let node_id = NodeId::new("non-existent-node").unwrap();

    let result = repo.update_certificate_serial(&node_id, "serial-123").await;

    match result {
        Err(StoreError::NotFound { entity, id }) => {
            assert_eq!(entity, "node");
            assert_eq!(id, node_id.to_string());
        }
        other => panic!("Expected NotFound error for missing node, got {:?}", other),
    }
}

async fn seed_node_for_serial_test(repo: &NodeRepository, node_id: &NodeId, serial: Option<&str>) {
    repo.upsert_node(&NodeUpsertInput {
        node_id: node_id.clone(),
        hostname: "host".into(),
        display_name: node_id.to_string(),
        certificate_serial: serial.map(str::to_string),
        agent_version: None,
        control_plane_version: None,
        enrolled_unix_ms: 1000,
        last_seen_unix_ms: 1000,
    })
    .await
    .expect("seed node");
}

#[tokio::test]
async fn test_get_certificate_serial_distinguishes_missing_node_from_null_serial() {
    let test_db = TestDb::new().await;
    let repo = NodeRepository::new(test_db.pool.clone());
    let node_id = NodeId::new("node-serial-null").unwrap();

    let missing = repo.get_certificate_serial(&node_id).await;
    assert!(
        matches!(missing, Err(StoreError::NotFound { .. })),
        "missing node must be NotFound, got {missing:?}"
    );

    seed_node_for_serial_test(&repo, &node_id, None).await;
    assert_eq!(
        repo.get_certificate_serial(&node_id).await.unwrap(),
        None,
        "an existing legacy row with NULL serial is Ok(None)"
    );

    seed_node_for_serial_test(&repo, &node_id, Some("serial-a")).await;
    assert_eq!(
        repo.get_certificate_serial(&node_id)
            .await
            .unwrap()
            .as_deref(),
        Some("serial-a")
    );
}

#[tokio::test]
async fn test_rotate_certificate_serial_is_compare_and_swap() {
    let test_db = TestDb::new().await;
    let repo = NodeRepository::new(test_db.pool.clone());
    let node_id = NodeId::new("node-serial-cas").unwrap();
    seed_node_for_serial_test(&repo, &node_id, Some("serial-a")).await;

    assert!(
        !repo
            .rotate_certificate_serial(&node_id, "stale", "serial-b")
            .await
            .unwrap(),
        "stale expected serial must not update"
    );
    assert_eq!(
        repo.get_certificate_serial(&node_id)
            .await
            .unwrap()
            .as_deref(),
        Some("serial-a")
    );

    assert!(
        repo.rotate_certificate_serial(&node_id, "serial-a", "serial-b")
            .await
            .unwrap(),
        "matching expected serial must update"
    );
    assert_eq!(
        repo.get_certificate_serial(&node_id)
            .await
            .unwrap()
            .as_deref(),
        Some("serial-b")
    );
}

#[tokio::test]
async fn test_revoke_certificate_is_idempotent_and_preserves_first_record() {
    let test_db = TestDb::new().await;
    let repo = CertificateRevocationRepository::new(test_db.pool.clone());

    let first = repo
        .revoke(&RevokeCertificateInput {
            certificate_serial: "serial-revoked",
            node_id: "node-a",
            reason: "host compromise",
            revoked_by: Some("admin"),
            revoked_at: Some("2026-01-01T00:00:00Z"),
            expires_at: Some("2027-01-01T00:00:00Z"),
        })
        .await
        .expect("first revocation");
    assert!(!first.already_revoked);
    assert_eq!(first.revocation.certificate_serial, "serial-revoked");
    assert_eq!(first.revocation.node_id, "node-a");
    assert_eq!(first.revocation.reason, "host compromise");
    assert_eq!(first.revocation.revoked_at, "2026-01-01T00:00:00Z");

    let second = repo
        .revoke(&RevokeCertificateInput {
            certificate_serial: "serial-revoked",
            node_id: "node-a",
            reason: "a later, different reason",
            revoked_by: Some("another-admin"),
            revoked_at: Some("2026-02-02T00:00:00Z"),
            expires_at: None,
        })
        .await
        .expect("repeat revocation is idempotent");
    assert!(second.already_revoked);
    assert_eq!(
        second.revocation.reason, "host compromise",
        "first revocation stays authoritative"
    );
    assert_eq!(second.revocation.revoked_by.as_deref(), Some("admin"));
    assert_eq!(second.revocation.revoked_at, "2026-01-01T00:00:00Z");

    assert_eq!(repo.list_all().await.unwrap().len(), 1);
    assert!(repo.is_revoked("serial-revoked").await.unwrap());
    assert!(!repo.is_revoked("serial-other").await.unwrap());
    assert_eq!(repo.list_for_node("node-a").await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_revoke_certificate_rejects_serial_attributed_to_another_node() {
    let test_db = TestDb::new().await;
    let repo = CertificateRevocationRepository::new(test_db.pool.clone());

    repo.revoke(&RevokeCertificateInput {
        certificate_serial: "serial-shared",
        node_id: "node-a",
        reason: "compromise",
        revoked_by: None,
        revoked_at: None,
        expires_at: None,
    })
    .await
    .unwrap();

    let result = repo
        .revoke(&RevokeCertificateInput {
            certificate_serial: "serial-shared",
            node_id: "node-b",
            reason: "typo",
            revoked_by: None,
            revoked_at: None,
            expires_at: None,
        })
        .await;
    assert!(
        matches!(result, Err(StoreError::Conflict { entity, .. }) if entity == "node_certificate_revocation"),
        "a serial cannot be silently re-attributed to another node: {result:?}"
    );
}

#[tokio::test]
async fn test_rotate_certificate_serial_refuses_revoked_serial() {
    let test_db = TestDb::new().await;
    let node_repo = NodeRepository::new(test_db.pool.clone());
    let revocation_repo = CertificateRevocationRepository::new(test_db.pool.clone());
    let node_id = NodeId::new("node-serial-revoked").unwrap();
    seed_node_for_serial_test(&node_repo, &node_id, Some("serial-a")).await;

    revocation_repo
        .revoke(&RevokeCertificateInput {
            certificate_serial: "serial-a",
            node_id: node_id.as_str(),
            reason: "stolen",
            revoked_by: Some("admin"),
            revoked_at: None,
            expires_at: None,
        })
        .await
        .unwrap();

    assert!(
        !node_repo
            .rotate_certificate_serial(&node_id, "serial-a", "serial-b")
            .await
            .unwrap(),
        "a revoked serial must never be exchangeable for fresh key material"
    );
    assert_eq!(
        node_repo
            .get_certificate_serial(&node_id)
            .await
            .unwrap()
            .as_deref(),
        Some("serial-a"),
        "the revoked serial remains recorded so operators can see what was burned"
    );
}

#[tokio::test]
async fn test_telemetry_missing_parent_node() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = ObservedStateRepository::new(pool.clone());
    let vm_id = ResourceId::new("vm-missing-node").unwrap();
    let node_id = NodeId::new("non-existent-parent-node").unwrap();

    // Ensure VM exists (or try to create it, but wait, if VM has a node_id FK we can test that)
    // Actually, vms(node_id) REFERENCES nodes(node_id).
    // But vm_observed_state(node_id) ALSO REFERENCES nodes(node_id).

    // First, create the VM record properly (without a node)
    let _node_repo = NodeRepository::new(pool.clone());
    sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(vm_id.as_str())
        .bind("test-vm")
        .execute(&pool)
        .await
        .unwrap();

    let input = VmObservedStateInput {
        vm_id: vm_id.clone(),
        observed_generation: Generation::new(1),
        runtime_status: "running".into(),
        health_status: None,
        node_id: Some(node_id.clone()), // NON-EXISTENT
        cloud_hypervisor_pid: None,
        api_socket_path: None,
        last_error: None,
        last_transition_unix_ms: None,
        observed_unix_ms: 1000,
    };

    let result = repo.upsert_vm(&input).await;

    match result {
        Err(StoreError::NotFound { entity, id }) => {
            assert_eq!(entity, "node");
            assert_eq!(id, node_id.to_string());
        }
        other => panic!("Expected NotFound(node) error, got {:?}", other),
    }
}

#[tokio::test]
async fn test_telemetry_missing_attached_vm() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = ObservedStateRepository::new(pool.clone());
    let volume_id = ResourceId::new("vol-missing-vm").unwrap();
    let vm_id = ResourceId::new("vm-not-attached").unwrap();

    // Ensure Volume exists
    sqlx::query("INSERT INTO volumes (volume_id, display_name, capacity_bytes) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
        .bind(volume_id.as_str())
        .bind("test-vol")
        .bind(1024 * 1024 * 1024i64)
        .execute(&pool)
        .await
        .unwrap();

    let input = VolumeObservedStateInput {
        volume_id: volume_id.clone(),
        observed_generation: Generation::new(1),
        runtime_status: "available".into(),
        health_status: None,
        attached_vm_id: Some(vm_id.clone()), // NON-EXISTENT
        device_path: None,
        export_path: None,
        last_transition_unix_ms: None,
        observed_unix_ms: 1000,
    };

    let result = repo.upsert_volume(&input).await;

    match result {
        Err(StoreError::NotFound { entity, id }) => {
            assert_eq!(entity, "vm");
            assert_eq!(id, vm_id.to_string());
        }
        other => panic!(
            "Expected NotFound(vm) error for attached-vm, got {:?}",
            other
        ),
    }
}

#[tokio::test]
async fn test_ack_node_generation_preserves_observed_state() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = ObservedStateRepository::new(pool.clone());
    let node_id = NodeId::new("test-node-ack-preserve").unwrap();

    sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES ($1, 'host', 'host')")
        .bind(node_id.as_str())
        .execute(&pool)
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO node_observed_state (node_id, observed_generation, observed_state, observed_at, updated_at) VALUES ($1, 1, 'Discovered', strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'))",
    )
    .bind(node_id.as_str())
    .execute(&pool)
    .await
    .unwrap();

    repo.acknowledge_node_generation(&node_id, Generation::new(5), 2000)
        .await
        .expect("ack should succeed when observed row exists");

    let row = sqlx::query(
        "SELECT observed_generation, observed_state FROM node_observed_state WHERE node_id = $1",
    )
    .bind(node_id.as_str())
    .fetch_one(&pool)
    .await
    .unwrap();

    let generation: i64 = sqlx::Row::get(&row, "observed_generation");
    let state: String = sqlx::Row::get(&row, "observed_state");
    assert_eq!(generation, 5);
    assert_eq!(state, "Discovered");
}

#[tokio::test]
async fn test_ack_node_generation_rejects_missing_observed_row() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = ObservedStateRepository::new(pool.clone());
    let node_id = NodeId::new("test-node-ack-missing").unwrap();

    sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES ($1, 'host', 'host')")
        .bind(node_id.as_str())
        .execute(&pool)
        .await
        .unwrap();

    // Seed desired state but no observed state
    sqlx::query(
        "INSERT INTO node_desired_state (node_id, desired_generation, desired_state, requested_at, updated_at, scheduling_paused) VALUES ($1, 1, 'TenantReady', strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'), false)",
    )
    .bind(node_id.as_str())
    .execute(&pool)
    .await
    .unwrap();

    let result = repo
        .acknowledge_node_generation(&node_id, Generation::new(5), 2000)
        .await;

    match result {
        Err(StoreError::NotFound { entity, id }) => {
            assert_eq!(entity, "node_observed_state");
            assert_eq!(id, node_id.to_string());
        }
        other => panic!(
            "Expected NotFound for missing observed row, got {:?}",
            other
        ),
    }

    // Verify no observed row was fabricated from desired state
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM node_observed_state WHERE node_id = $1")
            .bind(node_id.as_str())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn test_upsert_vm_rejects_stale_generation() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = DesiredStateRepository::new(pool.clone());

    let input = VmDesiredStateInput {
        vm_id: ResourceId::new("vm-gen-test").unwrap(),
        node_id: None,
        display_name: "gen-test".to_string(),
        tenant_id: None,
        placement_policy: None,
        desired_generation: Generation::new(5),
        desired_status: Some("Running".to_string()),
        requested_by: Some("test".to_string()),
        updated_by: Some("test".to_string()),
        target_node_id: None,
        cpu_count: Some(2),
        memory_bytes: Some(1024),
        image_ref: Some("img".to_string()),
        boot_mode: None,
        desired_power_state: Some("on".to_string()),
        vsock_enabled: None,
        requested_unix_ms: 1000,
    };

    repo.upsert_vm(&input).await.unwrap();

    let mut stale_input = input.clone();
    stale_input.desired_generation = Generation::new(3);
    stale_input.requested_unix_ms = 2000;

    match repo.upsert_vm(&stale_input).await {
        Err(StoreError::StaleGeneration {
            entity,
            id,
            incoming,
        }) => {
            assert_eq!(entity, "vm");
            assert_eq!(id, "vm-gen-test");
            assert_eq!(incoming, 3);
        }
        other => panic!("Expected StaleGeneration, got {:?}", other),
    }
}

#[tokio::test]
async fn test_upsert_vm_accepts_newer_generation() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = DesiredStateRepository::new(pool.clone());

    let input = VmDesiredStateInput {
        vm_id: ResourceId::new("vm-gen-newer").unwrap(),
        node_id: None,
        display_name: "gen-newer".to_string(),
        tenant_id: None,
        placement_policy: None,
        desired_generation: Generation::new(3),
        desired_status: Some("Running".to_string()),
        requested_by: Some("test".to_string()),
        updated_by: Some("test".to_string()),
        target_node_id: None,
        cpu_count: Some(2),
        memory_bytes: Some(1024),
        image_ref: Some("img".to_string()),
        boot_mode: None,
        desired_power_state: Some("on".to_string()),
        vsock_enabled: None,
        requested_unix_ms: 1000,
    };

    repo.upsert_vm(&input).await.unwrap();

    let mut newer_input = input.clone();
    newer_input.desired_generation = Generation::new(5);
    newer_input.requested_unix_ms = 2000;

    repo.upsert_vm(&newer_input).await.unwrap();

    let gen: i64 = sqlx::query_scalar(
        "SELECT desired_generation FROM vm_desired_state WHERE vm_id = 'vm-gen-newer'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(gen, 5);
}

#[tokio::test]
async fn test_upsert_vm_accepts_same_generation_idempotent() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = DesiredStateRepository::new(pool.clone());

    let input = VmDesiredStateInput {
        vm_id: ResourceId::new("vm-gen-idem").unwrap(),
        node_id: None,
        display_name: "gen-idem".to_string(),
        tenant_id: None,
        placement_policy: None,
        desired_generation: Generation::new(5),
        desired_status: Some("Running".to_string()),
        requested_by: Some("test".to_string()),
        updated_by: Some("test".to_string()),
        target_node_id: None,
        cpu_count: Some(2),
        memory_bytes: Some(1024),
        image_ref: Some("img".to_string()),
        boot_mode: None,
        desired_power_state: Some("on".to_string()),
        vsock_enabled: None,
        requested_unix_ms: 1000,
    };

    repo.upsert_vm(&input).await.unwrap();
    repo.upsert_vm(&input).await.unwrap();
}

// ── #384: transactional clone target materialization ──────────────────────
//
// `materialize_clone_target` is the clone path's store core: ONE
// `BEGIN IMMEDIATE` transaction that reads the source row (under the
// write lock, so a racing resize cannot split the read from the target
// write), strictly inserts the target's physical row (ON CONFLICT DO
// NOTHING — a concurrent same-target materialization fails closed with
// StoreError::Conflict instead of last-writer-wins), and writes the
// generation-guarded VDS intent row in the same transaction.

/// Seed a source `volumes` row the way the fragment reconcile would.
async fn seed_clone_source_volume(pool: &StorePool, volume_id: &str, capacity_bytes: i64) {
    sqlx::query(
        "INSERT OR IGNORE INTO nodes (node_id, hostname, display_name) \
         VALUES ('node-clone-a', 'host', 'host')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO volumes (volume_id, node_id, display_name, capacity_bytes, volume_kind, storage_class, owner_id, updated_at) \
         VALUES ($1, 'node-clone-a', $2, $3, 'disk', 'local', 'user-a', '2026-01-01T00:00:00Z')",
    )
    .bind(volume_id)
    .bind(format!("{volume_id}-name"))
    .bind(capacity_bytes)
    .execute(pool)
    .await
    .unwrap();
}

fn clone_target_spec(target: &str) -> CloneTargetSpec {
    CloneTargetSpec {
        target_volume_id: ResourceId::new(target).unwrap(),
        placement_node_id: Some(NodeId::new("node-clone-a").unwrap()),
        display_name: target.to_string(),
        desired_generation: Generation::new(7),
        requested_by: Some("test-user".to_string()),
        requested_unix_ms: 1000,
    }
}

#[tokio::test]
async fn test_materialize_clone_target_inserts_rows_in_one_tx() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = DesiredStateRepository::new(pool.clone());
    seed_clone_source_volume(&pool, "vol-ct-src", 10_737_418_240).await;

    let outcome = repo
        .materialize_clone_target(
            &ResourceId::new("vol-ct-src").unwrap(),
            &clone_target_spec("vol-ct-dst"),
            false,
        )
        .await
        .unwrap();

    assert!(outcome.created, "fresh target must be created");
    assert_eq!(outcome.source.capacity_bytes, 10_737_418_240);

    // Physical row carries the source's shape, including the inherited
    // owner (#381: an ownerless volumes row is admin-only in the BFF).
    let row = sqlx::query(
        "SELECT node_id, capacity_bytes, owner_id FROM volumes WHERE volume_id = 'vol-ct-dst'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let node: String = sqlx::Row::get(&row, "node_id");
    let capacity: i64 = sqlx::Row::get(&row, "capacity_bytes");
    let owner: Option<String> = sqlx::Row::get(&row, "owner_id");
    assert_eq!(node, "node-clone-a");
    assert_eq!(capacity, 10_737_418_240);
    assert_eq!(owner.as_deref(), Some("user-a"));

    // The VDS intent row landed in the same transaction, recording the
    // clone source and the generation.
    let (gen, clone_source): (i64, Option<String>) = sqlx::query_as(
        "SELECT desired_generation, clone_source_volume_id FROM volume_desired_state WHERE volume_id = 'vol-ct-dst'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(gen, 7);
    assert_eq!(clone_source.as_deref(), Some("vol-ct-src"));
}

#[tokio::test]
async fn test_materialize_clone_target_fails_closed_on_existing_target() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = DesiredStateRepository::new(pool.clone());
    seed_clone_source_volume(&pool, "vol-ct-src", 1024).await;
    // A foreign row already sits on the target id.
    seed_clone_source_volume(&pool, "vol-ct-dst", 4096).await;

    match repo
        .materialize_clone_target(
            &ResourceId::new("vol-ct-src").unwrap(),
            &clone_target_spec("vol-ct-dst"),
            false,
        )
        .await
    {
        Err(StoreError::Conflict { entity, id, .. }) => {
            assert_eq!(entity, "volume");
            assert_eq!(id, "vol-ct-dst");
        }
        other => panic!("expected Conflict, got {:?}", other.map(|_| ())),
    }

    // Fail closed: the existing row keeps its own shape.
    let capacity: i64 =
        sqlx::query_scalar("SELECT capacity_bytes FROM volumes WHERE volume_id = 'vol-ct-dst'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(capacity, 4096, "conflict must not overwrite the target");
}

#[tokio::test]
async fn test_materialize_clone_target_replay_is_idempotent() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = DesiredStateRepository::new(pool.clone());
    seed_clone_source_volume(&pool, "vol-ct-src", 2048).await;

    let first = repo
        .materialize_clone_target(
            &ResourceId::new("vol-ct-src").unwrap(),
            &clone_target_spec("vol-ct-dst"),
            false,
        )
        .await
        .unwrap();
    assert!(first.created);

    let (updated_at, vds_requested_at): (String, String) = sqlx::query_as(
        "SELECT v.updated_at, vds.requested_at FROM volumes v \
         JOIN volume_desired_state vds ON v.volume_id = vds.volume_id \
         WHERE v.volume_id = 'vol-ct-dst'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    // The same operation re-running its intent persist (a replayed
    // meta.operation_id) must be an idempotent success, not a Conflict
    // against its own earlier materialization.
    let replay = repo
        .materialize_clone_target(
            &ResourceId::new("vol-ct-src").unwrap(),
            &clone_target_spec("vol-ct-dst"),
            true,
        )
        .await
        .unwrap();
    assert!(!replay.created, "replay must not report a fresh creation");

    // Nothing was written the second time.
    let (updated_at_2, vds_requested_at_2): (String, String) = sqlx::query_as(
        "SELECT v.updated_at, vds.requested_at FROM volumes v \
         JOIN volume_desired_state vds ON v.volume_id = vds.volume_id \
         WHERE v.volume_id = 'vol-ct-dst'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(updated_at, updated_at_2, "replay must not rewrite volumes");
    assert_eq!(
        vds_requested_at, vds_requested_at_2,
        "replay must not rewrite the VDS intent row"
    );
}

#[tokio::test]
async fn test_materialize_clone_target_replay_with_reshaped_target_fails_closed() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = DesiredStateRepository::new(pool.clone());
    seed_clone_source_volume(&pool, "vol-ct-src", 2048).await;

    repo.materialize_clone_target(
        &ResourceId::new("vol-ct-src").unwrap(),
        &clone_target_spec("vol-ct-dst"),
        false,
    )
    .await
    .unwrap();

    // The target row no longer matches this operation's shape (e.g. it
    // was resized, or a foreign request owns the id): the replay must
    // fail closed rather than silently claim idempotent success.
    sqlx::query("UPDATE volumes SET capacity_bytes = 999 WHERE volume_id = 'vol-ct-dst'")
        .execute(&pool)
        .await
        .unwrap();

    match repo
        .materialize_clone_target(
            &ResourceId::new("vol-ct-src").unwrap(),
            &clone_target_spec("vol-ct-dst"),
            true,
        )
        .await
    {
        Err(StoreError::Conflict { .. }) => {}
        other => panic!("expected Conflict, got {:?}", other.map(|_| ())),
    }
}

/// Build a temp-file SQLite pool with the same pragma profile as prod
/// (WAL, busy_timeout) — the same shape as the BFF quota-race suite.
/// The in-memory `TestDb` pool cannot express the cross-connection write
/// locking these tests pin (an in-memory database has no WAL mode, and a
/// second connection's access during an open write transaction blocks
/// without a busy handler). The returned `TempDir` owns the database
/// files — bind it for the test's duration (`_dir`); it is returned
/// FIRST so it drops LAST, after the pool's connections close, and
/// cleans up on scope exit — no pid-suffixed temp dirs accumulate
/// under /tmp.
async fn clone_race_test_pool() -> (tempfile::TempDir, StorePool) {
    use std::str::FromStr as _;
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("test.db").display());
    let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
        .unwrap()
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .busy_timeout(std::time::Duration::from_secs(5));
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(8)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect_with(opts)
        .await
        .unwrap();
    run_migrations(&pool, None).await.unwrap();
    (dir, pool)
}

/// The #384 race, pinned at the store tier: two concurrent
/// materializations of the SAME target id — exactly one wins, the loser
/// fails closed with Conflict (red/green: with the old DO UPDATE upsert
/// both would succeed and the last writer would silently reshape the
/// row).
#[tokio::test]
async fn test_concurrent_materialize_clone_target_exactly_one_wins() {
    let (_dir, pool) = clone_race_test_pool().await;
    let repo = DesiredStateRepository::new(pool.clone());
    seed_clone_source_volume(&pool, "vol-ct-src", 1024).await;

    let a = {
        let repo = repo.clone();
        let source = ResourceId::new("vol-ct-src").unwrap();
        let spec = clone_target_spec("vol-ct-dst");
        tokio::spawn(async move { repo.materialize_clone_target(&source, &spec, false).await })
    };
    let b = {
        let repo = repo.clone();
        let source = ResourceId::new("vol-ct-src").unwrap();
        let spec = clone_target_spec("vol-ct-dst");
        tokio::spawn(async move { repo.materialize_clone_target(&source, &spec, false).await })
    };

    let results = vec![a.await.unwrap(), b.await.unwrap()];
    let winners = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(winners, 1, "exactly one materialization must win");
    for result in &results {
        match result {
            Ok(outcome) => assert!(outcome.created),
            Err(StoreError::Conflict { id, .. }) => assert_eq!(id, "vol-ct-dst"),
            other => panic!(
                "race loser must be a Conflict, got {:?}",
                other.as_ref().map(|_| ())
            ),
        }
    }

    // Exactly one target row exists, carrying the winner's (only) shape.
    let (count, capacity): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), MAX(capacity_bytes) FROM volumes WHERE volume_id = 'vol-ct-dst'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
    assert_eq!(capacity, 1024);
}

/// Source-read freshness (#384 premise (c)): the source row is read
/// INSIDE the `BEGIN IMMEDIATE` transaction, under the same RESERVED
/// lock that guards the target insert. A resize (the resize executor's
/// direct `UPDATE volumes SET capacity_bytes`) that commits while the
/// clone's transaction is starting must be visible to the in-tx read —
/// the target can no longer be shaped from a stale capacity.
#[tokio::test]
async fn test_materialize_clone_target_reads_source_under_write_lock() {
    let (_dir, pool) = clone_race_test_pool().await;
    let repo = DesiredStateRepository::new(pool.clone());
    seed_clone_source_volume(&pool, "vol-ct-src", 1024).await;

    // Hold a write transaction that resizes the source, uncommitted.
    let mut resize_tx = pool.begin_with("BEGIN IMMEDIATE;").await.unwrap();
    sqlx::query("UPDATE volumes SET capacity_bytes = 8192 WHERE volume_id = 'vol-ct-src'")
        .execute(&mut *resize_tx)
        .await
        .unwrap();

    // The clone's BEGIN IMMEDIATE cannot start while the resize holds
    // the RESERVED lock; it must wait and then read the committed
    // (post-resize) source. With the old read-outside-the-transaction
    // shape, this read would return the pre-resize snapshot (1024) and
    // the target would silently carry the stale size. The capacity
    // assert below is the actual discriminator — the clone's
    // not-finished outcome holds either way (the old shape's write
    // still blocked on the RESERVED lock); only the capacity value
    // distinguishes stale-snapshot from locked-fresh read.
    let clone = {
        let repo = repo.clone();
        let source = ResourceId::new("vol-ct-src").unwrap();
        let spec = clone_target_spec("vol-ct-dst");
        tokio::spawn(async move { repo.materialize_clone_target(&source, &spec, false).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !clone.is_finished(),
        "the clone must block on the write lock, not read a stale snapshot"
    );

    resize_tx.commit().await.unwrap();
    clone.await.unwrap().unwrap();

    let capacity: i64 =
        sqlx::query_scalar("SELECT capacity_bytes FROM volumes WHERE volume_id = 'vol-ct-dst'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        capacity, 8192,
        "the target must be shaped from the post-resize source capacity"
    );
}

#[tokio::test]
async fn test_set_vm_resources_stale_vs_not_found() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = DesiredStateRepository::new(pool.clone());

    // First, create a VM with generation 5
    let vm_input = VmDesiredStateInput {
        vm_id: ResourceId::new("vm-res-test").unwrap(),
        node_id: None,
        display_name: "res-test".to_string(),
        tenant_id: None,
        placement_policy: None,
        desired_generation: Generation::new(5),
        desired_status: Some("Running".to_string()),
        requested_by: Some("test".to_string()),
        updated_by: Some("test".to_string()),
        target_node_id: None,
        cpu_count: Some(2),
        memory_bytes: Some(1024),
        image_ref: Some("img".to_string()),
        boot_mode: None,
        desired_power_state: Some("on".to_string()),
        vsock_enabled: None,
        requested_unix_ms: 1000,
    };
    repo.upsert_vm(&vm_input).await.unwrap();

    // Stale generation → StaleGeneration error
    let stale_patch = VmResourcesPatchInput {
        vm_id: ResourceId::new("vm-res-test").unwrap(),
        cpu_count: Some(4),
        memory_bytes: None,
        desired_generation: Generation::new(3),
        requested_by: Some("test".to_string()),
        target_node_id: None,
        requested_unix_ms: 2000,
    };
    match repo.set_vm_resources(&stale_patch).await {
        Err(StoreError::StaleGeneration { entity, .. }) => {
            assert_eq!(entity, "vm");
        }
        other => panic!("Expected StaleGeneration, got {:?}", other),
    }

    // Non-existent VM → NotFound error
    let missing_patch = VmResourcesPatchInput {
        vm_id: ResourceId::new("vm-nonexistent").unwrap(),
        cpu_count: Some(4),
        memory_bytes: None,
        desired_generation: Generation::new(10),
        requested_by: Some("test".to_string()),
        target_node_id: None,
        requested_unix_ms: 2000,
    };
    match repo.set_vm_resources(&missing_patch).await {
        Err(StoreError::NotFound { entity, .. }) => {
            assert_eq!(entity, "vm");
        }
        other => panic!("Expected NotFound, got {:?}", other),
    }
}

/// Fail-closed regression for C4: a row with an undecryptable `enc:`-prefixed
/// credential MUST surface as `None` (with an error log), not as the ciphertext
/// literal. The previous fail-soft behavior wrote `enc:hex...` back into
/// `s3_access_key`/`s3_secret_key`, which the worker passed to the S3 client
/// as a credential — manifesting as opaque AWS auth errors.
#[tokio::test]
async fn decrypt_schedule_row_nulls_undecryptable_credential() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();

    // Insert a schedule row with deliberately-bad ciphertext directly. Bypass
    // the repository's encrypt path so we can simulate a row written by a
    // process running under a different (now-lost) key.
    let bad_ciphertext = "enc:deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    sqlx::query(
        "INSERT INTO backup_schedules ( \
             schedule_id, vm_id, name, cron_expression, retention_count, \
             enabled, s3_access_key, s3_secret_key \
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("sched-c4-regression")
    .bind("vm-x")
    .bind("c4-test")
    .bind("0 0 * * *")
    .bind(1_i64)
    .bind(true)
    .bind(bad_ciphertext)
    .bind(bad_ciphertext)
    .execute(&pool)
    .await
    .expect("insert schedule row");

    // BackupRepository::new picks up the *current* CHV_ENCRYPTION_KEY env.
    // Whatever the test process has, the bad ciphertext will not authenticate
    // under it (or KeyUnavailable / Malformed), so decrypt MUST fail.
    let repo = BackupRepository::new(pool);
    let row = repo
        .get_schedule("sched-c4-regression")
        .await
        .expect("query succeeds")
        .expect("row exists");

    assert!(
        row.s3_access_key.is_none(),
        "fail-closed: undecryptable s3_access_key MUST be None, got {:?}",
        row.s3_access_key
    );
    assert!(
        row.s3_secret_key.is_none(),
        "fail-closed: undecryptable s3_secret_key MUST be None, got {:?}",
        row.s3_secret_key
    );
    // Critically, neither field may equal the ciphertext literal.
    assert_ne!(row.s3_access_key.as_deref(), Some(bad_ciphertext));
    assert_ne!(row.s3_secret_key.as_deref(), Some(bad_ciphertext));
}

/// #655: with no encryption key, creating a schedule that carries S3
/// credentials must fail closed — no plaintext row may be written.
#[tokio::test]
async fn backups_create_schedule_without_key_fails_closed_and_writes_no_row() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = BackupRepository::with_crypto(pool.clone(), CredentialEncryption::without_key());

    let input = BackupScheduleCreateInput {
        vm_id: "vm-fail-closed".to_string(),
        volume_id: None,
        name: "no-key-schedule".to_string(),
        cron_expression: "0 0 * * *".to_string(),
        retention_count: 1,
        retention_days: 7,
        destination: None,
        enabled: true,
        s3_access_key: Some("AKIA-plaintext-access".to_string()),
        s3_secret_key: Some("plaintext-secret".to_string()),
    };

    let err = repo.create_schedule(&input).await.unwrap_err();
    match err {
        StoreError::InvalidConfiguration { reason } => assert!(
            reason.contains("fail-closed"),
            "expected fail-closed reason, got {reason}"
        ),
        other => panic!("expected InvalidConfiguration, got {other:?}"),
    }

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM backup_schedules")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "no schedule row may be written");
}

/// #655: an update carrying new credentials under a no-key cipher must return
/// an error and leave the existing row (including credentials) untouched.
#[tokio::test]
async fn backups_update_schedule_without_key_leaves_row_unchanged() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let keyed = BackupRepository::with_crypto(pool.clone(), test_credential_encryption());
    let create = BackupScheduleCreateInput {
        vm_id: "vm-fail-closed".to_string(),
        volume_id: None,
        name: "original-name".to_string(),
        cron_expression: "0 0 * * *".to_string(),
        retention_count: 1,
        retention_days: 7,
        destination: None,
        enabled: true,
        s3_access_key: Some("original-access".to_string()),
        s3_secret_key: Some("original-secret".to_string()),
    };
    let schedule_id = keyed.create_schedule(&create).await.unwrap();

    let no_key = BackupRepository::with_crypto(pool.clone(), CredentialEncryption::without_key());
    let update = BackupScheduleUpdateInput {
        schedule_id: schedule_id.clone(),
        vm_id: "vm-fail-closed".to_string(),
        volume_id: None,
        name: "should-not-persist".to_string(),
        cron_expression: "0 1 * * *".to_string(),
        retention_count: 1,
        retention_days: 7,
        destination: None,
        enabled: true,
        s3_access_key: Some("replacement-access".to_string()),
        s3_secret_key: Some("replacement-secret".to_string()),
    };
    let err = no_key.update_schedule(&update).await.unwrap_err();
    match err {
        StoreError::InvalidConfiguration { reason } => assert!(
            reason.contains("fail-closed"),
            "expected fail-closed reason, got {reason}"
        ),
        other => panic!("expected InvalidConfiguration, got {other:?}"),
    }

    let row = keyed
        .get_schedule(&schedule_id)
        .await
        .unwrap()
        .expect("schedule row still exists");
    assert_eq!(row.name, "original-name");
    assert_eq!(row.cron_expression, "0 0 * * *");
    assert_eq!(row.s3_access_key.as_deref(), Some("original-access"));
    assert_eq!(row.s3_secret_key.as_deref(), Some("original-secret"));
}

// --- ADR-021 fabric transport IP allocation (review finding M2) ---

/// Helper: register a node row (vtep_registry has a foreign key on
/// nodes.node_id) and return its id.
async fn seed_fabric_node_row(pool: &StorePool, node_id: &str) {
    sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES (?, ?, ?)")
        .bind(node_id)
        .bind(format!("host-{node_id}"))
        .bind(format!("host-{node_id}"))
        .execute(pool)
        .await
        .expect("insert node row");
}

/// Concurrent fabric identity registrations must not hand out the same
/// 100.100.0.0/16 transport IP twice. The unique index from migration
/// 0054 is the hard guarantee; the bounded retry in
/// `register_fabric_identity` turns a lost race into a successful
/// re-allocation instead of a spurious failure (review finding M2).
#[tokio::test]
async fn concurrent_fabric_identity_registrations_get_distinct_fabric_ips() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = VtepRepository::new(pool.clone());

    const CONCURRENT_REGISTRATIONS: usize = 8;
    for i in 0..CONCURRENT_REGISTRATIONS {
        seed_fabric_node_row(&pool, &format!("node-m2-{i}")).await;
    }

    let mut handles = Vec::new();
    for i in 0..CONCURRENT_REGISTRATIONS {
        let repo = repo.clone();
        let node_id = format!("node-m2-{i}");
        handles.push(tokio::spawn(async move {
            repo.register_fabric_identity(&node_id, &format!("pub-m2-{i}"), 1500, None)
                .await
        }));
    }

    for (i, handle) in handles.into_iter().enumerate() {
        handle
            .await
            .expect("registration task must not panic")
            .unwrap_or_else(|e| panic!("registration {i} must succeed: {e}"));
    }

    let mut ips = Vec::new();
    for i in 0..CONCURRENT_REGISTRATIONS {
        let entry = repo
            .get_vtep(&format!("node-m2-{i}"))
            .await
            .expect("get_vtep must succeed");
        ips.push(entry.fabric_ip.expect("fabric IP must be allocated"));
    }

    // Distinctness is the invariant under test.
    let mut sorted = ips.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        CONCURRENT_REGISTRATIONS,
        "concurrent registrations must receive distinct fabric IPs, got {ips:?}"
    );

    // Every address must live in the 100.100.0.0/16 host range
    // (100.100.0.1 ..= 100.100.255.254, ADR-021 §6).
    let network_base: u32 = (100 << 24) | (100 << 16);
    for ip in &ips {
        let addr: std::net::Ipv4Addr = ip.parse().expect("fabric IP must parse as IPv4");
        assert!(
            (network_base + 1..=network_base + 65_534).contains(&u32::from(addr)),
            "fabric IP {ip} outside 100.100.0.0/16 host range"
        );
    }
}

/// The migration 0054 unique index itself must reject a duplicate
/// fabric_ip, independent of the allocation logic (review finding M2).
#[tokio::test]
async fn fabric_ip_unique_index_rejects_duplicate_rows() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    seed_fabric_node_row(&pool, "node-m2-idx-a").await;
    seed_fabric_node_row(&pool, "node-m2-idx-b").await;

    sqlx::query(
        r#"INSERT INTO vtep_registry (node_id, vtep_ip, vtep_port, fabric_ip, updated_at)
           VALUES ('node-m2-idx-a', '', 4789, '100.100.0.1',
                   strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))"#,
    )
    .execute(&pool)
    .await
    .expect("first row with fabric_ip must insert");

    let err = sqlx::query(
        r#"INSERT INTO vtep_registry (node_id, vtep_ip, vtep_port, fabric_ip, updated_at)
           VALUES ('node-m2-idx-b', '', 4789, '100.100.0.1',
                   strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))"#,
    )
    .execute(&pool)
    .await
    .expect_err("duplicate fabric_ip must violate the unique index");

    assert!(
        err.to_string().contains("UNIQUE constraint failed"),
        "expected a UNIQUE constraint violation, got: {err}"
    );
}

// --- ADR-021 underlay endpoint policy (round-2 review finding) ---

/// The peer-derived `underlay_endpoint` follows first-registration-wins:
/// a second registration carrying a different endpoint must NOT
/// overwrite the stored one (a transient LB/proxy/VPN reconnection must
/// not silently replace a previously-good endpoint), while the public
/// key and underlay MTU keep their normal upsert semantics. A NULL
/// endpoint is still populated by the first registration that carries
/// one.
#[tokio::test]
async fn second_registration_does_not_overwrite_stored_underlay_endpoint() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = VtepRepository::new(pool.clone());
    seed_fabric_node_row(&pool, "node-ep-pin").await;

    // First registration pins the endpoint.
    repo.register_fabric_identity("node-ep-pin", "pub-ep-1", 1500, Some("198.51.100.10:65001"))
        .await
        .expect("first registration must succeed");

    // A re-report through a different observed peer address (e.g. an
    // LB/proxy reconnection) must not rotate the pinned endpoint.
    repo.register_fabric_identity("node-ep-pin", "pub-ep-2", 1400, Some("203.0.113.99:65001"))
        .await
        .expect("re-registration must succeed");

    let entry = repo.get_vtep("node-ep-pin").await.expect("row must exist");
    assert_eq!(
        entry.underlay_endpoint.as_deref(),
        Some("198.51.100.10:65001"),
        "first registration wins: the endpoint must not be overwritten"
    );
    assert_eq!(
        entry.public_key.as_deref(),
        Some("pub-ep-2"),
        "the public key keeps its normal upsert semantics"
    );
    assert_eq!(
        entry.underlay_mtu,
        Some(1400),
        "the underlay MTU keeps its normal upsert semantics"
    );

    // A re-report WITHOUT an endpoint must not erase the pinned value.
    repo.register_fabric_identity("node-ep-pin", "pub-ep-3", 0, None)
        .await
        .expect("re-registration without an endpoint must succeed");
    let entry = repo.get_vtep("node-ep-pin").await.expect("row must exist");
    assert_eq!(
        entry.underlay_endpoint.as_deref(),
        Some("198.51.100.10:65001")
    );

    // A node with no endpoint yet still gets one on its first
    // endpoint-carrying registration.
    seed_fabric_node_row(&pool, "node-ep-late").await;
    repo.register_fabric_identity("node-ep-late", "pub-ep-late", 1500, None)
        .await
        .expect("registration without an endpoint must succeed");
    repo.register_fabric_identity(
        "node-ep-late",
        "pub-ep-late",
        1500,
        Some("[2001:db8::1]:65001"),
    )
    .await
    .expect("registration with an endpoint must succeed");
    let entry = repo.get_vtep("node-ep-late").await.expect("row must exist");
    assert_eq!(
        entry.underlay_endpoint.as_deref(),
        Some("[2001:db8::1]:65001"),
        "a NULL endpoint is populated by the first registration carrying one"
    );
}

// --- ADR-021 fabric peer ordering (second-pass review of #500) ---

/// `get_fabric_peers_for_network` must return the flood list ordered by
/// `node_id`: the fabric-plan compile order — and therefore the per-node
/// roll-up order of the all-refusals `Unimplemented` error from the
/// overlay fan-out — is deterministic only if this query is. Seed four
/// peers in non-sorted registration order and assert the result comes
/// back sorted (without an ORDER BY, SQLite returns the rows in
/// insertion order and the roll-up would follow registration order
/// instead).
#[tokio::test]
async fn get_fabric_peers_for_network_returns_peers_in_node_id_order() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = VtepRepository::new(pool.clone());

    // Registration order deliberately differs from node_id order.
    let nodes = ["node-ord-m", "node-ord-z", "node-ord-a", "node-ord-b"];
    for node_id in nodes {
        seed_fabric_node_row(&pool, node_id).await;
        repo.register_fabric_identity(node_id, &format!("pub-{node_id}"), 1500, None)
            .await
            .expect("fabric identity registration must succeed");
    }

    // One network, one VM placement per node: every node joins the flood
    // list through the vm_nic_desired_state join.
    sqlx::query(
        "INSERT INTO networks (network_id, node_id, display_name, overlay_type) \
         VALUES ('net-ord', 'node-ord-m', 'net-ord', 'vxlan')",
    )
    .execute(&pool)
    .await
    .expect("insert network");
    for (idx, node_id) in nodes.iter().enumerate() {
        let vm_id = format!("vm-ord-{idx}");
        sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES (?, ?)")
            .bind(&vm_id)
            .bind(format!("VM {vm_id}"))
            .execute(&pool)
            .await
            .expect("insert vm");
        sqlx::query("INSERT INTO vm_desired_state (vm_id, desired_generation, target_node_id) VALUES (?, 1, ?)")
            .bind(&vm_id)
            .bind(node_id)
            .execute(&pool)
            .await
            .expect("insert vm desired state");
        sqlx::query(
            "INSERT INTO vm_nic_desired_state (nic_id, vm_id, network_id) VALUES (?, ?, 'net-ord')",
        )
        .bind(format!("nic-{vm_id}"))
        .bind(&vm_id)
        .execute(&pool)
        .await
        .expect("insert vm nic desired state");
    }

    let peers = repo
        .get_fabric_peers_for_network("net-ord")
        .await
        .expect("peer query must succeed");

    let ids: Vec<&str> = peers.iter().map(|p| p.node_id.as_str()).collect();
    assert_eq!(
        ids,
        ["node-ord-a", "node-ord-b", "node-ord-m", "node-ord-z"],
        "the flood list must come back in node_id order regardless of \
         registration order, got {ids:?}"
    );
    // Sanity: the join picked up every participant exactly once.
    assert_eq!(peers.len(), nodes.len());
}

// ─────────────────────────────────────────────────────────────────────
// #522 (review S1) — the delete tombstone vs. the mutation-verb patches
// ─────────────────────────────────────────────────────────────────────

/// Seed a volume with a `volume_desired_state` row in the given shape.
/// The tombstone case is `('Deleting'`, a small integer generation) —
/// exactly what the BFF delete route's one-tx journal writes.
async fn seed_vds_volume(pool: &StorePool, volume_id: &str, generation: i64, status: Option<&str>) {
    sqlx::query(
        "INSERT INTO volumes (volume_id, display_name, capacity_bytes) VALUES ($1, $1, 1024)",
    )
    .bind(volume_id)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO volume_desired_state (volume_id, desired_generation, desired_status) \
         VALUES ($1, $2, $3)",
    )
    .bind(volume_id)
    .bind(generation)
    .bind(status)
    .execute(pool)
    .await
    .unwrap();
}

async fn vds_shape(pool: &StorePool, volume_id: &str) -> (i64, Option<String>, Option<String>) {
    sqlx::query_as(
        "SELECT desired_generation, desired_status, attached_vm_id \
         FROM volume_desired_state WHERE volume_id = $1",
    )
    .bind(volume_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// The wall-clock-milliseconds generation the control plane mints —
/// the shape that always beat the tombstone's small integer before the
/// SQL guard landed (the S1 race).
const CP_WALL_CLOCK_GENERATION: i64 = 1_769_000_000_000;

#[tokio::test]
async fn volume_attachment_patch_refuses_to_journal_over_a_deleting_tombstone() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = DesiredStateRepository::new(pool.clone());
    seed_vds_volume(&pool, "vol-s1-attach", 2, Some("Deleting")).await;
    sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES ($1, $1)")
        .bind("vm-s1")
        .execute(&pool)
        .await
        .unwrap();

    for attached_vm_id in [Some("vm-s1"), None] {
        let err = repo
            .set_volume_attachment(&VolumeAttachmentPatchInput {
                volume_id: ResourceId::new("vol-s1-attach").unwrap(),
                desired_generation: Generation::new(CP_WALL_CLOCK_GENERATION as u64),
                desired_status: None,
                requested_by: Some("test".into()),
                updated_by: None,
                attached_vm_id: attached_vm_id.map(|id| ResourceId::new(id).unwrap()),
                requested_unix_ms: CP_WALL_CLOCK_GENERATION,
            })
            .await
            .expect_err("an attach/detach patch over a tombstone must fail");
        assert!(
            matches!(err, StoreError::Conflict { .. }),
            "the refusal must be the loud Conflict, got: {err:?}"
        );
        assert_eq!(
            vds_shape(&pool, "vol-s1-attach").await,
            (2, Some("Deleting".into()), None),
            "the tombstone must survive the refused patch byte-for-byte"
        );
    }
}

#[tokio::test]
async fn volume_resize_patch_refuses_to_journal_over_a_deleting_tombstone() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = DesiredStateRepository::new(pool.clone());
    seed_vds_volume(&pool, "vol-s1-resize", 2, Some("Deleting")).await;

    let err = repo
        .set_volume_resize(&VolumeResizePatchInput {
            volume_id: ResourceId::new("vol-s1-resize").unwrap(),
            desired_generation: Generation::new(CP_WALL_CLOCK_GENERATION as u64),
            desired_status: None,
            requested_by: Some("test".into()),
            updated_by: None,
            resize_to_bytes: Some(2_048),
            requested_unix_ms: CP_WALL_CLOCK_GENERATION,
        })
        .await
        .expect_err("a resize patch over a tombstone must fail");
    assert!(
        matches!(err, StoreError::Conflict { .. }),
        "the refusal must be the loud Conflict, got: {err:?}"
    );
    assert_eq!(
        vds_shape(&pool, "vol-s1-resize").await,
        (2, Some("Deleting".into()), None),
        "the tombstone must survive the refused patch byte-for-byte"
    );
}

#[tokio::test]
async fn volume_snapshot_patch_refuses_to_journal_over_a_deleting_tombstone() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = DesiredStateRepository::new(pool.clone());
    seed_vds_volume(&pool, "vol-s1-snap", 2, Some("Deleting")).await;

    let err = repo
        .set_volume_snapshot(&VolumeSnapshotPatchInput {
            volume_id: ResourceId::new("vol-s1-snap").unwrap(),
            desired_generation: Generation::new(CP_WALL_CLOCK_GENERATION as u64),
            desired_status: None,
            requested_by: Some("test".into()),
            updated_by: None,
            snapshot_op: Some("create".into()),
            snapshot_name: Some("snap".into()),
            requested_unix_ms: CP_WALL_CLOCK_GENERATION,
        })
        .await
        .expect_err("a snapshot patch over a tombstone must fail");
    assert!(
        matches!(err, StoreError::Conflict { .. }),
        "the refusal must be the loud Conflict, got: {err:?}"
    );
    assert_eq!(
        vds_shape(&pool, "vol-s1-snap").await,
        (2, Some("Deleting".into()), None),
        "the tombstone must survive the refused patch byte-for-byte"
    );
}

#[tokio::test]
async fn volume_patches_still_apply_to_a_live_volume() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = DesiredStateRepository::new(pool.clone());
    seed_vds_volume(&pool, "vol-s1-live", 1, None).await;
    sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES ($1, $1)")
        .bind("vm-live")
        .execute(&pool)
        .await
        .unwrap();

    repo.set_volume_attachment(&VolumeAttachmentPatchInput {
        volume_id: ResourceId::new("vol-s1-live").unwrap(),
        desired_generation: Generation::new(2),
        desired_status: None,
        requested_by: Some("test".into()),
        updated_by: None,
        attached_vm_id: Some(ResourceId::new("vm-live").unwrap()),
        requested_unix_ms: 2_000,
    })
    .await
    .expect("a patch on a live (non-tombstoned) volume must still apply");

    assert_eq!(
        vds_shape(&pool, "vol-s1-live").await,
        (2, None, Some("vm-live".into())),
        "the control case must journal exactly as before the guard"
    );
}

#[tokio::test]
async fn volume_patch_stale_generation_is_still_the_stale_generation_error() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = DesiredStateRepository::new(pool.clone());
    // NOT tombstoned — a live volume whose row generation outranks the
    // incoming patch. The disambiguation must keep this the pre-existing
    // StaleGeneration refusal, not misreport it as a delete conflict.
    seed_vds_volume(&pool, "vol-s1-stale", 100, Some("Active")).await;

    let err = repo
        .set_volume_attachment(&VolumeAttachmentPatchInput {
            volume_id: ResourceId::new("vol-s1-stale").unwrap(),
            desired_generation: Generation::new(50),
            desired_status: None,
            requested_by: Some("test".into()),
            updated_by: None,
            attached_vm_id: None,
            requested_unix_ms: 50_000,
        })
        .await
        .expect_err("a stale-generation patch must still fail");
    assert!(
        matches!(err, StoreError::StaleGeneration { .. }),
        "the stale-generation semantic must be unchanged, got: {err:?}"
    );
    assert_eq!(
        vds_shape(&pool, "vol-s1-stale").await,
        (100, Some("Active".into()), None),
        "the refused stale patch must not touch the row"
    );
}

// ─────────────────────────────────────────────────────────────────────
// #499 — the network delete tombstone vs. the fragment upsert
// ─────────────────────────────────────────────────────────────────────

/// Seed a network with a `network_desired_state` row in the given shape.
/// The tombstone case is `('Deleting'`, a small integer generation) —
/// exactly what the BFF delete route's one-tx tombstone writes (a
/// network created by the route carries generation 1; the tombstone
/// bumps it to 2).
async fn seed_nds_network(
    pool: &StorePool,
    network_id: &str,
    generation: i64,
    status: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO networks (network_id, display_name, network_class) VALUES ($1, $1, 'bridge')",
    )
    .bind(network_id)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO network_desired_state (network_id, desired_generation, desired_status) \
         VALUES ($1, $2, $3)",
    )
    .bind(network_id)
    .bind(generation)
    .bind(status)
    .execute(pool)
    .await
    .unwrap();
}

async fn nds_shape(pool: &StorePool, network_id: &str) -> (i64, Option<String>) {
    sqlx::query_as(
        "SELECT desired_generation, desired_status \
         FROM network_desired_state WHERE network_id = $1",
    )
    .bind(network_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn networks_row_shape(pool: &StorePool, network_id: &str) -> (Option<String>, String) {
    sqlx::query_as("SELECT node_id, display_name FROM networks WHERE network_id = $1")
        .bind(network_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn fragment_input(network_id: &str, generation: i64) -> NetworkDesiredStateInput {
    NetworkDesiredStateInput {
        network_id: ResourceId::new(network_id).unwrap(),
        node_id: None,
        display_name: format!("resurrected-{network_id}"),
        network_class: Some("bridge".into()),
        desired_generation: Generation::new(generation as u64),
        desired_status: None,
        requested_by: Some("agent".into()),
        updated_by: Some("agent".into()),
        firewall_rules_json: None,
        nat_rules_json: None,
        dhcp_scope_json: None,
        dns_enabled: None,
        dns_scope_json: None,
        requested_unix_ms: generation,
    }
}

fn fragment_exposures(network_id: &str) -> Vec<NetworkExposureInput> {
    vec![NetworkExposureInput {
        network_id: ResourceId::new(network_id).unwrap(),
        service_name: "web".into(),
        protocol: "tcp".into(),
        listen_address: None,
        listen_port: Some(80),
        target_address: None,
        target_port: Some(8080),
        exposure_policy: None,
        updated_unix_ms: 1_000,
    }]
}

/// The #499 resurrection pin, store tier: a fragment upsert against a
/// tombstoned network is refused (the loud Conflict), the tombstone
/// survives byte-for-byte, the physical `networks` row is NOT
/// re-asserted, and no exposure row lands. The wall-clock generation is
/// the shape that would beat the tombstone's small integer if the
/// `IS NOT 'Deleting'` term were absent.
#[tokio::test]
async fn network_fragment_upsert_refuses_to_journal_over_a_deleting_tombstone() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = DesiredStateRepository::new(pool.clone());
    seed_nds_network(&pool, "net-499-dead", 2, Some("Deleting")).await;

    let err = repo
        .upsert_network_with_exposures(
            &fragment_input("net-499-dead", CP_WALL_CLOCK_GENERATION),
            &fragment_exposures("net-499-dead"),
        )
        .await
        .expect_err("a fragment upsert over a tombstone must fail");
    assert!(
        matches!(err, StoreError::Conflict { .. }),
        "the refusal must be the loud Conflict, got: {err:?}"
    );
    assert_eq!(
        nds_shape(&pool, "net-499-dead").await,
        (2, Some("Deleting".into())),
        "the tombstone must survive the refused fragment byte-for-byte"
    );
    assert_eq!(
        networks_row_shape(&pool, "net-499-dead").await,
        (None, "net-499-dead".into()),
        "the physical row must not be re-asserted by the refused \
         fragment (the tx coupling rolls it back with the intent)"
    );
    let exposures: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM network_exposures WHERE network_id = $1")
            .bind("net-499-dead")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(exposures, 0, "no exposure row may land for a tombstone");
}

/// The regression pin: the same fragment upsert on a LIVE network
/// journals exactly as before the guard (row updated, exposures land).
#[tokio::test]
async fn network_fragment_upsert_still_applies_to_a_live_network() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = DesiredStateRepository::new(pool.clone());
    seed_nds_network(&pool, "net-499-live", 1, Some("Pending")).await;

    repo.upsert_network_with_exposures(
        &fragment_input("net-499-live", CP_WALL_CLOCK_GENERATION),
        &fragment_exposures("net-499-live"),
    )
    .await
    .expect("a fragment upsert on a live network must still apply");

    assert_eq!(
        nds_shape(&pool, "net-499-live").await,
        (CP_WALL_CLOCK_GENERATION, None),
        "the live control case must journal exactly as before the guard"
    );
    assert_eq!(
        networks_row_shape(&pool, "net-499-live").await,
        (None, "resurrected-net-499-live".into()),
        "the physical row follows the fragment on the live path"
    );
    let exposures: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM network_exposures WHERE network_id = $1")
            .bind("net-499-live")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(exposures, 1, "the live fragment's exposure must land");
}

/// The disambiguation pin: a stale generation on a LIVE network stays
/// the StaleGeneration refusal, not a misreported delete conflict.
#[tokio::test]
async fn network_fragment_stale_generation_is_still_the_stale_generation_error() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = DesiredStateRepository::new(pool.clone());
    seed_nds_network(&pool, "net-499-stale", 100, Some("Active")).await;

    let err = repo
        .upsert_network_with_exposures(
            &fragment_input("net-499-stale", 50),
            &fragment_exposures("net-499-stale"),
        )
        .await
        .expect_err("a stale-generation fragment must still fail");
    assert!(
        matches!(err, StoreError::StaleGeneration { .. }),
        "the stale-generation semantic must be unchanged, got: {err:?}"
    );
    assert_eq!(
        nds_shape(&pool, "net-499-stale").await,
        (100, Some("Active".into())),
        "the refused stale fragment must not touch the row"
    );
}

/// The sibling-verb pin (#499, the #522 review-S1 guard network-shaped):
/// a start/stop/restart status patch over a tombstone is refused with
/// the loud Conflict and the tombstone survives — the mutation verbs
/// carry wall-clock generations that would otherwise always beat the
/// tombstone's small integer.
#[tokio::test]
async fn network_status_patch_refuses_to_journal_over_a_deleting_tombstone() {
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = DesiredStateRepository::new(pool.clone());
    seed_nds_network(&pool, "net-499-verb", 2, Some("Deleting")).await;

    let err = repo
        .set_network_status(&NetworkStatusPatchInput {
            network_id: ResourceId::new("net-499-verb").unwrap(),
            desired_generation: Generation::new(CP_WALL_CLOCK_GENERATION as u64),
            desired_status: Some("Active".into()),
            requested_by: Some("test".into()),
            updated_by: None,
            requested_unix_ms: CP_WALL_CLOCK_GENERATION,
        })
        .await
        .expect_err("a status patch over a tombstone must fail");
    assert!(
        matches!(err, StoreError::Conflict { .. }),
        "the refusal must be the loud Conflict, got: {err:?}"
    );
    assert_eq!(
        nds_shape(&pool, "net-499-verb").await,
        (2, Some("Deleting".into())),
        "the tombstone must survive the refused status patch byte-for-byte"
    );
}

#[tokio::test]
async fn test_upsert_vm_vsock_opt_in_defaults_off_and_is_preserved_by_unspecified_updates() {
    // G5 (0062): the vsock opt-in is additive — a fresh upsert without
    // the field stores 0 (off, the pre-G5 meaning), Some(true) stores
    // 1, and — the load-bearing rule — a LATER upsert that does not
    // carry the field (the legacy gRPC fragment path, reconciliation
    // rewrites) preserves the stored opt-in instead of silently
    // stripping a VM's device intent.
    let test_db = TestDb::new().await;
    let pool = test_db.pool.clone();
    let repo = DesiredStateRepository::new(pool.clone());

    let base = VmDesiredStateInput {
        vm_id: ResourceId::new("vm-vsock-carry").unwrap(),
        node_id: None,
        display_name: "vsock-carry".to_string(),
        tenant_id: None,
        placement_policy: None,
        desired_generation: Generation::new(1),
        desired_status: Some("Pending".to_string()),
        requested_by: Some("test".to_string()),
        updated_by: Some("test".to_string()),
        target_node_id: None,
        cpu_count: Some(2),
        memory_bytes: Some(1024),
        image_ref: Some("img".to_string()),
        boot_mode: None,
        desired_power_state: Some("Running".to_string()),
        vsock_enabled: None,
        requested_unix_ms: 1000,
    };

    // Fresh insert, unspecified: off.
    repo.upsert_vm(&base).await.unwrap();
    let stored: Option<i64> = sqlx::query_scalar(
        "SELECT vsock_enabled FROM vm_desired_state WHERE vm_id = 'vm-vsock-carry'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored, Some(0));

    // Opt-in at a newer generation.
    let mut opted_in = base.clone();
    opted_in.desired_generation = Generation::new(2);
    opted_in.vsock_enabled = Some(true);
    opted_in.requested_unix_ms = 2000;
    repo.upsert_vm(&opted_in).await.unwrap();
    let stored: Option<i64> = sqlx::query_scalar(
        "SELECT vsock_enabled FROM vm_desired_state WHERE vm_id = 'vm-vsock-carry'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored, Some(1));

    // An unrelated newer upsert that does not carry the field preserves
    // the opt-in (COALESCE rule), never strips it.
    let mut unrelated = base.clone();
    unrelated.desired_generation = Generation::new(3);
    unrelated.requested_unix_ms = 3000;
    repo.upsert_vm(&unrelated).await.unwrap();
    let stored: Option<i64> = sqlx::query_scalar(
        "SELECT vsock_enabled FROM vm_desired_state WHERE vm_id = 'vm-vsock-carry'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        stored,
        Some(1),
        "unspecified update must preserve the opt-in"
    );

    // An explicit false at a newer generation is honored (the opt-out
    // is deliberate when a caller says so).
    let mut opted_out = base.clone();
    opted_out.desired_generation = Generation::new(4);
    opted_out.vsock_enabled = Some(false);
    opted_out.requested_unix_ms = 4000;
    repo.upsert_vm(&opted_out).await.unwrap();
    let stored: Option<i64> = sqlx::query_scalar(
        "SELECT vsock_enabled FROM vm_desired_state WHERE vm_id = 'vm-vsock-carry'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored, Some(0));
}
