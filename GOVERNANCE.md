# Project governance

Last reviewed: 2026-11-03. Maintainer membership is recorded in
[`MAINTAINERS.md`](MAINTAINERS.md).

This document describes how the CHV project makes decisions, who has authority
to merge and release, how security reports are handled, and how contributors
move through the contributor ladder. It applies to the repository, its
releases, and its security response. It does not replace the
[`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md).

## Roles

| Role | Who | Responsibilities | Repository access |
| ---- | --- | ---------------- | ----------------- |
| Contributor | Anyone who participates in the project | Open issues, propose changes, submit pull requests, review code, write documentation, and test releases | No write access is implied |
| Reviewer | A contributor recognized through the [contributor ladder](#contributor-ladder) | Review changes in an area, identify risk, and give actionable feedback. A reviewer has no merge or release authority unless they are also a maintainer | No write access is required |
| Maintainer | A person listed in [`MAINTAINERS.md`](MAINTAINERS.md) | Review and merge changes, sponsor contributors, triage security reports, approve releases, and keep ownership and governance records current | Maintain or admin access, as granted by the organization owner |
| Admin / owner | A GitHub organization owner or repository administrator | Manage repository settings, rulesets, teams, environments, secrets, and access. Act on governance decisions and break-glass requests. Do not decide technical disputes alone | Organization owner or repository admin |

The current maintainers are also repository administrators. That is an access
fact, not a separate technical authority: technical decisions follow the model
below.

## Decision-making

### Routine changes

Routine changes use lazy consensus. A contributor opens an issue or pull
request and states the problem, the proposed change, and the evidence that the
change works. A maintainer merges the change when:

- the required CI and security checks pass;
- the pull request has the approval required by the `protect-main` ruleset;
- a code owner has reviewed the changed paths; and
- no maintainer has raised a blocking objection.

Lazy consensus does not bypass branch protection. The current rules require at
least one approval, a code-owner review, signed commits, resolved
conversations, and the required status checks. See
[`docs/governance/BRANCH_PROTECTION.md`](docs/governance/BRANCH_PROTECTION.md).

### Architecture, API, and security changes

The following changes require a written architecture decision record (ADR) in
[`docs/specs/adr/`](docs/specs/adr/) and approval from the maintainers:

- architecture boundaries or component responsibilities;
- public APIs, protobuf contracts, or persisted data formats;
- authentication, authorization, mTLS, enrollment, or secret handling;
- storage or network isolation, sandboxing, or privilege boundaries;
- database migrations or destructive upgrade behavior; and
- the release or artifact-signing model.

The ADR records the context, the decision, the alternatives considered, and the
consequences. Security-sensitive changes also follow the high-risk review
guidance in [`CONTRIBUTING.md`](CONTRIBUTING.md#high-risk-changes) and the
reporting process in [`SECURITY.md`](SECURITY.md).

### Contested decisions and tie-break

Maintainers decide contested technical questions. Reviewers, contributors, and
administrators do not override a maintainer decision by virtue of access alone.

- Record the disagreement and the final rationale on the issue, pull request,
  or ADR.
- When the current maintainers split, the change does not land until they reach
  agreement or an ADR records the chosen path.
- An administrator may facilitate the discussion and is responsible for
  applying access or ruleset changes. The administrator does not break a
  technical tie.
- A maintainer with a conflict of interest recuses from the decision under the
  [conflict-of-interest](#conflict-of-interest) process.

### Changes to this document

Change this document by pull request. The change requires approval from the
maintainers and must keep the linked governance records consistent.

## Release authority

Only maintainers may create or push version tags. The `protect-tags` ruleset
restricts creation, update, and deletion of `v*` tags. The release workflow
requires approval through the protected `rc` or `production` GitHub
environment before it publishes artifacts.

- The release procedure is documented in
  [`docs/release/release-process.md`](docs/release/release-process.md).
- The release pipeline, jobs, and gates are documented in
  [`docs/release/PIPELINE.md`](docs/release/PIPELINE.md).
- Artifact verification is documented in
  [`docs/release/verify-release-artifacts.md`](docs/release/verify-release-artifacts.md).

A release must pass the quality gate, the supply-chain preflight, package smoke
and lifecycle tests, and the protected signing job before publication. A
maintainer may not use administrator access to bypass a failed gate. If signing
is enabled with `CHV_REQUIRE_SIGNING=1`, a missing or invalid signature blocks
the release.

Release signing key custody, rotation, and offboarding are documented in
[`docs/governance/KEY_ROTATION.md`](docs/governance/KEY_ROTATION.md). The
current signing status is recorded in
[`docs/release/verify-release-artifacts.md`](docs/release/verify-release-artifacts.md).

## Security response

Security reports use GitHub private vulnerability reporting. Do not open a
public issue for a suspected vulnerability. The reporting channel, scope,
severity levels, fix targets, and 90-day coordinated disclosure policy are in
[`SECURITY.md`](SECURITY.md).

The weekly primary and backup maintainer assignments are in
[`docs/governance/SECURITY_TRIAGE_ROTA.md`](docs/governance/SECURITY_TRIAGE_ROTA.md).
The rota schedules the maintainers; it does not create new response targets.
Maintainers acknowledge reports within 48 hours and complete initial triage
within 7 days, as stated in `SECURITY.md`.

If a report concerns a maintainer, that maintainer recuses. The other
maintainer handles the report. If both maintainers are conflicted or
unavailable, the organization owner applies the break-glass process in
[Bus-factor mitigations](#bus-factor-mitigations) and appoints an uninvolved
reviewer.

## Conflict of interest

A maintainer, reviewer, or administrator discloses a conflict of interest in
the issue or pull request and recuses from the related review, merge, release,
or security decision. Examples include:

- a financial or employment interest in a vendor, customer, or competitor;
- a personal relationship with the reporter or the author;
- responsibility for the code or decision under review; and
- a security report that names the person or their work.

Record the disclosure and the recusal in the issue or advisory. If a recusal
leaves no eligible maintainer, the organization owner appoints an uninvolved
reviewer under the break-glass process.

## Maintainer lifecycle

### Adding a maintainer

Maintainers are added through the [contributor ladder](#contributor-ladder).
The nomination records the candidate's contributions, review history, and
areas of demonstrated competence. The current maintainers must approve the
nomination unanimously. The organization owner then grants the required
repository access.

On acceptance:

1. Add the person to [`MAINTAINERS.md`](MAINTAINERS.md).
2. Update [`.github/CODEOWNERS`](.github/CODEOWNERS) in the same pull request
   when the person's ownership scope changes.
3. Complete the onboarding checks: two-factor authentication, signed commits,
   review of the release process, security triage shadowing, and key-custody
   briefing when the person will hold signing keys.
4. Add the person to the security triage rota at the next handoff.

### Emeritus status

A maintainer may become an emeritus maintainer on request, or by agreement with
the remaining maintainers after a sustained period without review, release, or
security-triage activity. Emeritus status:

- removes write, merge, release, environment, and key-custody access;
- removes review and response obligations;
- keeps the person's name in `MAINTAINERS.md` with emeritus status and the
  period of service; and
- allows a return through the normal contributor ladder.

### Removal for cause

Removal for cause covers a security risk, a Code of Conduct violation, bad-faith
participation, or repeated refusal to follow the security or review process.

1. A maintainer or the organization owner reports the concern privately to the
   other maintainers and the organization owner.
2. The affected person receives the concern in writing and an opportunity to
   respond.
3. An uninvolved maintainer and the organization owner decide the outcome. If
   both maintainers are involved, the organization owner decides.
4. If there is an active security or safety risk, the organization owner may
   suspend access immediately. The decision is reviewed after the risk is
   contained.
5. Record the outcome and the effective date in the maintainer record. Do not
   publish private personal information.

Removal does not erase a person's contributions. The record distinguishes
removal for cause from emeritus status.

### Access offboarding

The organization owner and a remaining maintainer complete these steps when a
maintainer leaves, becomes emeritus, or is removed:

- [ ] Remove organization, repository, and team roles.
- [ ] Remove the person from `MAINTAINERS.md` active membership and record the
      new status.
- [ ] Update [`.github/CODEOWNERS`](.github/CODEOWNERS) so every path still has
      an active owner.
- [ ] Remove the person from protected-environment approval lists.
- [ ] Rotate or replace release signing keys and repository-publishing
      credentials according to
      [`docs/governance/KEY_ROTATION.md`](docs/governance/KEY_ROTATION.md).
- [ ] Revoke package-repository, cloud, and self-hosted-runner access that the
      project granted.
- [ ] Review recent repository, environment, and release activity for
      unauthorized changes.
- [ ] Record the offboarding date and the access removed.

Do not delete the person's GitHub account or rewrite history. Access removal is
sufficient.

## Contributor ladder

The ladder is a path from participation to maintainership. It is based on
demonstrated work, judgement, and reliability. It is not based on tenure or
employer.

| Level | Criteria | Authority | How to join |
| ----- | -------- | --------- | ----------- |
| Contributor | At least one accepted contribution, or sustained issue triage, documentation, testing, or community support | Open issues and pull requests, review changes, and participate in design discussion | Start contributing; no nomination is required |
| Reviewer | Sustained, constructive participation in one or more areas; at least three substantive merged contributions or equivalent triage/documentation work; at least two substantive reviews of other contributors' changes; no unresolved Code of Conduct or security concern | Review changes in the recognized area and provide blocking or non-blocking feedback. No merge, release, or access authority | A maintainer nominates the contributor in an issue. The current maintainers approve. Reviewer status is recorded in the nomination issue and in [`MAINTAINERS.md`](MAINTAINERS.md#reviewers) |
| Maintainer | Demonstrated reviewer service across multiple changes and at least one release or security-triage cycle; ability to run CI, interpret failures, and follow the release and rollback process; sound judgement on security-sensitive changes; willingness to accept the review and response obligations | Merge changes, sponsor contributors, triage security reports, approve releases, and hold key custody when designated | A current maintainer nominates the candidate. The current maintainers approve unanimously. The organization owner grants access and the person is added to `MAINTAINERS.md` and `.github/CODEOWNERS` |

### Nomination and approval process

1. Open an issue titled `Maintainer nomination: @<handle>` or
   `Reviewer nomination: @<handle>`.
2. State the areas, the evidence, and the expected responsibilities. Link the
   merged contributions and reviews.
3. A current maintainer seconds the nomination.
4. The current maintainers approve in writing. For a maintainer nomination,
   approval must be unanimous. Resolve objections before granting the role.
5. The organization owner grants access. Do not grant access before the
   nomination is approved.
6. Update [`MAINTAINERS.md`](MAINTAINERS.md) and, for a maintainer, the
   ownership records in the same pull request.
7. Complete the onboarding steps in [Adding a maintainer](#adding-a-maintainer).

A nomination that fails does not prevent a later nomination. Record the reasons
without personal criticism.

## Bus-factor mitigations

### Coverage of critical areas

At least two maintainers cover every critical area. The current ownership map
lists both maintainers on every path in `.github/CODEOWNERS`, so no critical
area has a single owner.

| Critical area | Maintainers covering it | Evidence |
| ------------- | ----------------------- | -------- |
| Repository administration, CI, and security policy | @zoorpha, @senolcolak | `.github/CODEOWNERS` admin section; both are repository administrators |
| Rust backend, control plane, and protobuf contracts | @zoorpha, @senolcolak | `.github/CODEOWNERS` Rust and migration sections |
| Web UI and backend-for-frontend (BFF) | @zoorpha, @senolcolak | `.github/CODEOWNERS` `/ui/` section |
| Deployment, packaging, install, and release | @zoorpha, @senolcolak | `.github/CODEOWNERS` deployment and release sections |
| Architecture, ADRs, component specs, and migrations | @zoorpha, @senolcolak | `.github/CODEOWNERS` architecture and migration sections |

This table records current coverage. It is not a claim that the project has
enough maintainers. Two maintainers remain a continuity risk; the ladder and
the security rota are the mechanisms for adding coverage.

### Review coverage

- Every pull request needs the approval and code-owner review required by the
  `protect-main` ruleset.
- Because both maintainers are code owners on every path, a maintainer cannot
  rely on their own approval. The other maintainer must review the change.
- A high-risk change, as defined in
  [`CONTRIBUTING.md`](CONTRIBUTING.md#high-risk-changes), should receive a
  review from a maintainer who did not author it. If both maintainers authored
  the change, ask a reviewer from the contributor ladder for an independent
  review before merge.
- Maintainers do not use administrator bypass to merge without the required
  review. The ruleset has no bypass actors.
- Security reports take priority over feature review when the rota primary is
  handling an active report.

### Break-glass access

Break-glass access is a temporary continuity measure. It does not create a
maintainer and it does not change this governance model.

Use break-glass only when neither maintainer can review, approve, release, or
respond within the `SECURITY.md` timeline, or when both maintainers are
conflicted or locked out.

1. Open an incident issue, or a private advisory when the trigger is a security
   report. State the trigger, the affected decisions, and the time limit.
2. The organization owner confirms that no eligible maintainer is available or
   that both are conflicted.
3. Prefer a temporary maintain or write grant to a named reviewer over
   disabling branch protection. Add the person to the relevant ownership path
   through a reviewed pull request when possible.
4. If the owner must change a ruleset to restore access, record the before and
   after state and restore the ruleset when the incident ends.
5. Record every action in the incident. At the end of the incident, remove the
   temporary access and restore the ruleset.
6. A returning maintainer reviews any merge or release made under break-glass.
   Rotate shared secrets if the temporary access included environment or
   signing credentials.

If the break-glass action changes maintainer membership, follow the
[maintainer lifecycle](#maintainer-lifecycle) instead of treating it as a
temporary grant.

## Related documents

- [`MAINTAINERS.md`](MAINTAINERS.md) — current maintainers, reviewers, review
  expectations, and offboarding
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — development workflow, high-risk change
  guidance, and the pull request process
- [`SECURITY.md`](SECURITY.md) — vulnerability reporting, scope, severity, and
  disclosure
- [`docs/governance/SECURITY_TRIAGE_ROTA.md`](docs/governance/SECURITY_TRIAGE_ROTA.md)
  — weekly security triage assignments and handoff
- [`docs/governance/KEY_ROTATION.md`](docs/governance/KEY_ROTATION.md) — release
  signing key custody and rotation
- [`docs/governance/BRANCH_PROTECTION.md`](docs/governance/BRANCH_PROTECTION.md)
  — branch, tag, and review enforcement
- [`docs/governance/REPOSITORY_HARDENING.md`](docs/governance/REPOSITORY_HARDENING.md)
  — repository hardening checklist
- [`docs/governance/CODEOWNERS_TEAMS.md`](docs/governance/CODEOWNERS_TEAMS.md)
  — planned team migration for `.github/CODEOWNERS`
- [`docs/release/PIPELINE.md`](docs/release/PIPELINE.md) — release pipeline and
  gates
- [`docs/specs/adr/`](docs/specs/adr/) — architecture decision records

## Change history

| Date | Change |
| ---- | ------ |
| 2026-11-03 | Initial governance model, contributor ladder, and bus-factor plan (#665) |
