#!/bin/bash
# Entrypoint for the #549 converged container package-smoke image
# (packaging/container/Dockerfile.webui-smoke). Starts the packaged
# chv-controlplane with [webui] enabled behind the packaged proxy-only
# nginx edge, seeds a bootstrap admin (the quick-install contract), and
# starts the packaged chv-agent so the edge's /ws/ console proxy has a
# real upstream. NOT a production entrypoint: smoke-only judgments
# (baked secrets via generated-at-start values, no systemd, no
# enrollment) are disclosed in the Dockerfile and the smoke script.
#
# Environment:
#   CHV_SMOKE_ADMIN_PASSWORD - the admin password to seed (set by the
#                              smoke driver so it can drive login; no
#                              default — the entrypoint generates one if
#                              unset so manual `docker run` still works).

set -euo pipefail

CHV_USER="chv"
CHV_CONFIG_DIR="/etc/chv"
CHV_DATA_DIR="/var/lib/chv"
CHV_RUN_DIR="/run/chv"
CHV_UI_DIR="/usr/share/chv/ui"
CHV_DB_PATH="${CHV_DATA_DIR}/controlplane.db"
CHV_MIGRATIONS_DIR="/usr/share/chv/migrations"
LOG_DIR="/var/log/chv"

log() { echo "[webui-smoke-entrypoint] $*"; }

mkdir -p "${LOG_DIR}"

# ---------------------------------------------------------------------------
# Runtime/state directories the systemd units would create
# (RuntimeDirectory=/StateDirectory= equivalents, package-layout parity).
# ---------------------------------------------------------------------------
install -d -m 0755 -o root -g root "${CHV_RUN_DIR}"
install -d -m 0700 -o "${CHV_USER}" -g "${CHV_USER}" \
    "${CHV_RUN_DIR}/controlplane" "${CHV_RUN_DIR}/core" "${CHV_RUN_DIR}/agent" \
    "${CHV_RUN_DIR}/stord" "${CHV_RUN_DIR}/nwd"
install -d -m 0755 -o "${CHV_USER}" -g "${CHV_USER}" "${CHV_DATA_DIR}"
install -d -m 0700 -o "${CHV_USER}" -g "${CHV_USER}" \
    "${CHV_DATA_DIR}/agent" "${CHV_DATA_DIR}/cache" "${CHV_DATA_DIR}/storage/localdisk"

# ---------------------------------------------------------------------------
# TLS material (quick-install.sh generate_certs, verbatim shapes: CA,
# server cert with localhost/127.0.0.1 SANs). Smoke-only: regenerated at
# every container start; a real deployment's certs are operator-owned.
# ---------------------------------------------------------------------------
CERT_DIR="${CHV_CONFIG_DIR}/certs"
install -d -m 0750 -o root -g "${CHV_USER}" "${CERT_DIR}"

openssl genrsa -out "${CERT_DIR}/ca.key" 4096 2>/dev/null
openssl req -x509 -new -nodes -key "${CERT_DIR}/ca.key" \
    -sha256 -days 3650 -out "${CERT_DIR}/ca.crt" \
    -subj "/O=CHV-smoke/CN=chv-ca" \
    -addext "basicConstraints=critical,CA:TRUE" \
    -addext "keyUsage=critical,keyCertSign,cRLSign" 2>/dev/null
openssl genrsa -out "${CERT_DIR}/server.key" 2048 2>/dev/null
openssl req -new -key "${CERT_DIR}/server.key" \
    -out "${CERT_DIR}/server.csr" \
    -subj "/O=CHV-smoke/CN=chv-controlplane" 2>/dev/null
openssl x509 -req -in "${CERT_DIR}/server.csr" \
    -CA "${CERT_DIR}/ca.crt" -CAkey "${CERT_DIR}/ca.key" \
    -CAcreateserial -out "${CERT_DIR}/server.crt" \
    -days 825 -sha256 \
    -extfile <(printf "subjectAltName=DNS:localhost,IP:127.0.0.1\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth")
rm -f "${CERT_DIR}/server.csr"
chown root:"${CHV_USER}" "${CERT_DIR}"/*
chmod 640 "${CERT_DIR}/ca.key" "${CERT_DIR}/server.key"
chmod 644 "${CERT_DIR}/ca.crt" "${CERT_DIR}/server.crt"

# Agent client cert (same CA, clientAuth EKU). The agent's enrollment
# flow would normally mint this against the control plane; this stack
# never enrolls (the agent runs disconnected by design), so the config's
# tls_cert_path/tls_key_path would otherwise dangle — generated here so
# the config is self-consistent and a future startup validation of the
# cert files cannot silently break the /ws/ leg.
openssl genrsa -out "${CHV_RUN_DIR}/agent/agent.key" 2048 2>/dev/null
openssl req -new -key "${CHV_RUN_DIR}/agent/agent.key" \
    -out "${CERT_DIR}/agent.csr" \
    -subj "/O=CHV-smoke/CN=chv-agent-smoke" 2>/dev/null
openssl x509 -req -in "${CERT_DIR}/agent.csr" \
    -CA "${CERT_DIR}/ca.crt" -CAkey "${CERT_DIR}/ca.key" \
    -CAcreateserial -out "${CHV_RUN_DIR}/agent/agent.crt" \
    -days 825 -sha256 \
    -extfile <(printf "keyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=clientAuth")
rm -f "${CERT_DIR}/agent.csr"
chown "${CHV_USER}:${CHV_USER}" "${CHV_RUN_DIR}/agent/agent.key" "${CHV_RUN_DIR}/agent/agent.crt"
chmod 600 "${CHV_RUN_DIR}/agent/agent.key"
chmod 644 "${CHV_RUN_DIR}/agent/agent.crt"

# ---------------------------------------------------------------------------
# Control plane config — the converged package-mode shape: the HTTP
# listener binds loopback BEHIND the edge (quick-install's 0.0.0.0
# judgment does not apply here; this image HAS an edge), [webui] enabled
# against the packaged tree. jwt_secret is generated at start (smoke
# only — a deployment's secret is operator-owned).
# ---------------------------------------------------------------------------
JWT_SECRET="$(openssl rand -base64 32 | tr -d '=+/')"

cat > "${CHV_CONFIG_DIR}/controlplane.toml" <<EOF
# Generated by the #549 webui package-smoke entrypoint (container only).
grpc_bind = "127.0.0.1:8443"
http_bind = "127.0.0.1:8080"
# Dedicated Prometheus listener (unauthenticated, loopback).
metrics_bind = "127.0.0.1:9103"
log_level = "info"
runtime_dir = "${CHV_RUN_DIR}/controlplane"
jwt_secret = "${JWT_SECRET}"

[database]
url = "sqlite://${CHV_DB_PATH}"
migrations_dir = "${CHV_MIGRATIONS_DIR}"
max_connections = 4
min_connections = 1
acquire_timeout_secs = 5

[tls]
ca_cert_path = "${CERT_DIR}/ca.crt"
ca_key_path = "${CERT_DIR}/ca.key"
server_cert_path = "${CERT_DIR}/server.crt"
server_key_path = "${CERT_DIR}/server.key"
client_ca_path = "${CERT_DIR}/ca.crt"

[webui]
enabled = true
dir = "${CHV_UI_DIR}"
EOF
chown root:"${CHV_USER}" "${CHV_CONFIG_DIR}/controlplane.toml"
chmod 640 "${CHV_CONFIG_DIR}/controlplane.toml"

# Agent config — quick-install.sh's shape, minus enrollment (the smoke
# never enrolls the node; the agent runs disconnected, retrying the
# control plane — its console listener on 8444 is what the edge's /ws/
# proxy needs as a real upstream). The client cert the enrollment flow
# would mint is generated above so tls_cert_path/tls_key_path resolve.
CHV_NODE_ID="$(cat /proc/sys/kernel/random/uuid)"
cat > "${CHV_CONFIG_DIR}/agent.toml" <<EOF
# Generated by the #549 webui package-smoke entrypoint (container only).
socket_path = "${CHV_RUN_DIR}/agent/api.sock"
runtime_dir = "${CHV_DATA_DIR}/agent"
log_level = "info"
control_plane_addr = "https://127.0.0.1:8443"
stord_socket = "${CHV_RUN_DIR}/stord/api.sock"
nwd_socket = "${CHV_RUN_DIR}/nwd/api.sock"
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
tls_cert_path = "${CHV_RUN_DIR}/agent/agent.crt"
tls_key_path = "${CHV_RUN_DIR}/agent/agent.key"
ca_cert_path = "${CERT_DIR}/ca.crt"
console_bind = "127.0.0.1:8444"
jwt_secret = "${JWT_SECRET}"

stord_path_allowlist = ["${CHV_DATA_DIR}/storage/localdisk", "${CHV_DATA_DIR}/storage/lvm", "${CHV_DATA_DIR}/agent"]

stord_config_path = "${CHV_CONFIG_DIR}/stord.toml"
nwd_config_path = "${CHV_CONFIG_DIR}/nwd.toml"
EOF
chown root:"${CHV_USER}" "${CHV_CONFIG_DIR}/agent.toml"
chmod 640 "${CHV_CONFIG_DIR}/agent.toml"

# stord/nwd configs (the packaged example conffiles already exist at
# /etc/chv; they point sockets at /run/chv — usable as-is. If the
# packaged examples moved, regenerate the quick-install shapes.)
if [ ! -f "${CHV_CONFIG_DIR}/stord.toml" ] || ! grep -q "socket_path" "${CHV_CONFIG_DIR}/stord.toml"; then
    cat > "${CHV_CONFIG_DIR}/stord.toml" <<EOF
socket_path = "${CHV_RUN_DIR}/stord/api.sock"
runtime_dir = "${CHV_DATA_DIR}/storage/localdisk"
log_level = "info"
# Dedicated Prometheus listener (unauthenticated, loopback).
metrics_bind = "127.0.0.1:9101"
path_allowlist = ["${CHV_DATA_DIR}/storage/localdisk", "${CHV_DATA_DIR}/storage/lvm", "${CHV_DATA_DIR}/agent"]
device_allowlist = ["/dev/dm-*", "/dev/mapper/*"]
EOF
    chown root:chv-stord "${CHV_CONFIG_DIR}/stord.toml"
    chmod 640 "${CHV_CONFIG_DIR}/stord.toml"
fi
if [ ! -f "${CHV_CONFIG_DIR}/nwd.toml" ] || ! grep -q "socket_path" "${CHV_CONFIG_DIR}/nwd.toml"; then
    cat > "${CHV_CONFIG_DIR}/nwd.toml" <<EOF
socket_path = "${CHV_RUN_DIR}/nwd/api.sock"
runtime_dir = "${CHV_RUN_DIR}/nwd"
log_level = "info"
# Dedicated Prometheus listener (unauthenticated, loopback).
metrics_bind = "127.0.0.1:9102"
EOF
    chown root:"${CHV_USER}" "${CHV_CONFIG_DIR}/nwd.toml"
    chmod 640 "${CHV_CONFIG_DIR}/nwd.toml"
fi

# ---------------------------------------------------------------------------
# Start the control plane (the unit's ExecStart shape, runuser instead
# of systemd).
# ---------------------------------------------------------------------------
log "starting chv-controlplane"
runuser -u "${CHV_USER}" -- /usr/bin/chv-controlplane "${CHV_CONFIG_DIR}/controlplane.toml" \
    > "${LOG_DIR}/controlplane.log" 2>&1 &

for i in $(seq 1 60); do
    if curl -sf "http://127.0.0.1:8080/health" > /dev/null 2>&1; then
        log "control plane healthy (migrations applied)"
        break
    fi
    if [ "$i" -eq 60 ]; then
        log "FATAL: control plane did not become healthy; last log lines:"
        tail -n 40 "${LOG_DIR}/controlplane.log" || true
        exit 1
    fi
    sleep 1
done

# ---------------------------------------------------------------------------
# Seed the bootstrap admin (quick-install.sh seed_admin_user contract:
# bcrypt cost 12, must_change_password=1). sqlite3 runs as the service
# user — the live WAL database must never be written as root.
# ---------------------------------------------------------------------------
ADMIN_PASSWORD="${CHV_SMOKE_ADMIN_PASSWORD:-$(openssl rand -base64 18 | tr -d '\n' | tr '+/' '-_')}"
HASHED_PW="$(htpasswd -nbBC 12 admin "${ADMIN_PASSWORD}" | sed 's/^admin://')"
ADMIN_USER_ID="00000000-0000-0000-0000-000000000001"

runuser -u "${CHV_USER}" -- sqlite3 "${CHV_DB_PATH}" <<SQL
INSERT INTO users (user_id, username, password_hash, role, display_name, must_change_password, created_at, updated_at)
VALUES ('${ADMIN_USER_ID}', 'admin', '${HASHED_PW}', 'admin', 'Administrator', 1,
        strftime('%Y-%m-%dT%H:%M:%SZ','now'),
        strftime('%Y-%m-%dT%H:%M:%SZ','now'));
SQL
log "bootstrap admin seeded"

# ---------------------------------------------------------------------------
# Start the agent (console listener on 8444 = the edge /ws/ upstream).
# Best-effort with a bounded wait: the agent runs disconnected (no
# enrollment), which its loop tolerates by design.
# ---------------------------------------------------------------------------
log "starting chv-agent"
runuser -u "${CHV_USER}" -- /usr/bin/chv-agent "${CHV_CONFIG_DIR}/agent.toml" \
    > "${LOG_DIR}/agent.log" 2>&1 &

# ---------------------------------------------------------------------------
# Start the edge (the packaged example conf was installed into
# sites-enabled at image build).
# ---------------------------------------------------------------------------
log "starting nginx edge"
nginx -g "daemon off;" > "${LOG_DIR}/nginx.log" 2>&1 &

log "webui-smoke stack up: edge :80 -> controlplane 127.0.0.1:8080 ([webui] enabled), /ws/ -> agent 127.0.0.1:8444"

# Keep the container alive; the smoke driver asserts from outside.
exec tail -f /dev/null
