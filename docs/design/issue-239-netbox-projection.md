# #239 design — Architecture Designer → NetBox inventory projection

**Issue:** kubedoio/chv#239 — project an applied CHV architecture topology into a
NetBox inventory/IPAM/DCIM as a **bounded, downstream projection** (not a second
control-plane authority). See the issue's "Design direction",
"Required behavior", "Acceptance criteria", and "Non-goals" — all reproduced and
resolved to decision points (DPs) below.

**Status:** **IMPLEMENTED** (2026-10-09; status reconciled 2026-11-19) — the design below was adopted and the
8-PR stack merged. Shipped evidence: `crates/chv-netbox-adapter/` (mapping,
plan, runner, client, ownership), migration `0059_netbox_projection.sql`, the
post-apply worker (`crates/chv-controlplane-service/src/netbox_projection_worker.rs`),
the BFF `/v1/architectures/netbox/*` routes, the UI `NetboxPanel`, and the
end-to-end evidence in `docs/evidence/netbox-projection-e2e.md`. Issue #239 is
closed; this document remains the design of record. The full documentation set
for #239 (design → ADR → spec → contracts → plan) is:

| Document | Path | Status |
|---|---|---|
| Design (this file) | `docs/design/issue-239-netbox-projection.md` | Implemented |
| ADR | [`docs/specs/adr/023-netbox-projection.md`](../specs/adr/023-netbox-projection.md) | Accepted |
| Component spec | [`docs/specs/component/architecture-designer-netbox-projection.md`](../specs/component/architecture-designer-netbox-projection.md) | Implemented |
| Mapping contract | [`docs/specs/architecture-designer/contracts/netbox-mapping-contract.md`](../specs/architecture-designer/contracts/netbox-mapping-contract.md) | Implemented |
| API contract | [`docs/specs/architecture-designer/contracts/netbox-api-contract.md`](../specs/architecture-designer/contracts/netbox-api-contract.md) | Implemented |
| Implementation plan | [`docs/plans/2026-10-08-netbox-projection-implementation-plan.md`](../plans/2026-10-08-netbox-projection-implementation-plan.md) | Executed |

The DP recommendations below are the design of record for those documents;
the plan's PR sequence is the execution path.

**Census basis:** current designer pipeline inspected at the Architecture
Designer Phase 0–7 surface (`docs/specs/architecture-designer/`), the
`chv-architecture-validate` / `chv-architecture-reconcile` crates, the BFF
architecture handlers, the control-plane store repositories, and the
orchestrator. Line references are to `main` at the time of writing and may drift;
every reference was verified once.

---

## 1. Current implementation — what exists today

### 1.1 The designer pipeline (desired → validate → plan → apply → drift)

The Architecture Designer is a WebUI feature shipped in Phases 0–7
(`docs/specs/architecture-designer/README.md`). It is intentionally narrower
than a TOSCA engine (ADR-006) and keeps a **hard separation between desired
topology and live fleet state** (ADR-005). The pipeline today:

```text
CHVArchitecture YAML (desired)
   │  chv.kubedo.io/v1alpha1; 13 resource kinds
   ▼
validate        chv-architecture-validate: parse + static checks
   ▼             + fleet checks against live InventorySnapshot
plan            chv-architecture-reconcile::plan: diff(desired, snapshot)
   ▼             → ordered PlanChange[] (create/update/delete/replace/no_op)
   ▼             (+ destroy mode)
apply           chv-architecture-reconcile::apply: enqueue per-change
   ▼             Operations, run/plan CAS guards, idempotent crash-resume
orchestrator    chv-controlplane-service::orchestrator: dispatch operations
   ▼             to agents (nwd/stord/agent-runtime-ch)
drift           chv-architecture-reconcile::drift: compare baseline vs live
                 snapshot → DriftReport / DriftFinding[]
```

### 1.2 The authoritative source data model

The desired-state contract is the `CHVArchitecture` YAML document modelled
strongly-typed in `crates/chv-architecture-validate/src/model.rs`, with
**13 resource kinds**:

| Kind | Model struct | Notes |
|---|---|---|
| `servers` | `Server` | `management_ip`, `role`, `labels`, `resources{cpu_cores,memory_gb}`, `networks.interfaces[]` |
| `networks` | `Network` | `type{bridge,vlan,nat,isolated,routed}`, `bridge`, `vlan_id`, `cidr`, `gateway`, `dns`, `dhcp` |
| `datastores` | `Datastore` | `type`, `path`, `pool`, `capabilities`, `secret_ref` |
| `images` / `templates` | `Image` / `Template` | image→datastore; template→image+cpu/mem/disk+network |
| `instances` | `Instance` | `placement.server`, `resources`, `disks[]`, `networks[]{name,ip}`, `cloud_init`, `backup`, `tags` |
| `ssh_keys` / `instance_users` / `backup_targets` / `backup_policies` / `roles` / `users` / `projects` | … | config/identity kinds |

Container: `CHVArchitecture{ apiVersion, kind, metadata{name,display_name,
description,environment,owner,labels}, <13 kind vectors> }`.

### 1.3 Inventory / live-state surface

`InventorySnapshot` (`crates/chv-architecture-validate/src/fleet/inventory.rs`)
carries **live** `nodes`, `networks`, `datastores`, `images`, `backup_targets`,
`secrets`, plus completeness flags. Captured via the `InventoryProvider` trait;
the SQLite-backed `FleetInventoryProvider`
(`crates/chv-architecture-reconcile/src/fleet_inventory.rs`) is the production
implementation. Drift and fleet-check both consume this snapshot.

### 1.4 Persistence (control-plane store)

`crates/chv-controlplane-store/src/architectures/` exposes
`TopologyRepository`, `VersionRepository`, `PlanRepository`, `ApplyRunRepository`,
`DriftReportRepository`, `InventorySnapshotRepository` over SQLite
(`architecture_topologies`, `architecture_versions`, `architecture_plans`,
`architecture_apply_runs`, `architecture_drift_reports`,
`inventory_snapshots`, …). Persistence rule (component data-model spec): always
keep original YAML, normalized model JSON, graph JSON, validation/plan/apply/drift
results.

### 1.5 BFF API and routing

`crates/chv-webui-bff/src/handlers/architectures.rs` + `router.rs` expose the
POST-only verb surface `/v1/architectures/*` (list/get/validate/validate-yaml/
create/update/archive/check-fleet/generate-yaml/import-yaml/plan/destroy-plan/
discard-plan/apply/destroy/runs/drift). Ownership is object-scoped
(`require_owner_or_admin`, admin sees all; non-admin sees own + system rows,
starters read-only), and production environments escalate apply/destroy to Admin
(match the `metadata.environment = production` signal).

### 1.6 Security posture for the designer

`docs/specs/component/architecture-designer-security.md` establishes: no raw
secrets in YAML, `secret_ref` indirection, MVP uses the encrypted secret table
(`credential_crypto.rs`), apply escalation, and a recommended audit-event list
(which includes `architecture_exported`).

---

## 2. Gap analysis — why #239 does not fall out of the existing pipeline

The pipeline is **closed-loop and CHV-internal**. Nothing today can emit state to
an external inventory, and several pieces #239 requires simply do not exist.
Gaps are numbered and each is resolved by a DP in §4+.

**G1 — No outbound integration layer / adapter concept.** Every consumer of the
designer lives inside the control plane (BFF + SQLite + orchestrator). There is
no "downstream projection" abstraction, no outbound HTTP client for a
management integration, no "integration config" model, and no notion of a
non-CHV sink to keep in sync. The repo's only CI/adjacent integration patterns
are inbound (fleet inventory from SQLite) or protocol-level (agent/stord/nwd
gRPC). *→ DP1 (adapter crate + placement), DP2 (netbox client).*

**G2 — The inventory snapshot cannot drive a VM/address projection.**
`InventorySnapshot` carries nodes/networks/datastores/images/backup_targets, but
**not instances, templates, users, or per-VM interface/IP addresses**. The
plan diff itself documents this ("snapshot does not yet track instances;
emit unconditionally", `plan/diff.rs`). For a NetBox projection that must cover
"VMs, VM interfaces, addresses", the snapshot is insufficient as the sole
source. *→ DP3 (projection source of truth: the applied topology version,
enriched by the live snapshot for hosts).*

**G3 — No stable external-ID / ownership contract.** CHV resources are keyed by
in-DB ids and `kind/name`. There is no mechanism to (a) remember which NetBox
object a given CHV resource last projected into (idempotency), or (b) prove a
NetBox object is owned by CHV so we never clobber a pre-existing non-CHV object
(ownership-conflict detection). *→ DP4 (external-ID + custom-field ownership
contract), DP5 (idempotent reconcile via custom-field lookup).*

**G4 — No post-apply hook / projection trigger.** `apply_plan` terminates at
enqueueing Operations and a plan/run status; the architecture run's terminal
transition and the actual application of resources happen in the orchestrator /
apply resolution path. There is no hook "after successful apply, project". And
the issue requires a **manual** export too, plus a **dry-run/report** mode.
There is no asynchronous outbound job infrastructure for integrations (the
closest is `BackupWorker`, which is inbound scheduling, not an integration
projection). *→ DP6 (explicit trigger model), DP7 (projection runner + dry-run),
DP8 (failure/retry isolation from apply).*

**G5 — No outbound secrets/config for an external sink.** NetBox requires an
endpoint + API token over HTTPS. The existing `credential_crypto` infra
encrypts S3 backup credentials; there is no reusable "external integration
connection" config (endpoint, token secret, custom-field schema, retention
policy). *→ DP9 (projection config + encrypted token).*

**G6 — No mapping contract, no NetBox API model.** There is no versioned
CHV→NetBox mapping (which CHV kinds map to which NetBox DCIM/IPAM object types,
what custom fields carry the ownership/external-ID metadata). *→ §6 (mapping
contract), §7 (custom-field contract).*

**G7 — No audit/event record for projection.** The recommended audit list names
`architecture_exported` but nothing emits it, and there is no projection-run
table to persist attempt/result/retry state (the issue requires it). *→ DP10
(projection-run persistence + audit events).*

**G8 — No RBAC/permission surface for projection.** BFF uses Viewer/Operator/
Admin and object-scoped ownership. Projection is an operator action with an
external-data-write side effect and credential access; it needs a clear
permission mapping and production-escalation parity. *→ DP11.*

**G9 — No UI surface.** There is no place to configure NetBox, run export /
dry-run, or view a projection report. *→ §11.*

**G10 — Retention / no-delete policy absent.** The issue mandates "do not delete
by default; removed CHV resources marked stale per an explicit retention
policy" — no such concept exists anywhere. *→ DP12 (retention policy).*

---

## 3. Goals, non-goals, and scope

### Goals (v1)

1. A **projection**, not a reconciler: CHV stays authoritative for lifecycle and
   runtime; NetBox is a downstream inventory/IPAM/DCIM projection.
2. Project **hosts, VMs, VM interfaces, networks/prefixes/VLANs, IP addresses,
   and topology metadata** into NetBox per a versioned mapping contract.
3. **Idempotent, repeatable** export (manual and post-successful-apply) that
   preserves stable CHV external IDs so repeated exports reconcile, not duplicate.
4. **Ownership-conflict safety**: never silently take over a NetBox object CHV
   did not create.
5. **Dry-run / report** showing create/update/no-op/conflict/stale, with no
   secrets, deterministic output.
6. **Isolated failures**: a NetBox outage never rolls back or blocks a
   successful CHV apply; projection is separately visible and retryable.
7. **No destructive deletes by default**; removed CHV resources are marked
   stale per an explicit retention policy.
8. Repo-conventional design docs + ADR, a decomposition, tests on a bounded
   mock NetBox API contract.

### Non-goals (v1, fixed)

- No NetBox→CHV reconciliation / no bidirectional sync / NetBox is not the
  desired-state authority (issue + §6).
- No generic two-way/Kubernetes-style controllers.
- No automatic destructive cleanup of external inventory objects.
- No discovery/adoption of arbitrary pre-existing NetBox objects (only
  owned-by-CHV objects are managed; others are reported as conflicts).
- No projection of identity/config kinds (`users`, `roles`, `ssh_keys`,
  `instance_users`, `backup_targets`, `backup_policies`, `projects`) into
  NetBox in v1 — they have no NetBox-native home and the issue's mapping list
  does not demand them; they ride topology metadata only.
- No realtime/streaming sync; projection is on-trigger (manual + post-apply).

---

## 4. Architecture — proposed

### 4.1 Placement (resolves G1)

Add a new workspace crate in the **management / control-plane integration
layer**, physically outside any Core runtime:

```text
crates/chv-netbox-adapter/
  src/
    lib.rs           # re-exports; module wiring
    config.rs        # NetboxProjectionConfig (per-architecture integration)
    client.rs        # thin NetBox REST client (reqwest), token auth, TLS
    mapping.rs       # CHV model → NetBox object builders (pure) + MAPPING_VERSION
    ownership.rs     # custom-field key names + external_id derivation (pure)
    plan.rs          # compute NetBox diff (create/update/no-op/conflict/stale)
    runner.rs        # async execute a projection plan against NetBox
```

Dependencies (read-only): `chv_architecture_validate::model` (CHVArchitecture),
`chv_architecture_validate::fleet::InventorySnapshot` (host facts),
`chv_controlplane_types::architecture` (ids), `chv-errors`, `tracing`,
`reqwest`+`rustls` (HTTPS-only), `serde`/`serde_json`.
It does **not** depend on the agent/stord/nwd crates.

**Decision:** the adapter is a library + a runner invoked by the BFF (manual)
and by a control-plane worker hook (post-apply). It is **not** part of
`chv-controlplane-service`'s core runtime path; it sits behind the same
authorization boundary as the other architecture handlers. *→ DP1.*

### 4.2 Projection source of truth (resolves G2)

Project from the **applied architecture version** (the `architecture_versions`
row — `normalized_model_json` / `yaml_content` — identified by the
`architecture_version_id` of the most recent `succeeded` apply run) as the
authoritative CHV shape, **enriched** for hosts by the live
`InventorySnapshot` (node CPU/mem, datastore capacity/kind, network vlan/cidr
facts when present). The mutable `architecture_topologies.latest_yaml` (the
editable draft) is **never** the projection source — after a failed or
in-flight apply on a newer version it diverges from what was actually applied.
Rationale:

- The applied version already carries everything the issue's mapping needs:
  hosts, VMs, VM interfaces + IPs, networks + cidr/vlan, topology metadata
  (environment, owner, labels), and it is immutable and auditable (persistence
  rule).
- `InventorySnapshot` today lacks instances/VM interfaces/addresses (G2);
  extending the snapshot to carry full VM inventory is a larger, separate change
  and is deferred (DP3 follow-on). For v1 we project the *applied intent* for
  VM interfaces/IPs and the *live* host/datastore facts.

**Sizing note / follow-up:** if "addresses must reflect observed runtime
addresses" (DHCP-assigned, post-boot) becomes a requirement, that needs VM
inventory in the snapshot (extend `InventorySnapshot` + `InventoryProvider` +
`FleetInventoryProvider`) — recorded as a distinct follow-up, not v1. *→ DP3.*

### 4.3 Reconcile semantics (resolves G3)

Every projected NetBox object carries CHV ownership **custom fields**:

| Custom field name | Value | Purpose |
|---|---|---|
| `chv_external_id` | `arch:<arch_id>:<kind>/<name>:<version>` | Stable external-ID key, idempotency match |
| `chv_architecture_id` | `<arch_id>` | Group/query all objects of one topology |
| `chv_managed_by` | `chv` | Ownership marker |
| `chv_managed_state` | `active` \| `stale` | Retention marker (see DP12) |
| `chv_architecture_version` | `<version>` | Applied version provenance |
| `chv_mapping_version` | `v1` | Mapping contract version (see the mapping contract) |

Reconcile algorithm per object kind (pure `mapping`/`ownership`, then `plan`):

1. Look up NetBox objects by `chv_external_id` (custom-field exact match).
   - found **and** `chv_managed_by == chv` → governed by CHV → *update or no-op*.
   - found **and** not owned by CHV → **conflict** (report; never touch).
2. not found by external id → look up by natural key (e.g. device name, VM name,
   prefix `prefix`/`vrf`, IP `address`/`vrf`):
   - free → *create* with `chv_external_id` + ownership CFs.
   - occupied by a non-CHV object → **conflict** (report; never take over).
   - occupied by a chv-owned object of this architecture (create-after-partial-
     failure with the same external id, or a version bump with a changed one)
     → *update* (idempotent re-entry).
3. Unmapped surface: no delete by default; see DP12 retention.

**Decision:** ownership is expressed **only** through custom fields we create;
we never delete or modify a NetBox object whose `chv_managed_by` is absent or
not `chv`. This is the "never silently take over" guarantee. *→ DP4, DP5.*

### 4.4 Trigger model (resolves G4)

Two triggers, both funnel into the same `NetboxProjectionRunner`:

- **Manual**: `POST /v1/architectures/netbox/export` (dry-run flag + force).
- **Post-successful-apply**: enqueued at the point the architecture apply run
  transitions to `Succeeded` (status string `succeeded` — the durable
  apply-run status is lowercase snake_case). This is a **best-effort enqueue** — it never
  changes the apply result (DP8). Implemented as a hook in the apply-resolution
  path that inserts a `queued` projection run if the architecture has a NetBox
  projection config enabled.

> **Implementation note (realized shape of the post-apply trigger).** The
> apply-run terminal transition site named above does not exist yet: the
> orchestrator owns the terminal `Succeeded` / `PartiallyFailed` / `Failed`
> transitions, and the apply module's own doc says *"The orchestrator (out of
> scope for Phase 5) is responsible for the terminal `Succeeded` /
> `PartiallyFailed` / `Failed` transitions; this module only puts the run on
> the rails."* Nothing in the codebase writes `succeeded` to
> `architecture_apply_runs`. The trigger is therefore implemented as a
> **worker-side sweep** in `NetboxProjectionWorker::tick`: each tick lists
> architectures with `enable_post_apply = true`, takes each one's most recent
> `succeeded` apply run, and enqueues a `post_apply` export run for its
> version — unless a `post_apply` run of any status already exists for that
> (architecture, version) pair (idempotent across ticks; a permanently-failed
> post-apply run is retried by the operator, transient ones by the bounded
> auto-requeue) or an active run holds the architecture's one-active slot
> (coalescing, enforced by the partial unique index). This shape is
> **strictly more isolated** than an in-line hook — the apply path calls
> nothing, so a NetBox outage or a projection-store failure is structurally
> incapable of changing an apply result — and **behaviorally equivalent**
> once the orchestrator's terminal transitions land: the sweep fires on the
> very next tick with zero changes.

Projection runs are executed by a small control-plane worker (analogous to the
`BackupWorker` pattern: `NetboxProjectionWorker`, claim pending runs atomically,
advance through `queued/running/succeeded/failed`, retry with backoff). Runs are
**serialized per architecture** (a simple per-architecture mutex key / a
"one active run per architecture" claim) to keep idempotent ordering.
*→ DP6, DP7, DP8, DP10.*

---

## 5. Decision points (with recommendations)

| # | Question | Recommendation | Gap |
|---|----------|----------------|-----|
| DP1 | Where does the projection code live? | New `crates/chv-netbox-adapter` in the management/control-plane integration layer; outside any Core runtime. | G1 |
| DP2 | NetBox client | Thin `reqwest` (rustls, HTTPS-required) client, token auth from config; never logs the token. Bounded to a documented NetBox REST contract for v4 DCIM/IPAM endpoints. | G1,G5 |
| DP3 | Source of truth for the projection | Applied architecture version (`normalized_model_json`) enriched by live `InventorySnapshot` host facts. Defer runtime-observed VM IPs to a snapshot-extension follow-up. | G2 |
| DP4 | Ownership/external-ID mechanism | Custom fields `chv_external_id` / `chv_architecture_id` / `chv_managed_by=chv` / `chv_managed_state` / `chv_architecture_version` / `chv_mapping_version`; exact-match lookup by `chv_external_id`. | G3 |
| DP5 | Idempotency + conflict policy | Reconcile by external-id; never mutate non-`chv_managed_by=chv` objects; name-collision on create → conflict; partial-failure re-entry → update. | G3 |
| DP6 | Triggers | Manual export endpoint + best-effort post-successful-apply enqueue; both run the same runner. | G4 |
| DP7 | Dry-run/report | Runner first computes a `NetboxProjectionPlan` (create/update/no-op/conflict/stale) — deterministic, secret-free. Dry-run returns it; export executes it. | G4 |
| DP8 | Failure isolation from apply | Post-apply projection is best-effort and asynchronous; projection run failure never alters the apply run/plan/topology result. Retry independent. | G4 |
| DP9 | NetBox config + token | Per-architecture `NetboxProjectionConfig` (endpoint, token `secret_ref`, custom-field schema refs, retention policy). Token stored via `credential_crypto`; enforce HTTPS; redact everywhere. | G5 |
| DP10 | Projection persistence + audit | New `netbox_projection_runs` table (queued/running/succeeded/failed, result_json, error, retry); emit the five `architecture_netbox_*` audit events (config_updated, dry_run, export_succeeded, export_failed, export_retried — attempt/result/retry). | G7 |
| DP11 | RBAC | Operator-level permission; production environments escalate to Admin (parity with apply/destroy). Requires `architecture:export` semantics; reuse `require_owner_or_admin` scoping. | G8 |
| DP12 | Retention / no-delete | Configurable per-architecture retention policy; default = mark removed CHV objects `chv_managed_state=stale` and leave in place. `delete` is opt-in only. | G10 |

---

## 6. CHV → NetBox mapping contract v1

Versioned (`MAPPING_VERSION = "v1"`). Built by pure builders in `mapping.rs`.

| CHV source | NetBox object | Key fields | Custom fields |
|---|---|---|---|
| `servers[]` + live `NodeInfo` | **DCIM Device** (device_role `chv-node`, device_type `chv-host`) | `name`, `site` (from env/labels), `serial`/asset none; `custom_fields` | external_id, arch_id, managed_by, state, version; CPU/mem stored in `device.custom_fields` when not otherwise placed |
| `instances[]` | **Virtualization VirtualMachine** | `name`, `status` (from run drift/state: active/staged/offline when known) | external_id, arch_id, managed_by, state, version; placement via `cluster` or `device` = mapped server when resolvable |
| `instances[].networks[]{name,ip}` | **Virtualization Interface** (`type virtual`, attached to the VM) | `name`, `mac` (none in v1 — not modelled), `description` = network name | external_id, arch_id, … |
| `networks[]` | **IPAM Prefix** (+ **IPAM VLAN** when `type=vlan` / `vlan_id` present) | `prefix`=`cidr`, `vlan`=`vlan_id`, `description` | external_id, arch_id, … |
| `instances[].networks[].ip` | **IPAM IPAddress** | `address`, `vrf` (from network), `dns_name` optionally | external_id, arch_id, … |
| `metadata.{environment,owner,labels}` | on every object as NetBox tags / custom fields | `tags` = derived from `labels` + `environment` | `chv_architecture_id`, `chv_external_id`, … |

### 6.1 Mapping rules
- **Names** are projected verbatim from the model but validated for NetBox
  charset; invalid names become a `conflict`/`error` in the report, never a
  best-effort rename.
- **Natural keys** used for collision detection are: Device `name`; VirtualMachine
  `name`; Prefix `prefix`+`vrf`+`vlan`; IPAddress `address`+`vrf`; Interface
  `name`+parent.
- **Unknown/unsupported NetBox-only fields** are never fabricated; nullable
  fields are left unset.
- **Deterministic ordering** of the plan (stable by kind then name) so
  dry-runs are byte-stable across calls.

---

## 7. Ownership & idempotency contract (detailed)

- External ID format: `arch:<architecture_id>:<kind>/<name>:<version>`.
  `kind` uses the stable `ResourceType` slug already in
  `apply/resource_type_as_str` (`server`, `network`, `instance`, …).
- Every object the adapter creates carries the custom fields exactly once.
- Reconcile is **by custom field, then natural key** (§4.3). This gives:
  - repeated export → all `update`/`no-op` (idempotent, no duplicates);
  - retry after partial failure → resumed as `update`;
  - pre-existing unrelated NetBox object → `conflict`, untouched.
- **Isolation of object families**: the adapter only ever touches objects where
  `chv_managed_by == "chv"` AND `chv_architecture_id == <this arch>`. This is
  the collision-tested ownership boundary the issue's AC requires.

---

## 8. Execution model

### 8.1 Flow
```text
trigger (manual | post-apply)
   → load config + token (decrypted in-memory only)
   → load applied version + live InventorySnapshot
   → build NetboxProjectionPlan (pure; may be dry-run)
       entries: create | update | no_op | conflict | stale
   ─ dry-run: return plan to caller (secret-free, deterministic)
   ─ export:  runner executes plan entries in order
       success item → record
       failure item → abort remaining (fixed v1 policy: abort-on-first-
                     hard-failure) → run Failed, retry
   → persist run (result_json, error), emit audit event
```

`stale` entries are executed by the runner as **marks** under the default
`mark_stale` retention policy. Under the opt-in `delete` retention policy the
runner executes `stale` entries as NetBox **deletions** — still guarded by the
unconditional ownership check (only objects with `chv_managed_by == "chv"` and
this architecture's `chv_architecture_id` are ever deleted). The plan's action
set is unchanged (`delete` is a runner interpretation of `stale` under the
configured retention, recorded in the plan's `retention` field), so dry-run
output remains policy-independent.

### 8.2 Idempotency & retry
- Same version re-export → all `no_op` (nothing changes).
- Version bump re-export → `update` those objects whose external-id version
  changed; new objects → `create`.
- Interrupted export → re-run matches by external-id and resumes (`update`),
  no duplication (DP5).
- NetBox outage / 5xx → run `Failed` (retryable, backoff); does **not** touch
  the apply result (DP8).

### 8.3 Ordering & concurrency
- Plan entries ordered by the mapping contract's kind rank
  (vlan → prefix → device → VM → interface → IP), which is
  FK-dependency-safe: parents exist before children (contract rule 3).
- One active projection run per architecture (claim), preventing interleaved
  exports.

---

## 9. Security

- **Token**: stored via the existing `credential_crypto` encrypted-secret path;
  referenced from `NetboxProjectionConfig.secret_ref`. Never emitted to logs,
  error messages, dry-run output, or responses.
- **Transport**: HTTPS enforced; non-HTTPS endpoint rejected at config time.
- **TLS verification** on by default (rustls); CA customization is out of v1.
- **RBAC** (DP11): projection is Operator-scoped; production (environment)
  exports escalate to Admin, matching apply/destroy parity. Object-scoped
  ownership via `require_owner_or_admin` (a user projects only topologies they
  can apply/destroy).
- **Secrets in NetBox objects**: the adapter copies no CHV secrets into NetBox.
  `secret_ref` fields are never exported; only names/ids/metadata are projected.
- **Audit**: the five `architecture_netbox_*` events (`config_updated`,
  `dry_run`, `export_succeeded`, `export_failed`, `export_retried`) carry
  attempt/result/retry state and never the token. These supersede the security
  spec's recommended-but-unimplemented `architecture_exported` name.

---

## 10. API surface (BFF, POST-only convention — parity with §1.5)

The authoritative endpoint and error-code tables live in the API contract
(`docs/specs/architecture-designer/contracts/netbox-api-contract.md`); the
summary below mirrors it.

| Endpoint | Role | Request → Response |
|---|---|---|
| `POST /v1/architectures/netbox/config/get` | Operator | `{id}` → config summary (never the token) or 404 |
| `POST /v1/architectures/netbox/config/upsert` | Operator | `{id, expected_version, endpoint, token?, token_secret_ref, retention_policy, enable_post_apply, site_name?}` → config |
| `POST /v1/architectures/netbox/config/delete` | Operator | `{id}` → deleted (NetBox untouched) |
| `POST /v1/architectures/netbox/export/dry-run` | Operator | `{id}` → `{mapping_version, architecture_id, architecture_version, entries[], summary{create,update,no_op,conflict,stale}}` (projects the most recent `succeeded` apply run's version) |
| `POST /v1/architectures/netbox/export` | Operator (Admin if production) | `{id}` → `{run_id, status}` |
| `POST /v1/architectures/netbox/runs/list` | Operator | `{id, limit?}` → projection runs for the topology |
| `POST /v1/architectures/netbox/runs/get` | Operator | `{id, run_id}` → run detail + result |
| `POST /v1/architectures/netbox/runs/retry` | Operator | `{id, run_id}` → `{run_id, status}` (failed runs below the attempt cap) |

Stable error codes (mirroring the contract table): `NETBOX_NOT_CONFIGURED`,
`NETBOX_NOT_APPLIED`, `NETBOX_HTTPS_REQUIRED`, `NETBOX_TOKEN_MISSING`,
`NETBOX_RUN_ACTIVE`, `PROJECTION_RUN_NOT_RETRYABLE`,
`PRODUCTION_REQUIRES_ADMIN`, `NETBOX_UNREACHABLE`, `NETBOX_AUTH_FAILED`,
`PLAN_EXPIRED`. Production gating
uses the environment → Admin rule; non-admin owners get the same 403 policy.

---

## 11. UI (WebUI, Svelte)

Under `ui/src/routes/architectures/[id]/` add a "NetBox projection" panel:

- Config form (endpoint, token secret-ref, enable post-apply, retention mode).
- Dry-run report table (create/update/no-op/conflict/stale with per-entry
  detail) rendered deterministically.
- "Export to NetBox" action with confirmation for production.
- Run history + per-run result/retry display.
- Conflicts surfaced with an explicit "CHV does not own this object" cue.

Follows the existing BFF POST-only wiring (`mutateWithRefresh` for mutations;
architecture stores under `ui/src/lib/stores/architecture-*.svelte.ts`).

---

## 12. Data model additions (control-plane store)

```text
netbox_projection_config
  architecture_id PK, endpoint, token_secret_ref, token_ciphertext,
  retention_policy, enable_post_apply, custom_field_prefix, site_name,
  created_at, updated_at

netbox_projection_runs
  id PK, architecture_id, architecture_version_id, trigger (manual|post_apply),
  mode (dry_run|export), status (queued|running|succeeded|failed),
  plan_json, result_json, summary_json, error_message, attempt_count,
  started_at, finished_at, requested_by, created_at
```

(Field detail and status machines live in the component spec; the tables
above mirror it.)

Persistence follows the designer rule (keep full inputs + results for audit and
future migration).

---

## 13. Acceptance criteria (mapped to #239)

- [x→design] Mapping spec (contract §6) covers hosts, VMs, VM interfaces,
  networks/prefixes, addresses, topology metadata.
- [x→design] Stable external-ID/ownership scheme documented (§7) and
  collision-tested (mock tests: name collision, foreign-object collision,
  partial-failure re-entry).
- [x→design] Manual and post-successful-apply export paths are idempotent
  (re-run → no-op; version bump → update; interrupted → resume).
- [x→design] Dry-run output is deterministic and secret-free.
- [x→design] Retry after partial NetBox failure does not duplicate objects.
- [x→design] Existing non-CHV NetBox objects are never modified without an
  explicit ownership match.
- [x→design] NetBox outage does not change the already-completed apply result.
- [x→design] Audit/event records expose projection attempt, result, retry.
- [x→design] Integration tests run against a bounded mock/test NetBox API.

---

## 14. Decomposition (proposed PR sequence)

1. **PR 1 — design adoption incl. this doc + ADR-023.** No code. Records DPs.
2. **PR 2 — `chv-netbox-adapter` pure core**: `mapping.rs`, `ownership.rs`,
   `plan.rs` (NetboxProjectionPlan), unit tests incl. ownership/collision tests
   against a mock NetBox model. No I/O.
3. **PR 3 — config + secrets + store**: `netbox_projection_config` /
   `netbox_projection_runs` tables, `NetboxProjectionConfigRepository`,
   `NetboxProjectionRunRepository`, credential reuse for the token.
4. **PR 4 — client + runner + worker**: `client.rs` (reqwest/rustls), 
   `NetboxProjectionRunner`, `NetboxProjectionWorker` (claim → execute → 
   persist → audit), retry/backoff, HTTPS enforcement.
5. **PR 5 — BFF surface**: config + dry-run + export + runs endpoints, RBAC and
   production escalation, error codes.
6. **PR 6 — post-apply hook**: best-effort enqueue on `Succeeded` transition
   (isolated; DP8).
7. **PR 7 — UI**: config form, dry-run report, export action, run history.
8. **PR 8 — integration tests**: bounded mock NetBox API; end-to-end export /
   dry-run / conflict / outage-isolation legs.

Each PR keeps the targeted validation ladder (`cargo check/test/clippy -p …`,
then workspace + BFF tests when the surface touches the router).

---

## 15. Follow-ups (explicitly deferred, tracked, not v1)

- Runtime-observed VM interface/IP inventory in `InventorySnapshot`
  (requires extending `InventoryProvider` + `FleetInventoryProvider`) — the
  prerequisite for projecting *observed* addresses rather than applied intent.
- Capacity reporting unification (ties to #514/#515) — to push host
  CPU/mem/datastore capacity into NetBox from authoritative sources.
- NetBox → CHV pull/compare surface (out of scope by issue non-goal).
- Multi-sink abstraction (a trait so a second inventory sink can be added) —
  `chv-netbox-adapter` is structured so this can become one implementation.
