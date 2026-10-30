#!/usr/bin/env bash
# apply-branch-protection.sh
#
# Manage the `protect-main` ruleset on the CHV repository using the GitHub
# rulesets API (the mechanism actually in use — see
# docs/governance/BRANCH_PROTECTION.md).
#
# Must be run by a repository admin with an authenticated gh CLI.
#
# Usage:
#   ./scripts/github-setup/apply-branch-protection.sh            # update the
#                                                                # ruleset
#                                                                # definition,
#                                                                # keep current
#                                                                # enforcement
#   ./scripts/github-setup/apply-branch-protection.sh --enforce  # set
#                                                                # enforcement
#                                                                # to active
#   ./scripts/github-setup/apply-branch-protection.sh --audit    # print the
#                                                                # live ruleset
#                                                                # and exit
#
# Enforcement prerequisites (checked before --enforce proceeds):
#   - required_signatures is part of the ruleset: every committer must sign
#     commits (recent `main` history mixes signed and unsigned commits; until
#     signing is universal, enabling enforcement blocks unsigned PRs).
#   - require_code_owner_review + 1 approval: a PR author cannot self-approve;
#     the second CODEOWNERS maintainer must be available to review.
#
# The required status checks (Rust checks, UI checks, E2E tests from ci.yml;
# cargo audit + the cargo deny matrix from security.yml) all report on every
# PR — security.yml deliberately has no pull_request path filter for this
# reason (see its header comment).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

RULESET_NAME="protect-main"
MODE="update"

usage() {
    grep '^#   \./scripts' "$0" | sed 's/^#   //' >&2
    exit 2
}

for arg in "$@"; do
    case "$arg" in
        --enforce) MODE="enforce" ;;
        --audit)   MODE="audit" ;;
        -h|--help) usage ;;
        *)         echo "unknown argument: $arg" >&2; usage ;;
    esac
done

# ---------------------------------------------------------------------------
# Resolve repo slug from git remote
# ---------------------------------------------------------------------------
REMOTE_URL=$(cd "${REPO_ROOT}" && git remote get-url origin 2>/dev/null || true)
REPO_SLUG="${REPO_SLUG:-}"

if [ -z "${REPO_SLUG}" ] && [ -n "${REMOTE_URL}" ]; then
    # Handle both HTTPS and SSH remotes
    if [[ "${REMOTE_URL}" =~ github\.com[:/]([^/]+)/([^/]+)(\.git)?$ ]]; then
        REPO_SLUG="${BASH_REMATCH[1]}/${BASH_REMATCH[2]%.git}"
    fi
fi

if [ -z "${REPO_SLUG}" ]; then
    echo "ERROR: Could not determine repository slug from git remote."
    echo "       Set REPO_SLUG manually, e.g.:"
    echo "       REPO_SLUG=o3kio/chv ./scripts/github-setup/apply-branch-protection.sh"
    exit 1
fi

echo "Target repository: ${REPO_SLUG}"

# ---------------------------------------------------------------------------
# Verify gh CLI
# ---------------------------------------------------------------------------
if ! command -v gh >/dev/null 2>&1; then
    echo "ERROR: gh CLI is not installed. Install from https://cli.github.com/"
    exit 1
fi

if ! gh auth status >/dev/null 2>&1; then
    echo "ERROR: gh CLI is not authenticated. Run: gh auth login"
    exit 1
fi

# ---------------------------------------------------------------------------
# Resolve the ruleset id by name
# ---------------------------------------------------------------------------
RULESET_ID=$(gh api "repos/${REPO_SLUG}/rulesets" --paginate \
    -q ".[] | select(.name == \"${RULESET_NAME}\") | .id" | head -1)

if [ -z "${RULESET_ID}" ]; then
    echo "ERROR: ruleset '${RULESET_NAME}' not found on ${REPO_SLUG}."
    echo "       Create it in the GitHub UI (Settings → Rules → Rulesets) or"
    echo "       adapt this script to create it via POST /repos/{slug}/rulesets."
    exit 1
fi

LIVE=$(gh api "repos/${REPO_SLUG}/rulesets/${RULESET_ID}")
LIVE_ENFORCEMENT=$(echo "${LIVE}" | jq -r '.enforcement')

if [ "${MODE}" = "audit" ]; then
    echo "Ruleset ${RULESET_NAME} (id ${RULESET_ID}) — enforcement: ${LIVE_ENFORCEMENT}"
    echo "${LIVE}" | jq .
    exit 0
fi

echo "Ruleset '${RULESET_NAME}' (id ${RULESET_ID}) — current enforcement: ${LIVE_ENFORCEMENT}"

# ---------------------------------------------------------------------------
# Pre-enforcement checks
# ---------------------------------------------------------------------------
if [ "${MODE}" = "enforce" ]; then
    UNSIGNED=$(gh api "repos/${REPO_SLUG}/commits?per_page=10" \
        -q '[.[] | select(.commit.verification.verified != true)] | length')
    if [ "${UNSIGNED}" -gt 0 ]; then
        echo "ERROR: ${UNSIGNED} of the last 10 commits on main are unsigned."
        echo "       The ruleset includes required_signatures — enabling"
        echo "       enforcement now would block unsigned PRs. Configure commit"
        echo "       signing for all committers first (GitHub Settings →"
        echo "       SSH signing keys / GPG keys), then re-run."
        exit 1
    fi
fi

# ---------------------------------------------------------------------------
# The ruleset definition (idempotent PUT — replaces the definition).
#
# Required status checks must report on every PR:
#   - ci.yml: Rust checks, UI checks, E2E tests
#   - security.yml: cargo audit, cargo deny (advisories|bans|licenses|sources)
# ---------------------------------------------------------------------------
ENFORCEMENT="${LIVE_ENFORCEMENT}"
if [ "${MODE}" = "enforce" ]; then
    ENFORCEMENT="active"
fi

echo "Applying ruleset definition (enforcement: ${ENFORCEMENT}) ..."
gh api --method PUT "repos/${REPO_SLUG}/rulesets/${RULESET_ID}" \
    --input - <<EOF
{
  "name": "${RULESET_NAME}",
  "target": "branch",
  "enforcement": "${ENFORCEMENT}",
  "conditions": {
    "ref_name": { "include": ["~DEFAULT_BRANCH"], "exclude": [] }
  },
  "rules": [
    { "type": "deletion" },
    { "type": "non_fast_forward" },
    { "type": "required_signatures" },
    {
      "type": "pull_request",
      "parameters": {
        "required_approving_review_count": 1,
        "dismiss_stale_reviews_on_push": true,
        "require_code_owner_review": true,
        "require_last_push_approval": false,
        "required_review_thread_resolution": true,
        "require_extra_approval_for_unattributed_changes": true,
        "allowed_merge_methods": ["merge", "squash", "rebase"]
      }
    },
    {
      "type": "required_status_checks",
      "parameters": {
        "strict_required_status_checks_policy": false,
        "do_not_enforce_on_create": false,
        "required_status_checks": [
          { "context": "Rust checks" },
          { "context": "UI checks" },
          { "context": "E2E tests" },
          { "context": "cargo audit" },
          { "context": "cargo deny (advisories)" },
          { "context": "cargo deny (bans)" },
          { "context": "cargo deny (licenses)" },
          { "context": "cargo deny (sources)" }
        ]
      }
    }
  ]
}
EOF

# ---------------------------------------------------------------------------
# Verify
# ---------------------------------------------------------------------------
FINAL=$(gh api "repos/${REPO_SLUG}/rulesets/${RULESET_ID}")
FINAL_ENFORCEMENT=$(echo "${FINAL}" | jq -r '.enforcement')
REQUIRED=$(echo "${FINAL}" | jq -r '.rules[] | select(.type == "required_status_checks")
    | .parameters.required_status_checks[].context' | sort | tr '\n' ' ')

echo ""
echo "Ruleset applied successfully:"
echo "  enforcement:      ${FINAL_ENFORCEMENT}"
echo "  required checks:  ${REQUIRED}"
if [ "${FINAL_ENFORCEMENT}" != "active" ]; then
    echo ""
    echo "NOTE: enforcement is not active. When commit signing is set up for"
    echo "      all committers, run with --enforce to activate."
fi
