#!/bin/bash
# Smoke test for scripts/bump-version.sh.
#
# Regression test for the sed-escaping bug that bit the 0.3.0 release cut
# (PR #473): the script interpolated the old version into sed patterns
# unescaped, so with OLD_VERSION=0.2.0 the regex `0.2.0` (dots match any
# character) matched "0.200" inside the bridge CIDR 10.200.0.1/24 and
# corrupted docs/DEPLOYMENT.md and scripts/install.sh — including the
# installer's runtime INSTALL_CHV_BRIDGE_CIDR default.
#
# The test builds a throwaway fixture tree shaped like the repo (VERSION,
# Cargo.tomls, ui/package.json, sidebar, README/docs/install/hosting files)
# containing BOTH genuine version refs (which must bump) and CIDR trap lines
# (which must stay byte-identical), runs the real bump script inside it, and
# asserts the outcome. No cargo, no network, no writes outside the fixture.
#
# It runs twice: once with the ambient environment (npm branch if npm is
# installed) and once with a restricted PATH containing no npm/cargo, which
# deterministically exercises the sed-fallback branch of the UI version
# rewrite (the other escaped call site).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

ERRORS=0

error() {
    echo "FAIL: $1" >&2
    ERRORS=$((ERRORS + 1))
}

assert_contains() {
    # assert_contains FILE NEEDLE — NEEDLE must appear (fixed string)
    if ! grep -qF -- "$2" "$1"; then
        error "expected '$2' to appear in $1"
    fi
}

assert_not_contains() {
    # assert_not_contains FILE NEEDLE — NEEDLE must NOT appear (fixed string)
    if grep -qF -- "$2" "$1"; then
        error "expected '$2' to NOT appear in $1"
    fi
}

assert_line_unchanged() {
    # assert_line_unchanged FILE LINE — LINE must appear byte-identical
    if ! grep -qxF -- "$2" "$1"; then
        error "expected line '$2' to be byte-identical in $1"
    fi
}

# ---------------------------------------------------------------------------
# Build the fixture tree. OLD_VERSION=0.2.0 is chosen deliberately: the
# unescaped regex `0.2.0` matches inside "10.200.0.1/24" (via "0.200") and
# "10.0.200.1/24" — exactly the observed corruption class. The trap lines
# must not contain the literal substring "0.2.0": a genuine literal
# substring (e.g. a hypothetical bridge CIDR 10.0.2.0/24) is intentionally
# still rewritten by the global replace and is a documented residual, not
# a bug this fix claims to prevent.
# ---------------------------------------------------------------------------
make_fixture() {
    local FIXTURE="$1"
    mkdir -p \
        "${FIXTURE}/scripts/hosting" \
        "${FIXTURE}/docs" \
        "${FIXTURE}/cmd/chv-fixture" \
        "${FIXTURE}/crates/chv-decoy" \
        "${FIXTURE}/ui/src/lib/components/shell"

    cp "${SCRIPT_DIR}/bump-version.sh" "${FIXTURE}/scripts/bump-version.sh"

    echo "0.2.0" > "${FIXTURE}/VERSION"

    cat > "${FIXTURE}/README.md" <<'EOF'
# chv fixture README

**Version:** `0.2.0`

Current version: `0.2.0` (see [`VERSION`](./VERSION))

    curl -fsSL https://raw.githubusercontent.com/o3kio/chv/main/scripts/install.sh | sudo INSTALL_CHV_VERSION=0.2.0 bash -s

The sidebar shows chv-v0.2.0-alpha.

Trap lines (must stay byte-identical):
10.200.0.1/24
10.0.200.1/24
EOF

    cat > "${FIXTURE}/docs/DEPLOYMENT.md" <<'EOF'
| `INSTALL_CHV_BRIDGE_CIDR` | `10.200.0.1/24` | Gateway IP / subnet |

BRIDGE_CIDR="10.200.0.1/24"

Version: 0.2.0
EOF

    cat > "${FIXTURE}/scripts/install.sh" <<'EOF'
# Fixture install script (not executed — only rewritten by bump-version.sh)
INSTALL_CHV_BRIDGE_CIDR="${INSTALL_CHV_BRIDGE_CIDR:-10.200.0.1/24}"
INSTALL_CHV_VERSION="0.2.0"
EOF

    cat > "${FIXTURE}/scripts/hosting/cloudflare-worker.js" <<'EOF'
// chv fixture worker — v0.2.0
// Trap: 10.200.0.1/24
EOF

    cat > "${FIXTURE}/scripts/hosting/github-pages-index.html" <<'EOF'
<span>v0.2.0</span>
<!-- Trap: 10.200.0.1/24 -->
EOF

    # Root-level copies published by hand alongside scripts/hosting/.
    cat > "${FIXTURE}/cloudflare-worker.js" <<'EOF'
// chv fixture root worker — v0.2.0
// Trap: 10.200.0.1/24
EOF

    cat > "${FIXTURE}/github-pages-index.html" <<'EOF'
<span>v0.2.0</span>
<!-- Trap: 10.200.0.1/24 -->
EOF

    cat > "${FIXTURE}/cmd/chv-fixture/Cargo.toml" <<'EOF'
[package]
name = "chv-fixture"
version = "0.2.0"
EOF

    # Decoy crate with a different version — must not be touched.
    cat > "${FIXTURE}/crates/chv-decoy/Cargo.toml" <<'EOF'
[package]
name = "chv-decoy"
version = "0.20.1"
EOF

    cat > "${FIXTURE}/ui/package.json" <<'EOF'
{
  "name": "chv-ui-fixture",
  "version": "0.2.0",
  "private": true
}
EOF

    # Minimal package-lock.json with the two "version" occurrences the real
    # lock file carries (top-level and packages."") — the sed-fallback branch
    # rewrites both.
    cat > "${FIXTURE}/ui/package-lock.json" <<'EOF'
{
  "name": "chv-ui-fixture",
  "version": "0.2.0",
  "lockfileVersion": 3,
  "packages": {
    "": {
      "name": "chv-ui-fixture",
      "version": "0.2.0",
      "private": true
    }
  }
}
EOF

    cat > "${FIXTURE}/ui/src/lib/components/shell/Sidebar.svelte" <<'EOF'
<div class="truncate">chv-v0.2.0-alpha</div>
EOF
}

# ---------------------------------------------------------------------------
# Assert the post-bump state. 0.2.0 -> 0.3.0 (minor — the same minor bump
# as the 0.3.0 release cut where the bug was observed).
# ---------------------------------------------------------------------------
assert_fixture() {
    local FIXTURE="$1"

    # VERSION (source of truth) bumped exactly.
    if [ "$(cat "${FIXTURE}/VERSION")" != "0.3.0" ]; then
        error "VERSION is '$(cat "${FIXTURE}/VERSION")', expected 0.3.0"
    fi

    # Genuine version refs bumped...
    assert_contains "${FIXTURE}/README.md" '**Version:** `0.3.0`'
    assert_contains "${FIXTURE}/README.md" 'INSTALL_CHV_VERSION=0.3.0'
    assert_contains "${FIXTURE}/README.md" 'chv-v0.3.0-alpha'
    assert_contains "${FIXTURE}/docs/DEPLOYMENT.md" 'Version: 0.3.0'
    assert_contains "${FIXTURE}/scripts/install.sh" 'INSTALL_CHV_VERSION="0.3.0"'
    assert_contains "${FIXTURE}/scripts/hosting/cloudflare-worker.js" 'v0.3.0'
    assert_contains "${FIXTURE}/scripts/hosting/github-pages-index.html" 'v0.3.0'
    assert_contains "${FIXTURE}/cloudflare-worker.js" 'v0.3.0'
    assert_contains "${FIXTURE}/github-pages-index.html" 'v0.3.0'
    assert_contains "${FIXTURE}/cmd/chv-fixture/Cargo.toml" 'version = "0.3.0"'
    assert_contains "${FIXTURE}/ui/package.json" '"version": "0.3.0"'
    assert_contains "${FIXTURE}/ui/package-lock.json" '"version": "0.3.0"'
    assert_contains "${FIXTURE}/ui/src/lib/components/shell/Sidebar.svelte" 'chv-v0.3.0-alpha'

    # ...and no old version reference survives anywhere it appeared.
    assert_not_contains "${FIXTURE}/README.md" '0.2.0'
    assert_not_contains "${FIXTURE}/docs/DEPLOYMENT.md" '0.2.0'
    assert_not_contains "${FIXTURE}/scripts/install.sh" '0.2.0'
    assert_not_contains "${FIXTURE}/cloudflare-worker.js" '0.2.0'
    assert_not_contains "${FIXTURE}/github-pages-index.html" '0.2.0'
    assert_not_contains "${FIXTURE}/cmd/chv-fixture/Cargo.toml" '0.2.0'
    assert_not_contains "${FIXTURE}/ui/package.json" '"version": "0.2.0"'
    assert_not_contains "${FIXTURE}/ui/package-lock.json" '"version": "0.2.0"'
    assert_not_contains "${FIXTURE}/ui/src/lib/components/shell/Sidebar.svelte" 'chv-v0.2.0-alpha'

    # CIDR trap lines byte-identical...
    assert_line_unchanged "${FIXTURE}/README.md" '10.200.0.1/24'
    assert_line_unchanged "${FIXTURE}/README.md" '10.0.200.1/24'
    assert_line_unchanged "${FIXTURE}/docs/DEPLOYMENT.md" '| `INSTALL_CHV_BRIDGE_CIDR` | `10.200.0.1/24` | Gateway IP / subnet |'
    assert_line_unchanged "${FIXTURE}/docs/DEPLOYMENT.md" 'BRIDGE_CIDR="10.200.0.1/24"'
    assert_line_unchanged "${FIXTURE}/scripts/install.sh" 'INSTALL_CHV_BRIDGE_CIDR="${INSTALL_CHV_BRIDGE_CIDR:-10.200.0.1/24}"'
    assert_line_unchanged "${FIXTURE}/cloudflare-worker.js" '// Trap: 10.200.0.1/24'
    assert_line_unchanged "${FIXTURE}/github-pages-index.html" '<!-- Trap: 10.200.0.1/24 -->'

    # ...including the installer's runtime default — the exact line the bug
    # corrupted in the 0.3.0 release cut — and the known corruption outputs
    # of the unescaped regex must not appear anywhere.
    assert_not_contains "${FIXTURE}/docs/DEPLOYMENT.md" '10.3.0.0.1'
    assert_not_contains "${FIXTURE}/docs/DEPLOYMENT.md" '10.0.3.0.1'
    assert_not_contains "${FIXTURE}/scripts/install.sh" '10.3.0.0.1'
    assert_not_contains "${FIXTURE}/scripts/install.sh" '10.0.3.0.1'
    assert_not_contains "${FIXTURE}/README.md" '10.3.0.0.1'

    # Decoy crate with a different version untouched (no over-matching).
    assert_contains "${FIXTURE}/crates/chv-decoy/Cargo.toml" 'version = "0.20.1"'
    assert_not_contains "${FIXTURE}/crates/chv-decoy/Cargo.toml" '0.3.0'
}

# ---------------------------------------------------------------------------
# Pass 1: ambient environment (npm branch of the UI rewrite if npm exists)
# ---------------------------------------------------------------------------
FIXTURE="$(mktemp -d)"
trap 'rm -rf "${FIXTURE}"' EXIT

make_fixture "${FIXTURE}"
bash "${FIXTURE}/scripts/bump-version.sh" minor >/dev/null
assert_fixture "${FIXTURE}"

rm -rf "${FIXTURE}"

# ---------------------------------------------------------------------------
# Pass 2: restricted PATH with no npm and no cargo — deterministically
# exercises the sed-fallback branch (the other escaped call site) and skips
# the cargo lockfile refresh.
# ---------------------------------------------------------------------------
FIXTURE="$(mktemp -d)"

make_fixture "${FIXTURE}"

FALLBACK_BIN="${FIXTURE}/fallback-bin"
mkdir -p "${FALLBACK_BIN}"
for tool in bash dirname cat find sed grep; do
    ln -s "$(command -v "${tool}")" "${FALLBACK_BIN}/${tool}"
done

env PATH="${FALLBACK_BIN}" bash "${FIXTURE}/scripts/bump-version.sh" minor >/dev/null
assert_fixture "${FIXTURE}"

rm -rf "${FIXTURE}"
trap - EXIT

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
if [ "$ERRORS" -gt 0 ]; then
    echo "Smoke bump-version test failed with ${ERRORS} error(s)." >&2
    exit 1
fi

echo "Smoke bump-version test passed."
