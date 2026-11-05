# Install CHV on RHEL, Rocky Linux, and AlmaLinux

This guide covers installing CHV packages on RHEL-compatible systems.

## Supported versions

| OS | Minimum version |
|----|-----------------|
| RHEL | 9 |
| Rocky Linux | 9 |
| AlmaLinux | 9 |

Published CHV packages and release tarballs are `amd64` (`x86_64`) only. `arm64` is unsupported: `scripts/install.sh` exits before any download with a clear "use an x86_64 host" message, and `scripts/quick-install.sh` resolves architecture-specific asset names and aborts at release-asset resolution (before `dnf`/`rpm`) with the same explanation.

## Prerequisites

- Root or `sudo` access
- Cloud Hypervisor (the VMM) installed at `/usr/bin/cloud-hypervisor` — the packages neither install nor download it. Pre-stage the binary yourself, or use the all-in-one installer, which downloads it when missing.
- KVM access (`/dev/kvm` readable)

## Option 1 — Package repository (future)

> **Not yet available.** Once the CHV yum repository is configured, you will be able to install directly:
>
> ```bash
> # Add the repository (future)
> sudo tee /etc/yum.repos.d/chv.repo <<'EOF'
> [chv]
> name=CHV
> baseurl=https://repo.example.com/chv/stable/rpm/$basearch
> enabled=1
> gpgcheck=0
> EOF
>
> # Install
> sudo dnf install chv-controlplane chv-node chvctl
> ```
>
> See [Channels](channels.md) for stable, RC, and nightly repository options.

## Option 2 — Manual `.rpm` install (current)

Download the `.rpm` packages from the [GitHub Releases](https://github.com/o3kio/chv/releases) page and install them manually.

### 1. Download packages

Set `VERSION` to the release you want, without the leading `v`:

```bash
VERSION="<version>"
# GitHub rewrites RC asset filenames: X.Y.Z-rc.N -> X.Y.Z.rc.N.
ASSET_VERSION="${VERSION/-rc./.rc.}"
BASE_URL="https://github.com/o3kio/chv/releases/download/v${VERSION}"

curl -sLO "${BASE_URL}/chv-controlplane-${ASSET_VERSION}-1.x86_64.rpm"
curl -sLO "${BASE_URL}/chv-node-${ASSET_VERSION}-1.x86_64.rpm"
curl -sLO "${BASE_URL}/chvctl-${ASSET_VERSION}-1.x86_64.rpm"
```

### 2. Verify checksums (recommended)

```bash
curl -sLO "${BASE_URL}/SHA256SUMS"
sha256sum -c SHA256SUMS
```

### 3. Install packages

```bash
sudo rpm -i chv-controlplane-${ASSET_VERSION}-1.x86_64.rpm \
         chv-node-${ASSET_VERSION}-1.x86_64.rpm \
         chvctl-${ASSET_VERSION}-1.x86_64.rpm
```

If dependencies are missing:

```bash
sudo dnf install -y <missing-dependency>
# Then retry the rpm -i command
```

### 4. Verify installation

```bash
# Check CLI version
chvctl --version

# Check daemon versions
chv-controlplane --version
chv-agent --version
chv-stord --version
chv-nwd --version
```

### 5. Configure services

Before starting services, edit the configuration files:

```bash
sudo editor /etc/chv/controlplane.toml
sudo editor /etc/chv/agent.toml
sudo editor /etc/chv/stord.toml
sudo editor /etc/chv/nwd.toml
```

Key settings to review:
- `jwt_secret` in `controlplane.toml` — must be ≥ 32 characters
- `control_plane_addr` in `agent.toml` — must point to a reachable control plane
- TLS certificate paths — generate or provision certs before enabling mTLS

> **Do not start services with default/example configs in production.** The default `jwt_secret` is insecure and will be regenerated automatically, but you should set an explicit secret.

### 6. Start services

Services are installed but **not automatically started or enabled**:

```bash
# Reload systemd to recognize new units
sudo systemctl daemon-reload

# Enable and start control plane
sudo systemctl enable --now chv-controlplane

# Enable and start node services
sudo systemctl enable --now chv-agent chv-stord chv-nwd

# Check status
sudo systemctl status chv-agent
```

## Upgrade

To upgrade to a newer version, download the new packages and install them over the old ones:

```bash
# Download new packages
VERSION="<new-version>"
# GitHub rewrites RC asset filenames: X.Y.Z-rc.N -> X.Y.Z.rc.N.
ASSET_VERSION="${VERSION/-rc./.rc.}"
BASE_URL="https://github.com/o3kio/chv/releases/download/v${VERSION}"
curl -sLO "${BASE_URL}/chv-controlplane-${ASSET_VERSION}-1.x86_64.rpm"
curl -sLO "${BASE_URL}/chv-node-${ASSET_VERSION}-1.x86_64.rpm"
curl -sLO "${BASE_URL}/chvctl-${ASSET_VERSION}-1.x86_64.rpm"

# Upgrade (config files are preserved as .rpmsave if modified)
sudo rpm -U chv-controlplane-${ASSET_VERSION}-1.x86_64.rpm \
         chv-node-${ASSET_VERSION}-1.x86_64.rpm \
         chvctl-${ASSET_VERSION}-1.x86_64.rpm

# Restart services to pick up new binaries
sudo systemctl restart chv-controlplane chv-agent chv-stord chv-nwd
```

Your existing data in `/var/lib/chv` and config modifications in `/etc/chv` are preserved during upgrade.

## Remove

```bash
# Remove packages (data is preserved)
sudo rpm -e chv-node chv-controlplane chvctl
```

> **Data safety:** `/var/lib/chv`, `/etc/chv`, and `/var/log/chv` are intentionally preserved on removal. This prevents accidental data loss.
>
> To completely erase all CHV data after backing up:
> ```bash
> sudo rm -rf /var/lib/chv /etc/chv /var/log/chv
> sudo userdel chv
> sudo groupdel chv
> ```

## Troubleshooting

### `rpm` fails with dependency errors

Install the missing dependencies first:

```bash
sudo dnf install -y <missing-package>
```

### Services fail to start

Check logs and config:

```bash
sudo journalctl -u chv-controlplane -n 50
sudo journalctl -u chv-agent -n 50
```

Common issues:
- Missing `jwt_secret` or too short
- TLS certificates not found at configured paths
- `control_plane_addr` unreachable from the node

### `/dev/kvm` not accessible

```bash
# Add your user to the kvm group
sudo usermod -aG kvm $USER
# Log out and back in for group change to take effect
```

## See also

- [Channels](channels.md) — stable, RC, nightly, and PR artifacts
- [Uninstall](uninstall.md) — complete removal and data cleanup
- [Verify Release Artifacts](../release/verify-release-artifacts.md) — checksum and signature verification
