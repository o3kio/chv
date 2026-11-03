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
