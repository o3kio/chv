# `chv-nwd` Component Spec

## Purpose
`chv-nwd` is the host-side network control service for tenant connectivity and policy.

## MVP-1 functions
- bridge creation and verification
- Linux namespace lifecycle
- tap/veth wiring
- routing rules
- NAT rules
- DHCP service
- DNS service
- firewall rule application
- basic public exposure / LB functions

## Non-goals
- no network VM in MVP-1
- no eBPF VXLAN datapath; kernel VXLAN owns encapsulation/decapsulation
- no distributed overlay control plane
- no full flow-state replication guarantee during upgrades

## Responsibilities
- converge topology from desired state
- attach and detach VM NICs
- expose policy application through a typed local API
- preserve topology where possible across daemon restart
- publish health and namespace state
- publish Prometheus metrics on the dedicated `metrics_bind` listener
  (loopback default `127.0.0.1:9102`, unauthenticated; issue #657)

## Traffic policy
- east-west traffic stays local when safe
- routed/NAT/public exposure flows are managed through `chv-nwd` functions

## Service model
- one `chv-nwd` per node in MVP-1
- upgrade mode: drain and replace

## Recovery model
- prefer deterministic reconstruction from desired state plus local cache
- allow brief active-flow disturbance if exact flow preservation is not implemented

## Multi-node Overlay Status

`chv-nwd` owns kernel VXLAN interface lifecycle and explicit FDB management for multi-node L2 overlays. eBPF scope is policy enforcement, rate limiting, and counters at TC hooks; it must not be treated as the overlay datapath. Control-plane VNI/VTEP coordination is still required for new joins, migrations, and FDB updates.
