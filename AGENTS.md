# CHV - Cloud Hypervisor Virtualization Platform

## Project Overview

CHV is a Rust-first virtualization management repository with a SvelteKit frontend and proto/spec-driven backend direction. It provides API-driven VM lifecycle management built on Cloud Hypervisor for sovereign private cloud and edge environments.

## Repository Direction

- Active backend/control-plane language: **Rust**
- Active backend workspace: `/Cargo.toml`, `/cmd`, `/crates`, `/gen/rust`
- Authoritative contracts: `/proto`
- Authoritative design and behavior docs: `/docs/specs`, `/docs/plans`
- Current phase: Early-to-MVP transitioning to stability (see [`PHASED_IMPLEMENTATION_PLAN.md`](PHASED_IMPLEMENTATION_PLAN.md))

## Build Commands

The Rust toolchain is pinned by `rust-toolchain.toml` (single source of truth — CI and release packaging use the same pin via `.github/actions/setup-rust`). Toolchain bumps are reviewable PRs that change only that pin; see `docs/release/PIPELINE.md`.

```bash
# Rust workspace
cargo build --workspace
cargo nextest run --workspace --no-fail-fast  # unit + integration (CI runner)
cargo test --workspace --doc                 # doctests (not supported by nextest)
cargo clippy --workspace -- -D warnings
cargo fmt --all

# Frontend
cd ui && npm install && npm run build

# Release packaging
make build-release        # Build binaries + tarball
make package-deb          # Build .deb packages (requires nfpm)
make package-rpm          # Build .rpm packages (requires nfpm)
make package-local        # Build both formats
make package-smoke-deb    # Smoke test .deb in Docker
make package-smoke-rpm    # Smoke test .rpm in Docker
make check-release-local  # Run all local release checks

# Local dev install with systemd units
make dev-install
```

## Release Engineering

**If you are working on releases, packaging, CI/CD, or versioning, read [`docs/release/PIPELINE.md`](docs/release/PIPELINE.md) first.**

Quick reference:
- Version source of truth: [`VERSION`](VERSION)
- Version derivation: [`scripts/version.sh`](scripts/version.sh)
- Package builder: [`scripts/build-packages.sh`](scripts/build-packages.sh)
- CI workflows: [`.github/workflows/`](.github/workflows/)
- Release process: [`docs/release/release-process.md`](docs/release/release-process.md)

## Proto Generation

If you change `.proto` files:

```bash
cargo build --workspace
```

The workspace `build.rs` files use `tonic-prost-build` (the tonic 0.14 codegen crate) to regenerate code in `/gen/rust`. Do not hand-edit generated files.

## Backend Implementation Rules

- New backend/control-plane work belongs in the Rust workspace, not any archived Go tree.
- Proto contracts in `/proto` are the source of truth for inter-service APIs.
- ADRs and component specs in `/docs/specs` define the intended system boundaries.
- Use `chv-errors` for structured errors; avoid panics in service code.
- Use `tracing` for logging; never `println!` in library crates.
- Keep Svelte components under ~300 lines; extract helpers when growing larger.
- Use `mutateWithRefresh()` for all WebUI mutations; never call `refreshAll()` or `invalidatePattern()` directly in page components
- High-risk changes (data-loss paths, isolation/sandboxing, mTLS/authz, lifecycle
  authority, migrations, privilege grants) require explicit PR-disclosure and
  linked verification evidence — see the "High-risk changes" rule in
  [`CONTRIBUTING.md`](CONTRIBUTING.md).

## Key Files for Context

| File | Why it matters |
|------|---------------|
| [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | High-level architecture, data flow, current phase |
| [`docs/specs/adr/`](docs/specs/adr/) | Boundaries and invariants (agent/stord/nwd split, control-plane boundary, state machines, error handling, logging, async safety) |
| [`docs/specs/component/`](docs/specs/component/) | Component responsibilities and failure behavior |
| [`PHASED_IMPLEMENTATION_PLAN.md`](PHASED_IMPLEMENTATION_PLAN.md) | Phased implementation roadmap |
| [`docs/OPERATIONS.md`](docs/OPERATIONS.md) | Day-2 operations, monitoring, and troubleshooting |
| [`DESIGN.md`](DESIGN.md) | Design system tokens (colors, typography, spacing) |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | Dev setup, code style, PR workflow |
| [`docs/release/PIPELINE.md`](docs/release/PIPELINE.md) | **Release engineering: read this first for any packaging/CI/CD/versioning work** |
| [`VERSION`](VERSION) | Single source of truth for release version |
| [`Makefile`](Makefile) | Packaging, testing, signing, and integration targets |

## Agent Context and Token Efficiency

Use the repository broadly during planning only when the task genuinely requires it. Once the plan identifies the affected component, crate, files, contracts, and tests, execution must start from that bounded scope instead of rediscovering the whole repository.

### Search and reading discipline

- Prefer `rg`, `git grep`, targeted directory listings, and bounded file reads over recursive `find`, `ls -R`, or broad file dumps.
- Respect `.gitignore` and `.rgignore`. Do not search `target/`, `node_modules/`, `dist/`, Playwright output, worktrees, local VM images/data, caches, or agent-tool state unless the task explicitly depends on them.
- Do not repeatedly read files that are unchanged and already understood. Re-open only the relevant section or inspect `git diff` after edits.
- Do not dump large generated files or command logs into model context. For noisy commands, capture output and inspect the failure lines or a bounded tail.
- Generated Rust under `gen/rust/` remains searchable because it is part of the proto/build contract, but do not hand-edit it and do not inspect it unless the proto/API task requires it.

### Planner/executor discipline

Before implementation, record the smallest useful execution scope:

- affected service/component;
- affected Cargo package(s) or UI area;
- relevant proto/spec/ADR;
- files expected to change;
- tests/checks that prove the change.

An implementation agent should begin from that scope and expand only when code or test evidence shows the plan was incomplete. Do not repeat broad architecture discovery already completed by a planning agent.

### Validation ladder

Use the cheapest relevant validation first, then widen:

```bash
# Examples; replace <package> with the affected workspace package.
cargo check -p <package>
cargo test -p <package>
cargo clippy -p <package> --all-targets -- -D warnings
```

For UI-only work, stay under `ui/` until a cross-boundary check is needed. For proto changes, validate the affected generated API/consumer before widening to the workspace.

Run the repository's full required checks (`cargo nextest run --workspace
--no-fail-fast`, `cargo test --workspace --doc`, workspace clippy/build, frontend
build, release gates, or task-specific Make targets) once the targeted checks
pass and before declaring the task complete. This section changes validation
order, not the final quality gate.
