# ADR-014: API Evolution and Compatibility

## Status
Accepted

## Date
2026-06-06

## Context

CHV's gRPC and HTTP/JSON APIs are consumed by:

- Internal Rust crates via tonic-generated stubs (the `tonic-prost-build` codegen regenerates from `proto/` on every `cargo build` — `tonic-build` before the #235 stack bump)
- The SvelteKit UI via webui-bff (HTTP/JSON over `webui-bff.proto`)
- External operators (CLI, scripts, monitoring integrations) — current and future
- Third-party tooling that may rely on stable field numbers in serialised proto binary form

Without explicit evolution rules, contributors may:

- Reuse a retired field number after deletion, causing silent decode-time data corruption for clients
  that still ship old code recognising that number as a different field
- Rename a field, breaking JSON consumers that reference it by name
- Remove a service method without a deprecation cycle, breaking callers on upgrade
- Treat `INTERNAL` errors as catch-alls, denying clients the structured error codes they need to decide
  whether to retry, surface to users, or escalate

**Proto-history audit (2026-06-06):** A full `git log -p` audit of all seven proto files in `proto/`
(`control-plane-node.proto`, `chv-nwd-api.proto`, `chv-stord-api.proto`, `chv-stord-migration.proto`,
`webui-bff.proto`, `webui-tasks.proto`, `webui-viewmodels.proto`) shows **no field-number removals or
renames** since file introduction. The protos have followed additive-only discipline implicitly. This ADR
codifies that discipline explicitly and adds the CI gate to enforce it going forward.

## Decision

### 1. Proto field-number lifecycle

- **Never reuse a field number.** Once a field is removed, its number is permanently retired.
- **Always emit `reserved` on deletion.** When removing a field from a message, add the following
  lines **before** the existing fields (alphabetically within their section if multiple exist):

  ```proto
  message Foo {
    // Field 5 was previously `old_name` — removed 2026-MM-DD, do not reuse.
    reserved 5;
    reserved "old_name";

    // ... remaining fields ...
  }
  ```

- **Never renumber existing fields.** If a field must move semantically, retire the old number with
  `reserved` and add a new field at the next available number.
- **Enum values are append-only.** Never reuse an enum number; never reorder. Clients that
  receive an unknown enum value treat it as the zero value — they must not crash.

### 2. Field deprecation lifecycle

1. Mark the field `[deprecated = true]` at least one *minor* release before removal.
2. Document the replacement field (if any) in an adjacent comment.
3. Remove the field only after the deprecation appeared in a published release.
4. Add `reserved` lines in the same commit that removes the field.

A deprecated field is still wire-compatible — the annotation is purely informational for
tooling and humans.

### 3. Message and service evolution

- **Never remove a service or RPC** without a full minor-release deprecation cycle (mark the RPC
  deprecated, announce the timeline in the CHANGELOG, then remove).
- **Never change the semantics** of an existing field without a new field number. Add a new field for
  the new semantics; deprecate the old.
- **Never change a field type** (e.g. `string` → `bytes`). This is a wire-breaking change. Add a new
  field instead.
- **Streaming cardinality** (`repeated` vs singular) is a breaking change. Never add or remove
  `repeated` from an existing field.

### 4. Breaking-change CI gate

`buf breaking --against '.git#branch=main,subdir=proto'` runs on every pull request touching
`proto/**` (see `.github/workflows/proto.yml`). A failing check blocks merge unless:

1. The PR carries a `breaking-change` label, **and**
2. The CHANGELOG entry for the next release documents the migration path for downstream consumers.

### 4a. Naming-rule deferrals — pre-existing v1 protos

The `buf lint` gate runs `STANDARD` rules. Seven of those rules surface pre-existing
structural naming debt in protos that have already shipped under `chv.controlplane.node.v1`,
`chv.node.nwd.v1`, `chv.node.stord.v1`, `chv.webui.bff.v1`, `chv.webui.tasks.v1`, and
`chv.webui.viewmodels.v1`. Each is narrowly excepted in `proto/buf.yaml` with a comment
linking to this section. The gate continues to catch any *new* naming violations introduced
in future commits — we are deferring fixes for the existing debt, not silencing the rule
class wholesale.

| Rule | What it flags | Why a fix is currently blocked |
|------|---------------|--------------------------------|
| `FILE_LOWER_SNAKE_CASE` | Filenames such as `control-plane-node.proto`, `chv-nwd-api.proto`, `webui-bff.proto` (kebab-case). | Renaming the file changes every `import "<path>";` statement in the workspace and every `tonic_build::compile_protos!(...)` invocation in `crates/*/build.rs` (4 sites). The path is also baked into `gen/rust/**` module names. |
| `RPC_REQUEST_STANDARD_NAME` | RPC requests not named `<Verb><Noun>Request` (e.g. `EnrollmentRequest`, `NodeStateReport`, `OverviewRequest`, `VolumeHealthRequest`). | Renaming the message renames the generated Rust struct, breaking every consumer crate that constructs or matches on it. |
| `RPC_RESPONSE_STANDARD_NAME` | RPC responses not named `<Verb><Noun>Response`. Same root cause as the request rule. | Same as above. |
| `RPC_REQUEST_RESPONSE_UNIQUE` | A single message reused across many RPCs — specifically `chv.controlplane.node.v1.AckResponse` (~67 sites), `chv.node.stord.v1.Result` (~8 sites), and `chv.node.stord.v1.MigrationMessage`. | Splitting the shared message into per-RPC types is **wire-breaking**: the proto descriptor for each RPC's response changes, so an old binary on one side of an in-flight upgrade decoding a new-format reply would mis-parse. Avoiding wire incompatibility during rolling upgrades is the load-bearing reason. |
| `DIRECTORY_SAME_PACKAGE` | The `proto/node/` directory holds both `chv.node.nwd.v1` and `chv.node.stord.v1`. | The `node/` grouping is intentional: it co-locates protos consumed by the **agent/stord/nwd binaries that share a deployment target** (the node), per ADR-002. Splitting requires a directory reorg + import-path migration. |
| `ENUM_VALUE_PREFIX` | Enum values such as `OVERLAY_NONE`, `ACK_OK`, `MIGRATION_ERROR_DISK_FULL` lack the buf-recommended type-name prefix. | Rename changes the generated Rust enum variant identifier, breaking every `match` arm and constructor in consumer crates. |
| `ENUM_ZERO_VALUE_SUFFIX` | Enum zero values are domain-meaningful (`OVERLAY_NONE`, `DIRECTION_BOTH`, `ACK_OK`) instead of the buf-recommended `_UNSPECIFIED` sentinel. | Same Rust-side variant rename impact. The current zero values are also semantically load-bearing — `OVERLAY_NONE` is a real configuration, not "unset" — so renaming would also obscure intent unless paired with a value-shape redesign. |

#### Migration path

Each excepted rule will be fixed in a **v2 release of the affected proto package**
(`chv.controlplane.node.v2`, `chv.node.stord.v2`, etc.). The v1 → v2 transition is exactly
the case the §1 `reserved` discipline and the §3 deprecation lifecycle are designed for:
v1 keeps shipping in parallel under its current name until consumers have migrated, and v2
introduces the buf-conformant filenames, message names, and enum names from day one.

#### Removal trigger

An entry is removed from `proto/buf.yaml`'s `lint.except` list as soon as **every proto
file** previously violating that rule has either:
1. Been migrated into a v2 package that conforms to the rule, **and**
2. Had its v1 file deleted (or the v1 file independently renamed/restructured to conform).

Removing an exception while v1 files still violate it would re-break the gate. Adding a
new v1 file that violates an excepted rule is forbidden — `buf lint` no longer catches it,
so reviewers must manually enforce conformance for any new file added to `proto/`.

#### Tracking

The deferral is currently tracked only by this ADR section — no separate issue exists yet.
A follow-up issue titled "Plan v2 proto migration to clear buf lint deferrals" must be
filed and linked here before the first stable release. Until that issue exists, the
deferral is documented but not scheduled — that is acceptable while CHV is pre-1.0 but
must not persist past 1.0.0.

### 5. Pagination

All list endpoints **must** use cursor-based pagination per [AIP-158](https://google.aip.dev/158):

| Field | Type | Direction |
|---|---|---|
| `page_size` | `int32` | request |
| `page_token` | `string` | request |
| `next_page_token` | `string` | response |

- Servers treat `page_size = 0` as "use server default" (typically 100).
- Clients treat `next_page_token = ""` as end-of-results.
- Servers must never emit a `next_page_token` that, when used, returns an empty page — callers
  would loop forever.

### 6. Error responses

- gRPC errors use tonic `Status` with one of the canonical codes from `google.rpc.Code`. Choose the
  narrowest applicable code:
  - `NOT_FOUND` — resource does not exist
  - `ALREADY_EXISTS` — creation conflict
  - `INVALID_ARGUMENT` — client-supplied value is structurally invalid
  - `FAILED_PRECONDITION` — operation invalid given current resource state
  - `PERMISSION_DENIED` — authenticated but not authorised
  - `UNAUTHENTICATED` — no valid credentials
  - `RESOURCE_EXHAUSTED` — quota or rate limit
  - `UNAVAILABLE` — transient server-side failure, safe to retry with backoff
  - `INTERNAL` — unexpected server error (not client-correctable, not retryable)
- HTTP/JSON error bodies from the BFF include `message`, `code`, and `request_id` fields.
  The `request_id` matches the `x-correlation-id` response header and is injected by the
  correlation middleware so every error can be traced without the client needing to copy a header.
  The HTTP-level versioning and deprecation policy is separate from proto evolution — see
  [BFF HTTP API Versioning](../ops/bff-http-api-versioning.md).

## Alternatives Considered

### "We will never ship a breaking change"

Cynicism check: every team has shipped an unintended breaking change. Rules as code via `buf breaking`
are cheaper than rules as social convention. Rejected.

### Strict semver per proto file

Each proto file carries its own `v1`/`v2`/`v3` version with independent release cadence.

Pros: consumers can pin to a specific version without coupling to the service release schedule.
Cons: enormous coordination cost; rarely matches CHV's cadence at this scale.

Rejected. Additive-only `reserved` + `deprecated` is sufficient for our current consumer surface.

### google.rpc.Status rich error details

Using `google.rpc.Status` with `details` for rich, typed error payloads.

Pros: strongly typed error metadata; standard interop with google.rpc ecosystem.
Cons: adds `google/rpc/status.proto` dependency, touches every service proto.

Deferred — tracked as finding H-15. When implemented, this ADR will be updated.

## Consequences

- `proto/buf.yaml` defines STANDARD lint + FILE breaking rules.
- `.github/workflows/proto.yml` runs `buf lint` on every change and `buf breaking` on every PR.
- Contributors must run `buf breaking --against '.git#branch=main,subdir=proto'` locally before
  pushing any proto change.
- The CHANGELOG must document deprecated fields added in each release.
- Existing protos already follow additive-only discipline (confirmed by audit above). This ADR is
  primarily a forward-looking enforcement mechanism.

## References

- buf documentation: https://buf.build/docs
- AIP-158 (cursor pagination): https://google.aip.dev/158
- AIP-4 (proto versioning): https://google.aip.dev/4
- ADR-009 (logging and observability): [009-logging-and-observability.md](009-logging-and-observability.md)
- ADR-008 (error handling patterns): [008-error-handling-patterns.md](008-error-handling-patterns.md)
