// SPDX-License-Identifier: GPL-2.0
// ==========================================================================
// CHV eBPF TC Policy Classifier
// ==========================================================================
//
// This file is provided as DOCUMENTATION / reference implementation.
// It requires separate compilation with clang and cannot be built as part
// of the normal `cargo build` workflow.
//
// To compile (requires clang >= 12, kernel headers, and libbpf):
//   cd ebpf && make
//
// The resulting policy_tc.o is loaded by LinuxEbpfManager via:
//   tc qdisc add dev <iface> clsact
//   tc filter add dev <iface> egress bpf da obj policy_tc.o sec tc
//
// ==========================================================================

// NOTE: In a real build environment, you would include vmlinux.h (generated
// from BTF) or the standard BPF headers. We show the conceptual includes here.
//
// #include "vmlinux.h"
// #include <bpf/bpf_helpers.h>
// #include <bpf/bpf_endian.h>

// For documentation purposes, we define the minimum necessary types inline:
#ifndef __section
#define __section(NAME) __attribute__((section(NAME), used))
#endif

typedef unsigned char  __u8;
typedef unsigned short __u16;
typedef unsigned int   __u32;
typedef unsigned long long __u64;

#define ETH_P_IP    0x0800
#define IPPROTO_TCP  6
#define IPPROTO_UDP  17
#define IPPROTO_ICMP 1

#define TC_ACT_OK    0   // pass packet
#define TC_ACT_SHOT  2   // drop packet

#define MAX_RULES_PER_VM 64

// Fixed-size map key derived from the vm_id's raw bytes. Distinct vm_ids up
// to 32 bytes produce distinct keys, unlike the previous FNV-1a u32 hash
// which could collide and let one VM's policy overwrite another's. 32 bytes
// is exactly a 128-bit hex resource id (#664).
struct rule_key {
    __u8 id[32]; // vm_id ASCII bytes, zero-padded; longer ids are rejected by userspace
};

// ---------------------------------------------------------------------------
// Map definitions
// ---------------------------------------------------------------------------

// Key: struct rule_key (32-byte zero-padded vm_id)
// Value: array of rules for that VM
struct rule_entry {
    __u8  direction;    // 0=both, 1=ingress, 2=egress
    __u32 priority;
    __u32 src_ip;
    __u32 src_mask;
    __u32 dst_ip;
    __u32 dst_mask;
    __u16 src_port_min;
    __u16 src_port_max;
    __u16 dst_port_min;
    __u16 dst_port_max;
    __u8  protocol;    // 0=any, 6=tcp, 17=udp, 1=icmp
    __u8  action;      // 0=deny, 1=allow
};

struct rule_set {
    __u32 count;
    struct rule_entry rules[MAX_RULES_PER_VM];
};

// BPF_MAP_TYPE_HASH: rule_key -> rule_set
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 1024);
    __type(key, struct rule_key);
    __type(value, struct rule_set);
} rule_map __section(".maps");

// Key: struct rule_key (32-byte zero-padded vm_id)
// Value: rate limit parameters
struct rate_entry {
    __u64 rate_bps;       // bytes per second
    __u64 burst_bytes;    // max burst size
    __u64 tokens;         // current token count (token bucket)
    __u64 last_refill_ns; // last refill timestamp (ktime_ns)
};

// BPF_MAP_TYPE_HASH: rule_key -> rate_entry
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 1024);
    __type(key, struct rule_key);
    __type(value, struct rate_entry);
} rate_map __section(".maps");

// Key: vm_id_hash (u32)
// Value: traffic statistics
struct stats_entry {
    __u64 packets_allowed;
    __u64 packets_denied;
    __u64 bytes_allowed;
    __u64 bytes_denied;
};

// BPF_MAP_TYPE_HASH: vm_id_hash -> stats_entry
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 1024);
    __type(key, __u32);
    __type(value, struct stats_entry);
} stats_map __section(".maps");

// Key: struct rule_key (32-byte zero-padded vm_id)
// Value: default action (u8: 0=deny, 1=allow)
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 1024);
    __type(key, struct rule_key);
    __type(value, __u8);
} defaults_map __section(".maps");

// ---------------------------------------------------------------------------
// Packet parsing helpers
// ---------------------------------------------------------------------------

struct ethhdr {
    unsigned char h_dest[6];
    unsigned char h_source[6];
    __u16 h_proto;
} __attribute__((packed));

struct iphdr {
    __u8  ihl_version;
    __u8  tos;
    __u16 tot_len;
    __u16 id;
    __u16 frag_off;
    __u8  ttl;
    __u8  protocol;
    __u16 check;
    __u32 saddr;
    __u32 daddr;
} __attribute__((packed));

struct tcphdr {
    __u16 source;
    __u16 dest;
    // ... remaining fields not needed for filtering
} __attribute__((packed));

struct udphdr {
    __u16 source;
    __u16 dest;
    __u16 len;
    __u16 check;
} __attribute__((packed));

// ---------------------------------------------------------------------------
// Token bucket rate limiting
// ---------------------------------------------------------------------------

static __always_inline int check_rate_limit(
    const struct rule_key *key,
    __u32 pkt_len)
{
    struct rate_entry *rate = bpf_map_lookup_elem(&rate_map, key);
    if (!rate)
        return 1; // no rate limit configured -> allow

    __u64 now = bpf_ktime_get_ns();
    __u64 elapsed_ns = now - rate->last_refill_ns;

    // Refill tokens based on elapsed time
    // tokens_to_add = elapsed_ns * rate_bps / 1_000_000_000
    __u64 tokens_to_add = (elapsed_ns / 1000) * rate->rate_bps / 1000000;
    __u64 new_tokens = rate->tokens + tokens_to_add;
    if (new_tokens > rate->burst_bytes)
        new_tokens = rate->burst_bytes;

    if (new_tokens < pkt_len)
        return 0; // over rate limit -> deny

    // Consume tokens
    rate->tokens = new_tokens - pkt_len;
    rate->last_refill_ns = now;
    return 1; // allow
}

// ---------------------------------------------------------------------------
// Rule matching
// ---------------------------------------------------------------------------

static __always_inline int match_rules(
    const struct rule_key *key,
    __u8  direction,   // 1=ingress, 2=egress
    __u32 src_ip,
    __u32 dst_ip,
    __u16 src_port,
    __u16 dst_port,
    __u8  protocol)
{
    struct rule_set *rs = bpf_map_lookup_elem(&rule_map, key);
    if (!rs)
        goto default_action;

    // Iterate rules sorted by priority (lowest value = highest priority)
    // Note: in practice rules should be pre-sorted by userspace
    for (__u32 i = 0; i < rs->count && i < MAX_RULES_PER_VM; i++) {
        struct rule_entry *r = &rs->rules[i];

        // Direction check
        if (r->direction != 0 && r->direction != direction)
            continue;

        // Protocol check
        if (r->protocol != 0 && r->protocol != protocol)
            continue;

        // Source IP/mask check
        if (r->src_mask != 0 && (src_ip & r->src_mask) != (r->src_ip & r->src_mask))
            continue;

        // Destination IP/mask check
        if (r->dst_mask != 0 && (dst_ip & r->dst_mask) != (r->dst_ip & r->dst_mask))
            continue;

        // Source port range check (only for TCP/UDP)
        if (r->src_port_min != 0 || r->src_port_max != 0) {
            if (src_port < r->src_port_min || src_port > r->src_port_max)
                continue;
        }

        // Destination port range check (only for TCP/UDP)
        if (r->dst_port_min != 0 || r->dst_port_max != 0) {
            if (dst_port < r->dst_port_min || dst_port > r->dst_port_max)
                continue;
        }

        // All conditions matched
        return r->action; // 0=deny, 1=allow
    }

default_action:;
    // No rule matched -> check default action
    __u8 *def = bpf_map_lookup_elem(&defaults_map, key);
    if (def)
        return *def;
    return 1; // default allow if nothing configured
}

// ---------------------------------------------------------------------------
// TC classifier entry point
// ---------------------------------------------------------------------------

__section("tc")
int policy_tc(struct __sk_buff *skb)
{
    void *data = (void *)(long)skb->data;
    void *data_end = (void *)(long)skb->data_end;

    // Parse Ethernet header
    struct ethhdr *eth = data;
    if ((void *)(eth + 1) > data_end)
        return TC_ACT_OK;

    // Only handle IPv4
    if (eth->h_proto != __constant_htons(ETH_P_IP))
        return TC_ACT_OK;

    // Parse IP header
    struct iphdr *ip = (void *)(eth + 1);
    if ((void *)(ip + 1) > data_end)
        return TC_ACT_OK;

    __u32 src_ip = ip->saddr;
    __u32 dst_ip = ip->daddr;
    __u8  protocol = ip->protocol;
    __u16 src_port = 0;
    __u16 dst_port = 0;

    // Parse L4 headers for port info
    __u8 ihl = (ip->ihl_version & 0x0f) * 4;
    if (ihl < 20)
        return TC_ACT_OK;

    // Ignore non-first IP fragments (fragment offset != 0).
    __u16 frag_off = __bpf_ntohs(ip->frag_off);
    if (frag_off & 0x1FFF)
        return TC_ACT_OK;

    void *l4 = (void *)ip + ihl;

    if (protocol == IPPROTO_TCP) {
        struct tcphdr *tcp = l4;
        if ((void *)(tcp + 1) > data_end)
            return TC_ACT_OK;
        src_port = __bpf_ntohs(tcp->source);
        dst_port = __bpf_ntohs(tcp->dest);
    } else if (protocol == IPPROTO_UDP) {
        struct udphdr *udp = l4;
        if ((void *)(udp + 1) > data_end)
            return TC_ACT_OK;
        src_port = __bpf_ntohs(udp->source);
        dst_port = __bpf_ntohs(udp->dest);
    }

    // Determine VM identity from the interface index.
    //
    // NOTE: `skb->ifindex` is only a placeholder. The keys the control
    // plane stores in rule_map/defaults_map/rate_map are 32-byte
    // build_vm_key(vm_id) values (the zero-padded vm_id), NOT raw
    // ifindexes, so lookups keyed on the ifindex can never match until an
    // ifindex -> rule_key mapping is implemented (e.g. a separate
    // ifindex_map populated at attach time, or carrying the rule_key in
    // skb metadata). Until that mapping exists policy enforcement is a
    // no-op; the mismatch below is a known placeholder, not a bug.
    __u32 vm_id_hash = skb->ifindex; // placeholder: real impl uses ifindex_map
    struct rule_key vm_key = {0};
    __builtin_memcpy(vm_key.id, &vm_id_hash, sizeof(vm_id_hash));

    // Determine direction: egress on TAP = traffic FROM VM, ingress on bridge = TO VM
    __u8 direction = 2; // egress (from VM perspective)
    // Note: when attached as ingress on bridge, flip to 1

    // Check security rules
    int action = match_rules(&vm_key, direction, src_ip, dst_ip,
                             src_port, dst_port, protocol);

    // Check rate limit (only for allowed packets)
    if (action == 1) {
        __u32 pkt_len = skb->len;
        if (!check_rate_limit(&vm_key, pkt_len))
            action = 0; // rate limited -> deny
    }

    // Update stats
    struct stats_entry *stats = bpf_map_lookup_elem(&stats_map, &vm_id_hash);
    if (stats) {
        if (action == 1) {
            __sync_fetch_and_add(&stats->packets_allowed, 1);
            __sync_fetch_and_add(&stats->bytes_allowed, skb->len);
        } else {
            __sync_fetch_and_add(&stats->packets_denied, 1);
            __sync_fetch_and_add(&stats->bytes_denied, skb->len);
        }
    }

    return action == 1 ? TC_ACT_OK : TC_ACT_SHOT;
}

// License required for eBPF programs using kernel helpers
char _license[] __section("license") = "GPL";
