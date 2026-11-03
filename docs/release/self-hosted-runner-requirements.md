# Self-Hosted Runner Requirements — KVM Integration Tests

This document describes the infrastructure, security, and operational requirements for the GitHub Actions self-hosted runners that execute CHV KVM integration tests.

## Overview

CHV KVM integration tests require **real hardware virtualization** (`/dev/kvm`), **root privileges**, and **host network access**. They cannot run on standard GitHub-hosted runners. A dedicated, ephemeral or regularly-reset self-hosted runner is required.

## Runner Labels

The workflow `.github/workflows/integration-kvm.yml` targets runners with these labels:

```yaml
runs-on: [self-hosted, linux, x64, chv-kvm]
```

| Label | Purpose |
|-------|---------|
| `self-hosted` | Standard GitHub Actions label |
| `linux` | Linux operating system |
| `x64` | AMD64 architecture |
| `chv-kvm` | Custom label — indicates KVM capability and test-runner isolation |

> **Note:** The exact `chv-kvm` label can be adjusted in the workflow file to match your runner fleet naming convention.

## Operating System

| Requirement | Version / Details |
|-------------|-------------------|
| OS | Debian 12+, Ubuntu 22.04+, or RHEL-compatible 9+ |
| Kernel | 5.15+ with KVM support (`CONFIG_KVM=y`, `CONFIG_KVM_AMD` or `CONFIG_KVM_INTEL`) |
| Init system | systemd (for service unit validation) |
| Architecture | x86_64 (amd64) only |

### Verify kernel support

```bash
# Intel
grep -E 'vmx|kvm' /proc/cpuinfo

# AMD
grep -E 'svm|kvm' /proc/cpuinfo

# Kernel modules
lsmod | grep kvm
```

## KVM Availability

The runner **must** expose `/dev/kvm` to the user executing the workflow.

```bash
ls -la /dev/kvm
# Expected: crw-rw----+ 1 root kvm 10, 232 ...
```

The test user must be in the `kvm` group, or the workflow must run with `sudo`:

```bash
sudo usermod -aG kvm $USER
```

### Nested Virtualization (if runner is a VM)

If the self-hosted runner itself is a virtual machine, nested virtualization must be enabled on the **hypervisor**:

- **KVM/QEMU**: set `host-passthrough` CPU mode or expose `vmx`/`svm` via CPU flags
- **VMware**: enable "Virtualize Intel VT-x/EPT or AMD-V/RVI"
- **AWS**: use instance types with nested virtualization (e.g., metal instances, or enable on supported types)
- **Azure**: enable nested virtualization on Dv3/Ev3 series
- **GCP**: enable nested virtualization on N1/N2 instances

Verify nested virtualization:

```bash
# On the runner VM
cat /sys/module/kvm_intel/parameters/nested  # should print "Y" or "1"
# or
cat /sys/module/kvm_amd/parameters/nested    # should print "1"
```

## `cloud-hypervisor`

The integration test attempts to locate `cloud-hypervisor` at `/usr/bin/cloud-hypervisor`. If the binary is not present, the test script downloads it automatically from GitHub releases.

### Manual installation (recommended for faster tests)

```bash
CH_VERSION="v53.0"
curl -sL "https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/${CH_VERSION}/cloud-hypervisor-static" \
  -o /usr/bin/cloud-hypervisor
chmod +x /usr/bin/cloud-hypervisor
cloud-hypervisor --version
```

> **Pin the version** in your runner image to avoid test flakiness from upstream releases. The test script pins `v53.0` by default (the qualified pin since the #448 re-qualification campaign, 2026-10-03); override it with `--chv-version VER` or the `CHV_CLOUD_HYPERVISOR_VERSION` environment variable. An explicit override is staged in a private temp dir and never writes to `/usr/bin/cloud-hypervisor`, so the runner's qualified pin stays untouched (#458). Do not bump a runner past the qualified version.

## Required Privileges

The workflow job **must** run as `root` or with passwordless `sudo`. The test script uses `sudo` where necessary.

Root is required for:
- Installing `.deb` / `.rpm` packages
- Creating `/run/chv` runtime directories
- Creating transient Linux bridges (network daemon tests)
- Accessing `/dev/kvm` (if not in `kvm` group)
- Binding to privileged ports (if testing on ports < 1024)

## Disk Space

| Use | Minimum | Recommended |
|-----|---------|-------------|
| OS + dependencies | 10 GB | 20 GB |
| Rust build cache (`target/`) | 5 GB | 10 GB |
| CHV runtime state (`/var/lib/chv`) | 2 GB | 5 GB |
| VM images (future) | 5 GB | 20 GB |
| **Total** | **22 GB** | **55 GB** |

## Network Isolation

The integration test creates **transient** network resources:
- Temporary Linux bridges (named `chv-test-*` or `br-chv-test-*`)
- Temporary VXLAN interfaces (if nwd tests run)
- Loopback-only services by default (`127.0.0.1`)

The test **must not**:
- Modify the default route
- Bring down production interfaces
- Reconfigure existing bridges (unless explicitly named `chv-test-*`)

### Firewall considerations

If the runner has strict iptables/nftables rules:
- Allow loopback traffic on test ports (8080, 8443, 9100)
- Allow bridge-local traffic for `chv-test-*` interfaces

## Nightly Qualification Lane

`.github/workflows/nightly-kvm-qualification.yml` uses the same
`[self-hosted, linux, x64, chv-kvm]` labels for the M4.3–M4.8 qualification
scenarios:

| Schedule (UTC) | Scenarios |
|----------------|-----------|
| Daily 02:00 | `m4.7-faults.sh` (fault/interruption matrix) |
| Monday 03:00 | `m4.3-lifecycle`, `m4.4-network`, `m4.5-storage`, `m4.6-migration`, `m4.7-faults`, `m4.8-perf-soak` |

`scripts/integration/qual/nightly-run.sh` calls
`env-preflight.sh --stage-binaries` (release build from a clean checkout,
pinned cloud-hypervisor/firmware/guest image), then `patch-guest-image.sh`
(the stock cloud image cannot boot through the CHV firmware chain; all
M4.3–M4.8 evidence used `noble-qual-patched.img`), and runs each scenario
through `deploy.sh --exec`. It writes `logs/<scenario>.log`, `results.tsv`,
`summary.md`, and copies each `m4.N-artifacts` evidence directory into the
uploaded artifact. `env-preflight.sh` now installs `qemu-utils` for the patch
step, and the driver forwards `STOCK_GUEST_IMAGE` to it as `GUEST_IMAGE` so
preflight downloads exactly the image the patch step consumes.

Evidence hygiene: the driver sets `umask 022`, re-runs `chmod -R a+rX` after
each scenario (so a later hang cannot hide earlier evidence), and fails loudly
if an evidence copy fails. A dedicated upload requires `results.tsv` with
`if-no-files-found: error`; a run with no results marker has no usable
qualification record.

Operational requirements:

- **No repository secrets.** The lane uses loopback-only services and the
  runner's local qualification assets.
- **First run is manual.** The workflow has still not been executed. After
  merge, run the **chaos** lane once via `workflow_dispatch` before trusting
  the schedule; the scheduled lanes remain unverified until that run reports.
- **Offline runner = queued, never green.** There is no hosted-runner fallback
  and no skip-as-pass branch. A scheduled run with no matching runner remains
  queued and visible in the Actions UI.
- **Missing prerequisites = failure.** The workflow checks `/dev/kvm` and
  passwordless sudo before checkout/setup, emits an `::error::` annotation,
  and exits non-zero; the driver does the same for failed staging or scenario
  errors.
- **Disk and time.** The full weekly sequence includes the m4.8 soak and runs
  for hours; keep the disk headroom in the table above and do not run two
  qualification jobs concurrently (the workflow has a concurrency group, and
  the scenario scripts share `/var/lib/chv/qual` and host ports). GitHub keeps
  at most one running and one pending run per group; a newer scheduled run
  replaces an older **queued** run, while the active run is never cancelled.
- **Runner ownership and cargo homes.** The driver runs as root while the
  workspace is owned by the runner user; it sets `safe.directory` for the
  checkout so `env-preflight.sh` can record the candidate SHA. The workflow
  records the runner's `RUSTUP_HOME`/`CARGO_HOME`, passes them through
  `sudo -E`, and `chown`s those homes back to the runner user after the run so
  root staging cannot poison later caches or checkouts. `env-preflight.sh`
  builds into the isolated `CHV_QUAL_ROOT/target` (via `--target-dir`), not
  the workspace `target/`.

## Test User Expectations

The user account running the GitHub Actions runner service should:
1. Have passwordless `sudo` access
2. Be in the `kvm` group
3. Have a writable home directory for cargo cache
4. Not be used for production workloads

## Cleanup Expectations

The integration test script (`scripts/integration/kvm-smoke.sh`) includes comprehensive cleanup:
- Stops all CHV processes it started
- Removes temporary configs, certs, and bridges
- Removes installed packages (preserving `/var/lib/chv` and `/etc/chv` per package contract)
- Collects logs before cleanup for debugging

**You must still reset the runner periodically** because:
- Rust `target/` caches grow unbounded
- Leftover Docker images / containers may accumulate
- Kernel modules or network namespaces may leak

## Resetting a Dirty Runner

If a test run is interrupted (runner killed, network partition, etc.), the runner may be left in a dirty state.

### Quick reset procedure

```bash
#!/bin/bash
# /usr/local/bin/reset-chv-runner.sh
set -e

echo "=== Stopping any running CHV services ==="
systemctl stop chv-controlplane chv-agent chv-stord chv-nwd 2>/dev/null || true
systemctl disable chv-controlplane chv-agent chv-stord chv-nwd 2>/dev/null || true

echo "=== Killing any leftover CHV processes ==="
pkill -f 'chv-controlplane|chv-agent|chv-stord|chv-nwd|cloud-hypervisor' 2>/dev/null || true
sleep 2

echo "=== Removing temporary test resources ==="
ip link show | grep -E 'chv-test-|br-chv-test-' | awk -F: '{print $2}' | while read iface; do
    ip link delete "$iface" 2>/dev/null || true
done

echo "=== Removing test packages (preserving data) ==="
dpkg -r chv-node chv-controlplane chvctl 2>/dev/null || true
rpm -e chv-node chv-controlplane chvctl 2>/dev/null || true

echo "=== Cleaning temp directories ==="
rm -rf /tmp/chv-kvm-test-*

echo "=== Runner reset complete ==="
```

### Full reset (recommended weekly or after failed runs)

```bash
# Stop the GitHub Actions runner service
sudo systemctl stop actions.runner.*

# Run the quick reset above
sudo /usr/local/bin/reset-chv-runner.sh

# Clean Rust build cache
rm -rf ~runner/actions-runner/_work/chv/chv/target/tmp

# Restart runner
sudo systemctl start actions.runner.*
```

## Runner Provisioning Checklist

Use this checklist when setting up a new self-hosted runner for CHV integration tests:

- [ ] OS installed (Debian 12+ / Ubuntu 22.04+ / RHEL 9+)
- [ ] `/dev/kvm` exists and is accessible
- [ ] Nested virtualization enabled (if VM)
- [ ] `kvm` group exists; runner user is a member
- [ ] Passwordless `sudo` configured
- [ ] GitHub Actions runner service installed and registered
- [ ] Runner has label `self-hosted,linux,x64,chv-kvm`
- [ ] `cloud-hypervisor` installed at `/usr/bin/cloud-hypervisor`
- [ ] Rust toolchain installed (`rustup`)
- [ ] `protoc` installed
- [ ] Node.js 22.17+ + npm installed
- [ ] Docker installed (optional, for container smoke tests)
- [ ] `openssl` installed (for test cert generation)
- [ ] `jq` installed (for JSON parsing in tests)
- [ ] `curl` installed
- [ ] Disk space >= 30 GB free
- [ ] `/var/lib/chv` directory can be created
- [ ] Reset script installed at `/usr/local/bin/reset-chv-runner.sh`
- [ ] Cron job or systemd timer for periodic reset configured

## Security Hardening

Because the runner executes arbitrary code from PRs (with the `kvm-test` label gate), follow these practices:

1. **Network segmentation**: Place the runner in an isolated VLAN with no access to production infrastructure.
2. **Ephemeral runners**: Use ephemeral runners (destroy after each job) if possible.
3. **Label gating**: Require manual PR label `kvm-test` before running KVM tests. Do not auto-run on every PR.
4. **No secrets**: The KVM workflow does not require repository secrets. Do not mount `GITHUB_TOKEN` or other credentials unless absolutely necessary.
5. **Resource limits**: Use cgroups or systemd limits to prevent runaway VMs from consuming all host resources.
6. **Audit logging**: Log all commands run by the test script to `/var/log/chv-kvm-test/`.

## Troubleshooting

### Test fails with "No /dev/kvm"

- Verify KVM module is loaded: `lsmod | grep kvm`
- Check BIOS virtualization settings (Intel VT-x / AMD-V)
- If nested: verify host exposes CPU flags to guest

### Test fails with "cloud-hypervisor not found"

- Install manually or let the test script auto-download
- Check architecture mismatch (e.g., ARM binary on x86)

### Test fails with permission denied on `/run/chv`

- The test script creates `/run/chv` automatically
- Ensure the runner user can write to `/run`

### Services fail to start

- Check logs collected by the test script in `/tmp/chv-kvm-test-*/logs/`
- Verify no existing CHV services are running: `systemctl status chv-*`
- Verify port conflicts: `ss -tlnp | grep -E '8080|8443|9100'`

## References

- [GitHub Docs — Self-hosted runners](https://docs.github.com/en/actions/hosting-your-own-runners)
- [cloud-hypervisor releases](https://github.com/cloud-hypervisor/cloud-hypervisor/releases)
- [CHV Package Contract](package-contract.md)
- [CHV Local Release Commands](local-release-commands.md)
