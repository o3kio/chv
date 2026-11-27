# CellHV Core Native Authority Mode

Status: explicit opt-in production composition; the shipped default is
`core-managed` (see below).

`chv-agent` accepts `authority_mode = "legacy" | "core-managed" |
"core-native"`. A missing key defaults to `core-managed` (fail-closed; the
config loader's own default), matching every shipped configuration — the
packaged `/etc/chv/agent.toml`, `docs/examples/agent.toml`, and the
`install.sh` template — which also select `core-managed`, the qualified
production composition (#326). `legacy` is an explicit opt-in that logs a
warning and preserves the legacy Controller-facing AgentServer, reconciler,
provider, VMM, metrics, console, and NodeCache startup path.

`core-native` dispatches before any legacy component is constructed. It
requires the configured cache, Core store/archive, and native socket parents to
be pre-existing chv-owned 0700 directories. A present legacy NodeCache fails
closed before database migration or creation. An absent database is created
with the configured non-placeholder `node_id`, or a one-time UUID when empty;
an existing native database is reopened with identity consistency checks.

The mode starts only `CoreRuntimeOwner`: one database actor and the private
native HTTP API. It does not contact Controller, create a legacy cache, start
AgentServer, reconciliation, Cloud Hypervisor, storage/network providers,
console, metrics, or supervised daemons. VM mutations are durably accepted but
remain unexecuted in this phase. SIGINT and SIGTERM await listener drain, actor
shutdown/join, and runtime-lease release in order.

The shipped `chv-agent.service` retains its legacy-default `Wants=` and `After=`
relationship to `chv-controlplane.service`; systemd dependencies cannot be
conditional on a value inside `agent.toml`. This does not make the core-native
process contact Controller, but the stock unit will request that Controller be
started. A standalone deployment must use a unit drop-in that resets both
directives and retains only the network ordering:

```ini
[Unit]
Wants=
After=
After=network.target
```

Packaging a separate preset or generator for that topology is future work. The
default unit is intentionally unchanged for legacy compatibility.
