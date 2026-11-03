use crate::migration::volume_digest;
use chv_stord_api::chv_stord_api::{
    migration_message, Ack, AckStatus, FinalizeAck, MigrationMessage,
};
use chv_stord_backends::StorageBackend;
use std::sync::Arc;
use tokio::sync::mpsc;
use tonic::Streaming;
use tracing::{debug, error, info, warn};

const DEFAULT_ACK_INTERVAL: u32 = 64;

/// Maximum time the receiver will wait for the next message from an
/// authorized peer before closing the stream. Without this, a peer that
/// completes authorization and then stalls (or vanishes without a TCP reset)
/// could pin its admission slot indefinitely. Bulk copy sends continuously,
/// so 5 minutes is far longer than any healthy inter-message gap; a stalled
/// stream is failed and its RAII reservation released.
pub const RECEIVER_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Drives the destination side of a storage migration.
///
/// The receiver is created when an incoming stream starts with InitMigration.
/// It creates a receiving volume, acknowledges readiness, then processes
/// incoming BlockChunk messages, writing data to the volume.
///
/// # Acknowledgment protocol
///
/// The receiver sends an `Ack` every `ack_interval` chunks while streaming,
/// and additionally at stream boundaries so the sender's per-phase drains
/// complete for *arbitrary* chunk counts (not just multiples of the
/// interval):
///
/// - `RoundComplete` is always answered with an `Ack` carrying the highest
///   sequence number processed so far. By stream ordering that equals the
///   sender's cumulative sequence number at round end, so this one message
///   is both the round acknowledgment and the ack-window flush for the
///   round (issue #391).
/// - `FinalSync` and `FinalizeComplete` flush any chunks not yet
///   acknowledged (e.g. the bulk-copy remainder when no dirty round ran).
///
/// # Finalize verification
///
/// On `FinalizeComplete` the receiver re-computes the sender's versioned
/// full-volume digest over the destination volume (after the final ack
/// flush, before `FinalizeAck`) and reports the outcome in
/// `FinalizeAck.verified`. Unparseable/unknown digest formats and
/// unreadable destinations fail closed (`verified = false`), so the
/// sender's `Completed` state genuinely means "destination verified"
/// (issue #392).
pub struct MigrationReceiver<B: StorageBackend> {
    backend: Arc<B>,
    volume_id: String,
    handle: String,
    size_bytes: u64,
    ack_interval: u32,
    blocks_received: u32,
    blocks_since_last_ack: u32,
    last_sequence_num: u32,
    /// Byte offset of the last processed chunk (carried by boundary acks).
    last_offset: u64,
}

impl<B: StorageBackend> MigrationReceiver<B> {
    /// Create a new receiver (called after InitMigration is parsed).
    pub fn new(backend: Arc<B>, volume_id: String, handle: String, size_bytes: u64) -> Self {
        Self {
            backend,
            volume_id,
            handle,
            size_bytes,
            ack_interval: DEFAULT_ACK_INTERVAL,
            blocks_received: 0,
            blocks_since_last_ack: 0,
            last_sequence_num: 0,
            last_offset: 0,
        }
    }

    /// Run the receiver loop: process incoming messages and write blocks.
    ///
    /// This is spawned as a task and communicates back via the `tx` channel.
    /// However the stream ends, the receiving volume is closed exactly once:
    /// `create_receiving_volume` acquired a backend reference (an iSCSI
    /// session ref) that must be released to keep the backend refcount
    /// balanced.
    pub async fn run(
        mut self,
        mut inbound: Streaming<MigrationMessage>,
        tx: mpsc::Sender<MigrationMessage>,
    ) -> Result<(), tonic::Status> {
        let result = self.run_inner(&mut inbound, &tx).await;
        if let Err(e) = self.backend.close(&self.volume_id, &self.handle).await {
            warn!(
                volume_id = %self.volume_id,
                error = %e,
                "failed to release receiving volume after migration stream ended"
            );
        }
        result
    }

    async fn run_inner(
        &mut self,
        inbound: &mut Streaming<MigrationMessage>,
        tx: &mpsc::Sender<MigrationMessage>,
    ) -> Result<(), tonic::Status> {
        loop {
            let msg = match tokio::time::timeout(RECEIVER_IDLE_TIMEOUT, inbound.message()).await {
                Ok(result) => match result? {
                    Some(m) => m,
                    None => {
                        info!(volume_id = %self.volume_id, "inbound stream closed");
                        break;
                    }
                },
                Err(_) => {
                    warn!(
                        volume_id = %self.volume_id,
                        idle_timeout_secs = RECEIVER_IDLE_TIMEOUT.as_secs(),
                        "migration receiver idle timeout: closing stalled authorized stream"
                    );
                    return Err(tonic::Status::deadline_exceeded(format!(
                        "migration receiver idle for {}s with no message from peer; \
                         closing stream and releasing its admission slot",
                        RECEIVER_IDLE_TIMEOUT.as_secs()
                    )));
                }
            };

            match msg.payload {
                Some(migration_message::Payload::Chunk(chunk)) => {
                    self.handle_chunk(&chunk, tx).await?;
                }
                Some(migration_message::Payload::RoundStart(ref rs)) => {
                    debug!(
                        round_num = rs.round_num,
                        dirty_blocks = rs.dirty_block_count,
                        "dirty sync round starting"
                    );
                }
                Some(migration_message::Payload::RoundComplete(ref rc)) => {
                    info!(
                        round_num = rc.round_num,
                        blocks_sent = rc.blocks_sent,
                        bytes_sent = rc.bytes_sent,
                        "dirty sync round complete"
                    );
                    // Acknowledge the round (issue #391): the sender waits
                    // for this Ack after sending RoundComplete. It carries
                    // the highest sequence number processed so far — by
                    // stream ordering that includes every chunk of the
                    // round — so it simultaneously flushes the ack window
                    // and satisfies the sender's round-wait. Sent
                    // unconditionally: even when the interval acks already
                    // covered the round (chunk count a multiple of the
                    // interval), the sender may still be draining.
                    self.send_ack(AckStatus::AckOk, tx).await?;
                }
                Some(migration_message::Payload::FinalSync(ref fs)) => {
                    info!(
                        volume_id = %self.volume_id,
                        vm_paused = fs.vm_paused,
                        "received FinalSync"
                    );
                    // Flush the ack window at the phase boundary so the
                    // sender's post-FinalSync drain completes for chunk
                    // counts that are not a multiple of ack_interval (e.g.
                    // the bulk-copy remainder when no dirty round ran).
                    self.flush_pending_acks(tx).await?;
                }
                Some(migration_message::Payload::FinalizeComplete(ref fc)) => {
                    info!(
                        volume_id = %self.volume_id,
                        total_bytes = fc.total_bytes,
                        total_chunks = fc.total_chunks,
                        "received FinalizeComplete"
                    );

                    // Defense in depth: flush any still-unacknowledged
                    // chunks before the final verdict so the sender observes
                    // full acknowledgment (FinalSync normally already did).
                    self.flush_pending_acks(tx).await?;

                    // Destination verification (issue #392): re-compute the
                    // sender's full-volume digest over the destination and
                    // only then report `verified`. Per-chunk CRC32 catches
                    // corruption *in flight*; this catches corruption of the
                    // assembled volume (bit rot, truncation, external
                    // modification). Fails closed: an unparseable/unknown
                    // digest format or an unreadable destination yields
                    // `verified = false` — `Completed` at the sender
                    // genuinely means "destination verified".
                    let (verified, error_message) =
                        self.verify_destination(&fc.volume_checksum).await;
                    if verified {
                        info!(
                            volume_id = %self.volume_id,
                            blocks_received = self.blocks_received,
                            "destination volume digest verified"
                        );
                    } else {
                        error!(
                            volume_id = %self.volume_id,
                            error = %error_message,
                            "destination volume digest verification failed"
                        );
                    }

                    // Send FinalizeAck
                    let ack_msg = MigrationMessage {
                        payload: Some(migration_message::Payload::FinalizeAck(FinalizeAck {
                            verified,
                            error_message,
                        })),
                    };
                    tx.send(ack_msg).await.map_err(|_| {
                        tonic::Status::internal("failed to send FinalizeAck: channel closed")
                    })?;

                    info!(
                        volume_id = %self.volume_id,
                        blocks_received = self.blocks_received,
                        "migration receive complete"
                    );
                    break;
                }
                Some(migration_message::Payload::Error(ref err)) => {
                    error!(
                        volume_id = %self.volume_id,
                        code = ?err.code,
                        message = %err.message,
                        "received MigrationError from sender"
                    );
                    return Err(tonic::Status::internal(format!(
                        "sender reported error: {}",
                        err.message
                    )));
                }
                _ => {
                    warn!(volume_id = %self.volume_id, "unexpected message in receiver loop");
                }
            }
        }

        Ok(())
    }

    /// Process a single BlockChunk: verify CRC, write to volume, send Ack if needed.
    async fn handle_chunk(
        &mut self,
        chunk: &chv_stord_api::chv_stord_api::BlockChunk,
        tx: &mpsc::Sender<MigrationMessage>,
    ) -> Result<(), tonic::Status> {
        self.last_sequence_num = chunk.sequence_num;

        // Reject chunks that would write past the end of the receiving volume.
        let chunk_end = chunk
            .offset
            .checked_add(chunk.data.len() as u64)
            .ok_or_else(|| {
                tonic::Status::invalid_argument(format!(
                    "BlockChunk at offset {} overflows u64",
                    chunk.offset
                ))
            })?;
        if chunk_end > self.size_bytes {
            return Err(tonic::Status::invalid_argument(format!(
                "BlockChunk at offset {} with {} bytes exceeds volume size {}",
                chunk.offset,
                chunk.data.len(),
                self.size_bytes
            )));
        }

        // Verify CRC32 (skip for sparse blocks)
        if !chunk.is_sparse {
            let computed_crc = crc32fast::hash(&chunk.data);
            if computed_crc != chunk.crc32 {
                warn!(
                    offset = chunk.offset,
                    sequence = chunk.sequence_num,
                    expected_crc = chunk.crc32,
                    computed_crc,
                    "CRC mismatch"
                );
                // Send Ack with mismatch status (fail-closed: the sender
                // treats AckCrcMismatch as a data-loss error).
                let ack_msg = MigrationMessage {
                    payload: Some(migration_message::Payload::Ack(Ack {
                        last_offset: chunk.offset,
                        last_sequence_num: chunk.sequence_num,
                        status: AckStatus::AckCrcMismatch.into(),
                    })),
                };
                tx.send(ack_msg)
                    .await
                    .map_err(|_| tonic::Status::internal("failed to send Ack: channel closed"))?;
                return Err(tonic::Status::data_loss(format!(
                    "CRC mismatch at offset {}",
                    chunk.offset
                )));
            }
        }

        // Write the block
        if chunk.is_sparse {
            // For sparse blocks, write zeros. If the volume is sparse-allocated,
            // the backend may optimize this away.
            let zeros = vec![0u8; chunk.data.len().max(1)];
            // Only write if there's a meaningful block size. Sparse blocks with
            // empty data means the backend should already have zeros.
            if !chunk.data.is_empty() {
                self.backend
                    .write_block(&self.volume_id, &self.handle, chunk.offset, &zeros)
                    .await
                    .map_err(|e| {
                        tonic::Status::internal(format!(
                            "write_block failed at offset {}: {e}",
                            chunk.offset
                        ))
                    })?;
            }
            // If data is empty and is_sparse, skip write (volume was zero-initialized)
        } else {
            self.backend
                .write_block(&self.volume_id, &self.handle, chunk.offset, &chunk.data)
                .await
                .map_err(|e| {
                    tonic::Status::internal(format!(
                        "write_block failed at offset {}: {e}",
                        chunk.offset
                    ))
                })?;
        }

        self.blocks_received += 1;
        self.blocks_since_last_ack += 1;
        self.last_offset = chunk.offset;

        // Send Ack every ack_interval blocks
        if self.blocks_since_last_ack >= self.ack_interval {
            self.send_ack(AckStatus::AckOk, tx).await?;
        }

        Ok(())
    }

    /// Send an `Ack` covering everything processed so far.
    ///
    /// `last_sequence_num`/`last_offset` are the receiver's cumulative
    /// progress; the sender uses them to slide its send window and to
    /// complete its per-phase drain loops.
    async fn send_ack(
        &mut self,
        status: AckStatus,
        tx: &mpsc::Sender<MigrationMessage>,
    ) -> Result<(), tonic::Status> {
        let ack_msg = MigrationMessage {
            payload: Some(migration_message::Payload::Ack(Ack {
                last_offset: self.last_offset,
                last_sequence_num: self.last_sequence_num,
                status: status.into(),
            })),
        };
        tx.send(ack_msg)
            .await
            .map_err(|_| tonic::Status::internal("failed to send Ack: channel closed"))?;
        self.blocks_since_last_ack = 0;
        Ok(())
    }

    /// Verify the destination volume against the sender's finalize digest.
    ///
    /// Returns `(verified, error_message)` for the `FinalizeAck`. Every
    /// failure path — unknown/unparseable digest format, unreadable
    /// destination, digest mismatch — returns `verified = false` with a
    /// precise, content-free message (algorithm names and hex digests
    /// only; never volume data).
    async fn verify_destination(&self, volume_checksum: &[u8]) -> (bool, String) {
        let expected = match volume_digest::parse_volume_digest(volume_checksum) {
            Ok(expected) => expected,
            // Fail closed on unknown/unsupported digest formats so a
            // future algorithm change is detected, not misverified.
            Err(reason) => return (false, reason),
        };
        let actual = match volume_digest::compute_volume_digest(
            self.backend.as_ref(),
            &self.volume_id,
            &self.handle,
            self.size_bytes,
        )
        .await
        {
            Ok(actual) => actual,
            Err(e) => {
                return (
                    false,
                    format!("failed to compute destination volume digest: {e}"),
                )
            }
        };
        if actual == expected {
            (true, String::new())
        } else {
            (
                false,
                format!(
                    "destination digest mismatch: expected {}, got {}",
                    expected.display(),
                    actual.display()
                ),
            )
        }
    }

    /// Flush the ack window at a phase boundary: send the outstanding Ack
    /// if any chunks are unacknowledged.
    ///
    /// The sender drains each phase until its last acknowledged sequence
    /// equals the last sequence it sent. Without this flush, any phase
    /// whose chunk count is not a multiple of `ack_interval` would leave
    /// the sender waiting until its 30 s timeout (issue #391).
    async fn flush_pending_acks(
        &mut self,
        tx: &mpsc::Sender<MigrationMessage>,
    ) -> Result<(), tonic::Status> {
        if self.blocks_since_last_ack == 0 {
            return Ok(());
        }
        debug!(
            volume_id = %self.volume_id,
            pending = self.blocks_since_last_ack,
            sequence = self.last_sequence_num,
            "flushing ack window at phase boundary"
        );
        self.send_ack(AckStatus::AckOk, tx).await
    }
}
