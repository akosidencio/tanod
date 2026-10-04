# Release gates

What has to be true before a tag is cut, and what runs where.

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
| End-to-end suite | `./bench/all.sh` | any script's own assertions |
| Next.js proof | `./bench/nextjs.sh` (11 steps), `./bench/nextjs-browser.sh` | any assertion |
| Fuzz targets | `cargo +nightly fuzz build`, then 60s per target | a crash, or a target that no longer builds |

`bench/all.sh` runs, among the rest of the suite, the operability scripts added
in this phase at CI-sized parameters:

| Script | Asserts |
|---|---|
| [`admin.sh`](../bench/admin.sh) | readiness fails on drain **while traffic is still served**; liveness does not follow readiness; generation and the stable config fingerprint track a reload; metrics and endpoints agree |
| [`upgrade.sh`](../bench/upgrade.sh) | `--test` fails on a config that cannot start; plain SIGTERM exposes a serving/not-ready drain window; on Linux, zero failed requests across a socket handover; everywhere, an explicit pre-drain is not paid twice |
| [`tracing.sh`](../bench/tracing.sh) | trace context propagated and continued correctly; spans exported in a shape a collector accepts; log ids findable in the exported spans; a dead collector costs no request |
| [`soak.sh`](../bench/soak.sh) | over a short run: no leak, no permit left held, the cache filled *and* stayed inside its budget, no panic |
| [`memory.sh`](../bench/memory.sh) | every configured budget holds while the workload exceeds it; an oversized body is served and not stored |
| [`chaos.sh`](../bench/chaos.sh) | survives losing every backend; nothing private is shared; the admin surface answers *during* the outage; capacity comes back |

And the origin-resilience scripts. Both are written so the *origins* are the
witnesses — each fixture counts what it was actually asked to render — because
a breaker benchmark that reads the proxy's own metrics is measuring the
component under test:

| Script | Asserts |
|---|---|
| [`breaker.sh`](../bench/breaker.sh) | a backend whose `/healthz` answers `200` while every render answers `502` is **still reported healthy** and is nonetheless ejected; the good backend then takes essentially all the traffic; once the bad one heals, a recovery probe returns it to rotation and the split evens out again |
| [`retry.sh`](../bench/retry.sh) | with one of two backends killed outright: retries off loses about half the requests; a generous budget serves nearly all of them; **a budget with a floor of one retry refuses the rest rather than amplifying the failure** |
| [`storage.sh`](../bench/storage.sh) | a cache hit is orders of magnitude cheaper than a miss, and a small-body hit stays within the round-trip-plus-lookup budget that [`CACHE-STORAGE-EVALUATION.md`](./CACHE-STORAGE-EVALUATION.md) rests on |
| [`purge.sh`](../bench/purge.sh) | a purge by tag removes exactly the entries carrying it, leaves the others cached, and costs exactly that many re-renders at the origin; a purge by path removes every query variant of one page; a `GET`, a wrong token, a missing token, a misspelled parameter and a relative path each change nothing |

---

## Before a tag

Beyond CI. These are longer than a pull request should wait for, and they are
the ones that catch what short runs cannot.

### 1. The long soak — one hour minimum

```bash
SOAK_SECONDS=3600 SOAK_WORKERS=24 ./bench/soak.sh
```

**Gate:** exits zero.

The assertions inside it are the point: resident set size in the last quarter
of the run must not have grown past the first quarter's allowance, no origin
permit may still be held once traffic stops, the cache must have both filled
and stayed inside its budget, and the log must contain no panic.

An hour is the floor, not the target. A leak of a few kilobytes per thousand
requests is invisible in sixty seconds and obvious in an hour.

### 2. Memory pressure

```bash
MEMORY_ROUNDS=40 MEMORY_SLOW_READERS=64 ./bench/memory.sh
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

- `git tag` matches the `version` in `Cargo.toml`. The release workflow
  refuses otherwise, so this is a gate rather than a habit.
- `SHA256SUMS` is present and covers every published archive.
- The CycloneDX SBOM is attached.
- The image's provenance attestation verifies:
  `gh attestation verify "oci://ghcr.io/OWNER/tanod:<version>" --repo OWNER/tanod`
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
