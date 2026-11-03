# Runbook: VM Operation Failures (`VmOpFailureRateHigh`)

**Scenario:** The `VmOpFailureRateHigh` warning has fired — more than 5% of VM
lifecycle operations (create/start/stop/delete/pause/resume) are returning
errors for 5 minutes. The SLO is under 1%; 1–5% is degraded and above 5% is
breaching ([`docs/OBSERVABILITY.md`](../OBSERVABILITY.md#sli-2-vm-lifecycle-operation-error-rate)).
Use this runbook for **failed** operations. If operations are **succeeding but
slow**, use [VM Start Latency](vm-start-slow.md).

**Severity:** **SEV-2** for a sustained >5% error rate or multiple affected
VMs. **SEV-1** when create/delete failures risk data loss, a whole node's VMs
are affected, or a storage backend is failing. **SEV-3** for one failed
operation with no broader trend.

**Impact:** the affected lifecycle action is not completing. A failed delete
can leave stord/nwd handles open, and an ambiguous operation is quarantined
until an operator resolves it. Read the operation journal before retrying.

**Automation level:** The alert is automatic. `core-managed` accepts a
mutation durably and its executor retries an accepted operation up to the
fixed attempt bound; an already-running VM returns success idempotently. There
is no automatic resolution for a terminal `failed`/`unsupported` operation or
for an ambiguous `InspectRequired` one.

## Prerequisites

- `ssh` and `sudo` on the node, plus `chvctl` configured against the control
  plane.
- Access to the agent metrics listener (packaged default
  `127.0.0.1:9901`) or Prometheus.
- Knowledge of the node's authority mode:
  `grep -E '^authority_mode' /etc/chv/agent.toml`.
  Shipped configurations select `core-managed`; a missing key defaults to
  `legacy`; `core-native` does not serve the legacy metrics/console stack.

## 1. Scope the failures by operation

```promql
sum by (op) (rate(chv_vm_ops_total{result="err"}[5m]))
/
sum by (op) (rate(chv_vm_ops_total[5m]))
```

Also break down by node/scrape target. One `op` points at a specific path
(e.g. all deletes fail after a storage change); all ops failing points at the
node, the executor, or a shared dependency.

On the node:

```bash
curl -s http://127.0.0.1:9901/metrics \
  | grep -E 'chv_vm_ops_total\{.*result="err"|chv_vm_op_duration_seconds_count'
```

## 2. Determine the authority mode and read the real outcome

**In `core-managed` mode the gRPC acknowledgment means "accepted", not
"completed".** The Core journal executor performs the effect. Always read the
operation journal before concluding why a request failed:

```bash
curl -s --unix-socket /run/chv/core/core-v1.sock \
  http://localhost/v1/operations \
  | jq '.[] | select(.operation.vm_id=="<VM_ID>")
        | {id:.operation.id, kind:.operation.kind,
           status:.operation.status,
           attempts:.operation.attempt_count,
           max_attempts:.operation.max_attempts,
           error, result,
           recovery_assessment}'
```

| Journal state | Meaning | Action |
|---|---|---|
| `accepted` | Durable, not yet claimed by the executor | Check executor health/backlog; do not re-submit blindly |
| `running` without `recovery_assessment` | In flight in the live process | Let it finish or time out; check host/storage/network |
| `running` with `recovery_assessment` | **InspectRequired** — the owning process is gone; the VM is quarantined | Resolve via the RPC in §4 |
| `failed` | Terminal failure after a claim | Read `error` for the stable code; fix the cause, then re-drive |
| `unsupported` | The executor does not implement this operation | Use a supported surface; this is not retryable |

An operation starts with attempt count zero and has a fixed maximum of three
attempts. A terminal operation rejects every further claim. Success, failure,
and unsupported require a prior `running` claim.

**In `legacy` mode**, the reconciler/AgentServer owns the mutation surface;
use the agent logs and the control-plane task/operation record. The Core
operation journal and `chv_agent_journal_*` metrics are not the legacy path.

**In `core-native` mode**, the native power endpoints deliberately return a
structured `422 unsupported`; they do not enter the executor. Create/delete
are accepted through the native CRUD surface and executed by the composed
journal executor; `UpdateVm` is `unsupported` there. The legacy metrics
listener and console are absent. Do not expect a legacy task record.

## 3. Check the component named by the failure

1. **Control plane / task dispatch:** confirm the node is enrolled and the
   control plane can reach the agent socket. Check
   `journalctl -u chv-controlplane -n 200 --no-pager` for the operation ID.
2. **Agent:** read the structured log for the VM and operation:

   ```bash
   # -o cat prints the raw log line; structured fields need CHV_LOG_FORMAT=json.
   journalctl -u chv-agent -o cat --since '-30 min' \
     | jq 'select(.fields.vm_id=="<VM_ID>" or .fields.op=="<OP_ID>")'
   ```

   Recurring markers and their meaning:

   - `failed to start vm` — the provider start failed; inspect the wrapped
     error in the same event.
   - `vmm process exited, re-spawning` — the VMM died; the adapter is trying
     to recover.
   - `cannot determine vmm liveness; refusing to re-spawn` — the retry is
     required; the adapter will not risk a double-spawn.
   - `vm.boot returned non-success (VM may have auto-booted)` — check
     `vm.info` and the console before retrying.
   - `core journal executor terminated fatally` — the process exits non-zero
     for a supervisor restart; do not try to keep it alive.

3. **Storage (`chv-stord`):** deletes are especially sensitive because the
   effector drains volume sessions even when the hypervisor delete fails.

   ```bash
   systemctl status chv-stord --no-pager
   journalctl -u chv-stord -n 100 --no-pager
   ls -l /run/chv/stord/
   ls -l /var/lib/chv/storage/localdisk/ /var/lib/chv/storage/lvm/
   ```

   For a crash/restart that left volumes open, the documented manual drill is
   to list and close the leaked sessions through the stord
   `ListVolumeSessions`/`CloseVolume` RPCs, then detach the NIC through the
   nwd equivalent — see
   [`docs/specs/cellhv-core-journal-executor.md`](../specs/cellhv-core-journal-executor.md).

4. **Network (`chv-nwd`):** create/start failures often come from topology or
   NIC setup.

   ```bash
   systemctl status chv-nwd --no-pager
   journalctl -u chv-nwd -n 100 --no-pager
   ip -br link
   ```

5. **Host capacity:** disk full, memory pressure, and KVM/device problems
   produce deterministic failures:

   ```bash
   df -h /var/lib/chv /run
   free -h
   ls -l /dev/kvm
   dmesg | tail -n 50
   ```

6. **Per-VM artifacts:** a failed create/start may leave inspectable state:

   ```bash
   ls -la /var/lib/chv/agent/vms/<VM_ID>/
   tail -n 100 /var/lib/chv/agent/vms/<VM_ID>/console.log
   curl -s --unix-socket /var/lib/chv/agent/vms/<VM_ID>/vm.sock \
     http://localhost/api/v1/vm.info | jq .
   ```

## 4. Remediate

1. **Transient dependency failure:** restart the failed `chv-stord`/`chv-nwd`
   daemon, confirm its socket is back, then re-drive the mutation through the
   API/`chvctl`. Do not restart `chv-agent` for a dependency outage.
2. **Terminal `failed` operation:** read its `error` code, fix the cause, then
   submit the mutation again. A terminal operation is never re-run in place.
   Use a fresh idempotency key/operation identity for a genuinely new attempt.
3. **`unsupported`:** do not retry. Use the supported lifecycle surface for
   the mode. In `core-managed`, resize/spec updates are refused until the
   executor implements `UpdateVm`; in `core-native`, power actions are
   deliberately unsupported in v1.
4. **InspectRequired:** verify the real-world VM state, then resolve with the
   matching disposition (`succeeded` when the intent happened, `failed` when
   it did not). Follow
   [Stuck Operations After Agent Crash](../OPERATIONS.md#stuck-operations-after-agent-crash-inspectrequired).
   The resolution is replay-idempotent and records your note.
5. **Executor fatal / agent exit:** let systemd restart the agent. Then check
   the new startup classification; any operation that was running becomes
   InspectRequired. Do not loop-restart the unit.
6. **Storage data-loss risk:** stop destructive retries, preserve the volume
   and stord session state, and escalate to the storage owner before
   attempting delete again.

## Escalation

- **SEV-1 / page:** storage backend failure, delete failures that may leak or
  destroy data, a whole node unable to create/start any VM, or a crash-looping
  agent/executor.
- **SEV-2:** sustained >5% error rate, repeated VMM crashes, or a dependency
  that does not recover after one restart.
- **SEV-3:** one isolated failure with a clear, corrected cause.
- Escalate to the storage owner for LVM/iSCSI/Ceph/RBD and to the VMM/runtime
  owner for repeated Cloud Hypervisor faults.

## What not to do

- **Do not `systemctl restart chv-agent` (and never `kill -9`) while
  operations are in flight.** `TimeoutStopSec=75` is sized above the 60 s
  executor drain budget; a SIGKILL strands operations as InspectRequired.
- **Do not delete or edit `core.db`, the operation journal, or the legacy
  cache** to clear a failure.
- **Do not retry a destructive operation blindly** after an ambiguous
  failure. Inspect the operation and the volume/NIC state first.
- **Do not resolve InspectRequired as `succeeded` without checking that the
  VM is actually in the intended state.**
- **Do not treat `status: ok` in the gRPC acknowledgment as completion** in
  `core-managed` mode.
- **Do not manually stop `chv-stord` to "free" a volume** while VMs are
  running; drain or stop the guests first.
- **Do not raise the 5% threshold** before the root cause is understood.

## Related

- [VM Start Latency](vm-start-slow.md)
- [BFF Errors](bff-errors.md)
- [`docs/OPERATIONS.md`](../OPERATIONS.md#stuck-operations-after-agent-crash-inspectrequired)
- [`docs/specs/cellhv-core-operation-model.md`](../specs/cellhv-core-operation-model.md)
- [`docs/specs/cellhv-core-journal-executor.md`](../specs/cellhv-core-journal-executor.md)
- [Service-User Isolation](../specs/ops/service-user-isolation.md)
