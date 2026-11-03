# Agent Authentication and Per-Purpose Secret Rotation (#662)

This runbook covers the control-plane → agent credential and the split of the
former shared `jwt_secret` into independent keys.

## Trust model

The shipped single-host control-plane → agent hop is gRPC over a local
Unix-domain socket (`0600`). It is **not** mTLS. Two checks protect it:

1. **Unix peer credentials.** The agent reads the connecting UID with
   `SO_PEERCRED` and drops the connection before HTTP/2 starts when the UID
   is not allowed. `[security] agent_auth_allowed_peer_uids` defaults to empty,
   which means "the agent's own effective UID" — normally the same service
   user the control plane runs as on that host.
2. **Per-request bearer token.** The control plane attaches
   `x-chv-agent-token` to every RPC. The agent compares it in constant time
   before any handler runs and answers `UNAUTHENTICATED` for missing, wrong,
   or unconfigured tokens.

Remote (multi-host) mTLS dispatch is design-only in ADR-029; there is no TCP
agent listener. See `docs/specs/adr/002-control-plane-boundary.md`.

`[security] allow_insecure_agent_auth = true` disables both checks. It is
accepted only when `[security] production = false`, requires a `dev`-feature
agent build (`cargo build --features dev`), is logged loudly, and is for local
development only. A non-dev agent build refuses to start with it enabled.

## Configuration keys

Both `controlplane.toml` and `agent.toml` accept:

```toml
[secrets]
user_jwt = "..."                 # control plane only
console_jwt = "..."              # must match on both sides
agent_auth_token = "..."         # must match on both sides (per node in a fleet)
credential_encryption = "..."    # control plane only

[security]
production = true                # default
allow_insecure_agent_auth = false
agent_auth_allowed_peer_uids = [] # agent only; empty = own euid
```

Every explicit value must be at least 32 characters, must not be the shipped
development placeholder, and must differ from every other purpose. When a key
is absent it is derived from the legacy `jwt_secret` with HMAC-SHA256 domain
separation. That keeps an existing single-host install working, but the
derived keys all rotate with the master. Use explicit values for independent
rotation and for a fleet.

Generate keys with:

```bash
openssl rand -hex 32
```

Do not generate a fresh value on every restart: the control plane and agent
load their keys at startup, and console/user tokens minted under a previous
key stop validating.

## Migrating from the legacy shared `jwt_secret`

No configuration change is required for a single-host install: both processes
derive the same console and agent keys from a matching `jwt_secret`. The first
upgrade to the split keys still changes the effective user and console signing
keys, so **existing user sessions and any in-flight console URLs are
invalidated once** (users log in again and re-open the console; no VM is
interrupted and desired-state dispatch continues). If preserving existing user
sessions matters, set `[secrets] user_jwt` to the old `jwt_secret` before the
upgrade; console must still use a different value because the two purposes may
not share a key.

To move to independent secrets, do it purpose by purpose during a maintenance
window.

1. **Back up both configs and the database.**
   ```bash
   sudo cp /etc/chv/controlplane.toml /etc/chv/controlplane.toml.bak
   sudo cp /etc/chv/agent.toml /etc/chv/agent.toml.bak
   ```
2. **User sessions (`user_jwt`, control plane only).** Set the new value in
   `[secrets] user_jwt`, restart the control plane, and accept that all
   existing sessions are invalidated (users log in again). Console and agent
   traffic are unaffected.
3. **Serial console (`console_jwt`, both sides).** Set the same new value in
   both files, then restart the agent and the control plane. Any open console
   URL older than 60 seconds is already expired; no VM is interrupted.
4. **Agent RPC (`agent_auth_token`, both sides).** Set the same new value in
   both files. The agent accepts exactly one token, so there is a short
   window where dispatch fails while one side is on the old value. Restart
   the agent first, then the control plane (or perform a rolling restart if
   your supervision allows it). Dispatch errors during the window are
   retried; no desired state is lost.
5. **Credential encryption (`credential_encryption`, control plane only).**
   This key has no online rotation: existing ciphertext is only readable with
   the key that wrote it. Either keep the existing `CHV_ENCRYPTION_KEY`, or
   migrate deliberately:
   - export or re-enter the affected S3/NetBox credentials after switching
     the key, or
   - follow the decrypt/re-encrypt procedure in
     [`control-plane-dr.md`](control-plane-dr.md).
   Setting `[secrets] credential_encryption` takes precedence over
   `CHV_ENCRYPTION_KEY`; do not set both to different values.
6. **Remove the legacy master only after all purposes are explicit.** Keep
   `jwt_secret` in place until then; removing it while any derived key is
   still in use re-keys that purpose on the next restart.

A fleet must set an explicit `agent_auth_token` per node. The derived default
is shared by every process that shares the legacy master, so it is only
appropriate for a single-host compatibility install.

## Verifying

- Agent startup logs the rejection reason for a bad peer or token; a
  correctly configured agent logs no authentication warnings.
- A missing/wrong credential surfaces to the control plane as a gRPC
  `UNAUTHENTICATED` status on node dispatch, not as a generic timeout.
- To prove fail-closed behavior locally, point a client at the agent socket
  without the metadata header; it must be rejected before the handler runs.

## Rollback

Restore the backed-up files and restart the affected service. Console and
user-key rollback invalidates tokens minted under the new key (users log in
again; re-open the console). Agent-token rollback requires both sides to be
restored before dispatch recovers. Credential-encryption rollback only works
if no credential was written under the temporary key; otherwise the
credentials must be re-entered under the restored key.
