# CHV Architecture

This document describes the high-level architecture of CHV, the boundaries between components, and the current implementation phase.

## System Overview

CHV is a Linux-first, cloud-image-first virtualization platform. It targets sovereign private cloud and edge environments. Operators get full control over the hypervisor stack without the operational complexity of OpenStack or the licensing cost of VMware vSphere. VMs run on Cloud Hypervisor (the VMM); the binary is `cloud-hypervisor`.

The currently shipped CHV runtime is built around four binaries. Volvisor is
the selected storage target; its node service has not yet been integrated into
CHV releases.

| Binary | Responsibility | Runtime | Status |
|--------|---------------|---------|--------|
| `chv-controlplane` | Orchestration, desired-state management, node enrollment, Web UI BFF | Control-plane host | Shipped |
| `chv-agent` | VM lifecycle, Cloud Hypervisor runtime, serial console, local telemetry | Hypervisor host | Shipped |
| `volvisord` | Provider records, capabilities, capacity/health and storage lifecycle | Hypervisor host, called through `chv-agent` | Selected target; integration pending |
| `chv-stord` | Current storage path; retained for existing volumes during the Volvisor transition | Hypervisor host (sidecar to agent) | Shipped; current implementation |
| `chv-nwd` | Network topology, bridges, firewall/NAT, DHCP, DNS | Hypervisor host (sidecar to agent) | Shipped |

## Architectural Boundaries

### Control Plane ↔ Agent

- **Only `chv-agent` is reachable from the control plane.** All communication is gRPC over mTLS.
- The control plane owns **desired state**; the agent owns **observed state** and converges toward desired state.
See ADR-002: [Control Plane to Node Boundary](./specs/adr/002-control-plane-boundary.md)

### Agent ↔ Hypervisor Trait Boundary (`chv-hypervisor-api`)

- `chv-agent-core` is **fully decoupled** from Cloud Hypervisor and depends strictly on the `HypervisorAdapter` trait in `chv-hypervisor-api`.
- Low-level VMM interaction (process supervision, REST API over Unix socket, PTY master allocation) is isolated in `chv-agent-runtime-ch`.
- Alternative VMM backends (such as O3K or OpenStack Nova drivers) implement `chv_hypervisor_api::HypervisorAdapter` and `cellhv_core_executor::CoreVmRuntime`. Neither `chv-agent-core` nor `cellhv-core-*` needs modification.

### Agent ↔ Storage / Network

- The target architecture has `chv-agent` communicate with the node-local Volvisor service through its
  versioned authenticated internal API, and with `chv-nwd` via local gRPC.
  During the ownership transition it may also call `chv-stord` for volumes
  explicitly marked as legacy-owned. Volvisor and `chv-stord` must never
  independently mutate the same volume.
- These daemons may be upgraded independently inside a compatibility matrix, but the default is a bundle-tested node release.

See ADR-001: [Node Runtime Split](./specs/adr/001-node-runtime-split.md)

### Web UI ↔ Backend

- The browser talks **only** to the control-plane backend-for-frontend (BFF) HTTP service (`chv-webui-bff`).
- Direct browser access to `chv-agent`, Volvisor, `chv-stord`, `chv-nwd`, or Cloud Hypervisor APIs is forbidden.

See ADR-002-WebUI: [WebUI Architecture Boundary](./specs/adr/002-webui-architecture-boundary.md)

## Data Flow

### VM Creation (Happy Path)

```
User (Web UI)
    │ POST /api/v1/vms
    ▼
chv-controlplane (BFF)
    │ validate, quota check, assign node
    ▼
SQLite (desired_state, operation_journal)
    │
    ▼
Reconcile loop (target after Volvisor integration)
    │ gRPC CreateVm
    ▼
chv-agent
    │ 1. Volvisor: prepare/attach; existing volumes remain on chv-stord until ownership migration
    │ 2. call chv-nwd   (ensure network)
    │ 3. call cloud-hypervisor (vm.create)
    ▼
Observed state streamed back ──► SQLite ──► Web UI polling
```

### Serial Console

```
Browser ──► WebSocket /ws/vms/{id} ──► BFF ──► gRPC ──► chv-agent ──► PTY ──► Cloud Hypervisor API
```

Console access is gated by short-lived JWT tokens with one-time-use replay prevention.

## State Machines

### Node State

Nodes progress through explicit states before they are schedulable:

`Discovered` → `Bootstrapping` → `HostReady` → `StorageReady` → `NetworkReady` → `TenantReady`

Only `TenantReady` nodes receive new VMs. Nodes may also enter `Degraded`, `Draining`, `Maintenance`, or `Failed`.

#### Drain Evacuation Flow

When a node enters `Draining` (via `chvctl node drain` or the BFF API):

> **Qualification status:** The drain flow is code-supported but unqualified.
> No qualified path exercises node-drain evacuation.

1. Scheduling is paused on the node (no new VMs placed).
2. The agent reconcile loop detects `Draining` state and iterates running VMs.
3. The agent issues a migration request to the control plane for each VM. It tracks requests in `drain_requested_vms` to avoid duplicates.
4. When `vm_count` reaches 0 (all VMs evacuated or stopped), the node transitions to `Maintenance`.
5. After maintenance, an operator marks the node `TenantReady` to resume scheduling.

Implementation: `Reconciler` in `crates/chv-agent-core/src/reconcile.rs` handles the `NodeState::Draining` arm.

In core-managed (single-authority) mode the drain flow is deliberately
different: the reconcile loop is observe-only, issues **no** migration
requests (the control plane, not the agent, owns re-homing), and blocks the
`Draining` → `Maintenance` transition while any desired-Running or
undecodable fragment remains. A blocked drain raises a `drain_blocked`
control-plane alert on change; the control plane must re-home the VMs before
the node can reach `Maintenance`.

See ADR-003: [Node State Machine](./specs/adr/003-node-state-machine.md)

### Task State

Every mutating action creates a task record with states:

`queued` → `running` → (`succeeded` | `failed` | `cancelled`)

Tasks are first-class UI objects; users can inspect progress, cancel queued tasks, and view history.

## Storage Datapath

CHV selects Volvisor as its storage lifecycle and provider authority. CHV keeps
the tenant API, tenant-visible volume IDs, and VM desired state; `chv-agent` is
the node integration point and remains the only component that calls Cloud
Hypervisor. Volvisor owns provider-side volume records and generations,
capabilities, capacity/health, and storage-side fencing.
This is the target architecture; Volvisor is a prototype and its capabilities
are not enabled for tenants until each CHV integration passes its qualification
gates. See [ADR-028](./specs/adr/028-volvisor-storage-orchestration.md) and
the [adoption plan](./plans/2026-10-13-volvisor-storage-adoption.md).

The selected fast path remains host-side; adopting Volvisor does not require a
storage VM or a mandatory NBD proxy. Current storage behavior remains on
`chv-stord` until the agent adapter and explicit volume-owner transition are
implemented. New Volvisor-backed classes can be enabled only after their
provider and CHV integration are qualified:

- `native-local` (initial integration target: LVM)
- `ceph-rbd` (later, existing Ceph cluster adapter)
- `nearline-replicated` (later, after replication and fencing qualification)
- Existing raw/qcow2 and other legacy classes remain on `chv-stord` until an explicit migration.

After the owner transition is implemented, `chv-stord` serves only volumes
explicitly marked as legacy-owned. Volvisor-backed volumes use advertised
capabilities and fail closed when a requested class or policy is unavailable.
The Web UI and `chvctl` continue to call CHV APIs; Volvisor is not a second
tenant-facing control plane.

The former storage-VM / NBD model remains rejected for the native-local
foreground path.

### Migration Security Model

Storage migration between nodes is secured with mandatory mTLS:

- **mTLS enforcement**: `MigrationSender` rejects plaintext connections. If `tls_config` is not provided, `start_migration()` returns `FAILED_PRECONDITION`. There is no fallback to `http://`.
- **Certificate validation**: The sender presents the node certificate issued by the CP CA and validates the destination's certificate against the same CA.
- **Backpressure**: The receiver can send `Backpressure` messages with a `slow_down_factor`. The sender inserts throttle sleeps proportional to this factor between chunk sends.
- **Flow control**: A sliding send window (default 128 unacknowledged chunks) prevents memory exhaustion on either side.
- **MigrationReaper**: A background task (`crates/chv-controlplane-service/src/migration_reaper.rs`) scans every 60s for migrations stuck beyond 2 hours and force-transitions them to `Failed`.

Current status: quiescent-volume (single-writer) disk migration is qualified. M4.6 exercised it between two `chv-stord` instances over mTLS on a single host ([evidence](./evidence/production-readiness/v0.3.0-rc1/04-real-host-qualification/m4.6-migration.md)). Concurrent-write migration remains unproven (issue #394). The control-plane-orchestrated `migrate_vm` path is outside the qualified surface.

Implementation: `crates/chv-stord-core/src/migration/sender.rs`

See ADR-004: [Storage Datapath Model](./specs/adr/004-storage-datapath.md)

## Network Service Model

MVP-1 uses Linux bridge + netns + veth + nftables via a host-side `chv-nwd` daemon. Advanced features:

- Kernel VXLAN overlay networking with explicit FDB cleanup on VM detach
- eBPF policy and rate-limit enforcement only; eBPF is not the VXLAN datapath
- VXLAN teardown via `delete_topology` (cleans up VXLAN interfaces)

See ADR-005: [Network Service Model](./specs/adr/005-network-service-model.md)

## Partition and Autonomy

During control-plane outages, nodes preserve runtime state and allow limited local operations (self-heal, VM stop/reboot). They deny new VM creation, migrations, and destructive topology mutations. Upon reconnection, nodes converge back to the control-plane desired state.

### Partition Reconnect Flush

When an agent detects that it has reconnected to the control plane after a partition (state transitions from `Disconnected` to `Connected`), it flushes all pending messages queued during the outage. The flush is ordered and atomic per message. A failed dispatch leaves the remaining messages queued for the next attempt. The queue is also drained on every agent tick (~5 s) while connected, so messages queued by always-on reporters (e.g. migration progress) are delivered during stable connections without waiting for a reconnect; each dispatch is bounded by a 10 s deadline.

Implementation: `drain_pending_control_plane_queue` in `crates/chv-agent-core/src/control_plane.rs`. Pending messages are stored in `NodeCache::pending_control_plane_messages`.

See ADR-006: [Partition and Autonomy Policy](./specs/adr/006-partition-policy.md)

## Upgrade and Rollback

ADR-007 defines the policy: bundle-tested node releases by default, selective component upgrades only inside a compatibility matrix, and one-step rollback to the previous tested bundle. The ADR scopes the matrix over the control plane, the node daemons, Cloud Hypervisor, and host helper tools. The implemented gate checks `chv-agent`, `chv-stord`, and `chv-nwd` versions from `node_inventory`.

### Upgrade orchestration

> **Qualification status:** The surfaces below are code-supported but
> unqualified. No qualified path exercises them. No automated end-to-end
> upgrade flow exists (the dead `chvctl upgrade` surface was removed via
> issue #427).

The former control-plane upgrade stack (`UpgradeOrchestrator`, `SystemdNodeUpgrader`) was deleted as dead code in PR #213. No automated agent binary-swap path survives. ADR-007 defines bundle policy; it does not describe a swap mechanism. The surviving surfaces are:

| Surface | Behavior | Code |
|---------|----------|------|
| Compatibility-matrix boot gate | Refuses control-plane startup when enrolled node versions violate the matrix | `cmd/chv-controlplane/src/bootstrap.rs` |
| `DrainNode` handler | Pauses scheduling on a node and starts evacuation | `crates/chv-agent-core/src/agent_server.rs` |
| `EnterMaintenance` handler | Moves a drained node into `Maintenance` | `crates/chv-agent-core/src/agent_server.rs` |

The boot gate is operator-opt-in via `CHV_COMPAT_MATRIX_PATH`. Once opted in, it fails closed: a boot-time inventory query failure refuses startup instead of bypassing the gate. The BFF wires `drain` and `enter_maintenance` node actions to the handlers above (`crates/chv-controlplane-service/src/bff_mutations.rs`).

The `chvctl upgrade` subcommands were removed (issue #427). They targeted `/v1/upgrades` BFF routes that were never registered — the routes' backing control-plane upgrade stack (`UpgradeOrchestrator`, `SystemdNodeUpgrader`) was deleted as dead code in PR #213, so the subcommands could only fail at runtime with a 404. No automated upgrade surface exists; node upgrades are a manual procedure.

See ADR-007: [Upgrade and Rollback Policy](./specs/adr/007-upgrade-rollback.md)

## Resilience

### Circuit Breaker

Node communication from the control plane is protected by a circuit breaker (`crates/chv-controlplane-service/src/circuit_breaker.rs`). States: `Closed` (normal) → `Open` (reject immediately after N failures) → `HalfOpen` (probe). Defaults: 5 failures to trip, 30s recovery timeout, 3 successful probes to close.

The `with_circuit_breaker()` helper wraps any async operation and automatically records success/failure. When open, calls return `ChvError::BackendUnavailable` without attempting the RPC.

### Deep Health Checks

The `GET /health/deep` endpoint reports component-level health:

- **database**: SQLite connectivity with latency measurement
- **agent_socket_dir**: Agent runtime directory exists and is readable
- **agent_connectivity**: Can establish a Unix socket connection to at least one agent

Status values: `healthy` (all pass), `degraded` (DB pass but agent issues), `unhealthy` (DB fail). Degraded returns HTTP 200 (still serving); unhealthy returns 503.

## Current Implementation Phase

**Phase:** Early-to-MVP transitioning to stability  
**Roadmap:** [`../PHASED_IMPLEMENTATION_PLAN.md`](../PHASED_IMPLEMENTATION_PLAN.md)  
**Gap analysis:** [`./GAP_ANALYSIS.md`](./GAP_ANALYSIS.md)

### What Works

- VM lifecycle (create, start, stop, reboot, delete) via desired-state reconciliation
- Node enrollment with mTLS and bootstrap tokens
- Certificate authority with optional CA-backed issuer
- SQLite repositories with desired/observed state tracking
- Operation journal with idempotency
- Prometheus metrics endpoint
- Web UI dashboard, VM list/detail, events, images, networks, storage pools
- Serial console backend (WebSocket → PTY → Cloud Hypervisor)
- Hypervisor settings DB + BFF (orchestrator merge partially wired)
- Basic CI (GitHub Actions)
- Compatibility-matrix boot gate and drain/maintenance handlers (code-supported, unqualified)
- Storage migration with mTLS enforcement, backpressure, and flow control; quiescent-volume (single-writer) migration is qualified (M4.6), concurrent-write migration remains unproven (#394)
- Circuit breaker on node communication
- Deep health checks (database, agent socket, agent connectivity)
- Migration reaper (auto-fails stuck migrations after 2h)
- Drain evacuation on node drain (code-supported, unqualified; agent-driven in legacy mode, control-plane-driven in core-managed mode)
- Partition reconnect flush (pending messages delivered on reconnect)
- eBPF policy scope defined for policy/rate limiting; kernel VXLAN remains the overlay datapath
- FDB cleanup on VM detach
- VXLAN teardown on topology delete

### Remaining Gaps

| Area | Gap | Priority |
|------|-----|----------|
| Backend | Backup/DR: **not implemented — API/UI hard-fenced (`501 NOT_IMPLEMENTED`)**; tenant-data protection is the operator's responsibility | P2 |
| Backend | Concurrent-write disk migration unproven (issue #394); control-plane-orchestrated migration path unqualified | P1 |
| Backend | iSCSI and Ceph RBD storage backend adapters planned, not production-complete | P2 |
| UI | Some components still exceed 300 lines (`vms/[id]/+page.svelte` at 325, `SidebarNav.svelte` at 346) | P2 |

## Technology Choices

| Layer | Technology | Rationale |
|-------|-----------|-----------|
| Backend language | Rust | Memory safety, async performance, single-binary deployments |
| Database | SQLite | Zero-config, embedded, sufficient for early phase; migration path to PostgreSQL documented |
| Frontend | SvelteKit + TailwindCSS | Compile-time optimizations, minimal runtime, design-token-friendly |
| RPC | gRPC / protobuf | Strong contracts, streaming, generated bindings |
| BFF HTTP | axum | Rust-native, async, integrates with tonic stacks |
| Metrics | Prometheus | Industry standard, pull-based, low overhead |
| VMM | Cloud Hypervisor | Modern, Rust-based, KVM-only, minimal attack surface |

## Related Documents

- [Architecture Decision Records](./specs/adr/)
- [Component Specifications](./specs/component/)
- [Phased Implementation Plan](../PHASED_IMPLEMENTATION_PLAN.md)
- [Deployment Guide](./DEPLOYMENT.md)
- [Operations Guide](./OPERATIONS.md)
- [Design System](../DESIGN.md)
