# Authority-Mode Feature Matrix

Status: current behavior plus a maintainer removal target. This document does
not claim that legacy mode has been removed.

`chv-agent` accepts `authority_mode = "legacy" | "core-managed" |
"core-native"` ([`crates/chv-config/src/lib.rs`](../../../crates/chv-config/src/lib.rs),
`AgentAuthorityMode`). The config-loader default is `legacy`, but every shipped
configuration selects `core-managed`:
`packaging/config/chv.yaml`, `docs/examples/agent.toml`, and the
`scripts/install.sh` template. `core-native` is an explicit opt-in production
composition; it is not the shipped default.

> **Legacy removal target: 2027-06-30.** This is a maintainer decision
> recorded here, not a claim that removal has happened or a guarantee that
> legacy will remain until that date. Legacy is still selectable in the
> current release. Removal requires the exit criteria in §"Legacy exit
> criteria" to be met and recorded.

## Matrix

“Yes” means the component is composed and reachable in that mode. “No” means
the mode returns before that component is constructed. “Compatibility” means
the surface exists but delegates to the Core authority or fails closed instead
of performing its legacy side effect.

| Feature / behavior | `legacy` | `core-managed` (shipped default) | `core-native` (explicit opt-in) |
|---|---|---|---|
| Selection | Explicit opt-in; missing key falls back here with the loader default | Every shipped config (`chv.yaml`, `examples/agent.toml`, `install.sh` template) | Explicit opt-in; standalone composition |
| Legacy Controller connection, enrollment, inventory, health reporting | Yes | Yes | **No** — returns before the legacy stack |
| Legacy gRPC `AgentServer` (lifecycle, reconcile, snapshot, migration, console) | Yes, full surface | Yes, as a compatibility adapter; lifecycle mutations submit to Core, legacy effectors fail closed with `unimplemented` | **No** |
| Legacy `Reconciler` provider-mutation surface | Yes, full mutation surface | Observe-only (`Reconciler::new_observe_only`); structurally has no mutation state | **No** |
| Core journal + executor (`CoreRuntimeOwner`, `JournalExecutor`) | No | Yes; sole provider effector | Yes |
| Native local HTTP API at `/run/chv/core/core-v1.sock` | **Not started** | Yes | Yes |
| VM power actions (start/stop/reboot) | Legacy reconciler/AgentServer effectors | Accepted by the legacy lifecycle RPC, submitted to Core, executed by the executor | Native `POST /v1/vms/{id}/actions/{...}` returns structured `422 unsupported`; no legacy surface |
| VM create/delete | Legacy effectors | Core executor | Core executor via the native CRUD surface |
| VM spec update / resize (`UpdateVm`) | Legacy path supports resize | Refused at the legacy desired-state boundary (generations ≥ 2); the Core executor marks `UpdateVm` `Unsupported` | Native `PATCH` journals the request; the executor marks `UpdateVm` `Unsupported` |
| NodeCache | Legacy authoritative cache | Compatibility projection: rebuilt from the Core store at startup and projected after successful Core execution | **No** legacy cache; a present legacy NodeCache fails closed during activation |
| Console WebSocket / legacy console server (`127.0.0.1:8444` default) | Yes | Yes | **No** — no legacy console server (the Core adapter still owns VMM process/console cleanup) |
| Agent metrics listener (packaged default `127.0.0.1:9901`) and `chv_agent_journal_*` families | Yes (legacy metrics; journal families absent) | Yes (journal families present) | **No** — no legacy metrics listener |
| Supervised `chv-stord` / `chv-nwd` daemons | Yes | Yes | **No** — the Core resource controller connects to the sockets; it does not supervise the daemons |
| Core state prerequisites | None | `StartupTransaction` imports/prepares the Core store and archive | Cache, Core store/archive, and native socket parents must be pre-existing `chv`-owned `0700` directories; an absent database is created with the configured non-placeholder `node_id` |
| Legacy mutation RPCs after Core is authoritative | N/A — legacy is the authority | Fail closed with gRPC `unimplemented` (e.g. `apply_node_desired_state`, legacy provider effectors) | N/A — no legacy RPC surface |
| Executor-fatal behavior | N/A | Agent exits non-zero for supervisor restart; in-flight `running` operations become InspectRequired | Same |

## Verified behavior notes

- **`core-managed` acceptance is not completion.** The legacy lifecycle RPC
  returns `status: "ok"` after the Core journal accepts the operation; the
  executor performs the effect asynchronously. Read
  `/v1/operations` for the real terminal state. See
  [VM Operation Failures](../../runbooks/vm-op-failures.md).
- **The native power endpoints are deliberately not routed through the
  executor in v1.** They always return `422 unsupported`
  ([`crates/cellhv-core-api/src/lib.rs`](../../../crates/cellhv-core-api/src/lib.rs),
  `unsupported_action`). In `core-managed`, power actions still work through
  the legacy lifecycle RPC, which submits to Core.
- **`core-native` still composes the executor and Cloud Hypervisor adapter.**
  It is not a read-only mode: the native CRUD surface accepts create/delete
  and the executor can perform them. The distinction from `core-managed` is
  the absence of the legacy Controller/`AgentServer`/reconciler/console/
  metrics/supervisor stack, not the absence of execution.
- **`UpdateVm` is the documented execution gap.** The Core executor returns
  `Unsupported` for `UpdateVm`, `AttachVolume`, `DetachVolume`, `AttachNetwork`,
  and `DetachNetwork`
  ([`crates/chv-agent-runtime-ch/src/core_runtime.rs`](../../../crates/chv-agent-runtime-ch/src/core_runtime.rs)).
  Resize-through-Core is deferred.
- **`core-native` refuses a legacy cache.** Activation fails closed before
  migration/creation when a legacy `NodeCache` is present; this prevents a
  silent second source of truth
  ([`docs/specs/cellhv-core-native-authority-mode.md`](../cellhv-core-native-authority-mode.md)).
- **Unit dependencies are not mode-conditional.** The shipped
  `chv-agent.service` still `Wants=`/`After=` the control plane. A standalone
  `core-native` deployment must use a unit drop-in that resets both
  directives (documented in the same spec). The stock unit does not make the
  process contact Controller, but it does request that Controller be started.

## Evidence map

| Claim | Evidence |
|---|---|
| Mode enum and default | `crates/chv-config/src/lib.rs` (`AgentAuthorityMode`) |
| Shipped configs select `core-managed` | `packaging/config/chv.yaml`, `docs/examples/agent.toml`, `scripts/install.sh`, `scripts/quick-install.sh` |
| `core-native` returns before the legacy stack | `cmd/chv-agent/src/main.rs` (`run_core_native`, early return in `main`) |
| `core-managed` composition | `cmd/chv-agent/src/main.rs` (`start_core_managed`, `CoreManaged` reconciler arm) |
| Legacy mutation fail-closed | `crates/chv-agent-core/src/agent_server.rs` (`with_core_authority`, `Status::unimplemented` guards) |
| Native API routes/errors | `crates/cellhv-core-api/src/lib.rs` |
| Native mode composition and prerequisites | [`docs/specs/cellhv-core-native-authority-mode.md`](../cellhv-core-native-authority-mode.md) |
| Native API contract | [`docs/specs/cellhv-core-native-api-v1.md`](../cellhv-core-native-api-v1.md) |
| Operation states/attempt bound/InspectRequired | [`docs/specs/cellhv-core-operation-model.md`](../cellhv-core-operation-model.md) |
| Executor effectors and unsupported operations | [`docs/specs/cellhv-core-journal-executor.md`](../cellhv-core-journal-executor.md) |
| Failure behavior/severity | [`docs/specs/ops/failure-matrix.md`](failure-matrix.md) |

## Legacy exit criteria

Legacy mode may be removed only when all of the following are true and the
evidence is recorded (in this document or a linked decision record):

1. `core-managed` is the qualified default in every supported topology, and
   no supported installer, package, or example selects `legacy`.
2. No supported deployment depends on the legacy Controller mutation surface,
   the legacy `Reconciler` provider effectors, or the legacy
   `AgentServer`-only RPCs.
3. The compatibility adapter paths that still route legacy lifecycle RPCs to
   Core have a replacement that covers the supported lifecycle, or are
   documented as intentionally unsupported.
4. The remaining Core executor gaps (`UpdateVm`, attach/detach) are either
   implemented or explicitly out of scope for the supported lifecycle.
5. A deprecation announcement and migration guidance have shipped in the
   CHANGELOG and release notes for at least one published minor release before
   removal.
6. The upgrade/rollback compatibility matrix no longer lists `legacy` as a
   supported rollback target.
7. A maintainer records the removal decision and date; this target may slip,
   but it must not be silently treated as already complete.

Until then, `legacy` remains a selectable compatibility mode and the shipped
warning (`authority_mode=legacy: ... requires an explicit opt-in`) continues
to identify it.
