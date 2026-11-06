#!/usr/bin/env bash
# qual-harness-invariants.sh — offline guards for the self-hosted KVM
# qualification harness (#659).
#
# The nightly lane is the only real-KVM gate, so a harness regression used to
# surface only as a red schedule hours later. These are the two concrete
# regressions that made every scenario fail after #687/#693:
#
#   1. deploy.sh gave the control plane a `jwt_secret` but not the agent, so
#      the agent minted its own and every CP→agent RPC failed closed with
#      "invalid agent authentication token".
#   2. the two-stord scenarios omitted `metrics_bind`; after #687 each stord
#      binds 127.0.0.1:9101 by default, so the extra stords collided with the
#      deployed stord (and with each other) and aborted at startup.
#   3. the scenarios predate #663: a migration receiver now refuses to start
#      without `node_id` + a signing secret, and refuses every stream without
#      a control-plane authorization token. The harness mints those tokens
#      itself (it drives stord directly, bypassing the CP minter).
#
# Plus the runner-hygiene/observability hooks that keep a failed lane from
# poisoning the next one. This script is deliberately offline (no root, KVM,
# or network): CI runs it, and it fails loudly if one of the guards is
# deleted.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
QUAL="${REPO_ROOT}/scripts/integration/qual"
rc=0

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    rc=1
}

pass() {
    printf 'ok: %s\n' "$*"
}

# ---------------------------------------------------------------------------
# 1. deploy.sh: one agent-auth master, written to BOTH configs
# ---------------------------------------------------------------------------
deploy="${QUAL}/deploy.sh"
grep -q '^JWT_SECRET="qual-' "$deploy" \
    && pass "deploy.sh generates one shared JWT_SECRET" \
    || fail "deploy.sh no longer defines a single shared JWT_SECRET"

shared_lines="$(grep -c '^jwt_secret = "${JWT_SECRET}"$' "$deploy" || true)"
if [ "$shared_lines" -ge 2 ]; then
    pass "controlplane.toml and agent.toml both use the shared JWT_SECRET"
else
    fail "expected jwt_secret = \"\${JWT_SECRET}\" in both deploy configs, found ${shared_lines}"
fi

if grep -q 'jwt_secret = "qual-\$(openssl rand' "$deploy"; then
    fail "deploy.sh still mints a per-config jwt_secret (breaks the shared agent-auth derivation)"
else
    pass "no per-config jwt_secret generation remains"
fi

# The runtime guard must stay next to config generation.
grep -q 'agent-auth master mismatch between controlplane.toml and agent.toml' "$deploy" \
    && pass "deploy.sh keeps the runtime agent-auth equality guard" \
    || fail "deploy.sh lost the runtime agent-auth equality guard"

# ---------------------------------------------------------------------------
# 2. deployed daemons: explicit metrics ports
# ---------------------------------------------------------------------------
grep -q '^metrics_bind = "127.0.0.1:9101"$' "$deploy" \
    && pass "deployed stord metrics_bind is explicit (9101)" \
    || fail "deployed stord.toml lacks metrics_bind = 127.0.0.1:9101"
grep -q '^metrics_bind = "127.0.0.1:9102"$' "$deploy" \
    && pass "deployed nwd metrics_bind is explicit (9102)" \
    || fail "deployed nwd.toml lacks metrics_bind = 127.0.0.1:9102"

# ---------------------------------------------------------------------------
# 3. scenario stords: never inherit the shared default port
# ---------------------------------------------------------------------------
check_scenario_metrics() {
    local file="$1" minimum="$2" found
    found="$(grep -c '^metrics_bind = "127.0.0.1:0"$' "$file" || true)"
    if [ "$found" -ge "$minimum" ]; then
        pass "$(basename "$file") sets an ephemeral metrics_bind on ${found} stord config(s)"
    else
        fail "$(basename "$file") sets metrics_bind on ${found} config(s), expected >= ${minimum}"
    fi
}
# m4.6 writes SRC + DST + two N1b startup-failure configs.
check_scenario_metrics "${QUAL}/m4.6-migration.sh" 4
# m4.7/m4.8 write SRC + DST.
check_scenario_metrics "${QUAL}/m4.7-faults.sh" 2
check_scenario_metrics "${QUAL}/m4.8-perf-soak.sh" 2

# ---------------------------------------------------------------------------
# 3b. migration authorization (issue #663)
# ---------------------------------------------------------------------------
# The CP mints tokens from a dedicated secret; every receiver verifies with
# the same file. A drift here fails closed at dispatch/stream time, which
# used to surface only as a red nightly run.
grep -q '^MIGRATION_SIGNING_SECRET_FILE=' "$deploy" \
    && pass "deploy.sh generates a dedicated migration-signing secret" \
    || fail "deploy.sh no longer generates a migration-signing secret"
grep -q '^signing_secret_file = "${MIGRATION_SIGNING_SECRET_FILE}"$' "$deploy" \
    && pass "controlplane.toml points at the migration-signing secret" \
    || fail "controlplane.toml lost [migration].signing_secret_file"
grep -q 'QUAL_MIGRATION_SECRET_FILE="$MIGRATION_SIGNING_SECRET_FILE"' "$deploy" \
    && pass "deploy.sh hands the signing secret to scenarios" \
    || fail "deploy.sh no longer exports QUAL_MIGRATION_SECRET_FILE"
grep -q '^mint_migration_authorization()' "${QUAL}/lib.sh" \
    && pass "lib.sh provides the migration-authorization minter" \
    || fail "lib.sh lost mint_migration_authorization"

for scenario in m4.6-migration.sh m4.7-faults.sh m4.8-perf-soak.sh; do
    file="${QUAL}/${scenario}"
    grep -q 'QUAL_MIGRATION_SECRET_FILE not set' "$file" \
        && pass "${scenario} refuses to run without the migration-signing secret" \
        || fail "${scenario} no longer requires QUAL_MIGRATION_SECRET_FILE"
    grep -q 'node_id = "${QUAL_MIGRATION_DEST_NODE_ID}"' "$file" \
        && pass "${scenario} binds the receiver node id" \
        || fail "${scenario} DST config lost migration.node_id"
    grep -q 'signing_secret_file = "${QUAL_MIGRATION_SECRET_FILE}"' "$file" \
        && pass "${scenario} gives the receiver the signing secret" \
        || fail "${scenario} DST config lost migration.signing_secret_file"
    grep -q 'migrationAuthorization' "$file" \
        && pass "${scenario} attaches a fresh authorization to every trigger" \
        || fail "${scenario} no longer attaches migrationAuthorization"
done

if grep -rn 'allow_insecure_unauthenticated *= *true' "$QUAL" >/dev/null 2>&1; then
    fail "a qualification scenario enables the insecure migration bypass"
else
    pass "no qualification scenario enables the insecure migration bypass"
fi

# ---------------------------------------------------------------------------
# 3c. F7 stop/delete contract and the documented session residue
# ---------------------------------------------------------------------------
m47="${QUAL}/m4.7-faults.sh"
grep -q 'wait_vm_guest_stopped "\$F2_VM"' "$m47" \
    && pass "m4.7 waits for a terminal guest state after a graceful stop" \
    || fail "m4.7 no longer uses wait_vm_guest_stopped for the F7 stop"
grep -q 'vm_id IS NOT NULL AND vm_id !=' "$m47" \
    && pass "m4.7 fails the final sweep on any still-attached stord session" \
    || fail "m4.7 lost the attached-session final-sweep gate"
grep -q 'unexpected on the #711-fixed candidate' "$m47" \
    && pass "m4.7 records leftover detached sessions as an unexpected residual" \
    || fail "m4.7 no longer discloses the post-#711 detached-session expectation"

# ---------------------------------------------------------------------------
# 4. teardown and lane hygiene
# ---------------------------------------------------------------------------
for bin in chv-agent chv-stord chv-controlplane; do
    grep -q "\"${bin}|" "$deploy" \
        && pass "deploy teardown pattern-kills stale ${bin}" \
        || fail "deploy teardown lost the ${bin} pattern-kill"
done

nightly="${QUAL}/nightly-run.sh"
grep -q '^sweep_stale_qual_state$' "$nightly" \
    && pass "nightly lane sweeps stale qualification processes before preflight" \
    || fail "nightly lane no longer sweeps stale qualification state"
grep -q '^require_qual_ports_free$' "$nightly" \
    && pass "nightly lane fails loudly if qualification ports are still held" \
    || fail "nightly lane no longer asserts qualification ports are free"
grep -q 'preserve_failed_dirs "\$failed_before"' "$nightly" \
    && pass "nightly lane copies preserved failed trees into the uploaded artifact" \
    || fail "nightly lane no longer preserves failed test trees into the artifact"
grep -q 'snapshot_host_state "\${LOG_DIR}' "$nightly" \
    && pass "nightly lane captures host state on scenario failure" \
    || fail "nightly lane no longer captures host state on failure"

# A canceled workflow leaves the job's children alive; an orphaned
# nightly-run/deploy keeps starting new stacks after the daemon sweep.
grep -q '^sweep_orphaned_harness()' "$nightly" \
    && pass "nightly lane kills orphaned harness scripts, not just daemons" \
    || fail "nightly lane no longer kills orphaned harness scripts"
grep -q '^    sweep_orphaned_harness$' "$nightly" \
    && pass "the orphaned-harness sweep runs inside the stale-state sweep" \
    || fail "orphaned-harness sweep is not invoked"
grep -q 'nightly-run.lock' "$nightly" \
    && pass "nightly lane holds a single-lane lock" \
    || fail "nightly lane lost its single-lane lock"
grep -q 'for attempt in \$(seq 1 15)' "$nightly" \
    && pass "port assertion retries while a killed daemon releases listeners" \
    || fail "port assertion no longer tolerates a dying daemon"
grep -q 'deploy.sh" --exec "./\${scenario}.sh" 9>&-' "$nightly" \
    && pass "scenario children do not inherit the lane lock" \
    || fail "scenario children may inherit and leak the lane lock"

# ---------------------------------------------------------------------------
# 4. graceful stop vs delete contract (--no-shutdown)
# ---------------------------------------------------------------------------
# A graceful stop keeps the VMM alive until DeleteVm; only the guest
# reaches a terminal state. Scenarios that stop and then restart a VM must
# wait for that terminal state. run 41912418366 failed m4.4/m4.5/m4.8
# because they still waited for CH exit after `vm stop` (240 s each).
grep -q '^wait_vm_guest_stopped()' "${QUAL}/lib.sh" \
    && pass "lib.sh defines the shared terminal-guest stop wait" \
    || fail "lib.sh lost wait_vm_guest_stopped (graceful-stop contract)"
for f in m4.4-network m4.5-storage m4.8-perf-soak; do
    grep -q 'wait_vm_guest_stopped "' "${QUAL}/${f}.sh" \
        && pass "${f} waits for terminal guest state after stop" \
        || fail "${f} does not use the terminal-guest stop wait"
done
grep -q 'wait_no_ch_process "Leg F CH process reaped after delete"' "${QUAL}/m4.3-lifecycle.sh" \
    && pass "m4.3 waits for the VMM to be reaped after delete" \
    || fail "m4.3 delete absence assertion races the async VMM reaping"
grep -q 'no migration client identity' "${QUAL}/m4.6-migration.sh" \
    && pass "m4.6 accepts the synchronous no-client-identity refusal" \
    || fail "m4.6 only accepts the older async migration-refusal shape"

# Targeted re-qualification without the soak (manual dispatch).
grep -q 'QUAL_SCENARIOS' .github/workflows/nightly-kvm-qualification.yml \
    && pass "workflow exposes a manual scenario-subset input" \
    || fail "workflow lost the manual scenario-subset input"

# A prior root build in the workspace blocks actions/checkout (run 41932222532
# failed at Checkout on a root-owned target/). Both lanes must restore the
# workspace ownership before checkout.
[ "$(grep -c 'name: Restore workspace ownership' .github/workflows/nightly-kvm-qualification.yml)" -eq 2 ] \
    && pass "both lanes restore workspace ownership before checkout" \
    || fail "a lane can fail checkout on root-owned residue"

# ---------------------------------------------------------------------------
# 5. second-round full-lane lessons (run 41926076717)
# ---------------------------------------------------------------------------
# m4.6's negative TLS legs now fail synchronously at TriggerDiskMigration
# (the sender dials inside the RPC); the harness must accept that shape as
# well as the older accepted-then-FAILED task.
grep -q 'synchronous trigger refusal is a transport-level mTLS failure' "${QUAL}/m4.6-migration.sh" \
    && pass "m4.6 accepts the synchronous TLS-refusal shape" \
    || fail "m4.6 only accepts the async migration-refusal shape"
grep -q 'synchronous refusal' "${QUAL}/m4.6-migration.sh" \
    && pass "m4.6 records the synchronous N8 refusal in the evidence" \
    || fail "m4.6 drops the synchronous N8 refusal evidence"
# tracing quotes string fields: backend_type="lvm", not backend_type=lvm.
grep -q 'backend_type\.\*"lvm"' "${QUAL}/m4.5-storage.sh" \
    && pass "m4.5 matches the quoted backend_type log field" \
    || fail "m4.5 greps a log form tracing never emits"
# A graceful stop of an adopted VMM can fall back to force-kill (#345).
grep -q 'force-kill fallback' "${QUAL}/m4.3-lifecycle.sh" \
    && pass "m4.3 records the adopted-VMM force-kill fallback as a warning" \
    || fail "m4.3 treats the known #345 force-kill fallback as a hard failure"
# m4.4 tap residue must be scoped to this deployment's taps, not the host.
grep -q 'TAPS_UNDER_TEST' "${QUAL}/m4.4-network.sh" \
    && pass "m4.4 scopes tap residue to the scenario's own taps" \
    || fail "m4.4 counts every host tap (stale lanes become false failures)"
grep -q 'orphaned bridge from a previous lane' "${QUAL}/m4.4-network.sh" \
    && pass "m4.4 diagnoses a stale bridge holding the network route" \
    || fail "m4.4 cannot distinguish a stale route from a policy defect"
# Lane hygiene: orphaned links/tables from a killed run, and per-run evidence.
grep -q 'orphaned CHV host links' "${QUAL}/nightly-run.sh" \
    && pass "nightly lane sweeps orphaned CHV host links" \
    || fail "nightly lane leaves stale bridges/taps from a killed run"
grep -q 'orphaned CHV nft tables' "${QUAL}/nightly-run.sh" \
    && pass "nightly lane sweeps orphaned CHV nft tables" \
    || fail "nightly lane leaves stale nft tables from a killed run"
grep -q 'rm -rf -- "\$evidence_src"' "${QUAL}/nightly-run.sh" \
    && pass "nightly lane clears per-run scenario evidence" \
    || fail "nightly lane mixes evidence from previous runs"

# ---------------------------------------------------------------------------
# 6. fresh-host lane (#666 install/upgrade/uninstall + #556 quick-install)
# ---------------------------------------------------------------------------
# The gate is demonstrated by one driver, one workflow, and one evidence
# template. These guards keep the lane from silently losing a leg, floating
# the quick-install ref, or overstating the container as bare metal.
fresh="${QUAL}/fresh-host.sh"
fresh_workflow="${REPO_ROOT}/.github/workflows/fresh-host-qualification.yml"
fresh_report="${REPO_ROOT}/docs/evidence/production-readiness/v0.4.0-rc1/fresh-host-qualification.md"

if [ -x "$fresh" ]; then
    pass "fresh-host driver exists and is executable"
else
    fail "fresh-host driver is missing or not executable: ${fresh}"
fi

grep -q 'INSTALL_CHV_TARBALL_PATH=' "$fresh" \
    && pass "fresh-host install leg uses a candidate tarball path" \
    || fail "fresh-host install leg no longer drives INSTALL_CHV_TARBALL_PATH"
for guard in INSTALL_CHV_NO_SEED=1 INSTALL_CHV_NO_BRIDGE=1; do
    grep -q "$guard" "$fresh" \
        && pass "fresh-host sets ${guard} (container constraints)" \
        || fail "fresh-host lost ${guard}"
done

grep -q 'Persistent CHV data exists' "$fresh" \
    && pass "fresh-host upgrade leg asserts the persistent-data fail-closed guard" \
    || fail "fresh-host may silently overwrite persistent state on upgrade"
grep -q 'PACKAGE_UPGRADE_RC' "$fresh" \
    && pass "fresh-host upgrade leg exercises the package upgrade path" \
    || fail "fresh-host has no supported in-place upgrade path with --packages"

grep -q 'main|master|HEAD|latest' "$fresh" \
    && pass "fresh-host refuses floating quick-install refs" \
    || fail "fresh-host no longer rejects a floating quick-install ref"
grep -Fq 'QUICK_REF="${QUICK_REF:-}"' "$fresh" \
    && pass "fresh-host has no default quick-install ref" \
    || fail "fresh-host could default to a floating quick-install ref"
grep -q 'raw.githubusercontent.com/.*scripts/quick-install.sh' "$fresh" \
    && pass "fresh-host fetches quick-install.sh at the pinned ref" \
    || fail "fresh-host does not fetch the pinned quick-install.sh"
grep -q 'check_bringup candidate http://127.0.0.1/' "$fresh" \
    && pass "fresh-host asserts the preserved nginx edge after a config-preserving upgrade" \
    || fail "fresh-host would claim the candidate WebUI listener on a preserved v0.3.0 config"
if grep -q 'METRICS controlplane=not-configured' "$fresh"; then
    fail "fresh-host weakens the upgrade metrics check despite the binary's loopback default"
else
    pass "fresh-host keeps the documented default control-plane metrics listener asserted"
fi
grep -Fq 'expected_bin_version="${expected_bin_version%-rc.*}"' "$fresh" \
    && pass "fresh-host compares RC binaries against their base VERSION" \
    || fail "fresh-host can fail an RC quick leg because binaries report the base version"
grep -q 'quick_install_version' "$fresh_workflow" \
    && pass "fresh-host workflow can pin the quick leg to the published RC" \
    || fail "fresh-host workflow cannot exercise the quick installer against the RC"
grep -q 'QUICK_INSTALL_VERSION:-' "$fresh_workflow" \
    && pass "fresh-host workflow defaults the quick leg to the previous release" \
    || fail "fresh-host workflow has no quick-install version fallback"

quick_installer="${REPO_ROOT}/scripts/quick-install.sh"
grep -q 'QUICK_INSTALL_MIN_VERSION="0.4.0"' "$quick_installer" \
    && pass "quick installer declares its minimum supported release" \
    || fail "quick installer can install a release without the WebUI serving shape"
grep -q 'require_supported_release' "$quick_installer" \
    && pass "quick installer refuses releases older than its serving shape" \
    || fail "quick installer silently installs a pre-0.4.0 host with no WebUI"
grep -q '^ensure_runtime_contract()' "$quick_installer" \
    && pass "quick installer applies the cross-leg runtime contract" \
    || fail "quick installer can leave stord/nwd unable to start on the deb leg"
grep -q 'usermod -aG chv-stord "\$CHV_USER"' "$quick_installer" \
    && pass "quick installer grants chv the chv-stord group on both legs" \
    || fail "quick installer writes a stord.toml the service user cannot read"
grep -q 'systemd-tmpfiles --create "\$tmpfiles"' "$quick_installer" \
    && pass "quick installer applies the tmpfiles entry that creates /run/netns" \
    || fail "quick installer relies on a reboot before chv-nwd can start"
grep -q '^chv_sqlite_offline()' "$quick_installer" \
    && pass "quick installer has an offline SQLite write helper" \
    || fail "quick installer can unlink the WAL of a live control plane (#722)"
grep -q 'chv_sqlite_offline "${CHV_DB_PATH}" <<SQL' "$quick_installer" \
    && pass "quick installer seeds the admin with the control plane stopped" \
    || fail "quick installer seeds the admin against the live WAL"
grep -qF '${copy}/controlplane.db' "$quick_installer" \
    && pass "quick installer checks enrollment against a database snapshot" \
    || fail "quick installer polls the live WAL for enrollment"

# Exercise the real version gate, not just its presence: source the installer
# (its B1 guard makes that side-effect free) and assert the boundary plus a
# clean stderr. A malformed comparison (e.g. comparing a minor against a
# full "4.0" string) still "refuses" but prints an integer-expression error,
# which this catches.
quick_version_matrix="$(source_path="$quick_installer" bash -c '
    set +u
    # shellcheck disable=SC1090
    source "$source_path" >/dev/null 2>&1
    for v in 0.3.0 0.3.9 0.4.0 0.4.0-rc.1 0.4.0-rc.9 0.10.0 1.0.0; do
        if release_supported "$v"; then
            printf "%s supported\n" "$v"
        else
            printf "%s refused\n" "$v"
        fi
    done
' 2>&1)"
if printf '%s\n' "$quick_version_matrix" | grep -q 'integer expression expected'; then
    fail "quick installer version gate compares a malformed minor version"
else
    pass "quick installer version gate runs without integer-expression errors"
fi
for expect in '0.3.0 refused' '0.3.9 refused' '0.4.0 supported' \
    '0.4.0-rc.1 supported' '0.4.0-rc.9 supported' '0.10.0 supported' \
    '1.0.0 supported'; do
    printf '%s\n' "$quick_version_matrix" | grep -qxF "$expect" \
        && pass "quick installer version gate: ${expect}" \
        || fail "quick installer version gate: expected '${expect}'"
done

postinst="${REPO_ROOT}/packaging/scripts/postinstall.sh"
grep -q '^sync_legacy_migrations()' "$postinst" \
    && pass "postinstall synchronizes migrations into a preserved install.sh tree" \
    || fail "package upgrade over an install.sh deployment can keep a stale schema"
grep -q 'systemd-tmpfiles --create /usr/lib/tmpfiles.d/chv-node.conf' "$postinst" \
    && pass "postinstall applies the tmpfiles entry on a live host" \
    || fail "package install can leave /run/netns missing until reboot"

grep -q 'results.tsv' "$fresh" \
    && pass "fresh-host writes the required results.tsv marker" \
    || fail "fresh-host no longer writes results.tsv"
grep -q 'UNINSTALL_RESIDUE=' "$fresh" \
    && pass "fresh-host records uninstall residue instead of passing it" \
    || fail "fresh-host hides install.sh-layout uninstall residue"
grep -q 'POST http://127.0.0.1:8080/v1/auth/change-password' "$fresh" \
    && pass "fresh-host rotates the bootstrap password through the API" \
    || fail "fresh-host clears PASSWORD_CHANGE_REQUIRED with a live database write"
grep -q 'rotate_admin_password || true' "$fresh" \
    && pass "fresh-host calls the API rotation before login/node checks" \
    || fail "fresh-host can leave the rotation flag set and fail the node check"
grep -q 'check_credential_persistence' "$fresh" \
    && pass "fresh-host asserts clean-install writes survive a restart" \
    || fail "fresh-host would miss the unlinked-WAL data-loss regression"
grep -q 'controlplane.db-wal (deleted)' "$fresh" \
    && pass "fresh-host checks for an unlinked database WAL" \
    || fail "fresh-host would not diagnose an unlinked database WAL"
grep -q 'deprecated legacy CHV_JWT_SECRET fallback' "$fresh" \
    && pass "fresh-host detects the credential-key fallback in both releases" \
    || fail "fresh-host encryption check can pass on the legacy fallback"
grep -q 'failed to enable the one-shot service' "$fresh" \
    && pass "fresh-host records a row when the one-shot service cannot be enabled" \
    || fail "fresh-host can drop a leg without a results row"
grep -q 'state_lines" -eq 4' "$fresh" \
    && pass "fresh-host asserts all four upgrade state files were hashed" \
    || fail "fresh-host upgrade state hash set can shrink silently"
grep -q 'cached base extract failed' "$fresh" \
    && pass "fresh-host records a failed cached-base extract instead of exiting" \
    || fail "fresh-host mid-run base failure can lose leg accounting"
grep -q 'documented fail-closed guard did not fire' "$fresh" \
    && pass "fresh-host fails the upgrade leg when the candidate re-runs over persistent data" \
    || fail "fresh-host can pass the upgrade leg without exercising the fail-closed guard"
grep -q 'runuser -u chv -- sqlite3' "$fresh" \
    && fail "fresh-host writes to the live database from a second process" \
    || pass "fresh-host never writes to the live database from a second process"
grep -Fq '$copy/controlplane.db' "$fresh" \
    && pass "fresh-host diagnoses a database snapshot, not the live file" \
    || fail "fresh-host may inspect the live WAL database"
grep -qF 'journalctl -u "$u" -n 80 --no-pager' "$fresh" \
    && pass "fresh-host records a failing unit's journal tail in the evidence" \
    || fail "fresh-host failures are not diagnosable from the uploaded evidence"
grep -q 'export GITHUB_REPO=' "$fresh" \
    && pass "fresh-host exports GITHUB_REPO to the in-container child" \
    || fail "fresh-host in-container quick installer falls back to o3kio/chv"
grep -q 'command -v ss' "$fresh" \
    && pass "fresh-host requires ss for the shared-netns port preflight" \
    || fail "fresh-host port preflight can pass vacuously without ss"
grep -q 'partial —' "$fresh" \
    && pass "fresh-host summary marks a run with skipped legs as partial" \
    || fail "fresh-host summary can headline PASS with skipped legs"

[ -f "$fresh_workflow" ] \
    && pass "fresh-host workflow exists" \
    || fail "fresh-host workflow is missing"
grep -q 'workflow_dispatch' "$fresh_workflow" \
    && pass "fresh-host workflow is manually dispatched" \
    || fail "fresh-host workflow is not manually dispatched"
grep -Fq '[self-hosted, linux, x64, chv-kvm]' "$fresh_workflow" \
    && pass "fresh-host workflow targets the self-hosted chv-kvm fleet" \
    || fail "fresh-host workflow lost the self-hosted runner labels"
grep -q 'apt_run install gettext-base debootstrap systemd-container' "$fresh_workflow" \
    && pass "fresh-host workflow installs the container driver prerequisites" \
    || fail "fresh-host workflow cannot stage debootstrap/systemd-nspawn"
grep -q 'NEEDRESTART_SUSPEND=1' "$fresh_workflow" \
    && pass "fresh-host workflow keeps apt and needrestart non-interactive" \
    || fail "fresh-host workflow can block on an interactive apt/kernel prompt"
grep -q 'releases/download' "$fresh_workflow" \
    && pass "fresh-host workflow downloads the previous release" \
    || fail "fresh-host workflow does not stage the previous release"
grep -q 'sha256sum -c' "$fresh_workflow" \
    && pass "fresh-host workflow verifies the previous release checksum" \
    || fail "fresh-host workflow consumes the previous release without verifying its checksum"
grep -q 'sudo cp .*candidate-digests' "$fresh_workflow" \
    && pass "fresh-host workflow stages the digest file as root" \
    || fail "fresh-host workflow cannot write the digest into the root-owned evidence dir"
grep -q 'results.tsv' "$fresh_workflow" \
    && pass "fresh-host workflow requires the results marker" \
    || fail "fresh-host workflow does not require results.tsv"
grep -q 'if-no-files-found: error' "$fresh_workflow" \
    && pass "fresh-host workflow fails when results.tsv is missing" \
    || fail "fresh-host workflow tolerates a missing results marker"

[ -f "$fresh_report" ] \
    && pass "fresh-host evidence template exists" \
    || fail "fresh-host evidence template is missing"
grep -q 'shares the \*\*host kernel\*\*' "$fresh_report" \
    && pass "fresh-host report labels the shared host kernel honestly" \
    || fail "fresh-host report does not disclose the shared host kernel"
grep -q 'no `/dev/kvm`' "$fresh_report" \
    && pass "fresh-host report discloses the missing /dev/kvm" \
    || fail "fresh-host report hides the missing /dev/kvm"
grep -q 'not a bare-metal host and not a cloud VM' "$fresh_report" \
    && pass "fresh-host report refuses the bare-metal/cloud-VM claim" \
    || fail "fresh-host report overstates the container as a real host"
# The report is evidence only after the post-publication run; it must be
# filled from a green artifact, not left as a template.
if grep -q '_pending_' "$fresh_report"; then
    fail "fresh-host evidence report is still a pending template"
else
    pass "fresh-host evidence report is filled from the qualification run"
fi
grep -q 'Fresh-host qualification: PASS' "$fresh_report" \
    && pass "fresh-host evidence report records a passing run" \
    || fail "fresh-host evidence report does not record a passing run"
grep -q 'actions/runs/[0-9]\+' "$fresh_report" \
    && pass "fresh-host evidence report links the qualification run" \
    || fail "fresh-host evidence report has no run link"

# The in-container marker and the host-side assertion are one contract; a
# wording change on either side silently makes the leg unable to pass.
install_sh_leg="${QUAL}/install-sh-leg.sh"
encmarker='ENCKEY-LOADED (no plaintext/fallback/no-key warning in CP journal)'
marker_count="$(grep -cF "$encmarker" "$install_sh_leg" 2>/dev/null || true)"
[ "${marker_count:-0}" -ge 2 ] \
    && pass "install-sh leg asserts the exact encryption success marker it emits" \
    || fail "install-sh leg's encryption marker and host assertion have drifted"
grep -q 'CHV_ENCRYPTION_KEY is unset or empty' "$install_sh_leg" \
    && pass "install-sh leg treats a no-key control plane as a failure" \
    || fail "install-sh leg can pass while the control plane loaded no encryption key"
grep -q 'QUICK_VERSION" = "$CANDIDATE_VERSION' "$fresh" \
    && pass "fresh-host quick leg uses candidate-mode checks when installing the candidate" \
    || fail "fresh-host quick leg weakens candidate checks unconditionally"
grep -q -- '--quick-install-candidate' "$fresh" \
    && pass "fresh-host can force candidate-mode checks for a released RC" \
    || fail "fresh-host treats the candidate RC as an older release"
grep -q 'QUICK_INSTALL_CANDIDATE:-0' "$fresh" \
    && pass "fresh-host gates the persistence check on the candidate-release flag" \
    || fail "fresh-host drops the candidate-release persistence check"
grep -q 'quick_is_candidate=1' "$fresh_workflow" \
    && pass "fresh-host workflow marks the published RC quick leg as the candidate" \
    || fail "fresh-host workflow weakens the published RC quick leg"

# ---------------------------------------------------------------------------
# Release packaging: every nfpm package's binary must reach the Package job.
# ---------------------------------------------------------------------------
# The Package job runs `build-packages.sh --skip-build` on the
# `release-binaries` artifact, so a binary that nfpm packages but the Build
# job never uploads fails packaging outright (Release run 42018658104:
# chv-monitor-agent). Likewise upload-artifact drops mode bits, so the
# restore-permissions step must name that binary or it ships non-executable.
release_workflow="${REPO_ROOT}/.github/workflows/release.yml"
upload_block="$(awk '
    /name: release-binaries/ { capture = 1 }
    capture && /^      - name:/ && !/name: release-binaries/ { exit }
    capture { print }
' "$release_workflow")"
chmod_block="$(awk '
    /Restore binary executable permissions/ { capture = 1 }
    capture && /^      - name:/ && !/Restore binary executable permissions/ { exit }
    capture { print }
' "$release_workflow")"
[ -n "$upload_block" ] \
    && pass "release workflow declares the release-binaries upload list" \
    || fail "release workflow no longer uploads a release-binaries artifact"
[ -n "$chmod_block" ] \
    && pass "release workflow restores packaged binary mode bits" \
    || fail "release workflow no longer restores binary executable permissions"
for pkg_yaml in "${REPO_ROOT}"/packaging/nfpm/*.yaml; do
    [ -e "$pkg_yaml" ] || continue
    while IFS= read -r bin; do
        [ -n "$bin" ] || continue
        printf '%s\n' "$upload_block" | grep -q "target/release/${bin}\$" \
            && pass "release-binaries uploads packaged binary ${bin}" \
            || fail "release-binaries omits packaged binary ${bin} (packaging will fail)"
        printf '%s\n' "$chmod_block" | grep -q "target/release/${bin}" \
            && pass "release workflow restores mode bits on ${bin}" \
            || fail "release workflow leaves ${bin} non-executable in packages"
    done < <(sed -n 's|^[[:space:]]*-[[:space:]]*src:[[:space:]]*target/release/\([^[:space:]]*\)$|\1|p' "$pkg_yaml")
done

# ---------------------------------------------------------------------------
# Release assets: GitHub sanitizes filenames, so package files must be
# emitted with the same name the release page will serve. GitHub replaces
# characters outside [A-Za-z0-9._-] with '.', which turned a published
# `~rc.1` package into `.rc.1` while SHA256SUMS and the quick installer
# still named the `~` form (run 42023294265).
# ---------------------------------------------------------------------------
build_packages="${REPO_ROOT}/scripts/build-packages.sh"
check_package_files="${REPO_ROOT}/scripts/package/check-package-files.sh"
grep -q 'safe_asset_name' "$build_packages" \
    && pass "build-packages.sh derives a GitHub-safe asset filename" \
    || fail "build-packages.sh emits filenames GitHub will rewrite"
grep -q "tr -c 'A-Za-z0-9._-'" "$build_packages" \
    && pass "build-packages.sh uses GitHub's character-replacement rule" \
    || fail "build-packages.sh sanitization does not match GitHub's rule"
grep -q 'ASSET_VERSION' "$check_package_files" \
    && pass "check-package-files.sh validates the published asset name" \
    || fail "check-package-files.sh validates a filename the release never serves"
smoke_webui="${REPO_ROOT}/scripts/package/smoke-webui.sh"
grep -q 'asset_version' "$smoke_webui" \
    && pass "smoke-webui.sh selects the published asset name" \
    || fail "smoke-webui.sh selects a '~' filename the build no longer emits"
grep -q "tr -c 'A-Za-z0-9._-'" "$smoke_webui" \
    && pass "smoke-webui.sh applies GitHub's filename sanitization" \
    || fail "smoke-webui.sh filename sanitization drifted from GitHub's rule"

# The lifecycle upgrade test installs a synthetic 0.0.1 build. The workflows
# copy it to dist/packages-old and then rebuild the real version into the same
# dist/packages directory; without a clean in between, the 0.0.1 probe
# packages were published as release assets and listed in SHA256SUMS
# (observed on v0.3.0 and v0.4.0-rc.1).
for wf in "$release_workflow" "${REPO_ROOT}/.github/workflows/package-nightly.yml"; do
    wf_name="$(basename "$wf")"
    ordered_clean="$(awk '
        /cp dist\/packages\/\* dist\/packages-old\// { copied = NR }
        /rm -rf dist\/packages[[:space:]]*$/ && !/packages-old/ { cleaned = NR }
        END { if (copied && cleaned && cleaned > copied) print "ok" }
    ' "$wf")"
    [ "$ordered_clean" = "ok" ] \
        && pass "${wf_name} removes 0.0.1 probe packages before the real build" \
        || fail "${wf_name} would publish the 0.0.1 lifecycle probe packages"
    grep -q 'Refusing to publish package' "$wf" \
        && pass "${wf_name} fails closed on a foreign staged package version" \
        || fail "${wf_name} has no runtime guard against publishing a foreign version"
    # version.sh prints a trailing newline; piping it straight into tr -c
    # turns that newline into a trailing '.', which makes every package look
    # foreign. Strip first via command substitution, then sanitize.
    grep -q 'RAW_VERSION="$(./scripts/version.sh --deb)"' "$wf" \
        && pass "${wf_name} strips the version newline before sanitizing" \
        || fail "${wf_name} sanitizes a newline-terminated version"
    if grep -q 'version.sh --deb | tr -c' "$wf"; then
        fail "${wf_name} pipes a newline into the asset-version sanitizer"
    else
        pass "${wf_name} does not pipe a newline into the asset-version sanitizer"
    fi
    grep -Fq 'case "$base" in' "$wf" \
        && grep -Fq '*-"${ASSET_VERSION}"-*.rpm' "$wf" \
        && pass "${wf_name} matches the staged version with delimiters" \
        || fail "${wf_name} accepts a package version by prefix (rc.1 vs rc.10)"
done

nightly_workflow="${REPO_ROOT}/.github/workflows/package-nightly.yml"
grep -Fq 'ASSET_VERSION_DEB=' "$nightly_workflow" \
    && grep -Fq 'chv-controlplane_${ASSET_VERSION_DEB}_amd64.deb' "$nightly_workflow" \
    && grep -Fq 'chv-controlplane-${ASSET_VERSION_RPM}-1.x86_64.rpm' "$nightly_workflow" \
    && pass "nightly install notes use the published asset names" \
    || fail "nightly install notes name '~' assets the release never serves"

# Release-note install snippets must name the published (sanitized) asset,
# not the internal '~' version GitHub rewrites.
if grep -q 'pkg_version }}_amd64.deb\|pkg_version }}-1.x86_64.rpm' "$release_workflow"; then
    fail "release notes publish an install command for a '~' asset name"
else
    pass "release notes use the published asset version in install commands"
fi
grep -q 'ASSET_VERSION=' "$release_workflow" \
    && pass "release workflow derives the asset version for release notes" \
    || fail "release workflow has no asset-version derivation for release notes"

quick_deb_assets="$(source_path="$quick_installer" bash -c '
    set +u
    # shellcheck disable=SC1090
    source "$source_path" >/dev/null 2>&1
    PKG_KIND=deb
    DEB_ARCH=amd64
    ASSET_VERSION="$(package_asset_version "0.4.0~rc.1")"
    expected_assets
' 2>&1)"
printf '%s\n' "$quick_deb_assets" | grep -qxF 'chv-controlplane_0.4.0.rc.1_amd64.deb' \
    && pass "quick installer looks for the published (dot) RC asset name" \
    || fail "quick installer looks for an RC asset name GitHub never serves"
if printf '%s\n' "$quick_deb_assets" | grep -q '~'; then
    fail "quick installer still expects a literal '~' release asset"
else
    pass "quick installer no longer constructs a '~' release asset name"
fi
[ "$(source_path="$quick_installer" bash -c '
    set +u
    # shellcheck disable=SC1090
    source "$source_path" >/dev/null 2>&1
    package_asset_version "0.4.0~rc.1"
' 2>/dev/null)" = "0.4.0.rc.1" ] \
    && pass "package_asset_version maps 0.4.0~rc.1 to the published 0.4.0.rc.1" \
    || fail "package_asset_version does not match GitHub's filename sanitization"

exit "$rc"
