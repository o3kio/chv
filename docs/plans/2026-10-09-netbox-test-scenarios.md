# NetBox Integration Test Scenarios — Implementation Plan

Date: 2026-10-09
Status: Executed — shipped 2026-10-09 with ADR-024 (tracking issue
kubedoio/chv#586, closed 2026-10-09).

References:

- Parent campaign: issue #239 (merged via PRs #569–#581); design of record
  `docs/design/issue-239-netbox-projection.md` (DP1–DP12)
- Contracts: `docs/specs/architecture-designer/contracts/netbox-api-contract.md`,
  `docs/specs/architecture-designer/contracts/netbox-mapping-contract.md`
- Qualification boundary: `docs/evidence/netbox-projection-e2e.md`
- Companion docs this campaign adds (see PR 1): ADR-024
  (`docs/specs/adr/024-netbox-test-doubles-and-demo-gate.md`), a test-doubles
  section in `docs/specs/component/architecture-designer-netbox-projection.md`,
  and a simulator-conformance clause in the mapping contract
- Tracking issue: kubedoio/chv#586

## Goal

A small, effective integration-test suite that proves the NetBox integration
works and is usable — without installing full NetBox in PR CI — and that every
future NetBox integration (host projection, disk projection, inventory
autodiscovery, …) extends instead of reinvents.

## Non-goals

- No change to projection behavior, mapping, or wire contracts.
- No bidirectional sync; autodiscovery (a read path) is called out as future
  **design** work, not implemented here.
- No per-PR CI dependency on Docker or on network access to anything external.

## Why a third test double (strategy)

The merged campaign has two lanes already (their composed half is
superseded by the simulator below — lane 2):

1. **Adapter unit tests** (pure mapping/plan/ownership) — fast, exhaustive.
2. **wiremock composed e2e** (`netbox_projection_e2e_tests.rs`, 5 scenarios) —
   proves worker/runner/store behavior, but the mocks are stateless and
   hand-scripted per test. The same authors write the client and the mock, so
   a mock can silently agree with wrong client behavior (this materially
   happened in the PR-8 review round: two fabricated-shape mocks masked a real
   envelope bug).

What is missing is a double with **NetBox's own semantics**, not ours:
server-side natural-key filtering, pagination envelopes, id/url assignment,
custom fields, 401/404/400 behavior, and faultable state. That double serves
three purposes at once: a stronger composed test target, an interactive
"can a human use this" demo backend, and the shared foundation for the future
integrations. Full NetBox (docker compose: netbox + postgres + redis) stays
out of PR CI and becomes an on-demand qualification lane that also validates
the simulator itself (same scenarios, two backends).

| Question | Answered by |
|---|---|
| Does the projection logic behave (idempotent, safe, isolated)? | unit + wiremock (exists) — lane 1 |
| Does the client speak real NetBox wire format & semantics? | **simulator** (new) — lane 2 |
| Can a human use it end-to-end (config → export → runs)? | **`make netbox-demo`** (new, PR 4) — built on the simulator, not a separate lane |
| Does it work against today's real NetBox? | **qualification compose** (new, on-demand + weekly) — lane 3 |

## Implementation-surface analysis (where the code goes)

| Surface | Convention to follow | Concrete touchpoints |
|---|---|---|
| Simulator crate | workspace member under `crates/`, axum (already a workspace dep) | `crates/chv-netbox-sim` — lib (in-process server) + opt-in bin behind `required-features = ["bin"]` so `cargo build --workspace` (and release packaging, which only picks `packaging/nfpm/*.yaml`-listed binaries) never builds the dev tool unintentionally |
| Simulator scope | exactly the client's used surface: `GET` lists with `limit`/`offset` + natural-key/custom-field filters on the six endpoint families (`dcim/devices`, `virtualization/virtual-machines`, `virtualization/interfaces`, `ipam/prefixes`, `ipam/vlans`, `ipam/ip-addresses`), `POST`/`PATCH`/`DELETE`, `Authorization: Token …` auth, `{count, next, previous, results}` envelopes, `custom_fields` | no other NetBox API is emulated; anything else returns 404 like real NetBox |
| Fixtures | golden request/response pairs, one directory per NetBox major | `crates/chv-netbox-sim/tests/fixtures/netbox4/` — provenance recorded; refreshable via `--record` against a real instance (Lane 3) |
| Composed tests | `chv-controlplane-service` dev-deps pattern (like the current wiremock self-dev-dep) | `chv-netbox-sim` as dev-dependency; scenarios run the sim in-process on an ephemeral port via the existing `test-http` client constructor |
| Demo harness | `scripts/*.sh` + `Makefile` targets (see `docs/release/PIPELINE.md` conventions) | `scripts/netbox-demo.sh`, `make netbox-demo` |
| Demo plain-HTTP gate | default-off cargo feature, never in release builds (packaging builds default features only) | `netbox-demo` feature on `cmd/chv-controlplane` enabling the adapter's `test-http` constructor **and** requiring `CHV_NETBOX_ALLOW_HTTP=1` at runtime (double gate) |
| Qualification | docker compose under `deploy/`, scripted scenarios | `deploy/netbox-qualification/docker-compose.yml` + `scripts/netbox-qualify.sh` |
| Qualification CI | workflow files in `.github/workflows/` (model: `integration-kvm.yml`) | `netbox-qualification.yml` — `workflow_dispatch` + weekly cron, docker on `ubuntu-latest` |
| Companion docs | ADR under `docs/specs/adr/` (index in `docs/decisions/README.md`); spec sections + contract clauses under `docs/specs/` | ADR-024 (test-double strategy + demo gate), spec section in the existing netbox component spec, simulator-conformance clause in the existing mapping contract — all in PR 1 |

Validation ladder per PR: `cargo check -p <pkg>` → `cargo test -p <pkg>` →
`cargo clippy -p <pkg> --all-targets -- -D warnings`; widen to the workspace
when the service dev-dep graph or `cmd/` changes. UI-touching PRs (demo docs
aside, none expected) would run `cd ui && npm run build`.

---

## PR 1 — Adopt the documentation set (no code)

**Scope:**

- This plan; tracking issue filed with the lane matrix and the
  future-integration extension recipe below as its checklist skeleton.
- **ADR-024 — NetBox test doubles and the plain-HTTP demo gate**
  (`docs/specs/adr/024-netbox-test-doubles-and-demo-gate.md`): context (the
  same-author mock-drift failure observed in the #239 PR-8 review round; the
  qualification boundary in the evidence doc), decision (three-lane strategy,
  simulator scope rules — never a production dependency, never the source of
  truth for wire format; the compile-feature + runtime-env double gate for
  plain HTTP; qualification as the drift tripwire), consequences. ADR index
  row in `docs/decisions/README.md`. This is also the permanent home of the
  high-risk-change disclosure for the demo gate (per `CONTRIBUTING.md`).
- **Component-spec extension**: a "Test doubles, demo harness, and
  qualification" section in
  `docs/specs/component/architecture-designer-netbox-projection.md` — sim
  responsibilities and scope rules, the `__`-prefixed control endpoints,
  the demo gate, the qualification lane. No new spec file: `chv-netbox-sim`
  is a dev tool, not a deployed component.
- **Mapping-contract clause**: a short "Simulator conformance" section in
  `docs/specs/architecture-designer/contracts/netbox-mapping-contract.md` —
  the simulator implements exactly the surface
  this contract defines; golden fixtures under
  `crates/chv-netbox-sim/tests/fixtures/` are the captured wire truth;
  fixtures are never hand-edited after a real capture (refresh via
  `--record`). No new contract files: the BFF API surface is unchanged by
  this campaign, and the sim's control endpoints are an internal test
  interface documented in the spec section + rustdoc.

**Proves:** the strategy, the security posture of the demo gate, and the
simulator's fidelity rules are agreed before any infrastructure lands.

---

## PR 2 — `chv-netbox-sim`: stateful NetBox simulator

**Scope:**

- New crate `crates/chv-netbox-sim`:
  - `server.rs` — axum router, in-process start (`NetboxSim::start()` →
    ephemeral port, reused by tests) and a `[[bin]] netbox-sim` target with
    `required-features = ["bin"]` (fixed port, seed file, fault flags via
    CLI/env) for the demo harness and interactive use.
  - In-memory state per kind: objects with `id`, `url`, `created`,
    `last_updated`, `custom_fields`; next-id counter.
  - Real NetBox 4.x semantics on the six endpoint families:
    - `GET` list: `limit`/`offset` pagination with `count`/`next`/`previous`
      (absolute URLs, `next` null on last page); server-side filtering by the
      exact query params the client sends (natural keys per kind +
      custom-field filters); `limit=0` = server max page like NetBox
      (pinned by the first `--record` qualification run before it is
      wired in).
    - `POST` → `201` with assigned `id`/`url`; duplicate natural key → `400`
      with NetBox's error body shape; `PATCH` → `200`; `DELETE` → `204`;
      missing id → `404` `{"detail": "Not found."}`.
    - Auth: `Authorization: Token <t>` validated against the configured
      token(s); anything else → `401` NetBox-shaped body.
  - Test-control plane (mounted only under a configurable prefix, documented
    as never-enabled-by-NetBox):
    - `POST /__seed` — bulk-load objects (with chosen ids/custom fields).
    - `GET /__state` — full dump for assertions.
    - `POST /__reset` — clear state + faults.
    - `POST /__faults` — error injection: force `401` / `429` / `5xx` /
      latency / connection-drop, per kind or globally; replaces the
      hand-mounted wiremock outage stubs with stateful failure behavior.
  - Fixture fidelity: `tests/fixtures/netbox4/*.json` pinning response
    envelopes and object shapes; sim unit tests assert byte-shape fidelity
    (serde round-trip against fixtures). Initial fixtures derive from the
    mapping contract's documented examples; provenance header records origin
    and is refreshed by `--record` (PR 5).

**Tests (in-crate):** pagination math (offset beyond count, `next`/`previous`
links), natural-key filter matrix per kind, custom-field filter, auth
rejection, duplicate-key 400, 404 body shape, fault injection of every kind,
seed/reset/state round-trip.

**Proves:** the client runs against behavior we did not script per-test.

---

## PR 3 — Composed suite moves onto the simulator

**Scope:**

- `chv-controlplane-service` gains a dev-dependency on `chv-netbox-sim`
  (mirrors the existing wiremock self-dev-dep comment block: never in the
  production graph).
- New `netbox_projection_sim_tests.rs` (or the e2e file parameterized over
  the backend) running the five merged composed scenarios against the
  in-process sim: full lifecycle (apply → export → re-export leaves sim
  state unchanged — the all-`no_op` property, currently proven only in the
  worker suite, becomes a state-based assertion); foreign-occupied natural
  key → conflict, no write; outage → apply result unchanged; partial
  failure → requeue → resume without duplicate create; double enqueue → one
  active run. Assertions switch from wiremock request counting to
  `GET /__state` (stronger: asserts resulting NetBox state, not just calls
  made).
- The stateful wiremock mounts in `netbox_projection_e2e_tests.rs` are
  removed once the sim suite covers them; wiremock stays only for
  protocol-level client tests (`client_wire_tests.rs`: redirect refusal,
  pagination same-origin `next`-link guards, status classification —
  malformed-body/parse-failure cases are in-crate unit tests in
  `client.rs`) where a raw socket-level double is the right tool.
- Outage/retry scenarios move to `/__faults` injection.

**Proves:** end-to-end projection behavior against NetBox-shaped state;
single stateful double instead of two half-ones (suite stays small).

---

## PR 4 — `make netbox-demo`: interactive harness

**Scope:**

- `netbox-demo` feature (default-off) on `cmd/chv-controlplane`:
  - enables `chv-netbox-adapter/test-http`;
  - bootstrap's worker client factory (the existing `with_client_factory`
    seam) uses the plain-HTTP constructor **only** when
    `CHV_NETBOX_ALLOW_HTTP=1` is also set — double gate, loud comments, and a
    startup log line marking demo mode. Default-feature builds keep the
    fail-closed HTTPS-only path untouched. (Implementation note: the feature
    is forwarded to `chv-webui-bff` as well — its synchronous dry-run owns a
    second client seam that carries the same double gate; and because the
    BFF's config-upsert enforces HTTPS on accept, the demo script seeds the
    projection config row directly in sqlite — see
    `docs/dev/netbox-demo.md`.)
- `scripts/netbox-demo.sh`:
  1. builds `netbox-sim` (bin feature) and `chv-controlplane --features
     netbox-demo`, plus `ui && npm run build` if `ui/build` is stale;
  2. temp workspace: sqlite controlplane DB + generated config, sim on a
     fixed localhost port with a generated token;
  3. `--seed` flag (default on) drives the BFF API: creates a demo
     architecture (or reuses one) and the NetBox projection config pointing
     at the sim, so the user lands in the UI ready to press **Export** /
     **Dry run**;
  4. prints the UI URL, the sim `/__state` URL, and the click-path
     (architecture → NetBox tab → config → dry run → export → run history);
  5. traps Ctrl-C, tears both processes down.
- `Makefile` target `netbox-demo` wrapping the script.
- Short doc: `docs/dev/netbox-demo.md` (or the operations doc's dev section)
  covering the click-path and how to simulate faults live
  (`curl -X POST .../__faults`).

**Proves:** a human can configure and drive the integration end-to-end with
zero NetBox installation — the "we can use it" gate.

---

## PR 5 — Lane 3: real-NetBox qualification (on-demand + weekly)

**Scope:**

- `deploy/netbox-qualification/docker-compose.yml`: pinned
  `netboxcommunity/netbox` + postgres + redis; init service creates the
  superuser and a dedicated API token; all state under a named volume for
  re-runs; `make netbox-qualify-env` prints the connection env.
- The PR-3 sim scenarios gain a second backend mode: when
  `NETBOX_QUALIFICATION_URL` + `NETBOX_QUALIFICATION_TOKEN` are set, the same
  tests (marked `#[ignore]`) run against that instance instead of the
  in-process sim — **one scenario codebase, two backends**. The drift check
  is then: run the suite against the sim and against real NetBox; both green
  means the sim still models reality.
- `scripts/netbox-qualify.sh`: compose up → wait healthy → run the ignored
  suite with the env → capture a summary → compose down. Also supports
  `--record` to re-capture the `netbox4` golden fixtures from the live
  instance into `crates/chv-netbox-sim/tests/fixtures/`.
- `.github/workflows/netbox-qualification.yml`: `workflow_dispatch` + weekly
  cron, ubuntu-latest (docker available), runs the script, uploads the run
  log artifact on failure.
- `docs/evidence/netbox-projection-e2e.md` extended with the first real
  qualification run's evidence (NetBox version, commit, results), replacing
  the "qualification boundary" caveat with a dated entry.

**Proves:** the integration works against today's real NetBox, and the
simulator is kept honest.

---

## PR 6 — Client write-path conformance (added during execution)

**Why:** the PR-5 review pass over the qualification lane found the
adapter's write path would fail against real NetBox even with the
prerequisites provisioned. Verified against NetBox v4.7.2 source, the
gaps were:

1. **Tags** were written as bare strings (`["chv-team"]`). NetBox's
   `NestedTagSerializer` (`netbox/netbox/api/serializers/features.py`)
   resolves nested references only as a numeric PK or an attrs dict
   (`WritableNestedSerializer.to_internal_value` →
   `get_related_object_by_attrs`) — plain strings are a 400.
2. **Create order** was children-before-parents (`ip → interface → vm
   → device` after vlan/prefix). Real NetBox resolves nested-FK writes
   by existence, so a VM referencing a not-yet-created device fails.
3. **Device creates** omitted `device_type` (required — no
   `required=False` on `DeviceSerializer.device_type`) and `role`
   (likewise required; the `Device.role` model FK is non-nullable).

**Scope:**

- `chv-netbox-adapter` (`client.rs`, `mapping.rs`): all six write
  bodies send tags as name dicts; device bodies carry
  `device_type: {"manufacturer": {"slug": "chv"}, "slug": "chv-host"}`
  (a device-type slug is only unique per manufacturer — NetBox's
  `DeviceType.Meta.constraints`) and `role: {"slug": "chv-node"}`;
  `NetBoxKind` is reordered to the FK-dependency rank
  `vlan → prefix → device → vm → interface → ip` (declaration order is
  the rank). New consts `CHV_NETBOX_MANUFACTURER`/`CHV_NETBOX_DEVICE_TYPE`/
  `CHV_NETBOX_DEVICE_ROLE` pin the prerequisite slugs.
- `chv-netbox-sim`: mirrors the required-field semantics (device
  creates missing `device_type`/`role` are 400s, existence still not
  validated — the sim's documented posture), and `SimKind::ALL` is
  reordered to match the rank. The golden fixtures' relation ids
  shift where the new order makes a name-reference resolve to a real
  row instead of a synthetic one (an authored-fixture refresh, still
  pre-capture).
- Recorder + compose init: provision the `chv-node` device role; the
  recorder's seed bodies use the client's write forms in both
  dialects.
- Controlplane sim tests: `vlan_seed_body` mirrors the client's
  name-dict tag form.

Read surface and custom-field surface unchanged — `MAPPING_VERSION`
stays `v1`; the contract's rule 3, write-reference forms, and
provisioning prerequisites are documented in the mapping contract.

**Proves:** a qualification dispatch fails only on NetBox-side
environment, never on client write-form drift; the sim's 400s are the
PR-local tripwire for any future write-path regression.

---

## Extension recipe for the future integrations

Every new NetBox integration follows the same three-touch pattern — no new
test infrastructure:

| Integration | NetBox surface | Recipe |
|---|---|---|
| Host projection | `dcim/devices` (+ `dcim/interfaces` for host NICs) | mapping-contract kinds → adapter mapping/plan units → sim endpoints + filters → one composed sim scenario → qualification script step |
| Disk projection | `dcim/inventory-items` (component model on devices) | same as above |
| Inventory autodiscovery | **read path** — pulling state *from* NetBox | **design work first**: the projection DPs forbid bidirectional sync by design; a read path needs its own DP entries (ownership of locally-created objects that NetBox-side changes touch, conflict semantics, refresh cadence). The sim already serves reads, so tests are ready when the design lands. |

Rules of thumb: the sim only grows endpoints the client actually uses; every
sim behavior must trace to a golden fixture or the mapping contract; PR CI
never grows a Docker dependency — new heavyweight scenarios go to the
qualification lane.

## Risks and coordination notes

- **Plain-HTTP escape hatch**: the demo feature is the first place the
  production binary can be built with the adapter's test constructor. Gated
  twice (compile feature + runtime env), default-off, never in release
  packaging, and called out in the PR description as a high-risk-change
  disclosure per `CONTRIBUTING.md`.
- **Fixture provenance**: fixtures authored without a real capture are
  hypotheses; the PR-5 `--record` run is the correction mechanism. Do not
  hand-edit fixtures after a capture.
- **Sim drift**: the weekly qualification run is the tripwire; a failure
  there means the sim (and possibly the client) diverged from real NetBox —
  fix by re-recording fixtures and reconciling the sim, never by weakening
  the scenario.
- **CI cost**: the qualification workflow adds one ~10 min weekly job; PR CI
  gains only the sim suite (in-process, seconds).
