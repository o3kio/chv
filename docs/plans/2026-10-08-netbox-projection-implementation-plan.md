# NetBox Projection — Implementation Plan (issue #239)

Date: 2026-10-08
Status: Executed — all slices landed (implementation 2026-10-09; status
reconciled 2026-11-19). The design
(`docs/design/issue-239-netbox-projection.md`), ADR-023, the component spec,
and the two contracts were adopted and shipped.

References:

- Issue: kubedoio/chv#239
- Design: `docs/design/issue-239-netbox-projection.md` (DP1–DP12)
- ADR: `docs/specs/adr/023-netbox-projection.md`
- Component spec: `docs/specs/component/architecture-designer-netbox-projection.md`
- Contracts: `docs/specs/architecture-designer/contracts/netbox-mapping-contract.md`,
  `docs/specs/architecture-designer/contracts/netbox-api-contract.md`

## Implementation-surface analysis (where the code goes)

| Surface | Convention to follow | Concrete touchpoints |
|---|---|---|
| New crate | workspace member under `crates/` | `crates/chv-netbox-adapter/` — pure core (mapping/ownership/plan) + client + runner |
| Migrations | numbered SQL in `cmd/chv-controlplane/migrations/` (main's `0058_drop_storage_pools.sql` (#568) landed first; this campaign landed second and takes `0059`) | `0059_netbox_projection.sql` (config + runs tables) |
| Store repos | `chv-controlplane-store/src/architectures/<entity>.rs` (model: `drift.rs`) | `netbox_config.rs`, `netbox_run.rs`, re-export in `architectures/mod.rs` + crate root |
| Token encryption | `credential_crypto.rs` (`CredentialEncryption::encrypt/decrypt`) | reuse as-is; no new crypto |
| BFF wiring | `AppState` fields as `Arc<Repo>` (`router.rs:22-56`), POST-only routes | `handlers/netbox.rs` + route block after the architecture routes |
| Worker | `chv-controlplane-service/src/backup_worker.rs` pattern | `netbox_projection_worker.rs`, spawned in `cmd/chv-controlplane/src/bootstrap.rs` |
| Post-apply trigger | apply-run terminal transition site (see PR 6: realized as a worker-side sweep — that site does not exist yet) | best-effort enqueue hook (isolated; swallows errors) |
| Events | `EventRepository.append(EventAppendInput)` | five `architecture_netbox_*` event kinds |
| UI | `ui/src/lib/bff/architectures.ts` + stores + `routes/architectures/[id]/` | NetBox panel: config form, dry-run table, run history |

Validation ladder per PR: `cargo check -p <pkg>` → `cargo test -p <pkg>` →
`cargo clippy -p <pkg> --all-targets -- -D warnings`; widen to the workspace +
BFF suites when the router or store surface changes; UI PRs run
`cd ui && npm run build` (+ targeted vitest).

---

## PR 1 — Adopt the documentation set (no code)

**Scope:** design doc + ADR-023 + component spec + two contracts + this plan;
ADR index row in `docs/decisions/README.md`; designer README document-map
update.

**Proves:** the DP table is agreed before any code lands.

---

## PR 2 — `chv-netbox-adapter` pure core (no I/O)

**Scope:**

- New crate `crates/chv-netbox-adapter` with:
  - `ownership.rs` — external-id derivation (`arch:<id>:<kind>/<name>:<version>`),
    custom-field name set (prefix-configurable), `ManagedMarker` parsing.
  - `mapping.rs` — `MAPPING_VERSION`; pure builders from
    `CHVArchitecture` (+ `InventorySnapshot` enrichment) to NetBox object
    models (device, virtual machine, interface, prefix, vlan, ip address);
    the v1 exclusion list enforced (identity/config kinds never mapped);
    secret-field exclusion enforced.
  - `plan.rs` — `NetboxProjectionPlan` / `NetboxProjectionPlanEntry`;
    deterministic ordering (kind rank, then name); `compute_plan` against an
    in-memory NetBox state view.
- Depends only on `chv-architecture-validate` (model + fleet types),
  `serde`, `thiserror`. **No reqwest, no tokio, no
  sqlx.** (The full crate grows `chv-errors`/`tracing` deps only when the
  client/runner land in PR 4; PR 2's dependency set is the pure-core subset.)

**Tests (in-crate):**

- idempotency: same input twice → all `no_op`
- version bump → `update` entries with field diffs
- name collision with foreign object → `conflict`, no write proposed
- external-id match with foreign `chv_managed_by` → `conflict`
- partial-failure resume → `update`
- stale marking on removed CHV resources
- deterministic plan ordering (byte-stable serialization)
- secret exclusion (a `secret_ref`/`password` never reaches a mapped object)
- prefix-configurable custom fields

---

## PR 3 — Store: config + runs repositories, migration

**Scope:**

- `cmd/chv-controlplane/migrations/0059_netbox_projection.sql`
  (next free number at plan time — see the surface-analysis note above):
  `netbox_projection_config` (PK `architecture_id`, FK to topologies) and
  `netbox_projection_runs` (PK `id`, FKs to topology + version, status,
  trigger, mode, plan/result/summary JSON, attempt_count, timestamps; partial
  unique index enforcing one active run per architecture:
  `UNIQUE(architecture_id) WHERE status IN ('queued','running')`).
- `crates/chv-controlplane-store/src/architectures/netbox_config.rs` —
  upsert (idempotent), get, delete; token encrypted via
  `CredentialEncryption` at write time, decrypted only on explicit
  `read_token` by the worker/runner.
- `.../netbox_run.rs` — create/list/get, `claim_next_queued` (atomic
  queued→running), `mark_succeeded`/`mark_failed` (with attempt_count
  increment), retry enqueue.
- Types in `chv-controlplane-types/src/architecture/` (new module
  `netbox.rs`: config/run structs, status enums, ids) following the existing
  identifier pattern (`ArchitectureDriftReportId`-style).

**Tests:** repo suite mirroring `architectures/tests.rs` (create/claim/
terminal transitions, one-active-run invariant, FK behavior, token round-trip
+ redaction).

---

## PR 4 — Client + runner + worker

**Scope:**

- `chv-netbox-adapter/src/client.rs` — `reqwest` + rustls; HTTPS-only
  enforcement at construction; token auth header; bounded to the NetBox 4.x
  REST endpoints in the mapping contract; fail-closed response parsing;
  timeouts; the token held in an opaque wrapper whose `Debug`/`Display` are
  redacted.
- `chv-netbox-adapter/src/runner.rs` — fetch NetBox state (by custom-field
  + natural-key lookups), call `compute_plan`, execute entries in order
  (create/update/stale only — conflicts are recorded, never written),
  persist per-entry outcomes, abort-on-first-hard-failure policy with
  resume-by-external-id.
- `chv-controlplane-service/src/netbox_projection_worker.rs` —
  `BackupWorker`-shape loop: claim next queued run, load config + decrypt
  token in-memory, run, mark terminal, emit events; bounded retries with
  backoff; lease/timeout reclamation for crashed runs.
- Spawn in `cmd/chv-controlplane/src/bootstrap.rs` next to the orchestrator
  and backup worker.

**Tests:** runner against a mock NetBox HTTP server (wiremock): success,
partial failure → resume without duplicates, 401 → `NETBOX_AUTH_FAILED`,
outage → failed/retryable; worker claim atomicity; token never in logs
(`LogCollector` assertion, mirroring `credential_crypto` tests).

---

## PR 5 — BFF surface

**Scope:**

- `crates/chv-webui-bff/src/handlers/netbox.rs` implementing the eight
  endpoints of the API contract (config get/upsert/delete, dry-run, export,
  runs list/get/retry) with:
  - `require_owner_or_admin` object scoping (reuse from
    `handlers/architectures.rs`);
  - production → Admin escalation for export (parity with apply);
  - `mark_stale` default; `delete` retention requires Admin;
  - stable error codes from the contract;
  - dry-run executed synchronously (bounded by client timeout), export
    enqueued to the runs table.
- `AppState` gains `Arc<NetboxProjectionConfigRepository>` and
  `Arc<NetboxProjectionRunRepository>`; routes registered in the
  authenticated Operator/Admin layer of `bff_router`.

**Tests:** BFF suites mirroring `architectures.rs` /
`architecture_permission_matrix.rs`: permission matrix (viewer 403, operator
ok, admin ok, production gating), ownership/IDOR (foreign row 403/404),
config token redaction, active-run 409, error-code stability.

---

## PR 6 — Post-apply trigger (isolated)

**Scope:**

- At the architecture apply-run terminal transition site (where the run
  reaches `Succeeded` (status string `succeeded`), a best-effort hook: if a `netbox_projection_config`
  exists with `enable_post_apply = true`, insert a `queued` run
  (`trigger = post_apply`).
- The hook **cannot fail the apply**: all errors are logged and swallowed
  (this is the issue's "NetBox outage does not change the apply result" AC,
  made mechanical).
- Coalescing: if an active run exists, skip (the next export reconciles).

**Tests:** failed projection enqueue leaves apply run `Succeeded`; no config
→ no run; `enable_post_apply = false` → no run; active run → coalesce.

> **Deviation note (implemented shape).** The apply-run terminal transition
> site named above does not exist: `apply_plan`
> (`chv-architecture-reconcile`) never transitions runs to a terminal
> `Succeeded`/`PartiallyFailed` state (only `Running`, plus rollback
> `Cancelled`/`Failed` paths), and its
> module doc defers the terminal `Succeeded` / `PartiallyFailed` / `Failed`
> transitions to the (not-yet-implemented) orchestrator — "this module only
> puts the run on the rails". PR 6 is therefore implemented as a
> **worker-side sweep** in `NetboxProjectionWorker::tick`, checked after
> `reclaim_stale_runs` and before the claim loop: each tick lists configs
> with `enable_post_apply = true` (`NetboxProjectionConfigRepository::
> list_post_apply_enabled`), takes each architecture's most recent
> `succeeded` apply run, and enqueues a `queued` `post_apply` export run
> (system-requested, no plan snapshot) for that version.
> - *Idempotency* across ticks: `NetboxProjectionRunRepository::
>   has_post_apply_for_version` — a `post_apply` run of **any status**
>   counts as already attempted, so a permanently-failed post-apply run is
>   not re-enqueued every tick (transient retries are owned by the PR-4
>   bounded auto-requeue; after the attempt cap the operator retries).
> - *Coalescing*: the `netbox_projection_runs_one_active` partial index —
>   the enqueue's create fails with the store's active-run conflict, which
>   the sweep treats as a skip.
> - *Isolation*: the apply path calls nothing (a NetBox outage or a
>   projection-store failure is structurally incapable of changing an apply
>   result), and a per-architecture failure inside the sweep is warned and
>   skipped without blocking the other architectures. Once the
>   orchestrator's terminal transitions land, the sweep fires on the very
>   next tick with zero changes.

---

## PR 7 — WebUI

**Scope:**

- `ui/src/lib/bff/architectures.ts` — typed client functions for the eight
  endpoints (+ vitest).
- `ui/src/lib/stores/architecture-netbox-store.svelte.ts` — config, dry-run
  plan, runs state.
- `ui/src/routes/architectures/[id]/` — NetBox projection panel:
  - config form (endpoint, token write-only field, retention, post-apply
    toggle, site);
  - dry-run report table (deterministic render; conflict rows with the
    "CHV does not own this object" cue);
  - export action with production confirmation prompt;
  - run history with per-entry outcomes and retry.
- Mutations via `mutateWithRefresh()`; Svelte components stay under ~300
  lines (extract subcomponents).

**Tests:** store + client vitest suites; component tests for the conflict
rendering.

---

## PR 8 — Integration leg (mock NetBox, end-to-end)

**Scope:**

- An end-to-end test composing: topology → apply → post-apply enqueue →
  worker → mock NetBox (assert objects + custom fields) → dry-run no-op →
  topology update + re-apply → update entries → removed resource → stale
  entry → outage leg (mock down) → apply result still `Succeeded`.
- A short qualification note under `docs/evidence/` referencing the mock
  contract boundary (real-NetBox qualification is a separate follow-up).

**Proves (the issue's ACs):** idempotent manual + post-apply export,
deterministic secret-free dry-run, no duplication after partial failure,
foreign objects never modified, outage isolation, audit trail, bounded mock
contract.

---

## Out of scope (tracked follow-ups, not this plan)

- Observed runtime IP/interface projection (needs `InventorySnapshot`
  extension — its own design).
- Real-NetBox qualification leg (needs a live environment).
- Multi-sink abstraction (the adapter is structured so a second sink can
  become a trait implementation later).
- NetBox → CHB compare/report surface (non-goal of the issue).
