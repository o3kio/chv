# Summary

Describe the problem and the change. Include the user-visible behavior and the
scope of the change.

## Related issue

Use `Part of #<issue>` for work that advances an issue, or `Closes #<issue>`
when this pull request completes it.

Part of #

## Testing and evidence

Describe how you verified the change. Link tests, qualification runs, or an
evidence document under `docs/evidence/`. For high-risk changes, follow the
evidence guidance in
[`CONTRIBUTING.md`](https://github.com/o3kio/chv/blob/main/CONTRIBUTING.md#high-risk-changes).

## Risk and rollback

State the production risk and how to roll the change back if it fails. Write
"None" only when the change is genuinely low risk.

## Checklist

- [ ] Tests were added or updated, or existing tests and evidence are linked
      above.
- [ ] The change passes the repository checks that apply to it, or the reason
      they could not run is stated above.
- [ ] High-risk behavior is described with evidence and a rollback plan, per
      `CONTRIBUTING.md`.
- [ ] `CHANGELOG.md` is updated for a user-visible change.
- [ ] Documentation, specifications, or an ADR are updated when behavior,
      architecture, an API, or an operational procedure changes.
- [ ] The security impact is assessed. Authentication, authorization, mTLS,
      secrets, storage isolation, network isolation, and privilege changes are
      called out explicitly.
- [ ] The branch contains no secrets, tokens, customer data, or personal data.
- [ ] Commits are signed. `protect-main` requires signed commits.
- [ ] The related issue uses `Part of #<issue>` or `Closes #<issue>` wording.

## Screenshots or UI evidence

For UI changes, attach screenshots or a short recording. Remove secrets and
personal data.
