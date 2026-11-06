#!/bin/bash
# rc.7 blocker B9 reproduction: an nwd restart discards the in-memory
# topology table; the next UpdateNetworkPolicy dispatch fails NOT_FOUND.
set -u
BFF=http://127.0.0.1:8080
JAR=/tmp/opencode/b9-repro-jar
NET=eac319350ba9fff7b0b957aedef20ba6
PW="$(cat /root/chv-field/state/admin_password)"
rm -f "$JAR"

stamp() { date -u +%Y-%m-%dT%H:%M:%SZ; }

login() {
  /usr/bin/curl -sS -o /tmp/opencode/b9-repro-login.json -w '%{http_code}' \
    -c "$JAR" -H 'Content-Type: application/json' \
    --data-binary "{\"username\":\"admin\",\"password\":\"${PW}\"}" \
    "$BFF/v1/auth/login"
}

post_json() { # path payload
  /usr/bin/curl -sS -b "$JAR" -H 'Content-Type: application/json' \
    --data-binary "$2" "$BFF$1"
}

wait_task() { # task_id timeout_s label
  local id="$1" timeout="$2" label="$3" t=0 status="" body=""
  while [ "$t" -le "$timeout" ]; do
    body="$(post_json /v1/tasks/get "{\"task_id\":\"${id}\"}")"
    status="$(printf '%s' "$body" | /usr/bin/python3 -c \
      'import json,sys; d=json.load(sys.stdin); t=d.get("detail") or d.get("task") or d; print(t.get("status") or "")' 2>/dev/null)"
    echo "t=${t}s ${label}_status=${status:-?}"
    case "$status" in
      Succeeded|Failed|Cancelled) break ;;
    esac
    sleep 5
    t=$((t + 5))
  done
  printf '%s' "$body" | /usr/bin/python3 -c '
import json,sys
d=json.load(sys.stdin); t=d.get("detail") or d.get("task") or d
print("terminal_task=" + json.dumps({
  "task_id": t.get("task_id"),
  "operation": t.get("operation"),
  "status": t.get("status"),
  "error_code": t.get("error_code"),
  "error_message": t.get("error_message"),
}, sort_keys=True))'
}

echo "=== rc.7 blocker B9 reproduction (nwd restart -> policy update) — $(stamp) ==="
echo "host: rc.7 packages on field-host-1; authority_mode=core-managed"
printf 'login_http=%s\n' "$(login)"
echo "nwd_pid_before=$(systemctl show -p MainPID --value chv-nwd) active=$(systemctl is-active chv-nwd)"

echo
echo "=== step 1: policy update with the current nwd process (topology present) ==="
RULES1='[{"direction":"inbound","protocol":"icmp","action":"accept","source_cidr":"10.200.0.0/24"}]'
RESP1="$(post_json /v1/networks/update "{\"network_id\":\"${NET}\",\"firewall_rules\":${RULES1}}")"
echo "update1_body=${RESP1}"
TASK1="$(printf '%s' "$RESP1" | /usr/bin/python3 -c 'import json,sys; print(json.load(sys.stdin).get("task_id") or "")')"
echo "update1_task=${TASK1}"
wait_task "$TASK1" 120 update1
echo "dnsmasq_after_update1=$(pgrep -c dnsmasq 2>/dev/null || echo 0) (expected 0: B8)"

echo
echo "=== step 2: documented nwd restart (no agent restart, core-managed reconcile is observe-only) ==="
echo "nft tables before restart:"
nft list tables 2>/dev/null | grep "chv-${NET}" || true
systemctl restart chv-nwd
for i in $(seq 1 30); do
  [ -S /run/chv/nwd/api.sock ] && systemctl is-active --quiet chv-nwd && break
  sleep 1
done
echo "nwd_pid_after=$(systemctl show -p MainPID --value chv-nwd) active=$(systemctl is-active chv-nwd)"
echo "journal after restart (topology/journal/ensure lines):"
journalctl -u chv-nwd --since '-2 min' --no-pager 2>/dev/null \
  | grep -iE 'topolog|recover|ensure|listening' | tail -10 || true
echo "nft table still present after restart:"
nft list tables 2>/dev/null | grep "chv-${NET}" || true
echo "waiting 20s to show the table is not re-ensured in core-managed mode..."
sleep 20
journalctl -u chv-nwd --since '-25 s' --no-pager 2>/dev/null \
  | grep -iE 'ensuring topology|ensure_network' | tail -5 || echo "(no ensure calls)"

echo
echo "=== step 3: policy update after the restart ==="
RULES2='[{"direction":"inbound","protocol":"icmp","action":"accept","source_cidr":"10.200.0.0/24"},{"direction":"inbound","protocol":"tcp","action":"accept","source_cidr":"10.200.0.0/24","dest_port":"22"}]'
RESP2="$(post_json /v1/networks/update "{\"network_id\":\"${NET}\",\"firewall_rules\":${RULES2}}")"
echo "update2_body=${RESP2}"
TASK2="$(printf '%s' "$RESP2" | /usr/bin/python3 -c 'import json,sys; print(json.load(sys.stdin).get("task_id") or "")')"
echo "update2_task=${TASK2}"
wait_task "$TASK2" 200 update2

echo
echo "=== nwd log for the failed dispatch ==="
journalctl -u chv-nwd --since '-6 min' --no-pager 2>/dev/null \
  | grep -iE 'set_firewall_policy|not found|topology' | tail -15 || true
echo
echo "=== agent log for the failed dispatch ==="
journalctl -u chv-agent --since '-6 min' --no-pager 2>/dev/null \
  | grep -iE 'set_firewall_policy|not found|DISPATCH|retry' | tail -15 || true
echo "B9_REPRO_DONE"
