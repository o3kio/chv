# CHV Operations Guide

Day-2 operations for CHV deployments: monitoring, troubleshooting, backups, and scaling.

---

## Monitoring

### Health Endpoints

| Endpoint | Purpose | Expected |
|----------|---------|----------|
| `curl http://127.0.0.1:8080/health` | Control plane liveness | `200 OK` |
| `curl http://127.0.0.1:8080/ready` | Control plane readiness | `200 OK` after migrations |
| `curl http://127.0.0.1:8080/health/deep` | Component-level deep health | `200 OK` (degraded) or `503` (unhealthy) |
| `curl http://127.0.0.1:9901/metrics` | Agent Prometheus metrics | Prometheus text format |

#### Deep Health Check

The `/health/deep` endpoint reports per-component health:

```bash
curl -s http://127.0.0.1:8080/health/deep | jq .
```

Example response (`healthy`):
```json
{
  "status": "healthy",
  "components": {
    "database": { "status": "healthy", "latency_ms": 2 },
    "agent_socket_dir": { "status": "healthy" },
    "agent_connectivity": { "status": "healthy" }
  }
}
```

- **`database`**: SQLite connectivity with latency measurement.
- **`agent_socket_dir`**: Agent runtime directory exists and is readable.
- **`agent_connectivity`**: Can establish a Unix socket connection to at least one agent.

Status semantics:
- `healthy` — all components pass (HTTP 200)
- `degraded` — database passes but agent issues (HTTP 200, still serving)
- `unhealthy` — database fails (HTTP 503)

### Key Prometheus Metrics

```
chv_vms_total{status="running"}
chv_nodes_ready
chv_operations_completed_total{status="succeeded"}
chv_operations_latency_seconds_bucket
```

#### Circuit Breaker Behavior

Node communication from the control plane is protected by a circuit breaker (`Closed` → `Open` → `HalfOpen`). When open, RPCs to that node are rejected immediately with `BackendUnavailable` without attempting the call. The breaker probes with single requests every 30s (default) in `HalfOpen` state and closes after 3 consecutive successes.

Agent connectivity metrics on `:9901/metrics` complement this:

```
# 1 = connected, 0 = disconnected
curl -s http://127.0.0.1:9901/metrics | grep chv_agent_cp_connected

# Duration of current disconnect in ms
curl -s http://127.0.0.1:9901/metrics | grep chv_agent_cp_disconnected_duration_ms
```

Core journal health (core-managed authority mode only — the metrics
endpoint is part of the legacy/core-managed startup; core-native does not
serve it, and the families are absent in legacy mode):

```
# Current count of InspectRequired operations awaiting operator resolution
curl -s http://127.0.0.1:9901/metrics | grep chv_agent_journal_inspect_required

# 1 = the journal scanner is healthy, 0 = wedged/failing
curl -s http://127.0.0.1:9901/metrics | grep chv_agent_journal_healthy

# Cumulative failed journal scans since agent start
curl -s http://127.0.0.1:9901/metrics | grep chv_agent_journal_scan_failures_total
```

#### Partition / Autonomy Metrics

When an agent loses control-plane connectivity, it enters autonomous mode:

```
# 1 = connected, 0 = disconnected
curl -s http://127.0.0.1:9901/metrics | grep chv_agent_cp_connected

# Duration of current disconnect in ms
curl -s http://127.0.0.1:9901/metrics | grep chv_agent_cp_disconnected_duration_ms
```

During a partition:
- Existing VMs continue running.
- `CreateVm` and `MigrateVm` RPCs are rejected with `FAILED_PRECONDITION`.
- `StopVm` and `RebootVm` for existing VMs are allowed.
- Upon reconnection, pending messages are flushed in order.

### systemd Service Status

```bash
# All services
systemctl status chv-controlplane chv-agent chv-stord chv-nwd nginx

# Watch logs
journalctl -u chv-controlplane -f
journalctl -u chv-agent -f
journalctl -u chv-stord -f
journalctl -u chv-nwd -f
```

---

## CLI Reference (`chvctl`)

`chvctl` is the operator CLI for the CHV platform. It communicates with the backend-for-frontend (BFF) HTTP API.

### Global Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--server` | `http://localhost:8080` | BFF server URL |
| `--token` | (from `~/.chv/credentials`) | Auth token override |
| `--output` | `table` | Output format: `table`, `json`, `yaml` |

### Commands

| Command | Subcommands | Description |
|---------|-------------|-------------|
| `chvctl login` | — | Authenticate and store token |
| `chvctl vm` | `list`, `get`, `create`, `start`, `stop`, `reboot`, `delete`, `migrate`, `resize` | VM lifecycle management |
| `chvctl node` | `list`, `get`, `drain`, `maintenance` | Node operations |
| `chvctl image` | `list`, `import`, `delete` | Disk image management |
| `chvctl volume` | `list`, `snapshot`, `clone` | Storage volumes |
| `chvctl network` | `list`, `create`, `delete` | Network management |
| `chvctl task` | `list`, `watch` | Task/operation inspection |
| `chvctl user` | `list`, `create`, `delete` | User management (admin) |
| `chvctl migrate` | `start`, `status`, `cancel`, `list` | Live migration control (start via the vm-mutate path; cancel is admin-tier; status/list on the viewer-tier migration read routes — #372) |
| `chvctl health` | `check`, `report`, `cluster` | Cluster health |
| `chvctl version` | — | Show version and build info |

> **Removed groups:** the `chvctl storage` and `chvctl backup` command
> groups were removed (#372) — every subcommand 404'd against routes
> that never existed. The `storage_pools` catalog itself was later
> removed wholesale (#514): the BFF `/v1/storage-pools` routes, the CP
> stub routes, the UI page, and the table are gone (old clients calling
> those routes get 404; the storage inventory lives at `/volumes` and
> `chvctl volume list`). The `backup` group's removal is part of the same
> posture as the hard fence below: backup execution has no engine, every
> `/v1/backups/*`, `/v1/backup-jobs`, `/v1/backup-history`, and legacy
> `/api/v1/backup*` route answers `501 NOT_IMPLEMENTED` after authentication,
> the WebUI exposes no backup navigation or actions, and the scheduler creates
> no new jobs. Invoking either removed group now fails at argument parsing
> with "unrecognized subcommand".

### Examples

```bash
# List VMs
chvctl vm list

# Show node details
chvctl node get <NODE_ID>

# Drain a node for maintenance
chvctl node drain <NODE_ID>

# Check cluster health
chvctl health cluster

# Watch a task until it completes
chvctl task watch <TASK_ID>

# Resize a VM
chvctl vm resize <VM_ID> --cpu 4 --memory 8G
```

---

## Backup and Restore

> **Scope: the control-plane database only.** CHV does not implement
> tenant-data (VM disk/volume) backup or restore. That surface is
> hard-fenced: every backup/restore API route answers `501
> NOT_IMPLEMENTED`, the WebUI exposes no backup actions, and no artifact is
> ever produced. Protecting VM disks and volumes is the operator's
> responsibility outside CHV (see
> [`DEPLOYMENT-ARCHITECTURE.md`](DEPLOYMENT-ARCHITECTURE.md) §7). The
> procedures below back up and restore the operational database, which is
> required for control-plane recovery and is *not* a DR story for guest
> data.

### SQLite Database Backup

The database lives at `/var/lib/chv/controlplane.db`. Back up before upgrades or migrations. Always open the live database read-only (see [Live Database Access](#live-database-access)):

```bash
# Online backup (SQLite backup API, read-only source — safe while the CP runs)
sqlite3 "file:/var/lib/chv/controlplane.db?mode=ro" ".backup '/backup/chv-$(date +%Y%m%d-%H%M%S).db'"

# Automated pre-migration backup (built into chv-controlplane)
# The control plane automatically backs up the DB before running migrations,
# keeping the last 10 backups in /var/lib/chv/backups/.
```

#### Credential encryption key (back it up with the database)

S3 backup credentials are encrypted at rest (AES-256-GCM). The key material
lives in `/etc/chv/encryption.env` (`CHV_ENCRYPTION_KEY`, mode 0600 root),
minted once at install time by the package postinst or `install.sh` and
loaded by `chv-controlplane.service` via `EnvironmentFile`. It is never
regenerated on upgrade.

- **Back it up together with the database.** A database restore without the
  matching key leaves stored S3 credentials unreadable (they are treated as
  missing). While backup/restore is hard-fenced there is no supported UI or
  API path to re-enter them: the removed backup pages cannot be used, and the
  fence rejects schedule/job mutation with `501`. Treat any restored
  ciphertext as unrecoverable and keep the key with the database backup.
- **Rotation** is manual: write a new key to `/etc/chv/encryption.env`
  (0600 root) and `systemctl restart chv-controlplane`. Values encrypted
  under the old key read as missing, not corrupted. Re-entering guest-backup
  S3 credentials requires a backup implementation; it is not an operator
  action in this release.
- If no usable key is configured (no `[secrets] credential_encryption` in
  `controlplane.toml`, and either the environment file is absent with no
  legacy `CHV_JWT_SECRET` fallback or `CHV_ENCRYPTION_KEY` is present but
  empty), the control plane logs a warning and rejects credential writes
  (fail-closed): backup S3 and NetBox credentials are never stored in
  **plaintext**. The explicit config value takes precedence over the
  environment (DEPLOYMENT.md §8). A backup schedule that carries S3
  credentials cannot be created or updated until a key is configured.
  Existing plaintext rows remain readable; previously encrypted rows still
  require the original key.
- **Upgrading a pre-#335 host:** the postinst mints the key, but a
  *running* control plane does not re-read `EnvironmentFile` until
  restarted — `systemctl restart chv-controlplane` after the upgrade to
  activate encryption.

## Live Database Access

The control plane holds the database in WAL mode with a pool of open
connections. Rules for touching it from outside:

- **Reads — always use a read-only URI.** `sqlite3 "file:/var/lib/chv/controlplane.db?mode=ro" "SELECT ..."` is safe while the control plane runs: read-only opens never remove or rewrite the WAL sidecars and cannot race control-plane transactions.
- **Never open the live database read-write.** There is no operation that needs it: while the control plane's connection pool holds open connections, an external read-write close cannot remove the sidecars (SQLite removes `-wal`/`-shm` only when the last connection closes), but on a quiet deployment the pool can drain and an external read-write close then removes them. That is recovered transparently when the control plane reopens, but it is an avoidable risk — and a read-write open takes write locks that can block the control plane.
- **Never manipulate the database files directly while the control plane runs** — deleting `-wal`/`-shm`, copying a file over the database, or "cleaning up" sidecars. With open control-plane connections, subsequent commits continue into the deleted inode and are **silently lost** (verified: the connection keeps accepting writes, nothing errors, the data is gone). If sidecar files are inconsistent, stop the control plane first.
- **Manual writes (seeding, corrections):** stop the control plane, write with the plain `sqlite3` CLI, start it again. This is the pattern the qualification harness uses (`scripts/integration/qual/deploy.sh` seeds in a stop→write→restart window).

### Restore from Backup

```bash
sudo systemctl stop chv-controlplane chv-agent
sudo cp /backup/chv-YYYYMMDD-HHMMSS.db /var/lib/chv/controlplane.db
sudo chown chv:chv /var/lib/chv/controlplane.db
sudo systemctl start chv-controlplane
# Re-enroll the local agent if certificates were rotated
```

### Certificate Backup

```bash
sudo tar czf /backup/chv-certs-$(date +%Y%m%d).tar.gz /etc/chv/certs/
```

### Node Certificate Revocation

A stolen or host-compromised node certificate stays cryptographically valid
until expiry. Revoke it — do not wait for expiry:

```bash
chvctl node revoke-certificate <NODE_ID> \
  --reason "suspected host compromise: incident INC-1234"
```

The serial is added to the durable `node_certificate_revocations` denylist and
rejected with `UNAUTHENTICATED` by the gRPC peer-identity interceptor on every
steady-state node-facing RPC. Revocation also **burns rotation** for that
serial: it cannot be exchanged for a fresh certificate, so the node must
re-enroll with a new one-time bootstrap token. Enrollment deliberately remains
reachable with the revoked certificate (the token is the authentication factor);
enrollment rotation is still rejected. The endpoint is admin-only and
idempotent, and writes an audit event.

Use the canonical 8-character lowercase hex value from
`nodes.certificate_serial` when passing `--certificate-serial`;
`openssl x509 -serial` prints a different HEX encoding and is rejected.

- Propagation to authentication is bounded by the snapshot refresh interval
  (`CHV_REVOCATION_REFRESH_SECS`, default 5 s); rotation is database-immediate.
- Production startup fails closed if the denylist cannot be loaded.
- After a database restore, diff the denylist against incident records and
  re-revoke any missing serials.
- `CHV_ALLOW_INSECURE=1` (dev-feature builds only) bypasses revocation.

Full procedure (verify, rotation recovery, re-enrollment, rollback, failure
modes): [Node Certificate Compromise](runbooks/node-certificate-compromise.md).

### Disaster Recovery Runbooks

For detailed step-by-step procedures covering VM snapshot restore, volume snapshot restore, backup artifact restore, control plane DR, and full site recovery, see the runbooks in [`docs/runbooks/`](runbooks/).

| Runbook | Scenario |
|---------|----------|
| [VM Snapshot Restore](runbooks/vm-snapshot-restore.md) | Restore a VM from a local snapshot (automated) |
| [Volume Snapshot Restore](runbooks/volume-snapshot-restore.md) | Restore a volume from a local snapshot (local/Ceph automated; LVM/iSCSI manual) |
| [Backup Artifact Restore](runbooks/backup-artifact-restore.md) | Restore a VM from an externally produced S3/NFS disk artifact (manual; CHV creates none) |
| [Control Plane DR](runbooks/control-plane-dr.md) | Recover the control plane after host failure or DB corruption |
| [Full Site Recovery](runbooks/full-site-recovery.md) | Rebuild the entire infrastructure from backups |
| [Node Certificate Compromise](runbooks/node-certificate-compromise.md) | Revoke a node certificate, verify rejection, recover rotation, re-enroll |

---

## Scaling: Multi-Node

> **Qualification status:** Multi-node operation is unqualified. The enrollment
> protocol is code-supported, but control-plane dispatch resolves a local
> Unix-socket path and cannot drive a remote node. The qualified topology is
> single-host.

### Add a hypervisor-only node

1. **On the control plane host**, create a bootstrap token:
   ```bash
   TOKEN=$(openssl rand -hex 32)
   echo "$TOKEN" | sudo tee /etc/chv/bootstrap.token.new
   # Insert into DB (one-time use)
   ```

2. **On the new hypervisor node**, install binaries and Cloud Hypervisor (the VMM):
   ```bash
   sudo apt install -y qemu-kvm bridge-utils iproute2 iptables
   # Copy chv-agent, chv-stord, chv-nwd from the control plane host
   ```

3. Configure `/etc/chv/agent.toml`:
   ```toml
   control_plane_addr = "https://<CONTROL_PLANE_IP>:8443"
   # ... other settings same as all-in-one deploy
   ```

4. Start services:
   ```bash
   sudo systemctl enable --now chv-stord chv-nwd chv-agent
   ```

5. **Verify enrollment** in the Web UI or via API:
   ```bash
   curl -s http://127.0.0.1:8080/v1/nodes | jq '.items[].name'
   ```

---

## Troubleshooting Quick Reference

### Agent Fails to Enroll
| Check | Command |
|-------|---------|
| Bootstrap token exists | `sudo cat /etc/chv/bootstrap.token` |
| Token not expired | `sqlite3 "file:/var/lib/chv/controlplane.db?mode=ro" "SELECT expires_at FROM bootstrap_tokens;"` |
| Control plane listening | `ss -tlnp | grep 8443` |
| Agent can reach control plane | `curl -k https://127.0.0.1:8443/health` |
| Agent logs | `journalctl -u chv-agent -n 100 --no-pager` |

### VM Won't Start
| Check | Command |
|-------|---------|
| KVM available | `ls /dev/kvm && groups chv` |
| Cloud Hypervisor binary | `cloud-hypervisor --version` |
| Storage pool exists | `ls /var/lib/chv/storage/localdisk/` |
| Volume prepared by stord | `journalctl -u chv-stord -n 50` |
| Network bridge up | `ip addr show chvbr0` |

For a sustained p99 start-latency alert (`VmStartLatencyHighP99`) use
[VM Start Latency](runbooks/vm-start-slow.md); for lifecycle errors
(`VmOpFailureRateHigh`) use [VM Operation Failures](runbooks/vm-op-failures.md).

### Web UI Blank or API Errors
| Symptom | Fix |
|---------|-----|
| Blank page | Verify `/opt/chv/ui/index.html` exists; since #447 the control plane binary serves the tree — check `[webui]` in `/etc/chv/controlplane.toml` and `journalctl -u chv-controlplane` (the startup line names the dir; a missing dir warns) |
| JSON parse error | Ensure the nginx `proxy_pass` has NO trailing slash after the port |
| 502 Bad Gateway | Verify control plane is running: `systemctl status chv-controlplane` |
| Console disconnected | Check the edge's WebSocket `/ws/` proxy config (the binary does not proxy `/ws/`); verify agent PTY process is running |

For a sustained BFF 5xx alert (`BffApiErrorRate`) or a user-visible error
code, use [BFF API Errors](runbooks/bff-errors.md). The `request_id` in the
error body matches the `x-correlation-id` response header.

### chv-stord or chv-nwd Keep Restarting
| Check | Command |
|-------|---------|
| Binary permissions | `ls -l $(command -v chv-stord chv-nwd)` |
| Socket directory | `ls -la /run/chv/stord /run/chv/nwd` |
| Config syntax | `cat /etc/chv/stord.toml` / `cat /etc/chv/nwd.toml` |
| Daemon logs | `journalctl -u chv-stord -f` / `journalctl -u chv-nwd -f` |

If the node runs `chv-agent` with `stord_config_path` set in `/etc/chv/agent.toml`
(#385 respawn config fidelity), the agent's supervisor respawns a dead stord with
the operator's `stord.toml` verbatim — every operator key survives the respawn.
An agent log line `stord_config_path unusable for respawn; falling back to the
supervisor-generated config` means the file was unreadable, malformed, or its
`socket_path` did not match the agent's `stord_socket`: the respawned daemon
then runs the minimal generated config (allowlist per
`stord_path_allowlist`, `runtime_dir` under the agent's data dir) and any
operator keys — `backend_type`, `device_allowlist`, `[migration]`, a custom
`runtime_dir` — are NOT in effect until the file is fixed. The agent user must
be able to read the file (both standard install surfaces grant this via
`chv-stord` group membership: `install.sh` and the `.deb` postinst; note the
timing on package upgrades — the postinst never restarts the agent, so the
grant takes effect at the agent's next start; hardening `stord.toml` before
that restart yields the warn + fallback above until then);
hand-managed deployments should set `stord_config_path` to their
`stord.toml` to get the pass-through behavior.

A `stord.toml` that passes that check can still fail at daemon *startup*: a
missing or unwritable `runtime_dir` parent (stord aborts when it cannot create
its session store), a backend constructor with bad material (e.g. an LVM volume
group that does not exist), or migration TLS files that stord validates when
the listener comes up rather than at parse time. In that case the respawned
stord exits immediately and the supervisor re-validates the file (it still
parses and the socket still matches) and re-execs the operator path on its
normal restart throttle (~5 s): it does **not** fall back to the generated
config, because the fallback covers validation failures only. This mirrors
systemd `Restart=on-failure` restarting `ExecStart=chv-stord /etc/chv/stord.toml`
with the same broken file — the remedy is the same in both cases: fix the
operator config (create/widen the `runtime_dir`, repair the backend or TLS
material). The health check is never wedged: the agent keeps reporting stord
unhealthy and keeps retrying under the throttle while the file is broken.

The same contract applies to `nwd_config_path` (#504 respawn config fidelity,
the nwd twin of #385): with the key set in `/etc/chv/agent.toml`, the agent's
supervisor respawns a dead nwd with the operator's `nwd.toml` verbatim — every
operator key survives the respawn. An agent log line `nwd_config_path unusable
for respawn; falling back to the supervisor-generated config` means the file was
unreadable, malformed, or its `socket_path` did not match the agent's
`nwd_socket`: the respawned daemon then runs the minimal generated config and
any operator keys — the `[overlay]`, `[ebpf]` and `[fabric]` blocks,
`metrics_bind`, a custom `log_level` — are NOT in effect until the file is
fixed. The agent user must be able to read the file (the standard install
ships `nwd.toml` as `root:chv 0640`, so this holds out of the box);
hand-managed deployments should set `nwd_config_path` to their `nwd.toml` to
get the pass-through behavior. A config that passes validation but fails at
daemon startup (e.g. a `[fabric]` block whose provider cannot initialize, or an
unwritable `runtime_dir`) crash-loops on the operator path under the restart
throttle, exactly as stord does above — same posture as systemd
`Restart=on-failure` with the same file, same remedy: fix the operator config.
The health check is never wedged.

### LVM Storage Nodes (#379)

A node serves LVM volumes when its stord runs `backend_type = "lvm"`. The
operator contract:

**1. Provision the VG out-of-band — stord never creates it.**

```bash
pvcreate /dev/nvme0n1
vgcreate chv-vg /dev/nvme0n1
```

**2. Configure stord, and point the agent at the same file.** In
`/etc/chv/stord.toml`:

```toml
backend_type = "lvm"
lvm_volume_group = "chv-vg"          # default when unset
device_allowlist = ["/dev/dm-*", "/dev/mapper/*"]
```

and in `/etc/chv/agent.toml`, `stord_config_path = "/etc/chv/stord.toml"`
(the #385 pass-through key — it makes supervisor respawns keep the LVM shape,
and it is the SAME file the agent parses at startup to learn the node's real
backend class for inventory reporting and LVM locator shaping). **Changing the
backend requires an agent restart**: the agent parses `stord_config_path` once
at startup, so flipping `backend_type` without restarting the agent leaves it
reporting and dispatching against the old class.

**3. Fail-closed startup guards (by design).** stord aborts at startup rather
than serving the wrong backend: a present-but-unrecognized `backend_type`
(e.g. a typo'd `"lvv"`) aborts with `backend_type '…' is not a recognized stord
backend`, and `backend_type = "lvm"` against a VG that does not exist (or a
host without `vgs`) aborts with `backend_type is 'lvm' but volume group '…'
does not exist`. An *absent* `backend_type` still means `local` — the default
contract is unchanged.

**4. Volume opens are dm-path-shaped and allowlist-checked.** The agent opens
LVM-class volumes with a `/dev/mapper/{vg}-{volume_id}` locator token; the
`device_allowlist` check fires only for `lvm`/`block`-class opens, so the
recommended allowlist above admits exactly those tokens while a VM-dir path
(`{volume_id}.img`) is denied. Omitting `device_allowlist` leaves device opens
unconfined (fail-open) — set it explicitly on LVM nodes.

**5. Provisioning is create-on-open; seeding is out-of-band.** An open of an
**absent** LV with a size provisions it (`lvcreate`, sized from the volume's
capacity — a VM's boot disk carries its `volume_size_gb`); an open of an absent
LV *without* a size is rejected (`size_bytes`), and so is any `seed_from`
(`InvalidArgument` — LVM has no seed path by design). To place an image on an
LV, seed it out-of-band while the VM is stopped:

```bash
qemu-img convert -O raw /var/lib/chv/images/noble.raw /dev/chv-vg/<volume_id>
```

**6. Reclamation is out-of-band.** Deleting the VM closes the stord session
but never removes the LV — reclaim capacity with
`lvremove /dev/chv-vg/<volume_id>` once the data is no longer wanted.

**7. Class mismatches are rejected at accept time.** The node's inventory
reports the real backend class (from the parsed `stord_config_path`), and the
control plane rejects a VM create or volume attach requesting a class the node
does not offer (`400`, before any intent is journaled). A NULL/absent class
still means `local` everywhere. A node that has not reported classes (no
inventory yet, or a pre-#379 agent) is never rejected by this check — the
agent's volume open and stord's own backend validation remain the enforcement.

| Check | Command |
|-------|---------|
| VG exists and has free space | `vgs chv-vg` |
| stord running the LVM backend | `journalctl -u chv-stord \| grep backend_type` |
| Node reports the class | `chvctl node list` (storage classes in node detail) |
| LV materialized for a VM volume | `lvs chv-vg` |
| Agent failed to parse the config | `journalctl -u chv-agent \| grep -i stord_config` (silence is expected on success — the agent logs this key only on the warn/degrade path; "Node reports the class" above is the positive signal) |

### Upgrade Failures

The automated upgrade orchestrator was removed in PR #213, and the `chvctl upgrade` subcommands were removed in #427 — they targeted `/v1/upgrades` BFF routes that were never registered. There is no automated upgrade surface to troubleshoot; upgrades are performed manually (see "Upgrade Procedure (Manual)" below). If a manual binary swap leaves a node drained, force it back to a healthy state:

```bash
curl -X POST http://127.0.0.1:8080/v1/nodes/mutate \
  -d '{"node_id": "<NODE_ID>", "action": "exit_maintenance"}'
```

### Migration Failures

| Symptom | Diagnostic | Resolution |
|---------|-----------|------------|
| `FAILED_PRECONDITION: no migration client identity` | Sender node has no migration client identity configured (migration disabled, or a destination-only stord) | This node does not initiate migrations. Set `migration.client_cert_path`, `migration.client_key_path`, `migration.ca_cert_path`, and `migration.dest_server_name` with `migration.enabled = true` in `/etc/chv/stord.toml` **only if this node should send migrations** — a destination-only stord legitimately has no client identity |
| `failed to connect to peer with mTLS` | Certificate mismatch or network issue | Verify CA cert matches on both nodes; check firewall rules |
| `CRC mismatch reported by receiver` | Data corruption in transit | Check network for packet corruption; retry migration |
| `deadline_exceeded waiting for Ack` | Receiver not acknowledging | Check receiver node health; disk I/O saturation on destination |
| Migration stalled (reaper will clean up) | `chv_migration_phase` gauge stuck and no progress reports | Wait for the reaper's no-progress timeout (default 7200 s, `CHV_MIGRATION_STALL_TIMEOUT_SECS`) or manually fail the row. The reaper resumes the source VM only when the sender is proven quiescent; otherwise the VM may stay paused for operator review |
| Backpressure throttling | `slow_down_factor` in logs | Receiver is overwhelmed; reduce concurrent migrations or add I/O capacity |
| `UNAUTHENTICATED: missing control-plane migration authorization token` | Token did not reach stord (agent/stord version skew, or a manual `TriggerDiskMigration` without `migration_authorization`) | Upgrade the agent and stord together; never hand-drive the stord migration API without a CP-minted token |
| `PERMISSION_DENIED: migration authorization rejected: ...` | Signature/claim failure: wrong destination node id, wrong volume/size, expired, not-yet-valid, or replayed token | Ensure both sides use the *same* dedicated `[migration].signing_secret_file`; check `migration.node_id` matches the agent's node id; check clock skew (≤ 60 s) and that the token TTL is ≤ 900 s. A `size mismatch` line names both the token-authorized and receiver/backend sizes. A replay means the same token was used twice — inspect for a retried dispatch reusing the old token |
| `FAILED_PRECONDITION: this stord has no migration authorization verifier configured` | Destination stord has no secret (`DenyAll`) | Set `migration.node_id` and `migration.signing_secret_file` (or `CHV_MIGRATION_SIGNING_SECRET`) on the destination and restart stord |
| CP logs `disk migration requires a dedicated migration signing secret` | Control plane has attached volumes to migrate but no secret | Set `[migration].signing_secret_file` on the control plane (same secret as destination stords), or explicitly set the development-only `allow_insecure_unauthenticated` on the CP, the source agent (`allow_insecure_migration_dispatch = true`), and the destination stord |
| `RESOURCE_EXHAUSTED: migration admission rejected: ...` | Destination hit its per-node aggregate `max_inflight_receiving` (default 16) or `max_inflight_receiving_bytes` (default 64 TiB) ceiling | Wait for active migrations to finish (capacity is released on stream close or after the 300 s receiver idle timeout), reduce migration concurrency, or raise the limits on the destination. Rejected attempts do not consume the token, so retry after capacity frees. The source agent retries this automatically a few times with exponential backoff before failing |

**Check active migrations:**
```bash
sqlite3 "file:/var/lib/chv/controlplane.db?mode=ro" \
  "SELECT migration_id, vm_id, phase, started_at FROM migrations \
   WHERE phase NOT IN ('Completed', 'Failed', 'RolledBack');"
```

### Migration Interrupted by a Control-Plane Restart

The control-plane migration state machine is an in-process task; it does not
survive a control-plane restart. The agent-side migration task may still be
running. Before any worker (including the reaper) starts, the control plane
reconciles every `migrations` row that is still non-terminal. The row is made
terminal and keeps the interrupted phase in `interrupted_from_phase`, the
recovery time in `interrupted_at`, and an operator-readable reason in
`error_message`. The parent operation is failed with
`error_code = MIGRATION_INTERRUPTED`. Terminal rows are never changed.

Recovery treats the source as *presumed* authoritative for `Pending`,
`PreCopyDisk`, and `ConvergingDisk`; this is an inference, not proof that the
agent-side task stopped. Restart recovery never queues an automatic source-VM
resume. Once the row is terminal, late progress reports from a still-running
agent task are dropped, so the reaper's "sender has been silent" evidence would
be self-fulfilling. Every recovered row is therefore operator-only
(`reaper_resume_state` is cleared). Note that the pause-first gate normally
pauses the source for the entire migration, so "the VM is on the source" does
not mean it is running: resuming it is an explicit operator step.

**Inspect interrupted migrations:**
```bash
sqlite3 "file:/var/lib/chv/controlplane.db?mode=ro" \
  "SELECT migration_id, vm_id, phase, interrupted_from_phase, interrupted_at, \
          error_message \
   FROM migrations WHERE interrupted_at IS NOT NULL \
   ORDER BY interrupted_at DESC;"
```

The interruption fields (`interrupted_from_phase`, `interrupted_at`,
`error_message`) are also returned by `chvctl migrate status <MIGRATION_ID>` and
`GET /v1/migrations/<MIGRATION_ID>`.

| Recovered `phase` | Interrupted `interrupted_from_phase` | Automatic action | Operator action |
|-------------------|--------------------------------------|------------------|-----------------|
| `RolledBack` | `Pending`, `PreCopyDisk`, `ConvergingDisk` | None — no source resume is queued | The source is presumed authoritative. Confirm the VM's actual state on both nodes and check that no agent-side migration task is still writing. Under pause-first the source is normally paused; resume it explicitly if you decide to abandon the migration on the source. The migration is not retried automatically |
| `Failed` | `MemoryMigration`, `Paused`, or an unrecognized phase | None — recovery does not guess a rollback | Inspect `vm_desired_state.target_node_id` and the VM's actual state on both nodes. Resume on the source only if the source still owns the VM state; otherwise finish/clean the destination. Partial destination volumes are not deleted automatically |
| — | `Completed`, `Failed`, `RolledBack` | None | No action; terminal rows are left exactly as found |

Do **not** mark an interrupted row `Completed` to make it disappear: `Completed`
means the destination owns the VM and placement was atomically updated.
Interrupted migrations are not retried automatically; start a new migration only
after confirming placement and cleaning up any partial destination volume.
A cancel request for an interrupted row returns `already_terminal`: the
control-plane state machine is gone, so there is nothing to roll back. Recovery
already queued `migrations.agent_cancel_pending` for the detached agent task;
for a terminal `Failed`/`RolledBack` row an operator cancel also (re-)queues it.
The migration reaper drains the flag through the source and destination agents'
`CancelMigration` RPC. Inspect the row with:

```sql
SELECT migration_id, phase, agent_cancel_pending, agent_cancel_confirmed_at,
       agent_cancel_attempts, agent_cancel_last_attempt_at, agent_cancel_last_error
FROM migrations
WHERE migration_id = '<MIGRATION_ID>';
```

`agent_cancel_pending = 1` means at least one endpoint has not confirmed the
cancel yet. `agent_cancel_confirmed_at IS NOT NULL` with a NULL error means both
agents aborted their detached task (or reported `not_tracked` because the agent
restarted). A cancel that arrives while a task is inside the Cloud Hypervisor
send/receive transfer cannot be confirmed immediately: the agent does not
force-drop that future and returns `Unavailable`, leaving the row pending for a
later drain. If the agent's own VMM call bound expires, or the VMM reply is
lost after the request was delivered, while Cloud Hypervisor may still be
transferring, the task is sealed as "outcome unknown" and remains tracked, so
cancellation stays unconfirmable for that agent process; a restart is the only
mechanical clear (`agent_cancel_pending = 1`) and the paused source VM is left
paused. After the restart the next drain reports `not_tracked`, which counts as
success. That re-enables the ordinary resume gate only for a reaper-failed row
already queued for resume (`reaper_resume_state = 'pending'` and no ambiguous
interrupted phase); rows terminalized by restart recovery (NULL) and blocked
rows stay operator-only. An operator who cannot rule out a completed
destination transfer must verify the VM and set `reaper_resume_state =
'blocked'` before restarting the agent for a reaper-owned row. A non-NULL
`agent_cancel_last_error` means the agent refused
permanently (core-managed or pre-#656 agent, an unsafe/misrouted node id, or an
invalid request); the detached task may still exist, so verify placement and
the agent process before resuming the source VM. The
reaper will not auto-resume a source VM while `agent_cancel_pending = 1`, nor
when `agent_cancel_last_error` records a permanent refusal (the task may still
be running). A confirmed cancel must also settle for 30 s before the resume
pass acts, so a terminal progress report still queued on the agent cannot race
the resume. It also refuses when a late `Completed` report arrived after the
row was terminalized `Failed`/`RolledBack` (`reaper_resume_state = 'blocked'`):
the detached agent may have completed the transfer, so the destination could
own the VM. Only requeue after the node has been upgraded or verified idle, and write all
three columns in one statement:
`agent_cancel_pending = 1, agent_cancel_last_error = NULL,
reaper_resume_state = 'pending'`. The resume pass re-selects a row only when
`reaper_resume_state` is `pending` or `resuming`; a permanent refusal leaves a
reaper-failed row `blocked`, while a restart-recovered row keeps the state NULL,
and neither is resumed automatically. Setting `pending` without the queued
cancel is unsafe: `agent_cancel_pending` is 0, so the resume pass is not held by
an outstanding cancel, and with no fresh confirmation the 30 s settle gate does
not apply — the source could be resumed on the next tick. Even with a
correctly queued cancel, automatic resume only applies to reaper-failed rows: a
row whose `interrupted_at` observed `MemoryMigration`/`Paused` is never
auto-resumed (whether recovered or reaper-failed), and a row blocked by a late
`Completed` must stay blocked because the destination may own the VM. Let the
requeued cancel confirm and settle, and only set `pending` once you have
verified the destination does not own the VM; otherwise leave the row blocked
and resume explicitly where the VM actually is.

A successful pause-first migration can leave the destination VM paused. When
the source agent reports `Completed`, the control plane resumes the destination
before moving placement. If every resume attempt fails, placement still moves
(the VM state is on the destination) but `migrations.error_message` records the
failure and `chv_migration_destination_resume_failures_total` increments;
resume that VM on the destination manually rather than on the source.

The startup log line `migration restart recovery completed` reports
`reconciled`, `already_terminal`, and `unresolved` counts; reconciliation also
increments `chv_migration_restart_reconciled_total{recovered_phase=...}`.
`unresolved` rows kept changing during startup (a late progress report raced the
scan). They are left non-terminal but marked with `interrupted_at` and
`interrupted_from_phase` when the observation still matches. The normal reaper
later fails them; if either the marked phase (`interrupted_from_phase`) or the
phase scanned at reap time is `MemoryMigration` or `Paused`, the reaper records
`reaper_resume_state = 'blocked'` and does not resume the source. A row that
changed too fast to mark (or that never carried `interrupted_at`) falls back to
ordinary reaper semantics, so inspect it manually.

`interrupted_at` and `interrupted_from_phase` are **audit markers** and are
never cleared by this recovery path. If a marked, still-non-terminal row is
later driven to `Completed` by the agent's own progress reports, the marker
remains. Treat it as "a restart observed this migration", not as "recovery is
still pending"; only the current `phase` and placement tell you what happened.

**Verify mTLS configuration:**
```bash
# Check cert files exist and are readable
ls -la /etc/chv/certs/node.pem /etc/chv/certs/node-key.pem /etc/chv/certs/ca.pem

# Test TLS handshake to a peer
openssl s_client -connect <PEER_IP>:8444 \
  -cert /etc/chv/certs/node.pem \
  -key /etc/chv/certs/node-key.pem \
  -CAfile /etc/chv/certs/ca.pem
```

**Verify migration authorization (issue #663):**

The control plane and every destination stord must load the *same* dedicated
migration-signing secret. It is separate from the user JWT secret, the
enrollment secret, and the TLS keys.

```bash
# Secret file: at least 32 bytes, root:chv 0640, no trailing content
sudo install -d -m 0750 -o root -g chv /etc/chv/migration
sudo sh -c 'openssl rand -hex 32 > /etc/chv/migration/signing-secret'
sudo chown root:chv /etc/chv/migration/signing-secret
sudo chmod 0640 /etc/chv/migration/signing-secret
sudo test "$(tr -d '\r\n' < /etc/chv/migration/signing-secret | wc -c)" -ge 32 && echo ok

# Destination stord config
grep -A12 '^\[migration\]' /etc/chv/stord.toml | grep -E \
  'node_id|signing_secret_file|max_inflight'

# Startup log should say minting/verification is enabled, and must NOT contain
# the insecure-bypass warning.
journalctl -u chv-controlplane -u chv-stord | grep -i 'migration authorization\|allow_insecure'
```

Never set `allow_insecure_unauthenticated = true` in production on either the
control plane or a destination stord, and never set
`allow_insecure_migration_dispatch = true` on an agent; all three gates must
agree before a tokenless CP-dispatched migration can run, and enabling them
disables the control-plane binding so any CA-certified peer could create
receiving volumes. Rotate the secret by updating the file on the control
plane and all stords and restarting them; tokens minted with the old secret
are rejected immediately (maximum exposure is the 900 s token lifetime).

Admission defaults are per-destination-node aggregates: 16 concurrent
receiving streams and 64 TiB (70,368,744,177,664 bytes) of summed
token-authorized volume size — one 16-volume VM, or two 8-volume VM
migrations, can exhaust the default stream budget. The replay cache is
memory-only and bounded by token TTL, and `source_node` is signed but not
matched to the TLS peer identity (defense-in-depth metadata only). An
authenticated peer that never sends `InitMigration`, or stalls mid-stream, is
closed after a 300 s idle timeout so it cannot pin a connection or admission
slot forever.

Development-only insecure mode has a diskless-VM edge: the control plane sets
its transient tokenless marker when no tokens were minted, and a diskless VM
mints none by construction. An agent whose
`allow_insecure_migration_dispatch` is false therefore refuses that
`MigrateVm` with `FAILED_PRECONDITION` even though no volume needed a token.
Enable the agent gate on every source node, or do not combine the
control-plane flag with diskless VMs.

### Stuck Operations After Agent Crash (InspectRequired)

When the agent process dies while a Core operation is in flight, the next
startup durably marks that operation **InspectRequired** and quarantines its
VM: the executor will not touch the VM again until an operator resolves the
operation. This is fail-closed — the side effect may or may not have happened.

**Recognize it:**

- Agent log at startup: `restart classification marked running operations InspectRequired`
- Scan-time log on any change: `journal scan: inspect-required operations changed`
- Agent metrics (`curl -s http://127.0.0.1:9901/metrics`, core-managed mode
  only): `chv_agent_journal_inspect_required` (current stuck count),
  `chv_agent_journal_healthy` (0 while the journal scanner is wedged)
- Node-local Core API:
  ```bash
  curl -s --unix-socket /run/chv/core/core-v1.sock http://localhost/v1/operations
  ```
  Stuck operations are `status: "running"` **plus** a `recovery_assessment`
  field; in-flight operations are `running` without it.

**Resolve it** — two equivalent paths. Node-local (works even when the
control plane cannot reach the node):

```bash
grpcurl -unix /run/chv/agent/api.sock \
  chv.controlplane.node.v1.LifecycleService/ResolveInspectRequiredOperation \
  -d '{
    "meta": {
      "operation_id": "resolve-<operation_id>",
      "requested_by": "operator-name"
    },
    "operation_id": "<from /v1/operations>",
    "vm_id": "<the operation's vm_id>",
    "disposition": "succeeded",
    "note": "VM is running and healthy; start completed before the crash"
  }'
```

Or via the control plane's relay (no node login required: the control
plane forwards to the owning agent, which validates and
terminal-persists; if the agent socket is unreachable the call fails
with `UNAVAILABLE` and the node-local path above is the fallback). The
target node goes in `meta.target_node_id`. The control plane's gRPC
surface requires mTLS — every request must present a certificate signed
by the control-plane CA (the same requirement as every other node-facing
RPC). The installed agent client certificate works:

```bash
grpcurl \
  -cacert /etc/chv/certs/ca.crt \
  -cert /etc/chv/certs/agent-client.crt \
  -key /etc/chv/certs/agent-client.key \
  127.0.0.1:8443 \
  chv.controlplane.node.v1.LifecycleService/ResolveInspectRequiredOperation \
  -d '{
    "meta": {
      "operation_id": "<operation_id>",
      "requested_by": "operator-name",
      "target_node_id": "<owning node id>"
    },
    "operation_id": "<from /v1/operations>",
    "vm_id": "<the operation's vm_id>",
    "disposition": "succeeded",
    "note": "VM is running and healthy; start completed before the crash"
  }'
```

Use `"disposition": "succeeded"` when the real-world state matches the
operation's intent (e.g. the VM is actually running after a stuck start), and
`"failed"` when it does not. The resolution terminal-persists the operation
under its original attempt token, is replay-idempotent (safe to retry), and
the VM is un-quarantined on the next executor scan. The note must state what
was observed and how it was verified — it is recorded in the journal.

---

## Maintenance Windows

### Graceful Node Drain

Draining a node evacuates all VMs via migration before allowing maintenance.
Until #394 is proven, migration is **stop-the-world**: the VM is paused for
the full disk+memory transfer (live migration is rejected at the user-facing
API, UI, and chvctl).

> **Qualification status:** The drain machinery is code-supported but
> unqualified; no multi-host evidence exists.

**Via CLI:**
```bash
chvctl node drain <NODE_ID>
```

**Via API:**
```bash
curl -X POST http://127.0.0.1:8080/v1/nodes/mutate \
  -H "Content-Type: application/json" \
  -d '{"node_id": "<NODE_ID>", "action": "drain"}'
```

**What happens:**
1. Node transitions to `Draining` — scheduling is paused immediately
2. Agent reconcile loop issues migration requests for each running VM.
   **Core-managed mode:** the agent is observe-only and issues **no**
   migration requests — the control plane must re-home the VMs. While any
   desired-Running VM (or an undecodable fragment) remains, the agent blocks
   the `Maintenance` transition and raises a `drain_blocked` control-plane
   alert (on change, not per tick).
3. VMs are migrated to other `TenantReady` nodes (legacy mode; stop-the-world
   until #394 is proven), or re-homed by the control plane (core-managed mode)
4. When all VMs are evacuated, node transitions to `Maintenance` automatically
5. Perform maintenance (kernel update, hardware swap, etc.)
6. Restart agent: `sudo systemctl start chv-agent`
7. Mark node ready: set desired state to `TenantReady` via API or Web UI

**Monitor drain progress:**
```bash
# Check remaining VMs
sqlite3 "file:/var/lib/chv/controlplane.db?mode=ro" \
  "SELECT count(*) FROM vms v JOIN vm_observed_state o ON v.vm_id = o.vm_id \
   WHERE v.node_id = '<NODE_ID>' AND o.runtime_status NOT IN ('Stopped', 'Deleted');"

# Watch via metrics
watch -n5 'curl -s http://127.0.0.1:9901/metrics | grep chv_node_vm_count'
```

### Rolling Upgrade

> **Status:** No automated rolling-upgrade surface exists. The control-plane
> upgrade stack (`UpgradeOrchestrator`, `SystemdNodeUpgrader`) was deleted as
> dead code in PR #213, and the `chvctl upgrade` subcommands — which targeted
> `/v1/upgrades` BFF routes that were never registered — were removed in #427.
> Use the manual procedure below ("Upgrade Procedure (Manual)") together with
> `chvctl node drain` to upgrade nodes one at a time.

### Compatibility Matrix

The compatibility matrix defines allowed version ranges per component. Located at `/etc/chv/compat-matrix.toml`:

```toml
[compatibility]
[[compatibility.entry]]
component = "agent"
min_version = "0.1.0"
max_version = "1.0.0"

[[compatibility.entry]]
component = "stord"
min_version = "0.2.0"
max_version = "1.0.0"
```

**Check compatibility via API:**
```bash
chvctl health cluster
```

The control plane validates the matrix at upgrade time. If the target version falls outside the allowed range, the upgrade is rejected before any drain begins.

### Upgrade Procedure (Manual)

1. Back up database and certificates
2. Build or download new release tarball
3. Stop services in order: agent → stord/nwd → control plane
4. Install new binaries
5. Start control plane (runs migrations automatically)
6. Start stord and nwd
7. Start agent
8. Verify: `systemctl status` and `curl /health`

---

## Multi-Node Operations

> **Qualification status:** The procedures in this section are
> code-supported but unqualified. The qualified topology is single-host,
> capped at KVM-VERIFIED.

### Migration Monitoring

Key Prometheus metrics to watch during migration (stop-the-world until #394
is proven; the finalize wait is bounded at 30 s by default and a timeout is
counted under `chv_stord_migration_errors_total{reason="finalize_ack_timeout"}`):

| Metric | Type | What it tells you |
|--------|------|-------------------|
| `chv_migration_phase` | Gauge | Current phase (0=Pending, 1=PreCopyDisk, 2=ConvergingDisk, 3=MemoryMigration, 4=Paused, 5=Completed, 6=Failed, 7=RolledBack) |
| `chv_migration_bytes_transferred` | Counter | Total bytes copied so far |
| `chv_migration_duration_seconds` | Histogram | End-to-end migration time by outcome |
| `chv_migration_dirty_blocks` | Gauge | Remaining dirty blocks during convergence |
| `chv_migration_restart_reconciled_total` | Counter | Rows reconciled at startup, labelled by `recovered_phase`; nonzero means a restart interrupted migrations |
| `chv_migration_progress_dropped_total` | Counter | Late progress reports dropped after a terminal transition, labelled by `stored_phase` and `reported_phase`; `reported_phase="Completed"` after recovery is especially important |
| `migration_reaper_resume_skipped_total` | Counter | Source-VM resume attempts skipped, labelled by `reason` (`agent_cancel_pending`, `agent_cancel_settling`, `agent_cancel_refused`, `sender_active`, `source_node_not_reporting`, `interrupted_ambiguous`, `resume_blocked`) |
| `chv_migration_agent_cancel_total` | Counter | Detached agent migration tasks signalled, labelled by `outcome` (`cancelled`, `not_tracked`); `not_tracked` is success (agent restarted / task already finished) |
| `chv_migration_agent_cancel_failures_total` | Counter | Agent cancel attempts that did not confirm, labelled by `reason` (`resolve_error`, `unreachable`, `connect_timeout`, `timeout`, `rpc_error`, `permanent`); `permanent` parks the row in `agent_cancel_last_error` |
| `chv_migration_destination_resume_failures_total` | Counter | Destination VM resumes that failed after an agent-reported `Completed`; placement still moved, so the VM may be paused on the destination and `migrations.error_message` explains why |

**Expected phase durations** (100GB disk, 16GB RAM):

| Phase | Typical | Alarm threshold |
|-------|---------|-----------------|
| PreCopyDisk | 10-60 min | >90 min |
| ConvergingDisk | 1-10 min per round | >30 min total |
| MemoryMigration | 30s-5 min | >10 min |
| Paused (final sync) | <5s | >30s |

**When to intervene:**

- `dirty_blocks` not converging after 5 rounds: check I/O write rate on source VM
- Phase stuck in MemoryMigration >10 min: possible network partition between nodes
- Migration rolled back repeatedly: check source/destination connectivity and disk I/O saturation

```bash
# Check active migrations
sqlite3 "file:/var/lib/chv/controlplane.db?mode=ro" \
  "SELECT migration_id, vm_id, phase, bytes_transferred, dirty_blocks_remaining \
   FROM migrations WHERE completed_at IS NULL;"

# Watch migration progress
watch -n5 'curl -s http://127.0.0.1:9901/metrics | grep chv_migration'
```

### VXLAN Overlay Troubleshooting

> **Status:** The VXLAN/WireGuard fabric fails closed in the qualified
> configuration. The qualified network topology is a single-host local
> bridge with taps.

| Symptom | Diagnostic | Resolution |
|---------|-----------|------------|
| Stale FDB entries | `bridge fdb show dev vxlan<VNI>` | Trigger reconcile: restart chv-agent on affected node |
| VXLAN interface down | `ip link show \| grep vxlan` | Check `chv-nwd` logs; verify VTEP registration in DB |
| VNI exhaustion | `sqlite3 "file:/var/lib/chv/controlplane.db?mode=ro" "SELECT count(*) FROM vni_allocations WHERE released_at IS NULL;"` | Release unused VNIs or expand VNI range |
| Cross-node VM unreachable | `tcpdump -i <vtep_interface> udp port 4789` | Verify UDP/4789 not blocked by firewall between nodes |
| MTU issues / fragmentation | `ping -M do -s 1400 <remote_vm_ip>` | Check `[overlay] inner_mtu` in nwd.toml; ensure outer MTU >= inner + 50 |

```bash
# Verify VTEP registry
sqlite3 "file:/var/lib/chv/controlplane.db?mode=ro" \
  "SELECT node_id, vtep_ip, vtep_port FROM vtep_entries;"

# Check VNI allocation for a network
sqlite3 "file:/var/lib/chv/controlplane.db?mode=ro" \
  "SELECT network_id, vni, allocated_at FROM vni_allocations WHERE network_id = '<ID>';"

# Force overlay reconciliation on a node
systemctl restart chv-nwd
```

### eBPF Policy Troubleshooting

| Symptom | Diagnostic | Resolution |
|---------|-----------|------------|
| Program load failure | `journalctl -u chv-nwd \| grep "eBPF"` | Verify `/usr/lib/chv/ebpf/policy_tc.o` exists; check kernel version >= 5.10 |
| Rules not applied | `tc filter show dev <tap> egress` | Check that clsact qdisc is attached; verify rule_map entries |
| Stats all zeros | `journalctl -u chv-nwd \| grep "stats_map"` | eBPF programs may be in stub mode (libbpf-rs not available) |
| VM traffic blocked unexpectedly | Check `chv_ebpf_packets_total{action="denied"}` metric | Review security rules via API; check default_action in `[ebpf]` config |
| Rate limiting too aggressive | Check `chv_ebpf_bytes_total` vs configured rate | Adjust rate_bps in the VM's rate limit policy |

```bash
# Check if eBPF programs are loaded on a tap interface
tc filter show dev tap-<vm_short_id> egress

# View eBPF stats (via metrics endpoint)
curl -s http://127.0.0.1:9901/metrics | grep chv_ebpf

# Verify eBPF object files exist
ls -la /usr/lib/chv/ebpf/policy_tc.o
```

### Backup & Recovery for Multi-Node State

The SQLite backup (see above) automatically includes all multi-node state:

- **VTEP registry**: `vtep_entries` table (node-to-VTEP-IP mapping)
- **VNI allocations**: `vni_allocations` table (network-to-VNI mapping with cooldown)
- **Migration records**: `migrations` table (active and completed migrations)
- **FDB state**: Reconstructed at recovery time from VTEP registry + VM placement

**Recovery procedure for overlay state:**

1. Restore SQLite backup (standard procedure above)
2. Restart control plane: `sudo systemctl restart chv-controlplane`
3. Restart agents on all nodes: `sudo systemctl restart chv-agent`
4. Agents will re-register VTEPs and control plane will reconcile overlay state
5. Verify: `curl -s http://127.0.0.1:8080/v1/nodes | jq '.[].vtep_ip'`

**If overlay state is corrupt but VMs are running:**

```bash
# VMs continue running with stale FDB — connectivity may be intermittent
# Force full overlay rebuild (stop the control plane first — see
# Live Database Access):
sudo systemctl stop chv-controlplane
sqlite3 /var/lib/chv/controlplane.db "DELETE FROM vtep_entries;"
sudo systemctl start chv-controlplane
# Then restart all agents to re-register
```

---

## Security Hardening

- Replace self-signed CA with organization PKI
- Rotate bootstrap tokens after each use
- Restrict `/etc/chv/certs/` to `root:chv` with `640` permissions
- Run `chv-stord` under a dedicated service account with device/path allowlists
- Enable firewall rules limiting gRPC port 8443 to known hypervisor IPs
- Set `CHV_COOKIE_SECURE=1` when the BFF is reachable only over TLS (adds
  `Secure` to the session and hint cookies)

### Browser sessions, revocation, and console tickets (#660)

**The browser credential is an HttpOnly cookie.** `POST /v1/auth/login`
returns no JWT in its response body. It sets:

| Cookie | Scope | Purpose |
|--------|-------|---------|
| `chv_session` | `HttpOnly`, `SameSite=Strict`, `Path=/`, `Max-Age=86400` | The session JWT. Script cannot read it. |
| `chv_role` | script-readable, non-credential | Role hint used only to hide UI controls; the server never authorizes from it. |
| `chv_logged_in=1` | script-readable, non-credential | Authenticated-session hint for the SPA login redirect. |

The UI no longer writes tokens to `localStorage` or `sessionStorage`. Non-browser
clients that need a bearer token (for example `chvctl`) must use the separate
API-only endpoint `POST /api/v1/auth/login`.

**Server-side revocation.** Migration `0066_user_token_version.sql` adds
`users.token_version INTEGER NOT NULL DEFAULT 0`. Every newly issued session
JWT carries a `token_version` claim; the auth extractor compares it with the
database on **every authenticated request**, so revocation is immediate.
`POST /v1/auth/logout`, a successful password change, and an admin
password reset or role change (including demotion) increment the counter, so
every token issued earlier — including a copied one — fails immediately with:

- `401 SESSION_REVOKED` — the token's version is stale, or the user row is gone.
- `503 AUTH_UNAVAILABLE` — the lookup itself failed (database outage).

The per-request read is deliberate fail-closed behavior: if the database is
unavailable, a cookie that looks valid still yields `503 AUTH_UNAVAILABLE`
rather than being trusted without a revocation check.

Pre-#660 tokens carry no `token_version` claim (and no `aud`). During the
rollout they are still accepted for users that exist — the extractor does a
cheap existence/version lookup and rejects a legacy token whose `sub` has no
user row. They cannot be revoked server-side, so they remain a bounded
24-hour `exp` gap. To close it immediately:

1. Ask users to log out and back in, or rotate the JWT secret (invalidates
   all tokens); or
2. Set `CHV_REQUIRE_TOKEN_VERSION=1` on the BFF/control plane. This rejects
   **every** token without a `token_version` claim outright (401
   `SESSION_REVOKED`). Default is off during rollout; enable it after the
   upgrade window or after a suspected compromise.

Rolling the migration back is safe — older binaries ignore the extra column —
but it disables revocation until the column and the #660 binary are restored.

**API-token scopes fail closed.** `api_tokens.scope` is `NOT NULL` and
accepts exactly `full` (user's role) and `readonly` (viewer). Any other
value — empty, mixed case, or an unknown future scope — is rejected with
`403 INVALID_TOKEN_SCOPE`; it never falls back to the user's role. (A NULL
is impossible through the schema; a hand-edited NULL row fails token
deserialization as `401` before the scope check.) Audit existing rows
before upgrading:

```sql
SELECT scope, COUNT(*) FROM api_tokens GROUP BY scope;
```

Fix or recreate any row whose scope is not `full` or `readonly`.

**Console tickets are audience-bound and out-of-URL.** Console tickets are
minted with `aud: "chv:console"` and a 60-second TTL; session JWTs carry
`aud: "chv:session"`. Each side enforces its own audience, so a session JWT
or API token cannot open a console and a console ticket cannot be replayed
against the session API. The agent reads the ticket from the
`Sec-WebSocket-Protocol` handshake header, not from `?token=`. The BFF
returns the URL and ticket separately; the UI calls
`new WebSocket(url, [ticket])`. nginx forwards the subprotocol header by
default.

---

## Architecture Designer day-2 operations

The Architecture Designer (see [`docs/specs/architecture-designer/`](specs/architecture-designer/)) persists desired-state topologies, plans, apply runs, drift reports, and fleet inventory snapshots in the controlplane SQLite database. Day-2 ops mirror the rest of CHV: SQLite backups + targeted retention.

### Backup

The Architecture Designer ships six tables (migrations `0046`–`0051`):

| Table | Purpose |
|-------|---------|
| `architecture_topologies` | Saved topology metadata (id, name, owner, latest version pointer) |
| `architecture_versions` | Immutable, append-only history of every saved YAML revision |
| `architecture_plans` | Generated plans pending or completed (apply / destroy modes) |
| `architecture_apply_runs` | Apply / destroy execution records linked to CHV tasks |
| `architecture_drift_reports` | Drift findings per architecture per check |
| `inventory_snapshots` | Fleet observed state captured for plan determinism and drift baselines |

The standard SQLite backup procedure documented earlier (`sqlite3 ... ".backup"`) covers all six tables — they live in `/var/lib/chv/controlplane.db` along with everything else. No separate dump is required.

**Recommended cadence:**
- Hourly online backup retained 24h.
- Daily backup retained 30d.
- Weekly off-host backup retained 12 months.

**Recovery posture:**
- **RPO** ≤ 1 hour (last hourly backup).
- **RTO** ≤ 15 minutes for control-plane restore (stop service → copy file → start) on a single-node deployment.
- A restored database loses any plans / apply runs created after the backup window. Topologies and accepted versions are unaffected if backups are taken before user-facing edits.

### Starter topologies — opt-out and re-seed

On first boot the controlplane seeds six canonical reference topologies into `architecture_topologies` (see [`docs/plans/2026-06-16-starter-topologies-and-auto-seed.md`](plans/2026-06-16-starter-topologies-and-auto-seed.md)). They land as `status = draft`, `owner_user_id = NULL`, named `starter-NN-<slug>`, and the dashboard never auto-applies them — operators clone, not edit, starters to make their own.

The seed is gated by a sentinel row `seed_starters_completed` in `system_settings`. A single atomic `UPSERT … WHERE value = '0'` flips the sentinel to `'1'` and claims the seed; only the process whose UPDATE affects exactly one row owns the run. Subsequent boots see `value = '1'` and skip cleanly.

**Opt out before first boot** (e.g. on a real production deployment where the operator does not want demo content):

```bash
sudo systemctl stop chv-controlplane
sqlite3 /var/lib/chv/controlplane.db \
  "INSERT OR REPLACE INTO system_settings (key, value, updated_at)
   VALUES ('seed_starters_completed', '1', strftime('%Y-%m-%dT%H:%M:%fZ','now'));"
sudo systemctl start chv-controlplane
# verify: dashboard shows zero topologies; controlplane logs note "starter topologies already seeded; skipping"
```

**Re-seed missing starters** (operator deleted some starter rows and wants them back):

```bash
sudo systemctl stop chv-controlplane
sqlite3 /var/lib/chv/controlplane.db \
  "UPDATE system_settings SET value = '0',
       updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
   WHERE key = 'seed_starters_completed';"
sudo systemctl start chv-controlplane
# the seeder's per-starter AlreadyExists branch means existing rows are left untouched;
# only the missing ones get re-inserted. The sentinel flips back to '1' on success.
```

**Caveats:**
- The sentinel must be exactly `'0'` to trigger re-seed. Trailing whitespace (`'0 '`) does NOT match — by design, to keep typo-resistant operator overrides idempotent.
- A control plane that cannot read or write `system_settings` refuses to start. Sentinel-write failures are fail-closed; per-fixture parse/insert failures are fail-open and logged via `tracing::error!` so a single bad fixture does not block boot.

### Retention

Until the periodic pruner ships (tracked in [`docs/plans/2026-06-16-snapshot-pruner-followup.md`](plans/2026-06-16-snapshot-pruner-followup.md)), retention is best-effort and operator-driven. Recommended policy:

| Table | Retention | Rationale |
|-------|-----------|-----------|
| `architecture_topologies` | Indefinite | Small, user-curated, no churn. |
| `architecture_versions` | Indefinite | Append-only history of intent; per ADR-003-Designer the YAML is the source of truth. Small. |
| `architecture_plans` | 30 days post-terminal status | Per ADR-005-Designer; expired/discarded plans are not re-runnable. Active plans never deleted. |
| `architecture_apply_runs` | 90d for `succeeded`, indefinite for `failed`/`partially_failed` | Failed runs are audit material. Successful runs are operational telemetry. |
| `architecture_drift_reports` | 14 days, **but** retain latest report per architecture indefinitely | Most recent report is the user-facing "current drift" view. |
| `inventory_snapshots` | 7 days | High churn; only the recent ones drive plan determinism. |

Operators who need to free space before the pruner ships can run targeted
deletes (stop the control plane first — manual writes to the live database
are unsafe; see [Live Database Access](#live-database-access)):

```bash
sudo systemctl stop chv-controlplane
# Drop drift reports older than 14 days, keeping the latest per architecture
sqlite3 /var/lib/chv/controlplane.db <<'SQL'
DELETE FROM architecture_drift_reports
WHERE id NOT IN (
    SELECT id FROM (
        SELECT id, ROW_NUMBER() OVER (PARTITION BY architecture_id ORDER BY created_at DESC) AS rn
        FROM architecture_drift_reports
    ) WHERE rn = 1
)
AND created_at < datetime('now', '-14 days');
SQL

# Drop inventory snapshots older than 7 days
sqlite3 /var/lib/chv/controlplane.db \
  "DELETE FROM inventory_snapshots WHERE created_at < datetime('now', '-7 days');"

sudo systemctl start chv-controlplane
```

Always take a backup before manual deletes, and stop the control plane
before writing (see [Live Database Access](#live-database-access)).

### Monitoring

Every daemon exposes Prometheus metrics on its dedicated `metrics_bind` listener (loopback by default): control plane `127.0.0.1:9103`, stord `127.0.0.1:9101`, nwd `127.0.0.1:9102`, and the packaged agent `127.0.0.1:9901`. The Architecture Designer metrics are served there through the shared process registry. The legacy `/metrics` route on the control plane's `http_bind` (default `127.0.0.1:8080`) remains admin-gated for compatibility, but it is not the scraper path — the dedicated listener needs no admin credential. See [OBSERVABILITY.md](OBSERVABILITY.md) for the scrape auth model. The label vocabularies below match the actual emitter source (`crates/chv-webui-bff/src/metrics_apply.rs` and `metrics_drift.rs`); writing alerts against label values the emitter does not produce yields silent dead-on-arrival rules.

```
# Apply attempts (counter)
# status ∈ { started, enqueued, failed }
#   - started:  incremented on every POST /v1/architectures/apply entry,
#               before pre-condition guards run
#   - enqueued: apply_plan returned Ok and the run was accepted for execution
#   - failed:   apply_plan returned Err (4xx pre-condition or 5xx store)
# Note: this counter records the BFF-side enqueue lifecycle. Terminal
# orchestrator outcomes (succeeded / partially_failed / cancelled) are
# recorded in the `architecture_apply_runs.status` column today; a future
# emitter on the orchestrator terminal-state writeback can extend this
# counter or introduce `chv_architecture_apply_terminal_total`.
chv_architecture_apply_total{status="started|enqueued|failed"}

# Apply latency (histogram, BFF handler entry → response)
chv_architecture_apply_duration_seconds_bucket{...}

# Drift checks (counter)
# status ∈ { no_drift, drifted, unknown, check_failed, cache_hit }
#   - no_drift:     fresh compute returned an empty findings list
#   - drifted:      fresh compute returned at least one finding
#   - unknown:      reserved for the wire enum's DriftStatus::Unknown variant
#   - check_failed: snapshot capture or YAML parse failed; the BFF persisted
#                   a check_failed report and returned 200
#   - cache_hit:    the most recent persisted report was within the 5-minute
#                   TTL and force_refresh was false
chv_architecture_drift_total{status="no_drift|drifted|unknown|check_failed|cache_hit"}
```

Recommended alert templates (Prometheus `for:` durations are starting points; tune per environment):

```yaml
- alert: ArchitectureDriftCheckFailing
  expr: increase(chv_architecture_drift_total{status="check_failed"}[5m]) > 0
  for: 5m
  labels: { severity: page }
  annotations:
    summary: "Architecture drift checker is failing"
    description: "Drift checks have returned check_failed in the last 5 minutes — fleet visibility is degraded."

- alert: ArchitectureApplySlow
  expr: histogram_quantile(0.99, rate(chv_architecture_apply_duration_seconds_bucket[15m])) > 600
  for: 15m
  labels: { severity: ticket }
  annotations:
    summary: "Architecture apply p99 > 10 minutes"
    description: "Apply runs are taking longer than expected; investigate task-system backlog."

- alert: ArchitectureApplyEnqueueFailureRate
  # `failed` here is the BFF enqueue-time failure (pre-condition or store);
  # it does NOT cover orchestrator-side terminal failures because those are
  # not surfaced as a Prometheus counter today (see Monitoring note above).
  expr: |
    sum(rate(chv_architecture_apply_total{status="failed"}[15m]))
      / clamp_min(sum(rate(chv_architecture_apply_total{status=~"started|failed"}[15m])), 1e-9) > 0.2
  for: 30m
  labels: { severity: ticket }
  annotations:
    summary: "Architecture apply enqueue failure rate > 20% over 15m"
    description: >
      More than 20% of POST /v1/architectures/apply requests are failing the
      pre-condition or store guard. For terminal orchestrator outcomes
      (succeeded / partially_failed / cancelled), query
      `architecture_apply_runs` directly until a dedicated counter ships.
```

> **Label-vocabulary contract**: the bundled rules in `monitoring/rules/chv.yml` and `recording.yml` reference only labels the emitter actually sets. CI runs `scripts/check-metric-labels.sh` on every push to enforce that contract — see [`docs/specs/adr/009-logging-and-observability.md`](specs/adr/009-logging-and-observability.md). If you extend an emitter to add a new label key, run the gate locally before opening the PR.

### Troubleshooting

**Drift checks return `CheckFailed` repeatedly**

| Diagnostic | Resolution |
|-----------|------------|
| `journalctl -u chv-controlplane \| grep architecture_drift` for the underlying error | Most common cause: a referenced resource (network, datastore) was deleted out-of-band. Re-pin the architecture's baseline by re-running `apply` after editing the topology, or accept the current drift by writing a new version. |
| Inventory snapshot stale (look for `inventory_snapshots.created_at` > 1h old) | Restart the controlplane snapshot loop: `systemctl restart chv-controlplane`. The next drift check uses a fresh snapshot. |

**Apply stuck in `Applying` state**

The apply state machine uses CAS guards (compare-and-set on `status`) to ensure exactly one writer. A stuck `Applying` row usually means the worker process died mid-run.

```bash
# Inspect the run
sqlite3 "file:/var/lib/chv/controlplane.db?mode=ro" \
  "SELECT id, architecture_id, plan_id, status, started_at, updated_at \
   FROM architecture_apply_runs WHERE status = 'applying';"

# Check that no live task is still in flight
sqlite3 "file:/var/lib/chv/controlplane.db?mode=ro" \
  "SELECT operation_id, kind, state FROM operations \
   WHERE meta_json LIKE '%<APPLY_RUN_ID>%';"
```

If the linked task is `Failed` / `Cancelled` and no controlplane process is holding the row (no recent `updated_at`), reset the run to `failed` so a retry is possible (stop the control plane first — see [Live Database Access](#live-database-access)):

```bash
sudo systemctl stop chv-controlplane
sqlite3 /var/lib/chv/controlplane.db <<SQL
UPDATE architecture_apply_runs
SET status = 'failed',
    error_summary = 'manual recovery: stuck applying',
    updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')
WHERE id = '<APPLY_RUN_ID>' AND status = 'applying';
SQL
sudo systemctl start chv-controlplane
```

The CAS guard makes this safe: if a live worker still owns the row, the `WHERE status = 'applying'` clause races the update without corrupting state. Always restart the controlplane first if there is any doubt.

**Plan returns `PLAN_EXPIRED` on retry within 15 minutes**

Plans expire 15 minutes after generation (per ADR-005-Designer). If a fresh plan is rejected as expired, suspect clock skew between the BFF process and the controlplane.

| Diagnostic | Resolution |
|-----------|------------|
| `date -u` on each host running CHV components | Sync clocks: `sudo chronyc -a makestep` or `sudo systemctl restart systemd-timesyncd`. |
| Inspect the plan row: `SELECT created_at, expires_at FROM architecture_plans WHERE id = '<PLAN_ID>';` | If `expires_at - created_at` is not 15 minutes, the issuing process has wrong wall-clock; restart that process after the clock fix. |
| Container deployments with shared host clock | Verify NTP is enabled on the host; containers inherit host time, so a host-level fix propagates. |

If clocks are correct and the plan still expires immediately, file a bug — the controlplane and BFF should always agree on plan expiry.
