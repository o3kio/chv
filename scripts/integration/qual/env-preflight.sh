#!/usr/bin/env bash
# env-preflight.sh — provision and verify the qualification environment
# (prompt 04, M4.1).
#
# Idempotent: safe to re-run. Downloads are cached under CHV_QUAL_ROOT.
#
# Verifies / provides:
#   - root + /dev/kvm
#   - cloud-hypervisor v53.0 (pinned) at /usr/bin/cloud-hypervisor
#   - rust-hypervisor-firmware 0.5.0 (pinned) under CHV_QUAL_ROOT
#   - Ubuntu noble cloud image (qcow2) under CHV_QUAL_ROOT
#   - host tools: openssl, curl, python3, htpasswd (apache2-utils), jq
#   - records component digests for the evidence doc
#   - with --stage-binaries: builds the workspace in release mode and stages
#     the four daemons + chvctl into CHV_QUAL_ROOT/bin (records the repo SHA;
#     keep the checkout on the qualification candidate while doing so)
#
# Environment:
#   CHV_QUAL_ROOT   persistent root (default: /var/lib/chv/qual)
#   CHV_VERSION     pinned cloud-hypervisor version (default: v53.0)
#   FW_VERSION      pinned rust-hypervisor-firmware version (default: 0.5.0)
#   GUEST_IMAGE     guest seed filename (default: noble-server-cloudimg-amd64.img)

set -euo pipefail

STAGE_BINARIES=false
for arg in "$@"; do
    case "$arg" in
        --stage-binaries) STAGE_BINARIES=true ;;
        *) qual_die "unknown argument: $arg (usage: env-preflight.sh [--stage-binaries])" ;;
    esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
# shellcheck source=lib.sh
source "${SCRIPT_DIR}/lib.sh"

CHV_QUAL_ROOT="${CHV_QUAL_ROOT:-/var/lib/chv/qual}"
CHV_VERSION="${CHV_VERSION:-v53.0}"
FW_VERSION="${FW_VERSION:-0.5.0}"
GUEST_IMAGE="${GUEST_IMAGE:-noble-server-cloudimg-amd64.img}"

CHV_URL="https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/${CHV_VERSION}/cloud-hypervisor-static"
# NOTE: the firmware repository was renamed (rust-hypervisor-fw →
# rust-hypervisor-firmware); the asset URL below is the current one,
# verified reachable 2026-09-30 (prompt-04 plan §2).
FW_URL="https://github.com/cloud-hypervisor/rust-hypervisor-firmware/releases/download/${FW_VERSION}/hypervisor-fw"
GUEST_URL="https://cloud-images.ubuntu.com/noble/current/${GUEST_IMAGE}"

IMAGES_DIR="${CHV_QUAL_ROOT}/images"

echo "[QUAL] env-preflight starting (root=${CHV_QUAL_ROOT})"

# ---------------------------------------------------------------------------
# 1. Root + KVM
# ---------------------------------------------------------------------------
[ "$(id -u)" -eq 0 ] || qual_die "must run as root (qualification needs /dev/kvm, taps, nft)"
if [ -e /dev/kvm ]; then
    qual_pass "/dev/kvm present"
else
    qual_die "/dev/kvm missing — cannot qualify on this host"
fi

# ---------------------------------------------------------------------------
# 2. Host tools (apt-based; recorded in evidence)
# ---------------------------------------------------------------------------
# tool:package pairs (the htpasswd binary ships in apache2-utils).
# dnsmasq + genisoimage are product-declared runtime deps (install.sh
# installs them; nwd spawns DHCP-only dnsmasq instances and the agent
# builds the cloud-init NoCloud seed ISO with genisoimage). Without them
# the candidate degrades WARN-ONLY to a network-less guest (M4.4 finding
# N3) — the preflight must ensure them so the guest-path legs can run.
TOOL_PACKAGES=(
    "openssl:openssl"
    "curl:curl"
    "python3:python3"
    "htpasswd:apache2-utils"
    "jq:jq"
    "dnsmasq:dnsmasq"
    "genisoimage:genisoimage"
    # patch-guest-image.sh converts the stock cloud image into the bootable
    # qualification image; the nightly driver always calls it (#659).
    "qemu-img:qemu-utils"
)
MISSING_PACKAGES=""
for tp in "${TOOL_PACKAGES[@]}"; do
    tool="${tp%%:*}"
    pkg="${tp##*:}"
    command -v "$tool" >/dev/null 2>&1 || MISSING_PACKAGES="${MISSING_PACKAGES} ${pkg}"
done
if [ -n "${MISSING_PACKAGES}" ]; then
    qual_info "installing missing tool packages:${MISSING_PACKAGES}"
    # shellcheck disable=SC2086
    apt-get update -qq && apt-get install -y -qq ${MISSING_PACKAGES} >/dev/null
fi
for tp in "${TOOL_PACKAGES[@]}"; do
    command -v "${tp%%:*}" >/dev/null 2>&1 || qual_die "required tool missing after install: ${tp%%:*} (package ${tp##*:})"
done
qual_pass "host tools present (openssl, curl, python3, htpasswd, jq, dnsmasq, genisoimage, qemu-img)"

# The dnsmasq package's systemd unit must NOT run as a host DNS resolver:
# with its stock (all-comments) config it binds :53 broadly and forwards
# to whatever /etc/resolv.conf points at (here: systemd-resolved's
# 127.0.0.53 — a loop). CHV only needs the BINARY; nwd spawns its own
# DHCP-only instances (port=0) with configs under /run/chv/nwd. Stop and
# mask the unit so it cannot interfere (disposable qualification host —
# recorded in the evidence, not a general recommendation).
if command -v systemctl >/dev/null 2>&1 && systemctl is-active dnsmasq >/dev/null 2>&1; then
    systemctl stop dnsmasq >/dev/null 2>&1 || true
    systemctl mask dnsmasq >/dev/null 2>&1 || true
    qual_info "stopped+masked the system dnsmasq unit (host DNS stays on systemd-resolved)"
fi

# ---------------------------------------------------------------------------
# 3. cloud-hypervisor (pinned)
# ---------------------------------------------------------------------------
mkdir -p "$IMAGES_DIR" "${CHV_QUAL_ROOT}/bin"

if [ ! -x /usr/bin/cloud-hypervisor ]; then
    qual_info "downloading cloud-hypervisor ${CHV_VERSION} ..."
    curl -fsSL --retry 3 -o "${CHV_QUAL_ROOT}/bin/cloud-hypervisor" "$CHV_URL"
    install -m 0755 "${CHV_QUAL_ROOT}/bin/cloud-hypervisor" /usr/bin/cloud-hypervisor
fi

CHV_INSTALLED="$(/usr/bin/cloud-hypervisor --version 2>/dev/null | head -1 || true)"
case "$CHV_INSTALLED" in
    *"$CHV_VERSION"*) qual_pass "cloud-hypervisor pinned version: ${CHV_INSTALLED}" ;;
    *)
        qual_info "installed cloud-hypervisor is '${CHV_INSTALLED}' — reinstalling pinned ${CHV_VERSION}"
        curl -fsSL --retry 3 -o "${CHV_QUAL_ROOT}/bin/cloud-hypervisor" "$CHV_URL"
        install -m 0755 "${CHV_QUAL_ROOT}/bin/cloud-hypervisor" /usr/bin/cloud-hypervisor
        CHV_INSTALLED="$(/usr/bin/cloud-hypervisor --version 2>/dev/null | head -1)"
        case "$CHV_INSTALLED" in
            *"$CHV_VERSION"*) qual_pass "cloud-hypervisor pinned version: ${CHV_INSTALLED}" ;;
            *) qual_die "cloud-hypervisor version mismatch after install: ${CHV_INSTALLED}" ;;
        esac
        ;;
esac
qual_info "cloud-hypervisor sha256: $(sha256_of /usr/bin/cloud-hypervisor)"

# ---------------------------------------------------------------------------
# 4. rust-hypervisor-firmware (pinned)
# ---------------------------------------------------------------------------
FW_PATH="${CHV_QUAL_ROOT}/hypervisor-fw"
if [ ! -s "$FW_PATH" ]; then
    qual_info "downloading rust-hypervisor-firmware ${FW_VERSION} ..."
    curl -fsSL --retry 3 -o "$FW_PATH" "$FW_URL"
fi
FW_SHA="$(sha256_of "$FW_PATH")"
qual_pass "rust-hypervisor-firmware ${FW_VERSION} present (sha256 ${FW_SHA})"
# M2.5 recorded this firmware digest with the prefix 4a0a1e97 — assert the
# same artifact is in use.
case "$FW_SHA" in
    4a0a1e97*) qual_pass "firmware digest matches the M2.5-recorded prefix (4a0a1e97…)" ;;
    *) qual_warn "firmware digest prefix differs from the M2.5 record (4a0a1e97…): ${FW_SHA}" ;;
esac
install -m 0644 "$FW_PATH" /var/lib/chv/hypervisor-fw 2>/dev/null || {
    mkdir -p /var/lib/chv && install -m 0644 "$FW_PATH" /var/lib/chv/hypervisor-fw
}

# ---------------------------------------------------------------------------
# 5. Guest seed image (Ubuntu noble cloud image)
# ---------------------------------------------------------------------------
GUEST_PATH="${IMAGES_DIR}/${GUEST_IMAGE}"
if [ ! -s "$GUEST_PATH" ]; then
    qual_info "downloading Ubuntu noble cloud image (~600 MiB) ..."
    curl -fsSL --retry 3 -o "${GUEST_PATH}.part" "$GUEST_URL"
    mv "${GUEST_PATH}.part" "$GUEST_PATH"
fi
qual_pass "guest seed image present: ${GUEST_PATH} ($(du -h "$GUEST_PATH" | cut -f1), sha256 $(sha256_of "$GUEST_PATH" | cut -c1-16)…)"

# ---------------------------------------------------------------------------
# 6. Candidate binaries (optional --stage-binaries)
# ---------------------------------------------------------------------------
if [ "$STAGE_BINARIES" = true ]; then
    cd "${REPO_ROOT}" || qual_die "cannot cd to repo root ${REPO_ROOT}"
    REPO_SHA="$(git rev-parse HEAD)"
    REPO_DIRTY="$(git status --porcelain | wc -l | tr -d ' ')"
    if [ "$REPO_DIRTY" != "0" ]; then
        qual_die "repo is dirty (${REPO_DIRTY} modified paths) — qualification binaries must be built from a clean checkout of the candidate SHA"
    fi
    qual_info "building release binaries from ${REPO_SHA} ..."
    # Isolated target dir: never pollutes target/ and is gitignored (target-*).
    cargo build --release --target-dir "${CHV_QUAL_ROOT}/target" -p chv-controlplane -p chv-agent -p chv-stord -p chv-nwd -p chvctl
    for b in chv-controlplane chv-agent chv-stord chv-nwd chvctl; do
        install -m 0755 "${CHV_QUAL_ROOT}/target/release/${b}" "${CHV_QUAL_ROOT}/bin/${b}"
    done
    printf '%s\n' "$REPO_SHA" > "${CHV_QUAL_ROOT}/bin/CANDIDATE_SHA"
    qual_pass "candidate binaries staged from ${REPO_SHA} into ${CHV_QUAL_ROOT}/bin"
    for b in chv-controlplane chv-agent chv-stord chv-nwd chvctl; do
        qual_info "  ${b}: $("${CHV_QUAL_ROOT}/bin/${b}" --version 2>/dev/null | head -1)"
    done
fi

# ---------------------------------------------------------------------------
# 7. Resource headroom (recorded honestly — 4 vCPU / 7.8 GiB host)
# ---------------------------------------------------------------------------
qual_info "host: $(nproc) vCPU, $(free -h | awk '/Mem:/ {print $2}') RAM, $(df -h / | awk 'NR==2 {print $4}') free disk"
qual_info "kernel: $(uname -r)"

qual_summary "env-preflight"
