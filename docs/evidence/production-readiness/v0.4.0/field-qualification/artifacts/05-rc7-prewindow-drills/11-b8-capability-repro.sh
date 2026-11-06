#!/bin/bash
# rc.7 blocker B8 reproduction: the nwd unit runs as non-root `chv` with only
# CAP_NET_ADMIN CAP_NET_RAW CAP_SYS_ADMIN. dnsmasq must bind UDP/67; the host
# keeps net.ipv4.ip_unprivileged_port_start=1024, so the bind fails. This runs
# dnsmasq under a transient unit with exactly the rc.7 nwd capability set
# against the live bridge, then re-runs it with CAP_NET_BIND_SERVICE added to
# show the fix. It does not change the installed topology.
set -u
STAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
BR=br-eac31935dc4f
CONF=/tmp/b8-dnsmasq.conf
PID=/tmp/b8-dnsmasq.pid
LEASE=/tmp/b8-dnsmasq.leases

rm -f "$CONF" "$PID" "$LEASE"
cat > "$CONF" <<EOF
interface=${BR}
bind-interfaces
port=0
dhcp-range=10.200.0.50,10.200.0.205,255.255.255.0,12h
dhcp-option=3,10.200.0.1
dhcp-option=6,1.1.1.1
except-interface=lo
no-resolv
EOF
chmod 0644 "$CONF"

run_as_chv() { # unit-name caps...
  local unit="$1" caps="$2"
  systemd-run --unit="$unit" --wait --pipe --collect \
    --property=User=chv --property=Group=chv \
    --property="AmbientCapabilities=${caps}" \
    --property="CapabilityBoundingSet=${caps}" \
    --property=RuntimeMaxSec=5s \
    /usr/sbin/dnsmasq --conf-file="$CONF" --pid-file="$PID" \
      --dhcp-leasefile="$LEASE" --no-daemon --log-dhcp 2>&1
}

echo "=== rc.7 blocker B8 reproduction (dnsmasq UDP/67 as non-root chv) — ${STAMP} ==="
echo "host: rc.7 packages on field-host-1; nwd unit caps as installed by rc.7"
echo "=== installed nwd capability set ==="
systemctl cat chv-nwd 2>/dev/null | grep -E '^(AmbientCapabilities|CapabilityBoundingSet)='
PIDNWD="$(systemctl show -p MainPID --value chv-nwd)"
echo "nwd_pid=${PIDNWD}"
grep -E 'Cap(Inh|Prm|Eff|Bnd|Amb)' "/proc/${PIDNWD}/status"
/usr/sbin/capsh --decode="$(grep CapEff "/proc/${PIDNWD}/status" | awk '{print $2}')" 2>/dev/null \
  | sed 's/^/nwd_effective=/' || true
echo
echo "=== kernel privileged-port floor ==="
sysctl net.ipv4.ip_unprivileged_port_start
echo
echo "=== rc.7 caps: CAP_NET_ADMIN CAP_NET_RAW CAP_SYS_ADMIN (no NET_BIND_SERVICE) ==="
echo "conf:"; sed 's/^/  /' "$CONF"
set +e
run_as_chv b8-repro-rc7 "CAP_NET_ADMIN CAP_NET_RAW CAP_SYS_ADMIN"
RC7=$?
set -e
echo "dnsmasq_exit_rc7=${RC7}"
echo
echo "=== rc.8 caps: the same set plus CAP_NET_BIND_SERVICE (fix control) ==="
rm -f "$PID" "$LEASE"
set +e
run_as_chv b8-repro-rc8 "CAP_NET_ADMIN CAP_NET_RAW CAP_SYS_ADMIN CAP_NET_BIND_SERVICE"
RC8=$?
set -e
echo "dnsmasq_exit_rc8=${RC8} (the transient unit is stopped after RuntimeMaxSec)"
if [ -f "$PID" ]; then
  echo "pid_file_present=yes pid=$(cat "$PID")"
  kill "$(cat "$PID")" 2>/dev/null || true
else
  echo "pid_file_present=no"
fi
echo "B8_CAPABILITY_REPRO_DONE"
