# Fresh-host qualification (#666 + #556) — v0.4.0-rc.5

**Status:** `PASS` — filled from the published-RC run
[42159563210](https://github.com/o3kio/chv/actions/runs/42159563210)
(tag `v0.4.0-rc.5`, 2026-11-06). All four legs are green, the evidence
artifact was downloaded and re-checked, and the published assets were
verified independently.

**Gate:** [#666](https://github.com/o3kio/chv/issues/666) requires the
documented install → operate → upgrade → uninstall path to be demonstrated on
a fresh host for the candidate. [#556](https://github.com/o3kio/chv/issues/556)
covers the published one-line quick installer.

**Candidate:** `v0.4.0-rc.5` (package version `0.4.0~rc.5`; published asset
version `0.4.0.rc.5`). The source `VERSION` file holds the base `0.4.0`
because `release.yml` takes the `rc.5` prerelease suffix from the tag. The
lane itself builds from the bare `VERSION`, so its local candidate tarball is
`chv-0.4.0-linux-amd64.tar.gz` and `--candidate-version` is `0.4.0`.
**Commit:** `d1f0e63c9bb1834cd274aa4af4279bac6bb4cc95`.
**Tag object:** `c87f3424259543e2b8b784e42f882403929b69ae` (annotated,
SSH-signed by `Zoorpha <aaron@kubedo.com>`; `git tag -v` reports
`ED25519 key SHA256:v903QFOI6qt9Gz5e7DZdZOyVNXUn4tKd/Fafyo0qj28`).

The `install-tarball`, `install-packages`, and `upgrade` legs build candidate
artifacts locally from that commit; the `quick-install` leg fetches
`scripts/quick-install.sh` from the pinned tag commit and installs the
**published** `v0.4.0-rc.5` release assets (its binaries report the release
build's short commit `d1f0e63`). The pinned quick-install script
(`f384c797c7cbccb8b3834789a42e128a938ce78649bdae4a378f43b87de5902c`) is
byte-identical to `scripts/quick-install.sh` at the tag.

**Runtime lineage:** the rc.5 runtime is unchanged from rc.2/rc.3/rc.4. The
full real-KVM lane
[42067288852](https://github.com/o3kio/chv/actions/runs/42067288852) is green
(m4.3–m4.8 all `rc=0`) on the rc.2 product+harness tree `c1effc71`. rc.5
changes only the repo-only quick installer (#727), test-only qualification
invariants, and documentation. The fresh-host lane does not execute VMs and
does not add a real-KVM claim.

---

## 1. Environment (honest label)

A clean Ubuntu noble amd64 root filesystem and systemd inside a debootstrap +
systemd-nspawn container on the self-hosted qualification runner.

- It shares the **host kernel** and the **host network namespace**.
- It has **no `/dev/kvm`**.
- It is **not a bare-metal host and not a cloud VM**.

This is a container demonstration of the installer, service, packaging, state,
and uninstall contract. It is deliberately not described as a fresh-machine
qualification for VM execution; the real-KVM m4.3–m4.8 lane (#659) covers VM
execution. The host's own CHV deployment is never touched: every leg unpacks a
new root under `CHV_QUAL_ROOT` and the host ports are checked free before boot.

- Runner: `runner-2404` (labels `self-hosted, linux, x64, chv-kvm`).
- Run: [42159563210](https://github.com/o3kio/chv/actions/runs/42159563210)
  (8m56s overall; the single job ran 2026-11-06T03:57:04Z → 04:05:56Z).
- Artifacts: `fresh-host-qualification-42159563210`,
  `fresh-host-qualification-42159563210-results`.

## 2. Lane inputs

| Input | Value | Why |
|---|---|---|
| `previous_release` | `v0.3.0` | Latest stable; the upgrade leg's starting point |
| `quick_install_version` | `0.4.0-rc.5` | Equals the candidate release, so the workflow sets `--quick-install-candidate` and the quick leg runs the **full candidate contract**, including the credential-persistence gate |
| `skip_upgrade` | `false` | The upgrade leg ran |
| `--quick-install` | `d1f0e63c9bb1834cd274aa4af4279bac6bb4cc95` | Pinned tag commit, not a floating branch |

This is the mode the rc.3 lane lacked. At the rc.3 harness revision the
persistence gate ran only when `QUICK_VERSION` equalled `CANDIDATE_VERSION`
exactly. The lane passed `QUICK_VERSION=0.4.0-rc.3` against
`CANDIDATE_VERSION=0.4.0`, so the equality failed, and `sort -V` ranks the
`-rc.3` prerelease *after* `0.4.0`, so the lane fell through to the fallback
branch: candidate bring-up (including node enrollment) but **no**
credential-persistence check. #725 added the explicit
`--quick-install-candidate` flag so an `-rc.N` quick version triggers the full
contract instead of relying on string comparison.

## 3. Artifact digests

### 3.1 Published `v0.4.0-rc.5` assets (downloaded from the GitHub release)

SHA-256 of the release tarball (matches its `.sha256` sidecar):

```text
fcfcb73a55f0964502bdcb22e777154a53889292fd971882b52cc7c8444e5dad  chv-0.4.0-rc.5-linux-amd64.tar.gz
```

`SHA256SUMS` (verified with `sha256sum -c --strict`; all eight OK):

```text
55f9d1026e586c674fef0a6786ea948949fb22dbf71509a8ac7147f4c1b687b7  chv-controlplane_0.4.0.rc.5_amd64.deb
fd9820f5b5049689547adaa8018aca1201e78d9d14026f88f63b010bb432a21a  chv-monitor-agent_0.4.0.rc.5_amd64.deb
dbd3a9c58d889da0065c3671fa0eb48d1f22d1dbbd6bdec2a3f0347db4fb7f51  chv-node_0.4.0.rc.5_amd64.deb
96791ad476f5957c1b167957c26572b0da6589525ba020507daef3f1950b1b5b  chvctl_0.4.0.rc.5_amd64.deb
6beb86c1be8dab2bd903b80ca312d48bd280d8675e861d2a646b7a11fd460aaa  chv-controlplane-0.4.0.rc.5-1.x86_64.rpm
cae3f9f4031c3c99aaee7f3b5cc7151bfa879361d6f895c36671dfa13a3b68c0  chv-monitor-agent-0.4.0.rc.5-1.x86_64.rpm
a7145cfe0c02461f14aa64917b61c80034d9bb14faf404997c90bd7b26bab151  chv-node-0.4.0.rc.5-1.x86_64.rpm
b716c2956b17dc73bab4dfe1df9c24f8690ab109887281dfb83c4d3294a690af  chvctl-0.4.0.rc.5-1.x86_64.rpm
```

The release has 13 assets (`isPrerelease: true`, target `main`): the tarball,
its sidecar, four `.deb`, four `.rpm`, `SHA256SUMS`, and two SBOMs. The
extracted `chv-controlplane` reports:

```text
chv-controlplane 0.4.0 (commit d1f0e63, build 2026-11-06, channel stable)
```

The release workflow [42156752816](https://github.com/o3kio/chv/actions/runs/42156752816)
completed successfully: security preflight, the required quality-gate jobs,
build, package, sign, release, and package-repository publish all succeeded
(the `Rust coverage` and `Real-BFF E2E` jobs were skipped in that run).

### 3.2 Lane-built candidate artifacts

These are the artifacts the lane built from `d1f0e63c`, not the published
release assets; their digests intentionally differ.

```text
candidate_tarball  1faa284973d87c913a22fcc1dadaeaec80dd419f9f0f13c4c89d0781660ec02e  chv-0.4.0-linux-amd64.tar.gz
a6bb31fb162ceba6cd13d3bcd5e9a2ce6b609c0784e6067c6b4bcd56b3774f81  chv-controlplane_0.4.0_amd64.deb
a1433272282220269226efaf0120d581c74989674a189ed638391b915d8f4704  chv-monitor-agent_0.4.0_amd64.deb
3b0ab704e38abdcdc7e79de33766665930859e2bb134e196f300baa2031cefd1  chv-node_0.4.0_amd64.deb
24be90918c11185335a000065117b41ae164d4362bf9180cb8254d0c08ef60ac  chvctl_0.4.0_amd64.deb
previous_tarball   45990c38f9ba0a1ca573e0c7549479464ff6a0eb763da6ecec4bf1eb376eea0d  chv-0.3.0-linux-amd64.tar.gz
quick_install_script f384c797c7cbccb8b3834789a42e128a938ce78649bdae4a378f43b87de5902c  quick-install-d1f0e63c9bb1834cd274aa4af4279bac6bb4cc95.sh
```

## 4. Leg results

| Leg | Status | rc | Duration | What it asserted |
|---|---|---|---|---|
| `install-tarball` | pass | 0 | 66 s | Lane-built candidate `install.sh` from the tag commit; bring-up, login, enrollment, WebUI, metrics; `--uninstall` then `--purge`; every CHV port released |
| `install-packages` | pass | 0 | 22 s | Lane-built candidate `.deb` install on a clean root; packaged units present and fail-closed without certs; packaging contract |
| `upgrade` | pass | 0 | 92 s | `v0.3.0` install, identifiable state, lane-built candidate package upgrade, state preserved, node/agent version follows the new build |
| `quick-install` | pass | 0 | 31 s | Published `scripts/quick-install.sh` at the pinned tag, installing the published `0.4.0-rc.5` assets under the full candidate contract |

All four legs report `LEG_FAILURES=0`, `LEG_RC=0`.

## 5. Quick-leg detail — the rc.4 no-enrollment defect is exercised

`v0.4.0-rc.4` failed this leg with `FAIL node enrolled and visible (timeout)`
(run [42148690329](https://github.com/o3kio/chv/actions/runs/42148690329)):
the installer returned `rc=0` but no node was ever enrolled. rc.5 returned
`rc=0` and passed the assertions that defect broke:

```text
=== published quick installer (pinned ref d1f0e63c9bb1834cd274aa4af4279bac6bb4cc95) ===
PASS curl available for the quick installer
QUICK_INSTALL_RC=0
PASS quick installer returned rc=0 (ref d1f0e63c9bb1834cd274aa4af4279bac6bb4cc95, release 0.4.0-rc.5)

=== bring-up contract ===
PASS four CHV units active
UNIT chv-controlplane ActiveState=active SubState=running NRestarts=0
PASS chv-controlplane active/running
PASS chv-controlplane zero restarts
UNIT chv-agent ActiveState=active SubState=running NRestarts=0
PASS chv-agent active/running
PASS chv-agent zero restarts
UNIT chv-stord ActiveState=active SubState=running NRestarts=0
PASS chv-stord active/running
PASS chv-stord zero restarts
UNIT chv-nwd ActiveState=active SubState=running NRestarts=0
PASS chv-nwd active/running
PASS chv-nwd zero restarts
ENCRYPTION_ENV=600 root:root
PASS encryption.env is 0600 root:root
PASS CHV_ENCRYPTION_KEY is 64 hex chars
PASS control plane loaded the dedicated encryption key
PASS admin login round-trip
PASS first-login password change accepted
PASS admin re-login with the rotated password
PASS WebUI served on http://127.0.0.1:8080/
NODES_HTTP=200
NODES={"filters":{"applied":{}},"items":[{"active_tasks":0,"alerts":0,"cluster":"","cpu":"12","health":"Degraded","hypervisor_capabilities":[],"maintenance":false,"memory":"31.3 GiB","name":"fresh-host-quick-install","network":"","node_id":"8e94e033-4e01-4d56-90f6-39d288c5ee9b","state":"HostReady","storage":"","version":"0.4.0"}],"page":{"page":1,"page_size":50,"total_items":1,"total_pages":1}}
PASS node enrolled and visible
PASS controlplane /metrics is HTTP 200
PASS controlplane /metrics body is non-empty
PASS agent /metrics is HTTP 200
PASS agent /metrics body is non-empty
PASS stord /metrics is HTTP 200
PASS stord /metrics body is non-empty
PASS nwd /metrics is HTTP 200
PASS nwd /metrics body is non-empty

=== credential persistence across control-plane restart ===
PASS control plane restarted
PASS control plane came back with a new process (804 -> 981)
PASS rotated admin credential survived the restart
PASS control plane holds the on-disk database WAL after the restart
VERSION chv-controlplane=chv-controlplane 0.4.0 (commit d1f0e63, build 2026-11-06, channel stable)
PASS chv-controlplane reports version 0.4.0
VERSION chv-agent=chv-agent 0.4.0 (commit d1f0e63, build 2026-11-06, channel stable)
PASS chv-agent reports version 0.4.0
VERSION chv-stord=chv-stord 0.4.0 (commit d1f0e63, build 2026-11-06, channel stable)
PASS chv-stord reports version 0.4.0
VERSION chv-nwd=chv-nwd 0.4.0 (commit d1f0e63, build 2026-11-06, channel stable)
PASS chv-nwd reports version 0.4.0
LEG_FAILURES=0
LEG_RC=0
```

The `NODES=` line records node `8e94e033-4e01-4d56-90f6-39d288c5ee9b`
at version `0.4.0`, with the first-sighting `HostReady`/`Degraded` reading
explained in §6. The node row proves enrollment; the restart assertions prove
the #725 durability fix (the rotated credential and the WAL survive a
restart), so this exercises the credential-persistence gate the rc.3 lane did
not run.

The lane installs once, so it does **not** exercise the documented re-run path.
That path is covered separately by local clean-guest verification (see §7).

## 6. The transient `Degraded` / `HostReady` node reading

The node row captured at the first sighting reports:

```json
{"health":"Degraded","state":"HostReady","node_id":"8e94e033-...","version":"0.4.0"}
```

The rc.3 lane reported `Healthy` / `TenantReady`. This difference is a timing
artifact of the rc.5 ordering fix, not a runtime regression:

1. The agent reports `health_status = "Healthy"` **only** when its state is
   `TenantReady`; every other state reports `"Degraded"`
   (`cmd/chv-agent/src/main.rs`, the `reporter.node_state_report` call).
2. The agent climbs one state per 5 s reconcile tick:
   `Bootstrapping → HostReady → StorageReady → NetworkReady → TenantReady`,
   gated on `stord` and `nwd` probes (`crates/chv-agent-core/src/reconcile.rs`).
3. rc.5 starts the agent **after** the offline admin seeding, immediately
   before the harness's node check. `check_node_visible` returns on the first
   response containing `"node_id"`, so it records the ladder mid-climb. rc.3
   started the agent earlier in the install, so its check landed after the
   climb had completed.

**Reproduction on a pristine guest.** I unpacked the cached debootstrap noble
base (the same base the lane uses), installed the **published** `0.4.0-rc.5`
release with the **published** quick-install script, and measured the state:

```text
quick install: rc=0 at 04:26:41; agent unit started 04:26:40
wait t=2s  HostReady    Degraded
wait t=6s  NetworkReady Degraded
wait t=12s TenantReady  Healthy
final: node_observed_state = TenantReady, Healthy
final: chv-controlplane/agent/stord/nwd = active
cache: "node_state": "TenantReady"
```

This guest has no `/dev/kvm` (same container boundary). The node converges to
`Healthy` / `TenantReady` within ~12 s of the agent starting; it was not stuck.
The lane's node-visibility assertion is about enrollment, which is exactly what
the rc.4 defect broke, and it passed.

This is a local clean-guest reproduction on 2026-11-06, not a second CI lane.
It is recorded here so the lane's first-sighting reading is not mistaken for a
health defect. No product change was needed.

## 7. Re-run / upgrade evidence (outside this lane)

The lane installs once. The documented re-run path — the second rc.4 defect —
was verified separately in #727 on clean Ubuntu noble nspawn guests with the
patched script and the rc.4 assets:

- **Healthy re-run:** the enrolled identity is preserved (same `node_id`),
  four units active with `NRestarts=0`, 0 peer-node-id mismatches, and the
  agent logs `external chv-stord/nwd daemon is already listening ... skipping
  sub-process spawn` (no duplicate daemons).
- **Unusable-cache recovery:** the installer detects the stale identity,
  mints and seeds a fresh bootstrap token, and re-enrolls
  (`enrollment_complete=true`, node row updated, 0 peer mismatches).
- **Fresh install:** enrolls, with no deleted WAL fds and a surviving admin
  rotation.

The `upgrade` leg here covers `v0.3.0 → 0.4.0-rc.5` through the package path,
not a quick-installer re-run. The re-run claim rests on the local verification;
it is not a field or real-KVM claim.

## 8. rc.3 / rc.4 / rc.5 comparison

| Run | Tag | tarball | packages | upgrade | quick | Quick mode |
|---|---|---|---|---|---|---|
| [42139285660](https://github.com/o3kio/chv/actions/runs/42139285660) | `v0.4.0-rc.3` | pass | pass | pass | pass | candidate bring-up but no persistence gate — not evidence for the defect |
| [42148690329](https://github.com/o3kio/chv/actions/runs/42148690329) | `v0.4.0-rc.4` | pass | pass | pass | **fail** | candidate-mode; `node enrolled and visible (timeout)` |
| [42159563210](https://github.com/o3kio/chv/actions/runs/42159563210) | `v0.4.0-rc.5` | pass | pass | pass | **pass** | candidate-mode; enrollment + credential persistence |

## 9. What this establishes, and what it does not

**Establishes (container contract only):**

- The rc.5 tarball/package install, operate, package upgrade, and uninstall
  path, on lane-built candidate artifacts from the tag commit.
- The published rc.5 quick installer on a fresh guest: enrollment, admin
  rotation, credential persistence across a control-plane restart, WAL
  retention, WebUI, all four metrics endpoints, and unit health.
- The rc.4 no-enrollment defect is fixed on the fresh-install path.

**Does not establish:**

- Bare-metal or cloud-VM behavior, `/dev/kvm` VM execution, or host reboot
  recovery.
- The quick-installer re-run path (covered locally, §7) or a rolling upgrade /
  downgrade (unsupported).
- Multi-host operation, HA, live migration, backup/restore, Ceph/iSCSI,
  Volvisor, RPM-on-real-host, arm64, scale, or any field evidence.
- Stable status: `v0.4.0-rc.5` is a pre-release and the latest stable remains
  `v0.3.0`. The field campaign has not run.

No statement here widens the
[v0.4.0-rc.5 support matrix](../../../release/v0.4.0-rc.5-support-matrix.md).

## 10. Evidence files

The `fresh-host-qualification-42159563210` artifact contains:

```text
summary.md                     run summary, environment label, digests
results.tsv                    per-leg status/rc/duration
candidate-digests.txt          lane-built artifact digests
artifacts.sha256               lane artifact digests (candidate, previous release, pinned quick-install script)
install-tarball-results.txt    per-assertion results
install-tarball-boot.log
install-packages-results.txt
install-packages-boot.log
upgrade-results.txt
upgrade-boot.log
quick-install-results.txt
quick-install-boot.log
quick-install-d1f0e63....sh    pinned published quick-install script
```

The local convergence reproduction used the cached
`/var/lib/chv/qual/nspawn-base-noble.tar.gz` base, the published
`chv-0.4.0-rc.5-linux-amd64` assets, and the published quick-install script;
its guest root was retained under `/tmp/opencode/exp16` at the time of
writing. It is a workstation run, not the shared runner.
