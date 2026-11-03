//! First end-to-end sender↔receiver storage-migration test (M4.6, PR B).
//!
//! Everything on the path is production code: a real `LocalFileBackend` on
//! both sides (source and destination in separate tempdirs), the real
//! receiver served by [`chv_stord_core::server::serve_migration_tls`] on a
//! loopback mTLS TCP listener (rcgen certificate material, mandatory client
//! auth), and the real [`MigrationSender`] connecting with a real
//! [`MigrationTlsConfig`] over `https://127.0.0.1:<port>`.
//!
//! These tests pin the acknowledgment-protocol fixes for issue #391:
//!
//! - **Small volume** (2 chunks — not a multiple of the receiver's 64-chunk
//!   ack interval): previously the sender's bulk-phase drain waited for an
//!   ack that never came and timed out after 30 s. The receiver now flushes
//!   its ack window at phase boundaries, so the migration completes and the
//!   destination file matches the source bytes exactly.
//! - **Dirty rounds**: a pre-migration `write_block` on the source (the only
//!   protocol-level dirty generator per issue #394 — the source bitmap is
//!   populated by `LocalFileBackend::write_block`, and in production nothing
//!   calls it on a source during migration) must be picked up by a dirty
//!   sync round, transferred, and present in the destination. Before the
//!   round-ack fix the sender blocked forever after `RoundComplete`.
//! - **Pause handshake**: with a task in a `MigrationTaskTable` (the stord
//!   handler path), the sender must reach `PausedFinalSync` and *stay*
//!   blocked until `ResumeDiskMigration{vm_paused:true}` semantics (a
//!   `true` on the task's `pause_tx` watch) release it into FinalSync.
//!
//! The write canary (issue #394, Option A) is pinned by two tests below:
//!
//! - **Fail fast**: a source write that lands during bulk copy (the write
//!   path the dirty bitmap cannot see) must fail the migration at the
//!   first dirty-round boundary with the distinct
//!   `source_modified_during_migration` code — *before* the VM pause and
//!   *before* the finalize digest is computed (the empty
//!   `finalize_volume_digest` proves the failure was early, not the
//!   post-transfer #392 failure).
//! - **Residual window stays fail-closed**: a source write landing *after*
//!   the pre-pause gate (while the sender waits for the pause) is beyond
//!   the canary's reach by design; the post-pause whole-volume digest must
//!   still catch it and fail the migration with `data_loss` at finalize.
//!
//! Fail-closed semantics at the chunk level — the receiver's
//! CRC-mismatch rejection and out-of-bounds chunk rejection, and the
//! sender failing on a CRC-mismatch Ack arriving at the FinalizeAck
//! wait — are pinned by `tests/migration_failclosed.rs`, which drives
//! the *served* receiver with a hand-crafted protocol client (the
//! receiver has no unit-test module of its own). TLS-layer peer
//! rejection is covered by the negative mTLS listener tests in
//! `tests/migration_mtls.rs`; the finalize-digest corruption direction
//! is covered by the last test below.
//!
//! Since the destination-digest verification landed (issue #392), the
//! finalize exchange carries a versioned full-volume SHA-256 digest
//! (`"sha256:"` + 32 raw bytes) which the receiver re-computes over the
//! destination before reporting `verified`. Because the receiver fails
//! closed on an empty or unrecognized digest, the happy-path tests below
//! completing at all is itself evidence that the finalize exchange carried
//! a real, parseable checksum; the corruption test pins the other
//! direction (garbage in the destination ⇒ Failed, never Completed).

use chv_common::migration_auth::MigrationTokenCodec;
use chv_common::types::{BackendLocator, DevicePolicy};
use chv_stord_backends::{LocalFileBackend, StorageBackend};
use chv_stord_core::migration::auth::{MigrationLimits, MigrationReceiverAuthorization};
use chv_stord_core::migration::sender::{MigrationSender, MigrationTlsConfig};
use chv_stord_core::migration::service::StorageMigrationServiceImpl;
use chv_stord_core::migration::task::{MigrationPhase, MigrationTask, MigrationTaskTable};
use chv_stord_core::migration::tls_config::MigrationServerTls;
use chv_stord_core::server::serve_migration_tls;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tonic::Request;

/// Install the rustls ring crypto provider (same as `cmd/chv-stord`
/// startup). Idempotent.
fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Migration block size (must match the backends' dirty-tracking block size
/// so bitmap bits map 1:1 to migration chunks).
const BLOCK: u64 = 4 * 1024 * 1024;

/// Dedicated migration-signing secret shared by the in-test control-plane
/// minter and the receiver (issue #663). Never reused for anything else.
const MIGRATION_SIGNING_SECRET: &[u8] = b"e2e-migration-signing-secret-0123456789abcdef";

/// Node id the receiver expects in the token's destination claim.
const DESTINATION_NODE_ID: &str = "stord-receiver";

/// Mint the control-plane authorization a real `MigrateVm` dispatch would
/// attach to `InitMigration`.
fn migration_token(volume_id: &str, size_bytes: u64) -> String {
    MigrationTokenCodec::new(MIGRATION_SIGNING_SECRET)
        .expect("test signing secret is long enough")
        .mint(
            "stord-sender",
            DESTINATION_NODE_ID,
            volume_id,
            size_bytes,
            Duration::from_secs(300),
        )
        .expect("token mint must succeed")
}

/// Overall guard so a protocol regression fails fast instead of hanging.
const MIGRATION_TIMEOUT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------
// rcgen certificate material (same pattern as tests/migration_mtls.rs)
// ---------------------------------------------------------------------

/// A test CA that can issue leaf certificates.
struct TestCa {
    params: rcgen::CertificateParams,
    key: rcgen::KeyPair,
    cert_pem: Vec<u8>,
}

fn test_ca(cn: &str) -> TestCa {
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::default();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, cn);
    let cert = params.self_signed(&key).unwrap();
    TestCa {
        params,
        key,
        cert_pem: cert.pem().into_bytes(),
    }
}

/// Issue a leaf certificate signed by `ca`. Server leaves carry the
/// `localhost`/127.0.0.1 identities and serverAuth EKU; client leaves carry
/// clientAuth EKU.
fn issue_leaf(ca: &TestCa, cn: &str, server: bool) -> (Vec<u8>, Vec<u8>) {
    use rcgen::string::Ia5String;

    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::default();
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, cn);
    params.is_ca = rcgen::IsCa::NoCa;
    params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![if server {
        rcgen::ExtendedKeyUsagePurpose::ServerAuth
    } else {
        rcgen::ExtendedKeyUsagePurpose::ClientAuth
    }];
    if server {
        params.subject_alt_names.push(rcgen::SanType::DnsName(
            Ia5String::try_from("localhost").unwrap(),
        ));
        params.subject_alt_names.push(rcgen::SanType::IpAddress(
            std::net::Ipv4Addr::LOCALHOST.into(),
        ));
    }
    let issuer = rcgen::Issuer::from_params(&ca.params, &ca.key);
    let cert = params.signed_by(&key, &issuer).unwrap();
    (cert.pem().into_bytes(), key.serialize_pem().into_bytes())
}

// ---------------------------------------------------------------------
// Test environment
// ---------------------------------------------------------------------

/// Serve the production migration receiver on an ephemeral loopback mTLS
/// TCP listener backed by `dest_dir`. Returns the bound address and the
/// test CA (which also issued the receiver's server leaf, so the same CA
/// can issue the sender's client identity).
async fn spawn_receiver(dest_dir: &Path) -> (SocketAddr, TestCa) {
    let ca = test_ca("chv-migration-e2e-ca");
    let (server_cert, server_key) = issue_leaf(&ca, "stord-receiver", true);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let tls = MigrationServerTls {
        listen_addr: addr,
        cert_pem: server_cert,
        key_pem: server_key,
        client_ca_pem: ca.cert_pem.clone(),
    };
    let backend = Arc::new(LocalFileBackend::new(dest_dir.to_path_buf()));
    let service = StorageMigrationServiceImpl::new(
        backend,
        dest_dir.to_path_buf(),
        MigrationReceiverAuthorization::required(MIGRATION_SIGNING_SECRET, DESTINATION_NODE_ID)
            .expect("test authorization must construct"),
        MigrationLimits::default(),
    );
    tokio::spawn(async move {
        if let Err(e) = serve_migration_tls(listener, tls, service).await {
            panic!("migration TLS listener failed: {e:?}");
        }
    });
    (addr, ca)
}

/// Build the sender's mTLS config from a client leaf issued by the same CA
/// the receiver trusts.
fn sender_tls(ca: &TestCa) -> MigrationTlsConfig {
    let (client_cert, client_key) = issue_leaf(ca, "stord-sender", false);
    MigrationTlsConfig {
        cert_pem: client_cert,
        key_pem: client_key,
        ca_pem: ca.cert_pem.clone(),
        dest_domain: "localhost".to_string(),
    }
}

/// A non-zero fill byte that varies per 4 MiB block, so bulk-copy
/// correctness (right block, right order) is observable in the destination.
fn block_fill(block_index: u64) -> u8 {
    (block_index as u8).wrapping_mul(37).wrapping_add(11) | 1
}

/// Write `blocks` 4 MiB blocks of per-block non-zero pattern to the source
/// image file and return the expected bytes.
fn write_source_image(dir: &Path, file: &str, blocks: u64) -> Vec<u8> {
    let mut image = Vec::with_capacity((blocks * BLOCK) as usize);
    for block in 0..blocks {
        image.extend(std::iter::repeat_n(block_fill(block), BLOCK as usize));
    }
    std::fs::write(dir.join(file), &image).unwrap();
    image
}

/// Open the source volume on the backend and enable dirty tracking (the
/// stord `TriggerDiskMigration` handler does exactly this before spawning
/// the sender). Returns the attachment handle.
async fn open_source_volume(
    backend: &LocalFileBackend,
    volume_id: &str,
    locator: &str,
    size_bytes: u64,
) -> String {
    let export = backend
        .open(
            volume_id,
            &BackendLocator {
                backend_class: "local".to_string(),
                locator: locator.to_string(),
                options: [("size_bytes".to_string(), size_bytes.to_string())]
                    .into_iter()
                    .collect(),
            },
            &DevicePolicy::default(),
        )
        .await
        .expect("source volume must open");
    backend
        .enable_dirty_tracking(volume_id, &export.attachment_handle, size_bytes)
        .await
        .expect("dirty tracking must be enabled");
    export.attachment_handle
}

/// Simulate `ResumeDiskMigration{vm_paused:true}` as soon as the sender
/// requests the pause (used by the tests that are not about the handshake
/// itself).
fn spawn_auto_pause(task: Arc<MigrationTask>) {
    tokio::spawn(async move {
        loop {
            let (needs_pause, failed) = {
                let state = task.state.read().await;
                (state.needs_vm_pause, state.phase == MigrationPhase::Failed)
            };
            if needs_pause {
                task.pause_tx.send(true).unwrap();
                return;
            }
            if failed {
                return; // the migration failed; the test asserts on the result
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    });
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

/// The sender records the finalize digest on the task as
/// `"<algo>:<hex>"` once computed; happy-path tests assert the finalize
/// exchange genuinely carried a non-empty, versioned checksum.
fn assert_finalize_digest_observed(state: &chv_stord_core::migration::task::MigrationTaskState) {
    assert!(
        state.finalize_volume_digest.starts_with("sha256:"),
        "finalize digest must be versioned, got: {}",
        state.finalize_volume_digest
    );
    assert_eq!(
        state.finalize_volume_digest.len(),
        "sha256:".len() + 2 * 32,
        "finalize digest must be sha256 + 64 hex chars, got: {}",
        state.finalize_volume_digest
    );
}

/// **Small volume** (issue #391, ack-window flush): an 8 MiB volume is 2
/// chunks — not a multiple of the receiver's 64-chunk ack interval. Before
/// the flush fix the sender's bulk-phase drain waited for an interval ack
/// that never came and failed with a 30 s timeout; now the boundary flush
/// completes it. The destination file must match the source bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn small_volume_completes_and_matches() {
    install_crypto_provider();

    let src_dir = tempfile::tempdir().unwrap();
    let dest_dir = tempfile::tempdir().unwrap();

    // 8 MiB = 2 chunks of 4 MiB.
    let expected = write_source_image(src_dir.path(), "vol.img", 2);

    let src_backend = Arc::new(LocalFileBackend::new(src_dir.path().to_path_buf()));
    let handle = open_source_volume(&src_backend, "vol-e2e-small", "vol.img", 2 * BLOCK).await;

    let (addr, ca) = spawn_receiver(dest_dir.path()).await;

    let (task, _pause_rx) = MigrationTask::new(
        "vol-e2e-small".to_string(),
        handle.clone(),
        format!("https://{addr}"),
    );
    spawn_auto_pause(task.clone());

    let sender = MigrationSender::new(src_backend, "vol-e2e-small".to_string(), handle)
        .with_migration_authorization(migration_token("vol-e2e-small", 2 * BLOCK))
        .with_tls(sender_tls(&ca))
        .with_task(task.clone());

    let endpoint = format!("https://{addr}");
    tokio::time::timeout(MIGRATION_TIMEOUT, sender.start_migration(endpoint))
        .await
        .expect("migration must not hang (ack-window flush regression?)")
        .expect("migration must succeed");
    {
        let state = task.state.read().await;
        assert_eq!(state.phase, MigrationPhase::Completed);
        // #582 residue: no pause wait is pending on a completed task.
        assert!(
            !state.needs_vm_pause,
            "a Completed task must not report a pending VM pause"
        );
        assert_finalize_digest_observed(&state);
    }

    let dest = std::fs::read(dest_dir.path().join("vol-e2e-small.img"))
        .expect("receiving volume file must exist");
    assert_eq!(
        dest, expected,
        "destination must match the source bytes exactly"
    );
}

/// **Dirty rounds converge pre-seeded writes** (issue #391, round
/// acknowledgment): dirty blocks written via `write_block` *before* the
/// migration starts (the only protocol-level dirty generator per issue
/// #394 — and the only timing the quiescent-source contract permits,
/// since the write canary fails the migration on any source write after
/// the baseline) must be picked up by a dirty sync round, transferred,
/// and present in the destination. Before the round-ack fix the sender
/// blocked forever after `RoundComplete`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dirty_rounds_converge_preseeded_writes() {
    install_crypto_provider();

    let src_dir = tempfile::tempdir().unwrap();
    let dest_dir = tempfile::tempdir().unwrap();

    // 32 MiB = 8 chunks (still below the 64-chunk ack interval, so the
    // round path also depends on the boundary flush).
    let blocks: u64 = 8;
    let mut expected = write_source_image(src_dir.path(), "vol.img", blocks);

    let src_backend = Arc::new(LocalFileBackend::new(src_dir.path().to_path_buf()));
    let volume_id = "vol-e2e-dirty".to_string();
    let handle = open_source_volume(&src_backend, &volume_id, "vol.img", blocks * BLOCK).await;

    // Pre-seed two deterministic dirty blocks — block 1 and the last
    // block — BEFORE the sender starts, so the first dirty sync round
    // has real payload at two distant offsets and the migration stays
    // inside the quiescent-source contract (the canary baseline is
    // sampled after these writes).
    let d0 = vec![0xD0u8; 64 * 1024];
    src_backend
        .write_block(&volume_id, &handle, BLOCK, &d0)
        .await
        .expect("pre-seed dirty write must succeed");
    expected[BLOCK as usize..BLOCK as usize + d0.len()].copy_from_slice(&d0);

    let d1 = vec![0xD1u8; 64 * 1024];
    let d1_offset = (blocks - 1) * BLOCK;
    src_backend
        .write_block(&volume_id, &handle, d1_offset, &d1)
        .await
        .expect("pre-seed dirty write must succeed");
    expected[d1_offset as usize..d1_offset as usize + d1.len()].copy_from_slice(&d1);

    let (addr, ca) = spawn_receiver(dest_dir.path()).await;

    let (task, _pause_rx) =
        MigrationTask::new(volume_id.clone(), handle.clone(), format!("https://{addr}"));
    spawn_auto_pause(task.clone());

    let authorization = migration_token(&volume_id, blocks * BLOCK);
    let sender = MigrationSender::new(src_backend, volume_id, handle)
        .with_migration_authorization(authorization)
        .with_tls(sender_tls(&ca))
        .with_task(task.clone());
    let endpoint = format!("https://{addr}");
    tokio::time::timeout(MIGRATION_TIMEOUT, sender.start_migration(endpoint))
        .await
        .expect("migration must not hang (round-ack deadlock regression?)")
        .expect("migration must succeed");

    let state = task.state.read().await;
    assert_eq!(state.phase, MigrationPhase::Completed);
    // #582 residue: no pause wait is pending on a completed task.
    assert!(
        !state.needs_vm_pause,
        "a Completed task must not report a pending VM pause"
    );
    assert!(
        state.convergence_round >= 1,
        "migration must converge through at least one dirty sync round"
    );
    assert!(
        state.bytes_transferred > 0,
        "dirty rounds must transfer real bytes"
    );
    assert_finalize_digest_observed(&state);
    drop(state);

    let dest = std::fs::read(dest_dir.path().join("vol-e2e-dirty.img"))
        .expect("receiving volume file must exist");
    assert_eq!(
        dest, expected,
        "destination must contain the bulk image plus both pre-seeded dirty writes"
    );
}

/// **Concurrent source write fails fast** (issue #394, Option A): a
/// write to the source backing file during bulk copy — the write path
/// the dirty bitmap cannot see, i.e. what a running guest produces —
/// must fail the migration at the first dirty-round boundary with the
/// distinct `source_modified_during_migration` code. The failure must
/// be *early*: the VM is never paused (`needs_vm_pause` stays false)
/// and the finalize digest is never computed (empty
/// `finalize_volume_digest`) — pre-#394 this scenario burned a full
/// transfer plus two O(volume) digest passes before failing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_source_write_fails_fast() {
    install_crypto_provider();

    let src_dir = tempfile::tempdir().unwrap();
    let dest_dir = tempfile::tempdir().unwrap();

    // 64 MiB = 16 chunks: bulk copy is long enough that the concurrent
    // writer (polling at 1 ms, writing every iteration) lands its
    // out-of-band writes inside the phase, before the round-1 canary
    // check. A missed window cannot pass silently — the migration would
    // then complete or fail at the finalize digest, and the
    // failed_precondition/token assertions below fail loudly.
    let blocks: u64 = 16;
    write_source_image(src_dir.path(), "vol.img", blocks);

    let src_backend = Arc::new(LocalFileBackend::new(src_dir.path().to_path_buf()));
    let volume_id = "vol-e2e-canary".to_string();
    let handle = open_source_volume(&src_backend, &volume_id, "vol.img", blocks * BLOCK).await;

    let (addr, ca) = spawn_receiver(dest_dir.path()).await;

    let (task, _pause_rx) =
        MigrationTask::new(volume_id.clone(), handle.clone(), format!("https://{addr}"));
    spawn_auto_pause(task.clone());

    // Concurrent out-of-band writer: once the bulk phase is observed,
    // write the source backing file directly — a plain in-place write
    // through a descriptor stord never handed out, exactly the shape of
    // a hypervisor (guest) write and invisible to the dirty bitmap.
    let src_path = src_dir.path().join("vol.img");
    let writer_task = task.clone();
    let writer = tokio::spawn(async move {
        loop {
            let phase = writer_task.state.read().await.phase;
            match phase {
                MigrationPhase::Pending => {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                MigrationPhase::BulkCopy => {
                    tokio::task::spawn_blocking({
                        let path = src_path.clone();
                        move || {
                            use std::io::{Seek, SeekFrom, Write};
                            let mut f =
                                std::fs::OpenOptions::new().write(true).open(&path).unwrap();
                            f.seek(SeekFrom::Start(1024)).unwrap();
                            f.write_all(&[0x77u8; 512]).unwrap();
                            f.sync_all().unwrap();
                        }
                    })
                    .await
                    .unwrap();
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                _ => break, // DirtySync or later: stop writing
            }
        }
    });

    let authorization = migration_token(&volume_id, blocks * BLOCK);
    let sender = MigrationSender::new(src_backend, volume_id, handle)
        .with_migration_authorization(authorization)
        .with_tls(sender_tls(&ca))
        .with_task(task.clone());
    let endpoint = format!("https://{addr}");
    let result = tokio::time::timeout(MIGRATION_TIMEOUT, sender.start_migration(endpoint))
        .await
        .expect("migration must not hang (canary check regression?)");
    writer.await.expect("concurrent writer must finish");

    let status = result
        .expect_err("a concurrent source write must fail the migration, not converge or complete");
    assert_eq!(
        status.code(),
        tonic::Code::FailedPrecondition,
        "canary failure must be failed_precondition: {}",
        status.message()
    );
    assert!(
        status
            .message()
            .contains("source_modified_during_migration"),
        "error must carry the source_modified_during_migration token: {}",
        status.message()
    );

    let state = task.state.read().await;
    assert_eq!(state.phase, MigrationPhase::Failed);
    assert!(
        state
            .error_message
            .contains("source_modified_during_migration"),
        "task error_message must carry the token: {}",
        state.error_message
    );
    assert!(
        !state.needs_vm_pause,
        "the VM must never be paused for a canary failure (no resume needed)"
    );
    assert!(
        state.finalize_volume_digest.is_empty(),
        "canary failure must precede the finalize digest (fail fast, not fail late)"
    );
}

/// **Residual window stays fail-closed** (issue #394, Option A +
/// #392 layering): the canary's last check is the pre-pause gate, so a
/// source write landing while the sender waits for the VM pause is
/// beyond the canary's reach by design. The post-pause whole-volume
/// digest must still catch it: the destination (which holds the
/// pre-write bytes) diverges from the source, the FinalizeAck reports
/// `verified: false`, and the migration fails with `data_loss` —
/// loudly, never `Completed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn source_write_during_pause_window_fails_at_finalize_digest() {
    install_crypto_provider();

    let src_dir = tempfile::tempdir().unwrap();
    let dest_dir = tempfile::tempdir().unwrap();

    // 8 MiB = 2 chunks, quiescent through bulk copy and the (empty)
    // dirty rounds so the sender reaches the pause wait cleanly.
    let expected = write_source_image(src_dir.path(), "vol.img", 2);

    let src_backend = Arc::new(LocalFileBackend::new(src_dir.path().to_path_buf()));
    let volume_id = "vol-e2e-pause-write".to_string();
    let handle = open_source_volume(&src_backend, &volume_id, "vol.img", 2 * BLOCK).await;

    let (addr, ca) = spawn_receiver(dest_dir.path()).await;

    let (task, mut pause_rx) =
        MigrationTask::new(volume_id.clone(), handle.clone(), format!("https://{addr}"));

    let authorization = migration_token(&volume_id, 2 * BLOCK);
    let sender = MigrationSender::new(src_backend, volume_id, handle)
        .with_migration_authorization(authorization)
        .with_tls(sender_tls(&ca))
        .with_task(task.clone());
    let endpoint = format!("https://{addr}");
    let migration = tokio::spawn(async move {
        tokio::time::timeout(MIGRATION_TIMEOUT, sender.start_migration(endpoint))
            .await
            .expect("migration must not hang")
    });

    // Wait until the sender is provably blocked in the pause wait (the
    // pre-pause gate has passed — this is the window the canary cannot
    // cover), then write the source out-of-band, then release the pause.
    let deadline = tokio::time::Instant::now() + MIGRATION_TIMEOUT;
    loop {
        {
            let state = task.state.read().await;
            assert!(
                state.phase != MigrationPhase::Completed && state.phase != MigrationPhase::Failed,
                "sender must still be waiting for the pause when we write: {:?}",
                state.error_message
            );
            if state.needs_vm_pause {
                break;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "sender never reached the pause wait"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    // The out-of-band write: in-place, no truncate — a guest-shaped
    // write the transferred chunks do not carry.
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(src_dir.path().join("vol.img"))
            .unwrap();
        f.seek(SeekFrom::Start(1024)).unwrap();
        f.write_all(&[0x99u8; 512]).unwrap();
        f.sync_all().unwrap();
    }
    drop(expected); // (the destination is asserted to *diverge*, not match)

    task.pause_tx.send(true).unwrap();
    assert!(*pause_rx.borrow_and_update());

    let result = migration
        .await
        .expect("migration task must not panic (canary regression?)");
    let status = result.expect_err(
        "a source write inside the pause window must fail the finalize digest verification",
    );
    assert_eq!(
        status.code(),
        tonic::Code::DataLoss,
        "finalize digest divergence is a data-integrity failure: {}",
        status.message()
    );
    assert!(
        status.message().contains("finalization failed"),
        "error must name the finalization failure: {}",
        status.message()
    );

    let state = task.state.read().await;
    assert_eq!(state.phase, MigrationPhase::Failed);
    assert!(
        state.error_message.contains("finalization failed"),
        "task error_message must name the failure: {}",
        state.error_message
    );
    // #582 residue: this task DID request the VM pause (the flag was
    // set at the final-sync gate) — a terminal task must not keep
    // reporting a pending pause.
    assert!(
        !state.needs_vm_pause,
        "a Failed task must not report a pending VM pause"
    );
    // Unlike the canary's fail-fast path, this failure is *late* by
    // design: the digest was computed (the #392 gate did the catching).
    assert_finalize_digest_observed(&state);
}

/// **Pause handshake**: with the task registered in a `MigrationTaskTable`
/// (the stord handler path), the sender must reach `PausedFinalSync`, set
/// `needs_vm_pause`, and stay blocked until the pause is signaled — then
/// complete and match the source.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_handshake_releases_final_sync() {
    install_crypto_provider();

    let src_dir = tempfile::tempdir().unwrap();
    let dest_dir = tempfile::tempdir().unwrap();

    let expected = write_source_image(src_dir.path(), "vol.img", 2);

    let src_backend = Arc::new(LocalFileBackend::new(src_dir.path().to_path_buf()));
    let volume_id = "vol-e2e-pause".to_string();
    let handle = open_source_volume(&src_backend, &volume_id, "vol.img", 2 * BLOCK).await;

    let (addr, ca) = spawn_receiver(dest_dir.path()).await;

    // Register the task in the table exactly like `TriggerDiskMigration`
    // does, and drive the sender from the table entry.
    let table = MigrationTaskTable::new();
    let (task, _pause_rx) =
        MigrationTask::new(volume_id.clone(), handle.clone(), format!("https://{addr}"));
    table.insert("mig-e2e-pause".to_string(), task.clone());
    let task = table.get("mig-e2e-pause").unwrap();

    let authorization = migration_token(&volume_id, 2 * BLOCK);
    let sender = MigrationSender::new(src_backend, volume_id, handle)
        .with_migration_authorization(authorization)
        .with_tls(sender_tls(&ca))
        .with_task(task.clone());
    let endpoint = format!("https://{addr}");
    let migration = tokio::spawn(async move {
        tokio::time::timeout(MIGRATION_TIMEOUT, sender.start_migration(endpoint))
            .await
            .expect("migration must not hang")
            .expect("migration must succeed");
    });

    // Wait for the pause request (the sender is about to send FinalSync).
    let deadline = tokio::time::Instant::now() + MIGRATION_TIMEOUT;
    loop {
        let state = task.state.read().await;
        if state.needs_vm_pause {
            assert_eq!(
                state.phase,
                MigrationPhase::PausedFinalSync,
                "needs_vm_pause must be observed in PausedFinalSync"
            );
            break;
        }
        assert_ne!(
            state.phase,
            MigrationPhase::Completed,
            "sender must not complete before the VM pause is signaled"
        );
        assert_ne!(
            state.phase,
            MigrationPhase::Failed,
            "migration failed while waiting for pause: {}",
            state.error_message
        );
        drop(state);
        assert!(
            tokio::time::Instant::now() < deadline,
            "sender never requested the VM pause (stuck in bulk/dirty sync?)"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    // The sender must genuinely be blocked on the pause: give it a wide
    // window to (incorrectly) proceed on its own.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !migration.is_finished(),
        "sender must stay blocked in PausedFinalSync until the pause is signaled"
    );
    assert_eq!(
        task.state.read().await.phase,
        MigrationPhase::PausedFinalSync
    );

    // Simulate `ResumeDiskMigration{vm_paused: true}`.
    task.pause_tx.send(true).unwrap();

    tokio::time::timeout(MIGRATION_TIMEOUT, migration)
        .await
        .expect("migration future must not hang")
        .expect("migration task must not panic");
    let state = task.state.read().await;
    assert_eq!(state.phase, MigrationPhase::Completed);
    // #582 residue: no pause wait is pending on a completed task.
    assert!(
        !state.needs_vm_pause,
        "a Completed task must not report a pending VM pause"
    );
    assert_finalize_digest_observed(&state);
    drop(state);

    let dest = std::fs::read(dest_dir.path().join("vol-e2e-pause.img"))
        .expect("receiving volume file must exist");
    assert_eq!(dest, expected, "destination must match the source bytes");
}

/// **Pause-first mode** (issue #394, Option C): with the opt-in
/// stop-the-world mode, the pause request must arrive BEFORE any source
/// byte is read — observed as `PausedPreCopy` with zero bytes transferred
/// — and the sender must stay blocked there until the pause is signaled.
/// After the pause, the whole transfer runs against the quiesced source
/// and completes with the finalize digest verified: correct by
/// construction, the digest as proof rather than as a late loss detector.
///
/// A source write landing while the sender is still blocked in the
/// pre-copy pause is pre-baseline (the canary baseline is sampled after
/// the pause handshake), so it is legitimately part of the transferred
/// image — the test pins that too: the destination must contain it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_first_pauses_before_bulk_copy() {
    install_crypto_provider();

    let src_dir = tempfile::tempdir().unwrap();
    let dest_dir = tempfile::tempdir().unwrap();

    let mut expected = write_source_image(src_dir.path(), "vol.img", 2);

    let src_backend = Arc::new(LocalFileBackend::new(src_dir.path().to_path_buf()));
    let volume_id = "vol-e2e-pause-first".to_string();
    let handle = open_source_volume(&src_backend, &volume_id, "vol.img", 2 * BLOCK).await;

    let (addr, ca) = spawn_receiver(dest_dir.path()).await;

    let table = MigrationTaskTable::new();
    let (task, _pause_rx) =
        MigrationTask::new(volume_id.clone(), handle.clone(), format!("https://{addr}"));
    table.insert("mig-e2e-pause-first".to_string(), task.clone());
    let task = table.get("mig-e2e-pause-first").unwrap();

    let authorization = migration_token(&volume_id, 2 * BLOCK);
    let sender = MigrationSender::new(src_backend, volume_id, handle)
        .with_migration_authorization(authorization)
        .with_tls(sender_tls(&ca))
        .with_task(task.clone())
        .with_pause_first();
    let endpoint = format!("https://{addr}");
    let migration = tokio::spawn(async move {
        tokio::time::timeout(MIGRATION_TIMEOUT, sender.start_migration(endpoint))
            .await
            .expect("migration must not hang")
            .expect("migration must succeed");
    });

    // Wait for the pause request. The discriminator this test exists
    // for: it must arrive in PausedPreCopy with NOTHING transferred.
    let deadline = tokio::time::Instant::now() + MIGRATION_TIMEOUT;
    loop {
        let state = task.state.read().await;
        if state.needs_vm_pause {
            assert_eq!(
                state.phase,
                MigrationPhase::PausedPreCopy,
                "pause-first must request the pause in PausedPreCopy"
            );
            assert_eq!(
                state.bytes_transferred, 0,
                "pause-first must pause BEFORE any source byte is read"
            );
            assert_eq!(
                state.convergence_round, 0,
                "pause-first must pause before any dirty round"
            );
            break;
        }
        assert_ne!(
            state.phase,
            MigrationPhase::Completed,
            "sender must not complete before the VM pause is signaled"
        );
        assert_ne!(
            state.phase,
            MigrationPhase::Failed,
            "migration failed while waiting for pause: {}",
            state.error_message
        );
        drop(state);
        assert!(
            tokio::time::Instant::now() < deadline,
            "sender never requested the pre-copy VM pause"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    // The sender must genuinely be blocked on the pre-copy pause: give
    // it a wide window to (incorrectly) proceed on its own, then assert
    // on the state itself, not just the future's liveness — a slow
    // misbehaving sender could otherwise slip past the window.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !migration.is_finished(),
        "sender must stay blocked in PausedPreCopy until the pause is signaled"
    );
    {
        let state = task.state.read().await;
        assert_eq!(state.phase, MigrationPhase::PausedPreCopy);
        assert_eq!(
            state.bytes_transferred, 0,
            "no byte may move while blocked in PausedPreCopy"
        );
    }

    // While the "VM" is paused, a host-level write lands before the
    // canary baseline (sampled after the handshake) — pre-baseline, so
    // legitimately part of the transferred image. Mirror it into the
    // expected bytes.
    let mid_pause = vec![0x5Au8; 64 * 1024];
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(src_dir.path().join("vol.img"))
            .unwrap();
        f.seek(SeekFrom::Start(BLOCK)).unwrap();
        f.write_all(&mid_pause).unwrap();
    }
    let offset = BLOCK as usize;
    expected[offset..offset + mid_pause.len()].copy_from_slice(&mid_pause);

    // Simulate `ResumeDiskMigration{vm_paused: true}`.
    task.pause_tx.send(true).unwrap();

    tokio::time::timeout(MIGRATION_TIMEOUT, migration)
        .await
        .expect("migration future must not hang")
        .expect("migration task must not panic");
    let state = task.state.read().await;
    assert_eq!(state.phase, MigrationPhase::Completed);
    // #582 residue: no pause wait is pending on a completed task.
    assert!(
        !state.needs_vm_pause,
        "a Completed task must not report a pending VM pause"
    );
    assert_finalize_digest_observed(&state);
    drop(state);

    let dest = std::fs::read(dest_dir.path().join("vol-e2e-pause-first.img"))
        .expect("receiving volume file must exist");
    assert_eq!(
        dest, expected,
        "destination must match the quiesced source bytes exactly"
    );
}

/// **Pause-signal latching for staggered senders** (issue #394 Option C
/// review finding): the agent pauses and resumes ALL of a VM's volumes
/// when the FIRST one requests the pause — a sibling whose sender has
/// not yet reached its pause gate (still completing the mTLS handshake)
/// must not turn that resume into an error, and must observe the pause
/// when it later reaches its gate. Before the fix, the trigger handler
/// dropped the task's pause receiver when the spawned sender started, so
/// a `ResumeDiskMigration` arriving while the sender was still
/// connecting (not yet subscribed at its gate) hit a closed watch
/// channel: the send failed, the resume RPC errored, and the agent
/// aborted the whole migration — near-deterministic for multi-volume
/// pause-first, where the resume fires while siblings are still
/// connecting.
///
/// Driven through the REAL `TriggerDiskMigration` handler (the
/// production construction site of the task and its pause channel),
/// with a destination that accepts the TCP connection but never
/// completes the TLS handshake: the spawned sender is deterministically
/// stuck pre-handshake — it cannot have subscribed at its pause gate —
/// so the resume that follows must latch on the open channel rather
/// than fail on a closed one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_signal_latches_for_senders_not_yet_at_their_gate() {
    install_crypto_provider();

    let src_dir = tempfile::tempdir().unwrap();

    write_source_image(src_dir.path(), "vol-latch.img", 2);

    let backend = Arc::new(LocalFileBackend::new(src_dir.path().to_path_buf()));

    // A silent destination: accepts and holds connections, never
    // answers the TLS handshake. The sender hangs in connect — alive,
    // pre-handshake, and provably not subscribed at its pause gate.
    let silent = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let silent_addr = silent.local_addr().unwrap();
    let hold = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((sock, _)) = silent.accept().await {
            held.push(sock); // never read, never written, never closed
        }
    });

    let svc = chv_stord_core::handlers::StorageServiceImpl::new(
        backend.clone(),
        Arc::new(chv_stord_core::session::SessionTable::new()),
        Arc::new(chv_observability::Metrics::new()),
        src_dir.path().to_path_buf(),
        vec!["local".to_string()],
        vec![],
        vec![],
        vec![], // migration_dest_allowlist: empty = allow all
        // The TLS identity is required by the sender but never validated
        // against the silent peer (the handshake never completes).
        Some(sender_tls(&test_ca("silent-dest"))),
    );

    let handle = open_source_volume(&backend, "vol-latch", "vol-latch.img", 2 * BLOCK).await;
    svc.sessions().upsert(chv_stord_core::session::Session {
        volume_id: "vol-latch".to_string(),
        vm_id: None,
        attachment_handle: handle.clone(),
        export_kind: "raw".to_string(),
        export_path: src_dir
            .path()
            .join("vol-latch.img")
            .to_string_lossy()
            .to_string(),
        runtime_status: "open".to_string(),
    });

    // Trigger through the REAL handler, pause-first.
    let resp = chv_stord_api::chv_stord_api::storage_service_server::StorageService::trigger_disk_migration(
        &svc,
        Request::new(chv_stord_api::chv_stord_api::TriggerDiskMigrationRequest {
            meta: None,
            volume_id: "vol-latch".to_string(),
            attachment_handle: handle,
            dest_endpoint: format!("https://{silent_addr}"),
            pause_first: true,
            migration_authorization: migration_token("vol-latch", 2 * BLOCK),
        }),
    )
    .await
    .expect("trigger must be served");
    let inner = resp.into_inner();
    // #582 T=0 skew guard: the trigger response must echo the mode the
    // sender will actually run.
    assert!(
        inner.pause_first,
        "a pause-first trigger must echo the mode back"
    );
    assert_eq!(
        inner.result.as_ref().map(|r| r.status.as_str()),
        Some("OK"),
        "trigger must succeed"
    );
    let migration_id = inner.migration_id;

    // Let the spawned sender start and settle into the (never-completing)
    // TLS handshake — past the point where the pre-fix code had already
    // dropped the task's pause receiver.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // The resume the agent's poll would produce for a sibling volume:
    // the sender is stuck pre-handshake, nowhere near its pause gate.
    // It must latch, never error.
    let resume = chv_stord_api::chv_stord_api::storage_service_server::StorageService::resume_disk_migration(
        &svc,
        Request::new(chv_stord_api::chv_stord_api::ResumeDiskMigrationRequest {
            migration_id: migration_id.clone(),
            vm_paused: true,
        }),
    )
    .await
    .expect("resume must be served");
    let result = resume.into_inner().result.expect("result present");
    assert_eq!(
        result.status, "OK",
        "resume while the sender is not yet at its pause gate must latch, not fail: {}",
        result.human_summary
    );

    // And the latch must be observable: a sender reaching its gate now
    // would see the pause already signaled. The task is still alive and
    // pre-handshake (the silent destination never lets it progress) —
    // pinned by the status staying non-terminal and the pause flag set.
    let resp =
        chv_stord_api::chv_stord_api::storage_service_server::StorageService::get_disk_migration_status(
            &svc,
            Request::new(chv_stord_api::chv_stord_api::GetDiskMigrationStatusRequest {
                migration_id,
            }),
        )
        .await
        .expect("status must be served")
        .into_inner();
    assert_ne!(
        resp.phase,
        chv_stord_api::chv_stord_api::get_disk_migration_status_response::Phase::Failed as i32,
        "the latched resume must not fail the migration: {}",
        resp.error_message
    );

    hold.abort();
}

/// **Resume after completion is a benign no-op** (#582 review residue):
/// the pause channel's receiver lives exactly as long as the spawned
/// sender future, so once a migration completes the channel is closed
/// and a `ResumeDiskMigration` would hit a failed watch send. Before
/// the fix that errored the resume RPC — and a caller doing
/// resume-all (the agent's shape) would have treated a succeeded
/// migration as failed. A resume racing completion must answer OK.
///
/// Driven through the REAL handlers end-to-end: trigger pause-first,
/// latch an early resume (the sibling-volume shape), drive to
/// `Completed` via `GetDiskMigrationStatus`, then resume again — the
/// send provably fails on the closed channel (the sender future has
/// exited), and the handler must answer OK, not `Internal`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_after_completion_is_a_benign_noop() {
    install_crypto_provider();

    let src_dir = tempfile::tempdir().unwrap();
    let dest_dir = tempfile::tempdir().unwrap();

    let expected = write_source_image(src_dir.path(), "vol-resume.img", 2);

    let backend = Arc::new(LocalFileBackend::new(src_dir.path().to_path_buf()));
    let (addr, ca) = spawn_receiver(dest_dir.path()).await;

    let svc = chv_stord_core::handlers::StorageServiceImpl::new(
        backend.clone(),
        Arc::new(chv_stord_core::session::SessionTable::new()),
        Arc::new(chv_observability::Metrics::new()),
        src_dir.path().to_path_buf(),
        vec!["local".to_string()],
        vec![],
        vec![],
        vec![], // migration_dest_allowlist: empty = allow all
        Some(sender_tls(&ca)),
    );

    let handle = open_source_volume(&backend, "vol-resume", "vol-resume.img", 2 * BLOCK).await;
    svc.sessions().upsert(chv_stord_core::session::Session {
        volume_id: "vol-resume".to_string(),
        vm_id: None,
        attachment_handle: handle.clone(),
        export_kind: "raw".to_string(),
        export_path: src_dir
            .path()
            .join("vol-resume.img")
            .to_string_lossy()
            .to_string(),
        runtime_status: "open".to_string(),
    });

    // Trigger through the REAL handler, pause-first, then latch an
    // early resume (the sender has not reached its gate — the shape
    // the agent's resume-all produces for sibling volumes).
    let resp = chv_stord_api::chv_stord_api::storage_service_server::StorageService::trigger_disk_migration(
        &svc,
        Request::new(chv_stord_api::chv_stord_api::TriggerDiskMigrationRequest {
            meta: None,
            volume_id: "vol-resume".to_string(),
            attachment_handle: handle,
            dest_endpoint: format!("https://{addr}"),
            pause_first: true,
            migration_authorization: migration_token("vol-resume", 2 * BLOCK),
        }),
    )
    .await
    .expect("trigger must be served")
    .into_inner();
    // #582 T=0 skew guard: the trigger response must echo the mode the
    // sender will actually run.
    assert!(
        resp.pause_first,
        "a pause-first trigger must echo the mode back"
    );
    assert_eq!(
        resp.result.as_ref().map(|r| r.status.as_str()),
        Some("OK"),
        "trigger must succeed"
    );
    let migration_id = resp.migration_id;

    let resume = chv_stord_api::chv_stord_api::storage_service_server::StorageService::resume_disk_migration(
        &svc,
        Request::new(chv_stord_api::chv_stord_api::ResumeDiskMigrationRequest {
            migration_id: migration_id.clone(),
            vm_paused: true,
        }),
    )
    .await
    .expect("early resume must be served");
    assert_eq!(
        resume.into_inner().result.expect("result present").status,
        "OK",
        "the early resume must latch"
    );

    // Drive to completion.
    let deadline = tokio::time::Instant::now() + MIGRATION_TIMEOUT;
    loop {
        let status = chv_stord_api::chv_stord_api::storage_service_server::StorageService::get_disk_migration_status(
            &svc,
            Request::new(chv_stord_api::chv_stord_api::GetDiskMigrationStatusRequest {
                migration_id: migration_id.clone(),
            }),
        )
        .await
        .expect("status must be served")
        .into_inner();
        if status.phase
            == chv_stord_api::chv_stord_api::get_disk_migration_status_response::Phase::Completed
                as i32
        {
            assert!(
                !status.needs_vm_pause,
                "the completed task must not report a pending VM pause"
            );
            break;
        }
        assert_ne!(
            status.phase,
            chv_stord_api::chv_stord_api::get_disk_migration_status_response::Phase::Failed as i32,
            "migration failed: {}",
            status.error_message
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "migration never completed (stuck at phase {})",
            status.phase
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // The discriminating call: the sender future has exited, the pause
    // channel is closed, the send fails — the handler must still
    // answer OK for the terminal task.
    let late_resume = chv_stord_api::chv_stord_api::storage_service_server::StorageService::resume_disk_migration(
        &svc,
        Request::new(chv_stord_api::chv_stord_api::ResumeDiskMigrationRequest {
            migration_id: migration_id.clone(),
            vm_paused: true,
        }),
    )
    .await
    .expect("late resume must be served");
    let result = late_resume.into_inner().result.expect("result present");
    assert_eq!(
        result.status, "OK",
        "a resume racing completion must be a benign no-op, got: {}",
        result.human_summary
    );

    let dest = std::fs::read(dest_dir.path().join("vol-resume.img"))
        .expect("receiving volume file must exist");
    assert_eq!(dest, expected, "destination must match the source bytes");
}

/// **Corruption detection at finalize** (issue #392, the reason the digest
/// exists): garbage written directly into the destination file
/// mid-migration must be caught by the finalize digest comparison — the
/// migration fails with the digest-mismatch error and the task ends
/// `Failed`, never `Completed`. Before this fix the receiver replied
/// `FinalizeAck{verified:true}` unconditionally, so a corrupted or
/// truncated destination still reported success.
///
/// The corruption is injected while the sender is provably blocked in the
/// `PausedFinalSync` handshake: at that point bulk copy *and* all dirty
/// rounds have finished (nothing is re-sent after `FinalSync`), so every
/// destination write has already landed and the garbage is guaranteed to
/// still be there when the receiver computes its digest — the test is
/// deterministic rather than a race against the bulk-copy writer. The
/// migration is nonetheless mid-flight: the mTLS stream is open and the
/// finalize exchange has not happened yet.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corrupted_destination_fails_at_finalize() {
    install_crypto_provider();

    let src_dir = tempfile::tempdir().unwrap();
    let dest_dir = tempfile::tempdir().unwrap();

    // 16 MiB = 4 chunks; block 1 is pre-seeded dirty so the migration
    // genuinely converges through a dirty round before the pause point.
    let blocks: u64 = 4;
    let mut expected = write_source_image(src_dir.path(), "vol.img", blocks);

    let src_backend = Arc::new(LocalFileBackend::new(src_dir.path().to_path_buf()));
    let volume_id = "vol-e2e-corrupt".to_string();
    let handle = open_source_volume(&src_backend, &volume_id, "vol.img", blocks * BLOCK).await;

    let d0 = vec![0xD0u8; 64 * 1024];
    src_backend
        .write_block(&volume_id, &handle, BLOCK, &d0)
        .await
        .expect("pre-seed dirty write must succeed");
    expected[BLOCK as usize..BLOCK as usize + d0.len()].copy_from_slice(&d0);

    let (addr, ca) = spawn_receiver(dest_dir.path()).await;

    let (task, _pause_rx) =
        MigrationTask::new(volume_id.clone(), handle.clone(), format!("https://{addr}"));

    let authorization = migration_token(&volume_id, blocks * BLOCK);
    let sender = MigrationSender::new(src_backend, volume_id, handle)
        .with_migration_authorization(authorization)
        .with_tls(sender_tls(&ca))
        .with_task(task.clone());
    let endpoint = format!("https://{addr}");
    let migration = tokio::spawn(async move {
        tokio::time::timeout(MIGRATION_TIMEOUT, sender.start_migration(endpoint)).await
    });

    // Wait for the pause request: bulk copy and dirty rounds are complete,
    // the sender is blocked, and the destination holds the full image.
    let deadline = tokio::time::Instant::now() + MIGRATION_TIMEOUT;
    loop {
        let state = task.state.read().await;
        if state.needs_vm_pause {
            assert_eq!(state.phase, MigrationPhase::PausedFinalSync);
            break;
        }
        assert_ne!(
            state.phase,
            MigrationPhase::Failed,
            "migration failed before the pause point: {}",
            state.error_message
        );
        drop(state);
        assert!(
            tokio::time::Instant::now() < deadline,
            "sender never requested the VM pause (stuck in bulk/dirty sync?)"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(
        task.state.read().await.convergence_round >= 1,
        "migration must have run a dirty round before the corruption point"
    );

    // Corrupt the destination volume out-of-band (external to stord — the
    // threat model is anything that mutates the assembled file: bit rot,
    // truncation, another writer). The receiver's per-chunk CRC32 checks
    // all passed: this damage is invisible to every pre-existing
    // integrity mechanism and only the finalize digest can catch it.
    let corrupt_offset = 4096usize;
    let garbage: [u8; 4] = [0xDE, 0xAD, 0xBE, 0xEF];
    let dest_path = dest_dir.path().join("vol-e2e-corrupt.img");
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut dest = std::fs::OpenOptions::new()
            .write(true)
            .open(&dest_path)
            .expect("receiving volume file must exist at the pause point");
        dest.seek(SeekFrom::Start(corrupt_offset as u64))
            .expect("seek within receiving volume");
        dest.write_all(&garbage)
            .expect("out-of-band corruption write");
        dest.flush().expect("corruption must hit the disk");
    }

    // Release the sender into FinalSync/FinalizeComplete. The receiver
    // re-computes the destination digest, sees the garbage, and must
    // answer verified=false.
    task.pause_tx.send(true).unwrap();

    let result = migration
        .await
        .expect("migration task must not panic")
        .expect("migration must not hang (finalize deadlock regression?)");
    let status =
        result.expect_err("migration with a corrupted destination must FAIL, not report Completed");
    assert_eq!(
        status.code(),
        tonic::Code::DataLoss,
        "digest mismatch is a data-integrity failure: {}",
        status.message()
    );
    assert!(
        status.message().contains("digest mismatch"),
        "error must name the digest mismatch: {}",
        status.message()
    );
    assert!(
        status.message().contains("sha256:"),
        "error must name the digest algorithm (content-free): {}",
        status.message()
    );
    // The error message must stay content-free: no volume bytes leak into it.
    assert!(
        !status.message().contains("DEAD"),
        "error message must not contain volume data"
    );

    let state = task.state.read().await;
    assert_eq!(
        state.phase,
        MigrationPhase::Failed,
        "task must end Failed when the destination does not verify"
    );
    assert!(
        state.error_message.contains("digest mismatch"),
        "task error message must carry the digest-mismatch reason: {}",
        state.error_message
    );
    drop(state);

    // The failure really was the corruption: the garbage is still in the
    // destination, and the destination otherwise matches the source.
    let dest = std::fs::read(&dest_path).expect("receiving volume file must exist");
    assert_eq!(
        &dest[corrupt_offset..corrupt_offset + garbage.len()],
        &garbage,
        "corrupted bytes must still be present (the mismatch was real)"
    );
    let mut expected_corrupted = expected;
    expected_corrupted[corrupt_offset..corrupt_offset + garbage.len()].copy_from_slice(&garbage);
    assert_eq!(
        dest, expected_corrupted,
        "destination must be the source bytes plus exactly the injected corruption"
    );
}
