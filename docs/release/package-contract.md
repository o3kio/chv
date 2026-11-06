# CHV Package Contract

This document defines the exact contract for CHV system packages: what they install, own, configure, start, and remove. It is the reference for packaging scripts, CI jobs, and host administrators.

## Binary Inventory

The following binaries are built from this repository and included in packages:

| Binary | Crate | Type | Package |
|--------|-------|------|---------|
| `chvctl` | `cmd/chvctl` | CLI client | `chvctl` |
| `chv-controlplane` | `cmd/chv-controlplane` | Daemon | `chv-controlplane` |
| `chv-agent` | `cmd/chv-agent` | Daemon | `chv-node` |
| `chv-stord` | `cmd/chv-stord` | Daemon | `chv-node` |
| `chv-nwd` | `cmd/chv-nwd` | Daemon | `chv-node` |
| `chv-monitor-agent` | `cmd/chv-monitor-agent` | Guest daemon | `chv-monitor-agent` |

The `chv-monitor-agent` package is a **guest** package (ADR-026): it
installs inside VMs, never on CHV hosts, and has no dependency on (and
is never pulled in by) the three host packages. Its contract lives in
[`docs/PACKAGING.md`](../PACKAGING.md#chv-monitor-agent) and
`docs/specs/adr/026-optional-monitor-agent-and-guest-identity.md`.

## Package Split

### `chvctl`

**Purpose:** Standalone CLI client. Can be installed on any machine that needs to talk to a CHV control plane.

**Systemd services:** None.

**Host initialization:** None.

**Contents:**

```text
/usr/bin/chvctl
```

**Depends:** `libssl3` (`.deb`), `openssl-libs` (`.rpm`). The binary links against OpenSSL 3 via `native-tls`.

**Scripts:** None.

---

## Maintainer Scripts

All CHV packages share a set of generic, safe maintainer scripts under `packaging/scripts/`:

| Script | Purpose | Safety Notes |
|--------|---------|--------------|
| `postinstall.sh` | Creates `chv` user/group, `/var/lib/chv`, `/var/log/chv`, `/run/chv`, adds user to `kvm`, runs `daemon-reload` | Never creates bridges, initializes storage, or starts VMs |
| `preremove.sh` | Stops all known CHV services on remove; skips on upgrade; runs `daemon-reload` | Never removes persistent data |
| `postremove.sh` | Runs `daemon-reload` only | Never removes persistent data or the `chv` user |

---

### `chv-controlplane`

**Purpose:** Control-plane service, Web UI, database migrations, and the administrative HTTP/gRPC API.

**Systemd services:** `chv-controlplane.service`

**Host initialization:**
- Creates `chv` system user and group if absent.
- Creates `/var/lib/chv` with `chv:chv` ownership.
- Runs `systemctl daemon-reload`.

**Contents:**

```text
/usr/bin/chv-controlplane
/usr/share/chv/ui/                  # built Svelte SPA assets (type: tree)
/usr/share/chv/migrations/          # SQLite schema migrations (type: tree)
/usr/share/chv/examples/chv-example.conf   # example reverse-proxy config (D3 interim)
/lib/systemd/system/chv-controlplane.service
/etc/chv/controlplane.toml          # type: config|noreplace
```

The example nginx configuration documents how an operator can serve the
packaged UI tree. The package does not enable it and installs no web
server; serving the UI this way is
[CODE-SUPPORTED, UNQUALIFIED]. See `docs/DEPLOYMENT.md` "Serving the
Web UI in package mode".

**Depends:** `libssl3` (`.deb`), `openssl-libs` (`.rpm`). The binary links against OpenSSL 3 via `native-tls`.

**Scripts:**
- `postinstall`: `packaging/scripts/postinstall.sh`
- `preremove`: `packaging/scripts/preremove.sh`
- `postremove`: `packaging/scripts/postremove.sh`

---

### `chv-node`

**Purpose:** Node services. Installs `chv-agent`, `chv-stord`, and `chv-nwd`. `chvctl` is not included; install the standalone `chvctl` package where you need the CLI on a node.

**Systemd services:**
- `chv-agent.service`
- `chv-stord.service`
- `chv-nwd.service`

**Host initialization:**
- Creates `chv` system user and group if absent.
- Creates `/var/lib/chv`, `/var/log/chv`, `/run/chv` with `chv:chv` ownership.
- Adds `chv` user to the `kvm` group if it exists.
- Runs `systemctl daemon-reload`.

**Contents:**

```text
/usr/bin/chv-agent
/usr/bin/chv-stord
/usr/bin/chv-nwd
/lib/systemd/system/chv-agent.service
/lib/systemd/system/chv-stord.service
/lib/systemd/system/chv-nwd.service
/usr/lib/tmpfiles.d/chv-node.conf    # creates /run/chv at boot
/etc/chv/agent.toml                 # type: config|noreplace
/etc/chv/stord.toml                 # type: config|noreplace
/etc/chv/nwd.toml                   # type: config|noreplace
/etc/chv/chv.yaml                   # reference config; type: config|noreplace
```

**Depends:** `chv-controlplane`, plus `wireguard-tools`. On `.deb`, `wireguard-tools` is a hard dependency. On `.rpm`, it is a `recommends` (it is EPEL-only on EL distributions, and a hard requirement would break installs without EPEL).

> **Note:** `chvctl` is not included in `chv-node` to avoid file conflicts. Operators who want the CLI on a node should install `chvctl` separately.

> **Rationale:** The current agent unit file declares `Wants=chv-controlplane.service`. For single-node deployments this ensures ordering; for multi-node deployments operators may override this with drop-ins.

**Scripts:**
- `postinstall`: `packaging/scripts/postinstall.sh`
- `preremove`: `packaging/scripts/preremove.sh`
- `postremove`: `packaging/scripts/postremove.sh`

---

### `chv` (Meta-package)

**Purpose:** Convenience umbrella that installs the full single-node stack.

**Status:** Not yet implemented. Planned for a future release.

**Proposed contents:**

```text
# No files of its own; only dependencies
```

**Proposed depends:** `chv-controlplane`, `chv-node`

---

## Filesystem Ownership Map

| Path | Owner | Created by | Package | Notes |
|------|-------|------------|---------|-------|
| `/usr/bin/chvctl` | `root:root` | install | `chvctl` | Executable |
| `/usr/bin/chv-controlplane` | `root:root` | install | `chv-controlplane` | Executable |
| `/usr/bin/chv-agent` | `root:root` | install | `chv-node` | Executable |
| `/usr/bin/chv-stord` | `root:root` | install | `chv-node` | Executable |
| `/usr/bin/chv-nwd` | `root:root` | install | `chv-node` | Executable |
| `/usr/share/chv/ui/*` | `root:root` | install | `chv-controlplane` | Static SPA files |
| `/usr/share/chv/examples/chv-example.conf` | `root:root` | install | `chv-controlplane` | Example reverse-proxy config for the packaged UI tree; not enabled ([CODE-SUPPORTED, UNQUALIFIED]) |
| `/usr/share/chv/migrations/*` | `root:root` | install | `chv-controlplane` | SQL migrations |
| `/lib/systemd/system/chv-*.service` | `root:root` | install | respective package | Systemd units |
| `/etc/chv/controlplane.toml` | `root:root` | install | `chv-controlplane` | Config file; `config|noreplace` |
| `/etc/chv/agent.toml` | `root:root` | install | `chv-node` | Config file; `config|noreplace` |
| `/etc/chv/stord.toml` | `root:root` | install | `chv-node` | Config file; `config|noreplace` |
| `/etc/chv/nwd.toml` | `root:root` | install | `chv-node` | Config file; `config|noreplace` |
| `/etc/chv/chv.yaml` | `root:root` | install | `chv-node` | Reference config; `config|noreplace` |
| `/usr/lib/tmpfiles.d/chv-node.conf` | `root:root` | install | `chv-node` | Creates `/run/chv` at boot |
| `/var/lib/chv` | `chv:chv` | postinstall | any CHV package | Persistent state directory |
| `/var/log/chv` | `chv:chv` | postinstall | `chv-node` | Log directory |
| `/run/chv` | `chv:chv` | postinstall / runtime | `chv-node` | Runtime sockets, PID files |

## Directory Lifecycle Rules

### Created on install

- `/var/lib/chv` — persistent state (databases, caches, enrollment data)
- `/var/log/chv` — daemon logs
- `/run/chv` — runtime sockets, PID files, temporary TLS certs

### Preserved on remove

- `/var/lib/chv` — **NEVER deleted by package removal.** Contains enrolled node identity, SQLite databases, and VM state. Destructive cleanup requires explicit operator action.
- `/etc/chv/` — **NEVER deleted by package removal.** Config files marked `config|noreplace` are owned by the package manager. On Debian `purge` the operator may opt to delete them. Standard `remove` preserves them.
- `/var/log/chv` — preserved on remove; may be rotated by the OS.

### Removed on purge (optional future behavior)

If a future package implements purge semantics:
- `/var/lib/chv` may be deleted **only** after explicit confirmation or a separate `--purge` flag.
- `/etc/chv/` may be deleted on purge.
- `/var/log/chv` may be deleted on purge.

## Config File Treatment

All `.toml` files under `/etc/chv/` are declared with `type: config|noreplace` in the nfpm template. This means:

1. On **first install**, the example config is written to `/etc/chv/`.
2. On **upgrade**, existing config files are **never overwritten**.
3. New config keys or sections must be documented in release notes; operators merge changes manually.
4. Packaged example configs contain placeholder values (`<replace-me>`) that must be edited before services start.

## Upgrade Behavior

1. **Pre-upgrade (`preremove`):**
   - Detect upgrade via package-manager argument (`$1 = upgrade` / `1`).
   - **Do NOT stop services.** Services continue running during the package file swap.
   - Do NOT touch `/var/lib/chv`.

2. **File replacement:**
   - Binaries are replaced atomically by the package manager.
   - Config files are preserved (`config|noreplace`).
   - Systemd units are replaced; `daemon-reload` is triggered in postinstall.

3. **Post-upgrade (`postinstall`):**
   - Ensure `chv` user/group still exist.
   - Ensure directory ownership is correct.
   - Run `systemctl daemon-reload`.
   - **Do NOT restart services automatically.** The operator decides when to roll over to the new binaries.

## Remove Behavior

1. **Pre-remove (`preremove`):**
   - Detect remove vs upgrade.
   - On remove (not upgrade): stop relevant services gracefully.
   - Run `systemctl daemon-reload`.

2. **File removal:**
   - Binaries, systemd units, and static assets are removed by the package manager.
   - Config files in `/etc/chv/` are preserved (standard `remove`).

3. **Post-remove:**
   - No destructive cleanup is performed.
   - `/var/lib/chv` is left intact.
   - The `chv` system user and group are **not** removed automatically (to avoid UID/GID reuse issues).

## Runtime State Safety

The following directories contain persistent or sensitive state and must survive package operations:

- `/var/lib/chv/controlplane.db` — control-plane SQLite database
- `/var/lib/chv/agent-cache.json` — agent enrollment and desired-state cache
- `/var/lib/chv/storage/` — volume data and image files
- `/etc/chv/certs/` — TLS certificates and CA material
- `/etc/chv/bootstrap.token` — one-time enrollment token

**Policy:**
- Package remove must not delete `/var/lib/chv`.
- Package upgrade must not rewrite `/var/lib/chv`.
- Destructive cleanup requires explicit user action, for example:
  ```bash
  sudo rm -rf /var/lib/chv /etc/chv
  sudo userdel chv
  sudo groupdel chv
  ```

## Service Behavior

### Installation

- Systemd unit files are installed to `/lib/systemd/system/`.
- `systemctl daemon-reload` is executed in postinstall.

### Enablement

- Services are **installed but not automatically enabled or started**.
- The operator must explicitly enable them:
  ```bash
  sudo systemctl enable --now chv-controlplane
  sudo systemctl enable --now chv-agent chv-stord chv-nwd
  ```

### Rationale

Enterprise deployments often require config editing (`jwt_secret`, `control_plane_addr`, TLS certs) before services can start. Auto-starting a service with default/example config produces immediate failure loops. The postinstall scripts create the runtime environment; the operator brings services online.

### Security Context

All daemons run as the `chv` system user:

- `chv-agent`: supplementary group `kvm`, device access `/dev/kvm rw`
- `chv-stord`: read-write to `/var/lib/chv/storage`, `/run/chv/stord`
- `chv-nwd`: capabilities `CAP_NET_ADMIN CAP_NET_RAW CAP_SYS_ADMIN
  CAP_NET_BIND_SERVICE` (the first three cover topology/nft/netns work —
  `CAP_SYS_ADMIN` is required for `ip netns add` and the ProtectSystem
  tradeoff is documented on the unit, #328; `CAP_NET_BIND_SERVICE` lets the
  dnsmasq child bind UDP/67, B8), address families restricted to
  `AF_UNIX AF_INET AF_INET6 AF_NETLINK`, read-write to `/run/chv/nwd`,
  `/run/netns`, and `/var/lib/chv` (the durable topology journal and DHCP
  reservations live under `/var/lib/chv/nwd`)
- `chv-controlplane`: read-write to `/var/lib/chv`, `/run/chv/controlplane`; read-only to `/usr/share/chv`

All units use:
- `NoNewPrivileges=true`
- `ProtectSystem=strict`
- `ProtectHome=true`

## Smoke Tests

Container-based smoke tests verify install/remove/reinstall behavior for every package format:

| Script | Images | What it tests |
|--------|--------|---------------|
| `scripts/package/smoke-deb.sh` | `debian:12`, `ubuntu:24.04` | `.deb` install, binary/config/unit presence, version output, remove, data preservation, reinstall |
| `scripts/package/smoke-rpm.sh` | `rockylinux:9` | `.rpm` install, binary/config/unit presence, version output, remove, data preservation, reinstall |

Smoke tests run in the PR workflow (`package-pr.yml`) after package generation. They do not require KVM or a running systemd.

## Lifecycle Tests

Full lifecycle tests verify enterprise-grade package safety across install, upgrade, remove, and reinstall:

| Script | Images | What it tests |
|--------|--------|---------------|
| `scripts/package/lifecycle-deb.sh` | `debian:12`, `ubuntu:24.04` | Fresh install, sentinel state creation, reinstall same version, upgrade old→new, remove with data preservation, reinstall after remove |
| `scripts/package/lifecycle-rpm.sh` | `rockylinux:9` | Same scenarios for RPM |

Lifecycle tests use **sentinel files** to prove safety:
- `/var/lib/chv/test-persistent-state-sentinel` — must survive remove and upgrade
- `/etc/chv/test-config-sentinel` — must survive remove and upgrade
- A marker line added to `/etc/chv/controlplane.toml` — must survive upgrade (`config|noreplace`)

Lifecycle tests run in:
- Nightly workflow (`package-nightly.yml`)
- Release workflow (`release.yml`) for RC and stable tags

They are intentionally **not** run on every PR because they require building two package versions and running multiple container scenarios.

## Rollback and Downgrade

### What works today

- **Reinstall same version:** `dpkg -i` (with `--force-reinstall`) or `rpm -U --replacepkgs` reinstalls binaries and units without touching `/var/lib/chv` or `/etc/chv`.
- **Remove + reinstall:** Packages can be removed and reinstalled. Persistent data and configs are preserved.
- **Upgrade forward:** Standard upgrade from old version to new version preserves data and configs.

### What is unsupported

- **Downgrade to older version:** Not tested. Package managers may refuse or require `--force` flags. Do not downgrade in production without a backup.
- **Rollback after config schema change:** If a new version introduces a required config field, downgrading to an older binary that does not recognize it may cause startup failures.
- **Database migration rollback:** The control plane runs SQLite migrations forward only. There is no automatic down-migration.

### Manual rollback procedure

If an upgrade fails and you need to revert:

1. **Stop all CHV services:**
   ```bash
   sudo systemctl stop chv-controlplane chv-agent chv-stord chv-nwd
   ```

2. **Back up current state (if not already done):**
   ```bash
   sudo cp -a /var/lib/chv /var/lib/chv.backup
   sudo cp -a /etc/chv /etc/chv.backup
   ```

3. **Remove the new packages:**
   ```bash
   # Debian/Ubuntu
   sudo dpkg -r chv-node chv-controlplane chvctl

   # RHEL/CentOS/Fedora
   sudo rpm -e chv-node chv-controlplane chvctl
   ```

4. **Install the previous version packages** (download from the previous GitHub release).

5. **Restore configs if needed:**
   ```bash
   sudo cp -a /etc/chv.backup/* /etc/chv/
   ```

6. **Restart services:**
   ```bash
   sudo systemctl daemon-reload
   sudo systemctl start chv-controlplane chv-agent chv-stord chv-nwd
   ```

> **Warning:** This is a manual procedure. Automated rollback support is a future release engineering goal.

## Gaps and Future Work

| Gap | Impact | Plan |
|-----|--------|------|
| `chv` meta-package not implemented | Operators must install `chv-controlplane` + `chv-node` separately | Implement when single-command install is a priority |
| No purge script | `/var/lib/chv` and `/etc/chv` remain after `apt purge` / `rpm -e` | Add `postrm` / `%postun` purge logic in a future release |
| `chv-node` depends on `chv-controlplane` | Multi-node deployments install the control plane on every node | Revisit after node-to-control-plane topology is configurable at package level |
| No logrotate config | Logs in `/var/log/chv` may grow unbounded | Add `packaging/logrotate/chv` in a future release |
| No SELinux policy | `chv-nwd` with `CAP_NET_ADMIN` may trip MLS/MCS policies | Document or provide policy module if requested by enterprise users |
