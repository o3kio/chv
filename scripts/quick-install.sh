#!/bin/bash
# CHV Quick Install — the one-command lab installer (issue #482)
#
# Usage (designed to be curl-pipe-bash'd from raw.githubusercontent.com):
#   curl -fsSL https://raw.githubusercontent.com/o3kio/chv/main/scripts/quick-install.sh | sudo bash -s
#   curl -fsSL ... | sudo bash -s -- --version 0.3.0     # pin a release
#   sudo ./scripts/quick-install.sh --uninstall          # remove software, keep data
#   sudo ./scripts/quick-install.sh --uninstall --purge  # remove everything
#   ./scripts/quick-install.sh --dry-run                 # resolve+verify assets only
#
# What this is:
#   A single command that turns a fresh Linux host into a working CHV lab:
#   control plane with the WebUI served by the binary itself (#447 D3 shape
#   — [webui] enabled, NO nginx), plus the local agent enrolled against it
#   (chv-agent + chv-stord + chv-nwd), systemd units, and config under
#   /etc/chv.
#
# What this is NOT:
#   The qualified deployment path. scripts/install.sh stays the production
#   installer (nginx edge, m4.x qualification legs). This script shares its
#   conventions (user/group names, /etc/chv layout, systemd unit shapes,
#   chown discipline) but rides the #447 package-serving shape instead.
#
#   Deployment status: [CODE-SUPPORTED, UNQUALIFIED] — no qualification leg
#   exercises this path yet (same tier the #447 package serving shape
#   started at). See docs/DEPLOYMENT.md "Quick Install (single command)".
#
# Artifact source: tagged GitHub releases of o3kio/chv only (the publish
# pipeline .github/workflows/release.yml maintains). No nightly default.
# Asset naming matches what that pipeline actually publishes:
#   - tarball: chv-<version>-linux-<arch>.tar.gz (+ .sha256 sidecar)
#   - deb:     chv-controlplane_<version>_amd64.deb, chv-node_..., chvctl_...
#   - rpm:     chv-controlplane-<version>-1.x86_64.rpm, chv-node_..., chvctl_...
#   - SHA256SUMS covering the .deb/.rpm set
# RC releases map <X.Y.Z>-rc.N to package version <X.Y.Z>~rc.N (scripts/version.sh).
#
# Platform support: Linux amd64 + arm64. NOTE: the release pipeline
# currently publishes linux-amd64 assets only; on an arm64 host this script
# resolves the arm64 asset names and fails closed with an explanation if the
# release does not carry them (it does not today). The script is arm64-ready
# the day the pipeline publishes arm64 assets.
#
# Environment variables (house names shared with scripts/install.sh where the
# semantics are identical):
#   INSTALL_CHV_VERSION       - version to install, no leading 'v' (default: latest stable)
#   GITHUB_REPO               - override the source repository (default: o3kio/chv)
#   INSTALL_CHV_SKIP_DEPS     - "1" to skip dependency installation
#   INSTALL_CHV_SKIP_CLOUD_HV - "1" to skip the Cloud Hypervisor download
#   INSTALL_CHV_ALLOW_VMM_MISMATCH - "1" to keep a host cloud-hypervisor /
#                                ch-remote whose major.minor differs from the
#                                qualified pin (v53.0) instead of replacing it
#                                with the pinned, digest-verified static
#                                binaries. OFF-PIN and UNQUALIFIED.
#
# Disclosed judgments (full list in docs/DEPLOYMENT.md and the PR body):
#   - Packages (.deb/.rpm) are preferred when the distro family matches; the
#     release tarball is the fallback for any other Linux. Both legs produce
#     the same on-disk layout and the same generated config.
#   - http_bind is 0.0.0.0:8080 (not install.sh's 127.0.0.1): with no nginx
#     edge, the binary's own listener is the only front door. The WebUI static
#     assets are unauthenticated by design (login page); every API route
#     keeps its auth. There is no TLS on this path — do not expose it to
#     untrusted networks.
#   - The serial console (/ws/) does NOT work on this path: the control plane
#     binary does not proxy /ws/ (the #447 decision, maintainer-ratified
#     2026-10-07) and this path installs no edge. VM lifecycle, not console.
#   - Idempotency: a re-run upgrades artifacts in place. Existing config
#     under /etc/chv, certs, the database, and the bootstrap/admin secrets
#     are never overwritten. A pre-existing install.sh (qualified) layout is
#     refused unless --force.
#   - Uninstall: --uninstall removes software and preserves /etc/chv +
#     /var/lib/chv; --uninstall --purge also removes data and config.

set -euo pipefail

# -----------------------------------------------------------------------------
# Configuration (paths are env-overridable so the sandbox tests can redirect
# them; on a real host the defaults match install.sh and the package layout)
# -----------------------------------------------------------------------------
INSTALL_CHV_VERSION="${INSTALL_CHV_VERSION:-latest}"
GITHUB_REPO="${GITHUB_REPO:-o3kio/chv}"
INSTALL_CHV_SKIP_DEPS="${INSTALL_CHV_SKIP_DEPS:-0}"
INSTALL_CHV_SKIP_CLOUD_HV="${INSTALL_CHV_SKIP_CLOUD_HV:-0}"

CHV_USER="chv"
CHV_CONFIG_DIR="${CHV_CONFIG_DIR:-/etc/chv}"
CHV_DATA_DIR="${CHV_DATA_DIR:-/var/lib/chv}"
CHV_LOG_DIR="${CHV_LOG_DIR:-/var/log/chv}"
CHV_RUN_DIR="${CHV_RUN_DIR:-/run/chv}"
# Package layout (what the .deb/.rpm ships and what the packaged systemd
# units expect: ReadOnlyPaths=... /usr/share/chv). install.sh uses
# /opt/chv/ui + /usr/local/share/chv/migrations for its own tree; the quick
# path uses the package layout in BOTH legs so one config shape serves both.
CHV_UI_DIR="${CHV_UI_DIR:-/usr/share/chv/ui}"
CHV_MIGRATIONS_DIR="${CHV_MIGRATIONS_DIR:-/usr/share/chv/migrations}"
CHV_DB_PATH="${CHV_DB_PATH:-${CHV_DATA_DIR}/controlplane.db}"

# CLI flags (parsed in main)
ARG_VERSION=""
ARG_UNINSTALL="0"
ARG_PURGE="0"
ARG_FORCE="0"
ARG_DRYRUN="0"
ARG_TARBALL="0"

# Populated during the run
CHV_ARCH=""
PKG_KIND=""          # deb | rpm | tarball
VERSION=""           # resolved version, no leading 'v' (e.g. 0.3.0, 0.4.0-rc.1)
PKG_VERSION=""       # package version (RC maps -rc.N -> ~rc.N)
WORK_DIR=""
FRESH_INSTALL="1"
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

cleanup() {
    if [ -n "${WORK_DIR:-}" ] && [ -d "$WORK_DIR" ]; then
        rm -rf "$WORK_DIR"
    fi
}

usage() {
    cat <<'USAGE'
CHV Quick Install — one-command lab installer (issue #482)

Usage:
  curl -fsSL https://raw.githubusercontent.com/o3kio/chv/main/scripts/quick-install.sh | sudo bash -s
  curl -fsSL <url> | sudo bash -s -- --version 0.3.0   # pin a tagged release
  sudo ./scripts/quick-install.sh --uninstall           # remove software, keep data
  sudo ./scripts/quick-install.sh --uninstall --purge   # remove everything
  ./scripts/quick-install.sh --dry-run                  # resolve + verify assets only

Flags:
  --version <v>   install a specific tagged release (no leading 'v'; RCs OK)
  --tarball       force the tarball artifact leg (skip .deb/.rpm detection)
  --dry-run       resolve the release, download and verify checksums, change nothing
  --uninstall     stop and remove CHV software (preserves /etc/chv and /var/lib/chv)
  --purge         with --uninstall: also remove data and configuration
  --force         proceed even when a qualified install.sh deployment is detected
  -h, --help      show this help

Environment:
  INSTALL_CHV_VERSION        version to install (default: latest stable)
  GITHUB_REPO                source repository (default: o3kio/chv)
  INSTALL_CHV_SKIP_DEPS      "1" to skip dependency installation
  INSTALL_CHV_SKIP_CLOUD_HV  "1" to skip the Cloud Hypervisor download
  INSTALL_CHV_ALLOW_VMM_MISMATCH
                             "1" to keep an off-pin host cloud-hypervisor /
                             ch-remote (OFF-PIN and UNQUALIFIED)

Deployment status: [CODE-SUPPORTED, UNQUALIFIED] — the qualified path is
scripts/install.sh. See docs/DEPLOYMENT.md "Quick Install (single command)".
USAGE
}

# -----------------------------------------------------------------------------
# Platform detection
# -----------------------------------------------------------------------------
detect_arch() {
    local arch
    arch=$(uname -m)
    case "$arch" in
        x86_64)
            CHV_ARCH="amd64"
            ;;
        aarch64|arm64)
            CHV_ARCH="arm64"
            ;;
        *)
            fatal "Unsupported architecture: ${arch}. Only x86_64 (amd64) and aarch64 (arm64) are supported."
            ;;
    esac
    info "Detected architecture: ${arch} (asset suffix: ${CHV_ARCH})"
}

# Decide the artifact leg: .deb on Debian-family, .rpm on RH-family, tarball
# otherwise. Judgment, disclosed: packages are preferred where the distro
# matches because the package manager owns upgrades and dependency
# resolution (libssl3/openssl-libs) and the postinst creates the users and
# directories with the same conventions this script would; the tarball is
# the universal fallback. The tradeoff vs always-tarball is two artifact
# legs — bounded here by having both legs converge on the identical
# post-install flow (same layout, same generated config, same units).
detect_distro() {
    local id="" id_like=""
    if [ -r /etc/os-release ]; then
        # shellcheck disable=SC1091
        . /etc/os-release
        id="${ID:-}"
        id_like="${ID_LIKE:-}"
    fi

    if [ "$ARG_TARBALL" = "1" ]; then
        PKG_KIND="tarball"
        info "Forcing tarball artifact leg (--tarball)."
        return
    fi

    case " ${id} ${id_like} " in
        *" debian "*|*" ubuntu "*)
            if cmd_exists apt-get && cmd_exists dpkg; then
                PKG_KIND="deb"
                return
            fi
            ;;
        *" rhel "*|*" fedora "*|*" centos "*|*" rocky "*|*" almalinux "*|*" amzn "*)
            if cmd_exists dnf || cmd_exists yum; then
                if cmd_exists rpm; then
                    PKG_KIND="rpm"
                    return
                fi
            fi
            ;;
    esac

    PKG_KIND="tarball"
    warn "Distro '${id:-unknown}' has no matching package format (or the package"
    warn "manager is missing) — falling back to the release tarball."
}

# -----------------------------------------------------------------------------
# Release resolution (GitHub API; no jq dependency — grep/sed parsing like
# scripts/install.sh)
# -----------------------------------------------------------------------------
github_api_get() {
    # $1: API path (e.g. releases/latest). Prints body; non-200 is fatal.
    local path="$1" body code
    body=$(mktemp)
    # -L: follow the redirect left behind by a repository/org rename (e.g.
    # kubedoio/chv -> o3kio/chv returns 301 with an HTML/JSON body, which
    # would otherwise be parsed as a failed API response).
    code=$(curl -sSL -o "$body" -w '%{http_code}' \
        -H 'Accept: application/vnd.github+json' \
        "https://api.github.com/repos/${GITHUB_REPO}/${path}" 2>/dev/null) || code="000"
    if [ "$code" != "200" ]; then
        rm -f "$body"
        if [ "$code" = "404" ]; then
            fatal "GitHub API returned 404 for ${path}.
No release matches in ${GITHUB_REPO}. The quick installer installs tagged,
versioned releases only — the 'nightly' pre-release channel is deliberately
not supported. Check available releases at:
  https://github.com/${GITHUB_REPO}/releases"
        fi
        fatal "GitHub API request failed (HTTP ${code}) for ${path}."
    fi
    cat "$body"
    rm -f "$body"
}

resolve_release() {
    local pin="${ARG_VERSION:-${INSTALL_CHV_VERSION}}"
    pin="${pin#v}"

    if [ "$pin" = "nightly" ]; then
        fatal "Refusing to install the 'nightly' pre-release channel.
The quick installer installs tagged, versioned releases only (no nightly
default, no nightly pin). Use a pinned stable/RC version, e.g. --version 0.3.0."
    fi

    local api_path release_json
    if [ "$pin" = "latest" ]; then
        api_path="releases/latest"
    else
        api_path="releases/tags/v${pin}"
    fi

    info "Resolving release (${api_path}) from ${GITHUB_REPO}..."
    release_json=$(github_api_get "$api_path")

    # Tag name without the leading 'v' (e.g. v0.3.0 -> 0.3.0).
    VERSION=$(printf '%s' "$release_json" | grep '"tag_name":' | head -n1 \
        | sed -E 's/.*"tag_name": *"v?([^"]+)".*/\1/')
    if [ -z "$VERSION" ]; then
        fatal "Could not parse tag_name from the GitHub API response."
    fi

    # Package version: RC releases map <X.Y.Z>-rc.N to <X.Y.Z>~rc.N
    # (scripts/version.sh: '~' sorts below the stable release on both
    # Debian and RPM). Everything else is used verbatim.
    if [[ "$VERSION" =~ ^([0-9]+\.[0-9]+\.[0-9]+)-rc\.[0-9]+$ ]]; then
        PKG_VERSION="${BASH_REMATCH[1]}~rc.${VERSION##*-rc.}"
    else
        PKG_VERSION="$VERSION"
    fi

    # Asset names actually published on this release, taken from the
    # browser_download_url values (one per asset; the filename is the last
    # path segment). This is the reality check: we never assume an asset
    # exists, we verify it against this list (arm64 today fails here).
    RELEASE_ASSETS=$(printf '%s' "$release_json" | grep -o '"browser_download_url": *"[^"]*"' \
        | sed 's|.*/||; s/"$//')

    info "Resolved release: v${VERSION} (package version: ${PKG_VERSION})"
}

asset_exists() {
    # $1: exact asset filename. RELEASE_ASSETS is newline-separated names.
    printf '%s\n' "$RELEASE_ASSETS" | grep -qxF "$1"
}

require_asset() {
    local name="$1"
    if ! asset_exists "$name"; then
        if [ "$CHV_ARCH" = "arm64" ]; then
            fatal "Release v${VERSION} does not carry the arm64 asset '${name}'.
The current release pipeline (.github/workflows/release.yml) builds
linux-amd64 assets only. arm64 hosts cannot be quick-installed until the
pipeline publishes arm64 assets — this is a pipeline gap, not a host
problem. Track: https://github.com/${GITHUB_REPO}/issues"
        fi
        fatal "Release v${VERSION} does not carry the expected asset '${name}'.
This quick installer's asset naming no longer matches what the release
pipeline publishes — refusing rather than guessing. See:
  https://github.com/${GITHUB_REPO}/releases/tag/v${VERSION}"
    fi
}

# Names of the artifacts this host needs, per the detected leg.
expected_assets() {
    case "$PKG_KIND" in
        deb)
            printf '%s\n' \
                "chv-controlplane_${PKG_VERSION}_amd64.deb" \
                "chv-node_${PKG_VERSION}_amd64.deb" \
                "chvctl_${PKG_VERSION}_amd64.deb" \
                "SHA256SUMS"
            ;;
        rpm)
            printf '%s\n' \
                "chv-controlplane-${PKG_VERSION}-1.x86_64.rpm" \
                "chv-node-${PKG_VERSION}-1.x86_64.rpm" \
                "chvctl-${PKG_VERSION}-1.x86_64.rpm" \
                "SHA256SUMS"
            ;;
        tarball)
            printf '%s\n' \
                "chv-${VERSION}-linux-${CHV_ARCH}.tar.gz" \
                "chv-${VERSION}-linux-${CHV_ARCH}.tar.gz.sha256"
            ;;
    esac
}

download_and_verify() {
    WORK_DIR=$(mktemp -d /tmp/chv-quick-install.XXXXXX)

    local assets name
    assets=$(expected_assets)
    while IFS= read -r name; do
        require_asset "$name"
    done <<< "$assets"

    info "Downloading release artifacts to ${WORK_DIR}..."
    while IFS= read -r name; do
        local url="https://github.com/${GITHUB_REPO}/releases/download/v${VERSION}/${name}"
        if ! curl -fsSL --retry 3 --retry-delay 5 "$url" -o "${WORK_DIR}/${name}"; then
            fatal "Failed to download ${url}"
        fi
    done <<< "$assets"

    # Checksum verification, fail closed on any mismatch or missing entry.
    # - packages: the release's SHA256SUMS (covers the .deb/.rpm set; note it
    #   also covers stray 0.0.1 lifecycle-test packages the release job
    #   sweeps up — we match EXACT filenames, never a glob)
    # - tarball: the .tar.gz.sha256 sidecar
    # The SHA256SUMS file is GPG/cosign-signed by the release pipeline when
    # signing keys are configured, but no signature asset is published on
    # current releases; this path verifies checksums over GitHub TLS only
    # (disclosed residual — signature verification stays with the operator).
    info "Verifying checksums..."
    case "$PKG_KIND" in
        deb|rpm)
            # Exact per-name field matching (awk '$2 == n'), never a regex:
            # '.' in a pattern matches any character, and the release's
            # SHA256SUMS also lists stray 0.0.1 lifecycle-test packages
            # whose names differ from ours only in digits — matching must
            # be unambiguous. Fail-closed: exactly three entries required.
            local name count
            : > "${WORK_DIR}/local-SHA256SUMS"
            while IFS= read -r name; do
                [ "$name" = "SHA256SUMS" ] && continue
                awk -v n="$name" '$2 == n' "${WORK_DIR}/SHA256SUMS" \
                    >> "${WORK_DIR}/local-SHA256SUMS"
            done < <(expected_assets)
            count=$(grep -c . "${WORK_DIR}/local-SHA256SUMS" || true)
            if [ "$count" -ne 3 ]; then
                fatal "SHA256SUMS on release v${VERSION} does not list exactly the three expected packages (found ${count} matching entries). Refusing to install unverified packages."
            fi
            if ! (cd "$WORK_DIR" && sha256sum -c --strict local-SHA256SUMS); then
                fatal "Checksum mismatch against the release's SHA256SUMS — aborting."
            fi
            ;;
        tarball)
            local tb="chv-${VERSION}-linux-${CHV_ARCH}.tar.gz"
            if ! (cd "$WORK_DIR" && sha256sum -c --strict "${tb}.sha256"); then
                fatal "Checksum mismatch against the tarball's .sha256 sidecar — aborting."
            fi
            ;;
    esac
    info "Checksums verified."
}

# -----------------------------------------------------------------------------
# Existing-install guards (idempotency stance)
# -----------------------------------------------------------------------------
check_existing_install() {
    # Freshness rule, disclosed:
    #  - no /etc/chv/controlplane.toml, no database -> fresh install
    #  - config carries the quick-install marker   -> upgrade (config preserved)
    #  - config exists WITHOUT the marker and a control-plane database is
    #    present                                  -> a deployment this script
    #    did not configure (packages deployed by hand, or install.sh) —
    #    refuse unless --force
    #  - config absent but a database IS present  -> a partial or hand-managed
    #    state — refuse unless --force (fresh config would mint a new
    #    jwt_secret against a live database, invalidating existing sessions
    #    and agent tokens)
    #  - config exists WITHOUT the marker and NO database (e.g. the example
    #    conffile the .deb/.rpm ships with a placeholder jwt_secret) —
    #    take over and generate real config (the conffile is replaced
    #    deliberately; dpkg/rpm keep our version on later package upgrades)
    local cp_conf="${CHV_CONFIG_DIR}/controlplane.toml"

    if [ -f "$cp_conf" ] && grep -q 'Generated by scripts/quick-install.sh' "$cp_conf"; then
        FRESH_INSTALL="0"
        info "Existing quick-install configuration found — upgrading in place"
        info "(config, certs, secrets, and data under ${CHV_DATA_DIR} are preserved)."
    elif [ -f "$cp_conf" ] && [ -f "${CHV_DB_PATH}" ]; then
        if [ "$ARG_FORCE" != "1" ]; then
            fatal "An existing CHV deployment is present (${cp_conf} plus ${CHV_DB_PATH})
that this script did not configure (deployed via packages or install.sh).
Refusing to overwrite its configuration. To manage this host use the path
that configured it (scripts/install.sh or the package workflow), or re-run
with --force to keep the existing config and upgrade the software in place."
        fi
        FRESH_INSTALL="0"
        warn "--force: preserving the existing configuration and upgrading software only."
    elif [ ! -f "$cp_conf" ] && [ -f "${CHV_DB_PATH}" ]; then
        if [ "$ARG_FORCE" != "1" ]; then
            fatal "A control-plane database exists (${CHV_DB_PATH}) without a ${cp_conf} —
a partial or hand-managed deployment. Generating fresh configuration would
mint a new jwt_secret against that live database and invalidate existing
sessions and agent tokens. Re-run with --force to take the deployment over
(fresh config will be generated against the existing database)."
        fi
        warn "--force: generating fresh config against an existing database"
        warn "(a new jwt_secret invalidates existing sessions and agent tokens)."
    elif [ -f "$cp_conf" ]; then
        warn "Replacing an unconfigured ${cp_conf}"
        warn "(no database present — this is the example conffile the package ships,"
        warn "or a hand-written config that was never deployed)."
    fi

    # Refuse a mixed-layout hazard: an install.sh (qualified) deployment uses
    # /opt/chv/ui and /usr/local/share/chv/migrations and an nginx edge.
    # Re-pointing it at the package layout silently would strand its config.
    local layout_conflict=""
    if [ -d /opt/chv/ui ] && [ -n "$(ls -A /opt/chv/ui 2>/dev/null)" ]; then
        layout_conflict="/opt/chv/ui is populated (install.sh layout)"
    elif [ -L /etc/nginx/sites-enabled/chv ] || [ -f /etc/nginx/sites-enabled/chv ]; then
        layout_conflict="an nginx chv site is enabled (install.sh edge)"
    fi

    if [ -n "$layout_conflict" ]; then
        if [ "$ARG_FORCE" != "1" ]; then
            fatal "A qualified scripts/install.sh deployment appears to be present (${layout_conflict}).
The quick installer uses the package layout (/usr/share/chv/ui, no nginx) and
will not silently convert a qualified install. To manage this host, use
scripts/install.sh (the qualified path). To proceed anyway, re-run with --force
(you are taking over the deployment; the old nginx edge and /opt/chv tree are
left in place but unused)."
        fi
        warn "--force given: proceeding on top of an existing install.sh"
        warn "layout (${layout_conflict}). The nginx edge and /opt/chv tree are not removed."
    fi
}

# -----------------------------------------------------------------------------
# Dependencies
# -----------------------------------------------------------------------------
install_dependencies() {
    if [ "$INSTALL_CHV_SKIP_DEPS" = "1" ]; then
        info "Skipping dependency installation (INSTALL_CHV_SKIP_DEPS=1)"
        return
    fi

    # Needed by this script itself: curl (already used), openssl (certs,
    # secrets), sqlite3 (admin seeding, token fallback, enrollment check),
    # and a bcrypt provider for the admin password (apache2-utils /
    # httpd-tools ship htpasswd).
    case "$PKG_KIND" in
        deb)
            info "Installing dependencies (apt)..."
            export DEBIAN_FRONTEND=noninteractive
            apt-get update -qq
            apt-get install -y -qq curl openssl sqlite3 apache2-utils
            ;;
        rpm)
            info "Installing dependencies (dnf/yum)..."
            if cmd_exists dnf; then
                dnf install -y curl openssl sqlite httpd-tools
            else
                yum install -y curl openssl sqlite httpd-tools
            fi
            ;;
        tarball)
            # Unknown distro: no package manager to lean on. Verify the
            # hard requirements and fail closed with guidance if missing.
            local missing=""
            for bin in curl openssl sqlite3 sha256sum tar systemctl; do
                if ! cmd_exists "$bin"; then
                    missing="${missing} ${bin}"
                fi
            done
            if [ -n "$missing" ]; then
                fatal "Required commands not found on this host:${missing}.
The tarball fallback has no package manager to install them with. Install
them with your distro's tools and re-run (or set INSTALL_CHV_SKIP_DEPS=1
once they are present)."
            fi
            info "Tarball fallback: all required commands present."
            ;;
    esac
}

# -----------------------------------------------------------------------------
# Artifact installation
# -----------------------------------------------------------------------------
install_artifacts() {
    case "$PKG_KIND" in
        deb)
            info "Installing .deb packages (dpkg-owned upgrades)..."
            if ! (cd "$WORK_DIR" && apt-get install -y --allow-downgrades \
                    "./chv-controlplane_${PKG_VERSION}_amd64.deb" \
                    "./chv-node_${PKG_VERSION}_amd64.deb" \
                    "./chvctl_${PKG_VERSION}_amd64.deb"); then
                # No usable apt (offline mirror, broken lists): plain dpkg.
                # Dependency resolution (libssl3) is then the host's job;
                # dpkg fails loudly if it is missing.
                warn "apt-get local install failed; falling back to dpkg -i."
                (cd "$WORK_DIR" && dpkg -i \
                    "chv-controlplane_${PKG_VERSION}_amd64.deb" \
                    "chv-node_${PKG_VERSION}_amd64.deb" \
                    "chvctl_${PKG_VERSION}_amd64.deb")
            fi
            ;;
        rpm)
            info "Installing .rpm packages..."
            (cd "$WORK_DIR"
             if cmd_exists dnf; then
                 dnf install -y \
                    "./chv-controlplane-${PKG_VERSION}-1.x86_64.rpm" \
                    "./chv-node-${PKG_VERSION}-1.x86_64.rpm" \
                    "./chvctl-${PKG_VERSION}-1.x86_64.rpm"
             elif cmd_exists yum; then
                 yum install -y \
                    "./chv-controlplane-${PKG_VERSION}-1.x86_64.rpm" \
                    "./chv-node-${PKG_VERSION}-1.x86_64.rpm" \
                    "./chvctl-${PKG_VERSION}-1.x86_64.rpm"
             else
                 rpm -Uvh \
                    "chv-controlplane-${PKG_VERSION}-1.x86_64.rpm" \
                    "chv-node-${PKG_VERSION}-1.x86_64.rpm" \
                    "chvctl-${PKG_VERSION}-1.x86_64.rpm"
             fi)
            ;;
        tarball)
            install_artifacts_tarball
            ;;
    esac
}

# Tarball leg: reproduce the .deb's on-disk contract by hand (same files,
# same paths, same ownership as the package + its postinst). The packaged
# systemd units are installed verbatim from the tarball — the single source
# of truth install.sh also uses (packaging/systemd/, mirrored into the
# release tarball's systemd/).
install_artifacts_tarball() {
    info "Installing from release tarball..."
    local extract_dir="${WORK_DIR}/chv-${VERSION}-linux-${CHV_ARCH}"
    tar -xzf "${WORK_DIR}/chv-${VERSION}-linux-${CHV_ARCH}.tar.gz" -C "$WORK_DIR"

    for f in chv-controlplane chv-agent chv-stord chv-nwd chvctl; do
        if [ ! -f "${extract_dir}/bin/${f}" ]; then
            fatal "Tarball is missing bin/${f} — refusing to half-install."
        fi
    done
    for f in chv-controlplane chv-agent chv-stord chv-nwd; do
        if [ ! -f "${extract_dir}/systemd/${f}.service" ]; then
            fatal "Tarball is missing systemd/${f}.service — refusing to half-install."
        fi
    done
    if [ ! -f "${extract_dir}/tmpfiles/chv-node.conf" ]; then
        fatal "Tarball is missing tmpfiles/chv-node.conf — the chv-nwd unit cannot start without /run/netns."
    fi

    # Users, groups, and directories — mirrors packaging/scripts/postinstall.sh
    # (the shape-independent conventions install.sh shares too).
    info "Setting up users and directories (tarball leg)..."
    if ! getent group "$CHV_USER" >/dev/null 2>&1; then groupadd -r "$CHV_USER"; fi
    if ! getent passwd "$CHV_USER" >/dev/null 2>&1; then
        useradd -r -g "$CHV_USER" -d "$CHV_DATA_DIR" -s /usr/sbin/nologin "$CHV_USER"
    fi
    if ! getent group chv-stord >/dev/null 2>&1; then groupadd -r chv-stord; fi
    if ! getent passwd chv-stord >/dev/null 2>&1; then
        useradd -r -g chv-stord -d "$CHV_DATA_DIR" -s /usr/sbin/nologin chv-stord
    fi
    if getent group kvm >/dev/null 2>&1; then
        if ! id -nG "$CHV_USER" | tr ' ' '\n' | grep -qx kvm; then
            usermod -aG kvm "$CHV_USER"
        fi
    fi
    if getent group disk >/dev/null 2>&1; then
        if ! id -nG chv-stord | tr ' ' '\n' | grep -qx disk; then
            usermod -aG disk chv-stord
        fi
    fi
    if ! id -nG chv-stord | tr ' ' '\n' | grep -qx "$CHV_USER"; then
        usermod -aG "$CHV_USER" chv-stord
    fi
    if ! id -nG "$CHV_USER" | tr ' ' '\n' | grep -qx chv-stord; then
        usermod -aG chv-stord "$CHV_USER"
    fi

    mkdir -p "$CHV_DATA_DIR" "$CHV_LOG_DIR" "$CHV_RUN_DIR"
    chown "$CHV_USER:$CHV_USER" "$CHV_DATA_DIR" "$CHV_LOG_DIR" "$CHV_RUN_DIR" || true
    install -d -m 0700 -o "$CHV_USER" -g "$CHV_USER" \
        "${CHV_DATA_DIR}/agent" "${CHV_DATA_DIR}/cache" "${CHV_RUN_DIR}/core"
    install -d -m 0775 -o "$CHV_USER" -g "$CHV_USER" "${CHV_RUN_DIR}/agent"
    install -d -m 0770 -o "$CHV_USER" -g chv-stord \
        "${CHV_DATA_DIR}/storage/localdisk" "${CHV_DATA_DIR}/storage/lvm"
    install -d -m 0755 -o "$CHV_USER" -g "$CHV_USER" "${CHV_RUN_DIR}/stord"

    info "Installing binaries, Web UI assets, and migrations..."
    install -m 0755 "${extract_dir}/bin/chv-controlplane" /usr/bin/
    install -m 0755 "${extract_dir}/bin/chv-agent" /usr/bin/
    install -m 0755 "${extract_dir}/bin/chv-stord" /usr/bin/
    install -m 0755 "${extract_dir}/bin/chv-nwd" /usr/bin/
    install -m 0755 "${extract_dir}/bin/chvctl" /usr/bin/

    rm -rf "${CHV_UI_DIR:?}"
    mkdir -p "$CHV_UI_DIR"
    cp -r "${extract_dir}/ui/"* "$CHV_UI_DIR/"
    chown -R "$CHV_USER:$CHV_USER" "$CHV_UI_DIR"

    mkdir -p "$CHV_MIGRATIONS_DIR"
    cp -r "${extract_dir}/migrations/"* "$CHV_MIGRATIONS_DIR/"
    chown -R "$CHV_USER:$CHV_USER" "$CHV_MIGRATIONS_DIR"

    info "Installing systemd units (packaged units, verbatim)..."
    local unit
    for unit in chv-controlplane chv-agent chv-stord chv-nwd; do
        install -m 0644 "${extract_dir}/systemd/${unit}.service" /etc/systemd/system/
    done
    install -m 0644 "${extract_dir}/tmpfiles/chv-node.conf" /usr/lib/tmpfiles.d/chv-node.conf
    if ! systemd-tmpfiles --create /usr/lib/tmpfiles.d/chv-node.conf >/dev/null 2>&1; then
        fatal "systemd-tmpfiles --create failed for chv-node.conf; the chv-nwd unit cannot start without /run/netns."
    fi
    systemctl daemon-reload
}

# -----------------------------------------------------------------------------
# Cloud Hypervisor (shape-independent from install.sh: qualified-pin digests,
# D6 option (b), #448 campaign — docs/evidence/vmm-requalification/v53.0/)
# -----------------------------------------------------------------------------
install_cloud_hypervisor() {
    if [ "$INSTALL_CHV_SKIP_CLOUD_HV" = "1" ]; then
        info "Skipping Cloud Hypervisor installation (INSTALL_CHV_SKIP_CLOUD_HV=1)"
        return
    fi
    if [ "$CHV_ARCH" != "amd64" ]; then
        # Unreachable today (arm64 aborts at asset resolution), kept for the
        # day the pipeline publishes arm64 assets: the pinned static
        # artifacts below are the x86_64 ones.
        warn "Cloud Hypervisor install is only wired for amd64; skipping (VMs will not run until a VMM is installed)."
        return
    fi

    local chv_version="53.0"
    local chv_sha256="448af3d4e59b22c2987f7df94c213ad40fb53a10d437e42b5ee6c4fce7c29ecc"
    local chv_remote_sha256="13f32ba952e6791fd901f2279be2055fbacc64005f96c42a8e90d58860df84a7"

    # Version gate, same policy as scripts/install.sh: a host binary is
    # accepted only when its major.minor matches the qualified pin. Anything
    # else is replaced with the pinned, digest-verified static build — the
    # old "any version counts as installed" early return silently kept hosts
    # on the v43 branch the qualification campaign moved away from.
    local chv_pin_ok=0
    # Gate the binary the agent will actually exec (`chv_binary_path` is
    # /usr/bin/cloud-hypervisor), falling back to PATH only until that link
    # exists. Checking PATH alone would accept a pinned /usr/local/bin copy
    # while leaving a stale /usr/bin binary for the agent to run.
    local chv_bin=""
    if [ -x /usr/bin/cloud-hypervisor ]; then
        chv_bin="/usr/bin/cloud-hypervisor"
    elif cmd_exists cloud-hypervisor; then
        chv_bin="$(command -v cloud-hypervisor)"
        # A relative PATH entry makes `command -v` return a relative path;
        # normalize it so the /usr/bin link never depends on the installer's
        # working directory.
        case "$chv_bin" in
            /*) ;;
            *) chv_bin="$(cd "$(dirname "$chv_bin")" && pwd)/$(basename "$chv_bin")" ;;
        esac
    fi
    if [ -n "$chv_bin" ]; then
        local found_raw found_mm
        found_raw="$("$chv_bin" --version 2>/dev/null || true)"
        # Parse major.minor from e.g. "cloud-hypervisor v43.0.0".
        found_mm="$(printf '%s' "$found_raw" | grep -oE '[0-9]+\.[0-9]+' | head -n1 || true)"

        if [ "$found_mm" = "$chv_version" ]; then
            info "Cloud Hypervisor already installed at the qualified pin (${found_raw}); keeping it."
            if [ "$chv_bin" != "/usr/bin/cloud-hypervisor" ]; then
                ln -sf "$chv_bin" /usr/bin/cloud-hypervisor
            fi
            chv_pin_ok=1
        elif [ "${INSTALL_CHV_ALLOW_VMM_MISMATCH:-0}" = "1" ]; then
            warn "INSTALL_CHV_ALLOW_VMM_MISMATCH=1: keeping the host Cloud Hypervisor."
            warn "  found:    ${found_raw:-unparseable}"
            warn "  required: v${chv_version} (qualified pin)"
            warn "This install is OFF-PIN and UNQUALIFIED: VMs will run on a VMM the"
            warn "qualification campaign did not cover. Re-run without this override"
            warn "(and without INSTALL_CHV_SKIP_CLOUD_HV) to install the pinned binary."
            if [ "$chv_bin" != "/usr/bin/cloud-hypervisor" ]; then
                ln -sf "$chv_bin" /usr/bin/cloud-hypervisor
            fi
            chv_pin_ok=1
        else
            warn "Cloud Hypervisor version mismatch: found ${found_raw:-unparseable},"
            warn "but the qualified pin is v${chv_version}. Replacing the host binary"
            warn "with the pinned, digest-verified static build."
            warn "To keep the host binary instead (off-pin, unqualified), re-run with"
            warn "INSTALL_CHV_ALLOW_VMM_MISMATCH=1."
        fi
    fi

    if [ "$chv_pin_ok" = "0" ]; then
        info "Downloading Cloud Hypervisor v${chv_version}..."
        local chv_tmp
        chv_tmp=$(mktemp)
        if ! curl -fsSL "https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v${chv_version}/cloud-hypervisor-static" \
            -o "$chv_tmp"; then
            rm -f "$chv_tmp"
            fatal "Failed to download Cloud Hypervisor v${chv_version}."
        fi
        if ! echo "${chv_sha256}  ${chv_tmp}" | sha256sum -c - >/dev/null 2>&1; then
            rm -f "$chv_tmp"
            fatal "Cloud Hypervisor v${chv_version} digest mismatch (expected ${chv_sha256}); aborting install."
        fi
        install -m 0755 "$chv_tmp" /usr/local/bin/cloud-hypervisor
        rm -f "$chv_tmp"
        ln -sf /usr/local/bin/cloud-hypervisor /usr/bin/cloud-hypervisor
        info "Cloud Hypervisor installed: $(cloud-hypervisor --version)"
    fi

    # ch-remote is version-coupled to cloud-hypervisor; keep the same gate so a
    # stale host client is not left paired with the pinned VMM.
    local remote_pin_ok=0
    if cmd_exists ch-remote; then
        local remote_raw remote_mm
        remote_raw="$(ch-remote --version 2>/dev/null || true)"
        remote_mm="$(printf '%s' "$remote_raw" | grep -oE '[0-9]+\.[0-9]+' | head -n1 || true)"
        if [ "$remote_mm" = "$chv_version" ]; then
            info "ch-remote already installed at the qualified pin (${remote_raw}); keeping it."
            remote_pin_ok=1
        elif [ "${INSTALL_CHV_ALLOW_VMM_MISMATCH:-0}" = "1" ]; then
            warn "INSTALL_CHV_ALLOW_VMM_MISMATCH=1: keeping the host ch-remote (off-pin, unqualified)."
            remote_pin_ok=1
        else
            warn "ch-remote version mismatch: found ${remote_raw:-unparseable},"
            warn "but the qualified pin is v${chv_version}. Replacing it with the"
            warn "pinned, digest-verified static build."
        fi
    fi

    if [ "$remote_pin_ok" = "0" ]; then
        info "Downloading ch-remote v${chv_version}..."
        local remote_tmp
        remote_tmp=$(mktemp)
        if ! curl -fsSL "https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v${chv_version}/ch-remote-static" \
            -o "$remote_tmp"; then
            rm -f "$remote_tmp"
            fatal "Failed to download ch-remote v${chv_version}."
        fi
        if ! echo "${chv_remote_sha256}  ${remote_tmp}" | sha256sum -c - >/dev/null 2>&1; then
            rm -f "$remote_tmp"
            fatal "ch-remote v${chv_version} digest mismatch (expected ${chv_remote_sha256}); aborting install."
        fi
        install -m 0755 "$remote_tmp" /usr/local/bin/ch-remote
        rm -f "$remote_tmp"
        info "ch-remote installed: $(ch-remote --version 2>/dev/null || echo v${chv_version})"
    fi
}

# -----------------------------------------------------------------------------
# TLS certificates (same shapes as scripts/install.sh generate_certs)
# -----------------------------------------------------------------------------
generate_certs() {
    info "Generating TLS certificates..."

    mkdir -p "${CHV_CONFIG_DIR}/certs"
    chown root:"$CHV_USER" "${CHV_CONFIG_DIR}/certs"
    chmod 750 "${CHV_CONFIG_DIR}/certs"

    if [ ! -f "${CHV_CONFIG_DIR}/certs/ca.key" ]; then
        openssl genrsa -out "${CHV_CONFIG_DIR}/certs/ca.key" 4096 2>/dev/null
        openssl req -x509 -new -nodes -key "${CHV_CONFIG_DIR}/certs/ca.key" \
            -sha256 -days 3650 -out "${CHV_CONFIG_DIR}/certs/ca.crt" \
            -subj "/O=CHV/CN=chv-ca" \
            -addext "basicConstraints=critical,CA:TRUE" \
            -addext "keyUsage=critical,keyCertSign,cRLSign" 2>/dev/null
        chmod 640 "${CHV_CONFIG_DIR}/certs/ca.key"
        chmod 644 "${CHV_CONFIG_DIR}/certs/ca.crt"
        chown root:"$CHV_USER" "${CHV_CONFIG_DIR}/certs/ca.key" "${CHV_CONFIG_DIR}/certs/ca.crt"
    fi

    if [ ! -f "${CHV_CONFIG_DIR}/certs/server.key" ]; then
        openssl genrsa -out "${CHV_CONFIG_DIR}/certs/server.key" 2048 2>/dev/null
        openssl req -new -key "${CHV_CONFIG_DIR}/certs/server.key" \
            -out "${CHV_CONFIG_DIR}/certs/server.csr" \
            -subj "/O=CHV/CN=chv-controlplane" 2>/dev/null
        openssl x509 -req -in "${CHV_CONFIG_DIR}/certs/server.csr" \
            -CA "${CHV_CONFIG_DIR}/certs/ca.crt" -CAkey "${CHV_CONFIG_DIR}/certs/ca.key" \
            -CAcreateserial -out "${CHV_CONFIG_DIR}/certs/server.crt" \
            -days 825 -sha256 \
            -extfile <(printf "subjectAltName=DNS:localhost,IP:127.0.0.1\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth") 2>/dev/null
        rm -f "${CHV_CONFIG_DIR}/certs/server.csr"
        chmod 640 "${CHV_CONFIG_DIR}/certs/server.key"
        chmod 644 "${CHV_CONFIG_DIR}/certs/server.crt"
        chown root:"$CHV_USER" "${CHV_CONFIG_DIR}/certs/server.key" "${CHV_CONFIG_DIR}/certs/server.crt"
    fi

    # Agent client certificate (for mTLS to the control plane)
    if [ ! -f "${CHV_CONFIG_DIR}/certs/agent-client.key" ]; then
        openssl genrsa -out "${CHV_CONFIG_DIR}/certs/agent-client.key" 2048 2>/dev/null
        openssl req -new -key "${CHV_CONFIG_DIR}/certs/agent-client.key" \
            -out "${CHV_CONFIG_DIR}/certs/agent-client.csr" \
            -subj "/O=CHV/CN=chv-agent" 2>/dev/null
        openssl x509 -req -in "${CHV_CONFIG_DIR}/certs/agent-client.csr" \
            -CA "${CHV_CONFIG_DIR}/certs/ca.crt" -CAkey "${CHV_CONFIG_DIR}/certs/ca.key" \
            -CAcreateserial -out "${CHV_CONFIG_DIR}/certs/agent-client.crt" \
            -days 825 -sha256 \
            -extfile <(printf "keyUsage=digitalSignature\nextendedKeyUsage=clientAuth") 2>/dev/null
        rm -f "${CHV_CONFIG_DIR}/certs/agent-client.csr"
        chmod 640 "${CHV_CONFIG_DIR}/certs/agent-client.key"
        chmod 644 "${CHV_CONFIG_DIR}/certs/agent-client.crt"
        chown root:"$CHV_USER" "${CHV_CONFIG_DIR}/certs/agent-client.key" "${CHV_CONFIG_DIR}/certs/agent-client.crt"
    fi
}

# -----------------------------------------------------------------------------
# Configuration files (fresh installs only — a re-run never rewrites config;
# same key shapes as scripts/install.sh install_configs, package-layout paths)
# -----------------------------------------------------------------------------
write_configs() {
    info "Writing configuration files..."

    CHV_NODE_ID=$(uuidgen 2>/dev/null || cat /proc/sys/kernel/random/uuid 2>/dev/null || openssl rand -hex 16)
    info "Node ID: ${CHV_NODE_ID}"

    JWT_SECRET=$(openssl rand -base64 32 | tr -d '=+/')

    cat > "${CHV_CONFIG_DIR}/controlplane.toml" <<EOF
# Generated by scripts/quick-install.sh (CHV quick install, issue #482).
grpc_bind = "127.0.0.1:8443"
# Quick-path judgment: the binary serves the WebUI itself (#447) with no
# nginx edge, so its HTTP listener is the front door and must bind all
# interfaces. Static assets are unauthenticated (login page); every API
# route keeps its auth. No TLS on this path — keep it on a trusted network.
http_bind = "0.0.0.0:8080"
log_level = "info"
runtime_dir = "/run/chv/controlplane"
jwt_secret = "${JWT_SECRET}"

[database]
url = "sqlite://${CHV_DB_PATH}"
migrations_dir = "${CHV_MIGRATIONS_DIR}"
max_connections = 4
min_connections = 1
acquire_timeout_secs = 5

[tls]
ca_cert_path = "${CHV_CONFIG_DIR}/certs/ca.crt"
ca_key_path = "${CHV_CONFIG_DIR}/certs/ca.key"
server_cert_path = "${CHV_CONFIG_DIR}/certs/server.crt"
server_key_path = "${CHV_CONFIG_DIR}/certs/server.key"
client_ca_path = "${CHV_CONFIG_DIR}/certs/ca.crt"

# Web UI static serving (#447, D3 target): the control plane serves the
# UI itself from its HTTP listener. This is the quick path's only UI
# surface — there is no nginx on this path.
[webui]
enabled = true
dir = "${CHV_UI_DIR}"
EOF
    chmod 640 "${CHV_CONFIG_DIR}/controlplane.toml"
    chown root:"$CHV_USER" "${CHV_CONFIG_DIR}/controlplane.toml"

    cat > "${CHV_CONFIG_DIR}/agent.toml" <<EOF
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
# respawns — mirrors stord.toml's path_allowlist below.
stord_path_allowlist = ["${CHV_DATA_DIR}/storage/localdisk", "${CHV_DATA_DIR}/storage/lvm", "${CHV_DATA_DIR}/agent"]

# #385: respawn config fidelity — a respawned stord execs the operator's
# stord.toml below instead of a lossy generated config.
stord_config_path = "${CHV_CONFIG_DIR}/stord.toml"

# #504: nwd respawn config fidelity — same pass-through for nwd.
nwd_config_path = "${CHV_CONFIG_DIR}/nwd.toml"
EOF
    chmod 640 "${CHV_CONFIG_DIR}/agent.toml"
    chown root:"$CHV_USER" "${CHV_CONFIG_DIR}/agent.toml"

    cat > "${CHV_CONFIG_DIR}/stord.toml" <<EOF
socket_path = "/run/chv/stord/api.sock"
runtime_dir = "${CHV_DATA_DIR}/storage/localdisk"
log_level = "info"
path_allowlist = ["${CHV_DATA_DIR}/storage/localdisk", "${CHV_DATA_DIR}/storage/lvm", "${CHV_DATA_DIR}/agent"]
device_allowlist = ["/dev/dm-*", "/dev/mapper/*"]
EOF
    chmod 640 "${CHV_CONFIG_DIR}/stord.toml"
    chown root:chv-stord "${CHV_CONFIG_DIR}/stord.toml"

    cat > "${CHV_CONFIG_DIR}/nwd.toml" <<EOF
socket_path = "/run/chv/nwd/api.sock"
runtime_dir = "/run/chv/nwd"
log_level = "info"
# Bridge topology is configured at runtime via gRPC topology specs from
# the control plane, not in this config file.
EOF
    chmod 640 "${CHV_CONFIG_DIR}/nwd.toml"
    chown root:"$CHV_USER" "${CHV_CONFIG_DIR}/nwd.toml"
}

# -----------------------------------------------------------------------------
# Credential encryption key (#335 / report finding H-7) — same contract as
# install.sh and the package postinst: mint once, NEVER regenerate.
# -----------------------------------------------------------------------------
create_encryption_env() {
    if [ -f "${CHV_CONFIG_DIR}/encryption.env" ]; then
        if grep -q '^CHV_ENCRYPTION_KEY=[0-9a-f]\{64\}$' "${CHV_CONFIG_DIR}/encryption.env"; then
            info "Preserving existing credential encryption key."
        else
            warn "Existing ${CHV_CONFIG_DIR}/encryption.env is empty or malformed;"
            warn "S3 credentials will be stored in PLAINTEXT until it is fixed."
        fi
    else
        info "Generating credential encryption key..."
        local key
        key=$(openssl rand -hex 32 2>/dev/null || true)
        if ! printf '%s' "$key" | grep -qE '^[0-9a-f]{64}$'; then
            fatal "failed to generate the credential encryption key (openssl rand -hex 32); refusing to write an empty key — S3 credentials would be stored in plaintext with no warning"
        fi
        (
            umask 077
            printf 'CHV_ENCRYPTION_KEY=%s\n' "$key" \
                > "${CHV_CONFIG_DIR}/encryption.env"
        )
    fi
    chmod 0600 "${CHV_CONFIG_DIR}/encryption.env"
}

# Run sqlite3 as the CHV service user — install.sh's proven shape for
# touching the live WAL database. A root-run sqlite3 leaves root-owned
# -wal/-shm files behind, and the controlplane (running as chv) then fails
# on them; the chown-repair-afterwards alternative is silently skippable
# on a data-integrity path. sudo -u first (install.sh's exact idiom);
# runuser (util-linux, present wherever systemctl is) when sudo is not
# installed.
chv_sqlite() {
    if cmd_exists sudo; then
        sudo -u "$CHV_USER" sqlite3 "$@"
    else
        runuser -u "$CHV_USER" -- sqlite3 "$@"
    fi
}

# -----------------------------------------------------------------------------
# Bootstrap token (fresh installs only; the file is the agent's enrollment
# secret, the control plane learns its hash via the loopback-only
# /internal/bootstrap-token route — #547 first-run flow)
# -----------------------------------------------------------------------------
create_bootstrap_token() {
    info "Creating bootstrap token..."
    BOOTSTRAP_TOKEN=$(openssl rand -hex 32)
    printf '%s' "$BOOTSTRAP_TOKEN" > "${CHV_CONFIG_DIR}/bootstrap.token"
    chmod 640 "${CHV_CONFIG_DIR}/bootstrap.token"
    chown root:"$CHV_USER" "${CHV_CONFIG_DIR}/bootstrap.token"
}

seed_bootstrap_token() {
    info "Seeding bootstrap token via the control plane API..."
    local seed_response
    seed_response=$(curl -sf -X POST "http://127.0.0.1:8080/internal/bootstrap-token" \
        -H "Content-Type: application/json" \
        -d "{\"token\": \"${BOOTSTRAP_TOKEN}\", \"description\": \"CHV quick install\", \"one_time_use\": true}" 2>/dev/null) || true

    if echo "$seed_response" | grep -q '"status":"ok"'; then
        info "Bootstrap token seeded via API successfully."
        return
    fi

    # Fallback for controlplane builds without the /internal/bootstrap-token
    # route (it landed in v0.2.0; a pinned older release hits this). Same
    # shape as install.sh's fallback: sqlite3 insert of the sha256 hash RUN
    # AS THE CHV USER (see chv_sqlite — the live WAL database must never be
    # written as root), then restart. Disclosed residual, inherited from
    # install.sh's shape: if the insert fails under set -e, the controlplane
    # is left stopped between the stop above and the restart below — it
    # fails loudly (never half-installs silently), and only pre-v0.2.0 pins
    # can reach this branch.
    warn "API seeding failed (response: ${seed_response:-empty}), falling back to sqlite3 CLI..."
    if ! cmd_exists sqlite3; then
        fatal "sqlite3 is required to seed the bootstrap token. Install sqlite3 and re-run."
    fi
    systemctl stop chv-controlplane
    sleep 1

    local token_hash expires
    token_hash=$(printf '%s' "$BOOTSTRAP_TOKEN" | sha256sum | awk '{print $1}')
    expires=$(date -u -d "+1 hour" '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null \
              || date -u -v+1H '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null \
              || echo "")
    # Interpolated SQL (not install.sh's `?` + extra-args idiom — the
    # sqlite3 CLI does not bind extra arguments to placeholders and rejects
    # that shape; see the PR body's drift flag). Injection-safe by
    # construction, verified fail-closed: token_hash is sha256sum output,
    # expires is a fixed-format timestamp or empty.
    if ! printf '%s' "$token_hash" | grep -qE '^[0-9a-f]{64}$'; then
        fatal "bootstrap token hash is malformed (expected 64 hex chars); refusing to seed."
    fi
    chv_sqlite "${CHV_DB_PATH}" \
        "INSERT OR REPLACE INTO bootstrap_tokens
         (token_hash, description, one_time_use, used_at, expires_at, created_at, updated_at)
         VALUES ('${token_hash}', 'CHV quick install', 1, NULL,
                 NULLIF('${expires}', ''),
                 strftime('%Y-%m-%dT%H:%M:%SZ','now'),
                 strftime('%Y-%m-%dT%H:%M:%SZ','now'));"
    local seeded
    seeded=$(chv_sqlite "${CHV_DB_PATH}" \
        "SELECT COUNT(*) FROM bootstrap_tokens WHERE token_hash='${token_hash}';" 2>/dev/null || echo "0")
    if [ "${seeded}" -lt 1 ] 2>/dev/null; then
        fatal "Failed to seed bootstrap token in ${CHV_DB_PATH}."
    fi

    info "Restarting control plane..."
    systemctl start chv-controlplane
    wait_for_controlplane_health
}

# -----------------------------------------------------------------------------
# Bootstrap admin user (fresh installs only; same contract as install.sh
# seed_admin_user: random password, bcrypt cost 12, must_change_password=1,
# plaintext printed once and stored 0600 root-only)
# -----------------------------------------------------------------------------
seed_admin_user() {
    local pw_file="${CHV_CONFIG_DIR}/initial_admin_password"

    info "Seeding bootstrap admin user..."

    if ! cmd_exists sqlite3; then
        fatal "sqlite3 not available — cannot seed admin user. Install the sqlite3 package and re-run."
    fi

    # Idempotency guard: if the admin user already exists, do nothing.
    if [ -f "${CHV_DB_PATH}" ] && \
       [ "$(sqlite3 "${CHV_DB_PATH}" "SELECT COUNT(*) FROM users WHERE username = 'admin';" 2>/dev/null)" = "1" ]; then
        info "Admin user already exists — skipping seed."
        return 0
    fi

    local plaintext_pw
    plaintext_pw=$(openssl rand -base64 18 | tr -d '\n' | tr '+/' '-_')
    if [ -z "$plaintext_pw" ]; then
        fatal "Failed to generate random password (openssl rand failed)."
    fi

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
        fatal "Neither python3-bcrypt nor htpasswd available — cannot bcrypt the admin password. Install one of: 'pip3 install bcrypt' or 'apt install apache2-utils' (Debian/Ubuntu) / 'dnf install httpd-tools' (RHEL/Fedora), then re-run."
    fi

    if [ -z "$hashed_pw" ]; then
        fatal "Bcrypt produced empty hash — refusing to seed admin user."
    fi

    local admin_user_id="00000000-0000-0000-0000-000000000001"

    if ! sqlite3 "${CHV_DB_PATH}" <<SQL
INSERT INTO users (user_id, username, password_hash, role, display_name, must_change_password, created_at, updated_at)
VALUES ('${admin_user_id}', 'admin', '${hashed_pw}', 'admin', 'Administrator', 1,
        strftime('%Y-%m-%dT%H:%M:%SZ','now'),
        strftime('%Y-%m-%dT%H:%M:%SZ','now'));
SQL
    then
        fatal "Failed to insert bootstrap admin row into ${CHV_DB_PATH}."
    fi
    chown "${CHV_USER}:${CHV_USER}" "${CHV_DB_PATH}" "${CHV_DB_PATH}-wal" "${CHV_DB_PATH}-shm" 2>/dev/null || true

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
# Service start / enrollment
# -----------------------------------------------------------------------------
wait_for_controlplane_health() {
    info "Waiting for control plane to apply database migrations (up to 60s)..."
    local attempt=1
    while [ $attempt -le 60 ]; do
        if curl -sf "http://127.0.0.1:8080/health" &>/dev/null; then
            info "Control plane API is up (migrations applied)."
            return 0
        fi
        sleep 1
        attempt=$((attempt + 1))
    done
    fatal "Control plane did not become healthy within 60s. Check: journalctl -u chv-controlplane -n 50"
}

start_services() {
    info "Enabling and starting CHV services..."

    systemctl enable chv-controlplane
    if [ "$FRESH_INSTALL" = "1" ]; then
        systemctl start chv-controlplane
    else
        systemctl restart chv-controlplane 2>/dev/null || systemctl start chv-controlplane
    fi
    wait_for_controlplane_health

    if [ "$FRESH_INSTALL" = "1" ]; then
        seed_bootstrap_token
    else
        info "Existing install: bootstrap token left as-is (config preserved)."
    fi

    # Delete only the legacy enrollment cache; Core state is retained.
    rm -f "${CHV_DATA_DIR}/cache/agent-cache.json"

    # Pre-place the agent client cert so mTLS works immediately (idempotent:
    # /run is tmpfs, so this also restores the certs after a reboot+rerun).
    mkdir -p /run/chv/agent
    cp "${CHV_CONFIG_DIR}/certs/agent-client.crt" /run/chv/agent/agent.crt
    cp "${CHV_CONFIG_DIR}/certs/agent-client.key" /run/chv/agent/agent.key
    cp "${CHV_CONFIG_DIR}/certs/ca.crt" /run/chv/agent/ca.crt
    chown "$CHV_USER":"$CHV_USER" /run/chv/agent/agent.crt /run/chv/agent/agent.key /run/chv/agent/ca.crt
    chmod 640 /run/chv/agent/agent.key

    # Storage ownership contract (matches the .deb postinst, #323):
    # storage dirs chv:chv-stord 0770.
    if [ -d "${CHV_DATA_DIR}/storage" ]; then
        chown -R "$CHV_USER:chv-stord" "${CHV_DATA_DIR}/storage" 2>/dev/null || true
        chmod 770 "${CHV_DATA_DIR}/storage" "${CHV_DATA_DIR}/storage/localdisk" "${CHV_DATA_DIR}/storage/lvm" 2>/dev/null || true
    fi
    mkdir -p "${CHV_DATA_DIR}/agent/vms" 2>/dev/null || true
    chown "$CHV_USER:$CHV_USER" "${CHV_DATA_DIR}/agent" 2>/dev/null || true
    chown -R "$CHV_USER:chv-stord" "${CHV_DATA_DIR}/agent/vms" 2>/dev/null || true
    chmod 700 "${CHV_DATA_DIR}/agent" 2>/dev/null || true
    chmod 775 "${CHV_DATA_DIR}/agent/vms" 2>/dev/null || true

    systemctl enable chv-stord chv-nwd chv-agent
    if [ "$FRESH_INSTALL" = "1" ]; then
        systemctl start chv-stord chv-nwd chv-agent
    else
        systemctl restart chv-stord chv-nwd chv-agent 2>/dev/null \
            || systemctl start chv-stord chv-nwd chv-agent
    fi

    if [ ! -e /dev/kvm ]; then
        warn "/dev/kvm is not present (no KVM, or a container without device passthrough)."
        warn "The agent will enroll, but VMs cannot run on this host."
    fi
}

# Best-effort bounded wait for the local agent's enrollment. Read-only
# sqlite polling avoids the admin-auth/must_change_password dance install.sh
# needs for its API-based TenantReady wait. A timeout is a LOUD warning, not
# a fatal: the install itself is complete and the state is observable.
wait_for_enrollment() {
    if ! cmd_exists sqlite3 || [ ! -f "${CHV_DB_PATH}" ]; then
        info "Enrollment check skipped (sqlite3 or database not available)."
        return 0
    fi
    info "Waiting for the local agent to enroll (up to 90s)..."
    local attempt=1 enrolled
    while [ $attempt -le 90 ]; do
        enrolled=$(sqlite3 "${CHV_DB_PATH}" \
            "SELECT COUNT(*) FROM nodes WHERE enrolled_at IS NOT NULL;" 2>/dev/null || echo "0")
        if [ "${enrolled}" -ge 1 ] 2>/dev/null; then
            info "Local agent enrolled."
            return 0
        fi
        sleep 1
        attempt=$((attempt + 1))
    done
    warn "The local agent did not enroll within 90s. The install is complete,"
    warn "but enrollment is still pending — check:"
    warn "  journalctl -u chv-agent -n 50"
    warn "  journalctl -u chv-controlplane -n 50"
    return 0
}

get_local_ip() {
    hostname -I 2>/dev/null | awk '{print $1}' || echo "127.0.0.1"
}

print_success() {
    local local_ip
    local_ip=$(get_local_ip)

    cat <<EOF

===============================================
  CHV Quick Install Complete (v${VERSION})
===============================================

  Web UI:         http://${local_ip}:8080/
                  (served by the control plane binary itself — no nginx;
                   http_bind is 0.0.0.0:8080 on this path, plain HTTP)

  Admin login:    admin / (see ${CHV_CONFIG_DIR}/initial_admin_password)
                  The bootstrap password is in that file (mode 0600, root),
                  printed once above, and must be rotated on first login.

  Bootstrap token (agent enrollment): ${CHV_CONFIG_DIR}/bootstrap.token

  Services:
    systemctl status chv-controlplane chv-agent chv-stord chv-nwd

  Logs:
    journalctl -u chv-controlplane -f
    journalctl -u chv-agent -f

  NOTE — serial console: the /ws/ console does NOT work on this path
  (the control plane does not proxy /ws/, and no nginx edge is installed).
  VM lifecycle works from the WebUI; the console needs an edge proxy —
  see docs/DEPLOYMENT.md "Serving the Web UI in package mode".

  NOTE — qualification: this path is [CODE-SUPPORTED, UNQUALIFIED].
  The qualified deployment path is scripts/install.sh (see
  docs/DEPLOYMENT.md).

  Upgrade:    re-run this script (config and data are preserved).
  Uninstall:  sudo scripts/quick-install.sh --uninstall          (keeps data)
              sudo scripts/quick-install.sh --uninstall --purge  (removes all)
              (when curl-piping: append -- --uninstall to the bash -s call)

===============================================

EOF
}

# -----------------------------------------------------------------------------
# Uninstall (software removed, data preserved by default — mirrors
# docs/install/uninstall.md; --purge also removes data and config)
# -----------------------------------------------------------------------------
uninstall() {
    info "Stopping and disabling CHV services..."
    local svc
    for svc in chv-agent chv-stord chv-nwd chv-controlplane; do
        systemctl stop "$svc" 2>/dev/null || true
        systemctl disable "$svc" 2>/dev/null || true
    done

    local removed_via_pkg="0"
    if cmd_exists dpkg && dpkg -s chv-controlplane >/dev/null 2>&1; then
        info "Removing .deb packages (dpkg -r preserves /etc/chv and /var/lib/chv)..."
        dpkg -r chv-node chv-controlplane chvctl 2>/dev/null && removed_via_pkg="1"
    elif cmd_exists rpm && rpm -q chv-controlplane >/dev/null 2>&1; then
        info "Removing .rpm packages (rpm -e preserves /etc/chv and /var/lib/chv)..."
        rpm -e chv-node chv-controlplane chvctl 2>/dev/null && removed_via_pkg="1"
    fi
    if [ "$removed_via_pkg" != "1" ]; then
        info "No installed CHV package detected (tarball leg or already removed)."
    fi

    # Remove files directly as well — covers the tarball leg and any file
    # the package removal above failed to take (it is belt-and-braces, not
    # a silent fallback: package-manager failures above already warned).
    info "Removing binaries, units, and shared trees..."
    rm -f /usr/bin/chv-controlplane /usr/bin/chv-agent \
          /usr/bin/chv-stord /usr/bin/chv-nwd /usr/bin/chvctl
    rm -f /etc/systemd/system/chv-controlplane.service \
          /etc/systemd/system/chv-agent.service \
          /etc/systemd/system/chv-stord.service \
          /etc/systemd/system/chv-nwd.service
    rm -f /usr/lib/tmpfiles.d/chv-node.conf
    rm -rf /usr/share/chv
    systemctl daemon-reload

    if [ "$ARG_PURGE" = "1" ]; then
        info "Purging data and configuration..."
        rm -rf "$CHV_CONFIG_DIR" "$CHV_DATA_DIR" "$CHV_LOG_DIR" "$CHV_RUN_DIR"
        # The chv / chv-stord users are deliberately retained (matches
        # docs/install/uninstall.md; reclaiming them is the operator's call:
        #   userdel chv; groupdel chv; userdel chv-stord; groupdel chv-stord)
        info "Users chv and chv-stord retained (remove manually if desired)."
    else
        info "Preserved (use --uninstall --purge to remove):"
        info "  ${CHV_CONFIG_DIR} (config, certs, secrets)"
        info "  ${CHV_DATA_DIR} (database, volumes, agent state)"
        info "  ${CHV_LOG_DIR}"
    fi
    info "Uninstall complete."
}

# -----------------------------------------------------------------------------
# Main
# -----------------------------------------------------------------------------
main() {
    if [ -z "${BASH_VERSION:-}" ]; then
        fatal "This installer requires bash. Re-run with: ... | sudo bash -s"
    fi

    # Parse CLI flags
    while [ $# -gt 0 ]; do
        case "$1" in
            --version)
                [ $# -ge 2 ] || fatal "--version requires an argument (e.g. --version 0.3.0)"
                ARG_VERSION="$2"
                shift 2
                ;;
            --version=*)
                ARG_VERSION="${1#*=}"
                shift
                ;;
            --uninstall) ARG_UNINSTALL="1"; shift ;;
            --purge) ARG_PURGE="1"; shift ;;
            --force) ARG_FORCE="1"; shift ;;
            --dry-run) ARG_DRYRUN="1"; shift ;;
            --tarball) ARG_TARBALL="1"; shift ;;
            --wipe|--fresh)
                fatal "--wipe/--fresh are install.sh flags; the quick installer has no wipe. Use: --uninstall --purge, then re-run."
                ;;
            -h|--help) usage; exit 0 ;;
            *) fatal "Unknown argument: $1 (see --help)" ;;
        esac
    done

    if [ "$ARG_DRYRUN" != "1" ]; then
        if [ "$(id -u)" -ne 0 ]; then
            fatal "This installer must be run as root. Try: curl -fsSL <url> | sudo bash -s"
        fi
        if ! cmd_exists systemctl; then
            fatal "systemctl not found — this installer requires a systemd host."
        fi
    fi
    if ! cmd_exists curl; then
        fatal "curl is required (install it, then re-run)."
    fi

    trap cleanup EXIT

    detect_arch

    if [ "$ARG_UNINSTALL" = "1" ]; then
        if [ "$ARG_DRYRUN" = "1" ]; then
            fatal "--dry-run cannot be combined with --uninstall."
        fi
        if [ "$ARG_PURGE" = "1" ]; then
            warn "--purge: ALL CHV data and configuration will be deleted."
        fi
        uninstall
        return 0
    fi

    detect_distro
    resolve_release
    download_and_verify

    if [ "$ARG_DRYRUN" = "1" ]; then
        info "Dry run: artifacts resolved and verified. Plan:"
        info "  version:      v${VERSION} (package version ${PKG_VERSION})"
        info "  platform:     ${CHV_ARCH} via ${PKG_KIND} leg"
        info "  assets:       $(expected_assets | tr '\n' ' ')"
        info "  layout:       binaries /usr/bin/chv-*, UI ${CHV_UI_DIR},"
        info "                migrations ${CHV_MIGRATIONS_DIR}, config ${CHV_CONFIG_DIR}"
        info "No changes were made to this host."
        return 0
    fi

    check_existing_install
    install_dependencies
    install_artifacts
    install_cloud_hypervisor
    generate_certs
    if [ "$FRESH_INSTALL" = "1" ]; then
        write_configs
        create_encryption_env
        create_bootstrap_token
    else
        info "Existing configuration preserved — not rewriting ${CHV_CONFIG_DIR}/*.toml."
        create_encryption_env
    fi

    if [ "$FRESH_INSTALL" = "1" ]; then
        start_services
        seed_admin_user || fatal "Failed to seed bootstrap admin user."
        wait_for_enrollment
    else
        start_services
        wait_for_enrollment
    fi

    print_success
}

# Allow sourcing for sandbox tests without executing the install.
#
# B1 guard idiom: stdin execution (curl … | sudo bash -s, the documented
# one-liner) leaves BASH_SOURCE empty and $0 = "bash"; a file execution has
# BASH_SOURCE[0] == $0; a sourcing has BASH_SOURCE[0] != $0. All three are
# handled — and the empty-array deref must be ${BASH_SOURCE[0]:-} because
# the bare form is unset under stdin and trips `set -u` before main runs.
if [ -z "${BASH_SOURCE[0]:-}" ] || [ "${BASH_SOURCE[0]:-}" = "$0" ]; then
    main "$@"
fi
