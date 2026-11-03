//! Regression coverage for the bounded `FinalizeAck` wait (issue #656).
//!
//! The sender used to wait for `FinalizeAck` in an unbounded loop. A
//! destination that completed the handshake and acknowledged every phase
//! but never answered `FinalizeComplete` therefore hung the migration —
//! and left the source VM paused — forever. This test drives the real
//! [`MigrationSender`] over real mTLS against an in-test peer that sends
//! `Ready` and the phase/`FinalSync` acks but deliberately never sends
//! `FinalizeAck`, with a short configured timeout. The sender must return
//! `deadline_exceeded` and mark the shared task Failed; the outer
//! `TEST_TIMEOUT` guarantees the test itself cannot hang.

use chv_common::types::{BackendLocator, DevicePolicy};
use chv_stord_api::chv_stord_api::{
    migration_message,
    storage_migration_service_server::{StorageMigrationService, StorageMigrationServiceServer},
    Ack, AckStatus, MigrationMessage, MigrationReady,
};
use chv_stord_backends::{LocalFileBackend, StorageBackend};
use chv_stord_core::migration::sender::{MigrationSender, MigrationTlsConfig};
use chv_stord_core::migration::task::{MigrationPhase, MigrationTask};
use chv_stord_core::migration::MAX_MIGRATION_MESSAGE_SIZE_BYTES;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tokio_stream::Stream;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};
use tonic::{Request, Response, Status, Streaming};

const BLOCK: u64 = 4 * 1024 * 1024;
const TEST_TIMEOUT: Duration = Duration::from_secs(30);
const FINALIZE_ACK_TIMEOUT: Duration = Duration::from_millis(400);

fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

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

/// A destination peer that speaks the protocol through `FinalSync` but
/// never answers `FinalizeComplete` with a `FinalizeAck`.
struct SilentFinalizePeer;

#[tonic::async_trait]
impl StorageMigrationService for SilentFinalizePeer {
    type StreamBlocksStream = Pin<Box<dyn Stream<Item = Result<MigrationMessage, Status>> + Send>>;

    async fn stream_blocks(
        &self,
        request: Request<Streaming<MigrationMessage>>,
    ) -> Result<Response<Self::StreamBlocksStream>, Status> {
        let mut inbound = request.into_inner();
        let (tx, rx) = mpsc::channel::<MigrationMessage>(64);
        tokio::spawn(async move {
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
                        // The scenario under test: keep the stream open and
                        // never send FinalizeAck. The sender's bounded wait
                        // must fire.
                    }
                    _ => {}
                }
            }
        });
        let stream = tokio_stream::StreamExt::map(ReceiverStream::new(rx), Ok);
        Ok(Response::new(Box::pin(stream) as Self::StreamBlocksStream))
    }
}

async fn spawn_silent_finalize_peer(ca: &TestCa) -> SocketAddr {
    let (server_cert, server_key) = issue_leaf(ca, "fake-dest", true);
    let tls = ServerTlsConfig::new()
        .identity(Identity::from_pem(server_cert, server_key))
        .client_ca_root(Certificate::from_pem(ca.cert_pem.clone()));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = StorageMigrationServiceServer::new(SilentFinalizePeer)
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finalize_ack_timeout_fails_task_and_returns_deadline_exceeded() {
    install_crypto_provider();

    let src_dir = tempfile::tempdir().unwrap();
    let image = vec![0x5Au8; BLOCK as usize];
    std::fs::write(src_dir.path().join("vol.img"), &image).unwrap();

    let src_backend = Arc::new(LocalFileBackend::new(src_dir.path().to_path_buf()));
    let volume_id = "vol-656-finalize-timeout".to_string();
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

    let ca = test_ca("chv-migration-656-timeout-ca");
    let addr = spawn_silent_finalize_peer(&ca).await;

    let (task, _pause_rx) =
        MigrationTask::new(volume_id.clone(), handle.clone(), format!("https://{addr}"));
    // Simulate ResumeDiskMigration{vm_paused:true} when the sender asks
    // for the pause, so the test reaches the finalize wait promptly.
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
        .with_task(task.clone())
        .with_finalize_ack_timeout(FINALIZE_ACK_TIMEOUT);

    let started = Instant::now();
    let result = tokio::time::timeout(
        TEST_TIMEOUT,
        sender.start_migration(format!("https://{addr}")),
    )
    .await
    .expect("migration must not hang waiting for FinalizeAck (bounded-wait regression?)");
    let elapsed = started.elapsed();

    let status = result.expect_err("a missing FinalizeAck must fail the migration");
    assert_eq!(
        status.code(),
        tonic::Code::DeadlineExceeded,
        "a missing FinalizeAck is a timeout: {}",
        status.message()
    );
    assert!(
        status.message().contains("FinalizeAck") && status.message().contains("timed out"),
        "error must name the timed-out wait: {}",
        status.message()
    );
    assert!(
        elapsed >= FINALIZE_ACK_TIMEOUT,
        "the sender returned before the configured bound elapsed: {elapsed:?}"
    );
    assert!(
        elapsed < TEST_TIMEOUT,
        "the sender did not respect the configured bound: {elapsed:?}"
    );

    let state = task.state.read().await;
    assert_eq!(
        state.phase,
        MigrationPhase::Failed,
        "a finalize timeout must mark the task Failed"
    );
    assert!(
        state.error_message.contains("FinalizeAck") && state.error_message.contains("timed out"),
        "task error must explain the timeout: {}",
        state.error_message
    );
    assert!(
        !state.needs_vm_pause,
        "a terminal task must not keep reporting a pending VM pause"
    );
}
