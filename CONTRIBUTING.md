# Contributing to CHV

Thank you for contributing to CHV. This document covers development setup, code style, and the contribution workflow.

## Governance and recognition

- [`GOVERNANCE.md`](GOVERNANCE.md) — roles, decision-making, release authority,
  and the [contributor ladder](GOVERNANCE.md#contributor-ladder).
- [`MAINTAINERS.md`](MAINTAINERS.md) — current maintainers, review
  expectations, emeritus status, and access offboarding.
- [`docs/governance/SECURITY_TRIAGE_ROTA.md`](docs/governance/SECURITY_TRIAGE_ROTA.md)
  — the weekly primary and backup maintainers for private security reports. The
  reporting process and timelines are in [`SECURITY.md`](SECURITY.md).
- [`.github/PULL_REQUEST_TEMPLATE.md`](.github/PULL_REQUEST_TEMPLATE.md) — the
  checklist applied to every pull request.

## Development Environment

### Required Tools

- **Rust** — the exact toolchain is pinned by [`rust-toolchain.toml`](rust-toolchain.toml); install it via [rustup](https://rustup.rs/), which selects the pinned version automatically for any command run in this repository
- **Node.js 22.17+** and npm — for the Web UI
- **protobuf-compiler** — for regenerating gRPC bindings when proto files change
- **Docker** (optional) — for containerized local deployment (`docker compose up`)

### Optional but Recommended

- `cargo-watch` — for auto-rebuilding Rust during development
- `just` or `make` — the repository includes a `Makefile` with common commands

## Building

```bash
# Rust workspace (debug)
cargo build --workspace

# Rust workspace (release)
make build

# Web UI
cd ui && npm install && npm run build

# Both
cd ui && npm run build && cd .. && cargo build --workspace --release
```

## Testing

```bash
# Rust tests (unit + integration; CI uses nextest)
cargo nextest run --workspace --no-fail-fast
# Doctests are not run by nextest, so CI runs them separately
cargo test --workspace --doc

# Coverage floor (same command CI uses; main baseline 78.97%, threshold 76%)
cargo llvm-cov nextest --workspace --profile coverage --lcov --output-path lcov.info --fail-under-lines 76 --no-fail-fast

# Rust linting
cargo clippy --workspace -- -D warnings

# Format check
cargo fmt --all -- --check

# UI build check
cd ui && npm run build

# UI type check (if available)
cd ui && npm run check
```

## Code Style

### Rust

- Format with `rustfmt` (`cargo fmt --all`)
- Lint with Clippy at `-D warnings` level (`cargo clippy --workspace -- -D warnings`)
- Prefer structured errors from `chv-errors` over panics in service code
- Use `tracing` for logging; never `println!` in library crates
- Proto contracts in `/proto` are the source of truth — do not hand-edit generated code in `/gen/rust`

### TypeScript / Svelte

- Follow the existing Prettier configuration in `ui/`
- Use TailwindCSS utility classes; avoid arbitrary values where design tokens exist
- Keep Svelte components under ~300 lines; extract helpers and sub-components when growing larger
- Use TypeScript strictly; avoid `any`

## Frontend Development Guidelines

### State Management

All mutating actions MUST use `mutateWithRefresh()` from `#lib/stores/mutation.svelte.ts`. This ensures page cache, sidebar inventory, and task stream stay in sync.

```svelte
<!-- ✅ Correct -->
<script>
  import { mutateWithRefresh } from '#lib/stores/mutation.svelte.ts';
  async function handleAction() {
    await mutateWithRefresh(
      () => myBffCall(args, token),
      { patterns: ['my-resource:'] }
    );
  }
</script>

<!-- ❌ Incorrect -->
<script>
  import { refreshAll } from '$app/navigation';
  import { invalidatePattern } from '#lib/stores/api-cache.svelte.ts';
  async function handleAction() {
    await myBffCall(args, token);
    invalidatePattern('my-resource:');
    await refreshAll();  // DON'T DO THIS
  }
</script>
```

### Compliance

CI runs `mutation-compliance.test.ts` which scans all `+page.svelte` files. Adding direct `refreshAll` or `invalidatePattern` imports breaks the build.

### New Resource Types

When adding a new resource with mutations:
1. Edit `src/lib/stores/live-state.svelte.ts` and add the task summary → cache pattern mapping to the `taskPatternMap` object inside the `LiveState` class
2. Wire the page mutation through `mutateWithRefresh()`
3. Verify your page passes the compliance tests by running `npm test -- --run src/lib/stores/mutation-compliance.test.ts`

## Changing Protocol Buffers

1. Edit the `.proto` file in `/proto/`
2. Run `cargo build --workspace` to regenerate Rust bindings
3. Update any affected TypeScript types in [`ui/src/lib/bff/types.ts`](ui/src/lib/bff/types.ts) if the backend-for-frontend (BFF) contract changes
4. Update the BFF API spec in [`docs/specs/spec/webui-api-bff-spec.md`](docs/specs/spec/webui-api-bff-spec.md) if the API semantics change

## Commit Messages

Use concise, descriptive messages in the imperative mood:

```
Add quota enforcement to VM create path

- Check project quota before inserting desired state
- Return QuotaExceeded error with limit context
```

## Pull Request Workflow

1. Branch from `main` with a descriptive name: `feat/serial-console`, `fix/db-ownership`, etc.
2. Ensure CI passes: `cargo clippy`, `cargo test`, UI build
3. Update `CHANGELOG.md` under `[Unreleased]` if the change is user-facing
4. Update relevant specs or ADRs if the change affects architectural boundaries
5. Open a PR with a clear description of the problem, solution, and testing performed. The [pull request template](.github/PULL_REQUEST_TEMPLATE.md) prompts for the test evidence, risk and rollback, documentation, security impact, and signed-commit checks
6. Link the related issue with `Part of #<issue>` or `Closes #<issue>`

### High-risk changes

Some changes carry production risk regardless of who wrote them — the rule is
about the change, not the author. If your PR does any of the following, say so
explicitly in the PR description. Link evidence that the behavior was verified,
not just that it compiles. Acceptable evidence includes tests, qualification
runs, or an evidence doc under `docs/evidence/`:

- create, delete, or migrate VMs, volumes, or disks (data-loss paths);
- alter storage or network isolation (capabilities, sandboxing, firewall
  scoping, ownership/permissions models);
- change mTLS, authentication, or authorization boundaries;
- modify lifecycle authority, idempotency, or crash-recovery semantics;
- change database migrations, schema, or destructive upgrade behavior;
- grant new privileges to a service user or unit.

Reviewers should treat the absence of such a statement on a high-risk diff as
a review defect in itself.

## Documentation

- **Architecture decisions** → write or update an ADR in `docs/specs/adr/`
- **Component behavior** → update the component spec in `docs/specs/component/`
- **User-facing features** → update `CHANGELOG.md`
- **Deployment changes** → update `docs/DEPLOYMENT.md`
- **Design system changes** → update `DESIGN.md`
- **Load testing** → follow [`docs/load-testing.md`](docs/load-testing.md)

## Getting Help

- Review existing ADRs in `docs/specs/adr/` for system boundaries and invariants
- Check `docs/plans/` for the current sprint roadmap and gap analysis
- Read `AGENTS.md` for agent-oriented build and architecture guidance (canonical; `CLAUDE.md` just points there)

## Adding a new architecture resource kind

The Architecture Designer ships a closed set of CHV-native resource kinds — by design, not as a TOSCA-style open type system. See [`docs/specs/architecture-designer/`](docs/specs/architecture-designer/) and [ADR-001-Designer](docs/specs/adr/001-designer-first-class-surface.md) through [ADR-006-Designer](docs/specs/adr/006-designer-no-tosca-engine.md). Adding a new kind is an end-to-end change that touches the YAML model, schema, validation, diff, UI, and reviewer ladder.

Use this 8-step recipe. The `server` kind is a good reference: search `crates/chv-architecture-validate/src/model.rs` for `Server` to see every touchpoint.

1. **Model** — Add the kind to `crates/chv-architecture-validate/src/model.rs`. Add a struct with `#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]`, attach it to the `CHVArchitecture` aggregate, and update any `enum ResourceKind`-style discriminator. Round-trip through `serde_yaml` MUST work (covered by `tests/yaml_roundtrip.rs`).

2. **JSON Schema** — Update the embedded YAML schema in `crates/chv-architecture-validate/src/schema.rs`. The schema and the Rust type must drift together; the `schema_drift_test` CI gate catches mismatches.

3. **Static checks** — Add validation rules in `crates/chv-architecture-validate/src/static_checks.rs`. At minimum: name uniqueness within the kind, and references that resolve (a NIC's `network` points to a defined network). Also add capacity bounds for CPU, memory, and disk within the project's accepted ranges. Findings carry stable `code` strings — register the new codes in `crates/chv-architecture-validate/src/codes.rs` (the registry is CI-snapshotted; renames are blocked).

4. **Fleet check** — If the kind has a live counterpart on the cluster (most do — networks, datastores, instances), wire it into the fleet consistency checker under `crates/chv-architecture-validate/src/fleet/`. The check compares the desired YAML against the latest `inventory_snapshot` and reports `BLOCKED_BY_FLEET` findings when prerequisites aren't satisfied.

5. **Diff rules** — Update `crates/chv-architecture-reconcile/src/plan/diff.rs` with the create / update / delete / replace / noop rules for the kind. Field-level rules decide which mutations are in-place vs. require replacement (e.g. CPU resize is in-place; storage backend change is replace). The diff feeds the planner and shapes the user-visible plan preview.

6. **UI palette and inspector** — Add a draggable palette node under `ui/src/lib/components/architectures/nodes/` with the canonical icon and label. Add a corresponding inspector pane (right-hand panel) that exposes every editable field. Keep components under ~300 lines; extract sub-components if the inspector grows. Wire the new kind into the canvas graph and `graphToYaml()` in `ui/src/lib/stores/architecture-canvas-store.svelte.ts` so canvas → YAML round-trips.

7. **Fixtures** — Add at least three fixtures to `crates/chv-architecture-validate/tests/fixtures/`:
   - **Positive** — a minimal valid topology that includes the new kind.
   - **Edge case** — boundary values (max name length, max resources of this kind, sparse optional fields).
   - **Negative** — a topology that violates a static check the new kind introduced; assert the expected `Finding.code`.

8. **Reviewer checklist** — On the PR, request these reviewers (per the Phase 7 review ladder):
   - `reviewer-api-contract` — confirms the schema change is backward-compatible (additive); flags any breaking field rename or type change.
   - `reviewer-security` — required if the kind is RBAC-scoped (users, roles, permissions, secrets) or affects a production-environment guarded path.
   - `reviewer-language-specialist` — Rust and TypeScript idiom and structure review.

Reviewer-test-analyzer should also confirm fixtures cover the new code paths.
