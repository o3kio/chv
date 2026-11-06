# ADR-020: Per-VM Systemd Supervision and Exclusivity

## Status

Proposed

## Date

2026-07-21

## Implementation status

This ADR is a proposal only. It does not authorize production wiring and no
current VM launch, recovery, stop, or cleanup behavior changes as a result of
this document. `chv-agent` remains the only CellHV Core runtime authority and
the existing `chv-agent-runtime-ch` process path remains in force until every
acceptance condition below is met and this ADR is separately accepted.

The current ownership observer deliberately reports
`DuplicateEvidence::Indeterminate`; absence in a bounded `/proc` scan is not
proof of exclusivity. The proposed protocol supplies the missing positive
supervision boundary. It is not implemented by the existing
`packaging/systemd/chv-agent.service`, which supervises the agent as a whole
(`KillMode=process`; live VMMs are adopted by the restarted agent rather than
supervised individually), but has no per-VM unit or cgroup contract.

## Context

ADR-016 makes `chv-agent` and CellHV Core the same runtime. ADR-017 requires
Core to remain the single VM mutation and recovery authority. Recovery must
therefore prove that one durable VM generation corresponds to exactly one
legitimate Cloud Hypervisor process before adopting or controlling it.

PID, start time, boot ID, executable inode, credentials, socket inode, peer
credentials, API liveness, and cgroup membership can prove the identity of a
process that was observed. They cannot prove that an unobserved duplicate does
not exist. Linux `/proc` enumeration has no atomic absence snapshot. Systemd
`MainPID` alone also does not exclude an independently launched process.

The installed node already runs `chv-agent` as the unprivileged `chv` user in
`packaging/systemd/chv-agent.service`, with KVM group access and a private Core
state directory. Packaging installs separate `chv-stord` and `chv-nwd`
services. Those are attachment providers, not alternative VM authorities.
Per-VM supervision must preserve these boundaries and must not introduce a
`cellhvd`, helper daemon, second database, or second operation engine.

## Decision proposed

### Authority and launch protocol

1. `chv-agent` remains the sole caller authorized to accept a VM lifecycle
   operation, allocate a runtime generation, and request creation or removal
   of its supervised scope. Systemd is a process supervisor and resource
   boundary, not a VM lifecycle authority.
2. The durable Core journal is the authority for launch intent. Before any
   systemd request, Core commits a unique tuple of `host_id`, `vm_id`,
   `runtime_generation`, `operation_id`, `active_attempt_token`, and accepted
   configuration fingerprint.
3. A systemd unit name is a deterministic, injective encoding of the stable
   host-local VM identity and runtime generation. The encoding is generated
   from validated identifiers, escaped with systemd's unit-name rules, bounded
   to the platform limit, and never accepts caller-provided unit text. Hash
   shortening, if required, includes a domain separator and collision check
   against the durable tuple. One proposed shape is
   `cellhv-vm-<vm-key>-<generation-key>.service`; the final encoding is an
   implementation artifact that requires hostile collision tests.
4. `chv-agent` asks PID 1 to create and start exactly one non-delegated service
   unit for that tuple. Unit creation uses a no-replace semantic: an existing
   unit is inspected and accepted only when all durable identity properties
   match exactly. A conflicting or unverifiable unit blocks the VM.
5. The unit starts Cloud Hypervisor directly. It does not start another CellHV
   daemon or operation worker. `chv-agent` retains the Cloud Hypervisor API
   control path and attachment coordination with `chv-stord` and `chv-nwd`.
6. Readiness is established only after systemd unit identity, `MainPID`,
   `ControlGroup`, non-delegated cgroup membership, pidfd/process identity,
   owner marker, socket inode and peer credentials, and the Cloud Hypervisor
   API probe all match before and after the probe. The durable journal may then
   record launch completion using the active attempt token.
7. On restart, `chv-agent` reconstructs control from the durable tuple and PID
   1. It never adopts a unit by VM ID alone. Generation, operation attempt,
   configuration fingerprint, unit properties, cgroup, process, owner marker,
   socket, and API evidence must agree. Any incomplete or conflicting evidence
   is `AmbiguousPreserve` or `DuplicateConflict`, never positive ownership.
8. Stop, kill, restart, and unit removal are requested only by the same Core
   operation engine after token-fenced journal admission. Completion is
   revalidated against the same generation. Cleanup never unlinks a socket,
   removes a runtime directory, or resets a unit based only on a pathname or
   unit name.

### Systemd mechanism

The preferred mechanism is a transient `.service` unit created through the
systemd manager API with explicit immutable identity properties. Required
properties include a direct `ExecStart`, `User=chv`, `Group=chv`, KVM device
access, a non-delegated cgroup, `KillMode=control-group`, bounded stop time,
restart behavior selected by the durable operation policy, and environment or
credentials containing no secret or caller-controlled unit directives.
Durable identity values must be represented in inspectable unit properties
and in the owner marker; they must not rely only on the transient unit
description.

A packaged template unit such as `cellhv-vm@.service` is the fallback design
if the target systemd versions cannot supply and atomically inspect the
required transient properties. A template reduces dynamically supplied unit
configuration but needs an agent-owned, descriptor-safe manifest lookup keyed
by the escaped instance. The template must not read an arbitrary path from
`%i`, and instance activation must still be authorized by a committed Core
journal attempt. Template and transient modes cannot be mixed for the same
host authority epoch without an explicit migration record.

A delegated scope beneath `chv-agent.service` is rejected for the initial
protocol. Delegation lets the agent place processes directly and weakens PID
1's role as the only legitimate launcher. A single shared VM cgroup is also
rejected because it cannot bind one process set to one durable generation.

### Trust model

The positive exclusivity claim is bounded to an intact host security boundary:

- PID 1 and the systemd manager API are trusted;
- the Core database, private runtime roots, unit definitions/manifests, and
  owner markers are writable only by their designated principals;
- unprivileged tenants cannot run as `chv`, access the systemd authorization
  path, write Core state, or create entries in CellHV runtime roots;
- the agent's systemd authorization permits only the bounded CellHV per-VM
  operation set and unit namespace;
- same-UID arbitrary code, root compromise, systemd compromise, kernel
  compromise, or direct privileged cgroup manipulation is outside the
  exclusivity guarantee.

If same-UID hostile code is in scope, systemd and cgroups are insufficient.
Production must first add and qualify a stronger boundary such as a distinct
launcher identity, systemd-polkit mediation, and MAC confinement. Documentation
and compatibility claims must state which threat model was tested.

### Compatibility and fallback

The default remains the current direct child-process implementation while this
ADR is proposed. There is no automatic fallback from supervised launch to
direct launch after a supervised journal intent has been committed: that could
create two processes. Hosts incapable of the accepted systemd protocol remain
on an explicitly selected legacy authority mode or are unsupported for native
Core recovery.

Existing directly launched VMs are preserved across rollout. They are not
silently moved into a unit or killed. A drain-and-recreate migration is the
initial safe path. A future live adoption protocol requires its own accepted
decision and real-KVM evidence. Downgrade must refuse to manage supervised
generations unless the host has been drained and durable supervision records
have been retired transactionally.

`chv-stord` and `chv-nwd` remain separately packaged providers. Their systemd
units do not launch, adopt, stop, or recover Cloud Hypervisor processes.

## Failure behavior

| Failure | Required result |
|---|---|
| systemd unavailable, API timeout, or authorization denied before unit creation is proven | journal attempt remains inspect-required; no direct-launch fallback |
| unit name already exists with an exact durable tuple | inspect and resume the same attempt; never start a second unit |
| unit name exists with different or unreadable identity | quarantine the VM as a foreign conflict |
| unit created but Core crashes before observing start | restart inspects unit, cgroup, process, marker, socket, and token before resolving |
| process exits while unit remains | preserve exit evidence; resolve only through the token-fenced operation state machine |
| process escapes the expected cgroup or multiple members can act as the VMM | ambiguity or duplicate conflict; do not signal by PID |
| owner marker, unit properties, cgroup, PID, executable, socket, or API evidence disagree | preserve all evidence and block automated mutation |
| systemd loses transient-unit state while a process remains | ambiguous preserve; never direct-launch or unlink |
| agent is upgraded or restarted | VM unit remains under PID 1; new agent must revalidate the complete durable tuple |
| host reboots | only durable requested state may authorize relaunch; systemd enablement alone must not create a VM |
| stop times out | systemd control-group result and surviving membership are inspected; no success is journaled while membership is ambiguous |

## Rollout

1. Keep the proposal documentation-only and the Linux observer fail-closed.
2. Implement an unwired systemd-manager interface and pure unit-property
   validator beneath `chv-agent`; add no new daemon or production unit.
3. Add T1 deterministic tests for names, tuple binding, property validation,
   replay, conflict, downgrade, and every failure-table row.
4. Add T2 isolated-process tests against supported systemd versions, including
   authorization denial, manager restart, agent crash at every launch boundary,
   cgroup membership races, and transient-unit garbage collection.
5. Add an opt-in, default-off laboratory composition that consumes the same
   Core journal and runtime adapter. It must be mutually exclusive with direct
   launch for the entire host, not selected per request.
6. Run T3 real-KVM tests proving launch, restart re-adoption, stop, host reboot,
   upgrade, failed upgrade rollback, provider failure, and no duplicate process
   under retries and injected crashes.
7. Review security policy, supported systemd/cgroup versions, packaging,
   operator recovery, compatibility claims, and collected evidence. Accept
   this ADR only after that review.
8. Enable the supervised mode only through an explicit migration with a drain
   prerequisite. Retain rollback only to a drained, process-free host.

## Consequences

### Positive

- supplies a kernel/PID-1-backed supervision boundary that bounded process
  observation alone cannot provide;
- keeps lifecycle intent, retry, and recovery authority in `chv-agent`;
- gives each durable VM generation an inspectable process and cgroup identity;
- enables crash recovery without requiring a retained Rust `Child` handle.

### Negative

- adds a systemd D-Bus/API and policy dependency to the node runtime;
- requires explicit support and qualification by systemd and cgroups-v2
  version;
- cannot defend against root, PID 1, kernel, or arbitrary same-UID compromise
  without stronger confinement;
- requires drain-based migration for existing direct children initially;
- transient units and template units have different upgrade and persistence
  behavior that must be tested independently before either is supported.

## Rejected alternatives

### Treat a negative `/proc` scan as exclusivity

Rejected because enumeration has no atomic absence guarantee and bounded or
permission-limited scans must remain indeterminate.

### Trust only `MainPID` or one cgroup scan

Rejected because neither excludes an unregistered process, and membership can
change around observation without the complete launch and revalidation
protocol.

### Add a VM supervisor daemon

Rejected because it would create a second runtime service and lifecycle
authority, contrary to ADR-016.

### Let systemd restart VMs from enabled persistent units independently

Rejected because durable desired state and operation fencing belong to Core;
systemd must not become a second desired-state engine.

### Fall back automatically to direct launch

Rejected because uncertainty about unit creation followed by direct launch can
create a duplicate VM process.

## Evidence gates and acceptance conditions

This ADR may move from Proposed only when all of the following are reviewable:

- the authority lease proves one active `chv-agent` for the Core database;
- unit-name injectivity and hostile escaping tests cover all accepted VM and
  generation identifiers;
- unit creation has demonstrable no-replace/replay semantics correlated to the
  durable operation attempt token;
- unit, cgroup, `MainPID`, complete membership, pidfd, start time, boot ID,
  executable, credentials, owner marker, socket inode, peer credentials, and
  API liveness are revalidated as one bounded recovery decision;
- all observation limits and permission failures fail closed;
- crash injection covers every boundary before journal commit, unit creation,
  process start, marker publication, readiness, completion, stop, and cleanup;
- T2 covers each supported systemd/cgroups-v2 version and manager restart;
- T3 real-KVM evidence satisfies `CORE-IDEMP-001`, `CORE-OPS-001`, restart
  recovery, host reboot, upgrade/rollback, and duplicate-process negative
  assertions;
- packaging and authorization reviews prove no second daemon, broad unit
  creation privilege, independently enabled VM unit, or second VM database;
- downgrade and legacy-VM migration tests prove ambiguous state never triggers
  direct launch, stop, socket unlink, or runtime-directory deletion;
- the threat model and evidence tier are reflected truthfully in compatibility
  claims and operator documentation.

Until these gates pass and the ADR is accepted, `DuplicateEvidence::Exclusive`
must not be produced by the Linux production observer and production VM launch
and management behavior must remain unchanged.
