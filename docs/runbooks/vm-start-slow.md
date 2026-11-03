# Runbook: VM Start Latency (`VmStartLatencyHighP99`)

**Scenario:** The `VmStartLatencyHighP99` warning has fired — the 99th percentile
of `chv_vm_op_duration_seconds{op="start"}` has exceeded 45 s for 10 minutes.
The SLO is a p99 under 30 s; 30–45 s is degraded and above 45 s is breaching
([`docs/OBSERVABILITY.md`](../OBSERVABILITY.md#sli-1-vm-start-latency-p99)).
Use this runbook when starts are **slow but succeeding**. If starts are
**failing**, use [VM Operation Failures](vm-op-failures.md) instead.

**Severity:** **SEV-2** when the breach is sustained or multiple VMs/nodes are
affected. **SEV-3** for an isolated slow start that has not moved the p99.
A fleet-wide p99 breach with guest impact is **SEV-1** — escalate.

**Impact:** starts still complete, but the node is above its latency error
budget and concurrent operations for the same VM queue behind its lifecycle
lock. This alert does not by itself show data loss or a failed start.

**Automation level:** Alert evaluation is automatic. There is no automatic
latency remediation: accepted Core operations retry up to their fixed attempt
bound, and an already-running VM returns through the idempotent
`vm already booted, skipping vm.boot` path. Everything else is diagnosis.

## What the metric actually measures

`chv_vm_op_duration_seconds{op="start"}` is recorded inside
`chv-agent-runtime-ch` from the start operation beginning until `start_vm`
returns after the Cloud Hypervisor `/api/v1/vm.boot` call. It **does not**
measure guest boot to login or application readiness. A guest that takes two minutes to reach
sshd will not breach this alert if `/vm.boot` returned quickly.

## Metrics availability caveat

**As shipped, the agent does not expose these series.** `VmOpGuard` records
`chv_vm_ops_total` and `chv_vm_op_duration_seconds` into the global Prometheus
registry, but the agent's `:9901` handler renders only the hand-built
`chv_agent_*` families; it does not render that registry. `cmd/chv-agent` does
not enable the `http-metrics` feature the other daemons use, and no other
process records these names.

Until [issue #702](https://github.com/o3kio/chv/issues/702) lands, the PromQL
below returns no data from a packaged agent, and `VmStartLatencyHighP99`
cannot fire from agent scrapes. If this alert is paging you, the scrape target
is running a build with #702 or an external exporter — confirm that first. The
operation-journal and log steps work today; use them while the series is
missing.

## Prerequisites

- `ssh` and `sudo` on the node, plus `chvctl` configured against the control
  plane.
- Access to the node's agent metrics listener (packaged default
  `127.0.0.1:9901`) or the Prometheus instance that scrapes it. See
  [Metrics availability caveat](#metrics-availability-caveat) — the listener
  currently exposes `chv_agent_*` only.
- Knowledge of the node's authority mode:
  `grep -E '^authority_mode' /etc/chv/agent.toml`.
  Shipped configurations select `core-managed`; a missing key defaults to
  `legacy`; `core-native` does not serve the legacy metrics/console stack.

## 1. Confirm scope and separate slow from failing

1. Query the p99 and the ok/error split per node or scrape target:

   ```promql
   histogram_quantile(0.99,
     sum by (instance, le) (
       rate(chv_vm_op_duration_seconds_bucket{op="start"}[5m])
     )
   )
   ```

   ```promql
   sum by (op, result) (rate(chv_vm_ops_total{op="start"}[5m]))
   ```

2. If `result="err"` is a meaningful share, stop here and use
   [VM Operation Failures](vm-op-failures.md). This runbook assumes the starts
   eventually succeed.
3. On the node, confirm the listener is healthy. It currently renders
   `chv_agent_*` only — the VM-op families are not exported
   ([metrics availability caveat](#metrics-availability-caveat), #702):

   ```bash
   curl -s http://127.0.0.1:9901/metrics | grep -c '^chv_agent_'
   ```

   When #702 lands, the same endpoint will also carry
   `chv_vm_ops_total{op="start"}` and
   `chv_vm_op_duration_seconds_{bucket,count,sum}{op="start"}`.

4. Record whether the latency is one VM, one node, or fleet-wide. A single
   node points at host/storage/network; fleet-wide points at a shared
   dependency (control plane dispatch, storage backend, or a common image).

## 2. Determine the authority mode and operation state

1. Check the mode (see §Prerequisites).
2. In `core-managed`, the gRPC acceptance is not completion: the Core journal
   executor performs the start. Inspect the operation journal through the
   node-local API:

   ```bash
   curl -s --unix-socket /run/chv/core/core-v1.sock \
     http://localhost/v1/operations \
     | jq '.[] | select(.operation.vm_id=="<VM_ID>")
           | {id:.operation.id, status:.operation.status,
              attempts:.operation.attempt_count,
              max_attempts:.operation.max_attempts,
              error, recovery_assessment}'
   ```

   - `accepted`: not yet claimed. A backlog or a fatal executor delays the
     start before any provider work happens.
   - `running` **without** `recovery_assessment`: in flight in the live
     process.
   - `running` **with** `recovery_assessment`: InspectRequired — the agent
     restarted mid-operation. Follow
     [Stuck Operations After Agent Crash](../OPERATIONS.md#stuck-operations-after-agent-crash-inspectrequired)
     instead of waiting for the latency to improve.
   - `failed` / `unsupported`: this is a failure, not latency — use
     [VM Operation Failures](vm-op-failures.md).
3. In `legacy`, the reconciler/AgentServer drives the start and the same
   operation journal does not exist. Use the agent logs and VMM state below.
4. In `core-native`, the native power endpoints return a structured
   `422 unsupported`; there is no legacy metrics/console stack. A start that
   is "slow" there is not this alert path — check that the caller is using a
   supported surface.

## 3. Find where the time went

1. Read the agent log for the VM and operation:

   ```bash
   # -o cat prints the raw log line; structured fields need CHV_LOG_FORMAT=json.
   journalctl -u chv-agent -o cat --since '-30 min' \
     | jq 'select(.fields.vm_id=="<VM_ID>")'
   ```

   Useful markers (exact strings from `chv-agent-runtime-ch`):

   - `booting vm via ch api` — the provider path has reached Cloud Hypervisor.
   - `vm already booted, skipping vm.boot` — idempotent no-op.
   - `vmm process exited, re-spawning` — the VMM was dead and had to restart.
   - `vm.boot returned non-success (VM may have auto-booted)` — investigate
     the VMM/API state.
   - `cannot determine vmm liveness; refusing to re-spawn` — a retry is
     required; the adapter will not guess.

   Compare the timestamp of `booting vm via ch api` with the operation start.
   A long gap **before** it is queue/resource preparation; a long gap **after**
   it is the Cloud Hypervisor call or the guest.

2. Check the executor backlog. A large number of `accepted` operations means
   starts are waiting for a claim, not running slowly. The executor admits a
   bounded queue and never overlaps effects for the same VM; work for other
   VMs may proceed concurrently.

3. Check host pressure:

   ```bash
   free -h
   uptime
   vmstat 1 5
   iostat -xz 1 3
   ps -eo pid,pcpu,pmem,comm --sort=-pcpu | head
   ls -l /dev/kvm
   ```

   Memory pressure, swap, CPU steal, and a saturated disk delay both VMM
   startup and `/vm.boot`.

## 4. Check the storage and network preparation path

The Core create/start effector opens storage through `chv-stord` and prepares
topology/NICs through `chv-nwd` before/with the Cloud Hypervisor work. Slow
storage or a wedged daemon shows up as latency here.

1. Storage:

   ```bash
   systemctl status chv-stord --no-pager
   journalctl -u chv-stord -n 100 --no-pager
   ls -l /var/lib/chv/storage/localdisk/
   ls -l /run/chv/stord/
   ```

   For LVM-class backends, confirm the volume group exists and is responsive.
   `chv-stord` runs as the `chv` user by design (see
   [Service-User Isolation](../specs/ops/service-user-isolation.md)); do not
   change its unit to "fix" latency.

2. Network:

   ```bash
   systemctl status chv-nwd --no-pager
   journalctl -u chv-nwd -n 100 --no-pager
   ip -br link
   ```

   Qualified networks are `br-<net_id>` managed by `chv-nwd`; the legacy
   `chvbr0` bridge is a dev convenience, not the qualified path.

3. Per-VM runtime artifacts:

   ```bash
   ls -la /var/lib/chv/agent/vms/<VM_ID>/
   cat /var/lib/chv/agent/vms/<VM_ID>/ch.pid
   tail -n 100 /var/lib/chv/agent/vms/<VM_ID>/console.log
   ```

   A missing `vm.sock`/`ch.pid` means the VMM never reached spawn; a present
   socket with a slow boot points at the VMM/guest.

## 5. Check the VMM and guest

1. Ask Cloud Hypervisor for its own state:

   ```bash
   curl -s --unix-socket /var/lib/chv/agent/vms/<VM_ID>/vm.sock \
     http://localhost/api/v1/vm.info | jq .
   ```

2. Inspect `console.log` / `console.log.last` for kernel boot, panic, or
   storage/network errors. Remember the alert stopped at `/vm.boot`; guest
   boot symptoms belong in the guest/console investigation, not this metric.
3. Check `cloud-hypervisor` processes and their age:

   ```bash
   pgrep -a cloud-hypervisor
   ```

## Remediation

1. **Queue/backlog:** let the executor drain. If the agent is healthy and the
   backlog is falling, no intervention is needed beyond capacity planning.
2. **Host pressure:** relieve memory/CPU/disk pressure (stop non-essential
   guests, free disk space, address I/O saturation). Do not start more VMs
   until the node is back under its capacity envelope.
3. **Storage/network daemon:** restart the failed daemon with
   `sudo systemctl restart chv-stord` or `chv-nwd`, then re-check its logs and
   the VM's operation state. Do not restart `chv-agent` for a stord/nwd
   problem.
4. **Dead VMM with a retryable path:** re-drive the start through the
   supported API/`chvctl`. The adapter's idempotent already-running path will
   not double-boot a live VM.
5. **InspectRequired:** do not retry blindly. Resolve the operation using the
   disposition that matches observed reality, per
   [OPERATIONS.md](../OPERATIONS.md#stuck-operations-after-agent-crash-inspectrequired).
6. **Guest-only slowness:** if the metric is healthy but users report slow
   boots, investigate the guest image, cloud-init, and console; do not tune
   this alert.

## Escalation

- **SEV-1 / page:** fleet-wide p99 breach, nodes unable to start any VM, or a
  storage backend failure affecting running guests.
- **SEV-2:** sustained single-node breach, repeated VMM re-spawns, or a
  backlog that is not draining.
- **SEV-3:** one isolated slow start with no user impact.
- Escalate to the storage owner for backend-specific stalls (LVM/iSCSI/Ceph)
  and to the VMM/runtime owner for repeated `cloud-hypervisor` crashes.

## What not to do

- **Do not `systemctl restart chv-agent` while starts are in flight.** The
  unit's `TimeoutStopSec=75` exists to let the executor drain for up to 60 s;
  a SIGKILL before that budget strands in-flight operations as
  InspectRequired.
- **Do not delete `core.db`, the journal, or the legacy cache** to "clear" a
  slow start.
- **Do not resolve an InspectRequired operation as `succeeded` without
  verifying the VM's actual state.**
- **Do not raise the 45 s alert threshold or widen the SLO** before the root
  cause is understood and recorded.
- **Do not treat a `status: ok` gRPC acknowledgment as proof the VM is
  running** in `core-managed` mode.
- **Do not chmod the agent socket or run the agent as another user** as a
  latency workaround.

## Related

- [VM Operation Failures](vm-op-failures.md)
- [BFF Errors](bff-errors.md)
- [`docs/OBSERVABILITY.md`](../OBSERVABILITY.md#sli-1-vm-start-latency-p99)
- [`docs/OPERATIONS.md`](../OPERATIONS.md#vm-wont-start)
- [Service-User Isolation](../specs/ops/service-user-isolation.md)
