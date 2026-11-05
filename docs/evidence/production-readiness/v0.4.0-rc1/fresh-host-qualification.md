# Fresh-host qualification (#666 + #556)

**Status:** `PASS` — filled from the published-RC run
[42042149140](https://github.com/o3kio/chv/actions/runs/42042149140)
(tag `v0.4.0-rc.1`, 2026-11-05). All four legs are green and the evidence
artifact was downloaded and re-checked.

**Gate:** [#666](https://github.com/o3kio/chv/issues/666) requires the
documented install → operate → upgrade → uninstall path to be demonstrated on
a fresh host for the candidate. [#556](https://github.com/o3kio/chv/issues/556)
covers the published one-line quick installer.

**Candidate:** `v0.4.0-rc.1` (package version `0.4.0~rc.1`; published asset
version `0.4.0.rc.1`). The source `VERSION` file holds the base `0.4.0`
because `release.yml` takes the `rc.1` prerelease suffix from the tag — the
eventual release tarball is `chv-0.4.0-rc.1-linux-amd64.tar.gz`. The lane
itself builds from the bare `VERSION`, so its local candidate tarball is
`chv-0.4.0-linux-amd64.tar.gz` and `--candidate-version` is `0.4.0`.
**Commit:** `c25360a816b5ceb85d857b0278e7f778a9080817`.

The lane's `install-tarball`, `install-packages`, and `upgrade` legs build
candidate artifacts locally from that commit; the `quick-install` leg installs
the **published** `v0.4.0-rc.1` release assets (its binaries report the release
build's short commit `c25360a`). Both report base version `0.4.0`.

---

## 1. Environment (honest label)

A clean Ubuntu noble amd64 root filesystem and systemd inside a debootstrap +
systemd-nspawn container on the self-hosted qualification runner.

- It shares the **host kernel** and the **host network namespace**.
- It has **no `/dev/kvm`**.
- It is **not a bare-metal host and not a cloud VM**.

This is a container demonstration of the installer, service, packaging, state
and uninstall contract. It is deliberately not described as a fresh machine
qualification for VM execution; the real-KVM m4.3–m4.8 lane (#659) covers VM
execution, and a separate throwaway cloud VM should be used if bare-metal
evidence is required. The host's own CHV deployment is never touched: every
leg unpacks a new root under `CHV_QUAL_ROOT` and the host ports are checked
free before boot.

Runner: `runner-2404` (labels `self-hosted, linux, x64, chv-kvm`; group
`default`).
Run: [42042149140](https://github.com/o3kio/chv/actions/runs/42042149140)
(11m31s).
Artifact: `fresh-host-qualification-42042149140`.

## 2. Artifact digests

Recorded in `artifacts.sha256` by the harness and re-printed here from the
uploaded evidence:

```text
candidate_tarball  9fc5b12ecb6d4a3a30b25a01d23fdfa91a5294256d8254e7f100af8c53c4403d  chv-0.4.0-linux-amd64.tar.gz
previous_tarball   45990c38f9ba0a1ca573e0c7549479464ff6a0eb763da6ecec4bf1eb376eea0d  chv-0.3.0-linux-amd64.tar.gz
4660ad5691f5b959372c436183dfec616e7ec72e0dee1147a4fb983eef91b0b3   chv-controlplane_0.4.0_amd64.deb
590ca7b098c64f7b93e5b0a00fb3cce379277b14c67b001cd57237476e6a8edd   chv-monitor-agent_0.4.0_amd64.deb
47c0e05ce4bb8f2b9304ad47c77e0a8ecbe438f04b55f7de9063f719f891f4f7   chv-node_0.4.0_amd64.deb
2dea45ecf516a4552e3119ed5419cc3b1abdb6c23eadc3319b52c60fe30069c8   chvctl_0.4.0_amd64.deb
quick_install_script 8d1e1a50b407eff11cfc7c19e966dec43f06e555fc0f27d73c971ac51c07fa73  quick-install-c25360a816b5ceb85d857b0278e7f778a9080817.sh
```

| Artifact | SHA-256 |
|---|---|
| candidate tarball (`chv-0.4.0-linux-amd64.tar.gz`) | `9fc5b12ecb6d4a3a30b25a01d23fdfa91a5294256d8254e7f100af8c53c4403d` |
| candidate `.deb` set | controlplane `4660ad56…b0b3`, node `47c0e05c…f4f7`, chvctl `2dea45ec…69c8`, monitor-agent `590ca7b0…8edd` |
| previous release tarball (`v0.3.0`) | `45990c38f9ba0a1ca573e0c7549479464ff6a0eb763da6ecec4bf1eb376eea0d` |
| pinned `scripts/quick-install.sh` | `8d1e1a50b407eff11cfc7c19e966dec43f06e555fc0f27d73c971ac51c07fa73` |

The candidate binaries embed `CHV_GIT_SHA`, so the upgrade leg pins the exact
candidate commit; that also distinguishes the pair in a cycle where both
carry the same base version string (in this run the previous release `v0.3.0`
carries a different base). The harness's `--candidate-version` must match the
tarball's top-level directory and the binaries' embedded `VERSION` base; the
values recorded above are from this run, not assumed.

The **published** RC assets were verified separately before this lane ran:
`sha256sum -c --strict SHA256SUMS` succeeded over all eight packages and the
tarball sidecar succeeded for `chv-0.4.0-rc.1-linux-amd64.tar.gz`
(`94747b92…db70c`); a pinned `scripts/quick-install.sh --version
0.4.0-rc.1 --dry-run` resolved and verified
`chv-controlplane_0.4.0.rc.1_amd64.deb`, `chv-node_0.4.0.rc.1_amd64.deb`
and `chvctl_0.4.0.rc.1_amd64.deb` from the release page.

## 3. Legs and assertions

The harness writes one row per leg to `results.tsv`
(`leg status rc duration_s log evidence`). A leg passes only when its
in-container transcript contains no `FAIL` line and ends with `LEG_RC=0`.

### 3.1 `install-tarball` — candidate install.sh from a release tarball

`INSTALL_CHV_TARBALL_PATH`, `INSTALL_CHV_VERSION`,
`INSTALL_CHV_TARBALL_SHA256`, `INSTALL_CHV_NO_SEED=1`,
`INSTALL_CHV_NO_BRIDGE=1`.

Asserted:

- `install.sh` returns `rc=0`; four units (`chv-controlplane`, `chv-agent`,
  `chv-stord`, `chv-nwd`) are `active/running` with `NRestarts=0`.
- `/etc/chv/encryption.env` is `0600 root:root` with a 64-hex
  `CHV_ENCRYPTION_KEY`, and the candidate control plane logs that it loaded the
  dedicated key (never the plaintext/legacy-JWT-fallback warning).
- Admin login round-trip against `POST /api/v1/auth/login`; the local agent is
  enrolled and visible through `POST /v1/nodes` (node visibility is asserted
  after the recorded test fixture clears the bootstrap admin's
  `must_change_password` flag; the BFF 403s node routes for a rotation-pending
  token); the WebUI answers on `127.0.0.1:8080`; the nginx edge answers on
  `127.0.0.1:80`.
- `/metrics` is HTTP 200 with a non-empty body on controlplane `:9103`,
  agent `:9901`, stord `:9101`, nwd `:9102`.
- Every installed binary reports the candidate version and commit.
- `quick-install.sh --uninstall` (run from the candidate checkout) stops and
  removes the units and releases the listeners while preserving
  `/etc/chv/controlplane.toml`, `/var/lib/chv/controlplane.db` and
  `/etc/chv/encryption.env`.
- `quick-install.sh --uninstall --purge` removes `/etc/chv`, `/var/lib/chv`,
  `/var/log/chv`, `/run/chv` and the binaries.
- Install.sh-layout residue that quick-install does not claim to remove
  (`/opt/chv/ui`, `/usr/local/share/chv/migrations`, the nginx site) is
  recorded as `UNINSTALL_RESIDUE=` and surfaced as a warning, never as a pass.
  This run recorded exactly those three residues and zero leg failures.

### 3.2 `install-packages` — candidate `.deb` install (only with `--packages`)

On a separate clean root: `dpkg -i` of the candidate
`chv-controlplane`/`chv-node`/`chvctl` debs followed by `apt-get install -f`,
then `dpkg --audit` must be clean.

Asserted: all three packages installed; binaries report the candidate
version/commit; packaged units exist in `/lib/systemd/system`; packaged
conffiles exist in `/etc/chv`; `chv` and `chv-stord` users exist;
`encryption.env` is minted `0600` with a valid key. The services are expected
to fail closed without generated certs, so this leg asserts the static
packaging contract, not bring-up.

### 3.3 `upgrade` — previous release then candidate, state preserved

Installs the previous released tarball (`v0.3.0` by default) with the same
container constraints, then creates identifiable state:

- hashes of `/etc/chv/encryption.env`, `/etc/chv/certs/ca.crt`,
  `/etc/chv/certs/ca.key` and `/etc/chv/controlplane.toml`;
- a marker file under `/var/lib/chv`;
- a dedicated `fresh-host-upgrade-probe` operator row in the control-plane
  database, in addition to the seeded `admin` row.

Then re-runs the candidate `install.sh` over that state. Current `install.sh`
deliberately fails closed on persistent data (`Persistent CHV data exists`),
so the leg asserts that guard preserves every artifact above. A candidate
re-run that returns `rc=0` is recorded as a failure — the guard did not fire.
This run recorded `RERUN_RC=1` with `RERUN_GUARD=persistent-data`. It then
applies the documented package upgrade path (`dpkg --force-confold -i` +
`apt-get install -f`), restarts the services and re-asserts:

- candidate binaries report the candidate version/commit;
- the four units are `active/running` with zero restarts;
- the active unit file (`FragmentPath`) is recorded for each unit. The
  documented path re-uses the previous release's `/etc/systemd/system` units,
  which shadow the packaged `/lib` copies; the evidence records that rather
  than implying the packaged units were exercised;
- the same config/cert/database/marker/operator/admin state is still present;
- bring-up, login, node visibility and metrics still pass. Because the
  upgrade preserves the previous release's `controlplane.toml`, the serving
  surface is the one that config describes, not the candidate's own: the
  WebUI is asserted on the preserved nginx edge (`http://127.0.0.1/`), since
  a pre-0.4.0 config has no `[webui]` section and the candidate does not
  serve the UI on `:8080` without one. The control-plane's dedicated
  `/metrics` listener is still asserted: the candidate binds it on
  `127.0.0.1:9103` by default when `metrics_bind` is absent (the prior
  run's journal records the warning and the listener). The evidence never
  attributes the candidate's `[webui]` shape to a config the candidate did
  not write, and never weakens a check the binary's documented default
  actually satisfies.

This run asserted all four `/metrics` listeners (controlplane, agent, stord,
nwd) HTTP 200 with non-empty bodies after the upgrade. One honest detail: the
`POST /v1/nodes` row still reported `version: "0.3.0"` at assertion time while
every installed binary reported `0.4.0`; the gate asserts the binaries, units
and bring-up, not the node row's reported version field.

If `--packages` is not supplied the leg fails after the guard check rather
than pretending a tarball re-run upgraded the host.

### 3.4 `quick-install` — published one-liner at a pinned ref (#556)

Fetches `scripts/quick-install.sh` from an explicitly pinned 40-hex SHA or
release tag (branch names and `main`/`master`/`HEAD`/`latest` are refused),
hashes it, then runs it with `--version <quick version>` in a clean root.
This run pinned the candidate commit `c25360a8…` and installed the published
`0.4.0-rc.1` release (`QUICK_INSTALL_RC=0`, 164 s).

The quick installer requires a **published release of v0.4.0 or newer**
(it fails closed on older releases: its serving shape — the control plane
serves the static WebUI and its own metrics listener — landed in 0.4.0,
and installing v0.3.0 left a 404 UI). This leg is gate evidence only from a
post-publication run with `--quick-install-version 0.4.0-rc.1` (the RC is then
resolvable from GitHub releases); the pre-publication run that pinned v0.3.0
recorded the documented refusal and was not gate evidence.

Asserted: the pinned script passes `bash -n`; the installer returns `rc=0`;
the full candidate bring-up contract from §3.1 passes (units, encryption
key source, login, node, WebUI, metrics); binaries report the base version
from `VERSION` (`0.4.0` for the `v0.4.0-rc.1` tag, because the release
workflow takes the `rc.N` suffix from the tag). The node reported
`state: TenantReady` and all four `/metrics` listeners answered 200.

## 4. Results

| Leg | Status | rc | Duration | Evidence |
|---|---|---|---|---|
| install-tarball | `pass` | `0` | 77 s | `install-tarball-results.txt` |
| install-packages | `pass` | `0` | 13 s | `install-packages-results.txt` |
| upgrade | `pass` | `0` | 93 s | `upgrade-results.txt` |
| quick-install | `pass` | `0` | 164 s | `quick-install-results.txt` |

Summary line from `summary.md`: `# Fresh-host qualification: PASS`.

## 5. What this lane proves

- The release tarball installs end-to-end on a clean Ubuntu noble
  filesystem with the documented environment switches, and every daemon
  reaches `active/running` without restarts.
- The packaging static contract (units, conffiles, users, encryption key) is
  correct for the candidate `.deb` set.
- The candidate upgrade path preserves configuration, certificates, the
  database, a pre-existing operator account and the seeded admin account,
  and the upgraded deployment operates (login, node visibility, WebUI on the
  preserved nginx edge, all four `/metrics` listeners).
- The documented uninstall removes software while preserving state, and purge
  removes state; listeners are released.
- The published quick installer at a pinned commit installs and operates the
  release it advertises (#556), with every artifact resolved from the
  published release page and checksum-verified.

## 6. What this does **not** prove

- **Not bare metal or a cloud VM.** The container shares the host kernel and
  network namespace and has no `/dev/kvm`; kernel, firmware, device, and
  scheduling behavior are the host's.
- **No VM execution.** No guest boots in this lane. Real-KVM lifecycle,
  storage, migration and fault behavior are covered by #659's m4.3–m4.8 lane.
- **No network isolation claim.** The container has no private netns; the
  legacy bridge is intentionally skipped (`INSTALL_CHV_NO_BRIDGE=1`) and dev
  seeding is skipped (`INSTALL_CHV_NO_SEED=1`).
- **Not a full upgrade matrix.** Only one previous release (`v0.3.0`) to the
  current candidate is exercised, and only the package upgrade path is a
  supported in-place upgrade for an existing persistent deployment; the
  tarball re-run is asserted to fail closed by design.
- **No signature/authenticity claim beyond the local digest.** The tarball leg
  supplies `INSTALL_CHV_TARBALL_SHA256`; release GPG verification is exercised
  separately by the release/install signature tests.
- **Node-row version lag.** In the upgraded deployment the API's node row
  still reported the previous release's version at assertion time, although
  every installed binary reported `0.4.0`. This lane does not claim the node
  row's version field is refreshed by an in-place upgrade.

## 7. Reproduce

```bash
# On the self-hosted chv-kvm runner, as root:
# <candidate-version> is the tarball top-level version the build produced
# (the VERSION base for the pre-tag gate run; the RC tag itself is
# v0.4.0-rc.1). The quick-install leg needs a *published* release >= 0.4.0,
# so this is the post-publication run with the RC pinned.
sudo ./scripts/integration/qual/fresh-host.sh \
  --candidate-tarball dist/chv-<candidate-version>-linux-amd64.tar.gz \
  --candidate-version <candidate-version> \
  --candidate-sha "$(git rev-parse HEAD)" \
  --previous-release v0.3.0 \
  --previous-version 0.3.0 \
  --packages dist/packages \
  --quick-install "$(git rev-parse HEAD)" \
  --quick-install-version 0.4.0-rc.1 \
  --evidence-dir /var/lib/chv/qual/ci-artifacts/fresh-host-manual
```

Or dispatch **Fresh-host qualification** with the previous release input and
`quick_install_version=0.4.0-rc.1` after the RC is published — the run
recorded above used:

```bash
gh workflow run fresh-host-qualification.yml --ref v0.4.0-rc.1 \
  -f previous_release=v0.3.0 -f skip_upgrade=false \
  -f quick_install_version=0.4.0-rc.1
```
