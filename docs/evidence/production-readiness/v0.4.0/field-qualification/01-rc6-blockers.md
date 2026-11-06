# 01 — `v0.4.0-rc.6` blocker report

**Recorded:** 2026-11-06 (UTC)
**Host:** `field-host-1` (virtualized KVM cloud VM, never described as bare metal)
**Candidate:** `v0.4.0-rc.6`, tag object `70a362a3`, commit
`df86e53935b8d2ccacc1908ffa5e4b9c472911b7`
**Clean install:** run 3 from the published tarball (22 s, `rc=0`), after the
documented complete removal. Node `f22b8cc7-cd0d-435f-8d7e-22f81b4b04da`;
seeded VM `5531bda5a80cea45770addaba999ba3c` (`dev-vm-1`); network
`eac319350ba9fff7b0b957aedef20ba6`.

## Status and decision

**`v0.4.0-rc.6` is disqualified as the field-window and stable base.** Four
defects reproduced on a clean, published-artifact install are blocker-class
under field plan §6.9; two further default-install defects are operational
defects under §6.2 (B3 also prevents this profile from being qualified as
configured; B7 misreports the artifact in the install record that becomes the
field evidence).

The field plan (§6.3) says the campaign runs on rc.6 and "there is no successor
unless a blocker forces a new RC." These findings force a successor RC. No
field-window day has started, no stable tag exists, and the published rc.6 tag
and assets are untouched.

The seven findings below are ordered by the path they break. B6 is a
consequence of the first two and is recorded so the recovery contract is not
silently wrong.

| # | Finding | Class | Breaks |
|---|---|---|---|
| B1 | Agent unit's closed device cgroup denies `/dev/net/tun`; CH cannot attach the VM NIC | blocker | documented `start`; §6.3 network workload; §6.4 recovery |
| B2 | `start_vm` marks `vm.boot` failures `succeeded`; after a failed first boot the retry runs with no NIC and no console | blocker | authority/state divergence; §6.9 |
| B3 | Default install creates two interfaces on `10.200.0.1/24` (`chvbr0` and nwd's `br-<id>`); the host routes guest traffic to the wrong bridge | operational defect (§6.2); blocker for this profile as configured | network topology; §6.1 baseline; §6.4 nwd drill |
| B4 | nwd starts dnsmasq without a writable lease file, so DHCP fails under `ProtectSystem=strict` | blocker | documented network profile; §6.3 policy/connectivity |
| B5 | `install.sh --wipe` retains `core.db` but generates a new `node_id`; the agent crash-loops on `Identity(Conflict)` | blocker | documented removal/recovery; §6.9 |
| B6 | After any failed first boot, CH's retry reports success with `config.net=[]`; stop/start does not heal | consequence of B1/B2; recovery contract | §6.9 "hidden maintainer knowledge" if undocumented |
| B7 | Local release-directory mode prints/writes `Version: 0.3.0` while installing the rc artifact (the offline fallback pin, not the directory's version) | operational truth defect (§6.2); not blocker-class | install record / evidence integrity; operator cannot tell the installed artifact from the banner |

**Correction to an earlier working hypothesis:** tap ownership is **not** a
defect. CH as `chv` with `CapEff=0` attaches an nwd-created (ownerless) tap and
boots to `login:` (artifact 03). The EPERM was at `open("/dev/net/tun")` from
the agent's device cgroup, not at `TUNSETIFF`. Only B1 needs a unit change; nwd
does not need `user chv` taps and CH does not need `CAP_NET_ADMIN`.

## B1 — the agent's device cgroup denies `/dev/net/tun`

**Observed on the field path.** The documented start of the seeded VM returned
HTTP 200 and the BFF reported `power_state=Running`, while the VMM stayed
`Created`, the tap stayed `DOWN`, and `console.log` stayed 0 bytes for the
36-second poll in the run-3 transcript. The agent logged:

```
WARN chv_agent_runtime_ch::process: vm.boot returned non-success
  (VM may have auto-booted) vm_id=5531bda5... status=500
```

A raw first `vm.boot` against a fresh VM captured the VMM body verbatim
(artifact 07; `boot_http=500`, `state_after=Created`, `console_bytes=0`):

```json
["Error from API","The VM could not boot","Error from device manager",
 "Cannot create virtio-net device","Failed to open taps",
 "Open tap device failed","Couldn't open /dev/net/tun",
 "Operation not permitted (os error 1)"]
```

**Root cause.** `packaging/systemd/chv-agent.service` has
`DeviceAllow=/dev/kvm rw` and no `DevicePolicy=`, so systemd selects the closed
device policy. The CH child process inherits the agent cgroup and runs as `chv`
with `CapEff=0`. `/dev/net/tun` is `crw-rw-rw- root:root 666`, but the device
cgroup still denies the `open()` with `EPERM`.

**Proof (artifact 02).** A transient unit in the agent's exact context:

- `DevicePolicy=closed` + `DeviceAllow=/dev/kvm rw`:
  `FAILED /dev/net/tun [Errno 1] Operation not permitted`.
- adding `DeviceAllow=/dev/net/tun rw`:
  `opened /dev/net/tun`.

**Definitive end-to-end proof (artifact 04).** With only the device allowance
applied at runtime (`systemctl set-property chv-agent.service
DeviceAllow=/dev/kvm DeviceAllow=/dev/net/tun`), a fresh API-created VM
`daf9d7f943cf1ce96e7198aa0022e283` (`dev-vm-7`) with the tap **exactly as nwd
created it** started through the documented API:

```
start_http=200
t=2s  state=Running carrier=1 console=2556
t=4s  state=Running carrier=1 console=11141
t=8s  state=Running carrier=1 console=39739
t=10s state=Running carrier=1 console=53003
```

The VMM held five `/dev/net/tun` fds, `vm.info.config.net` had one entry, the
guest reached `cloud-init` (`init-local` at 7.24 s), and the agent logged no
`vm.boot` warning. No tap ownership change was made.

**Fix.** Add `DeviceAllow=/dev/net/tun rw` to the packaged agent unit (and to
the tarball's copy). Add a packaging test that asserts the allowance is present
and records why the closed device policy is intentional for every other
device.

## B2 — `start_vm` swallows `vm.boot` failure; retries run without a NIC

**Observed.** `crates/chv-agent-runtime-ch/src/process.rs` treats a non-2xx
`vm.boot` as a warning in both the normal path (~line 4326) and the re-spawn
path (~line 3628). The operation consequently completes `succeeded`, and the
BFF projects the requested state as the display state.

The clean run-3 transcript shows the exact divergence:

```
vm {'power_state': 'Running', 'health': 'Unknown', ...}
start_http=200
t=36s vmm=Created tap=DOWN console_bytes=0
```

The operations table records `start_vm ... succeeded` for every VM, including
four starts that logged `vm.boot ... status=500` (VM1, VM2, VM4, and the run-1
seed VM `c69b42bce3f870245c80bdad430acc3f`). The BFF's `power_state` is the
desired state (`COALESCE(desired_power_state, runtime_status)`), so it cannot
be used as ground truth.

**Second facet (B6).** After a first boot fails at device creation, a retry
`vm.boot` can return `204` and move the VMM to `Running` with
`vm.info.config.net = []` — no tap, no `/dev/net/tun` fd, and (for VM1) no
console capture. Measured on VMs 2–5:

```
4290fa2f... state Running net 0
cf750cab... state Running net 0
08de2de5... state Running net 0
a66c74e3... state Running net 0
7b0cd93e... state Running net 1   (first boot succeeded)
daf9d7f9... state Running net 1   (first boot succeeded)
```

Stop/start does not heal this. A fresh VM is the only recovery observed.
Without documentation, that is hidden maintainer knowledge; with B2 fixed the
operation should at least fail visibly.

**Fix.** On non-2xx `vm.boot`, re-read `vm.info`; treat `Running`/`Paused` as
idempotent success, otherwise return a structured error that includes the VMM
status and body. After a successful boot, compare the requested NIC count with
`vm.info.config.net` and fail with a clear "booted without the requested NIC"
error if it is short. Extend the mock VMM in `process.rs` to return a
configurable `vm.boot` status and `config.net`, and add tests for: 500 with
state `Created` (operation must fail), 500 with state `Running` (idempotent
success), and 204 with an empty `config.net` (operation must fail).

## B3 — two interfaces own `10.200.0.1/24`

**Observed (artifact 05).**

```
chvbr0           10.200.0.1/24
br-eac31935dc4f  10.200.0.1/24

10.200.0.0/24 dev chvbr0           proto kernel scope link src 10.200.0.1
10.200.0.0/24 dev br-eac31935dc4f  proto kernel scope link src 10.200.0.1

ip route get 10.200.0.175
10.200.0.175 dev chvbr0 src 10.200.0.1 uid 0

ip neigh show
10.200.0.18  dev chvbr0 FAILED
10.200.0.175 dev chvbr0 FAILED
...
```

**Root cause.** `install.sh` `setup_network` creates the legacy `chvbr0`
"dev bootstrap" at `10.200.0.1/24` and enslaves `ens19`. `seed_dev_resources`
then creates the default network with the same CIDR and gateway
(`${bridge_ip%.*}.0/${prefix}`, gateway `${bridge_ip}`). nwd materializes that
network as `br-eac31935dc4f` and assigns the same gateway address. The two
kernel routes are identical; the host selects `chvbr0`, which has no guest
ports, so ARP for guest addresses fails.

nwd's tenant policy also default-drops host-originated new connections
(`chv-policy-out` allows only established/DHCP), so ICMP is not a valid probe.
The defect is nevertheless real: any host-originated traffic that the policy
is later changed to allow will be routed to the wrong bridge.

**Classification.** This is an operational defect under §6.2 (a documented
install step produces a wrong host configuration). It is treated as a blocker
for this profile because §6.1 requires the network topology to be recorded and
§6.4 requires an nwd restart to re-materialize the documented network. The
default install cannot be qualified with two identical routes.

**Fix options (recommended first).**

1. When the installer will seed the default network (`INSTALL_CHV_NO_SEED !=
   1`), remove/skip the legacy `chvbr0` bridge, its persisted config, and its
   NAT rules; keep `net.ipv4.ip_forward=1` and the source-based NAT rule for
   the seeded CIDR. nwd owns the only bridge. Add a post-install assertion
   that exactly one interface owns the seeded gateway
   (`ip -o addr show | grep '10.200.0.1/24'` has one line).
2. Or default `INSTALL_CHV_NO_BRIDGE=1` and document the nwd-owned bridge as
   the profile; keep the legacy bootstrap opt-in.
3. Do not simply seed a different CIDR: the installer, uninstall path, and
   `chv_network_cidr` all derive from `INSTALL_CHV_BRIDGE_CIDR`, and a split
   source of truth is a larger, riskier change.

## B4 — dnsmasq cannot write its lease file

**Observed (artifact 05).** Every nwd start since the clean install logs:

```
dnsmasq[...]: cannot open or create lease file
  /var/lib/misc/dnsmasq.leases: Permission denied
dnsmasq[...]: FAILED to start up
WARN chv_nwd_core::executor: failed to start dnsmasq
  error=network unavailable: dnsmasq — dnsmasq failed
```

**Root cause.** `crates/chv-nwd-core/src/executor.rs` `start_dnsmasq` generates
a config with no `dhcp-leasefile` and passes only `--conf-file` and
`--pid-file`. dnsmasq then uses its compiled default,
`/var/lib/misc/dnsmasq.leases`. nwd runs as `chv` with `ProtectSystem=strict`
and `ReadWritePaths=/run/chv/nwd /run/netns /var/lib/chv`; `/var/lib/misc` is
`root:root 0755`, so the write is denied. The seeded VM uses a static
`network-config`, which is why the workload can look healthy while the DHCP
path is dead.

**Fix.** Add
`--dhcp-leasefile=/run/chv/nwd/dnsmasq-<network>.leases` (writable, and
cleaned with `/run/chv/nwd`). Update `dnsmasq_args`, `stop_dnsmasq`, and the
installer/uninstall cleanup lists (`dnsmasq-*.leases`). Add a test that the
generated argv contains the lease path and that the path is under a
`ReadWritePaths` entry. Re-run the nwd restart drill and assert dnsmasq is
running and the lease file is created.

## B5 — `--wipe` retains authority state but changes the node identity

**Observed.** Run 2 (`install.sh --wipe` over the run-1 install) completed the
installer steps but the agent crash-looped:

```
Error: Identity(Conflict {
  authoritative_source: "existing Core",
  conflicting_source: "importable NodeCache"
})
```

The retained `/var/lib/chv/agent/core.db` has host
`be476a16-135c-410c-a439-51a3f3c15730`; the regenerated
`/etc/chv/agent.toml` has `node_id = "d0d70212-03e1-4688-9ebf-45c30d642a8f"`.
The installer then skipped VM seeding after its 60 s wait. Evidence is
preserved at `/root/chv-field/preserved/rc6-clean-run2-wipe-conflict/`
(`DEFECT.txt`, `core.db`, `node-cache-v1.archive`, `agent.toml`, journals).

**Root cause.** `wipe_deployment` deliberately retains "persistent agent
authority state" (`core.db`, its runtime lease, `node-cache-v1.archive`).
`install_configs` then unconditionally generates a new `CHV_NODE_ID` and
writes it into `agent.toml`. ADR-019 makes the existing Core host ID
authoritative and treats a differing configured value as
`blocked`/`Identity(Conflict)`, so the agent fails closed. Recovery requires
deleting the authority state — knowledge that is not in the operator docs.

`--wipe` is advertised as "Full teardown before clean install" and is named a
final removal path in the rc.6 release notes (§2.3, §4). This is a failed
documented removal/recovery path.

**Fix (recommended).** On `--wipe`, after the agent is stopped and
`validate_no_chv_processes` passes, remove the retained authority state
atomically with the VM/control-plane state it already destroys:
`core.db`, `core.db-wal`, `core.db-shm`, `core.db-journal`, the
`.core.db.cellhv-runtime-authority.lease`, and `node-cache-v1.archive`.
`--wipe` already deletes every VM and the control-plane database, so there is
no VM identity to orphan and no concurrent start to split the lock namespace.
Update the `wipe_deployment` comment to explain that `--wipe` is the one path
that intentionally discards authority state.

**Alternative.** Preserve the Core host ID by reading `host_identity` from the
retained `core.db` and using it as `CHV_NODE_ID`. This is smaller but leaves
stale VM/attachment rows in `core.db` after the VM directories are deleted;
the install must then reconcile or clear them. The removal option is the
cleaner match for the advertised "full teardown."

Add a test: run `install.sh --wipe` twice in a row on the same host and assert
the second run reaches `TenantReady` and the agent reports no identity
conflict. The test must not depend on the field host's pre-existing data.

## B6 — post-failure NIC omission and dead console

Recorded with B2 because it changes the recovery contract.

- VM1: VMM `Running`, `config.net=[]`, no `/dev/net/tun` fd, `console.log` 0
  bytes.
- VM2–VM5: same, after the first boot had failed.
- VM6/VM7: first boot succeeded with the device fix; `config.net` has one
  entry, five `/dev/net/tun` fds, 74 KB/58 KB console, guest `ens3` got the
  BFF-assigned address, cloud-init finished.

The documented stop/start does not restore the NIC; only delete-and-recreate
did. The rc.7 change should make the operation fail visibly (B2) and the
rc.7 docs must state the delete-and-recreate recovery. If the post-boot
`config.net` check is implemented, the failure is detected at start time and
the operator is not left with a silent "Running" VM.

## B7 — the install record names the wrong version

The run-3 transcript (artifact `rc6-08-clean-install-run3.log`) was produced
from the published release directory
`chv-0.4.0-rc.6-linux-amd64`, and the binaries in it are the rc.6 build
(installer SHA-256 `63f465f0…6076`, recorded at the top of the log). The same
transcript says:

```
[INFO] Local tarball mode with no explicit version — using 0.3.0 for the extracted directory name.
[INFO] Installing CHV version: 0.3.0
...
Version:        0.3.0
```

`resolve_version()` set `INSTALL_CHV_VERSION` to the `fallback_version`
offline pin whenever a local release directory/tarball was used without an
explicit pin, and the completion banner prints `INSTALL_CHV_VERSION`
verbatim. So the documented local-release-directory install of an RC reports
the last stable version. This is not blocker-class under §6.9, but it is an
operational truth defect under §6.2: the artifact record for the field
install would name the wrong release, and an operator could not distinguish
the installed RC from `0.3.0` using the banner alone.

rc.7 derives the version from the `chv-<version>-linux-amd64` release
directory or tarball name (`version_from_release_dir()`); an explicit
`INSTALL_CHV_VERSION` still wins, and `fallback_version` stays `0.3.0` for
the genuine offline path where no release artifact names a version.

## What rc.7 must contain (scope proposal)

> **Implementation status (pre-publication).** Items 1–5 and item 6 (B7) are
> implemented on the release-prep branch with targeted tests; items 7–8 are in
> progress and are **not claimed complete**. rc.7 has not been committed,
> reviewed, published, or re-run on the field host at the time this note was
> updated. The clean rc.7 install, DHCP lease, policy change, nwd restart, and
> two consecutive `--wipe` installs remain required post-publication drills.

This is a blocker-fix RC, not a feature RC. Proposed minimal scope:

1. **B1** — `packaging/systemd/chv-agent.service`:
   `DeviceAllow=/dev/net/tun rw`. Packaging test asserts it.
2. **B2/B6** — `crates/chv-agent-runtime-ch/src/process.rs`:
   non-2xx `vm.boot` is a hard failure unless `vm.info` reports
   `Running`/`Paused`; post-boot NIC-count check; structured error carries the
   VMM body; mock-based tests for the three cases above.
3. **B4** — `crates/chv-nwd-core/src/executor.rs`:
   `--dhcp-leasefile=/run/chv/nwd/dnsmasq-<id>.leases`; argv/cleanup tests;
   nwd restart drill proves dnsmasq runs and a lease is written.
4. **B3** — `scripts/install.sh`: remove/skip the legacy `chvbr0` bootstrap,
   its persisted config, and its NAT rules when the installer seeds the
   nwd-managed default network (keep forwarding/NAT), or default
   `INSTALL_CHV_NO_BRIDGE=1`; assert exactly one owner of the seeded gateway.
5. **B5** — `scripts/install.sh`: `--wipe` removes the retained agent
   authority state atomically (or preserves the matching node ID); two
   consecutive `--wipe` installs must reach `TenantReady`.
6. **B7** — `scripts/install.sh`: local release-directory/tarball mode derives
   `INSTALL_CHV_VERSION` from `chv-<version>-linux-amd64` so the install
   record and completion banner name the artifact; explicit pin still wins;
   `fallback_version` stays `0.3.0`. Packaging test asserts the derivation.
7. **Docs** — rc.7 release notes and support matrix; install/uninstall and
   troubleshooting recovery text; update this profile to rc.7; mark rc.6 as
   disqualified/superseded without rewriting the rc.6 tag.
8. **Re-run** — v0.3.0 → rc.7 upgrade drill, purge drill, clean rc.7 install
   from the published tarball, documented start with console + guest network,
   DHCP lease, policy change with before/after connectivity, nwd restart,
   `--wipe` reinstall, then the 21–30 day window.

**Not in scope:** tap ownership changes (disproven), new `CAP_NET_ADMIN` on
the agent or CH, multi-host/Volvisor work, and any change to a published tag.

## Claim boundary

No field-window day has run. No stable tag exists. This report is pre-window
blocker evidence, not a field result. The rc.6 artifacts remain published and
their digests are unchanged; the rc.6 tag is not rewritten. rc.7 was published
on 2026-11-06 as the successor and then disqualified by its own pre-window
drill (B8/B9, Part 2); it is not rewritten and its clean-install re-run was
never completed. rc.8 is the approved successor; its artifact identity is
recorded in the profile after publication, and the rc.8 clean-install and
upgrade drills run before the window opens. Nothing in this report claims that
rc.7 fixes the defects on a published artifact.

## Evidence index

- `artifacts/03-rc6-blockers/02-device-cgroup-proof.log` — closed-policy EPERM
  and the `/dev/net/tun` allowance.
- `artifacts/03-rc6-blockers/07-raw-vm-boot-500-body.log` — raw CH response
  (`500`, `Couldn't open /dev/net/tun`) for a BFF-created VM under the shipped
  device policy; VMM stays `Created`, console 0 bytes.
- `artifacts/03-rc6-blockers/04-definitive-untouched-tap.log` — documented
  create/start of VM7 with the untouched nwd tap; `carrier=1`, 5 tun fds.
- `artifacts/03-rc6-blockers/03-tap-ownership-proof.log` — CH attaches an
  ownerless nwd tap as `chv` with `CapEff=0`; correction of the earlier
  hypothesis.
- `artifacts/03-rc6-blockers/01-start-vm-swallow.log` — all `start_vm`
  operations `succeeded`, including the ones that logged `vm.boot` 500.
- `artifacts/03-rc6-blockers/05-duplicate-cidr-and-dnsmasq.log` — two
  `10.200.0.1/24` interfaces, wrong route selection, failed ARP, dnsmasq
  lease-file denial, generated conf without `dhcp-leasefile`.
- `artifacts/03-rc6-blockers/rc6-08-clean-install-run3.log` — run-3 install,
  the `Version: 0.3.0` banner for the rc.6 release directory (B7), and the
  36 s `Running`/`Created` divergence.
- `artifacts/03-rc6-blockers/rc6-09-start-defect-repro.log` — second
  API-created VM reproduces the documented-start failure.
- `artifacts/03-rc6-blockers/wipe-identity-conflict-DEFECT.txt` — run-2
  `--wipe` identity conflict summary; full state is preserved at
  `/root/chv-field/preserved/rc6-clean-run2-wipe-conflict/`.

---

# Part 2 — `v0.4.0-rc.7` pre-window drill findings (B8, B9)

**Recorded:** 2026-11-06 (UTC)
**Host:** `field-host-1` (virtualized KVM cloud VM, never described as bare metal)
**Candidate:** published `v0.4.0-rc.7`, annotated tag object
`e0da45d70e58f64f8d0e60b0ac76298974edffe8`, commit
`1b8f68f17378d135cb0bbd72f46932e4e7240199`
**Path:** package upgrade from the published rc.7 `.deb` set over the rc.6
tarball install; authority `core-managed`; node
`f22b8cc7-cd0d-435f-8d7e-22f81b4b04da`; network
`eac319350ba9fff7b0b957aedef20ba6` (`10.200.0.0/24`, gateway `10.200.0.1`,
DHCP `.50`–`.205`).

## Status and decision

**`v0.4.0-rc.7` is disqualified as the field-window and stable base.** Two
defects reproduced on the published artifact after the documented upgrade and
restart are blocker-class under field plan §6.9. rc.7 remains published; its
tag and assets are **not** rewritten. `v0.4.0-rc.8` is the successor and the
final release candidate.

| # | Finding | Class | Breaks |
|---|---|---|---|
| B8 | `chv-nwd` runs as non-root `chv` with only `CAP_NET_ADMIN CAP_NET_RAW CAP_SYS_ADMIN`; dnsmasq cannot bind UDP/67 because `net.ipv4.ip_unprivileged_port_start=1024`, so the documented DHCP path is dead | blocker | documented network profile; §6.3 connectivity; §6.9 |
| B9 | `TopologyTable` is in-memory only; after a documented nwd restart the next policy update fails `set_firewall_policy failed: not found: topology <id> (NOT_FOUND)` and the operation ends `DISPATCH_FAILED` even though the bridge, TAPs, and nft table still exist | blocker | documented recovery; §6.4 nwd-restart drill; §6.9 |

Two follow-ups were found while reviewing the B9 fix. They are operational
defects under §6.2, not additional blocker-class findings:

| # | Follow-up | Class | Effect |
|---|---|---|---|
| F1 | DHCP MAC→IP reservations lived under `/run/chv/nwd`, which `RuntimeDirectory=` wipes on every restart | operational defect (§6.2) | a restarted dnsmasq stopped reserving the Core-assigned address |
| F2 | A topology journal or reservation set survived `--wipe` and package removal | operational defect (§6.2) | a clean install could replay a deleted deployment's networks |

## B8 — dnsmasq cannot bind the DHCP server socket as non-root

**Observed on the published rc.7 artifact.** The installed unit carried
exactly:

```
AmbientCapabilities=CAP_NET_ADMIN CAP_NET_RAW CAP_SYS_ADMIN
CapabilityBoundingSet=CAP_NET_ADMIN CAP_NET_RAW CAP_SYS_ADMIN
```

The live process's `CapEff`/`CapAmb` decoded to
`cap_net_admin,cap_net_raw,cap_sys_admin`; the kernel floor was
`net.ipv4.ip_unprivileged_port_start = 1024`. Running dnsmasq as `chv` with
that exact capability set and the nwd-generated DHCP configuration produced:

```
dnsmasq: failed to bind DHCP server socket: Permission denied
```

exit status `2`, no pid file. Re-running the same command with
`CAP_NET_BIND_SERVICE` added bound the socket and logged
`DHCP, sockets bound exclusively to interface br-eac31935dc4f` until the
transient unit was stopped. Evidence:
`artifacts/05-rc7-prewindow-drills/11-b8-capability-repro.log` (script
`11-b8-capability-repro.sh`).

The upgrade drill also showed the dnsmasq process absent across a 40 s poll
after a policy change (`08-policy-change-and-b4-on-upgraded-host.log`), and
the nwd log carried no successful spawn. **Correction:** that script's
`pgrep -c dnsmasq` line matched the host's two libvirt `default`-network
dnsmasq processes; the exact CHV-owned count is zero (the correction note is
appended to `10-b9-repro.log`). B8 stands.

**Fix in rc.8.** The unit's ambient and bounding sets add
`CAP_NET_BIND_SERVICE`, which the dnsmasq child inherits and which grants
nothing beyond binding privileged ports. dnsmasq start remains a hard failure
in `ensure_topology`: a topology that reports success with no DHCP server is
the silent-success class behind B4 and B8. Packaging tests pin the exact cap
set in both the packaged and example units.

## B9 — an nwd restart loses the topology table; the next policy update fails `NOT_FOUND`

**Observed on the published rc.7 artifact.** With the topology present, a
policy update succeeded (`UpdateNetworkPolicy` task
`fc68d8e5ce7c627d7c6ec1f05fd85c12`, `Succeeded`). The documented
`systemctl restart chv-nwd` changed the main PID `133166` → `174335`. No
re-ensure followed: in `core-managed` mode `reconcile_networks` returns early
through `require_mutation()`, so reconcile is observe-only and the in-memory
`TopologyTable` stays empty. The next policy update (task
`ecdc0f0d0deb4eed927b22b1e59e475f`) stayed `RetryPending` through
`MAX_DISPATCH_RETRIES` and then failed:

```
status: Failed
error_code: DISPATCH_FAILED
error_message: permanently failed after 3 retries: ... apply_network_policy
  failed: ... network unavailable: nwd — set_firewall_policy failed:
  not found: topology eac319350ba9fff7b0b957aedef20ba6 (NOT_FOUND)
```

The bridge, TAPs, and `inet chv-eac319350ba9fff7b0b957aedef20ba6` table were
still present across the restart; only nwd's ownership record was lost. The
nft guard therefore remained but the operator could no longer change policy
without hidden maintainer knowledge: `core-managed` reconcile is observe-only,
so an agent restart did not re-ensure the network either; only a new
materialization (for example a VM create on that network) did. Evidence:
`artifacts/05-rc7-prewindow-drills/10-b9-repro.log` (script
`10-b9-repro.sh`) and `09-nwd-restart-contract-repro.log` (the same contract
before the restart: the create succeeds, but nwd has no dnsmasq and no
topology state to recover).

**Why this is blocker-class.** Plan §6.4 requires an nwd restart / network
disruption to re-materialize the bridge and policy; plan §6.9 treats a failed
documented recovery that needs hidden maintainer knowledge as blocker-class.
The defect reproduces on the published artifact, not only in a unit test.

**Fix in rc.8 (three layers).**

1. **Durable bridge-only journal.** `crates/chv-nwd-core/src/topology_store.rs`
   atomically persists the exact `TopologySpec` protobuf under
   `/var/lib/chv/nwd/topologies/<network_id>.pb` after a successful apply,
   removes it on delete, and `NetworkServer::serve` replays every entry
   before binding its socket. Writes fsync both the file and the directory;
   delete fsyncs the unlink; a corrupt or ambiguous entry fails startup
   loudly instead of serving a partial table. The idempotent ensure path
   reads the current entry and repairs a missing or divergent one without
   rewriting an unchanged journal on every reconcile tick. Fabric topologies
   are deliberately **not** journaled: the stretched-L2 fabric is design-only
   and its provider owns its durable surface.
2. **Durable firewall/NAT guard.** `crates/chv-nwd-core/src/policy_store.rs`
   journals the last requested policy pair and version under
   `/var/lib/chv/nwd/policies/<network_id>.json`, written under the same lock
   as the in-memory `policy_state` mutation so concurrent firewall/NAT RPCs
   cannot persist a half pair. Recovery restores the pair and re-applies it
   fail-closed before serving; delete removes it. This is required by §6.4:
   nftables survives a process restart but not a host reboot, and the
   in-memory record is what re-scopes the interface guard on attach. The base
   hooks are `policy accept` by design; only the guard dispatches guest
   traffic into the policy chains, so a lost guard is an isolation gap, not
   just a missing convenience. A payload the engine can never apply is rejected
   at the RPC boundary before it is journaled (recovery replays fail-closed, so
   a poison entry would crash-loop the daemon); if a persisted entry still
   cannot be re-applied, startup names the exact file to remove.
3. **Agent startup replay.** The rc.7 → rc.8 upgrade starts with an empty
   journal, while the Core store already holds the live networks. The agent
   queries nwd's owned topologies and re-ensures only the distinct
   bridge-only networks referenced by a live Core VM that nwd does not
   already own, with bounded retries, before serving. Skipping an
   already-owned network matters: nwd recovers its journal before serving,
   and a VM definition is a create-time snapshot, so replaying it could roll
   a later CIDR/gateway change back. This both closes the upgrade gap and
   seeds the journal for later nwd-only restarts. A failed ownership query
   falls back to replaying every network; the replay is best-effort and
   cannot block agent startup.

**Verification.** `crates/chv-nwd-core/tests/nwd_daemon.rs` covers restart →
policy update, policy-guard recovery plus re-scope on attach, delete removing
both journals, the idempotent path repairing a deleted journal entry, the
fabric exclusion, and two guardrails the review added: an unapplicable
firewall/NAT payload is rejected **before** it is journaled (a journaled
poison entry would crash-loop recovery), and a persisted entry that still
cannot be re-applied fails with an error naming the exact journal file.
`crates/chv-nwd-core/src/firewall.rs` unit tests pin the pure validators.
`crates/chv-agent-core/src/network_replay.rs` covers derivation, merge
semantics, and the skip-already-owned filter.

## B9 follow-ups included in rc.8

- **F1 — persistent DHCP reservations.** The `dhcp-hostsfile` now lives under
  `/var/lib/chv/nwd/dhcp/`, is created only when absent, and is removed only
  by topology teardown. A failed unlink fails the delete closed (the
  idempotent retry clears it) instead of leaving a MAC→IP map a re-created
  network would inherit. Lease files intentionally stay under `/run` (B4).
- **F2 — durable state follows teardown.** `install.sh --wipe`/re-run
  cleanup, the packaged `preremove.sh` (remove/purge only, never upgrade),
  and `dev-install.sh` remove `${CHV_DATA_DIR:-/var/lib/chv}/nwd`.
  `scripts/integration/qual/m4.4-network.sh` asserts the persistent hosts
  path.

## Upgrade order and one-time gap

The package scripts deliberately do not restart services.
`chv-agent.service` already orders `After=chv-nwd.service`, so
`systemctl restart chv-nwd chv-agent` is the documented rc.7 → rc.8 upgrade
step; nwd must be restarted before or with the agent so the startup replay
reaches rc.8 nwd and seeds the journal. Reservations written by rc.7 under
`/run/chv/nwd` are not migrated (one-time gap): the upgrade restart
re-creates the topology but not the per-NIC reservations; a DHCP guest keeps
its current lease until renewal, and a VM re-create (or a clean rc.8 install)
repopulates the persistent file. The upgrade drill uses cloud-init static
addressing; the reservation-survives-restart contract is exercised on the
clean install.

## Claim boundary

No field-window day has run and no stable tag exists. These are pre-window
findings from a published artifact, not field results. The rc.7 tag and
assets remain published and unchanged; rc.8 is the successor. Nothing here
claims that rc.8 fixes the defects on a published artifact until the rc.8
upgrade, clean install, DHCP lease, policy-change-plus-restart, and two
consecutive `--wipe` drills run after publication. That claim is made only by
the rc.8 drill evidence record, not by this report.

## Evidence index — rc.7 pre-window drills

All paths below are under
`artifacts/05-rc7-prewindow-drills/`. Logs were copied from the field host and
scrubbed for credentials and management addresses; only `127.0.0.1` (the BFF
loopback), the drill subnet `10.200.0.0/24`, and the dnsmasq test options
(`1.1.1.1`, netmask `255.255.255.0`) appear.

- `01-pre-upgrade.log` — pre-upgrade host state, rc.7 artifact presence, and
  the service/config baseline.
- `02-pre-upgrade-workload.log` — operator login proves the password hash and
  role survived the write; running-VM baseline.
- `03-rc6-tun-workaround.log` — the rc.6 `/dev/net/tun` residue/cleanup path
  before the rc.7 upgrade.
- `04-rc7-package-upgrade.log` — the published rc.7 `.deb` upgrade.
- `05-unit-shadow-and-restart.log` — the tarball `/etc` unit shadows the
  package unit; the services restarted with the rc.7 units.
- `06-post-upgrade-verification.log` — config/certificate hashes, node id,
  database rows, operator login, and the adopted running VM survived.
- `07-legacy-bridge-removal.log` — one-time removal of the rc.6 `chvbr0`;
  exactly one owner of `10.200.0.1`.
- `08-policy-change-and-b4-on-upgraded-host.log` — live policy update; no
  CHV dnsmasq appears (B8); nft policy is applied.
- `09-nwd-restart-contract-repro.log` — a VM create materializes the
  topology; nwd has no dnsmasq or durable topology state before the B9
  restart.
- `10-b9-repro.log` + `10-b9-repro.sh` — the B9 sequence: policy update
  succeeds, nwd restarts (PID `133166` → `174335`), no re-ensure, the next
  update exhausts retries and fails `DISPATCH_FAILED` / `NOT_FOUND`; includes
  the `pgrep -c dnsmasq` correction.
- `11-b8-capability-repro.log` + `11-b8-capability-repro.sh` — B8 in both
  directions: the rc.7 cap set fails to bind UDP/67 with the exact dnsmasq
  error; the rc.8 cap set binds exclusively to the bridge.

The committed logs are the raw pre-publication drill output; the rc.8
post-publication drill evidence is a separate record created only after the
rc.8 drill runs.
