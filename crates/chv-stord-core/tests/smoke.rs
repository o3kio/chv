use chv_observability::Metrics;
use chv_stord_api::chv_stord_api::{
    storage_service_client::StorageServiceClient, AttachVolumeToVmRequest, BackendLocator,
    CloseVolumeRequest, DetachVolumeFromVmRequest, DevicePolicy, ListVolumeSessionsRequest,
    OpenVolumeRequest, PrepareCloneRequest, PrepareSnapshotRequest, ResizeVolumeRequest,
    SetDevicePolicyRequest, VolumeHealthRequest,
};
use chv_stord_backends::LocalFileBackend;
use chv_stord_core::migration::auth::{MigrationLimits, MigrationReceiverAuthorization};
use chv_stord_core::store::SessionStore;
use chv_stord_core::StorageServer;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::UnixStream;
use tonic::transport::{Endpoint, Uri};
use tower::service_fn;

async fn make_client(socket: PathBuf) -> StorageServiceClient<tonic::transport::Channel> {
    let channel = Endpoint::try_from("http://[::]:50051")
        .unwrap()
        .connect_with_connector(service_fn(move |_: Uri| {
            let s = socket.clone();
            async move {
                let stream = UnixStream::connect(s).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::tokio::TokioIo::new(stream))
            }
        }))
        .await
        .unwrap();
    StorageServiceClient::new(channel)
}

async fn setup_server() -> (
    tempfile::TempDir,
    PathBuf,
    StorageServiceClient<tonic::transport::Channel>,
) {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("stord.sock");

    let backend = LocalFileBackend::new(dir.path().to_path_buf());
    let server = StorageServer::new(
        backend,
        dir.path().to_path_buf(),
        Metrics::new(),
        vec!["local".to_string()],
        vec![],
        vec![],
        vec![],
        None,
        None,
        MigrationReceiverAuthorization::DenyAll,
        MigrationLimits::default(),
        None,
    );

    let socket_clone = socket.clone();
    tokio::spawn(async move {
        server.serve(&socket_clone, None).await.ok();
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    let client = make_client(socket.clone()).await;
    (dir, socket, client)
}

#[tokio::test]
async fn open_close_health_list_smoke() {
    let (_dir, _socket, mut client) = setup_server().await;

    // OpenVolume
    let open_req = OpenVolumeRequest {
        meta: None,
        volume_id: "vol-1".to_string(),
        backend: Some(BackendLocator {
            backend_class: "local".to_string(),
            locator: "vol-1.img".to_string(),
            options: Default::default(),
        }),
        policy: None,
    };

    let open_resp = client
        .open_volume(open_req.clone())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(open_resp.volume_id, "vol-1");
    assert_eq!(open_resp.export_kind, "raw");
    let handle = open_resp.attachment_handle;

    // Idempotent open returns same handle
    let open_resp2 = client.open_volume(open_req).await.unwrap().into_inner();
    assert_eq!(open_resp2.attachment_handle, handle);

    // GetVolumeHealth
    let health_resp = client
        .get_volume_health(VolumeHealthRequest {
            volume_id: "vol-1".to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(health_resp.volume_id, "vol-1");
    assert_eq!(health_resp.health_status, "healthy");
    assert_eq!(health_resp.backend_state, "open");

    // ListVolumeSessions
    let list_resp = client
        .list_volume_sessions(ListVolumeSessionsRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(list_resp.sessions.len(), 1);
    assert_eq!(list_resp.sessions[0].volume_id, "vol-1");
    assert_eq!(list_resp.sessions[0].attachment_handle, handle);

    // CloseVolume
    let close_resp = client
        .close_volume(CloseVolumeRequest {
            meta: None,
            volume_id: "vol-1".to_string(),
            attachment_handle: handle.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(close_resp.status, "OK");

    // Idempotent close: second close succeeds
    let close_resp2 = client
        .close_volume(CloseVolumeRequest {
            meta: None,
            volume_id: "vol-1".to_string(),
            attachment_handle: handle,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(close_resp2.status, "OK");

    // Health after close = unknown/closed
    let health_resp2 = client
        .get_volume_health(VolumeHealthRequest {
            volume_id: "vol-1".to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(health_resp2.health_status, "unknown");
    assert_eq!(health_resp2.backend_state, "closed");
}

#[tokio::test]
async fn attach_and_detach_volume_smoke() {
    let (_dir, _socket, mut client) = setup_server().await;

    // Open volume
    let open_resp = client
        .open_volume(OpenVolumeRequest {
            meta: None,
            volume_id: "vol-1".to_string(),
            backend: Some(BackendLocator {
                backend_class: "local".to_string(),
                locator: "vol-1.img".to_string(),
                options: Default::default(),
            }),
            policy: None,
        })
        .await
        .unwrap()
        .into_inner();
    let handle = open_resp.attachment_handle;

    // Attach volume to VM
    let attach_resp = client
        .attach_volume_to_vm(AttachVolumeToVmRequest {
            meta: None,
            volume_id: "vol-1".to_string(),
            vm_id: "vm-1".to_string(),
            attachment_handle: handle.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(attach_resp.volume_id, "vol-1");
    assert_eq!(attach_resp.vm_id, "vm-1");
    assert_eq!(attach_resp.result.as_ref().unwrap().status, "OK");

    // List sessions to verify attachment state
    let list_resp = client
        .list_volume_sessions(ListVolumeSessionsRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(list_resp.sessions.len(), 1);
    assert_eq!(list_resp.sessions[0].vm_id, "vm-1");
    assert_eq!(list_resp.sessions[0].runtime_status, "attached");

    // Detach volume from VM
    let detach_resp = client
        .detach_volume_from_vm(DetachVolumeFromVmRequest {
            meta: None,
            volume_id: "vol-1".to_string(),
            vm_id: "vm-1".to_string(),
            force: false,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(detach_resp.status, "OK");

    // Verify session is marked detached (distinct from an opened-but-not-
    // attached session, so the agent's sweep cannot close a live open).
    let list_resp2 = client
        .list_volume_sessions(ListVolumeSessionsRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(list_resp2.sessions[0].vm_id, "");
    assert_eq!(list_resp2.sessions[0].runtime_status, "detached");

    // Idempotent detach: no session with vm_id should still return OK
    let detach_resp2 = client
        .detach_volume_from_vm(DetachVolumeFromVmRequest {
            meta: None,
            volume_id: "vol-1".to_string(),
            vm_id: "vm-1".to_string(),
            force: false,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(detach_resp2.status, "OK");

    // Close volume
    let _ = client
        .close_volume(CloseVolumeRequest {
            meta: None,
            volume_id: "vol-1".to_string(),
            attachment_handle: handle,
        })
        .await
        .unwrap()
        .into_inner();
}

#[tokio::test]
async fn attach_volume_missing_session_returns_not_found() {
    let (_dir, _socket, mut client) = setup_server().await;

    let attach_resp = client
        .attach_volume_to_vm(AttachVolumeToVmRequest {
            meta: None,
            volume_id: "vol-missing".to_string(),
            vm_id: "vm-1".to_string(),
            attachment_handle: "no-handle".to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    let result = attach_resp.result.unwrap();
    assert_eq!(result.status, "error");
    assert_eq!(result.error_code, "NOT_FOUND");
}

#[tokio::test]
async fn allowlist_rejects_unknown_backend() {
    let (_dir, _socket, mut client) = setup_server().await;

    let resp = client
        .open_volume(OpenVolumeRequest {
            meta: None,
            volume_id: "vol-1".to_string(),
            backend: Some(BackendLocator {
                backend_class: "iscsi".to_string(),
                locator: "tgt".to_string(),
                options: Default::default(),
            }),
            policy: None,
        })
        .await
        .unwrap()
        .into_inner();

    let result = resp.result.unwrap();
    assert_eq!(result.status, "error");
    assert_eq!(result.error_code, "BACKEND_UNAVAILABLE");
}

#[tokio::test]
async fn resize_volume_smoke() {
    let (dir, _socket, mut client) = setup_server().await;

    let locator = "vol-resize.img".to_string();
    let path = dir.path().join(&locator);
    {
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&[0u8; 512]).unwrap();
    }

    let open_resp = client
        .open_volume(OpenVolumeRequest {
            meta: None,
            volume_id: "vol-resize".to_string(),
            backend: Some(BackendLocator {
                backend_class: "local".to_string(),
                locator,
                options: Default::default(),
            }),
            policy: None,
        })
        .await
        .unwrap()
        .into_inner();
    let handle = open_resp.attachment_handle;

    let resize_resp = client
        .resize_volume(ResizeVolumeRequest {
            meta: None,
            volume_id: "vol-resize".to_string(),
            new_size_bytes: 1024,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resize_resp.status, "OK");

    let meta = std::fs::metadata(&path).unwrap();
    assert_eq!(meta.len(), 1024);

    client
        .close_volume(CloseVolumeRequest {
            meta: None,
            volume_id: "vol-resize".to_string(),
            attachment_handle: handle,
        })
        .await
        .unwrap()
        .into_inner();
}

#[tokio::test]
async fn set_device_policy_smoke() {
    let (_dir, _socket, mut client) = setup_server().await;

    let open_resp = client
        .open_volume(OpenVolumeRequest {
            meta: None,
            volume_id: "vol-policy".to_string(),
            backend: Some(BackendLocator {
                backend_class: "local".to_string(),
                locator: "vol-policy.img".to_string(),
                options: Default::default(),
            }),
            policy: None,
        })
        .await
        .unwrap()
        .into_inner();
    let handle = open_resp.attachment_handle;

    let policy_resp = client
        .set_device_policy(SetDevicePolicyRequest {
            meta: None,
            volume_id: "vol-policy".to_string(),
            policy: Some(DevicePolicy {
                read_bps: 1000,
                write_bps: 2000,
                read_iops: 100,
                write_iops: 100,
                burst_allowed: false,
                read_only: false,
                no_exec: false,
                io_scheduler: String::new(),
                cache_mode: String::new(),
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(policy_resp.status, "OK");

    client
        .close_volume(CloseVolumeRequest {
            meta: None,
            volume_id: "vol-policy".to_string(),
            attachment_handle: handle,
        })
        .await
        .unwrap()
        .into_inner();
}

#[tokio::test]
async fn set_device_policy_missing_session_returns_not_found() {
    let (_dir, _socket, mut client) = setup_server().await;

    let policy_resp = client
        .set_device_policy(SetDevicePolicyRequest {
            meta: None,
            volume_id: "vol-missing".to_string(),
            policy: Some(DevicePolicy {
                read_bps: 1000,
                write_bps: 2000,
                read_iops: 100,
                write_iops: 100,
                burst_allowed: false,
                read_only: false,
                no_exec: false,
                io_scheduler: String::new(),
                cache_mode: String::new(),
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(policy_resp.status, "error");
    assert_eq!(policy_resp.error_code, "NOT_FOUND");
}

#[tokio::test]
async fn resize_volume_missing_session_returns_not_found() {
    let (_dir, _socket, mut client) = setup_server().await;

    let resize_resp = client
        .resize_volume(ResizeVolumeRequest {
            meta: None,
            volume_id: "vol-missing".to_string(),
            new_size_bytes: 1024,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resize_resp.status, "error");
    assert_eq!(resize_resp.error_code, "NOT_FOUND");
}

#[tokio::test]
async fn prepare_snapshot_smoke() {
    let (dir, _socket, mut client) = setup_server().await;

    let locator = "vol-snap.img".to_string();
    let path = dir.path().join(&locator);
    {
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&[0u8; 512]).unwrap();
    }

    let open_resp = client
        .open_volume(OpenVolumeRequest {
            meta: None,
            volume_id: "vol-snap".to_string(),
            backend: Some(BackendLocator {
                backend_class: "local".to_string(),
                locator,
                options: Default::default(),
            }),
            policy: None,
        })
        .await
        .unwrap()
        .into_inner();
    let handle = open_resp.attachment_handle;

    let resp = client
        .prepare_snapshot(PrepareSnapshotRequest {
            meta: None,
            volume_id: "vol-snap".to_string(),
            snapshot_name: "snap1".to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.status, "OK");

    let snap_path = dir.path().join("vol-snap-snap1.img");
    assert!(snap_path.exists());

    client
        .close_volume(CloseVolumeRequest {
            meta: None,
            volume_id: "vol-snap".to_string(),
            attachment_handle: handle,
        })
        .await
        .unwrap()
        .into_inner();
}

#[tokio::test]
async fn prepare_clone_smoke() {
    let (dir, _socket, mut client) = setup_server().await;

    let locator = "vol-clone.img".to_string();
    let path = dir.path().join(&locator);
    {
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&[0u8; 512]).unwrap();
    }

    let open_resp = client
        .open_volume(OpenVolumeRequest {
            meta: None,
            volume_id: "vol-clone".to_string(),
            backend: Some(BackendLocator {
                backend_class: "local".to_string(),
                locator,
                options: Default::default(),
            }),
            policy: None,
        })
        .await
        .unwrap()
        .into_inner();
    let handle = open_resp.attachment_handle;

    let resp = client
        .prepare_clone(PrepareCloneRequest {
            meta: None,
            volume_id: "vol-clone".to_string(),
            clone_name: "clone1".to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.status, "OK");

    // #540: the clone's backing file lands at the TARGET's carrier
    // locator `{clone_id}.img` — the relative name every open path
    // (create carrier, standalone attach, #522 destroy) resolves under
    // stord's runtime dir — not the pre-#540
    // `{source_id}-{clone_id}.img` no open path navigated.
    let clone_path = dir.path().join("clone1.img");
    assert!(clone_path.exists());
    assert!(
        !dir.path().join("vol-clone-clone1.img").exists(),
        "the pre-#540 unreachable clone name must not be minted"
    );

    client
        .close_volume(CloseVolumeRequest {
            meta: None,
            volume_id: "vol-clone".to_string(),
            attachment_handle: handle,
        })
        .await
        .unwrap()
        .into_inner();
}

#[tokio::test]
async fn prepare_snapshot_missing_session_returns_not_found() {
    let (_dir, _socket, mut client) = setup_server().await;

    let resp = client
        .prepare_snapshot(PrepareSnapshotRequest {
            meta: None,
            volume_id: "vol-missing".to_string(),
            snapshot_name: "snap1".to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.status, "error");
    assert_eq!(resp.error_code, "NOT_FOUND");
}

#[tokio::test]
async fn prepare_clone_missing_session_returns_not_found() {
    let (_dir, _socket, mut client) = setup_server().await;

    let resp = client
        .prepare_clone(PrepareCloneRequest {
            meta: None,
            volume_id: "vol-missing".to_string(),
            clone_name: "clone1".to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.status, "error");
    assert_eq!(resp.error_code, "NOT_FOUND");
}

#[tokio::test]
async fn sqlite_persistence_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("stord-persist.sock");
    let db_path = dir.path().join("stord.db");

    let backend = LocalFileBackend::new(dir.path().to_path_buf());
    let store = SessionStore::new(&db_path).unwrap();
    let server = StorageServer::new(
        backend,
        dir.path().to_path_buf(),
        Metrics::new(),
        vec!["local".to_string()],
        vec![],
        vec![],
        vec![],
        None,
        None,
        MigrationReceiverAuthorization::DenyAll,
        MigrationLimits::default(),
        Some(store),
    );
    let socket_clone = socket.clone();
    let db_path_clone = db_path.clone();
    tokio::spawn(async move {
        server.serve(&socket_clone, Some(&db_path_clone)).await.ok();
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut client = make_client(socket).await;

    // Open volume
    let open_resp = client
        .open_volume(OpenVolumeRequest {
            meta: None,
            volume_id: "vol-persist".to_string(),
            backend: Some(BackendLocator {
                backend_class: "local".to_string(),
                locator: "vol-persist.img".to_string(),
                options: Default::default(),
            }),
            policy: None,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(open_resp.volume_id, "vol-persist");

    // Verify persisted to SQLite by opening a new store connection
    let store2 = SessionStore::new(&db_path).unwrap();
    let sessions = store2.list().await.unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].volume_id, "vol-persist");

    // Close volume
    client
        .close_volume(CloseVolumeRequest {
            meta: None,
            volume_id: "vol-persist".to_string(),
            attachment_handle: open_resp.attachment_handle,
        })
        .await
        .unwrap()
        .into_inner();

    // Verify removed from SQLite
    let store3 = SessionStore::new(&db_path).unwrap();
    let sessions = store3.list().await.unwrap();
    assert!(sessions.is_empty());
}

/// #368 residue-idempotency pin (local backend): a re-driven create
/// re-executes the SAME stord calls against the residue of a
/// terminally-failed create — a provisioned volume file on disk whose
/// session the create unwind closed. Against the real
/// `LocalFileBackend`:
///
/// - a re-open carrying the original provisioning hints (size + seed)
///   must NOT re-provision: `LocalFileBackend::open` skips seeding and
///   resizing when the path already exists, so the residue volume's
///   content and size survive the re-drive;
/// - re-attach is path-based and idempotent (no AlreadyExists).
///
/// This pin is LOCAL-backend only. ceph/iscsi/lvm have no equivalent
/// test (unpinned — see the #368 design doc's residual-risk note: an
/// explicit attach-idempotency pin for non-local backends is follow-up
/// scope before enabling re-drives against them).
#[tokio::test]
async fn create_redrive_residue_is_idempotent_against_the_local_backend() {
    let (dir, _socket, mut client) = setup_server().await;

    // The seed image the original create provisioned from.
    let seed = dir.path().join("seed.img");
    let mut seed_file = std::fs::File::create(&seed).unwrap();
    seed_file.write_all(&[0xAA; 4096]).unwrap();
    drop(seed_file);

    // 1. The original create's open: provisions the volume from the seed
    //    (copies 4096 bytes, then expands to the requested size).
    let mut options = std::collections::HashMap::new();
    options.insert("size_bytes".to_string(), "8192".to_string());
    options.insert("seed_from".to_string(), seed.to_string_lossy().to_string());
    let open_req = OpenVolumeRequest {
        meta: None,
        volume_id: "vol-redrive".to_string(),
        backend: Some(BackendLocator {
            backend_class: "local".to_string(),
            locator: "vol-redrive.img".to_string(),
            options,
        }),
        policy: None,
    };
    let open_resp = client
        .open_volume(open_req.clone())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(open_resp.result.as_ref().unwrap().status, "OK");
    let handle = open_resp.attachment_handle;

    // 2. The failed create's unwind closes the session — the volume file
    //    is the residue left on disk.
    let close_resp = client
        .close_volume(CloseVolumeRequest {
            meta: None,
            volume_id: "vol-redrive".to_string(),
            attachment_handle: handle,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(close_resp.status, "OK");

    // Residue state: data written into the provisioned volume after
    // seeding — anything a re-drive must never clobber.
    let volume_path = dir.path().join("vol-redrive.img");
    let mut volume = std::fs::OpenOptions::new()
        .write(true)
        .open(&volume_path)
        .unwrap();
    volume
        .write_all(b"RESIDUE-THAT-MUST-SURVIVE-THE-RE-DRIVE")
        .unwrap();
    volume.flush().unwrap();
    drop(volume);

    // 3. The re-driven create re-opens with the SAME provisioning hints.
    let reopen_resp = client.open_volume(open_req).await.unwrap().into_inner();
    assert_eq!(
        reopen_resp.result.as_ref().unwrap().status,
        "OK",
        "re-open of the residue volume must succeed"
    );
    let rehandle = reopen_resp.attachment_handle;

    // The residue content survived: re-open did not re-seed or resize.
    let content = std::fs::read(&volume_path).unwrap();
    assert!(
        content.starts_with(b"RESIDUE-THAT-MUST-SURVIVE-THE-RE-DRIVE"),
        "re-open must not re-seed an already-provisioned volume"
    );
    assert_eq!(content.len(), 8192, "re-open must not resize the volume");

    // 4. Re-attach is idempotent (path-based local attach).
    for attempt in 1..=2 {
        let attach_resp = client
            .attach_volume_to_vm(AttachVolumeToVmRequest {
                meta: None,
                volume_id: "vol-redrive".to_string(),
                vm_id: "vm-redrive".to_string(),
                attachment_handle: rehandle.clone(),
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            attach_resp.result.as_ref().unwrap().status,
            "OK",
            "attach (attempt {attempt}) of the residue volume must succeed"
        );
    }
}
