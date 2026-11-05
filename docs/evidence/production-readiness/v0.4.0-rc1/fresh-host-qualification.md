# Fresh-host qualification (#666 + #556)

**Status:** `_pending_` — this template is filled from the
`Fresh-host qualification` workflow run. It is not evidence until the run
completes and every leg below is green.

**Gate:** [#666](https://github.com/o3kio/chv/issues/666) requires the
documented install → operate → upgrade → uninstall path to be demonstrated on
a fresh host for the candidate. [#556](https://github.com/o3kio/chv/issues/556)
covers the published one-line quick installer.

**Candidate:** `_pending_` (`VERSION` is still `0.3.0`; the commit below is the
candidate identity). **Commit:** `_pending_`.

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

Runner: `_pending_` (workflow run URL).
Artifact: `_pending_` (uploaded `fresh-host-qualification-<run_id>`).

## 2. Artifact digests

Recorded in `artifacts.sha256` by the harness and re-printed here from the
uploaded evidence:

```text
_pending_
```

| Artifact | SHA-256 |
|---|---|
| candidate tarball (`chv-<version>-linux-amd64.tar.gz`) | `_pending_` |
| candidate `.deb` set | `_pending_` |
| previous release tarball (`v0.3.0`) | `_pending_` |
| pinned `scripts/quick-install.sh` | `_pending_` |

The candidate binaries embed `CHV_GIT_SHA`, so the upgrade leg can distinguish
the candidate build from the previous release even though both still report
version `0.3.0`.

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
  `CHV_ENCRYPTION_KEY`, and the control plane does **not** log the
  plaintext-fallback warning.
- Admin login round-trip against `POST /api/v1/auth/login`; the local agent is
  enrolled and visible through `POST /v1/nodes`; the WebUI answers on
  `127.0.0.1:8080`; the nginx edge answers on `127.0.0.1:80`.
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
so the leg asserts that guard preserves every artifact above. It then applies
the documented package upgrade path (`dpkg --force-confold -i` + `apt-get
install -f`), restarts the services and re-asserts:

- candidate binaries report the candidate version/commit;
- the four units are `active/running` with zero restarts;
- the same config/cert/database/marker/operator/admin state is still present;
- bring-up, login, WebUI, node visibility and metrics still pass.

If `--packages` is not supplied the leg fails after the guard check rather
than pretending a tarball re-run upgraded the host.

### 3.4 `quick-install` — published one-liner at a pinned ref (#556)

Fetches `scripts/quick-install.sh` from an explicitly pinned 40-hex SHA or
release tag (branch names and `main`/`master`/`HEAD`/`latest` are refused),
hashes it, then runs it with `--version <previous-release>` in a clean root.

Asserted: the pinned script passes `bash -n`; the installer returns `rc=0`;
the same bring-up contract as §3.1 passes (units, encryption key, login, node,
WebUI, metrics); binaries report the release version that was installed.

## 4. Results

| Leg | Status | rc | Duration | Evidence |
|---|---|---|---|---|
| install-tarball | `_pending_` | `_pending_` | `_pending_` | `_pending_` |
| install-packages | `_pending_` | `_pending_` | `_pending_` | `_pending_` |
| upgrade | `_pending_` | `_pending_` | `_pending_` | `_pending_` |
| quick-install | `_pending_` | `_pending_` | `_pending_` | `_pending_` |

Summary line from `summary.md`: `_pending_`.

## 5. What this proves

- The release tarball installs end-to-end on a clean Ubuntu noble filesystem
  with the documented environment switches, and every daemon reaches
  `active/running` without restarts.
- The packaging static contract (units, conffiles, users, encryption key) is
  correct for the candidate `.deb` set.
- The candidate upgrade path preserves configuration, certificates, the
  database, a pre-existing operator account and the seeded admin account, and
  the upgraded deployment operates.
- The documented uninstall removes software while preserving state, and
  purge removes state; listeners are released.
- The published quick installer at a pinned commit installs and operates the
  release it advertises (#556).

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

## 7. Reproduce

```bash
# On the self-hosted chv-kvm runner, as root:
sudo ./scripts/integration/qual/fresh-host.sh \
  --candidate-tarball dist/chv-0.3.0-linux-amd64.tar.gz \
  --candidate-version 0.3.0 \
  --candidate-sha "$(git rev-parse HEAD)" \
  --previous-release v0.3.0 \
  --previous-version 0.3.0 \
  --packages dist/packages \
  --quick-install "$(git rev-parse HEAD)" \
  --quick-install-version 0.3.0 \
  --evidence-dir /var/lib/chv/qual/ci-artifacts/fresh-host-manual
```

Or dispatch **Fresh-host qualification** with the previous release input.
