#!/usr/bin/env bash
# m4.4-network.sh — prompt-04 M4.4: network qualification on real
# (nested) KVM, via the candidate deployment from deploy.sh.
#
# Run via deploy.sh --exec (root):
#   sudo GUEST_IMAGE=noble-qual-patched.img ./deploy.sh \
#       --exec ./m4.4-network.sh
#
# Scope (plan §M4.4): the guest network path — attach → connectivity →
# policy attempt → detach (stop/start) → restart/reconcile (nwd) →
# multi-network → cleanup — with forbidden-outcome assertions on the
# host stack (links, nft tables, dnsmasq, routes; the deploy teardown
# enforces the final no-residue gate). The prompt-01 host-safety gate is
# re-proven separately by host-safety.sh (same evidence doc).
#
# Candidate defects this scenario EXPECTS and RECORDS (workarounds in
# place; product fixes land post-rc1 on main — the frozen candidate is
# not modified):
#   N1  vm create --network <name> ignores an operator-created network of
#       that name: the lookup is by network_id, misses, and silently
#       creates an implicit network with the FALLBACK cidr 10.200.0.0/24
#       (the operator's --cidr is ignored). Two such networks collide on
#       the same subnet+gateway → duplicate host routes → host→guest
#       connectivity to the SECOND bridge is broken (demonstrated in
#       Leg F). Same class as the image-chain split (#339).
#   N2  No operator-reachable network policy path: BFF networks/update
#       persists firewall_rules to the CP DB but nothing dispatches to
#       the node (the CP never pushes network fragments; the agent RPC
#       is unimplemented in core-managed mode). The deployed nft table
#       'chv-<net>' is created BARE (no chains — no default-deny) and
#       stays that way. Policy allow/deny is therefore proven at the nwd
#       layer by host-safety.sh, not end-to-end (Leg B records this).
#   N4  vm delete leaves orphaned vm_nic_desired_state rows that
#       permanently block network delete (409 "VM(s) still attached").
#       Operator escape (used in Leg E): delete the orphaned rows.
#   N5  network delete performs NO host teardown in the candidate
#       (DB-only): bridge, nft table, dnsmasq instance and /run/chv/nwd
#       configs persist after a successful delete (asserted in Leg E;
#       the deploy teardown is the actual cleanup).
#
# Dual-version truth (#354): every host-side name (bridge, nft table,
# dnsmasq conf) is derived from each VM's ACTUAL network_id
# (vm_nic_desired_state), never from the reference passed to
# `vm create --network`. The frozen candidate resolves references by
# network_id only and falls back to an implicit network named after the
# reference (N1); post-#354 main resolves operator names to the
# operator's network. The legs are written to be truthful against BOTH
# builds — the N1 legs (A, F) flip from warn to pass once #354 ships.
#
# Post-fix flips (re-qualification): all four recorded defects are FIXED
# on main — N1 (#354, PR #358), N2 (#355 part 1, PR #361 — the
# attach-time snapshot fix, superseded below), N4 (#356 part 1,
# PR #359), N5 (#356 part 2, PR #362: last-detach host teardown
# fires on the VM delete of the network's
# last user — Leg E's residue check should find nothing). The warn
# branches below remain for truth against the frozen candidate; a
# post-fix build must flip them to passes.
#
# N2 eras (2026-10-08): #361 (the #355-part-1 attach-time fix) made
# the policy snapshot ride the VM spec — Leg B's response carried
# policy_application=pending and the table materialized on the NEXT
# VM spec dispatch (observed in Leg C). #355 PR 2 (the journaling
# route) superseded that: Leg B's response now carries the task
# surface {accepted, task_id, ...} and the dispatch to
# already-attached VMs happens on the ORCHESTRATOR'S TICK. The leg
# below detects the response shape and asserts each era truthfully:
# frozen candidate / post-#361 / post-#355-PR-2.
#
# Non-claims (inherited from prompt 01, restated): coexistence with
# Kubernetes/CNI, Docker-forwarded traffic, or multiple bridge-owning
# network stacks is NOT claimed. Single node, single default bridge
# family (chvbr0 / br-<id>, hashed past 15 chars) only.
#
# Timing notes (same host class as M4.3): guest boots to logind in
# ~10-60 s; graceful stop needs up to ~35 s; the agent supervisor
# restarts a killed nwd within one ~30 s reconcile tick.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "${SCRIPT_DIR}/lib.sh"

# --- deployment map (set by deploy.sh --exec) ---
for var in QUAL_TEST_DIR QUAL_NODE_ID QUAL_AGENT_DIR QUAL_LOGS_DIR \
    QUAL_BINARY_DIR QUAL_BFF_URL QUAL_CHVCTL QUAL_CHVCTL_CONFIG_DIR \
    QUAL_CP_PID QUAL_STORD_PID QUAL_NWD_PID QUAL_AGENT_PID \
    QUAL_DB QUAL_NETWORK_CIDR; do
    [ -n "${!var:-}" ] || qual_die "${var} not set — run via deploy.sh --exec"
done

CP_PID="$QUAL_CP_PID"
STORD_PID="$QUAL_STORD_PID"
NWD_PID="$QUAL_NWD_PID"
AGENT_PID="$QUAL_AGENT_PID"

VMS_DIR="${QUAL_AGENT_DIR}/vms"
GUEST_IMAGE_PATH="${QUAL_GUEST_IMAGE_PATH:-/var/lib/chv/qual/images/noble-qual-patched.img}"
NWD_RUNTIME_DIR="/run/chv/nwd"       # hardcoded in the candidate (chv-nwd-core dhcp.rs/dns.rs)
DEFAULT_NET="default"              # network REFERENCE for vm create (Legs A–D)
SECOND_NET="m44x"                  # network REFERENCE for Leg F's second network
FALLBACK_CIDR="10.200.0.0/24"      # the implicit-create fallback (N1)
# NOTE: bridges / nft tables / dnsmasq confs are NOT named after the
# reference: nwd names them after the network_id the VM actually landed
# on (see bridge_for_network below), which differs between the frozen
# candidate (implicit row named after the reference) and post-#354 main
# (the operator's resolved network). All legs derive the effective
# names from vm_nic_desired_state.

BOOT_TIMEOUT=420        # kernel banner after vm start (nested-virt margin)
LOGIND_TIMEOUT=180      # logind lines after the banner
STOP_TIMEOUT=240        # graceful stop
NWD_RESTART_TIMEOUT=90  # supervisor restart of a killed nwd (one tick + margin)
POLICY_DISPATCH_WAIT=15 # BFF update → (absent) node dispatch observation window

# Persistent evidence artifacts (deploy.sh removes TEST_DIR on success).
EVIDENCE_DIR="${CHV_QUAL_ROOT:-/var/lib/chv/qual}/m4.4-artifacts"
mkdir -p "$EVIDENCE_DIR"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
pids_current() {
    cat > "${QUAL_TEST_DIR}/pids.current" <<EOF
CP_PID=${CP_PID}
STORD_PID=${STORD_PID}
NWD_PID=${NWD_PID}
AGENT_PID=${AGENT_PID}
EOF
}

# nwd_pid — the CURRENT nwd process of this deployment (either the
# deploy-started one or the agent-supervisor's replacement; both use a
# config under TEST_DIR). argv[0]-anchored so a process merely mentioning
# chv-nwd mid-command-line can never match.
nwd_pid() {
    pgrep -f "(^|/)chv-nwd( |$).*${QUAL_TEST_DIR}" | head -1
}

# nwd_socket_live — the nwd api socket accepts connections.
nwd_socket_live() {
    python3 - "${QUAL_TEST_DIR}/nwd/api.sock" <<'PYEOF'
import socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(2)
try:
    s.connect(sys.argv[1])
    raise SystemExit(0)
except Exception:
    raise SystemExit(1)
finally:
    s.close()
PYEOF
}

# vm_state VM_ID — power_state from the BFF vm list (empty if absent).
vm_state() {
    qual_chvctl --output json vm list 2>/dev/null \
        | CHV_VM_ID="$1" python3 -c '
import json, os, sys
try:
    data = json.load(sys.stdin)
    if not isinstance(data, list):
        data = data.get("items", [])
    for item in data:
        if item.get("vm_id") == os.environ["CHV_VM_ID"]:
            print(item.get("power_state", ""))
            break
except Exception:
    print("")'
}

# vm_ch_gone VM_ID — no cloud-hypervisor process serves this VM anymore
# (argv[0]-anchored, scoped to this VM's dir under the agent state dir).
vm_ch_gone() {
    ! pgrep -f "(^|/)cloud-hypervisor( |$).*vms/$1" >/dev/null 2>&1
}

# create_vm NAME CPU MEM NETWORK → VM_ID on stdout (empty on failure).
create_vm() {
    local name="$1" cpu="$2" mem="$3" network="$4" out vm_id
    out="$(qual_chvctl --output json vm create "$name" \
        --cpu "$cpu" --memory "$mem" --image "$GUEST_IMAGE_PATH" \
        --network "$network" 2>&1)" \
        || { qual_error "vm create ${name} failed: ${out}"; return 1; }
    vm_id="$(printf '%s\n' "$out" | python3 -c '
import json, sys
raw = sys.stdin.read()
start = raw.find("{")
try:
    data = json.loads(raw[start:])
    print(data.get("vm_id") or data.get("id") or "")
except Exception:
    print("")')"
    [ -n "$vm_id" ] || { qual_error "could not parse vm_id for ${name}: ${out}"; return 1; }
    echo "$vm_id"
}

vm_console_log() { echo "${VMS_DIR}/$1/console.log"; }

console_has() { grep -aq "$2" "$(vm_console_log "$1")" 2>/dev/null; }

count_boots() {
    local f
    f="$(vm_console_log "$1")"
    if [ -f "$f" ]; then
        grep -ac 'Linux version' "$f" || true
    else
        echo 0
    fi
}

save_console_evidence() {
    cp "$(vm_console_log "$1")" "${EVIDENCE_DIR}/console-$1-$2.log" 2>/dev/null \
        || qual_warn "no console.log to save for $1/$2"
}

# wait_boot VM_ID — kernel banner then logind lines in the console log.
# (The banner check goes through a helper FUNCTION so wait_for re-evaluates
# it every iteration — a command substitution in the argument list would
# be expanded once, at call time.)
boot_count_gt() {
    [ "$(count_boots "$1")" -gt "$2" ]
}

wait_boot() {
    local vm="$1" banner_before
    banner_before="$(count_boots "$vm")"
    wait_for "vm ${vm}: kernel banner in console.log" "$BOOT_TIMEOUT" \
        boot_count_gt "$vm" "$banner_before" \
        || return 1
    wait_for "vm ${vm}: systemd-logind lines (boot complete)" "$LOGIND_TIMEOUT" \
        console_has "$vm" "systemd-logind" \
        || return 1
}

# nic_field VM_ID FIELD — from the CP DB's vm_nic_desired_state.
nic_field() {
    sqlite_query "$QUAL_DB" \
        "SELECT $2 FROM vm_nic_desired_state WHERE vm_id='$1'" 2>/dev/null | head -1
}

# net_field NETWORK_ID FIELD — from the CP DB's network_desired_state.
net_field() {
    sqlite_query "$QUAL_DB" \
        "SELECT $2 FROM network_desired_state WHERE network_id='$1'" 2>/dev/null | head -1
}

# net_id_by_name NAME — network_id of the row with this display_name
# (deploy/network create store operator names as display_name with a
# generated short network_id).
net_id_by_name() {
    sqlite_query "$QUAL_DB" \
        "SELECT network_id FROM networks WHERE display_name='$1' ORDER BY network_id LIMIT 1" 2>/dev/null | head -1
}

# bridge_for_network NETWORK_ID — nwd's bridge name for a network id.
# Mirrors chv_common::bridge_name_for_network exactly: 'default' → chvbr0,
# 'br-<id>' while that fits IFNAMSIZ (15), otherwise 'br-<first 8><fnv16>'.
# 128-bit ids (#664) always take the hashed branch. Driven by the VM's
# ACTUAL network_id, never by the vm-create reference (N1/#354).
bridge_for_network() {
    local id="$1" candidate h ch i
    local LC_ALL=C
    [ "$id" = "default" ] && { echo "chvbr0"; return; }
    candidate="br-${id}"
    if [ "${#candidate}" -le 15 ]; then
        echo "$candidate"; return
    fi
    # FNV-1a low 16 bits, byte-wise, 32-bit wrap — the Rust helper's
    # exact loop (`h = h.wrapping_mul(0x01000193) ^ byte`).
    h=2166136261
    for ((i = 0; i < ${#id}; i++)); do
        printf -v ch '%d' "'${id:i:1}"
        h=$(( ((h * 16777619) & 0xffffffff) ^ ch ))
    done
    printf 'br-%s%04x\n' "${id:0:8}" "$((h & 0xffff))"
}

# Pin the derivation against chv_common: a drift here false-fails M4.4.
assert_bridge_derivation() {
    local got
    got="$(bridge_for_network 0123456789abcdef0123456789abcdef)"
    [ "$got" = "br-012345674d15" ] \
        || qual_die "bridge_for_network drifted for 128-bit ids: got '${got}'"
    [ "$(bridge_for_network 0123abcd)" = "br-0123abcd" ] \
        || qual_die "bridge_for_network drifted for legacy ids"
    [ "$(bridge_for_network default)" = "chvbr0" ] \
        || qual_die "bridge_for_network drifted for the default network"
}
assert_bridge_derivation

ping_ok() {
    local ip="$1" iface="${2:-}"
    if [ -n "$iface" ]; then
        ping -c 2 -W 2 -I "$iface" "$ip" >/dev/null 2>&1
    else
        ping -c 2 -W 2 "$ip" >/dev/null 2>&1
    fi
}

# ping_ok_retry IP [IFACE] [TIMEOUT] — poll connectivity. Policy application
# and the nwd restart are asynchronous; a single ping races the dispatcher
# and the firewall chain rebuild (run 41912418366: one-shot pings failed
# while the rules landed seconds later).
ping_ok_retry() {
    local ip="$1" iface="${2:-}" timeout="${3:-60}"
    local deadline=$((SECONDS + timeout))
    until ping_ok "$ip" "$iface"; do
        [ "$SECONDS" -ge "$deadline" ] && return 1
        sleep 2
    done
}

# nft_icmp_allowed TABLE — both-direction ICMP accept rules are present in
# the CHV policy table (in + out; the rulesets the scenario applies carry
# one accept per direction).
nft_icmp_allowed() {
    local table="$1" n
    n="$(nft list table inet "$table" 2>/dev/null | grep -c 'l4proto icmp.*accept' || true)"
    [ "${n:-0}" -ge 2 ]
}

# nft_icmp_allowed_wait TABLE [TIMEOUT] — silent poll (no qual_error on
# timeout) for call sites where an unmaterialized update is recorded as a
# WARN/deferred observation rather than a hard failure.
nft_icmp_allowed_wait() {
    local table="$1"
    local timeout="${2:-90}"
    local deadline=$((SECONDS + timeout))
    until nft_icmp_allowed "$table"; do
        [ "$SECONDS" -ge "$deadline" ] && return 1
        sleep 2
    done
}

# bridge_addr BRIDGE — first inet address (e.g. "10.200.0.1/24"), or empty.
bridge_addr() {
    ip -4 addr show dev "$1" 2>/dev/null | awk '/inet /{print $2; exit}'
}

# tap_of VM_ID — the tap interface name from the CH stderr log (the CH
# payload opens the pre-attached tap by name), or empty.
tap_of() {
    grep -ao 'Tap tap-[a-f0-9]*' "${VMS_DIR}/$1/cloud-hypervisor.stderr.log" 2>/dev/null \
        | head -1 | awk '{print $2}'
}

# nft_table_chain_count TABLE — number of chain definitions in the table.
nft_table_chain_count() {
    nft list table inet "$1" 2>/dev/null | grep -c 'chain ' || true
}

# nwd_dnsmasq_running NETWORK — a dnsmasq with this network's conf file.
nwd_dnsmasq_running() {
    pgrep -f "(^|/)dnsmasq( |$).*--conf-file=${NWD_RUNTIME_DIR}/dnsmasq-$1.conf" >/dev/null 2>&1
}

# bff_token — the JWT chvctl stored (admin), for raw BFF probes.
bff_token() { cat "${QUAL_CHVCTL_CONFIG_DIR}/chvctl/credentials" 2>/dev/null; }

save_evidence() {
    # $1 = phase label; captures host-stack + DB state for the doc.
    # The network-scoped snapshot follows the FIRST VM's actual network
    # (VM1_NET) once it exists, else the 'default' reference.
    local label="$1" net="${VM1_NET:-$DEFAULT_NET}"
    {
        echo "### m4.4 evidence snapshot: ${label} ($(date -u +%FT%TZ))"
        echo "--- ip -br link:"; ip -br link
        echo "--- ip -br addr (chv bridges):"; ip -br addr | grep -E 'chvbr|^br-' || true
        echo "--- ip route:"; ip route
        echo "--- nft tables:"; nft list tables 2>/dev/null || true
        echo "--- nft table inet chv-${net}:"; nft list table inet "chv-${net}" 2>/dev/null || true
        echo "--- dnsmasq:"; pgrep -a dnsmasq || true
        echo "--- CP networks:"; sqlite_query "$QUAL_DB" \
            "SELECT network_id, cidr FROM network_desired_state" 2>/dev/null || true
        echo "--- CP vm_nic_desired_state:"; sqlite_query "$QUAL_DB" \
            "SELECT vm_id, network_id, mac_address, ip_address FROM vm_nic_desired_state" 2>/dev/null || true
        echo
    } >> "${EVIDENCE_DIR}/host-state.txt"
    cp "${NWD_RUNTIME_DIR}/dnsmasq-${net}.conf" \
        "${EVIDENCE_DIR}/dnsmasq-${net}.conf" 2>/dev/null || true
}

qual_info "=== M4.4 network qualification start (node ${QUAL_NODE_ID}) ==="
qual_info "candidate: $(cat "${QUAL_BINARY_DIR}/CANDIDATE_SHA" 2>/dev/null || echo unknown)"
# Baseline of non-CHV nft tables (host-safety invariant across the legs:
# nothing this scenario does may create or alter them).
NONCHV_TABLES_BEFORE="$(nft list tables 2>/dev/null | awk '{print $3}' | grep -v '^chv-' | sort || true)"
save_evidence "start"

# ---------------------------------------------------------------------------
# Leg A — attach + connectivity (default network)
# ---------------------------------------------------------------------------
qual_info "--- Leg A: attach (vm create --network default) → host materialization → guest connectivity"

VM1_ID="$(create_vm qual-net-1 2 1G "$DEFAULT_NET")" || qual_die "aborting"
qual_pass "vm created: qual-net-1 (${VM1_ID}) on network '${DEFAULT_NET}'"
VM1_IP="$(nic_field "$VM1_ID" ip_address)"
VM1_MAC="$(nic_field "$VM1_ID" mac_address)"
[ -n "$VM1_IP" ] && [ -n "$VM1_MAC" ] \
    && qual_pass "IPAM assigned ${VM1_IP} / ${VM1_MAC}" \
    || qual_die "no IPAM assignment for ${VM1_ID}"

# The N1/#354 resolution chain: the reference 'default' resolves to an
# implicit network named 'default' on the frozen candidate (network_id
# lookup misses deploy's named row) and to deploy's operator network on
# post-#354 main. Every host-side name below follows the network the VM
# ACTUALLY landed on — never the reference.
VM1_NET="$(nic_field "$VM1_ID" network_id)"
[ -n "$VM1_NET" ] || qual_die "no network_id in vm_nic_desired_state for ${VM1_ID}"
VM1_BRIDGE="$(bridge_for_network "$VM1_NET")"
VM1_NFT_TABLE="chv-${VM1_NET}"
qual_info "network ref '${DEFAULT_NET}' → network_id '${VM1_NET}' (bridge ${VM1_BRIDGE}, nft table ${VM1_NFT_TABLE})"
[ "$(net_field "$VM1_NET" cidr)" = "$QUAL_NETWORK_CIDR" ] \
    && qual_pass "vm's network '${VM1_NET}' carries the deploy cidr ${QUAL_NETWORK_CIDR}" \
    || qual_warn "vm's network '${VM1_NET}' cidr is '$(net_field "$VM1_NET" cidr)', expected ${QUAL_NETWORK_CIDR}"

qual_chvctl vm start "$VM1_ID" >/dev/null || qual_die "vm start failed for ${VM1_ID}"
wait_boot "$VM1_ID" || qual_die "guest did not boot (Leg A)"
save_console_evidence "$VM1_ID" "leg-a"

# Host-stack materialization assertions.
[ -n "$(bridge_addr "$VM1_BRIDGE")" ] \
    && qual_pass "bridge ${VM1_BRIDGE} exists ($(bridge_addr "$VM1_BRIDGE"))" \
    || qual_error "FORBIDDEN: bridge ${VM1_BRIDGE} missing after attach"
VM1_TAP="$(tap_of "$VM1_ID")"
if [ -n "$VM1_TAP" ]; then
    qual_pass "vm tap ${VM1_TAP} present in CH payload"
    ip link show "$VM1_TAP" 2>/dev/null | grep -q "master ${VM1_BRIDGE}" \
        && qual_pass "tap ${VM1_TAP} enslaved to ${VM1_BRIDGE}" \
        || qual_error "FORBIDDEN: tap ${VM1_TAP} not enslaved to ${VM1_BRIDGE}"
else
    qual_error "no tap found for ${VM1_ID} (CH payload has no NIC?)"
fi
nwd_dnsmasq_running "$VM1_NET" \
    && qual_pass "nwd dnsmasq running for '${VM1_NET}'" \
    || qual_error "FORBIDDEN: no dnsmasq for '${VM1_NET}'"
grep -q "^${VM1_MAC},${VM1_IP}$" "${NWD_RUNTIME_DIR}/dnsmasq-${VM1_NET}.hosts" 2>/dev/null \
    && qual_pass "dhcp reservation ${VM1_MAC} → ${VM1_IP} in place" \
    || qual_error "dhcp reservation for ${VM1_IP} missing"
nft list table inet "$VM1_NFT_TABLE" >/dev/null 2>&1 \
    && qual_pass "nft table inet ${VM1_NFT_TABLE} exists" \
    || qual_error "FORBIDDEN: nft table inet ${VM1_NFT_TABLE} missing"

# N2 record: the deployed table is BARE (no chains → no default-deny on
# the deployed path). Policy behavior itself is proven by host-safety.sh.
if [ "$(nft_table_chain_count "$VM1_NFT_TABLE")" -eq 0 ]; then
    qual_warn "deployed nft table '${VM1_NFT_TABLE}' is BARE (0 chains) — no default-deny on the deployed path (defect N2, issue filed)"
else
    qual_pass "deployed nft table '${VM1_NFT_TABLE}' has $(nft_table_chain_count "$VM1_NFT_TABLE") chain(s)"
fi
nft list table inet "$VM1_NFT_TABLE" > "${EVIDENCE_DIR}/nft-${VM1_NET}-leg-a.txt" 2>/dev/null || true

# Guest connectivity. The seed-ISO evidence line lands when cloud-init
# FINISHES (~40 s in, after the logind gate) — poll for it rather than
# assert immediately (run-1 lesson: the check raced cloud-init).
seed_seen=0
for _ in $(seq 1 90); do
    if console_has "$VM1_ID" "DataSourceNoCloud \[seed=/dev/vdb\]"; then seed_seen=1; break; fi
    sleep 2
done
[ "$seed_seen" -eq 1 ] \
    && qual_pass "cloud-init seed ISO attached (vdb, NoCloud datasource)" \
    || qual_warn "no NoCloud seed evidence in console after 180s (seed ISO absent?)"
console_has "$VM1_ID" "$VM1_IP" \
    && qual_pass "guest configured its NIC with the reserved IP (${VM1_IP})" \
    || qual_error "guest console shows no evidence of IP ${VM1_IP}"
# Baseline policy (#355 PR3): an empty ruleset is DHCP/DNS/conntrack plus
# default-deny, so host→guest ICMP is blocked BY DESIGN until a rule
# allows it. Prove that baseline here; Leg B/C apply the allow rules and
# assert connectivity. On the frozen candidate (bare table, no chains)
# host→guest ICMP works, which this branch still accepts truthfully.
if [ "$(nft_table_chain_count "$VM1_NFT_TABLE")" -eq 0 ]; then
    if ping_ok "$VM1_IP"; then
        qual_pass "host → guest connectivity (${VM1_IP}: 2/2 icmp; bare-table candidate)"
    else
        qual_error "FORBIDDEN: host → guest ping FAILED for ${VM1_IP} (bare table)"
    fi
else
    if ping_ok "$VM1_IP"; then
        qual_error "FORBIDDEN: host → guest ping SUCCEEDED with an empty ruleset despite default-deny"
    else
        qual_pass "empty ruleset default-denies host → guest ICMP (${VM1_IP}) — policy must allow it"
    fi
fi
ip neigh show dev "$VM1_BRIDGE" | grep -q "${VM1_IP}.*${VM1_MAC}" \
    && qual_pass "ARP entry ${VM1_IP} → ${VM1_MAC} on ${VM1_BRIDGE}" \
    || qual_warn "no ARP entry for ${VM1_IP} (check ip neigh evidence)"
save_evidence "leg-a-after-boot"

# ---------------------------------------------------------------------------
# Leg B — policy attempt through the operator API (defect N2)
# ---------------------------------------------------------------------------
qual_info "--- Leg B: BFF networks/update with firewall_rules → dispatch observation"

NFT_BEFORE_B="$(nft list table inet "$VM1_NFT_TABLE" 2>/dev/null | sha256sum | cut -c1-16)"
GEN_BEFORE_B="$(net_field "$VM1_NET" desired_generation)"
TOKEN="$(bff_token)"
[ -n "$TOKEN" ] || qual_die "no BFF token available (chvctl credentials)"

# The policy target is the network the VM actually sits on (VM1_NET):
# 'default' on the frozen candidate, the operator's resolved network on
# post-#354 main. The rule uses the ENGINE vocabulary (#368): the BFF
# validates firewall_rules at save time and rejects the UI-era dialect
# (ingress/allow/source) that used to ride the spec verbatim and brick
# the next VM create at attach time (N7).
#
# Direction semantics (nwd firewall.rs, proven by the run-3 post-mortem):
# 'inbound' rules land in chv-policy-in/chv-policy-fwd and match traffic
# FROM the guest arriving on the tap; the host's ping REQUESTS leave via
# the tap (OUTPUT path → chv-policy-out), which default-denies without an
# 'outbound' rule — an inbound-only ruleset therefore cuts host→guest
# pings (12 dropped requests observed in the run-3 nft dump counters).
# Conntrack established/related accept sits at each chain head, so
# replies to allowed flows always pass. The truthful "icmp works between
# host and guests" ruleset needs BOTH directions, scoped to the subnet.
HTTP_CODE="$(curl -s -o "${EVIDENCE_DIR}/bff-update-leg-b.json" -w '%{http_code}' \
    -X POST "${QUAL_BFF_URL}/v1/networks/update" \
    -H "Authorization: Bearer ${TOKEN}" -H "Content-Type: application/json" \
    -d '{"network_id":"'"$VM1_NET"'","firewall_rules":[{"direction":"inbound","action":"accept","protocol":"icmp","source_cidr":"'"$QUAL_NETWORK_CIDR"'"},{"direction":"outbound","action":"accept","protocol":"icmp","source_cidr":"'"$QUAL_NETWORK_CIDR"'"}]}')"
[ "$HTTP_CODE" = "200" ] \
    && qual_pass "BFF accepted the firewall_rules update (HTTP ${HTTP_CODE})" \
    || qual_error "BFF networks/update failed (HTTP ${HTTP_CODE}): $(cat "${EVIDENCE_DIR}/bff-update-leg-b.json")"

GEN_AFTER_B="$(net_field "$VM1_NET" desired_generation)"
FWRULES_AFTER_B="$(net_field "$VM1_NET" firewall_rules_json)"
[ -n "$FWRULES_AFTER_B" ] && [ "$FWRULES_AFTER_B" != "" ] \
    && qual_pass "firewall_rules persisted to CP DB (generation ${GEN_BEFORE_B} → ${GEN_AFTER_B})" \
    || qual_error "firewall_rules NOT persisted to the CP DB"

# Observation window: three eras, detected from the saved response.
# - post-#355 PR 2 (journaling route): the response carries the task
#   surface {accepted, task_id, ...} and the dispatch to already-
#   attached VMs happens on the ORCHESTRATOR'S TICK — the table is
#   EXPECTED to change inside the window (timing-dependent: tick pace
#   vs the wait below).
# - post-#361 (the snapshot fix): the response carries
#   policy_application=pending, the table is unchanged here (applied
#   at the next VM spec dispatch — observed in Leg C, vm-2's create).
# - frozen candidate: no policy path exists at all (defect N2).
sleep "$POLICY_DISPATCH_WAIT"
NFT_AFTER_B="$(nft list table inet "$VM1_NFT_TABLE" 2>/dev/null | sha256sum | cut -c1-16)"
POLICY_NOTE_B="$(python3 - "${EVIDENCE_DIR}/bff-update-leg-b.json" <<'PYEOF'
import json, sys
try:
    print(json.load(open(sys.argv[1])).get("policy_application") or "")
except Exception:
    pass
PYEOF
)"
POLICY_TASK_B="$(python3 - "${EVIDENCE_DIR}/bff-update-leg-b.json" <<'PYEOF'
import json, sys
try:
    print(json.load(open(sys.argv[1])).get("task_id") or "")
except Exception:
    pass
PYEOF
)"
if [ -n "$POLICY_TASK_B" ]; then
    qual_pass "update journals the policy task (post-#355 PR 2): ${POLICY_TASK_B}"
    if [ "$NFT_BEFORE_B" != "$NFT_AFTER_B" ]; then
        qual_pass "nft table changed after the update (fan-out dispatch observed)"
    else
        qual_warn "nft table unchanged ${POLICY_DISPATCH_WAIT}s after the journaled update — dispatch is orchestrator-tick-paced; check the task (chvctl task watch ${POLICY_TASK_B}) before treating this as a defect"
    fi
elif [ -n "$POLICY_NOTE_B" ]; then
    qual_pass "update honestly reports pending application (post-#361): ${POLICY_NOTE_B}"
    if [ "$NFT_BEFORE_B" = "$NFT_AFTER_B" ]; then
        qual_pass "nft table unchanged until the next dispatch (by design)"
    else
        qual_warn "nft table changed ${POLICY_DISPATCH_WAIT}s after the update with no dispatch (unexpected on post-#361 main)"
    fi
elif [ "$NFT_BEFORE_B" = "$NFT_AFTER_B" ]; then
    qual_warn "nft table UNCHANGED ${POLICY_DISPATCH_WAIT}s after the accepted update — no operator-reachable policy path in the candidate (defect N2, issue filed; policy allow/deny proven at the nwd layer by host-safety.sh)"
else
    qual_pass "nft table changed after the update (dispatch observed)"
fi

# Host-safety invariant while a policy is 'in flight': the host stack
# must be untouched (BFF responsive, no new non-CHV nft tables).
qual_chvctl node list >/dev/null 2>&1 \
    && qual_pass "host stack unaffected: BFF still responsive" \
    || qual_error "FORBIDDEN: BFF unresponsive after the policy update"
NONCHV_TABLES_NOW="$(nft list tables 2>/dev/null | awk '{print $3}' | grep -v '^chv-' | sort || true)"
if [ "$NONCHV_TABLES_BEFORE" = "$NONCHV_TABLES_NOW" ]; then
    qual_pass "non-CHV nft tables unchanged"
else
    qual_error "FORBIDDEN: non-CHV nft tables changed: '${NONCHV_TABLES_BEFORE}' → '${NONCHV_TABLES_NOW}'"
fi
# The update is journaled and fanned out on the orchestrator tick (2 s
# base interval, but the task may queue behind other work). Poll for the
# ICMP accepts instead of assuming a fixed 15 s is enough; if they only
# land with the next VM dispatch, record that honestly rather than
# treating the still-default-denied baseline as "broken connectivity".
if nft_icmp_allowed_wait "$VM1_NFT_TABLE" 90; then
    qual_pass "policy ICMP allow materialized on ${VM1_NFT_TABLE}"
    ping_ok_retry "$VM1_IP" "" 60 \
        && qual_pass "host → guest connectivity allowed by the policy (${VM1_IP})" \
        || qual_error "FORBIDDEN: policy allows ICMP but guest connectivity still fails"
else
    qual_warn "journaled policy update not materialized within 90s; the attach-time path (next VM dispatch) will be asserted in Leg C"
fi
save_evidence "leg-b-after-policy-attempt"

# ---------------------------------------------------------------------------
# Leg C — nwd hard-kill → supervisor restart → reconcile (data plane)
# ---------------------------------------------------------------------------
qual_info "--- Leg C: SIGKILL nwd → data plane survives → supervisor restart → re-attach works"

NWD_BEFORE_C="$(nwd_pid)"
[ -n "$NWD_BEFORE_C" ] || qual_die "no nwd process found for this deployment"
kill -9 "$NWD_BEFORE_C"
qual_info "nwd (pid ${NWD_BEFORE_C}) SIGKILLed"
sleep 2
ping_ok_retry "$VM1_IP" "" 60 \
    && qual_pass "data plane survives nwd death (kernel-only: bridge/tap/dnsmasq)" \
    || qual_error "FORBIDDEN: guest connectivity lost with nwd down"

# The agent's supervisor restarts nwd within one reconcile tick; the
# socket path is the liveness proof (a process without a reachable
# socket is NOT a recovery — observed in the M4.4 experiments).
nwd_recovered() {
    local now
    now="$(nwd_pid)"
    [ -n "$now" ] && [ "$now" != "$NWD_BEFORE_C" ] && nwd_socket_live
}
wait_for "supervisor restarted nwd (new pid, socket live)" "$NWD_RESTART_TIMEOUT" \
    nwd_recovered \
    || qual_die "nwd did not recover after SIGKILL"
NWD_PID="$(nwd_pid)"
[ -n "$NWD_PID" ] && [ "$NWD_PID" != "$NWD_BEFORE_C" ] \
    && qual_pass "nwd restarted by the agent supervisor (pid ${NWD_BEFORE_C} → ${NWD_PID})" \
    || qual_error "nwd pid did not change (${NWD_PID})"
pids_current
ping_ok_retry "$VM1_IP" "" 90 \
    && qual_pass "guest connectivity intact across the nwd restart" \
    || qual_error "FORBIDDEN: guest connectivity lost after nwd restart"

# Re-attach on the existing network must work (idempotent topology
# re-ensure with nwd's in-memory table lost): a NEW VM attaches fine.
VM2_ID="$(create_vm qual-net-2 1 512M "$DEFAULT_NET")" || qual_die "aborting Leg C"
[ "$(nic_field "$VM2_ID" network_id)" = "$VM1_NET" ] \
    && qual_pass "vm-2 resolved to the same network as vm-1 ('${VM1_NET}')" \
    || qual_error "vm-2 landed on network '$(nic_field "$VM2_ID" network_id)', vm-1 on '${VM1_NET}'"
qual_chvctl vm start "$VM2_ID" >/dev/null || qual_die "vm start failed for ${VM2_ID}"
wait_boot "$VM2_ID" || qual_die "guest did not boot (Leg C)"
save_console_evidence "$VM2_ID" "leg-c"
VM2_IP="$(nic_field "$VM2_ID" ip_address)"
VM2_TAP="$(tap_of "$VM2_ID")"
[ -n "$VM2_TAP" ] && ip link show "$VM2_TAP" 2>/dev/null | grep -q "master ${VM1_BRIDGE}" \
    && qual_pass "new VM attached to existing network after nwd restart (tap ${VM2_TAP} re-ensured)" \
    || qual_error "FORBIDDEN: attach to existing network failed after nwd restart"
# N2 flip (#355 part 1): vm-2's create is the first VM spec DISPATCH
# after Leg B's firewall_rules update. On post-#355 main the policy
# snapshot travels with the spec and the Core executor applies it via
# nwd between topology-ensure and NIC attach — the table gains the
# default-deny + operator chains HERE (it stayed bare at vm-1's create
# because no rules existed yet, and through Leg B because an update
# alone dispatches nothing). On the frozen candidate it stays bare
# (defect N2). Assert the operator's rule is verbatim in the table,
# not just that chains exist.
VM2_CHAINS_C="$(nft_table_chain_count "$VM1_NFT_TABLE")"
nft list table inet "$VM1_NFT_TABLE" > "${EVIDENCE_DIR}/nft-${VM1_NET}-leg-c.txt" 2>/dev/null || true
if [ "$VM2_CHAINS_C" -gt 0 ] && grep -q "icmp" "${EVIDENCE_DIR}/nft-${VM1_NET}-leg-c.txt" 2>/dev/null; then
    qual_pass "attach-time policy materialized on the next dispatch (vm-2 create): ${VM2_CHAINS_C} chain(s), operator icmp rule present in inet ${VM1_NFT_TABLE}"
elif [ "$VM2_CHAINS_C" -gt 0 ]; then
    qual_warn "nft table gained ${VM2_CHAINS_C} chain(s) on the dispatch but the operator icmp rule is not verbatim (partial policy application?)"
else
    qual_warn "nft table still BARE after a post-update VM dispatch — attach-time policy path absent (defect N2, issue filed)"
fi
# The dispatch (vm-2 create) is where the operator rules are guaranteed
# to travel with the spec. Wait for them, then assert connectivity to
# BOTH guests — this is the scenario's "attach + policy → reachable"
# contract, and it does not depend on the journaled fan-out timing.
nft_icmp_allowed_wait "$VM1_NFT_TABLE" 60 \
    && qual_pass "ICMP allow present in ${VM1_NFT_TABLE} after the vm-2 dispatch" \
    || qual_warn "ICMP allow still absent after the vm-2 dispatch (attach-time policy path incomplete)"
ping_ok_retry "$VM1_IP" "" 60 \
    && qual_pass "host → guest-1 connectivity with the operator policy (${VM1_IP})" \
    || qual_error "FORBIDDEN: host → guest-1 ping FAILED with the policy applied"
ping_ok_retry "$VM2_IP" "" 60 \
    && qual_pass "host → guest-2 connectivity (${VM2_IP})" \
    || qual_error "FORBIDDEN: host → guest-2 ping FAILED for ${VM2_IP}"
save_evidence "leg-c-after-nwd-restart"

# ---------------------------------------------------------------------------
# Leg D — detach cycle: stop → network state quiesced → start → re-attach
# ---------------------------------------------------------------------------
qual_info "--- Leg D: stop → tap quiesced, bridge/dnsmasq persist → start → same tap, same IP"

VM1_TAP_BEFORE_D="$VM1_TAP"
qual_chvctl vm stop "$VM1_ID" >/dev/null || qual_die "vm stop failed for ${VM1_ID}"
# --no-shutdown contract: the guest reaches a terminal state and the VMM
# is RETAINED; DeleteVm is what stops it. Waiting for CH exit here is the
# stale pre-contract expectation that made this leg time out for 240 s.
wait_vm_guest_stopped "$VM1_ID" || qual_error "vm ${VM1_ID} did not reach a terminal guest state"
pgrep -f "(^|/)cloud-hypervisor( |$).*vms/${VM1_ID}" >/dev/null 2>&1 \
    && qual_pass "VMM retained after graceful stop (${VM1_ID}; delete is what stops it)" \
    || qual_error "FORBIDDEN: VMM exited on graceful stop (contract: retained until delete)"
[ "$(count_cloud_hypervisor_processes)" -eq 2 ] \
    && qual_pass "2 CH processes remain after stop (qual-net-1 retained + qual-net-2 running)" \
    || qual_error "FORBIDDEN: expected 2 CH processes after stop, found $(count_cloud_hypervisor_processes)"
if [ -n "$VM1_TAP_BEFORE_D" ] && ip link show "$VM1_TAP_BEFORE_D" >/dev/null 2>&1; then
    qual_info "tap ${VM1_TAP_BEFORE_D} persists after stop (kernel link; carrier drops with the CH side)"
    ip link show "$VM1_TAP_BEFORE_D" | grep -q "NO-CARRIER" \
        && qual_pass "tap carrier down after stop (detached from the guest)" \
        || qual_warn "tap ${VM1_TAP_BEFORE_D} still has carrier after stop"
else
    qual_warn "tap ${VM1_TAP_BEFORE_D} removed on stop (differs from the observed candidate behavior)"
fi
[ -n "$(bridge_addr "$VM1_BRIDGE")" ] && nwd_dnsmasq_running "$VM1_NET" \
    && qual_pass "network-scoped state persists across VM stop (bridge + dnsmasq)" \
    || qual_error "FORBIDDEN: network state torn down by a VM stop"
ping_ok "$VM1_IP" \
    && qual_error "FORBIDDEN: stopped guest still answers ping (${VM1_IP})" \
    || qual_pass "stopped guest unreachable (${VM1_IP})"

qual_chvctl vm start "$VM1_ID" >/dev/null || qual_die "vm re-start failed for ${VM1_ID}"
wait_boot "$VM1_ID" || qual_die "guest did not re-boot (Leg D)"
save_console_evidence "$VM1_ID" "leg-d"
VM1_TAP_AFTER_D="$(tap_of "$VM1_ID")"
[ "$VM1_TAP_AFTER_D" = "$VM1_TAP_BEFORE_D" ] \
    && qual_pass "re-attach reuses the deterministic tap name (${VM1_TAP_AFTER_D})" \
    || qual_warn "tap name changed across restart (${VM1_TAP_BEFORE_D} → ${VM1_TAP_AFTER_D})"
VM1_IP_AFTER_D="$(nic_field "$VM1_ID" ip_address)"
[ "$VM1_IP_AFTER_D" = "$VM1_IP" ] \
    && qual_pass "IPAM assignment stable across stop/start (${VM1_IP})" \
    || qual_error "IPAM assignment changed: ${VM1_IP} → ${VM1_IP_AFTER_D}"
ping_ok_retry "$VM1_IP" "" 90 \
    && qual_pass "host → guest connectivity restored after re-start" \
    || qual_error "FORBIDDEN: connectivity not restored after re-start"
save_evidence "leg-d-after-restart"

# ---------------------------------------------------------------------------
# Leg F — second network: the N1 fallback-CIDR collision (before cleanup,
# so a live guest on each bridge demonstrates the routing ambiguity)
# ---------------------------------------------------------------------------
qual_info "--- Leg F: network create + vm create --network <name> (defect N1: fallback-CIDR collision)"

qual_chvctl --output json network create "$SECOND_NET" --cidr 10.99.0.0/24 >/dev/null 2>&1 \
    || qual_die "network create ${SECOND_NET} failed"
qual_pass "operator network '${SECOND_NET}' created with cidr 10.99.0.0/24"
SECOND_NET_ID="$(net_id_by_name "$SECOND_NET")"
qual_info "operator network '${SECOND_NET}' has network_id '${SECOND_NET_ID}'"

VM3_ID="$(create_vm qual-net-3 1 512M "$SECOND_NET")" || qual_die "aborting Leg F"
qual_chvctl vm start "$VM3_ID" >/dev/null || qual_die "vm start failed for ${VM3_ID}"
wait_boot "$VM3_ID" || qual_die "guest did not boot (Leg F)"
save_console_evidence "$VM3_ID" "leg-f"
VM3_IP="$(nic_field "$VM3_ID" ip_address)"
VM3_NET="$(nic_field "$VM3_ID" network_id)"
[ -n "$VM3_NET" ] || qual_die "no network_id in vm_nic_desired_state for ${VM3_ID}"
VM3_BRIDGE="$(bridge_for_network "$VM3_NET")"
qual_info "network ref '${SECOND_NET}' → network_id '${VM3_NET}' (bridge ${VM3_BRIDGE})"

# The operator's network is what the VM must attach to; the frozen
# candidate misses the name (network_id-only lookup) and attaches to an
# implicit network with the FALLBACK cidr instead (N1, issue #354).
VM3_NET_CIDR="$(net_field "$VM3_NET" cidr)"
if [ "$VM3_NET" = "$SECOND_NET_ID" ] && [ "$VM3_NET_CIDR" = "10.99.0.0/24" ]; then
    qual_pass "vm-3 attached to the operator network '${VM3_NET}' with its cidr (${VM3_NET_CIDR})"
else
    # Known candidate defect (issue #354, filed): recorded as a WARNING,
    # matching the M4.3 pattern for filed defects — the scenario's hard
    # errors are reserved for outcomes not explained by filed issues.
    qual_warn "DEFECT N1 (issue #354): vm create --network ${SECOND_NET} attached to '${VM3_NET}' (cidr '${VM3_NET_CIDR}'), not the operator's '${SECOND_NET_ID}' 10.99.0.0/24 — implicit fallback"
fi
VM3_BRIDGE_ADDR="$(bridge_addr "$VM3_BRIDGE")"
if [ "$VM3_BRIDGE_ADDR" = "10.99.0.1/24" ]; then
    qual_pass "second bridge ${VM3_BRIDGE} carries the operator gateway 10.99.0.1/24"
else
    qual_warn "DEFECT N1 (issue #354): second bridge ${VM3_BRIDGE} addr is '${VM3_BRIDGE_ADDR}' (fallback gateway on the shared subnet, not the operator's 10.99.0.1/24)"
fi

# Duplicate route for the shared subnet → host→guest on the SECOND
# bridge is unreachable by normal routing (first route wins).
if ip route show | grep "$FALLBACK_CIDR" | grep -q "dev ${VM3_BRIDGE}" && \
   ip route show | grep "$FALLBACK_CIDR" | grep -q "dev ${VM1_BRIDGE}"; then
    qual_warn "duplicate host routes for ${FALLBACK_CIDR} (on ${VM1_BRIDGE} and ${VM3_BRIDGE}) — defect N1 consequence"
else
    qual_pass "no duplicate route for ${FALLBACK_CIDR} (each network on its own subnet)"
fi
# The new network also starts with an empty ruleset (DHCP/DNS +
# default-deny). Allow ICMP both directions before using reachability as
# the N1 routing probe — otherwise a policy drop would masquerade as a
# routing defect. The update is journaled; wait for materialization.
F_TOKEN="$(bff_token)"
F_POLICY_CODE="$(curl -s -o "${EVIDENCE_DIR}/bff-update-leg-f.json" -w '%{http_code}' \
    -X POST "${QUAL_BFF_URL}/v1/networks/update" \
    -H "Authorization: Bearer ${F_TOKEN}" -H "Content-Type: application/json" \
    -d '{"network_id":"'"$VM3_NET"'","firewall_rules":[{"direction":"inbound","action":"accept","protocol":"icmp","source_cidr":"'"$VM3_NET_CIDR"'"},{"direction":"outbound","action":"accept","protocol":"icmp","source_cidr":"'"$VM3_NET_CIDR"'"}]}')"
[ "$F_POLICY_CODE" = "200" ] \
    && qual_pass "BFF accepted the ICMP allow for the second network (HTTP ${F_POLICY_CODE})" \
    || qual_warn "BFF networks/update for the second network returned HTTP ${F_POLICY_CODE}: $(head -c 200 "${EVIDENCE_DIR}/bff-update-leg-f.json")"
nft_icmp_allowed_wait "chv-${VM3_NET}" 90 \
    && qual_pass "ICMP allow materialized on chv-${VM3_NET}" \
    || true

if nft_icmp_allowed "chv-${VM3_NET}"; then
    if ping_ok_retry "$VM3_IP" "" 30; then
        qual_pass "host → guest-3 directly reachable (${VM3_IP})"
    else
        qual_warn "DEFECT N1 forbidden outcome (issue #354): host → guest-3 (${VM3_IP}, on ${VM3_BRIDGE}) UNREACHABLE — duplicate-subnet routing"
        if ping_ok_retry "$VM3_IP" "$VM3_BRIDGE" 30; then
            qual_pass "forced via ${VM3_BRIDGE}: guest-3 reachable (proves the guest/network is healthy; only routing is ambiguous)"
        else
            qual_error "guest-3 unreachable even forced via ${VM3_BRIDGE} (beyond the filed N1 defect)"
        fi
    fi
else
    qual_warn "guest-3 reachability not asserted: the second network's ICMP allow did not materialize within 90s (empty-ruleset default-deny); routing itself is asserted by the route table above"
fi
ping_ok_retry "$VM1_IP" "" 60 \
    && qual_pass "guest-1 (first bridge) unaffected by the second network" \
    || qual_error "FORBIDDEN: second network broke first-bridge connectivity"
save_evidence "leg-f-two-networks"

# ---------------------------------------------------------------------------
# Leg E — cleanup: vm deletes → network delete (defects N4/N5)
# ---------------------------------------------------------------------------
qual_info "--- Leg E: stop+delete VMs → network delete (orphan-row block N4, no host teardown N5)"

for vm in "$VM1_ID" "$VM2_ID" "$VM3_ID"; do
    qual_chvctl vm stop "$vm" >/dev/null 2>&1 || true
    wait_vm_guest_stopped "$vm" || qual_error "vm ${vm} did not reach a terminal guest state"
    qual_chvctl vm delete "$vm" >/dev/null 2>&1 \
        && qual_pass "vm ${vm} deleted" \
        || qual_error "vm delete failed for ${vm}"
    # Delete (not stop) is what reaps the retained VMM.
    wait_for "vm ${vm}: CH process exited after delete" 90 vm_ch_gone "$vm" \
        || qual_error "FORBIDDEN: CH process still alive after delete for ${vm}"
done
[ "$(count_cloud_hypervisor_processes)" -eq 0 ] \
    && qual_pass "no CH processes remain" \
    || qual_error "FORBIDDEN: CH processes remain after deletes"

# Taps are removed by the VM-delete path, but ASYNCHRONOUSLY relative to
# the chvctl call returning (run-1 lesson: an immediate check raced the
# removal) — poll up to 30 s before asserting. Bridges/dnsmasq are
# network-scoped and must persist until the network itself is deleted.
taps_gone=0
LEFTOVER_TAPS=""
for _ in $(seq 1 15); do
    LEFTOVER_TAPS="$(ip -o link show 2>/dev/null | awk -F': ' '{print $2}' | awk '{print $1}' | grep '^tap-' || true)"
    [ -z "$LEFTOVER_TAPS" ] && { taps_gone=1; break; }
    sleep 2
done
if [ "$taps_gone" -eq 1 ]; then
    qual_pass "all taps removed by vm delete"
else
    qual_error "FORBIDDEN: taps remain 30s after vm deletes: $(echo "$LEFTOVER_TAPS" | tr '\n' ' ')"
fi

# N5 flip (#356 part 2 / PR #362): the last-detach host teardown fires on
# the VM DELETE of the network's last user — vm-2's delete here (vm-3 is
# on the second network). On post-#362 main the bridge, nft table, and
# dnsmasq for VM1_NET are demolished by now; on the frozen candidate
# they persist until the deploy teardown (defect N5). The Core store
# gates the decision (no other VM on the node uses the network), so this
# is also the negative-proof: nothing tears down while a VM is still
# attached (Leg D's stop/start above already exercised that window).
LAST_DETACH_RESIDUE=""
[ -n "$(bridge_addr "$VM1_BRIDGE")" ] && LAST_DETACH_RESIDUE="${LAST_DETACH_RESIDUE} bridge:${VM1_BRIDGE}"
nft list table inet "$VM1_NFT_TABLE" >/dev/null 2>&1 && LAST_DETACH_RESIDUE="${LAST_DETACH_RESIDUE} nft:${VM1_NFT_TABLE}"
nwd_dnsmasq_running "$VM1_NET" && LAST_DETACH_RESIDUE="${LAST_DETACH_RESIDUE} dnsmasq:${VM1_NET}"
if [ -z "$LAST_DETACH_RESIDUE" ]; then
    qual_pass "last-detach teardown fired on the last VM delete (post-#362): no host state for ${VM1_NET}"
else
    qual_warn "DEFECT N5 (issue filed): last VM deleted but host state remains (${LAST_DETACH_RESIDUE}) — no node teardown on vm delete"
fi
save_evidence "leg-e-after-vm-deletes"

# N4: orphaned nic rows block network delete even with all VMs deleted.
# Fixed on main (#356 part 1, PR #359): vm delete removes its nic rows,
# so this delete should take the CLEAN branch on a post-#359 build. The
# escape below remains for the frozen candidate.
# Known candidate defect (issue filed): recorded as a WARNING. The delete
# target is the network vm-1 actually sat on (VM1_NET): 'default' on the
# frozen candidate, the operator's resolved network on post-#354 main —
# i.e. on a #354 build this leg deletes the DEPLOY-SEEDED fleet 'default'
# network (fine in the disposable qual env; the deploy teardown is the
# real cleanup anyway).
DEL_OUT="$(qual_chvctl network delete "$VM1_NET" 2>&1)" && RC=0 || RC=$?
if [ "$RC" -ne 0 ] && printf '%s' "$DEL_OUT" | grep -q "still attached"; then
    qual_warn "DEFECT N4 (issue filed): network delete refused with all VMs deleted (orphaned vm_nic_desired_state rows): ${DEL_OUT}"
    # Documented operator escape (frozen-candidate workaround): drop the
    # orphaned rows for THIS scenario's (deleted) VMs, then retry the
    # delete. NOTE: this is a read-write connection to the live CP DB —
    # the M4.1 evidence documented a WAL-unlink hazard for exactly this
    # shape. It is used anyway because (a) the qual DB is disposable and
    # destroyed at teardown, and (b) the M4.4 experiments ran this escape
    # and verified subsequent BFF writes landed correctly (the network
    # delete below is itself that verification). The post-delete BFF
    # success is asserted; if it ever fails, suspect this write.
    python3 - "$QUAL_DB" "$VM1_ID" "$VM2_ID" "$VM3_ID" <<'PYEOF'
import sqlite3, sys
conn = sqlite3.connect(sys.argv[1])
qs = ",".join("?" for _ in sys.argv[2:])
n = conn.execute(
    f"DELETE FROM vm_nic_desired_state WHERE vm_id IN ({qs})", sys.argv[2:]
).rowcount
conn.commit(); conn.close()
print(n)
PYEOF
    qual_info "operator escape: removed orphaned vm_nic_desired_state rows"
    qual_chvctl network delete "$VM1_NET" >/dev/null 2>&1 \
        && qual_pass "network delete succeeds after the escape" \
        || qual_error "network delete still refused after removing orphaned rows"
else
    qual_pass "network delete did not hit the orphaned-rows block (rc=${RC}: ${DEL_OUT})"
fi

# N5: the successful delete must leave NO host state. Post-#362 main the
# teardown already fired at the last VM delete (asserted above); this is
# the end-state gate after the network delete itself. The candidate
# leaves everything (DB-only delete).
sleep 3
RESIDUE=""
[ -n "$(bridge_addr "$VM1_BRIDGE")" ] && RESIDUE="${RESIDUE} bridge:${VM1_BRIDGE}"
nft list table inet "$VM1_NFT_TABLE" >/dev/null 2>&1 && RESIDUE="${RESIDUE} nft:${VM1_NFT_TABLE}"
nwd_dnsmasq_running "$VM1_NET" && RESIDUE="${RESIDUE} dnsmasq:${VM1_NET}"
if [ -n "$RESIDUE" ]; then
    qual_warn "DEFECT N5 (issue filed): network delete left host residue (${RESIDUE}) — DB-only delete, no node teardown (deploy teardown is the actual cleanup)"
else
    qual_pass "network delete cleaned up all host state"
fi
save_evidence "leg-e-after-network-delete"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
qual_info "=== M4.4 scenario complete: errors=${QUAL_ERRORS} warnings=${QUAL_WARNINGS} ==="
qual_info "defect recordings (N1/N2/N4/N5) are WARN-level: known candidate defects with issues filed; hard errors are reserved for outcomes beyond them"
qual_info "non-claims (inherited from prompt 01, restated): K8s/CNI, Docker-forwarded, and multi-bridge coexistence are NOT claimed"
qual_info "evidence artifacts: ${EVIDENCE_DIR}"
if [ "${QUAL_ERRORS}" -gt 0 ]; then
    qual_die "M4.4 scenario finished with ${QUAL_ERRORS} error(s)"
fi
