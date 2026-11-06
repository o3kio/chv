# 00 — Approved profile and field host record

**Campaign:** `v0.4.0` field qualification (Prompt 06)
**Host label:** `field-host-1` (internal hostname and management addresses withheld)
**Recorded:** 2026-11-06 (UTC)
**Status:** pre-flight, **blocked**. A clean `v0.4.0-rc.6` install reproduced
blocker-class defects on 2026-11-06. rc.6 is disqualified as the field/stable
base, and `v0.4.0-rc.7` is the approved successor and is being cut. See
[`01-rc6-blockers.md`](01-rc6-blockers.md). The field window has not started.
This profile is re-pinned to rc.7 below, but the artifact digests and the
clean-install re-run are pending publication; keep this status blocked until
the rc.7 pre-window drills pass. No field-window evidence is claimed before
that point.

## Approved profile

This run uses the narrow profile in
[`docs/plans/2026-11-05-v0.4.0-field-qualification.md`](../../../../plans/2026-11-05-v0.4.0-field-qualification.md)
§5 and [`docs/DEPLOYMENT-ARCHITECTURE.md`](../../../../DEPLOYMENT-ARCHITECTURE.md)
§5 UC-11:

| Dimension | Recorded value |
|---|---|
| Topology | one host running control plane, agent, stord, nwd, and nginx |
| Host | Ubuntu 24.04.5 LTS, amd64, **virtualized KVM guest** (QEMU/SeaBIOS). Not bare metal. |
| VMM | Cloud Hypervisor v53.0, installed and digest-verified by the published installer |
| Artifacts | **approved target:** published `v0.4.0-rc.7` `.deb` set and tarball; the clean install uses the published tarball's `scripts/install.sh` (release-directory mode). The pinned quick installer is a container-verified path and is not used for this window. Digests **pending publication** and must be recorded here before the window |
| Storage | local directory + LVM on the dedicated blank `/dev/sdc` (100 GiB) |
| Network | default single-host bridge only; no VLAN, no fabric/VXLAN |
| Lifecycle | Core-managed authority; stop-first |
| Workload | 3–5 VMs; repeated create/delete; start/stop/reboot; console; one policy change; LVM attach/use |
| Upgrade | `v0.3.0` → `v0.4.0-rc.7` (or `v0.4.0-rc.6` → `v0.4.0-rc.7`) before the window; an rc.6/older source host needs the one-time legacy `chvbr0` removal documented in the rc.7 release notes §4; the window starts from a clean rc.7 install |
| Data | disposable qualification data only; no backup means no data-protection claim |
| Duration | 21 days minimum, 30 preferred |

The profile does **not** claim multi-host, HA, control-plane failover,
migration, backup/restore, scale/density/throughput, RPM, arm64, hardware
minimums, S3/object storage, or Volvisor.

### Disclosed residual: M2.5 delete retention

`vm delete` removes the authority-side state, but the BFF/CP rows, the VM
directory, the volume row, and the backing file persist after the delete
returns success. The truthful delete signal is the task/operation result.
This is a **pre-existing disclosed residual**, not part of the rc.6/rc.7
fixes, and it is acceptable for this campaign only because it is documented
with that workaround (field plan §6.9). It does not involve isolation, data
corruption, authority split, or recovery, so it is not blocker-class.

## Host record

| Item | Value |
|---|---|
| Virtualization | `systemd-detect-virt` = `kvm`; DMI `QEMU` / `Standard PC (i440FX + PIIX, 1996)` / SeaBIOS `rel-1.17.0-0-gb52ca86e094d-prebuilt.qemu.org` |
| CPU | AMD EPYC 9554P 64-Core Processor, 16 vCPU visible; `svm`, `aes`, `avx2`, `avx512f` |
| Nested virtualization | `kvm_amd.nested=1` |
| `/dev/kvm` | present (`crw-rw---- root:kvm 10,232`) |
| Memory | 31 GiB total, 0 B swap |
| Root disk | `/dev/sda1` ext4, 527.5 GiB, 158 GiB free at recording |
| Field LVM disk | `/dev/sdc`, 100 GiB, blank (no signatures, not mounted, no holders) |
| Other disk | `/dev/sdb` ext4 mounted at `/var/lib/libvirt/images` (not used by CHV) |
| Kernel | `6.8.0-142-generic` |
| OS image digest | **not available**: the host is a pre-installed cloud VM and no base-image artifact was supplied. This is recorded as a pre-flight limitation, not fabricated. |
| Network | one management interface (`eth0`, addresses withheld); default single-host CHV bridge; nginx edge |
| Existing CHV state | cleared before this run: no `chv-*` packages, no installed units, no `/opt/chv`, `/etc/chv`, `/var/lib/chv`, or `/run/chv`. The earlier legacy residue (`chv.yaml`, `jwt_secret`, `/var/lib/chv/qual`) was archived to `/root/chv-field/preserved/` and purged. |
| nftables baseline | 7 pre-existing tables (`ip`/`ip6` nat/filter/mangle/raw) from the base image |

A machine-readable snapshot is in
[`artifacts/00-host-snapshot.txt`](artifacts/00-host-snapshot.txt); the
residue purge transcript is in
[`artifacts/01-legacy-residue-purge.txt`](artifacts/01-legacy-residue-purge.txt).

## Artifact identity

### Target: rc.7 (pending publication)

| Artifact | Digest / identity |
|---|---|
| rc.7 tag | **pending** — record the annotated tag object and commit after publication |
| rc.7 tarball + sidecar | **pending** |
| rc.7 `SHA256SUMS` and package set | **pending** |
| rc.7 provenance | **pending** — verify the SLSA attestation and record the run link |

Do not fill these values from a local build. The field window uses the
published artifacts only.

### Prior artifact: rc.6 (disqualified, historical)

| Artifact | Digest / identity |
|---|---|
| rc.6 tag | annotated tag object `70a362a33cab10ae331eb602d0d34e3cf5eaba41`, commit `df86e53935b8d2ccacc1908ffa5e4b9c472911b7` |
| rc.6 tarball | `chv-0.4.0-rc.6-linux-amd64.tar.gz` = `fa999d64d43e0cfb018b9c9b41edbf35888ce5181680584f201bb9fd403e4906` |
| rc.6 tarball sidecar | `chv-0.4.0-rc.6-linux-amd64.tar.gz.sha256` = `dd17c27ab2cac8f16296e4d4dfb9bb1191cf4b71a4d3cb3c41c19481b6572cfe` |
| rc.6 `SHA256SUMS` | `c5e74b201742bf5caa8d99e03a1307697b5cd1dea15c106210275094ea12985c` (8 package lines) |
| rc.6 control plane deb | `854caf395f1f405c288de5bdc40464da20d8d033ed68cf6aad5388131099db7f` |
| rc.6 monitor-agent deb | `bc3818b283dd94008a51130d2f9e45c8e42a884d362678d7ec1ea0cb69f8216b` |
| rc.6 node deb | `c676460080a51ffa3e46dbad3a8c47a306410e1ef08986a447df8d0aaf7e349a` |
| rc.6 chvctl deb | `0ff97a53fbcc01a81cd32640edc1cbcf5079faf6b02e636e6deaaa739f99435c` |
| rc.6 control plane rpm | `3e4379c15c9a0fbde48a6aa5c66fd85343f1c36e96d9b5b8d5e9ea3fbe91f65a` |
| rc.6 monitor-agent rpm | `3616b6931e741b9243f145b6b36a78f4bd5d0fe3b5a7e6bc0c3365db7e80c677` |
| rc.6 node rpm | `14ca55a4bcf5ac697f5ac608f7287a72f4048c2836dad0ffaa59f5450a69b425` |
| rc.6 chvctl rpm | `e25ffed37e04910c916e3ccead0ab9458ea0a542e61a87eff0495679dc93f3c0` |
| v0.3.0 tarball (upgrade start) | `chv-0.3.0-linux-amd64.tar.gz` = `45990c38f9ba0a1ca573e0c7549479464ff6a0eb763da6ecec4bf1eb376eea0d` |

The rc.6 tarball sidecar and all eight `.deb`/`.rpm` assets were verified
against the published `SHA256SUMS`. The extracted binaries report
`0.4.0 (commit df86e53, build 2026-11-06, channel stable)`; the package
version is `0.4.0~rc.6` and the published asset version is `0.4.0.rc.6`.
The node row reports the base `0.4.0`, so rc.6 is discriminated by the
package version and the commit banner, not by the node row.

### Build provenance (rc.6, historical)

The release workflow
([42228721214](https://github.com/o3kio/chv/actions/runs/42228721214))
generated a SLSA v1 build-provenance attestation with
`actions/attest-build-provenance`. It was verified through the GitHub
attestation REST API (the installed `gh` has no `attestation` subcommand):
one attestation covers all ten uploaded subjects, the DSSE statement names
`.github/workflows/release.yml @ refs/tags/v0.4.0-rc.6`, the resolved
dependency is commit `df86e53935b8d2ccacc1908ffa5e4b9c472911b7`, and every
local asset digest matches its attested subject digest. The transcript is in
[`artifacts/02-rc6-provenance.txt`](artifacts/02-rc6-provenance.txt).

## Pre-flight decisions

- The host is recorded as a virtualized KVM cloud VM, never as bare metal.
- The upgrade drill starts from the published `v0.3.0` tarball. The old
  installer carries the WAL-unlink defect, so the old control plane is
  restarted once **before** any identifiable-state writes.
- The **published rc.7** artifacts are the approved target for the clean
  install and the window. No source builds, local patches, or unreleased
  artifacts are used. The digests in the target table must be filled from the
  published release before the window.
- The field window starts only after publication, the `v0.3.0` → rc.7 (or
  `rc.6` → rc.7) drill, the uninstall/purge drill, the clean rc.7 install,
  a documented start with a guest DHCP lease and `login:` console, a policy
  change plus nwd restart, and **two consecutive `--wipe` installs reaching
  `TenantReady`** succeed.
- The rc.6 clean install (published artifact) is historical pre-window
  evidence for the blocker investigation only. It cannot qualify rc.7.
- The M2.5 delete-retention residual is disclosed above and in field plan
  §4 and §6.9; it is not a blocker and not a new rc.6/rc.7 finding.
