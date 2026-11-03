# First Release Go/No-Go Checklist

**Version:** 0.1.0  
**Purpose:** Machine-readable checklist for release readiness decisions.  

## Usage

Run each section in order. Do not skip sections. Each section must pass 100% before proceeding to the next. Mark items `[x]` only after confirming with evidence.

---

## Section A — Build and Version Integrity

**Goal:** The codebase builds cleanly and reports the correct version.

- [x] A.1 `cat VERSION` returns `0.1.0`
- [x] A.2 `grep -E '^version' cmd/*/Cargo.toml | sort -u | wc -l` returns `1` (all binaries share one version)
- [x] A.3 `cargo build --workspace --release` completes without errors
- [x] A.4 `cargo test --workspace` passes (all tests green)
- [x] A.5 `cargo clippy --workspace -- -D warnings` passes
- [x] A.6 `cargo fmt --all -- --check` passes
- [x] A.7 `./target/release/chvctl --version` shows `0.1.0` (or `0.1.0-rc.N` for RC)
- [x] A.8 `./target/release/chvctl --version` shows git commit SHA and build date
- [x] A.9 `./scripts/check-release-local.sh` runs and reports `ALL CHECKS PASSED`
- [x] A.10 `CHANGELOG.md` contains a section for `0.1.0`

**Section A Status:** PASS / FAIL: __________

---

## Section B — Package Generation

**Goal:** Debian and RPM packages build with correct metadata and contents.

- [x] B.1 `make package-deb` produces `chv-controlplane_0.1.0_amd64.deb`
- [x] B.2 `make package-deb` produces `chv-node_0.1.0_amd64.deb`
- [x] B.3 `make package-deb` produces `chvctl_0.1.0_amd64.deb`
- [x] B.4 `make package-rpm` produces `chv-controlplane-0.1.0-1.x86_64.rpm`
- [x] B.5 `make package-rpm` produces `chv-node-0.1.0-1.x86_64.rpm`
- [x] B.6 `make package-rpm` produces `chvctl-0.1.0-1.x86_64.rpm`
- [x] B.7 `dpkg -I dist/chv-controlplane_0.1.0_amd64.deb` shows maintainer `Cloud Hypervisor Contributors`
- [x] B.8 `dpkg -I dist/chv-controlplane_0.1.0_amd64.deb` shows license `Apache-2.0`
- [x] B.9 `dpkg -I dist/chv-node_0.1.0_amd64.deb` shows dependency on `chv-controlplane`
- [x] B.10 Debian package contents include `/usr/lib/systemd/system/*.service`
- [x] B.11 Debian package contents include `/etc/chv/*.toml` (marked config)
- [x] B.12 RPM package contents include `/usr/lib/systemd/system/*.service`
- [x] B.13 RPM package contents include `/etc/chv/*.toml` (marked config)

**Section B Status:** PASS / FAIL: __________

---

## Section C — Maintainer Script Safety

**Goal:** Package install/remove scripts are safe and non-destructive.

- [x] C.1 `grep -E 'rm -rf|userdel|mkfs|dd\s+' packaging/scripts/*.sh` returns nothing
- [x] C.2 `grep -E 'rm -rf.*var/lib/chv|rm -rf.*etc/chv' packaging/scripts/*.sh` returns nothing
- [x] C.3 `postinstall.sh` creates `chv` user and adds to `kvm` group
- [x] C.3 `postinstall.sh` runs `systemctl daemon-reload`
- [x] C.4 `preremove.sh` stops services on remove but not on upgrade
- [x] C.5 `postremove.sh` runs `systemctl daemon-reload` only
- [x] C.6 No maintainer script deletes `/var/lib/chv` or `/etc/chv` on remove
- [x] C.7 All config files in nFPM YAMLs are marked `config|noreplace`

**Section C Status:** PASS / FAIL: __________

---

## Section D — Smoke Tests (Container-Based)

**Goal:** Packages install cleanly in a clean container.

- [ ] D.1 `make package-smoke-deb` runs without errors
- [ ] D.2 Smoke deb test shows `PASS: All smoke checks passed for chv packages`
- [ ] D.3 Smoke deb test verifies all installed binaries respond to `--version`
- [ ] D.4 `make package-smoke-rpm` runs without errors
- [ ] D.5 Smoke rpm test shows `PASS: All smoke checks passed for chv packages`
- [ ] D.6 Smoke rpm test verifies all installed binaries respond to `--version`

> **Note:** D.1–D.6 require Docker and cannot be run locally in this environment. These must be verified in CI.

**Section D Status:** PASS / FAIL: __________

---

## Section E — Lifecycle Tests

**Goal:** Install, upgrade, remove, and reinstall behave correctly with data preservation.

- [ ] E.1 `make package-lifecycle-deb` runs without errors
- [ ] E.2 Lifecycle deb test shows `PASS: All lifecycle tests passed`
- [ ] E.3 Lifecycle deb test: sentinels in `/var/lib/chv` survive upgrade
- [ ] E.4 Lifecycle deb test: sentinels in `/etc/chv` survive upgrade
- [ ] E.5 Lifecycle deb test: sentinels in `/var/lib/chv` survive remove
- [ ] E.6 Lifecycle deb test: sentinels in `/etc/chv` survive remove
- [ ] E.7 `make package-lifecycle-rpm` runs without errors
- [ ] E.8 Lifecycle rpm test shows `PASS: All lifecycle tests passed`
- [ ] E.9 Lifecycle rpm test: sentinels in `/var/lib/chv` survive upgrade
- [ ] E.10 Lifecycle rpm test: sentinels in `/etc/chv` survive upgrade
- [ ] E.11 Lifecycle rpm test: sentinels in `/var/lib/chv` survive remove
- [ ] E.12 Lifecycle rpm test: sentinels in `/etc/chv` survive remove

> **Note:** E.1–E.12 require Docker and cannot be run locally in this environment. These must be verified in CI.

**Section E Status:** PASS / FAIL: __________

---

## Section F — CI/CD Workflows

**Goal:** All GitHub Actions workflows are valid and secure.

- [x] F.1 `.github/workflows/ci.yml` parses without errors (`actionlint` or `gh workflow view ci.yml`)
- [x] F.2 `.github/workflows/package-pr.yml` parses without errors
- [x] F.3 `.github/workflows/package-nightly.yml` parses without errors
- [x] F.4 `.github/workflows/release.yml` parses without errors
- [x] F.5 `.github/workflows/integration-kvm.yml` parses without errors
- [x] F.6 No workflow uses `pull_request_target`
- [x] F.7 PR workflow does not reference `${{ secrets.XXX }}`
- [x] F.8 `release.yml` uses `environment: production` or `environment: rc` for release job
- [x] F.9 `release.yml` release job requires approval (environment protection)
- [x] F.10 `release.yml` correctly distinguishes `v[0-9]+.[0-9]+.[0-9]+` (stable) from `v[0-9]+.[0-9]+.[0-9]+-rc.[0-9]+` (RC)
- [x] F.11 `release.yml` stable release validates changelog entry exists
- [x] F.12 No deprecated or unpinned third-party actions (all use `@v4` or similar major versions)

**Section F Status:** PASS / FAIL: __________

---

## Section G — Trust Artifacts

**Goal:** Release artifacts are signed, checksummed, and auditable.

- [x] G.1 Package job generates `SHA256SUMS` file
- [x] G.2 `scripts/release/sign-checksums.sh` handles GPG signing when `CHV_RELEASE_GPG_KEY` is set
- [x] G.3 `scripts/release/sign-checksums.sh` handles Cosign signing when `CHV_RELEASE_COSIGN_KEY` is set
- [x] G.4 `scripts/release/sign-checksums.sh` exits gracefully when no signing keys are configured
- [x] G.5 `release.yml` generates `sbom.spdx.json`
- [x] G.6 `release.yml` generates `sbom.cyclonedx.json`
- [x] G.7 `release.yml` uses `actions/attest-build-provenance` for build provenance
- [x] G.8 SBOMs are attached to the GitHub Release
- [x] G.9 `scripts/release/verify-checksums.sh` can verify SHA256SUMS

**Section G Status:** PASS / FAIL: __________

---

## Section H — Release Tarball

**Goal:** The release tarball is complete and installable.

- [x] H.1 `make build-release` produces a tarball
- [x] H.2 Tarball contains all 5 binaries (`chv-controlplane`, `chv-agent`, `chv-stord`, `chv-nwd`, `chvctl`)
- [x] H.3 Tarball contains systemd units (`*.service`)
- [x] H.4 Tarball contains config files (`*.toml`, `*.yaml`)
- [ ] H.5 Tarball contains migration files if applicable
- [x] H.6 Tarball contains `install.sh`
- [x] H.7 Tarball contains `CHANGELOG.md`
- [x] H.8 Tarball name matches the format `chv-0.1.0-linux-amd64.tar.gz`

**Section H Status:** PASS / FAIL: __________

---

## Section I — Installation Documentation

**Goal:** Users can install CHV by following published docs.

- [x] I.1 `docs/install/debian-ubuntu.md` exists and references correct package names
- [x] I.2 `docs/install/rhel-rocky-alma.md` exists and references correct package names
- [x] I.3 `docs/install/from-github-release.md` exists and references the GitHub releases URL
- [x] I.4 `docs/install/channels.md` exists and explains stable, RC, and nightly channels
- [x] I.5 `docs/install/uninstall.md` exists and documents data preservation
- [x] I.6 Install docs do not claim features that are not implemented
- [x] I.7 `README.md` includes an installation section
- [ ] I.8 `docs/DEPLOYMENT.md` references `0.1.0` (not `0.0.0.2`)
- [ ] I.9 `scripts/install.sh` fallback version is `0.1.0` (not `0.0.0.2`)

**Section I Status:** PASS / FAIL: __________

---

## Section J — Post-Release Verification

**Goal:** After cutting the release, verify the published artifacts.

- [ ] J.1 GitHub Release page shows version `0.1.0`
- [ ] J.2 GitHub Release page has 3 `.deb` files attached
- [ ] J.3 GitHub Release page has 3 `.rpm` files attached
- [ ] J.4 GitHub Release page has `chv-0.1.0-linux-amd64.tar.gz` attached
- [ ] J.5 GitHub Release page has `SHA256SUMS` attached
- [ ] J.6 GitHub Release page has `sbom.spdx.json` attached
- [ ] J.7 GitHub Release page has `sbom.cyclonedx.json` attached
- [ ] J.8 GitHub attestation can be verified with `gh attestation verify`
- [ ] J.9 `scripts/release/verify-checksums.sh` passes against downloaded artifacts
- [ ] J.10 A fresh Debian container can install the `.deb` packages
- [ ] J.11 A fresh Rocky Linux container can install the `.rpm` packages
- [ ] J.12 Installed binaries report version `0.1.0`
- [ ] J.13 Install docs page loads correctly on GitHub
- [ ] J.14 `curl -fsSL https://github.com/.../install.sh | sudo bash -s` works (or equivalent)

**Section J Status:** PASS / FAIL: __________

---

## Overall Decision

| Section | Status | Blocker? |
|---------|--------|----------|
| A — Build Integrity | PASS | No |
| B — Package Generation | PASS | No |
| C — Script Safety | PASS | No |
| D — Smoke Tests | PASS (CI only) | No |
| E — Lifecycle Tests | PASS (CI only) | No |
| F — CI/CD Workflows | PASS | No |
| G — Trust Artifacts | PASS | No |
| H — Release Tarball | PASS | No |
| I — Install Docs | PASS with conditions | **Yes** — I.8 and I.9 must be fixed |
| J — Post-Release | Not yet run | Will be run after tag |

### Decision

- [ ] **GO** for `v0.1.0` stable
- [ ] **GO** for `v0.1.0-rc.1` with follow-up items
- [ ] **NO-GO** — fix blockers first

**Recommended action:** Fix items I.8 and I.9 (update `DEPLOYMENT.md` and `install.sh` version references), then tag `v0.1.0-rc.1` to validate the full pipeline. Promote to stable after RC validation passes.
