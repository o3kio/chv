# CHV Release Engineering Plan

> Version: 0.1.0  
> Status: Draft  
> Scope: Release pipeline, packaging, CI/CD, and artifact distribution for the CHV project.

---

## 1. Current Repository Findings

This is a factual audit of release-related assets that exist **today** in the repository.

### Versioning
- `VERSION` file at repo root contains `0.1.0`. This is the source of truth.
- `Cargo.toml` workspace `version` field is synchronized with `VERSION` via `scripts/bump-version.sh`.
- `ui/package.json` version is also synchronized by the bump script.
- `CHANGELOG.md` exists with an `[Unreleased]` section and dated release notes.

### Binaries
Five release binaries are produced from the Cargo workspace:

| Binary | Source | Role |
|--------|--------|------|
| `chvctl` | `cmd/chvctl` | CLI client (clap-based) |
| `chv-controlplane` | `cmd/chv-controlplane` | API/management plane (gRPC + HTTP BFF, SQLite, migrations) |
| `chv-agent` | `cmd/chv-agent` | Node agent (KVM lifecycle) |
| `chv-stord` | `cmd/chv-stord` | Storage daemon |
| `chv-nwd` | `cmd/chv-nwd` | Network daemon (bridges, DHCP, eBPF) |

### Web UI
- SvelteKit app in `ui/`.
- Built to `ui/build/`.
- Served as static assets by `chv-controlplane`.

### Packaging Infrastructure (Already Implemented)
- **`packaging/nfpm.yaml`** — envsubst-driven nfpm template for `.deb` and `.rpm`.
- **`packaging/systemd/*.service`** — four systemd units:
  - `chv-controlplane.service`
  - `chv-agent.service`
  - `chv-stord.service`
  - `chv-nwd.service`
- **`packaging/scripts/*-postinstall.sh`** — `chv-controlplane-postinstall.sh`, `chv-node-postinstall.sh`.
- **`packaging/scripts/*-preremove.sh`** — `chv-controlplane-preremove.sh`, `chv-node-preremove.sh`.
- **`scripts/build-packages.sh`** — local package builder that uses `nfpm` + `envsubst`. Builds three packages in both `.deb` and `.rpm` formats.
- **`scripts/smoke-packages.sh`** — packaging smoke test that checks binaries, UI build, nfpm config, package contents, and metadata.
- **`docs/PACKAGING.md`** — installation, upgrade, and uninstall instructions for Debian and RHEL families.
- **`docs/examples/systemd/*.service`** — example units (updated to `/usr/bin/` paths).

### Release Scripts
- **`scripts/build-release.sh`** — tarball assembler (`dist/chv-<VERSION>-linux-amd64.tar.gz`). Includes binaries, UI build, migrations, systemd units, nginx config, example TOMLs, and `install.sh`.
- **`scripts/bump-version.sh`** — bumps `VERSION`, `Cargo.toml`, `ui/package.json`, sidebar label, docs, and `Cargo.lock`.
- **`scripts/install.sh`** — all-in-one installer (creates bridges, certs, networks, seeds VMs). Debian-centric (`apt-get`, `dpkg`).

### CI Workflows
- **`.github/workflows/ci.yml`** — PR + `main` checks:
  - Rust: `cargo fmt`, `cargo check --workspace`, `cargo clippy --workspace -D warnings`, `cargo test --workspace`
  - UI: `npm ci`, `npm run build`, optional `npm run check`
  - E2E: Playwright browser install + `npm run test:e2e` (needs UI build, 15 min timeout)
- **`.github/workflows/release.yml`** — triggered on `v*` tags or manual dispatch:
  - `build` job: compile Rust release, build UI, assemble tarball, upload binaries/UI/tarball as artifacts.
  - `package` job: download artifacts, install nfpm, run `scripts/build-packages.sh --skip-build`, upload `.deb`/`.rpm` artifacts.
  - `release` job: download tarball + packages, generate SHA256 checksums, generate SBOM (`anchore/sbom-action`), generate build provenance attestation (`actions/attest-build-provenance`), create GitHub Release with attached assets.

### Makefile Targets
- `build`, `build-ui`, `release`, `dev-install`, `test`, `fmt`, `bump-version`.

### Tests
- ~48 Rust source files contain `mod tests`.
- Dedicated integration tests:
  - `crates/chv-stord-core/tests/smoke.rs`
  - `crates/chv-nwd-core/tests/nwd_daemon.rs`
- UI e2e tests via Playwright.
- No `chv init` or `chv doctor` commands exist yet.
- No dedicated package install/remove/upgrade tests exist yet.

### Gaps in Current State
- No `nightly` or `RC` CI workflows.
- No artifact repository (apt/dnf) — packages are manual download only.
- No package signing.
- No clean-install or upgrade/remove automated tests in CI.
- No KVM-required integration tests in CI (GitHub-hosted `ubuntu-latest` lacks `/dev/kvm`).
- PR artifacts are not retained explicitly; CI uploads them as workflow artifacts with default retention.

---

## 2. Proposed Package Split

We keep the **three-package split** already implemented in `scripts/build-packages.sh` and documented in `docs/PACKAGING.md`. This is the correct model for CHV's architecture.

### Packages

| Package | Binaries | Purpose | Deployed On |
|---------|----------|---------|-------------|
| `chvctl` | `chvctl` | CLI client for operators | Management workstations, CI pipelines, any host |
| `chv-controlplane` | `chv-controlplane` | API plane, Web UI BFF, SQLite DB, migrations | Dedicated management host(s) |
| `chv-node` | `chv-agent`, `chv-stord`, `chv-nwd` | Hypervisor node services (KVM VMs, storage, networking) | Compute nodes |

### Rationale

1. **Separation of concerns**: The control plane can run on hosts that never see KVM or raw networking. Nodes can scale horizontally without pulling in UI assets or database migrations.
2. **Security surface**: `chv-node` requires `CAP_NET_ADMIN`, `CAP_NET_RAW`, and `/dev/kvm` access. Keeping it separate limits privilege escalation on the management plane host.
3. **Upgrade granularity**: Control plane upgrades (which include DB migrations and UI asset changes) can roll independently of node agent restarts.
4. **Dependency clarity**: `chv-node` declares a package dependency on `chv-controlplane` so that a single-node install is `chv-controlplane` + `chv-node`, while a remote node only needs `chv-node` pointed at an existing control plane.
5. **CLI portability**: `chvctl` is a single static-ish binary with no runtime dependency on the other packages. It can be distributed standalone.

---

## 3. Proposed File Layout for Packaging

All packaging assets live under `packaging/`. Build orchestration stays in `scripts/`.

```
packaging/
├── nfpm.yaml                      # envsubst template (already exists)
├── systemd/
│   ├── chv-controlplane.service   # (already exists)
│   ├── chv-agent.service          # (already exists)
│   ├── chv-stord.service          # (already exists)
│   └── chv-nwd.service            # (already exists)
├── scripts/
│   ├── chv-controlplane-postinstall.sh   # (already exists)
│   ├── chv-controlplane-preremove.sh     # (already exists)
│   ├── chv-node-postinstall.sh           # (already exists)
│   └── chv-node-preremove.sh             # (already exists)
└── configs/
    └── (future: per-package default config fragments if needed)

scripts/
├── build-packages.sh              # (already exists)
├── build-release.sh               # (already exists)
├── bump-version.sh                # (already exists)
├── smoke-packages.sh              # (already exists)
├── install.sh                     # all-in-one installer (already exists)
├── dev-install.sh                 # dev-only install (already exists)
└── (future)
    ├── test-install-clean.sh      # clean-install smoke test
    ├── test-upgrade.sh            # upgrade path test
    └── test-remove.sh             # remove/purge test

docs/
├── PACKAGING.md                   # user-facing packaging docs (already exists)
└── release/
    └── release-engineering-plan.md   # this document
```

### Notes
- `docs/examples/systemd/*.service` are **examples** for tarball users; `packaging/systemd/*.service` are the **canonical units** embedded in packages.
- `docs/examples/*.toml` are example configs for tarball/manual installs.
- `cmd/chv-controlplane/migrations/` are embedded in `chv-controlplane` package.

---

## 4. Proposed Versioning Model

### Source of Truth
The `VERSION` file at repository root is the single source of truth.

### Format
`MAJOR.MINOR.PATCH` (e.g., `0.1.0`).

This is a SemVer three-segment scheme.

### Synchronization Chain

```
VERSION file
    │
    ├──> Cargo.toml  (workspace.version)
    │       └──> Cargo.lock (via cargo update --workspace)
    │
    ├──> ui/package.json
    │       └──> ui/package-lock.json
    │
    ├──> ui/src/lib/components/shell/Sidebar.svelte (version label)
    │
    ├──> README.md
    ├──> docs/DEPLOYMENT.md
    ├──> scripts/install.sh
    ├──> scripts/hosting/cloudflare-worker.js
    ├──> scripts/hosting/github-pages-index.html
    ├──> cloudflare-worker.js        (hand-published copy; must stay byte-identical)
    └──> github-pages-index.html     (hand-published copy; must stay byte-identical)
```

`cloudflare-worker.js` and `github-pages-index.html` are duplicated at the
repository root for manual publishing to the hosting surfaces. They are
copies of the `scripts/hosting/` files, are rewritten together by
`scripts/bump-version.sh`, and must not drift; `scripts/check-release-local.sh`
fails if they differ.

### Git Tags
- Stable releases: `v<VERSION>` (e.g., `v0.1.0`).
- Release candidates: `v<VERSION>-rc.N` (e.g., `v0.0.0.5-rc.1`).
- Nightlies: `nightly-YYYYMMDD` (e.g., `nightly-20260510`). No tag push required if using workflow dispatch; tag only if a nightly is promoted.

### Alignment with Package Versions
- `.deb` and `.rpm` versions are derived directly from `VERSION`.
- `scripts/build-packages.sh` reads `VERSION` and passes it to `nfpm`.
- There is **no separate package epoch or revision** for stable releases; the three-segment version is sufficient.
- If a packaging-only fix is needed without code changes, bump `PATCH` and re-run the pipeline.

### Workflow
1. Developer runs `make bump-version BUMP_TYPE=patch`.
2. Review diff; update `CHANGELOG.md`.
3. Commit and open PR.
4. On merge to `main`, CI validates.
5. To cut an RC: tag `v0.0.0.5-rc.1` and push.
6. To cut stable: tag `v0.0.0.5` and push.

---

## 5. Proposed CI Workflow Map

### Workflows

| Workflow | File | Trigger | Purpose |
|----------|------|---------|---------|
| **PR Checks** | `.github/workflows/ci.yml` | `pull_request` to `main` | Fast feedback: fmt, check, clippy, unit tests, UI build, Playwright e2e |
| **Main Branch** | `.github/workflows/ci.yml` | `push` to `main` | Same as PR checks; ensures `main` is always green |
| **Nightly Build** | *(new)* `.github/workflows/nightly.yml` | Scheduled `cron: '0 3 * * *'` + `workflow_dispatch` | Full build, package, tarball, PR-artifact-style retention; no release created |
| **RC Build** | *(extend)* `.github/workflows/release.yml` | `push` to tags matching `v*-rc.*` | Same jobs as stable release, but publishes to a **pre-release** GitHub Release; attaches `-rc` packages |
| **Stable Release** | `.github/workflows/release.yml` | `push` to tags matching `v*` (not `*-rc.*`) | Full build, package, SBOM, provenance, checksums, GitHub Release |
| **Package Smoke** | *(new job in ci.yml or standalone)* | PR + `main` | Run `scripts/smoke-packages.sh` to validate packaging metadata |
| **Clean Install Test** | *(new)* `.github/workflows/test-install.yml` | `push` to `main`, RC tags, stable tags | Spin up container/VM, install packages, assert services start |

### Diagram: What Runs When

```
PR opened ──────────────────────────────> ci.yml (rust, ui, e2e, package-smoke)
                                              │
                                              ▼
                                         Merge to main
                                              │
                    ┌─────────────────────────┼─────────────────────────┐
                    ▼                         ▼                         ▼
               ci.yml (fast)          nightly.yml (03:00 UTC)    test-install.yml
               - rust checks          - release build            - dpkg install
               - unit tests           - package build             - systemd start
               - ui build             - tarball                   - health check
               - e2e tests            - artifact upload           - remove test
               - package-smoke        (no release created)
                    │
                    ▼
            Tag pushed: v0.1.1-rc.1
                    │
                    ▼
            release.yml (prerelease=true)
               - build
               - package
               - SBOM + provenance
               - GitHub Pre-Release
                    │
                    ▼
            Tag pushed: v0.1.1
                    │
                    ▼
            release.yml (prerelease=false)
               - build
               - package
               - SBOM + provenance
               - GitHub Release (stable)
```

### Artifact Flow

```
build job
    ├── release-binaries (5 executables)
    ├── ui-build
    └── release-tarball
            │
            ▼
    package job (needs: build)
    └── packages (.deb + .rpm)
            │
            ▼
    release job (needs: [build, package])
    └── GitHub Release + SBOM + checksums + provenance
```

---

## 6. Proposed Test Levels

### Matrix

| Test Level | What It Covers | Where It Runs | Frequency | Blocking For |
|------------|----------------|---------------|-----------|--------------|
| **Rust unit tests** | `cargo test --workspace`; ~48 `mod tests` + dedicated integration test files | GitHub Actions `ubuntu-latest` | PR + `main` | PR merge, stable release |
| **Lint / static checks** | `cargo fmt --check`, `cargo check --workspace`, `cargo clippy --workspace -D warnings` | GitHub Actions `ubuntu-latest` | PR + `main` | PR merge, stable release |
| **UI build + check** | `npm run build`, optional `npm run check` | GitHub Actions `ubuntu-latest` | PR + `main` | PR merge, stable release |
| **Playwright e2e** | UI end-to-end tests | GitHub Actions `ubuntu-latest` | PR + `main` | PR merge, stable release |
| **Package build tests** | `scripts/smoke-packages.sh` (binary presence, nfpm config, package contents, metadata) | GitHub Actions `ubuntu-latest` | PR + `main` | PR merge, stable release |
| **Clean install smoke tests** | Install `.deb` in clean container; assert files present, `systemctl daemon-reload` succeeds, units parse | GitHub Actions `ubuntu-latest` (container) | `main` + RC | RC promotion, stable release |
| **Upgrade / remove tests** | Install old version → upgrade to new → assert DB migration OK → remove → assert no crash, data preserved | GitHub Actions `ubuntu-latest` (container) | RC + stable | Stable release |
| **KVM integration tests** | Full VM lifecycle via `chv-agent` on real KVM host | Self-hosted runner OR manual | Nightly / manual | Nothing (informational until runner exists) |

### KVM Integration Tests
- **Why not in PR checks**: GitHub-hosted `ubuntu-latest` does not expose `/dev/kvm`.
- **Target**: A self-hosted runner on a bare-metal or nested-KVM-enabled VM.
- **Scope**: Agent bootstrapping, VM creation, serial console attachment, storage backend I/O, network bridge + DHCP.
- **Trigger**: Nightly cron, or manual `workflow_dispatch`.
- **Fallback until runner exists**: Run manually on a dev box with `./scripts/dev-install.sh` + `cargo test`.

---

## 7. Publishing Policy

### Artifact Retention

| Artifact Type | Retention | Where |
|---------------|-----------|-------|
| PR build artifacts (binaries, UI, tarball) | 30 days | GitHub Actions artifacts |
| `main` branch artifacts | 90 days | GitHub Actions artifacts |
| Nightly artifacts | 30 days | GitHub Actions artifacts (not a Release) |
| RC artifacts | Until superseded by next RC or stable | GitHub Pre-Release assets |
| Stable artifacts | Permanent | GitHub Release assets |

### PR Artifacts
- Each PR build uploads `release-binaries`, `ui-build`, and `release-tarball` as workflow artifacts.
- These are **not** Releases. They allow QA to download and test a PR build without building locally.
- Retention: 30 days (GitHub default for public repos; configurable for private).

### Nightly Repository
- **Not an apt/dnf repo yet.** Nightlies are stored as GitHub Actions artifacts.
- A future enhancement is to publish nightlies to an S3-backed apt repository or a GitHub Packages OCI layer.
- Nightlies are **unsigned** or signed with a nightly-only key. Stable releases are signed with the production key (future).

### RC Repository
- RCs are published as GitHub **Pre-Releases**.
- The release body clearly marks: "This is a release candidate. Do not use in production."
- Package filenames do **not** include `-rc` in the version segment if the `VERSION` file itself is not bumped to an RC string. Instead, the Git tag carries the RC identifier.
- If we want package filenames to include `-rc`, the bump script and `build-packages.sh` must be extended to accept a `CHV_PRERELEASE` env var.

### Stable Repository
- Stable releases are published as GitHub **Releases** (not pre-release).
- Assets: tarball, `.deb` packages (3), `.rpm` packages (3), `sha256sums.txt`, `sbom.spdx.json`.
- Build provenance attestations are generated via `actions/attest-build-provenance`.
- Future: signed `.deb` (via `debsigs` or `dpkg-sig`) and signed `.rpm` (via `rpmsign`), plus an apt/dnf repository.

---

## 8. Safety Policy for Package Scripts

Package scripts (`postinstall`, `preremove`) run as `root` during `dpkg`/`rpm` transactions. The following rules are **mandatory**.

### DO

1. **Idempotent user creation**
   - Check `getent group chv` and `getent passwd chv` before `groupadd` / `useradd`.
   - Use `useradd -r -g chv -d /var/lib/chv -s /usr/sbin/nologin`.

2. **Create directories with safe permissions**
   - `mkdir -p` before `chown`.
   - `chown chv:chv` with `|| true` so the script does not fail if the filesystem is read-only.

3. **Reload systemd only when available**
   - Guard with `if command -v systemctl >/dev/null 2>&1; then systemctl daemon-reload; fi`.

4. **Detect upgrade vs. remove in preremove**
   - Debian: `$1` can be `remove`, `purge`, `upgrade`, `failed-upgrade`, `abort-install`, `abort-upgrade`, `disappear`.
   - RPM: `$1` can be `0` (uninstall) or `1` (upgrade).
   - **Only stop services on actual removal**, not on upgrade.

5. **Use `|| true` for non-critical cleanup**
   - Service stop failures must not block package removal.

6. **Preserve data on removal**
   - Do **not** delete `/var/lib/chv/`, `/var/log/chv/`, or `/etc/chv/` in `preremove`.
   - Document manual cleanup steps in `docs/PACKAGING.md`.

### DO NOT

1. **Do not `rm -rf` anything outside the package's manifest**
   - Never delete user data, VM images, or bridges.

2. **Do not `systemctl start` services in `postinstall`**
   - `daemon-reload` is acceptable; auto-starting is the admin's decision.

3. **Do not fail the transaction on optional actions**
   - If `usermod -aG kvm chv` fails, log and continue.

4. **Do not assume `systemctl` is present**
   - Container builds or minimal chroots may not have it.

5. **Do not modify config files marked `config|noreplace`**
   - Let `dpkg`/`rpm` handle config file preservation.

6. **Do not use `set -e` without careful exit-code handling**
   - Current scripts use `set -e` combined with `|| true` on optional lines, which is acceptable.

### Current Script Audit
The existing scripts in `packaging/scripts/` follow these rules correctly:
- `postinstall` scripts create user/group, ensure directories, reload systemd.
- `preremove` scripts detect action type, stop services only on remove, leave data intact.

---

## 9. Release Readiness Checklist

Use this checklist before declaring a release (RC or stable) ready.

### Pre-Build

- [ ] `VERSION` file is set to the intended release version.
- [ ] `Cargo.toml` workspace version matches `VERSION`.
- [ ] `CHANGELOG.md` has an entry for this version (not `[Unreleased]` if stable).
- [ ] All `docs/` version references are consistent (bump script handles most).
- [ ] No open P1/P2 bugs targeted for this release.
- [ ] `main` branch CI is green.

### Build & Packaging

- [ ] `cargo build --workspace --release` succeeds locally.
- [ ] `make release` produces a valid tarball.
- [ ] `scripts/build-packages.sh` produces 3 `.deb` and 3 `.rpm` files.
- [ ] `scripts/smoke-packages.sh` passes.
- [ ] All five binaries are present in `target/release/`.
- [ ] `ui/build/index.html` exists.

### Functional Validation

- [ ] `cargo test --workspace` passes.
- [ ] UI e2e tests pass (`cd ui && npm run test:e2e`).
- [ ] `chvctl version` reports the correct version.
- [ ] Clean install test passes (install packages in fresh container, no errors).
- [ ] Upgrade test passes (install previous stable → upgrade → services restart OK).
- [ ] Remove test passes (remove packages → no crash → `/var/lib/chv/` still exists).

### Security & Compliance

- [ ] SBOM generated and attached.
- [ ] SHA256 checksums generated and verified.
- [ ] Build provenance attestation generated.
- [ ] No secrets or credentials in tarball/package contents.
- [ ] systemd units do not contain hardcoded passwords.

### Documentation

- [ ] `docs/PACKAGING.md` reflects the current package split and install steps.
- [ ] Release notes drafted (GitHub Release body or `CHANGELOG.md` excerpt).
- [ ] Known issues section included if applicable.

### Final Sign-Off

- [ ] RC tested by at least one external user or QA environment (for stable).
- [ ] Tag pushed: `v<VERSION>` (or `v<VERSION>-rc.N`).
- [ ] GitHub Release created with all assets attached.
- [ ] Announcement sent (Slack/Discord/email) if stable.

---

## 10. Gaps and Recommendations

### Missing Files / Workflows to Create

| # | Item | Priority | File to Create / Modify |
|---|------|----------|------------------------|
| 1 | **Nightly CI workflow** | Medium | `.github/workflows/nightly.yml` — scheduled full build + package, artifact upload only, no Release. |
| 2 | **Clean install test script** | High | `scripts/test-install-clean.sh` — spin up `ubuntu:24.04` container, install `.deb`, assert units parse, `systemctl daemon-reload` works (or `systemd-analyze verify`), assert binaries in `PATH`. |
| 3 | **Upgrade test script** | High | `scripts/test-upgrade.sh` — download previous release packages, install, upgrade to current, assert no dpkg errors, assert services can be restarted. |
| 4 | **Remove test script** | High | `scripts/test-remove.sh` — install, remove, assert no errors, assert `/var/lib/chv/` preserved, assert systemd units removed. |
| 5 | **Package install CI job** | High | Add job to `.github/workflows/ci.yml` or new `.github/workflows/test-install.yml` that runs #2–#4 in containers on `main` and tags. |
| 6 | **RC tag support in release.yml** | Medium | Extend `.github/workflows/release.yml` to detect `v*-rc.*` tags and create a **pre-release** instead of a full release. |
| 7 | **Package signing** | Medium | Add GPG signing step to `.github/workflows/release.yml` for `.deb` (`debsigs`/`dpkg-sig`) and `.rpm` (`rpmsign`). Requires secrets management. |
| 8 | **apt / dnf repository** | Low | Build a repository structure (e.g., `reprepro` for apt, `createrepo` for dnf) and publish to S3 or GitHub Pages. |
| 9 | **Self-hosted KVM runner** | Low | Provision a bare-metal or nested-KVM GitHub Actions runner for `chv-agent` integration tests. Workflow: `.github/workflows/kvm-integration.yml`. |
| 10 | **Version validation CI step** | Low | Add a CI step that fails if `VERSION`, `Cargo.toml`, and `ui/package.json` are out of sync. |
| 11 | **Artifact retention policy** | Low | Configure `actions/upload-artifact` with explicit `retention-days` to match Section 7. |
| 12 | **Release notes template** | Low | `.github/release.yml` or a script to auto-generate release notes from `CHANGELOG.md`. |

### Process Gaps

1. **No `chv doctor` or `chv init` commands**: These would simplify install smoke tests. Consider adding `chvctl doctor` to verify KVM, bridges, and certificates exist.
2. **Install script is Debian-only**: `scripts/install.sh` uses `apt-get` and `dpkg`. An RPM-aware path (detecting `yum`/`dnf`/`rpm`) would broaden test coverage.
3. **No rollback procedure documented**: If a stable release breaks, there is no documented downgrade path for packages. Add downgrade steps to `docs/PACKAGING.md`.
4. **No changelog enforcement**: There is no CI gate ensuring `CHANGELOG.md` is updated on release-bound PRs.

### Immediate Next Steps (Recommended Order)

1. Create `scripts/test-install-clean.sh` and wire it into a new CI job on `main`.
2. Create `scripts/test-upgrade.sh` and `scripts/test-remove.sh`.
3. Extend `.github/workflows/release.yml` to handle RC tags as pre-releases.
4. Create `.github/workflows/nightly.yml` for nightly builds.
5. Add GPG signing to the release workflow once a signing key is provisioned.
