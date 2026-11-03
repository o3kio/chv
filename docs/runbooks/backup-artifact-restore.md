# Runbook: Backup Artifact Restore

**Severity:** SEV-1 (data loss) / SEV-2 (production VM unavailability)<br>
**Automation Level:** **Manual** — CHV has no backup or restore engine<br>
**Estimated Time:** 10–30 minutes<br>
**Prerequisites:**
- An off-host disk artifact produced by **your own** tooling (S3/NFS/object store), or a pre-fence CHV job whose artifact still exists
- Access to the artifact destination (S3 bucket or NFS mount)
- `qemu-img` installed on the target node
- Target VM is **stopped** before disk replacement
- Sufficient disk space on the target node for the artifact + working copy

> **CHV cannot create backup artifacts.** The backup/restore API is
> hard-fenced (`501 NOT_IMPLEMENTED`): no job runs, no artifact is shipped,
> and no restore executes. This runbook documents a **manual** disk
> replacement using an artifact that already exists outside CHV. If you have
> no such artifact, there is nothing to restore — start a real backup process
> with your own tooling now. See
> [`DEPLOYMENT-ARCHITECTURE.md`](../DEPLOYMENT-ARCHITECTURE.md) §7.

---

## 1. Identify the Artifact

If the artifact came from a pre-fence CHV job, its metadata is still in the
`backup_jobs` table and can be read directly. The API is **not** usable: every
`/v1/backups/*` route answers `501 NOT_IMPLEMENTED`.

> The `chvctl backup` command group was removed (#372) — its subcommands
> 404'd against routes that never existed.

### Via SQLite (direct DB access)

```bash
sudo sqlite3 "file:/var/lib/chv/controlplane.db?mode=ro" \
  "SELECT job_id, status, destination, storage_backend, checksum, size_bytes, completed_at
   FROM backup_jobs
   WHERE vm_id = '<VM_ID>' AND status = 'Succeeded'
   ORDER BY completed_at DESC LIMIT 5;"
```

Note:
- `job_id` — the artifact filename will be `{job_id}.backup`
- `destination` — the remote path (S3 key prefix or NFS mount path); named `target_path` in the API responses
- `storage_backend` — `s3`, `nfs`, or `null`
- `checksum` — SHA256 of the artifact (verify after download)

---

## 2. Locate and Download the Artifact

### 2a. S3 Destination

```bash
# Extract bucket and prefix from the schedule configuration
# (or inspect the destination field directly)

# If you have the schedule_id:
SCHEDULE_ID=$(sudo sqlite3 "file:/var/lib/chv/controlplane.db?mode=ro" \
  "SELECT schedule_id FROM backup_jobs WHERE job_id = '<JOB_ID>';")
S3_DEST=$(sudo sqlite3 "file:/var/lib/chv/controlplane.db?mode=ro" \
  "SELECT destination FROM backup_schedules WHERE schedule_id = '$SCHEDULE_ID';")

# Example: s3://my-backup-bucket/chv/vm-backups/
# The artifact key is: {prefix}/{job_id}.backup

aws s3 cp "s3://my-backup-bucket/chv/vm-backups/<JOB_ID>.backup" \
  /tmp/<JOB_ID>.backup

# Or with a custom endpoint (MinIO, etc.):
aws s3 cp "s3://bucket/prefix/<JOB_ID>.backup" /tmp/<JOB_ID>.backup \
  --endpoint-url https://s3.example.com
```

### 2b. NFS Destination

```bash
# If the artifact lives on an NFS export your external backup tooling writes
# to, mount it:
sudo mount -t nfs backup-nfs.example.com:/exports/backups /mnt/backups

# Copy the artifact
cp /mnt/backups/<JOB_ID>.backup /tmp/<JOB_ID>.backup
```

### 2c. Null Destination (dev/test)

The artifact was never copied off the control plane. Check the local staging directory:

```bash
ls -la /run/chv/controlplane/backups/<JOB_ID>.backup
# If present, copy it:
cp /run/chv/controlplane/backups/<JOB_ID>.backup /tmp/<JOB_ID>.backup
```

> **Note:** The `/run/chv/controlplane/backups/` directory is tmpfs and may be cleared on reboot. Do not rely on it for production recovery.

---

## 3. Verify Artifact Integrity

```bash
# Compute SHA256 of the downloaded artifact
downloaded_checksum=$(sha256sum /tmp/<JOB_ID>.backup | awk '{print $1}')

# Compare with the expected checksum from the database
expected_checksum=$(sudo sqlite3 "file:/var/lib/chv/controlplane.db?mode=ro" \
  "SELECT checksum FROM backup_jobs WHERE job_id = '<JOB_ID>';")

if [ "$downloaded_checksum" = "$expected_checksum" ]; then
  echo "✅ Checksum matches"
else
  echo "❌ CHECKSUM MISMATCH — do not proceed with restore"
  echo "Expected: $expected_checksum"
  echo "Got:      $downloaded_checksum"
  exit 1
fi
```

---

## 4. Inspect the Artifact Format

CHV backup artifacts are Cloud Hypervisor snapshot files. Inspect before restoring:

```bash
# Check file type
file /tmp/<JOB_ID>.backup

# If it's a qcow2 image
qemu-img info /tmp/<JOB_ID>.backup

# If it's a directory (older Cloud Hypervisor versions or certain configurations)
ls -la /tmp/<JOB_ID>.backup/
```

The artifact may be:
- A single `qcow2` disk image
- A Cloud Hypervisor snapshot directory with `memory` and disk state

---

## 5. Prepare the Target VM

### 5a. Stop the VM

```bash
chvctl vm stop <VM_ID>
```

Verify it is stopped:

```bash
chvctl vm get <VM_ID> | grep status
```

### 5b. Locate the VM's Current Disk

```bash
# On the node hosting the VM
ls -la /var/lib/chv/agent/vms/<VM_ID>/
# Look for disk.qcow2 or similar
```

### 5c. Create a Safety Backup of Current State (Optional but Recommended)

```bash
sudo cp /var/lib/chv/agent/vms/<VM_ID>/disk.qcow2 \
  /var/lib/chv/agent/vms/<VM_ID>/disk-pre-restore.qcow2
```

---

## 6. Restore the Artifact

### 6a. Single Disk Image (qcow2)

```bash
# Replace the existing disk
sudo cp /tmp/<JOB_ID>.backup /var/lib/chv/agent/vms/<VM_ID>/disk.qcow2

# Ensure correct ownership
sudo chown -R chv:chv /var/lib/chv/agent/vms/<VM_ID>/
```

### 6b. Cloud Hypervisor Snapshot Directory

If the artifact is a snapshot directory:

```bash
# Remove old snapshot state (if any)
sudo rm -rf /var/lib/chv/agent/vms/<VM_ID>/snapshots/manual-restore

# Extract/place the snapshot
sudo mkdir -p /var/lib/chv/agent/vms/<VM_ID>/snapshots/manual-restore
sudo cp -a /tmp/<JOB_ID>.backup/* /var/lib/chv/agent/vms/<VM_ID>/snapshots/manual-restore/
sudo chown -R chv:chv /var/lib/chv/agent/vms/<VM_ID>/snapshots/

# Use the VM Snapshot Restore API to load it
curl -X POST https://controlplane.example.com/v1/vms/snapshots/restore \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "vm_id": "<VM_ID>",
    "snapshot_path": "/var/lib/chv/agent/vms/<VM_ID>/snapshots/manual-restore"
  }'
```

> **Note:** The backend-for-frontend (BFF) snapshot restore endpoint only accepts `vm_id` and `snapshot_id`, not a raw filesystem path. To restore from a manual path, use the Cloud Hypervisor API directly via the agent socket (see section 6c). Alternatively, copy the snapshot files into the expected snapshot directory and use the standard restore API.

### 6c. Direct Cloud Hypervisor API (last resort)

If CHV APIs are unavailable, use the Cloud Hypervisor API socket directly:

```bash
# Find the VM's API socket
VM_SOCK=/var/lib/chv/agent/vms/<VM_ID>/vm.sock

# Create VM config first (if VM is fully down)
# Then restore:
curl -X PUT --unix-socket "$VM_SOCK" \
  http://localhost/api/v1/vm.restore \
  -H "Content-Type: application/json" \
  -d "{\"source_url\": \"file:///var/lib/chv/agent/vms/<VM_ID>/snapshots/manual-restore\"}"
```

---

## 7. Start the VM and Verify

```bash
chvctl vm start <VM_ID>
```

Monitor boot:

```bash
# chv-agent logs
sudo journalctl -u chv-agent -f | grep <VM_ID>
```

> **Note:** There is no `chvctl vm console` command. To access the serial console, use the Web UI (Default Cloud → Hosts → Instances → `<VM_NAME>` → Console tab), which retrieves a console URL from `GET /v1/vms/{vm_id}/console`.

Verify guest integrity:

Start the VM and verify filesystem health via SSH, or inspect the VM boot logs for filesystem errors. There is no `chvctl vm guest-exec` command.

---

## 8. Record the Restore in CHV

There is **no API for this**: `POST /v1/backups/restores` is fenced and
answers `501 NOT_IMPLEMENTED`. If a pre-fence restore row exists, you may
record the outcome directly in the database. This is a live-database write:
stop the control plane first and restart it after (see
[`docs/OPERATIONS.md`](../OPERATIONS.md), section "Live Database Access").

```bash
sudo systemctl stop chv-controlplane
sudo sqlite3 /var/lib/chv/controlplane.db \
  "UPDATE backup_restores SET status = 'Succeeded', completed_at = datetime('now')
   WHERE backup_job_id = '<JOB_ID>' AND target_vm_id = '<VM_ID>';"
sudo systemctl start chv-controlplane
```

If no row exists, record the manual restore in **your own** incident/tracking
system rather than fabricating CHV state.

---

## Troubleshooting

| Symptom | Diagnostic | Resolution |
|---------|-----------|------------|
| `CHECKSUM MISMATCH` after download | Network corruption or wrong artifact | Re-download; verify bucket/path |
| `qemu-img info` shows corruption | Incomplete download or Cloud Hypervisor version mismatch | Re-download; check artifact size matches `size_bytes` in DB |
| VM fails to start after restore | Disk format mismatch | Convert with `qemu-img convert -O qcow2` |
| Cloud Hypervisor API returns `BadRequest` on restore | Snapshot format incompatible with the Cloud Hypervisor version | Check the Cloud Hypervisor version at backup time vs now; may need intermediate conversion |
| Permission denied on disk | Wrong ownership | `sudo chown -R chv:chv /var/lib/chv/agent/vms/<VM_ID>/` |
| Guest filesystem errors | Disk image from running VM | Always stop VM before creating snapshots; `fsck` the restored disk |

## Automation Status

There is no restore worker and no backup engine. The API is hard-fenced
(`501 NOT_IMPLEMENTED`) and there is no roadmap date. Treat this manual
runbook as the supported procedure for externally produced artifacts, and
treat pod-level backup/restore as out of scope for CHV until an end-to-end
create → ship → restore → verify capability ships with tests.

## Related Runbooks

- [VM Snapshot Restore](vm-snapshot-restore.md) — For restoring from local snapshots (automated)
- [Control Plane DR](control-plane-dr.md) — If the control plane itself is lost
