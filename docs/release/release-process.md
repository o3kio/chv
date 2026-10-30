# Release Process

This document describes how CHV release candidates and stable releases are built, tested, and published.

## Release types

| Type | Tag format | Example | GitHub Release | Package channel |
|------|------------|---------|----------------|-----------------|
| **Stable** | `vX.Y.Z` | `v<version>` | Full release | `stable` |
| **RC** | `vX.Y.Z-rc.N` | `v<version>-rc.1` | Pre-release | `rc` |

## Preparing a release

### 1. Version bump

Update the project version before tagging:

```bash
# Bump the patch version
make bump-version BUMP_TYPE=patch

# Or bump the minor version
make bump-version BUMP_TYPE=minor
```

This updates:
- `VERSION` file
- All `Cargo.toml` files
- `ui/package.json` and `ui/package-lock.json`
- The UI sidebar version label
- Version references in `README.md`, `docs/DEPLOYMENT.md`, and `scripts/install.sh`
- `Cargo.lock`

### 2. Update CHANGELOG.md

Add a section for the new version:

```markdown
## [<version>] - YYYY-MM-DD

### Added
- ...

### Fixed
- ...
```

Stable releases **require** a changelog entry. The CI workflow fails if it is missing.

### 3. Schema-bump checklist (architecture model)

If this release adds, removes, or renames a required field on `chv_architecture_validate::model::CHVArchitecture`, update the six embedded starter topology YAMLs in `crates/chv-controlplane-seed/fixtures/` atomically. The seeder fails closed on a fixture that does not parse. A bumped schema with stale fixtures takes the control plane down on first boot.

Run the per-fixture round-trip suite as a release-cut gate:

```bash
cargo test -p chv-controlplane-seed --test fixtures_round_trip
cargo test -p chv-controlplane-seed --test seed_idempotency
```

If `fixtures_round_trip` fails, update each affected starter YAML to satisfy the new schema, re-run, and only then proceed to the commit-and-push step. The starter YAMLs are checked-in source — never hand-edit a generated row in `architecture_topologies` to fix a bumped schema.

### 4. Commit and push

```bash
git add VERSION Cargo.toml ui/package.json CHANGELOG.md
git commit -m "Release v<version>"
git push origin main
```

## Tagging

Releases are created exclusively from git tags. Pushing a tag triggers `.github/workflows/release.yml`.

### Cut a release candidate

```bash
# Ensure main is in the desired state
git checkout main
git pull origin main

# Create and push the RC tag
git tag v<version>-rc.1
git push origin v<version>-rc.1
```

The workflow performs the following steps:
1. Build release binaries and UI
2. Build `.deb` and `.rpm` packages
3. Run package smoke and lifecycle tests
4. Sign checksums (if signing secrets are configured)
5. Generate SBOM and build provenance attestation
6. Create a GitHub **pre-release**
7. Publish to the RC package repository (if configured)

### Test the RC

Download the RC artifacts and validate:

```bash
# Download and verify
gh release download v<version>-rc.1
sha256sum -c SHA256SUMS

# Install on a test host
sudo dpkg -i chv-controlplane_<version>~rc.1_amd64.deb ...

# Run smoke tests
make package-smoke-deb
```

If issues are found, fix them on `main`, bump the RC number, and retag:

```bash
git tag v<version>-rc.2
git push origin v<version>-rc.2
```

### Promote RC to stable

Once the RC is validated, promote it to stable by tagging the same commit without the RC suffix:

```bash
git checkout main
git pull origin main

# Tag the exact commit that passed RC validation
git tag v<version>
git push origin v<version>
```

> **Do not add new commits between the final RC and the stable tag.** The stable release should be byte-for-byte identical to the tested RC, minus the version string.

The workflow performs the following steps:
1. Validate the changelog entry exists
2. Build release binaries and UI
3. Build `.deb` and `.rpm` packages
4. Run package smoke and lifecycle tests
5. Sign checksums (if signing secrets are configured)
6. Generate SBOM and build provenance attestation
7. Create a GitHub **release**
8. Publish to the stable package repository (if configured)

## Verifying a release

After the release workflow completes, verify the artifacts before announcing:

```bash
# Download the release artifacts
gh release download v<version>

# Verify checksums
sha256sum -c SHA256SUMS

# Verify GitHub attestation
gh attestation verify chv-<version>-linux-amd64.tar.gz --repo o3kio/chv

# Inspect SBOM
jq '.packages | length' sbom.spdx.json

# Verify GPG signature (if configured)
gpg --verify SHA256SUMS.sig SHA256SUMS
```

See [Verify Release Artifacts](verify-release-artifacts.md) for full instructions.

## Rollback and deprecation

### If a stable release is broken

1. **Do not delete the release.** Deleting a release breaks links and confuses users who already downloaded it.
2. **Edit the release notes** to add a prominent warning: `⚠️ This release has a critical issue. Use a newer stable release instead.`
3. **Cut a hotfix release** with the fix: bump the patch version, tag, and publish.
4. **Update CHANGELOG.md** to document the issue and the fix.

### Deprecating old releases

GitHub releases are retained indefinitely. To deprecate an old release:

1. Edit the release notes to indicate deprecation.
2. Do not remove artifacts (breaking existing installs).
3. Document the upgrade path in the release notes.

### Downgrade is unsupported

Downgrading from stable to an older stable, or from stable to RC/nightly, is not tested and not recommended. If you must revert:

1. Back up `/var/lib/chv` and `/etc/chv`.
2. Remove the new packages.
3. Install the previous version packages.
4. Restore configs if needed.
5. Restart services.

See [Package Contract](package-contract.md) for the manual rollback procedure.

## Version mapping

### Tag → package version

| Tag | Debian package | RPM package |
|-----|----------------|-------------|
| `vX.Y.Z` | `X.Y.Z` | `X.Y.Z` |
| `vX.Y.Z-rc.1` | `X.Y.Z~rc.1` | `X.Y.Z~rc.1` |

Both formats use `~` for pre-release sorting (`X.Y.Z~rc.1 < X.Y.Z` in
`dpkg` and in RPM's `rpmvercmp` alike). `version.sh --rpm` emits the same
`~` strings as `--deb` for every channel; its former `^` post-release
output for nightly/PR, which would have sorted above stable in RPM, was
removed (#432).

### Channel precedence

Package managers treat these versions in ascending order (the CI stamps the Debian-derived `~` string on both formats, so the same ordering ships for `.deb` and `.rpm`):

```text
<version>~nightly.20260510.g0872c4a7   (nightly)
<version>~rc.1                         (RC)
<version>                              (stable)
```

Upgrading from nightly → RC → stable is always a forward upgrade on both formats as shipped. See [Nightly Packages](nightly-packages.md) for the version-format details.

## Workflow details

`.github/workflows/release.yml` runs three jobs:

### 1. Build
- Compiles Rust workspace in release mode
- Builds Web UI
- Assembles release tarball (`chv-<VERSION>-linux-amd64.tar.gz`)
- Uploads binaries, UI, and tarball as artifacts

### 2. Package
- Downloads artifacts from the build job
- Installs nfpm and builds `.deb`/`.rpm` packages
- Runs `check-package-files.sh` and `check-safety.sh`
- Runs container smoke tests (`smoke-deb.sh`, `smoke-rpm.sh`)
- Generates `SHA256SUMS`
- Uploads packages as artifacts

### 3. Release
- Downloads tarball and packages
- Generates SBOM (`anchore/sbom-action`)
- Generates build provenance attestation (`actions/attest-build-provenance`)
- Extracts release notes from `CHANGELOG.md`
- Creates GitHub Release (or pre-release for RC)
- Attaches tarball, packages, checksums, and SBOM

### 4. Publish to package repository (optional)
- Generates apt/yum repository metadata
- Uploads to the configured repository target (S3, rsync, etc.)
- Runs in dry-run mode if no upload credentials are configured

## Dry-run mode

You can test the full release pipeline without publishing:

```bash
# Via GitHub UI: Actions → Release → Run workflow
# Leave the version field empty for a dry-run build.
```

In dry-run mode:
- Binaries, packages, and tarball are built
- Smoke tests run
- No GitHub Release is created
- No repository publishing occurs

## Changelog requirement

Stable releases **require** a changelog entry. The workflow looks for a section like:

```markdown
## [<version>] - YYYY-MM-DD

### Added
- ...
```

If the section is missing, the workflow fails before building.

RC releases do not strictly require a changelog entry (they are pre-releases), but having one is recommended.

## Required environments

The workflow uses GitHub Environments for approval gating:

| Release type | Environment | Purpose |
|--------------|-------------|---------|
| Stable | `production` | Gate stable releases behind manual approval |
| RC | `rc` | Gate RC releases behind manual approval (optional) |
| Package repo (stable) | `production-repo` | Gate repository publishing |
| Package repo (RC) | `rc-repo` | Gate RC repository publishing |

Configure these environments in **Settings → Environments** with required reviewers.

## Required secrets

### GitHub Release (always required)
- `GITHUB_TOKEN` — provided automatically

### Package repository publishing (optional)
| Secret | Purpose |
|--------|---------|
| `CHV_REPO_S3_BUCKET` | S3 bucket for repository hosting |
| `CHV_REPO_AWS_ACCESS_KEY_ID` | AWS credential |
| `CHV_REPO_AWS_SECRET_ACCESS_KEY` | AWS credential |
| `CHV_REPO_RSYNC_TARGET` | rsync destination |
| `CHV_REPO_GPG_KEY` | ASCII-armored GPG signing key |
| `CHV_REPO_GPG_PASSPHRASE` | GPG key passphrase |

If repository secrets are not configured, the `publish-repo` job runs in dry-run mode.

## Local release commands

```bash
# Full local release check (format, lint, test, build, version)
make check

# Build release binaries and UI
make build-release

# Build packages
make package-local

# Run package smoke tests
make package-smoke-deb
make package-smoke-rpm

# Bump version (patch, minor, major)
make bump-version BUMP_TYPE=patch
```

## Artifacts attached to releases

| Artifact | Description |
|----------|-------------|
| `chv-<VERSION>-linux-amd64.tar.gz` | Release tarball with binaries, UI, configs, install script |
| `chv-controlplane_*.deb` / `*.rpm` | Control-plane package |
| `chv-node_*.deb` / `*.rpm` | Node services package |
| `chvctl_*.deb` / `*.rpm` | CLI package |
| `SHA256SUMS` | Checksums for all packages |
| `sbom.spdx.json` | Software Bill of Materials |

## Safety rules

- **Only tags create releases.** Branch pushes never trigger the release workflow.
- **Changelog required for stable.** Missing changelog = failed build.
- **Smoke tests gate publishing.** If container smoke tests fail, no release is created.
- **Environments gate publishing.** Even if tests pass, a human must approve the release job.
- **No PR releases.** The workflow does not trigger on pull requests.

## References

- [Versioning Policy](versioning-policy.md)
- [Package Contract](package-contract.md)
- [Nightly Packages](nightly-packages.md)
- [Local Release Commands](local-release-commands.md)
