#!/bin/bash
# Dev Install: build from source, uninstall any previous installation, then
# install all-in-one on the local machine with predefined dev defaults.
#
# After install, a default network and a test VM are automatically created:
#   Network: default (CIDR derived from INSTALL_CHV_BRIDGE_CIDR)
#   VM:      test-1 (1 CPU, 512 MB RAM, 10 GB disk)
#
# Usage: sudo ./scripts/dev-install.sh [--no-uninstall] [--no-seed] [--install-only] [--wipe]
#
# Predefined dev defaults (override via environment):
#   INSTALL_CHV_BRIDGE_IFACE  - default: ens19
#   INSTALL_CHV_BRIDGE_NAME   - default: chvbr0
#   INSTALL_CHV_BRIDGE_CIDR   - default: 10.200.0.1/24
#
# Options:
#   --no-uninstall   Skip the uninstall step (useful for first install)
#   --no-seed        Skip creating the default network and test-1 VM
#   --install-only   Skip uninstall + build; run install.sh with existing tarball
#   --wipe           Full teardown before clean install (removes certs, DB, network)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

if ! command -v cargo &>/dev/null; then
    if [ -f "$HOME/.cargo/env" ]; then
        source "$HOME/.cargo/env"
    elif [ -d "/root/.cargo/bin" ]; then
        export PATH="/root/.cargo/bin:$PATH"
    fi
fi

NO_UNINSTALL=0
NO_SEED=0
INSTALL_ONLY=0
WIPE=0
for arg in "$@"; do
    case "$arg" in
        --no-uninstall) NO_UNINSTALL=1 ;;
        --no-seed) NO_SEED=1 ;;
        --install-only) INSTALL_ONLY=1 ;;
        --wipe) WIPE=1 ;;
        *) echo "Unknown argument: $arg"; exit 1 ;;
    esac
done

# Dev network defaults
export INSTALL_CHV_BRIDGE_IFACE="${INSTALL_CHV_BRIDGE_IFACE:-ens19}"
export INSTALL_CHV_BRIDGE_NAME="${INSTALL_CHV_BRIDGE_NAME:-chvbr0}"
export INSTALL_CHV_BRIDGE_CIDR="${INSTALL_CHV_BRIDGE_CIDR:-10.200.0.1/24}"

echo "==============================================="
echo "CHV Local Dev Install (All-in-One)"
echo "Bridge: ${INSTALL_CHV_BRIDGE_NAME} (${INSTALL_CHV_BRIDGE_CIDR}) on ${INSTALL_CHV_BRIDGE_IFACE}"
if [ "$NO_SEED" = "0" ]; then
    echo "Seed:   default network + test-1 VM (1 CPU, 512 MB)"
else
    echo "Seed:   skipped (--no-seed)"
fi
if [ "$INSTALL_ONLY" = "1" ]; then
    echo "Mode:   install-only (skipping uninstall + build)"
fi
echo "==============================================="
echo ""

if [ "$(id -u)" -ne 0 ]; then
    echo "This script must be run as root. Try:"
    echo "  sudo ./scripts/dev-install.sh"
    exit 1
fi

BUILD_USER="${SUDO_USER:-}"
if [ -z "$BUILD_USER" ] || ! id -u "$BUILD_USER" &>/dev/null; then
    BUILD_USER="root"
fi
if [ "$BUILD_USER" != "root" ]; then
    if ! sudo -u "$BUILD_USER" test -r "$PROJECT_ROOT" \
        || ! sudo -u "$BUILD_USER" test -w "$PROJECT_ROOT" \
        || ! sudo -u "$BUILD_USER" test -x "$PROJECT_ROOT"; then
        echo "  [WARN] $BUILD_USER cannot access $PROJECT_ROOT; falling back to root build."
        BUILD_USER="root"
    fi
fi

# Determine version and tarball path up front (needed for install-only mode)
VERSION=$(cat "${PROJECT_ROOT}/VERSION" | tr -d '[:space:]')
TARBALL="${PROJECT_ROOT}/dist/chv-${VERSION}-linux-amd64.tar.gz"

step_num=0
next_step() {
    step_num=$((step_num + 1))
    echo "[${step_num}] $*"
}

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
    if command -v ss &>/dev/null; then
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
        echo "  No CHV runtime processes remain."
        return 0
    fi

    echo "  Terminating leftover CHV runtime processes: ${pids[*]}"
    kill -TERM "${pids[@]}" 2>/dev/null || true
    if ! wait_for_chv_process_exit; then
        mapfile -t pids < <(list_chv_runtime_processes | awk '{print $1}' | sort -u)
        if [ "${#pids[@]}" -gt 0 ]; then
            echo "  Forcing leftover CHV runtime processes to exit: ${pids[*]}"
            kill -KILL "${pids[@]}" 2>/dev/null || true
        fi
    fi
}

validate_no_chv_processes() {
    local remaining
    remaining="$(list_chv_runtime_processes)"
    if [ -n "$remaining" ]; then
        echo "ERROR: CHV runtime processes are still running after cleanup:"
        echo "$remaining"
        exit 1
    fi

    remaining="$(list_chv_socket_listeners)"
    if [ -n "$remaining" ]; then
        echo "ERROR: CHV runtime socket listeners are still active after cleanup:"
        echo "$remaining"
        exit 1
    fi

    echo "  Validated: no CHV processes or runtime socket listeners remain."
}

# -----------------------------------------------------------------------------
# Uninstall previous installation
# -----------------------------------------------------------------------------
if [ "$INSTALL_ONLY" = "1" ]; then
    if [ ! -f "$TARBALL" ]; then
        echo "ERROR: --install-only requires existing tarball: $TARBALL"
        echo "Run without --install-only first, or build manually with:"
        echo "  ./scripts/build-release.sh"
        exit 1
    fi
    echo "[skip] Uninstall + build skipped (--install-only)"
elif [ "$NO_UNINSTALL" = "0" ]; then
    next_step "Removing previous CHV installation..."

    # Stop and disable all CHV services
    for svc in chv-agent chv-stord chv-nwd chv-controlplane; do
        if systemctl is-active --quiet "$svc" 2>/dev/null; then
            echo "  Stopping $svc..."
            systemctl stop "$svc" || true
        fi
        if systemctl is-enabled --quiet "$svc" 2>/dev/null; then
            systemctl disable "$svc" || true
        fi
    done

    remove_running_chv_processes
    validate_no_chv_processes

    # Remove systemd unit files
    rm -f /etc/systemd/system/chv-controlplane.service
    rm -f /etc/systemd/system/chv-agent.service
    rm -f /etc/systemd/system/chv-stord.service
    rm -f /etc/systemd/system/chv-nwd.service
    systemctl daemon-reload

    # Remove binaries
    rm -f /usr/local/bin/chv-controlplane
    rm -f /usr/local/bin/chv-agent
    rm -f /usr/local/bin/chv-stord
    rm -f /usr/local/bin/chv-nwd

    # Remove config (keep certs to avoid re-generating if same host)
    rm -f /etc/chv/controlplane.toml
    rm -f /etc/chv/agent.toml
    rm -f /etc/chv/stord.toml
    rm -f /etc/chv/nwd.toml
    rm -f /etc/chv/bootstrap.token

    # Remove runtime sockets left behind by unclean daemon exits.
    rm -f /run/chv/agent/api.sock
    rm -f /run/chv/stord/api.sock
    rm -f /run/chv/nwd/api.sock
    rm -f /run/chv/nwd/dnsmasq-*.conf
    rm -f /run/chv/nwd/dnsmasq-*.hosts
    rm -f /run/chv/nwd/dnsmasq-*.pid
    rm -f /run/chv/nwd/dnsmasq-*.leases
    # Durable nwd state (topology journal + DHCP reservations) is derived
    # from the deployment being replaced.
    rm -rf /var/lib/chv/nwd

    # Remove database (clean state for new install)
    rm -f /var/lib/chv/controlplane.db*

    # Remove agent cache and stale certificates so re-install triggers fresh enrollment
    rm -f /var/lib/chv/cache/agent-cache.json
    rm -f /var/lib/chv/agent/agent.crt /var/lib/chv/agent/agent.key /var/lib/chv/agent/ca.crt

    # Remove any stale volume files so seeding works on fresh install
    rm -rf /var/lib/chv/storage/localdisk/*

    # Remove UI assets and migrations
    rm -rf /opt/chv/ui/*
    rm -rf /usr/local/share/chv/migrations/*

    # Remove nginx site (will be reinstalled)
    rm -f /etc/nginx/sites-enabled/chv
    rm -f /etc/nginx/sites-available/chv

    # Tear down bridge (will be recreated)
    if ip link show "${INSTALL_CHV_BRIDGE_NAME}" &>/dev/null; then
        echo "  Removing bridge ${INSTALL_CHV_BRIDGE_NAME}..."
        ip link set "${INSTALL_CHV_BRIDGE_NAME}" down || true
        ip link delete "${INSTALL_CHV_BRIDGE_NAME}" type bridge 2>/dev/null || true
    fi

    # Remove network persistence files
    rm -f "/etc/network/interfaces.d/${INSTALL_CHV_BRIDGE_NAME}"
    rm -f "/etc/systemd/network/10-${INSTALL_CHV_BRIDGE_NAME}.netdev"
    rm -f "/etc/systemd/network/11-${INSTALL_CHV_BRIDGE_NAME}.network"
    rm -f "/etc/systemd/network/12-${INSTALL_CHV_BRIDGE_IFACE}.network"

    echo "  Previous installation removed."
else
    echo "[skip] Skipping uninstall (--no-uninstall)"
fi

# -----------------------------------------------------------------------------
# Build release tarball from source
# -----------------------------------------------------------------------------
if [ "$INSTALL_ONLY" = "1" ]; then
    echo "[skip] Build skipped (--install-only)"
else
    next_step "Building release tarball from source..."
    cd "$PROJECT_ROOT"
    if [ "$BUILD_USER" = "root" ]; then
        ./scripts/build-release.sh
    else
        echo "  Building as user: $BUILD_USER (preserves incremental cargo/npm caches)"
        sudo -u "$BUILD_USER" -H /bin/bash -lc "cd \"$PROJECT_ROOT\" && ./scripts/build-release.sh"
    fi

    if [ ! -f "$TARBALL" ]; then
        echo "ERROR: Build completed but tarball not found: $TARBALL"
        exit 1
    fi
    next_step "Built tarball: $TARBALL"
fi

# -----------------------------------------------------------------------------
# Run installer with local tarball
# -----------------------------------------------------------------------------
next_step "Running installer..."
export INSTALL_CHV_TARBALL_PATH="$(realpath "$TARBALL")"
export INSTALL_CHV_VERSION="$VERSION"
export INSTALL_CHV_NO_SEED="$NO_SEED"
export INSTALL_CHV_WIPE="$WIPE"

"$PROJECT_ROOT/scripts/install.sh"

chown chv:chv /var/lib/chv/controlplane.db /var/lib/chv/controlplane.db-wal /var/lib/chv/controlplane.db-shm 2>/dev/null || true

echo ""
echo "Dev install complete! Version: $VERSION"
if [ "$NO_SEED" = "0" ]; then
    echo ""
    echo "Seeded resources:"
    echo "  Network: default (${INSTALL_CHV_BRIDGE_CIDR%/*} network)"
    echo "  VM:      test-1 (1 CPU, 512 MB RAM, 10 GB disk)"
fi
