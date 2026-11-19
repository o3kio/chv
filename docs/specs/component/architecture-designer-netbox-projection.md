# Specification: Architecture Designer — NetBox Projection

> **Status:** Implemented (2026-10; status reconciled 2026-11-19) with `docs/design/issue-239-netbox-projection.md` and
> [ADR-023](../adr/023-netbox-projection.md). Issue: kubedoio/chv#239
> (closed by the change that lands this status update).
> Test doubles: [ADR-024](../adr/024-netbox-test-doubles-and-demo-gate.md)
> (issue kubedoio/chv#586).
> Contracts: [`contracts/netbox-mapping-contract.md`](../architecture-designer/contracts/netbox-mapping-contract.md),
> [`contracts/netbox-api-contract.md`](../architecture-designer/contracts/netbox-api-contract.md).

## Responsibility

Project an **applied** CHV architecture topology into a NetBox
inventory/IPAM/DCIM instance as a bounded, downstream, idempotent copy.

- CHV remains authoritative for CHV-managed lifecycle and runtime state.
- NetBox is a downstream projection target only; nothing is read back into
  CHV decision-making.
- Export is explicit (manual) or triggered (post-successful-apply, best
  effort), always idempotent, never destructive by default.

## Component boundaries

```text
chv-webui-bff (handlers/netbox.rs)           ← manual export / dry-run / config / runs
        │ uses
        ▼
chv-netbox-adapter                            ← pure mapping + ownership + plan
  mapping.rs / ownership.rs / plan.rs         (no I/O, deterministic)
  client.rs (reqwest, rustls, HTTPS-only)     ← thin NetBox REST client
  runner.rs                                   ← executes a projection plan
        ▲ used by
chv-controlplane-service
  netbox_projection_worker.rs                 ← claims queued runs, retries
        ▲ enqueued by
apply-resolution hook (post-apply trigger)    ← best-effort, isolated

chv-controlplane-store
  architectures/netbox_config.rs              ← netbox_projection_config
  architectures/netbox_run.rs                 ← netbox_projection_runs
  credential_crypto.rs                        ← token encryption (existing)
```

The adapter **must not** be depended on by `chv-agent`, `chv-stord`,
`chv-nwd`, or any `cellhv-core-*` crate (ADR-016/017).

## Main entities

```text
NetboxProjectionConfig
NetboxProjectionRun
NetboxProjectionPlan          (pure data; persisted inside runs, not its own table)
NetboxProjectionPlanEntry     (create | update | no_op | conflict | stale)
```

## NetboxProjectionConfig

Per-architecture integration configuration.

```text
architecture_id          PK, FK → architecture_topologies
endpoint                 HTTPS base URL (enforced)
token_secret_ref         reference to the encrypted secret holding the API token
token_ciphertext         encrypted token (CredentialEncryption, same path as S3 creds)
retention_policy         "mark_stale" (default) | "delete" (opt-in)
enable_post_apply        bool — enqueue projection when an apply run succeeds
custom_field_prefix      default "chv_" — namespace for ownership custom fields
site_name                optional NetBox site label for projected devices
created_at / updated_at
```

Rules:

- One config row per architecture (upsert semantics).
- Non-HTTPS endpoints are rejected at write time (`NETBOX_HTTPS_REQUIRED`).
- The token is write-only through the API: responses never include it, only
  `token_secret_ref` and a boolean `token_set`.
- Deleting the config does not touch NetBox (no implicit cleanup).

## NetboxProjectionRun

```text
id                      PK (netrun_<short-id>)
architecture_id         FK
architecture_version_id FK — the applied version projected
trigger                 "manual" | "post_apply"
status                  queued | running | succeeded | failed
mode                    "dry_run" | "export"
plan_json               persisted NetboxProjectionPlan (deterministic)
result_json             per-entry outcomes
summary_json            {create, update, no_op, conflict, stale}
error_message           redacted — never contains the token
attempt_count           retries so far
requested_by            user id
started_at / finished_at / created_at
```

Status machine:

```text
queued ──▶ running ──▶ succeeded
   │           │
   │           └────────▶ failed ──▶ (retry) ──▶ queued
   └──────────────────────▲
     (post-apply / manual enqueue)
```

- One **active** (queued or running) run per architecture — a new enqueue
  while one is active either coalesces (post-apply) or is rejected with
  `NETBOX_RUN_ACTIVE` (manual).
- `failed` runs are retryable up to a bounded attempt count with backoff;
  exhausted runs stay `failed` for operator inspection.

## Failure behavior

| Failure | Behavior |
|---|---|
| NetBox unreachable / 5xx / timeout | Run `failed` (retryable); apply result untouched; alert surfaced in run history |
| Auth failure (401/403) | Run `failed` with `NETBOX_AUTH_FAILED`; no retry escalation beyond attempt cap |
| Foreign-object collision | Entry `conflict`; object untouched; run continues other entries |
| Token missing/unreadable | Run `failed` with `NETBOX_TOKEN_MISSING` before any request |
| Config absent | Manual export rejected `NETBOX_NOT_CONFIGURED`; post-apply trigger silently skips |
| No succeeded apply run | Manual export / dry-run rejected `NETBOX_NOT_APPLIED` — the projection source is the most recent `succeeded` apply run's version, never the editable draft |
| Partial failure mid-plan | Executed entries persist in `result_json`; retry resumes via external-id match |
| Worker crash mid-run | Run reclaimed after lease timeout; retry re-enters idempotently |

**Invariant (the issue's core requirement):** a NetBox outage never changes the
success/failure result of an already-completed CHV infrastructure apply. The
apply-resolution path performs, at most, a best-effort enqueue of a projection
run; enqueue failure is logged and swallowed.

## Security, RBAC and secrets

1. NetBox API token is stored encrypted (`CredentialEncryption`), referenced by
   `token_secret_ref`; plaintext exists only in-memory during a run.
2. HTTPS enforced; rustls verification on; CA customization out of scope v1.
3. No CHV secrets are projected into NetBox: `secret_ref`, `password`,
   `token`, SSH private material are never exported (only names/ids/metadata).
4. The token never appears in logs, errors, dry-run output, or API responses.
5. RBAC: projection endpoints are **Operator**-level; architectures with
   `metadata.environment = production` escalate to **Admin** (parity with
   apply/destroy). Object-level ownership follows `require_owner_or_admin`.
6. Audit events: `architecture_netbox_config_updated`,
   `architecture_netbox_dry_run`, `architecture_netbox_export_succeeded`,
   `architecture_netbox_export_failed`, `architecture_netbox_export_retried`
   (carrying architecture id, run id, trigger, summary — never the token).

## Idempotency and ownership

- The six ownership custom fields (see the mapping contract's table for the
  authoritative set): `chv_external_id`, `chv_architecture_id`,
  `chv_managed_by`, `chv_managed_state`, `chv_architecture_version`,
  `chv_mapping_version`.
- External ID: `arch:<architecture_id>:<kind>/<name>:<version>` where `kind`
  is the stable `ResourceType` slug (`server`, `network`, `instance`, …).
- Reconcile: match by `chv_external_id` custom field → then by natural key.
  Objects without `chv_managed_by="chv"` are never written.
- Re-running the same version → all `no_op`. Version bump → `update` of
  changed objects + `create` of new ones. Removed resources → `stale` marks
  (or opt-in delete).
- Partial-failure retry resumes as `update` (no duplicates).

## Testing requirements

- Pure core (mapping/ownership/plan): unit tests with a mock NetBox model —
  idempotency, name collision, foreign-object collision, partial-failure
  re-entry, stale marking, deterministic ordering.
- Runner: integration tests against a bounded mock NetBox HTTP contract
  (wiremock or equivalent), including outage and 401 legs.
- Isolation: test that a failed projection does not alter an apply run's
  terminal status (the issue's headline AC).
- BFF: permission-matrix and ownership tests mirroring the existing
  architecture handler suites.

## Test doubles, demo harness, and qualification (ADR-024)

The projection's test infrastructure has three lanes; each answers a
different question and no lane substitutes for another:

| Lane | Double | Question answered |
|---|---|---|
| PR CI (fast) | adapter unit tests + protocol-level wiremock tests | does the client parse/classify wire shapes correctly? |
| PR CI (composed) | **`chv-netbox-sim`** — first-party stateful simulator | does the projection behave against NetBox-shaped state and semantics? |
| On-demand + weekly | **real NetBox** via docker compose | does it still work against today's NetBox release? |

- **`chv-netbox-sim`** (`crates/chv-netbox-sim`, dev-only): an in-process
  axum server emulating exactly the REST surface defined by the mapping
  contract — pagination envelopes, server-side natural-key and custom-field
  filtering, id/url assignment, Token auth, NetBox-shaped 400/401/404. Test
  control endpoints are `__`-prefixed and obviously not NetBox:
  `POST /__seed`, `GET /__state`, `POST /__reset`, `POST /__faults`
  (forced 401/429/5xx/latency/connection-drop). The bin target is behind
  `required-features = ["bin"]` so release builds never produce it.
- **Responsibility rules**: the simulator implements only the surface the
  client uses (anything else 404s like real NetBox); every behavior traces
  to the mapping contract or a golden fixture; it never appears in a
  production dependency graph and is never the source of truth for the wire
  format.
- **Demo harness** (`make netbox-demo`): boots the simulator plus a
  controlplane (sqlite, converged UI+BFF serving) for interactive use. The
  plain-HTTP client escape hatch is double-gated — the default-off
  `netbox-demo` cargo feature (forwarded to every crate owning a NetBox
  client seam: the worker's factory and the BFF's dry-run client)
  **and** `CHV_NETBOX_ALLOW_HTTP=1` at runtime — and cannot reach a
  shipped binary. Default-feature builds keep the
  fail-closed HTTPS-only client untouched.
- **Qualification** (`deploy/netbox-qualification/`): a pinned docker
  compose runs the **same** composed scenarios against real NetBox
  (`#[ignore]`d tests + env-provided URL/token). Identical scenarios on both
  backends make this run the simulator-drift tripwire; divergences are fixed
  by re-recording fixtures and reconciling the simulator, never by weakening
  a scenario.
- Future NetBox integrations (hosts, disks, autodiscovery) extend these
  lanes (contract kinds → adapter units → simulator endpoints → one composed
  scenario → qualification step) instead of adding new infrastructure.

## Non-goals

- NetBox→CHV reconciliation or any read-back into CHV decisions.
- Generic two-way/Kubernetes-style reconciliation.
- Automatic destructive delete of external inventory objects (default).
- Realtime/streaming sync.
- Projection of identity/config kinds (users, roles, ssh_keys,
  instance_users, backup_targets, backup_policies, projects) — they have no
  NetBox-native home in v1.
