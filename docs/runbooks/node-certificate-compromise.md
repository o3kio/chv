# Node Certificate Compromise and Revocation

**Severity:** SEV-1 (suspected key compromise) or SEV-2 (planned credential retirement)
**Scope:** control-plane gRPC mTLS node certificates (`/etc/chv/certs/` on a node)
**Applies to:** single-host and multi-node deployments running the control plane
with TLS/mTLS enabled

This runbook covers:

1. [Revoking a node certificate](#1-revoke-the-certificate)
2. [Verifying the revocation took effect](#2-verify-revocation)
3. [Recovering a healthy node after rotation](#3-rotation-recovery-healthy-node)
4. [Re-enrolling a compromised or revoked node](#4-re-enroll-a-compromised-node)
5. [What revocation does and does not do](#5-what-revocation-does-and-does-not-do)
6. [Rollback and failure modes](#6-rollback-and-failure-modes)

---

## Background

CHV v1 has no CRL or OCSP responder. Node certificates are short-lived and the
control plane keeps a durable **denylist keyed by certificate serial**
(`node_certificate_revocations`). Every production mTLS request is checked
against an in-memory snapshot of that table by the gRPC peer-identity
interceptor before the handler runs.

Revocation is therefore two independent guarantees:

- **Authentication:** the revoked leaf certificate is rejected with
  `UNAUTHENTICATED` on every node-facing gRPC method.
- **Rotation:** the store's compare-and-swap refuses to exchange the revoked
  serial for a new certificate, even if the stored `nodes.certificate_serial`
  still equals it. A thief cannot rotate the stolen credential into a fresh
  one.

The snapshot refreshes every **5 seconds** by default
(`CHV_REVOCATION_REFRESH_SECS`, 1–300). Revocation of *rotation* is immediate
(database-enforced); authentication changes take at most one refresh interval.

> `CHV_ALLOW_INSECURE=1` (dev-only builds with the `dev` Cargo feature)
> bypasses mTLS peer identity **and revocation entirely**. Never use it in
> production. Production builds reject the flag at startup.

---

## 1. Revoke the certificate

Run on the control-plane host as an admin. `chvctl` must be logged in with an
admin token.

```bash
# Revoke the node's currently recorded serial.
chvctl node revoke-certificate <NODE_ID> \
  --reason "suspected host compromise: incident INC-1234"

# Revoke a specific (e.g. previous/stale) serial explicitly.
chvctl node revoke-certificate <NODE_ID> \
  --certificate-serial <SERIAL> \
  --reason "stolen backup of agent-client.crt"
```

Equivalent API call:

```bash
curl -sS -X POST http://127.0.0.1:8080/v1/nodes/revoke-certificate \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "node_id": "<NODE_ID>",
    "certificate_serial": "<SERIAL>",
    "reason": "suspected host compromise: incident INC-1234"
  }'
```

The endpoint is **admin-only**, **idempotent**, and records an `Audit` event
with the serial, reason, actor, and timestamp. A repeat call returns
`"already_revoked": true` and keeps the original reason/time/actor.

Get the active serial if you do not know it:

```bash
sqlite3 "file:/var/lib/chv/controlplane.db?mode=ro" \
  "SELECT node_id, certificate_serial FROM nodes WHERE node_id = '<NODE_ID>';"
```

> **Do not write to the live database.** Use the CLI/API. Manual writes require
> stopping the control plane first (see `docs/OPERATIONS.md` → *Live Database
> Access*).

---

## 2. Verify revocation

### 2.1 Confirm the row exists

```bash
sqlite3 "file:/var/lib/chv/controlplane.db?mode=ro" \
  "SELECT certificate_serial, node_id, reason, revoked_at, revoked_by
     FROM node_certificate_revocations
    WHERE certificate_serial = '<SERIAL>';"
```

### 2.2 Confirm the peer is rejected

From a host that still has the revoked certificate (or a copy of it), call any
node-facing RPC:

```bash
grpcurl \
  -cacert /etc/chv/certs/ca.crt \
  -cert /etc/chv/certs/agent-client.crt \
  -key /etc/chv/certs/agent-client.key \
  <CONTROL_PLANE_HOST>:8443 \
  chv.controlplane.node.v1.EnrollmentService/RotateNodeCertificate \
  -d '{"node_id":"<NODE_ID>","meta":{"operation_id":"verify-revocation"}}'
```

Expected:

```text
ERROR:
  Code: Unauthenticated
  Message: peer certificate is revoked
```

A still-valid certificate for another node must continue to work — revocation is
per-serial, not fleet-wide.

### 2.3 Observe telemetry

- Metric `chv_revoked_peer_rejected_total` increments.
- Metric `chv_node_certificate_revocations_active` equals the number of rows.
- Metric `chv_revocation_snapshot_reload_total{result="ok"}` increments every
  refresh interval. `result="error"` means the last reload failed and the
  previous snapshot is still enforced — investigate the store immediately.
- Control-plane logs contain
  `rejecting request: peer certificate serial is revoked` with the node id and
  serial.

### 2.4 Confirm rotation is burned

```bash
grpcurl ... EnrollmentService/RotateNodeCertificate \
  -d '{"node_id":"<NODE_ID>","meta":{"operation_id":"verify-rotation-burn"}}'
```

Expected: `PermissionDenied: the presented certificate has been revoked;
re-enroll the node with a fresh bootstrap token`.

---

## 3. Rotation recovery (healthy node)

Use this path when a node is healthy but its credential is due for normal
rotation (the agent rotates automatically every 12 hours) or an operator wants a
fresh credential **without** suspecting compromise.

- **Normal rotation requires no operator action.** `chv-agent` checks the
  certificate age and calls `RotateNodeCertificate` when due. The control plane
  requires proof of possession: the presented mTLS leaf serial must equal the
  node's recorded `nodes.certificate_serial`. A stale or foreign certificate is
  rejected with `PermissionDenied` and no key material is minted.
- **If rotation was interrupted** (agent restarted mid-flight, or the control
  plane lost the response), the agent retries on the next cycle. The serial in
  `nodes.certificate_serial` is authoritative. If the agent repeatedly fails
  with `presented certificate serial does not match`, the local cache and the
  control plane disagree:
  1. Check `nodes.certificate_serial` and the serial in the node's
     `/etc/chv/certs/agent-client.crt` (`openssl x509 -noout -serial -in ...`).
  2. If the node holds the current serial but the stored one is stale, the
     previous rotation committed late — wait one agent cycle and retry.
  3. If the node cannot present the recorded serial, it must re-enroll
     ([step 4](#4-re-enroll-a-compromised-node)). Do **not** delete or edit the
     serial in the database to force a rotation.
- **A revoked serial never recovers by rotation.** Revocation deliberately burns
  that serial. Re-enrollment is the only path back to service.

---

## 4. Re-enroll a compromised node

Do this after containment (host rebuilt/restored from trusted media, malware
removed, credentials rotated). The revoked certificate cannot authenticate, so
enrollment uses the bootstrap-token path, which is independent of mTLS.

1. **Keep the node isolated** until you have rebuilt/verified it. Revocation
   blocks control-plane access; it does not stop the host from running local
   workloads or reaching the network.

2. **Mint a one-time bootstrap token** on the control-plane host:

   ```bash
   TOKEN=$(openssl rand -hex 32)
   curl -sS -X POST http://127.0.0.1:8080/internal/bootstrap-token \
     -H "Content-Type: application/json" \
     -d "{\"token\":\"$TOKEN\",\"description\":\"re-enroll <NODE_ID> after INC-1234\",\"one_time_use\":true}"
   ```

3. **Install the token on the node** and clear stale enrollment state:

   ```bash
   sudo install -m 640 -o root -g chv /dev/stdin /etc/chv/bootstrap.token <<< "$TOKEN"
   sudo rm -f /var/lib/chv/cache/agent-cache.json
   sudo systemctl restart chv-agent
   ```

   The agent consumes the token, receives a **new certificate with a new
   serial**, and records it. The old serial stays in the denylist; no operator
   action removes it.

4. **Verify the new identity**:

   ```bash
   # New serial is recorded:
   sqlite3 "file:/var/lib/chv/controlplane.db?mode=ro" \
     "SELECT node_id, certificate_serial FROM nodes WHERE node_id = '<NODE_ID>';"

   # New serial is NOT in the denylist:
   sqlite3 "file:/var/lib/chv/controlplane.db?mode=ro" \
     "SELECT COUNT(*) FROM node_certificate_revocations
       WHERE certificate_serial = '<NEW_SERIAL>';"
   ```

5. **Confirm normal reporting resumes** (`chv-agent` logs show successful
   inventory/telemetry; the node is `Unknown`→`Ready` in the UI).

6. **Rotate any other secrets** that lived on the compromised host: WireGuard
   fabric private keys, storage credentials, SSH host/user keys, API tokens.
   Revoking the node certificate does not rotate those.

---

## 5. What revocation does and does not do

**Does:**

- Rejects the leaf certificate at the gRPC peer-identity interceptor with
  `UNAUTHENTICATED` on all production mTLS node RPCs (enrollment rotation,
  inventory, telemetry, reconcile, lifecycle, monitoring ingest).
- Prevents that serial from being rotated into a new certificate.
- Persists across control-plane restarts (table-backed, loaded before the gRPC
  listener starts).
- Fail-closed: if the denylist cannot be loaded at startup in production mode,
  the control plane **refuses to start**. A failed refresh keeps the
  last-known-good snapshot; it never clears it.
- Audited (durable `events` row + `audit` tracing target) and idempotent.

**Does not:**

- Stop the host from running, reaching the network, or accessing data it already
  has. Containment is a host/network action.
- Revoke the WireGuard fabric key, SSH keys, or application credentials.
- Withdraw a certificate from a third-party system that trusts the same CA.
- Purge the denylist row. `expires_at` is optional pruning metadata only — a row
  whose `expires_at` has passed still denies the serial.
- Protect a deployment running `CHV_ALLOW_INSECURE=1` (dev only).

---

## 6. Rollback and failure modes

**There is no "un-revoke" operation.** Revocation is deliberately one-way; a
false positive is recovered by re-enrolling the node (step 4), not by deleting
the row. If you believe a row is wrong, capture evidence first, then re-enroll.

| Symptom | Meaning | Action |
|---|---|---|
| All node RPCs fail after upgrade | Control plane refused to start | Check logs for `cannot load node certificate revocation snapshot`. Fix store/DB; do not start with unknown state. |
| `chv_revocation_snapshot_reload_total{result="error"}` | Store hiccup during refresh | Last-known-good snapshot still enforced. Restore store health; revocations already loaded stay active. New revocations may be delayed. |
| Node rejected immediately after revoke | Expected | Re-enroll (step 4). |
| Node rejected but no denylist row | Another cause (expired cert, CN/SAN mismatch, missing serial) | Check control-plane logs for `peer mTLS certificate required` vs `peer certificate is revoked`. |
| Rotation returns `presented certificate serial does not match` | Agent holds a stale/foreign certificate | See step 3; re-enroll if it cannot present the recorded serial. |
| Rotation returns `the presented certificate has been revoked` | Expected for a burned serial | Re-enroll. |

### Migration / rollback of the feature itself

- Migration `0064_node_certificate_revocations.sql` adds a new table and two
  indexes; it does not alter existing tables. Rollback of the binary leaves the
  table in place (inert). Do not drop it while an incident is live.
- Refresh interval: set `CHV_REVOCATION_REFRESH_SECS` (1–300) in the control
  plane's environment file and restart. Lowering it shortens the revocation
  propagation window; it does not affect rotation, which is database-enforced.
- Dev/test: `CHV_ALLOW_INSECURE=1` with a `dev`-feature build bypasses
  revocation. Production builds reject that configuration at startup.

---

## See also

- [`docs/OPERATIONS.md`](../OPERATIONS.md) — live database access, certificate
  backup, enrollment troubleshooting
- [`docs/specs/adr/029-multi-host-dispatch.md`](../specs/adr/029-multi-host-dispatch.md)
  — node identity, certificate requirements, and the v1 revocation story
