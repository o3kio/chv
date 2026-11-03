use crate::migration::auth::{
    MigrationAdmission, MigrationLimits, MigrationReceiverAuthorization, MigrationReplayCache,
};
use crate::migration::receiver::MigrationReceiver;
use chv_common::migration_auth::MigrationAuthError;
use chv_stord_api::chv_stord_api::{
    migration_message, storage_migration_service_server::StorageMigrationService, MigrationMessage,
    MigrationReady,
};
use chv_stord_backends::StorageBackend;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{error, info, warn};

/// Maximum volume size accepted for a migration (16 TiB).
const MAX_MIGRATION_SIZE_BYTES: u64 = 16 * 1024 * 1024 * 1024 * 1024;

/// Tonic service implementation for the storage migration receiver.
///
/// This handles incoming bidirectional streams. The first message in the
/// stream must be InitMigration, which causes this node to act as the
/// migration destination (receiver).
pub struct StorageMigrationServiceImpl<B: StorageBackend> {
    backend: Arc<B>,
    runtime_dir: Option<std::path::PathBuf>,
    /// Control-plane token verification policy (issue #663). Checked before
    /// any backend call that can provision a volume.
    authorization: MigrationReceiverAuthorization,
    /// Per-node in-flight count/byte limits, shared by every accepted
    /// stream (including both serving paths for the same service value).
    admission: Arc<MigrationAdmission>,
    /// Single-use token nonces, bounded by token expiry.
    replay: Arc<MigrationReplayCache>,
}

impl<B: StorageBackend> StorageMigrationServiceImpl<B> {
    pub fn new(
        backend: Arc<B>,
        runtime_dir: std::path::PathBuf,
        authorization: MigrationReceiverAuthorization,
        limits: MigrationLimits,
    ) -> Self {
        let admission = MigrationAdmission::new(limits).unwrap_or_else(|error| {
            warn!(
                error = %error,
                "invalid migration admission limits; all inbound migrations will be rejected"
            );
            MigrationAdmission::unavailable()
        });
        Self {
            backend,
            runtime_dir: Some(runtime_dir),
            authorization,
            admission,
            replay: MigrationReplayCache::new(),
        }
    }

    /// Verify the control-plane authorization before any volume creation.
    ///
    /// Ordering is deliberate: signature/claims first, then admission
    /// reservation, then nonce consumption. A replay is rejected even while
    /// capacity is free; a reservation failure never reaches the backend and
    /// drops the slot immediately.
    fn authorize_and_reserve(
        &self,
        init: &chv_stord_api::chv_stord_api::InitMigration,
    ) -> Result<crate::migration::auth::MigrationReservation, Status> {
        let claims = self
            .authorization
            .authorize(&init.authorization, &init.volume_id, init.size_bytes)
            .map_err(migration_auth_status)?;

        let reservation = self.admission.reserve(init.size_bytes).map_err(|error| {
            warn!(
                volume_id = %init.volume_id,
                size_bytes = init.size_bytes,
                error = %error,
                "migration rejected by per-node admission limits"
            );
            Status::resource_exhausted(format!("migration admission rejected: {error}"))
        })?;

        self.replay
            .consume(
                &claims.nonce,
                claims.expires_at_unix_ms,
                chv_common::now_unix_ms(),
            )
            .map_err(|error| {
                warn!(
                    volume_id = %init.volume_id,
                    "migration authorization replay rejected"
                );
                migration_auth_status(error)
            })?;

        Ok(reservation)
    }

    /// Defense in depth at the migration gRPC boundary.
    ///
    /// The local backend builds the receiving volume path as
    /// `runtime_dir/{volume_id}.img`, so a hostile peer could escape the
    /// runtime directory via `../` components in `volume_id`. The agent-side
    /// `is_safe_id` check already blocks such ids before they reach stord,
    /// but a peer stord is its own trust boundary: reject unsafe ids here
    /// too, and additionally verify that the resolved receiving path stays
    /// inside `runtime_dir` (fail closed on any resolution error).
    fn validate_receiving_volume_id(&self, volume_id: &str) -> Result<(), ReceivingVolumeError> {
        if !chv_common::is_safe_id(volume_id) {
            return Err(ReceivingVolumeError::UnsafeId(volume_id.to_string()));
        }
        if let Some(runtime_dir) = self.runtime_dir.as_deref() {
            let dest = runtime_dir.join(format!("{}.img", volume_id));
            let canonical = crate::handlers::canonicalize_or_ancestor(&dest)
                .map_err(|e| ReceivingVolumeError::Resolution(e.to_string()))?;
            let canonical_runtime = crate::handlers::canonicalize_or_ancestor(runtime_dir)
                .map_err(|e| ReceivingVolumeError::Resolution(e.to_string()))?;
            if !canonical.starts_with(&canonical_runtime) {
                return Err(ReceivingVolumeError::Escape(dest.display().to_string()));
            }
        }
        Ok(())
    }
}

/// Small error type for receiving-volume id validation, kept separate from
/// `tonic::Status` so the hot error path doesn't carry a 176-byte `Status`.
enum ReceivingVolumeError {
    UnsafeId(String),
    Resolution(String),
    Escape(String),
}

impl From<ReceivingVolumeError> for Status {
    fn from(error: ReceivingVolumeError) -> Self {
        match error {
            ReceivingVolumeError::UnsafeId(id) => Status::invalid_argument(format!(
                "volume_id '{id}' is not a safe id (path separators or traversal rejected)"
            )),
            ReceivingVolumeError::Resolution(message) => Status::permission_denied(message),
            ReceivingVolumeError::Escape(path) => Status::invalid_argument(format!(
                "receiving volume path '{path}' escapes runtime_dir"
            )),
        }
    }
}

type ResponseStream = Pin<Box<dyn Stream<Item = Result<MigrationMessage, Status>> + Send>>;

/// Cheap clone so the migration service can be served simultaneously on the
/// Unix socket and on the mTLS TCP receiver listener (issue #390): all state
/// is shared (`Arc` backend) or immutable, and per-stream receiver state is
/// created fresh in `stream_blocks`.
impl<B: StorageBackend> Clone for StorageMigrationServiceImpl<B> {
    fn clone(&self) -> Self {
        Self {
            backend: Arc::clone(&self.backend),
            runtime_dir: self.runtime_dir.clone(),
            authorization: self.authorization.clone(),
            admission: Arc::clone(&self.admission),
            replay: Arc::clone(&self.replay),
        }
    }
}

/// Map a rejected authorization to a closed gRPC status. No variant ever
/// permits the caller to proceed to volume creation.
fn migration_auth_status(error: MigrationAuthError) -> Status {
    match error {
        MigrationAuthError::Missing => {
            Status::unauthenticated("missing control-plane migration authorization token")
        }
        MigrationAuthError::NotConfigured => Status::failed_precondition(
            "this stord has no migration authorization verifier configured; \
             inbound migrations are rejected",
        ),
        MigrationAuthError::SecretTooShort => Status::failed_precondition(
            "migration signing secret is misconfigured (shorter than 32 bytes)",
        ),
        MigrationAuthError::WrongDestination
        | MigrationAuthError::WrongVolume
        | MigrationAuthError::WrongSize => {
            Status::permission_denied(format!("migration authorization rejected: {error}"))
        }
        MigrationAuthError::Malformed
        | MigrationAuthError::UnsupportedVersion
        | MigrationAuthError::BadSignature
        | MigrationAuthError::Expired
        | MigrationAuthError::NotYetValid
        | MigrationAuthError::InvalidLifetime
        | MigrationAuthError::Replay
        | MigrationAuthError::InvalidClaims(_) => {
            Status::permission_denied(format!("migration authorization rejected: {error}"))
        }
    }
}

#[tonic::async_trait]
impl<B: StorageBackend> StorageMigrationService for StorageMigrationServiceImpl<B> {
    type StreamBlocksStream = ResponseStream;

    async fn stream_blocks(
        &self,
        request: Request<Streaming<MigrationMessage>>,
    ) -> Result<Response<Self::StreamBlocksStream>, Status> {
        let mut inbound = request.into_inner();

        // Read the first message to determine role
        let first_msg = inbound
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("stream closed without sending a message"))?;

        let init = match first_msg.payload {
            Some(migration_message::Payload::Init(init)) => init,
            _ => {
                return Err(Status::invalid_argument(
                    "first message must be InitMigration",
                ));
            }
        };

        if init.size_bytes > MAX_MIGRATION_SIZE_BYTES {
            return Err(Status::invalid_argument(format!(
                "InitMigration size_bytes {} exceeds maximum allowed {} bytes",
                init.size_bytes, MAX_MIGRATION_SIZE_BYTES
            )));
        }

        // Defense in depth: never let a peer-supplied volume_id escape the
        // runtime directory when the backend constructs the receiving path.
        self.validate_receiving_volume_id(&init.volume_id)?;

        // Issue #663: a CA-certified peer is not automatically authorized
        // to create this volume. Verify the control-plane token and reserve
        // capacity BEFORE the backend can provision anything.
        let reservation = self.authorize_and_reserve(&init)?;

        info!(
            volume_id = %init.volume_id,
            size_bytes = init.size_bytes,
            block_size = init.block_size,
            format = %init.format,
            "received authorized InitMigration, starting receiver"
        );

        // Create the receiving volume
        let export = self
            .backend
            .create_receiving_volume(&init.volume_id, init.size_bytes, &init.format)
            .await
            .map_err(|e| {
                error!(
                    volume_id = %init.volume_id,
                    error = %e,
                    "failed to create receiving volume"
                );
                Status::internal(format!("failed to create receiving volume: {e}"))
            })?;

        let dest_volume_id = init.volume_id.clone();
        let handle = export.attachment_handle.clone();

        // Set up outgoing response channel
        let (tx, rx) = mpsc::channel::<MigrationMessage>(256);

        // Send MigrationReady
        let ready_msg = MigrationMessage {
            payload: Some(migration_message::Payload::Ready(MigrationReady {
                dest_volume_id: dest_volume_id.clone(),
            })),
        };
        tx.send(ready_msg)
            .await
            .map_err(|_| Status::internal("failed to send MigrationReady: channel closed"))?;

        // Spawn the receiver loop
        let backend = self.backend.clone();
        let volume_id = dest_volume_id.clone();
        tokio::spawn(async move {
            // Held for the entire stream lifetime and released on every exit
            // path (success, failure, or peer closing the stream).
            let _reservation = reservation;
            let receiver =
                MigrationReceiver::new(backend, volume_id.clone(), handle, init.size_bytes);
            if let Err(e) = receiver.run(inbound, tx).await {
                error!(
                    volume_id = %volume_id,
                    error = %e,
                    "migration receiver failed"
                );
            }
        });

        // Return the response stream
        let output_stream = ReceiverStream::new(rx);
        let mapped_stream: ResponseStream =
            Box::pin(tokio_stream::StreamExt::map(output_stream, Ok));

        Ok(Response::new(mapped_stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chv_stord_backends::LocalFileBackend;

    fn make_service(
        runtime_dir: std::path::PathBuf,
    ) -> StorageMigrationServiceImpl<LocalFileBackend> {
        StorageMigrationServiceImpl::new(
            Arc::new(LocalFileBackend::new(runtime_dir.clone())),
            runtime_dir,
            MigrationReceiverAuthorization::DenyAll,
            MigrationLimits::default(),
        )
    }

    #[test]
    fn receiving_volume_id_rejects_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path().to_path_buf());
        assert!(svc.validate_receiving_volume_id("../escape").is_err());
        assert!(svc
            .validate_receiving_volume_id("../../etc/passwd")
            .is_err());
        assert!(svc.validate_receiving_volume_id("a/b").is_err());
        assert!(svc.validate_receiving_volume_id("a\\b").is_err());
        assert!(svc.validate_receiving_volume_id("a..b").is_err());
        assert!(svc.validate_receiving_volume_id("").is_err());
    }

    #[test]
    fn receiving_volume_id_accepts_safe_ids() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path().to_path_buf());
        assert!(svc.validate_receiving_volume_id("vol-1").is_ok());
        assert!(svc.validate_receiving_volume_id("vm_2.disk").is_ok());
    }

    #[test]
    fn receiving_volume_id_fails_closed_on_unresolvable_runtime_dir() {
        // A symlink loop under runtime_dir cannot be canonicalized; the
        // containment check must fail closed instead of skipping.
        let dir = tempfile::tempdir().unwrap();
        let loop_path = dir.path().join("loop");
        std::os::unix::fs::symlink(&loop_path, &loop_path).unwrap();
        let svc = make_service(loop_path);
        assert!(svc.validate_receiving_volume_id("vol-1").is_err());
    }
}
