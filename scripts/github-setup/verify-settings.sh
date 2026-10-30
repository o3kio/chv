#!/usr/bin/env bash
# verify-settings.sh
#
# Verify that the CHV repository hardening settings are in place.
# Prints a human-readable report; exits non-zero if critical settings are missing.
#
# Usage:
#   ./scripts/github-setup/verify-settings.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

ERRORS=0
WARNINGS=0

warn()  { echo "  [WARN]  $1"; ((WARNINGS++)); }
error() { echo "  [ERROR] $1"; ((ERRORS++)); }
ok()    { echo "  [OK]    $1"; }

# ---------------------------------------------------------------------------
# Resolve repo slug
# ---------------------------------------------------------------------------
REMOTE_URL=$(cd "${REPO_ROOT}" && git remote get-url origin 2>/dev/null || true)
REPO_SLUG=""

if [ -n "${REMOTE_URL}" ]; then
    if [[ "${REMOTE_URL}" =~ github\.com[:/]([^/]+)/([^/]+)(\.git)?$ ]]; then
        REPO_SLUG="${BASH_REMATCH[1]}/${BASH_REMATCH[2]%.git}"
    fi
fi

if [ -z "${REPO_SLUG}" ]; then
    error "Could not determine repository slug from git remote."
    REPO_SLUG="${REPO_SLUG:-o3kio/chv}"
fi

echo "========================================"
echo "CHV Repository Hardening Verification"
echo "Repository: ${REPO_SLUG}"
echo "========================================"
echo ""

# ---------------------------------------------------------------------------
# gh CLI availability
# ---------------------------------------------------------------------------
if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
    GH_AVAILABLE=1
else
    GH_AVAILABLE=0
    warn "gh CLI not available or not authenticated; skipping live GitHub checks."
fi

# ---------------------------------------------------------------------------
# 1. CODEOWNERS file
# ---------------------------------------------------------------------------
echo "--- 1. CODEOWNERS ---"
if [ -f "${REPO_ROOT}/.github/CODEOWNERS" ]; then
    ok ".github/CODEOWNERS exists"
    TEAM_COUNT=$(grep -cE '@kubedoio/' "${REPO_ROOT}/.github/CODEOWNERS" || true)
    ok "CODEOWNERS references ${TEAM_COUNT} team entries"
else
    error ".github/CODEOWNERS is missing"
fi
echo ""

# ---------------------------------------------------------------------------
# 2. Workflow permissions
# ---------------------------------------------------------------------------
echo "--- 2. CI Workflow Permissions ---"
for workflow in ci.yml proto.yml security.yml package-pr.yml package-nightly.yml integration-kvm.yml release.yml; do
    wf_path="${REPO_ROOT}/.github/workflows/${workflow}"
    if [ -f "${wf_path}" ]; then
        if grep -qE '^permissions:' "${wf_path}"; then
            ok "${workflow} has explicit permissions block"
        else
            error "${workflow} is missing explicit permissions block"
        fi
    else
        warn "${workflow} not found"
    fi
done
echo ""

# ---------------------------------------------------------------------------
# 3. Live GitHub checks (requires gh CLI)
# ---------------------------------------------------------------------------
if [ "${GH_AVAILABLE}" -eq 1 ]; then
    echo "--- 3. Live GitHub Settings ---"

    # Branch protection for main (ruleset — the mechanism actually in use;
    # the classic branches/main/protection API 404s when only a ruleset exists)
    MAIN_RULESET=$(gh api "repos/${REPO_SLUG}/rulesets" --paginate 2>/dev/null \
        | jq -r '.[] | select(.name == "protect-main") | .id' 2>/dev/null | head -1)
    if [ -n "${MAIN_RULESET}" ]; then
        ok "Ruleset 'protect-main' (id ${MAIN_RULESET}) exists for the default branch"
        RULESET=$(gh api "repos/${REPO_SLUG}/rulesets/${MAIN_RULESET}" 2>/dev/null || true)

        ENFORCEMENT=$(echo "${RULESET}" | jq -r '.enforcement' 2>/dev/null)
        if [ "${ENFORCEMENT}" = "active" ]; then
            ok "protect-main enforcement is active"
        else
            warn "protect-main enforcement is '${ENFORCEMENT}' (staged until commit signing is universal — prompt 03 workstream E; apply with apply-branch-protection.sh --enforce)"
        fi

        if echo "${RULESET}" | jq -e '.rules[] | select(.type == "pull_request")' >/dev/null 2>&1; then
            ok "PR reviews are required"
        else
            error "PR reviews are NOT required"
        fi

        if echo "${RULESET}" | jq -e '.rules[] | select(.type == "required_signatures")' >/dev/null 2>&1; then
            ok "Commit signatures are required (once enforced)"
        else
            warn "Commit signature requirement not present in the ruleset"
        fi

        for ctx in "Rust checks" "UI checks" "E2E tests" "cargo audit" \
                   "cargo deny (advisories)" "cargo deny (bans)" \
                   "cargo deny (licenses)" "cargo deny (sources)"; do
            if echo "${RULESET}" | jq -e --arg c "${ctx}" \
                '.rules[] | select(.type == "required_status_checks")
                 | .parameters.required_status_checks[] | select(.context == $c)' >/dev/null 2>&1; then
                ok "required check present: ${ctx}"
            else
                error "required check missing: ${ctx}"
            fi
        done
    else
        error "Ruleset 'protect-main' not found for 'main'"
    fi

    # Tag protection (ruleset — the mechanism apply-tag-protection.sh manages)
    TAG_RULESET=$(gh api "repos/${REPO_SLUG}/rulesets" --paginate 2>/dev/null \
        | jq -r '.[] | select(.name == "protect-tags") | .id' 2>/dev/null | head -1)
    if [ -n "${TAG_RULESET}" ]; then
        TAGRS=$(gh api "repos/${REPO_SLUG}/rulesets/${TAG_RULESET}" 2>/dev/null || true)
        TAG_ENF=$(echo "${TAGRS}" | jq -r '.enforcement' 2>/dev/null)
        if [ "${TAG_ENF}" = "active" ]; then
            ok "Tag ruleset 'protect-tags' (id ${TAG_RULESET}) is enforced"
        else
            warn "Tag ruleset 'protect-tags' enforcement is '${TAG_ENF}'"
        fi
        for r in creation update deletion; do
            if echo "${TAGRS}" | jq -e --arg t "$r" '.rules[] | select(.type == $t)' >/dev/null 2>&1; then
                ok "tag rule present: ${r}"
            else
                error "tag rule missing: ${r}"
            fi
        done
    else
        error "Tag ruleset 'protect-tags' not found (run apply-tag-protection.sh)"
    fi

    # Default workflow token permissions (Actions permissions API)
    WF_PERMS=$(gh api "repos/${REPO_SLUG}/actions/permissions/workflow" 2>/dev/null || true)
    if echo "${WF_PERMS}" | grep -q '"default_workflow_permissions":"read"'; then
        ok "Default workflow token permissions are read-only"
    else
        warn "Default workflow token permissions are not read-only (repos/<slug>/actions/permissions/workflow)"
    fi

    # Actions SHA-pinning requirement (platform enforcement of the pin policy)
    ACT_PERMS=$(gh api "repos/${REPO_SLUG}/actions/permissions" 2>/dev/null || true)
    if echo "${ACT_PERMS}" | grep -q '"sha_pinning_required":true'; then
        ok "Actions SHA pinning is required at the platform level"
    else
        warn "Actions SHA pinning is not required at the platform level"
    fi
else
    echo "--- 3. Live GitHub Settings ---"
    warn "Skipped (gh CLI not available)"
fi
echo ""

# ---------------------------------------------------------------------------
# 4. Dependabot
# ---------------------------------------------------------------------------
echo "--- 4. Dependabot Configuration ---"
if [ -f "${REPO_ROOT}/.github/dependabot.yml" ]; then
    ok ".github/dependabot.yml exists"
    ECOSYSTEMS=$(grep -cE 'package-ecosystem:' "${REPO_ROOT}/.github/dependabot.yml" || true)
    ok "Dependabot configured for ${ECOSYSTEMS} ecosystem(s)"
else
    error ".github/dependabot.yml is missing"
fi
echo ""

# ---------------------------------------------------------------------------
# 5. Security policy
# ---------------------------------------------------------------------------
echo "--- 5. Security Policy ---"
if [ -f "${REPO_ROOT}/SECURITY.md" ]; then
    ok "SECURITY.md exists"
else
    warn "SECURITY.md is missing"
fi
echo ""

# ---------------------------------------------------------------------------
# 6. Release safety
# ---------------------------------------------------------------------------
echo "--- 6. Release Workflow Safety ---"
RELEASE_WF="${REPO_ROOT}/.github/workflows/release.yml"
if [ -f "${RELEASE_WF}" ]; then
    if grep -q 'environment:' "${RELEASE_WF}"; then
        ok "release.yml uses environment gating"
    else
        warn "release.yml may not use environment gating"
    fi

    if grep -q 'skip_changelog_check' "${RELEASE_WF}"; then
        ok "release.yml has changelog check toggle"
    else
        warn "release.yml may not validate changelog"
    fi
else
    warn "release.yml not found"
fi
echo ""

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo "========================================"
echo "Summary: ${ERRORS} error(s), ${WARNINGS} warning(s)"
echo "========================================"

if [ "${ERRORS}" -gt 0 ]; then
    echo ""
    echo "Critical issues found. Run the setup scripts to fix:"
    echo "  ./scripts/github-setup/apply-branch-protection.sh [--enforce]"
    echo "  ./scripts/github-setup/apply-tag-protection.sh"
    exit 1
else
    echo "All critical checks passed."
    exit 0
fi
