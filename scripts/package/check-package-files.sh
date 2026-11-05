#!/bin/bash
# Verify that built packages exist, contain the expected version, and are valid.
#
# Usage:
#   ./scripts/package/check-package-files.sh
#
# Environment:
#   PACKAGE_VERSION - expected version string (default: read from VERSION file)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
PKGDIR="${REPO_ROOT}/dist/packages"
VERSION="${PACKAGE_VERSION:-$(cat "${REPO_ROOT}/VERSION")}"

ERRORS=0
error() { echo "[FAIL] $*" >&2; ERRORS=$((ERRORS + 1)); }
info() { echo "[INFO] $*"; }

info "Checking for packages in ${PKGDIR}..."

# ---------------------------------------------------------------------------
# 1. Package files were produced
# ---------------------------------------------------------------------------
for pkg in chvctl chv-controlplane chv-node chv-monitor-agent; do
    deb_count=$(find "$PKGDIR" -maxdepth 1 -name "${pkg}_*.deb" | wc -l)
    rpm_count=$(find "$PKGDIR" -maxdepth 1 -name "${pkg}-*.rpm" | wc -l)

    if [ "$deb_count" -eq 0 ]; then
        error "No .deb found for ${pkg}"
    else
        info "  Found .deb for ${pkg}"
    fi

    if [ "$rpm_count" -eq 0 ]; then
        error "No .rpm found for ${pkg}"
    else
        info "  Found .rpm for ${pkg}"
    fi
done

# ---------------------------------------------------------------------------
# 2. Package names contain the expected version
# ---------------------------------------------------------------------------
info "Checking version strings in package names (expecting ${VERSION})..."
for deb in "${PKGDIR}"/*.deb; do
    [ -e "$deb" ] || continue
    if ! basename "$deb" | grep -q "$VERSION"; then
        error "$(basename "$deb") missing version ${VERSION}"
    fi
done
for rpm in "${PKGDIR}"/*.rpm; do
    [ -e "$rpm" ] || continue
    if ! basename "$rpm" | grep -q "$VERSION"; then
        error "$(basename "$rpm") missing version ${VERSION}"
    fi
done

# ---------------------------------------------------------------------------
# 3. Packages are non-empty
# ---------------------------------------------------------------------------
info "Checking packages are non-empty..."
for pkg in "${PKGDIR}"/*.deb "${PKGDIR}"/*.rpm; do
    [ -e "$pkg" ] || continue
    size=$(stat -c%s "$pkg" 2>/dev/null || stat -f%z "$pkg" 2>/dev/null)
    if [ "$size" -lt 100 ]; then
        error "$(basename "$pkg") is suspiciously small (${size} bytes)"
    fi
done

# ---------------------------------------------------------------------------
# 4. Package metadata can be inspected where tools are available
# ---------------------------------------------------------------------------
if command -v dpkg-deb &>/dev/null; then
    info "Inspecting Debian package metadata..."
    for deb in "${PKGDIR}"/*.deb; do
        [ -e "$deb" ] || continue
        if dpkg-deb -I "$deb" >/dev/null 2>&1; then
            info "  $(basename "$deb") metadata OK"
        else
            error "$(basename "$deb") metadata invalid"
        fi
    done
fi

if command -v rpm &>/dev/null; then
    info "Inspecting RPM package metadata..."
    for rpm in "${PKGDIR}"/*.rpm; do
        [ -e "$rpm" ] || continue
        if rpm -qip "$rpm" >/dev/null 2>&1; then
            info "  $(basename "$rpm") metadata OK"
        else
            error "$(basename "$rpm") metadata invalid"
        fi
    done
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
if [ "$ERRORS" -gt 0 ]; then
    echo ""
    echo "[RESULT] Check FAILED with ${ERRORS} error(s)"
    exit 1
else
    echo ""
    echo "[RESULT] Check PASSED"
    exit 0
fi
