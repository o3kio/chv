# Runbook: Audit Outbox

**Scenario:** the transactional audit outbox is not draining. A
privileged mutation that adopted the outbox has committed, but its
`audit_log` row is still `pending`, so the admin audit API does not show
it yet.

**Severity:** **SEV-3** by default — evidence is **delayed, not lost**.
Escalate when an active security investigation needs the missing
window, or when the same database problem is affecting the request path.

**Automation level:** dispatch, retry, lease reclamation, and pruning of
old dispatched rows are automatic. There is **no dead-letter and no
attempt ceiling**: the dispatcher retries forever. This runbook covers
detection, triage, and safe restart. It promises no manual data-loss
recovery path we cannot honour.

> **Rollout status (#706 half 1).** The outbox is live for the adopted
> BFF routes, the three adopted gRPC enrollment RPCs, the five adopted
> gRPC reconcile RPCs, the five adopted gRPC node-lifecycle
> transitions, and the three adopted gRPC volume snapshot-family RPCs
> listed below.
> On those paths a pending row means the mutation **committed** and its
> evidence row is waiting for the dispatcher. The response was normally
> 2xx, but a committed mutation whose final response was later patched
> to a non-2xx (e.g. a downstream middleware failure) keeps its pending
> row too — with `outcome` and `status_code` rewritten to match; see the
> patch note below. The rest of the gRPC lifecycle surface and every
> unadopted BFF route still use the direct post-hoc insert; their rows
> never appear in this table, and their narrow crash window remains open
> (tracked under #706).
>
> Adopted BFF routes: `POST /v1/users/create`, `/v1/users/update`,
> `/v1/users/delete`, `/v1/tokens/create`, `/v1/tokens/revoke`,
> `/v1/networks/create`, `/v1/networks/update`, `/v1/networks/delete`,
> and the final transaction of `POST /v1/vms/import`.
>
> Adopted gRPC RPCs: `EnrollNode`, `RotateNodeCertificate`, and
> `ReportBootstrapResult`; `ApplyNodeDesiredState`,
> `ApplyVmDesiredState`, `ApplyVolumeDesiredState`,
> `ApplyNetworkDesiredState`, and `AcknowledgeDesiredStateVersion`;
> `PauseNodeScheduling`, `ResumeNodeScheduling`, `DrainNode`,
> `EnterMaintenance`, and `ExitMaintenance`; and `SnapshotVolume`,
> `RestoreVolume`, and `DeleteVolumeSnapshot`.
>
> **Enrollment specifics.** For `RotateNodeCertificate` and
> `ReportBootstrapResult` the outbox row commits with the single
> mutation transaction. For `EnrollNode` it commits with the **final**
> fail-closed write of the sequence (fabric-identity registration when
> the node reports a key, otherwise the version-history append): the
> one-time bootstrap-token consumption, the node row, and the inventory
> row commit earlier. A pending row therefore means "the enrollment's
> final write committed, evidence is queued". The inverse is **not**
> true: the token can be consumed and a crash can leave a pending-less,
> partially applied enrollment with no row at all. That gap is a
> documented residual risk, not something this table can show.
>
> **Reconcile specifics.** Each `Apply*DesiredState` RPC commits the row
> with its **desired-state write** (the fragment upsert). The
> convergence event appended afterwards is a separate commit: a crash
> between the two leaves the desired-state change and its evidence row
> durable with no `events` row, and a late event-append failure patches
> the already-committed row to a non-2xx outcome.
> `AcknowledgeDesiredStateVersion` commits the row with the
> observed-state acknowledgement; the operation-status update and the
> event that follow are separate commits with the same residual shape.
> A missing observed-state row fails the acknowledgement before the
> commit, so no outbox row exists and the failed request appears only in
> `audit_log`.
>
> **Lifecycle specifics (node transitions).** `PauseNodeScheduling`,
> `ResumeNodeScheduling`, `DrainNode`, `EnterMaintenance`, and
> `ExitMaintenance` commit the row with the **intent write**
> (`set_scheduling_paused`, `set_drain_intent`, or
> `set_state_preserving_policy`). A pending row here means: the node's
> scheduling/drain/maintenance state change **committed**, and its
> evidence is queued. Their `operations` row and `OperationStarted` event
> commit **earlier** and are not coupled: a crash between them and the
> intent leaves no state change and **no row** (pre-existing window), and
> a crash after the intent commit but before the operation status update
> leaves the applied state and a `success` row with a possibly stale
> operation status. An intent failure (e.g. pause/resume on a node with
> no `node_desired_state` row) rolls the enqueue back and writes exactly
> one direct failed row; no pending row appears. The rest of the
> lifecycle surface is still post-hoc.
>
> The BFF twin is also still post-hoc: `POST /v1/nodes/mutate`
> (`node.mutate`, the WebUI/chvctl path) calls the same five service
> methods in-process without a ticket, so a pause/drain/maintenance
> change made from the WebUI keeps the post-hoc insert and its crash
> window and never appears in this table.
>
> **Volume snapshot specifics.** `SnapshotVolume`, `RestoreVolume`, and
> `DeleteVolumeSnapshot` commit the row with the **intent write**
> (`set_volume_snapshot`: `snapshot_op = create`/`restore`/`delete`
> plus `snapshot_name`). A pending row here means the snapshot intent
> **committed** and its evidence is queued. Their `operations` row and
> `OperationStarted` event commit **earlier** and are not coupled: a
> crash between them and the intent leaves no state change and **no
> row** (pre-existing window), and a crash after the intent commit but
> before the operation status update leaves the applied intent and a
> `success` row with a possibly stale operation status. A `Deleting`
> tombstone refuses the patch inside the write transaction, so a
> non-crash conflict after the journal produces exactly one direct
> failed row (409) and **no pending row**, with the volume row
> unchanged.
>
> The BFF twin routes are still post-hoc: `POST /v1/volumes/snapshot`
> (`volume.snapshot`), `POST /v1/volumes/restore-snapshot`
> (`volume.restore_snapshot`), and `POST /v1/volumes/delete-snapshot`
> (`volume.delete_snapshot`) call the same three service methods
> in-process without a ticket, so a snapshot/restore/delete made from
> the WebUI keeps the post-hoc insert and its crash window and never
> appears in this table.
>
> A non-zero `chv_audit_outbox_pending` on a build without this adoption
> (or with no adopted traffic) means something wrote the table out of
> band — investigate before assuming adoption.

## What exists where

| Piece | Location |
|---|---|
| `audit_outbox` (plumbing: pending/dispatched rows) | operational database (`[database] url`, default `sqlite:///var/lib/chv/controlplane.db`) |
| `audit_log` (the evidence table) | same database |
| Dispatcher worker | inside `chv-controlplane`, spawned at boot next to the audit retention worker |
| Configuration | `/etc/chv/controlplane.toml`, `[audit]` (`outbox_dispatch_interval_secs`, `outbox_retention_days`) |
| Admin read surface | `POST /v1/audit-log`, `POST /v1/audit-log/export` — reads `audit_log` only; pending outbox rows are never exposed |

A pending outbox row is **not** evidence that the mutation failed. On an
adopted route it means the mutation committed — the caller normally
already got its 2xx — and the evidence row has not been written to
`audit_log` yet.
Evidence is delayed, not lost. A non-2xx final response on an adopted
route is patched into the pending row (`outcome`/`status_code`) before
dispatch; if the dispatcher wins that race the row keeps its commit-time
`success` prediction and the control plane logs the mismatch with the
`outbox_id`.

## 1. Signals

| Metric | Type | Meaning | Alert |
|---|---|---|---|
| `chv_audit_outbox_pending` | gauge | rows currently `pending` | page if `> 0` for 10 minutes |
| `chv_audit_outbox_oldest_pending_ms` | gauge | age of the oldest pending row | page if `> 600000` (10 min) |
| `chv_audit_outbox_dispatched_total` | counter | pending rows converted into `audit_log` rows | rate should track adopted mutation volume |
| `chv_audit_outbox_dispatch_failures_total` | counter | failed claim/complete attempts (retried forever) | investigate a sustained increase |

The gauges are refreshed by the dispatch pass (default every second).
`chv_bff_audit_write_failures_total` and
`chv_grpc_audit_write_failures_total` still cover the **direct** insert
fallback and are not outbox signals.

Dispatcher logs use `target: "audit"`:

```sh
journalctl -u chv-controlplane | grep -E 'audit outbox|SQLITE_BUSY'
```

A pass that dispatches rows logs `audit outbox dispatch pass completed`;
per-row failures log the `outbox_id`, `attempts`, `next_attempt_ms`, and
`last_error`. There is no per-pass log line otherwise. A healthy or empty
outbox emits no outbox log lines at any level **except** routine prune
notices when retention-aged dispatched rows are removed (and the
debug-level pending summary, which requires `pending > 0`), so silence —
or only occasional prune notices — is normal.

## 2. Triage

**Step 1 — is anything actually pending, and how old?**

```sh
sqlite3 /var/lib/chv/controlplane.db \
  "SELECT status, COUNT(*), MAX(attempts) FROM audit_outbox GROUP BY status;"

sqlite3 /var/lib/chv/controlplane.db \
  "SELECT outbox_id, attempts, created_at_ms, next_attempt_at_ms, \
          substr(last_error,1,200) \
   FROM audit_outbox WHERE status = 'pending' \
   ORDER BY next_attempt_at_ms ASC LIMIT 20;"
```

`created_at_ms` is epoch milliseconds. A pending row is delayed
evidence; do not delete it.

**Step 2 — is the dispatcher running?**

Check the service and the logs:

```sh
systemctl status chv-controlplane
journalctl -u chv-controlplane --since '15 min ago' | grep 'audit outbox'
```

No dispatch or error lines is normal when nothing is pending; the only
routine line is the occasional prune notice. A repeating
`SQLITE_BUSY` warning means another writer is holding the database; the
worker backs off 250 ms → 5 s per pass and keeps retrying.

**Step 3 — why did dispatch fail?**

`last_error` shows the head of the queue. Typical causes:

| `last_error` | What it means | Action |
|---|---|---|
| contains `database is locked` / code 5 / 517 | SQLite write contention (`SQLITE_BUSY`) | find the long-running writer; dispatch resumes on its own |
| `database or disk is full` | disk full | free space; the row stays pending and retries |
| `UNIQUE constraint failed: audit_log.audit_id` | an `audit_log` row already owns the pre-minted id | **do not delete evidence rows**; escalate to the incident lead and check for a duplicate/manual replay |
| `unparseable payload` | outbox row corrupt (should be impossible; payload is validated at enqueue) | preserve the row and escalate; it will retry forever |
| no row errors, but `pending` never drops | worker not mounted, process unhealthy, or clock skew | restart safely (step 4) |

**Step 4 — restart safely.**

```sh
sudo systemctl restart chv-controlplane
```

A restart is safe and self-healing:

- the dispatcher runs an immediate pass at startup (tokio's interval
  fires once immediately), so post-crash replay starts within a second;
- in-flight claims carry a **60 s lease**; rows claimed by the old
  process become due again when the lease expires, with no reaper and
  no manual unstick;
- dispatch is **exactly-once per outbox row** (one transaction inserts
  the `audit_log` row and marks the outbox row `dispatched`), so a
  restart cannot double-write or strand a half-completed row.

**Step 5 — confirm recovery.**

```sh
sqlite3 /var/lib/chv/controlplane.db \
  "SELECT status, COUNT(*) FROM audit_outbox GROUP BY status;"

# Every dispatched outbox row must have exactly one evidence row.
sqlite3 /var/lib/chv/controlplane.db \
  "SELECT COUNT(*) FROM audit_outbox o \
   LEFT JOIN audit_log a ON a.audit_id = o.audit_id \
   WHERE o.status = 'dispatched' AND a.audit_id IS NULL;"
# expected: 0
```

`chv_audit_outbox_pending` should fall to 0 and
`chv_audit_outbox_dispatched_total` should increase.

## 3. Gap in the trail

If an investigation needs a specific mutation's row **now**:

1. Find the pending row by timestamp/actor/resource. The actor, action,
   path, and outcome are inside the row's already-redacted `payload`;
   the row is the same content the dispatcher will insert.
2. Treat that payload as evidence for the investigation, but state that
   it is **not yet in the admin audit API** until dispatch succeeds.
3. Do not copy pending rows into `audit_log` by hand. A manual insert
   with the same pre-minted `audit_id` makes the dispatcher's retry
   fail with a UNIQUE conflict forever; use the dispatcher.
4. Record the outbox `outbox_id` and `audit_id` in the incident notes so
   the eventual `audit_log` row can be correlated.

Unadopted paths keep their old window: the audit row is inserted after
the handler returns, with no retry. A crash in that narrow window can
still yield an unrecorded mutation until adoption lands. The outbox
cannot detect or recover rows that were never enqueued.

For `EnrollNode` specifically: if the trail has no row for an enrollment
attempt at all, check whether the bootstrap token was consumed (the
`bootstrap_tokens.used_at` column) and whether a node/inventory row
exists. A consumed token with no node row means the process died after
the token commit and before the final write — there is no outbox row to
replay, and the operator must issue a fresh token.

For the node transitions specifically: a **denied** request always
leaves a direct `denied` row — a missing row is never a denial. If the
trail has no row, first check how the change was made: the WebUI/chvctl
path (`POST /v1/nodes/mutate`) is not outbox-coupled and keeps the
post-hoc crash window. On the gRPC path, check `operations` for a
still-`Pending` `PauseNodeScheduling`-style row with its
`OperationStarted` event and no matching `node_desired_state` change
(the journal-only crash window), and `chv_grpc_audit_write_failures_total`
/ the `AUDIT_LOG_WRITE_FAILED` log for a failed direct insert. If the
state change *is* present but the row missing, check `audit_outbox` for
a pending row before concluding evidence was lost.

For the volume snapshot family specifically: a **denied** request
always leaves a direct `denied` row — a missing row is never a denial.
If the trail has no row, first check how the change was made: the
WebUI/chvctl paths (`POST /v1/volumes/snapshot`,
`/v1/volumes/restore-snapshot`, `/v1/volumes/delete-snapshot`) are not
outbox-coupled and keep the post-hoc crash window. On the gRPC path,
check `operations` for a still-`Pending` `SnapshotVolume`-style row
with its `OperationStarted` event and no matching
`volume_desired_state` snapshot-op change (the journal-only crash
window), and `chv_grpc_audit_write_failures_total` / the
`AUDIT_LOG_WRITE_FAILED` log for a failed direct insert. If the intent
*is* present but the row missing, check `audit_outbox` for a pending
row before concluding evidence was lost. A direct failed 409 row means
a `Deleting` tombstone refused the patch (the volume row is unchanged).

A dispatched row is indistinguishable from a direct one in `audit_log`;
for 7 days you can join `audit_outbox.audit_id` to prove a row was
outbox-coupled.

## 4. Configuration

```toml
[audit]
outbox_dispatch_interval_secs = 1   # >= 1; staleness bound (default 1 s)
outbox_retention_days = 7           # 1..=365; dispatched rows only
```

Defaults are safe and inert for unadopted paths. The worker is always
mounted; there is no enable switch, because an empty outbox is a valid
steady state. Changing either value requires a restart.

Retention prunes up to 5,000 **dispatched** rows per pass. Pending rows
are never pruned, regardless of age.

## 5. Change-risk notes (PR 1 plumbing + PR 2 BFF + PR 3a/3b/3c/4 gRPC)

- **Audit loss / double-write (BFF):** the check-or-insert composition is
  what keeps this safe on adopted routes: a present outbox row suppresses
  the direct insert, an absent one takes the direct path, and a failed
  check after a committed enqueue skips the direct insert rather than
  risk a duplicate. The BFF integration suite covers adopted success,
  rollback, crash-before-middleware, injected post-commit 500, unadopted
  routes, and the oversized-actor fail-closed case.
- **Audit loss / double-write (gRPC enrollment, reconcile, and
  lifecycle):** the adapter mints a per-request
  `outbox_id`/`audit_id` (UUIDv4, never from the wire) and applies the
  same check-or-insert in `finish_audited`. A
  committed outbox row suppresses the direct insert; a missing row takes
  the direct path (denied, rolled back, or unadopted); a failed check
  consults the request's committed flag, set by the service once its
  final transaction committed the enqueue, and skips the insert rather
  than risk a duplicate. The adoption tests cover one dispatched row per
  adopted RPC, a service that enqueues and then fails (exactly one row,
  `finalize_pending` rewrites its outcome/status — removing the patch
  fails the test), the failed/denied direct row with no outbox row,
  crash replay, an unadopted lifecycle RPC, and the oversized-payload
  rollback. Rows are stamped at enqueue (not request start), so a
  pending row's timestamp is the commit attempt. The reconcile tests
  additionally pin a stale-generation rollback (no outbox row, no
  desired-state change, one direct failed row) and a missing
  observed-state row on `AcknowledgeDesiredStateVersion` (one direct
  404 row, no outbox row).
- **`EnrollNode` multi-commit risk (PR 3a):** the outbox row rides the
  final fail-closed write only. Token consumption is a separate, earlier
  commit; a crash between it and the final write leaves a
  partially-applied enrollment **and no outbox row**, and the consumed
  one-time token means the operator must issue a new token (or remove
  the half-enrolled node). This PR does not claim to close that window;
  folding enrollment into one transaction is follow-up work.
- **Reconcile multi-commit risk (PR 3b):** each `Apply*DesiredState`
  couples only its desired-state write. The convergence event append (and
  for `AcknowledgeDesiredStateVersion` the operation-status update) is a
  later, independent commit. A crash in between leaves the state change
  and its evidence durable but the journal incomplete — the same partial
  mutation that existed before, now with the audit row present rather
  than lost. A late journal failure returns an error and patches the
  already-committed row to `failed`/500; the evidence is not retracted.
  This is per-write coverage, not a claim that the whole RPC is atomic.
- **Lifecycle multi-commit risk (PR 3c):** the five node transitions
  couple only the intent write. Their `operations` INSERT and
  `OperationStarted` event are earlier, independent commits: a crash
  between journal and intent leaves **no state change and no audit row**
  (the pre-existing window, unchanged); a crash after the intent commit
  but before the `Accepted` status update leaves the applied state and
  its `success` row with a possibly stale operation status (`Pending` is
  neither dispatched nor reaped today). An intent failure keeps the
  existing `Failed` journal and writes exactly one direct failed row;
  the intent transaction and its enqueue roll back. Journal-anchoring
  these RPCs was rejected because it would produce a false `success` row
  for a state change a later crash prevented. Per-RPC adoption only: the
  rest of the lifecycle surface is not covered.
- **Lock scope (PR 3a):** the PR 3a adopted store methods take an
  optional outbox row and open a transaction only when one is present;
  `register_fabric_identity` already had a transaction, and the rotation
  compare-and-swap / bootstrap-result upsert gain a transaction around
  the mutation statement plus the outbox INSERT. The transaction is held
  for the store call only (never across certificate minting or transport
  work), so no new lock ordering is introduced.
- **Lock scope (PR 3b):** `upsert_vm`, `upsert_volume`, and
  `upsert_network_with_exposures` already held their desired-state
  transaction, so only the extra INSERT is new.
  `set_state_preserving_policy` and the four observed-state
  acknowledgements gain a transaction around the mutation statement plus
  the outbox INSERT on the adopted path. The transaction never spans the
  convergence event append or the operation-status update.
- **Lock scope (PR 3c):** `set_scheduling_paused` and `set_drain_intent`
  take an optional outbox row. `set_state_preserving_policy` already took
  one in PR 3b (it also serves `ApplyNodeDesiredState`); PR 3c changed
  only that adopted branch's transaction mode, from a deferred `begin()`
  to `BEGIN IMMEDIATE`, so the reconcile RPC gets the same mode. With a
  row these methods wrap the single intent statement plus the outbox
  INSERT in `begin_with("BEGIN IMMEDIATE;")`; without one they keep the
  original autocommit statement byte-for-byte. The lock is held for one
  extra INSERT and never across the operation-status update or any
  transport work. The **other** PR 3a/3b adopted store methods still open a
  deferred `begin()` (same atomicity, busier first statement under
  contention): `BEGIN IMMEDIATE` is the discipline of the BFF outbox paths
  converted for #706 (all but `vms/import`, which keeps its pre-existing
  transaction) and these intent methods, not a repo-wide rule.
- **Lock scope (PR 4):** `set_volume_snapshot` takes an optional outbox
  row. With one it wraps the single intent UPSERT plus the outbox INSERT
  in `begin_with("BEGIN IMMEDIATE;")`; without one it keeps the original
  autocommit statement. Its `'Deleting'`-tombstone check runs on the
  **same connection inside that transaction** (a connection-taking
  helper shared with the pool-level form, so the conflict semantics are
  identical). Calling the pool-level helper there would need a second
  pool connection while the first still holds the write lock; under
  concurrent adopted mutations that stalls on connection acquisition
  (the shipped configs allow 4 connections, and the in-memory test pool
  holds one and blocks outright). Running it on the held connection
  needs no extra connection and observes exactly the row state the patch
  saw.
- **Remaining surfaces stay post-hoc:** the rest of the gRPC lifecycle
  service (VM power, volume attach/resize/clone, networks, migrate,
  overlay, ARP, resolve inspect) and every unadopted BFF route still
  insert after the handler returns, with the original narrow crash
  window. Denied and failed requests stay post-hoc by design.
- **Lock scope (PR 2):** `users`/`tokens` mutations that were autocommit
  single statements are now wrapped in
  `begin_with("BEGIN IMMEDIATE;")` so the mutation and the outbox INSERT
  commit together. The write lock is held for one extra INSERT; no new
  lock ordering is introduced. `networks/update` and `networks/delete`
  already held a transaction, so only the INSERT is new there;
  `networks/create`'s transaction pre-existed but its mode was converted
  from a deferred `begin()` to `BEGIN IMMEDIATE` in PR 2, so it now takes
  the write lock at `BEGIN`; `vms/import` likewise keeps its existing
  transaction.
- **Oversized actor (PR 2, NIT 5):** adopted sites fail closed. A payload
  over the store's 16 KiB bound is not truncated; enqueue fails, the
  mutation transaction rolls back (the caller sees a 500 and no state
  change), and the middleware records the failed attempt once on the
  direct path. Only a pathologically long `actor_username` can reach the
  bound.
- **Verification scope:** the crash window is proven deterministically by
  integration tests, not by a real process kill: an adopted handler that
  commits with the middleware never running (the crash made static) is
  replayed exactly once by the dispatcher, and a commit failure after a
  successful enqueue leaves neither the mutation nor the outbox row. The
  gRPC adoption suite does the same for the enrollment and reconcile
  RPCs and also pins the post-commit-failure patch path (one row,
  rewritten outcome/status; removing `finalize_pending` fails it). The
  PR 3c lifecycle tests add per-RPC dispatch reuse, intent-failure
  journal preservation with one direct failed row, crash replay, the
  patch path, and the oversized-payload rollback, plus a store-level
  deferred-FK commit-failure test for the scheduling intent. The
  design's `SIGKILL` end-to-end test (long dispatch interval, kill,
  restart) is **not implemented yet** and is a follow-up; this runbook
  does not claim one.
- **Migration:** `0071_audit_outbox.sql` is additive (one table, two
  indexes). No backfill, no change to `audit_log`. It runs through the
  normal startup migrator; disk-full remains the only new startup
  failure class, identical to every other migration.
- **Throughput:** at most 128 rows/pass at 1 pass/s, far below SQLite
  write throughput. Dispatched rows are pruned after 7 days; outbox size
  is bounded by mutation volume × dispatch lag.
- **Interaction with #762:** the interceptor-denial writer inserts
  directly into `audit_log` and is unaffected.
