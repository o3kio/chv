//! NetBox projection repository tests (config + runs). Uses the in-memory
//! SQLite test pool, mirroring `architectures/tests.rs`.
//!
//! Token-redaction note: [`NetboxProjectionConfig`] has no token field by
//! construction (a compile-level property — there is nothing to assert at
//! runtime), so `upsert`/`get` cannot leak token material; only
//! `read_token` ever decrypts.

use crate::architectures::netbox_run::retry_backoff;
use crate::architectures::*;
use crate::test_util::{test_credential_encryption, TestDb};
use crate::StoreError;
use chv_controlplane_types::architecture::{
    ArchitectureId, ArchitectureVersionId, NetboxProjectionMode, NetboxProjectionRunId,
    NetboxProjectionRunStatus, NetboxProjectionTrigger, NetboxRetentionPolicy,
};

fn aid(s: &str) -> ArchitectureId {
    ArchitectureId::new(s).unwrap()
}

fn vid(s: &str) -> ArchitectureVersionId {
    ArchitectureVersionId::new(s).unwrap()
}

fn nid(s: &str) -> NetboxProjectionRunId {
    NetboxProjectionRunId::new(s).unwrap()
}

fn make_topology_input(id: &str, name: &str) -> TopologyCreateInput {
    TopologyCreateInput {
        id: aid(id),
        name: name.to_string(),
        display_name: Some(format!("{name} display")),
        description: None,
        environment: Some("test".to_string()),
        status: chv_controlplane_types::architecture::ArchitectureStatus::Draft,
        owner_user_id: Some("user-1".to_string()),
        design_graph_json: None,
        latest_yaml: None,
    }
}

/// Creates a topology + version pair so config/run rows have valid FKs.
async fn setup_architecture(db: &TestDb, topo_id: &str, version_id: &str) {
    TopologyRepository::new(db.pool.clone())
        .create(make_topology_input(topo_id, &format!("{topo_id}-name")))
        .await
        .unwrap();
    VersionRepository::new(db.pool.clone())
        .create(VersionCreateInput {
            id: vid(version_id),
            architecture_id: aid(topo_id),
            version_number: 1,
            yaml_content: "x".to_string(),
            design_graph_json: None,
            normalized_model_json: None,
            change_summary: None,
            created_by: None,
        })
        .await
        .unwrap();
}

/// Config repository with a deterministic, injected encryption key. The
/// encrypt path is fail-closed (#655), so token-writing tests must not rely
/// on ambient `CHV_ENCRYPTION_KEY`.
fn config_repo(db: &TestDb) -> NetboxProjectionConfigRepository {
    NetboxProjectionConfigRepository::with_crypto(db.pool.clone(), test_credential_encryption())
}

fn make_config_input(topo_id: &str) -> NetboxProjectionConfigUpsertInput {
    NetboxProjectionConfigUpsertInput {
        architecture_id: aid(topo_id),
        endpoint: "https://netbox.example.internal".to_string(),
        token: Some("netbox-api-token-plaintext".to_string()),
        token_secret_ref: format!("netbox-{topo_id}"),
        retention_policy: NetboxRetentionPolicy::MarkStale,
        enable_post_apply: false,
        custom_field_prefix: "chv_".to_string(),
        site_name: Some("dc1".to_string()),
    }
}

fn make_run_input(run_id: &str, topo_id: &str, version_id: &str) -> NetboxProjectionRunCreateInput {
    NetboxProjectionRunCreateInput {
        id: nid(run_id),
        architecture_id: aid(topo_id),
        architecture_version_id: vid(version_id),
        trigger_kind: NetboxProjectionTrigger::Manual,
        mode: NetboxProjectionMode::Export,
        plan_json: None,
        requested_by: Some("senol".to_string()),
    }
}

/// The sweep-enqueued shape: `post_apply` trigger, system-requested
/// (no `requested_by`).
fn make_post_apply_run_input(
    run_id: &str,
    topo_id: &str,
    version_id: &str,
) -> NetboxProjectionRunCreateInput {
    NetboxProjectionRunCreateInput {
        trigger_kind: NetboxProjectionTrigger::PostApply,
        requested_by: None,
        ..make_run_input(run_id, topo_id, version_id)
    }
}

// ── NetboxProjectionConfigRepository ───────────────────────────────────────

#[tokio::test]
async fn netbox_config_upsert_get_roundtrip() {
    let db = TestDb::new().await;
    setup_architecture(&db, "topo-1", "v-1").await;
    let repo = config_repo(&db);

    // The upsert input always carries explicit values (SQL defaults like
    // retention 'mark_stale' only apply to raw inserts), so the roundtrip
    // must persist exactly what was provided.
    let created = repo.upsert(make_config_input("topo-1")).await.unwrap();
    assert_eq!(created.architecture_id, aid("topo-1"));
    assert_eq!(created.endpoint, "https://netbox.example.internal");
    assert_eq!(created.token_secret_ref, "netbox-topo-1");
    assert_eq!(created.retention_policy, NetboxRetentionPolicy::MarkStale);
    assert!(!created.enable_post_apply);
    assert_eq!(created.custom_field_prefix, "chv_");
    assert_eq!(created.site_name.as_deref(), Some("dc1"));
    assert!(created.created_at <= created.updated_at);

    let fetched = repo.get(&aid("topo-1")).await.unwrap().expect("config row");
    assert_eq!(fetched, created);
}

#[tokio::test]
async fn netbox_config_upsert_is_idempotent_and_refreshes_updated_at() {
    let db = TestDb::new().await;
    setup_architecture(&db, "topo-1", "v-1").await;
    let repo = config_repo(&db);

    repo.upsert(make_config_input("topo-1")).await.unwrap();

    // Timestamps have second resolution; backdate the row so the refresh
    // is observable without sleeping.
    sqlx::query(
        "UPDATE netbox_projection_config SET updated_at = '2020-01-01T00:00:00Z' \
         WHERE architecture_id = $1",
    )
    .bind("topo-1")
    .execute(&db.pool)
    .await
    .unwrap();

    let mut input = make_config_input("topo-1");
    input.endpoint = "https://netbox2.example.internal".to_string();
    input.retention_policy = NetboxRetentionPolicy::Delete;
    input.enable_post_apply = true;
    input.site_name = None;

    let updated = repo.upsert(input).await.unwrap();
    assert_eq!(updated.endpoint, "https://netbox2.example.internal");
    assert_eq!(updated.retention_policy, NetboxRetentionPolicy::Delete);
    assert!(updated.enable_post_apply);
    assert_eq!(updated.site_name, None);
    assert!(updated.updated_at.timestamp() > 1_577_836_800); // 2020-01-01
                                                             // created_at is not touched by the update path.
    assert_eq!(
        updated.created_at,
        repo.get(&aid("topo-1")).await.unwrap().unwrap().created_at
    );
}

#[tokio::test]
async fn netbox_config_upsert_unknown_topology_returns_not_found() {
    let db = TestDb::new().await;
    let repo = config_repo(&db);

    let err = repo.upsert(make_config_input("missing")).await.unwrap_err();
    assert!(
        matches!(&err, StoreError::NotFound { entity, id }
            if *entity == "architecture_topology_or_version" && id == "missing"),
        "expected NotFound, got {err:?}"
    );
}

#[tokio::test]
async fn netbox_config_get_unknown_returns_none() {
    let db = TestDb::new().await;
    let repo = config_repo(&db);

    assert!(repo.get(&aid("missing")).await.unwrap().is_none());
}

#[tokio::test]
async fn netbox_config_delete_returns_true_then_false() {
    let db = TestDb::new().await;
    setup_architecture(&db, "topo-1", "v-1").await;
    let repo = config_repo(&db);

    repo.upsert(make_config_input("topo-1")).await.unwrap();

    assert!(repo.delete(&aid("topo-1")).await.unwrap());
    assert!(!repo.delete(&aid("topo-1")).await.unwrap());
    assert!(repo.get(&aid("topo-1")).await.unwrap().is_none());
    assert!(repo.read_token(&aid("topo-1")).await.unwrap().is_none());
}

#[tokio::test]
async fn netbox_config_token_roundtrip() {
    let db = TestDb::new().await;
    setup_architecture(&db, "topo-1", "v-1").await;
    let repo = config_repo(&db);

    let mut input = make_config_input("topo-1");
    input.token = Some("PAbCd-super-secret-token".to_string());
    repo.upsert(input).await.unwrap();

    // The only decrypt path returns exactly the plaintext that was
    // encrypted at write time. (Whether encryption is active depends on
    // the ambient CHV_ENCRYPTION_KEY; the same repo instance performs
    // both directions, so the roundtrip holds either way.)
    let token = repo.read_token(&aid("topo-1")).await.unwrap();
    assert_eq!(token.as_deref(), Some("PAbCd-super-secret-token"));
}

#[tokio::test]
async fn netbox_config_update_without_token_keeps_existing_secret() {
    let db = TestDb::new().await;
    setup_architecture(&db, "topo-1", "v-1").await;
    let repo = config_repo(&db);

    let mut input = make_config_input("topo-1");
    input.token = Some("original-secret".to_string());
    repo.upsert(input).await.unwrap();

    // Token-less edit: `None` must preserve the stored credential, not
    // clobber it (the API contract: omitted/null keeps the secret).
    let mut edit = make_config_input("topo-1");
    edit.endpoint = "https://netbox2.example.internal".to_string();
    edit.token = None;
    let updated = repo.upsert(edit).await.unwrap();
    assert_eq!(updated.endpoint, "https://netbox2.example.internal");

    let token = repo.read_token(&aid("topo-1")).await.unwrap();
    assert_eq!(token.as_deref(), Some("original-secret"));

    // A careless BFF sending an empty string is treated the same as an
    // omitted token: the secret survives.
    let mut edit = make_config_input("topo-1");
    edit.token = Some("   ".to_string());
    repo.upsert(edit).await.unwrap();
    let token = repo.read_token(&aid("topo-1")).await.unwrap();
    assert_eq!(token.as_deref(), Some("original-secret"));
}

#[tokio::test]
async fn netbox_config_update_with_new_token_replaces_secret() {
    let db = TestDb::new().await;
    setup_architecture(&db, "topo-1", "v-1").await;
    let repo = config_repo(&db);

    let mut input = make_config_input("topo-1");
    input.token = Some("original-secret".to_string());
    repo.upsert(input).await.unwrap();

    let mut edit = make_config_input("topo-1");
    edit.token = Some("rotated-secret".to_string());
    repo.upsert(edit).await.unwrap();

    let token = repo.read_token(&aid("topo-1")).await.unwrap();
    assert_eq!(token.as_deref(), Some("rotated-secret"));
}

#[tokio::test]
async fn netbox_config_fresh_upsert_without_token_is_invalid_configuration() {
    let db = TestDb::new().await;
    setup_architecture(&db, "topo-1", "v-1").await;
    let repo = config_repo(&db);

    let mut input = make_config_input("topo-1");
    input.token = None;
    let err = repo.upsert(input).await.unwrap_err();
    assert!(
        matches!(&err, StoreError::InvalidConfiguration { reason }
            if reason.contains("token is required when creating")),
        "expected InvalidConfiguration, got {err:?}"
    );
    // Nothing was persisted.
    assert!(repo.get(&aid("topo-1")).await.unwrap().is_none());
}

/// #655: a NetBox token that cannot be encrypted must reject the upsert
/// before any row is written — never fall back to storing plaintext.
#[tokio::test]
async fn netbox_config_upsert_without_key_fails_closed_and_writes_no_row() {
    let db = TestDb::new().await;
    setup_architecture(&db, "topo-1", "v-1").await;
    let repo = NetboxProjectionConfigRepository::with_crypto(
        db.pool.clone(),
        crate::CredentialEncryption::without_key(),
    );

    let err = repo.upsert(make_config_input("topo-1")).await.unwrap_err();
    match err {
        StoreError::InvalidConfiguration { reason } => assert!(
            reason.contains("fail-closed"),
            "expected fail-closed reason, got {reason}"
        ),
        other => panic!("expected InvalidConfiguration, got {other:?}"),
    }

    assert!(
        repo.get(&aid("topo-1")).await.unwrap().is_none(),
        "no config row may be written when encryption fails"
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM netbox_projection_config")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

/// #655: a token-bearing update under a no-key cipher must not touch the
/// existing row (endpoint or stored ciphertext).
#[tokio::test]
async fn netbox_config_update_without_key_leaves_existing_row_unchanged() {
    let db = TestDb::new().await;
    setup_architecture(&db, "topo-1", "v-1").await;
    let keyed = config_repo(&db);
    keyed.upsert(make_config_input("topo-1")).await.unwrap();

    let no_key = NetboxProjectionConfigRepository::with_crypto(
        db.pool.clone(),
        crate::CredentialEncryption::without_key(),
    );
    let mut edit = make_config_input("topo-1");
    edit.endpoint = "https://should-not-persist.example.internal".to_string();
    edit.token = Some("replacement-token".to_string());
    assert!(no_key.upsert(edit).await.is_err());

    let row = keyed
        .get(&aid("topo-1"))
        .await
        .unwrap()
        .expect("row exists");
    assert_eq!(row.endpoint, "https://netbox.example.internal");
    assert_eq!(
        keyed.read_token(&aid("topo-1")).await.unwrap().as_deref(),
        Some("netbox-api-token-plaintext")
    );
}

#[tokio::test]
async fn netbox_config_token_decrypt_failure_is_fail_closed() {
    let db = TestDb::new().await;
    setup_architecture(&db, "topo-1", "v-1").await;
    let repo = config_repo(&db);

    repo.upsert(make_config_input("topo-1")).await.unwrap();

    // Corrupt the ciphertext out-of-band. The `enc:` prefix forces the
    // decrypt path: with a key configured it fails as Malformed/
    // AuthFailed, without one as KeyUnavailable — never a passthrough.
    sqlx::query(
        "UPDATE netbox_projection_config SET token_ciphertext = 'enc:garbage' \
         WHERE architecture_id = $1",
    )
    .bind("topo-1")
    .execute(&db.pool)
    .await
    .unwrap();

    let err = repo.read_token(&aid("topo-1")).await.unwrap_err();
    assert!(
        matches!(err, StoreError::InvalidConfiguration { .. }),
        "expected InvalidConfiguration (fail-closed), got {err:?}"
    );
}

// ── NetboxProjectionRunRepository ──────────────────────────────────────────

#[tokio::test]
async fn netbox_run_create_get_roundtrip() {
    let db = TestDb::new().await;
    setup_architecture(&db, "topo-1", "v-1").await;
    let repo = NetboxProjectionRunRepository::new(db.pool.clone());

    let run = repo
        .create(make_run_input("netrun-1", "topo-1", "v-1"))
        .await
        .unwrap();
    assert_eq!(run.id, nid("netrun-1"));
    assert_eq!(run.architecture_id, aid("topo-1"));
    assert_eq!(run.architecture_version_id, vid("v-1"));
    assert_eq!(run.trigger_kind, NetboxProjectionTrigger::Manual);
    assert_eq!(run.mode, NetboxProjectionMode::Export);
    assert_eq!(run.requested_by.as_deref(), Some("senol"));
    // Create defaults: queued, no attempt yet, no timestamps.
    assert_eq!(run.status, NetboxProjectionRunStatus::Queued);
    assert_eq!(run.attempt_count, 0);
    assert!(run.started_at.is_none());
    assert!(run.finished_at.is_none());
    assert!(run.plan_json.is_none());
    assert!(run.result_json.is_none());
    assert!(run.summary_json.is_none());
    assert!(run.error_message.is_none());

    let fetched = repo.get(&nid("netrun-1")).await.unwrap().expect("run row");
    assert_eq!(fetched, run);
}

#[tokio::test]
async fn netbox_run_create_persists_plan_json() {
    let db = TestDb::new().await;
    setup_architecture(&db, "topo-1", "v-1").await;
    let repo = NetboxProjectionRunRepository::new(db.pool.clone());

    // Dry-run style: the plan is known at enqueue time.
    let mut input = make_run_input("netrun-1", "topo-1", "v-1");
    input.plan_json = Some("{\"entries\":[{\"op\":\"create\"}]}".to_string());
    let run = repo.create(input).await.unwrap();
    assert_eq!(
        run.plan_json.as_deref(),
        Some("{\"entries\":[{\"op\":\"create\"}]}")
    );

    let fetched = repo.get(&nid("netrun-1")).await.unwrap().unwrap();
    assert_eq!(fetched.plan_json, run.plan_json);

    // Without a plan the column stays NULL (the worker may instead
    // persist results via mark_succeeded). Uses a second architecture:
    // netrun-1 is still queued, so a sibling run for topo-1 would trip
    // the one-active index instead.
    setup_architecture(&db, "topo-2", "v-2").await;
    let run = repo
        .create(make_run_input("netrun-2", "topo-2", "v-2"))
        .await
        .unwrap();
    assert_eq!(run.plan_json, None);
}

#[tokio::test]
async fn netbox_run_create_duplicate_run_id_conflicts() {
    let db = TestDb::new().await;
    setup_architecture(&db, "topo-1", "v-1").await;
    let repo = NetboxProjectionRunRepository::new(db.pool.clone());

    repo.create(make_run_input("netrun-1", "topo-1", "v-1"))
        .await
        .unwrap();
    // Drain to terminal so the retry hits the PK, not the one-active
    // partial index (which would report architecture_id).
    let claimed = repo
        .claim_next_queued(&aid("topo-1"))
        .await
        .unwrap()
        .unwrap();
    repo.mark_succeeded(&claimed.id, None, None).await.unwrap();

    let err = repo
        .create(make_run_input("netrun-1", "topo-1", "v-1"))
        .await
        .unwrap_err();
    match err {
        StoreError::Conflict { entity, id, reason } => {
            assert_eq!(entity, "netbox_projection_run");
            assert_eq!(id, "netrun-1");
            assert!(
                reason.contains("run id already exists"),
                "unexpected reason: {reason}"
            );
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
}

#[tokio::test]
async fn netbox_run_get_unknown_returns_none() {
    let db = TestDb::new().await;
    let repo = NetboxProjectionRunRepository::new(db.pool.clone());
    assert!(repo.get(&nid("missing")).await.unwrap().is_none());
}

#[tokio::test]
async fn netbox_run_create_unknown_fk_returns_not_found() {
    let db = TestDb::new().await;
    setup_architecture(&db, "topo-1", "v-1").await;
    let repo = NetboxProjectionRunRepository::new(db.pool.clone());

    // Unknown topology.
    let err = repo
        .create(make_run_input("netrun-1", "missing", "v-1"))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, StoreError::NotFound { entity, id }
            if *entity == "architecture_topology_or_version" && id == "missing"),
        "expected NotFound, got {err:?}"
    );

    // Known topology, unknown version.
    let err = repo
        .create(make_run_input("netrun-2", "topo-1", "missing-v"))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, StoreError::NotFound { entity, .. }
            if *entity == "architecture_topology_or_version"),
        "expected NotFound, got {err:?}"
    );
}

#[tokio::test]
async fn netbox_run_one_active_invariant() {
    let db = TestDb::new().await;
    setup_architecture(&db, "topo-1", "v-1").await;
    let repo = NetboxProjectionRunRepository::new(db.pool.clone());

    repo.create(make_run_input("netrun-1", "topo-1", "v-1"))
        .await
        .unwrap();

    // A second queued run for the same architecture must Conflict —
    // surfaced as 409 NETBOX_RUN_ACTIVE at the BFF.
    let err = repo
        .create(make_run_input("netrun-2", "topo-1", "v-1"))
        .await
        .unwrap_err();
    match err {
        StoreError::Conflict { entity, reason, .. } => {
            assert_eq!(entity, "netbox_projection_run");
            assert!(
                reason.contains("active run already exists"),
                "unexpected reason: {reason}"
            );
        }
        other => panic!("expected Conflict, got {other:?}"),
    }

    // Drain the active run to terminal state, then a new create succeeds.
    let claimed = repo
        .claim_next_queued(&aid("topo-1"))
        .await
        .unwrap()
        .unwrap();
    repo.mark_succeeded(&claimed.id, Some("{}".into()), Some("{}".into()))
        .await
        .unwrap();
    repo.create(make_run_input("netrun-3", "topo-1", "v-1"))
        .await
        .unwrap();
}

#[tokio::test]
// Concurrency note: this is a sequential simulation of single-statement
// atomicity, not a true multi-worker race. `claim_next_queued` performs
// the `queued → running` flip in one UPDATE, so SQLite serializes
// concurrent claims; the second `claim_next_queued` below observing
// "nothing queued" stands in for a second worker losing the race. True
// concurrency is not exercised here (single connection, no threads).
async fn netbox_run_claim_next_queued() {
    let db = TestDb::new().await;
    setup_architecture(&db, "topo-1", "v-1").await;
    let repo = NetboxProjectionRunRepository::new(db.pool.clone());

    // Nothing queued → None.
    assert!(repo
        .claim_next_queued(&aid("topo-1"))
        .await
        .unwrap()
        .is_none());
    // Other architectures are not affected.
    setup_architecture(&db, "topo-2", "v-2").await;
    repo.create(make_run_input("netrun-2", "topo-2", "v-2"))
        .await
        .unwrap();
    assert!(repo
        .claim_next_queued(&aid("topo-1"))
        .await
        .unwrap()
        .is_none());

    repo.create(make_run_input("netrun-1", "topo-1", "v-1"))
        .await
        .unwrap();

    let claimed = repo
        .claim_next_queued(&aid("topo-1"))
        .await
        .unwrap()
        .expect("queued run claimed");
    assert_eq!(claimed.id, nid("netrun-1"));
    assert_eq!(claimed.status, NetboxProjectionRunStatus::Running);
    assert!(claimed.started_at.is_some());

    // The single-statement claim means a second claim finds nothing
    // queued (sequential simulation of concurrent workers).
    assert!(repo
        .claim_next_queued(&aid("topo-1"))
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn netbox_run_mark_succeeded_from_running() {
    let db = TestDb::new().await;
    setup_architecture(&db, "topo-1", "v-1").await;
    let repo = NetboxProjectionRunRepository::new(db.pool.clone());

    repo.create(make_run_input("netrun-1", "topo-1", "v-1"))
        .await
        .unwrap();
    let claimed = repo
        .claim_next_queued(&aid("topo-1"))
        .await
        .unwrap()
        .unwrap();

    let done = repo
        .mark_succeeded(
            &claimed.id,
            Some("{\"outcomes\":[]}".to_string()),
            Some("{\"create\":0}".to_string()),
        )
        .await
        .unwrap();
    assert_eq!(done.status, NetboxProjectionRunStatus::Succeeded);
    assert!(done.finished_at.is_some());
    assert_eq!(done.result_json.as_deref(), Some("{\"outcomes\":[]}"));
    assert_eq!(done.summary_json.as_deref(), Some("{\"create\":0}"));

    // A stale worker (still holding the pre-terminal view) cannot
    // overwrite the terminal state.
    let err = repo
        .mark_succeeded(&claimed.id, Some("{\"evil\":true}".into()), None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Conflict { entity, .. } if entity == "netbox_projection_run"),
        "expected Conflict, got {err:?}"
    );
    // The stale write did not land.
    let after = repo.get(&claimed.id).await.unwrap().unwrap();
    assert_eq!(after.result_json.as_deref(), Some("{\"outcomes\":[]}"));
}

#[tokio::test]
async fn netbox_run_mark_failed_increments_attempts() {
    let db = TestDb::new().await;
    setup_architecture(&db, "topo-1", "v-1").await;
    let repo = NetboxProjectionRunRepository::new(db.pool.clone());

    repo.create(make_run_input("netrun-1", "topo-1", "v-1"))
        .await
        .unwrap();
    let claimed = repo
        .claim_next_queued(&aid("topo-1"))
        .await
        .unwrap()
        .unwrap();

    let failed = repo
        .mark_failed(&claimed.id, Some("netbox unreachable".to_string()), None)
        .await
        .unwrap();
    assert_eq!(failed.status, NetboxProjectionRunStatus::Failed);
    assert_eq!(failed.attempt_count, 1);
    assert!(failed.finished_at.is_some());
    assert_eq!(failed.error_message.as_deref(), Some("netbox unreachable"));
    assert!(
        failed.next_attempt_at.is_none(),
        "a failed run is not scheduled for retry until requeue"
    );

    // CAS guard: a terminal state cannot be re-failed.
    let err = repo
        .mark_failed(&claimed.id, Some("stale".to_string()), None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Conflict { .. }),
        "expected Conflict, got {err:?}"
    );
}

#[tokio::test]
async fn netbox_run_requeue_from_failed() {
    let db = TestDb::new().await;
    setup_architecture(&db, "topo-1", "v-1").await;
    let repo = NetboxProjectionRunRepository::new(db.pool.clone());

    repo.create(make_run_input("netrun-1", "topo-1", "v-1"))
        .await
        .unwrap();
    let claimed = repo
        .claim_next_queued(&aid("topo-1"))
        .await
        .unwrap()
        .unwrap();
    let failed = repo
        .mark_failed(&claimed.id, Some("netbox unreachable".to_string()), None)
        .await
        .unwrap();
    assert_eq!(failed.attempt_count, 1);

    let requeued = repo.requeue(&claimed.id).await.unwrap();
    assert_eq!(requeued.status, NetboxProjectionRunStatus::Queued);
    assert_eq!(requeued.attempt_count, 1, "attempt_count is preserved");
    assert!(requeued.finished_at.is_none(), "finished_at is cleared");
    assert!(
        requeued.next_attempt_at.is_some(),
        "requeue schedules the retry via next_attempt_at"
    );

    // The backoff gates the claim: nothing is claimable until it
    // elapses (simulated here by backdating the timestamp directly).
    assert!(repo
        .claim_next_queued(&aid("topo-1"))
        .await
        .unwrap()
        .is_none());
    clear_retry_backoff(&db, "netrun-1").await;

    // Requeue from a non-failed state must Conflict.
    let claimed_again = repo
        .claim_next_queued(&aid("topo-1"))
        .await
        .unwrap()
        .unwrap();
    let err = repo.requeue(&claimed_again.id).await.unwrap_err();
    assert!(
        matches!(err, StoreError::Conflict { .. }),
        "expected Conflict requeueing a running run, got {err:?}"
    );
    repo.mark_succeeded(&claimed_again.id, None, None)
        .await
        .unwrap();
    let err = repo.requeue(&claimed_again.id).await.unwrap_err();
    assert!(
        matches!(err, StoreError::Conflict { .. }),
        "expected Conflict requeueing a succeeded run, got {err:?}"
    );
}

#[tokio::test]
async fn netbox_run_requeue_while_another_run_is_active_conflicts() {
    let db = TestDb::new().await;
    setup_architecture(&db, "topo-1", "v-1").await;
    let repo = NetboxProjectionRunRepository::new(db.pool.clone());

    // The exact reachable sequence: run A fails (freeing the one-active
    // slot), run B is created (allowed), then requeueing A would flip it
    // back to queued and collide with B in the partial unique index.
    repo.create(make_run_input("netrun-a", "topo-1", "v-1"))
        .await
        .unwrap();
    let claimed_a = repo
        .claim_next_queued(&aid("topo-1"))
        .await
        .unwrap()
        .unwrap();
    repo.mark_failed(&claimed_a.id, Some("netbox unreachable".to_string()), None)
        .await
        .unwrap();

    repo.create(make_run_input("netrun-b", "topo-1", "v-1"))
        .await
        .unwrap();

    // Must surface as Conflict (409 NETBOX_RUN_ACTIVE), not a raw
    // database error.
    let err = repo.requeue(&nid("netrun-a")).await.unwrap_err();
    match err {
        StoreError::Conflict { entity, reason, .. } => {
            assert_eq!(entity, "netbox_projection_run");
            assert!(
                reason.contains("active run already exists"),
                "unexpected reason: {reason}"
            );
        }
        other => panic!("expected Conflict, got {other:?}"),
    }

    // Run A stays failed; run B is untouched.
    let a = repo.get(&nid("netrun-a")).await.unwrap().unwrap();
    assert_eq!(a.status, NetboxProjectionRunStatus::Failed);
    let b = repo.get(&nid("netrun-b")).await.unwrap().unwrap();
    assert_eq!(b.status, NetboxProjectionRunStatus::Queued);
}

/// A failed run's per-entry outcome ledger round-trips through
/// `result_json` (mark_failed) and `get` — the partial work of a failed
/// run stays inspectable and resumable.
#[tokio::test]
async fn netbox_run_mark_failed_persists_result_ledger() {
    let db = TestDb::new().await;
    setup_architecture(&db, "topo-1", "v-1").await;
    let repo = NetboxProjectionRunRepository::new(db.pool.clone());

    repo.create(make_run_input("netrun-1", "topo-1", "v-1"))
        .await
        .unwrap();
    let claimed = repo
        .claim_next_queued(&aid("topo-1"))
        .await
        .unwrap()
        .unwrap();

    let ledger = r#"{"entries":[{"status":"succeeded"},{"status":"failed"},{"status":"not_attempted"}],"summary":{"succeeded":1,"failed":1,"not_attempted":1}}"#;
    let failed = repo
        .mark_failed(
            &claimed.id,
            Some("netbox api error (status 500)".to_string()),
            Some(ledger.to_string()),
        )
        .await
        .unwrap();
    assert_eq!(failed.result_json.as_deref(), Some(ledger));

    // The ledger round-trips through get.
    let fetched = repo.get(&nid("netrun-1")).await.unwrap().unwrap();
    assert_eq!(fetched.result_json.as_deref(), Some(ledger));
    assert_eq!(
        fetched.error_message.as_deref(),
        Some("netbox api error (status 500)")
    );

    // A later success clears the failure bookkeeping (the retry that
    // succeeds must not keep showing the previous attempt's error).
    repo.requeue(&claimed.id).await.unwrap();
    clear_retry_backoff(&db, "netrun-1").await;
    let claimed = repo
        .claim_next_queued(&aid("topo-1"))
        .await
        .unwrap()
        .unwrap();
    let done = repo
        .mark_succeeded(&claimed.id, Some("{}".to_string()), None)
        .await
        .unwrap();
    assert_eq!(done.error_message, None);
    assert_eq!(done.next_attempt_at, None);
}

/// Requeue schedules the retry via `next_attempt_at` on an exponential
/// 30s·2^attempt curve capped at 30 minutes, and `claim_next_queued`
/// honors the gate: a queued run whose backoff has not elapsed is not
/// claimable.
#[tokio::test]
async fn netbox_run_requeue_sets_exponential_backoff_and_gates_claim() {
    let db = TestDb::new().await;
    setup_architecture(&db, "topo-1", "v-1").await;
    let repo = NetboxProjectionRunRepository::new(db.pool.clone());

    repo.create(make_run_input("netrun-1", "topo-1", "v-1"))
        .await
        .unwrap();

    // Attempt 1 → 60s, attempt 2 → 120s.
    for (attempt, expected_backoff_secs) in [(1, 60), (2, 120)] {
        let claimed = repo
            .claim_next_queued(&aid("topo-1"))
            .await
            .unwrap()
            .unwrap();
        repo.mark_failed(&claimed.id, Some("netbox unreachable".to_string()), None)
            .await
            .unwrap();
        let before = chrono::Utc::now();
        let requeued = repo.requeue(&claimed.id).await.unwrap();
        let after = chrono::Utc::now();
        assert_eq!(requeued.attempt_count, attempt);
        let next = requeued.next_attempt_at.expect("backoff scheduled");
        let scheduled = next.timestamp() - before.timestamp();
        let slack = (after - before).num_seconds() + 1;
        assert!(
            (expected_backoff_secs..=expected_backoff_secs + slack).contains(&scheduled),
            "attempt {attempt}: expected ~{expected_backoff_secs}s backoff, got {scheduled}s"
        );

        // The backoff gates the claim.
        assert!(
            repo.claim_next_queued(&aid("topo-1"))
                .await
                .unwrap()
                .is_none(),
            "queued run must not be claimable before the backoff elapses"
        );
        clear_retry_backoff(&db, "netrun-1").await;
    }

    // The schedule itself (including the 30-minute cap, which the
    // public API cannot reach while MAX_ATTEMPTS = 5) is pinned by the
    // unit test below.
}

/// The backoff schedule is exponential and capped at 30 minutes.
#[test]
fn retry_backoff_schedule_is_exponential_and_capped() {
    let secs = |attempt| retry_backoff(attempt).num_seconds();
    assert_eq!(secs(0), 30);
    assert_eq!(secs(1), 60);
    assert_eq!(secs(2), 120);
    assert_eq!(secs(3), 240);
    assert_eq!(secs(4), 480);
    assert_eq!(secs(5), 960);
    // 30·2⁶ = 1920s would exceed the 30-minute cap.
    assert_eq!(secs(6), 1800);
    assert_eq!(secs(100), 1800);
    assert_eq!(secs(-3), 30, "defensive: negative counts clamp to the base");
}

#[tokio::test]
async fn netbox_run_requeue_at_attempt_cap_conflicts() {
    let db = TestDb::new().await;
    setup_architecture(&db, "topo-1", "v-1").await;
    let repo = NetboxProjectionRunRepository::new(db.pool.clone());

    repo.create(make_run_input("netrun-1", "topo-1", "v-1"))
        .await
        .unwrap();

    // Burn through all MAX_ATTEMPTS attempts (each mark_failed +1, each
    // requeue keeps the count).
    for expected_attempt in 1..=MAX_ATTEMPTS {
        let claimed = repo
            .claim_next_queued(&aid("topo-1"))
            .await
            .unwrap()
            .unwrap();
        let failed = repo
            .mark_failed(&claimed.id, Some("netbox unreachable".to_string()), None)
            .await
            .unwrap();
        assert_eq!(failed.attempt_count, expected_attempt);
        if expected_attempt < MAX_ATTEMPTS {
            repo.requeue(&claimed.id).await.unwrap();
            // The exponential backoff would otherwise gate the next
            // claim for up to 30 minutes; clear it to keep the loop
            // sequential (the gating itself is covered by
            // `netbox_run_requeue_sets_exponential_backoff`).
            clear_retry_backoff(&db, "netrun-1").await;
        }
    }

    let err = repo.requeue(&nid("netrun-1")).await.unwrap_err();
    match err {
        StoreError::Conflict { reason, .. } => {
            assert!(reason.contains("exhausted"), "unexpected reason: {reason}");
        }
        other => panic!("expected Conflict at the attempt cap, got {other:?}"),
    }

    // The exhausted run stays failed for operator inspection — and no
    // longer blocks new runs.
    let exhausted = repo.get(&nid("netrun-1")).await.unwrap().unwrap();
    assert_eq!(exhausted.status, NetboxProjectionRunStatus::Failed);
    assert_eq!(exhausted.attempt_count, MAX_ATTEMPTS);
    repo.create(make_run_input("netrun-2", "topo-1", "v-1"))
        .await
        .unwrap();
}

#[tokio::test]
async fn netbox_run_list_by_architecture_ordering_and_limit() {
    let db = TestDb::new().await;
    setup_architecture(&db, "topo-1", "v-1").await;
    let repo = NetboxProjectionRunRepository::new(db.pool.clone());

    // created_at has second resolution; cycle runs to terminal states and
    // backdate the older rows so the DESC ordering is deterministic.
    for (i, run_id) in ["netrun-1", "netrun-2", "netrun-3"].iter().enumerate() {
        repo.create(make_run_input(run_id, "topo-1", "v-1"))
            .await
            .unwrap();
        let claimed = repo
            .claim_next_queued(&aid("topo-1"))
            .await
            .unwrap()
            .unwrap();
        repo.mark_succeeded(&claimed.id, None, None).await.unwrap();

        sqlx::query("UPDATE netbox_projection_runs SET created_at = $2 WHERE id = $1")
            .bind(run_id)
            .bind(format!("2026-01-0{}T00:00:00Z", i + 1))
            .execute(&db.pool)
            .await
            .unwrap();
    }

    let all = repo.list_by_architecture(&aid("topo-1"), 10).await.unwrap();
    assert_eq!(all.len(), 3);
    // Newest first.
    assert_eq!(all[0].id, nid("netrun-3"));
    assert_eq!(all[1].id, nid("netrun-2"));
    assert_eq!(all[2].id, nid("netrun-1"));

    let limited = repo.list_by_architecture(&aid("topo-1"), 2).await.unwrap();
    assert_eq!(limited.len(), 2);
    assert_eq!(limited[0].id, nid("netrun-3"));

    // Other architectures see nothing.
    setup_architecture(&db, "topo-2", "v-2").await;
    assert!(repo
        .list_by_architecture(&aid("topo-2"), 10)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn netbox_run_terminal_transition_on_missing_run_is_not_found() {
    let db = TestDb::new().await;
    let repo = NetboxProjectionRunRepository::new(db.pool.clone());

    let err = repo
        .mark_succeeded(&nid("missing"), None, None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::NotFound { entity, .. }
            if entity == "netbox_projection_run"),
        "expected NotFound, got {err:?}"
    );
    let err = repo.requeue(&nid("missing")).await.unwrap_err();
    assert!(matches!(err, StoreError::NotFound { .. }), "got {err:?}");
}

// ── Post-apply sweep support (PR 6) ─────────────────────────────────────────

/// `list_post_apply_enabled` returns only the enabled configs, ordered
/// by `architecture_id` for deterministic sweeps, and reflects
/// enablement toggles.
#[tokio::test]
async fn netbox_config_list_post_apply_enabled_filters_and_orders() {
    let db = TestDb::new().await;
    setup_architecture(&db, "topo-1", "v-1").await;
    setup_architecture(&db, "topo-2", "v-2").await;
    setup_architecture(&db, "topo-3", "v-3").await;
    let repo = config_repo(&db);

    // Insertion order differs from architecture_id order on purpose:
    // the sweep must not depend on physical row order.
    let mut enabled_mid = make_config_input("topo-2");
    enabled_mid.enable_post_apply = true;
    repo.upsert(enabled_mid).await.unwrap();
    repo.upsert(make_config_input("topo-1")).await.unwrap(); // disabled
    let mut enabled_last = make_config_input("topo-3");
    enabled_last.enable_post_apply = true;
    repo.upsert(enabled_last).await.unwrap();

    let listed = repo.list_post_apply_enabled().await.unwrap();
    let ids: Vec<ArchitectureId> = listed.iter().map(|c| c.architecture_id.clone()).collect();
    assert_eq!(ids, vec![aid("topo-2"), aid("topo-3")], "enabled only, ASC");

    // Toggling a config off removes it from the sweep set.
    repo.upsert(make_config_input("topo-2")).await.unwrap();
    let listed = repo.list_post_apply_enabled().await.unwrap();
    let ids: Vec<ArchitectureId> = listed.iter().map(|c| c.architecture_id.clone()).collect();
    assert_eq!(ids, vec![aid("topo-3")]);

    // Deleting an enabled config removes it too.
    repo.delete(&aid("topo-3")).await.unwrap();
    assert!(repo.list_post_apply_enabled().await.unwrap().is_empty());
}

/// `has_post_apply_for_version` matches any-status `post_apply` runs
/// for exactly the (architecture, version) pair — the sweep's
/// idempotency guard. A manual run never matches; a failed or
/// succeeded post_apply run still does ("already attempted").
#[tokio::test]
async fn netbox_run_has_post_apply_for_version_is_pair_scoped_and_any_status() {
    let db = TestDb::new().await;
    setup_architecture(&db, "topo-1", "v-1").await;
    setup_architecture(&db, "topo-2", "v-2").await;
    setup_architecture(&db, "topo-3", "v-3").await;
    let repo = NetboxProjectionRunRepository::new(db.pool.clone());

    // Nothing yet.
    assert!(!repo
        .has_post_apply_for_version(&aid("topo-1"), &vid("v-1"))
        .await
        .unwrap());

    // A FAILED post_apply run still counts: a permanently-failed sweep
    // enqueue must not be re-enqueued every tick (transient retries
    // are owned by the bounded auto-requeue instead).
    repo.create(make_post_apply_run_input("netrun-failed", "topo-1", "v-1"))
        .await
        .unwrap();
    let claimed = repo
        .claim_next_queued(&aid("topo-1"))
        .await
        .unwrap()
        .unwrap();
    repo.mark_failed(&claimed.id, Some("netbox unreachable".to_string()), None)
        .await
        .unwrap();
    assert!(repo
        .has_post_apply_for_version(&aid("topo-1"), &vid("v-1"))
        .await
        .unwrap());

    // A SUCCEEDED post_apply run counts too (the common case).
    repo.create(make_post_apply_run_input("netrun-done", "topo-2", "v-2"))
        .await
        .unwrap();
    let claimed = repo
        .claim_next_queued(&aid("topo-2"))
        .await
        .unwrap()
        .unwrap();
    repo.mark_succeeded(&claimed.id, None, None).await.unwrap();
    assert!(repo
        .has_post_apply_for_version(&aid("topo-2"), &vid("v-2"))
        .await
        .unwrap());

    // A manual run never matches — even terminal.
    repo.create(make_run_input("netrun-manual", "topo-3", "v-3"))
        .await
        .unwrap();
    let claimed = repo
        .claim_next_queued(&aid("topo-3"))
        .await
        .unwrap()
        .unwrap();
    repo.mark_succeeded(&claimed.id, None, None).await.unwrap();
    assert!(!repo
        .has_post_apply_for_version(&aid("topo-3"), &vid("v-3"))
        .await
        .unwrap());

    // Pair-scoped: another architecture's post_apply run does not
    // match, and neither does another version of the same
    // architecture.
    assert!(!repo
        .has_post_apply_for_version(&aid("topo-2"), &vid("v-1"))
        .await
        .unwrap());
    assert!(!repo
        .has_post_apply_for_version(&aid("topo-3"), &vid("v-1"))
        .await
        .unwrap());
}

// ── Reclamation + worker claim-loop driver (PR 4) ──────────────────────────

/// Backdate a run's `started_at` so it looks older than the reclamation
/// horizon (mirrors a worker that claimed the run and died).
async fn backdate_started_at(db: &TestDb, run_id: &str, started_at: &str) {
    sqlx::query(
        "UPDATE netbox_projection_runs SET status = 'running', started_at = $2 WHERE id = $1",
    )
    .bind(run_id)
    .bind(started_at)
    .execute(&db.pool)
    .await
    .unwrap();
}

/// Clear a requeued run's retry backoff so the next claim succeeds
/// immediately (simulates the backoff having elapsed; the scheduling
/// itself is covered by `netbox_run_requeue_sets_exponential_backoff`).
async fn clear_retry_backoff(db: &TestDb, run_id: &str) {
    sqlx::query("UPDATE netbox_projection_runs SET next_attempt_at = NULL WHERE id = $1")
        .bind(run_id)
        .execute(&db.pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn netbox_run_reclaim_stale_running_fails_only_old_running_runs() {
    let db = TestDb::new().await;
    setup_architecture(&db, "topo-1", "v-1").await;
    setup_architecture(&db, "topo-2", "v-2").await;
    let repo = NetboxProjectionRunRepository::new(db.pool.clone());

    // A stale `running` run on topo-1 (claimed 20 minutes ago).
    repo.create(make_run_input("netrun-stale", "topo-1", "v-1"))
        .await
        .unwrap();
    backdate_started_at(&db, "netrun-stale", "2026-01-01T00:00:00Z").await;

    // A fresh `running` run on topo-2 (just claimed — inside the lease).
    repo.create(make_run_input("netrun-fresh", "topo-2", "v-2"))
        .await
        .unwrap();
    repo.claim_next_queued(&aid("topo-2"))
        .await
        .unwrap()
        .unwrap();

    // A merely queued run (topo-3 — the one-active invariant forbids a
    // second active run on topo-2) is never reclaimed.
    setup_architecture(&db, "topo-3", "v-3").await;
    repo.create(make_run_input("netrun-queued", "topo-3", "v-3"))
        .await
        .unwrap();

    let before = chrono::Utc::now() - chrono::Duration::minutes(15);
    let reclaimed = repo.reclaim_stale_running(before).await.unwrap();
    assert_eq!(reclaimed.len(), 1, "only the stale running run");
    assert_eq!(reclaimed[0].id, nid("netrun-stale"));
    assert_eq!(reclaimed[0].status, NetboxProjectionRunStatus::Failed);
    assert!(
        reclaimed[0]
            .error_message
            .as_deref()
            .unwrap_or_default()
            .contains("reclaimed"),
        "reclamation message set: {:?}",
        reclaimed[0].error_message
    );
    assert_eq!(
        reclaimed[0].attempt_count, 1,
        "reclamation consumed an attempt"
    );

    // The fresh running run and the queued run are untouched.
    let fresh = repo.get(&nid("netrun-fresh")).await.unwrap().unwrap();
    assert_eq!(fresh.status, NetboxProjectionRunStatus::Running);
    let queued = repo.get(&nid("netrun-queued")).await.unwrap().unwrap();
    assert_eq!(queued.status, NetboxProjectionRunStatus::Queued);

    // Idempotent: a second sweep finds nothing.
    assert!(repo.reclaim_stale_running(before).await.unwrap().is_empty());

    // The reclaimed run stays retryable under the attempt cap.
    repo.requeue(&nid("netrun-stale")).await.unwrap();
}

#[tokio::test]
async fn netbox_run_list_architecture_ids_with_queued_is_distinct() {
    let db = TestDb::new().await;
    setup_architecture(&db, "topo-1", "v-1").await;
    setup_architecture(&db, "topo-2", "v-2").await;
    let repo = NetboxProjectionRunRepository::new(db.pool.clone());

    // Two queued runs on topo-1 (one per active tick is claimed, so the
    // worker lists ids, not runs), one queued on topo-2.
    repo.create(make_run_input("netrun-a", "topo-1", "v-1"))
        .await
        .unwrap();
    // topo-1 cannot hold two active runs — run the first to terminal.
    let claimed = repo
        .claim_next_queued(&aid("topo-1"))
        .await
        .unwrap()
        .unwrap();
    repo.mark_succeeded(&claimed.id, None, None).await.unwrap();
    repo.create(make_run_input("netrun-b", "topo-1", "v-1"))
        .await
        .unwrap();
    repo.create(make_run_input("netrun-c", "topo-2", "v-2"))
        .await
        .unwrap();

    let ids = repo.list_architecture_ids_with_queued().await.unwrap();
    assert_eq!(ids, vec![aid("topo-1"), aid("topo-2")], "distinct + sorted");

    // Claiming topo-1's run removes it from the driver list.
    repo.claim_next_queued(&aid("topo-1"))
        .await
        .unwrap()
        .unwrap();
    let ids = repo.list_architecture_ids_with_queued().await.unwrap();
    assert_eq!(ids, vec![aid("topo-2")]);

    // And draining everything empties the list.
    repo.claim_next_queued(&aid("topo-2"))
        .await
        .unwrap()
        .unwrap();
    assert!(repo
        .list_architecture_ids_with_queued()
        .await
        .unwrap()
        .is_empty());
}
