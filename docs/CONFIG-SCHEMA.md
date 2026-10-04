# Configuration schema versioning

Tanod is pre-1.0 and the configuration format will change. This document
says what "will change" is allowed to mean, so that a file written today has a
defined relationship with a binary released later.

**Current schema version: 1.** Required in every file:

```yaml
version: 1
```

A file naming a version this binary does not understand is **refused at
startup**, naming both numbers:

```
error: invalid configuration in /etc/tanod/tanod.yaml
  caused by: config schema version 2 is not supported; this build of tanod
  understands version 1. See docs/CONFIG-SCHEMA.md for the compatibility rules
  and the migration notes
```

Refused, never coerced. A binary that guessed at a version it did not know
would be applying a policy nobody wrote — and in this configuration file, the
policy decides whether a response is shared between users.

`tanod version` prints the schema version a binary speaks, and `/status`
reports it as `config.schema_version`.

---

## The rules

### What may change without a version bump

- **New optional keys.** Anything with a default that reproduces the previous
  behaviour exactly. `telemetry.admin`, `telemetry.tracing` and
  `server.graceful` were all added this way.
- **New enum variants** on a key that already exists, where the existing
  variants keep their meaning.
- **Wider accepted ranges**, where the previously accepted values are unchanged.
- **A key becoming implemented.** Tanod rejects options it accepts but does
  not honour, so a key moving from "rejected as unimplemented" to "works" only
  ever turns a failing config into a working one.

### What requires a version bump

- Removing a key, or renaming one.
- Changing what an existing value **does**, including a changed default that
  alters behaviour.
- Changing a unit, a scale, or the meaning of a bare number.
- Tightening validation so that a previously valid file is now refused.

### What is *never* silent

Two rules, and they are the whole reason this file is short:

1. **An unknown key is an error.** Every struct is `deny_unknown_fields`. A
   typo is a silent policy change otherwise, and in this file a silent policy
   change means an unprotected origin or a cache serving something it should
   not.
2. **An accepted-but-unimplemented key is an error.** If Tanod cannot honour
   a setting, it refuses to start rather than ignoring it. A config that claims
   a protection it is not running is worse than one that does not compile.

Both mean an upgrade can fail loudly. That is the intent: the alternative is an
upgrade that succeeds and quietly does something else.

---

## Migration notes

### → version 1

The first published schema. Nothing to migrate.

Within version 1, these keys have been **added** since the initial release.
All are optional and default to the previous behaviour, so no existing file
needs to change:

| Key | Added with | Default |
|---|---|---|
| `mode` | staged Next.js rollout | `protect` |
| `capacity` | static multi-replica budget partition | absent |
| `server.h2c` | protocol coverage | `false` |
| `server.tls` | TLS termination | absent |
| `server.trusted_proxies` | forwarded-header trust | trusts nobody |
| `origin.tls`, `origin.http_version` | origin protocol | absent / `http1` |
| `spool.*` | the response spool | disabled |
| `upgrade.*` | WebSocket/Upgrade | disabled, `501` when off |
| `server.graceful.*` | zero-downtime restart | `/tmp` paths, 5s drain, 10s shutdown |
| `telemetry.admin` | readiness and status | absent — **no readiness endpoint** |
| `telemetry.tracing` | OpenTelemetry | correlation on, export off |
| `origin.breaker` | circuit breaking | disabled |
| `origin.retry` | bounded retries | disabled |
| `origin.priorities` | reserved capacity | every tier 100% — no tiering |
| `origin.load_balancing: least_loaded` | load-aware selection | `round_robin` |
| `origin.resolve_interval` | multi-instance origins behind one name | `10s`; `0` resolves once at startup |
| `route.priority`, `route.weight` | weighted admission | `normal`, `1` |
| `cache.eviction` | measured eviction policy | `clock` — **a behaviour change**, see below |
| `cache.tag_header` | cache tags | `x-tanod-cache-tags` |
| `cache.purge.token` | the purge API | absent — **endpoint disabled** |

Two of those are worth acting on rather than merely noting:

- **`mode: observe` does not protect the origin.** It records classification
  and telemetry while bypassing admission, caching, coalescing, and spooling.
  Use it only as the first rollout stage.

- **`capacity` declares a static replica partition.** Tanod verifies that
  `origin.concurrency.max × capacity.replicas` does not exceed
  `capacity.global_max`. The orchestrator must enforce the replica count.

- **`server.graceful.pid_file` and `upgrade_socket` default to `/tmp`.** Two
  Tanod processes on one host with the defaults will hand each other their
  listening sockets. Set them per instance, under `/run` on a systemd host.
- **Without `telemetry.admin` there is no readiness endpoint**, so a load
  balancer cannot tell when an instance is draining and a rolling restart
  drops requests. `tanod check` says so.
- **`telemetry.admin.require_healthy_upstream: true` requires `health:`.** A
  configured backend is unknown until it completes the configured
  `healthy_after` success streak; readiness does not optimistically report it
  healthy during startup.
- **Inbound trace context is ignored by default.** Opt into
  `from_trusted_proxies` only when every trusted proxy strips or replaces
  client-supplied `traceparent` and `tracestate`.
- **`origin.breaker` needs at least two upstreams.** With one backend the
  ejection cap keeps it in rotation — ejecting the only backend turns a partial
  failure into a total outage — so the breaker would observe failures and never
  act on one. Validation refuses the combination rather than leaving it inert.
- **`cache.eviction` defaults to `clock`, not to the previous FIFO.** This is
  the one added key whose default changes existing behaviour. It only changes
  *which* entry is discarded when the budget is full, never how many or which
  responses are shareable, and it measured a 0.600 hit ratio against FIFO's
  0.525 on a skewed workload. Set `eviction: fifo` to restore the old
  behaviour exactly.
- **`cache.purge.token` needs `telemetry.admin`.** The endpoint is served on
  the admin listener, so a token with no listener is a protection configured
  against nothing. It must also be at least 24 printable-ASCII characters.
- **`route.priority` needs `origin.priorities`.** Labelling routes by priority
  while every tier may occupy the whole ceiling is a prioritisation that does
  nothing, so it is refused for the same reason as everything else in this
  file.

### Keys that are refused because they would do nothing

Distinct from the table below, which lists keys Tanod cannot honour. These
*could* be honoured and would have no effect, which is the failure mode this
project treats as worse than a crash: someone ships believing a protection is
running.

| Combination | Why |
|---|---|
| `origin.breaker.enabled` with one upstream | The ejection cap keeps the only backend in rotation. |
| `origin.breaker.max_ejected_percent` that floors to 0 backends | Failures would be observed and never acted on. |
| `origin.retry.max_attempts: 1` | Attempts include the first try, so it never retries. |
| `origin.retry` with `budget_percent: 0` and `budget_min: 0` | No retry can ever be afforded. |
| An `origin.priorities` share that floors to a ceiling of 0 | Every request at that priority would be refused. |
| `route.priority` with uniform `origin.priorities` | Every priority competes for the same ceiling. |
| `route.weight` above any ceiling it is charged against | No request on the route could ever be admitted. |
| Replica allocation above `capacity.global_max` | Copying a local ceiling across replicas would exceed the declared group budget. |
| `cache.purge.token` with no `telemetry.admin` | Nothing is listening for the endpoint it secures. |
| `cache.tag_header` naming a *request* header (`Cookie`, `Accept`, …) | Tags decide what a purge destroys; taking them from the client hands that decision to the client. |

### Keys that are deliberately refused

These parse and are then rejected at startup. Each is listed because someone
will reasonably expect it to work, and silently ignoring it would be the worse
outcome.

| Key | Why |
|---|---|
| `cache.respect_origin: false` | Not implemented. Use per-route `cache.override_origin`, which is fenced by validation. |
| `deployment.id_header` | Impossible by construction: the cache key is built before the response exists. |
| `coalesce.on_timeout: requeue` | Not implemented. |
| `origin.tls.ca` | Pingora 0.8's rustls connector never reads a per-peer CA store. Use `SSL_CERT_FILE` / `SSL_CERT_DIR`. |
| `telemetry.tracing.otlp.endpoint: https://…` | The exporter is plaintext-only by design. Run a collector as a sidecar. |

---

## Checking a file

```bash
tanod check --config /etc/tanod/tanod.yaml
```

Exits non-zero on anything that would prevent a start, and prints the things
that are legal but worth knowing about: a route overriding origin cache
directives, an origin TLS connection that is encrypted but not authenticated,
an admin listener on an unspecified address, and the total time a `SIGTERM`
will take.

`tanod run --config … --test` goes further: it binds every listener and exits
zero only if the process could actually have started. That is the pre-flight
for a zero-downtime upgrade — see [OPERATIONS.md](./OPERATIONS.md).
