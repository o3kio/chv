# Service-User Isolation: `chv-agent`, `chv-stord`, and `UMask`

Status: decision record for the current release. **The systemd units are not
changed by this document.** It records the shipped model, its compensating
controls, and the preconditions for a future dedicated storage user or a
tighter umask.

## Current model

| Piece | Shipped setting | Evidence |
|---|---|---|
| `chv-agent` service user | `User=chv`, `Group=chv`, `SupplementaryGroups=kvm` | `packaging/systemd/chv-agent.service` |
| `chv-agent` umask | `UMask=002` | same unit |
| `chv-agent` runtime dir | `RuntimeDirectory=chv/agent`, `RuntimeDirectoryMode=0775` | same unit |
| `chv-agent` state dir | `StateDirectory=chv/agent`, `StateDirectoryMode=0700` | same unit |
| Agent-created Core/state dirs | `/run/chv/core`, `/var/lib/chv/agent`, `/var/lib/chv/cache` created `0700 chv:chv` by `ExecStartPre` | same unit; `packaging/tmpfiles/chv-node.conf` |
| `chv-stord` service user | `User=chv`, `Group=chv` — **not** `chv-stord` | `packaging/systemd/chv-stord.service` |
| `chv-stord` umask | Unit sets no `UMask`; systemd's default `0022` applies | same unit (no `UMask=` directive) |
| `chv-stord` runtime/state | `RuntimeDirectory=chv/stord`; `StateDirectory=chv`; `ReadWritePaths` limits writes to `/run/chv/stord` and `/var/lib/chv/storage`, plus the state-directory root `/var/lib/chv` | same unit |
| Storage directory contract | `/var/lib/chv/storage/{localdisk,lvm}` = `chv:chv-stord 0770` | `packaging/scripts/postinstall.sh`; `scripts/install.sh` |
| VM runtime directory contract | `/var/lib/chv/agent/vms` = `chv:chv-stord 0775` | `scripts/install.sh`; `packaging/scripts/postinstall.sh` |
| Reserved storage user/group | `chv-stord` user and group are created even though the unit does not run as them | `packaging/scripts/postinstall.sh`; `scripts/install.sh` |
| Group memberships | `chv` is added to `chv-stord`; `chv-stord` is added to `chv` and `disk` | same scripts |
| `stord.toml` | `scripts/install.sh` writes `root:chv-stord 0640`; the packaged config can be hardened to the same. The agent reads it through `chv-stord` group membership. | `scripts/install.sh`; `packaging/scripts/postinstall.sh` |
| Agent API socket | mode `0600`, set explicitly after bind | `crates/chv-agent-core/src/agent_server.rs` |
| stord API socket | mode `0600` | `crates/chv-stord-core/src/server.rs` |

### Why `chv-stord` runs as `chv`

The shipped unit comment gives the operational reason: the stord API socket is
mode `0600` with `chv-agent` as the only client, and Cloud Hypervisor (spawned
by `chv-agent` as `chv`) must read and write volume files under
`/var/lib/chv/storage`. Running stord as the isolated `chv-stord` user would
require a group-accessible socket and group-writable volumes. That is the
documented future direction, not the shipped model.

In other words, the storage daemon and the VM runtime currently share a UID so
that the VMM can open the volume files the daemon prepares without a
permission-translation layer. The `chv-stord` group and user exist as the
isolation seam for changing that later.

### What `UMask=002` does and does not do

`UMask=002` makes agent-created files `0664` and directories `0775` by
default: group-readable/writable, world-readable, not world-writable. It keeps
group-shared artifacts usable without a timing-dependent `chmod`. It does
**not** by itself make Core state confidential — the `0700` directory modes
(`/run/chv/core`, `/var/lib/chv/agent`, `/var/lib/chv/cache`,
`StateDirectoryMode=0700`) and the `0600` socket/cache files are the primary
controls. Tightening the umask alone would not close the same-UID gap; it
would only remove world-read permission from newly created artifacts.

## Compensating controls in the current model

- **systemd sandboxing.** Both units set `NoNewPrivileges=true`,
  `ProtectSystem=strict`, `ProtectHome=true`, and narrow `ReadWritePaths`:
  the agent may write `/run/chv` and `/var/lib/chv`; stord's explicit writes
  are `/run/chv/stord` and `/var/lib/chv/storage`, on top of its
  `StateDirectory=chv` root `/var/lib/chv`. stord has no capabilities;
  the agent has `DeviceAllow=/dev/kvm rw` and `SupplementaryGroups=kvm`.
- **Socket access is owner-only.** The agent API socket and stord API socket
  are `0600`. The agent's gRPC surface also has fail-closed application-layer
  authentication and a peer-UID allowlist (#662); the socket mode is not the
  only gate.
- **State-directory separation by mode.** Core state and the agent cache are
  under `0700` directories and `0600` files, so other host users cannot read
  them. The same-UID stord process still can; this is the residual risk.
- **Group seam.** `chv-stord` group ownership on storage/VM directories and
  the reserved `chv-stord` user make a future UID split a permission-contract
  change rather than a redesign.
- **Device separation.** Only the agent is in `kvm`; only `chv-stord` is in
  `disk`. The storage daemon does not get KVM access, and the agent does not
  get block-device group access.
- **Hardened config sharing.** `stord.toml` is written `root:chv-stord 0640`
  by `scripts/install.sh` and may be hardened to the same on packages; the
  agent reads it through group membership for respawn config fidelity (#385)
  without making it world-readable.

## Residual risk

Because `chv-stord` and `chv-agent` share UID `chv`:

- a compromised stord process can read agent Core state, the legacy cache,
  and the agent API socket (all owned by `chv`);
- a compromised agent process can read/write storage files and the stord
  socket;
- the systemd `ReadWritePaths` sandbox limits *writes* outside each unit's
  paths but is not a read-confidentiality boundary between same-UID
  processes.

This is accepted for the current release because changing it requires a
coordinated socket/volume permission contract and re-qualification (below),
and no known advisory at the time of this decision requires the split.

## Decision

**Keep the current service-user model for this release.** Do not change
`chv-agent.service` or `chv-stord.service` as part of this decision. The
`chv-stord` user/group remains a reserved seam, and the current unit comments
remain the source of truth for why stord runs as `chv`.

## Preconditions for a future dedicated `chv-stord` user

A follow-up change must satisfy all of these before switching
`chv-stord.service` to `User=chv-stord Group=chv-stord`:

1. **Socket contract.** Make the stord API socket group-accessible (for
   example `0660 root:chv-stord`, or `chv-stord:chv-stord`) and ensure
   `chv-agent` is in the `chv-stord` group and can traverse the socket
   directory. The current `0600` socket cannot be used by a different UID.
2. **Volume contract.** Ensure volume/disk files created by stord are
   group-readable and group-writable by `chv` (Cloud Hypervisor runs as
   `chv`), and that stord can read files created by `chv`. The existing
   `chv:chv-stord 0770` directories are the starting point; file modes and
   the stord umask (for example `UMask=0007`) must be verified, not assumed.
3. **Traversal and config.** Keep `/var/lib/chv` traversable by the
   `chv-stord` group, keep `stord.toml` `root:chv-stord 0640`, and confirm
   the agent's group membership is present before the agent starts.
4. **Device access.** Keep `chv-stord` in `disk` (and any backend-specific
   groups) and verify LVM/iSCSI/RBD operations under the new UID. Do not grant
   KVM to stord.
5. **systemd sandbox.** Re-check `ReadWritePaths`, `StateDirectory`,
   `RuntimeDirectory`, and `ProtectSystem` for the new UID; keep
   `NoNewPrivileges=true`.
6. **Install/upgrade parity.** Update both `packaging/scripts/postinstall.sh`
   and `scripts/install.sh`, plus `.deb`/`.rpm` smoke assertions
   (`scripts/package/smoke-common.sh` currently asserts
   `User=chv Group=chv UMask=002` for the agent and would need a matching
   stord assertion), and provide a migration path for existing file
   ownership on upgrade.
7. **Re-qualification.** Re-run the clean-install and lifecycle/KVM legs,
   including a VM create/start/delete that exercises stord volume open and
   Cloud Hypervisor disk access, and a restart/upgrade rollback leg. Treat it
   as a high-risk isolation change under `CONTRIBUTING.md` (PR disclosure +
   linked evidence).
8. **Documentation.** Update this document, `docs/DEPLOYMENT-ARCHITECTURE.md`,
   `docs/PACKAGING.md`, and the unit comments in the same change.

## Preconditions for a tighter umask

A lower-risk follow-up is to remove world-read from agent-created artifacts
(for example `UMask=007` while keeping group access), but only after:

1. Confirming no supported tooling, nginx configuration, console path, or
   operator script depends on world-readable `0664`/`0775` artifacts.
2. Re-running the packaging smoke and clean-install legs, because the
   installed directory modes are also set by `ExecStartPre`/tmpfiles and must
   stay consistent.
3. Recording that the `0700` state directories and `0600` sockets — not the
   umask — remain the confidentiality boundary.

## Follow-up trigger and issue

**Follow-up issue: [#700](https://github.com/o3kio/chv/issues/700)** (child of
#664) — *“Run `chv-stord` as the dedicated `chv-stord` user: socket and volume
permission contract.”*

Trigger the follow-up when any of these occurs, whichever comes first:

- a `chv-stord` compromise, privilege-escalation report, or storage-path
  advisory;
- a requirement that the storage daemon must not be able to read agent Core
  state or secrets;
- a storage-backend change that materially increases stord's attack surface
  (for example a network-facing Volvisor integration);
- the next major release planning cycle, and no later than the legacy removal
  target in the [authority-mode matrix](mode-feature-matrix.md) (`2027-06-30`),
  since removing the legacy coupling changes the permission assumptions.

Until the contract in #700 is accepted, this document is the decision record:
the current model ships as-is and the units must not be changed ad hoc.

## Evidence

- `packaging/systemd/chv-agent.service`
- `packaging/systemd/chv-stord.service`
- `docs/examples/systemd/chv-agent.service`
- `docs/examples/systemd/chv-stord.service`
- `packaging/scripts/postinstall.sh`
- `scripts/install.sh`
- `packaging/tmpfiles/chv-node.conf`
- `crates/chv-agent-core/src/agent_server.rs` (socket mode, application-layer auth)
- `crates/chv-stord-core/src/server.rs` (socket mode)
- `docs/PACKAGING.md`
- `docs/DEPLOYMENT-ARCHITECTURE.md` §2 (packaged systemd units)
