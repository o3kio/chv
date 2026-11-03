#!/bin/sh
set -e

# If the caller supplied a command (docker compose `command:` overrides the
# image CMD, not the ENTRYPOINT), run that instead of the all-in-one supervisor
# below. Both compose files in this repo rely on this.
if [ "$#" -gt 0 ]; then
    exec "$@"
fi

echo "=== CHV Starting ==="

# Start nginx (serves UI + proxies API)
nginx

# Start storage daemon
chv-stord /etc/chv/stord.toml &
STORD_PID=$!

# Start network daemon
chv-nwd /etc/chv/nwd.toml &
NWD_PID=$!

# Start control plane (includes BFF)
chv-controlplane /etc/chv/controlplane.toml &
CP_PID=$!

# Wait for controlplane to be ready before starting agent
sleep 2

# Start agent
chv-agent /etc/chv/agent.toml &
AGENT_PID=$!

echo "=== CHV Ready ==="
echo "  UI:     http://localhost:80"
echo "  API:    http://localhost:8080"
echo "  gRPC:   localhost:8443"

# Wait for the control plane (the primary service), then tear the rest down.
# NOTE: `wait -n` is a bashism — this script runs under dash, where it fails
# with "wait: Illegal option -n" and (with the old `|| true`) shut the
# container down immediately after start.
wait "$CP_PID" || true
echo "=== CHV process exited, shutting down ==="
kill $CP_PID $AGENT_PID $STORD_PID $NWD_PID 2>/dev/null || true
wait
