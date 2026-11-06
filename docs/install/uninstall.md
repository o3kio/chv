# Uninstall CHV

This guide explains how to safely remove CHV packages while preserving or deleting data.

## Standard removal (data preserved)

The default package removal intentionally preserves all persistent state. This prevents accidental data loss.

### Debian / Ubuntu

```bash
sudo dpkg -r chv-node chv-controlplane chvctl
sudo apt-get autoremove
```

### RHEL / Rocky / AlmaLinux

```bash
sudo rpm -e chv-node chv-controlplane chvctl
```

### What is preserved

After standard removal, the following remain on disk:

| Path | Contents | Preserved? |
|------|----------|------------|
| `/var/lib/chv` | SQLite database, agent cache, storage volumes, images | ✅ Yes |
| `/etc/chv` | Configuration files (`*.toml`, certs, bootstrap token) | ✅ Yes |
| `/var/log/chv` | Service logs | ✅ Yes |
| `chv` user and group | System user | ✅ Yes |
| `chv-stord` user and group | Reserved system user | ✅ Yes |

### What is removed

Package removal stops the four services, so any running VMs are stopped
(with their CHV-owned cloud-hypervisor processes terminated) before the
software goes away. Persistent data is preserved and a reinstall resumes
from it.

| Path | Contents | Removed? |
|------|----------|----------|
| `/usr/bin/chv*` | Binaries | ✅ Yes |
| `/lib/systemd/system/chv-*.service` | Systemd units | ✅ Yes |
| `/usr/share/chv` | UI assets, migrations | ✅ Yes |
| `/usr/lib/tmpfiles.d/chv-node.conf` | tmpfiles entry (`chv-node` package) | ✅ Yes |
| `/opt/chv`, `/usr/local/share/chv/migrations`, nginx site | `scripts/install.sh` UI tree, migrations, and edge config (when present) | ✅ Yes |
| Host network state | Per-network bridges, namespaces, CHV tap slaves, and `inet chv-*` nftables tables (including the literal `default` network; plus the baseline `chvbr0` bridge), the live and persisted CHV `MASQUERADE` rules, `99-chv-forward.conf`, and the networkd/ifupdown bridge config | ✅ Yes (remove/purge, not upgrade) |

## Verify removal

```bash
# Binaries should be gone
which chvctl          # should return nothing
which chv-agent       # should return nothing

# Data should still exist
ls -la /var/lib/chv   # should show contents
ls -la /etc/chv       # should show contents

# User should still exist
getent passwd chv     # should show chv user
getent group chv      # should show chv group
```

## Reinstall after removal

Because data is preserved, you can reinstall CHV and resume operation:

```bash
# Debian/Ubuntu
sudo dpkg -i chv-controlplane_<version>_amd64.deb chv-node_<version>_amd64.deb chvctl_<version>_amd64.deb

# RHEL/Rocky/Alma
sudo rpm -i chv-controlplane-<version>-1.x86_64.rpm chv-node-<version>-1.x86_64.rpm chvctl-<version>-1.x86_64.rpm

# Start services
sudo systemctl daemon-reload
sudo systemctl enable --now chv-controlplane chv-agent chv-stord chv-nwd
```

Your previous database, caches, and configs will be intact.

## Complete removal (destructive)

> **⚠️ WARNING:** This procedure permanently destroys all CHV data. Back up anything you need before proceeding.

### 1. Stop services

```bash
sudo systemctl stop chv-controlplane chv-agent chv-stord chv-nwd
```

### 2. Remove packages

```bash
# Debian/Ubuntu
sudo dpkg -r chv-node chv-controlplane chvctl
sudo apt-get autoremove

# RHEL/Rocky/Alma
sudo rpm -e chv-node chv-controlplane chvctl
```

### 3. Delete persistent data

```bash
sudo rm -rf /var/lib/chv
sudo rm -rf /etc/chv
sudo rm -rf /var/log/chv
```

### 4. Remove users and groups

```bash
sudo userdel chv
sudo groupdel chv
sudo userdel chv-stord
sudo groupdel chv-stord
```

### 5. Clean up runtime directories

```bash
sudo rm -rf /run/chv
```

### 6. Remove systemd state (if any lingering units)

```bash
sudo systemctl daemon-reload
sudo systemctl reset-failed
```

### 7. Verify no host network state remains

Package removal tears down the bridges, namespaces, tap interfaces, and
`inet chv-*` nftables tables nwd created. Check that nothing CHV-owned is
left (other host bridges/namespaces are not CHV's):

```bash
sudo nft list tables | grep 'inet chv-' || echo "no CHV nftables tables"
sudo ip netns list | grep -E '^ns-([0-9a-f]+|default)( |$)' || echo "no CHV namespaces"
ip -o link show type bridge | grep -E 'chvbr0|br-[A-Za-z0-9_.-]+' || echo "no CHV bridges"
```

The default network id is the literal `default` (namespace `ns-default`,
nftables table `chv-default`, bridge `chvbr0`); operator-created network ids
use the same `ns-<id>`/`chv-<id>`/`br-<id>` naming. On a host upgraded from an
older release, an orphaned table/bridge from that older layout may still be
present. Remove it explicitly once: `nft delete table inet chv-<id>`,
`ip netns del ns-<id>`, `ip link del dev br-<id>` (and
`ip link del dev chvbr0`).

Removal also deletes any networkd `12-*.network` drop-in that enslaved the
uplink to a CHV bridge, because leaving it would recreate the bridge at the
next boot. That drop-in is also what configured the uplink. Before rebooting,
restore or verify the uplink's own network configuration (for example
`networkctl status <uplink>` plus a `10-<uplink>.network`/DHCP stanza) so the
host does not come back with an unconfigured NIC.

## What to back up before removal

If you plan to reinstall or migrate CHV, back up these paths before removal:

```bash
# Back up everything
sudo tar -czf chv-backup-$(date +%Y%m%d).tar.gz /var/lib/chv /etc/chv /var/log/chv
```

Critical files to back up:
- `/var/lib/chv/controlplane.db` — control plane database
- `/var/lib/chv/agent-cache.json` — agent enrollment state
- `/var/lib/chv/storage/` — volume data and images
- `/etc/chv/certs/` — TLS certificates
- `/etc/chv/encryption.env` — credential encryption key (required to decrypt stored S3 credentials)
- `/etc/chv/bootstrap.token` — enrollment token (if still needed)

## Troubleshooting

### `userdel: user chv is currently used by process ...`

If the `chv` user is still referenced by running processes:

```bash
# Find and stop any lingering processes
sudo ps aux | grep chv
sudo kill -9 <pid>

# Retry user deletion
sudo userdel chv
```

### Files remain after `rm -rf`

If files in `/var/lib/chv` are locked by a kernel module or container:

```bash
# Check for open file handles
sudo lsof +D /var/lib/chv

# Unmount any bind mounts
sudo umount /var/lib/chv/* 2>/dev/null || true
```

## See also

- [Debian / Ubuntu Install](debian-ubuntu.md)
- [RHEL / Rocky / AlmaLinux Install](rhel-rocky-alma.md)
- [Package Contract](../release/package-contract.md) — formal rules for package behavior
