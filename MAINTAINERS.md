# Maintainers

> **This file is the source of truth for maintainer membership.** Membership
> changes take effect when they are recorded here. `.github/CODEOWNERS`
> enforces review for paths; it does not define who is a maintainer.

Last reviewed: 2026-11-03.

## Current maintainers

The repository has two maintainers. Both are repository administrators and both
are listed as code owners on every path in `.github/CODEOWNERS`.

| Maintainer | GitHub | Repository access | CODEOWNERS scope | Areas evidenced in the repository |
| ---------- | ------ | ----------------- | ---------------- | --------------------------------- |
| Zoorpha | @zoorpha | Admin | All paths: `*`, `.github/`, Rust crates and protobuf, `ui/`, deployment and release, ADRs and specs, migrations | UI and backend-for-frontend (BFF), control plane, agent, storage, and specification work. The largest shares of authored changes are in `ui/src`, `crates/chv-webui-bff`, `crates/chv-controlplane-service`, `crates/chv-agent-core`, and `docs/specs` |
| Senol Colak | @senolcolak | Admin | All paths: `*`, `.github/`, Rust crates and protobuf, `ui/`, deployment and release, ADRs and specs, migrations | Backend-for-frontend (BFF), control plane, agent, architecture validation and reconciliation, storage, and release documentation. Authored changes span `crates/chv-webui-bff`, `ui/src`, `crates/chv-controlplane-service`, `crates/chv-agent-core`, and `docs/specs` |

Access was verified on 2026-11-03 with
`gh api repos/o3kio/chv/collaborators`; both accounts returned
`role_name: admin`.

**Organization ownership.** The `o3kio` organization has one owner,
`@senolcolak`, who is also a maintainer. `@zoorpha` is a repository
administrator but not an organization owner
(`gh api 'orgs/o3kio/members?role=admin'` returns only `senolcolak`). Escalation
paths that name "the organization owner" therefore have no independent actor
if the sole owner is unavailable or conflicted; see the residual in
[`GOVERNANCE.md`](GOVERNANCE.md#organization-owner-residual). Appointing a
third, uninvolved organization owner is an open action.

The areas column summarizes where work is visible in commit history. It is not
a restriction on review authority: both maintainers are code owners for every
path, and either may review any area.

## Reviewers

No reviewers are designated under the
[contributor ladder](GOVERNANCE.md#contributor-ladder) as of 2026-11-03.
Reviewer status is recognition for review work; it does not grant write, merge,
release, or key-custody access. When a reviewer is recognized, record the
handle and the recognized area here.

## Review and response expectations

### Pull requests

The `protect-main` ruleset requires at least one approval, a code-owner review,
signed commits, resolved conversations, and the required status checks. See
[`docs/governance/BRANCH_PROTECTION.md`](docs/governance/BRANCH_PROTECTION.md).

- Maintainers review changes in the areas they own and route a change to the
  other maintainer when they cannot review it.
- A maintainer does not approve their own pull request. Because both
  maintainers are code owners on every path, the other maintainer provides the
  required code-owner review.
- High-risk changes, as defined in
  [`CONTRIBUTING.md`](CONTRIBUTING.md#high-risk-changes), need evidence in the
  pull request and should receive an independent review. If both maintainers
  authored the change, ask a reviewer from the contributor ladder before merge.
- Security and release-blocking fixes take priority over feature review.
- The repository does not publish a separate numeric pull-request review SLA.
  The published response targets are the security timelines in
  [`SECURITY.md`](SECURITY.md). Maintainers should acknowledge a review request
  and either review it or hand it to the other maintainer rather than leave it
  unowned.

### Security reports

- Acknowledge a private report within 48 hours.
- Complete initial triage within 7 days.
- Apply the fix targets in [`SECURITY.md`](SECURITY.md): Critical 7 days, High
  30 days, Medium 60 days, Low best effort.
- Follow the 90-day coordinated disclosure policy in `SECURITY.md`.
- Follow the weekly assignment and handoff in
  [`docs/governance/SECURITY_TRIAGE_ROTA.md`](docs/governance/SECURITY_TRIAGE_ROTA.md).

### Releases

- Maintainers own the release decision. The `protect-tags` ruleset restricts
  `v*` tag creation, update, and deletion and currently has no bypass actors,
  so an administrator must grant bypass or pause the ruleset to cut a tag and
  restore it afterwards (record the before/after state).
- Release jobs run in the `rc`/`production` environments. Required-reviewer
  protection is not configured yet; configuring it is a pending administrator
  step in [`docs/release/release-process.md`](docs/release/release-process.md).
- A maintainer verifies the release gates and artifacts described in
  [`docs/release/release-process.md`](docs/release/release-process.md) and
  [`docs/release/verify-release-artifacts.md`](docs/release/verify-release-artifacts.md).
- Signing key custody and rotation follow
  [`docs/governance/KEY_ROTATION.md`](docs/governance/KEY_ROTATION.md).

## Adding a maintainer

1. Follow the ladder criteria and nomination process in
   [`GOVERNANCE.md`](GOVERNANCE.md#contributor-ladder).
2. The current maintainers approve the nomination unanimously.
3. The organization owner grants repository access.
4. Record the new maintainer in this file and update `.github/CODEOWNERS` in
   the same pull request when the ownership scope changes.
5. Add the person to the security triage rota at the next weekly handoff.
6. Complete the onboarding checks in
   [`GOVERNANCE.md`](GOVERNANCE.md#adding-a-maintainer).

Do not grant access before the nomination is approved and recorded here.

## Emeritus maintainers

Emeritus status removes write, merge, release, environment, and key-custody
access while preserving the person's record and credit. A maintainer may
request emeritus status, or the remaining maintainers may agree to it after a
sustained period without review, release, or security-triage activity.

To move a maintainer to emeritus status:

1. Record the request or the agreement in an issue.
2. Remove access using the [access offboarding](#access-offboarding) checklist.
3. Replace the active entry with an emeritus entry that records the handle, the
   period of service, and the areas of contribution.
4. Keep the person in the security rota only if they explicitly accept the
   obligation; otherwise remove them at the next handoff.

Emeritus maintainers may return through the normal contributor ladder.

## Removing a maintainer

Removal for cause follows the process in
[`GOVERNANCE.md`](GOVERNANCE.md#removal-for-cause). The organization owner may
suspend access immediately when there is an active security or safety risk. A
remaining maintainer and the organization owner review the decision after the
risk is contained.

Record the outcome and the effective date in this file. Do not publish private
personal information.

## Access offboarding

The organization owner and a remaining maintainer complete this checklist when
a maintainer leaves, becomes emeritus, or is removed:

- [ ] Remove organization, repository, and team roles.
- [ ] Remove the person from protected-environment approval lists.
- [ ] Rotate or replace release signing keys and repository-publishing
      credentials per
      [`docs/governance/KEY_ROTATION.md`](docs/governance/KEY_ROTATION.md).
- [ ] Revoke package-repository, cloud, and self-hosted-runner access granted by
      the project.
- [ ] Update `.github/CODEOWNERS` so every path has an active owner.
- [ ] Move the entry to emeritus status or remove it from active membership.
- [ ] Review recent repository, environment, and release activity for
      unauthorized changes.
- [ ] Record the offboarding date and the access removed.

If the departing maintainer was one of two key custodians, the remaining
custodian rotates the affected keys even when there is no evidence of
compromise.

## Changes to this file

Submit a pull request. The change requires approval from the current
maintainers. Membership changes and `.github/CODEOWNERS` changes belong in the
same pull request so review enforcement never points at a former maintainer.

## Related documents

- [`GOVERNANCE.md`](GOVERNANCE.md) — roles, decision-making, contributor ladder,
  and bus-factor mitigations
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — contribution workflow and high-risk
  change guidance
- [`SECURITY.md`](SECURITY.md) — reporting channel, severity, and disclosure
- [`docs/governance/SECURITY_TRIAGE_ROTA.md`](docs/governance/SECURITY_TRIAGE_ROTA.md)
  — weekly primary and backup assignments
- [`docs/governance/KEY_ROTATION.md`](docs/governance/KEY_ROTATION.md) — signing
  key custody and rotation
- [`docs/governance/BRANCH_PROTECTION.md`](docs/governance/BRANCH_PROTECTION.md)
  — branch, tag, and review rules
