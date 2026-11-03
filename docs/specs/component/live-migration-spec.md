# Live Migration Orchestration Spec

## Purpose
Orchestrates VM migration between two nodes, coordinating disk pre-copy (quiescent-volume; see *Claimed mode / not claimed*), memory migration via Cloud Hypervisor, and post-migration validation. Runs as part of the single control plane (CP) orchestrator.

## Owner
chv-controlplane-service (orchestrator module)

## Scope
- Coordinates: source agent, destination agent, and the `chv-stord` and `chv-nwd` daemons on both nodes
- Does NOT: perform actual data transfer (stord does that), manage VXLAN tunnels (nwd does that)

## Claimed mode / not claimed

**Claimed: quiescent-volume migration.** The disk phase is correct when the
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
verification and the task ends Failed (`Status::data_loss`) instead of
Completed — but converging dirty rounds under load and a post-pause sweep
are unimplemented, so the mode is not claimed.

**Temporary fail-closed gate (#656):** until #394 is proven, the
user-facing API does not offer the live path. The BFF vm-mutate handler
and the control-plane mutation service reject `pause_first=false` (and a
missing field) with `400 Bad Request` — "live/concurrent-write migration
is not supported until #394 is proven; set pause_first=true
(stop-the-world)" — before dispatching the lifecycle RPC; both chvctl
commands reject the live path client-side before sending; and the WebUI
modal renders pause-first checked and disabled and always sends `true`.
The VM is paused for the full disk+memory transfer. Live migration
returns when #394 is proven; this gate is not a capability claim.

## State Machine

```
                    ┌──────────────────────────────────────────────┐
                    │                                              │

  ┌─────────┐    ┌──▼──────────┐    ┌──────────────┐    ┌────────────────┐
  │ Pending  │───►│ PreCopyDisk │───►│ConvergingDisk│───►│MemoryMigration │
  └─────────┘    └─────────────┘    └──────────────┘    └────────────────┘
                       │                    │                     │
                       │ (fail)             │ (fail)              │ (fail)
                       ▼                    ▼                     ▼
                  ┌──────────┐        ┌──────────┐         ┌──────────┐
                  │RolledBack│        │RolledBack│         │  Failed  │
                  └──────────┘        └──────────┘         └──────────┘

    ┌────────────────┐    ┌────────────┐    ┌───────────┐
    │MemoryMigration │───►│   Paused   │───►│ Completed │
    └────────────────┘    └────────────┘    └───────────┘
                               │
                               │ (fail)
                               ▼
                          ┌──────────┐
                          │RolledBack│
                          └──────────┘
```

## Phases

### Phase 1: PreCopyDisk
**Trigger:** MigrateVm operation dispatched by orchestrator

**Actions:**
1. CP validates preconditions (`validate_preconditions`, `crates/chv-controlplane-service/src/migration.rs`): source node heartbeat fresh (≤ 30 s), dest node in a schedulable state (not Maintenance/Draining/Failed/Unreachable, not scheduling-paused), no other migration in progress for the VM. There is no destination capacity (CPU/memory/disk) check
2. CP dispatches `MigrateVm` to the source agent; the agent calls the local stord's `TriggerDiskMigration` (stord Unix socket) once per attached volume, passing the destination stord's endpoint (`crates/chv-agent-core/src/migration.rs`)
3. Overlay participation for the destination node is not part of the migration flow: fabric plans are fanned out by the orchestrator's overlay arm (ADR-021, `crates/chv-controlplane-service/src/overlay.rs`); the migration flow's only overlay interaction is the post-migration gratuitous ARP (Phase 5)
4. Source stord: validates the destination endpoint against `migration_dest_allowlist`, checks volume health, enables dirty tracking on the volume, and spawns the migration sender, which connects to the destination stord's mTLS migration receiver and opens the gRPC block stream (`crates/chv-stord-core/src/handlers.rs`, `crates/chv-stord-core/src/migration/sender.rs`)
5. Progress: the agent polls stord's `GetDiskMigrationStatus` (phase, convergence_round, dirty_blocks_remaining, bytes_transferred, total_bytes) and reports `MigrationProgress` to the CP, which persists it in the migrations table (`crates/chv-agent-core/src/migration.rs`, `crates/chv-controlplane-service/src/migration.rs`)

**Note:** the source agent derives the destination stord endpoint as
`https://{destination_node_id}:50052`, treating the destination node id as a
resolvable host (`crates/chv-agent-core/src/agent_server.rs`). The
destination stord's migration receiver actually listens on the
operator-configured `migration.listen_addr` — deployments must keep the two
aligned (see Configuration).

**Exit conditions:**
- Success: bulk copy complete (all blocks transferred at least once) → ConvergingDisk
- Failure: dest volume creation fails, stream error, timeout → RolledBack

### Phase 2: ConvergingDisk
**Actions:**
1. Source stord: each round atomically snapshots-and-clears the dirty bitmap (`snapshot_and_clear_dirty_bitmap`), streams only the dirty blocks (bracketed by `RoundStart` / `RoundComplete`), and waits for the round acknowledgment before the next round (`crates/chv-stord-core/src/migration/sender.rs` `dirty_sync_rounds`)
2. Ack protocol: the receiver acks every 64 chunks while streaming, answers every `RoundComplete` with an `Ack` carrying its cumulative sequence number, and flushes its ack window at boundaries (`FinalSync`, pre-`FinalizeAck`), so the sender's per-round drains complete for arbitrary chunk counts (`crates/chv-stord-core/src/migration/receiver.rs`, issue #391)
3. CP monitors: dirty_blocks_remaining / convergence_round, reported by the agent from stord's `GetDiskMigrationStatus` and persisted in the migrations table; `wait_for_convergence` polls that table (`crates/chv-controlplane-service/src/migration.rs`). The convergence declaration is guarded (#582): the dirty-threshold check requires `convergence_round >= 1` (stord sets the round to 1 at the top of dirty-sync round 1, before any dirty count exists — round 0 means bulk copy has not finished), so the phase cannot exit while bulk copy is still running, with the disclosed exception of multi-volume skew (an early volume converged while a later one still bulks — see the adoption record `docs/design/issue-394-write-canary.md` for that boundary and its cost); diskless VMs (no dirty rounds ever) converge on the agent-reported `MemoryMigration` phase. Liveness is row freshness (the agent reports every ~5 s and the agent daemon flushes its pending telemetry queue every 5 s tick; a row un-updated for 90 s means the agent stopped reporting), and once dirty sync has begun the loop proceeds to the memory phase at the round cap — matching stord's forced cutover at MAX_DIRTY_ROUNDS
4. Convergence check — constants, not config (see Configuration): a round with dirty_blocks == 0 ends the phase; dirty_blocks < DIRTY_THRESHOLD (1024 blocks = 4 GiB at the 4 MiB block size) ends the phase; a hard cap of MAX_DIRTY_ROUNDS (10) forces the exit (`crates/chv-stord-core/src/migration/sender.rs`)

**Exit conditions:**
- Success: dirty count below threshold (or zero) → MemoryMigration
- Forced: MAX_DIRTY_ROUNDS (10) reached → MemoryMigration (forced cutover)
- Failure: stream error, source node unreachable, timeout → RolledBack

### Phase 3: MemoryMigration
**Actions:**
1. CP instructs dest agent: open the Cloud Hypervisor migration receiving socket (TCP, port from pool)
2. Dest agent calls the Cloud Hypervisor API: `PUT /api/v1/vm.receive-migration` with `{"receiver_url": "tcp://0.0.0.0:{port}"}`
3. CP instructs source agent: start memory migration to dest socket
4. Source agent calls the Cloud Hypervisor API: `PUT /api/v1/vm.send-migration` with `{"receiver_url": "tcp://{dest_ip}:{port}"}`
5. Cloud Hypervisor handles iterative memory pre-copy (dirty pages tracked internally by Cloud Hypervisor)
6. CP monitors: `wait_for_memory_migration` polls the migration record's phase (driven by agent-reported `MigrationProgress`) until Paused/Completed (`crates/chv-controlplane-service/src/migration.rs`)

**Note:** by the time memory migration starts, the disk phase has already
finished — including the VM pause for the final disk sync (see Phase 2/4):
the agent runs disk pre-copy to completion, then calls send-migration
(`crates/chv-agent-core/src/migration.rs` `source_migration_with_disk_precopy`).

**Exit conditions:**
- Success: memory transfer completes (the migration record reaches Paused/Completed) → Paused
- Failure: dest agent unreachable, Cloud Hypervisor API error, timeout → Failed (NOT RolledBack — partial state may exist on dest)

### Phase 4: Paused (Final Sync)
**Actions:**
1. Disk final sync happens inside the agent-driven disk phase, before memory migration: when the stord task reaches PausedFinalSync it signals `needs_vm_pause`; the agent pauses the VM via the Cloud Hypervisor API and signals stord back via `ResumeDiskMigration(vm_paused=true)` (`crates/chv-agent-core/src/migration.rs`, `crates/chv-stord-core/src/handlers.rs`)
2. Source stord sends `FinalSync{vm_paused: true}` — there is **no post-pause dirty sweep** (issue #394): the last dirty round ran before the pause; for a quiescent volume there is nothing left to flush
3. Dest stord flushes its ack window at the FinalSync boundary; the sender drains until every chunk is acknowledged (fail-closed: a CRC-mismatch or write-error Ack fails the migration)
4. Finalize verification: the sender computes a versioned full-volume SHA-256 digest (`"sha256:"` + 32 raw bytes) over the source — after the pause — and carries it in `FinalizeComplete.volume_checksum`; the receiver re-computes it over the destination and answers `FinalizeAck{verified}`. `verified=false` (digest mismatch, unknown digest format, unreadable destination) fails the task with `Status::data_loss`, so Completed genuinely means "destination verified" (`crates/chv-stord-core/src/migration/volume_digest.rs`, `migration/sender.rs`, `migration/receiver.rs`)
5. CP explicitly pauses the source VM (best-effort — it may already be paused by the disk final sync / Cloud Hypervisor send-migration), then instructs the dest agent to resume the VM on the destination (`crates/chv-controlplane-service/src/migration.rs`)
6. On any post-pause failure the agent best-effort resumes the VM on the source (`PausedVmGuard::resume_if_paused`, `crates/chv-agent-core/src/migration.rs`)

**Exit conditions:**
- Success: VM confirmed running on dest → Completed (proceed to Phase 5)
- Failure: dest fails to resume, final sync error → RolledBack (source still has everything)

### Phase 5: Completed (Validation and Cleanup)
**Actions:**
1. CP completes the migration atomically: migration → Completed and VM placement → dest node in a single SQLite transaction (`complete_migration_atomically`, `crates/chv-controlplane-service/src/migration.rs`)
2. Dest nwd: sends gratuitous ARP for the VM's IP/MAC (best-effort, via the dest agent — `notify_overlay_after_migration`, `crates/chv-controlplane-service/src/migration.rs`)
3. CP best-effort disables dirty tracking on the source volumes (`disable_source_dirty_tracking`, per ADR-012)
4. The source volume copy is NOT deleted automatically — the source volume remains on the source node after a successful migration

**Note:** This phase is post-completion. Failures here do not affect the VM (already running on dest). Logged as warnings.

## Rollback Behavior

| Failure Point | Action | VM Continuity |
|---|---|---|
| Phase 1 (PreCopyDisk) | Mark migration RolledBack (`rollback_precopy`, `crates/chv-controlplane-service/src/migration.rs`); the agent's cancel/failure paths stop the disk phase. Partial destination volumes are not auto-deleted (see Recovery model) | VM never stopped, continues on source |
| Phase 2 (ConvergingDisk) | Same as Phase 1 | VM never stopped, continues on source |
| Phase 3 (MemoryMigration) | Cannot cleanly rollback if Cloud Hypervisor is mid-transfer. Mark Failed. | Manual recovery required |
| Phase 4 (Paused) | If dest fails to resume: best-effort resume on source (`rollback_paused`; the agent's `PausedVmGuard` also best-effort resumes on every post-pause failure path) | Brief pause experienced by VM |

## Timeouts

| Phase | Default Timeout | Calculation |
|---|---|---|
| PreCopyDisk | disk_size_gb * 60s | 100GB disk = ~100 min at 1Gbps |
| ConvergingDisk | 300s per round, total 3000s | 10 rounds max |
| MemoryMigration | memory_size_gb * 30s + 120s | 16GB RAM = ~10 min |
| Paused (final sync) | 60s | Should be fast (small residual) |
| Total | sum of above + 300s buffer | Hard abort if exceeded |

Phase timeouts are calculated by `PhaseTimeouts::calculate`
(`crates/chv-controlplane-service/src/migration.rs`); the `timeout_multiplier`
(default 1.0) scales them. Separately, the stord-level protocol has a
hardcoded 30 s ack-wait timeout (`crates/chv-stord-core/src/migration/flow_control.rs`).

## Required Proto Messages

```protobuf
message MigrateVmRequest {
  string vm_id = 1;
  string source_node_id = 2;
  string destination_node_id = 3;
  MigrationConfig config = 6;
}

message MigrationConfig {
  uint32 dirty_threshold_blocks = 1;  // default: 1024
  uint32 max_convergence_rounds = 2;  // default: 10
  uint32 block_size_bytes = 3;        // default: 4194304 (4MB)
  uint32 total_timeout_seconds = 4;   // 0 = use calculated default
  bool pause_first = 5;               // issue #394 Option C: stop-the-world
}

message MigrationProgress {
  string vm_id = 1;
  string operation_id = 2;
  MigrationPhase phase = 3;
  uint64 bytes_transferred = 4;
  uint64 total_bytes = 5;
  uint32 convergence_round = 6;
  uint64 dirty_blocks_remaining = 7;
  float progress_percent = 8;
}

enum MigrationPhase {
  MIGRATION_PHASE_UNSPECIFIED = 0;
  MIGRATION_PHASE_PENDING = 1;
  MIGRATION_PHASE_PRECOPY_DISK = 2;
  MIGRATION_PHASE_CONVERGING_DISK = 3;
  MIGRATION_PHASE_MEMORY_MIGRATION = 4;
  MIGRATION_PHASE_PAUSED = 5;
  MIGRATION_PHASE_COMPLETED = 6;
  MIGRATION_PHASE_FAILED = 7;
  MIGRATION_PHASE_ROLLED_BACK = 8;
}
```

Note: the `MigrationConfig` values are advisory CP-side inputs (they size
convergence polling and phase timeouts); the stord-side convergence threshold,
round cap, and block size are constants — see Configuration. `pause_first`
(issue #394, Option C) is the exception — the agent reads it: when true, the
source agent pauses the VM before triggering stord's bulk copy (the
`PAUSED_PRE_COPY` handshake) instead of only at final sync, making the disk
transfer correct by construction at the cost of stop-the-world downtime; it
is set per-migration from the BFF vm-mutate migrate action's `pause_first`
field (`chvctl migrate start --pause-first` / `chvctl vm migrate
--pause-first`, and the WebUI migrate modal's mandatory checked control —
landed via #582, with an honest downtime warning). Since #656 the
user-facing surfaces require `true`: the BFF and chvctl fail closed on
the live path (see *Temporary fail-closed gate*), so every
operator-initiated migration is stop-the-world until #394 is proven.

## Operation Integration

- New operation type: `MigrateVm` in orchestrator dispatch table
- Operation follows existing retry/timeout pattern (but migration is NOT retried automatically — failure requires operator review)
- Operation reaper: scans every 60 s; a migration in a non-terminal phase whose `updated_at` has not moved for the stall timeout (default 7200 s, overridable with `CHV_MIGRATION_STALL_TIMEOUT_SECS`; this is a no-progress threshold, not a total-runtime cap) is marked Failed and its parent operation failed. The source VM is resumed only after a liveness gate: the row is terminal, no progress has arrived for at least `max(stall timeout, 90 s)`, and the source node heartbeat is fresh (≤ 30 s). If the gate fails, the VM stays paused for operator review (`crates/chv-controlplane-service/src/migration_reaper.rs`).

## Configuration

The operator-facing migration config surface is the stord `[migration]` section
(`StordMigrationConfig`, `crates/chv-config/src/lib.rs`) plus the stord-level
`migration_dest_allowlist`. There are no `migration.*` tuning knobs: the
protocol parameters below are hardcoded constants.

### `[migration]` (stord config, `crates/chv-config/src/lib.rs`)

`migration.enabled` is the single master switch for both halves of migration
TLS. All validation is fail-closed at startup
(`crates/chv-stord-core/src/migration/tls_config.rs`,
`cmd/chv-stord/src/main.rs`): missing/unreadable/invalid material is a
startup error, never a plaintext or skipped-verify fallback.

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
- `enabled = false` with **any** receiver field set → startup error (an
  operator who believes migration is off must not get an inbound TCP listener)
- `enabled = true` with **no** client fields → destination-only stord: no
  client identity, outbound migration actions fail with a
  `failed_precondition` error (issue #401). The client half is
  all-or-nothing *when any client field is set*. The receiver half is
  all-or-nothing *when any receiver field is set*; `enabled = true` with
  **no** receiver fields is a legitimate source-only stord (no listener,
  logged at startup)
- `enabled = true` with **neither** half configured → startup error (an
  enabled migration section that configures nothing is a misconfiguration)
- Receiver files unreadable, keypair mismatch, empty/invalid CA bundle, or an
  unparseable `listen_addr` → startup error

Stord additionally enforces `migration_dest_allowlist` (top-level stord key,
not under `[migration]`): the destination endpoint host of every
`TriggerDiskMigration` must be listed (empty list = allow all)
(`crates/chv-stord-core/src/handlers.rs`).

### Hardcoded protocol constants (not config)

| Constant | Value | Where |
|---|---|---|
| Migration block size | 4 MiB (`DIRTY_TRACKING_BLOCK_SIZE`; must equal the bitmap's per-bit block) | `crates/chv-stord-backends/src/trait.rs` |
| Dirty convergence threshold (`DIRTY_THRESHOLD`) | 1024 blocks | `crates/chv-stord-core/src/migration/sender.rs` |
| Max dirty rounds (`MAX_DIRTY_ROUNDS`) | 10 | `crates/chv-stord-core/src/migration/sender.rs` |
| Ack interval (`DEFAULT_ACK_INTERVAL`) | 64 chunks | `crates/chv-stord-core/src/migration/receiver.rs` |
| Send window (max unacked) | 128 chunks | `crates/chv-stord-core/src/migration/flow_control.rs` |
| Ack-wait timeout | 30 s | `crates/chv-stord-core/src/migration/flow_control.rs` |
| Max gRPC message size (`MAX_MIGRATION_MESSAGE_SIZE_BYTES`) | 8 MiB (2 × block size; a `BlockChunk` carries a full 4 MiB block plus protobuf overhead, exceeding tonic's 4 MiB default) | `crates/chv-stord-core/src/migration/mod.rs` |
| Cloud Hypervisor memory-migration port pool | 49152–49200 | `crates/chv-agent-core/src/migration.rs` |

The CP-side `MigrationConfig` values (`dirty_threshold_blocks`,
`max_convergence_rounds`, `block_size_bytes`, `total_timeout_seconds`) exist
in the `MigrateVmRequest` proto and in
`crates/chv-controlplane-service/src/migration.rs` (defaults 1024 / 10 /
4 MiB / calculated), where they size the CP's convergence polling and phase
timeouts; they are not stord config-file knobs and do not change the stord
constants above.

## Implementation Status

| Component | File | Status |
|---|---|---|
| MigrateVm operation dispatch | orchestrator.rs | DONE |
| Phase 1: PreCopyDisk orchestration | migration.rs | DONE |
| Phase 2: ConvergingDisk monitoring | migration.rs `wait_for_convergence` | DONE — agent polls stord `GetDiskMigrationStatus` and reports `MigrationProgress`; CP polls the persisted state |
| Phase 3: MemoryMigration via Cloud Hypervisor | migration.rs | DONE |
| Phase 4: Paused / final sync | migration.rs, agent-core/migration.rs | DONE — pause handshake + `FinalSync{vm_paused:true}` + finalize digest verification; no post-pause dirty sweep (issue #394, see Claimed mode) |
| Phase 5: Completed / cleanup | migration.rs | DONE |
| Rollback per phase | migration.rs | DONE |
| Migration reaper (stale ops) | migration_reaper.rs | DONE |
| Phase timeouts | migration.rs `PhaseTimeouts` | DONE |
| MigrationProgress proto | control-plane-node.proto | DONE |
| Source stord: bulk copy | sender.rs `bulk_copy()` | DONE |
| Source stord: dirty sync rounds | sender.rs `dirty_sync_rounds()` | DONE — atomic snapshot-and-clear per round, `RoundStart`/chunks/`RoundComplete`, `DIRTY_THRESHOLD`/`MAX_DIRTY_ROUNDS` constants |
| Source stord: flow control | flow_control.rs | DONE |
| Source stord: CRC32 per chunk | sender.rs | DONE |
| Source stord: sparse detection | sender.rs `is_all_zeros()` | DONE |
| Source stord: finalize digest | volume_digest.rs, sender.rs | DONE — versioned SHA-256 over the full source, sent in `FinalizeComplete.volume_checksum` (issue #392) |
| Dest stord: receiver | receiver.rs | DONE |
| Dest stord: CRC validation | receiver.rs | DONE |
| Dest stord: destination verification | receiver.rs `verify_destination()` | DONE — re-computes the digest over the destination; `verified=false` ⇒ sender fails with `Status::data_loss` (issue #392) |
| Ack protocol (interval + boundary acks) | receiver.rs, sender.rs | DONE — ack every 64 chunks, round ack on `RoundComplete`, window flush at `FinalSync`/pre-`FinalizeAck` (issue #391) |
| mTLS on migration channel (client half) | sender.rs, tls_config.rs | DONE — sender refuses to run without `MigrationTlsConfig` (no plaintext fallback); identity loaded and validated at startup |
| mTLS on migration channel (receiver half) | server.rs `serve_migration_tls`, tls_config.rs | DONE — TLS TCP listener with mandatory client-cert auth, all-or-nothing receiver fields (issue #390) |
| gRPC message size limit (8 MiB, both serving paths) | migration/mod.rs, server.rs | DONE — raised above tonic's 4 MiB default so real 4 MiB chunks decode |
| Stord disk-migration control API | handlers.rs | DONE — `TriggerDiskMigration` (allowlist + dirty-tracking enable), `GetDiskMigrationStatus`, `ResumeDiskMigration` |
| Agent pause coordination | agent-core/migration.rs | DONE — `PausedVmGuard`: pauses VM on stord's `needs_vm_pause`, best-effort resume on every post-pause failure path |
| Dirty block tracking trait methods | StorageBackend trait | DONE |
| Migration port allocation | agent-core/migration.rs | DONE (TOCTOU race present) |
| Post-migration gARP | crates/chv-controlplane-service/src/migration.rs | DONE |
| FDB update after migration | overlay.rs | N/A — per-VTEP FDB push retired (ADR-021): kernel MAC learning + gratuitous ARP provide correctness |

### Known constraints

1. **Concurrent-write migration is not claimed** — see *Claimed mode / not
   claimed* above; issue #394 is the durable record. The dirty bitmap only
   observes stord's own `write_block` (its only product caller is the
   migration receiver), and the sender performs no post-pause final sweep.

2. **Agent endpoint assumption**: the source agent derives the destination
   stord endpoint as `https://{destination_node_id}:50052`
   (`crates/chv-agent-core/src/agent_server.rs`), while the receiver listens
   on the operator-configured `migration.listen_addr`. Deployments must
   configure `listen_addr` on port 50052 (and use node ids resolvable as
   hosts) for agent-driven migrations to connect.

3. **`docs/specs/component/disk-migration-protocol-spec.md`** (the stord-level
   protocol spec) was corrected to the same M4.6 reality in #403 (issue #399):
   its status table now marks dirty rounds / mTLS / verification DONE and it
   carries the same claimed/not-claimed boundary (issue #394).

## Non-goals
- Automatic retry of failed migrations (operator must review)
- Multi-VM batch migration (one at a time per orchestrator)
- Post-copy disk fallback (v1 is pre-copy only)
- Cross-control-plane migration (each CP manages its own cluster)
- Concurrent-write ("live") disk migration — not claimed; see *Claimed mode / not claimed* (issue #394)

## Recovery model
- If CP crashes during migration: on restart, find in-progress migration operations, mark them Failed (cannot safely resume mid-migration)
- If source agent crashes during Phase 1-2: the reaper eventually marks the migration Failed and best-effort resumes the source VM; the partial destination volume is NOT deleted automatically (the receiver only releases its backend handle when the stream ends) — manual cleanup required
- If dest agent crashes during Phase 3-4: mark Failed, source resumes VM (manual cleanup of dest)
