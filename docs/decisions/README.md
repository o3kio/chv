# Architecture Decision Records (ADRs)

This directory indexes all Architecture Decision Records for CHV. The canonical storage location is [`docs/specs/adr/`](../specs/adr/); this file provides a quick-reference index.

## Backend & Infrastructure ADRs

| ADR | Title | Status | Summary |
|-----|-------|--------|---------|
| [ADR-001](../specs/adr/001-node-runtime-split.md) | Node Runtime Split | **Accepted** | Defines `chv-agent` (orchestrator), `chv-stord` (storage), `chv-nwd` (network), and `cloud-hypervisor` (VMM) as separate node-side daemons. Forbids collapsing storage/network into the agent. |
| [ADR-002](../specs/adr/002-control-plane-boundary.md) | Control Plane to Node Boundary | **Accepted** | Control plane communicates only with `chv-agent` over local Unix-socket gRPC, authenticated per request with an agent bearer token and peer-UID check (remote mTLS is design-only, ADR-029). Cloud Hypervisor is local-only. Control plane owns desired state; nodes report observed state. |
| [ADR-003](../specs/adr/003-node-state-machine.md) | Node State Machine | **Accepted** | Defines explicit node states (`Discovered` → `TenantReady`) and scheduling rules. Only `TenantReady` nodes are schedulable. |
| [ADR-004](../specs/adr/004-storage-datapath.md) | Storage Datapath Model | **Accepted** | Rejects storage-VM/NBD for MVP-1 in favor of host-side `chv-stord`. Defines supported storage classes and runtime limits. |
| [ADR-005](../specs/adr/005-network-service-model.md) | Network Service Model | **Accepted** | Defers network-VM approach; `chv-nwd` is a host-side Linux bridge/netns/veth/nftables daemon. Defers eBPF and distributed overlay. |
| [ADR-006](../specs/adr/006-partition-policy.md) | Partition and Autonomy Policy | **Accepted** | Defines node behavior during control-plane outages: preserve runtime, allow limited local ops, deny destructive mutations, converge on reconnection. |
| [ADR-007](../specs/adr/007-upgrade-rollback.md) | Upgrade and Rollback Policy | **Accepted** | Mandates bundle-tested node upgrades with one-step rollback. Tracks versions for control plane, all node daemons, Cloud Hypervisor, and host helpers. |
| [ADR-008](../specs/adr/008-error-handling-patterns.md) | Error Handling Patterns | **Accepted** | Structured errors via `chv-errors`; no panics in service code; graceful mutex poison recovery; explicit gRPC/HTTP mapping. |
| [ADR-009](../specs/adr/009-logging-and-observability.md) | Logging and Observability | **Accepted** | `tracing` for structured logging; no `println!` in library crates; secret redaction; Prometheus metrics endpoint. |
| [ADR-010](../specs/adr/010-async-runtime-safety.md) | Async Runtime Safety | **Accepted** | `tokio::sync::Mutex` in async contexts; `std::sync::Mutex` only in sync helpers with graceful poison handling; minimize lock scope. |
| [ADR-011](../specs/adr/011-single-node-controlplane.md) | Single-Node Control Plane with SQLite | **Accepted** | One control plane per cluster (~20 nodes), SQLite WAL as sole persistence, no HA/leader election. Running workloads survive CP outage via agent NodeCache and partition autonomy (ADR-006). DR via SQLite file backup. |
| [ADR-012](../specs/adr/012-disk-migration-precopy.md) | Block-Level Disk Migration (Pre-Copy) | **Accepted** | Live VM migration uses block-level pre-copy disk replication coordinated with Cloud Hypervisor's memory migration. Defines phases, convergence criteria, and rollback. |
| [ADR-013](../specs/adr/013-network-overlay-vxlan-ebpf.md) | Network Overlay (VXLAN + eBPF Policy) | **Accepted — partially superseded by ADR-021** | Multi-node networking uses VXLAN overlay with kernel datapath and eBPF for security policy. Extends ADR-005's host-side `chv-nwd` model with `TopologySpec`, `VtepEndpoint`, `SecurityPolicy`, `RateLimitPolicy`. |
| [ADR-014](../specs/adr/014-api-evolution.md) | API Evolution and Compatibility | **Accepted** | Proto/gRPC compatibility rules (no field-number reuse, additive evolution), `buf` lint/breaking CI gate, naming-rule deferrals tracked, pagination and error contract. |
| [ADR-015](../specs/adr/015-layered-ecosystem-compatibility.md) | Evidence-Driven Ecosystem Integration Strategy | **Proposed** | Time-boxes OpenStack discovery and leaves libvirt versus native adapter choices provisional. Defers CloudStack and OpenNebula until the Core authority and first OpenStack path are stable. |
| [ADR-016](../specs/adr/016-evolve-chv-agent-into-cellhv-core.md) | Evolve `chv-agent` into CellHV Core | **Accepted** | The existing `chv-agent` evolves in place into the standalone Core runtime. No parallel `cellhvd`, state store, operation engine, or VM authority is introduced. |
| [ADR-017](../specs/adr/017-core-compatibility-invariants.md) | Core Compatibility Invariants | **Accepted** | Locks native Core authority, Cloud Hypervisor as the Core 1.0 VMM, truthful VMM identity, independent compatibility axes, and evidence-backed support claims. Other VMMs are outside the active programme. |
| [ADR-018](../specs/adr/018-libvirt-delegation-over-core-api.md) | Optional Libvirt Delegation Over the CellHV Core API | **Proposed** | If productised, libvirt compatibility is an optional adapter outside `chv-agent` Core that uses only the versioned public Core API. `chv-agent` remains the sole lifecycle and Cloud Hypervisor process authority. |
| [ADR-019](../specs/adr/019-stable-core-host-identity-and-nodecache-authority.md) | Stable Core Host Identity and NodeCache Authority Modes | **Proposed** | A Core host keeps one opaque `HostId` resolved with strict precedence (existing Core DB, importable NodeCache, configured seed, generated UUID) and never replaced afterward. NodeCache authority modes freeze the VM-authoritative projection in Core mode. |
| [ADR-020](../specs/adr/020-per-vm-systemd-supervision-and-exclusivity.md) | Per-VM Systemd Supervision and Exclusivity | **Proposed** | Proposes per-VM systemd units as a supervision and exclusivity boundary for Cloud Hypervisor processes. Systemd supervises processes only; `chv-agent` retains all VM lifecycle authority. |
| [ADR-021](../specs/adr/021-stretched-l2-vxlan-her-wireguard-fabric.md) | Stretched-L2 Fabric (VXLAN Head-End Replication over WireGuard) | **Accepted** | Tenant networks behave as one literal L2 segment (shared VLAN) across all nodes: ARP/DHCP/BUM traverse via VXLAN head-end replication to enrolled peers, with kernel MAC learning; inter-site transport is an encrypted WireGuard mesh (`wg-chv` in a `chv-fabric` netns, per-host keys provisioned at enrollment). Partially supersedes ADR-013's `nolearning`/no-BUM and encryption-non-goal decisions; keeps ADR-013's eBPF policy role and proto surface. |
| [ADR-022](../specs/adr/022-dual-vm-boot-model.md) | Dual VM Boot Model: Firmware Boot and Direct Kernel Boot | **Proposed** | Defines explicit `firmware` and `direct_kernel` VM boot modes, content-addressed boot artifacts, runtime-only path resolution, no silent fallback, and separate real-KVM qualification for direct kernel boot. |
| [ADR-023](../specs/adr/023-netbox-projection.md) | NetBox Projection as a Bounded Downstream Inventory Adapter | **Proposed** | Projects applied CHV architectures into NetBox as a one-way, idempotent downstream copy via a `chv-netbox-adapter` crate in the management layer. Ownership is carried only by `chv_*` custom fields; foreign objects are never written; projection failure never affects apply results; no delete by default. |
| [ADR-024](../specs/adr/024-netbox-test-doubles-and-demo-gate.md) | NetBox Test Doubles and the Plain-HTTP Demo Gate | **Proposed** | Adds a dev-only stateful NetBox simulator (`chv-netbox-sim`) with golden fixtures and `__`-prefixed test-control endpoints; real-NetBox qualification runs the same scenarios on demand/weekly as a drift tripwire; the demo harness's plain-HTTP escape hatch is double-gated (compile feature + runtime env) and never shipped. |
| [ADR-025](../specs/adr/025-native-monitoring-architecture.md) | Native Monitoring Without a Mandatory Stack | **Proposed** | Read-only Core sampling, truthful metrics, native BFF dashboards, bounded monitoring storage; external monitoring remains optional. |
| [ADR-026](../specs/adr/026-optional-monitor-agent-and-guest-identity.md) | Optional Guest Monitoring Agent and Identity | **Proposed** | Opt-in Linux agent for guest OS checks, authenticated enrollment, bounded local plugins, and future vsock; no VM authority. |
| [ADR-027](../specs/adr/027-monitoring-history-alerts-and-export.md) | Monitoring Storage, Alerts, and Optional Export | **Proposed** | Isolated bounded time-series history, native alert state, secure notifications, and optional external metrics backend. |

## Web UI ADRs

| ADR | Title | Status | Summary |
|-----|-------|--------|---------|
| [ADR-001-WebUI](../specs/adr/001-webui-product-principles.md) | WebUI Product Principles | **Accepted** | 7 principles: cluster-first nav, tasks as first-class, legible state, no browser-to-node coupling, progressive depth, predictable mutation UX, private-cloud-first usability. |
| [ADR-002-WebUI](../specs/adr/002-webui-architecture-boundary.md) | WebUI Architecture Boundary | **Accepted** | Browser talks only to the control-plane backend-for-frontend (BFF). Direct access to `chv-agent`, `chv-stord`, `chv-nwd`, or Cloud Hypervisor APIs is forbidden. |
| [ADR-003-WebUI](../specs/adr/003-webui-navigation-model.md) | WebUI Navigation Model | **Superseded by ADR-006-WebUI** | Primary navigation hierarchy and detail-page tab structure. |
| [ADR-004-WebUI](../specs/adr/004-webui-task-and-state-model.md) | WebUI Task and State Model | **Accepted** | Tasks and state are first-class UI objects. Every mutation creates a task. Defines task states and resource health states. |
| [ADR-005-WebUI](../specs/adr/005-webui-design-system-direction.md) | WebUI Design System Direction | **Accepted** | Modern but restrained, enterprise-serious, light-mode first, high information density, border-first surfaces, strong typography. Avoids copying Proxmox/Xen Orchestra visually. |
| [ADR-006-WebUI](../specs/adr/006-webui-navigation-revised.md) | Revised WebUI Navigation Model (Cloud/Hosts/Instances) | **Accepted** | Supersedes ADR-003-WebUI. Operator-facing taxonomy: Default Cloud → Hosts → Instances. Renames Nodes→Hosts and Virtual Machines→Instances to match private/public cloud conventions. |

## Architecture Designer ADRs

These ADRs scope the Architecture Designer feature (design-time topology editor with YAML source-of-truth, validation, plan/apply via the CHV task system, and drift detection). All **Accepted** as of 2026-06-16 with the landing of Phase 7 (hardening).

| ADR | Title | Status | Summary |
|-----|-------|--------|---------|
| [ADR-001-Designer](../specs/adr/001-designer-first-class-surface.md) | Architecture Designer as First-Class Surface | **Accepted** | Adds a `DESIGN` group above Fleet Overview with `Architecture Designer` and `Saved Topologies` routes. Designer is the design-time view; Fleet Overview remains the operational view. |
| [ADR-002-Designer](../specs/adr/002-designer-svelte-flow.md) | Use Svelte Flow for the Editable Designer Canvas | **Accepted** | Adopts Svelte Flow for the editable canvas; existing CHV live-topology canvas stays as the read-only operational view. |
| [ADR-003-Designer](../specs/adr/003-designer-yaml-source-of-truth.md) | CHVArchitecture YAML as Source of Truth | **Accepted** | `CHVArchitecture` YAML (`apiVersion: chv.kubedo.io/v1alpha1`) is the authoritative serialized form; the graph is derived. Schema lives at [`docs/schemas/chvarchitecture-v1alpha1.schema.json`](../schemas/chvarchitecture-v1alpha1.schema.json). |
| [ADR-004-Designer](../specs/adr/004-designer-validation-plan-apply.md) | Validation-, Plan-, and Task-Gated Deployment | **Accepted** | Deployment is forbidden without static validation, fleet consistency check, and a confirmed plan. Apply runs through the existing CHV task system. |
| [ADR-005-Designer](../specs/adr/005-designer-separate-desired-vs-live.md) | Separate Desired Topology from Live Fleet Topology | **Accepted** | The Designer's desired-state graph is intentionally separate from live-fleet topology data. Drift detection compares baseline vs current. |
| [ADR-006-Designer](../specs/adr/006-designer-no-tosca-engine.md) | Do Not Adopt a Generic Cloudify/TOSCA Engine | **Accepted** | The Designer borrows the topology-orchestrator pattern but stays scoped to CHV-native virtualization resources. No generic TOSCA/Cloudify DSL compatibility. |

Feature documentation (contracts, prompts, research notes, READMEs) lives at [`docs/specs/architecture-designer/`](../specs/architecture-designer/). Component specs live alongside other component specs at [`docs/specs/component/architecture-designer-*.md`](../specs/component/). The phased implementation history is at [`docs/plans/2026-06-13-architecture-designer-implementation-plan.md`](../plans/2026-06-13-architecture-designer-implementation-plan.md).

## Lifecycle

```
PROPOSED → ACCEPTED → (SUPERSEDED by ADR-XXX or DEPRECATED)
```

Do not delete old ADRs. When a decision changes, write a new ADR that references and supersedes the old one.

## Naming Convention

ADRs use suffixes to disambiguate parallel namespaces with overlapping numbering:

- **Backend & Infrastructure ADRs** use no suffix (e.g., `ADR-001`).
- **Web UI ADRs** use the `-WebUI` suffix (e.g., `ADR-001-WebUI`).
- **Architecture Designer ADRs** use the `-Designer` suffix (e.g., `ADR-001-Designer`).

Cross-references to a non-backend ADR must include the suffix.

## Contributing

When making a significant architectural decision:

1. Write a new ADR following the documentation guidance in [`CONTRIBUTING.md`](../../CONTRIBUTING.md#documentation)
2. Store it in `docs/specs/adr/` with sequential numbering
3. Update this index
4. If the decision changes an existing ADR, mark the old one as `Superseded by ADR-XXX`
