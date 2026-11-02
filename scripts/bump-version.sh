#!/bin/bash
# Bump the project version across all relevant files.
# Usage: ./scripts/bump-version.sh [major|minor|patch]
# Default bump type is "patch".
# Pass --dry-run as a second argument to preview changes without writing files.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

BUMP_TYPE="${1:-patch}"
DRY_RUN="${2:-}"

# ---------------------------------------------------------------------------
# Validate bump type
# ---------------------------------------------------------------------------
case "$BUMP_TYPE" in
  major|minor|patch)
    ;;
  *)
    echo "Unknown bump type: $BUMP_TYPE" >&2
    echo "Usage: $0 [major|minor|patch] [--dry-run]" >&2
    exit 1
    ;;
esac

# ---------------------------------------------------------------------------
# Read current version
# ---------------------------------------------------------------------------
OLD_VERSION=$(cat VERSION)
IFS='.' read -r MAJOR MINOR PATCH <<< "$OLD_VERSION"

# ---------------------------------------------------------------------------
# Compute new version
# ---------------------------------------------------------------------------
case "$BUMP_TYPE" in
  major)
    MAJOR=$((MAJOR + 1))
    MINOR=0
    PATCH=0
    ;;
  minor)
    MINOR=$((MINOR + 1))
    PATCH=0
    ;;
  patch)
    PATCH=$((PATCH + 1))
    ;;
esac

NEW_VERSION="${MAJOR}.${MINOR}.${PATCH}"

# ---------------------------------------------------------------------------
# Escape sed (BRE) metacharacters so a version string matches literally
#
# Rationale: every sed call below interpolates a version into a BRE pattern.
# Unescaped, "0.2.0" is the regex `0.2.0`, whose dots match any character —
# with OLD_VERSION=0.2.0 it matched "0.200" inside the bridge CIDR
# 10.200.0.1/24 and corrupted six lines in docs/DEPLOYMENT.md and
# scripts/install.sh (including the installer's runtime
# INSTALL_CHV_BRIDGE_CIDR default) during the 0.3.0 release cut; PR #473
# repaired the damage by hand and left this script as the follow-up. Only
# "." occurs in a plain semver, but the full BRE metacharacter set is
# escaped so the helper stays correct for pre-release-suffixed versions
# read from files (ui/package.json, the sidebar label).
#
# This intentionally does NOT add word-boundary anchoring: the global
# replace must keep catching version refs embedded in larger tokens
# (v0.2.0, chv-v0.2.0-alpha, Version: 0.2.0, version = "0.2.0").
# ---------------------------------------------------------------------------
escape_sed_pattern() {
  # Bracket list: ] [ \ . * ^ $ — every BRE metacharacter that can appear
  # in a version string. Each match is prefixed with a literal backslash.
  # Assumption: versions are backslash-free (semver-validated VERSION,
  # package.json semver, sidebar [0-9][0-9.]* extraction). A literal
  # backslash would double-escape and fail to MATCH (fail-safe: no
  # corruption, just no rewrite) — no realistic source can produce one.
  printf '%s' "$1" | sed 's/[][\.*^$]/\\&/g'
}

# Literal (BRE-escaped) form of the old version, computed once for the
# call sites below that interpolate it.
OLD_VERSION_ESC="$(escape_sed_pattern "$OLD_VERSION")"

if [ "$DRY_RUN" = "--dry-run" ]; then
  echo "[DRY RUN] Would bump: ${OLD_VERSION} -> ${NEW_VERSION}"
  exit 0
fi

echo "Bumping version: ${OLD_VERSION} -> ${NEW_VERSION}"

# ---------------------------------------------------------------------------
# 1. VERSION (source of truth)
# ---------------------------------------------------------------------------
echo "$NEW_VERSION" > VERSION

# ---------------------------------------------------------------------------
# 2. All Cargo.toml files (workspace + crates)
# ---------------------------------------------------------------------------
# The ^version = "..." anchor keeps the match to the version stanza, but the
# interpolated version is still a BRE pattern — escape it so only the exact
# literal version rewrites (belt and braces; see escape_sed_pattern above).
find . -name 'Cargo.toml' -not -path './target/*' -exec sed -i "s/^version = \"${OLD_VERSION_ESC}\"/version = \"${NEW_VERSION}\"/" {} +

# ---------------------------------------------------------------------------
# 3. UI package.json + package-lock.json (via npm, with sed fallback)
# ---------------------------------------------------------------------------
UI_OLD_VERSION=$(grep -m1 '"version"' ui/package.json | sed -E 's/.*"version": "([^"]+)".*/\1/')
# UI_OLD_VERSION is read from ui/package.json, not derived — escape it
# before it becomes a sed pattern.
UI_OLD_VERSION_ESC="$(escape_sed_pattern "$UI_OLD_VERSION")"
if command -v npm &>/dev/null; then
  (
    cd ui
    npm --no-git-tag-version version "$NEW_VERSION" >/dev/null 2>&1 || true
  )
else
  sed -i "s/\"version\": \"${UI_OLD_VERSION_ESC}\"/\"version\": \"${NEW_VERSION}\"/" ui/package.json
  sed -i "s/\"version\": \"${UI_OLD_VERSION_ESC}\"/\"version\": \"${NEW_VERSION}\"/" ui/package-lock.json
fi

# ---------------------------------------------------------------------------
# 4. UI sidebar version label
# ---------------------------------------------------------------------------
SIDEBAR_FILE="ui/src/lib/components/shell/Sidebar.svelte"
if [ -f "$SIDEBAR_FILE" ]; then
  SIDEBAR_OLD_VERSION=$(grep -o 'chv-v[0-9][0-9.]*-alpha' "$SIDEBAR_FILE" | sed 's/chv-v//;s/-alpha//' || echo "$OLD_VERSION")
  # Extracted from the sidebar label — escape before it becomes a sed pattern.
  SIDEBAR_OLD_VERSION_ESC="$(escape_sed_pattern "$SIDEBAR_OLD_VERSION")"
  sed -i "s/chv-v${SIDEBAR_OLD_VERSION_ESC}-alpha/chv-v${NEW_VERSION}-alpha/" "$SIDEBAR_FILE"
fi

# ---------------------------------------------------------------------------
# 5. Documentation & install scripts
# ---------------------------------------------------------------------------
# Global replaces — OLD_VERSION_ESC (not OLD_VERSION) so the version matches
# literally. This is the site of the 0.3.0-release CIDR corruption: the
# unescaped regex 0.2.0 matched "0.200" inside 10.200.0.1/24 (see
# escape_sed_pattern above).
sed -i "s/${OLD_VERSION_ESC}/${NEW_VERSION}/g" README.md
sed -i "s/${OLD_VERSION_ESC}/${NEW_VERSION}/g" docs/DEPLOYMENT.md
sed -i "s/${OLD_VERSION_ESC}/${NEW_VERSION}/g" scripts/install.sh
sed -i "s/${OLD_VERSION_ESC}/${NEW_VERSION}/g" scripts/hosting/cloudflare-worker.js
sed -i "s/${OLD_VERSION_ESC}/${NEW_VERSION}/g" scripts/hosting/github-pages-index.html

# Root-level copies of the hosting templates (published by hand; see
# docs/release/release-engineering-plan.md). They are duplicates of the
# scripts/hosting/ files and must not drift.
for f in cloudflare-worker.js github-pages-index.html; do
  if [ -f "$f" ]; then
    sed -i "s/${OLD_VERSION_ESC}/${NEW_VERSION}/g" "$f"
  fi
done

# ---------------------------------------------------------------------------
# 6. Update Cargo.lock so it stays in sync with Cargo.toml
# ---------------------------------------------------------------------------
if command -v cargo &>/dev/null; then
  cargo update --workspace >/dev/null 2>&1 || true
fi

echo "Version bumped to ${NEW_VERSION}"
echo ""
echo "Next steps:"
echo "  1. Review the diff: git diff"
echo "  2. Update CHANGELOG.md if this is a new release"
echo "  3. Commit the changes and optionally tag:"
echo "     git add -A && git commit -m \"release: bump version to ${NEW_VERSION}\""
echo "     git tag v${NEW_VERSION}"
