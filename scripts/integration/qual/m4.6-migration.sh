#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# M4.6 — Two-stord mTLS disk-migration qualification (real host, in-stack)
#
# The prompt-04 milestone spec (§M4.6): positive + negative identity cases
# for storage migration between two stord daemons, all fail-closed legs.
#
# Topology (inside a deploy.sh --exec run, on the real host):
#   - The DEPLOYED stack (CP/agent/stord/nwd, core-managed authority)
#     provides the environment; its stord has NO [migration] config and is
#     the N1 "missing TLS config" target (trigger-time failure).
#   - The scenario stands up TWO additional standalone stords, deliberately
#     NOT agent-supervised (#385: a supervisor respawn generates a minimal
#     config that drops [migration]) and NOT on the agent's stord_socket:
#     SRC — own socket_path + runtime_dir under TEST_DIR, [migration]
#           enabled with the client identity (client_cert_path,
#           client_key_path, ca_cert_path, dest_server_name);
#     DST — own socket_path + runtime_dir, [migration] enabled with the
#           receiver fields (listen_addr = 127.0.0.1:51052,
#           server_cert_path/server_key_path/client_ca_path) — the mTLS
#           TCP receiver listener (#390/#393).
#   - Both are driven over their UDS with grpcurl (pinned v1.9.3,
#     checksum-verified at runtime — see provision_grpcurl) using the
#     M2.5-RC4-proven invocation: `unix://` scheme in the address
#     positional, -plaintext, -import-path/-proto, flags before positionals
#     (the node daemons serve no reflection).
#
# Legs:
#   P  positive: OpenVolume (seed_from, 4 GiB patterned) on SRC →
#      TriggerDiskMigration → BULK_COPY observed → dirty-round machinery
#      runs and converges at 0 dirty blocks (QUIESCENT source — the #394
#      boundary: no dirty-block *transfer* is claimed here; that is proven
#      at protocol level by the in-repo e2e test, not by deployment-realistic
#      guest writes) → PAUSED_FINAL_SYNC with needs_vm_pause=true →
#      ResumeDiskMigration{vm_paused:true} → COMPLETED → harness-level
#      digest + byte-compare of source vs destination files.
#   N1 missing TLS config: trigger on the DEPLOYED stord (no [migration])
#      → task Failed with the mTLS-required error; plus the startup-vs-
#      trigger distinction — client/receiver fields with enabled=false are
#      STARTUP errors (#393/#396).
#   N2 wrong CA (SRC trusts an unrelated CA)          → trigger-time Failed.
#   N3 wrong server name (dest_server_name ≠ SAN)     → trigger-time Failed.
#   N4 wrong destination identity (SRC's client cert signed by the
#      unrelated CA; DST still trusts the good CA)    → handshake rejected
#      at DST → trigger-time Failed.
#   N5 mismatched keypairs (client half + server half) → STARTUP exit.
#   N6 malformed cert/key/CA (+ empty CA on the server half) → STARTUP exit.
#   N7 expired client leaf (openssl ca backdating recipe) → trigger-time
#      Failed (handshake-time expiry check at DST).
#   N8 plaintext/downgrade: dest_endpoint http://… → the sender force-
#      upgrades to https (asserted via the SRC log line) and the TLS
#      handshake against a non-TLS endpoint fails → Failed. Together with
#      N1 (no TLS config ⇒ refused) this proves there is no plaintext
#      migration path.
#   N9 interrupted transfer + deterministic recovery: SIGKILL DST during
#      BULK_COPY → task Failed; restart DST; re-trigger → the receiver
#      REFUSES to truncate the partial receiving volume (create_new,
#      documented operator step: remove it); remove + re-trigger →
#      COMPLETED + digest match.
#
# Non-claims (recorded in the evidence doc):
#   - Concurrent-write migration (#394): on a real host this scenario's
#     legs are quiescent-only. Since the #394 Option A write canary, a
#     concurrent source write fails the migration fail-fast at the
#     first dirty-round boundary — proven at protocol level by
#     crates/chv-stord-core/tests/migration_e2e.rs
#     (concurrent_source_write_fails_fast; dirty-block transfer itself
#     by dirty_rounds_converge_preseeded_writes — [renamed 2026-10-08,
#     #394 Option A: was dirty_rounds_transfer_concurrent_writes]). No
#     real-host concurrent-writer leg has run.
#   - The CP-orchestrated migration path (agent migrate_vm) fails closed
#     in core-managed mode by design (single-writer enforcement) and is
#     NOT exercised; this scenario drives stord↔stord directly.
#   - Multi-host migration, non-local backends: out of scope (declaration).
#
# Run via deploy.sh --exec (as root, on the qualification host):
#   sudo env "PATH=$PATH" GUEST_IMAGE=noble-qual-patched.img \
#     ./scripts/integration/qual/deploy.sh --exec \
#     ./scripts/integration/qual/m4.6-migration.sh
# ---------------------------------------------------------------------------

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "${SCRIPT_DIR}/lib.sh"

# --- deployment map (set by deploy.sh --exec) ---
for var in QUAL_TEST_DIR QUAL_LOGS_DIR QUAL_BINARY_DIR QUAL_STORD_PID; do
    [ -n "${!var:-}" ] || qual_die "${var} not set — run via deploy.sh --exec"
done

REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
M46_DIR="${QUAL_TEST_DIR}/m46"          # ALL scenario resources live here
CERTS_DIR="${M46_DIR}/certs"
GRPCURL_DIR="${M46_DIR}/grpcurl"
EVIDENCE_DIR="${CHV_QUAL_ROOT:-/var/lib/chv/qual}/m4.6-artifacts"
mkdir -p "$EVIDENCE_DIR"

CHV_STORD="${QUAL_BINARY_DIR}/chv-stord"
DEPLOY_STORD_SOCK="${QUAL_TEST_DIR}/stord/api.sock"
DEPLOY_STORD_LOG="${QUAL_LOGS_DIR}/stord.log"

# Pinned grpcurl (supply-chain discipline: pinned version + checksum
# verification against the release's own checksums file; qual_die on
# mismatch). NOTE the release names the checksum asset
# "grpcurl_<ver>_checksums.txt" (not "checksums.txt").
GRPCURL_VERSION="1.9.3"
GRPCURL_TARBALL="grpcurl_${GRPCURL_VERSION}_linux_x86_64.tar.gz"
GRPCURL_CHECKSUMS="grpcurl_${GRPCURL_VERSION}_checksums.txt"
GRPCURL_CHECKSUMS_ASSET="${SCRIPT_DIR}/assets/${GRPCURL_CHECKSUMS}"
GRPCURL_BASE="https://github.com/fullstorydev/grpcurl/releases/download/v${GRPCURL_VERSION}"

# Scenario ports (loopback only; asserted free before use).
DST_PORT="51052"      # DST mTLS receiver listener
PLAIN_PORT="51053"    # N8 throwaway plaintext echo listener

# The migration volume: 4 GiB = 1024 chunks of the 4 MiB
# DIRTY_TRACKING_BLOCK_SIZE — large enough that BULK_COPY is observable
# via status polling and long enough to SIGKILL the destination mid-copy.
VOL_SIZE_BYTES=$((4 * 1024 * 1024 * 1024))
SEED_FILE="${M46_DIR}/m46-seed.img"

RPC_TIMEOUT=180       # per status poll loop budget (transfer + digest)
STARTUP_TIMEOUT=20    # stord startup / expected-failure guard

# Every scenario-owned process (standalone stords + the N8 listener).
M46_PIDS=()

# ---------------------------------------------------------------------------
# Cleanup — owns ALL scenario resources on every exit path (the deploy's
# own teardown knows nothing about the standalone stords; a leaked one would
# survive deploy cleanup, so the scoped pkill fallback is load-bearing).
# ---------------------------------------------------------------------------
scenario_cleanup() {
    local pid alive
    for pid in "${M46_PIDS[@]:-}"; do
        [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
    done
    for _ in $(seq 1 25); do
        alive=0
        for pid in "${M46_PIDS[@]:-}"; do
            if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
                alive=1
            fi
        done
        [ "$alive" -eq 0 ] && break
        sleep 0.2
    done
    for pid in "${M46_PIDS[@]:-}"; do
        [ -n "$pid" ] && kill -9 "$pid" 2>/dev/null || true
    done
    # Safety net for any scenario stord not in the pid list. Scoped to
    # THIS scenario's config dir and argv[0]-anchored (same self-match-proof
    # form deploy.sh uses) so the deployed stord and unrelated host
    # processes can never match.
    pkill -f "(^|/)chv-stord( |$).*${M46_DIR}" 2>/dev/null || true
    sleep 0.5
    pkill -9 -f "(^|/)chv-stord( |$).*${M46_DIR}" 2>/dev/null || true
}
trap scenario_cleanup EXIT
trap 'exit 1' INT TERM

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
qual_info "=== M4.6 two-stord mTLS migration qualification start (node ${QUAL_NODE_ID:-}) ==="
qual_info "candidate: $(cat "${QUAL_BINARY_DIR}/CANDIDATE_SHA" 2>/dev/null || echo unknown)"

# Identity guard: the whole scenario tests the staged chv-stord's migration
# code — the staged candidate must be identical to THIS tree for the crates
# (and proto) that implement it. (The scenario commit itself touches only
# scripts/docs, so the guard passes with binaries staged from any commit in
# this range that left crates/proto untouched.)
CANDIDATE_SHA="$(cat "${QUAL_BINARY_DIR}/CANDIDATE_SHA" 2>/dev/null || true)"
if [ -n "$CANDIDATE_SHA" ]; then
    if git -C "$REPO_ROOT" diff --quiet "$CANDIDATE_SHA" HEAD -- \
        crates/chv-stord-core crates/chv-stord-backends crates/chv-config \
        cmd/chv-stord proto/node; then
        qual_pass "candidate identity: migration-relevant crates identical at ${CANDIDATE_SHA:0:8} and HEAD"
    else
        qual_die "migration-relevant crates DIFFER between candidate ${CANDIDATE_SHA:0:8} and HEAD — the staged binaries would not test this tree"
    fi
else
    qual_warn "no candidate sha available — identity check skipped; results apply to THIS tree only"
fi

# provision_grpcurl — download the pinned release and verify its checksum
# against the CHECKED-IN copy of the release's checksums.txt
# (scripts/integration/qual/assets/grpcurl_1.9.3_checksums.txt). Vendoring
# the checksums gives an immutable, reviewable anchor: a later compromise
# of the release assets cannot move both halves together at runtime.
provision_grpcurl() {
    command -v curl >/dev/null 2>&1 || qual_die "curl missing (env-preflight)"
    command -v tar >/dev/null 2>&1 || qual_die "tar missing"
    [ -s "$GRPCURL_CHECKSUMS_ASSET" ] \
        || qual_die "checked-in grpcurl checksums asset missing: ${GRPCURL_CHECKSUMS_ASSET}"
    mkdir -p "$GRPCURL_DIR"
    qual_info "downloading grpcurl v${GRPCURL_VERSION} (official release) ..."
    curl -fsSL --retry 3 -o "${GRPCURL_DIR}/${GRPCURL_TARBALL}" \
        "${GRPCURL_BASE}/${GRPCURL_TARBALL}" \
        || qual_die "grpcurl download failed (${GRPCURL_BASE}/${GRPCURL_TARBALL})"
    # Isolate the expected line and verify with sha256sum -c (the line is
    # "<sha256>  <tarball>"; grep with an end anchor, NOT -F — a literal
    # '$' would never match).
    grep " ${GRPCURL_TARBALL}\$" "$GRPCURL_CHECKSUMS_ASSET" \
        > "${GRPCURL_DIR}/expected.sha256" \
        || qual_die "no checksum entry for ${GRPCURL_TARBALL} in the checked-in ${GRPCURL_CHECKSUMS_ASSET}"
    if (cd "$GRPCURL_DIR" && sha256sum -c expected.sha256 >/dev/null 2>&1); then
        qual_pass "grpcurl v${GRPCURL_VERSION} checksum verified against the checked-in release checksums"
    else
        qual_die "grpcurl checksum VERIFICATION FAILED — refusing to run unverified binaries"
    fi
    tar -xzf "${GRPCURL_DIR}/${GRPCURL_TARBALL}" -C "$GRPCURL_DIR" grpcurl \
        || qual_die "grpcurl extraction failed"
    chmod 0755 "${GRPCURL_DIR}/grpcurl"
    GRPCURL="${GRPCURL_DIR}/grpcurl"
    qual_info "grpcurl: $("$GRPCURL" --version 2>&1 | head -1), sha256 $(sha256_of "${GRPCURL_DIR}/${GRPCURL_TARBALL}")"
    {
        echo "### m4.6 grpcurl provisioning record ($(date -u +%FT%TZ))"
        echo "verified against checked-in asset: ${GRPCURL_CHECKSUMS_ASSET}"
        cat "${GRPCURL_DIR}/expected.sha256"
        "$GRPCURL" --version 2>&1 | head -1
    } >> "${EVIDENCE_DIR}/grpcurl-provisioning.txt"
}
provision_grpcurl

# stord_rpc SOCK METHOD JSON — StorageService call over the UDS with the
# M2.5-RC4-proven grpcurl invocation (unix:// scheme, -plaintext, explicit
# proto; no reflection on node daemons). Run as root (sockets are 0600).
stord_rpc() {
    local sock="$1" method="$2" json="$3"
    "$GRPCURL" -plaintext \
        -import-path "${REPO_ROOT}/proto/node" -proto chv-stord-api.proto \
        -d "$json" "unix://${sock}" \
        "chv.node.stord.v1.StorageService/${method}"
}

# open_volume SOCK VOLID LOCATOR SIZE [SEED] → attachment_handle on stdout.
open_volume() {
    local sock="$1" volid="$2" locator="$3" size="$4" seed="${5:-}"
    local opts json resp status handle
    if [ -n "$seed" ]; then
        opts="{\"size_bytes\":\"${size}\",\"seed_from\":\"${seed}\"}"
    else
        opts="{\"size_bytes\":\"${size}\"}"
    fi
    json="{\"volumeId\":\"${volid}\",\"backend\":{\"backendClass\":\"local\",\"locator\":\"${locator}\",\"options\":${opts}}}"
    resp="$(stord_rpc "$sock" OpenVolume "$json")" \
        || { qual_error "OpenVolume RPC failed for ${volid}"; return 1; }
    status="$(printf '%s' "$resp" | jq -r '.result.status // empty')"
    handle="$(printf '%s' "$resp" | jq -r '.attachmentHandle // empty')"
    if [ "$status" = "OK" ] && [ -n "$handle" ]; then
        printf '%s' "$handle"
        return 0
    fi
    qual_error "OpenVolume for ${volid} not accepted (status=${status}): $(printf '%s' "$resp" | head -c 300)"
    return 1
}

# close_volume SOCK VOLID HANDLE
close_volume() {
    local sock="$1" volid="$2" handle="$3"
    local resp status
    resp="$(stord_rpc "$sock" CloseVolume \
        "{\"volumeId\":\"${volid}\",\"attachmentHandle\":\"${handle}\"}")" \
        || { qual_error "CloseVolume RPC failed for ${volid}"; return 1; }
    # CloseVolume returns a bare Result (no wrapper object) — unlike
    # OpenVolume/TriggerDiskMigration which wrap it as .result.
    status="$(printf '%s' "$resp" | jq -r '.status // empty')"
    if [ "$status" = "OK" ]; then
        qual_pass "CloseVolume accepted for ${volid} (session closed)"
        return 0
    fi
    qual_error "CloseVolume for ${volid} not OK (status=${status})"
    return 1
}

# trigger_migration SOCK VOLID HANDLE ENDPOINT → migration_id on stdout.
trigger_migration() {
    local sock="$1" volid="$2" handle="$3" endpoint="$4"
    local json resp status mid
    json="{\"volumeId\":\"${volid}\",\"attachmentHandle\":\"${handle}\",\"destEndpoint\":\"${endpoint}\"}"
    resp="$(stord_rpc "$sock" TriggerDiskMigration "$json")" \
        || { qual_error "TriggerDiskMigration RPC failed for ${volid}"; return 1; }
    status="$(printf '%s' "$resp" | jq -r '.result.status // empty')"
    mid="$(printf '%s' "$resp" | jq -r '.migrationId // empty')"
    if [ "$status" = "OK" ] && [ -n "$mid" ]; then
        printf '%s' "$mid"
        return 0
    fi
    qual_error "TriggerDiskMigration for ${volid} not accepted (status=${status}): $(printf '%s' "$resp" | head -c 300)"
    return 1
}

# migration_status SOCK MID → raw status JSON (-emit-defaults so zero-valued
# fields like dirtyBlocksRemaining=0 are actually present — an absent field
# must never be read as a value).
migration_status() {
    "$GRPCURL" -plaintext -emit-defaults \
        -import-path "${REPO_ROOT}/proto/node" -proto chv-stord-api.proto \
        -d "{\"migrationId\":\"$2\"}" "unix://$1" \
        "chv.node.stord.v1.StorageService/GetDiskMigrationStatus"
}

migration_field() {
    local field="$3"
    migration_status "$1" "$2" | jq -r ".${field} // empty" 2>/dev/null
}

phase_is() {
    [ "$(migration_field "$1" "$2" phase)" = "$3" ]
}

# poll_phases_seen SOCK MID TIMEOUT — tight (0.3 s) status polling until the
# task reaches a stable/terminal-for-polling phase; records every phase
# observed in PHASES_SEEN. wait_for's 2 s cadence is too coarse for the
# transient BULK_COPY/DIRTY_SYNC phases of a fast loopback transfer.
PHASES_SEEN=""
poll_phases_seen() {
    local sock="$1" mid="$2" timeout="$3"
    local p deadline=$((SECONDS + timeout))
    PHASES_SEEN=""
    while :; do
        p="$(migration_field "$sock" "$mid" phase)"
        if [ -n "$p" ]; then
            case ",${PHASES_SEEN}," in
                *",${p},"*) ;;
                *) PHASES_SEEN="${PHASES_SEEN:+${PHASES_SEEN},}${p}" ;;
            esac
            case "$p" in
                PAUSED_FINAL_SYNC | FAILED | COMPLETED) break ;;
            esac
        fi
        [ "$SECONDS" -ge "$deadline" ] && break
        sleep 0.3
    done
}

# record_migration_error LABEL SOCK MID — capture a FAILED task's error
# message into the persistent evidence record (full text, once).
record_migration_error() {
    {
        echo "### $1 ($(date -u +%FT%TZ))"
        migration_status "$2" "$3" 2>/dev/null || true
        echo
    } >> "${EVIDENCE_DIR}/failed-migrations.jsonl"
}

# start_scenario_stord NAME CONFIG LOG → PID; registers the process with the
# cleanup trap. disown: scenarios may SIGKILL these daemons (N9) — without
# it bash prints a "Killed" job-control notice into the evidence log.
start_scenario_stord() {
    local config="$1" log="$2" pid
    "${CHV_STORD}" "$config" > "$log" 2>&1 &
    pid=$!
    disown "$pid"
    M46_PIDS+=("$pid")
    printf '%s' "$pid"
}

# stop_scenario_stord PID LOG_LABEL — SIGTERM, wait, SIGKILL; asserts exit.
stop_scenario_stord() {
    local pid="$1" label="$2"
    kill "$pid" 2>/dev/null || true
    local _
    for _ in $(seq 1 50); do
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.2
    done
    if kill -0 "$pid" 2>/dev/null; then
        kill -9 "$pid" 2>/dev/null || true
        qual_error "stord ${label} did not exit on SIGTERM (SIGKILLed)"
        return 1
    fi
    qual_pass "stord ${label} stopped cleanly (SIGTERM)"
}

# expect_stord_startup_failure DESC CONFIG LOG NEEDLE — the fail-closed
# startup contract: the daemon must EXIT non-zero and name the cause.
expect_stord_startup_failure() {
    local desc="$1" config="$2" log="$3" needle="$4" rc=0
    timeout "$STARTUP_TIMEOUT" "${CHV_STORD}" "$config" > "$log" 2>&1 || rc=$?
    if [ "$rc" -eq 124 ]; then
        qual_error "${desc}: stord did NOT exit within ${STARTUP_TIMEOUT}s — fail-closed startup violated"
        return 1
    fi
    if [ "$rc" -eq 0 ]; then
        qual_error "${desc}: stord exited 0 — expected a startup failure"
        return 1
    fi
    qual_pass "${desc}: stord exited at startup (rc=${rc})"
    assert_file_contains "${desc}: error names the cause" "$log" "$needle"
}

# tcp_port_free PORT — a loopback bind on the port succeeds. SO_REUSEADDR is
# set so that TIME_WAIT sockets left by closed migration connections do not
# read as "in use" (an active listener still fails the bind, which is the
# semantic we want: can a fresh listener bind here?).
tcp_port_free() {
    python3 - "$1" <<'PYEOF' 2>/dev/null
import socket, sys
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
try:
    s.bind(("127.0.0.1", int(sys.argv[1])))
    raise SystemExit(0)
except OSError:
    raise SystemExit(1)
finally:
    s.close()
PYEOF
}

# tcp_port_open PORT — a loopback connect to the port succeeds.
tcp_port_open() {
    python3 - "$1" <<'PYEOF' 2>/dev/null
import socket, sys
s = socket.socket()
s.settimeout(2)
try:
    s.connect(("127.0.0.1", int(sys.argv[1])))
    raise SystemExit(0)
except OSError:
    raise SystemExit(1)
finally:
    s.close()
PYEOF
}

# ---------------------------------------------------------------------------
# Certificates (openssl, X.509 **v3** via extfiles — rustls/webpki rejects
# v1; the deploy.sh-documented trap) + negative-matrix material.
# ---------------------------------------------------------------------------
qual_info "generating migration identities (one good CA, leaves, negative matrix)"

mkdir -p "$CERTS_DIR" "${M46_DIR}/openssl-ca/newcerts"
touch "${M46_DIR}/openssl-ca/index.txt"
echo 1000 > "${M46_DIR}/openssl-ca/serial"

# Good CA (2-day validity — throwaway, like the deploy CA).
openssl genrsa -out "${CERTS_DIR}/ca.key" 2048 2>/dev/null
openssl req -x509 -new -nodes -key "${CERTS_DIR}/ca.key" -sha256 -days 2 \
    -out "${CERTS_DIR}/ca.crt" \
    -subj "/O=CHV Qual M46/CN=m46-qual-ca" 2>/dev/null

# Unrelated CA (valid, wrong trust domain).
openssl genrsa -out "${CERTS_DIR}/ca2.key" 2048 2>/dev/null
openssl req -x509 -new -nodes -key "${CERTS_DIR}/ca2.key" -sha256 -days 2 \
    -out "${CERTS_DIR}/ca2.crt" \
    -subj "/O=CHV Qual M46/CN=m46-unrelated-ca" 2>/dev/null

# make_leaf NAME CN EXTFILE-CA-CRT EXTFILE-CA-KEY EKU-EXT — issues a v3
# leaf from the given CA.
make_leaf() {
    local name="$1" cn="$2" ca_crt="$3" ca_key="$4" ext="$5"
    openssl genrsa -out "${CERTS_DIR}/${name}.key" 2048 2>/dev/null
    openssl req -new -key "${CERTS_DIR}/${name}.key" -out "${CERTS_DIR}/${name}.csr" \
        -subj "/O=CHV Qual M46/CN=${cn}" 2>/dev/null
    printf '%s\n' "$ext" > "${CERTS_DIR}/${name}.ext"
    openssl x509 -req -in "${CERTS_DIR}/${name}.csr" \
        -CA "$ca_crt" -CAkey "$ca_key" -CAcreateserial \
        -out "${CERTS_DIR}/${name}.crt" -days 2 -sha256 \
        -extfile "${CERTS_DIR}/${name}.ext" 2>/dev/null
    rm -f "${CERTS_DIR}/${name}.csr" "${CERTS_DIR}/${name}.ext"
}

CLIENT_EXT="basicConstraints = CA:FALSE
keyUsage = digitalSignature, keyEncipherment
extendedKeyUsage = clientAuth"

SERVER_EXT="basicConstraints = CA:FALSE
keyUsage = digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = DNS:localhost, IP:127.0.0.1"

# Positive-leg identities: SRC client leaf + DST server leaf (SAN covers
# both the endpoint IP and the dest_server_name DNS name) + a DST client
# leaf. NOTE (product truth, recorded in the evidence doc): migration
# receivers are NOT expressible without a client identity — `enabled =
# true` makes load_migration_tls require all four client fields, so the
# destination must carry an (unused-by-these-legs) client identity too;
# only the source-only direction is expressible.
make_leaf src-client m46-src-client "${CERTS_DIR}/ca.crt" "${CERTS_DIR}/ca.key" "$CLIENT_EXT"
make_leaf dst-server m46-dst-server "${CERTS_DIR}/ca.crt" "${CERTS_DIR}/ca.key" "$SERVER_EXT"
make_leaf dst-client m46-dst-client "${CERTS_DIR}/ca.crt" "${CERTS_DIR}/ca.key" "$CLIENT_EXT"

# N4 material: a client leaf signed by the UNRELATED CA (DST still trusts
# the good CA as its client_ca).
make_leaf rogue-client m46-rogue-client "${CERTS_DIR}/ca2.crt" "${CERTS_DIR}/ca2.key" "$CLIENT_EXT"

# N7 material: an EXPIRED client leaf signed by the GOOD CA (so the chain
# is trusted and only validity fails). `openssl x509 -req` cannot backdate;
# `openssl ca` with explicit -startdate/-enddate can (minimal CA database).
openssl genrsa -out "${CERTS_DIR}/expired-client.key" 2048 2>/dev/null
openssl req -new -key "${CERTS_DIR}/expired-client.key" \
    -out "${CERTS_DIR}/expired-client.csr" \
    -subj "/O=CHV Qual M46/CN=m46-expired-client" 2>/dev/null
cat > "${M46_DIR}/openssl-ca/ca.cnf" <<EOF
[ca]
default_ca = CA_default
[CA_default]
dir = ${M46_DIR}/openssl-ca
database = \$dir/index.txt
new_certs_dir = \$dir/newcerts
serial = \$dir/serial
certificate = ${CERTS_DIR}/ca.crt
private_key = ${CERTS_DIR}/ca.key
default_md = sha256
policy = policy_any
x509_extensions = v3_client
[policy_any]
commonName = supplied
[v3_client]
basicConstraints = CA:FALSE
keyUsage = digitalSignature, keyEncipherment
extendedKeyUsage = clientAuth
EOF
openssl ca -batch -notext -config "${M46_DIR}/openssl-ca/ca.cnf" \
    -in "${CERTS_DIR}/expired-client.csr" \
    -out "${CERTS_DIR}/expired-client.crt" \
    -startdate 20200101000000Z -enddate 20210101000000Z \
    > "${M46_DIR}/openssl-ca/ca-sign.log" 2>&1 \
    || qual_die "openssl ca failed to issue the expired leaf (see ${M46_DIR}/openssl-ca/ca-sign.log)"
rm -f "${CERTS_DIR}/expired-client.csr"

chmod 644 "$CERTS_DIR"/*.crt
chmod 600 "$CERTS_DIR"/*.key

# Assert the generated material is what the legs claim (no vacuous legs).
if openssl x509 -in "${CERTS_DIR}/expired-client.crt" -noout -checkend 0 >/dev/null 2>&1; then
    qual_error "N7 precondition: expired-client.crt is NOT expired (backdating failed)"
else
    qual_pass "N7 precondition: expired-client.crt is genuinely expired (checkend)"
fi
assert_contains "N7 precondition: expired leaf carries the clientAuth EKU" \
    "$(openssl x509 -in "${CERTS_DIR}/expired-client.crt" -noout -text 2>/dev/null)" \
    "TLS Web Client Authentication"
if openssl verify -CAfile "${CERTS_DIR}/ca.crt" "${CERTS_DIR}/src-client.crt" >/dev/null 2>&1; then
    qual_pass "good CA chain verifies (src-client)"
else
    qual_error "good CA chain does not verify — positive leg would be vacuous"
fi
if openssl verify -CAfile "${CERTS_DIR}/ca2.crt" "${CERTS_DIR}/src-client.crt" >/dev/null 2>&1; then
    qual_error "src-client unexpectedly verifies against the UNRELATED CA — N2 would be vacuous"
else
    qual_pass "src-client does NOT verify against the unrelated CA (N2 precondition)"
fi
if openssl verify -CAfile "${CERTS_DIR}/ca.crt" "${CERTS_DIR}/rogue-client.crt" >/dev/null 2>&1; then
    qual_error "rogue-client unexpectedly verifies against the good CA — N4 would be vacuous"
else
    qual_pass "rogue-client does NOT verify against the good CA (N4 precondition)"
fi

# ---------------------------------------------------------------------------
# Port preconditions
# ---------------------------------------------------------------------------
if tcp_port_free "$DST_PORT"; then
    qual_pass "destination port ${DST_PORT} is free before startup"
else
    qual_die "destination port ${DST_PORT} is already in use — refusing to run"
fi
if tcp_port_free "$PLAIN_PORT"; then
    qual_pass "plaintext-listener port ${PLAIN_PORT} is free before startup"
else
    qual_die "plaintext-listener port ${PLAIN_PORT} is already in use — refusing to run"
fi

# ---------------------------------------------------------------------------
# Config writers
# ---------------------------------------------------------------------------
write_src_config() {
    # write_src_config PATH SOCKET RUNTIME CA CERT KEY DEST_NAME
    cat > "$1" <<EOF
socket_path = "${2}"
runtime_dir = "${3}"
log_level = "info"
# Port 0: scenario stords run alongside the deployed stord (9101) and up to
# two at once; a fixed default would collide (#687 makes the metrics bind
# fatal). The harness never scrapes these listeners.
metrics_bind = "127.0.0.1:0"
path_allowlist = ["${3}"]

[migration]
enabled = true
client_cert_path = "${5}"
client_key_path = "${6}"
ca_cert_path = "${4}"
dest_server_name = "${7}"
EOF
}

write_dst_config() {
    # write_dst_config PATH SOCKET RUNTIME PORT CERT KEY CA — receiver
    # fields plus the (structurally required, see the identity note above)
    # client identity: dest_server_name points at itself, harmless.
    cat > "$1" <<EOF
socket_path = "${2}"
runtime_dir = "${3}"
log_level = "info"
metrics_bind = "127.0.0.1:0"
path_allowlist = ["${3}"]

[migration]
enabled = true
client_cert_path = "${CERTS_DIR}/dst-client.crt"
client_key_path = "${CERTS_DIR}/dst-client.key"
ca_cert_path = "${7}"
dest_server_name = "localhost"
listen_addr = "127.0.0.1:${4}"
server_cert_path = "${5}"
server_key_path = "${6}"
client_ca_path = "${7}"
EOF
}

# Seed file: 4 GiB of non-zero patterned content (one random 4 MiB block
# repeated) — no sparse chunks, a non-trivial full-volume digest, fast to
# generate.
qual_info "preparing the ${VOL_SIZE_BYTES}-byte patterned seed ..."
python3 - "$SEED_FILE" "$VOL_SIZE_BYTES" <<'PYEOF'
import os, sys
path, size = sys.argv[1], int(sys.argv[2])
block = os.urandom(4 * 1024 * 1024)
with open(path, "wb") as f:
    written = 0
    while written < size:
        n = min(len(block), size - written)
        f.write(block[:n])
        written += n
    f.flush()
    os.fsync(f.fileno())
PYEOF
[ "$(stat -c %s "$SEED_FILE")" = "$VOL_SIZE_BYTES" ] \
    && qual_pass "seed file prepared (${VOL_SIZE_BYTES} bytes, patterned)" \
    || qual_die "seed file preparation failed"

# ===========================================================================
# Leg P — positive: two-stord mTLS migration, full protocol + digest
# ===========================================================================
qual_info "--- Leg P: SRC + DST standalone stords → seeded volume → mTLS migration → verified destination"

SRC_DIR="${M46_DIR}/src"; DST_DIR="${M46_DIR}/dst"
mkdir -p "$SRC_DIR" "$DST_DIR"
SRC_SOCK="${SRC_DIR}/api.sock"; DST_SOCK="${DST_DIR}/api.sock"
SRC_LOG="${SRC_DIR}/stord.log"; DST_LOG="${DST_DIR}/stord.log"

write_src_config "${SRC_DIR}/stord.toml" "$SRC_SOCK" "$SRC_DIR" \
    "${CERTS_DIR}/ca.crt" "${CERTS_DIR}/src-client.crt" "${CERTS_DIR}/src-client.key" \
    "localhost"
write_dst_config "${DST_DIR}/stord.toml" "$DST_SOCK" "$DST_DIR" "$DST_PORT" \
    "${CERTS_DIR}/dst-server.crt" "${CERTS_DIR}/dst-server.key" "${CERTS_DIR}/ca.crt"

SRC_PID="$(start_scenario_stord "${SRC_DIR}/stord.toml" "$SRC_LOG")"
DST_PID="$(start_scenario_stord "${DST_DIR}/stord.toml" "$DST_LOG")"

wait_for "SRC stord UDS up" 20 test -S "$SRC_SOCK" \
    || qual_die "SRC stord did not come up — log: $(tail -20 "$SRC_LOG" 2>/dev/null)"
wait_for "DST stord UDS up" 20 test -S "$DST_SOCK" \
    || qual_die "DST stord did not come up — log: $(tail -20 "$DST_LOG" 2>/dev/null)"
assert_process_alive "SRC stord alive" "$SRC_PID"
assert_process_alive "DST stord alive" "$DST_PID"

# The mTLS wiring, asserted from the daemons' own startup evidence.
assert_file_contains "SRC logged migration mTLS enabled (credentials validated at startup)" \
    "$SRC_LOG" "storage migration mTLS enabled"
# The source-only shape is legitimate (enabled=true + no receiver fields):
# SRC must NOT open an inbound listener.
assert_file_contains "SRC is source-only (receiver listener not configured)" \
    "$SRC_LOG" "storage migration receiver listener not configured"
assert_file_contains "DST logged the receiver listener bound (mTLS, client auth required)" \
    "$DST_LOG" "storage migration receiver listening on 127.0.0.1:${DST_PORT} (mTLS, client auth required)"
tcp_port_open "$DST_PORT" \
    && qual_pass "DST receiver TCP listener accepts connections on ${DST_PORT}" \
    || qual_error "DST receiver TCP listener not reachable on ${DST_PORT}"
# The deployed stord (no [migration]) is the contrast case: disabled, not broken.
assert_file_contains "deployed stord logged migration disabled (no [migration] config)" \
    "$DEPLOY_STORD_LOG" "storage migration is disabled"

# Seeded source volume on SRC (known content ⇒ non-trivial digest).
VOL1_ID="m46vol1"
HANDLE1="$(open_volume "$SRC_SOCK" "$VOL1_ID" "vol1.img" "$VOL_SIZE_BYTES" "$SEED_FILE")" \
    || qual_die "aborting (Leg P)"
qual_pass "OpenVolume on SRC: ${VOL1_ID} seeded from the patterned file (handle ${HANDLE1})"
[ "$(stat -c %s "${SRC_DIR}/vol1.img" 2>/dev/null)" = "$VOL_SIZE_BYTES" ] \
    && qual_pass "source volume materialized at full size (${SRC_DIR}/vol1.img)" \
    || qual_error "source volume size mismatch: ${SRC_DIR}/vol1.img"

MID1="$(trigger_migration "$SRC_SOCK" "$VOL1_ID" "$HANDLE1" "https://127.0.0.1:${DST_PORT}")" \
    || qual_die "aborting (Leg P)"
qual_pass "TriggerDiskMigration accepted → migration_id ${MID1} (dest https://127.0.0.1:${DST_PORT})"

# Phase observation. BULK_COPY must be visible in status polling (4 GiB =
# 1024 chunks keeps the phase alive for multiple 0.3 s samples); the
# transient DIRTY_SYNC pass-through is recorded, and the dirty-round
# machinery itself is asserted from the SRC log below.
poll_phases_seen "$SRC_SOCK" "$MID1" "$RPC_TIMEOUT"
case ",${PHASES_SEEN}," in
    *,BULK_COPY,*) qual_pass "BULK_COPY observed via GetDiskMigrationStatus (phases seen: ${PHASES_SEEN})" ;;
    *) qual_error "BULK_COPY never observed in status polling (phases seen: ${PHASES_SEEN})" ;;
esac
case ",${PHASES_SEEN}," in
    *,DIRTY_SYNC,*) qual_info "transient DIRTY_SYNC phase was visible in status polling" ;;
    *) qual_info "DIRTY_SYNC was too transient for status polling — the phase execution is asserted from the SRC log below" ;;
esac

# The pause handshake: the task must park in PAUSED_FINAL_SYNC demanding a
# VM pause (the pre-copy convergence point) — the operator gate.
wait_for "migration reached PAUSED_FINAL_SYNC" "$RPC_TIMEOUT" \
    phase_is "$SRC_SOCK" "$MID1" "PAUSED_FINAL_SYNC" \
    || { record_migration_error "leg-P no-pause" "$SRC_SOCK" "$MID1"; qual_die "Leg P: migration never paused for final sync"; }
PAUSE_NEEDS="$(migration_field "$SRC_SOCK" "$MID1" needsVmPause)"
[ "$PAUSE_NEEDS" = "true" ] \
    && qual_pass "PAUSED_FINAL_SYNC demands the VM pause (needs_vm_pause=true)" \
    || qual_error "needs_vm_pause is '${PAUSE_NEEDS}' at PAUSED_FINAL_SYNC (expected true)"
PAUSE_DIRTY="$(migration_field "$SRC_SOCK" "$MID1" dirtyBlocksRemaining)"
[ "$PAUSE_DIRTY" = "0" ] \
    && qual_pass "dirty-round machinery converged at 0 dirty blocks (quiescent source)" \
    || qual_error "dirtyBlocksRemaining is '${PAUSE_DIRTY}' at pause (expected 0 on a quiescent source)"

# The #394 boundary, asserted from the daemon's own trail: the dirty-round
# machinery RAN (round 1 executed and found nothing to send on a quiescent
# source). This is NOT a claim of dirty-block transfer under concurrent
# writes — that is protocol-level only (in-repo e2e test).
assert_file_contains "SRC log: dirty-sync phase executed (rounds started)" \
    "$SRC_LOG" "starting iterative dirty sync rounds"
assert_file_contains "SRC log: round 1 converged with no dirty blocks (quiescent source — #394 boundary)" \
    "$SRC_LOG" "no dirty blocks remaining"

# Resume → FinalSync → digest-verified finalize → COMPLETED.
stord_rpc "$SRC_SOCK" ResumeDiskMigration \
    "{\"migrationId\":\"${MID1}\",\"vmPaused\":true}" > "${EVIDENCE_DIR}/leg-p-resume.json" 2>/dev/null \
    && qual_pass "ResumeDiskMigration{vm_paused:true} accepted" \
    || qual_error "ResumeDiskMigration RPC failed"
wait_for "migration COMPLETED (destination digest verified at finalize)" "$RPC_TIMEOUT" \
    phase_is "$SRC_SOCK" "$MID1" "COMPLETED" \
    || { record_migration_error "leg-p not-completed" "$SRC_SOCK" "$MID1"; qual_die "Leg P: migration never completed"; }
TOTAL1="$(migration_field "$SRC_SOCK" "$MID1" totalBytes)"
[ "$TOTAL1" = "$VOL_SIZE_BYTES" ] \
    && qual_pass "status reports the full volume size (total_bytes=${TOTAL1})" \
    || qual_error "total_bytes is '${TOTAL1}' (expected ${VOL_SIZE_BYTES})"

# Harness-level verification INDEPENDENT of the protocol digest implied by
# COMPLETED: sha256 + byte-compare of the source and destination files.
SRC_VOL1="${SRC_DIR}/vol1.img"
DST_VOL1="${DST_DIR}/${VOL1_ID}.img"
assert_file_exists "destination volume file materialized (${DST_VOL1})" "$DST_VOL1"
SRC_SHA="$(sha256_of "$SRC_VOL1")"
DST_SHA="$(sha256_of "$DST_VOL1")"
[ "$SRC_SHA" = "$DST_SHA" ] \
    && qual_pass "sha256(source) == sha256(destination): ${SRC_SHA:0:16}…" \
    || qual_error "digest mismatch: src=${SRC_SHA} dst=${DST_SHA}"
if cmp -s "$SRC_VOL1" "$DST_VOL1"; then
    qual_pass "byte-compare: destination is bit-identical to the source (${VOL_SIZE_BYTES} bytes)"
else
    qual_error "byte-compare failed for ${VOL1_ID}"
fi
{
    echo "### leg-P digest record ($(date -u +%FT%TZ))"
    echo "src ${SRC_VOL1} ${SRC_SHA}"
    echo "dst ${DST_VOL1} ${DST_SHA}"
} >> "${EVIDENCE_DIR}/digests.txt"
cp "$SRC_LOG" "${EVIDENCE_DIR}/leg-p-src-stord.log" 2>/dev/null || true
cp "$DST_LOG" "${EVIDENCE_DIR}/leg-p-dst-stord.log" 2>/dev/null || true

# ===========================================================================
# Leg N1 — missing TLS config
# ===========================================================================
qual_info "--- Leg N1: missing TLS config — deployed stord trigger fails closed; half-configured disabled stords exit at startup"

# N1a: the DEPLOYED stord (core-managed stack, NO [migration] section) —
# trigger must be accepted (journaled task) and the task must fail with the
# mTLS-required error. Ties the deployed stack into the scenario.
N1_VOL="m46n1"
N1_HANDLE="$(open_volume "$DEPLOY_STORD_SOCK" "$N1_VOL" "m46n1.img" "8388608")" \
    || qual_die "aborting (Leg N1a) — could not open a volume on the deployed stord"
qual_pass "OpenVolume on the DEPLOYED stord accepted (${N1_VOL}, handle ${N1_HANDLE})"
N1_MID="$(trigger_migration "$DEPLOY_STORD_SOCK" "$N1_VOL" "$N1_HANDLE" "https://127.0.0.1:${DST_PORT}")" \
    || qual_die "aborting (Leg N1a)"
wait_for "deployed-stord migration task FAILED (no mTLS config)" "$RPC_TIMEOUT" \
    phase_is "$DEPLOY_STORD_SOCK" "$N1_MID" "FAILED" \
    || qual_error "deployed-stord migration task never failed (no mTLS config)"
N1_ERR="$(migration_field "$DEPLOY_STORD_SOCK" "$N1_MID" errorMessage)"
assert_contains "deployed-stord task error is the mTLS-required precondition" \
    "$N1_ERR" "mTLS is required for storage migration"
record_migration_error "leg-N1a deployed stord (no [migration])" "$DEPLOY_STORD_SOCK" "$N1_MID"
close_volume "$DEPLOY_STORD_SOCK" "$N1_VOL" "$N1_HANDLE" || true

# N1b: the startup-vs-trigger distinction (#393/#396/#395) — client or
# receiver fields configured while enabled=false are STARTUP errors (an
# operator who believes migration is off must not get a half-ignored
# identity, nor a silent inbound listener).
N1B_DIR="${M46_DIR}/n1b"; mkdir -p "$N1B_DIR"
cat > "${N1B_DIR}/client-disabled.toml" <<EOF
socket_path = "${N1B_DIR}/client.sock"
runtime_dir = "${N1B_DIR}"
log_level = "info"
metrics_bind = "127.0.0.1:0"

[migration]
enabled = false
client_cert_path = "${CERTS_DIR}/src-client.crt"
client_key_path = "${CERTS_DIR}/src-client.key"
ca_cert_path = "${CERTS_DIR}/ca.crt"
dest_server_name = "localhost"
EOF
expect_stord_startup_failure "N1b client fields with enabled=false" \
    "${N1B_DIR}/client-disabled.toml" "${N1B_DIR}/client-disabled.log" \
    "migration.enabled = false"

cat > "${N1B_DIR}/receiver-disabled.toml" <<EOF
socket_path = "${N1B_DIR}/receiver.sock"
runtime_dir = "${N1B_DIR}"
log_level = "info"
metrics_bind = "127.0.0.1:0"

[migration]
enabled = false
listen_addr = "127.0.0.1:${DST_PORT}"
server_cert_path = "${CERTS_DIR}/dst-server.crt"
server_key_path = "${CERTS_DIR}/dst-server.key"
client_ca_path = "${CERTS_DIR}/ca.crt"
EOF
expect_stord_startup_failure "N1b receiver fields with enabled=false" \
    "${N1B_DIR}/receiver-disabled.toml" "${N1B_DIR}/receiver-disabled.log" \
    "migration.enabled = false"

# assert_contains_any DESC HAYSTACK NEEDLE [NEEDLE...] — passes if any needle
# is present, reporting the one that matched. For server-side mTLS
# rejections the TLS 1.3 alert races the client's in-flight request, so the
# surfaced error is one of several transport-level forms (run 2:
# Cancelled/"operation was canceled"; run 3: Unknown/"transport error").
assert_contains_any() {
    local desc="$1" haystack="$2"
    shift 2
    local needle
    for needle in "$@"; do
        if printf '%s' "$haystack" | grep -qF -- "$needle"; then
            qual_pass "${desc} (matched: '${needle}')"
            return 0
        fi
    done
    qual_error "${desc} (none of '$*' present — got: ${haystack:0:200})"
    return 1
}

# ===========================================================================
# Legs N2/N3/N4/N7 — wrong-identity SOURCES: each starts (the material is
# structurally valid), triggers a migration at the LIVE DST, and must fail
# closed at the TLS handshake. run_negative_src NAME CA CERT KEY DEST_NAME
# ERROR_NEEDLE [LOG_NEEDLE]
#
# Two distinct client-side manifestations, both observed against the pinned
# candidate (see failed-migrations.jsonl):
#   - client-side rejection (SRC cannot validate DST's cert: N2 wrong CA,
#     N3 wrong server name): the tonic transport error surfaces as
#     "failed to connect to peer with mTLS: transport error" — the rustls
#     detail (unknown issuer vs name mismatch) is NOT surfaced (finding).
#   - server-side rejection (DST rejects SRC's client identity: N4 rogue
#     CA, N7 expired leaf): in TLS 1.3 the SRC's handshake completes before
#     DST processes its cert flight, so the alert races the client's
#     in-flight request and the surfaced task error is an OPAQUE
#     transport-level form — observed both as Cancelled/"operation was
#     canceled" and as Unknown/"transport error" (finding), while the DST
#     logs nothing at all. Asserted as a disjunction of the observed forms.
# ===========================================================================
run_negative_src() {
    local name="$1" ca="$2" cert="$3" key="$4" dest="$5" log_needle="$6"
    shift 6
    local dir sock log pid vol handle mid err bytes
    dir="${M46_DIR}/${name}"
    mkdir -p "$dir"
    sock="${dir}/api.sock"
    log="${dir}/stord.log"
    vol="m46${name}"
    write_src_config "${dir}/stord.toml" "$sock" "$dir" "$ca" "$cert" "$key" "$dest"
    pid="$(start_scenario_stord "${dir}/stord.toml" "$log")"
    wait_for "N-leg ${name}: stord UDS up" 20 test -S "$sock" \
        || { qual_error "N-leg ${name}: stord did not come up — log: $(tail -5 "$log" 2>/dev/null)"; return 1; }
    handle="$(open_volume "$sock" "$vol" "vol.img" "8388608")" \
        || { qual_error "N-leg ${name}: OpenVolume failed"; return 1; }
    mid="$(trigger_migration "$sock" "$vol" "$handle" "https://127.0.0.1:${DST_PORT}")" \
        || { qual_error "N-leg ${name}: trigger failed"; return 1; }
    wait_for "N-${name}: migration task FAILED at the TLS handshake" "$RPC_TIMEOUT" \
        phase_is "$sock" "$mid" "FAILED" \
        || qual_error "N-${name}: task never failed"
    err="$(migration_field "$sock" "$mid" errorMessage)"
    assert_contains_any "N-${name}: error is a transport-level mTLS failure" "$err" "$@"
    if [ -n "$log_needle" ]; then
        assert_file_contains "N-${name}: SRC log shows the leg's distinguishing input" \
            "$log" "$log_needle"
    fi
    # Fail-closed: nothing left the source and the receiver never
    # materialized a receiving volume for this identity-rejected source.
    bytes="$(migration_field "$sock" "$mid" bytesTransferred)"
    if [ "$bytes" = "0" ]; then
        qual_pass "N-${name}: zero bytes transferred (fail-closed)"
    else
        qual_error "N-${name}: bytes were transferred despite rejection (${bytes})"
    fi
    if [ ! -e "${DST_DIR}/${vol}.img" ]; then
        qual_pass "N-${name}: no receiving volume created on the destination"
    else
        qual_error "N-${name}: destination materialized a receiving volume (${DST_DIR}/${vol}.img)"
    fi
    record_migration_error "leg-${name}" "$sock" "$mid"
    stop_scenario_stord "$pid" "${name}"
    cp "$log" "${EVIDENCE_DIR}/leg-${name}-stord.log" 2>/dev/null || true
}

# --- Leg N2: wrong CA -------------------------------------------------------
qual_info "--- Leg N2: SRC trusts an unrelated CA → destination certificate cannot validate"
# Differential: identical to Leg P except ca2.crt instead of ca.crt — the
# failure is attributable to the trust anchor alone.
run_negative_src n2 "${CERTS_DIR}/ca2.crt" \
    "${CERTS_DIR}/src-client.crt" "${CERTS_DIR}/src-client.key" \
    "localhost" "" "failed to connect to peer with mTLS: transport error"

# --- Leg N3: wrong server name ----------------------------------------------
qual_info "--- Leg N3: dest_server_name does not match the destination certificate SAN"
run_negative_src n3 "${CERTS_DIR}/ca.crt" \
    "${CERTS_DIR}/src-client.crt" "${CERTS_DIR}/src-client.key" \
    "wrong.example" "wrong.example" "failed to connect to peer with mTLS: transport error"

# --- Leg N4: wrong destination identity (from the receiver side) ------------
qual_info "--- Leg N4: SRC presents a client identity from the unrelated CA → DST rejects the handshake"
run_negative_src n4 "${CERTS_DIR}/ca.crt" \
    "${CERTS_DIR}/rogue-client.crt" "${CERTS_DIR}/rogue-client.key" \
    "localhost" "" "operation was canceled" "transport error" "failed to connect to peer with mTLS"

# --- Leg N7: expired client certificate --------------------------------------
qual_info "--- Leg N7: SRC's client leaf is expired → handshake-time rejection"
run_negative_src n7 "${CERTS_DIR}/ca.crt" \
    "${CERTS_DIR}/expired-client.crt" "${CERTS_DIR}/expired-client.key" \
    "localhost" "" "operation was canceled" "transport error" "failed to connect to peer with mTLS"

# ===========================================================================
# Leg N5 — mismatched keypairs: STARTUP errors (both halves)
# ===========================================================================
qual_info "--- Leg N5: certificate/key mismatch — stord refuses to start (client half + server half)"

N5_DIR="${M46_DIR}/n5"; mkdir -p "$N5_DIR"
# A fresh, valid-but-unrelated key for both mismatch legs.
openssl genrsa -out "${CERTS_DIR}/wrong.key" 2048 2>/dev/null
chmod 600 "${CERTS_DIR}/wrong.key"

write_src_config "${N5_DIR}/client-mismatch.toml" "${N5_DIR}/client.sock" "$N5_DIR" \
    "${CERTS_DIR}/ca.crt" "${CERTS_DIR}/src-client.crt" "${CERTS_DIR}/wrong.key" \
    "localhost"
expect_stord_startup_failure "N5 client-half cert/key mismatch" \
    "${N5_DIR}/client-mismatch.toml" "${N5_DIR}/client-mismatch.log" \
    "does not match"

write_dst_config "${N5_DIR}/server-mismatch.toml" "${N5_DIR}/server.sock" "$N5_DIR" \
    "$DST_PORT" "${CERTS_DIR}/dst-server.crt" "${CERTS_DIR}/wrong.key" "${CERTS_DIR}/ca.crt"
expect_stord_startup_failure "N5 server-half cert/key mismatch" \
    "${N5_DIR}/server-mismatch.toml" "${N5_DIR}/server-mismatch.log" \
    "does not match"

# ===========================================================================
# Leg N6 — malformed / empty identity material: STARTUP errors
# ===========================================================================
qual_info "--- Leg N6: malformed cert/key/CA (and an empty CA bundle) — stord refuses to start"

N6_DIR="${M46_DIR}/n6"; mkdir -p "$N6_DIR"
printf 'this is not a certificate\n' > "${N6_DIR}/garbage.pem"
printf '' > "${N6_DIR}/empty.pem"

# Malformed client certificate.
write_src_config "${N6_DIR}/bad-cert.toml" "${N6_DIR}/c1.sock" "$N6_DIR" \
    "${CERTS_DIR}/ca.crt" "${N6_DIR}/garbage.pem" "${CERTS_DIR}/src-client.key" \
    "localhost"
expect_stord_startup_failure "N6 malformed client certificate" \
    "${N6_DIR}/bad-cert.toml" "${N6_DIR}/bad-cert.log" "invalid certificate"

# Malformed client key.
write_src_config "${N6_DIR}/bad-key.toml" "${N6_DIR}/c2.sock" "$N6_DIR" \
    "${CERTS_DIR}/ca.crt" "${CERTS_DIR}/src-client.crt" "${N6_DIR}/garbage.pem" \
    "localhost"
expect_stord_startup_failure "N6 malformed client key" \
    "${N6_DIR}/bad-key.toml" "${N6_DIR}/bad-key.log" "invalid private key"

# Malformed client CA bundle.
write_src_config "${N6_DIR}/bad-ca.toml" "${N6_DIR}/c3.sock" "$N6_DIR" \
    "${N6_DIR}/garbage.pem" "${CERTS_DIR}/src-client.crt" "${CERTS_DIR}/src-client.key" \
    "localhost"
expect_stord_startup_failure "N6 malformed client CA bundle" \
    "${N6_DIR}/bad-ca.toml" "${N6_DIR}/bad-ca.log" "invalid CA bundle"

# Empty client CA bundle on the SERVER half (parseable file, no certs).
write_dst_config "${N6_DIR}/empty-ca.toml" "${N6_DIR}/s1.sock" "$N6_DIR" \
    "$DST_PORT" "${CERTS_DIR}/dst-server.crt" "${CERTS_DIR}/dst-server.key" "${N6_DIR}/empty.pem"
expect_stord_startup_failure "N6 empty client CA bundle (server half)" \
    "${N6_DIR}/empty-ca.toml" "${N6_DIR}/empty-ca.log" "no certificates"

# ===========================================================================
# Leg N8 — plaintext endpoint / downgrade attempt
# ===========================================================================
qual_info "--- Leg N8: http:// dest_endpoint is force-upgraded to https and fails against a non-TLS listener"

# Throwaway plaintext echo listener (a TLS ClientHello gets echoed back —
# never a valid TLS server). Logs every accepted connection so the leg can
# prove the sender actually dialed the plaintext port.
N8_LOG="${M46_DIR}/n8-plain-listener.log"
python3 - "$PLAIN_PORT" > "$N8_LOG" 2>&1 <<'PYEOF' &
import socket, sys
port = int(sys.argv[1])
srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", port))
srv.listen(8)
print(f"plain listener on 127.0.0.1:{port}", flush=True)
while True:
    conn, _ = srv.accept()
    print("accepted connection", flush=True)
    try:
        conn.settimeout(5)
        while True:
            data = conn.recv(4096)
            if not data:
                break
            conn.sendall(data)  # echo — never a valid TLS response
    except OSError:
        pass
    finally:
        conn.close()
PYEOF
N8_LISTENER_PID=$!
disown "$N8_LISTENER_PID"
M46_PIDS+=("$N8_LISTENER_PID")
wait_for "N8 plaintext listener up" 10 tcp_port_open "$PLAIN_PORT" \
    || qual_die "N8 plaintext listener did not come up"

# Trigger from the GOOD source stord (the endpoint is per-trigger): an
# http:// destination must be upgraded by the sender (sender.rs
# force-upgrade) — there is no plaintext migration path.
N8_VOL="m46n8"
N8_HANDLE="$(open_volume "$SRC_SOCK" "$N8_VOL" "n8.img" "8388608")" \
    || qual_die "aborting (Leg N8)"
N8_MID="$(trigger_migration "$SRC_SOCK" "$N8_VOL" "$N8_HANDLE" "http://127.0.0.1:${PLAIN_PORT}")" \
    || qual_die "aborting (Leg N8)"
wait_for "N8: migration task FAILED against the non-TLS endpoint" "$RPC_TIMEOUT" \
    phase_is "$SRC_SOCK" "$N8_MID" "FAILED" \
    || qual_error "N8: task never failed against the plaintext endpoint"
N8_ERR="$(migration_field "$SRC_SOCK" "$N8_MID" errorMessage)"
assert_contains "N8: error indicates the mTLS connect failure (no plaintext fallback)" \
    "$N8_ERR" "failed to connect to peer with mTLS"
# The upgrade proof, from the sender's own log: the http:// endpoint was
# rewritten to https:// before dialing.
# tracing writes ANSI escapes between the message text and the endpoint=
# field, so the needle is the upgraded https URL alone — it appears only in
# the sender's "connecting to migration peer with mTLS" line (the trigger
# line logs the http:// form the RPC was given).
assert_file_contains "N8: sender force-upgraded http:// → https:// before dialing (SRC log)" \
    "$SRC_LOG" "https://127.0.0.1:${PLAIN_PORT}"
assert_file_contains "N8: the plaintext listener actually received the (upgraded) dial" \
    "$N8_LOG" "accepted connection"
record_migration_error "leg-N8 plaintext endpoint" "$SRC_SOCK" "$N8_MID"
kill "$N8_LISTENER_PID" 2>/dev/null || true

# ===========================================================================
# Leg N9 — interrupted transfer + deterministic recovery
# ===========================================================================
qual_info "--- Leg N9: SIGKILL DST during BULK_COPY → task Failed → restart → create_new refusal → remove partial → retry → COMPLETED"

VOL9_ID="m46vol9"
HANDLE9="$(open_volume "$SRC_SOCK" "$VOL9_ID" "vol9.img" "$VOL_SIZE_BYTES" "$SEED_FILE")" \
    || qual_die "aborting (Leg N9)"
qual_pass "OpenVolume on SRC: ${VOL9_ID} (handle ${HANDLE9})"
MID9="$(trigger_migration "$SRC_SOCK" "$VOL9_ID" "$HANDLE9" "https://127.0.0.1:${DST_PORT}")" \
    || qual_die "aborting (Leg N9)"

# Kill the destination mid-bulk-copy. Tight poll: break the moment BULK_COPY
# is visible (the 4 GiB transfer leaves a multi-second window).
N9_PHASE=""
N9_DEADLINE=$((SECONDS + RPC_TIMEOUT))
while :; do
    N9_PHASE="$(migration_field "$SRC_SOCK" "$MID9" phase)"
    [ "$N9_PHASE" = "BULK_COPY" ] && break
    case "$N9_PHASE" in
        FAILED | PAUSED_FINAL_SYNC | COMPLETED)
            qual_error "N9: missed the BULK_COPY window (phase ${N9_PHASE} before the kill)"
            break
            ;;
    esac
    [ "$SECONDS" -ge "$N9_DEADLINE" ] && { qual_error "N9: no phase observed before timeout"; break; }
    sleep 0.2
done
if [ "$N9_PHASE" = "BULK_COPY" ]; then
    kill -9 "$DST_PID" 2>/dev/null || true
    # A SIGKILLed stord leaves its UDS socket file behind — `test -S` would
    # then pass vacuously for the restart below. Remove it so the restart
    # wait actually gates on the NEW process's socket.
    rm -f "$DST_SOCK"
    qual_pass "DST stord SIGKILLed during BULK_COPY (transfer interrupted mid-copy)"
    wait_for "N9: interrupted migration task FAILED at the source" "$RPC_TIMEOUT" \
        phase_is "$SRC_SOCK" "$MID9" "FAILED" \
        || qual_error "N9: interrupted task never failed"
    N9_ERR="$(migration_field "$SRC_SOCK" "$MID9" errorMessage)"
    [ -n "$N9_ERR" ] \
        && qual_pass "interrupted task carries a non-empty error (${N9_ERR:0:80}…)" \
        || qual_error "interrupted task FAILED without an error message"
    record_migration_error "leg-N9 interrupted (DST SIGKILL during BULK_COPY)" "$SRC_SOCK" "$MID9"
else
    qual_error "N9: skipping the kill — BULK_COPY was never observed (phase: ${N9_PHASE})"
fi

# Recovery: restart DST with the same identity/config. Migration task state
# is in-memory (no resume): the operator re-triggers, getting a NEW
# migration_id for the same volume+handle. The restarted instance logs to a
# FRESH file so its listener assert is unambiguous (the first instance's log
# is preserved separately as evidence of the interrupted run).
N9_DST_LOG="${DST_DIR}/stord-restart.log"
DST_PID="$(start_scenario_stord "${DST_DIR}/stord.toml" "$N9_DST_LOG")"
wait_for "N9: DST restarted (UDS up)" 20 test -S "$DST_SOCK" \
    || qual_die "N9: DST did not restart — log: $(tail -20 "$N9_DST_LOG" 2>/dev/null)"
assert_file_contains "N9: restarted DST re-bound the receiver listener" \
    "$N9_DST_LOG" "storage migration receiver listening on 127.0.0.1:${DST_PORT}"

# First retry hits the create_new refusal: the partial receiving volume
# exists and the receiver REFUSES to truncate it (documented operator step:
# remove the partial volume; there is no in-place resume).
MID9B="$(trigger_migration "$SRC_SOCK" "$VOL9_ID" "$HANDLE9" "https://127.0.0.1:${DST_PORT}")" \
    || qual_die "aborting (Leg N9 recovery)"
wait_for "N9: retry FAILED — receiver refuses to truncate the partial receiving volume" \
    "$RPC_TIMEOUT" phase_is "$SRC_SOCK" "$MID9B" "FAILED" \
    || qual_error "N9: retry against the partial volume never failed"
N9B_ERR="$(migration_field "$SRC_SOCK" "$MID9B" errorMessage)"
assert_contains "N9: refusal names the existing receiving volume (create_new semantics)" \
    "$N9B_ERR" "refusing to truncate"
record_migration_error "leg-N9 retry against the partial receiving volume" "$SRC_SOCK" "$MID9B"
assert_file_exists "N9: partial receiving volume still on disk (not clobbered)" "${DST_DIR}/${VOL9_ID}.img"

# Deterministic recovery: remove the partial volume, re-trigger (a THIRD
# migration_id), run the full protocol to COMPLETED, verify the digest.
rm -f "${DST_DIR}/${VOL9_ID}.img"
qual_pass "partial receiving volume removed (the documented operator recovery step)"
MID9C="$(trigger_migration "$SRC_SOCK" "$VOL9_ID" "$HANDLE9" "https://127.0.0.1:${DST_PORT}")" \
    || qual_die "aborting (Leg N9 final retry)"
wait_for "N9: post-recovery migration reached PAUSED_FINAL_SYNC" "$RPC_TIMEOUT" \
    phase_is "$SRC_SOCK" "$MID9C" "PAUSED_FINAL_SYNC" \
    || { record_migration_error "leg-N9c no-pause" "$SRC_SOCK" "$MID9C"; qual_error "N9: recovery migration never paused"; }
stord_rpc "$SRC_SOCK" ResumeDiskMigration \
    "{\"migrationId\":\"${MID9C}\",\"vmPaused\":true}" >/dev/null 2>&1 \
    || qual_error "N9: ResumeDiskMigration RPC failed"
wait_for "N9: post-recovery migration COMPLETED" "$RPC_TIMEOUT" \
    phase_is "$SRC_SOCK" "$MID9C" "COMPLETED" \
    || { record_migration_error "leg-N9c not-completed" "$SRC_SOCK" "$MID9C"; qual_error "N9: recovery migration never completed"; }
SRC9_SHA="$(sha256_of "${SRC_DIR}/vol9.img")"
DST9_SHA="$(sha256_of "${DST_DIR}/${VOL9_ID}.img")"
[ "$SRC9_SHA" = "$DST9_SHA" ] \
    && qual_pass "N9: post-recovery digest match (${SRC9_SHA:0:16}…)" \
    || qual_error "N9: post-recovery digest mismatch (src=${SRC9_SHA} dst=${DST9_SHA})"
if cmp -s "${SRC_DIR}/vol9.img" "${DST_DIR}/${VOL9_ID}.img"; then
    qual_pass "N9: byte-compare identical after recovery"
else
    qual_error "N9: byte-compare failed after recovery"
fi
{
    echo "### leg-N9 digest record ($(date -u +%FT%TZ))"
    echo "src ${SRC_DIR}/vol9.img ${SRC9_SHA}"
    echo "dst ${DST_DIR}/${VOL9_ID}.img ${DST9_SHA}"
} >> "${EVIDENCE_DIR}/digests.txt"
cp "$SRC_LOG" "${EVIDENCE_DIR}/leg-n9-src-stord.log" 2>/dev/null || true
cp "$DST_LOG" "${EVIDENCE_DIR}/leg-n9-dst-stord.log" 2>/dev/null || true
cp "$N9_DST_LOG" "${EVIDENCE_DIR}/leg-n9-dst-restart-stord.log" 2>/dev/null || true

# ===========================================================================
# Close-out — sessions closed, daemons stopped, zero residue
# ===========================================================================
qual_info "--- Close-out: close sessions, stop scenario stords, assert zero residue"

close_volume "$SRC_SOCK" "$VOL1_ID" "$HANDLE1" || true
close_volume "$SRC_SOCK" "$VOL9_ID" "$HANDLE9" || true
close_volume "$SRC_SOCK" "$N8_VOL" "$N8_HANDLE" || true

stop_scenario_stord "$SRC_PID" "SRC"
stop_scenario_stord "$DST_PID" "DST"

# No stord sessions remain on the scenario SRC (its DB is now at rest).
SRC_SESSIONS_LEFT="$(sqlite_query "${SRC_DIR}/stord.db" \
    "SELECT COUNT(*) FROM sessions" 2>/dev/null | head -1)"
if [ "${SRC_SESSIONS_LEFT}" = "0" ]; then
    qual_pass "no stord sessions remain on the scenario source"
elif [ -z "${SRC_SESSIONS_LEFT}" ]; then
    qual_error "could not read the scenario source sessions DB (${SRC_DIR}/stord.db) — residue state unknown"
else
    qual_error "stord sessions remain on the scenario source (${SRC_SESSIONS_LEFT})"
fi

# Forbidden outcomes: no scenario-owned processes, both ports freed.
if [ -z "$(pgrep -f "(^|/)chv-stord( |$).*${M46_DIR}" 2>/dev/null)" ]; then
    qual_pass "no scenario chv-stord processes remain"
else
    qual_error "scenario chv-stord processes remain: $(pgrep -af "(^|/)chv-stord( |$).*${M46_DIR}" | head -3)"
fi
tcp_port_free "$DST_PORT" \
    && qual_pass "destination port ${DST_PORT} freed" \
    || qual_error "destination port ${DST_PORT} still in use"
tcp_port_free "$PLAIN_PORT" \
    && qual_pass "plaintext-listener port ${PLAIN_PORT} freed" \
    || qual_error "plaintext-listener port ${PLAIN_PORT} still in use"
assert_process_alive "deployed stack stord untouched by the scenario" "$QUAL_STORD_PID"

# File residue: on success everything scenario-owned is removed (deploy
# removes TEST_DIR itself; this makes the scenario's own contribution
# explicit). On failure the dir is deliberately KEPT for post-mortem (the
# deploy preserves it as /tmp/chv-qual-failed-*).
if [ "$QUAL_ERRORS" -eq 0 ]; then
    rm -rf "$M46_DIR"
    [ ! -e "$M46_DIR" ] \
        && qual_pass "scenario resource dir removed (${M46_DIR})" \
        || qual_error "scenario resource dir could not be removed: ${M46_DIR}"
else
    qual_info "errors recorded — ${M46_DIR} kept for post-mortem (deploy preserves TEST_DIR on failure)"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
if [ "$QUAL_ERRORS" -gt 0 ]; then
    qual_error "M4.6 scenario finished with ${QUAL_ERRORS} error(s)"
    qual_info "test dir: ${QUAL_TEST_DIR} (deploy keeps it on failure)"
    exit 1
fi
qual_pass "M4.6 two-stord mTLS migration scenario complete: positive leg + 9 negative/failure legs, fail-closed throughout"
exit 0
