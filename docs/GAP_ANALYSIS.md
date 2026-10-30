# CHV Specification vs Implementation Gap Analysis

**Date:** 2026-06-02  
**Version:** 0.1.0  
**Scope:** Backend (Rust), Agent, UI (SvelteKit), Infrastructure  
**Method:** Cross-reference of ADRs 001–013, component specs, ARCHITECTURE.md, DESIGN.md, and PHASED_IMPLEMENTATION_PLAN.md against the actual codebase.

---

## Correction (2026-10-30) — superseded for capability claims

**This document is a historical analysis dated 2026-06-02 (v0.1.0).** Its
"zero gaps across all categories" executive summary is **not current and must
not be used for capability claims.** Effective 2026-10-30, capability claims
are owned by [DEPLOYMENT-ARCHITECTURE.md](DEPLOYMENT-ARCHITECTURE.md) §7
("Supported envelope"). The rest of this file is preserved unchanged as
historical analysis as of its original date.

Open gaps as of 2026-10-30 (verified against this branch):

| Area | Honest status | Evidence |
|---|---|---|
| Backup / restore | **Excluded from the supported matrix.** There is no execution engine: `execute_job` always fails with "backup execution engine not yet implemented". The ship/fetch path was removed in #213; the shipper retains only `delete`. | [DEPLOYMENT-ARCHITECTURE.md](DEPLOYMENT-ARCHITECTURE.md) §7, §5 UC-9; `crates/chv-controlplane-service/src/backup_worker.rs` (`execute_job`); `crates/chv-controlplane-service/src/backup_shipper.rs` (`BackupShipper::delete` only) |
| Multi-host | **Architecturally impossible as coded.** Dispatch resolves a local filesystem `agent_socket_pattern` path and connects with `UnixStream::connect`; the agent serves gRPC from a `UnixListener` only (no TCP gRPC listener). Single-host is the declared posture for now. | [DEPLOYMENT-ARCHITECTURE.md](DEPLOYMENT-ARCHITECTURE.md) §3.3, §5 UC-6, §8 D1; `crates/chv-controlplane-service/src/migration.rs` (`resolve_agent_socket`); `crates/chv-controlplane-service/src/node_client.rs` (`UnixStream::connect`); `crates/chv-agent-core/src/agent_server.rs` (`UnixListener::bind`) |
| Volvisor storage backend | **Design direction only.** Accepted by ADR-028 and the 2026-10-13 adoption plan, but there are **zero** `volvisor` references in `crates/`, `cmd/`, `proto/`, or `Cargo.toml`; storage is still `chv-stord`. | [ADR-028](specs/adr/028-volvisor-storage-orchestration.md); [adoption plan](plans/2026-10-13-volvisor-storage-adoption.md); `grep -rni volvisor crates cmd proto Cargo.toml` → no matches |
| iSCSI / Ceph RBD | **Coded but unqualified; `destroy()` is explicitly unimplemented** and refuses loudly. | [DEPLOYMENT-ARCHITECTURE.md](DEPLOYMENT-ARCHITECTURE.md) §7; `crates/chv-stord-backends/src/iscsi.rs`; `crates/chv-stord-backends/src/ceph.rs` |
| BFF / architecture stubs | **Stubs remain.** `list_architecture_versions` returns `NotImplemented("phase 0")`. | `crates/chv-webui-bff/src/handlers/architectures.rs` |
| Docker Compose | **Not a supported install path.** The image builds, but the compose stacks do not run end-to-end (no TLS/mTLS provisioning, no bootstrap token; the control plane fails closed). Use packages or `scripts/install.sh`. | [DEPLOYMENT-ARCHITECTURE.md](DEPLOYMENT-ARCHITECTURE.md) §10; `docker-compose.yml`, `docker-compose.prod.yml` banners |
| Hardware minimums | **No qualified minimum.** "4 cores / 8 GB / 50 GB" is an unevidenced recommendation. | [DEPLOYMENT-ARCHITECTURE.md](DEPLOYMENT-ARCHITECTURE.md) §7; [DEPLOYMENT.md](DEPLOYMENT.md) |

**Consequences for the sections below:** the "resolved" entries in §1–§5 and
Appendix A reflect the 2026-06-02/04 codebase. Where they conflict with the
table above or with `DEPLOYMENT-ARCHITECTURE.md` §7, the latter wins. In
particular, §1.2 (backup engine "complete"), §1.3 (iSCSI/Ceph "resolved"), and
§4.2 (Docker Compose "resolved") are no longer accurate.

---

## Executive Summary

> **Historical (2026-06-02):** the zero-gap table below is superseded — see
> [Correction](#correction-2026-10-30--superseded-for-capability-claims) above.

| Category | Total Gaps | P0 | P1 | P2 | P3 |
|----------|-----------|----|----|----|----|
| Backend / Control Plane | 0 | 0 | 0 | 0 | 0 |
| Agent / Node Runtime | 0 | 0 | 0 | 0 | 0 |
| UI / Web Frontend | 0 | 0 | 0 | 0 | 0 |
| Infrastructure / Deployment | 0 | 0 | 0 | 0 | 0 |
| **Total** | **0** | **0** | **0** | **0** | **0** |

**Previously reported gaps that are now resolved:**
- Partition policy (ADR-006) is fully implemented via `ConnectivityTracker`, the per-tick pending-queue drain (`drain_pending_control_plane_queue`), and agent-side RPC rejection.
- VM resize is wired end-to-end (backend-for-frontend (BFF) → desired state → agent reconcile → Cloud Hypervisor (the VMM)).
- Network mutations (`start`/`stop`/`restart`) are wired through BFF → control plane → agent → `chv-nwd`.
- svelte-check reports **0 errors and 0 warnings**.
- Playwright E2E tests run in CI (`.github/workflows/ci.yml` `e2e` job).
- Toast component uses design-system CSS variables (no hardcoded hex).
- TopologyCanvas and settings/users page are under 300 lines after refactor; `SidebarNav.svelte` has since regressed above 300 (see §3.1 re-check).
- There are **zero** `unimplemented!()` stubs in production Rust code.
- **`awaiting-operator-input` task state** implemented in `OperationStatus`, UI task list, and migration reaper exclusion.
- **Disk migration dirty sync + final flush** implemented end-to-end: `chv-stord` `TriggerDiskMigration`/`GetDiskMigrationStatus`/`ResumeDiskMigration` RPCs, `MigrationTaskTable` with VM-pause coordination, agent progress polling and reporting to control plane.
- **chv-stord security hardening** implemented: dedicated `chv-stord` service account, path/device allowlists, capability dropping in systemd, socket `chown` to the `chv-stord` group.
- **Multi-node WebSocket routing** implemented: nginx `map`-based dynamic upstream by `node_id`, BFF proxied-mode URL generation, documented in `DEPLOYMENT.md`.

---

## Legend

- **P0** — Safety or data-loss risk; blocks production usage
- **P1** — Required for MVP completeness; user-facing broken promise
- **P2** — Quality / maintainability; degrades operator experience
- **P3** — Nice to have; future enhancement
- **Evidence** — File path and line number where the gap is observable

---

## 1. Backend / Control Plane Gaps

### 1.1 Disk Migration Dirty Sync and Final Flush — ✅ RESOLVED 2026-06-02
- **Spec:** ADR-012, `chv-stord-spec.md`, live-migration-spec.md
- **Resolution:** Implemented end-to-end with `chv-stord` `TriggerDiskMigration`/`GetDiskMigrationStatus`/`ResumeDiskMigration` RPCs. `MigrationTaskTable` coordinates VM pauses via `tokio::sync::watch`. The agent polls progress every 5s. It reports progress to the control plane via the `PendingControlPlaneMessage` queue.
- **Evidence:** `proto/node/chv-stord-api.proto`, `crates/chv-stord-core/src/migration/task.rs`, `crates/chv-stord-core/src/handlers.rs:844-1067`, `crates/chv-agent-core/src/migration.rs:229-410`, `crates/chv-agent-core/src/daemon_clients.rs:422-520`
- **Priority:** P1

### 1.2 Backup Jobs: Execution Engine Complete — Restore/DR Runbooks Pending
- **Spec:** ARCHITECTURE.md, PHASED_IMPLEMENTATION_PLAN.md Phase 3
- **Status:** Core execution engine resolved (2026-06-03). Backup shipper trait (Null, NFS, S3 with streaming upload) wired into `BackupWorker`. VM-level snapshots are staged and shipped to remote destinations. DB records gain checksum, size, remote path, and storage backend. `retention_days` enforcement works alongside `retention_count` pruning. Remote artifact deletion uses `job.destination` (original URL) to build the shipper and `job.target_path` (shipped key/path) for the delete. S3 credentials are configurable per-schedule. 9 unit tests cover shipper implementations.

  **Post-implementation review fixes (2026-06-03):**
  - Race condition in scheduled job creation eliminated via optimistic locking (`try_claim_schedule_run`)
  - Count-based retention pruning now cleans up remote artifacts before deleting DB rows (`list_old_jobs_for_count_retention` + `delete_jobs_by_ids`)
  - S3 credentials encrypted at rest with AES-256-GCM (`CredentialEncryption`), key from `CHV_ENCRYPTION_KEY` or `CHV_JWT_SECRET`
  - BFF duplicate JSON keys removed; destination tracking preserved across job creation and re-run paths

  Remaining gaps: volume-level `snapshot_volume` shipping (requires agent protocol changes). Restore execution/validation remains a future feature (no restore worker yet). DR runbooks now document all current manual and automated restore paths.
- **Evidence:** `crates/chv-controlplane-service/src/backup_shipper.rs`, `crates/chv-controlplane-service/src/backup_worker.rs`, `crates/chv-controlplane-store/src/backups.rs`, `crates/chv-controlplane-store/src/credential_crypto.rs`, `cmd/chv-controlplane/migrations/0043_backup_destination_and_credentials.sql`
- **Priority:** P2

### 1.3 iSCSI and Ceph RBD Storage Backend Adapters — ✅ RESOLVED 2026-06-03
- **Spec:** ADR-004, `chv-stord-spec.md`
- **Resolution:** Both backends implement the full `StorageBackend` trait (open, close, attach, detach, health, resize, snapshot/clone preparation, restore/delete snapshot, device policy, read/write block, volume size, dirty tracking). They are selectable at runtime in `cmd/chv-stord/src/main.rs` via `backend_type = "iscsi"` or `backend_type = "ceph"` with corresponding config sections. Config parsing (`chv-config`) supports `StordIscsiConfig` and `StordCephConfig`. The generic `StorageServer<B>` and `StorageMigrationServiceImpl<B>` work with `Box<dyn StorageBackend>` via the blanket impl. All backends compile, pass clippy, and have unit tests.
- **Evidence:** `crates/chv-stord-backends/src/iscsi.rs`, `crates/chv-stord-backends/src/ceph.rs`, `cmd/chv-stord/src/main.rs:43-77`, `crates/chv-config/src/lib.rs:237-242`
- **Priority:** P2

---

## 2. Agent / Node Runtime Gaps

### 2.1 chv-stord-spec Security Requirements — ✅ RESOLVED 2026-06-02
- **Spec:** `chv-stord-spec.md` (dedicated service account, restricted socket permissions, explicit device/path allowlists, capability drop)
- **Resolution:** Dedicated `chv-stord` system user/group created in `install.sh` and `postinstall.sh`. `path_allowlist` and `device_allowlist` enforced in `StorageServiceImpl::open_volume`. Systemd service updated with `CapabilityBoundingSet=CAP_SYS_ADMIN CAP_MKNOD CAP_DAC_OVERRIDE`, `RestrictAddressFamilies=AF_UNIX`, `RestrictSUIDSGID=true`, and socket `chown` to the `chv-stord` group.
- **Evidence:** `scripts/install.sh:203-235`, `packaging/scripts/postinstall.sh:15-40`, `docs/examples/systemd/chv-stord.service`, `crates/chv-stord-core/src/handlers.rs:108-213`
- **Priority:** P1

---

## 3. UI / Web Frontend Gaps

### 3.1 Components Over 300 Lines — ✅ RESOLVED 2026-06-04
- **Spec:** CLAUDE.md / CONTRIBUTING.md: "Keep Svelte components under ~300 lines"
- **Gap:** Previously `CreateVMModal.svelte` (~580 lines) and `DataTable.svelte` exceeded the threshold.
- **Resolution:** `CreateVMModal.svelte` is now 283 lines. `DataTable.svelte` is fully decomposed into `ResourceTable.svelte` (198 lines), `InventoryTable.svelte` (72 lines), and `UserTable.svelte` (125 lines). The monolithic `DataTable.svelte` no longer exists. Table logic utilities live in `table.svelte.ts` (451 lines of non-component logic).
- **Re-check 2026-10-03:** `CreateVMModal.svelte` is 288 lines and `settings/users/+page.svelte` is 253 lines. `SidebarNav.svelte` has regressed to 346 lines and exceeds the threshold again. PR #213 later deleted `ResourceTable.svelte` as dead code; `table.svelte.ts` is now 12 lines.
- **Evidence:** `wc -l ui/src/lib/components/vms/CreateVMModal.svelte` = 288; no file named `DataTable.svelte` exists in `ui/src/`
- **Priority:** P2

### 3.2 InventoryListPage Uses `any` Types — ✅ RESOLVED 2026-06-03
- **Spec:** CONTRIBUTING.md: "Use TypeScript strictly; avoid `any`"
- **Resolution:** Added Svelte 5 generic parameter `<T extends Record<string, unknown>>` to `InventoryListPage.svelte`. Replaced `any[]` props with `ColumnDef<T>[]`, `T[]`, and typed `rowHref`/`cell` snippets. All list views (VMs, volumes, networks, nodes, tasks) now get type-safe column and row typing.
- **Evidence:** `ui/src/lib/components/shell/InventoryListPage.svelte:1-45`
- **Priority:** P2

### 3.3 "awaiting-operator-input" Task State — ✅ RESOLVED 2026-06-02
- **Spec:** ADR-004-WebUI: Required task states include `awaiting-operator-input` (reserved for later)
- **Resolution:** Added `AwaitingOperatorInput` to `OperationStatus` enum with `is_active()` helper. UI displays the state with a warning-tone badge, includes it in active ops count, and the migration reaper excludes it from timeout failures.
- **Evidence:** `crates/chv-controlplane-types/src/domain.rs:280`, `ui/src/lib/webui/tasks.ts:137-142`, `ui/src/routes/tasks/+page.svelte:99-103`
- **Priority:** P3

---

## 4. Infrastructure / Deployment Gaps

### 4.1 Multi-Node WebSocket Routing — ✅ RESOLVED 2026-06-02
- **Spec:** PHASED_IMPLEMENTATION_PLAN.md Phase 3: "Nginx Routing: configure multi-node WebSocket routing (`/ws/vms/`) using a dynamic upstream based on `node_id`"
- **Resolution:** nginx `map $request_uri $ws_backend` selects the correct agent backend by `node_id` prefix. Regex location rewrites `/ws/vms/{node_id}/{vm_id}/console` to `/vms/{vm_id}/console` before proxying. BFF returns node-routed URLs when `proxied=true` or `agent_ws_address` is empty. Direct `ws://` URLs still work for single-node deployments.
- **Evidence:** `docs/examples/nginx/chv-ui.conf:45-85`, `crates/chv-webui-bff/src/handlers/vms.rs:1047-1062`, `docs/DEPLOYMENT.md`
- **Priority:** P1

### 4.2 Docker Compose Incomplete for Production Use — ✅ RESOLVED 2026-06-03
- **Spec:** DEPLOYMENT.md, CONTRIBUTING.md (Docker optional)
- **Resolution:** Added `docker-compose.prod.yml` with production-oriented configuration. It provides health checks for all services and `read_only: true` root filesystems with `tmpfs` overlays. It adds dedicated service users (`chv`, `chv-stord`), KVM device passthrough for `chv-agent`, and host networking for `chv-nwd`. Services share volumes via named Docker volumes. An nginx reverse proxy handles WebSocket upgrades. `Dockerfile` updated with runtime dependencies, service user creation, and additional exposed ports.
- **Evidence:** `docker-compose.prod.yml`, `Dockerfile`
- **Priority:** P2

---

## 5. Already Implemented (No Gap)

These areas were previously flagged as gaps but are now complete:

| Area | Evidence |
|------|----------|
| **Partition policy (ADR-006)** | `crates/chv-agent-core/src/connectivity.rs` — `ConnectivityTracker` with `Connected`/`Disconnected`/`Reconnecting` states; `agent_server.rs:554` rejects `CreateVm` when disconnected; `agent_server.rs:1817` rejects `MigrateVm` when disconnected; `control_plane.rs` `drain_pending_control_plane_queue` drains `NodeCache::pending_control_plane_messages` on every tick and on reconnect |
| **VM resize end-to-end** | `crates/chv-webui-bff/src/handlers/vms.rs:733-830` — BFF updates `vm_desired_state` cpu_count/memory_bytes and bumps generation; `crates/chv-agent-core/src/reconcile.rs:1318-1339` — agent detects drift and calls `vm_runtime.resize_vm()` |
| **Network mutations end-to-end** | `crates/chv-webui-bff/src/handlers/networks.rs` — BFF lifecycle handlers. Network policy persists via the `networks/update` `firewall_rules` field (engine-vocabulary validated, #369 gate) into `network_desired_state.firewall_rules_json`, which `crates/chv-controlplane-service/src/orchestrator.rs` attaches to the VM spec at the next dispatch — core-managed nodes materialize it at attach time (#361, m4.4). Live re-dispatch of an updated ruleset to already-attached VMs lands with #355 (PR 1 carrier #565 + PR 2 journaling #566: the update journals an `UpdateNetworkPolicy` operation that the orchestrator fans out to every node with a live attached VM on the network), and the empty-ruleset boundary is the DP4 baseline (`[]` = DHCP/DNS/conntrack + default-deny, applied at dispatch and at attach — #355 PR 3). The `set_firewall_policy`/`set_nat_policy`/`ensure_dhcp_scope`/`ensure_dns_scope` reconcile call sites are the fenced `authority_mode = "legacy"` path, not the qualified one. |
| **svelte-check warnings** | `npm run check` reports **0 errors and 0 warnings** |
| **Playwright E2E in CI** | `.github/workflows/ci.yml` has dedicated `e2e` job running Playwright against a mock BFF server |
| **Toast design tokens** | `ui/src/lib/components/primitives/Toast.svelte` uses `var(--color-success)`, `var(--color-danger)`, `var(--color-info)` |
| **TopologyCanvas refactor** | Now 299 lines (was 986) |
| **SidebarNav refactor** | Now 259 lines (was 688) |
| **settings/users refactor** | Now 231 lines (was 815) |
| **No production `unimplemented!()` stubs** | `grep -rn "unimplemented!()" crates/` returns only test-mock implementations |
| **Operation ID propagation** | `crates/chv-agent-core/src/daemon_clients.rs:21-223` — `with_operation_id` wraps all stord/nwd RPCs with `x-operation-id` metadata and tracing spans |
| **Console token replay prevention** | `crates/chv-agent-core/src/console_server.rs:24-151` — 2048-entry LRU cache + 2-second rate limiter per VM |
| **Quota enforcement** | `crates/chv-webui-bff/src/handlers/vms.rs:390-988` — `enforce_user_quota` checks max_vms, max_cpu, max_memory_bytes, max_storage_bytes |
| **RBAC middleware** | `crates/chv-webui-bff/src/router.rs:35-400` — Three-tier routing (viewer / operator / admin) with `require_operator_or_admin` and `require_admin` gates |
| **Node state machine library** | `crates/chv-agent-core/src/cache.rs:268-276` — `StateMachine` with `transition_node_state` and transition validation |
| **nginx WebSocket proxy** | `docs/examples/nginx/chv-ui.conf` — `/ws/vms/` location with upgrade headers |
| **systemd services** | `docs/examples/systemd/*.service` — All 4 daemons with `KillMode=mixed` and `TimeoutStopSec=5` |
| **Pre-migration DB backup** | Automatic DB backup before migrations with 10-backup rotation |
| **Version bump automation** | `scripts/bump-version.sh` + `Makefile` target |
| **Dark mode** | Full implementation with `UserMenu` toggle and `[data-theme="dark"]` tokens |
| **Command palette** | `ui/src/lib/components/shell/CommandPalette.svelte` — fuzzy-search with 16 commands |
| **Design token alignment** | Earthy palette applied to `app.css`, `tailwind.config.cjs`, and all components |
| **Error handling alignment** | `crates/chv-webui-bff/src/error.rs` — no `unreachable!()` panic paths |
| **Logging alignment** | `crates/chv-config/src/lib.rs` — `tracing` replaces `eprintln!` |
| **Async runtime safety** | `crates/chv-agent-core/src/console_server.rs`, `crates/chv-controlplane-service/src/container.rs` — `tokio::sync::Mutex` in async paths |
| **Circuit breaker** | `crates/chv-controlplane-service/src/circuit_breaker.rs` — `Closed` → `Open` → `HalfOpen` with configurable thresholds |
| **Deep health checks** | `GET /health/deep` returns database, agent socket, and agent connectivity status |
| **Migration reaper** | `crates/chv-controlplane-service/src/migration_reaper.rs` — scans every 60s for migrations stuck beyond 2h |
| **Drain evacuation** | `crates/chv-agent-core/src/reconcile.rs` — `Draining` state triggers migration requests |
| **VXLAN teardown** | `crates/chv-nwd-core/src/executor.rs` — `delete_topology` cleans up VXLAN interfaces and FDB entries |
| **FDB cleanup on VM detach** | Implemented in `chv-nwd-core` executor and reconcile modules |

---

## Appendix A: Gap-to-Spec Cross-Reference

| Gap | ADR | Component Spec | Plan Phase | Status |
|-----|-----|---------------|------------|--------|
| 1.1 Disk migration dirty sync/final flush | ADR-012 | chv-stord-spec | Phase 3 | ✅ Resolved |
| 1.2 Backup execution engine | — | — | Phase 3 | ✅ Resolved (shipper wired, retention fixed, crypto at rest, tests added, DR runbooks documented) |
| 1.3 iSCSI / Ceph adapters | ADR-004 | chv-stord-spec | Phase 2 | ✅ Resolved |
| 2.1 stord security hardening | — | chv-stord-spec | — | ✅ Resolved |
| 3.1 Components >300 lines | CLAUDE.md | — | Phase 3 | ✅ Resolved |
| 3.2 InventoryListPage `any` types | CONTRIBUTING.md | — | — | ✅ Resolved |
| 3.3 awaiting-operator-input | ADR-004-WebUI | — | — | ✅ Resolved |
| 4.1 Multi-node WS routing | — | — | Phase 3 | ✅ Resolved |
| 4.2 Docker compose production | DEPLOYMENT.md | — | — | ✅ Resolved |

---

## Appendix B: Files Examined

**Specs:** `docs/ARCHITECTURE.md`, `docs/DEPLOYMENT.md`, `docs/OPERATIONS.md`, `PHASED_IMPLEMENTATION_PLAN.md`, `DESIGN.md`, `CLAUDE.md`, `CONTRIBUTING.md`, all ADRs 001–013, all component specs.

**Backend:** `crates/chv-controlplane-service/src/lifecycle.rs`, `bff_mutations.rs`, `orchestrator.rs`, `reconcile.rs`, `server.rs`, `circuit_breaker.rs`, `migration_reaper.rs`; `crates/chv-webui-bff/src/handlers/*.rs`, `router.rs`, `mutations.rs`, `error.rs`.

**Agent:** `crates/chv-agent-core/src/reconcile.rs`, `agent_server.rs`, `vm_runtime.rs`, `console_server.rs`, `cache.rs`, `daemon_clients.rs`, `supervisor.rs`, `connectivity.rs`, `control_plane.rs`; `crates/chv-nwd-core/src/executor.rs`, `reconcile.rs`, `handlers.rs`; `crates/chv-stord-core/src/handlers.rs`, `migration/sender.rs`.

**UI:** `ui/src/routes/**/*.svelte`, `ui/src/lib/components/**/*.svelte`, `ui/src/lib/api/client.ts`, `ui/tests/e2e/*.ts`.

**Infra:** `.github/workflows/ci.yml`, `docs/examples/nginx/chv-ui.conf`, `docs/examples/systemd/*.service`, `Dockerfile`, `docker-compose.yml`, `scripts/*.sh`.
