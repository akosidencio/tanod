# Operator artifacts

These artifacts monitor Tanod as an overload governor for expensive SSR and
dynamic origin workloads. They do not treat cache-hit rate or lower latency as
the product goal; the primary signals are bounded origin work, queueing,
shedding, origin health and recovery.

Prometheus loads alert rules and compact recording rules for local and replica
group saturation, shedding, origin latency, avoided origin work, and
cache/spool occupancy.

Three files, all meant to be copied into your own monitoring stack rather than
imported and forgotten.

## Local end-to-end demo

The repository includes a self-contained deployment that proves the metrics,
rules, and dashboard work together:

```bash
docker compose -f compose.observability.yaml up --build
```

The stack runs three Next.js fixture origins behind Tanod. A traffic container
continuously exercises reusable, dynamic, private, and queueing routes while
Prometheus scrapes Tanod and Grafana provisions this dashboard automatically.

| Service | Local address |
|---|---|
| Tanod | <http://127.0.0.1:18080> |
| Prometheus | <http://127.0.0.1:19000> |
| Grafana dashboard | <http://127.0.0.1:13000/d/tanod-overview/tanod> |

Grafana is included in the Compose stack, so the demo does not require a local
Grafana installation. The first build compiles the Rust and Next.js images;
later starts reuse Docker's build cache.

Capture the same real dashboard images used in the main README after the stack
has collected data:

```bash
npm --prefix bench/browser ci
npm exec --prefix bench/browser -- playwright install chromium
node scripts/capture-dashboard.mjs
docker compose -f compose.observability.yaml down
```

The dependency and browser installation commands are only needed once. The
capture writes `assets/tanod-dashboard.png` plus
`assets/tanod-dashboard-full.png`.

## `prometheus/alerts.yml`

Alerting rules, grouped by the question they answer: availability, origin load,
resources, configuration.

```yaml
# prometheus.yml
rule_files:
  - /etc/prometheus/rules/tanod-alerts.yml

scrape_configs:
  - job_name: tanod
    static_configs:
      - targets: ["tanod-1:9090", "tanod-2:9090"]
```

The rules assume `job="tanod"`. Every other label they use — `route`,
`upstream`, `limiter`, `decision`, `status`, `reason`, `outcome` — is
config-derived on Tanod's side and never client-controlled, so no rule here
can be made expensive by traffic.

**Every threshold is a starting point, not a recommendation.** How much load
shedding counts as normal, and how long a drain may last, are properties of
your deployment. They are written as obvious knobs for that reason.

Check them before shipping:

```bash
promtool check rules ops/prometheus/alerts.yml ops/prometheus/recording-rules.yml
```

## `grafana/dashboard.json`

Import through **Dashboards → New → Import**; it asks for a Prometheus
datasource.

Panels are ordered by the question an operator actually asks, health first:
whether anything is draining or unhealthy, then whether the origin ceiling is
holding, then what is being reused, then telemetry. Two conventions worth
knowing when you extend it:

- **One axis per panel.** Where two series share a panel they share a unit —
  in-flight against its ceiling, occupancy against its budget. A second y-axis
  makes two unrelated scales look correlated, and that is the single most
  common way a dashboard misleads.
- **A budget is published as a metric**, not hardcoded in a panel.
  `tanod_cache_max_bytes` and `tanod_spool_max_bytes` come from the running
  config, so a panel cannot go stale the first time somebody edits it.

The four signals worth understanding before an incident are in
[`../docs/OPERATIONS.md`](../docs/OPERATIONS.md#what-to-alert-on).
