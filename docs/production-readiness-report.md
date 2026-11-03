# CHV Production Readiness Report

**Date:** 2026-06-26  
**Phase:** 3 — Production Readiness & Observability (In Progress)  
**Review basis:** 6 parallel code reviews across Correctness, Security, Error Handling, Observability, Code Quality, and Architecture dimensions. 51 findings total: **11 P0 blockers**, **24 P1 required**, **16 P2 recommended**.

> **Status (2026-10-30): superseded for capability tier.**
> This report is dated 2026-06-26 and is stale in both directions. For what is
> supported today, use [DEPLOYMENT-ARCHITECTURE.md](DEPLOYMENT-ARCHITECTURE.md)
> §7 ("Supported envelope"). The original content below is preserved as a
> historical snapshot, not as a current verdict.
>
> **Fixed since this report (verified on this branch):** in-process
> `chv_observability::Metrics` counters for `chv-stord` and `chv-nwd` (each now
> serves a dedicated loopback `/metrics` listener, as do the control plane and
> agent; see [OBSERVABILITY.md](OBSERVABILITY.md)); gRPC streaming RED status
> now read from trailers instead of always `"unknown"`; migration-reaper resume of paused VMs;
> the `StorageMigrationService` implementation; DHCP `network_id`
> sanitization; and the drain gate on in-flight migrations.
>
> **Still open:** the backup execution engine (backup/restore is
> **hard-fenced** — every route answers `501 NOT_IMPLEMENTED`, the WebUI has
> no backup actions, and tenant-data protection is the operator's
> responsibility);
> multi-host (single-host is the declared posture; no TCP agent listener);
> Volvisor adoption (ADR-028 direction only; no code); and iSCSI/Ceph
> `destroy()` (explicitly unimplemented).

---

## Executive Summary

CHV has a solid foundation: VM lifecycle via desired-state reconciliation works end-to-end, mTLS node enrollment is correctly implemented, the Prometheus metrics surface is well-designed for the control-plane and agent layers, and RBAC middleware is correctly wired on all BFF route groups. The architecture is coherent and the ADRs are largely followed.

However, **five areas block production**. Two daemon crates (`chv-stord-core`, `chv-nwd-core`) emit zero Prometheus metrics despite owning the most operationally critical paths — disk I/O, migration block transfer, FDB reconciliation, and DHCP enforcement. This means a production operator would have no visibility into the system's two most complex subsystems. Drain evacuation has a race condition where a node can transition to Maintenance while migrations are still in-flight. The `CHV_ALLOW_INSECURE` bypass disables all mTLS peer-identity enforcement globally with no deployment guard. The migration reaper correctly times out stuck migrations in the DB but does not resume paused VMs, leaving them permanently paused after a migration failure. And `StorageMigrationService` is defined in proto but never generated or called — the dirty-sync protocol is architecturally incomplete.

The path to production is achievable and well-scoped. The P0s fall into three categories: observability gaps (instrument stord and nwd), correctness bugs (drain race, reaper resume, mutex poisoning), and security config hardening (insecure bypass guard). None requires a major redesign.

---

## Subsystem Verdicts

| Subsystem | Verdict | Key Rationale |
|-----------|---------|---------------|
| Core VM Lifecycle | **CONDITIONAL** | Happy path works; drain race condition (reconcile.rs:277) must be fixed before node maintenance operations are safe |
| Storage / Migration | **NO-GO** | Dirty-sync rounds unimplemented in stord; `StorageMigrationService` proto never generated; migration reaper doesn't resume paused VMs; zero metrics from stord-core |
| Networking (VXLAN/FDB) | **CONDITIONAL** | FDB lifecycle and VXLAN teardown complete; path-traversal in `dhcp.rs` (network_id unsanitized) must be fixed; zero metrics from nwd-core |
| Backup / DR | **NO-GO** | Execution engine absent; calling the backup API today has no effect beyond a DB record; no snapshot, no off-host shipping, no restore path |
| Auth / Security | **CONDITIONAL** | RBAC and JWT validation correct; `CHV_ALLOW_INSECURE` needs deployment guard; WebSocket scheme relies on unauthenticated `X-Forwarded-Proto` header |
| Observability | **NO-GO** | stord-core and nwd-core emit zero metrics; migration_id Prometheus label causes unbounded cardinality; deep health check only verifies socket existence not gRPC liveness |
| UI / BFF | **CONDITIONAL** | RBAC correct; migration progress not exposed to UI; several VM operations (PowerButton, AddDisk, AddNet, RemoveDevice) are gRPC-only with no BFF surface |

---

## P0 Blockers

Must be fixed before any production traffic.

### 1. stord-core and nwd-core emit zero Prometheus metrics
**Location:** `crates/chv-stord-core/src/`, `crates/chv-nwd-core/src/`  
These daemons own disk I/O, migration block transfer, FDB reconciliation, DHCP enforcement — the most operationally critical paths in the system. No `metrics::counter`, `metrics::gauge`, or `metrics::histogram` calls exist anywhere in either crate. A production operator cannot distinguish "healthy and idle" from "silently failing."  
**Fix:** Instrument the migration sender (bytes transferred, dirty rounds, backpressure events), storage backend operations (open, read, write, error by backend type), FDB reconcile (entries added/removed, errors), and DHCP scope operations.

### 2. Drain evacuation races to Maintenance before migrations complete
**Location:** `crates/chv-agent-core/src/reconcile.rs:277-349`  
The `Draining` arm of `run_once` collects `running_vms` by filtering `vm_runtime.list()` for `Running` or `Created` status. When this list is empty the node immediately transitions to `Maintenance`. However, a VM that has already been handed off to `chv-stord` for disk migration no longer appears as `Running` in `vm_runtime.list()`. The node can declare itself drained while the disk transfer is still in progress, allowing the host to be repurposed or powered off mid-migration.  
**Fix:** Gate the `Draining → Maintenance` transition on `migration_registry.is_empty()` in addition to no running VMs.

### 3. MigrationReaper marks migrations Failed but does not resume paused VMs
**Location:** `crates/chv-agent-core/src/reconcile.rs:1313-1331` (`create_one_vm`), migration reaper path  
The reaper correctly updates the DB record to `Failed` after 2 hours but does not send a resume signal to the source VM. A VM that was paused for final-dirty-flush and whose migration then stalls will remain permanently paused. The reaper also does not check whether the destination has a partially-written disk that needs cleanup.  
**Fix:** After marking a migration Failed, emit a `ResumeVm` command to the source node and trigger cleanup on the destination.

### 4. `migration_registry` `.expect()` on Mutex will panic on lock poisoning
**Location:** `crates/chv-agent-core/src/migration_registry.rs:60, 75, 87, 101, 114, 120`  
`MigrationRegistry` calls `.expect("migration_tasks mutex poisoned")` at six sites including `abort_all()`, which is called on agent shutdown. If any thread panics while holding the lock (e.g., during a migration abort) the agent process will panic on the next reconcile tick, taking down all VM management on that node. This violates ADR-008.  
**Fix:** Replace all `.expect()` calls with `.unwrap_or_else(|e| e.into_inner())` per ADR-008 guidance.

### 5. `CHV_ALLOW_INSECURE=1` disables all mTLS peer-identity enforcement globally with no deployment guard
**Location:** `crates/chv-controlplane-service/src/peer_identity.rs:54-91`  
When this env var is set, `PeerIdentityInterceptor` inserts `InsecurePeer` into every request extension and `verify_peer_matches` returns `Ok(())` unconditionally — any unauthenticated caller can reach any gRPC endpoint as any peer identity. There is no compile-time or startup-time check preventing this being set in a production deployment. This is a complete authentication bypass.  
**Fix:** Add a startup assertion that aborts with a clear error if `CHV_ALLOW_INSECURE` is set outside of `#[cfg(test)]` context, or gate it behind a `dev-only` Cargo feature that must be explicitly enabled at build time.

### 6. `dhcp::ensure_dhcp_scope` uses network_id in file paths without sanitization
**Location:** `crates/chv-nwd-core/src/dhcp.rs:8-18, 49-80`  
The private helpers `conf_path`, `pid_path`, and `hosts_path` call `PathBuf::from(RUNTIME_DIR).join(format!("dnsmasq-{}.conf", network_id))` without first passing `network_id` through the `sanitize_id` allowlist that the `executor.rs` functions use for nftables table names. A network_id containing `../` or shell metacharacters can write dnsmasq config files outside the runtime directory.  
**Fix:** Apply `sanitize_id` to `network_id` before constructing any file path in `dhcp.rs`, matching the pattern used in `executor.rs:382-384`.

### 7. `StorageMigrationService` proto defined but never generated or implemented
**Location:** `proto/node/chv-stord-migration.proto`, `gen/rust/`  
The `StorageMigrationService` with bidirectional-streaming `StreamBlocks` RPC — the entire inter-node disk transfer protocol described in ADR-012 — is defined in proto but absent from `gen/rust/`. The dirty-sync rounds, convergence reporting, and paused final flush that the roadmap marks as "partial" are partial because the underlying RPC transport doesn't exist. The current migration path uses an alternative mechanism that bypasses this service.  
**Fix:** Run `cargo build --workspace` to regenerate from proto, then implement `StreamBlocks` in `chv-stord-core` and wire it from the migration sender/receiver in `chv-agent-core`.

### 8. Deep health check verifies socket existence, not gRPC liveness
**Location:** `crates/chv-controlplane-service/src/api/health.rs:69-130`  
The deep health `agent_connectivity` check calls `tokio::net::UnixStream::connect` with a 2-second timeout. A successful socket connection is counted as pass. An agent process that crashed after creating the socket, or that is deadlocked in its reconcile loop, will pass the health check. This means load balancers and orchestrators that use `/deep-health` for readiness will route traffic to a degraded control plane.  
**Fix:** Replace the bare `UnixStream::connect` with a gRPC health check RPC call to the agent endpoint.

### 9. gRPC streaming responses always produce `grpc_status='unknown'` in RED metrics
**Location:** `crates/chv-observability/src/grpc_metrics.rs:149-170`  
The `GrpcMetricsLayer` intercepts response status from `http::Response` headers. For server-streaming RPCs (console, migration), the actual gRPC status code is in the trailers, not the headers. The middleware reads headers only, so every streaming response — including successful ones — emits `grpc_status='unknown'`. All serial-console and migration RED metrics are incorrectly classified.  
**Fix:** Inspect trailers using a `tower` response body wrapper to extract the `grpc-status` trailer after the stream completes.

### 10. Migration metrics use `migration_id` as a Prometheus label — unbounded cardinality
**Location:** `crates/chv-observability/src/lib.rs:131-167`  
`set_migration_phase`, `add_migration_bytes`, and `set_migration_dirty_blocks` use `migration_id` as a Prometheus gauge/counter label dimension. Each unique migration creates a new label combination that Prometheus retains indefinitely. In a system running hundreds of migrations per day this will OOM Prometheus within days.  
**Fix:** Remove `migration_id` from all metric labels. Use `vm_id` for bounded cardinality, or use `node_id + direction` for aggregate tracking.

### 11. Backup API surface exists but execution engine is absent — API calls silently no-op
**Location:** `crates/chv-controlplane-service/src/` (backup routes), `crates/chv-controlplane-store/src/`  
The backup schema and API surface exist, but the backup execution worker, VM/volume snapshot orchestration, off-host artifact shipping, restore validation, and retention enforcement are unimplemented. An operator calling the backup API today will receive a success response and a DB record with no actual backup artifact created. This is silent data loss under a disaster scenario.  
**Fix (short-term):** Return `501 Not Implemented` from all backup/restore endpoints until the execution engine exists. Document this clearly in the API.
**Resolved 2026-11-03 (#654):** the short-term fix was adopted as the posture — every route answers `501 NOT_IMPLEMENTED`, the UI surface is removed, and tenant-data backup is documented as the operator's responsibility. The engine itself remains unimplemented.

---

## P1 Required

Must be fixed before stable production. Listed by dimension.

### Error Handling

**VM metrics silently discarded on DB write failure**  
`crates/chv-controlplane-service/src/telemetry.rs:199-213` — `let _ = observed_state_repo.insert_vm_metrics()` discards any DB write failure silently. No log, no counter. Disk-full or SQLite-locked failures during metrics collection are invisible.

**`undrain_node` failure swallowed in upgrade rollback**  
`crates/chv-controlplane-service/src/upgrade.rs:254, 286, 342` — Three upgrade rollback branches use `let _ = self.upgrader.undrain_node(node_id).await`. If undrain fails the node remains drained (no VMs scheduled) and the operator receives no indication. Replace with `warn!` logging and a counter increment at minimum.

**VM create leaves orphaned disk on agent-side failure**  
`crates/chv-agent-core/src/reconcile.rs` (`create_one_vm`) — If the Cloud Hypervisor API call succeeds but the subsequent `vm_runtime.register()` call fails, the VM process is running but unregistered. The next reconcile tick will attempt to create it again, resulting in a second CHV process for the same VM.

**Error context missing operation IDs in storage paths**  
`crates/chv-stord-core/src/` — Storage errors propagate without `vm_id`, `volume_id`, or `migration_id` context. In a multi-tenant environment, diagnosing "I/O error during migration" requires correlating timestamps across logs.

### Observability

**Reconciler has no staleness/stuck metric**  
`crates/chv-agent-core/src/reconcile.rs` — The reconciler emits `reconcile_ticks` and `reconcile_failures` but no "time since last successful reconcile." An agent that processes reconcile ticks but makes no progress (e.g., stuck waiting on a CHV response) is invisible to alerting.

**Agent health endpoint not implemented — `/deep-health` has no agent-side equivalent**  
The control plane's deep health check cannot actually verify agent liveness beyond a socket connection (see P0 #8 above). The agent has no `/health` gRPC endpoint to call against.

**Migration progress not exposed via BFF or UI**  
`proto/webui/webui-bff.proto`, `crates/chv-webui-bff/src/router.rs` — The control plane tracks `phase`, `convergence_round`, `dirty_blocks_remaining`, and `bytes_transferred` in the migrations table. None of this is exposed via the BFF. Operators cannot monitor live migrations from the UI.

### Architecture / API Contracts

**MutateVm BFF omits 9 lifecycle operations — gRPC-only**  
`crates/chv-controlplane-service/src/bff_mutations.rs` — `PowerButtonVm`, `CoredumpVm`, `AddDisk`, `AddNet`, `RemoveDevice`, `ResizeDisk`, `ResizeVm`, `RestoreSnapshot`, `SnapshotVm` are defined in the proto but have no BFF route. Web UI users cannot perform these operations.

**Single-node control plane not enforced at runtime (ADR-011)**  
`crates/chv-controlplane-service/src/` — ADR-011 states no HA, no multi-instance. The code has no flock, no pidfile, no SQLite `PRAGMA locking_mode=EXCLUSIVE`. Two control plane processes starting against the same SQLite DB will corrupt state without error.

**API versioning: `/v1/` prefix exists but no negotiation**  
`crates/chv-webui-bff/src/router.rs` — The BFF has a `/v1/` path prefix but no `API-Version` header, no version negotiation, and no documented compatibility contract. Any client built against the current surface will break silently on future changes.

**`BackupService` proto RPCs unimplemented in service**  
`proto/controlplane/` — Backup-related RPCs defined in proto return `Unimplemented` from the service layer. Clients calling these via gRPC directly will get misleading errors.

### Security

**WebSocket scheme determined by unauthenticated `X-Forwarded-Proto` header**  
`crates/chv-webui-bff/src/` (console route) — The serial console WebSocket upgrade uses the `X-Forwarded-Proto` header to decide `ws://` vs `wss://`. An attacker who can inject this header (e.g., through a misconfigured reverse proxy) can downgrade the WebSocket connection to plaintext.  
**Fix:** Hard-code `wss://` or derive the scheme from the TLS termination state, not a request header.

**Bootstrap token not rate-limited on enrollment endpoint**  
`crates/chv-controlplane-service/src/` (enrollment) — The enrollment endpoint accepts bootstrap tokens without rate limiting. A stolen token can be brute-forced if the token space is small, or an attacker can repeatedly attempt enrollment.

**JWT `sub` claim not validated against a known identity set**  
`crates/chv-webui-bff/src/` — JWT signature and expiry are validated, but the `sub` claim is not checked against an allowlist or revocation store. A valid token for a deleted user continues to work until expiry.

### Code Quality

**`create_one_vm` has no resource cleanup on partial failure**  
`crates/chv-agent-core/src/reconcile.rs:1313-1331` — The VM creation sequence: allocate volume → start CHV process → register. If registration fails after CHV starts, there is no cleanup path. (Also noted in error handling above.)

**`InventoryListPage` and several UI components use `any[]`**  
`ui/src/routes/` — `InventoryListPage` uses `any[]` for node and VM lists, defeating TypeScript's type safety and masking structural mismatches between API responses and component expectations.

**`TopResourceConsumers` accesses `node_id` via `(vm as any).node_id`**  
`ui/src/lib/components/shell/TopResourceConsumers.svelte:29, 41` — The VM type from `$lib/api/types` does not declare `node_id`. This field is cast through `any`, meaning a type-breaking API change would be invisible at compile time.

---

## P2 Recommended

Not blocking production launch but should be addressed for long-term health.

| # | Area | Finding | Location |
|---|------|---------|----------|
| 1 | UI | `ImagesTable` casts `ImageRow` to `any` for `is_template` and dynamic column access | `ui/src/lib/components/images/ImagesTable.svelte:64-71` |
| 2 | UI | `vms/[id]/+page.svelte` and `CreateVMModal.svelte` exceed 300-line component limit | `ui/src/routes/vms/[id]/+page.svelte`, `ui/src/lib/components/vms/CreateVMModal.svelte` |
| 3 | UI | Tailwind-first migration incomplete — mixed CSS approach increases style maintenance surface | `ui/src/` |
| 4 | UI | Command palette not implemented — documented as P2 roadmap item | `PHASED_IMPLEMENTATION_PLAN.md` |
| 5 | UI | DataTable/Overview refactor pending | `ui/src/lib/components/` |
| 6 | Backend | iSCSI and Ceph RBD adapters integrated but not production-validated against external arrays | `crates/chv-stord-backends/src/{iscsi,ceph}.rs` |
| 7 | Backend | iSCSI multi-portal failover not implemented | `crates/chv-stord-backends/src/iscsi.rs` |
| 8 | Backend | Ceph RBD locking edge cases unhandled under concurrent access | `crates/chv-stord-backends/src/ceph.rs` |
| 9 | Observability | `reconcile_drift_ema` metric has no corresponding alert definition | `crates/chv-observability/src/lib.rs` |
| 10 | Observability | Circuit breaker state transitions not emitted as metrics | `crates/chv-controlplane-service/src/` |
| 11 | Architecture | eBPF policy enforcement scoped to rate limiting only — network policy enforcement gap is undocumented | ADR-013 |
| 12 | Architecture | Upgrade rollback failure leaves node in ambiguous state — no reconciliation of partially-upgraded nodes | `crates/chv-controlplane-service/src/upgrade.rs` |
| 13 | Security | Session tokens not invalidated on logout — JWT expiry is the only revocation mechanism | `crates/chv-webui-bff/src/auth.rs` |
| 14 | Security | Prometheus `/metrics` endpoint has no authentication — exposes internal system topology | `crates/chv-observability/src/` |
| 15 | Error Handling | `chv-nwd-core` nftables executor doesn't distinguish transient vs. permanent failures — all errors trigger the same retry path | `crates/chv-nwd-core/src/executor.rs` |
| 16 | Error Handling | Reconciler does not emit an event log entry when a VM fails to start — operator has no audit trail | `crates/chv-agent-core/src/reconcile.rs` |

---

## Per-Dimension Findings Summary

### Correctness & Business Logic
9 findings (4 P0, 3 P1, 2 P2). The drain evacuation race (P0) and migration reaper's failure to resume paused VMs (P0) are the two highest-risk correctness issues. The control-plane and agent reconciler state machines are otherwise correct. The dirty-sync protocol gap is architectural (missing StorageMigrationService implementation) rather than a logic bug.

### Security
9 findings (2 P0, 4 P1, 3 P2). The `CHV_ALLOW_INSECURE` bypass (P0) and `dhcp.rs` path traversal (P0) are the blocking items. RBAC, JWT validation, and bootstrap token handling are correctly implemented. The WebSocket scheme header issue and missing JWT revocation are P1.

### Silent Failures & Error Handling
8 findings (2 P0, 4 P1, 2 P2). The mutex poisoning panic (P0) and silent backup no-op (P0) are the blockers. The pattern of `let _ =` silencing errors on DB writes in telemetry and upgrade paths is the dominant P1 theme — these need `warn!` logging and metric counters at minimum.

### Observability
9 findings (3 P0, 3 P1, 3 P2). Three P0s: zero metrics from stord-core and nwd-core, and unbounded Prometheus cardinality from migration_id labels. The gRPC streaming metric classification bug (grpc_status='unknown') is also P0. The control-plane and agent reconciler observability is solid.

### Code Quality & Type Safety
8 findings (0 P0, 5 P1, 3 P2). No P0 blockers. The main theme is TypeScript `any` casts in UI components that mask API shape mismatches. The `dhcp.rs` path traversal (classified under Security/P0) originated from this review. The Rust type system is used correctly in the backend domain model.

### Architecture & API Contracts
8 findings (0 P0, 5 P1, 3 P2). No new P0s. The `StorageMigrationService` proto gap (P0) was already captured in Correctness. The missing BFF surface for 9 lifecycle operations and the absent single-node enforcement (ADR-011) are the key P1s.

---

## Gap Verification Table

| Documented Gap | Roadmap Claim | Code Reality | Status |
|----------------|---------------|--------------|--------|
| Disk migration dirty sync / final flush | PARTIAL — protocol messages exist | `StorageMigrationService` not generated; stord has no dirty-round loop | **Worse than documented** |
| Drain evacuation | COMPLETE | Race: transitions to Maintenance before migrations finish | **Bug found** |
| MigrationReaper | COMPLETE | Marks DB Failed but does not resume paused source VM | **Incomplete** |
| stord observability | Not in roadmap | Zero metrics emitted from entire crate | **Gap not documented** |
| nwd observability | Not in roadmap | Zero metrics emitted from entire crate | **Gap not documented** |
| Backup/DR execution | PARTIAL — SQLite backup only | API returns success with no artifact; should return 501 | **Confirmed** |
| RBAC middleware | COMPLETE | Correctly implemented on all route groups | **Confirmed** |
| mTLS enforcement | COMPLETE | Correct; `CHV_ALLOW_INSECURE` bypass needs deployment guard | **Needs hardening** |
| Circuit breaker | COMPLETE | Implemented; state transitions not metriced | **Mostly confirmed** |
| Deep health checks | COMPLETE | Socket connection only, not gRPC liveness | **Shallower than claimed** |
| FDB cleanup on VM detach | COMPLETE | Confirmed correct | **Confirmed** |
| VXLAN teardown | COMPLETE | Confirmed correct | **Confirmed** |
| Rolling upgrade orchestration | COMPLETE | Correct; rollback undrain swallows errors | **Minor gap** |
| iSCSI and Ceph backends | PARTIAL | Integrated, unit-tested, not field-validated | **Confirmed** |
| UI `any[]` types | PARTIAL | Multiple components affected beyond InventoryListPage | **Broader than documented** |

---

## Path to Production

Minimum ordered work to reach production. Effort: S = days, M = 1-2 weeks, L = 3+ weeks.

| # | Item | Effort | Blocks |
|---|------|--------|--------|
| 1 | Fix drain evacuation race (gate on migration_registry.is_empty()) | S | Core VM Lifecycle |
| 2 | Fix MigrationReaper: resume paused source VM on timeout | S | Storage/Migration |
| 3 | Fix mutex poisoning in migration_registry | S | Error Handling |
| 4 | Add deployment guard on CHV_ALLOW_INSECURE | S | Security |
| 5 | Apply sanitize_id to dhcp.rs file paths | S | Security |
| 6 | Make backup endpoints return 501 until implemented | S | Backup/DR |
| 7 | Fix deep health check to use gRPC health RPC | S | Observability |
| 8 | Fix gRPC streaming metrics to read trailers | S | Observability |
| 9 | Remove migration_id from Prometheus labels | S | Observability |
| 10 | Instrument chv-stord-core with basic metrics | M | Observability |
| 11 | Instrument chv-nwd-core with basic metrics | M | Observability |
| 12 | Implement StorageMigrationService in stord; wire dirty-sync rounds | L | Storage/Migration |
| 13 | Add let _ = → warn! for telemetry and upgrade rollback silent failures | S | Error Handling |
| 14 | Expose migration progress via BFF + UI | M | UI/BFF |
| 15 | Enforce single-node control plane (pidfile or SQLite exclusive lock) | S | Architecture |
| 16 | Fix WebSocket scheme header (use TLS state not X-Forwarded-Proto) | S | Security |

Items 1–11 are achievable in 1–2 sprints and unlock GO verdicts for Core VM Lifecycle, Networking, Auth/Security, and Observability. Items 12–16 are the second sprint and are required before the Storage/Migration subsystem can be production-approved. Backup/DR (full execution engine) is a third sprint and is not required for an initial limited production rollout if VMs are protected by external snapshot tooling.
