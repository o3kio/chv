#!/bin/bash
set -euo pipefail

# Configuration
BFF_URL="${BFF_URL:-http://localhost:8080}"
USERNAME="${CHV_USER:-admin}"
# If CHV_PASS is not set, try to read the bootstrap password from the local install.
if [ -n "${CHV_PASS:-}" ]; then
    PASSWORD="$CHV_PASS"
elif [ -r /etc/chv/initial_admin_password ]; then
    PASSWORD="$(cat /etc/chv/initial_admin_password)"
else
    PASSWORD=""
fi
DURATION="${DURATION:-30s}"
CONNECTIONS="${CONNECTIONS:-50}"
RPS="${RPS:-100}"

echo "=== CHV Load Test ==="
echo "Target: $BFF_URL"
echo "Duration: $DURATION, Connections: $CONNECTIONS, RPS: $RPS"
echo ""

# Verify BFF is reachable
if ! curl -sf -o /dev/null "$BFF_URL" && ! curl -sf -o /dev/null "$BFF_URL/api/v1/auth/login"; then
    echo "ERROR: BFF is not reachable at $BFF_URL"
    echo "Make sure the CHV services are running (e.g., docker-compose up)."
    exit 1
fi

# 1. Get JWT token
echo "[1/2] Authenticating..."
TOKEN_RESPONSE=$(curl -s -X POST "$BFF_URL/api/v1/auth/login" \
  -H "Content-Type: application/json" \
  -d "{\"username\":\"$USERNAME\",\"password\":\"$PASSWORD\"}")

# Try jq first, then fall back to grep
if command -v jq >/dev/null 2>&1; then
    TOKEN=$(echo "$TOKEN_RESPONSE" | jq -r '.token // .access_token // empty')
else
    TOKEN=$(echo "$TOKEN_RESPONSE" | grep -oP '"(?:token|access_token)":"\K[^"]+' | head -n1)
fi

if [ -z "$TOKEN" ]; then
    echo "ERROR: Failed to get token. Response: $TOKEN_RESPONSE"
    exit 1
fi
echo "Authenticated successfully."
echo ""

# 2. Run load tests
echo "[2/2] Running load tests..."

run_test() {
    local name="$1"
    local endpoint="$2"
    echo "--- Testing $name ($endpoint) ---"
    oha \
        --no-tui \
        -z "$DURATION" \
        -c "$CONNECTIONS" \
        -q "$RPS" \
        -m POST \
        -H "Authorization: Bearer $TOKEN" \
        -H "Content-Type: application/json" \
        -d "{}" \
        "$BFF_URL$endpoint"
    echo ""
}

run_test "List VMs"      "/v1/vms"
run_test "List Nodes"    "/v1/nodes"
run_test "List Volumes"  "/v1/volumes"
run_test "List Networks" "/v1/networks"
run_test "Overview"      "/v1/overview"

echo "=== Load Test Complete ==="
