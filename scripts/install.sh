#!/bin/bash
# CHV All-in-One Installer
# Usage (requires bash — the installer uses `set -o pipefail`; dash/sh will not work):
#   curl -fsSL https://raw.githubusercontent.com/o3kio/chv/main/scripts/install.sh | sudo bash -s
#   curl -fsSL https://raw.githubusercontent.com/o3kio/chv/main/scripts/install.sh | sudo INSTALL_CHV_VERSION=0.3.0 bash -s
#   ./scripts/install.sh --wipe    # Full teardown before clean install
#   ./scripts/install.sh --fresh   # Alias for --wipe
#
# Environment variables:
#   INSTALL_CHV_VERSION         - Version to install (default: latest)
#   INSTALL_CHV_TARBALL_PATH    - Path to local release tarball (dev mode)
#   INSTALL_CHV_SKIP_DEPS       - Set to "1" to skip apt dependency installation
#   INSTALL_CHV_SKIP_CLOUD_HV   - Set to "1" to skip cloud-hypervisor download
#   INSTALL_CHV_ALLOW_VMM_MISMATCH - Set to "1" to keep a host cloud-hypervisor
#                                 whose major.minor differs from the qualified
#                                 pin (v53.0) instead of replacing it with the
#                                 pinned, digest-verified static binary. The
#                                 resulting install is OFF-PIN and UNQUALIFIED.
#   INSTALL_CHV_BRIDGE_IFACE    - Host interface to attach to bridge (default: ens19)
#   INSTALL_CHV_BRIDGE_NAME     - Bridge name (default: chvbr0)
#   INSTALL_CHV_BRIDGE_CIDR     - Bridge CIDR (default: 10.200.0.1/24)
#   INSTALL_CHV_NO_SEED         - Set to "1" to skip default network + test VM creation
#   INSTALL_CHV_NO_BRIDGE       - Set to "1" to skip the legacy host bridge/NAT
#                                 bootstrap (qualified networks are created via
#                                 the API and managed by chv-nwd as br-<net_id>;
#                                 the pre-created chvbr0 is a dev convenience)
#   INSTALL_CHV_WIPE            - Set to "1" to wipe the previous deployment first,
#                                 including existing VMs and the control-plane database

set -euo pipefail

# -----------------------------------------------------------------------------
# Configuration
# -----------------------------------------------------------------------------
INSTALL_CHV_VERSION="${INSTALL_CHV_VERSION:-latest}"
INSTALL_CHV_TARBALL_PATH="${INSTALL_CHV_TARBALL_PATH:-}"
INSTALL_CHV_SKIP_DEPS="${INSTALL_CHV_SKIP_DEPS:-0}"
INSTALL_CHV_SKIP_CLOUD_HV="${INSTALL_CHV_SKIP_CLOUD_HV:-0}"
INSTALL_CHV_NO_SEED="${INSTALL_CHV_NO_SEED:-0}"
INSTALL_CHV_NO_BRIDGE="${INSTALL_CHV_NO_BRIDGE:-0}"
INSTALL_CHV_WIPE="${INSTALL_CHV_WIPE:-0}"

# Parse CLI flags
for arg in "$@"; do
    case "$arg" in
        --wipe|--fresh) INSTALL_CHV_WIPE="1" ;;
    esac
done

# Network defaults
INSTALL_CHV_BRIDGE_IFACE="${INSTALL_CHV_BRIDGE_IFACE:-ens19}"
INSTALL_CHV_BRIDGE_NAME="${INSTALL_CHV_BRIDGE_NAME:-chvbr0}"
INSTALL_CHV_BRIDGE_CIDR="${INSTALL_CHV_BRIDGE_CIDR:-10.200.0.1/24}"

CHV_USER="chv"
CHV_CONFIG_DIR="/etc/chv"
CHV_DATA_DIR="/var/lib/chv"
CHV_LOG_DIR="/var/log/chv"
CHV_RUN_DIR="/run/chv"
CHV_UI_DIR="/opt/chv/ui"
CHV_MIGRATIONS_DIR="/usr/local/share/chv/migrations"
CHV_MONITORING_MIGRATIONS_DIR="/usr/local/share/chv/monitoring-migrations"
CHV_DB_PATH="${CHV_DATA_DIR}/controlplane.db"

GITHUB_REPO="${GITHUB_REPO:-o3kio/chv}"

# Populated later
EXTRACT_DIR=""
CLEANUP_TMPDIR=""
JWT_SECRET=""
BOOTSTRAP_TOKEN=""
CHV_NODE_ID=""

# -----------------------------------------------------------------------------
# Helpers
# -----------------------------------------------------------------------------
info() { echo "[INFO] $*"; }
warn() { echo "[WARN] $*" >&2; }
fatal() { echo "[ERROR] $*" >&2; exit 1; }
cmd_exists() { command -v "$1" &>/dev/null; }

list_chv_processes() {
    ps -eo pid=,comm=,args= | awk '
        $2 == "chv-agent" ||
        $2 == "chv-stord" ||
        $2 == "chv-nwd" ||
        $2 == "chv-controlplane" ||
        $2 == "chv-controlplan" {
            print
        }
    '
}

list_chv_dnsmasq_processes() {
    ps -eo pid=,comm=,args= | awk '
        $2 == "dnsmasq" && $0 ~ /\/run\/chv\/nwd\/dnsmasq-/ {
            print
        }
    '
}

list_chv_runtime_processes() {
    list_chv_processes
    list_chv_dnsmasq_processes
}

list_chv_socket_listeners() {
    if cmd_exists ss; then
        ss -xlpn | awk '/\/run\/chv\/(agent|stord|nwd)\/api\.sock/ { print }'
    fi
}

wait_for_chv_process_exit() {
    local attempt
    for attempt in {1..10}; do
        if [ -z "$(list_chv_runtime_processes)" ]; then
            return 0
        fi
        sleep 1
    done
    return 1
}

remove_running_chv_processes() {
    local pids
    mapfile -t pids < <(list_chv_runtime_processes | awk '{print $1}' | sort -u)
    if [ "${#pids[@]}" -eq 0 ]; then
        info "No CHV runtime processes remain."
        return 0
    fi

    warn "Terminating leftover CHV runtime processes: ${pids[*]}"
    kill -TERM "${pids[@]}" 2>/dev/null || true
    if ! wait_for_chv_process_exit; then
        mapfile -t pids < <(list_chv_runtime_processes | awk '{print $1}' | sort -u)
        if [ "${#pids[@]}" -gt 0 ]; then
            warn "Forcing leftover CHV runtime processes to exit: ${pids[*]}"
            kill -KILL "${pids[@]}" 2>/dev/null || true
        fi
    fi
}

validate_no_chv_processes() {
    local remaining
    remaining="$(list_chv_runtime_processes)"
    if [ -n "$remaining" ]; then
        fatal "CHV runtime processes are still running after cleanup:
$remaining"
    fi

    remaining="$(list_chv_socket_listeners)"
    if [ -n "$remaining" ]; then
        fatal "CHV runtime socket listeners are still active after cleanup:
$remaining"
    fi

    info "Validated no CHV processes or runtime socket listeners remain."
}

get_local_ip() {
    hostname -I 2>/dev/null | awk '{print $1}' || echo "127.0.0.1"
}

# -----------------------------------------------------------------------------
# Pre-flight checks
# -----------------------------------------------------------------------------
if [ "$(id -u)" -ne 0 ]; then
    fatal "This installer must be run as root. Try: sudo ./scripts/install.sh"
fi

ARCH=$(uname -m)
case "$ARCH" in
    x86_64)
        CHV_ARCH="amd64"
        ;;
    aarch64|arm64)
        CHV_ARCH="arm64"
        ;;
    *)
        fatal "Unsupported architecture: $ARCH. Only x86_64 and arm64 are supported."
        ;;
esac
info "Detected architecture: $ARCH (image suffix: $CHV_ARCH)"

# Detect if we are running from inside a release tarball directory.
# BASH_SOURCE[0] is unset when the script is piped into `bash -s` (the
# documented curl|bash path), so default it instead of tripping `set -u`.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-}")" && pwd)"
if [ -f "${SCRIPT_DIR}/bin/chv-controlplane" ] && [ -z "$INSTALL_CHV_TARBALL_PATH" ]; then
    info "Detected local release directory mode (binaries next to install.sh)"
    INSTALL_CHV_TARBALL_PATH="${SCRIPT_DIR}"
fi

# -----------------------------------------------------------------------------
# Install system dependencies
# -----------------------------------------------------------------------------
install_dependencies() {
    if [ "$INSTALL_CHV_SKIP_DEPS" = "1" ]; then
        info "Skipping dependency installation (INSTALL_CHV_SKIP_DEPS=1)"
        return
    fi

    info "Installing system dependencies..."
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq \
        nginx \
        qemu-kvm bridge-utils iproute2 iptables curl openssl \
        dnsmasq coreutils tar gzip sqlite3 genisoimage apache2-utils
}

# -----------------------------------------------------------------------------
# Create user and directories
# -----------------------------------------------------------------------------
setup_user_and_dirs() {
    info "Setting up user and directories..."

    if ! id -u "$CHV_USER" &>/dev/null; then
        useradd --system --no-create-home --shell /usr/sbin/nologin "$CHV_USER"
    fi

    if ! id -u "chv-stord" &>/dev/null; then
        useradd --system --no-create-home --shell /usr/sbin/nologin chv-stord
    fi

    usermod -aG kvm "$CHV_USER" 2>/dev/null || true
    usermod -aG disk chv-stord 2>/dev/null || true
    usermod -aG chv chv-stord 2>/dev/null || true
    usermod -aG chv-stord "$CHV_USER" 2>/dev/null || true

    mkdir -p "$CHV_CONFIG_DIR"/certs
    mkdir -p "$CHV_DATA_DIR"/{cache,images,storage/localdisk,storage/lvm}
    # 0700 chv:chv matches the deb postinst and the packaged tmpfiles entry
    # (packaging/tmpfiles/chv-node.conf), which force-resets this mode on
    # every boot on deb installs — keep the surfaces from diverging.
    install -d -m 0700 -o "$CHV_USER" -g "$CHV_USER" "$CHV_DATA_DIR/agent"
    install -d -m 0775 -o "$CHV_USER" -g chv-stord "$CHV_DATA_DIR/agent/vms"
    install -d -m 0700 -o "$CHV_USER" -g "$CHV_USER" "$CHV_DATA_DIR/cache" "$CHV_RUN_DIR/core"
    install -d -m 0775 -o "$CHV_USER" -g "$CHV_USER" "$CHV_RUN_DIR/agent"
    mkdir -p "$CHV_LOG_DIR"
    mkdir -p "$CHV_RUN_DIR"/{controlplane,agent,stord,nwd}
    mkdir -p "$CHV_UI_DIR"
    mkdir -p "$CHV_MIGRATIONS_DIR"

    chown -R "$CHV_USER:$CHV_USER" "$CHV_DATA_DIR"/cache "$CHV_DATA_DIR"/images "$CHV_LOG_DIR" "$CHV_RUN_DIR"/controlplane "$CHV_RUN_DIR"/agent "$CHV_RUN_DIR"/nwd "$CHV_RUN_DIR"/stord
    chown "$CHV_USER:$CHV_USER" "$CHV_DATA_DIR/agent"
    chown -R "$CHV_USER:chv-stord" "$CHV_DATA_DIR/agent/vms"
    # Storage dirs must be writable by 'chv': chv-stord runs as the chv
    # service user (0600 API socket with chv-agent as the only client) and
    # cloud-hypervisor (as chv) reads/writes volume files. Group chv-stord
    # is kept as the seam for a future dedicated storage-user model.
    chown -R "$CHV_USER:chv-stord" "$CHV_DATA_DIR"/storage
    chmod 750 "$CHV_DATA_DIR" "$CHV_LOG_DIR"
    chmod 700 "$CHV_DATA_DIR/agent"
    chmod 775 "$CHV_DATA_DIR/agent/vms"
    # Match the .deb postinst contract (#323): storage dirs chv:chv-stord 0770.
    chmod 770 "$CHV_DATA_DIR"/storage
    chmod 770 "$CHV_DATA_DIR"/storage/localdisk
    chmod 770 "$CHV_DATA_DIR"/storage/lvm
}

# -----------------------------------------------------------------------------
# Resolve version and download/locate release
# -----------------------------------------------------------------------------
# Alternatives shown when the from-GitHub path cannot work yet. Kept in one
# place so the two failure paths below (latest lookup, tarball download)
# give the same guidance (#440).
print_github_install_alternatives() {
    warn "Supported install alternatives today:"
    warn "  - nightly .deb/.rpm packages (development only):"
    warn "      https://github.com/${GITHUB_REPO}/releases/tag/nightly"
    warn "  - build from source and install from the local tarball:"
    warn "      make build-release"
    warn "      sudo INSTALL_CHV_TARBALL_PATH=dist/chv-<version>-linux-amd64.tar.gz ./scripts/install.sh"
    warn "See docs/install/from-github-release.md for details."
}

resolve_version() {
    # Offline/air-gapped fallback pin: used ONLY when the GitHub API cannot be
    # reached or queried (never silently — every use is logged as a warning
    # naming the version, so an operator can tell a stale pin from a resolved
    # one). Keep in sync with the last qualified release.
    local fallback_version="0.3.0"

    # A local tarball/directory needs no GitHub API lookup — and the 404
    # fail-fast below must not abort the documented local-tarball install
    # path while no stable release is published yet (#440, #454 review).
    # Preserve main's semantics when no explicit version is given: the same
    # default the failed-'latest' fallback uses, which only names the
    # extracted top-level directory (EXTRACT_DIR) in download_release().
    if [ -n "$INSTALL_CHV_TARBALL_PATH" ] && [ "$INSTALL_CHV_VERSION" = "latest" ]; then
        INSTALL_CHV_VERSION="$fallback_version"
        info "Local tarball mode with no explicit version — using ${INSTALL_CHV_VERSION} for the extracted directory name."
    fi
    if [ "$INSTALL_CHV_VERSION" = "latest" ]; then
        if cmd_exists curl; then
            local latest_api="https://api.github.com/repos/${GITHUB_REPO}/releases/latest"
            local latest=""
            local api_body http_code
            api_body=$(mktemp)
            # -L: follow a repo/org rename redirect (301) instead of treating
            # the redirect body as the API response.
            http_code=$(curl -sSL -o "$api_body" -w '%{http_code}' "$latest_api" 2>/dev/null) || http_code="000"
            if [ "$http_code" = "200" ]; then
                latest=$(grep '"tag_name":' "$api_body" | head -n1 | sed -E 's/.*"v([^"]+)".*/\1/') || true
            fi
            rm -f "$api_body"
            if [ -n "$latest" ]; then
                INSTALL_CHV_VERSION="$latest"
            elif [ "$http_code" = "404" ]; then
                # Fail fast instead of cascading into the version fallback and
                # a cryptic tarball 404 below (#440): 404 from releases/latest
                # means no stable (non-prerelease) release exists yet — the
                # 'nightly' prerelease is ignored by this endpoint.
                warn "Lookup failed: ${latest_api} returned 404."
                warn "The from-GitHub install path needs the first stable GitHub"
                warn "Release to be published; none exists in ${GITHUB_REPO} yet."
                print_github_install_alternatives
                fatal "Cannot resolve 'latest' CHV version — no stable release published yet."
            else
                warn "Could not determine the latest version from the GitHub API"
                warn "(HTTP ${http_code}). Falling back to ${fallback_version} —"
                warn "an offline/air-gapped fallback pin, NOT the resolved latest release."
                warn "Pin explicitly with INSTALL_CHV_VERSION=<version> to silence this."
                INSTALL_CHV_VERSION="$fallback_version"
            fi
        else
            warn "curl is not available, so the latest release cannot be resolved."
            warn "Falling back to ${fallback_version} — an offline/air-gapped fallback"
            warn "pin, NOT the resolved latest release."
            INSTALL_CHV_VERSION="$fallback_version"
        fi
    fi
    info "Installing CHV version: $INSTALL_CHV_VERSION"
}

# Best-effort diagnostics for a failed from-GitHub tarball download: probe
# whether a GitHub Release exists for the requested tag so the operator gets
# an actionable message instead of a bare curl error (#440). Only called on
# the failure path.
print_tarball_download_diagnostics() {
    local tarball_url="$1"
    local release_api="https://api.github.com/repos/${GITHUB_REPO}/releases/tags/v${INSTALL_CHV_VERSION}"
    local http_code
    http_code=$(curl -sSL -o /dev/null -w '%{http_code}' "$release_api" 2>/dev/null) || http_code="000"

    if [ "$http_code" = "404" ]; then
        warn "No GitHub Release is published for tag v${INSTALL_CHV_VERSION} in ${GITHUB_REPO}."
        warn "A git tag alone carries no downloadable assets — a Release must be"
        warn "created from the tag (release.yml does this automatically for tags"
        warn "matching vX.Y.Z or vX.Y.Z-rc.N; any other tag needs a manual"
        warn "Release or a workflow_dispatch run)."
    elif [ "$http_code" = "200" ]; then
        warn "A GitHub Release exists for v${INSTALL_CHV_VERSION}, but it does not"
        warn "carry the expected tarball asset:"
        warn "  ${tarball_url}"
    else
        warn "Could not check release status via the GitHub API (HTTP ${http_code})."
    fi
    print_github_install_alternatives
}

download_release() {
    if [ -n "$INSTALL_CHV_TARBALL_PATH" ]; then
        if [ -d "$INSTALL_CHV_TARBALL_PATH" ]; then
            info "Using local release directory: $INSTALL_CHV_TARBALL_PATH"
            EXTRACT_DIR="$INSTALL_CHV_TARBALL_PATH"
        elif [ -f "$INSTALL_CHV_TARBALL_PATH" ]; then
            info "Using local release tarball: $INSTALL_CHV_TARBALL_PATH"
            local tmpdir
            tmpdir=$(mktemp -d)
            tar -xzf "$INSTALL_CHV_TARBALL_PATH" -C "$tmpdir"
            EXTRACT_DIR="$tmpdir/chv-${INSTALL_CHV_VERSION}-linux-amd64"
            CLEANUP_TMPDIR="$tmpdir"
        else
            fatal "Local tarball path not found: $INSTALL_CHV_TARBALL_PATH"
        fi
        return
    fi

    local tarball_url="https://github.com/${GITHUB_REPO}/releases/download/v${INSTALL_CHV_VERSION}/chv-${INSTALL_CHV_VERSION}-linux-amd64.tar.gz"
    local tmpdir
    tmpdir=$(mktemp -d)
    local tarball="$tmpdir/chv-release.tar.gz"
    CLEANUP_TMPDIR="$tmpdir"

    info "Downloading release tarball..."
    if ! curl -fsSL "$tarball_url" -o "$tarball"; then
        print_tarball_download_diagnostics "$tarball_url"
        fatal "Failed to download release from $tarball_url"
    fi

    info "Extracting release tarball..."
    tar -xzf "$tarball" -C "$tmpdir"
    EXTRACT_DIR="$tmpdir/chv-${INSTALL_CHV_VERSION}-linux-amd64"
}

# -----------------------------------------------------------------------------
# Install binaries and assets
# -----------------------------------------------------------------------------
install_binaries_and_assets() {
    info "Installing CHV binaries..."

    install -m 755 "${EXTRACT_DIR}/bin/chv-controlplane" /usr/bin/
    install -m 755 "${EXTRACT_DIR}/bin/chv-agent"        /usr/bin/
    install -m 755 "${EXTRACT_DIR}/bin/chv-stord"        /usr/bin/
    install -m 755 "${EXTRACT_DIR}/bin/chv-nwd"          /usr/bin/

    info "Installing Web UI assets..."
    rm -rf "$CHV_UI_DIR"/*
    cp -r "${EXTRACT_DIR}/ui/"* "$CHV_UI_DIR/"
    # Owned by the control plane user since #447: the binary serves the
    # tree ([webui] in controlplane.toml), not nginx. Files/dirs stay
    # world-readable (cp under the default umask), so any edge can
    # still read them too.
    chown -R "$CHV_USER:$CHV_USER" "$CHV_UI_DIR"

    info "Installing database migrations..."
    cp -r "${EXTRACT_DIR}/migrations/"* "$CHV_MIGRATIONS_DIR/"
    chown -R "$CHV_USER:$CHV_USER" "$CHV_MIGRATIONS_DIR"

    # Monitoring store migrations (ADR-027, #602): applied to the
    # separate disposable monitoring.db; failure to open that file only
    # degrades monitoring.
    mkdir -p "$CHV_MONITORING_MIGRATIONS_DIR" "$CHV_DATA_DIR/monitoring"
    cp -r "${EXTRACT_DIR}/monitoring-migrations/"* "$CHV_MONITORING_MIGRATIONS_DIR/"
    chown -R "$CHV_USER:$CHV_USER" "$CHV_MONITORING_MIGRATIONS_DIR"
    chown -R "$CHV_USER:$CHV_USER" "$CHV_DATA_DIR/monitoring"
}

# -----------------------------------------------------------------------------
# Install Cloud Hypervisor
# -----------------------------------------------------------------------------
install_cloud_hypervisor() {
    if [ "$INSTALL_CHV_SKIP_CLOUD_HV" = "1" ]; then
        info "Skipping Cloud Hypervisor installation (INSTALL_CHV_SKIP_CLOUD_HV=1)"
        return
    fi

    local chv_version="53.0"

    # Version gate: a host cloud-hypervisor is accepted only when its
    # major.minor matches the qualified pin. Any other version is replaced
    # with the pinned, digest-verified static binary below — the previous
    # "any version counts as installed" early return silently kept hosts on
    # the v43 branch the qualification campaign moved away from.
    if cmd_exists cloud-hypervisor; then
        local found_raw found_mm
        found_raw="$(cloud-hypervisor --version 2>/dev/null || true)"
        # Parse major.minor from e.g. "cloud-hypervisor v43.0.0". Comparing
        # major.minor (not the full string) lets v53.0.0 match the "53.0" pin.
        found_mm="$(printf '%s' "$found_raw" | grep -oE '[0-9]+\.[0-9]+' | head -n1 || true)"

        if [ "$found_mm" = "$chv_version" ]; then
            info "Cloud Hypervisor already installed at the qualified pin (${found_raw}); keeping it."
            if [ ! -f /usr/bin/cloud-hypervisor ] && [ -f /usr/local/bin/cloud-hypervisor ]; then
                ln -sf /usr/local/bin/cloud-hypervisor /usr/bin/cloud-hypervisor
            fi
            return
        fi

        if [ "${INSTALL_CHV_ALLOW_VMM_MISMATCH:-0}" = "1" ]; then
            warn "INSTALL_CHV_ALLOW_VMM_MISMATCH=1: keeping the host Cloud Hypervisor."
            warn "  found:    ${found_raw:-unparseable}"
            warn "  required: v${chv_version} (qualified pin)"
            warn "This install is OFF-PIN and UNQUALIFIED: VMs will run on a VMM the"
            warn "qualification campaign did not cover. Re-run without this override"
            warn "(and without INSTALL_CHV_SKIP_CLOUD_HV) to install the pinned binary."
            if [ ! -f /usr/bin/cloud-hypervisor ] && [ -f /usr/local/bin/cloud-hypervisor ]; then
                ln -sf /usr/local/bin/cloud-hypervisor /usr/bin/cloud-hypervisor
            fi
            return
        fi

        warn "Cloud Hypervisor version mismatch: found ${found_raw:-unparseable},"
        warn "but the qualified pin is v${chv_version}. Replacing the host binary"
        warn "with the pinned, digest-verified static build."
        warn "To keep the host binary instead (off-pin, unqualified), re-run with"
        warn "INSTALL_CHV_ALLOW_VMM_MISMATCH=1."
    fi

    # Qualified-pin digests — D6 option (b), #448 campaign
    # (docs/evidence/vmm-requalification/v53.0/). The download aborts on
    # mismatch; the pin and its digests move together, always.
    local chv_sha256="448af3d4e59b22c2987f7df94c213ad40fb53a10d437e42b5ee6c4fce7c29ecc"
    local chv_remote_sha256="13f32ba952e6791fd901f2279be2055fbacc64005f96c42a8e90d58860df84a7"

    info "Downloading Cloud Hypervisor v${chv_version}..."
    local chv_tmp
    chv_tmp="$(mktemp)"
    curl -fsSL "https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v${chv_version}/cloud-hypervisor-static" \
        -o "${chv_tmp}"
    if ! echo "${chv_sha256}  ${chv_tmp}" | sha256sum -c - >/dev/null 2>&1; then
        rm -f "${chv_tmp}"
        fatal "Cloud Hypervisor v${chv_version} digest mismatch (expected ${chv_sha256}); aborting install"
    fi
    install -m 0755 "${chv_tmp}" /usr/local/bin/cloud-hypervisor
    rm -f "${chv_tmp}"
    ln -sf /usr/local/bin/cloud-hypervisor /usr/bin/cloud-hypervisor
    info "Cloud Hypervisor installed: $(cloud-hypervisor --version)"

    if ! cmd_exists ch-remote; then
        info "Downloading ch-remote v${chv_version}..."
        local chv_remote_tmp
        chv_remote_tmp="$(mktemp)"
        curl -fsSL "https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v${chv_version}/ch-remote-static" \
            -o "${chv_remote_tmp}"
        if ! echo "${chv_remote_sha256}  ${chv_remote_tmp}" | sha256sum -c - >/dev/null 2>&1; then
            rm -f "${chv_remote_tmp}"
            fatal "ch-remote v${chv_version} digest mismatch (expected ${chv_remote_sha256}); aborting install"
        fi
        install -m 0755 "${chv_remote_tmp}" /usr/local/bin/ch-remote
        rm -f "${chv_remote_tmp}"
        info "ch-remote installed: $(ch-remote --version 2>/dev/null || echo v${chv_version})"
    else
        info "ch-remote already installed: $(ch-remote --version 2>/dev/null || true)"
    fi
}

# -----------------------------------------------------------------------------
# Base OS Image Download
# -----------------------------------------------------------------------------
BASE_IMAGE_URL="https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-${CHV_ARCH}.img"
BASE_IMAGE_PATH="${CHV_DATA_DIR}/images/ubuntu-noble-${CHV_ARCH}.img"

# Cloud Hypervisor Firmware
FIRMWARE_ARCH="${ARCH}"
FIRMWARE_URL="https://github.com/kubedoio/rust-hypervisor-firmware/releases/download/v0.5.0/hypervisor-fw-${FIRMWARE_ARCH}"
FIRMWARE_PATH="${CHV_DATA_DIR}/hypervisor-fw"

download_base_image() {
    info "Checking for base OS image..."

    mkdir -p "${CHV_DATA_DIR}/images"
    chown "${CHV_USER}:${CHV_USER}" "${CHV_DATA_DIR}/images"

    if [ -f "$BASE_IMAGE_PATH" ]; then
        # Ensure existing image is raw (not qcow2)
        if cmd_exists qemu-img; then
            local img_fmt
            img_fmt=$(qemu-img info --output=json "$BASE_IMAGE_PATH" 2>/dev/null | grep -o '"format":"[^"]*"' | cut -d'"' -f4 || true)
            if [ "$img_fmt" = "qcow2" ]; then
                info "Converting existing qcow2 image to raw format..."
                local raw_path="${BASE_IMAGE_PATH}.raw"
                qemu-img convert -f qcow2 -O raw "$BASE_IMAGE_PATH" "$raw_path"
                mv "$raw_path" "$BASE_IMAGE_PATH"
                chown "${CHV_USER}:${CHV_USER}" "$BASE_IMAGE_PATH"
                info "Base image converted to raw -> ${BASE_IMAGE_PATH}"
            else
                info "Base image already exists at ${BASE_IMAGE_PATH}, skipping download."
            fi
        else
            info "Base image already exists at ${BASE_IMAGE_PATH}, skipping download."
        fi
        return
    fi

    # Check common locations for a pre-downloaded image
    for candidate in \
        "/root/noble-server-cloudimg-${CHV_ARCH}.img" \
        "/root/ubuntu-noble-${CHV_ARCH}.img" \
        "/tmp/noble-server-cloudimg-${CHV_ARCH}.img"; do
        if [ -f "$candidate" ]; then
            info "Found pre-downloaded image at ${candidate}, copying..."
            cp "$candidate" "$BASE_IMAGE_PATH"
            chown "${CHV_USER}:${CHV_USER}" "$BASE_IMAGE_PATH"
            chmod 644 "$BASE_IMAGE_PATH"
            # Convert qcow2 to raw if needed
            if cmd_exists qemu-img; then
                local img_fmt
                img_fmt=$(qemu-img info --output=json "$BASE_IMAGE_PATH" 2>/dev/null | grep -o '"format":"[^"]*"' | cut -d'"' -f4 || true)
                if [ "$img_fmt" = "qcow2" ]; then
                    info "Converting qcow2 image to raw format..."
                    local raw_path="${BASE_IMAGE_PATH}.raw"
                    qemu-img convert -f qcow2 -O raw "$BASE_IMAGE_PATH" "$raw_path"
                    mv "$raw_path" "$BASE_IMAGE_PATH"
                    chown "${CHV_USER}:${CHV_USER}" "$BASE_IMAGE_PATH"
                fi
            fi
            info "Base image ready -> ${BASE_IMAGE_PATH}"
            return
        fi
    done

    info "Downloading Ubuntu Noble base image for ${CHV_ARCH}..."
    info "URL: ${BASE_IMAGE_URL}"

    local tmp_image
    tmp_image="${BASE_IMAGE_PATH}.tmp"

    if curl -fsSL --retry 3 --retry-delay 5 "$BASE_IMAGE_URL" -o "$tmp_image"; then
        # Ubuntu cloud images are qcow2; convert to raw for CHV firmware boot
        if cmd_exists qemu-img; then
            local img_fmt
            img_fmt=$(qemu-img info --output=json "$tmp_image" 2>/dev/null | grep -o '"format":"[^"]*"' | cut -d'"' -f4 || true)
            if [ "$img_fmt" = "qcow2" ]; then
                info "Converting qcow2 image to raw format..."
                qemu-img convert -f qcow2 -O raw "$tmp_image" "$BASE_IMAGE_PATH"
                rm -f "$tmp_image"
            else
                mv "$tmp_image" "$BASE_IMAGE_PATH"
            fi
        else
            warn "qemu-img not found; keeping image as-is (may be qcow2)."
            mv "$tmp_image" "$BASE_IMAGE_PATH"
        fi

        chown "${CHV_USER}:${CHV_USER}" "$BASE_IMAGE_PATH"
        chmod 644 "$BASE_IMAGE_PATH"
        local img_size
        img_size=$(du -h "$BASE_IMAGE_PATH" | cut -f1)
        info "Base image ready (${img_size}) -> ${BASE_IMAGE_PATH}"
    else
        warn "Failed to download base image from ${BASE_IMAGE_URL}"
        warn "You can manually download and import it later via the Web UI."
    fi
}

## Explicit local firmware override. A firmware build dropped at
## /root/CLOUDHV.fd wins over the pinned download in download_firmware()
## (which skips when ${FIRMWARE_PATH} already exists).
copy_firmware() {
    if [ -f /root/CLOUDHV.fd ]; then
        info "Using local firmware override /root/CLOUDHV.fd -> ${FIRMWARE_PATH}"
        cp /root/CLOUDHV.fd "${FIRMWARE_PATH}"
        chown "${CHV_USER}:${CHV_USER}" "${FIRMWARE_PATH}"
        chmod 644 "${FIRMWARE_PATH}"
    else
        info "No local firmware override at /root/CLOUDHV.fd; using the pinned download."
    fi
}


# -----------------------------------------------------------------------------
# Import Base Image into Control Plane
# -----------------------------------------------------------------------------
download_firmware() {
    info "Checking for cloud-hypervisor firmware..."

    if [ -f "$FIRMWARE_PATH" ]; then
        info "Firmware already exists at ${FIRMWARE_PATH}, skipping download."
        return
    fi

    info "Downloading cloud-hypervisor firmware for ${FIRMWARE_ARCH}..."
    info "URL: ${FIRMWARE_URL}"

    if curl -fsSL --retry 3 --retry-delay 5 "$FIRMWARE_URL" -o "$FIRMWARE_PATH"; then
        chown "${CHV_USER}:${CHV_USER}" "$FIRMWARE_PATH"
        chmod 755 "$FIRMWARE_PATH"
        local fw_size
        fw_size=$(du -h "$FIRMWARE_PATH" | cut -f1)
        info "Firmware downloaded (${fw_size}) -> ${FIRMWARE_PATH}"
    else
        warn "Failed to download firmware from ${FIRMWARE_URL}"
        warn "No firmware will be present at ${FIRMWARE_PATH}: VMs that request"
        warn "firmware will fail to start with 'firmware not found: ${FIRMWARE_PATH}'."
        warn "Retry manually with: curl -fSL --retry 3 '${FIRMWARE_URL}' -o '${FIRMWARE_PATH}'"
    fi
}

import_base_image() {
    if [ ! -f "$BASE_IMAGE_PATH" ]; then
        info "No base image to import."
        return
    fi

    info "Importing base image via API..."

    local api_base="http://127.0.0.1:8080"

    # Wait for CP to be ready
    local attempt=1
    while [ $attempt -le 30 ]; do
        if curl -sf "${api_base}/health" &>/dev/null; then
            break
        fi
        sleep 1
        ((attempt++))
    done

    if [ $attempt -gt 30 ]; then
        warn "Control plane not available for image import."
        return
    fi

    # Login to get auth token. Read the bootstrap admin password from the
    # 0600 root-only file written by seed_admin_user(); never embed credentials.
    local admin_pw
    admin_pw=$(cat "${CHV_CONFIG_DIR}/initial_admin_password" 2>/dev/null) || {
        warn "Cannot read ${CHV_CONFIG_DIR}/initial_admin_password — skipping image import."
        return
    }
    if [ -z "$admin_pw" ]; then
        warn "Empty admin password file — skipping image import."
        return
    fi
    local login_response
    login_response=$(curl -sf -X POST "${api_base}/v1/auth/login" \
        -H "Content-Type: application/json" \
        -d "{\"username\":\"admin\",\"password\":\"${admin_pw}\"}" 2>/dev/null || true)
    if [ -z "$login_response" ]; then
        warn "Failed to login for image import."
        return
    fi

    local token
    token=$(echo "$login_response" | grep -o '"token":"[^"]*"' | head -1 | sed 's/"token":"//;s/"//' || true)
    if [ -z "$token" ]; then
        warn "Failed to extract auth token for image import."
        return
    fi

    local auth_header="Authorization: Bearer ${token}"

    # Import the image via BFF API
    local import_response
    import_response=$(curl -sf -X POST "${api_base}/v1/images/import" \
        -H "Content-Type: application/json" \
        -H "$auth_header" \
        -d "{
            \"name\": \"ubuntu-noble\",
            \"source_url\": \"${BASE_IMAGE_PATH}\",
            \"format\": \"raw\",
            \"os\": \"ubuntu\"
        }" 2>/dev/null || true)

    if [ -n "$import_response" ]; then
        local image_id
        image_id=$(echo "$import_response" | grep -o '"image_id":"[^"]*"' | head -1 | sed 's/"image_id":"//;s/"//' || true)
        info "Base image imported via API (image_id: ${image_id})."
    else
        info "Base image import skipped (may already exist)."
    fi
}

# -----------------------------------------------------------------------------
# Seed Dev Resources (default network + test VM)
# -----------------------------------------------------------------------------
seed_dev_resources() {
    local api_base="http://127.0.0.1:8080"

    if [ "$INSTALL_CHV_NO_BRIDGE" = "1" ]; then
        warn "Seeding the default network with INSTALL_CHV_NO_BRIDGE=1: the legacy"
        warn "host bridge/NAT bootstrap was skipped, so guests on the seeded network"
        warn "will have no external connectivity unless forwarding is provided out of band."
    fi

    info "Seeding dev resources via API..."

    # Wait for API to be healthy
    local attempt=1
    while [ $attempt -le 30 ]; do
        if curl -sf "${api_base}/health" &>/dev/null; then
            break
        fi
        sleep 1
        ((attempt++))
    done
    if [ $attempt -gt 30 ]; then
        warn "Control plane API not available, skipping dev resource seeding."
        return
    fi

    # Login as admin to get JWT token. Read the bootstrap password from the
    # 0600 root-only file written by seed_admin_user(); never embed credentials.
    local admin_pw
    admin_pw=$(cat "${CHV_CONFIG_DIR}/initial_admin_password" 2>/dev/null) || {
        warn "Cannot read ${CHV_CONFIG_DIR}/initial_admin_password — skipping dev resource seeding."
        return
    }
    if [ -z "$admin_pw" ]; then
        warn "Empty admin password file — skipping dev resource seeding."
        return
    fi
    local login_response
    login_response=$(curl -sf -X POST "${api_base}/v1/auth/login" \
        -H "Content-Type: application/json" \
        -d "{\"username\":\"admin\",\"password\":\"${admin_pw}\"}" 2>/dev/null || true)
    if [ -z "$login_response" ]; then
        warn "Failed to login as admin, skipping dev resource seeding."
        return
    fi

    local token
    token=$(echo "$login_response" | grep -o '"token":"[^"]*"' | head -1 | sed 's/"token":"//;s/"//' || true)
    if [ -z "$token" ]; then
        warn "Failed to extract auth token, skipping dev resource seeding."
        return
    fi

    local auth_header="Authorization: Bearer ${token}"

    # Derive network CIDR from bridge CIDR
    local bridge_ip="${INSTALL_CHV_BRIDGE_CIDR%/*}"
    local bridge_prefix="${INSTALL_CHV_BRIDGE_CIDR#*/}"
    local network_cidr="${bridge_ip%.*}.0/${bridge_prefix}"
    local gateway_ip="${bridge_ip}"

    # --- Create default network via API ---
    local net_response
    net_response=$(curl -sf -X POST "${api_base}/v1/networks/create" \
        -H "Content-Type: application/json" \
        -H "$auth_header" \
        -d "{
            \"name\": \"default\",
            \"cidr\": \"${network_cidr}\",
            \"gateway\": \"${gateway_ip}\",
            \"dhcp_enabled\": true,
            \"ipam_mode\": \"internal\",
            \"is_default\": true
        }" 2>/dev/null || true)

    if [ -n "$net_response" ]; then
        local net_id
        net_id=$(echo "$net_response" | grep -o '"network_id":"[^"]*"' | head -1 | sed 's/"network_id":"//;s/"//' || true)
        info "Default network created via API (id=${net_id}, ${network_cidr}, gateway ${gateway_ip})."
    else
        info "Default network creation skipped (may already exist)."
        # Try to find existing default network ID for VM creation
        net_id="default"
    fi

    # --- Create test VM via API ---
    if [ ! -f "$BASE_IMAGE_PATH" ]; then
        warn "Base image not found at ${BASE_IMAGE_PATH}, skipping test VM creation."
        return
    fi

    # Wait for node to reach TenantReady before attempting VM placement
    info "Waiting for node to reach TenantReady (up to 60s)..."
    attempt=1
    while [ $attempt -le 60 ]; do
        local nodes_json
        nodes_json=$(curl -sf -X POST "${api_base}/v1/nodes" \
            -H "Content-Type: application/json" \
            -H "$auth_header" \
            -d '{}' 2>/dev/null || echo "")
        if echo "$nodes_json" | grep -q '"state":"TenantReady"'; then
            info "Node is TenantReady, proceeding with VM creation."
            break
        fi
        sleep 1
        ((attempt++))
    done
    if [ $attempt -gt 60 ]; then
        warn "No node reached TenantReady within 60s, skipping test VM creation."
        warn "Check node state: journalctl -u chv-agent -n 50"
        return
    fi

    local vm_response
    vm_response=$(curl -sf -X POST "${api_base}/v1/vms/create" \
        -H "Content-Type: application/json" \
        -H "$auth_header" \
        -d "{
            \"display_name\": \"dev-vm-1\",
            \"cpu_count\": 1,
            \"memory_mb\": 512,
            \"image_ref\": \"${BASE_IMAGE_PATH}\",
            \"network_id\": \"${net_id:-default}\",
            \"volume_size_gb\": 10,
            \"requested_by\": \"dev-install\"
        }" 2>/dev/null || true)

    if [ -n "$vm_response" ]; then
        local vm_id
        vm_id=$(echo "$vm_response" | grep -o '"vm_id":"[^"]*"' | head -1 | sed 's/"vm_id":"//;s/"//' || true)
        info "Test VM created via API (id=${vm_id}, 1 CPU, 512 MB, 10 GB disk)."
    else
        warn "Failed to create test VM via API."
    fi
}

# -----------------------------------------------------------------------------
# TLS Certificate Generation
# -----------------------------------------------------------------------------
generate_certs() {
    info "Generating TLS certificates..."

    chown root:"$CHV_USER" "$CHV_CONFIG_DIR"/certs
    chmod 750 "$CHV_CONFIG_DIR"/certs

    if [ ! -f "$CHV_CONFIG_DIR/certs/ca.key" ]; then
        openssl genrsa -out "$CHV_CONFIG_DIR/certs/ca.key" 4096 2>/dev/null
        openssl req -x509 -new -nodes -key "$CHV_CONFIG_DIR/certs/ca.key" \
            -sha256 -days 3650 -out "$CHV_CONFIG_DIR/certs/ca.crt" \
            -subj "/O=CHV/CN=chv-ca" \
            -addext "basicConstraints=critical,CA:TRUE" \
            -addext "keyUsage=critical,keyCertSign,cRLSign" 2>/dev/null
        chmod 640 "$CHV_CONFIG_DIR/certs/ca.key"
        chmod 644 "$CHV_CONFIG_DIR/certs/ca.crt"
        chown root:"$CHV_USER" "$CHV_CONFIG_DIR/certs/ca.key" "$CHV_CONFIG_DIR/certs/ca.crt"
    fi

    if [ ! -f "$CHV_CONFIG_DIR/certs/server.key" ]; then
        openssl genrsa -out "$CHV_CONFIG_DIR/certs/server.key" 2048 2>/dev/null
        openssl req -new -key "$CHV_CONFIG_DIR/certs/server.key" \
            -out "$CHV_CONFIG_DIR/certs/server.csr" \
            -subj "/O=CHV/CN=chv-controlplane" 2>/dev/null
        openssl x509 -req -in "$CHV_CONFIG_DIR/certs/server.csr" \
            -CA "$CHV_CONFIG_DIR/certs/ca.crt" -CAkey "$CHV_CONFIG_DIR/certs/ca.key" \
            -CAcreateserial -out "$CHV_CONFIG_DIR/certs/server.crt" \
            -days 825 -sha256 \
            -extfile <(printf "subjectAltName=DNS:localhost,IP:127.0.0.1\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth") 2>/dev/null
        rm -f "$CHV_CONFIG_DIR/certs/server.csr"
        chmod 640 "$CHV_CONFIG_DIR/certs/server.key"
        chmod 644 "$CHV_CONFIG_DIR/certs/server.crt"
        chown root:"$CHV_USER" "$CHV_CONFIG_DIR/certs/server.key" "$CHV_CONFIG_DIR/certs/server.crt"
    fi

    # Agent client certificate (for mTLS to control plane)
    if [ ! -f "$CHV_CONFIG_DIR/certs/agent-client.key" ]; then
        openssl genrsa -out "$CHV_CONFIG_DIR/certs/agent-client.key" 2048 2>/dev/null
        openssl req -new -key "$CHV_CONFIG_DIR/certs/agent-client.key" \
            -out "$CHV_CONFIG_DIR/certs/agent-client.csr" \
            -subj "/O=CHV/CN=chv-agent" 2>/dev/null
        openssl x509 -req -in "$CHV_CONFIG_DIR/certs/agent-client.csr" \
            -CA "$CHV_CONFIG_DIR/certs/ca.crt" -CAkey "$CHV_CONFIG_DIR/certs/ca.key" \
            -CAcreateserial -out "$CHV_CONFIG_DIR/certs/agent-client.crt" \
            -days 825 -sha256 \
            -extfile <(printf "keyUsage=digitalSignature\nextendedKeyUsage=clientAuth") 2>/dev/null
        rm -f "$CHV_CONFIG_DIR/certs/agent-client.csr"
        chmod 640 "$CHV_CONFIG_DIR/certs/agent-client.key"
        chmod 644 "$CHV_CONFIG_DIR/certs/agent-client.crt"
        chown root:"$CHV_USER" "$CHV_CONFIG_DIR/certs/agent-client.key" "$CHV_CONFIG_DIR/certs/agent-client.crt"
    fi
}

# -----------------------------------------------------------------------------
# Bridge and NAT network setup
# -----------------------------------------------------------------------------
setup_network() {
    # The legacy dev bootstrap: pre-create chvbr0 + NAT so the seeded default
    # network has external connectivity. Qualified networks are created via
    # the API and managed by chv-nwd (br-<net_id>); hosts that do not want
    # this convenience bridge can skip it with INSTALL_CHV_NO_BRIDGE=1.
    if [ "$INSTALL_CHV_NO_BRIDGE" = "1" ]; then
        warn "Skipping legacy bridge/NAT bootstrap (INSTALL_CHV_NO_BRIDGE=1)."
        return 0
    fi
    info "Setting up bridge network ${INSTALL_CHV_BRIDGE_NAME} (${INSTALL_CHV_BRIDGE_CIDR}) on ${INSTALL_CHV_BRIDGE_IFACE}..."

    # Create bridge if absent
    if ! ip link show "${INSTALL_CHV_BRIDGE_NAME}" &>/dev/null; then
        ip link add name "${INSTALL_CHV_BRIDGE_NAME}" type bridge
    fi
    ip link set "${INSTALL_CHV_BRIDGE_NAME}" up

    # Assign gateway IP to the bridge (idempotent)
    local bridge_ip="${INSTALL_CHV_BRIDGE_CIDR%/*}"
    local bridge_prefix="${INSTALL_CHV_BRIDGE_CIDR#*/}"
    if ! ip addr show "${INSTALL_CHV_BRIDGE_NAME}" | grep -q "${bridge_ip}"; then
        ip addr add "${INSTALL_CHV_BRIDGE_CIDR}" dev "${INSTALL_CHV_BRIDGE_NAME}"
    fi

    # Attach host interface to the bridge if it exists and is not already a member
    if ip link show "${INSTALL_CHV_BRIDGE_IFACE}" &>/dev/null; then
        local current_master
        current_master=$(ip link show "${INSTALL_CHV_BRIDGE_IFACE}" | grep -o "master [^ ]*" | awk '{print $2}' || true)
        if [ "$current_master" != "${INSTALL_CHV_BRIDGE_NAME}" ]; then
            ip link set "${INSTALL_CHV_BRIDGE_IFACE}" master "${INSTALL_CHV_BRIDGE_NAME}"
            ip link set "${INSTALL_CHV_BRIDGE_IFACE}" up
        fi
    else
        warn "Interface ${INSTALL_CHV_BRIDGE_IFACE} not found — bridge created without upstream port."
        warn "Set INSTALL_CHV_BRIDGE_IFACE to an existing interface if you need external connectivity."
    fi

    # Enable IP forwarding
    sysctl -qw net.ipv4.ip_forward=1
    echo "net.ipv4.ip_forward=1" > /etc/sysctl.d/99-chv-forward.conf

    # NAT outbound traffic from bridge subnet
    local bridge_net="${bridge_ip%.*}.0/${bridge_prefix}"
    if ! iptables -t nat -C POSTROUTING -s "${bridge_net}" ! -d "${bridge_net}" -j MASQUERADE 2>/dev/null; then
        iptables -t nat -A POSTROUTING -s "${bridge_net}" ! -d "${bridge_net}" -j MASQUERADE
    fi

    # Persist iptables rules (iptables-persistent / netfilter-persistent)
    if cmd_exists netfilter-persistent; then
        netfilter-persistent save 2>/dev/null || true
    elif cmd_exists iptables-save; then
        mkdir -p /etc/iptables
        iptables-save > /etc/iptables/rules.v4
    fi

    # Persist bridge via systemd-networkd or a simple netplan/ifupdown snippet
    # We use a drop-in under /etc/network/interfaces.d (ifupdown) if available,
    # otherwise write a systemd.network unit (networkd).
    if [ -d /etc/network/interfaces.d ]; then
        cat > "/etc/network/interfaces.d/chvbr0" <<EOF
auto ${INSTALL_CHV_BRIDGE_NAME}
iface ${INSTALL_CHV_BRIDGE_NAME} inet static
    address ${INSTALL_CHV_BRIDGE_CIDR}
    bridge_ports ${INSTALL_CHV_BRIDGE_IFACE}
    bridge_stp off
    bridge_fd 0
EOF
    else
        mkdir -p /etc/systemd/network
        cat > "/etc/systemd/network/10-${INSTALL_CHV_BRIDGE_NAME}.netdev" <<EOF
[NetDev]
Name=${INSTALL_CHV_BRIDGE_NAME}
Kind=bridge
EOF
        cat > "/etc/systemd/network/11-${INSTALL_CHV_BRIDGE_NAME}.network" <<EOF
[Match]
Name=${INSTALL_CHV_BRIDGE_NAME}

[Network]
Address=${INSTALL_CHV_BRIDGE_CIDR}
IPForward=yes
EOF
        cat > "/etc/systemd/network/12-${INSTALL_CHV_BRIDGE_IFACE}.network" <<EOF
[Match]
Name=${INSTALL_CHV_BRIDGE_IFACE}

[Network]
Bridge=${INSTALL_CHV_BRIDGE_NAME}
EOF
    fi

    info "Bridge ${INSTALL_CHV_BRIDGE_NAME} ready (${INSTALL_CHV_BRIDGE_CIDR}), NAT via ${INSTALL_CHV_BRIDGE_IFACE}."
}

# -----------------------------------------------------------------------------
# Configuration Files
# -----------------------------------------------------------------------------
install_configs() {
    info "Installing configuration files..."

    CHV_NODE_ID=$(uuidgen 2>/dev/null || cat /proc/sys/kernel/random/uuid 2>/dev/null || openssl rand -hex 16)
    info "Node ID: ${CHV_NODE_ID}"

    JWT_SECRET=$(openssl rand -base64 32 | tr -d '=+/')

    cat > "$CHV_CONFIG_DIR/controlplane.toml" <<EOF
grpc_bind = "127.0.0.1:8443"
http_bind = "127.0.0.1:8080"
log_level = "info"
runtime_dir = "/run/chv/controlplane"
jwt_secret = "${JWT_SECRET}"

[database]
url = "sqlite://${CHV_DB_PATH}"
migrations_dir = "${CHV_MIGRATIONS_DIR}"
max_connections = 4
min_connections = 1
acquire_timeout_secs = 5

[monitoring]
enabled = true
database_url = "sqlite://${CHV_DATA_DIR}/monitoring/monitoring.db"
migrations_dir = "${CHV_MONITORING_MIGRATIONS_DIR}"

[tls]
ca_cert_path = "${CHV_CONFIG_DIR}/certs/ca.crt"
ca_key_path = "${CHV_CONFIG_DIR}/certs/ca.key"
server_cert_path = "${CHV_CONFIG_DIR}/certs/server.crt"
server_key_path = "${CHV_CONFIG_DIR}/certs/server.key"
client_ca_path = "${CHV_CONFIG_DIR}/certs/ca.crt"

# Web UI static serving (#447, D3 target): the control plane serves
# the UI itself; nginx (install_nginx below) is a proxy-only edge.
[webui]
enabled = true
dir = "${CHV_UI_DIR}"
EOF
    chmod 640 "$CHV_CONFIG_DIR/controlplane.toml"
    chown root:"$CHV_USER" "$CHV_CONFIG_DIR/controlplane.toml"

    cat > "$CHV_CONFIG_DIR/agent.toml" <<EOF
socket_path = "/run/chv/agent/api.sock"
runtime_dir = "${CHV_DATA_DIR}/agent"
log_level = "info"
control_plane_addr = "https://127.0.0.1:8443"
stord_socket = "/run/chv/stord/api.sock"
nwd_socket = "/run/chv/nwd/api.sock"
chv_binary_path = "/usr/bin/cloud-hypervisor"
stord_binary_path = "/usr/bin/chv-stord"
nwd_binary_path = "/usr/bin/chv-nwd"
cache_path = "${CHV_DATA_DIR}/cache/agent-cache.json"
authority_mode = "core-managed"
core_store_path = "${CHV_DATA_DIR}/agent/core.db"
core_api_socket_path = "${CHV_RUN_DIR}/core/core-v1.sock"
core_archive_path = "${CHV_DATA_DIR}/agent/node-cache-v1.archive"
node_id = "${CHV_NODE_ID}"
metrics_bind = "127.0.0.1:9901"
storage_base_dir = "${CHV_DATA_DIR}/storage"
bootstrap_token_path = "${CHV_CONFIG_DIR}/bootstrap.token"
tls_cert_path = "/run/chv/agent/agent.crt"
tls_key_path = "/run/chv/agent/agent.key"
ca_cert_path = "${CHV_CONFIG_DIR}/certs/ca.crt"
console_bind = "127.0.0.1:8444"
jwt_secret = "${JWT_SECRET}"

# #376: preserve stord's path confinement across agent-supervisor
# respawns — mirrors stord.toml's path_allowlist below so a respawned
# stord keeps the operator's posture instead of running allow-all.
stord_path_allowlist = ["${CHV_DATA_DIR}/storage/localdisk", "${CHV_DATA_DIR}/storage/lvm", "${CHV_DATA_DIR}/agent"]

# #385: respawn config fidelity — when the agent's supervisor has to
# respawn a dead stord, exec it with the operator's stord.toml below
# (validated: same socket) instead of generating a lossy config, so
# runtime_dir, backend_type, device_allowlist and [migration] survive
# the respawn. The agent user reads it via the chv-stord group.
stord_config_path = "${CHV_CONFIG_DIR}/stord.toml"

# #504: nwd respawn config fidelity — same pass-through for nwd: a
# respawned chv-nwd runs with the operator's nwd.toml below (validated:
# same socket) instead of a lossy generated config, so the [overlay],
# [ebpf] and [fabric] blocks survive the respawn. The agent user can
# read that file (it is installed root:chv 0640 below).
nwd_config_path = "${CHV_CONFIG_DIR}/nwd.toml"
EOF
    chmod 640 "$CHV_CONFIG_DIR/agent.toml"
    chown root:"$CHV_USER" "$CHV_CONFIG_DIR/agent.toml"

    cat > "$CHV_CONFIG_DIR/stord.toml" <<EOF
socket_path = "/run/chv/stord/api.sock"
runtime_dir = "${CHV_DATA_DIR}/storage/localdisk"
log_level = "info"
path_allowlist = ["${CHV_DATA_DIR}/storage/localdisk", "${CHV_DATA_DIR}/storage/lvm", "${CHV_DATA_DIR}/agent"]
device_allowlist = ["/dev/dm-*", "/dev/mapper/*"]
EOF
    chmod 640 "$CHV_CONFIG_DIR/stord.toml"
    chown root:chv-stord "$CHV_CONFIG_DIR/stord.toml"

    cat > "$CHV_CONFIG_DIR/nwd.toml" <<EOF
socket_path = "/run/chv/nwd/api.sock"
runtime_dir = "/run/chv/nwd"
log_level = "info"
# Bridge topology (bridge name, CIDR, upstream interface) is configured at
# runtime via gRPC topology specs from the control plane, not in this config
# file. The dev-resource seeder creates the default network via the API using
# INSTALL_CHV_BRIDGE_CIDR; nothing here reads the old bridge_* keys.
EOF
    chmod 640 "$CHV_CONFIG_DIR/nwd.toml"
    chown root:"$CHV_USER" "$CHV_CONFIG_DIR/nwd.toml"
}

# -----------------------------------------------------------------------------
# Credential Encryption Key (#335 / report finding H-7)
# -----------------------------------------------------------------------------
# The control plane derives an AES-256-GCM key from CHV_ENCRYPTION_KEY to
# encrypt S3 credentials at rest (credential_crypto.rs); without it they are
# stored in plaintext. chv-controlplane.service loads this file via
# EnvironmentFile. Mint once and NEVER regenerate on reinstall: existing
# encrypted credentials would become unrecoverable.
create_encryption_env() {
    if [ -f "$CHV_CONFIG_DIR/encryption.env" ]; then
        # Never regenerate — but validate what we are preserving: an empty
        # or malformed file disables encryption, and (unlike an unset var)
        # an empty CHV_ENCRYPTION_KEY historically produced no runtime
        # warning either. Warn loudly so the operator can fix it.
        if grep -q '^CHV_ENCRYPTION_KEY=[0-9a-f]\{64\}$' \
            "$CHV_CONFIG_DIR/encryption.env"; then
            info "Preserving existing credential encryption key."
        else
            warn "Existing ${CHV_CONFIG_DIR}/encryption.env is empty or malformed;"
            warn "S3 credentials will be stored in PLAINTEXT until it is fixed"
            warn "(write a 64-hex-char CHV_ENCRYPTION_KEY, chmod 0600, restart chv-controlplane)."
        fi
    else
        info "Generating credential encryption key..."
        _chv_key="$(openssl rand -hex 32 2>/dev/null || true)"
        # Fail closed: command substitution swallows openssl's failure and
        # yields an empty string. Writing that would poison the file — the
        # create-if-absent guard above would then preserve the empty key on
        # every future run, silently disabling encryption forever.
        if ! printf '%s' "$_chv_key" | grep -qE '^[0-9a-f]{64}$'; then
            fatal "failed to generate the credential encryption key (openssl rand -hex 32); refusing to write an empty key — S3 credentials would be stored in plaintext with no warning"
        fi
        # Subshell: umask 077 must not leak into later file creation.
        (
            umask 077
            printf 'CHV_ENCRYPTION_KEY=%s\n' "$_chv_key" \
                > "$CHV_CONFIG_DIR/encryption.env"
        )
    fi
    chmod 0600 "$CHV_CONFIG_DIR/encryption.env"
}

# -----------------------------------------------------------------------------
# Bootstrap Token (written to file; control plane reads it on startup)
# -----------------------------------------------------------------------------
create_bootstrap_token() {
    info "Creating bootstrap token..."

    BOOTSTRAP_TOKEN=$(openssl rand -hex 32)
    printf '%s' "$BOOTSTRAP_TOKEN" > "$CHV_CONFIG_DIR/bootstrap.token"
    chmod 640 "$CHV_CONFIG_DIR/bootstrap.token"
    chown root:"$CHV_USER" "$CHV_CONFIG_DIR/bootstrap.token"
}

# -----------------------------------------------------------------------------
# systemd Services
# -----------------------------------------------------------------------------
install_systemd_services() {
    info "Installing systemd services..."

    # Install the canonical unit files verbatim — the same content the .deb
    # packages ship (packaging/systemd/, mirrored into docs/examples/systemd/
    # and the release tarball's systemd/). This is deliberately a copy from a
    # single source of truth: install.sh previously embedded its own unit
    # definitions, which drifted into the stale pre-#323/#328 model (stord as
    # the isolated chv-stord user with legacy capabilities, nwd as root with
    # no hardening) and shadowed the packaged units in /etc/systemd/system.
    local unit_src=""
    if [ -f "${EXTRACT_DIR}/systemd/chv-agent.service" ]; then
        unit_src="${EXTRACT_DIR}/systemd"
    elif [ -f "${SCRIPT_DIR}/../packaging/systemd/chv-agent.service" ]; then
        # Running from a repository checkout (dev mode).
        unit_src="${SCRIPT_DIR}/../packaging/systemd"
    else
        fatal "canonical systemd units not found (looked in ${EXTRACT_DIR}/systemd and ${SCRIPT_DIR}/../packaging/systemd); refusing to fall back to stale embedded copies"
    fi
    info "Installing systemd units from: ${unit_src}"

    local unit
    for unit in chv-controlplane chv-agent chv-stord chv-nwd; do
        install -m 0644 "${unit_src}/${unit}.service" /etc/systemd/system/
    done

    # Same tmpfiles entry the .deb ships (#324): creates /run/netns with the
    # ownership/mode the hardened chv-nwd unit requires. Install the config
    # so it survives reboots, then apply it immediately so the units can
    # start without one. Fail closed like the units above: without this
    # entry the chv-nwd unit cannot start (ReadWritePaths=/run/netns).
    local tmpfiles_src=""
    if [ -f "${EXTRACT_DIR}/tmpfiles/chv-node.conf" ]; then
        tmpfiles_src="${EXTRACT_DIR}/tmpfiles/chv-node.conf"
    elif [ -f "${SCRIPT_DIR}/../packaging/tmpfiles/chv-node.conf" ]; then
        tmpfiles_src="${SCRIPT_DIR}/../packaging/tmpfiles/chv-node.conf"
    fi
    if [ -n "$tmpfiles_src" ]; then
        mkdir -p /usr/lib/tmpfiles.d
        install -m 0644 "$tmpfiles_src" /usr/lib/tmpfiles.d/chv-node.conf
        # Fail closed on invocation/config errors (previously
        # '2>/dev/null || true' swallowed even those). Caveat:
        # systemd-tmpfiles can exit 0 while skipping individual entries
        # for some failure classes — the qual legs' /run/netns mode/owner
        # assertions bound that residual gap.
        if ! systemd-tmpfiles --create /usr/lib/tmpfiles.d/chv-node.conf \
            >/dev/null 2>&1; then
            fatal "systemd-tmpfiles --create failed for chv-node.conf; the chv-nwd unit cannot start without /run/netns"
        fi
    else
        fatal "canonical tmpfiles entry not found (looked in ${EXTRACT_DIR}/tmpfiles and ${SCRIPT_DIR}/../packaging/tmpfiles); the chv-nwd unit cannot start without /run/netns"
    fi

    systemctl daemon-reload
}

# -----------------------------------------------------------------------------
# nginx Setup (serves UI + proxies API)
# -----------------------------------------------------------------------------
install_nginx() {
    info "Configuring nginx..."

    cat > /etc/nginx/conf.d/chv-websocket-map.conf <<'EOF'
map $http_upgrade $connection_upgrade {
    default upgrade;
    '' close;
}
EOF

    # D3 target (#447): the control plane binary serves the UI itself
    # ([webui] enabled in the generated controlplane.toml), so nginx is
    # a PROXY-ONLY edge — the static root, try_files/SPA fallback, and
    # Cache-Control locations now live in the binary
    # (crates/chv-controlplane-service/src/api/router.rs). The /ws/
    # serial-console proxy stays here: the binary does not proxy /ws/
    # (the #447 decision — documented in docs/DEPLOYMENT.md, not
    # implemented; maintainer-ratified 2026-10-07).
    cat > /etc/nginx/sites-available/chv <<'EOF'
server {
    listen 80;
    server_name _;

    # Everything (UI + BFF/API) from the control plane binary.
    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }

    # WebSocket proxy for serial console.
    # The BFF returns proxied paths like /ws/vms/{node_id}/{vm_id}/console?token=...
    # We strip the node_id and forward /vms/{vm_id}/console?token=... to the agent.
    location ~ ^/ws/vms/[^/]+/(.*)$ {
        proxy_pass http://127.0.0.1:8444/vms/$1$is_args$args;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_read_timeout 3600s;
        proxy_send_timeout 3600s;
    }

    gzip on;
    gzip_types text/plain text/css application/json application/javascript text/xml application/xml+rss text/javascript;
}
EOF

    rm -f /etc/nginx/sites-enabled/default
    ln -sf /etc/nginx/sites-available/chv /etc/nginx/sites-enabled/chv

    nginx -t || fatal "nginx configuration test failed"
    systemctl restart nginx
}

# -----------------------------------------------------------------------------
# Start Services and Wait for Enrollment
# -----------------------------------------------------------------------------
start_services() {
    info "Enabling and starting CHV services..."

    # Phase 1: Start controlplane briefly to run migrations and create schema
    systemctl enable --now chv-controlplane

    info "Waiting for control plane to apply database migrations (up to 30s)..."
    local attempt=1
    while [ $attempt -le 30 ]; do
        if curl -sf "http://127.0.0.1:8080/health" &>/dev/null; then
            info "Control plane API is up (migrations applied)."
            break
        fi
        sleep 1
        ((attempt++))
    done
    if [ $attempt -gt 30 ]; then
        fatal "Control plane did not become healthy within 30s. Check: journalctl -u chv-controlplane -n 50"
    fi

    # Phase 2: Seed bootstrap token via HTTP API (no restart needed).
    # This avoids WAL contention between system sqlite3 and the bundled SQLite
    # in the controlplane process (which caused SQLITE_IOERR_SHORT_READ error 522).
    info "Seeding bootstrap token via API..."
    local seed_response
    seed_response=$(curl -sf -X POST "http://127.0.0.1:8080/internal/bootstrap-token" \
        -H "Content-Type: application/json" \
        -d "{\"token\": \"${BOOTSTRAP_TOKEN}\", \"description\": \"All-in-one installer\", \"one_time_use\": true}" 2>/dev/null) || true

    if echo "$seed_response" | grep -q '"status":"ok"'; then
        info "Bootstrap token seeded via API successfully."
    else
        # Fallback: stop controlplane, use sqlite3 CLI, then restart.
        # This path handles older controlplane builds that lack the /internal/bootstrap-token endpoint.
        warn "API seeding failed (response: ${seed_response:-empty}), falling back to sqlite3 CLI..."
        systemctl stop chv-controlplane
        sleep 1

        info "Inserting bootstrap token into database via sqlite3..."
        local token_hash
        token_hash=$(printf '%s' "$BOOTSTRAP_TOKEN" | sha256sum | awk '{print $1}')
        local expires
        expires=$(date -u -d "+1 hour" '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null \
                  || date -u -v+1H '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null \
                  || echo "")
        if ! cmd_exists sqlite3; then
            fatal "sqlite3 is required to seed bootstrap tokens. Install sqlite3 or rerun without INSTALL_CHV_SKIP_DEPS=1."
        fi
        sudo -u "${CHV_USER}" sqlite3 "${CHV_DB_PATH}" \
            "INSERT OR REPLACE INTO bootstrap_tokens
             (token_hash, description, one_time_use, used_at, expires_at, created_at, updated_at)
             VALUES (?, 'All-in-one installer', 1, NULL,
                     NULLIF(?, ''),
                     strftime('%Y-%m-%dT%H:%M:%SZ','now'),
                     strftime('%Y-%m-%dT%H:%M:%SZ','now'));" \
            "${token_hash}" "${expires}"

        local seeded_tokens
        seeded_tokens=$(sudo -u "${CHV_USER}" sqlite3 "${CHV_DB_PATH}" \
            "SELECT COUNT(*) FROM bootstrap_tokens WHERE token_hash=?;" "${token_hash}" 2>/dev/null || echo "0")
        if [ "${seeded_tokens}" -lt 1 ] 2>/dev/null; then
            fatal "Failed to seed bootstrap token in ${CHV_DB_PATH}."
        fi
        chown "${CHV_USER}:${CHV_USER}" "${CHV_DB_PATH}" "${CHV_DB_PATH}-wal" "${CHV_DB_PATH}-shm" 2>/dev/null || true

        # Restart controlplane after sqlite3 seeding
        info "Restarting control plane..."
        systemctl start chv-controlplane
        attempt=1
        while [ $attempt -le 30 ]; do
            if curl -sf "http://127.0.0.1:8080/health" &>/dev/null; then
                info "Control plane API is up."
                break
            fi
            sleep 1
            ((attempt++))
        done
        if [ $attempt -gt 30 ]; then
            fatal "Control plane did not become healthy after restart. Check: journalctl -u chv-controlplane -n 50"
        fi
    fi

    # Delete only the legacy enrollment cache. Core prerequisites, core.db, and
    # its persistent runtime lease are deliberately retained.
    rm -f "${CHV_DATA_DIR}/cache/agent-cache.json"

    # Pre-place agent client cert so mTLS works immediately
    mkdir -p /run/chv/agent
    cp "$CHV_CONFIG_DIR/certs/agent-client.crt" /run/chv/agent/agent.crt
    cp "$CHV_CONFIG_DIR/certs/agent-client.key" /run/chv/agent/agent.key
    cp "$CHV_CONFIG_DIR/certs/ca.crt" /run/chv/agent/ca.crt
    chown "$CHV_USER":"$CHV_USER" /run/chv/agent/agent.crt /run/chv/agent/agent.key /run/chv/agent/ca.crt
    chmod 640 /run/chv/agent/agent.key

    # Ensure storage directory ownership is correct before starting stord.
    # stord runs as the 'chv' service user (see the packaged
    # chv-stord.service): its API socket is 0600 with chv-agent as the only
    # client, and cloud-hypervisor (spawned by the agent as 'chv') must
    # read/write volume files. The chv:chv-stord 0770 contract matches
    # setup_user_and_dirs() and the .deb postinst (#323).
    if [ -d "$CHV_DATA_DIR/storage" ]; then
        chown -R "$CHV_USER:chv-stord" "$CHV_DATA_DIR/storage" 2>/dev/null || true
        chmod 770 "$CHV_DATA_DIR/storage" "$CHV_DATA_DIR/storage/localdisk" "$CHV_DATA_DIR/storage/lvm" 2>/dev/null || true
    fi
    mkdir -p "$CHV_DATA_DIR/agent/vms" 2>/dev/null || true
    chown "$CHV_USER:$CHV_USER" "$CHV_DATA_DIR/agent" 2>/dev/null || true
    chown -R "$CHV_USER:chv-stord" "$CHV_DATA_DIR/agent/vms" 2>/dev/null || true
    chmod 700 "$CHV_DATA_DIR/agent" 2>/dev/null || true
    chmod 775 "$CHV_DATA_DIR/agent/vms" 2>/dev/null || true

    systemctl enable --now chv-stord chv-nwd
    systemctl enable --now chv-agent

    # We intentionally do NOT wait for enrollment or TenantReady here.
    # start_services() runs before seed_admin_user(), so the admin password
    # file does not exist yet and we have no way to authenticate with the
    # BFF.  seed_dev_resources() performs its own TenantReady wait (60s)
    # before attempting VM creation, which is sufficient.
    info "Agent started.  Giving it a few seconds to begin enrollment..."
    sleep 5
}

# -----------------------------------------------------------------------------
# Seed the bootstrap admin user with a randomly-generated password.
#
# As of CHV 0.2.x, the SQL migrations no longer ship a default 'admin/admin'
# credential. This function:
#   1. Generates a 24-char URL-safe random password (openssl).
#   2. Bcrypts it at cost 12 (matching the cost used historically in 0008).
#   3. Inserts a single 'admin' row into the users table with
#      must_change_password=1 (see migration 0044).
#   4. Writes the plaintext to /etc/chv/initial_admin_password (mode 0600,
#      root-owned) and prints it ONCE in a stdout banner.
#
# Idempotent: if the admin user already exists in the DB, this is a no-op.
# Hard-fails if no bcrypt implementation is available; never falls back to
# a known/weak password.
# -----------------------------------------------------------------------------
seed_admin_user() {
    local pw_file="${CHV_CONFIG_DIR}/initial_admin_password"
    local db_path="${CHV_DB_PATH}"

    info "Seeding bootstrap admin user..."

    if ! cmd_exists sqlite3; then
        fatal "sqlite3 not available — cannot seed admin user. Install sqlite3 package."
    fi

    # Idempotency: if the admin user already exists, do nothing. We talk to the
    # SQLite DB directly because the API requires authentication (the very
    # thing we're bootstrapping).
    if [ -f "$db_path" ] && \
       [ "$(sqlite3 "$db_path" "SELECT COUNT(*) FROM users WHERE username = 'admin';" 2>/dev/null)" = "1" ]; then
        info "Admin user already exists — skipping seed."
        return 0
    fi

    # 18 bytes of entropy → 24 chars after base64; replace '+/' with '-_' for
    # URL-safety so operators can paste the password into URL query strings.
    local plaintext_pw
    plaintext_pw=$(openssl rand -base64 18 | tr -d '\n' | tr '+/' '-_')
    if [ -z "$plaintext_pw" ]; then
        fatal "Failed to generate random password (openssl rand failed)."
    fi

    # Bcrypt cost 12 matches the hashes that earlier migrations shipped, so
    # password verification cost stays consistent across upgrades.
    local hashed_pw=""
    if python3 -c 'import bcrypt' 2>/dev/null; then
        hashed_pw=$(python3 -c '
import bcrypt, sys
pw = sys.argv[1].encode("utf-8")
print(bcrypt.hashpw(pw, bcrypt.gensalt(rounds=12)).decode("utf-8"))
' "$plaintext_pw")
    elif cmd_exists htpasswd; then
        # htpasswd emits '$2y$' which bcrypt verifiers accept identically to '$2b$'.
        hashed_pw=$(htpasswd -nbBC 12 admin "$plaintext_pw" | sed 's/^admin://')
    else
        fatal "Neither python3-bcrypt nor htpasswd available — cannot bcrypt admin password. Install one of: 'pip3 install bcrypt' or 'apt install apache2-utils' (Debian/Ubuntu) / 'dnf install httpd-tools' (RHEL/Fedora)."
    fi

    if [ -z "$hashed_pw" ]; then
        fatal "Bcrypt produced empty hash — refusing to seed admin user."
    fi

    # Fixed user_id for predictability across reinstalls (matches the value
    # historically used by migration 0008).
    local admin_user_id="00000000-0000-0000-0000-000000000001"

    # The SQL is delivered via heredoc with the surrounding shell expanding
    # ${admin_user_id} and ${hashed_pw}. The bcrypt hash contains '$' but those
    # are protected by the surrounding single quotes inside the SQL itself
    # (heredoc expands shell vars but the SQL parser sees the literal string).
    if ! sqlite3 "$db_path" <<SQL
INSERT INTO users (user_id, username, password_hash, role, display_name, must_change_password, created_at, updated_at)
VALUES ('${admin_user_id}', 'admin', '${hashed_pw}', 'admin', 'Administrator', 1,
        strftime('%Y-%m-%dT%H:%M:%SZ','now'),
        strftime('%Y-%m-%dT%H:%M:%SZ','now'));
SQL
    then
        fatal "Failed to insert bootstrap admin row into ${db_path}."
    fi

    # Atomically create the credential file with strict perms BEFORE writing
    # the plaintext, so there is no window where the file exists with permissive
    # default umask perms.
    install -m 0600 -o root -g root /dev/null "$pw_file"
    printf '%s\n' "$plaintext_pw" > "$pw_file"
    chmod 0600 "$pw_file"

    cat <<BANNER

================================================================================
  CHV Bootstrap Admin Credentials
================================================================================
  Username:      admin
  Password:      ${plaintext_pw}
  Stored at:     ${pw_file} (mode 0600, root only)
  Rotation:      Required on first login (must_change_password=1).

  RECORD THIS PASSWORD NOW. It will not be displayed again.
================================================================================

BANNER
}

# -----------------------------------------------------------------------------
# Wipe Deployment (--wipe flag: teardown while preserving agent authority state)
# -----------------------------------------------------------------------------
wipe_deployment() {
    info "Wiping previous CHV deployment (--wipe)..."
    # Stop and disable all services
    for svc in chv-agent chv-controlplane chv-stord chv-nwd; do
        systemctl stop "$svc" 2>/dev/null || true
        systemctl disable "$svc" 2>/dev/null || true
    done
    remove_running_chv_processes
    # Remove systemd units
    rm -f /etc/systemd/system/chv-*.service
    systemctl daemon-reload
    # Remove replaceable deployment state. Agent authority state is persistent:
    # deleting its database or runtime lease could orphan VM identity or create
    # two independent lock namespaces on a concurrent-start failure.
    rm -rf "${CHV_CONFIG_DIR}"
    rm -rf "${CHV_DATA_DIR}/cache" "${CHV_DATA_DIR}/storage"
    rm -f "${CHV_DATA_DIR}/agent/agent.crt" "${CHV_DATA_DIR}/agent/agent.key" "${CHV_DATA_DIR}/agent/ca.crt"
    rm -f "${CHV_DATA_DIR}/controlplane.db"*
    rm -rf "${CHV_RUN_DIR}"
    rm -rf "${CHV_LOG_DIR}"
    rm -rf "${CHV_UI_DIR}"
    rm -rf "${CHV_MIGRATIONS_DIR}"
    # Remove binaries
    rm -f /usr/bin/chv-controlplane /usr/bin/chv-agent \
          /usr/bin/chv-stord /usr/bin/chv-nwd
    # Remove nginx config
    rm -f /etc/nginx/sites-available/chv /etc/nginx/sites-enabled/chv
    # Remove the tmpfiles entry installed with the units (#324). /run/netns
    # itself is left in place: it is tmpfs (emptied at boot anyway) and other
    # tooling may reference the directory.
    rm -f /usr/lib/tmpfiles.d/chv-node.conf
    # Remove bridge network
    if ip link show "${INSTALL_CHV_BRIDGE_NAME}" &>/dev/null; then
        ip link set "${INSTALL_CHV_BRIDGE_NAME}" down 2>/dev/null || true
        ip link delete "${INSTALL_CHV_BRIDGE_NAME}" 2>/dev/null || true
    fi
    # Remove iptables NAT rules
    local bridge_subnet
    bridge_subnet=$(echo "${INSTALL_CHV_BRIDGE_CIDR}" | sed 's|/[0-9]*$||' | sed 's|\.[0-9]*$|.0/24|')
    iptables -t nat -D POSTROUTING -s "${bridge_subnet}" ! -o "${INSTALL_CHV_BRIDGE_NAME}" -j MASQUERADE 2>/dev/null || true
    info "Wipe complete; persistent agent authority state was retained. Starting fresh install."
}

# -----------------------------------------------------------------------------
# Cleanup
# -----------------------------------------------------------------------------
reset_must_change_password() {
    # Ensure the bootstrap admin is flagged for rotation on first login even
    # if we temporarily cleared it during install to allow API seeding.
    if [ -f "${CHV_DB_PATH}" ] && cmd_exists sqlite3; then
        if ! sqlite3 "${CHV_DB_PATH}" \
            "UPDATE users SET must_change_password = 1 WHERE username = 'admin';" 2>/dev/null; then
            warn "CRITICAL: Failed to reset must_change_password flag. Admin may not be forced to rotate password on first login."
        fi
    fi
}

cleanup() {
    if [ -n "${CLEANUP_TMPDIR:-}" ] && [ -d "$CLEANUP_TMPDIR" ]; then
        rm -rf "$CLEANUP_TMPDIR"
    fi
    reset_must_change_password
}

# -----------------------------------------------------------------------------
# Main
# -----------------------------------------------------------------------------
trap cleanup EXIT

if [ "$INSTALL_CHV_WIPE" = "1" ]; then
    wipe_deployment
fi

install_dependencies
setup_user_and_dirs

# Clean up previous installation artifacts
#
# Persistent state (VM runtime data, VM disk images, and the control-plane
# database) is only ever removed when the operator explicitly opts in via
# INSTALL_CHV_WIPE=1 (or --wipe/--fresh). A plain re-run must never silently
# destroy running VMs or the control-plane database, so it refuses to proceed
# when persistent data is present and otherwise only replaces the replaceable
# artifacts (sockets, configs, binaries, UI assets, caches).
persistent_data_present() {
    if [ -d "${CHV_DATA_DIR}/agent/vms" ] && [ -n "$(ls -A "${CHV_DATA_DIR}/agent/vms" 2>/dev/null)" ]; then
        return 0
    fi
    # Real volume files live in stord's localdisk runtime dir
    # (runtime_dir = "${CHV_DATA_DIR}/storage/localdisk" in the generated
    # stord config), not under agent/.
    if [ -d "${CHV_DATA_DIR}/storage/localdisk" ] && [ -n "$(ls -A "${CHV_DATA_DIR}/storage/localdisk" 2>/dev/null)" ]; then
        return 0
    fi
    # User-imported images share the images dir with the base image the
    # script manages itself (BASE_IMAGE_PATH); any other file there is
    # persistent data a plain re-run must not destroy.
    if [ -d "${CHV_DATA_DIR}/images" ] && [ -n "$(find "${CHV_DATA_DIR}/images" -mindepth 1 -maxdepth 1 ! -name "ubuntu-noble-${CHV_ARCH}.img" ! -name "ubuntu-noble-${CHV_ARCH}.img.tmp" -print -quit 2>/dev/null)" ]; then
        return 0
    fi
    if [ -f "${CHV_DB_PATH}" ]; then
        return 0
    fi
    if ls "${CHV_DATA_DIR}/agent/"*.img >/dev/null 2>&1; then
        return 0
    fi
    return 1
}

clean_previous_install() {
    if [ "$INSTALL_CHV_WIPE" = "1" ]; then
        info "INSTALL_CHV_WIPE=1: persistent VM and control-plane database state will be wiped."
    elif persistent_data_present; then
        fatal "Persistent CHV data exists (${CHV_DATA_DIR}/agent/vms, ${CHV_DATA_DIR}/agent/*.img, ${CHV_DATA_DIR}/storage/localdisk, ${CHV_DATA_DIR}/images, or ${CHV_DB_PATH}).
Refusing to overwrite it on a re-run. To destroy existing VMs and the control-plane
database, rerun with INSTALL_CHV_WIPE=1 (or --wipe/--fresh)."
    fi

    info "Cleaning previous installation..."
    # Stop services if running
    for svc in chv-agent chv-controlplane chv-stord chv-nwd; do
        systemctl stop "$svc" 2>/dev/null || true
        systemctl disable "$svc" 2>/dev/null || true
    done
    remove_running_chv_processes
    validate_no_chv_processes
    rm -f "${CHV_RUN_DIR}/agent/api.sock" "${CHV_RUN_DIR}/stord/api.sock" "${CHV_RUN_DIR}/nwd/api.sock"
    rm -f "${CHV_RUN_DIR}"/nwd/dnsmasq-*.conf "${CHV_RUN_DIR}"/nwd/dnsmasq-*.hosts "${CHV_RUN_DIR}"/nwd/dnsmasq-*.pid
    # Remove old configs (will be regenerated)
    rm -f "${CHV_CONFIG_DIR}/agent.toml" "${CHV_CONFIG_DIR}/controlplane.toml" \
          "${CHV_CONFIG_DIR}/stord.toml" "${CHV_CONFIG_DIR}/nwd.toml" \
          "${CHV_CONFIG_DIR}/bootstrap.token"
    # Remove replaceable agent-dir artifacts (certs and daemon configs are
    # re-created from /etc/chv at startup).
    rm -f "${CHV_DATA_DIR}/agent/agent.crt" "${CHV_DATA_DIR}/agent/agent.key" \
          "${CHV_DATA_DIR}/agent/ca.crt" \
          "${CHV_DATA_DIR}/agent/chv-nwd.toml" "${CHV_DATA_DIR}/agent/chv-stord.toml"
    # Persistent VM/volume/database state: gated on the explicit wipe flag.
    if [ "$INSTALL_CHV_WIPE" = "1" ]; then
        rm -rf "${CHV_DATA_DIR}/agent/vms" "${CHV_DATA_DIR}/agent/"*.img
        rm -f "${CHV_DB_PATH}" "${CHV_DB_PATH}-wal" "${CHV_DB_PATH}-shm" "${CHV_DB_PATH}-journal"
    fi
    # Remove old cache
    rm -f "${CHV_DATA_DIR}/cache/agent-cache.json"
    info "Previous installation cleaned."
}
clean_previous_install

resolve_version
download_release
install_binaries_and_assets
install_cloud_hypervisor
download_base_image
copy_firmware        # explicit local override wins (/root/CLOUDHV.fd)
download_firmware    # otherwise fetch the pinned firmware into ${FIRMWARE_PATH}
generate_certs
setup_network
install_configs
create_encryption_env
create_bootstrap_token
install_systemd_services
install_nginx
start_services

# Seed bootstrap admin (must run before import_base_image which authenticates).
seed_admin_user || fatal "Failed to seed bootstrap admin user."

# Temporarily clear must_change_password so the install script's own API calls
# (image import, network + VM creation) are not blocked by the password-rotation
# middleware.  The flag is restored to 1 in cleanup() / reset_must_change_password()
# so the operator is still forced to rotate on first UI login.
info "Temporarily clearing must_change_password for install-time API seeding..."
sqlite3 "${CHV_DB_PATH}" \
    "UPDATE users SET must_change_password = 0 WHERE username = 'admin';" \
    || warn "Failed to clear must_change_password; API seeding may be blocked."

import_base_image

if [ "${INSTALL_CHV_NO_SEED:-0}" != "1" ]; then
    seed_dev_resources
fi

LOCAL_IP=$(get_local_ip)

cat <<EOF

===============================================
  CHV All-in-One Installation Complete!
===============================================

Version:        ${INSTALL_CHV_VERSION}
Node ID:        ${CHV_NODE_ID}
Web UI:         http://${LOCAL_IP}/
API:            http://127.0.0.1:8080/
Database:       ${CHV_DB_PATH}
EOF
    if [ "$INSTALL_CHV_NO_BRIDGE" = "1" ]; then
        cat <<EOF2
Bridge:         skipped (INSTALL_CHV_NO_BRIDGE=1; networks are created via
                the API and managed by chv-nwd)
EOF2
    else
        cat <<EOF2
Bridge:         ${INSTALL_CHV_BRIDGE_NAME} (${INSTALL_CHV_BRIDGE_CIDR})
Upstream iface: ${INSTALL_CHV_BRIDGE_IFACE} (NAT enabled)
EOF2
    fi
    cat <<EOF

Services:
  systemctl status chv-controlplane
  systemctl status chv-agent
  systemctl status nginx

Logs:
  journalctl -u chv-controlplane -f
  journalctl -u chv-agent -f

Defaults:
  Admin login:   admin / (see /etc/chv/initial_admin_password)
                 The bootstrap password is in that file (mode 0600 root).
                 You will be required to rotate it on first login.
  Bootstrap token valid for 1 hour from installation.

EOF
