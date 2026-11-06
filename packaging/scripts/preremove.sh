#!/bin/sh
set -e

# CHV Generic Preremove
# Safely stops CHV services before package removal.
# Preserves all persistent data.
#
# Package manager conventions:
#   Debian: $1 = remove | purge | upgrade | failed-upgrade | ...
#   RPM:    $1 = 0 (uninstall) | 1 (upgrade)

ACTION=""
if [ "$1" = "remove" ] || [ "$1" = "purge" ] || [ "$1" = "0" ]; then
    ACTION="remove"
fi

# Skip service stop on upgrade
if [ "$1" = "upgrade" ] || [ "$1" = "1" ]; then
    ACTION="upgrade"
fi

have() {
    command -v "$1" >/dev/null 2>&1
}

stop_service() {
    if have systemctl; then
        systemctl stop "$1.service" 2>/dev/null || true
    fi
    # Without systemd (container/chroot package removal) the daemons are not
    # stopped here; the VMM teardown below still matches by process listing,
    # but stord/nwd may keep running until the container exits. Documented
    # limitation, unchanged by this script.
}

chv_sqlite_read() {
    if id -u chv >/dev/null 2>&1 && have runuser; then
        runuser -u chv -- sqlite3 "$@"
    elif id -u chv >/dev/null 2>&1 && have sudo; then
        sudo -u chv sqlite3 "$@"
    else
        sqlite3 "$@"
    fi
}

# CHV-owned host network state does not live under /var/lib/chv: nwd
# materializes a bridge, a netns and an nft table per network (plus tap
# slaves). Package removal never asks nwd to tear those down, so without this
# they outlive even the documented complete-removal procedure (field-plan
# section 6.9: network residue that survives documented cleanup is
# blocker-class). Derive ids from the deployment database while it is still
# readable AND from CHV-owned nft tables / netns names, so a network whose row
# is gone is still cleaned. Best-effort: removal must not fail because a tool
# is missing or one step finds nothing.
CHV_DB_PATH="${CHV_DB_PATH:-/var/lib/chv/controlplane.db}"
CHV_HOST_ETC="${CHV_HOST_ETC:-/etc}"

chv_bridge_name_for_network() {
    id="$1"
    if [ "$id" = "default" ]; then
        printf 'chvbr0'
        return 0
    fi
    candidate="br-${id}"
    if [ "${#candidate}" -le 15 ]; then
        printf '%s' "$candidate"
        return 0
    fi
    # Same FNV-1a name as chv_common::bridge_name_for_network: the kernel
    # device name is capped at 15 chars, so long ids keep an 8-char prefix and
    # a 4-hex hash.
    h=$((0x811c9dc5))
    prefix=$(printf '%s' "$id" | cut -c1-8)
    i=0
    while [ "$i" -lt "${#id}" ]; do
        c=$(printf '%s' "$id" | cut -c$((i + 1)))
        b=$(printf '%d' "'$c")
        h=$(( ((h * 0x01000193) & 0xffffffff) ^ b ))
        i=$((i + 1))
    done
    printf 'br-%s%04x' "$prefix" "$((h & 0xffff))"
}

chv_network_ids() {
    {
        if have sqlite3 && [ -f "$CHV_DB_PATH" ]; then
            # Read as the service user where possible: a root sqlite3 open
            # can leave root-owned -wal/-shm sidecars (issue #669).
            chv_sqlite_read "$CHV_DB_PATH" \
                "SELECT network_id FROM network_desired_state;" 2>/dev/null || true
            chv_sqlite_read "$CHV_DB_PATH" \
                "SELECT network_id FROM networks;" 2>/dev/null || true
            # The root fallback in chv_sqlite_read can create -wal/-shm
            # sidecars owned by root. The preserved database restarts as the
            # chv user, so hand any sidecars back to it (issue #669).
            chown chv:chv "$CHV_DB_PATH-wal" "$CHV_DB_PATH-shm" \
                2>/dev/null || true
        fi
        if have nft; then
            nft list tables 2>/dev/null \
                | sed -n 's/^table inet chv-\([A-Za-z0-9_.-][A-Za-z0-9_.-]*\)$/\1/p'
        fi
        if have ip; then
            # A netns name alone is weaker evidence than a CHV nft table or
            # database row, so only the literal `default` network and legacy
            # hex ids are treated as ours from this source; custom ids come
            # from the two sources above. This keeps an unrelated `ns-<name>`
            # from being deleted.
            ip netns list 2>/dev/null \
                | awk '{print $1}' \
                | sed -n 's/^ns-\([0-9a-f][0-9a-f]*\|default\)$/\1/p'
        fi
    } | awk '/^[A-Za-z0-9_.-]+$/' | sort -u
}

# Remove one CHV bridge plus the host state that would recreate or outlive it:
# its CHV tap slaves, the exact CHV MASQUERADE rules (live and persisted), and
# the networkd/ifupdown drop-in. Never touches other slaves or other bridges:
# the uplink enslaved to chvbr0 must survive, and the persisted-rule match is
# exact.
remove_chv_bridge_and_nat() {
    cidr=""
    rules_file="${CHV_IPTABLES_RULES_V4:-${CHV_HOST_ETC}/iptables/rules.v4}"
    if have ip; then
        # The kernel route carries the network CIDR without re-deriving it
        # from the address/prefix.
        cidr="$(ip -o -4 route show dev "$1" proto kernel scope link 2>/dev/null \
            | awk '{print $1; exit}')"
        for tap in $(ip -o link show master "$1" 2>/dev/null \
            | awk -F': ' '{print $2}' | cut -d@ -f1 \
            | grep '^tap-[0-9a-f]\{8\}$' || true); do
            ip link del dev "$tap" 2>/dev/null || true
        done
        ip link set dev "$1" down 2>/dev/null || true
        ip link del dev "$1" 2>/dev/null || true
    fi

    if [ -n "$cidr" ] && have iptables; then
        while iptables -t nat -C POSTROUTING -s "$cidr" ! -d "$cidr" -j MASQUERADE 2>/dev/null; do
            iptables -t nat -D POSTROUTING -s "$cidr" ! -d "$cidr" -j MASQUERADE 2>/dev/null || break
        done
        while iptables -t nat -C POSTROUTING -s "$cidr" ! -o "$1" -j MASQUERADE 2>/dev/null; do
            iptables -t nat -D POSTROUTING -s "$cidr" ! -o "$1" -j MASQUERADE 2>/dev/null || break
        done
        if [ -f "$rules_file" ]; then
            awk -v d="-A POSTROUTING -s $cidr ! -d $cidr -j MASQUERADE" \
                -v o="-A POSTROUTING -s $cidr ! -o $1 -j MASQUERADE" \
                '$0 != d && $0 != o' "$rules_file" \
                > "${rules_file}.chv-clean" \
                && mv "${rules_file}.chv-clean" "$rules_file" \
                || rm -f "${rules_file}.chv-clean"
        fi
    fi

    # Persistent bridge configuration: install.sh writes a networkd netdev/
    # network pair (plus an uplink Bridge= drop-in) or an ifupdown stanza.
    # Leaving these would recreate the bridge at the next boot.
    rm -f "${CHV_HOST_ETC}/systemd/network/10-$1.netdev" \
          "${CHV_HOST_ETC}/systemd/network/11-$1.network" 2>/dev/null || true
    for f in "${CHV_HOST_ETC}"/systemd/network/12-*.network; do
        [ -f "$f" ] || continue
        if grep -Fqx "Bridge=$1" "$f" 2>/dev/null; then
            rm -f "$f" 2>/dev/null || true
        fi
    done
    if [ "$1" = "chvbr0" ]; then
        rm -f "${CHV_HOST_ETC}/network/interfaces.d/chvbr0" 2>/dev/null || true
    fi
}

delete_chv_network_state() {
    chv_network_ids | while IFS= read -r id; do
        [ -n "$id" ] || continue
        case "$id" in
            ''|*[!A-Za-z0-9_.-]*) continue ;;
        esac
        bridge="$(chv_bridge_name_for_network "$id")"
        remove_chv_bridge_and_nat "$bridge"
        if have ip; then
            ip netns del "ns-$id" 2>/dev/null || true
        fi
        if have nft; then
            nft delete table inet "chv-$id" 2>/dev/null || true
        fi
    done

    # The baseline bridge can predate any network row, so it has no id to
    # derive from. Delete the configured name and the historical default; the
    # uplink stays up.
    baseline_bridge="${INSTALL_CHV_BRIDGE_NAME:-chvbr0}"
    remove_chv_bridge_and_nat "$baseline_bridge"
    if [ "$baseline_bridge" != "chvbr0" ]; then
        remove_chv_bridge_and_nat chvbr0
    fi
    rm -f "${CHV_HOST_ETC}/sysctl.d/99-chv-forward.conf" 2>/dev/null || true
    if have netfilter-persistent; then
        netfilter-persistent save 2>/dev/null || true
    fi

    # Sweep any CHV-owned table whose id could not be derived above.
    if have nft; then
        nft list tables 2>/dev/null \
            | sed -n 's/^table inet \(chv-[A-Za-z0-9_.-][A-Za-z0-9_.-]*\)$/\1/p' \
            | while IFS= read -r table; do
                nft delete table inet "$table" 2>/dev/null || true
            done
    fi
}

if [ "$ACTION" = "remove" ]; then
    # Stop the control plane and the agent first so nothing reconciles the
    # VMs back to life, then terminate the VMMs the agent (by design) leaves
    # running, and only then take storage and network down.
    stop_service chv-controlplane
    stop_service chv-agent

    # VMMs are children of chv-agent but outlive it by design (the unit uses
    # KillMode=process so a restart adopts them instead of killing them).
    # Package removal is final: stop any CHV-owned cloud-hypervisor too, or
    # removal leaves orphaned guests (and their taps/nftables state) behind.
    # Match the truncated comm (TASK_COMM_LEN) plus the CHV VM api-socket path
    # so an unrelated cloud-hypervisor on the host is never signalled. The
    # path is the chv-config state-dir default (/var/lib/chv/agent/vms/); a
    # custom CHV_DATA_DIR would need this matcher updated in lockstep.
    vmm_pids="$(ps -eo pid=,comm=,args= 2>/dev/null | awk '
        ($2 == "cloud-hyperviso" || $2 == "cloud-hypervisor") &&
        $0 ~ /\/var\/lib\/chv\/agent\/vms\// { print $1 }')"
    if [ -n "$vmm_pids" ]; then
        # Word splitting is deliberate: the list is PIDs only.
        kill -TERM $vmm_pids 2>/dev/null || true
        n=0
        while [ "$n" -lt 10 ]; do
            sleep 1
            vmm_pids="$(ps -eo pid=,comm=,args= 2>/dev/null | awk '
                ($2 == "cloud-hyperviso" || $2 == "cloud-hypervisor") &&
                $0 ~ /\/var\/lib\/chv\/agent\/vms\// { print $1 }')"
            if [ -z "$vmm_pids" ]; then
                break
            fi
            n=$((n + 1))
        done
        if [ -n "$vmm_pids" ]; then
            kill -KILL $vmm_pids 2>/dev/null || true
        fi
    fi

    stop_service chv-stord
    stop_service chv-nwd

    # nwd is down: remove the bridges, netns, taps and nft tables it created
    # (the database is still present here, before postrm/purge deletes it).
    delete_chv_network_state

    if have systemctl; then
        systemctl daemon-reload 2>/dev/null || true
        systemctl reset-failed chv-controlplane chv-agent chv-stord chv-nwd \
            2>/dev/null || true
    fi
fi

exit 0
