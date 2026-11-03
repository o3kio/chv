# #385 design writeup — supervisor respawn config fidelity (stord drops operator `stord.toml` keys)

**Issue:** kubedoio/chv#385 — the agent supervisor's generated respawn
config (`crates/chv-agent-core/src/supervisor.rs`, `start_daemon`) writes
only `socket_path`, `runtime_dir`, `log_level`, and (since #377)
`path_allowlist`. Every other key the operator configured in
`stord.toml` is silently dropped when the supervisor respawns a dead
stord.

**Status:** FINAL — investigation + maintainer decision (§6.1,
2026-10-05). The adopted fix (C1, pass-through respawn via the opt-in
`stord_config_path` key, with degrade-to-generated fallback) is
implemented for stord only; nwd is tracked as #504. Evidence cited
`file:line` at main `728fcd5f`.

---

## 1. Problem statement

When the agent's `DaemonSupervisor` respawns a dead stord, it writes a
minimal `chv-stord.toml` into the *agent's* runtime dir and execs
`chv-stord` with it (`supervisor.rs:221-245`). The generated file
carries four keys (`socket_path`, `runtime_dir`, `log_level = "info"`,
and — when `AgentConfig.stord_path_allowlist` is non-empty —
`path_allowlist`, `supervisor.rs:73-92`). Everything else the operator
put in their `stord.toml` is gone in the respawned daemon:

1. **`path_allowlist` drift (the #376 failure mode transposed).**
   #377 wires the allowlist from `agent.toml`'s
   `stord_path_allowlist` (`chv-config/src/lib.rs:466-474`), and
   `install.sh`/`deploy.sh` copy the same literal list into both files
   at *install* time (`scripts/install.sh:1019` vs `:1028`;
   `scripts/integration/qual/deploy.sh:422` vs `:434`) — there is no
   live link. An operator who edits `stord.toml`'s allowlist without
   mirroring `agent.toml` gets a respawned stord that drops the added
   prefix → locators under it become `AccessDenied` after a stord
   crash/restart.
2. **`backend_type` dropped.** The respawned stord always runs the
   `local` backend (`cmd/chv-stord/src/main.rs:53-87` defaults to
   `"local"` when `backend_type` is absent), regardless of the
   operator's LVM/iscsi/ceph configuration. Adjacent to #379 — but see
   §2.6: stord honors `backend_type` *today*; #379 only gates whether
   the control plane dispatches to non-local backend classes.
3. **`device_allowlist` and the `[migration]` block dropped.** Omitting
   `[migration]` means migration disabled — fail-closed in direction,
   but a silent posture change (an operator running migration receivers
   loses the inbound listener after a respawn;
   `cmd/chv-stord/src/main.rs:94-132`).

The issue proposes two fix directions: (a) minimal — a spawn-time
cross-check that warns when the live `stord.toml` contains keys the
generated config would drop; (b) structural — read/merge the operator's
`stord.toml` as the *base* for the respawn config instead of generating
from scratch. This writeup verifies the premises, enumerates the full
key surface, and evaluates the options (plus a third found during the
investigation).

## 2. Ground truth at `728fcd5f`

### 2.1 What the supervisor generates, verified

`start_daemon` (`supervisor.rs:199-254`) writes
(`supervisor.rs:221-227`):

```toml
socket_path = <agent's stord_socket>
runtime_dir = <agent's runtime_dir>      # NOT the operator's stord runtime_dir
log_level = "info"                        # hardcoded
<path_allowlist = [...] only when agent stord_path_allowlist non-empty>
```

Construction: `DaemonSupervisor::new` at `cmd/chv-agent/src/main.rs:974-981`
passes `config.stord_binary_path`, `config.stord_socket`,
`config.runtime_dir` (the **agent's**), and
`config.stord_path_allowlist`. The respawn check runs on the agent's
5 s main-loop tick (`cmd/chv-agent/src/main.rs:1020`, `:1078`) with a
5 s restart throttle (`supervisor.rs:8`, `MIN_RESTART_INTERVAL`). If an
external daemon is already listening on the socket, the supervisor
skips spawning entirely (`supervisor.rs:211-214`) — the respawn path is
a fallback that fires when the socket is dead, racing whatever else
supervises stord (systemd in the `install.sh` layout —
`scripts/install.sh:1350` enables `chv-stord.service`, whose unit is
`Restart=on-failure`, `docs/examples/systemd/chv-stord.service:16-17`).

Two fidelity facts the issue understates:

- **`runtime_dir` is written but with the wrong value.** The generated
  config uses the agent's runtime dir, not the operator's stord
  runtime dir. This is the *recorded* "#376 relocation" trap
  (m4.5-storage.md §4.2, `:82-97`): the sessions DB (`stord.db`) and
  the local backend's snapshot/clone destination files relocate on
  respawn (`m4.5-storage.md:211-215`, §5 "Layer truths"). The agent
  compensates by re-driving volume opens, but two `stord.db` files with
  disjoint session rows remain.
- **`log_level` is written but hardcoded** to `"info"` — an operator's
  `debug`/`warn` choice is dropped (minor).

### 2.2 The full `stord.toml` key surface and classification

`StordConfig` (`crates/chv-config/src/lib.rs:233-269`), parsed by
`load_stord_config` (`lib.rs:380-387`) — the file *replaces* the
`Default` impl; absent `#[serde(default)]` keys deserialize to the
**type** default (empty vec / `None`), *not* the `Default` impl's
values (`lib.rs:356-378`). stord consumes it once at startup
(`cmd/chv-stord/src/main.rs:36-37`); there is no reload/SIGHUP path.

Classification against the supervisor-generated respawn config:

| Key (`lib.rs` line) | Mirrored by respawn config? | Effect when dropped | Class |
|---|---|---|---|
| `socket_path` (:235) | **yes** (agent's `stord_socket`) | — | supervisor-owned, correct |
| `runtime_dir` (:236) | **wrong value** (agent's dir, not operator's) | sessions DB + local-backend snapshot/clone files relocate (`m4.5-storage.md:211-215`) | **dangerous** (data/silence trap, recorded) |
| `log_level` (:237) | hardcoded `"info"` | operator's level lost | safe (diagnostic only) |
| `path_allowlist` (:241) | only via `agent.toml` copy (#377) | empty = **allow-all** (`handlers.rs:234-235`) — confinement lost; conversely, operator-added prefixes missing → `AccessDenied` on locators (`handlers.rs:274-282`) | **dangerous, both directions** |
| `device_allowlist` (:243) | **dropped** | empty = allow-all device paths (`handlers.rs:311-313`); operator's `/dev/dm-*` restriction lost | **dangerous (fail-open)** |
| `backend_allowlist` (:239) | **dropped** — *missed by the issue* | empty = all backend classes allowed on provision (`handlers.rs:194-196`, call site `:385`) | dangerous (fail-open, security posture) |
| `migration_dest_allowlist` (:250) | **dropped** — *missed by the issue* | empty = all migration destinations allowed (`handlers.rs:207-208`) | dangerous (fail-open) |
| `backend_type` (:247) | **dropped** | defaults to `"local"` (`main.rs:53`) → LVM/iscsi/ceph deployment respawns as local backend; daemon looks healthy, serves the wrong backend | **dangerous (data availability)** |
| `iscsi` (:253), `ceph` (:256), `lvm_volume_group` (:260) | **dropped** | inert without `backend_type` (backend constructors read them only under the matching `backend_type`, `main.rs:54-87`) | dangerous only in combination with `backend_type` |
| `[migration]` (:268; struct `:276-329`: `enabled`, client half `client_cert_path`/`client_key_path`/`ca_cert_path`/`dest_server_name`, receiver half `listen_addr`/`server_cert_path`/`server_key_path`/`client_ca_path`) | **dropped** | `enabled = false` default → migration actions fail unavailable; inbound receiver listener silently disappears (`main.rs:94-132`) | safe *direction* (fail-closed), silent posture change |
| `metrics_bind` (:244) | dropped | at the time, a **dead key** (parsed but unread). Since #657 `chv-stord` binds and serves its own `/metrics` listener from this key (loopback default `127.0.0.1:9101`) | inert then; now a real scrape surface |

Summary: of 13 top-level surfaces, the respawn config mirrors 1
correctly (`socket_path`), mirrors 2 with wrong/hardcoded values
(`runtime_dir`, `log_level`), mirrors 1 through a stale install-time
copy (`path_allowlist`), and drops 9 — of which 5 are dangerous
(`runtime_dir`, `path_allowlist`, `device_allowlist`,
`backend_allowlist`, `backend_type`+backend sections) and one
(`migration_dest_allowlist`) is fail-open. `metrics_bind` is inert in
stord today regardless.

### 2.3 The install-time "sync", verified

- `scripts/install.sh` writes `agent.toml` (`:991-1022`, allowlist
  `:1016-1019`) and `stord.toml` (`:1024-1032`, allowlist `:1028`,
  `device_allowlist` `:1029`) with the *same literal list* — synced at
  install only. Note the two files are also shaped differently:
  `stord.toml` carries `device_allowlist`, `agent.toml` has no
  equivalent key to mirror it with.
- `scripts/integration/qual/deploy.sh` does the same (`:422` vs `:434`)
  and starts stord *directly*, not under systemd (`:509-511`) — so in
  the qual topology the agent's supervisor is the **only** respawn
  mechanism, which is why the m4.5 scenario can assert respawn parity
  (m4.5-storage.md §4.2, `:96-97`).
- The m4.5 scenario already discovers the live stord's config path by
  reading `/proc/<pid>/cmdline` (`scripts/integration/qual/m4.5-storage.sh:546-560`,
  `stord_config_path()`) — a precedent for acquisition-by-discovery,
  but it only works while the daemon is alive.

### 2.4 Docker-compose posture, verified

- `docker-compose.yml`: the stord container's inline `stord.toml` has
  only the three base keys — no allowlist at all (`:60-66`,
  allow-all). The agent sets `stord_binary_path = "/dev/null"`
  (`:114`) and **no** `stord_path_allowlist` — confirmed. With
  `/dev/null` as the binary, a respawn attempt fails at
  `Command::spawn` (`supervisor.rs:247-250`) — no respawn happens, so
  the pre-#376 allow-all posture is moot there today.
- `docker-compose.prod.yml`: the stord container *does* set
  `path_allowlist` + `device_allowlist` (`:99-100`); the agent again
  has `stord_binary_path = "/dev/null"` (`:194`) and no
  `stord_path_allowlist`. Same conclusion: no respawn possible. If
  someone later points `stord_binary_path` at a real binary, the
  respawned stord would drop both allowlists — the issue's
  parenthetical holds.

### 2.5 The live-config question (acquisition routes)

**Does stord expose its running config?** No. The stord gRPC surface is
volume/session/migration operations only — `proto/node/chv-stord-api.proto:185-199`
(`OpenVolume` … `ResumeDiskMigration`); there is no status/admin/
config-report RPC, and no SIGHUP/reload path (config is read once,
`cmd/chv-stord/src/main.rs:36-37`). A supervisor could not query the
*running* daemon's effective config; it can only read the file.

**Does the supervisor know the operator's `stord.toml` path?** No.
`AgentConfig` (`chv-config/src/lib.rs:438-481`) has `stord_socket`,
`stord_binary_path`, `stord_path_allowlist` — no config path. The
conventional location is `/etc/chv/stord.toml`
(`CHV_CONFIG_DIR="/etc/chv"`, `scripts/install.sh:51`; unit
`ExecStart=/usr/bin/chv-stord /etc/chv/stord.toml`,
`docs/examples/systemd/chv-stord.service:16`), but nothing in the
agent knows or enforces it.

**Readability.** In the `install.sh` layout the agent runs as `chv`
(`docs/examples/systemd/chv-agent.service:8`), `stord.toml` is
`root:chv-stord 0640` (`scripts/install.sh:1031-1032`), and install
adds the agent user to the `chv-stord` group
(`scripts/install.sh:221`) — so the agent *can* read it, but only via
an install-time group-membership side effect. In the prod compose
topology stord runs as `chv-stord:chv-stord`
(`docker-compose.prod.yml:92`) in a separate container from the agent
— the agent there could not read the file (moot while
`stord_binary_path = /dev/null`).

**Failure modes per route:**

| Route | Failure modes |
|---|---|
| New explicit `AgentConfig` key (e.g. `stord_config_path`) | unset (default → today's generated behavior); path moved/typo'd (detectable: read fails); file unreadable (permissions — detectable); malformed TOML (parse error — detectable) |
| Convention `/etc/chv/stord.toml` | file legitimately elsewhere (qual deploy writes it under `$TEST_DIR`, `deploy.sh:430`); silent wrong-file risk if a stale `/etc/chv/stord.toml` exists; no way to distinguish "deliberately absent" from "not yet installed" |
| `/proc/<pid>/cmdline` discovery (m4.5 precedent) | only works while the daemon is alive — dead by respawn time; PID reuse; guarded in the scenario precisely because an empty pid reads the *host* cmdline (`m4.5-storage.sh:552-554`) |
| Run-dir snapshot written by stord at startup | stale relative to post-start operator edits (an edit followed by a crash respawns with the *old* config — drift persists); snapshot write adds a new permissions-sensitive artifact |

The malformed-TOML and permission failures are detectable at respawn
time and must pick a policy: fail the respawn (stord stays down until
operator intervention) vs degrade to the generated config (today's
behavior, possibly fail-open). See §5.

### 2.6 Interaction with #379 (open)

#379 is **OPEN** as a recorded design decision: "LVM unreachable from
the VM lifecycle (backend class hardcoded 'local')"
(`docs/evidence/.../m4.9-status.md:107`). Current state:

- The **stord side already honors `backend_type`** (`main.rs:53-87`)
  and the LVM backend is qualified at the stord layer (m4.5 run 10;
  `m4.9-status.md:40`).
- What #379 tracks is the **control-plane side**: volume models hardcode
  backend class `"local"`, so the VM lifecycle never dispatches to LVM
  (`m4.5-storage.md:122`, §4.5).

Premise sharpening for #385: the `backend_type` drop is **not**
conditional on #379 landing. An operator who configures
`backend_type = "lvm"` in `stord.toml` today (supported, qualified at
the stord layer) and whose stord crashes gets an agent-respawned
*local* backend with `runtime_dir` = the agent dir — a daemon that
reports healthy on the socket while serving an entirely different
backend and data location. If #379 lands, the same drop becomes a
mainline data-availability break instead of an edge case. Ordering:
#385's structural fix should land **before or with** #379's
control-plane work; #379 makes no sense to operate on top of a respawn
path that silently discards `backend_type`.

### 2.7 Sibling surface: nwd has the same defect class

`start_nwd` generates the same three-key config with `extra_config =
""` (`supervisor.rs:95-105`), but `NwdConfig` carries `overlay`,
`ebpf`, and `fabric` sections (`chv-config/src/lib.rs:389-404`) that
nwd *does* consume (e.g. `config.fabric.enabled` → fabric provider,
`cmd/chv-nwd/src/main.rs:36-44`). A respawned nwd silently drops them.
The shipped layouts don't configure those sections today
(`install.sh:1034-1042`, `deploy.sh:437-442`), so it's latent — but
any structural fix should be designed to generalize to nwd rather than
solve stord only.

## 3. Goals & non-goals

**Goals**

- A respawned stord must preserve the operator's `stord.toml` posture —
  every key the operator set, including keys added in the future.
- The supervisor must keep owning `socket_path` (it must point at
  `AgentConfig.stord_socket` or the agent's health check never
  recovers).
- Never worse than status quo on any failure of the acquisition route.
- Backwards compatible: deployments that don't opt in keep today's
  generated-config behavior.

**Non-goals** (scope boundaries, §8 restates): the install-time sync
scripts' own drift; #379's control-plane backend dispatch; the compose
files' posture; stord reload/SIGHUP; making the fallback generated
config itself fully faithful (unless the maintainer opts in, §5 DP4).

## 4. Options

### Option A — spawn-time cross-check warn (the issue's minimal fix)

**Shape.** The supervisor reads the operator's `stord.toml` at respawn
time (acquisition still required — see below), diffs it against the
config it is about to generate, and emits `warn!` for every key that
would be dropped or change value.

**What exactly is compared.** Key-set presence is insufficient:
`path_allowlist` is *present* in the generated config but with a stale
value (the #377 drift). The check must compare parsed values for every
key in `StordConfig`, i.e. deserialize the operator file into
`StordConfig` and diff against the would-be generated `StordConfig`
field by field (both types are `Clone`/comparable; `StordConfig` has no
`PartialEq` today — a small derive). `log_level` and `runtime_dir`
drift would also surface (they differ in every install.sh deployment —
see DP4).

**When.** Every respawn (`start_stord`) is the natural point; a
per-tick check (every 5 s, `main.rs:1020`) would spam the log for a
daemon that is down and throttled. Respawn-only means the warning fires
at most once per restart episode.

**What the operator sees.** A `warn!` line in the agent's stderr/log
capture naming the dropped keys. That's all.

**Does it prevent the harm? No.** Locators still break post-respawn;
the backend still flips to local; migration receivers still disappear.
The warn converts a *silent* posture change into a *diagnosable* one —
it shortens the m4.5-style forensics but does not close the issue's
failure mode.

**Hidden cost.** Option A needs the same acquisition route (a
`stord.toml` path the supervisor can read) as the structural options,
plus the diff machinery. It is *not* meaningfully cheaper than Option
B/C1 on the only hard part (acquisition); it is cheaper only on the
write side.

### Option B — merge operator TOML as the base (the issue's structural fix)

**Shape.** At respawn, the supervisor reads the operator's
`stord.toml`, parses it into a TOML document (not `StordConfig` — see
below), and writes a respawn config that is the operator document with
exactly one override: `socket_path` = `AgentConfig.stord_socket`. The
supervisor keeps owning `socket_path`; everything else — including
`runtime_dir`, `path_allowlist`, `backend_type`, `[migration]` — comes
from the operator file. The generated-from-scratch config remains the
fallback when the operator file is unset/unreadable/malformed.

Key design points:

- **Merge at the document level, not the struct level.** Deserializing
  into `StordConfig` and re-serializing would drop unknown keys (future
  stord.toml additions would be silently eaten — the exact defect class
  being fixed) and can't round-trip `Option`/section shapes faithfully.
  A `toml::Value`/`toml_edit` document with a single keyed override
  preserves everything, including comments (with `toml_edit`).
- **Which keys the supervisor still owns.** Only `socket_path`
  (health-check contract). `runtime_dir` deliberately comes from the
  operator file — that *fixes* the recorded relocation trap
  (§2.1): the respawned stord reuses the same `stord.db` and the same
  snapshot/clone destination dir as the daemon it replaces.
- **`path_allowlist` precedence.** If the operator file sets it, it
  wins; `AgentConfig.stord_path_allowlist` becomes the fallback-source
  for the *generated* config only (back-compat). Document that both
  routes are operator-controlled files in `/etc/chv`.
- **Malformed-file fallback.** Fail the respawn vs fall back to
  generated. Falling back to the generated config is *today's
  behavior* — never worse than status quo — provided the fallback still
  carries `stord_path_allowlist` (#377). Failing the respawn is more
  fail-closed but keeps storage down on a config typo, and in the
  install.sh layout systemd may be racing to restart stord anyway
  (§2.1). Recommend: parse failure → loud `warn!` + generated fallback
  (never worse than today); see DP3.
- **Security / trust boundary.** Does honoring operator `stord.toml`
  reintroduce anything #376/#377 closed? No — the boundary is
  unchanged. #376's fix exists because the supervisor *generated* a
  config from nothing; its threat model (handlers.rs:226-232: hostile
  API callers, not hostile local users) doesn't care which root-owned
  file the policy came from. `agent.toml` and `stord.toml` are both
  root-owned operator files (`install.sh:1021-1022`, `:1031-1032`), and
  the agent already execs stord as its own child with the agent's
  privileges. The one genuine new surface: a respawned stord may now
  open the `[migration]` receiver listener — but that's *restoring*
  the posture the operator configured and the fresh daemon had; the
  current drop is the anomaly. Guard to add: refuse the merge (fall
  back + warn) if the operator file's permissions are looser than the
  agent's own config (e.g. world/group-writable), so the respawn path
  can't become a "edit stord.toml, crash stord, get policy applied"
  shortcut for a user who couldn't edit `agent.toml`.

**Tradeoffs.** Highest fidelity; fixes the relocation trap; needs the
acquisition key; adds a document-merge code path (moderate complexity);
the generated fallback remains lossy.

### Option C — alternatives found during investigation

**C1 (recommended): pass-through respawn — exec stord with the
operator's config file directly.** New `AgentConfig` key
`stord_config_path: Option<PathBuf>`. When the supervisor must respawn
stord and the key is set, it (1) reads and parses the file, (2)
verifies its `socket_path` equals `AgentConfig.stord_socket`, and (3)
execs `chv-stord <operator-config-path>` — **no config is generated at
all**. On unreadable/malformed/socket-mismatch: loud `warn!` + fall
back to today's generated config.

- *Fidelity:* complete and future-proof — every key, including keys
  added to `stord.toml` in later releases, survives respawn by
  construction. Option B needs a re-verified merge every time the
  config schema grows; C1 is schema-independent.
- *Complexity:* minimal — no serializer, no merge, no `PartialEq`
  derives; reuses `load_stord_config` for the validation parse.
- *Security:* identical trust boundary to systemd doing
  `ExecStart=/usr/bin/chv-stord /etc/chv/stord.toml`
  (`docs/examples/systemd/chv-stord.service:16`) — the agent merely
  takes over systemd's role for the crash window. The loose-permission
  guard sketched for Option B is deliberately **deferred, not
  implemented**: as-built validation is parse + socket-path match only
  (per the §6.1 decision), the permission posture is the standard
  install's (`root:chv-stord 0640`, `install.sh:1038-1039`; agent
  group-read via the `chv-stord` group, `install.sh:221`), and a
  deployment with a looser `stord.toml` relies on the same trust it
  already places in systemd applying that file — a permission check
  would spuriously fall back on legitimately group-writable configs in
  non-standard deployments.
- *Fixes the relocation trap:* yes — the respawned daemon reads the
  operator's `runtime_dir`.
- *Drawback:* the agent's log line "starting chv-stord" currently
  records the generated config path (`supervisor.rs:246`); with C1 the
  *effective* config is the operator file (better for forensics, but
  the m4.5 scenario's config-assertion helper that resolves the
  config via `/proc/<pid>/cmdline` — `m4.5-storage.sh:546-560` — now
  resolves the operator file; the scenario's parity assertion gets
  *stronger*, not broken).
- *nwd generalization:* trivially extends to `nwd_config_path`
  (§2.7) with the same shape.

**C2: agent-owned source of truth + `chvctl stord-sync`.** Keep two
files, add a command that copies the effective stord posture into
`agent.toml`. Rejected as a primary fix: it detects or repairs drift
on demand but doesn't remove the failure mode (an operator who doesn't
run it gets the same break), and it needs a writable-`agent.toml`
story the platform doesn't have (config is root-owned; `chvctl` runs
as the operator).

**C3: stord writes its startup config to a run-dir snapshot; the
supervisor reuses it.** Rejected: stale relative to post-start operator
edits (edit → crash → respawn uses the *old* config — drift persists in
exactly the #385 scenario), and it adds a permissions-sensitive
artifact under the runtime dir.

### Decomposition

The two sub-problems are separable but share the acquisition key:

- **path_allowlist drift fix** = "the supervisor must see the
  operator's stord posture" — solved by acquisition + any of A/B/C1.
- **backend_type / `[migration]` / device_allowlist fidelity** =
  "the respawn config must carry the operator's keys" — solved only by
  B or C1 (A merely warns).

Since acquisition is the shared hard part and A doesn't close the
failure mode, decomposing into "A now, structural later" buys little.
A clean decomposition that *does* make sense: **C1 for stord in this
issue; generalize to nwd as a follow-up** (§2.7), leaving the
generated-fallback fidelity (DP4) explicit.

## 5. Decision points (maintainer)

**DP1 — structural approach: C1 (pass-through) vs B (merge) vs A
(warn-only).** *Recommendation: C1.* It is the smallest structural fix
(comparable in size to A once acquisition is paid for), gives complete
and schema-independent fidelity, fixes the recorded `runtime_dir`
relocation trap for free, and generalizes to nwd. Choose B only if
there is a requirement that the respawned daemon's config always be a
file the supervisor wrote (e.g. for auditability of what was exec'd) —
C1 can approximate that by logging the resolved operator path and its
mtime/hash at respawn. A alone does not close the issue's failure mode
and should at most ride along as a warn in the fallback path.

**DP2 — acquisition route.** *Recommendation: new explicit
`AgentConfig.stord_config_path: Option<PathBuf>`, default `None` =
today's generated behavior.* Explicit beats convention (the qual
deploy keeps `stord.toml` outside `/etc/chv`, `deploy.sh:430`;
convention would silently miss it) and beats `/proc` discovery (dead
daemon). `install.sh`, `deploy.sh`, and the example
`docs/examples/agent.toml` set it. Alternative worth considering:
default it to `/etc/chv/stord.toml` when the key is absent *and* that
file exists — more zero-config drift protection, but it makes the
agent act on a file the operator never pointed it at (surprise factor,
and the prod-compose cross-user readability question); recommend
keeping opt-in for v1 and revisiting the default later.

**DP3 — malformed/unreadable operator config at respawn.**
*Recommendation: loud `warn!` + fall back to today's generated config
(still carrying `stord_path_allowlist`).* Rationale: the fallback is
byte-for-byte today's behavior — never worse than status quo — whereas
failing the respawn keeps storage down on a config typo and interacts
badly with the systemd race (§2.1). The socket-path-mismatch case
(currently not validated anywhere) must also fall back + warn: execing
a stord that listens elsewhere would wedge the agent's health check
permanently. If the maintainer prefers strict fail-closed (respawn
refused, operator paged), that is defensible — but it should be a
deliberate availability tradeoff, not an accident.

**DP4 — the fallback path's own fidelity (the recorded relocation
trap).** With C1, deployments that set `stord_config_path` get full
fidelity; deployments that don't keep the lossy generated config —
including the `runtime_dir` relocation (§2.1) that m4.5 records as
current truth. *Recommendation: leave the fallback as-is in this issue
(status-quo preservation, back-compat pin
`supervisor.rs:461-476`), and record that the relocation trap is only
closed for opt-in deployments.* Optional add-on if cheap: also honor
`stord_runtime_dir` from `AgentConfig` in the generated config — but
that is a behavior change for existing opt-outs and deserves its own
issue.

**DP5 — ordering vs #379.** *Recommendation: land #385's structural
fix before or alongside #379's control-plane work.* #379 makes
`backend_type` a mainline dispatch input; operating it on top of a
respawn path that drops `backend_type` converts an edge-case
availability break into a guaranteed one on any stord crash (§2.6).

### 6.1 Decision (maintainer, 2026-10-05)

All recommendations adopted, as written:

1. **Structural approach: C1 (pass-through).** When the supervisor must
   respawn stord and the acquisition key is set, it execs
   `chv-stord <operator-config-path>` directly — no config is
   generated at all, so every operator key (including keys added to
   `stord.toml` in future releases) survives respawn by construction.
   The supervisor validates only that the operator config parses and
   that its `socket_path` matches `AgentConfig.stord_socket`; options A
   (warn-only) and B (document merge) are rejected — A does not close
   the issue's failure mode, and B re-opens a merge surface every time
   the config schema grows.
2. **Acquisition: explicit opt-in key.** New
   `AgentConfig.stord_config_path: Option<PathBuf>`, default `None` =
   today's generated-config behavior byte-exactly. No `/etc/chv`
   convention fallback (the qual deploy keeps `stord.toml` outside
   `/etc/chv`; a convention would silently act on a file the operator
   never pointed the agent at). `install.sh` sets the key for standard
   installs (it writes both files and knows the path); the docs carry
   the opt-in contract for hand-managed deployments.
3. **Malformed/unreadable config or socket-path mismatch at respawn:
   degrade to the generated config with a loud warn** — never worse
   than status quo. The socket-mismatch fallback is mandatory: execing
   a stord whose socket lives elsewhere would wedge the agent's health
   check permanently.
4. **Fallback path fidelity: left as-is.** The generated fallback keeps
   today's shape exactly (the byte-compat pin
   `supervisor.rs:461-476` at `728fcd5f` stays green); the recorded
   `runtime_dir` relocation trap (§2.1) is closed only for opt-in
   deployments, per DP4.
5. **Scope: stord only.** The nwd spawn path is deliberately untouched
   and tracked as issue #504; the pass-through shape generalizes
   trivially (`nwd_config_path`) when that lands.
6. **Ordering: this lands before #379** (informational — #379's
   control-plane backend dispatch builds on a respawn path that no
   longer silently discards `backend_type`).

## 6. Test strategy (per option)

Existing pins to keep green (line refs at the pre-change base
`728fcd5f`): `supervisor_generated_stord_config_carries_path_allowlist`
(`supervisor.rs:431-479`, including the byte-compat empty-shape pin
`:461-476`), the restart/throttle tests (`:354-424`), and the m4.5
qual scenario's respawn-parity leg (m4.5-storage.md §4.2).

- **Option A:** new unit test — write an operator-shaped `stord.toml`
  fixture (with `backend_type`, `device_allowlist`, `[migration]`, and
  an extra allowlist prefix) into a temp dir; construct the supervisor
  with the fixture path; trigger `start_stord`; capture the agent's log
  with the `tracing` subscriber convention already used in
  `cmd/chv-stord/src/main.rs:231-292` (`log_capture`); assert a warn
  naming each dropped key fires, and that the generated config is
  unchanged (no behavior change).
- **Option B:** unit tests asserting the written respawn config equals
  the operator document with only `socket_path` overridden — including
  a fixture with a *future/unknown* key, to pin document-level merge
  (struct-level merge would fail this test). Malformed fixture →
  fallback shape + warn. World-writable fixture → fallback + warn.
  Socket-mismatch fixture → fallback + warn.
- **Option C1:** unit tests with the existing fake-daemon harness
  (`supervisor.rs:262-289`): (a) happy path — respawn execs the fake
  script with the operator path as `argv[1]` (the fake script can
  `cat` its argument into the runtime dir to make it assertable);
  (b) `stord_config_path = None` → generated config, byte-compatible
  with today (reuse the existing pin); (c) malformed/unreadable/
  socket-mismatch → generated fallback + warn; (d) the respawned
  daemon's config carries `backend_type` and `[migration]` from the
  operator fixture (fidelity pin).
- **Integration/qual (any option):** extend the m4.5 respawn-parity
  leg into a drift test — after install, edit `stord.toml` (add an
  allowlist prefix, set `backend_type`), kill stord, let the supervisor
  respawn, then assert either the operator keys survive (B/C1: locator
  under the new prefix opens; effective backend unchanged) or the warn
  fired (A). The scenario's `stord_config_path()` helper
  (`m4.5-storage.sh:546-560`) already resolves the respawned daemon's
  config for exactly this assertion.
- **`install.sh`/`deploy.sh`:** assert the generated `agent.toml` sets
  `stord_config_path` to the same path as the written `stord.toml`
  (the install-time link, now pointing at the live file rather than
  duplicating its contents).

## 7. Rollout & rollback

- **Rollout.** Config-gated: `stord_config_path` defaults to `None` →
  every existing deployment keeps today's behavior exactly (the
  byte-compat pin at `supervisor.rs:461-476`, base-relative, is the
  regression guard).
  Set the key in `install.sh` (`:991-1022` heredoc) and
  `docs/examples/agent.toml`; `deploy.sh` is deliberately **not** wired
  — the m4.5 respawn-parity leg pins the current generated-config
  behavior, and flipping the qual topology's respawn semantics belongs
  with an m4.5 re-qualification run (a stated residual, §9). Ship note
  in `docs/OPERATIONS.md`: operators with hand-managed `agent.toml`
  should set `stord_config_path` to their `stord.toml`. Compose files
  unchanged (`stord_binary_path = /dev/null` — no respawn; §2.4).
- **Rollback.** Unset `stord_config_path` (or revert the release):
  behavior reverts to the generated config with #377's allowlist. No
  on-disk state to migrate — the respawn config under the agent
  runtime dir is ephemeral. The only lasting effect of a C1-era
  respawn is that `stord.db`/snapshot files stay in the operator's
  `runtime_dir` (the correct location) — a rollback to the generated
  fallback re-introduces the relocation, which is the pre-existing
  recorded trap, not a new regression.
- **Documentation.** m4.5-storage.md §4.2/§5 record the relocation as
  current truth — update the "recorded, not fixed" wording for opt-in
  deployments when this lands; `docs/examples/stord.toml` and the
  `AgentConfig` doc comment (`chv-config/src/lib.rs:466-474`) gain the
  `stord_config_path` contract.

## 8. Non-goals / scope boundaries

- The install-time sync scripts' own drift (re-running `install.sh`
  regenerates both files; post-install edits are the operator's
  domain) — only the *live* link is fixed here.
- #379's control-plane backend dispatch (this issue's fix must merely
  not destroy `backend_type` on respawn).
- The compose files' posture (no respawn possible today; §2.4).
- nwd's identical defect class (§2.7) — the fix should *generalize*,
  but wiring `nwd_config_path` is a follow-up.
- stord reload/SIGHUP, a config-report RPC, and the fallback path's
  `runtime_dir` relocation (DP4) — explicitly out unless the
  maintainer opts in.

## 9. Residual risks

1. **The fallback path remains lossy.** Deployments that never set
   `stord_config_path` keep every §2.2 drop, including the relocation
   trap. Mitigation: `install.sh` sets the key; `deploy.sh` is deferred
   to an m4.5 re-qualification (§9.7). Warn-on-fallback only if Option
   A's check rides along.
2. **Readability is an install-time side effect.** The agent can read
   `stord.toml` only because the install surfaces add the agent user to
   the `chv-stord` group (`install.sh:221`; the `.deb` postinst carries
   the same grant via a parity follow-up to this issue — before it, the
   shipped `/etc/chv/stord.toml` was 0644 root:root and a `.deb`
   operator applying the documented `root:chv-stord 0640` hardening
   made the file unreadable by the agent). A deployment that tightens
   group membership silently loses the feature (falls back + warns —
   safe, but surprising). The loose-file permission guard considered
   during investigation is deliberately deferred, not implemented (the
   §6.1 decision validates parse + socket-path only; the permission
   posture is the standard install's — `root:chv-stord 0640`,
   agent group-read — and a looser `stord.toml` relies on the same
   trust already placed in systemd applying that file); the ops doc
   states the contract.
3. **Two supervisors still race.** In the install.sh layout, systemd
   (`Restart=on-failure`) and the agent can both try to revive stord
   within the same window; whoever binds the socket first wins and the
   other's child dies on bind. Pre-existing (§2.1), unchanged by this
   issue, but C1 makes the race outcome *uniform* (either path now
   uses the operator config).
4. **A warn doesn't heal.** If Option A ships alone or the fallback
   fires, locators still break post-respawn; the failure becomes
   diagnosable, not absent.
5. **Operator-file staleness is inherent.** Any file-based route reads
   the config as of respawn time — a mid-edit file (non-atomic save)
   can parse-fail and fall back. Degrade path is status-quo-safe;
   frequency is low (editors rename-atomically).
6. **A config that validates but fails at daemon startup crash-loops
   on the operator path — it never falls back.** The fallback is
   validation-scoped (parse + socket-path match) by design; a
   `stord.toml` whose `runtime_dir` parent is missing/unwritable
   (`SessionStore::new` fails opening `stord.db`, stord exits), whose
   backend constructor has bad material, or whose migration TLS files
   are checked at startup rather than parse time makes every respawn
   exit immediately: the supervisor re-validates the file (it still
   passes), re-execs the operator path under its restart throttle, and
   never degrades to the generated config. The health check is never
   wedged (the supervisor keeps observing the exit and retrying,
   throttled). This mirrors systemd `Restart=on-failure` restarting the
   same broken file; the remedy is fixing the operator config.
   Disclosed in `docs/OPERATIONS.md`; pinned by
   `supervisor_passthrough_startup_failure_retries_operator_config_under_throttle`.
7. **`deploy.sh` (qual topology) is not wired.** Only `install.sh` and
   the docs/examples carry the key; the m4.5 respawn-parity leg pins
   the current generated-config behavior, and flipping the qual
   topology's respawn semantics belongs with an m4.5 re-qualification
   run.
