# ADR-021 Fabric Implementation Plan

Status: In execution
Date: 2026-09-28
Decision source: [ADR-021](../specs/adr/021-stretched-l2-vxlan-her-wireguard-fabric.md) — stretched-L2 fabric (VXLAN head-end replication over WireGuard)
Shared provider: [o3kio/fabric](https://github.com/o3kio/fabric) (tag `v0.2.0`) — `fabric-plan` / `fabric-linux` / `fabric-conformance`, governed by `contracts/fabric-provider-v2.md`
Tracking: issue #270

## Goal

Make CHV tenant networks one literal L2 segment (shared VLAN) across all
hypervisors: VMs in one network ARP and ping each other with real MACs,
identically on-node and cross-node, with all inter-site traffic encrypted.
Realize it by consuming the shared Kubedo fabric provider (`fabric-linux`)
inside `chv-nwd`, driven by control-plane-compiled plans over the existing
CP → agent → nwd path.

## Current state (evidence)

- `chv-nwd` networking (bridges, TAPs, dnsmasq, nft firewall) works locally;
  the VXLAN/FDB code is inert (`vtep_ip` never set, `nolearning` datapath,
  netns/bridge inconsistency) — replaced wholesale by the fabric path.
- CP has the full VNI/VTEP machinery but nothing invokes it:
  `VtepRepository::allocate_vni` (24h no-reuse) has no production caller;
  `vtep_registry` is never populated (agent reports `vtep_ip: ""`);
  `UpdateOverlay` ops are journaled but the orchestrator has no dispatch arm.
- `TopologySpec.vni/vtep_endpoints/overlay_type` exist on the wire but the
  agent hardcodes `vni: 0, overlay_type: 0`.
- No durable nwd state root (topology state is in-memory; `/run` is tmpfs).
- No DHCP option 26; no MTU on bridges/TAPs; eBPF is policy-only (Noop
  manager in production) and stays that way.

## Design decisions

1. **Provider embedding.** `chv-nwd-core` gains a `fabric` module wrapping
   `LinuxFabricProvider<RealCommandRunner>` (sync) behind a
   `std::sync::Mutex` + `tokio::task::spawn_blocking`. One provider instance
   for the daemon lifetime; it owns the fabric state root
   (`/var/lib/chv/nwd/fabric` — persistent, `StateDirectory=chv`, NOT `/run`
   which is tmpfs and would lose the WireGuard key on reboot).
2. **Names.** The shared provider derives all names from the `chv` prefix:
   fabric netns `chv-fabric`, WireGuard `chv-wg`, per-network consumer veth
   `chv-c-<fnv1a32-8hex>` in the host namespace. (ADR-021's prose said
   `wg-chv`; the provider's deterministic `chv-wg` supersedes it — names are
   hints, ownership is journaled.)
3. **Plan carriage.** New proto message `FabricPlan` (peer identities, MTUs,
   generations) added additively to `TopologySpec` and `UpdateOverlayRequest`
   in `chv-nwd-api.proto`, and to `UpdateOverlayRequest` in
   `control-plane-node.proto` (agent relays field-by-field between the two
   packages). Legacy fields remain (buf FILE rules permit additions).
4. **Drive path.** Fabric realization happens in nwd when either
   `TopologySpec.fabric` is set (ensure path) or `UpdateOverlayRequest.fabric`
   is set (update path — the multi-node fan-out). Both call the same
   internal apply: validate plan → provider `apply_plan` (journal-before-
   mutate, idempotent, fail-closed on foreign state) → enslave the consumer
   veth to the tenant bridge → set bridge/TAP MTU to `tenant_mtu` → refresh
   firewall policy scope. Teardown: provider `remove_network` (reverse
   order, preserves the WG key) before local bridge/dnsmasq/netns teardown.
5. **Key provisioning (node-local, never transported).** nwd exposes a new
   `GetFabricIdentity` RPC: ensures the host keypair exists
   (`fabric-linux::ensure_private_key`, 0600, adopt-if-valid) and returns the
   public key plus the measured underlay MTU. The agent reports both in
   `NodeInventory` over mTLS; the CP upserts them into `vtep_registry` and
   allocates the node's fabric transport IP. Private key material never
   leaves the node (ADR-021 guardrail satisfied; "delivered over mTLS"
   applies to the public identity).
6. **Fabric addressing.** CP allocates each node a unique fabric transport
   IP from `100.100.0.0/16` (stored in `vtep_registry.fabric_ip`).
   WireGuard AllowedIPs carry only transport /32s.
7. **MTU.** nwd measures the default-route MTU for `GetFabricIdentity`.
   The CP plan compiler derives `tenant_mtu = min(participants' underlay_mtu)
   − 110` and `fabric_mtu = min − 60` (defaults 1380/1440 when unmeasured),
   per ADR-021 §3. Advertised via DHCP option 26 (both dnsmasq templates).
8. **Generation fencing.** `plan_generation` = the network's
   `desired_generation`. nwd rejects a plan whose `plan_generation` is lower
   than the last applied generation for that network
   (`ChvError::StaleGeneration`). `binding_generation` comes from
   `vni_allocations` (new column, default 1).
9. **VNI allocation.** Lazy: the CP planner allocates via
   `VtepRepository::allocate_vni` when a network has
   `overlay_type = 'vxlan'` and `vni = 0` (avoids touching the BFF's raw-SQL
   create path in this phase).
10. **Bounded flood list.** Peers = `get_vteps_for_network` join
    (`vtep_registry` × `vm_desired_state.target_node_id` ×
    `vm_nic_desired_state.network_id`), extended with fabric identity
    columns — exactly the enrolled hosts hosting endpoints of that network.

## Phases

### Phase 1 — nwd fabric core (shared-crate adoption)
Branch: `fabric-adr021-implementation`. Files:
- Root `Cargo.toml` + `deny.toml` (`allow-git += https://github.com/o3kio/fabric`):
  git deps `fabric-plan`/`fabric-linux`/`fabric-conformance` @ `v0.1.0`.
- `crates/chv-config`: `NwdConfig.fabric` section (`enabled` default false,
  `state_dir` default `/var/lib/chv/nwd/fabric`, `name_prefix` "chv",
  `wireguard_port` 65001, `vxlan_port` 4789, default MTUs 1380/1440).
- `proto/node/chv-nwd-api.proto`: `FabricPeer`, `FabricPlan` messages;
  `TopologySpec.fabric = 11`; `UpdateOverlayRequest.fabric = 5`;
  `DhcpScope.mtu = 6`; `GetFabricIdentity` RPC + messages.
- `crates/chv-nwd-core/src/fabric.rs` (new): `FabricHandle` trait (async:
  apply / remove / identity / status / consumer veth name), proto→plan
  conversion with validation, `NwdFabricProvider<R: FabricCommand>` impl,
  `FabricError → ChvError` mapping (ForeignState→Conflict, generation
  checks→StaleGeneration).
- `crates/chv-nwd-core/src/executor.rs`: fabric field on `LinuxExecutor`;
  fabric path in `ensure_topology` / `UpdateOverlay` / `delete_topology`;
  bridge enslavement + MTU; TAP MTU on attach; `GetOverlayStatus` fabric
  awareness; `GetFabricIdentity` handler; DHCP option 26 in both dnsmasq
  templates.
- `crates/chv-nwd-core/src/state.rs`: `TopologyState` gains
  `tenant_mtu: Option<u32>`, `fabric_plan_generation: Option<u64>`.
- `packaging/systemd/chv-nwd.service`: `ReadWritePaths += /var/lib/chv`;
  `packaging/nfpm/chv-node.yaml`: depends `wireguard-tools`.
- `crates/chv-nwd-core/tests/fabric_conformance.rs`: runs
  `fabric_conformance::run_suite()` (anti-drift gate).
- Legacy nolearning VXLAN/FDB executor methods stay (inert, unreachable in
  fabric mode); removal is a separate cleanup PR (Phase 4).
- Tests: RecordingRunner-backed `FabricHandle` injected into
  `LinuxExecutor`; assert command sequences, enslavement, idempotent replay,
  teardown order, key non-leakage, fail-closed when disabled.
- ADR-021 prose touch-up: `wg-chv` → provider-generated `chv-wg`.

### Phase 2 — agent relay + identity reporting
- `proto/controlplane/control-plane-node.proto`: `FabricPeer`/`FabricPlan`
  (mirror), `NodeInventory.wireguard_public_key = 11`,
  `underlay_mtu = 12`.
- `crates/chv-agent-core`: `NwdClient::get_fabric_identity`;
  inventory/enrollment populate the new fields (best-effort with warn —
  periodic inventory re-syncs); `update_overlay` relay maps `fabric`
  field-by-field.
- Agent in-file tests updated.

### Phase 3 — CP: identity store, planner, dispatch
- Migration `0040_fabric_identity.sql`: `vtep_registry` gains `public_key`,
  `underlay_endpoint`, `fabric_ip`, `underlay_mtu`; `vni_allocations` gains
  `binding_generation INTEGER NOT NULL DEFAULT 1`.
- `chv-controlplane-store/src/vtep.rs`:
  `register_fabric_identity` (upsert + fabric-IP allocation from
  100.100.0.0/16), `get_fabric_peers_for_network` (placement join with full
  identity).
- `chv-controlplane-service/src/fabric_planner.rs` (new): compile per-node
  `FabricPlan`s (lazy VNI allocation, bounded flood list, MTU derivation,
  generations).
- Orchestrator: `UpdateOverlay` dispatch arm → planner → per-node
  `OverlayManager::send_overlay_update` (extended to carry the fabric plan;
  generation-fenced `desired_state_version`).
- Enrollment: register fabric identity when `wireguard_public_key` present.
- Integration tests (test pool): planner compilation, flood-list scoping,
  VNI/fabric-IP allocation, identity upsert.

### Phase 4 — follow-ups (explicitly out of this round)
- Removal of legacy nolearning VXLAN/FDB executor code and
  `reconcile_fdb_entries`.
- BFF network-create `overlay_type` surface; placement-change triggers for
  overlay fan-out (VM create/destroy/migrate); migration FDB re-point
  retirement (learning + GARP suffice per ADR-021).
- Privileged multi-host evidence harness (o3kio/fabric#2 Phase 3): three
  real hosts, real handshakes, cleartext-underlay capture, zero-leak
  teardown. Required before any production claim.
- Endpoint policy follow-up: `vtep_registry.underlay_endpoint` is
  first-registration-wins because today's only writer derives it from the
  observed gRPC peer address (LB/proxy churn would otherwise rotate it).
  When agents self-report their underlay addressing, an explicit
  node-reported endpoint must win over the earlier peer-derived pin (and
  the operator-correction tooling the store docstring mentions should
  land with it).
- Startup reconciliation from the provider's plan journal.

## Test strategy

- Phase 1–3: unprivileged — RecordingRunner fake kernel (fabric-linux),
  mock executors, tonic UDS tests, SQLite test pools.
- Gates per phase: `cargo check -p <crate>` → `cargo test -p <crate>` →
  `cargo clippy -p <crate> --all-targets -- -D warnings`; full workspace
  gates before the PR; `buf lint` / `buf breaking` run in CI (proto changes
  are additive-only).
