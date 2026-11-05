#!/bin/bash
# Build CHV .deb and .rpm packages using nFPM.
#
# Pipeline role: The main package builder. Compiles Rust binaries + Web UI,
# then calls nfpm for each packaging/nfpm/*.yaml config. Produces .deb and
# .rpm files in dist/packages/. Called by: Makefile, CI workflows.
# Usage: ./scripts/build-packages.sh [--skip-build] [--format deb|rpm]
#
# Environment:
#   PACKAGE_VERSION    - override the package version (default: derived from VERSION + channel)
#   CHV_RELEASE_CHANNEL - release channel: stable, rc, nightly, pr (default: stable)
#   ARCH               - target architecture (default: amd64)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

ARCH="${ARCH:-amd64}"
OUTDIR="${REPO_ROOT}/dist/packages"
CHANNEL="${CHV_RELEASE_CHANNEL:-stable}"

SKIP_BUILD=false
FORMAT=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --skip-build)
            SKIP_BUILD=true
            shift
            ;;
        --format)
            if [[ -n "${2:-}" ]]; then
                FORMAT="$2"
                if [[ "$FORMAT" != "deb" && "$FORMAT" != "rpm" ]]; then
                    echo "ERROR: --format must be 'deb' or 'rpm'" >&2
                    exit 1
                fi
                shift 2
            else
                echo "ERROR: --format requires an argument (deb or rpm)" >&2
                exit 1
            fi
            ;;
        --format=*)
            FORMAT="${1#--format=}"
            if [[ "$FORMAT" != "deb" && "$FORMAT" != "rpm" ]]; then
                echo "ERROR: --format must be 'deb' or 'rpm'" >&2
                exit 1
            fi
            shift
            ;;
        *)
            echo "WARNING: Unknown argument: $1" >&2
            shift
            ;;
    esac
done

cd "${REPO_ROOT}"

# Check prerequisites
check_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "ERROR: '$1' is required but not installed." >&2
        return 1
    fi
}

if [ "$SKIP_BUILD" = false ]; then
    if ! check_cmd cargo; then
        echo "Install Rust: https://rustup.rs/" >&2
        exit 1
    fi

    if ! check_cmd npm; then
        echo "Install Node.js / npm." >&2
        exit 1
    fi
fi

if ! check_cmd nfpm; then
    echo "ERROR: nfpm is not installed." >&2
    echo "Install it with:" >&2
    echo "  go install github.com/goreleaser/nfpm/v2/cmd/nfpm@latest" >&2
    echo "Or download a binary from https://github.com/goreleaser/nfpm/releases" >&2
    exit 1
fi

if ! check_cmd envsubst; then
    echo "ERROR: envsubst is not installed (install gettext)." >&2
    exit 1
fi

mkdir -p "${OUTDIR}"

# Build Rust binaries
if [ "$SKIP_BUILD" = false ]; then
    echo "==> Building Rust binaries (release)..."
    cargo build --workspace --release
else
    echo "==> Skipping Rust build (--skip-build)"
fi

# Build Web UI
if [ "$SKIP_BUILD" = false ]; then
    echo "==> Building Web UI..."
    cd ui
    npm install
    npm run build
    cd "${REPO_ROOT}"
else
    echo "==> Skipping UI build (--skip-build)"
fi

# Prepare adapted config files with correct paths for packaging
TMPDIR=$(mktemp -d)
trap 'rm -rf "${TMPDIR}"' EXIT

mkdir -p "${TMPDIR}/configs"

sed 's|/usr/local/share/chv/migrations|/usr/share/chv/migrations|g; s|/usr/local/share/chv/monitoring-migrations|/usr/share/chv/monitoring-migrations|g' \
    docs/examples/controlplane.toml > "${TMPDIR}/configs/controlplane.toml"

sed 's|/usr/local/bin/chv-stord|/usr/bin/chv-stord|g; s|/usr/local/bin/chv-nwd|/usr/bin/chv-nwd|g' \
    docs/examples/agent.toml > "${TMPDIR}/configs/agent.toml"

cp docs/examples/stord.toml "${TMPDIR}/configs/stord.toml"
cp docs/examples/nwd.toml "${TMPDIR}/configs/nwd.toml"
# Guest agent config (ADR-026): packaged paths already match the
# example; it passes through unchanged. The example doubles as the
# operator documentation for every field.
cp docs/examples/monitor-agent.toml "${TMPDIR}/configs/monitor-agent.toml"

# Determine version
version_extra_args() {
    case "$CHANNEL" in
        rc)
            echo "${CHV_RC_NUMBER:-1}"
            ;;
        pr)
            echo "${CHV_PR_NUMBER:-0}"
            ;;
        *)
            echo ""
            ;;
    esac
}

extra_args="$(version_extra_args)"
formats=("deb" "rpm")
if [[ -n "$FORMAT" ]]; then
    formats=("$FORMAT")
fi

# Build each package
for pkg_yaml in packaging/nfpm/*.yaml; do
    pkg_name="$(basename "$pkg_yaml" .yaml)"

    for fmt in "${formats[@]}"; do
        if [ -n "${PACKAGE_VERSION:-}" ]; then
            pkg_version="$PACKAGE_VERSION"
        else
            pkg_version="$("${REPO_ROOT}/scripts/version.sh" --"${fmt}" "${CHANNEL}" ${extra_args})"
        fi

        export ARCH="$ARCH"
        export PACKAGE_VERSION="$pkg_version"
        export TMPDIR="$TMPDIR"

        tmp_config="${TMPDIR}/nfpm-${pkg_name}-${fmt}.yaml"
        envsubst '$ARCH $PACKAGE_VERSION $TMPDIR' < "$pkg_yaml" > "$tmp_config"

        echo "==> Building ${pkg_name} (${fmt})..."
        nfpm package -f "$tmp_config" -p "$fmt" --target "${OUTDIR}/"
    done
done

# GitHub release assets sanitize characters outside [A-Za-z0-9._-] to '.',
# so a file built as `chv-controlplane_0.4.0~rc.1_amd64.deb` is published as
# `chv-controlplane_0.4.0.rc.1_amd64.deb`. Left alone, SHA256SUMS and the
# quick installer then name files that do not exist on the release. Emit the
# GitHub-safe name here (the package's internal Version keeps `~` for correct
# dpkg/rpm ordering) so every consumer sees one name.
safe_asset_name() {
    printf '%s' "$1" | tr -c 'A-Za-z0-9._-' '.'
}

for pkg in "${OUTDIR}"/*.deb "${OUTDIR}"/*.rpm; do
    [ -e "$pkg" ] || continue
    base="$(basename "$pkg")"
    safe_base="$(safe_asset_name "$base")"
    if [ "$base" != "$safe_base" ]; then
        mv -f -- "$pkg" "${OUTDIR}/${safe_base}"
    fi
done

# Checksums
echo "==> Generating SHA256 checksums..."
cd "$OUTDIR"
sha256sum *.deb *.rpm > SHA256SUMS 2>/dev/null || true
cd "$REPO_ROOT"

echo "==> Packages built successfully in ${OUTDIR}:"
ls -la "${OUTDIR}/"
