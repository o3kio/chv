#!/bin/bash
# Container package-smoke test for the converged WebUI serving shape.
#
# Issue #549 (the #447 tier-label gate): builds the converged container
# image FROM the built .deb packages — the packaged proxy-only nginx
# edge (packaging/nginx/chv-example.conf) in front of chv-controlplane
# with [webui] enabled, serving the packaged UI tree, with the
# chv-agent console listener behind the edge's /ws/ proxy — then
# smoke-tests the single-listener shape end to end.
#
# Assertions (all against the edge's single :80 listener):
#   1. GET /                      -> 200, HTML shell, no-cache header
#   2. GET /_app/immutable/...    -> 200, 1-year immutable cache header
#   3. GET /observability         -> 200, HTML shell (hard-load, the
#                                    #447 review rename)
#   4. GET /v1/<missing>          -> JSON 404 (reserved prefix, never
#                                    the SPA shell)
#   4a. POST /internal/bootstrap-token -> 404 (internal management routes
#                                    are never proxied through the edge)
#   5. GET /metrics               -> 401 unauthenticated (admin-gated
#                                    prometheus route, not shadowed)
# 6. POST /api/v1/auth/login    -> 200 + JWT (login round-trip)
#   6a. POST /v1/auth/change-password -> 200 (the first-login flow: the
#       seeded admin carries must_change_password=1, the deployment
#       contract; the BFF gates every non-auth route behind it)
#   6b. POST /api/v1/auth/login  -> 200 + fresh JWT (re-login after the
#       password change — the old token's claim stays set)
#   6c. GET /api/v1/auth/me      -> 200 + admin identity
#   6d. POST /v1/overview        -> 200 JSON (BFF API through the edge)
#   9. /ws/vms/<node>/<vm>/console?token=<invalid>
#                                 -> agent-sourced response (4xx or
#                                    101), never an edge 502 — proves
#                                    the /ws/ proxy reaches the agent
#
# All requests run from INSIDE the stack container (engine exec,
# 127.0.0.1:80): host-side port publishing is unreliable on locked-down
# hosts (rootless podman without routed netns access), and the leg's
# contract is the edge's single listener, not the host mapping. The
# published host port (WEBUI_SMOKE_PORT) remains as a debugging
# convenience on hosts where it works.
#
# Usage: ./scripts/package/smoke-webui.sh [packages_dir]
#   packages_dir defaults to dist/packages (make package-deb output).
#
# Environment:
#   CONTAINER_ENGINE  - docker (default) or podman. The driver is
#                       engine-agnostic; CI uses docker, local
#                       qualification can use podman.
#   CONTAINER_BUILD_NETWORK - passed as --network to the image build
#                       (e.g. "host" on hosts whose container networking
#                       has no outbound DNS; unset = engine default).
#   WEBUI_SMOKE_BASE_IMAGE - base image build-arg override (default
#                       debian:12-slim; e.g. ubuntu:24.04 when the
#                       packages were built on a glibc-newer host — the
#                       release.yml ubuntu-22.04 build pin exists for
#                       exactly this reason).
#   WEBUI_SMOKE_PORT  - host port mapped to the edge's :80 for manual
#                       debugging (default 18080; assertions do not
#                       depend on it).
#   IMAGE              - image tag (default chv-webui-smoke:<version>).
#   PACKAGE_VERSION    - exact package version to smoke (the CI
#                       convention; default: newest by mtime).
#
# Make: package-smoke-webui. Siblings: smoke-deb.sh / smoke-rpm.sh
# (packaging-contract smokes); this one adds the runtime leg — the
# binary actually starts and serves.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
PKGDIR="${1:-${REPO_ROOT}/dist/packages}"

CONTAINER_ENGINE="${CONTAINER_ENGINE:-docker}"
WEBUI_SMOKE_PORT="${WEBUI_SMOKE_PORT:-18080}"

ERRORS=0

info()  { echo "[webui-smoke] $*"; }
pass()  { echo "[webui-smoke] PASS: $*"; }
error() { echo "[webui-smoke] ERROR: $*" >&2; ERRORS=$((ERRORS + 1)); }

# --- preflight -------------------------------------------------------------

if ! command -v "${CONTAINER_ENGINE}" > /dev/null 2>&1; then
    echo "ERROR: container engine '${CONTAINER_ENGINE}' not found" >&2
    exit 1
fi
if ! "${CONTAINER_ENGINE}" info > /dev/null 2>&1; then
    echo "ERROR: container engine '${CONTAINER_ENGINE}' is not running" >&2
    exit 1
fi

# Package selection: exact PACKAGE_VERSION when exported (the CI
# convention) — failing closed on a mismatch rather than silently
# smoking a different build (a dist/packages dir can carry stale
# older-version debs from previous builds). Without PACKAGE_VERSION,
# the newest build by mtime is used.
select_deb() {
    local name="$1"
    if [ -n "${PACKAGE_VERSION:-}" ]; then
        local exact
        exact="$(ls "${PKGDIR}/${name}_${PACKAGE_VERSION}"_*.deb 2>/dev/null | head -n1 || true)"
        if [ -z "${exact}" ]; then
            echo "ERROR: PACKAGE_VERSION='${PACKAGE_VERSION}' set but no ${name}_${PACKAGE_VERSION}_*.deb under ${PKGDIR}" >&2
            return 1
        fi
        echo "${exact}"
    else
        ls -t "${PKGDIR}"/${name}_*.deb 2>/dev/null | head -n1 || true
    fi
}
CONTROLPLANE_DEB="$(select_deb chv-controlplane)" || exit 1
NODE_DEB="$(select_deb chv-node)" || exit 1
if [ -z "${CONTROLPLANE_DEB}" ] || [ -z "${NODE_DEB}" ]; then
    echo "ERROR: chv-controlplane/chv-node .deb not found under ${PKGDIR}" >&2
    echo "  build them first: make package-deb" >&2
    exit 1
fi

PKG_VERSION="$(basename "${CONTROLPLANE_DEB}" | sed -E 's/^chv-controlplane_(.+)_(amd64|arm64)\.deb$/\1/')"
# Image tags cannot carry '~' (deb rc versions use it: 0.4.0~rc.2).
IMAGE="${IMAGE:-chv-webui-smoke:$(echo "${PKG_VERSION}" | tr '~+' '--')}"
CONTAINER="chv-webui-smoke-$$"

# --- build the converged image (minimal context: debs + Dockerfile +
# entrypoint — never the whole repo tree) ------------------------------------

CONTEXT="$(mktemp -d)"
cleanup() {
    "${CONTAINER_ENGINE}" rm -f "${CONTAINER}" > /dev/null 2>&1 || true
    rm -rf "${CONTEXT}" "${TMP:-}"
}
trap cleanup EXIT
cp "${REPO_ROOT}/packaging/container/Dockerfile.webui-smoke" "${CONTEXT}/Dockerfile"
cp "${REPO_ROOT}/packaging/container/webui-smoke-entrypoint.sh" "${CONTEXT}/"
cp "${CONTROLPLANE_DEB}" "${NODE_DEB}" "${CONTEXT}/"

BUILD_ARGS=()
if [ -n "${WEBUI_SMOKE_BASE_IMAGE:-}" ]; then
    BUILD_ARGS=(--build-arg "BASE_IMAGE=${WEBUI_SMOKE_BASE_IMAGE}")
fi
BUILD_NETWORK_ARGS=()
if [ -n "${CONTAINER_BUILD_NETWORK:-}" ]; then
    BUILD_NETWORK_ARGS=(--network "${CONTAINER_BUILD_NETWORK}")
fi

info "building ${IMAGE} from ${PKGDIR} (context: $(du -sh "${CONTEXT}" | cut -f1))"
if ! "${CONTAINER_ENGINE}" build -t "${IMAGE}" "${BUILD_ARGS[@]+"${BUILD_ARGS[@]}"}" "${BUILD_NETWORK_ARGS[@]+"${BUILD_NETWORK_ARGS[@]}"}" "${CONTEXT}" > "${CONTEXT}/build.log" 2>&1; then
    echo "ERROR: image build failed; last 40 lines:" >&2
    tail -n 40 "${CONTEXT}/build.log" >&2
    echo "[webui-smoke] RESULT: FAILED"
    exit 1
fi

# --- run ---------------------------------------------------------------------

ADMIN_PASSWORD="$(openssl rand -base64 18 | tr -d '\n' | tr '+/' '-_')"

# The host port publish is a DEBUGGING convenience only (assertions run
# in-container); on hosts where publishing fails (locked-down rootless
# setups), fall back to running unpublished rather than aborting.
info "running ${CONTAINER} (edge on container :80; host debug port ${WEBUI_SMOKE_PORT})"
if ! "${CONTAINER_ENGINE}" run -d --rm --name "${CONTAINER}" \
    -p "127.0.0.1:${WEBUI_SMOKE_PORT}:80" \
    -e "CHV_SMOKE_ADMIN_PASSWORD=${ADMIN_PASSWORD}" \
    "${IMAGE}" > /dev/null; then
    info "port publish unavailable on this host; running without -p (assertions are in-container)"
    "${CONTAINER_ENGINE}" run -d --rm --name "${CONTAINER}" \
        -e "CHV_SMOKE_ADMIN_PASSWORD=${ADMIN_PASSWORD}" \
        "${IMAGE}" > /dev/null
fi

# --- request layer: curl from inside the stack container ---------------------
# One request = one engine exec; headers and body come back on stdout
# between ===HDRS=== / ===BODY=== markers (the container's /tmp holds
# the intermediate files).
TMP="$(mktemp -d)"

edge_request() {
    "${CONTAINER_ENGINE}" exec "${CONTAINER}" bash -c '
        curl -sS --max-time 10 -D /tmp/webui-smoke.h -o /tmp/webui-smoke.b "$@" 2>/dev/null || true
        printf "===HDRS===\n"
        cat /tmp/webui-smoke.h 2>/dev/null
        printf "===BODY===\n"
        cat /tmp/webui-smoke.b 2>/dev/null
    ' smoke "$@"
}

# do_request <url> [curl args...] -> ${TMP}/h (headers), ${TMP}/b (body)
do_request() {
    edge_request "$@" > "${TMP}/resp.raw"
    awk '/^===BODY===$/{exit} !/^===HDRS===$/{print}' "${TMP}/resp.raw" > "${TMP}/h"
    awk 'f{print} /^===BODY===$/{f=1}' "${TMP}/resp.raw" > "${TMP}/b"
}

container_logs() {
    "${CONTAINER_ENGINE}" logs "${CONTAINER}" 2>&1 | tail -n 40 >&2 || true
}

# --- wait for the stack (CP migrations + admin seed + nginx) -----------------

STACK_UP=0
for _ in $(seq 1 90); do
    if "${CONTAINER_ENGINE}" exec "${CONTAINER}" curl -sf "http://127.0.0.1:80/health" > /dev/null 2>&1; then
        STACK_UP=1
        break
    fi
    sleep 1
done
if [ "${STACK_UP}" -ne 1 ]; then
    error "stack did not come up within 90s; container logs follow"
    container_logs
    echo "[webui-smoke] RESULT: FAILED"
    exit 1
fi
pass "stack up: /health reachable through the edge"

# 1. Console shell + no-cache ------------------------------------------------
do_request "http://127.0.0.1:80/"
grep -qi '^HTTP/.* 200' "${TMP}/h" \
    && pass "GET / -> 200" \
    || error "GET / did not return 200"
grep -qi '<!doctype html>' "${TMP}/b" \
    && pass "GET / serves the HTML shell" \
    || error "GET / did not serve the HTML shell"
grep -qi '^cache-control: no-cache, no-store, must-revalidate' "${TMP}/h" \
    && pass "GET / index no-cache header" \
    || error "GET / missing no-cache cache-control (got: $(grep -i '^cache-control' "${TMP}/h" || echo none))"

# 2. Immutable asset ---------------------------------------------------------
IMMUTABLE="$(sed -n 's/.*\(\/_app\/immutable\/[^"]*\).*/\1/p' "${TMP}/b" | head -n1)"
if [ -n "${IMMUTABLE}" ]; then
    do_request "http://127.0.0.1:80${IMMUTABLE}"
    grep -qi '^HTTP/.* 200' "${TMP}/h" \
        && pass "immutable asset ${IMMUTABLE} -> 200" \
        || error "immutable asset ${IMMUTABLE} did not return 200"
    grep -qi '^cache-control: public, max-age=31536000, immutable' "${TMP}/h" \
        && pass "immutable asset 1-year immutable header" \
        || error "immutable asset missing immutable cache-control (got: $(grep -i '^cache-control' "${TMP}/h" || echo none))"
else
    error "no /_app/immutable/ reference found in the served index"
fi

# 3. /observability hard-load ------------------------------------------------
do_request "http://127.0.0.1:80/observability"
grep -qi '^HTTP/.* 200' "${TMP}/h" \
    && pass "GET /observability -> 200" \
    || error "GET /observability did not return 200"
grep -qi '<!doctype html>' "${TMP}/b" \
    && pass "GET /observability serves the shell (hard-load)" \
    || error "GET /observability did not serve the SPA shell"

# 4. Reserved prefix: JSON 404, never the shell -------------------------------
do_request "http://127.0.0.1:80/v1/definitely-not-a-route"
grep -qi '^HTTP/.* 404' "${TMP}/h" \
    && pass "GET /v1/<missing> -> 404" \
    || error "GET /v1/<missing> did not return 404"
grep -qi 'content-type: application/json' "${TMP}/h" \
    && pass "reserved-prefix miss is JSON" \
    || error "reserved-prefix miss is not JSON (the SPA shell must never shadow API 404s)"

# 4a. Internal management routes are never exposed through the edge ----------
do_request "http://127.0.0.1:80/internal/bootstrap-token" \
    -H 'Content-Type: application/json' \
    -X POST \
    -d '{"token":"edge-must-not-reach-this"}'
grep -qi '^HTTP/.* 404' "${TMP}/h" \
    && pass "POST /internal/bootstrap-token -> 404 (not proxied)" \
    || error "POST /internal/bootstrap-token was exposed through the edge (got: $(head -n1 "${TMP}/h"))"

# 5. /metrics stays the admin-gated prometheus route --------------------------
do_request "http://127.0.0.1:80/metrics"
grep -qi '^HTTP/.* 401' "${TMP}/h" \
    && pass "GET /metrics unauthenticated -> 401 (route not shadowed by the UI)" \
    || error "GET /metrics unauthenticated did not return 401 (got: $(head -n1 "${TMP}/h"))"

# 6. Login round-trip ----------------------------------------------------------
do_request "http://127.0.0.1:80/api/v1/auth/login" \
    -H 'Content-Type: application/json' \
    -X POST \
    -d "{\"username\":\"admin\",\"password\":\"${ADMIN_PASSWORD}\"}"
grep -qi '^HTTP/.* 200' "${TMP}/h" \
    && pass "POST /api/v1/auth/login -> 200" \
    || error "login did not return 200 (body: $(head -c 200 "${TMP}/b"))"
TOKEN="$(sed -n 's/.*"token":"\([^"]*\)".*/\1/p' "${TMP}/b" | head -n1)"
if [ -n "${TOKEN}" ]; then
    pass "login returned a JWT"
else
    error "login response did not include a JWT token"
fi

# 7. /api/v1/auth/me (with the bootstrap token) ------------------------------
if [ -n "${TOKEN}" ]; then
    do_request "http://127.0.0.1:80/api/v1/auth/me" \
        -H "Authorization: Bearer ${TOKEN}"
    grep -qi '^HTTP/.* 200' "${TMP}/h" \
        && pass "GET /api/v1/auth/me -> 200" \
        || error "GET /api/v1/auth/me did not return 200"
    grep -q '"username":"admin"' "${TMP}/b" \
        && pass "auth/me carries the admin identity" \
        || error "auth/me payload missing admin identity"
fi

# 7a. First-login flow: change-password, then re-login -----------------------
# The seeded admin carries must_change_password=1 (the quick-install
# contract); the BFF's role middleware answers 403
# PASSWORD_CHANGE_REQUIRED to everything else until the password is
# changed — the exact first-login UX every fresh deployment walks.
NEW_PASSWORD="webui-smoke-$(openssl rand -hex 8)"
TOKEN2=""
if [ -n "${TOKEN}" ]; then
    do_request "http://127.0.0.1:80/v1/auth/change-password" \
        -H "Authorization: Bearer ${TOKEN}" \
        -H 'Content-Type: application/json' \
        -X POST \
        -d "{\"current_password\":\"${ADMIN_PASSWORD}\",\"new_password\":\"${NEW_PASSWORD}\"}"
    grep -qi '^HTTP/.* 200' "${TMP}/h" \
        && pass "POST /v1/auth/change-password -> 200" \
        || error "change-password did not return 200 (body: $(head -c 200 "${TMP}/b"))"

    # The changed token's must_change_password claim is minted at issue
    # time — the flow re-logs-in for a clean one, like the UI does.
    do_request "http://127.0.0.1:80/api/v1/auth/login" \
        -H 'Content-Type: application/json' \
        -X POST \
        -d "{\"username\":\"admin\",\"password\":\"${NEW_PASSWORD}\"}"
    grep -qi '^HTTP/.* 200' "${TMP}/h" \
        && pass "re-login with the new password -> 200" \
        || error "re-login did not return 200 (body: $(head -c 200 "${TMP}/b"))"
    TOKEN2="$(sed -n 's/.*"token":"\([^"]*\)".*/\1/p' "${TMP}/b" | head -n1)"
    if [ -n "${TOKEN2}" ]; then
        pass "re-login returned a fresh JWT"
    else
        error "re-login response did not include a JWT token"
    fi
fi

# 8. API through the single listener -------------------------------------------
# The BFF namespace the UI itself drives: JSON-body POSTs at /v1/*
# (bff_router merged bare into the CP listener — ui/src/lib/bff/
# endpoints.ts; the /api/v1/* namespace is the CP's own auth surface,
# exercised by the login assertions above).
if [ -n "${TOKEN2}" ]; then
    do_request "http://127.0.0.1:80/v1/overview" \
        -H "Authorization: Bearer ${TOKEN2}" \
        -H 'Content-Type: application/json' \
        -X POST -d '{}'
    grep -qi '^HTTP/.* 200' "${TMP}/h" \
        && pass "POST /v1/overview -> 200 (BFF API through the edge)" \
        || error "POST /v1/overview did not return 200 (got: $(head -n1 "${TMP}/h"); body: $(head -c 200 "${TMP}/b"))"
    grep -qi 'content-type: application/json' "${TMP}/h" \
        && pass "overview response is JSON" \
        || error "overview response is not JSON"
fi

# 9. /ws/ console proxy reaches the agent --------------------------------------
# A websocket upgrade attempt with an invalid token: the AGENT must
# answer (401/403/400/404 for the bogus token/VM, or 101 if a console
# existed) — an nginx 502/504 would mean the edge cannot reach the
# agent, which is the failure this assertion exists to catch.
do_request "http://127.0.0.1:80/ws/vms/smoke-node/smoke-vm/console?token=invalid-smoke-token" \
    -H 'Connection: Upgrade' \
    -H 'Upgrade: websocket' \
    -H 'Sec-WebSocket-Version: 13' \
    -H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ=='
WS_STATUS="$(head -n1 "${TMP}/h" | tr -d '\r' | awk '{print $2}')"
case "${WS_STATUS}" in
    101|400|401|403|404)
        pass "/ws/ upgrade reached the agent (status ${WS_STATUS})"
        # The agent's console rejection is a bare StatusCode response —
        # empty body, no content-type (console_server.rs). An
        # nginx-generated 502/504 upstream error would carry an HTML
        # error page — pins the response as upstream-sourced.
        if [ ! -s "${TMP}/b" ] && ! grep -qi '^content-type: text/html' "${TMP}/h"; then
            pass "/ws/ rejection is the agent's bare-401 shape (empty body)"
        else
            error "/ws/ response looks edge-generated, not agent-sourced (content-type: $(grep -i '^content-type' "${TMP}/h" || echo none); body: $(head -c 100 "${TMP}/b"))"
        fi
        ;;
    *)
        error "/ws/ console proxy did not reach the agent (status: ${WS_STATUS:-none}; the edge's /ws/ location must proxy to the agent console listener)"
        container_logs
        ;;
esac

# --- summary -------------------------------------------------------------------

echo
echo "==================================================="
if [ "${ERRORS}" -eq 0 ]; then
    echo "[webui-smoke] RESULT: PASSED (all assertions green)"
    exit 0
else
    echo "[webui-smoke] RESULT: FAILED (${ERRORS} error(s))"
    exit 1
fi
