#!/bin/sh
set -e

# CHV Generic Postinstall
# Safe operations only:
#   - Create system user and group
#   - Create runtime/state directories
#   - Add user to kvm group (if available)
#   - Reload systemd
#
# This script intentionally does NOT:
#   - Create network bridges
#   - Initialize storage pools
#   - Start VMs
#   - Modify firewall rules
#   - Wipe or format disks

# Credential encryption key (#335, report finding H-7): the control plane
# derives an AES-256-GCM key from CHV_ENCRYPTION_KEY to encrypt S3
# credentials at rest (crates/chv-controlplane-store/src/
# credential_crypto.rs); without it they are stored in plaintext.
# chv-controlplane.service loads this file via EnvironmentFile. Mint once
# and NEVER regenerate (dpkg configure on upgrade runs this again): existing
# encrypted credentials would become unrecoverable. Key material comes from
# /dev/urandom (no openssl dependency in the postinst environment).
if [ -x /usr/bin/chv-controlplane ] && [ ! -f /etc/chv/encryption.env ]; then
    mkdir -p /etc/chv
    _chv_key="$(head -c 32 /dev/urandom 2>/dev/null | od -An -tx1 2>/dev/null | tr -d ' \n')"
    # Only write a well-formed key: an empty file would disable encryption
    # and be preserved forever by the create-if-absent guard above. (If we
    # skip entirely the variable stays unset, so the control plane still
    # logs its plaintext warning at startup — loud, not silent.)
    if printf '%s' "$_chv_key" | grep -qE '^[0-9a-f]{64}$'; then
        # Subshell: umask 077 must not leak into later directory creation
        # in this script (state/log dirs are 0755 by contract).
        (
            umask 077
            printf 'CHV_ENCRYPTION_KEY=%s\n' "$_chv_key" > /etc/chv/encryption.env
        )
    else
        # Not fatal (the daemon still warns at startup while the variable
        # is unset), but say so here too instead of skipping silently.
        echo "chv-controlplane: WARNING: failed to generate the credential encryption key;" >&2
        echo "chv-controlplane: S3 credentials will be stored in plaintext until it is provisioned." >&2
    fi
elif [ -x /usr/bin/chv-controlplane ] && [ -f /etc/chv/encryption.env ]; then
    # Preserved, not regenerated — but warn loudly if it is unusable.
    if ! grep -q '^CHV_ENCRYPTION_KEY=[0-9a-f]\{64\}$' /etc/chv/encryption.env; then
        echo "chv-controlplane: WARNING: /etc/chv/encryption.env exists but is empty or malformed;" >&2
        echo "chv-controlplane: S3 credentials will be stored in plaintext until it is fixed." >&2
    fi
fi
if [ -f /etc/chv/encryption.env ]; then
    chmod 0600 /etc/chv/encryption.env
fi

# Create the 'chv' system user and group if they don't exist
if ! getent group chv >/dev/null 2>&1; then
    groupadd -r chv
fi

if ! getent passwd chv >/dev/null 2>&1; then
    useradd -r -g chv -d /var/lib/chv -s /usr/sbin/nologin chv
fi

# Create the 'chv-stord' system user and group if they don't exist
if ! getent group chv-stord >/dev/null 2>&1; then
    groupadd -r chv-stord
fi

if ! getent passwd chv-stord >/dev/null 2>&1; then
    useradd -r -g chv-stord -d /var/lib/chv -s /usr/sbin/nologin chv-stord
fi

# Ensure state and runtime directories exist and are owned by chv
mkdir -p /var/lib/chv /var/log/chv /run/chv
chown chv:chv /var/lib/chv /var/log/chv /run/chv || true
install -d -m 0700 -o chv -g chv /var/lib/chv/agent /var/lib/chv/cache /run/chv/core
install -d -m 0775 -o chv -g chv /run/chv/agent

# Storage state lives under /var/lib/chv/storage. chv-stord runs as the
# 'chv' service user: its API socket is mode 0600 with chv-agent as the
# only client, and cloud-hypervisor (spawned by chv-agent as 'chv') must
# read and write volume files. The directories therefore must be writable
# by 'chv'. Group 'chv-stord' is kept as the isolation seam for a future
# dedicated storage-user model.
install -d -m 0770 -o chv -g chv-stord /var/lib/chv/storage/localdisk /var/lib/chv/storage/lvm
# Runtime dir for the systemd unit path (RuntimeDirectory=chv/stord creates
# the same ownership); kept here so non-systemd starts also work as 'chv'.
install -d -m 0755 -o chv -g chv /run/chv/stord

# Apply the shipped tmpfiles entry now. systemd-tmpfiles runs it at boot, but
# a package installed on a live host starts the units in this same configure
# run, and chv-nwd's ReadWritePaths=/run/netns requires the directory to
# exist before the unit starts. Failure is loud but not fatal: a chroot or
# container install without a running systemd still needs to configure.
if [ -f /usr/lib/tmpfiles.d/chv-node.conf ] && command -v systemd-tmpfiles >/dev/null 2>&1; then
    if ! systemd-tmpfiles --create /usr/lib/tmpfiles.d/chv-node.conf 2>/dev/null; then
        echo "chv-node: WARNING: systemd-tmpfiles --create failed; chv-nwd needs /run/netns to start." >&2
    fi
fi

# Add chv user to the kvm group if it exists (required for VM runtime)
if getent group kvm >/dev/null 2>&1; then
    # NOTE: match the group list per-entry (`grep -qx` over one group per
    # line). A plain `id -nG | grep -qw chv` would also match `chv-stord`
    # (the hyphen is a word boundary) and silently skip the usermod.
    if ! id -nG chv | tr ' ' '\n' | grep -qx kvm; then
        usermod -aG kvm chv
    fi
fi

# Add chv-stord user to the disk group for block device access
if getent group disk >/dev/null 2>&1; then
    if ! id -nG chv-stord | tr ' ' '\n' | grep -qx disk; then
        usermod -aG disk chv-stord
    fi
fi

# Add chv-stord to chv group so it can traverse /var/lib/chv
if ! id -nG chv-stord | tr ' ' '\n' | grep -qx chv; then
    usermod -aG chv chv-stord
fi

# Add chv (the agent service user) to the chv-stord group so the agent can
# read a hardened stord.toml (root:chv-stord 0640) for #385 respawn
# pass-through — the same grant scripts/install.sh makes at install time.
# Without it, a .deb operator applying the documented hardening makes
# stord.toml unreadable by the agent and every respawn silently degrades
# to the generated config. Idempotent: the guard skips an already-granted
# membership on reinstall/upgrade. The grant applies at the agent's next
# (re)start — systemd merges the user's NSS groups into the unit's
# SupplementaryGroups — and respawns before that warn and fall back to
# the generated config safely (postinst deliberately does not restart
# services, matching the preremove upgrade contract).
if ! id -nG chv | tr ' ' '\n' | grep -qx chv-stord; then
    usermod -aG chv-stord chv
fi

# In-place upgrade from the scripts/install.sh layout (#666 fresh-host
# qualification). install.sh copies migrations to
# /usr/local/share/chv/migrations and writes that path into the preserved
# controlplane.toml; the packages install the same tree under /usr/share.
# On a package upgrade the upgraded binary would otherwise start against the
# previous release's migration set and fail on schema it now expects. Copy
# only absent files (migration files are immutable); never rewrite the
# preserved config.
sync_legacy_migrations() {
    _section="$1"
    _packaged="$2"
    _configured=""
    [ -f /etc/chv/controlplane.toml ] || return 0
    [ -d "$_packaged" ] || return 0
    _configured="$(awk -v want="$_section" '
        /^[[:space:]]*\[/ { section=$0; gsub(/[[:space:]]/, "", section) }
        section == want && /^[[:space:]]*migrations_dir[[:space:]]*=/ {
            sub(/^[^=]*=[[:space:]]*"/, ""); sub(/".*/, ""); print; exit
        }' /etc/chv/controlplane.toml 2>/dev/null)" || true
    [ -n "$_configured" ] || return 0
    if [ "$_configured" = "$_packaged" ]; then
        return 0
    fi
    [ -d "$_configured" ] || return 0
    # Copy each absent migration individually. Existing files are left
    # untouched so the checksums recorded for already-applied migrations
    # stay valid; a bulk `cp -n` would also work but is non-portable.
    for _mig in "$_packaged"/*.sql; do
        [ -e "$_mig" ] || continue
        _name="${_mig##*/}"
        if [ -e "$_configured/$_name" ]; then
            continue
        fi
        if ! cp -a "$_mig" "$_configured/"; then
            echo "chv-controlplane: WARNING: could not copy ${_name} into ${_configured}; the upgraded control plane may fail its schema migration." >&2
        fi
    done
}
sync_legacy_migrations "[database]" /usr/share/chv/migrations
sync_legacy_migrations "[monitoring]" /usr/share/chv/monitoring-migrations

# Reload systemd so new service files are recognized
if command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload 2>/dev/null || true
fi

exit 0
