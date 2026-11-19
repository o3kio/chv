# ADR-023 NetBox Projection as a Bounded Downstream Inventory Adapter

Date: 2026-10-08
Status: Accepted (implemented 2026-10; status reconciled 2026-11-19; companion design: `docs/design/issue-239-netbox-projection.md`; issue: kubedoio/chv#239)

## Context

The Architecture Designer (Phases 0–7, ADR-001-Designer…ADR-006-Designer) models,
validates, plans, applies, and drift-detects CHV infrastructure. Its original
concept called out **NetBox as the next-step inventory target** (#239): operators
running NetBox for IPAM/DCIM want the applied CHV topology reflected there.

CHV today is a closed loop: desired state (CHVArchitecture YAML), live state
(InventorySnapshot over SQLite), plan/apply/drift, all internal to the control
plane. There is no outbound integration layer, no external-object ownership
mechanism, and no async integration-job infrastructure. Writing into NetBox is
the first surface in the repo that mutates **state owned by an external system**.

Two prior decisions shape the boundary:

- **ADR-005-Designer** separates desired topology (editable) from live fleet
  state (operational). A NetBox projection is neither: it is a *derived,
  downstream copy* of both, and must be labelled as such.
- **ADR-016/ADR-017** lock `chv-agent`/CellHV Core as the sole lifecycle and
  VM authority. Any integration must live **outside the Core runtime** and must
  never feed back into lifecycle decisions.

The issue itself pre-commits the shape: CHV stays authoritative; NetBox is a
downstream inventory/IPAM/DCIM projection; export is explicit and idempotent;
projection failure must never roll back a successful apply; no bidirectional
reconciliation in the first version; no destructive deletes by default.

## Decision

1. **Projection, not reconciliation.** The NetBox integration is a one-way,
   on-trigger, idempotent projection of the **applied architecture version**
   (enriched with live fleet facts for hosts). NetBox is never a source of
   truth for CHV: nothing in CHV reads NetBox to make decisions, and no
   NetBox-side change is pulled back. The projection is labelled downstream in
   all UI surfaces ("NetBox projection", never "sync").

2. **The adapter lives in the management/control-plane integration layer.**
   A new workspace crate `crates/chv-netbox-adapter` (pure mapping/ownership/
   plan builders + a thin HTTPS REST client + a runner). It is a library
   consumed by the WebUI BFF (manual export, dry-run) and a control-plane
   worker (post-apply trigger). It is explicitly **not** part of
   `chv-controlplane-service`'s core dispatch path, `chv-agent`, `chv-stord`,
   or `chv-nwd`, and it has no proto surface. This honors ADR-016/017: the
   Core runtime never depends on, awaits, or observes the projection.

3. **Ownership is expressed only through custom fields.** Every object the
   adapter creates carries `chv_managed_by="chv"`, `chv_external_id`
   (`arch:<arch_id>:<kind>/<name>:<version>`), `chv_architecture_id`,
   `chv_managed_state`, `chv_architecture_version`, and
   `chv_mapping_version`. Reconcile matches by
   `chv_external_id`, then by natural key. The adapter **never modifies or
   deletes** a NetBox object that does not carry `chv_managed_by="chv"` —
   collisions with foreign objects are reported as conflicts, never taken
   over. This is the "never silently take over" guarantee made mechanical.

4. **Failure isolation is absolute.** Projection runs are persisted
   (`netbox_projection_runs`) and executed asynchronously by a worker
   (`NetboxProjectionWorker`). The post-successful-apply trigger is a
   best-effort enqueue at the apply-run terminal transition; a NetBox outage
   or projection failure never changes the apply run's result, the plan
   status, or the topology's lifecycle status. Projection failure is
   separately visible and independently retryable.

5. **No delete by default.** Removed CHV resources are marked
   `chv_managed_state="stale"` in NetBox and left in place. A `delete`
   retention mode exists but is opt-in per-architecture configuration, never
   a default.

6. **Secrets follow the platform pattern.** The NetBox API token is stored via
   the existing `CredentialEncryption` secret path and referenced by
   `secret_ref`; HTTPS is enforced; the token never appears in logs, errors,
   dry-run output, or API responses.

7. **The mapping is a versioned contract.** `MAPPING_VERSION = "v1"` covers
   hosts→Devices, VMs→VirtualMachines, VM NICs→Interfaces, networks→
   Prefixes/VLANs, addresses→IPAddresses, topology metadata→tags/custom
   fields. Version bumps are additive and documented in the contract file.

## Rationale

- A pull/compare (bidirectional) design was rejected: it requires conflict
  resolution against an external authority, violating both the issue's
  non-goals and the platform's single-authority posture (ADR-017).
- Projecting from the applied version (not the live snapshot) is chosen
  because `InventorySnapshot` does not yet carry instances, VM interfaces, or
  addresses (the plan diff documents this gap), while the applied version
  carries the full shape and is immutable and auditable. Runtime-observed
  addresses are an explicit follow-up gated on snapshot extension.
- Custom fields (not, e.g., a dedicated NetBox site or tenant hack) carry
  ownership because they are queryable, survivable, and visible to NetBox
  operators without requiring privileged NetBox schema changes.
- A dedicated worker (rather than inline execution in the BFF handler) keeps
  NetBox latency/outages off the request path and provides the retry/
  visibility surface the issue requires.

## Consequences

- New surfaces: `chv-netbox-adapter` crate, two SQLite tables
  (`netbox_projection_config`, `netbox_projection_runs`), a control-plane
  worker, BFF endpoints under `/v1/architectures/netbox/*`, and a WebUI
  projection panel. All are additive; no existing contract changes.
- The projection is eventually-consistent with applied topologies; operators
  see a projection-run history rather than a live mirror.
- Foreign-object name collisions are surfaced as conflicts and require manual
  NetBox-side resolution; the adapter will not resolve them.
- Follow-up capability (observed runtime IP projection) is enabled but not
  delivered by this decision; it requires `InventorySnapshot` extension and
  its own qualification.
- If NetBox changes its custom-field or REST contract, the adapter is the
  single place to adapt (the mapping contract is versioned).
