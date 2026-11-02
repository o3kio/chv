# Cloud Hypervisor Reference

Version: v53.0 (qualified pin; downloaded by the CHV installers with sha256 verification)

The qualified integration and KVM-qualification pin is v53.0
(`scripts/integration/kvm-smoke.sh`, `scripts/integration/qual/env-preflight.sh`).
`scripts/install.sh` downloads the same v53.0 for fresh installs and verifies
the qualified sha256 digests before install. The installer/qualification
version split previously recorded as a reconciliation item (see the M2.5 KVM
qualification evidence under
`docs/evidence/production-readiness/v0.3.0-rc1/02-single-authority-cutover/`)
was resolved by pinning the installer to the qualified version.

**Pin history (disclosed):** the qualified pin was v43.0 until 2026-10-03,
when the #448 re-qualification campaign (decision D6, option (b)) moved it to
v53.0 after six evidence legs (anchor, serial re-check, M4.3 lifecycle, M4.6
migration, M4.2/M4.4/M4.5 smokes, security regression — campaign evidence:
`docs/evidence/vmm-requalification/v53.0/`). The former v43.0 pin's exposure
to CVE-2026-27211 and CVE-2026-45782 is closed at v53.0 (runtime proof for
CVE-2026-27211 — leg 06); no advisory affects v53.0 as of 2026-10-03.
"Upstream-only" annotations in this document refer to features present in
upstream releases **at or above the listed version** and absent from the
pinned v53.0; nothing here is a claim that CHV exercises them.

Source of truth (pinned to the v53.0 tag so it does not silently track main):
[Cloud Hypervisor API docs](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v53.0/docs/api.md)
OpenAPI spec (same v53.0 pin):
https://raw.githubusercontent.com/cloud-hypervisor/cloud-hypervisor/v53.0/vmm/src/api/openapi/cloud-hypervisor.yaml

## Binaries

| Binary | Download | Install path |
|--------|----------|-------------|
| `cloud-hypervisor-static` | https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v53.0/cloud-hypervisor-static | `/usr/local/bin/cloud-hypervisor` |
| `ch-remote-static` | https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v53.0/ch-remote-static | `/usr/local/bin/ch-remote` |

## Architecture

The CLI can only be used for **launching** cloud-hypervisor. Once running, all
control goes through the REST API (on the Unix socket) or ch-remote (which wraps
the REST API). The CLI cannot control a running VM.

```
              REST API                    Internal API
User ------> micro_http ----+
                             |
User ------> ch-remote -----+----------> VMM control loop
                             |
CLI  ------> clap ----------+
```

## CLI Options

```
cloud-hypervisor [OPTIONS]
```

| Flag | Description | Default |
|------|-------------|---------|
| `--cpus <cpus>` | `boot=<boot_vcpus>,max=<max_vcpus>,topology=<t>:<c>:<d>:<p>,kvm_hyperv=on\|off,max_phys_bits=<n>,affinity=<list>,features=<list>` | `boot=1,max_phys_bits=46` |
| `--platform <platform>` | `num_pci_segments=<n>,iommu_segments=<list>,serial_number=<sn>,uuid=<uuid>,oem_strings=<list>` | |
| `--memory <memory>` | `size=<size>,mergeable=on\|off,shared=on\|off,hugepages=on\|off,hugepage_size=<size>,hotplug_method=acpi\|virtio-mem,hotplug_size=<size>,hotplugged_size=<size>,prefault=on\|off,thp=on\|off` | `size=512M` |
| `--memory-zone <zone>` | Per-zone: `size=<size>,file=<path>,shared=on\|off,hugepages=on\|off,host_numa_node=<id>,id=<id>,hotplug_size=<size>,prefault=on\|off` (repeatable) | |
| `--firmware <firmware>` | Path to firmware (architecture-specific load, e.g. CLOUDHV.fd) | |
| `--kernel <kernel>` | Path to kernel or PVH-capable firmware (e.g. vmlinux) | |
| `--initramfs <initramfs>` | Path to initramfs image | |
| `--cmdline <cmdline>` | Kernel command line | |
| `--disk <disk>` | `path=<path>,readonly=on\|off,direct=on\|off,iommu=on\|off,num_queues=<n>,queue_size=<n>,vhost_user=on\|off,socket=<path>,bw_size=<bytes>,bw_one_time_burst=<bytes>,bw_refill_time=<ms>,ops_size=<io_ops>,ops_one_time_burst=<io_ops>,ops_refill_time=<ms>,id=<device_id>,pci_segment=<id>` (repeatable) | |
| `--net <net>` | `tap=<name>,ip=<ip>,mask=<mask>,mac=<mac>,fd=<fds>,iommu=on\|off,num_queues=<n>,queue_size=<n>,id=<id>,vhost_user=<bool>,socket=<path>,vhost_mode=client\|server,pci_segment=<id>,offload_tso=on\|off,offload_ufo=on\|off,offload_csum=on\|off` (repeatable) | |
| `--rng <rng>` | `src=<path>,iommu=on\|off` | `src=/dev/urandom` |
| `--balloon <balloon>` | `size=<size>,deflate_on_oom=on\|off,free_page_reporting=on\|off` | |
| `--fs <fs>` | virtio-fs: `tag=<tag>,socket=<path>,num_queues=<n>,queue_size=<n>,id=<id>,pci_segment=<id>` (repeatable) | |
| `--pmem <pmem>` | `file=<path>,size=<size>,iommu=on\|off,discard_writes=on\|off,id=<id>,pci_segment=<id>` (repeatable) | |
| `--serial <serial>` | `off\|null\|pty\|tty\|file=<path>\|socket=<path>` | `null` |
| `--console <console>` | `off\|null\|pty\|tty\|file=<path>,iommu=on\|off` | `tty` |
| `--device <device>` | VFIO passthrough: `path=<path>,iommu=on\|off,id=<id>,pci_segment=<id>` (repeatable) | |
| `--user-device <dev>` | Userspace device: `socket=<path>,id=<id>,pci_segment=<id>` (repeatable) | |
| `--vdpa <vdpa>` | `path=<path>,num_queues=<n>,iommu=on\|off,id=<id>,pci_segment=<id>` (repeatable) | |
| `--vsock <vsock>` | `cid=<cid>,socket=<path>,iommu=on\|off,id=<id>,pci_segment=<id>` | |
| `--pvpanic` | Enable pvpanic device | |
| `--numa <numa>` | `guest_numa_id=<id>,cpus=<ids>,distances=<list>,memory_zones=<list>,sgx_epc_sections=<list>,pci_segments=<list>` (repeatable; `sgx_epc_sections` removed upstream v48.0 — not in the pinned v53.0) | |
| `--watchdog` | Enable virtio-watchdog | |
| `-v` | Increase debug output level (repeatable) | |
| `--log-file <path>` | Log file path (stderr if not set) | |
| `--api-socket <socket>` | HTTP API Unix socket: `path=<path>` or `fd=<fd>` | |
| `--no-shutdown` | Keep the VMM process alive after a guest-triggered shutdown | disabled |
| `--event-monitor <monitor>` | Event file: `path=<path>` or `fd=<fd>` | |
| `--restore <restore>` | Restore from snapshot: `source_url=<url>,prefault=on\|off` | |
| `--seccomp <mode>` | `true\|false\|log` | `true` |
| `--tpm <tpm>` | TPM device: `socket=<path>` | |
| `--sgx-epc <epc>` | SGX EPC: `id=<id>,size=<size>,prefault=on\|off` (repeatable) — **removed upstream v48.0; not in the pinned v53.0** | |

**The CLI does NOT have `--user-data` or any cloud-init flag.** Cloud-init userdata must be injected via a seed disk image.

## REST API

Available as soon as cloud-hypervisor starts, on the Unix socket from `--api-socket`.

### VMM Actions

| Action | Endpoint | Request Body | Response Body | Prerequisites |
|--------|----------|-------------|---------------|---------------|
| Ping | `/vmm.ping` | N/A | `VmmPingResponse` | N/A |
| Shutdown VMM | `/vmm.shutdown` | N/A | N/A | VMM running |

### VM Actions

| Action | Endpoint | Request Body | Response Body | Prerequisites |
|--------|----------|-------------|---------------|---------------|
| Create VM | `/vm.create` | `VmConfig` | N/A | VM not created yet |
| Delete VM | `/vm.delete` | N/A | N/A | N/A |
| Boot VM | `/vm.boot` | N/A | N/A | Created but not booted |
| Shutdown VM | `/vm.shutdown` | N/A | N/A | VM booted |
| Reboot VM | `/vm.reboot` | N/A | N/A | VM booted |
| Power button | `/vm.power-button` | N/A | N/A | VM booted |
| Pause VM | `/vm.pause` | N/A | N/A | VM booted |
| Resume VM | `/vm.resume` | N/A | N/A | VM paused |
| Snapshot VM | `/vm.snapshot` | `VmSnapshotConfig` | N/A | **VM paused** |
| Coredump VM | `/vm.coredump` | `VmCoredumpData` | N/A | **VM paused** (x86_64 + guest_debug only) |
| Restore VM | `/vm.restore` | `RestoreConfig` | N/A | Created but not booted |
| Resize VM | `/vm.resize` | `VmResize` | N/A | VM booted |
| Resize disk (available in the pinned v53.0 — added upstream v50.0, #7476; not exercised by CHV) | `/vm.resize-disk` | `VmResizeDisk` | N/A | VM created |
| Resize memory zone | `/vm.resize-zone` | `VmResizeZone` | N/A | VM booted |
| VM info | `/vm.info` | N/A | `VmInfo` | VM created |
| VM counters | `/vm.counters` | N/A | `VmCounters` | VM booted |
| Inject NMI | `/vm.nmi` (the v43.0 OpenAPI file carried a typo listing `/vmm.nmi`, corrected upstream; the real endpoint was always `/vm.nmi` — the pinned v53.0 OpenAPI carries the correct path) | N/A | N/A | VM booted |
| Add VFIO device | `/vm.add-device` | `VmAddDevice` | `PciDeviceInfo` | VM booted |
| Add disk | `/vm.add-disk` | `DiskConfig` | `PciDeviceInfo` | VM booted |
| Add fs | `/vm.add-fs` | `FsConfig` | `PciDeviceInfo` | VM booted |
| Add pmem | `/vm.add-pmem` | `PmemConfig` | `PciDeviceInfo` | VM booted |
| Add net | `/vm.add-net` | `NetConfig` | `PciDeviceInfo` | VM booted |
| Add user device | `/vm.add-user-device` | `VmAddUserDevice` | `PciDeviceInfo` | VM booted |
| Add vdpa | `/vm.add-vdpa` | `VdpaConfig` | `PciDeviceInfo` | VM booted |
| Add vsock | `/vm.add-vsock` | `VsockConfig` | `PciDeviceInfo` | VM booted |
| Remove device | `/vm.remove-device` | `VmRemoveDevice` | N/A | VM booted |
| Receive migration | `/vm.receive-migration` | `ReceiveMigrationData` | N/A | N/A |
| Send migration | `/vm.send-migration` | `SendMigrationData` | N/A | VM booted + shared mem or hugepages |

### REST API Examples

Create a VM:
```bash
curl --unix-socket /tmp/ch.sock -i \
     -X PUT 'http://localhost/api/v1/vm.create' \
     -H 'Accept: application/json' \
     -H 'Content-Type: application/json' \
     -d '{
         "cpus":{"boot_vcpus": 4, "max_vcpus": 4},
         "payload":{"kernel":"/opt/clh/kernel/vmlinux", "cmdline":"console=ttyS0 root=/dev/vda1 rw"},
         "disks":[{"path":"/opt/clh/images/focal.raw"}],
         "rng":{"src":"/dev/urandom"},
         "net":[{"ip":"192.168.10.10", "mask":"255.255.255.0", "mac":"12:34:56:78:90:01"}]
     }'
```

Boot, info, reboot, shutdown:
```bash
curl --unix-socket /tmp/ch.sock -i -X PUT 'http://localhost/api/v1/vm.boot'
curl --unix-socket /tmp/ch.sock -i -X GET 'http://localhost/api/v1/vm.info'
curl --unix-socket /tmp/ch.sock -i -X PUT 'http://localhost/api/v1/vm.reboot'
curl --unix-socket /tmp/ch.sock -i -X PUT 'http://localhost/api/v1/vm.shutdown'
```

## How CHV launches Cloud Hypervisor

CHV does not configure the VM on the cloud-hypervisor command line. The agent
spawns the VMM with `--no-shutdown` and the API socket, with stdout discarded
and stderr appended to `cloud-hypervisor.stderr.log` in the VM runtime directory
(`crates/chv-agent-runtime-ch/src/process.rs`):

```bash
cloud-hypervisor --no-shutdown --api-socket /var/lib/chv/agent/vms/{vm_id}/vm.sock
```

With this option, a guest poweroff leaves Cloud Hypervisor running and reports
the VM in a terminal state. A later start calls `/vm.boot` on that same VMM;
the agent still re-spawns and re-creates the VM when the VMM itself exited
unexpectedly. Delete requests `/vmm.shutdown`, verifies process exit, and then
uses the existing identity-checked SIGKILL fallback if needed. Force stop also
continues to use that verified kill path. The lifecycle regression tests are
in [`process.rs`](../../../crates/chv-agent-runtime-ch/src/process.rs); the
KVM lifecycle qualification covers same-process stop/start in
[`m4.3-lifecycle.sh`](../../../scripts/integration/qual/m4.3-lifecycle.sh).
Verification status and the high-risk lifecycle disclosure are recorded in
[`issue-457-no-shutdown.md`](../../evidence/issue-457-no-shutdown.md).

The VM itself is then created over the **REST API**: the agent builds the full
`VmConfig` JSON (cpus, memory, payload — firmware or kernel —, disks including
the cloud-init seed ISO, net `tap`/`mac` pairs, serial, and console) and PUTs
it to `/api/v1/vm.create` on that socket via its HTTP client
(`ch_api_request_with_body` → `PUT /api/v1/vm.create`,
`crates/chv-agent-runtime-ch/src/process.rs`; any non-200/204 response kills
the child — a half-created VM is never left running). Before the create call
the agent waits up to 10 s for the API socket to appear and persists the VMM
pid plus the exact `vm.create` payload, so adoption and respawn can recover a
VM whose agent died between spawn and create.

**Serial console:** the shipped default serial mode is **Socket**, not tty —
the `vm.create` payload carries
`{"mode": "Socket", "socket": "/var/lib/chv/agent/vms/{vm_id}/serial.sock"}`,
and cloud-hypervisor binds that listener during `vm.create` while the agent
connects to it as a client right after (`DEFAULT_SERIAL_MODE`, `crates/chv-common/src/hypervisor.rs`;
seeded Pty defaults were flipped to Socket by control-plane migration
`cmd/chv-controlplane/migrations/0055_serial_socket_default.sql`). Pty remains
available as an explicit global/per-VM tuning (`hv.serial_mode`) for
interactive kernel debugging. Console mode defaults to `Off`.

After spawning, the VM is controlled entirely via the HTTP API on the Unix socket.
The CHV agent uses direct HTTP requests; operators can use `ch-remote` for manual interaction.

## Important Notes for CHV Development

1. The CLI is for **launching only**. All runtime control uses the REST API.
2. There is **no `--user-data` flag**. Cloud-init must use a seed disk.
3. `--firmware` is for UEFI boot (CLOUDHV.fd); `--kernel` is for direct kernel boot (vmlinux).
4. `--net` requires a pre-created tap device (nwd creates these via `ip tuntap add`).
5. `--api-socket` creates the Unix socket used by ch-remote and the CHV agent.
6. **Snapshots require the VM to be paused first** (`/vm.pause` then `/vm.snapshot`).
7. **Restore requires the VM to be in Created-but-not-booted state** (`/vm.restore`).
8. VM coredump is x86_64-only and requires the `guest_debug` feature.
