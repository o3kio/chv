# Issue #394 — Concurrent-write storage migration: write canary (Option A) and pause-first mode (Option C)

Adoption record. Ruling 2026-10-08 (session, recorded on the issue):
**DP1 adopted** (state correction — the loss is loud-but-late post-#392,
not silent); **DP2 adopted** — Option A (write canary, fail fast) lands
now; **DP3** — Option C (pause-first opt-in mode) was ruled a tight
follow-up, its own PR, and has since landed (§5); **DP4 adopted** —
Options B/D parked with recorded triggers; **DP5** — the M4.6 recorded
boundary stays as-is under A.

The options write-up with the full analysis lives on the issue
(2026-10-08, `#394` comment "options write-up — request for a design
ruling"). This document records what landed and the boundaries that
hold after landing.

## 0. The state correction (DP1)

The issue was filed 2026-10-02 against a pre-#392 sender and titled the
defect *silent* data loss. The #392 finalize verification (merged
2026-10-03) changed the outcome: the sender computes a full-volume
SHA-256 over the source **post-pause**, the receiver re-computes it over
the assembled destination, and any divergence fails the task
`data_loss` with the VM best-effort resumed. Concurrent-write migration
is therefore **loud, not silent** — but **late**: a full transfer plus
two O(volume) digest passes to discover what a write canary can report
in microseconds. That lateness is the surviving defect Option A closes.

The three recorded facts the design rests on:

- **R1** — the dirty bitmap is stord-private userspace state, marked
  only by `LocalFileBackend::write_block`, whose only product caller is
  the migration *receiver* (on the destination volume). The plan §2.2
  write interception (io_uring / handle wrapping) was never implemented.
  On a source volume the bitmap is always zero; the dirty rounds are
  protocol machinery, not live-migration support.
- **R2** — there is no post-pause final sweep: after the pause the
  sender sends `FinalSync` and finalizes without re-reading anything.
  (Option B's subject, not A's.)
- **R3** — the practical outcome pre-A: fail late, maximally wasteful.

## 1. What landed (Option A)

**Mechanism.** `StorageBackend::write_canary_probe(volume_id, handle)`
returns a `WriteCanaryFingerprint` — the backing store's
`(mtime, ctime, size)` plus its `WriteCanaryCapability`:

- `FileStat` — regular-file backing (the local backend, implemented):
  every `write(2)` through **any** descriptor — Cloud Hypervisor's
  included — updates mtime/ctime, so the stat triple is a reliable
  tripwire for *whether* the source was written. `atime` is
  deliberately not sampled (the sender's own `read_block` reads would
  trip it on `relatime` mounts).
- `Unavailable` — the default trait implementation: block devices
  (LVM, iSCSI — data writes do not update the device node's
  timestamps) and RADOS objects (Ceph — no local path). The `Box<dyn
  StorageBackend>` blanket impl delegates, so a boxed local backend
  keeps its canary.

The sender samples the baseline immediately before bulk copy and
re-checks it **at every dirty-round boundary and at the pre-pause
gate**. A change fails the migration with `failed_precondition`
carrying the distinct `source_modified_during_migration` token —
*before* the VM is paused (no resume needed) and *before* the finalize
digest is computed (the task's `finalize_volume_digest` stays empty:
the observable early-vs-late discriminator). A probe error fails
closed (`internal`), never silently disarming the canary. On
`Unavailable` the sender proceeds exactly as before and logs the
reason: fail-late at the digest, the pre-A behavior.

## 2. The layering contract (what the canary is and is not)

The canary is a **latency-of-failure optimization**, not a correctness
gate:

- It claims *whether* the source moved, never *where* — placement
  truth stays with the #392 whole-volume finalize digest.
- Its last check is the **pre-pause gate**; writes landing between the
  gate and the pause completing are beyond its reach **by design** and
  remain fail-closed via the post-pause digest. (Pinned end-to-end:
  `source_write_during_pause_window_fails_at_finalize_digest`.)
- Symmetrically, writes landing **before the baseline** (between the
  trigger and the MigrationReady handshake) are inside not-yet-read
  blocks: bulk copy transfers them, nothing diverges, and the
  migration legitimately converges. The canary's baseline is sampled
  before the first source read, so the covered window is exactly the
  transfer window.
- It does not make live migration supported. The quiescent-only
  contract is unchanged; a canary failure tells the operator to retry
  with the source quiesced — or to opt into pause-first mode (§5), now
  landed.

## 3. Test surface

- `migration_e2e.rs::concurrent_source_write_fails_fast` — an
  out-of-band in-place write during bulk copy (the guest-shaped path
  the bitmap cannot see) fails `failed_precondition` with the token,
  `needs_vm_pause` never set, `finalize_volume_digest` empty.
- `migration_e2e.rs::source_write_during_pause_window_fails_at_finalize_digest`
  — the §2 layering: a same-window write fails `data_loss` at
  finalize, digest non-empty.
- `migration_e2e.rs::dirty_rounds_converge_preseeded_writes` — the
  rewritten round-protocol test (pre-seeded dirty blocks, written
  *before* the baseline). The former
  `dirty_rounds_transfer_concurrent_writes` exercised a
  production-impossible shape — a stord-visible source write during
  migration — which is now, by design, a canary failure.
- Sender unit tests: no-op when unavailable; pass when unchanged; trip
  on mtime and on size-only movement; task marked Failed with the
  token.
- Local-backend unit tests: `FileStat` reported; an out-of-band
  in-place write trips; re-probe stable; foreign handles rejected
  fail-closed.

## 4. Parked with intent (DP4)

- **Option B — post-pause diff sweep** (true concurrent-write
  correctness up to the pause point, O(volume) pause window): reopen
  when a tenant-facing live-migration requirement lands.
- **Option D — dm-snapshot COW** (true live migration, bounded
  pause): reopen when bounded-downtime live migration is required;
  device-backed volumes only.

## 5. Option C — pause-first opt-in mode (landed)

The follow-up the DP3 ruling deferred to. What landed:

- **Stord** (`crates/chv-stord-core/src/migration/sender.rs`): the
  opt-in `pause_first` mode on the sender. When set, the VM-pause
  handshake runs **before any source byte is read**: the task enters
  the new `PausedPreCopy` phase with `needs_vm_pause = true`
  (observable with zero bytes transferred), the sender blocks until
  the pause is signaled, and only then samples the canary baseline
  and starts bulk copy. The transfer is correct by construction for
  any write pattern — the canary becomes a tripwire for non-VM
  writers, the dirty rounds converge trivially, the finalize digest
  verifies instead of catching loss. At the pre-pause gate the
  handshake is already satisfied (the VM has been paused since
  before bulk copy): the task transitions to `PausedFinalSync` for
  observability and the sender proceeds to `FinalSync` without
  waiting. Without a task attached the mode fails closed with
  `failed_precondition` **before connecting** — an operator who
  asked for the pause must never get a silent degradation to
  quiescent-assumed semantics.
- **Pause-signal latching** (`crates/chv-stord-core/src/handlers.rs`):
  the trigger handler now holds the task's pause-channel receiver
  open for the spawned sender's lifetime. Before, the receiver was
  dropped when the handler returned, so a `ResumeDiskMigration`
  arriving while the sender was still connecting (not yet
  subscribed at a pause gate) hit a closed watch channel: the send
  failed, the resume RPC errored, and the agent's resume-all loop
  aborted the whole migration. Pause-first makes that window
  near-deterministic for multi-volume VMs (the resume fires when
  the FIRST volume requests the pause, while siblings are still
  connecting); the same staggered shape existed latently in the
  default mode's final-sync pause. With the receiver held, the
  signal latches: a sender reaching its gate later observes the
  pause already signaled and proceeds.
- **Contract** (`proto/node/chv-stord-api.proto`):
  `TriggerDiskMigrationRequest.pause_first` and the
  `PAUSED_PRE_COPY` status phase; `proto/controlplane/
  control-plane-node.proto`: `MigrationConfig.pause_first` (the
  first config field the agent actually reads — the tuning fields
  remain defaults-only).
- **Agent** (`crates/chv-agent-core/src/migration.rs`): the poll
  handles `PausedPreCopy` with the same pause-and-resume handshake
  as `PausedFinalSync`; progress during a pause-first pause stays in
  the disk phase (`MIGRATION_PHASE_PRECOPY_DISK`) instead of
  falsely claiming the memory phase. The VM stays paused through
  disk and memory migration and is resumed on the destination —
  the same resume contract the default mode already had at its
  final-sync pause; only the pause's position moved.
- **Operator surface (temporary fail-closed gate, #656)**: until #394
  is proven the user-facing surfaces no longer offer the live path.
  The BFF vm-mutate migrate action and the control-plane mutation
  service reject `pause_first=false` (and a missing field) with a 400
  — "live/concurrent-write migration is not supported until #394 is
  proven; set pause_first=true (stop-the-world)" — before any lifecycle
  dispatch; a *present* non-bool value is likewise rejected rather than
  coerced. `chvctl migrate start --pause-first` and `chvctl vm migrate
  --pause-first` require the flag and fail closed client-side without
  sending a request. Nothing silently coerces false to true; the
  stord wire protocol still accepts false for internal/older callers.
- **Boundary (WebUI — #582, made mandatory by #656)**: the migrate
  modal now renders pause-first checked and disabled and always sends
  `true`. Its description states that live migration is disabled until
  #394 is proven and that the VM is paused for the full disk + memory
  transfer; the downtime warning is always visible. Threaded through
  the vm-mutate migrate action's `pause_first` JSON field (the BFF hop
  landed with #578). Version-skew note: the UI and BFF are
  release-co-packaged, and as with the API/chvctl path the
  fail-closed posture here is within-version only — a pre-`pause_first`
  BFF drops the unknown JSON key and would run the request live
  (the stord hop, by contrast, has the T=0 echo guard).
- **Boundary (version skew — guard landed 2026-10-09, #582)**: the
  T=0 echo guard is now in place: stord echoes `pause_first` in
  `TriggerDiskMigrationResponse`, and the agent fails the migration
  at trigger time when a pause-first request gets no echo back (a
  pre-guard stord drops the unknown field and would silently run the
  default quiescent-assumed path). Default-mode requests never
  require an echo (one-directional guard). Within-version, the
  fail-closed posture also holds at the sender (no task ⇒ fail
  before connecting).
- **Boundary (pre-existing CP convergence looseness — fixed
  2026-10-09, #582)**: `wait_for_convergence` used to declare
  convergence on `dirty_remaining <= threshold` without a
  bytes-or-phase guard, firing on the first 5 s status poll of *any*
  migration — default mode included — while the disk transfer was
  still running (the memory phase then ran under its own timeout
  budget with the VM paused for the remainder of the disk transfer).
  The fix guards the dirty-threshold check with `convergence_round
  >= 1`: stord sets the round to 1 at the top of dirty-sync round 1,
  before any dirty count exists, so round 0 means bulk copy has not
  finished (a Completed task always passes through at least round 1;
  the agent forwards the max across volumes). Diskless VMs — which
  skip the disk loop and never run a dirty round — converge via the
  agent-reported `MemoryMigration` phase instead. Making the guard
  real exposed that two of the loop's other safety nets were written
  against a wrong model of the telemetry and would have false-fired
  the moment they went live, so they were fixed in the same change:
  the stall detector is re-keyed from byte movement (stord's
  `bytes_transferred` only accumulates dirty re-send bytes — it
  freezes at 0 through bulk copy, so a byte-based detector
  false-fires ~65 s into any legitimate bulk) to row freshness
  (`updated_at` older than 90 s ≈ 18 missed ~5 s reports means the
  agent stopped reporting); and the dirty-sync caps (round cap /
  poll cap) are gated on `round >= 1`, count only dirty-sync poll
  time (bulk-copy polls are excluded), and now PROCEED to the memory
  phase instead of failing — matching stord's own forced cutover at
  MAX_DIRTY_ROUNDS (the sender moves to final sync there, it does
  not fail) and the branch's original stated intent (erroring there
  would roll back migrations stord is completing via forced
  cutover). The freshness net required fixing a pre-existing agent
  telemetry delivery bug that the vacuous check had masked: the
  agent's migration progress reporter only ENQUEUED messages into
  the cache's pending queue, which was drained solely on
  (re)connect — during a stable connection nothing flushed it, so
  the CP's migration row never updated at all (this also stranded
  the agent's MemoryMigration/Completed phase reports, meaning the
  memory-phase wait could never observe completion either). The
  agent daemon now drains the queue on every 5 s tick while
  connected — dispatching outside the cache lock, each dispatch
  bounded by a 10 s deadline (a hung-but-established connection is
  converted into a connectivity failure and a client drop instead
  of wedging the tick loop), and subsuming the old dedicated
  reconnect flush (which held the cache lock across unbounded
  dispatch). Disclosed residual, pinned by a boundary test: in
  multi-volume skew — an early volume converged (round >= 1, dirty
  0) while a later volume is still in bulk copy — convergence is
  declared early through either of two triggers: the max-dirty
  signal is 0 (the round-based check passes), and in default mode
  the agent reports `MemoryMigration` at the early volume's
  final-sync pause (the phase-based check passes). The row cannot
  express that state today (stord's `bytes_transferred` excludes
  bulk bytes; the agent's phase vocabulary has no "all volumes
  finished bulk" value). The migration still completes correctly
  (the final sync waits for every volume); the cost is that the
  memory phase's timeout budget starts early. Closing it needs a
  telemetry contract change. The two fixes in this change (CP
  guard, agent drain) are causally coupled and must deploy
  together or agent-first: a new CP with a pre-fix agent never
  sees refreshed migration rows over a stable connection and
  would roll back every ConvergingDisk migration at the 90 s
  freshness bound. One direction of observable change
  worth naming for operators: bulk-copy time is now genuinely spent
  in ConvergingDisk and bounded by `converging_disk_total_secs`
  (default 3000 s) — a bulk slower than that now fails the phase
  with a convergence timeout, where the vacuous check previously
  masked it (the memory phase absorbed the transfer instead). Pinned
  by default-mode regression tests, each behavior sabotage-verified.
- **Tests**: `migration_e2e.rs::pause_first_pauses_before_bulk_copy`
  (pause arrives in `PausedPreCopy` with zero bytes, stays blocked,
  completes verified; a write during the blocked window is
  pre-baseline and legitimately transferred);
  `migration_e2e.rs::pause_signal_latches_for_senders_not_yet_at_
  their_gate` (the resume-latch fix, driven through the real
  `TriggerDiskMigration` handler against a silent destination — the
  sender is provably stuck pre-handshake when the resume arrives);
  `sender::tests::pause_first_without_task_fails_closed_before_
  connecting`; agent poll test `paused_pre_copy_drives_pause_
  handshake_and_reports_disk_phase`;
  `daemon_clients::stord_trigger_disk_migration_carries_pause_first`
  (the agent→stord wire hop); chvctl contract test
  `vm_migrate_pause_first_row` pinning the CLI→BFF→mutation thread;
  BFF route rows `vm_migrate_pause_first.rs` (the non-bool rejection,
  the bool forward, the absent-field default).
