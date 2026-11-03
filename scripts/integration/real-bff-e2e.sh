#!/usr/bin/env bash
# real-bff-e2e.sh — run a browser/API flow against the REAL Rust control-plane
# BFF, not ui/tests/e2e/mock-bff.mjs.
#
# Why this exists (#659): the Playwright suite intercepts /v1/** with static
# fixtures, so it cannot catch wire-shape drift between the SvelteKit UI and
# the Rust BFF's actual serde structs. This harness boots the real binary
# against a disposable SQLite database, seeds one admin user, waits for the
# BFF to be ready, then runs the caller's command (normally the
# `real-bff.spec.ts` Playwright flow) with REAL_BFF_BASE_URL +
# REAL_BFF_E2E_USER/PASSWORD exported.
#
# Security posture: this is a loopback-only, ephemeral test instance. It sets
# CHV_ALLOW_INSECURE=1, which is accepted ONLY by a control-plane built with
# the `dev` Cargo feature (the fail-closed peer-identity gate). Do not reuse
# this config outside tests. The password is generated per run and never
# logged; only its bcrypt hash is written to the database.
#
# Requirements: a `chv-controlplane` built with `--features dev`; python3
# (sqlite3 stdlib) plus either python3-bcrypt or htpasswd (apache2-utils);
# curl. No repository secrets.
#
# Usage:
#   ./real-bff-e2e.sh --binary target/debug/chv-controlplane -- \
#       bash -c 'cd ui && npx playwright test tests/e2e/real-bff.spec.ts'
#
# Environment:
#   REAL_BFF_PORT            HTTP port (default 18080)
#   REAL_BFF_GRPC_PORT       gRPC port (default 18443)
#   REAL_BFF_E2E_USER        seeded username (default "admin")
#   REAL_BFF_E2E_PASSWORD    seeded password (default: random per run)
#   REAL_BFF_ARTIFACT_DIR    control-plane log output (default artifacts/real-bff)
#   REAL_BFF_STARTUP_BUDGET  readiness budget in seconds (default 60)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

BINARY="${REPO_ROOT}/target/debug/chv-controlplane"
PORT="${REAL_BFF_PORT:-18080}"
GRPC_PORT="${REAL_BFF_GRPC_PORT:-18443}"
USERNAME="${REAL_BFF_E2E_USER:-admin}"
PASSWORD="${REAL_BFF_E2E_PASSWORD:-}"
ARTIFACT_DIR="${REAL_BFF_ARTIFACT_DIR:-${REPO_ROOT}/artifacts/real-bff}"
STARTUP_BUDGET="${REAL_BFF_STARTUP_BUDGET:-60}"
COMMAND=()

fail() {
    echo "::error::real-bff-e2e: $*" >&2
    exit 1
}

usage() {
    sed -n '2,45p' "$0" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
    case "$1" in
        --binary) BINARY="${2:?--binary requires a path}"; shift 2 ;;
        --port) PORT="${2:?--port requires a value}"; shift 2 ;;
        --grpc-port) GRPC_PORT="${2:?--grpc-port requires a value}"; shift 2 ;;
        --artifact-dir) ARTIFACT_DIR="${2:?--artifact-dir requires a path}"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        --) shift; COMMAND=("$@"); break ;;
        *) fail "unknown argument: $1 (use -- before the command)" ;;
    esac
done

[ "${#COMMAND[@]}" -gt 0 ] || fail "no command given (usage: $0 --binary PATH -- COMMAND...)"
[ -x "$BINARY" ] || fail "control-plane binary not executable: ${BINARY} (build it with --features dev)"
command -v python3 >/dev/null 2>&1 || fail "python3 is required (stdlib sqlite3)"
command -v curl >/dev/null 2>&1 || fail "curl is required for readiness/login checks"

case "$ARTIFACT_DIR" in
    /*) ;;
    *) ARTIFACT_DIR="${REPO_ROOT}/${ARTIFACT_DIR}" ;;
esac
mkdir -p "$ARTIFACT_DIR"
CP_LOG="${ARTIFACT_DIR}/controlplane.log"
HARNESS_LOG="${ARTIFACT_DIR}/harness.log"
: > "$CP_LOG"
: > "$HARNESS_LOG"

ROOT="$(mktemp -d /tmp/chv-real-bff-XXXXXX)"
DB="${ROOT}/controlplane.db"
CONFIG="${ROOT}/controlplane.toml"
CP_PID=""

log() {
    printf '[real-bff] %s\n' "$*" | tee -a "$HARNESS_LOG"
}

stop_cp() {
    [ -n "$CP_PID" ] || return 0
    if kill -0 "$CP_PID" 2>/dev/null; then
        kill "$CP_PID" 2>/dev/null || true
        for _ in $(seq 1 100); do
            kill -0 "$CP_PID" 2>/dev/null || break
            sleep 0.1
        done
        if kill -0 "$CP_PID" 2>/dev/null; then
            kill -9 "$CP_PID" 2>/dev/null || true
        fi
        wait "$CP_PID" 2>/dev/null || true
    fi
    CP_PID=""
}

# shellcheck disable=SC2317  # invoked indirectly by the EXIT trap below.
cleanup() {
    stop_cp
    rm -rf "$ROOT"
}
trap cleanup EXIT

hash_password() {
    local password="$1"
    if python3 -c 'import bcrypt' >/dev/null 2>&1; then
        python3 - "$password" <<'PY'
import bcrypt, sys
print(bcrypt.hashpw(sys.argv[1].encode("utf-8"), bcrypt.gensalt(rounds=12)).decode("utf-8"))
PY
    elif command -v htpasswd >/dev/null 2>&1; then
        # htpasswd emits "$2y$"; the Rust verifier accepts it identically to
        # "$2b$" but normalize anyway to match install.sh's contract.
        # shellcheck disable=SC2016  # $2y$/$2b$ are sed anchors, not shell vars.
        htpasswd -bnBC 10 "" "$password" 2>/dev/null | tr -d ':\n' | sed 's/^\$2y\$/\$2b\$/'
    else
        return 1
    fi
}

if [ -z "$PASSWORD" ]; then
    PASSWORD="$(python3 -c 'import secrets; print(secrets.token_urlsafe(24))')"
fi

cat > "$CONFIG" <<EOF
grpc_bind = "127.0.0.1:${GRPC_PORT}"
http_bind = "127.0.0.1:${PORT}"
log_level = "info"
runtime_dir = "${ROOT}/cp-run"
jwt_secret = "real-bff-e2e-only-secret-not-for-production"

# Disposable SQLite is the whole persistence layer for this lane; migrations
# are the checked-in production set so the schema matches a real install.
[database]
url = "sqlite://${DB}"
migrations_dir = "${REPO_ROOT}/cmd/chv-controlplane/migrations"
max_connections = 1
min_connections = 1
acquire_timeout_secs = 5

# Telemetry is not under test here and would add a second SQLite file.
[monitoring]
enabled = false
EOF

start_cp() {
    log "starting control plane (http 127.0.0.1:${PORT})"
    CHV_ALLOW_INSECURE=1 "$BINARY" "$CONFIG" >>"$CP_LOG" 2>&1 &
    CP_PID=$!

    local deadline=$((SECONDS + STARTUP_BUDGET))
    while [ "$SECONDS" -lt "$deadline" ]; do
        if ! kill -0 "$CP_PID" 2>/dev/null; then
            tail -n 40 "$CP_LOG" >&2 || true
            fail "control plane exited during startup (log: ${CP_LOG}). Is the binary built with --features dev?"
        fi
        if curl -fsS "http://127.0.0.1:${PORT}/health" >/dev/null 2>&1; then
            log "control plane ready"
            return 0
        fi
        sleep 0.5
    done
    tail -n 40 "$CP_LOG" >&2 || true
    fail "control plane did not become ready within ${STARTUP_BUDGET}s (log: ${CP_LOG})"
}

# First boot runs the full migration set. The HTTP listener binds only after
# migrations + seed complete, so a successful /health means the users table
# exists.
start_cp

# Seed inside a clean stop window (the WAL-unlink hazard documented in
# scripts/integration/qual/lib.sh): never write with a read-write connection
# while the control plane holds the database open.
log "stopping control plane for the seeding window"
stop_cp

HASH="$(hash_password "$PASSWORD")" || fail "no bcrypt implementation available (install python3-bcrypt or apache2-utils for htpasswd)"
[ -n "$HASH" ] || fail "bcrypt hash generation failed"
python3 - "$DB" "$USERNAME" "$HASH" <<'PY'
import sqlite3, sys
db, username, password_hash = sys.argv[1], sys.argv[2], sys.argv[3]
conn = sqlite3.connect(db)
conn.execute(
    "INSERT OR REPLACE INTO users "
    "(user_id, username, password_hash, role, must_change_password) "
    "VALUES ('real-bff-e2e-admin', ?, ?, 'admin', 0)",
    (username, password_hash),
)
conn.commit()
conn.close()
PY
log "seeded admin user (bcrypt, must_change_password=0)"

start_cp

# Preflight the real login endpoint before handing the browser to Playwright:
# a broken seed or auth route fails here with a clear message, not as a
# mysterious UI timeout.
# Build the request body with Python instead of shell string interpolation:
# this keeps the per-run password out of curl's argv and avoids breaking on
# characters that need JSON escaping. Both files live in the disposable temp
# dir, never in the uploaded artifact (they are credentials/evidence, not CI
# output).
REAL_BFF_LOGIN_USER="$USERNAME" REAL_BFF_LOGIN_PASSWORD="$PASSWORD" \
    python3 - "${ROOT}/login-request.json" <<'PY'
import json, os, sys
with open(sys.argv[1], "w", encoding="utf-8") as handle:
    json.dump(
        {
            "username": os.environ["REAL_BFF_LOGIN_USER"],
            "password": os.environ["REAL_BFF_LOGIN_PASSWORD"],
        },
        handle,
    )
PY
if ! curl -fsS -X POST "http://127.0.0.1:${PORT}/v1/auth/login" \
    -H 'Content-Type: application/json' \
    --data @"${ROOT}/login-request.json" \
    -D "${ROOT}/login-headers.txt" \
    -o "${ROOT}/login-response.json"; then
    fail "real BFF login preflight failed (log: ${CP_LOG})"
fi
python3 - "${ROOT}/login-response.json" "${ROOT}/login-headers.txt" <<'PY' || fail "login preflight did not find an HttpOnly session JWT"
import json, sys
with open(sys.argv[1], encoding="utf-8") as handle:
    body = json.load(handle)
# #660: the JWT is never returned to JavaScript. The body must not expose
# it; the only credential is the HttpOnly `chv_session` cookie.
assert "token" not in body, body
cookies = []
with open(sys.argv[2], encoding="utf-8", errors="replace") as handle:
    for line in handle:
        name, _, value = line.partition(":")
        if name.strip().lower() == "set-cookie":
            cookies.append(value.strip())
session = [c for c in cookies if c.startswith("chv_session=")]
assert len(session) == 1, cookies
token = session[0].split(";", 1)[0].split("=", 1)[1]
assert token.count(".") == 2, "chv_session is not a JWT"
assert "HttpOnly" in session[0], session[0]
assert "SameSite=Strict" in session[0], session[0]
for hint in ("chv_role=", "chv_logged_in="):
    assert any(c.startswith(hint) for c in cookies), cookies
PY
log "real BFF login preflight passed (HttpOnly session cookie, no JWT in body)"

export REAL_BFF_BASE_URL="http://127.0.0.1:${PORT}"
export REAL_BFF_E2E_USER="$USERNAME"
export REAL_BFF_E2E_PASSWORD="$PASSWORD"

log "running: ${COMMAND[*]}"
set +e
"${COMMAND[@]}"
RC=$?
set -e
log "command exited ${RC}"

exit "$RC"
