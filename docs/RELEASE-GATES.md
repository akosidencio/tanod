# Release gates

What has to be true before a tag is cut, and what runs where.

The current 0.4 candidate is **CONDITIONALLY READY**. It fixes four
vulnerabilities present in 0.3.2 ([release notes](RELEASE-NOTES-0.4.0.md#2-security-fixes)):
three found by the 0.4.0 cache security review
([package](reviews/CACHE-SECURITY-REVIEW-v0.4.0.md)) and the forwarded-header
defect fixed earlier in the cycle. Both required hour-long runs (CLOCK and
TinyUFO, 24 workers) passed with the uncached-disconnect arm, holding the
origin ceiling at 8 against 8 and completing every disconnect drain inside the
deadline. The release image is now smoke-tested before it is pushed, and
every workflow action is pinned by commit SHA. Still open before a tag: an
independent sign-off on the cache security review, a Kubernetes staging run
([procedure](../ops/production/KUBERNETES-STAGING.md)), and green CI on the
release commit.

The distinction that matters: **a gate is something that fails the build**, not
something a person reads afterwards. Every item below either exits non-zero on
its own or does not belong on this list. An earlier version of this project's
benchmark suite printed numbers for a human to eyeball, and that is how a
regression ships.

---

## Every push and pull request

Run by [`.github/workflows/ci.yml`](../.github/workflows/ci.yml). All of these
must pass before merge.

| Gate | Command | Fails on |
|---|---|---|
| Formatting | `cargo fmt --all -- --check` | any diff |
| Lints | `cargo clippy --workspace --all-targets --locked -- -D warnings` | any warning |
| Lints, TLS build | the same `--features tls` | any warning |
| Tests | `cargo test --workspace --all-targets --locked` | any failure |
| Tests, TLS build | `cargo test --workspace --lib --locked --features tls` | any failure |
| Doc tests | `cargo test --workspace --doc --locked` | any failure |
| Dependency advisories | `cargo audit -D warnings` | any advisory not listed in `.cargo/audit.toml` with a reason |
| Vendored Pingora proxy | `scripts/check-vendored-pingora.sh` | `vendor/pingora-proxy` differing from the checksum-verified published crate plus `tanod.patch` (a path package is invisible to `cargo audit`) |
| Workflow pins and gating | `python3 scripts/check-workflows.py` | an action not pinned to a full commit SHA, a job with write permission that does not wait for every check, a missing top-level `permissions`, an undeclared job or step output, or an input expanded inside a `run:` script |
| End-to-end suite | `./bench/all.sh` | any script's own assertions |
| Next.js compatibility | `bash bench/nextjs-compatibility.sh` for every declared Next.js/Node pair | actual version mismatch, generated policy rejection, or any HTTP, browser, invalidation, or replica assertion |
| Fuzz targets | `cargo +nightly fuzz build`, then 60s per target | a crash, or a target that no longer builds |

The separate Next.js matrix is generated from
[`packages/tanod-next/src/compatibility.json`](../packages/tanod-next/src/compatibility.json).
It covers Next 16.4.0 and 16.3.8 on Node 22 and 24. Adapter unit tests also run
on both Node lines and the declared Bun version. A copied manifest fixture is
not evidence that the full integration passed on another framework release.

`bench/all.sh` runs, among the rest of the suite, the operability scripts added
in this phase at CI-sized parameters:

| Script | Asserts |
|---|---|
| [`admin.sh`](../bench/admin.sh) | readiness fails on drain **while traffic is still served**; liveness does not follow readiness; generation and the stable config fingerprint track a reload; metrics and endpoints agree |
| [`upgrade.sh`](../bench/upgrade.sh) | `--test` fails on a config that cannot start; plain SIGTERM exposes a serving/not-ready drain window; on Linux, zero failed requests across a socket handover; everywhere, an explicit pre-drain is not paid twice |
| [`tracing.sh`](../bench/tracing.sh) | trace context propagated and continued correctly; spans exported in a shape a collector accepts; log ids findable in the exported spans; a dead collector costs no request |
| [`soak.sh`](../bench/soak.sh) | over a short run: no leak, no permit left held, no disconnect drain left active, the cache filled *and* stayed inside its budget, the origin's own peak within the ceiling, no panic; with the uncached hang-up arm (default), completed drains > 0 and none cut off at the deadline |
| [`origin-concurrency.py`](../bench/origin-concurrency.py) | with an origin whose render count cannot drift: the ceiling holds across client disconnects before and after headers, cache-pressure fallback, cache fills, disconnect storms and coalesced streams, and each case took the expected path (drain completed, cache fill, or none); an origin that works past its own response or past `timeouts.origin` produces exactly the documented overlap and a `deadline` count. Runs with CLOCK and TinyUFO |
| `origin-concurrency.py --h2` | the same scenarios against [`h2-origin.mjs`](../bench/h2-origin.mjs), an h2c origin with the same witness, so the vendored drain's HTTP/2 upstream loop is driven directly; plus an origin reset during a drain (`upstream_error`, never `completed`), no retry after a reset once rendering began, and a connect failure retried onto the live backend. Needs `node` |
| [`cache-isolation.py`](../bench/cache-isolation.py) | no response reaches a user it was not rendered for, across unclassified, public, private, coalescing-override and cache-override routes, eight origin answers, sequential and concurrent, `Authorization`, draft mode, Server Actions, RSC, a reload mid-flight and a cancelled shared stream; sharing only where the route's contract permits it, and those paths must be taken; query-name spellings cannot poison a key; dot-segment targets are refused before the origin. CLOCK and TinyUFO; with Docker also the Next.js fixture's cookie-reading `/account`, HTML and RSC |
| [`query-cardinality.py`](../bench/query-cardinality.py) | under a unique-query flood (plain, 2 KiB names and values, with purges, and with an `include` policy): cache bytes within budget, fill bytes within budget plus one body, the origin within its ceiling, nothing held afterwards. Records RSS, CPU, evictions and hot-key latency. CLOCK and TinyUFO |
| [`memory.sh`](../bench/memory.sh) | every configured budget holds while the workload exceeds it; an oversized body is served and not stored |
| [`cache-pressure.sh`](../bench/cache-pressure.sh) | with no cache memory a coalesced herd is served uncached and complete; a fill that runs out mid-body finishes for its leader and follower and is not stored; a burst past the headroom aborts exactly the fills that exhausted it, and every other response is byte-exact; an oversized chunked body fails once, then streams uncached; pinned bytes return to zero. Runs with CLOCK and TinyUFO |
| [`forwarded.sh`](../bench/forwarded.sh) | a client's forwarding headers move neither the origin's view nor the key; a forged host cannot poison a cacheable page (`/reflect`); a trusted proxy's chain is walked, a spoofed prefix chooses neither scheme nor host, and an opted-in public host is preserved and keyed; IPv6 peers |
| [`nextjs-forwarded.sh`](../bench/nextjs-forwarded.sh) | against a real Next.js server: a forged scheme cannot shape a redirect, a forged host cannot pass the Server Action origin check while a genuine or public origin can, SSR/RSC entries survive forged headers, http/https/host variants are separate entries. Needs Docker |
| [`chaos.sh`](../bench/chaos.sh) | survives losing every backend; nothing private is shared; the admin surface answers *during* the outage; capacity comes back |

The suite also runs coalescing, streaming, HTTP protocol semantics, purge,
memory pressure, storage cost and the short soak with `CACHE_EVICTION=tinyufo`.
`bench/cache-policy-reload.sh` also runs with CLOCK and TinyUFO: actual origin
render counts must stay unchanged for an operational reload and increase after
TTL, query, variant, override, public/private-route and deployment changes.
Each run records its policy and writes a separate artifact. The cache unit
tests additionally cover admission rejection, repeated invalidation and
replacement, metadata revalidation, TTL/stale metadata and concurrent
accounting. The default CLOCK coverage remains part of the suite.

And the origin-resilience scripts. Both are written so the *origins* are the
witnesses — each fixture counts what it was actually asked to render — because
a breaker benchmark that reads the proxy's own metrics is measuring the
component under test:

| Script | Asserts |
|---|---|
| [`breaker.sh`](../bench/breaker.sh) | a backend whose `/healthz` answers `200` while every render answers `502` is **still reported healthy** and is nonetheless ejected; the good backend then takes essentially all the traffic; once the bad one heals, a recovery probe returns it to rotation and the split evens out again |
| [`retry.sh`](../bench/retry.sh) | with one of two backends killed outright: retries off loses about half the requests; a generous budget serves nearly all of them; **a budget with a floor of one retry refuses the rest rather than amplifying the failure** |
| [`storage.sh`](../bench/storage.sh) | a cache hit is orders of magnitude cheaper than a miss, and a small-body hit stays within the script's round-trip-plus-lookup budget |
| [`purge.sh`](../bench/purge.sh) | a purge by tag removes exactly the entries carrying it, leaves the others cached, and costs exactly that many re-renders at the origin; a purge by path removes every query variant of one page; a `GET`, a wrong token, a missing token, a misspelled parameter and a relative path each change nothing |

---

## Before a tag

Beyond CI. These are longer than a pull request should wait for, and they are
the ones that catch what short runs cannot.

### 1. The long soak — one hour minimum

```bash
SOAK_SECONDS=3600 SOAK_WORKERS=24 ./bench/soak.sh
CACHE_EVICTION=tinyufo SOAK_SECONDS=3600 SOAK_WORKERS=24 ./bench/soak.sh
```

**Gate:** exits zero.

The assertions inside it are the point: resident set size in the last quarter
of the run must not have grown past the first quarter's allowance, no origin
permit may still be held once traffic stops, the cache must have both filled
and stayed inside its budget, the origin's own count of concurrent renders
must never have exceeded the ceiling of 8, and the log must contain no panic.

The origin fixture is the witness for that ceiling. It decrements its render
count before writing the last byte of a response, never after: a proxy
releases capacity when it reads the end of a response, so a count that fell
only after the final write returned could include a render whose output was
already complete. That ordering, not a looser bound, is what made the 0.4
soak's witness agree with the work actually running. Run with
`SLOW_ORIGIN_EVENTS=<file>` to record every render start and end for
attributing a failure.

An hour is the floor, not the target. A leak of a few kilobytes per thousand
requests is invisible in sixty seconds and obvious in an hour.

### 2. Memory pressure

```bash
MEMORY_ROUNDS=40 MEMORY_SLOW_READERS=64 ./bench/memory.sh
CACHE_EVICTION=tinyufo MEMORY_ROUNDS=40 MEMORY_SLOW_READERS=64 ./bench/memory.sh
```

**Gate:** exits zero, and record `rss_peak_kb` in the release notes.

Peak RSS is a number people size containers with. Publishing it alongside the
budgets that produced it is what makes it usable; publishing it without them
would be a number nobody can apply to their own configuration.

### 3. Restart and chaos

```bash
./bench/upgrade.sh 20
CHAOS_ROUNDS=10 ./bench/chaos.sh
```

**Gate:** both exit zero. On Linux, `upgrade.sh` additionally has to report
`failed: 0` across the socket handover — a non-zero there is a release blocker,
not a flake, because it is the claim the feature exists to make.

### 4. The full local suite, at full parameters

```bash
./bench/all.sh
```

**Gate:** `all benchmarks held`, with nothing in the `FAILED:` line — including
the skip markers. `nextjs.sh(no-docker)` counts as a failure on purpose: a
suite that reports success while silently omitting its only real-framework test
is exactly the untrustworthy evidence this project spent a phase removing.

### 5. Artifacts

- `git tag` matches the `version` in `Cargo.toml` and in
  `packages/tanod-next/package.json`. The release workflow refuses otherwise,
  so this is a gate rather than a habit.
- `@tanod/next` at the tag's version is on npm (with provenance once the
  repository is public; npm refuses it from a private one), published
  last by the release workflow (`NPM_TOKEN` repository secret):
  `npm view @tanod/next@<version>` and `npm audit signatures` in a project
  that installs it.
- `SHA256SUMS` is present and covers every published archive.
- The CycloneDX SBOM is attached.
- The image was smoke-tested before publication and published under the
  digest that was tested (the release page and the workflow summary name it;
  `skopeo inspect --raw docker://ghcr.io/akosidencio/tanod:<version> | sha256sum`
  must match). It has BuildKit provenance metadata and an SBOM. Inspect the reviewed
  digest with `docker buildx imagetools inspect IMAGE@sha256:DIGEST --format '{{ json .Provenance.SLSA }}'`
  and compare its inputs/revision with the release source. The workflow rejects
  missing provenance. These OCI attachments are not signed GitHub artifact
  attestations; `gh attestation verify` is not a verification gate provided here.
  See [BuildKit provenance](https://docs.docker.com/build/metadata/attestations/slsa-provenance/).
- A Linux release binary reproduces from a clean checkout of the tag —
  [`scripts/reproducible-build.sh`](../scripts/reproducible-build.sh).

### 6. Documentation honesty

The one gate with no script behind it, and the one this project has failed
before — an audit once found seven wrong or dead claims in the README.

- Every capability claimed in the README is either exercised by a benchmark or
  labelled as not yet proven.
- Every configuration key in the schema is either implemented or refused at
  startup, and `docs/CONFIG-SCHEMA.md` lists the refusals.
- The project status table names what is still outstanding. The independent
  cache-key review has been outstanding since phase 1 and stays listed until
  someone takes it up.


### 7. The release workflow itself

[`release.yml`](../.github/workflows/release.yml) runs on a `v*` tag (or a
`workflow_dispatch` re-cut of an existing tag). Nothing it publishes can go
out before every check below has passed; `scripts/check-workflows.py` fails CI
if a job with write permission could run without waiting for one of them.

```text
preflight      tag format, tag on main, one commit for the whole run,
               clean source, Cargo/npm versions = tag, Cargo.lock in sync
stability      extended.yml on that commit: one-hour soaks per policy,
               memory, reload, upgrade, chaos, hardened container
test           ci.yml on that commit: the full CI gate above
binaries/musl  locked, reproducible builds; version, TLS and config smoke
sbom           CycloneDX of the resolved tree, checked to name the version
image          build once into an OCI archive (no registry credentials);
               verify: one linux/amd64 image, provenance, SBOM, labels;
               load and compare layers; scripts/image-smoke.sh;
               rehearse the digest-preserving push to a loopback registry
--------------------------------------- nothing is public before this line
image-publish  `release` environment (approval point); same archive, same
               digest: version tag (refused if it names another image),
               verified, then MAJOR.MINOR and latest
publish        GitHub release: every expected archive, each binary checked
               against its recorded checksum, SBOM, SHA256SUMS
npm            @tanod/linux-x64 (binary checked against the musl job's
               hash), then @tanod/next
```

`scripts/image-smoke.sh` checks the image as production runs it (read-only
root, uid 10001, no capabilities, no-new-privileges, pid and memory limits):
metadata and labels; `tanod version` reports the tag's version with `tls`;
`tanod check` accepts the production example and refuses a configuration
0.4.0 must refuse; liveness and readiness; a proxied origin response; a
dot-segment target refused with 400; admission past a ceiling of one against
an origin that counts its own renders (some shed with `Retry-After`, origin
peak at most 1); metrics; and SIGTERM (not ready while still serving, exit 0
within the drain budget, no panic). Run it locally against any loaded image:

```sh
bash scripts/image-smoke.sh tanod:candidate 0.4.0
```

**Approval.** Configure Settings → Environments → `release` with required
reviewers and a deployment rule for `v*` tags. The run then stops before
`image-publish`, after every check has passed, until a maintainer approves.
Without reviewers the environment is created on first use and does not wait.

**Partial publication.** The three registries are written in order (GHCR,
then the GitHub release, then npm) and cannot be written atomically. If a
later step fails:

- *GHCR pushed, GitHub release failed:* fix the cause and re-run the failed
  jobs. `image-publish` re-verifies the stored archive and skips a version tag
  that already names the same digest; it refuses one that names a different
  image.
- *GitHub release published, npm failed:* re-run the `npm` job; it skips any
  package version already on npm.
- *A published artifact is wrong:* do not move or reuse the version. Delete
  the GitHub release assets and mark the release as a draft, point `latest`
  and `MAJOR.MINOR` back at the previous digest
  (`skopeo copy --all --preserve-digests docker://ghcr.io/akosidencio/tanod@<previous> docker://ghcr.io/akosidencio/tanod:latest`),
  `npm deprecate` the affected npm versions, and publish a new patch version.
  An npm version can never be republished.

A `workflow_dispatch` re-cut of an existing tag builds a new image, whose
digest differs (BuildKit provenance records the build), so `image-publish`
refuses to overwrite the version tag. Re-running failed jobs of the original
run is the way to resume a release.

### Pinned actions

Every `uses:` names a full commit SHA and, in a comment, the release it was
resolved from (`# v4.4.0`, or `# stable` for dtolnay/rust-toolchain's branch).
To update one deliberately:

1. Pick the release and read its changelog.
2. Resolve the commit from the action's own repository, peeling annotated
   tags: `git ls-remote --tags https://github.com/<owner>/<action> 'refs/tags/<tag>*'`
   (use the `^{}` line when present). For dtolnay/rust-toolchain use
   `git ls-remote https://github.com/dtolnay/rust-toolchain refs/heads/stable`.
3. Replace the SHA and the comment in every workflow that uses the action, in
   one commit that names the old and new releases.
4. Run `python3 scripts/check-workflows.py` and let CI run on the pull
   request. A release-only action (docker/*, skopeo use) is first exercised
   by the next tag; prefer updating those just after a release.

Dependabot's `github-actions` ecosystem understands this format and can
propose these updates; review its SHA against step 2 before merging.

---

## Performance measurement

Measurement tools, not gates — see below for why throughput is never one.
Each records the parameters, machine and binary that produced its numbers.

| Tool | Measures | Notes |
|---|---|---|
| [`bench/perf.sh`](../bench/perf.sh) | Full-system throughput, latency percentiles, Tanod CPU per request and RSS, origin renders, for cache hits across concurrency and body size, misses, coalescing bursts of 10–1000, a configurable production mix ([`perf-mixed.mix`](../bench/perf-mixed.mix)), origin overload direct and through Tanod, forwarding overhead alone (`passthrough`), streaming with slow readers, and memory pressure | JSON lines under `benchmark-results/perf/<label>/`, plus `environment.json`. Pins Tanod, load and origin to separate CPUs on a six-CPU machine. Driven by [`bench/loadgen`](../bench/loadgen), a closed-loop HTTP/1.1 generator in the workspace |
| [`bench/perf-ab.sh`](../bench/perf-ab.sh) | Two binaries, interleaved trial by trial, then [`perf-compare.py`](../bench/perf-compare.py) medians and spread | Interleaving is what makes a comparison survive a busy machine: drift lands on both sides |
| [`bench/hotpath.rs`](../bench/hotpath.rs) | In-process microbenchmarks: store hits by body size and concurrency, fills under memory pressure, refills of stored keys, coalesced followers, route resolution for 5–1000 routes, cache-key construction, metadata decoding | `cargo bench --bench hotpath`; compare two builds with [`hotpath-compare.py`](../bench/hotpath-compare.py). Under `cargo test` each scenario runs a few iterations only |
| [`bench/alloc.sh`](../bench/alloc.sh) | Heap allocations and bytes per request by request type, counted by valgrind DHAT | Deterministic, so it compares cleanly between builds on any machine |

The short `perf.sh` scenarios fit a CI runner but are not run by CI: a shared
runner's throughput is not evidence about the change under test.

---

## What is deliberately *not* a gate

Naming these is part of the point: a list of gates that quietly omits its
exclusions reads as more coverage than it has.

- **Throughput and latency numbers.** Measured and published with the machine
  that produced them, never asserted against a threshold. A CI runner's
  absolute numbers are not a baseline anybody else's hardware should be held
  to, and a performance gate on shared runners fails for reasons unrelated to
  the change.
- **Every platform except Linux.** Tanod is deployed on Linux, the image is
  `linux/amd64` and the only release binary is `x86_64-unknown-linux-gnu`, so
  nothing is built or tested anywhere else. Tanod still *compiles* on macOS
  and is usable for local development, but that is unverified by CI and no
  artifact is published for it.
- **Dynamic replica coordination.** The replicated reference tests a fixed
  partition, URI affinity, failure, recovery, and purge fan-out. It does not
  discover excess replicas or lease capacity dynamically; the orchestrator
  must honor the declared replica count.

## Three validation tiers for the 0.4 candidate

**Tier 1 — PR:** the existing formatting, both Clippy/TLS paths, unit/doc/cache/
forwarding tests and dependency audit remain mandatory. Production examples are
checked by the actual configuration loader, and the hotpath target runs in its
smoke mode. The existing real Next.js matrix and end-to-end jobs remain on PRs;
this change does not weaken them or substitute mocks for framework evidence.

**Tier 2 — nightly/extended:** [extended.yml](../.github/workflows/extended.yml)
runs bounded fuzz campaigns, both cache policies, memory pressure, repeated
policy reload/purge cycles, local lifecycle schedules, ten-minute soaks and the
non-root read-only container/CPU-quota probe with a real Next.js origin. Fuzz
corpora and crash artifacts are retained. This workflow needs no publication
permissions or new secrets. Its schedule runs on the repository default branch.
Once the workflow is registered there, dispatch a candidate branch explicitly;
before that, run the local commands below. See the
[workflow_dispatch requirements](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#workflow_dispatch).
It also captures three short hit/fill performance samples per policy, without
shared-runner throughput thresholds. Memory runs use 40 rounds and 64 slow
readers. Release-candidate runs use the 20-second upgrade and ten-round chaos
parameters above; scheduled runs use six seconds and two rounds.

**Tier 3 — RC:** the release workflow calls the same extended workflow on the
explicit tag/ref, with **one hour per policy**, before its existing CI, real
framework matrix, locked native/musl builds, SBOM, checksums and provenance
steps. `python3 scripts/release-preflight.py` refuses dirty source or inconsistent
Cargo/npm versions. `--allow-dirty` exists for local preparation only, reports
`release_gate: false`, and is never used by the release workflow. No local
benchmark or this document authorizes publication.

Additional reproducible local checks:

```sh
python3 bench/lifecycle.py
TANOD_IMAGE=tanod:review-candidate bash bench/container.sh
FUZZ_SECONDS=60 bash scripts/fuzz-smoke.sh
python3 scripts/check-workers.py target/release/tanod
bash bench/phase31.sh /absolute/phase2/tanod /absolute/candidate/tanod
```

Build each measured binary with locked dependencies, then immediately run
`python3 scripts/build-manifest.py /absolute/path/to/binary`. Keep that sidecar
with the binary: perf.sh verifies its SHA-256 and retains every invocation's
build identity and environment in `environments.jsonl`. The build-input digest
covers Rust sources, Cargo manifests/lockfiles, toolchain and Cargo config;
it is distinct from a clean Git commit and from a Docker image digest.

The performance runner requires six useful CPU affinities for the full local
1/2/available-worker comparison, or explicit taskset overrides appropriate to
the machine. Do not run builds, fuzzers or stability loads alongside controlled
performance comparisons. Short local trials are sampling evidence, not proof
that a noisy p99 difference is statistically significant.

Fuzz seeds come from `fuzz/corpus/<target>` and are preserved between runs.
Keep secrets and production requests out of corpora. Failures stay in
`fuzz/artifacts/<target>`; minimize a reproducer with `cargo +nightly fuzz tmin`
and add a targeted regression before restarting a campaign. The lifecycle
model is a separate Loom unit test; it does not model HTTP parsers or all
TinyUFO internals.

For the capacity demonstration, the origin fixture's `total`/`peak` count
received/outstanding requests, including its own queue. They are not completed
render counters. The final CSV separates those from successful uncached
responses completed inside the measurement window. Warmup-inclusive counters
must not be divided by measured-window counts or advertised as extra render
capacity. Streaming paths with a query use the fixture's four-chunk fallback;
inspect the captured harness when reproducing historical stream samples.
