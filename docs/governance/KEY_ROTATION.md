# Release signing key rotation

Last reviewed: 2026-11-03.

This document defines custody, rotation, and offboarding for the keys used to
sign CHV release artifacts and package-repository metadata. It cites the actual
workflow secrets and signing mechanism; it does not invent a key that is not
configured.

> **Current status:** release artifact signing is not fully configured. The
> `sign` job in `.github/workflows/release.yml` runs, but it degrades to an
> unsigned release when no key is present. The current gap is recorded in
> [`docs/release/verify-release-artifacts.md`](../release/verify-release-artifacts.md).
> This document is the procedure for provisioning and rotating the keys when
> they are enabled.

## Key types and pipeline wiring

The release workflow signs checksum files, not the binaries or packages
directly. The `sign` job downloads only the checksum sidecars, then calls
[`scripts/release/sign-checksums.sh`](../../scripts/release/sign-checksums.sh).
The script supports GPG and key-based cosign signing. It does not use cosign
keyless signing or an OIDC identity.

| Purpose | Mechanism | Secret or variable | Used by | Output |
| ------- | --------- | ------------------ | ------- | ------ |
| Release checksum signing | GPG detached, ASCII-armored signature | `CHV_RELEASE_GPG_KEY` (ASCII-armored private key), `CHV_RELEASE_GPG_PASSPHRASE` (optional passphrase) | `release.yml` `sign` job; `scripts/release/sign-checksums.sh` | `dist/packages/SHA256SUMS.sig` and `dist/chv-<version>-linux-amd64.tar.gz.sha256.sig` |
| Release checksum signing | Cosign key-based blob signature (`cosign sign-blob --key`, `--tlog-upload=false`) | `CHV_RELEASE_COSIGN_KEY` (private key), `CHV_RELEASE_COSIGN_PASSWORD` (optional password) | `release.yml` `sign` job; `scripts/release/sign-checksums.sh` | `SHA256SUMS.cosign.sig` and `chv-<version>-linux-amd64.tar.gz.sha256.cosign.sig` |
| Fail-closed release gate | Repository variable, not a secret | `CHV_REQUIRE_SIGNING=1` | `release.yml` `sign` job; `scripts/release/sign-checksums.sh` | Fails the release when signing is missing or invalid |
| Package repository metadata | GPG repository key | `CHV_REPO_GPG_KEY`, `CHV_REPO_GPG_PASSPHRASE` | `release.yml` `publish-repo` job; `scripts/publish/publish-repo.sh` | Signed apt and yum repository metadata |

The release signing secrets and the package-repository key are separate.
Rotating one does not rotate the other. The release signing job runs in the
`rc` or `production` environment (currently without a required-reviewer
protection rule; see [`docs/release/release-process.md`](../release/release-process.md)),
and the package-repository job runs
in `rc-repo` or `production-repo`. Store each secret in its protected
environment according to the release process.

The `sign` job has read-only `contents` permission. It receives only the
checksum files, never the build artifacts. Do not add build or package
artifacts to that job.

## Key custodians

The pipeline supports one active GPG release key and one active cosign release
key at a time. No such key is configured today. Both maintainers are designated
custodians so that neither is a single point of failure. A custodian must be an
active maintainer in [`MAINTAINERS.md`](../../MAINTAINERS.md).

| Custodian | GitHub | Release GPG | Release cosign | Package-repository GPG | Current key status |
| --------- | ------ | ----------- | -------------- | ---------------------- | ------------------ |
| Primary custodian | @zoorpha | Designated | Designated | Designated | No release or package-repository signing key is configured as of 2026-11-03 |
| Backup custodian | @senolcolak | Designated | Designated | Designated | No release or package-repository signing key is configured as of 2026-11-03 |

"Designated" means the person is expected to hold a recovery copy in the
approved secret store and to be able to perform rotation. It does not mean that
key material is committed to the repository or stored in a personal account.

## Rotation cadence

- **Planned rotation:** every 12 months for the release GPG key and the release
  cosign key. Review the package-repository key on the same schedule.
- **Custodian change:** rotate when a custodian leaves, becomes emeritus, or
  changes role.
- **Suspected compromise:** rotate immediately under the emergency procedure
  below.
- **Secret exposure:** rotate immediately if a key, passphrase, environment
  secret, or artifact containing key material is exposed.
- **Policy or algorithm change:** rotate when the signing algorithm, key size,
  or verification policy changes.
- **Signing enablement:** provision the first key before setting
  `CHV_REQUIRE_SIGNING=1` for stable releases.

Do not reuse a key across the release artifact and package-repository roles.

## Normal rotation procedure

Perform the rotation with both custodians available. Keep key material out of
issues, pull requests, chat, and logs.

1. Open a maintainer-only tracking issue. Do not include private key material,
   passphrases, or recovery shares.
2. Generate the new key on a trusted machine:
   - GPG: generate a signing-capable key with the project release identity.
     Export the public key and an ASCII-armored private key.
   - Cosign: generate a key pair with `cosign generate-key-pair`. Keep the
     private key and password separate.
3. Store the new private key and passphrase in the protected `rc` and
   `production` environment secrets:
   - `CHV_RELEASE_GPG_KEY` and `CHV_RELEASE_GPG_PASSPHRASE`; or
   - `CHV_RELEASE_COSIGN_KEY` and `CHV_RELEASE_COSIGN_PASSWORD`.
   Do not put key material in repository secrets that are visible to workflows
   outside the protected environments.
4. Keep the old key available for verification until the new key has produced a
   verified release. Do not delete the old public key immediately.
5. Publish the new public key and fingerprint in
   [`docs/release/verify-release-artifacts.md`](../release/verify-release-artifacts.md)
   and in the release notes for the first release signed with it. Publish the
   fingerprint in a channel that users already trust.
6. Run a release candidate signed with the new key, or a release workflow run
   that exercises the `sign` job. Set the repository variable
   `CHV_REQUIRE_SIGNING=1` for the test so a missing signature fails the job.
7. Verify the outputs before enabling a stable release:
   - `gpg --verify SHA256SUMS.sig SHA256SUMS`
   - `cosign verify-blob --key <public-key> --signature SHA256SUMS.cosign.sig SHA256SUMS`
   - Confirm `SIGNING-STATUS.txt` lists the expected signature files.
   - Confirm the release job downloads the signature artifact and publishes the
     expected `.sig` files.
8. After the first successful stable release with the new key, revoke the old
   key when it is no longer needed to verify supported releases. Publish the
   revocation certificate for the old GPG key.
9. Record the rotation date, key type, fingerprint, reason, and custodians in
   the [rotation log](#rotation-log). Do not record private key material.

## Emergency rotation

Use this procedure when a key is lost, stolen, exposed, or suspected of
compromise. Treat the event as a security incident and follow
[`SECURITY.md`](../../SECURITY.md) for disclosure.

1. Stop release publication. Do not publish an unsigned or old-key-signed
   release after a suspected compromise.
2. Revoke the affected GPG key and publish the revocation certificate. For
   cosign, remove the compromised key from the environment and stop using its
   signatures; cosign key-based signatures do not have a revocation list.
3. Remove or replace `CHV_RELEASE_GPG_KEY`, `CHV_RELEASE_GPG_PASSPHRASE`,
   `CHV_RELEASE_COSIGN_KEY`, and `CHV_RELEASE_COSIGN_PASSWORD` in both the
   `rc` and `production` environments.
4. Set or keep `CHV_REQUIRE_SIGNING=1` so a missing replacement key fails the
   `sign` job.
5. Generate the replacement key, store it, and publish the new public key
   fingerprint.
6. Verify a signed release candidate before publishing a stable release.
7. Assess existing releases and published package repositories. If users may
   have consumed a compromised artifact or repository metadata, publish an
   advisory and follow the coordinated disclosure process.
8. Rotate the package-repository key and credentials if the compromise could
   have reached the `publish-repo` environment.
9. Record the incident, the rotation, and the affected releases.

## Key-holder offboarding

When a custodian leaves, becomes emeritus, or is removed:

1. Remove the person from protected-environment approval lists.
2. Replace the environment secrets that the person could access. Rotate the
   release GPG and cosign keys and the package-repository key even when there is
   no evidence of compromise.
3. Remove the person from the key-custodian table and update
   [`MAINTAINERS.md`](../../MAINTAINERS.md).
4. Confirm that at least one active maintainer remains as custodian. If not,
   appoint a replacement under the break-glass process in
   [`GOVERNANCE.md`](../../GOVERNANCE.md), then complete the rotation.
5. Review release and environment audit logs for access after the person's last
   approved activity.
6. Record the offboarding and rotation dates.

## Public-key distribution

- Publish the GPG public key, fingerprint, and revocation certificate location
  in [`docs/release/verify-release-artifacts.md`](../release/verify-release-artifacts.md).
- Publish the cosign public key and its fingerprint in the same document.
- Repeat the fingerprint and verification commands in the release notes for
  releases signed with a new key.
- Never publish a private key, passphrase, recovery share, or a screenshot that
  contains them.
- Do not claim that a release is signed when the `sign` job reported
  `SIGNING NOT CONFIGURED`. The verification document and the release assets
  must match the actual signing status.

## Verification steps

Users and maintainers verify a signed release with the commands in
[`docs/release/verify-release-artifacts.md`](../release/verify-release-artifacts.md).
For a rotation, verify all of the following:

- [ ] The new public key fingerprint matches the fingerprint published in the
      verification document and release notes.
- [ ] `gpg --verify SHA256SUMS.sig SHA256SUMS` reports a good signature from
      the expected key.
- [ ] `cosign verify-blob --key <public-key> --signature SHA256SUMS.cosign.sig SHA256SUMS`
      succeeds when cosign signing is enabled.
- [ ] The `sign` job artifact contains the expected `.sig` files and
      `SIGNING-STATUS.txt`.
- [ ] The release job verifies the artifacts against their checksums before
      publication.
- [ ] `CHV_REQUIRE_SIGNING=1` is set only after a successful signed release
      candidate, and an unsigned test fails closed.
- [ ] The old key is revoked and the revocation certificate is published after
      the new key is in use.

## Rotation log

| Date | Key type | Fingerprint | Reason | Custodians |
| ---- | -------- | ----------- | ------ | ---------- |
| — | — | — | No rotation recorded; no release signing key is configured as of 2026-11-03 | @zoorpha, @senolcolak |

Add one row per rotation. Do not include private key material or passphrases.

## Related documents

- [`SECURITY.md`](../../SECURITY.md) — security reporting and disclosure
- [`GOVERNANCE.md`](../../GOVERNANCE.md) — release authority and break-glass
  process
- [`MAINTAINERS.md`](../../MAINTAINERS.md) — current maintainers and access
  offboarding
- [`docs/governance/SECURITY_TRIAGE_ROTA.md`](SECURITY_TRIAGE_ROTA.md) — security
  triage assignments
- [`docs/release/PIPELINE.md`](../release/PIPELINE.md) — release pipeline,
  signing job, and environments
- [`docs/release/release-process.md`](../release/release-process.md) — release
  procedure and required environments
- [`docs/release/verify-release-artifacts.md`](../release/verify-release-artifacts.md)
  — artifact verification and current signing status
- [`scripts/release/sign-checksums.sh`](../../scripts/release/sign-checksums.sh)
  — signing implementation
