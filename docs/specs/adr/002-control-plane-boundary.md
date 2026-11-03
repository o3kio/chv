# ADR-002 — Control Plane to Node Boundary

## Status
Accepted

## Date
2026-04-13

## Context
The control plane must remain authoritative for long-lived desired state, while the node must preserve runtime state during outages and reboots. Cloud Hypervisor should not be remotely reachable.

## Decision
The control plane communicates only with `chv-agent`.

### External transport

The shipped single-host topology and the design-only multi-host topology use
different transports. ADR-029 tracks the transition; until its P1 gate passes,
the implementation is the single-host row below.

- **Single host (shipped):** gRPC over a local Unix-domain socket
  (`0600`) at the agent's configured `socket_path`.
  - Every control-plane → agent RPC carries a per-daemon bearer token in the
    `x-chv-agent-token` gRPC metadata key. The agent compares it in constant
    time before any handler runs and rejects missing, wrong, or unconfigured
    tokens with `UNAUTHENTICATED` (fail-closed).
  - The agent additionally verifies the peer UID of each accepted Unix
    connection via `SO_PEERCRED`. An empty
    `[security] agent_auth_allowed_peer_uids` means "the agent's own
    effective UID", which is the normal control-plane/agent service-user
    pairing on one host.
  - `[security] allow_insecure_agent_auth = true` is an explicit local-dev
    bypass. It is refused when `security.production = true` (the default) and
    requires a `dev`-feature agent build.
- **Multi-host (design only, ADR-029):** gRPC over mTLS with
  certificate-based steady-state trust. Not implemented.
- Bootstrap token for initial enrollment.
- Certificate-based steady-state trust after enrollment.

### Secret separation (#662)

The single shared `jwt_secret` is no longer a credential for every purpose.
It remains the legacy migration master; the following keys are independently
configurable under `[secrets]`:

| Key | Purpose | Notes |
|-----|---------|-------|
| `user_jwt` | End-user session JWTs (BFF login) | Rejected for console/agent use |
| `console_jwt` | Short-lived serial-console JWTs | Must match on control plane and agent; tokens carry `aud = "chv:console"` |
| `agent_auth_token` | Control-plane → agent bearer token | Per-daemon credential; set a distinct value per node in a fleet |
| `credential_encryption` | Backup S3 / NetBox credential encryption at rest | Takes precedence over `CHV_ENCRYPTION_KEY` |

When a key is absent, it is derived from the legacy `jwt_secret` with
HMAC-SHA256 domain separation and a versioned label. This keeps an existing
single-host install operational without new configuration, but the first
upgrade invalidates existing user sessions and in-flight console URLs once,
and the derived keys rotate together with the master. Explicit values are
required for independent rotation. Validation is fail-closed: explicit values
must be at least 32 characters, must not be the development placeholder, and
must not duplicate another purpose. Rotation and migration are documented in
[`../runbooks/agent-auth-and-secret-rotation.md`](../runbooks/agent-auth-and-secret-rotation.md).

### Local VMM boundary
- only `chv-agent` may call Cloud Hypervisor
- Cloud Hypervisor is accessed only over local Unix API sockets
- Cloud Hypervisor APIs must never be exposed remotely

### Desired state authority
- control plane owns long-lived desired state
- node reports observed state
- node may preserve local runtime state during partition
- node must not invent new long-lived desired state independently

## Consequences
Pros:
- single trusted entrypoint on the node
- easier auditing and authorization
- clean operational and security boundary

Cons:
- `chv-agent` becomes operationally critical
- the local durable cache must be implemented carefully

## Rules
- all remote mutations flow through `chv-agent`
- every control-plane → agent RPC carries the per-daemon bearer token; the
  agent rejects unauthenticated requests before running a handler
- the agent socket is Unix-only, `0600`, and gated by peer UID
- user, console, agent, and credential-encryption keys are distinct and may
  not be reused for another purpose
- desired-state generations are monotonic decimal strings
- stale desired-state generations must be rejected cleanly
- non-numeric generation values must be rejected as invalid contract input
- retries should be safe and idempotent where possible

## Related ADRs
- **ADR-007** references the compatibility matrix and rollback policy during upgrades. Desired-state generation strings (defined in this ADR) remain authoritative during version skew; rollback targets must validate generation compatibility.
