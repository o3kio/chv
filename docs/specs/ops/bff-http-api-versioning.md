# BFF HTTP API Versioning

Status: current contract and policy. The BFF does not implement runtime
version negotiation.

## Scope

This document covers the browser/operator HTTP/JSON surface served by
`chv-webui-bff`, which is embedded in `chv-controlplane` and exposed on the
control-plane HTTP listener (packaged/`install.sh` default
`127.0.0.1:8080`, normally behind nginx). It does **not** cover:

- the native CellHV Core local API on `/run/chv/core/core-v1.sock`
  ([`cellhv-core-native-api-v1.md`](../cellhv-core-native-api-v1.md));
- the gRPC node/control-plane contracts governed by
  [ADR-014](../adr/014-api-evolution.md);
- the legacy `/api/v1/*` control-plane mount (see §"The `/api/v1` surface").

## `/v1` is a path prefix, not a negotiable version

Every supported BFF route is mounted under the literal `/v1/` prefix
([`crates/chv-webui-bff/src/router.rs`](../../../crates/chv-webui-bff/src/router.rs)).
There is **no** `API-Version` request or response header, no `Accept` media-type
version parameter, no query-parameter version, and no server-side version
negotiation. The correlation middleware adds `x-correlation-id`; the CORS
allow-list includes `Authorization`, `Content-Type`, `Accept`,
`x-correlation-id`, and `x-csrf-token` — nothing version-related.

Consequences:

- A client must call the path it supports (today, `/v1/...`). Sending an
  `API-Version` header has no effect and may be rejected by a proxy if the
  proxy is configured to allow only specific headers.
- The server cannot select an older behavior for an older client. Compatibility
  is preserved by keeping `/v1` backward-compatible, not by negotiating.
- `Deprecation` and `Sunset` response headers are **not** emitted today.
  Deprecation is announced through the release notes, `CHANGELOG.md`, and this
  document.

## What is additive within `/v1`

The following are non-breaking and may ship in a minor release:

- a new route under `/v1`;
- a new optional request field whose absence preserves the old behavior;
- a new response field (clients must ignore fields they do not know);
- a new enum value or a new stable error `code` (clients must handle an
  unknown value without crashing and fall back to a generic error path);
- new optional response metadata/headers;
- relaxed validation that accepts requests the previous release rejected;
- documentation and message-text changes that do not change the stable
  `code` or HTTP status for a given condition.

Examples of additive changes in the current surface include new read
endpoints and new typed error codes. They do not require a `/v2`.

## What is breaking and requires a new major path

The following require a new major HTTP prefix (for example `/v2`) and a
deprecation window for `/v1`:

- removing or renaming a route, request field, response field, or stable error
  `code`;
- changing a field's type, units, encoding, or meaning;
- making an optional request field required, or tightening validation so a
  previously valid request now fails;
- changing the HTTP status associated with a stable condition;
- changing authentication, cookie, CSRF, or CORS requirements in a way that
  invalidates existing clients;
- removing or reordering enum values in a way clients cannot tolerate;
- changing idempotency or retry semantics.

A new route is additive; a new field on an existing resource is additive only
if clients are expected to ignore unknown fields (they are). Adding a required
field to a request is breaking.

## Deprecation policy

When a breaking change is planned:

1. **Announce** it in `CHANGELOG.md`, the release notes, and this document.
   State the affected route/field/code, the replacement, and the removal
   release. Because no runtime deprecation headers exist, the release notes
   are the authoritative announcement.
2. **Keep the old surface working** for at least one published minor release.
   For surfaces with external consumers, target a minimum 90-day wall-clock
   window as well; that wall-clock target is a maintainer policy decision, not
   something the server enforces today.
3. **Mark the contract** in the corresponding proto field with
   `[deprecated = true]` when the payload is defined there, and document the
   replacement next to it (ADR-014 §2). For hand-wired HTTP routes, add a
   comment at the route table and a row here.
4. **Remove only with the major path available.** Existing `/v1` clients keep
   working through the window; `/v2` ships alongside, not instead of, `/v1`
   until the window closes.
5. **Record the removal** in the CHANGELOG and this document, including the
   release and date. Do not silently delete a route or field.

Deprecating a route is not the same as fencing a feature. A fenced feature
returns `501 NOT_IMPLEMENTED` deterministically; a deprecated route still
works until removal. Do not use one to stand in for the other.

## Error and correlation contract

BFF error bodies are top-level JSON objects with `message`, `code`, and
`request_id`; `request_id` equals the `x-correlation-id` response header and
is injected by the correlation middleware. An inbound `x-operation-id` seeds
the correlation ID. This is the ADR-014 §6 error contract.

The control plane's unmatched-route fallback is different: it returns a 404
with a nested `{"error":{"code":"NOT_IMPLEMENTED",...}}` object. That is the
control-plane fallback, not a BFF handler response; clients should not assume
every 4xx body has the flat BFF shape. See
[BFF API Errors](../../runbooks/bff-errors.md).

Stable `code` values are part of the contract; human-readable `message` text
is not. A client may display the message but must branch on `code` and HTTP
status.

## Relationship to `proto/webui/webui-bff.proto` and ADR-014

- [`proto/webui/webui-bff.proto`](../../../proto/webui/webui-bff.proto)
  declares package `chv.webui.bff.v1` and the `WebUiQueryService` /
  `WebUiMutationService` payload contracts. It is the typed schema source for
  the BFF/UI payloads.
- [ADR-014](../adr/014-api-evolution.md) governs **proto** evolution:
  field numbers are never reused, removals emit `reserved`, fields are
  deprecated before removal, enums are append-only, and
  `buf breaking --against '.git#branch=main,subdir=proto'` gates changes.
- The HTTP route table in `router.rs` is hand-wired. A proto package version
  (`chv.webui.bff.v1`) and the HTTP path prefix (`/v1`) are aligned by
  convention, not by a shared runtime version router. A change can touch one
  without the other, so reviewers must check both:
  - proto changes follow ADR-014 and the `buf` gates;
  - HTTP route/shape changes follow this document and the compatibility
    matrix.
- A new proto package major (`chv.webui.bff.v2`) does not automatically create
  a `/v2` HTTP surface, and a `/v2` HTTP surface does not automatically
  require a new proto package. The mapping is a review decision recorded in
  the change.

## The `/api/v1` surface

`/api/v1/*` is a **separate legacy control-plane mount**, not a second version
of the BFF contract. It includes the API-only auth endpoints used by
non-browser clients (`POST /api/v1/auth/login` is the bearer-token login used
by `chvctl`; `GET /api/v1/auth/me` and `POST /api/v1/auth/logout` also exist);
most other `/api/v1` routes are legacy or stubs (and some are explicitly
fenced). They are not covered by the `/v1` deprecation policy in this
document.

Use `/v1/*` for the supported browser/operator API. Use
`POST /api/v1/auth/login` when a non-browser client needs a bearer token.

## Compatibility and review checklist

Before merging a BFF HTTP change:

- [ ] Is the change additive under §"What is additive within `/v1`"?
- [ ] If breaking, is a new major path planned and is the old surface kept
      for the deprecation window?
- [ ] Are stable `code` values unchanged for unchanged conditions?
- [ ] Are new response fields optional/ignored by existing clients?
- [ ] If a proto payload changed, did `buf breaking` pass and is the change
      consistent with ADR-014?
- [ ] Is the change recorded in `CHANGELOG.md` / this document when it is a
      deprecation or removal?
- [ ] Does the [compatibility matrix](compatibility-matrix.md) still describe
      a supported combination?

## Verification notes

- The absence of `API-Version`/negotiation was checked in the BFF router,
  correlation middleware, and CORS allow-list; no code reads or writes such a
  header today.
- The route list and auth/CSRF behavior are grounded in
  `crates/chv-webui-bff/src/router.rs`,
  `crates/chv-webui-bff/src/error.rs`,
  `crates/chv-webui-bff/src/csrf_middleware.rs`, and
  `crates/chv-webui-bff/src/correlation_middleware.rs`.
- This document records policy; it does not claim a `/v2` surface exists.
  There is no `/v2` HTTP router today.
