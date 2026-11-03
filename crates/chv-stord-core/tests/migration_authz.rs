//! Receiver-side migration authorization at the served gRPC boundary
//! (issue #663).
//!
//! mTLS proves *a* node certificate, not that the control plane asked for
//! this particular migration. These tests drive the production
//! [`StorageMigrationServiceImpl`] over an in-process tonic server and pin
//! the policy that closes the gap:
//!
//! - every missing/malformed/expired/wrong-destination/wrong-volume/
//!   wrong-size/bad-signature token is rejected **before**
//!   `create_receiving_volume` (no destination file exists afterwards),
//! - a control-plane-minted token for exactly this node/volume/size is
//!   accepted,
//! - a token is single-use (replay rejected), and
//! - the per-node concurrency/byte admission limits reject an over-limit
//!   stream before volume creation, then release capacity when the active
//!   stream closes.

use chv_common::migration_auth::MigrationTokenCodec;
use chv_stord_api::chv_stord_api::{
    migration_message, storage_migration_service_client::StorageMigrationServiceClient,
    storage_migration_service_server::StorageMigrationServiceServer, InitMigration,
    MigrationMessage,
};
use chv_stord_backends::LocalFileBackend;
use chv_stord_core::migration::auth::{MigrationLimits, MigrationReceiverAuthorization};
use chv_stord_core::migration::service::StorageMigrationServiceImpl;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::transport::{Channel, Endpoint, Server};
use tonic::{Code, Response, Status, Streaming};

const BLOCK: u64 = 4 * 1024 * 1024;
const MIGRATION_SIGNING_SECRET: &[u8] = b"authz-migration-signing-secret-0123456789abcdef";
const SOURCE_NODE_ID: &str = "stord-source";
const DESTINATION_NODE_ID: &str = "stord-destination";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn codec() -> MigrationTokenCodec {
    MigrationTokenCodec::new(MIGRATION_SIGNING_SECRET).expect("test secret is long enough")
}

fn token(volume_id: &str, size_bytes: u64) -> String {
    codec()
        .mint(
            SOURCE_NODE_ID,
            DESTINATION_NODE_ID,
            volume_id,
            size_bytes,
            Duration::from_secs(300),
        )
        .expect("token mint must succeed")
}

fn token_for(
    destination_node: &str,
    volume_id: &str,
    size_bytes: u64,
    issued_at_unix_ms: i64,
    ttl: Duration,
) -> String {
    codec()
        .mint_at(
            SOURCE_NODE_ID,
            destination_node,
            volume_id,
            size_bytes,
            issued_at_unix_ms,
            ttl,
        )
        .expect("token mint must succeed")
}

fn init(volume_id: &str, size_bytes: u64, authorization: String) -> MigrationMessage {
    MigrationMessage {
        payload: Some(migration_message::Payload::Init(InitMigration {
            volume_id: volume_id.to_string(),
            size_bytes,
            block_size: BLOCK as u32,
            format: "raw".to_string(),
            checksum_type: "crc32".to_string(),
            authorization,
        })),
    }
}

fn tamper_signature(token: &str) -> String {
    let mut parts: Vec<String> = token.split('.').map(str::to_string).collect();
    let first = parts[2].chars().next().expect("signature is non-empty");
    let replacement = if first == '0' { "1" } else { "0" };
    parts[2].replace_range(0..1, replacement);
    parts.join(".")
}

async fn start_receiver(
    dir: &Path,
    authorization: MigrationReceiverAuthorization,
    limits: MigrationLimits,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let backend = Arc::new(LocalFileBackend::new(dir.to_path_buf()));
    let service =
        StorageMigrationServiceImpl::new(backend, dir.to_path_buf(), authorization, limits);
    let handle = tokio::spawn(async move {
        Server::builder()
            .add_service(StorageMigrationServiceServer::new(service))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .expect("in-process migration server must serve");
    });
    (addr, handle)
}

async fn client(addr: SocketAddr) -> StorageMigrationServiceClient<Channel> {
    let channel = Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect_timeout(Duration::from_secs(5))
        .connect()
        .await
        .expect("in-process client must connect");
    StorageMigrationServiceClient::new(channel)
}

/// Open a `StreamBlocks` call and keep the request sender alive so the
/// stream stays open until the caller drops it.
async fn begin(
    client: &mut StorageMigrationServiceClient<Channel>,
    first: MigrationMessage,
) -> (
    Result<Response<Streaming<MigrationMessage>>, Status>,
    mpsc::Sender<MigrationMessage>,
) {
    let (tx, rx) = mpsc::channel::<MigrationMessage>(8);
    tx.send(first).await.unwrap();
    let result = tokio::time::timeout(
        REQUEST_TIMEOUT,
        client.stream_blocks(ReceiverStream::new(rx)),
    )
    .await
    .expect("receiver must answer promptly");
    (result, tx)
}

async fn expect_ready(inbound: &mut Streaming<MigrationMessage>, volume_id: &str) {
    let ready = tokio::time::timeout(REQUEST_TIMEOUT, inbound.message())
        .await
        .expect("receiver must send MigrationReady promptly")
        .expect("stream must stay healthy")
        .expect("expected MigrationReady, got end of stream");
    match ready.payload {
        Some(migration_message::Payload::Ready(ready)) => {
            assert_eq!(ready.dest_volume_id, volume_id)
        }
        other => panic!("expected MigrationReady, got {other:?}"),
    }
}

fn assert_no_volume(dir: &Path, volume_id: &str) {
    let path = dir.join(format!("{volume_id}.img"));
    assert!(
        !path.exists(),
        "rejected migration must not create the receiving volume: {}",
        path.display()
    );
}

fn required_auth() -> MigrationReceiverAuthorization {
    MigrationReceiverAuthorization::required(MIGRATION_SIGNING_SECRET, DESTINATION_NODE_ID)
        .expect("test authorization must construct")
}

/// Every negative token shape is rejected before the backend provisions a
/// receiving volume.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_tokens_are_rejected_before_volume_creation() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _server) =
        start_receiver(dir.path(), required_auth(), MigrationLimits::default()).await;
    let mut client = client(addr).await;
    let now = now_unix_ms();

    let cases: Vec<(&str, String, Code)> = vec![
        ("missing", String::new(), Code::Unauthenticated),
        ("garbage", "not-a-token".to_string(), Code::PermissionDenied),
        (
            "malformed-hex",
            "v1.not-hex.not-hex".to_string(),
            Code::PermissionDenied,
        ),
        (
            "bad-signature",
            tamper_signature(&token("vol-authz-bad-sig", BLOCK)),
            Code::PermissionDenied,
        ),
        (
            "expired",
            token_for(
                DESTINATION_NODE_ID,
                "vol-authz-expired",
                BLOCK,
                now - 600_000,
                Duration::from_secs(60),
            ),
            Code::PermissionDenied,
        ),
        (
            "not-yet-valid",
            token_for(
                DESTINATION_NODE_ID,
                "vol-authz-future",
                BLOCK,
                now + 120_000,
                Duration::from_secs(60),
            ),
            Code::PermissionDenied,
        ),
        (
            "wrong-destination",
            token_for(
                "stord-somewhere-else",
                "vol-authz-wrong-dest",
                BLOCK,
                now,
                Duration::from_secs(300),
            ),
            Code::PermissionDenied,
        ),
        (
            "wrong-volume",
            token("vol-authz-other", BLOCK),
            Code::PermissionDenied,
        ),
        (
            "wrong-size",
            token("vol-authz-wrong-size", BLOCK * 2),
            Code::PermissionDenied,
        ),
    ];

    for (name, authorization, expected_code) in cases {
        let volume_id = match name {
            "missing" => "vol-authz-missing",
            "garbage" | "malformed-hex" => "vol-authz-malformed",
            "bad-signature" => "vol-authz-bad-sig",
            "expired" => "vol-authz-expired",
            "not-yet-valid" => "vol-authz-future",
            "wrong-destination" => "vol-authz-wrong-dest",
            "wrong-volume" => "vol-authz-wrong-volume",
            "wrong-size" => "vol-authz-wrong-size",
            other => panic!("unhandled case {other}"),
        };
        let (result, _tx) = begin(&mut client, init(volume_id, BLOCK, authorization)).await;
        let status = result.expect_err(&format!("{name} token must be rejected"));
        assert_eq!(
            status.code(),
            expected_code,
            "{name} token must map to {expected_code:?}: {}",
            status.message()
        );
        assert_no_volume(dir.path(), volume_id);
    }
}

/// A deny-all receiver rejects even a correctly signed token: no
/// authorization material configured is fail-closed, not a free pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unconfigured_receiver_rejects_before_volume_creation() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _server) = start_receiver(
        dir.path(),
        MigrationReceiverAuthorization::DenyAll,
        MigrationLimits::default(),
    )
    .await;
    let mut client = client(addr).await;

    let (result, _tx) = begin(
        &mut client,
        init(
            "vol-authz-denyall",
            BLOCK,
            token("vol-authz-denyall", BLOCK),
        ),
    )
    .await;
    let status = result.expect_err("DenyAll must reject a valid-looking token");
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert_no_volume(dir.path(), "vol-authz-denyall");
}

/// A control-plane token naming exactly this destination/volume/size is
/// accepted and the receiver provisions the volume.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn valid_control_plane_token_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _server) =
        start_receiver(dir.path(), required_auth(), MigrationLimits::default()).await;
    let mut client = client(addr).await;

    let volume_id = "vol-authz-valid";
    let (result, tx) = begin(&mut client, init(volume_id, BLOCK, token(volume_id, BLOCK))).await;
    let response = result.expect("a valid control-plane token must be accepted");
    let mut inbound = response.into_inner();
    expect_ready(&mut inbound, volume_id).await;

    let path = dir.path().join(format!("{volume_id}.img"));
    assert!(path.exists(), "accepted migration must create the volume");
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        BLOCK,
        "receiving volume must be allocated to the authorized size"
    );

    drop(tx);
    drop(inbound);
}

/// A token is single-use: the same token replayed on a second stream is
/// rejected before it can touch the backend again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replayed_token_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _server) =
        start_receiver(dir.path(), required_auth(), MigrationLimits::default()).await;
    let mut client = client(addr).await;

    let volume_id = "vol-authz-replay";
    let authorization = token(volume_id, BLOCK);

    let (first, first_tx) = begin(&mut client, init(volume_id, BLOCK, authorization.clone())).await;
    let mut first_inbound = first.expect("first use must succeed").into_inner();
    expect_ready(&mut first_inbound, volume_id).await;
    drop(first_tx);
    drop(first_inbound);

    let (second, _tx) = begin(&mut client, init(volume_id, BLOCK, authorization)).await;
    let status = second.expect_err("a replayed token must be rejected");
    assert_eq!(status.code(), Code::PermissionDenied);
    assert!(
        status.message().contains("already been used"),
        "replay must be named in the error: {}",
        status.message()
    );
    assert!(
        dir.path().join(format!("{volume_id}.img")).exists(),
        "the first, legitimate use must have created the volume"
    );
}

/// The admission limit is checked before volume creation and released when
/// the active stream closes, so a rejected stream does not leak a slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrency_limit_rejects_then_releases_on_stream_close() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _server) = start_receiver(
        dir.path(),
        required_auth(),
        MigrationLimits {
            max_inflight: 1,
            max_inflight_bytes: 64 * 1024 * 1024,
        },
    )
    .await;
    let mut client = client(addr).await;

    let first_id = "vol-authz-limit-first";
    let (first, first_tx) = begin(&mut client, init(first_id, BLOCK, token(first_id, BLOCK))).await;
    let mut first_inbound = first.expect("first stream must be admitted").into_inner();
    expect_ready(&mut first_inbound, first_id).await;

    let second_id = "vol-authz-limit-second";
    let second_auth = token(second_id, BLOCK);
    let (second, _second_tx) =
        begin(&mut client, init(second_id, BLOCK, second_auth.clone())).await;
    let status = second.expect_err("second stream must hit the concurrency limit");
    assert_eq!(status.code(), Code::ResourceExhausted);
    assert_no_volume(dir.path(), second_id);

    // End the active stream: the RAII reservation is released by the
    // receiver task, and the same (not-yet-consumed) token is then
    // admitted. Poll to avoid racing the task's stream-close handling.
    drop(first_tx);
    drop(first_inbound);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let (retry, retry_tx) =
            begin(&mut client, init(second_id, BLOCK, second_auth.clone())).await;
        match retry {
            Ok(response) => {
                let mut inbound = response.into_inner();
                expect_ready(&mut inbound, second_id).await;
                drop(retry_tx);
                drop(inbound);
                break;
            }
            Err(status) => {
                assert_eq!(status.code(), Code::ResourceExhausted);
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "capacity was never released after the active stream closed"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}

/// The byte-budget limit is likewise enforced before volume creation:
/// a second stream whose size would exceed the total reservation is
/// rejected and creates nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn byte_limit_rejects_before_volume_creation() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _server) = start_receiver(
        dir.path(),
        required_auth(),
        MigrationLimits {
            max_inflight: 4,
            max_inflight_bytes: BLOCK + BLOCK / 2,
        },
    )
    .await;
    let mut client = client(addr).await;

    let first_id = "vol-authz-bytes-first";
    let (first, first_tx) = begin(&mut client, init(first_id, BLOCK, token(first_id, BLOCK))).await;
    let mut first_inbound = first
        .expect("first stream must fit the budget")
        .into_inner();
    expect_ready(&mut first_inbound, first_id).await;

    let second_id = "vol-authz-bytes-second";
    let (second, _second_tx) =
        begin(&mut client, init(second_id, BLOCK, token(second_id, BLOCK))).await;
    let status = second.expect_err("second stream must exceed the byte budget");
    assert_eq!(status.code(), Code::ResourceExhausted);
    assert!(
        status.message().contains("byte limit"),
        "byte-limit rejection must be named: {}",
        status.message()
    );
    assert_no_volume(dir.path(), second_id);

    drop(first_tx);
    drop(first_inbound);
}
