# Test Evidence Tiers

This page records **what is actually executed, where, and with what evidence**.
It exists to stop three failure modes: claiming a mocked suite exercises the
real wire, claiming a self-hosted job ran when no runner was available, and
treating a skipped capability check as a pass (#659).

## Tier 1 — Pull requests (GitHub-hosted, blocking)

Every PR to `main` runs `.github/workflows/ci.yml` on `ubuntu-latest`:

| Job | Evidence produced | Gate |
|-----|-------------------|------|
| `Rust checks` | fmt/check/clippy `--all-targets`, `cargo nextest --workspace --no-fail-fast`, doctests, process-level security/startup smokes, architecture and packaging guards | Blocking (required check) |
| `Rust coverage (ratchet, lines >= 76%)` | lcov artifact `rust-coverage-lcov`, `cargo llvm-cov report --summary-only` in the job summary | Fails the workflow; required-check addition is an admin step |
| `UI checks` | `npm run build` + `svelte-check` | Blocking (required check) |
| `UI unit tests (Vitest)` | Vitest results | Fails the workflow; required-check addition is an admin step |
| `E2E tests` | Playwright against `ui/tests/e2e/mock-bff.mjs` and per-spec `page.route` fixtures; HTML report + traces uploaded on failure | Blocking (required check) |
| `Real-BFF E2E` | Playwright against a **real `chv-controlplane`** (BFF router + serde), disposable SQLite, migrations, seeded admin. Control-plane log, harness log, HTML report and traces uploaded on failure | Fails the workflow; required-check addition is an admin step |

### Coverage ratchet

`cargo llvm-cov` publishes line coverage on every PR. The recorded `main`
cargo-llvm-cov baseline is **78.92% lines**; the job fails below **76%**
(`--fail-under-lines 76`), leaving ~2.9 points of headroom for unrelated
refactors. Raise the threshold as the baseline rises; never lower it to make a
red PR green.

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

All of it is uploaded as an artifact on success **and** failure.

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

The PR that added the ratchet and nightly/real-BFF lanes ran on a development
container, not the GitHub-hosted or self-hosted fleets. Specifically:

- `nightly-kvm-qualification.yml` was **not executed** — no runner with
  `/dev/kvm` and the `chv-kvm` label was available. Its YAML was parsed
  locally; the first real signal is the scheduled/manual run on the runner.
- The real-BFF harness and Playwright spec were exercised locally against a
  `chv-controlplane` built with `--features dev` (disposable SQLite, seeded
  admin): all 3 tests passed, and the spec skipped cleanly with
  `REAL_BFF_BASE_URL` unset. The GitHub job itself was not run; its first real
  signal is the PR/push run.
- No claim is made that the self-hosted scenarios are green on the current
  candidate until the nightly lane reports on that candidate.
