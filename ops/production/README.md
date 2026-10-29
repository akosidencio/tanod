# Production examples

These are starting configurations, not calibrated capacity claims. Validate the
origin ceiling on your application and machine. Keep an RSS margin above cache
bytes + one maximum body + spool, connections, stacks, indexes and detached
readers. 64 MiB of cache inside a 256 MiB container is a starting point only.

Set `TANOD_PURGE_TOKEN` to a random credential in your service environment or
secret manager; never put a production credential in Git. Purge is the only
admin HTTP mutation, requires a bearer token, and is disabled without one.
Reload uses local SIGHUP, not an HTTP endpoint. Status and metrics are read-only
but unauthenticated: restrict the entire operator network, including peers on a
Compose bridge. A loopback host publication does not isolate container peers.

* VPS: install `vps-tanod.yaml` as `/etc/tanod/tanod.yaml` and use
  `packaging/systemd/tanod.service`. Use separate origin service shutdown
  ordering or `origin.command` so the origin outlives Tanod's drain. One worker
  deliberately leaves CPU to Node on a shared host. Each instance needs its own
  PID and handover socket directory.
* Compose: set `SSR_IMAGE`, `TANOD_IMAGE` to reviewed image digests, set the
  credential, then `docker compose -f ops/production/compose.yaml config` and
  `docker compose -f ops/production/compose.yaml up -d`. The app must implement
  `/healthz`. Keep the origin private. During maintenance stop Tanod before
  the app; validate application-specific dependency shutdown ordering.
* Kubernetes, separate tier: create `tanod-config` from
  `kubernetes-tanod.yaml`, create the `tanod-admin` Secret, substitute the
  reviewed image digest, then apply `kubernetes.yaml` in a staging namespace.
  `kubectl create configmap tanod-config --from-file=tanod.yaml=ops/production/kubernetes-tanod.yaml`.
  The origin Service is `next-origin:3000`. At most two active Tanod replicas
  share an eight-unit ceiling (four each). Do not HPA this separate tier or
  origin pool without redesigning the budget; terminate old pods fully before
  exceeding the declared group size. Coalescing, caches and purge are per
  replica. Purge every replica; use path-stable ingress where reuse matters.
  Label the ingress, monitoring and operator namespaces with the labels used
  by the shipped NetworkPolicy. Its enforcement, kubelet probes and rolling
  behavior require cluster validation.
* Trusted reverse proxy: pair `trusted-proxy-tanod.yaml` with `nginx.conf`
  inside an existing TLS virtual host. The edge overwrites every forwarding
  value. Do not use `$proxy_add_x_forwarded_for` for this single-edge example.
  For a CDN, replace loopback trust with the actual maintained proxy CIDRs
  and enforce an origin firewall; never trust all private address space merely
  because the connection came from a container.

For Compose/Kubernetes behind a TLS edge, configure the actual trusted proxy
source addresses and its forwarding-header overwrite policy as in the trusted
proxy example; the generic templates intentionally start with no trusted peers.
TLS terminates at the edge in these examples. A `server.tls` configuration
requires a TLS-enabled Tanod image and still leaves the cleartext listener
active: limit that listener to the trusted network.

SIGTERM advertises not-ready for 5 seconds while continuing to serve. Pingora
then stops listeners and gives active requests 35 seconds, plus teardown.
The 50-second container/pod deadline includes margin; match it to any longer
streams you permit. SIGINT is fast shutdown. Long-lived WebSockets can be
terminated at the drain deadline and need application reconnect handling.
Liveness remains 200 when origins fail; readiness optionally considers origin
health only when an alternative useful destination exists.

Validate all configurations before restarting:

```sh
bash scripts/check-production-configs.sh
curl -fsS http://127.0.0.1:9091/health/live
curl -fsS http://127.0.0.1:9091/health/ready
curl -fsS http://127.0.0.1:9091/status
```
