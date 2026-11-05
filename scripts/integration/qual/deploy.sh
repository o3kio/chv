#!/usr/bin/env bash
# deploy.sh — candidate-path deployment of the full CHV stack for the
# prompt-04 qualification (M4.1).
#
# This is the M2.5-proven deployment shape, committed as code:
#   - self-signed CA (2-day validity) + server cert (SAN localhost/127.0.0.1);
#   - REAL mTLS: a pre-placed client cert is used ONLY for the EnrollNode
#     handshake; afterwards the control-plane-issued node certificate
#     (CN = node id) is used;
#   - one-time bootstrap token seeded directly into the control-plane DB
#     (the production install path creates it via the loopback-only internal
#     endpoint; the harness seeds it before the agent starts — the agent
#     enrolls at startup and the token is consumed exactly once);
#   - admin user seeded with a fresh bcrypt hash (install.sh's job in
#     production; must_change_password=0 for the qualification run);
#   - authority_mode = "core-managed" (single durable CellHV Core authority);
#   - agent runtime dir mode 0700 (Core validate_paths contract);
#   - stord path_allowlist: agent runtime dir + images dir (operator-shaped);
#   - NO CHV_ALLOW_INSECURE anywhere.
#
# Usage:
#   sudo ./deploy.sh [OPTIONS]
#
# Options:
#   --binary-dir DIR   staged binaries (default: /var/lib/chv/qual/bin —
#                      produced by env-preflight.sh --stage-binaries)
#   --test-dir DIR     deployment dir (default: mktemp /tmp/chv-qual-XXXXXX)
#   --exec CMD...      run CMD with the deployment map exported
#                      (QUAL_MAP + individual variables) after verification;
#                      teardown runs afterwards with CMD's exit status
#   --hold             keep the stack up until SIGTERM/SIGINT, then tear down
#   --skip-cleanup     leave services running and files in place on exit
#
# Default (no --exec/--hold): full deployment smoke — bring the stack up,
# verify, tear down, assert no residue. Scenario scripts (M4.3+) use --exec.
#
# On success prints the deployment map (test dir, PIDs, ports) for the
# per-milestone scenario scripts to consume. Exit code non-zero on any
# hard failure; teardown asserts forbidden residue (no cloud-hypervisor
# processes, no new links, no new nft tables) unless --skip-cleanup.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "${SCRIPT_DIR}/lib.sh"

CHV_QUAL_ROOT="${CHV_QUAL_ROOT:-/var/lib/chv/qual}"
BINARY_DIR="${CHV_QUAL_ROOT}/bin"
# Per-service log level for the generated configs (default: info — the
# evidence shape). CHV_QUAL_LOG_LEVEL=debug is the diagnosis shape: the
# agent's client spans (RPC names + error reasons) and the CP's dispatch
# loop become visible. Applied to every service config so one run captures
# the whole chain consistently.
LOG_LEVEL="${CHV_QUAL_LOG_LEVEL:-info}"
TEST_DIR=""
SKIP_CLEANUP=false
EXEC_CMD=""
HOLD=false
NODE_ID="qual-node-1"
NETWORK_CIDR="${QUAL_NETWORK_CIDR:-10.200.0.0/24}"
GUEST_IMAGE="${GUEST_IMAGE:-noble-server-cloudimg-amd64.img}"

while [ $# -gt 0 ]; do
    case "$1" in
        --binary-dir) BINARY_DIR="${2:?}"; shift 2 ;;
        --binary-dir=*) BINARY_DIR="${1#*=}"; shift ;;
        --test-dir) TEST_DIR="${2:?}"; shift 2 ;;
        --test-dir=*) TEST_DIR="${1#*=}"; shift ;;
        --exec) shift; EXEC_CMD="$*"; break ;;
        --hold) HOLD=true; shift ;;
        --skip-cleanup) SKIP_CLEANUP=true; shift ;;
        *) qual_die "unknown argument: $1" ;;
    esac
done

# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------
[ "$(id -u)" -eq 0 ] || qual_die "must run as root"
[ -e /dev/kvm ] || qual_die "/dev/kvm missing — run env-preflight.sh on a KVM host"
[ -x /usr/bin/cloud-hypervisor ] || qual_die "/usr/bin/cloud-hypervisor missing — run env-preflight.sh"
[ -s "${CHV_QUAL_ROOT}/hypervisor-fw" ] || qual_die "firmware missing — run env-preflight.sh"
[ -s "${CHV_QUAL_ROOT}/images/${GUEST_IMAGE}" ] || qual_die "guest image missing — run env-preflight.sh"
for b in chv-controlplane chv-agent chv-stord chv-nwd chvctl; do
    [ -x "${BINARY_DIR}/${b}" ] || qual_die "binary missing: ${BINARY_DIR}/${b} — run env-preflight.sh --stage-binaries"
done
CANDIDATE_SHA="$(cat "${BINARY_DIR}/CANDIDATE_SHA" 2>/dev/null || echo unknown)"
qual_info "candidate SHA: ${CANDIDATE_SHA}"

# Service PIDs + deployment state (consumed by scenario scripts via the
# deployment map file).
CP_PID="" STORD_PID="" NWD_PID="" AGENT_PID=""

if [ -z "$TEST_DIR" ]; then
    TEST_DIR="$(mktemp -d /tmp/chv-qual-XXXXXX)"
else
    mkdir -p "$TEST_DIR"
fi
chmod 0700 "$TEST_DIR"
MAP_FILE="${TEST_DIR}/deployment.map"
qual_info "test directory: $TEST_DIR"

# Baseline snapshot for teardown residue assertions: host links + nft tables
# that exist BEFORE the deployment must all exist AFTER teardown, and no new
# ones may remain.
ip -o link show 2>/dev/null | awk -F': ' '{print $2}' | awk '{print $1}' \
    | sort > "${TEST_DIR}/links.before"
nft list tables 2>/dev/null | sort > "${TEST_DIR}/nft-tables.before" || true
# Baseline for the nwd dnsmasq runtime dir: the candidate hardcodes it to
# /run/chv/nwd (chv-nwd-core dhcp.rs/dns.rs/executor.rs — NOT the nwd.toml
# runtime_dir), so teardown must restore it only if this deployment created
# it. A pre-existing dir (another CHV install) is left untouched.
if [ -d /run/chv/nwd ]; then
    echo yes > "${TEST_DIR}/nwd-runtime-dir.before"
fi

cleanup() {
    local rc=$?
    if [ "$SKIP_CLEANUP" = true ]; then
        qual_info "skip-cleanup: services left running, test dir preserved: $TEST_DIR"
        return "$rc" 2>/dev/null || exit "$rc"
    fi
    qual_info "teardown starting"
    # Scenario scripts (M4.3+) restart daemons as part of their recovery
    # legs; the restarted PIDs cannot propagate back through --exec's
    # child process, so they record them in ${TEST_DIR}/pids.current
    # (KEY=PID lines, same keys as the deployment map). Re-read it here so
    # teardown always kills the CURRENT processes, not the original ones.
    if [ -f "${TEST_DIR}/pids.current" ]; then
        # shellcheck disable=SC1090
        while IFS='=' read -r k v; do
            case "$k" in
                CP_PID) CP_PID="$v" ;;
                STORD_PID) STORD_PID="$v" ;;
                NWD_PID) NWD_PID="$v" ;;
                AGENT_PID) AGENT_PID="$v" ;;
            esac
        done < "${TEST_DIR}/pids.current"
        qual_info "teardown: PID overrides from pids.current: CP=${CP_PID:-} STORD=${STORD_PID:-} NWD=${NWD_PID:-} AGENT=${AGENT_PID:-}"
    fi
    # Stop in reverse dependency order; SIGTERM then SIGKILL.
    for pid in "${AGENT_PID:-}" "${NWD_PID:-}" "${STORD_PID:-}" "${CP_PID:-}"; do
        [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
    done
    for _ in $(seq 1 50); do
        local alive=0
        for pid in "${AGENT_PID:-}" "${NWD_PID:-}" "${STORD_PID:-}" "${CP_PID:-}"; do
            # set -e: a bare `cond && alive=1` list returning non-zero would
            # abort the script mid-cleanup — use a real if.
            if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
                alive=1
            fi
        done
        if [ "$alive" -eq 0 ]; then
            break
        fi
        sleep 0.2
    done
    for pid in "${AGENT_PID:-}" "${NWD_PID:-}" "${STORD_PID:-}" "${CP_PID:-}"; do
        [ -n "$pid" ] && kill -9 "$pid" 2>/dev/null || true
    done
    # Kill any cloud-hypervisor the stack left behind (forbidden residue).
    # pkill -x cannot match "cloud-hypervisor": /proc/<pid>/comm is
    # truncated to 15 chars (same class as the lib.sh counter bug found by
    # M4.3 run 3 — an exact-name match never fires). Match the full command
    # line instead, scoped to THIS test dir (the agent spawns CH with its
    # api-socket under ${TEST_DIR}/agent/vms) so unrelated host VMs are
    # never touched. The (^|/)…( |$) anchor around the binary name is the
    # same self-match-proof form lib.sh uses: a process merely MENTIONING
    # "cloud-hypervisor" mid-command-line (an editor, a logger, this very
    # pattern's text) can never match — the token must be argv[0].
    pkill -f "(^|/)cloud-hypervisor( |$).*${TEST_DIR}" 2>/dev/null || true
    sleep 1
    # SIGKILL fallback: a VMM whose control loop is wedged (observed in
    # M4.3 run 5: an adopted VM's graceful stop left CH alive in Shutdown
    # state, SIGTERM ineffective) must not survive teardown either.
    pkill -9 -f "(^|/)cloud-hypervisor( |$).*${TEST_DIR}" 2>/dev/null || true
    sleep 1

    # Safety net for nwd processes the PID overrides cannot reach: the
    # agent's supervisor spawns nwd as its own child (config under
    # ${TEST_DIR}/agent/chv-nwd.toml) whenever the deploy-started instance
    # dies mid-scenario (verified in M4.4: SIGKILL of the external nwd is
    # recovered by the supervisor within one reconcile tick). pids.current
    # normally carries the new pid, but a restart racing the scenario's
    # last update would leak it — this scoped pattern (argv[0]-anchored,
    # config path under THIS test dir) closes that gap. Both the deploy's
    # nwd.toml and the supervisor's agent/chv-nwd.toml live under
    # ${TEST_DIR}, so one pattern covers both.
    pkill -f "(^|/)chv-nwd( |$).*${TEST_DIR}" 2>/dev/null || true
    sleep 1
    pkill -9 -f "(^|/)chv-nwd( |$).*${TEST_DIR}" 2>/dev/null || true

    # Same safety net for every other daemon: pids.current is written by the
    # SCENARIO, so a scenario that dies before its first write (or a
    # supervisor respawn that races the last write) used to leave the agent,
    # stord, or control plane holding 8443/9100/9101/9103 into the NEXT
    # scenario — observed as `127.0.0.1:9101: Address already in use` and as
    # CP→agent auth failures against the stale process. Scope is THIS test
    # dir's config path, so nothing else on the host is touched.
    for spec in \
        "chv-agent|${TEST_DIR}/agent.toml" \
        "chv-stord|${TEST_DIR}/(stord\.toml$|agent/chv-stord)" \
        "chv-controlplane|${TEST_DIR}/controlplane.toml"; do
        bin="${spec%%|*}"
        cfg="${spec#*|}"
        pkill -f "(^|/)${bin}( |$).*${cfg}" 2>/dev/null || true
    done
    sleep 1
    for spec in \
        "chv-agent|${TEST_DIR}/agent.toml" \
        "chv-stord|${TEST_DIR}/(stord\.toml$|agent/chv-stord)" \
        "chv-controlplane|${TEST_DIR}/controlplane.toml"; do
        bin="${spec%%|*}"
        cfg="${spec#*|}"
        pkill -9 -f "(^|/)${bin}( |$).*${cfg}" 2>/dev/null || true
    done

    # Remove nwd-owned links this deployment created (bridges and taps —
    # see chv-common bridge_name_for_network: 'br-<id>' or
    # 'br-<first8><hash>' past 15 chars, and 'tap-*', PLUS 'chvbr0' for the
    # default network — see
    # chv-hypervisor-api resources.rs default_network_bridge_name; run 4's
    # teardown left chvbr0 behind because the pattern only covered br-/tap-).
    # Scoped by the baseline diff so pre-existing host links are never
    # touched.
    ip -o link show 2>/dev/null | awk -F': ' '{print $2}' | awk '{print $1}' \
        | sort > "${TEST_DIR}/links.mid" || true
    comm -13 "${TEST_DIR}/links.before" "${TEST_DIR}/links.mid" \
        | grep -E '^(br-|tap-|chvbr)' | grep -v '^$' > "${TEST_DIR}/links.owned" || true
    while IFS= read -r iface; do
        [ -n "$iface" ] || continue
        qual_info "teardown: deleting nwd link ${iface}"
        ip link delete "$iface" 2>/dev/null || true
    done < "${TEST_DIR}/links.owned"

    # Remove nft tables this deployment created (nwd's per-network firewall
    # tables, e.g. 'table inet chv-default'). Normally nwd's own SIGTERM
    # shutdown removes them; when it cannot (a CH process still held the
    # bridge, as in run 4), this fallback must — scoped by the baseline
    # diff so pre-existing host tables are never touched.
    nft list tables 2>/dev/null | sort > "${TEST_DIR}/nft-tables.mid" || true
    while IFS= read -r table; do
        [ -n "$table" ] || continue
        if ! grep -qx "$table" "${TEST_DIR}/nft-tables.before" 2>/dev/null; then
            qual_info "teardown: deleting nft ${table}"
            nft delete table ${table#table } 2>/dev/null || true
        fi
    done < "${TEST_DIR}/nft-tables.mid"

    # Kill dnsmasq instances nwd spawned for this deployment's networks.
    # The candidate hardcodes their config/pid location to
    # /run/chv/nwd/dnsmasq-<network_id>.{conf,pid} (chv-nwd-core dhcp.rs/
    # dns.rs/executor.rs), and `network delete` performs NO host teardown
    # at all in the candidate (M4.4 finding: the delete path is DB-only),
    # so these processes outlive both the networks and nwd itself
    # (verified by experiment: two orphaned dnsmasq instances remained
    # after an otherwise-clean teardown). Scoped by the conf path so a
    # pre-existing system dnsmasq is never touched; argv[0]-anchored the
    # same way as the CH pattern above.
    if pgrep -f "(^|/)dnsmasq( |$).*--conf-file=/run/chv/nwd/dnsmasq-" >/dev/null 2>&1; then
        qual_info "teardown: killing nwd-spawned dnsmasq instances"
        pkill -f "(^|/)dnsmasq( |$).*--conf-file=/run/chv/nwd/dnsmasq-" 2>/dev/null || true
        sleep 1
        pkill -9 -f "(^|/)dnsmasq( |$).*--conf-file=/run/chv/nwd/dnsmasq-" 2>/dev/null || true
    fi
    # Restore the hardcoded dnsmasq runtime dir only if this deployment
    # created it (see the baseline snapshot above).
    if [ ! -f "${TEST_DIR}/nwd-runtime-dir.before" ] && [ -d /run/chv/nwd ]; then
        rm -rf /run/chv/nwd
        qual_info "teardown: removed /run/chv/nwd (nwd dnsmasq runtime dir)"
    fi

    # --- Forbidden-outcome residue assertions ---
    # (|| true: an assertion failure must not abort the remaining cleanup —
    # every residue finding is reported, then the dir is preserved.)
    assert_no_ch_residue "teardown" || true
    ip -o link show 2>/dev/null | awk -F': ' '{print $2}' | awk '{print $1}' \
        | sort > "${TEST_DIR}/links.after" || true
    local new_links
    new_links="$(comm -13 "${TEST_DIR}/links.before" "${TEST_DIR}/links.after" | grep -v '^$' || true)"
    if [ -z "$new_links" ]; then
        qual_pass "teardown: no new host links remain"
    else
        qual_error "teardown: FORBIDDEN new host links remain: $(echo "$new_links" | tr '\n' ' ')"
    fi
    nft list tables 2>/dev/null | sort > "${TEST_DIR}/nft-tables.after" || true
    local new_tables
    new_tables="$(comm -13 "${TEST_DIR}/nft-tables.before" "${TEST_DIR}/nft-tables.after" | grep -v '^$' || true)"
    if [ -z "$new_tables" ]; then
        qual_pass "teardown: no new nft tables remain"
    else
        qual_error "teardown: FORBIDDEN new nft tables remain: $(echo "$new_tables" | tr '\n' ' ')"
    fi
    if pgrep -f "(^|/)dnsmasq( |$).*--conf-file=/run/chv/nwd/dnsmasq-" >/dev/null 2>&1; then
        qual_error "teardown: FORBIDDEN — nwd-spawned dnsmasq instances remain"
        pgrep -af "(^|/)dnsmasq( |$).*--conf-file=/run/chv/nwd/" >&2 || true
    else
        qual_pass "teardown: no nwd-spawned dnsmasq remains"
    fi

    # Keep the test dir for post-mortem if anything failed; remove on success.
    if [ "${QUAL_ERRORS}" -gt 0 ]; then
        local kept
        kept="/tmp/chv-qual-failed-$(basename "$TEST_DIR")"
        mv "$TEST_DIR" "$kept" 2>/dev/null || kept="$TEST_DIR"
        qual_error "teardown: test dir preserved for post-mortem: ${kept}"
    else
        rm -rf "$TEST_DIR"
        qual_info "teardown complete (test dir removed)"
    fi
    return "$rc" 2>/dev/null || exit "$rc"
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# 1. Certificates, token, admin credential, configs
# ---------------------------------------------------------------------------
qual_info "generating certificates, bootstrap token, admin credential, configs"

certs_dir="${TEST_DIR}/certs"; logs_dir="${TEST_DIR}/logs"
cp_dir="${TEST_DIR}/controlplane"; agent_dir="${TEST_DIR}/agent"
stord_dir="${TEST_DIR}/stord"; nwd_dir="${TEST_DIR}/nwd"
images_dir="${CHV_QUAL_ROOT}/images"
mkdir -p "$certs_dir" "$logs_dir" "$cp_dir" "$stord_dir" "$nwd_dir" "${agent_dir}/storage"

# CA (2-day validity — long enough for a qualification run, short enough to
# be obviously throwaway).
openssl genrsa -out "$certs_dir/ca.key" 4096 2>/dev/null
openssl req -x509 -new -nodes -key "$certs_dir/ca.key" -sha256 -days 2 \
    -out "$certs_dir/ca.crt" \
    -subj "/O=CHV Qualification/CN=chv-qual-ca" 2>/dev/null

# Server cert with SAN localhost + 127.0.0.1 (control-plane gRPC/HTTP).
# NOTE: `openssl x509 -req` without -extfile emits X.509 **v1**, which
# rustls/webpki rejects (UnsupportedCertVersion) — every cert here must be
# explicitly v3 via an extfile.
openssl genrsa -out "$certs_dir/server.key" 2048 2>/dev/null
openssl req -new -key "$certs_dir/server.key" -out "$certs_dir/server.csr" \
    -subj "/O=CHV Qualification/CN=localhost" 2>/dev/null
cat > "$certs_dir/server.ext" <<EOF
basicConstraints = CA:FALSE
keyUsage = digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = DNS:localhost, IP:127.0.0.1
EOF
openssl x509 -req -in "$certs_dir/server.csr" \
    -CA "$certs_dir/ca.crt" -CAkey "$certs_dir/ca.key" \
    -CAcreateserial -out "$certs_dir/server.crt" \
    -days 2 -sha256 -extfile "$certs_dir/server.ext" 2>/dev/null
rm -f "$certs_dir/server.csr" "$certs_dir/server.ext"

# Pre-placed client cert used ONLY for the EnrollNode handshake.
openssl genrsa -out "$certs_dir/enroll-client.key" 2048 2>/dev/null
openssl req -new -key "$certs_dir/enroll-client.key" -out "$certs_dir/enroll-client.csr" \
    -subj "/O=CHV Qualification/CN=qual-enroll" 2>/dev/null
cat > "$certs_dir/enroll-client.ext" <<EOF
basicConstraints = CA:FALSE
keyUsage = digitalSignature, keyEncipherment
extendedKeyUsage = clientAuth
EOF
openssl x509 -req -in "$certs_dir/enroll-client.csr" \
    -CA "$certs_dir/ca.crt" -CAkey "$certs_dir/ca.key" \
    -CAcreateserial -out "$certs_dir/enroll-client.crt" \
    -days 2 -sha256 -extfile "$certs_dir/enroll-client.ext" 2>/dev/null
rm -f "$certs_dir/enroll-client.csr" "$certs_dir/enroll-client.ext"

chmod 644 "$certs_dir"/*.crt
chmod 600 "$certs_dir"/*.key

# One-time bootstrap token (value only in this file, mode 0600; the DB gets
# the sha256 hash — same as the production validate_and_consume path).
BOOTSTRAP_TOKEN="$(openssl rand -hex 24)"
install -m 0600 /dev/null "${agent_dir}/bootstrap-token"
printf '%s\n' "$BOOTSTRAP_TOKEN" > "${agent_dir}/bootstrap-token"

# Admin credential (fresh per run; must_change_password=0).
ADMIN_PASSWORD="$(openssl rand -hex 12)"

# Migrations directory: repo checkout shape (package installs use
# /usr/share/chv/migrations — same files).
MIGRATIONS_DIR="${REPO_ROOT_OVERRIDE:-$(cd "${SCRIPT_DIR}/../../.." && pwd)}/cmd/chv-controlplane/migrations"
[ -d "$MIGRATIONS_DIR" ] || qual_die "migrations dir not found: $MIGRATIONS_DIR"

# One master secret shared by BOTH configs. Since #662/#693 the control plane
# authenticates every CP→agent RPC with `[secrets] agent_auth_token`; when
# that key is absent, both sides derive it from their own `jwt_secret`. If
# only the control plane is given a secret and the agent is not, the agent
# mints an unrelated random one at startup and every dispatch fails closed
# with "invalid agent authentication token" (#659: nightly red on every
# scenario after #693). install.sh has always written the same JWT_SECRET
# into both files; this harness must do the same.
JWT_SECRET="qual-$(openssl rand -hex 32)-jwt-secret"

# Dedicated migration-signing secret (issue #663). Deliberately distinct from
# the JWT/enrollment secret: rotating user auth must not invalidate in-flight
# migration authorizations. The control plane mints tokens from this file and
# every migration receiver (the scenario DST stords) verifies them with the
# same file, so both sides must agree — like the agent-auth master above.
# 32 random bytes (64 hex chars) meets the codec's minimum. The file is
# trimmed by both readers.
MIGRATION_SIGNING_SECRET_FILE="${TEST_DIR}/migration-signing.key"
install -m 0600 /dev/null "$MIGRATION_SIGNING_SECRET_FILE"
openssl rand -hex 32 > "$MIGRATION_SIGNING_SECRET_FILE"
[ "$(tr -d '\n' < "$MIGRATION_SIGNING_SECRET_FILE" | wc -c)" -ge 32 ] \
    || qual_die "generated migration signing secret is shorter than 32 bytes"

# --- controlplane.toml ---
# agent_socket_pattern / agent_runtime_dir / firmware_path: the candidate's
# defaults match the PRODUCTION layout (/run/chv/agent/api.sock,
# /var/lib/chv/agent, /var/lib/chv/hypervisor-fw — installed there by
# install.sh). This deployment deliberately uses a throwaway dir tree, so
# the CP config must teach it where the agent actually lives — without
# these, VM dispatch fails with "backend unavailable: agent — transport
# error" (found by the M4.3 run 1; the M4.1 smoke never dispatched a VM
# operation, so the gap was invisible). Firmware boot is the qualified
# path (M2.5); kernel_path stays default because it is unused when the
# spec carries a firmware.
cat > "${TEST_DIR}/controlplane.toml" <<EOF
grpc_bind = "127.0.0.1:8443"
http_bind = "127.0.0.1:8080"
log_level = "${LOG_LEVEL}"
runtime_dir = "${cp_dir}"
jwt_secret = "${JWT_SECRET}"
agent_socket_pattern = "${agent_dir}/api.sock"
agent_runtime_dir = "${agent_dir}"
firmware_path = "${CHV_QUAL_ROOT}/hypervisor-fw"

[database]
url = "sqlite://${cp_dir}/controlplane.db"
migrations_dir = "${MIGRATIONS_DIR}"
max_connections = 4
min_connections = 1
acquire_timeout_secs = 5

[tls]
ca_cert_path = "${certs_dir}/ca.crt"
ca_key_path = "${certs_dir}/ca.key"
server_cert_path = "${certs_dir}/server.crt"
server_key_path = "${certs_dir}/server.key"
client_ca_path = "${certs_dir}/ca.crt"

# Issue #663: disk migrations are authorized with a short-lived HMAC token.
# A production-shaped deployment must configure a dedicated secret or every
# MigrateVm dispatch fails closed before contacting the agent. The same file
# is handed to the scenario migration receivers.
[migration]
signing_secret_file = "${MIGRATION_SIGNING_SECRET_FILE}"
token_ttl_seconds = 300
EOF

# --- agent.toml (core-managed authority) ---
cat > "${TEST_DIR}/agent.toml" <<EOF
socket_path = "${agent_dir}/api.sock"
runtime_dir = "${agent_dir}"
log_level = "${LOG_LEVEL}"
authority_mode = "core-managed"
core_api_socket_path = "${agent_dir}/core.sock"
core_store_path = "${agent_dir}/core.db"
core_archive_path = "${agent_dir}/node-cache-v1.archive"
control_plane_addr = "https://127.0.0.1:8443"
stord_socket = "${stord_dir}/api.sock"
nwd_socket = "${nwd_dir}/api.sock"
chv_binary_path = "/usr/bin/cloud-hypervisor"
stord_binary_path = "${BINARY_DIR}/chv-stord"
nwd_binary_path = "${BINARY_DIR}/chv-nwd"
cache_path = "${agent_dir}/agent-cache.json"
node_id = "${NODE_ID}"
metrics_bind = "127.0.0.1:9100"
storage_base_dir = "${agent_dir}/storage"
console_bind = "127.0.0.1:8444"
bootstrap_token_path = "${agent_dir}/bootstrap-token"

# #376: preserve stord's path confinement across supervisor respawns —
# the agent writes this into the generated chv-stord.toml when it has to
# respawn a dead stord. Mirrors the operator-shaped allowlist in
# stord.toml below (volume locators + the seed-image dir).
stord_path_allowlist = ["${agent_dir}", "${stord_dir}", "${images_dir}"]

# #385/#379: respawn from the operator's own stord.toml (below) instead
# of the generated config, so the backend_type / allowlist keys survive a
# respawn; the agent also parses this file to learn the node's real
# backend class (inventory + LVM locator shaping, #379 DP4/DP5).
stord_config_path = "${TEST_DIR}/stord.toml"

tls_cert_path = "${certs_dir}/enroll-client.crt"
tls_key_path = "${certs_dir}/enroll-client.key"
ca_cert_path = "${certs_dir}/ca.crt"

# Must match controlplane.toml: the agent derives the same
# [secrets] agent_auth_token from this master value. Omitting it makes the
# agent mint its own random secret, so every CP→agent RPC is rejected
# (see the note above JWT_SECRET).
jwt_secret = "${JWT_SECRET}"
EOF

# --- stord.toml (operator-shaped path allowlist) ---
cat > "${TEST_DIR}/stord.toml" <<EOF
socket_path = "${stord_dir}/api.sock"
runtime_dir = "${stord_dir}"
log_level = "${LOG_LEVEL}"
# Explicit so the deployed daemon never relies on the loopback default: the
# scenario topologies below start additional stords and must know which
# ports are already taken (#687 made the metrics listener fatal on conflict).
metrics_bind = "127.0.0.1:9101"
path_allowlist = ["${agent_dir}", "${stord_dir}", "${images_dir}"]
# #379 DP5: the standard device allowlist — dm-path tokens only, which
# the LVM locator convention (/dev/mapper/{vg}-{vid}) is shaped to pass.
# The check only fires for lvm/block-class opens, so the default local
# class is unaffected (Leg F keeps passing under this posture).
device_allowlist = ["/dev/dm-*", "/dev/mapper/*"]
EOF

# --- nwd.toml ---
cat > "${TEST_DIR}/nwd.toml" <<EOF
socket_path = "${nwd_dir}/api.sock"
runtime_dir = "${nwd_dir}"
log_level = "${LOG_LEVEL}"
metrics_bind = "127.0.0.1:9102"
EOF

# Regression guard for the #662/#693 shared-secret invariant: both configs
# must resolve to the SAME agent-auth credential. This is the check whose
# absence let the nightly lane go red on every scenario while the harness
# itself looked healthy. Derivation is `[secrets] agent_auth_token` when set,
# else HMAC(jwt_secret); this harness writes no explicit [secrets], so equal
# jwt_secret values are the invariant.
CP_AGENT_MASTER="$(sed -n 's/^jwt_secret = "\(.*\)"$/\1/p' "${TEST_DIR}/controlplane.toml")"
AGENT_MASTER="$(sed -n 's/^jwt_secret = "\(.*\)"$/\1/p' "${TEST_DIR}/agent.toml")"
if [ -z "$CP_AGENT_MASTER" ] || [ "$CP_AGENT_MASTER" != "$AGENT_MASTER" ]; then
    qual_die "agent-auth master mismatch between controlplane.toml and agent.toml — CP→agent RPCs would fail closed with 'invalid agent authentication token'"
fi
qual_pass "control-plane and agent share the agent-auth master secret"

# Agent runtime dir MUST be 0700 for the Core-authority startup validation
# (cellhv-core-startup::validate_paths). Done last, after token file writes.
chmod 0700 "${agent_dir}"

qual_pass "configs generated (core-managed authority, real mTLS, 0700 runtime dir)"

# ---------------------------------------------------------------------------
# 2. Start control-plane, wait for migrations, seed admin + bootstrap token
# ---------------------------------------------------------------------------
qual_info "starting chv-controlplane (first pass: run migrations)"
"${BINARY_DIR}/chv-controlplane" "${TEST_DIR}/controlplane.toml" \
    > "${logs_dir}/controlplane.log" 2>&1 &
CP_PID=$!
disown "$CP_PID"

DB="${cp_dir}/controlplane.db"
# Poll helper: the sqlite query must be re-evaluated on every attempt (a
# command substitution in the wait_for argument list would be expanded once).
cp_users_table_ready() {
    [ -s "$DB" ] || return 1
    [ -n "$(sqlite_query "$DB" "SELECT name FROM sqlite_master WHERE type='table' AND name='users'" 2>/dev/null)" ]
}
wait_for "control-plane migrations applied (users table exists)" 30 cp_users_table_ready \
    || qual_die "control-plane did not become ready — log: $(tail -20 "${logs_dir}/controlplane.log" 2>/dev/null)"

# Stop the control-plane before writing to its database. A read-write
# sqlite connection that closes while the CP's pool connections are idle
# unlinks the -wal/-shm sidecars (verified with strace; see lib.sh), which
# would silently divert the CP's subsequent commits into the unlinked WAL
# inode. Seeding between a clean stop and a restart is the safe window.
qual_info "stopping chv-controlplane for DB seeding (WAL-safe window)"
kill "$CP_PID" 2>/dev/null || true
for _ in $(seq 1 50); do
    if ! kill -0 "$CP_PID" 2>/dev/null; then
        break
    fi
    sleep 0.2
done
if kill -0 "$CP_PID" 2>/dev/null; then
    qual_die "control-plane did not stop for seeding"
fi

# Seed admin user (production: install.sh post-migration with a random
# password; here: fresh bcrypt hash, must_change_password=0).
ADMIN_HASH="$(bcrypt_hash "$ADMIN_PASSWORD")"
[ -n "$ADMIN_HASH" ] || qual_die "bcrypt hash generation failed (htpasswd)"
sqlite_exec "$DB" "INSERT OR REPLACE INTO users (user_id, username, password_hash, role, must_change_password) VALUES ('qual-admin', 'admin', '${ADMIN_HASH}', 'admin', 0)"
qual_pass "admin user seeded (bcrypt, fresh random password)"

# Seed the one-time bootstrap token (sha256 hex — same as validate_and_consume).
TOKEN_HASH="$(printf '%s' "$BOOTSTRAP_TOKEN" | python3 -c 'import hashlib,sys; print(hashlib.sha256(sys.stdin.buffer.read()).hexdigest())')"
sqlite_exec "$DB" "INSERT INTO bootstrap_tokens (token_hash, description, one_time_use) VALUES ('${TOKEN_HASH}', 'qual harness one-time token', 1)"
qual_pass "one-time bootstrap token seeded"

qual_info "restarting chv-controlplane (seeded DB)"
"${BINARY_DIR}/chv-controlplane" "${TEST_DIR}/controlplane.toml" \
    >> "${logs_dir}/controlplane.log" 2>&1 &
CP_PID=$!
disown "$CP_PID"
wait_for "control-plane up after seeding" 30 cp_users_table_ready \
    || qual_die "control-plane did not restart — log: $(tail -20 "${logs_dir}/controlplane.log" 2>/dev/null)"
assert_file_contains "control plane mints migration authorizations (dedicated signing secret loaded)" \
    "${logs_dir}/controlplane.log" "migration authorization minting enabled" \
    || true

# ---------------------------------------------------------------------------
# 3. Start stord + nwd, then the agent (enrolls at startup)
# ---------------------------------------------------------------------------
qual_info "starting chv-stord"
"${BINARY_DIR}/chv-stord" "${TEST_DIR}/stord.toml" \
    > "${logs_dir}/stord.log" 2>&1 &
STORD_PID=$!
disown "$STORD_PID"
wait_for "stord socket up" 20 test -S "${stord_dir}/api.sock" \
    || qual_die "stord did not come up — log: $(tail -20 "${logs_dir}/stord.log" 2>/dev/null)"

qual_info "starting chv-nwd"
"${BINARY_DIR}/chv-nwd" "${TEST_DIR}/nwd.toml" \
    > "${logs_dir}/nwd.log" 2>&1 &
NWD_PID=$!
# Disown: scenarios may SIGKILL this daemon (M4.4 Leg C kills nwd to
# exercise the supervisor restart); without disown, bash prints a
# "Killed" job-control notice into the evidence log when it reaps it.
disown "$NWD_PID"
wait_for "nwd socket up" 20 test -S "${nwd_dir}/api.sock" \
    || qual_die "nwd did not come up — log: $(tail -20 "${logs_dir}/nwd.log" 2>/dev/null)"

qual_info "starting chv-agent (authority_mode=core-managed; enrolls at startup)"
"${BINARY_DIR}/chv-agent" "${TEST_DIR}/agent.toml" \
    > "${logs_dir}/agent.log" 2>&1 &
AGENT_PID=$!
disown "$AGENT_PID"

# Enrollment evidence: the control-plane-issued node cert (NOT the pre-placed
# enrollment client cert) must appear in the runtime dir.
wait_for "agent enrolled (CP-issued node cert present)" 60 \
    test -s "${agent_dir}/agent.crt" -a -s "${agent_dir}/agent.key" \
    || { qual_error "agent enrollment did not complete — log tail:"; tail -30 "${logs_dir}/agent.log" >&2; qual_die "aborting"; }
ISSUED_CN="$(openssl x509 -in "${agent_dir}/agent.crt" -noout -subject 2>/dev/null)"
assert_contains "issued node cert CN is the node id" "$ISSUED_CN" "CN = ${NODE_ID}" || true
assert_file_exists "core API socket up" "${agent_dir}/core.sock" || true
wait_for "agent gRPC socket up" 30 test -S "${agent_dir}/api.sock" \
    || qual_error "agent api.sock did not appear"

# The one-time token must be consumed exactly once.
# (|| true: diagnostic — records the failure without aborting the run;
# the final exit status still carries it.)
TOKEN_STATE="$(sqlite_query "$DB" "SELECT used_at IS NOT NULL FROM bootstrap_tokens WHERE token_hash='${TOKEN_HASH}'")"
assert_contains "one-time bootstrap token consumed" "$TOKEN_STATE" "1" || true

# #709: on success the agent must report the first-enrollment bootstrap result
# on the identity the control plane just issued. Pre-fix the report went out
# on the pre-enrollment channel, the control plane's peer-node-id check
# rejected it, and no row was written; a `success = 1` row is the direct
# observable of the fix. The report is asynchronous, so poll briefly.
bootstrap_result_recorded() {
    [ "$(sqlite_query "$DB" "SELECT success FROM node_bootstrap_results WHERE node_id='${NODE_ID}'" 2>/dev/null | head -1)" = "1" ]
}
wait_for "first-enrollment bootstrap result recorded (success=1)" 30 bootstrap_result_recorded \
    && qual_pass "first-enrollment bootstrap result recorded on the issued identity (#709)" \
    || qual_error "first-enrollment bootstrap result NOT recorded for ${NODE_ID} (node_bootstrap_results: $(sqlite_query "$DB" "SELECT node_id, success FROM node_bootstrap_results" 2>/dev/null | tr '\n' ' '))"

# No insecure-mode escape hatch anywhere in the environment.
if env | grep -q '^CHV_ALLOW_INSECURE='; then
    qual_error "FORBIDDEN: CHV_ALLOW_INSECURE is set in the environment"
else
    qual_pass "CHV_ALLOW_INSECURE not set"
fi

# ---------------------------------------------------------------------------
# 4. Management-plane verification via chvctl (BFF → control-plane)
# ---------------------------------------------------------------------------
QUAL_CHVCTL="${BINARY_DIR}/chvctl"
QUAL_BFF_URL="http://127.0.0.1:8080"
QUAL_CHVCTL_CONFIG_DIR="${TEST_DIR}/chvctl-config"
mkdir -p "${QUAL_CHVCTL_CONFIG_DIR}/chvctl"

qual_chvctl login --username admin --password "$ADMIN_PASSWORD" >/dev/null \
    && qual_pass "chvctl login (bcrypt admin over BFF)" \
    || qual_error "chvctl login failed"

# Create the qualification network (M2.5 shape: 'default', bridge-local)
# BEFORE asserting node readiness — the node converges to TenantReady only
# once a network exists for the tenant.
qual_chvctl network create default --cidr "$NETWORK_CIDR" >/dev/null \
    && qual_pass "network 'default' created (${NETWORK_CIDR})" \
    || qual_error "network creation failed"

# Import the guest seed image via file:// source URL.
qual_chvctl image import ubuntu-noble --url "file://${images_dir}/${GUEST_IMAGE}" --format qcow2 >/dev/null \
    && qual_pass "image 'ubuntu-noble' imported (file://${images_dir}/${GUEST_IMAGE})" \
    || qual_error "image import failed"

# Node readiness: the reconciler converges the node to TenantReady once the
# network exists — poll rather than assert immediately.
node_state_is_tenant_ready() {
    qual_chvctl node list --output json 2>/dev/null | grep -q '"state": "TenantReady"'
}
wait_for "node state TenantReady (converged after network create)" 60 node_state_is_tenant_ready || true
NODE_LIST="$(qual_chvctl node list --output json 2>/dev/null || true)"
qual_info "node list: ${NODE_LIST}"
assert_contains "node enrolled and visible via BFF" "$NODE_LIST" "${NODE_ID}" || true
assert_contains "node state TenantReady" "$NODE_LIST" "TenantReady" || true
assert_contains "node health Healthy" "$NODE_LIST" '"health": "Healthy"' || true

# NOTE: `chvctl health check|cluster|report` all target /v1/health* routes
# that the BFF-backed control-plane HTTP surface does not implement (501/
# 404 NOT_IMPLEMENTED) — a chvctl/BFF surface gap recorded in the M4.1
# evidence and tracked in an issue, not a deployment failure. The node-list
# checks above (state TenantReady + health Healthy) are the readiness gate.

# ---------------------------------------------------------------------------
# 5. Deployment map (for scenario scripts) + summary
# ---------------------------------------------------------------------------
cat > "$MAP_FILE" <<EOF
TEST_DIR=${TEST_DIR}
CANDIDATE_SHA=${CANDIDATE_SHA}
NODE_ID=${NODE_ID}
NETWORK_CIDR=${NETWORK_CIDR}
CP_PID=${CP_PID}
STORD_PID=${STORD_PID}
NWD_PID=${NWD_PID}
AGENT_PID=${AGENT_PID}
DB=${DB}
AGENT_DIR=${agent_dir}
CERTS_DIR=${certs_dir}
LOGS_DIR=${logs_dir}
BINARY_DIR=${BINARY_DIR}
ADMIN_PASSWORD=${ADMIN_PASSWORD}
BFF_URL=${QUAL_BFF_URL}
CHVCTL_CONFIG_DIR=${QUAL_CHVCTL_CONFIG_DIR}
EOF
chmod 0600 "$MAP_FILE"

qual_info "deployment map: ${MAP_FILE}"
qual_info "stack running: CP=${CP_PID} STORD=${STORD_PID} NWD=${NWD_PID} AGENT=${AGENT_PID}"

# ---------------------------------------------------------------------------
# 6. Scenario hand-off (--exec / --hold) or deployment smoke (default)
# ---------------------------------------------------------------------------
# Non-fatal: an assertion failure above is already recorded in QUAL_ERRORS;
# it must not abort --hold/--exec runs (and the final exit status carries it).
qual_summary "deploy (stack up, verified)" || true

SCENARIO_RC=0
if [ "${QUAL_ERRORS}" -gt 0 ]; then
    SCENARIO_RC=1
fi

if [ -n "$EXEC_CMD" ]; then
    qual_info "running scenario: ${EXEC_CMD}"
    # shellcheck disable=SC2086
    if set -- $EXEC_CMD; env QUAL_MAP="$MAP_FILE" QUAL_TEST_DIR="$TEST_DIR" \
        QUAL_NODE_ID="$NODE_ID" QUAL_DB="$DB" QUAL_AGENT_DIR="$agent_dir" \
        QUAL_CERTS_DIR="$certs_dir" QUAL_LOGS_DIR="$logs_dir" \
        QUAL_BINARY_DIR="$BINARY_DIR" QUAL_BFF_URL="$QUAL_BFF_URL" \
        QUAL_ADMIN_PASSWORD="$ADMIN_PASSWORD" \
        QUAL_CHVCTL_CONFIG_DIR="$QUAL_CHVCTL_CONFIG_DIR" \
        QUAL_CHVCTL="$QUAL_CHVCTL" QUAL_CP_PID="$CP_PID" \
        QUAL_MIGRATION_SECRET_FILE="$MIGRATION_SIGNING_SECRET_FILE" \
        QUAL_STORD_PID="$STORD_PID" QUAL_NWD_PID="$NWD_PID" \
        QUAL_AGENT_PID="$AGENT_PID" QUAL_NETWORK_CIDR="$NETWORK_CIDR" \
        QUAL_GUEST_IMAGE_PATH="${CHV_QUAL_ROOT}/images/${GUEST_IMAGE}" \
        "$@"; then
        qual_pass "scenario exited 0"
    else
        SCENARIO_RC=$?
        qual_error "scenario exited non-zero (${SCENARIO_RC})"
    fi
elif [ "$HOLD" = true ]; then
    qual_info "holding stack until SIGTERM/SIGINT ..."
    trap 'qual_info "signal received — tearing down"; exit 0' TERM INT
    while :; do sleep 5; done
else
    qual_info "deployment smoke complete — tearing down (use --exec/--hold to keep the stack for scenarios)"
fi

qual_summary "deploy + scenario (rc=${SCENARIO_RC})" || true
exit "$SCENARIO_RC"
