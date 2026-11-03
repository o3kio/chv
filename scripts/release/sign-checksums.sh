#!/bin/bash
# Sign a checksum file with GPG or cosign if signing secrets are configured.
#
# Pipeline role: Called by the protected `sign` job in release.yml (#650).
# Reads CHV_RELEASE_GPG_KEY and CHV_RELEASE_COSIGN_KEY env vars.
# Gracefully exits 0 if no secrets are configured, unless CHV_REQUIRE_SIGNING=1.
#
# Usage:
#   ./scripts/release/sign-checksums.sh [checksums-file]
#
# release.yml calls it twice:
#   ./scripts/release/sign-checksums.sh dist/packages/SHA256SUMS
#     -> dist/packages/SHA256SUMS.sig
#   ./scripts/release/sign-checksums.sh dist/chv-<v>-linux-amd64.tar.gz.sha256
#     -> dist/chv-<v>-linux-amd64.tar.gz.sha256.sig
#
# Environment:
#   CHV_RELEASE_GPG_KEY        ASCII-armored GPG private key
#   CHV_RELEASE_GPG_PASSPHRASE GPG key passphrase
#   CHV_RELEASE_COSIGN_KEY     Cosign private key
#   CHV_RELEASE_COSIGN_PASSWORD Cosign key password
#   CHV_REQUIRE_SIGNING        "1" = missing/failed signing is fatal (fail
#                              closed); default "0" = graceful degradation
#
# If no signing secrets are configured, prints a clear TODO and exits 0
# (exit 1 when CHV_REQUIRE_SIGNING=1).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

CHECKSUMS="${1:-${REPO_ROOT}/dist/packages/SHA256SUMS}"
REQUIRE_SIGNING="${CHV_REQUIRE_SIGNING:-0}"

if [[ ! -s "$CHECKSUMS" ]]; then
    echo "Error: Checksums file not found or empty: $CHECKSUMS" >&2
    exit 1
fi

echo "Signing checksums: $CHECKSUMS"

# Called on every path that would otherwise silently produce no signature.
# With CHV_REQUIRE_SIGNING=1 this is a hard failure (used for stable releases
# once keys are configured); otherwise it documents the degraded state.
signing_unavailable() {
    local reason="$1"
    echo "Warning: $reason" >&2
    if [[ "$REQUIRE_SIGNING" == "1" ]]; then
        echo "Error: CHV_REQUIRE_SIGNING=1 — refusing to continue without a valid signature." >&2
        exit 1
    fi
    echo "Continuing without a signature. Set CHV_REQUIRE_SIGNING=1 to make this fatal." >&2
    exit 0
}

# ---------------------------------------------------------------------------
# GPG signing
# ---------------------------------------------------------------------------
if [[ -n "${CHV_RELEASE_GPG_KEY:-}" ]]; then
    echo "GPG signing key detected — signing with GPG..."

    GPG_HOME="$(mktemp -d)"
    export GNUPGHOME="$GPG_HOME"

    # Import key
    if ! echo "$CHV_RELEASE_GPG_KEY" | gpg --batch --yes --import 2>/dev/null; then
        rm -rf "$GPG_HOME"
        signing_unavailable "Failed to import GPG key"
    fi

    KEY_ID="$(gpg --list-keys --with-colons | grep '^fpr' | head -1 | cut -d: -f10)"
    if [[ -z "$KEY_ID" ]]; then
        rm -rf "$GPG_HOME"
        signing_unavailable "Could not determine GPG key ID"
    fi

    gpg_opts=(--batch --yes --detach-sign --armor)
    if [[ -n "${CHV_RELEASE_GPG_PASSPHRASE:-}" ]]; then
        gpg_opts+=(--pinentry-mode loopback --passphrase-fd 0)
        if ! printf '%s' "$CHV_RELEASE_GPG_PASSPHRASE" | gpg "${gpg_opts[@]}" \
            --local-user "$KEY_ID" \
            --output "${CHECKSUMS}.sig" \
            "$CHECKSUMS" 2>/dev/null; then
            rm -rf "$GPG_HOME"
            signing_unavailable "GPG signing failed"
        fi
    else
        if ! gpg "${gpg_opts[@]}" \
            --local-user "$KEY_ID" \
            --output "${CHECKSUMS}.sig" \
            "$CHECKSUMS" 2>/dev/null; then
            rm -rf "$GPG_HOME"
            signing_unavailable "GPG signing failed"
        fi
    fi

    if ! gpg --verify "${CHECKSUMS}.sig" "$CHECKSUMS" 2>/dev/null; then
        rm -rf "$GPG_HOME"
        rm -f "${CHECKSUMS}.sig"
        signing_unavailable "GPG signature verification failed"
    fi

    echo "GPG signature created and verified: ${CHECKSUMS}.sig"
    rm -rf "$GPG_HOME"
    exit 0
fi

# ---------------------------------------------------------------------------
# Cosign signing (Sigstore)
# ---------------------------------------------------------------------------
if [[ -n "${CHV_RELEASE_COSIGN_KEY:-}" ]]; then
    if ! command -v cosign >/dev/null 2>&1; then
        echo "Cosign key detected but cosign CLI not found — installing..."
        if ! curl -sL https://github.com/sigstore/cosign/releases/latest/download/cosign-linux-amd64 -o /tmp/cosign; then
            signing_unavailable "Failed to download cosign CLI"
        fi
        chmod +x /tmp/cosign
        COSIGN_BIN="/tmp/cosign"
    else
        COSIGN_BIN="cosign"
    fi

    echo "Cosign signing key detected — signing with cosign..."

    COSIGN_KEY_FILE="$(mktemp)"
    chmod 600 "$COSIGN_KEY_FILE"
    echo "$CHV_RELEASE_COSIGN_KEY" > "$COSIGN_KEY_FILE"
    if [[ -n "${CHV_RELEASE_COSIGN_PASSWORD:-}" ]]; then
        if ! COSIGN_PASSWORD="$CHV_RELEASE_COSIGN_PASSWORD" "$COSIGN_BIN" sign-blob \
            --key "$COSIGN_KEY_FILE" \
            --output-signature "${CHECKSUMS}.cosign.sig" \
            "$CHECKSUMS" 2>/dev/null; then
            rm -f "$COSIGN_KEY_FILE"
            signing_unavailable "Cosign signing failed"
        fi
    else
        if ! "$COSIGN_BIN" sign-blob \
            --key "$COSIGN_KEY_FILE" \
            --output-signature "${CHECKSUMS}.cosign.sig" \
            "$CHECKSUMS" 2>/dev/null; then
            rm -f "$COSIGN_KEY_FILE"
            signing_unavailable "Cosign signing failed"
        fi
    fi

    echo "Cosign signature created: ${CHECKSUMS}.cosign.sig"
    rm -f "$COSIGN_KEY_FILE"
    exit 0
fi

# ---------------------------------------------------------------------------
# No signing configured
# ---------------------------------------------------------------------------
echo ""
echo "============================================================"
echo "  SIGNING NOT CONFIGURED"
echo "============================================================"
echo ""
echo "The checksum file was generated but NOT signed: $CHECKSUMS"
echo ""
echo "To enable signing, configure one of these secret sets:"
echo ""
echo "  GPG signing:"
echo "    CHV_RELEASE_GPG_KEY        ASCII-armored private key"
echo "    CHV_RELEASE_GPG_PASSPHRASE (optional) key passphrase"
echo ""
echo "  Cosign (Sigstore) signing:"
echo "    CHV_RELEASE_COSIGN_KEY     Cosign private key"
echo "    CHV_RELEASE_COSIGN_PASSWORD (optional) key password"
echo ""
echo "Set the repository variable CHV_REQUIRE_SIGNING=1 to make an unsigned"
echo "release fail instead of degrading gracefully."
echo ""
echo "Users can still verify integrity using SHA256 checksums."
echo ""

signing_unavailable "No signing keys configured"
