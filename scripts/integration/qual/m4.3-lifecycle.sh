#!/usr/bin/env bash
# m4.3-lifecycle.sh — prompt-04 M4.3: lifecycle & recovery scenarios on
# real (nested) KVM, via the candidate deployment from deploy.sh.
#
# Run via deploy.sh --exec (root):
#   sudo GUEST_IMAGE=noble-qual-patched.img ./deploy.sh \
#       --exec ./m4.3-lifecycle.sh
# (the patched image is built by patch-guest-image.sh — the stock noble
# cloud image cannot boot through the firmware chain; see that script's
# header).
#
# Legs (plan §M4.3), all against one guest (qual-vm-1, 2 vCPU / 1 GiB,
# firmware boot, default network) driven through
# chvctl → BFF → control-plane → agent gRPC → CellHV Core → CH v53.0:
#
#   A. create → start → guest-boot evidence (console.log kernel banner +
#      logind) → reboot (guest-level, same CH process) → stop (graceful)
#      → start (same VMM process) → still healthy.
#   B. S1 replay: SIGKILL the agent while Running — CH survives, the
#      restarted agent re-adopts it (same CH pid, exactly one CH process,
#      no duplicate state).
#   C. control-plane restart while Running — VM unaffected, reconnect.
#   D. management-plane outage window (CP down 60 s) — API refuses, guest
#      keeps running; deterministic operation history after reconnect.
#   E. full stack restart (all four daemons) — CH orphaned but alive, cold
#      reconcile on restart (adoption, same CH pid). The provable subset
#      of host-reboot recovery, labeled as such.
#   F. stop (graceful) → delete → absence.
#
# Identity + operation-history determinism is asserted at every leg: VM
# id/name/node constant, exactly one VM, no new operations journaled by
# any restart (core.db operations/events counts + BFF task list stable
# across restart legs).
#
# Timing notes inherited from the M2.5 qualification (same host class):
# the guest boots to logind in ~10-60 s (kernel banner within 300 s),
# the graceful stop needs up to ~32 s inside the agent's 60 s window
# (snapd's stop job), and stops must be issued only after the current
# boot's systemd-logind lines appear (a pre-logind ACPI press is
# silently lost — see M2.5 run-8 root causes). Timeouts here carry
# nested-virtualization margin on top.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "${SCRIPT_DIR}/lib.sh"

# --- deployment map (set by deploy.sh --exec) ---
for var in QUAL_TEST_DIR QUAL_NODE_ID QUAL_AGENT_DIR QUAL_LOGS_DIR \
    QUAL_BINARY_DIR QUAL_BFF_URL QUAL_CHVCTL QUAL_CHVCTL_CONFIG_DIR \
    QUAL_CP_PID QUAL_STORD_PID QUAL_NWD_PID QUAL_AGENT_PID; do
    [ -n "${!var:-}" ] || qual_die "${var} not set — run via deploy.sh --exec"
done

CP_PID="$QUAL_CP_PID"
STORD_PID="$QUAL_STORD_PID"
NWD_PID="$QUAL_NWD_PID"
AGENT_PID="$QUAL_AGENT_PID"

VM_NAME="qual-vm-1"
VMS_DIR="${QUAL_AGENT_DIR}/vms"
# Disk seed: the ABSOLUTE image path (the orchestrator's documented
# operator escape hatch — resolve_disk_seed_path passes absolute paths
# through verbatim, and stord's path allowlist covers the qual images
# dir). The image-import → vm-create-by-name chain is broken in the
# frozen candidate (chvctl sends "url", the BFF reads "source_url"; the
# create-time lookup is by image_id UUID, not name; file:// URLs are
# rejected as remote) — filed as an issue and fixed post-rc1 on main.
# The harness does not depend on that chain.
GUEST_IMAGE_PATH="${QUAL_GUEST_IMAGE_PATH:-/var/lib/chv/qual/images/noble-qual-patched.img}"
CORE_DB="${QUAL_AGENT_DIR}/core.db"
CP_CONFIG="${QUAL_TEST_DIR}/controlplane.toml"
AGENT_CONFIG="${QUAL_TEST_DIR}/agent.toml"
STORD_CONFIG="${QUAL_TEST_DIR}/stord.toml"
NWD_CONFIG="${QUAL_TEST_DIR}/nwd.toml"

BOOT_TIMEOUT=420        # kernel banner after vm start (nested-virt margin)
LOGIND_TIMEOUT=180      # logind lines after the banner
STOP_TIMEOUT=240        # graceful stop (60 s window + snapd's ~32 s + margin)
RESTART_TIMEOUT=90      # daemon restarts / sockets
OUTAGE_WINDOW=60        # management-plane outage leg

# Persistent evidence artifacts (deploy.sh removes TEST_DIR on success).
EVIDENCE_DIR="${CHV_QUAL_ROOT:-/var/lib/chv/qual}/m4.3-artifacts"
mkdir -p "$EVIDENCE_DIR"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
# pids_current — record the CURRENT daemon pids for deploy.sh's teardown
# (the --exec child cannot propagate restarted pids any other way).
pids_current() {
    cat > "${QUAL_TEST_DIR}/pids.current" <<EOF
CP_PID=${CP_PID}
STORD_PID=${STORD_PID}
NWD_PID=${NWD_PID}
AGENT_PID=${AGENT_PID}
EOF
}

# vm_json_field FIELD — extract FIELD from our VM's row in
# `vm list --output json` (chvctl prints the bare items array in json
# mode — not a {"items": [...]} wrapper).
vm_json_field() {
    local json
    json="$(qual_chvctl --output json vm list 2>/dev/null || true)"
    VM_JSON="$json" CHV_FIELD="$1" CHV_VM_ID="$VM_ID" python3 <<'PYEOF'
import json, os
try:
    data = json.loads(os.environ["VM_JSON"])
    if not isinstance(data, list):
        data = data.get("items", [])
    for item in data:
        if item.get("vm_id") == os.environ["CHV_VM_ID"]:
            print(item.get(os.environ["CHV_FIELD"], ""))
            break
    else:
        print("")
except Exception:
    print("")
PYEOF
}

vm_count() {
    qual_chvctl --output json vm list 2>/dev/null \
        | python3 -c '
import json, sys
try:
    data = json.load(sys.stdin)
    if not isinstance(data, list):
        data = data.get("items", [])
    print(len(data))
except Exception:
    print(-1)' \
        || echo "-1"
}

vm_console_log() { echo "${VMS_DIR}/${VM_ID}/console.log"; }

count_boots() {
    local f n
    f="$(vm_console_log)"
    if [ -f "$f" ]; then
        n="$(grep -c 'Linux version' "$f" || true)"
        echo "${n:-0}"
    else
        echo 0
    fi
}

count_logind() {
    local f n
    f="$(vm_console_log)"
    if [ -f "$f" ]; then
        n="$(grep -c 'systemd-logind' "$f" || true)"
        echo "${n:-0}"
    else
        echo 0
    fi
}

# ch_pid — the CH process pid the agent persisted for this VM.
ch_pid() {
    cat "${VMS_DIR}/${VM_ID}/ch.pid" 2>/dev/null || echo ""
}

# ch_alive — the CH process the agent persisted for this VM is a LIVE
# process. Zombie-aware (state Z/X = gone): the graceful stop path leaves
# the exited CH child unreaped until the next start reaps it via
# prove_exited (verified by experiment: guest powers down orderly in
# ~32 s, CH exits WITH the guest, /proc/<pid> lingers as Z with an empty
# cmdline). This matches the product's own liveness semantics (pid_exists
# treats Z as gone) and pgrep -f (zombies have no cmdline to match).
ch_alive() {
    local pid state
    pid="$(ch_pid)"
    [ -n "$pid" ] || return 1
    [ -d "/proc/${pid}" ] || return 1
    state="$(awk '{print $3}' "/proc/${pid}/stat" 2>/dev/null || true)"
    [ "$state" != "Z" ] && [ "$state" != "X" ]
}

console_has() { grep -q "$1" "$(vm_console_log)" 2>/dev/null; }

save_console_evidence() {
    local label="$1"
    cp "$(vm_console_log)" "${EVIDENCE_DIR}/console-${label}.log" 2>/dev/null \
        || qual_warn "no console.log to save for ${label}"
}

# ops_snapshot — deterministic fingerprint of the operation history:
# core.db operations/events row counts + the BFF task list JSON.
ops_snapshot() {
    local ops events tasks
    ops="$(sqlite_query "$CORE_DB" 'SELECT count(*) FROM operations' 2>/dev/null || echo x)"
    events="$(sqlite_query "$CORE_DB" 'SELECT count(*) FROM events' 2>/dev/null || echo x)"
    tasks="$(qual_chvctl --output json task list 2>/dev/null || echo x)"
    printf 'ops=%s events=%s tasks_sha=%s\n' "$ops" "$events" \
        "$(printf '%s' "$tasks" | sha256sum | cut -c1-16)"
}

# assert_ops_unchanged DESC BEFORE AFTER
assert_ops_unchanged() {
    local desc="$1" before="$2" after="$3"
    if [ "$before" = "$after" ]; then
        qual_pass "${desc}: operation history unchanged (${before%% tasks*})"
    else
        qual_error "${desc}: operation history CHANGED by the restart (FORBIDDEN duplicate/replayed work)"
        qual_error "  before: ${before}"
        qual_error "  after:  ${after}"
    fi
}

assert_vm_identity() {
    local desc="$1" expected_state="$2"
    local n id name node state
    n="$(vm_count)"
    if [ "$n" = "1" ]; then
        qual_pass "${desc}: exactly one VM present"
    else
        qual_error "${desc}: expected exactly 1 VM, found ${n}"
    fi
    id="$(vm_json_field vm_id)"
    [ "$id" = "$VM_ID" ] \
        && qual_pass "${desc}: VM id constant (${VM_ID})" \
        || qual_error "${desc}: VM id changed: '${id}'"
    name="$(vm_json_field name)"
    [ "$name" = "$VM_NAME" ] \
        && qual_pass "${desc}: VM name constant (${VM_NAME})" \
        || qual_error "${desc}: VM name changed: '${name}'"
    node="$(vm_json_field node_id)"
    [ "$node" = "$QUAL_NODE_ID" ] \
        && qual_pass "${desc}: VM pinned to node ${QUAL_NODE_ID}" \
        || qual_error "${desc}: VM node changed: '${node}'"
    state="$(vm_json_field power_state)"
    [ "$state" = "$expected_state" ] \
        && qual_pass "${desc}: power_state ${expected_state}" \
        || qual_error "${desc}: power_state '${state}', expected '${expected_state}'"
}

assert_one_ch_process() {
    local desc="$1" n
    n="$(count_cloud_hypervisor_processes)"
    if [ "$n" = "1" ]; then
        qual_pass "${desc}: exactly one cloud-hypervisor process"
    else
        qual_error "${desc}: expected 1 cloud-hypervisor process, found ${n}"
        pgrep -af '(^|/)cloud-hypervisor( |$)' >&2 || true
    fi
}

assert_no_ch_process() {
    local desc="$1" n
    n="$(count_cloud_hypervisor_processes)"
    if [ "$n" = "0" ]; then
        qual_pass "${desc}: no cloud-hypervisor process remains"
    else
        qual_error "${desc}: FORBIDDEN — ${n} cloud-hypervisor process(es) remain"
        pgrep -af '(^|/)cloud-hypervisor( |$)' >&2 || true
    fi
}

# wait_no_ch_process desc timeout — delete stops the retained VMM
# asynchronously (the authority-side delete op completes before the CH
# process is reaped). Wait for the process to disappear before asserting
# absence, mirroring m4.7's wait_vm_stopped after delete. Without this the
# assertion races the teardown and records a false FORBIDDEN residue.
wait_no_ch_process() {
    local desc="$1" timeout="$2" waited=0
    while [ "$(count_cloud_hypervisor_processes)" != "0" ]; do
        sleep 2
        waited=$((waited + 2))
        if [ "$waited" -ge "$timeout" ]; then
            qual_error "${desc} (timed out after ${timeout}s)"
            return 1
        fi
    done
    qual_pass "${desc}"
}

# stop_daemon PID — SIGTERM, wait up to 10 s, then SIGKILL.
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

# start_cp / start_stord / start_nwd / start_agent — restart a daemon
# exactly as deploy.sh started it (same config, log appended) and record
# the new pid for teardown. Stale unix sockets from a SIGKILLed daemon
# are removed first so the wait_for below can only pass on the NEW
# process's socket.
start_cp() {
    "${QUAL_BINARY_DIR}/chv-controlplane" "$CP_CONFIG" \
        >> "${QUAL_LOGS_DIR}/controlplane.log" 2>&1 &
    CP_PID=$!
    pids_current
    wait_for "control-plane BFF responsive after restart" "$RESTART_TIMEOUT" \
        qual_chvctl_ok \
        || qual_die "control-plane did not come back — log: $(tail -20 "${QUAL_LOGS_DIR}/controlplane.log")"
}

qual_chvctl_ok() {
    qual_chvctl node list >/dev/null 2>&1
}

start_stord() {
    rm -f "${QUAL_TEST_DIR}/stord/api.sock"
    "${QUAL_BINARY_DIR}/chv-stord" "$STORD_CONFIG" \
        >> "${QUAL_LOGS_DIR}/stord.log" 2>&1 &
    STORD_PID=$!
    pids_current
    wait_for "stord socket up after restart" "$RESTART_TIMEOUT" \
        test -S "${QUAL_TEST_DIR}/stord/api.sock" \
        || qual_die "stord did not come back — log: $(tail -20 "${QUAL_LOGS_DIR}/stord.log")"
}

start_nwd() {
    rm -f "${QUAL_TEST_DIR}/nwd/api.sock"
    "${QUAL_BINARY_DIR}/chv-nwd" "$NWD_CONFIG" \
        >> "${QUAL_LOGS_DIR}/nwd.log" 2>&1 &
    NWD_PID=$!
    pids_current
    wait_for "nwd socket up after restart" "$RESTART_TIMEOUT" \
        test -S "${QUAL_TEST_DIR}/nwd/api.sock" \
        || qual_die "nwd did not come back — log: $(tail -20 "${QUAL_LOGS_DIR}/nwd.log")"
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

# logind_gate DESC BASE_COUNT — wait until the current boot's
# systemd-logind Starting/Started lines appeared (M2.5: a pre-logind ACPI
# power-button press is silently lost; the guest's serial lines are
# ANSI-fragmented, so grep the contiguous unit name — 2 lines per boot).
logind_gate() {
    local desc="$1" base="$2"
    local target=$((base + 2))
    local waited=0 cur
    cur="$(count_logind)"
    while [ "$cur" -lt "$target" ]; do
        sleep 2
        waited=$((waited + 2))
        if [ "$waited" -ge "$LOGIND_TIMEOUT" ]; then
            qual_error "${desc}: logind gate timed out (${cur}/${target} logind lines)"
            return 1
        fi
        cur="$(count_logind)"
    done
    qual_pass "${desc}: logind gate passed (${cur} logind lines)"
}

# logind_ready DESC — the CURRENT boot already shows logind evidence
# (>= 2 lines). Used before stops on a boot that started earlier: there
# is no new boot to wait for, only confirmation that userspace (the ACPI
# event consumer) is up.
logind_ready() {
    local desc="$1"
    local waited=0 cur
    cur="$(count_logind)"
    while [ "$cur" -lt 2 ]; do
        sleep 2
        waited=$((waited + 2))
        if [ "$waited" -ge "$LOGIND_TIMEOUT" ]; then
            qual_error "${desc}: no logind evidence on the current boot (${cur} lines) — an ACPI stop press may be silently lost"
            return 1
        fi
        cur="$(count_logind)"
    done
    qual_pass "${desc}: logind present on the current boot (${cur} lines)"
}

# ===========================================================================
qual_info "M4.3 lifecycle & recovery scenarios starting"
qual_info "evidence dir: ${EVIDENCE_DIR}"
qual_info "guest image: ${GUEST_IMAGE_PATH}"
[ -s "$GUEST_IMAGE_PATH" ] \
    || qual_die "guest image missing: ${GUEST_IMAGE_PATH} — build it with patch-guest-image.sh and run deploy.sh with GUEST_IMAGE=noble-qual-patched.img"
pids_current

# ---------------------------------------------------------------------------
# Leg A — full lifecycle
# ---------------------------------------------------------------------------
qual_info "--- Leg A: create → start → boot evidence → reboot → stop → start"

CREATE_OUT="$(qual_chvctl --output json vm create "$VM_NAME" \
    --cpu 2 --memory 1G --image "$GUEST_IMAGE_PATH" --network default 2>&1)" \
    || { qual_error "vm create failed: ${CREATE_OUT}"; qual_die "aborting"; }
VM_ID="$(printf '%s\n' "$CREATE_OUT" | python3 -c '
import json, sys
raw = sys.stdin.read()
start = raw.find("{")
try:
    data = json.loads(raw[start:])
    print(data.get("vm_id") or data.get("id") or "")
except Exception:
    print("")')"
[ -n "$VM_ID" ] || qual_die "could not parse vm_id from create output: ${CREATE_OUT}"
qual_pass "vm created: ${VM_NAME} (${VM_ID})"

CPU_F="$(vm_json_field cpu)"
[ "$CPU_F" = "2" ] \
    && qual_pass "spec honored: cpu_count 2" \
    || qual_error "spec NOT honored: cpu '${CPU_F}', expected 2 (#274 class regression)"
MEM_F="$(vm_json_field memory)"
case "$MEM_F" in
    1*GiB*|1*G*|1073741824*) qual_pass "spec honored: memory '${MEM_F}' (1 GiB)" ;;
    *) qual_error "spec NOT honored: memory '${MEM_F}', expected 1 GiB" ;;
esac
assert_vm_identity "after create" "Running"

qual_chvctl vm start "$VM_ID" >/dev/null 2>&1 \
    && qual_pass "vm start accepted" \
    || qual_error "vm start rejected"
vm_running_desired() { [ "$(vm_json_field power_state)" = "Running" ]; }
wait_for "VM Running (desired state)" 60 vm_running_desired \
    || qual_die "VM never reached Running — agent log: $(tail -30 "${QUAL_LOGS_DIR}/agent.log")"

# The desired-state flip is acceptance, not execution: the CH process and
# its runtime dir appear only after the dispatched CreateVm/StartVm
# converge. The kernel-banner gate below is the real "guest is up" wait;
# the process/runtime assertions only become meaningful after it.
console_banner() { console_has "Linux version"; }
wait_for "guest kernel banner in console.log" "$BOOT_TIMEOUT" console_banner \
    || qual_die "no kernel banner — guest did not boot (see $(vm_console_log) and ${QUAL_LOGS_DIR}/agent.log)"
qual_pass "guest boot evidence: kernel banner at $(grep -m1 'Linux version' "$(vm_console_log)" | head -c 120)"
[ -d "${VMS_DIR}/${VM_ID}" ] \
    && qual_pass "VM runtime dir present (${VMS_DIR}/${VM_ID})" \
    || qual_error "VM runtime dir missing"
assert_one_ch_process "Leg A start"
CH_PID_A="$(ch_pid)"
if [ -n "$CH_PID_A" ] && ch_alive; then
    qual_pass "CH process pid: ${CH_PID_A}"
else
    qual_error "CH pid not persisted/readable (${CH_PID_A:-empty}) — later same-pid assertions will be wrong"
fi
logind_gate "Leg A first boot" 0 || true
save_console_evidence "boot1"

BOOTS_BEFORE="$(count_boots)"
qual_chvctl vm reboot "$VM_ID" >/dev/null 2>&1 \
    && qual_pass "vm reboot accepted" \
    || qual_error "vm reboot rejected"
boots_two() { [ "$(count_boots)" -ge 2 ]; }
wait_for "second boot captured (kernel banner x2)" "$BOOT_TIMEOUT" boots_two \
    || qual_error "reboot did not produce a second boot (boots=$(count_boots))"
CH_PID_AFTER_REBOOT="$(ch_pid)"
if [ -n "$CH_PID_A" ] && [ "$CH_PID_AFTER_REBOOT" = "$CH_PID_A" ] && ch_alive; then
    qual_pass "reboot is guest-level: same CH process (${CH_PID_A})"
else
    qual_error "reboot changed the CH process (${CH_PID_A:-?} → ${CH_PID_AFTER_REBOOT:-?})"
fi
assert_one_ch_process "Leg A after reboot"
logind_gate "Leg A second boot" "$(count_logind)" \
    || qual_error "no logind evidence in second boot — stop may be lost (M2.5 run-8b)"
save_console_evidence "boot2"

# Graceful stop (logind-gated per M2.5: pre-logind presses are lost; the
# ~32 s snapd stop must fit the agent's 60 s window).
qual_chvctl vm stop "$VM_ID" >/dev/null 2>&1 \
    && qual_pass "vm stop accepted" \
    || qual_error "vm stop rejected"
# A clean guest stop keeps the VMM and its API alive with --no-shutdown.
# Wait for vm.info to report a terminal guest state, then pin the process
# identity across the next start.
guest_terminal() {
    curl -sg --max-time 2 --unix-socket "${VMS_DIR}/${VM_ID}/vm.sock" \
        http://localhost/api/v1/vm.info 2>/dev/null \
        | grep -Eq '"state"[[:space:]]*:[[:space:]]*"(Shutdown|Created)"'
}
wait_for "guest terminal state after graceful stop" "$STOP_TIMEOUT" guest_terminal \
    || qual_error "guest did not reach Shutdown/Created while the VMM remained available"
assert_one_ch_process "Leg A after guest stop"
CH_PID_STOPPED="$(ch_pid)"
[ "$CH_PID_STOPPED" = "$CH_PID_A" ] \
    && qual_pass "guest stop retained the same CH process (${CH_PID_A})" \
    || qual_error "guest stop changed CH process (${CH_PID_A:-?} → ${CH_PID_STOPPED:-?})"
vm_stopped_desired() { [ "$(vm_json_field power_state)" = "Stopped" ]; }
wait_for "VM Stopped (desired state)" 60 vm_stopped_desired \
    || qual_error "VM power_state never reached Stopped"
save_console_evidence "after-stop"

# Second start — boot the stopped guest through the retained VMM.
qual_chvctl vm start "$VM_ID" >/dev/null 2>&1 \
    && qual_pass "second vm start accepted" \
    || qual_error "second vm start rejected"
wait_for "VM Running again (desired state)" 60 vm_running_desired \
    || qual_error "VM did not return to Running after second start"
assert_one_ch_process "Leg A second start"
CH_PID_B="$(ch_pid)"
if [ -n "$CH_PID_B" ] && [ "$CH_PID_B" = "$CH_PID_A" ]; then
    qual_pass "second start reused the retained CH process (${CH_PID_A})"
else
    qual_error "second start replaced the retained CH process (${CH_PID_A:-?} → ${CH_PID_B:-?})"
fi
wait_for "guest kernel banner after second start" "$BOOT_TIMEOUT" console_banner \
    || qual_error "no boot evidence after second start"
logind_gate "Leg A third boot" 0 || true
assert_vm_identity "Leg A end" "Running"
OPS_A="$(ops_snapshot)"
qual_info "Leg A op snapshot: ${OPS_A}"

# ---------------------------------------------------------------------------
# Leg B — S1 replay: agent SIGKILL while Running
# ---------------------------------------------------------------------------
qual_info "--- Leg B: S1 — agent SIGKILL while Running (CH must survive)"

kill -9 "$AGENT_PID"
sleep 1
if ch_alive; then
    qual_pass "S1: CH process survived the agent SIGKILL (pid $(ch_pid))"
else
    qual_error "S1: CH process died with the agent (FORBIDDEN)"
fi
sleep 4
ch_alive && qual_pass "S1: CH still alive 5 s after the crash" \
    || qual_error "S1: CH died within 5 s of the agent crash"

start_agent
ch_same_as_b() { [ "$(ch_pid)" = "$CH_PID_B" ] && ch_alive; }
wait_for "S1: agent re-adopted (CH still original pid)" "$RESTART_TIMEOUT" ch_same_as_b \
    || qual_error "S1: agent did not re-adopt the running CH (pid $(ch_pid), expected ${CH_PID_B})"
assert_one_ch_process "Leg B after agent restart"
assert_vm_identity "Leg B" "Running"
OPS_B="$(ops_snapshot)"
assert_ops_unchanged "Leg B: agent crash+restart" "$OPS_A" "$OPS_B"
save_console_evidence "after-s1"

# ---------------------------------------------------------------------------
# Leg C — control-plane restart while Running
# ---------------------------------------------------------------------------
qual_info "--- Leg C: control-plane restart while Running"

stop_daemon "$CP_PID"
wait_gone "$CP_PID" || qual_error "control-plane did not stop"
ch_alive && qual_pass "Leg C: CH unaffected while CP down" \
    || qual_error "Leg C: CH died during CP restart"
start_cp
CH_PID_C="$(ch_pid)"
[ "$CH_PID_C" = "$CH_PID_B" ] && ch_alive \
    && qual_pass "Leg C: same CH process across CP restart (${CH_PID_B})" \
    || qual_error "Leg C: CH process changed across CP restart (${CH_PID_B} → ${CH_PID_C})"
assert_vm_identity "Leg C" "Running"
OPS_C="$(ops_snapshot)"
assert_ops_unchanged "Leg C: CP restart" "$OPS_B" "$OPS_C"

# ---------------------------------------------------------------------------
# Leg D — management-plane outage window
# ---------------------------------------------------------------------------
qual_info "--- Leg D: management-plane outage (${OUTAGE_WINDOW}s window)"

stop_daemon "$CP_PID"
wait_gone "$CP_PID" || qual_error "control-plane did not stop for outage window"
CPU_T0="$(ps -o times= -p "$(ch_pid)" 2>/dev/null | tr -d ' ' || echo 0)"
OUTAGE_ELAPSED=0
while [ "$OUTAGE_ELAPSED" -lt "$OUTAGE_WINDOW" ]; do
    sleep 5
    OUTAGE_ELAPSED=$((OUTAGE_ELAPSED + 5))
    ch_alive || qual_error "Leg D: CH died during the outage window (${OUTAGE_ELAPSED}s in)"
    if qual_chvctl_ok; then
        qual_error "Leg D: API answered during the outage window (FORBIDDEN)"
    fi
done
CPU_T1="$(ps -o times= -p "$(ch_pid)" 2>/dev/null | tr -d ' ' || echo 0)"
if [ "${CPU_T1:-0}" -gt "${CPU_T0:-0}" ]; then
    qual_pass "Leg D: guest kept executing through the outage (cputime ${CPU_T0}s → ${CPU_T1}s)"
else
    qual_warn "Leg D: guest cputime did not advance (${CPU_T0} → ${CPU_T1}) — idle guest?"
fi
qual_pass "Leg D: API refused connections for the full ${OUTAGE_WINDOW}s window"
start_cp
CH_PID_D="$(ch_pid)"
[ "$CH_PID_D" = "$CH_PID_B" ] && ch_alive \
    && qual_pass "Leg D: same CH process across the outage (${CH_PID_B})" \
    || qual_error "Leg D: CH process changed across the outage (${CH_PID_B} → ${CH_PID_D})"
assert_vm_identity "Leg D" "Running"
OPS_D="$(ops_snapshot)"
assert_ops_unchanged "Leg D: management-plane outage" "$OPS_C" "$OPS_D"
save_console_evidence "after-outage"

# Outage-heal gate: the agent must have reconnected and FLUSHED its
# deferred control-plane reports before Leg E stops the stack. The
# candidate's every-boot startup path runs the legacy-cache import
# validation on the retained NodeCache (cellhv-core-startup
# prepare_activation → cellhv-nodecache-migration plan) and fails
# closed with Migration(Unsupported("pending_control_plane")) when the
# cache holds deferred reports — an agent restart with an unflushed
# queue bricks the node until an operator repairs the cache (issue
# #343, found by run 4's Leg E). The queue is a live observable: the
# agent saves agent-cache.json on every defer AND on the reconnect
# flush (one batch), so poll it to zero. A cold restart with unflushed
# reports is recorded as residual risk in the evidence doc, not run
# here.
pending_reports() {
    # The cache path is env-passed, never interpolated into the Python
    # source: a path containing quotes or backslashes must degrade to
    # "unreadable" (and the drain timeout below), not break the parser.
    # A partially-written cache (the agent persists on every defer)
    # also reads as "unreadable" and simply keeps the loop waiting.
    CHV_CACHE_FILE="${QUAL_AGENT_DIR}/agent-cache.json" python3 -c '
import json, os
try:
    with open(os.environ["CHV_CACHE_FILE"]) as f:
        print(len(json.load(f).get("pending_control_plane", [])))
except Exception:
    print("unreadable")
' 2>/dev/null || echo "unreadable"
}
DRAIN_T0="$(date +%s)"
DRAIN_START="$(pending_reports)"
DRAIN_WAITED=0
while [ "$(pending_reports)" != "0" ]; do
    sleep 2
    DRAIN_WAITED=$((DRAIN_WAITED + 2))
    if [ "$DRAIN_WAITED" -ge 240 ]; then
        qual_warn "Leg D: deferred reports did not drain (${DRAIN_START} queued, still $(pending_reports)) — Leg E cold restart will hit the #343 startup failure"
        break
    fi
done
if [ "$(pending_reports)" = "0" ]; then
    qual_pass "Leg D: outage healed — deferred reports flushed (${DRAIN_START} queued → 0 in ${DRAIN_WAITED}s)"
fi

# ---------------------------------------------------------------------------
# Leg E — full stack restart (cold reconcile)
# ---------------------------------------------------------------------------
qual_info "--- Leg E: full stack restart (all four daemons; cold reconcile)"

stop_daemon "$AGENT_PID"
stop_daemon "$NWD_PID"
stop_daemon "$STORD_PID"
stop_daemon "$CP_PID"
for p in "$AGENT_PID" "$NWD_PID" "$STORD_PID" "$CP_PID"; do
    wait_gone "$p" || qual_error "daemon ${p} did not stop"
done
ch_alive && qual_pass "Leg E: CH orphaned but alive after full stack stop" \
    || qual_error "Leg E: CH died with the stack (host-reboot subset does not hold)"

start_cp
start_stord
start_nwd
start_agent

CH_PID_E="$(ch_pid)"
if [ "$CH_PID_E" = "$CH_PID_B" ] && ch_alive; then
    qual_pass "Leg E: cold reconcile re-adopted the running CH (${CH_PID_B})"
else
    qual_error "Leg E: cold reconcile did not re-adopt (pid $(ch_pid), expected ${CH_PID_B})"
fi
assert_one_ch_process "Leg E after cold reconcile"
assert_vm_identity "Leg E" "Running"
NODE_JSON="$(qual_chvctl --output json node list 2>/dev/null || true)"
assert_contains "Leg E: node visible after stack restart" "$NODE_JSON" "$QUAL_NODE_ID" || true
assert_contains "Leg E: node health Healthy" "$NODE_JSON" '"health": "Healthy"' || true
OPS_E="$(ops_snapshot)"
assert_ops_unchanged "Leg E: full stack restart" "$OPS_D" "$OPS_E"
save_console_evidence "after-cold-reconcile"

# ---------------------------------------------------------------------------
# Leg F — stop (graceful, logind-gated) → delete → absence
# ---------------------------------------------------------------------------
qual_info "--- Leg F: stop → delete"

logind_ready "Leg F pre-stop"
qual_chvctl vm stop "$VM_ID" >/dev/null 2>&1 \
    && qual_pass "final vm stop accepted" \
    || qual_error "final vm stop rejected"
wait_for "Leg F guest terminal state" "$STOP_TIMEOUT" guest_terminal \
    || qual_error "Leg F guest did not reach Shutdown/Created"
assert_one_ch_process "Leg F after guest stop"
CH_PID_F_STOPPED="$(ch_pid)"
[ "$CH_PID_F_STOPPED" = "$CH_PID_E" ] \
    && qual_pass "Leg F guest stop retained the same CH process (${CH_PID_E})" \
    || qual_error "Leg F guest stop changed CH process (${CH_PID_E:-?} → ${CH_PID_F_STOPPED:-?})"
wait_for "VM Stopped (desired state)" 60 vm_stopped_desired || true
save_console_evidence "final-stop"

qual_chvctl vm delete "$VM_ID" >/dev/null 2>&1 \
    && qual_pass "vm delete accepted" \
    || qual_error "vm delete rejected"
# The authority-side delete is the contract (delete op succeeded, core
# state gone); the BFF/CP row, the vm dir and the volume are RETAINED —
# the documented M2.5 deferred-scope findings (runs 8e/9), re-observed
# here, not M4.3 gates. Record honestly instead of failing.
if [ "$(vm_count)" = "0" ]; then
    qual_pass "VM absent from BFF list after delete"
else
    qual_warn "BFF list still renders the VM after delete — documented M2.5 deferred-scope retention (authority-side delete only; vm dir + volume also retained)"
fi
wait_no_ch_process "Leg F CH process reaped after delete" "$STOP_TIMEOUT" || true
assert_no_ch_process "Leg F after delete"
OPS_F="$(ops_snapshot)"
qual_info "Leg F op snapshot: ${OPS_F}"

# ---------------------------------------------------------------------------
qual_summary "m4.3-lifecycle"
[ "${QUAL_ERRORS}" -gt 0 ] && exit 1 || exit 0
