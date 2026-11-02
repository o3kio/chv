# CHV volume backend contract v1

**Status:** Proposed — not implemented; requires ADR-028 runtime integration
and ratification of the multi-host plan
**Authority:** [ADR-028](../adr/028-volvisor-storage-orchestration.md) (storage
authority and ownership split), [ADR-002](../adr/002-control-plane-boundary.md)
(control-plane → agent boundary), [ADR-029](../adr/029-multi-host-dispatch.md)
(dispatch transport; independent of this contract)
**Purpose:** define the normative, backend-neutral volume contract that
`chv-agent` exposes internally, the ownership rules that select a backend, and
the exact Volvisor v2 binding used by the native-local implementation

## 1. Scope

This contract defines:

- the single storage-owner rule and how an owner is selected;
- the operation set a volume backend MUST implement, with preconditions and
  postconditions;
- capability advertisement and the refusal behaviour for unsupported
  operations;
- identity, idempotency, and generation rules;
- the attachment-phase honesty rules;
- the error taxonomy CHV uses and how backend errors map onto it;
- the Volvisor Volume API v2 binding for `native-local`;
- the legacy `chv-stord` binding, preserved unchanged.

It does not define:

- tenant-facing HTTP/gRPC APIs (those remain CHV's own surface);
- VM lifecycle, VMM device configuration, or Cloud Hypervisor invocation;
- cross-host migration policy (see ADR-021 and the transition plan);
- `ceph-rbd`, `nearline-replicated`, or PCI passthrough enablement;
- Volvisor's witness, DRBD, or Rook-cell operation.

## 2. Normative terms

The key words **MUST**, **MUST NOT**, **SHOULD**, and **MAY** are normative.

| Term | Meaning |
|---|---|
| Volume backend | The component that performs storage lifecycle side effects on one node: `legacy-stord` or `volvisor`. |
| Storage owner | The single backend authorized to mutate a volume: `legacy-stord` or `volvisor`. |
| Backend class | The provider class string within an owner's vocabulary. |
| Attachment | A host-scoped record binding an open volume to one VM device. |
| Attachment generation | The backend's detach-fencing counter for one attachment instance; it never advances on an evidence report. |
| Volume generation | The backend's monotonic counter for a volume's state transitions. |
| Attachment state revision | The backend's monotonic counter for an attachment's evidence state; it advances only on an applied report. |
| Observed attachment phase | The strongest attachment fact CHV can prove, never stronger than the backend can report. |

Do not use "storage class" to mean owner. A class is a backend vocabulary
value; an owner is an authority decision.

## 3. Ownership

### 3.1 Owner values

The durable owner value is one of:

| Value | Meaning |
|---|---|
| `legacy-stord` | `chv-stord` is the only authorized backend. |
| `volvisor` | Volvisor is the only authorized backend. |

There is no `both`, no `auto`, and no empty value.

### 3.2 Rules

1. Every volume row MUST have exactly one `storage_owner`.
2. `storage_owner` MUST NOT be inferred from `storage_class`, the node's
   configured backend, the presence of a Volvisor record, or the outcome of a
   probe.
3. `storage_owner` MUST NOT change while the volume is attached to a VM or
   has a non-terminal operation.
4. The agent MUST refuse to open, attach, detach, grow, or delete a volume
   through a backend whose `owner()` does not equal the volume's
   `storage_owner`.
5. The control plane MUST refuse to schedule a volume onto a node that cannot
   serve its owner.
6. A backend MUST NOT adopt, import, reformat, or "take over" a resource it
   did not create, unless a separate, explicit, operator-authorized procedure
   exists and is documented. Discovery is read-only.
7. Two backends MUST NOT mutate the same volume, even for read-only
   operations that change observable state.

### 3.3 Class vocabulary

| Owner | Permitted classes |
|---|---|
| `legacy-stord` | `local`, `lvm`, `iscsi`, `ceph` (existing meaning, unchanged) |
| `volvisor` | `native-local` (v1) |

`native-local` MUST NOT be treated as a synonym for `lvm`, `local`, or
`local-direct`. The reverse mapping is equally prohibited. A request naming a
class outside the selected owner's set is `INVALID_ARGUMENT`, never a silent
fallback.

The legacy empty-string class normalisation (`""` → `local`) MUST NOT apply to
Volvisor-owned volumes. An absent class on a Volvisor-owned request is a
validation error.

## 4. Operations

A conforming backend MUST implement the operations below. Each operation
carries a CHV `operation_id` (except read-only inspection) and an expected
generation where stated.

### 4.1 `ensure_volume`

Creates the volume if absent; otherwise verifies that the existing volume
matches the requested identity and size.

- **Input:** `volume_id`, `project_id`, `backend_class`, `size_bytes`,
  `provisioning`, optional placement preferences, `operation_id`.
- **Precondition:** owner matches; class is in the backend's class set; the
  capability handshake admits `create` (or the volume already exists).
- **Postcondition:** the backend has a durable record for `volume_id` with
  `size_bytes >= requested`; the operation is idempotent on `operation_id`.
- **On replay:** returns the recorded outcome; a different payload with the
  same `operation_id` is `IDEMPOTENCY_CONFLICT`.
- **Size mismatch:** if the existing volume is smaller than requested,
  `ensure` MUST NOT grow it implicitly and MUST NOT delete/recreate it. It
  reports the observed size so the caller issues `grow` (or fails).
- **MUST NOT:** delete and recreate an existing volume to satisfy a size
  change. Growth is `grow`.

### 4.2 `attach`

Binds an existing volume to one VM device and returns a frontend descriptor.

- **Input:** `volume_id`, `attachment_id`, `vm_id`, `host_id`, access mode,
  `expected_volume_generation`, optional `vmm_disk_id`, `operation_id`.
- **Precondition:** volume exists and is owned by this backend; no conflicting
  writer; expected generation matches.
- **Postcondition:** the backend records the attachment with a fresh
  attachment generation and returns a host-scoped device path or an explicit
  unsupported-frontend refusal.
- **MUST NOT:** return a path that is not backed by a live attachment record.

### 4.3 `detach`

Releases the attachment after the consumer proves I/O cannot be in flight.

- **Input:** `volume_id`, `attachment_id`, `expected_attachment_generation`,
  drain proof, `operation_id`.
- **Precondition:** the attachment exists; expected generation matches; the
  drain proof is truthful.
- **Postcondition:** the attachment is released; the volume remains.
- **MUST NOT:** accept a detach with an unknown or stale attachment
  generation by "forcing" it. A stale detach is a refusal that requires
  inspection, not a best-effort release.

### 4.4 `close`

Releases host-side resources for an attachment that ended without a VM detach
(e.g. a create unwind).

- **Input:** `volume_id`, `attachment_id` (when known), `operation_id`.
- **Postcondition:** idempotent; closing an already-closed or never-attached
  handle succeeds without side effects.
- **MUST NOT:** delete the volume.

### 4.5 `grow`

Increases logical size.

- **Input:** `volume_id`, `new_size_bytes`, `expected_generation`,
  `operation_id`.
- **Precondition:** `new_size_bytes` is strictly greater and aligned to the
  backend's required granularity (Volvisor: 512 bytes).
- **Postcondition:** provisioned size is the new size; generation advances.
- **MUST NOT:** shrink through this operation. The legacy `resize` RPC keeps
  its existing (backend-dependent) behaviour and is not redefined here.

Volvisor's grow response carries `effective_size_bytes` but **not** a
generation; the agent MUST inspect after a grow to record the new volume
generation before issuing the next mutation.

### 4.6 `destroy`

Deletes the volume and its data.

- **Input:** `volume_id`, `expected_generation`, erasure policy,
  `operation_id`.
- **Precondition:** the volume is detached; owner matches.
- **Postcondition:** the backend record is gone; the operation is idempotent.
- **MUST NOT:** delete a volume with a live attachment or an in-doubt
  operation.

### 4.7 `inspect`

Read-only observation.

- **Input:** `volume_id`.
- **Postcondition:** returns at least: existence, class, size, volume
  generation, lifecycle state, attachment ids, current writer, health,
  evidence status. When the backend advertises `attachment_state_report`, it
  also returns the full per-attachment projection (`attachments[]`: state,
  `state_revision`, evidence, reporter, `recorded_at_ms`, the detach-fencing
  `attachment_generation` and the frontend) — CHV consumes that projection and
  never infers a phase it cannot see there. `attachments[]` order is not
  significant; CHV reconciles by `attachment_id`, never by position.
- **MUST NOT:** mutate state, claim `Healthy` without evidence, or advance a
  generation. A report never advances `attachment_generation`; only an
  applied report advances `state_revision`.

### 4.8 `report_attachment_state`

Optional consumer attestation, available only when the backend advertises
`attachment_state_report`.

- **Input:** `volume_id`, `attachment_id`, `expected_attachment_generation`,
  `state = advertised|active`, `evidence = vmm_configured|guest_confirmed`,
  optional `reporter`, optional bounded `detail`, `operation_id`.
- **Precondition:** the attachment instance exists and its generation matches;
  `advertised` requires `vmm_configured` or `guest_confirmed`; `active`
  requires `guest_confirmed`. `state_revision` starts at `1` for a fresh
  attachment instance and is scoped to it.
- **Postcondition:** an evidence advance yields `applied = true` with a bumped
  `state_revision`; an equal/lower report yields `applied = false` with the
  current projection, untouched (`evidence`, `reporter`, `recorded_at_ms`
  included). `attachment_generation` is unchanged in both cases. `detail` is
  journal-only diagnostic context, never part of the projection.
- **MUST NOT:** downgrade state, advance the detach-fencing generation, treat
  the untrusted `reporter` as authentication, or claim `active` from a host
  configuration/path check. The `reporter` label is non-empty when present,
  bounded to 1024 bytes, and must not contain control characters.

A post-detach or unknown attachment is `NOT_FOUND` for a new `operation_id`;
replaying a recorded `operation_id` returns its recorded outcome (including a
recorded no-op). Envelope refusals (invalid api version, `prepared` target,
unsupported evidence/state pairing, malformed or reserved `operation_id`,
over-long or control-bearing `reporter`/`detail`) are decided before the
backend journal write, so the `operation_id` remains reusable. A
stale-generation or not-found refusal is journaled as a terminal outcome; its
`operation_id` is consumed and the refusal replays verbatim.

An attachment id is unique for a volume's lifetime, including across detach and
re-attach. CHV MUST mint a fresh `attachment_id` per attach. A backend that
advertises `attachment_state_report` MUST refuse a consumed id with
`IDEMPOTENCY_CONFLICT`, so a delayed report from an old instance can never be
attributed to a new one.

### 4.9 Unsupported operations

`snapshot`, `clone`, `restore`, `delete_snapshot`, and
`same_host_live_backing_move` are **not implemented by any v1 backend**. The
capability vocabulary in §5 includes tokens for them so a future backend can
advertise them truthfully, but v1 MUST refuse the operations via capabilities
(§5), not by returning an empty success.

## 5. Capabilities

### 5.1 CHV capability tokens

A backend MUST advertise a set of CHV capability tokens:

`ensure`, `attach`, `detach`, `grow`, `destroy`, `inspect`, `snapshot`,
`clone`, `restore`, `live-migrate`, `offline-copy`, `same-host-backing-move`,
`report-attachment-state`.

`inspect` is mandatory for every backend and MUST NOT be treated as optional;
the other tokens may be absent. Absence of a token is authoritative: the
operation MUST be refused with `UNIMPLEMENTED` before any journal write or
backend call.

### 5.2 Negotiation

1. At startup, the agent queries the backend's capability set.
2. The set MUST be intersected with the classes the backend serves and the
   operator's enabled classes.
3. The agent reports only the intersection in node inventory.
4. The control plane MUST refuse to place an operation on a node whose
   advertised set lacks the required token.
5. A backend capability change (restart, provider change, upgrade) MUST be
   re-queried and reflected in inventory; stale capability claims are a bug.

### 5.3 Volvisor binding

Volvisor advertises its own tokens. CHV maps them one-to-one for v1:

| Volvisor capability | CHV token |
|---|---|
| `create` | `ensure` |
| `attach` | `attach`, `detach` |
| `resize` | `grow` |
| `snapshot` | `snapshot` |
| `clone` | `clone` |
| `live_migrate` | `live-migrate` |
| `offline_copy` | `offline-copy` |
| `same_host_live_backing_move` | `same-host-backing-move` |
| `attachment_state_report` | `report-attachment-state` |

The table is a vocabulary mapping, not a v1 enablement: the pinned `native-local`
provider advertises only `create`/`attach`/`resize` (plus the unused
`same_vg_extent_move`), so the snapshot, clone, live-migrate, offline-copy,
backing-move and attachment-state-report rows are dormant and the operations
stay refused. The corrected `attachment_state_report` implementation exists on
a Volvisor feature branch; it is not part of the CHV pin until the issue is
dispositioned, merged and consciously re-pinned (§8).

The v1 Volvisor binding requires `create`, `attach`, and `resize` present for
the `native-local` class. A missing token MUST disable the class, not degrade
silently.

Volvisor's LVM provider additionally advertises `same_vg_extent_move`
(the pvmove path). CHV v1 does **not** consume it: there is no CHV token for
it and no CHV operation invokes it. Advertising it upstream is not permission
for CHV to call it.

## 6. Identity and idempotency

### 6.1 Identifier conformance

Volvisor validates identifiers as non-empty strings of at most **128 bytes**
from the alphabet `[A-Za-z0-9_.:-]`. `/` is forbidden.

CHV identifiers used at the boundary MUST conform:

| CHV value | Boundary field | Rule |
|---|---|---|
| CHV `volume_id` | `volume_id` | MUST match `^[A-Za-z0-9_.:-]{1,128}$` |
| CHV `attachment_id` | `attachment_id` | MUST match the same pattern |
| VM id | `vm_id` | MUST match the same pattern |
| Node id | `host_id` | MUST match the same pattern |
| Tenant | `project_id` | MUST match the same pattern |

A value that does not conform MUST be rejected at the agent boundary with
`INVALID_ARGUMENT`. It MUST NOT be slugified, truncated, hashed, or rewritten
silently. If an existing CHV identifier cannot conform, the migration requires
an explicit mapping table and operator approval — not an implicit transform.

### 6.2 Operation identifier derivation

CHV MUST supply a stable `operation_id` for every mutation.

- If the control plane supplied an operation id, the agent MUST use it
  verbatim. The control plane MUST keep that id stable across retries of the
  same logical operation and MUST NOT mint a new id for a retry. CHV MUST NOT
  mint ids carrying the reserved `derived:` prefix.
- Otherwise the agent MUST derive one deterministically:

```text
operation_id = "chv:" + verb + ":" + lowercase_hex(sha256(canonical_payload))
verb ∈ { ens, att, det, clo, grw, des }
canonical_payload = UTF-8 JSON, no insignificant whitespace, keys in exactly
  this order:
  {"v":1,"verb":<verb>,"volume_id":<string>,"vm_id":<string|null>,
   "attachment_id":<string|null>,"generation":<u64>,"size_bytes":<u64|null>}
```

- The derived id MUST fit 128 bytes (the fixed `chv:` prefix, a three-letter
  verb, and 64 hex characters is 72 bytes).
- The derived id MUST NOT use the reserved `derived:` prefix, which Volvisor
  reserves for its own engine-derived ids and refuses from consumers.
- A random UUID MUST NOT be used for a retried mutation. A random id makes a
  crash-replay create a second attachment.
- `generation` is the expected volume generation for `ens`/`att`/`grw`/`des`,
  and the expected attachment generation for `det`.

### 6.3 Idempotency semantics

1. Replaying the same `operation_id` with the same canonical payload MUST
   return the recorded outcome without a new side effect.
2. Reusing `operation_id` with a different payload MUST fail closed with
   `IDEMPOTENCY_CONFLICT` → CHV `CONFLICT`.
3. The agent MUST persist enough state to reconstruct the same
   `operation_id` after a restart. Deriving from desired state (not from a
   transient counter) is what makes this possible.
4. A derived id MUST be recomputed from the same durable inputs on replay. If
   an input legitimately changed (new desired generation), the operation is a
   new operation and gets a new id.

## 7. Generations and conflicts

1. The backend's volume and attachment generations are authoritative for
   storage mutations.
2. CHV MUST record the generation returned by the last successful mutation
   and MUST send it as the expected generation on the next mutation.
3. A mismatch is `STALE_GENERATION`, mapped to CHV `STALE_GENERATION`. It is
   **not** retried automatically; the control plane MUST re-inspect and
   re-issue intent from fresh state.
4. CHV's desired-state generation and the backend's storage generation are
   distinct values. They MUST NOT be compared to each other or used
   interchangeably.
5. `OPERATION_IN_DOUBT` (outcome unknown after a backend or process failure)
   MUST be surfaced as a non-retryable conflict requiring inspection. It MUST
   NOT be mapped to `BackendUnavailable` (which retries) or to success.
6. A backend MUST NOT advance a generation on a refused operation.

## 8. Attachment phase and observability truthfulness

Volvisor's corrected attachment-state design ([o3kio/volvisor#34](https://github.com/o3kio/volvisor/issues/34))
separates three honest facts per attachment instance:

- `prepared` — the backend prepared a host-scoped handle (provider-recorded);
- `advertised` — a consumer attested that it installed the frontend in a VMM
  (`vmm_configured`) or observed it guest-side;
- `active` — a consumer attested a positive guest-side observation
  (`guest_confirmed`).

`state_revision` orders these states monotonically
(`prepared < advertised < active`); `attachment_generation` is the detach
fence and is **never** advanced by a report. An equal or lower report is an
idempotent no-op, and the backend never downgrades state. The inspect surface
carries the full per-attachment projection (`attachments[]` with state,
revision, evidence, reporter and the frontend). The capability token is
`attachment_state_report` (not the earlier `attachment_phase_report` name).

Implementation status: the design is corrected and implemented on a Volvisor
feature branch, but the issue's disposition and any upstream merge/release are
still pending. This contract does **not** assume a released upstream surface.
Until the CHV pin's Volvisor build advertises `attachment_state_report` and its
conformance evidence is reviewed:

1. CHV MUST record the attachment as `phase = prepared`, `observed = false`.
2. CHV MUST NOT report `advertised` or `active` for a Volvisor-owned
   attachment.
3. CHV MAY corroborate the returned `host_device_path` with a host-side
   existence check and record `device_present = true|false`. That is evidence
   of a path, not of a live VMM attachment, and MUST NOT be labelled `active`.
4. A UI or API MUST render `prepared` distinctly from `active`.
5. Health and evidence MUST default to `unknown` unless the backend reports a
   proven value. `unknown` MUST NOT be rendered as healthy.
6. Once the pin advertises `attachment_state_report` and the capability is
   re-reviewed and re-pinned (§10.1.1, §13), CHV MAY report `advertised` from a
   truthful `vmm_configured` attestation — after CHV has actually installed the
   returned frontend into the VMM. CHV MUST NOT report `active` in v1: the
   default CHV install has no guest agent/confirmation path
   ([ADR-026](../adr/026-optional-monitor-agent-and-guest-identity.md)), and a
   host-side configuration or path check is not a guest confirmation. `active`
   becomes honest only with a separately designed, pinned guest-confirmation
   path.
7. CHV MUST surface the backend's `state_revision` and never derive its own
   ordering from wall-clock timestamps.

This posture is deliberate: an honest `prepared` (and, later, a
`vmm_configured`-backed `advertised`) is correct; a fabricated `active`
violates ADR-028 and Volvisor's own evidence rule.

## 9. Errors

CHV maps backend errors by their machine-readable code, not by HTTP status.

| Backend condition | CHV error | Retryable |
|---|---|---|
| `UNSUPPORTED_CLASS_OR_POLICY`, `MOVE_UNSUPPORTED_SCOPE`, `VMM_HANDOFF_UNSUPPORTED`, `MIGRATION_UNSUPPORTED_LOCAL_STORAGE` | `Unimplemented` | No |
| `INVALID_REQUEST` | `InvalidArgument` | No |
| `NOT_FOUND` | `NotFound` | No |
| `STALE_GENERATION`, `STALE_EPOCH` | `StaleGeneration` | No (re-inspect) |
| `IDEMPOTENCY_CONFLICT`, `INVALID_STATE`, `WRITER_ALREADY_ACTIVE`, `LEASE_HELD` | `Conflict` | No |
| `FORBIDDEN` | `AccessDenied` | No |
| `NO_SAFE_CAPACITY`, `THIN_METADATA_EXHAUSTED` | `QuotaExceeded` | No |
| `FOREIGN_DEVICE_STATE`, `UNSAFE_DATA_LOSS` | `Conflict` | No |
| `OPERATION_IN_DOUBT` | `Conflict` (in-doubt) | No until inspected |
| `INTERNAL`, `CEPH_CLUSTER_UNHEALTHY`, `REPLICA_NOT_DURABLE`, `UNKNOWN_FENCING_AUTHORITY` | `BackendUnavailable` | Yes, bounded |
| Transport timeout / connection refused | `BackendUnavailable` | Yes, bounded |
| `401` / `403` from Volvisor | `AccessDenied` + loud log | No |

Rules:

1. An error MUST NOT be converted to success, and an unknown error MUST
   become `BackendUnavailable` or `Internal`, never `Ok`.
2. `401`/`403` indicates a misconfigured or rotated token. It MUST be logged
   without the token and MUST NOT be retried in a tight loop.
3. The backend's `message` MAY be included in logs. It MUST be treated as
   untrusted text (no format-string interpolation, no log injection).

## 10. Refusal matrix (v1)

| Operation | `legacy-stord` | `volvisor` (`native-local`) |
|---|---|---|
| ensure / open | yes (existing) | yes |
| attach / detach | yes (existing) | yes |
| close | yes | yes |
| grow | yes (existing) | yes (grow-only) |
| destroy | yes (existing) | yes (detached only) |
| snapshot | yes (existing) | **refused** (`UNIMPLEMENTED`) |
| clone | yes (existing) | **refused** |
| restore | yes (existing) | **refused** |
| shrink | not specified by this contract (legacy behaviour unchanged) | **refused** |
| live cross-host migration | stord pre-copy (existing, single backend) | **refused**; the Volvisor mobility surface is not enabled for LVM (404), and CHV refuses before calling it |
| same-host backing move | absent | **refused** (capability not enabled in v1) |
| offline copy | not applicable | **refused** (not enabled in v1) |
| disk seeding (`seed_from`) | yes (existing) | **refused** in v1 (no Volvisor field; see §12) |

Refusals MUST be terminal. They MUST NOT be journaled as retryable
operations, and they MUST NOT be re-attempted by a reaper.

## 11. Volvisor binding (Volume API v2)

### 11.1 Placement and transport

- `volvisord` runs on the storage node and binds **loopback**.
- Only `chv-agent` talks to it. The control plane, BFF, UI, and `chvctl` MUST
  NOT call it.
- Transport is HTTP/JSON. The agent MUST use a bounded connect timeout, a
  bounded request timeout, and MUST NOT follow redirects.
- Every mutating request and the whole `/v2/admin` surface require the admin
  bearer token. The token MUST be read from a file at startup, mode `0400`,
  owned by the agent user, never logged, never placed in a URL or error
  message.

### 11.2 Endpoints used

| Operation | Request |
|---|---|
| capabilities | `GET /v2/capabilities` |
| ensure (inspect) | `GET /v2/volumes/{volume_id}` |
| ensure (create) | `POST /v2/volumes` |
| attach | `POST /v2/volumes/{volume_id}/attach` |
| detach | `POST /v2/volumes/{volume_id}/detach` |
| report attachment state (optional, capability-gated) | `POST /v2/volumes/{volume_id}/attachment-state` |
| grow | `POST /v2/volumes/{volume_id}/grow` |
| destroy | `DELETE /v2/volumes/{volume_id}` |
| health | `GET /healthz` |

CHV v1 MUST NOT call the admin device surface, the nearline adopt surface, or
the mobility routes. They are outside this contract.

### 11.3 Request fields

All requests carry `"api_version": "volvisor.volume.v2"` and an
`operation_id`.

Create (`ensure` on `404`):

| Field | Source |
|---|---|
| `project_id` | VM tenant (`StorageAttachmentRef.project_id`) — required |
| `volume_id` | CHV volume id |
| `class` | `native-local` |
| `size_bytes` | requested size; MUST be a multiple of 512 and of `logical_block_size` |
| `logical_block_size` | 4096 unless the volume definition says otherwise (allowed: 512/1024/2048/4096) |
| `provisioning` | definition value, else omit (provider default) |

Attach:

| Field | Source |
|---|---|
| `vm_id` | CHV VM id |
| `host_id` | node id |
| `attachment_id` | CHV attachment id |
| `expected_volume_generation` | last inspected generation |
| `access_mode` | `single_writer`; `read_only` only when the definition says so |
| `requested_frontend` | `virtio-blk` |
| `vmm_disk_id` | the CHV disk id when the VMM configured one |

Detach:

| Field | Source |
|---|---|
| `attachment_id` | the attachment being released (required in the JSON body; the path carries only `volume_id`) |
| `expected_attachment_generation` | attach response / last inspect |
| `vm_stopped_or_io_drained_proof` | `vm_stopped` when the VM is stopped; `io_drained` only after CHV's own drain check |

Grow:

| Field | Source |
|---|---|
| `new_size_bytes` | requested size |
| `expected_generation` | last inspected volume generation |

Report attachment state (only when the pin advertises `attachment_state_report`):

| Field | Source |
|---|---|
| `attachment_id` | the attachment being reported (required in the JSON body) |
| `expected_attachment_generation` | attach response / last inspect (the detach fence) |
| `state` | `advertised` only in v1 (CHV configured the frontend); `active` refused without guest confirmation |
| `evidence` | `vmm_configured` for an `advertised` report |
| `reporter` | CHV agent identity, as an untrusted label (at most 1024 bytes, no control characters) |
| `detail` | optional bounded diagnostic |

Destroy:

| Field | Source |
|---|---|
| `expected_generation` | last inspected volume generation |
| `data_erasure_policy` | operator policy; no implicit default |

### 11.4 Response consumption

- Attach returns `frontend` and `state`. For `native-local` v1, `frontend` is
  `{"virtio_blk":{"host_device_path":"..."}}`.
- CHV passes `host_device_path` to Cloud Hypervisor as the disk path, and
  records `attachment_id`, `attachment_generation`, and
  `volume_generation`. It mints a fresh `attachment_id` per attach and never
  reuses one after detach; a `consumed id` refusal is
  `IDEMPOTENCY_CONFLICT`, not a retryable transient.
- A `prepared` state is recorded as §8 requires. When the pin advertises
  `attachment_state_report`, CHV records `state_revision` and `evidence` from
  inspect/report verbatim; `attachment_generation` is never treated as an
  evidence counter.
- Grow returns `backing_resized`, `guest_notification_status`, and
  `effective_size_bytes`, but no generation. The agent MUST re-inspect to
  record the new volume generation before the next mutation.
- Detach returns an inspect snapshot (`InspectVolumeResponse`); the agent
  records its `generation` and the remaining `attachment_ids`. A post-detach
  report is `NOT_FOUND` for a new operation; a replay of a recorded report
  returns the recorded outcome.
- `evidence_status` and `health` are recorded verbatim; `unknown` stays
  unknown.

### 11.5 Startup readiness

The agent MUST, before advertising the `native-local` class:

1. reach `/healthz`;
2. fetch `/v2/capabilities`;
3. verify API compatibility. The capabilities payload carries no
   `api_version`; compatibility is established by the `/v2` route set and by
   the `volvisor.volume.v2` envelope that every request carries (a rejected
   envelope is `INVALID_REQUEST`). The agent MUST fail the handshake rather
   than retry an incompatible revision;
4. verify the provider is `lvm`; `fake` is refused outside dev/test builds;
5. verify the required capability tokens are present.

Failure keeps the class disabled and the volume owner unusable. It MUST NOT
fall back to stord for Volvisor-owned volumes.

## 12. Legacy `chv-stord` binding

1. The legacy binding keeps its existing semantics exactly. This contract
   adds no new stord features.
2. `seed_from`, snapshot, clone, restore, and disk migration remain available
   **only** for `legacy-stord` volumes.
3. The legacy path MUST route through the same backend interface so that both
   the Core runtime and the legacy reconcile path share one dispatch point.
   Behaviour MUST be equivalent to the current stord calls, including error
   codes and idempotency expectations.
4. The legacy class normalisation (`""` → `local`, `local-file`/`localdisk` →
   `local`) stays scoped to the legacy backend.
5. No new code may read `stord_config_path` to decide an owner.

## 13. Conformance

A backend implementation conforms only if it passes:

1. **Ownership:** cross-backend open/attach/detach refusals in both
   directions.
2. **Idempotency:** replay after a simulated crash at each boundary returns
   the recorded outcome and creates no second attachment.
3. **Conflict:** same `operation_id` with a different payload is
   `IDEMPOTENCY_CONFLICT`; stale generations are `STALE_GENERATION`.
4. **Capability refusal:** every unsupported operation is `UNIMPLEMENTED`
   before a backend call.
5. **Identifier conformance:** non-conforming ids are rejected, not rewritten.
6. **Phase honesty:** a `prepared` attachment is never reported as `active`.
7. **Error mapping:** every backend code in §9 maps to the stated CHV error
   and retry classification.
8. **Token hygiene:** the admin token never appears in logs, errors, metrics,
   or process arguments.
9. **Legacy regression:** the full existing stord suite is unchanged with the
   interface in place.
10. **Contract drift:** the Volvisor schema test fails when the pinned
    revision's request/response fields change.

Volvisor-specific conformance MUST run against the exact pinned revision. A
passing run against a different revision is not evidence.

## 14. Rollback

1. Ownership is per volume and immutable while attached. Rolling back a
   deployment means stopping new Volvisor creates, not rewriting owner rows.
2. A Volvisor-owned volume remains Volvisor-owned forever in v1. There is no
   in-place conversion to stord.
3. Stopping `volvisord` makes Volvisor-owned volumes unavailable. That is an
   honest failure; it MUST NOT be papered over by opening the volume with
   stord.
4. The agent MUST refuse to start with a volume row whose owner cannot be
   served, unless the operator explicitly marks the node as
   `storage-degraded` (operations on that owner fail closed; other owners
   continue).

## 15. Non-goals

- Cross-host migration of `native-local`.
- Snapshot/clone/restore for Volvisor-owned volumes.
- `ceph-rbd` or `nearline-replicated` in v1.
- Automatic adoption of stord volumes.
- Exposing Volvisor's API to any component other than `chv-agent`.
- Treating Volvisor capability presence as a production-support claim.
