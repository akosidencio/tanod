# Phase 4 — production readiness and release engineering

Validation date: 2026-10-29 (Asia/Manila). This is maintainer engineering work, not an independent audit. The user authorized commits, push and a PR into dev, then merged PR #11 while checks continued. No tag, npm publish, container-registry push or release was performed by this task. This follow-up records the completed validation, including failures.

## Executive summary

Phase 3.1 cache hardening and the production changes were delivered as one combined
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
`5770f32c813e45fd51452031ea64557ee4baa25d3c6c8adabc9aa6221a0567ea`.
Candidate SHA-256:
`bebced626e2db8bd565c4f1ae49b473e20c2a71be7d3c8cf5be221f3e1127626`.
Root lockfile SHA-256:
`621be51b8f0fda171a7933a1aad405279078be1eaf44e3797ef6523b5ff0c0e1`.
The final build-input digest includes embedded `src/` assets as well as Rust/Cargo/toolchain inputs. Manifests were captured from clean commit `325aa63`; subsequent script, workflow and documentation changes preserve that binary input identity. [PR #11](https://github.com/akosidencio/tanod/pull/11) was merged into `dev` by the user.

All required formatting, default/TLS Clippy, workspace tests, TLS library tests,
doctests and both dependency audits passed. The full workspace run includes
588 library, 7 binary, 6 spike, 3 origin and 1 load-generator test, plus benchmark
smoke execution. TLS library testing passed 589 Tanod + 6 spike tests.
The source-cleanliness/version preflight passed after the authorized commits. Its earlier refusal of the dirty preparation tree is retained as an expected failure, not a release pass.

Most local gates passed, including a complete isolated end-to-end run with Docker, browser and TLS enabled. Both required hour-long soaks failed their origin-concurrency ceiling. Those failures block release regardless of the successful shorter checks below.

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

The Kubernetes example partitions eight work units over two stable Tanod pod identities and exposes traffic through its Service. Review caught a real template flaw: Deployment `maxSurge: 0` excludes terminating pods from its availability arithmetic and can overlap old/new work. The example now uses an ordered StatefulSet and headless identity Service; force deletion requires fencing the old process first. See the [deployment behavior](https://kubernetes.io/docs/concepts/workloads/controllers/deployment/) and [StatefulSet lifecycle](https://kubernetes.io/docs/concepts/workloads/controllers/statefulset/). Its NetworkPolicy
separates ingress, monitoring and operators by namespace labels. Schema validation
passed all four resources with no skips. Cluster-specific CNI/probe enforcement,
capacity during terminating pods, origin lifetime and real rolling behavior
still require staging-cluster validation; no cluster was modified here.

The final TLS-enabled musl image was rebuilt from the final source/lockfile.
Local image identity:
`sha256:7b5fe0d747b9ae6e29d4de366bdcc47ab821b786a81b689941cdc6ecdfb89b88`. Its version command confirms TLS, and OCI labels record commit `325aa63` plus the build-input digest above. The final quota/Compose probe passed on this exact image; real-framework runs rebuild their own images from the current checkout.
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

The first full local suite exposed a synchronization flaw in its trace-export test: it accepted the first nonempty collector batch (three earlier traces), then immediately required the fourth request's log ID. The test now waits up to the existing 12-second deadline for that actual trace ID. All existing payload, correlation, metrics and collector-failure assertions remain. Three targeted reruns each exported four server/four origin spans and passed. The first failure is retained; the complete suite is rerun after the fix rather than reported green from selected subtests.

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
credentials enter the corpus. All nine 60-second campaigns completed successfully (61 seconds reported per target), totaling 7,542,570 executions. Peak per-target RSS was 429–601 MiB. No crash, sanitizer failure or lockfile change was reported. [Per-target results](validation/phase4/fuzz-summary.json) and the retained log distinguish this bounded smoke campaign from exhaustive security assurance.

## Stability and soak

The first remote end-to-end run failed its existing disconnect-coverage assertion: all 1,720 abort-arm responses completed within 50 ms, so it correctly refused to pass a soak without actual disconnects. This was a fixture problem, not a relaxed gate. The arm now requests four progressive 256 KiB chunks spaced 25 ms apart (at least 75 ms total), with a 50 ms client deadline and a 16 KiB/s reader limit. It counts only timeouts after partial HTTP 200 body bytes as mid-body disconnects, records pre-body timeouts separately, and exposes unexpected HTTP/transport errors. The positive mid-body-abort assertion remains. This also ensures the origin is unfinished when the client disconnects. The original CI failure is retained; corrected runtime evidence follows below.

**Both hour-long release-profile gates failed.** Each used 24 client workers, a 16 MiB cache and an eight-unit origin limit:

| Policy | Requests | Unexpected client responses | RSS first → last median, KiB | Peak RSS, KiB | Cache peak, bytes | Origin peak / ceiling |
|---|---:|---:|---:|---:|---:|---:|
| CLOCK | 787,044 | 0 | 73,716 → 66,328 | 81,656 | 16,776,760 | **10 / 8: FAIL** |
| TinyUFO | 693,833 | 0 | 62,008 → 51,456 | 67,624 | 16,769,956 | **10 / 8: FAIL** |

Raw samples and outcome counts are preserved in [soak evidence](validation/phase4/soak-raw/), alongside the failed result JSON. RSS stayed below the configured leak ceiling and declined between quarter medians; that does not turn the concurrency failure into a pass. The failing assertion runs before the final permit/readiness assertions, so no successful hour-long final-drain claim is made.

A [controlled disconnect diagnostic](validation/phase4/disconnect-boundary/result.json) used a limit of one and a backend deliberately continuing work after its connection closed. Private/uncached serving and cache-pressure fallback released the permit while that work remained active; a second request produced an origin peak of two. Ordinary cached serving retained its permit both before and after response headers in this diagnostic. Thus proxy-held permits do not bound all non-cooperative work after a canceled connection. The exact path causing each long-soak peak is not yet isolated. No production workaround or relaxed peak assertion was introduced; resolution and fresh successful hour-long runs are release blockers.

Extended 40-round/64-reader memory gates passed for both policies: peak RSS 70,144 KiB for CLOCK and 63,928 KiB for TinyUFO, with accounted cache peaks below 8 MiB. The 20-second Linux handover served 370/370 requests, and ten chaos rounds passed.

Early concurrent validation was not clean: a short TinyUFO diagnostic captured a 500 ms upstream connect timeout without cache-pressure events; a one-second drain fixture served 9/10 requests under competing loads. My parallel framework runners also collided on fixed fixture ports 13002/13003. The final isolated `./bench/all.sh` passed with all Docker/browser/TLS checks enabled, followed by three successful isolated 60-second TinyUFO runs. These reruns resolve the short integration gates; they do not supersede the failed hour-long gates. All original failures remain in the evidence.

## Framework compatibility

The declared support matrix remains Next 16.4.0 and 16.3.8 on Node 22 and 24;
no broader version policy was invented. Host Node/Bun adapter tests passed
79/80 with the one existing manifest-copy fixture check skipped. That skip is
not framework compatibility evidence. Full real-build HTTP, browser navigation/prefetch, Server Action, forwarding, invalidation and replica-recovery suites passed for all four declared pairs. The initially colliding Next 16.3.8/Node 22 run is retained as failed infrastructure evidence and superseded by its serialized full rerun; no missing suite is counted as a pass.

## CI and release engineering

PR checks retain the existing real-framework/end-to-end jobs and add validation
of production examples and benchmark smoke. The fuzz runner cannot silently
run zero targets. Workflow lint passed. The new read-only extended workflow
runs 40-round/64-reader memory pressure, repeated invalidation, lifecycle, container/quotas, three short hit/fill performance samples per policy, and longer fuzz/soak checks. Release-candidate mode also runs the 20-second handover and ten-round chaos gates. The container probe caps quota-derived expectations at the Docker host CPU count, so a four-CPU ceiling on a two-core runner correctly expects two workers. Release calls it on the explicit tag/ref with one hour per policy
before the existing full CI, locked native/musl builds, checksums, SBOM and
provenance publication pipeline. No new publication permission or credential
was introduced.

`release-preflight.py` enforces clean source and matching Cargo/lock/npm versions.
Its local `--allow-dirty` mode explicitly reports `release_gate: false` and is
not used by CI. The existing binary rebuild workflow remains. Two independent TLS release builds reproduced exactly from clean commit `32cafb59b46b945509ee9a160f6e2f97a14a39af`, frozen snapshot `12eaf23498a2c8acb6959fa631eafc913cf57e40647cd7bf094651f921b55f46`. Both binary SHA-256 values are `63a3d3b91319627cb4169b4ec8ace7f2804cc588a57773518fd0a7d40ebab26f`; see [reproduction metadata](validation/phase4/reproduction.json). Later changes to guides, scripts and reports do not change the measured Rust/Cargo/embedded-asset input digest.

Cargo-cyclonedx 0.5.9 generated CycloneDX 1.5 inventories with all features; these inventory the resolved graph, not just linked runtime code. Local native/musl review archives passed version/config smoke tests and checksum generation. npm dry runs verified 0.4.0 package identity and required CLI/binary contents. Their metadata and checksums are retained under [artifacts](validation/phase4/artifacts/); binaries and archives remain local and unpublished.

Release review corrected an existing false verification recipe: BuildKit OCI provenance attachments are not GitHub-signed artifact attestations. The workflow now checks that SLSA metadata is present (v0.2/v1 shapes supported), records the exact pushed image digest, and documents metadata inspection without claiming signed builder identity. The SBOM tool is pinned to the locally verified version. No new signing/publication permission was added; registry/tag verification still requires an authorized future release run. The verifier freezes source once, verifies clean inputs against Git blob identities, and builds both directories from that frozen copy with checkout/cache paths remapped. An earlier attempt was interrupted before comparison because its second source copy could have read intervening edits; no reproduction claim comes from that attempt. No signed release provenance or external attestation is claimed.

## Documentation and known limitations

The operations/release guides, changelog, migration guide, deployment/security
models, examples, benchmark provenance and required root reports are included.
Reports are not excluded by .gitignore. Internal Phase 1–3 implementation reports remain local and ignored under `docs/PHASE*.md`. The Phase 3 report was removed from Git tracking at the user’s request; public operations text no longer links to it or repeats its affected numbers. Public operator, security, architecture and release references remain tracked.

Remaining operational boundaries include detached-reader RSS, the first
oversized chunked response abort, bounded headroom exhaustion, per-process
cache/coalescing/purge, static replica capacity partitions, edge upload/
connection limits, and long-lived tunnel termination at the grace deadline.
The external cache-key review remains outstanding. There is no claim of a new
TinyUFO optimization, scheduler tuning proof, dynamic capacity coordinator,
cluster certification or complete external security audit.

## Release readiness decision

**NOT READY** — both mandatory hour-long origin-concurrency gates failed (observed 10, configured 8). The disconnect diagnostic confirms a relevant limitation for uncached/cache-pressure fallback work that outlives a connection, but does not fully attribute the long-soak peaks. Resolve the backend/proxy cancellation contract, retain a meaningful origin-side witness, and rerun `SOAK_SECONDS=3600 SOAK_WORKERS=24` under CLOCK and TinyUFO without weakening the ceiling assertion.

The independent cache-key/shareability review and Kubernetes staging validation of CNI/probes/ordered replacement remain open. Clean final-ref CI and any tag/registry checks must also pass before publication. Successful builds, short tests, stable RSS and byte-reproduced binaries do not override the failed stability gates. This report makes no independent-audit or release-ready claim.
