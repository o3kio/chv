#!/usr/bin/env bash
# One-off diagnostic collector for a failed Nightly KVM Qualification run.
# Reads host/image state and the preserved failed-test directories, and writes
# everything to stdout (the workflow redirects it into an artifact).
set -u

section() {
    printf '\n\n===== %s =====\n' "$1"
}

section "identity"
date -u
hostname
uname -a
printf 'nproc=%s\n' "$(nproc)"
free -h || true
df -h / /tmp /var/lib/chv 2>&1 || true
df -i / 2>&1 || true

section "kvm"
ls -l /dev/kvm 2>&1 || true
for p in /sys/module/kvm_intel/parameters/nested /sys/module/kvm_amd/parameters/nested; do
    [ -r "$p" ] && printf '%s=%s\n' "$p" "$(cat "$p")"
done
sudo -n dmesg 2>/dev/null | grep -iE 'kvm|oom|out of memory|segfault|blocked for more than' | tail -n 80 || true

section "hypervisor"
cloud-hypervisor --version 2>&1 || true
sha256sum /var/lib/chv/qual/hypervisor-fw 2>&1 || true
ls -lh /var/lib/chv/qual/images/ 2>&1 || true

section "guest images"
for img in /var/lib/chv/qual/images/noble-qual-patched.img /var/lib/chv/qual/images/noble-server-cloudimg-amd64.img; do
    [ -e "$img" ] || continue
    echo "--- $img"
    sha256sum "$img" 2>&1 || true
    qemu-img info "$img" 2>&1 || true
    qemu-img check "$img" 2>&1 | tail -n 20 || true
done

section "listeners"
ss -ltnp 2>&1 | grep -E ':(9100|9101|50051|8080|8443)\b' || true

section "processes"
ps -eo pid,ppid,etime,rss,pcpu,args --sort=-rss 2>&1 | grep -E 'cloud-hypervisor|chv-(agent|stord|nwd|controlplane)|qemu-system' | grep -v grep || true

section "failed test directories"
shopt -s nullglob
dirs=(/tmp/chv-qual-failed-*)
if [ "${#dirs[@]}" -eq 0 ]; then
    echo "no /tmp/chv-qual-failed-* directories remain"
else
    for d in "${dirs[@]}"; do
        echo "--- $d"
        sudo -n du -sh "$d" 2>&1 || true
        sudo -n find "$d" -maxdepth 5 -type f -printf '%s\t%p\n' 2>&1 | sort -k2 | head -n 200 || true
    done

    section "console logs"
    for d in "${dirs[@]}"; do
        while IFS= read -r f; do
            echo "--- $f"
            echo "[head]"
            sudo -n head -c 131072 "$f" 2>&1 || true
            echo
            echo "[tail]"
            sudo -n tail -c 131072 "$f" 2>&1 || true
            echo
        done < <(sudo -n find "$d" -type f -name 'console.log' 2>/dev/null | sort)
    done

    section "daemon and deploy logs"
    for d in "${dirs[@]}"; do
        for name in agent stord nwd controlplane control-plane deploy; do
            while IFS= read -r f; do
                echo "--- $f"
                sudo -n tail -n 300 "$f" 2>&1 || true
                echo
            done < <(sudo -n find "$d" -type f -name "${name}.log" 2>/dev/null | sort)
        done
    done
fi
