# CODEOWNERS team migration

> **Status (2026-11-03): NOT MIGRATED.** `.github/CODEOWNERS` still uses the
> individual handles `@zoorpha` and `@senolcolak`. The `o3kio` organization
> returned no teams when checked on 2026-11-03. Do not replace the individual
> handles with team handles until the teams exist, are visible, have repository
> write access, and a test pull request confirms that code-owner review still
> triggers.

This document records the intended team ownership, the exact migration steps,
and the verification required before the repository switches from individual
owners to teams.

The commands below require an organization owner account and a token with
`admin:org` and `repo` scope. A repository administrator who is not an
organization owner cannot create teams or grant team access.

## Why the switch is staged

GitHub silently ignores a CODEOWNERS entry that names a team that does not
exist, is secret, or lacks write access. Replacing working individual handles
with non-existent team handles would weaken review enforcement without a
visible error.

The current `.github/CODEOWNERS` owners are effective:

- `@zoorpha` and `@senolcolak` are repository administrators.
- Both are listed on every ownership path, including the default `*` entry.
- The `protect-main` ruleset requires code-owner review, and that enforcement is
  active.

Keep those entries until the migration is verified.

## Intended team handles

These handles are the planned names for the `o3kio` organization. They do not
exist yet. The `@kubedoio/...` names in older repository comments are from a
different organization and must not be used in this repository.

| Intended team | Intended scope | Current individual owners |
| ------------- | -------------- | ------------------------- |
| `@o3kio/chv-admins` | Repository administration, CI, security policy, `.github/`, `SECURITY.md`, governance records, release settings | @zoorpha @senolcolak |
| `@o3kio/chv-maintainers` | Rust backend, control plane, `cmd/`, `crates/`, `proto/`, `gen/rust/`, `ebpf/`, migrations | @zoorpha @senolcolak |
| `@o3kio/chv-frontend` | Web UI and backend-for-frontend integration, `ui/` | @zoorpha @senolcolak |
| `@o3kio/chv-ops` | Deployment, install, packaging, release engineering, `deploy/`, `scripts/`, `packaging/`, `docs/install/`, `docs/release/`, `Makefile`, `VERSION` | @zoorpha @senolcolak |
| `@o3kio/chv-architecture` | ADRs, component specs, architecture and design documents, `docs/specs/adr/`, `docs/specs/component/`, `docs/ARCHITECTURE.md`, `DESIGN.md` | @zoorpha @senolcolak |

The intended final CODEOWNERS keeps the current path granularity. The
`@o3kio/chv-maintainers` team is the intended default for unmatched paths, and
the more specific teams cover the paths in the table. The `@o3kio/chv-admins`
team remains on administrative and governance paths.

Initial membership must come from the active maintainers in
[`MAINTAINERS.md`](../../MAINTAINERS.md). Do not add a person who is not a
maintainer, and do not treat this table as a record of current team membership.

## Migration steps

### 1. Confirm the current state

Run from a checkout of this repository:

```bash
gh api repos/o3kio/chv --jq '.owner.login'
gh api orgs/o3kio/teams --jq '.[].slug'
```

The first command must print `o3kio`. The second command must list the team
slugs after creation. If it returns nothing, the teams do not exist yet.

### 2. Create the teams

Create each team as a visible team (`privacy: closed`), not a secret team.
Secret teams are not visible to the repository and do not work in CODEOWNERS.

```bash
gh api --method POST orgs/o3kio/teams \
  -f name='CHV Admins' \
  -f description='CHV repository administration and security policy' \
  -f privacy='closed'

gh api --method POST orgs/o3kio/teams \
  -f name='CHV Maintainers' \
  -f description='CHV Rust backend and control plane' \
  -f privacy='closed'

gh api --method POST orgs/o3kio/teams \
  -f name='CHV Frontend' \
  -f description='CHV Web UI and backend-for-frontend (BFF) integration' \
  -f privacy='closed'

gh api --method POST orgs/o3kio/teams \
  -f name='CHV Ops' \
  -f description='CHV deployment, packaging, and release engineering' \
  -f privacy='closed'

gh api --method POST orgs/o3kio/teams \
  -f name='CHV Architecture' \
  -f description='CHV ADRs, specs, and architecture' \
  -f privacy='closed'
```

The resulting slugs must match the handles in the table:
`chv-admins`, `chv-maintainers`, `chv-frontend`, `chv-ops`, and
`chv-architecture`.

### 3. Grant repository access

A team needs at least write (`push`) permission for GitHub to accept it as a
code owner. The admin team may need a higher permission for repository
settings.

```bash
for team in chv-admins chv-maintainers chv-frontend chv-ops chv-architecture; do
  gh api --method PUT "orgs/o3kio/teams/${team}/repos/o3kio/chv" \
    -f permission='push'
done
```

Use `permission='admin'` for `chv-admins` only if its members need to manage
repository settings. Do not grant admin to every team.

### 4. Add members

Add only the active maintainers recorded in `MAINTAINERS.md`:

```bash
gh api --method PUT orgs/o3kio/teams/chv-admins/memberships/<username> \
  -f role='member'
```

Repeat for each team and each maintainer. Do not add an account that is not in
`MAINTAINERS.md`.

### 5. Verify team visibility and access

Run these checks before editing CODEOWNERS:

```bash
gh api orgs/o3kio/teams --jq '.[] | {slug, privacy}'
gh api orgs/o3kio/teams/chv-admins --jq '{name, slug, privacy}'
gh api orgs/o3kio/teams/chv-admins/repos/o3kio/chv --jq '.permissions'
gh api orgs/o3kio/teams/chv-maintainers/repos/o3kio/chv --jq '.permissions'
```

Required result:

- `privacy` is `closed` for every team.
- `permissions.push` is `true` (or `permissions.admin` is `true`) for every
  team used as an owner.
- Each team slug resolves; no command returns `404`.

### 6. Update CODEOWNERS in two phases

Do not replace all individual handles in one step.

**Phase A — add team handles alongside the individuals.** For one section at a
time, add the intended team to the existing owner line. For example:

```text
/crates/  @zoorpha @senolcolak @o3kio/chv-maintainers
```

The individual owners remain effective, so a missing or invisible team cannot
remove review coverage.

**Phase B — remove the individuals after verification.** After the verification
in step 7 passes for that section, remove `@zoorpha` and `@senolcolak` from that
line and leave the team handle. Repeat the verification for each section you
change.

### 7. Verify that code-owner review still triggers

Open a draft pull request that touches a path covered by the migrated section.
Do not merge it until all checks pass.

```bash
gh pr view <number> --json reviewRequests,reviewDecision
```

Confirm that the requested reviewers include the intended team. Then confirm
that the live ruleset still requires code-owner review:

```bash
RULESET_ID=$(gh api repos/o3kio/chv/rulesets \
  --jq '.[] | select(.name=="protect-main") | .id')
gh api "repos/o3kio/chv/rulesets/${RULESET_ID}" \
  --jq '.rules[] | select(.type=="pull_request") | .parameters.require_code_owner_review'
```

The result must be `true`. Also confirm that the pull request shows a pending
code-owner review, or that it receives one from the team. If the team does not
appear, stop and restore the individual handles.

### 8. Record completion

Only after every migrated section passes verification:

1. Remove the individual handles that are no longer needed.
2. Update this document's status blockquote and the header comment in
   `.github/CODEOWNERS`.
3. Update [`REPOSITORY_HARDENING.md`](REPOSITORY_HARDENING.md) and
   [`BRANCH_PROTECTION.md`](BRANCH_PROTECTION.md) if their statements about
   CODEOWNERS change.
4. Keep the fallback instructions in this document in case a team is removed or
   made secret later.

## Rollback

If a team is missing, secret, lacks write access, or does not trigger a review:

1. Restore the individual handles in the affected CODEOWNERS section.
2. Re-run the pull-request verification.
3. Fix the team visibility or permission before trying again.

A rollback is a repository change and must pass the same review and CI gates.

## Related documents

- [`.github/CODEOWNERS`](../../.github/CODEOWNERS) — current enforcement file
- [`GOVERNANCE.md`](../../GOVERNANCE.md) — ownership and bus-factor model
- [`MAINTAINERS.md`](../../MAINTAINERS.md) — source of truth for maintainer
  membership
- [`BRANCH_PROTECTION.md`](BRANCH_PROTECTION.md) — `protect-main` ruleset
- [`REPOSITORY_HARDENING.md`](REPOSITORY_HARDENING.md) — CODEOWNERS checklist
