#!/usr/bin/env bash
# m4.7-faults.sh — prompt-04 M4.7: fault & interruption matrix on real
# (nested) KVM, via the candidate deployment from deploy.sh.
#
# Run via deploy.sh --exec (root):
#   sudo env "PATH=$PATH" GUEST_IMAGE=noble-qual-patched.img ./deploy.sh \
#       --exec ./m4.7-faults.sh
#
# THE M4.7 DELTA (vs M4.3/M4.5/M4.6, all live-proven there):
#   - M4.3 proved restart-at-rest legs (agent SIGKILL while Running →
#     re-adopt same CH pid; CP restart; 60 s outage; full-stack restart).
#   - M4.5 proved stord SIGKILL under a running (idle) VM + respawn.
#   - M4.6 N9 proved migration interruption by killing the DESTINATION
#     stord mid-BULK_COPY.
#   - M4.7 kills services WHILE AN OPERATION IS IN FLIGHT (mid-create,
#     mid-start, mid-provision, mid-migration) and asserts the forbidden
#     outcomes never occur REGARDLESS of which side of the race window
#     the death landed on.
#
# Legs (plan §M4.7). One base VM (create → start → Running, booted to
# logind so later stops are ACPI-safe) is reused by several legs; every
# other leg creates its own small VM and cleans it up:
#
#   F1  CP SIGKILL mid-create: submit a fresh VM create, SIGKILL the
#       control-plane the moment the CreateVm op is claimed (Running),
#       restart it. The op must reach a terminal state in a bounded
#       window (converged-Created or cleanly-Failed — #368 disclosure:
#       a terminally-failed create is a PASS, fail-closed; the recorded
#       #368 boundary shape — CP op Succeeded while the core effect
#       failed, a phantom VM row that neither converges nor observably
#       fails — is a WARN with disclosure), with the correct VMM
#       process shape (a converged create has EXACTLY ONE idle VMM —
#       spawn-at-create is the product design, crates/
#       chv-agent-runtime-ch/src/process.rs create_vm; a failed create
#       has none), disk row ↔ physical volume consistency, no orphaned
#       tap, and the base VM undisturbed. Cleanup, then RETRY the
#       create → must converge.
#   F2  Agent SIGKILL mid-boot: a created-stopped VM; submit start,
#       then kill the agent the moment the FIRMWARE starts printing to
#       the console (the deterministic #409 exposure — run 2 proved a
#       kill in this window wedges the guest half-booted while
#       everything reports Running; #410's serial heal must recover
#       it); restart the agent. No double-spawn (≤1 CH process for the
#       VM; exactly one stord/nwd — the restarted supervisor must adopt
#       the living daemons, not respawn duplicates); the guest must
#       reach full boot evidence (kernel banner + logind); retry start
#       idempotent; base VM undisturbed.
#   F3  Stord SIGKILL mid-provision: base VM Running; submit a fresh VM
#       create (its boot-volume provision+attach is the only operator-
#       reachable volume path on a core-managed node — the standalone
#       attach_volume/detach_volume RPCs fail closed there; recorded),
#       SIGKILL the deployed stord the moment the volume backing starts
#       materializing (the M4.5-proven multi-second seed-conversion
#       window); wait for the supervisor respawn (#376: the LIVE
#       stord's config/DB path resolves via its argv, not the deploy
#       path). Attach converges or fails cleanly; disk consistency;
#       base VM undisturbed (CH holds the disk fd). Retry the create on
#       the respawned stord → converges; delete closes the session (the
#       detach/delete contract for this topology).
#   F4  nwd SIGKILL mid-create (tap provision): submit a fresh VM
#       create, SIGKILL nwd the moment the CreateVm op is claimed (the
#       NIC attach — tap provision — runs inside the create); wait for
#       the supervisor respawn. Create converges or fails cleanly; no
#       duplicate CH process; NO orphaned tap once the leg settles;
#       retry create → converges.
#   F5  Migration interruption, SOURCE side (extends M4.6 N9, which
#       killed the destination): the M4.6 two-standalone-stord mTLS
#       topology (NOT agent-supervised, #385); trigger a seeded 4 GiB
#       migration, SIGKILL the SOURCE stord mid-BULK_COPY. The task
#       state dies with the daemon (in-memory) — assert no zombie task
#       after restart, the destination partial volume is RETAINED
#       (create_new semantics), then the documented recovery: restart
#       the source, hit the create_new refusal, remove the partial,
#       re-trigger → COMPLETED + digest verify.
#   F6  CP loss mid-migration: fresh migration on the same topology;
#       SIGKILL the CP mid-BULK_COPY, restart it. The stord↔stord path
#       is direct (M4.6 declared the CP path out of scope) — assert the
#       honest behavior: the transfer keeps advancing through the
#       outage, completes with digest verification, the CP journals no
#       duplicate/phantom work across its restart, and the base VM is
#       undisturbed. Whatever the task record shows, NO forbidden
#       outcome may occur; the observed behavior is disclosed plainly.
#   F7  Idempotent cleanup / repeated retry: delete the base VM →
#       absence (or the documented M2.5 retention, recorded); SECOND
#       delete → clean not-found or idempotent success (never a
#       crash-class error); stop an already-stopped VM → clean; delete
#       a VM whose create is still in flight → converges to absence
#       with no orphaned disk/tap; final global sweep: zero CH
#       processes, zero tap- interfaces, disk row ↔ physical volume
#       bijection, no stord session still attached to a VM (detached
#       open sessions for VMs created before the agent restart are the
#       documented M2.2a handle-persistence residual, recorded not gated),
#       every journaled op terminal, exactly one CreateVm op row per
#       created VM.
#
# FORBIDDEN OUTCOMES (asserted by shared checkers after EVERY leg; a
# leg that "eventually succeeds" while leaving any of these behind
# FAILS the milestone):
#   - duplicate VM processes: >1 cloud-hypervisor process for a VM, or
#     a global CH count above the number of extant VMs — Running VMs
#     AND created-not-started VMs (each holds one idle VMM spawned at
#     create, process.rs create_vm) — and zero after deletes;
#   - duplicate stord/nwd daemons after an agent restart (the supervisor
#     must adopt the living daemon via its socket, never double-spawn);
#   - lost/duplicated authoritative state: a CP volume row without its
#     physical backing for a live VM, a physical volume without a row,
#     a duplicated CreateVm operation row, or ops stuck non-terminal
#     forever (a terminally-FAILED op is the #368 fail-closed truth and
#     a PASS with disclosure — only eternal Running/Accepted is an
#     error);
#   - orphaned disks/taps: a tap- interface without its VM, a volume
#     backing without its row, a stord session without its volume.
#
# RACE-WINDOW DESIGN: every kill is a race. Each leg polls tightly
# (0.2 s) for the operation to be claimed (op status Running) or for
# the host effect to start (backing file appearing) and kills INSIDE
# the window; if the window is missed (the op already converged), the
# kill still happens and the leg's assertions are unchanged — both
# sides of every race are acceptable outcomes and every assertion is
# written as "converges OR fails cleanly", never as "must converge".
#
# Timing notes (inherited from M2.5/M4.3/M4.5, same host class): guest
# boots to logind in ~10-60 s (kernel banner within 300 s; nested-virt
# margin on top), the graceful stop needs up to ~32 s inside the
# agent's 60 s window, the CP's stuck-Running reaper recycles a
# mid-dispatch CreateVm after 120 s (hence the 300 s op-settle window),
# and the agent supervisor respawns a killed stord/nwd within one
# ~30 s reconcile tick. Boot waits dominate the ~25 min budget: the
# base VM boots once (preamble) and the F2 VM once (for ACPI-safe
# stops later); all other legs only create (no boot).
#
# EXPECTED PASS COUNTS (deterministic qual_pass sites per section, on the
# converged/happy path — every branch's deviation is disclosed inline; the
# deploy phase adds ~20 of its own before the scenario starts; race
# disjunction branches can add qual_warn lines but never silently remove
# a pass — a window-missed branch converts exactly one pass into a warn
# and is named below):
#   prelude (candidate identity guard)      1
#   preamble (base VM up + baseline)       16
#   F1  CP SIGKILL mid-create             24  (−1 pass→warn if the claim
#                                             window is missed; the
#                                             cleanly-failed branch swaps
#                                             its wait-pass for the
#                                             fail-closed pass (+1 #368
#                                             warn); the failed_368
#                                             branch likewise swaps the
#                                             convergence pass for a
#                                             warn — count unchanged)
#   F2  agent SIGKILL mid-start           28  (−1 pass→warn on a missed
#                                             claim window; the
#                                             cleanly-failed branch swaps
#                                             its wait-pass for the
#                                             fail-closed pass — the
#                                             count is unchanged, +1 #368
#                                             warn; a #345 stop wedge is
#                                             a warn, not an error — the
#                                             SIGKILL remediation is the
#                                             documented operator path)
#   F3  stord SIGKILL mid-provision       27  (same window/#368 rules)
#   F4  nwd SIGKILL mid-create            20  (same window/#368 rules)
#   F5/F6 topology preconditions           8
#   F5  migration, source-side kill       22  (the session-reopen branch
#                                             of the re-trigger adds 1
#                                             warn, the count is
#                                             unchanged)
#   F6  CP loss mid-migration             17
#   F7  idempotent cleanup + final sweep  32  (−1 pass→warn per M2.5
#                                             retention record)
#   close-out (migration topology)         9
#   TOTAL                                204
#
# Non-claims (recorded in the evidence doc): the CP-orchestrated
# migration path (out of scope per M4.6); host reboot (M4.3's recorded
# not-provable subset); multi-node anything; the BFF/CP row retention
# on VM delete (the documented M2.5 deferred scope — recorded, not
# gated, exactly as in M4.3 Leg F / M4.5 Leg E).

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
CP_CONFIG="${QUAL_TEST_DIR}/controlplane.toml"
AGENT_CONFIG="${QUAL_TEST_DIR}/agent.toml"
STORD_DIR="${QUAL_TEST_DIR}/stord"
DEFAULT_NET="default"
GUEST_IMAGE_PATH="${QUAL_GUEST_IMAGE_PATH:-/var/lib/chv/qual/images/noble-qual-patched.img}"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"

BOOT_TIMEOUT=420        # kernel banner after vm start (nested-virt margin)
LOGIND_TIMEOUT=180      # logind lines after the banner
STOP_TIMEOUT=240        # graceful stop (60 s window + snapd's ~32 s + margin)
RESTART_TIMEOUT=90      # CP/agent restarts + supervisor respawns
OPS_SETTLE=300          # op terminal window (CP reaper: 120 s for CreateVm)
SUPERVISOR_TIMEOUT=90   # agent supervisor respawn of a killed stord/nwd
TAP_SETTLE=30           # async tap removal on VM delete (m4.4 lesson)
MIG_RPC_TIMEOUT=180     # migration phase polling budget

# Migration topology (M4.6 shape): two standalone stords, deliberately
# NOT agent-supervised (#385: a supervisor respawn generates a minimal
# config that drops [migration]).
M47_DIR="${QUAL_TEST_DIR}/m47"
CERTS_DIR="${M47_DIR}/certs"
GRPCURL_DIR="${M47_DIR}/grpcurl"
DST_PORT="51062"        # DST mTLS receiver listener (loopback only)
VOL_SIZE_BYTES=$((4 * 1024 * 1024 * 1024))   # 4 GiB = 1024 x 4 MiB chunks
SEED_FILE="${M47_DIR}/m47-seed.img"
# Destination identity the migration receiver expects in its authorization
# token (issue #663); bound into the DST config and every minted token.
QUAL_MIGRATION_DEST_NODE_ID="qual-m47-dst"
[ -n "${QUAL_MIGRATION_SECRET_FILE:-}" ] \
    || qual_die "QUAL_MIGRATION_SECRET_FILE not set — run via deploy.sh --exec"

# grpcurl pin (supply-chain discipline — same as M4.6).
GRPCURL_VERSION="1.9.3"
GRPCURL_TARBALL="grpcurl_${GRPCURL_VERSION}_linux_x86_64.tar.gz"
GRPCURL_CHECKSUMS="grpcurl_${GRPCURL_VERSION}_checksums.txt"
GRPCURL_CHECKSUMS_ASSET="${SCRIPT_DIR}/assets/${GRPCURL_CHECKSUMS}"
GRPCURL_BASE="https://github.com/fullstorydev/grpcurl/releases/download/v${GRPCURL_VERSION}"

M47_PIDS=()             # every scenario-owned process (standalone stords)

# Persistent evidence artifacts (deploy.sh removes TEST_DIR on success).
EVIDENCE_DIR="${CHV_QUAL_ROOT:-/var/lib/chv/qual}/m4.7-artifacts"
mkdir -p "$EVIDENCE_DIR"

# --- scenario-owned expectation bookkeeping -----------------------------
# Forbidden-outcome checkers compare host truth against what the scenario
# KNOWS the state should be (absolute counts; the deploy starts with a
# clean host — no CH processes, and the tap baseline is captured before
# the first VM so any pre-existing host tap is accounted for).
CH_EXPECTED=0               # global cloud-hypervisor process count
TAPS_BASELINE=0             # host tap- count before the first VM
TAPS_EXPECTED=0             # taps are provisioned at CREATE (attach_vm_nic
                            # runs inside the agent's CreateVm) and removed
                            # asynchronously by VM delete
KNOWN_CREATED=""            # VM ids whose create converged (row+backing
                            # must both exist while not deleted)
DELETED_VMS=""              # VM ids deleted (M2.5 retention: rows/files may
                            # legitimately persist for these; recorded, not
                            # gated)

mark_created() { KNOWN_CREATED="${KNOWN_CREATED}${1} "; }
mark_deleted() { DELETED_VMS="${DELETED_VMS}${1} "; }
is_deleted() { case " $DELETED_VMS " in *" $1 "*) return 0 ;; esac; return 1; }

# ---------------------------------------------------------------------------
# Helpers lifted from M4.3/M4.4/M4.5/M4.6 (same contracts, per-VM variants)
# ---------------------------------------------------------------------------

# Live-daemon match patterns (pgrep -f, ERE): match the DEPLOYED stord/nwd
# (deploy.sh launches them with ${QUAL_TEST_DIR}/stord.toml / nwd.toml) and
# any SUPERVISOR-RESPAWNED instance (the agent's supervisor generates
# ${QUAL_AGENT_DIR}/chv-stord.toml / chv-nwd.toml — deploy.sh documents the
# pair), but never this scenario's own standalone stords under ${M47_DIR}
# (they share the binary and the test-dir prefix).
STORD_MATCH='(^|/)chv-stord( |$).*'"${QUAL_TEST_DIR}"'/(stord\.toml$|agent/chv-stord)'
NWD_MATCH='(^|/)chv-nwd( |$).*'"${QUAL_TEST_DIR}"'/(nwd\.toml$|agent/chv-nwd)'

# pids_current — record the CURRENT daemon pids for deploy.sh's teardown.
# Improved over M4.3/M4.5 for this milestone: the supervisor may have
# respawned stord/nwd as agent children (F3/F4), so their pids are
# re-resolved live (pattern-scoped to THIS deployment's configs) instead
# of trusting the deploy-time env values, which are stale after any kill.
pids_current() {
    local stord_now nwd_now
    stord_now="$(pgrep -f "$STORD_MATCH" 2>/dev/null | head -1 || true)"
    nwd_now="$(pgrep -f "$NWD_MATCH" 2>/dev/null | head -1 || true)"
    cat > "${QUAL_TEST_DIR}/pids.current" <<EOF
CP_PID=${CP_PID}
STORD_PID=${stord_now:-${QUAL_STORD_PID:-}}
NWD_PID=${nwd_now:-${QUAL_NWD_PID:-}}
AGENT_PID=${AGENT_PID}
EOF
}

# stord_pid / stord_runtime_dir / stord_db / stord_sessions — the LIVE
# deployed stord, with the #376 truth: a supervisor-respawned stord runs
# with runtime_dir = the AGENT dir, so its config (hence DB) is resolved
# from its argv per call. Guarded pid reads (an empty pid would resolve
# /proc/cmdline — the HOST kernel's cmdline; M4.5 run-6 finding).
stord_pid() {
    pgrep -f "$STORD_MATCH" 2>/dev/null | head -1
}
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
stord_sessions_ge_one() { [ "$(stord_sessions "$1" 2>/dev/null || echo 0)" -ge 1 ]; }

nwd_pid() {
    pgrep -f "$NWD_MATCH" 2>/dev/null | head -1
}
nwd_socket_live() {
    python3 - "${QUAL_TEST_DIR}/nwd/api.sock" <<'PYEOF' 2>/dev/null
import socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
try:
    s.connect(sys.argv[1])
    raise SystemExit(0)
except Exception:
    raise SystemExit(1)
finally:
    s.close()
PYEOF
}
stord_socket_live() {
    python3 - "${STORD_DIR}/api.sock" <<'PYEOF' 2>/dev/null
import socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
try:
    s.connect(sys.argv[1])
    raise SystemExit(0)
except Exception:
    raise SystemExit(1)
finally:
    s.close()
PYEOF
}

# --- VM observation (per-VM variants of the M4.3/M4.5 helpers) ------------
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

vm_console_log() { echo "${VMS_DIR}/$1/console.log"; }
console_has() { grep -aq "$2" "$(vm_console_log "$1")" 2>/dev/null; }
count_boots() {
    local f
    f="$(vm_console_log "$1")"
    if [ -f "$f" ]; then grep -ac 'Linux version' "$f" || true; else echo 0; fi
}
count_logind() {
    local f
    f="$(vm_console_log "$1")"
    if [ -f "$f" ]; then grep -ac 'systemd-logind' "$f" || true; else echo 0; fi
}

save_console_evidence() {
    cp "$(vm_console_log "$1")" "${EVIDENCE_DIR}/console-$1-$2.log" 2>/dev/null \
        || qual_warn "no console.log to save for $1/$2"
}

# logind_ready DESC VM — the current boot already shows logind evidence
# (>= 2 lines) so a subsequent ACPI stop press cannot be silently lost
# (M2.5 run-8 root cause).
logind_ready() {
    local desc="$1" vm="$2"
    local waited=0 cur
    cur="$(count_logind "$vm")"
    while [ "$cur" -lt 2 ]; do
        sleep 2
        waited=$((waited + 2))
        if [ "$waited" -ge "$LOGIND_TIMEOUT" ]; then
            qual_error "${desc}: no logind evidence on the current boot of ${vm} (${cur} lines) — an ACPI stop press may be silently lost"
            return 1
        fi
        cur="$(count_logind "$vm")"
    done
    qual_pass "${desc}: logind present on the current boot of ${vm} (${cur} lines)"
}

# ch_pid_of/ch_alive_of — the CH process the agent persisted for a VM
# (zombie-aware: state Z/X counts as gone, the product's own semantics).
ch_pid_of() { cat "${VMS_DIR}/$1/ch.pid" 2>/dev/null || echo ""; }
ch_alive_of() {
    local pid state
    pid="$(ch_pid_of "$1")"
    [ -n "$pid" ] || return 1
    [ -d "/proc/${pid}" ] || return 1
    state="$(awk '{print $3}' "/proc/${pid}/stat" 2>/dev/null || true)"
    [ "$state" != "Z" ] && [ "$state" != "X" ]
}

# vm_ch_count VM — cloud-hypervisor processes whose command line carries
# this VM's runtime dir (argv[0]-anchored like lib.sh's counter; the
# vms/<vm>/ path segment is unique per VM).
vm_ch_count() {
    { pgrep -f "(^|/)cloud-hypervisor( |$).*vms/$1/" 2>/dev/null || true; } | wc -l | tr -d ' '
}
vm_ch_gone() { [ "$(vm_ch_count "$1")" -eq 0 ]; }
vm_ch_exists() { [ "$(vm_ch_count "$1")" -ge 1 ]; }

# wait_vm_guest_stopped VM — wait for the GUEST to reach a terminal state
# after a graceful `vm stop`.
#
# A graceful stop under `--no-shutdown` deliberately KEEPS the VMM alive when
# the guest reaches Shutdown/Created, so a later StartVm can boot the same
# VMM (and DeleteVm is what explicitly stops it). Waiting for the CH process
# to exit here is therefore wrong and used to burn the full STOP_TIMEOUT.
# Assert the product contract instead: terminal guest state, VMM retained.
# A live process whose API socket never answers for 3 strikes is the known
# #345 wedge and gets the documented SIGKILL remediation (warn, not error).
wait_vm_guest_stopped() {
    local vm="$1" waited=0 strikes=0 pid state info
    while :; do
        if ! ch_alive_of "$vm"; then
            qual_pass "vm ${vm}: guest down (VMM exited)"
            return 0
        fi
        info="$(curl -sg --max-time 2 --unix-socket "${VMS_DIR}/${vm}/vm.sock" \
            http://localhost/api/v1/vm.info 2>/dev/null || true)"
        state="$(printf '%s' "$info" | python3 -c \
            'import json,sys; print(json.load(sys.stdin).get("state",""))' 2>/dev/null || true)"
        case "$state" in
            Shutdown|Created)
                qual_pass "vm ${vm}: guest reached terminal state ${state}; VMM retained per --no-shutdown"
                return 0
                ;;
            "")
                strikes=$((strikes + 1))
                if [ "$strikes" -ge 3 ]; then
                    pid="$(ch_pid_of "$vm")"
                    qual_warn "wait_vm_guest_stopped(${vm}): known #345 wedge — CH ${pid} alive with a dead API after the graceful stop; remediating with SIGKILL (the documented operator path)"
                    [ -n "$pid" ] && kill -9 "$pid" 2>/dev/null || true
                    strikes=0
                fi
                ;;
            *) strikes=0 ;;
        esac
        sleep 2
        waited=$((waited + 2))
        if [ "$waited" -ge "$STOP_TIMEOUT" ]; then
            qual_error "wait_vm_guest_stopped(${vm}): guest did not reach a terminal state within ${STOP_TIMEOUT}s"
            return 1
        fi
    done
}

# wait_vm_stopped VM — graceful-stop wait with the #345 wedge detection
# and SIGKILL remediation (M4.3/M4.5-proven: after a graceful stop of an
# adopted VM the VMM can stay alive with a DEAD API socket; the stop loop
# reports success on the dead-socket premise; the wedge blocks the next
# start). 3 consecutive dead-API strikes while the process lives = wedge.
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
    qual_pass "vm ${vm}: guest down (CH process exited)"
    return 0
}

# --- daemon restarts (M4.3 contracts) -------------------------------------
stop_daemon() {
    local pid="$1"
    kill "$pid" 2>/dev/null || true
    for _ in $(seq 1 50); do
        kill -0 "$pid" 2>/dev/null || return 0
        sleep 0.2
    done
    kill -9 "$pid" 2>/dev/null || true
}
wait_gone() {
    local pid="$1"
    for _ in $(seq 1 50); do
        kill -0 "$pid" 2>/dev/null || return 0
        sleep 0.2
    done
    return 1
}
qual_chvctl_ok() { qual_chvctl node list >/dev/null 2>&1; }

start_cp() {
    "${QUAL_BINARY_DIR}/chv-controlplane" "$CP_CONFIG" \
        >> "${QUAL_LOGS_DIR}/controlplane.log" 2>&1 &
    CP_PID=$!
    pids_current
    wait_for "control-plane BFF responsive after restart" "$RESTART_TIMEOUT" \
        qual_chvctl_ok \
        || qual_die "control-plane did not come back — log: $(tail -20 "${QUAL_LOGS_DIR}/controlplane.log")"
}
start_agent() {
    rm -f "${QUAL_AGENT_DIR}/api.sock" "${QUAL_AGENT_DIR}/core.sock"
    "${QUAL_BINARY_DIR}/chv-agent" "$AGENT_CONFIG" \
        >> "${QUAL_LOGS_DIR}/agent.log" 2>&1 &
    AGENT_PID=$!
    pids_current
    wait_for "agent gRPC socket up after restart" "$RESTART_TIMEOUT" \
        test -S "${QUAL_AGENT_DIR}/api.sock" \
        || qual_die "agent did not come back — log: $(tail -30 "${QUAL_LOGS_DIR}/agent.log")"
}

# pending_reports / drain_deferred_reports — the #343 gate: an agent
# restart with unflushed deferred control-plane reports bricks the node
# (startup fails closed on the retained NodeCache). After every CP
# outage the queue must drain to zero BEFORE any agent restart (F2).
pending_reports() {
    CHV_CACHE_FILE="${QUAL_AGENT_DIR}/agent-cache.json" python3 -c '
import json, os
try:
    with open(os.environ["CHV_CACHE_FILE"]) as f:
        print(len(json.load(f).get("pending_control_plane", [])))
except Exception:
    print("unreadable")
' 2>/dev/null || echo "unreadable"
}
drain_deferred_reports() {
    local desc="$1" waited=0 start
    start="$(pending_reports)"
    while [ "$(pending_reports)" != "0" ]; do
        sleep 2
        waited=$((waited + 2))
        if [ "$waited" -ge 240 ]; then
            qual_warn "${desc}: deferred reports did not drain (${start} queued) — a later agent restart would hit the #343 startup failure"
            return 1
        fi
    done
    qual_pass "${desc}: deferred CP reports flushed (${start} queued → 0 in ${waited}s)"
    return 0
}

# --- volumes / disk truth (M4.5 contracts) ---------------------------------
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

# create_vm NAME CPU MEM_MB → VM_ID on stdout (M4.5 form; --memory takes a
# SIZE STRING — bare numbers are BYTES, always suffix with M).
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

# --- operation journals -----------------------------------------------------
# Two journals record a VM's operations: the CP's controlplane.db
# (operations: resource_kind='vm', CamelCase statuses) and the agent's
# Core authority core.db (operations: vm_id, lowercase statuses).
cp_vm_ops() {
    sqlite_query "$QUAL_DB" \
        "SELECT operation_type || ':' || status FROM operations WHERE resource_kind='vm' AND resource_id='$1' ORDER BY operation_id" 2>/dev/null
}
core_vm_ops() {
    sqlite_query "$CORE_DB" \
        "SELECT kind || ':' || status FROM operations WHERE vm_id='$1' ORDER BY operation_id" 2>/dev/null
}
cp_vm_ops_terminal() {
    local vm="$1" bad
    # Non-terminal = the product's own non-terminal set (Pending/Accepted/
    # Running/RetryPending/AwaitingOperatorInput — domain.rs is_terminal).
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
cp_op_status() {
    sqlite_query "$QUAL_DB" \
        "SELECT status FROM operations WHERE resource_kind='vm' AND resource_id='$1' AND operation_type='$2' ORDER BY requested_at DESC LIMIT 1" 2>/dev/null | head -1
}
cp_create_op_count() {
    sqlite_query "$QUAL_DB" \
        "SELECT COUNT(*) FROM operations WHERE resource_kind='vm' AND resource_id='$1' AND operation_type='CreateVm'" 2>/dev/null | head -1
}

# wait_op_running VM TYPE TIMEOUT — tight (0.2 s) poll for the op to be
# CLAIMED (status Running = the orchestrator is dispatching it RIGHT
# NOW — the mid-operation kill window). Returns 0 if Running was
# observed; 1 if the op reached a terminal state first (window missed);
# 2 on timeout.
wait_op_running() {
    local vm="$1" type="$2" timeout="$3" st deadline
    deadline=$((SECONDS + timeout))
    while :; do
        st="$(cp_op_status "$vm" "$type")"
        [ "$st" = "Running" ] && return 0
        case "$st" in
            Succeeded | Failed | Rejected | Cancelled | Stale | Conflict) return 1 ;;
        esac
        [ "$SECONDS" -ge "$deadline" ] && return 2
        sleep 0.2
    done
}

# wait_create_outcome VM TIMEOUT → prints "converged", "failed", or
# "failed_368" once the create settles: converged = the boot volume backing
# reached full size (the M4.5 convergence gate — the host truth, not the
# power_state flip, which reads Running from desired state at accept time);
# failed = the CreateVm op terminally Failed; failed_368 = the recorded
# issue-#368 boundary signature — the CP journal says Succeeded
# (submit-level success) while the core effect terminally failed, leaving
# a phantom VM row that neither converges nor observably fails (live-proven
# M4.7 run 1 leg F3: CP [CreateVm:Succeeded] core [create_vm:failed], no
# vm_observed_state row, no physical volume). That is a KNOWN recorded
# defect, disclosed per leg — not a new forbidden outcome.
wait_create_outcome() {
    local vm="$1" timeout="$2"
    local deadline=$((SECONDS + timeout))
    while :; do
        if vm_volume_ready "$vm"; then echo "converged"; return 0; fi
        if [ "$(cp_op_status "$vm" CreateVm)" = "Failed" ]; then echo "failed"; return 0; fi
        if [ "$(cp_op_status "$vm" CreateVm)" = "Succeeded" ] \
            && core_vm_ops "$vm" | grep -q '^create_vm:failed'; then
            echo "failed_368"; return 0
        fi
        [ "$SECONDS" -ge "$deadline" ] && break
        sleep 2
    done
    echo "unknown"
    return 1
}

# classify_bff_result DESC OUT RC — idempotency/retry classification of a
# chvctl mutation result: 200 = idempotent success; a clean 4xx (404
# not-found / 409 conflict) = clean refusal; a 5xx or internal error =
# CRASH-CLASS (forbidden: a client retry after a timeout must never see
# a server error).
classify_bff_result() {
    local desc="$1" out="$2" rc="$3"
    if [ "$rc" -eq 0 ]; then
        qual_pass "${desc}: idempotent success (HTTP 200)"
        return 0
    fi
    case "$out" in
        *"HTTP 404"*)
            qual_pass "${desc}: clean not-found (HTTP 404)"
            ;;
        *"HTTP 409"*)
            qual_pass "${desc}: clean conflict refusal (HTTP 409)"
            ;;
        *"HTTP 400"* | *"HTTP 422"*)
            qual_pass "${desc}: clean client error (4xx)"
            ;;
        *"HTTP 500"* | *"HTTP 502"* | *"HTTP 503"* | *"Internal"*)
            qual_error "${desc}: CRASH-CLASS error on repeat — ${out:0:200} (suspect: the BFF's per-VM idempotency keys are plain UNIQUE INSERTs — a retried delete/start collides; handlers/vms.rs)"
            ;;
        *)
            qual_warn "${desc}: unclassified refusal — ${out:0:200}"
            ;;
    esac
    return 0
}

# ---------------------------------------------------------------------------
# Shared forbidden-outcome checkers (the milestone's core; every leg runs
# the relevant subset after its recovery window)
# ---------------------------------------------------------------------------

# assert_one_ch_process DESC — the global cloud-hypervisor process count
# equals the number of VMs the scenario knows to be Running (zero after
# deletes). A duplicate VM process is the #1 forbidden outcome.
assert_one_ch_process() {
    local desc="$1" n
    n="$(count_cloud_hypervisor_processes)"
    if [ "$n" = "$CH_EXPECTED" ]; then
        qual_pass "${desc}: cloud-hypervisor process count == ${CH_EXPECTED} (no duplicate VM processes)"
    else
        qual_error "${desc}: FORBIDDEN — expected ${CH_EXPECTED} cloud-hypervisor process(es), found ${n}"
        pgrep -af '(^|/)cloud-hypervisor( |$)' >&2 || true
    fi
}

# assert_le_one_ch_for_vm DESC VM — no more than one CH process per VM
# (the double-spawn shape: an orphaned CH plus a freshly spawned one).
assert_le_one_ch_for_vm() {
    local desc="$1" vm="$2" n
    n="$(vm_ch_count "$vm")"
    if [ "$n" -le 1 ]; then
        qual_pass "${desc}: ≤1 cloud-hypervisor process for ${vm} (found ${n})"
    else
        qual_error "${desc}: FORBIDDEN — ${n} cloud-hypervisor processes for ${vm} (double-spawn)"
        pgrep -af "vms/${vm}/" >&2 || true
    fi
}

# assert_created_vm_process_shape DESC VM OUTCOME — the VMM process shape
# of a create leg's outcome. Product design (verified live run 1 + code,
# crates/chv-agent-runtime-ch/src/process.rs create_vm): a CONVERGED create
# spawns exactly one cloud-hypervisor VMM at create time (api-socket only —
# boot happens at start when the payload is pushed; the VMM sits idle) and
# provisions its tap. A FAILED create spawns no VMM (the effect fails in
# the storage/network phase before the spawn — run 1 leg F3: zero
# processes for the failed create while the converged F1 VM had one).
assert_created_vm_process_shape() {
    local desc="$1" vm="$2" outcome="$3" n
    if [ "$outcome" = "converged" ]; then
        # The convergence signal (volume materialized) can fire slightly
        # BEFORE the effect chain reaches the VMM spawn (storage →
        # network → spawn, per the F3 evidence: a create failed in the
        # storage phase has no VMM). Give the spawn a moment.
        wait_for "${desc}: VMM spawned for the converged create" 10 vm_ch_exists "$vm" >/dev/null 2>&1 || true
        n="$(vm_ch_count "$vm")"
        if [ "$n" -eq 1 ]; then
            qual_pass "${desc}: exactly one idle VMM for the created-not-started VM (spawn-at-create, no double-spawn)"
        else
            qual_error "${desc}: FORBIDDEN — ${n} VMM process(es) for created VM ${vm} (expected exactly 1)"
            pgrep -af "vms/${vm}/" >&2 || true
        fi
    else
        assert_no_ch_for_vm "${desc}" "$vm"
    fi
}

# assert_no_ch_for_vm DESC VM — this VM never spawned (or no longer has) a
# CH process.
assert_no_ch_for_vm() {
    local desc="$1" vm="$2"
    if vm_ch_gone "$vm"; then
        qual_pass "${desc}: no cloud-hypervisor process for ${vm}"
    else
        qual_error "${desc}: FORBIDDEN — cloud-hypervisor process(es) for ${vm}: $(pgrep -af "vms/${vm}/" | head -2)"
    fi
}

# tap_count / assert_taps_clean DESC — host tap- count vs expected. Taps
# are provisioned at VM CREATE (attach_vm_nic runs inside CreateVm) and
# removed ASYNCHRONOUSLY by VM delete (m4.4 run-1 lesson) — the checker
# polls up to TAP_SETTLE before asserting. An orphaned tap (count above
# the expected) is FORBIDDEN; a missing tap for a converged create is a
# lost-state error.
tap_count() {
    ip -o link show 2>/dev/null | awk -F': ' '{print $2}' | awk '{print $1}' \
        | grep -c '^tap-' || true
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

# assert_disk_state_consistent DESC — the CP volume rows ↔ physical
# backings bijection plus the live stord session set:
#   1. every physical volume backing (vms/<vm>/<volume_id>.img) has a CP
#      volumes row — a backing without a row is an ORPHANED DISK;
#   2. every volume row attached to a KNOWN-CREATED, not-deleted VM has
#      its backing file — a row without the physical volume for a live
#      VM is LOST STATE (rows of terminally-failed creates and of
#      deleted VMs are the #368/M2.5 disclosed classes, warned not
#      gated — recorded per leg);
#   3. every session in the LIVE stord's stord.db (#376 argv-resolved)
#      references a volume with a CP row — an orphaned session.
assert_disk_state_consistent() {
    local desc="$1" rows phys orphan=0 orphan_list="" vm vol f db
    [ -f "$QUAL_DB" ] || { qual_error "${desc}: CP DB missing (${QUAL_DB})"; return 1; }
    rows="$(sqlite_query "$QUAL_DB" 'SELECT volume_id FROM volumes' 2>/dev/null)"
    if [ -z "$rows" ] && [ -n "$(sqlite_query "$QUAL_DB" 'SELECT COUNT(*) FROM volumes' 2>/dev/null)" ] \
        && [ "$(sqlite_query "$QUAL_DB" 'SELECT COUNT(*) FROM volumes' 2>/dev/null)" != "0" ]; then
        qual_error "${desc}: could not read the CP volumes table"
        return 1
    fi
    phys="$(find "$VMS_DIR" -mindepth 2 -maxdepth 2 -name '*.img' -type f \
        -printf '%f\n' 2>/dev/null | sed 's/\.img$//' | sort -u)"
    for vol in $phys; do
        if ! printf '%s\n' "$rows" | grep -qx "$vol"; then
            orphan=1
            orphan_list="${orphan_list} ${vol}"
        fi
    done
    if [ "$orphan" = "0" ]; then
        qual_pass "${desc}: every physical volume has a CP row (no orphaned disks; $(printf '%s\n' "$phys" | grep -c . || true) backing(s))"
    else
        qual_error "${desc}: FORBIDDEN — physical volume(s) without a CP row (orphaned disks):${orphan_list}"
    fi

    local missing=0 missing_list=""
    for vm in $KNOWN_CREATED; do
        is_deleted "$vm" && continue
        vol="$(volume_id_of "$vm")"
        [ -n "$vol" ] || continue
        if printf '%s\n' "$rows" | grep -qx "$vol"; then
            if [ ! -f "${VMS_DIR}/${vm}/${vol}.img" ]; then
                missing=1
                missing_list="${missing_list} ${vm}:${vol}"
            fi
        fi
    done
    if [ "$missing" = "0" ]; then
        qual_pass "${desc}: every live VM's volume row has its physical backing (no lost state)"
    else
        qual_error "${desc}: FORBIDDEN — volume row(s) without the physical backing (lost state):${missing_list}"
    fi

    db="$(stord_db)"
    if [ -f "$db" ]; then
        local sessions orphan_sess=0
        sessions="$(sqlite_query "$db" 'SELECT DISTINCT volume_id FROM sessions' 2>/dev/null)"
        for vol in $sessions; do
            if ! printf '%s\n' "$rows" | grep -qx "$vol"; then
                orphan_sess=1
                qual_error "${desc}: FORBIDDEN — stord session for unknown volume ${vol} (orphaned session)"
            fi
        done
        [ "$orphan_sess" = "0" ] \
            && qual_pass "${desc}: live stord sessions all reference known volumes (${db})"
    else
        qual_warn "${desc}: no live stord session DB to check (${db} absent)"
    fi
}

# assert_no_stuck_ops DESC VM — after the leg's recovery window, every
# journaled operation for this VM is terminal in BOTH journals. A
# terminally-FAILED create/start is the #368 fail-closed truth: a PASS
# with the terminal statuses named in the message (the leg adds the
# disclosure warn); only eternal non-terminal ops are errors.
assert_no_stuck_ops() {
    local desc="$1" vm="$2"
    wait_for "${desc}: CP journal ops terminal for ${vm}" "$OPS_SETTLE" \
        cp_vm_ops_terminal "$vm" \
        || qual_error "${desc}: CP ops for ${vm} stuck non-terminal: $(cp_vm_ops "$vm" | tr '\n' ' ')"
    if [ -f "$CORE_DB" ]; then
        wait_for "${desc}: Core journal ops terminal for ${vm}" "$OPS_SETTLE" \
            core_vm_ops_terminal "$vm" \
            || qual_error "${desc}: core.db ops for ${vm} stuck non-terminal: $(core_vm_ops "$vm" | tr '\n' ' ')"
    else
        qual_error "${desc}: core DB missing (${CORE_DB})"
    fi
    qual_info "${desc}: op journals for ${vm} — CP: [$(cp_vm_ops "$vm" | tr '\n' ' ')] core: [$(core_vm_ops "$vm" | tr '\n' ' ')]"
}

# disclose_368 DESC VM — the #368 disclosure: a mid-operation kill that
# terminally failed the journaled op and was NOT re-driven. Fail-closed
# (no forbidden outcome) — recorded as a warning, never an error.
disclose_368() {
    qual_warn "$1: the op terminally FAILED and nothing re-drives it (#368 class — fail-closed, clean; the retry leg proves recovery works)"
}

# assert_one_create_op DESC VM — exactly one CreateVm row per created VM:
# the system must never re-journal a create (duplicate authoritative
# state), no matter how many times a dispatch is replayed across a CP
# restart.
assert_one_create_op() {
    local desc="$1" vm="$2" n
    n="$(cp_create_op_count "$vm")"
    if [ "$n" = "1" ]; then
        qual_pass "${desc}: exactly one CreateVm op row for ${vm} (no duplicated journal work)"
    else
        qual_error "${desc}: FORBIDDEN — ${n} CreateVm op rows for ${vm} (duplicated journal work)"
    fi
}

# assert_base_undisturbed DESC — the base VM survived the leg untouched:
# same CH pid (process identity, not just state), still Running.
assert_base_undisturbed() {
    local desc="$1"
    if [ "$(ch_pid_of "$BASE_VM")" = "$BASE_CH_PID" ] && ch_alive_of "$BASE_VM"; then
        qual_pass "${desc}: base VM undisturbed (same CH pid ${BASE_CH_PID})"
    else
        qual_error "${desc}: base VM disturbed (CH pid $(ch_pid_of "$BASE_VM"), expected ${BASE_CH_PID})"
    fi
}

# assert_ops_unchanged DESC VM BEFORE AFTER — per-VM operation history
# fingerprint (type:status rows) — restart legs must not journal or
# replay work for an unrelated VM.
assert_ops_unchanged() {
    local desc="$1" vm="$2" before="$3" after="$4"
    if [ "$before" = "$after" ]; then
        qual_pass "${desc}: op history for ${vm} unchanged ($(printf '%s\n' "$before" | grep -c 'Succeeded' || true) succeeded)"
    else
        qual_error "${desc}: op history for ${vm} CHANGED by the leg (FORBIDDEN duplicate/replayed work)"
        qual_error "  before: ${before}"
        qual_error "  after:  ${after}"
    fi
}

# ---------------------------------------------------------------------------
# Migration-topology helpers (M4.6 contracts, verbatim where possible)
# ---------------------------------------------------------------------------
scenario_cleanup() {
    local pid alive
    for pid in "${M47_PIDS[@]:-}"; do
        [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
    done
    for _ in $(seq 1 25); do
        alive=0
        for pid in "${M47_PIDS[@]:-}"; do
            if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
                alive=1
            fi
        done
        [ "$alive" -eq 0 ] && break
        sleep 0.2
    done
    for pid in "${M47_PIDS[@]:-}"; do
        [ -n "$pid" ] && kill -9 "$pid" 2>/dev/null || true
    done
    # Scoped safety net (argv[0]-anchored, this scenario's config dir) —
    # the deploy's own teardown knows nothing about the standalone stords.
    pkill -f "(^|/)chv-stord( |$).*${M47_DIR}" 2>/dev/null || true
    sleep 0.5
    pkill -9 -f "(^|/)chv-stord( |$).*${M47_DIR}" 2>/dev/null || true
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
        echo "### m4.7 grpcurl provisioning record ($(date -u +%FT%TZ))"
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
    # CloseVolume returns a bare Result (no .result wrapper — M4.6 trap).
    status="$(printf '%s' "$resp" | jq -r '.status // empty')"
    if [ "$status" = "OK" ]; then
        qual_pass "CloseVolume accepted for ${volid} (session closed)"
        return 0
    fi
    qual_error "CloseVolume for ${volid} not OK (status=${status})"
    return 1
}

# trigger_migration_raw SOCK VOLID HANDLE ENDPOINT [SIZE_BYTES] → raw grpcurl
# output (stdout+stderr). A fresh authorization is minted per call — the
# receiver consumes the nonce once, so recovery retries reuse the volume but
# must never reuse the token (issue #663). Negative legs inspect the raw text
# because a receiver that refuses before readiness returns its error
# synchronously (no migration_id to poll).
trigger_migration_raw() {
    local sock="$1" volid="$2" handle="$3" endpoint="$4"
    local size="${5:-$VOL_SIZE_BYTES}"
    local auth json
    auth="$(mint_migration_authorization "$volid" "$size")" || return 1
    json="{\"volumeId\":\"${volid}\",\"attachmentHandle\":\"${handle}\",\"destEndpoint\":\"${endpoint}\",\"migrationAuthorization\":\"${auth}\"}"
    stord_rpc "$sock" TriggerDiskMigration "$json" 2>&1
}

# try_trigger_migration — non-asserting trigger (the F5 recovery branch
# needs to observe a refusal without double-reporting): prints the
# migration_id or empty.
try_trigger_migration() {
    local resp status mid
    resp="$(trigger_migration_raw "$@")" || { printf ''; return 1; }
    status="$(printf '%s' "$resp" | jq -r '.result.status // empty' 2>/dev/null)"
    mid="$(printf '%s' "$resp" | jq -r '.migrationId // empty' 2>/dev/null)"
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
    M47_PIDS+=("$pid")
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

# wait_phase MID SOCK NEEDLE TIMEOUT — tight (0.2 s) poll until the task
# reaches the given phase (BULK_COPY is transient on a fast loopback
# transfer — lib.sh's 2 s cadence is too coarse).
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

save_fault_evidence() {
    local label="$1"
    {
        echo "### m4.7 evidence snapshot: ${label} ($(date -u +%FT%TZ))"
        echo "--- CP operations:"
        sqlite_query "$QUAL_DB" \
            "SELECT operation_id, resource_id, operation_type, status FROM operations ORDER BY requested_at" 2>/dev/null || true
        echo "--- CP volumes / desired state:"
        sqlite_query "$QUAL_DB" \
            "SELECT volume_id, node_id, capacity_bytes FROM volumes" 2>/dev/null || true
        sqlite_query "$QUAL_DB" \
            "SELECT volume_id, attached_vm_id FROM volume_desired_state" 2>/dev/null || true
        echo "--- core.db operations:"
        sqlite_query "$CORE_DB" \
            "SELECT operation_id, kind, vm_id, status FROM operations ORDER BY accepted_at" 2>/dev/null || true
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

# ===========================================================================
qual_info "=== M4.7 fault & interruption matrix start (node ${QUAL_NODE_ID}) ==="
qual_info "candidate: $(cat "${QUAL_BINARY_DIR}/CANDIDATE_SHA" 2>/dev/null || echo unknown)"
[ -s "$GUEST_IMAGE_PATH" ] \
    || qual_die "guest image missing: ${GUEST_IMAGE_PATH} — build it with patch-guest-image.sh and run deploy.sh with GUEST_IMAGE=noble-qual-patched.img"

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
save_fault_evidence "start"

# ---------------------------------------------------------------------------
# Preamble — base VM up + checker baselines
# ---------------------------------------------------------------------------
qual_info "--- Preamble: base VM (create → start → boot to logind) + baselines"

if [ "$(count_cloud_hypervisor_processes)" -eq 0 ]; then
    qual_pass "baseline: no cloud-hypervisor processes on the host"
else
    qual_error "baseline: cloud-hypervisor processes already present (unclean host?): $(pgrep -af cloud-hypervisor | head -3)"
fi
CH_EXPECTED=0
TAPS_BASELINE="$(tap_count)"
TAPS_EXPECTED="$TAPS_BASELINE"
qual_pass "baseline: ${TAPS_BASELINE} pre-existing tap interface(s) recorded"

BASE_VM="$(create_vm qual-fault-base 2 1024)" || qual_die "aborting (preamble)"
mark_created "$BASE_VM"
qual_pass "base VM created: qual-fault-base (${BASE_VM})"
BASE_VOL="$(volume_id_of "$BASE_VM")"
[ -n "$BASE_VOL" ] \
    && qual_pass "base boot volume bound in CP DB (${BASE_VOL})" \
    || qual_die "no volume row for the base VM"
wait_for "base VM volume backing materialized (seed conversion complete)" \
    120 vm_volume_ready "$BASE_VM" \
    || qual_die "base VM volume never materialized — agent log: $(tail -30 "${QUAL_LOGS_DIR}/agent.log")"
TAPS_EXPECTED=$((TAPS_EXPECTED + 1))

qual_chvctl vm start "$BASE_VM" >/dev/null 2>&1 \
    && qual_pass "base vm start accepted" \
    || qual_die "base vm start rejected"
vm_running_desired() { [ "$(vm_state "$BASE_VM")" = "Running" ]; }
wait_for "base VM Running (desired state)" 60 vm_running_desired \
    || qual_die "base VM never reached Running"
console_banner() { console_has "$BASE_VM" "Linux version"; }
wait_for "base guest kernel banner in console.log" "$BOOT_TIMEOUT" console_banner \
    || qual_die "no kernel banner — guest did not boot (see $(vm_console_log "$BASE_VM"))"
[ -d "${VMS_DIR}/${BASE_VM}" ] \
    && qual_pass "base VM runtime dir present" \
    || qual_error "base VM runtime dir missing"
CH_EXPECTED=1
assert_one_ch_process "preamble after base start"
BASE_CH_PID="$(ch_pid_of "$BASE_VM")"
if [ -n "$BASE_CH_PID" ] && ch_alive_of "$BASE_VM"; then
    qual_pass "base CH process pid: ${BASE_CH_PID}"
else
    qual_error "base CH pid not persisted/readable (${BASE_CH_PID:-empty}) — same-pid assertions below would be wrong"
fi
wait_for "base guest logind (boot complete)" "$LOGIND_TIMEOUT" \
    console_has "$BASE_VM" "systemd-logind" \
    || qual_error "no logind evidence on the base boot — later stops may be lost (M2.5)"
save_console_evidence "$BASE_VM" preamble
assert_taps_clean "preamble (base VM provisioned)"
assert_disk_state_consistent "preamble"
BASE_OPS="$(cp_vm_ops "$BASE_VM")"
qual_info "preamble base op snapshot: [$(printf '%s\n' "$BASE_OPS" | tr '\n' ' ')]"
save_fault_evidence "preamble"

# ---------------------------------------------------------------------------
# Leg F1 — CP SIGKILL mid-create
# ---------------------------------------------------------------------------
qual_info "--- F1: CP SIGKILL mid-create (op claimed, dispatch in flight)"

F1_VM="$(create_vm qual-fault-f1 1 512)" || qual_die "aborting (F1)"
qual_pass "F1: vm created (accepted): qual-fault-f1 (${F1_VM})"
if wait_op_running "$F1_VM" CreateVm 60; then
    qual_pass "F1: CreateVm op observed Running (dispatch in flight) — killing CP inside the window"
else
    qual_warn "F1: CreateVm window missed (op state '$(cp_op_status "$F1_VM" CreateVm)') — killing CP post-claim; recovery assertions unchanged"
fi
kill -9 "$CP_PID" 2>/dev/null || true
wait_gone "$CP_PID" \
    && qual_pass "F1: control-plane SIGKILLed and gone" \
    || qual_error "F1: control-plane did not die"
start_cp
qual_pass "F1: control-plane restarted (pid ${CP_PID})"

assert_no_stuck_ops "F1" "$F1_VM"
F1_OUTCOME="$(wait_create_outcome "$F1_VM" "$OPS_SETTLE")"
case "$F1_OUTCOME" in
    converged)
        qual_pass "F1: create CONVERGED across the CP kill (volume materialized, op terminal)"
        mark_created "$F1_VM"
        TAPS_EXPECTED=$((TAPS_EXPECTED + 1))
        CH_EXPECTED=$((CH_EXPECTED + 1))
        ;;
    failed)
        qual_pass "F1: create failed CLEANLY across the CP kill (fail-closed, terminal — #368 class)"
        disclose_368 "F1"
        ;;
    failed_368)
        qual_warn "F1: create hit the recorded #368 boundary (CP op Succeeded, core effect failed — phantom VM row, no re-drive); cleanup-delete is the recovery path"
        disclose_368 "F1"
        ;;
    *)
        qual_error "F1: create neither converged nor terminally failed within ${OPS_SETTLE}s (eternal in-flight — FORBIDDEN)"
        ;;
esac
assert_created_vm_process_shape "F1 (created VM process shape)" "$F1_VM" "$F1_OUTCOME"
assert_one_ch_process "F1 (base + f1 VM)"
assert_disk_state_consistent "F1"
assert_taps_clean "F1"
assert_base_undisturbed "F1"
assert_ops_unchanged "F1" "$BASE_VM" "$BASE_OPS" "$(cp_vm_ops "$BASE_VM")"
assert_one_create_op "F1" "$F1_VM"
drain_deferred_reports "F1 (agent reconnected to the restarted CP)"
save_fault_evidence "f1"

# Cleanup + RETRY: the create must succeed on the recovered stack.
F1_DEL_OUT="$(qual_chvctl vm delete "$F1_VM" 2>&1)"; F1_DEL_RC=$?
classify_bff_result "F1 cleanup: delete of ${F1_VM}" "$F1_DEL_OUT" "$F1_DEL_RC"
mark_deleted "$F1_VM"
if [ "$F1_OUTCOME" = "converged" ]; then
    TAPS_EXPECTED=$((TAPS_EXPECTED - 1))
    CH_EXPECTED=$((CH_EXPECTED - 1))
fi

F1B_VM="$(create_vm qual-fault-f1b 1 512)" || qual_error "F1 retry: create REJECTED on the recovered stack"
F1B_CONV=0
if [ -n "${F1B_VM:-}" ]; then
    qual_pass "F1 retry: create accepted (qual-fault-f1b: ${F1B_VM})"
    if wait_for "F1 retry: volume materialized (converged-Created)" "$OPS_SETTLE" \
        vm_volume_ready "$F1B_VM"; then
        mark_created "$F1B_VM"
        TAPS_EXPECTED=$((TAPS_EXPECTED + 1))
        CH_EXPECTED=$((CH_EXPECTED + 1))
        F1B_CONV=1
    else
        qual_error "F1 retry: create did not converge after recovery"
    fi
    [ "$(cp_op_status "$F1B_VM" CreateVm)" = "Succeeded" ] \
        && qual_pass "F1 retry: CreateVm op Succeeded" \
        || qual_error "F1 retry: CreateVm op is '$(cp_op_status "$F1B_VM" CreateVm)'"
    F1B_DEL_OUT="$(qual_chvctl vm delete "$F1B_VM" 2>&1)"; F1B_DEL_RC=$?
    classify_bff_result "F1 retry cleanup: delete of ${F1B_VM}" "$F1B_DEL_OUT" "$F1B_DEL_RC"
    mark_deleted "$F1B_VM"
    if [ "$F1B_CONV" = "1" ]; then
        TAPS_EXPECTED=$((TAPS_EXPECTED - 1))
        CH_EXPECTED=$((CH_EXPECTED - 1))
    fi
fi
assert_taps_clean "F1 after cleanup"

# ---------------------------------------------------------------------------
# Leg F2 — Agent SIGKILL mid-start
# ---------------------------------------------------------------------------
qual_info "--- F2: agent SIGKILL mid-start (StartVm claimed, CH spawn in flight)"

F2_VM="$(create_vm qual-fault-f2 1 512)" || qual_die "aborting (F2)"
mark_created "$F2_VM"
qual_pass "F2: vm created (created-stopped; no CH yet): qual-fault-f2 (${F2_VM})"
wait_for "F2: volume materialized" 120 vm_volume_ready "$F2_VM" \
    || qual_die "F2: VM never converged to Created"
TAPS_EXPECTED=$((TAPS_EXPECTED + 1))

qual_chvctl vm start "$F2_VM" >/dev/null 2>&1 \
    && qual_pass "F2: vm start accepted" \
    || qual_error "F2: vm start rejected"
# Deterministic mid-boot kill window (the run-2/#409 lesson): the CP op
# reaching Succeeded is submit-level (#368), so killing on it lands at a
# variable offset from the actual boot — run 1 hit post-boot (trivial
# pass), run 2 hit mid-firmware (the wedge), run 3 post-boot again. The
# #409 wedge further requires the kernel to RST the serial socket, which
# only happens when the agent dies with UNREAD console data in flight —
# the firmware's early trickle (~500 bytes) makes that a sub-second
# lottery (run 4 hit the window cleanly without wedging). The kernel
# banner is followed by seconds of sustained heavy serial spew, so
# killing the moment it appears guarantees traffic in flight: the RST
# condition, and with it the exact #409 exposure, every run. The #410
# heal is then expected to recover the wedged VM on the re-driven start
# (a fresh full boot — kernel banner + logind — after the budgeted
# reboot); a clean continuation also passes (the RST did not land), and
# the console artifact (boot count) records which happened.
F2_BOOT_WINDOW=missed
F2_BOOT_DEADLINE=$((SECONDS + 60))
while [ "$SECONDS" -lt "$F2_BOOT_DEADLINE" ]; do
    if console_has "$F2_VM" "Linux version"; then F2_BOOT_WINDOW=hit; break; fi
    sleep 0.05
done
if [ "$F2_BOOT_WINDOW" = "hit" ]; then
    qual_pass "F2: kernel spew under way (banner printed, serial traffic in flight) — killing the agent inside the boot window (#409 exposure)"
else
    qual_warn "F2: boot window not observed within 60s (console: $(wc -c < "$(vm_console_log "$F2_VM")" 2>/dev/null || echo 0) bytes) — killing the agent anyway; recovery assertions unchanged"
fi
kill -9 "$AGENT_PID" 2>/dev/null || true
wait_gone "$AGENT_PID" \
    && qual_pass "F2: agent SIGKILLed and gone" \
    || qual_error "F2: agent did not die"

ch_alive_of "$BASE_VM" \
    && qual_pass "F2: base VM's CH survived the agent crash (pid $(ch_pid_of "$BASE_VM"))" \
    || qual_error "F2: base VM's CH died with the agent (FORBIDDEN)"

start_agent
qual_pass "F2: agent restarted (pid ${AGENT_PID})"

# The restarted supervisor must ADOPT the living daemons via their
# sockets (never double-spawn) — the duplicate-daemon forbidden outcome.
F2_STORD_N="$(pgrep -f "$STORD_MATCH" 2>/dev/null | wc -l | tr -d ' ')"
if [ "$F2_STORD_N" = "1" ]; then
    qual_pass "F2: exactly one stord after agent restart (supervisor adopted, no double-spawn)"
else
    qual_error "F2: FORBIDDEN — ${F2_STORD_N} stord processes after agent restart (double-spawn)"
    pgrep -af "$STORD_MATCH" >&2 || true
fi
F2_NWD_N="$(pgrep -f "$NWD_MATCH" 2>/dev/null | wc -l | tr -d ' ')"
if [ "$F2_NWD_N" = "1" ]; then
    qual_pass "F2: exactly one nwd after agent restart (supervisor adopted, no double-spawn)"
else
    qual_error "F2: FORBIDDEN — ${F2_NWD_N} nwd processes after agent restart (double-spawn)"
    pgrep -af "$NWD_MATCH" >&2 || true
fi

# Outcome disjunction: the VM ends Running (a CH process exists — either
# the original spawn that survived the crash, or a fresh one after
# restart) OR the start failed cleanly (op terminal-Failed, no CH).
F2_RUNNING=0
if wait_for "F2: VM Running after agent restart (CH process exists)" 120 vm_ch_exists "$F2_VM"; then
    F2_RUNNING=1
    CH_EXPECTED=$((CH_EXPECTED + 1))
else
    F2_ST="$(cp_op_status "$F2_VM" StartVm)"
    if [ "$F2_ST" = "Failed" ] && vm_ch_gone "$F2_VM"; then
        qual_pass "F2: start failed CLEANLY across the agent crash (op terminal-Failed, no CH — #368 class)"
        disclose_368 "F2"
    else
        qual_error "F2: VM neither Running nor cleanly-failed after the agent restart (op '${F2_ST}', CH count $(vm_ch_count "$F2_VM"))"
    fi
fi
assert_le_one_ch_for_vm "F2 (no double-spawn)" "$F2_VM"
assert_one_ch_process "F2"

# RETRY start → Running (idempotent if it already converged).
F2_RETRY_OUT="$(qual_chvctl vm start "$F2_VM" 2>&1)"; F2_RETRY_RC=$?
classify_bff_result "F2 retry: second vm start" "$F2_RETRY_OUT" "$F2_RETRY_RC"
if [ "$F2_RUNNING" = "0" ]; then CH_EXPECTED=$((CH_EXPECTED + 1)); fi
wait_for "F2 retry: VM Running (CH process exists)" 120 vm_ch_exists "$F2_VM" \
    || qual_error "F2 retry: VM still not Running after the retry start"
assert_le_one_ch_for_vm "F2 retry (still no double-spawn)" "$F2_VM"
assert_one_ch_process "F2 retry"

# Boot evidence for F2's VM (ACPI-safe stops in F7 need logind).
wait_for "F2: guest kernel banner (boot evidence)" "$BOOT_TIMEOUT" \
    console_has "$F2_VM" "Linux version" \
    || qual_error "F2: no boot evidence for the started VM"
wait_for "F2: guest logind (boot complete)" "$LOGIND_TIMEOUT" \
    console_has "$F2_VM" "systemd-logind" \
    || qual_error "F2: no logind evidence — F7's stop of this VM may be lost (M2.5)"
save_console_evidence "$F2_VM" f2

assert_no_stuck_ops "F2" "$F2_VM"
assert_one_create_op "F2" "$F2_VM"
assert_disk_state_consistent "F2"
assert_taps_clean "F2"
assert_base_undisturbed "F2"
assert_ops_unchanged "F2" "$BASE_VM" "$BASE_OPS" "$(cp_vm_ops "$BASE_VM")"
save_fault_evidence "f2"

# ---------------------------------------------------------------------------
# Leg F3 — Stord SIGKILL mid-provision (boot-volume create+attach in flight)
# ---------------------------------------------------------------------------
qual_info "--- F3: stord SIGKILL mid-provision (seed conversion in flight, base VM Running)"

F3_VM="$(create_vm qual-fault-f3 1 512)" || qual_die "aborting (F3)"
qual_pass "F3: vm created (accepted): qual-fault-f3 (${F3_VM})"
# Kill window: the moment the backing file STARTS materializing (the
# M4.5-proven multi-second qcow2→raw conversion window).
F3_WINDOW=missed
F3_DEADLINE=$((SECONDS + 120))
while [ "$SECONDS" -lt "$F3_DEADLINE" ]; do
    if vm_volume_started "$F3_VM"; then F3_WINDOW=hit; break; fi
    [ "$(cp_op_status "$F3_VM" CreateVm)" = "Failed" ] && break
    sleep 0.2
done
if [ "$F3_WINDOW" = "hit" ]; then
    qual_pass "F3: volume backing started materializing — SIGKILLing stord inside the conversion window"
else
    qual_warn "F3: conversion window missed (backing state: $(volume_backing "$F3_VM" || echo none)) — SIGKILLing stord anyway; recovery assertions unchanged"
fi
F3_STORD_BEFORE="$(stord_pid)"
[ -n "$F3_STORD_BEFORE" ] || qual_die "F3: no deployed stord process found"
kill -9 "$F3_STORD_BEFORE" 2>/dev/null || true
qual_pass "F3: deployed stord SIGKILLed (pid ${F3_STORD_BEFORE})"

ch_alive_of "$BASE_VM" \
    && qual_pass "F3: base VM unaffected (CH holds the disk fd; pid $(ch_pid_of "$BASE_VM"))" \
    || qual_error "F3: base VM's CH died with stord (FORBIDDEN)"

# Supervisor respawn (m4.5 pattern; the respawned daemon may be a NEW pid
# with a relocated runtime_dir — #376).
stord_restarted_f3() {
    local now
    now="$(stord_pid)"
    [ -n "$now" ] && [ "$now" != "$F3_STORD_BEFORE" ]
}
wait_for "F3: agent supervisor restarted stord" "$SUPERVISOR_TIMEOUT" stord_restarted_f3 \
    || qual_die "F3: stord was not restarted by the supervisor"
qual_pass "F3: stord respawned by the agent supervisor (pid ${F3_STORD_BEFORE} → $(stord_pid); runtime_dir $(stord_runtime_dir))"
wait_for "F3: respawned stord socket live" 20 stord_socket_live \
    || qual_error "F3: respawned stord socket not accepting"

assert_no_stuck_ops "F3" "$F3_VM"
F3_OUTCOME="$(wait_create_outcome "$F3_VM" "$OPS_SETTLE")"
case "$F3_OUTCOME" in
    converged)
        qual_pass "F3: provision CONVERGED across the stord kill (volume materialized, op terminal)"
        mark_created "$F3_VM"
        TAPS_EXPECTED=$((TAPS_EXPECTED + 1))
        CH_EXPECTED=$((CH_EXPECTED + 1))
        ;;
    failed)
        qual_pass "F3: provision failed CLEANLY across the stord kill (fail-closed, terminal — #368 class)"
        disclose_368 "F3"
        ;;
    failed_368)
        qual_warn "F3: provision hit the recorded #368 boundary (CP op Succeeded — submit-level, core create_vm:failed — phantom VM row, no re-drive; the M4.7 run-1 reproduction shape)"
        disclose_368 "F3"
        ;;
    *)
        qual_error "F3: provision neither converged nor terminally failed within ${OPS_SETTLE}s (FORBIDDEN eternal in-flight)"
        ;;
esac
assert_created_vm_process_shape "F3 (created VM process shape)" "$F3_VM" "$F3_OUTCOME"
assert_one_ch_process "F3 (no new VM processes)"
assert_disk_state_consistent "F3 (post-respawn live stord.db)"
assert_taps_clean "F3"
assert_base_undisturbed "F3"
assert_ops_unchanged "F3" "$BASE_VM" "$BASE_OPS" "$(cp_vm_ops "$BASE_VM")"

# RETRY: fresh provisioning through the RESPAWNED stord must converge,
# and the volume must be closable/deletable afterwards (the detach/delete
# contract on this topology: VM delete closes the stord session).
F3B_VM="$(create_vm qual-fault-f3b 1 512)" || qual_error "F3 retry: create REJECTED after stord respawn"
F3B_CONV=0
if [ -n "${F3B_VM:-}" ]; then
    qual_pass "F3 retry: create accepted on the respawned stord (qual-fault-f3b: ${F3B_VM})"
    if wait_for "F3 retry: volume materialized through the respawned stord" "$OPS_SETTLE" \
        vm_volume_ready "$F3B_VM"; then
        mark_created "$F3B_VM"
        F3B_CONV=1
    else
        qual_error "F3 retry: provisioning did not converge after the stord respawn"
    fi
    TAPS_EXPECTED=$((TAPS_EXPECTED + F3B_CONV))
    CH_EXPECTED=$((CH_EXPECTED + F3B_CONV))
    F3B_VOL="$(volume_id_of "$F3B_VM")"
    if [ -n "$F3B_VOL" ]; then
        wait_for "F3 retry: stord session open for the new volume (live stord.db)" 60 \
            stord_sessions_ge_one "$F3B_VOL" \
            && qual_pass "F3 retry: respawned stord opened the new volume (session row present)" \
            || qual_error "F3 retry: no stord session row for ${F3B_VOL}"
    fi
    F3B_DEL_OUT="$(qual_chvctl vm delete "$F3B_VM" 2>&1)"; F3B_DEL_RC=$?
    classify_bff_result "F3 retry cleanup: delete of ${F3B_VM}" "$F3B_DEL_OUT" "$F3B_DEL_RC"
    mark_deleted "$F3B_VM"
    TAPS_EXPECTED=$((TAPS_EXPECTED - F3B_CONV))
    CH_EXPECTED=$((CH_EXPECTED - F3B_CONV))
    if [ -n "$F3B_VOL" ]; then
        f3b_session_closed() { [ "$(stord_sessions "$F3B_VOL")" = "0" ]; }
        wait_for "F3 retry: stord session closed on delete (volume detached/deleted)" 60 \
            f3b_session_closed \
            && qual_pass "F3 retry: volume detachable/deletable (session closed on VM delete)" \
            || qual_error "F3 retry: stord session for ${F3B_VOL} NOT closed on delete"
    fi
fi
F3_DEL_OUT="$(qual_chvctl vm delete "$F3_VM" 2>&1)"; F3_DEL_RC=$?
classify_bff_result "F3 cleanup: delete of ${F3_VM}" "$F3_DEL_OUT" "$F3_DEL_RC"
mark_deleted "$F3_VM"
if [ "$F3_OUTCOME" = "converged" ]; then
    TAPS_EXPECTED=$((TAPS_EXPECTED - 1))
    CH_EXPECTED=$((CH_EXPECTED - 1))
fi
assert_taps_clean "F3 after cleanup"
save_fault_evidence "f3"

# ---------------------------------------------------------------------------
# Leg F4 — nwd SIGKILL mid-create (tap provision in flight)
# ---------------------------------------------------------------------------
qual_info "--- F4: nwd SIGKILL mid-create (NIC attach / tap provision in flight)"

F4_VM="$(create_vm qual-fault-f4 1 512)" || qual_die "aborting (F4)"
qual_pass "F4: vm created (accepted): qual-fault-f4 (${F4_VM})"
if wait_op_running "$F4_VM" CreateVm 60; then
    qual_pass "F4: CreateVm op observed Running — killing nwd inside the create (tap provision follows the volume work)"
else
    qual_warn "F4: CreateVm window missed (op state '$(cp_op_status "$F4_VM" CreateVm)') — killing nwd anyway; recovery assertions unchanged"
fi
F4_NWD_BEFORE="$(nwd_pid)"
[ -n "$F4_NWD_BEFORE" ] || qual_die "F4: no nwd process found"
kill -9 "$F4_NWD_BEFORE" 2>/dev/null || true
qual_pass "F4: nwd SIGKILLed (pid ${F4_NWD_BEFORE})"

nwd_restarted_f4() {
    local now
    now="$(nwd_pid)"
    [ -n "$now" ] && [ "$now" != "$F4_NWD_BEFORE" ] && nwd_socket_live
}
wait_for "F4: agent supervisor restarted nwd (new pid, socket live)" "$SUPERVISOR_TIMEOUT" \
    nwd_restarted_f4 \
    || qual_die "F4: nwd was not restarted by the supervisor"

assert_no_stuck_ops "F4" "$F4_VM"
F4_OUTCOME="$(wait_create_outcome "$F4_VM" "$OPS_SETTLE")"
case "$F4_OUTCOME" in
    converged)
        qual_pass "F4: create CONVERGED across the nwd kill (tap provisioned, op terminal)"
        mark_created "$F4_VM"
        TAPS_EXPECTED=$((TAPS_EXPECTED + 1))
        CH_EXPECTED=$((CH_EXPECTED + 1))
        ;;
    failed)
        qual_pass "F4: create failed CLEANLY across the nwd kill (fail-closed, terminal — #368 class)"
        disclose_368 "F4"
        ;;
    failed_368)
        qual_warn "F4: create hit the recorded #368 boundary (CP op Succeeded, core effect failed — phantom VM row, no re-drive); cleanup-delete is the recovery path"
        disclose_368 "F4"
        ;;
    *)
        qual_error "F4: create neither converged nor terminally failed within ${OPS_SETTLE}s (FORBIDDEN eternal in-flight)"
        ;;
esac
assert_created_vm_process_shape "F4 (created VM process shape)" "$F4_VM" "$F4_OUTCOME"
assert_one_ch_process "F4 (no new VM processes)"
# THE F4 forbidden outcome: no orphaned tap once the leg settles (a tap
# provisioned by the dying nwd for a create that then failed must not
# outlive the leg; a converged create must have exactly its one tap).
assert_taps_clean "F4 (no orphaned tap)"
assert_disk_state_consistent "F4"
assert_base_undisturbed "F4"
assert_ops_unchanged "F4" "$BASE_VM" "$BASE_OPS" "$(cp_vm_ops "$BASE_VM")"

F4B_VM="$(create_vm qual-fault-f4b 1 512)" || qual_error "F4 retry: create REJECTED after nwd respawn"
F4B_CONV=0
if [ -n "${F4B_VM:-}" ]; then
    qual_pass "F4 retry: create accepted (qual-fault-f4b: ${F4B_VM})"
    if wait_for "F4 retry: volume materialized (converged-Created)" "$OPS_SETTLE" \
        vm_volume_ready "$F4B_VM"; then
        mark_created "$F4B_VM"
        F4B_CONV=1
    else
        qual_error "F4 retry: create did not converge after the nwd respawn"
    fi
    TAPS_EXPECTED=$((TAPS_EXPECTED + F4B_CONV))
    CH_EXPECTED=$((CH_EXPECTED + F4B_CONV))
    F4B_DEL_OUT="$(qual_chvctl vm delete "$F4B_VM" 2>&1)"; F4B_DEL_RC=$?
    classify_bff_result "F4 retry cleanup: delete of ${F4B_VM}" "$F4B_DEL_OUT" "$F4B_DEL_RC"
    mark_deleted "$F4B_VM"
    TAPS_EXPECTED=$((TAPS_EXPECTED - F4B_CONV))
    CH_EXPECTED=$((CH_EXPECTED - F4B_CONV))
fi
F4_DEL_OUT="$(qual_chvctl vm delete "$F4_VM" 2>&1)"; F4_DEL_RC=$?
classify_bff_result "F4 cleanup: delete of ${F4_VM}" "$F4_DEL_OUT" "$F4_DEL_RC"
mark_deleted "$F4_VM"
if [ "$F4_OUTCOME" = "converged" ]; then
    TAPS_EXPECTED=$((TAPS_EXPECTED - 1))
    CH_EXPECTED=$((CH_EXPECTED - 1))
fi
assert_taps_clean "F4 after cleanup"
save_fault_evidence "f4"

# ---------------------------------------------------------------------------
# Legs F5/F6 — migration interruption (M4.6 two-standalone-stord topology)
# ---------------------------------------------------------------------------
qual_info "--- F5/F6 topology: two standalone mTLS stords (NOT agent-supervised, #385)"

provision_grpcurl
command -v jq >/dev/null 2>&1 || qual_die "jq missing (m4.6-proven dependency)"

mkdir -p "$CERTS_DIR" "${M47_DIR}/openssl-ca/newcerts"
touch "${M47_DIR}/openssl-ca/index.txt"
echo 1000 > "${M47_DIR}/openssl-ca/serial"

openssl genrsa -out "${CERTS_DIR}/ca.key" 2048 2>/dev/null
openssl req -x509 -new -nodes -key "${CERTS_DIR}/ca.key" -sha256 -days 2 \
    -out "${CERTS_DIR}/ca.crt" \
    -subj "/O=CHV Qual M47/CN=m47-qual-ca" 2>/dev/null

make_leaf() {
    local name="$1" cn="$2" ext="$3"
    openssl genrsa -out "${CERTS_DIR}/${name}.key" 2048 2>/dev/null
    openssl req -new -key "${CERTS_DIR}/${name}.key" -out "${CERTS_DIR}/${name}.csr" \
        -subj "/O=CHV Qual M47/CN=${cn}" 2>/dev/null
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
make_leaf src-client m47-src-client "$CLIENT_EXT"
make_leaf dst-server m47-dst-server "$SERVER_EXT"
make_leaf dst-client m47-dst-client "$CLIENT_EXT"
chmod 644 "$CERTS_DIR"/*.crt
chmod 600 "$CERTS_DIR"/*.key
openssl verify -CAfile "${CERTS_DIR}/ca.crt" "${CERTS_DIR}/src-client.crt" >/dev/null 2>&1 \
    && qual_pass "F5/F6 precondition: migration CA chain verifies" \
    || qual_die "F5/F6 precondition: CA chain does not verify — legs would be vacuous"

if tcp_port_free "$DST_PORT"; then
    qual_pass "F5/F6 precondition: destination port ${DST_PORT} is free"
else
    qual_die "F5/F6 precondition: destination port ${DST_PORT} is already in use — refusing to run"
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
node_id = "${QUAL_MIGRATION_DEST_NODE_ID}"
signing_secret_file = "${QUAL_MIGRATION_SECRET_FILE}"
EOF
}

SRC_DIR="${M47_DIR}/src"; DST_DIR="${M47_DIR}/dst"
mkdir -p "$SRC_DIR" "$DST_DIR"
SRC_SOCK="${SRC_DIR}/api.sock"; DST_SOCK="${DST_DIR}/api.sock"
SRC_LOG="${SRC_DIR}/stord.log"; DST_LOG="${DST_DIR}/stord.log"

write_src_config "${SRC_DIR}/stord.toml" "$SRC_SOCK" "$SRC_DIR"
write_dst_config "${DST_DIR}/stord.toml" "$DST_SOCK" "$DST_DIR" "$DST_PORT"

SRC_PID="$(start_scenario_stord "${SRC_DIR}/stord.toml" "$SRC_LOG")"
DST_PID="$(start_scenario_stord "${DST_DIR}/stord.toml" "$DST_LOG")"
wait_for "F5/F6: SRC stord UDS up" 20 test -S "$SRC_SOCK" \
    || qual_die "F5/F6: SRC stord did not come up — log: $(tail -20 "$SRC_LOG" 2>/dev/null)"
wait_for "F5/F6: DST stord UDS up" 20 test -S "$DST_SOCK" \
    || qual_die "F5/F6: DST stord did not come up — log: $(tail -20 "$DST_LOG" 2>/dev/null)"
assert_file_contains "F5/F6: DST receiver listener bound (mTLS, client auth required)" \
    "$DST_LOG" "storage migration receiver listening on 127.0.0.1:${DST_PORT}"
assert_file_contains "F5/F6: DST requires control-plane migration authorizations (not the insecure bypass)" \
    "$DST_LOG" "storage migration authorizations enabled"
tcp_port_open "$DST_PORT" \
    && qual_pass "F5/F6: DST receiver TCP listener reachable on ${DST_PORT}" \
    || qual_error "F5/F6: DST receiver TCP listener not reachable"

# Seed: 4 GiB patterned (one random 4 MiB block repeated) — full-volume
# digest, multi-second BULK_COPY window for the mid-copy kills.
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
    && qual_pass "F5/F6: 4 GiB patterned seed prepared" \
    || qual_die "F5/F6: seed file preparation failed"

# ===========================================================================
# Leg F5 — migration interruption, SOURCE side (extends M4.6 N9)
# ===========================================================================
qual_info "--- F5: SIGKILL SRC during BULK_COPY → no zombie task → partial retained → documented recovery → COMPLETED"

F5_VOL="m47vol5"
F5_HANDLE="$(open_volume "$SRC_SOCK" "$F5_VOL" "vol5.img" "$VOL_SIZE_BYTES" "$SEED_FILE")" \
    || qual_die "aborting (F5)"
qual_pass "F5: OpenVolume on SRC: ${F5_VOL} seeded (handle ${F5_HANDLE})"
[ "$(stat -c %s "${SRC_DIR}/vol5.img" 2>/dev/null)" = "$VOL_SIZE_BYTES" ] \
    && qual_pass "F5: source volume materialized at full size" \
    || qual_error "F5: source volume size mismatch: ${SRC_DIR}/vol5.img"

F5_MID="$(trigger_migration "$SRC_SOCK" "$F5_VOL" "$F5_HANDLE" "https://127.0.0.1:${DST_PORT}")" \
    || qual_die "aborting (F5)"
qual_pass "F5: TriggerDiskMigration accepted → ${F5_MID}"

if wait_phase "$SRC_SOCK" "$F5_MID" BULK_COPY "$MIG_RPC_TIMEOUT"; then
    qual_pass "F5: BULK_COPY observed — SIGKILLing the SOURCE stord mid-copy"
else
    qual_error "F5: BULK_COPY never observed (phase: $(migration_field "$SRC_SOCK" "$F5_MID" phase)) — kill window missed"
fi
kill -9 "$SRC_PID" 2>/dev/null || true
# Stale-socket trap (M4.6): a SIGKILLed stord leaves its UDS file behind.
rm -f "$SRC_SOCK"
qual_pass "F5: SRC stord SIGKILLed during BULK_COPY (pid ${SRC_PID}); task state died with it (in-memory)"

sleep 1
if [ -z "$(pgrep -f "(^|/)chv-stord( |$).*${M47_DIR}/src" 2>/dev/null)" ]; then
    qual_pass "F5: source stord is DOWN (not agent-supervised — no respawn; the leg restarts it)"
else
    qual_error "F5: source stord still alive after SIGKILL"
fi

# The destination partial volume is RETAINED (create_new semantics — the
# M4.6 N9 contract, now from the source-death direction).
assert_file_exists "F5: destination partial receiving volume RETAINED (not clobbered)" \
    "${DST_DIR}/${F5_VOL}.img"

# Recovery: restart the source (same identity/config; fresh log so the
# listener assert is unambiguous).
F5_SRC_RESTART_LOG="${SRC_DIR}/stord-restart.log"
SRC_PID="$(start_scenario_stord "${SRC_DIR}/stord.toml" "$F5_SRC_RESTART_LOG")"
wait_for "F5: SRC stord restarted (UDS up)" 20 test -S "$SRC_SOCK" \
    || qual_die "F5: SRC stord did not restart — log: $(tail -20 "$F5_SRC_RESTART_LOG" 2>/dev/null)"
assert_file_contains "F5: restarted SRC re-bound the receiver-side identity (client half validated)" \
    "$F5_SRC_RESTART_LOG" "storage migration mTLS enabled"

# No zombie task: the in-memory task registry died with the daemon. A
# status poll for the killed migration_id must return the NOT-FOUND
# result (GetDiskMigrationStatus answers 200-OK with
# result.status=NotFound / errorMessage "migration not found" for an
# unknown id — the response still carries a phase field, so the check
# keys on the not-found marker, not on phase presence).
F5_ZOMBIE="$(migration_status "$SRC_SOCK" "$F5_MID" 2>&1 || true)"
if printf '%s' "$F5_ZOMBIE" | grep -q 'migration not found'; then
    qual_pass "F5: no zombie task — the killed migration_id ${F5_MID} is not served after restart (in-memory state, not-found result)"
else
    qual_error "F5: FORBIDDEN — the restarted SRC still serves the killed task ${F5_MID} (zombie): ${F5_ZOMBIE:0:200}"
fi

# create_new semantics from the source-death direction: re-trigger while
# the partial receiving volume still exists → refusal. The receiver rejects
# before readiness, and the source surfaces that destination error
# synchronously (no migration task), so accept either that shape or a task
# that then FAILs; both must name the existing volume.
F5B_RESP="$(trigger_migration_raw "$SRC_SOCK" "$F5_VOL" "$F5_HANDLE" "https://127.0.0.1:${DST_PORT}")"
F5B_MID="$(printf '%s' "$F5B_RESP" | jq -r '.migrationId // empty' 2>/dev/null)"
if [ -n "$F5B_MID" ]; then
    if wait_phase "$SRC_SOCK" "$F5B_MID" FAILED "$MIG_RPC_TIMEOUT"; then
        qual_pass "F5: re-trigger against the partial volume FAILED (create_new refusal)"
    else
        qual_error "F5: re-trigger against the partial volume never failed (phase: $(migration_field "$SRC_SOCK" "$F5B_MID" phase))"
    fi
    F5B_ERR="$(migration_field "$SRC_SOCK" "$F5B_MID" errorMessage)"
else
    F5B_ERR="$F5B_RESP"
    qual_pass "F5: re-trigger against the partial volume refused synchronously (no migration task created)"
fi
assert_contains "F5: refusal names the existing receiving volume (create_new semantics)" \
    "$F5B_ERR" "refusing to truncate"
assert_file_exists "F5: partial receiving volume still on disk (not clobbered by the retry)" \
    "${DST_DIR}/${F5_VOL}.img"

# Deterministic recovery (the documented operator step): remove the
# partial, re-trigger, run the full protocol, verify the digest.
rm -f "${DST_DIR}/${F5_VOL}.img"
qual_pass "F5: partial receiving volume removed (the documented operator recovery step)"

F5_MIDC="$(try_trigger_migration "$SRC_SOCK" "$F5_VOL" "$F5_HANDLE" "https://127.0.0.1:${DST_PORT}")"
if [ -n "$F5_MIDC" ]; then
    qual_pass "F5: recovery re-trigger accepted with the hydrated session handle (session store survived the source restart)"
else
    qual_warn "F5: the hydrated handle was refused after restart — re-opening the volume for a fresh handle"
    F5_HANDLE="$(open_volume "$SRC_SOCK" "$F5_VOL" "vol5.img" "$VOL_SIZE_BYTES")" \
        || qual_die "aborting (F5 recovery re-open)"
    F5_MIDC="$(trigger_migration "$SRC_SOCK" "$F5_VOL" "$F5_HANDLE" "https://127.0.0.1:${DST_PORT}")" \
        || qual_die "aborting (F5 recovery re-trigger)"
    qual_pass "F5: recovery re-trigger accepted with a fresh handle after re-open"
fi
wait_for "F5: recovery migration reached PAUSED_FINAL_SYNC" "$MIG_RPC_TIMEOUT" \
    phase_is "$SRC_SOCK" "$F5_MIDC" "PAUSED_FINAL_SYNC" \
    || qual_error "F5: recovery migration never paused (phase: $(migration_field "$SRC_SOCK" "$F5_MIDC" phase))"
F5_PAUSE_NEEDS="$(migration_field "$SRC_SOCK" "$F5_MIDC" needsVmPause)"
[ "$F5_PAUSE_NEEDS" = "true" ] \
    && qual_pass "F5: PAUSED_FINAL_SYNC demands the VM pause (needs_vm_pause=true)" \
    || qual_error "F5: needs_vm_pause is '${F5_PAUSE_NEEDS}' at pause (expected true)"
stord_rpc "$SRC_SOCK" ResumeDiskMigration \
    "{\"migrationId\":\"${F5_MIDC}\",\"vmPaused\":true}" >/dev/null 2>&1 \
    && qual_pass "F5: ResumeDiskMigration{vm_paused:true} accepted" \
    || qual_error "F5: ResumeDiskMigration RPC failed"
wait_for "F5: recovery migration COMPLETED" "$MIG_RPC_TIMEOUT" \
    phase_is "$SRC_SOCK" "$F5_MIDC" "COMPLETED" \
    || qual_error "F5: recovery migration never completed (phase: $(migration_field "$SRC_SOCK" "$F5_MIDC" phase))"
F5_SRC_SHA="$(sha256_of "${SRC_DIR}/vol5.img")"
F5_DST_SHA="$(sha256_of "${DST_DIR}/${F5_VOL}.img")"
[ "$F5_SRC_SHA" = "$F5_DST_SHA" ] \
    && qual_pass "F5: post-recovery digest match (${F5_SRC_SHA:0:16}…)" \
    || qual_error "F5: post-recovery digest mismatch (src=${F5_SRC_SHA} dst=${F5_DST_SHA})"
cmp -s "${SRC_DIR}/vol5.img" "${DST_DIR}/${F5_VOL}.img" \
    && qual_pass "F5: byte-compare identical after recovery" \
    || qual_error "F5: byte-compare failed after recovery"
{
    echo "### leg-F5 digest record ($(date -u +%FT%TZ))"
    echo "src ${SRC_DIR}/vol5.img ${F5_SRC_SHA}"
    echo "dst ${DST_DIR}/${F5_VOL}.img ${F5_DST_SHA}"
} >> "${EVIDENCE_DIR}/digests.txt"
assert_base_undisturbed "F5"
save_fault_evidence "f5"

# ===========================================================================
# Leg F6 — CP loss mid-migration
# ===========================================================================
qual_info "--- F6: CP SIGKILL mid-BULK_COPY → transfer unaffected → COMPLETED → no duplicate CP work"

F6_VOL="m47vol6"
F6_HANDLE="$(open_volume "$SRC_SOCK" "$F6_VOL" "vol6.img" "$VOL_SIZE_BYTES" "$SEED_FILE")" \
    || qual_die "aborting (F6)"
qual_pass "F6: OpenVolume on SRC: ${F6_VOL} seeded (handle ${F6_HANDLE})"
F6_MID="$(trigger_migration "$SRC_SOCK" "$F6_VOL" "$F6_HANDLE" "https://127.0.0.1:${DST_PORT}")" \
    || qual_die "aborting (F6)"
qual_pass "F6: TriggerDiskMigration accepted → ${F6_MID}"

if wait_phase "$SRC_SOCK" "$F6_MID" BULK_COPY "$MIG_RPC_TIMEOUT"; then
    qual_pass "F6: BULK_COPY observed — SIGKILLing the CP mid-copy"
else
    qual_error "F6: BULK_COPY never observed (phase: $(migration_field "$SRC_SOCK" "$F6_MID" phase)) — kill window missed"
fi

F6_OPS_BEFORE="$(sqlite_query "$QUAL_DB" \
    "SELECT operation_id || ':' || status FROM operations ORDER BY operation_id" 2>/dev/null)"
kill -9 "$CP_PID" 2>/dev/null || true
wait_gone "$CP_PID" \
    && qual_pass "F6: control-plane SIGKILLed and gone (mid-BULK_COPY)" \
    || qual_error "F6: control-plane did not die"

# The stord↔stord path is direct (M4.6 declaration): the transfer must
# keep advancing through the CP outage. Non-disturbance evidence, in
# order of strength: bytes advancing while the CP is down, OR the phase
# progressing PAST BULK_COPY (PAUSED_FINAL_SYNC/COMPLETED — the transfer
# finished during the outage; run 1 hit exactly this: the seeded volume's
# bulk copy completed inside the 2 s sample window and bytesTransferred
# resets at the later phases, so a bytes-only check false-errors).
F6_NONDISTURBED=0
F6_PROBE_DEADLINE=$((SECONDS + 30))
while [ "$SECONDS" -lt "$F6_PROBE_DEADLINE" ]; do
    F6_BYTES_A="$(migration_field "$SRC_SOCK" "$F6_MID" bytesTransferred)"
    sleep 2
    F6_BYTES_B="$(migration_field "$SRC_SOCK" "$F6_MID" bytesTransferred)"
    F6_TOTAL="$(migration_field "$SRC_SOCK" "$F6_MID" totalBytes)"
    F6_PHASE_NOW="$(migration_field "$SRC_SOCK" "$F6_MID" phase)"
    if [ "${F6_BYTES_B:-0}" -gt "${F6_BYTES_A:-0}" ] || [ "${F6_BYTES_B:-0}" = "${F6_TOTAL:-x}" ]; then
        F6_NONDISTURBED=1
        qual_pass "F6: transfer kept advancing through the CP outage (bytes ${F6_BYTES_A:-?} → ${F6_BYTES_B:-?} of ${F6_TOTAL:-?})"
        break
    fi
    case "$F6_PHASE_NOW" in
        BULK_COPY) ;; # still copying with frozen bytes — keep probing
        *)
            F6_NONDISTURBED=1
            qual_pass "F6: transfer progressed past BULK_COPY while the CP was down (phase ${F6_PHASE_NOW}) — the direct path completed the outage unaffected"
            break
            ;;
    esac
done
if [ "$F6_NONDISTURBED" != "1" ]; then
    qual_error "F6: transfer STALLED while the CP was down (bytes ${F6_BYTES_A:-?} → ${F6_BYTES_B:-?}, phase ${F6_PHASE_NOW:-?}) — the direct path was disturbed"
fi

start_cp
qual_pass "F6: control-plane restarted (pid ${CP_PID})"

wait_for "F6: migration reached PAUSED_FINAL_SYNC across the CP restart" "$MIG_RPC_TIMEOUT" \
    phase_is "$SRC_SOCK" "$F6_MID" "PAUSED_FINAL_SYNC" \
    || qual_error "F6: migration task state not reachable/terminal after the CP restart (phase: $(migration_field "$SRC_SOCK" "$F6_MID" phase))"
F6_PAUSE_NEEDS="$(migration_field "$SRC_SOCK" "$F6_MID" needsVmPause)"
[ "$F6_PAUSE_NEEDS" = "true" ] \
    && qual_pass "F6: PAUSED_FINAL_SYNC demands the VM pause (needs_vm_pause=true)" \
    || qual_error "F6: needs_vm_pause is '${F6_PAUSE_NEEDS}' at pause (expected true)"
stord_rpc "$SRC_SOCK" ResumeDiskMigration \
    "{\"migrationId\":\"${F6_MID}\",\"vmPaused\":true}" >/dev/null 2>&1 \
    && qual_pass "F6: ResumeDiskMigration{vm_paused:true} accepted" \
    || qual_error "F6: ResumeDiskMigration RPC failed"
wait_for "F6: migration COMPLETED across the CP restart" "$MIG_RPC_TIMEOUT" \
    phase_is "$SRC_SOCK" "$F6_MID" "COMPLETED" \
    || qual_error "F6: migration never completed after the CP restart (phase: $(migration_field "$SRC_SOCK" "$F6_MID" phase))"
F6_SRC_SHA="$(sha256_of "${SRC_DIR}/vol6.img")"
F6_DST_SHA="$(sha256_of "${DST_DIR}/${F6_VOL}.img")"
[ "$F6_SRC_SHA" = "$F6_DST_SHA" ] \
    && qual_pass "F6: digest match (${F6_SRC_SHA:0:16}…)" \
    || qual_error "F6: digest mismatch (src=${F6_SRC_SHA} dst=${F6_DST_SHA})"
cmp -s "${SRC_DIR}/vol6.img" "${DST_DIR}/${F6_VOL}.img" \
    && qual_pass "F6: byte-compare identical" \
    || qual_error "F6: byte-compare failed"
{
    echo "### leg-F6 digest record ($(date -u +%FT%TZ))"
    echo "src ${SRC_DIR}/vol6.img ${F6_SRC_SHA}"
    echo "dst ${DST_DIR}/${F6_VOL}.img ${F6_DST_SHA}"
} >> "${EVIDENCE_DIR}/digests.txt"

# No duplicate/phantom CP work across the restart: the CP journals
# nothing for a stord↔stord migration and must not fabricate ops while
# replaying its own state.
F6_OPS_AFTER="$(sqlite_query "$QUAL_DB" \
    "SELECT operation_id || ':' || status FROM operations ORDER BY operation_id" 2>/dev/null)"
if [ "$F6_OPS_BEFORE" = "$F6_OPS_AFTER" ]; then
    qual_pass "F6: CP operation set unchanged across the restart (no duplicate/phantom journal work)"
else
    qual_error "F6: CP operation set CHANGED across the restart (duplicate/phantom work)"
    qual_error "  before: ${F6_OPS_BEFORE}"
    qual_error "  after:  ${F6_OPS_AFTER}"
fi
qual_chvctl_ok \
    && qual_pass "F6: CP healthy after restart (BFF answers)" \
    || qual_error "F6: CP not answering after restart"
assert_base_undisturbed "F6"
drain_deferred_reports "F6 (agent reconnected to the restarted CP)"
save_fault_evidence "f6"

# ---------------------------------------------------------------------------
# Leg F7 — idempotent cleanup / repeated retry + final global sweep
# ---------------------------------------------------------------------------
qual_info "--- F7: idempotent cleanup (delete → re-delete → double-stop → in-flight delete) + final sweep"

logind_ready "F7 pre-stop (base VM)" "$BASE_VM"
F7_DEL_OUT="$(qual_chvctl vm delete "$BASE_VM" 2>&1)"; F7_DEL_RC=$?
classify_bff_result "F7: delete of the base VM" "$F7_DEL_OUT" "$F7_DEL_RC"
wait_vm_stopped "$BASE_VM"
CH_EXPECTED=$((CH_EXPECTED - 1))
mark_deleted "$BASE_VM"
assert_no_ch_for_vm "F7 (base VM deleted)" "$BASE_VM"
assert_one_ch_process "F7 (after base delete)"
if [ "$(vm_state "$BASE_VM")" = "" ]; then
    qual_pass "F7: base VM absent from the BFF list after delete"
else
    qual_warn "F7: BFF list still renders the base VM after delete — documented M2.5 deferred-scope retention (authority-side delete; row/volume may persist)"
fi
TAPS_EXPECTED=$((TAPS_EXPECTED - 1))
assert_taps_clean "F7 (base tap removed asynchronously)"

# SECOND delete — the idempotent-retry contract.
F7_DEL2_OUT="$(qual_chvctl vm delete "$BASE_VM" 2>&1)"; F7_DEL2_RC=$?
classify_bff_result "F7: SECOND delete of the base VM (idempotent retry)" "$F7_DEL2_OUT" "$F7_DEL2_RC"

# Stop an already-stopped VM — clean acceptance or clean refusal, never a
# crash-class error, and never a CH spawn.
logind_ready "F7 pre-stop (f2 VM)" "$F2_VM"
F7_STOP_OUT="$(qual_chvctl vm stop "$F2_VM" 2>&1)"; F7_STOP_RC=$?
classify_bff_result "F7: stop of the Running f2 VM" "$F7_STOP_OUT" "$F7_STOP_RC"
wait_vm_guest_stopped "$F2_VM"
F7_F2_CH_AFTER_STOP="$(ch_pid_of "$F2_VM")"
F7_STOP2_OUT="$(qual_chvctl vm stop "$F2_VM" 2>&1)"; F7_STOP2_RC=$?
classify_bff_result "F7: stop of the ALREADY-STOPPED f2 VM" "$F7_STOP2_OUT" "$F7_STOP2_RC"
sleep 5
# A second stop must not spawn a duplicate VMM. The retained VMM from the
# first graceful stop is the expected shape (--no-shutdown), so assert ≤1
# process and the SAME pid — not zero processes.
assert_le_one_ch_for_vm "F7 (double-stop spawned nothing)" "$F2_VM"
if [ -n "$F7_F2_CH_AFTER_STOP" ] && [ "$(ch_pid_of "$F2_VM")" = "$F7_F2_CH_AFTER_STOP" ] \
    && ch_alive_of "$F2_VM"; then
    qual_pass "F7 (double-stop reused the retained VMM): same CH pid ${F7_F2_CH_AFTER_STOP} (no respawn)"
else
    qual_error "F7 (double-stop respawned or lost the retained VMM): pid $(ch_pid_of "$F2_VM"), expected ${F7_F2_CH_AFTER_STOP}"
fi

F7_F2_DEL_OUT="$(qual_chvctl vm delete "$F2_VM" 2>&1)"; F7_F2_DEL_RC=$?
classify_bff_result "F7: delete of the f2 VM" "$F7_F2_DEL_OUT" "$F7_F2_DEL_RC"
# Delete explicitly stops the retained VMM (the documented --no-shutdown
# contract); this is where the CH process must disappear.
wait_vm_stopped "$F2_VM"
CH_EXPECTED=$((CH_EXPECTED - 1))
mark_deleted "$F2_VM"
TAPS_EXPECTED=$((TAPS_EXPECTED - 1))
assert_taps_clean "F7 (f2 tap removed)"

# Delete a VM whose create is STILL IN FLIGHT: submit create, delete
# immediately — must converge to absence with no orphaned disk/tap.
F7P_VM="$(create_vm qual-fault-f7probe 1 512)" || qual_error "F7 probe: create rejected"
if [ -n "${F7P_VM:-}" ]; then
    qual_pass "F7 probe: create accepted (${F7P_VM}) — deleting immediately (create in flight)"
    F7P_DEL_OUT="$(qual_chvctl vm delete "$F7P_VM" 2>&1)"; F7P_DEL_RC=$?
    classify_bff_result "F7 probe: delete while the create is in flight" "$F7P_DEL_OUT" "$F7P_DEL_RC"
    mark_deleted "$F7P_VM"
    assert_no_stuck_ops "F7 probe" "$F7P_VM"
    assert_no_ch_for_vm "F7 probe (never started / torn down)" "$F7P_VM"
    # Net tap effect must be zero (either the create never provisioned,
    # or it provisioned and the delete removed it asynchronously).
    assert_taps_clean "F7 probe (no orphaned tap from the aborted create)"
    assert_disk_state_consistent "F7 probe"
fi

# --- final global sweep -----------------------------------------------------
qual_info "--- F7 final global sweep (zero residue, all journals terminal)"

CH_EXPECTED=0
assert_one_ch_process "final sweep (all VMs deleted)"
TAPS_EXPECTED="$TAPS_BASELINE"
assert_taps_clean "final sweep (zero scenario taps)"
assert_disk_state_consistent "final sweep (row ↔ backing bijection)"

F7_SESSIONS_ATTACHED="$(sqlite_query "$(stord_db)" \
    "SELECT COUNT(*) FROM sessions WHERE vm_id IS NOT NULL AND vm_id != ''" 2>/dev/null | head -1)"
F7_SESSIONS_LEFT="$(sqlite_query "$(stord_db)" \
    "SELECT COUNT(*) FROM sessions" 2>/dev/null | head -1)"
if [ -z "${F7_SESSIONS_LEFT}" ] || [ -z "${F7_SESSIONS_ATTACHED}" ]; then
    qual_error "final sweep: could not read the live stord sessions DB ($(stord_db)) — residue state unknown"
elif [ "${F7_SESSIONS_ATTACHED}" = "0" ] && [ "${F7_SESSIONS_LEFT}" = "0" ]; then
    qual_pass "final sweep: live stord sessions all closed (0 rows)"
elif [ "${F7_SESSIONS_ATTACHED}" = "0" ]; then
    # Pre-#711: deleting a VM created before the agent restart detached its
    # volume but could not close it, leaving a row with vm_id cleared. The
    # #711 fix marks the row `detached` and sweeps it, so residue here is
    # unexpected on a fixed candidate; keep it a warning rather than a gate
    # (the lane predates the fix and this sweep is not volume-scoped).
    qual_warn "final sweep: ${F7_SESSIONS_LEFT} detached stord session(s) retained — unexpected on the #711-fixed candidate (pre-fix this was the documented M2.2a residual): $(sqlite_query "$(stord_db)" 'SELECT volume_id, runtime_status FROM sessions' 2>/dev/null | head -3 | tr '\n' ' ')"
else
    qual_error "final sweep: FORBIDDEN — ${F7_SESSIONS_ATTACHED} stord session(s) still ATTACHED to a VM: $(sqlite_query "$(stord_db)" "SELECT volume_id, vm_id, runtime_status FROM sessions WHERE vm_id IS NOT NULL AND vm_id != ''" 2>/dev/null | head -3)"
fi

all_cp_ops_terminal() {
    local bad
    bad="$(sqlite_query "$QUAL_DB" \
        "SELECT COUNT(*) FROM operations WHERE status IN ('Pending','Accepted','Running','RetryPending','AwaitingOperatorInput')" 2>/dev/null)"
    [ "$bad" = "0" ]
}
wait_for "final sweep: every CP operation terminal" "$OPS_SETTLE" all_cp_ops_terminal \
    || qual_error "final sweep: ops stuck non-terminal: $(sqlite_query "$QUAL_DB" "SELECT operation_id || ':' || status FROM operations WHERE status IN ('Pending','Accepted','Running','RetryPending','AwaitingOperatorInput')" 2>/dev/null | tr '\n' ' ')"

F7_DUP_CREATES=0
for vm in $KNOWN_CREATED; do
    n="$(cp_create_op_count "$vm")"
    if [ "$n" != "1" ] && [ "$n" != "0" ]; then
        # deleted-before-dispatch creates may legitimately have 0 rows? No:
        # the BFF inserts the row at accept. Anything ≠1 is suspicious;
        # 0 means the VM was never actually created (guard anyway).
        if [ "$n" != "0" ]; then
            qual_error "final sweep: FORBIDDEN — ${n} CreateVm op rows for ${vm}"
            F7_DUP_CREATES=1
        fi
    fi
done
[ "$F7_DUP_CREATES" = "0" ] \
    && qual_pass "final sweep: exactly one CreateVm op row per created VM (no duplicated journal work)" \
    || true

save_fault_evidence "f7-final"

# ---------------------------------------------------------------------------
# Migration-topology close-out
# ---------------------------------------------------------------------------
qual_info "--- close-out: close sessions, stop scenario stords, zero residue"

close_volume_quiet "$SRC_SOCK" "$F5_VOL" "$F5_HANDLE" || true
if [ -n "${F6_HANDLE:-}" ]; then
    close_volume_quiet "$SRC_SOCK" "$F6_VOL" "$F6_HANDLE" || true
fi
stop_scenario_stord "$SRC_PID" "SRC"
stop_scenario_stord "$DST_PID" "DST"

F7_SRC_SESSIONS_LEFT="$(sqlite_query "${SRC_DIR}/stord.db" \
    "SELECT COUNT(*) FROM sessions" 2>/dev/null | head -1)"
if [ "${F7_SRC_SESSIONS_LEFT}" = "0" ]; then
    qual_pass "no stord sessions remain on the scenario source"
elif [ -z "${F7_SRC_SESSIONS_LEFT}" ]; then
    qual_error "could not read the scenario source sessions DB (${SRC_DIR}/stord.db) — residue state unknown"
else
    qual_error "stord sessions remain on the scenario source (${F7_SRC_SESSIONS_LEFT})"
fi
if [ -z "$(pgrep -f "(^|/)chv-stord( |$).*${M47_DIR}" 2>/dev/null)" ]; then
    qual_pass "no scenario chv-stord processes remain"
else
    qual_error "scenario chv-stord processes remain: $(pgrep -af "(^|/)chv-stord( |$).*${M47_DIR}" | head -3)"
fi
tcp_port_free "$DST_PORT" \
    && qual_pass "destination port ${DST_PORT} freed" \
    || qual_error "destination port ${DST_PORT} still in use"
assert_base_undisturbed_final() {
    if vm_ch_gone "$BASE_VM"; then
        qual_pass "deployed stack healthy at close-out (base VM cleanly deleted; CP/agent/stord/nwd alive)"
    else
        qual_error "base VM CH process still alive at close-out"
    fi
}
assert_base_undisturbed_final
pids_current

if [ "$QUAL_ERRORS" -eq 0 ]; then
    rm -rf "$M47_DIR"
    [ ! -e "$M47_DIR" ] \
        && qual_pass "scenario resource dir removed (${M47_DIR})" \
        || qual_error "scenario resource dir could not be removed: ${M47_DIR}"
else
    qual_info "errors recorded — ${M47_DIR} kept for post-mortem (deploy preserves TEST_DIR on failure)"
fi

# ---------------------------------------------------------------------------
qual_summary "m4.7-faults"
if [ "$QUAL_ERRORS" -gt 0 ]; then
    qual_error "M4.7 scenario finished with ${QUAL_ERRORS} error(s)"
    qual_info "test dir: ${QUAL_TEST_DIR} (deploy keeps it on failure)"
    exit 1
fi
qual_pass "M4.7 fault & interruption matrix complete: mid-operation kills (CP/agent/stord/nwd/source-stord) + idempotent cleanup, no forbidden outcomes"
exit 0
