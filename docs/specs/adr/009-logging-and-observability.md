# ADR-009: Logging and Observability

## Status
Accepted

## Date
2026-04-26

## Context
CHV runs as a distributed system of daemons on hypervisor hosts and control-plane nodes. Logs are the primary diagnostic tool for operators. Without structured logging:
- Log aggregation across multiple daemons is impossible
- Correlating events for a single VM lifecycle requires manual grep
- Secret material (JWT tokens, bootstrap tokens) may leak to stderr
- Production incidents cannot be traced across service boundaries

## Decision
Use `tracing` as the unified logging framework across all Rust code. Follow these rules:

### 1. `tracing` everywhere, never `println!` / `eprintln!`
- Library crates must use `tracing::info!`, `tracing::warn!`, `tracing::error!`, `tracing::debug!`
- `println!` and `eprintln!` are forbidden in library and service code
- CLI tools may use `println!` for user-facing output only

### 2. Structured fields, not string interpolation
- Prefer `tracing::info!(vm_id = %id, "VM started")` over `tracing::info!("VM {} started", id)`
- Structured fields enable filtering and aggregation in log collectors
- Operation IDs must be attached as `operation_id` span fields (see ADR-002 and chv-agent-spec)

### 3. Secret redaction
- Never log JWT secrets, bootstrap tokens, or private keys
- Log file paths where secrets are loaded (`tracing::info!("loaded jwt_secret from {}", path)`) but never the secret value

### 4. Prometheus metrics endpoint
- Every daemon (`chv-controlplane`, `chv-agent`, `chv-stord`, `chv-nwd`)
  exposes a Prometheus text `/metrics` endpoint on a dedicated `metrics_bind`
  HTTP listener. The listener is unauthenticated and loopback by default;
  remote scraping is an explicit operator binding/reverse-proxy decision
  (issue #657).
- Key metrics: VM state counts, node health, operation latency, gRPC error
  rates, storage/network daemon operation counters, process/build info.
- Metrics names follow Prometheus conventions: `chv_vms_total`, `chv_nodes_ready`, etc.

### 5. Log levels
- `error!` — daemon crashes, invariant violations, unrecoverable downstream failures
- `warn!` — recoverable issues (token replay detected, retry exhaustion, degraded node)
- `info!` — lifecycle events (VM created, node enrolled, service started)
- `debug!` — request/response details, reconciliation loop iterations
- `trace!` — internal state dumps, per-packet details (use sparingly)

### 6. Detached background tasks must surface terminal status
- Any task spawned via `tokio::spawn` whose `JoinHandle` outlives the
  spawning function **must** be reaped by a small follow-up task that:
  1. `await`s the `JoinHandle`,
  2. emits an `info!` (Ok), `error!` (Err), or `warn!` (cancelled) breadcrumb
     tagged with `operation_id`,
  3. removes the entry from any tracking registry.
- Without this, terminal `Err` values from spawned futures are silently
  swallowed and operators cannot tell whether a long-running operation
  succeeded, failed, or was aborted. The agent-side migration task
  registry (`crates/chv-agent-core/src/migration_registry.rs`) is the
  canonical example; see ADR-008 §6 for the structural rule.

### 7. `warn!` is not a substitute for failing closed
A `tracing::warn!` line documents that something happened; it does not change
program control flow. For boot-time gates the operator has opted into (see
ADR-008 §7), `warn!` followed by `continue` is a category mismatch: it
downgrades a security/safety boundary to a log line and lets the daemon proceed
into a degraded state.

When a security/safety gate fails:
- emit `tracing::error!` with structured fields (path, error, operation),
- return `Err(...)` to abort the boot or operation,
- never use `warn!` and continue.

The pattern that motivated this rule:
- `cmd/chv-controlplane/src/bootstrap.rs::check_compat_matrix` previously logged
  `warn!("failed to query node versions for compatibility check")` on a
  `node_inventory` query error and continued boot, silently bypassing an
  operator-opted-in version gate. That code now emits `tracing::error!` *and*
  returns `Err`. See H8 in the security review notes.

## Alternatives Considered

### `log` crate + `env_logger`
- Pros: simple, widely understood
- Cons: no structured fields, no async-aware spans, harder to correlate across tasks
- Rejected: `tracing` provides spans and structured fields that are essential for multi-daemon debugging

### `slog` with JSON drain
- Pros: structured logging, configurable drains
- Cons: heavier API, less ecosystem integration with tonic/axum
- Rejected: `tracing` is the de facto standard in the Tokio ecosystem and integrates with OpenTelemetry for future expansion

## Consequences
- All crates must declare `tracing` as a dependency
- Log output format is environment-configurable (pretty for dev, JSON for production)
- Future work: OpenTelemetry trace export for cross-service request tracing

## Enforcement

The `println!`/`eprintln!` ban for library code (rule 1 above) is enforced by a CI gate:

- **Script:** [`scripts/check-no-println.sh`](../../../scripts/check-no-println.sh) greps `crates/` for `println!`, `eprintln!`, `print!`, and `eprint!` macro invocations and exits non-zero on any match.
- **CI step:** the `Rust checks` job in [`.github/workflows/ci.yml`](../../../.github/workflows/ci.yml) runs the script after `cargo clippy`.
- **Local invocation:** `make check-no-println`.

### Scope

The hard gate applies to **library crates only** (`crates/*`), where current usage is zero. Binary crates under `cmd/*` are excluded for these documented exceptions:

- `cmd/chvctl/**` — CLI tool, allowed by rule 1 to use `println!` for user-facing output.
- `cmd/<daemon>/src/main.rs` — single `println!` for `--version` flag output, conventional CLI surface.
- `cmd/*/build.rs` — `println!` is the documented Cargo build-script API for emitting `cargo:rustc-env` directives, not application logging.

### Future tightening

A follow-up may extend enforcement to `cmd/*/src/` (excluding `main.rs --version` blocks and `chvctl`) once the daemon entry points are refactored to route version output through a small helper that the gate can allow-list explicitly. Tracking this here so it does not get lost; opening a new ADR is not required.

