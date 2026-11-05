# Install CHV from GitHub Releases

This guide covers installing CHV by downloading artifacts directly from GitHub Releases. This method works on any Linux distribution that supports `.deb` or `.rpm` packages.

> **Status:** live — v0.3.0 is the current **stable** release (published,
> not a pre-release; its assets include the `.deb`/`.rpm` packages,
> `SHA256SUMS`, and the `chv-0.3.0-linux-amd64.tar.gz` tarball with its
> `.sha256` sidecar), so the version-pinned URLs and the release tarball
> described below are available today: `releases/latest` resolves v0.3.0 and
> `releases/download/v0.3.0/...` serves its assets. The rolling
> [`nightly` pre-release](https://github.com/o3kio/chv/releases/tag/nightly)
> remains development-only. The all-in-one installer's default `latest` path
> resolves v0.3.0 and installs it; with an explicit `INSTALL_CHV_VERSION`
> pinned to a tag that has no Release, it fails at the tarball download with
> per-tag diagnostics. Building from source (`make build-release`, then
> `INSTALL_CHV_TARBALL_PATH=dist/chv-<version>-linux-amd64.tar.gz ./scripts/install.sh`;
> see [DEPLOYMENT](../DEPLOYMENT.md#build--package-a-release)) remains the
> offline alternative.

## Choose your release

Go to the [CHV Releases](https://github.com/o3kio/chv/releases) page and select a release:

| Release type | Tag example | Who should use it |
|--------------|-------------|-------------------|
| **Stable** | `v<version>` | Production and evaluation |
| **RC** | `v<version>-rc.1` | Pre-release validation |
| **Nightly** | `nightly` | Latest `main` branch, development only |

See [Channels](channels.md) for a detailed comparison.

## Download artifacts

Each release provides:
- `.deb` packages (Debian, Ubuntu)
- `.rpm` packages (RHEL, Rocky, AlmaLinux, Fedora)
- `SHA256SUMS` — checksums for the packages
- `SHA256SUMS.sig` — GPG or cosign signature (when signing secrets are configured)
- `sbom.spdx.json` — Software Bill of Materials
- `chv-<version>-linux-amd64.tar.gz` — release tarball with binaries and install script
- `chv-<version>-linux-amd64.tar.gz.sha256` — checksum for the release tarball

### Quick download

Set `VERSION` to the release tag, without the leading `v`:

```bash
VERSION="<version>"
# GitHub rewrites asset filenames: an RC tag X.Y.Z-rc.N publishes packages
# as X.Y.Z.rc.N (the installed Version stays X.Y.Z~rc.N).
ASSET_VERSION="${VERSION/-rc./.rc.}"
RELEASE_URL="https://github.com/o3kio/chv/releases/download/v${VERSION}"

# Download .deb packages
curl -sLO "${RELEASE_URL}/chv-controlplane_${ASSET_VERSION}_amd64.deb"
curl -sLO "${RELEASE_URL}/chv-node_${ASSET_VERSION}_amd64.deb"
curl -sLO "${RELEASE_URL}/chvctl_${ASSET_VERSION}_amd64.deb"

# Download checksums
curl -sLO "${RELEASE_URL}/SHA256SUMS"
```

For `.rpm` packages, replace the filenames accordingly (see [RHEL/Rocky/Alma](rhel-rocky-alma.md)).

## Verify integrity

Always verify checksums before installing:

```bash
sha256sum -c SHA256SUMS
```

For additional verification (signatures, SBOM, attestations), see [Verify Release Artifacts](../release/verify-release-artifacts.md).

## Install

### Debian / Ubuntu

```bash
sudo dpkg -i chv-controlplane_${ASSET_VERSION}_amd64.deb \
             chv-node_${ASSET_VERSION}_amd64.deb \
             chvctl_${ASSET_VERSION}_amd64.deb
sudo apt-get install -f
```

Full instructions: [Debian / Ubuntu](debian-ubuntu.md)

### RHEL / Rocky / AlmaLinux

```bash
sudo rpm -i chv-controlplane-${ASSET_VERSION}-1.x86_64.rpm \
         chv-node-${ASSET_VERSION}-1.x86_64.rpm \
         chvctl-${ASSET_VERSION}-1.x86_64.rpm
```

Full instructions: [RHEL / Rocky / AlmaLinux](rhel-rocky-alma.md)

## Install from tarball (alternative)

If your distribution does not support `.deb` or `.rpm`, use the release tarball. The install script targets Debian and Ubuntu systems; it installs dependencies with `apt`:

```bash
VERSION="<version>"
TARBALL="chv-${VERSION}-linux-amd64.tar.gz"
curl -sLO "https://github.com/o3kio/chv/releases/download/v${VERSION}/${TARBALL}"

# Extract
tar -xzf "${TARBALL}"
cd "chv-${VERSION}-linux-amd64"

# Run the install script
sudo ./install.sh
```

The install script keeps a preinstalled Cloud Hypervisor VMM and `ch-remote` client only when their major.minor matches the qualified v53.0 pin; any other version is replaced with the pinned, digest-verified static builds. Set `INSTALL_CHV_SKIP_CLOUD_HV=1` to skip that step (the VMM and client are then neither installed nor replaced).

The tarball includes:
- Pre-built binaries (`bin/`)
- Web UI static assets (`ui/`)
- Database migrations (`migrations/`)
- Systemd unit files (`systemd/`)
- Example configs (`*.toml`)
- Nginx config (`nginx/`)

## Configure and start

After installation, edit configs and start services:

```bash
# Edit configs
sudo editor /etc/chv/controlplane.toml
sudo editor /etc/chv/agent.toml

# Start services
sudo systemctl daemon-reload
sudo systemctl enable --now chv-controlplane
sudo systemctl enable --now chv-agent chv-stord chv-nwd
```

> **Do not start services with default configs in production.** Set a strong `jwt_secret` and provision TLS certificates first.

## Upgrade

Download the new release and install over the existing packages. Config files and persistent data are preserved.

```bash
# ASSET_VERSION is the published filename version: X.Y.Z for stable,
# X.Y.Z.rc.N for an RC tag X.Y.Z-rc.N (the installed Version stays X.Y.Z~rc.N).
VERSION="<new-version>"
ASSET_VERSION="${VERSION/-rc./.rc.}"

# Debian/Ubuntu
sudo dpkg -i chv-controlplane_${ASSET_VERSION}_amd64.deb ...

# RHEL/Rocky/Alma
sudo rpm -U chv-controlplane-${ASSET_VERSION}-1.x86_64.rpm ...

# Restart services
sudo systemctl restart chv-controlplane chv-agent chv-stord chv-nwd
```

## See also

- [Channels](channels.md) — choosing between stable, RC, nightly, and PR artifacts
- [Uninstall](uninstall.md) — safe removal and data cleanup
- [Verify Release Artifacts](../release/verify-release-artifacts.md) — signatures and attestations
