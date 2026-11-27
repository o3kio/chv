# CellHV Core Startup Authority Coordinator

Status: Production wiring in the explicit `core-native` and `core-managed`
authority modes (the config loader defaults to `core-managed`, fail-closed;
shipped configurations select it explicitly).

## Boundary

`cellhv-core-startup` is a library used to make the `chv-agent` startup
authority choice. It creates no daemon, database format, operation engine, VM
process, storage attachment, or network attachment. All database access passes
through `cellhv-core-operations::OperationService`; NodeCache conversion passes
through `cellhv-nodecache-migration`.

`cmd/chv-agent` calls this boundary when the operator selects the
`core-native` or `core-managed` authority mode. A config that omits the key now
selects the core-managed boundary (fail-closed); the legacy launch/reconcile
path remains available as an explicit `legacy` opt-in and is unchanged. This
boundary never launches, inspects, recovers, or manages a VM process.

`StartupTransaction::begin` is the activation boundary intended for that
wiring. It acquires the non-blocking process-lifetime `RuntimeAuthorityLease`
first and then the blocking NodeCache `AuthorityLock`. It snapshots cache and
database presence only while both guards are held. `activate` performs identity
resolution and fresh creation, import, or reopen without releasing either
guard. Once the database decision is durable, it releases the short
`AuthorityLock` and yields `ActivatedStore`, containing an already-open
`OperationService`, validated migration provenance, and an opaque
`RuntimeAuthorityGuard`. Consuming `into_runtime_parts` transfers those values
without releasing the runtime lease.

For agent composition, `prepare_activation` returns an opaque
`PendingActivatedStore` that retains the short lock and exact snapshot bytes;
`finish` releases the lock and yields `ActivatedStore`. The former
`chv-agent-core::AgentCoreActivation::from_pending` consumer was removed with
the NodeCache authority facade (#213). Production core-native startup now uses
`activate_native_only`, which refuses a live NodeCache and never synthesizes a
replacement JSON file. Ownership observation and recovery composition remain
outside this boundary.

The direct `activate` boundary activates only the Core database. It does not
construct or authorize `NodeCacheAuthority`, select a process-wide cache mode, wire
production handlers, or prove that every `OperationService` in the repository
is runtime-lease guarded. `OperationService` remains publicly constructible for
the existing library and test surfaces; production composition must close that
residual before claiming process-wide enforcement.

The older `coordinate` decision API remains for the existing migration and
fault-injection harness. It is not the production activation boundary and does
not return an authority handle. Future `cmd/chv-agent` composition must consume
`StartupTransaction` and retain `RuntimeAuthorityGuard` for the runtime
lifetime.

## Durable Protocol

For a legacy cache with no database, the coordinator:

1. acquires the exclusive advisory lock derived by `cellhv-core-fs` from the
   NodeCache path; `NodeCache::save` uses the same lock;
2. reads the exact NodeCache bytes once and validates a complete import plan;
3. writes those bytes to a temporary archive with `create_new` and mode `0600`;
4. syncs the file, atomically renames it, and syncs the archive directory;
5. creates a `0600` migration target through `OperationService` and imports;
6. commits the explicit checksum-bound cutover marker;
7. returns `ActivateCore` and releases the lock.

A restart accepts an existing archive only when its SHA-256 matches the exact
source. It can complete a matching, synced temporary archive after a returned
error and reopen. An imported marker is resumed only with the matching source
and archive. A cutover marker is irreversible through this API. A database left
after an interrupted create is resumed only if every authority and journal table
is empty and the source is still present. These in-process fault hooks do not
simulate power loss or prove persistence on a specific filesystem.

The bounded filesystem trust model requires each immediate cache, archive, and
database parent to pre-exist as a real, service-owner-owned `0700` directory.
The parents may differ. Authority files must be regular, service-owner-owned,
owner-only files. Symlinks, special files, multi-link files, and aliases between
configured paths or derived archive-temporary, lock, and SQLite sidecar paths
fail closed using lexical/normalized and existing-inode checks.
The derived runtime-lease path participates in the same alias checks.
Directory-fd-relative `openat` hardening remains appropriate if this trust
boundary is later broadened.

## Fail-Closed Decisions

| NodeCache | Core database | Marker | Result |
|---|---|---|---|
| absent | absent | n/a | `InitializeFreshCore`; only the shared exclusion lock is created |
| valid | absent | n/a | archive, import, cut over, `ActivateCore` |
| absent | present | none | `ActivateCore` for a native Core database |
| absent | present | none plus migration archive | error: interrupted migration source missing |
| absent | present | imported | error: imported source missing |
| absent | present | cutover | verify owner-only archive checksum, then `ActivateCore` |
| valid | present | none and pristine | resume create-before-import crash |
| valid | present | none and non-pristine | error: unrelated competing authority |
| matching | present | imported | verify archive, complete cutover |
| matching | present | cutover | verify archive, `ActivateCore` |
| changed | present | imported/cutover | checksum mismatch; never activate JSON |
| malformed or unsupported | import/resume path | absent/pristine | parser error; no new database or import |
| archive mismatch | any migration path | any | archive error; no import/cutover |

Future production wiring must treat `ActivateCore` as an exclusion rule: it
must not construct a VM-writable NodeCache. The retained JSON and archive are
compatibility/recovery evidence only.

ADR-019 proposes the pending fresh-host identity and process-wide compatibility
mode policy. `InitializeFreshCore` does not authorize a placeholder identity or an
in-memory/fallback authority. Production wiring must persist either a valid
fresh seed or a one-time generated UUID before publishing the authority. It
must select `core-vm-authority` only after that durable creation succeeds.

A compatibility-only cache whose `node_id` is a reserved placeholder remains
unresolved in this slice. `StartupTransaction` routes every present cache
through the strict importer, which rejects that identity and fails closed. The
ADR-019 archive/retire-then-fresh path requires a separately reviewed cache
classification and retirement transaction; this implementation does not infer
that an apparently empty JSON document is disposable.

The pure resolution and explicit fresh-create primitives now exist in
`cellhv-core-startup` and are specified in
`docs/specs/cellhv-core-host-identity-resolution.md`. The explicit
`core-native` composition uses them; `InitializeFreshCore` itself still
neither selects an ID nor creates a store.
