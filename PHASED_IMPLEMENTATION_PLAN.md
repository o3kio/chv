# CHV Phased Implementation Plan

> **Status: historical roadmap, not a capability claim.** This document records
> sprint-era intent. Delivered/partial statements can lag the code; the
> authoritative supported envelope is
> [`docs/DEPLOYMENT-ARCHITECTURE.md`](docs/DEPLOYMENT-ARCHITECTURE.md) §7 with
> the capability table in §9. Stale entries that were verified against the code
> are corrected inline below.

Based on the comprehensive repository review and existing roadmap, the following is a detailed, phased implementation plan to bring the CHV platform to production readiness.

## Current Status Summary

As of Sprint 15 (PRs 49-52), the platform has transitioned from early-MVP toward stability, but several production-readiness areas remain partial. The high-level VM lifecycle, node enrollment, desired-state reconciliation, and base storage/network daemons are usable; live migration, overlay policy enforcement, storage backends, and Backup/DR still have important gaps.

**Delivered in recent sprints:**
- Compatibility matrix boot gate (version validation before upgrade)
- mTLS enforcement on storage migration (plaintext rejected)
- Backpressure handling in migration sender
- Drain evacuation path (node drain issues migration requests; full safety still depends on migration gaps below)
- Partition reconnect flush (pending messages flushed on reconnect)
- `chv-nwd` kernel VXLAN/FDB lifecycle work (policy eBPF remains enforcement scope only, not a VXLAN datapath)
- FDB cleanup on VM detach
- VXLAN teardown on topology delete
- MigrationReaper (auto-fails stalled migrations after a no-progress timeout; not a total-runtime cap)
- Circuit breaker on node communication
- Deep health checks (database, agent socket, agent connectivity)
- Hypervisor trait decoupling (`chv-hypervisor-api` crate, `HypervisorAdapter` interface, `chv-agent-core` isolated from Cloud Hypervisor runtime)
- Dev-install teardown & reinstall hardening (SQLite WAL file cleanup, mTLS cert reset on `--wipe`, daemon socket mode `0666`, systemd `StateDirectoryMode=0750`, `0750` directory validation in `cellhv-core-*` crates)

**Known partials that remain production-relevant:**
- Disk migration orchestration is partial: control-plane phases, reaper, flow control, and mTLS exist, but dirty sync rounds, convergence reporting from stord, and paused final dirty flush remain incomplete.
- `chv-nwd` scope is kernel VXLAN plus explicit FDB management; eBPF is for policy/rate limiting only and is not the overlay datapath.
- `chv-stord` ships local file, local block/LVM, iSCSI, and Ceph RBD adapters (`crates/chv-stord-backends/src/{local,lvm,iscsi,ceph}.rs`) implementing the `StorageBackend` trait. iSCSI and Ceph adapters are integrated and unit-tested but have not yet been validated against production-grade external arrays/clusters; field hardening (multi-portal failover, RBD locking edge cases, performance tuning) is the remaining work.
- Backup/DR has SQLite pre-migration backup and schema/API surfaces, but still lacks the VM/volume backup execution engine, off-host shipping, restore validation, and operator-run DR automation.

---

## Phase 1: Stability, Bug Fixes & Agent Parity [COMPLETE]

**Goal**: Ensure the foundation is solid, fix failing tests, and replace mock agent implementations with real daemon calls.

### 1.1 UI & Frontend Fixes [COMPLETE]
*   ~~Fix `Ctrl+K` Command Palette Test~~
*   ~~Clean Up Unused CSS~~
*   ~~Fix A11y Warnings~~

### 1.2 Agent Daemon Integration (Testing Mocks & Coverage) [COMPLETE]
*   ~~`chv-stord` and `chv-nwd` Mock Completion~~
*   ~~Implement missing mock logic for storage RPCs~~
*   ~~Implement missing mock logic for network RPCs~~

---

## Phase 2: Feature Completion (Orchestration, Quotas, & Networking) [COMPLETE]

**Goal**: Fulfill the remaining core features outlined in the gap analysis.

### 2.1 Backend / BFF Completion [COMPLETE]
*   ~~Hypervisor Orchestrator Merge (`B5`)~~
*   ~~Quota Enforcement (`B3`)~~
*   ~~Agent API Token Auth (`B10`)~~
*   ~~Metadata Propagation (`A6`)~~

### 2.2 Storage & Network Daemons [PARTIAL]
*   ~~`chv-nwd` Linux Executor (`A8`)~~
*   ~~Storage Pool Provisioning (`B7`)~~
*   ~~Image Import Validation (`B8`)~~
*   **iSCSI and Ceph RBD backends**: planned by the storage datapath ADR/component spec, but not complete production adapters.

---

## Phase 3: Production Readiness & Observability [IN PROGRESS]

**Goal**: Prepare the system for long-term maintainability, backup strategies, and scale.

### 3.1 UI Production Refactor (`U1`, `U6`, `U7`) [PARTIAL]
*   ~~**Component Reorganization**: Reorganized `ui/src/lib/components` into 10 feature-based folders (`vms/`, `nodes/`, `networks/`, `storage/`, `settings/`, `tasks/`, `events/`, `shell/`, `primitives/`, `shared/`) with barrel exports.~~
*   ~~**DataTable Refactoring**: Extracted `Selection`, `Sorting`, and `Visibility` into `shared/datatable/` sub-modules.~~
*   ~~**Overview Logic Extraction**: Extracted `dashboard.ts` helpers and `dashboard.svelte.ts` store; reduced `+page.svelte` from 635 → 292 lines.~~
*   ~~**Tailwind-first Migration**: Design system fully aligned with Tailwind config; `app.css` uses design tokens. Dark mode implemented with `[data-theme="dark"]` tokens.~~
*   **Remaining**: Some components still exceed 300 lines (`vms/[id]/+page.svelte` at 467 lines, `CreateVMModal.svelte` at 580 lines). Full Tailwind-first purge of legacy CSS selectors not yet complete.

### 3.2 Backups & Role-Based Access [PARTIAL]
*   **Backup/DR (`B2`)**: **Decision (2026-11-03): fenced, not implemented for
    the pre-production gate.** The backup/restore API and WebUI surface are
    hard-disabled (`501 NOT_IMPLEMENTED`); tenant-data protection is the
    operator's responsibility. Implementing the execution worker, off-host
    artifact shipping, restore validation, retention, and documented DR
    automation remains post-gate work. SQLite pre-migration backup exists, but
    it is not a full Backup/DR system.
*   ~~RBAC Middleware (`B6`)~~: Role-based access on BFF routes implemented.

### 3.3 CI/CD & Deployments (`I3`, `I4`, `I5`, `I6`) [COMPLETE]
*   ~~GitHub Actions~~: CI set up.
*   ~~DB Migration Backups~~: Pre-migration SQLite backup implemented.
*   ~~Nginx Routing~~: WebSocket routing configured.
*   ~~Versioning~~: Release versioning automated via `VERSION` file + Makefile.

### 3.4 Production Gaps (Sprints 11-15) [PARTIAL]
*   **Rolling upgrade orchestration**: **not delivered.** `UpgradeOrchestrator`
    and `SystemdNodeUpgrader` do not exist in the tree; the earlier "delivered"
    entry was stale.
*   ~~Compatibility matrix enforcement~~ (boot gate rejects incompatible versions)
*   ~~mTLS on storage migration~~ (plaintext connections rejected)
*   ~~Backpressure handling~~ (sender throttles based on receiver feedback)
*   **Disk migration dirty sync/final flush**: partial; protocol messages and orchestration exist, but stord does not yet perform dirty rounds or paused final dirty flush.
*   **eBPF policy enforcement**: scoped to policy/rate limiting. Kernel VXLAN remains the overlay datapath.
*   ~~FDB cleanup on VM detach~~ (forwarding database entries cleaned)
*   ~~Drain evacuation~~ (Draining state triggers VM migration requests)
*   ~~Partition reconnect flush~~ (agent flushes pending messages on reconnect)
*   ~~VXLAN teardown~~ (delete_topology cleans up VXLAN interfaces)
*   ~~MigrationReaper~~ (background task times out stalled migrations)
*   ~~Circuit breaker~~ (protects against cascading failures in node communication)
*   ~~Deep health checks~~ (component-level: database, agent socket, agent connectivity)

---

## Success Criteria
1.  ~~All Playwright E2E tests pass reliably.~~
2.  ~~`svelte-check` reports 0 errors and 0 warnings.~~
3.  ~~All `unimplemented!()` stubs in the agent `reconcile.rs` are replaced with real daemon clients.~~
4.  ~~No `BffError::NotImplemented` returned from any frontend API call.~~
5.  CI pipeline is green; automated Backup/DR execution is **PARTIAL** and limited to SQLite pre-migration backup today.
6.  All production-gap items (P0, P1) resolved — **PARTIAL** until disk migration dirty sync/final flush is complete.

## Remaining Work

| Area | Item | Priority |
|------|------|----------|
| UI | Tailwind-first migration | P2 |
| UI | Command palette | P2 |
| Backend | Backup/DR job implementation | P2 |
| Backend | Disk migration dirty sync rounds, convergence reporting, and paused final dirty flush | P1 |
| Backend | iSCSI and Ceph RBD backend adapters | P2 |
| UI | DataTable / Overview refactor | P3 |
