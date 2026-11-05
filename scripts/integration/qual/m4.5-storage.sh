#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# M4.5 — Storage qualification on real KVM (nested)
#
# The prompt's reusable storage contract
#   validate → provision/consume → attach → guest write/read →
#   restart/interruption → recover → detach → cleanup → repeat
# for the two DECLARED storage profiles:
#
#   local file  — the VM-integrated path (the only profile an operator
#                 can reach today): BFF vm create seeds the boot volume
#                 (qcow2 → raw conversion), stord opens/attaches it,
#                 cloud-init writes a marker from inside the guest, a
#                 stop/start cycle proves the bytes survived (read-back
#                 on the next boot), a stord SIGKILL proves the running
#                 VM survives the daemon's death and the supervisor's
#                 replacement serves NEW provisioning, snapshot/clone
#                 assert the post-#495 #378 truth (REJECTED at accept
#                 time on core-managed nodes — InvalidArgument → HTTP
#                 400 BEFORE any journaling; the agent's fail-closed
#                 dispatch remains the enforcement backstop), and VM
#                 delete closes the stord session.
#
#   LVM         — TWO legs since #379:
#                 (stord layer, Leg F) the LVMBackend's real contract
#                 (open/export, block write/read, COW snapshot, clone,
#                 resize, read-only policy, health, and — new in #379
#                 DP2 — create-on-open provisioning of an absent LV)
#                 via the root-gated integration tests in
#                 crates/chv-stord-backends/tests/lvm_real.rs (the
#                 host-safety pattern: harness provisions the VG, runs
#                 the built test binary, asserts no residue);
#                 (VM-integrated, Leg G) the node's stord flips to
#                 backend_type=lvm (operator stord.toml via the #385
#                 stord_config_path pass-through, agent restarted so it
#                 re-parses the same file — DP4 inventory + DP5 locator
#                 shaping), a storage_class=lvm VM create provisions its
#                 boot LV via DP2 create-on-open, the operator seeds it
#                 out-of-band (qemu-img convert onto the LV — LVM has
#                 no seed path by design), and the guest
#                 boots/writes/persists through the dm-path locator the
#                 device_allowlist admits.
#
# Layer truths this scenario records (not works around):
#   - The VM-integrated LVM path exists since #379 (DP1/DP2/DP5): a
#     class-carrying disk dispatches to the LVM backend, the open
#     locator is shaped /dev/mapper/{vg}-{vid} (the device_allowlist's
#     dm-path shape), and LVMBackend::open provisions an absent LV when
#     the open carries size_bytes. LVM remains SEED-LESS: image seeding
#     is out-of-band (qemu-img convert onto the LV — the documented
#     operator model, docs/OPERATIONS.md), and LV reclamation on VM
#     delete is out-of-band too (stord closes the session but never
#     lvremoves — Leg G asserts and cleans up).
#   - VM create makes exactly ONE boot volume (no data volumes, no
#     standalone volume-create API) — the contract's "guest write/read"
#     is proven on the boot volume (vda).
#   - Snapshot/clone of a RUNNING VM's volume has no guest freeze; this
#     scenario snapshots a STOPPED VM (deterministic content). The
#     running-VM consistency question is recorded, not claimed. (At the
#     LVM layer both snapshot and clone are `lvcreate -s` COW snapshots
#     — point-in-time INDEPENDENT views, not full block copies; the
#     lvm_real tests pin the independence, not a copy mechanism.)
#
# Non-claims (inherited): Ceph RBD and iSCSI are declared non-scope
# (declaration §3). Coexistence residue beyond this scenario's own
# artifacts is not claimed.
#
# Run via deploy.sh --exec (as root, on the qualification host):
#   sudo env "PATH=$PATH" GUEST_IMAGE=noble-qual-patched.img \
#     ./scripts/integration/qual/deploy.sh --exec \
#     ./scripts/integration/qual/m4.5-storage.sh
# ---------------------------------------------------------------------------

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

VMS_DIR="${QUAL_AGENT_DIR}/vms"
STORD_DIR="${QUAL_TEST_DIR}/stord"          # deploy's stord runtime_dir
STORD_DB="${STORD_DIR}/stord.db"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
DEFAULT_NET="default"                       # network REFERENCE for vm create
GUEST_IMAGE_PATH="${QUAL_GUEST_IMAGE_PATH:-/var/lib/chv/qual/images/noble-qual-patched.img}"

BOOT_TIMEOUT=420        # kernel banner after vm start (nested-virt margin)
LOGIND_TIMEOUT=180      # logind lines after the banner
STOP_TIMEOUT=240        # graceful stop
STORD_RESTART_TIMEOUT=90  # agent supervisor restart of a killed stord
DISPATCH_TIMEOUT=60     # journaled intent → observable host effect
LVM_VG="chvqual-m45"    # disposable VG for the LVM leg (FIXED name: single-
                        # run assumption — two concurrent scenarios on one
                        # host would collide on vgcreate/lvcreate and a
                        # dying run's vgremove could tear down the other's
                        # VG. The fixed name doubles as stale-VG recovery:
                        # a leftover chvqual-m45 from an aborted run is
                        # removed by this run's cleanup.)
LVM_BACKING_MB=256      # loopback PV size (sparse file)
LVM_VG_G="chvqual-m45g" # Leg G's OWN VG (distinct from Leg F's so neither
                        # leg's teardown can touch the other's; same
                        # fixed-name/single-run reasoning as LVM_VG)
LVM_BACKING_GB_G=6      # Leg G loopback PV size (sparse): the guest image's
                        # VIRTUAL size is 3.5 GiB, so the LV must be ≥ 4 GiB
                        # for the out-of-band raw convert to fit

# Persistent evidence artifacts (deploy.sh removes TEST_DIR on success).
EVIDENCE_DIR="${CHV_QUAL_ROOT:-/var/lib/chv/qual}/m4.5-artifacts"
mkdir -p "$EVIDENCE_DIR"

# Unique per-run marker the guest writes and reads back.
M45_MARKER="m45-$$-persist"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
pids_current() {
    cat > "${QUAL_TEST_DIR}/pids.current" <<EOF
CP_PID=${QUAL_CP_PID}
STORD_PID=${QUAL_STORD_PID}
NWD_PID=${QUAL_NWD_PID}
AGENT_PID=${QUAL_AGENT_PID}
EOF
}

# stord_socket_live — the stord api socket accepts connections.
stord_socket_live() {
    python3 - "$STORD_DIR/api.sock" <<'PYEOF' 2>/dev/null
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

vm_ch_gone() {
    ! pgrep -f "(^|/)cloud-hypervisor( |$).*vms/$1" >/dev/null 2>&1
}

wait_vm_stopped() {
    local vm="$1" waited=0 strikes=0 pid
    while ! vm_ch_gone "$vm"; do
        if curl -sg --max-time 2 --unix-socket "${VMS_DIR}/${vm}/vm.sock" \
            http://localhost/api/v1/vm.info >/dev/null 2>&1; then
            strikes=0
        else
            strikes=$((strikes + 1))
            if [ "$strikes" -ge 3 ]; then
                pid="$(cat "${VMS_DIR}/${vm}/ch.pid" 2>/dev/null || true)"
                qual_error "wait_vm_stopped(${vm}): #345 wedge — CH ${pid} alive with a dead API; remediating with SIGKILL"
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

vm_console_log() { echo "${VMS_DIR}/$1/console.log"; }

console_has() { grep -aq "$2" "$(vm_console_log "$1")" 2>/dev/null; }

console_count() {
    local f
    f="$(vm_console_log "$1")"
    if [ -f "$f" ]; then
        grep -ac "$2" "$f" || true
    else
        echo 0
    fi
}

count_boots() {
    local f
    f="$(vm_console_log "$1")"
    if [ -f "$f" ]; then
        grep -ac 'Linux version' "$f" || true
    else
        echo 0
    fi
}

boot_count_gt() {
    [ "$(count_boots "$1")" -gt "$2" ]
}

save_console_evidence() {
    cp "$(vm_console_log "$1")" "${EVIDENCE_DIR}/console-$1-$2.log" 2>/dev/null \
        || qual_warn "no console.log to save for $1/$2"
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

# volume_id_of VM_ID — the boot volume bound to this VM (CP DB).
volume_id_of() {
    sqlite_query "$QUAL_DB" \
        "SELECT volume_id FROM volume_desired_state WHERE attached_vm_id='$1'" 2>/dev/null | head -1
}

# volume_field VOLUME_ID FIELD — from the CP DB's volumes table.
volume_field() {
    sqlite_query "$QUAL_DB" \
        "SELECT $2 FROM volumes WHERE volume_id='$1'" 2>/dev/null | head -1
}

# stord_sessions VOLUME_ID — open session rows for the volume in the LIVE
# stord's stord.db. #385 truth: with stord_config_path set, the
# supervisor-respawned stord runs the OPERATOR config — runtime_dir stays
# the deploy's stord dir. Resolve the CURRENT stord's config (its argv)
# per call so the query always targets the live daemon's DB; fall back to
# the deploy's path (unresolvable).
stord_pid() {
    pgrep -f "(^|/)chv-stord( |$).*${QUAL_TEST_DIR}" 2>/dev/null | head -1
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
stord_db() {
    echo "$(stord_runtime_dir)/stord.db"
}
stord_sessions() {
    sqlite_query "$(stord_db)" \
        "SELECT COUNT(*) FROM sessions WHERE volume_id='$1'" 2>/dev/null | head -1
}

# volume_backing VM_ID — the boot volume's backing FILE on the host
# (discovered, not assumed: the agent derives the path at reconcile).
volume_backing() {
    find "${VMS_DIR}/$1" -maxdepth 1 -name '*.img' -type f 2>/dev/null | head -1
}

# create_vm_userdata NAME CPU MEM_MB → VM_ID on stdout (empty on failure).
# Raw BFF POST (chvctl cannot carry cloud_init_userdata): the userdata
# writes the per-run marker on first boot (runcmd, per-instance) and
# reads it back on EVERY boot (bootcmd) — the guest-visible write/read
# proof of the storage contract.
create_vm_userdata() {
    local name="$1" cpu="$2" mem="$3" payload http
    payload="$(python3 - "$name" "$cpu" "$mem" "$M45_MARKER" <<'PYEOF'
import json, sys
name, cpu, mem, marker = sys.argv[1:5]
userdata = (
    "#cloud-config\n"
    "bootcmd:\n"
    "  - [ sh, -c, 'if [ -f /var/lib/m45.marker ]; then "
    "echo \"M45-MARKER-READBACK:$(cat /var/lib/m45.marker)\" > /dev/console; fi' ]\n"
    "runcmd:\n"
    "  - [ sh, -c, "
    f"'echo \"{marker}\" > /var/lib/m45.marker; "
    "echo \"M45-MARKER-WRITTEN:$(cat /var/lib/m45.marker)\" > /dev/console' ]\n"
)
print(json.dumps({
    "name": name,
    "cpu_count": int(cpu),
    "memory_mb": int(mem),
    "image_ref": "ubuntu-noble",
    "network_id": "default",
    "cloud_init_userdata": userdata,
}))
PYEOF
)" || { qual_error "could not build vm-create payload for ${name}"; return 1; }
    http="$(curl -s -o "${EVIDENCE_DIR}/vm-create-${name}.json" -w '%{http_code}' \
        -X POST "${QUAL_BFF_URL}/v1/vms/create" \
        -H "Authorization: Bearer $(bff_token)" -H "Content-Type: application/json" \
        -d "$payload")"
    [ "$http" = "200" ] \
        || { qual_error "BFF vm create ${name} failed (HTTP ${http}): $(cat "${EVIDENCE_DIR}/vm-create-${name}.json" 2>/dev/null)"; return 1; }
    python3 -c '
import json, sys
data = json.load(open(sys.argv[1]))
print(data.get("vm_id") or data.get("id") or "")' "${EVIDENCE_DIR}/vm-create-${name}.json"
}

# create_vm NAME CPU MEM_MB → VM_ID via chvctl (no userdata; the M4.3/M4.4
# shape, used where guest write/read is not the point). NOTE: chvctl's
# --memory takes a SIZE STRING (parse_size_bytes: bare numbers are BYTES
# — run-5 finding: --memory 1024 created a VM with 1024 BYTES of RAM,
# which CH accepts but can never boot) — always suffix with M here.
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

# bff_post_json PATH BODY_FILE → HTTP code (response saved to stdout).
bff_post_json() {
    curl -s -o "${EVIDENCE_DIR}/bff-$(basename "$1").json" -w '%{http_code}' \
        -X POST "${QUAL_BFF_URL}$1" \
        -H "Authorization: Bearer $(bff_token)" -H "Content-Type: application/json" \
        -d @"$2"
}

# bff_token — the JWT chvctl stored (admin), for raw BFF probes.
bff_token() { cat "${QUAL_CHVCTL_CONFIG_DIR}/chvctl/credentials" 2>/dev/null; }

save_evidence() {
    # $1 = phase label; captures storage-relevant host + DB state.
    local label="$1"
    {
        echo "### m4.5 evidence snapshot: ${label} ($(date -u +%FT%TZ))"
        echo "--- vm dirs and volume backings:"
        find "$VMS_DIR" -maxdepth 2 -name '*.img' -printf '%p %s bytes\n' 2>/dev/null || true
        echo "--- stord runtime dir (deploy's):"
        ls -la "$STORD_DIR" 2>/dev/null || true
        echo "--- stord sessions (deploy's stord.db):"
        sqlite_query "$STORD_DB" \
            "SELECT volume_id, vm_id, runtime_status FROM sessions" 2>/dev/null || true
        # #385: with stord_config_path set the respawned stord runs the
        # operator config — same runtime_dir as deploy's. Capture the live
        # dir's DB too when a config ever relocates it again.
        if [ "$(stord_db)" != "$STORD_DB" ]; then
            echo "--- live stord runtime_dir ($(stord_runtime_dir)):"
            ls -la "$(stord_runtime_dir)" 2>/dev/null || true
            echo "--- stord sessions (live stord.db):"
            sqlite_query "$(stord_db)" \
                "SELECT volume_id, vm_id, runtime_status FROM sessions" 2>/dev/null || true
        fi
        echo "--- CP volumes:"
        sqlite_query "$QUAL_DB" \
            "SELECT volume_id, node_id, capacity_bytes FROM volumes" 2>/dev/null || true
        echo "--- CP volume_desired_state:"
        sqlite_query "$QUAL_DB" \
            "SELECT volume_id, attached_vm_id, snapshot_op, snapshot_name, clone_source_volume_id FROM volume_desired_state" 2>/dev/null || true
        echo
    } >> "${EVIDENCE_DIR}/host-state.txt"
}

qual_info "=== M4.5 storage qualification start (node ${QUAL_NODE_ID}) ==="
qual_info "candidate: $(cat "${QUAL_BINARY_DIR}/CANDIDATE_SHA" 2>/dev/null || echo unknown)"
pids_current
save_evidence "start"

# ---------------------------------------------------------------------------
# Leg A — local file: provision (seed → raw) → attach → boot → guest WRITE
# ---------------------------------------------------------------------------
qual_info "--- Leg A: vm create (seeded boot volume) → host materialization → boot → guest writes the marker"

VM1_ID="$(create_vm_userdata qual-stor-1 2 1024)" || qual_die "aborting"
qual_pass "vm created: qual-stor-1 (${VM1_ID}) with cloud_init_userdata (marker ${M45_MARKER})"

VOL1_ID="$(volume_id_of "$VM1_ID")"
[ -n "$VOL1_ID" ] \
    && qual_pass "boot volume bound in CP DB (${VOL1_ID})" \
    || qual_die "no volume in volume_desired_state for ${VM1_ID}"

VOL1_CAPACITY="$(volume_field "$VOL1_ID" capacity_bytes)"
[ -n "$VOL1_CAPACITY" ] && [ "$VOL1_CAPACITY" -gt 0 ] \
    && qual_pass "volume row carries capacity (${VOL1_CAPACITY} bytes)" \
    || qual_warn "volume capacity missing/zero in CP DB: '${VOL1_CAPACITY}'"

# NOTE: wait_for evaluates its args ONCE — the backing discovery must
# re-run per poll, hence the function (the m4.4-documented trap). The
# wait gates on CONVERSION COMPLETION, not file existence: the local
# backend materializes the backing as an intermediate qcow2 copy first
# (smaller than capacity) and only reaches full size after the
# qcow2→raw conversion + set_len (run-2 finding).
vm_volume_ready() {
    local p
    p="$(volume_backing "$1")"
    [ -n "$p" ] && [ "$(stat -c %s "$p" 2>/dev/null || echo 0)" -ge "$2" ]
}
wait_for "volume backing materialized + seed conversion complete (full size)" \
    "$DISPATCH_TIMEOUT" \
    vm_volume_ready "$VM1_ID" "${VOL1_CAPACITY:-1}" \
    || qual_die "no volume backing file under ${VMS_DIR}/${VM1_ID}"
VOL1_PATH="$(volume_backing "$VM1_ID")"
qual_pass "volume backing materialized on host: ${VOL1_PATH}"

# The seed conversion contract: the backing must be RAW (the qcow2 seed
# was converted — a lingering qcow2 would still boot via CH but breaks
# the local backend's snapshot/clone, which reject qcow2 explicitly).
if qemu-img info "$VOL1_PATH" 2>/dev/null | grep -q 'file format: raw'; then
    qual_pass "seeded volume is RAW (qcow2 → raw conversion ran)"
else
    qual_warn "volume backing is not raw per qemu-img: $(qemu-img info "$VOL1_PATH" 2>/dev/null | grep 'file format' || echo 'qemu-img unavailable')"
fi

VOL1_SIZE="$(stat -c %s "$VOL1_PATH" 2>/dev/null || echo 0)"
[ "$VOL1_SIZE" -ge "${VOL1_CAPACITY:-0}" ] \
    && qual_pass "backing size ${VOL1_SIZE} ≥ requested capacity ${VOL1_CAPACITY}" \
    || qual_warn "backing size ${VOL1_SIZE} < requested capacity ${VOL1_CAPACITY}"

# stord opened a session for the volume (the agent↔stord attach path).
stord_session_open() { [ "$(stord_sessions "$VOL1_ID")" -ge 1 ]; }
wait_for "stord session open for the volume (live stord.db)" "$DISPATCH_TIMEOUT" \
    stord_session_open \
    || qual_error "no stord session row for ${VOL1_ID}"

save_evidence "leg-a provisioned"

qual_chvctl vm start "$VM1_ID" >/dev/null || qual_die "vm start failed for ${VM1_ID}"
wait_boot "$VM1_ID" || qual_die "guest did not boot (Leg A)"

# THE guest write: cloud-init runcmd wrote the marker to the boot volume.
wait_for "guest wrote the marker to the boot volume (console)" "$LOGIND_TIMEOUT" \
    console_has "$VM1_ID" "M45-MARKER-WRITTEN:${M45_MARKER}" \
    || qual_die "marker write never appeared in the guest console (Leg A)"
qual_pass "guest WROTE the marker (vda): ${M45_MARKER}"
save_console_evidence "$VM1_ID" leg-a
save_evidence "leg-a booted"

# ---------------------------------------------------------------------------
# Leg B — stop/start → the SAME bytes read back (persistence across the
# attach/detach cycle of the boot volume)
# ---------------------------------------------------------------------------
qual_info "--- Leg B: stop → start → guest reads the SAME marker back"

qual_chvctl vm stop "$VM1_ID" >/dev/null || qual_error "vm stop failed for ${VM1_ID}"
# --no-shutdown contract: graceful stop leaves the VMM retained; the guest
# reaching a terminal state is the stop-completion signal.
wait_vm_guest_stopped "$VM1_ID" || qual_die "vm did not stop (Leg B)"

# At rest: the backing persists, unchanged (data survives the stop).
[ -f "$VOL1_PATH" ] && [ "$(stat -c %s "$VOL1_PATH" 2>/dev/null)" = "$VOL1_SIZE" ] \
    && qual_pass "volume backing persists at stop (size unchanged, ${VOL1_SIZE})" \
    || qual_error "volume backing changed/disappeared at stop: ${VOL1_PATH}"

# The stord session survives the stop (the volume stays open agent-side).
[ "$(stord_sessions "$VOL1_ID")" -ge 1 ] \
    && qual_pass "stord session survives the stop (volume stays open)" \
    || qual_warn "stord session for ${VOL1_ID} gone at stop (re-open on start would be the truth to record)"

qual_chvctl vm start "$VM1_ID" >/dev/null || qual_die "vm re-start failed for ${VM1_ID}"

# Boot detection on the SECOND boot cannot count kernel banners: the
# graceful stop TRUNCATES console.log (agent: "truncated console.log on
# graceful stop") and the agent re-spawns CH quickly, so the fresh log
# may already contain boot 2's banner when a snapshot is taken (run-4
# finding: banner_before=1 with only one more boot coming → false
# timeout). Instead wait for the boot-2 evidence directly: logind lines
# in the truncated log (boot 1's are gone), then THE contract — the
# readback, which only a boot with the persisted marker file can print.
wait_for "vm ${VM1_ID}: re-booted (logind lines in the fresh console)" \
    "$LOGIND_TIMEOUT" \
    console_has "$VM1_ID" "systemd-logind" \
    || qual_die "guest did not re-boot (Leg B)"

# THE guest read: bootcmd (every boot) cats the marker the FIRST boot
# wrote — proving the bytes survived the stop/start cycle on the same
# volume. This cannot false-positive: boot 1's bootcmd ran before the
# marker existed, and boot 1's console was truncated at the stop — a
# READBACK line in the current log can only come from boot 2 reading
# the persisted file. It also proves start did NOT re-seed the volume
# (a re-seed would delete the marker file).
wait_for "guest read the marker back (console)" "$LOGIND_TIMEOUT" \
    console_has "$VM1_ID" "M45-MARKER-READBACK:${M45_MARKER}" \
    || qual_die "marker read-back never appeared in the guest console (Leg B)"
qual_pass "guest READ BACK the same marker after restart: ${M45_MARKER}"

# runcmd is per-instance: boot 2 must NOT have re-written the marker
# (its console is fresh post-truncation, so zero WRITTEN lines is the
# expected shape; any WRITTEN line means runcmd re-ran or the console
# was not truncated — either way worth recording).
WRITTEN_ON_BOOT2="$(console_count "$VM1_ID" "M45-MARKER-WRITTEN")"
if [ "$WRITTEN_ON_BOOT2" -eq 0 ]; then
    qual_pass "runcmd did not re-run on the second boot (per-instance; no re-seed)"
else
    qual_warn "M45-MARKER-WRITTEN appeared ${WRITTEN_ON_BOOT2}x in the second boot's console — runcmd re-ran (per-instance violation?) or the console was not truncated"
fi
save_console_evidence "$VM1_ID" leg-b
save_evidence "leg-b restarted"

# ---------------------------------------------------------------------------
# Leg C — interruption: stord SIGKILL under a running VM → data plane
# unaffected → supervisor recovery → NEW provisioning works (repeat)
# ---------------------------------------------------------------------------
qual_info "--- Leg C: stord SIGKILL (VM running) → recovery → fresh provisioning"

STORD_PID_BEFORE="$(stord_pid)"
[ -n "$STORD_PID_BEFORE" ] || qual_die "no stord process found (Leg C)"
kill -9 "$STORD_PID_BEFORE"
qual_info "stord (pid ${STORD_PID_BEFORE}) SIGKILLed"

# The running VM is unaffected: CH holds the disk fd; the data plane is
# kernel state (the M4.4-proven shape, now for storage).
sleep 3
[ "$(vm_state "$VM1_ID")" = "Running" ] && ! vm_ch_gone "$VM1_ID" \
    && qual_pass "running VM unaffected by stord death (CH alive, power_state Running)" \
    || qual_error "VM ${VM1_ID} disturbed by stord SIGKILL (state: $(vm_state "$VM1_ID"))"

# The agent supervisor restarts stord (like nwd — the same supervisor).
# Run-6 trap: the pid comparison must require a NON-EMPTY new pid — the
# old form (anything != old pid) passed on EMPTY output during the
# dead-old/not-yet-spawned-new window, and the follow-up /proc read of
# an empty pid resolved to /proc/cmdline (the HOST kernel's cmdline).
stord_restarted() {
    local now
    now="$(stord_pid)"
    [ -n "$now" ] && [ "$now" != "$STORD_PID_BEFORE" ]
}
wait_for "agent supervisor restarted stord" "$STORD_RESTART_TIMEOUT" \
    stord_restarted \
    || qual_die "stord was not restarted by the supervisor (Leg C)"
STORD_PID_AFTER="$(stord_pid)"
qual_pass "stord restarted by the agent supervisor (pid ${STORD_PID_BEFORE} → ${STORD_PID_AFTER})"
wait_for "restarted stord socket live" 20 stord_socket_live \
    || qual_error "restarted stord socket not accepting"

# #385/#379 truth, asserted: with deploy.sh's agent.toml setting
# stord_config_path to the operator's stord.toml, the supervisor
# respawns stord by exec'ing THAT FILE directly (the #385 pass-through)
# — runtime_dir stays the deploy's stord dir and the sessions DB does
# NOT relocate (the pre-#385 generated-config respawn relocated both;
# recorded in the evidence doc). The respawned daemon therefore keeps
# every operator key by construction; the assertions below pin the
# pass-through itself plus the confinement keys that matter
# (#376 path_allowlist, #379 device_allowlist).
# (Guarded pid → config resolution: an unguarded empty pid reads
# /proc/cmdline — the HOST kernel cmdline, run-6 finding.)
stord_config_path() {
    local pid cfg
    pid="$(stord_pid)"
    if [ -n "$pid" ] && [ -r "/proc/${pid}/cmdline" ]; then
        cfg="$(tr '\0' '\n' < "/proc/${pid}/cmdline" | tail -1)"
        if [ -f "$cfg" ]; then
            echo "$cfg"
            return 0
        fi
    fi
    echo ""
}
STORD_CFG_AFTER="$(stord_config_path)"
qual_info "respawned stord config: ${STORD_CFG_AFTER:-unresolved} (runtime_dir: $(stord_runtime_dir))"
if [ "$STORD_CFG_AFTER" = "${QUAL_TEST_DIR}/stord.toml" ]; then
    qual_pass "respawned stord execs the OPERATOR config (#385 pass-through: ${STORD_CFG_AFTER})"
    grep -E '^(path_allowlist|device_allowlist)' "$STORD_CFG_AFTER" \
        >> "${EVIDENCE_DIR}/respawned-stord-config.txt" 2>/dev/null || true
    grep -q '^path_allowlist' "$STORD_CFG_AFTER" \
        && qual_pass "respawned stord keeps path confinement (#376 fix: path_allowlist present)" \
        || qual_warn "operator stord.toml has NO path_allowlist (#376: the daemon runs allow-all after restart)"
    grep -q '^device_allowlist' "$STORD_CFG_AFTER" \
        && qual_pass "respawned stord keeps device confinement (#379: device_allowlist present)" \
        || qual_warn "operator stord.toml has NO device_allowlist (#379: device opens are unconfined)"
else
    qual_warn "respawned stord is NOT running the operator config (${STORD_CFG_AFTER:-unresolved}) — the #385 pass-through did not engage (generated-config fallback?)"
fi
# The sessions DB must stay at the deploy's stord dir (no relocation).
if [ "$(stord_runtime_dir)" = "$STORD_DIR" ]; then
    qual_pass "respawned stord runtime_dir unchanged (no DB relocation: $(stord_db))"
else
    qual_warn "respawned stord runtime_dir moved to $(stord_runtime_dir) — sessions DB relocated from ${STORD_DB}"
fi

# Recovery must serve NEW provisioning (the repeat of the contract): a
# fresh VM create goes through the restarted stord's open/attach.
VM2_ID="$(create_vm qual-stor-2 2 1024)" || qual_die "fresh provisioning failed after stord restart (Leg C)"
qual_pass "fresh vm create accepted after stord restart (qual-stor-2: ${VM2_ID})"
VOL2_ID="$(volume_id_of "$VM2_ID")"
VM2_session_open() { [ "$(stord_sessions "$VOL2_ID")" -ge 1 ]; }
[ -n "$VOL2_ID" ] && wait_for "restarted stord opened the new volume (session row present)" \
    "$DISPATCH_TIMEOUT" VM2_session_open \
    && qual_pass "restarted stord opened the new volume (session row present)" \
    || qual_error "no stord session for the new volume ${VOL2_ID}"
qual_chvctl vm start "$VM2_ID" >/dev/null || qual_die "vm start failed for ${VM2_ID}"
wait_boot "$VM2_ID" || qual_die "guest did not boot (Leg C repeat)"
save_console_evidence "$VM2_ID" leg-c

# VM-1 is still healthy through all of this.
[ "$(vm_state "$VM1_ID")" = "Running" ] \
    && qual_pass "vm-1 still Running across the whole interruption" \
    || qual_error "vm-1 state drifted during Leg C: $(vm_state "$VM1_ID")"

# Detach/close on delete (VM-2's teardown): the stord session row must
# be gone after VM delete (close_volume deletes the session row).
qual_chvctl vm delete "$VM2_ID" >/dev/null || qual_error "vm delete failed for ${VM2_ID}"
wait_vm_stopped "$VM2_ID" || qual_error "vm ${VM2_ID} did not stop for delete"
wait_for "vm ${VM2_ID}: CH gone" 30 vm_ch_gone "$VM2_ID" || true
vm2_session_closed() { [ "$(stord_sessions "$VOL2_ID")" = "0" ]; }
wait_for "stord session for ${VOL2_ID} closed on delete" "$DISPATCH_TIMEOUT" \
    vm2_session_closed \
    && qual_pass "stord session closed on VM delete (detach/close contract)" \
    || qual_error "stord session for ${VOL2_ID} NOT closed on VM delete"
save_evidence "leg-c recovered"

# ---------------------------------------------------------------------------
# Leg D — snapshot + clone of the (stopped) boot volume: the advertised
# operator surface. TRUTH on core-managed nodes since #495 (#378): all
# four volume-op surfaces are REJECTED AT ACCEPT TIME — the CP lifecycle
# handler returns InvalidArgument (→ HTTP 400 through the BFF's map_ack;
# chvctl prints the message) BEFORE create_operation_and_emit, so a
# rejected request leaves no operations row, no volume_desired_state
# intent and (clone) no target volume row. The node's mode comes from
# the agent's inventory (authority_mode carrier, #495); the check fails
# OPEN on an unknown mode, and the agent's fail-closed dispatch
# (Unimplemented — M2.2b single-writer enforcement) remains the
# enforcement backstop, now unreachable through this surface. This leg
# asserts the new boundary: immediate 400 with the agreed message + no
# journaled state + no dispatch + NO side effect on the host.
# ---------------------------------------------------------------------------
qual_info "--- Leg D: snapshot + clone — rejected at accept time on core-managed nodes (#378, fixed by #495)"

# The would-be snapshot/clone destination is the LIVE stord's runtime_dir
# (after Leg C's restart: the deploy's stord dir, #385 pass-through) —
# used for the NO-side-effect assertions below.
STORD_LIVE_DIR="$(stord_runtime_dir)"
qual_info "live stord runtime_dir: ${STORD_LIVE_DIR}"

qual_chvctl vm stop "$VM1_ID" >/dev/null || qual_error "vm stop failed for ${VM1_ID}"
wait_vm_guest_stopped "$VM1_ID" || qual_die "vm did not stop (Leg D)"

# Snapshot: rejected at accept time — chvctl exits non-zero with the
# CP's InvalidArgument message surfaced by the BFF's map_ack (the #495
# contract text; the m4.4 DEL_OUT/RC pattern for asserted CLI failures).
SNAP_NAME="m45snap"
SNAP_OUT="$(qual_chvctl volume snapshot "$VOL1_ID" --name "$SNAP_NAME" 2>&1)" && RC=0 || RC=$?
if [ "$RC" -ne 0 ] && printf '%s' "$SNAP_OUT" | grep -q "volume snapshot is not supported on core-managed nodes"; then
    qual_pass "chvctl volume snapshot rejected at accept time (${VOL1_ID} → ${SNAP_NAME}): ${SNAP_OUT}"
elif [ "$RC" -ne 0 ]; then
    qual_error "chvctl volume snapshot failed with the WRONG error (expected the #495 core-managed rejection): ${SNAP_OUT}"
else
    qual_error "chvctl volume snapshot ACCEPTED on a core-managed node — the pre-#495 accept-then-fail-closed shape is back (architecture violation!)"
fi

# The rejection must be PRE-JOURNAL: no operations row and no
# volume_desired_state snapshot intent for the attempt. The settle gives
# a journal-then-reject regression time to show up before the absence
# assertions run (an immediate check could false-pass against one).
sleep 5
SNAP_OPS="$(sqlite_query "$QUAL_DB" \
    "SELECT COUNT(*) FROM operations WHERE resource_id='${VOL1_ID}' AND operation_type IN ('SnapshotVolume','DeleteVolumeSnapshot')" 2>/dev/null | head -1)"
[ "${SNAP_OPS:-}" = "0" ] \
    && qual_pass "no operations row for the rejected snapshot (SnapshotVolume/DeleteVolumeSnapshot count=0)" \
    || qual_error "operations row(s) journaled for the rejected snapshot (count='${SNAP_OPS}') — the rejection was NOT pre-journal (#495 regression)"
SNAP_OP_NOW="$(sqlite_query "$QUAL_DB" \
    "SELECT snapshot_op FROM volume_desired_state WHERE volume_id='${VOL1_ID}'" 2>/dev/null | head -1)"
if [ -z "${SNAP_OP_NOW}" ]; then
    qual_pass "no snapshot intent journaled (volume_desired_state.snapshot_op stays empty for ${VOL1_ID})"
else
    qual_error "snapshot intent journaled despite the rejection (snapshot_op='${SNAP_OP_NOW}') — the rejection was NOT pre-journal (#495 regression)"
fi

# No dispatch ever happens: the agent's fail-closed refusal line must
# NOT appear in the CP log — nothing was journaled for the orchestrator
# to claim and dispatch.
cp_log_has() { grep -aq "$1" "${QUAL_LOGS_DIR}/controlplane.log"; }
if cp_log_has "snapshot_volume is unsupported in core-managed mode"; then
    qual_error "dispatch-refusal line in the CP log — a snapshot WAS dispatched to the agent (the pre-#495 accept-then-fail shape is back)"
else
    qual_pass "no snapshot dispatch-refusal in the CP log (nothing was dispatched)"
fi

SNAP_FILE="${STORD_LIVE_DIR}/${VOL1_ID}-${SNAP_NAME}.img"
sleep 10
[ ! -e "$SNAP_FILE" ] \
    && qual_pass "no snapshot file materialized (rejected pre-dispatch: no side effect on the host)" \
    || qual_error "snapshot file MATERIALIZED on a core-managed node: ${SNAP_FILE} (architecture violation!)"

# Clone: the target is a NEW volume id (the #372 contract), rejected at
# accept time against the SOURCE's placement node (#495: clone checks
# the node the target would materialize on and the orchestrator would
# dispatch to). NOTE: the CP's ResourceId caps ids at 16 BYTES
# (lifecycle.rs parse_volume_id) — run-6 finding: keep the id short so
# the ONLY rejection reason can be the core-managed mode check.
CLONE_ID="clone$$"
CLONE_OUT="$(qual_chvctl volume clone "$VOL1_ID" --name "$CLONE_ID" 2>&1)" && RC=0 || RC=$?
if [ "$RC" -ne 0 ] && printf '%s' "$CLONE_OUT" | grep -q "volume clone is not supported on core-managed nodes"; then
    qual_pass "chvctl volume clone rejected at accept time (${VOL1_ID} → ${CLONE_ID}): ${CLONE_OUT}"
elif [ "$RC" -ne 0 ]; then
    qual_error "chvctl volume clone failed with the WRONG error (expected the #495 core-managed rejection): ${CLONE_OUT}"
else
    qual_error "chvctl volume clone ACCEPTED on a core-managed node — the pre-#495 accept-then-fail-closed shape is back (architecture violation!)"
fi

# No residue for the rejected clone: no CloneVolume operation row (its
# resource_id is the TARGET), no target volumes row, no
# volume_desired_state intent for the target. (#387's owner-inheritance
# contract is unreachable on a core-managed node — no target row is
# ever created; that assertion belongs to a legacy-mode clone leg.)
sleep 5
CLONE_OPS="$(sqlite_query "$QUAL_DB" \
    "SELECT COUNT(*) FROM operations WHERE resource_id='${CLONE_ID}' AND operation_type='CloneVolume'" 2>/dev/null | head -1)"
[ "${CLONE_OPS:-}" = "0" ] \
    && qual_pass "no CloneVolume operations row for the rejected clone" \
    || qual_error "CloneVolume operations row journaled despite the rejection (count='${CLONE_OPS}') — the rejection was NOT pre-journal (#495 regression)"
CLONE_VOL_ROW="$(sqlite_query "$QUAL_DB" \
    "SELECT COUNT(*) FROM volumes WHERE volume_id='${CLONE_ID}'" 2>/dev/null | head -1)"
CLONE_DS_ROW="$(sqlite_query "$QUAL_DB" \
    "SELECT COUNT(*) FROM volume_desired_state WHERE volume_id='${CLONE_ID}'" 2>/dev/null | head -1)"
if [ "${CLONE_VOL_ROW:-x}" = "0" ] && [ "${CLONE_DS_ROW:-x}" = "0" ]; then
    qual_pass "no target volume row / desired-state row for the rejected clone (${CLONE_ID} never existed)"
else
    qual_error "clone target rows journaled despite the rejection (volumes='${CLONE_VOL_ROW}', volume_desired_state='${CLONE_DS_ROW}') — the rejection was NOT pre-journal (#495 regression)"
fi
sleep 10
CLONE_FILE="$(find "$STORD_LIVE_DIR" -maxdepth 1 -name "*${CLONE_ID}*.img" -type f 2>/dev/null | head -1)"
[ -z "$CLONE_FILE" ] \
    && qual_pass "no clone file materialized (rejected pre-dispatch: no side effect on the host)" \
    || qual_error "clone file MATERIALIZED on a core-managed node: ${CLONE_FILE} (architecture violation!)"

# Delete the snapshot through the BFF (chvctl has no delete-snapshot
# command — recorded; the route exists). Same truth: rejected at accept
# time — HTTP 400 with the CP's message (#495 closed the old accepted-
# then-~70s-dispatch-retry UX gap; the response body carries the
# message, saved by bff_post_json).
DELETE_BODY="$(mktemp "${QUAL_TEST_DIR}/m45-del-snap.XXXXXX")"
echo "{\"volume_id\":\"${VOL1_ID}\",\"snapshot_name\":\"${SNAP_NAME}\"}" > "$DELETE_BODY"
HTTP_CODE="$(bff_post_json "/v1/volumes/delete-snapshot" "$DELETE_BODY")"
rm -f "$DELETE_BODY"
if [ "$HTTP_CODE" = "400" ] && grep -q "volume snapshot deletion is not supported on core-managed nodes" \
        "${EVIDENCE_DIR}/bff-delete-snapshot.json" 2>/dev/null; then
    qual_pass "BFF delete-snapshot rejected at accept time (HTTP ${HTTP_CODE} + the #495 message)"
else
    qual_error "BFF delete-snapshot not rejected as contracted (HTTP ${HTTP_CODE}): $(cat "${EVIDENCE_DIR}/bff-delete-snapshot.json" 2>/dev/null)"
fi
# Comprehensive-review follow-up (carried): assert the DB trail, not
# just the HTTP code — the rejected delete must leave snapshot_op EMPTY
# (the pre-#495 leg asserted the create → delete flip; post-#495 BOTH
# intents stay unjournaled). Absence of the file is trivially true
# (nothing was ever created), so the DB row is the load-bearing check.
sleep 5
SNAP_OP_AFTER_DEL="$(sqlite_query "$QUAL_DB" \
    "SELECT snapshot_op FROM volume_desired_state WHERE volume_id='${VOL1_ID}'" 2>/dev/null | head -1)"
if [ -z "${SNAP_OP_AFTER_DEL}" ]; then
    qual_pass "delete intent NOT journaled (snapshot_op stays empty for ${VOL1_ID})"
else
    qual_error "snapshot_op='${SNAP_OP_AFTER_DEL}' after the rejected delete-snapshot — the rejection was NOT pre-journal (#495 regression)"
fi
sleep 5
[ ! -e "${SNAP_FILE}" ] \
    && qual_pass "still no snapshot file after delete-snapshot (no side effect)" \
    || qual_error "snapshot file MATERIALIZED after delete-snapshot: ${SNAP_FILE}"
save_evidence "leg-d volume-ops rejected"

# ---------------------------------------------------------------------------
# Leg E — cleanup: VM delete → stord session closed → residue assertions
# ---------------------------------------------------------------------------
qual_info "--- Leg E: vm delete → session closed → no residue"

qual_chvctl vm delete "$VM1_ID" >/dev/null || qual_error "vm delete failed for ${VM1_ID}"
wait_vm_stopped "$VM1_ID" || qual_error "vm ${VM1_ID} did not stop for delete"
wait_for "vm ${VM1_ID}: CH gone" 30 vm_ch_gone "$VM1_ID" || true

vm1_session_closed() { [ "$(stord_sessions "$VOL1_ID")" = "0" ]; }
wait_for "stord session for ${VOL1_ID} closed on delete" "$DISPATCH_TIMEOUT" \
    vm1_session_closed \
    && qual_pass "stord session closed on VM delete" \
    || qual_error "stord session for ${VOL1_ID} NOT closed on VM delete"

# The volume row: cascade or retention — record the truth.
VOL1_ROW="$(sqlite_query "$QUAL_DB" \
    "SELECT COUNT(*) FROM volumes WHERE volume_id='${VOL1_ID}'" 2>/dev/null | head -1)"
[ "$VOL1_ROW" = "0" ] \
    && qual_pass "volume row removed with the VM (cascade)" \
    || qual_warn "volume row retained after VM delete (${VOL1_ROW} row) — M2.5 residual class"

# The volume backing: the M2.5 residual was retention. Record the truth.
if [ -f "$VOL1_PATH" ]; then
    qual_warn "volume backing RETAINED on VM delete (known M2.5 residual): ${VOL1_PATH}"
elif [ -d "${VMS_DIR}/${VM1_ID}" ]; then
    qual_pass "volume backing removed on VM delete (vm dir retained: $(ls "${VMS_DIR}/${VM1_ID}" 2>/dev/null | tr '\n' ' '))"
else
    qual_pass "vm dir fully removed on delete"
fi

# Forbidden outcomes: no CH processes, no leaked conversion processes,
# no stray stord sessions at all.
[ -z "$(pgrep -f "(^|/)cloud-hypervisor( |$).*vms/" 2>/dev/null)" ] \
    && qual_pass "no CH processes remain" \
    || qual_error "CH processes remain: $(pgrep -af cloud-hypervisor | head -3)"
[ -z "$(pgrep -x qemu-img 2>/dev/null)" ] \
    && qual_pass "no leaked qemu-img conversion processes" \
    || qual_error "qemu-img still running: $(pgrep -a qemu-img)"
STORD_SESSIONS_LEFT="$(sqlite_query "$(stord_db)" \
    "SELECT COUNT(*) FROM sessions" 2>/dev/null | head -1)"
# No ':-0' default here (review of #382): an empty result (DB read
# failure, relocated DB) must NOT read as "zero sessions" — that would
# be a vacuous pass. Empty fails loud.
if [ "${STORD_SESSIONS_LEFT}" = "0" ]; then
    qual_pass "no stord sessions remain"
elif [ -z "${STORD_SESSIONS_LEFT}" ]; then
    qual_error "could not read the live stord sessions DB ($(stord_db)) — residue state unknown"
else
    qual_warn "stord sessions remain (${STORD_SESSIONS_LEFT}): $(sqlite_query "$(stord_db)" 'SELECT volume_id, runtime_status FROM sessions' 2>/dev/null | head -3)"
fi
save_evidence "leg-e cleaned"

# ---------------------------------------------------------------------------
# Leg F — LVM profile (stord layer): real-LVM contract via the root-gated
# integration tests, on a harness-provisioned loopback VG
# ---------------------------------------------------------------------------
qual_info "--- Leg F: LVM (stord layer) — loopback PV → VG → LV, real-LVM backend contract"

for tool in losetup pvcreate vgcreate lvcreate blockdev lvs cargo; do
    command -v "$tool" >/dev/null 2>&1 \
        || qual_die "LVM leg needs ${tool} on the qualification host"
done

# Identity guard (host-safety pattern): building the tests from THIS tree
# only tests the deployed build if chv-stord-backends is identical.
CANDIDATE_SHA="$(cat "${QUAL_BINARY_DIR}/CANDIDATE_SHA" 2>/dev/null || true)"
if [ -n "$CANDIDATE_SHA" ]; then
    if git -C "$REPO_ROOT" diff --quiet "$CANDIDATE_SHA" HEAD -- crates/chv-stord-backends; then
        qual_pass "candidate identity: crates/chv-stord-backends identical at ${CANDIDATE_SHA:0:8} and HEAD"
    else
        qual_die "crates/chv-stord-backends DIFFERS between candidate ${CANDIDATE_SHA:0:8} and HEAD — building from this tree would not test the candidate"
    fi
else
    qual_warn "no candidate sha available — identity check skipped; results apply to THIS tree only"
fi

# Baseline for the residue assertions.
LOOPS_BEFORE="$(losetup -a 2>/dev/null | cut -d: -f1 | sort)"

# Build the test binary (host-safety pattern). NOTE: the pipeline's
# non-zero status relies on `set -o pipefail` (line ~55) — without it
# `tail -5` would mask a cargo failure and the find below could pick a
# STALE test binary from a previous build. Do not remove pipefail.
qual_info "building chv-stord-backends test binaries (cargo test --no-run)..."
if ! (cd "$REPO_ROOT" && cargo test -p chv-stord-backends --no-run 2>&1 | tail -5); then
    qual_die "cargo test --no-run failed for chv-stord-backends"
fi
TEST_BIN="$(find "${REPO_ROOT}/target/debug/deps" -maxdepth 1 -name 'lvm_real-*' -type f -executable -printf '%T@ %p\n' 2>/dev/null | sort -nr | head -1 | cut -d' ' -f2-)"
[ -n "$TEST_BIN" ] || qual_die "could not locate the lvm_real test binary under target/debug/deps"
qual_info "test binary: ${TEST_BIN}"

# Provision the disposable VG (the operator model: stord consumes
# pre-provisioned volumes; provisioning is out-of-band by design).
# Interrupt safety (review of #382): from loop attach until the explicit
# teardown below, an INT/TERM must not leak the loop device + VG + backing
# file — the residue assertions only run on normal completion. The cleanup
# is idempotent and best-effort (nothing here can touch host LVM state:
# the VG name is uniquely ours).
LVM_BACKING="${QUAL_TEST_DIR}/m45-lvm-backing.img"
LVM_LOOP=""
# Idempotent, best-effort cleanup — safe on ANY exit path and safe to run
# twice. Nothing here can touch host LVM state: the VG name is uniquely
# ours (see the LVM_VG comment above).
lvm_leg_cleanup() {
    vgremove -f "${LVM_VG}" >/dev/null 2>&1 || true
    [ -n "${LVM_LOOP}" ] && losetup -d "${LVM_LOOP}" >/dev/null 2>&1 || true
    rm -f "${LVM_BACKING}" 2>/dev/null || true
}
# From backing-file creation until the explicit teardown below, ANY exit —
# a provisioning qual_die (no free loop, pvcreate/vgcreate failure), an
# INT/TERM, or a later failure — must not leak the backing file, loop
# device, or a half-built VG. The EXIT trap owns cleanup on every exit
# path; INT/TERM route through `exit 1` so it fires exactly once.
# (Comprehensive-review follow-up: the previous INT/TERM-only trap missed
# the qual_die provisioning failures — exactly where residue is created,
# and a half-failed vgcreate left the FIXED VG name behind, poisoning the
# next run's vgcreate.)
trap lvm_leg_cleanup EXIT
trap 'exit 1' INT TERM
truncate -s "${LVM_BACKING_MB}M" "$LVM_BACKING"
LVM_LOOP="$(losetup -f --show "$LVM_BACKING")"
[ -n "$LVM_LOOP" ] || qual_die "no free loop device"
pvcreate -f "$LVM_LOOP" >/dev/null 2>&1 || qual_die "pvcreate failed"
vgcreate "$LVM_VG" "$LVM_LOOP" >/dev/null 2>&1 || qual_die "vgcreate failed"
qual_pass "loopback VG provisioned: ${LVM_LOOP} → ${LVM_VG} ($(vgs "$LVM_VG" --noheadings -o vg_size 2>/dev/null | tr -d ' '))"

# Run the real-LVM contract tests serially (snapshots claim 100%FREE).
# The three #379 DP2 tests (create-on-open + its two refusals) run with
# the contract suite — they are the new provisioning contract.
LVM_TESTS="lvm_real_open_export_and_block_roundtrip lvm_real_open_rejects_wrong_class \
lvm_real_open_provisions_absent_lv_with_size lvm_real_open_refuses_absent_lv_without_size \
lvm_real_open_rejects_seed_from lvm_real_snapshot_is_copy_on_write lvm_real_clone_copies_data \
lvm_real_resize_grows_volume lvm_real_read_only_policy_blocks_writes lvm_real_health_reflects_existence"
LVM_FAILED=0
for test_name in $LVM_TESTS; do
    qual_info "running ${test_name} (ignored/root-gated real-LVM test)..."
    if CHV_LVM_TEST_VG="$LVM_VG" "$TEST_BIN" --ignored --exact --nocapture "$test_name" \
        > "${EVIDENCE_DIR}/lvm-${test_name}.log" 2>&1; then
        qual_pass "lvm-real: ${test_name}"
    else
        qual_error "lvm-real: ${test_name} FAILED (see ${EVIDENCE_DIR}/lvm-${test_name}.log)"
        LVM_FAILED=1
    fi
done

# Teardown + residue assertions (forbidden outcomes).
vgremove -f "$LVM_VG" >/dev/null 2>&1 || qual_error "vgremove failed"
pvremove "$LVM_LOOP" >/dev/null 2>&1 || qual_error "pvremove failed"
losetup -d "$LVM_LOOP" 2>/dev/null || qual_error "losetup -d failed"
rm -f "$LVM_BACKING"
trap - INT TERM EXIT   # explicit teardown done — no exit path needs the LVM cleanup anymore

[ -z "$(vgs --noheadings -o vg_name 2>/dev/null | grep -x "$LVM_VG")" ] \
    && qual_pass "no VG residue (${LVM_VG} removed)" \
    || qual_error "VG ${LVM_VG} still present"
[ -z "$(lvs --noheadings -o lv_name,vg_name 2>/dev/null | grep "$LVM_VG")" ] \
    && qual_pass "no LV residue in ${LVM_VG}" \
    || qual_error "LVs remain in ${LVM_VG}: $(lvs "$LVM_VG" --noheadings 2>/dev/null)"
losetup "$LVM_LOOP" >/dev/null 2>&1 \
    && qual_error "loop device ${LVM_LOOP} still attached" \
    || qual_pass "loop device ${LVM_LOOP} detached"
LOOPS_AFTER="$(losetup -a 2>/dev/null | cut -d: -f1 | sort)"
NEW_LOOPS="$(comm -13 <(printf '%s\n' "$LOOPS_BEFORE") <(printf '%s\n' "$LOOPS_AFTER") | grep -v '^$' || true)"
[ -z "$NEW_LOOPS" ] \
    && qual_pass "no new loop devices remain" \
    || qual_error "new loop devices remain: ${NEW_LOOPS}"
[ ! -f "$LVM_BACKING" ] \
    && qual_pass "loopback backing file removed" \
    || qual_error "backing file remains: ${LVM_BACKING}"

[ "$LVM_FAILED" = "0" ] \
    && qual_pass "LVM backend contract proven on real LVM (10 root-gated tests)" \
    || qual_error "LVM backend contract has failures"

save_evidence "leg-f lvm done"

# ---------------------------------------------------------------------------
# Leg G — LVM profile (VM-integrated, #379): node backend flip → lvm-class
# VM create (DP2 create-on-open) → out-of-band seed → boot → persist →
# teardown. The agent restart is the m4.3-proven shape and is REQUIRED
# here: the agent parses stord_config_path at startup (DP4 inventory
# reporting + DP5 locator shaping), so flipping stord alone would leave
# the agent reporting/shaping against a backend class it no longer
# serves. Seeding is out-of-band (qemu-img convert onto the LV) — LVM
# has no seed path by design (DP2 scope cut, documented in
# docs/OPERATIONS.md); LV reclamation on VM delete is out-of-band too
# (stord closes the session but never lvremoves).
# ---------------------------------------------------------------------------
qual_info "--- Leg G: LVM (VM-integrated) — backend flip → lvm-class vm create → out-of-band seed → boot/persist"

command -v qemu-img >/dev/null 2>&1 \
    || qual_die "Leg G needs qemu-img on the qualification host"

# stop_daemon PID — SIGTERM, wait up to 10 s, then SIGKILL (m4.3's shape).
stop_daemon() {
    local pid="$1"
    kill "$pid" 2>/dev/null || true
    for _ in $(seq 1 50); do
        kill -0 "$pid" 2>/dev/null || return 0
        sleep 0.2
    done
    kill -9 "$pid" 2>/dev/null || true
}

# 1. Provision Leg G's OWN loopback VG FIRST — stord's DP2 fail-closed
#    startup guard (verify_volume_group) aborts the daemon when
#    backend_type=lvm names a VG that does not exist; provisioning the
#    VG before the flip keeps the respawn thrash-free.
LVM_BACKING_G="${QUAL_TEST_DIR}/m45-lvm-g-backing.img"
LVM_LOOP_G=""
VOL3_ID=""
VOL3_LV=""
LOOPS_BEFORE_G="$(losetup -a 2>/dev/null | cut -d: -f1 | sort)"
# Idempotent, best-effort cleanup on ANY exit path (Leg F's pattern).
lvm_g_cleanup() {
    [ -n "$VOL3_LV" ] && lvremove -f "$VOL3_LV" >/dev/null 2>&1 || true
    vgremove -f "${LVM_VG_G}" >/dev/null 2>&1 || true
    [ -n "${LVM_LOOP_G}" ] && losetup -d "${LVM_LOOP_G}" >/dev/null 2>&1 || true
    rm -f "${LVM_BACKING_G}" 2>/dev/null || true
}
trap lvm_g_cleanup EXIT
trap 'exit 1' INT TERM
truncate -s "${LVM_BACKING_GB_G}G" "$LVM_BACKING_G"
LVM_LOOP_G="$(losetup -f --show "$LVM_BACKING_G")"
[ -n "$LVM_LOOP_G" ] || qual_die "no free loop device (Leg G)"
pvcreate -f "$LVM_LOOP_G" >/dev/null 2>&1 || qual_die "pvcreate failed (Leg G)"
vgcreate "$LVM_VG_G" "$LVM_LOOP_G" >/dev/null 2>&1 || qual_die "vgcreate failed (Leg G)"
qual_pass "Leg G loopback VG provisioned: ${LVM_LOOP_G} → ${LVM_VG_G}"

# 2. Flip the operator stord.toml to the LVM shape (backup first). The
#    flip only APPENDS backend_type/lvm_volume_group, so every deploy
#    key survives into the LVM daemon by construction — including the
#    #376 path_allowlist and the #379 device_allowlist the Leg G open
#    must pass.
STORD_TOML="${QUAL_TEST_DIR}/stord.toml"
cp "$STORD_TOML" "${QUAL_TEST_DIR}/stord.toml.local-backup"
printf '\n# m4.5 Leg G (#379): LVM flip, appended on top of the deploy shape\nbackend_type = "lvm"\nlvm_volume_group = "%s"\n' \
    "$LVM_VG_G" >> "$STORD_TOML"
qual_pass "operator stord.toml flipped to backend_type=lvm (vg ${LVM_VG_G}; backup: stord.toml.local-backup)"

# 3. Restart the agent: its shutdown kills the supervised stord, and its
#    startup respawns stord from stord_config_path — now the LVM shape,
#    through the DP2 vgs guard — while the agent itself re-parses the
#    same file (DP4 inventory + DP5 locator shaping). A still-listening
#    stord is stopped explicitly: the supervisor ADOPTS a live socket
#    and would otherwise keep the local-class daemon serving.
AGENT_PID_G_BEFORE="$QUAL_AGENT_PID"
stop_daemon "$QUAL_AGENT_PID"
STORD_PID_G="$(stord_pid)"
[ -z "$STORD_PID_G" ] || stop_daemon "$STORD_PID_G"
rm -f "${STORD_DIR}/api.sock" "${QUAL_AGENT_DIR}/api.sock" "${QUAL_AGENT_DIR}/core.sock"
"${QUAL_BINARY_DIR}/chv-agent" "${QUAL_TEST_DIR}/agent.toml" \
    >> "${QUAL_LOGS_DIR}/agent.log" 2>&1 &
QUAL_AGENT_PID=$!
pids_current
wait_for "agent gRPC socket up after LVM restart" 60 \
    test -S "${QUAL_AGENT_DIR}/api.sock" \
    || qual_die "agent did not come back (Leg G) — log: $(tail -30 "${QUAL_LOGS_DIR}/agent.log")"
wait_for "stord respawned from the LVM config (DP2 vgs guard passed)" \
    "$STORD_RESTART_TIMEOUT" stord_socket_live \
    || qual_die "stord did not come back on the LVM config (Leg G) — agent log: $(tail -30 "${QUAL_LOGS_DIR}/agent.log")"
qual_pass "agent + stord restarted on the LVM config (agent pid ${AGENT_PID_G_BEFORE} → ${QUAL_AGENT_PID})"

# The respawned stord must exec the operator config and run the LVM
# backend (its startup line lands in agent.log via inherited stdio —
# supervisor.rs M4.4 lesson).
STORD_CFG_G="$(stord_config_path)"
[ "$STORD_CFG_G" = "$STORD_TOML" ] \
    && qual_pass "LVM stord execs the operator config (${STORD_CFG_G})" \
    || qual_error "LVM stord is NOT the operator config: ${STORD_CFG_G:-unresolved}"
# tracing renders a string field as backend_type="lvm" and interleaves ANSI
# escapes around the key/value, so match the quoted value with a wildcard
# rather than the literal backend_type=lvm form (which never matches).
grep -aqE 'backend_type.*"lvm"' "${QUAL_LOGS_DIR}/agent.log" \
    && qual_pass "stord initialized the LVM backend (backend_type=\"lvm\" in agent.log)" \
    || qual_error "no backend_type=\"lvm\" startup line in agent.log — the respawned stord is not the LVM daemon"

# DP4: the node's inventory must report the REAL backend class.
node_reports_lvm() {
    [ "$(sqlite_query "$QUAL_DB" \
        "SELECT storage_classes FROM node_inventory WHERE node_id='${QUAL_NODE_ID}'" 2>/dev/null | head -1)" = '["lvm"]' ]
}
wait_for "node inventory reports storage_classes=[\"lvm\"] (DP4)" 180 node_reports_lvm \
    && qual_pass "node inventory reports the real backend class (DP4: [\"lvm\"])" \
    || qual_error "node inventory never reported lvm (DP4): $(sqlite_query "$QUAL_DB" "SELECT storage_classes FROM node_inventory WHERE node_id='${QUAL_NODE_ID}'" 2>/dev/null | head -1)"
save_evidence "leg-g backend flipped"

# 4. lvm-class VM create: storage_class=lvm boot volume, image_ref
#    "default" (NO seed — the LVM contract has none), volume_size_gb
#    sized above the guest image's 3.5 GiB virtual size.
create_vm_lvm() {
    local name="$1" cpu="$2" mem="$3" size_gb="$4" payload http
    payload="$(python3 - "$name" "$cpu" "$mem" "$size_gb" "$M45_MARKER" <<'PYEOF'
import json, sys
name, cpu, mem, size_gb, marker = sys.argv[1:6]
userdata = (
    "#cloud-config\n"
    "bootcmd:\n"
    "  - [ sh, -c, 'if [ -f /var/lib/m45.marker ]; then "
    "echo \"M45-MARKER-READBACK:$(cat /var/lib/m45.marker)\" > /dev/console; fi' ]\n"
    "runcmd:\n"
    "  - [ sh, -c, "
    f"'echo \"{marker}\" > /var/lib/m45.marker; "
    "echo \"M45-MARKER-WRITTEN:$(cat /var/lib/m45.marker)\" > /dev/console' ]\n"
)
print(json.dumps({
    "name": name,
    "cpu_count": int(cpu),
    "memory_mb": int(mem),
    "image_ref": "default",
    "network_id": "default",
    "storage_class": "lvm",
    "volume_size_gb": int(size_gb),
    "cloud_init_userdata": userdata,
}))
PYEOF
)" || { qual_error "could not build lvm vm-create payload for ${name}"; return 1; }
    http="$(curl -s -o "${EVIDENCE_DIR}/vm-create-${name}.json" -w '%{http_code}' \
        -X POST "${QUAL_BFF_URL}/v1/vms/create" \
        -H "Authorization: Bearer $(bff_token)" -H "Content-Type: application/json" \
        -d "$payload")"
    [ "$http" = "200" ] \
        || { qual_error "BFF vm create ${name} failed (HTTP ${http}): $(cat "${EVIDENCE_DIR}/vm-create-${name}.json" 2>/dev/null)"; return 1; }
    python3 -c '
import json, sys
data = json.load(open(sys.argv[1]))
print(data.get("vm_id") or data.get("id") or "")' "${EVIDENCE_DIR}/vm-create-${name}.json"
}

VM3_ID="$(create_vm_lvm qual-stor-lvm 2 1024 4)" || qual_die "lvm-class vm create failed (Leg G)"
qual_pass "lvm-class vm created: qual-stor-lvm (${VM3_ID})"
VOL3_ID="$(volume_id_of "$VM3_ID")"
[ -n "$VOL3_ID" ] || qual_die "no volume in volume_desired_state for ${VM3_ID} (Leg G)"
VOL3_CLASS="$(volume_field "$VOL3_ID" storage_class)"
[ "$VOL3_CLASS" = "lvm" ] \
    && qual_pass "boot volume carries storage_class=lvm in the CP DB (${VOL3_ID})" \
    || qual_error "boot volume storage_class is '${VOL3_CLASS:-NULL}', expected lvm"

# 5. DP2 create-on-open: the reconcile dispatch carries size_bytes and
#    the LVM backend provisions the ABSENT LV (lvcreate). The block
#    device appearing IS the proof — no operator pre-provisioning ran.
VOL3_LV="/dev/${LVM_VG_G}/${VOL3_ID}"
lv_provisioned() { [ -b "$VOL3_LV" ]; }
wait_for "boot LV provisioned via DP2 create-on-open (${VOL3_LV})" \
    "$DISPATCH_TIMEOUT" lv_provisioned \
    && qual_pass "boot LV provisioned by create-on-open: ${VOL3_LV}" \
    || qual_die "LV never materialized (DP2 create-on-open): ${VOL3_LV}"
LV3_SIZE_BYTES="$(blockdev --getsize64 "$VOL3_LV" 2>/dev/null || echo 0)"
[ "$LV3_SIZE_BYTES" -ge 4294967296 ] \
    && qual_pass "LV size honors the requested capacity (${LV3_SIZE_BYTES} bytes ≥ 4 GiB)" \
    || qual_warn "LV size ${LV3_SIZE_BYTES} < requested 4 GiB"

# The stord session: the open went through the dm-path locator
# (/dev/mapper/{vg}-{vid}, DP5) and the device_allowlist admitted it —
# a denied locator would have failed the open (and the LV would never
# have been provisioned).
stord_session_open_g() { [ "$(stord_sessions "$VOL3_ID")" -ge 1 ]; }
wait_for "stord session open for the LV volume (DP5 locator passed the device_allowlist)" \
    "$DISPATCH_TIMEOUT" stord_session_open_g \
    && qual_pass "stord session open (dm-path locator admitted by device_allowlist)" \
    || qual_error "no stord session row for ${VOL3_ID}"
{
    echo "### leg-g lvs ($(date -u +%FT%TZ))"
    lvs "$LVM_VG_G" --noheadings 2>/dev/null || true
} >> "${EVIDENCE_DIR}/host-state.txt"
save_evidence "leg-g lvm volume provisioned"

# 6. Out-of-band seeding (the documented operator model): the VM is
#    Stopped and nothing re-opens volumes on start (Core uses the
#    persisted config), so converting the guest image straight onto the
#    LV is safe. qemu-img convert respects the target's bounds — the
#    4 GiB LV holds the image's 3.5 GiB virtual size.
qemu-img convert -O raw "$GUEST_IMAGE_PATH" "$VOL3_LV" \
    || qual_die "out-of-band seed (qemu-img convert) failed onto ${VOL3_LV}"
qual_pass "boot LV seeded out-of-band (qemu-img convert → ${VOL3_LV})"

# 7. Boot from the LV + guest WRITE (the marker, on vda = the LV).
qual_chvctl vm start "$VM3_ID" >/dev/null || qual_die "vm start failed for ${VM3_ID} (Leg G)"
wait_boot "$VM3_ID" || qual_die "guest did not boot from the LVM boot volume (Leg G)"
wait_for "guest wrote the marker on the LVM boot volume (console)" "$LOGIND_TIMEOUT" \
    console_has "$VM3_ID" "M45-MARKER-WRITTEN:${M45_MARKER}" \
    || qual_die "marker write never appeared in the guest console (Leg G)"
qual_pass "guest WROTE the marker on the LVM boot volume: ${M45_MARKER}"
save_console_evidence "$VM3_ID" leg-g

# 8. stop → start → the SAME bytes read back (LV persistence across the
#    attach/detach cycle).
qual_chvctl vm stop "$VM3_ID" >/dev/null || qual_error "vm stop failed for ${VM3_ID} (Leg G)"
wait_vm_guest_stopped "$VM3_ID" || qual_die "vm did not stop (Leg G)"
qual_chvctl vm start "$VM3_ID" >/dev/null || qual_die "vm re-start failed for ${VM3_ID} (Leg G)"
wait_for "vm ${VM3_ID}: re-booted (logind lines in the fresh console)" \
    "$LOGIND_TIMEOUT" \
    console_has "$VM3_ID" "systemd-logind" \
    || qual_die "guest did not re-boot (Leg G)"
wait_for "guest read the marker back (LVM console)" "$LOGIND_TIMEOUT" \
    console_has "$VM3_ID" "M45-MARKER-READBACK:${M45_MARKER}" \
    || qual_die "marker read-back never appeared in the guest console (Leg G)"
qual_pass "guest READ BACK the same marker after restart on the LV: ${M45_MARKER}"
save_console_evidence "$VM3_ID" leg-g-restart

# 9. Delete → session closed; the LV itself REMAINS (stord never
#    lvremoves — the out-of-band reclamation model; asserted, then
#    cleaned up with the VG below).
qual_chvctl vm delete "$VM3_ID" >/dev/null || qual_error "vm delete failed for ${VM3_ID} (Leg G)"
wait_vm_stopped "$VM3_ID" || qual_error "vm ${VM3_ID} did not stop for delete (Leg G)"
wait_for "vm ${VM3_ID}: CH gone" 30 vm_ch_gone "$VM3_ID" || true
vm3_session_closed() { [ "$(stord_sessions "$VOL3_ID")" = "0" ]; }
wait_for "stord session for ${VOL3_ID} closed on delete" "$DISPATCH_TIMEOUT" \
    vm3_session_closed \
    && qual_pass "stord session closed on VM delete (LVM)" \
    || qual_error "stord session for ${VOL3_ID} NOT closed on VM delete"
[ -b "$VOL3_LV" ] \
    && qual_pass "LV retained on VM delete (out-of-band reclamation: operator lvremoves)" \
    || qual_warn "LV disappeared on VM delete (unexpected — stord does not remove LVs)"
save_evidence "leg-g lvm vm deleted"

# 10. Restore the local shape and restart the agent (the flip's mirror;
#     leaves the deployment in the state the Summary and deploy teardown
#     expect: local backend, operator config restored byte-for-byte).
cp "${QUAL_TEST_DIR}/stord.toml.local-backup" "$STORD_TOML"
rm -f "${QUAL_TEST_DIR}/stord.toml.local-backup"
stop_daemon "$QUAL_AGENT_PID"
STORD_PID_G="$(stord_pid)"
[ -z "$STORD_PID_G" ] || stop_daemon "$STORD_PID_G"
rm -f "${STORD_DIR}/api.sock" "${QUAL_AGENT_DIR}/api.sock" "${QUAL_AGENT_DIR}/core.sock"
"${QUAL_BINARY_DIR}/chv-agent" "${QUAL_TEST_DIR}/agent.toml" \
    >> "${QUAL_LOGS_DIR}/agent.log" 2>&1 &
QUAL_AGENT_PID=$!
pids_current
wait_for "agent gRPC socket up after restore" 60 \
    test -S "${QUAL_AGENT_DIR}/api.sock" \
    || qual_die "agent did not come back (Leg G restore) — log: $(tail -30 "${QUAL_LOGS_DIR}/agent.log")"
wait_for "stord respawned on the restored local config" \
    "$STORD_RESTART_TIMEOUT" stord_socket_live \
    || qual_die "stord did not come back on the restored config (Leg G)"
node_reports_local() {
    [ "$(sqlite_query "$QUAL_DB" \
        "SELECT storage_classes FROM node_inventory WHERE node_id='${QUAL_NODE_ID}'" 2>/dev/null | head -1)" = '["local"]' ]
}
wait_for "node inventory back to storage_classes=[\"local\"]" 180 node_reports_local \
    && qual_pass "node inventory restored to [\"local\"]" \
    || qual_error "node inventory did not return to local after restore: $(sqlite_query "$QUAL_DB" "SELECT storage_classes FROM node_inventory WHERE node_id='${QUAL_NODE_ID}'" 2>/dev/null | head -1)"
qual_pass "deployment restored to the local backend (agent pid → ${QUAL_AGENT_PID})"

# 11. Teardown Leg G's VG + residue assertions (Leg F's pattern).
lvremove -f "$VOL3_LV" >/dev/null 2>&1 || qual_error "lvremove failed for ${VOL3_LV}"
vgremove -f "$LVM_VG_G" >/dev/null 2>&1 || qual_error "vgremove failed (Leg G)"
pvremove "$LVM_LOOP_G" >/dev/null 2>&1 || qual_error "pvremove failed (Leg G)"
losetup -d "$LVM_LOOP_G" 2>/dev/null || qual_error "losetup -d failed (Leg G)"
rm -f "$LVM_BACKING_G"
trap - INT TERM EXIT   # explicit teardown done

[ -z "$(vgs --noheadings -o vg_name 2>/dev/null | grep -x "$LVM_VG_G")" ] \
    && qual_pass "no Leg G VG residue (${LVM_VG_G} removed)" \
    || qual_error "VG ${LVM_VG_G} still present"
[ -z "$(lvs --noheadings -o lv_name,vg_name 2>/dev/null | grep "$LVM_VG_G")" ] \
    && qual_pass "no Leg G LV residue in ${LVM_VG_G}" \
    || qual_error "LVs remain in ${LVM_VG_G}: $(lvs "$LVM_VG_G" --noheadings 2>/dev/null)"
losetup "$LVM_LOOP_G" >/dev/null 2>&1 \
    && qual_error "loop device ${LVM_LOOP_G} still attached" \
    || qual_pass "loop device ${LVM_LOOP_G} detached"
LOOPS_AFTER_G="$(losetup -a 2>/dev/null | cut -d: -f1 | sort)"
NEW_LOOPS_G="$(comm -13 <(printf '%s\n' "$LOOPS_BEFORE_G") <(printf '%s\n' "$LOOPS_AFTER_G") | grep -v '^$' || true)"
[ -z "$NEW_LOOPS_G" ] \
    && qual_pass "no new loop devices remain (Leg G)" \
    || qual_error "new loop devices remain (Leg G): ${NEW_LOOPS_G}"
[ ! -f "$LVM_BACKING_G" ] \
    && qual_pass "Leg G loopback backing file removed" \
    || qual_error "Leg G backing file remains: ${LVM_BACKING_G}"
save_evidence "leg-g lvm-vm done"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
pids_current
if [ "$QUAL_ERRORS" -gt 0 ]; then
    qual_error "M4.5 scenario finished with ${QUAL_ERRORS} error(s)"
    if [ -n "${QUAL_PRESERVE_DIR:-}" ]; then
        qual_error "test dir preserved for post-mortem: ${QUAL_PRESERVE_DIR}"
    else
        qual_info "test dir: ${QUAL_TEST_DIR} (deploy keeps it on failure)"
    fi
    exit 1
fi
qual_pass "M4.5 storage scenario complete: local file (VM-integrated) + LVM (stord layer + VM-integrated) qualified"
exit 0
