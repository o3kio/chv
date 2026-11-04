# Nightly KVM failure analysis and pre-production gate assessment (2026-11-04)

**Scope:** the `[qual-nightly]` failure reported against `main` `a4da490d`, the
root-cause analysis of every scenario in that lane, the harness fixes, and what
the episode does — and does not — say about the pre-production gate (#666).

**Status:** failure root-caused and fixed; re-qualification run recorded below.
No candidate product defect was found in the failing path.

---

## 1. Summary

The full real-KVM lane run
[41894243197](https://github.com/o3kio/chv/actions/runs/41894243197)
(`a4da490d`) failed **all six** scenarios. The same shape had already failed
run [41836073033](https://github.com/o3kio/chv/actions/runs/41836073033)
(`428745d0`) hours earlier. The failure was **not** a candidate regression in
VM lifecycle, storage, or migration code:

- The control plane rejected every control-plane → agent dispatch with
  `invalid agent authentication token`. `scripts/integration/qual/deploy.sh`
  wrote the `[secrets] agent_auth_token` master only into
  `controlplane.toml`; with the #662/#693 secret split, the agent minted an
  unrelated random master at startup. Every create/start/volume operation
  failed closed. This is a **qualification-harness configuration bug**, not a
  product bug (packaged `install.sh` has always shared one `JWT_SECRET`).
- The two-stord scenarios (m4.6/m4.7 F5–F6/m4.8 P5) omitted `metrics_bind`.
  After #687 each stord binds `127.0.0.1:9101` by default, so the extra
  stords collided with the deployed stord and with each other. The first
  standalone stord exited at startup with `Address already in use`. This is
  also a **harness configuration bug**; a single-stord-per-host deployment is
  unaffected, but the default is not multi-instance-safe.
- A failed lane leaked `chv-agent`/`chv-stord`/`chv-controlplane` processes
  (the diagnostic found four still holding `:9100`/`:9101`/`:8080`/`:8443`).
  The teardown only pattern-killed `nwd` and relied on `pids.current`, which a
  scenario that dies early never writes. The leak turned the *next* lane's
  first stord start into a port collision.

One genuine **product** finding was filed from the same evidence: **#709** —
the first-enrollment `ReportBootstrapResult` is sent on the pre-enrollment
certificate and is rejected by the peer-node-id check. It is fail-closed and
non-fatal, but the CP never records bootstrap success on a fresh install and
audits a spurious denial.

The harness defects are fixed in `fix/659-qual-auth` (PR links below). The
nightly lane is a real tripwire: it caught configuration drift that the
packaged path would not have caught, because the harness generates its own
configs instead of reusing `install.sh`'s.

---

## 2. Evidence: what failed and why

Artifact: `/tmp/opencode/kvm-full-41894243197` (downloaded from the run);
runner evidence: diagnostic run
[41904796872](https://github.com/o3kio/chv/actions/runs/41904796872),
`/tmp/opencode/kvm-diag-41904796872/kvm-diagnostic-41904796872/host.txt`.

| Scenario | Observed failure | Mechanistic cause |
|---|---|---|
| m4.3-lifecycle | `guest kernel banner in console.log (timed out after 420s)`; no VM process ever started | `CreateVm`/`StartVm` dispatch rejected with `invalid agent authentication token` |
| m4.4-network | same kernel-banner timeout in Leg A | same auth rejection |
| m4.5-storage | `volume backing materialized + seed conversion complete (timed out after 60s)` | the volume is materialized by the agent; the dispatch never ran |
| m4.6-migration | `SRC stord did not come up`; `failed to bind metrics listener on 127.0.0.1:9101: Address already in use` | extra stord inherited the post-#687 default metrics port |
| m4.7-faults | `base VM volume backing materialized ... timed out after 120s` | same auth rejection |
| m4.8-perf-soak | six create cycles never converged; P4 anchor `never converged within 300s` | same auth rejection |
| teardown | `test dir preserved ...` but daemons survived | `pids.current` absent/stale; only nwd was pattern-killed |

The decisive log lines (from the preserved `agent.log` and
`controlplane.log`):

```
WARN chv_agent_core::agent_server: agent rejected a control-plane RPC
  authentication attempt reason="invalid agent authentication token"
WARN chv_controlplane_service::orchestrator: dispatch failed
  operation_type=CreateVm error=internal error: apply_vm_desired_state failed:
  code: 'The request does not have valid authentication credentials',
  message: "invalid agent authentication token"
```

Config evidence from `deploy.sh` at the failing commit:

- `controlplane.toml`: `jwt_secret = "qual-<hex>-jwt-secret"`
- `agent.toml`: **no `jwt_secret`, no `[secrets] agent_auth_token`**

With #693's derivation (`HMAC-SHA256(domain, jwt_secret)`), the two sides
therefore produced different tokens. `install.sh` writes the same
`JWT_SECRET` into both files, which is why the packaged path did not have the
same symptom.

Diagnostic host evidence also ruled out the environmental explanations that
were considered first:

- CH v53.0, firmware sha256 `4a0a1e97…` (matches the M2.5 prefix), patched
  image sha256 `ee977650…`, `qemu-img check`: `No errors were found`.
- `/dev/kvm` present, `kvm_amd.nested=1`, 12 vCPU / 31 GiB, 76 G free.
- No `console.log` was present in the preserved trees because no VM ever
  started (consistent with the dispatch failure, not a boot bug).

---

## 3. Fixes

Branch `fix/659-qual-auth` (based on `a4da490d`):

1. **Shared agent-auth master.** `deploy.sh` generates one `JWT_SECRET` and
   writes it to both `controlplane.toml` and `agent.toml`, mirroring
   `install.sh`. A runtime assertion fails the deployment if the two values
   ever diverge (`agent-auth master mismatch ...`).
2. **Isolated metrics listeners.** The deployed stord/nwd declare
   `127.0.0.1:9101`/`:9102`; every scenario-generated stord (m4.6 SRC/DST/N1b,
   m4.7/m4.8 SRC/DST) uses `127.0.0.1:0` (ephemeral), so extra instances can
   never inherit a shared default.
3. **Runner hygiene.** `nightly-run.sh` sweeps leaked qual processes and
   `/tmp/chv-qual-*` trees before preflight and fails loudly if any
   qualification port is still held. `deploy.sh` teardown pattern-kills
   `chv-agent`, `chv-stord`, and `chv-controlplane` scoped to the test dir,
   not only `nwd`.
4. **Failure observability.** On failure the driver copies each newly
   preserved `/tmp/chv-qual-failed-*` tree's logs/configs/console into
   `QUAL_ARTIFACT_ROOT/failed/<scenario>/` (capped per file) and writes a
   host snapshot (`ss`, `ps`, memory/disk, `dmesg` tail) to
   `logs/<scenario>-host-state.txt`. The previous run's root cause was
   invisible in the artifact because those trees never left the runner.
5. **Offline CI guard.** `tests/scripts/qual-harness-invariants.sh` asserts
   the shared-secret invariant, the per-scenario metrics binds, and the
   hygiene/observability hooks; it is wired into `ci.yml`.

Product finding filed separately (not fixed in this slice): **#709**.

---

## 4. Re-qualification evidence

> Filled from the dispatched runs. The lane is accepted only when
> `results.tsv` shows every requested scenario `pass` and the per-scenario
> evidence directories are present.

| Run | Lane | Candidate | Result |
|---|---|---|---|
| [41905691606](https://github.com/o3kio/chv/actions/runs/41905691606) | chaos (m4.7) | `90f97ddb` | _pending_ |
| _full lane_ | full (m4.3–m4.8) | _final fix commit_ | _pending_ |

---

## 5. Pre-production gate assessment

### 5.1 Gate scorecard (as of 2026-11-04)

| Gate element | State | Evidence / gap |
|---|---|---|
| P0 blockers (#646–#654) | **9/9 closed** | Wave 3.1 merges; tracker #666 |
| P1 (#655–#663) | **8/9 closed** | #659 is the only open P1 item |
| #659 coverage / nextest / real-BFF E2E | **landed and green** | Tier 1 table in `docs/dev/test-evidence-tiers.md`; main CI green |
| #659 real-KVM execution evidence | **open** | this incident; requires a green full lane on the final fix |
| #666 fresh-host demonstration | **open** | not yet demonstrated on the current `main`; #556 (quick-install) is also open |
| Main CI / Security / Nightly Packages / proto | **green** | run 41894194933 / 41894195280 / 41894195039 / 41894195048 at `a4da490d` |

### 5.2 What the failed run proves — and what it does not

**It does not prove a product regression.** The candidate never executed a VM
lifecycle operation: the control plane's dispatch was rejected before the
agent ran any handler. The failure is upstream of the code paths that m4.3–
m4.8 exercise.

**It does prove the gate is doing real work.** A lane that only re-ran a
maintainer's manual recipe would not have caught that #693 changed the
config contract for every generated deployment. The harness is now pinned to
the same invariant as `install.sh`, with an offline CI guard.

**It also exposes a residual process risk:** the qualification environment is
reconstructed by scripts, not by the packaged installer. Any change to the
config/secret/port contract must be reflected in `deploy.sh` and the scenario
generators. The new invariant test covers the two contracts that broke; it is
not a general proof of harness/config equivalence.

### 5.3 Readiness verdict

**Not yet a closed pre-production gate.** The codebase is P0-clean and its
P1 slices are merged, but the gate's own definition is evidence-based:

1. the real-KVM full lane must be green on the final fix commit, with
   `results.tsv` and per-scenario artifacts verified (this closes #659);
2. the definition must then be demonstrated on a fresh host (this closes
   #666), including the install/upgrade path and the documented exclusions.

Neither step is satisfied by the historical manual evidence alone. The
correct statement today is:

> **CHV is pre-production-ready in capability terms (P0/P1 code and
> documentation complete, main CI green), but the pre-production *gate* is
> not yet evidenced on the current `main`.** The limiting factor is
> qualification evidence, not a known candidate defect.

### 5.4 Residual risks that remain after #659 closes

These are unchanged from `m4.9-status.md` §3 and are not fixed by this slice.
They must be carried into any production-readiness decision:

- **Concurrent-write migration (#394)** — quiescent-source migration is the
  only claimed mode; dirty-block transfer under concurrent writers can lose
  data. This is the largest honest boundary.
- **Transient effector failure (#368)** — a journaled VM create can
  terminally fail with no re-drive; disclosed, not gated.
- **CH serial-console upstream defect / stop-wedge (#345 root)** — contained,
  not cured; re-verify at every Cloud Hypervisor upgrade.
- **UI firewall editor (#355/#360)** — operator-authored rules do not reach a
  node; legacy call sites can apply empty rulesets.
- **Snapshot/clone on core-managed nodes (#378)** — accepted then fails
  closed; UX gap.
- **Ownerless imported volumes (#386)**, **last-writer-wins upserts / clone
  TOCTOU (#384)**, **respawned stord config drop (#385)**, **receiver-only
  stord (#401)**, **mTLS rejection observability (#402)**,
  **delete-path kill tolerance (#351)**, **JWT/encryption-key coupling
  (#336)**.
- **Backup/restore is hard-fenced**, not DR-validated.
- **Single-host, nested-KVM, quiescent, serial-lifecycle evidence only** —
  no multi-host, HA, Ceph/iSCSI, or scale claim.
- **Multi-stord-per-host metrics default** — a deployment running more than
  one stord on a host must set distinct `metrics_bind` values (the harness
  now does; this is not yet documented as an operator requirement).

### 5.5 Recommended plan

1. **Land the harness fix** (`fix/659-qual-auth`) after the offline guard and
   green CI.
2. **Chaos lane** on the fix (m4.7) to validate auth, volume, and the
   two-stord legs quickly.
3. **Full lane** on the final commit; verify `results.tsv`, per-scenario
   evidence, and that the artifact now contains the preserved failed-tree
   logs/host snapshot. Close #659 with the run links.
4. **Fresh-host demonstration** of the documented install + upgrade +
   uninstall path on the same candidate; close #666 only then. The #556
   quick-install leg is a separate, still-open claim.
5. **Publish the residual-risk list** (5.4) with the release notes so the
   supported-envelope statement in `docs/DEPLOYMENT-ARCHITECTURE.md` §7 is
   not read as broader than the evidence.
6. **Follow up #709** (first-enrollment bootstrap result) as the next
   correctness/audit item; it is not a gate blocker but it weakens the
   fresh-install audit trail.
