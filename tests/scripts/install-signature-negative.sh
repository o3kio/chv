#!/bin/bash
# Negative-path test for the installer's release signature verification (#646).
#
# The installer refuses a tarball when a release GPG key is configured and the
# detached signature is missing, invalid, or does not match the checksum
# sidecar. This test exercises the real functions from scripts/install.sh
# (sourced; main is not executed) with a temporary throwaway signing key.
#
# Usage: ./tests/scripts/install-signature-negative.sh
#
# Requires: bash, gpg, coreutils, curl (only the shell function is stubbed).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

if ! command -v gpg >/dev/null 2>&1; then
    echo "SKIP: gpg is not installed; cannot exercise signature verification."
    exit 0
fi

# scripts/install.sh intentionally fails closed on non-x86_64 hosts; the
# signature helpers are exercised only on the supported architecture.
if [ "$(uname -m)" != "x86_64" ]; then
    echo "SKIP: installer signature test runs on x86_64 only."
    exit 0
fi

WORK="$(mktemp -d)"
SIGNER_HOME="${WORK}/signer"
mkdir -m 700 "${SIGNER_HOME}"

# Source the installer functions without running main. This must happen
# BEFORE defining the test cleanup: scripts/install.sh defines its own
# `cleanup` (which writes the live database), and a trap installed earlier
# would silently be replaced by it.
# shellcheck source=/dev/null
source "${REPO_ROOT}/scripts/install.sh"

# Never let the installer's EXIT cleanup touch a real database from a test.
# shellcheck disable=SC2034  # consumed by the sourced cleanup/reset functions.
CHV_DB_PATH="${WORK}/does-not-exist.db"
reset_must_change_password() { :; }

cleanup() {
    rm -rf "${WORK}"
    if [ -n "${RELEASE_GPG_HOME:-}" ]; then
        rm -rf "${RELEASE_GPG_HOME}"
    fi
}
trap cleanup EXIT

failures=0
FAIL() {
    echo "FAIL: $*" >&2
    failures=$((failures + 1))
}

# Throwaway signing key. No passphrase: the key lives only in this temp dir.
GNUPGHOME="${SIGNER_HOME}" gpg --batch --pinentry-mode loopback --passphrase '' \
    --quick-gen-key "CHV Installer Test <installer-test@example.invalid>" ed25519 sign 0 \
    >/dev/null 2>&1
GNUPGHOME="${SIGNER_HOME}" gpg --batch --armor --export \
    > "${WORK}/release-pubkey.asc"

printf 'tarball checksum sidecar payload\n' > "${WORK}/sidecar.sha256"
GNUPGHOME="${SIGNER_HOME}" gpg --batch --yes --armor --detach-sign \
    -o "${WORK}/sidecar.sha256.sig" "${WORK}/sidecar.sha256"

# 1. Armored pubkey in CHV_RELEASE_GPG_PUBKEY: a good signature is accepted.
CHV_RELEASE_GPG_PUBKEY="$(cat "${WORK}/release-pubkey.asc")"
INSTALL_CHV_GPG_PUBKEY_FILE=""
setup_release_gpg
if ! verify_detached_signature "${WORK}/sidecar.sha256.sig" "${WORK}/sidecar.sha256"; then
    FAIL "valid detached signature was rejected"
fi

# 2. Tampered signed data is rejected.
cp "${WORK}/sidecar.sha256" "${WORK}/tampered.sha256"
printf 'tampered\n' >> "${WORK}/tampered.sha256"
if verify_detached_signature "${WORK}/sidecar.sha256.sig" "${WORK}/tampered.sha256"; then
    FAIL "tampered checksum sidecar was accepted"
fi

# 3. Missing signature is rejected.
if verify_detached_signature "${WORK}/does-not-exist.sig" "${WORK}/sidecar.sha256"; then
    FAIL "missing signature was accepted"
fi

# 4. INSTALL_CHV_GPG_PUBKEY_FILE is an equivalent key source.
rm -rf "${RELEASE_GPG_HOME}"
export CHV_RELEASE_GPG_PUBKEY=""
export INSTALL_CHV_GPG_PUBKEY_FILE="${WORK}/release-pubkey.asc"
setup_release_gpg
if ! verify_detached_signature "${WORK}/sidecar.sha256.sig" "${WORK}/sidecar.sha256"; then
    FAIL "valid signature was rejected with INSTALL_CHV_GPG_PUBKEY_FILE"
fi

# 5. With a key configured, verify_release_tarball aborts when the preferred
#    signature and the SHA256SUMS fallback are both absent (HTTP 404).
#    curl is stubbed so this stays offline and deterministic.
stub_dir="${WORK}/stub"
mkdir -p "${stub_dir}"
: > "${stub_dir}/chv-9.9.9-linux-amd64.tar.gz.sha256"
# shellcheck disable=SC2317  # these overrides are invoked indirectly by verify_release_tarball.
if (
    release_signature_configured() { return 0; }
    curl() {
        local out=""
        while [ "$#" -gt 0 ]; do
            case "$1" in
                -o) out="$2"; shift 2 ;;
                *) shift ;;
            esac
        done
        [ -z "$out" ] || : > "$out"
        printf '404'
    }
    verify_release_tarball \
        "chv-9.9.9-linux-amd64.tar.gz" \
        "https://example.invalid/releases/download/v9.9.9/chv-9.9.9-linux-amd64.tar.gz" \
        "${stub_dir}"
) >/dev/null 2>&1; then
    FAIL "missing signature did not abort the tarball verification path"
fi

# 6. A VALID signature over a sidecar whose digest does not match the tarball
#    must still abort. This is the case that proves the checksum verification
#    runs after authenticity, not instead of it.
wrong_dir="${WORK}/wrong-digest"
mkdir -p "${wrong_dir}"
tarball="chv-9.9.9-linux-amd64.tar.gz"
printf 'tarball bytes\n' > "${wrong_dir}/${tarball}"
printf '%064d  %s\n' 0 "${tarball}" > "${wrong_dir}/${tarball}.sha256"
GNUPGHOME="${SIGNER_HOME}" gpg --batch --yes --armor --detach-sign \
    -o "${wrong_dir}/${tarball}.sha256.sig" "${wrong_dir}/${tarball}.sha256"
cp "${wrong_dir}/${tarball}.sha256.sig" "${WORK}/sig-for-stub"
# shellcheck disable=SC2317  # these overrides are invoked indirectly by verify_release_tarball.
if (
    release_signature_configured() { return 0; }
    curl() {
        local out=""
        while [ "$#" -gt 0 ]; do
            case "$1" in
                -o) out="$2"; shift 2 ;;
                *) shift ;;
            esac
        done
        [ -z "$out" ] || cp "${WORK}/sig-for-stub" "$out"
        printf '200'
    }
    verify_release_tarball \
        "${tarball}" \
        "https://example.invalid/releases/download/v9.9.9/${tarball}" \
        "${wrong_dir}"
) >/dev/null 2>&1; then
    FAIL "a valid signature over a mismatched checksum sidecar was accepted"
fi

if [ "$failures" -ne 0 ]; then
    echo "install signature negative test FAILED (${failures} failure(s))." >&2
    exit 1
fi
echo "install signature negative test passed."
