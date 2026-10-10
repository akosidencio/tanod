# Operating Tanod

Tanod is an overload governor for expensive SSR and dynamic origin workloads,
not a general Next.js performance accelerator. Operate it around origin-work
ceilings, queueing, shedding and recovery; a cache hit rate or latency change
alone does not establish that the deployment benefits from Tanod.

Everything here is asserted by a script in [`bench/`](../bench) rather than
described from memory. Where a number appears, the benchmark that produced it
is named.

- [The operator surface](#the-operator-surface)
- [Restarting without dropping requests](#restarting-without-dropping-requests)
- [systemd](#systemd)
- [Kubernetes](#kubernetes)
- [Reloading configuration](#reloading-configuration)
- [Sizing the queue](#sizing-the-queue)
- [Tracing and correlation](#tracing-and-correlation)
- [What to alert on](#what-to-alert-on)

---

## The operator surface

Three endpoints on a listener of their own:

```yaml
telemetry:
  admin:
    listen: "127.0.0.1:9091"
    # Report not-ready when no upstream is passing its health check.
    # Off by default — see the warning below.
    require_healthy_upstream: false
```

| Endpoint | Answers |
|---|---|
| `GET /health/live` | `200` while the process is running. Never anything else. |
| `GET /health/ready` | `200` when this instance should receive traffic, `503` while draining (and, if configured, while no upstream is healthy). |
| `GET /status` | Configuration generation and stable fingerprint, per-backend health, cache and spool occupancy, admission limits and in-flight counts, drain state, compiled features. |

`/healthz` and `/readyz` are accepted as aliases, because half the tooling in
the world spells them that way.

**Bind it to loopback or a private address.** `/status` publishes backend
health, cache occupancy and your configuration generation. `tanod check`
warns when it is bound to an unspecified address, and startup refuses to share
an address with the traffic listener or the metrics listener.

Nothing on this surface is parameterised. There is no path segment, query key
or header that changes what is computed — the same rule the Prometheus labels
follow, and for the same reason: an operator surface must never become a way to
make the process do unbounded work during the incident it exists to explain.

**Liveness is not readiness.** A draining instance answers `200` on
`/health/live` and `503` on `/health/ready`. Wiring an orchestrator's liveness
probe to the readiness endpoint makes it kill the process part-way through a
drain, which is precisely the dropped request the drain existed to prevent.

**`require_healthy_upstream` is off by default and the default is the careful
one.** Tanod keeps serving a fully unhealthy pool — refusing to pick a
backend would turn a degraded origin into a guaranteed outage, and
`stale_if_error` exists for that window. Turn it on only when something above
Tanod can route around this instance; otherwise a single origin problem takes
every replica out of rotation at once.

When enabled, it requires a `health:` block and readiness starts at `503`.
Tanod does not call an unprobed backend healthy: `/health/ready` changes to
`200` only after at least one backend completes its configured successful-probe
streak.

Evidence: [`bench/admin.sh`](../bench/admin.sh) and
[`bench/chaos.sh`](../bench/chaos.sh), which samples `/health/live` and
`/status` *during* an origin outage rather than after it.

---

## Restarting without dropping requests

There are two mechanisms and they are not interchangeable.

### Socket handover — Linux only

Pingora passes listening file descriptors between processes over a Unix socket
with `SCM_RIGHTS`. **This works on Linux and nowhere else**: its non-Linux
`get_fds_from` is a stub that logs `Upgrade is not currently supported` and
returns `ECONNREFUSED`. Tanod refuses `--upgrade` up front on other
platforms rather than letting that surface as a connection error that reads
like "the old process is not running".

```bash
# 1. Prove the new binary and config can actually start. Non-zero here means
#    stop: you have not touched the running process yet.
tanod run --config /etc/tanod/tanod.yaml --test

# Obtain this from the supervisor, a captured `$!`, or the pid file written by
# --daemon. Foreground Tanod does not write server.graceful.pid_file.
pid=${TANOD_PID:?set TANOD_PID to the running Tanod process}

# 2. Start the new process. It takes the listening sockets over the
#    upgrade socket; both processes must name the same path.
tanod run --config /etc/tanod/tanod.yaml --upgrade &

# 3. Tell the old one to hand over and drain.
kill -QUIT "$pid"
```

The old process keeps serving everything it had already accepted and exits when
those finish. No connection is refused in between — asserted under continuous
traffic by [`bench/upgrade.sh`](../bench/upgrade.sh).

### Drain and replace — everywhere

Without a socket handover there is a window in which nothing owns the port. The
drain window is what lets a load balancer withdraw the instance *before* that
window opens.

```bash
# 1. Start draining. Readiness begins answering 503 immediately; the process
#    keeps serving normally.
kill -USR1 "$pid"

# 2. Wait for your load balancer to notice. Longer than its check interval
#    times its unhealthy threshold.
sleep 15

# 3. Stop, then start the replacement.
kill -TERM "$pid"
```

`SIGUSR1` drains **without exiting**. That is the whole point: the instance
keeps answering requests correctly for as long as you leave it there.

### `shutdown_timeout` is a floor, not a ceiling

The one genuinely surprising thing in this document, and it is measured rather
than assumed — see the `shutdown_seconds` result in
[`bench/upgrade.sh`](../bench/upgrade.sh).

After the drain, Pingora closes the listeners and gives requests already in
flight `shutdown_timeout` to finish; then a one-second teardown cancels
whatever is left. Pingora has no early exit from that window, so it runs to
completion **whether or not anything is still in flight**:

```
time from SIGTERM to exit  ≈  drain_period + shutdown_timeout + 1s
```

on a completely idle process. (Before 0.3.0 the window went to the teardown
instead, which cancelled every in-flight request the moment the drain ended.)
With `origin.command`, the origin is stopped only after this, so a request in
flight reaches it to the end. Two things follow:

- **Your supervisor's stop timeout must exceed that sum.** The defaults —
  `drain_period: 5s`, `shutdown_timeout: 10s` — total about 16 seconds, which fits
  inside Kubernetes' default `terminationGracePeriodSeconds: 30` and systemd's
  default `TimeoutStopSec=90`. Raise these two and you must raise those, or the
  supervisor `SIGKILL`s Tanod mid-drain.
- **Set `shutdown_timeout` just above your slowest response.** Requests
  longer than it are cut at the teardown, and a larger value buys nothing
  else: every restart pays it in full. `tanod check` prints
  the total and warns above 30 seconds.

If `SIGUSR1` already started draining, the two windows overlap: `SIGTERM` waits
only the unelapsed part of `drain_period`. A pre-stop hook that sleeps longer
than the drain period therefore pays `preStop sleep + shutdown_timeout`, not a
second full drain.

---

## systemd

Release archives include the ready-to-install unit from
[`packaging/systemd/tanod.service`](../packaging/systemd/tanod.service).
The expanded example below documents each operational setting.

```ini
[Unit]
Description=Tanod origin workload governor
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=tanod
Group=tanod
RuntimeDirectory=tanod
RuntimeDirectoryMode=0750

# Refuses to start on a bad config, before the old unit is stopped.
ExecStartPre=/usr/local/bin/tanod check --config /etc/tanod/tanod.yaml
ExecStart=/usr/local/bin/tanod run --config /etc/tanod/tanod.yaml

# SIGHUP reloads policy in place. An invalid config is refused and the
# running one keeps serving, so a failed reload is not a failed service.
ExecReload=/bin/kill -HUP $MAINPID

# Must exceed server.graceful.drain_period + shutdown_timeout. See the note
# above: that sum is spent on every stop, idle or not.
KillSignal=SIGTERM
TimeoutStopSec=45
Restart=on-failure
RestartSec=2

# Nothing here needs to write outside its runtime directory.
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/run/tanod
# Binding 80/443 without running as root.
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

with

```yaml
server:
  graceful:
    pid_file: /run/tanod/tanod.pid
    upgrade_socket: /run/tanod/upgrade.sock
```

For a zero-downtime deploy on Linux, do the handover by hand rather than
through `systemctl restart` — systemd stops the unit before starting the new
one, which is the gap the handover exists to remove. Either run the three-step
sequence above against the running unit, or use a socket-activated
`Type=notify` arrangement.

---

## Kubernetes

```yaml
apiVersion: apps/v1
kind: Deployment
spec:
  strategy:
    type: RollingUpdate
    rollingUpdate:
      maxUnavailable: 0
      maxSurge: 1
  template:
    spec:
      # The SIGUSR1 drain overlaps the preStop sleep. Budget the larger of the
      # two, then shutdown_timeout. Here: max(15, 5) + 10 = 25.
      terminationGracePeriodSeconds: 35
      containers:
        - name: tanod
          image: ghcr.io/OWNER/tanod:0.1.1
          args: ["run", "--config", "/etc/tanod/tanod.yaml"]
          ports:
            - { name: http, containerPort: 8080 }
            - { name: admin, containerPort: 9091 }
            - { name: metrics, containerPort: 9090 }

          # Readiness gates Service endpoints. This is what actually stops
          # traffic arriving during a drain.
          readinessProbe:
            httpGet: { path: /health/ready, port: admin }
            periodSeconds: 2
            failureThreshold: 2

          # Liveness must NOT point at /health/ready. A draining pod is alive;
          # killing it here undoes the drain.
          livenessProbe:
            httpGet: { path: /health/live, port: admin }
            periodSeconds: 10
            failureThreshold: 3

          # Kubernetes removes the pod from endpoints and sends SIGTERM at the
          # same time, and endpoint propagation is not instant. The sleep is
          # the window in which kube-proxy on every node catches up; Tanod's
          # own SIGTERM handling then fails readiness and drains. The image
          # has no shell, so this is the built-in sleep action (Kubernetes
          # 1.30+), not an exec of `sleep`.
          lifecycle:
            preStop:
              sleep: { seconds: 15 }

          resources:
            requests: { cpu: "500m", memory: "512Mi" }
            # Must exceed cache.max_memory + spool.max_memory plus headroom.
            limits: { memory: "1Gi" }
```

The arithmetic that matters, in one place:

```
terminationGracePeriodSeconds  >  max(preStop sleep, drain_period) + shutdown_timeout
memory limit                   >  cache.max_memory + spool.max_memory + ~128Mi
```

The second is asserted by [`bench/memory.sh`](../bench/memory.sh), which drives
every configured budget past its limit at once and measures resident set size.

`maxUnavailable: 0` matters more than usual here. Each replica has its own
process-local cache and its own admission ceiling, so a replacement pod starts
cold: it has no cached entries and its first requests all reach the origin.
Rolling one pod at a time keeps that surge bounded.

---

## Reloading configuration

`SIGHUP` re-reads the file. **An invalid config is refused and the running one
keeps serving** — reloads happen during incidents, and half-applying one then
is worse than not reloading at all.

Confirm it applied, from outside the process:

```bash
curl -s localhost:9091/status | grep -o '"\(generation\|fingerprint\)":[0-9]*'
# or
curl -s localhost:9090/metrics | grep 'tanod_config_\(generation\|fingerprint\)'
```

A refused reload leaves both values unchanged. Generation distinguishes
requests on either side of a reload within one process; the stable fingerprint
compares effective configuration across replicas regardless of their restart
or reload counts.

Some settings are **startup-bound and a reload refuses rather than ignores
them**: listeners, TLS, `trusted_proxies`, `origin.upstreams`,
`origin.load_balancing`, `origin.http_version`, `origin.breaker`,
`origin.retry`, `spool.max_memory`, `upgrade.max_concurrent`, the cache budget,
`cache.eviction`, `cache.tag_header`, `cache.purge.token`,
`timeouts.origin`, `server.graceful`, `telemetry.admin`, and
`telemetry.tracing.otlp`. A reload that reported success while the old trust
policy stayed in force would be the worst version of this, so it says so
instead.

`origin.breaker` and `origin.retry` are on that list because each carries a
rolling window built at startup. Swapping one while requests are in flight
would leave two windows disagreeing about what has been observed or spent,
which is not a threshold and not a budget.

What **does** reload is the set you actually reach for mid-incident:
`origin.concurrency`, per-route `concurrency`, `origin.priorities`,
`route.priority` and `route.weight`, every cache and coalescing setting,
`spool.enabled`, `telemetry.tracing.sample` and `trust_incoming`. Raising a
ceiling or moving a priority share is an incident action, so it happens without
a restart. Limiters are **resized** rather than replaced, so in-flight permits
are never double-counted across the change.

---

## Invalidating the cache

`POST /purge` on the **admin listener**, authorised by `cache.purge.token`.
Without a token the endpoint does not exist — a `404`, not a `401`. That is the
only safe default: purging makes the origin re-render on demand, so an open
purge endpoint is a stampede trigger anybody can pull.

```bash
# One tag, or several.
curl -X POST -H "Authorization: Bearer $TANOD_PURGE_TOKEN" \
  "http://127.0.0.1:9091/purge?tag=product-42&tag=collection-sale"

# One path, and every variant of it — query strings, Accept, the RSC payload
# beside the HTML. This is what revalidatePath() means by a path.
curl -X POST -H "Authorization: Bearer $TANOD_PURGE_TOKEN" \
  "http://127.0.0.1:9091/purge?path=/products/iphone"

# Both at once, so a deploy hook is one call rather than two.
curl -X POST -H "Authorization: Bearer $TANOD_PURGE_TOKEN" \
  "http://127.0.0.1:9091/purge?tag=collection-sale&path=/products/iphone"

# Everything. Expect an origin load spike proportional to your traffic.
curl -X POST -H "Authorization: Bearer $TANOD_PURGE_TOKEN" \
  "http://127.0.0.1:9091/purge?all=1"
```

It answers with what it removed:

```json
{"purged":true,"scope":"selective","tags":2,"paths":1,"entries":17,"bytes":410233,"remaining_entries":128}
```

Four things worth knowing before you rely on it:

- **`POST` only.** A `GET` that invalidates a cache gets fetched by crawlers,
  link prefetchers and browser history. A `GET /purge` answers `405`.
- **A misspelled parameter is a `400`, not a silent success.** `?tags=x` is
  refused rather than treated as "purge nothing", on the same reasoning as
  unknown keys in the config file: an invalidation that quietly does nothing
  looks like a working one until somebody checks.
- **Purge is per process.** Every replica has its own cache and endpoint. Fan
  out from the deploy pipeline; `@tanod/next` accepts all admin listeners
  through `endpoints` or `TANOD_PURGE_URLS` and fails on partial delivery.
- **In-flight renders keep streaming, but are not admitted afterward.** A
  matching render already streaming to a client is allowed to finish, while
  its temporary fill is marked invalid so it cannot repopulate the cache after
  the purge has reported success.

### Purging by path

`?path=` matches the request path **exactly**, and matches every entry
answering it. A single route is several cache entries — a query variant, an
`Accept` variant, the RSC flight payload beside the HTML — and invalidating the
page means invalidating all of them.

Four constraints worth knowing:

- **Exact, not a prefix.** `?path=/products` does not touch
  `/products/iphone`. There is no equivalent of
  `revalidatePath('/products/[slug]', 'page')` — matching a dynamic route
  pattern needs route metadata Tanod does not have until phase 6.
- **Absolute and query-encoded.** Parameter values are percent-decoded exactly
  once. The decoded value must equal the stored request path, including that
  path's own encoding. For example, purging a stored `/products/a%2Fb` uses
  `?path=/products/a%252Fb`. A relative path is a `400` rather than a purge
  that silently matches nothing.
- **Across hosts.** A path purge matches that path on every virtual host this
  instance serves. It over-purges rather than under-purges: the cost is origin
  work, never stale content.
- **Paths over 512 bytes are not remembered**, and so are not purgeable by
  path. They remain purgeable by tag and by `all`.

Where the path is stored is worth one sentence, because it is the part that
could have gone wrong: it rides in the cache key's `user_tag`, which Pingora
hashes into nothing. The cache key — which decides what is shared with whom —
is provably unchanged, and a test pins that.

### Where tags come from

An entry is tagged by whatever the origin puts in the header named by
`cache.tag_header`, default `x-tanod-cache-tags`, comma-separated. Tanod
strips that header from the downstream response: tag names describe an origin's
internal content model and are nobody else's business.

```
X-Tanod-Cache-Tags: product-42, collection-sale
```

Any origin that can set a header can be purged by tag — no adapter, no
JavaScript. At most 64 tags per response, 256 bytes each; extras are dropped.

**Next.js** emits its own `x-next-cache-tags`, so pointing `cache.tag_header`
at it makes `revalidateTag()` content reachable without an integration:

```yaml
cache:
  tag_header: "x-next-cache-tags"
```

Know what that costs before you do it. The header is only set when the Next
server runs with `NEXT_PRIVATE_MINIMAL_MODE=1`, and only for statically
generated App Router routes — verified against Next 16.3.3. `NEXT_PRIVATE_*`
is undocumented and unversioned, so this can break on a minor Next upgrade with
no deprecation. Treat it as a useful shortcut with a compatibility risk, not as
a supported contract; the supported one is phase 6's `@tanod/next`.

### Deployment rollovers

Nothing to call. The cache key already carries `deployment.id`, so the previous
build's entries become unreachable the moment the id changes — and a `SIGHUP`
that changes it also purges them, so their bytes go back to the budget instead
of aging out. A reload that does *not* change the id leaves the cache alone;
purging on every `SIGHUP` would turn a routine config change into a stampede.

### What to watch

| Signal | Means |
|---|---|
| `tanod_cache_purged_total{scope="tags"}` | Invalidations arriving. A flat line after a deploy that should have invalidated something is the failure to look for. |
| `tanod_cache_purged_total{scope="all"}` rising | Somebody is purging everything, repeatedly. That is a stampede generator, not an invalidation strategy. |
| `tanod_cache_evicted_total` rising while `tanod_cache_bytes` sits at its ceiling | The working set does not fit. More memory, a shorter TTL, or a narrower key. |
| `tanod_cache_tags` growing without bound | The origin mints a tag per revision. The index is bounded by the entries pointing at it, but a tag per render means a tag index the size of the cache. |

---

## Sizing the queue

A request that cannot get an origin slot waits in a bounded queue, oldest
first, until a slot frees or `queue.timeout` passes. A full queue refuses new
arrivals immediately (`shed_queue_full`).

Under sustained overload the queue never empties, so every admitted request
waits roughly

```
queue.max × render time ÷ concurrency.max
```

before its render starts, or `queue.timeout` if that is shorter. That wait buys
no throughput — the origin is already at its ceiling — it only delays the
response, often past the point where the visitor is still waiting for it.

**Start with `queue.max` equal to `concurrency.max`**, about one render time
when full, and a `queue.timeout` you would accept on top of a render. This is
what `tanod init` generates. A deeper queue absorbs short bursts; it does
nothing for overload that lasts.

The estimate assumes requests of equal cost at one limiter. It is rougher when:

- **routes have a `weight`.** Waiters are served strictly in order, so a heavy
  request at the head holds lighter ones behind it until enough capacity frees.
- **a request passes several limiters.** Route, priority tier and global each
  have their own queue, and the request waits at each in turn against one
  shared deadline: the smallest non-zero `queue.timeout` among them.
- **render time varies.** Use `tanod_origin_latency_seconds` under load, not
  the latency of an idle origin.

Check the result during overload:

| Signal | Means |
|---|---|
| `tanod_queue_wait_seconds` near `queue.timeout` | A standing queue. Admitted requests pay the full wait. Lower `queue.max`. |
| `tanod_admission_total{decision="shed_queue_timeout"}` high relative to `shed_queue_full` | Requests waited the whole deadline and were refused anyway — the slowest way to fail. Lower `queue.max` or `queue.timeout`. |
| `tanod_queue_depth` pinned at `queue.max` | Arrivals exceed the ceiling continuously. The queue cannot help; caching, coalescing or origin capacity can. |

---

## Tracing and correlation

Correlation is unconditional and costs nothing. Every request gets a W3C trace
id and span id; both appear on every access log line, and the id Tanod
concluded is the one it forwards to the origin as `traceparent`. That is what
joins Tanod's log to the origin's for the same request — even when the origin
has no idea Tanod exists.

Span *export* is configuration:

```yaml
telemetry:
  tracing:
    service_name: tanod
    sample:
      mode: parent_or_ratio   # follow the caller, else sample one in N
      one_in: 20
    otlp:
      endpoint: "http://127.0.0.1:4318/v1/traces"
```

Two spans per sampled request: a server span for what Tanod did, and a client
span for the origin fetch nested under it. The nesting is what lets you tell
"the origin was slow" apart from "we queued for two seconds before asking it".

**An inbound `traceparent` is a claim and is ignored by default.**
`from_trusted_proxies` is safe only when every trusted proxy strips or replaces
client-supplied `traceparent` and `tracestate`. Unlike `X-Forwarded-For`, trace
context has no hop chain Tanod can walk to distinguish an edge-generated
value from one the edge merely forwarded. Ignoring one never costs the request
— Tanod simply starts a fresh trace.

**The exporter is plaintext OTLP/HTTP only.** `https://` endpoints are refused
at startup rather than quietly downgraded. Run an OpenTelemetry Collector as a
sidecar and let it handle transport, authentication and retry.

**Telemetry is never load-bearing.** The span queue is bounded and full means
drop; recording is a non-blocking `try_send`; an export failure is counted and
logged at debug. Final shutdown flushing has one total `otlp.timeout` deadline,
not one timeout per queued batch. A collector that is down costs `tanod_spans_total{outcome=
"export_failed"}` and nothing else — asserted by
[`bench/tracing.sh`](../bench/tracing.sh), which kills the collector and then
measures that fifteen requests still complete promptly.

---

## What to alert on

Rules in [`ops/prometheus/alerts.yml`](../ops/prometheus/alerts.yml), a
dashboard in [`ops/grafana/dashboard.json`](../ops/grafana/dashboard.json). The
four that matter most:

| Signal | Means |
|---|---|
| `tanod_admission_total{decision=~"shed_.*"}` rising | The origin ceiling is being hit. Either the origin got slower or traffic grew. This is Tanod working, but it is also users seeing `503`; `tanod_responses_total{source="tanod"}` says how many. |
| `tanod_origin_in_flight` pinned at `tanod_concurrency_limit` | Saturated. Look at `tanod_origin_latency_seconds` before raising the ceiling — a higher ceiling against a slower origin makes it slower still. |
| `tanod_upstream_healthy == 0` | No backend is passing its health check. Tanod is still serving, on `stale_if_error` and on whatever the origin manages. |
| `tanod_draining == 1` for longer than a deploy | An instance drained and was never replaced. It is serving and reporting itself not-ready, so a balancer has withdrawn it and nothing is watching. |

Three that are easy to miss:

- `tanod_build_info` with more than one `version` outside a rollout — part of
  the fleet did not upgrade.
- `tanod_spans_total{outcome="dropped"}` — the trace queue is too small for
  the traffic. Costs traces, never requests.
- `tanod_cache_bytes` at `cache.max_memory` **with a low hit ratio** — the
  working set does not fit and eviction is destroying entries before they are
  reused. More memory, or a shorter TTL and a narrower key.

### Origin resilience

Only meaningful with `origin.breaker` or `origin.retry` enabled.

| Signal | Means |
|---|---|
| `tanod_upstream_ejected == 1` **while** `tanod_upstream_healthy == 1` | The case passive observation exists for: the backend answers its probe and fails real requests. Look at that backend's logs, not at Tanod's. |
| `tanod_upstream_breaker_trips_total` climbing steadily | Flapping. The backend recovers enough to pass its probe and fails again. Usually worse than a backend that stays down, because each cycle sends live traffic at it. |
| `sum(tanod_upstream_ejected) == count(tanod_upstream_ejected)` | Every backend is ejected, so the ejection cap has taken over and breaker state is being ignored. This is an origin-wide failure, not a backend one. |
| `tanod_origin_retries_total{outcome="budget_exhausted"}` rising | Requests are failing faster than the budget absorbs. Raising the budget makes it worse; the origin is the problem. |
| `tanod_upstream_failures_total{kind="connect"}` rising | Processes are gone or refusing connections. Distinct from `kind="status"`, which is a process that is up and cannot render. |

Two things worth knowing before you trust these:

- **Read `tanod_upstream_failures_total` before enabling `origin.retry`.**
  Retries added to a misdiagnosed problem make it worse, and the budget is
  designed to make that failure *bounded*, not impossible.
- **`least_loaded` publishes what it decides on.**
  `tanod_upstream_in_flight` and
  `tanod_upstream_latency_ewma_microseconds` are the two inputs to the score,
  so a routing decision that looks wrong can be checked rather than guessed at.
  A backend with a much higher EWMA and much less traffic is the strategy
  working.
