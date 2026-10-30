# Security Policy

CHV is a virtualization platform that runs untrusted workloads (guest VMs) on shared infrastructure. We take security reports seriously and investigate every credible report.

## Supported Versions

CHV is in the early-to-MVP phase. Only the latest minor release line receives security updates. Older lines are not patched; users are expected to upgrade to a supported version.

| Version | Supported          | Notes                                |
| ------- | ------------------ | ------------------------------------ |
| 0.2.x   | :white_check_mark: | Current supported line               |
| < 0.2   | :x:                | Pre-release; please upgrade to 0.2.x |

The single source of truth for the current release is the [`VERSION`](VERSION) file at the repository root.

## Reporting a Vulnerability

Please **do not** open a public GitHub issue for security reports. Use GitHub's private vulnerability reporting instead:

**GitHub Security Advisories (the reporting channel):**

Open a private advisory via the repository's **Security → Advisories → Report a vulnerability** page (direct link: <https://github.com/o3kio/chv/security/advisories/new>). This creates a private channel between you and the maintainers, and lets us collaborate on a fix and CVE assignment if applicable.

> This is the project's only reporting channel. It is private, does not require
> an email mailbox, and is verified enabled on the repository. Do not send
> reports to individual maintainers' personal addresses.

**What to include in a report:**

- A clear description of the issue and the affected component (e.g., `chv-controlplane`, `chv-agent`, `chv-stord`, `chv-nwd`, the backend-for-frontend (BFF), UI)
- The CHV version (`chvctl version` output is ideal) and deployment shape (single-node dev install, multi-node, distro)
- Steps to reproduce, including any minimal proof-of-concept
- Impact assessment from your perspective (what can an attacker do?)
- Whether the issue is already public anywhere (mailing list, blog, CVE)

### Process and Timeline

| Stage              | Target                                                                |
| ------------------ | --------------------------------------------------------------------- |
| Acknowledgement    | Within **48 hours** of receipt                                        |
| Initial triage     | Within **7 days** — severity classification, scope confirmation       |
| Fix target         | See [Severity Classification](#severity-classification) below         |
| Coordinated public disclosure | **90 days** from report, or earlier by mutual agreement     |

We will keep you informed at each stage. If we cannot meet a target, we will tell you and explain why.

## Disclosure Timeline

CHV follows a **90-day coordinated disclosure** policy, aligned with industry norms (Project Zero, distros@):

- Day 0: Report received and acknowledged
- Day 1–7: Triage and severity classification
- Day 7–N: Fix development and review (N depends on severity, see below)
- Fix released (patch version, advisory published)
- Day 90 (or earlier by mutual agreement): Public disclosure of the advisory and reporter credit (with consent)

If a fix is non-trivial and the maintainers and reporter agree, the disclosure window may be extended. Extensions are granted at maintainer discretion and recorded in the advisory.

## Scope

### In scope

Vulnerabilities in any of the following components, as built from this repository:

- **`chv-controlplane`** — orchestration, control-plane HTTP/gRPC, BFF, enrollment
- **`chv-agent`** — node agent, VM lifecycle, Cloud Hypervisor (the VMM) runtime
- **`chv-stord`** — storage daemon (volumes, pools, images, snapshots)
- **`chv-nwd`** — network daemon (bridges, netns, nftables, DHCP, DNS)
- **`chv-webui-bff`** — Web UI backend-for-frontend
- **UI** (`/ui`) — SvelteKit frontend served via the BFF

This includes, but is not limited to:

- Authentication or authorization bypass
- Privilege escalation between tenants, projects, or guests
- Guest-to-host or guest-to-guest escape via the agent or runtime glue
- Remote code execution in any control-plane or daemon process
- Insecure cryptographic handling (mTLS material, enrollment tokens, secrets at rest)
- Injection vulnerabilities in the BFF or HTTP surfaces
- Information disclosure of sensitive operator or tenant data

### Out of scope

The following are not handled under this policy. Please report them through the listed channel instead.

| Issue                                                | Where to report                                  |
| ---------------------------------------------------- | ------------------------------------------------ |
| Vulnerabilities in third-party dependencies          | Upstream project (we will pick up the fix on rebase) |
| Vulnerabilities in Cloud Hypervisor itself           | https://github.com/cloud-hypervisor/cloud-hypervisor |
| Vulnerabilities in the host kernel, KVM, distro      | Distribution security tracker                    |
| Social engineering of maintainers or contributors    | Not in scope                                     |
| Physical access attacks against operator hardware    | Not in scope                                     |
| Denial of service requiring privileged local access  | Not in scope                                     |
| Self-XSS, clickjacking on pages without sensitive actions, missing best-practice headers without demonstrated impact | Not in scope |

If you are unsure whether something is in scope, report it anyway and we will route it appropriately.

## Severity Classification

We classify reports using a four-tier scale aligned with CVSS v3.1. Severity sets the fix-target SLA.

| Severity     | Fix target  | Example for CHV's attack surface                                                                 |
| ------------ | ----------- | ------------------------------------------------------------------------------------------------ |
| **Critical** | 7 days      | Unauthenticated remote code execution in `chv-controlplane`; guest-to-host escape via `chv-agent` |
| **High**     | 30 days     | Authenticated tenant-to-tenant data access; mTLS bypass on the agent gRPC surface                 |
| **Medium**   | 60 days     | Authenticated information disclosure of non-sensitive metadata; CSRF on a privileged BFF route    |
| **Low**      | Best effort | Verbose error messages leaking internal paths; missing rate limiting on a non-privileged endpoint |

These targets are guidance, not contracts. Complex fixes can take longer; trivial fixes ship sooner.

## Hall of Fame / Acknowledgements

We credit reporters (with consent) once an advisory is published. This section will be populated as reports are received and resolved.

<!-- MAINTAINER: append entries here as advisories are published, e.g.:
- 2026-09-01 — CVE-YYYY-NNNNN — `chv-agent` privilege issue — reported by @reporter
-->

_No advisories published yet._

## License

This security policy applies to the CHV project, which is licensed under the [Apache License 2.0](LICENSE). Reporting a vulnerability does not transfer rights to your report; we will only use the report for the purpose of fixing the issue and crediting you.
