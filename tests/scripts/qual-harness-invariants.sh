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

exit "$rc"
