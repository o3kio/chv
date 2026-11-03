# CHV Disaster Recovery Runbooks

This directory contains operational runbooks for backup, restore, and disaster recovery scenarios. Each runbook is designed to be followed under pressure — steps are numbered, commands are copy-pasteable, and prerequisites are listed upfront.

> **CHV does not back up tenant data.** There is no backup or restore engine:
> the whole backup API is hard-fenced (`501 NOT_IMPLEMENTED`) and the WebUI
> exposes no backup actions. Snapshot/restore runbooks below cover the
> agent-level snapshot surface, which is a different feature. Any
> "backup artifact" procedure applies only to artifacts your own tooling
> produced outside CHV (or artifacts from a pre-fence CHV job that still
> exist); CHV cannot create new ones. See
> [`DEPLOYMENT-ARCHITECTURE.md`](../DEPLOYMENT-ARCHITECTURE.md) §7.

## Runbook Index

| Runbook | Scenario | Automation Level | Time to Execute |
|---------|----------|------------------|-----------------|
| [VM Snapshot Restore](vm-snapshot-restore.md) | Restore a VM from a local snapshot | Fully automated via API/UI | 2–5 min |
| [Volume Snapshot Restore](volume-snapshot-restore.md) | Restore a volume from a local snapshot | Automated (local/Ceph); manual steps for LVM/iSCSI | 2–10 min |
| [Backup Artifact Restore](backup-artifact-restore.md) | Restore a VM from an externally produced S3/NFS disk artifact | **Manual** — no CHV backup engine | 10–30 min |
| [Control Plane DR](control-plane-dr.md) | Recover the control plane after a host failure | Partially automated + manual steps | 15–30 min |
| [Full Site Recovery](full-site-recovery.md) | Rebuild entire site using externally produced artifacts | Manual | 1–4 hrs |
| [Monitoring Store](monitoring-store.md) | Monitoring history degraded/corrupted/disabled (retention, disk headroom, reset) | Automatic degradation + manual recovery | 5–10 min |
| [Alerting and Notifications](alerting.md) | Alert rules, incidents, webhook/Slack delivery (configuration, secret rotation, dead-letter triage) | Automatic evaluation + manual configuration | 5–15 min |
| [VM Monitoring vsock Transport](vm-monitoring-vsock.md) | Guest monitoring over the optional vsock device (opt-in, relay diagnosis, transport switch, batch enrollment) | Automatic relay rebind + manual diagnosis | 5–15 min |
| [Monitoring Export](monitoring-export.md) | Optional remote-write export to an external backend (VictoriaMetrics/Prometheus) — configuration, degraded state, backfill bounds, readback | Automatic export + manual configuration | 5–15 min |
| [Node Certificate Compromise](node-certificate-compromise.md) | Revoke a node certificate (denylist at the gRPC mTLS layer), verify rejection, recover rotation, re-enroll a compromised node | Manual revocation + verification (refresh ≤ 5 s) | 5–20 min |
| [Agent Auth and Secret Rotation](agent-auth-and-secret-rotation.md) | Control-plane → agent authentication, per-purpose secret migration, key rotation | Manual configuration + restart | 10–20 min |

## Severity Levels

Runbooks use the following severity taxonomy:

- **SEV-1** — Complete service outage, data loss risk, or security incident. Page the on-call immediately.
- **SEV-2** — Degraded service, partial data unavailability, or single-node failure. Respond within 30 minutes.
- **SEV-3** — Non-urgent recovery, planned migration, or drill. Can be scheduled.

## Before You Begin

All runbooks assume:
- You have `ssh` access to the control-plane host.
- `chvctl` is installed and configured (`/etc/chv/chvctl.toml`).
- You have `sudo` privileges on CHV nodes.
- For S3 restores: you have `aws` CLI or `s3cmd` installed with credentials for the backup bucket.

## Quick Reference: What's Automated vs Manual

| Capability | Status | How to Invoke |
|------------|--------|---------------|
| Scheduled VM backups with S3/NFS shipping | ❌ Fenced (`501 NOT_IMPLEMENTED`) | Not available — use your own tooling |
| On-demand VM backup | ❌ Fenced (`501 NOT_IMPLEMENTED`) | Not available — use your own tooling |
| Retention enforcement for CHV backup rows | ❌ Inert (no new jobs are created) | Pre-fence rows are drained and failed, never deleted silently |
| VM snapshot restore (local) | ✅ Automated | UI: Default Cloud → Hosts → Instances → `<VM_NAME>` → Snapshots → Restore |
| Volume snapshot restore (local/Ceph) | ✅ Automated | API `POST /v1/volumes/restore-snapshot` |
| Volume snapshot restore (LVM/iSCSI) | ❌ Not implemented | Manual disk replacement required |
| Restore from an external S3/NFS artifact | ❌ No CHV worker | Follow [Backup Artifact Restore](backup-artifact-restore.md); artifacts must come from outside CHV |
| Control-plane DB restore | ⚠️ Manual | Follow [Control Plane DR](control-plane-dr.md) |

## See Also

- [`docs/OPERATIONS.md`](../OPERATIONS.md) — Day-2 operations, monitoring, troubleshooting
- [Backup and restore operations](../OPERATIONS.md#backup-and-restore) — Backup schedules, DB backups, and restore behavior in `docs/OPERATIONS.md`
