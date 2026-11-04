# Test Evidence Tiers

This page records **what is actually executed, where, and with what evidence**.
It exists to stop three failure modes: claiming a mocked suite exercises the
real wire, claiming a self-hosted job ran when no runner was available, and
treating a skipped capability check as a pass (#659).

## Tier 1 — Pull requests (GitHub-hosted, blocking)

Every PR to `main` runs `.github/workflows/ci.yml` on `ubuntu-latest`.
Jobs marked **PR/push-only** are skipped when `release.yml` calls this workflow
with `skip_optional_checks: true`; direct PR/push runs use the default `false`:

| Job | Evidence produced | Gate |
|-----|-------------------|------|
| `Rust checks` | fmt/check/clippy `--all-targets`, `cargo nextest --workspace --no-fail-fast` (default profile, `retries = 0`), doctests, process-level security/startup smokes, architecture and packaging guards | Blocking (required check) |
| `Rust coverage (ratchet, lines >= 76%)` | `cargo llvm-cov nextest --profile coverage` + coverage-only retry profile, lcov artifact `rust-coverage-lcov`, `cargo llvm-cov report --summary-only` in the job summary | PR/push-only; fails the workflow below 76% lines; required-check addition is an admin step |
| `UI checks` | `npm run build` + `svelte-check` | Blocking (required check) |
| `UI unit tests (Vitest)` | Vitest results | Fails the workflow; required-check addition is an admin step |
| `E2E tests` | Playwright against `ui/tests/e2e/mock-bff.mjs` and per-spec `page.route` fixtures; HTML report + traces uploaded on failure | Blocking (required check) |
| `Real-BFF E2E` | Playwright against a **real `chv-controlplane`** (BFF router + serde), disposable SQLite, migrations, seeded admin. Control-plane log, harness log, HTML report and traces uploaded on failure | PR/push-only; fails the workflow; required-check addition is an admin step |

### Coverage floor

`cargo llvm-cov nextest` publishes line coverage on every PR. The reproducible
`main` baseline is **78.97% lines**, measured from the lcov artifact of run
[41669901088](https://github.com/o3kio/chv/actions/runs/41669901088/job/126035158484).
The job fails below **76%** (`--fail-under-lines 76`), leaving ~3 points of
headroom for unrelated refactors.

Nextest runs each test in its own process. Only the `coverage` nextest profile
in `.config/nextest.toml` enables retries (`retries = 2`,
`fail-fast = false`); the default profile used by `Rust checks` keeps
`retries = 0`, so the merge gate does not absorb intermittent failures. The
coverage command also passes `--no-fail-fast` so the report is not aborted by
the first failure. This matters because a blocking libtest run previously
aborted the whole report on one 5 s scheduling flake.

This is a **floor, not a trend ratchet**. It does not compare the current PR
against the previous one, so slow drift between baseline refreshes is
invisible. Refresh the number from a `main` lcov artifact and raise it
deliberately; never lower it to make a red PR green. A per-PR delta check is a
follow-up (it needs a stable main-baseline service) and is intentionally not
added yet.

Publishing the report happens in the workflow. Making the
`Rust coverage (ratchet, lines >= 76%)` check *required* for merges is a GitHub
ruleset change — see [BRANCH_PROTECTION.md](../governance/BRANCH_PROTECTION.md)
and the admin note there.

### What the real-BFF lane does and does not cover

`scripts/integration/real-bff-e2e.sh`:

1. boots `chv-controlplane --features dev` with `CHV_ALLOW_INSECURE=1`
   (loopback only) and a disposable SQLite database;
2. applies the checked-in production migrations;
3. stops the process, seeds one `admin` user with a per-run bcrypt hash, and
   restarts it (the WAL-safe window used by the qualification harness);
4. waits for `/health`, preflights `/v1/auth/login`, then exports
   `REAL_BFF_BASE_URL` / `REAL_BFF_E2E_USER` / `REAL_BFF_E2E_PASSWORD`;
5. runs `ui/tests/e2e/real-bff.spec.ts`, which logs in through the SvelteKit
   UI and asserts the real `/v1/overview` JSON contract plus a fail-closed
   rejection of bad credentials.

It is an in-process control plane with no agent, VM, KVM, or external network.
It proves the HTTP/auth/serialization boundary, **not** VM lifecycle behavior.

## Tier 2 — Scheduled self-hosted KVM (nightly / weekly, visible)

`.github/workflows/nightly-kvm-qualification.yml` targets
`[self-hosted, linux, x64, chv-kvm]`:

| Schedule | Lane | Scenarios |
|----------|------|-----------|
| Daily 02:00 UTC | chaos | `scripts/integration/qual/m4.7-faults.sh` |
| Monday 03:00 UTC | full | `m4.3-lifecycle`, `m4.4-network`, `m4.5-storage`, `m4.6-migration`, `m4.7-faults`, `m4.8-perf-soak` |
| Manual dispatch | either | choose `chaos` or `full` |

Each scenario runs through `scripts/integration/qual/deploy.sh --exec` with
release binaries staged from the checked-out candidate and the **patched**
guest image (`noble-qual-patched.img`) that all recorded M4.3–M4.8 evidence
used. The driver (`scripts/integration/qual/nightly-run.sh`) calls
`env-preflight.sh --stage-binaries`, then `patch-guest-image.sh`, and writes:

- `logs/env-preflight.log` and `logs/<scenario>.log` (full output),
- `results.tsv` (scenario, status, rc, duration),
- `summary.md` + the GitHub step summary,
- copies of each scenario's own `m4.N-artifacts` evidence directory.

All of it is uploaded as an artifact on success **and** failure. The driver
sets `umask 022` and re-runs `chmod -R a+rX` after each scenario so earlier
evidence survives a later hang, and a failed evidence copy emits an
`::error::` annotation. A separate upload requires `results.tsv`
(`if-no-files-found: error`); if even that marker is missing, the run has no
usable qualification record and the upload fails.

The workflow uses one concurrency group with `cancel-in-progress: false`.
GitHub keeps at most one running and one pending run per group, so a newer
scheduled run replaces an older **queued** run in that group (the active run
is never killed). That prevents an unbounded backlog of stale nightly/weekly
runs; the replacement is visible in the Actions UI.

**Prerequisites are infrastructure, not secrets.** The runner needs
`/dev/kvm`, passwordless sudo, cloud-hypervisor, the pinned firmware/guest
image, disk headroom, and the `chv-kvm` label. No repository secret is
required. If the runner is offline the scheduled run stays **queued**; that is
visible in the Actions UI and is never a green result. If the runner exists but
cannot meet the prerequisites, the driver emits an `::error::` annotation and
exits non-zero. See
[self-hosted-runner-requirements.md](../release/self-hosted-runner-requirements.md).

### What the nightly lanes prove

They are the same scenario scripts recorded in
`docs/evidence/production-readiness/v0.3.0-rc1/04-real-host-qualification/`:
real Cloud Hypervisor VMs, taps/nft, storage provisioning, two-stord mTLS
migration, mid-operation kills, and the bounded soak baseline. They are a
**regression tripwire on qualified hardware**, not a scale or production-SLA
claim. The m4.8 numbers remain baselines for that host class.

## Tier 3 — Manual / maintainer-attended

These still require a human, credentials, or infrastructure outside CI and are
**not** claimed by this repository's automated lanes:

- Release signing keys (`CHV_RELEASE_GPG_KEY` / cosign) and the protected
  release environments.
- Packaged-install qualification on a real host / clean container images
  beyond the container smokes in `package-pr.yml` / `package-nightly.yml`.
- First-time runner provisioning and the periodic reset of the self-hosted KVM
  runner.
- Any production-like multi-node, scale, HA, or backup/restore claim (backup
  remains explicitly out of the RC supported matrix).

## Unverified in the change that introduced these tiers

This section records the state at introduction and is updated when new
evidence lands. As of 2026-11-04:

- `nightly-kvm-qualification.yml` **has executed** on the self-hosted runner
  (schedule and `workflow_dispatch`). Its first live runs
  ([41836073033](https://github.com/o3kio/chv/actions/runs/41836073033),
  [41894243197](https://github.com/o3kio/chv/actions/runs/41894243197))
  failed every scenario for **harness configuration drift**, not a candidate
  defect: `deploy.sh` did not share the #693 agent-auth master with the agent,
  and the two-stord scenarios reused the post-#687 default metrics port. A
  later chaos rerun also exposed a third gap: the scenarios predate #663, so
  their migration receivers had neither `migration.node_id` nor a signing
  secret, and their triggers carried no authorization token. All three are
  fixed, with an offline invariant test
  (`tests/scripts/qual-harness-invariants.sh`) in PR CI so the config contract
  cannot rot again. Root cause, fixes, and the re-qualification run are in
  [nightly-kvm-2026-11-04.md](../evidence/production-readiness/v0.3.0-rc1/04-real-host-qualification/nightly-kvm-2026-11-04.md).
- The real-BFF harness and Playwright spec were exercised locally against a
  `chv-controlplane` built with `--features dev` (disposable SQLite, seeded
  admin): all 3 tests passed, and the spec skipped cleanly with
  `REAL_BFF_BASE_URL` unset. The strengthened assertions landed in commit
  `f6b76f88`; the GitHub-hosted `Real-BFF E2E` job passed on exactly that
  commit in
  [run 41696156512](https://github.com/o3kio/chv/actions/runs/41696156512/job/126127119260)
  (4m35s).
- The lane is a **tripwire, not an installer test**: it generates its own
  deployment shape. Any change to the config/secret/port contract must be
  reflected in `deploy.sh` and the scenario generators, which is what the new
  invariant test checks.
- No claim is made that the self-hosted scenarios are green on the current
  candidate until the nightly lane reports green on that candidate.
