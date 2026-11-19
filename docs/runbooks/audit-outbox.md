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

> **Rollout status (#706 half 1).** The table, dispatcher, and metrics
> shipped before any request path adopted them. On a build without PR 2
> or PR 3, the outbox is empty by design and `chv_audit_outbox_pending`
> should be 0. A non-zero value on such a build means a path was adopted
> unexpectedly or something wrote the table out of band — investigate
> before assuming adoption.

## What exists where

| Piece | Location |
|---|---|
| `audit_outbox` (plumbing: pending/dispatched rows) | operational database (`[database] url`, default `sqlite:///var/lib/chv/controlplane.db`) |
| `audit_log` (the evidence table) | same database |
| Dispatcher worker | inside `chv-controlplane`, spawned at boot next to the audit retention worker |
| Configuration | `/etc/chv/controlplane.toml`, `[audit]` (`outbox_dispatch_interval_secs`, `outbox_retention_days`) |
| Admin read surface | `POST /v1/audit-log`, `POST /v1/audit-log/export` — reads `audit_log` only; pending outbox rows are never exposed |

A pending outbox row is **not** evidence that the mutation failed. It
means the mutation committed and its evidence row has not been written
to `audit_log` yet.

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

## 5. Change-risk notes (PR 1 plumbing)

- **Audit loss / double-write:** adoption (PR 2/3) is what changes
  request behavior. This PR only creates the table and drains it; it
  cannot add, remove, or duplicate a current `audit_log` row.
- **Migration:** `0071_audit_outbox.sql` is additive (one table, two
  indexes). No backfill, no change to `audit_log`. It runs through the
  normal startup migrator; disk-full remains the only new startup
  failure class, identical to every other migration.
- **Lock scope:** enqueue (once adopted) is one INSERT inside a
  transaction the mutation already holds — no new lock ordering. The
  dispatcher's `complete` transaction is one INSERT plus one indexed
  UPDATE, and busy errors back off.
- **Throughput:** at most 128 rows/pass at 1 pass/s, far below SQLite
  write throughput. Dispatched rows are pruned after 7 days; outbox size
  is bounded by mutation volume × dispatch lag.
- **Interaction with #762:** the interceptor-denial writer inserts
  directly into `audit_log` and is unaffected.
