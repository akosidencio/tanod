# Phase 4 — production readiness and release engineering

Validation date: 2026-10-29 (Asia/Manila). This report describes internal
engineering work, not an independent audit. Commit, push and a PR into dev were subsequently authorized by the user.
No merge, tag, npm publish, image push or release was performed.

## Executive summary

Phase 3.1 cache hardening and the production changes are prepared as one combined
review stack from clean `dev` at `725ad4e` (Phases 1–3 already merged). Package
metadata is synchronized at **0.4.0**, including the npm adapter's optional
binary dependency; this is an unpublished candidate, not an available release.

Runtime changes are small: late retired fills cannot accept new followers,
purge credentials use the existing transitive `subtle` implementation directly,
and fixed-label reload outcome counters plus a process-start timestamp make
reload refusal and restart churn observable. The Pingora lifecycle, CPU detector,
cache policies and worker default are retained.

A material benchmark-tool defect was also corrected: a timed worker could lose
its accumulated tally while waiting on its final exchange. Completed-window
accounting and explicit boundary censoring now preserve that work. Successful
and non-2xx latency are reported separately. The initial campaign is retained
as invalidated evidence and is not used for a performance claim.

## Production readiness assessment

The source/build input identity used by the final GNU benchmark binaries is
`31989fb9b80e57723b0b99fd7c3fd65e520e810df9e7edb85780bb9b07e8ad9d`.
Candidate SHA-256:
`bebced626e2db8bd565c4f1ae49b473e20c2a71be7d3c8cf5be221f3e1127626`.
Root lockfile SHA-256:
`621be51b8f0fda171a7933a1aad405279078be1eaf44e3797ef6523b5ff0c0e1`.
The unchanged embedded overload HTML is separately hashed against the base Git
commit. Build manifests name the precise Rust/Cargo/toolchain inputs; they do
not turn an uncommitted tree into a clean release commit.

All required formatting, default/TLS Clippy, workspace tests, TLS library tests,
doctests and both dependency audits passed. The full workspace run includes
588 library, 7 binary, 6 spike, 3 origin and 1 load-generator test, plus benchmark
smoke execution. TLS library testing passed 589 Tanod + 6 spike tests.
The source-cleanliness gate correctly refuses this uncommitted tree.

The rest of the evidence is summarized below after the extended campaign is
checked. A pending gate is not a successful gate.

## Security review findings

The [security model](docs/SECURITY-MODEL.md) describes client/edge/origin/operator
boundaries, privacy guards, framing delegation, credential handling, resource
limits and all 16 existing advisory exceptions. The exception set is unchanged.
Both root and fuzz lockfiles pass a refreshed RustSec database with 1,381
advisories; the initially stale fuzz graph was rebuilt from the current locked
graph and now includes the Tokio 1.53.3 compatibility/security constraint.

Forwarding trust remains opt-in by exact source network; trusted public host
requires a separate setting. The NGINX example overwrites forwarding fields.
Authorization, cookies, draft mode, Server Actions, RSC/prefetch variants,
Set-Cookie and unsupported Vary retain their earlier guards and regression
coverage. New policy and route fuzz targets exercise unsafe-sharing exclusion
and indexed-vs-linear route selection. HTTP parsing/smuggling resistance still
depends on Pingora; these tests do not constitute a complete parser audit.

Administrative diagnostics remain unauthenticated on a separate private
listener. Purge is disabled without a token and otherwise uses bearer auth;
reload uses local SIGHUP. There is no new HTTP mutation/reload endpoint.
Length is not concealed by credential comparison. Credential holders may
trigger expensive purges, so deployments needing per-client throttling must
apply it at their operator gateway.

No new unsafe code was introduced. Tanod's existing crate-level prohibition
remains. No independent security review is claimed.

## Deployment and container hardening

[ops/production](ops/production/README.md) contains actual-schema-validated
examples for a VPS, Compose, a fixed separate Kubernetes tier, and a trusted
reverse proxy. They explicitly bound queue/timeouts, cache/body budgets, CPU,
memory, PID/file limits, writable runtime paths and stop deadlines. They do not
force origin privacy overrides to make dynamic pages cacheable.

The Kubernetes example partitions eight work units over at most two Tanod pods,
with no surge, and exposes only traffic through its Service. Its NetworkPolicy
separates ingress, monitoring and operators by namespace labels. Schema validation
passed all three resources with no skips. Cluster-specific CNI/probe enforcement,
capacity during terminating pods, origin lifetime and real rolling behavior
still require staging-cluster validation; no cluster was modified here.

The final TLS-enabled musl image was rebuilt from the final source/lockfile.
Local image identity:
`sha256:613a3c50ce08b64aa4d26d6d69e575b0e6edafb6b35c9c3507fbf220b13ed3b0`.
The container probe passed CPU quotas 0.5/1/1.5/2/4 with 1/1/1/2/4 default
workers, respectively, under a 64 MiB limit. The production Compose fixture
passed with non-root UID/GID 10001, read-only root, dropped capabilities,
no-new-privileges, bounded tmpfs/PIDs/descriptors, a real Next.js origin,
authenticated purge and an observable SIGTERM readiness window.

Affinity tests passed for one/two/four CPUs, an explicit four-worker override
on one CPU, and multiple replicas sharing one CPU. Idle windows reported no
scheduler ticks at the sampling resolution and roughly 10 MiB RSS per process.
The standard library detector remains in place. cgroup v1 is unavailable on
this v2-only host; CPU shares alone are not a quota.

## Graceful shutdown and restart

`bench/lifecycle.py` passed **14 scenarios**, seven under each retention policy:
idle SIGTERM, 16 concurrent/queued requests, a cache fill with eight readers,
a long progressive stream, a WebSocket, reload followed by shutdown, and fast
SIGINT. Every process exited zero. The SIGTERM fixtures used three seconds of
readiness withdrawal plus four seconds of Pingora grace and exited in about
7.0 seconds; SIGINT was immediate. Requests completed with whole bodies, and
observable pre-close cases returned permits and pinned bytes to zero.

The longer stream completed across listener shutdown. WebSockets were bounded
by the shutdown deadline; Pingora does not promise an application-level close
frame. Clients need reconnect handling. During the readiness withdrawal window
traffic intentionally continues; the load balancer must stop routing new work
before listener closure. This uses Pingora's lifecycle rather than a second
shutdown framework.

The first new queue probe incorrectly observed only the global limiter; actual
requests were queued in the priority limiter. The corrected probe observes all
admission limiters. This was a test-fixture assumption, not a runtime fix.

## Observability and alerting

Actual names and units were checked in source. `tanod_origin_in_flight` is held
weighted work units; `tanod_upgrade_active` is separate tunnel slots. Queue,
cache/pinned/headroom, health, breaker and latency diagnostics retain their
existing semantics. Requests never supply metric label values.

`tanod_config_reloads_total{outcome="success"|"failure"}` counts processed
attempts, not sent signals (signals may coalesce). Refusal preserves generation
and policy. `tanod_process_start_time_seconds` remains constant across reloads.
The admin regression checks one successful and one refused reload against the
actual metrics, plus timestamp stability and unchanged generation on failure.

Prometheus examples now include reload refusal, repeated restarts, cache
pressure, breaker open and sustained upstream failures alongside existing load,
queue and response-abort rules. Promtool validated **22 alert rules and 10
recording rules**. The existing Grafana dashboard adds a reload-outcome panel.
A timestamp change cannot distinguish planned from unplanned restart by itself;
correlate supervisor exit/OOM/deploy events. Retired limiter gauges are removed,
but route counter/histogram series can persist across operator-driven route-ID
churn. Keep route IDs stable and monitor series counts.

## Fuzzing

cargo-fuzz 0.13.2 and rustc 1.101.0-nightly (`c36f14571`, 2026-10-01) built all
nine targets with AddressSanitizer, optimization and debug assertions. Nightly
emits deprecation warnings for existing fetch_update calls; the supported stable
toolchain passes warning-denying Clippy. The older supported Rust API is retained.

The bounded runner validates both locked graphs, preserves fixtures/corpora and
crash artifacts, limits input length/time/RSS, refuses an empty target list, and
checks that neither dependency lockfile changed. No production traffic or
credentials enter the corpus. Campaign results are filled from the retained log
when complete; build success alone is not reported as a successful fuzz campaign.

## Stability and soak

Final one-hour results for both policies, full memory-pressure parameters,
restart/chaos and the complete end-to-end suite are pending the checked extended
campaign. CI-sized runs and an hour-long soak are distinct gates. No peak RSS,
leak result or time span is estimated.

## Framework compatibility

The declared support matrix remains Next 16.4.0 and 16.3.8 on Node 22 and 24;
no broader version policy was invented. Host Node/Bun adapter tests passed
79/80 with the one existing manifest-copy fixture check skipped. That skip is
not framework compatibility evidence. Real Docker/browser/policy/invalidation/
replica tests are recorded per declared pair once the matrix completes.

## CI and release engineering

PR checks retain the existing real-framework/end-to-end jobs and add validation
of production examples and benchmark smoke. The fuzz runner cannot silently
run zero targets. Workflow lint passed. The new read-only extended workflow
runs pressure, repeated invalidation, lifecycle, container/quotas and longer fuzz/
soak checks. Release calls it on the explicit tag/ref with one hour per policy
before the existing full CI, locked native/musl builds, checksums, SBOM and
provenance publication pipeline. No new publication permission or credential
was introduced.

`release-preflight.py` enforces clean source and matching Cargo/lock/npm versions.
Its local `--allow-dirty` mode explicitly reports `release_gate: false` and is
not used by CI. Existing reproducible-build and attestation workflows remain;
local working-tree snapshot reproduction and SBOM results are recorded after
execution. No signed release provenance or external attestation is claimed.

## Documentation and known limitations

The operations/release guides, changelog, migration guide, deployment/security
models, examples, benchmark provenance and required root reports are included.
Reports are not excluded by .gitignore. The historical Phase 3 report now flags
the measurement defect rather than leaving its affected numbers unqualified.

Remaining operational boundaries include detached-reader RSS, the first
oversized chunked response abort, bounded headroom exhaustion, per-process
cache/coalescing/purge, static replica capacity partitions, edge upload/
connection limits, and long-lived tunnel termination at the grace deadline.
The external cache-key review remains outstanding. There is no claim of a new
TinyUFO optimization, scheduler tuning proof, dynamic capacity coordinator,
cluster certification or complete external security audit.

## Release readiness decision

**NOT READY** — the source-cleanliness gate is intentionally failing while this
review stack is uncommitted, and extended/performance/reproduction evidence is
not yet complete. Review and commit the intended changes, then run the mandatory
clean-ref CI/release gates in docs/RELEASE-GATES.md. No unavailable or pending
check is treated as successful. This decision will be updated with checked
results when the queued validation finishes.
