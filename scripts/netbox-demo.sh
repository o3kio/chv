#!/bin/bash
# CHV NetBox Demo — interactive NetBox-projection harness (issue #586,
# ADR-024). Boots everything a human needs to drive the NetBox
# integration end-to-end with ZERO NetBox installation:
#
#   - `chv-netbox-sim`        (stateful NetBox 4.x simulator, loopback)
#   - `chv-controlplane`      (demo-mode build: --features netbox-demo,dev)
#   - the built Web UI        (ui/build, served by the controlplane)
#   - a seeded sqlite DB      (bootstrap admin + starter topology +
#                              applied version + NetBox projection config)
#
# The controlplane is built with the double-gated plain-HTTP NetBox path
# (ADR-024 decision 5): the `netbox-demo` compile feature AND
# CHV_NETBOX_ALLOW_HTTP=1 at runtime. Both are set only for the
# controlplane process this script spawns; nothing about the demo leaks
# into any other build or deployment.
#
# Usage:
#   ./scripts/netbox-demo.sh [OPTIONS]
#   make netbox-demo                      # same thing
#
# Options:
#   --port PORT     Controlplane HTTP port (default: 18080)
#   --sim-port PORT netbox-sim port       (default: 18081)
#   --no-seed       Skip the demo architecture / projection-config
#                   seeding only (the admin user is ALWAYS seeded — the
#                   UI needs it to log in; you land on the six starters
#                   with no NetBox config)
#   --workspace DIR Reuse a workspace dir (e.g. one preserved with
#                   --keep): its sqlite DB, admin credentials, and sim
#                   token are reused and the seed steps skip what
#                   already exists. The dir is created if missing and
#                   is never deleted (implies --keep). Default: a
#                   fresh mktemp -d, removed on exit.
#   --keep          Keep the workspace on exit; re-run against it with
#                   --workspace DIR
#   --help          This help
#
# Environment:
#   CHV_NETBOX_DEMO_SKIP_BUILD=1   Skip the cargo builds (binaries must
#                                  already exist in target/debug)
#
# Prerequisites: rust toolchain, node+npm (only if ui/build is stale),
# sqlite3, openssl, curl, jq, python3 with PyYAML, and python3-bcrypt
# (or htpasswd) for the admin seed. See docs/dev/netbox-demo.md.
#
# Safety: by default everything lives in a mktemp workspace that is
# removed on exit; --keep preserves it and --workspace DIR reuses one
# (and is never deleted). Both servers bind 127.0.0.1 only; Ctrl-C
# tears everything down.

set -euo pipefail

# ---------------------------------------------------------------------------
# Helpers (repo script conventions)
# ---------------------------------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

info()    { echo "[netbox-demo] [INFO] $*"; }
warn()    { echo "[netbox-demo] [WARN] $*" >&2; }
fatal()   { echo "[netbox-demo] [FATAL] $*" >&2; exit 1; }

usage() {
    # Print the header comment (everything from line 2 up to the first
    # line that is not part of it) as the help text.
    awk 'NR == 1 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' \
        "${BASH_SOURCE[0]}"
    exit 0
}

# ---------------------------------------------------------------------------
# Options
# ---------------------------------------------------------------------------
CP_PORT="18080"
SIM_PORT="18081"
GRPC_PORT="18443"
SEED=true
KEEP=false
WORKSPACE_ARG=""
SKIP_BUILD="${CHV_NETBOX_DEMO_SKIP_BUILD:-0}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --port)       CP_PORT="${2:?--port requires a value}"; shift 2 ;;
        --sim-port)   SIM_PORT="${2:?--sim-port requires a value}"; shift 2 ;;
        --no-seed)    SEED=false; shift ;;
        --keep)       KEEP=true; shift ;;
        --workspace)  WORKSPACE_ARG="${2:?--workspace requires a value}"; shift 2 ;;
        --help|-h)    usage ;;
        *)            fatal "unknown option: $1 (try --help)" ;;
    esac
done

CP_URL="http://127.0.0.1:${CP_PORT}"
SIM_URL="http://127.0.0.1:${SIM_PORT}"

command -v cargo   >/dev/null 2>&1 || fatal "cargo not found (rust toolchain required)"
command -v sqlite3 >/dev/null 2>&1 || fatal "sqlite3 not found"
command -v curl    >/dev/null 2>&1 || fatal "curl not found"
command -v jq      >/dev/null 2>&1 || fatal "jq not found"
command -v openssl >/dev/null 2>&1 || fatal "openssl not found"
python3 -c 'import yaml' 2>/dev/null || fatal "python3 PyYAML not found (pip3 install pyyaml)"

cd "$REPO_ROOT"

# ---------------------------------------------------------------------------
# Workspace + process tracking; teardown on any exit path
# ---------------------------------------------------------------------------
# Default: a fresh mktemp -d, removed on exit. --workspace DIR reuses a
# named directory (typically one preserved by a previous --keep run):
# it is created if missing and NEVER deleted — a workspace the user
# pointed at explicitly is not ours to remove (implies --keep).
if [[ -n "$WORKSPACE_ARG" ]]; then
    mkdir -p "$WORKSPACE_ARG"
    WORKSPACE="$(cd "$WORKSPACE_ARG" && pwd)"
else
    WORKSPACE="$(mktemp -d /tmp/chv-netbox-demo.XXXXXX)"
fi
DB_PATH="${WORKSPACE}/controlplane.db"
CONFIG_PATH="${WORKSPACE}/controlplane.toml"
CRED_FILE="${WORKSPACE}/admin_password"
ENV_FILE="${WORKSPACE}/demo.env"
CP_LOG="${WORKSPACE}/controlplane.log"
SIM_LOG="${WORKSPACE}/netbox-sim.log"

# Reuse detection: a --workspace dir that carries a kept demo.env with
# a sim token. On reuse the token is re-read from that file (below) —
# the DB's stored projection config authenticates against the sim with
# that exact token, so regenerating it would invalidate the kept DB.
REUSE=false
if [[ -n "$WORKSPACE_ARG" ]] && [[ -f "$ENV_FILE" ]] && \
   grep -q '^SIM_TOKEN=' "$ENV_FILE" 2>/dev/null; then
    REUSE=true
fi

SIM_PID=""
CP_PID=""

cleanup() {
    trap - EXIT INT TERM
    info "tearing down..."
    if [[ -n "$CP_PID" ]] && kill -0 "$CP_PID" 2>/dev/null; then
        kill "$CP_PID" 2>/dev/null || true
    fi
    if [[ -n "$SIM_PID" ]] && kill -0 "$SIM_PID" 2>/dev/null; then
        kill "$SIM_PID" 2>/dev/null || true
    fi
    [[ -n "$CP_PID" ]] && wait "$CP_PID" 2>/dev/null || true
    [[ -n "$SIM_PID" ]] && wait "$SIM_PID" 2>/dev/null || true
    if [[ "$KEEP" == true || -n "$WORKSPACE_ARG" ]]; then
        info "workspace kept: ${WORKSPACE}"
        info "  controlplane log: ${CP_LOG}"
        info "  simulator log:    ${SIM_LOG}"
        info "  re-run against it: ./scripts/netbox-demo.sh --workspace ${WORKSPACE}"
    else
        rm -rf "$WORKSPACE"
        info "workspace removed"
    fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# ---------------------------------------------------------------------------
# Build (sim + demo-mode controlplane + UI if stale)
# ---------------------------------------------------------------------------
SIM_BIN="${REPO_ROOT}/target/debug/netbox-sim"
CP_BIN="${REPO_ROOT}/target/debug/chv-controlplane"

if [[ "$SKIP_BUILD" != "1" ]]; then
    info "building chv-netbox-sim (bin feature)..."
    cargo build -p chv-netbox-sim --features bin

    # netbox-demo: the plain-HTTP double gate's compile half (ADR-024).
    # dev: the controlplane runs without TLS on loopback, which the
    # CHV_ALLOW_INSECURE security gate only permits in dev-feature
    # builds (issue #233) — same rebuild contract as scripts/dev-install.sh.
    info "building chv-controlplane (features: netbox-demo,dev)..."
    cargo build -p chv-controlplane --features netbox-demo,dev
fi

[[ -x "$SIM_BIN" ]] || fatal "netbox-sim binary missing at ${SIM_BIN} (build failed?)"
[[ -x "$CP_BIN" ]]  || fatal "chv-controlplane binary missing at ${CP_BIN} (build failed?)"

UI_INDEX="${REPO_ROOT}/ui/build/index.html"
if [[ ! -f "$UI_INDEX" ]] || [[ -n "$(find "${REPO_ROOT}/ui/src" -newer "$UI_INDEX" -print -quit 2>/dev/null)" ]]; then
    info "ui/build missing or stale — building the Web UI..."
    (cd ui && npm install && npm run build)
else
    info "ui/build is up to date — skipping the UI build"
fi
[[ -f "$UI_INDEX" ]] || fatal "ui/build/index.html missing after the UI build step"

# ---------------------------------------------------------------------------
# Generated secrets + controlplane config
# ---------------------------------------------------------------------------
if [[ "$REUSE" == true ]]; then
    # Reuse the kept sim token: the stored projection-config row
    # authenticates with it, so a regenerated token would leave the
    # kept DB unable to talk to the (new) simulator process.
    SIM_TOKEN="$(grep '^SIM_TOKEN=' "$ENV_FILE" | cut -d= -f2)"
    [[ -n "$SIM_TOKEN" ]] || fatal "failed to read SIM_TOKEN from ${ENV_FILE}"
    info "reusing workspace ${WORKSPACE} (sim token re-read from ${ENV_FILE})"
else
    SIM_TOKEN="$(openssl rand -hex 24)"
fi
# The JWT secret is per-process (old sessions simply log in again) and
# the admin password is recovered from the cred file on reuse (below).
JWT_SECRET="$(openssl rand -hex 32)"
ADMIN_PASSWORD="$(openssl rand -base64 18 | tr -d '\n' | tr '+/' '-_')"

mkdir -p "${WORKSPACE}/run"

cat > "$CONFIG_PATH" <<TOML
# Generated by scripts/netbox-demo.sh — throwaway demo instance.
grpc_bind = "127.0.0.1:${GRPC_PORT}"
http_bind = "127.0.0.1:${CP_PORT}"
log_level = "info"
runtime_dir = "${WORKSPACE}/run"
jwt_secret = "${JWT_SECRET}"

[database]
url = "sqlite://${DB_PATH}"
migrations_dir = "cmd/chv-controlplane/migrations"

[webui]
enabled = true
dir = "${REPO_ROOT}/ui/build"
TOML

# Key facts for --workspace re-runs and for driving the demo from a shell.
cat > "$ENV_FILE" <<ENV
# Generated by scripts/netbox-demo.sh
CONTROLPLANE_URL=${CP_URL}
SIM_URL=${SIM_URL}
SIM_TOKEN=${SIM_TOKEN}
DB_PATH=${DB_PATH}
WORKSPACE=${WORKSPACE}
ENV
chmod 0600 "$ENV_FILE"

# ---------------------------------------------------------------------------
# Start/stop helpers. The demo uses a two-phase boot so that no external
# process ever writes the sqlite DB while the controlplane is running
# (cross-process WAL writes produced intermittent SQLITE_IOERR_SHORT_READ
# reads in the controlplane): phase 1 runs migrations + starter seeding,
# is stopped again, the demo rows are seeded offline, and phase 2 serves.
# ---------------------------------------------------------------------------
start_controlplane() {
    info "starting chv-controlplane on ${CP_URL} (NETBOX DEMO MODE: plain-HTTP NetBox client)"
    # CHV_NETBOX_ALLOW_HTTP=1 — the runtime half of the double gate. The demo
    # factory inside the controlplane fails closed without it, so the env var
    # is scoped to this process only. CHV_ALLOW_INSECURE=1 is the loopback
    # no-TLS dev gate (issue #233; requires the dev feature we built with).
    CHV_NETBOX_ALLOW_HTTP=1 CHV_ALLOW_INSECURE=1 "$CP_BIN" "$CONFIG_PATH" \
        > "$CP_LOG" 2>&1 &
    CP_PID=$!
}

stop_controlplane() {
    [[ -n "$CP_PID" ]] || return 0
    if kill -0 "$CP_PID" 2>/dev/null; then
        kill "$CP_PID" 2>/dev/null || true
        wait "$CP_PID" 2>/dev/null || true
    fi
    CP_PID=""
}

wait_for() {
    local desc="$1"; shift
    local url="$1"; shift
    local attempts=0
    until curl -sf "$@" -o /dev/null "$url" 2>/dev/null; do
        attempts=$((attempts + 1))
        if (( attempts > 60 )); then
            fatal "${desc} did not come up at ${url} — tails:
--- controlplane.log ---
$(tail -n 30 "$CP_LOG")
--- netbox-sim.log ---
$(tail -n 30 "$SIM_LOG")"
        fi
        sleep 0.5
    done
}

# ---------------------------------------------------------------------------
# Phase 1 boot: migrations + the six starter topologies, then stop again
# ---------------------------------------------------------------------------
start_controlplane
info "waiting for the controlplane /health endpoint..."
wait_for "chv-controlplane" "${CP_URL}/health"
# The HTTP listener answers /health before component construction
# finishes (the demo-mode marker is logged during component
# bootstrap), so a warm start can win the race — retry briefly before
# declaring the binary wrongly built.
if ! grep -q "NETBOX DEMO MODE" "$CP_LOG"; then
    MARKER_OK=""
    for _ in $(seq 1 20); do
        if grep -q "NETBOX DEMO MODE" "$CP_LOG"; then MARKER_OK=1; break; fi
        sleep 0.5
    done
    if [[ -z "$MARKER_OK" ]]; then
        fatal "controlplane started but the NETBOX DEMO MODE marker is missing from its log — \
the binary was probably built without --features netbox-demo"
    fi
fi
info "demo-mode marker confirmed in the controlplane log"
info "phase 1 boot done (migrations + starter topologies); stopping for offline seeding..."
stop_controlplane

# ---------------------------------------------------------------------------
# Start the simulator
# ---------------------------------------------------------------------------
info "starting netbox-sim on ${SIM_URL} (token generated; see ${ENV_FILE})"
"$SIM_BIN" --listen "127.0.0.1:${SIM_PORT}" --token "$SIM_TOKEN" \
    > "${WORKSPACE}/sim-url.txt" 2> "$SIM_LOG" &
SIM_PID=$!

info "waiting for the netbox-sim API..."
wait_for "netbox-sim" "${SIM_URL}/api/ipam/vlans/?limit=1" -H "Authorization: Token ${SIM_TOKEN}"

# ---------------------------------------------------------------------------
# Seed the bootstrap admin user (mirrors scripts/install.sh seed_admin_user)
# — offline: the controlplane is not running at this point.
# ---------------------------------------------------------------------------
seed_admin_user() {
    info "seeding bootstrap admin user (sqlite direct, bcrypt cost 12)..."

    # Idempotent: an existing admin row (a --workspace re-run) is left
    # alone so the stored password keeps working; recover it from the
    # 0600 cred file.
    if [[ -f "$DB_PATH" ]] && \
       [[ "$(sqlite3 "$DB_PATH" "SELECT COUNT(*) FROM users WHERE username = 'admin';" 2>/dev/null)" == "1" ]]; then
        if [[ -r "$CRED_FILE" ]]; then
            ADMIN_PASSWORD="$(cat "$CRED_FILE")"
            info "admin user already exists — reusing (password recovered from ${CRED_FILE})"
            return 0
        fi
        fatal "admin user already exists but ${CRED_FILE} is unreadable — cannot recover the \
password. Remove the workspace (drop --keep) or set a new password via the UI."
    fi

    local hashed_pw=""
    if python3 -c 'import bcrypt' 2>/dev/null; then
        hashed_pw=$(python3 -c '
import bcrypt, sys
pw = sys.argv[1].encode("utf-8")
print(bcrypt.hashpw(pw, bcrypt.gensalt(rounds=12)).decode("utf-8"))
' "$ADMIN_PASSWORD")
    elif command -v htpasswd >/dev/null 2>&1; then
        hashed_pw=$(htpasswd -nbBC 12 admin "$ADMIN_PASSWORD" | sed 's/^admin://')
    else
        fatal "Neither python3-bcrypt nor htpasswd available — cannot bcrypt the admin password. \
Install one of: 'pip3 install bcrypt' or 'apt install apache2-utils' (Debian/Ubuntu) / 'dnf install httpd-tools' (RHEL/Fedora)."
    fi
    [[ -n "$hashed_pw" ]] || fatal "bcrypt produced an empty hash — refusing to seed"

    local admin_user_id="00000000-0000-0000-0000-000000000001"
    sqlite3 "$DB_PATH" <<SQL
INSERT INTO users (user_id, username, password_hash, role, display_name, must_change_password, created_at, updated_at)
VALUES ('${admin_user_id}', 'admin', '${hashed_pw}', 'admin', 'Administrator', 0,
        strftime('%Y-%m-%dT%H:%M:%SZ','now'),
        strftime('%Y-%m-%dT%H:%M:%SZ','now'));
SQL

    install -m 0600 /dev/null "$CRED_FILE"
    printf '%s\n' "$ADMIN_PASSWORD" > "$CRED_FILE"
    chmod 0600 "$CRED_FILE"
}

# ---------------------------------------------------------------------------
# Login helper → JWT
# ---------------------------------------------------------------------------
login() {
    local body status
    body="$(curl -s -w '\n%{http_code}' -H 'Content-Type: application/json' \
        -X POST "${CP_URL}/api/v1/auth/login" \
        -d "{\"username\":\"admin\",\"password\":\"${ADMIN_PASSWORD}\"}")"
    status="$(echo "$body" | tail -n1)"
    body="$(echo "$body" | sed '$d')"
    if [[ "$status" != "200" ]]; then
        warn "login failed (HTTP ${status}): ${body}"
        return 1
    fi
    echo "$body" | jq -r '.token'
}

# ---------------------------------------------------------------------------
# Seed the demo architecture + projection config (--seed, default on) —
# offline: the controlplane is stopped, the sqlite DB is ours alone.
# ---------------------------------------------------------------------------
seed_projection() {
    # 1. Pick the first starter architecture (seeded by the phase-1 boot;
    #    ids are the seeder's deterministic starter-NN-slug ids).
    local arch_id version_number
    arch_id="$(sqlite3 "$DB_PATH" "SELECT id FROM architecture_topologies WHERE id LIKE 'starter-%' ORDER BY id LIMIT 1;")"
    if [[ -z "$arch_id" ]]; then
        warn "no starter architecture found — nothing to seed. The UI is still usable; \
create an architecture and configure NetBox manually."
        return 0
    fi
    version_number="$(sqlite3 "$DB_PATH" "SELECT version_number FROM architecture_topologies WHERE id = '${arch_id}';")"
    info "demo architecture: ${arch_id} (topology version ${version_number})"

    # 2. Normalize the starter's YAML into the model JSON the projection
    #    worker consumes (architecture_versions.normalized_model_json).
    sqlite3 "$DB_PATH" "SELECT latest_yaml FROM architecture_topologies WHERE id = '${arch_id}';" \
        > "${WORKSPACE}/starter.yaml"
    [[ -s "${WORKSPACE}/starter.yaml" ]] || fatal "starter ${arch_id} has no latest_yaml"
    python3 -c 'import yaml, json, sys; print(json.dumps(yaml.safe_load(sys.stdin.read())))' \
        < "${WORKSPACE}/starter.yaml" > "${WORKSPACE}/model.json"
    [[ -s "${WORKSPACE}/model.json" ]] || fatal "YAML→model-JSON normalization produced nothing"

    # 3. Seed an applied version + succeeded apply run + the projection
    #    config pointing at the simulator. The projection projects the
    #    most recent *succeeded apply run's* version — never the editable
    #    draft — and a starter is born draft-only with no fleet to apply
    #    against, so the demo stands in for the apply step the same way
    #    the composed test suites do (seeded apply run).
    #
    #    The projection config row is written directly because the BFF's
    #    config/upsert is the accept-time HTTPS gate
    #    (NETBOX_HTTPS_REQUIRED) and rejects the simulator's http://
    #    endpoint by design; the store layer persists the endpoint
    #    verbatim (its documented contract). CHV_ENCRYPTION_KEY is
    #    deliberately NOT set for the controlplane, so the credential
    #    store keeps tokens in plaintext (its warned, documented no-key
    #    behavior) and this seed can store the sim token directly.
    DEMO_ARCH_ID="$arch_id" DEMO_VERSION_NUMBER="$version_number" \
    DEMO_WORKSPACE="$WORKSPACE" DEMO_SIM_URL="$SIM_URL" DEMO_SIM_TOKEN="$SIM_TOKEN" \
    python3 - <<'PYEOF'
import os
import sqlite3

arch_id = os.environ["DEMO_ARCH_ID"]
version_number = int(os.environ["DEMO_VERSION_NUMBER"])
ws = os.environ["DEMO_WORKSPACE"]
sim_url = os.environ["DEMO_SIM_URL"]
sim_token = os.environ["DEMO_SIM_TOKEN"]

db = sqlite3.connect(os.path.join(ws, "controlplane.db"))
cur = db.cursor()

# Idempotent on --workspace re-runs: one demo version + one demo apply run.
cur.execute(
    "SELECT id FROM architecture_versions WHERE architecture_id = ? AND change_summary = 'netbox-demo'",
    (arch_id,),
)
row = cur.fetchone()
if row is None:
    version_id = f"demover-{os.urandom(6).hex()}"
    with open(os.path.join(ws, "starter.yaml")) as f:
        yaml_content = f.read()
    with open(os.path.join(ws, "model.json")) as f:
        model_json = f.read()
    cur.execute(
        "INSERT INTO architecture_versions "
        "(id, architecture_id, version_number, yaml_content, design_graph_json, "
        " normalized_model_json, change_summary, created_by) "
        "VALUES (?, ?, ?, ?, NULL, ?, 'netbox-demo', 'netbox-demo')",
        (version_id, arch_id, version_number, yaml_content, model_json),
    )
    cur.execute(
        "UPDATE architecture_topologies SET latest_version_id = ? WHERE id = ?",
        (version_id, arch_id),
    )
    cur.execute(
        "INSERT INTO architecture_apply_runs "
        "(id, architecture_id, architecture_version_id, status, started_at, finished_at, requested_by) "
        "VALUES (?, ?, ?, 'succeeded', "
        " strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'), 'netbox-demo')",
        (f"demoapply-{os.urandom(6).hex()}", arch_id, version_id),
    )
    db.commit()
    print(f"[netbox-demo] [INFO] seeded demo version {version_id} + succeeded apply run")
else:
    print(f"[netbox-demo] [INFO] demo version {row[0]} already seeded — reusing")

cur.execute(
    "SELECT architecture_id FROM netbox_projection_config WHERE architecture_id = ?",
    (arch_id,),
)
if cur.fetchone() is None:
    cur.execute(
        "INSERT INTO netbox_projection_config "
        "(architecture_id, endpoint, token_secret_ref, token_ciphertext, "
        " retention_policy, enable_post_apply, custom_field_prefix, site_name) "
        "VALUES (?, ?, 'netbox-demo', ?, 'mark_stale', 0, 'chv_', 'chv-demo')",
        (arch_id, sim_url, sim_token),
    )
    db.commit()
    print("[netbox-demo] [INFO] seeded netbox projection config pointing at the simulator")
else:
    # Keep the stored config in lockstep with this run's sim: the
    # token is re-read from demo.env on workspace reuse (so normally a
    # no-op), and the endpoint is refreshed in case the sim port
    # changed. Without this, a stale row would 401 against the new
    # simulator process.
    cur.execute(
        "UPDATE netbox_projection_config SET endpoint = ?, token_ciphertext = ? "
        "WHERE architecture_id = ?",
        (sim_url, sim_token, arch_id),
    )
    db.commit()
    print("[netbox-demo] [INFO] netbox projection config already seeded — reusing (endpoint/token synced)")
db.close()
PYEOF

    # Persist the chosen architecture for --workspace re-runs / shell driving.
    echo "ARCH_ID=${arch_id}" >> "$ENV_FILE"
}

# ---------------------------------------------------------------------------
# Post-boot verification through the BFF (read-only)
# ---------------------------------------------------------------------------
verify_projection() {
    info "logging in as the seeded admin..."
    local token
    token="$(login)" || fatal "login failed — cannot verify the demo seed"
    [[ -n "$token" && "$token" != "null" ]] || fatal "login returned no token — cannot verify the demo seed"

    local arch_id
    arch_id="$(grep '^ARCH_ID=' "$ENV_FILE" | cut -d= -f2)"

    # The BFF's architecture list must show the seeded starter.
    local listed
    listed="$(curl -s -H "Authorization: Bearer ${token}" -H 'Content-Type: application/json' \
        -X POST "${CP_URL}/v1/architectures/list" -d '{}' \
        | jq -r --arg id "$arch_id" '.architectures[] | select(.id == $id) | .id // empty')"
    [[ "$listed" == "$arch_id" ]] || fatal "architecture ${arch_id} not returned by the BFF list"

    # config/get must show the seeded config with a token set.
    local body status attempt
    for attempt in 1 2 3; do
        body="$(curl -s -w '\n%{http_code}' -H "Authorization: Bearer ${token}" -H 'Content-Type: application/json' \
            -X POST "${CP_URL}/v1/architectures/netbox/config/get" -d "{\"id\":\"${arch_id}\"}")"
        status="$(echo "$body" | tail -n1)"
        body="$(echo "$body" | sed '$d')"
        if [[ "$status" == "200" && "$(echo "$body" | jq -r '.token_set')" == "true" ]]; then
            info "verified via the BFF: ${arch_id} listed, projection config present, token_set=true"
            return 0
        fi
        sleep 1
    done
    fatal "config/get did not report token_set=true (last HTTP ${status}): ${body}"
}

# ---------------------------------------------------------------------------
# Offline seeding (controlplane stopped, DB ours alone), then phase 2 boot
# ---------------------------------------------------------------------------
seed_admin_user
if [[ "$SEED" == true ]]; then
    seed_projection
else
    info "--no-seed: skipping the demo architecture / projection-config seeding \
(the admin user is still seeded — the UI needs it to log in)"
fi

start_controlplane
info "waiting for the controlplane /health endpoint..."
wait_for "chv-controlplane" "${CP_URL}/health"
if [[ "$SEED" == true ]]; then
    verify_projection
fi

# ---------------------------------------------------------------------------
# Tell the human what to do
# ---------------------------------------------------------------------------
WORKSPACE_NOTE="(removed on exit; --keep to preserve)"
if [[ "$REUSE" == true ]]; then
    WORKSPACE_NOTE="(reused workspace)"
elif [[ -n "$WORKSPACE_ARG" ]]; then
    WORKSPACE_NOTE="(new named workspace — never removed on exit)"
fi
if [[ "$KEEP" == true || -n "$WORKSPACE_ARG" ]]; then
    WORKSPACE_NOTE="${WORKSPACE_NOTE} — kept on exit; re-run with: ./scripts/netbox-demo.sh --workspace ${WORKSPACE}"
fi

cat <<BANNER

==============================================================
 CHV NetBox demo is up (ADR-024, issue #586)
==============================================================

  Web UI:            ${CP_URL}
  Demo credentials:  admin / ${ADMIN_PASSWORD}
                     (also stored 0600 at ${CRED_FILE})

  NetBox simulator:  ${SIM_URL}   (token: ${SIM_TOKEN})
  Simulator state:   curl -s ${SIM_URL}/__state | jq .
  Workspace:         ${WORKSPACE} ${WORKSPACE_NOTE}

  Click-path (what this demo proves):
    1. Open ${CP_URL} and log in with the demo credentials.
    2. Architectures -> open the seeded starter topology
       ($(grep '^ARCH_ID=' "$ENV_FILE" 2>/dev/null | cut -d= -f2 || echo 'see demo.env'))    3. NetBox tab -> Configure: the projection config is ALREADY seeded
       (endpoint = the simulator, token set, mark_stale retention).
    4. Dry run -> see the projection plan (creates/updates/no-ops).
    5. Export -> enqueues a run; Run history shows it Succeed.
    6. Inspect what landed in "NetBox":
         curl -s ${SIM_URL}/__state | jq '.objects.virtual_machines'

  Fault simulation (the simulator's __-prefixed control plane):
    # Force 401 auth failures on every NetBox call, then Dry run again:
    curl -s -X POST ${SIM_URL}/__faults \\
         -H 'Content-Type: application/json' -d '{"auth_failure": true}' | jq .
    # Simulate a NetBox outage (5xx) for one kind only:
    curl -s -X POST ${SIM_URL}/__faults \\
         -H 'Content-Type: application/json' -d '{"kind": "virtual_machine", "server_error": 503}' | jq .
    # Add latency, or drop connections mid-response:
    curl -s -X POST ${SIM_URL}/__faults \\
         -H 'Content-Type: application/json' -d '{"latency_ms": 2000}' | jq .
    # Clear the GLOBAL fault config ('{}' replaces it with all-false).
    # NOTE: per-kind entries are NOT touched by this — clear those
    # separately (next example):
    curl -s -X POST ${SIM_URL}/__faults \\
         -H 'Content-Type: application/json' -d '{}' | jq .
    # Clear a PER-KIND fault: an all-false entry for that kind replaces
    # whatever the global config says for it:
    curl -s -X POST ${SIM_URL}/__faults \\
         -H 'Content-Type: application/json' -d '{"kind": "virtual_machine"}' | jq .
    # Nuclear option — clear faults AND every object in the simulator:
    curl -s -X POST ${SIM_URL}/__reset | jq .

  Press Ctrl-C to tear everything down.
==============================================================
BANNER

# Foreground the demo: block on the controlplane so Ctrl-C reaches this
# script's traps and both processes are torn down cleanly.
wait "$CP_PID"
