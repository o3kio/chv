#!/usr/bin/env bash
# nightly-run.sh — scheduled qualification driver for the real-KVM M4.3–M4.8
# scenarios (#659).
#
# Runs on the self-hosted [self-hosted, linux, x64, chv-kvm] runner only. A
# GitHub-hosted runner cannot provide /dev/kvm, root, cloud-hypervisor, or the
# guest image, so this driver FAILS LOUDLY rather than skipping to a green
# status. If the self-hosted runner is offline the workflow stays queued —
# GitHub shows that state; it is never reported as a pass.
#
# Per scenario it writes a full run log under QUAL_ARTIFACT_ROOT/logs, copies
# the scenario's own evidence directory (m4.N-artifacts), and appends a row to
# results.tsv. The workflow uploads that directory as an artifact on every
# outcome (success or failure).
#
# Usage (root):
#   sudo env "PATH=$PATH" CHV_QUAL_ROOT=/var/lib/chv/qual \
#     QUAL_ARTIFACT_ROOT=/var/lib/chv/qual/ci-artifacts/run-123 \
#     ./nightly-run.sh --scenarios "m4.7-faults"
#
# Environment:
#   CHV_QUAL_ROOT       persistent qual root (default /var/lib/chv/qual)
#   QUAL_ARTIFACT_ROOT  artifact/log root (default: ./qual-artifacts)
#   STOCK_GUEST_IMAGE   stock seed used to build the patched image
#                       (default: noble-server-cloudimg-amd64.img)

set -uo pipefail
# Files created as root must still be readable by the runner-user artifact
# upload step; do not inherit a restrictive root umask.
umask 022

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHV_QUAL_ROOT="${CHV_QUAL_ROOT:-/var/lib/chv/qual}"
QUAL_ARTIFACT_ROOT="${QUAL_ARTIFACT_ROOT:-${PWD}/qual-artifacts}"
SCENARIOS=""

export CHV_QUAL_ROOT

# The self-hosted workspace is owned by the runner user while this driver runs
# as root; git otherwise refuses with "detected dubious ownership" and
# env-preflight cannot record the candidate SHA.
if [ -n "${GITHUB_WORKSPACE:-}" ] && [ -d "${GITHUB_WORKSPACE}" ]; then
    export GIT_CONFIG_COUNT=1
    export GIT_CONFIG_KEY_0=safe.directory
    export GIT_CONFIG_VALUE_0="${GITHUB_WORKSPACE}"
fi

while [ $# -gt 0 ]; do
    case "$1" in
        --scenarios) SCENARIOS="${2:?--scenarios requires a space-separated list}"; shift 2 ;;
        --artifact-root) QUAL_ARTIFACT_ROOT="${2:?--artifact-root requires a path}"; shift 2 ;;
        -h|--help) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "::error::nightly-run: unknown argument: $1" >&2; exit 2 ;;
    esac
done

case "$QUAL_ARTIFACT_ROOT" in
    /*) ;;
    *) QUAL_ARTIFACT_ROOT="${PWD}/${QUAL_ARTIFACT_ROOT}" ;;
esac

LOG_DIR="${QUAL_ARTIFACT_ROOT}/logs"
RESULT_FILE="${QUAL_ARTIFACT_ROOT}/results.tsv"
SUMMARY_FILE="${QUAL_ARTIFACT_ROOT}/summary.md"
mkdir -p "$LOG_DIR"

write_summary() {
    local status="$1"
    {
        echo "# Nightly KVM qualification: ${status}"
        echo ""
        echo "- root: \`${CHV_QUAL_ROOT}\`"
        echo "- artifacts: \`${QUAL_ARTIFACT_ROOT}\`"
        echo "- candidate: \`$(git -C "$SCRIPT_DIR" rev-parse HEAD 2>/dev/null || echo unknown)\`"
        echo ""
        if [ -s "$RESULT_FILE" ]; then
            echo '```'
            column -t -s $'\t' "$RESULT_FILE" 2>/dev/null || cat "$RESULT_FILE"
            echo '```'
        fi
    } > "$SUMMARY_FILE"
    if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
        cat "$SUMMARY_FILE" >> "$GITHUB_STEP_SUMMARY"
    fi
    # The driver runs as root; the artifact-upload step runs as the runner
    # user. Make the evidence readable regardless of root's umask.
    chmod -R a+rX "$QUAL_ARTIFACT_ROOT" 2>/dev/null || true
}

# Best-effort readability even when a preflight check exits early. A hard
# timeout cannot run traps, but every normal failure path still leaves the
# logs/results readable for the upload.
trap 'chmod -R a+rX "$QUAL_ARTIFACT_ROOT" 2>/dev/null || true' EXIT

fail() {
    echo "::error::nightly-run: $*" >&2
    # Append, do not truncate: if a later preflight/scenario check fails we
    # still want the rows already collected in results.tsv.
    if [ ! -s "$RESULT_FILE" ]; then
        printf 'scenario\tstatus\trc\tduration_s\tlog\tevidence\n' > "$RESULT_FILE"
    fi
    printf 'preflight\tblocked\t1\t0\t\t%s\n' "$*" >> "$RESULT_FILE"
    write_summary "BLOCKED"
    exit 1
}

# --- Runner hygiene and failure observability ------------------------------
# The self-hosted runner is dedicated to this lane. A lane that dies
# mid-scenario can leave chv daemons/VMMs holding 8443/8080/9100-9103 and
# /tmp/chv-qual-* trees behind; the next lane then fails for reasons that
# have nothing to do with the candidate ("Address already in use", CP→agent
# auth against a stale process). Sweep before preflight, scoped by argv[0]
# and the qualification roots so nothing else on the host is touched.
qual_daemon_pattern="(^|/)(cloud-hypervisor|chv-controlplane|chv-agent|chv-stord|chv-nwd)( |\$).*(/tmp/chv-qual-|${CHV_QUAL_ROOT})"

sweep_stale_qual_state() {
    local found
    found="$(pgrep -af "$qual_daemon_pattern" 2>/dev/null || true)"
    if [ -n "$found" ]; then
        echo "::warning::nightly-run: killing stale qualification processes from a previous lane:" >&2
        printf '%s\n' "$found" >&2
        pkill -f "$qual_daemon_pattern" 2>/dev/null || true
        sleep 2
        pkill -9 -f "$qual_daemon_pattern" 2>/dev/null || true
        sleep 1
    fi
    # Only the throwaway trees: the persistent root holds staged binaries,
    # firmware, and guest images and must survive. Never touch the artifact
    # root, even when a manual invocation puts it under /tmp.
    local d
    for d in /tmp/chv-qual-*; do
        [ -d "$d" ] || continue
        case "${QUAL_ARTIFACT_ROOT}/" in "$d"/*) continue ;; esac
        case "$d/" in "${QUAL_ARTIFACT_ROOT}"/*) continue ;; esac
        rm -rf -- "$d"
    done
    # nwd hardcodes its dnsmasq runtime here; a killed nwd can leave it behind.
    rm -rf /run/chv/nwd 2>/dev/null || true
}

require_qual_ports_free() {
    local ports="8080 8443 8444 9100 9101 9102 9103 51052 51053 51062 51063"
    local busy
    busy="$(ss -ltnH 2>/dev/null | awk -v want="$ports" \
        'BEGIN { split(want, a, " "); for (i in a) w[a[i]] = 1 }
         { n = split($4, p, ":"); if (p[n] in w) print $4 }')"
    if [ -n "$busy" ]; then
        fail "qualification ports still in use after the stale-state sweep: $(echo "$busy" | tr '\n' ' ') — inspect with: ss -ltnp"
    fi
}

snapshot_host_state() {
    local out="$1"
    {
        date -u
        hostname
        echo "--- listeners"
        ss -ltnp 2>/dev/null | grep -E ':(8080|8443|8444|9100|9101|9102|9103|5105[0-9]|5106[0-9])\b' || true
        echo "--- qual processes"
        ps -eo pid,ppid,etime,rss,args 2>/dev/null \
            | grep -E 'cloud-hypervisor|chv-(agent|stord|nwd|controlplane)' \
            | grep -v grep || true
        echo "--- memory/disk"
        free -h 2>/dev/null || true
        df -h / /tmp 2>/dev/null || true
        echo "--- kernel"
        dmesg 2>/dev/null | tail -n 50 || true
    } > "$out" 2>&1
}

# Copy the log/state files from each NEWLY preserved failed test tree into the
# uploaded artifact. The deploy teardown keeps those trees on the runner for
# post-mortem, but the workflow artifact only carries QUAL_ARTIFACT_ROOT, so
# without this the per-VM console and daemon logs that explain a boot or
# volume failure never leave the runner. Text files only, capped per file.
preserve_failed_dirs() {
    local before="$1" scenario="$2" d f rel out dest
    while IFS= read -r d; do
        [ -n "$d" ] || continue
        if [ -n "$before" ] && printf '%s\n' "$before" | grep -Fxq -- "$d"; then
            continue
        fi
        dest="${QUAL_ARTIFACT_ROOT}/failed/${scenario}/$(basename "$d")"
        mkdir -p "$dest"
        while IFS= read -r f; do
            [ -n "$f" ] || continue
            rel="${f#"$d"/}"
            out="${dest}/${rel}"
            mkdir -p "$(dirname "$out")"
            head -c 2097152 "$f" > "$out" 2>/dev/null || true
        done < <(find "$d" -type f \
            \( -name '*.log' -o -name '*.toml' -o -name 'deployment.map' \
               -o -name 'pids.current' -o -name 'host-state.txt' -o -name '*.jsonl' \) \
            -print 2>/dev/null)
        echo "::warning::nightly-run: preserved failed test tree for ${scenario}: ${dest}" >&2
    done < <(ls -1d /tmp/chv-qual-failed-* 2>/dev/null || true)
}

[ "$(id -u)" -eq 0 ] || fail "must run as root on the self-hosted KVM runner"
[ -e /dev/kvm ] || fail "/dev/kvm is absent on this runner — real-KVM qualification cannot run (this is a failure, not a skip)"
[ -n "$SCENARIOS" ] || fail "no scenarios requested (use --scenarios)"
command -v python3 >/dev/null 2>&1 || fail "python3 is required by the qualification harness"
command -v curl >/dev/null 2>&1 || fail "curl is required by the qualification harness"

printf 'scenario\tstatus\trc\tduration_s\tlog\tevidence\n' > "$RESULT_FILE"

# deploy.sh resolves the --exec command relative to its caller's working
# directory (the documented usage is `cd qual && ./deploy.sh --exec ./m4.x.sh`).
cd "$SCRIPT_DIR" || fail "cannot cd to ${SCRIPT_DIR}"

echo "[qual-nightly] sweeping stale qualification state from a previous lane"
sweep_stale_qual_state
require_qual_ports_free

# The stock Ubuntu cloud image cannot boot through the CHV firmware chain
# (GRUB's LABEL= root needs an initramfs the shim/firmware path never
# delivers). All recorded M4.3–M4.8 evidence used the patched image, so the
# nightly lane must build and use it too. Keep the stock name in one variable
# so preflight downloads (L1) and the patch step use the same file.
STOCK_GUEST_IMAGE="${STOCK_GUEST_IMAGE:-noble-server-cloudimg-amd64.img}"

# Provision/verify the host (cloud-hypervisor, firmware, guest image, host
# tools) and stage release binaries from the checked-out candidate. A dirty
# checkout is a hard failure by design: evidence must name the candidate SHA.
echo "[qual-nightly] host preflight + binary staging"
if ! GUEST_IMAGE="$STOCK_GUEST_IMAGE" ./env-preflight.sh --stage-binaries > "${LOG_DIR}/env-preflight.log" 2>&1; then
    fail "env-preflight.sh failed; see ${LOG_DIR}/env-preflight.log"
fi

echo "[qual-nightly] preparing patched guest image"
if ! GUEST_IMAGE="$STOCK_GUEST_IMAGE" ./patch-guest-image.sh > "${LOG_DIR}/patch-guest-image.log" 2>&1; then
    fail "patch-guest-image.sh failed; see ${LOG_DIR}/patch-guest-image.log"
fi
export GUEST_IMAGE="noble-qual-patched.img"

RC=0
for scenario in $SCENARIOS; do
    script="${SCRIPT_DIR}/${scenario}.sh"
    scenario_log="${LOG_DIR}/${scenario}.log"
    evidence_name="${scenario%%-*}-artifacts"
    evidence_src="${CHV_QUAL_ROOT}/${evidence_name}"
    evidence_dst="${QUAL_ARTIFACT_ROOT}/${evidence_name}"

    if [ ! -f "$script" ]; then
        fail "scenario script not found: ${script}"
    fi

    echo "[qual-nightly] running ${scenario}"
    started="$(date +%s)"
    failed_before="$(ls -1d /tmp/chv-qual-failed-* 2>/dev/null || true)"
    if "${SCRIPT_DIR}/deploy.sh" --exec "./${scenario}.sh" > "$scenario_log" 2>&1; then
        status="pass"
        scenario_rc=0
    else
        scenario_rc=$?
        status="fail"
        RC=1
        echo "::error::nightly-run: ${scenario} failed (rc=${scenario_rc}); log artifact: ${scenario_log}"
    fi
    duration="$(( $(date +%s) - started ))"

    preserve_failed_dirs "$failed_before" "$scenario"
    if [ "$status" = "fail" ]; then
        snapshot_host_state "${LOG_DIR}/${scenario}-host-state.txt"
    fi

    if [ -d "$evidence_src" ]; then
        rm -rf "$evidence_dst"
        if ! cp -a "$evidence_src" "$evidence_dst"; then
            echo "::error::nightly-run: failed to copy evidence ${evidence_src} -> ${evidence_dst}" >&2
            status="fail"
            RC=1
        fi
    else
        echo "::warning::nightly-run: scenario ${scenario} produced no evidence directory: ${evidence_src}" >&2
    fi

    printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$scenario" "$status" "$scenario_rc" "$duration" \
        "logs/${scenario}.log" "$( [ -d "$evidence_dst" ] && echo "$evidence_name" || echo '')" \
        >> "$RESULT_FILE"

    # Make this scenario's logs/evidence readable now, not only when the
    # whole loop finishes: a timeout or hang must not lose earlier evidence.
    chmod -R a+rX "$QUAL_ARTIFACT_ROOT" 2>/dev/null || true

    # Keep the tail in the job log so a failure is visible without opening
    # the artifact first (full log remains in the uploaded artifact).
    tail -n 20 "$scenario_log" || true
done

write_summary "$( [ "$RC" -eq 0 ] && echo PASS || echo FAIL )"
exit "$RC"
