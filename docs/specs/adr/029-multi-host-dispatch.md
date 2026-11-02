# ADR-029 — Multi-Host Dispatch: Opt-In Agent gRPC over mTLS

## Status

Proposed — not accepted. On acceptance this ADR:

- implements the external transport already required by
  [ADR-002](002-control-plane-boundary.md) ("gRPC over mTLS") for the
  control-plane → agent hop;
- supersedes the declared single-host posture recorded in
  [`../../DEPLOYMENT-ARCHITECTURE.md`](../../DEPLOYMENT-ARCHITECTURE.md) §8
  decision D1 (option (b), 2026-10-30);
- does **not** supersede ADR-021 (tenant fabric) or ADR-028 (storage
  authority).

Until accepted, the shipped posture remains single-host and every statement
below is a proposal.

## Date

2026-11-02

## Context

### 1. The documented boundary and the implemented boundary disagree

ADR-002 states that the control plane communicates with `chv-agent` over
"gRPC over mTLS" with certificate-based steady-state trust. The
implementation satisfies neither half for a remote host:

- the control plane resolves `agent_socket_pattern` to a **filesystem path**
  and connects with `UnixStream::connect`
  (`crates/chv-controlplane-service/src/node_client.rs:204-211`);
- the agent serves gRPC only from a `UnixListener`
  (`crates/chv-agent-core/src/agent_server.rs:181-188`);
- the address is *derived* from the node id, never learned
  (`crates/chv-controlplane-service/src/migration.rs:1654`);
- the resolution is used by dispatch surfaces across `orchestrator.rs`,
  `migration.rs`, `migration_reaper.rs`, `overlay.rs`, and `lifecycle.rs`.

Consequence: a control plane cannot reach an agent on another host, by
construction.

### 2. The surrounding pieces already assume a fleet

The enrollment protocol is transport-neutral (bootstrap token, certificate
issuance, `CN = <node_id>`, serial recording). `NodeInventory` already
carries `vtep_ip`, `wireguard_public_key`, `underlay_mtu`, and
`authority_mode`. `nodes.agent_ws_address` exists and the BFF already returns
a node-addressed `/ws/vms/{node_id}/…` console URL (the nginx example proxies
it). `peer_identity.rs` already extracts a node identity
from a peer certificate and rejects an asserted `node_id` that does not match
it. What is missing is the transport, not the model.

### 3. Two exposures are unresolved today

- The control-plane gRPC listener is contractually loopback-only
  (`DEPLOYMENT-ARCHITECTURE.md` §3.1) and its install-time certificate has
  SANs `localhost`/`127.0.0.1` only (`scripts/install.sh`). A remote agent
  cannot enroll against it even if a listener were opened.
- Every CHV service shares one symmetric `jwt_secret`
  (`crates/chv-config/src/lib.rs` `resolve_jwt_secret`; `scripts/install.sh`
  writes the same value into the control-plane and agent configs). In a
  fleet, any node can mint a console token accepted by any other node.

### 4. Why now

Volvisor integration (ADR-028) is per-node and therefore compatible with a
single host, but the rest of the product direction — fleet networks
(ADR-021), per-node storage placement, node drain — assumes more than one
node. Continuing to expose only a local socket forces every future feature to
be designed twice.

### 5. Constraints

- Single-host installs MUST keep working with no new listener, no new
  certificate, and no new failure mode.
- The control plane remains a single authority (ADR-011). This ADR does not
  introduce control-plane HA.
- Cloud Hypervisor remains reachable only through `chv-agent` (ADR-002).
- Tenant L2 across hosts remains ADR-021's problem; this ADR only makes
  remote dispatch possible.
- No component may be publicly exposed. The nginx edge remains the only
  externally listening surface.

## Decision

### 1. Transport

`chv-agent` MAY serve its gRPC surface over TCP with **mandatory mutual TLS**.
The Unix socket remains the default and remains supported indefinitely for
same-host deployments.

Both listeners may run concurrently. Enabling TCP MUST NOT disable or alter
the Unix path.

Plaintext gRPC, `client_auth_optional`, and TLS without a client CA are
prohibited. A server that cannot build its TLS configuration MUST refuse to
start; there is no "continue insecurely" mode (`CHV_ALLOW_INSECURE` remains a
dev-only compile-time feature and MUST be refused when
`deployment.topology = multi-host`).

### 2. Listener exposure

| Listener | Default | Multi-host | Rule |
|---|---|---|---|
| Control-plane gRPC | `127.0.0.1:8443` | operator-declared management `bind` | mTLS, node CA as client CA |
| Control-plane HTTP/BFF | `127.0.0.1:8080` | unchanged | never exposed directly |
| Agent gRPC | absent | operator-declared `bind` + `advertise_addr` | mTLS, control-plane client CA only |
| Agent console | `127.0.0.1:8444` | management address, TLS | token-gated; never public |
| Agent metrics | loopback | unchanged | outbound reporting is the primary path |

Rules:

1. TCP listeners are enabled only by explicit configuration. Absence of a
   block is not an error.
2. `0.0.0.0`/`::` binds are refused unless `allow_wildcard_bind = true`
   **and** `advertise_addr` is set.
3. A bind failure is fatal. No random-port fallback.
4. Actual bound address, protocol, and TLS posture are logged at startup and
   reported in inventory.
5. `deployment.topology = multi-host` is a declaration, not a switch: it
   MUST fail startup if a required listener/certificate/address is missing.

Default agent gRPC port: **9443/tcp**. It is a new entry in the §3.1 contract
and MUST NOT collide with the console (`8444`), CP gRPC (`8443`), stord
migration receiver, or fabric ports.

### 3. Identity model

Node identity splits into two purposes with two key pairs:

| Identity | Issued by | EKU | Used for |
|---|---|---|---|
| `node-client` | control-plane node CA | `clientAuth` | agent → CP enrollment, reports, acks |
| `node-server` | control-plane node CA | `serverAuth` | CP → agent dispatch |

Both carry the same `node_id`. A stolen client key cannot impersonate a node's
server, and vice versa.

The existing `/etc/chv/certs/agent-client.{crt,key}` (`CN=chv-agent`, issued
by the installer) is the enrollment bootstrap credential, not the
steady-state `node-client` identity. The two MUST NOT be conflated; the
bootstrap certificate is retired once enrollment completes.

The control plane has its own client identity for dispatch:

| Identity | Issued by | EKU | Used for |
|---|---|---|---|
| `chv-controlplane-client` | **dedicated control-plane client CA** (sub-CA or separate root) | `clientAuth` | CP → agent dispatch |

The agent MUST trust only the control-plane client CA for its TCP listener.
Node certificates are signed by the node CA and therefore structurally cannot
authenticate to a peer agent. A CN allowlist against the shared node CA is
explicitly rejected as the boundary: one configuration mistake turns every
node into a control plane.

Certificate content rules:

1. The first DNS SAN of a node certificate MUST be exactly `node_id` (the
   value used as the TLS server name and by
   `peer_identity::parse_node_id_from_der`, which takes the first DNS SAN).
   Additional SANs MUST be appended after it or expressed as IP SANs.
2. Server certificates MUST include the node's `advertise_addr` as an IP SAN
   (and the management DNS name if one is used). A missing advertised
   identity is a startup error.
3. The control-plane server certificate MUST include the address agents dial
   (`control_plane_addr` host) as a DNS or IP SAN. The current
   `localhost`/`127.0.0.1`-only certificate is insufficient for multi-host.
4. EKUs are explicit. `keyUsage` is `digitalSignature` (+ `keyEncipherment`
   for RSA).
5. Serial numbers are recorded in `nodes.certificate_serial` (already present)
   and checked against a denylist on every new connection (the denylist is
   new work in P1; it does not exist today). There is no CRL/OCSP
   infrastructure in v1; short lifetimes plus the denylist are the
   revocation story.

The CP MUST verify that the agent's server certificate chains to the node CA,
carries `serverAuth`, and has `CN == expected node_id` for the endpoint being
dialled. Certificate validity alone is not authorization: without the CN
check, any node can answer for any other node.

The agent MUST additionally verify that the presented client certificate has
`CN == "chv-controlplane-client"` (or the configured
`allowed_client_identity`) in addition to chaining to the CP client CA.

### 4. Endpoint authority

A node's endpoint is learned, not derived:

```text
agent_grpc_address = advertise_addr ":" agent_grpc_port
agent_grpc_server_name = node_id
```

- `advertise_addr` is an explicit operator value or is derived from a named
  interface. It MUST be a unicast address that is neither loopback,
  unspecified, link-local, nor multicast.
- Auto-detection with multiple candidates is refused, not resolved by
  "first non-loopback".
- The agent reports `advertise_addr`, `agent_grpc_port`, and its transport in
  enrollment and in every inventory cycle (additive proto fields).
- The CP persists the values and `agent_endpoint_generation`.
  Re-enrollment, address change, or certificate rotation increments the
  generation and evicts the pooled client.
- The CP MUST NOT derive an endpoint from `node_id`, a hostname convention,
  or the pattern that produced Unix socket paths.

### 5. Dispatch

A single resolver owns endpoint selection:

```text
NodeEndpoint = Unix { path } | Tcp { host, port, server_name }
```

- Every dispatch call site uses the resolver. No call site constructs a
  socket path.
- The client pool is keyed by `(node_id, endpoint_generation)`.
- Failure semantics are unchanged: the same circuit breaker, the same
  per-method keys, the same timeout-to-`BackendUnavailable` mapping.
- A TCP dial failure MUST NOT fall back to a Unix path. Fallback would hide a
  misconfigured node behind a successful local operation.

### 6. Console

Console is not multiplexed onto the gRPC listener.

- On multi-host, the agent console binds the management address with TLS and
  the BFF dials `agent_ws_address`.
- The console token MUST be CP-signed, short-lived, and audience-scoped to
  `node:<node_id>`. The shared symmetric `jwt_secret` MUST NOT be the
  cross-host trust root.
- The nginx edge remains the only browser-facing surface.

### 7. Deployment declaration

`deployment.topology` is required in multi-host installs:

| Value | Required listeners | Refused |
|---|---|---|
| `single-host` | CP gRPC loopback | remote agent TCP |
| `multi-host` | CP gRPC on management bind; per-node agent TCP | loopback-only CP gRPC; wildcard bind without advertise; `CHV_ALLOW_INSECURE`; legacy EKU mode |

Startup validation compares the declaration against the actual configuration
and exits non-zero on mismatch. The declaration is also written into
inventory so the control plane can refuse to place workloads on a node that
claims a topology it does not implement.

## Invariants

1. **I1** — No plaintext gRPC and no optional client authentication on any
   listener.
2. **I2** — An agent accepts control-plane clients only from the dedicated
   CP client CA; node certificates are rejected for that role.
3. **I3** — The control plane accepts an agent server certificate only when
   its CN equals the expected `node_id`.
4. **I4** — A node's endpoint comes from persisted advertised state and is
   never derived from `node_id`.
5. **I5** — A dispatch failure never falls back to a local Unix socket.
6. **I6** — Single-host configuration is behaviour-identical to the current
   release: same listeners, same certificates, same defaults, no new
   listener opened.
7. **I7** — Certificate or address rotation invalidates pooled clients before
   the next dispatch.
8. **I8** — `deployment.topology = multi-host` refuses to start when any
   required listener, certificate, or advertised address is missing.
9. **I9** — Console tokens are audience-scoped; no cross-host symmetric
   secret is accepted.
10. **I10** — Every negative identity case has a test that fails closed
    (peer agent, wrong CN, expired cert, revoked serial, wrong CA).
11. **I11** — The §3.1 listener table in `DEPLOYMENT-ARCHITECTURE.md` is
    updated in the same change that adds a listener.
12. **I12** — ADR-002's rule that only `chv-agent` reaches Cloud Hypervisor
    is unchanged; the new listener adds no VMM surface.

## Consequences

### Pros

- Implements the transport ADR-002 already requires.
- Unblocks fleet placement, node drain, remote console, and per-node storage
  without redesigning the operation model.
- Keeps the single-host path untouched and reversible.
- Separates node and control-plane identities before there is a fleet to
  retrofit.

### Cons

- Adds an exposed listener and a new certificate lifecycle.
- Requires an operator-managed management network (or an explicit decision to
  share one).
- Adds endpoint state to the node model and a generation to invalidate it.
- Mixed-version fleets need a compatibility window during rollout.

### Risks and mitigations

| Risk | Mitigation |
|---|---|
| A node certificate is accepted as a control-plane client | Dedicated CP client CA; explicit negative test in the gate |
| One call site keeps using Unix paths, silently limiting the fleet | Single resolver; CI check forbidding `UnixStream::connect` outside the transport module |
| Certificate SAN changes break `peer_identity` | Keep `node_id` as DNS SAN index 0; test the parser against new certificates |
| Operators expose the agent listener to the internet | Wildcard guard, documented exposure rule, startup refusal without `advertise_addr` |
| Console secret reuse across the fleet | Asymmetric, audience-scoped tokens; no shared `jwt_secret` path for console |

## Rejected alternatives

- **SSH tunnelling.** No application identity, no per-node authorization, no
  backpressure model, and a second credential system.
- **A sidecar per node.** Reintroduces the same transport problem with an
  extra process and no reduction in attack surface.
- **gRPC only over WireGuard.** Couples dispatch to the optional fabric
  (ADR-021); a node without the fabric would be unmanageable.
- **CN allowlist on the shared node CA.** One mistake turns every node into a
  control plane.
- **Deriving endpoints from `node_id`.** Works only for a hostname convention
  the operator cannot rotate.

## Qualification gates

This ADR is accepted only with the P1 gate evidence in
[`../../plans/2026-11-02-multihost-and-volvisor-transition.md`](../../plans/2026-11-02-multihost-and-volvisor-transition.md):

1. Two separate hosts: enrollment, inventory, dispatch, and console.
2. Node A certificate rejected by node B's agent listener.
3. `CN=node-a` server certificate rejected when dialling `node-b`.
4. Plaintext, optional client auth, wildcard-without-advertise, and
   loopback-only CP gRPC under `multi-host` all refuse startup.
5. Endpoint generation change evicts the pooled client.
6. Single-host regression suite is unchanged.
7. Revoked serial cannot reconnect.

No `MULTI-HOST-VERIFIED` claim may be made from container legs or from
multiple processes on one kernel. The gate requires separate kernels or
separate machines.

## Related

- [ADR-002 — Control Plane to Node Boundary](002-control-plane-boundary.md)
- [ADR-011 — Single-Node Control Plane](011-single-node-controlplane.md)
- [ADR-021 — Stretched-L2 Fabric](021-stretched-l2-vxlan-her-wireguard-fabric.md)
- [ADR-028 — Volvisor Storage Orchestration](028-volvisor-storage-orchestration.md)
- [`../../DEPLOYMENT-ARCHITECTURE.md`](../../DEPLOYMENT-ARCHITECTURE.md) §3, §5 UC-6, §8 D1/D2
- [`../../plans/2026-11-02-multihost-and-volvisor-transition.md`](../../plans/2026-11-02-multihost-and-volvisor-transition.md)
- `crates/chv-controlplane-service/src/peer_identity.rs`
- `crates/chv-controlplane-service/src/node_client.rs`
- `crates/chv-agent-core/src/agent_server.rs`
- `crates/chv-controlplane-service/src/enrollment.rs`
