# Runbook: Full Site Recovery

> Qualification status: the multi-node rebuild procedures in this runbook
> are code-supported but unqualified. The qualified topology is
> single-host. See `docs/OPERATIONS.md` for details.
>
> **Backup scope:** CHV does not back up tenant data. Its backup/restore API
> is hard-fenced (`501 NOT_IMPLEMENTED`) and no artifact is produced. The
> "backup artifacts" this runbook restores must be produced by **your own**
> tooling outside CHV (or predate the fence). See
> [`DEPLOYMENT-ARCHITECTURE.md`](../DEPLOYMENT-ARCHITECTURE.md) §7.

**Severity:** SEV-1 (complete infrastructure loss)<br>
**Automation Level:** Manual<br>
**Estimated Time:** 1–4 hours (depending on VM count and artifact destination)<br>
**Prerequisites:**
- Access to off-site disk artifacts produced by external tooling (S3 or NFS)
- Replacement hardware, or VMs in an external cloud provider, for all CHV roles (control plane and hypervisor nodes)
- CHV installation media/packages
- Network configuration (IPs, DNS, VLANs) documented or recoverable
- Database backup from the control plane
- Certificate backup (or willingness to rotate all certs)

---

## 1. Recovery Order

CHV recovery follows a strict startup order. Do not skip steps.

| Order | Component | Why It Must Come First |
|-------|-----------|------------------------|
| 1 | Control-plane host | Source of truth for all metadata |
| 2 | Network/storage infrastructure | Hypervisor nodes need connectivity to the control plane and storage |
| 3 | Hypervisor nodes (`chv-agent`) | Host the VMs |
| 4 | Storage nodes (`chv-stord`) | Volume backends |
| 5 | VMs (from external disk artifacts) | Final workload restoration |

---

## 2. Rebuild the Control Plane

Follow [Control Plane DR](control-plane-dr.md) — Scenario A (control-plane host rebuild) in full.

Key additional steps for a full site recovery:

### 2a. Verify External Disk Artifacts Are Accessible

Before rebuilding the hypervisor nodes, confirm you can reach the location
where **your own** backup tooling writes disks (CHV produces none):

```bash
# S3
aws s3 ls s3://my-backup-bucket/chv/ --recursive | head -20

# NFS
showmount -e backup-nfs.example.com
```

### 2b. Document the Original Topology

If the old control plane and the DB backup are both lost, reconstruct the topology from memory or external documentation. Minimum required:

- List of all VM IDs and their hypervisor assignments
- Volume IDs and storage backend configuration
- Network/VLAN assignments
- External backup tooling configuration (CHV has no backup schedules to restore)

> **Prevention:** Export this data regularly:
> ```bash
> chvctl vm list > /backup/topology-vms-$(date +%Y%m%d).json
> chvctl volume list > /backup/topology-volumes-$(date +%Y%m%d).json
> chvctl node list > /backup/topology-nodes-$(date +%Y%m%d).json
> ```

---

## 3. Rebuild Hypervisor Nodes

### 3a. Install the node daemons

```bash
# On each hypervisor node
dpkg -i chv-node_<version>_amd64.deb

# Or from tarball
tar xzf chv-<version>-linux-amd64.tar.gz
sudo ./install.sh
```

The `chv-node` package installs `chv-agent`, `chv-stord`, and `chv-nwd`.
`install.sh` installs the full stack. It has no component selection.

### 3b. Configure Agent

```bash
cat > /etc/chv/agent.toml <<'EOF'
[agent]
node_id = "<ORIGINAL_NODE_ID_OR_NEW>"
control_plane_url = "https://new-cp.example.com:443"
runtime_dir = "/var/lib/chv/agent"

[agent.tls]
ca_cert = "/etc/chv/certs/ca.crt"
client_cert = "/etc/chv/certs/agent.crt"
client_key = "/etc/chv/certs/agent.key"
EOF
```

### 3c. Enroll with Control Plane

```bash
# Generate a bootstrap token on the control plane
TOKEN=$(openssl rand -hex 32)
echo "$TOKEN" | sudo tee /etc/chv/bootstrap.token.new

# Enroll agent
sudo chvctl agent enroll --token "$TOKEN" --server https://new-cp.example.com:443

# Start agent
sudo systemctl enable --now chv-agent
```

### 3d. Verify Agent Registration

```bash
# On the control plane
chvctl node list | jq '.nodes[] | {node_id, status}'
```

---

## 4. Rebuild Storage Nodes (`chv-stord`)

### 4a. Install `chv-stord`

```bash
dpkg -i chv-node_<version>_amd64.deb
```

The `chv-node` package installs `chv-agent`, `chv-stord`, and `chv-nwd`.

### 4b. Configure Storage Backend

**Local backend:**

```bash
cat > /etc/chv/stord.toml <<'EOF'
[stord]
backend = "local"
data_dir = "/var/lib/chv/stord"
EOF
```

**Ceph backend:**

```bash
cat > /etc/chv/stord.toml <<'EOF'
[stord]
backend = "ceph"
ceph_pool = "chv-volumes"
ceph_user = "chv"
EOF
```

### 4c. Start `chv-stord`

```bash
sudo systemctl enable --now chv-stord
```

---

## 5. Restore VMs from Backup Artifacts

For each VM that needs recovery:

### 5a. Re-create VM Metadata (if DB was lost)

If you restored the control plane DB, VM metadata should already exist. Skip to 5b.

If the DB was lost, recreate VMs:

```bash
# Create VM definition
curl -X POST http://localhost:8080/v1/vms \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "<VM_NAME>",
    "vcpu_count": 4,
    "memory_mb": 8192,
    "node_id": "<HYPERVISOR_NODE_ID>"
  }' | jq '.vm_id'
```

Attach volumes:

```bash
curl -X POST http://localhost:8080/v1/vms/<VM_ID>/volumes \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"volume_id": "<VOLUME_ID>"}'
```

### 5b. Restore Disk from Backup Artifact

Follow [Backup Artifact Restore](backup-artifact-restore.md) for each VM.

Bulk restore script (if many VMs):

```bash
#!/bin/bash
# restore-all-vms.sh — run on the control plane

BACKEND="s3"
BUCKET="my-backup-bucket"
PREFIX="chv/vm-backups"

# List candidate artifact metadata from the restored control-plane DB.
# The backup API is fenced (501), so read SQLite directly (read-only).
jobs=$(sudo sqlite3 "file:/var/lib/chv/controlplane.db?mode=ro" \
  "SELECT job_id || char(9) || vm_id || char(9) || target_path
   FROM backup_jobs WHERE status = 'Succeeded' AND destination IS NOT NULL;")

while IFS=$'\t' read -r job_id vm_id target_path; do
  echo "Restoring VM $vm_id from job $job_id"

  # Download artifact
  aws s3 cp "s3://$BUCKET/$PREFIX/$job_id.backup" /tmp/$job_id.backup

  # Verify checksum
  expected=$(sqlite3 "file:/var/lib/chv/controlplane.db?mode=ro" \
    "SELECT checksum FROM backup_jobs WHERE job_id = '$job_id';")
  actual=$(sha256sum /tmp/$job_id.backup | awk '{print $1}')

  if [ "$expected" != "$actual" ]; then
    echo "  ❌ CHECKSUM MISMATCH for $job_id — skipping"
    continue
  fi

  # Stop VM
  chvctl vm stop "$vm_id" 2>/dev/null || true
  sleep 5

  # Copy disk to hypervisor (requires SSH access)
  node_ip=$(chvctl node get $(chvctl vm get "$vm_id" | jq -r '.node_id') | jq -r '.management_ip')
  scp /tmp/$job_id.backup "root@$node_ip:/var/lib/chv/agent/vms/$vm_id/disk.qcow2"

  # Fix ownership
  ssh "root@$node_ip" "chown -R chv:chv /var/lib/chv/agent/vms/$vm_id/"

  # Start VM
  chvctl vm start "$vm_id"

  echo "  ✅ Restored VM $vm_id"
done <<< "$jobs"
```

---

## 6. Restore Volumes

### 6a. Local Volumes

Local volumes were stored on `chv-stord` nodes. If the storage node was lost, the volumes are lost unless backed up externally.

### 6b. Ceph Volumes

Ceph volumes survive the loss of a `chv-stord` node (data is in the Ceph cluster). Simply:

1. Reinstall `chv-stord`
2. Connect to the same Ceph pool
3. Volumes are automatically available

### 6c. Recreate Lost Volumes

If volumes were lost:

```bash
# Create new volume
curl -X POST http://localhost:8080/v1/volumes \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "<VOLUME_NAME>",
    "size_gb": 100,
    "storage_backend": "local",
    "node_id": "<STOR_NODE_ID>"
  }' | jq '.volume_id'
```

If you have a volume backup artifact, restore it manually to the `chv-stord` data directory.

---

## 7. Backup Configuration

**There is nothing to reconfigure: CHV has no backup engine.** The
backup/restore API is hard-fenced (`501 NOT_IMPLEMENTED`), no schedules can be
created, and no new jobs are queued. Any `backup_schedules` rows present in a
restored database are inert; the worker will not create jobs from them.

Re-establish your **external** backup process (the one that produced the
artifacts you restored from) and verify it is writing to off-host storage
again. Do not attempt to create CHV backup schedules — the request will be
rejected with `501`.

> Pre-fence S3 credential rows remain encrypted at rest with
> `CHV_ENCRYPTION_KEY`; treat that key as part of the database backup set
> (see [`docs/OPERATIONS.md`](../OPERATIONS.md#backup-and-restore)).

---

## 8. Post-Recovery Validation

### 8a. Infrastructure Health

```bash
# All services running
sudo systemctl is-active chv-controlplane chv-agent chv-stord chv-nwd

# All nodes registered
chvctl node list | jq '.nodes | length'

# All VMs running or stopped as expected
chvctl vm list | jq '.vms[] | {vm_id, name, status, node_id}'
```

### 8b. VM Health Checks

Verify each recovered VM is running and reachable:

```bash
# For each critical VM
for vm in vm-1 vm-2 vm-3; do
  echo "Checking $vm..."
  chvctl vm get "$vm" | jq '{vm_id, status, node_id}'
  # Or verify via SSH:
  # ssh user@$vm_ip "uptime"
done
```

### 8c. Verify the Backup Surface Is Fenced

CHV does not back up tenant data. Confirm the fence is intact so nobody
mistakes a 501 for a transient failure during recovery:

```bash
curl -s -o /dev/null -w '%{http_code}\n' \
  http://localhost:8080/v1/backups/jobs -H "Authorization: Bearer $TOKEN"
# Expected: 501  (body code: NOT_IMPLEMENTED)
```

### 8d. Verify Your External Backup Process

There is no CHV backup cycle to run. Confirm **your own** backup tooling is
writing to off-host storage and that a restore drill is scheduled:

```bash
# Example only — this is your tooling, not CHV.
aws s3 ls s3://my-backup-bucket/chv/ --recursive | grep $(date +%Y%m%d)
```

---

## 9. Post-Incident Actions

1. **Document the root cause** in your incident tracker
2. **Update network diagrams** if they changed during rebuild
3. **Verify your external backup process is still running** — the incident may have disrupted it; CHV has no backup schedules to check
4. **Run a restore drill** within 48 hours to ensure the recovered system can actually restore from your external artifacts
5. **Update this runbook** if any steps didn't work as expected
6. **Treat tenant-data protection as your responsibility** — CHV does not implement backup/restore, so the manual steps here are the whole story

---

## Troubleshooting

| Symptom | Diagnostic | Resolution |
|---------|-----------|------------|
| Agent enrollment fails with `invalid token` | Token expired or wrong CA | Generate fresh token; verify CA cert matches |
| VMs won't start after restore | Disk images incompatible with the new Cloud Hypervisor version | Convert with `qemu-img convert`; check the Cloud Hypervisor release notes |
| Ceph volumes not visible | Wrong pool or user | Verify `stord.toml` matches old configuration |
| CHV backup endpoints return `501` | Expected — backup/restore is hard-fenced | Use external tooling; see [Backup Artifact Restore](backup-artifact-restore.md) |
| External S3 upload fails after recovery | Credentials rotated or bucket policy changed | Fix your backup tooling's credentials (not a CHV config) |
| High VM boot time after restore | Disk images need `virtio` drivers | Ensure VM config matches original (same disk bus) |

## Related Runbooks

- [Control Plane DR](control-plane-dr.md) — For control-plane-only failures
- [Backup Artifact Restore](backup-artifact-restore.md) — For single-VM recovery from an externally produced disk artifact
- [VM Snapshot Restore](vm-snapshot-restore.md) — For restoring from local snapshots
