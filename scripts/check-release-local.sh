#!/bin/bash
# CHV Local Release Check
# Runs formatting, linting, tests, release build, and version validation.
#
# Usage:
#   ./scripts/check-release-local.sh
#   make check

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${PROJECT_ROOT}"

VERSION="$(cat "${PROJECT_ROOT}/VERSION")"

echo "==============================================="
echo "CHV Local Release Check"
echo "Version: ${VERSION}"
echo "==============================================="

# ---------------------------------------------------------------------------
# 1. Formatting check
# ---------------------------------------------------------------------------
echo "[1/6] Checking formatting..."
cargo fmt --all -- --check
echo "  OK"

# ---------------------------------------------------------------------------
# 2. Lint
# ---------------------------------------------------------------------------
echo "[2/6] Running clippy..."
cargo clippy --workspace -- -D warnings
echo "  OK"

# ---------------------------------------------------------------------------
# 3. Tests
# ---------------------------------------------------------------------------
echo "[3/6] Running tests..."
cargo test --workspace
echo "  OK"

# ---------------------------------------------------------------------------
# 4. Release build
# ---------------------------------------------------------------------------
echo "[4/6] Building release binaries..."
cargo build --workspace --release
echo "  OK"

# ---------------------------------------------------------------------------
# 5. Version output check
# ---------------------------------------------------------------------------
echo "[5/6] Checking CLI version output..."
cargo build --package chvctl >/dev/null 2>&1 || cargo build --package chvctl
VERSION_OUTPUT="$("${PROJECT_ROOT}/target/debug/chvctl" version)"
echo "  Output: ${VERSION_OUTPUT}"

if ! echo "${VERSION_OUTPUT}" | grep -q "chvctl"; then
    echo "  FAIL: expected 'chvctl' in version output" >&2
    exit 1
fi
if ! echo "${VERSION_OUTPUT}" | grep -q "commit"; then
    echo "  FAIL: expected 'commit' in version output" >&2
    exit 1
fi
if ! echo "${VERSION_OUTPUT}" | grep -q "build"; then
    echo "  FAIL: expected 'build' in version output" >&2
    exit 1
fi
if ! echo "${VERSION_OUTPUT}" | grep -q "channel"; then
    echo "  FAIL: expected 'channel' in version output" >&2
    exit 1
fi
echo "  OK"

# ---------------------------------------------------------------------------
# 6. Hosting template copies
# ---------------------------------------------------------------------------
echo "[6/6] Checking hosting template copies..."
for f in cloudflare-worker.js github-pages-index.html; do
    if ! cmp -s "${PROJECT_ROOT}/${f}" "${PROJECT_ROOT}/scripts/hosting/${f}"; then
        echo "  FAIL: ${f} differs from scripts/hosting/${f}" >&2
        exit 1
    fi
done
echo "  OK"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "==============================================="
echo "All checks passed!"
echo "==============================================="
