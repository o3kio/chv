# Observability

CHV exports Prometheus metrics, structured logs, and per-request correlation IDs.
This document defines the operator-facing observability contract: what to scrape,
what to alert on, and how to investigate incidents.

---

## Metrics endpoints

| Service | Endpoint | Default bind |
|---|---|---|
| `chv-controlplane` | `/metrics` on a dedicated metrics listener | `metrics_bind`; code default `127.0.0.1:9103` |
| `chv-agent` | `/metrics` | `metrics_bind` in agent config; packaged/installer default `127.0.0.1:9901`; code fallback when unset `127.0.0.1:9100` (loopback) |
| `chv-stord` | `/metrics` on a dedicated metrics listener | `metrics_bind`; code default `127.0.0.1:9101` |
| `chv-nwd` | `/metrics` on a dedicated metrics listener | `metrics_bind`; code default `127.0.0.1:9102` |
| `chv-webui-bff` | served on the `chv-controlplane` HTTP listener | via the control plane's HTTP routes, not a separate scrape target |

`chv-controlplane`, `chv-stord`, and `chv-nwd` render the real process
registry on their dedicated listener: every `metrics` counter and histogram the
daemon records, plus build/process series (`chv_build_info`,
`process_start_time_seconds`, `chv_process_uptime_seconds`). It is not a
placeholder or a static page.

`chv-agent` has a **separate, older endpoint**: it hand-rolls its exposition
(`chv_agent_*`, journal/monitoring counters) rather than rendering the shared
process registry, and it does not emit the `chv_build_info` /
`process_start_time_seconds` / `chv_process_uptime_seconds` preamble. Its
metric vocabulary and labels are unchanged by #657; unifying the agent with the
shared renderer is deferred to a follow-up.

### Scrape authentication model

- **Default: loopback-only, unauthenticated scraper access.** Every
  `metrics_bind` default above is `127.0.0.1`. A Prometheus instance on the
  same host scrapes without a bearer token or credential file.
- **No admin JWT rotation.** The control-plane's *legacy* `/metrics` route on
  `http_bind` remains behind the admin auth gate for backward compatibility
  (unauthenticated requests still get `401`, and the Web UI route contract is
  unchanged). Scrapers must use the dedicated metrics listener instead of
  minting short-lived admin JWTs.
- **Remote scraping is an explicit operator action.** Set `metrics_bind` to
  the node's management address (for example
  `metrics_bind = "192.0.2.10:9101"`), never `0.0.0.0`/`[::]`, and restrict
  access at the firewall/network-policy layer. Alternatively keep the loopback
  bind and put an operator-managed reverse proxy in front with TLS, mTLS, or
  basic auth. The metrics listener has no authentication of its own; the bind
  address and the surrounding network are the security boundary. A wildcard
  bind is still accepted for compatibility, but each daemon logs a loud
  warning that the unauthenticated listener is exposed on **all** interfaces;
  treat that warning as a misconfiguration unless a firewall/reverse proxy is
  deliberately in front of it.
- **Fail closed on misconfiguration.** `chv-stord`, `chv-nwd`, and
  `chv-controlplane` bind their metrics listener before serving and abort
  startup on an invalid `metrics_bind` or an occupied port. `chv-agent` keeps
  its historical behavior: a bind failure is logged, not fatal.
- The agent code fallback `9100` is node_exporter's default port: on hosts
  running node_exporter, set `metrics_bind` explicitly so the agent cannot lose
  its listener to an address-in-use error. The stord/nwd/control-plane defaults
  use dedicated ports (`9101`/`9102`/`9103`) to avoid that collision.

`quick-install.sh` deliberately binds the control plane HTTP listener on
`0.0.0.0:8080` (plain HTTP); firewall it. That does not affect the metrics
listener, which stays on its own loopback `metrics_bind` unless the operator
changes it. The packages and `scripts/install.sh` leave the HTTP listener on
`127.0.0.1:8080`.

---

## Service Level Indicators (SLIs)

SLIs are the measurable properties we use to assess service health. Each SLI is expressed
as a PromQL expression that can be evaluated continuously.

> **Availability caveat ([#702](https://github.com/o3kio/chv/issues/702)).**
> SLI-1 and SLI-2 are backed by `chv_vm_ops_total` /
> `chv_vm_op_duration_seconds`. The shipped agent records those samples into
> the global Prometheus registry but its `:9901` handler renders only the
> hand-built `chv_agent_*` families, so packaged agents expose neither series.
> Until #702 lands, SLI-1/SLI-2 have no data from agent scrapes and
> `VmStartLatencyHighP99` / `VmOpFailureRateHigh` cannot fire. The
> journal-based diagnosis in their runbooks works today.

### SLI-1: VM start latency (p99)

**Definition:** 99th percentile of the time from `create_vm` invocation on the Cloud Hypervisor
(the VMM) adapter to the operation returning `Ok`. This covers the path from the agent accepting a start
command to the Cloud Hypervisor API completing.

```promql
histogram_quantile(0.99,
  sum by (le) (
    rate(chv_vm_op_duration_seconds_bucket{op="start"}[5m])
  )
)
```

**Good state:** < 30s. Degraded: 30–45s. Breaching: > 45s.

---

### SLI-2: VM lifecycle operation error rate

**Definition:** Fraction of VM lifecycle operations (create/start/stop/delete/pause/resume)
that return an error.

```promql
sum(rate(chv_vm_ops_total{result="err"}[5m]))
/
sum(rate(chv_vm_ops_total[5m]))
```

**Good state:** < 1%. Degraded: 1–5%. Breaching: > 5%.

---

### SLI-3: gRPC control-plane error rate

**Definition:** Fraction of gRPC requests to the control plane that return a non-OK gRPC status.

```promql
(
  sum(rate(chv_grpc_server_requests_total{grpc_status!~"^(0|OK)$"}[5m]))
  /
  sum(rate(chv_grpc_server_requests_total[5m]))
)
```

**Good state:** < 1%. Breaching: > 5% for 5 minutes.

---

### SLI-4: Control-plane convergence latency

**Definition:** Rolling average of the time for the control plane to converge desired to
observed state across all tracked resources.

```promql
chv_cp_convergence_avg_ms
```

**Good state:** < 5000ms. Degraded: 5000–10000ms. Breaching: > 10000ms.

---

### SLI-5: BFF API success rate

**Definition:** Fraction of HTTP requests to the webui-bff that return a non-5xx status code.

```promql
1 - (
  sum(rate(http_requests_total{status=~"5.."}[5m]))
  /
  sum(rate(http_requests_total[5m]))
)
```

**Good state:** > 99.9%. Degraded: 99.5–99.9%. Breaching: < 99%.

---

### SLI-6: Node liveness

**Definition:** All enrolled CHV nodes are in a Ready state.

```promql
chv_nodes_ready
```

**Good state:** equals the expected node count. Breaching: any drop.

---

## Service Level Objectives (SLOs)

| SLI | Target | Measurement window | Alert threshold | Severity |
|---|---|---|---|---|
| VM start p99 | < 30s | 30-day rolling | > 45s for 10m | warning |
| VM op error rate | < 1% | 30-day rolling | > 5% for 5m | warning |
| gRPC error rate | < 1% | 30-day rolling | > 5% for 5m | warning |
| Convergence latency | < 5000ms avg | 30-day rolling | > 10000ms for 10m | warning |
| BFF API success | > 99.9% | 30-day rolling | < 99% for 5m | warning |
| Node liveness | 100% | 7-day rolling | any drop > 2m | critical |

Error budget: for each SLO the error budget is 1 – target over the window. When an alert fires,
check the recording rules (`chv:*:5m`) to see whether you are on track to exhaust the error budget.

---

## Key metrics reference

### VM lifecycle (`chv-agent-runtime-ch`)

| Metric | Type | Labels | Description |
|---|---|---|---|
| `chv_vm_ops_total` | counter | `op`, `result` | VM lifecycle operation count. `op` ∈ {create,start,stop,delete,pause,resume}; `result` ∈ {ok,err} |
| `chv_vm_op_duration_seconds` | histogram | `op` | Duration of VM lifecycle operations (both ok and err paths) |

### gRPC server (`chv-observability GrpcMetricsLayer`)

| Metric | Type | Labels | Description |
|---|---|---|---|
| `chv_grpc_server_requests_total` | counter | `service`, `method`, `grpc_status` | gRPC request count per method |
| `chv_grpc_server_duration_seconds` | histogram | `service`, `method`, `grpc_status` | gRPC request duration |

> `grpc_status` is the numeric gRPC status code (`"0"` = OK). Unary responses
> use the leading `grpc-status` header; streaming responses are observed at the
> trailer frame, so a completed or errored stream is recorded with its terminal
> status. `"unknown"` now means only that no status was carried in either
> headers or trailers (a protocol violation), plus transport-level body errors.
> `"client_cancelled"` is a distinct terminal value recorded when the client
> drops the response body before it completes (`RST_STREAM`, disconnect, or a
> canceled request) — the handler ran, but no terminal status was observed.

### Control-plane convergence

| Metric | Type | Labels | Description |
|---|---|---|---|
| `chv_cp_drift_count` | gauge | — | Number of resources currently diverged from desired state |
| `chv_cp_pending_operations` | gauge | — | Operations in the orchestrator queue |
| `chv_cp_convergence_avg_ms` | gauge | — | Rolling average convergence time in milliseconds |
| `chv_cp_consecutive_drift_ticks` | gauge | — | Ticks since last successful full convergence |
| `chv_cp_reconcile_ticks_total` | counter | — | Total reconcile ticks fired |
| `chv_cp_operations_dispatched_total` | counter | — | Total operations claimed and dispatched |

### Migration

| Metric | Type | Labels | Description |
|---|---|---|---|
| `chv_migration_phase` | gauge | — | Current migration phase (ordinal) |
| `chv_migration_bytes_transferred` | counter | — | Bytes transferred during disk migration |
| `chv_migration_duration_seconds` | histogram | `result` | Total migration duration |
| `chv_migration_dirty_blocks` | gauge | — | Remaining dirty blocks during precopy convergence |
| `chv_migration_restart_reconciled_total` | counter | `recovered_phase` | Non-terminal migration rows reconciled at control-plane startup (`recovered_phase` ∈ {`RolledBack`,`Failed`}); a nonzero value means a restart interrupted migrations |
| `chv_migration_progress_dropped_total` | counter | `stored_phase`, `reported_phase` | Progress reports dropped because the migration was already terminal. Watch for `reported_phase="Completed"` after recovery: it means an agent-side task is still running |
| `migration_reaper_resume_skipped_total` | counter | `reason` | Source-VM resume attempts skipped by the reaper; `reason` ∈ {`sender_active`,`source_node_not_reporting`,`interrupted_ambiguous`} |

### Network (eBPF / VXLAN)

| Metric | Type | Labels | Description |
|---|---|---|---|
| `chv_vxlan_fdb_entries` | gauge | — | Active VXLAN FDB entries |
| `chv_ebpf_packets_total` | counter | — | Packets processed by eBPF datapath |
| `chv_ebpf_bytes_total` | counter | — | Bytes processed by eBPF datapath |

### HTTP (BFF)

| Metric | Type | Labels | Description |
|---|---|---|---|
| `http_requests_total` | counter | `method`, `status` | HTTP request count |
| `http_request_duration_seconds` | histogram | `method`, `path` | HTTP request duration; path is cardinality-capped with `{id}` substitution |

---

## Logs

All CHV daemons emit structured logs via the `tracing` crate (see ADR-009). Log
format is configurable:

```bash
# JSON (production)
CHV_LOG_FORMAT=json chv-agent ...

# Pretty (development)
CHV_LOG_FORMAT=pretty chv-agent ...   # or unset
```

### Filtering logs by operation

Operation-scoped events carry the operation id: the control plane and
`chv-nwd` put it on an `operation_id` span field, and the Cloud Hypervisor
adapter emits it as the `op` event field. `journalctl -o cat` prints the raw
log line so `jq` can filter it; `journalctl -o json` wraps the line in
journald's `MESSAGE` field and will not match these filters. Structured-field
queries require the process to log JSON (`CHV_LOG_FORMAT=json`); the packaged
units do not set it by default, so plain `grep` on `journalctl -o cat` is the
fallback.

```bash
# systemd journal (requires CHV_LOG_FORMAT=json)
journalctl -u chv-agent -o cat | jq 'select(.fields.op == "abc123ef")'
journalctl -u chv-controlplane -o cat | jq 'select((.span.operation_id // .span.op_id // .fields.operation_id) == "abc123ef")'

# File log
grep '"operation_id":"abc123ef"' /var/log/chv/agent.log
```

Every HTTP error response body includes a `request_id` field that matches the
`x-correlation-id` response header. The BFF stores that id on the durable
audit row (`audit_log.correlation_id`), not on every log event; query the
audit table for the route, actor, outcome, and `operation_id`, then filter
logs by that operation id:

```bash
# User reports: request_id = "x7k9p2"
sqlite3 "file:/var/lib/chv/controlplane.db?mode=ro" \
  "SELECT occurred_at_ms, status_code, method, path, outcome, operation_id
   FROM audit_log WHERE correlation_id='x7k9p2'
   ORDER BY occurred_at_ms DESC LIMIT 20;"
```

---

## Distributed tracing

**CHV does not emit OTLP/OpenTelemetry traces today.** There is no OTLP
exporter, collector configuration, or trace-backend integration in the shipped
binaries. Do not point an OpenTelemetry collector at CHV and expect spans.

What exists instead:

- Every operation record has an ID, and operation-scoped log events carry it
  (`operation_id` span field in the control plane and `chv-nwd`; `op` event
  field in the Cloud Hypervisor adapter). Filter structured logs by it as
  shown in [Filtering logs by operation](#filtering-logs-by-operation).
- Every HTTP error body includes a top-level `request_id` that matches the
  `x-correlation-id` response header. An inbound `x-operation-id` seeds that
  correlation ID; otherwise the BFF generates one.
- Use the structured logs and the metrics in this document to follow a
  request across the BFF, control plane, and agent. `operation_id` and the
  correlation/`request_id` value are the join keys; there is no trace-ID
  propagation beyond those fields today.

---

## Alert runbooks

| Alert | Severity | Runbook |
|---|---|---|
| `VmStartLatencyHighP99` | warning | [docs/runbooks/vm-start-slow.md](runbooks/vm-start-slow.md) |
| `VmOpFailureRateHigh` | warning | [docs/runbooks/vm-op-failures.md](runbooks/vm-op-failures.md) |
| `ControlPlaneGrpcErrorRateHigh` | warning | [docs/runbooks/control-plane-dr.md](runbooks/control-plane-dr.md) |
| `ControlPlaneOpQueueGrowing` | warning | [docs/runbooks/control-plane-dr.md](runbooks/control-plane-dr.md) |
| `ControlPlaneConvergenceSlow` | warning | [docs/runbooks/control-plane-dr.md](runbooks/control-plane-dr.md) |
| `ControlPlaneDriftPersistent` | critical | [docs/runbooks/control-plane-dr.md](runbooks/control-plane-dr.md) |
| `MigrationFailureRateHigh` | warning | [docs/runbooks/vm-snapshot-restore.md](runbooks/vm-snapshot-restore.md) |
| `BffApiErrorRate` | warning | [docs/runbooks/bff-errors.md](runbooks/bff-errors.md) |
| `NodeDown` | critical | [docs/runbooks/full-site-recovery.md](runbooks/full-site-recovery.md) |

All alert rules above reference a runbook. The runbook file is the operational
procedure; this table is the index.

---

## Prometheus scrape configuration

Prometheus must run on the same host as the scraped daemons (or reach them
through an operator-managed relay): every packaged/`scripts/install.sh`
metrics listener is loopback. All four jobs below scrape the unauthenticated
dedicated listener and need no credential file. `quick-install.sh` binds the
control plane HTTP listener on `0.0.0.0:8080` (plain HTTP), so firewall that
port; the metrics listener is unaffected and remains loopback unless changed.

```yaml
# Minimal prometheus.yml scrape config for CHV on the local host.
# Adjust job names, targets, and TLS settings to match your deployment.

scrape_configs:
  - job_name: chv-controlplane
    static_configs:
      - targets: ['127.0.0.1:9103']   # metrics_bind default

  - job_name: chv-agent
    # Packaged/installer default. Qualification and unset-fallback configs
    # bind 127.0.0.1:9100 instead.
    static_configs:
      - targets: ['127.0.0.1:9901']

  - job_name: chv-stord
    static_configs:
      - targets: ['127.0.0.1:9101']   # metrics_bind default

  - job_name: chv-nwd
    static_configs:
      - targets: ['127.0.0.1:9102']   # metrics_bind default

rule_files:
  - 'monitoring/rules/chv.yml'
  - 'monitoring/rules/recording.yml'
```

To scrape from another host, set that daemon's `metrics_bind` explicitly to
its management address (for example `192.0.2.10:9101`) and restrict access at
the network layer; never bind it to a wildcard/public address. Keep the
target list in this example loopback when using a reverse proxy or an
operator-managed relay instead. See “Scrape authentication model” above.

---

## References

- ADR-009 Logging and observability: [docs/specs/adr/009-logging-and-observability.md](specs/adr/009-logging-and-observability.md)
- ADR-014 API evolution: [docs/specs/adr/014-api-evolution.md](specs/adr/014-api-evolution.md)
- Prometheus rule files: [monitoring/rules/](../monitoring/rules/)
- Prometheus naming conventions: https://prometheus.io/docs/practices/naming/

## Proposed native monitoring design (not yet implemented)

The [native monitoring implementation plan](plans/2026-10-09-native-monitoring-implementation.md) defines a Proxmox-style embedded monitoring experience. The [prompt campaign](prompts/native-monitoring/README.md) describes the staged implementation and qualification.

The proposed designs are [ADR-025](specs/adr/025-native-monitoring-architecture.md), [ADR-026](specs/adr/026-optional-monitor-agent-and-guest-identity.md), and [ADR-027](specs/adr/027-monitoring-history-alerts-and-export.md). They add truthful node and VM measurements, bounded history, optional guest operating-system checks, native alerts, and optional external export. This section does not claim these features already work.
