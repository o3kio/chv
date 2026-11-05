# Runbook: Control Plane Disaster Recovery

**Severity:** SEV-1 (complete control plane outage)  
**Automation Level:** Partially automated (DB auto-backup) + manual steps  
**Estimated Time:** 15–30 minutes  
**Prerequisites:**
- Access to backup artifacts: SQLite DB backups, certificate backups, and VM backup artifacts
- A fresh or spare host meeting CHV control-plane requirements
- Same CHV version (or newer) installed on the replacement host
- DNS or load balancer updated to point to the replacement host

---

## 1. Assess the Scope of Failure

### Scenario A: Control-Plane Host Failed (VMs still running on hypervisor nodes)
- VMs continue running but cannot be managed
- No API, no UI, no scheduling
- **Recovery target:** Rebuild control plane, re-enroll agents

### Scenario B: Control Plane Database Corrupted
- Control plane starts but API returns errors
- SQLite corruption or disk full
- **Recovery target:** Restore DB from latest backup

### Scenario C: Certificate Authority (CA) Compromised or Lost
- mTLS between control plane and agents breaks
- Agents cannot reconnect
- **Recovery target:** Rotate certificates, re-enroll agents

---

## 2. Scenario A: Rebuild the Control-Plane Host

### 2a. Prepare the Replacement Host

Install the CHV control plane on the new host:

```bash
# Install from package (Debian/Ubuntu)
dpkg -i chv-controlplane_<version>_amd64.deb

# Or from release tarball
tar xzf chv-<version>-linux-amd64.tar.gz
sudo ./install.sh
```

`install.sh` installs the full stack. It has no component selection.

### 2b. Restore the Database

CHV automatically backs up the DB before migrations:

```bash
# List available auto-backups on the old host (mount the old disk if needed)
ls -lt /var/lib/chv/backups/

# Copy the latest backup to the new host
scp old-host:/var/lib/chv/backups/controlplane-*.db.bak /tmp/

# Or use your external backup:
# scp backup-server:/backups/chv-YYYYMMDD-HHMMSS.db /tmp/

# Stop the control plane before restoring
sudo systemctl stop chv-controlplane

# Restore the database
sudo cp /tmp/controlplane-*.db.bak /var/lib/chv/controlplane.db
sudo chown chv:chv /var/lib/chv/controlplane.db
sudo chmod 640 /var/lib/chv/controlplane.db
```

### 2c. Restore Certificates

```bash
# Extract certificate backup
sudo tar xzf /backup/chv-certs-YYYYMMDD.tar.gz -C /

# Ensure correct ownership
sudo chown -R root:root /etc/chv/certs/
sudo chmod 600 /etc/chv/certs/*.key
```

### 2d. Verify Configuration

```bash
# Check the control-plane config
cat /etc/chv/controlplane.toml

# Ensure listen addresses match the new host's IPs
# Ensure database path is correct
# Ensure JWT secret is set (or CHV_JWT_SECRET env var)

# Ensure the dedicated credential encryption key is present
sudo cat /etc/chv/encryption.env   # CHV_ENCRYPTION_KEY=<64 hex chars>
sudo stat -c '%a' /etc/chv/encryption.env   # must be 600
```

**Credential encryption key.** S3 backup credentials are encrypted at rest
(AES-256-GCM) with the dedicated `CHV_ENCRYPTION_KEY` in
`/etc/chv/encryption.env`, loaded by `chv-controlplane.service` via
`EnvironmentFile`. Both install surfaces (`install.sh` and the package
postinst) mint it automatically, so it exists on any host installed after
that wiring; on a replacement host it is **not** part of the DB restore —
copy it from the old host (or your secret backup) together with the
database, because a restored DB without the matching key has unreadable S3
credentials. If the file is lost, mint a fresh one:

```bash
sudo sh -c 'umask 077; printf "CHV_ENCRYPTION_KEY=%s\n" "$(openssl rand -hex 32)" > /etc/chv/encryption.env'
sudo chmod 0600 /etc/chv/encryption.env
```

**Legacy fallback hazard.** If `/etc/chv/encryption.env` is absent and
`CHV_JWT_SECRET` is set (pre-fallback-wiring installs, or env-driven
deployments), the control plane falls back to encrypting S3 credentials
under the **JWT signing secret** and logs a startup warning saying so. This
fallback is **deprecated**: it is retained only for backward compatibility
with existing deployments, and a dedicated key is required for all new
deployments. At startup, `chv-controlplane` logs the selected key source at
INFO (dedicated key, including an explicit `[secrets] credential_encryption`
key) or, for this fallback, a WARN prefixed `SECURITY WARNING: ...`, so the
key source is never implicit. A set-but-whitespace-only key fails closed;
unlike a fully unset `CHV_ENCRYPTION_KEY`, it does not fall back to
`CHV_JWT_SECRET`.

On such a host, rotating `CHV_JWT_SECRET` — a routine, sanctioned action —
invalidates every stored S3 credential: decryption fails closed
(`AuthFailed`) and the credentials must be re-entered in the backup
schedule settings. To migrate to a dedicated key without re-entry, copy the
current `CHV_JWT_SECRET` value into `/etc/chv/encryption.env` (same
derivation, so existing ciphertexts stay readable), restart
`chv-controlplane`, and rotate `CHV_JWT_SECRET` freely afterwards — the
encryption key no longer follows it. Alternatively, mint a fresh key as
above and re-enter the S3 credentials once.

### 2e. Start Control Plane

```bash
sudo systemctl start chv-controlplane
sudo systemctl status chv-controlplane

# Verify API health
curl -s http://localhost:8080/health | jq
```

### 2f. Re-enroll Agents

Agents will attempt to reconnect but may fail if the control plane's gRPC listener address changed.

```bash
# On each hypervisor node:
sudo systemctl restart chv-agent

# Check agent logs for connection success
sudo journalctl -u chv-agent -n 100 | grep -i "connected\|certificate\|error"
```

If agents cannot connect due to certificate issues:

```bash
# Generate new bootstrap token on control plane
TOKEN=$(openssl rand -hex 32)
echo "$TOKEN" | sudo tee /etc/chv/bootstrap.token.new
sudo chmod 600 /etc/chv/bootstrap.token.new

# On the agent node, re-enroll
sudo chvctl agent enroll --token "$TOKEN" --server https://new-cp.example.com:443
```

### 2g. Verify VM States

```bash
# List all VMs — they should show their last known state
chvctl vm list

# VMs that were Running should still be running on their nodes
# The control plane will reconcile state on next agent heartbeat
```

---

## 3. Scenario B: Database Corruption Recovery

### 3a. Stop Control Plane

```bash
sudo systemctl stop chv-controlplane
```

### 3b. Attempt SQLite Repair

```bash
# Backup the corrupted DB first
sudo cp /var/lib/chv/controlplane.db /var/lib/chv/controlplane.db.corrupt.$(date +%s)

# Try SQLite's built-in repair
sqlite3 /var/lib/chv/controlplane.db ".recover" | sqlite3 /var/lib/chv/controlplane.db.recovered

# If recovered file looks valid, replace
sudo mv /var/lib/chv/controlplane.db.recovered /var/lib/chv/controlplane.db
sudo chown chv:chv /var/lib/chv/controlplane.db
```

### 3c. If Repair Fails, Restore from Backup

Follow steps 2b (restore DB) and 2e (start control plane) from Scenario A.

---

## 4. Scenario C: Certificate Rotation

### 4a. Generate New CA and Certificates

```bash
# Use CHV's built-in cert generation (if available)
# Or manually with openssl

# Generate new CA
sudo openssl req -x509 -newkey rsa:4096 -keyout /etc/chv/certs/ca.key \
  -out /etc/chv/certs/ca.crt -days 3650 -nodes \
  -subj "/CN=chv-ca/O=CloudHypervisor"

# Generate new control plane cert
sudo openssl req -newkey rsa:4096 -keyout /etc/chv/certs/controlplane.key \
  -out /etc/chv/certs/controlplane.csr -nodes \
  -subj "/CN=new-cp.example.com/O=CloudHypervisor"
sudo openssl x509 -req -in /etc/chv/certs/controlplane.csr \
  -CA /etc/chv/certs/ca.crt -CAkey /etc/chv/certs/ca.key \
  -CAcreateserial -out /etc/chv/certs/controlplane.crt -days 365

# Generate new agent cert (per agent node)
# Agents will re-generate their own certs on re-enrollment
```

### 4b. Distribute New CA

```bash
# Copy the new CA to all agent nodes
for host in agent-1 agent-2 agent-3; do
  scp /etc/chv/certs/ca.crt "$host:/tmp/chv-ca.crt"
  ssh "$host" "sudo cp /tmp/chv-ca.crt /etc/chv/certs/ca.crt && sudo systemctl restart chv-agent"
done
```

### 4c. Re-enroll All Agents

Follow step 2f from Scenario A for each agent.

---

## 5. Post-Recovery Validation Checklist

```bash
# 1. API health
curl -s http://localhost:8080/health | jq

# 2. All agents connected
chvctl node list | jq '.nodes[] | {node_id, status, last_seen_at}'

# 3. All VMs visible
chvctl vm list | wc -l

# 4. Backup schedules still configured
#    (the `chvctl backup` group was removed — #372; use the API)
curl -s http://localhost:8080/v1/backups/schedules -H "Authorization: Bearer $TOKEN" | jq '.items[] | {schedule_id, vm_id, enabled}'

# 5. Test a non-destructive operation
chvctl vm get <TEST_VM_ID>

# 6. Verify backup worker is running
sudo systemctl status chv-controlplane | grep -i backup

# 7. Check for any failed operations after recovery
curl -s "http://localhost:8080/v1/operations?status=Failed" \
  -H "Authorization: Bearer $TOKEN" | jq '.operations | length'
```

---

## Troubleshooting

| Symptom | Diagnostic | Resolution |
|---------|-----------|------------|
| Control plane won't start after DB restore | Migration version mismatch | `rm /var/lib/chv/controlplane.db` and re-initialize (data loss) or apply pending migrations manually |
| Agents show `certificate verify failed` | CA mismatch | Re-distribute CA cert and restart agents |
| VMs show `Unknown` state | Agent hasn't heartbeated yet | Wait 30s; check agent connectivity |
| Backup schedules missing | Restored from old DB | Recreate schedules via API or UI |
| `chvctl` returns `connection refused` | Control plane not listening | Check `controlplane.toml` bind addresses; check firewall |
| S3 backups fail with auth errors after a DB restore | Restored credentials don't match the encryption key: the DB was encrypted with a different `CHV_ENCRYPTION_KEY`, or on the old host with the legacy `CHV_JWT_SECRET` fallback (check the old host's startup log for the fallback warning) | Restore the matching `/etc/chv/encryption.env` and restart `chv-controlplane`; if the original key is unrecoverable, re-enter the S3 credentials in the backup schedule settings (fail-closed `AuthFailed`, never silently corrupted) |
| SQLite `database is locked` | Another process holds the lock | `fuser /var/lib/chv/controlplane.db`; kill stale processes |

## What Happens Under the Hood

1. CHV control plane is stateful — the SQLite DB is the source of truth for all metadata
2. Agents maintain local state (VM runtimes) and reconcile with the control plane on heartbeat
3. Certificates are pinned — rotating the CA requires re-enrollment of all agents
4. The backup worker is an in-process task — it resumes automatically when the control plane starts

## Related Runbooks

- [Full Site Recovery](full-site-recovery.md) — When hypervisors are also lost
- [Backup Artifact Restore](backup-artifact-restore.md) — For recovering individual VMs from shipped backups
