# Privileged-mutation audit log (#658)

The BFF keeps a durable, append-only record of every privileged
mutation. Emission is centralized in middleware wrapped around the
`/v1` BFF surface; handlers do not call an audit function.

## Scope

Recorded: every non-safe HTTP method under `/v1` that is not an
explicitly read-shaped POST. This includes operator/admin mutations,
the hard-fenced backup routes (which currently answer 501), and
unknown future non-safe `/v1` paths (the classifier fails closed).
Authentication routes under `/v1/auth/*` are excluded because they are
self-service credential operations and their bodies contain secrets.

Reads are never recorded, including filter POSTs such as
`POST /v1/vms`, `/v1/tasks`, `/v1/audit-log`, and
`/v1/audit-log/export`.

## Row contract

`audit_log` (control-plane SQLite database, migration `0064`) stores:

| Column | Meaning |
|---|---|
| `audit_id` | Server-minted id (primary key). |
| `occurred_at_ms` | Epoch-millisecond completion timestamp. |
| `actor_id`, `actor_username`, `actor_role` | Verified claims, or the explicit anonymous marker. |
| `action` | Stable dotted action derived from method/path, e.g. `vm.create`. |
| `method`, `path` | Request shape; never the query string. |
| `resource_kind`, `resource_id` | Best-effort path-derived resource. Body ids are intentionally not read. |
| `outcome` | `success` (2xx/3xx), `denied` (401/403), `failed` (other 4xx/5xx). |
| `status_code` | Final HTTP status. |
| `correlation_id` | Correlation id assigned by the correlation middleware. |
| `operation_id` | Inbound `x-operation-id` when supplied. |
| `source_ip` | Transport peer address, or a validated forwarding hop. |
| `metadata` | Allowlisted redacted JSON object; currently only `auth_source`. |

There is no foreign key to `users`: an audit row must outlive the
account it names, otherwise deleting a user would erase evidence of
that user's privileged actions.

## Exactly-once semantics

A request is classified once, executed once, and then the middleware
attempts one insert. Under a healthy database this is exactly one row
per mutating request, including failed and denied attempts.

The insert happens **after** the handler returns. If the insert fails:

- the failure is logged loudly at `error` level with target `audit`
  and the action/path/status;
- if the mutation response was 2xx/3xx, the BFF replaces it with HTTP
  500 `AUDIT_LOG_WRITE_FAILED` rather than claiming success without a
  trail;
- if the response was already a 4xx/5xx (including 401/403), the
  original response is preserved and the missing row is logged.

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

## Redaction contract

The audit row never stores:

- `Authorization` or `Cookie` headers, or any token/JWT/API token;
- passwords or password hashes;
- S3 access keys, secret keys, or session tokens;
- request bodies or query strings;
- arbitrary handler payloads.

`metadata` is constructed from an explicit allowlist in
`crates/chv-webui-bff/src/audit.rs` (auth mechanism only). A future
field must be added deliberately to that allowlist and must be safe to
retain for the full retention period. The repository exposes no
free-form metadata path from request data.

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
neutralized.

## Retention

`[audit] retention_days` defaults to **365**; `sweep_interval_secs`
defaults to **86400** and is floored at 60. The control-plane startup
runs an immediate sweep and then a bounded periodic sweep. Only rows
strictly older than `now - retention_days` are deleted; a row exactly
at the cutoff is retained. Sweep failures are logged and leave the
previous rows in place — retention never blocks startup or requests.

Configuration validation rejects `retention_days = 0` and
`sweep_interval_secs < 60`.
