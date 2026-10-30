#!/usr/bin/env bash
# apply-tag-protection.sh
#
# Manage the `protect-tags` ruleset (version-tag protection) on the CHV
# repository using the GitHub rulesets API — the same mechanism as
# `protect-main` (see docs/governance/BRANCH_PROTECTION.md).
#
# Must be run by a repository admin with an authenticated gh CLI.
#
# Usage:
#   ./scripts/github-setup/apply-tag-protection.sh            # create or
#                                                                # update the
#                                                                # ruleset,
#                                                                # keep current
#                                                                # enforcement
#   ./scripts/github-setup/apply-tag-protection.sh --enforce  # set enforcement
#                                                                # to active
#   ./scripts/github-setup/apply-tag-protection.sh --audit    # print the live
#                                                                # ruleset and
#                                                                # exit
#
# Unlike protect-main there is no commit-signing prerequisite: the rules
# restrict ref creation/update/deletion on refs/tags/v* to admins and
# maintainers, which is exactly the release process (no workflow or CI job
# creates tags — release.yml only reacts to tag pushes). The ruleset is
# therefore created with enforcement active on first run.
#
# This prevents accidental or malicious version-tag creation that could
# trigger the release workflow (release.yml).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

RULESET_NAME="protect-tags"
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
    echo "       REPO_SLUG=o3kio/chv ./scripts/github-setup/apply-tag-protection.sh"
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
# Resolve the ruleset id by name (may not exist yet — creation is supported)
# ---------------------------------------------------------------------------
RULESET_ID=$(gh api "repos/${REPO_SLUG}/rulesets" --paginate \
    -q ".[] | select(.name == \"${RULESET_NAME}\") | .id" | head -1)

if [ "${MODE}" = "audit" ]; then
    if [ -z "${RULESET_ID}" ]; then
        echo "Ruleset '${RULESET_NAME}' does not exist on ${REPO_SLUG}."
        exit 0
    fi
    gh api "repos/${REPO_SLUG}/rulesets/${RULESET_ID}" | jq .
    exit 0
fi

# ---------------------------------------------------------------------------
# The ruleset definition (idempotent create-or-replace).
#
# - target: tag; conditions: refs/tags/v*
# - creation/update/deletion restricted to admins/maintainers (the human
#   release process); no workflow creates tags.
# ---------------------------------------------------------------------------
ENFORCEMENT="active"
if [ -n "${RULESET_ID}" ] && [ "${MODE}" != "enforce" ]; then
    ENFORCEMENT=$(gh api "repos/${REPO_SLUG}/rulesets/${RULESET_ID}" -q '.enforcement')
fi

echo "Applying ruleset '${RULESET_NAME}' (enforcement: ${ENFORCEMENT}) ..."

PAYLOAD=$(cat <<EOF
{
  "name": "${RULESET_NAME}",
  "target": "tag",
  "enforcement": "${ENFORCEMENT}",
  "conditions": {
    "ref_name": { "include": ["refs/tags/v*"], "exclude": [] }
  },
  "rules": [
    { "type": "creation" },
    { "type": "update" },
    { "type": "deletion" }
  ]
}
EOF
)

if [ -n "${RULESET_ID}" ]; then
    gh api --method PUT "repos/${REPO_SLUG}/rulesets/${RULESET_ID}" --input - <<<"${PAYLOAD}" > /dev/null
else
    gh api --method POST "repos/${REPO_SLUG}/rulesets" --input - <<<"${PAYLOAD}" \
        -q '.id' | xargs -I{} echo "Created ruleset '${RULESET_NAME}' (id {})."
fi

# ---------------------------------------------------------------------------
# Verify
# ---------------------------------------------------------------------------
RULESET_ID=$(gh api "repos/${REPO_SLUG}/rulesets" --paginate \
    -q ".[] | select(.name == \"${RULESET_NAME}\") | .id" | head -1)
FINAL=$(gh api "repos/${REPO_SLUG}/rulesets/${RULESET_ID}")
FINAL_ENFORCEMENT=$(echo "${FINAL}" | jq -r '.enforcement')
FINAL_RULES=$(echo "${FINAL}" | jq -r '.rules[].type' | sort | tr '\n' ' ')

echo ""
echo "Ruleset '${RULESET_NAME}' (id ${RULESET_ID}) applied successfully:"
echo "  enforcement:  ${FINAL_ENFORCEMENT}"
echo "  rules:        ${FINAL_RULES}"
echo "  tag pattern:  refs/tags/v*"
