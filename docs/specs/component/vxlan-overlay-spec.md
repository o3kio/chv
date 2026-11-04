# VXLAN Overlay Networking Spec

## Purpose
Defines the L2 overlay network architecture for cross-node VM connectivity using kernel VXLAN with eBPF-based security policy enforcement.

## Implementation Status

Kernel VXLAN is the overlay datapath. `chv-nwd` is responsible for VXLAN interface lifecycle, explicit FDB entries, teardown, and post-migration ARP handling. eBPF is scoped to policy enforcement, rate limiting, and counters at TC hooks; it is not used for VXLAN encapsulation/decapsulation. Control-plane VNI/VTEP coordination remains required for new joins, migrations, and FDB updates.

## Owner
- **chv-nwd**: VXLAN interface lifecycle, FDB management, eBPF program loading
- **Control plane (CP)**: VNI assignment, VTEP registry, topology coordination

## Scope
- L2 overlay across nodes for tenant VM networks
- MAC address preservation during live migration
- Per-VM security policy enforcement via eBPF
- Integration with existing bridge/namespace model (ADR-005 extension)

## Architecture Overview

```
Node A                                          Node B
┌──────────────────────────────┐    ┌──────────────────────────────┐
│  namespace: net-{tenant}     │    │  namespace: net-{tenant}     │
│  ┌──────────────────────┐    │    │  ┌──────────────────────┐    │
│  │  bridge: br-{net_id}  │   │    │  │  bridge: br-{net_id}  │   │
│  │   │        │       │  │   │    │  │   │        │       │  │   │
│  │ tap-vm1  tap-vm2  vxlan│  │    │  │ tap-vm3  tap-vm4  vxlan│  │
│  │                   {VNI}│  │    │  │                   {VNI}│  │
│  └───────────────────┬───┘   │    │  └───────────────────┬───┘   │
│                      │       │    │                      │       │
│  eBPF TC hook ◄──────┘       │    │  eBPF TC hook ◄──────┘       │
│                              │    │                              │
└──────────────────┬───────────┘    └──────────────────┬───────────┘
                   │ VTEP_IP_A                          │ VTEP_IP_B
                   │                                    │
                   └────── VXLAN UDP 4789 ──────────────┘
                           (underlay network)
```

## Control Plane Responsibilities

### VNI Assignment
- Each tenant network gets a unique VNI (uint32 range: 1-16777214)
- VNI allocated on network creation, stored in SQLite (networks table)
- VNI not reused for 24 hours after network deletion (prevents stale FDB issues)
- VNI 0 reserved (means "no overlay, bridge-only")

### VTEP Registry
- Stored in SQLite: table `vtep_registry(node_id, vtep_ip, vtep_port, updated_at)`
- Each node registers its VTEP IP during enrollment (part of inventory report)
- CP pushes VTEP updates to nwd via existing gRPC channel (UpdateOverlay RPC)

### Topology Coordination Events

| Event | CP Action |
|---|---|
| VM created on node | Notify node's nwd to join VNI. Notify all peers to add FDB entry for VM's MAC. |
| VM destroyed | Notify all peers to remove FDB entry. If last VM on VNI for this node, remove VXLAN interface. |
| VM migrated | Notify dest nwd: add FDB locally. Notify all peers: update FDB (MAC now at dest VTEP). Notify dest: send gratuitous ARP. |
| Node joins | Node's nwd auto-joins all VNIs where its VMs participate. |
| Node leaves | Remove all FDB entries pointing to that node's VTEP. |

## chv-nwd Responsibilities

### VXLAN Interface Management

```bash
# {bridge} is chv_common::bridge_name_for_network(net_id): br-{net_id} while
# the name fits IFNAMSIZ (15 chars), otherwise br-{first8}{fnv16} — 128-bit
# ids (#664) always take the hashed form.

# Create VXLAN interface (per VNI, per namespace)
ip link add vxlan{VNI} type vxlan id {VNI} local {VTEP_IP} dstport 4789 nolearning

# Move to namespace and attach to bridge
ip link set vxlan{VNI} netns {namespace}
ip netns exec {namespace} ip link set vxlan{VNI} master {bridge}
ip netns exec {namespace} ip link set vxlan{VNI} up
```

### FDB Entry Management

```bash
# Add FDB entry (remote MAC reachable via peer VTEP)
ip netns exec {namespace} bridge fdb append {MAC} dev vxlan{VNI} dst {PEER_VTEP_IP}

# Remove FDB entry
ip netns exec {namespace} bridge fdb del {MAC} dev vxlan{VNI} dst {PEER_VTEP_IP}

# Replace (migration: MAC moves to new VTEP)
ip netns exec {namespace} bridge fdb replace {MAC} dev vxlan{VNI} dst {NEW_VTEP_IP}
```

### Gratuitous ARP (post-migration)

```bash
# Send from bridge interface within namespace ({bridge} per the note above)
ip netns exec {namespace} arping -U -c 3 -I {bridge} {VM_IP}
# Also send from vxlan interface to propagate to remote peers
ip netns exec {namespace} arping -A -c 3 -I vxlan{VNI} {VM_IP}
```

### ARP Suppression (optional optimization)

```bash
# Reduce broadcast by having bridge reply to ARP on behalf of known MACs
ip netns exec {namespace} bridge link set dev vxlan{VNI} neigh_suppress on
# Populate neighbor table with known VM MAC/IP pairs
ip netns exec {namespace} ip neigh add {VM_IP} lladdr {VM_MAC} dev vxlan{VNI} nud permanent
```

## eBPF Policy Enforcement

### Attachment Points
- TC egress hook on each TAP interface (per-VM egress policy)
- TC ingress hook on bridge interface (per-network ingress policy)
- Programs loaded from pre-compiled .o files (BPF CO-RE format)

### Security Policy Model

```
SecurityPolicy:
  vm_id: string
  default_action: ALLOW | DENY
  rules:
    - direction: INGRESS | EGRESS
      protocol: TCP | UDP | ICMP | ANY
      src_cidr: "10.0.0.0/24" (or "any")
      dst_cidr: "10.0.1.0/24" (or "any")
      src_port: 0-65535 (0 = any)
      dst_port: 0-65535 (0 = any)
      action: ALLOW | DENY
      priority: uint32 (lower = higher priority)
```

### eBPF Map Structure

```c
// Per-VM rule table (BPF_MAP_TYPE_HASH)
struct rule_key {
    __u32 vm_id_hash;      // FNV-1a of vm_id
    __u8  direction;       // 0=ingress, 1=egress
    __u16 priority;
};

struct rule_value {
    __u32 src_ip;
    __u32 src_mask;
    __u32 dst_ip;
    __u32 dst_mask;
    __u16 src_port_min;
    __u16 src_port_max;
    __u16 dst_port_min;
    __u16 dst_port_max;
    __u8  protocol;        // IPPROTO_TCP, UDP, ICMP, 0=any
    __u8  action;          // 0=deny, 1=allow
};

// Rate limit table (BPF_MAP_TYPE_HASH)
struct rate_key {
    __u32 vm_id_hash;
};

struct rate_value {
    __u64 tokens;          // current tokens
    __u64 last_refill_ns;  // last refill timestamp
    __u64 rate_bps;        // bytes per second limit
    __u64 burst_bytes;     // max burst size
};

// Stats counters (BPF_MAP_TYPE_PERCPU_ARRAY)
struct stats_key {
    __u32 vm_id_hash;
    __u8  direction;
};

struct stats_value {
    __u64 packets_allowed;
    __u64 packets_denied;
    __u64 bytes_allowed;
    __u64 bytes_denied;
};
```

### Program Update (atomic)
- New rules: write to BPF map (atomic per-entry)
- Full policy replace: write new entries, delete old entries (no program reload needed)
- Rate limit change: update rate_value in map (takes effect immediately)
- Program itself only reloaded on chv-nwd upgrade (rare)

## Proto Changes

### TopologySpec Extension

```protobuf
message TopologySpec {
  string network_id = 1;
  string tenant_id = 2;
  string bridge_name = 3;
  string namespace_name = 4;
  string subnet_cidr = 5;
  string gateway_ip = 6;
  map<string, string> options = 7;
  // NEW fields for overlay
  uint32 vni = 8;                              // 0 = no overlay
  repeated VtepEndpoint vtep_endpoints = 9;    // peer VTEPs for this network
  OverlayType overlay_type = 10;
}

enum OverlayType {
  OVERLAY_NONE = 0;     // bridge-only (single node)
  OVERLAY_VXLAN = 1;    // kernel VXLAN
}

message VtepEndpoint {
  string node_id = 1;
  string vtep_ip = 2;
  uint32 vtep_port = 3;  // default 4789
}
```

### Security Policy Messages

```protobuf
message SecurityPolicy {
  string vm_id = 1;
  string network_id = 2;
  PolicyAction default_action = 3;
  repeated SecurityRule rules = 4;
}

message SecurityRule {
  Direction direction = 1;
  Protocol protocol = 2;
  string src_cidr = 3;
  string dst_cidr = 4;
  PortRange src_port = 5;
  PortRange dst_port = 6;
  PolicyAction action = 7;
  uint32 priority = 8;
}

message PortRange {
  uint32 min = 1;   // 0 = any
  uint32 max = 2;   // 0 = same as min (single port)
}

enum Direction {
  DIRECTION_BOTH = 0;
  DIRECTION_INGRESS = 1;
  DIRECTION_EGRESS = 2;
}

enum Protocol {
  PROTOCOL_ANY = 0;
  PROTOCOL_TCP = 1;
  PROTOCOL_UDP = 2;
  PROTOCOL_ICMP = 3;
}

enum PolicyAction {
  POLICY_DENY = 0;
  POLICY_ALLOW = 1;
}

message RateLimitPolicy {
  string vm_id = 1;
  uint64 rate_bps = 2;        // bytes per second
  uint64 burst_bytes = 3;     // max burst
}
```

### New RPCs

```protobuf
service NetworkService {
  // Existing RPCs...

  // Overlay management
  rpc UpdateOverlay(UpdateOverlayRequest) returns (UpdateOverlayResponse);
  rpc UpdateSecurityPolicy(SecurityPolicy) returns (UpdateSecurityPolicyResponse);
  rpc UpdateRateLimit(RateLimitPolicy) returns (UpdateRateLimitResponse);
  rpc GetOverlayStatus(GetOverlayStatusRequest) returns (OverlayStatus);
}

message UpdateOverlayRequest {
  string network_id = 1;
  uint32 vni = 2;
  repeated VtepEndpoint vtep_endpoints = 3;
  repeated FdbEntry fdb_entries = 4;
}

message FdbEntry {
  string mac_address = 1;
  string vtep_ip = 2;
}

message OverlayStatus {
  string network_id = 1;
  uint32 vni = 2;
  bool vxlan_interface_up = 3;
  uint32 fdb_entry_count = 4;
  uint32 ebpf_programs_loaded = 5;
}
```

## MTU Considerations

| Underlay MTU | VXLAN Overhead | Inner (VM) MTU | Notes |
|---|---|---|---|
| 1500 | 50 | 1450 | Standard. VMs see 1450 MTU. |
| 9000 | 50 | 8950 | Jumbo frames. VMs can use standard 1500. |

- chv-nwd MUST set bridge MTU to (underlay_mtu - 50)
- If underlay_mtu >= 1550: inner MTU can be 1500 (transparent to VMs)
- underlay_mtu discovery: read from VTEP interface (`ip link show {underlay_if}`)
- CP SHOULD verify all nodes have compatible underlay MTU during enrollment

## Migration Flow (Network Perspective)

```
1. CP decides to migrate VM from Node A → Node B
2. CP → Node B nwd: UpdateOverlay (ensure VNI joined, FDB entries current)
3. [Disk migration happens]
4. [Memory migration happens - Cloud Hypervisor live migration]
5. VM resumes on Node B
6. CP → Node B nwd: send gratuitous ARP for VM's IP
7. CP → All peer nwd: UpdateOverlay (FDB: VM's MAC now at Node B's VTEP)
8. CP → Node A nwd: remove FDB entry for VM's MAC (if no other VMs share that MAC's network)
```

## Failure Modes

| Failure | Impact | Recovery |
|---|---|---|
| VXLAN interface down | Cross-node traffic for that VNI stops | nwd detects via link monitor, recreates interface, re-adds to bridge |
| FDB stale (points to old VTEP) | Packets go to wrong node, dropped | CP sends correct FDB update; nwd applies. Max stale duration: 1 heartbeat cycle (5s) |
| eBPF program fails to load | VM traffic unfiltered | nwd reports error to CP. Default-deny until program loads successfully |
| Underlay network partition | VXLAN traffic drops between partitioned nodes | VMs on same node continue. Cross-node connectivity lost until underlay recovers. |
| CP unavailable | No new FDB updates, no new VNIs | Existing tunnels continue working (cached state). No migrations possible (ADR-006). |

## Configuration

| Parameter | Default | Description |
|---|---|---|
| overlay.vxlan_port | 4789 | UDP port for VXLAN encapsulation |
| overlay.vtep_interface | auto-detect | Interface to use as VTEP (first non-loopback with default route) |
| overlay.nolearning | true | Disable MAC learning on VXLAN (explicit FDB only) |
| overlay.arp_suppress | false | Enable neighbor suppression (reduces broadcast) |
| overlay.inner_mtu | auto | Calculated as underlay_mtu - 50, capped at 1500 |
| ebpf.program_path | /usr/lib/chv/ebpf/ | Directory containing compiled BPF .o files |
| ebpf.default_action | deny | Default action when no policy loaded for VM |
| ebpf.stats_interval_secs | 10 | How often to read eBPF stats counters |

## Non-goals
- eBPF-based VXLAN encapsulation (kernel module handles datapath)
- Multi-tenant VNI sharing (each tenant network = unique VNI)
- EVPN/BGP control plane (explicit CP-managed FDB)
- IPv6 overlay (IPv4 VTEP addresses only in v1)
- Encryption of VXLAN traffic (rely on underlay encryption if needed, e.g., WireGuard)

## Security requirements
- eBPF programs must pass kernel verifier before loading
- BPF maps restricted to chv-nwd process (pinned with restricted permissions)
- VTEP IP must be validated against enrolled node inventory (prevent spoofing)
- FDB updates only accepted from CP over mTLS channel (not from peer nodes directly)

## Recovery model
- nwd restart: reconstruct VXLAN interfaces and FDB from CP-provided topology on next heartbeat
- eBPF program crash: kernel detaches program, traffic flows unfiltered. nwd detects via BPF link check, reloads.
- Bridge failure: recreate bridge, re-attach TAPs and VXLAN interface. Brief traffic interruption for VMs on that bridge.
