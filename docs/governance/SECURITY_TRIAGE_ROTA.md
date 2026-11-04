# Security triage rota

Last updated: 2026-11-03. The maintainers listed here are the members recorded
in [`MAINTAINERS.md`](../../MAINTAINERS.md).

This rota assigns a primary and a backup maintainer for private vulnerability
reports. It schedules the work; it does not change the reporting channel,
severity levels, fix targets, or disclosure policy in
[`SECURITY.md`](../../SECURITY.md).

## Rota model

- The rotation runs weekly, from Monday 00:00 UTC to the following Monday
  00:00 UTC.
- The primary maintainer owns acknowledgement, triage, and coordination for the
  week.
- The backup covers the primary when the primary is unavailable. The backup
  also reviews any decision that the primary identifies as a conflict of
  interest.
- Both maintainers have repository administrator access and can open the
  private advisory area. No other people are in the rota.
- Update the table during each handoff. Do not leave a past week as the current
  assignment.

## Current rotation

| Week starting (UTC) | Primary | Backup |
| ------------------- | ------- | ------ |
| 2026-11-02 | @zoorpha | @senolcolak |
| 2026-11-09 | @senolcolak | @zoorpha |
| 2026-11-16 | @zoorpha | @senolcolak |
| 2026-11-23 | @senolcolak | @zoorpha |

The table is a four-week schedule. Extend it at the last handoff so the next
primary and backup are always assigned. Add only people who are active
maintainers in `MAINTAINERS.md`.

## Weekly handoff

The outgoing primary and the incoming primary complete these steps at the
start of the week:

1. Review open private advisories and any security-labelled issues, including
   reports waiting for acknowledgement or triage.
2. Confirm which reports have an active fix, a planned release, or a disclosure
   deadline in the coming week.
3. Brief the incoming primary on active reports, affected components,
   reproduction status, and coordination with reporters.
4. Confirm that the incoming primary can access the repository Security tab,
   private advisories, the release environments, and the maintainer-only
   records needed for the role.
5. Update the rotation table and record a short handoff confirmation. Keep
   vulnerability details in the private advisory, not in a public issue.
6. The outgoing primary remains available as an additional reviewer unless
   they are unavailable or conflicted.

## Triage procedure

Follow [`SECURITY.md`](../../SECURITY.md) for scope, severity definitions, and
the reporting channel. The steps below add the operational sequence.

1. **Acknowledge within 48 hours.** Reply in the private advisory. Confirm the
   report was received and state the next step. Do not open a public issue and
   do not move the report to a public tracker.
2. **Complete initial triage within 7 days.** Confirm the affected component
   and version, reproduce where feasible, identify the attack preconditions,
   and decide whether the report is in scope.
3. **Assign a severity.** Use the four-tier scale in `SECURITY.md` and record
   the rationale in the advisory. Base the decision on demonstrated impact, not
   on the reporter's label.
4. **Apply the fix target.** Use the table below. If the team cannot meet the
   target, tell the reporter and record the reason in the advisory.
5. **Open or update the private advisory.** Add the maintainers who need to
   work on the fix. Use a private fork or private branch for exploit details and
   tests. Do not commit proof-of-concept material to `main`.
6. **Coordinate the fix and release.** Follow
   [`docs/release/release-process.md`](../release/release-process.md). A
   security release still passes the release workflow gates. Do not publish
   artifact details before the advisory is ready.
7. **Prepare disclosure.** At 90 days from receipt, or earlier by mutual
   agreement, publish the advisory and credit the reporter with consent. Update
   the Hall of Fame section in `SECURITY.md` when an advisory is published.
8. **Close the loop.** Record the fix version, the advisory link, and any
   follow-up work in the advisory before closing it.

## Severity and fix targets

| Severity | Fix target | Triage action |
| -------- | ---------- | ------------- |
| Critical | 7 days | Notify both maintainers immediately. Treat as release-blocking. Consider an out-of-band patch release. |
| High | 30 days | Assign a fix owner, schedule the release, and keep the reporter informed. |
| Medium | 60 days | Schedule the fix in the normal release flow; record any delay. |
| Low | Best effort | Track the report, batch the fix when practical, and tell the reporter the expected handling. |

These targets come from `SECURITY.md`. They are guidance, not contracts.
Complex fixes may take longer; trivial fixes should ship sooner.

## Private advisory handling

- Use GitHub private vulnerability reporting. The reporting link is in
  `SECURITY.md`.
- Keep the reporter, the maintainers working on the fix, and any explicitly
  invited subject-matter experts in the advisory. Do not add unrelated people.
- Do not paste secrets, customer data, or personal data into the advisory.
  Reference the minimum evidence needed to reproduce the issue.
- Do not discuss an undisclosed vulnerability in a public issue, pull request,
  commit message, or chat that is visible to the public.
- If a fix must be developed in a private fork, keep the same access list as
  the advisory and delete the fork after the advisory is published.
- If the report is out of scope, explain why and point the reporter to the
  correct channel in `SECURITY.md`. Do not dismiss a report without a written
  reason.

## Escalation

| Situation | Action |
| --------- | ------ |
| Primary is unavailable | Backup assumes the primary role and updates the rotation table. |
| Primary is conflicted | Backup handles the report. Record the recusal in the advisory. |
| Both maintainers are conflicted | The organization owner appoints an uninvolved reviewer under the break-glass process in `GOVERNANCE.md`. See [`GOVERNANCE.md#organization-owner-residual`](../../GOVERNANCE.md#organization-owner-residual): the sole owner is also a maintainer, so if they are conflicted or unavailable use the GitHub Support organization-recovery path. |
| Both maintainers are unavailable | The organization owner applies the break-glass process in `GOVERNANCE.md` and restores normal coverage at handoff. Same residual: use the GitHub Support organization-recovery path when the sole owner cannot act. |
| Active exploitation is reported | Treat the report as Critical, notify both maintainers immediately, and consider an emergency patch release. Do not wait for the weekly handoff. |
| The report concerns a maintainer | The other maintainer handles it. If both are named, the organization owner handles the escalation. |
| The issue is in a third-party dependency or upstream component | Follow the out-of-scope routing table in `SECURITY.md`; track the upstream fix and the CHV update. |

## Related documents

- [`SECURITY.md`](../../SECURITY.md) — reporting channel, scope, severity, and
  disclosure policy
- [`MAINTAINERS.md`](../../MAINTAINERS.md) — current maintainers and review
  expectations
- [`GOVERNANCE.md`](../../GOVERNANCE.md) — conflict of interest, removal, and
  break-glass process
- [`docs/governance/KEY_ROTATION.md`](KEY_ROTATION.md) — signing key rotation
  during a security incident
- [`docs/release/release-process.md`](../release/release-process.md) — release
  procedure for a security fix
- [`docs/release/PIPELINE.md`](../release/PIPELINE.md) — release gates and
  signing job
