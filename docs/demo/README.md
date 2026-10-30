# Next.js overload demonstration

This is a reproducible experiment that compares two ways of serving the same three Next.js origins when more requests arrive than they can render:

- **Baseline:** NGINX sends requests straight to the origins.
- **Tanod:** the same NGINX sends requests to Tanod, which forwards them to the same origins.

The experiment counts what happens to every request. It records how many succeed, how many are refused or fail, how long successful responses take, and how much work the origins actually perform. The origins report that work themselves, so the result does not depend on Tanod's own metrics.

The harness lives in [`bench/showcase/`](../../bench/showcase). This page explains how to run it, how the experiment is designed, what it can and cannot show, and how to present the results without overstating them.

> [!IMPORTANT]
> This page contains no benchmark results. Run the experiment on your own hardware: every chart and number it produces is derived from files that run writes. If you publish a result, publish it with its `metadata.json`, which records the hardware, versions and parameters.

## Contents

- [What the experiment can and cannot show](#what-the-experiment-can-and-cannot-show)
- [Prerequisites](#prerequisites)
- [Run it](#run-it)
- [Experiment design](#experiment-design)
- [What is measured](#what-is-measured)
- [Outputs](#outputs)
- [Reading the results](#reading-the-results)
- [Choosing the load and the ceiling](#choosing-the-load-and-the-ceiling)
- [Limitations](#limitations)
- [Live dashboard and screenshots](#live-dashboard-and-screenshots)
- [Storyboard for a 60–90 second video](#storyboard-for-a-6090-second-video)
- [Embedding results in a README](#embedding-results-in-a-readme)

## What the experiment can and cannot show

The experiment has two modes. They answer different questions, so label them separately whenever you report a result.

| Mode | What the origin does per request | What it can show | What it cannot show |
| --- | --- | --- | --- |
| `delay` (default) | Waits for 350 ms using the fixture's existing artificial delay, which uses no CPU | That Tanod keeps concurrent origin work under its configured ceiling, sheds excess requests with `503`, and reuses responses for repeated URLs | Anything about saturation. A waiting origin can hold thousands of requests at once, so the baseline may look healthy here, often with better throughput than Tanod |
| `cpu` (opt-in) | Builds and ranks a synthetic 40,000-item catalog, then renders it with React. This is a fixed amount of CPU work that yields between chunks, as streaming renders do | What happens when work really competes for CPU: whether latency, timeouts and wasted work grow without Tanod, and whether limiting admission keeps successful responses fast | How your application behaves. The catalog is a representative stand-in for expensive rendering, not a model of any real site |

**A sleep is not CPU saturation.** A result from `delay` mode demonstrates a mechanism. Do not present it as a performance improvement.

The experiment also uses two workloads:

- **`unique`:** every request asks for a URL that was never requested before, so nothing can be cached or coalesced. This is the only workload that provides evidence of **admission control**. The analysis marks the run invalid if any cache hit appears.
- **`repeated`:** requests cycle through four URLs. Tanod can coalesce identical in-flight requests and serve short-lived cached responses. This workload shows **reuse**, and its results must not be presented as admission control.

## Prerequisites

- Linux or macOS with Docker Engine and the Docker Compose v2 plugin. The stack builds two images: the Next.js fixture and Tanod from this checkout.
- About 4 GB of free memory and at least 6 logical CPUs. By default, each origin, Tanod and NGINX each have a quota of 1 CPU, and k6 runs outside those quotas.
- `python3` 3.9 or newer. Analysis uses only the standard library.
- [k6](https://grafana.com/docs/k6/latest/set-up/install-k6/) on `PATH`. If it is not installed, the script runs the pinned `grafana/k6:2.3.0` image with host networking, which works on Linux.
- Matplotlib for the charts, using one of these options:
  - Install it for `python3`.
  - Set `SHOWCASE_PLOT_PYTHON` to an interpreter that has it.
  - Install [uv](https://docs.astral.sh/uv/), and the script runs `uv run --with matplotlib`.

  Without Matplotlib, the run still writes all data and skips only the charts.
- Free loopback ports: 18480 (edge), 13401–13403 (origin counters), 19490–19491 (Tanod metrics and admin), and, with `--observe`, 19400 and 13400.

## Run it

From the repository root:

```bash
# Mechanism test: delay mode, unique and repeated workloads, both arms (about 8 minutes)
bench/showcase/run.sh

# The constrained, CPU-bound workload
bench/showcase/run.sh --mode cpu

# For results you intend to publish: both modes, three trials, arm order alternating per trial
bench/showcase/run.sh --mode delay,cpu --trials 3

# Validate configuration, images and tools without generating any load
bench/showcase/run.sh --check
```

Common options:

| Option | Default | Meaning |
| --- | --- | --- |
| `--mode delay,cpu` | `delay` | Which origin workload to use |
| `--workload unique,repeated` | both | Which request pattern to use |
| `--rate N` | 120 (`delay`), 90 (`cpu`) | Offered requests per second during the measured phase |
| `--duration S` | 60 | Length of the measured phase, in seconds |
| `--profile constant\|spike` | `constant` | `spike` runs 15 s at a base rate, ramps to `--rate`, holds the rate for `--duration`, ramps down and recovers for 20 s |
| `--ceiling N` | 12 (`delay`), 6 (`cpu`) | Tanod's global concurrency limit for origin requests |
| `--trials N` | 1 | Repeats each case, reversing the arm order on every other trial |
| `--observe` | off | Also starts Prometheus and Grafana, and streams k6 metrics to them |
| `--set name=value` | | Sets any other parameter in [`showcase.py`](../../bench/showcase/showcase.py) (`DEFAULTS`), for example `--set tanod_cache=false` or `--set nginx_limit_key=ip` |
| `--out DIR` | `benchmark-results/showcase/<run id>` | Output directory. The default location is ignored by Git |
| `--analyze DIR` | | Re-analyses an existing run and redraws its charts without generating load |

Stop a run with Ctrl-C at any time. The script removes the containers it started on every exit path, unless you pass `--keep`.

## Experiment design

```text
                     baseline arm                              Tanod arm
k6 (open model) ──► NGINX ──► next-1, next-2, next-3     k6 ──► NGINX ──► Tanod ──► next-1, next-2, next-3
                     │                                             │
                     rate limit, keep-alive, timeouts               identical NGINX settings
```

### Matched conditions

| Matched item | How it is enforced |
| --- | --- |
| Next.js version, Node version and build | One image (`tanod-next-origin:showcase`), built once from [`fixtures/next-storefront`](../../fixtures/next-storefront) and used by both arms. Its versions are recorded in `metadata.json` |
| Origin resources | Each origin has `cpus: 1.0` and `mem_limit: 768m` in both arms ([`compose.yaml`](../../bench/showcase/compose.yaml)). A test checks that the three origin definitions are identical |
| Origin configuration | The same environment in both arms. The only per-origin value is `INSTANCE_ID` |
| NGINX settings | Both arms render NGINX config from one template ([`nginx.conf.tpl`](../../bench/showcase/nginx.conf.tpl)). A test checks that the only difference between the arms is the upstream block. Proxy timeouts default to NGINX's own 60 s, as in the repository's existing [`ops/edge/nginx.conf`](../../ops/edge/nginx.conf) |
| NGINX rate limit | The same `limit_req` zone, rate (20 r/s), burst (40) and key in both arms. Rejections return `429`, so they are never mistaken for Tanod's `503` |
| Load distribution | NGINX round robin in the baseline arm; Tanod `load_balancing: round_robin` in the Tanod arm |
| Request rate and URLs | The same k6 script, schedule and URL sequence. The URL namespace changes per trial, not per arm |
| Warming | Each arm sends 15 s at 5 req/s to warmup URLs that the measured phase never requests. The run then waits until every origin reports zero renders in progress |
| Isolation | Every case starts from freshly created containers and is torn down afterwards. The arms never run at the same time |

### Sequence for each case

Each case is one combination of mode, workload, arm and trial, and runs these steps:

1. The script renders the configuration and validates it with `nginx -t` and `tanod check`.
2. It starts the origins and waits for their health checks to pass. In the Tanod arm, it then starts Tanod and waits for `/health/ready`. Finally, it starts NGINX and waits for a response through it.
3. Warmup runs, followed by a wait until the origins drain.
4. The script takes a "before" snapshot of the origin and Tanod counters and resets the origins' peak window.
5. The measured phase runs while the sampler polls the origin counters, Tanod's gauges and the container statistics.
6. Drain: after k6 finishes, the script keeps sampling until every origin reports zero renders in progress, for up to 60 s. Without Tanod, origins can still be working on requests whose clients have already given up. The drain time is recorded.
7. The script takes an "after" snapshot, saves container state (including `OOMKilled`) and logs, analyses the case, and tears the stack down.

The load is an **open model**: k6 starts requests on a fixed schedule (`constant-arrival-rate` or `ramping-arrival-rate`) whether earlier requests have finished or not. Real visitors behave this way. A closed-loop benchmark, in which each client waits for its previous response, slows down whenever the server does, which hides overload. If k6 runs out of virtual users (`max_vus`, default 2,000), it *drops* the iteration, and the analysis reports dropped iterations as load that was offered but never sent.

## What is measured

| Measurement | Source | Where it appears |
| --- | --- | --- |
| Scheduled iterations, executed iterations and dropped iterations | k6 schedule and summary | `results.json` → `scheduled_iterations`, `iterations`, `dropped_iterations`, `attempted_iterations` |
| HTTP 200 from origin, HTTP 200 from Tanod cache, 503 shed, 429, 5xx, connection errors and timeouts | k6, classified once per response by status code and the `X-Tanod` header | `outcomes.*` |
| Successful-response p50, p90, p95, p99 and maximum latency | k6, HTTP 200 only | `ok_ms` (all 200s), `ok_origin_ms` (200s that were not cache hits) |
| Peak concurrent renders across all origins | The origins' own `render_start`/`render_end` log lines. Each line carries that origin's count of renders in progress, and Docker timestamps it, so merging the three logs gives the exact total at every change | `origin.peak_total_concurrent` (`peak_total_source` names the source) |
| Peak concurrent renders at each origin | The same logs, plus the origin's stats endpoint, which reports the exact peak since the previous read | `origin.peak_per_origin_from_logs`, `origin.per_origin.*.peak_concurrent` |
| Fallback when logs are unavailable | The sampled sum of the three origins' in-progress counts (a lower bound) and the sum of the per-origin window peaks (an upper bound) | `origin.peak_total_active_sampled`, `origin.peak_total_window_upper_bound` |
| Origin renders | The origins' own counters (after − before) | `origin.renders`, `origin.renders_per_origin` |
| Renders that did not produce an HTTP 200 (`unique` workload) | Origin renders − HTTP 200s from origin | `origin.renders_not_delivered_as_200` |
| Time for the origins to finish outstanding renders after load stopped | Origin counters | `origin.drain_seconds_after_load` |
| Tanod admission, shedding, cache outcomes and origin requests | Tanod `/metrics` (after − before) | `tanod.admission`, `tanod.cache`, `tanod.origin_requests`, `tanod.responses` |
| Queueing | Live queue depth from Tanod's admin `/status`, summed over every limiter, because a request waits in whichever limiter refused it first. Queue-wait percentiles are given as the upper bound of the histogram bucket that holds them | `tanod.peak_queue_depth_sampled`, `tanod.queue_wait_p95_at_most_s`, `tanod.queue_wait_buckets` |
| CPU and memory | Docker Engine statistics per container: cumulative CPU time and memory excluding page cache | `resources.<container>.cpu_seconds`, `mean_cores`, `peak_cores`, `peak_memory_mib` |
| Why NGINX failed upstream requests | NGINX's error log, counted by reason (for example `no live upstreams` or `upstream timed out ... while connecting to`) | `nginx_errors`, and the case notes |
| Stack isolation | `docker inspect` at the end of each case: which services existed, when they were created, OOM kills and exit states | `containers`, check `isolated_fresh_stack` |
| Environment | Git commit and dirty state, image IDs, Next.js, Node, NGINX, Tanod, k6 and Docker versions, CPU model, CPU count, memory, kernel, CPU governor and load average | `metadata.json` |

The origins' counters come from an opt-in fixture endpoint, `/api/fixture-stats`, which is enabled only when `TANOD_FIXTURE_STATS=1`. The CPU workload is the opt-in route `/catalog/[slug]`, enabled only when `TANOD_FIXTURE_CPU_ROUTE=1`. No other suite in the repository enables either one.

## Outputs

```text
benchmark-results/showcase/<run id>/
├── params.json            every parameter used
├── metadata.json          hardware, versions, images, Git commit
├── results.json           every case, plus per-workload comparisons
├── results.csv            one row per case (columns listed in showcase.py CSV_COLUMNS)
├── summary.md             generated table of the measured results, with recorded notes
├── charts/                SVG + PNG for each chart, light and dark, plus manifest.json
│   ├── outcomes-<mode>         what happened to every attempted request
│   ├── latency-<mode>          successful-response p50/p95/p99
│   ├── origin-concurrency-<mode>   renders in progress over time, with the Tanod ceiling
│   ├── goodput-<mode>          HTTP 200 per second over time
│   └── origin-work-<mode>      origin renders compared with successful responses
├── config/                rendered NGINX and Tanod configs per mode and arm
└── cases/<mode>-<workload>-<arm>-t<trial>/
    ├── case.json          the analysed case
    ├── k6-summary.json, k6-warmup-summary.json, k6-requests.csv.gz
    ├── samples.csv        sampler time series (long format: time, source, metric, value)
    ├── timeline.csv       per-second outcomes and successful-response latency
    ├── origin-concurrency.csv  total renders in progress (max per 100 ms), from the origin logs
    ├── snapshot-before.json, snapshot-after.json, drain.json, containers.json
    └── logs/              k6, NGINX, Tanod and origin logs (gzip)
```

`charts/manifest.json` lists the cases behind each chart. The charts read only `results.json` and the case files. They do not interpolate, smooth or fill in missing data, and a missing sample appears as a gap.

## Reading the results

Each case records checks and notes. `summary.md` lists them, and a failed check means the case cannot support the claim the check names.

| Check | Case | What it means |
| --- | --- | --- |
| `isolated_fresh_stack` | Every case | Every container was created after the case started, and the baseline arm had no Tanod container. If this fails, state from an earlier case could have leaked into this one |
| `no_cache_reuse` | `unique` workload | No response was served from cache. If this fails, the case is not evidence of admission control |
| `ceiling_held` | Tanod arm | The origins' own peak counts and Tanod's sampled in-flight work never exceeded the configured ceiling |
| `load_exceeded_ceiling` | Baseline arm, `unique` workload | Without Tanod, the origins actually ran more concurrent work than the ceiling. If this fails, the offered load was too low for the comparison to show anything |

When you read the results, keep these points in mind:

- **Shed requests are a cost, not a free win.** With Tanod, some visitors receive `503` with `Retry-After`. Not every shed is instant: a request shed when the queue is full is answered at once, but one shed because its queue wait timed out has already waited `tanod_queue_timeout`. `tanod.admission` separates the two causes, and `shed_ms` gives the latency of shed requests. The useful comparison is how many visitors got a fast, successful response, how many waited until they timed out, and how much origin work was wasted on responses nobody received. Always report shed counts next to success counts.
- **Queue time is part of Tanod's latency.** A request admitted after waiting in Tanod's queue carries that wait in its response time, which is bounded by `tanod_queue_timeout` (default 1 s). A shorter queue lowers successful-response latency and sheds more requests. A longer queue does the reverse. Report the queue settings with the result.
- **Throughput can be lower with Tanod.** This is expected in `delay` mode and whenever the ceiling is set below the origin's real capacity. A ceiling that is set too low turns away work the origin could have handled.
- **Report successful-response latency together with the success count.** A p95 calculated over a handful of successful responses describes very little. Every latency in the outputs comes with its count.
- **Dropped iterations count against the arm that caused them.** If the baseline makes k6 run out of virtual users, those requests were offered and never served.
- **Use the median of several trials.** This desktop-class experiment is noisy. Run at least three trials (`--trials 3`) and report the median with the range. `results.json` → `comparisons` holds the medians.

## Choosing the load and the ceiling

The defaults were chosen so that, on a 6-core desktop with 1 CPU per origin, offered load is roughly three times the origins' capacity in `cpu` mode. They are starting points, not tuned values. The time each render takes, and therefore the capacity, depends on the host's CPU.

1. Run a short, low-rate baseline and read the warmup latency. `results.json` → `warmup.ok_origin_p50_ms` gives the unloaded service time.
2. The approximate capacity in `cpu` mode is `origins × cpus_per_origin / service_time`. For example, 3 origins × 1 CPU / 50 ms gives about 60 req/s. To demonstrate overload, offer two to three times that rate.
3. Set the Tanod ceiling near the number of renders the origins can make progress on at once. For CPU-bound work, that is about one or two per CPU. Treat the ceiling as part of the experiment and report it.
4. Confirm that `load_exceeded_ceiling` passed for the baseline arm.

## Limitations

- **This is one host.** k6, the sampler, NGINX, Tanod and the origins share one machine, and Docker CPU quotas limit CPU time without pinning cores. Background load affects every run, so `metadata.json` records the load average at the start. Treat results from a laptop or desktop as illustrative.
- **The Tanod arm has an extra process.** Tanod has its own 1-CPU quota, and its CPU time is reported in `resources.tanod`. The comparison is therefore between an origin pool with an admission-control proxy in front and the same pool without one, not between two systems that use equal hardware.
- **The workloads are synthetic.** The CPU workload is a stand-in for expensive rendering. It does not include database calls, network fan-out or memory pressure from a real application. The delay workload is not a saturation test at all.
- **The baseline is a common default, not a tuned NGINX deployment.** NGINX can bound upstream work with `max_conns` and `queue` (the `queue` directive is available only in NGINX Plus) or with per-location limits. The baseline uses neither, because many deployments do not. A comparison against a tuned NGINX would be a different experiment.
- **NGINX's passive health check shapes the baseline's failures.** With the default `max_fails=1 fail_timeout=10s`, a single failed connection takes an origin out of rotation for 10 s. When every origin is backlogged, NGINX answers `502` (`no live upstreams`) without contacting any of them. The result depends on `proxy_connect_timeout`: with a short value (`--set nginx_connect_timeout=2s`), backlogged origins fail fast and the baseline reports `502` responses; with NGINX's default of 60 s, requests wait and the baseline reports timeouts. Both are real NGINX behaviour. Keep the default unless you are deliberately testing the other, and report which one you used.
- **The rate limit is per visitor.** It is keyed on a synthetic visitor ID that k6 sends, which models many distinct visitors, so it does not trigger at the default load. That is the point: a per-visitor rate limit does not bound aggregate origin work. With `--set nginx_limit_key=ip`, every request comes from one address and the limit applies to all traffic at once, which is a different experiment.
- **Debug headers are enabled.** The Tanod config sets `debug_headers: true` so k6 can tell cache hits apart from origin renders. The header does not change any admission or caching decision.
- **Origin concurrency counts renders, not queued connections.** The fixture counts a render from the start of page work to its end. Requests that Node has accepted but not yet started rendering are invisible to the counter. That is why, without Tanod, the origins can keep a steady number of renders in progress for many seconds after the load stops: they are working through a backlog nobody is waiting for any more. `drain_seconds_after_load` measures that time.
- **Sampling has limits.** An overloaded origin answers its stats endpoint late or not at all, which is why concurrency totals come from the origin logs. Container CPU and memory, and Tanod's queue depth, are still sampled every 0.5 s, and failed reads are counted in the case notes.
- **Queue-wait percentiles are reported as histogram bucket bounds** ("p95 was at most 1 s"), not as interpolated values. Interpolating inside a wide bucket invents numbers: with a 1 s queue timeout, interpolation can "estimate" 2 s.
- **The experiment measures HTTP/1.1 only, behind one ingress.** It does not cover HTTP/2 multiplexing, TLS, CDNs or multi-instance Tanod deployments.

## Live dashboard and screenshots

Use `--observe` to start Prometheus and Grafana alongside the experiment:

```bash
bench/showcase/run.sh --mode cpu --workload unique --observe
```

To see the live view, open <http://127.0.0.1:13400/d/tanod-showcase>. The repository's operator dashboard (`tanod-overview`) is also provisioned. The live view is for watching and recording only: published numbers must come from the run's files, because scrape gaps and rate windows smooth the live view. Running the observability stack adds CPU load to the host, and `metadata.json` records whether it was running.

The demo dashboard [`bench/showcase/observe/dashboard.json`](../../bench/showcase/observe/dashboard.json) has three rows:

1. **Visitors:** offered and answered requests per second, requests that were turned away or failed, successful-response p50 and p95, and cache hits. All panels use k6 metrics tagged `arm=baseline|tanod`.
2. **Origins:** renders in progress as reported by the Next.js processes, with Tanod's ceiling shown as a dashed line, and renders per second per origin.
3. **Tanod:** in-flight work compared with the ceiling, queue depth, admission decisions, cache outcomes, queue-wait p95 and responses by source. This row has data only during the Tanod arm.

Screenshot guidance:

- Capture one arm at a time, with the time range set to the measured phase plus about 10 seconds on each side. Put the arm and mode in the caption, such as "Baseline, `cpu` mode, unique URLs, 90 req/s".
- Capture the Tanod arm with the same time span and panel layout so that the two images can be compared directly.
- Keep panel titles, axes and units visible. Do not crop out the ceiling line, the shed series or the timeout series.
- If you annotate a screenshot, quote numbers from `summary.md` or `results.json` for the same run, not values read off a graph.
- Grafana starts in its dark theme. For light-background documents, switch the theme in your user preferences instead of editing the image.

## Storyboard for a 60–90 second video

Record a real run, `--mode cpu --workload unique --observe`, with a screen recorder. Do not speed up one arm relative to the other. If you speed up the footage, apply the same factor to the whole video and say so on screen.

| Time | Picture | Narration or caption |
| --- | --- | --- |
| 0:00–0:08 | Title card: "Three Next.js origins, one traffic spike, with and without Tanod" | "Same origins, same NGINX, same load. One difference: Tanod in the middle." |
| 0:08–0:18 | The topology diagram from this page | "Every request asks for a page no one has seen before, so caching cannot help. Only admission control can." |
| 0:18–0:38 | Grafana during the baseline arm: renders in progress climbing, then p95 latency and timeouts rising | "Without Tanod, every request reaches the origins. Work piles up, every render slows down, and visitors start timing out." Show the counts from `summary.md` for this run. |
| 0:38–0:58 | Grafana during the Tanod arm: renders held at the dashed ceiling, shed requests visible, p95 latency flat | "With Tanod, origin work stays at the ceiling. Excess requests get a `503` with `Retry-After`, within the bounded queue wait, instead of an unbounded one." Name the shed count and its latency (`shed_ms`) explicitly: requests shed because the queue timed out waited for the full queue timeout first. |
| 0:58–1:15 | The `outcomes-cpu` and `origin-concurrency-cpu` charts from the same run, side by side | "The same run, from its result files: what happened to every request, and what the origins actually did." |
| 1:15–1:30 | Text card listing the limitations: one host, synthetic CPU workload, shed requests are a real cost, run it yourself, and the `bench/showcase/run.sh` command | "Run it on your own hardware with one command." |

Do not include `delay`-mode footage in a performance claim. If you show it, caption it "mechanism test: the origin waits instead of computing".

## Embedding results in a README

Commit charts only from a run that you are willing to stand behind. That means at least three trials, all checks passing, and its `metadata.json` and `results.csv` committed next to the charts. Then use a `<picture>` element so that GitHub serves the dark variant in dark mode:

```html
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/demo/results/outcomes-cpu-dark.svg">
  <img alt="Outcome of every request at 90 req/s against three CPU-bound Next.js origins, with and without Tanod"
       src="docs/demo/results/outcomes-cpu.svg" width="900">
</picture>
```

Put the conditions right under the image, with numbers copied from that run's `summary.md`:

```markdown
<sub>CPU-bound Next.js fixture, 3 origins × 1 CPU, 90 req/s unique URLs for 60 s, Tanod ceiling 6,
median of 3 trials on <CPU model>. Shed requests received 503 + Retry-After.
[Method and limitations](docs/demo/README.md) · [raw results](docs/demo/results/results.csv)</sub>
```

When you write the caption:

- Name the mode. A chart from `delay` mode needs "mechanism test" in its caption.
- Report successful responses, shed requests and timeouts together. Do not report a latency percentile without the success count behind it.
- Link to this page for the method and the limitations.
- Do not compare the outcome to production numbers or extrapolate it to other applications.
