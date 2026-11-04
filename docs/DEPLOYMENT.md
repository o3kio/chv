# CHV Deployment Guide — All-in-One Host

This guide deploys CHV on a single Linux host. The host runs the **control plane** (orchestration, API, Web UI) and Cloud Hypervisor (the VMM) as the VM runtime.

> **Version:** 0.3.0  
> **Target:** Ubuntu 22.04/24.04 LTS or equivalent Linux with KVM support  
> **Database:** SQLite (no external database service required)

---

## Table of Contents

1. [Quick Start (One-Liner Install)](#quick-start-one-liner-install)
2. [Quick Install (single command, from GitHub releases)](#quick-install-single-command-from-github-releases)
3. [What Gets Installed](#what-gets-installed)
4. [Build & Package a Release](#build--package-a-release)
5. [Manual Deployment (Step-by-Step)](#manual-deployment-step-by-step)
6. [Hosting the Installer (`get.cellhv.com`)](#hosting-the-installer-getcellhvcom)
7. [Operations & Troubleshooting](#operations--troubleshooting)

---

## Quick Start (One-Liner Install)

Run the qualified all-in-one installer on a fresh Ubuntu server with root access:

```bash
curl -fsSL https://raw.githubusercontent.com/o3kio/chv/main/scripts/install.sh | sudo bash -s
```

Or install a specific version:

```bash
curl -fsSL https://raw.githubusercontent.com/o3kio/chv/main/scripts/install.sh | sudo INSTALL_CHV_VERSION=0.3.0 bash -s
```

The installer script requires **bash**: it uses `set -o pipefail`, which `sh`
(dash on Ubuntu) rejects. The old `curl … | sh -` form does not work — keep
`bash -s`.

**Network defaults** (override with environment variables):

| Variable | Default | Description |
|----------|---------|-------------|
| `INSTALL_CHV_BRIDGE_NAME` | `chvbr0` | Linux bridge name |
| `INSTALL_CHV_BRIDGE_CIDR` | `10.200.0.1/24` | Gateway IP / subnet |
| `INSTALL_CHV_BRIDGE_IFACE` | `ens19` | Host interface attached to bridge (NAT upstream) |

Example with custom network:

```bash
curl -fsSL https://raw.githubusercontent.com/o3kio/chv/main/scripts/install.sh | \
  sudo INSTALL_CHV_BRIDGE_IFACE=eth0 \
  INSTALL_CHV_BRIDGE_CIDR=192.168.100.1/24 \
  bash -s
```

### Installer behavior: release artifact, VMM pin, and firmware

- **Architecture.** `scripts/install.sh` supports **x86_64 only**. On
  `aarch64`/`arm64` it exits before installing dependencies or downloading
  anything, because the release pipeline publishes `linux-amd64` artifacts
  only (issue #647). Use an x86_64 host.
- **Release tarball.** When downloading from GitHub, the installer fetches
  `chv-<version>-linux-amd64.tar.gz` and its published `.sha256` sidecar, and
  refuses to extract unless the digest matches. A release without a sidecar
  (or with a mismatched one) aborts before anything is installed. When a
  release signing key is configured, the sidecar's detached signature is
  required as well (see [Release signature verification](#release-signature-verification-and-trust-model)).
  The local `INSTALL_CHV_TARBALL_PATH` flow uses the operator's file; set
  `INSTALL_CHV_TARBALL_SHA256` to verify it, otherwise the installer warns
  loudly that the file is unverified.
- **Cloud Hypervisor pin.** The installer requires major.minor **53.0** for
  both `cloud-hypervisor` and its version-coupled `ch-remote` client. A
  preinstalled binary at that pin is kept; any other version is replaced with
  the pinned, digest-verified v53.0 static build (with a warning).
  `INSTALL_CHV_ALLOW_VMM_MISMATCH=1` keeps the host binaries instead,
  producing an **off-pin, unqualified** install.
- **Firmware.** `/root/CLOUDHV.fd` is used when present. Otherwise the
  installer downloads the pinned rust-hypervisor-firmware 0.5.0 build to
  `/var/lib/chv/hypervisor-fw` and verifies its sha256 digest (x86_64) before
  installing it. Without firmware, VMs that request it fail to start with
  `firmware not found`.
- **Base OS image.** The installer accepts a pre-downloaded image only from
  root-owned `/root/` locations; the previous attacker-writable `/tmp`
  candidate is gone (#667). The mutable Ubuntu `noble/current` image is
  downloaded over TLS. Set `INSTALL_CHV_BASE_IMAGE_SHA256` to verify the
  image against an out-of-band digest; when unset the installer warns that
  TLS to `cloud-images.ubuntu.com` is the only assurance.
- **Version fallback.** If the GitHub API cannot be reached, the installer
  warns explicitly and falls back to the `0.3.0` offline pin instead of
  silently claiming it resolved the latest release.

| Variable | Default | Description |
|----------|---------|-------------|
| `INSTALL_CHV_ALLOW_VMM_MISMATCH` | unset | Set to `1` to keep host `cloud-hypervisor` / `ch-remote` binaries whose major.minor is not `53.0`. Resulting install is off-pin and unqualified. |
| `INSTALL_CHV_TARBALL_SHA256` | unset | Expected 64-hex sha256 of the local `INSTALL_CHV_TARBALL_PATH` tarball. When unset, local tarballs are used with a clear unverified warning. |
| `CHV_RELEASE_GPG_PUBKEY` | unset | ASCII-armored release signing public key. When set, a detached signature for the tarball checksum sidecar is required and verified; missing/invalid signatures are fatal. |
| `INSTALL_CHV_GPG_PUBKEY_FILE` | unset | Path to the ASCII-armored release public key (alternative to `CHV_RELEASE_GPG_PUBKEY`). |
| `INSTALL_CHV_BASE_IMAGE_SHA256` | unset | Expected 64-hex sha256 of the Ubuntu noble base image (existing, pre-downloaded, or freshly downloaded). |

#### Release signature verification and trust model

The release pipeline must publish this asset for every tarball:

| Asset | Format |
|-------|--------|
| `chv-<version>-linux-amd64.tar.gz.sha256` | Sidecar generated by `sha256sum` (already published) |
| `chv-<version>-linux-amd64.tar.gz.sha256.sig` | **Detached, ASCII-armored GnuPG signature over the `.sha256` sidecar** |

`scripts/install.sh` verifies in this order when a public key is configured:

1. Download the tarball and its `.sha256` sidecar (missing sidecar is fatal).
2. Download `chv-<version>-linux-amd64.tar.gz.sha256.sig`, import the configured
   public key into a throwaway keyring, and run `gpg --verify` over the
   sidecar. A missing or invalid signature is fatal.
3. Verify the tarball digest against the now-signature-covered sidecar.

A `SHA256SUMS.sig` fallback is accepted **only** if that same `SHA256SUMS`
file actually lists the tarball. Current releases list packages only, so
`SHA256SUMS.sig` is not a substitute for signing the tarball sidecar.

When neither `CHV_RELEASE_GPG_PUBKEY` nor `INSTALL_CHV_GPG_PUBKEY_FILE` is
set, the installer still fails closed on a missing or mismatched checksum, but
prints an explicit warning that the release is **unsigned**: a checksum
downloaded alongside the artifact over TLS proves transport integrity, not
authority or provenance. Operators who need authenticity must obtain the
public key and fingerprint through an out-of-band channel (for example the
published release-verification documentation or a signed announcement), not
from the same release page.

Signing is a release-pipeline responsibility: until the signing job publishes
`*.sha256.sig`, a key-configured install fails closed by design. GitHub
artifact attestations are a separate provenance signal and do not replace the
GPG signature check in the installer.

> **Release source:** the one-liner resolves the latest **stable** GitHub
> release and downloads its tarball. v0.3.0 is that release today (published,
> not a pre-release; its assets include the `chv-0.3.0-linux-amd64.tar.gz`
> tarball with a `.sha256` sidecar alongside the `.deb`/`.rpm` packages), so
> this resolution leg is live — tagged releases are the artifact source, and
> the rolling `nightly` pre-release is never selected. The
> `get.cellhv.com` hosting surface is **not stood up** (the name does not
> resolve); see
> [Hosting the Installer (`get.cellhv.com`)](#hosting-the-installer-getcellhvcom)
> for the optional future design. Use the raw-URL commands above, or
> [Quick Install](#quick-install-single-command-from-github-releases) below.

The installer will:

1. Install system dependencies (`nginx`, `qemu-kvm`, `bridge-utils`, `iptables`, etc.)
2. Download and install CHV binaries, Web UI assets, and database migrations
3. Download and install the Cloud Hypervisor VMM
4. Generate a self-signed TLS CA
5. Create and configure the `chvbr0` bridge with NAT
6. Write `controlplane.toml`, `agent.toml`, `stord.toml`, and `nwd.toml`
7. Install systemd services for all four CHV daemons
8. Create a one-time bootstrap token (saved to `/etc/chv/bootstrap.token`)
9. Configure nginx to serve the Web UI and proxy API calls
10. Start all services and wait for the local agent to enroll as a compute node

After ~60 seconds, open the printed IP address in your browser.  
Default login: **admin / (random password)**. The bootstrap password is written
to `/etc/chv/initial_admin_password` (mode 0600, root-owned) and printed once
in the install output. You will be required to change it on first login.

---

## Quick Install (single command, from GitHub releases)

> **Deployment status:** [CODE-SUPPORTED, UNQUALIFIED]. This path has no
> qualification leg yet — no m4.x campaign or container smoke test
> exercises it. It shares install.sh's conventions (users, `/etc/chv`
> layout, systemd units, chown discipline) but rides the #447
> package-serving shape. `scripts/install.sh` remains the qualified
> deployment path.

A curl-pipe-bash installer for trying CHV on a fresh Linux host — one
command yields the control plane with the WebUI served by the binary
itself (#447 D3 shape — `[webui]` enabled, **no nginx involved**), plus
the local agent enrolled against it (`chv-agent` + `chv-stord` +
`chv-nwd`), systemd units, and config under `/etc/chv`:

```bash
curl -fsSL https://raw.githubusercontent.com/o3kio/chv/main/scripts/quick-install.sh | sudo bash -s
```

Or pinned to a tagged release, with a dry run available:

```bash
curl -fsSL https://raw.githubusercontent.com/o3kio/chv/main/scripts/quick-install.sh | sudo bash -s -- --version 0.3.0
curl -fsSL https://raw.githubusercontent.com/o3kio/chv/main/scripts/quick-install.sh | sudo bash -s -- --dry-run
```

The script (`scripts/quick-install.sh`, issue #482) resolves the latest
**stable** tagged release via the GitHub API (nightly is deliberately not
supported), detects architecture (amd64/arm64) and distro family
(.deb on Debian-family, .rpm on RH-family, release tarball fallback
elsewhere), downloads the matching release assets, and verifies them
against the release's `SHA256SUMS` (packages) or the tarball's `.sha256`
sidecar — a missing asset or checksum mismatch aborts before anything is
installed. It then installs the artifacts, generates TLS material and
`/etc/chv/*.toml`, seeds the bootstrap token (via the loopback-only
`/internal/bootstrap-token` route) and the bootstrap admin user, enables
and starts the four systemd services, and waits for the local agent's
enrollment.

After it completes, open `http://<host-ip>:8080/` and log in as
**admin** with the password from `/etc/chv/initial_admin_password`
(printed once, mode 0600, root-only; rotation is forced on first login).

### Disclosed judgments and limits

- **`http_bind` is `0.0.0.0:8080`, plain HTTP.** With no nginx edge, the
  control plane's own listener is the only front door. Static assets are
  unauthenticated by design (the login page must load); every API route
  keeps its auth. Do not expose this port to untrusted networks — put an
  edge proxy in front (see "Serving the Web UI in package mode") if you
  need TLS or remote access with a hardened posture.
- **The serial console (`/ws/`) does not work on this path.** The control
  plane binary does not proxy `/ws/` (the #447 decision,
  maintainer-ratified 2026-10-07) and this path installs no edge. VM
  lifecycle works from the WebUI; the console needs an edge proxy.
- **arm64 hosts fail closed today.** Both install paths target x86_64. The
  quick installer resolves architecture-specific asset names (`arm64` for
  `.deb`, `aarch64` for `.rpm`, `linux-arm64` for the tarball) and aborts at
  asset resolution before running `dpkg`/`rpm`, because the release pipeline
  publishes linux-amd64 assets only. `scripts/install.sh` likewise exits
  before downloading on arm64. This is a pipeline gap, not a host problem.
- **Idempotency.** A re-run upgrades artifacts in place; config under
  `/etc/chv`, certs, the database, and the bootstrap/admin secrets are
  never overwritten. An existing deployment this script did not configure
  (packages deployed by hand, or `install.sh`) is refused unless `--force`
  is given — `install.sh`'s `/opt/chv/ui` layout and nginx edge are never
  silently converted. A control-plane database without configuration (a
  partial or hand-managed state) is likewise refused unless `--force`:
  fresh config would mint a new `jwt_secret` against a live database and
  invalidate existing sessions and agent tokens.
- **VMM pin.** The same v53.0 policy as `install.sh` applies: a preinstalled
  `cloud-hypervisor` or `ch-remote` at major.minor 53.0 is kept; any other
  version is replaced with the pinned, digest-verified static build.
  `INSTALL_CHV_ALLOW_VMM_MISMATCH=1` keeps the host binaries instead, making
  the install off-pin and unqualified.
- **Uninstall.** `sudo scripts/quick-install.sh --uninstall` removes the
  software and preserves `/etc/chv` and `/var/lib/chv`;
  `--uninstall --purge` also removes data and config (the `chv` and
  `chv-stord` users are retained, matching
  [docs/install/uninstall.md](install/uninstall.md)).
- **No bridge/NAT bootstrap, no base image, no seeded test VM.** This
  path stops at "enrolled node + WebUI". Networks and images are created
  from the WebUI/API (or use the qualified `install.sh` path for its
  dev-resource seeding).

### Tier-label gate

The label stays [CODE-SUPPORTED, UNQUALIFIED] until this path's own
qualification leg passes — the fresh-host end-to-end smoke tracked in
#556 (run the one-liner on a clean Linux host and assert package
install, config generation, service bring-up, agent enrollment, and
WebUI serving), in the same gate shape as the container package-smoke
leg that ran in #549 (2026-10-08).

---

## What Gets Installed

### Processes on the Host

```
┌─────────────────────────────────────────────────────────────┐
│                      Combined Host                          │
│  ┌─────────────────┐  ┌─────────────────────────────────┐   │
│  │  chv-controlplane│  │        chv-agent               │   │
│  │  gRPC :8443      │◄─┤  enrolls to control plane      │   │
│  │  HTTP :8080      │  │  manages chv-stord / chv-nwd   │   │
│  │  SQLite DB       │  │  launches cloud-hypervisor VMs │   │
│  └─────────────────┘  └─────────────────────────────────┘   │
│           ▲                        │                        │
│           │                        ├─► chv-stord (daemon)   │
│           │                        └─► chv-nwd   (daemon)   │
│      nginx :80 (proxy edge — the binary serves the UI)      │
│  ┌─────────────────────────────────────────────────────┐    │
│  │  chvbr0 (10.200.0.1/24) ──NAT──► ens19 ──► internet │    │
│  └─────────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────────┘
```

### Key Files & Directories

| Path | Purpose |
|------|---------|
| `/usr/bin/chv-*` | CHV binaries |
| `/usr/bin/cloud-hypervisor` | Cloud Hypervisor VMM |
| `/etc/chv/controlplane.toml` | Control plane config |
| `/etc/chv/agent.toml` | Agent config |
| `/etc/chv/stord.toml` | Storage daemon config |
| `/etc/chv/nwd.toml` | Network daemon config (socket, runtime dir; bridges are API-created from the network id via `chv_common::bridge_name_for_network`) |
| `/etc/chv/certs/` | TLS CA and certificates |
| `/var/lib/chv/controlplane.db` | SQLite database |
| `/var/lib/chv/cache/` | Agent durable cache |
| `/var/lib/chv/storage/localdisk/` | Local disk storage pool |
| `/opt/chv/ui/` | Web UI static files |
| `/usr/local/share/chv/migrations/` | Database migration files |

### Default Ports

| Port | Service | Bound To |
|------|---------|----------|
| 8443 | gRPC (control plane ↔ agent) | `127.0.0.1` |
| 8080 | HTTP admin API (+ Web UI when `[webui]` is enabled) | `127.0.0.1` |
| 80 | Edge proxy (nginx; Web UI + API) | `0.0.0.0` |
| 9103 | Control-plane metrics (`metrics_bind`) | `127.0.0.1` |
| 9901 | Agent metrics (optional) | `127.0.0.1` |
| 9101 | Storage daemon metrics (`metrics_bind`) | `127.0.0.1` |
| 9102 | Network daemon metrics (`metrics_bind`) | `127.0.0.1` |

The metrics listeners are unauthenticated and loopback by default; Prometheus
scrapes them without an admin JWT. See
[docs/OBSERVABILITY.md](OBSERVABILITY.md) for the auth model and remote-scrape
guidance.

### Verification

```bash
# Check services
systemctl status chv-controlplane
systemctl status chv-agent
systemctl status chv-stord
systemctl status chv-nwd
systemctl status nginx

# Health endpoint
curl http://127.0.0.1:8080/health

# List nodes (should show the local host after enrollment)
curl -s http://127.0.0.1:8080/v1/nodes -X POST \
  -H "Content-Type: application/json" -d '{}' | jq .

# Logs
journalctl -u chv-controlplane -f
journalctl -u chv-agent -f
```

---

## Build & Package a Release

If you are developing CHV or want to host your own installer, use the build script in this repository.

### Prerequisites
- Rust toolchain (`rustup`)
- Node.js 22.17+ and `npm`
- Ubuntu/Debian build host

### Build the Release Tarball

```bash
# From the repository root
./scripts/build-release.sh
```

This will:
1. Build all Rust binaries in release mode
2. Build the SvelteKit Web UI
3. Assemble a release directory with binaries, UI, migrations, configs, and the installer
4. Create `dist/chv-<VERSION>-linux-amd64.tar.gz`
5. Generate a SHA256 checksum

### Install from a Local Build (Dev/Test)

```bash
# Build, uninstall previous version, and reinstall in one step
sudo ./scripts/dev-install.sh

# First-time install (skip uninstall step)
sudo ./scripts/dev-install.sh --no-uninstall

# Override network defaults
sudo INSTALL_CHV_BRIDGE_IFACE=eth0 ./scripts/dev-install.sh
```

Or manually:

```bash
./scripts/build-release.sh
sudo INSTALL_CHV_TARBALL_PATH=dist/chv-<version>-linux-amd64.tar.gz ./scripts/install.sh
```

---

## Manual Deployment (Step-by-Step)

If you prefer to deploy manually or need to customize every step, follow the detailed guide below.

### 1. Prerequisites

#### Hardware
- x86_64 server with **hardware virtualization** (VT-x / AMD-V)
- Minimum 4 cores, 8 GB RAM, 50 GB disk (unevidenced recommendation — see caveat below)

> These minimums are an unevidenced documentation guideline, not a
> qualification-backed envelope. The qualified deployment host was 16 vCPU
> with 31 GiB RAM; no scale claims are derivable from it.

#### Software
```bash
sudo apt update
sudo apt install -y \
  build-essential curl git pkg-config libssl-dev \
  nginx qemu-kvm bridge-utils iproute2 iptables

# Verify KVM
ls /dev/kvm
```

#### Cloud Hypervisor
```bash
CHV_VERSION="53.0"
CHV_SHA256="448af3d4e59b22c2987f7df94c213ad40fb53a10d437e42b5ee6c4fce7c29ecc"
curl -fsSL "https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v${CHV_VERSION}/cloud-hypervisor-static" \
  -o /usr/local/bin/cloud-hypervisor
echo "${CHV_SHA256}  /usr/local/bin/cloud-hypervisor" | sha256sum -c -
chmod +x /usr/local/bin/cloud-hypervisor
ln -sf /usr/local/bin/cloud-hypervisor /usr/bin/cloud-hypervisor
cloud-hypervisor --version
```

### 2. Build from Source

```bash
# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env

# Node.js (setup_22.x installs the latest 22.x release; CHV requires >= 22.17)
curl -fsSL https://deb.nodesource.com/setup_22.x | sudo -E bash -
sudo apt install -y nodejs

# Build binaries
cargo build --workspace --release

# Build UI
cd ui && npm install && npm run build
```

### 3. Host Preparation

```bash
sudo useradd --system --no-create-home --shell /usr/sbin/nologin chv
sudo usermod -aG kvm chv

sudo mkdir -p /etc/chv/certs
sudo mkdir -p /var/lib/chv/{cache,images,storage/localdisk}
sudo mkdir -p /var/log/chv
sudo mkdir -p /run/chv/{controlplane,agent,stord,nwd}
sudo mkdir -p /opt/chv/ui
sudo mkdir -p /usr/local/share/chv/migrations

sudo chown -R chv:chv /var/lib/chv /var/log/chv /run/chv
sudo chmod 750 /var/lib/chv /var/log/chv
```

### 4. Install Binaries & Assets

```bash
sudo install -m 755 target/release/chv-controlplane /usr/local/bin/
sudo install -m 755 target/release/chv-agent        /usr/local/bin/
sudo install -m 755 target/release/chv-stord        /usr/local/bin/
sudo install -m 755 target/release/chv-nwd          /usr/local/bin/

sudo cp -r ui/build/* /opt/chv/ui/
sudo chown -R www-data:www-data /opt/chv/ui

sudo cp -r cmd/chv-controlplane/migrations/* /usr/local/share/chv/migrations/
sudo chown -R chv:chv /usr/local/share/chv/migrations
```

### 5. Bridge and NAT Network Setup

```bash
BRIDGE_NAME="chvbr0"
BRIDGE_CIDR="10.200.0.1/24"
BRIDGE_NET="10.200.0.0/24"
UPSTREAM_IFACE="ens19"

# Create bridge
ip link add name $BRIDGE_NAME type bridge
ip link set $BRIDGE_NAME up
ip addr add $BRIDGE_CIDR dev $BRIDGE_NAME

# Attach upstream interface
ip link set $UPSTREAM_IFACE master $BRIDGE_NAME
ip link set $UPSTREAM_IFACE up

# Enable IP forwarding and NAT
sysctl -w net.ipv4.ip_forward=1
echo "net.ipv4.ip_forward=1" | sudo tee /etc/sysctl.d/99-chv-forward.conf

iptables -t nat -A POSTROUTING -s $BRIDGE_NET ! -d $BRIDGE_NET -j MASQUERADE

# Persist iptables
sudo iptables-save | sudo tee /etc/iptables/rules.v4
```

### 6. TLS Setup

```bash
sudo openssl genrsa -out /etc/chv/certs/ca.key 4096
sudo openssl req -x509 -new -nodes -key /etc/chv/certs/ca.key \
  -sha256 -days 3650 -out /etc/chv/certs/ca.crt \
  -subj "/O=CHV/CN=chv-ca"

sudo chmod 640 /etc/chv/certs/ca.key
sudo chmod 644 /etc/chv/certs/ca.crt
sudo chown root:chv /etc/chv/certs/ca.key /etc/chv/certs/ca.crt
```

### 7. Configuration

**`/etc/chv/controlplane.toml`**
```toml
grpc_bind = "127.0.0.1:8443"
http_bind = "127.0.0.1:8080"
# Dedicated Prometheus listener (unauthenticated, loopback). The legacy
# /metrics route on http_bind stays admin-gated; scrapers use this instead.
metrics_bind = "127.0.0.1:9103"
log_level = "info"
runtime_dir = "/run/chv/controlplane"
# Legacy migration master. Per-purpose keys below are derived from this
# value when they are not configured explicitly.
jwt_secret = "<generate with: openssl rand -base64 32>"

# Per-purpose secrets (#662): >=32 chars each and distinct from one another.
# user_jwt signs end-user sessions; console_jwt signs serial-console JWTs;
# agent_auth_token authenticates every control-plane → agent RPC.
# The control plane sends ONE agent_auth_token to every agent it serves, so
# all of those agents must use this same value. Per-node tokens are future
# ADR-029 work; rotating one agent's token alone requires re-keying the
# control plane or a maintenance window.
[secrets]
user_jwt = "<generate with: openssl rand -hex 32>"
console_jwt = "<generate once; use the SAME value in agent.toml>"
agent_auth_token = "<generate once; use the SAME value in every served agent.toml>"
# credential_encryption = "<optional; overrides CHV_ENCRYPTION_KEY>"

[security]
# Fail-closed default. The local-dev bypass is refused while this is true.
production = true

[database]
url = "sqlite:///var/lib/chv/controlplane.db"
migrations_dir = "/usr/local/share/chv/migrations"
max_connections = 4
min_connections = 1
acquire_timeout_secs = 5

[tls]
ca_cert_path = "/etc/chv/certs/ca.crt"
ca_key_path = "/etc/chv/certs/ca.key"
```

**`/etc/chv/agent.toml`**
```toml
socket_path = "/run/chv/agent/api.sock"
runtime_dir = "/var/lib/chv/agent"
log_level = "info"
control_plane_addr = "https://127.0.0.1:8443"
stord_socket = "/run/chv/stord/api.sock"
nwd_socket = "/run/chv/nwd/api.sock"
chv_binary_path = "/usr/bin/cloud-hypervisor"
stord_binary_path = "/usr/local/bin/chv-stord"
nwd_binary_path = "/usr/local/bin/chv-nwd"
cache_path = "/var/lib/chv/cache/agent-cache.json"
# Single durable lifecycle authority (prompt-02 cutover): core-managed runs
# CellHV Core as the sole authority on this node — the qualified default.
# "legacy" is a compatibility adapter only (explicit opt-in).
authority_mode = "core-managed"
node_id = ""
metrics_bind = "127.0.0.1:9901"
bootstrap_token_path = "/etc/chv/bootstrap.token"
tls_cert_path = "/run/chv/agent/agent.crt"
tls_key_path = "/run/chv/agent/agent.key"
ca_cert_path = "/etc/chv/certs/ca.crt"

# Per-purpose secrets (#662). These MUST match the control plane's values.
# On a single-host upgrade you may omit [secrets] entirely: both sides
# derive the same keys from a matching legacy jwt_secret. Configure them
# explicitly to rotate a purpose independently. agent_auth_token is shared:
# every agent served by one control plane uses that control plane's single
# token (per-node tokens are future ADR-029 work).
[secrets]
console_jwt = "<same as controlplane.toml [secrets] console_jwt>"
agent_auth_token = "<same as controlplane.toml [secrets] agent_auth_token>"

[security]
# Fail-closed default. On the agent, allow_insecure_agent_auth is a
# local-development bypass and is refused while production = true. On the
# control plane this key only relaxes config validation; it never changes
# the token sent to agents.
production = true
# Agent-only: Unix UIDs allowed to connect to the agent socket. Empty means
# the agent's own effective UID. The control plane ignores this key.
# agent_auth_allowed_peer_uids = []

# Stord respawn config fidelity (#385): point the supervisor at the
# operator's stord.toml so a respawned stord keeps every operator key
# (runtime_dir, backend_type, device_allowlist, [migration]) instead of
# a minimal generated config. Omitted = historical generated respawn.
stord_config_path = "/etc/chv/stord.toml"

# nwd respawn config fidelity (#504): same pass-through for the network
# daemon — point the supervisor at the operator's nwd.toml so a
# respawned nwd keeps every operator key (the [overlay], [ebpf] and
# [fabric] blocks) instead of a minimal generated config. Omitted =
# historical generated respawn.
nwd_config_path = "/etc/chv/nwd.toml"

# Guest-liveness (boot) watchdog — OPT-IN, disabled when omitted.
# Detects a guest frozen mid-boot (the cloud-hypervisor serial-manager
# defect, present at both the former v43.0 pin and the current v53.0 pin
# — #448 campaign leg 02: the console stalls with no boot-complete
# marker while vm.info keeps reporting Running) and recovers it with a
# bounded vm.reboot.
# Only enable this on nodes whose guest images print the marker:
# a marker-based detector cannot distinguish a frozen boot from a quiet
# marker-less guest, nor from an adopted guest whose console wrapped
# past its banner (all bounded by max_reboots).
# The stall window is progress-based, not rate-based: any console byte
# resets it. A console crawling through a v53.0 pre-connect backlog
# (CH #8322 — after a late attach, the boot marker can arrive well over
# a minute behind the backlog head) is waited out; only a console that
# goes genuinely silent for stall_secs fires.
[watchdog]
enabled = false
boot_marker = "systemd-logind"  # printed by every systemd boot
stall_secs = 120                # no console bytes for this long = stalled
max_reboots = 2                 # per unhealthy episode, then stand down
healthy_reset_secs = 900        # sustained health resets the budget
```

The control-plane → agent hop is a local Unix-domain socket (`0600`), not
mTLS. Every RPC carries the `x-chv-agent-token` metadata credential; the
agent compares it in constant time and also verifies the connecting UID via
`SO_PEERCRED` before accepting the HTTP/2 connection. A missing or wrong
token is rejected with `UNAUTHENTICATED`, and the agent logs those token
rejections at `warn` (rate-limited to one per 60 seconds with a suppressed
count; token material is never logged). The control plane holds one
`agent_auth_token` and sends it to every agent it serves, so all of those
agents must share it; this is not per-node isolation. On the agent,
`allow_insecure_agent_auth = true` is accepted only with
`production = false`, logs a loud warning, and is for local development only; the agent
also requires a `dev`-feature build (`cargo build --features dev`) or it
refuses to start with the bypass enabled. On the control plane the same key
only relaxes config validation. `agent_auth_allowed_peer_uids` is an
agent-only setting. Rotation and migration from the legacy shared
`jwt_secret` are documented in
[`runbooks/agent-auth-and-secret-rotation.md`](runbooks/agent-auth-and-secret-rotation.md).

**`/etc/chv/nwd.toml`**
```toml
socket_path = "/run/chv/nwd/api.sock"
runtime_dir = "/run/chv/nwd"
log_level = "info"
# Dedicated Prometheus listener; loopback default is 127.0.0.1:9102.
metrics_bind = "127.0.0.1:9102"

# Bridge topology (bridge name, CIDR, upstream interface) is configured at
# runtime via gRPC topology specs from the control plane, not in this file.
```

**`/etc/chv/stord.toml`**
```toml
socket_path = "/run/chv/stord/api.sock"
runtime_dir = "/var/lib/chv/storage/localdisk"
log_level = "info"
# Dedicated Prometheus listener; loopback default is 127.0.0.1:9101.
metrics_bind = "127.0.0.1:9101"
```

### 8. Credential Encryption Key

The control plane encrypts S3 backup credentials and NetBox API tokens at
rest (AES-256-GCM). The key can be supplied either as
`[secrets] credential_encryption` in `controlplane.toml` (#662) or through
the `CHV_ENCRYPTION_KEY` environment variable; the explicit config value
takes precedence. Generate the key once and never regenerate it — a database
restored without the matching key cannot read stored credentials:

```bash
sudo sh -c 'umask 077; printf "CHV_ENCRYPTION_KEY=%s\n" "$(openssl rand -hex 32)" > /etc/chv/encryption.env'
sudo chmod 0600 /etc/chv/encryption.env
```

`chv-controlplane.service` loads it via `EnvironmentFile`. If no usable key
is configured (the file is absent with no legacy `CHV_JWT_SECRET` fallback,
`CHV_ENCRYPTION_KEY` is present but empty, and `[secrets]
credential_encryption` is unset), the control plane logs a warning at startup
and **rejects credential writes** (fail-closed): S3 backup and NetBox
credentials are never stored in plaintext. See OPERATIONS.md
"Credential encryption key" for backup and rotation semantics.

### 9. systemd Services

```bash
sudo cp docs/examples/systemd/chv-controlplane.service /etc/systemd/system/
sudo cp docs/examples/systemd/chv-agent.service        /etc/systemd/system/
sudo cp docs/examples/systemd/chv-stord.service        /etc/systemd/system/
sudo cp docs/examples/systemd/chv-nwd.service          /etc/systemd/system/
# The hardened chv-nwd unit needs /run/netns to exist (root:chv 0770).
# The .deb ships this as a tmpfiles entry; on the tarball/manual path install
# the same entry so it is recreated on every boot, then apply it now:
sudo cp packaging/tmpfiles/chv-node.conf /usr/lib/tmpfiles.d/chv-node.conf
sudo systemd-tmpfiles --create /usr/lib/tmpfiles.d/chv-node.conf
sudo systemctl daemon-reload
```

Service startup order:
- `chv-controlplane` — starts first (runs migrations, opens SQLite DB)
- `chv-stord` and `chv-nwd` — start independently
- `chv-agent` — starts after all three above are up; enrolls with the control plane

### 10. Bootstrap Token

```bash
BOOTSTRAP_TOKEN=$(openssl rand -hex 32)
printf '%s' "$BOOTSTRAP_TOKEN" | sudo tee /etc/chv/bootstrap.token
sudo chmod 640 /etc/chv/bootstrap.token
sudo chown root:chv /etc/chv/bootstrap.token

# Insert token hash into SQLite database (stop the control plane first —
# manual writes to the live database are unsafe; see OPERATIONS.md
# "Live Database Access")
sudo systemctl stop chv-controlplane
TOKEN_HASH=$(printf '%s' "$BOOTSTRAP_TOKEN" | sha256sum | awk '{print $1}')
EXPIRES=$(date -u -d "+1 hour" '+%Y-%m-%dT%H:%M:%SZ')
sqlite3 /var/lib/chv/controlplane.db \
  "INSERT OR IGNORE INTO bootstrap_tokens
   (token_hash, description, one_time_use, expires_at, created_at)
   VALUES ('${TOKEN_HASH}', 'Manual deploy', 1,
           '${EXPIRES}', strftime('%Y-%m-%dT%H:%M:%SZ','now'));"
sudo systemctl start chv-controlplane
```

### 11. Web UI (nginx)

```bash
sudo cp docs/examples/nginx/chv-ui.conf /etc/nginx/sites-available/chv
sudo ln -sf /etc/nginx/sites-available/chv /etc/nginx/sites-enabled/chv
sudo rm -f /etc/nginx/sites-enabled/default
sudo nginx -t
sudo systemctl restart nginx
```

### 12. Start Services

```bash
sudo systemctl enable --now chv-controlplane
sudo systemctl enable --now chv-stord
sudo systemctl enable --now chv-nwd
sudo systemctl enable --now chv-agent
```

---

## Multi-Node WebSocket Console Routing

> **Deployment status:** Multi-node operation is code-supported but unqualified.
> Control-plane dispatch resolves a local Unix-socket path, so
> control-plane-driven operations cannot reach a remote node. The qualified
> topology is single-host.

When CHV runs on multiple hypervisor nodes, VM serial consoles must be routed to the correct node.  There are two deployment modes.

### Direct Mode

Each node has its `agent_ws_address` column set in the `nodes` table (e.g. `192.168.1.10:8444`).  The backend-for-frontend (BFF) returns a full `ws://` or `wss://` URL and the browser connects directly to the node agent.  This is the simplest setup, but it requires the browser to reach every node on the network.

```bash
# Set a node's WebSocket address (stop the control plane first — manual
# writes to the live database are unsafe; see OPERATIONS.md
# "Live Database Access")
sudo systemctl stop chv-controlplane
sqlite3 /var/lib/chv/controlplane.db \
  "UPDATE nodes SET agent_ws_address = '192.168.1.10:8444' WHERE node_id = 'node-1';"
sudo systemctl start chv-controlplane
```

### Proxied Mode (Default)

When `agent_ws_address` is empty (or the client passes `?proxied=true`), the BFF returns a relative path that includes the `node_id`:

```
/ws/vms/{node_id}/{vm_id}/console
```

The short-lived console ticket is **not** in the URL. The UI passes it as
the WebSocket subprotocol (`new WebSocket(url, [ticket])`), which travels in
the `Sec-WebSocket-Protocol` request header (#660) and therefore never lands
in access logs, proxy logs, or browser history. nginx forwards that header
by default; no special configuration is required.

nginx strips the `/ws/vms/{node_id}` prefix and forwards the request to the correct agent backend using a static `map`.  This works when nodes are on private networks or the UI is behind a firewall.

#### Configuring nginx for Multi-Node

1. Open `/etc/nginx/sites-available/chv` (copied from `docs/examples/nginx/chv-ui.conf`).

2. Edit the `map $request_uri $ws_backend` block near the top of the file.  Add one line per compute node:

   ```nginx
   map $request_uri $ws_backend {
       default              127.0.0.1:8444;
       ~^/ws/vms/node-1/   192.168.1.10:8444;
       ~^/ws/vms/node-2/   192.168.1.11:8444;
   }
   ```

   The `node_id` value must match the `node_id` stored in the CHV database.

3. Test and reload nginx:

   ```bash
   sudo nginx -t
   sudo systemctl reload nginx
   ```

#### Security Considerations

- Terminate TLS in nginx and set `X-Forwarded-Proto: https` so the BFF generates `wss://` URLs for direct mode.
- In proxied mode, the WebSocket inherits nginx's TLS termination automatically because the browser connects to the same `wss://` origin.
- Do **not** expose agent console ports (`:8444`) to untrusted networks unless you are using direct mode with mTLS or WSS.

#### Scaling Note

The static `map` approach is production-standard for small-to-medium clusters (tens of nodes).  For very large clusters, consider OpenResty with `lua-resty-upstream` or a dedicated gateway (Envoy, Traefik) that can route dynamically based on path or query parameters.

---

## Serving the Web UI in package mode

> **Deployment status:** [CONTAINER-VERIFIED]. The packaged serving
> shape — `[webui] enabled` behind the packaged proxy-only example
> edge, from the `.deb` layout — is exercised end to end by the
> container package-smoke leg (`scripts/package/smoke-webui.sh`, wired
> into `package-pr.yml`; #549, passed 2026-10-08). The operator still
> owns the edge, including TLS (decision D7), and guest-level UI
> behavior is a separate surface (#355). `scripts/install.sh` remains
> the qualified deployment path.

The `chv-controlplane` package ships the Web UI static tree at
`/usr/share/chv/ui`, and the control plane can serve it directly from
its own HTTP listener (`http_bind`, `127.0.0.1:8080` by default) —
decision D3 target, issue #447. Serving is **opt-in and disabled by
default** (fail-closed): no static assets are served unless the
operator enables the `[webui]` section. The package also ships an
example proxy-only nginx configuration at
`/usr/share/chv/examples/chv-example.conf` for operators who want an
edge (TLS termination, gzip, the `/ws/` console proxy) in front.

### Prerequisites

1. Provision the control plane first. Provide the TLS CA and
   certificates, a real `jwt_secret`, and the admin user. The control
   plane fails closed without them. See
   [PACKAGING.md](PACKAGING.md) "Post-Install Steps".
2. Start the CHV services and verify the loopback listeners: the BFF
   on `127.0.0.1:8080`, the agent serial console on `127.0.0.1:8444`.
   Do not expose either listener directly; an edge proxy is the only
   supported front door.

### Steps

1. Enable the Web UI in `/etc/chv/controlplane.toml`:

   ```toml
   [webui]
   enabled = true
   # dir = "/usr/share/chv/ui"   # the package default
   ```

2. Restart the control plane and verify it serves the UI shell:

   ```bash
   sudo systemctl restart chv-controlplane
   curl -s http://127.0.0.1:8080/ | head
   ```

3. Optional edge: install the example proxy-only configuration (TLS
   termination, gzip, and the `/ws/` serial-console proxy):

   ```bash
   sudo cp /usr/share/chv/examples/chv-example.conf \
        /etc/nginx/sites-available/chv
   sudo ln -sf /etc/nginx/sites-available/chv /etc/nginx/sites-enabled/chv
   sudo rm -f /etc/nginx/sites-enabled/default
   sudo nginx -t
   sudo systemctl enable --now nginx
   ```

### What the binary serves (and what it does not)

- Request paths outside the reserved API prefixes (`/v1`, `/api`,
  `/admin`, `/health*`, `/ready`, `/internal`, `/metrics`) are served
  from `dir`; unmatched ones fall back to `index.html` (the SPA
  fallback, `try_files $uri $uri/ /index.html`). Reserved prefixes
  keep the JSON 404 — an API miss stays machine-readable.
- Cache posture mirrors the previous nginx edge: `index.html` (and
  the SPA fallback) is served `no-cache, no-store, must-revalidate`;
  `/_app/immutable/` (SvelteKit's content-hashed assets) is served
  `public, max-age=31536000, immutable`; other assets carry no
  `Cache-Control`.
- The router's security headers (CSP, `x-content-type-options`,
  `x-frame-options`, `referrer-policy`) apply to served assets.
- Static assets are unauthenticated (the login page must load before
  login); every API route keeps its existing auth and CSRF posture.

### Serial console requires the `/ws/` proxy

In proxied mode the BFF returns console URLs of the form
`/ws/vms/{node_id}/{vm_id}/console`. The short-lived ticket is delivered
separately and carried in the `Sec-WebSocket-Protocol` header (#660), not
the query string. These URLs work only through an edge that proxies `/ws/`
to the agent on `127.0.0.1:8444`: **the control plane binary does not proxy
`/ws/`** (the #447 decision — documented, not implemented;
maintainer-ratified 2026-10-07; see the issue). The example configuration
includes this proxy. Do not remove it; the VM serial console stops working
without it.

### Notes

- TLS termination at the edge is an operator concern. See
  [DEPLOYMENT-ARCHITECTURE.md](DEPLOYMENT-ARCHITECTURE.md) §8 D7.
- The example configuration mirrors the one written by
  `scripts/install.sh` (both are proxy-only edges since #447; the
  binary owns static serving). The two copies can still drift.
- The tier label for serving-from-packages moved to
  [CONTAINER-VERIFIED] on 2026-10-08: the container package-smoke leg
  (#549) enables `[webui]`, starts the binary behind the packaged
  proxy-only edge in a clean container built from the `.deb`s, and
  asserts the UI is served (`scripts/package/smoke-webui.sh`, wired
  into `package-pr.yml`).

---

## Hosting the Installer (`get.cellhv.com`)

> **Not currently stood up.** `get.cellhv.com` does not resolve (NXDOMAIN).
> This section is a future/optional hosting design, **not** a live install
> command. Use the raw-URL commands in
> [Quick Start](#quick-start-one-liner-install) above today.

If the endpoint were stood up, it would serve `scripts/install.sh` as plain
text so that operators could run
`curl -fsSL https://get.cellhv.com/ | sudo bash -s` (the script still needs
**bash**, not `sh`).

### Option A: GitHub Pages
1. Create a repo `o3kio/get.cellhv.com`
2. Add a `CNAME` file with `get.cellhv.com`
3. Serve `scripts/install.sh` as the index
4. Point DNS to GitHub Pages

### Option B: Cloudflare Worker
```javascript
export default {
  async fetch(request) {
    const installScript = await fetch('https://raw.githubusercontent.com/o3kio/chv/main/scripts/install.sh');
    return new Response(installScript.body, {
      headers: { 'Content-Type': 'text/plain' }
    });
  }
};
```

### Recommended Release Workflow
1. Developer runs `./scripts/build-release.sh`
2. CI uploads `dist/chv-<VERSION>-linux-amd64.tar.gz` to GitHub Releases
3. The installer script at `get.cellhv.com` queries the GitHub API for the latest release tag
4. End user runs `curl -fsSL https://get.cellhv.com/ | sudo bash -s` (once the endpoint is stood up)

---

## Operations & Troubleshooting

### Restart Services
```bash
sudo systemctl restart chv-controlplane
sudo systemctl restart chv-agent
sudo systemctl restart chv-stord
sudo systemctl restart chv-nwd
sudo systemctl restart nginx
```

### View Logs
```bash
sudo journalctl -u chv-controlplane -f
sudo journalctl -u chv-agent -f
sudo journalctl -u chv-stord -f
sudo journalctl -u chv-nwd -f
```

### Post-deploy Web UI smoke test
```bash
./scripts/smoke-webui-auth.sh http://<host-or-ip>
```

### Agent Fails to Enroll
- Ensure `/etc/chv/bootstrap.token` exists and is readable by the `chv` user
- Verify the control plane is listening: `ss -tlnp | grep 8443`
- Check that the token was inserted into `bootstrap_tokens` and has not expired:
  ```bash
  sqlite3 "file:/var/lib/chv/controlplane.db?mode=ro" \
    "SELECT description, expires_at, used_at FROM bootstrap_tokens;"
  ```
- Review agent logs: `journalctl -u chv-agent -n 100`

### `chv-stord` or `chv-nwd` Keep Restarting
- Check daemon logs for missing binary paths or permission errors
- Verify the `chv-stord` and `chv-nwd` binaries are executable (`/usr/bin` for package installs, `/usr/local/bin` for the manual procedure)

### Database Issues
The database is SQLite at `/var/lib/chv/controlplane.db`. It is created automatically by `chv-controlplane` on first start via sqlx migrations.

```bash
# Inspect the database
sqlite3 "file:/var/lib/chv/controlplane.db?mode=ro" .tables
sqlite3 "file:/var/lib/chv/controlplane.db?mode=ro" "SELECT * FROM nodes;"

# Reset to clean state (destructive — removes all data)
sudo systemctl stop chv-controlplane chv-agent
sudo rm /var/lib/chv/controlplane.db
sudo systemctl start chv-controlplane
```

### Bridge / NAT Not Working
```bash
# Verify bridge is up
ip addr show chvbr0

# Verify IP forwarding
sysctl net.ipv4.ip_forward

# Verify NAT rule
iptables -t nat -L POSTROUTING -n -v

# Verify interface is attached to bridge
bridge link show
```

### Web UI Shows JSON Parse Error
This happens when API endpoints return HTML instead of JSON.

**Fix:** Ensure nginx `proxy_pass` does not have a trailing slash:
```nginx
location /v1/ {
    proxy_pass http://127.0.0.1:8080;   # NO trailing slash after port
}
```

### Web UI Blank Page
- Verify `npm run build` succeeded and `index.html` exists in `/opt/chv/ui/`
- Check nginx `root` directive: `sudo nginx -T | grep root`

### Permission Denied on `/dev/kvm`
- Ensure the `chv` user is in the `kvm` group: `groups chv`
- If just added: `sudo systemctl restart chv-agent`

---

## Next Steps

- **Multi-node expansion (code-supported, unqualified):** The enrollment protocol is code-supported, but control-plane dispatch resolves a local Unix-socket path and cannot reach remote nodes. The qualified topology is single-host.
- **External storage:** Configure `chv-stord` backends for shared storage.
- **Networking:** Define tenant bridges and network segments via the Web UI or API.
- **TLS hardening:** Replace the self-signed CA with your organization's PKI.
