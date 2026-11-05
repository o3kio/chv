# Nightly Packages

This document describes the CHV nightly package builds, how to install them, and what to expect. For the full channel comparison, see [CHV Release Channels](../install/channels.md).

## What is a nightly build?

Every merge to the `main` branch triggers an automated build that produces installable `.deb` and `.rpm` packages. These are called **nightly packages**.

Nightly packages let you test the latest features, verify bug fixes, and validate integrations before a stable release is cut.

### Version format

Nightly versions include the date and git short SHA:

```text
<version>~nightly.20260510.g0872c4a7   (Debian and RPM, as shipped by CI)
```

This guarantees that each nightly build is uniquely identifiable and traceable to a specific commit.

### Support expectation

| Aspect | Expectation |
|--------|-------------|
| **Stability** | Unstable. Nightly builds may contain unfinished features, regressions, or breaking changes. |
| **Data safety** | Do not use nightly packages in production. Use them only on disposable test nodes or VMs. |
| **Upgrade path** | Newer nightlies and later stable releases are upgrades. On RPM, the same-base stable release sorts below a nightly; see [From nightly to stable](#from-nightly-to-stable). |
| **Support** | Community / best-effort. File issues against the specific commit if you find bugs. |
| **Retention** | GitHub nightly release assets are retained indefinitely but may be replaced. Package repository retention depends on storage policy. |

## Installation

### Option 1 — GitHub nightly release (current)

Until the package repository is fully configured, nightly packages are attached to the rolling [CHV Nightly](https://github.com/o3kio/chv/releases/tag/nightly) GitHub pre-release.

#### Debian / Ubuntu

```bash
# Download the latest .deb files from the Nightly release page.
# GitHub rewrites release-asset filenames, so the published name uses '.'
# where the package's internal Version uses '~'.
curl -sL "https://github.com/o3kio/chv/releases/download/nightly/chv-controlplane_<version>.nightly.$(date +%Y%m%d).g$(curl -s https://api.github.com/repos/o3kio/chv/releases/tags/nightly | jq -r '.target_commitish' | head -c7)_amd64.deb" -o chv-controlplane.deb

# Or download manually from the browser, then install:
sudo dpkg -i chv-controlplane_*.deb chv-node_*.deb chvctl_*.deb
```

#### RHEL / CentOS / Fedora

```bash
# Download the latest .rpm files from the Nightly release page, then:
sudo rpm -i chv-controlplane-*.rpm chv-node-*.rpm chvctl-*.rpm
```

### Option 2 — Package repository (future)

Once the nightly apt/yum repository is configured, you will be able to install directly:

#### apt (Debian / Ubuntu)

```bash
# Add the nightly repository
echo "deb [trusted=yes] https://repo.example.com/chv nightly main" | \
  sudo tee /etc/apt/sources.list.d/chv-nightly.list

# Install
sudo apt update
sudo apt install chv-controlplane chv-node chvctl
```

To switch to the stable channel later:

```bash
sudo sed -i 's/nightly/stable/' /etc/apt/sources.list.d/chv-nightly.list
sudo apt update
sudo apt install chv-controlplane chv-node chvctl
```

#### yum / dnf (RHEL / CentOS / Fedora)

```bash
# Add the nightly repository
sudo tee /etc/yum.repos.d/chv-nightly.repo <<'EOF'
[chv-nightly]
name=CHV Nightly
baseurl=https://repo.example.com/chv/nightly/yum/$basearch
enabled=1
gpgcheck=0
EOF

# Install
sudo dnf install chv-controlplane chv-node chvctl
```

To switch to the stable channel later:

```bash
sudo sed -i 's|nightly/yum|stable/yum|' /etc/yum.repos.d/chv-nightly.repo
sudo dnf clean all
sudo dnf install chv-controlplane chv-node chvctl
```

## Upgrading

### From an older nightly

Nightly packages use the same package name as stable releases, so your package manager treats newer nightlies as upgrades:

```bash
# Debian/Ubuntu
sudo dpkg -i chv-controlplane_<version>.nightly.NEW_amd64.deb
sudo apt-get install -f

# Or via apt once the repo is configured
sudo apt upgrade

# RHEL/CentOS/Fedora
sudo rpm -U chv-controlplane-<version>~nightly.NEW-1.x86_64.rpm

# Or via dnf once the repo is configured
sudo dnf upgrade
```

### From nightly to stable

The CI derives one version string with `scripts/version.sh --deb` and
stamps it on both the `.deb` and the `.rpm` (`scripts/build-packages.sh`
applies a single `PACKAGE_VERSION`). So a shipped nightly or RC carries a
`~` pre-release suffix on **both** formats, and sorts below the stable
release on both:

| Comparison | Result |
|------------|--------|
| `<version>` vs `<version>~nightly.20260510.g0872c4a7` (`.deb`, as shipped) | `<version>` is newer |
| `<version>` vs `<version>~nightly.20260510.g0872c4a7` (`.rpm`, as shipped) | `<version>` is newer |

`~` is the pre-release operator in both Debian and RPM version ordering.

`scripts/version.sh`'s per-format RPM channel path emits the same `~`
suffix (`--rpm nightly` produces the same
`<version>~nightly.…` string as `--deb`), so the below-stable ordering
holds even if the pipeline ever adopts per-format version strings. That
path previously emitted a `^` (post-release) suffix, which would have
sorted **above** the same-base stable in RPM; it was removed (#432).

Installing a stable release over a nightly is a normal upgrade on both
formats:

```bash
# Debian/Ubuntu — stable .deb files
sudo dpkg -i chv-controlplane_<version>_amd64.deb chv-node_<version>_amd64.deb chvctl_<version>_amd64.deb
sudo apt-get install -f
```

On RPM, the same rule holds as shipped: the CI stamps the Debian-derived
`~` version on the RPM too, so installing the stable release over a
nightly is a normal upgrade:

```bash
# RHEL/CentOS/Fedora — stable .rpm files
sudo rpm -U chv-controlplane-<version>-1.x86_64.rpm chv-node-<version>-1.x86_64.rpm chvctl-<version>-1.x86_64.rpm
```

## Removing nightly packages

Removing packages follows the standard package manager workflow. Data is preserved per the [package contract](package-contract.md):

```bash
# Debian/Ubuntu
sudo dpkg -r chv-node chv-controlplane chvctl
sudo apt-get autoremove

# RHEL/CentOS/Fedora
sudo rpm -e chv-node chv-controlplane chvctl
```

> **Note:** `/var/lib/chv`, `/etc/chv`, and `/var/log/chv` are intentionally preserved on removal. Delete them manually only if you are sure you want to destroy all data.

## Nightly workflow internals

The nightly build is produced by `.github/workflows/package-nightly.yml`:

1. Triggered on every push to `main` or manually via `workflow_dispatch`
2. Builds release binaries and Web UI
3. Derives a nightly version with date and git SHA
4. Builds `.deb` and `.rpm` packages
5. Runs container smoke tests for both formats
6. Publishes:
   - **GitHub pre-release** (default): attaches packages to the rolling `nightly` tag
   - **Package repository** (optional): generates apt/yum metadata and uploads if secrets are configured

### Disabling publishing

To run the workflow without publishing (dry-run):

```bash
# Via GitHub UI: workflow_dispatch → dry_run = true
```

This builds and tests packages but skips all publishing steps.

### Required secrets for repository publishing

| Secret | Purpose | Required for |
|--------|---------|--------------|
| `CHV_REPO_S3_BUCKET` | S3 bucket name | S3 upload |
| `CHV_REPO_AWS_ACCESS_KEY_ID` | AWS credential | S3 upload |
| `CHV_REPO_AWS_SECRET_ACCESS_KEY` | AWS credential | S3 upload |
| `CHV_REPO_RSYNC_TARGET` | rsync destination | rsync upload |
| `CHV_REPO_GPG_KEY` | ASCII-armored private key | Repository signing |
| `CHV_REPO_GPG_PASSPHRASE` | GPG key passphrase | Repository signing |

If none of these are configured, the repository publish step runs in dry-run mode and logs what it would have done.

## Gaps and future work

| Gap | Status | Plan |
|-----|--------|------|
| Package repository hosting | Not configured | Configure S3, CloudFront, or self-hosted repo mirror |
| GPG signing | Not configured | Generate a CHV release signing key and store in secrets |
| Repository CDN | Not configured | Add CloudFront or similar in front of S3/static host |
| Multi-arch packages | amd64 only | Add aarch64 builds when runners are available |
| Retention policy | Undefined | Define how many nightly builds to retain |

## References

- [Package Contract](package-contract.md)
- [Versioning Policy](versioning-policy.md)
- [Local Release Commands](local-release-commands.md)
