# Branch and Tag Protection

This document describes the required GitHub repository settings for the CHV project. These settings **cannot be expressed as files in the repository**; they must be configured via the GitHub UI, the `gh` CLI, or the rulesets API.

> **Status (2026-10-03): `protect-main` enforcement is ACTIVE.** The ruleset
> (id 17358522) matches the specification below, including all eight required
> status checks, and is enforced on the default branch. The signing
> prerequisite recorded on 2026-09-30 (mixed signed/unsigned history) was
> resolved: the 2026-10-03 activation verified the last 10 `main` commits all
> `verified=true` (`reason=valid`), a direct-push smoke test was declined by
> the ruleset, and `verify-settings.sh` reports `protect-main enforcement is
> active` with 0 errors / 0 warnings. Activation procedure and evidence:
> [main-protection.md §5/§7](../evidence/production-readiness/v0.3.0-rc1/03-security-reproducibility-governance/main-protection.md).

---

## Branch Ruleset: `protect-main`

Applied to the default branch (`main`) via **Settings → Rules → Rulesets** (the
mechanism actually in use — the legacy branch-protection API is not).

| Rule | Value | Rationale |
|---------|-------|-----------|
| **Restrict deletions** | ✅ Enabled | Prevents accidental branch deletion |
| **Block force pushes** | ✅ Enabled (non-fast-forward) | Prevents history rewriting |
| **Require signed commits** | ✅ Enabled | Cryptographic provenance for every commit |
| **Require a pull request before merging** | ✅ Enabled | No direct pushes to `main` |
| **Required approvals** | `1` minimum | At least one human review |
| **Dismiss stale PR approvals when new commits are pushed** | ✅ Enabled | Prevents approval hijacking |
| **Require review from CODEOWNERS** | ✅ Enabled | Enforces the ownership model in `.github/CODEOWNERS` |
| **Require conversation resolution before merging** | ✅ Enabled | Ensures all review threads are addressed |
| **Extra approval for unattributed changes** | ✅ Enabled | Changes pushed by someone other than the PR author need a second look |
| **Require status checks to pass** | ✅ Enabled | CI and Security must be green |
| **Status checks that are required** | `Rust checks`, `UI checks`, `E2E tests`, `cargo audit`, `cargo deny (advisories)`, `cargo deny (bans)`, `cargo deny (licenses)`, `cargo deny (sources)` | Gates from `.github/workflows/ci.yml` and `.github/workflows/security.yml` |
| **Allowed merge methods** | merge, squash, rebase | Matches current conventions |

> **Note:** every required check must report on **every** PR. For that reason
> `security.yml` has no `pull_request` path filter — a path-filtered workflow
> leaves its checks unreported ("Expected") on PRs outside the filter and
> blocks their merge once the check is required.

> **Pending admin step (from #659, not yet applied):** `ci.yml` now also emits
> `Rust coverage (ratchet, lines >= 76%)` and `Real-BFF E2E`. They are
> blocking within the workflow, but they are **not** in the live `protect-main`
> ruleset's required-check list yet. `apply-branch-protection.sh` now declares
> both contexts, so an admin can add them by re-applying the ruleset
> (`--enforce`, or the rulesets UI); this repository change alone cannot make a
> check required. Until that step runs, a red coverage/real-BFF job is visible
> but does not itself block merge.

---

## Tag Ruleset: `protect-tags`

Applied to version tags via **Settings → Rules → Rulesets** (managed by
`apply-tag-protection.sh`, same mechanism as `protect-main`).

| Rule | Value | Rationale |
|---------|-------|-----------|
| **Tag name pattern** | `refs/tags/v*` | Protects all version tags |
| **Restrict creations** | ✅ Enabled | Only admins/maintainers can create version tags |
| **Restrict updates** | ✅ Enabled | Tags cannot be force-moved |
| **Restrict deletions** | ✅ Enabled | Tags cannot be deleted |
| **Enforcement** | ✅ Active | No signing prerequisite — no workflow creates tags; `release.yml` only reacts to tag pushes |

This prevents accidental or malicious tag creation that could trigger the release workflow (`release.yml`).

---

## Automated Application

Run the helper scripts (requires `gh` CLI and repo admin access):

```bash
# Update the protect-main ruleset definition (keeps current enforcement state)
./scripts/github-setup/apply-branch-protection.sh

# Activate enforcement (was gated on commit-signing readiness — active since 2026-10-03, see the status note above)
./scripts/github-setup/apply-branch-protection.sh --enforce

# Print the live ruleset without changing anything
./scripts/github-setup/apply-branch-protection.sh --audit

# Apply tag protection
./scripts/github-setup/apply-tag-protection.sh

# Verify current settings
./scripts/github-setup/verify-settings.sh
```

See the script source for the exact API payloads.

---

## Related

- [`.github/CODEOWNERS`](../../.github/CODEOWNERS) — ownership mapping
- [`REPOSITORY_HARDENING.md`](./REPOSITORY_HARDENING.md) — full hardening checklist
