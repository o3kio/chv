# CHV

> Control plane for Cloud Hypervisor (the VMM) — lightweight virtualization and infrastructure evaluation.

CHV is an open-source control plane and evaluation platform around **Cloud Hypervisor**. It is designed for teams exploring lightweight virtualization, KVM-based workloads, and infrastructure patterns beyond traditional hypervisor stacks.

| Field   | Value                                  |
| ------- | -------------------------------------- |
| Status  | Technical Preview / Active Development |
| License | Apache-2.0 core |
| Website | [cellhv.com](https://cellhv.com)       |

## What this is

CHV is a technical evaluation platform for lightweight virtualization and Cloud Hypervisor-based infrastructure.

It focuses on:

* Cloud Hypervisor-based VM lifecycle experiments
* lightweight virtualization control-plane patterns
* KVM-based workload evaluation
* API-driven infrastructure management
* reproducible technical evaluation
* operator-friendly infrastructure workflows

## What this is not

CHV is not presented as a general VMware, Proxmox, or OpenStack replacement.

It is not a fully mature production virtualization platform yet. It is a technical-preview infrastructure product for teams that want to evaluate lightweight virtualization patterns before adopting or extending them.

It does not implement tenant-data backup or restore. That surface is hard-fenced (`501 NOT_IMPLEMENTED`) and tenant-data protection remains the operator's responsibility; see [`docs/DEPLOYMENT-ARCHITECTURE.md`](./docs/DEPLOYMENT-ARCHITECTURE.md) §7.


CHV is a Linux-first, cloud-image-first virtualization platform for sovereign private cloud and edge cloud environments. It provides API-driven VM lifecycle management built on [Cloud Hypervisor](https://www.cloudhypervisor.org/).

## Current Phase

**Version:** `0.4.0` (release candidate `v0.4.0-rc.5`)  
**Phase:** Early-to-MVP transitioning to stability  

The project has a solid Phase 1 foundation (Rust control plane, SQLite store, certificate enrollment, gRPC services) and a functional SvelteKit Web UI. Active work is tracked in the [Phased Implementation Plan](./PHASED_IMPLEMENTATION_PLAN.md) covering stability hardening, feature completion, and production readiness.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                         Web UI                              │
│                     (SvelteKit + Tailwind)                  │
└─────────────────────────────┬───────────────────────────────┘
                              │ HTTP / BFF
┌─────────────────────────────▼───────────────────────────────┐
│                   chv-controlplane                            │
│         (Orchestration · SQLite · BFF HTTP · gRPC)          │
│                        + chvctl CLI                           │
└─────────────────────────────┬───────────────────────────────┘
                              │ mTLS gRPC
┌─────────────────────────────▼───────────────────────────────┐
│                      chv-agent                                │
│  (VM lifecycle · Cloud Hypervisor runtime · Serial console) │
├─────────────────────────────┬───────────────────────────────┤
│         chv-stord           │           chv-nwd             │
│   (Volumes · Pools · Images)│  (Networks · Firewall · NAT)  │
└─────────────────────────────┴───────────────────────────────┘
```

- **Backend / Control Plane**: Rust (Tokio, tonic, axum, sqlx/SQLite)
- **Frontend**: SvelteKit 3 + Svelte 5 + TailwindCSS + Vite
- **Contracts**: gRPC/protobuf
- **Metrics**: Prometheus `/metrics` from every daemon (`chv-controlplane`, `chv-agent`, `chv-stord`, `chv-nwd`) on a dedicated, loopback-by-default `metrics_bind` listener — see [`docs/OBSERVABILITY.md`](./docs/OBSERVABILITY.md)

## Repository Structure

```
.
├── cmd/                    # Rust binaries
│   ├── chv-agent/          # Node agent (VM lifecycle, Cloud Hypervisor runtime, serial console)
│   ├── chv-controlplane/   # Control plane (orchestration, node mgmt, enrollment, BFF)
│   ├── chv-nwd/            # Network daemon (bridge, netns, nftables, DHCP, DNS)
│   └── chv-stord/          # Storage daemon (volumes, pools, images, snapshots)
├── crates/                 # Rust library crates
│   ├── chv-common/
│   ├── chv-config/
│   ├── chv-hypervisor-api/  # Hypervisor-neutral VMM adapter interface
│   ├── chv-controlplane-*
│   ├── chv-agent-*
│   ├── cellhv-core-*        # CellHV Core single-node authority & journal crates
│   ├── chv-nwd-core/
│   ├── chv-stord-*
│   ├── chv-webui-bff/
│   ├── chv-errors/
│   └── chv-observability/
├── gen/rust/               # Generated Rust code from proto contracts
├── proto/                  # Authoritative gRPC/protobuf contracts
│   ├── controlplane/       # Control plane ↔ Agent
│   ├── node/               # Agent ↔ Storage/Network
│   └── webui/              # Web UI ↔ BFF
├── ui/                     # SvelteKit Web UI
│   └── src/
│       ├── routes/         # Page routes
│       └── lib/            # Components, stores, API clients
├── scripts/                # Install and build scripts
├── deploy/                 # Deployment artifacts (entrypoint, systemd)
└── docs/                   # Specs, ADRs, design docs, and plans
    ├── specs/adr/          # Architecture Decision Records
    ├── specs/component/    # Component specifications
    ├── plans/              # Sprint and implementation plans
    └── examples/           # Example configs (TOML, nginx, systemd)
```

## Install

**Supported today:** one host, Cloud Hypervisor v53.0, Ubuntu amd64. No multi-host, HA, or backup/restore — see [`docs/DEPLOYMENT-ARCHITECTURE.md` §7](./docs/DEPLOYMENT-ARCHITECTURE.md#7-supported-envelope).

The qualified deployment path is the all-in-one installer on a fresh Ubuntu amd64 host:

```bash
curl -fsSL https://raw.githubusercontent.com/o3kio/chv/main/scripts/install.sh | sudo bash -s

# Pin a release instead of resolving the latest stable (the current release
# candidate is 0.4.0-rc.5; 0.3.0 remains the latest stable):
curl -fsSL https://raw.githubusercontent.com/o3kio/chv/main/scripts/install.sh | sudo INSTALL_CHV_VERSION=0.4.0-rc.5 bash -s
```

The installer requires **bash** (it uses `set -o pipefail`); do not replace `bash -s` with `sh -`.

CHV is also distributed as `.deb` and `.rpm` packages. Choose the guide for your distribution:

| Distribution | Guide |
|--------------|-------|
| Debian, Ubuntu | [`docs/install/debian-ubuntu.md`](./docs/install/debian-ubuntu.md) |
| RHEL, Rocky Linux, AlmaLinux | [`docs/install/rhel-rocky-alma.md`](./docs/install/rhel-rocky-alma.md) |
| Other / Tarball | [`docs/install/from-github-release.md`](./docs/install/from-github-release.md) |
| In-guest monitoring agent (optional) | [`docs/install/guest-monitor-agent.md`](./docs/install/guest-monitor-agent.md) |

Quick package install (Debian/Ubuntu):

```bash
VERSION="<version>"
BASE_URL="https://github.com/o3kio/chv/releases/download/v${VERSION}"
curl -sLO "${BASE_URL}/chv-controlplane_${VERSION}_amd64.deb"
curl -sLO "${BASE_URL}/chv-node_${VERSION}_amd64.deb"
curl -sLO "${BASE_URL}/chvctl_${VERSION}_amd64.deb"
sudo dpkg -i chv-controlplane_${VERSION}_amd64.deb chv-node_${VERSION}_amd64.deb chvctl_${VERSION}_amd64.deb
sudo systemctl daemon-reload
sudo systemctl enable --now chv-controlplane chv-agent chv-stord chv-nwd
```

Packages provide the layout only: provision TLS and a real `jwt_secret` before
starting the services (see the distribution guide, steps 5–6), and do not start
them with the example configs. Package-only installs do not seed an admin login
— use `scripts/install.sh` or `scripts/quick-install.sh` for a working
deployment.

See [`docs/install/channels.md`](./docs/install/channels.md) for stable, RC, nightly, and PR artifact channels.

## Development

### Prerequisites

- [Rust](https://rustup.rs/) (stable)
- [Node.js](https://nodejs.org/) 22.17+ and npm
- `protobuf-compiler` (for regenerating `gen/rust/` from `proto/`)

### Quick Start

```bash
# Build the entire workspace
cargo build --workspace

# Run all Rust tests (unit + integration)
cargo nextest run --workspace --no-fail-fast
# Doctests are not run by nextest
cargo test --workspace --doc

# Build the Web UI
cd ui && npm install && npm run build

# Run the Web UI dev server
cd ui && npm run dev
```

### Makefile Shortcuts

| Command | Description |
|---------|-------------|
| `make build` | Release build of the Rust workspace |
| `make build-ui` | Install deps and build the UI |
| `make test` | Run Rust tests |
| `make fmt` | Format all Rust code |
| `make release` | Create a release tarball (`dist/`) |
| `make dev-install` | Install systemd services for local development |
| `make clean` | Clean build artifacts |

### Proto Changes

If you modify `.proto` files, regenerate the Rust bindings:

```bash
# The workspace uses tonic-build; typically a rebuild is sufficient:
cargo build --workspace
```

## Key Documentation

| Document | Purpose |
|----------|---------|
| [`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md) | System architecture, data flow, and boundaries |
| [`docs/DEPLOYMENT.md`](./docs/DEPLOYMENT.md) | Deploy CHV on a combined control-plane + hypervisor host |
| [`docs/specs/adr/`](./docs/specs/adr) | Architecture Decision Records (41 ADRs; numbering ADR-001–ADR-029, with `001`–`006` reused across domains) |
| [`docs/specs/component/`](./docs/specs/component) | Component specs (agent, stord, nwd) |
| [`PHASED_IMPLEMENTATION_PLAN.md`](./PHASED_IMPLEMENTATION_PLAN.md) | Phased implementation roadmap |
| [`docs/OPERATIONS.md`](./docs/OPERATIONS.md) | Day-2 operations, monitoring, and troubleshooting |
| [`docs/OBSERVABILITY.md`](./docs/OBSERVABILITY.md) | Observability contract: metrics, SLIs/SLOs, logs, and alerts |
| [`docs/PACKAGING.md`](./docs/PACKAGING.md) | Package layout for `chvctl`, `chv-controlplane`, and `chv-node` |
| [`docs/runbooks/`](./docs/runbooks) | Disaster-recovery and restore runbooks |
| [`docs/GAP_ANALYSIS.md`](./docs/GAP_ANALYSIS.md) | Specification-versus-implementation gap analysis |
| [`docs/install/`](./docs/install) | Installation guides (Debian, RHEL, channels, uninstall) |
| [`docs/release/`](./docs/release) | Release process, versioning, and artifact verification |
| [`docs/governance/`](./docs/governance) | Repository governance: branch protection and hardening records |
| [`docs/prompts/`](./docs/prompts) | Bounded execution prompt packs (CellHV Core, production readiness, ADR-022) |
| [`DESIGN.md`](./DESIGN.md) | Design system (typography, color, spacing, dark mode) |
| [`CHANGELOG.md`](./CHANGELOG.md) | Release history |
| [`CONTRIBUTING.md`](./CONTRIBUTING.md) | Development workflow, code style, and PR process |
| [`SECURITY.md`](./SECURITY.md) | Security policy and vulnerability reporting |
| [`CODE_OF_CONDUCT.md`](./CODE_OF_CONDUCT.md) | Community code of conduct |
| [`AGENTS.md`](./AGENTS.md) | Agent orientation and build rules (canonical for coding agents) |

## CI / CD

GitHub Actions runs on every push and PR:
- `cargo check`, `cargo clippy -- -D warnings`, `cargo nextest run --workspace --no-fail-fast`, plus `cargo test --workspace --doc` for doctests
- A blocking `cargo llvm-cov nextest` line-coverage floor (minimum 76%; reproducible `main` baseline 78.97%), with lcov artifact + job summary
- UI `npm ci`, `npm run build`, `npm run check`, and Playwright tests against the mock BFF (`ui/tests/e2e/mock-bff.mjs`)
- A real-BFF Playwright flow against a disposable-SQLite `chv-controlplane` (real router/auth/serialization), plus a fail-closed bad-credential case
- `chvctl` smoke test and VERSION validation

The nightly/weekly self-hosted `chv-kvm` lanes run the M4.3–M4.8 qualification
scripts (m4.7 chaos nightly, the full sequence weekly). The exact split between
PR, scheduled, and manual evidence — and what remains unverified — is documented
in [`docs/dev/test-evidence-tiers.md`](./docs/dev/test-evidence-tiers.md).

See [`.github/workflows/ci.yml`](./.github/workflows/ci.yml) and
[`.github/workflows/nightly-kvm-qualification.yml`](./.github/workflows/nightly-kvm-qualification.yml).

## Version

Current version: `0.4.0` (see [`VERSION`](./VERSION))

## Direction

> Rust is the active backend and control-plane language for CHV. New backend work belongs in the Rust workspace (`/cmd`, `/crates`, `/gen/rust`), driven by the proto contracts in `/proto` and the specs in `/docs/specs`.
