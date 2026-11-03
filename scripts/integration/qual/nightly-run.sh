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

fail() {
    echo "::error::nightly-run: $*" >&2
    {
        echo "scenario	status	rc	duration_s	log	evidence"
        echo "preflight	blocked	1	0		$*"
    } > "$RESULT_FILE"
    write_summary "BLOCKED"
    exit 1
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

# Provision/verify the host (cloud-hypervisor, firmware, guest image, host
# tools) and stage release binaries from the checked-out candidate. A dirty
# checkout is a hard failure by design: evidence must name the candidate SHA.
echo "[qual-nightly] host preflight + binary staging"
if ! ./env-preflight.sh --stage-binaries > "${LOG_DIR}/env-preflight.log" 2>&1; then
    fail "env-preflight.sh failed; see ${LOG_DIR}/env-preflight.log"
fi

# The stock Ubuntu cloud image cannot boot through the CHV firmware chain
# (GRUB's LABEL= root needs an initramfs the shim/firmware path never
# delivers). All recorded M4.3–M4.8 evidence used the patched image, so the
# nightly lane must build and use it too.
STOCK_GUEST_IMAGE="${STOCK_GUEST_IMAGE:-noble-server-cloudimg-amd64.img}"
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

    if [ -d "$evidence_src" ]; then
        rm -rf "$evidence_dst"
        cp -a "$evidence_src" "$evidence_dst" || true
    fi

    printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$scenario" "$status" "$scenario_rc" "$duration" \
        "logs/${scenario}.log" "$( [ -d "$evidence_dst" ] && echo "$evidence_name" || echo '')" \
        >> "$RESULT_FILE"

    # Keep the tail in the job log so a failure is visible without opening
    # the artifact first (full log remains in the uploaded artifact).
    tail -n 20 "$scenario_log" || true
done

write_summary "$( [ "$RC" -eq 0 ] && echo PASS || echo FAIL )"
exit "$RC"
