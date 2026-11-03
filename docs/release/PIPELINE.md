# CHV Release Pipeline — LLM Context Document

**Purpose:** This is the single source of truth for the CHV release engineering pipeline. If you are an LLM agent working on releases, packaging, CI/CD, or versioning, **read this file first** before exploring the repository.

**Last updated:** 2026-11-03
**Version:** 0.1.2  

---

## Pipeline Overview

```
┌─────────────┐     ┌─────────────┐     ┌─────────────┐     ┌─────────────┐     ┌─────────────┐
│   Source    │────▶│    Build    │────▶│   Package   │────▶│    Test     │────▶│   Publish   │
│   (git)     │     │   (Rust+UI) │     │  (nFPM)     │     │(smoke/life) │     │(GitHub+repo)│
└─────────────┘     └─────────────┘     └─────────────┘     └─────────────┘     └─────────────┘
       │                   │                   │                   │                   │
       │                   │                   │                   │                   │
   VERSION file      build.rs injects     .deb / .rpm         Docker containers    GitHub Release
   scripts/version.sh  version metadata    systemd units       install/upgrade      + apt/yum repo
                       into binaries       config files        remove/reinstall     (optional)
                                           maintainer scripts
```

**Channels:** `stable` (vX.Y.Z) → `rc` (vX.Y.Z-rc.N) → `nightly` (rolling from main) → `pr` (branch builds)

---

## Source of Truth

| What | File | Format |
|------|------|--------|
| Semantic version | `VERSION` | Plain text, e.g. `X.Y.Z` |
| Per-crate version | `cmd/*/Cargo.toml` | Must match `VERSION` |
| Rust toolchain | `rust-toolchain.toml` | Exact channel pin, e.g. `1.98.1` |
| Changelog | `CHANGELOG.md` | Keep a Changelog format |
| Git tag | `vX.Y.Z` or `vX.Y.Z-rc.N` | Must match `VERSION` |

**Rule:** All 5 binary crates (`chv-controlplane`, `chv-agent`, `chv-stord`, `chv-nwd`, `chvctl`) share the same version. CI validates this.

### Rust toolchain (#229)

- **Source of truth:** `rust-toolchain.toml` pins the exact compiler channel.
  rustup resolves it automatically for local development; CI, KVM integration,
  nightly, PR-package, and release workflows install it through the in-repo
  composite action `.github/actions/setup-rust`, which reads the same pin and
  never selects a toolchain on its own.
- **Why:** a floating `stable` let CI behavior change without any repository
  commit (new lints in generated tonic/prost output failing unrelated PRs,
  silently changed compiler behavior in release artifacts). The pin makes the
  compiler input of every artifact reviewable and reproducible.
- **Generated-code policy:** handwritten Rust stays `-D warnings`; the
  generated crates under `gen/rust/*/src/lib.rs` carry only the narrow
  crate-root `#![allow(clippy::result_large_err)]`. Generated files are
  regenerated (`cargo build --workspace`), never hand-patched. A new compiler
  lint can only be addressed by (in order of preference) a generator update, a
  new narrow crate-root allow with review rationale, or a toolchain bump —
  never a global `allow(warnings)`.

**Toolchain-bump procedure:**

1. Open a PR changing **only** the `channel` value in `rust-toolchain.toml`
   (plus, if needed, a narrow generated-code lint adjustment).
2. CI (fmt/check/clippy/test), Security, and package smoke must pass on that
   PR — they run the proposed toolchain.
3. Merge as a reviewable tooling change. Never let a workflow select a
   different toolchain than the pin declares.
4. Cadence: bump deliberately (e.g. with each minor release line), not
   automatically. Coordinate MSRV-sensitive migrations (e.g. tonic major
   bumps, which change generated output) with a toolchain bump so the two
   changes do not fight each other. (The tonic 0.12 → 0.14 upgrade, #235,
   needed no toolchain change: the pinned compiler already exceeded
   tonic 0.14's MSRV.)


---

## File Map (Who Calls Whom)

### Version Derivation
```
VERSION ──▶ scripts/version.sh ──▶ deb: <version>~rc.1
                              ──▶ rpm: <version>~rc.1
                              ──▶ nightly: <version>~nightly.<date>.g<sha>
```
- Called by: `scripts/build-packages.sh`, CI workflows, Makefile
- Environment override: `CHV_PKG_PRERELEASE` (set by CI for RC builds)

### Build
```
Makefile:build-release ──▶ cargo build --workspace --release
                        ──▶ cd ui && npm ci && npm run build
scripts/build-release.sh ──▶ (builds the above, then)
                            scripts/release/assemble-tarball.sh <version>
                            ──▶ dist/chv-<version>-linux-amd64.tar.gz (+ .sha256)
```
- `scripts/release/assemble-tarball.sh` is the single source of truth for the
  tarball layout/naming; `release.yml` calls it with the tag-derived version
  so the published asset lands exactly where `scripts/install.sh` looks:
  `releases/download/v<version>/chv-<version>-linux-amd64.tar.gz` (#440)
- Version metadata injected via `cmd/*/build.rs` (CHV_VERSION, CHV_GIT_SHA, CHV_BUILD_DATE, CHV_RELEASE_CHANNEL)
- Binaries respond to `--version` with: `chvctl <version> (commit <sha>, build <date>, channel stable)`

### Package Generation
```
scripts/build-packages.sh ──▶ nfpm package -f config.yaml -p deb/rpm
   │
   ├── packaging/nfpm/chv-controlplane.yaml  → chv-controlplane_<version>_amd64.deb
   ├── packaging/nfpm/chv-node.yaml          → chv-node_<version>_amd64.deb
   └── packaging/nfpm/chvctl.yaml            → chvctl_<version>_amd64.deb
   └── packaging/scripts/postinstall.sh      → runs on package install
   └── packaging/scripts/preremove.sh        → runs before package removal
   └── packaging/scripts/postremove.sh       → runs after package removal
```
- **Tool:** nFPM v2.41.1 (pinned in CI)
- **Formats:** `.deb` (Debian/Ubuntu) and `.rpm` (RHEL/Rocky/Alma/Fedora)
- **Package `chv-node` depends on `chv-controlplane`** (plus `wireguard-tools`: hard dependency in `.deb`, `recommends` in `.rpm`)
- Config files marked `config|noreplace` (survive upgrades)
- Services installed but NOT auto-started

### Testing
```
scripts/package/smoke-deb.sh     → installs .deb in clean Debian container, checks binaries
scripts/package/smoke-rpm.sh     → installs .rpm in clean Rocky container, checks binaries
scripts/package/smoke-webui.sh   → builds the converged WebUI container image from the .debs
                                   (proxy-only nginx edge + [webui] enabled + packaged UI tree)
                                   and smoke-tests the single-listener shape end to end (#549)
scripts/package/lifecycle-deb.sh → install → upgrade → remove → reinstall with sentinel files
scripts/package/lifecycle-rpm.sh → same for RPM
```
- Sentinel files in `/var/lib/chv/` and `/etc/chv/` prove data survives operations
- Lifecycle tests require Docker

### CI/CD Workflows

| Workflow | Trigger | What it does | Runner |
|----------|---------|--------------|--------|
| `ci.yml` | push/PR to `main`; `workflow_call` | fmt, check, clippy `--all-targets`, workspace tests via nextest + doctests, UI build/check, Vitest unit tests, mock-BFF Playwright e2e; when called by `release.yml` it receives `skip_optional_checks: true`, so the PR-only coverage floor (≥76%, main baseline 78.97%) and real-BFF Playwright flow are skipped | `ubuntu-latest` |
| `package-pr.yml` | PR to `main`, push to other branches | build, package, smoke deb/rpm/webui | `ubuntu-22.04` (glibc 2.35 pin — oldest smoke target is debian:12/glibc 2.36) |
| `package-nightly.yml` | push to `main`, dispatch | build, package, smoke, lifecycle, publish pre-release | `ubuntu-22.04` (build job; same glibc pin) |
| `security.yml` | push/PR to `main`, daily schedule | `cargo audit`, `cargo deny` (advisories/bans/licenses/sources) | `ubuntu-22.04` |
| `release.yml` | tag `v*`, dispatch | quality gate, supply-chain preflight, build, package, sign, GitHub Release, optional repo publish | quality/security `ubuntu-latest`/`ubuntu-22.04`; build `ubuntu-22.04` (glibc pin); package/sign/release jobs `ubuntu-latest` (binaries only run in containers) |
| `integration-kvm.yml` | dispatch, PR label, push `main` | host diagnostics, KVM smoke test, package install | self-hosted `chv-kvm` |
| `nightly-kvm-qualification.yml` | daily 02:00 UTC (m4.7 chaos), Monday 03:00 UTC (m4.3–m4.8), dispatch | real-KVM qualification scenarios; per-scenario logs + evidence artifacts | self-hosted `chv-kvm` |

The `UI unit tests (Vitest)` job runs `npm ci` + `npm run test` in `ui/`.
Making that check *required* for merges is a GitHub ruleset/branch-protection
setting, not repository configuration.

`Rust coverage (ratchet, lines >= 76%)` runs `cargo llvm-cov nextest ...
--profile coverage --fail-under-lines 76 --no-fail-fast` on direct PR/push
runs, publishes the lcov artifact `rust-coverage-lcov` plus a job-summary
table, and fails the job when line coverage drops below 76%. `release.yml`
passes `skip_optional_checks: true`, so the release `workflow_call` skips this
lane. The dedicated `coverage` nextest profile carries `retries = 2` and
`fail-fast = false`; the default profile (used by `Rust checks`) keeps
`retries = 0` so the merge gate does not absorb intermittent failures.
`--no-fail-fast` preserves the full failure blast radius.

The reproducible `main` baseline is **78.97% lines**, measured from the lcov
artifact of run
[41669901088](https://github.com/o3kio/chv/actions/runs/41669901088/job/126035158484).
This is a **floor, not a trend ratchet**: there is no per-PR delta comparison,
so slow drift between baseline refreshes is invisible. Refresh the number from
a `main` artifact and raise it deliberately. A per-PR delta check is a
follow-up (it needs a stable main-baseline service and is intentionally not
added here).

The `Real-BFF E2E` job uses the same `skip_optional_checks` gate, so
`release.yml` skips it while direct PR/push runs execute it. Adding either
check to the `protect-main` ruleset's required checks is an **admin step** —
see [../governance/BRANCH_PROTECTION.md](../governance/BRANCH_PROTECTION.md).

**Workflow dependencies:**
```
ci.yml ──▶ (gates PRs; also called by release.yml via workflow_call)
package-pr.yml ──▶ produces artifacts (7 day retention)
release.yml:
  quality (calls ci.yml) ─┐
  security-audit ─────────┼──▶ build ──▶ package ──▶ sign ──▶ release ──▶ publish-repo
  security-deny ──────────┘
```

**release.yml job conditions (authoritative graph):**

| Job | `needs` | `if` |
|-----|---------|------|
| `quality` | — (reusable `ci.yml`) | always on trigger |
| `security-audit` | — | always on trigger |
| `security-deny` | — | always on trigger |
| `build` | `quality`, `security-audit`, `security-deny` | — |
| `package` | `build` | — |
| `sign` | `build`, `package` | `needs.build.outputs.should_release == 'true'` |
| `release` | `quality`, `security-audit`, `security-deny`, `build`, `package`, `sign` | `needs.build.outputs.should_release == 'true'` |
| `publish-repo` | `build`, `package`, `sign`, `release` | `should_release == 'true'` **and** (`refs/tags/v*` **or** dispatch `version` input) **and** `channel` is `stable` or `rc` |

`sign` and `release` run in the protected `rc` / `production` environments;
`publish-repo` runs in `rc-repo` / `production-repo`. A tag push cannot reach
any publication job unless every quality/security/build/package/sign job
succeeded.

### Signing and Trust Artifacts
```
dist/packages/SHA256SUMS ─────────────▶ release.yml:sign
   ├── SHA256SUMS.sig          (GPG, if CHV_RELEASE_GPG_KEY is configured)
   └── SHA256SUMS.cosign.sig   (Cosign, if CHV_RELEASE_COSIGN_KEY is configured)

dist/chv-<version>-linux-amd64.tar.gz.sha256 ─▶ release.yml:sign
   ├── chv-<version>-linux-amd64.tar.gz.sha256.sig         (GPG)
   └── chv-<version>-linux-amd64.tar.gz.sha256.cosign.sig  (Cosign)
```
- Signing runs in its own `sign` job, **not** in `package`: protected `rc` /
  `production` environment, `contents: read` only, and it downloads only the
  two checksum files (small dedicated artifacts), never binaries or packages.
- The tarball checksum sidecar signature matters because `scripts/install.sh`
  verifies the tarball against `chv-<version>-linux-amd64.tar.gz.sha256`; the
  `.sha256.sig` is the published authenticity asset for that exact file.
- **Graceful default:** with no signing secrets, `sign` logs
  `SIGNING NOT CONFIGURED`, still uploads `SIGNING-STATUS.txt`, and the release
  proceeds unsigned.
- **Fail closed:** set the repository variable `CHV_REQUIRE_SIGNING=1` to make
  missing or failed signing fail the `sign` job (and therefore block
  `release`/`publish-repo`). Enable it for stable releases once keys exist.
- **Operational prerequisite (not configured in this repository):** the
  maintainer must add `CHV_RELEASE_GPG_KEY` (and optional passphrase) or
  `CHV_RELEASE_COSIGN_KEY` (and optional password) as secrets on the protected
  `rc` / `production` environments, then set `CHV_REQUIRE_SIGNING=1` when the
  keys are verified. No keys are committed or invented by the pipeline.

```
SBOM:
   ├── dist/sbom.spdx.json      (anchore/sbom-action)
   └── dist/sbom.cyclonedx.json (anchore/sbom-action)

Provenance:
   └── GitHub artifact attestation (actions/attest-build-provenance),
       including the tarball `.sha256` sidecar
```
- Signature files are published as release assets when present
- Without keys the release is unsigned, never falsely marked as signed

### Publishing
```
GitHub Release (if should_release == 'true'):
   └── Created by softprops/action-gh-release@v2

Package Repository (optional: explicit tag/dispatch input + stable/rc channel,
                    protected rc-repo/production-repo environment):
   └── scripts/publish/publish-repo.sh
       ├── apt repository (dpkg-scanpackages + GPG-signed Release/InRelease)
       └── yum repository (createrepo_c + GPG-signed repomd.xml)
       └── Upload: S3 sync OR rsync
```
- Repo publish is dry-run by default (no credentials = prints what it would do)

---

## Exact Commands (Copy-Paste)

### Local Development

```bash
# Build release binaries and tarball
make build-release

# Build packages (requires nfpm + envsubst)
make package-deb    # or: make package-rpm
make package-local  # both formats

# Run smoke tests (requires Docker)
make package-smoke-deb
make package-smoke-rpm
make package-smoke-webui

# Run lifecycle tests (requires Docker)
make package-lifecycle-deb
make package-lifecycle-rpm

# Verify everything locally
make check-release-local

# Generate and sign checksums
make sign-checksums
```

### Version Management

```bash
# Bump VERSION and all derived version references
# (Cargo.toml files, ui/package.json, sidebar label, README, install script,
#  hosting templates and their root copies)
make bump-version BUMP_TYPE=patch   # or: minor / major

# Derive package versions
./scripts/version.sh --deb        # <version>
./scripts/version.sh --rpm        # <version>
./scripts/version.sh --deb rc 1   # <version>~rc.1
./scripts/version.sh --rpm rc 1   # <version>~rc.1
./scripts/version.sh --deb nightly   # <version>~nightly.<date>.g<sha>
./scripts/version.sh --rpm nightly   # <version>~nightly.<date>.g<sha>
```

`--rpm` and `--deb` produce identical output: both formats use the `~`
pre-release suffix, which sorts below the stable release in `dpkg` and in
RPM's `rpmvercmp` alike (#432).

### Release a New Version

```bash
# 1. Bump version
make bump-version BUMP_TYPE=patch   # or: minor / major

# 2. Update CHANGELOG.md
# 3. Commit and push
# 4. Tag (triggers release.yml)
git tag v<version>
git push origin v<version>

# For RC:
git tag v<version>-rc.1
git push origin v<version>-rc.1
```

---

## Key Decisions and Rationale

| Decision | Why |
|----------|-----|
| **nFPM instead of cargo-deb/cargo-rpm** | Single tool generates both formats; simpler config (YAML); handles maintainer scripts natively |
| **Generic maintainer scripts** | One `postinstall.sh`/`preremove.sh`/`postremove.sh` for all packages instead of per-package scripts. Safer, easier to maintain, no duplication |
| **Services NOT auto-started** | User must configure network/storage first. Prevents broken first-boot states |
| **Config files `config\|noreplace`** | Modified configs survive package upgrades. No silent overwrites |
| **`/var/lib/chv` and `/etc/chv` preserved on remove** | Data safety. Intentional design choice. User can purge manually if needed |
| **No purge script** | Not implemented. Package remove preserves data by design |
| **Self-hosted runner for KVM** | GitHub-hosted runners don't support nested virtualization. KVM tests need bare-metal or dedicated VM |
| **Rolling nightly pre-release** | Single `nightly` tag on GitHub that gets overwritten. Avoids clutter. Users pin to specific nightly versions via exact filename |
| **GPG + Cosign dual signing** | GPG for traditional package manager trust; Cosign for modern Sigstore ecosystem |

---

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `nfpm: command not found` | nFPM not installed | `go install github.com/goreleaser/nfpm/v2/cmd/nfpm@latest` or download binary |
| `envsubst: command not found` | gettext not installed | `sudo apt-get install gettext-base` |
| Smoke tests fail with "Docker not available" | Docker daemon not running | `sudo systemctl start docker` or run in CI |
| `rpm` command not found (on Debian) | Can't inspect RPM metadata locally | Use CI, or install `rpm` package |
| `systemd-analyze verify` fails locally | Binaries not in `/usr/bin/` during build | Expected in build container; verified in smoke tests instead |
| Release workflow fails at "Create GitHub Release" | Missing `contents: write` permission | Check workflow `permissions` block |
| `Sign artifacts` job shows "SIGNING NOT CONFIGURED" | Secrets not set | Add `CHV_RELEASE_GPG_KEY` or `CHV_RELEASE_COSIGN_KEY` to the protected `rc`/`production` environment, then set repo variable `CHV_REQUIRE_SIGNING=1` for stable releases |
| `sign` job fails with "CHV_REQUIRE_SIGNING=1" | Required signing enabled but no keys/valid signature | Configure the signing secrets or unset `CHV_REQUIRE_SIGNING`; do not bypass by publishing unsigned |
| `local: can only be used in a function` | Bash `local` outside function | Fix: remove `local` keyword from top-level code |
| Nightly/RC RPM sorts below the stable release | CI stamps the Debian-derived `~` pre-release string on both formats; `~` sorts before the base version in RPM too | Expected behavior — nightly → RC → stable is a forward upgrade on both formats (`version.sh --rpm` emits the same `~` strings as `--deb`; see versioning-policy §3.1) |

---

## LLM Agent Quick Reference

**If the user asks you to:**
- "Build packages" → run `make package-local` or `make package-deb` / `make package-rpm`
- "Run smoke tests" → run `make package-smoke-deb` and `make package-smoke-rpm` (requires Docker); `make package-smoke-webui` for the converged WebUI container leg
- "Cut a release" → bump VERSION, update CHANGELOG, commit, tag `vX.Y.Z`, push tag
- "Fix the install script" → edit `scripts/install.sh` (not the hosting scripts unless explicitly asked)
- "Update version everywhere" → run `make bump-version BUMP_TYPE=<major|minor|patch>`
- "Review release workflow" → read `.github/workflows/release.yml` and `docs/release/PIPELINE.md`
- "Sign artifacts" → check the protected `sign` job; if `CHV_RELEASE_GPG_KEY` / `CHV_RELEASE_COSIGN_KEY` are absent, explain graceful degradation and that `CHV_REQUIRE_SIGNING=1` makes it fail closed

**Before modifying any packaging or release file:**
1. Read this document (`docs/release/PIPELINE.md`)
2. Read the specific file you're changing
3. Check if the change affects other files in the pipeline (use the File Map above)
4. Run `make check-release-local` after changes

**Files you should NOT modify without explicit user approval:**
- `packaging/scripts/postinstall.sh`, `preremove.sh`, `postremove.sh` (run as root on user machines)
- `.github/workflows/release.yml` environment protection rules
- `scripts/install.sh` when it downloads and executes binaries
