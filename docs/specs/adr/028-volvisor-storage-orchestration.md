# ADR-028 — Adopt Volvisor for CHV Storage Orchestration

## Status

Accepted; runtime integration pending

## Date

2026-10-13

> **Implementation note (2026-11-02).** The runtime integration is specified by
> [`chv-volume-backend-v1`](../contracts/chv-volume-backend-v1.md) and sequenced
> by
> [`2026-11-02-multihost-and-volvisor-transition.md`](../../plans/2026-11-02-multihost-and-volvisor-transition.md).
> Volvisor's reference v2 implementation (pin pending the P3 contract tests)
> has no route that advances an
> attachment from `prepared` to `advertised`/`active`. The corrected design is
> tracked in [o3kio/volvisor#34](https://github.com/o3kio/volvisor/issues/34)
> and implemented on a Volvisor feature branch, but it is not merged, released
> or part of the CHV pin. Until the pin advertises `attachment_state_report`,
> CHV records `prepared` with `observed = false` and does not claim a live
> attachment (contract §8). Once re-pinned, CHV may reach `advertised` from a
> truthful `vmm_configured` attestation, but v1 MUST NOT claim `active`: that
> requires `guest_confirmed` evidence, and the default install has no guest
> confirmation path. That is an upstream contract gap, not a weakening of this
> ADR's truthfulness requirement.

## Context

CHV currently implements storage lifecycle and provider behavior across the
control plane, `chv-agent`, and `chv-stord`. This has left volume creation,
storage placement/capacity, backend selection, and storage mobility split
across interfaces with different ownership models. Issue #515 tracks deferred
storage capabilities; issue #379 established the current storage-class
dispatch path.

[Volvisor](https://github.com/o3kio/volvisor) is the selected storage
virtualization and provider project for CHV. Its volume API and provider model
cover the capabilities CHV needs to grow toward: native local logical volumes,
Ceph RBD, nearline replication, capacity and health reporting, attachment
fencing, and storage mobility. Volvisor is a prototype. This decision selects
it as the integration target; it does not claim that any Volvisor capability
is production-qualified for CHV.

The integration must preserve CHV's control-plane-to-node boundary (ADR-002),
Cloud Hypervisor ownership by `chv-agent`, and the host-side storage datapath
chosen in ADR-004. It must also avoid having CHV and Volvisor independently
mutate or journal the same storage resource.

## Decision

CHV adopts Volvisor as its storage lifecycle and provider authority. CHV
continues to expose the tenant-facing API, assign tenant-visible volume IDs,
and own VM desired state. Volvisor owns the provider-side volume record and
generation, provider selection and capability claims, provisioning, storage
health/capacity, and storage-side attachment state.

All node storage mutations continue to pass through `chv-agent`. The agent
integrates with the node-local Volvisor service using its versioned internal
API; the control plane, Web UI, and tenant clients do not call Volvisor
directly. The agent remains the only component that calls Cloud Hypervisor.
The agent coordinates VMM attachment and reports its observed attachment
phase to Volvisor so storage authority is not released before VMM detach and
I/O drain are established.

The ownership split is:

| Concern | Authority |
|---|---|
| Tenant API, VM desired state, VM scheduling and VMM lifecycle | CHV control plane and `chv-agent` |
| Tenant-visible volume ID | CHV assigns and owns the ID; Volvisor uses it as the stable storage-resource key |
| Provider state, storage generation, backend policy/capabilities, pool/device claims and storage lifecycle | Volvisor |
| VMM device creation/removal and observed VMM state | `chv-agent` |
| Compatibility storage operations during migration | `chv-stord`, only for volumes explicitly remaining on the legacy path |

Every volume has one declared storage owner. A volume must never be provisioned,
attached, detached, resized, or deleted by both Volvisor and `chv-stord` as
independent authorities. CHV may persist a projection/reference to a Volvisor
volume for VM intent and UI queries, but Volvisor's storage generation and
journal are authoritative for storage mutations. Mutations carry stable
operation IDs and expected generations so retries and crash recovery are
idempotent and stale operations fail closed.

Volvisor classes and features are enabled only when the connected Volvisor
instance advertises them. CHV does not silently map legacy `local` volumes to a
different durability or mobility class. In particular, nearline replication,
Ceph, capacity-aware placement, and live movement remain unavailable in CHV
until the exact integration and provider capability have passed the
qualification gates below.

## Integration sequence

1. **Freeze the integration contract.** Pin a Volvisor commit/release and
   document the API version, local transport and authentication, operation
   IDs, generation mapping, capability discovery, frontend handle, and error
   mapping. Reconcile the prototype's implementation status with its normative
   API documents before treating either as a compatibility promise.
2. **Build the agent adapter.** Add a `chv-agent` storage client that reaches
   Volvisor only over a node-local authenticated boundary. Preserve
   control-plane-to-agent mTLS and the rule that only the agent reaches the
   VMM. Add startup readiness and compatibility checks.
3. **Prove the native-local LVM lifecycle.** Exercise create, inspect, attach
   to a Cloud Hypervisor VM, detach after stop/I/O drain, grow, delete, and
   recovery after crashes at each boundary. Persist Volvisor volume references
   in CHV desired state and reconcile them without duplicate side effects.
4. **Move capacity and health into placement.** Project Volvisor's observed
   capacity and capability data into CHV scheduling only after reports are
   freshness-bounded and unknown/stale values fail closed.
5. **Qualify additional providers separately.** Integrate existing-cluster
   Ceph RBD, then mixed-provider placement and nearline replication only after
   their own auth, failure-domain, durability, and recovery evidence is
   complete. Storage migration requires a proven storage handoff in addition
   to VM migration evidence.

## Qualification gates

No Volvisor-backed class is production-supported by this ADR alone. Each
enabled class requires exact Volvisor SHA, CHV SHA, kernel/LVM/Ceph/DRBD and
Cloud Hypervisor versions, real-host evidence, and failure-injection results
matching its durability and mobility claims. At minimum, the native-local
integration must demonstrate:

- idempotent replay and recovery when either process restarts between intent,
  provider mutation, VMM attach, and recorded outcome;
- no second writer or premature delete/detach when responses are lost or
  generations are stale;
- truthful capacity, health, and attachment states, including unknown and
  unavailable-provider behavior;
- guest flush/FUA behavior, host reboot, device loss, LVM exhaustion, and
  storage-service outage behavior;
- compatibility and rollback behavior across supported CHV/Volvisor versions.

High-risk storage changes must disclose the data-loss and isolation behavior in
their PR and link the matching verification evidence, per `CONTRIBUTING.md`.

## Consequences

- Volvisor is the strategic implementation for #515's deferred storage
  capabilities; CHV should not grow parallel LVM, Ceph, replication, or
  migration authorities.
- ADR-004 remains authoritative for the host-side fast path and the rejection
  of a mandatory storage VM/NBD datapath. This ADR supersedes it only where
  ADR-004 assigns storage lifecycle/provider ownership to `chv-stord`.
- `chv-stord` remains supported during migration and for explicitly
  legacy-owned volumes. Its overlapping lifecycle paths can be retired only
  after volume ownership is explicit and existing volumes have a safe,
  reversible migration plan.
- Volvisor's prototype status requires CHV to pin an exact revision, maintain
  an integration compatibility matrix, and qualify capabilities before
  enabling them to tenants.
- The Web UI and `chvctl` continue to use CHV APIs; Volvisor's internal API is
  not a second tenant-facing control plane.

## Related

- [ADR-002 — Control Plane to Node Boundary](002-control-plane-boundary.md)
- [ADR-004 — Storage Datapath Model](004-storage-datapath.md)
- [Issue #379 — Storage-class dispatch](../../design/issue-379-storage-class-dispatch.md)
- [Issue #515 — Deferred storage capabilities](https://github.com/o3kio/chv/issues/515)
- [Volvisor project](https://github.com/o3kio/volvisor)
- [Volvisor adoption implementation plan](../../plans/2026-10-13-volvisor-storage-adoption.md)
- [CHV volume backend contract v1](../contracts/chv-volume-backend-v1.md)
- [Multi-host and Volvisor transition plan](../../plans/2026-11-02-multihost-and-volvisor-transition.md)
- [ADR-029 — Multi-Host Dispatch](029-multi-host-dispatch.md)
