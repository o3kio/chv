# Privileged-mutation audit log (#658)

The control plane keeps one durable, append-only record of every
privileged mutation, written to `audit_log` by two emitters:

1. **HTTP (BFF)** — centralized in middleware wrapped around the whole
   HTTP surface; handlers do not call an audit function.
2. **gRPC (node-facing)** — centralized in the tonic server adapters in
   `crates/chv-controlplane-service/src/server.rs`; handlers do not call
   an audit function. Coverage and its explicit exclusions are below.

The two emitters share the schema, retention, and admin read surface,
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

Recorded: every gRPC mutation on the node-facing control-plane listener
(`chv.controlplane.node.v1.*`). The wrapper is applied to the server
adapters, not the service traits, so in-process BFF calls into
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
- the machine-to-machine telemetry and inventory services (high-rate,
  not privileged mutations).

**Denial boundary (honest limit).** The steady-state mTLS interceptor
rejects a missing/unparseable client certificate and a revoked serial
*before* the handler runs, so those rejections produce the
`chv_revoked_peer_rejected_total` counter and a `warn` log, **not** an
`audit_log` row. Handler-level denials (peer/node mismatch, revoked
peer on `RotateNodeCertificate`) are inside the audited block and are
recorded. Post-revocation attempts against the lifecycle surface are
therefore visible in metrics/logs, not in the durable trail; an
investigator must check both. Missing-certificate denial rows would
require emitting from the interceptor (which lacks the gRPC method
name) or moving revocation into each audited handler; both are
follow-up work, not claims of this emitter.

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
| `action` | Stable action. HTTP: dotted, derived from method/path, e.g. `vm.create`. gRPC: the protobuf RPC name, e.g. `StartVm`. |
| `method`, `path` | Request shape; never the query string. HTTP: verb + matched route. gRPC: `gRPC` + `<fully-qualified service>/<RpcName>`. |
| `resource_kind`, `resource_id` | Best-effort resource. HTTP: path-derived; bodies are intentionally not read. gRPC: request fields (capped), with `EnrollNode` using the issued node id on success. |
| `outcome` | `success` (2xx/3xx), `denied` (401/403), `failed` (other 4xx/5xx). gRPC status codes are mapped to the HTTP equivalents (200/400/401/403/404/409/412/429/499/500/501/503/504). |
| `status_code` | Final HTTP status, or the gRPC code's HTTP equivalent. |
| `correlation_id` | Correlation id assigned by the correlation middleware (HTTP), capped at 128 chars. gRPC rows leave it NULL; `operation_id` is the join key for agent traffic. |
| `operation_id` | Inbound `x-operation-id` when supplied (HTTP or gRPC metadata), capped at 128 chars. |
| `source_ip` | Transport peer address (see below), or a validated forwarding hop. On gRPC this is always the direct TCP peer, never a metadata header. |
| `metadata` | Allowlisted redacted JSON object. HTTP: `auth_source`. gRPC: `transport: "grpc"` and `auth_source` ∈ `mtls` \| `insecure` \| `bootstrap_token`. |

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
There is no retry queue and no transactional coupling to handler
transactions, so a process crash in the narrow window after the
handler commits but before the audit insert commits yields an
unrecorded mutation. Closing that window would require every mutation
to share a transaction with its audit insert, which the current
handler/store boundary does not provide.

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
certificate rejections are outside the handler and therefore have no
row (see the denial boundary above).

## Redaction contract

The audit row never stores:

- `Authorization` or `Cookie` headers, or any token/JWT/API token;
- passwords or password hashes;
- S3 access keys, secret keys, or session tokens;
- request bodies or query strings;
- arbitrary handler payloads.

`metadata` is constructed from an explicit allowlist — `auth_source`
on the HTTP emitter (`crates/chv-webui-bff/src/audit.rs`) and
`transport` + `auth_source` on the gRPC emitter
(`crates/chv-controlplane-service/src/server.rs`). A future field must
be added deliberately to the relevant allowlist and must be safe to
retain for the full retention period. Neither repository path exposes
free-form metadata from request data. The gRPC emitter also caps every
request-derived string before insert and never records a bootstrap
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
`sweep_interval_secs < 60`, `max_rows < 1000`, and
`max_rows > 1000000000000`.
