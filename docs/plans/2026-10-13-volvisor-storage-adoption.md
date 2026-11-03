# Volvisor Storage Adoption Plan

> **Superseded (2026-11-02).** The active sequencing is
> [`2026-11-02-multihost-and-volvisor-transition.md`](2026-11-02-multihost-and-volvisor-transition.md).
> This document is retained as the original ADR-028 implementation review; do
> not use its phase ordering or work packages.

This plan implements [ADR-028](../specs/adr/028-volvisor-storage-orchestration.md),
which selects Volvisor as CHV's storage lifecycle and provider authority. It
does not enable a Volvisor-backed storage class for tenants until its gates
pass. Volvisor is a prototype, so integration is pinned to an exact revision
and qualified independently for every enabled class.

## Prototype input

The initial integration review uses
[`o3kio/volvisor@286f038c99db5aba29243acd7d3e0bfdb25bf50a`](https://github.com/o3kio/volvisor/commit/286f038c99db5aba29243acd7d3e0bfdb25bf50a)
as its candidate source revision. This is an immutable review reference, not a
CHV release pin or production-support commitment. Before packaging the service,
verify its build provenance, API/contract alignment, and real-host LVM evidence
against that exact commit; subsequent updates go through the CHV compatibility
matrix.

## Bounded execution scope

- **Component:** CHV control-plane storage intent, `chv-agent` reconciliation,
  node storage service integration, and Cloud Hypervisor attachment.
- **Likely packages:** `chv-controlplane-types`,
  `chv-controlplane-store`, `chv-controlplane-service`, `chv-agent-core`,
  `chv-agent-runtime-ch`, and the `chv-stord` compatibility boundary. Confirm
  the package list after the interface design is accepted.
- **Contracts:** Volvisor Volume API v2 and provider capability contract;
  CHV control-plane-to-agent and agent-to-stord contracts; ADR-002, ADR-004,
  ADR-016, ADR-017, and ADR-028.
- **Initial provider:** native-local LVM only. Ceph, nearline replication,
  automated placement across backends, and live storage migration are out of
  the first implementation slice.
- **Initial validation:** focused API/client and reconciliation tests, then
  workspace checks and root-gated real-LVM/Cloud Hypervisor qualification.

## Work packages

### 0. Pin and reconcile the external contract

- Select an immutable Volvisor commit and record its API version, build
  provenance, license, and supported kernel/LVM/Cloud Hypervisor matrix.
- Compare implemented routes and provider behavior with the v2 contract and
  capability documents; resolve any normative-document status drift upstream
  before relying on undocumented behavior.
- Specify the node-local transport and authentication. Do not expose the
  Volvisor admin API to tenant networks or allow the BFF to contact it.
- Define stable operation-ID derivation, expected-generation handling,
  capability negotiation, error mapping, frontend descriptor, and API
  compatibility checks.

### 1. Establish a single-owner transition model

- Add an explicit owner/version field to CHV volume intent (Volvisor or
  legacy-stord); do not infer ownership from a path, class string, or daemon
  default.
- Persist the Volvisor volume ID, storage class, and last observed storage
  generation in CHV's VM/volume references. Volvisor remains authoritative for
  provider state and storage generation.
- Add a reconciliation state machine for create/attach/detach/delete that
  records intent before calls, retries identical operation IDs, and inspects
  Volvisor after ambiguous outcomes.
- Keep existing volumes on their current owner until a separately reviewed
  migration explicitly transfers ownership. No dual writes and no automatic
  adoption or deletion of foreign storage state.

### 2. Implement the node-local client and native-local lifecycle

- Add a typed Volvisor client behind an agent-owned trait, including timeout,
  retry, capability, and health behavior.
- Route volume operations through `chv-agent`; it is the only remote node
  endpoint and remains the only Cloud Hypervisor client.
- On VM create, create/inspect the Volvisor volume, request a fenced
  single-writer attachment, attach the returned host-scoped frontend through
  the VMM adapter, then report observed attachment state.
- On VM stop/delete, stop or drain I/O, detach from the VMM, confirm that
  removal, then release the Volvisor attachment. Delete storage only on an
  explicit volume-delete operation with the right generation and no
  dependents.
- Preserve local/qcow2 legacy behavior during rollout. Reject unsupported
  classes or policies explicitly instead of silently routing them to the
  legacy backend.

### 3. Prove crash consistency and operator recovery

- Inject process termination after every boundary: CHV journal write,
  Volvisor request, Volvisor provider mutation, VMM attach/detach, and CHV
  outcome write.
- Verify identical retries converge, stale generations are rejected, and
  ambiguous results are inspected before retrying destructive work.
- Test Volvisor unavailable during normal operation, node restart, and CHV
  control-plane partition. Existing VMs must continue using established
  attachments; new or destructive operations fail safely until storage state
  can be reconciled.
- Add diagnostics exposing storage owner, Volvisor revision/API version,
  operation ID, generation, capability set, last observation time, and
  actionable failure state without exposing credentials.

### 4. Add capacity/health projections and qualify additional classes

- Project Volvisor capacity and backend health into scheduling with an
  explicit freshness bound; stale or unknown capacity cannot authorize a new
  placement.
- Qualify existing Ceph RBD as a separate provider integration, including
  auth/secret handling, health truthfulness, frontend compatibility, and
  real-cluster failure tests.
- Treat nearline replication and storage migration as separate high-risk
  work. Require writer fencing, flush/FUA, host-loss, partition, interrupted
  handoff, and recovery evidence before capability advertisement.
- Close or split #515 items only when the corresponding Volvisor capability is
  integrated and qualified; an upstream feature or API route alone does not
  satisfy the CHV issue.

## Acceptance criteria

The native-local slice is ready for tenant enablement only when:

1. New volumes are created through CHV and owned by Volvisor without a
   competing `chv-stord` lifecycle record.
2. The CHV control plane can restart or partition without losing desired
   volume references, and the agent safely reconciles to Volvisor's recorded
   generation.
3. VMM attachment and detach evidence fences writes and deletion correctly,
   including lost-response and process-crash cases.
4. Root-gated real-LVM tests and real Cloud Hypervisor tests pass on the pinned
   compatibility matrix, including host reboot, LVM exhaustion, and guest
   flush/FUA verification.
5. The PR discloses storage ownership and data-loss behavior and links exact
   SHA qualification evidence as required for high-risk storage changes.

Until these criteria pass, Volvisor remains the selected architectural target
and prototype, while current production behavior stays on the explicitly
identified legacy storage owner.
