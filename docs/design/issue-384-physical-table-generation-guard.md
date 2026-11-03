# #384 design writeup — physical-table generation guard (`volumes`/`networks` last-writer-wins)

**Issue:** kubedoio/chv#384 — `UPSERT_VOLUME_SQL` and `UPSERT_NETWORK_SQL`
(`crates/chv-controlplane-store/src/desired_state.rs`) do an unconditional
`ON CONFLICT ... DO UPDATE`: the desired-state upserts guard with
`WHERE <table>.desired_generation <= EXCLUDED.desired_generation`, but the
physical tables have no generation column, so physical rows are
last-writer-wins. The issue asks for a guard (or a deliberate
last-writer-wins decision) plus a fix for the clone path's
check-then-upsert TOCTOU.

**Status:** FINAL — investigation + maintainer decision (§6.1, 2026-10-05).
The adopted fix (option 3, the targeted transactional clone path) is
implemented; the general physical-row guard is deferred by decision with
named reopen triggers (§6.1). Evidence cited `file:line` at main
`b4fcb9e2`; refs re-anchored at `728fcd5f` (the #501 merge) after that
merge's own insertions shifted the store/service line numbers, with the
clone-path rows updated to the as-built shape.

---

## 1. Problem statement

The control plane's store layer couples every intent-bearing write to a
generation-guarded desired-state row, but the *physical* inventory rows
(`volumes`, `networks`) are written unconditionally. The issue's own
accuracy correction holds: `upsert_volume` runs the physical upsert and the
generation-guarded `volume_desired_state` upsert **in one transaction**
(`desired_state.rs:618-667`), failing closed atomically on a stale
generation — so the agent fragment-reconcile path cannot silently regress a
physical row today. What remains is:

- **(a)** racing writers with *fresh* generations overwrite
  `node_id`/`capacity_bytes`/`volume_kind`/`storage_class` unconditionally
  (e.g. two ops materializing the same volume id through the clone TOCTOU);
- **(b)** direct physical-table writers bypass any guard (the resize
  executor's `UPDATE volumes SET capacity_bytes`,
  `orchestrator.rs:1209` — consistent today, but nothing enforces the
  ordering);
- **(c)** `clone_volume` READS `capacity_bytes` to shape the target and
  WRITES the target via the same upsert (amplifies #381: a resize landing
  between the read and the write gives the target a stale size);
- **(d)** clone's target-absence check → upsert is a TOCTOU.

The core question for the maintainer: is the right fix a *general*
physical-row guard (generation column, or a subquery guard on the existing
desired-state row), a *targeted* transactional clone path, or both as
separate PRs — or none of the above, on the evidence below?

## 2. Ground truth (verified at `b4fcb9e2`; refs re-anchored at `728fcd5f`)

### 2.1 The unguarded upserts, verified

- `UPSERT_VOLUME_SQL` (`desired_state.rs:76-110`): `ON CONFLICT (volume_id)
  DO UPDATE` at `:97-109` with **no WHERE guard** (contrast the VDS upsert's
  guard at `:191`). Only guarded column: `owner_id` is COALESCE-preserved
  (`:108`, the #381 review fix).
- `UPSERT_NETWORK_SQL` (`desired_state.rs:194-214`): `ON CONFLICT
  (network_id) DO UPDATE` at `:209-213`, no guard (contrast NDS guard at
  `:257`).
- Neither physical table has a generation column
  (`cmd/chv-controlplane/migrations/0001_initial.sql:122-131` volumes,
  `:166-173` networks).

### 2.2 Single-transaction fail-closed coupling, verified

`upsert_volume` (`desired_state.rs:618-667`), `upsert_network`
(`:941-984`), and `upsert_network_with_exposures` (`:986-1057`) all follow
the same shape: `begin()` → physical upsert → desired-state upsert →
`rows_affected() == 0` ⇒ return `StoreError::StaleGeneration` **before
`tx.commit()`** ⇒ the physical write is rolled back with the intent. A
stale-generation writer can never commit a physical-row change through
these methods.

### 2.3 Line-reference corrections (issue → current main)

| Issue said | Main at investigation (`b4fcb9e2`) |
|---|---|
| clone target-absence check `lifecycle.rs ~:1063` | `lifecycle.rs:1156-1161` (target `get_volume_summary` → `InvalidArgument("target volume id already exists")`) |
| clone upsert `lifecycle.rs ~:1096` | pre-fix: `upsert_volume` inside `persist_intent_and_accept`; as built (#501): `materialize_clone_target` at `lifecycle.rs:1226-1244` → `desired_state.rs:824-939` |
| resize executor `orchestrator.rs ~:925` | `orchestrator.rs:1209` (`UPDATE volumes SET capacity_bytes = ? WHERE volume_id = ?`), plus the unguarded VDS clear `UPDATE volume_desired_state SET resize_to_bytes = NULL` at `:1223-1227` |

The drift is from #495 (accept-time rejection inserted ~75 lines of
`ensure_volume_snapshot_family_supported`/`ensure_node_not_core_managed`
into `lifecycle.rs:340-396`) and #498 (retry-machinery changes above the
dispatch handler in `orchestrator.rs`).

### 2.4 Writer census — `volumes` (production code only)

| # | Writer | file:line | Shape | Generation-aware? |
|---|---|---|---|---|
| V1 | `UPSERT_VOLUME_SQL` via `DesiredStateRepository::upsert_volume` | SQL `desired_state.rs:76-110`; method `:618-667` | unconditional DO UPDATE, **in one tx with the guarded VDS upsert**; StaleGeneration rolls back both | yes (tx coupling) |
| V1a | ← clone target materialization | as built (#501): `lifecycle.rs:1226-1244` → `desired_state.rs:824-939` | generation = caller-supplied `meta.desired_state_version`; the BFF mints a fresh ms-clock generation per request (`bff_mutations.rs:50-65` `fresh_generation`, `:67-76` `build_meta`) | caller-supplied, always fresh from the BFF |
| V1b | ← agent volume fragment (`ApplyVolumeDesiredState`) | `reconcile.rs:470-500` | generation = the fragment's, which the node client sets from the dispatched intent's `desired_state_version` (`agent_server.rs:4967-4971`; the VM-side twin enforces match at `:225-232`) | yes |
| V2 | resize success persist | `orchestrator.rs:1209` | direct single-statement `UPDATE ... SET capacity_bytes`, autocommit, after successful agent dispatch | **no** — ordering by construction only |
| V3 | BFF VM-create embedded volume | `chv-webui-bff/src/handlers/vms.rs:603-616` (id minted `:511`) | plain `INSERT` — PK collision aborts the whole create tx (fail-closed) | n/a (no conflict path) |
| V4 | BFF VM import | `chv-webui-bff/src/handlers/imports.rs:219-231` (id minted `:160`) | plain `INSERT`, fail-closed | n/a |
| V5 | BFF template instantiate | `chv-webui-bff/src/handlers/templates.rs:420-433` (id minted `:383`) | plain `INSERT`, fail-closed | n/a |
| — | **delete** | none | there is **no `DELETE FROM volumes` anywhere in production code** — volume rows are never removed | — |

Test-only writers excluded from the census (`orchestrator.rs:1995-2006`
`seed_volume`, `:2008-2014` `seed_network`, `fabric_planner.rs:267-284`,
store `networks.rs:86-100`, and the various test-suite seeds). No
migration writes to either table (verified: no `INSERT`/`UPDATE` on
`volumes`/`networks` under `cmd/chv-controlplane/migrations/`).

### 2.5 Writer census — `networks` (production code only)

| # | Writer | file:line | Shape | Generation-aware? |
|---|---|---|---|---|
| N1 | `UPSERT_NETWORK_SQL` via `upsert_network` / `upsert_network_with_exposures` | SQL `desired_state.rs:194-214`; methods `:941-984`, `:986-1057` | unconditional DO UPDATE, one tx with the guarded NDS upsert | yes (tx coupling) |
| N1a | ← agent network fragment | `reconcile.rs:646-671` | **the only production caller** of the physical networks upsert | yes |
| N2 | BFF network create | `chv-webui-bff/src/handlers/networks.rs:301-312` (id minted `:293` via `gen_resource_id()` since #664; `gen_short_id()` before that) | plain `INSERT`, fail-closed | n/a |
| N3 | BFF network delete | `networks.rs:483-487`, inside a `BEGIN IMMEDIATE` tx (`:384-388`, the #356 serialization) gated on zero live attachments | `DELETE FROM networks` (NDS cascades, `0001_initial.sql:176`) | no (tx + gate) |
| N4 | BFF network rename | `networks.rs:578` | `UPDATE networks SET display_name` in a plain tx | no |
| N5 | BFF VM-create implicit network | `vms.rs:718-729` (after check-then-resolve `:642-670`) | plain `INSERT` — check-then-insert TOCTOU but fail-closed on PK | n/a |
| N6 | BFF template network ensure | `templates.rs:458-469` (check at `:450-455`) | plain `INSERT`, fail-closed | n/a |
| N7 | VTEP VNI set / release | `chv-controlplane-store/src/vtep.rs:404-408`, `:427-430` | `UPDATE networks SET vni` — scoped to the `vni` column only | no |

[Corrected 2026-10-07, #544:] census row N3's shape is historical —
the network delete no longer runs `DELETE FROM networks`: it retains
the row and writes a terminal `'Deleting'` NDS tombstone (generation
bumped, deleter stamped), which the fragment upserts refuse at the
SQL level on both conflict arms (`StoreError::Conflict`), closing the
§3.6 late-fragment resurrection window.

The networks census differs materially from volumes (see §3.6): there is no
network clone, no resize executor, and the fragment upsert is the *only*
conflict-capable writer.

### 2.6 What actually protects us today (quantified)

- **BFF-minted ids** (V3/V4/V5, N2): *at the time of this design*,
  `gen_short_id()` was 8 hex chars = 32 bits (`chv-common/src/lib.rs`).
  Resource ids are now 128-bit (`gen_resource_id()`, #664); the historical
  analysis below is retained because the clone/operation-id conclusions do
  not depend on the id width. Birthday-collision territory around
  ~65–77 k rows — but these writers are plain `INSERT`s, so a collision is
  a PK violation that aborts the creating transaction: **fail-closed, not
  corrupting**. The failure mode is an ugly 500, not divergence.
- **Clone target ids** (V1a): **caller-supplied**, not minted — the BFF
  forwards the request body's `target_volume_id`
  (`chv-webui-bff/src/handlers/volumes.rs:406-410`), and chvctl documents
  it as a required operator-chosen field
  (`cmd/chvctl/src/commands/volume.rs:23`). No randomness protects these.
- **Operation idempotency** does NOT dedupe double-submits: the derived key
  includes the generation (`lifecycle.rs:438-448`,
  `CloneVolume:{node}:{target}:{gen}:source={source}`), and the BFF sends
  `operation_id: ""` (`bff_mutations.rs:70`) with a *fresh* ms-clock
  generation per request (`:50-65`) — two concurrent identical clones get
  **different** idempotency keys and both journal operations
  (`operations.rs:37` `ON CONFLICT (idempotency_key) DO NOTHING`,
  key UNIQUE at `0001_initial.sql:204`). Only a direct gRPC caller that
  repeats `meta.operation_id` (`request:{id}`, `lifecycle.rs:450`) gets
  replay semantics.
- **The desired-state guard** (`<=`) fails closed for stale writers via the
  tx coupling (§2.2). Equal generations both pass — the BFF's monotonic CAS
  generator makes that unreachable from the BFF, but two direct gRPC
  callers can supply equal generations.
- **All CP writers share one process and one SQLite pool** (BFF router
  hosted inside `chv-controlplane`, `cmd/chv-controlplane/src/bootstrap.rs:271-312`),
  so "races" are statement/transaction-boundary interleavings under WAL,
  not parallel writes — the TOCTOU windows are real but bounded by tx
  boundaries.

## 3. The real races

### 3.1 clone ∥ clone, same target id — REACHABLE, the one real bug

Both requests pass the target-absence check (`lifecycle.rs:1156-1161`)
before either commits; both journal `CloneVolume` ops (distinct
idempotency keys, §2.6). The upserts then serialize:

- If the **lower**-generation tx commits last, its VDS guard fails
  (`StaleGeneration`) → its op is marked Failed by
  `persist_intent_and_accept` (`lifecycle.rs:569-587` → `fail_operation`)
  — a confusing error ("stale generation", not "target exists"), but
  coherent: one op, one row, values match the winner.
- If the **higher**-generation tx commits last (the common case — the
  BFF's ms-clock generator means the later request carries the higher
  generation), the earlier op **already committed and was Accepted**: both
  ops are live. The `volumes`/VDS rows end up shaped like the winner's
  source, but **both `CloneVolume` ops dispatch to the agent** and clone
  into the same target volume id. Same source ⇒ duplicate agent-side clone
  work; different sources ⇒ the DB shape and the agent's final data depend
  on dispatch order — divergence.

The volumes row itself is always *some* op's coherent shape; the harm is
two live conflicting operations on one target id, plus a misleading
failure mode for the loser. This is exactly what a transactional
check-and-insert (option 3) eliminates: the second request never journals
an op at all.

### 3.2 clone ∥ fragment, same target id — effectively closed

A fragment for the target id exists only after a *prior* clone's intent
was dispatched and the agent applied + reported it (fragment generation =
the dispatched intent's generation, §2.4 V1b). Scenarios:

- Fragment lands, then a second clone arrives: the target-absence check
  sees the row → `InvalidArgument`. Closed.
- Second clone commits first, then the first clone's delayed fragment
  arrives: its generation < the second clone's fresh generation → VDS
  guard rejects → whole tx (physical write included) rolled back. Closed.
- Fragment in flight while the second clone is between check and commit:
  the fragment's upsert and the clone's upsert serialize; whichever
  commits second wins coherently per §3.1's rules. Same exposure class as
  §3.1, one step removed.

### 3.3 resize executor ∥ anything — consistent by construction, unguarded by mechanism

- resize ∥ clone-with-me-as-target: the target-absence check rejects a
  clone onto an existing (resized) volume id. Closed.
- resize ∥ fragment on the same volume: the fragment reports the
  post-resize spec at the resize intent's generation — same values as the
  executor's `UPDATE`. The interleaving that would corrupt (fragment with
  the current generation but pre-resize capacity) requires the agent to
  report an unapplied spec, which is not how fragments are built
  (generation must match the dispatched intent it applied). Benign by
  agent discipline, **not** by any DB mechanism — the honest statement of
  issue premise (b).
- resize ∥ clone where the resized volume is the clone **source**: the
  clone read `capacity_bytes` at accept time (`lifecycle.rs:1130-1139`);
  a resize committing between that read and the target materialization
  gives the target the stale size — issue premise (c), a read-write skew,
  silently wrong data. REACHABLE, currently unguarded.

### 3.4 import / template / VM-create ∥ anything — fail-closed

Plain `INSERT`s; a same-id collision is a PK violation that aborts the
creating transaction (500 to the operator). The implicit-network
check-then-insert pairs (`vms.rs:642-729`, `templates.rs:449-469`) have the
same shape: TOCTOU present, outcome fail-closed. Safe but rough UX; not
this issue.

### 3.5 #484's RecreateVm path — not a writer

`RecreateVm` journals only an `operations` row
(`orchestrator.rs:613-680`) and dispatches like the create family
(`:771-776`); it writes neither `volumes` nor `networks`. No exposure.

### 3.6 Networks-specific finding (not in the issue's premises)

Because `networks` rows are deleted outright (N3) while fragments can
arrive late (the agent's deferred-report queue survives CP outages), a
network fragment in flight when the operator deletes the network will
**resurrect** it: the delete cascades away the NDS row, so the fragment's
`upsert_network_with_exposures` takes the INSERT path — no conflict, no
guard evaluated (`desired_state.rs:986-1057`, delete at
`networks.rs:483-487`). Window is small (fragment queued pre-delete,
delivered post-delete) and the resurrection is shape-complete (network +
NDS + exposures re-created). Flagged as a **separate issue** — it is not a
last-writer-wins bug and none of the three options below fixes it (there
is no row left to guard).

## 4. Options

### Option 1 — generation column on `volumes`/`networks`

Add `desired_generation` (or `row_generation`) to both physical tables
(migration `0058`, precedent `0027_owner_id.sql`, `0057`), guard the DO
UPDATE with `WHERE volumes.desired_generation <= EXCLUDED.desired_generation`,
backfill from the corresponding desired-state table, update every census
writer.

- **What it fixes that is reachable today:** nothing the tx coupling does
  not already fix. V1/V1a/V1b are already fail-closed; V3/V4/V5 never
  conflict (plain INSERT); the only unguarded writer (V2, resize) would
  *become* guardable — and that is its real merit: the executor has the
  intent generation available (the claim query returns
  `desired_generation`, `orchestrator.rs:190`), so the resize persist
  could become a compare-and-swap.
- **Costs:** migration + backfill; every writer must maintain a second
  generation copy (8 writer sites across 3 crates); BFF plain-INSERTs need
  the column or a DEFAULT; and the semantic question is genuinely open —
  for a physical row, "generation" can only mean "the generation of the
  last intent that shaped it", i.e. a **mirror of the desired-state row's
  generation one join away**. Two copies of the same fact invites drift,
  and drift in the mirror is exactly the kind of bug this issue exists to
  prevent.
- **Verdict:** heavy, fixes no reachable bug, adds a drift surface. Only
  justified if the resize executor (or a future writer) genuinely needs
  CAS semantics on the physical row.

### Option 2 — subquery guard on the DO UPDATE

Guard the physical upsert with the current desired-state generation:

```sql
ON CONFLICT (volume_id) DO UPDATE SET ...
WHERE COALESCE(
    (SELECT desired_generation FROM volume_desired_state
     WHERE volume_id = EXCLUDED.volume_id), 0
) <= $9   -- the incoming generation, bound alongside
```

- **Correctness:** the physical upsert runs *first* in the tx
  (`desired_state.rs:622-632`), so the subquery sees the pre-tx desired
  state — exactly the guard intended. No-VDS-row case (fresh volume, or
  orphan physical row) maps to 0 ⇒ update permitted, which is the current
  behavior for those shapes.
- **What it fixes:** **nothing reachable.** The very next statement in the
  same transaction applies the same comparison to the same row and, on
  failure, rolls the physical write back (§2.2). Option 2 is pure
  defense-in-depth against a future refactor that splits the tx or adds a
  standalone physical upsert.
- **Costs:** one PK-lookup subquery per upsert (negligible in SQLite); a
  little more SQL to reason about; a subtle ordering dependency (guard
  must stay *before* the desired-state statement in the tx) that nothing
  documents or enforces.
- **Verdict:** cheap and harmless, but it should be sold honestly as
  insurance, not as a fix. If the maintainer wants belt-and-braces, this
  is the one to take — with a comment in the SQL pinning the
  statement-order dependency.

### Option 3 — make the clone path transactional (targeted)

Close the TOCTOU and the read-write skew in one place:

1. **Strict insert for the clone target.** Clone does not need DO UPDATE
   semantics at all — the target must not exist. A dedicated store method
   (`insert_volume_for_clone` or a flag on `upsert_volume`) using
   `INSERT ... ON CONFLICT (volume_id) DO NOTHING` + `rows_affected == 0`
   ⇒ `StoreError::Conflict { entity: "volume", .. }` (the variant already
   exists, `db.rs:43-48`), mapped by the lifecycle to
   `ControlPlaneServiceError::Conflict`/`InvalidArgument` with today's
   "target volume id already exists" message. The race loser gets the
   same clean 400/409 the pre-check produces — before journaling an op.
2. **Move the reads inside the write tx.** Read the source summary *and*
   re-check target absence inside the same `BEGIN IMMEDIATE` transaction
   that inserts (repo precedent for the pattern: BFF VM-create
   `vms.rs:488-494`, template quota `templates.rs:363-367`, network delete
   `networks.rs:384-388`; the deferred-vs-immediate hazard is documented
   in the quota race test, `vms.rs:1740-1746`). This also closes premise
   (c): the source `capacity_bytes` read serializes against the resize
   executor's UPDATE (`orchestrator.rs:1209`), so the target can no longer
   be shaped from a stale size.
3. **Handle idempotent replay.** A direct gRPC caller repeating
   `meta.operation_id` gets the *existing* op from `create_or_get`
   (`operations.rs:79-135`, `lifecycle.rs:465-500`) and the lifecycle
   proceeds to re-run the intent persist — with a strict insert, the
   replay would now collide with its own earlier materialization. The
   store method needs a way to distinguish "row exists because a *racing
   different* request created it" from "row exists because *this op
   already materialized it*". Cheapest: have `create_or_get` return a
   `created: bool` on the receipt (or compare the existing
   `volumes` row against the intended shape and treat an exact match as
   idempotent success). Note the pre-existing adjacent quirk: replaying a
   terminal op currently re-runs `accept_operation` and can flip a Failed
   op back to Accepted — same no-status-guard shape as `mark_for_retry`
   (flagged in #378 §7); out of scope here but worth a comment.

- **What it fixes:** §3.1 (the reachable race) and premise (c) outright;
  reduces §3.2's window to zero for the check side. Does not touch V2
  (premise (b)) or the general last-writer-wins property — deliberately.
- **Costs:** one new store method (+ its error mapping), the
  receipt-`created` plumbing, and clone's read/write restructure. No
  migration, no contract change (BFF `map_ack` already maps
  `Conflict → BffError::Conflict`, `bff_mutations.rs:85`; the pre-check's
  `InvalidArgument → 400` arm is unchanged, `:83`).

### Decomposition (the framing the issue invites)

§3 shows the reachable bug is **only** the clone TOCTOU (+ its source-read
skew). Premise (b) is unguarded-but-consistent-by-construction; premise
(a) outside the clone path collapses into the tx coupling's fail-closed
behavior. So the natural decomposition is:

- **PR A (option 3):** small, targeted, fixes everything reachable. Ship
  first.
- **PR B (option 1 or 2, or nothing):** the general physical-row guard —
  a separate decision with a different risk profile (migration + writer
  sweep vs. a SQL-only assertion), justified only by defense-in-depth or
  future CAS needs, not by any open bug.

## 5. Decision points (maintainer) and recommendations

1. **Decomposition** — bundle vs split. **Recommend split**: PR A (clone
   tx + strict insert) now; the general guard as its own decision. The
   evidence (§3) shows the reachable exposure is clone-shaped; a
   migration-bearing guard riding along would inflate the review surface
   of a fix that needs none.
2. **General physical-row guard** — option 1 (column) / option 2
   (subquery) / status quo. **Recommend status quo now, with named reopen
   triggers**: (i) a second direct physical writer appears (the resize
   executor today is the only one); (ii) the #380/#381 provenance work
   unifies desired/physical state (that is the natural moment to define
   what a physical row's generation means); (iii) a desired-state row is
   ever allowed to exist without its physical twin (breaks option 2's
   COALESCE fallback). If the maintainer wants a guard regardless, take
   **option 2** and document it as defense-in-depth — option 1's mirrored
   generation is a drift surface with no reachable bug behind it.
3. **Race-loser error contract** — reuse today's pre-check
   `InvalidArgument` ("target volume id already exists") for the
   race-detected path, vs `Conflict` (gRPC `ALREADY_EXISTS`,
   `error.rs:89`; BFF `BffError::Conflict`, `bff_mutations.rs:85`). Both
   are zero-plumbing. **Recommend `Conflict` for the race-detected path,
   `InvalidArgument` kept for the pre-check**: the race loser's request
   was well-formed and the target genuinely existed at persist time — a
   409 is the honest code, and the split makes the two paths
   distinguishable in logs. If one message/one code is preferred, unify
   on `InvalidArgument` to match the pre-check's existing contract.
4. **Clone source-read freshness (premise (c))** — fold into PR A (read
   the source inside the write tx) vs leave. **Recommend fold in**: it is
   the same transaction, zero extra statements, and it is the only fix
   for a silently-wrong-data path (§3.3 third bullet).

### 6.1 Decision (maintainer, 2026-10-05)

All four recommendations adopted, as written:

1. **Decomposition: split** — this change is PR A only (option 3, the
   targeted transactional clone path; implemented as #501). The general
   physical-row guard is a separate decision, not bundled here.
2. **General physical-row guard: status quo, deferred** — no guard code
   ships with this decision. The single-transaction fail-closed coupling
   (§2.2) already closes every reachable path (§3); a guard would be
   insurance against future writers, not a fix for a present bug.
   Named reopen triggers, any one of which reopens the guard decision:
   (i) a **new direct physical-table writer** appears (the resize
   executor's `UPDATE volumes SET capacity_bytes` is the only one
   today); (ii) a writer **decouples from the VDS transaction** that
   makes every current conflict-capable writer fail closed; (iii)
   **corruption of a physical row is demonstrated** through that
   coupling. If a guard is ever wanted regardless, option 2 (subquery)
   is the shape — option 1's mirrored generation is a drift surface
   with no reachable bug behind it (§4).
3. **Race-loser error: `Conflict`** — the strict-insert race loser gets
   the Conflict class (gRPC `ALREADY_EXISTS`; HTTP 409 through the
   BFF's `map_ack`, which already maps `Conflict → BffError::Conflict`);
   the accept-time pre-check keeps `InvalidArgument` (HTTP 400) with
   today's "target volume id already exists" message. The split keeps
   the two paths distinguishable in logs and gives the race loser the
   honest code — its request was well-formed and the target genuinely
   existed at persist time.
4. **Source-read freshness: folded in** — the source row is read inside
   the same `BEGIN IMMEDIATE` transaction that inserts the target
   (§4 option 3 step 2), closing premise (c) with zero extra
   statements.

Idempotent replay (§4 option 3 step 3) is a correctness requirement of
choice 3, not an optional nicety: the operation receipt's `created` flag
(the `create_or_get` insert's `rows_affected`) distinguishes a replayed
`meta.operation_id` from a fresh journal entry, and the store's strict
insert treats "row exists AND this is a replay whose existing row
matches this operation's shape" as idempotent success — everything else
that collides fails closed with Conflict.

## 6. Test strategy

House patterns exist for both levels this needs:

- **Store-level serialized guard tests:** the desired-state guard is
  already pinned this way (`test_upsert_vm_rejects_stale_generation`,
  `test_upsert_vm_accepts_newer_generation`,
  `test_upsert_vm_accepts_same_generation_idempotent`,
  `chv-controlplane-store/src/tests.rs:344-458`). Mirror for the new
  strict insert: pre-existing `volumes` row → `StoreError::Conflict`;
  absent row → insert succeeds and the VDS row lands in the same tx.
- **SQLite write-race tests:** two precedents — the BFF quota race suite
  (`vms.rs:1738+`, N concurrent tasks against a shared on-disk WAL pool
  with prod pragmas, asserting exactly-N-success) and the store's
  concurrent fabric-IP registration test
  (`tests.rs:943+`, `tokio::spawn` against a shared `TestDb` pool). The
  clone-collision test belongs in the CP lifecycle suite with a real
  `TestDb` (the #380/#381 clone suite shape,
  `chv-controlplane-service/src/tests.rs:3746+`): two concurrent
  `clone_volume` calls, same target, different sources → exactly one
  `ok_ack`, one `Conflict`/`InvalidArgument`, **one** `operations` row
  (the no-residue assertion, per the #378 test discipline), and the
  `volumes` row matches the winner's source shape.
- **Idempotent replay pin:** same `meta.operation_id` twice → both acks
  name the same operation id, no `Conflict`, no second volumes/VDS write
  (guards the PR-A regression risk in §4 option 3 step 3).
- **Source-read freshness pin (if folded in):** seed source volume,
  run a resize-success persist (`UPDATE volumes SET capacity_bytes`
  shaped), then clone → target carries the post-resize size; with the
  read inside the write tx, a resize committing between is serialized
  out — assert the target never carries a size older than the source's
  committed row.
- **Red/green discipline** (the quota-race suite documents it,
  `vms.rs:1740-1746`): neutralizing the strict insert (back to DO
  UPDATE) must fail the collision test; removing `BEGIN IMMEDIATE` must
  make the concurrency test flaky-fail (deterministic serialized twin
  still passes).
- **If option 2 is taken:** a store test that a stale-generation
  `upsert_volume` leaves the physical row untouched *even if* the VDS
  statement were hypothetically reordered — in practice: pin the
  statement-order dependency with a comment and a test asserting the
  physical row is unchanged on StaleGeneration (already implied by the
  tx test, made explicit).

## 7. Rollout & rollback

**PR A (option 3):** code-only, no migration, no proto/BFF contract change
(if `InvalidArgument` is reused) or a new-but-already-mapped error arm
(if `Conflict`). Rollback: revert the commit; no journaled-state hazard
(the strict insert rejects *before* journaling, same as the pre-check).
No ordering constraint with #495's accept-time rejections or #498's
retry-machinery changes — the code regions are adjacent in
`lifecycle.rs` but semantically independent.

**PR B (option 2, if taken):** SQL-only, single statement, no migration.
Rollback: revert. Behavior change is nil for every current caller (the
guard is subsumed by the tx coupling); the risk is a subtle regression if
a caller ever supplies a generation *below* the stored VDS generation and
previously relied on the physical write landing before the VDS failure —
no such caller exists (both V1a and V1b carry fresh-or-matching
generations).

**Option 1 (if ever taken):** migration `0058` (additive, backfill from
desired-state tables — a data-writing migration, which per
`CONTRIBUTING.md`'s high-risk rule needs explicit PR disclosure), then
writer updates, then the guard flip. Three-step rollout, reversible only
by leaving the column in place (write-only for old code) while reverting
the guard.

## 8. Residual risks

- **Premise (b) stays open by choice:** the resize executor's direct
  `UPDATE volumes SET capacity_bytes` (`orchestrator.rs:1209`) and the
  VDS `resize_to_bytes` clear (`:1223-1227`) remain unguarded; their
  consistency rests on dispatch ordering and agent fragment discipline,
  not on a mechanism. The reopen triggers in §5.2 are the mitigation.
- **Equal-generation direct-gRPC writers:** two callers supplying the
  same `desired_state_version` both pass the `<=` guard; last committer
  wins the physical row. Unreachable from the BFF (monotonic
  `fresh_generation`); accepted.
- **Network resurrection (§3.6):** a late network fragment can re-create
  a deleted network. Not fixed by any option here; recommend filing as
  its own issue (the fix shape is a tombstone or a delete-generation
  watermark, not a DO UPDATE guard). [Corrected 2026-10-07, #544:] the
  predicted tombstone fix landed (#544, fixing #499) — the delete
  retains the row and the fragment upserts refuse the terminal
  `'Deleting'` NDS tombstone at the SQL level.
- **Clone residue:** a clone whose dispatch later fails still leaves the
  materialized target row (volumes rows are never deleted, §2.4) — PR A
  narrows but does not remove this (it prevents *racing* duplicates, not
  failed-op residue); the #378 doc already flags the phantom-target trap
  class. Replaying such a Conflict-failed operation with the same
  caller-supplied `meta.operation_id` re-runs `accept_operation` — the
  #378 §7 terminal-replay quirk (the `Failed` row is resurrected to
  `Accepted` and re-dispatched against the now-existing target) — newly
  reachable for this residue, unchanged here.
- **Idempotent replay semantics:** if PR A skips the receipt-`created`
  plumbing, replayed clones with a caller-supplied `meta.operation_id`
  regress from idempotent-ack to Conflict. §4 option 3 step 3 is a
  correctness requirement, not an optional nicety.
- **32-bit `gen_short_id` collisions** (historical — resource ids are
  128-bit since #664) surfaced as 500s on the BFF create/import/template
  paths — fail-closed, but a UX wart worth its own ticket someday.

## 9. Non-goals / scope boundaries

- The desired-state guards (`vm_desired_state`/`volume_desired_state`/
  `network_desired_state`) are correct and stay as they are.
- #380/#381's broader provenance work (who wrote a physical row and why)
  — a generation column decided here would pre-empt design space that
  work owns; one more reason to defer option 1.
- The networks table has a **different writer census** (§2.5): no clone,
  no resize executor, one conflict-capable writer (the fragment upsert,
  already tx-coupled). Its only live finding is the resurrection hazard
  (§3.6), which is a delete-lifecycle bug, not a last-writer-wins bug —
  treated separately.
- `volume_observed_state`/`network_observed_state` writes
  (`observed_state.rs:82-117`, `:144-172`) are different tables with
  their own (observed-generation) semantics — not in scope.
- The BFF create/import/template plain-INSERT TOCTOUs (§3.4) — fail-closed
  today; UX polish at most.
