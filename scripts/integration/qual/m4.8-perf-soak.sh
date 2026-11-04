#!/usr/bin/env bash
# m4.8-perf-soak.sh — prompt-04 M4.8: performance & soak baseline on real
# (nested) KVM, via the candidate deployment from deploy.sh.
#
# Run via deploy.sh --exec (root):
#   sudo env "PATH=$PATH" GUEST_IMAGE=noble-qual-patched.img ./deploy.sh \
#       --exec ./m4.8-perf-soak.sh
#
# THE M4.8 MANDATE (plan §M4.8): "Measure, don't guess, all labeled with
# the exact hardware and 'baseline measurement, not a scale claim'." This
# is a MEASUREMENT milestone, not a feature milestone: every number the
# scenario produces lands in /var/lib/chv/qual/m4.8-artifacts/ and in the
# evidence doc labeled
#   "baseline measurement on the qualification host (16 vCPU, 31 GiB RAM,
#    nested KVM), not a scale claim".
#
# THE ASSERTION vs RECORD DISTINCTION (the milestone's core rule):
#   ASSERTIONS (qual_pass/qual_error — a failure fails the milestone):
#     - zero residue after EVERY soak cycle: zero cloud-hypervisor
#       processes, tap count back to baseline, the cycle's stord session
#       closed, both op journals terminal, exactly one CreateVm row;
#     - no leaked daemons/processes at the final sweep;
#     - NO fd/socket growth AT REST across the soak (two at-rest windows
#       with net-zero VMs bracket the run — see thresholds below);
#     - no unbounded log cardinality (per-op unique message shapes);
#     - no control-plane DB growth with ZERO ops (at-rest windows);
#     - every concurrent-workload request answers 200 (no 5xx);
#     - the migration leg completes with digest + byte equality (the
#       M4.6-qualified path — its throughput is a RECORD, its correctness
#       is an ASSERTION).
#   RECORDS (labeled numbers, never pass/fail on magnitude): idle CPU/RSS
#     per daemon, per-op lifecycle latency distributions, read/write
#     latency under the bounded concurrency, migration bytes/sec, DB
#     sizes + per-op bytes, fd/socket counts mid-soak, VMM RSS, log
#     bytes/min. A measurement being slow or large is NOT a failure
#     unless it indicates an unbounded growth/leak (those are errors).
#
# PRE-REGISTERED LEAK THRESHOLDS (chosen before the live run so the
# result is interpretable; each is commented at its definition site):
#   - FD_GROWTH_MAX        = 24   fds gained by a daemon between the two
#                                  at-rest windows (same net-zero-VM
#                                  state). >24 across ~30 ops ⇒ a per-op
#                                  fd leak (≥0.8 fd/op); ≤24 covers pool
#                                  warm-up and transient accept sockets.
#   - SOCK_GROWTH_MAX      = 12   socket fds, same comparison.
#   - IDLE_DB_GROWTH_MAX   = 524288 bytes (512 KiB) of controlplane.db +
#                                  -wal net growth across a 60 s at-rest
#                                  window with zero API calls. WAL phase
#                                  noise is page-bounded; row
#                                  accumulation is not.
#   - NEW_LOG_SHAPES_MAX   = 8    NEW normalized message shapes in the
#                                  second half of the soak vs the first
#                                  (per daemon). Steady-state repetition
#                                  adds zero shapes; per-op unique
#                                  strings add ~1/op.
#   - LOG_SHAPES_TOTAL_MAX = 2000 distinct normalized shapes per daemon
#                                  across the whole run (bounded set).
#   - METRIC_SERIES_MAX    = 200  Prometheus series at the agent's
#                                  /metrics (the exporter is a fixed
#                                  node-scoped gauge set).
#   - RSS growth at rest   = record-only (allocator retention makes
#                                  gating unsound); warn > 100 MiB.
#
# N CHOICES (sequential, resource-bounded for a 16 vCPU / 31 GiB host
# shared by all daemons + one guest at a time):
#   N_SOAK         = 6   create→start→boot→stop→delete cycles (typical
#                        ~2.5-4 min/cycle: create seed-conversion
#                        ~10-25 s, boot to logind ~60-150 s nested,
#                        graceful stop ~35 s, delete+settle ~20 s);
#   IDLE_SAMPLES   = 30 × 2 s = 60 s per at-rest window (two windows);
#   READERS        = 4   parallel read loops (vm list + vm get), 90 s;
#   WRITERS        = 2   sequential create→delete cycles during the
#                        same 90 s window;
#   SOAK_READS     = 10  list+get pairs per soak cycle;
#   migration      = 4 GiB seeded volume (the M4.6/M4.7 size —
#                        comparable evidence), BULK_COPY sampled at
#                        0.5 s.
# PROJECTED RUNTIME: scenario ~30-40 min (P1 1.5 + soak 15-24 + P3 1.5 +
# P4 4 + P5 5-7 + P6 1) + deploy ~3-5 min ⇒ ~35-45 min typical, inside
# the 45-60 min mandate with margin for slow boots (worst case ~55 min).
#
# Legs:
#   P1  Idle baseline at rest (zero VMs): CPU% + RSS sampled every 2 s
#       for 60 s per daemon (control-plane, agent, stord, nwd) via
#       /proc/<pid>/stat (utime+stime ticks) and /proc/<pid>/status
#       (VmRSS); DB (db+wal+page_count) snapshot; idle log rate; fd/
#       socket counts; /metrics series count. DB idle-growth ASSERTION.
#   P2  Soak + lifecycle latency (the run's core): N_SOAK sequential
#       cycles of create → start → boot to logind → repeated list/get
#       reads → graceful stop → delete. Every op is timed
#       (submit → API accept RTT, and accept → convergence) and lands
#       in lifecycle-latency.csv; distributions (count/min/median/p90/
#       max) are computed at the end. Convergence signals (the
#       M4.3-M4.7-proven host truths, never the submit-level BFF
#       answer): create = boot-volume backing at full capacity; start =
#       power_state Running AND console.log receiving bytes; stop = CH
#       process gone; delete = CH gone + tap removed + stord session
#       closed. Boot-to-banner and boot-to-logind are recorded
#       separately. Zero-residue ASSERTIONS after every cycle. Mid- and
#       end-of-soak fd/socket + metrics samples; per-cycle DB sizes.
#   P3  Idle at rest AFTER the soak: repeat of P1. fd/socket growth
#       vs P1 is the leak ASSERTION; RSS/CPU records; DB idle-growth
#       ASSERTION; metrics cardinality verdict.
#   P4  Bounded concurrent API workload (EXPLICITLY bounded-by-this-
#       host, NOT a scale claim): 4 parallel reader loops (curl → BFF
#       /v1/vms list + /v1/vms/get, per-request http_code + time_total)
#       + 2 sequential writer loops (chvctl create → converge → delete)
#       for 90 s against one anchored created-not-started VM. All-200
#       ASSERTION; latency distributions + writer cycle counts RECORDS.
#   P5  Migration throughput on the qualified path: the M4.6 two-
#       standalone-stord mTLS topology (NOT agent-supervised, #385),
#       4 GiB patterned seed, TriggerDiskMigration → BULK_COPY sampled
#       at 0.5 s (bytesTransferred resets past BULK_COPY — the M4.6
#       truth — so throughput = totalBytes / BULK_COPY duration) →
#       pause handshake → resume → COMPLETED (ASSERTED) + digest +
#       byte-compare (ASSERTED). MiB/s RECORD. src/dst stord CPU+RSS
#       during the transfer RECORD.
#   P6  Final sweep + summaries: zero CH processes, zero scenario taps,
#       physical backings ↔ CP rows, live stord sessions all closed,
#       every journaled op terminal, one CreateVm row per created VM,
#       log-cardinality verdict, DB growth summary (total + per-op
#       bytes), artifacts manifest.
#
# FORBIDDEN OUTCOMES (asserted; a "successful" run leaving any behind
# FAILS the milestone): residue after a soak cycle (CH process, tap,
# stord session, non-terminal op, duplicated CreateVm row); fd/socket
# growth at rest across the soak; DB growth at rest with zero ops;
# unbounded per-op log shapes; any 5xx from the BFF under the bounded
# workload; a failed/digest-mismatched migration.
#
# Non-claims (recorded in the evidence doc): everything scale-shaped —
# concurrency/density/throughput beyond the bounded N above; multi-node;
# the CP-orchestrated migration path (M4.6 declaration); M2.5 delete
# retention (BFF rows/backings of deleted VMs persist by design —
# disclosed once, physical-side cleanup is what is asserted); host
# reboot (M4.3's recorded not-provable subset); #394 concurrent-write
# migration (quiescent seeded volumes only).
#
# EXPECTED PASS COUNTS (deterministic qual_pass sites on the happy
# path; measurement RECORDS are not passes; the deploy phase adds ~20
# of its own before the scenario starts):
#   prelude (tools + identity guard + baselines) 5
#   P1 idle baseline                         4
#   P2 soak (17 per cycle × 6 + 2 samples)  104
#   P3 idle after + leak verdicts            5
#   P4 bounded concurrent workload          14
#   P5 migration throughput (M4.6 shape)    24
#   P6 final sweep + summaries              12
#   TOTAL                                  168

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
AGENT_PID="$QUAL_AGENT_PID"
VMS_DIR="${QUAL_AGENT_DIR}/vms"
CORE_DB="${QUAL_AGENT_DIR}/core.db"
STORD_DIR="${QUAL_TEST_DIR}/stord"
DEFAULT_NET="default"
GUEST_IMAGE_PATH="${QUAL_GUEST_IMAGE_PATH:-/var/lib/chv/qual/images/noble-qual-patched.img}"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"

# --- N choices (see header) ------------------------------------------------
N_SOAK=6
IDLE_SAMPLES=30
IDLE_INTERVAL=2          # 30 × 2 s = 60 s at-rest window
READERS=4
WRITERS=2
WORKLOAD_WINDOW=90       # seconds
SOAK_READS=10            # list+get pairs per soak cycle

BOOT_TIMEOUT=420         # kernel banner after vm start (nested-virt margin)
LOGIND_TIMEOUT=180       # logind lines after the banner
STOP_TIMEOUT=240         # graceful stop (60 s window + snapd's ~32 s + margin)
OPS_SETTLE=300           # op terminal window (CP reaper: 120 s for CreateVm)
TAP_SETTLE=30            # async tap removal on VM delete (m4.4 lesson)
MIG_RPC_TIMEOUT=180      # migration phase polling budget
SAMPLE_CADENCE=0.5       # BULK_COPY bytes sampling

# --- pre-registered leak thresholds (see header rationale) -----------------
FD_GROWTH_MAX=24
SOCK_GROWTH_MAX=12
IDLE_DB_GROWTH_MAX=$((512 * 1024))
NEW_LOG_SHAPES_MAX=8
LOG_SHAPES_TOTAL_MAX=2000
METRIC_SERIES_MAX=200
RSS_GROWTH_WARN_MB=100

# Migration topology (M4.6 shape): two standalone stords, deliberately
# NOT agent-supervised (#385: a supervisor respawn generates a minimal
# config that drops [migration]).
M48_DIR="${QUAL_TEST_DIR}/m48"
CERTS_DIR="${M48_DIR}/certs"
GRPCURL_DIR="${M48_DIR}/grpcurl"
DST_PORT="51063"        # DST mTLS receiver listener (loopback only)
VOL_SIZE_BYTES=$((4 * 1024 * 1024 * 1024))   # 4 GiB = 1024 x 4 MiB chunks
SEED_FILE="${M48_DIR}/m48-seed.img"

# grpcurl pin (supply-chain discipline — same as M4.6/M4.7).
GRPCURL_VERSION="1.9.3"
GRPCURL_TARBALL="grpcurl_${GRPCURL_VERSION}_linux_x86_64.tar.gz"
GRPCURL_CHECKSUMS="grpcurl_${GRPCURL_VERSION}_checksums.txt"
GRPCURL_CHECKSUMS_ASSET="${SCRIPT_DIR}/assets/${GRPCURL_CHECKSUMS}"
GRPCURL_BASE="https://github.com/fullstorydev/grpcurl/releases/download/v${GRPCURL_VERSION}"

M48_PIDS=()             # every scenario-owned process (standalone stords)

# Persistent evidence artifacts (deploy.sh removes TEST_DIR on success).
EVIDENCE_DIR="${CHV_QUAL_ROOT:-/var/lib/chv/qual}/m4.8-artifacts"
mkdir -p "$EVIDENCE_DIR"

LIFECYCLE_CSV="${EVIDENCE_DIR}/lifecycle-latency.csv"
READS_CSV="${EVIDENCE_DIR}/read-latency.csv"
CLK_TCK="$(getconf CLK_TCK 2>/dev/null || echo 100)"

# --- scenario-owned expectation bookkeeping (m4.7 conventions) -------------
CH_EXPECTED=0               # global cloud-hypervisor process count
TAPS_BASELINE=0             # host tap- count before the first VM
TAPS_EXPECTED=0             # taps provisioned at CREATE, removed async
KNOWN_CREATED=""            # VM ids whose create converged (row+backing
                            # must both exist while not deleted)
DELETED_VMS=""              # VM ids deleted (M2.5 retention class)

mark_created() { KNOWN_CREATED="${KNOWN_CREATED}${1} "; }
mark_deleted() { DELETED_VMS="${DELETED_VMS}${1} "; }   # M2.5 retention ledger
                                                            # (rows/backings persist)

DAEMONS="controlplane agent stord nwd"

# ---------------------------------------------------------------------------
# Measurement helpers (RECORDS — none of these gate on magnitude)
# ---------------------------------------------------------------------------

# now_us — microsecond wall clock (bash-5 EPOCHREALTIME; verified in the
# prelude). Used for every latency figure in the artifacts.
now_us() {
    if [ -n "${EPOCHREALTIME:-}" ]; then
        echo "${EPOCHREALTIME/./}"
    else
        date +%s%6N
    fi
}

# latency_s T0_US T1_US — elapsed seconds, millisecond precision.
latency_s() {
    awk -v a="$1" -v b="$2" 'BEGIN { printf "%.3f", (b - a) / 1000000 }'
}

# record FILE TEXT… — append a timestamped measurement line to an
# artifacts file and echo it into the run log (the evidence doc quotes
# these files verbatim).
record() {
    local f="$EVIDENCE_DIR/$1"; shift
    printf '%s %s\n' "$(date -u +%FT%TZ)" "$*" >> "$f"
    qual_info "[RECORD] $*"
}

# dist_stats — count/min/median/p90/max/sum of one float per stdin line.
# Distributions, never just means (the M4.8 mandate). The values are
# read in bash FIRST and passed as argv: a `python3 -` heredoc consumes
# stdin for the program text, so piping straight into it would read
# nothing (found by local helper unit test).
dist_stats() {
    local vals
    vals="$(cat)"
    python3 - "$vals" <<'PYEOF'
import math
import sys
vals = sorted(float(x) for x in sys.argv[1].split() if x)
n = len(vals)
if n == 0:
    print("n=0")
    raise SystemExit(0)
med = vals[n // 2] if n % 2 else (vals[n // 2 - 1] + vals[n // 2]) / 2.0
p90 = vals[max(0, math.ceil(0.9 * n) - 1)]
print(f"n={n} min={vals[0]:.3f} median={med:.3f} p90={p90:.3f} "
      f"max={vals[-1]:.3f} sum={sum(vals):.3f}")
PYEOF
}

# csv_col_dist CSV COLIDX [FILTER_COL FILTER_VAL] — dist_stats of one
# comma-separated column, optionally row-filtered.
csv_col_dist() {
    local csv="$1" col="$2" fcol="${3:-}" fval="${4:-}"
    if [ -n "$fcol" ]; then
        awk -F, -v c="$col" -v fc="$fcol" -v fv="$fval" \
            'NR > 1 && $fc == fv { print $c }' "$csv" 2>/dev/null
    else
        awk -F, -v c="$col" 'NR > 1 { print $c }' "$csv" 2>/dev/null
    fi
}

# proc_sample_all NAME:PID… — one python spawn samples every daemon:
# "name cpu_ticks rss_kb" lines (stat utime+stime after the comm field;
# VmRSS from status). One spawn per sample tick keeps the sampling
# itself from perturbing the idle window.
proc_sample_all() {
    python3 - "$@" <<'PYEOF'
import sys
for arg in sys.argv[1:]:
    name, _, pid = arg.rpartition(":")
    try:
        with open(f"/proc/{pid}/stat") as f:
            data = f.read()
        rest = data[data.rfind(")") + 2:].split()
        ticks = int(rest[11]) + int(rest[12])
        rss = 0
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    rss = int(line.split()[1])
                    break
        print(f"{name} {ticks} {rss}")
    except Exception:
        pass
PYEOF
}

proc_fd_count() { ls "/proc/$1/fd" 2>/dev/null | wc -l | tr -d ' '; }
proc_sock_count() {
    find "/proc/$1/fd" -maxdepth 1 -lname 'socket:*' 2>/dev/null | wc -l | tr -d ' '
}

# --- live daemon pid resolution (m4.7 patterns; no daemon is killed in
# this scenario, but live resolution keeps the checks honest if a
# supervisor respawn ever happens) -------------------------------------------
STORD_MATCH='(^|/)chv-stord( |$).*'"${QUAL_TEST_DIR}"'/(stord\.toml$|agent/chv-stord)'
NWD_MATCH='(^|/)chv-nwd( |$).*'"${QUAL_TEST_DIR}"'/(nwd\.toml$|agent/chv-nwd)'
stord_pid() { pgrep -f "$STORD_MATCH" 2>/dev/null | head -1; }
nwd_pid() { pgrep -f "$NWD_MATCH" 2>/dev/null | head -1; }
daemon_pid() {
    case "$1" in
        controlplane) echo "$CP_PID" ;;
        agent) echo "$AGENT_PID" ;;
        stord) stord_pid ;;
        nwd) nwd_pid ;;
    esac
}
pids_current() {
    local stord_now nwd_now
    stord_now="$(stord_pid)"
    nwd_now="$(nwd_pid)"
    cat > "${QUAL_TEST_DIR}/pids.current" <<EOF
CP_PID=${CP_PID}
STORD_PID=${stord_now:-${QUAL_STORD_PID:-}}
NWD_PID=${nwd_now:-${QUAL_NWD_PID:-}}
AGENT_PID=${AGENT_PID}
EOF
}

# stord sessions of the LIVE deployed stord (#376: DB resolved from the
# daemon's argv per call).
stord_runtime_dir() {
    local pid cfg
    pid="$(stord_pid)"
    if [ -n "$pid" ] && [ -r "/proc/${pid}/cmdline" ]; then
        cfg="$(tr '\0' '\n' < "/proc/${pid}/cmdline" | tail -1)"
        if [ -f "$cfg" ] && grep -q '^runtime_dir' "$cfg" 2>/dev/null; then
            awk -F'"' '/^runtime_dir/ { print $2; exit }' "$cfg"
            return 0
        fi
    fi
    echo "$STORD_DIR"
}
stord_db() { echo "$(stord_runtime_dir)/stord.db"; }
stord_sessions() {
    sqlite_query "$(stord_db)" \
        "SELECT COUNT(*) FROM sessions WHERE volume_id='$1'" 2>/dev/null | head -1
}

# --- DB growth (controlplane.db + core.db; db+wal net bytes + page_count;
# reads via lib.sh's read-only URI — the M4.1 WAL rule) ----------------------
db_file_size() { stat -c %s "$1" 2>/dev/null || echo 0; }
db_total_bytes() { echo $(( $(db_file_size "$1") + $(db_file_size "$1-wal") )); }
record_db_sizes() {
    local phase="$1" cppages corepages
    cppages="$(sqlite_query "$QUAL_DB" 'PRAGMA page_count' 2>/dev/null | head -1)"
    corepages="$(sqlite_query "$CORE_DB" 'PRAGMA page_count' 2>/dev/null | head -1)"
    record db-growth.txt \
        "phase=${phase} cp_db_bytes=$(db_file_size "$QUAL_DB") cp_wal_bytes=$(db_file_size "${QUAL_DB}-wal") cp_total_bytes=$(db_total_bytes "$QUAL_DB") cp_pages=${cppages:-unreadable} core_db_bytes=$(db_file_size "$CORE_DB") core_wal_bytes=$(db_file_size "${CORE_DB}-wal") core_total_bytes=$(db_total_bytes "$CORE_DB") core_pages=${corepages:-unreadable} cp_op_rows=$(sqlite_query "$QUAL_DB" 'SELECT COUNT(*) FROM operations' 2>/dev/null | head -1)"
}

# --- log cardinality (normalized message shapes; the M4.8 "no unbounded
# per-op unique strings" gate) ----------------------------------------------
log_off() { stat -c %s "$1" 2>/dev/null || echo 0; }

# log_shapes_between FILE START_BYTE END_BYTE — normalized distinct
# message shapes in the byte window: strip the leading timestamp and ANSI
# color codes, then mask UUIDs / 0x-hex / bare hex runs ≥8 chars / integers
# (ids, ports, durations, counters) so a repeated message with varying
# values collapses to ONE shape. The bare-hex8 mask is load-bearing: VM
# ids, op-id fragments, and tap names are 8-hex-char words — without the
# mask every cycle's new VM re-inflates the shape set (~9 kinds × 3 cycles
# = 27 "new" shapes in run 1 with byte-identical halves, the tell that the
# normalizer — not the product — was at fault; with it, run 1's logs
# normalize to 1–2 new shapes per daemon across halves).
log_shapes_between() {
    tail -c +"$(( $2 + 1 ))" "$1" 2>/dev/null | head -c "$(( $3 - $2 ))" \
        | sed -E \
            -e 's/^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:.]+Z?[[:space:]]*//' \
            -e 's/\x1b\[[0-9;]*m//g' \
            -e 's/[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}/<UUID>/g' \
            -e 's/0x[0-9a-fA-F]+/<HEX>/g' \
            -e 's/[0-9a-fA-F]{8,}/<HEX8>/g' \
            -e 's/[0-9]+/<N>/g' \
        | sort -u
}

declare -A LOG_OFF_A=() LOG_OFF_B=() LOG_OFF_C=()
capture_log_offsets() {
    local which="$1" lg
    for lg in $DAEMONS; do
        case "$which" in
            a) LOG_OFF_A[$lg]="$(log_off "${QUAL_LOGS_DIR}/${lg}.log")" ;;
            b) LOG_OFF_B[$lg]="$(log_off "${QUAL_LOGS_DIR}/${lg}.log")" ;;
            c) LOG_OFF_C[$lg]="$(log_off "${QUAL_LOGS_DIR}/${lg}.log")" ;;
        esac
    done
}

# metrics_series_count — Prometheus series (non-comment lines) at the
# agent's /metrics (deploy binds 127.0.0.1:9100); "unreadable" if the
# endpoint does not answer (callers must guard non-numeric).
metrics_series_count() {
    local body
    body="$(curl -s --max-time 5 http://127.0.0.1:9100/metrics 2>/dev/null || true)"
    if [ -z "$body" ]; then
        echo "unreadable"
        return 0
    fi
    printf '%s\n' "$body" | grep -vc '^#' || true
}
is_number() { [[ "$1" =~ ^[0-9]+$ ]]; }

# ---------------------------------------------------------------------------
# VM observation + lifecycle helpers (M4.3/M4.5/M4.7 contracts)
# ---------------------------------------------------------------------------

vm_console_log() { echo "${VMS_DIR}/$1/console.log"; }
console_has() { grep -aq "$2" "$(vm_console_log "$1")" 2>/dev/null; }
console_bytes() { stat -c %s "$(vm_console_log "$1")" 2>/dev/null || echo 0; }
count_logind() {
    local f
    f="$(vm_console_log "$1")"
    if [ -f "$f" ]; then grep -ac 'systemd-logind' "$f" || true; else echo 0; fi
}

vm_ch_count() {
    { pgrep -f "(^|/)cloud-hypervisor( |$).*vms/$1/" 2>/dev/null || true; } | wc -l | tr -d ' '
}
vm_ch_gone() { [ "$(vm_ch_count "$1")" -eq 0 ]; }
vm_ch_exists() { [ "$(vm_ch_count "$1")" -ge 1 ]; }

ch_pid_of() { cat "${VMS_DIR}/$1/ch.pid" 2>/dev/null || echo ""; }
ch_alive_of() {
    local pid state
    pid="$(ch_pid_of "$1")"
    [ -n "$pid" ] || return 1
    [ -d "/proc/${pid}" ] || return 1
    state="$(awk '{print $3}' "/proc/${pid}/stat" 2>/dev/null || true)"
    [ "$state" != "Z" ] && [ "$state" != "X" ]
}

tap_count() {
    ip -o link show 2>/dev/null | awk -F': ' '{print $2}' | awk '{print $1}' \
        | grep -c '^tap-' || true
}

# vm_state — desired power_state via chvctl (JSON).
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

# BFF fast path (curl + the chvctl-stored JWT — the m4.4 precedent): a
# per-request round-trip without a chvctl process spawn per sample, so
# read-latency numbers measure the API, not the CLI startup.
BFF_TOKEN="$(cat "${QUAL_CHVCTL_CONFIG_DIR}/chvctl/credentials" 2>/dev/null || true)"
bff_post() {
    curl -s --max-time 10 -X POST \
        -H "Authorization: Bearer ${BFF_TOKEN}" \
        -H "Content-Type: application/json" \
        -d "$2" "$QUAL_BFF_URL$1"
}
vm_state_fast() {
    bff_post /v1/vms '{}' \
        | jq -r --arg id "$1" '.items[]? | select(.vm_id == $id) | .power_state // empty' 2>/dev/null \
        | head -1
}

# create_vm NAME CPU MEM_MB → VM_ID (m4.7 form; --memory takes a SIZE
# STRING — bare numbers are BYTES, always suffix with M).
create_vm() {
    local name="$1" cpu="$2" mem="$3" out vm_id
    out="$(qual_chvctl --output json vm create "$name" \
        --cpu "$cpu" --memory "${mem}M" --image "$GUEST_IMAGE_PATH" \
        --network "$DEFAULT_NET" 2>&1)" \
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

volume_id_of() {
    sqlite_query "$QUAL_DB" \
        "SELECT volume_id FROM volume_desired_state WHERE attached_vm_id='$1'" 2>/dev/null | head -1
}
volume_backing() {
    find "${VMS_DIR}/$1" -maxdepth 1 -name '*.img' -type f 2>/dev/null | head -1
}
vm_volume_ready() {
    local p cap
    p="$(volume_backing "$1")"
    cap="$(sqlite_query "$QUAL_DB" \
        "SELECT capacity_bytes FROM volumes WHERE volume_id='$(volume_id_of "$1")'" 2>/dev/null | head -1)"
    [ -n "$p" ] && [ -n "$cap" ] && [ "$(stat -c %s "$p" 2>/dev/null || echo 0)" -ge "$cap" ]
}
vm_volume_started() {
    local p
    p="$(volume_backing "$1")"
    [ -n "$p" ]
}

# --- operation journals (m4.7 contracts) ------------------------------------
cp_vm_ops_terminal() {
    local vm="$1" bad
    bad="$(sqlite_query "$QUAL_DB" \
        "SELECT operation_id || ':' || status FROM operations WHERE resource_kind='vm' AND resource_id='${vm}' AND status IN ('Pending','Accepted','Running','RetryPending','AwaitingOperatorInput')" 2>/dev/null)"
    [ -z "$bad" ] && [ -n "$(sqlite_query "$QUAL_DB" \
        "SELECT COUNT(*) FROM operations WHERE resource_kind='vm' AND resource_id='${vm}'" 2>/dev/null)" ]
}
core_vm_ops_terminal() {
    local vm="$1" bad n
    [ -f "$CORE_DB" ] || return 0
    bad="$(sqlite_query "$CORE_DB" \
        "SELECT operation_id FROM operations WHERE vm_id='${vm}' AND status IN ('accepted','running')" 2>/dev/null)"
    n="$(sqlite_query "$CORE_DB" \
        "SELECT COUNT(*) FROM operations WHERE vm_id='${vm}'" 2>/dev/null)"
    [ -z "$bad" ] && [ -n "$n" ]
}
cp_create_op_count() {
    sqlite_query "$QUAL_DB" \
        "SELECT COUNT(*) FROM operations WHERE resource_kind='vm' AND resource_id='$1' AND operation_type='CreateVm'" 2>/dev/null | head -1
}

# poll_until TIMEOUT CMD... — silent tight (1 s) poll, no assertion
# side effects (usable inside measurement loops and background jobs).
poll_until() {
    local timeout="$1"; shift
    local deadline=$((SECONDS + timeout))
    while ! "$@" >/dev/null 2>&1; do
        sleep 1
        [ "$SECONDS" -ge "$deadline" ] && return 1
    done
    return 0
}

# wait_vm_stopped — graceful-stop wait with the #345 wedge detection and
# SIGKILL remediation (m4.7-verbatim; a wedge is a WARN — the documented
# operator path — and the stop latency record carries the flag).
wait_vm_stopped() {
    local vm="$1" waited=0 strikes=0 pid
    while ! vm_ch_gone "$vm"; do
        if curl -sg --max-time 2 --unix-socket "${VMS_DIR}/${vm}/vm.sock" \
            http://localhost/api/v1/vm.info >/dev/null 2>&1; then
            strikes=0
        else
            strikes=$((strikes + 1))
            if [ "$strikes" -ge 3 ]; then
                pid="$(ch_pid_of "$vm")"
                VM_LAST_STOP_WEDGED=1
                qual_warn "wait_vm_stopped(${vm}): known #345 wedge — CH ${pid} alive with a dead API after the graceful stop; remediating with SIGKILL (the documented operator path)"
                [ -n "$pid" ] && kill -9 "$pid" 2>/dev/null || true
            fi
        fi
        sleep 2
        waited=$((waited + 2))
        if [ "$waited" -ge "$STOP_TIMEOUT" ]; then
            qual_error "wait_vm_stopped(${vm}): guest did not stop within ${STOP_TIMEOUT}s"
            return 1
        fi
    done
    return 0
}

# ---------------------------------------------------------------------------
# Idle resource window (P1/P3 shape) — RECORDS + the two at-rest
# ASSERTIONS that use the window (DB growth; fd/socket baseline capture)
# ---------------------------------------------------------------------------
idle_resource_window() {
    local label="$1"
    local csv="${EVIDENCE_DIR}/idle-${label}.csv"
    echo "daemon,sample,rss_kb" > "$csv"
    local t0 t1 elapsed_us name pid i line
    local -A live=() ticks_start=()
    for name in $DAEMONS; do
        pid="$(daemon_pid "$name")"
        if [ -n "$pid" ] && [ -r "/proc/${pid}/stat" ]; then
            live[$name]="$pid"
        else
            qual_error "idle window ${label}: daemon ${name} not running (pid '${pid:-none}')"
        fi
    done
    local sample_args=()
    for name in $DAEMONS; do
        [ -n "${live[$name]:-}" ] && sample_args+=("${name}:${live[$name]}")
    done
    # Starting CPU ticks: one spawn for all daemons.
    while read -r name line _; do
        [ -n "${live[$name]:-}" ] && ticks_start[$name]="$line"
    done < <(proc_sample_all "${sample_args[@]:-}")
    t0="$(now_us)"
    for i in $(seq 1 "$IDLE_SAMPLES"); do
        sleep "$IDLE_INTERVAL"
        while read -r name _ rss; do
            [ -n "${live[$name]:-}" ] && echo "${name},${i},${rss}" >> "$csv"
        done < <(proc_sample_all "${sample_args[@]:-}")
    done
    t1="$(now_us)"
    elapsed_us=$((t1 - t0))
    for name in $DAEMONS; do
        [ -n "${live[$name]:-}" ] || continue
        pid="${live[$name]}"
        local ticks_end="" rss_dist cpu_pct
        while read -r n2 t2 _; do
            [ "$n2" = "$name" ] && ticks_end="$t2"
        done < <(proc_sample_all "${name}:${pid}")
        if [ -n "${ticks_start[$name]:-}" ] && [ -n "$ticks_end" ]; then
            cpu_pct="$(awk -v dt=$(( ticks_end - ticks_start[$name] )) \
                -v clk="$CLK_TCK" -v e="$elapsed_us" \
                'BEGIN { printf "%.3f", (dt / clk) / (e / 1000000) * 100 }')"
        else
            cpu_pct="unreadable"
        fi
        rss_dist="$(csv_col_dist "$csv" 3 1 "$name" | dist_stats)"
        record idle-resources.txt \
            "label=${label} daemon=${name} pid=${pid} window_s=$(latency_s "$t0" "$t1") cpu_idle_pct=${cpu_pct} rss_kb ${rss_dist}"
        record fd-sockets.txt \
            "phase=${label} daemon=${name} pid=${pid} fds=$(proc_fd_count "$pid") sockets=$(proc_sock_count "$pid")"
    done
    qual_pass "idle window ${label}: ${#live[@]} daemons sampled over $(latency_s "$t0" "$t1") s at rest (records in idle-${label}.csv)"
}

# idle_db_growth_check LABEL T0BYTES — at-rest DB growth ASSERTION.
idle_db_growth_check() {
    local label="$1" before="$2" after grow
    after="$(db_total_bytes "$QUAL_DB")"
    grow=$(( after - before ))
    record db-growth.txt "phase=${label}-idle-window cp_total_bytes_before=${before} cp_total_bytes_after=${after} growth_bytes=${grow}"
    if [ "$grow" -le "$IDLE_DB_GROWTH_MAX" ]; then
        qual_pass "idle window ${label}: CP DB net growth at rest ${grow} bytes (≤ ${IDLE_DB_GROWTH_MAX} threshold)"
    else
        qual_error "idle window ${label}: FORBIDDEN — CP DB grew ${grow} bytes across a $((IDLE_SAMPLES * IDLE_INTERVAL))s at-rest window with zero ops (unbounded growth)"
    fi
}

# idle_log_rate LABEL WHICH — bytes appended per daemon across the idle
# window just completed (WHICH selects the offsets captured at that
# window's start: a for P1, c for P3). Steady-state log rate record;
# warn-only threshold.
idle_log_rate() {
    local label="$1" which="$2" lg off0 off1 bytes rate
    for lg in $DAEMONS; do
        case "$which" in
            a) off0="${LOG_OFF_A[$lg]:-0}" ;;
            c) off0="${LOG_OFF_C[$lg]:-0}" ;;
            *) off0=0 ;;
        esac
        off1="$(log_off "${QUAL_LOGS_DIR}/${lg}.log")"
        bytes=$(( off1 - off0 ))
        rate="$(awk -v b="$bytes" -v s=$((IDLE_SAMPLES * IDLE_INTERVAL)) \
            'BEGIN { printf "%.1f", b / s * 60 }')"
        record log-rate.txt "phase=${label} daemon=${lg} idle_bytes=${bytes} bytes_per_min=${rate}"
        if awk -v r="$rate" -v m=262144 'BEGIN { exit !(r > m) }'; then
            qual_warn "idle window ${label}: ${lg} logs ${rate} bytes/min at rest (> 256 KiB/min — recorded for the evidence doc)"
        fi
    done
    qual_pass "idle window ${label}: steady-state log rate recorded (log-rate.txt)"
}

# ---------------------------------------------------------------------------
# Forbidden-outcome checkers (m4.7 contracts, soak-shaped)
# ---------------------------------------------------------------------------

assert_one_ch_process() {
    local desc="$1" n
    n="$(count_cloud_hypervisor_processes)"
    if [ "$n" = "$CH_EXPECTED" ]; then
        qual_pass "${desc}: cloud-hypervisor process count == ${CH_EXPECTED}"
    else
        qual_error "${desc}: FORBIDDEN — expected ${CH_EXPECTED} cloud-hypervisor process(es), found ${n}"
        pgrep -af '(^|/)cloud-hypervisor( |$)' >&2 || true
    fi
}

assert_taps_clean() {
    local desc="$1" waited=0 n
    n="$(tap_count)"
    while [ "$n" != "$TAPS_EXPECTED" ] && [ "$waited" -lt "$TAP_SETTLE" ]; do
        sleep 2
        waited=$((waited + 2))
        n="$(tap_count)"
    done
    if [ "$n" = "$TAPS_EXPECTED" ]; then
        qual_pass "${desc}: tap count == ${TAPS_EXPECTED} (no orphaned taps)"
    else
        qual_error "${desc}: FORBIDDEN — tap count ${n}, expected ${TAPS_EXPECTED}: $(ip -o link show | awk -F': ' '{print $2}' | awk '{print $1}' | grep '^tap-' | tr '\n' ' ')"
    fi
}

assert_no_ch_for_vm() {
    local desc="$1" vm="$2"
    if vm_ch_gone "$vm"; then
        qual_pass "${desc}: no cloud-hypervisor process for ${vm}"
    else
        qual_error "${desc}: FORBIDDEN — cloud-hypervisor process(es) for ${vm}: $(pgrep -af "vms/${vm}/" | head -2)"
    fi
}

assert_no_stuck_ops() {
    local desc="$1" vm="$2"
    poll_until "$OPS_SETTLE" cp_vm_ops_terminal "$vm" \
        || qual_error "${desc}: CP ops for ${vm} stuck non-terminal"
    if [ -f "$CORE_DB" ]; then
        poll_until "$OPS_SETTLE" core_vm_ops_terminal "$vm" \
            || qual_error "${desc}: core.db ops for ${vm} stuck non-terminal"
    else
        qual_error "${desc}: core DB missing (${CORE_DB})"
    fi
    qual_pass "${desc}: both journals terminal for ${vm} (CP: [$(sqlite_query "$QUAL_DB" "SELECT operation_type || ':' || status FROM operations WHERE resource_kind='vm' AND resource_id='${vm}'" 2>/dev/null | tr '\n' ' ')])"
}

assert_one_create_op() {
    local desc="$1" vm="$2" n
    n="$(cp_create_op_count "$vm")"
    if [ "$n" = "1" ]; then
        qual_pass "${desc}: exactly one CreateVm op row for ${vm}"
    else
        qual_error "${desc}: FORBIDDEN — ${n} CreateVm op rows for ${vm}"
    fi
}

assert_backings_have_rows() {
    local desc="$1" rows phys vol orphan=0 list=""
    rows="$(sqlite_query "$QUAL_DB" 'SELECT volume_id FROM volumes' 2>/dev/null)"
    phys="$(find "$VMS_DIR" -mindepth 2 -maxdepth 2 -name '*.img' -type f \
        -printf '%f\n' 2>/dev/null | sed 's/\.img$//' | sort -u)"
    for vol in $phys; do
        if ! printf '%s\n' "$rows" | grep -qx "$vol"; then
            orphan=1
            list="${list} ${vol}"
        fi
    done
    if [ "$orphan" = "0" ]; then
        qual_pass "${desc}: every physical volume backing has a CP row (no orphaned disks)"
    else
        qual_error "${desc}: FORBIDDEN — physical volume(s) without a CP row (orphaned disks):${list}"
    fi
}

save_state_evidence() {
    local label="$1"
    {
        echo "### m4.8 evidence snapshot: ${label} ($(date -u +%FT%TZ))"
        echo "--- CP operations:"
        sqlite_query "$QUAL_DB" \
            "SELECT operation_id, resource_id, operation_type, status FROM operations ORDER BY requested_at" 2>/dev/null || true
        echo "--- live stord ($(stord_pid)) sessions:"
        sqlite_query "$(stord_db)" \
            "SELECT volume_id, vm_id, runtime_status FROM sessions" 2>/dev/null || true
        echo "--- host links (chv-owned):"
        ip -br link 2>/dev/null | grep -E 'tap-|chvbr|^br-' || true
        echo "--- CH processes:"
        pgrep -af '(^|/)cloud-hypervisor( |$)' || true
        echo
    } >> "${EVIDENCE_DIR}/host-state.txt"
}

# ---------------------------------------------------------------------------
# Migration topology (M4.6 contracts, verbatim where possible)
# ---------------------------------------------------------------------------
scenario_cleanup() {
    local pid alive
    for pid in "${M48_PIDS[@]:-}"; do
        [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
    done
    for _ in $(seq 1 25); do
        alive=0
        for pid in "${M48_PIDS[@]:-}"; do
            if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
                alive=1
            fi
        done
        [ "$alive" -eq 0 ] && break
        sleep 0.2
    done
    for pid in "${M48_PIDS[@]:-}"; do
        [ -n "$pid" ] && kill -9 "$pid" 2>/dev/null || true
    done
    # Scoped safety net (argv[0]-anchored, this scenario's config dir).
    pkill -f "(^|/)chv-stord( |$).*${M48_DIR}" 2>/dev/null || true
    sleep 0.5
    pkill -9 -f "(^|/)chv-stord( |$).*${M48_DIR}" 2>/dev/null || true
}
trap scenario_cleanup EXIT
trap 'exit 1' INT TERM

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
    {
        echo "### m4.8 grpcurl provisioning record ($(date -u +%FT%TZ))"
        cat "${GRPCURL_DIR}/expected.sha256"
        "$GRPCURL" --version 2>&1 | head -1
    } >> "${EVIDENCE_DIR}/grpcurl-provisioning.txt"
}

stord_rpc() {
    local sock="$1" method="$2" json="$3"
    "$GRPCURL" -plaintext \
        -import-path "${REPO_ROOT}/proto/node" -proto chv-stord-api.proto \
        -d "$json" "unix://${sock}" \
        "chv.node.stord.v1.StorageService/${method}"
}

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

close_volume_quiet() {
    local sock="$1" volid="$2" handle="$3" resp status
    resp="$(stord_rpc "$sock" CloseVolume \
        "{\"volumeId\":\"${volid}\",\"attachmentHandle\":\"${handle}\"}")" \
        || { qual_error "CloseVolume RPC failed for ${volid}"; return 1; }
    status="$(printf '%s' "$resp" | jq -r '.status // empty')"
    if [ "$status" = "OK" ]; then
        qual_pass "CloseVolume accepted for ${volid} (session closed)"
        return 0
    fi
    qual_error "CloseVolume for ${volid} not OK (status=${status})"
    return 1
}

try_trigger_migration() {
    local sock="$1" volid="$2" handle="$3" endpoint="$4"
    local json resp status mid
    json="{\"volumeId\":\"${volid}\",\"attachmentHandle\":\"${handle}\",\"destEndpoint\":\"${endpoint}\"}"
    resp="$(stord_rpc "$sock" TriggerDiskMigration "$json" 2>&1)" || { printf ''; return 1; }
    status="$(printf '%s' "$resp" | jq -r '.result.status // empty')"
    mid="$(printf '%s' "$resp" | jq -r '.migrationId // empty')"
    if [ "$status" = "OK" ] && [ -n "$mid" ]; then
        printf '%s' "$mid"
        return 0
    fi
    printf ''
    return 1
}

trigger_migration() {
    local mid
    mid="$(try_trigger_migration "$@")" \
        || { qual_error "TriggerDiskMigration not accepted: $*"; return 1; }
    printf '%s' "$mid"
}

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
phase_is() { [ "$(migration_field "$1" "$2" phase)" = "$3" ]; }

start_scenario_stord() {
    local config="$1" log="$2" pid
    "${QUAL_BINARY_DIR}/chv-stord" "$config" > "$log" 2>&1 &
    pid=$!
    disown "$pid"
    M48_PIDS+=("$pid")
    printf '%s' "$pid"
}
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

wait_phase() {
    local sock="$1" mid="$2" want="$3" timeout="$4" p deadline
    deadline=$((SECONDS + timeout))
    while :; do
        p="$(migration_field "$sock" "$mid" phase)"
        [ "$p" = "$want" ] && return 0
        case "$p" in FAILED | COMPLETED) return 1 ;; esac
        [ "$SECONDS" -ge "$deadline" ] && return 2
        sleep 0.2
    done
}

# ===========================================================================
qual_info "=== M4.8 performance & soak baseline start (node ${QUAL_NODE_ID}) ==="
qual_info "candidate: $(cat "${QUAL_BINARY_DIR}/CANDIDATE_SHA" 2>/dev/null || echo unknown)"
qual_info "host baseline label: baseline measurement on the qualification host (16 vCPU, 31 GiB RAM, nested KVM), not a scale claim"
[ -s "$GUEST_IMAGE_PATH" ] \
    || qual_die "guest image missing: ${GUEST_IMAGE_PATH} — build it with patch-guest-image.sh and run deploy.sh with GUEST_IMAGE=noble-qual-patched.img"

# Tool preflight (everything the measurement methodology depends on).
[ -n "${EPOCHREALTIME:-}" ] || qual_die "EPOCHREALTIME missing (bash >= 5 required for microsecond timing)"
for tool in jq curl python3 getconf find stat awk comm; do
    command -v "$tool" >/dev/null 2>&1 || qual_die "measurement tool missing: ${tool}"
done
[ -n "$BFF_TOKEN" ] || qual_die "no BFF token available (chvctl credentials) — the read-latency methodology needs it"
qual_pass "measurement tooling present (microsecond clock, jq, curl, python3, /proc sampling)"

# Identity guard: the milestone exercises the whole stack (CP, agent,
# stord, nwd, Core) — the staged candidate must be identical to THIS
# tree for all product code (crates/, cmd/, proto/); this scenario and
# the evidence docs are scripts/docs-only and never trip it.
CANDIDATE_SHA="$(cat "${QUAL_BINARY_DIR}/CANDIDATE_SHA" 2>/dev/null || true)"
if [ -n "$CANDIDATE_SHA" ]; then
    if git -C "$REPO_ROOT" diff --quiet "$CANDIDATE_SHA" HEAD -- crates cmd proto; then
        qual_pass "candidate identity: crates/cmd/proto identical at ${CANDIDATE_SHA:0:8} and HEAD"
    else
        qual_die "crates/cmd/proto DIFFER between candidate ${CANDIDATE_SHA:0:8} and HEAD — the staged binaries would not test this tree"
    fi
else
    qual_warn "no candidate sha available — identity check skipped; results apply to THIS tree only"
fi

pids_current
save_state_evidence "start"

# ---------------------------------------------------------------------------
# Baselines (m4.7 preamble shape)
# ---------------------------------------------------------------------------
if [ "$(count_cloud_hypervisor_processes)" -eq 0 ]; then
    qual_pass "baseline: no cloud-hypervisor processes on the host"
else
    qual_error "baseline: cloud-hypervisor processes already present (unclean host?): $(pgrep -af cloud-hypervisor | head -3)"
fi
CH_EXPECTED=0
TAPS_BASELINE="$(tap_count)"
TAPS_EXPECTED="$TAPS_BASELINE"
qual_pass "baseline: ${TAPS_BASELINE} pre-existing tap interface(s) recorded"

# One BFF round-trip via the fast path before any measurement relies on
# it (a stale/expired JWT would poison every read-latency number).
BFF_PROBE_CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 -X POST \
    -H "Authorization: Bearer ${BFF_TOKEN}" -H "Content-Type: application/json" \
    -d '{}' "$QUAL_BFF_URL/v1/vms")"
[ "$BFF_PROBE_CODE" = "200" ] \
    && qual_pass "BFF fast path verified (GET-shape list probe HTTP 200)" \
    || qual_die "BFF fast path probe returned ${BFF_PROBE_CODE} — read-latency methodology unusable"

echo "cycle,op,accept_rtt_s,convergence_s,note" > "$LIFECYCLE_CSV"
echo "phase,endpoint,http_code,time_total_s" > "$READS_CSV"
capture_log_offsets a
record measurements.txt "scenario start; N_SOAK=${N_SOAK} READERS=${READERS} WRITERS=${WRITERS} WORKLOAD_WINDOW=${WORKLOAD_WINDOW}s IDLE_WINDOW=$((IDLE_SAMPLES * IDLE_INTERVAL))s"

# ===========================================================================
# P1 — idle baseline at rest (zero VMs)
# ===========================================================================
qual_info "--- P1: idle daemon resources at rest (zero VMs, ${IDLE_SAMPLES}×${IDLE_INTERVAL}s window)"

P1_DB_T0="$(db_total_bytes "$QUAL_DB")"
P1_CORE_T0="$(db_total_bytes "$CORE_DB")"
P1_METRICS="$(metrics_series_count)"
record metrics-cardinality.txt "phase=p1idle series=${P1_METRICS}"
idle_resource_window p1-idle
idle_db_growth_check p1-idle "$P1_DB_T0"
idle_log_rate p1-idle a
if is_number "${P1_METRICS:-}" && [ "$P1_METRICS" -le "$METRIC_SERIES_MAX" ]; then
    qual_pass "P1: agent /metrics series count ${P1_METRICS} (≤ ${METRIC_SERIES_MAX})"
else
    qual_error "P1: agent /metrics series count ${P1_METRICS:-unreadable} exceeds ${METRIC_SERIES_MAX} or unreadable (unbounded metric cardinality)"
fi
record_db_sizes p1-idle
# fd/socket baseline for the P1→P3 leak comparison.
declare -A FD_BASE=() SOCK_BASE=()
for name in $DAEMONS; do
    pid="$(daemon_pid "$name")"
    if [ -n "$pid" ]; then
        FD_BASE[$name]="$(proc_fd_count "$pid")"
        SOCK_BASE[$name]="$(proc_sock_count "$pid")"
    fi
done
save_state_evidence "p1-idle"

# ===========================================================================
# P2 — bounded steady-state soak + lifecycle latency distributions
# ===========================================================================
qual_info "--- P2: soak — ${N_SOAK} sequential create→start→boot→stop→delete cycles + repeated list/get traffic"

# M2.5 retention truth (disclosed ONCE, not per cycle): on VM delete the
# BFF list/volume row/backing are RETAINED (authority-side delete). The
# soak's residue assertions are the PHYSICAL truth (processes, taps,
# sessions, journals); the retained rows are the recorded product shape.
qual_warn "soak preamble: deleted VMs remain visible in the BFF list and retain rows/backings (documented M2.5 deferred scope) — physical-side cleanup is what each cycle asserts"

capture_log_offsets b   # mid-soak marker is set after cycle N_SOAK/2
LOG_MID_MARKED=0

for c in $(seq 1 "$N_SOAK"); do
    qual_info "--- soak cycle ${c}/${N_SOAK}"
    VM_LAST_STOP_WEDGED=0

    # --- create -------------------------------------------------------------
    T_SUB="$(now_us)"
    SOAK_VM="$(create_vm "qual-soak-c${c}" 1 512)" || qual_die "aborting (soak cycle ${c} create)"
    T_ACC="$(now_us)"
    qual_pass "cycle ${c}: vm create accepted (${SOAK_VM})"
    if poll_until "$OPS_SETTLE" vm_volume_ready "$SOAK_VM"; then
        T_CONV="$(now_us)"
        mark_created "$SOAK_VM"
        TAPS_EXPECTED=$((TAPS_EXPECTED + 1))
        echo "${c},create,$(latency_s "$T_SUB" "$T_ACC"),$(latency_s "$T_ACC" "$T_CONV"),converged-Created" >> "$LIFECYCLE_CSV"
        qual_pass "cycle ${c}: create converged in $(latency_s "$T_ACC" "$T_CONV") s (volume materialized; accept RTT $(latency_s "$T_SUB" "$T_ACC") s)"
    else
        qual_error "cycle ${c}: create never converged within ${OPS_SETTLE}s"
        echo "${c},create,$(latency_s "$T_SUB" "$T_ACC"),timeout,create-timeout" >> "$LIFECYCLE_CSV"
        # Best-effort cleanup so the failure does not poison later cycles
        # (the error is already recorded; residue assertions below the
        # next cycle will catch anything this cannot reap).
        qual_chvctl vm delete "$SOAK_VM" >/dev/null 2>&1 || true
        mark_deleted "$SOAK_VM"
        poll_until "$OPS_SETTLE" vm_ch_gone "$SOAK_VM" || true
        continue
    fi
    # A converged create spawns EXACTLY ONE idle VMM (api-socket only —
    # spawn-at-create, process.rs create_vm) and provisions its tap.
    poll_until 15 vm_ch_exists "$SOAK_VM" || true
    if [ "$(vm_ch_count "$SOAK_VM")" -eq 1 ]; then
        qual_pass "cycle ${c}: exactly one idle VMM for the created-not-started VM"
    else
        qual_error "cycle ${c}: FORBIDDEN — $(vm_ch_count "$SOAK_VM") VMM process(es) for the created VM"
    fi
    assert_taps_clean "cycle ${c} (tap provisioned at create)"

    # --- start --------------------------------------------------------------
    T_SUB="$(now_us)"
    qual_chvctl vm start "$SOAK_VM" >/dev/null 2>&1 \
        && qual_pass "cycle ${c}: vm start accepted" \
        || qual_error "cycle ${c}: vm start rejected"
    T_ACC="$(now_us)"
    # Start convergence = desired Running AND the console receiving
    # bytes (payload pushed, firmware executing) — the host truth, not
    # the submit-level BFF answer.
    start_converged() {
        [ "$(vm_state_fast "$SOAK_VM")" = "Running" ] && [ "$(console_bytes "$SOAK_VM")" -gt 0 ]
    }
    if poll_until 120 start_converged; then
        T_CONV="$(now_us)"
        echo "${c},start,$(latency_s "$T_SUB" "$T_ACC"),$(latency_s "$T_ACC" "$T_CONV"),Running+console" >> "$LIFECYCLE_CSV"
        qual_pass "cycle ${c}: start converged in $(latency_s "$T_ACC" "$T_CONV") s (Running + console flowing; accept RTT $(latency_s "$T_SUB" "$T_ACC") s)"
    else
        qual_error "cycle ${c}: start never converged (state '$(vm_state_fast "$SOAK_VM")', console $(console_bytes "$SOAK_VM") bytes)"
        echo "${c},start,$(latency_s "$T_SUB" "$T_ACC"),timeout,start-timeout" >> "$LIFECYCLE_CSV"
    fi

    # --- boot evidence (also the ACPI-safety gate for the stop) --------------
    T_B0="$(now_us)"
    T_B1="$T_B0"
    if poll_until "$BOOT_TIMEOUT" console_has "$SOAK_VM" "Linux version"; then
        T_B1="$(now_us)"
        echo "${c},boot-to-banner,0.000,$(latency_s "$T_B0" "$T_B1"),kernel-banner" >> "$LIFECYCLE_CSV"
        qual_pass "cycle ${c}: guest kernel banner ($(latency_s "$T_B0" "$T_B1") s)"
    else
        qual_error "cycle ${c}: no kernel banner within ${BOOT_TIMEOUT}s — guest did not boot"
    fi
    # logind lines must be re-counted on every poll (a command
    # substitution in the argument list would expand once).
    cycle_logind_ready() { [ "$(count_logind "$SOAK_VM")" -ge 2 ]; }
    if poll_until "$LOGIND_TIMEOUT" cycle_logind_ready; then
        T_B2="$(now_us)"
        echo "${c},boot-to-logind,0.000,$(latency_s "$T_B1" "$T_B2"),banner-to-logind" >> "$LIFECYCLE_CSV"
        qual_pass "cycle ${c}: guest logind (banner→logind $(latency_s "$T_B1" "$T_B2") s; full boot $(latency_s "$T_B0" "$T_B2") s — ACPI-safe stop)"
    else
        qual_error "cycle ${c}: no logind evidence — the graceful stop may be silently lost (M2.5)"
    fi

    # VMM running-RSS record (baseline, not a claim).
    VMM_PID="$(ch_pid_of "$SOAK_VM")"
    if [ -n "$VMM_PID" ]; then
        VMM_RSS="$(awk '/^VmRSS:/ { print $2; exit }' "/proc/${VMM_PID}/status" 2>/dev/null || echo 0)"
        record vmm-resources.txt "cycle=${c} vm=${SOAK_VM} ch_pid=${VMM_PID} running_rss_kb=${VMM_RSS:-0} console_bytes=$(console_bytes "$SOAK_VM")"
    fi

    # --- repeated read traffic while the VM runs -----------------------------
    READ_BAD=0
    for r in $(seq 1 "$SOAK_READS"); do
        for ep in list get; do
            if [ "$ep" = "list" ]; then
                ROW="$(curl -s -o /dev/null -w '%{http_code},%{time_total}' --max-time 10 -X POST \
                    -H "Authorization: Bearer ${BFF_TOKEN}" -H "Content-Type: application/json" \
                    -d '{}' "$QUAL_BFF_URL/v1/vms")"
            else
                ROW="$(curl -s -o /dev/null -w '%{http_code},%{time_total}' --max-time 10 -X POST \
                    -H "Authorization: Bearer ${BFF_TOKEN}" -H "Content-Type: application/json" \
                    -d "{\"vm_id\":\"${SOAK_VM}\"}" "$QUAL_BFF_URL/v1/vms/get")"
            fi
            echo "cycle${c},${ep},${ROW}" >> "$READS_CSV"
            [ "${ROW%%,*}" = "200" ] || READ_BAD=$((READ_BAD + 1))
        done
    done
    if [ "$READ_BAD" -eq 0 ]; then
        qual_pass "cycle ${c}: $((SOAK_READS * 2)) read requests while Running, all HTTP 200"
    else
        qual_error "cycle ${c}: ${READ_BAD} read request(s) non-200 under single-reader steady state"
    fi

    # --- stop ----------------------------------------------------------------
    T_SUB="$(now_us)"
    qual_chvctl vm stop "$SOAK_VM" >/dev/null 2>&1 \
        && qual_pass "cycle ${c}: vm stop accepted" \
        || qual_error "cycle ${c}: vm stop rejected"
    T_ACC="$(now_us)"
    if wait_vm_stopped "$SOAK_VM"; then
        T_CONV="$(now_us)"
        NOTE="graceful"
        [ "$VM_LAST_STOP_WEDGED" = "1" ] && NOTE="#345-wedge-sigkill"
        echo "${c},stop,$(latency_s "$T_SUB" "$T_ACC"),$(latency_s "$T_ACC" "$T_CONV"),${NOTE}" >> "$LIFECYCLE_CSV"
        qual_pass "cycle ${c}: stop converged in $(latency_s "$T_ACC" "$T_CONV") s (CH process exited; ${NOTE})"
    else
        echo "${c},stop,$(latency_s "$T_SUB" "$T_ACC"),timeout,stop-timeout" >> "$LIFECYCLE_CSV"
    fi

    # --- delete (physical convergence: CH gone + tap removed + session closed)
    T_SUB="$(now_us)"
    qual_chvctl vm delete "$SOAK_VM" >/dev/null 2>&1 \
        && qual_pass "cycle ${c}: vm delete accepted" \
        || qual_error "cycle ${c}: vm delete rejected"
    T_ACC="$(now_us)"
    SOAK_VOL="$(volume_id_of "$SOAK_VM")"
    TAPS_EXPECTED=$((TAPS_EXPECTED - 1))
    delete_converged() {
        vm_ch_gone "$SOAK_VM" \
            && [ "$(tap_count)" = "$TAPS_EXPECTED" ] \
            && { [ -z "$SOAK_VOL" ] || [ "$(stord_sessions "$SOAK_VOL" 2>/dev/null || echo 0)" = "0" ]; }
    }
    if poll_until "$OPS_SETTLE" delete_converged; then
        T_CONV="$(now_us)"
        mark_deleted "$SOAK_VM"
        echo "${c},delete,$(latency_s "$T_SUB" "$T_ACC"),$(latency_s "$T_ACC" "$T_CONV"),ch+tap+session" >> "$LIFECYCLE_CSV"
        qual_pass "cycle ${c}: delete converged in $(latency_s "$T_ACC" "$T_CONV") s (CH gone, tap removed, stord session closed; accept RTT $(latency_s "$T_SUB" "$T_ACC") s)"
    else
        qual_error "cycle ${c}: delete residue — CH $(vm_ch_count "$SOAK_VM"), taps $(tap_count)/${TAPS_EXPECTED}, sessions $(stord_sessions "$SOAK_VOL" 2>/dev/null || echo '?')"
        mark_deleted "$SOAK_VM"
    fi

    # --- per-cycle forbidden-outcome assertions (zero residue) ----------------
    # (Set explicitly, not arithmetically: post-delete the host MUST be
    # at zero VMMs regardless of which convergence branch fired above.)
    CH_EXPECTED=0
    assert_one_ch_process "cycle ${c} post-delete"
    assert_taps_clean "cycle ${c} post-delete"
    assert_no_stuck_ops "cycle ${c}" "$SOAK_VM"
    assert_one_create_op "cycle ${c}" "$SOAK_VM"
    record_db_sizes "soak-cycle-${c}"
    save_state_evidence "soak-cycle-${c}"

    if [ "$LOG_MID_MARKED" -eq 0 ] && [ "$c" -eq $((N_SOAK / 2)) ]; then
        capture_log_offsets b
        LOG_MID_MARKED=1
        # Mid-soak fd/socket + metrics sample (records for the growth curve).
        for name in $DAEMONS; do
            pid="$(daemon_pid "$name")"
            [ -n "$pid" ] && record fd-sockets.txt \
                "phase=soak-mid daemon=${name} pid=${pid} fds=$(proc_fd_count "$pid") sockets=$(proc_sock_count "$pid")"
        done
        record metrics-cardinality.txt "phase=soak-mid series=$(metrics_series_count)"
        qual_pass "mid-soak fd/socket + metrics samples recorded (after cycle ${c})"
    fi
done

capture_log_offsets c
for name in $DAEMONS; do
    pid="$(daemon_pid "$name")"
    [ -n "$pid" ] && record fd-sockets.txt \
        "phase=soak-end daemon=${name} pid=${pid} fds=$(proc_fd_count "$pid") sockets=$(proc_sock_count "$pid")"
done
record metrics-cardinality.txt "phase=soak-end series=$(metrics_series_count)"
qual_pass "end-soak fd/socket + metrics samples recorded"
record_db_sizes soak-end

# ===========================================================================
# P3 — idle at rest AFTER the soak (leak verdicts)
# ===========================================================================
qual_info "--- P3: idle at rest after the soak (leak verdicts + records)"

P3_DB_T0="$(db_total_bytes "$QUAL_DB")"
idle_resource_window p3-idle
idle_db_growth_check p3-idle "$P3_DB_T0"
idle_log_rate p3-idle c

# fd/socket growth at rest: P3 vs P1, same net-zero-VM state, all ops
# settled. THE leak assertion of the milestone.
FD_LEAK_MAX=0
FD_VERDICT_BAD=0
for name in $DAEMONS; do
    pid="$(daemon_pid "$name")"
    [ -n "$pid" ] || continue
    fds_now="$(proc_fd_count "$pid")"
    socks_now="$(proc_sock_count "$pid")"
    fd_delta=$(( fds_now - ${FD_BASE[$name]:-0} ))
    sock_delta=$(( socks_now - ${SOCK_BASE[$name]:-0} ))
    record fd-sockets.txt "phase=p3-verdict daemon=${name} pid=${pid} fds=${fds_now} sockets=${socks_now} fd_delta_vs_p1=${fd_delta} sock_delta_vs_p1=${sock_delta}"
    [ "$fd_delta" -gt "$FD_LEAK_MAX" ] && FD_LEAK_MAX="$fd_delta"
    if [ "$fd_delta" -gt "$FD_GROWTH_MAX" ]; then
        qual_error "P3: FORBIDDEN — ${name} gained ${fd_delta} fds at rest across the soak (> ${FD_GROWTH_MAX}; per-op fd leak)"
        FD_VERDICT_BAD=1
    fi
    if [ "$sock_delta" -gt "$SOCK_GROWTH_MAX" ]; then
        qual_error "P3: FORBIDDEN — ${name} gained ${sock_delta} socket fds at rest across the soak (> ${SOCK_GROWTH_MAX})"
        FD_VERDICT_BAD=1
    fi
    rss_a="$(awk -F, -v d="$name" '$1 == d { s += $3; n++ } END { if (n) printf "%d", s / n }' "${EVIDENCE_DIR}/idle-p1-idle.csv" 2>/dev/null || echo 0)"
    rss_b="$(awk -F, -v d="$name" '$1 == d { s += $3; n++ } END { if (n) printf "%d", s / n }' "${EVIDENCE_DIR}/idle-p3-idle.csv" 2>/dev/null || echo 0)"
    record rss-growth.txt "daemon=${name} mean_rss_p1_kb=${rss_a:-0} mean_rss_p3_kb=${rss_b:-0} delta_kb=$(( ${rss_b:-0} - ${rss_a:-0} ))"
    if [ $(( ${rss_b:-0} - ${rss_a:-0} )) -gt $((RSS_GROWTH_WARN_MB * 1024)) ]; then
        qual_warn "P3: ${name} mean RSS grew by $(( (${rss_b:-0} - ${rss_a:-0}) / 1024 )) MiB across the soak (record-only; allocator retention makes gating unsound)"
    fi
done
if [ "$FD_VERDICT_BAD" = "0" ]; then
    qual_pass "P3: fd/socket growth at rest within thresholds (max fd delta ${FD_LEAK_MAX} ≤ ${FD_GROWTH_MAX})"
fi

P3_METRICS="$(metrics_series_count)"
record metrics-cardinality.txt "phase=p3idle series=${P3_METRICS}"
if is_number "${P1_METRICS:-}" && is_number "${P3_METRICS:-}"; then
    if [ "$(( P3_METRICS - P1_METRICS ))" -le 10 ]; then
        qual_pass "P3: /metrics series at rest unchanged across the soak (${P1_METRICS} → ${P3_METRICS})"
    else
        qual_error "P3: FORBIDDEN — /metrics series grew at rest across the soak (${P1_METRICS} → ${P3_METRICS}; unbounded metric cardinality)"
    fi
else
    qual_warn "P3: /metrics unreadable at one of the at-rest windows — cardinality comparison skipped"
fi
save_state_evidence "p3-idle"

# ===========================================================================
# P4 — bounded concurrent API workload (NOT a scale claim)
# ===========================================================================
qual_info "--- P4: bounded concurrent workload — ${READERS} readers + ${WRITERS} writers, ${WORKLOAD_WINDOW}s (bounded by this host, not a scale claim)"

# Anchor VM: created-not-started (idle VMM) so readers' vm get has a
# stable subject that no writer touches.
ANCHOR_VM="$(create_vm "qual-load-anchor" 1 512)" || qual_die "aborting (P4 anchor)"
if poll_until "$OPS_SETTLE" vm_volume_ready "$ANCHOR_VM"; then
    mark_created "$ANCHOR_VM"
    TAPS_EXPECTED=$((TAPS_EXPECTED + 1))
    qual_pass "P4: anchor VM created and converged (${ANCHOR_VM})"
else
    qual_die "P4: anchor VM never converged"
fi
poll_until 15 vm_ch_exists "$ANCHOR_VM" || true
CH_EXPECTED=1
assert_one_ch_process "P4 (anchor running)"
assert_taps_clean "P4 (anchor tap provisioned)"

# Reader loop (background, silent — the parent owns all assertions): a
# list + a get per iteration until the window closes; every request's
# http_code + time_total lands in the shared read-latency CSV.
reader_loop() {
    local id="$1" deadline=$((SECONDS + WORKLOAD_WINDOW)) row
    while [ "$SECONDS" -lt "$deadline" ]; do
        row="$(curl -s -o /dev/null -w '%{http_code},%{time_total}' --max-time 10 -X POST \
            -H "Authorization: Bearer ${BFF_TOKEN}" -H "Content-Type: application/json" \
            -d '{}' "$QUAL_BFF_URL/v1/vms")"
        echo "p4-r${id}-list,list,${row}" >> "$READS_CSV"
        row="$(curl -s -o /dev/null -w '%{http_code},%{time_total}' --max-time 10 -X POST \
            -H "Authorization: Bearer ${BFF_TOKEN}" -H "Content-Type: application/json" \
            -d "{\"vm_id\":\"${ANCHOR_VM}\"}" "$QUAL_BFF_URL/v1/vms/get")"
        echo "p4-r${id}-get,get,${row}" >> "$READS_CSV"
    done
}

# Writer loop (background, silent): sequential create → converge →
# delete cycles via the hermetic chvctl path; per-cycle timings land in
# a per-writer CSV. Statuses are folded by the parent.
writer_loop() {
    local id="$1" n=0 deadline=$((SECONDS + WORKLOAD_WINDOW))
    local csv="${EVIDENCE_DIR}/p4-writer-${id}.csv"
    echo "cycle,create_accept_s,create_conv_s,delete_accept_s,delete_conv_s,status" > "$csv"
    local t0 t1 t2 t3 vm out vol
    while [ "$SECONDS" -lt "$deadline" ]; do
        n=$((n + 1))
        t0="$(now_us)"
        out="$(qual_chvctl --output json vm create "qual-load-w${id}-c${n}" \
            --cpu 1 --memory 512M --image "$GUEST_IMAGE_PATH" \
            --network "$DEFAULT_NET" 2>/dev/null || true)"
        t1="$(now_us)"
        vm="$(printf '%s\n' "$out" | python3 -c '
import json, sys
raw = sys.stdin.read()
start = raw.find("{")
try:
    data = json.loads(raw[start:])
    print(data.get("vm_id") or data.get("id") or "")
except Exception:
    print("")')"
        if [ -z "$vm" ]; then
            echo "${n},$(latency_s "$t0" "$t1"),,,,create-rejected" >> "$csv"
            continue
        fi
        if ! poll_until "$OPS_SETTLE" vm_volume_ready "$vm"; then
            echo "${n},$(latency_s "$t0" "$t1"),timeout,,,create-timeout" >> "$csv"
            continue
        fi
        t2="$(now_us)"
        poll_until 15 vm_ch_exists "$vm" || true
        qual_chvctl vm delete "$vm" >/dev/null 2>&1 || true
        t3="$(now_us)"
        vol="$(sqlite_query "$QUAL_DB" \
            "SELECT volume_id FROM volume_desired_state WHERE attached_vm_id='${vm}'" 2>/dev/null | head -1)"
        # The session count must be re-queried on every poll (a command
        # substitution in the argument list would expand once).
        writer_vol_closed() { [ "$(stord_sessions "$1" 2>/dev/null || echo 0)" = "0" ]; }
        if poll_until "$OPS_SETTLE" vm_ch_gone "$vm" \
            && { [ -z "$vol" ] || poll_until 60 writer_vol_closed "$vol"; }; then
            echo "${n},$(latency_s "$t0" "$t1"),$(latency_s "$t1" "$t2"),$(latency_s "$t2" "$t3"),$(latency_s "$t3" "$(now_us)"),ok" >> "$csv"
        else
            echo "${n},$(latency_s "$t0" "$t1"),$(latency_s "$t1" "$t2"),$(latency_s "$t2" "$t3"),timeout,delete-residue" >> "$csv"
        fi
    done
}

T_W0="$(now_us)"
P4_JOBS=()
for r in $(seq 1 "$READERS"); do
    reader_loop "$r" &
    P4_JOBS+=("$!")
done
for w in $(seq 1 "$WRITERS"); do
    writer_loop "$w" &
    P4_JOBS+=("$!")
done
for j in "${P4_JOBS[@]}"; do
    wait "$j" || true
done
T_W1="$(now_us)"
qual_pass "P4: workload window completed in $(latency_s "$T_W0" "$T_W1") s (${READERS} readers + ${WRITERS} writers)"

# Reader verdicts: every request 200 (a 5xx under bounded load is a
# crash-class forbidden outcome), latency distributions recorded.
P4_NON200="$(awk -F, 'NR > 1 && $1 ~ /^p4-/ && $3 != "200" { n++ } END { print n + 0 }' "$READS_CSV")"
P4_REQS="$(awk -F, 'NR > 1 && $1 ~ /^p4-/ { n++ } END { print n + 0 }' "$READS_CSV")"
if [ "$P4_NON200" -eq 0 ]; then
    qual_pass "P4: all ${P4_REQS} concurrent read requests HTTP 200 (no 5xx under bounded load)"
else
    qual_error "P4: FORBIDDEN — ${P4_NON200}/${P4_REQS} concurrent read request(s) non-200"
    awk -F, 'NR > 1 && $3 != "200" { print; if (++n >= 5) exit }' "$READS_CSV" >&2 || true
fi
record measurements.txt "P4 reader-1 list: $(csv_col_dist "$READS_CSV" 4 1 p4-r1-list | dist_stats)"
for r in $(seq 1 "$READERS"); do
    record measurements.txt "P4 reader-${r} list: $(csv_col_dist "$READS_CSV" 4 1 "p4-r${r}-list" | dist_stats)"
    record measurements.txt "P4 reader-${r} get: $(csv_col_dist "$READS_CSV" 4 1 "p4-r${r}-get" | dist_stats)"
done
record measurements.txt "P4 all-readers list: $(awk -F, 'NR > 1 && $1 ~ /-list$/ { print $4 }' "$READS_CSV" | dist_stats)"
record measurements.txt "P4 all-readers get: $(awk -F, 'NR > 1 && $1 ~ /-get$/ { print $4 }' "$READS_CSV" | dist_stats)"
qual_pass "P4: read latency distributions recorded (read-latency.csv)"

# Writer verdicts.
P4_WSTATUS_BAD=0
P4_WCYCLES=0
for w in $(seq 1 "$WRITERS"); do
    wcsv="${EVIDENCE_DIR}/p4-writer-${w}.csv"
    P4_WCYCLES=$(( P4_WCYCLES + $(awk -F, 'NR > 1 { n++ } END { print n + 0 }' "$wcsv") ))
    bad="$(awk -F, 'NR > 1 && $6 != "ok" { n++ } END { print n + 0 }' "$wcsv")"
    [ "$bad" -gt 0 ] && P4_WSTATUS_BAD=$(( P4_WSTATUS_BAD + bad ))
    record measurements.txt "P4 writer-${w} create-accept: $(csv_col_dist "$wcsv" 2 | dist_stats)"
    record measurements.txt "P4 writer-${w} create-converge: $(csv_col_dist "$wcsv" 3 | dist_stats)"
    record measurements.txt "P4 writer-${w} delete-converge: $(csv_col_dist "$wcsv" 5 | dist_stats)"
done
record measurements.txt "P4 writers completed ${P4_WCYCLES} create+delete cycles under concurrency (baseline, not a scale claim)"
if [ "$P4_WSTATUS_BAD" -eq 0 ]; then
    qual_pass "P4: all ${P4_WCYCLES} writer cycles completed cleanly under concurrency"
else
    qual_error "P4: ${P4_WSTATUS_BAD} writer cycle(s) failed/left residue under concurrency"
    for w in $(seq 1 "$WRITERS"); do
        awk -F, 'NR > 1 && $6 != "ok" { print }' "${EVIDENCE_DIR}/p4-writer-${w}.csv" >&2 || true
    done
fi

# Delete the anchor, then the workload residue assertions.
qual_chvctl vm delete "$ANCHOR_VM" >/dev/null 2>&1 \
    && qual_pass "P4: anchor VM delete accepted" \
    || qual_error "P4: anchor VM delete rejected"
ANCHOR_VOL="$(volume_id_of "$ANCHOR_VM")"
TAPS_EXPECTED=$((TAPS_EXPECTED - 1))
mark_deleted "$ANCHOR_VM"
anchor_delete_converged() {
    vm_ch_gone "$ANCHOR_VM" \
        && [ "$(tap_count)" = "$TAPS_EXPECTED" ] \
        && { [ -z "$ANCHOR_VOL" ] || [ "$(stord_sessions "$ANCHOR_VOL" 2>/dev/null || echo 0)" = "0" ]; }
}
poll_until "$OPS_SETTLE" anchor_delete_converged \
    && qual_pass "P4: anchor delete converged (CH gone, tap removed, session closed)" \
    || qual_error "P4: anchor delete residue (CH $(vm_ch_count "$ANCHOR_VM"), taps $(tap_count)/${TAPS_EXPECTED}, sessions $(stord_sessions "$ANCHOR_VOL" 2>/dev/null || echo '?'))"

# Writer VMs (M2.5-rows retained): every created VM's journal is terminal.
CH_EXPECTED=0
assert_one_ch_process "P4 post-workload"
assert_taps_clean "P4 post-workload"
P4_SESSIONS_LEFT="$(sqlite_query "$(stord_db)" \
    "SELECT COUNT(*) FROM sessions" 2>/dev/null | head -1)"
if [ "${P4_SESSIONS_LEFT}" = "0" ]; then
    qual_pass "P4 post-workload: live stord sessions all closed (0 rows)"
elif [ -z "${P4_SESSIONS_LEFT}" ]; then
    qual_error "P4 post-workload: could not read the live stord sessions DB — residue state unknown"
else
    qual_error "P4 post-workload: FORBIDDEN — stord sessions remain (${P4_SESSIONS_LEFT}): $(sqlite_query "$(stord_db)" 'SELECT volume_id, runtime_status FROM sessions' 2>/dev/null | head -3)"
fi
all_cp_ops_terminal() {
    [ "$(sqlite_query "$QUAL_DB" \
        "SELECT COUNT(*) FROM operations WHERE status IN ('Pending','Accepted','Running','RetryPending','AwaitingOperatorInput')" 2>/dev/null)" = "0" ]
}
poll_until "$OPS_SETTLE" all_cp_ops_terminal \
    && qual_pass "P4 post-workload: every CP operation terminal" \
    || qual_error "P4 post-workload: ops stuck non-terminal: $(sqlite_query "$QUAL_DB" "SELECT operation_id || ':' || status FROM operations WHERE status IN ('Pending','Accepted','Running','RetryPending','AwaitingOperatorInput')" 2>/dev/null | tr '\n' ' ')"
qual_chvctl node list >/dev/null 2>&1 \
    && qual_pass "P4 post-workload: CP healthy after the workload (BFF answers)" \
    || qual_error "P4 post-workload: CP not answering after the workload"
record_db_sizes p4-post-workload
save_state_evidence "p4-post-workload"

# ===========================================================================
# P5 — migration throughput on the qualified path (M4.6 topology)
# ===========================================================================
qual_info "--- P5: migration throughput — two standalone mTLS stords, seeded 4 GiB, BULK_COPY timing"

provision_grpcurl
command -v jq >/dev/null 2>&1 || qual_die "jq missing (m4.6-proven dependency)"

mkdir -p "$CERTS_DIR" "${M48_DIR}/openssl-ca/newcerts"
touch "${M48_DIR}/openssl-ca/index.txt"
echo 1000 > "${M48_DIR}/openssl-ca/serial"

openssl genrsa -out "${CERTS_DIR}/ca.key" 2048 2>/dev/null
openssl req -x509 -new -nodes -key "${CERTS_DIR}/ca.key" -sha256 -days 2 \
    -out "${CERTS_DIR}/ca.crt" \
    -subj "/O=CHV Qual M48/CN=m48-qual-ca" 2>/dev/null

make_leaf() {
    local name="$1" cn="$2" ext="$3"
    openssl genrsa -out "${CERTS_DIR}/${name}.key" 2048 2>/dev/null
    openssl req -new -key "${CERTS_DIR}/${name}.key" -out "${CERTS_DIR}/${name}.csr" \
        -subj "/O=CHV Qual M48/CN=${cn}" 2>/dev/null
    printf '%s\n' "$ext" > "${CERTS_DIR}/${name}.ext"
    openssl x509 -req -in "${CERTS_DIR}/${name}.csr" \
        -CA "${CERTS_DIR}/ca.crt" -CAkey "${CERTS_DIR}/ca.key" \
        -CAcreateserial -out "${CERTS_DIR}/${name}.crt" -days 2 -sha256 \
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

# NOTE (M4.6 §4.2 truth, issue #401): a receiver-only stord is not
# expressible — enabled=true makes the client identity mandatory, so DST
# carries an (unused) client identity too.
make_leaf src-client m48-src-client "$CLIENT_EXT"
make_leaf dst-server m48-dst-server "$SERVER_EXT"
make_leaf dst-client m48-dst-client "$CLIENT_EXT"
chmod 644 "$CERTS_DIR"/*.crt
chmod 600 "$CERTS_DIR"/*.key
openssl verify -CAfile "${CERTS_DIR}/ca.crt" "${CERTS_DIR}/src-client.crt" >/dev/null 2>&1 \
    && qual_pass "P5 precondition: migration CA chain verifies" \
    || qual_die "P5 precondition: CA chain does not verify — the leg would be vacuous"

if tcp_port_free "$DST_PORT"; then
    qual_pass "P5 precondition: destination port ${DST_PORT} is free"
else
    qual_die "P5 precondition: destination port ${DST_PORT} is already in use — refusing to run"
fi

write_src_config() {
    cat > "$1" <<EOF
socket_path = "${2}"
runtime_dir = "${3}"
log_level = "info"
metrics_bind = "127.0.0.1:0"
path_allowlist = ["${3}"]

[migration]
enabled = true
client_cert_path = "${CERTS_DIR}/src-client.crt"
client_key_path = "${CERTS_DIR}/src-client.key"
ca_cert_path = "${CERTS_DIR}/ca.crt"
dest_server_name = "localhost"
EOF
}
write_dst_config() {
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
ca_cert_path = "${CERTS_DIR}/ca.crt"
dest_server_name = "localhost"
listen_addr = "127.0.0.1:${4}"
server_cert_path = "${CERTS_DIR}/dst-server.crt"
server_key_path = "${CERTS_DIR}/dst-server.key"
client_ca_path = "${CERTS_DIR}/ca.crt"
EOF
}

SRC_DIR="${M48_DIR}/src"; DST_DIR="${M48_DIR}/dst"
mkdir -p "$SRC_DIR" "$DST_DIR"
SRC_SOCK="${SRC_DIR}/api.sock"; DST_SOCK="${DST_DIR}/api.sock"
SRC_LOG="${SRC_DIR}/stord.log"; DST_LOG="${DST_DIR}/stord.log"

write_src_config "${SRC_DIR}/stord.toml" "$SRC_SOCK" "$SRC_DIR"
write_dst_config "${DST_DIR}/stord.toml" "$DST_SOCK" "$DST_DIR" "$DST_PORT"

SRC_PID="$(start_scenario_stord "${SRC_DIR}/stord.toml" "$SRC_LOG")"
DST_PID="$(start_scenario_stord "${DST_DIR}/stord.toml" "$DST_LOG")"
wait_for "P5: SRC stord UDS up" 20 test -S "$SRC_SOCK" \
    || qual_die "P5: SRC stord did not come up — log: $(tail -20 "$SRC_LOG" 2>/dev/null)"
wait_for "P5: DST stord UDS up" 20 test -S "$DST_SOCK" \
    || qual_die "P5: DST stord did not come up — log: $(tail -20 "$DST_LOG" 2>/dev/null)"
assert_file_contains "P5: DST receiver listener bound (mTLS, client auth required)" \
    "$DST_LOG" "storage migration receiver listening on 127.0.0.1:${DST_PORT}"

# Seed: 4 GiB patterned (one random 4 MiB block repeated) — the M4.6/
# M4.7 size, so the throughput record is comparable across milestones.
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
    && qual_pass "P5: 4 GiB patterned seed prepared" \
    || qual_die "P5: seed file preparation failed"

P5_VOL="m48vol"
P5_DB_T0="$(db_total_bytes "$QUAL_DB")"
P5_HANDLE="$(open_volume "$SRC_SOCK" "$P5_VOL" "vol.img" "$VOL_SIZE_BYTES" "$SEED_FILE")" \
    || qual_die "aborting (P5)"
qual_pass "P5: OpenVolume on SRC: ${P5_VOL} seeded (handle ${P5_HANDLE})"

P5_MID="$(trigger_migration "$SRC_SOCK" "$P5_VOL" "$P5_HANDLE" "https://127.0.0.1:${DST_PORT}")" \
    || qual_die "aborting (P5)"
qual_pass "P5: TriggerDiskMigration accepted → ${P5_MID}"

# Throughput measurement (RECORD): BULK_COPY duration from phase entry
# to phase exit, sampled at ${SAMPLE_CADENCE}s; bytesTransferred resets
# past BULK_COPY (the M4.6 truth), so the denominator is totalBytes.
P5_SAMPLES_CSV="${EVIDENCE_DIR}/migration-throughput-samples.csv"
echo "t_elapsed_s,phase,bytes_transferred" > "$P5_SAMPLES_CSV"
P5_BULK_T0=""
P5_BULK_T1=""
P5_BULK_SEEN=0
P5_DEADLINE=$((SECONDS + MIG_RPC_TIMEOUT))
# src/dst stord CPU ticks at bulk start (records).
P5_SRC_TICKS_A="$(awk '{ print $14 + $15 }' "/proc/${SRC_PID}/stat" 2>/dev/null || echo 0)"
P5_DST_TICKS_A="$(awk '{ print $14 + $15 }' "/proc/${DST_PID}/stat" 2>/dev/null || echo 0)"
P5_T0="$(now_us)"
while :; do
    p="$(migration_field "$SRC_SOCK" "$P5_MID" phase)"
    b="$(migration_field "$SRC_SOCK" "$P5_MID" bytesTransferred)"
    echo "$(latency_s "$P5_T0" "$(now_us)"),${p},${b:-0}" >> "$P5_SAMPLES_CSV"
    case "$p" in
        BULK_COPY)
            [ "$P5_BULK_SEEN" -eq 0 ] && { P5_BULK_SEEN=1; P5_BULK_T0="$(now_us)"; }
            ;;
        PAUSED_FINAL_SYNC | COMPLETED)
            # Bulk end; the sample cadence (grpcurl spawn + sleep) is the
            # resolution limit of the duration — noted in the record.
            [ "$P5_BULK_SEEN" -eq 1 ] && P5_BULK_T1="$(now_us)"
            break
            ;;
        FAILED) break ;;
    esac
    [ "$SECONDS" -ge "$P5_DEADLINE" ] && break
    sleep "$SAMPLE_CADENCE"
done
P5_SRC_TICKS_B="$(awk '{ print $14 + $15 }' "/proc/${SRC_PID}/stat" 2>/dev/null || echo 0)"
P5_DST_TICKS_B="$(awk '{ print $14 + $15 }' "/proc/${DST_PID}/stat" 2>/dev/null || echo 0)"

if [ "$P5_BULK_SEEN" -eq 1 ] && [ -n "$P5_BULK_T1" ]; then
    P5_BULK_S="$(latency_s "$P5_BULK_T0" "$P5_BULK_T1")"
    P5_MIBS="$(awk -v bytes="$VOL_SIZE_BYTES" -v secs="$P5_BULK_S" \
        'BEGIN { printf "%.1f", (bytes / 1048576) / secs }')"
    record migration-throughput.txt \
        "volume_bytes=${VOL_SIZE_BYTES} bulk_copy_s=${P5_BULK_S} throughput_mib_s=${P5_MIBS} src_stord_cpu_s=$(awk -v a="$P5_SRC_TICKS_A" -v b="$P5_SRC_TICKS_B" -v c="$CLK_TCK" 'BEGIN { printf "%.1f", (b - a) / c }') dst_stord_cpu_s=$(awk -v a="$P5_DST_TICKS_A" -v b="$P5_DST_TICKS_B" -v c="$CLK_TCK" 'BEGIN { printf "%.1f", (b - a) / c }') — baseline measurement on the qualification host (16 vCPU, 31 GiB RAM, nested KVM, loopback mTLS), not a scale claim"
    qual_pass "P5: BULK_COPY throughput recorded (${P5_MIBS} MiB/s over ${P5_BULK_S} s; samples in migration-throughput-samples.csv)"
else
    qual_warn "P5: BULK_COPY window not observed (phase raced past the ${SAMPLE_CADENCE}s sampler) — throughput degraded to trigger-to-terminal upper bound: $(latency_s "$P5_T0" "$(now_us)") s"
fi

# Correctness ASSERTIONS (the qualified path must hold; only the speed
# is a record).
wait_for "P5: migration reached PAUSED_FINAL_SYNC" "$MIG_RPC_TIMEOUT" \
    phase_is "$SRC_SOCK" "$P5_MID" "PAUSED_FINAL_SYNC" \
    || qual_error "P5: migration never paused (phase: $(migration_field "$SRC_SOCK" "$P5_MID" phase))"
P5_PAUSE_NEEDS="$(migration_field "$SRC_SOCK" "$P5_MID" needsVmPause)"
[ "$P5_PAUSE_NEEDS" = "true" ] \
    && qual_pass "P5: PAUSED_FINAL_SYNC demands the VM pause (needs_vm_pause=true)" \
    || qual_error "P5: needs_vm_pause is '${P5_PAUSE_NEEDS}' at pause (expected true)"
stord_rpc "$SRC_SOCK" ResumeDiskMigration \
    "{\"migrationId\":\"${P5_MID}\",\"vmPaused\":true}" >/dev/null 2>&1 \
    && qual_pass "P5: ResumeDiskMigration{vm_paused:true} accepted" \
    || qual_error "P5: ResumeDiskMigration RPC failed"
wait_for "P5: migration COMPLETED" "$MIG_RPC_TIMEOUT" \
    phase_is "$SRC_SOCK" "$P5_MID" "COMPLETED" \
    || qual_error "P5: migration never completed (phase: $(migration_field "$SRC_SOCK" "$P5_MID" phase))"
P5_SRC_SHA="$(sha256_of "${SRC_DIR}/vol.img")"
P5_DST_SHA="$(sha256_of "${DST_DIR}/${P5_VOL}.img")"
[ "$P5_SRC_SHA" = "$P5_DST_SHA" ] \
    && qual_pass "P5: digest match (${P5_SRC_SHA:0:16}…)" \
    || qual_error "P5: digest mismatch (src=${P5_SRC_SHA} dst=${P5_DST_SHA})"
cmp -s "${SRC_DIR}/vol.img" "${DST_DIR}/${P5_VOL}.img" \
    && qual_pass "P5: byte-compare identical" \
    || qual_error "P5: byte-compare failed"
{
    echo "### P5 digest record ($(date -u +%FT%TZ))"
    echo "src ${SRC_DIR}/vol.img ${P5_SRC_SHA}"
    echo "dst ${DST_DIR}/${P5_VOL}.img ${P5_DST_SHA}"
} >> "${EVIDENCE_DIR}/digests.txt"

# The direct stord↔stord path must leave the CP DB untouched (M4.6/M4.7
# truth) — recorded, and asserted as no-growth.
P5_DB_T1="$(db_total_bytes "$QUAL_DB")"
record db-growth.txt "phase=p5-migration cp_total_bytes_before=${P5_DB_T0} cp_total_bytes_after=${P5_DB_T1} growth_bytes=$(( P5_DB_T1 - P5_DB_T0 ))"
if [ "$P5_DB_T1" -le $(( P5_DB_T0 + IDLE_DB_GROWTH_MAX )) ]; then
    qual_pass "P5: CP DB unchanged across the migration (direct stord↔stord path)"
else
    qual_error "P5: CP DB grew $(( P5_DB_T1 - P5_DB_T0 )) bytes across a stord↔stord migration (unexpected journaling)"
fi

# Close-out: sessions closed, stords stopped, zero residue.
close_volume_quiet "$SRC_SOCK" "$P5_VOL" "$P5_HANDLE" || true
stop_scenario_stord "$SRC_PID" "SRC"
stop_scenario_stord "$DST_PID" "DST"
P5_SESSIONS_LEFT="$(sqlite_query "${SRC_DIR}/stord.db" \
    "SELECT COUNT(*) FROM sessions" 2>/dev/null | head -1)"
if [ "${P5_SESSIONS_LEFT}" = "0" ]; then
    qual_pass "P5: no stord sessions remain on the scenario source"
elif [ -z "${P5_SESSIONS_LEFT}" ]; then
    qual_error "P5: could not read the scenario source sessions DB — residue state unknown"
else
    qual_error "P5: stord sessions remain on the scenario source (${P5_SESSIONS_LEFT})"
fi
if [ -z "$(pgrep -f "(^|/)chv-stord( |$).*${M48_DIR}" 2>/dev/null)" ]; then
    qual_pass "P5: no scenario chv-stord processes remain"
else
    qual_error "P5: scenario chv-stord processes remain: $(pgrep -af "(^|/)chv-stord( |$).*${M48_DIR}" | head -3)"
fi
tcp_port_free "$DST_PORT" \
    && qual_pass "P5: destination port ${DST_PORT} freed" \
    || qual_error "P5: destination port ${DST_PORT} still in use"
save_state_evidence "p5-migration"

# ===========================================================================
# P6 — final sweep + summaries
# ===========================================================================
qual_info "--- P6: final sweep (zero residue, all journals terminal) + measurement summaries"

CH_EXPECTED=0
assert_one_ch_process "final sweep (all VMs deleted)"
TAPS_EXPECTED="$TAPS_BASELINE"
assert_taps_clean "final sweep (zero scenario taps)"
assert_backings_have_rows "final sweep (row ↔ backing bijection, physical→row)"
P6_SESSIONS_LEFT="$(sqlite_query "$(stord_db)" \
    "SELECT COUNT(*) FROM sessions" 2>/dev/null | head -1)"
if [ "${P6_SESSIONS_LEFT}" = "0" ]; then
    qual_pass "final sweep: live stord sessions all closed (0 rows)"
elif [ -z "${P6_SESSIONS_LEFT}" ]; then
    qual_error "final sweep: could not read the live stord sessions DB — residue state unknown"
else
    qual_error "final sweep: FORBIDDEN — stord sessions remain (${P6_SESSIONS_LEFT}): $(sqlite_query "$(stord_db)" 'SELECT volume_id, runtime_status FROM sessions' 2>/dev/null | head -3)"
fi
poll_until "$OPS_SETTLE" all_cp_ops_terminal \
    && qual_pass "final sweep: every CP operation terminal" \
    || qual_error "final sweep: ops stuck non-terminal: $(sqlite_query "$QUAL_DB" "SELECT operation_id || ':' || status FROM operations WHERE status IN ('Pending','Accepted','Running','RetryPending','AwaitingOperatorInput')" 2>/dev/null | tr '\n' ' ')"

P6_DUP_CREATES=0
for vm in $KNOWN_CREATED; do
    n="$(cp_create_op_count "$vm")"
    if [ "$n" != "1" ] && [ "$n" != "0" ]; then
        qual_error "final sweep: FORBIDDEN — ${n} CreateVm op rows for ${vm}"
        P6_DUP_CREATES=1
    fi
done
[ "$P6_DUP_CREATES" = "0" ] \
    && qual_pass "final sweep: exactly one CreateVm op row per created VM" \
    || true

# --- log cardinality verdict (the M4.8 "no unbounded per-op unique
# strings" gate): shapes in soak-half-1 vs soak-half-2, per daemon.
CARDINALITY_BAD=0
for lg in $DAEMONS; do
    lfile="${QUAL_LOGS_DIR}/${lg}.log"
    shapes_a="$(log_shapes_between "$lfile" "${LOG_OFF_A[$lg]:-0}" "${LOG_OFF_B[$lg]:-0}")"
    shapes_b="$(log_shapes_between "$lfile" "${LOG_OFF_B[$lg]:-0}" "${LOG_OFF_C[$lg]:-0}")"
    n_a="$(printf '%s\n' "$shapes_a" | grep -c . || true)"
    n_b="$(printf '%s\n' "$shapes_b" | grep -c . || true)"
    n_new="$(comm -13 <(printf '%s\n' "$shapes_a") <(printf '%s\n' "$shapes_b") | grep -c . || true)"
    record log-cardinality.txt \
        "daemon=${lg} shapes_half1=${n_a} shapes_half2=${n_b} new_shapes_half2=${n_new} bytes_half1=$(( ${LOG_OFF_B[$lg]:-0} - ${LOG_OFF_A[$lg]:-0} )) bytes_half2=$(( ${LOG_OFF_C[$lg]:-0} - ${LOG_OFF_B[$lg]:-0} ))"
    if [ "$n_new" -gt "$NEW_LOG_SHAPES_MAX" ]; then
        qual_error "P6: FORBIDDEN — ${lg} added ${n_new} NEW log message shapes in soak half 2 (> ${NEW_LOG_SHAPES_MAX}; unbounded per-op unique strings)"
        CARDINALITY_BAD=1
    fi
    if [ "$n_b" -gt "$LOG_SHAPES_TOTAL_MAX" ]; then
        qual_error "P6: FORBIDDEN — ${lg} emitted ${n_b} distinct normalized shapes (> ${LOG_SHAPES_TOTAL_MAX})"
        CARDINALITY_BAD=1
    fi
done
[ "$CARDINALITY_BAD" = "0" ] \
    && qual_pass "P6: log cardinality bounded (no unbounded per-op unique strings; log-cardinality.txt)" \
    || true

# --- lifecycle latency distributions (the milestone's headline records).
for op in create start stop delete boot-to-banner boot-to-logind; do
    record measurements.txt "lifecycle ${op}: $(csv_col_dist "$LIFECYCLE_CSV" 4 2 "$op" | dist_stats)"
done
qual_pass "P6: lifecycle latency distributions recorded (lifecycle-latency.csv — count/min/median/p90/max per op)"

# --- DB growth summary: total + per-op bytes attribution.
record_db_sizes final
P6_CP_ROWS="$(sqlite_query "$QUAL_DB" 'SELECT COUNT(*) FROM operations' 2>/dev/null | head -1)"
record db-growth.txt "summary: cp op rows=${P6_CP_ROWS} cp_total_growth_bytes=$(( $(db_total_bytes "$QUAL_DB") - P1_DB_T0 )) core_total_growth_bytes=$(( $(db_total_bytes "$CORE_DB") - P1_CORE_T0 )) per_op_cp_bytes_hint=cp_total_growth/op_rows"
qual_pass "P6: DB growth summary recorded (db-growth.txt — sizes, page counts, per-phase; per-op bytes = growth / op rows)"

# --- final fd/socket sample (the P1→final curve completes at zero VMs).
for name in $DAEMONS; do
    pid="$(daemon_pid "$name")"
    [ -n "$pid" ] && record fd-sockets.txt \
        "phase=final daemon=${name} pid=${pid} fds=$(proc_fd_count "$pid") sockets=$(proc_sock_count "$pid")"
done
record metrics-cardinality.txt "phase=final series=$(metrics_series_count)"
qual_pass "P6: final fd/socket + metrics samples recorded"

save_state_evidence "final"
pids_current

if [ "$QUAL_ERRORS" -eq 0 ]; then
    rm -rf "$M48_DIR"
    [ ! -e "$M48_DIR" ] \
        && qual_pass "scenario resource dir removed (${M48_DIR})" \
        || qual_error "scenario resource dir could not be removed: ${M48_DIR}"
else
    qual_info "errors recorded — ${M48_DIR} kept for post-mortem (deploy preserves TEST_DIR on failure)"
fi

{
    echo "### m4.8 artifacts manifest ($(date -u +%FT%TZ))"
    ls -la "$EVIDENCE_DIR"
} >> "${EVIDENCE_DIR}/manifest.txt"
qual_pass "P6: artifacts manifest written (${EVIDENCE_DIR}/manifest.txt)"

# ---------------------------------------------------------------------------
qual_summary "m4.8-perf-soak"
if [ "$QUAL_ERRORS" -gt 0 ]; then
    qual_error "M4.8 scenario finished with ${QUAL_ERRORS} error(s)"
    qual_info "test dir: ${QUAL_TEST_DIR} (deploy keeps it on failure)"
    exit 1
fi
qual_pass "M4.8 performance & soak baseline complete: zero residue across ${N_SOAK} cycles + bounded workload + migration, all measurements recorded as baseline (not a scale claim)"
exit 0
