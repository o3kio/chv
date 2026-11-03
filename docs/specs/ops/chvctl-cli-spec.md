# `chvctl` CLI Spec

## Purpose
Provide local operator-safe inspection and limited recovery workflows for the CHV platform.

## Principles
- read-first by default
- mutation commands gated by maintenance mode or explicit force policy
- output aligned with node state machine and operation IDs
- structured output via `--output json|yaml` for automation

## Implemented Commands

This section documents the binary as built (`cmd/chvctl`); it was
realigned to the real command surface in the #372 live-path-fixes pass —
the previous revision documented commands that never existed and
spellings the binary never accepted.

### Authentication
- `chvctl login --username <username> --password <password>` — authenticate against the backend-for-frontend (BFF) and store the token locally

### Virtual Machines
- `chvctl vm list` — List all VMs
- `chvctl vm get <vm_id>` — Show VM details
- `chvctl vm create <name> [--cpu <n>] [--memory <size>] [--image <ref>] [--network <id>] [--node <node_id>] [--storage-class <local|iscsi|ceph|lvm>] [--disk-size-gb <n>] [--cloud-init <userdata|@file>]` — Create a new VM. `--memory` takes a size string ("512M", "2G"); `--node` overrides default placement; `--storage-class` is validated against the same vocabulary the BFF enforces (the create is rejected if the node does not offer the class); `--cloud-init` takes the userdata inline or reads it from a file with the `@file` form
- `chvctl vm start <vm_id>` — Start a VM
- `chvctl vm stop <vm_id>` — Stop a VM
- `chvctl vm reboot <vm_id>` — Reboot a VM
- `chvctl vm delete <vm_id>` — Delete a VM
- `chvctl vm migrate <vm_id> --to <node_id>` — Live-migrate a VM
- `chvctl vm resize <vm_id> [--cpu <n>] [--memory <size>]` — Resize VM resources
- `chvctl vm import <name> --file <path>` — Import a VM from a qcow2 disk image (`POST /v1/vms/import`, the route's first in-tree client). The file is uploaded as a multipart form (a `name` field and the image as the `file` part); the server validates the qcow2 magic and rejects a non-qcow2 file with a 400 naming it. The request carries a non-empty `x-csrf-token` header — the BFF's CSRF layer requires it on multipart (JSON mutations satisfy the same layer by content type alone; the header value is a fixed marker, not a verified secret, because the platform has no CSRF token model). The response carries the server-minted VM `id`, the `name`, a `Pending` desired state, and the node-side `disk_path` the image landed at — no `task_id` (unlike the volume create/delete responses); the journaled `CreateVm` operation is observable through `task list`. The imported VM, its `-disk` volume, and the operation are stamped with the importing operator as owner, and the VM appears in `vm list` immediately

### Nodes
- `chvctl node list` — List compute nodes
- `chvctl node get <node_id>` — Show node details
- `chvctl node drain <node_id>` — Drain node (evacuate VMs via live migration)
- `chvctl node maintenance <node_id> --enable` — Enter (`--enable`) or exit (omitted) maintenance mode

### Volumes
- `chvctl volume list` — List volumes
- `chvctl volume create <name> --node <node_id> --size <bytes|K|M|G|T> [--storage-class <local|iscsi|ceph|lvm>]` — Create a standalone data volume on the named node. `--node` is required: standalone volumes have no VM to place them and there is no default placement node (a node the inventory says does not offer the requested class rejects the create). `--size` is **bytes-denominated** — deliberately unlike `vm create`'s GiB-valued `--disk-size-gb`: a bare number is bytes and the `K`/`M`/`G`/`T` suffixes are binary (KiB/MiB/GiB/TiB), e.g. `--size 10G` = 10737418240 bytes, bounded 1 byte ..= 64 TiB (the bound and the class vocabulary are validated client-side against the same shared definitions the BFF enforces). The response carries the server-minted `volume_id` and a `task_id` to feed `chvctl task watch`. Attach-at-create is not offered — `attached_vm_id` is a reserved key the BFF rejects with a 400 naming the mutate-attach path (create the volume, then attach via `POST /v1/volumes/mutate`); image seeding is likewise not offered (`seed_image_ref` is a reserved key rejected with a 400)
- `chvctl volume snapshot <volume_id> --name <snapshot>` — Create a volume snapshot
- `chvctl volume clone <volume_id> --name <target_volume_id>` — Clone a volume
- `chvctl volume delete <volume_id>` — Delete a standalone data volume. **Destructive and irreversible**: once the `DeleteVolume` task dispatches, the backing store on the node (the file or LV) is destroyed; the volume's rows persist as a `'Deleting'` tombstone and the volume disappears from operator surfaces. The request is `POST /v1/volumes/delete` with the single `volume_id` (the `vm delete` body shape); the response carries the accepted `task_id` to feed `chvctl task watch` (which prints the journaled terminal-failure cause if the dispatch fails). Only standalone data volumes (`volume_kind = 'data'` — the `volume create` lineage and its clones) are deletable: a boot disk or VM-embedded volume rejects with a 400 naming the VM lifecycle. An attached volume rejects with a 400 naming the detach path (`POST /v1/volumes/mutate` with action `detach` first) — there is deliberately **no `--force` flag and no `--kind` override**. A volume with an in-flight operation rejects with a 409; a volume covered by an enabled backup schedule rejects with a 400 naming the schedule

Volume show is not implemented. No pre-`volume create` volume is deletable (every volume created before the standalone-create surface is VM-embedded lineage with a NULL `volume_kind` — the legacy-residue sweep is tracked separately in #522's design §8).

### Images
- `chvctl image list` — List disk images
- `chvctl image import <name> --url <source_url> [--format <qcow2|raw>]` — Import an image
- `chvctl image delete <image_id>` — Delete an image

### Networks
- `chvctl network list` — List networks
- `chvctl network create <name> --cidr <cidr>` — Create a network
- `chvctl network delete <network_id>` — Delete a network

There is no `--vlan` flag: VLAN tagging does not exist at any layer of
the platform today and the flag was silently ignored when it existed
(#372); #517 tracks the real implementation.

### Tasks / Operations
- `chvctl task list` — List tasks/operations
- `chvctl task watch <task_id> [--timeout <seconds>]` — Poll a task until it reaches a terminal status (`Succeeded` exits 0; `Failed`/`Cancelled`/`Rejected`/`Stale`/`Conflict` exit non-zero). The watch is bounded: it gives up after `--timeout` seconds (default 900 = 15 minutes). On a terminally-failed task the watch prints the journaled cause before exiting (`Cause: <error_code> — <error_message>`, e.g. `UNSUPPORTED_BY_AGENT — snapshot_volume is unsupported in core-managed mode`) — the code and the agents' verbatim refusal text the control plane records at terminal failure (#502); a terminal task with no recorded cause prints no cause line, never a placeholder

### Users (Admin)
- `chvctl user list` — List users
- `chvctl user create <username> --password <password> [--role <admin|operator|viewer>]` — Create a user
- `chvctl user delete <user_id>` — Delete a user (takes the user id from `user list`, not the username)

### Migrations
- `chvctl migrate start <vm_id> <target_node>` — Start a live migration. Delegates to the vm-mutate migrate path (`POST /v1/vms/mutate`) — the same route `chvctl vm migrate` drives; the two commands are equivalent entry points. Operator role; the response carries the `task_id` to feed `chvctl task watch`
- `chvctl migrate status <migration_id>` — Show a migration's phase and progress counters (`GET /v1/migrations/{id}`, viewer tier; unknown ids are a 404)
- `chvctl migrate cancel <migration_id>` — Request a cooperative cancel of an in-flight migration (`POST /admin/migrations/{id}/cancel` on the control-plane admin surface). **Admin role required** — an operator-role token gets a 403, and the server must be a CP admin endpoint, not a plain BFF bind. The cancel is best-effort: the migration loop observes the flag at a safe point and rolls back; the response's `outcome` distinguishes `requested` from the `already_requested`/`already_terminal` no-ops
- `chvctl migrate list` — List migrations (`GET /v1/migrations`, viewer tier)

Migration ids come from the control plane's migration machinery
(`migrate list` / `migrate status`); a migration started via
`migrate start` or `vm migrate` is tracked as an operation (`task
list`) and as a row on these read routes once the migration loop
records it.

### Health
- `chvctl health check` — Quick control-plane health check
- `chvctl health report <node_id>` — Per-node health report
- `chvctl health cluster` — Cluster-wide health summary

### Version
- `chvctl version` — Show CLI version, git commit, build date, and release channel

## Removed command groups

Two command groups that could only ever 404 were **removed** (#372 §2.2,
§2.4; DP3/DP5 of the adopted design) — their target BFF routes never
existed:

- `chvctl storage list|show|create|delete` — every subcommand 404'd;
  the backing `storage_pools` catalog is a phantom surface nothing in
  provisioning reads or writes (#379 C4; the catalog question is tracked
  in #514 — which later removed the catalog wholesale: the BFF routes,
  the CP stub routes, the UI page, and the table are gone; the storage
  inventory lives at `/volumes` and `chvctl volume list`).
- `chvctl backup list|run` — every subcommand 404'd; even repointed, the
  live `BackupWorker`'s execute is a guaranteed-fail no-op ("Backup is
  not DR" per the production-readiness declaration). The BFF's
  `/v1/backups/*` routes are now **hard-fenced** (`501 NOT_IMPLEMENTED`,
  #654), the UI's backup catalog pages and navigation are removed, and the
  `BackupWorker` scaffold only drains legacy rows.

Invoking either group now fails at argument parsing with
"unrecognized subcommand" — louder and truthful (the old behavior was a
404 from a route that never existed). Scripts carrying the muscle memory
must stop calling them; there was never a working invocation to lose.

## Repointed command group (PR 4 of #372)

The `migrate` group was repointed in PR 4 of the #372 decomposition
(design §2.3/DP4 + DP4b, adopted 2026-10-06) — every subcommand
previously 404'd against BFF routes that were never registered:

- `migrate start` now drives the vm-mutate migrate path
  (`POST /v1/vms/mutate`, wire field `target_node_id`) — the route
  `chvctl vm migrate` always used; the old `POST /v1/migrations` target
  never existed.
- `migrate cancel` now calls the control plane's admin-tier
  `POST /admin/migrations/{id}/cancel` — admin role required, disclosed
  above.
- `migrate status`/`list` read the new viewer-tier
  `GET /v1/migrations[/{id}]` routes added by the same PR (plain
  SELECTs over the real `migrations` table; previously no migration
  read surface existed at the BFF tier).

The subcommand names, arguments, and output conventions are unchanged —
only the routes behind them. There was never a working invocation to
lose (every subcommand 404'd from introduction).

## Global Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--server` | `http://localhost:8080` | BFF server URL |
| `--token` | (from `~/.config/chvctl/credentials`) | Auth token override |
| `--output` | `table` | Output format: `table`, `json`, `yaml` |

The credentials written by `chvctl login` live under the platform config
dir (`~/.config/chvctl/credentials` on Linux; `XDG_CONFIG_HOME` is
honored), not `~/.chv/`.

## Safety Requirements
- local access only unless a future remote operator model is explicitly defined
- mutations must surface confirmation, policy check result, and operation ID
- failures must map to stable error codes
- `resize` and `delete` operations enforce quota checks and ownership validation
- `drain` operations require `Operator` or `Admin` role
