# `chv-stord` Component Spec

> **Transition status:** ADR-028 selects Volvisor as CHV's storage lifecycle
> and provider authority. This specification describes the shipped legacy
> storage path and remains binding for volumes explicitly assigned to
> `chv-stord`; it is not the target for new storage capabilities.

## Purpose
`chv-stord` is the current host-side storage service. After the ADR-028 owner
transition, it becomes the compatibility boundary for legacy-owned volume
attach/open/serve and lifecycle hooks.

## MVP-1 storage classes
- local qcow2/raw file
- local block/LVM
- iSCSI block (adapter implemented; selectable via `backend_type = "iscsi"`)
- Ceph RBD (adapter implemented; selectable via `backend_type = "ceph"`)

## Responsibilities
- open and close volumes
- attach and detach volumes for VMs
- expose block endpoints for Cloud Hypervisor integration
- resize hooks
- snapshot preparation hooks
- clone preparation hooks
- publish volume health
- publish storage metrics
- apply per-device runtime policy
- serve only volumes explicitly assigned to the legacy storage owner

## Non-goals
- no global storage scheduler
- no distributed replication controller
- no control-plane client responsibilities
- no VM lifecycle management

## Required properties
- typed local API over Unix socket
- idempotent open/close behavior for repeated desired attachment intent
- reconstructable runtime state
- explicit error codes
- structured logging
- per-volume health visibility

## Security requirements
- dedicated service account
- restricted Unix socket permissions
- minimal filesystem visibility
- explicit device/path allowlists
- capability drop and sandboxing where possible

## Recovery model
- existing VMs should continue where safe if service restarts
- session/runtime state should be rebuilt from durable metadata plus local cache

## Implementation Status

| Area | Status |
|---|---|
| Local qcow2/raw file backend | Active implementation path |
| Local block/LVM backend | Active implementation path |
| iSCSI backend | Implemented; full `StorageBackend` trait with `iscsiadm`/`targetcli` integration |
| Ceph RBD backend | Implemented; full `StorageBackend` trait with `rbd` CLI integration |
| Disk migration bulk copy, flow control, CRC, resumability | Implemented |
| Disk migration dirty sync rounds and convergence reporting | Implemented; `MigrationTaskTable` tracks rounds, dirty blocks, bytes transferred |
| Paused final dirty flush | Implemented; `MigrationSender` pauses at `PausedFinalSync`, signals via watch channel, waits for agent `resume_disk_migration` |
| Pause-first mode (issue #394 Option C) | Implemented; `TriggerDiskMigrationRequest.pause_first` moves the same handshake before bulk copy (`PausedPreCopy`, zero bytes transferred); sender fails closed `failed_precondition` without a task; pause channel latches for senders not yet at their gate. Since #656 the user-facing API/UI/chvctl require `true` (live migration fails closed) until #394 is proven — see `disk-migration-protocol-spec.md` |

New storage capabilities and provider ownership are developed against
Volvisor per [ADR-028](../adr/028-volvisor-storage-orchestration.md). Existing
`chv-stord` behavior remains available only during the explicit ownership
transition; no volume may be mutated by both services.
