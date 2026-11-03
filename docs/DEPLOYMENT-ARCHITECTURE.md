# CHV Deployment Architecture — Single Source of Truth

> **Status (2026-10-03, branch `docs/deployment-architecture`, base `1fbb2b04`):**
> this document is the authoritative reference for **what deploys, how the
> components connect, and which use cases are supported at which confidence
> tier**. It does not replace the operator how-to ([DEPLOYMENT.md](DEPLOYMENT.md)),
> the package contract ([release/package-contract.md](release/package-contract.md)),
> release engineering ([release/PIPELINE.md](release/PIPELINE.md)), or day-2
> operations ([OPERATIONS.md](OPERATIONS.md)); those documents link here for
> architecture and capability tier, and keep their own roles.
> Capability claims in this document are capped at **KVM-VERIFIED (single
> host)** per the frozen release declaration
> ([evidence/production-readiness/v0.3.0-rc1/00-execution-declaration.md](evidence/production-readiness/v0.3.0-rc1/00-execution-declaration.md)
> §5). MULTI-HOST-VERIFIED and FIELD-QUALIFIED are **unprovable** on the
> qualification infrastructure and are reported as unproven, never claimed.
> Two campaign prompts are pending and will extend this document without
> restructuring it: Prompt 05 (release-candidate notes wording) and
> Prompt 06 (reference deployment / field qualification). Where their results
> are expected, this document says **pending** and does not pre-claim.

---

## 1. Purpose and scope

### 1.1 What this document owns

| Question | Owner |
|---|---|
| What are the deployable components and their responsibilities? | This document (§2) |
| Which interfaces, ports, and sockets exist, and which may be exposed? | This document (§3) |
| How is a deployment provisioned (identity, keys, secrets)? | This document (§4) |
| Which deployment topologies and use cases are supported, and at what tier? | This document (§5–§7) |
| What are the upgrade and lifecycle paths? | This document (§6) |
| Which design decisions remain open? | This document (§8) |
| What is explicitly not supported? | This document (§9–§10) |
| Which document owns which kind of truth? | This document (§11) |

### 1.2 Capability tier vocabulary

Every capability claim in this document carries one of these labels. The
labels map to the campaign vocabulary in
[prompts/production-readiness/README.md](prompts/production-readiness/README.md)
(CODED / CI-VERIFIED / KVM-VERIFIED / MULTI-HOST-VERIFIED / FIELD-QUALIFIED /
RELEASED), narrowed to what is provable today.

| Label | Meaning | Typical evidence |
|---|---|---|
| **[QUALIFIED — KVM-VERIFIED]** | Behavior proven on the real KVM host, in-stack, with forbidden-outcome assertions. Single host only. | Prompt-04 milestone evidence: [m4.1](evidence/production-readiness/v0.3.0-rc1/04-real-host-qualification/m4.1-harness.md)–[m4.9](evidence/production-readiness/v0.3.0-rc1/04-real-host-qualification/m4.9-status.md) |
| **[CI-VERIFIED]** | Repository CI proves the contract at non-privileged tiers (package build, smoke, and lifecycle tests). | [release/PIPELINE.md](release/PIPELINE.md); `scripts/package/` tests in `release.yml` / `package-nightly.yml` |
| **[CONTAINER-VERIFIED]** | A clean-container leg proves the packaging/systemd/installer contract, not host-level behavior. A subset of CI-VERIFIED evidence, called out where the distinction matters. | [m4.2 install-sh leg](evidence/production-readiness/v0.3.0-rc1/04-real-host-qualification/m4.2-clean-install.md) |
| **[CODE-SUPPORTED, UNQUALIFIED]** | Code exists and is wired, but no qualified path exercises it. | Code references in this document |
| **[DESIGN-ONLY]** | An accepted ADR defines the design; the capability is not qualified (and may not be implemented end-to-end). | [specs/adr/](specs/adr/) |
| **[UNSUPPORTED/ABSENT]** | No support. Stated as an explicit non-claim. | Declaration §3/§6; this document |

**Cap:** the qualification infrastructure is a single shared host (16 vCPU /
31 GiB, nested KVM). Per declaration §5, no capability may be labeled above
KVM-VERIFIED, and multi-host behavior is reported as unproven.

### 1.3 Terminology

- **CHV** names the platform only. The VMM is **Cloud Hypervisor** (binary
  `cloud-hypervisor`), never "CHV" or "CH".
- **node** is the managed host in prose. The UI label **"Hosts"** and the
  Designer YAML key **`servers`** are synonyms for node; the mapping is noted
  here once and not repeated.
- **VM** is used in prose. The UI label **"Instances"** is a synonym.
- Daemon names: `chv-controlplane`, `chv-agent`, `chv-stord`, `chv-nwd`,
  `chvctl` (this is a developer-facing document — full names at first
  mention, bare `stord`/`nwd` may follow, per the documentation standard).
  **chv-agent (CellHV Core)** is the agent runtime; the CellHV Core
  lifecycle authority is the single-writer authority it hosts.
- **control plane** is the noun; **control-plane** is the adjective. This
  document abbreviates it as **CP** on later reference.
- Inter-process local endpoints are **Unix sockets**.
- **O3K** names the sibling Kubedo edge-fabric project whose provider code
  CHV consumes for the fabric design.
- The web API layer behind nginx is the **backend-for-frontend (BFF)**. The
  BFF is embedded in `chv-controlplane`; there is no separate BFF binary.
- Versions are written as `<version>` except where this document states a
  pinned fact (for example the qualified Cloud Hypervisor pin in §7).

## 2. Component inventory

### 2.1 CHV daemons and tools

| Component | Role | Key details | Tier of the role statement |
|---|---|---|---|
| `chv-controlplane` | Single control-plane daemon | Orchestrator (desired-state operations), node enrollment, CA issuer, SQLite persistence. Embeds the backend-for-frontend (BFF) on its HTTP listener. gRPC on `127.0.0.1:8443` (mTLS), HTTP/BFF on `127.0.0.1:8080`. | [QUALIFIED — KVM-VERIFIED] ([m4.1](evidence/production-readiness/v0.3.0-rc1/04-real-host-qualification/m4.1-harness.md), [m4.3](evidence/production-readiness/v0.3.0-rc1/04-real-host-qualification/m4.3-lifecycle.md)) |
| `chv-agent` | Node lifecycle agent | VM lifecycle; spawns, supervises, and adopts `cloud-hypervisor`; serial-console WebSocket on `127.0.0.1:8444`; metrics on `127.0.0.1:9901` (qualification config uses `127.0.0.1:9100`). Hosts the CellHV Core single-writer lifecycle authority (`authority_mode = "core-managed"` is the qualified default). Supervises `chv-stord` and `chv-nwd` as a fallback when no daemon is already serving their socket (for example, when their units are absent). | [QUALIFIED — KVM-VERIFIED] (as above) |
| `chv-stord` | Storage daemon | Volumes, pools, images, snapshots. Backends: local file and LVM (qualified, §7); Ceph RBD and iSCSI are coded but unqualified. Opt-in mTLS migration TCP listener, disabled by default. | Storage paths: [QUALIFIED — KVM-VERIFIED] within profiles ([m4.5](evidence/production-readiness/v0.3.0-rc1/04-real-host-qualification/m4.5-storage.md)); Ceph/iSCSI: [CODE-SUPPORTED, UNQUALIFIED] |
| `chv-nwd` | Network daemon | Bridges, taps, per-network namespaces, nftables policy, dnsmasq DHCP/DNS. Stretched-L2 VXLAN + WireGuard fabric via `fabric-linux` v0.1.5 (`Cargo.toml` pin) — disabled and fails closed in the qualification configuration. | Local-bridge topology + host-safety gate: [QUALIFIED — KVM-VERIFIED] ([m4.4](evidence/production-readiness/v0.3.0-rc1/04-real-host-qualification/m4.4-network.md)); fabric: [DESIGN-ONLY] ([ADR-021](specs/adr/021-stretched-l2-vxlan-her-wireguard-fabric.md)) |
| `chvctl` | CLI client | Talks to the BFF on `:8080`. Packaged standalone; not part of `chv-node`. | [QUALIFIED — KVM-VERIFIED] as the campaign's driver surface (all prompt-04 scenarios drive chvctl/BFF/API) |

### 2.2 Web UI and edge

| Component | Role | Key details | Tier |
|---|---|---|---|
| SvelteKit UI statics | Browser application | Built to static files. `scripts/install.sh` serves them from `/opt/chv/ui` through **nginx** on `:80` — the only component that listens on a non-loopback address. Since #447 (D3 target) the control plane binary itself can serve the tree: opt-in `[webui]` config section (`chv-config`), disabled by default, prefix-guarded static fallback with SPA + cache headers on the HTTP listener (`crates/chv-controlplane-service/src/api/router.rs`); install.sh enables it and its nginx edge is proxy-only. The `.deb`/`.rpm` ships the same tree at `/usr/share/chv/ui` plus the proxy-only example configuration under `/usr/share/chv/examples/` — no web server is installed or enabled (see §5 UC-3 and decision D3). | UI serving via install.sh: [CONTAINER-VERIFIED] — the packaging/systemd contract, proven in a clean container by the [m4.2 install-sh leg](evidence/production-readiness/v0.3.0-rc1/04-real-host-qualification/m4.2-clean-install.md) (the pre-#447 nginx-statics shape; the proxy-only shape re-qualification is the #447 container leg). UI serving from the packaged tree via the example conf / `[webui]`: [CONTAINER-VERIFIED] — the #549 container package-smoke leg (`scripts/package/smoke-webui.sh`, built from the `.deb`s and wired into `package-pr.yml`, 2026-10-08) starts the binary with `[webui] enabled = true` behind the packaged proxy-only edge and asserts shell, cache headers, login round-trip, BFF API, and the `/ws/` console proxy. UI behavior end-to-end: see §9 (#355). |
| nginx | Edge reverse proxy | Terminates HTTP `:80`; proxies `/api/` and `/v1/` to the BFF `:8080`; proxies `/ws/vms/{node_id}/…` to the agent console `:8444`. Installed and configured by `scripts/install.sh`; the packages ship an example configuration only (D3 interim). | [CONTAINER-VERIFIED] — the same m4.2 install-sh contract leg |

### 2.3 External dependencies

| Dependency | Version fact | Notes |
|---|---|---|
| Cloud Hypervisor | Qualified pin **v53.0** (moved from v43.0 by the #448 re-qualification campaign — decision D6, option (b); `scripts/integration/kvm-smoke.sh` default `v53.0`) | `scripts/install.sh` downloads the same **v53.0** and verifies the qualified sha256 digests before install (campaign evidence: [v53.0 tree](evidence/vmm-requalification/v53.0/README.md)). |
| rust-hypervisor-firmware | 0.5.0 (qualification firmware; `scripts/install.sh` `FIRMWARE_URL`) | `scripts/install.sh` uses a pre-placed `/root/CLOUDHV.fd` when present; otherwise it downloads and sha256-verifies the pinned 0.5.0 build to `/var/lib/chv/hypervisor-fw`. The former commented-out download is fixed in this branch — a clean host now gets firmware. |
| fabric-linux | v0.1.5 (`Cargo.toml` git tag pin) | Consumed by `chv-nwd` for the (design-only) fabric. |
| Host OS | Linux x86_64; `.deb` (Debian/Ubuntu) + `.rpm` | Qualified on Ubuntu noble amd64; `.rpm` built but untested in prompt-04 (declaration §3, m4.2). |

### 2.4 Package set (nfpm; `.deb` and `.rpm`)

Source of truth for contents: the nfpm configurations
([`../packaging/nfpm/chv-controlplane.yaml`](../packaging/nfpm/chv-controlplane.yaml),
[`../packaging/nfpm/chv-node.yaml`](../packaging/nfpm/chv-node.yaml),
[`../packaging/nfpm/chvctl.yaml`](../packaging/nfpm/chvctl.yaml)). Where
[release/package-contract.md](release/package-contract.md) disagrees with the
nfpm configurations, the nfpm configurations win; one such contradiction is
recorded below.

| Package | Contents | Dependencies | Notes |
|---|---|---|---|
| `chv-controlplane` | Binary; UI tree at `/usr/share/chv/ui`; example reverse-proxy conf at `/usr/share/chv/examples/` (D3 interim, documentation only); migrations; `chv-controlplane.service`; `/etc/chv/controlplane.toml` (`config\|noreplace`) | `libssl3` (deb) / `openssl-libs` (rpm) | — |
| `chv-node` | `chv-agent`, `chv-stord`, `chv-nwd` binaries; 3 units; tmpfiles.d entry; 4 configs (`agent.toml`, `stord.toml`, `nwd.toml`, plus the reference-only `chv.yaml`) | **hard-depends on `chv-controlplane`**; deb additionally hard-depends `wireguard-tools` (rpm: `recommends`) | The hard dependency installs a control plane on every node host — flagged as decision D2. `chvctl` is **not** included (nfpm is truth; `package-contract.md` states both "pulled in by `chv-node`" and "not included" — a self-contradiction resolved in favor of nfpm). |
| `chvctl` | Binary only | `libssl3` / `openssl-libs` | Install separately on any management machine. |

Packaged systemd units ([`../packaging/systemd/`](../packaging/systemd/)):

| Unit | Key facts |
|---|---|
| `chv-controlplane.service` | `User=chv`; loads `-/etc/chv/encryption.env` via `EnvironmentFile`; `ProtectSystem=strict`. |
| `chv-agent.service` | `User=chv`; `After=`/`Wants=` the other three units; `SupplementaryGroups=kvm`; `TimeoutStopSec=75` so SIGTERM drain (60 s budget) always completes before SIGKILL. |
| `chv-stord.service` | `User=chv` by design (socket is 0600 with `chv-agent` as sole client; `cloud-hypervisor` spawned as `chv` must read/write volume files). Storage dirs `chv:chv-stord 0770`; the `chv-stord` group is the documented future isolation seam. |
| `chv-nwd.service` | `User=chv`; `AmbientCapabilities`/`CapabilityBoundingSet` = `CAP_NET_ADMIN CAP_NET_RAW CAP_SYS_ADMIN`; `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK` as the compensating control. The `CAP_SYS_ADMIN` grant (needed for `ip netns add`) is a documented, deliberately accepted tradeoff — see the unit file and issue #328. |

Package scripts ([`../packaging/scripts/`](../packaging/scripts/)):

| Script | Behavior |
|---|---|
| `postinstall.sh` | Mints `/etc/chv/encryption.env` once from `/dev/urandom` (never regenerated — regeneration would make existing encrypted credentials unrecoverable). Creates `chv` and `chv-stord` users and group memberships (`chv`→`kvm`, `chv-stord`→`disk`, `chv-stord`→`chv`). Creates state dirs; storage dirs `chv:chv-stord 0770`. Explicitly does **not** create bridges, pools, VMs, or firewall rules. Reloads systemd; does **not** enable or start services. |
| `preremove.sh` | Stops the four services on remove/purge; skips stopping on upgrade. |
| `postremove.sh` | Preserves `/var/lib/chv`, `/etc/chv`, `/var/log/chv`. Destructive cleanup requires explicit operator action. |

## 3. Interface and port contract

This table is the contract. "Loopback" means the endpoint binds `127.0.0.1`
and must not be exposed. Changes to this table are architecture changes and
require review against the declaration.

### 3.1 TCP listeners

| Listener | Bind (qualified config) | Protocol | Consumers | Exposure rule | Tier |
|---|---|---|---|---|---|
| Control-plane gRPC | `127.0.0.1:8443` | gRPC, mTLS (server cert + client CA; agents present CA-signed certs) | `chv-agent` (enrollment, reports), loopback tooling | Do not expose. | [QUALIFIED — KVM-VERIFIED] ([m4.1](evidence/production-readiness/v0.3.0-rc1/04-real-host-qualification/m4.1-harness.md)) |
| Control-plane HTTP / BFF | `127.0.0.1:8080` | HTTP (BFF `/v1/…`, admin `/api/v1/…`, health) | nginx, `chvctl` | Do not expose directly; nginx is the edge. | [QUALIFIED — KVM-VERIFIED] |
| Agent serial console | `127.0.0.1:8444` | WebSocket (`/vms/{vm_id}/console?token=…`) | nginx (`/ws/vms/…` proxy) | **Must not be exposed.** Default from `chv-config` (`crates/chv-config/src/lib.rs:642-643`). | [QUALIFIED — KVM-VERIFIED] |
| Agent metrics | `127.0.0.1:9100` (qualification config, `deploy.sh`); packaged/install.sh default `127.0.0.1:9901`; code fallback when `metrics_bind` is unset: `127.0.0.1:9100` (loopback) | HTTP `/metrics` | Prometheus-style scrapers | Loopback in the qualified config. | [QUALIFIED — KVM-VERIFIED] |
| nginx edge | `:80` (all interfaces) | HTTP; UI statics; proxies `/api/`, `/v1/` → `:8080`, `/ws/vms/{node_id}/…` → `:8444` | Browsers | The only externally listening component. TLS at the edge is a recommendation, not a default — see decision D7. | [CONTAINER-VERIFIED] — the m4.2 install-sh contract leg |
| stord migration receiver | disabled by default; example `127.0.0.1:50052` | gRPC, mTLS, mandatory client certs; no plaintext mode | peer `chv-stord` | Opt-in only. With any receiver field set while `enabled = false`, the daemon refuses to start (fail-closed). | [QUALIFIED — KVM-VERIFIED] between two stord instances on one host ([m4.6](evidence/production-readiness/v0.3.0-rc1/04-real-host-qualification/m4.6-migration.md)) |
| Fabric WireGuard | `65001/udp` (default; configurable) | WireGuard underlay mesh | peer `chv-nwd` | Underlay network only; peer-authenticated. | [DESIGN-ONLY] — [ADR-021](specs/adr/021-stretched-l2-vxlan-her-wireguard-fabric.md); fails closed today. |
| Fabric VXLAN | `4789/udp` | kernel VXLAN overlay | peer `chv-nwd` | Underlay network only; peered to ADR-021's WireGuard fabric. | [DESIGN-ONLY] — ADR-021. |
| Control-plane gRPC (multi-host) | operator-declared management bind (proposed default `:8443`) | gRPC, mTLS (server cert carries the advertised management identity; client CA = node CA) | remote `chv-agent` (enrollment, reports) | Management network only; never public. **[ACCEPTED — ADR-029; NOT IMPLEMENTED]** | [DESIGN-ONLY] |
| Agent gRPC (multi-host) | operator-declared management bind + `advertise_addr` (proposed default `:9443`) | gRPC, mTLS (server cert `CN=<node_id>`; client CA = dedicated control-plane client CA) | `chv-controlplane` (dispatch) | Management network only; requires explicit opt-in, `advertise_addr`, and a non-wildcard bind. **[ACCEPTED — ADR-029; NOT IMPLEMENTED]** | [DESIGN-ONLY] |

The two multi-host rows are accepted as design by
[ADR-029](specs/adr/029-multi-host-dispatch.md) (ratified 2026-11-03) and are
**not implemented**. Until the P1 gate passes, only the loopback listeners
above exist.

### 3.2 Unix sockets

| Socket | Owner | Clients | Notes | Tier |
|---|---|---|---|---|
| `/run/chv/agent/api.sock` | `chv-agent` | `chv-controlplane` (dispatch) | Legacy gRPC API surface; the agent sets the socket mode after bind. | [QUALIFIED — KVM-VERIFIED] |
| `/run/chv/core/core-v1.sock` | `chv-agent` (CellHV Core) | Core clients | Directory `0700`. Native Core API. | [QUALIFIED — KVM-VERIFIED] |
| `/run/chv/stord/api.sock` | `chv-stord` | `chv-agent` | Mode 0600; sole client is `chv-agent`. | [QUALIFIED — KVM-VERIFIED] |
| `/run/chv/nwd/api.sock` | `chv-nwd` | `chv-agent` | Mode 0600; sole client is `chv-agent`. | [QUALIFIED — KVM-VERIFIED] |

### 3.3 The control-plane → agent dispatch constraint (decisive)

`chv-controlplane` reaches agents by resolving the configured
`agent_socket_pattern` to a **filesystem path** and connecting over a **Unix
socket**:

- `resolve_agent_socket` returns a `PathBuf`
  (`crates/chv-controlplane-service/src/migration.rs:1654-1665`); node ids are
  validated as single safe path components.
- The orchestrator dispatches through the node client pool with that path
  (`crates/chv-controlplane-service/src/orchestrator.rs:768-771` and
  `:1605-1609`), and the client connects with `UnixStream::connect`
  (`crates/chv-controlplane-service/src/node_client.rs:204-211`).
- The agent serves gRPC from a `UnixListener`
  (`crates/chv-agent-core/src/agent_server.rs:181-188`). **The agent has no TCP
  gRPC listener.** Its only TCP listeners are the console (`:8444`) and
  metrics (`metrics_bind`: packaged/installer `:9901`, qualification and
  unset-fallback `:9100`) bindings.

Consequence: **a control plane cannot reach an agent on another host as
coded.** This is the decisive multi-host constraint; see UC-6 and decision D1.

#### 3.3.1 Accepted multi-host dispatch design (ADR-029 — not implemented)

[ADR-029](specs/adr/029-multi-host-dispatch.md) specifies replacing the path
resolution with a `NodeEndpoint` resolver (`Unix { path }` |
`Tcp { host, port, server_name }`) backed by node-advertised state, plus the
optional agent TCP listener listed in §3.1. None of this is implemented.

Until ADR-029's P1 gate passes, every statement in §3.3 remains true, UC-6
remains [UNSUPPORTED/ABSENT], and no multi-host claim may be made.

### 3.4 Certificate and identity model

| Item | Value | Evidence |
|---|---|---|
| CA | `/etc/chv/certs/ca.crt` + `ca.key`, held by the control plane | `docs/examples/controlplane.toml` `[tls]`; [m4.1](evidence/production-readiness/v0.3.0-rc1/04-real-host-qualification/m4.1-harness.md) |
| Node certificates | Issued by the control plane at enrollment; subject `CN = <node_id>`; serial recorded in the nodes table | `crates/chv-controlplane-service/src/enrollment.rs`; `peer_identity.rs` |
| Enrollment client cert | Pre-placed by the operator/installer at `/run/chv/agent/agent.crt` (+ key); used only for the EnrollNode handshake | `scripts/install.sh:1004-1019` (issued), `:1508-1513` (installed); m4.1 harness invariants |
| Steady-state agent TLS | `/run/chv/agent/agent.crt`, `agent.key` (control-plane-issued material replaces the enrollment cert) | `docs/examples/agent.toml` |
| Bootstrap token | One-time; minted/rotated over loopback `/internal/bootstrap-token` | `scripts/install.sh:1283-1284` (mint), `:1447-1449` (API seed); m4.1 asserts single consumption |
| Peer identity enforcement | stord↔stord migration peers authenticate by certificate identity; negative cases fail closed with zero bytes leaving the source | [m4.6](evidence/production-readiness/v0.3.0-rc1/04-real-host-qualification/m4.6-migration.md) |

**Accepted design (ADR-029, not implemented):** split node identities
(`node-client` / `node-server`), a dedicated control-plane client CA for
CP→agent dispatch, explicit EKUs, and an IP SAN for `advertise_addr`. The
table above describes the shipped model and remains accurate until the P1
gate is implemented.

## 4. Security-provisioning model

Two install paths provision differently. Both end in the same qualified
runtime posture.

| Path | Provisioning | Result |
|---|---|---|
| `scripts/install.sh` (all-in-one) | Generates CA + server/client certs, writes configs, mints the encryption key and bootstrap token, seeds the admin user (forced password change), base image, default network, and `dev-vm-1` (skippable with `INSTALL_CHV_NO_SEED=1`), installs and starts nginx + the four units. Root-only (`scripts/install.sh:168`). | Working single-host deployment. [QUALIFIED — KVM-VERIFIED] topology; the install path itself carries the m4.2 install-sh contract leg (clean container, ~30 assertions + 7 host-side checks, errors=0 warnings=0). |
| Packages on a clean host | Layout only. The control plane **fails closed** at startup without operator provisioning: CA + server/client certs, a real `jwt_secret`, and admin seeding (typed error, restart loop, no insecure fallback). | [QUALIFIED — KVM-VERIFIED] as the fail-closed boundary ([m4.2](evidence/production-readiness/v0.3.0-rc1/04-real-host-qualification/m4.2-clean-install.md) §3). |

Additional provisioning facts:

- `CHV_ALLOW_INSECURE` is refused by production binaries; the qualification
  harness asserts its absence (m4.1).
- The credential-encryption key (`CHV_ENCRYPTION_KEY`) is minted once by
  postinstall and never regenerated. The fallback that couples it to
  `CHV_JWT_SECRET` is open issue **#336**.
- Authority mode: every shipped configuration surface selects
  `authority_mode = "core-managed"` (single durable CellHV Core authority).
  The reference-only `packaging/config/chv.yaml` now matches (`core-managed`,
  fixed via #424); no daemon consumes it for authority selection (§10).
- The optional legacy `chvbr0` host bridge/NAT bootstrap is opt-out
  (`INSTALL_CHV_NO_BRIDGE=1`). Qualified networks are created via the API and
  managed by `chv-nwd` as `br-<net_id>`; the pre-created `chvbr0` is a dev
  convenience.
- `--wipe` tears down the deployment **but preserves persistent agent
  authority state by design** (`scripts/install.sh:1651-1691`).

## 5. Deployment topologies and use cases

Each use case states: status tier, what it includes, what it excludes, and
evidence. Pending Prompt-05/Prompt-06 results slot into this section.

### UC-1 — Single-host reference deployment via `scripts/install.sh`

**Status: [QUALIFIED — KVM-VERIFIED] (topology); install-path contract
evidenced at container tier.**

| Aspect | Statement |
|---|---|
| Topology | One host runs `chv-controlplane`, `chv-agent`, `chv-stord`, `chv-nwd`, nginx. All internal listeners loopback; nginx is the edge. |
| Includes | Everything in §2; core-managed authority; enrollment with real mTLS; seeded admin (forced password change), base image, default network, `dev-vm-1`. |
| Excludes | Multi-host anything (UC-6), fabric (UC-7), backup/restore as DR (declaration §3), HA (UC-9). |
| Evidence | Declaration §3/§5; prompt-04 milestones ran this topology on the qualification host (m4.3–m4.8). The install.sh path itself has permanent coverage via `scripts/integration/qual/install-sh-leg.sh` (m4.2 §"New harness leg"): clean container, all four units active, contract assertions — with `NO_SEED`/`NO_BRIDGE` set, so it proves the packaging/systemd contract, not VM end-to-end behavior. |
| Disclosed conflicts | (a) Repository identity — **resolved 2026-10-30**: `github.com/kubedoio/chv` now returns HTTP 301 to the canonical `github.com/o3kio/chv` (which matches `git remote` and the installer defaults); old-name references are historical only, with one load-bearing exception: the pinned rust-hypervisor-firmware artifact is still published from `github.com/kubedoio/rust-hypervisor-firmware` (the `o3kio` org has no such repository), so `scripts/install.sh` downloads firmware from that old-org URL. The `get.cellhv.com` hosting surface is **not stood up** (NXDOMAIN) and is optional/future (§9). (b) Firmware — **resolved in this branch**: `scripts/install.sh` uses `/root/CLOUDHV.fd` when present, otherwise downloads the pinned rust-hypervisor-firmware 0.5.0 to `/var/lib/chv/hypervisor-fw`; a clean host now has firmware. (The former installer/qualification VMM version conflict was resolved by moving the pin: `scripts/install.sh` now installs the qualified **v53.0** — decision D6, option (b).) |

### UC-2 — Package-based clean-host install

**Status: [QUALIFIED — KVM-VERIFIED] (packages provide layout; control plane
fails closed until operator provisioning).**

| Aspect | Statement |
|---|---|
| Includes | The `.deb`/`.rpm` static contract (users, groups, modes, units, conffiles, migrations, tmpfiles), boot behavior with packaged configs, and a real-host package smoke against `/dev/kvm`. |
| Excludes | A working deployment from packages alone. The operator must provision CA + certs, a real `jwt_secret`, and admin seeding (documented in [PACKAGING.md](PACKAGING.md) and m4.2 §3). |
| Evidence | [m4.2](evidence/production-readiness/v0.3.0-rc1/04-real-host-qualification/m4.2-clean-install.md) Legs A/B/C + fix rounds; residue after removal is exactly the deb contract (conffiles, users, data dirs). |
| Notes | Ubuntu noble amd64 qualified; `.rpm` built but untested in prompt-04. No apt/dnf repository yet; no package signing yet (verify checksums out-of-band) — [PACKAGING.md](PACKAGING.md) "Known Gaps". |

### UC-3 — Headless / package-only deployment

**Status: [CONTAINER-VERIFIED].**

The packages install all daemons (`chvctl` is a separate package, §2.4)
but no web server. The UI tree ships at `/usr/share/chv/ui`. Under the
D3 interim resolution, `chv-controlplane` also ships an example nginx
configuration (`/usr/share/chv/examples/chv-example.conf`)
and [DEPLOYMENT.md](DEPLOYMENT.md) "Serving the Web UI in package mode"
documents how an operator serves the tree with it. The operator owns
the edge, including TLS (decision D7). A headless
operator can still drive the platform with `chvctl` against the BFF.
Decision **D3** is resolved as interim (b), target (d) — see §8.

[2026-10-07, #447:] the D3 target (d) is now implemented — the control
plane binary serves the tree itself through the opt-in `[webui]`
config section (disabled by default, fail-closed), and the example
conf is a proxy-only edge pointing at the control plane's HTTP
listener. The `/ws/`
serial-console proxy stays an edge concern (the binary does not proxy
`/ws/` — documented, per the #447 decision).

[2026-10-08, #549:] the container package-smoke leg ran and passed —
`scripts/package/smoke-webui.sh` builds the converged image from the
`.deb`s (the packaged proxy-only edge + `[webui] enabled` + the
packaged UI tree + the agent console behind `/ws/`) and asserts the
single-listener shape end to end; it is wired into `package-pr.yml`.
The label moved [CODE-SUPPORTED, UNQUALIFIED] → [CONTAINER-VERIFIED]
per the tier-path convention. The scope is the packaging/serving
contract in a clean container — not guest-level UI behavior (§9, #355)
and not a published container image (none exists; the leg builds a
local one).

### UC-4 — Development install

**Status: [CODE-SUPPORTED, UNQUALIFIED] (development surface).**

`make dev-install` → `scripts/dev-install.sh` → builds a release tarball and
runs `scripts/install.sh` with `INSTALL_CHV_TARBALL_PATH` pointing at the
local tarball (skipping the GitHub download). Dev defaults (bridge iface,
name, CIDR) and seeds are documented in the script header. Used for
development; not a qualified deployment path.

### UC-5 — Qualification harness topology

**Status: [QUALIFIED — KVM-VERIFIED] (this is the topology that produced the
evidence).**

`scripts/integration/qual/deploy.sh` deploys the candidate as plain processes
into throwaway directories with real mTLS, core-managed authority, and
teardown that asserts forbidden residue (no `cloud-hypervisor` processes, no
new links, no new nft tables). Every prompt-04 milestone cites these scripts
at the exact SHA. Treat this as the reproducibility reference for any future
re-qualification; it is not an operator install path.

### UC-6 — Multi-host (control plane + remote nodes)

**Status: [UNSUPPORTED/ABSENT] as a qualified capability — and
architecturally blocked as coded. Single-host is the declared posture for
now (decision D1, option (b)). ADR-029 is accepted as design but not
implemented, so it does not change this status.**

| Aspect | Statement |
|---|---|
| The blocker | The CP→agent dispatch resolves to a Unix-socket filesystem path (§3.3). The agent has no TCP gRPC listener. A remote node cannot be reached, by construction. |
| The packaging blocker | `chv-node` hard-depends on `chv-controlplane`, so a "node-only" host installs a control plane it does not need (decision D2). |
| What IS code-supported | The enrollment protocol (cert issue, `CN = <node_id>`, bootstrap token, node rows) works over the mTLS gRPC surface; nothing in the protocol is single-host-specific. |
| What IS documented | Multi-node serial-console routing via nginx (`agent_ws_address` direct mode, or the proxied `/ws/vms/{node_id}` map) — [DEPLOYMENT.md](DEPLOYMENT.md) "Multi-Node WebSocket Console Routing". |
| Declaration | MULTI-HOST-VERIFIED is **unprovable** on this infrastructure (declaration §5); multi-host migration and multi-node network semantics are not claimed. |
| Posture | **Single-host is the declared posture for now (decision D1, option (b)).** The opt-in TCP gRPC listener that multi-host needs is an unstarted project; no multi-host claim may be made without MULTI-HOST-VERIFIED evidence. |
| Options | Decision **D1** (and D2). D1 is resolved as option (b); D2 (the `chv-node` dependency) follows from that choice. |
| Accepted design | [ADR-029](specs/adr/029-multi-host-dispatch.md) (Status: **Accepted — implementation not started**, ratified 2026-11-03) specifies a refined option (a): opt-in agent TCP gRPC with mandatory mTLS, split node identities, a dedicated control-plane client CA, learned endpoints, and a CP gRPC management bind. It is **not implemented**; the posture and status above are unchanged until the P1 gate passes. |

### UC-7 — Multi-host stretched-L2 fabric (VXLAN over WireGuard)

**Status: [DESIGN-ONLY] ([ADR-021](specs/adr/021-stretched-l2-vxlan-her-wireguard-fabric.md),
accepted; not qualified).**

The fabric (kernel VXLAN head-end replication over a WireGuard mesh, ports
65001/udp and 4789/udp, `fabric-linux` v0.1.5 provider) fails closed in the
qualification configuration. The agent logs a benign, non-fatal
fabric-identity warning at enrollment when the nwd fabric provider is
disabled (`failed to fetch fabric identity from nwd; reporting empty fabric
identity`, m4.1 §6). No multi-host fabric behavior is claimed.

### UC-8 — Air-gapped / offline install

**Status: [UNSUPPORTED/ABSENT].**

Escape hatches exist in `scripts/install.sh` — `INSTALL_CHV_TARBALL_PATH`
(local tarball), `INSTALL_CHV_SKIP_DEPS=1`, `INSTALL_CHV_SKIP_CLOUD_HV=1`,
and pre-staged base-image paths (`scripts/install.sh:634-640`) — but no
documented offline recipe exists, and the installer's default flow downloads
the tarball, Cloud Hypervisor, and the base image from the network. Whether
to define and document an air-gap profile is decision **D4**.

### UC-9 — High availability and disaster recovery

**Status: [UNSUPPORTED/ABSENT] for HA — explicitly rejected by design.**

- [ADR-011](specs/adr/011-single-node-controlplane.md): one control-plane
  process per cluster; SQLite single-writer; **no leader election, no
  replication, no multi-instance deployment**. "Speculative control-plane HA"
  is declaration §6 non-scope.
- DR posture: pre-migration and operator-run SQLite backups plus runbooks
  ([runbooks/control-plane-dr.md](runbooks/control-plane-dr.md),
  [runbooks/full-site-recovery.md](runbooks/full-site-recovery.md)). The
  control plane keeps pre-migration DB backups under
  `/var/lib/chv/backups/` (last 10).
- **Backup/restore is hard-fenced, not merely unfinished** (declaration
  §3: "a backup without restore validation is not DR"). CHV ships no
  tenant-data backup execution engine, so the whole surface — every
  `/v1/backups/**`, `/v1/backup-jobs`, `/v1/backup-history`, and legacy
  `/api/v1/backup*` route — answers `501 NOT_IMPLEMENTED`, and the WebUI
  exposes no backup navigation or actions. Tenant-data backup and restore
  are the operator's responsibility outside CHV. Snapshot/restore at the
  agent layer is a separate, existing surface, not a DR claim.

### UC-10 — Edge / resource-constrained profiles

**Status: [UNSUPPORTED/ABSENT].**

ADR-011 positions CHV for "sovereign edge environments with small cluster
sizes of approximately 20 nodes" — that is a management-plane **design
target**, not a qualified scale claim (see §7). No resource profiles (small
footprints, tuned limits, constrained-kernel variants) exist. Whether to
define them is decision **D5**.

## 6. Upgrade and lifecycle paths

| Path | Status | Statement |
|---|---|---|
| Package upgrade (deb/rpm) | **[CI-VERIFIED]** | Conffiles are `config\|noreplace`; services are not auto-restarted on upgrade (`preremove` skips stop on upgrade); SQLite migrations are forward-only; **downgrade is unsupported**; manual rollback procedure is documented in [release/package-contract.md](release/package-contract.md) ("Manual rollback procedure"). Evidence: `scripts/package/lifecycle-{deb,rpm}.sh` (fresh install, upgrade, remove, reinstall, persistent-data safety) run in `release.yml` and `package-nightly.yml`; `scripts/package/smoke-{deb,rpm}.sh` (install/remove/reinstall) in PR/nightly CI. |
| Agent drain on stop | **[QUALIFIED — KVM-VERIFIED]** | SIGTERM drains in-flight journal operations for up to 60 s; the unit's `TimeoutStopSec=75` protects the budget (unit file; m4.7 F1→F2 ordering). |
| Node-upgrade orchestration | **[CODE-SUPPORTED, UNQUALIFIED]** — partial, and narrower than documented | What exists: the compatibility-matrix boot gate (`crates/chv-controlplane-service/src/compat.rs`, wired at startup via `cmd/chv-controlplane/src/bootstrap.rs`, opt-in through `CHV_COMPAT_MATRIX_PATH`, fail-closed) and the `DrainNode` / `EnterMaintenance` / resume-scheduling gRPC handlers. What does **not** exist: the `SystemdNodeUpgrader` rolling-upgrade stack was **deleted as dead code** (PR #213, commit `26209555`); [ARCHITECTURE.md](ARCHITECTURE.md) still references it (stale — see §9). ADR-007's bundle-upgrade/one-step-rollback model is not implemented by the package lifecycle; how to present that gap is decision **D8**. |
| `chvctl upgrade` subcommands | **Removed** | `chvctl upgrade start/status/rollback/list` POSTed to BFF `/v1/upgrades` routes, which were **never registered** in the BFF router (`crates/chv-webui-bff/src/router.rs`), so the commands could not succeed. Originally recorded here as a finding (same class as the fixed #320); the dead surface was **removed** in #427. |
| Live network-policy update | **Half-open** | Policy enforcement end-to-end is qualified (attach-time materialization, m4.4). The UI firewall editor that wrote a dead-end store was **removed** with its `/v1/firewall-rules` CRUD and backing table (#355 Phase 1; the issue stays open for the live re-dispatch path). Remaining: legacy `set_firewall_policy` call sites can apply empty rulesets (**#360**, open). |
| Storage migration during upgrade | Boundary | Quiescent-source migration only (**#394**): concurrent-write migration silently loses data and is not claimed. Migration task state is in-memory; no cross-restart resume. |

## 7. Supported envelope

Only declaration-backed statements appear here. Anything not in this section
is not supported.

| Dimension | Supported statement | Source |
|---|---|---|
| Host count | **Single host.** All qualification evidence is single-host (nested KVM on a 16 vCPU / 31 GiB shared host). | Declaration §5; m4.9 §3 |
| VMM | Cloud Hypervisor only, pinned **v53.0** (moved from v43.0 via the #448 campaign — [campaign evidence root](evidence/vmm-requalification/v53.0/README.md)). The CH serial-console upstream defect (silent serial-manager thread death — present at both v43.0 and v53.0, campaign leg 02) is the recorded gate above KVM-VERIFIED; re-verify at any Cloud Hypervisor upgrade. | Declaration §3; m4.9 §3; #448 leg 02 |
| Architecture / OS | Linux x86_64; `.deb` + `.rpm`. Qualified on Ubuntu noble amd64; `.rpm` untested in prompt-04. | Declaration §3; m4.2 |
| Network profile | Single-host CHV-owned bridge overlay (local bridge + taps) with the nwd host-safety gate. Multi-host VXLAN fabric unproven. | Declaration §3 with the m4.9 §4.5 precision record |
| Storage profiles | Local file (VM-integrated) + LVM (stord layer only; not reachable from the VM lifecycle, #379). Ceph RBD / iSCSI not claimed. | Declaration §3; m4.5 |
| Migration | Quiescent-volume (single-writer) disk migration over mTLS between two stord instances, single host. Multi-host migration unproven. | Declaration §3; m4.6; m4.9 §4.1 |
| Lifecycle authority | Exactly one durable CellHV Core authority per node; legacy/control-plane paths are compatibility adapters only. | Declaration §3; prompt-02 |
| Backup/restore | **Hard-fenced (`501 NOT_IMPLEMENTED`).** | Declaration §3; no execution engine or restore validation exists |
| Scale | **No concurrency, density, throughput, or requests-per-second claim.** M4.8 numbers are baselines on the qualification host only. | m4.8; m4.9 §3 |
| Hardware minimums | **No qualified minimum.** [DEPLOYMENT.md](DEPLOYMENT.md) states "Minimum 4 cores, 8 GB RAM, 50 GB disk" and now labels it an **unevidenced recommendation**; it remains pending Prompt-05/06 qualification. ADR-011's "approximately 20 nodes" is a design target, not a qualified scale claim. | This document; ADR-011 |

**Pending:** Prompt 05 (release-candidate notes) will fix the public wording
of the supported matrix; Prompt 06 (reference deployment) will add the first
field-evidence tier. Neither is pre-claimed here.

## 8. Open design decisions

None of these is decided by this document. Each row names the decision, the
options, the trade-offs, and what would unblock it.

| # | Decision | Options | Trade-offs | What unblocks it |
|---|---|---|---|---|
| D1 | Multi-host dispatch: how can a control plane reach remote agents? | (a) Add an opt-in TCP gRPC listener to `chv-agent` (mTLS, node-cert identity). (b) Declare single-host-only and document it. (c) Stage both: document single-host now, land the listener behind a feature flag later. | (a) Unblocks real multi-host but adds an exposed listener, new attack surface, and requires MULTI-HOST-VERIFIED evidence that the current infrastructure cannot produce (declaration §5). (b) Honest and cheap, but strands the code-supported enrollment protocol and the documented console routing. (c) Keeps both paths open, but carries undecided surface area longer. | Maintainer choice plus a qualification topology with a second host (or an explicit re-scoping of the claim). **Declared posture 2026-10-30 — option (b), pending maintainer ratification of the decision record: single-host is the declared posture for now. The opt-in TCP gRPC listener remains an unstarted project; any multi-host claim requires MULTI-HOST-VERIFIED evidence.** [2026-11-03: **Accepted supersession** — [ADR-029](specs/adr/029-multi-host-dispatch.md) (Status: **Accepted — implementation not started**, ratified 2026-11-03) selects a refined option (a): an opt-in agent TCP gRPC listener with mandatory mTLS, split node identities, a dedicated control-plane client CA, and learned endpoints. It replaces option (b) as the design; the [DESIGN-ONLY] tier stands and no multi-host claim may be made until the P1 gate passes.] |
| D2 | `chv-node` package dependency: keep or relax the hard dependency on `chv-controlplane`? | (a) Keep. (b) Drop to `recommends`/`suggests`. (c) Split a `chv-node-common` and make the CP dependency explicit only in a meta-package. | (a) Simple, matches the qualified single-host topology; installs a useless control plane on every future node host. (b) Correct shape for node-only hosts, but changes tested install semantics and can produce half-provisioned hosts (CP fails closed anyway). (c) Cleanest dependency graph, most packaging work. | D1: node-only hosts only matter if multi-host dispatch exists. [2026-11-02: ADR-029 does not decide D2. The transition plan sequences the package split after remote dispatch; option (c) is the planned shape.] |
| D3 | Serving the UI from packages | (a) Headless-only posture: document `chvctl` as the package-mode interface; UI requires install.sh. (b) Document an operator-provided reverse proxy against `/usr/share/chv/ui` + the BFF/console proxies. (c) The deb configures nginx itself (or ships a snippet). (d) Serve the UI from `chv-controlplane` itself (tower-http `ServeDir` from disk, opt-in `[webui]` config section). | (a) Cheapest, narrows the product. (b) Flexible, pushes TLS/edge concerns to the operator with no contract. (c) Best out-of-box parity with install.sh, but adds a web-server dependency and edge ownership to the packages. (d) Out-of-box parity without an edge dependency, but adds a serving surface to the binary and needs its own qualification. | **Resolved 2026-10-03, phased: interim (b), target (d); (c) rejected.** Interim: the packages ship an example nginx conf (`/usr/share/chv/examples/chv-example.conf`) plus operator docs ([DEPLOYMENT.md](DEPLOYMENT.md) "Serving the Web UI in package mode"); serving-from-packages is [CODE-SUPPORTED, UNQUALIFIED] until a container qualification leg exists. Target: serve from the binary — tracked in #447. (c) is rejected permanently: it breaks the fail-closed "packages provide layout only" contract (§4) and forces nginx onto every host. [2026-10-07, #447:] target (d) implemented — `tower-http` `ServeDir` from disk behind an opt-in `[webui]` config section (`enabled`, default `false`; `dir`, default `/usr/share/chv/ui`), prefix-guarded static fallback with SPA and cache headers in `admin_router`; `install_nginx` and the example conf shrunk to proxy-only edges; the `/ws/` console proxy stays at the edge by decision (maintainer-ratified 2026-10-07). [2026-10-08, #549:] the container package-smoke leg ran and passed (`scripts/package/smoke-webui.sh`, wired into `package-pr.yml`) — serving-from-packages moved [CODE-SUPPORTED, UNQUALIFIED] → [CONTAINER-VERIFIED] (§5 UC-3). |
| D4 | Air-gapped support | (a) Not supported; state it. (b) Document an offline recipe from the existing escape hatches (`INSTALL_CHV_TARBALL_PATH`, `INSTALL_CHV_SKIP_DEPS`, `INSTALL_CHV_SKIP_CLOUD_HV`, pre-staged images). (c) Add a first-class offline mode to the installer. | (a) Free, loses sovereign/edge adopters the positioning targets. (b) Cheap documentation work; the hatches exist but are untested as a recipe. (c) Real installer work plus a qualification leg. | Maintainer priority; a qualification leg if (b) or (c). |
| D5 | Edge / resource profiles | (a) None; the current units and defaults are the only profile. (b) Document one "small" profile (documented unit overrides). (c) Ship alternate unit/config variants in the packages. | (a) Honest, no false edge signal. (b) Documentation-only, unqualified. (c) Real surface area, needs evidence on constrained hardware. | Field evidence from Prompt-06; ADR-011 positioning vs. actual demand. |
| D6 | Installer VMM pin reconciliation | (a) Change `scripts/install.sh` to download the qualified pin (v43.0). (b) Re-qualify against the newer VMM and move the pin. (c) Make the VMM version an installer variable with the qualified pin as default. | (a) Aligns installer with evidence; pins all installs to a VMM with a known serial-console defect (contained, not cured — m4.9 §3). (b) Gets fixes but requires full re-qualification and re-verification of the #345/#409-class thread-name coupling. (c) Flexible, but a default that differs from evidence is a footgun either way. | **Resolved 2026-10-03 — option (a) implemented**: the installer downloads the qualified v43.0 pin (#422); the v51.1 drift is closed. Follow-up (2026-10-03): upstream stable is now **v53.0** (released 2026-07-12); the resulting 10-release gap is deliberate — the recorded path to close it is option (b), tracked as issue **#448** (see the §9 CVE disclosure for what the gap costs). **Resolved 2026-10-03 (later) — option (b) implemented**: the #448 campaign re-qualified v43.0 → v53.0 on the qualification host across six evidence legs (anchor kvm-smoke; serial-console re-check; M4.3 lifecycle — the #345 stop wedge not reproduced at v53 with a v43 control reproducing it; M4.6 migration; M4.2/M4.4/M4.5 smokes; security regression — CVE-2026-27211 closed with runtime proof, CVE-2026-45782 closed at records tier, no new advisories, no new regressions). The pin moves to **v53.0** with download digest verification; Landlock stays off (the CVE closes via the version move — `backing_files=false` + the agent's existing `image_type` pinning, leg 06 §4.2); the silent serial-manager thread death remains upstream-unfixed at v53.0 and is carried by existing containment (adaptations tracked: #469; the merged #284 rotation and #410 heal detector carry the rest). [Campaign evidence root](evidence/vmm-requalification/v53.0/README.md). |
| D7 | TLS termination at the edge | (a) Keep `:80` plaintext as the documented default; recommend TLS in nginx. (b) Make the installer configure TLS by default (self-signed or operator cert). | (a) Matches all qualified evidence; ships plaintext by default in a product positioned for sovereign environments. (b) Stronger default posture; no qualified evidence for the TLS-edge shape, cert-management burden lands on install.sh. | Prompt-06 deployment profile; security review appetite. |
| D8 | Presenting ADR-007 (upgrade/rollback policy) against reality | (a) Mark ADR-007 as aspirational relative to the shipped lifecycle and point here. (b) Update ADR-007 to the qualified package-upgrade model. (c) Re-implement the rolling-upgrade stack (deleted in #213) and qualify it. | (a) Cheap, leaves a stale-looking ADR. (b) Honest and small, narrows the design promise. (c) Largest effort, restores the designed capability, needs qualification evidence that does not exist. | Maintainer decision on whether node rolling upgrade is roadmap or non-goal; the dead `chvctl upgrade` surface was removed via #427, so option (c) would also require re-adding a client surface (§6). |

## 9. Known boundaries and residual risk

Consolidated from [m4.9 §3](evidence/production-readiness/v0.3.0-rc1/04-real-host-qualification/m4.9-status.md).
Nothing here is a surprise; each item is disclosed in the milestone that
found it.

**Infrastructure-provability boundaries:**

- Host reboot is not provable on the qualification topology; the
  four-daemon cold restart + cold reconcile leg is the labeled subset.
- Lifecycle evidence is single-VM, single-node, serial. Concurrency evidence
  is limited to the bounded M4.8 read/write workload.
- Nested virtualization only; bare-metal KVM behavior is not separately
  evidenced.
- All M4.8 numbers are baselines on this host; no scale claims derivable.
- Multi-host anything is unprovable (declaration §5); capability is capped at
  KVM-VERIFIED.

**Open disclosed issues (worst-first; anchored in m4.9 §3, with later
disclosures added here):**

- **Pinned-VMM CVE exposure — CLOSED 2026-10-03 (D6 option (b) implemented).**
  The former v43.0 pin was in the affected range of CVE-2026-27211
  (GHSA-jmr4-g2hv-mjj6) and CVE-2026-45782 (GHSA-f47p-p25q-83rh); both
  close at the v53.0 pin, with runtime proof for CVE-2026-27211 (the full
  exfiltration chain reproduced at v43 under the CHV invocation shape and
  refused fail-closed at v53 on every entry point) —
  [campaign leg 06](evidence/vmm-requalification/v53.0/06-security-regression.md).
  No advisory affects v53.0 as of 2026-10-03.
- **#394** — concurrent-write migration silently loses data; quiescent-source
  migration is the claimed mode.
- **#368** — fixed by #484 — a transient effector failure terminally
  failing a journaled VM create is now re-driven by a bounded orchestrator
  pass (a new `RecreateVm` operation per attempt, spaced by the
  dispatch-retry backoff curve 10 s / 20 s / 40 s, at most 3 attempts);
  on exhaustion the VM is marked terminally Failed with
  `CREATE_REDRIVE_EXHAUSTED` plus the reported failure code, and the BFF
  renders a `Failed` runtime status as-is with `last_error` (Failed-wins
  over the desired power state). No pre-fix state persists
  (already-terminal creates stay terminal — a re-drive is always a new
  operation, never a resurrection).
- **#345 (Cloud Hypervisor side)** — the v43 control-loop hang after guest
  poweroff is contained, not cured; the CH v43 serial-console upstream defect
  gates above KVM-VERIFIED and requires re-verification at any Cloud
  Hypervisor upgrade. Restated 2026-10-03: upstream has since fixed parts of
  the chain (Pty flush gate + socket fd leak, v50.0 #7502; pre-connect
  output buffering, v53.0 #8322), but per source inspection the silent
  serial-manager thread death is **still present at v53.0** — the defect is
  contained, not cured, on both v43.0 and v53.0, so CHV's containment
  (reboot rotation, drain-then-close, console healing) carries forward and
  the gate above KVM-VERIFIED survives any pin move, including the #448
  campaign.
- **#355 / #360** — the UI firewall editor that wrote a dead-end store was
  removed (#355 Phase 1; the issue stays open for live re-dispatch), and
  the legacy empty-ruleset hazard at the two legacy `set_firewall_policy`
  call sites was fixed by #452. Pre-fix stored data caveats apply
  (pre-#365 NULL gateways, pre-#369 dialect rules; re-save repairs).
- **#378 / #379** — #378 fixed by #495 — the volume snapshot family
  (snapshot/clone/restore/delete-snapshot) is now rejected at accept
  time on core-managed nodes: InvalidArgument surfaced as HTTP 400
  BEFORE any journaling (no operations row, no volume_desired_state
  intent, no clone target row), keyed on the node's authority mode as
  carried by the agent's node inventory and failing OPEN on an unknown
  mode; the agent-side Unimplemented fail-closed dispatch remains the
  enforcement backstop. #379 stays open — LVM is still unreachable
  from the VM lifecycle (stord layer only).
- **#384 / #385 / #386** — last-writer-wins physical upserts + clone TOCTOU;
  respawned stord drops operator config beyond the allowlist; ownerless
  imported/template volumes (fixed by #481 — pre-fix volumes remain
  ownerless and admin-only).
- **#401 / #402** — destination-only stord was not expressible and
  migration mTLS rejections were unobservable on both ends; fixed by #483
  (the two migration halves are independently optional under
  `enabled = true`) and #479 (rejection causes surfaced at the sender,
  rejected handshakes warn-logged on the destination). No pre-fix state
  persists.
- **#351 / #336 / #372** — delete-path kill-refusal tolerance;
  JWT-secret/encryption-key coupling; remaining chvctl↔BFF contract drift.

**Inherited / design non-claims:**

- The UI itself was not re-qualified in prompt-04; all scenario evidence is
  chvctl/BFF/API-driven. The declaration's "UI/BFF/CLI: supported for the
  reference deployment" should be scoped to the exercised surfaces when
  Prompt 05 words the release notes (m4.9 §4.6).
- M2.5 delete retention: `vm delete` removes authority-side state but the
  BFF list row, CP row, VM dir, volume row, and backing file persist.
- CP-orchestrated migration (`agent migrate_vm`) fails closed in
  core-managed mode by design (single-writer enforcement).
- Migration-path edges: FinalizeAck wait has no timeout; migration task
  state is in-memory (no cross-restart resume).
- The dnsmasq runtime path is hardcoded to `/run/chv/nwd`
  (`crates/chv-nwd-core/src/dns.rs`) — a single-host assumption.
- Cross-restart snapshot-lifecycle edge (stord restart does not rehydrate
  handles taken before the restart).

**Documentation drift found while writing this document (not fixed here —
this branch changes only this file):**

- [ARCHITECTURE.md](ARCHITECTURE.md) still describes `SystemdNodeUpgrader`
  at `crates/chv-controlplane-service/src/systemd_upgrader.rs`; that file was
  deleted in PR #213. The ADR-007 presentation gap is decision D8.
  *(Resolved after this document was written: PR #433 rewrote the
  ARCHITECTURE.md upgrade section to the surviving surfaces, and the
  `chvctl upgrade` dead surface was removed per #427.)*
- [release/package-contract.md](release/package-contract.md) contradicts
  itself on whether `chv-node` includes `chvctl` (nfpm is truth: it does
  not).
- `packaging/config/chv.yaml` shipped `authority_mode: legacy` while every
  shipped `.toml` config selected core-managed (§10) — resolved via #424.
- `scripts/install.sh` formerly defaulted to the `cellhv/chv` repository and
  downloaded Cloud Hypervisor v51.1 (§5 UC-1, decision D6; also the
  `get.cellhv.com` hosting design in [DEPLOYMENT.md](DEPLOYMENT.md)). The
  repository is now canonical at `o3kio/chv` (`kubedoio/chv` returns HTTP 301
  there; the installer default follows it), and the installer pins the
  qualified v53.0 (#448). `get.cellhv.com` is **not stood up** (NXDOMAIN);
  [DEPLOYMENT.md](DEPLOYMENT.md#hosting-the-installer-getcellhvcom) keeps only
  the optional hosting design — it is not a live command.

## 10. Legacy and unsupported surfaces

The following surfaces are **not part of any supported topology**. Do not
deploy them; do not file evidence against them.

| Surface | Status | Why fenced |
|---|---|---|
| `docker-compose.yml`, `docker-compose.prod.yml` | `docker-compose.yml`: Legacy / DEV-ONLY; `docker-compose.prod.yml`: Legacy / UNMAINTAINED (both explicit in-repo banners) | **The image builds on amd64; the stacks remain unsupported.** Neither compose file provisions TLS/mTLS material or a bootstrap token, and the control plane fails closed without them by design, so the stacks do not run end-to-end as configured. Use the `.deb`/`.rpm` packages or `scripts/install.sh`. |
| `Dockerfile`, `deploy/entrypoint.sh` | DEV / LAB convenience only (explicit in-repo banner) | The image builds on amd64 (Rust 1.98.1 pin, protoc/cmake, `VERSION`, digest-verified Cloud Hypervisor v53.0); non-amd64 builds fail fast because the pinned VMM assets are x86_64-only. It is useful for local iteration and image-build smoke checks, but it is not a supported deployment path. |
| Legacy `chvbr0` host bridge/NAT bootstrap | Opt-out dev convenience (`INSTALL_CHV_NO_BRIDGE=1`) | Qualified networks are API-created and `chv-nwd`-managed (`br-<net_id>`). |
| `authority_mode = "legacy"` | Compatibility adapter only | Explicit opt-in; the agent logs a warning. Qualified posture is core-managed everywhere. |
| `packaging/config/chv.yaml` (`/etc/chv/chv.yaml` in `chv-node`) | Reference-only | Ships `authority_mode: core-managed` (aligned with the shipped `.toml` configs via #424). No daemon consumes it for authority selection. |
| `docs/examples/bootstrap.sh` | Manual helper | Points operators to `scripts/install.sh`; not a supported install path. |
| BFF backup manager | Hard-fenced (`501 NOT_IMPLEMENTED`) | No execution engine. The API and WebUI surface are disabled rather than left as a broken no-op; tenant-data backup is the operator's responsibility (declaration §3). |
| `chvctl upgrade` subcommands | Removed | Targeted unimplemented BFF routes (§6); removed via #427. |

## 11. Source-of-truth map

| Document | Owns | Relationship to this document |
|---|---|---|
| **This document** (`docs/DEPLOYMENT-ARCHITECTURE.md`) | Deployment architecture: components, interface/port contract, provisioning model, topology + use-case tiers, upgrade paths, supported envelope, open design decisions | Authoritative for the above; all other docs link here for architecture and tier claims. |
| [DEPLOYMENT.md](DEPLOYMENT.md) | Single-host operator how-to (quick start, manual steps, multi-node console routing, installer hosting) | How-to only; its hardware-minimums claim is flagged in §7 pending Prompt-05/06. |
| [release/PIPELINE.md](release/PIPELINE.md) | Release engineering: versioning, CI/CD, artifact pipeline | Links here for what the artifacts deploy. |
| [release/package-contract.md](release/package-contract.md) | The package contract: contents, ownership, upgrade/rollback procedures | nfpm configurations remain the mechanical truth for contents (§2.4). |
| [PACKAGING.md](PACKAGING.md) | Packaging overview and operator package steps | Links here for topology and tiers. |
| [OPERATIONS.md](OPERATIONS.md) | Day-2 operations: monitoring, CLI reference, live-database access, multi-node operations | Day-2 only. |
| [install/](install/) (channels, debian-ubuntu, rhel-rocky-alma, from-github-release, uninstall) | Per-distro install instructions | Use the canonical `o3kio/chv` repository URLs; `kubedoio/chv` 301-redirects there. The installer/repository resolution is recorded in §5 UC-1. |
| [runbooks/](runbooks/) | Incident procedures (control-plane DR, full-site recovery, snapshot restores) | DR procedures; the DR *claim* boundary is §5 UC-9. |
| [ARCHITECTURE.md](ARCHITECTURE.md) | System design and data flow | The deleted upgrade stack and the removed `chvctl upgrade` surface are recorded there in past tense; this document supersedes on deployment matters. |
| [specs/adr/](specs/adr/) | Design authority | ADRs are decisions; this document records their qualification tier (for example ADR-021 is DESIGN-ONLY). |
| [evidence/](evidence/) (frozen prompt-04 milestones) | Proof | Every [QUALIFIED — KVM-VERIFIED] label here cites a milestone doc. |
| [prompts/production-readiness/](prompts/production-readiness/) | Campaign definitions | Prompt 05 and Prompt 06 are pending; their results extend §5 and §7. |

---

*Terminology in this document follows the controlled technical English
standard: CHV names the platform; the VMM is Cloud Hypervisor; node (UI
"Hosts", Designer `servers`) and VM (UI "Instances") are the prose terms;
daemons are named in full; local endpoints are Unix sockets.*
