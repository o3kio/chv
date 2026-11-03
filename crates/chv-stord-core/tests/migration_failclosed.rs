//! Fail-closed receiver semantics of the storage-migration protocol,
//! pinned end-to-end at the served gRPC boundary (M4.6 close-out, F1).
//!
//! The close-out review found that the receiver's fail-closed paths —
//! a `BlockChunk` whose CRC32 does not match its payload, and a chunk
//! that would write past the end of the receiving volume — were
//! exercised by *no* test in the workspace, even though
//! `tests/migration_e2e.rs` claimed otherwise (that doc-comment has
//! been corrected to point here). These tests close the gap against
//! the *production* served path:
//!
//! - [`crc_mismatch_chunk_fails_closed`]: a hand-crafted
//!   `StorageMigrationService` client (the generated one, over real
//!   mTLS via [`chv_stord_core::server::serve_migration_tls`]) opens a
//!   stream, sends a valid `InitMigration`, then a `BlockChunk` whose
//!   `crc32` field does not match its data. The real receiver — the
//!   real `StorageMigrationServiceImpl` and `MigrationReceiver` over a
//!   real `LocalFileBackend` — must answer with
//!   `Ack{ACK_CRC_MISMATCH}` identifying the offending chunk and then
//!   terminate the exchange *without* ever sending a
//!   `FinalizeAck{verified:true}`: the corrupted payload must never be
//!   written to the destination.
//! - [`out_of_bounds_chunk_is_rejected`]: the same setup, but the
//!   chunk carries a *correct* CRC and an offset beyond the volume
//!   size. The receiver must reject it before the CRC check (no `Ack`
//!   at all — not even an interval or boundary ack may launder it) and
//!   terminate the exchange; nothing is written.
//! - [`crc_mismatch_ack_at_finalize_wait_fails_migration`][]: the
//!   sender-side half of the fail-closed chain (issue #397): the real
//!   [`MigrationSender`] is driven against a served in-test peer that
//!   answers `FinalizeComplete` with an in-flight
//!   `Ack{ACK_CRC_MISMATCH}` instead of a `FinalizeAck`. The sender's
//!   tolerant FinalizeAck wait must process that Ack and fail the
//!   migration with `Status::data_loss` ("CRC mismatch reported by
//!   receiver") — a boundary ack may be tolerated, a CRC failure may
//!   not.
//!
//! Together with `tests/migration_e2e.rs` (happy path and
//! finalize-digest corruption) and `tests/migration_mtls.rs` (TLS-layer
//! peer rejection), these cover the fail-closed contract of the disk
//! migration protocol end to end.

use chv_common::migration_auth::MigrationTokenCodec;
use chv_common::types::{BackendLocator, DevicePolicy};
use chv_stord_api::chv_stord_api::{
    migration_message,
    storage_migration_service_client::StorageMigrationServiceClient,
    storage_migration_service_server::{StorageMigrationService, StorageMigrationServiceServer},
    Ack, AckStatus, BlockChunk, InitMigration, MigrationMessage, MigrationReady,
};
use chv_stord_backends::{LocalFileBackend, StorageBackend};
use chv_stord_core::migration::auth::{MigrationLimits, MigrationReceiverAuthorization};
use chv_stord_core::migration::sender::{MigrationSender, MigrationTlsConfig};
use chv_stord_core::migration::service::StorageMigrationServiceImpl;
use chv_stord_core::migration::task::{MigrationPhase, MigrationTask};
use chv_stord_core::migration::tls_config::MigrationServerTls;
use chv_stord_core::migration::MAX_MIGRATION_MESSAGE_SIZE_BYTES;
use chv_stord_core::server::serve_migration_tls;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tokio_stream::Stream;
use tonic::transport::{
    Certificate, Channel, ClientTlsConfig, Endpoint, Identity, Server, ServerTlsConfig,
};
use tonic::{Request, Response, Status, Streaming};

/// Install the rustls ring crypto provider (same as `cmd/chv-stord`
/// startup). Idempotent.
fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Migration block size (must match the backends' dirty-tracking block
/// size so bitmap bits map 1:1 to migration chunks).
const BLOCK: u64 = 4 * 1024 * 1024;

/// Dedicated migration-signing secret shared by the in-test control-plane
/// minter and the receiver (issue #663).
const MIGRATION_SIGNING_SECRET: &[u8] = b"failclosed-migration-signing-secret-0123456789";

/// Node id the receiver expects in the token's destination claim.
const DESTINATION_NODE_ID: &str = "stord-receiver";

/// Mint the control-plane authorization for a hand-crafted `InitMigration`.
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
const TEST_TIMEOUT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------
// rcgen certificate material (same pattern as tests/migration_e2e.rs)
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
/// `localhost`/127.0.0.1 identities and serverAuth EKU; client leaves
/// carry clientAuth EKU.
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
/// TCP listener backed by `dest_dir` (identical to the environment in
/// `tests/migration_e2e.rs`). Returns the bound address and the test CA
/// (which also issued the receiver's server leaf, so the same CA can
/// issue the client identity).
async fn spawn_receiver(dest_dir: &Path) -> (SocketAddr, TestCa) {
    let ca = test_ca("chv-migration-failclosed-ca");
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

/// Open a generated `StorageMigrationService` client to the receiver
/// over real mTLS (client identity issued by the CA the receiver
/// trusts).
async fn receiver_client(addr: SocketAddr, ca: &TestCa) -> StorageMigrationServiceClient<Channel> {
    let (client_cert, client_key) = issue_leaf(ca, "stord-sender", false);
    let tls = ClientTlsConfig::new()
        .domain_name("localhost")
        .ca_certificate(Certificate::from_pem(ca.cert_pem.clone()))
        .identity(Identity::from_pem(client_cert, client_key));
    let channel = Endpoint::from_shared(format!("https://{addr}"))
        .unwrap()
        .tls_config(tls)
        .unwrap()
        .connect_timeout(Duration::from_secs(10))
        .connect()
        .await
        .expect("mTLS client must connect to the receiver");
    StorageMigrationServiceClient::new(channel)
}

/// A valid `InitMigration` for a `size_bytes`-byte raw volume.
fn init_migration(volume_id: &str, size_bytes: u64) -> MigrationMessage {
    MigrationMessage {
        payload: Some(migration_message::Payload::Init(InitMigration {
            volume_id: volume_id.to_string(),
            size_bytes,
            block_size: BLOCK as u32,
            format: "raw".to_string(),
            checksum_type: "crc32".to_string(),
            authorization: migration_token(volume_id, size_bytes),
        })),
    }
}

/// Read the next message from the response stream, failing fast on a
/// hang or an early stream end.
async fn next_message(inbound: &mut Streaming<MigrationMessage>) -> MigrationMessage {
    tokio::time::timeout(Duration::from_secs(10), inbound.message())
        .await
        .expect("receiver must answer (deadlock/hang regression?)")
        .expect("stream must stay healthy")
        .expect("expected a message, got end of stream")
}

/// Assert the response stream terminates without delivering any further
/// protocol message: no interval or boundary `Ack{ACK_OK}`, no
/// `Backpressure`, and — the fail-closed direction — no
/// `FinalizeAck{verified:true}` may launder a failed exchange into a
/// success.
async fn assert_stream_terminates_without_messages(inbound: &mut Streaming<MigrationMessage>) {
    match tokio::time::timeout(Duration::from_secs(10), inbound.message()).await {
        Ok(Ok(Some(msg))) => {
            panic!(
                "no further protocol message may follow the fail-closed rejection, got: {msg:?}"
            );
        }
        Ok(Ok(None)) => {} // clean end of stream — what the served receiver does (see below)
        Ok(Err(status)) => {
            // Not expected against the production service — the handler only
            // logs a receiver error (service.rs stream_blocks) and the stream
            // ends cleanly — but if a transport status ever DOES surface, it
            // must not be a success: nothing may launder the exchange into ok.
            assert_ne!(
                status.code(),
                tonic::Code::Ok,
                "terminal transport status must not be Ok after the rejection"
            );
        }
        Err(_) => panic!("stream must terminate after the rejection, it hung instead"),
    }
}

// ---------------------------------------------------------------------
// Receiver-side fail-closed semantics (served production receiver)
// ---------------------------------------------------------------------

/// **CRC mismatch fails closed** (M4.6 close-out F1): a `BlockChunk`
/// whose `crc32` field does not match its payload must be answered with
/// `Ack{ACK_CRC_MISMATCH}` identifying the offending chunk, and the
/// exchange must end there — the corrupted bytes are never written to
/// the destination and no `FinalizeAck{verified:true}` is ever sent.
///
/// The chunk is hand-crafted on purpose: the real sender computes
/// correct CRCs, so the receiver's verification branch is only
/// reachable from a peer that is buggy, hostile, or corrupted in
/// flight — exactly the threat the fail-closed path exists for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crc_mismatch_chunk_fails_closed() {
    install_crypto_provider();

    let dest_dir = tempfile::tempdir().unwrap();
    let (addr, ca) = spawn_receiver(dest_dir.path()).await;

    let mut client = receiver_client(addr, &ca).await;
    let (tx, rx) = mpsc::channel::<MigrationMessage>(8);
    // Queue InitMigration BEFORE awaiting the stream open: the served
    // handler reads the first message before returning response headers
    // (the same ordering constraint the real sender honors — see #396's
    // init-before-stream fix; awaiting first would deadlock the open).
    let volume_id = "vol-fc-crc";
    tx.send(init_migration(volume_id, BLOCK)).await.unwrap();
    let mut inbound = client
        .stream_blocks(ReceiverStream::new(rx))
        .await
        .expect("stream must open with a valid InitMigration")
        .into_inner();

    // MigrationReady: the receiver accepted the volume.
    let ready = next_message(&mut inbound).await;
    match ready.payload {
        Some(migration_message::Payload::Ready(ready)) => {
            assert_eq!(ready.dest_volume_id, volume_id);
        }
        other => panic!("expected MigrationReady, got: {other:?}"),
    }

    // A chunk whose crc32 field does not match its payload.
    let payload = vec![0xC5u8; 64 * 1024];
    let wrong_crc = crc32fast::hash(&payload).wrapping_add(1);
    let chunk = MigrationMessage {
        payload: Some(migration_message::Payload::Chunk(BlockChunk {
            offset: 0,
            data: payload,
            crc32: wrong_crc,
            is_sparse: false,
            sequence_num: 1,
        })),
    };
    tx.send(chunk).await.unwrap();
    drop(tx); // the peer sent everything it intends to send

    // The receiver must answer with the mismatch Ack, identifying the
    // offending chunk.
    let ack = next_message(&mut inbound).await;
    match ack.payload {
        Some(migration_message::Payload::Ack(ack)) => {
            assert_eq!(
                ack.status(),
                AckStatus::AckCrcMismatch,
                "a chunk with a wrong crc32 must be reported as ACK_CRC_MISMATCH, got {:?}",
                ack.status()
            );
            assert_eq!(ack.last_sequence_num, 1);
            assert_eq!(ack.last_offset, 0);
        }
        other => panic!("expected Ack{{ACK_CRC_MISMATCH}}, got: {other:?}"),
    }

    // Fail-closed: the exchange ends here.
    assert_stream_terminates_without_messages(&mut inbound).await;

    // The corrupted payload was rejected before `write_block`: the
    // pre-allocated receiving volume must still be all zeros.
    let dest = std::fs::read(dest_dir.path().join(format!("{volume_id}.img")))
        .expect("receiving volume file must exist");
    assert_eq!(dest.len(), BLOCK as usize);
    assert!(
        dest.iter().all(|&b| b == 0),
        "a CRC-mismatched chunk must never be written to the destination"
    );
}

/// **Out-of-bounds chunks are rejected** (M4.6 close-out F1): a chunk
/// whose payload is internally consistent (correct CRC) but whose
/// offset would write past the end of the receiving volume is rejected
/// before any data is written and before any Ack is issued — not even
/// an interval or boundary ack may acknowledge it. The exchange
/// terminates without a `FinalizeAck`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn out_of_bounds_chunk_is_rejected() {
    install_crypto_provider();

    let dest_dir = tempfile::tempdir().unwrap();
    let (addr, ca) = spawn_receiver(dest_dir.path()).await;

    let mut client = receiver_client(addr, &ca).await;
    let (tx, rx) = mpsc::channel::<MigrationMessage>(8);
    // Queue InitMigration BEFORE awaiting the stream open (see the CRC
    // test above — the served handler reads the first message before
    // returning response headers; awaiting first would deadlock).
    let volume_id = "vol-fc-oob";
    tx.send(init_migration(volume_id, BLOCK)).await.unwrap();
    let mut inbound = client
        .stream_blocks(ReceiverStream::new(rx))
        .await
        .expect("stream must open with a valid InitMigration")
        .into_inner();

    let ready = next_message(&mut inbound).await;
    match ready.payload {
        Some(migration_message::Payload::Ready(ready)) => {
            assert_eq!(ready.dest_volume_id, volume_id);
        }
        other => panic!("expected MigrationReady, got: {other:?}"),
    }

    // Offset exactly at the volume size: the chunk would end past it.
    // The CRC is correct, isolating the bounds check (which runs first)
    // from the CRC path.
    let payload = vec![0xB7u8; 64 * 1024];
    let chunk = MigrationMessage {
        payload: Some(migration_message::Payload::Chunk(BlockChunk {
            offset: BLOCK, // == size_bytes: out of bounds
            crc32: crc32fast::hash(&payload),
            data: payload,
            is_sparse: false,
            sequence_num: 1,
        })),
    };
    tx.send(chunk).await.unwrap();
    drop(tx);

    // Fail-closed: no Ack of any kind for the rejected chunk, and the
    // exchange terminates without a FinalizeAck.
    assert_stream_terminates_without_messages(&mut inbound).await;

    // Nothing was written; the receiving volume is still all zeros.
    let dest = std::fs::read(dest_dir.path().join(format!("{volume_id}.img")))
        .expect("receiving volume file must exist");
    assert_eq!(dest.len(), BLOCK as usize);
    assert!(
        dest.iter().all(|&b| b == 0),
        "an out-of-bounds chunk must never be written to the destination"
    );
}

// ---------------------------------------------------------------------
// Sender-side half of the chain (issue #397)
// ---------------------------------------------------------------------

/// An in-test `StorageMigrationService` stand-in for the *destination*
/// peer. It speaks the protocol exactly like the real receiver for the
/// phases a clean migration runs through (Ready on Init, the ack-window
/// flush on FinalSync), but answers `FinalizeComplete` with an
/// in-flight `Ack{ACK_CRC_MISMATCH}` instead of a `FinalizeAck` — the
/// late chunk-CRC failure the sender's tolerant FinalizeAck wait (added
/// with the #396/#397 finalize-verification work) must not swallow.
struct FinalizeWaitCrcPeer;

#[tonic::async_trait]
impl StorageMigrationService for FinalizeWaitCrcPeer {
    type StreamBlocksStream = Pin<Box<dyn Stream<Item = Result<MigrationMessage, Status>> + Send>>;

    async fn stream_blocks(
        &self,
        request: Request<Streaming<MigrationMessage>>,
    ) -> Result<Response<Self::StreamBlocksStream>, Status> {
        let mut inbound = request.into_inner();
        let (tx, rx) = mpsc::channel::<MigrationMessage>(64);
        tokio::spawn(async move {
            // Receiver-side bookkeeping the sender's drains rely on.
            let mut last_sequence_num: u32 = 0;
            let mut last_offset: u64 = 0;
            while let Ok(Some(msg)) = inbound.message().await {
                match msg.payload {
                    Some(migration_message::Payload::Init(ref init)) => {
                        let ready = MigrationMessage {
                            payload: Some(migration_message::Payload::Ready(MigrationReady {
                                dest_volume_id: init.volume_id.clone(),
                            })),
                        };
                        if tx.send(ready).await.is_err() {
                            break;
                        }
                    }
                    Some(migration_message::Payload::Chunk(ref chunk)) => {
                        last_sequence_num = chunk.sequence_num;
                        last_offset = chunk.offset;
                    }
                    Some(migration_message::Payload::FinalSync(_)) => {
                        // The real receiver flushes its ack window at the
                        // FinalSync boundary; the sender drains on it.
                        let ack = MigrationMessage {
                            payload: Some(migration_message::Payload::Ack(Ack {
                                last_offset,
                                last_sequence_num,
                                status: AckStatus::AckOk.into(),
                            })),
                        };
                        if tx.send(ack).await.is_err() {
                            break;
                        }
                    }
                    Some(migration_message::Payload::FinalizeComplete(_)) => {
                        // The scenario under test: while the sender waits
                        // for FinalizeAck, a chunk-CRC failure Ack from the
                        // bulk/dirty phase arrives instead. The sender must
                        // fail the migration, not keep waiting.
                        let ack = MigrationMessage {
                            payload: Some(migration_message::Payload::Ack(Ack {
                                last_offset,
                                last_sequence_num,
                                status: AckStatus::AckCrcMismatch.into(),
                            })),
                        };
                        let _ = tx.send(ack).await;
                        break; // no FinalizeAck: the peer failed the stream
                    }
                    _ => {}
                }
            }
        });
        let stream = tokio_stream::StreamExt::map(ReceiverStream::new(rx), Ok);
        Ok(Response::new(Box::pin(stream) as Self::StreamBlocksStream))
    }
}

/// Serve [`FinalizeWaitCrcPeer`] on an ephemeral loopback mTLS TCP
/// listener (server identity + mandatory client auth, both from `ca`,
/// mirroring the production listener's posture).
async fn spawn_finalize_wait_crc_peer(ca: &TestCa) -> SocketAddr {
    let (server_cert, server_key) = issue_leaf(ca, "fake-dest", true);
    let tls = ServerTlsConfig::new()
        .identity(Identity::from_pem(server_cert, server_key))
        .client_ca_root(Certificate::from_pem(ca.cert_pem.clone()));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = StorageMigrationServiceServer::new(FinalizeWaitCrcPeer)
        .max_decoding_message_size(MAX_MIGRATION_MESSAGE_SIZE_BYTES);
    tokio::spawn(async move {
        if let Err(e) = Server::builder()
            .tls_config(tls)
            .expect("fake peer TLS config")
            .add_service(service)
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
        {
            panic!("fake migration peer failed: {e:?}");
        }
    });
    addr
}

/// **A CRC-mismatch Ack arriving at the FinalizeAck wait still fails the
/// migration** (issue #397, M4.6 close-out F1): the sender's finalize
/// wait tolerates *boundary* acks still in flight, but an
/// `Ack{ACK_CRC_MISMATCH}` is an integrity failure and must surface as
/// `Status::data_loss` — never as a hang, and never as `Completed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crc_mismatch_ack_at_finalize_wait_fails_migration() {
    install_crypto_provider();

    let src_dir = tempfile::tempdir().unwrap();

    // 4 MiB = 1 chunk of non-zero data (avoids the sparse path).
    let image = vec![0x5Au8; BLOCK as usize];
    std::fs::write(src_dir.path().join("vol.img"), &image).unwrap();

    let src_backend = Arc::new(LocalFileBackend::new(src_dir.path().to_path_buf()));
    let volume_id = "vol-fc-finalize-crc".to_string();
    let export = src_backend
        .open(
            &volume_id,
            &BackendLocator {
                backend_class: "local".to_string(),
                locator: "vol.img".to_string(),
                options: [("size_bytes".to_string(), BLOCK.to_string())]
                    .into_iter()
                    .collect(),
            },
            &DevicePolicy::default(),
        )
        .await
        .expect("source volume must open");
    src_backend
        .enable_dirty_tracking(&volume_id, &export.attachment_handle, BLOCK)
        .await
        .expect("dirty tracking must be enabled");
    let handle = export.attachment_handle;

    let ca = test_ca("chv-migration-finalize-crc-ca");
    let addr = spawn_finalize_wait_crc_peer(&ca).await;

    let (task, _pause_rx) =
        MigrationTask::new(volume_id.clone(), handle.clone(), format!("https://{addr}"));
    // Simulate ResumeDiskMigration{vm_paused:true} as soon as the sender
    // requests the pause (this test is about the finalize wait, not the
    // handshake).
    tokio::spawn({
        let task = task.clone();
        async move {
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
                    return;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
    });

    let (client_cert, client_key) = issue_leaf(&ca, "stord-sender", false);
    let sender = MigrationSender::new(src_backend, volume_id, handle)
        .with_tls(MigrationTlsConfig {
            cert_pem: client_cert,
            key_pem: client_key,
            ca_pem: ca.cert_pem.clone(),
            dest_domain: "localhost".to_string(),
        })
        .with_task(task.clone());

    let endpoint = format!("https://{addr}");
    let result = tokio::time::timeout(TEST_TIMEOUT, sender.start_migration(endpoint))
        .await
        .expect("migration must not hang waiting for FinalizeAck (tolerant-wait regression?)");

    let status =
        result.expect_err("a CRC-mismatch Ack at the FinalizeAck wait must FAIL the migration");
    assert_eq!(
        status.code(),
        tonic::Code::DataLoss,
        "a receiver-reported CRC mismatch is a data-integrity failure: {}",
        status.message()
    );
    assert!(
        status.message().contains("CRC mismatch"),
        "error must name the CRC mismatch: {}",
        status.message()
    );
}
