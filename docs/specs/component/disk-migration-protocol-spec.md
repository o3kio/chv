# Disk Migration Protocol Spec (stord-to-stord)

## Purpose
Defines the block-level data transfer protocol between two `chv-stord` instances during VM migration. This is the stord-level wire companion of `live-migration-spec.md`, which covers the control plane (CP) side of orchestration: where the two overlap, that spec covers orchestration and this one covers the stream itself. The user-facing API currently enforces stop-the-world (pause-first) migration until issue #394 is proven; the wire protocol itself still carries the `pause_first` mode bit.

## Participants
- **Source stord**: reads volume blocks, tracks dirty blocks, streams data (`crates/chv-stord-core/src/migration/sender.rs`)
- **Destination stord**: creates the receiving volume, receives blocks, acknowledges (`crates/chv-stord-core/src/migration/receiver.rs`, `crates/chv-stord-core/src/migration/service.rs`)
- **Control plane / agent**: initiates the transfer via `TriggerDiskMigration`, monitors it via `GetDiskMigrationStatus`, and performs the VM-pause handshake via `ResumeDiskMigration` (`crates/chv-stord-core/src/handlers.rs`); it does not participate in data transfer

## Claimed mode / not claimed

**Claimed: quiescent-volume migration.** The protocol is correct when the
source volume receives no writes while bulk copy and dirty rounds run and the
VM is paused before finalize.

**Not claimed: concurrent-write ("live") migration** — issue #394 is the
durable record. The source dirty bitmap is populated only by stord's own
`write_block` (`crates/chv-stord-backends/src/trait.rs`), whose only product
caller is the migration *receiver* (`crates/chv-stord-core/src/migration/receiver.rs`);
there is no stord write RPC and no interception of VM/host writes to an
attached volume. On a migration source the bitmap is therefore always
all-zero, the first dirty round converges at 0 immediately, and the
dirty-sync machinery is effectively a no-op. There is also no post-pause
final sweep in the sender (`crates/chv-stord-core/src/migration/sender.rs`):
after the VM is paused it sends `FinalSync` and finalizes without re-reading
the bitmap. A concurrent write is not *silently* lost — the finalize digest
(`crates/chv-stord-core/src/migration/volume_digest.rs`) is computed over the
source after the pause, so a destination that missed a write fails
verification and the task ends Failed instead of Completed — but converging
dirty rounds under load and a post-pause sweep are unimplemented, so the mode
is not claimed. What was actually proven on a real host (positive path plus
fail-closed identity/interruption negatives) is recorded in
`docs/evidence/production-readiness/v0.3.0-rc1/04-real-host-qualification/m4.6-migration.md`.

**Write canary (issue #394, Option A — fail fast).** Since #394's Option A
ruling, concurrent source writes are not merely caught late at the finalize
digest: the sender samples a write-canary fingerprint of the source's
backing store immediately before bulk copy
(`StorageBackend::write_canary_probe`) and re-checks it at every
dirty-round boundary and at the pre-pause gate. On a file-backed backend
(`WriteCanaryCapability::FileStat`) any stat change — mtime, ctime, or
size; every `write(2)` through any descriptor updates mtime/ctime, the
hypervisor's included — fails the migration immediately with a distinct
`source_modified_during_migration` `failed_precondition` error, *before*
the VM pause and *before* the finalize digest is computed (an empty
`finalize_volume_digest` distinguishes the early failure). Backends whose
backing store cannot be stat-observed (block devices, RADOS objects)
report `WriteCanaryCapability::Unavailable` and keep the pre-canary
behavior: fail-late at the digest. The canary is a latency-of-failure
optimization layered in front of the digest, not a correctness gate of
its own — the residual window between the pre-pause gate and the pause
completing remains covered by the post-pause whole-volume digest.

**Pause-first mode (issue #394, Option C — temporary mandatory gate, #656).**
`TriggerDiskMigrationRequest.pause_first` (threaded end-to-end: the
agent's `MigrateVmRequest`→`MigrationConfig.pause_first`, the BFF
vm-mutate migrate action's `pause_first` JSON field, the WebUI
migrate modal's mandatory checked pause-first control, and
`chvctl migrate start --pause-first` / `chvctl vm migrate
--pause-first`) moves the VM-pause handshake to **before any source
byte is read**: the task enters the `PAUSED_PRE_COPY` status phase
with `needs_vm_pause = true` — observable with zero bytes transferred
— and the sender blocks there until `ResumeDiskMigration{vm_paused:
true}`. The canary baseline is sampled *after* the pause, so the
covered window is exactly the quiesced transfer; the dirty rounds then
converge trivially on the quiesced source; the finalize digest
verifies instead of catching loss. A migration in this mode is
correct by construction for any write pattern, at the cost of
downtime equal to the full disk+memory transfer — the operator-facing
alternative to #394's "retry with the source quiesced" answer.

**Fail-closed until #394 is proven (#656).** The user-facing API no
longer defaults to the live path: the BFF vm-mutate handler and the
control-plane mutation service reject `pause_first=false` (and a
missing field) with `400 Bad Request` — "live/concurrent-write
migration is not supported until #394 is proven; set pause_first=true
(stop-the-world)" — before any lifecycle dispatch, and both chvctl
commands fail closed client-side before sending a request. The WebUI
modal defaults to pause-first, renders the control checked and
disabled, and always sends `true`. This is a temporary gate, not a
new capability claim: live migration returns when #394 is proven.
The stord wire protocol still accepts `pause_first=false` (the
default) for internal/older callers; the gate lives at the
user-facing surfaces.

The sender fails closed (`failed_precondition`, before connecting) if
the mode is requested without a task attached: an operator who asked
for the pause must never get a silent degradation to quiescent-assumed
semantics. **T=0 skew guard (#582)**: stord echoes the requested mode
in `TriggerDiskMigrationResponse.pause_first`, and the agent fails
the migration at trigger time when a pause-first request comes back
without the echo — a stord that predates the field drops the unknown
request field (proto3) and would silently run the default path. The
guard is one-directional: default-mode requests never require an
echo, so a skewed stord still serves them.

## Transport
- gRPC bidirectional streaming: `StorageMigrationService.StreamBlocks` over a single `MigrationMessage` stream (`proto/node/chv-stord-migration.proto`)
- The migration stream is served on **two** listeners (`crates/chv-stord-core/src/server.rs`):
  - an **mTLS TCP listener** bound to the operator-configured `migration.listen_addr`, with **mandatory client-certificate authentication** — the TLS config is built with `client_ca_root(...)` and no client-auth-optional mode, so the handshake fails unless the peer presents a certificate chaining to `migration.client_ca_path`
  - stord's **Unix socket** (plaintext, local-node only), shared with the control RPCs
- The sender only ever dials `https://` with its client identity: an `http://` endpoint is force-upgraded to `https://` before dialing, and a missing TLS config is a hard precondition failure naming the config keys — there is **no plaintext fallback** (`crates/chv-stord-core/src/migration/sender.rs`)
- Identity material is loaded and validated fail-closed at **startup**, not lazily: unreadable files, mismatched keypairs, empty/invalid CA bundles, and half-configured client or receiver field sets are startup errors (`crates/chv-stord-core/src/migration/tls_config.rs`)
- Both serving paths accept messages up to **8 MiB** (`MAX_MIGRATION_MESSAGE_SIZE_BYTES`, `crates/chv-stord-core/src/migration/mod.rs`, applied in `crates/chv-stord-core/src/server.rs`): a `BlockChunk` carries a full 4 MiB block plus protobuf overhead and exceeds tonic's 4 MiB default decode limit

## Protocol Phases

### Phase 1: Handshake

```
Source                          Destination
  │                                 │
  │  InitMigration(volume_meta)     │
  │────────────────────────────────►│
  │                                 │
  │  MigrationReady(dest_volume_id) │
  │◄────────────────────────────────│
```

- Source sends: `volume_id`, `size_bytes`, `block_size` (4 MiB), `format` (always `"raw"` today), `checksum_type` (always `"crc32"`) (`crates/chv-stord-core/src/migration/sender.rs`)
- The sender sends `InitMigration` **before** awaiting the stream response: the tonic client future resolves only when the server sends response headers, and the server sends them only after it has read the first message and created the receiving volume (`crates/chv-stord-core/src/migration/sender.rs`)
- Dest validates before creating anything (`crates/chv-stord-core/src/migration/service.rs`):
  - `size_bytes` ≤ 16 TiB (`MAX_MIGRATION_SIZE_BYTES`)
  - `volume_id` is a safe id and the receiving path (`runtime_dir/{volume_id}.img`) stays inside `runtime_dir` — path-traversal defense at the trust boundary
- Dest creates the receiving volume via `create_receiving_volume` — an existing file is a **hard refusal, never a truncate** (`crates/chv-stord-backends/src/trait.rs`)
- Dest responds `MigrationReady{dest_volume_id}` (the same volume id), or the stream fails with an error

### Phase 2: Bulk Copy

```
Source                          Destination
  │                                 │
  │  BlockChunk(offset, data, crc)  │
  │────────────────────────────────►│  (sequential, full volume)
  │  BlockChunk(offset, data, crc)  │
  │────────────────────────────────►│
  │        ...                      │
  │                                 │
  │  Ack(last_seq, status)          │
  │◄────────────────────────────────│  (every 64 chunks)
```

- Source reads the volume sequentially in 4 MiB chunks (`DEFAULT_BLOCK_SIZE` = `DIRTY_TRACKING_BLOCK_SIZE`, so bitmap bits map 1:1 to migration chunks) and sends each as a `BlockChunk` with CRC32 of the data (`crates/chv-stord-core/src/migration/sender.rs`)
- Immediately before this phase, the sender samples the **write-canary baseline** — a stat fingerprint of the source's backing store (`write_canary_probe`) — which every later phase boundary re-checks (#394, Option A; see Claimed mode)
- Sparse handling: an all-zero block is sent with empty `data`, `is_sparse = true`, and `crc32 = 0`; the receiver writes zeros if payload bytes are present and skips the write when the payload is empty (the receiving volume is created zero-initialized) (`crates/chv-stord-core/src/migration/sender.rs`, `migration/receiver.rs`)
- The receiver rejects a chunk whose `offset + len` would write past the end of the receiving volume (`crates/chv-stord-core/src/migration/receiver.rs`)
- Flow control: send window of at most 128 unacknowledged chunks, interval acks every 64 chunks, 30 s ack-wait timeout (see Flow Control)
- There is deliberately **no drain at the end of this phase**: the receiver cannot know the bulk phase ended until it sees the next boundary message (`RoundStart` or `FinalSync`), which the sender only sends after bulk copy returns — a drain here could deadlock until the 30 s timeout whenever the chunk count is not a multiple of the ack interval (issue #391). Every chunk is instead awaited at the next observable boundary

### Phase 3: Dirty Sync (iterative)

```
Source                          Destination
  │                                 │
  │  RoundStart(round_num,          │
  │    dirty_block_count)           │
  │────────────────────────────────►│
  │                                 │
  │  BlockChunk(offset, data, crc)  │  (only dirty blocks)
  │────────────────────────────────►│
  │        ...                      │
  │                                 │
  │  RoundComplete(round_num,       │
  │    blocks_sent, bytes_sent)     │
  │────────────────────────────────►│
  │                                 │
  │  Ack(last_seq, ACK_OK)          │  (round ack = window flush)
  │◄────────────────────────────────│
```

- Each round starts with a **write-canary re-check** against the migration-start fingerprint (`write_canary_probe`; a stat change fails the migration with `source_modified_during_migration` before any further data moves — issue #394, Option A), then an **atomic snapshot-and-clear** of the dirty bitmap (`snapshot_and_clear_dirty_bitmap`), so no stord-visible write is lost between reading the bitmap and clearing it (`crates/chv-stord-core/src/migration/sender.rs`, `crates/chv-stord-backends/src/trait.rs`)
- The round's dirty blocks are bracketed by `RoundStart` / `RoundComplete`
- The receiver answers every `RoundComplete` with an `Ack` carrying its cumulative sequence number — by stream ordering that includes every chunk of the round, so the round ack doubles as the ack-window flush; the sender drains after it until its last acknowledged sequence equals the last sequence it sent (`crates/chv-stord-core/src/migration/receiver.rs`, `migration/sender.rs`, issue #391)
- Round termination (`crates/chv-stord-core/src/migration/sender.rs`):
  - `dirty_block_count == 0` → exit immediately (the quiescent-volume common case)
  - `dirty_block_count < DIRTY_THRESHOLD` (1024 blocks = 4 GiB at 4 MiB) → exit
  - `MAX_DIRTY_ROUNDS` (10) reached → forced exit
- Progress (convergence round, dirty blocks remaining, bytes transferred) is published in the migration task state and served via `GetDiskMigrationStatus` (`crates/chv-stord-core/src/migration/task.rs`, `crates/chv-stord-core/src/handlers.rs`)

### Phase 4: VM Pause Handshake

```
Source stord                Agent                   Destination
  │                           │                         │
  │ needs_vm_pause = true     │                         │
  │ (task reaches             │                         │
  │  PausedFinalSync; sender  │                         │
  │  blocks on the pause      │                         │
  │  channel)                 │                         │
  │──────────────────────────►│  pause VM via the       │
  │                           │  Cloud Hypervisor API   │
  │                           │                         │
  │  ResumeDiskMigration(vm_paused=true)                │
  │◄──────────────────────────│                         │
  │                           │                         │
  │  FinalSync(vm_paused=true)│                         │
  │─────────────────────────────────────────────────────►│
```

- When the dirty rounds are done, the sender runs the **pre-pause gate** — the last write-canary re-check (#394, Option A); failing there means the VM was never paused, so no resume is needed — then the task moves to `PausedFinalSync` and sets `needs_vm_pause = true`, and the sender blocks on the task's pause channel (`crates/chv-stord-core/src/migration/sender.rs`, `migration/task.rs`)
- In pause-first mode (#394, Option C) this same handshake runs **before bulk copy** (`PausedPreCopy`): the pre-pause gate and the wait below it are then already satisfied — the VM has been paused since before the first source read — so the task transitions to `PausedFinalSync` for observability and the sender proceeds to `FinalSync` without waiting
- The agent (polling `GetDiskMigrationStatus`) pauses the VM via the Cloud Hypervisor API and signals back with `ResumeDiskMigration{vm_paused: true}` (`crates/chv-stord-core/src/handlers.rs`, `crates/chv-agent-core/src/migration.rs`)
- Only then does the sender emit `FinalSync{vm_paused: true}`
- There is **no post-pause dirty sweep** (issue #394): the last dirty round ran before the pause; for a quiescent volume there is nothing left to flush
- The receiver flushes its ack window at the `FinalSync` boundary, and the sender drains until every chunk is acknowledged before announcing finalization (fail-closed)

### Phase 5: Finalize

```
Source                          Destination
  │                                 │
  │  FinalizeComplete(total_bytes,  │
  │    total_chunks,                │
  │    volume_checksum)             │
  │────────────────────────────────►│
  │                                 │  re-compute digest over the
  │                                 │  destination volume
  │  FinalizeAck(verified, error)   │
  │◄────────────────────────────────│
```

- The sender computes a **versioned full-volume SHA-256 digest** over the source — after the pause — streamed through the same `read_block` path used for bulk copy (the volume is never held in memory) and carries it in `FinalizeComplete.volume_checksum` (`crates/chv-stord-core/src/migration/volume_digest.rs`, `migration/sender.rs`)
- Wire format: `"sha256:"` (7 ASCII bytes) followed by 32 raw digest bytes. The digest is self-describing and versioned so a future algorithm change is *detected*, never misinterpreted
- The receiver flushes any still-unacknowledged chunks (defense in depth; `FinalSync` normally already did), then re-computes the same digest over the destination and answers `FinalizeAck{verified, error_message}` (`crates/chv-stord-core/src/migration/receiver.rs`)
- **Fail-closed**: `verified = false` — digest mismatch, unknown/unparseable digest format, or an unreadable destination — makes the sender fail the migration with `Status::data_loss` and the task end Failed. `Completed` genuinely means "destination verified" (issue #392)
- Boundary acks still in flight when `FinalizeAck` is awaited are processed normally; a CRC-mismatch Ack arriving there still fails the migration
- **Bounded wait (#656)**: each read of the `FinalizeAck` wait is wrapped in a configurable timeout (default 30 s, production bounds 5–300 s; `MigrationSender::with_finalize_ack_timeout` for tests). On expiry the sender emits the migration error metric (`reason="finalize_ack_timeout"`), marks the task Failed with the timeout message, and returns `Status::deadline_exceeded("timed out waiting for FinalizeAck after Ns")`. A destination that never answers finalization can no longer hang the task — or leave the VM paused — forever.

## Dirty Block Tracking

**Mechanism:** a userspace bitmap maintained by the storage backend itself
(`enable_dirty_tracking`, `snapshot_and_clear_dirty_bitmap`,
`crates/chv-stord-backends/src/trait.rs`). There is no device-mapper snapshot
and no I/O interception.

**Bitmap spec:**
- 1 bit per block; the block size is `DIRTY_TRACKING_BLOCK_SIZE` (4 MiB, `crates/chv-stord-backends/src/trait.rs`) and must equal the migration chunk size so bits map 1:1 to chunks
- Bitmap is local to the source stord (not transferred)
- The snapshot-and-clear at each round start is atomic
- Bits are set only by stord's own `write_block`, whose only product caller is the migration receiver — **guest-initiated writes through the Cloud Hypervisor-held file descriptor and host-side writes to the volume file are not observed** (issue #394; see *Claimed mode / not claimed*)
- stord enables dirty tracking when a volume is opened and again before spawning the migration sender, so the first round can always snapshot a bitmap (`crates/chv-stord-core/src/handlers.rs`)

## Block Chunk Message

```protobuf
message BlockChunk {
  uint64 offset = 1;          // byte offset in volume
  bytes data = 2;             // block data (up to block_size bytes)
  uint32 crc32 = 3;           // CRC32 of data field
  bool is_sparse = 4;         // if true, data is empty, block is all zeros
  uint32 sequence_num = 5;    // monotonically increasing per stream
}
```

## Flow Control

- The sender maintains a send window of at most **128** unacknowledged chunks (`SendWindow`, `crates/chv-stord-core/src/migration/flow_control.rs`); when the window is full it blocks until an Ack arrives
- The receiver sends an `Ack` every **64** chunks while streaming (`DEFAULT_ACK_INTERVAL`, `crates/chv-stord-core/src/migration/receiver.rs`), and additionally at stream boundaries so the sender's per-phase drains complete for *arbitrary* chunk counts (issue #391):
  - every `RoundComplete` is answered with an `Ack` (the round acknowledgment, doubling as the window flush)
  - `FinalSync` and `FinalizeComplete` flush any chunks not yet acknowledged
- If no Ack arrives within **30 s**, the sender fails with `deadline_exceeded` and the task ends Failed — it does not pause and retry (`crates/chv-stord-core/src/migration/flow_control.rs`, `migration/sender.rs`)
- The `Backpressure` message exists in the proto and the sender honors one if it arrives — `handle_inbound_message` records the factor (`crates/chv-stord-core/src/migration/sender.rs`) and the `bulk_copy` / `dirty_sync_rounds` send loops apply the throttle sleep — but the receiver never sends it in the current implementation — it is a defensive wire surface, not an active mechanism

## Integrity

- Every `BlockChunk` carries CRC32 of the data field (0 for sparse chunks); the receiver verifies it on receipt (`crates/chv-stord-core/src/migration/receiver.rs`)
- A CRC mismatch is **fail-closed, not repaired**: the receiver answers `Ack{ACK_CRC_MISMATCH}` and fails the stream; the sender treats that Ack as `data_loss` and fails the migration. There is no NACK/retransmit protocol (`crates/chv-stord-core/src/migration/receiver.rs`, `migration/sender.rs`)
- At finalize the sender always carries the versioned full-volume SHA-256 digest (no size cutoff, no per-round checksums), and `verified = false` fails the migration with `data_loss` (issue #392) — per-chunk CRC32 catches corruption *in flight*, the digest catches corruption of the *assembled* volume

## Resumability

There is none, by design. Migration task state is in-memory only; a stream
break (peer death, transport error) fails the source task — surfaced as a
h2/transport error or the 30 s ack timeout, not a hang. A restarted
destination knows nothing of the killed task, and a re-trigger of the same
volume hits the `create_new` refusal on the partial receiving volume
("refusing to truncate"). Recovery is the documented operator step: remove
the partial destination volume and re-trigger
(`crates/chv-stord-core/src/migration/task.rs`,
`crates/chv-stord-backends/src/trait.rs`; demonstrated end-to-end as leg N9
of the M4.6 qualification).

## Error Handling

| Error | Source Action | Dest Action |
|---|---|---|
| Stream disconnect | Task Failed (transport error or 30 s ack timeout) | Releases its backend handle when the stream ends; partial receiving volume is **retained**, not deleted |
| CRC mismatch | Fails with `data_loss` | Sends `Ack{ACK_CRC_MISMATCH}`, fails the stream |
| Write error at dest | Fails (stream error) | Fails the stream with an internal error |
| Receiving volume creation fails (incl. existing partial file) | Handshake fails | Error before `MigrationReady` |
| Chunk past end of receiving volume | — | `invalid_argument` |
| Timeout (no Ack in 30 s) | `deadline_exceeded`, task Failed | — |
| Timeout waiting for `FinalizeAck` (default 30 s, #656) | `deadline_exceeded`, task Failed | — |
| Live (`pause_first=false`) request at the user-facing API (BFF/chvctl/UI, #656) | `400 Bad Request` before dispatch (CLI fails locally) | — |
| Finalize digest mismatch / unknown format / unreadable destination | `data_loss`, task Failed | `FinalizeAck{verified: false}` with a content-free error message |
| Oversized InitMigration (> 16 TiB) or unsafe volume_id | Handshake fails | `invalid_argument` |

Note: `ACK_WRITE_ERROR` is defined in the proto but never emitted by the
current receiver — destination write failures surface as stream errors
instead.

## Proto Service Definition

```protobuf
service StorageMigrationService {
  // Bidirectional streaming for block transfer
  rpc StreamBlocks(stream MigrationMessage) returns (stream MigrationMessage);
}

message MigrationMessage {
  oneof payload {
    InitMigration init = 1;
    MigrationReady ready = 2;
    BlockChunk chunk = 3;
    Ack ack = 4;
    Backpressure backpressure = 5;
    RoundStart round_start = 6;
    RoundComplete round_complete = 7;
    FinalSync final_sync = 8;
    FinalizeComplete finalize_complete = 9;
    FinalizeAck finalize_ack = 10;
    MigrationError error = 11;
  }
}

message InitMigration {
  string volume_id = 1;
  uint64 size_bytes = 2;
  uint32 block_size = 3;
  string format = 4;           // "raw" or "qcow2"
  string checksum_type = 5;    // "crc32"
}

message MigrationReady {
  string dest_volume_id = 1;
}

message Ack {
  uint64 last_offset = 1;
  uint32 last_sequence_num = 2;
  AckStatus status = 3;
}

enum AckStatus {
  ACK_OK = 0;
  ACK_CRC_MISMATCH = 1;
  ACK_WRITE_ERROR = 2;
}

message Backpressure {
  float slow_down_factor = 1;  // 0.5 = halve send rate
}

message RoundStart {
  uint32 round_num = 1;
  uint64 dirty_block_count = 2;
}

message RoundComplete {
  uint32 round_num = 1;
  uint64 blocks_sent = 2;
  uint64 bytes_sent = 3;
}

message FinalSync {
  bool vm_paused = 1;
}

message FinalizeComplete {
  uint64 total_bytes = 1;
  uint64 total_chunks = 2;
  // Versioned, self-describing full-volume digest the receiver must
  // re-compute over the destination before reporting verified=true:
  // "sha256:" (7 ASCII bytes) followed by 32 raw digest bytes. Receivers
  // fail closed on unrecognized formats. (Previously optional/unused.)
  bytes volume_checksum = 3;
}

message FinalizeAck {
  bool verified = 1;
  string error_message = 2;   // populated if verified=false
}

message MigrationError {
  MigrationErrorCode code = 1;
  string message = 2;
}

enum MigrationErrorCode {
  MIGRATION_ERROR_UNSPECIFIED = 0;
  MIGRATION_ERROR_DISK_FULL = 1;
  MIGRATION_ERROR_IO_ERROR = 2;
  MIGRATION_ERROR_VOLUME_NOT_FOUND = 3;
  MIGRATION_ERROR_CHECKSUM_MISMATCH = 4;
  MIGRATION_ERROR_TIMEOUT = 5;
}
```

(`proto/node/chv-stord-migration.proto` is the source of truth; the sender
never emits `MigrationError` today — both peers handle it defensively.)

## Configuration

The operator-facing config surface is the stord `[migration]` section
(`StordMigrationConfig`, `crates/chv-config/src/lib.rs`), plus the stord-level
`migration_dest_allowlist`. There are no per-protocol tuning knobs
(`ack_interval`, `send_window`, timeouts, block size are constants — see
below).

### `[migration]` (stord config, `crates/chv-config/src/lib.rs`)

`migration.enabled` is the single master switch for both halves of migration
TLS. All validation is fail-closed at startup
(`crates/chv-stord-core/src/migration/tls_config.rs`,
`cmd/chv-stord/src/main.rs`).

| Parameter | Required when | Description |
|---|---|---|
| migration.enabled | — | Master switch. `false` (default): no migration credentials, migration actions unavailable. `true`: at least one half (client and/or receiver) must be configured |
| migration.client_cert_path | any client field set | PEM node/client certificate (issued by the CHV CA) the sender presents to the destination |
| migration.client_key_path | any client field set | PEM private key for the client certificate |
| migration.ca_cert_path | any client field set | PEM CA bundle used to validate the migration destination |
| migration.dest_server_name | any client field set | Expected server name in the destination's certificate (DNS SAN / identity) |
| migration.listen_addr | any receiver field set | TCP address for the migration receiver mTLS listener, e.g. `"0.0.0.0:50052"`. Unset (default) = source-only stord, no inbound migrations |
| migration.server_cert_path | any receiver field set | PEM server certificate presented to migration peers |
| migration.server_key_path | any receiver field set | PEM private key for the server certificate |
| migration.client_ca_path | any receiver field set | PEM CA bundle used to authenticate peer client certificates on the receiver listener (client-cert auth is mandatory) |

Fail-closed gating rules (issues #390, #395, #401):

- `enabled = false` with **any** client identity field set → startup error
- `enabled = false` with **any** receiver field set → startup error (an operator who believes migration is off must not get an inbound TCP listener)
- `enabled = true` with **no** client fields → destination-only stord: no client identity, outbound migration actions fail with a `failed_precondition` error (issue #401). The client half is all-or-nothing *when any client field is set*. The receiver half is all-or-nothing *when any receiver field is set*; `enabled = true` with **no** receiver fields is a legitimate source-only stord (no listener, logged at startup)
- `enabled = true` with **neither** half configured → startup error (an enabled migration section that configures nothing is a misconfiguration)
- Receiver files unreadable, keypair mismatch, empty/invalid CA bundle, or an unparseable `listen_addr` → startup error

Stord additionally enforces `migration_dest_allowlist` (top-level stord key,
not under `[migration]`): the destination endpoint host of every
`TriggerDiskMigration` must be listed (empty list = allow all)
(`crates/chv-stord-core/src/handlers.rs`).

### Hardcoded protocol constants (not config)

| Constant | Value | Where |
|---|---|---|
| Migration block size | 4 MiB (`DEFAULT_BLOCK_SIZE` = `DIRTY_TRACKING_BLOCK_SIZE`; must equal the bitmap's per-bit block) | `crates/chv-stord-core/src/migration/sender.rs`, `crates/chv-stord-backends/src/trait.rs` |
| Dirty convergence threshold (`DIRTY_THRESHOLD`) | 1024 blocks | `crates/chv-stord-core/src/migration/sender.rs` |
| Max dirty rounds (`MAX_DIRTY_ROUNDS`) | 10 | `crates/chv-stord-core/src/migration/sender.rs` |
| Ack interval (`DEFAULT_ACK_INTERVAL`) | 64 chunks | `crates/chv-stord-core/src/migration/receiver.rs` |
| Send window (max unacked) | 128 chunks | `crates/chv-stord-core/src/migration/flow_control.rs` |
| Ack-wait timeout | 30 s | `crates/chv-stord-core/src/migration/flow_control.rs` |
| Max gRPC message size (`MAX_MIGRATION_MESSAGE_SIZE_BYTES`) | 8 MiB (2 × block size; a `BlockChunk` carries a full 4 MiB block plus protobuf overhead, exceeding tonic's 4 MiB default) | `crates/chv-stord-core/src/migration/mod.rs` |
| Max migrated volume size (`MAX_MIGRATION_SIZE_BYTES`) | 16 TiB | `crates/chv-stord-core/src/migration/service.rs` |
| Migration TLS handshake timeout (`MIGRATION_HANDSHAKE_TIMEOUT`) | 30 s (how long an accepted connection may take to complete its TLS handshake before the migration receiver gives up on it and closes it; guards only the ClientHello-never-arrives case — deliberately not a tunable) | `crates/chv-stord-core/src/server.rs` |

## Non-goals
- Compression (may add later if network is bottleneck)
- Encryption beyond mTLS (data is encrypted in transit by TLS)
- Multi-volume parallel streams (one stream per volume in v1)
- Bandwidth throttling via the wire (`Backpressure` is never sent by the receiver; use OS-level tc/qdisc if needed)
- Retransmission of failed chunks (fail-closed instead — see Integrity)
- Resumption of interrupted transfers (see Resumability)
- Concurrent-write ("live") disk migration — not claimed; see *Claimed mode / not claimed* (issue #394)

## Security requirements
- mTLS is mandatory and total: the sender refuses to run without `MigrationTlsConfig` (no plaintext fallback, `http://` endpoints are force-upgraded before dialing), and the receiver listener requires client-certificate authentication (`crates/chv-stord-core/src/migration/sender.rs`, `crates/chv-stord-core/src/server.rs`)
- Identity material is validated at startup — fail-closed, never a skipped-verify option (`crates/chv-stord-core/src/migration/tls_config.rs`)
- The sender validates the destination against the configured CA bundle and expected server name (`migration.ca_cert_path`, `migration.dest_server_name`)
- Destination endpoints are constrained by `migration_dest_allowlist` (`crates/chv-stord-core/src/handlers.rs`)
- Volume data never written to intermediate storage (direct stream between peers); the receiving volume path is contained inside the destination's `runtime_dir` with unsafe ids rejected (`crates/chv-stord-core/src/migration/service.rs`)
- A failed migration leaves a partial receiving volume in place (no auto-delete); a re-trigger refuses to truncate it, so a blind retry fails loudly instead of silently destroying data — removal is the documented operator step

## Recovery model
- Stream break at any phase: the source task ends Failed; there is no resume (in-memory task state)
- Destination restart mid-transfer: the restarted stord knows nothing of the killed task; re-triggering the same volume hits the `create_new` refusal on the partial receiving volume
- Documented operator recovery: remove the partial destination volume, re-trigger — the new run is a full protocol pass ending in digest verification
- The CP-side view (reaper, rollback, source-VM resume) is specified in `live-migration-spec.md`

## Implementation Status

| Protocol element | File | Status |
|---|---|---|
| Phase 1: Handshake (InitMigration/MigrationReady) | sender.rs, service.rs | DONE — init-before-stream ordering, 16 TiB cap, receiving-path traversal defense, create_new refusal on existing files |
| Phase 2: Bulk copy (sequential 4 MiB streaming) | sender.rs `bulk_copy()` | DONE |
| Phase 2: CRC32 per chunk | sender.rs, receiver.rs | DONE |
| Phase 2: Sparse block detection | sender.rs `is_all_zeros()` | DONE — all-zero blocks sent as empty payload with crc32=0 |
| Phase 3: Dirty sync rounds | sender.rs `dirty_sync_rounds()` | DONE — atomic snapshot-and-clear per round, `RoundStart`/chunks/`RoundComplete`, `DIRTY_THRESHOLD`/`MAX_DIRTY_ROUNDS` constants, early exit at 0 dirty |
| Phase 3/4: Write canary | sender.rs `verify_source_canary()`, backends `write_canary_probe()` | DONE — file-stat fingerprint sampled before bulk copy, re-checked at every round boundary and the pre-pause gate; `source_modified_during_migration` fail-fast (issue #394, Option A) |
| Phase 3: Round acks / boundary flushes | receiver.rs, sender.rs | DONE — ack every 64 chunks, round ack on `RoundComplete`, window flush at `FinalSync`/pre-`FinalizeAck` (issue #391) |
| Phase 4: Pause handshake (`needs_vm_pause` → `ResumeDiskMigration{vm_paused:true}` → `FinalSync{vm_paused:true}`) | sender.rs, task.rs, handlers.rs | DONE — no post-pause dirty sweep (issue #394, see Claimed mode) |
| Phase 0/4: Pause-first mode (`pause_first` → `PAUSED_PRE_COPY` before bulk copy; fail-closed without a task) | sender.rs, handlers.rs, task.rs; agent migration.rs, daemon_clients.rs, agent_server.rs; CP bff_mutations.rs; BFF handlers/vms.rs; chvctl; WebUI | DONE — issue #394, Option C; mandatory at the user-facing API until #394 is proven (issue #656, see Claimed mode) |
| Phase 5: Finalize digest | volume_digest.rs, sender.rs | DONE — versioned SHA-256 over the full source, sent in `FinalizeComplete.volume_checksum` (issue #392) |
| Phase 5: Bounded `FinalizeAck` wait | sender.rs `finalize_ack_timeout` | DONE — 30 s default, 5–300 s production bounds; timeout marks task Failed and returns `deadline_exceeded` (issue #656) |
| Phase 5: Destination verification | receiver.rs `verify_destination()` | DONE — re-computes the digest over the destination; `verified=false` ⇒ sender fails with `Status::data_loss`, task Failed |
| Flow control (SendWindow, Ack) | flow_control.rs | DONE — window 128, ack interval 64, 30 s timeout |
| gRPC message size limit (8 MiB, both serving paths) | migration/mod.rs, server.rs | DONE |
| mTLS on migration channel (client half) | sender.rs, tls_config.rs | DONE — sender refuses to run without `MigrationTlsConfig`; identity loaded and validated at startup |
| mTLS on migration channel (receiver half) | server.rs `serve_migration_tls`, tls_config.rs | DONE — TLS TCP listener with mandatory client-cert auth, all-or-nothing receiver fields (issue #390) |
| Backpressure handling | sender.rs `handle_inbound_message` | DONE (sender side) — the receiver never sends `Backpressure`; defensive surface only |

### Known constraints

1. **Concurrent-write migration is not claimed** — see *Claimed mode / not
   claimed*; issue #394 is the durable record. The dirty bitmap only observes
   stord's own `write_block` (its only product caller is the migration
   receiver), and the sender performs no post-pause final sweep.

2. **Destination-only stord was not expressible (RESOLVED — issue #401)**:
   `migration.enabled = true` used to gate *both* halves, making the client
   identity mandatory even on a stord that only ever receives migrations, so
   only source-only was expressible. Recorded as a config-model finding in
   the M4.6 evidence doc (§4.2). Resolved in #401: under `enabled = true`
   the two halves are now independently optional (each all-or-nothing within
   itself; **neither** half configured is a startup error — see the config
   table and gating rules above), so destination-only is expressible by
   configuring the receiver half alone
   (`crates/chv-stord-core/src/migration/tls_config.rs`).

3. **mTLS rejection observability (RESOLVED — issue #402 / PR #479,
   further hardened in #485)**: identity rejections used to surface at the
   client as opaque, race-dependent transport-level errors, and the
   destination logged nothing about a rejected handshake — fail-closed
   held, but triage required reproducing the handshake out-of-band.
   Recorded as a handshake-observability finding in the M4.6 evidence doc
   (§4.3). Resolved in #479: the sender now walks the tonic connect
   error's source chain and surfaces the terminal rustls alert/reason
   (wrong CA, wrong server name, expired destination certificate are
   distinguishable) in the failure message and the migration task's
   `error_message`, and the destination completes the handshake in its
   own accept loop and warn-logs every rejected handshake with the peer
   address and the reason — including received fatal alerts, so a
   sender-aborted handshake is distinguishable from a
   receiver-initiated rejection. Hardened in #485: the fatal-alert leg
   is pinned by test, a peer that goes silent mid-handshake is closed by
   a 30 s handshake timeout (info line with the peer) instead of pinning
   the accept loop, and a handshake completing after a raced shutdown is
   debug-logged rather than silently dropped
   (`crates/chv-stord-core/src/migration/sender.rs`,
   `crates/chv-stord-core/src/server.rs`).

What was actually exercised on a real host — the positive two-stord mTLS
migration and every fail-closed identity/plaintext/interruption negative — is
recorded in
`docs/evidence/production-readiness/v0.3.0-rc1/04-real-host-qualification/m4.6-migration.md`;
the dirty-round transfer-under-concurrent-writers boundary of that evidence
is exactly the not-claimed mode above.
