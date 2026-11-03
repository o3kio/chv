# Runbook: BFF API Errors (`BffApiErrorRate`)

**Scenario:** The `BffApiErrorRate` warning has fired — more than 1% of HTTP
requests to the BFF have returned 5xx for 5 minutes
([`docs/OBSERVABILITY.md`](../OBSERVABILITY.md#sli-5-bff-api-success-rate)).
Use it for any user-reported BFF error too, even when the alert has not fired:
a 4xx storm can break the UI without moving the 5xx ratio.

**Severity:** **SEV-2** for a sustained 5xx rate above 1%, a broken login/UI,
or a single error class blocking operators. **SEV-1** if the control plane or
its database is down for all users, or if the failure risks data loss.
A bounded 4xx error (bad request, stale plan, CSRF) is normally **SEV-3**.

**Impact:** operators cannot log in or complete the affected UI/API action.
A 5xx can also indicate a control-plane or database dependency failure; VM
lifecycle on the nodes is not directly stopped by a BFF outage.

**Automation level:** The alert is automatic. Error responses carry a
correlation ID end-to-end; there is no automatic BFF remediation.

## What the BFF is

- `chv-webui-bff` is **embedded in `chv-controlplane`**, not a separate
  daemon. The packaged/`install.sh` HTTP listener is `127.0.0.1:8080` behind
  nginx (`quick-install.sh` binds `0.0.0.0:8080`, plain HTTP, and must be
  firewalled).
- Browser traffic uses `/v1/*` and the `chv_session` HttpOnly cookie
  (`SameSite=Strict`, `Path=/`, `Max-Age=86400`; `Secure` only when
  `CHV_COOKIE_SECURE` is set). `chvctl` and other non-browser clients use the
  API-only `POST /api/v1/auth/login` bearer endpoint.
- The BFF records `http_requests_total{method,status}` and
  `http_request_duration_seconds{method,path}` in the control-plane process
  registry. Scrape the control-plane metrics listener (`metrics_bind`,
  default `127.0.0.1:9103`), not a separate BFF port.
- The remainder of `/api/v1/*` is the legacy control-plane surface and is not
  the supported BFF `/v1` contract. See
  [BFF HTTP API Versioning](../specs/ops/bff-http-api-versioning.md).

## Correlation contract

- Inbound `x-operation-id` becomes the correlation ID if present; otherwise
  the BFF generates a short ID.
- Every response carries `x-correlation-id`.
- Every JSON error body carries a top-level `request_id` equal to that ID.
  `code` and `message` are also top-level on BFF errors.
- The control plane's unmatched-route fallback is the exception: it nests
  under `error` — `{"error":{"code":"NOT_IMPLEMENTED","message":"...",
  "retryable":false,"hint":"..."}}` — and is a 404, not a BFF handler error.
- Every privileged mutation is also written to the `audit_log` table with
  `correlation_id`, `operation_id`, `status_code`, and `outcome`.

Use `request_id` in tickets and searches. Never paste session cookies or
bearer tokens.

## 1. Confirm the alert and scope

```promql
sum by (status) (rate(http_requests_total{status=~"5.."}[5m]))
/
sum(rate(http_requests_total[5m]))
```

```promql
sum by (status, method) (rate(http_requests_total{status=~"5.."}[5m]))
```

Check whether the failures are one status (e.g. all 503), one method, or one
path. `http_request_duration_seconds` separates timeouts from fast errors.

## 2. Check the control plane and the edge

```bash
systemctl status chv-controlplane nginx --no-pager
journalctl -u chv-controlplane -n 200 --no-pager
journalctl -u nginx -n 50 --no-pager
curl -sS -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8080/health
curl -sS http://127.0.0.1:8080/ready
curl -sS http://127.0.0.1:8080/health/deep | jq .
```

- A **502 from nginx** means nginx could not reach the control plane. Start
  or fix `chv-controlplane`; this is not a BFF handler bug.
- `/health/deep` distinguishes `database` from `agent_socket_dir` and
  `agent_connectivity`. A failing `database` is the usual cause of 503
  `AUTH_UNAVAILABLE` and 500s.
- `systemctl status` also shows a crash-loop: check the journal for the
  fail-loud startup error rather than restarting repeatedly.

## 3. Correlate one failing request

From the response header or body, take `x-correlation-id` / `request_id`. The
BFF does not put that id on every log event — it writes it to the durable
audit row. Start with the audit table; the returned `operation_id` is the log
join key:

```bash
sqlite3 "file:/var/lib/chv/controlplane.db?mode=ro" \
  "SELECT occurred_at_ms, status_code, method, path, outcome, operation_id
   FROM audit_log WHERE correlation_id='<ID>'
   ORDER BY occurred_at_ms DESC LIMIT 20;"
```

Then filter structured logs by that operation id (`-o cat` prints the raw
line; structured fields need `CHV_LOG_FORMAT=json`):

```bash
journalctl -u chv-controlplane -o cat --since '-30 min' \
  | jq 'select(.span.operation_id=="<OP_ID>")'
```

The audit row tells you the route, the acting user/role, and whether the
request was denied or failed; the journal tells you the server-side error.

## 4. Read the status/code pair

| Status | Code(s) | Meaning and action |
|---|---|---|
| 400 | `BAD_REQUEST`, `MISSING_CONFIRMATION`, `WARNINGS_NOT_ACKNOWLEDGED`, `PLAN_MODE_MISMATCH`, `INVALID_RESOURCE_NAME`, monitoring `INVALID_RANGE`/`QUERY_TOO_LARGE`/`UNKNOWN_METRIC`/`UNSUPPORTED_SOURCE`/`INVALID_TARGET_KIND` | Client/request-shape error. Fix the request; not a server incident. |
| 401 | `UNAUTHORIZED` | Missing/expired cookie or bearer token. Re-authenticate. |
| 401 | `SESSION_REVOKED` | The token's `token_version` is stale or the user row is gone. Log in again. |
| 403 | `FORBIDDEN` | Authenticated but not allowed. Check the role and VM ownership. |
| 403 | `PASSWORD_CHANGE_REQUIRED` | The user must change the password before other routes work. |
| 403 | `INVALID_TOKEN_SCOPE` | The token was minted for a different purpose. Use the browser or API login as appropriate. |
| 403 | `PRODUCTION_REQUIRES_ADMIN` | Applying to a production environment requires the Admin role. |
| 403 / 415 | `CSRF_REJECTED` | Non-GET request was not `application/json`, or multipart lacked a non-empty `x-csrf-token`. See §5. |
| 404 | `NOT_FOUND` | Resource does not exist. |
| 404 | nested `error.code: NOT_IMPLEMENTED` | Unmatched API path hit the control-plane fallback. Check the route and `/v1` prefix. |
| 409 | `CONFLICT`, `PLAN_EXPIRED`, `PLAN_NOT_DISCARDABLE`, `PLAN_NOT_APPLICABLE`, `NETBOX_RUN_ACTIVE`, `PROJECTION_RUN_NOT_RETRYABLE` | State moved on. Re-read the resource/plan and retry with fresh state. |
| 422 | `GRAPH_EMPTY`, `QUOTA_EXCEEDED` | The request is well-formed but cannot be applied yet. |
| 429 | `RATE_LIMITED` ("too many login attempts, try again later") | Login is limited to 10 attempts per username per 60 s. Wait, or investigate a credential-stuffing attempt. |
| 500 | `INTERNAL_ERROR` | Server bug or unexpected dependency failure. Correlate the `request_id` and inspect the journal. |
| 501 | `NOT_IMPLEMENTED` | Deliberately fenced feature (for example backup). Expected; do not chase it as an outage. |
| 502 | `DRIFT_CHECK_FAILED`, `NETBOX_UNREACHABLE`, `NETBOX_AUTH_FAILED` | An upstream integration failed. Check NetBox reachability/credentials. |
| 503 | `AUTH_UNAVAILABLE` | The per-request revocation lookup failed (database outage). Fail-closed by design. Fix the database. |
| 503 | `MONITORING_UNAVAILABLE` | The native monitoring subsystem is degraded/disabled. It does **not** mean nodes or VMs are unhealthy. |

`RATE_LIMITED` / `TooManyRequests` is emitted only by the login rate limiters
(BFF `/v1/auth/login` and legacy `/api/v1/auth/login`), both 10 attempts per
username per 60 s.

## 5. CSRF and proxy checks

Every non-GET request must have an un-forgeable shape:

- `Content-Type: application/json` is accepted; or
- `multipart/form-data` with a non-empty `x-csrf-token` header.

Anything else is rejected with **415**; multipart without the header is
**403 `CSRF_REJECTED`**. `GET`/`OPTIONS` are exempt. There is no token
issuance endpoint and no shared CSRF secret — do not go looking for one.

If a browser that normally works starts getting `CSRF_REJECTED`:

1. Confirm nginx is not stripping `Content-Type` or `x-csrf-token`.
2. Confirm the UI is calling the same origin (CORS is disabled unless
   `CHV_ALLOWED_ORIGIN` is set).
3. Confirm the request method and body encoding match the route (JSON vs
   multipart import).

Do **not** disable the middleware or add a permissive CORS origin to work
around a client bug.

## 6. Database, disk, and monitoring

```bash
df -h /var/lib/chv /run
sqlite3 "file:/var/lib/chv/controlplane.db?mode=ro" 'PRAGMA quick_check;'
journalctl -u chv-controlplane -n 500 --no-pager | grep -iE 'sqlite|database|disk|locked|corrupt'
```

- Database corruption or a full disk produces 500s and `AUTH_UNAVAILABLE`;
  follow [Control Plane DR](control-plane-dr.md) rather than deleting the
  database.
- `MONITORING_UNAVAILABLE` is scoped to monitoring queries; check the
  monitoring store and the monitoring runbooks
  ([Monitoring Store](monitoring-store.md),
  [Monitoring Export](monitoring-export.md)).

## Remediation

1. **Control plane down / nginx 502:** start or fix `chv-controlplane`.
2. **Database unavailable/locked:** free disk, resolve the lock holder, or
   restore per [Control Plane DR](control-plane-dr.md). Do not delete
   `controlplane.db`.
3. **Auth 401/403:** re-authenticate; for `SESSION_REVOKED`, log in again.
   For `PASSWORD_CHANGE_REQUIRED`, complete the password change.
4. **429:** wait out the 60 s window. If attempts are not from your users,
   treat it as a credential-stuffing signal and escalate to security.
5. **CSRF 403/415:** fix the client/proxy request shape, not the server.
6. **501:** use the supported route; the feature is fenced by design.
7. **502 integration codes:** restore NetBox connectivity/credentials and
   retry the operation.
8. **Monitoring 503:** follow the monitoring runbooks; VM lifecycle is
   unaffected.

## Escalation

- **SEV-1 / page:** control-plane or database outage for all users, data-loss
  risk, or a security signal (token replay, credential stuffing, unexpected
  401/403 spike).
- **SEV-2:** sustained 5xx above 1%, login broken, or a dependency failure
  that survives one restart.
- **SEV-3:** a bounded 4xx class or a single user's request-shape error.
- Security incidents follow [`SECURITY.md`](../../SECURITY.md); do not open a
  public issue for a suspected vulnerability.

## What not to do

- **Do not disable authentication, CSRF protection, or CORS** to clear a
  401/403/415.
- **Do not delete `controlplane.db` or `audit_log` rows** to "reset" errors.
- **Do not restart `chv-agent` for a BFF error.** It is a different process;
  restarting it during a Core operation can strand the operation as
  InspectRequired.
- **Do not treat the control plane's nested `NOT_IMPLEMENTED` 404 as a BFF
  bug**; it usually means the path never matched a route.
- **Do not chase a 501** as if it were an outage; fenced features return it
  deterministically.
- **Do not paste cookies, bearer tokens, or full request bodies** into
  tickets or chat. Use `request_id`.
- **Do not raise the 1% alert threshold** before the root cause is understood.

## Related

- [VM Operation Failures](vm-op-failures.md)
- [VM Start Latency](vm-start-slow.md)
- [Control Plane DR](control-plane-dr.md)
- [Monitoring Store](monitoring-store.md)
- [`docs/OPERATIONS.md`](../OPERATIONS.md#web-ui-blank-or-api-errors)
- [BFF HTTP API Versioning](../specs/ops/bff-http-api-versioning.md)
