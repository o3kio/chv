#!/usr/bin/env bash
# fresh-host.sh — fresh-host qualification lane for the pre-production gate
# (#666, with the #556 quick-install leg).
#
# This driver automates the demonstration required by #666: install the
# candidate from release artifacts on a clean host, operate it, upgrade from
# the previous released version while preserving state, uninstall/purge, and
# run the published one-line quick installer. It is deliberately a CONTAINER
# demonstration, and says so everywhere:
#
#   Environment: a clean Ubuntu noble amd64 root filesystem and systemd
#   inside a debootstrap + systemd-nspawn container on the qualification
#   runner. It shares the host kernel and network namespace, has no
#   /dev/kvm, and is NOT a bare-metal or cloud-VM host. It proves the
#   installer/service/state contract; VM execution is covered by the
#   real-KVM m4.3-m4.8 lane (#659).
#
# Isolation: every leg gets its own freshly unpacked root under
# CHV_QUAL_ROOT. The host's own CHV installation is never touched. Port
# preflight fails loudly if the shared network namespace already has a CHV
# listener bound.
#
# Legs:
#   install-tarball   candidate install.sh from a release tarball, the full
#                     bring-up contract, then quick-install --uninstall and
#                     --purge on the same deployment.
#   install-packages  candidate .deb install on a clean root and the static
#                     packaging contract (only with --packages).
#   upgrade           previous released tarball, identifiable state, then the
#                     candidate upgrade. install.sh refuses to re-run over a
#                     persistent database by design (clean_previous_install);
#                     the driver asserts that fail-closed guard, then uses the
#                     documented package upgrade path when --packages is
#                     supplied. Without packages the leg fails rather than
#                     pretending a tarball upgrade happened.
#   quick-install     the published one-line quick installer fetched from an
#                     explicitly pinned ref/SHA (never a floating main) and
#                     the same bring-up contract (#556).
#
# Usage (root):
#   sudo ./fresh-host.sh \
#     --candidate-tarball dist/chv-0.3.0-linux-amd64.tar.gz \
#     --candidate-version 0.3.0 \
#     --candidate-sha "$GITHUB_SHA" \
#     --previous-tarball /tmp/chv-0.3.0-linux-amd64.tar.gz \
#     --packages dist/packages \
#     --quick-install "$GITHUB_SHA" \
#     --quick-install-version 0.3.0 \
#     --evidence-dir /var/lib/chv/qual/ci-artifacts/fresh-host
#
# Options:
#   --candidate-tarball PATH    candidate release tarball (required)
#   --candidate-version VER     version without leading 'v' (derived from the
#                               tarball name when omitted)
#   --candidate-sha SHA         expected commit embedded in the binaries
#                               (asserted when supplied)
#   --previous-tarball PATH     previous release tarball (upgrade leg)
#   --previous-release TAG      release tag to download with gh (default v0.3.0)
#   --previous-version VER      previous version (derived when omitted)
#   --packages DIR              candidate .deb directory (enables the package
#                               install leg and the package upgrade path)
#   --quick-install REF         pinned ref/SHA for scripts/quick-install.sh
#                               (enables the #556 leg; branch names are refused)
#   --quick-install-version VER release version the quick installer installs
#                               (default: the previous version)
#   --quick-install-candidate   the quick-install version is the candidate
#                               release (assert the full candidate contract,
#                               including the credential-persistence gate)
#   --evidence-dir DIR          results/summary/log destination
#                               (default: CHV_QUAL_ROOT/fresh-host-evidence-TS)
#   --keep                      keep container roots after a passing leg too
#   --skip-upgrade              do not run the upgrade leg
#   -h, --help                  show this help
#
# Environment:
#   CHV_QUAL_ROOT   persistent qualification root (default /var/lib/chv/qual)
#   SUITE           debootstrap suite (default noble)
#   GITHUB_REPO     release repository for gh/quick-install (default o3kio/chv)

set -uo pipefail
# Evidence is uploaded by the runner user while this driver runs as root.
umask 022

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
# shellcheck source=lib.sh
source "${SCRIPT_DIR}/lib.sh"

CHV_QUAL_ROOT="${CHV_QUAL_ROOT:-/var/lib/chv/qual}"
SUITE="${SUITE:-noble}"
GITHUB_REPO="${GITHUB_REPO:-o3kio/chv}"

CANDIDATE_TARBALL="${CANDIDATE_TARBALL:-}"
CANDIDATE_VERSION="${CANDIDATE_VERSION:-}"
CANDIDATE_SHA="${CANDIDATE_SHA:-}"
PREVIOUS_TARBALL="${PREVIOUS_TARBALL:-}"
PREVIOUS_RELEASE="${PREVIOUS_RELEASE:-v0.3.0}"
PREVIOUS_VERSION="${PREVIOUS_VERSION:-}"
PACKAGES_DIR="${PACKAGES_DIR:-}"
QUICK_REF="${QUICK_REF:-}"
QUICK_VERSION="${QUICK_VERSION:-}"
QUICK_INSTALL_CANDIDATE=false
EVIDENCE_DIR="${EVIDENCE_DIR:-}"
KEEP=false
SKIP_UPGRADE=false

usage() {
    sed -n '2,75p' "$0" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
    case "$1" in
        --candidate-tarball) CANDIDATE_TARBALL="${2:?--candidate-tarball requires a path}"; shift 2 ;;
        --candidate-tarball=*) CANDIDATE_TARBALL="${1#*=}"; shift ;;
        --candidate-version) CANDIDATE_VERSION="${2:?--candidate-version requires a version}"; shift 2 ;;
        --candidate-version=*) CANDIDATE_VERSION="${1#*=}"; shift ;;
        --candidate-sha) CANDIDATE_SHA="${2:?--candidate-sha requires a commit}"; shift 2 ;;
        --candidate-sha=*) CANDIDATE_SHA="${1#*=}"; shift ;;
        --previous-tarball) PREVIOUS_TARBALL="${2:?--previous-tarball requires a path}"; shift 2 ;;
        --previous-tarball=*) PREVIOUS_TARBALL="${1#*=}"; shift ;;
        --previous-release) PREVIOUS_RELEASE="${2:?--previous-release requires a tag}"; shift 2 ;;
        --previous-release=*) PREVIOUS_RELEASE="${1#*=}"; shift ;;
        --previous-version) PREVIOUS_VERSION="${2:?--previous-version requires a version}"; shift 2 ;;
        --previous-version=*) PREVIOUS_VERSION="${1#*=}"; shift ;;
        --packages) PACKAGES_DIR="${2:?--packages requires a directory}"; shift 2 ;;
        --packages=*) PACKAGES_DIR="${1#*=}"; shift ;;
        --quick-install) QUICK_REF="${2:?--quick-install requires a ref or SHA}"; shift 2 ;;
        --quick-install=*) QUICK_REF="${1#*=}"; shift ;;
        --quick-install-version) QUICK_VERSION="${2:?--quick-install-version requires a version}"; shift 2 ;;
        --quick-install-version=*) QUICK_VERSION="${1#*=}"; shift ;;
        --quick-install-candidate) QUICK_INSTALL_CANDIDATE=true; shift ;;
        --evidence-dir) EVIDENCE_DIR="${2:?--evidence-dir requires a directory}"; shift 2 ;;
        --evidence-dir=*) EVIDENCE_DIR="${1#*=}"; shift ;;
        --keep) KEEP=true; shift ;;
        --skip-upgrade) SKIP_UPGRADE=true; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "fresh-host: unknown argument: $1 (use --help)" >&2; exit 2 ;;
    esac
done

# ---------------------------------------------------------------------------
# Evidence setup (before validation so even a blocked preflight leaves the
# results marker the workflow requires)
# ---------------------------------------------------------------------------
if [ -z "$EVIDENCE_DIR" ]; then
    EVIDENCE_DIR="${CHV_QUAL_ROOT}/fresh-host-evidence-$(date -u +%Y%m%dT%H%M%SZ)"
fi
case "$EVIDENCE_DIR" in
    /*) ;;
    *) EVIDENCE_DIR="${PWD}/${EVIDENCE_DIR}" ;;
esac
mkdir -p "$EVIDENCE_DIR" || { echo "fresh-host: cannot create ${EVIDENCE_DIR}" >&2; exit 1; }
RESULTS="${EVIDENCE_DIR}/results.tsv"
SUMMARY="${EVIDENCE_DIR}/summary.md"
ARTIFACT_DIGESTS="${EVIDENCE_DIR}/artifacts.sha256"
printf 'leg\tstatus\trc\tduration_s\tlog\tevidence\n' > "$RESULTS"

write_summary() {
    local status="$1" skipped="" skipped_n=0
    if [ -s "$RESULTS" ]; then
        skipped_n="$(awk -F'\t' 'NR > 1 && $2 == "skipped" { n++ } END { print n + 0 }' "$RESULTS")"
    fi
    if [ "$status" = "PASS" ] && [ "$skipped_n" -gt 0 ]; then
        skipped=" (partial — ${skipped_n} leg(s) skipped)"
    fi
    {
        echo "# Fresh-host qualification: ${status}${skipped}"
        echo ""
        echo "- candidate version: \`${CANDIDATE_VERSION:-_pending_}\`"
        echo "- candidate commit: \`${CANDIDATE_SHA:-_not supplied_}\`"
        echo "- previous release: \`${PREVIOUS_RELEASE:-_not used_}\` (version \`${PREVIOUS_VERSION:-_unknown_}\`)"
        echo "- quick-install ref: \`${QUICK_REF:-_not requested_}\` (release version \`${QUICK_VERSION:-_unknown_}\`)"
        echo "- evidence: \`${EVIDENCE_DIR}\`"
        echo ""
        echo "## Environment (honest label)"
        echo ""
        echo "Clean Ubuntu ${SUITE} amd64 root filesystem and systemd inside a"
        echo "debootstrap + systemd-nspawn container on the qualification runner."
        echo "It shares the **host kernel and network namespace**, has **no /dev/kvm**,"
        echo "and is **not a bare-metal or cloud-VM host**."
        echo ""
        echo "## Artifact digests"
        echo ""
        if [ -s "$ARTIFACT_DIGESTS" ]; then
            echo '```'
            cat "$ARTIFACT_DIGESTS"
            echo '```'
        else
            echo "_pending_"
        fi
        echo ""
        echo "## Legs"
        echo ""
        if [ -s "$RESULTS" ]; then
            echo '```'
            column -t -s $'\t' "$RESULTS" 2>/dev/null || cat "$RESULTS"
            echo '```'
        fi
    } > "$SUMMARY"
    if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
        cat "$SUMMARY" >> "$GITHUB_STEP_SUMMARY"
    fi
}

record_result() {
    local leg="$1" status="$2" rc="$3" duration="$4" log="$5" evidence="$6"
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$leg" "$status" "$rc" "$duration" "$log" "$evidence" >> "$RESULTS"
}

preflight_fail() {
    local msg="$*"
    qual_error "$msg"
    printf 'preflight\tblocked\t1\t0\t\t%s\n' "$(printf '%s' "$msg" | tr '\t\n' '  ')" >> "$RESULTS"
    write_summary "BLOCKED"
    exit 1
}

# ---------------------------------------------------------------------------
# Input validation
# ---------------------------------------------------------------------------
[ "$(id -u)" -eq 0 ] || preflight_fail "must run as root (sudo)"
[ -n "$CANDIDATE_TARBALL" ] || preflight_fail "--candidate-tarball is required"
[ -f "$CANDIDATE_TARBALL" ] || preflight_fail "candidate tarball not found: ${CANDIDATE_TARBALL}"
CANDIDATE_TARBALL="$(realpath "$CANDIDATE_TARBALL")"

derive_tarball_version() {
    basename "$1" | sed -nE 's/^chv-(.+)-linux-amd64\.tar\.gz$/\1/p'
}

if [ -z "$CANDIDATE_VERSION" ]; then
    CANDIDATE_VERSION="$(derive_tarball_version "$CANDIDATE_TARBALL")"
fi
[ -n "$CANDIDATE_VERSION" ] \
    || preflight_fail "cannot derive --candidate-version from $(basename "$CANDIDATE_TARBALL"); pass it explicitly"
if [ -n "$CANDIDATE_SHA" ]; then
    printf '%s' "$CANDIDATE_SHA" | grep -qE '^[0-9a-f]{40}$' \
        || preflight_fail "--candidate-sha must be a full 40-hex commit (got '${CANDIDATE_SHA}')"
fi

if [ -n "$PACKAGES_DIR" ]; then
    [ -d "$PACKAGES_DIR" ] || preflight_fail "packages directory not found: ${PACKAGES_DIR}"
    PACKAGES_DIR="$(cd "$PACKAGES_DIR" && pwd)"
    ls "$PACKAGES_DIR"/chv-controlplane_*.deb "$PACKAGES_DIR"/chv-node_*.deb "$PACKAGES_DIR"/chvctl_*.deb \
        >/dev/null 2>&1 \
        || preflight_fail "expected chv-controlplane_*.deb, chv-node_*.deb and chvctl_*.deb in ${PACKAGES_DIR}"
fi

if [ -z "$PREVIOUS_VERSION" ]; then
    if [ -n "$PREVIOUS_TARBALL" ]; then
        PREVIOUS_VERSION="$(derive_tarball_version "$PREVIOUS_TARBALL")"
    else
        PREVIOUS_VERSION="${PREVIOUS_RELEASE#v}"
    fi
fi

if [ -n "$QUICK_REF" ]; then
    case "$QUICK_REF" in
        main|master|HEAD|latest|refs/heads/main|refs/heads/master)
            preflight_fail "--quick-install must pin an explicit ref/SHA, not the floating '${QUICK_REF}'"
            ;;
    esac
    if ! printf '%s' "$QUICK_REF" | grep -qE '^[0-9a-f]{40}$|^refs/tags/v[0-9]+\.[0-9]+\.[0-9]+([-+][A-Za-z0-9._-]+)?$|^v[0-9]+\.[0-9]+\.[0-9]+([-+][A-Za-z0-9._-]+)?$'; then
        preflight_fail "--quick-install must be a full 40-hex SHA or a release tag (got '${QUICK_REF}')"
    fi
    QUICK_VERSION="${QUICK_VERSION:-$PREVIOUS_VERSION}"
    [ -n "$QUICK_VERSION" ] || preflight_fail "--quick-install requires --quick-install-version (or a previous version)"
fi

# ---------------------------------------------------------------------------
# Host prerequisites
# ---------------------------------------------------------------------------
command -v debootstrap >/dev/null 2>&1 || preflight_fail "debootstrap is required (apt-get install debootstrap)"
command -v systemd-nspawn >/dev/null 2>&1 || preflight_fail "systemd-nspawn is required (apt-get install systemd-container)"
command -v curl >/dev/null 2>&1 || preflight_fail "curl is required"
command -v sha256sum >/dev/null 2>&1 || preflight_fail "sha256sum is required"
# `ss` is the only guard preventing a collision with a host listener on the
# shared network namespace. Without it the preflight would pass vacuously.
command -v ss >/dev/null 2>&1 || preflight_fail "ss is required to check host qualification ports (iproute2)"

if [ "$SKIP_UPGRADE" = false ] && [ -z "$PREVIOUS_TARBALL" ]; then
    command -v gh >/dev/null 2>&1 \
        || preflight_fail "gh is required to download ${PREVIOUS_RELEASE} (or pass --previous-tarball)"
fi

# The container shares the host network namespace. A host CHV deployment or
# any other listener on these ports would collide and, worse, would make the
# lane look like it exercised the host install. Refuse before booting.
HOST_PORTS="80 8080 8443 8444 9101 9102 9103 9901"
busy="$(ss -ltnH 2>/dev/null | awk -v want="$HOST_PORTS" '
    BEGIN { n = split(want, a, " "); for (i = 1; i <= n; i++) w[a[i]] = 1 }
    { p = $4; sub(/^.*:/, "", p); if (p in w) print $4 }' | tr '\n' ' ')"
[ -z "$busy" ] \
    || preflight_fail "host listeners occupy qualification ports (${busy}); the container shares the host network namespace — stop the host service first"
qual_pass "host qualification ports are free"

# Keep the checkout ownership behavior of the nightly lane: the job runs as
# root through sudo, but git/evidence reads may happen as the runner user.
if [ -n "${GITHUB_WORKSPACE:-}" ] && [ -d "${GITHUB_WORKSPACE}" ]; then
    export GIT_CONFIG_COUNT=1
    export GIT_CONFIG_KEY_0=safe.directory
    export GIT_CONFIG_VALUE_0="${GITHUB_WORKSPACE}"
fi

mkdir -p "$CHV_QUAL_ROOT"

# ---------------------------------------------------------------------------
# Previous release artifact
# ---------------------------------------------------------------------------
if [ "$SKIP_UPGRADE" = false ] && [ -z "$PREVIOUS_TARBALL" ]; then
    PREV_DIR="${CHV_QUAL_ROOT}/fresh-host-previous/${PREVIOUS_RELEASE}"
    mkdir -p "$PREV_DIR"
    qual_info "downloading previous release ${PREVIOUS_RELEASE} via gh"
    if ! gh release download "$PREVIOUS_RELEASE" --repo "$GITHUB_REPO" \
        --pattern 'chv-*-linux-amd64.tar.gz' \
        --pattern 'chv-*-linux-amd64.tar.gz.sha256' \
        --dir "$PREV_DIR" --clobber >/dev/null 2>&1; then
        preflight_fail "gh release download ${PREVIOUS_RELEASE} failed (repo ${GITHUB_REPO}); pass --previous-tarball"
    fi
    PREVIOUS_TARBALL="$(find "$PREV_DIR" -maxdepth 1 -name 'chv-*-linux-amd64.tar.gz' -print -quit)"
    [ -n "$PREVIOUS_TARBALL" ] \
        || preflight_fail "release ${PREVIOUS_RELEASE} carries no chv-*-linux-amd64.tar.gz asset"
fi
if [ "$SKIP_UPGRADE" = false ]; then
    [ -f "$PREVIOUS_TARBALL" ] || preflight_fail "previous tarball not found: ${PREVIOUS_TARBALL}"
    PREVIOUS_TARBALL="$(realpath "$PREVIOUS_TARBALL")"
    [ -n "$PREVIOUS_VERSION" ] \
        || preflight_fail "cannot derive --previous-version from $(basename "$PREVIOUS_TARBALL")"
fi

# ---------------------------------------------------------------------------
# Quick-install script (pinned ref, fetched once and hashed as evidence)
# ---------------------------------------------------------------------------
QUICK_SCRIPT=""
if [ -n "$QUICK_REF" ]; then
    QUICK_URL_REF="${QUICK_REF#refs/tags/}"
    QUICK_SCRIPT="${EVIDENCE_DIR}/quick-install-${QUICK_URL_REF}.sh"
    qual_info "fetching quick-install.sh from pinned ref ${QUICK_REF}"
    curl -fsSL --retry 3 \
        "https://raw.githubusercontent.com/${GITHUB_REPO}/${QUICK_URL_REF}/scripts/quick-install.sh" \
        -o "$QUICK_SCRIPT" \
        || preflight_fail "failed to fetch quick-install.sh at pinned ref ${QUICK_REF}"
    bash -n "$QUICK_SCRIPT" || preflight_fail "fetched quick-install.sh failed bash -n"
fi

# ---------------------------------------------------------------------------
# Artifact digests (recorded before any container runs)
# ---------------------------------------------------------------------------
{
    printf 'candidate_tarball  %s  %s\n' \
        "$(sha256sum "$CANDIDATE_TARBALL" | awk '{print $1}')" "$(basename "$CANDIDATE_TARBALL")"
    if [ "$SKIP_UPGRADE" = false ]; then
        printf 'previous_tarball   %s  %s\n' \
            "$(sha256sum "$PREVIOUS_TARBALL" | awk '{print $1}')" "$(basename "$PREVIOUS_TARBALL")"
    fi
    if [ -n "$PACKAGES_DIR" ]; then
        ( cd "$PACKAGES_DIR" && sha256sum ./*.deb 2>/dev/null | sed 's# \./#  #' )
    fi
    if [ -n "$QUICK_SCRIPT" ]; then
        printf 'quick_install_script %s  %s\n' \
            "$(sha256sum "$QUICK_SCRIPT" | awk '{print $1}')" "$(basename "$QUICK_SCRIPT")"
    fi
} > "$ARTIFACT_DIGESTS"
qual_info "artifact digests written to ${ARTIFACT_DIGESTS}"

# ---------------------------------------------------------------------------
# Cached clean base (same recipe as clean-install.sh)
# ---------------------------------------------------------------------------
BASE_TARBALL="${CHV_QUAL_ROOT}/nspawn-base-${SUITE}.tar.gz"
NEED_PKGS=""
for p in debootstrap systemd-container ubuntu-keyring; do
    dpkg -s "$p" >/dev/null 2>&1 || NEED_PKGS="${NEED_PKGS} ${p}"
done
if [ -n "$NEED_PKGS" ]; then
    qual_info "installing host prerequisites:${NEED_PKGS}"
    # shellcheck disable=SC2086
    if ! { apt-get update -qq && apt-get install -y -qq ${NEED_PKGS} >/dev/null; }; then
        preflight_fail "failed to install host prerequisites:${NEED_PKGS}"
    fi
fi

# Rebuild the cached base if it is missing or unreadable. A partially-written
# or corrupt cache must fail here with a clear message instead of producing an
# opaque "no transcript" for every leg.
if [ -s "$BASE_TARBALL" ] && ! tar -tzf "$BASE_TARBALL" >/dev/null 2>&1; then
    qual_warn "cached base ${BASE_TARBALL} is unreadable; rebuilding it"
    rm -f "$BASE_TARBALL"
fi
if [ ! -s "$BASE_TARBALL" ]; then
    qual_info "debootstrap ${SUITE} base (one-time, cached)"
    # curl is needed by quick-install.sh before it installs its own deps; the
    # shared base may have been cached by clean-install.sh without it, so the
    # quick leg also installs it defensively.
    debootstrap --include=systemd,systemd-sysv,procps,iproute2,wireguard-tools,libssl3t64,curl \
        "$SUITE" "${CHV_QUAL_ROOT}/nspawn-base" http://archive.ubuntu.com/ubuntu \
        >"${CHV_QUAL_ROOT}/fresh-host-debootstrap.log" 2>&1 \
        || preflight_fail "debootstrap failed — see ${CHV_QUAL_ROOT}/fresh-host-debootstrap.log"
    rm -rf "${CHV_QUAL_ROOT}/nspawn-base/dev/.static/"
    tar -C "${CHV_QUAL_ROOT}/nspawn-base" -czf "$BASE_TARBALL" . \
        || preflight_fail "failed to write cached base ${BASE_TARBALL}"
    rm -rf "${CHV_QUAL_ROOT}/nspawn-base"
    tar -tzf "$BASE_TARBALL" >/dev/null 2>&1 \
        || preflight_fail "cached base ${BASE_TARBALL} is unreadable after creation"
fi
qual_pass "cached ${SUITE} base present"

# ---------------------------------------------------------------------------
# Container plumbing
# ---------------------------------------------------------------------------
CURRENT_ROOT=""
CURRENT_MACHINE=""

cleanup_container() {
    local rc=$?
    if [ -n "$CURRENT_MACHINE" ]; then
        machinectl terminate "$CURRENT_MACHINE" 2>/dev/null || true
    fi
    if [ -n "$CURRENT_ROOT" ] && [ -d "$CURRENT_ROOT" ]; then
        if [ "$KEEP" != true ] && [ "${QUAL_ERRORS:-0}" -eq 0 ]; then
            rm -rf "$CURRENT_ROOT"
        else
            qual_info "container root preserved: ${CURRENT_ROOT}"
        fi
    fi
    # Evidence is uploaded by the runner user while this driver runs as root.
    chmod -R a+rX "$EVIDENCE_DIR" 2>/dev/null || true
    exit "$rc"
}
trap 'cleanup_container' EXIT

prepare_root() {
    local leg="$1"
    CURRENT_ROOT="${CHV_QUAL_ROOT}/fresh-host-${leg}-root"
    rm -rf "$CURRENT_ROOT"
    mkdir -p "$CURRENT_ROOT"
    if ! tar -C "$CURRENT_ROOT" -xzf "$BASE_TARBALL"; then
        # Do not exit mid-run: that would leave later legs with no row at all.
        # Record this leg's failure and let the remaining accounting stand.
        qual_error "${leg}: failed to extract cached base ${BASE_TARBALL}"
        record_result "$leg" "fail" "1" "0" "" "cached base extract failed"
        return 1
    fi
    rm -rf "${CURRENT_ROOT}/dev/.static/" 2>/dev/null || true
    rm -f "${CURRENT_ROOT}/etc/hostname" "${CURRENT_ROOT}/etc/machine-id"
    printf 'fresh-host-%s\n' "$leg" > "${CURRENT_ROOT}/etc/hostname"
    touch "${CURRENT_ROOT}/etc/machine-id"
    mkdir -p "${CURRENT_ROOT}/etc/apt"
    printf 'deb http://archive.ubuntu.com/ubuntu %s main universe\n' "$SUITE" \
        > "${CURRENT_ROOT}/etc/apt/sources.list"
    if [ -s /etc/resolv.conf ]; then
        cp /etc/resolv.conf "${CURRENT_ROOT}/etc/resolv.conf"
    fi
    mkdir -p "${CURRENT_ROOT}/root/fresh-host/packages"
    cp "$CANDIDATE_TARBALL" "${CURRENT_ROOT}/root/fresh-host/candidate.tar.gz"
    if [ "$SKIP_UPGRADE" = false ]; then
        cp "$PREVIOUS_TARBALL" "${CURRENT_ROOT}/root/fresh-host/previous.tar.gz"
    fi
    if [ -n "$PACKAGES_DIR" ]; then
        cp "$PACKAGES_DIR"/*.deb "${CURRENT_ROOT}/root/fresh-host/packages/"
    fi
    if [ -n "$QUICK_SCRIPT" ]; then
        cp "$QUICK_SCRIPT" "${CURRENT_ROOT}/root/fresh-host/quick-install.sh"
    fi
    # Reuse the qualification runner's cached base image when present; it
    # only avoids a repeated ~600 MiB download, and is recorded as a staged
    # input rather than claimed as part of the clean host.
    local cached_image="${CHV_QUAL_ROOT}/images/noble-server-cloudimg-amd64.img"
    if [ -s "$cached_image" ]; then
        cp "$cached_image" "${CURRENT_ROOT}/root/noble-server-cloudimg-amd64.img"
    fi
    local cached_fw="${CHV_QUAL_ROOT}/hypervisor-fw"
    if [ -s "$cached_fw" ]; then
        cp "$cached_fw" "${CURRENT_ROOT}/root/CLOUDHV.fd"
    fi
    # The uninstall/purge leg uses the candidate's own quick-install.sh; the
    # #556 leg uses the separately pinned published copy.
    cp "${REPO_ROOT}/scripts/quick-install.sh" "${CURRENT_ROOT}/root/fresh-host/quick-install-local.sh"

    {
        printf 'CANDIDATE_VERSION=%q\n' "$CANDIDATE_VERSION"
        printf 'CANDIDATE_SHA=%q\n' "$CANDIDATE_SHA"
        printf 'CANDIDATE_TARBALL=/root/fresh-host/candidate.tar.gz\n'
        printf 'CANDIDATE_SHA256=%q\n' "$(sha256sum "$CANDIDATE_TARBALL" | awk '{print $1}')"
        printf 'PREVIOUS_VERSION=%q\n' "$PREVIOUS_VERSION"
        printf 'PREVIOUS_TARBALL=/root/fresh-host/previous.tar.gz\n'
        if [ "$SKIP_UPGRADE" = false ]; then
            printf 'PREVIOUS_SHA256=%q\n' "$(sha256sum "$PREVIOUS_TARBALL" | awk '{print $1}')"
        else
            printf 'PREVIOUS_SHA256=%q\n' ""
        fi
        printf 'HAS_PACKAGES=%q\n' "$( [ -n "$PACKAGES_DIR" ] && echo 1 || echo 0 )"
        printf 'PACKAGES_DIR=/root/fresh-host/packages\n'
        printf 'QUICK_REF=%q\n' "$QUICK_REF"
        printf 'QUICK_VERSION=%q\n' "$QUICK_VERSION"
        printf 'QUICK_INSTALL_CANDIDATE=%q\n' "$( [ "$QUICK_INSTALL_CANDIDATE" = true ] && echo 1 || echo 0 )"
        printf 'QUICK_INSTALL_SCRIPT=/root/fresh-host/quick-install.sh\n'
        printf 'QUICK_INSTALL_LOCAL=/root/fresh-host/quick-install-local.sh\n'
        printf 'export GITHUB_REPO=%q\n' "$GITHUB_REPO"
    } > "${CURRENT_ROOT}/root/fresh-host/env"
    chmod 0644 "${CURRENT_ROOT}/root/fresh-host/env"
}

write_common() {
    cat > "${CURRENT_ROOT}/root/fresh-host/common.sh" <<'EOS'
#!/bin/bash
# In-container helpers for fresh-host.sh. Records PASS/FAIL lines and a final
# LEG_RC; the host driver turns those into lib.sh assertions.
set -uo pipefail
: "${LEG:?LEG must be set before sourcing fresh-host-common.sh}"
# shellcheck disable=SC1091
. /root/fresh-host/env
OUT="/var/fresh-host-${LEG}-results.txt"
: > "$OUT"

section() { printf '\n=== %s ===\n' "$*" >>"$OUT"; }
record() { printf '%s\n' "$*" >>"$OUT"; }
ok() { printf 'PASS %s\n' "$*" >>"$OUT"; }
bad() { printf 'FAIL %s\n' "$*" >>"$OUT"; }

wait_cmd() {
    local desc="$1" timeout="$2" waited=0
    shift 2
    while ! "$@" >/dev/null 2>&1; do
        sleep 2
        waited=$((waited + 2))
        if [ "$waited" -ge "$timeout" ]; then
            bad "$desc (timeout ${timeout}s)"
            return 1
        fi
    done
    ok "$desc"
    return 0
}

http_code() {
    local url="$1" out="$2"
    curl -sS --max-time 10 -o "$out" -w '%{http_code}' "$url" 2>/dev/null || true
}

wait_http_200() {
    local desc="$1" url="$2" out="$3" timeout="${4:-120}" waited=0 code
    while :; do
        code="$(http_code "$url" "$out")"
        if [ "$code" = "200" ]; then
            ok "$desc (HTTP 200)"
            return 0
        fi
        sleep 2
        waited=$((waited + 2))
        if [ "$waited" -ge "$timeout" ]; then
            bad "$desc (HTTP ${code:-no-response})"
            return 1
        fi
    done
}

unit_field() {
    systemctl show "$1" -p "$2" --value 2>/dev/null | head -1
}

# A failing unit's journal is the only place the reason exists. Record a
# bounded tail (and the resolved unit file) in the transcript so a failed
# lane is diagnosable from the uploaded evidence instead of a crash-loop
# count alone.
unit_diagnostics() {
    local u="$1" line
    record "--- journalctl -u ${u} (last 80 lines) ---"
    journalctl -u "$u" -n 80 --no-pager 2>/dev/null \
        | while IFS= read -r line; do record "  ${line}"; done
    record "--- systemctl cat ${u} ---"
    systemctl cat "$u" 2>/dev/null \
        | while IFS= read -r line; do record "  ${line}"; done
    record "--- end diagnostics for ${u} ---"
}

wait_units_active() {
    local timeout="${1:-180}" waited=0 all u
    while :; do
        all=1
        for u in chv-controlplane chv-agent chv-stord chv-nwd; do
            [ "$(systemctl is-active "$u" 2>/dev/null || true)" = "active" ] || all=0
        done
        if [ "$all" -eq 1 ]; then
            ok "four CHV units active"
            return 0
        fi
        sleep 2
        waited=$((waited + 2))
        if [ "$waited" -ge "$timeout" ]; then
            bad "four CHV units active (timeout ${timeout}s)"
            return 1
        fi
    done
}

check_units_contract() {
    local u state sub nrest diagnosed
    for u in chv-controlplane chv-agent chv-stord chv-nwd; do
        diagnosed=0
        state="$(unit_field "$u" ActiveState)"
        sub="$(unit_field "$u" SubState)"
        nrest="$(unit_field "$u" NRestarts)"
        record "UNIT $u ActiveState=${state:-missing} SubState=${sub:-missing} NRestarts=${nrest:-missing}"
        if [ "$state" = "active" ] && [ "$sub" = "running" ]; then
            ok "${u} active/running"
        else
            bad "${u} not active/running (state=${state:-missing} sub=${sub:-missing})"
            unit_diagnostics "$u"
            diagnosed=1
        fi
        if [ "$nrest" = "0" ]; then
            ok "${u} zero restarts"
        else
            bad "${u} restarted (NRestarts=${nrest:-missing})"
            [ "$diagnosed" -eq 1 ] || unit_diagnostics "$u"
        fi
    done
}

check_encryption() {
    local mode="${1:-candidate}" stat journal
    stat="$(stat -c '%a %U:%G' /etc/chv/encryption.env 2>/dev/null || true)"
    record "ENCRYPTION_ENV=${stat:-missing}"
    if [ "$stat" = "600 root:root" ]; then
        ok "encryption.env is 0600 root:root"
    else
        bad "encryption.env mode/owner is '${stat:-missing}', expected 600 root:root"
    fi
    if grep -q '^CHV_ENCRYPTION_KEY=[0-9a-f]\{64\}$' /etc/chv/encryption.env 2>/dev/null; then
        ok "CHV_ENCRYPTION_KEY is 64 hex chars"
    else
        bad "CHV_ENCRYPTION_KEY is missing or malformed"
    fi
    if [ "$mode" = "static" ]; then
        # The packages leg expects the units to be down without generated
        # certs, so an empty journal proves nothing. Do not claim the check.
        record "ENCRYPTION_JOURNAL=skipped (static packaging leg; services expected down)"
        return 0
    fi
    journal="$(journalctl -u chv-controlplane --no-pager 2>/dev/null || true)"
    # The candidate's warning text differs from v0.3.0's. Match both so the
    # check cannot pass just because the runtime changed its wording.
    if printf '%s' "$journal" | grep -qE 'S3 credentials will be stored in plaintext|deprecated legacy CHV_JWT_SECRET fallback'; then
        bad "control plane is not using a dedicated encryption key (plaintext/fallback warning logged)"
    elif [ "$mode" = "previous" ]; then
        # v0.3.0 logs nothing on the dedicated-key path; the absence of the
        # warning is the strongest assertion that binary supports.
        ok "previous release logged no plaintext/fallback warning"
    elif printf '%s' "$journal" | grep -q 'credential encryption key source: dedicated key'; then
        ok "control plane loaded the dedicated encryption key"
    else
        bad "candidate did not log the dedicated encryption key source"
    fi
}

TOKEN=""
ADMIN_PW=""
LOGIN_HTTP=""

# api_login <password> — control-plane API login; sets TOKEN on success.
api_login() {
    local pw="$1" body=/tmp/fresh-host-login.json
    LOGIN_HTTP="$(curl -sS --max-time 10 -o "$body" -w '%{http_code}' \
        -X POST http://127.0.0.1:8080/api/v1/auth/login \
        -H 'Content-Type: application/json' \
        --data-binary "{\"username\":\"admin\",\"password\":\"${pw}\"}" 2>/dev/null || true)"
    if [ "$LOGIN_HTTP" = "200" ] && grep -q '"token"' "$body" 2>/dev/null; then
        TOKEN="$(grep -o '"token":"[^"]*"' "$body" | head -1 | sed 's/^"token":"//; s/"$//')"
        [ -n "$TOKEN" ]
        return
    fi
    return 1
}

# wait_api_login — retry the current ADMIN_PW until the API answers. The
# control plane needs a moment after a fresh install or a package-upgrade
# restart; a single immediate login would race the listener.
wait_api_login() {
    local i
    for i in $(seq 1 60); do
        if api_login "${ADMIN_PW:-}"; then
            return 0
        fi
        sleep 2
    done
    return 1
}

check_admin_login() {
    if [ -z "${ADMIN_PW:-}" ]; then
        if [ ! -s /etc/chv/initial_admin_password ]; then
            bad "admin login round-trip (initial_admin_password missing)"
            return 1
        fi
        ADMIN_PW="$(cat /etc/chv/initial_admin_password)"
    fi
    if wait_api_login; then
        ok "admin login round-trip"
        return 0
    fi
    bad "admin login round-trip (HTTP ${LOGIN_HTTP:-no-response})"
    return 1
}

# The bootstrap admin is seeded with must_change_password=1 and install.sh
# restores that flag before exiting. The BFF denies every non-auth route for
# such a token (403 PASSWORD_CHANGE_REQUIRED), so node visibility cannot be
# asserted until the first-login rotation happens.
#
# Rotate through the documented API (`POST /v1/auth/change-password`) rather
# than a direct `sqlite3` UPDATE against the live database. The earlier
# fixture opened the live WAL database from a second process (UPDATE plus
# COUNT(*) reads) while the control plane was serving; the v0.3.0 -> rc.2
# version-assert runs (42116072364, 42118425284) then hit
# "database disk image is malformed", and the product must not be blamed for
# a test-only concurrent writer. The API path also proves the operator can
# actually rotate the seeded credential, which the core contract depends on.
rotate_admin_password() {
    local new_pw code body=/tmp/fresh-host-change-password.json
    if [ -z "$TOKEN" ]; then
        bad "first-login password change (no admin token)"
        return 1
    fi
    new_pw="FreshHost-$(openssl rand -hex 12)"
    code="$(curl -sS --max-time 15 -o "$body" -w '%{http_code}' \
        -X POST http://127.0.0.1:8080/v1/auth/change-password \
        -H "Authorization: Bearer ${TOKEN}" \
        -H 'Content-Type: application/json' \
        --data-binary "{\"current_password\":\"${ADMIN_PW}\",\"new_password\":\"${new_pw}\"}" 2>/dev/null || true)"
    if [ "$code" != "200" ] || ! grep -q '"ok":true' "$body" 2>/dev/null; then
        bad "first-login password change (HTTP ${code:-no-response})"
        return 1
    fi
    ok "first-login password change accepted"
    # The change bumps token_version and revokes the token that made the
    # request; the node-visibility check needs a session minted after it.
    ADMIN_PW="$new_pw"
    if api_login "$new_pw"; then
        ok "admin re-login with the rotated password"
        return 0
    fi
    bad "admin re-login with the rotated password (HTTP ${LOGIN_HTTP:-no-response})"
    return 1
}

# A clean install must preserve the documented first-login credential across a
# control-plane restart. Older installers seeded the admin with a second
# sqlite3 writer while the service was live; that writer unlinked the WAL the
# service had open, so the rotation was acknowledged and then silently lost on
# the next restart. The defect was fixed in scripts/install.sh for rc.3 and in
# scripts/quick-install.sh for rc.4 (both use chv_sqlite_offline). Assert both
# the durable result and the direct cause: no pooled connection may hold a
# "(deleted)" WAL.
check_credential_persistence() {
    local before_pid after_pid
    section "credential persistence across control-plane restart"
    if [ -z "${ADMIN_PW:-}" ]; then
        bad "credential persistence (no rotated admin password)"
        return 1
    fi
    before_pid="$(systemctl show -p MainPID --value chv-controlplane 2>/dev/null || true)"
    if systemctl restart chv-controlplane >/dev/null 2>&1; then
        ok "control plane restarted"
    else
        bad "control plane failed to restart"
        return 1
    fi
    after_pid="$(systemctl show -p MainPID --value chv-controlplane 2>/dev/null || true)"
    if [ -n "${before_pid:-}" ] && [ "$before_pid" != "0" ] \
        && [ -n "${after_pid:-}" ] && [ "$after_pid" != "$before_pid" ]; then
        ok "control plane came back with a new process (${before_pid} -> ${after_pid})"
    else
        bad "control plane did not restart (PID ${before_pid:-unknown} -> ${after_pid:-unknown})"
    fi
    if wait_api_login; then
        ok "rotated admin credential survived the restart"
    else
        bad "rotated admin credential was lost across the restart (HTTP ${LOGIN_HTTP:-no-response})"
    fi
    if [ -d "/proc/${after_pid:-}/fd" ] \
        && ls -l "/proc/${after_pid}/fd" 2>/dev/null | grep -qF 'controlplane.db-wal (deleted)'; then
        bad "control plane holds an unlinked database WAL after the restart"
    else
        ok "control plane holds the on-disk database WAL after the restart"
    fi
}

# Read-only control-plane diagnostics for a failed bring-up. The DB/WAL state,
# the open file descriptors and the filesystem type are the facts that
# distinguish a product defect from a storage/runner problem.
#
# Never open the live WAL database from a second process here: a CLI
# connection can checkpoint or delete the WAL the control plane is using and
# destroy the state under investigation. Snapshot the raw files and inspect
# the copy instead.
diagnose_controlplane() {
    local db=/var/lib/chv/controlplane.db copy=/tmp/fresh-host-db-diagnostic pid
    record "--- control-plane database diagnostics ---"
    ls -l "$db" "$db-wal" "$db-shm" 2>&1 | while IFS= read -r line; do record "  ${line}"; done
    pid="$(systemctl show -p MainPID --value chv-controlplane 2>/dev/null || true)"
    record "  MainPID=${pid:-unknown}"
    if [ -n "${pid:-}" ] && [ "$pid" != "0" ] && [ -d "/proc/${pid}/fd" ]; then
        record "  open fds matching ${db} ('(deleted)' means the file was replaced/unlinked under the service):"
        ls -l "/proc/${pid}/fd" 2>/dev/null | grep -F "$db" \
            | while IFS= read -r line; do record "    ${line}"; done
    fi
    rm -rf "$copy"
    mkdir -p "$copy"
    if cp -a "$db" "$copy/controlplane.db" 2>/dev/null; then
        cp -a "$db-wal" "$copy/controlplane.db-wal" 2>/dev/null || true
        record "  snapshot: $(ls -l "$copy" | tail -n +2 | awk '{print $5, $9}' | tr '\n' ' ')"
        if command -v sqlite3 >/dev/null 2>&1; then
            record "  sqlite3 --version: $(sqlite3 --version 2>&1 | head -1)"
            record "  PRAGMA integrity_check(5) on the snapshot:"
            sqlite3 -cmd '.timeout 5000' "$copy/controlplane.db" 'PRAGMA integrity_check(5);' 2>&1 \
                | while IFS= read -r line; do record "    ${line}"; done
        fi
    else
        record "  could not snapshot the database"
    fi
    record "  df -T /var/lib/chv: $(df -T /var/lib/chv 2>&1 | tail -1)"
    record "  findmnt: $(findmnt -no FSTYPE,SOURCE,TARGET /var/lib/chv 2>/dev/null || echo unknown)"
    record "  dmesg tail:"
    dmesg 2>/dev/null | tail -20 | while IFS= read -r line; do record "    ${line}"; done
    record "--- end control-plane database diagnostics ---"
}

# check_node_visible [expected_version]
#
# The base assertion is that the node is listed at all. The optional
# expected_version covers the in-place-upgrade contract (#720): the node row
# must follow the agent's own ReportServiceVersions report, which the agent
# sends every 6th 5 s reconcile tick (~30 s). After a v0.3.0 -> 0.4.0 package
# upgrade the row still carries the enrollment version when the check starts,
# so a version assertion has to poll past at least one report interval; an
# immediate read would pass visibility while silently proving nothing about
# the refresh.
check_node_visible() {
    local expect_version="${1:-}" body code i out=/tmp/fresh-host-nodes.json
    if [ -z "$TOKEN" ]; then
        bad "node enrolled and visible (no auth token)"
        return 1
    fi
    for i in $(seq 1 75); do
        code="$(curl -sS --max-time 10 -o "$out" -w '%{http_code}' -X POST http://127.0.0.1:8080/v1/nodes \
            -H "Authorization: Bearer ${TOKEN}" \
            -H 'Content-Type: application/json' -d '{}' 2>/dev/null || true)"
        body="$(cat "$out" 2>/dev/null || true)"
        if printf '%s' "$body" | grep -q '"node_id"'; then
            record "NODES_HTTP=${code}"
            record "NODES=${body}"
            if [ -z "$expect_version" ]; then
                ok "node enrolled and visible"
                return 0
            fi
            if printf '%s' "$body" | grep -q "\"version\":\"${expect_version}\""; then
                ok "node visible and reports version ${expect_version}"
                return 0
            fi
        fi
        sleep 2
    done
    record "NODES_LAST_HTTP=${code:-no-response}"
    record "NODES_LAST=${body:-<no body>}"
    record "--- journalctl -u chv-controlplane (last 80 lines) ---"
    journalctl -u chv-controlplane -n 80 --no-pager 2>/dev/null \
        | while IFS= read -r line; do record "  ${line}"; done
    record "--- end controlplane journal ---"
    diagnose_controlplane
    if [ -n "$expect_version" ]; then
        bad "node did not report version ${expect_version} within 150 s (last HTTP ${code:-no-response}: ${body:-<no body>})"
    else
        bad "node enrolled and visible (timeout)"
    fi
    return 1
}

check_webui() {
    local url="${1:-http://127.0.0.1:8080/}" out=/tmp/fresh-host-ui.html code
    code="$(http_code "$url" "$out")"
    if [ "$code" = "200" ] && grep -qi '<!doctype html>' "$out" 2>/dev/null; then
        ok "WebUI served on ${url}"
    else
        bad "WebUI served on ${url} (HTTP ${code:-no-response})"
    fi
}

check_metrics() {
    local spec name port code out
    for spec in controlplane:9103 agent:9901 stord:9101 nwd:9102; do
        name="${spec%%:*}"
        port="${spec##*:}"
        out="/tmp/fresh-host-${name}-metrics.txt"
        code="$(http_code "http://127.0.0.1:${port}/metrics" "$out")"
        if [ "$code" = "200" ]; then
            ok "${name} /metrics is HTTP 200"
            if [ -s "$out" ]; then
                ok "${name} /metrics body is non-empty"
            else
                bad "${name} /metrics body is empty"
            fi
        else
            bad "${name} /metrics is HTTP ${code:-no-response} on :${port}"
        fi
    done
}

check_binaries_version() {
    local expect_version="$1" expect_sha="${2:-}" b v short_sha
    short_sha="${expect_sha:0:7}"
    for b in chv-controlplane chv-agent chv-stord chv-nwd; do
        v="$("/usr/bin/${b}" --version 2>/dev/null | head -1 || true)"
        record "VERSION ${b}=${v:-missing}"
        case "$v" in
            *"$expect_version"*) ok "${b} reports version ${expect_version}" ;;
            *) bad "${b} does not report version ${expect_version}: ${v:-missing}" ;;
        esac
        if [ -n "$short_sha" ]; then
            case "$v" in
                *"$short_sha"*) ok "${b} reports candidate commit ${short_sha}" ;;
                *) bad "${b} does not report candidate commit ${short_sha}: ${v:-missing}" ;;
            esac
        fi
    done
}

check_bringup() {
    local encryption_mode="${1:-candidate}"
    local webui_url="${2:-http://127.0.0.1:8080/}"
    local expected_node_version="${3:-}"
    section "bring-up contract"
    wait_units_active 180 || true
    check_units_contract
    check_encryption "$encryption_mode"
    check_admin_login || true
    rotate_admin_password || true
    check_webui "$webui_url"
    check_node_visible "$expected_node_version" || true
    check_metrics
}

check_chv_ports_free() {
    local port
    for port in 8080 8443 8444 9101 9102 9103 9901; do
        if ss -ltnH 2>/dev/null | awk -v p=":${port}" '$4 ~ p "$" { found = 1 } END { exit !found }'; then
            bad "listener on :${port} remains after uninstall"
        else
            ok "listener :${port} released"
        fi
    done
}

finish() {
    local failures rc=0
    failures="$(grep -c '^FAIL ' "$OUT" 2>/dev/null || true)"
    [ "${failures:-0}" -eq 0 ] || rc=1
    printf 'LEG_FAILURES=%s\n' "${failures:-0}" >>"$OUT"
    printf 'LEG_RC=%s\n' "$rc" >>"$OUT"
    sync
    systemctl poweroff >/dev/null 2>&1 || true
    exit "$rc"
}
EOS
    chmod 0755 "${CURRENT_ROOT}/root/fresh-host/common.sh"
}

write_leg_script() {
    local leg="$1"
    local body="$2"
    {
        printf '#!/bin/bash\nset -uo pipefail\nLEG=%q\n# shellcheck source=/dev/null\n. /root/fresh-host/common.sh\n' "$leg"
        printf '%s\n' "$body"
    } > "${CURRENT_ROOT}/usr/local/bin/fresh-host-leg.sh"
    chmod 0755 "${CURRENT_ROOT}/usr/local/bin/fresh-host-leg.sh"
}

run_boot_leg() {
    local leg="$1" timeout="$2" body="$3"
    prepare_root "$leg" || return 1
    write_common
    write_leg_script "$leg" "$body"
    CURRENT_MACHINE="fresh-host-${leg}-$(date -u +%s)"

    cat > "${CURRENT_ROOT}/etc/systemd/system/fresh-host-leg.service" <<EOF
[Unit]
Description=Fresh-host qualification leg: ${leg}
After=multi-user.target

[Service]
Type=oneshot
ExecStart=/usr/local/bin/fresh-host-leg.sh
TimeoutStartSec=${timeout}

[Install]
WantedBy=multi-user.target
EOF

    if ! systemd-nspawn -q -D "$CURRENT_ROOT" --machine="${CURRENT_MACHINE}-setup" \
        systemctl enable fresh-host-leg.service \
        >"${EVIDENCE_DIR}/${leg}-boot.log" 2>&1; then
        # Record the failure row too: a marker with no row for this leg lets
        # the summary silently omit it. Leave CURRENT_ROOT set so the EXIT
        # trap preserves it for post-mortem (QUAL_ERRORS is now non-zero).
        qual_error "${leg}: failed to enable the one-shot service (see ${leg}-boot.log)"
        record_result "$leg" "fail" "1" "0" "evidence/${leg}-boot.log" ""
        return 1
    fi

    qual_info "booting ${leg} container (timeout ${timeout}s)"
    local started duration transcript rc status
    started="$(date +%s)"
    timeout "$((timeout + 120))" systemd-nspawn -b -D "$CURRENT_ROOT" --machine="$CURRENT_MACHINE" \
        >"${EVIDENCE_DIR}/${leg}-boot.log" 2>&1 \
        || qual_warn "${leg}: container boot ended via timeout/failure (see ${leg}-boot.log)"
    duration="$(( $(date +%s) - started ))"
    machinectl terminate "$CURRENT_MACHINE" 2>/dev/null || true

    transcript="${CURRENT_ROOT}/var/fresh-host-${leg}-results.txt"
    if [ -s "$transcript" ]; then
        cp "$transcript" "${EVIDENCE_DIR}/${leg}-results.txt"
        qual_pass "${leg}: transcript collected"
    else
        qual_error "${leg}: no transcript (boot log: ${EVIDENCE_DIR}/${leg}-boot.log)"
        record_result "$leg" "fail" "1" "$duration" "evidence/${leg}-boot.log" ""
        return 1
    fi

    local line fail_lines
    fail_lines="$(grep -c '^FAIL ' "$transcript" 2>/dev/null || true)"
    while IFS= read -r line; do
        qual_error "${leg}: ${line#FAIL }"
    done < <(grep '^FAIL ' "$transcript" || true)

    rc="$(sed -n 's/^LEG_RC=//p' "$transcript" | tail -1)"
    [ -n "$rc" ] || rc=1
    status="pass"
    if [ "$rc" -ne 0 ] || [ "${fail_lines:-0}" -gt 0 ]; then
        status="fail"
    fi
    # A leg that dies without emitting a FAIL line still fails the gate.
    if [ "$status" = "fail" ] && [ "${fail_lines:-0}" -eq 0 ]; then
        qual_error "${leg}: leg exited rc=${rc} without a FAIL line (see ${leg}-results.txt)"
    fi
    record_result "$leg" "$status" "$rc" "$duration" \
        "evidence/${leg}-results.txt" "evidence/${leg}-results.txt"

    if [ "$status" = "fail" ] || [ "$KEEP" = true ]; then
        qual_info "${leg}: container root preserved at ${CURRENT_ROOT}"
    else
        rm -rf "$CURRENT_ROOT"
    fi

    # Make the in-container transcript's non-failing markers visible in the
    # job log without dumping the whole thing.
    grep -E '^(PASS|LEG_)' "$transcript" | tail -n 20 || true
    CURRENT_ROOT=""
    CURRENT_MACHINE=""
    [ "$status" = "pass" ]
}

# ---------------------------------------------------------------------------
# Leg bodies (run INSIDE the container; quoted so host variables do not leak)
# ---------------------------------------------------------------------------
LEG_INSTALL_TARBALL=$(cat <<'EOS'
section "candidate install (release tarball)"
mkdir -p /root/fresh-host/candidate
if ! tar -xzf "$CANDIDATE_TARBALL" -C /root/fresh-host/candidate; then
    bad "candidate tarball extraction failed"
    finish
fi
INSTALLER="/root/fresh-host/candidate/chv-${CANDIDATE_VERSION}-linux-amd64/install.sh"
if [ ! -f "$INSTALLER" ]; then
    bad "installer not found at ${INSTALLER} (tarball top directory does not match --candidate-version ${CANDIDATE_VERSION})"
    finish
fi
env INSTALL_CHV_TARBALL_PATH="$CANDIDATE_TARBALL" \
    INSTALL_CHV_VERSION="$CANDIDATE_VERSION" \
    INSTALL_CHV_TARBALL_SHA256="$CANDIDATE_SHA256" \
    INSTALL_CHV_NO_SEED=1 INSTALL_CHV_NO_BRIDGE=1 \
    bash "$INSTALLER" > /var/fresh-host-install.log 2>&1
rc=$?
record "INSTALL_RC=$rc"
if [ "$rc" -eq 0 ]; then
    ok "candidate install.sh returned rc=0"
else
    bad "candidate install.sh returned rc=${rc}"
    tail -n 30 /var/fresh-host-install.log >>"$OUT"
    finish
fi
check_bringup candidate http://127.0.0.1:8080/ "$CANDIDATE_VERSION"
check_credential_persistence
check_binaries_version "$CANDIDATE_VERSION" "$CANDIDATE_SHA"
section "WebUI edge"
wait_http_200 "nginx edge serves the WebUI" http://127.0.0.1/ /tmp/fresh-host-edge.html 60 || true

section "uninstall (software removed, data preserved)"
bash "$QUICK_INSTALL_LOCAL" --uninstall > /var/fresh-host-uninstall.log 2>&1
rc=$?
record "UNINSTALL_RC=$rc"
if [ "$rc" -eq 0 ]; then ok "quick-install --uninstall returned rc=0"; else bad "quick-install --uninstall returned rc=${rc}"; tail -n 20 /var/fresh-host-uninstall.log >>"$OUT"; fi
for u in chv-controlplane chv-agent chv-stord chv-nwd; do
    state="$(systemctl is-active "$u" 2>/dev/null || true)"
    case "$state" in
        inactive|failed|unknown|"") ok "${u} stopped" ;;
        *) bad "${u} still ${state} after uninstall" ;;
    esac
    if [ -e "/etc/systemd/system/${u}.service" ]; then
        bad "${u}.service still present after uninstall"
    else
        ok "${u}.service removed"
    fi
done
check_chv_ports_free
for p in /etc/chv/controlplane.toml /var/lib/chv/controlplane.db /etc/chv/encryption.env; do
    if [ -e "$p" ]; then ok "uninstall preserved ${p}"; else bad "uninstall removed ${p}"; fi
done
for p in /opt/chv/ui /usr/local/share/chv/migrations /etc/nginx/sites-enabled/chv; do
    [ -e "$p" ] && record "UNINSTALL_RESIDUE=${p}"
done

section "purge (data and configuration removed)"
bash "$QUICK_INSTALL_LOCAL" --uninstall --purge > /var/fresh-host-purge.log 2>&1
rc=$?
record "PURGE_RC=$rc"
if [ "$rc" -eq 0 ]; then ok "quick-install --uninstall --purge returned rc=0"; else bad "quick-install --uninstall --purge returned rc=${rc}"; tail -n 20 /var/fresh-host-purge.log >>"$OUT"; fi
for p in /etc/chv /var/lib/chv /var/log/chv /run/chv; do
    if [ -e "$p" ]; then bad "purge left ${p}"; else ok "purge removed ${p}"; fi
done
for b in chv-controlplane chv-agent chv-stord chv-nwd; do
    if [ -e "/usr/bin/${b}" ]; then bad "purge left /usr/bin/${b}"; else ok "purge removed /usr/bin/${b}"; fi
done
check_chv_ports_free
finish
EOS
)

LEG_INSTALL_PACKAGES=$(cat <<'EOS'
section "candidate install (deb packages, static contract)"
export DEBIAN_FRONTEND=noninteractive
# dpkg resolves inter-package ordering but not remote dependencies; the
# documented install (scripts/package/smoke-deb.sh, release.yml) is
# `dpkg -i` followed by `apt-get install -f`. Accept the dpkg rc only when
# the follow-up repair and a clean `dpkg --audit` prove the install landed.
apt-get update -qq > /var/fresh-host-apt-update.log 2>&1
record "APT_UPDATE_RC=$?"
dpkg -i /root/fresh-host/packages/*.deb > /var/fresh-host-packages.log 2>&1
rc=$?
record "DPKG_RC=$rc"
apt-get install -f -y -qq >> /var/fresh-host-packages.log 2>&1
record "DPKG_FIX_RC=$?"
audit="$(dpkg --audit 2>&1 || true)"
if [ -n "$audit" ]; then
    bad "dpkg --audit reports unresolved package state"
    printf '%s\n' "$audit" >>"$OUT"
else
    ok "dpkg --audit reports no unresolved package state (initial dpkg rc=${rc})"
fi
for pkg in chv-controlplane chv-node chvctl; do
    v="$(dpkg-query -W -f='${Version}' "$pkg" 2>/dev/null || true)"
    record "PACKAGE ${pkg}=${v:-missing}"
    [ -n "$v" ] && ok "${pkg} package installed (${v})" || bad "${pkg} package is not installed"
done
check_binaries_version "$CANDIDATE_VERSION" "$CANDIDATE_SHA"
for u in chv-controlplane chv-agent chv-stord chv-nwd; do
    if [ -f "/lib/systemd/system/${u}.service" ]; then ok "packaged unit present: ${u}.service"; else bad "packaged unit missing: ${u}.service"; fi
done
for c in controlplane agent stord nwd; do
    if [ -f "/etc/chv/${c}.toml" ]; then ok "packaged conffile present: /etc/chv/${c}.toml"; else bad "packaged conffile missing: /etc/chv/${c}.toml"; fi
done
for u in chv chv-stord; do
    getent passwd "$u" >/dev/null 2>&1 && ok "service user exists: ${u}" || bad "service user missing: ${u}"
done
check_encryption static
section "packaged services (expected fail-closed without certs)"
for u in chv-controlplane chv-agent chv-stord chv-nwd; do
    record "PACKAGED_UNIT ${u} enabled=$(systemctl is-enabled "$u" 2>/dev/null || true)"
done
finish
EOS
)

LEG_UPGRADE=$(cat <<'EOS'
section "previous release install (${PREVIOUS_VERSION})"
mkdir -p /root/fresh-host/previous
if ! tar -xzf "$PREVIOUS_TARBALL" -C /root/fresh-host/previous; then
    bad "previous tarball extraction failed"
    finish
fi
PREV_INSTALLER="/root/fresh-host/previous/chv-${PREVIOUS_VERSION}-linux-amd64/install.sh"
if [ ! -f "$PREV_INSTALLER" ]; then
    bad "previous installer not found at ${PREV_INSTALLER}"
    finish
fi
env INSTALL_CHV_TARBALL_PATH="$PREVIOUS_TARBALL" \
    INSTALL_CHV_VERSION="$PREVIOUS_VERSION" \
    INSTALL_CHV_TARBALL_SHA256="$PREVIOUS_SHA256" \
    INSTALL_CHV_NO_SEED=1 INSTALL_CHV_NO_BRIDGE=1 \
    bash "$PREV_INSTALLER" > /var/fresh-host-previous-install.log 2>&1
rc=$?
record "PREVIOUS_INSTALL_RC=$rc"
if [ "$rc" -eq 0 ]; then
    ok "previous release install.sh returned rc=0"
else
    bad "previous release install.sh returned rc=${rc}"
    tail -n 30 /var/fresh-host-previous-install.log >>"$OUT"
    finish
fi
wait_cmd "previous release seeded the admin credential" 90 test -s /etc/chv/initial_admin_password || true
for b in chv-controlplane chv-agent chv-stord chv-nwd; do
    record "PREVIOUS_VERSION ${b}=$("/usr/bin/${b}" --version 2>/dev/null | head -1 || true)"
done

# v0.3.0 and v0.4.0-rc.1/rc.2 seeded the admin with a second sqlite3 writer
# while the control plane was live, which unlinked the WAL that service still
# had open. Those releases therefore lose API writes made in that process
# lifetime on the next restart - including the restart the upgrade performs.
# rc.3 fixed the installer. Restart the previous release once here so this leg
# measures upgrade preservation, not a known defect of the release being
# upgraded *from*; the clean-install leg asserts the candidate does not need
# this repair.
if systemctl restart chv-controlplane >/dev/null 2>&1; then
    ok "restarted the previous release to reopen its database WAL"
else
    bad "could not restart the previous release control plane"
fi
wait_cmd "previous release control plane healthy after restart" 60 \
    curl -sf http://127.0.0.1:8080/health || true

section "identifiable pre-upgrade state"
STATE_DIR=/var/fresh-host-upgrade-state
mkdir -p "$STATE_DIR"
missing_state=""
for p in /etc/chv/encryption.env /etc/chv/certs/ca.crt /etc/chv/certs/ca.key /etc/chv/controlplane.toml; do
    [ -e "$p" ] || missing_state="${missing_state} ${p}"
done
if [ -n "$missing_state" ]; then
    # Do not silently shrink the preservation set: a missing artifact would
    # otherwise drop out of before.sha256 and make the claim vacuous.
    bad "pre-upgrade state is missing:${missing_state}"
    : > "$STATE_DIR/before.sha256"
else
    sha256sum /etc/chv/encryption.env /etc/chv/certs/ca.crt /etc/chv/certs/ca.key /etc/chv/controlplane.toml \
        > "$STATE_DIR/before.sha256"
    state_lines="$(wc -l < "$STATE_DIR/before.sha256")"
    if [ "$state_lines" -eq 4 ]; then
        ok "hashed all 4 pre-upgrade state files"
    else
        bad "pre-upgrade state hash set has ${state_lines} entries, expected 4"
    fi
fi
printf '%s\n' "$PREVIOUS_VERSION" > /var/lib/chv/fresh-host-upgrade-marker

# This leg drives the product only through its own API. The earlier fixture
# opened the live WAL database with the sqlite3 CLI (a probe-user INSERT and
# two COUNT(*) reads). That second process checkpointed/deleted the WAL under
# the running control plane, and the v0.3.0 -> rc.2 version-assert runs
# (42116072364, 42118425284) then failed with
# "database disk image is malformed". A test-only concurrent writer must not
# be able to convict the product, so the probe account is created and checked
# over HTTP instead.
PROBE_USER="fresh-host-upgrade-probe"
probe_pw="$(openssl rand -hex 12)"

api_user_list() {
    local body=/tmp/fresh-host-users.json code
    code="$(curl -sS --max-time 10 -o "$body" -w '%{http_code}' \
        -X POST http://127.0.0.1:8080/v1/users \
        -H "Authorization: Bearer ${TOKEN}" \
        -H 'Content-Type: application/json' -d '{}' 2>/dev/null || true)"
    record "USERS_HTTP=${code}"
    cat "$body" 2>/dev/null || true
}

create_probe_user() {
    local body=/tmp/fresh-host-create-user.json code
    code="$(curl -sS --max-time 15 -o "$body" -w '%{http_code}' \
        -X POST http://127.0.0.1:8080/v1/users/create \
        -H "Authorization: Bearer ${TOKEN}" \
        -H 'Content-Type: application/json' \
        --data-binary "{\"username\":\"${PROBE_USER}\",\"password\":\"${probe_pw}\",\"role\":\"operator\",\"display_name\":\"Fresh Host Upgrade Probe\"}" 2>/dev/null || true)"
    if [ "$code" = "200" ] && grep -q "\"username\":\"${PROBE_USER}\"" "$body" 2>/dev/null; then
        ok "created identifiable operator account ${PROBE_USER}"
        return 0
    fi
    bad "failed to create identifiable operator account (HTTP ${code:-no-response})"
    record "CREATE_USER_BODY=$(cat "$body" 2>/dev/null || true)"
    return 1
}

# The seeded admin must complete the documented first-login rotation before
# the BFF permits non-auth routes, so this also exercises that flow on the
# previous release (the upgrade must not break a rotated account).
if check_admin_login && rotate_admin_password; then
    create_probe_user || true
else
    bad "could not establish an authenticated admin session before the upgrade"
fi
probe_users="$(api_user_list)"
if printf '%s' "$probe_users" | grep -q "\"username\":\"${PROBE_USER}\""; then
    record "PRE_UPGRADE_OPERATOR_COUNT=1"
    ok "pre-upgrade operator row present"
else
    record "PRE_UPGRADE_OPERATOR_COUNT=0"
    bad "pre-upgrade operator row missing"
fi

check_state_preserved() {
    local phase="$1" want now users
    section "state preservation (${phase})"
    while read -r want path; do
        [ -n "${path:-}" ] || continue
        if [ ! -e "$path" ]; then
            bad "${phase}: ${path} is missing"
            continue
        fi
        now="$(sha256sum "$path" | awk '{print $1}')"
        if [ "$now" = "$want" ]; then ok "${phase}: ${path} preserved"; else bad "${phase}: ${path} changed"; fi
    done < "$STATE_DIR/before.sha256"
    if [ -f /var/lib/chv/fresh-host-upgrade-marker ] \
        && grep -qx "$PREVIOUS_VERSION" /var/lib/chv/fresh-host-upgrade-marker; then
        ok "${phase}: marker file preserved"
    else
        bad "${phase}: marker file missing or changed"
    fi
    # Account preservation is asserted through the product API. A direct
    # sqlite3 read of the live database is exactly what destabilised the WAL
    # in the earlier runs.
    if ! wait_api_login; then
        bad "${phase}: could not authenticate to inspect accounts (HTTP ${LOGIN_HTTP:-no-response})"
        return 1
    fi
    users="$(api_user_list)"
    if printf '%s' "$users" | grep -q "\"username\":\"${PROBE_USER}\""; then
        ok "${phase}: operator account preserved"
    else
        bad "${phase}: operator account lost"
    fi
    if printf '%s' "$users" | grep -q '"username":"admin"'; then
        ok "${phase}: admin account preserved"
    else
        bad "${phase}: admin account lost"
    fi
}

section "candidate install.sh re-run over persistent state"
mkdir -p /root/fresh-host/candidate
if ! tar -xzf "$CANDIDATE_TARBALL" -C /root/fresh-host/candidate; then
    bad "candidate tarball extraction failed"
    finish
fi
INSTALLER="/root/fresh-host/candidate/chv-${CANDIDATE_VERSION}-linux-amd64/install.sh"
if [ ! -f "$INSTALLER" ]; then
    bad "candidate installer not found at ${INSTALLER} (top directory does not match --candidate-version ${CANDIDATE_VERSION})"
    finish
fi
env INSTALL_CHV_TARBALL_PATH="$CANDIDATE_TARBALL" \
    INSTALL_CHV_VERSION="$CANDIDATE_VERSION" \
    INSTALL_CHV_TARBALL_SHA256="$CANDIDATE_SHA256" \
    INSTALL_CHV_NO_SEED=1 INSTALL_CHV_NO_BRIDGE=1 \
    bash "$INSTALLER" > /var/fresh-host-candidate-rerun.log 2>&1
rc=$?
record "RERUN_RC=$rc"
UPGRADED=0
if [ "$rc" -eq 0 ]; then
    # The previous release left a database and VM state in place (the leg
    # just seeded both), so the candidate installer must refuse. rc=0 means
    # the documented fail-closed guard regressed; an in-place tarball
    # "upgrade" is a contract violation, not a pass.
    bad "candidate install.sh re-ran over persistent data (rc=0) — the documented fail-closed guard did not fire"
    tail -n 30 /var/fresh-host-candidate-rerun.log >>"$OUT"
    finish
elif grep -q 'Persistent CHV data exists' /var/fresh-host-candidate-rerun.log; then
    ok "candidate install.sh fail-closed on persistent data (documented guard)"
    record "RERUN_GUARD=persistent-data"
    check_state_preserved "after-rerun-guard"
    if [ "$HAS_PACKAGES" = "1" ]; then
        section "candidate upgrade via .deb packages (documented upgrade path)"
        DEBIAN_FRONTEND=noninteractive dpkg --force-confold -i /root/fresh-host/packages/*.deb \
            > /var/fresh-host-package-upgrade.log 2>&1
        rc=$?
        record "PACKAGE_UPGRADE_RC=$rc"
        DEBIAN_FRONTEND=noninteractive apt-get install -f -y -qq >> /var/fresh-host-package-upgrade.log 2>&1
        record "PACKAGE_UPGRADE_FIX_RC=$?"
        audit="$(dpkg --audit 2>&1 || true)"
        if [ -n "$audit" ]; then
            bad "dpkg --audit reports unresolved state after the package upgrade"
            printf '%s\n' "$audit" >>"$OUT"
        else
            ok "package upgrade left no unresolved dpkg state (initial rc=${rc})"
        fi
        systemctl daemon-reload >/dev/null 2>&1 || true
        if systemctl restart chv-controlplane chv-stord chv-nwd chv-agent >/dev/null 2>&1; then
            ok "services restarted after package upgrade"
        else
            bad "services failed to restart after package upgrade"
        fi
        # Record which unit file is actually active: install.sh writes units
        # to /etc/systemd/system, which shadow the packaged /lib copies on the
        # documented tarball-install → package-upgrade path. This is expected,
        # but the evidence must not imply the packaged units were exercised.
        for u in chv-controlplane chv-agent chv-stord chv-nwd; do
            record "UNIT_FRAGMENT ${u}=$(systemctl show -p FragmentPath --value "$u" 2>/dev/null || true)"
        done
        UPGRADED=1
        check_state_preserved "after-package-upgrade"
    else
        bad "no supported in-place upgrade: install.sh refused persistent data and no --packages DIR was supplied"
    fi
else
    bad "candidate install.sh failed for a reason other than the persistent-data guard"
    tail -n 30 /var/fresh-host-candidate-rerun.log >>"$OUT"
fi

if [ "$UPGRADED" = "1" ]; then
    check_binaries_version "$CANDIDATE_VERSION" "$CANDIDATE_SHA"
    # The package upgrade preserves the previous release's controlplane.toml
    # by contract, so the candidate binary keeps the v0.3.0 serving surface:
    # the nginx edge at :80 serves the static UI, and the config has no
    # [webui]. Assert that edge rather than a :8080 UI the preserved config
    # disables. Metrics stay asserted: the candidate binds its dedicated
    # listener on 127.0.0.1:9103 by default when metrics_bind is absent.
    section "preserved-edge surface (upgrade keeps previous config)"
    # The node row starts at the previous release's version and must follow
    # the upgraded agent's report (#720); poll past one ~30 s report interval.
    check_bringup candidate http://127.0.0.1/ "$CANDIDATE_VERSION"
else
    bad "upgrade did not complete; bring-up not asserted"
fi
finish
EOS
)

LEG_QUICK=$(cat <<'EOS'
section "published quick installer (pinned ref ${QUICK_REF})"
if [ ! -f "$QUICK_INSTALL_SCRIPT" ]; then
    bad "pinned quick-install.sh not staged"
    finish
fi
# The documented one-liner is curl-piped, so curl is a precondition, not an
# artifact under test. The shared debootstrap base predates this lane and may
# not carry it; install it before quick-install's own dependency pass.
if ! command -v curl >/dev/null 2>&1; then
    apt-get update -qq > /var/fresh-host-quick-deps.log 2>&1 \
        && apt-get install -y -qq curl >> /var/fresh-host-quick-deps.log 2>&1
    record "QUICK_CURL_BOOTSTRAP_RC=$?"
fi
command -v curl >/dev/null 2>&1 && ok "curl available for the quick installer" || bad "curl is unavailable for the quick installer"
chmod +x "$QUICK_INSTALL_SCRIPT"
env INSTALL_CHV_SKIP_DEPS=0 \
    bash "$QUICK_INSTALL_SCRIPT" --version "$QUICK_VERSION" > /var/fresh-host-quick.log 2>&1
rc=$?
record "QUICK_INSTALL_RC=$rc"
if [ "$rc" -eq 0 ]; then
    ok "quick installer returned rc=0 (ref ${QUICK_REF}, release ${QUICK_VERSION})"
else
    bad "quick installer returned rc=${rc} (ref ${QUICK_REF}, release ${QUICK_VERSION})"
    tail -n 30 /var/fresh-host-quick.log >>"$OUT"
    finish
fi
if [ "$QUICK_VERSION" = "$CANDIDATE_VERSION" ] \
    || [ "${QUICK_INSTALL_CANDIDATE:-0}" = "1" ]; then
    # A manual run may point the quick leg at the candidate itself, and a
    # post-publication run installs the candidate *release* whose version
    # carries an -rc.N suffix while the binaries report the base version.
    # Only a strictly older release may use the weaker previous-mode
    # assertion; the workflow sets --quick-install-candidate for the exact
    # tag it is qualifying.
    check_bringup
    check_credential_persistence
elif [ "$(printf '%s\n%s\n' "$QUICK_VERSION" "$CANDIDATE_VERSION" | sort -V | head -1)" = "$QUICK_VERSION" ]; then
    # QUICK_VERSION sorts strictly before the candidate: a previous release,
    # whose dedicated-key path logs nothing, so only the absence of the
    # fallback warning can be asserted.
    check_bringup previous
else
    # Newer than the candidate (manual misuse): assert the full candidate
    # contract rather than silently weakening it.
    check_bringup
fi
# RC binaries report the base version from VERSION (release.yml takes the
# -rc.N suffix from the tag), so compare against 0.4.0 for a 0.4.0-rc.1 run.
expected_bin_version="$QUICK_VERSION"
case "$expected_bin_version" in
    *-rc.*) expected_bin_version="${expected_bin_version%-rc.*}" ;;
esac
check_binaries_version "$expected_bin_version" ""
finish
EOS
)

# ---------------------------------------------------------------------------
# Run legs
# ---------------------------------------------------------------------------
qual_info "fresh-host qualification: candidate ${CANDIDATE_VERSION} (${CANDIDATE_SHA:-sha not supplied})"
qual_info "environment: debootstrap+systemd-nspawn ${SUITE}, shared host kernel/netns, no /dev/kvm"

# Optional legs that were not requested are recorded as skipped, never as a
# pass. The workflow always supplies packages and quick-install; a reduced
# manual run must still say exactly which claims it did not exercise.
if [ -z "$PACKAGES_DIR" ]; then
    record_result "install-packages" "skipped" "0" "0" "" "no --packages supplied"
fi
if [ "$SKIP_UPGRADE" = true ]; then
    record_result "upgrade" "skipped" "0" "0" "" "--skip-upgrade"
fi
if [ -z "$QUICK_REF" ]; then
    record_result "quick-install" "skipped" "0" "0" "" "no --quick-install ref supplied"
fi

run_boot_leg "install-tarball" 2400 "$LEG_INSTALL_TARBALL" || true

if [ -n "$PACKAGES_DIR" ]; then
    run_boot_leg "install-packages" 900 "$LEG_INSTALL_PACKAGES" || true
fi

if [ "$SKIP_UPGRADE" = false ]; then
    run_boot_leg "upgrade" 2400 "$LEG_UPGRADE" || true
fi

if [ -n "$QUICK_REF" ]; then
    run_boot_leg "quick-install" 2400 "$LEG_QUICK" || true
fi

# The uninstall layout residue is a known boundary of quick-install.sh on an
# install.sh layout: surface it as a warning, never as a pass.
if [ -f "${EVIDENCE_DIR}/install-tarball-results.txt" ]; then
    while IFS= read -r residue; do
        [ -n "$residue" ] && qual_warn "uninstall did not remove install.sh-layout path ${residue}"
    done < <(sed -n 's/^UNINSTALL_RESIDUE=//p' "${EVIDENCE_DIR}/install-tarball-results.txt")
fi

write_summary "$( [ "${QUAL_ERRORS}" -eq 0 ] && echo PASS || echo FAIL )"
qual_summary "fresh-host qualification"
[ "${QUAL_ERRORS}" -gt 0 ] && exit 1 || exit 0
