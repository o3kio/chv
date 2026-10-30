# CHV Release Channels

CHV distributes packages through multiple channels. Choose the channel that matches your risk tolerance and use case.

## Channel comparison

| Channel | Stability | Use case | Source |
|---------|-----------|----------|--------|
| **Stable** | Production-ready | Production deployments, long-term evaluation | GitHub Release (tagged) |
| **RC** | Pre-release | Validation before stable, integration testing | GitHub Pre-release (tagged) |
| **Nightly** | Unstable | Development, feature preview, CI integration | GitHub Nightly pre-release |
| **PR** | Experimental | Testing specific changes before merge | GitHub Actions artifacts |

## Stable

Stable releases are tagged with SemVer versions, for example `v1.2.3`.

- **Quality:** Full CI pipeline passes, container smoke tests pass, lifecycle tests pass, changelog entry required.
- **Artifacts:** `.deb`, `.rpm`, tarball, checksums, SBOM, build provenance attestation.
- **Support:** Best-effort community support. Security fixes are backported to the latest stable minor version.
- **Upgrade path:** Forward upgrades to newer stable versions are safe. Persistent data and configs are preserved.

### Install stable

Set `VERSION` to the release you want, without the leading `v`:

```bash
VERSION="<version>"
BASE_URL="https://github.com/o3kio/chv/releases/download/v${VERSION}"
curl -sLO "${BASE_URL}/chv-controlplane_${VERSION}_amd64.deb"
curl -sLO "${BASE_URL}/chv-node_${VERSION}_amd64.deb"
curl -sLO "${BASE_URL}/chvctl_${VERSION}_amd64.deb"
sudo dpkg -i chv-controlplane_${VERSION}_amd64.deb chv-node_${VERSION}_amd64.deb chvctl_${VERSION}_amd64.deb
```

Full instructions: [Debian / Ubuntu](debian-ubuntu.md) or [RHEL / Rocky / AlmaLinux](rhel-rocky-alma.md)

## RC (Release Candidate)

RC releases are tagged as `v<version>-rc.1`, `v<version>-rc.2`, and so on.

- **Quality:** Same CI pipeline as stable, but may contain unfinished edge cases.
- **Artifacts:** Same as stable.
- **Support:** Community support. RCs are intended for validation, not production.
- **Upgrade path:** Can upgrade to the final stable release with the same minor version.

### When to use RC

- You need a specific fix or feature that is not yet in stable.
- You are validating CHV in a staging environment before a stable release.
- You are a contributor testing the release pipeline.

### Install RC

Download from the GitHub Pre-release page. The install command is identical to stable.

## Nightly

Nightly packages are built automatically from every merge to `main`.

- **Quality:** Automated tests pass, but the code may contain regressions, breaking changes, or incomplete features.
- **Artifacts:** `.deb`, `.rpm`, checksums.
- **Support:** No support guarantee. File issues against the specific commit if you find bugs.
- **Upgrade path:** Nightly and RC versions carry a `~` pre-release suffix on both `.deb` and `.rpm` (the CI derives one version string and stamps it on both formats). `~` sorts before the stable release in Debian and RPM ordering alike, so nightly → RC → stable is a forward upgrade on both formats.

### Version format

```text
<version>~nightly.20260510.g0872c4a7   (Debian and RPM)
```

The version includes the date and git short SHA, making every nightly build uniquely identifiable.

### When to use nightly

- You want to test the latest changes.
- You are developing integrations against CHV and need bleeding-edge APIs.
- You are a contributor validating a fix on real hardware.

### Install nightly

Download from the rolling [CHV Nightly](https://github.com/o3kio/chv/releases/tag/nightly) GitHub pre-release.

> **Warning:** Do not use nightly packages in production. Use them only on disposable test hosts or VMs.

## PR artifacts

Every pull request to `main` triggers a package build. The packages are uploaded as GitHub Actions artifacts with 7-day retention.

- **Quality:** The PR's CI passes, but the code is unmerged and may be rejected or revised.
- **Artifacts:** `.deb`, `.rpm`, checksums.
- **Support:** No support. These artifacts are for manual testing by reviewers and contributors.
- **Retention:** 7 days.

### When to use PR artifacts

- You are reviewing a PR and want to test the changes on real hardware.
- You are a contributor sharing a build with a reviewer.

### Install PR artifacts

1. Go to the PR's GitHub Actions page.
2. Find the "PR Packages" workflow run.
3. Download the artifact (`chv-packages-pr-N`).
4. Install the `.deb` or `.rpm` files manually.

## Version precedence

Package managers order versions from oldest to newest — the same order on both formats as shipped (the CI stamps the Debian-derived `~` string on the `.rpm` too):

```text
nightly < RC < stable
```

Examples:
- `<version>~nightly.20260510.g0872c4a7` < `<version>~rc.1` < `<version>` (both formats, as shipped)

`~` is the pre-release operator in both Debian and RPM version comparison,
so upgrading from nightly → RC → stable is always a forward upgrade.

`scripts/version.sh`'s per-format RPM path emits the same `~` suffix as
the Debian path, so nightly stays below stable on RPM even if the
pipeline ever adopts per-format version strings (the former `^`
post-release output, which would have sorted above the same-base stable,
was removed; see
[versioning policy](../release/versioning-policy.md) §3.1).

## Switching channels

### Nightly → Stable

Install the stable release over the nightly package:

```bash
# Debian/Ubuntu
sudo dpkg -i chv-controlplane_<version>_amd64.deb chv-node_<version>_amd64.deb chvctl_<version>_amd64.deb

# RHEL/Rocky/Alma
sudo rpm -U chv-controlplane-<version>-1.x86_64.rpm chv-node-<version>-1.x86_64.rpm chvctl-<version>-1.x86_64.rpm
```

### Stable → Nightly (not recommended)

Downgrading from stable to nightly is possible but not recommended. The package manager may require `--force` flags.

## Repository publishing (future)

Once apt and yum repositories are configured, you will be able to install CHV using standard package manager commands:

```bash
# apt (future)
sudo apt install chv-controlplane chv-node chvctl

# dnf (future)
sudo dnf install chv-controlplane chv-node chvctl
```

See [Nightly Packages](../release/nightly-packages.md) for repository configuration details.

## See also

- [Release Process](../release/release-process.md) — how releases are built and published
- [Verify Release Artifacts](../release/verify-release-artifacts.md) — checksum and signature verification
- [Nightly Packages](../release/nightly-packages.md) — nightly build internals
