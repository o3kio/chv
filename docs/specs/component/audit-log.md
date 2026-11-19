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
remains open and is tracked under #706.

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
| `occurred_at_ms` | Epoch-millisecond completion timestamp. |
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
per mutating request, including failed and denied attempts.

The insert happens **after** the handler returns. If the insert fails:

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

Residual risk: the mutation has already happened when the insert is
attempted. A 500 `AUDIT_LOG_WRITE_FAILED` therefore does not roll the
mutation back; it tells the caller the audit trail is incomplete.

### Transactional outbox (plumbing landed, not yet adopted)

The control plane ships an `audit_outbox` table (migration `0071`) and a
bounded dispatcher for it, but **no request path enqueues into it yet**
(#706 half 1). The table is created empty, the dispatcher runs once per
second and finds nothing, and every current behavior above is unchanged
byte-for-byte, including the crash window.

The contract the plumbing provides, and what adoption changes:

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
- **Bounded staleness.** Once a path is adopted, its row becomes
  visible to the admin API at most one dispatch interval plus one
  backoff step after commit (default ≈ 2 s), including a replay pass
  immediately after restart. Until adoption, latency is unchanged.
- **Adopted success paths fail closed at commit.** If the enqueue
  itself fails, the mutation transaction fails, so the awkward
  post-hoc 500 for those paths disappears. Denied/failed/unadopted
  paths keep the direct post-hoc insert above.
- **The crash window closes only where mutation and outbox share a
  commit.** Multi-statement mutations (notably `EnrollNode`, whose
  token consumption and node upsert are separate commits) couple only
  the final transaction; a crash between commits remains a
  half-applied mutation with no outbox row. Folding enrollment into one
  transaction is follow-up work.

Dispatched outbox rows are kept for 7 days (`[audit]
outbox_retention_days`, validated 1..=365) and pruned in bounded
batches; pending rows are never pruned. This cleanup is plumbing, not
evidence deletion — `audit_log` retention is separate.

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
