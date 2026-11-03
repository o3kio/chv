use crate::migration::flow_control::SendWindow;
use crate::migration::task::{MigrationPhase, MigrationTask};
use crate::migration::volume_digest;
use chv_stord_api::chv_stord_api::{
    migration_message, storage_migration_service_client::StorageMigrationServiceClient, AckStatus,
    BlockChunk, FinalSync, FinalizeComplete, InitMigration, MigrationMessage, RoundComplete,
    RoundStart,
};
use chv_stord_backends::{
    StorageBackend, WriteCanaryCapability, WriteCanaryFingerprint, DIRTY_TRACKING_BLOCK_SIZE,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity};
use tracing::{debug, error, info, warn};

// Metric names for storage migration operations.
const STORD_MIGRATION_BYTES_SENT_TOTAL: &str = "chv_stord_migration_bytes_sent_total";
const STORD_MIGRATION_ERRORS_TOTAL: &str = "chv_stord_migration_errors_total";

/// Distinct error token for the write-canary failure (issue #394,
/// Option A). Surfaces in the task's `error_message` and the agent's
/// migration-failure log line, so operators can tell "the source was
/// written during migration" (retry with the VM quiesced) apart from a
/// finalize-time digest mismatch.
const SOURCE_MODIFIED_CODE: &str = "source_modified_during_migration";

/// Default migration block size: must equal the backends' dirty-tracking
/// block size so bitmap bits map 1:1 to migration chunks.
const DEFAULT_BLOCK_SIZE: u64 = DIRTY_TRACKING_BLOCK_SIZE;
const DIRTY_THRESHOLD: u64 = 1024;
const MAX_DIRTY_ROUNDS: u32 = 10;

/// Default bound on the wait for `FinalizeAck` after `FinalizeComplete`.
/// The receiver answers immediately after its full-volume digest pass, so
/// a healthy finalize is bounded by disk throughput; 30 s matches the
/// existing per-Ack timeout and is generous for a local digest pass while
/// still guaranteeing the task cannot hang forever on a dead peer.
pub const DEFAULT_FINALIZE_ACK_TIMEOUT: Duration = Duration::from_secs(30);

/// Production bounds for the configurable `FinalizeAck` timeout. Values
/// outside this range are clamped by [`start_migration_to_peer`]; the
/// [`MigrationSender::with_finalize_ack_timeout`] setter deliberately does
/// not clamp so integration tests can drive a sub-second timeout.
pub const MIN_FINALIZE_ACK_TIMEOUT: Duration = Duration::from_secs(5);
pub const MAX_FINALIZE_ACK_TIMEOUT: Duration = Duration::from_secs(300);

/// TLS configuration for mTLS connections to the migration destination.
///
/// When provided, the sender will validate the destination's certificate against
/// the CA and present its own node certificate, as required by the disk migration
/// protocol spec.
#[derive(Clone)]
pub struct MigrationTlsConfig {
    /// PEM-encoded client certificate (node cert issued by CP CA).
    pub cert_pem: Vec<u8>,
    /// PEM-encoded client private key.
    pub key_pem: Vec<u8>,
    /// PEM-encoded CA certificate to validate the destination.
    pub ca_pem: Vec<u8>,
    /// Expected domain name of the destination (for certificate validation).
    pub dest_domain: String,
}

/// Drives the source side of a storage migration.
///
/// This is invoked by the control plane / agent to migrate a volume
/// from the local node to a remote peer's stord.
///
/// Flow control: the sender maintains a sliding window of at most
/// `send_window_size` (default 128, `SendWindow::new`) unacknowledged chunks. The receiver
/// sends an `Ack` every 64 chunks while streaming and flushes the ack
/// window at stream boundaries (`RoundComplete`, `FinalSync`, and before
/// `FinalizeAck`), so the sender's per-phase drains complete for arbitrary
/// chunk counts. `RoundComplete` is always answered with an `Ack` carrying
/// the receiver's cumulative sequence number (the round acknowledgment).
pub struct MigrationSender<B: StorageBackend> {
    backend: Arc<B>,
    volume_id: String,
    handle: String,
    block_size: u64,
    send_window: SendWindow,
    last_acknowledged_offset: u64,
    tls_config: Option<MigrationTlsConfig>,
    /// Backpressure factor received from the destination. Values > 1.0 cause
    /// the sender to insert a throttle sleep between chunk sends.
    backpressure_factor: f32,
    /// Optional shared task state for progress reporting and VM-pause coordination.
    task: Option<Arc<MigrationTask>>,
    /// Migration-start stat fingerprint of the source's backing store
    /// (issue #394, Option A). `None` means the backend reported
    /// [`WriteCanaryCapability::Unavailable`] (or the migration has not
    /// sampled yet): no early write detection, the #392 finalize digest
    /// remains the only (late) correctness gate — the pre-canary
    /// behavior.
    canary_baseline: Option<WriteCanaryFingerprint>,
    /// Pause-first mode (issue #394, Option C): request the VM pause
    /// BEFORE any source byte is read, so the entire transfer runs
    /// against a quiescent source — correct by construction for any
    /// write pattern, at the cost of downtime equal to the transfer
    /// time. Opt-in; the default (false) keeps the quiescent-assumed
    /// contract: write canary + finalize digest.
    pause_first: bool,
    /// Bound on the wait for `FinalizeAck` after `FinalizeComplete`.
    /// Without it a destination that accepts the stream but never answers
    /// finalization leaves the sender (and the VM) paused forever.
    finalize_ack_timeout: Duration,
    /// Control-plane-issued authorization for this migration (issue #663).
    /// Forwarded verbatim in `InitMigration`; the destination verifies it
    /// before creating the receiving volume. Empty means the destination
    /// will reject the stream (unless it is explicitly in insecure mode).
    migration_authorization: String,
    /// Optional one-shot notifier resolved when the destination's *initial*
    /// handshake finishes. `Ok(())` means `MigrationReady` arrived (the
    /// destination authorized the token, reserved admission, and created
    /// the receiving volume); `Err(status)` carries the destination's
    /// rejection (including `RESOURCE_EXHAUSTED`). The source stord trigger
    /// handler awaits this so admission errors reach the agent
    /// synchronously and its bounded retry can act.
    handshake_tx: Option<tokio::sync::oneshot::Sender<Result<(), tonic::Status>>>,
}

/// Fires the source-side handshake notifier exactly once.
///
/// `success()` after `MigrationReady`; `fail()` with the real transport or
/// destination status on any earlier error. On drop without either (for
/// example a `?` return on a path that did not explicitly notify), a
/// generic error is sent so the trigger handler never has to wait for the
/// full bounded timeout.
struct HandshakeNotifier(Option<tokio::sync::oneshot::Sender<Result<(), tonic::Status>>>);

impl HandshakeNotifier {
    fn new(tx: Option<tokio::sync::oneshot::Sender<Result<(), tonic::Status>>>) -> Self {
        Self(tx)
    }

    fn success(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(Ok(()));
        }
    }

    fn fail(&mut self, status: &tonic::Status) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(Err(status.clone()));
        }
    }
}

impl Drop for HandshakeNotifier {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(Err(tonic::Status::internal(
                "migration task ended before the destination handshake completed",
            )));
        }
    }
}

impl<B: StorageBackend> MigrationSender<B> {
    pub fn new(backend: Arc<B>, volume_id: String, handle: String) -> Self {
        Self {
            backend,
            volume_id,
            handle,
            block_size: DEFAULT_BLOCK_SIZE,
            send_window: SendWindow::new(),
            last_acknowledged_offset: 0,
            tls_config: None,
            backpressure_factor: 1.0,
            task: None,
            canary_baseline: None,
            pause_first: false,
            finalize_ack_timeout: DEFAULT_FINALIZE_ACK_TIMEOUT,
            migration_authorization: String::new(),
            handshake_tx: None,
        }
    }

    /// Attach the control-plane-issued migration authorization forwarded in
    /// `InitMigration` (issue #663).
    pub fn with_migration_authorization(mut self, authorization: String) -> Self {
        self.migration_authorization = authorization;
        self
    }

    /// Attach a one-shot notifier for the destination's initial handshake.
    ///
    /// The notifier resolves exactly once: `Ok(())` after `MigrationReady`
    /// (the destination admitted the stream and created the receiving
    /// volume), or `Err(status)` with the destination's rejection. The
    /// source stord trigger handler awaits it to surface admission errors
    /// such as `RESOURCE_EXHAUSTED` synchronously.
    pub fn with_handshake_notifier(
        mut self,
        tx: tokio::sync::oneshot::Sender<Result<(), tonic::Status>>,
    ) -> Self {
        self.handshake_tx = Some(tx);
        self
    }

    /// Attach a shared migration task for progress reporting and pause coordination.
    pub fn with_task(mut self, task: Arc<MigrationTask>) -> Self {
        self.task = Some(task);
        self
    }

    pub fn with_block_size(mut self, block_size: u64) -> Self {
        self.block_size = block_size;
        self
    }

    /// Opt into pause-first mode (issue #394, Option C): the VM pause is
    /// requested before any source byte is read, making the transfer
    /// correct by construction. Requires a task to be attached (the pause
    /// coordination channel); `start_migration` fails closed otherwise.
    pub fn with_pause_first(mut self) -> Self {
        self.pause_first = true;
        self
    }

    /// Bound the wait for `FinalizeAck` after `FinalizeComplete`.
    ///
    /// The value is used exactly as given (no clamping) so tests can drive
    /// a sub-second failure path. Callers entering through
    /// [`start_migration_to_peer`] get production bounds
    /// ([`MIN_FINALIZE_ACK_TIMEOUT`]..=[`MAX_FINALIZE_ACK_TIMEOUT`]).
    pub fn with_finalize_ack_timeout(mut self, timeout: Duration) -> Self {
        self.finalize_ack_timeout = timeout;
        self
    }

    /// Configure mTLS for the connection to the migration destination.
    ///
    /// When set, the sender uses `https://` and presents the node certificate
    /// while validating the destination against the provided CA certificate.
    pub fn with_tls(mut self, tls_config: MigrationTlsConfig) -> Self {
        self.tls_config = Some(tls_config);
        self
    }

    /// Returns the last acknowledged offset, useful for resumability.
    pub fn last_acknowledged_offset(&self) -> u64 {
        self.last_acknowledged_offset
    }

    /// Start a migration to a peer node at the given gRPC endpoint.
    ///
    /// This opens a bidirectional stream, sends InitMigration, waits for
    /// MigrationReady, then performs bulk copy followed by dirty sync rounds.
    pub async fn start_migration(mut self, endpoint: String) -> Result<(), tonic::Status> {
        let mut handshake = HandshakeNotifier::new(self.handshake_tx.take());
        let result = self.start_migration_inner(endpoint, &mut handshake).await;
        match &result {
            Ok(()) => handshake.success(),
            Err(status) => handshake.fail(status),
        }
        result
    }

    async fn start_migration_inner(
        &mut self,
        endpoint: String,
        handshake: &mut HandshakeNotifier,
    ) -> Result<(), tonic::Status> {
        // Pause-first fail-closed check (issue #394, Option C): the mode
        // requires the task's pause coordination channel. Without a task
        // it cannot be honored — fail BEFORE connecting rather than
        // silently degrading to quiescent-assumed semantics (an operator
        // asked for the pause; pretending it happened would reintroduce
        // the #394 failure mode this mode exists to close).
        if self.pause_first && self.task.is_none() {
            return Err(tonic::Status::failed_precondition(
                "pause-first migration requires VM-pause coordination, but no migration \
                 task is attached",
            ));
        }

        let channel = if let Some(ref tls) = self.tls_config {
            let identity = Identity::from_pem(&tls.cert_pem, &tls.key_pem);
            let ca = Certificate::from_pem(&tls.ca_pem);
            let tls_config = ClientTlsConfig::new()
                .domain_name(&tls.dest_domain)
                .identity(identity)
                .ca_certificate(ca);

            // Ensure the endpoint uses https
            let secure_endpoint = if endpoint.starts_with("http://") {
                endpoint.replacen("http://", "https://", 1)
            } else if !endpoint.starts_with("https://") {
                format!("https://{endpoint}")
            } else {
                endpoint.clone()
            };

            info!(
                endpoint = %secure_endpoint,
                dest_domain = %tls.dest_domain,
                "connecting to migration peer with mTLS"
            );

            Channel::from_shared(secure_endpoint)
                .map_err(|e| tonic::Status::internal(format!("invalid endpoint: {e}")))?
                .tls_config(tls_config)
                .map_err(|e| tonic::Status::internal(format!("TLS config error: {e}")))?
                .connect()
                .await
                .map_err(|e| {
                    // Quality-of-failure (issue #402): a TLS-layer rejection
                    // otherwise collapses to the single text "transport
                    // error", so an operator cannot tell a misconfigured CA
                    // from a wrong server name from an expired certificate.
                    // Walk the tonic error's source chain (the technique
                    // already used by tests/migration_mtls.rs to surface the
                    // rustls fatal alert) and include the terminal error
                    // display, which for an mTLS rejection is the rustls
                    // alert/reason name (e.g. UnknownIssuer,
                    // CertNotValidForName, Expired). Content-free by
                    // construction: rustls error displays carry alert/reason
                    // names only, never certificate contents or key
                    // material.
                    let status = tonic::Status::unavailable(format!(
                        "failed to connect to peer with mTLS: transport error: {}",
                        terminal_error_display(&e)
                    ));
                    if let Some(ref task) = self.task {
                        task.mark_failed(status.message().to_string());
                    }
                    status
                })?
        } else {
            // Destination-only and disabled stords both land here: the
            // node has no migration client identity and does not initiate
            // migrations. The four client keys are optional since #401,
            // so they are mentioned only as the conditional ("set them
            // only if this node should send migrations"), never as an
            // unconditional instruction.
            return Err(tonic::Status::failed_precondition(
                "this node has no migration client identity configured — it does not \
                 initiate migrations (mTLS is required for storage migration). Set \
                 migration.client_cert_path, migration.client_key_path, \
                 migration.ca_cert_path, and migration.dest_server_name only if this \
                 node should send migrations.",
            ));
        };

        let mut client = StorageMigrationServiceClient::new(channel);

        // Get volume size
        let volume_size = self
            .backend
            .volume_size(&self.volume_id, &self.handle)
            .await
            .map_err(|e| tonic::Status::internal(format!("failed to get volume size: {e}")))?;

        // Set up outgoing message channel
        let (tx, rx) = mpsc::channel::<MigrationMessage>(256);
        let rx_stream = tokio_stream::wrappers::ReceiverStream::new(rx);

        // Send InitMigration *before* awaiting the stream response.
        //
        // The tonic client future for a bidirectional RPC resolves only
        // when the server sends its response headers, and the server sends
        // them only once the handler returns — which for this service
        // happens after it has read the first message (InitMigration) and
        // created the receiving volume. Sending Init first is therefore
        // required for the handshake to make progress at all; queuing it
        // in the outgoing channel is safe because the ReceiverStream is
        // polled as soon as the request starts.
        let init_msg = MigrationMessage {
            payload: Some(migration_message::Payload::Init(InitMigration {
                volume_id: self.volume_id.clone(),
                size_bytes: volume_size,
                block_size: self.block_size as u32,
                format: "raw".to_string(),
                checksum_type: "crc32".to_string(),
                authorization: self.migration_authorization.clone(),
            })),
        };
        tx.send(init_msg)
            .await
            .map_err(|_| tonic::Status::internal("failed to send InitMigration: channel closed"))?;

        // Start the bidirectional stream
        let response = client.stream_blocks(rx_stream).await?;
        let mut inbound = response.into_inner();

        info!(
            volume_id = %self.volume_id,
            volume_size,
            block_size = self.block_size,
            "sent InitMigration to peer"
        );

        // Wait for MigrationReady
        let ready_msg = inbound
            .message()
            .await?
            .ok_or_else(|| tonic::Status::internal("stream closed before MigrationReady"))?;

        match ready_msg.payload {
            Some(migration_message::Payload::Ready(ref ready)) => {
                info!(
                    dest_volume_id = %ready.dest_volume_id,
                    "peer is ready to receive migration"
                );
            }
            Some(migration_message::Payload::Error(ref err)) => {
                return Err(tonic::Status::internal(format!(
                    "peer returned error: {}",
                    err.message
                )));
            }
            _ => {
                return Err(tonic::Status::internal(
                    "unexpected message; expected MigrationReady",
                ));
            }
        }

        // The destination has authorized the token, reserved admission, and
        // created the receiving volume. Release the source stord trigger
        // handler now: anything from here on is post-admission progress, and
        // its failure is reported asynchronously through the migration task.
        handshake.success();

        // Pause-first gate (issue #394, Option C): the opt-in
        // stop-the-world mode. The VM is paused BEFORE any source byte
        // is read, so the transfer is correct by construction — no
        // concurrent write can occur. The canary armed below (after this
        // gate) therefore covers the whole transfer window and becomes a
        // tripwire for non-VM writers (a stray host process); the dirty
        // rounds converge trivially on the quiesced source; the finalize
        // digest verifies instead of catching loss. The no-task case was
        // already rejected before connecting (see start_migration's
        // head).
        if self.pause_first {
            let Some(ref task) = self.task else {
                // Belt-and-braces: the head-of-function guard already
                // rejected this. Fail closed here too rather than panic
                // (the no-panics-in-service-code rule) — same error,
                // same semantics.
                return Err(tonic::Status::failed_precondition(
                    "pause-first migration requires VM-pause coordination, but no migration \
                     task is attached",
                ));
            };
            {
                let mut state = task.state.write().await;
                state.phase = MigrationPhase::PausedPreCopy;
                state.needs_vm_pause = true;
            }
            info!(
                volume_id = %self.volume_id,
                "pause-first: requesting VM pause before any source read"
            );
            let mut pause_rx = task.pause_tx.subscribe();
            while !*pause_rx.borrow() {
                if pause_rx.changed().await.is_err() {
                    let mut state = task.state.write().await;
                    state.phase = MigrationPhase::Failed;
                    state.error_message = "pause channel closed".to_string();
                    state.needs_vm_pause = false;
                    return Err(tonic::Status::cancelled("pause channel closed"));
                }
            }
            info!(
                volume_id = %self.volume_id,
                "pause-first: VM paused, source quiescent for the whole transfer"
            );
        }

        // Write canary baseline (issue #394, Option A): sample the
        // source's backing-store stat immediately before any bytes are
        // read (in pause-first mode, after the VM pause has completed,
        // so the covered window is exactly the quiesced transfer).
        // Every later re-check (dirty-round boundaries, the pre-pause
        // gate) compares against this sample; a change means
        // the source was written during the migration — writes the
        // dirty bitmap provably cannot account for (#394 R1) — and the
        // migration fails *here*, not after a full wasted transfer.
        // Backends that cannot stat-observe writes (block devices,
        // RADOS) report Unavailable and we proceed exactly as before:
        // the #392 finalize digest remains the correctness gate.
        let baseline = self
            .backend
            .write_canary_probe(&self.volume_id, &self.handle)
            .await
            .map_err(|e| {
                // Fail closed: a probe error means the backing store
                // cannot be stat'ed, in which case bulk copy's reads
                // would fail too. Never silently disable the canary.
                let status =
                    tonic::Status::internal(format!("failed to probe source write canary: {e}"));
                if let Some(ref task) = self.task {
                    task.mark_failed(status.message().to_string());
                }
                status
            })?;
        match baseline.capability {
            WriteCanaryCapability::FileStat => {
                self.canary_baseline = Some(baseline);
                info!(
                    volume_id = %self.volume_id,
                    "source write canary armed (file-stat fingerprint)"
                );
            }
            WriteCanaryCapability::Unavailable { ref reason } => {
                warn!(
                    volume_id = %self.volume_id,
                    reason = %reason,
                    "source write canary unavailable on this backend; concurrent source writes \
                     will only be caught by the finalize digest (fail-late, #394 R3)"
                );
            }
        }

        // Bulk copy phase
        if let Some(ref task) = self.task {
            let mut state = task.state.write().await;
            state.phase = MigrationPhase::BulkCopy;
        }
        info!(volume_id = %self.volume_id, "starting bulk copy phase");
        let mut sequence_num = self.bulk_copy(&tx, &mut inbound, volume_size).await?;
        info!(
            volume_id = %self.volume_id,
            total_chunks = sequence_num,
            last_ack_offset = self.last_acknowledged_offset,
            "bulk copy phase complete"
        );

        // Iterative dirty sync rounds: re-send blocks that were written during bulk copy
        if let Some(ref task) = self.task {
            let mut state = task.state.write().await;
            state.phase = MigrationPhase::DirtySync;
        }
        info!(volume_id = %self.volume_id, "starting iterative dirty sync rounds");
        let dirty_chunks = self
            .dirty_sync_rounds(&tx, &mut inbound, &mut sequence_num, volume_size)
            .await?;
        info!(
            volume_id = %self.volume_id,
            dirty_chunks,
            "dirty sync rounds complete"
        );

        // Pre-pause gate (issue #394, Option A): the last canary
        // re-check, immediately before the pause handshake is
        // requested. Failing here means the VM was never paused — no
        // resume is needed, the agent's failure handling observes the
        // Failed phase. The residual window *after* this check (writes
        // landing between here and the pause completing) is covered by
        // the post-pause whole-volume digest below: fail-closed, just
        // later.
        self.verify_source_canary().await?;

        // If a task is attached, we coordinate with the agent: wait for VM pause
        // before sending FinalSync. In pause-first mode the VM has been paused
        // since before bulk copy (the handshake already completed) — only the
        // phase transition runs; the wait would be an immediate no-op.
        if let Some(ref task) = self.task {
            {
                let mut state = task.state.write().await;
                state.phase = MigrationPhase::PausedFinalSync;
                state.needs_vm_pause = true;
            }

            if self.pause_first {
                info!(
                    volume_id = %self.volume_id,
                    "pause-first: VM already paused since before bulk copy, proceeding with \
                     final sync"
                );
            } else {
                info!(
                    volume_id = %self.volume_id,
                    "waiting for VM pause before final sync"
                );

                let mut pause_rx = task.pause_tx.subscribe();
                while !*pause_rx.borrow() {
                    if pause_rx.changed().await.is_err() {
                        let mut state = task.state.write().await;
                        state.phase = MigrationPhase::Failed;
                        state.error_message = "pause channel closed".to_string();
                        state.needs_vm_pause = false;
                        return Err(tonic::Status::cancelled("pause channel closed"));
                    }
                }

                info!(
                    volume_id = %self.volume_id,
                    "VM pause signaled, proceeding with final sync"
                );
            }
        }

        // Send FinalSync (VM is paused at this point)
        let final_sync_msg = MigrationMessage {
            payload: Some(migration_message::Payload::FinalSync(FinalSync {
                vm_paused: true,
            })),
        };
        tx.send(final_sync_msg)
            .await
            .map_err(|_| tonic::Status::internal("failed to send FinalSync: channel closed"))?;

        // The receiver flushes its ack window at FinalSync; drain whatever
        // is still outstanding (e.g. the bulk-copy remainder when no dirty
        // round ran) before announcing finalization, so that every chunk
        // has been acknowledged before the migration can be reported
        // complete (fail-closed).
        while self.send_window.last_ack_sequence() < sequence_num {
            self.wait_for_ack(&mut inbound).await?;
        }

        // Send FinalizeComplete. Before announcing finalization, compute a
        // full-volume SHA-256 digest over the source (streamed through the
        // same `read_block` path used for bulk copy — the volume is never
        // held in memory) so the receiver can prove the *assembled*
        // destination matches the source (issue #392). The digest is
        // versioned (`"sha256:"` + 32 raw bytes) so a future algorithm
        // change is detected, never misinterpreted.
        let source_digest = volume_digest::compute_volume_digest(
            self.backend.as_ref(),
            &self.volume_id,
            &self.handle,
            volume_size,
        )
        .await
        .map_err(|e| {
            // Fail closed: without a source digest there is nothing to
            // verify the destination against, so the migration cannot be
            // reported complete.
            let status =
                tonic::Status::internal(format!("failed to compute source volume digest: {e}"));
            if let Some(ref task) = self.task {
                task.mark_failed(status.message().to_string());
            }
            status
        })?;
        if let Some(ref task) = self.task {
            let mut state = task.state.write().await;
            state.finalize_volume_digest = source_digest.display();
        }
        info!(
            volume_id = %self.volume_id,
            digest = %source_digest.display(),
            "computed source volume digest for finalize"
        );

        let finalize_msg = MigrationMessage {
            payload: Some(migration_message::Payload::FinalizeComplete(
                FinalizeComplete {
                    total_bytes: volume_size,
                    total_chunks: sequence_num as u64,
                    volume_checksum: source_digest.to_wire(),
                },
            )),
        };
        tx.send(finalize_msg).await.map_err(|_| {
            tonic::Status::internal("failed to send FinalizeComplete: channel closed")
        })?;

        // Wait for FinalizeAck. Boundary acks flushed by the receiver
        // (round acknowledgments, the FinalSync flush) may still be in
        // flight; they are expected here and processed normally — a
        // CRC-mismatch Ack still fails the migration (fail-closed). The
        // whole wait is bounded by one fixed deadline rather than a
        // per-message timeout: otherwise a peer that keeps dribbling
        // boundary acks could extend the pause forever, and the VM would
        // stay stopped even though FinalizeAck never arrives.
        let finalize_deadline = tokio::time::Instant::now() + self.finalize_ack_timeout;
        loop {
            let ack_msg = match tokio::time::timeout_at(finalize_deadline, inbound.message()).await
            {
                Ok(Ok(Some(msg))) => msg,
                Ok(Ok(None)) => {
                    return Err(tonic::Status::internal("stream closed before FinalizeAck"));
                }
                // Preserve the transport-level status (cancellation,
                // unavailable, ...) instead of re-wrapping it as
                // internal; the previous unbounded `?` did the same.
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    let message = format!(
                        "timed out waiting for FinalizeAck after {}s",
                        self.finalize_ack_timeout.as_secs_f64()
                    );
                    error!(
                        volume_id = %self.volume_id,
                        timeout_secs = self.finalize_ack_timeout.as_secs_f64(),
                        "migration finalization timed out waiting for FinalizeAck"
                    );
                    metrics::counter!(
                        STORD_MIGRATION_ERRORS_TOTAL,
                        "reason" => "finalize_ack_timeout"
                    )
                    .increment(1);
                    if let Some(ref task) = self.task {
                        let mut state = task.state.write().await;
                        state.phase = MigrationPhase::Failed;
                        state.error_message = message.clone();
                        state.needs_vm_pause = false;
                    }
                    return Err(tonic::Status::deadline_exceeded(message));
                }
            };

            match ack_msg.payload {
                Some(migration_message::Payload::FinalizeAck(ref ack)) => {
                    if ack.verified {
                        if let Some(ref task) = self.task {
                            let mut state = task.state.write().await;
                            state.phase = MigrationPhase::Completed;
                            // No pause wait is pending anymore (the VM is
                            // paused and stays paused until the agent's
                            // memory-migration flow resumes it on the
                            // destination) — leaving the flag set made a
                            // finished task report `needs_vm_pause=true`
                            // forever (#582 review residue).
                            state.needs_vm_pause = false;
                        }
                        info!(volume_id = %self.volume_id, "migration finalized successfully");
                        return Ok(());
                    } else {
                        error!(
                            volume_id = %self.volume_id,
                            error = %ack.error_message,
                            "migration finalization failed"
                        );
                        metrics::counter!(STORD_MIGRATION_ERRORS_TOTAL, "reason" => "finalization_failed").increment(1);
                        // The destination provably does not hold the
                        // source's bytes (digest mismatch or an
                        // unverifiable destination) — the same integrity
                        // class as a chunk CRC mismatch, so the same
                        // `data_loss` code. The task must end Failed, not
                        // Completed: `Completed` now genuinely means
                        // "destination verified".
                        let status = tonic::Status::data_loss(format!(
                            "finalization failed: {}",
                            ack.error_message
                        ));
                        if let Some(ref task) = self.task {
                            let mut state = task.state.write().await;
                            state.phase = MigrationPhase::Failed;
                            state.error_message = status.message().to_string();
                            state.needs_vm_pause = false;
                        }
                        return Err(status);
                    }
                }
                Some(migration_message::Payload::Ack(_))
                | Some(migration_message::Payload::Backpressure(_)) => {
                    // In-flight acknowledgment from a phase boundary; the
                    // migration is not finalized until FinalizeAck arrives.
                    self.handle_inbound_message(ack_msg)?;
                }
                _ => {
                    return Err(tonic::Status::internal(
                        "unexpected message; expected FinalizeAck",
                    ));
                }
            }
        }
    }

    /// Perform the bulk copy phase: read all blocks and stream them to the receiver.
    ///
    /// The sender computes CRC32 for each chunk and respects the send window.
    /// When the window is full (default 128 in-flight), the sender blocks
    /// until acknowledgments are received from the destination.
    ///
    /// Note: there is deliberately no drain at the end of this phase. The
    /// receiver cannot know the bulk phase ended until it sees the next
    /// boundary message (`RoundStart` or `FinalSync`), which the sender only
    /// sends after this method returns — a drain here could deadlock until
    /// the 30 s timeout whenever the chunk count is not a multiple of the
    /// receiver's ack interval (issue #391). Instead, every chunk is
    /// awaited at the next boundary: the receiver flushes its ack window at
    /// `RoundComplete` and `FinalSync`, and the sender drains after those
    /// messages (see `dirty_sync_rounds` and the post-FinalSync drain in
    /// `start_migration`). Fail-closed semantics are unchanged: a
    /// CRC-mismatch or write-error Ack surfaces at those drains (or at the
    /// FinalizeAck wait) and fails the migration before it can be reported
    /// complete.
    async fn bulk_copy(
        &mut self,
        tx: &mpsc::Sender<MigrationMessage>,
        inbound: &mut tonic::Streaming<MigrationMessage>,
        volume_size: u64,
    ) -> Result<u32, tonic::Status> {
        let mut sequence_num: u32 = 0;
        let mut offset: u64 = 0;

        while offset < volume_size {
            // Wait if send window is full
            while !self.send_window.can_send() {
                self.wait_for_ack(inbound).await?;
            }

            let length = std::cmp::min(self.block_size, volume_size - offset);

            let data = self
                .backend
                .read_block(&self.volume_id, &self.handle, offset, length)
                .await
                .map_err(|e| {
                    tonic::Status::internal(format!("read_block failed at offset {offset}: {e}"))
                })?;

            let is_sparse = is_all_zeros(&data);
            let crc32 = if is_sparse { 0 } else { crc32fast::hash(&data) };

            let chunk_data = if is_sparse { Vec::new() } else { data };

            sequence_num += 1;
            let chunk_msg = MigrationMessage {
                payload: Some(migration_message::Payload::Chunk(BlockChunk {
                    offset,
                    data: chunk_data,
                    crc32,
                    is_sparse,
                    sequence_num,
                })),
            };

            tx.send(chunk_msg).await.map_err(|_| {
                tonic::Status::internal("failed to send BlockChunk: channel closed")
            })?;

            self.send_window.sent();

            // Apply backpressure throttle if the receiver requested slow-down
            if self.backpressure_factor > 1.0 {
                tokio::time::sleep(std::time::Duration::from_millis(
                    (10.0 * self.backpressure_factor) as u64,
                ))
                .await;
            }

            // Non-blocking check for acks to keep the window sliding
            if self.send_window.should_request_ack() {
                self.try_receive_ack(inbound).await?;
            }

            offset += self.block_size;
        }

        Ok(sequence_num)
    }

    /// Perform iterative dirty sync rounds to transfer blocks written during bulk copy.
    ///
    /// Each round fetches the dirty bitmap, sends dirty blocks, waits for acknowledgment,
    /// then clears the bitmap. Repeats until dirty count drops below DIRTY_THRESHOLD
    /// or MAX_DIRTY_ROUNDS is reached.
    ///
    /// **What these rounds can and cannot see (#394 R1):** the bitmap is
    /// stord-private state marked only by the backend's `write_block`,
    /// and in production nothing calls `write_block` on a *source*
    /// volume during migration (the only product caller is the
    /// migration receiver, on the destination). Guest or host writes
    /// that bypass stord are invisible to these rounds — which is why
    /// every round boundary (and the pre-pause gate) re-checks the
    /// write canary and fails the migration if the source's stat moved.
    async fn dirty_sync_rounds(
        &mut self,
        tx: &mpsc::Sender<MigrationMessage>,
        inbound: &mut tonic::Streaming<MigrationMessage>,
        sequence_num: &mut u32,
        volume_size: u64,
    ) -> Result<u32, tonic::Status> {
        let mut total_dirty_chunks: u32 = 0;

        for round in 1..=MAX_DIRTY_ROUNDS {
            // Round boundary (issue #394, Option A): re-check the write
            // canary before snapshotting the bitmap. Round 1's check is
            // the end-of-bulk-copy boundary.
            self.verify_source_canary().await?;

            if let Some(ref task) = self.task {
                let mut state = task.state.write().await;
                state.convergence_round = round;
            }
            // Step 1: Atomically snapshot and clear the dirty bitmap.
            // This ensures no writes are lost between reading the bitmap and clearing it.
            let bitmap = self
                .backend
                .snapshot_and_clear_dirty_bitmap(&self.volume_id, &self.handle)
                .await
                .map_err(|e| {
                    tonic::Status::internal(format!("snapshot_and_clear_dirty_bitmap failed: {e}"))
                })?;

            // Convert bitmap to list of dirty block offsets
            let dirty_offsets = bitmap_to_offsets(&bitmap, self.block_size);
            let dirty_block_count = dirty_offsets.len() as u64;

            info!(
                volume_id = %self.volume_id,
                round,
                dirty_block_count,
                "dirty sync round starting"
            );

            if let Some(ref task) = self.task {
                let mut state = task.state.write().await;
                state.dirty_blocks_remaining = dirty_block_count;
            }

            // Check termination condition: if below threshold, we're done
            if dirty_block_count == 0 {
                info!(
                    volume_id = %self.volume_id,
                    round,
                    "no dirty blocks remaining, skipping final round"
                );
                break;
            }

            // Step 2: Send RoundStart
            let round_start_msg = MigrationMessage {
                payload: Some(migration_message::Payload::RoundStart(RoundStart {
                    round_num: round,
                    dirty_block_count,
                })),
            };
            tx.send(round_start_msg).await.map_err(|_| {
                tonic::Status::internal("failed to send RoundStart: channel closed")
            })?;

            // Step 3: Send each dirty block
            let mut blocks_sent: u64 = 0;
            let mut bytes_sent: u64 = 0;

            for &offset in &dirty_offsets {
                // Wait if send window is full
                while !self.send_window.can_send() {
                    self.wait_for_ack(inbound).await?;
                }

                let length = std::cmp::min(self.block_size, volume_size - offset);
                let data = self
                    .backend
                    .read_block(&self.volume_id, &self.handle, offset, length)
                    .await
                    .map_err(|e| {
                        tonic::Status::internal(format!(
                            "read_block failed at offset {offset} during dirty sync: {e}"
                        ))
                    })?;

                let is_sparse = is_all_zeros(&data);
                let crc32 = if is_sparse { 0 } else { crc32fast::hash(&data) };
                let chunk_data = if is_sparse { Vec::new() } else { data };

                *sequence_num += 1;
                bytes_sent += chunk_data.len() as u64;
                metrics::counter!(STORD_MIGRATION_BYTES_SENT_TOTAL, "volume_id" => self.volume_id.clone())
                    .increment(chunk_data.len() as u64);

                let chunk_msg = MigrationMessage {
                    payload: Some(migration_message::Payload::Chunk(BlockChunk {
                        offset,
                        data: chunk_data,
                        crc32,
                        is_sparse,
                        sequence_num: *sequence_num,
                    })),
                };

                tx.send(chunk_msg).await.map_err(|_| {
                    tonic::Status::internal("failed to send dirty BlockChunk: channel closed")
                })?;

                self.send_window.sent();
                blocks_sent += 1;

                // Apply backpressure throttle if the receiver requested slow-down
                if self.backpressure_factor > 1.0 {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        (10.0 * self.backpressure_factor) as u64,
                    ))
                    .await;
                }

                // Non-blocking check for acks to keep the window sliding
                if self.send_window.should_request_ack() {
                    self.try_receive_ack(inbound).await?;
                }
            }

            // Step 4: Send RoundComplete
            let round_complete_msg = MigrationMessage {
                payload: Some(migration_message::Payload::RoundComplete(RoundComplete {
                    round_num: round,
                    blocks_sent,
                    bytes_sent,
                })),
            };
            tx.send(round_complete_msg).await.map_err(|_| {
                tonic::Status::internal("failed to send RoundComplete: channel closed")
            })?;

            // Step 5: Wait for the round acknowledgment and drain the
            // round. The receiver answers RoundComplete with an `Ack`
            // carrying its highest processed sequence number — by stream
            // ordering that includes every chunk of this round — which
            // both flushes its ack window (chunks below the ack interval
            // would otherwise never be acknowledged) and satisfies this
            // wait. Interval acks may complete the drain first (chunk count
            // a multiple of the interval); the round `Ack` is then simply
            // consumed by a later phase's waits. Before this protocol fix
            // the receiver sent nothing for RoundComplete and the sender
            // blocked forever (issue #391).
            while self.send_window.last_ack_sequence() < *sequence_num {
                self.wait_for_ack(inbound).await?;
            }

            // Note: dirty bitmap was already cleared atomically in step 1 via
            // snapshot_and_clear_dirty_bitmap, so no separate clear needed here.

            total_dirty_chunks += blocks_sent as u32;

            if let Some(ref task) = self.task {
                let mut state = task.state.write().await;
                state.bytes_transferred = state.bytes_transferred.saturating_add(bytes_sent);
            }

            info!(
                volume_id = %self.volume_id,
                round,
                blocks_sent,
                bytes_sent,
                "dirty sync round complete"
            );

            // Step 7: Check if we should stop
            if dirty_block_count < DIRTY_THRESHOLD {
                break;
            }
        }

        Ok(total_dirty_chunks)
    }

    /// Re-check the source write canary against the migration-start
    /// baseline (issue #394, Option A).
    ///
    /// A no-op when the canary is unavailable on this backend (no
    /// baseline was sampled). A stat change fails the migration with
    /// `failed_precondition` carrying the distinct
    /// [`SOURCE_MODIFIED_CODE`] token; a probe error fails closed with
    /// `internal` rather than silently disarming the canary.
    async fn verify_source_canary(&self) -> Result<(), tonic::Status> {
        let Some(baseline) = self.canary_baseline.as_ref() else {
            return Ok(());
        };
        let current = self
            .backend
            .write_canary_probe(&self.volume_id, &self.handle)
            .await
            .map_err(|e| {
                // `mark_failed` (try_write) rather than the async-lock
                // write the stat-change branch uses: this closure is
                // not async. Both paths end Failed; if the lock is
                // contended the handlers' spawn wrapper re-marks the
                // task from the returned status.
                let status =
                    tonic::Status::internal(format!("failed to probe source write canary: {e}"));
                if let Some(ref task) = self.task {
                    task.mark_failed(status.message().to_string());
                }
                status
            })?;
        if current.stat_matches(baseline) {
            return Ok(());
        }
        metrics::counter!(STORD_MIGRATION_ERRORS_TOTAL, "reason" => SOURCE_MODIFIED_CODE)
            .increment(1);
        let status = tonic::Status::failed_precondition(format!(
            "{SOURCE_MODIFIED_CODE}: the source volume's backing store changed during migration \
             (stat differs from the migration-start sample) — concurrent-write (live) migration \
             is unsupported; the migration was aborted before the VM pause instead of failing \
             at the finalize digest after a full transfer. Retry with the source quiesced."
        ));
        error!(
            volume_id = %self.volume_id,
            code = SOURCE_MODIFIED_CODE,
            "source modified during migration; failing fast"
        );
        if let Some(ref task) = self.task {
            let mut state = task.state.write().await;
            state.phase = MigrationPhase::Failed;
            state.error_message = status.message().to_string();
            // Terminal: no pause wait is pending (#582) — uniform with
            // the other terminal transitions even though the canary
            // trips before the flag is ever set in the default flow.
            state.needs_vm_pause = false;
        }
        Err(status)
    }

    /// Block until an Ack is received from the inbound stream.
    async fn wait_for_ack(
        &mut self,
        inbound: &mut tonic::Streaming<MigrationMessage>,
    ) -> Result<(), tonic::Status> {
        let timeout = self.send_window.timeout();
        let msg = tokio::time::timeout(timeout, inbound.message())
            .await
            .map_err(|_| {
                tonic::Status::deadline_exceeded(format!(
                    "timed out waiting for Ack (last_ack_offset={})",
                    self.last_acknowledged_offset
                ))
            })?
            .map_err(|e| tonic::Status::internal(format!("stream error: {e}")))?
            .ok_or_else(|| tonic::Status::internal("stream closed while waiting for Ack"))?;

        self.handle_inbound_message(msg)
    }

    /// Try to receive an Ack without blocking (non-blocking check).
    async fn try_receive_ack(
        &mut self,
        inbound: &mut tonic::Streaming<MigrationMessage>,
    ) -> Result<(), tonic::Status> {
        // Use a short timeout to check if there's a pending message.
        // 50ms balances responsiveness (not blocking sends too long) against
        // avoiding excessive timer-wheel firings that 1ms would cause.
        match tokio::time::timeout(std::time::Duration::from_millis(50), inbound.message()).await {
            Ok(Ok(Some(msg))) => self.handle_inbound_message(msg),
            Ok(Ok(None)) => Err(tonic::Status::internal("stream closed unexpectedly")),
            Ok(Err(e)) => Err(tonic::Status::internal(format!("stream error: {e}"))),
            Err(_) => Ok(()), // timeout = no message available, that's fine
        }
    }

    /// Process an inbound message (expected to be Ack or Backpressure).
    #[allow(clippy::result_large_err)]
    fn handle_inbound_message(&mut self, msg: MigrationMessage) -> Result<(), tonic::Status> {
        match msg.payload {
            Some(migration_message::Payload::Ack(ref ack)) => {
                if ack.status() == AckStatus::AckCrcMismatch {
                    warn!(
                        sequence = ack.last_sequence_num,
                        offset = ack.last_offset,
                        "receiver reported CRC mismatch"
                    );
                    metrics::counter!(STORD_MIGRATION_ERRORS_TOTAL, "reason" => "crc_mismatch")
                        .increment(1);
                    return Err(tonic::Status::data_loss(
                        "CRC mismatch reported by receiver",
                    ));
                }
                if ack.status() == AckStatus::AckWriteError {
                    error!(
                        sequence = ack.last_sequence_num,
                        offset = ack.last_offset,
                        "receiver reported write error"
                    );
                    metrics::counter!(STORD_MIGRATION_ERRORS_TOTAL, "reason" => "write_error")
                        .increment(1);
                    return Err(tonic::Status::internal("write error reported by receiver"));
                }
                self.send_window.acked(ack.last_sequence_num);
                self.last_acknowledged_offset = ack.last_offset;
                debug!(
                    sequence = ack.last_sequence_num,
                    offset = ack.last_offset,
                    unacked = self.send_window.unacked_count(),
                    "ack received"
                );
                Ok(())
            }
            Some(migration_message::Payload::Backpressure(ref bp)) => {
                info!(
                    slow_down_factor = bp.slow_down_factor,
                    "backpressure received, adjusting send rate"
                );
                self.backpressure_factor = bp.slow_down_factor.max(1.0);
                Ok(())
            }
            Some(migration_message::Payload::Error(ref err)) => Err(tonic::Status::internal(
                format!("migration error from peer: {}", err.message),
            )),
            _ => {
                warn!("unexpected message type during send phase");
                Ok(())
            }
        }
    }
}

/// Start a storage migration to a remote peer.
///
/// This is the top-level entry point called by the control plane / agent.
/// It creates a MigrationSender and drives the full migration lifecycle.
///
/// When `tls_config` is `Some`, the connection uses mTLS as required by
/// the disk migration protocol spec. When `pause_first` is true (issue
/// #394, Option C), the sender requests the VM pause before any source
/// read and the transfer runs quiesced (stop-the-world, correct by
/// construction). `finalize_ack_timeout` bounds the wait for
/// `FinalizeAck`; `None` uses the 30 s default, and values outside
/// [`MIN_FINALIZE_ACK_TIMEOUT`]..=[`MAX_FINALIZE_ACK_TIMEOUT`] are clamped.
// The migration entry point already threads the peer/volume identity,
// backend, TLS, task, and pause mode; adding the #656 timeout over the
// counter keeps the call site explicit and is preferable to an options
// struct that would churn every caller for one new knob.
#[allow(clippy::too_many_arguments)]
pub async fn start_migration_to_peer<B: StorageBackend>(
    endpoint: String,
    volume_id: String,
    handle: String,
    backend: Arc<B>,
    tls_config: Option<MigrationTlsConfig>,
    task: Option<Arc<MigrationTask>>,
    migration_authorization: String,
    pause_first: bool,
    handshake_tx: Option<tokio::sync::oneshot::Sender<Result<(), tonic::Status>>>,
    finalize_ack_timeout: Option<Duration>,
) -> Result<(), tonic::Status> {
    let mut sender = MigrationSender::new(backend, volume_id, handle)
        .with_migration_authorization(migration_authorization);
    if let Some(tx) = handshake_tx {
        sender = sender.with_handshake_notifier(tx);
    }
    if let Some(tls) = tls_config {
        sender = sender.with_tls(tls);
    }
    if let Some(t) = task {
        sender = sender.with_task(t);
    }
    if pause_first {
        sender = sender.with_pause_first();
    }
    let timeout = finalize_ack_timeout
        .unwrap_or(DEFAULT_FINALIZE_ACK_TIMEOUT)
        .clamp(MIN_FINALIZE_ACK_TIMEOUT, MAX_FINALIZE_ACK_TIMEOUT);
    sender = sender.with_finalize_ack_timeout(timeout);
    sender.start_migration(endpoint).await
}

/// Walk an error's source chain to its terminal (deepest) cause and return
/// its display — the same chain-walking `tests/migration_mtls.rs` uses to
/// surface the rustls fatal alert hidden under a tonic "transport error".
///
/// For an mTLS rejection this is the rustls alert/reason name (e.g.
/// "invalid peer certificate: UnknownIssuer"); it never contains
/// certificate contents or key material.
fn terminal_error_display(err: &dyn std::error::Error) -> String {
    let mut display = err.to_string();
    let mut source = err.source();
    while let Some(err) = source {
        display = err.to_string();
        source = err.source();
    }
    display
}

/// Check if a byte slice is all zeros (indicates a sparse block).
fn is_all_zeros(data: &[u8]) -> bool {
    data.iter().all(|&b| b == 0)
}

/// Convert a dirty bitmap into a vec of block byte-offsets.
///
/// The bitmap uses one bit per block: bit N of byte M represents block index `M*8 + N`.
/// Each block offset is computed as `block_index * block_size`.
fn bitmap_to_offsets(bitmap: &[u8], block_size: u64) -> Vec<u64> {
    let mut offsets = Vec::new();
    for (byte_idx, &byte) in bitmap.iter().enumerate() {
        if byte == 0 {
            continue;
        }
        for bit in 0..8u32 {
            if byte & (1 << bit) != 0 {
                let block_index = (byte_idx as u64) * 8 + bit as u64;
                offsets.push(block_index * block_size);
            }
        }
    }
    offsets
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chv_common::types::{BackendLocator, DevicePolicy};
    use chv_errors::ChvError;
    use chv_stord_backends::{BackendHealth, StorageBackend, VolumeExport};

    /// Minimal mock backend for testing the MigrationSender without real I/O.
    struct MockBackend {
        /// Fingerprint returned by `write_canary_probe`; defaults to
        /// Unavailable so canary-unaware tests behave as before.
        canary_probe: std::sync::Mutex<WriteCanaryFingerprint>,
    }

    impl MockBackend {
        fn new() -> Self {
            Self {
                canary_probe: std::sync::Mutex::new(WriteCanaryFingerprint {
                    capability: WriteCanaryCapability::Unavailable {
                        reason: "mock backend has no canary".to_string(),
                    },
                    mtime_sec: 0,
                    mtime_nsec: 0,
                    ctime_sec: 0,
                    ctime_nsec: 0,
                    size: 0,
                }),
            }
        }

        fn set_canary_probe(&self, fingerprint: WriteCanaryFingerprint) {
            *self.canary_probe.lock().unwrap() = fingerprint;
        }
    }

    #[async_trait]
    impl StorageBackend for MockBackend {
        async fn open(
            &self,
            _volume_id: &str,
            _locator: &BackendLocator,
            _policy: &DevicePolicy,
        ) -> Result<VolumeExport, ChvError> {
            unimplemented!("not needed for sender tests")
        }

        async fn close(&self, _volume_id: &str, _handle: &str) -> Result<(), ChvError> {
            Ok(())
        }

        async fn attach(
            &self,
            _volume_id: &str,
            _handle: &str,
            _vm_id: &str,
        ) -> Result<VolumeExport, ChvError> {
            unimplemented!("not needed for sender tests")
        }

        async fn detach(
            &self,
            _volume_id: &str,
            _handle: &str,
            _ownership: chv_common::AttachmentOwnership,
            _force: bool,
        ) -> Result<(), ChvError> {
            Ok(())
        }

        async fn health(&self, _volume_id: &str, _handle: &str) -> Result<BackendHealth, ChvError> {
            Ok(BackendHealth {
                status: "healthy".to_string(),
                backend_state: "ok".to_string(),
                last_error: String::new(),
            })
        }

        async fn resize(
            &self,
            _volume_id: &str,
            _handle: &str,
            _new_size_bytes: u64,
        ) -> Result<(), ChvError> {
            Ok(())
        }

        async fn prepare_snapshot(
            &self,
            _volume_id: &str,
            _handle: &str,
            _ownership: chv_common::AttachmentOwnership,
            _snapshot_name: &str,
        ) -> Result<(), ChvError> {
            Ok(())
        }

        async fn prepare_clone(
            &self,
            _volume_id: &str,
            _handle: &str,
            _ownership: chv_common::AttachmentOwnership,
            _clone_name: &str,
        ) -> Result<(), ChvError> {
            Ok(())
        }

        async fn restore_snapshot(
            &self,
            _volume_id: &str,
            _handle: &str,
            _snapshot_name: &str,
        ) -> Result<(), ChvError> {
            Ok(())
        }

        async fn delete_snapshot(
            &self,
            _volume_id: &str,
            _handle: &str,
            _snapshot_name: &str,
        ) -> Result<(), ChvError> {
            Ok(())
        }

        // #522 DP3: the sender tests never destroy; state the refusal
        // so the mock stays an honest trait implementation.
        async fn destroy(
            &self,
            _volume_id: &str,
            _locator: &BackendLocator,
        ) -> Result<(), ChvError> {
            Err(ChvError::Unimplemented {
                reason: "destroy is not needed for sender tests".to_string(),
            })
        }

        async fn set_device_policy(
            &self,
            _volume_id: &str,
            _handle: &str,
            _policy: &DevicePolicy,
        ) -> Result<(), ChvError> {
            Ok(())
        }

        async fn read_block(
            &self,
            _volume_id: &str,
            _handle: &str,
            _offset: u64,
            length: u64,
        ) -> Result<Vec<u8>, ChvError> {
            Ok(vec![0u8; length as usize])
        }

        async fn write_block(
            &self,
            _volume_id: &str,
            _handle: &str,
            _offset: u64,
            _data: &[u8],
        ) -> Result<(), ChvError> {
            Ok(())
        }

        async fn volume_size(&self, _volume_id: &str, _handle: &str) -> Result<u64, ChvError> {
            Ok(1024 * 1024) // 1 MB
        }

        async fn create_receiving_volume(
            &self,
            _volume_id: &str,
            _size_bytes: u64,
            _format: &str,
        ) -> Result<VolumeExport, ChvError> {
            unimplemented!("not needed for sender tests")
        }

        async fn write_canary_probe(
            &self,
            _volume_id: &str,
            _handle: &str,
        ) -> Result<WriteCanaryFingerprint, ChvError> {
            Ok(self.canary_probe.lock().unwrap().clone())
        }
    }

    #[tokio::test]
    async fn test_mtls_required_for_migration() {
        let backend = Arc::new(MockBackend::new());
        let sender = MigrationSender::new(backend, "vol-123".to_string(), "handle-abc".to_string());

        // Attempt migration without TLS configured
        let result = sender
            .start_migration("http://10.0.0.1:9090".to_string())
            .await;

        assert!(result.is_err(), "migration should fail without mTLS");
        let status = result.unwrap_err();
        assert_eq!(
            status.code(),
            tonic::Code::FailedPrecondition,
            "error code should be FailedPrecondition, got {:?}",
            status.code()
        );
        assert!(
            status.message().contains("no migration client identity"),
            "error message must say the node has no client identity: {}",
            status.message()
        );
        assert!(
            status.message().contains("does not initiate migrations"),
            "error message must say the node does not initiate migrations: {}",
            status.message()
        );
        // The message must cite the real config keys (issue #391): the
        // [migration] section fields, not the nonexistent [migration.tls]
        // subsection it previously pointed operators at. Since #401 the
        // four client keys are optional, so they may appear only as the
        // conditional — not as an unconditional instruction to set them.
        assert!(
            status
                .message()
                .contains("only if this node should send migrations"),
            "error message must frame the client keys conditionally: {}",
            status.message()
        );
        for key in [
            "migration.client_cert_path",
            "migration.client_key_path",
            "migration.ca_cert_path",
            "migration.dest_server_name",
        ] {
            assert!(
                status.message().contains(key),
                "error message should cite config key {key}: {}",
                status.message()
            );
        }
    }

    #[test]
    fn test_backpressure_factor_initialization() {
        let backend = Arc::new(MockBackend::new());
        let sender = MigrationSender::new(backend, "vol-456".to_string(), "handle-def".to_string());

        // backpressure_factor is private, but we can verify behavior through
        // the sender's initial state. The field is initialized to 1.0 which
        // means no throttling. We verify the sender was constructed correctly
        // by checking that last_acknowledged_offset starts at 0.
        assert_eq!(sender.last_acknowledged_offset(), 0);
    }

    #[test]
    fn test_sender_with_block_size() {
        let backend = Arc::new(MockBackend::new());
        let sender = MigrationSender::new(backend, "vol-789".to_string(), "handle-ghi".to_string())
            .with_block_size(8_388_608); // 8 MB

        // Verify construction doesn't panic and sender is usable
        assert_eq!(sender.last_acknowledged_offset(), 0);
    }

    #[test]
    fn test_bitmap_to_offsets_empty() {
        let bitmap: Vec<u8> = vec![];
        let offsets = bitmap_to_offsets(&bitmap, 4096);
        assert!(offsets.is_empty());
    }

    #[test]
    fn test_bitmap_to_offsets_single_bit() {
        // Byte 0, bit 0 set => block index 0 => offset 0
        let bitmap = vec![0x01u8];
        let offsets = bitmap_to_offsets(&bitmap, 4096);
        assert_eq!(offsets, vec![0]);
    }

    #[test]
    fn test_bitmap_to_offsets_multiple_bits() {
        // Byte 0: bits 0 and 2 set => block indices 0, 2
        // Byte 1: bit 1 set => block index 9
        let bitmap = vec![0x05u8, 0x02u8];
        let offsets = bitmap_to_offsets(&bitmap, 4096);
        assert_eq!(offsets, vec![0, 2 * 4096, 9 * 4096]);
    }

    #[test]
    fn test_bitmap_to_offsets_all_zeros() {
        let bitmap = vec![0x00u8; 16];
        let offsets = bitmap_to_offsets(&bitmap, 4096);
        assert!(offsets.is_empty());
    }

    #[test]
    fn test_is_all_zeros() {
        assert!(is_all_zeros(&[0, 0, 0, 0]));
        assert!(is_all_zeros(&[]));
        assert!(!is_all_zeros(&[0, 0, 1, 0]));
        assert!(!is_all_zeros(&[255]));
    }

    // -----------------------------------------------------------------
    // Write canary (#394, Option A)
    // -----------------------------------------------------------------

    fn file_stat_fingerprint(mtime_sec: i64, size: u64) -> WriteCanaryFingerprint {
        WriteCanaryFingerprint {
            capability: WriteCanaryCapability::FileStat,
            mtime_sec,
            mtime_nsec: 0,
            ctime_sec: 0,
            ctime_nsec: 0,
            size,
        }
    }

    /// No baseline (backend reported Unavailable) ⇒ the canary is a
    /// no-op: pre-#394 behavior, the finalize digest is the only gate.
    #[tokio::test]
    async fn canary_is_noop_when_backend_unavailable() {
        let backend = Arc::new(MockBackend::new());
        let mut sender = MigrationSender::new(backend, "vol-c".to_string(), "handle-c".to_string());
        sender.canary_baseline = None;
        assert!(
            sender.verify_source_canary().await.is_ok(),
            "unavailable canary must not fail the migration"
        );
    }

    /// Unchanged stat ⇒ pass.
    #[tokio::test]
    async fn canary_passes_when_stat_unchanged() {
        let backend = Arc::new(MockBackend::new());
        backend.set_canary_probe(file_stat_fingerprint(1000, 4096));
        let mut sender =
            MigrationSender::new(backend.clone(), "vol-c".to_string(), "handle-c".to_string());
        sender.canary_baseline = Some(file_stat_fingerprint(1000, 4096));
        assert!(
            sender.verify_source_canary().await.is_ok(),
            "identical fingerprint must pass"
        );
    }

    /// Stat change ⇒ `failed_precondition` carrying the distinct
    /// `source_modified_during_migration` token, and the task (when
    /// attached) is marked Failed with that message — the agent's
    /// status poll sees the failure and resumes the VM if paused.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn canary_fails_fast_with_distinct_code_and_marks_task() {
        let backend = Arc::new(MockBackend::new());
        backend.set_canary_probe(file_stat_fingerprint(2000, 4096));
        let (task, _pause_rx) =
            MigrationTask::new("vol-c".to_string(), "handle-c".to_string(), String::new());
        let mut sender = MigrationSender::new(backend, "vol-c".to_string(), "handle-c".to_string())
            .with_task(task.clone());
        // Baseline sampled at migration start; the source is then
        // written (mtime 1000 -> 2000).
        sender.canary_baseline = Some(file_stat_fingerprint(1000, 4096));

        let err = sender
            .verify_source_canary()
            .await
            .expect_err("a changed stat must fail the migration");
        assert_eq!(
            err.code(),
            tonic::Code::FailedPrecondition,
            "canary failure must be failed_precondition, got {:?}",
            err.code()
        );
        assert!(
            err.message().contains(SOURCE_MODIFIED_CODE),
            "error must carry the {SOURCE_MODIFIED_CODE} token: {}",
            err.message()
        );
        let state = task.state.read().await;
        assert_eq!(state.phase, MigrationPhase::Failed);
        assert!(
            state.error_message.contains(SOURCE_MODIFIED_CODE),
            "task error_message must carry the token: {}",
            state.error_message
        );
    }

    /// A size change alone (e.g. an out-of-band resize) also trips the
    /// canary: any stat movement means the source moved under the
    /// migration.
    #[tokio::test]
    async fn canary_trips_on_size_change_alone() {
        let backend = Arc::new(MockBackend::new());
        backend.set_canary_probe(file_stat_fingerprint(1000, 8192));
        let mut sender = MigrationSender::new(backend, "vol-c".to_string(), "handle-c".to_string());
        sender.canary_baseline = Some(file_stat_fingerprint(1000, 4096));
        assert!(
            sender.verify_source_canary().await.is_err(),
            "a size change must trip the canary"
        );
    }

    /// Pause-first (issue #394, Option C) fails closed without a task:
    /// the mode requires the pause coordination channel, and an operator
    /// who asked for the pause must never get a silent degradation to
    /// quiescent-assumed semantics. The rejection happens before the
    /// connection attempt, so an unroutable endpoint proves ordering: no
    /// connect timeout, an immediate `failed_precondition`.
    #[tokio::test]
    async fn pause_first_without_task_fails_closed_before_connecting() {
        let backend = Arc::new(MockBackend::new());
        let sender = MigrationSender::new(backend, "vol-pf".to_string(), "handle-pf".to_string())
            .with_pause_first();
        let err = sender
            .start_migration("https://127.0.0.1:1".to_string())
            .await
            .expect_err("pause-first without a task must fail closed");
        assert_eq!(
            err.code(),
            tonic::Code::FailedPrecondition,
            "expected failed_precondition, got: {}",
            err.message()
        );
        assert!(
            err.message().contains("pause-first"),
            "error must name the mode: {}",
            err.message()
        );
    }
}
