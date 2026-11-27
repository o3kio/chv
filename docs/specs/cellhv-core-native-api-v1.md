# CellHV Core Native Local API v1

Status: Phase B local authority contract; explicit production composition available

## Boundary

`cellhv-core-api` is an HTTP/JSON transport around an injected clone of the
single `cellhv-core-operations::AuthorityHandle`. It has no SQLite, provider,
Cloud Hypervisor, control-plane, or cloud model dependency. It is intended to
run inside `chv-agent`; it is not a daemon or a second lifecycle authority.

The configured endpoint is `/run/chv/core/core-v1.sock`. `chv-agent` composes
the listener in both `core-managed` and `core-native` authority modes. In
core-managed mode, `CoreRuntimeOwner::start` binds the native listener
alongside the legacy stack. Only the `legacy` authority mode leaves the
listener unstarted. The config-loader default is `core-managed` and shipped
configurations select it explicitly, so both the default and shipped agent bind
the native listener. Core-native mode does not compose the legacy Controller,
VMM, or provider stack.

## Contract

| Method | Path | Semantics |
|---|---|---|
| `GET` | `/v1/host` | Durable host identity and truthful capabilities |
| `GET` | `/v1/host/capabilities` | Capability flags; all default to false |
| `GET`, `POST` | `/v1/vms` | List definitions; asynchronously accept create |
| `GET`, `PATCH`, `DELETE` | `/v1/vms/{id}` | Inspect; asynchronously accept update/delete |
| `POST` | `/v1/vms/{id}/actions/{start,stop,reboot}` | Structured `unsupported` — power actions are deliberately not routed through the executor in v1 |
| `GET` | `/v1/operations` | Ordered operation journal inspection; newest-first, bounded to the newest 1000 entries |
| `GET` | `/v1/operations/{id}` | Operation journal entry inspection |
| `GET` | `/v1/events?after=N&limit=M` | Ordered polling; limit is 1 through 1000 |

Mutation bodies carry a caller `request_id`; Core maps it to the durable,
surface-namespaced operation ID `native:v1:{request_id}`. Because the
`request_id` *is* the durable operation identity, it must be unique per
distinct mutation the caller wants admitted: reusing a `request_id` with a
fresh `Idempotency-Key` for a different mutation fails with a typed
operation conflict (it cannot become a second operation), while re-sending
the same mutation with the same `request_id` and `Idempotency-Key` is the
documented replay path and the same key with a different body is rejected.
Every supported mutation requires an
`Idempotency-Key` header. Update and delete additionally require `If-Match`
containing exactly one quoted positive decimal resource version, for example
`"7"`. Bare integers, weak tags, lists, zero, and noncanonical leading zeroes
are rejected. A stale version returns HTTP 412. PATCH definitions carry the
next resource version (`If-Match + 1`). Accepted mutations return
HTTP 202 and the durable operation identity, disposition (`accepted` or
`replay`), and reserved resource version.

The schema rejects unknown fields through the Core domain and request types.
It contains no tenant, project, quota, scheduler, Neutron, Cinder, Kubernetes,
libvirt XML, or other cloud-platform fields.

## Durable request audit metadata

Every native submit journals durable request metadata on the accepted operation
(`OperationJournalEntry.request_metadata`, persisted in the four `operations`
columns added by migration 0004):

- `requested_by` = `core-native-v1`;
- `external_operation_id` = the caller's `request_id`;
- `request_unix_ms` = the time Core received the request (positive Unix
  milliseconds);
- `legacy_generation` = `null` (native submits are never legacy provenance).

The journal entry is therefore self-contained audit evidence for who requested a
mutation, when, and under which `request_id`, without consulting any in-memory
state.

## Local security

`bind_private` fails closed unless the socket parent already exists, is a real
directory owned by the effective service user, and has exactly mode `0700`.
It refuses to replace any existing path, binds while the directory is
inaccessible to other users, and sets the socket to mode `0600`. This owner-only policy provides an authenticated local principal
without relying on unimplemented request credentials. Production startup must
also own stale-socket cleanup explicitly after proving the prior process is
gone.

## Deferred work

- NodeCache cutover and legacy gRPC routing into this same operation service;
- deterministic OpenAPI publication/client generation decision;
- event streaming (v1 exposes deterministic polling);
- native-API lifecycle execution: the power endpoints remain `unsupported`
  and the corresponding capability flags default to false.

The router cannot construct an `OperationService` or actor. Async handlers send
typed requests through the bounded shared authority queue and await typed
replies, preserving authority ordering without blocking Tokio workers. The
`chv-agent` composition root retains the actor owner for the whole listener
lifetime and performs ordered shutdown in core-managed and core-native modes.

`vm_definitions=false` currently describes executable production availability:
the writable definition journal is not an executable VM-definition backend.
The `JournalExecutor` and a managed Cloud Hypervisor transport are composed in
both core-managed and core-native modes, but the native power endpoints
deliberately return structured `unsupported` and the capability flags default
to false. The CRUD contract is implemented, tested, and reachable in
core-managed and core-native modes. The flag must not become true until
executable definition handling is safely wired.
