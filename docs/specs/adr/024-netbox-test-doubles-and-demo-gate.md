# ADR-024 NetBox Test Doubles and the Plain-HTTP Demo Gate

Date: 2026-10-09
Status: Accepted (implemented 2026-10-09; status reconciled 2026-11-19; companion plan: `docs/plans/2026-10-09-netbox-test-scenarios.md`; issue: kubedoio/chv#586)

## Context

ADR-023 established the NetBox projection: a one-way, idempotent downstream
copy driven by `chv-netbox-adapter`'s HTTPS-only client. The merged campaign
(#569–#581) validated it with adapter unit tests and a wiremock-based
composed e2e suite. Two structural gaps surfaced:

1. **Same-author mock drift.** The wiremock mounts are stateless and
   hand-scripted per test by the same authors who wrote the client, so a mock
   can silently agree with wrong client behavior. This materially happened:
   in the campaign's PR-8 review round, two fabricated-shape mocks masked a
   real result-envelope bug until a cross-layer review caught it. What is
   missing is a double with **NetBox's own semantics** (pagination envelopes,
   server-side natural-key filtering, id/url assignment, NetBox-shaped error
   bodies), not ours.
2. **No usable-without-NetBox story.** Exercising the integration
   end-to-end (UI → BFF → worker → NetBox) today requires installing full
   NetBox (netbox + postgres + redis). Operators and developers have no cheap
   way to confirm "the integration works and we can use it", and nothing
   periodically confirms the client still works against a current NetBox
   release — the campaign explicitly deferred real-NetBox qualification
   (see `docs/evidence/netbox-projection-e2e.md`).

Future NetBox integrations (host projection, disk projection, inventory
autodiscovery) need a shared test foundation they extend rather than
reinvent per campaign.

## Decision

1. **A first-party stateful simulator, `chv-netbox-sim`.** A workspace crate
   providing an in-process axum server that emulates **exactly the REST
   surface the adapter client uses** (six endpoint families, `limit`/`offset`
   pagination with `count`/`next`/`previous`, server-side natural-key and
   custom-field filters, `POST`/`PATCH`/`DELETE` semantics, `Authorization:
   Token` auth, NetBox-shaped 400/401/404 bodies). Anything outside that
   surface returns 404, like real NetBox. The simulator is a **dev-only
   tool**: a library for in-process tests plus an opt-in bin behind
   `required-features` so `cargo build --workspace` (and release packaging,
   which installs only the binaries listed in `packaging/nfpm/`) never
   builds it unintentionally.

2. **The simulator is never the source of truth for the wire format.** Its
   behavioral requirements are the mapping contract
   (`docs/specs/architecture-designer/contracts/netbox-mapping-contract.md`);
   its wire shapes are pinned by golden
   fixtures under `crates/chv-netbox-sim/tests/fixtures/` with recorded
   provenance. Fixtures are never hand-edited after a real capture; they are
   refreshed only by the qualification `--record` mode against a live
   NetBox. The simulator must never appear in a production dependency graph.

3. **Test-control plane is `__`-prefixed and obviously not NetBox.**
   `POST /__seed`, `GET /__state`, `POST /__reset`, `POST /__faults`
   (force 401/429/5xx/latency/connection-drop) exist for tests and the demo
   harness. The `__` prefix plus the spec section keep them unambiguous.

4. **Real NetBox stays out of PR CI; qualification is on-demand plus
   weekly.** A pinned docker compose (netbox + postgres + redis) runs the
   **same** composed scenarios against a real instance (`#[ignore]`d tests +
   env-provided URL/token). Running the identical scenarios against both
   backends makes the qualification run double as a simulator-drift tripwire:
   a divergence is fixed by re-recording fixtures and reconciling the
   simulator, never by weakening the scenario.

5. **The plain-HTTP demo gate is double-gated and never shipped.** The
   production client rejects non-HTTPS endpoints fail-closed; that invariant
   is unchanged. A default-off `netbox-demo` cargo feature may enable the
   adapter's test-only plain-HTTP constructor **and only when
   `CHV_NETBOX_ALLOW_HTTP=1` is also set at runtime**, for the
   `make netbox-demo` harness (simulator + controlplane + built UI over
   sqlite). The feature is forwarded through the crates that own a NetBox
   client seam: `cmd/chv-controlplane` → `chv-controlplane-service` (the
   projection worker's `with_client_factory` seam) → `chv-webui-bff` (the
   synchronous dry-run's client seam) — each seam applies the same double
   gate and fails closed without it. Release packaging builds default
   features only, so the escape hatch cannot reach a shipped binary.
   Demo-mode startup logs a loud marker. This is the recorded
   high-risk-change disclosure for that gate.

6. **Future integrations extend, never reinvent.** Each new NetBox
   integration adds mapping-contract kinds, adapter unit tests, simulator
   endpoints/filters, one composed scenario, and a qualification-script step.
   Read-path features (inventory autodiscovery) require their own design
   decisions first — ADR-023's no-bidirectional-sync posture is untouched by
   this ADR.

## Rationale

- More wiremock was rejected: it cannot provide server-side semantics
  without re-implementing the simulator inside each test, and its
  hand-scripted nature is the drift risk being solved.
- Full NetBox in PR CI was rejected: it adds Docker, three containers, and
  minutes of setup to every PR, and NetBox releases move independently of
  CHV PRs — a moving target that belongs in a periodic lane, not a blocking
  one.
- The simulator is first-party (not a generic HTTP mock library) because the
  value is precisely NetBox-specific semantics maintained next to the client
  that consumes them, in the same repo and review stream.
- The double gate (feature + env) exists because either gate alone is a
  realistic accident: a feature accidentally enabled, or an env var leaking
  into a real deployment.

## Consequences

- New dev-only surface: `crates/chv-netbox-sim` (lib + opt-in bin), golden
  fixtures, `scripts/netbox-demo.sh`, `make netbox-demo`,
  `deploy/netbox-qualification/`, `scripts/netbox-qualify.sh`, and a weekly
  `netbox-qualification.yml` workflow. No production dependency graph, wire
  contract, or behavior change.
- The composed e2e suite consolidates onto the simulator; wiremock survives
  only for protocol-level client tests (`client_wire_tests.rs`: redirect
  refusal, pagination same-origin `next`-link guards, status classification
  — malformed-body/parse-failure cases are in-crate unit tests in
  `client.rs`). Suite count stays flat while coverage deepens
  (state-based assertions via `/__state` instead of request counting).
- `cmd/chv-controlplane` (and, via feature forwarding, the service and BFF
  crates' NetBox client seams) grow a `netbox-demo` feature; reviewers must
  treat any change to the gate conditions (feature name, env var, factory
  seams) as high-risk and re-verify default-feature builds keep the
  fail-closed HTTPS-only path.
- Simulator fidelity is a maintained obligation: the weekly qualification
  run is the tripwire, and fixture refreshes are reviewable events.
- The demo harness depends on the converged controlplane serving shape
  (UI + BFF from one binary) and on the BFF API remaining stable enough for
  seeding; deliberate BFF breaking changes must update `scripts/netbox-demo.sh`.
