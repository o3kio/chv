# Privileged-mutation audit log (#658)

The control plane keeps one durable, append-only record of every
privileged mutation, written to `audit_log` by two mutation emitters:

1. **HTTP (BFF)** — centralized in middleware wrapped around the whole
   HTTP surface; handlers do not call an audit function.
2. **gRPC (node-facing)** — centralized in the tonic server adapters in
   `crates/chv-controlplane-service/src/server.rs`; handlers do not call
   an audit function. Coverage and its explicit exclusions are below.

A third emitter,
`crates/chv-controlplane-service/src/denial_audit.rs`, appends anonymous
interceptor-rejection rows (`interceptor_denied`); those are denial
evidence, not privileged mutations, and are scoped separately in the
denial boundary below.

All three emitters share the schema, retention, and admin read surface,
but use different `action`/`method`/`path` vocabularies (documented in
the Row contract).

## Scope

### HTTP surfaces

Recorded: every non-safe HTTP method on an audited surface that is not
an explicitly read-shaped POST. Audited surfaces:

- `/v1/*` — the BFF surface, including the guest-agent browser routes
  (`/v1/monitoring/agents/*`, mounted by the control plane outside the
  BFF router) and the security-relevant self-service auth actions
  `/v1/auth/change-password` and `/v1/auth/logout` (a session thief's
  seizure step is evidence);
- `/admin/*` — control-plane admin mutations;
- `/api/v1/*` — the legacy cookie-authenticated surface (the hard-fenced
  backup routes, which currently answer 501);
- `/internal/*` — control-plane internal management.

Unknown future non-safe paths on any audited surface fail closed into a
row: probing an unrouted write path leaves a trace.

Excluded:

- `POST /v1/auth/login` and `POST /api/v1/auth/login` — credential
  entry: the body carries the password, the caller is anonymous by
  definition, and brute-force volume is accounted for by auth rate
  limiting, not by this trail;
- `/monitoring/v1/*` — the machine-to-machine guest ingest transport
  (TLS-certificate authenticated, high frequency, not a privileged
  human mutation);
- reads, including filter POSTs such as `POST /v1/vms`, `/v1/tasks`,
  `/v1/audit-log`, `/v1/audit-log/export`,
  `POST /v1/cloud-init-templates/:id/render` (pure substitution, no
  write), and the guest-agent inventory list
  `POST /v1/monitoring/agents`.

### gRPC surfaces

Recorded: every gRPC mutation on the three mutable node-facing services
below (listener `chv.controlplane.node.v1.*`). The wrapper is applied to
the server adapters, not the service traits, so in-process BFF calls into
`LifecycleService` are not double-audited: only the wire surface emits.

- `LifecycleService` — all RPCs except `PingVmm`, including the three
  `Unimplemented` shims (`CreateVolume`, `DeleteVolume`,
  `ApplyNetworkPolicy`) so probing an unsupported mutation leaves a
  trace;
- `EnrollmentService` — `EnrollNode`, `RotateNodeCertificate`, and
  `ReportBootstrapResult`;
- `ReconcileService` — `ApplyNodeDesiredState`, `ApplyVmDesiredState`,
  `ApplyVolumeDesiredState`, `ApplyNetworkDesiredState`, and
  `AcknowledgeDesiredStateVersion`. Desired-state convergence is a
  privileged node mutation: a compromised node changing its own desired
  state must be visible.

Excluded:

- `PingVmm` — unconditional read-shaped probe that returns
  `Unimplemented` without reading node state;
- the agent-side `LifecycleService` (a different process and trust
  boundary);
- the read-shaped `InventoryService` and `TelemetryService` (high-rate
  machine reporting, not privileged mutations);
- `MonitoringService` (`IngestNodeMetricBatch`) — machine-to-machine
  metric ingest on the same listener, high-rate and not a privileged
  mutation.

Those services remain excluded at the **handler** level only:
interceptor-level rejections on `InventoryService`, `TelemetryService`,
and `MonitoringService` still write durable denial rows (see the denial
boundary below).

**Denial boundary.** The steady-state mTLS interceptor rejects requests
*before* the handler runs. Those rejections now produce a durable
`audit_log` row with the anonymous actor: `action=interceptor_denied`,
`method=gRPC`, `path=<fully-qualified gRPC service>`, `outcome=denied`,
`status_code=401`. `metadata` records `reason` (`missing_certificate`,
`unusable_serial`, or `revoked_serial`) and, when present, the
certificate serial (never the certificate itself, tokens, or bodies).
`source_ip` is recorded when the transport exposes it. A revoked serial
still increments `chv_revoked_peer_rejected_total`.

**Method-level fidelity limit.** Server-side tonic does not expose the
RPC method: `tonic::extensions::GrpcMethod` is inserted only by generated
client code, and `tonic::Request` drops the HTTP URI. Interceptor rows
therefore record the gRPC **service** as `path`; method-level path
fidelity is a follow-up. Handler-level denials continue to carry
`<service>/<RpcName>` and are inside the audited block, so peer/node
mismatch and revoked-peer rejections on `RotateNodeCertificate` and
`ReportBootstrapResult` are recorded as before.

**Flood control.** Rejections are queued on a bounded (1024) channel and
written off the request path. A full or closed channel increments
`chv_grpc_denial_audit_dropped_total{cause="full"|"closed"}`; it never
blocks or changes request handling. Identical `(service, reason, peer)`
rejections are recorded at most once per 60 seconds (peer = certificate
serial when usable, else source IP, else the literal `unknown`), and
suppressed duplicates increment
`chv_grpc_denial_audit_suppressed_total{reason="..."}` while still
incrementing the rejection metrics — a quiet audit trail is not proof of
a quiet boundary. The window is an operational default and may be tuned
later. Successful and failed writes are counted by
`chv_grpc_denial_audit_written_total{reason="..."}` and
`chv_grpc_denial_audit_write_failures_total{reason="..."}`.

At shutdown the writer flushes at most 64 already-queued events; anything
still queued is counted in
`chv_grpc_denial_audit_dropped_total{cause="shutdown"}`. A hard process
kill loses whatever is still queued — the same crash-window family as the
still-open half of #706. If the listener fails or the 5s drain budget is
exceeded, the writer is aborted and queued events are lost **uncounted**
(the same crash-window family), bounded at ≤1024 events.

The crash window (a mutation commits before its audit row is written)
remains open for the denial writer and for every mutation site not
coupled to the audit outbox; the BFF and enrollment adoption below
closes it for the listed routes and tracks the rest under #706.

## Mounting

Standalone callers (tests, embedders) get the audited surface from
`chv_webui_bff::bff_router`, which wraps its routes with
correlation → metrics → audit (outermost first). The production
control plane instead mounts the doc-hidden
`chv_webui_bff::bff_router_unaudited` (routes, role middleware, and
CSRF only) inside `admin_router_with_guest_agents` and applies the
same three layers **at the outermost production router, after fallback
registration**, so they also wrap the CP's extra privileged surfaces
and the router fallback — exactly once, never twice.

Layer order (outermost first) and why:

1. **correlation** — the id is already in request extensions when the
   audit row is built, and every response (including audit's 500
   replacement) carries `x-correlation-id`;
2. **metrics** — outside the audit layer so the status it counts is the
   final one on the wire, including the 500 `AUDIT_LOG_WRITE_FAILED`
   replacement. The `path` label is the matched route template from
   axum's `MatchedPath` (bounded by the route table), never the raw
   URI; API-prefixed requests that match no route (`/v1/`, `/api/`,
   `/admin/`, `/internal/` probes) collapse to the single constant
   label `__unmatched__`, so attacker-chosen 404 segments cannot
   balloon histogram cardinality. Non-API paths (the static tree,
   health probes, scrapes) are skipped;
3. **audit** — wraps every route and the fallback.

## Row contract

`audit_log` (control-plane SQLite database, migration `0064`) stores:

| Column | Meaning |
|---|---|
| `audit_id` | Server-minted 128-bit id (primary key). |
| `occurred_at_ms` | Epoch-millisecond completion timestamp; for an adopted mutation it is stamped at enqueue (the commit attempt) and replayed verbatim by the dispatcher. |
| `actor_id`, `actor_username`, `actor_role` | Verified claims, or the explicit anonymous marker. |
| `action` | Stable action. HTTP: dotted, derived from method/path, e.g. `vm.create`. gRPC: the protobuf RPC name, e.g. `StartVm`; interceptor rejections use the fixed literal `interceptor_denied` (not an RPC name). |
| `method`, `path` | Request shape; never the query string. HTTP: verb + matched route. gRPC: `gRPC` + `<fully-qualified service>/<RpcName>`. The only gRPC rows without an RPC suffix are interceptor rejections, whose `path` is `<fully-qualified gRPC service>`: server-side tonic does not expose `GrpcMethod` (it is inserted only by generated client code) and `tonic::Request` drops the HTTP URI. |
| `resource_kind`, `resource_id` | Best-effort resource. HTTP: path-derived; bodies are intentionally not read. gRPC: request fields (capped), with `EnrollNode` using the issued node id on success. |
| `outcome` | `success` (2xx/3xx), `denied` (401/403), `failed` (other 4xx/5xx). gRPC status codes are mapped to the HTTP equivalents (200/400/401/403/404/409/412/429/499/500/501/503/504). |
| `status_code` | Final HTTP status, or the gRPC code's HTTP equivalent. |
| `correlation_id` | Correlation id assigned by the correlation middleware (HTTP), capped at 128 chars. gRPC rows leave it NULL; `operation_id` is the join key for agent traffic. |
| `operation_id` | Inbound `x-operation-id` when supplied (HTTP or gRPC metadata), capped at 128 chars. |
| `source_ip` | Transport peer address (see below), or a validated forwarding hop. On gRPC this is always the direct TCP peer, never a metadata header. |
| `metadata` | Allowlisted redacted JSON object. HTTP: `auth_source`. gRPC: `transport: "grpc"` and `auth_source` ∈ `mtls` \| `insecure` \| `bootstrap_token`. Interceptor denials use `auth_source: "mtls_rejected"`, `reason` ∈ `missing_certificate` \| `unusable_serial` \| `revoked_serial`, and — only when the serial matched the operator-controlled revocation denylist, so the value is bounded by operator data rather than arbitrary client bytes — `certificate_serial` (capped at 256 chars). |

Client-controlled identifier strings (`operation_id`,
`correlation_id`) are capped at 128 characters so a hostile or buggy
client cannot balloon row size with megabyte header values. Path-derived
fields (`path`, `action`, `resource_kind`, `resource_id`) are capped at
256 characters before insert: unknown-path fallbacks and dynamic URL
segments are attacker-chosen, and a row must not become a payload.

`source_ip` prefers the transport peer address when the server
provides connect info — both serve paths (plain HTTP via
`into_make_service_with_connect_info`, TLS via the peer-inject
service) inject `ConnectInfo`. Only when connect info is absent does
the row fall back to the first valid `x-forwarded-for` / `x-real-ip`
hop, a value only a **trusted reverse proxy** can be expected to set
correctly; deployments not behind such a proxy must treat `source_ip`
as unverified. Invalid values are dropped.

The audit middleware decodes the credential once and shares the result
with the `BearerToken` extractor through request extensions
(server-side only — a client cannot forge extensions), so the audit
row's actor and the authorizer's identity cannot drift and an API
token is decoded (and its `last_used_at` touched) once per mutating
request instead of twice.

There is no foreign key to `users`: an audit row must outlive the
account it names, otherwise deleting a user would erase evidence of
that user's privileged actions.

## Exactly-once semantics

A request is classified once, executed once, and then the middleware
attempts one insert. Under a healthy database this is exactly one row
per mutating request, including failed and denied attempts. Adopted BFF
routes and the adopted gRPC RPCs (enrollment and reconcile) keep the same
exactly-one-row guarantee through a different emitter: the handler
commits the row into `audit_outbox` with the mutation and the emitter's
check-or-insert skips the direct insert (see the transactional-outbox
section below).

For the direct (post-hoc) path, the insert happens **after** the handler
returns. If the insert fails:

- the `chv_bff_audit_write_failures_total` counter is incremented — a
  systemic audit-store outage is visible on dashboards, not only in
  logs;
- the failure is logged loudly at `error` level with target `audit`
  and the action/path/status;
- if the mutation response was 2xx/3xx, the BFF replaces it with HTTP
  500 `AUDIT_LOG_WRITE_FAILED` (including the correlation id as
  `request_id`, so the operator can find what the mutation actually
  did) rather than claiming success without a trail;
- if the response was already a 4xx/5xx (including 401/403), the
  original response is preserved and the missing row is logged.

**gRPC emitter.** The same contract applies at the handler boundary:

- one row per RPC, including validation denials, peer mismatch, revoked
  peers rejected inside a handler, handler errors, and the
  `Unimplemented` shims;
- the insert happens after the handler returns. On insert failure the
  `chv_grpc_audit_write_failures_total` counter is incremented and an
  `error` log is emitted under target `audit`;
- a successful mutation whose row cannot be written returns gRPC
  `INTERNAL` with a greppable `AUDIT_LOG_WRITE_FAILED` message; a
  handler failure keeps its original status;
- `EnrollNode` fails closed the same way. Because its bootstrap token is
  single-use and already consumed, a retry after an
  `AUDIT_LOG_WRITE_FAILED` cannot succeed: the operator must issue a new
  one-time token (or remove the half-enrolled node) before the node can
  re-enroll. This is the deliberate fail-closed tradeoff, not automatic
  recovery.

**Adopted gRPC RPCs.** The three enrollment RPCs, the five reconcile
RPCs, and the five node-lifecycle transitions commit their evidence row
with the mutation's transaction (see the transactional-outbox section),
so the post-hoc insert above is skipped on success. The failure modes
are narrower but not gone: an enqueue failure fails the mutation
transaction (the adapter then records the failed request once on the
direct path), and a crash before the coupled transaction commits is not
covered for any earlier writes the RPC makes. The `EnrollNode` token
caveat stands — the token is consumed in an earlier commit, so a
rolled-back enrollment still burns it and the operator must issue a new
one. The reconcile residual is smaller: only the convergence-event
append (and, for `AcknowledgeDesiredStateVersion`, the operation-status
update) follows the coupled write.

Residual risk: the mutation has already happened when the insert is
attempted. A 500 `AUDIT_LOG_WRITE_FAILED` therefore does not roll the
mutation back; it tells the caller the audit trail is incomplete.

### Transactional outbox (BFF and gRPC adopted)

The control plane ships an `audit_outbox` table (migration `0071`) and a
bounded dispatcher for it (#706 half 1). Mutation sites are adopted
incrementally; the contract below holds for every adopted site, and
everything else keeps the post-hoc insert above byte-for-byte, including
its crash window.

**Adopted in PR 2 (HTTP/BFF).** These are pure-database,
single-transaction mutations:

| Route | Action | Shape |
|---|---|---|
| `POST /v1/users/create` | `user.create` | INSERT + outbox row in one transaction (was autocommit) |
| `POST /v1/users/update` | `user.update` | UPDATE + outbox row in one transaction |
| `POST /v1/users/delete` | `user.delete` | DELETE + outbox row in one transaction |
| `POST /v1/tokens/create` | `api_token.create` | INSERT + outbox row in one transaction (was autocommit) |
| `POST /v1/tokens/revoke` | `api_token.revoke` | DELETE + outbox row in one transaction |
| `POST /v1/networks/create` | `network.create` | the existing transaction now also enqueues |
| `POST /v1/networks/update` | `network.update` | the existing transaction now also enqueues |
| `POST /v1/networks/delete` | `network.delete` | the existing transaction now also enqueues |
| `POST /v1/vms/import` | `vm.import` | the **final** transaction couples the VM/volume/operation rows; the filesystem work happens before it |

**Adopted in PR 3a (gRPC enrollment).** The adapter mints the ticket and
passes the outbox row into the service, which enqueues it inside its
final mutation transaction:

| RPC | Action | Shape |
|---|---|---|
| `EnrollNode` | `EnrollNode` | the **final** fail-closed write carries the row: `register_fabric_identity` when the node reports a fabric key, otherwise the version-history append. Token consumption, the node row, and the inventory row commit earlier and are **not** coupled (residual risk below) |
| `RotateNodeCertificate` | `RotateNodeCertificate` | the compare-and-swap that records the new serial and the outbox row are one transaction; a lost race enqueues neither and the adapter records the conflict on the direct path |
| `ReportBootstrapResult` | `ReportBootstrapResult` | the bootstrap-result upsert and the outbox row are one transaction |

**Adopted in PR 3b (gRPC reconcile).** Same adapter shape; the service
enqueues inside the desired-state write it already runs in a
transaction:

| RPC | Action | Shape |
|---|---|---|
| `ApplyNodeDesiredState` | `ApplyNodeDesiredState` | `set_state_preserving_policy` and the outbox row are one transaction (the single mutation statement plus the outbox INSERT on the adopted path) |
| `ApplyVmDesiredState` | `ApplyVmDesiredState` | the existing desired-state upsert transaction now also enqueues; a stale generation rolls both back |
| `ApplyVolumeDesiredState` | `ApplyVolumeDesiredState` | as above |
| `ApplyNetworkDesiredState` | `ApplyNetworkDesiredState` | as above, including the exposure rows; a `Deleting` tombstone or stale generation rolls everything back |
| `AcknowledgeDesiredStateVersion` | `AcknowledgeDesiredStateVersion` | the observed-state acknowledgement and the outbox row are one transaction (the single mutation statement plus the outbox INSERT on the adopted path); a missing observed-state row enqueues neither |

**Adopted in PR 3c (gRPC lifecycle — node transitions).** Same adapter
shape, but the coupling is **intent-anchored**: the privileged state
change and its outbox row share one commit, and nothing else is folded
in. The operation row and its `OperationStarted` event are earlier,
separate commits (residual below).

| RPC | Action | Coupled intent write |
|---|---|---|
| `PauseNodeScheduling` | `PauseNodeScheduling` | `set_scheduling_paused` (`scheduling_paused = true`) and the outbox row are one transaction |
| `ResumeNodeScheduling` | `ResumeNodeScheduling` | `set_scheduling_paused` (`scheduling_paused = false`) and the outbox row are one transaction |
| `DrainNode` | `DrainNode` | `set_drain_intent` (upsert `desired_state = Draining`) and the outbox row are one transaction |
| `EnterMaintenance` | `EnterMaintenance` | `set_state_preserving_policy` (upsert `desired_state = Maintenance` plus `state_reason`) and the outbox row are one transaction |
| `ExitMaintenance` | `ExitMaintenance` | `set_state_preserving_policy` (upsert `desired_state = TenantReady`) and the outbox row are one transaction |

Journal-anchoring these five (putting the outbox row in the operation
INSERT) was **rejected** for this slice: that row commits before the
intent, so a crash in between would leave a `success` audit row for a
scheduling change that never happened — false evidence, which is worse
for incident response than missing evidence. The cost of the chosen
shape is that the operation journal is not covered by the same commit;
see the residual paragraph below. An intent failure (for pause/resume, a
missing `node_desired_state` row) still runs the existing `Failed`
operation journal and `OperationFailed` event and writes exactly one
direct failed audit row; no outbox row exists because the intent
transaction rolled back.

**Residual window (lifecycle node transitions, PR 3c).** The coupled
unit is exactly one statement pair: the intent write plus the outbox
INSERT. Everything else in these five handlers is outside it, by design:

- The `operations` INSERT and the `OperationStarted` event are **earlier,
  separate commits**. A crash between them and the intent commit leaves
  **no state change and no audit row** — the pre-existing window,
  unchanged by this PR, not a covered case.
- A crash after the intent + outbox commit but before the operation's
  `Accepted` status update leaves the **applied state plus a `success`
  audit row** (the commit-time prediction) and a possibly **stale
  operation status** (`Pending`, which neither the dispatcher nor the
  reaper claims today). The audit evidence is correct; the journal is
  incomplete.
- An **intent failure** keeps the existing `Failed` operation journal and
  `OperationFailed` event and produces **exactly one direct failed audit
  row**; the intent transaction (including the enqueue) rolled back, so
  no outbox row exists.
- A **non-crash failure of the later `Accepted` status update** returns an
  error and patches the already-committed row to `failed`/500; the state
  change and its evidence stay (evidence is not retracted).
- Adoption is **per-RPC**. The other lifecycle RPCs (including `StartVm`)
  remain post-hoc with their original crash window. This PR makes **no
  claim that the lifecycle surface is globally closed**; the remaining
  groups are follow-up slices under #706.

**Not adopted (still post-hoc).** Every other audited route — notably
the rest of the gRPC `LifecycleService` surface (agent-ack fan-out and
filesystem work inside the handlers) including `StartVm`, `StopVm`,
`RebootVm`, `DeleteVm`, `ResizeVm`, `PauseVm`, `ResumeVm`,
`PowerButtonVm`, the volume attach/resize/snapshot/clone RPCs, the
network lifecycle RPCs, `MigrateVm`, `UpdateOverlay`,
`SendGratuitousArp`, `ResolveInspectRequiredOperation`, the
`Unimplemented` shims, `vm.delete`, `volume.delete`, anything that
performs filesystem or agent RPC work inside the handler, and the
guest-agent browser routes. They keep today's direct insert and its
narrow crash window. Denied and failed requests also stay post-hoc:
there is no state change to lose. The reconcile surface has no
read-shaped or agent-fan-out RPC, so all five of its RPCs are adopted;
the lifecycle surface is adopted per-RPC, not as a whole.

**Check-or-insert (HTTP).** For every classified HTTP mutation the
middleware mints (server-side, UUIDv4, never read from a request header
or body) an `outbox_id` and a pre-minted `audit_id`, and puts them on the
request as an `AuditOutboxTicket`. An adopted handler passes its mutation
transaction to `commit_adopted_mutation`, which calls `enqueue_on_tx`
immediately before the commit. After the handler returns the middleware
checks the outbox by primary key:

- **row present** → the evidence row is durable; no second row is
  written. If the final status is not 2xx/3xx, `finalize_pending` patches
  the pending row's outcome/status. If the dispatcher already won the
  race, the mismatch is logged with the `outbox_id` and the row keeps its
  commit-time `success` prediction.
- **row absent** → denied, failed before commit, rolled back, or
  unadopted. The middleware inserts directly, exactly as before.
- **check fails** → never a double write: when the ticket was committed
  (the handler enqueued and committed), the middleware trusts the
  durable outbox row and skips the direct insert; otherwise it falls back
  to the direct insert.

**Check-or-insert (gRPC).** The adopted gRPC RPCs (the three enrollment
RPCs, the five reconcile RPCs, and the five node-lifecycle transitions)
use the same composition in
`finish_audited`: the adapter mints an `outbox_id`/`audit_id` pair
(UUIDv4, never from the wire), hands the `AuditOutboxInsert` to the
service, and after the RPC returns:

- **row present** → the evidence committed with the mutation's final
  transaction; the direct insert is skipped. A non-success final status
  patches the pending row via `finalize_pending` (the mutation and its
  evidence stay; only the recorded outcome changes).
- **row absent** → denied, failed before commit, rolled back, or the
  service did not adopt. The adapter inserts directly, exactly as the
  unadopted path always has.
- **check fails** → the service records that its enqueue committed in a
  per-request committed flag; a set flag means the evidence is durable in
  the outbox even though the check failed, so the adapter skips the
  direct insert (no duplicate) and logs loudly. Without the flag nothing
  was enqueued and the adapter falls back to the direct insert. If the RPC
  also reported failure, the retained row keeps its commit-time `success`
  prediction, because the patch cannot read a row the check could not —
  the same disclosed limit as the patch-failure arms of
  `patch_final_outcome`.

Both transports stamp `occurred_at_ms` (and the outbox row's
`created_at_ms`, its initial due time) at **enqueue**, not request
start: the BFF ticket does it when it builds the insert, and the adopted
`chv-controlplane-store` methods clone the pre-minted row and re-stamp it
immediately before `enqueue_on_tx`. The unadopted direct path keeps its
post-handler stamp.

The composition assumes **enqueue is the last fallible operation** of an
adopted mutation; that is true for every current adopted site. The
committed flag makes the check-error fallback duplicate-free even if a
future service ever enqueues and then fails. Patching a committed row's
final outcome is evidence-preserving either way: the mutation and its
row stay, only the recorded `outcome`/`status_code` changes.

No path produces two rows for one request, and no adopted success path
produces zero rows: the outbox row and the mutation are one commit. The
verification evidence is `crates/chv-webui-bff/tests/audit_log.rs`
(adopted success, rollback after the `BEGIN IMMEDIATE` mutation
transaction, crash between commit and middleware return, injected
post-commit non-2xx, unadopted route, oversized actor, distinct ids) and
the `server_audit_*` adoption tests in
`crates/chv-controlplane-service/src/server_audit_tests.rs` (one
dispatched row per adopted RPC, the post-commit-failure patch path — a
service that enqueues and then fails keeps exactly one row whose
outcome/status are patched; removing `finalize_pending` fails that test —
rollback/denial with no outbox row, crash replay, unadopted lifecycle RPC
still direct, oversized payload rolls back).

The contract adoption relies on:

- **Enqueue is transactional.** An adopted mutation calls
  `enqueue_on_tx` inside the transaction it already holds, so the
  `audit_outbox` row commits with the mutation or not at all. The row
  carries a pre-minted `audit_id` and the already-redacted
  `AuditLogInsert` as JSON; the dispatcher never re-derives content.
- **Enqueue is at-least-once; dispatch is exactly-once.** The emitter
  mints one `outbox_id` per request. A duplicate enqueue is a loud
  store error (there is no `INSERT OR IGNORE` against the outbox key),
  while a replayed *dispatch* is safe: `complete` inserts the
  `audit_log` row and flips the outbox row to `dispatched` in one
  transaction, reusing the pre-minted `audit_id`. There is no dedupe
  scan and no duplicate evidence row.
- **No dead-letter and no attempt ceiling.** Audit evidence is never
  droppable, so failed dispatches retry forever with 1 s → 60 s capped
  exponential backoff (±20 % jitter). The claim lease is 60 s; a
  dispatcher crash simply lets the lease expire and the rows return.
- **Bounded staleness.** An adopted path's row becomes visible to the
  admin API at most one dispatch interval plus one backoff step after
  commit (default ≈ 2 s), including a replay pass immediately after
  restart. Unadopted routes keep the immediate visibility above.
- **Adopted success paths fail closed at commit.** If the enqueue
  itself fails, the mutation transaction fails, so the post-hoc 500
  `AUDIT_LOG_WRITE_FAILED` cannot happen for an adopted route. The
  caller instead gets a 500 saying the mutation was not performed, and
  the middleware records that failed request once on the direct path.
  Denied/failed/unadopted paths keep the direct post-hoc insert above.
- **The crash window closes only where mutation and outbox share a
  commit.** Every adopted store method enqueues inside the transaction
  that already commits the mutation, so a crash after that commit and
  before the adapter's post-hoc insert loses nothing: the dispatcher
  replays the row. Coverage is per-write, not per-RPC, where an RPC
  performs more than one commit:
  - the three enrollment RPCs carry the row on their **final**
    transaction (details above). `EnrollNode` remains multi-statement:
    its one-time token consumption is a separate, earlier commit, as are
    the node and inventory rows. A crash between those commits and the
    final one leaves a partially applied enrollment with **no** outbox
    row — the token can be consumed with no enrollment evidence. That is
    a residual risk, not a covered case; folding enrollment into one
    transaction is follow-up work. `vms/import` has the same shape: its
    temporary-directory setup and file move precede the final
    transaction.
  - the four reconcile `Apply*DesiredState` RPCs couple their **primary**
    desired-state write (the one that changes the resource). The
    convergence event appended afterwards is a separate commit: a crash
    between the two leaves the desired-state change and its audit row
    durable with no `events` row (the same partial-mutation shape that
    exists today, now with the evidence present rather than missing), and
    a late event-append failure patches the already-committed row to
    `failed` — the mutation and evidence stay, only the recorded outcome
    changes. Coupling the *later* event append instead was rejected: it
    would leave the earlier state change unaudited on crash.
  - `AcknowledgeDesiredStateVersion` couples the observed-state
    acknowledgement; the operation-status update and the convergence
    event that follow are separate commits with the same residual shape.
  - the five node-lifecycle transitions (`PauseNodeScheduling`,
    `ResumeNodeScheduling`, `DrainNode`, `EnterMaintenance`,
    `ExitMaintenance`) couple the **intent** write only. Their operation
    row and `OperationStarted` event are earlier commits, so a crash
    between journal and intent leaves no state change and no audit row
    (the pre-existing window); a crash after the intent commit leaves the
    applied state and its `success` row with a possibly stale operation
    status. Coupling the journal instead was rejected: it would record a
    `success` audit row for a scheduling change a later crash prevented
    (false evidence), not merely missing evidence.
  - `RotateNodeCertificate`, `ReportBootstrapResult`, and the reconcile
    RPCs have no earlier mutation commit on their adopted path.

**Lock scope.** Wrapping the formerly autocommit `users`/`tokens`
statements in a transaction extends the SQLite write lock by one INSERT
(the outbox row) per adopted mutation; it introduces no new lock
ordering. Those transactions, and `networks/create`, use
`BEGIN IMMEDIATE` — the repo's write-serializing discipline — because
their first statement is a write; a deferred `BEGIN` would only defer
the same lock and risk a `SQLITE_BUSY` at that first write statement
(the mutation) instead of at `BEGIN`. `networks/update`,
`networks/delete`, and `vms/import` already held a transaction, so only
the extra INSERT is new there.

On gRPC the adopted store methods take an optional outbox row and open a
transaction only when one is present (otherwise they keep the original
autocommit statement). `register_fabric_identity` already had its own
transaction, so enrollment adds only the INSERT there; the rotation
compare-and-swap and the bootstrap-result upsert gain a transaction
around the mutation statement plus the outbox INSERT on the adopted
path. On the reconcile path
`upsert_vm`, `upsert_volume`, and `upsert_network_with_exposures`
already opened a transaction, so only the INSERT is new;
`set_state_preserving_policy` and the four observed-state
acknowledgements gain a transaction around the mutation statement plus
the outbox INSERT on the adopted path. For the PR 3c node transitions
`set_scheduling_paused`, `set_drain_intent`, and
`set_state_preserving_policy` wrap the single intent statement plus the
outbox INSERT in `BEGIN IMMEDIATE` when a row is present (the `None`
branch keeps the original autocommit statement). Because
`set_state_preserving_policy` is shared with `ApplyNodeDesiredState`
(PR 3b), that adopted branch's move from a deferred `begin()` to
`BEGIN IMMEDIATE` applies to that reconcile RPC too: it stays a
single-statement mutation in one transaction under WAL with the 5s
`busy_timeout`, with identical atomicity and no lock-ordering change.
Every transaction is held for the store call only — never across
certificate minting, the convergence event append, the operation-status
update, or transport work — so no new lock ordering is introduced.

**Actor-field cap (NIT 5 decision).** The store caps the serialized
payload at 16 KiB; the direct path has no explicit bound on `actor_id` /
`actor_username` / `actor_role`. Adopted sites **fail closed**: the row
is never silently truncated. An oversized payload makes enqueue fail, the
mutation transaction rolls back (no mutation without durable evidence),
and the middleware records the failed request once on the direct path
(which does not truncate the actor either). Covered by
`oversized_actor_fails_closed_and_rolls_back` in the BFF audit tests.

**Username bound (review S2).** To keep that fail-closed path
unreachable for API-created users, `POST /v1/users/create` rejects a
`username` longer than 256 characters with the existing validation shape
(400 `BAD_REQUEST`, no truncation). `users/update` has no rename field,
so there is nothing else to bound. Residual risk, stated honestly: the
bound is enforced only on this creation path. A username longer than 256
characters that already exists in the database (created before this
change, written out-of-band, or seeded/managed outside the BFF) still
works for login and still yields an oversized `actor_username`; that
user's adopted mutations would fail closed until the row is corrected.
The login/JWT path itself performs no username-length validation beyond
the request body limit — it does not need one to be correct, but it does
not close the legacy-row case.

Dispatched outbox rows are kept for 7 days (`[audit]
outbox_retention_days`, validated 1..=365) and pruned in bounded
batches; pending rows are never pruned. This cleanup is plumbing, not
evidence deletion — `audit_log` retention is separate.

**Verification scope.** The crash window and the tx coupling are proven
deterministically by `crates/chv-webui-bff/tests/audit_log.rs` and the
gRPC adoption tests in
`crates/chv-controlplane-service/src/server_audit_tests.rs` (the
`SIGKILL` end-to-end test from the design is a follow-up, not
implemented): an adopted mutation that commits with the emitter never
running is replayed exactly once by the dispatcher, and a **commit
failure after a successful enqueue** (the
`commit_failure_after_a_successful_enqueue_rolls_back_mutation_and_outbox`
test, using a deferred foreign key that SQLite only checks at `COMMIT`)
leaves neither the mutation nor the outbox row — which a pool/autocommit
enqueue could not satisfy. The gRPC tests additionally cover each
adopted RPC's dispatched row, the failed/denied direct row with no
outbox row, crash replay, an unadopted lifecycle RPC, and the oversized
payload rollback (`oversized_audit_payload_rolls_back_the_adopted_rotation`
in `enrollment.rs`, and
`server_audit_reconcile_oversized_payload_rolls_back_the_adopted_write`
for reconcile). The reconcile slice adds, per adopted RPC, one
dispatched row reusing the pre-minted `audit_id` with a second pass
proven to be a no-op; a stale-generation rollback (no outbox row, no
desired-state change, one direct failed row); a missing observed-state
row on `AcknowledgeDesiredStateVersion` (one direct 404 row, no outbox
row); crash replay; the post-commit-failure patch path (a reconcile
service that enqueues and then fails keeps exactly one row whose
outcome/status are patched); the unadopted shape (a service that
ignores the ticket writes one direct row and no outbox row); and both
check-error branches of the gRPC composition (a committed enqueue whose
lookup fails must not direct-insert and is replayed from the durable
row; an uncommitted one falls back to exactly one direct failed row).
The PR 3c lifecycle slice adds, per adopted RPC, the same one-dispatched-
row proof (pre-minted `audit_id` reused, second pass a no-op), an intent
failure (missing `node_desired_state` row: no outbox row, the `Failed`
operation journal and `OperationFailed` event intact, exactly one direct
404 row), crash replay, the `EnqueueThenFail` patch path on
`PauseNodeScheduling`, and an oversized payload that rolls the intent
back (state unchanged, zero outbox rows).
The store suite pins the rotation transaction's atomicity directly
(`test_rotate_certificate_serial_audit_is_atomic_with_the_compare_and_swap`:
a lost compare-and-swap commits neither the serial nor the outbox row)
and the reconcile upsert's
(`test_upsert_vm_audit_is_atomic_with_the_desired_state_write`: a stale
generation commits neither the fragment nor the outbox row, an accepted
one commits both with the pre-minted ids;
`test_upsert_vm_audit_commit_failure_rolls_back_the_desired_state_write`:
a successful enqueue whose COMMIT fails rolls the desired-state write
back too, and the PR 3c
`test_set_scheduling_paused_audit_commit_failure_rolls_back_the_intent`:
the same deferred-FK construction rolls the node scheduling intent back
with the outbox row, while
`test_set_scheduling_paused_audit_is_atomic_with_the_intent` pins the
committed pair and the `NotFound` rollback).

A stuck outbox (dispatcher wedged, database errors) delays evidence
rather than losing it; see
[`docs/runbooks/audit-outbox.md`](../../runbooks/audit-outbox.md) for
the metrics and triage. The interceptor-denial writer (#762) inserts
directly into `audit_log` and is unaffected: denial events have no
mutation transaction to couple to.

## Denials and anonymous actors

The audit middleware wraps the role middlewares, so a 403 from
`viewer_middleware`/`operator_middleware`/`admin_middleware` is
recorded with the verified credential's actor and role. A request with
no valid credential (401) is recorded with the literal actor
`anonymous` / `anonymous` / `anonymous`; the BFF never infers an actor
from an unverified body or header.

On gRPC the actor comes only from the mTLS `PeerNodeId` extension
inserted by the interceptor; a wire-asserted `node_id` is never used
for attribution. `EnrollNode` is token-authenticated rather than
certificate-authenticated, so it forces the anonymous actor and labels
`auth_source: "bootstrap_token"`. When the interceptor ran in insecure
dev mode (`CHV_ALLOW_INSECURE=1`), the handler sees `InsecurePeer`, the
row is anonymous, and `auth_source` is `insecure`. Interceptor-level
certificate rejections **do** write a durable anonymous row, even though
they happen outside the handler: `action=interceptor_denied`,
`method=gRPC`, `path=<fully-qualified gRPC service>`, `outcome=denied`,
`status=401`, with the reason and serial in `metadata` (see the denial
boundary above).

## Redaction contract

The audit row never stores:

- `Authorization` or `Cookie` headers, or any token/JWT/API token;
- passwords or password hashes;
- S3 access keys, secret keys, or session tokens;
- request bodies or query strings;
- arbitrary handler payloads.

`metadata` is constructed from an explicit allowlist:

| Emitter | Allowlisted `metadata` keys |
|---|---|
| HTTP (`crates/chv-webui-bff/src/audit.rs`) | `auth_source` |
| gRPC handlers (`crates/chv-controlplane-service/src/server.rs`) | `transport`, `auth_source` |
| gRPC interceptor denials (`crates/chv-controlplane-service/src/denial_audit.rs`) | `transport`, `auth_source`, `reason`, optional `certificate_serial` |

A future field must be added deliberately to the relevant allowlist and
must be safe to retain for the full retention period. No repository path
exposes free-form metadata from request data. The gRPC emitters also cap
every request-derived string before insert and never record a bootstrap
token, certificate, or handler error detail.

## Admin API

Both endpoints require the `admin` role (and repeat `require_admin` in
the handler as defense in depth).

`POST /v1/audit-log` — filtered page:

```json
{
  "actor": "alice",
  "resource_kind": "vm",
  "resource_id": "vm-123",
  "action": "vm.delete",
  "outcome": "denied",
  "from_ms": 1700000000000,
  "to_ms": 1700003600000,
  "limit": 100,
  "offset": 0
}
```

`actor` matches either the stable actor id or the username. Time bounds
are inclusive. `limit` is clamped to 1..=500 and defaults to 100.
Response: `{ items, count, total, limit, offset }`, newest first with a
stable `(occurred_at_ms DESC, audit_id DESC)` order.

`POST /v1/audit-log/export` — same filters plus `format: "csv"|"json"`
(default `csv`). Exports are bounded to 1..=10,000 rows (default
1,000), chronological and stable. JSON includes
`{ items, count, total, truncated }`; CSV sends
`x-audit-export-total` and, when bounded, `x-audit-export-truncated:
true`. CSV fields are RFC 4180 quoted and spreadsheet-formula
neutralized (a field whose first non-whitespace character is a formula
trigger `=`/`+`/`-`/`@`, or that begins with a tab/CR, is prefixed with
a single quote — covering importers that trim the field before
evaluating it). Both query and export responses carry
`Cache-Control: no-store`: the trail holds actor identities and source
addresses and must never be cached by a shared browser or
intermediary.

## Retention

`[audit] retention_days` defaults to **365**; `sweep_interval_secs`
defaults to **86400** and is floored at 60. The control-plane startup
runs an immediate sweep and then a bounded periodic sweep. Only rows
strictly older than `now - retention_days` are deleted; a row exactly
at the cutoff is retained. Sweep failures are logged and leave the
previous rows in place — retention never blocks startup or requests.

Deletion is **batched**: each statement removes at most 5,000 rows
(oldest first) and the sweep loops to completion, so a large backlog
cannot hold the SQLite write lock in one transaction and turn
successful mutations into 500 `AUDIT_LOG_WRITE_FAILED` responses.

`[audit] max_rows` (default **1,000,000**) is a hard ceiling on table
size: after every age sweep the worker prunes the table down to its
`max_rows` newest rows, oldest first, in the same bounded batches.
Age retention alone cannot bound the table when rows arrive faster
than they age out — an anonymous-denial flood, for example, is always
recorded (denials are evidence) and could otherwise grow the database
without limit. Hitting the cap logs a `warn`: evidence is being
discarded ahead of schedule and the operator should investigate the
flood or raise the cap.

Configuration validation rejects `retention_days = 0`,
`retention_days > 36500` (overflow guard on the millisecond cutoff),
`sweep_interval_secs < 60`, `max_rows < 1000`,
`max_rows > 1000000000000`, `outbox_dispatch_interval_secs < 1`, and
`outbox_retention_days` outside `1..=365`. Outbox retention applies only
to **dispatched** plumbing rows; pending rows are never pruned and the
evidence table's own retention above is unchanged.
