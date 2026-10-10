# Choosing a deployment

Which setup to run, how to find the right origin ceiling, and how Tanod works
alongside a CDN and an autoscaler.

This guide is recommendations, not measurements. The configurations below pass
`tanod check`, but the numbers in them are placeholders: the ceiling has to be
measured for your application, on your hardware.

- [Where Tanod sits](#where-tanod-sits)
- [Pick a scenario](#pick-a-scenario)
- [Finding the ceiling](#finding-the-ceiling)
- [Autoscaling](#autoscaling)
- [Recommended configurations](#recommended-configurations)

---

## Where Tanod sits

A CDN, Tanod and your framework's cache each solve a different problem, and a
serious deployment usually runs all three.

```
Visitor → CDN / edge cache → Tanod → app server (with its own cache)
```

| Layer | What it does | What it cannot do |
|---|---|---|
| CDN or edge cache (Cloudflare, Fastly, CloudFront) | Answers cacheable public pages close to the visitor, so they never reach you | Protect your origin from traffic it cannot cache: logged-in pages, carts, search, personalised content, cache misses. It does not know how much work your origin can take. |
| Tanod | Counts the work in flight at the origin, queues briefly or refuses quickly beyond the ceiling, merges identical requests, serves stale content when the origin fails | Replace a CDN. Its cache is short-lived and local; its job is protection, not distribution. |
| Framework cache (Next.js ISR, data cache, `use cache`) | Avoids recomputing data and pages inside the app | Protect the app from overload. It lives in the process that is overloaded, and every request that reaches the app still starts rendering. |

In one line: **a CDN and the framework cache reduce how much work reaches your
app; Tanod limits how much work your app accepts at once.** Per-client rate
limiting is a fourth, separate job — usually the CDN's or the ingress's — and
cannot stop many ordinary visitors from overloading the origin together.

---

## Pick a scenario

| Scenario | Recommended setup | Why |
|---|---|---|
| **A. One server or VM** | Tanod in front of the app on the same host, systemd for both, or `origin.command` so Tanod runs the app | Simplest case: one ceiling, one cache, one purge endpoint. See [Standalone server](./STANDALONE.md). |
| **B. One container** (Docker, Compose, a container platform where you control the image) | `origin.command` in the app image: Tanod is the entrypoint and supervises the app | One process tree, correct shutdown order, nothing else to deploy. |
| **C. Kubernetes with the app autoscaled (HPA, KEDA)** | **Tanod inside every app pod** via `origin.command` | The ceiling is per pod, so capacity grows and shrinks with the pod count and nothing needs reconfiguring. See [Autoscaling](#autoscaling). |
| **D. Fixed app pool, shared Tanod tier** (Kubernetes or VMs, no autoscaling) | Separate Tanod deployment with a `capacity` partition, behind path-stable ingress | One budget for the whole pool, shared cache and coalescing per replica. Do not autoscale either side. See the [Next.js production reference](./NEXTJS-PRODUCTION-REFERENCE.md#replica-behavior). |
| **E. Behind a CDN** | Any of A–D, with the CDN's addresses in `server.trusted_proxies` | The CDN takes cacheable public traffic; Tanod protects the origin from the rest. |
| **F. Fully managed hosting** (no origin tier you control) | Tanod does not fit | It has to run in front of an app server you operate. |

Scenario C is the default recommendation for Kubernetes. Choose D only when
the app pool is a fixed size and a shared cache across fewer Tanod replicas is
worth the extra operational work: replica limits, path-stable ingress, and
purge fan-out to every replica.

**Sidecar instead of `origin.command`.** Running Tanod as a second container in
the pod works, but Kubernetes sends `SIGTERM` to both containers at once. The
app can stop before Tanod has finished draining, and the requests Tanod is
still forwarding fail. `origin.command` avoids this: Tanod drains first, then
stops the app.

---

## Finding the ceiling

The ceiling (`origin.concurrency.max`) is how many requests the app can work on
at once before it slows down badly. Past that point, accepting more work does
not finish more of it; it only makes every response slower.

### 1. A first guess

- **Rendering is mostly CPU** (typical SSR): Node runs JavaScript on one core
  per process, so start near 2–4 per Node process.
- **Rendering mostly waits on a database or API**: the app can hold more
  requests at once; start higher and let calibration decide.
- **From traffic you already know** (Little's law): ceiling ≈ peak requests per
  second × average response time in seconds. 40 req/s at 250 ms is 10.

### 2. Measure it

`tanod-next calibrate` sends load at increasing concurrency and reports
throughput and p95 latency at each step:

```bash
tanod-next calibrate --target http://127.0.0.1:3000 \
  --route /products/a-real-product --steps 1,2,4,8,16 --allow-load
```

It calls the highest step with no failures and a p95 no worse than twice the
single-request p95 the *observed knee*, and recommends 70% of it. It never
changes configuration.

- **Point it at the app directly**, or at Tanod in `observe` mode. A Tanod in
  `protect` mode refuses requests above its current ceiling, which shows up as
  failures and hides the real knee.
- **Use a realistic route.** A page lighter than your real traffic produces a
  ceiling that is too high.
- **Use production-shaped hardware**, with the same CPU and memory limits as
  production. A pod's CPU limit moves the knee; change the limit and the
  ceiling has to be measured again.
- Non-local targets need `--allow-production` as well. In Kubernetes,
  `kubectl port-forward` to one staging pod keeps the target local and the
  measurement per pod.
- **Note the pod's CPU at the recommended ceiling.** Autoscaling needs it.

### 3. Roll out in stages

Generate `--rollout observe` first. It records decisions without enforcing
them. Watch `tanod_origin_in_flight` through a normal peak: that is what
healthy busy looks like. Then move to `protect` with a ceiling above the
normal peak and at or below the calibrated recommendation. The full sequence
is in the [Next.js production reference](./NEXTJS-PRODUCTION-REFERENCE.md#roll-out-in-stages).

### 4. Adjust from what you see

| Signal | Meaning | Action |
|---|---|---|
| `tanod_admission_total{decision=~"shed_.*"}` rising while app CPU is low | Ceiling too low | Raise it |
| `tanod_origin_latency_seconds` climbing before any shedding | Ceiling too high | Lower it |
| Shedding only during real spikes, origin latency flat | About right | Leave it |

Size the queue with the ceiling: see
[Sizing the queue](./OPERATIONS.md#sizing-the-queue).

---

## Autoscaling

Tanod and an autoscaler cover different timescales and work well together:

```
Spike → Tanod refuses the excess in milliseconds → the app stays healthy
      → the autoscaler sees the load → adds pods within a minute or two
      → each new pod brings its own Tanod ceiling → refusals stop
```

Without Tanod, the app can slow down or fall over before the new pods are
ready. Set up wrongly, though, the two can block each other.

### Conflict: Tanod can stop the autoscaler from scaling

An HPA usually scales on CPU, say when it goes above 70% of the pod's request.
Tanod caps the work each pod accepts. If the ceiling holds pods at 60% CPU, the
HPA sees a healthy service and never scales, while Tanod refuses visitors at
the door.

The rule: **the autoscaler must react before Tanod starts refusing.**

- **Scaling on CPU:** set the HPA target well below the CPU the pod reaches at
  its Tanod ceiling, which is the figure noted during calibration. If a pod at
  its ceiling runs at 90% of its CPU request, a target of 60–70% scales first.
- **Better: scale on Tanod's own load.** In-flight work against the ceiling
  says directly how close the pods are to refusing. See
  [the KEDA example](#autoscaling-on-tanods-metrics).

### Conflict: a shared Tanod tier does not grow with the app

In scenario D, the group budget (`capacity.global_max`) is fixed. When the app
pool autoscales, Tanod's budget stays the same: new pods sit idle while Tanod
keeps refusing. The Tanod deployment itself must not autoscale either — Tanod
cannot check how many replicas are running, so the orchestrator has to keep the
count at or below `capacity.replicas`, or the budget is exceeded. Autoscaled
apps belong in scenario C.

### Other points

- **New pods start cold.** In scenario C each new pod has an empty cache, so
  its first requests all render. Keep `minReplicas` at a level that handles
  normal peak traffic, and use the autoscaler for spikes.
- **Scale-down drains.** Tanod drains in-flight requests on `SIGTERM` before
  stopping the app. Set `terminationGracePeriodSeconds` as described in
  [Operations](./OPERATIONS.md#kubernetes).
- **Alert on refusals; do not scale on them.** A refusal means a visitor
  already received a `503`. Scale on load before that point, and alert when
  refusals happen anyway.

---

## Recommended configurations

### Scenario C: Tanod inside each app pod

Add the static Tanod binary to the app image and make Tanod the entrypoint.
For a Next.js standalone image (the Tanod image is `linux/amd64` only):

```dockerfile
FROM node:22-alpine
# ... your existing Next.js standalone build ...
COPY --from=ghcr.io/akosidencio/tanod:0.3.1 /usr/local/bin/tanod /usr/local/bin/tanod
COPY tanod.yaml /etc/tanod/tanod.yaml
RUN apk add --no-cache tini
ENTRYPOINT ["/sbin/tini", "--", "tanod"]
CMD ["run", "--config", "/etc/tanod/tanod.yaml"]
```

`tanod.yaml`, with the ceiling **per pod**:

```yaml
version: 1

server:
  listen: "0.0.0.0:8080"
  trusted_proxies:
    from: ["10.244.0.0/16"]      # your ingress controller's pod network
    client_ip: x_forwarded
    scheme: x_forwarded

origin:
  upstreams: ["127.0.0.1:3000"]
  command:
    args: ["node", "server.js"]
    ready_timeout: 60s
    stop_timeout: 10s
  concurrency:
    max: 8                       # from calibration, per pod
    queue:
      max: 8                     # about one render time when full
      timeout: 1s

telemetry:
  admin:
    listen: "0.0.0.0:9091"       # probes; keep it off the public Service
  prometheus:
    listen: "0.0.0.0:9090"       # scraped per pod

routes:
  - id: default-private
    match: "/**"
    class: private_dynamic
    cache:
      enabled: false
    coalesce:
      enabled: false
```

Add reviewed public routes above the catch-all, or generate them with
`tanod-next generate --upstream 127.0.0.1:3000 --concurrency 8`. The
Service sends traffic to port 8080; probes, `preStop` and the grace period
follow the [Kubernetes section of Operations](./OPERATIONS.md#kubernetes).

A CPU-based HPA, with the target below the CPU at the Tanod ceiling:

```yaml
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata:
  name: storefront
spec:
  scaleTargetRef:
    apiVersion: apps/v1
    kind: Deployment
    name: storefront
  minReplicas: 2                 # enough for normal peak traffic
  maxReplicas: 10
  metrics:
    - type: Resource
      resource:
        name: cpu
        target:
          type: Utilization
          averageUtilization: 60   # below the CPU a pod reaches at its ceiling
```

#### Autoscaling on Tanod's metrics

With [KEDA](https://keda.sh) and Prometheus scraping every pod's port 9090,
scale on in-flight work, keeping CPU as a second trigger:

```yaml
apiVersion: keda.sh/v1alpha1
kind: ScaledObject
metadata:
  name: storefront
spec:
  scaleTargetRef:
    name: storefront
  minReplicaCount: 2
  maxReplicaCount: 10
  triggers:
    - type: prometheus
      metadata:
        serverAddress: http://prometheus.monitoring:9090
        # Total origin work in flight across the deployment's pods.
        query: sum(tanod_origin_in_flight{limiter="global", job="storefront"})
        # Target per pod: 75% of the per-pod ceiling of 8.
        threshold: "6"
    - type: cpu
      metricType: Utilization
      metadata:
        value: "60"
```

KEDA divides the query result by the replica count and compares it with the
threshold, so the query returns the *total* and the threshold is the *per-pod*
target. Adjust the `job` label to match your scrape configuration. Tanod
updates these gauges as requests pass through, not on a timer, so after
traffic stops entirely the last value can linger. The CPU trigger and
`minReplicaCount` keep that from mattering.

### Scenario D: shared Tanod tier in front of a fixed pool

The app pool sits behind a **headless** Service, so the name resolves to every
pod and Tanod balances across them. The group budget is split statically
across a fixed number of Tanod replicas:

```yaml
version: 1

capacity:
  global_max: 48                 # what the whole app pool can take, measured
  replicas: 2                    # Tanod replicas; never autoscale past this

server:
  listen: "0.0.0.0:8080"
  trusted_proxies:
    from: ["10.244.0.0/16"]
    client_ip: x_forwarded
    scheme: x_forwarded

origin:
  upstreams: ["storefront-headless.default.svc.cluster.local:3000"]
  resolve_interval: 10s
  load_balancing: least_loaded
  concurrency:
    max: 24                      # global_max / replicas; tanod check enforces it
    queue:
      max: 24
      timeout: 1s

telemetry:
  admin:
    listen: "0.0.0.0:9091"
  prometheus:
    listen: "0.0.0.0:9090"

routes:
  - id: default-private
    match: "/**"
    class: private_dynamic
    cache:
      enabled: false
    coalesce:
      enabled: false
```

`tanod check` refuses a configuration where `concurrency.max × replicas`
exceeds `global_max`. When the app pool changes size, measure again, update
`global_max` and the per-replica `max`, and reload. For path-stable ingress and
purge fan-out to every replica, see the
[Next.js production reference](./NEXTJS-PRODUCTION-REFERENCE.md#replica-behavior).
