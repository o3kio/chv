# Monitoring

Prometheus alerting and recording rules for CHV.

## Files

| File | Purpose |
|---|---|
| `rules/chv.yml` | Alerting rules covering the SLOs defined in [docs/OBSERVABILITY.md](../docs/OBSERVABILITY.md) |
| `rules/recording.yml` | Pre-computed recording rules for expensive multi-label queries used by dashboards |

## Validating locally

```bash
# Requires promtool (ships with Prometheus)
promtool check rules monitoring/rules/chv.yml
promtool check rules monitoring/rules/recording.yml
```

Both rules files must pass `promtool check rules` before merging. CI enforces
this in the `promtool` job of the [`CI` workflow](../.github/workflows/ci.yml),
which runs on every pull request and push to `main`. That job downloads the
pinned Prometheus v3.15.1 release, verifies its SHA256 checksum, and runs
`promtool check rules monitoring/rules/*.yml`. It does not depend on the Rust
build.

## Scrape endpoints

All four CHV daemons expose Prometheus text exposition on a dedicated HTTP
listener. The listeners are **unauthenticated and loopback-bound by default**,
so local scraping does not need a bearer token or admin JWT rotation.

| Daemon | Config key | Default bind |
|---|---|---|
| `chv-controlplane` | `metrics_bind` | `127.0.0.1:9103` |
| `chv-agent` | `metrics_bind` | `127.0.0.1:9100` code fallback; packaged/installer `127.0.0.1:9901` |
| `chv-stord` | `metrics_bind` | `127.0.0.1:9101` |
| `chv-nwd` | `metrics_bind` | `127.0.0.1:9102` |

The control plane's older `/metrics` route on `http_bind` remains admin-gated
for compatibility; use the dedicated `metrics_bind` listener for scraping. For
remote scraping, bind `metrics_bind` to a specific management address and
firewall it, or keep loopback and front it with an operator-managed reverse
proxy. Never bind a metrics listener to a wildcard/public address.

```yaml
scrape_configs:
  - job_name: chv-controlplane
    static_configs:
      - targets: ['127.0.0.1:9103']
  - job_name: chv-agent
    static_configs:
      - targets: ['127.0.0.1:9901']
  - job_name: chv-stord
    static_configs:
      - targets: ['127.0.0.1:9101']
  - job_name: chv-nwd
    static_configs:
      - targets: ['127.0.0.1:9102']
```

See [docs/OBSERVABILITY.md](../docs/OBSERVABILITY.md) for the full scrape
authentication model and the metric reference.

## Loading into Prometheus

In your `prometheus.yml`:

```yaml
rule_files:
  - '/etc/prometheus/chv/chv.yml'
  - '/etc/prometheus/chv/recording.yml'
```

Or, if you point Prometheus at this repository directly:

```yaml
rule_files:
  - 'monitoring/rules/chv.yml'
  - 'monitoring/rules/recording.yml'
```

## Alert severity conventions

| Severity | Meaning | Response |
|---|---|---|
| `critical` | Production impact; page on-call immediately | PagerDuty / immediate response |
| `warning` | Degraded; file ticket and investigate | Ticket + investigation within 1 business day |
| `info` | Informational; no action required | Dashboard widget only |

## Adding new alerts

1. Define or update the SLI/SLO in [docs/OBSERVABILITY.md](../docs/OBSERVABILITY.md).
2. Add the alerting rule to `rules/chv.yml`.
3. Add any supporting recording rules to `rules/recording.yml`.
4. Run `promtool check rules monitoring/rules/*.yml` locally.
5. Link a runbook in the alert's `annotations.runbook` field.

## References

- CHV observability contract: [docs/OBSERVABILITY.md](../docs/OBSERVABILITY.md)
- ADR-009 logging and observability: [docs/specs/adr/009-logging-and-observability.md](../docs/specs/adr/009-logging-and-observability.md)
- Prometheus alerting documentation: https://prometheus.io/docs/prometheus/latest/configuration/alerting_rules/
