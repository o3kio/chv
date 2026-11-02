//! End-to-end seed-idempotency test.
//!
//! Spins up an in-memory SQLite, runs the full migrations stack, calls
//! [`seed_if_first_deployment`] and verifies:
//!
//! - First call returns `Seeded { count: 6 }`, sentinel flips to `'1'`,
//!   `architecture_topologies` has 6 rows.
//! - Second call returns `Skipped`, no duplicate rows are added.
//! - Manually flipping the sentinel back to `'0'`, deleting one starter,
//!   and re-calling: re-creates the missing starter and skips the five
//!   surviving ones via the per-fixture `AlreadyExists` path. Sentinel
//!   ends at `'1'` again.

use chv_architecture_validate::parse_yaml;
use chv_controlplane_seed::{seed_if_first_deployment, SeedOutcome, STARTER_FIXTURES};
use chv_controlplane_store::{
    connect_pool, run_migrations, ControlPlaneStoreConfig, TopologyListFilter, TopologyRepository,
};
use chv_controlplane_types::architecture::ArchitectureStatus;
use std::path::PathBuf;

fn migrations_dir() -> PathBuf {
    // Resolve the repo migrations folder relative to this crate, which
    // sits at `crates/chv-controlplane-seed/`. Two parents up gets us to
    // the workspace root, then descend into the canonical migrations
    // directory used by the rest of the workspace.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("cmd")
        .join("chv-controlplane")
        .join("migrations")
}

async fn fresh_pool_with_migrations() -> chv_controlplane_store::StorePool {
    let config = ControlPlaneStoreConfig {
        database_url: "sqlite::memory:".to_string(),
        migrations_dir: migrations_dir(),
        max_connections: 4,
        acquire_timeout_secs: 5,
    };
    let pool = connect_pool(&config).await.expect("connect in-memory pool");
    run_migrations(&pool, Some(&config))
        .await
        .expect("run migrations");
    pool
}

async fn read_sentinel(pool: &chv_controlplane_store::StorePool) -> Option<String> {
    sqlx::query_scalar("SELECT value FROM system_settings WHERE key = 'seed_starters_completed'")
        .fetch_optional(pool)
        .await
        .expect("sentinel read")
}

async fn count_topologies(repo: &TopologyRepository) -> usize {
    repo.list(TopologyListFilter {
        include_archived: true,
        visible_to_user: None,
    })
    .await
    .expect("list topologies")
    .len()
}

#[tokio::test]
async fn first_run_seeds_six_and_flips_sentinel() {
    let pool = fresh_pool_with_migrations().await;
    let repo = TopologyRepository::new(pool.clone());

    let outcome = seed_if_first_deployment(&repo).await.expect("seed ok");
    assert_eq!(
        outcome,
        SeedOutcome::Seeded { count: 6 },
        "first run must seed all 6 fixtures"
    );
    assert_eq!(read_sentinel(&pool).await.as_deref(), Some("1"));
    assert_eq!(count_topologies(&repo).await, 6);

    // Reviewer test-analyzer F11: pin the §3 status=draft invariant
    // at the DB level. The plan promises starters NEVER auto-apply;
    // status=draft is the structural enforcement.
    let statuses: Vec<String> =
        sqlx::query_scalar("SELECT status FROM architecture_topologies ORDER BY id")
            .fetch_all(&pool)
            .await
            .expect("read statuses");
    assert_eq!(statuses.len(), 6);
    for s in &statuses {
        assert_eq!(
            s,
            ArchitectureStatus::Draft.as_str(),
            "every starter must be seeded as draft, never planned/applied"
        );
    }

    // Reviewer language-spec G3: pin owner_user_id IS NULL on every
    // starter so a future bootstrap-admin auto-claim can't slip in.
    let null_owners: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM architecture_topologies WHERE owner_user_id IS NULL",
    )
    .fetch_one(&pool)
    .await
    .expect("count null owners");
    assert_eq!(null_owners, 6, "every starter must be system-owned (NULL)");

    // The seeder must persist a v1.0 canvas graph payload, not the raw
    // CHVArchitecture model JSON, so the designer overview renders starters.
    let graph_blobs: Vec<String> =
        sqlx::query_scalar("SELECT design_graph_json FROM architecture_topologies ORDER BY id")
            .fetch_all(&pool)
            .await
            .expect("read graph blobs");
    assert_eq!(graph_blobs.len(), 6);
    for blob in &graph_blobs {
        let parsed: serde_json::Value =
            serde_json::from_str(blob).expect("design_graph_json must be valid JSON");
        assert_eq!(parsed["version"], "1.0");
        assert!(parsed["nodes"].is_array());
        assert!(parsed["edges"].is_array());
    }
}

#[tokio::test]
async fn second_run_is_no_op() {
    let pool = fresh_pool_with_migrations().await;
    let repo = TopologyRepository::new(pool.clone());

    let _ = seed_if_first_deployment(&repo).await.expect("first seed");
    assert_eq!(count_topologies(&repo).await, 6);

    let outcome = seed_if_first_deployment(&repo)
        .await
        .expect("second seed ok");
    assert_eq!(
        outcome,
        SeedOutcome::Skipped,
        "second run must short-circuit on the sentinel"
    );
    assert_eq!(
        count_topologies(&repo).await,
        6,
        "second run must not duplicate rows"
    );
}

#[tokio::test]
async fn opt_in_re_seed_recreates_missing_starter() {
    let pool = fresh_pool_with_migrations().await;
    let repo = TopologyRepository::new(pool.clone());

    let _ = seed_if_first_deployment(&repo).await.expect("first seed");
    assert_eq!(count_topologies(&repo).await, 6);

    // Reviewer test-analyzer F12: derive the starter id from the
    // STARTER_FIXTURES const rather than hand-typing it. If the id
    // format ever changes (e.g. starter-NN-<slug> → starter-<slug>-NN),
    // the test fails on the construction line, not on a confusing
    // "expected to delete exactly one row" mismatch.
    let target_index: usize = 3; // 0-based -> "starter-04-k8s-ha"
    let target_fixture = STARTER_FIXTURES[target_index];
    let target_id = format!("starter-{:02}-{}", target_index + 1, target_fixture.slug);
    assert_eq!(target_fixture.slug, "k8s-ha", "fixture order changed");

    let deleted = sqlx::query("DELETE FROM architecture_topologies WHERE id = $1")
        .bind(&target_id)
        .execute(&pool)
        .await
        .expect("delete one starter")
        .rows_affected();
    assert_eq!(deleted, 1, "expected to delete exactly one starter row");
    assert_eq!(count_topologies(&repo).await, 5);

    // Flip the sentinel back to '0' so the seeder runs again.
    sqlx::query("UPDATE system_settings SET value = '0' WHERE key = 'seed_starters_completed'")
        .execute(&pool)
        .await
        .expect("flip sentinel back");

    let outcome = seed_if_first_deployment(&repo).await.expect("re-seed ok");
    // Only one fixture was actually inserted on this run; the other five
    // were skipped via the per-fixture AlreadyExists branch.
    assert_eq!(
        outcome,
        SeedOutcome::Seeded { count: 1 },
        "re-seed must insert exactly the missing starter"
    );
    assert_eq!(
        count_topologies(&repo).await,
        6,
        "re-seed must restore the full set"
    );
    assert_eq!(read_sentinel(&pool).await.as_deref(), Some("1"));
}

// ─────────────────────────────────────────────────────────────────────
// Reviewer-driven additions
// ─────────────────────────────────────────────────────────────────────

/// Reviewer language-spec F3 / test-analyzer #1: two control planes
/// booting concurrently against the same DB must produce 6 rows total,
/// not 12. The atomic `INSERT … ON CONFLICT … WHERE value = '0'` claim
/// flips the sentinel under SQLite's per-connection write lock — only
/// one process wins, the other observes `rows_affected = 0` and skips.
#[tokio::test]
async fn concurrent_seeders_produce_six_rows_total() {
    let pool = fresh_pool_with_migrations().await;
    let repo_a = TopologyRepository::new(pool.clone());
    let repo_b = TopologyRepository::new(pool.clone());

    let (a, b) = tokio::join!(
        seed_if_first_deployment(&repo_a),
        seed_if_first_deployment(&repo_b),
    );
    let outcome_a = a.expect("arm a");
    let outcome_b = b.expect("arm b");

    // Exactly one arm must have claimed the seed; the other must have
    // skipped. No outcome is allowed to be Seeded { count: 0 } because
    // that would mean one arm got past the claim and inserted nothing,
    // which is the silent-double-claim bug the atomic UPDATE prevents.
    let outcomes = [outcome_a, outcome_b];
    let seeded_count = outcomes
        .iter()
        .filter(|o| matches!(o, SeedOutcome::Seeded { count: 6 }))
        .count();
    let skipped_count = outcomes
        .iter()
        .filter(|o| matches!(o, SeedOutcome::Skipped))
        .count();
    assert_eq!(
        seeded_count, 1,
        "exactly one arm must report Seeded {{ count: 6 }}; got {outcomes:?}"
    );
    assert_eq!(
        skipped_count, 1,
        "exactly one arm must report Skipped; got {outcomes:?}"
    );

    // Total rows must be exactly 6 — no race-driven duplicates.
    assert_eq!(count_topologies(&repo_a).await, 6);
    assert_eq!(read_sentinel(&pool).await.as_deref(), Some("1"));
}

/// Reviewer test-analyzer F4 / language-spec F2: a sloppy manual
/// `UPDATE … SET value='1 '` (trailing whitespace) must NOT trigger
/// a re-seed. The atomic claim's `WHERE trim(value) = '0'` makes the
/// match trim-tolerant on the input side, so a value of `'1 '` keeps
/// the seeder firmly skipped.
#[tokio::test]
async fn sentinel_with_whitespace_does_not_re_seed() {
    let pool = fresh_pool_with_migrations().await;
    let repo = TopologyRepository::new(pool.clone());

    let _ = seed_if_first_deployment(&repo).await.expect("first seed");
    assert_eq!(count_topologies(&repo).await, 6);

    // Operator typo: trailing space.
    sqlx::query("UPDATE system_settings SET value = '1 ' WHERE key = 'seed_starters_completed'")
        .execute(&pool)
        .await
        .expect("set whitespace sentinel");

    let outcome = seed_if_first_deployment(&repo).await.expect("re-seed ok");
    assert_eq!(
        outcome,
        SeedOutcome::Skipped,
        "whitespace-padded sentinel must skip; '1 ' is not '0'"
    );
    assert_eq!(count_topologies(&repo).await, 6, "no extra rows");
}

/// Reviewer language-spec M2 + test-analyzer #4: the sentinel row was
/// deleted out-of-band (DB rollback / manual cleanup / migration error).
/// The seeder must recover — the atomic claim's UPSERT re-creates the
/// sentinel row with `value = '1'` and seeds the missing starters.
#[tokio::test]
async fn deleted_sentinel_row_self_heals() {
    let pool = fresh_pool_with_migrations().await;
    let repo = TopologyRepository::new(pool.clone());

    // Wipe the sentinel row entirely — simulate operator running
    // `DELETE FROM system_settings WHERE key = 'seed_starters_completed'`.
    let deleted = sqlx::query("DELETE FROM system_settings WHERE key = 'seed_starters_completed'")
        .execute(&pool)
        .await
        .expect("delete sentinel")
        .rows_affected();
    assert_eq!(deleted, 1);
    assert!(read_sentinel(&pool).await.is_none(), "sentinel is gone");

    let outcome = seed_if_first_deployment(&repo).await.expect("seed ok");
    assert_eq!(
        outcome,
        SeedOutcome::Seeded { count: 6 },
        "deleted sentinel must self-heal and seed all 6"
    );
    assert_eq!(
        read_sentinel(&pool).await.as_deref(),
        Some("1"),
        "sentinel must be re-created and flipped to '1'"
    );
    assert_eq!(count_topologies(&repo).await, 6);
}

/// Reviewer test-analyzer F10 (existing-issue #2): pin the emitter as
/// a fixed point. Round-tripping every fixture through
/// `to_yaml(parse_yaml(yaml))` must produce a stable string — a future
/// change to the YAML emitter that subtly breaks key ordering or
/// quoting style fails this test before it ships a broken seeder.
#[test]
fn fixture_yaml_emitter_is_stable() {
    use chv_architecture_validate::to_yaml;
    for fixture in STARTER_FIXTURES {
        let model = parse_yaml(fixture.yaml).expect("parse fixture");
        let emit_a = to_yaml(&model).expect("emit once");
        let model_b = parse_yaml(&emit_a).expect("re-parse");
        let emit_b = to_yaml(&model_b).expect("emit twice");
        assert_eq!(
            emit_a, emit_b,
            "fixture {} emitter is not a fixed point",
            fixture.name
        );
    }
}
