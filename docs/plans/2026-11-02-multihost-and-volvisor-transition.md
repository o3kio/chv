# Multi-Host and Native Volvisor Transition Plan

Status: Proposed — requires ratification of the decisions in §4 before P1 starts
Date: 2026-11-02
Decision sources: [ADR-028](../specs/adr/028-volvisor-storage-orchestration.md) (accepted, runtime pending), [ADR-021](../specs/adr/021-stretched-l2-vxlan-her-wireguard-fabric.md) (accepted, in execution), [ADR-002](../specs/adr/002-control-plane-boundary.md) (control-plane/node boundary), [ADR-004](../specs/adr/004-storage-datapath.md) (host-side storage datapath)
Supersedes: the single-host envelope in [`../DEPLOYMENT-ARCHITECTURE.md`](../DEPLOYMENT-ARCHITECTURE.md) §3.1/§5/§8 (D1), once D-MH5 is ratified
Related: [`2026-10-13-volvisor-storage-adoption.md`](2026-10-13-volvisor-storage-adoption.md), [`FABRIC_IMPLEMENTATION_PLAN.md`](FABRIC_IMPLEMENTATION_PLAN.md), [`MULTI_NODE_IMPLEMENTATION_PLAN.md`](MULTI_NODE_IMPLEMENTATION_PLAN.md) (historical, 2026-05-07 — verify against the tree), [`../GAP_ANALYSIS.md`](../GAP_ANALYSIS.md)

---

## 1. Purpose

Two gaps separate the documented direction from the shipped product:

1. **CHV is single-host by construction, not by policy.** The documentation
   describes a control-plane-to-node boundary; the code implements it as a
   local Unix socket between two processes on one machine. A second host
   cannot be managed at all.
2. **Storage authority is `chv-stord`.** ADR-028 selects Volvisor as the
   storage lifecycle authority, but Volvisor has zero code references in
   CHV, and `chv-stord` is still the only storage effector on every path.

This plan closes both gaps in a sequence where each phase is independently
valuable, independently revertible, and gated on evidence rather than on a
green CI run. It deliberately does **not** promise "multi-host" or "Volvisor
production support" as a single flip.

### 1.1 What "done" means

There are four distinct finish lines, and conflating them is how projects
claim capabilities they do not have:

| Finish line | Meaning | Reached at |
|---|---|---|
| **MH-A: remote management** | One control plane manages N hosts: enrollment, inventory, dispatch, console, lifecycle. Tenant networking across hosts is not implied. | P2 |
| **MH-B: multi-host tenant networks** | VMs on different hosts share a literal L2 segment with real MACs (ADR-021). | P5 |
| **VS-A: native Volvisor local storage** | `native-local` volumes are created/attached/detached by Volvisor on a node, under CHV intent, with no stord writer. | P4 |
| **VS-B: stord retirement** | No shipped configuration selects stord for new volumes; remaining legacy volumes are explicitly owned and documented. | P7 |

Cross-host **live migration with local storage** is not on this plan. Volvisor's
mobility surface is wired only for its replicated (`drbd`) provider, so a
`native-local` deployment has no valid prepare/transfer path: the routes answer
404 `NOT_FOUND` because the handoff surface is absent for the LVM provider (the
typed `MIGRATION_UNSUPPORTED_LOCAL_STORAGE` code exists, but is not reached in
that configuration). CHV has no qualified shared or replicated storage class.
Migration is refused at P4.6 for `native-local` VMs and delivered only with
`ceph-rbd` or `nearline-replicated`, each of which needs its own contract and
failure campaign.

---

## 2. Reality check (evidence)

Every claim below is checkable in the tree at the time of writing.

### 2.1 Multi-host blockers

| # | Blocker | Evidence |
|---|---|---|
| B1 | The control plane reaches an agent only through a filesystem path. | `crates/chv-controlplane-service/src/node_client.rs` — `NodeClient::connect_with_breaker` builds a tonic channel over `UnixStream::connect(path)` via `tower::service_fn`; there is no TCP or TLS branch. |
| B2 | Agent address is *derived*, never *learned*. | `crates/chv-controlplane-service/src/migration.rs:1654` `resolve_agent_socket(pattern, node_id)` substitutes `{node_id}` into a local path pattern. |
| B3 | That assumption is spread across the whole dispatch surface. | Callers: `orchestrator.rs:768,1605`, `migration.rs:464,637,682,823,910,1754`, `overlay.rs:50`, `lifecycle.rs:2155`, `migration_reaper.rs:224`. |
| B4 | The agent has no gRPC TCP listener. | `crates/chv-agent-core/src/agent_server.rs` serves only `UnixListener` + `UnixListenerStream`; the only non-loopback-capable listener is the console (`:8444`) and optional metrics. |
| B5 | The control-plane gRPC listener is loopback-only by contract. | `docs/DEPLOYMENT-ARCHITECTURE.md` §3.1: CP gRPC `127.0.0.1:8443`, "Do not expose." A remote agent therefore cannot enroll or report. |
| B6 | The CP server certificate has no address a remote node could validate. | `scripts/install.sh` issues `CN=chv-controlplane` with SAN `localhost,127.0.0.1` only. |
| B7 | Node certificates are not usable as agent *server* identities for a remote control plane. | `crates/chv-controlplane-service/src/enrollment.rs` `issue_node_certificate`: CN=`<node_id>`, DNS SAN `<node_id>`, no explicit EKU, no IP SAN, no advertise address. |
| B8 | Packaging hard-couples node and control plane. | `docs/DEPLOYMENT-ARCHITECTURE.md` §8 D2: `chv-node` depends on `chv-controlplane`; a node-only host installs a control plane it does not use. `packaging/nfpm/chv-node.yaml`. |
| B9 | Placement is "the most recently enrolled node". | `crates/chv-webui-bff/src/handlers/vms.rs:357` — `SELECT node_id FROM nodes ORDER BY enrolled_at DESC LIMIT 1`. |
| B10 | Console tokens use one shared symmetric secret. | `crates/chv-config/src/lib.rs` `resolve_jwt_secret` reads `/etc/chv/jwt_secret`; `scripts/install.sh` writes the same `JWT_SECRET` into every service config. Any node could mint a console token for any other node. |
| B11 | Tenant networking across hosts is design-only and unqualified. | ADR-021 is accepted and `FABRIC_IMPLEMENTATION_PLAN.md` is "In execution". The control-plane side exists: `fabric_planner.rs` compiles per-node plans, `orchestrator.rs:735-739` has the `UpdateOverlay` fan-out arm, and `vtep_repo.allocate_vni` is called from that path. But the agent still reports `vtep_ip: ""`, the fabric datapath is disabled and fails closed in the qualified configuration (`DEPLOYMENT-ARCHITECTURE.md` §2), and there is no cross-host L2 evidence. |
| B12 | Fleet-wide networks (`networks.node_id IS NULL`) have no qualified L2 realization. | Schema permits it; the plan compiler handles it for `UpdateOverlay`, but no datapath evidence exists (B11). |

### 2.2 Volvisor blockers

| # | Blocker | Evidence |
|---|---|---|
| S1 | Volvisor is not called anywhere. | No `volvisor` reference in `crates/`, `cmd/`, `proto/`. |
| S2 | Storage dispatch is class-string-based and stord-only. | `crates/chv-hypervisor-api/src/resources.rs` `HostResourceController::open_volume(volume_id, backend_class, locator, …)`; `crates/chv-agent-core/src/resources.rs` implements it by dialing stord. |
| S3 | There are two independent storage effector paths, not one. | Core runtime: `crates/chv-agent-runtime-ch/src/core_runtime.rs:501,516,651,659` via the trait. Legacy: `crates/chv-agent-core/src/reconcile.rs` and `agent_server.rs` call `StordClient` directly. A single adapter at the trait would leave the legacy path on stord. |
| S4 | The seam lacks the tenant identity Volvisor requires. | `cellhv-core-types::StorageAttachmentRef` has `attachment_id, storage_ref, read_only, size_bytes, seed_from, backend_class`; `volvisor.volume.v2` `CreateVolumeRequest` requires `project_id`. |
| S5 | Class vocabularies are disjoint and must not be mapped. | CHV: `local`, `lvm`, `iscsi`, `ceph`. Volvisor: `native-local`, `nearline-replicated`, `ceph-rbd`. |
| S6 | Volvisor has no snapshot/clone/restore routes. | `crates/volvisor-api/src/router.rs`: create, list, inspect, attach, detach, grow, move-backing, delete, capabilities, healthz, admin, mobility. |
| S6b | Volvisor *does* have a mobility surface, but it is not enabled for `native-local`. | `POST /v2/vms/{vm_id}/check-mobility` and the `/v2/migrations/*` routes exist, but `volvisord` wires the handoff only for the `drbd` provider. With the `lvm` provider, `check-mobility` answers 404 `NOT_FOUND` ("handoff surface not available for this provider") and the migration routes answer 404 `NOT_FOUND` ("the mobility surface is not enabled on this deployment"). The typed `MIGRATION_UNSUPPORTED_LOCAL_STORAGE` code exists but is not reached in this configuration. |
| S7 | Volvisor's pinned v1 cannot report attachment phase beyond `prepared`. | The pinned implementation returns `state = prepared` and has no route that advances it. A **corrected** design is tracked in [o3kio/volvisor#34](https://github.com/o3kio/volvisor/issues/34) and implemented on a Volvisor feature branch (not merged/released/pinned): `POST /v2/volumes/{id}/attachment-state`, a per-attachment `attachments[]` inspect view, monotonic `state_revision` (the detach-fencing `attachment_generation` never advances), and the `attachment_state_report` capability. ADR-028 requires observed attachment reporting; the pinned gap remains until the upstream disposition and a conscious re-pin. |
| S8 | Volvisor is a prototype with no CHV production qualification. | `volvisor/README.md`, `volvisor/AGENTS.md`, ADR-028 "Volvisor is a prototype". |
| S9 | There is no in-place adoption route from stord state. | `POST /v2/admin/nearline/{volume_id}/adopt` is DRBD failover promotion, not foreign-volume adoption. |
| S10 | Volvisor's privileged API is loopback + bearer token. | `volvisor/docs/configuration.md`: `listen` must be loopback without `admin_token`; privileged requests still `401` without it. |

### 2.3 What the code already gives us

Not everything is missing. These reduce cost and should be reused:

- `nodes.agent_ws_address` exists (`cmd/chv-controlplane/migrations/0029_node_ws_address.sql`); the BFF already returns a node-addressed `/ws/vms/{node_id}/…` console URL and the nginx example proxies it.
- `NodeInventory` already carries `vtep_ip`, `wireguard_public_key`, `underlay_mtu`, `authority_mode` (proto `control-plane-node.proto:24-51`) — extending it for advertise address is additive.
- The reconcile/operation model (desired state, generations, operation IDs, circuit breakers, reapers) is transport-agnostic in intent.
- `HostResourceController` already isolates the Core runtime from stord.
- Volvisor's `operation_id` idempotency and `generation` optimistic concurrency map cleanly onto CHV operation IDs and desired-state generations.

---

## 3. Scope

### 3.1 In scope

- Remote control-plane ↔ agent transport with mutual TLS.
- Node enrollment, address advertisement, inventory, rotation, and revocation for N hosts.
- Node-only packaging and install.
- Per-node console routing and per-node metrics/report paths.
- Explicit placement instead of "newest node".
- A per-volume storage-owner model and a single internal volume-backend seam.
- A Volvisor Volume API v2 client, capability negotiation, and `native-local` lifecycle behind that seam.
- Multi-host tenant networking (P5) as the gate for MH-B.
- Qualification: crash, retry, fencing, stale-generation, observability truthfulness.
- stord retirement criteria (not execution in the first pass).

### 3.2 Explicit non-goals

- Multiple control planes, control-plane HA, or control-plane failover.
- Cross-host live migration of native-local volumes.
- Enabling `ceph-rbd` or `nearline-replicated` to tenants.
- Automatic adoption or reformatting of existing stord volumes.
- Volvisor witness/DRBD/Rook cell operation from CHV.
- Replacing `chv-nwd`, `chv-agent`, or the Core runtime.
- Public exposure of any new listener.

---

## 4. Decisions to ratify

The plan is executable only after these are accepted. Each becomes an ADR on
ratification.

### 4.1 Multi-host

| ID | Decision | Recommended option | Rejected alternatives and why |
|---|---|---|---|
| **D-MH1** | Control-plane → agent transport | Opt-in agent gRPC TCP listener with **mandatory mTLS**; Unix socket remains the default and the single-host path. | SSH tunnelling (no identity model, no backpressure); a CP-side sidecar per node (new component, same problem); gRPC over WireGuard only (couples identity to fabric, which is optional). |
| **D-MH2** | Agent authenticates the control plane | A **dedicated control-plane client CA** (sub-CA or separate root) whose certs carry `clientAuth` + `CN=chv-controlplane-client`. Agent requires a cert from that CA; node certs are structurally unable to authenticate. | CN allowlist against the shared node CA (one-line misconfig from node→node); shared bearer token (replayable, unrevocable). |
| **D-MH3** | The control plane authenticates the agent | Agent server cert must chain to the node CA, carry `serverAuth`, and its **CN must equal the expected `node_id`**; the CP pins that equality per connection. | Trusting any CA-signed cert (a compromised node impersonates another node's VM control surface). |
| **D-MH4** | Node address authority | The node reports `advertise_addr` (explicit config, or a named interface; never auto-guessed) + `agent_grpc_port` in enrollment and every inventory cycle; the CP persists it in `nodes`. The CP never derives an endpoint from `node_id`. | DNS-only discovery (no bootstrap ordering guarantee); CP-side static inventory (drift, no rotation). |
| **D-MH5** | Multi-host posture | Replaces D1 ("single-host posture") in `DEPLOYMENT-ARCHITECTURE.md`. Adds a `deployment.topology = single-host \| multi-host` declaration enforced at startup, plus an updated §3.1 listener table. | Keeping the single-host declaration while shipping TCP (docs lie); unconstrained exposure (fails the exposure rule). |
| **D-MH6** | Console token trust root | CP-signed asymmetric token with `aud = "node:<node_id>"`, short TTL, verified by the agent against a CP public key; retire the shared `jwt_secret` for cross-host console. | Shipping the same symmetric secret to every node (any node mints any console token). |
| **D-MH7** | Placement | `node_id` is required whenever more than one schedulable node exists. A deterministic placement policy (least allocated vCPU/memory, anti-affinity opt-in) replaces `ORDER BY enrolled_at DESC`. Single-node keeps the implicit default. | Silent "newest node" in multi-host (unpredictable, unauditable). |
| **D-MH8** | Agent gRPC port | `9443/TCP` (unused in the §3.1 listener table; the only `9443` references today are config test fixtures). | Reusing `8444` (console) or `8443` (CP) — conflates trust domains. |

### 4.2 Volvisor

| ID | Decision | Recommended option | Rejected alternatives and why |
|---|---|---|---|
| **D-VS1** | Where Volvisor sits | **Node-local, loopback-only** `volvisord`, spoken to by `chv-agent`. The control plane never speaks Volvisor. | CP→Volvisor direct (breaks the CP/node boundary, requires exposing storage admin per node). |
| **D-VS2** | Ownership | Per-volume `storage_owner ∈ {legacy-stord, volvisor}`; immutable while attached; changed only by an explicit, operator-authorized, offline procedure. No dual writers, ever. | Inferring owner from class at runtime (two writers during rollback); auto-adoption (destructive, violates Volvisor rule 7). |
| **D-VS3** | Seam | Introduce a single internal `VolumeBackend` trait below both the Core runtime and the legacy reconcile path; `HostResourceController` and legacy call sites both route through it. | Implementing Volvisor only behind `HostResourceController` (leaves `reconcile.rs` and `agent_server.rs` on stord — the second writer problem). |
| **D-VS4** | Class vocabulary | New canonical values only (`native-local`). Legacy `local`/`lvm`/`iscsi`/`ceph` continue to mean stord and are never translated. | Mapping `lvm` → `native-local` (different identity, fencing, and migration semantics). |
| **D-VS5** | Client shape | Hand-written typed client in `crates/chv-storage-volvisor` + contract-conformance tests run against the pinned Volvisor source in CI. No production dependency on Volvisor prototype crates. | Git-dependency on `volvisor-types` (drags a prototype's semver and build graph into CHV releases). |
| **D-VS6** | Prototype posture | Pin an exact SHA; gate every class on a capability handshake and per-class qualification; `fake` provider is test-only and refused in production config. | "Enable everything because the API exists." |
| **D-VS7** | Attachment-phase honesty | CHV records `Prepared` + `observed = false` until the pin advertises `attachment_state_report`. Then CHV may report `Advertised` **only** from a truthful `vmm_configured` attestation after installing the frontend; `Active` is never claimed in v1 (no guest-confirmation path). Optional local block-device probe remains corroboration only. | Treating `Prepared` as `Active` (observability lie, breaks ADR-028) or a host path check as guest confirmation. |

---

## 5. Target architecture

### 5.1 Two independent axes

```
                 ┌──────────────────────────────────────────┐
                 │            chv-controlplane              │
                 │  orchestrator / lifecycle / migration    │
                 │            NodeEndpointResolver          │
                 └───────────┬──────────────────┬───────────┘
                             │                  │
              Unix (default) │                  │ mTLS TCP (multi-host)
                             │                  │
                 ┌───────────▼──────────────────▼───────────┐
                 │                chv-agent                  │
                 │   gRPC server (Unix | TCP+mTLS)           │
                 │   Core runtime ─┐                         │
                 │   legacy reconcile ─┤                     │
                 │                     ▼                     │
                 │            VolumeBackend router           │
                 │         ┌───────────┴───────────┐         │
                 │         ▼                       ▼         │
                 │   StordBackend            VolvisorBackend │
                 │   (Unix gRPC)             (HTTP/JSON)     │
                 └─────────┬───────────────────────┬─────────┘
                           ▼                       ▼
                      chv-stord                volvisord
                   (legacy-owned)          (volvisor-owned)
```

The axes are independent: a single-host install can run Volvisor; a
multi-host fleet can run stord. That independence is what makes incremental
delivery and rollback possible.

### 5.2 Control-plane → agent dispatch

**New types** (`crates/chv-controlplane-service/src/node_endpoint.rs`):

```rust
pub enum NodeEndpoint {
    Unix { path: PathBuf },
    Tcp  { host: String, port: u16, server_name: String },
}

pub trait NodeEndpointResolver: Send + Sync {
    fn resolve(&self, node_id: &NodeId) -> Result<NodeEndpoint, ChvError>;
}
```

- `DbNodeEndpointResolver` reads `nodes.agent_grpc_address`,
  `nodes.agent_grpc_server_name`, `nodes.agent_endpoint_generation`, with a
  short TTL cache and generation-based invalidation.
- `StaticPatternResolver` wraps today's `agent_socket_pattern` for
  single-host and for tests. `resolve_agent_socket` stays as a private helper
  inside it so its traversal tests keep their value.
- `NodeClient::connect(endpoint, tls_material)` gains a TCP branch:
  `Endpoint::from_shared(format!("https://{host}:{port}"))` +
  `tls_config` with the CP client identity, node CA, and
  `domain_name(server_name)`.
- `NodeClientPool` keys on `(node_id, endpoint_generation)` so a moved node
  never reuses a stale channel; `evict(node_id)` is called on
  re-enrollment/rotation.
- Failure semantics stay identical: same circuit breaker, same per-method
  keys, same timeout mapping. A TCP connection failure must produce
  `BackendUnavailable`, not a panic or an operation retry storm.

**Call-site migration is mechanical but complete.** Every `resolve_agent_socket`
caller moves to the resolver in the same PR series; leaving one caller on the
path pattern is how a "multi-host" release silently manages only localhost.

### 5.3 Agent listener

`crates/chv-agent-core/src/agent_server.rs` currently owns the serve loop.
Refactor to:

```rust
pub enum AgentListener {
    Unix { path: PathBuf },
    Tcp    { bind: SocketAddr, tls: Arc<ServerTlsConfig> },
}
```

Rules enforced at startup (fail closed, no warnings-and-continue):

1. TCP is enabled only by explicit `[agent.grpc] listen = …`.
2. `listen` must not be `0.0.0.0`/`::` unless
   `allow_wildcard_bind = true` **and** `advertise_addr` is set.
3. `cert_path`, `key_path`, `client_ca_path` are all-or-nothing.
4. The client CA must be the **control-plane client CA**, not the node CA.
   Startup inspects the CA bundle's subject and refuses a node CA unless
   `allow_node_ca_clients = true` (test-only escape hatch, refused when
   `deployment.topology = multi-host`).
5. A bind failure is fatal. No random port fallback (same rule as the
   fabric provider).
6. Unix serving remains available concurrently when configured, so the
   single-host tooling path is never broken by enabling TCP.

`AgentConfig` gains:

```toml
[agent.grpc]
listen = "10.20.0.11:9443"        # opt-in
advertise_addr = "10.20.0.11"     # required with TCP
cert_path = "/etc/chv/certs/node-server.crt"
key_path = "/etc/chv/certs/node-server.key"
client_ca_path = "/etc/chv/certs/cp-client-ca.crt"
# allow_wildcard_bind = false
# allow_node_ca_clients = false
```

### 5.4 Control-plane listener and PKI

The CP gRPC listener must become reachable by remote agents. This is an
architecture change to §3.1 and is part of D-MH5.

- New `[grpc]` (or extended `[tls]`) config: `bind`, `advertise_addr`,
  `server_cert_path`, `server_key_path`, `client_ca_path` (node CA),
  optional `bind_interface`.
- Server certificate SANs must include the advertised management DNS name
  and IP. `scripts/install.sh` and the packaged examples must generate them;
  a cert missing the advertised identity is a startup error, not a runtime
  handshake failure.
- Client certificates presented by agents continue to be node certificates
  (`CN=<node_id>`), now with explicit EKUs:
  - `clientAuth` for the CP gRPC surface,
  - `serverAuth` for the agent gRPC surface,
  - both, or a split pair. Recommendation: **split identities** —
    `node-client` (to CP) and `node-server` (from CP) — so a stolen client
    cert cannot impersonate a node's server. The existing
    `agent-client.{crt,key}` (`CN=chv-agent`) remains the enrollment
    bootstrap credential and is not the steady-state identity.
- `issue_node_certificate` gains `advertise_addr`/`advertise_dns` and writes
  IP/DNS SANs accordingly, plus explicit EKU and a `not_after` shorter than
  the CA. **Constraint:** `node_id` MUST remain the first DNS SAN —
  `peer_identity::parse_node_id_from_der` takes the first DNS SAN, so
  prepending a human-readable name silently changes the authenticated
  identity. Append additional names after it or use IP SANs.
- Rotation (`RotateNodeCertificate`) must update both identities and bump
  `agent_endpoint_generation`; the pool evicts.
- Revocation: the CP refuses a node whose cert serial is on the denylist
  (`nodes.certificate_serial` is already stored; the denylist is new P1
  work); agents refuse CP clients whose cert is expired or not from the CP
  client CA. No CRL infrastructure in the first pass — serial denylist at the
  CP, short lifetimes.

### 5.5 Node-only packaging

- Split `chv-node`'s dependency on `chv-controlplane` per D2 option (c):
  `chv-node-common` (agent, stord, nwd, runtime deps) + `chv-node` (common +
  node systemd units) + `chv-controlplane-server` (the CP). `chv-node`
  `Recommends:` the CP only in the all-in-one meta-package.
- `scripts/install.sh` gains `--role=all-in-one|control-plane|node` and
  `--control-plane=https://cp.example:8443`; node role writes an enrollment
  bootstrap token path, not a local CA/CP config.
- Node role must not generate a CA or a CP server cert. Enrollment is the
  only identity source.
- `packaging/config/chv.yaml` remains reference-only; the plan does not
  pretend it configures anything.

### 5.6 Storage authority

**Owner model.** `volumes.storage_owner` is authoritative. The class string
is descriptive, never a dispatcher for ownership.

**Seam.** New `crates/chv-storage-api` (or a module in
`chv-hypervisor-api`) with a narrow trait:

```rust
#[async_trait]
pub trait VolumeBackend: Send + Sync + 'static {
    fn owner(&self) -> StorageOwner;                    // LegacyStord | Volvisor
    fn classes(&self) -> &'static [&'static str];
    async fn ensure_volume(&self, req: EnsureVolume) -> Result<VolumeHandle, ChvError>;
    async fn attach(&self, h: &VolumeHandle, vm: &VmId) -> Result<AttachResult, ChvError>;
    async fn detach(&self, h: &VolumeHandle, vm: &VmId) -> Result<(), ChvError>;
    async fn close(&self, h: VolumeHandle) -> Result<(), ChvError>;
    async fn destroy(&self, id: &VolumeId, op: Option<&str>) -> Result<(), ChvError>;
    async fn grow(&self, id: &VolumeId, size: u64, op: Option<&str>) -> Result<(), ChvError>;
    async fn inspect(&self, id: &VolumeId) -> Result<VolumeObservation, ChvError>;
}
```

Capability methods (`supports_snapshot`, `supports_clone`,
`supports_migration`, `supports_shrink`) are answered by the backend and
surfaced to the CP, which refuses unsupported operations **before** journaling
them. A refused operation must be terminal, not retried.

`AgentResourceController` (Core) and the legacy `reconcile.rs` /
`agent_server.rs` call sites both construct the same router and select the
backend by `storage_owner`. `backend_class` is validated against the selected
backend's `classes()`; a mismatch is `InvalidArgument`, never a silent
fallback.

**Volvisor client** (`crates/chv-storage-volvisor`):

- `reqwest` with a process-wide client, connect/request timeouts, and no
  redirect following.
- Bearer token read from a file at startup (`admin_token_file`, mode 0400,
  owned by the agent user); never logged; redacted in errors.
- `GET /v2/capabilities` at startup and on health failure; cache the
  result; refuse operations whose class is absent.
- Types mirror the pinned contract (`api_version = "volvisor.volume.v2"`).
- Operation IDs: `op_id = chv-op-id` when the CP supplies one; otherwise a
  deterministic derivation defined in
  [`chv-volume-backend-v1`](../specs/contracts/chv-volume-backend-v1.md) §6.2
  (`chv:<verb>:<sha256-hex>`), so a retry after a crash replays the same
  idempotency key instead of creating a second attachment. Never a random
  UUID for a retried mutation, and never the reserved `derived:` prefix.
- Identifier conformance: every CHV id crossing the boundary (`volume_id`,
  `attachment_id`, `vm_id`, node id, tenant) MUST satisfy Volvisor's
  `[A-Za-z0-9_.:-]{1,128}` rule. Non-conforming ids are rejected, never
  rewritten.
- Capability negotiation: `GET /v2/capabilities` returns the provider,
  capability set, and served classes. The native-local baseline is
  `{create, attach, resize}`; a missing token disables the class rather than
  degrading silently. The LVM provider also advertises
  `same_vg_extent_move` (pvmove), which CHV v1 does not consume.
- Error mapping is by Volvisor's machine-readable `code`, never by HTTP status
  (contract §9): `UNSUPPORTED_*`/`*_UNSUPPORTED` → `Unimplemented`;
  `INVALID_REQUEST` → `InvalidArgument`; `NOT_FOUND` → `NotFound`;
  `STALE_GENERATION`/`STALE_EPOCH` → `StaleGeneration` (no auto-retry);
  `IDEMPOTENCY_CONFLICT`/`INVALID_STATE`/`WRITER_ALREADY_ACTIVE` → `Conflict`;
  `FORBIDDEN` → `AccessDenied`; `INTERNAL`/transport failure →
  `BackendUnavailable` (bounded retry); `401/403` → `AccessDenied` plus a loud
  log (misconfigured token, not a transient fault).

### 5.7 Mapping CHV intent onto Volvisor

| CHV operation | Volvisor call | Notes |
|---|---|---|
| `open_volume` (Core) / first open (legacy) | `GET /v2/volumes/{id}`; if `404`, `POST /v2/volumes` with `volume_id = CHV volume_id`, `project_id` from the attachment ref, `class = native-local`, `size_bytes`, `provisioning` | Volume creation is *ensure-exists*, idempotent on `operation_id`. `seed_from` is not a Volvisor field: CHV must either pre-seed outside Volvisor or refuse `seed_from` for Volvisor-owned volumes in P4. |
| `attach_volume_to_vm` | `POST /v2/volumes/{id}/attach` with `attachment_id = CHV attachment_id`, `host_id = node_id`, `vm_id`, `expected_volume_generation`, `requested_frontend = "virtio-blk"` | Returns `frontend.host_device_path`, `attachment_generation`, and `volume_generation`. A stale expected generation is a typed conflict. |
| `detach_volume_from_vm` | `POST /v2/volumes/{id}/detach` with `attachment_id`, `expected_attachment_generation`, and `vm_stopped_or_io_drained_proof` | Stale generation → conflict, never forced. The drain proof must be truthful: `io_drained` only after CHV's own drain check. |
| `close_volume` | idempotent detach (or no-op if already detached) | Never deletes the volume. |
| `destroy_volume` | `DELETE /v2/volumes/{id}` with `expected_generation` and an explicit `data_erasure_policy` | Refused unless owner = volvisor and the volume is detached. There is no implicit erasure policy. |
| `resize_volume` (grow) | `POST /v2/volumes/{id}/grow` | Grow-only; shrink refused. `new_size_bytes` must be 512-aligned. |
| `snapshot_volume`, `clone_volume`, `restore_volume`, `delete_volume_snapshot` | none | Refused with `Unimplemented` for volvisor-owned volumes, advertised via capabilities. |
| disk migration RPCs | none | CHV does not call Volvisor mobility routes for `native-local`: `check-mobility` and `/v2/migrations/*` are disabled for the LVM provider (404). The VM is marked migration-ineligible by CHV policy and the scheduler refuses before journaling. |

CHV's `open_volume` / `attach_volume_to_vm` split does not map one-to-one onto
Volvisor's attach. For a Volvisor-owned volume, `open_volume` is
ensure-exists (inspect, then create on `404`) and returns the volume id as the
attachment handle; the real backend attachment is created by
`attach_volume_to_vm`. `close_volume` therefore means "detach if attached" and
MUST be idempotent. This is why the contract defines `ensure` and `attach` as
separate operations.

**Attachment phase.** CHV records:
`state = Prepared`, `observed = false`, `backend_attachment_id`,
`backend_generation`. It may corroborate with a host-side existence check of
`frontend.host_device_path`, recorded as `device_present = true/false` —
never as `Active`. The corrected upstream design is P0.5 and is
[o3kio/volvisor#34](https://github.com/o3kio/volvisor/issues/34); once it is
dispositioned, merged and the pin consciously re-reviewed, P4 adds
`Advertised` reporting from a truthful `vmm_configured` attestation behind
capability negotiation. P4 does **not** add `Active`: v1 has no
guest-confirmation path, and `Active` requires `guest_confirmed` evidence.
It also records `state_revision` verbatim and never derives evidence ordering
from `attachment_generation` (the detach fence) or wall-clock time.

**Project identity.** `StorageAttachmentRef` gains `project_id` (proto
additive) filled by the CP from the VM's tenant. The agent passes it through
the router. Until it lands, Volvisor-owned create is refused — an empty
project id must not default to a shared tenant.

---

## 6. Data model and configuration changes

### 6.1 Migrations

`cmd/chv-controlplane/migrations/0063_node_endpoints.sql`

```sql
ALTER TABLE nodes ADD COLUMN advertise_addr text;
ALTER TABLE nodes ADD COLUMN agent_grpc_address text;          -- host:port
ALTER TABLE nodes ADD COLUMN agent_grpc_server_name text;       -- expected TLS identity
ALTER TABLE nodes ADD COLUMN agent_endpoint_generation integer NOT NULL DEFAULT 1;
ALTER TABLE nodes ADD COLUMN agent_transport text NOT NULL DEFAULT 'unix';  -- unix | tcp
```

`cmd/chv-controlplane/migrations/0064_volume_storage_owner.sql`

```sql
ALTER TABLE volumes ADD COLUMN storage_owner text NOT NULL DEFAULT 'legacy-stord';
ALTER TABLE volumes ADD COLUMN storage_generation integer;
ALTER TABLE volumes ADD COLUMN storage_pool_ref text;
ALTER TABLE volume_observed_state ADD COLUMN backend_attachment_id text;
ALTER TABLE volume_observed_state ADD COLUMN backend_attachment_generation integer;  -- detach fence; never advanced by a report
ALTER TABLE volume_observed_state ADD COLUMN attachment_phase text;   -- prepared | advertised | active
ALTER TABLE volume_observed_state ADD COLUMN attachment_state_revision integer;  -- backend evidence ordering (authoritative)
ALTER TABLE volume_observed_state ADD COLUMN attachment_evidence text;   -- vmm_configured | guest_confirmed | NULL (prepared)
ALTER TABLE volume_observed_state ADD COLUMN attachment_observed integer NOT NULL DEFAULT 0;
```

Backfill rules:

- Every existing row → `storage_owner = 'legacy-stord'`, `storage_generation NULL`.
- `storage_owner` is never inferred from `storage_class`.
- A row with `storage_owner = 'volvisor'` and a legacy class is a **startup
  integrity error** on the CP, not a runtime guess.

### 6.2 Configuration additions

| File | Key | Purpose |
|---|---|---|
| CP | `deployment.topology` | `single-host` / `multi-host`; validated against listeners |
| CP | `[grpc] bind`, `advertise_addr`, `client_ca_path` | remote agent reachability |
| CP | `[agent_transport] mode`, `socket_pattern`, `tcp_port`, `server_name_template` | resolver selection |
| CP | `[agent_transport.tls] client_cert_path/client_key_path/ca_cert_path` | CP client identity |
| Agent | `[agent.grpc]` (see §5.3) | opt-in listener |
| Agent | `advertise_addr` | inventory fact |
| Agent | `[storage.volvisor] enabled`, `endpoint`, `admin_token_file`, `api_version`, `request_timeout_ms` | Volvisor |
| Agent | `[storage.ownership] mode = legacy-stord \| volvisor \| both` | operator switch, default `legacy-stord` in P3/P4 |

`mode = both` exists only for migration windows and is refused when
`deployment.topology = multi-host` if any volume has an ambiguous owner.

---

## 7. Phases and work packages

Each phase ends in a **gate**: a list of evidence artifacts, not a checklist
of merged PRs. A phase does not start until the previous gate passes.

### P0 — Freeze decisions and contracts (1–2 weeks)

| WP | Work | Files |
|---|---|---|
| P0.1 | Write ADR-029 (multi-host dispatch) from §4.1, mark Proposed; update ADR-028 status wording for this plan | `docs/specs/adr/029-multi-host-dispatch.md` (new), `028-…` |
| P0.2 | Write the `chv-volume-backend-v1` internal contract (owner, capabilities, error mapping, idempotency derivation) | `docs/specs/contracts/chv-volume-backend-v1.md` (new) |
| P0.3 | Update `DEPLOYMENT-ARCHITECTURE.md` §3.1 listener table, §5 UC-6, §8 D1/D2 as **proposed** | `docs/DEPLOYMENT-ARCHITECTURE.md` |
| P0.4 | Pin Volvisor: exact SHA, API version, provider/config surface, capability set; record it in the integration record (not `compat.rs` — Volvisor is not a versioned CHV component yet; that is a P3 change) | `docs/specs/contracts/chv-volume-backend-v1.md` §11/§13, this plan §10.1 |
| P0.5 | File the upstream attachment-phase amendment request; record the accepted/dispositioned outcome | Volvisor issue [#34](https://github.com/o3kio/volvisor/issues/34) — **filed 2026-11-02, disposition pending** |
| P0.6 | Decide `project_id` carriage and `seed_from` posture for Volvisor-owned volumes | this plan + proto draft |

**Status 2026-11-02:** P0.1 ([ADR-029](../specs/adr/029-multi-host-dispatch.md)),
P0.2 ([`chv-volume-backend-v1`](../specs/contracts/chv-volume-backend-v1.md)),
the proposed markings in P0.3, and the P0.4 pin record (§10.1.1) are written.
The pin is a review reference until P3's contract tests run against it in CI.
P0.5 is **filed but not dispositioned**: the corrected upstream design is
[o3kio/volvisor#34](https://github.com/o3kio/volvisor/issues/34) (a
`POST /v2/volumes/{id}/attachment-state` consumer report with monotonic
`state_revision` and an unchanging detach-fencing generation, an `attachments[]`
inspect projection, and the `attachment_state_report` capability). The design
is implemented on a Volvisor feature branch with conformance/API tests, but it
is **not merged, released or pinned**. Acceptance is not assumed; §8 keeps the
`prepared`-only posture until the disposition lands, the implementation is
merged, and the pin is re-reviewed. P0.6 (project/seed decisions)
also remains open, and its recommendation is already encoded in contract v1
§10/§11.3 pending ratification. No P1 work has started.

**Gate P0:** all decisions in §4 (D-MH1–D-MH8, D-VS1–D-VS7) ratified; no
code merged that assumes an unratified option.

### P1 — Remote dispatch (5–7 weeks)

| WP | Work | Files |
|---|---|---|
| P1.1 | `NodeEndpoint`, resolver trait, DB-backed resolver, static compat resolver | `crates/chv-controlplane-service/src/node_endpoint.rs` (new) |
| P1.2 | `NodeClient` TCP+mTLS branch; pool keying by generation | `node_client.rs`, `node_client_pool.rs` |
| P1.3 | Migrate **all** call sites (orchestrator, migration, overlay, lifecycle, reaper) | those files |
| P1.4 | Agent listener abstraction; Unix + optional TCP; fail-closed validation | `crates/chv-agent-core/src/agent_server.rs`, `cmd/chv-agent/src/main.rs` |
| P1.5 | CP gRPC remote bind + server cert SANs + advertised identity validation | `cmd/chv-controlplane/src/bootstrap.rs`, `crates/chv-controlplane-service/src/server.rs` |
| P1.6 | Split node identities (client/server), EKUs, SANs, rotation, serial denylist | `enrollment.rs`, `scripts/install.sh` |
| P1.7 | Dedicated CP client CA generation and distribution | `scripts/install.sh`, packaging examples |
| P1.8 | `advertise_addr` detection/validation + inventory fields + persistence | `chv-config`, `control-plane-node.proto`, `ReportNodeInventory` |
| P1.9 | Listener contract tests: refuse node CA as CP, refuse CN mismatch, refuse plaintext, refuse wildcard without advertise | service + agent tests |

**Gate P1 (MH-A minus packaging):**

1. Two hosts, separate machines/VMs: node B enrolls over mTLS, reports
   inventory, appears in `nodes` with a populated `agent_grpc_address`.
2. CP dispatches `StartVm`/`StopVm`/`ReportVmState` to B and never touches a
   local socket for B.
3. A certificate from node A cannot authenticate to node B's agent listener
   (explicit negative test).
4. A cert with `CN=node-a` presented as a server for `node-b` is rejected by
   the CP (explicit negative test).
5. Killing the TCP path produces `BackendUnavailable` and circuit-breaker
   behavior identical to the Unix path.
6. Single-host install with no `[agent.grpc]` behaves exactly as before
   (regression suite green).

### P2 — Node lifecycle and operability (4–5 weeks)

| WP | Work | Files |
|---|---|---|
| P2.1 | Node-only package split (`chv-node-common`, `chv-node`, meta-package); `Recommends` not `Depends` | `packaging/nfpm/*` |
| P2.2 | `install.sh --role` and remote enrollment (token, CP address, no local CA) | `scripts/install.sh` |
| P2.3 | Console routing to remote nodes via `agent_ws_address`; per-node TLS/token | BFF handlers, nginx examples, `config.rs` |
| P2.4 | CP-signed console tokens (`aud=node:<id>`) replacing the shared `jwt_secret` for cross-host | `chv-config`, agent console auth |
| P2.5 | Explicit placement; remove newest-node default for multi-node; required `node_id` | BFF `vms.rs`, CP placement module |
| P2.6 | Drain/maintenance semantics for local-storage VMs (block, not evacuate) | orchestrator, `DrainNode`/`EnterMaintenance` |
| P2.7 | Node removal/revocation: cert serial denylist, endpoint tombstone, in-flight operation reaping | enrollment, reaper |
| P2.8 | Node health/last-seen surfacing in CLI and UI | `chvctl`, BFF |

**Gate P2:** a three-node fleet can be installed from packages (no source
build), enrolled, inventoried, placed onto explicitly, consoled, drained,
and one node can be removed without orphaning operations. Node-only hosts
have no control-plane process or CA key.

### P3 — Volvisor seam and read-only qualification (4–5 weeks)

| WP | Work | Files |
|---|---|---|
| P3.1 | `VolumeBackend` trait + router + capability advertisement | `crates/chv-storage-api` (new) or `chv-hypervisor-api` |
| P3.2 | `StordBackend` adapter wrapping today's `StordClient`; behaviour-preserving | `chv-agent-core/src/daemon_clients.rs`, `resources.rs` |
| P3.3 | Route both the Core path and the legacy reconcile path through the router | `core_runtime.rs`, `reconcile.rs`, `agent_server.rs` |
| P3.4 | `chv-storage-volvisor` client (create/inspect/attach/detach/grow/delete/capabilities), token handling, error mapping | new crate |
| P3.5 | Contract-conformance tests against the pinned Volvisor source (schema + fake provider) | new crate tests + CI job |
| P3.6 | `storage_owner` schema + integrity checks + `chvctl volume owner` | migration 0064, `chvctl` |
| P3.7 | Shadow mode: `volvisord` runs on a node, reports capabilities/health, **no volume is created or attached** | agent config, observability |

**Gate P3:**

1. Legacy suite is green with the router in place (no behaviour change).
2. `volvisord` health and capabilities are visible through CHV without
   exposing the token.
3. A Volvisor-owned row cannot be opened by `StordBackend` and vice versa —
   negative tests on both sides.
4. Contract tests fail when the pinned schema drifts (prove the pin works).

### P4 — Native Volvisor local lifecycle (5–7 weeks)

| WP | Work | Files |
|---|---|---|
| P4.1 | `VolvisorBackend::ensure_volume` / `attach` / `detach` / `close` with deterministic op IDs | `chv-storage-volvisor` |
| P4.2 | `project_id` carriage end to end (`StorageAttachmentRef`, proto, CP compile step) | `cellhv-core-types`, proto, CP |
| P4.3 | Attach-phase recording (`Prepared`, `observed=false`) and refusal to claim `Active` | observed-state writer |
| P4.4 | Capability-based refusal for snapshot/clone/restore/shrink; terminal `Unimplemented` | CP lifecycle + agent RPCs |
| P4.5 | VM create/delete end-to-end on a Volvisor-owned `native-local` volume | Core runtime + legacy path tests |
| P4.6 | Migration eligibility: any VM with a Volvisor native-local volume is ineligible; scheduler refuses with a specific reason | CP placement/migration |
| P4.7 | Node inventory advertises `native-local` only when the capability handshake succeeds | agent inventory |
| P4.8 | `seed_from` posture: refuse for Volvisor-owned volumes (or implement an explicit pre-seed job) | router + CP validation |
| P4.9 | Operator doc: ownership model, refusal matrix, how to inspect, how to roll back | `docs/` |

**Gate P4:** on a real host with a claimed LVM pool, a VM boots from a
Volvisor-created `native-local` volume, survives agent restart, detaches on
delete, and leaves no stord session. The refusal matrix is enforced and
observable.

### P5 — Multi-host tenant networking (parallel; 8–12 weeks, owned by ADR-021 plan)

This phase is tracked in `FABRIC_IMPLEMENTATION_PLAN.md`; this plan only
records the **dependency and the gate**, because MH-B is not true without it.

> **Status caveat (2026-11-02):** `FABRIC_IMPLEMENTATION_PLAN.md`'s "Current
> state" section predates the code it describes. It says `allocate_vni` has no
> production caller and the orchestrator has no `UpdateOverlay` dispatch arm;
> both now exist (`fabric_planner.rs`, `orchestrator.rs:735-739`). It also
> says the `UpdateOverlayRequest.fabric` field is still to be added; it is
> present in both protos. Treat that section as historical and verify against
> the tree. The plan's design decisions and gate remain the authority.

| WP | Work | Dependency on this plan |
|---|---|---|
| P5.1 | Fabric plan carriage, key identity, VTEP registry convergence from inventory | Needs P1 (remote dispatch) to fan out `UpdateOverlay` |
| P5.2 | HER VXLAN + WireGuard datapath in `chv-nwd` | none |
| P5.3 | Fleet-wide network semantics (`node_id IS NULL`) | P5.1 |
| P5.4 | MTU negotiation and DHCP option 26 | P5.1 |
| P5.5 | Cross-host ARP/ping acceptance (real MACs) | P5.2 |

**Gate P5 (MH-B):** two VMs on two physical hosts, same tenant network, ARP
and ping with ordinary resolution; captured traffic shows WireGuard
encapsulation; a third host joining mid-flight converges.

### P6 — Qualification and failure campaign (6–8 weeks)

| WP | Work |
|---|---|
| P6.1 | Crash matrix: kill agent, CP, volvisord, and the host at each operation boundary; replay must not duplicate an attach or lose an idempotency key |
| P6.2 | Network partition: CP↔agent split during create/delete; stale generation after heal |
| P6.3 | Certificate rotation mid-operation; revoked node cannot reconnect |
| P6.4 | Storage failures: LVM thin-pool exhaustion, device disappearance, Volvisor 5xx and timeout storms |
| P6.5 | Observability truthfulness: no `Active` without evidence, no `Healthy` without proof, metrics for backend/op/result |
| P6.6 | Upgrade/downgrade: mixed-version fleet (N-1 agent, N CP) under the compat matrix |
| P6.7 | Security review: listener exposure, token handling, cert boundary, log redaction |

**Gate P6:** the failure campaign report is published with exact
source/hardware/versions; every failure is classified (fixed, documented,
refused). No "known flake".

### P7 — stord retirement and default flip (3–4 weeks)

| WP | Work |
|---|---|
| P7.1 | New installs default to Volvisor-owned `native-local` where the handshake passes; stord still installed for legacy rows |
| P7.2 | Legacy volume inventory: every remaining `legacy-stord` row is tagged, reported, and documented |
| P7.3 | Operator migration procedure: offline re-create + data copy, or keep legacy forever; **no auto-adoption** |
| P7.4 | Deprecation notice, then removal of stord from the node package once P7.2 is empty or explicitly waived |
| P7.5 | Docs: remove "Volvisor pending" language; state exactly which classes are supported and which are not |

**Gate P7:** no shipped configuration creates a new stord-owned volume; the
remaining set is enumerated with an owner and a retirement date; removing
stord does not break any documented workflow.

---

## 8. Security model

| Threat | Control | Phase |
|---|---|---|
| Node A connects to node B's agent and drives its VMs | Agent trusts only the CP client CA; node certs structurally rejected | P1 |
| Compromised node presents another node's identity to the CP | Split node identities; server CN pinned to expected `node_id` | P1 |
| Compromised node mints console tokens for another node | CP-signed asymmetric tokens with `aud=node:<id>`; agents verify the CP public key and check the audience | P2 |
| MITM on the management network | mTLS both directions; server name checked; no `danger_accept_invalid_certs` anywhere | P1 |
| Stolen Volvisor admin token | 0400 file, agent-user only, never logged, loopback-only bind, `401` mapped loudly | P4 |
| Cross-tenant volume attach | `project_id` required; Volvisor ownership+fencing is the authority; CHV never fabricates an attachment id | P4 |
| Replayed mutation after crash | Deterministic operation IDs; generation checks; conflict is terminal | P3/P4 |
| Stale endpoint after node move | `agent_endpoint_generation` invalidates the pool; rotation evicts | P1 |
| Over-broad listener exposure | Explicit opt-in, no wildcard without advertise, documented exposure rule, startup validation of topology vs listeners | P1/P2 |

Non-negotiable: no plaintext gRPC, no `client_auth_optional`, no shared
symmetric secret crossing a host boundary, no token in a URL that reaches a
log.

---

## 9. Validation ladder

| Level | What it proves | Required for |
|---|---|---|
| L0 unit | resolver/endpoint/capability/error-mapping logic | every WP |
| L1 simulated | full lifecycle against fake providers and a mock Volvisor | P3, P4 |
| L2 single-host real | one host, real stord/Volvisor, KVM VM | P4 |
| L3 two-host | remote dispatch + console + inventory | P1, P2 |
| L4 three-host privileged | fabric, node loss, concurrent operations | P5, P6 |
| L5 crash matrix | kill/replay at every boundary | P6 |

Rules:

- L3+ evidence must come from separate kernels or separate machines, not
  three containers on one host. Volvisor's own rule 20 applies by analogy:
  three microVMs on one host are not three failure domains.
- Every claim in `DEPLOYMENT-ARCHITECTURE.md` gets a tier marker. A phase
  cannot mark anything `[QUALIFIED]` from L1.
- Negative tests are evidence. A gate that only proves the happy path is
  incomplete.

---

## 10. Compatibility, pins, and rollback

### 10.1 Pins

- **Volvisor:** exact SHA (candidate
  `286f038c99db5aba29243acd7d3e0bfdb25bf50a`), API `volvisor.volume.v2`,
  provider `lvm`, capability set recorded at gate P3. Any change to the pin
  updates the integration record and requires a contract-test re-run; Volvisor
  is not a CHV component and does not enter `compat.rs` until P3 defines it.
- **CHV proto:** additive only. New fields get new numbers; no reuse.
  `buf lint` and `buf breaking --against main`
  (`.github/workflows/proto.yml`) remain the enforcement.
- **Agent/CP skew:** mixed-version fleet supported for one minor version,
  with the matrix updated per release. An agent that cannot advertise an
  endpoint is treated as Unix-only, not as broken.

#### 10.1.1 Volvisor pin record (P0.4)

Recorded 2026-11-02 from the working tree. This is a **review reference**, not
a release pin; the pin becomes normative when P3's contract tests run against
it in CI.

| Item | Value |
|---|---|
| Repository | `o3kio/volvisor` |
| Commit | `286f038c99db5aba29243acd7d3e0bfdb25bf50a` (no tags in the repo; `git describe --tags --always` → `286f038`) |
| Workspace version | `0.1.0` |
| API version string | `volvisor.volume.v2` |
| Provider | `lvm` (`native-local` class); `fake` is test-only |
| Served classes | `native-local` only (`volvisor-lvm` `SUPPORTED_CLASSES`) |
| Capability baseline | `{create, attach, resize}` (`CapabilitySet::native_local_p0`); LVM adds `same_vg_extent_move`, which CHV v1 does not call |
| Transport | HTTP/JSON over loopback; `/healthz` and `/v2/capabilities` for readiness |
| Privileged auth | `admin_token` bearer; unset means loopback-only and privileged requests `401` |
| Required LVM config | `listen`, `journal_dir`, `provider = "lvm"`, `lvm_vg_prefix`, `device_claim_token` |
| Routes used by CHV | `GET /v2/capabilities`, `GET /healthz`, `GET \| POST /v2/volumes`, `POST …/attach`, `POST …/detach`, `POST …/grow`, `DELETE /v2/volumes/{id}` (plus `POST …/attachment-state` only if/when the re-pinned build advertises `attachment_state_report`) |
| Routes refused by CHV | `/v2/admin/*`, `/v2/migrations/*`, `/v2/vms/*/check-mobility`, nearline adopt |
| Upstream design (P0.5) | [o3kio/volvisor#34](https://github.com/o3kio/volvisor/issues/34) — corrected design implemented on a Volvisor feature branch; filed, disposition/merge/release pending; **not** part of this pin |

### 10.2 Rollback

| Change | Rollback |
|---|---|
| TCP dispatch | Flip `agent_transport.mode = unix`; no schema change needed; endpoints remain recorded but unused |
| Agent TCP listener | Remove `[agent.grpc]`; Unix serving is unaffected |
| Volvisor volumes | Per-volume owner is immutable; roll back by stopping new Volvisor creates, not by rewriting rows |
| Volvisor daemon | Stop `volvisord`; Volvisor-owned volumes become unavailable (honest failure), never silently opened by stord |
| `storage_owner` migration | Additive with a safe default; downgrade keeps the column and ignores it |
| Fabric | Out of scope here; governed by ADR-021's teardown rules |

The one rollback that must be **impossible** is falling back to stord for a
volume Volvisor has attached. Fencing wins over availability: a failed
Volvisor attach is a failed operation, never a second writer.

---

## 11. Risks

| Risk | Likelihood | Impact | Mitigation | Early trigger |
|---|---|---|---|---|
| Volvisor prototype quality blocks VS-A | High | High | P3 shadow mode + contract tests before any tenant volume; refuse classes that fail | capability handshake failures, flaky attach |
| Attachment-phase gap makes CHV report something it cannot prove | Certain | Medium | Record `Prepared/observed=false`; request upstream amendment | reviewers asking "is it active?" |
| Two storage writers during migration | Medium | Critical | Owner column + router refusal + no class inference | any code path selecting a backend by class alone |
| Multi-host dispatch ships but one call site stays on Unix | Medium | High | Single resolver; grep-level CI check forbidding `UnixStream::connect` outside the transport module | a new `resolve_agent_socket` caller |
| Console token compromise across fleet | Medium | High | Asymmetric audience-scoped tokens | shared `jwt_secret` in any node config |
| Fabric slips, MH-B never lands | Medium | High | Sequence MH-A independently; state MH-B honestly as gated on P5 | P5 gate missed twice |
| Cert/SAN mistakes make enrollment fragile | High | Medium | Startup validation of advertised identity; L3 negative tests | handshake errors only in staging |
| Scope creep into ceph/nearline | High | High | Explicit non-goals; each class needs its own plan | "while we're here" PRs |
| Node-only packaging breaks tested install | Medium | Medium | Package-split smoke leg in CI; keep meta-package byte-compatible | install-sh regression |

---

## 12. Effort and critical path

Rough order-of-magnitude, one experienced engineer per track:

| Phase | Weeks | Can run in parallel with |
|---|---|---|
| P0 | 1–2 | — |
| P1 | 5–7 | P0 tail |
| P2 | 4–5 | P5 |
| P3 | 4–5 | P2, P5 |
| P4 | 5–7 | P5 |
| P5 | 8–12 | P2, P3, P4 |
| P6 | 6–8 | — |
| P7 | 3–4 | — |

- **MH-A (remote management)** ≈ 10–14 weeks from P0.
- **VS-A (native Volvisor local)** ≈ 15–21 weeks from P0 (P0 → P1 → P3 → P4).
- **MH-B (tenant L2)** is gated by P5 and roughly 8–12 weeks of fabric work.
- **Full supported multi-host + Volvisor** ≈ 30–36 weeks sequential, ~24–28
  with a second track on P5.

These are planning numbers, not commitments. The gate list, not the
calendar, decides progression.

---

## 13. Sequencing summary

```
P0 decisions
   │
   ├──► P1 remote dispatch ──► P2 node lifecycle ──► (MH-A)
   │                                   │
   │                                   └──► P3 seam/shadow ──► P4 native-local ──► (VS-A)
   │                                                                     │
   └──► P5 fabric (parallel, ADR-021) ──────────────────────────────────► (MH-B)
                                                                         │
                                              P6 qualification ◄─────────┘
                                                     │
                                                     ▼
                                              P7 stord retirement
```

Ordering rules:

1. Transport before storage. A Volvisor integration that cannot be dispatched
   to a remote node is still a single-host product.
2. Owner column before the first Volvisor write. Never a dual-writer window.
3. Capability before journaling. Unsupported operations fail terminally
   before they are journaled or dispatched.
4. Fabric before claiming MH-B. Do not call remote management "multi-host"
   in user-facing docs.
5. Retirement after enumeration. stord leaves only when every remaining
   volume has an explicit owner and a documented path.

---

## 14. Open questions (resolve in P0)

Resolved by this draft:

- **Split node identity** — D-MH2/D-MH3: split `node-client` / `node-server`,
  plus a dedicated control-plane client CA (ADR-029 §3).
- **Agent gRPC port** — D-MH8: `9443/tcp`. It is unused in the §3.1 listener
  table; the only existing `9443` references are config test fixtures
  (`crates/chv-config/src/lib.rs:2172`).

Still open:

1. **Management network assumption.** Is there a dedicated management
   network/VLAN, or must the agent listener coexist with tenant traffic?
   This changes the exposure rule and the firewall model.
2. **`project_id` source.** VM tenant id, or a new first-class project
   concept? Recommendation: VM tenant id in P4, first-class project later.
   Contract v1 §11.3 encodes the recommendation; ratification is still
   required.
3. **`seed_from` for Volvisor-owned volumes.** Refuse in P4 (recommended), or
   build an explicit pre-seed job? Contract v1 §10 encodes the refusal;
   ratification is still required.
4. **Volvisor packaging.** Build from the pinned SHA into `chv-node`, or a
   separate `chv-volvisord` package with its own upgrade cadence?
5. **LVM pool claiming.** Which operator workflow claims devices
   (`/v2/admin/devices/claim`) and who holds the claim token? This is
   destructive and needs a written procedure before P4.
6. **Console transport.** BFF → agent console over the management network
   with mTLS (recommended), or tunnel through the gRPC stream later?

---

## Appendix A — File-level index

**New**

- `docs/specs/adr/029-multi-host-dispatch.md`
- `docs/specs/contracts/chv-volume-backend-v1.md`
- `crates/chv-controlplane-service/src/node_endpoint.rs`
- `crates/chv-storage-api/` (trait + router) or a module in `chv-hypervisor-api`
- `crates/chv-storage-volvisor/` (client, types, capabilities, errors)
- `cmd/chv-controlplane/migrations/0063_node_endpoints.sql`
- `cmd/chv-controlplane/migrations/0064_volume_storage_owner.sql`
- packaging: `chv-node-common`, meta-package, `volvisord.service`, config examples
- `scripts/integration/qual/multihost-*.sh`, `volvisor-*.sh`

**Modified**

- `crates/chv-controlplane-service/src/{node_client,node_client_pool,migration,orchestrator,overlay,lifecycle,migration_reaper,enrollment}.rs`
- `cmd/chv-controlplane/src/bootstrap.rs`
- `crates/chv-agent-core/src/{agent_server,resources,reconcile,stord_backend,daemon_clients}.rs`
- `crates/chv-agent-runtime-ch/src/core_runtime.rs`
- `crates/chv-hypervisor-api/src/resources.rs`
- `crates/cellhv-core-types/src/lib.rs`
- `crates/chv-config/src/lib.rs`
- `proto/controlplane/control-plane-node.proto` (additive)
- `crates/chv-webui-bff/src/handlers/vms.rs`
- `scripts/install.sh`
- `packaging/nfpm/*`, `packaging/systemd/*`, `packaging/config/*`
- `docs/DEPLOYMENT-ARCHITECTURE.md`, `docs/ARCHITECTURE.md` (where it asserts single-host)
- `docs/plans/2026-10-13-volvisor-storage-adoption.md` (superseded work packages)

## Appendix B — Acceptance test names

Multi-host:

- `multihost_enroll_two_hosts_over_mtls`
- `cp_dispatches_to_remote_agent_not_local_socket`
- `node_cert_cannot_authenticate_to_peer_agent`
- `server_cn_mismatch_is_rejected`
- `endpoint_generation_invalidates_pool`
- `plaintext_and_wildcard_bind_are_refused`
- `console_token_audience_is_node_scoped`
- `placement_requires_node_id_when_multi_node`
- `node_removal_reaps_inflight_operations`

Volvisor:

- `capability_handshake_refuses_unknown_class`
- `create_is_idempotent_on_operation_id`
- `attach_replay_after_crash_does_not_duplicate`
- `detach_with_stale_generation_conflicts`
- `owner_routing_refuses_cross_backend_open`
- `snapshot_is_refused_for_volvisor_owned_volume`
- `native_local_vm_is_migration_ineligible`
- `prepared_is_never_reported_as_active`
- `token_is_redacted_in_logs_and_errors`
- `legacy_suite_green_with_router_enabled`
